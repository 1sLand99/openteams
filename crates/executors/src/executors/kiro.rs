use std::{path::Path, process::Stdio, sync::Arc, time::Duration};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use super::acp::{
    AcpAccessMode, AcpAgentHarness, AcpApprovalMode, AcpApprovalPolicy, AcpAuthSelection,
    AcpCapabilityProbe, AcpClientServicePolicy, AcpExecutionOptions, AcpResumePolicy,
    mcp::{AcpMcpPolicy, load_prepared_acp_mcp_config, prepare_acp_mcp_for_run},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuilder, CommandParts, apply_overrides, command_is_available},
    env::ExecutionEnv,
    executors::{
        AcpModelFallback, AcpProbeAuthState, AcpProbeInterpretation, AppendPrompt,
        AvailabilityInfo, ExecutorError, ExecutorPrompt, SpawnedChild, StandardCodingAgentExecutor,
    },
    mcp_config::MemberMcpConfig,
    mcp_run::{McpRunContext, PreparedMcpRun},
};

#[derive(Derivative, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct KiroCli {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Model to use, as advertised by the Kiro CLI ACP probe")]
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

impl KiroCli {
    const AUTH_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
    const BASE_COMMAND: &'static str = "kiro-cli";
    const API_KEY_ENV: &'static str = "KIRO_API_KEY";
    const RESTRICTED_RUNTIME_ARGUMENTS: &'static [&'static str] =
        &["--agent", "--trust-all-tools", "--mode", "session/set_mode"];

    fn build_command_builder(&self) -> Result<CommandBuilder, crate::command::CommandBuildError> {
        Self::validate_command_overrides(&self.cmd)?;
        apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params(["acp"]),
            &self.cmd,
        )
    }

    fn validate_command_overrides(
        overrides: &CmdOverrides,
    ) -> Result<(), crate::command::CommandBuildError> {
        let mut values = overrides.parsed_additional_params()?;
        if let Some(base) = &overrides.base_command_override {
            values.extend(
                base.replace(['=', '\t', '\n'], " ")
                    .split_ascii_whitespace()
                    .map(|token| token.trim_matches(['\'', '"']).to_string()),
            );
        }

        if let Some(argument) = values.iter().find_map(|token| {
            let name = token
                .split_once('=')
                .map_or(token.as_str(), |(name, _)| name);
            Self::RESTRICTED_RUNTIME_ARGUMENTS
                .iter()
                .copied()
                .find(|restricted| name == *restricted)
        }) {
            return Err(crate::command::CommandBuildError::InvalidShellParams(
                format!(
                    "Kiro CLI {argument} is controlled by the default ACP agent and OpenTeams approval policy"
                ),
            ));
        }
        Ok(())
    }

    fn diagnostic_command_overrides(&self) -> CmdOverrides {
        CmdOverrides {
            additional_params: None,
            ..self.cmd.clone()
        }
    }

    fn build_version_command(&self) -> Result<CommandParts, ExecutorError> {
        let overrides = self.diagnostic_command_overrides();
        Self::validate_command_overrides(&overrides)?;
        Ok(apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params(["--version"]),
            &overrides,
        )?
        .build_initial()?)
    }

    fn build_whoami_command(&self) -> Result<CommandParts, ExecutorError> {
        let overrides = self.diagnostic_command_overrides();
        Self::validate_command_overrides(&overrides)?;
        Ok(apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params(["whoami", "--format", "json"]),
            &overrides,
        )?
        .build_initial()?)
    }

    async fn acp_harness(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<AcpAgentHarness, ExecutorError> {
        if !self.probe_authentication(current_dir, env).await? {
            return Err(ExecutorError::AuthRequired(
                "Kiro CLI is not authenticated; run `kiro-cli login` or configure KIRO_API_KEY in the member environment"
                    .to_string(),
            ));
        }

        let options = self.acp.clone().unwrap_or_default();
        if matches!(options.auth, Some(AcpAuthSelection::MethodId { .. })) {
            return Err(ExecutorError::Configuration(
                "Kiro CLI uses its local login or KIRO_API_KEY and does not advertise ACP authentication methods"
                    .to_string(),
            ));
        }
        if options
            .additional_directories
            .as_ref()
            .is_some_and(|directories| !directories.is_empty())
        {
            return Err(ExecutorError::Configuration(
                "Kiro CLI ACP does not support additional directories".to_string(),
            ));
        }

        let approval_policy = match options.approval_mode.unwrap_or_default() {
            AcpApprovalMode::Ask => AcpApprovalPolicy::Ask,
            AcpApprovalMode::AutoAllow => AcpApprovalPolicy::AutoAllow,
            AcpApprovalMode::AutoReject => AcpApprovalPolicy::AutoReject,
        };
        let full_access = options.access_mode.unwrap_or_default() == AcpAccessMode::FullAccess;
        let mut harness = AcpAgentHarness::new()
            .with_approval_policy(approval_policy)
            .with_resume_policy(AcpResumePolicy::RefusalMeansInvalidSession)
            .with_client_services(AcpClientServicePolicy {
                read_text_file: true,
                write_text_file: true,
                terminal: true,
                full_access,
                ..AcpClientServicePolicy::default()
            });

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

        let effective = load_prepared_acp_mcp_config(env).await?;
        tracing::debug!(
            server_count = effective.servers.len(),
            config_hash = %effective.config_hash,
            "resolved effective Kiro CLI ACP MCP configuration"
        );
        Ok(harness.with_mcp_servers(effective.servers))
    }
}

fn whoami_reports_account(output: &[u8]) -> bool {
    let Ok(serde_json::Value::Object(account)) = serde_json::from_slice(output) else {
        return false;
    };
    if account.is_empty() {
        return false;
    }

    for key in ["loggedIn", "authenticated", "isAuthenticated"] {
        if let Some(authenticated) = account.get(key).and_then(serde_json::Value::as_bool) {
            return authenticated;
        }
    }
    account
        .get("account")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|account| !account.is_empty())
}

#[async_trait]
impl StandardCodingAgentExecutor for KiroCli {
    async fn prepare_mcp_for_run(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        prepare_acp_mcp_for_run(canonical, context, env, &mut self.cmd, "kiro-acp-mcp")
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

    fn acp_model_fallback(&self) -> AcpModelFallback {
        AcpModelFallback::Disabled
    }

    fn interpret_acp_probe(&self, probe: &AcpCapabilityProbe) -> AcpProbeInterpretation {
        AcpProbeInterpretation {
            models: probe.model_ids(),
            auth_state: Some(AcpProbeAuthState::Authenticated),
            model_fallback: self.acp_model_fallback(),
        }
    }

    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        self.authentication_detected(&env, &[Self::API_KEY_ENV], false)
    }

    async fn probe_authentication(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<bool, ExecutorError> {
        if self.is_authenticated(env) {
            return Ok(true);
        }

        let command_parts = self.build_whoami_command()?;
        let (program, args) = command_parts.into_resolved().await?;
        let mut command = tokio::process::Command::new(program);
        command
            .kill_on_drop(true)
            .current_dir(current_dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut command);

        let output = tokio::time::timeout(Self::AUTH_PROBE_TIMEOUT, command.output())
            .await
            .map_err(|_| {
                ExecutorError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Kiro CLI authentication probe timed out",
                ))
            })??;
        Ok(output.status.success() && whoami_reports_account(&output.stdout))
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
        if auth_method_id.is_some()
            || self.acp.as_ref().is_some_and(|options| {
                matches!(options.auth, Some(AcpAuthSelection::MethodId { .. }))
            })
        {
            return Err(ExecutorError::Configuration(
                "Kiro CLI does not advertise ACP authentication methods".to_string(),
            ));
        }
        if !self.probe_authentication(current_dir, env).await? {
            return Err(ExecutorError::AuthRequired(
                "Kiro CLI is not authenticated; run `kiro-cli login` or configure KIRO_API_KEY in the member environment"
                    .to_string(),
            ));
        }

        Ok(Some(
            super::acp::runtime::probe_acp_command(
                self.build_command_builder()?.build_initial()?,
                current_dir,
                env,
                &self.cmd,
                None,
            )
            .await?,
        ))
    }

    fn runtime_command_for_diagnostics(&self) -> Result<Option<CommandParts>, ExecutorError> {
        Ok(Some(self.build_command_builder()?.build_initial()?))
    }

    fn version_command_for_diagnostics(&self) -> Result<Option<CommandParts>, ExecutorError> {
        Ok(Some(self.build_version_command()?))
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let harness = self.acp_harness(current_dir, env).await?;
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
        let harness = self.acp_harness(current_dir, env).await?;
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
        let harness = self.acp_harness(current_dir, env).await?;
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
        let harness = self.acp_harness(current_dir, env).await?;
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
        None
    }

    fn supports_mcp(&self) -> bool {
        true
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
    use crate::{
        executors::{
            BaseAgentCapability, BaseCodingAgent, CodingAgent,
            acp::mcp::PREPARED_ACP_MCP_SNAPSHOT_ENV,
        },
        profile::{ExecutorConfigs, ExecutorProfileId},
    };

    const PREPARED_ACP_MCP_HASH_ENV: &str = "OPENTEAMS_ACP_MCP_SNAPSHOT_HASH";

    fn kiro() -> KiroCli {
        KiroCli::default()
    }

    fn run_context(workspace: &Path) -> McpRunContext {
        McpRunContext::new(workspace, uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
            .expect("run context")
    }

    #[test]
    fn command_builder_uses_only_default_kiro_acp_agent() {
        let (program, args) = kiro()
            .build_command_builder()
            .expect("build Kiro command")
            .build_initial()
            .expect("build initial Kiro command")
            .into_parts_for_test();
        assert_eq!(program, "kiro-cli");
        assert_eq!(args, vec!["acp"]);
        assert!(!args.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "--agent" | "--trust-all-tools" | "--mode" | "session/set_mode"
            )
        }));
    }

    #[test]
    fn runtime_command_rejects_agent_approval_and_mode_overrides() {
        for additional_params in [
            vec!["--agent planner".to_string()],
            vec!["--agent=planner".to_string()],
            vec!["--trust-all-tools".to_string()],
            vec!["--trust-all-tools=true".to_string()],
            vec!["--mode planner".to_string()],
            vec!["--mode=planner".to_string()],
            vec!["session/set_mode planner".to_string()],
        ] {
            let mut executor = kiro();
            executor.cmd.additional_params = Some(additional_params.clone());

            let error = executor
                .build_command_builder()
                .expect_err("Kiro runtime override must be rejected");

            assert!(
                error
                    .to_string()
                    .contains("controlled by the default ACP agent"),
                "unexpected error for {additional_params:?}: {error}"
            );
        }
    }

    #[test]
    fn base_command_cannot_embed_agent_approval_or_mode_overrides() {
        for base_command_override in [
            "kiro-cli --agent planner",
            "kiro-cli --trust-all-tools",
            "kiro-cli --mode=planner",
            "sh -c 'kiro-cli acp session/set_mode planner'",
        ] {
            let mut executor = kiro();
            executor.cmd.base_command_override = Some(base_command_override.to_string());

            assert!(
                executor.build_command_builder().is_err(),
                "runtime base override must be rejected: {base_command_override}"
            );
            assert!(
                executor.build_version_command().is_err(),
                "diagnostic base override must be rejected: {base_command_override}"
            );
            assert!(
                executor.build_whoami_command().is_err(),
                "authentication base override must be rejected: {base_command_override}"
            );
        }
    }

    #[test]
    fn non_conflicting_runtime_arguments_remain_supported() {
        let mut executor = kiro();
        executor.cmd.additional_params = Some(vec!["--future-acp-option=value".to_string()]);

        let (program, args) = executor
            .build_command_builder()
            .expect("non-conflicting Kiro runtime argument")
            .build_initial()
            .expect("build Kiro runtime command")
            .into_parts_for_test();

        assert_eq!(program, "kiro-cli");
        assert_eq!(args, vec!["acp", "--future-acp-option=value"]);
    }

    #[test]
    fn diagnostic_commands_are_executor_owned_and_do_not_reuse_acp_arguments() {
        let mut executor = kiro();
        executor.cmd.additional_params = Some(vec!["--future-acp-option".to_string()]);

        let (version_program, version_args) = executor
            .version_command_for_diagnostics()
            .expect("build version command")
            .expect("Kiro version command")
            .into_parts_for_test();
        assert_eq!(version_program, "kiro-cli");
        assert_eq!(version_args, vec!["--version"]);

        let (whoami_program, whoami_args) = executor
            .build_whoami_command()
            .expect("build whoami command")
            .into_parts_for_test();
        assert_eq!(whoami_program, "kiro-cli");
        assert_eq!(whoami_args, vec!["whoami", "--format", "json"]);
    }

    #[test]
    fn whoami_output_is_reduced_to_a_boolean() {
        assert!(whoami_reports_account(
            br#"{"account":{"email":"sensitive@example.com"}}"#
        ));
        assert!(whoami_reports_account(br#"{"loggedIn":true}"#));
        assert!(!whoami_reports_account(br#"{"loggedIn":false}"#));
        assert!(!whoami_reports_account(b"{}"));
        assert!(!whoami_reports_account(b"not json"));
    }

    #[tokio::test]
    async fn nonempty_api_key_authenticates_without_running_whoami() {
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert(KiroCli::API_KEY_ENV, "fixture-secret");

        assert!(kiro().is_authenticated(&env));
        assert!(
            kiro()
                .probe_authentication(Path::new("."), &env)
                .await
                .expect("API key authentication probe")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn local_login_probe_accepts_only_an_explicit_account_response() {
        let mut executor = kiro();
        executor.cmd.base_command_override =
            Some("sh -c 'printf \"{\\\"account\\\":{\\\"id\\\":\\\"fixture\\\"}}\"'".to_string());
        let env = ExecutionEnv::new(Default::default(), false, String::new());

        assert!(
            executor
                .probe_authentication(Path::new("."), &env)
                .await
                .expect("local account probe")
        );

        executor.cmd.base_command_override =
            Some("sh -c 'printf \"{\\\"error\\\":\\\"not logged in\\\"}\"'".to_string());
        assert!(
            !executor
                .probe_authentication(Path::new("."), &env)
                .await
                .expect("unauthenticated account probe")
        );
    }

    #[tokio::test]
    async fn explicit_acp_auth_and_additional_directories_fail_before_spawn() {
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert(KiroCli::API_KEY_ENV, "fixture-secret");

        let mut executor = kiro();
        executor.acp = Some(AcpExecutionOptions {
            auth: Some(AcpAuthSelection::MethodId {
                method_id: "unsupported".to_string(),
            }),
            ..AcpExecutionOptions::default()
        });
        let auth_error = match executor.acp_harness(Path::new("."), &env).await {
            Ok(_) => panic!("Kiro must reject ACP authentication methods"),
            Err(error) => error,
        };
        assert!(matches!(auth_error, ExecutorError::Configuration(_)));
        assert!(auth_error.to_string().contains("does not advertise"));

        executor.acp = Some(AcpExecutionOptions {
            additional_directories: Some(vec!["/tmp/unsupported".to_string()]),
            ..AcpExecutionOptions::default()
        });
        let directory_error = match executor.acp_harness(Path::new("."), &env).await {
            Ok(_) => panic!("Kiro must reject additional directories"),
            Err(error) => error,
        };
        assert!(matches!(directory_error, ExecutorError::Configuration(_)));
        assert!(
            directory_error
                .to_string()
                .contains("additional directories")
        );
    }

    #[tokio::test]
    async fn probe_rejects_explicit_acp_auth_without_resolving_a_process() {
        let executor = kiro();
        let env = ExecutionEnv::new(Default::default(), false, String::new());

        let error = executor
            .probe_acp(Path::new("."), &env, Some("unsupported"))
            .await
            .expect_err("explicit ACP auth must be rejected");

        assert!(matches!(error, ExecutorError::Configuration(_)));
        assert!(error.to_string().contains("does not advertise"));
    }

    #[tokio::test]
    async fn mcp_preparation_uses_only_the_private_run_snapshot() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut executor = kiro();
        let fake_secret = "kiro-mcp-unit-secret-never-log";
        let canonical = MemberMcpConfig {
            mcp_servers: [(
                "member-only".to_string(),
                serde_json::json!({
                    "command": "/bin/echo",
                    "env": {"TOKEN": fake_secret}
                }),
            )]
            .into_iter()
            .collect(),
        };
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());

        let prepared = executor
            .prepare_mcp_for_run(&canonical, &run_context(workspace.path()), &mut env)
            .await
            .expect("Kiro MCP preparation");
        let effective = load_prepared_acp_mcp_config(&env)
            .await
            .expect("prepared Kiro member MCP");

        assert_eq!(effective.server_names(), ["member-only".to_string()].into());
        assert!(!format!("{effective:?}").contains(fake_secret));
        for key in [PREPARED_ACP_MCP_SNAPSHOT_ENV, PREPARED_ACP_MCP_HASH_ENV] {
            assert_eq!(
                executor.cmd.env.as_ref().and_then(|values| values.get(key)),
                env.get(key),
                "prepared Kiro MCP metadata must be pinned in both environment layers"
            );
        }
        let snapshot_path = std::path::PathBuf::from(
            env.get(PREPARED_ACP_MCP_SNAPSHOT_ENV)
                .expect("prepared Kiro snapshot path"),
        );
        assert!(snapshot_path.is_file());
        assert!(!env.contains_key("KIRO_HOME"));
        assert!(
            executor
                .cmd
                .env
                .as_ref()
                .is_none_or(|values| !values.contains_key("KIRO_HOME"))
        );
        assert_eq!(executor.default_mcp_config_path(), None);
        assert!(executor.supports_mcp());
        drop(prepared.into_cleanup());
        assert!(!snapshot_path.exists());
    }

    #[test]
    fn wire_value_profile_and_capabilities_are_minimal() {
        assert_eq!(
            serde_json::to_string(&BaseCodingAgent::KiroCli).expect("serialize Kiro runner"),
            "\"KIRO_CLI\""
        );
        assert_eq!(
            CodingAgent::KiroCli(kiro()).capabilities(),
            Vec::<BaseAgentCapability>::new()
        );

        let profiles = ExecutorConfigs::from_defaults();
        let configured = profiles
            .get_coding_agent(&ExecutorProfileId::new(BaseCodingAgent::KiroCli))
            .expect("default Kiro profile");
        let CodingAgent::KiroCli(configured) = configured else {
            panic!("default Kiro profile must contain KIRO_CLI");
        };
        assert!(configured.model.is_none());
        assert!(configured.acp.is_none());
        assert!(configured.cmd.additional_params.is_none());
    }
}
