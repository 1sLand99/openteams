use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use super::acp::{
    AcpAgentHarness, AcpApprovalPolicy,
    mcp::{load_mcp_servers, write_mcp_isolation_settings},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
    },
    model_discovery::{
        ProviderKind, cli_model_commands, discover_from_sources, runner_config_paths,
    },
    skill_config::NativeSkillConfigBackend,
};

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
    pub yolo: Option<bool>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl Gemini {
    const BASE_COMMAND: &'static str = "npx -y @google/gemini-cli@0.45.0";

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        let mut builder = CommandBuilder::new(Self::BASE_COMMAND);

        builder = builder.extend_params(["--acp"]);

        apply_overrides(builder, &self.cmd)
    }

    async fn acp_harness(&self) -> Result<AcpAgentHarness, ExecutorError> {
        let mut harness =
            AcpAgentHarness::new().with_approval_policy(if self.yolo.unwrap_or(false) {
                AcpApprovalPolicy::AutoAllow
            } else {
                AcpApprovalPolicy::Ask
            });
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
        let servers = load_mcp_servers(config_path.as_deref()).await?;
        Ok(harness.with_mcp_servers(servers))
    }

    async fn acp_runtime_env(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<ExecutionEnv, ExecutorError> {
        let path = write_mcp_isolation_settings(current_dir, "gemini-acp-settings").await?;
        let mut runtime_env = env.clone();
        runtime_env.insert(
            "GEMINI_CLI_SYSTEM_SETTINGS_PATH",
            path.to_string_lossy().to_string(),
        );
        Ok(runtime_env)
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for Gemini {
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
            dirs::home_dir().map(|home| home.join(".gemini").join("settings.jsonc")),
        ]);
        discover_from_sources(
            current_dir,
            env,
            &self.cmd,
            self.model.as_deref(),
            config_paths,
            cli_model_commands(Self::BASE_COMMAND, &self.cmd),
            &[ProviderKind::Google],
        )
        .await
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let harness = self.acp_harness().await?;
        let runtime_env = self.acp_runtime_env(current_dir, env).await?;
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
        let harness = self.acp_harness().await?;
        let runtime_env = self.acp_runtime_env(current_dir, env).await?;
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
        if let Some(timestamp) = dirs::home_dir()
            .and_then(|home| std::fs::metadata(home.join(".gemini").join("oauth_creds.json")).ok())
            .and_then(|m| m.modified().ok())
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
        {
            return AvailabilityInfo::LoginDetected {
                last_auth_timestamp: timestamp,
            };
        }

        let mcp_config_found = self
            .default_mcp_config_path()
            .map(|p| p.exists())
            .unwrap_or(false);

        let installation_indicator_found = dirs::home_dir()
            .map(|home| home.join(".gemini").join("installation_id").exists())
            .unwrap_or(false);

        if mcp_config_found || installation_indicator_found {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_builder_uses_current_acp_flag() {
        let gemini = Gemini {
            append_prompt: AppendPrompt::default(),
            model: Some("gemini-3-pro-preview".to_string()),
            thinking_effort: None,
            yolo: Some(true),
            cmd: CmdOverrides::default(),
            approvals: None,
        };

        let (_program, args) = gemini
            .build_command_builder()
            .expect("build command")
            .build_initial()
            .expect("build initial")
            .into_parts_for_test();

        assert!(args.iter().any(|arg| arg == "--acp"));
        assert!(!args.iter().any(|arg| arg == "--experimental-acp"));
    }
}
