use std::{collections::BTreeSet, path::Path, sync::Arc};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use super::acp::{
    AcpAccessMode, AcpAgentHarness, AcpApprovalMode, AcpApprovalPolicy, AcpAuthSelection,
    AcpCapabilityProbe, AcpClientServicePolicy, AcpExecutionOptions,
    mcp::{AcpMcpPolicy, load_prepared_acp_mcp_config, prepare_acp_mcp_for_run},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides, command_is_available,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, ExecutorPrompt, SlashCommandDescription,
        SpawnedChild, StandardCodingAgentExecutor,
        utils::{json_has_nonempty_string, read_json_file},
    },
    logs::utils::patch,
    mcp_config::MemberMcpConfig,
    mcp_run::{McpRunContext, PreparedMcpRun},
    model_discovery::{ModelCollector, read_config_value},
    skill_config::NativeSkillConfigBackend,
};

const QODER_AUTH_ENV_VARS: &[&str] = &["QODER_PERSONAL_ACCESS_TOKEN"];
const CONFLICTING_FLAGS: &[&str] = &[
    "--acp",
    "--yolo",
    "--dangerously-skip-permissions",
    "--permission-mode",
    "--add-dir",
    "--allowed-mcp-server-names",
    "--mcp-config",
    "--strict-mcp-config",
    "--cwd",
    "-w",
];

fn qoder_login_state_detected(qoder_home: &Path) -> bool {
    std::fs::metadata(qoder_home.join(".auth").join("user"))
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

fn qoder_config_dir(env: &ExecutionEnv) -> Option<std::path::PathBuf> {
    env.get("QODER_CONFIG_DIR")
        .filter(|value| !value.trim().is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("QODER_CONFIG_DIR").map(std::path::PathBuf::from))
        .or_else(|| dirs::home_dir().map(|home| home.join(".qoder")))
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct QoderCli {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Qoder model tier or custom model identifier")]
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

impl QoderCli {
    const BASE_COMMAND: &'static str = "qodercli";

    fn effective_approval_mode(&self) -> AcpApprovalMode {
        self.acp
            .as_ref()
            .and_then(|options| options.approval_mode)
            .unwrap_or_default()
    }

    fn validate_command_overrides(&self) -> Result<(), CommandBuildError> {
        let mut values = Vec::new();
        if let Some(base) = &self.cmd.base_command_override {
            values.push(base.as_str());
        }
        values.extend(
            self.cmd
                .additional_params
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(String::as_str),
        );
        for value in values {
            let normalized = value.replace(['=', '\t', '\n'], " ");
            if let Some(flag) = CONFLICTING_FLAGS.iter().find(|flag| {
                normalized
                    .split_ascii_whitespace()
                    .any(|token| token.trim_matches(['\'', '"']) == **flag)
            }) {
                return Err(CommandBuildError::InvalidShellParams(format!(
                    "Qoder {flag} is controlled by structured ACP settings"
                )));
            }
        }
        Ok(())
    }

    fn build_command_builder(
        &self,
        allowed_mcp_server_names: &BTreeSet<String>,
    ) -> Result<CommandBuilder, CommandBuildError> {
        self.validate_command_overrides()?;
        let mut builder = CommandBuilder::new(Self::BASE_COMMAND).extend_params([
            "--acp",
            "--permission-mode",
            "default",
            "--strict-mcp-config",
        ]);
        // Strict mode prevents Qoder from merging user/project MCP files, while
        // the process allowlist limits the ACP-provided servers. Always pass
        // both controls, including an empty allowlist, so ambient configuration
        // cannot bypass OpenTeams policy.
        builder = builder.extend_params([
            "--allowed-mcp-server-names".to_string(),
            allowed_mcp_server_names
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
        ]);
        apply_overrides(builder, &self.cmd)
    }

    async fn acp_harness(
        &self,
        env: &ExecutionEnv,
    ) -> Result<(AcpAgentHarness, BTreeSet<String>), ExecutorError> {
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
        if let Some(method_id) = Self::acp_auth_method_id(&options) {
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

        let effective = load_prepared_acp_mcp_config(env).await?;
        let allowed_names = effective.server_names();
        tracing::debug!(
            server_count = effective.servers.len(),
            config_hash = %effective.config_hash,
            "resolved effective Qoder ACP MCP configuration"
        );
        Ok((harness.with_mcp_servers(effective.servers), allowed_names))
    }

    fn configured_auth_detected(&self, env: &ExecutionEnv) -> bool {
        let Some(qoder_home) = qoder_config_dir(env) else {
            return false;
        };
        if qoder_login_state_detected(&qoder_home) {
            return true;
        }
        ["credentials.json", "oauth_creds.json", "auth.json"]
            .iter()
            .filter_map(|name| read_json_file(&qoder_home.join(name)))
            .any(|value| {
                json_has_nonempty_string(
                    &value,
                    &[
                        "/accessToken",
                        "/access_token",
                        "/refreshToken",
                        "/refresh_token",
                        "/token",
                    ],
                )
            })
    }

    fn acp_auth_method_id(options: &AcpExecutionOptions) -> Option<&str> {
        match &options.auth {
            Some(AcpAuthSelection::MethodId { method_id }) => Some(method_id),
            Some(AcpAuthSelection::Auto) | None => None,
        }
    }

    fn probe_auth_method_id(&self, requested_method_id: Option<&str>) -> Option<String> {
        requested_method_id.map(str::to_string).or_else(|| {
            self.acp
                .as_ref()
                .and_then(Self::acp_auth_method_id)
                .map(str::to_string)
        })
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for QoderCli {
    async fn prepare_mcp_for_run(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        prepare_acp_mcp_for_run(canonical, context, env, &mut self.cmd, "qoder-acp-mcp")
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

    async fn available_slash_commands(
        &self,
        _workdir: &Path,
    ) -> Result<futures::stream::BoxStream<'static, json_patch::Patch>, ExecutorError> {
        let commands = [
            ("init", "Understand the project and generate AGENTS.md"),
            ("memory", "Show or refresh project memory"),
            ("about", "Show Qoder CLI version information"),
            ("help", "Show available Qoder ACP commands"),
        ]
        .into_iter()
        .map(|(name, description)| SlashCommandDescription {
            name: name.to_string(),
            description: Some(description.to_string()),
        })
        .collect();
        Ok(Box::pin(futures::stream::once(async move {
            patch::slash_commands(commands, false, None)
        })))
    }

    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        self.authentication_detected(
            &env,
            QODER_AUTH_ENV_VARS,
            self.configured_auth_detected(&env),
        )
    }

    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn list_models(
        &self,
        _current_dir: &Path,
        _env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        let mut models = ["lite", "efficient", "auto", "performance", "ultimate"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            models.insert(model.to_string());
        }
        let mut discovered = ModelCollector::new();
        if let Some(path) = self.default_mcp_config_path()
            && let Some(settings) = read_config_value(&path).await?
        {
            discovered.add_value_models(&settings);
        }
        if let Some(discovered) = discovered.finish() {
            models.extend(discovered);
        }
        Ok(Some(models.into_iter().collect()))
    }

    async fn probe_acp(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
        auth_method_id: Option<&str>,
    ) -> Result<Option<AcpCapabilityProbe>, ExecutorError> {
        let auth_method_id = self.probe_auth_method_id(auth_method_id);
        let allowed = BTreeSet::new();
        Ok(Some(
            super::acp::runtime::probe_acp_command(
                self.build_command_builder(&allowed)?.build_initial()?,
                current_dir,
                env,
                &self.cmd,
                auth_method_id,
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
        let (harness, allowed) = self.acp_harness(env).await?;
        let command = self.build_command_builder(&allowed)?.build_initial()?;
        harness
            .spawn_with_command(
                current_dir,
                self.append_prompt.combine_prompt(prompt),
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
        let (harness, allowed) = self.acp_harness(env).await?;
        let command = self.build_command_builder(&allowed)?.build_initial()?;
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

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let (harness, allowed) = self.acp_harness(env).await?;
        let command = self.build_command_builder(&allowed)?.build_follow_up(&[])?;
        harness
            .spawn_follow_up_with_command(
                current_dir,
                self.append_prompt.combine_prompt(prompt),
                session_id,
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
        let (harness, allowed) = self.acp_harness(env).await?;
        let command = self.build_command_builder(&allowed)?.build_follow_up(&[])?;
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
        crate::executors::acp::normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        let home = dirs::home_dir()?;
        Some(
            std::env::var_os("QODER_CONFIG_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| home.join(".qoder"))
                .join("settings.json"),
        )
    }

    fn native_skill_discovery_roots(&self) -> Vec<std::path::PathBuf> {
        let Some(home) = dirs::home_dir() else {
            return Vec::new();
        };
        let qoder_home = std::env::var_os("QODER_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| home.join(".qoder"));
        vec![qoder_home.join("skills")]
    }

    fn default_skill_config_path(&self) -> Option<std::path::PathBuf> {
        self.default_mcp_config_path()
    }

    fn native_skill_config_backend(&self) -> NativeSkillConfigBackend {
        NativeSkillConfigBackend::Qoder
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

    fn run_context(workspace: &Path) -> McpRunContext {
        McpRunContext::new(workspace, uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
            .expect("run context")
    }

    fn qoder() -> QoderCli {
        QoderCli {
            append_prompt: AppendPrompt::default(),
            model: Some("auto".to_string()),
            acp: None,
            cmd: CmdOverrides::default(),
            acp_mcp_policy: AcpMcpPolicy::default(),
            approvals: None,
        }
    }

    #[test]
    fn command_uses_safe_acp_permission_mode_and_empty_mcp_allowlist() {
        let (program, args) = qoder()
            .build_command_builder(&BTreeSet::new())
            .expect("build Qoder command")
            .build_initial()
            .expect("build initial command")
            .into_parts_for_test();
        assert_eq!(program, "qodercli");
        assert_eq!(
            args,
            [
                "--acp",
                "--permission-mode",
                "default",
                "--strict-mcp-config",
                "--allowed-mcp-server-names",
                ""
            ]
        );
    }

    #[test]
    fn conflicting_command_arguments_are_rejected() {
        for value in [
            "--yolo",
            "--dangerously-skip-permissions",
            "--permission-mode=auto",
            "--add-dir ../secret",
            "--allowed-mcp-server-names rogue",
        ] {
            let mut executor = qoder();
            executor.cmd.additional_params = Some(vec![value.to_string()]);
            assert!(
                executor.build_command_builder(&BTreeSet::new()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn enabled_mcp_names_are_pinned_on_the_process() {
        let allowed = BTreeSet::from(["context7".to_string(), "github".to_string()]);
        let (_, args) = qoder()
            .build_command_builder(&allowed)
            .expect("build Qoder command")
            .build_initial()
            .expect("build initial command")
            .into_parts_for_test();
        assert!(args.windows(2).any(|pair| {
            pair[0] == "--allowed-mcp-server-names" && pair[1] == "context7,github"
        }));
    }

    #[tokio::test]
    async fn public_preparation_pins_exact_member_allowlist_and_ignores_legacy_policy() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut executor = qoder();
        executor.acp_mcp_policy = AcpMcpPolicy {
            allowed_server_names: Some(Default::default()),
            disabled_server_names: Default::default(),
        };
        let canonical = MemberMcpConfig {
            mcp_servers: [
                (
                    "alpha".to_string(),
                    serde_json::json!({"command": "/bin/echo"}),
                ),
                (
                    "beta".to_string(),
                    serde_json::json!({"command": "/bin/echo"}),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());

        let prepared = executor
            .prepare_mcp_for_run(&canonical, &run_context(workspace.path()), &mut env)
            .await
            .expect("Qoder MCP preparation");
        let (_, allowed) = executor.acp_harness(&env).await.expect("Qoder ACP harness");
        let (_, args) = executor
            .build_command_builder(&allowed)
            .expect("Qoder command")
            .build_initial()
            .expect("initial command")
            .into_parts_for_test();

        assert_eq!(
            allowed,
            BTreeSet::from(["alpha".to_string(), "beta".to_string()])
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair[0] == "--allowed-mcp-server-names" && pair[1] == "alpha,beta" })
        );
        assert!(args.iter().any(|arg| arg == "--strict-mcp-config"));
        drop(prepared.into_cleanup());
    }

    #[tokio::test]
    async fn public_empty_member_map_keeps_strict_empty_override_and_ignores_ambient_mcp() {
        let workspace = tempfile::tempdir().expect("workspace");
        let qoder_home = workspace.path().join("qoder-home");
        tokio::fs::create_dir_all(qoder_home.join(".auth"))
            .await
            .expect("Qoder config directory");
        let ambient_path = qoder_home.join("settings.json");
        let vendor_files: Vec<(std::path::PathBuf, &[u8])> = vec![
            (
                ambient_path.clone(),
                br#"{"mcpServers":{"ambient-global":{"command":"must-not-run"}}}"#,
            ),
            (qoder_home.join(".auth/user"), b"fixture-login-state\n"),
            (
                qoder_home.join("credentials.json"),
                br#"{"accessToken":"qoder-fixture-access-token"}"#,
            ),
            (
                qoder_home.join("oauth_creds.json"),
                br#"{"refresh_token":"qoder-fixture-refresh-token"}"#,
            ),
            (
                qoder_home.join("auth.json"),
                br#"{"token":"qoder-fixture-auth-token"}"#,
            ),
        ];
        let mut original_vendor_files = Vec::new();
        for (path, contents) in vendor_files {
            tokio::fs::write(&path, contents)
                .await
                .unwrap_or_else(|_| panic!("write Qoder vendor file {}", path.display()));
            original_vendor_files.push((
                path.clone(),
                tokio::fs::read(&path)
                    .await
                    .unwrap_or_else(|_| panic!("read Qoder vendor file {}", path.display())),
            ));
        }
        let mut executor = qoder();
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert(
            "QODER_CONFIG_DIR",
            qoder_home.to_string_lossy().into_owned(),
        );
        assert!(executor.is_authenticated(&env));

        let prepared = executor
            .prepare_mcp_for_run(
                &MemberMcpConfig::default(),
                &run_context(workspace.path()),
                &mut env,
            )
            .await
            .expect("Qoder empty MCP preparation");
        let (_, allowed) = executor
            .acp_harness(&env)
            .await
            .expect("Qoder empty ACP harness");
        let effective = load_prepared_acp_mcp_config(&env)
            .await
            .expect("prepared empty Qoder MCP");
        let (_, args) = executor
            .build_command_builder(&allowed)
            .expect("Qoder command")
            .build_initial()
            .expect("initial command")
            .into_parts_for_test();

        assert!(allowed.is_empty());
        assert!(effective.server_names().is_empty());
        assert!(ambient_path.is_file());
        assert!(args.iter().any(|arg| arg == "--strict-mcp-config"));
        assert!(
            args.windows(2)
                .any(|pair| { pair[0] == "--allowed-mcp-server-names" && pair[1].is_empty() })
        );
        for (path, original) in &original_vendor_files {
            let current = tokio::fs::read(path)
                .await
                .unwrap_or_else(|_| panic!("read Qoder vendor file {}", path.display()));
            assert_eq!(
                current.as_slice(),
                original.as_slice(),
                "Qoder preparation changed user file {}",
                path.display()
            );
        }
        drop(prepared.into_cleanup());
        for (path, original) in &original_vendor_files {
            let current = tokio::fs::read(path)
                .await
                .unwrap_or_else(|_| panic!("read Qoder vendor file {}", path.display()));
            assert_eq!(
                current.as_slice(),
                original.as_slice(),
                "Qoder cleanup changed user file {}",
                path.display()
            );
        }
    }

    #[test]
    fn personal_access_token_marks_qoder_authenticated() {
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert(
            "QODER_PERSONAL_ACCESS_TOKEN",
            "test-token-never-logged".to_string(),
        );
        assert!(qoder().is_authenticated(&env));
    }

    #[test]
    fn qoder_acp_auto_and_missing_auth_do_not_request_authentication() {
        assert_eq!(
            QoderCli::acp_auth_method_id(&AcpExecutionOptions::default()),
            None
        );
        let options = AcpExecutionOptions {
            auth: Some(AcpAuthSelection::Auto),
            ..AcpExecutionOptions::default()
        };
        assert_eq!(QoderCli::acp_auth_method_id(&options), None);
    }

    #[test]
    fn qoder_acp_preserves_explicit_auth_method() {
        let options = AcpExecutionOptions {
            auth: Some(AcpAuthSelection::MethodId {
                method_id: "enterprise-login".to_string(),
            }),
            ..AcpExecutionOptions::default()
        };
        assert_eq!(
            QoderCli::acp_auth_method_id(&options),
            Some("enterprise-login")
        );
        let mut executor = qoder();
        executor.acp = Some(options);
        assert_eq!(
            executor.probe_auth_method_id(None).as_deref(),
            Some("enterprise-login")
        );
        assert_eq!(
            executor
                .probe_auth_method_id(Some("diagnostics-login"))
                .as_deref(),
            Some("diagnostics-login")
        );
    }

    #[test]
    fn consecutive_auto_probes_do_not_request_authentication() {
        let executor = qoder();

        for _ in 0..3 {
            assert_eq!(executor.probe_auth_method_id(None), None);
        }
    }

    #[test]
    fn qoder_login_state_file_marks_configured_auth() {
        let temp = tempfile::tempdir().expect("temporary Qoder home");
        let auth_dir = temp.path().join(".auth");
        std::fs::create_dir(&auth_dir).expect("create auth directory");
        assert!(!qoder_login_state_detected(temp.path()));

        std::fs::write(auth_dir.join("user"), "encrypted-login-state").expect("write login state");
        assert!(qoder_login_state_detected(temp.path()));

        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert(
            "QODER_CONFIG_DIR",
            temp.path().to_string_lossy().into_owned(),
        );
        assert!(qoder().is_authenticated(&env));
    }

    #[tokio::test]
    async fn model_discovery_contains_all_documented_tiers() {
        let env = ExecutionEnv::new(Default::default(), false, String::new());
        let models = qoder()
            .list_models(Path::new("."), &env)
            .await
            .expect("list models")
            .expect("Qoder models");
        for tier in ["lite", "efficient", "auto", "performance", "ultimate"] {
            assert!(models.iter().any(|model| model == tier), "{tier}");
        }
    }

    /// Real ACP probe against the installed `qodercli`.
    /// Gated behind `QODER_E2E_PROBE=1` so it never runs in CI.
    #[tokio::test]
    async fn real_qoder_cli_acp_probe_does_not_request_authentication() {
        if std::env::var_os("QODER_E2E_PROBE").is_none() {
            eprintln!("skipping real Qoder CLI probe (set QODER_E2E_PROBE=1 to enable)");
            return;
        }
        let executor = qoder();
        let env = ExecutionEnv::new(Default::default(), false, String::new());

        assert!(
            executor.is_authenticated(&env),
            "Qoder must be authenticated via local login state"
        );

        for i in 1..=3 {
            let probe = executor
                .probe_acp(Path::new("."), &env, None)
                .await
                .unwrap_or_else(|error| panic!("ACP probe iteration {i} should succeed: {error}"))
                .expect("probe should return a result");

            assert_eq!(
                probe.protocol_version, "1",
                "iteration {i}: protocol version must be v1"
            );
            assert!(
                probe.agent_name.is_some(),
                "iteration {i}: agent name should be present"
            );
            assert!(
                probe.agent_version.is_some(),
                "iteration {i}: agent version should be present"
            );

            let model_ids = probe.model_ids().unwrap_or_else(Vec::new);
            for tier in ["lite", "efficient", "auto", "performance", "ultimate"] {
                assert!(
                    model_ids.iter().any(|m| m == tier),
                    "iteration {i}: model tier `{tier}` should be advertised"
                );
            }

            eprintln!(
                "probe {i}: agent={:?} version={:?} models={:?} auth_methods={}",
                probe.agent_name,
                probe.agent_version,
                model_ids,
                probe.auth_methods.len()
            );
        }
    }

    /// Real ACP session new + close against the installed `qodercli`.
    /// Gated behind `QODER_E2E_PROBE=1`.
    #[tokio::test]
    async fn real_qoder_cli_acp_session_new_with_empty_mcp_allowlist() {
        if std::env::var_os("QODER_E2E_PROBE").is_none() {
            eprintln!("skipping real Qoder CLI session test (set QODER_E2E_PROBE=1 to enable)");
            return;
        }
        let executor = qoder();
        let env = ExecutionEnv::new(Default::default(), false, String::new());
        let allowed = BTreeSet::new();

        let builder = executor
            .build_command_builder(&allowed)
            .expect("build command with empty MCP allowlist");
        let command = builder.build_initial().expect("build initial command");
        let (program, args) = command.clone().into_parts_for_test();

        assert_eq!(program, "qodercli");
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--allowed-mcp-server-names" && pair[1].is_empty()),
            "empty MCP allowlist must be pinned on the process"
        );

        let probe = executor
            .probe_acp(Path::new("."), &env, None)
            .await
            .expect("ACP probe should succeed with empty allowlist")
            .expect("probe should return a result");

        assert!(
            probe.supports_session_resume || probe.supports_session_load,
            "agent must support resume or load for follow-up sessions"
        );
    }
}
