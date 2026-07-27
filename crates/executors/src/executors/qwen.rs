use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::{msg_store::MsgStore, shell::resolve_executable_path_blocking};

use super::acp::{
    AcpAccessMode, AcpAgentHarness, AcpApprovalMode, AcpApprovalPolicy, AcpAuthSelection,
    AcpCapabilityProbe, AcpClientServicePolicy, AcpExecutionOptions,
    mcp::{AcpMcpPolicy, resolve_effective_mcp_config, write_mcp_isolation_settings},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
    },
    mcp_config::{McpConfig, read_canonical_mcp_config},
    model_discovery::{
        ProviderKind, cli_model_commands, discover_from_sources, runner_config_paths,
    },
};

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
        description = "Per-run Qwen Code reasoning effort: off, low, medium, high, max, or a numeric token budget"
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
        if let Some(model) = self
            .model
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            harness = harness.with_model(model);
        }
        if let Some(effort) = self
            .thinking_effort
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            harness = harness.with_thought_level(effort);
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
        let path =
            write_mcp_isolation_settings(current_dir, "qwen-acp-settings", serde_json::json!({}))
                .await?;
        let mut runtime_env = env.clone();
        runtime_env.insert(
            "QWEN_CODE_SYSTEM_SETTINGS_PATH",
            path.to_string_lossy().to_string(),
        );
        Ok(runtime_env)
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for QwenCode {
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
    ) -> Result<Option<AcpCapabilityProbe>, ExecutorError> {
        Ok(Some(
            super::acp::runtime::probe_acp_command(
                self.build_command_builder()?.build_initial()?,
                current_dir,
                env,
                &self.cmd,
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

#[cfg(test)]
mod tests {
    use super::*;

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
