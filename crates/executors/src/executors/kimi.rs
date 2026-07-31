use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::{msg_store::MsgStore, shell::resolve_executable_path_blocking};

use super::acp::{
    AcpAccessMode, AcpAgentHarness, AcpApprovalMode, AcpApprovalPolicy, AcpAuthSelection,
    AcpCapabilityProbe, AcpClientServicePolicy, AcpConfigChoice, AcpConfigOptionKind,
    AcpConfigOptionSnapshot, AcpConfigSource, AcpExecutionOptions,
    mcp::{AcpMcpPolicy, resolve_effective_mcp_config},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
    },
    mcp_config::{McpConfig, read_canonical_mcp_config},
    model_discovery::{discover_model_map_from_cli_command, read_config_value},
};

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct KimiCode {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Kimi model alias configured in Kimi Code CLI")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Per-run Kimi thinking effort: low, high, or max")]
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

impl KimiCode {
    const BASE_COMMAND: &'static str = "kimi";
    const TERMINAL_AUTH_METHOD: &'static str = "login";

    pub fn base_command() -> &'static str {
        Self::BASE_COMMAND
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params(["acp"]),
            &self.cmd,
        )
    }

    fn provider_list_command(&self) -> Result<CommandBuilder, CommandBuildError> {
        apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params(["provider", "list", "--json"]),
            &self.cmd,
        )
    }

    fn validate_auth_selection(&self) -> Result<(), ExecutorError> {
        match self.acp.as_ref().and_then(|options| options.auth.as_ref()) {
            Some(AcpAuthSelection::MethodId { method_id }) => {
                Err(unsupported_terminal_auth(method_id))
            }
            Some(AcpAuthSelection::Auto) | None => Ok(()),
        }
    }

    async fn acp_harness(&self, env: &ExecutionEnv) -> Result<AcpAgentHarness, ExecutorError> {
        let options = self.acp.clone().unwrap_or_default();
        let approval_policy = match options.approval_mode.unwrap_or_default() {
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
            .with_required_session_mode("mode", "default")
            .with_additional_directories(additional_directories)
            .with_client_services(AcpClientServicePolicy {
                read_text_file: true,
                write_text_file: true,
                terminal: true,
                full_access,
                ..AcpClientServicePolicy::default()
            });
        self.validate_auth_selection()?;

        let config_overrides = options.config_overrides.as_deref().unwrap_or_default();
        let has_model_override = config_overrides.iter().any(|selection| {
            selection.category_snapshot.as_deref() == Some("model")
                || selection.option_id.eq_ignore_ascii_case("model")
        });
        let has_thought_override = config_overrides.iter().any(|selection| {
            selection.category_snapshot.as_deref() == Some("thought_level")
                || matches!(
                    normalized_config_key(&selection.option_id).as_str(),
                    "thinking" | "thoughtlevel"
                )
        });
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|_| !has_model_override)
        {
            harness = harness.with_model(model);
        }
        if let Some(effort) = self
            .thinking_effort
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|_| !has_thought_override)
        {
            harness = harness.with_native_thought_level_fallback(effort);
        }
        for selection in config_overrides {
            harness = harness.with_config_override(selection);
        }

        let canonical = match kimi_mcp_config_path(Some(env)) {
            Some(path) => read_canonical_mcp_config(&path, &McpConfig::canonical_acp()).await?,
            None => serde_json::json!({ "mcpServers": {} }),
        };
        let effective = resolve_effective_mcp_config(&canonical, &self.acp_mcp_policy)?;
        tracing::debug!(
            server_count = effective.servers.len(),
            config_hash = %effective.config_hash,
            "resolved effective Kimi ACP MCP configuration"
        );
        Ok(harness.with_mcp_servers(effective.servers))
    }

    async fn configured_default_model(
        &self,
        env: &ExecutionEnv,
    ) -> Result<Option<String>, ExecutorError> {
        let Some(path) = kimi_code_home(Some(env)).map(|home| home.join("config.toml")) else {
            return Ok(None);
        };
        let Some(config) = read_config_value(&path).await? else {
            return Ok(None);
        };
        Ok(config
            .get("default_model")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned))
    }

    fn discovered_model_option(
        &self,
        models: Vec<String>,
        default_model: Option<&str>,
    ) -> AcpConfigOptionSnapshot {
        let current_value = self
            .model
            .as_deref()
            .or(default_model)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|selected| models.iter().any(|model| model == selected))
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| models[0].clone());
        AcpConfigOptionSnapshot {
            id: "model".to_string(),
            name: "Model".to_string(),
            description: Some("Models reported by Kimi Code CLI".to_string()),
            category: Some("model".to_string()),
            kind: AcpConfigOptionKind::Select {
                current_value,
                options: models
                    .into_iter()
                    .map(|model| AcpConfigChoice {
                        name: model.clone(),
                        value: model,
                        description: None,
                    })
                    .collect(),
            },
        }
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for KimiCode {
    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn list_models(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        let mut models = discover_model_map_from_cli_command(
            current_dir,
            env,
            &self.cmd,
            self.provider_list_command()?,
        )
        .await?
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            models.insert(model.to_string());
        }
        Ok((!models.is_empty()).then(|| models.into_iter().collect()))
    }

    async fn probe_acp(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
        auth_method_id: Option<&str>,
    ) -> Result<Option<AcpCapabilityProbe>, ExecutorError> {
        if let Some(method_id) = auth_method_id {
            return Err(unsupported_terminal_auth(method_id));
        }
        let mut probe = super::acp::runtime::probe_acp_command_without_session(
            self.build_command_builder()?.build_initial()?,
            current_dir,
            env,
            &self.cmd,
            None,
        )
        .await?;
        if let Some(models) = self.list_models(current_dir, env).await?
            && !models.is_empty()
        {
            let default_model = self.configured_default_model(env).await?;
            probe.config_source = AcpConfigSource::Stable;
            probe
                .config_options
                .push(self.discovered_model_option(models, default_model.as_deref()));
        }
        Ok(Some(probe))
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let harness = self.acp_harness(env).await?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let command = self.build_command_builder()?.build_initial()?;
        harness
            .spawn_with_command(
                current_dir,
                combined_prompt,
                command,
                env,
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
        let harness = self.acp_harness(env).await?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let command = self.build_command_builder()?.build_follow_up(&[])?;
        harness
            .spawn_follow_up_with_command(
                current_dir,
                combined_prompt,
                session_id,
                command,
                env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        super::acp::normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<PathBuf> {
        kimi_mcp_config_path(None)
    }

    fn native_skill_discovery_roots(&self) -> Vec<PathBuf> {
        let mut roots = dirs::home_dir()
            .map(|home| vec![home.join(".agents").join("skills")])
            .unwrap_or_default();
        if let Some(home) = kimi_code_home(None) {
            roots.push(home.join("skills"));
        }
        roots
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        let command = self
            .cmd
            .base_command_override
            .as_deref()
            .and_then(shlex::split)
            .and_then(|parts| parts.into_iter().next())
            .unwrap_or_else(|| Self::BASE_COMMAND.to_string());
        if resolve_executable_path_blocking(&command).is_some() {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

fn normalized_config_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn unsupported_terminal_auth(method_id: &str) -> ExecutorError {
    let message = if method_id == KimiCode::TERMINAL_AUTH_METHOD {
        "Kimi ACP login is terminal-based; run `kimi login` and keep ACP auth set to auto"
            .to_string()
    } else {
        format!(
            "Kimi ACP authentication method `{method_id}` is not supported; run `kimi login` and keep ACP auth set to auto"
        )
    };
    ExecutorError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

fn kimi_code_home(env: Option<&ExecutionEnv>) -> Option<PathBuf> {
    let configured = env
        .and_then(|env| env.get("KIMI_CODE_HOME").cloned())
        .or_else(|| std::env::var("KIMI_CODE_HOME").ok());
    configured
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".kimi-code")))
}

fn kimi_mcp_config_path(env: Option<&ExecutionEnv>) -> Option<PathBuf> {
    kimi_code_home(env).map(|home| home.join("mcp.json"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::env::RepoContext;

    fn kimi() -> KimiCode {
        KimiCode {
            append_prompt: AppendPrompt::default(),
            model: Some("kimi-code/k3".to_string()),
            thinking_effort: Some("high".to_string()),
            acp: None,
            cmd: CmdOverrides::default(),
            acp_mcp_policy: AcpMcpPolicy::default(),
            approvals: None,
        }
    }

    #[test]
    fn command_builder_uses_kimi_acp_subcommand() {
        let (_program, args) = kimi()
            .build_command_builder()
            .unwrap()
            .build_initial()
            .unwrap()
            .into_parts_for_test();
        assert!(args.iter().any(|arg| arg == "acp"));
        assert!(!args.iter().any(|arg| arg == "--print"));
        assert!(!args.iter().any(|arg| arg == "--output-format"));
        assert!(!args.iter().any(|arg| arg == "--model"));
    }

    #[test]
    fn model_discovery_uses_provider_list_json() {
        let (_program, args) = kimi()
            .provider_list_command()
            .unwrap()
            .build_initial()
            .unwrap()
            .into_parts_for_test();
        assert_eq!(args, vec!["provider", "list", "--json"]);
    }

    #[test]
    fn discovered_models_become_stable_acp_model_option() {
        let option = kimi().discovered_model_option(
            vec![
                "kimi-code/kimi-for-coding".to_string(),
                "kimi-code/k3".to_string(),
            ],
            Some("kimi-code/k3"),
        );
        assert_eq!(option.category.as_deref(), Some("model"));
        let AcpConfigOptionKind::Select {
            current_value,
            options,
        } = option.kind
        else {
            panic!("expected select option");
        };
        assert_eq!(current_value, "kimi-code/k3");
        assert_eq!(options.len(), 2);
    }

    #[test]
    fn explicit_terminal_auth_is_rejected_with_login_guidance() {
        let mut executor = kimi();
        executor.acp = Some(AcpExecutionOptions {
            auth: Some(AcpAuthSelection::MethodId {
                method_id: "login".to_string(),
            }),
            ..Default::default()
        });
        let error = executor.validate_auth_selection().unwrap_err().to_string();
        assert!(error.contains("kimi login"));
        assert!(error.contains("auth set to auto"));
    }

    #[tokio::test]
    async fn every_approval_policy_enforces_default_agent_mode() {
        let temp = TempDir::new().expect("create Kimi test home");
        let mut env = ExecutionEnv::new(
            RepoContext::new(temp.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        );
        env.insert("KIMI_CODE_HOME", temp.path().to_string_lossy().into_owned());

        for approval_mode in [
            AcpApprovalMode::Ask,
            AcpApprovalMode::AutoAllow,
            AcpApprovalMode::AutoReject,
        ] {
            let mut executor = kimi();
            executor.acp = Some(AcpExecutionOptions {
                approval_mode: Some(approval_mode),
                config_overrides: Some(vec![super::super::acp::AcpConfigOverride {
                    option_id: "mode".to_string(),
                    value: super::super::acp::AcpConfigValue::ValueId {
                        value: "yolo".to_string(),
                    },
                    label_snapshot: Some("Mode".to_string()),
                    category_snapshot: Some("mode".to_string()),
                }]),
                ..Default::default()
            });

            let harness = executor
                .acp_harness(&env)
                .await
                .expect("build Kimi harness");
            let (option_id, value) = harness.required_session_mode().expect("required Kimi mode");
            assert_eq!(option_id, "mode");
            assert_eq!(
                value,
                &agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id("default")
            );
        }
    }
}
