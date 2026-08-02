use std::{path::Path, process::Stdio, sync::Arc};

use async_trait::async_trait;
use command_group::AsyncCommandGroup;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum_macros::AsRefStr;
use tokio::{io::AsyncWriteExt, process::Command};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use crate::{
    command::{CommandBuildError, CommandBuilder, CommandParts, command_is_available},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
        utils::{json_has_nonempty_string, read_json_file},
    },
    logs::utils::EntryIndexProvider,
    model_discovery::{
        ProviderKind, cli_model_commands, discover_from_sources, runner_config_paths,
    },
};

pub mod normalize_logs;
pub mod session;

use normalize_logs::normalize_logs;

use self::session::fork_session;

// Configuration types for Droid executor
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Autonomy {
    Normal,
    Low,
    Medium,
    High,
    SkipPermissionsUnsafe,
}

fn default_autonomy() -> Autonomy {
    Autonomy::SkipPermissionsUnsafe
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema, AsRefStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[ts(rename = "DroidReasoningEffort")]
pub enum ReasoningEffortLevel {
    None,
    Dynamic,
    Off,
    Low,
    Medium,
    High,
}

/// Droid executor configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
pub struct Droid {
    #[serde(default)]
    pub append_prompt: AppendPrompt,

    #[serde(default = "default_autonomy")]
    #[schemars(
        title = "Autonomy Level",
        description = "Permission level for file and system operations"
    )]
    pub autonomy: Autonomy,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "Model",
        description = "Model to use (e.g., gpt-5.2-codex, sonnet, gpt-5-2025-08-07, opus, claude-haiku-4-5-20251001, glm-4.6)"
    )]
    pub model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "Reasoning Effort",
        description = "Reasoning effort level: none, dynamic, off, low, medium, high"
    )]
    pub reasoning_effort: Option<ReasoningEffortLevel>,

    #[serde(flatten)]
    pub cmd: crate::command::CmdOverrides,
}

impl Droid {
    pub fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        use crate::command::{CommandBuilder, apply_overrides};
        let mut builder =
            CommandBuilder::new("droid exec").params(["--output-format", "stream-json"]);
        builder = match &self.autonomy {
            Autonomy::Normal => builder,
            Autonomy::Low => builder.extend_params(["--auto", "low"]),
            Autonomy::Medium => builder.extend_params(["--auto", "medium"]),
            Autonomy::High => builder.extend_params(["--auto", "high"]),
            Autonomy::SkipPermissionsUnsafe => builder.extend_params(["--skip-permissions-unsafe"]),
        };
        if let Some(model) = &self.model {
            builder = builder.extend_params(["--model", model.as_str()]);
        }
        if let Some(effort) = &self.reasoning_effort {
            builder = builder.extend_params(["--reasoning-effort", effort.as_ref()]);
        }

        apply_overrides(builder, &self.cmd)
    }
}

fn droid_auth_value_has_credentials(value: &serde_json::Value) -> bool {
    json_has_nonempty_string(
        value,
        &[
            "/apiKey",
            "/api_key",
            "/token",
            "/accessToken",
            "/refreshToken",
            "/auth/token",
            "/auth/accessToken",
            "/auth/refreshToken",
        ],
    )
}

async fn spawn_droid(
    command_parts: CommandParts,
    prompt: &String,
    current_dir: &Path,
    env: &ExecutionEnv,
    cmd_overrides: &crate::command::CmdOverrides,
) -> Result<SpawnedChild, ExecutorError> {
    let (program_path, args) = command_parts.into_resolved().await?;

    let mut command = Command::new(program_path);
    command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(current_dir)
        .env("NPM_CONFIG_LOGLEVEL", "error")
        .args(args);

    env.clone()
        .with_profile(cmd_overrides)
        .apply_to_command(&mut command);

    let mut child = command.group_spawn()?;

    if let Some(mut stdin) = child.inner().stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    Ok(child.into())
}

#[async_trait]
impl StandardCodingAgentExecutor for Droid {
    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        let cli_login = dirs::home_dir().is_some_and(|home| {
            let factory_home = home.join(".factory");
            [
                factory_home.join("auth.json"),
                factory_home.join("credentials.json"),
                factory_home.join("config.json"),
            ]
            .iter()
            .filter_map(|path| read_json_file(path))
            .any(|value| droid_auth_value_has_credentials(&value))
        });
        self.authentication_detected(&env, &["FACTORY_API_KEY"], cli_login)
    }

    async fn list_models(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        let config_paths = runner_config_paths([
            self.default_mcp_config_path(),
            dirs::home_dir().map(|home| home.join(".factory").join("config.json")),
            dirs::home_dir().map(|home| home.join(".factory").join("settings.json")),
        ]);
        discover_from_sources(
            current_dir,
            env,
            &self.cmd,
            self.model.as_deref(),
            config_paths,
            cli_model_commands("droid", &self.cmd),
            &[ProviderKind::OpenAiCompatible],
        )
        .await
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let droid_command = self.build_command_builder()?.build_initial()?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        spawn_droid(droid_command, &combined_prompt, current_dir, env, &self.cmd).await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let forked_session_id = fork_session(session_id).map_err(|e| {
            ExecutorError::FollowUpNotSupported(format!(
                "Failed to fork Droid session {session_id}: {e}"
            ))
        })?;
        let continue_cmd = self
            .build_command_builder()?
            .build_follow_up(&["--session-id".to_string(), forked_session_id.clone()])?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        spawn_droid(continue_cmd, &combined_prompt, current_dir, env, &self.cmd).await
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, current_dir: &Path) {
        normalize_logs(
            msg_store.clone(),
            current_dir,
            EntryIndexProvider::start_from(&msg_store),
        );
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if command_is_available("droid", &self.cmd) {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".factory").join("mcp.json"))
    }

    fn native_skill_discovery_roots(&self) -> Vec<std::path::PathBuf> {
        dirs::home_dir()
            .map(|home| vec![home.join(".factory").join("skills")])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::droid_auth_value_has_credentials;

    #[test]
    fn droid_auth_requires_nonempty_credentials() {
        assert!(!droid_auth_value_has_credentials(
            &serde_json::json!({"settings": {}, "apiKey": " "})
        ));
        assert!(droid_auth_value_has_credentials(
            &serde_json::json!({"auth": {"refreshToken": "token"}})
        ));
    }
}
