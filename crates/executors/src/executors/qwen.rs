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
    mcp::{
        AcpMcpPolicy, load_prepared_acp_mcp_config, pin_mcp_run_environment,
        prepare_acp_mcp_for_run, write_mcp_isolation_settings,
    },
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides, command_is_available,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, ExecutorPrompt, SpawnedChild,
        StandardCodingAgentExecutor,
        utils::{dotenv_has_nonempty_value, json_has_nonempty_string, read_json_file},
    },
    mcp_config::MemberMcpConfig,
    mcp_run::{McpRunContext, PreparedMcpRun},
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
const QWEN_STREAM_IDLE_TIMEOUT_MS_ENV: &str = "QWEN_STREAM_IDLE_TIMEOUT_MS";
const QWEN_SYSTEM_SETTINGS_ENV: &str = "QWEN_CODE_SYSTEM_SETTINGS_PATH";
// ACP agents can legitimately be quiet while a tool runs. The workflow runtime
// still detects genuinely stalled runs, so leave Qwen's shorter transport-level
// idle timer disabled unless the user has explicitly configured one.
const DEFAULT_QWEN_STREAM_IDLE_TIMEOUT_MS: &str = "0";

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

    async fn acp_harness(&self, env: &ExecutionEnv) -> Result<AcpAgentHarness, ExecutorError> {
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
        let effective = load_prepared_acp_mcp_config(env).await?;
        tracing::debug!(
            server_count = effective.servers.len(),
            config_hash = %effective.config_hash,
            "resolved effective ACP MCP configuration"
        );
        Ok(harness.with_mcp_servers(effective.servers))
    }

    fn acp_runtime_env(&self, env: &ExecutionEnv) -> ExecutionEnv {
        let mut runtime_env = env.clone();
        if !runtime_env.contains_key(QWEN_STREAM_IDLE_TIMEOUT_MS_ENV)
            && !self
                .cmd
                .env
                .as_ref()
                .is_some_and(|values| values.contains_key(QWEN_STREAM_IDLE_TIMEOUT_MS_ENV))
            && std::env::var_os(QWEN_STREAM_IDLE_TIMEOUT_MS_ENV).is_none()
        {
            runtime_env.insert(
                QWEN_STREAM_IDLE_TIMEOUT_MS_ENV,
                DEFAULT_QWEN_STREAM_IDLE_TIMEOUT_MS,
            );
        }
        runtime_env
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
    async fn prepare_mcp_for_run(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        let prepared =
            prepare_acp_mcp_for_run(canonical, context, env, &mut self.cmd, "qwen-acp-mcp")?;
        let (path, cleanup) = write_mcp_isolation_settings(
            context,
            "qwen-acp-settings",
            self.native_reasoning_settings()?,
        )?;
        pin_mcp_run_environment(
            env,
            &mut self.cmd,
            QWEN_SYSTEM_SETTINGS_ENV,
            path.to_string_lossy().into_owned(),
        );
        Ok(prepared.with_cleanup(cleanup))
    }

    fn overlay_acp_execution_options(&mut self, higher_priority: &AcpExecutionOptions) {
        let inherited = self.acp.clone().unwrap_or_default();
        self.acp = Some(inherited.overlay(higher_priority));
    }

    fn acp_full_access_enabled(&self) -> bool {
        self.acp
            .as_ref()
            .and_then(|options| options.access_mode)
            .unwrap_or_default()
            == AcpAccessMode::FullAccess
    }

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
        let runtime_env = self.acp_runtime_env(env);
        let harness = self.acp_harness(&runtime_env).await?;
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
        let runtime_env = self.acp_runtime_env(env);
        let harness = self.acp_harness(&runtime_env).await?;
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

    async fn spawn_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let command = self.build_command_builder()?.build_initial()?;
        let runtime_env = self.acp_runtime_env(env);
        let harness = self.acp_harness(&runtime_env).await?;
        let mut prompt = prompt.clone();
        prompt.text = self.append_prompt.combine_prompt(&prompt.text);
        harness
            .spawn_structured_with_command(
                current_dir,
                prompt,
                command,
                &runtime_env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    async fn spawn_follow_up_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let command = self.build_command_builder()?.build_follow_up(&[])?;
        let runtime_env = self.acp_runtime_env(env);
        let harness = self.acp_harness(&runtime_env).await?;
        let mut prompt = prompt.clone();
        prompt.text = self.append_prompt.combine_prompt(&prompt.text);
        harness
            .spawn_follow_up_structured_with_command(
                current_dir,
                prompt,
                session_id,
                command,
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

    fn run_context(workspace: &Path) -> McpRunContext {
        McpRunContext::new(workspace, uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
            .expect("run context")
    }

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
        let workspace = tempfile::tempdir().expect("workspace");
        let mut qwen = qwen_with_approval(None);
        qwen.thinking_effort = Some("high".to_string());
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());

        let prepared = qwen
            .prepare_mcp_for_run(
                &MemberMcpConfig::default(),
                &run_context(workspace.path()),
                &mut env,
            )
            .await
            .expect("Qwen MCP preparation");
        let runtime_env = qwen.acp_runtime_env(&env);
        let settings_path = std::path::PathBuf::from(
            runtime_env
                .get(QWEN_SYSTEM_SETTINGS_ENV)
                .expect("Qwen system settings path"),
        );
        let settings: Value = serde_json::from_slice(
            &tokio::fs::read(&settings_path)
                .await
                .expect("read Qwen system settings"),
        )
        .expect("parse Qwen system settings");

        assert_eq!(settings["model"]["reasoningEffort"], json!("high"));
        assert_eq!(
            runtime_env
                .get(QWEN_STREAM_IDLE_TIMEOUT_MS_ENV)
                .map(String::as_str),
            Some(DEFAULT_QWEN_STREAM_IDLE_TIMEOUT_MS)
        );
        assert!(
            settings["mcpServers"]
                .as_object()
                .expect("server map")
                .is_empty()
        );

        drop(prepared.into_cleanup());
        assert!(!settings_path.exists());
    }

    #[tokio::test]
    async fn explicit_qwen_stream_idle_timeout_is_preserved() {
        let qwen = qwen_with_approval(None);
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert(QWEN_STREAM_IDLE_TIMEOUT_MS_ENV, "900000");

        let runtime_env = qwen.acp_runtime_env(&env);

        assert_eq!(
            runtime_env
                .get(QWEN_STREAM_IDLE_TIMEOUT_MS_ENV)
                .map(String::as_str),
            Some("900000")
        );
    }

    #[tokio::test]
    async fn public_preparation_uses_member_canonical_not_legacy_allowlist() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut qwen = qwen_with_approval(None);
        qwen.acp_mcp_policy = AcpMcpPolicy {
            allowed_server_names: Some(Default::default()),
            disabled_server_names: Default::default(),
        };
        let canonical = MemberMcpConfig {
            mcp_servers: [("member-only".to_string(), json!({"command": "/bin/echo"}))]
                .into_iter()
                .collect(),
        };
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());

        let prepared = qwen
            .prepare_mcp_for_run(&canonical, &run_context(workspace.path()), &mut env)
            .await
            .expect("Qwen MCP preparation");
        let effective = load_prepared_acp_mcp_config(&env)
            .await
            .expect("prepared member config");

        assert_eq!(effective.server_names(), ["member-only".to_string()].into());
        drop(prepared.into_cleanup());
    }

    #[tokio::test]
    async fn explicit_empty_member_map_overrides_ambient_qwen_mcp() {
        let workspace = tempfile::tempdir().expect("workspace");
        let home = workspace.path().join("home");
        let vendor_dir = home.join(".qwen");
        tokio::fs::create_dir_all(&vendor_dir)
            .await
            .expect("Qwen vendor directory");
        let ambient_path = vendor_dir.join("settings.json");
        tokio::fs::write(
            &ambient_path,
            br#"{"mcpServers":{"ambient-global":{"command":"must-not-run"}}}"#,
        )
        .await
        .expect("ambient Qwen settings");
        let mut qwen = qwen_with_approval(None);
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert("HOME", home.to_string_lossy().into_owned());

        let prepared = qwen
            .prepare_mcp_for_run(
                &MemberMcpConfig::default(),
                &run_context(workspace.path()),
                &mut env,
            )
            .await
            .expect("empty Qwen MCP preparation");
        let effective = load_prepared_acp_mcp_config(&env)
            .await
            .expect("prepared empty Qwen MCP");
        let settings_path = std::path::PathBuf::from(
            env.get(QWEN_SYSTEM_SETTINGS_ENV)
                .expect("Qwen system settings path"),
        );
        let settings: Value = serde_json::from_slice(
            &tokio::fs::read(&settings_path)
                .await
                .expect("read Qwen system settings"),
        )
        .expect("parse Qwen system settings");

        assert!(ambient_path.is_file());
        assert_ne!(settings_path, ambient_path);
        assert!(effective.server_names().is_empty());
        assert!(
            settings["mcpServers"]
                .as_object()
                .expect("Qwen system MCP override")
                .is_empty()
        );

        drop(prepared.into_cleanup());
        assert!(!settings_path.exists());
        assert!(
            ambient_path.is_file(),
            "ambient settings must remain untouched"
        );
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
