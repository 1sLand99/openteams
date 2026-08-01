use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use super::acp::{
    AcpAccessMode, AcpAgentHarness, AcpApprovalMode, AcpApprovalPolicy, AcpAuthSelection,
    AcpCapabilityProbe, AcpClientServicePolicy, AcpExecutionOptions,
    mcp::{AcpMcpPolicy, resolve_effective_mcp_config, write_mcp_isolation_settings},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides, command_is_available,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
        utils::{dotenv_has_nonempty_value, json_has_nonempty_string, read_json_file},
    },
    mcp_config::{McpConfig, read_canonical_mcp_config},
    model_discovery::{
        ProviderKind, cli_model_commands, discover_from_sources, runner_config_paths,
    },
};

const QWEN_AUTH_ENV_VARS: &[&str] = &[
    "QWEN_API_KEY",
    "DASHSCOPE_API_KEY",
    "BAILIAN_CODING_PLAN_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
];

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct QwenCode {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Model to use (e.g., qwen3-coder-plus, qwen3-coder-flash)")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Per-run Qwen Code reasoning effort: low, medium, high, xhigh, max, or a numeric token budget"
    )]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpExecutionOptions>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub acp_mcp_policy: AcpMcpPolicy,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl QwenCode {
    const BASE_COMMAND: &'static str = "qwen";

    fn effective_approval_mode(&self) -> AcpApprovalMode {
        self.acp
            .as_ref()
            .and_then(|options| options.approval_mode)
            .unwrap_or_default()
    }

    fn native_reasoning_settings(&self) -> Result<Value, ExecutorError> {
        let Some(effort) = self
            .thinking_effort
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(json!({}));
        };
        let normalized = effort.to_ascii_lowercase();
        if matches!(
            normalized.as_str(),
            "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Ok(json!({
                "model": {
                    "reasoningEffort": normalized
                }
            }));
        }
        if matches!(normalized.as_str(), "off" | "none") {
            return Ok(json!({
                "model": {
                    "generationConfig": {
                        "reasoning": false
                    }
                }
            }));
        }
        if let Ok(budget) = normalized.parse::<u32>() {
            return Ok(json!({
                "model": {
                    "generationConfig": {
                        "reasoning": {
                            "effort": "high",
                            "budget_tokens": budget
                        }
                    }
                }
            }));
        }
        Err(invalid_reasoning_effort(format!(
            "unsupported Qwen Code reasoning effort `{effort}`"
        )))
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        if self
            .cmd
            .additional_params
            .as_ref()
            .is_some_and(|params| params.iter().any(|value| value.contains("--approval-mode")))
        {
            return Err(CommandBuildError::InvalidShellParams(
                "Qwen --approval-mode is controlled by structured ACP approval settings"
                    .to_string(),
            ));
        }

        let qwen_approval_mode = match self.effective_approval_mode() {
            AcpApprovalMode::Ask | AcpApprovalMode::AutoAllow | AcpApprovalMode::AutoReject => {
                "default"
            }
        };
        let builder = CommandBuilder::new(Self::BASE_COMMAND).extend_params([
            "--acp",
            "--approval-mode",
            qwen_approval_mode,
        ]);
        apply_overrides(builder, &self.cmd)
    }

    async fn acp_harness(&self) -> Result<AcpAgentHarness, ExecutorError> {
        let options = self.acp.clone().unwrap_or_default();
        let approval_policy = match self.effective_approval_mode() {
            AcpApprovalMode::Ask => AcpApprovalPolicy::Ask,
            AcpApprovalMode::AutoAllow => AcpApprovalPolicy::AutoAllow,
            AcpApprovalMode::AutoReject => AcpApprovalPolicy::AutoReject,
        };
        let additional_directories = options
            .validated_directories()
            .await
            .map_err(ExecutorError::Io)?;
        let full_access = options.access_mode.unwrap_or_default() == AcpAccessMode::FullAccess;
        let mut harness = AcpAgentHarness::new()
            .with_approval_policy(approval_policy)
            .with_additional_directories(additional_directories)
            .with_client_services(AcpClientServicePolicy {
                read_text_file: true,
                write_text_file: true,
                terminal: true,
                full_access,
                ..AcpClientServicePolicy::default()
            });
        if let Some(AcpAuthSelection::MethodId { method_id }) = options.auth {
            harness = harness.with_auth_method_id(method_id);
        }
        let config_overrides = options.config_overrides.as_deref().unwrap_or_default();
        let has_model_override = config_overrides.iter().any(|selection| {
            selection.category_snapshot.as_deref() == Some("model")
                || selection.option_id.eq_ignore_ascii_case("model")
        });
        let has_thought_override = config_overrides.iter().any(|selection| {
            selection.category_snapshot.as_deref() == Some("thought_level")
                || selection
                    .option_id
                    .replace(['-', '_', ' '], "")
                    .eq_ignore_ascii_case("thoughtlevel")
        });
        if let Some(model) = self
            .model
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .filter(|_| !has_model_override)
        {
            harness = harness.with_model(model);
        }
        if let Some(effort) = self
            .thinking_effort
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .filter(|_| !has_thought_override)
        {
            harness = harness.with_native_thought_level_fallback(effort);
        }
        for selection in config_overrides {
            harness = harness.with_config_override(selection);
        }
        let config_path = self.default_mcp_config_path();
        let canonical = match config_path {
            Some(path) => read_canonical_mcp_config(&path, &McpConfig::canonical_acp()).await?,
            None => serde_json::json!({ "mcpServers": {} }),
        };
        let effective = resolve_effective_mcp_config(&canonical, &self.acp_mcp_policy)?;
        tracing::debug!(
            server_count = effective.servers.len(),
            config_hash = %effective.config_hash,
            "resolved effective ACP MCP configuration"
        );
        Ok(harness.with_mcp_servers(effective.servers))
    }

    async fn acp_runtime_env(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<ExecutionEnv, ExecutorError> {
        let path = write_mcp_isolation_settings(
            current_dir,
            "qwen-acp-settings",
            self.native_reasoning_settings()?,
        )
        .await?;
        let mut runtime_env = env.clone();
        runtime_env.insert(
            "QWEN_CODE_SYSTEM_SETTINGS_PATH",
            path.to_string_lossy().to_string(),
        );
        Ok(runtime_env)
    }
}

fn invalid_reasoning_effort(message: impl Into<String>) -> ExecutorError {
    ExecutorError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message.into(),
    ))
}

#[async_trait]
impl StandardCodingAgentExecutor for QwenCode {
    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        let Some(home) = dirs::home_dir() else {
            return self.authentication_detected(&env, QWEN_AUTH_ENV_VARS, false);
        };
        let qwen_home = home.join(".qwen");
        let oauth_login =
            read_json_file(&qwen_home.join("oauth_creds.json")).is_some_and(|value| {
                json_has_nonempty_string(&value, &["/access_token", "/refresh_token"])
            });
        let settings = read_json_file(&qwen_home.join("settings.json"));
        let provider_configured = settings
            .as_ref()
            .is_some_and(|value| qwen_provider_configured(value, &env));
        let mut dotenv_env_vars = QWEN_AUTH_ENV_VARS.to_vec();
        if let Some(settings) = settings.as_ref() {
            dotenv_env_vars.extend(
                settings
                    .get("modelProviders")
                    .into_iter()
                    .flat_map(auth_env_keys),
            );
        }
        let dotenv_key = dotenv_has_nonempty_value(&qwen_home.join(".env"), &dotenv_env_vars);
        self.authentication_detected(
            &env,
            QWEN_AUTH_ENV_VARS,
            oauth_login || provider_configured || dotenv_key,
        )
    }

    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn list_models(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        let config_paths = runner_config_paths([
            self.default_mcp_config_path(),
            dirs::home_dir().map(|home| home.join(".qwen").join("settings.jsonc")),
        ]);
        discover_from_sources(
            current_dir,
            env,
            &self.cmd,
            self.model.as_deref(),
            config_paths,
            cli_model_commands(Self::BASE_COMMAND, &self.cmd),
            &[ProviderKind::OpenAiCompatible],
        )
        .await
    }

    async fn probe_acp(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
        auth_method_id: Option<&str>,
    ) -> Result<Option<AcpCapabilityProbe>, ExecutorError> {
        let configured_auth_method_id = self.acp.as_ref().and_then(|options| match &options.auth {
            Some(AcpAuthSelection::MethodId { method_id }) => Some(method_id.clone()),
            _ => None,
        });
        Ok(Some(
            super::acp::runtime::probe_acp_command(
                self.build_command_builder()?.build_initial()?,
                current_dir,
                env,
                &self.cmd,
                auth_method_id
                    .map(str::to_string)
                    .or(configured_auth_method_id),
            )
            .await?,
        ))
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let qwen_command = self.build_command_builder()?.build_initial()?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let harness = self.acp_harness().await?;
        let runtime_env = self.acp_runtime_env(current_dir, env).await?;
        harness
            .spawn_with_command(
                current_dir,
                combined_prompt,
                qwen_command,
                &runtime_env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let qwen_command = self.build_command_builder()?.build_follow_up(&[])?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let harness = self.acp_harness().await?;
        let runtime_env = self.acp_runtime_env(current_dir, env).await?;
        harness
            .spawn_follow_up_with_command(
                current_dir,
                combined_prompt,
                session_id,
                qwen_command,
                &runtime_env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        crate::executors::acp::normalize_logs(msg_store, worktree_path);
    }

    // MCP configuration methods
    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".qwen").join("settings.json"))
    }

    fn native_skill_discovery_roots(&self) -> Vec<std::path::PathBuf> {
        dirs::home_dir()
            .map(|home| vec![home.join(".qwen").join("skills")])
            .unwrap_or_default()
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if command_is_available(Self::BASE_COMMAND, &self.cmd) {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

fn qwen_provider_configured(value: &Value, env: &ExecutionEnv) -> bool {
    let has_provider = value
        .get("modelProviders")
        .and_then(Value::as_object)
        .is_some_and(|providers| {
            providers.values().any(|provider| match provider {
                Value::Array(models) => !models.is_empty(),
                Value::Object(config) => !config.is_empty(),
                _ => false,
            })
        });
    let has_inline_key =
        json_has_nonempty_string(value, &["/security/auth/apiKey", "/security/auth/token"])
            || value
                .get("env")
                .and_then(Value::as_object)
                .is_some_and(|vars| {
                    vars.values()
                        .any(|value| value.as_str().is_some_and(|value| !value.trim().is_empty()))
                });
    let has_referenced_key = value
        .get("modelProviders")
        .into_iter()
        .flat_map(auth_env_keys)
        .any(|key| {
            env.get(key).is_some_and(|value| !value.trim().is_empty())
                || std::env::var_os(key).is_some_and(|value| !value.is_empty())
        });

    has_provider || has_inline_key || has_referenced_key
}

fn auth_env_keys(value: &Value) -> Box<dyn Iterator<Item = &str> + '_> {
    match value {
        Value::Array(values) => Box::new(values.iter().flat_map(auth_env_keys)),
        Value::Object(object) => Box::new(
            object
                .get("envKey")
                .and_then(Value::as_str)
                .into_iter()
                .chain(object.values().flat_map(auth_env_keys)),
        ),
        _ => Box::new(std::iter::empty()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_model_provider_counts_as_authentication() {
        let value = serde_json::json!({
            "modelProviders": {
                "openai": [{
                    "id": "local-model",
                    "baseUrl": "http://localhost:11434/v1",
                    "envKey": "LOCAL_MODEL_API_KEY"
                }]
            }
        });
        let env = ExecutionEnv::new(Default::default(), false, String::new());

        assert!(qwen_provider_configured(&value, &env));
        assert!(!qwen_provider_configured(
            &serde_json::json!({"modelProviders": {}}),
            &env
        ));
    }

    fn qwen_with_approval(approval_mode: Option<AcpApprovalMode>) -> QwenCode {
        QwenCode {
            append_prompt: AppendPrompt::default(),
            model: Some("qwen3-coder-plus".to_string()),
            thinking_effort: None,
            acp: Some(AcpExecutionOptions {
                approval_mode,
                ..Default::default()
            }),
            cmd: CmdOverrides::default(),
            acp_mcp_policy: AcpMcpPolicy::default(),
            approvals: None,
        }
    }

    fn command_parts(qwen: &QwenCode) -> (String, Vec<String>) {
        qwen.build_command_builder()
            .expect("build command")
            .build_initial()
            .expect("build initial")
            .into_parts_for_test()
    }

    #[test]
    fn native_reasoning_settings_use_current_qwen_effort_scale() {
        let mut qwen = qwen_with_approval(None);
        qwen.thinking_effort = Some("xhigh".to_string());

        assert_eq!(
            qwen.native_reasoning_settings()
                .expect("Qwen native reasoning settings"),
            json!({
                "model": {
                    "reasoningEffort": "xhigh"
                }
            })
        );
    }

    #[tokio::test]
    async fn acp_writes_native_qwen_reasoning_fallback() {
        let workspace =
            std::env::temp_dir().join(format!("openteams-qwen-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&workspace)
            .await
            .expect("create workspace");
        let mut qwen = qwen_with_approval(None);
        qwen.thinking_effort = Some("high".to_string());
        let env = ExecutionEnv::new(Default::default(), false, String::new());

        let runtime_env = qwen
            .acp_runtime_env(&workspace, &env)
            .await
            .expect("ACP runtime environment");
        let settings_path = runtime_env
            .get("QWEN_CODE_SYSTEM_SETTINGS_PATH")
            .expect("Qwen system settings path");
        let settings: Value = serde_json::from_slice(
            &tokio::fs::read(settings_path)
                .await
                .expect("read Qwen system settings"),
        )
        .expect("parse Qwen system settings");

        assert_eq!(settings["model"]["reasoningEffort"], json!("high"));
        assert!(
            settings["mcpServers"]
                .as_object()
                .expect("server map")
                .is_empty()
        );

        tokio::fs::remove_dir_all(workspace)
            .await
            .expect("remove workspace");
    }

    #[test]
    fn all_structured_approval_modes_keep_qwen_permission_requests_enabled() {
        for mode in [
            AcpApprovalMode::Ask,
            AcpApprovalMode::AutoReject,
            AcpApprovalMode::AutoAllow,
        ] {
            let (program, args) = command_parts(&qwen_with_approval(Some(mode)));
            assert_eq!(program, "qwen");
            assert_eq!(args.iter().filter(|arg| *arg == "--acp").count(), 1);
            assert_eq!(
                args.iter().filter(|arg| *arg == "--approval-mode").count(),
                1
            );
            let approval_index = args
                .iter()
                .position(|arg| arg == "--approval-mode")
                .expect("approval mode flag");
            assert_eq!(
                args.get(approval_index + 1).map(String::as_str),
                Some("default")
            );
        }
    }

    #[test]
    fn explicit_acp_mode_is_used() {
        let qwen = qwen_with_approval(Some(AcpApprovalMode::AutoReject));
        assert_eq!(qwen.effective_approval_mode(), AcpApprovalMode::AutoReject);

        let default = qwen_with_approval(None);
        assert_eq!(default.effective_approval_mode(), AcpApprovalMode::Ask);
    }

    #[test]
    fn additional_params_cannot_override_qwen_approval_mode() {
        let mut qwen = qwen_with_approval(Some(AcpApprovalMode::Ask));
        qwen.cmd.additional_params = Some(vec!["--approval-mode auto".to_string()]);

        let error = qwen
            .build_command_builder()
            .expect_err("approval override must be rejected");
        assert!(error.to_string().contains("controlled by structured ACP"));
    }
}
