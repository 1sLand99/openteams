use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use super::acp::{
    AcpAccessMode, AcpAgentHarness, AcpApprovalMode, AcpApprovalPolicy, AcpAuthSelection,
    AcpCapabilityProbe, AcpClientServicePolicy, AcpExecutionOptions, AcpResumePolicy,
    mcp::{AcpMcpPolicy, resolve_effective_mcp_config},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuilder, apply_overrides, command_is_available},
    env::ExecutionEnv,
    executors::{
        AcpModelFallback, AcpProbeAuthState, AcpProbeInterpretation, AppendPrompt,
        AvailabilityInfo, ExecutorError, ExecutorPrompt, SpawnedChild, StandardCodingAgentExecutor,
    },
    mcp_config::{McpConfig, read_canonical_mcp_config},
};

mod probe;

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
    const SKIP_CONFIGURED_MCP_ENV: &'static str = "HERMES_ACP_SKIP_CONFIGURED_MCP";

    fn build_command_builder(&self) -> Result<CommandBuilder, crate::command::CommandBuildError> {
        apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params(["acp"]),
            &self.cmd,
        )
    }

    async fn acp_harness(&self) -> Result<AcpAgentHarness, ExecutorError> {
        let options = self.acp.clone().unwrap_or_default();
        if options.access_mode == Some(AcpAccessMode::WorkspaceOnly) {
            return Err(ExecutorError::Configuration(
                "Hermes native tools cannot enforce ACP workspace-only access; use full access in a trusted workspace"
                    .to_string(),
            ));
        }
        if options
            .additional_directories
            .as_ref()
            .is_some_and(|directories| !directories.is_empty())
        {
            return Err(ExecutorError::Configuration(
                "Hermes ACP does not support additional directories".to_string(),
            ));
        }
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
            .with_resume_policy(AcpResumePolicy::RefusalMeansInvalidSession)
            .with_additional_directories(additional_directories)
            .with_client_services(AcpClientServicePolicy {
                read_text_file: true,
                write_text_file: true,
                terminal: true,
                full_access,
                ..AcpClientServicePolicy::default()
            });
        if let Some(AcpAuthSelection::MethodId { method_id }) = options.auth {
            if method_id == probe::HERMES_SETUP_AUTH_METHOD_ID {
                return Err(ExecutorError::AuthRequired(
                    "Hermes provider setup is required; run `hermes acp --setup` in a terminal"
                        .to_string(),
                ));
            }
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
            Some(path) => read_canonical_mcp_config(&path, &McpConfig::hermes()).await?,
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

    fn isolated_env(&self, env: &ExecutionEnv) -> ExecutionEnv {
        let mut isolated = env.clone();
        isolated.insert(Self::SKIP_CONFIGURED_MCP_ENV, "1");
        isolated
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for Hermes {
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

    fn acp_model_fallback(&self) -> AcpModelFallback {
        AcpModelFallback::Disabled
    }

    fn interpret_acp_probe(&self, probe: &AcpCapabilityProbe) -> AcpProbeInterpretation {
        AcpProbeInterpretation {
            models: probe.model_ids(),
            auth_state: Some(if probe::provider_needs_setup(probe) {
                AcpProbeAuthState::Unauthenticated
            } else {
                AcpProbeAuthState::Authenticated
            }),
            model_fallback: self.acp_model_fallback(),
        }
    }

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
        let isolated_env = self.isolated_env(env);
        Ok(Some(
            probe::probe_hermes_acp_command(
                self.build_command_builder()?.build_initial()?,
                current_dir,
                &isolated_env,
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
        let isolated_env = self.isolated_env(env);
        harness
            .spawn_with_command(
                current_dir,
                combined_prompt,
                command,
                &isolated_env,
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
        let isolated_env = self.isolated_env(env);
        harness
            .spawn_follow_up_with_command(
                current_dir,
                combined_prompt,
                session_id,
                command,
                &isolated_env,
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
        let isolated_env = self.isolated_env(env);
        harness
            .spawn_structured_with_command(
                current_dir,
                prompt,
                command,
                &isolated_env,
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
        let isolated_env = self.isolated_env(env);
        harness
            .spawn_follow_up_structured_with_command(
                current_dir,
                prompt,
                session_id,
                command,
                &isolated_env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        super::acp::normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".hermes").join("config.yaml"))
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

    #[test]
    fn default_mcp_config_path_is_hermes_config_yaml() {
        let path = hermes()
            .default_mcp_config_path()
            .expect("hermes must expose a default MCP config path");
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some("config.yaml"),
            "Hermes stores MCP servers under mcp_servers in ~/.hermes/config.yaml, not mcp.json"
        );
        assert_eq!(
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str()),
            Some(".hermes")
        );
    }

    #[test]
    fn capabilities_only_advertise_implemented_hermes_features() {
        assert_eq!(
            crate::executors::CodingAgent::Hermes(hermes()).capabilities(),
            vec![crate::executors::BaseAgentCapability::ContextUsage]
        );
    }
}
