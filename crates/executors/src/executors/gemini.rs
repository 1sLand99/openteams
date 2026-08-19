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
    skill_config::NativeSkillConfigBackend,
};

const GEMINI_AUTH_ENV_VARS: &[&str] = &[
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
];

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Gemini {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Model to use (e.g., gemini-2.5-pro, gemini-2.5-flash, gemini-2.5-flash-lite, gemini-3-pro-preview)"
    )]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Per-run Gemini thinking effort: off, low, medium, high, max, or a numeric thinking budget"
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

impl Gemini {
    const BASE_COMMAND: &'static str = "gemini";
    const TRUST_WORKSPACE_ENV: &'static str = "GEMINI_CLI_TRUST_WORKSPACE";
    const SYSTEM_SETTINGS_ENV: &'static str = "GEMINI_CLI_SYSTEM_SETTINGS_PATH";

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = CommandBuilder::new(Self::BASE_COMMAND);

        builder = builder.extend_params(["--acp"]);

        apply_overrides(builder, &self.cmd)
    }

    fn workspace_trusted_env(env: &ExecutionEnv) -> ExecutionEnv {
        let mut runtime_env = env.clone();
        if !runtime_env.contains_key(Self::TRUST_WORKSPACE_ENV) {
            runtime_env.insert(Self::TRUST_WORKSPACE_ENV, "true");
        }
        runtime_env
    }

    fn acp_client_services(full_access: bool) -> AcpClientServicePolicy {
        // Gemini runs beside OpenTeams and has its own filesystem service. Do
        // not advertise ACP FS callbacks: Gemini relies on native ENOENT when
        // deciding whether `write_file` is creating a new file.
        AcpClientServicePolicy {
            terminal: true,
            full_access,
            ..AcpClientServicePolicy::default()
        }
    }

    fn native_thinking_settings(&self) -> Result<Value, ExecutorError> {
        let Some(effort) = self
            .thinking_effort
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(json!({}));
        };
        let model = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("auto");
        let thinking_config = gemini_thinking_config(model, effort)?;
        Ok(json!({
            "modelConfigs": {
                "customOverrides": [{
                    "match": { "model": model },
                    "modelConfig": {
                        "generateContentConfig": {
                            "thinkingConfig": thinking_config
                        }
                    }
                }]
            }
        }))
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
            .with_additional_directories(additional_directories)
            .with_client_services(Self::acp_client_services(full_access));
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
}

fn gemini_thinking_config(model: &str, effort: &str) -> Result<Value, ExecutorError> {
    let normalized = effort.trim().to_ascii_lowercase();
    let is_gemini_2_5 = model.to_ascii_lowercase().contains("gemini-2.5");

    if let Ok(budget) = normalized.parse::<u32>() {
        if !is_gemini_2_5 {
            return Err(invalid_thinking_effort(format!(
                "numeric Gemini thinking budgets require a Gemini 2.5 model, got `{model}`"
            )));
        }
        return Ok(json!({ "thinkingBudget": budget }));
    }

    if is_gemini_2_5 {
        let budget = match normalized.as_str() {
            "off" | "none" => 0,
            "low" => 1024,
            "medium" => 4096,
            "high" => 8192,
            "xhigh" | "max" => 16384,
            _ => {
                return Err(invalid_thinking_effort(format!(
                    "unsupported Gemini thinking effort `{effort}`"
                )));
            }
        };
        return Ok(json!({ "thinkingBudget": budget }));
    }

    let level = match normalized.as_str() {
        "off" | "none" | "low" => "LOW",
        "medium" => "MEDIUM",
        "high" | "xhigh" | "max" => "HIGH",
        _ => {
            return Err(invalid_thinking_effort(format!(
                "unsupported Gemini thinking effort `{effort}`"
            )));
        }
    };
    Ok(json!({ "thinkingLevel": level }))
}

fn invalid_thinking_effort(message: impl Into<String>) -> ExecutorError {
    ExecutorError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message.into(),
    ))
}

#[async_trait]
impl StandardCodingAgentExecutor for Gemini {
    async fn prepare_mcp_for_run(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        let prepared =
            prepare_acp_mcp_for_run(canonical, context, env, &mut self.cmd, "gemini-acp-mcp")?;
        // Gemini CLI 0.52 creates a non-resumable transcript while loading an
        // ACP session. Its next-process retention cleanup groups files by the
        // session short ID and can delete the resumable transcript with that
        // placeholder. Disable vendor cleanup for OpenTeams-managed ACP runs.
        let mut settings = self.native_thinking_settings()?;
        settings
            .as_object_mut()
            .expect("native Gemini settings are always an object")
            .insert(
                "general".to_string(),
                json!({
                    "sessionRetention": {
                        "enabled": false
                    }
                }),
            );
        let (path, cleanup) =
            write_mcp_isolation_settings(context, "gemini-acp-settings", settings)?;
        pin_mcp_run_environment(
            env,
            &mut self.cmd,
            Self::SYSTEM_SETTINGS_ENV,
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
            return self.authentication_detected(&env, GEMINI_AUTH_ENV_VARS, false);
        };
        let gemini_home = home.join(".gemini");
        let oauth_login =
            read_json_file(&gemini_home.join("oauth_creds.json")).is_some_and(|value| {
                json_has_nonempty_string(&value, &["/access_token", "/refresh_token"])
            });
        let settings_key =
            read_json_file(&gemini_home.join("settings.json")).is_some_and(|value| {
                json_has_nonempty_string(
                    &value,
                    &["/security/auth/apiKey", "/security/auth/token", "/apiKey"],
                )
            });
        let dotenv_key = dotenv_has_nonempty_value(&gemini_home.join(".env"), GEMINI_AUTH_ENV_VARS);
        self.authentication_detected(
            &env,
            GEMINI_AUTH_ENV_VARS,
            oauth_login || settings_key || dotenv_key,
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
        let runtime_env = Self::workspace_trusted_env(env);
        let config_paths = runner_config_paths([
            self.default_mcp_config_path(),
            dirs::home_dir().map(|home| home.join(".gemini").join("settings.jsonc")),
        ]);
        discover_from_sources(
            current_dir,
            &runtime_env,
            &self.cmd,
            self.model.as_deref(),
            config_paths,
            cli_model_commands(Self::BASE_COMMAND, &self.cmd),
            &[ProviderKind::Google],
        )
        .await
    }

    async fn probe_acp(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
        auth_method_id: Option<&str>,
    ) -> Result<Option<AcpCapabilityProbe>, ExecutorError> {
        let runtime_env = Self::workspace_trusted_env(env);
        let configured_auth_method_id = self.acp.as_ref().and_then(|options| match &options.auth {
            Some(AcpAuthSelection::MethodId { method_id }) => Some(method_id.clone()),
            _ => None,
        });
        Ok(Some(
            super::acp::runtime::probe_acp_command(
                self.build_command_builder()?.build_initial()?,
                current_dir,
                &runtime_env,
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
        let runtime_env = Self::workspace_trusted_env(env);
        let harness = self.acp_harness(&runtime_env).await?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let gemini_command = self.build_command_builder()?.build_initial()?;
        harness
            .spawn_with_command(
                current_dir,
                combined_prompt,
                gemini_command,
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
        let runtime_env = Self::workspace_trusted_env(env);
        let harness = self.acp_harness(&runtime_env).await?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let gemini_command = self.build_command_builder()?.build_follow_up(&[])?;
        harness
            .spawn_follow_up_with_command(
                current_dir,
                combined_prompt,
                session_id,
                gemini_command,
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
        let runtime_env = Self::workspace_trusted_env(env);
        let harness = self.acp_harness(&runtime_env).await?;
        let command = self.build_command_builder()?.build_initial()?;
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
        let runtime_env = Self::workspace_trusted_env(env);
        let harness = self.acp_harness(&runtime_env).await?;
        let command = self.build_command_builder()?.build_follow_up(&[])?;
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
        super::acp::normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".gemini").join("settings.json"))
    }

    fn default_skill_config_path(&self) -> Option<std::path::PathBuf> {
        self.default_mcp_config_path()
    }

    fn native_skill_discovery_roots(&self) -> Vec<std::path::PathBuf> {
        dirs::home_dir()
            .map(|home| vec![home.join(".gemini").join("skills")])
            .unwrap_or_default()
    }

    fn native_skill_config_backend(&self) -> NativeSkillConfigBackend {
        NativeSkillConfigBackend::Gemini
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

    #[test]
    fn workspace_is_trusted_by_default() {
        let env = ExecutionEnv::new(Default::default(), false, String::new());

        let runtime_env = Gemini::workspace_trusted_env(&env);

        assert_eq!(
            runtime_env.get(Gemini::TRUST_WORKSPACE_ENV),
            Some(&"true".to_string())
        );
    }

    #[test]
    fn explicit_workspace_trust_override_is_preserved() {
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert(Gemini::TRUST_WORKSPACE_ENV, "false");

        let runtime_env = Gemini::workspace_trusted_env(&env);

        assert_eq!(
            runtime_env.get(Gemini::TRUST_WORKSPACE_ENV),
            Some(&"false".to_string())
        );
    }

    #[test]
    fn acp_uses_gemini_local_filesystem() {
        let services = Gemini::acp_client_services(false);

        assert!(!services.read_text_file);
        assert!(!services.write_text_file);
        assert!(services.terminal);
        assert!(!services.full_access);
    }

    #[tokio::test]
    async fn acp_disables_gemini_session_retention_cleanup() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut gemini = Gemini {
            append_prompt: AppendPrompt::default(),
            model: None,
            thinking_effort: None,
            acp: None,
            cmd: CmdOverrides::default(),
            acp_mcp_policy: AcpMcpPolicy::default(),
            approvals: None,
        };
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());

        let prepared = gemini
            .prepare_mcp_for_run(
                &MemberMcpConfig::default(),
                &run_context(workspace.path()),
                &mut env,
            )
            .await
            .expect("Gemini MCP preparation");
        let settings_path = std::path::PathBuf::from(
            env.get(Gemini::SYSTEM_SETTINGS_ENV)
                .expect("Gemini system settings path"),
        );
        let settings: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(&settings_path)
                .await
                .expect("read Gemini system settings"),
        )
        .expect("parse Gemini system settings");

        assert_eq!(
            settings["general"]["sessionRetention"]["enabled"],
            serde_json::json!(false)
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

    #[test]
    fn native_thinking_settings_map_gemini_model_families() {
        assert_eq!(
            gemini_thinking_config("gemini-3.1-pro-preview", "medium")
                .expect("Gemini 3 thinking config"),
            json!({ "thinkingLevel": "MEDIUM" })
        );
        assert_eq!(
            gemini_thinking_config("gemini-2.5-flash", "high").expect("Gemini 2.5 thinking config"),
            json!({ "thinkingBudget": 8192 })
        );
        assert_eq!(
            gemini_thinking_config("gemini-2.5-pro", "12000")
                .expect("explicit Gemini 2.5 thinking budget"),
            json!({ "thinkingBudget": 12000 })
        );
    }

    #[tokio::test]
    async fn acp_writes_native_gemini_thinking_fallback() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut gemini = Gemini {
            append_prompt: AppendPrompt::default(),
            model: Some("gemini-3.1-pro-preview".to_string()),
            thinking_effort: Some("medium".to_string()),
            acp: None,
            cmd: CmdOverrides::default(),
            acp_mcp_policy: AcpMcpPolicy::default(),
            approvals: None,
        };
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());

        let prepared = gemini
            .prepare_mcp_for_run(
                &MemberMcpConfig::default(),
                &run_context(workspace.path()),
                &mut env,
            )
            .await
            .expect("Gemini MCP preparation");
        let settings_path = std::path::PathBuf::from(
            env.get(Gemini::SYSTEM_SETTINGS_ENV)
                .expect("Gemini system settings path"),
        );
        let settings: Value = serde_json::from_slice(
            &tokio::fs::read(&settings_path)
                .await
                .expect("read Gemini system settings"),
        )
        .expect("parse Gemini system settings");

        assert_eq!(
            settings["modelConfigs"]["customOverrides"][0]["match"]["model"],
            json!("gemini-3.1-pro-preview")
        );
        assert_eq!(
            settings["modelConfigs"]["customOverrides"][0]["modelConfig"]["generateContentConfig"]
                ["thinkingConfig"],
            json!({ "thinkingLevel": "MEDIUM" })
        );

        drop(prepared.into_cleanup());
        assert!(!settings_path.exists());
    }

    #[tokio::test]
    async fn public_preparation_uses_member_canonical_not_legacy_allowlist() {
        let workspace = tempfile::tempdir().expect("workspace");
        let mut gemini = Gemini {
            append_prompt: AppendPrompt::default(),
            model: None,
            thinking_effort: None,
            acp: None,
            cmd: CmdOverrides::default(),
            acp_mcp_policy: AcpMcpPolicy {
                allowed_server_names: Some(Default::default()),
                disabled_server_names: Default::default(),
            },
            approvals: None,
        };
        let canonical = MemberMcpConfig {
            mcp_servers: [("member-only".to_string(), json!({"command": "/bin/echo"}))]
                .into_iter()
                .collect(),
        };
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());

        let prepared = gemini
            .prepare_mcp_for_run(&canonical, &run_context(workspace.path()), &mut env)
            .await
            .expect("Gemini MCP preparation");
        let effective = load_prepared_acp_mcp_config(&env)
            .await
            .expect("prepared member config");

        assert_eq!(effective.server_names(), ["member-only".to_string()].into());
        drop(prepared.into_cleanup());
    }

    #[test]
    fn command_builder_uses_current_acp_flag() {
        let gemini = Gemini {
            append_prompt: AppendPrompt::default(),
            model: Some("gemini-3-pro-preview".to_string()),
            thinking_effort: None,
            acp: None,
            cmd: CmdOverrides::default(),
            acp_mcp_policy: AcpMcpPolicy::default(),
            approvals: None,
        };

        let (program, args) = gemini
            .build_command_builder()
            .expect("build command")
            .build_initial()
            .expect("build initial")
            .into_parts_for_test();

        assert_eq!(program, "gemini");
        assert!(args.iter().any(|arg| arg == "--acp"));
        assert!(!args.iter().any(|arg| arg == "--experimental-acp"));
    }
}
