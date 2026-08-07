use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use super::acp::{
    AcpAccessMode, AcpAgentHarness, AcpApprovalMode, AcpApprovalPolicy, AcpAuthSelection,
    AcpCapabilityProbe, AcpClientServicePolicy, AcpExecutionOptions,
    mcp::{AcpMcpPolicy, resolve_effective_mcp_config},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuilder, apply_overrides, command_is_available},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, ExecutorPrompt, SpawnedChild,
        StandardCodingAgentExecutor,
    },
    mcp_config::{McpConfig, read_canonical_mcp_config},
};

#[derive(Derivative, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Hermes {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Model to use, as advertised by the Hermes ACP probe")]
    pub model: Option<String>,
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

impl Hermes {
    const BASE_COMMAND: &'static str = "hermes";

    fn build_command_builder(&self) -> Result<CommandBuilder, crate::command::CommandBuildError> {
        apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params(["acp"]),
            &self.cmd,
        )
    }

    async fn acp_harness(&self) -> Result<AcpAgentHarness, ExecutorError> {
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
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|_| !has_model_override)
        {
            harness = harness.with_model(model);
        }
        for selection in config_overrides {
            harness = harness.with_config_override(selection);
        }
        let canonical = match self.default_mcp_config_path() {
            Some(path) => read_canonical_mcp_config(&path, &McpConfig::canonical_acp()).await?,
            None => serde_json::json!({ "mcpServers": {} }),
        };
        let effective = resolve_effective_mcp_config(&canonical, &self.acp_mcp_policy)?;
        tracing::debug!(
            server_count = effective.servers.len(),
            config_hash = %effective.config_hash,
            "resolved effective Hermes ACP MCP configuration"
        );
        Ok(harness.with_mcp_servers(effective.servers))
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for Hermes {
    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        self.authentication_detected(&env, &[], false)
    }

    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn list_models(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        Ok(self
            .probe_acp(current_dir, env, None)
            .await?
            .and_then(|probe| probe.model_ids()))
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
        let harness = self.acp_harness().await?;
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
        let harness = self.acp_harness().await?;
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

    async fn spawn_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let harness = self.acp_harness().await?;
        let command = self.build_command_builder()?.build_initial()?;
        let mut prompt = prompt.clone();
        prompt.text = self.append_prompt.combine_prompt(&prompt.text);
        harness
            .spawn_structured_with_command(
                current_dir,
                prompt,
                command,
                env,
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
        let harness = self.acp_harness().await?;
        let command = self.build_command_builder()?.build_follow_up(&[])?;
        let mut prompt = prompt.clone();
        prompt.text = self.append_prompt.combine_prompt(&prompt.text);
        harness
            .spawn_follow_up_structured_with_command(
                current_dir,
                prompt,
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

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".hermes").join("mcp.json"))
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if command_is_available(Self::BASE_COMMAND, &self.cmd) {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hermes() -> Hermes {
        Hermes {
            append_prompt: AppendPrompt::default(),
            model: None,
            acp: None,
            cmd: CmdOverrides::default(),
            acp_mcp_policy: AcpMcpPolicy::default(),
            approvals: None,
        }
    }

    #[test]
    fn command_builder_uses_hermes_acp_subcommand() {
        let (program, args) = hermes()
            .build_command_builder()
            .expect("build command")
            .build_initial()
            .expect("build initial")
            .into_parts_for_test();
        assert_eq!(program, "hermes");
        assert_eq!(args, vec!["acp"]);
    }

    #[test]
    fn empty_model_does_not_inject_model_config() {
        let hermes = hermes();
        assert!(
            hermes
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .is_none(),
            "default Hermes profile must not carry a model so the ACP probe stays authoritative"
        );
    }

    #[test]
    fn explicit_model_is_forwarded_when_no_config_override_present() {
        let mut hermes = hermes();
        hermes.model = Some("hermes-pro".to_string());
        let has_model_override = hermes
            .acp
            .as_ref()
            .and_then(|options| options.config_overrides.as_deref())
            .unwrap_or_default()
            .iter()
            .any(|selection| {
                selection.category_snapshot.as_deref() == Some("model")
                    || selection.option_id.eq_ignore_ascii_case("model")
            });
        assert!(!has_model_override);
        assert_eq!(hermes.model.as_deref(), Some("hermes-pro"));
    }

    #[test]
    fn hermes_is_not_authenticated_by_default() {
        let env = ExecutionEnv::new(Default::default(), false, String::new());
        assert!(!hermes().is_authenticated(&env));
    }
}
