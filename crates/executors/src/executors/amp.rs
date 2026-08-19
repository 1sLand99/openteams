use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use async_trait::async_trait;
use command_group::AsyncCommandGroup;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{io::AsyncWriteExt, process::Command};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use crate::{
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides, command_is_available,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
        acp::mcp::pin_mcp_run_environment,
        claude::{ClaudeLogProcessor, HistoryStrategy},
        opencode::FrozenProcessCommand,
        utils::{json_has_nonempty_string, read_json_file},
    },
    logs::{stderr_processor::normalize_stderr_logs, utils::EntryIndexProvider},
    mcp_config::MemberMcpConfig,
    mcp_run::{McpRunContext, PreparedMcpRun, PrivateMcpRunDirectory},
    model_discovery::{
        ProviderKind, cli_model_commands, discover_from_sources, runner_config_paths,
    },
};

#[derive(Clone)]
struct AmpMcpRuntimeSnapshot {
    config_home: PathBuf,
    settings_path: PathBuf,
    process_command: FrozenProcessCommand,
    server_count: usize,
}

impl std::fmt::Debug for AmpMcpRuntimeSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AmpMcpRuntimeSnapshot")
            .field("config_home", &self.config_home)
            .field("settings_path", &self.settings_path)
            .field("server_count", &self.server_count)
            .finish()
    }
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Amp {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "Model",
        description = "AMP mode profile. Mapped to CLI `--mode` (smart, deep, rush, free)."
    )]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "Dangerously Allow All",
        description = "Allow all commands to be executed, even if they are not safe."
    )]
    pub dangerously_allow_all: Option<bool>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,

    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    runtime_mcp_snapshot: Option<Arc<AmpMcpRuntimeSnapshot>>,

    #[cfg(test)]
    #[serde(skip)]
    #[ts(skip)]
    #[schemars(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    test_base_command: Option<String>,
}

impl Amp {
    const BASE_COMMAND: &'static str = "amp";

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        let configured_base = Self::BASE_COMMAND;
        #[cfg(test)]
        let configured_base = self.test_base_command.as_deref().unwrap_or(configured_base);
        let mut builder =
            CommandBuilder::new(configured_base).params(["--execute", "--stream-json"]);
        if let Some(model) = &self.model {
            builder = builder.extend_params(["--mode", model]);
        }
        if self.dangerously_allow_all.unwrap_or(false) {
            builder = builder.extend_params(["--dangerously-allow-all"]);
        }
        apply_overrides(builder, &self.cmd)
    }

    fn validate_mcp_command_overrides(&self) -> Result<(), CommandBuildError> {
        for value in self.cmd.additional_params.as_deref().unwrap_or_default() {
            let normalized = value.replace(['=', '\t', '\n'], " ");
            if let Some(flag) = ["--settings-file", "--mcp-config"].iter().find(|flag| {
                normalized
                    .split_ascii_whitespace()
                    .any(|token| token == **flag)
            }) {
                return Err(CommandBuildError::InvalidShellParams(format!(
                    "Amp {flag} is controlled by run-scoped MCP isolation"
                )));
            }
        }
        Ok(())
    }

    async fn prepare_mcp_for_run_from(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
        source_settings_path: Option<&Path>,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        self.runtime_mcp_snapshot = None;
        if self.cmd.base_command_override.is_some() {
            return Err(ExecutorError::Configuration(
                "Amp run-scoped MCP isolation cannot be verified for a custom base command"
                    .to_string(),
            ));
        }
        self.validate_mcp_command_overrides()?;
        let source_settings = match source_settings_path {
            Some(path) => match tokio::fs::read(path).await {
                Ok(contents) => Some(serde_json::from_slice(&contents)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(ExecutorError::Io(error)),
            },
            None => None,
        };
        let prepared = PreparedMcpRun::new(canonical)?;
        let directory = PrivateMcpRunDirectory::create(context, "amp-mcp")?;
        let settings = build_amp_run_settings(canonical, source_settings.as_ref());
        let settings_path = directory.write_file(
            Path::new("amp").join("settings.json"),
            &serde_json::to_vec_pretty(&settings)?,
        )?;
        let config_home = directory.path().to_path_buf();
        pin_mcp_run_environment(
            env,
            &mut self.cmd,
            "XDG_CONFIG_HOME",
            config_home.to_string_lossy().into_owned(),
        );
        pin_mcp_run_environment(
            env,
            &mut self.cmd,
            "AMP_SETTINGS_FILE",
            settings_path.to_string_lossy().into_owned(),
        );
        let process_command =
            FrozenProcessCommand::resolve(self.build_command_builder()?.build_initial()?).await?;
        self.runtime_mcp_snapshot = Some(Arc::new(AmpMcpRuntimeSnapshot {
            config_home,
            settings_path,
            process_command,
            server_count: prepared.server_count(),
        }));
        Ok(prepared.with_cleanup(directory.into_cleanup()))
    }
}

fn amp_auth_value_has_credentials(value: &serde_json::Value) -> bool {
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

fn amp_settings_path() -> Option<PathBuf> {
    amp_settings_path_from(
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn amp_settings_path_from(
    config_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
) -> Option<PathBuf> {
    let config_home = config_home.filter(|value| !value.as_os_str().is_empty());
    #[cfg(windows)]
    let config_home = config_home.or_else(dirs::config_dir);
    #[cfg(not(windows))]
    let config_home = config_home.or_else(|| user_home.map(|home| home.join(".config")));
    config_home.map(|home| home.join("amp").join("settings.json"))
}

fn build_amp_run_settings(
    canonical: &MemberMcpConfig,
    source_settings: Option<&serde_json::Value>,
) -> serde_json::Value {
    let mut settings = serde_json::Map::new();
    if let Some(source) = source_settings.and_then(serde_json::Value::as_object) {
        for key in [
            "apiKey",
            "api_key",
            "token",
            "accessToken",
            "refreshToken",
            "auth",
        ] {
            if let Some(value) = source.get(key) {
                settings.insert(key.to_string(), value.clone());
            }
        }
    }
    settings.insert(
        "amp.mcpServers".to_string(),
        serde_json::to_value(&canonical.mcp_servers)
            .expect("BTreeMap JSON serialization cannot fail"),
    );
    serde_json::Value::Object(settings)
}

#[async_trait]
impl StandardCodingAgentExecutor for Amp {
    async fn prepare_mcp_for_run(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        let source_env = env.clone().with_profile(&self.cmd);
        let source_settings_path = source_env
            .get("AMP_SETTINGS_FILE")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                amp_settings_path_from(
                    source_env
                        .get("XDG_CONFIG_HOME")
                        .filter(|value| !value.trim().is_empty())
                        .map(PathBuf::from),
                    dirs::home_dir(),
                )
            });
        self.prepare_mcp_for_run_from(canonical, context, env, source_settings_path.as_deref())
            .await
    }

    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        let cli_login = amp_settings_path()
            .and_then(|path| read_json_file(&path))
            .is_some_and(|value| amp_auth_value_has_credentials(&value));
        self.authentication_detected(&env, &["AMP_API_KEY"], cli_login)
    }

    async fn list_models(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        let config_paths = runner_config_paths([self.default_mcp_config_path()]);
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

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let snapshot = self
            .runtime_mcp_snapshot
            .as_ref()
            .ok_or(ExecutorError::McpIsolationNotImplemented)?;
        if self.cmd.base_command_override.is_some() {
            return Err(ExecutorError::Configuration(
                "Amp command changed after run-scoped MCP preparation".to_string(),
            ));
        }
        let (executable_path, args) = snapshot.process_command.parts();

        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        let mut command = Command::new(executable_path);
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .args(args);

        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut command);
        command
            .env("XDG_CONFIG_HOME", &snapshot.config_home)
            .env("AMP_SETTINGS_FILE", &snapshot.settings_path);

        let mut child = command.group_spawn()?;

        // Feed the prompt in, then close the pipe so amp sees EOF
        if let Some(mut stdin) = child.inner().stdin.take() {
            stdin.write_all(combined_prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        Ok(child.into())
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        // 1) Fork the thread synchronously to obtain new thread id
        let snapshot = self
            .runtime_mcp_snapshot
            .as_ref()
            .ok_or(ExecutorError::McpIsolationNotImplemented)?;
        if self.cmd.base_command_override.is_some() {
            return Err(ExecutorError::Configuration(
                "Amp command changed after run-scoped MCP preparation".to_string(),
            ));
        }
        let (fork_program, frozen_args) = snapshot.process_command.parts();
        let mut fork_args = frozen_args.to_vec();
        fork_args.extend([
            "threads".to_string(),
            "fork".to_string(),
            session_id.to_string(),
        ]);
        let mut fork_command = Command::new(fork_program);
        fork_command
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .args(&fork_args);
        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut fork_command);
        fork_command
            .env("XDG_CONFIG_HOME", &snapshot.config_home)
            .env("AMP_SETTINGS_FILE", &snapshot.settings_path);
        let fork_output = fork_command.output().await?;
        let stdout_str = String::from_utf8_lossy(&fork_output.stdout);
        let new_thread_id = stdout_str
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        if new_thread_id.is_empty() {
            return Err(ExecutorError::Io(std::io::Error::other(
                "AMP threads fork did not return a thread id",
            )));
        }

        tracing::debug!("AMP threads fork -> new thread id: {}", new_thread_id);

        // 2) Continue using the new thread id
        let mut continue_args = frozen_args.to_vec();
        continue_args.extend([
            "threads".to_string(),
            "continue".to_string(),
            new_thread_id.clone(),
        ]);

        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        let mut command = Command::new(fork_program);
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .args(&continue_args);

        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut command);
        command
            .env("XDG_CONFIG_HOME", &snapshot.config_home)
            .env("AMP_SETTINGS_FILE", &snapshot.settings_path);

        let mut child = command.group_spawn()?;

        // Feed the prompt in, then close the pipe so amp sees EOF
        if let Some(mut stdin) = child.inner().stdin.take() {
            stdin.write_all(combined_prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        Ok(child.into())
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, current_dir: &Path) {
        let entry_index_provider = EntryIndexProvider::start_from(&msg_store);

        // Process stdout logs (Amp's stream JSON output) using Claude's log processor
        ClaudeLogProcessor::process_logs(
            msg_store.clone(),
            current_dir,
            entry_index_provider.clone(),
            HistoryStrategy::AmpResume,
        );

        // Process stderr logs using the standard stderr processor
        normalize_stderr_logs(msg_store, entry_index_provider);
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if command_is_available(Self::BASE_COMMAND, &self.cmd) {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }

    // MCP configuration methods
    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        amp_settings_path()
    }

    fn native_skill_discovery_roots(&self) -> Vec<std::path::PathBuf> {
        dirs::home_dir()
            .map(|home| vec![home.join(".agents").join("skills")])
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{Amp, amp_auth_value_has_credentials};
    use crate::{
        command::CmdOverrides,
        env::{ExecutionEnv, RepoContext},
        executors::{AppendPrompt, StandardCodingAgentExecutor},
        mcp_config::MemberMcpConfig,
        mcp_run::McpRunContext,
    };

    fn test_amp() -> Amp {
        let mut amp: Amp =
            serde_json::from_value(serde_json::json!({})).expect("deserialize Amp test config");
        amp.test_base_command = Some(
            std::env::current_exe()
                .expect("resolve current test executable")
                .to_string_lossy()
                .into_owned(),
        );
        amp
    }

    fn run_context(workspace: &TempDir) -> McpRunContext {
        McpRunContext::new(workspace.path(), Uuid::new_v4(), Uuid::new_v4())
            .expect("create Amp MCP run context")
    }

    fn execution_env(workspace: &TempDir) -> ExecutionEnv {
        ExecutionEnv::new(
            RepoContext::new(workspace.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        )
    }

    #[test]
    fn command_builder_uses_native_amp_command() {
        let amp = Amp {
            append_prompt: AppendPrompt::default(),
            model: None,
            dangerously_allow_all: None,
            cmd: CmdOverrides::default(),
            runtime_mcp_snapshot: None,
            test_base_command: None,
        };
        let (program, args) = amp
            .build_command_builder()
            .expect("build Amp command")
            .build_initial()
            .expect("build initial Amp command")
            .into_parts_for_test();

        assert_eq!(program, "amp");
        assert_eq!(args, ["--execute", "--stream-json"]);
    }

    #[test]
    fn amp_auth_requires_nonempty_credentials() {
        assert!(!amp_auth_value_has_credentials(
            &serde_json::json!({"theme": "dark", "token": " "})
        ));
        assert!(amp_auth_value_has_credentials(
            &serde_json::json!({"auth": {"accessToken": "token"}})
        ));
    }

    #[tokio::test]
    async fn amp_private_xdg_contains_only_member_servers_and_auth_bridge() {
        let workspace = TempDir::new().expect("create workspace");
        let source = workspace.path().join("global-settings.json");
        let source_bytes = br#"{
            "accessToken":"amp-auth-secret",
            "theme":"dark",
            "amp.mcpServers":{"ambient":{"command":"must-not-run"}}
        }"#;
        fs::write(&source, source_bytes).expect("write source settings");
        let mut amp = test_amp();
        let mut env = execution_env(&workspace);
        let canonical: MemberMcpConfig = serde_json::from_value(serde_json::json!({
            "mcpServers": {
                "member-only": {"command": "/bin/echo", "env": {"TOKEN": "mcp-secret"}}
            }
        }))
        .expect("deserialize member MCP config");

        let prepared = amp
            .prepare_mcp_for_run_from(
                &canonical,
                &run_context(&workspace),
                &mut env,
                Some(&source),
            )
            .await
            .expect("prepare Amp MCP run");
        let snapshot = amp.runtime_mcp_snapshot.as_ref().expect("runtime snapshot");
        let settings: serde_json::Value =
            serde_json::from_slice(&fs::read(&snapshot.settings_path).expect("read run settings"))
                .expect("parse run settings");
        assert_eq!(settings["accessToken"], "amp-auth-secret");
        assert!(settings.get("theme").is_none());
        assert!(settings["amp.mcpServers"].get("ambient").is_none());
        assert!(settings["amp.mcpServers"].get("member-only").is_some());
        assert_eq!(
            env.get("XDG_CONFIG_HOME"),
            Some(&snapshot.config_home.to_string_lossy().into_owned())
        );
        assert_eq!(
            env.get("AMP_SETTINGS_FILE"),
            Some(&snapshot.settings_path.to_string_lossy().into_owned())
        );
        assert_eq!(fs::read(&source).unwrap(), source_bytes);
        assert!(!format!("{snapshot:?}").contains("mcp-secret"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&snapshot.config_home)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&snapshot.settings_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let run_home = snapshot.config_home.clone();
        drop(prepared);
        assert!(!run_home.exists(), "cancelled run must remove Amp overlay");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn amp_spawn_failure_releases_private_mcp_overlay() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TempDir::new().expect("create workspace");
        let command_path = workspace.path().join("amp-test-command");
        fs::write(&command_path, b"#!/bin/sh\nexit 0\n").expect("write command fixture");
        let mut permissions = fs::metadata(&command_path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&command_path, permissions).expect("make command executable");
        let mut amp = test_amp();
        amp.test_base_command = Some(command_path.to_string_lossy().into_owned());
        let mut env = execution_env(&workspace);
        let prepared = amp
            .prepare_mcp_for_run_from(
                &MemberMcpConfig::default(),
                &run_context(&workspace),
                &mut env,
                None,
            )
            .await
            .expect("prepare Amp MCP run");
        let run_root = amp
            .runtime_mcp_snapshot
            .as_ref()
            .unwrap()
            .config_home
            .clone();

        fs::remove_file(command_path).expect("remove command before spawn");
        assert!(amp.spawn(workspace.path(), "prompt", &env).await.is_err());
        drop(prepared);
        assert!(!run_root.exists());
    }

    #[tokio::test]
    async fn amp_empty_config_still_pins_explicit_private_settings() {
        let workspace = TempDir::new().expect("create workspace");
        let mut amp = test_amp();
        let mut env = execution_env(&workspace);
        let prepared = amp
            .prepare_mcp_for_run_from(
                &MemberMcpConfig::default(),
                &run_context(&workspace),
                &mut env,
                None,
            )
            .await
            .expect("prepare empty Amp MCP run");
        let snapshot = amp.runtime_mcp_snapshot.as_ref().expect("runtime snapshot");
        let settings: serde_json::Value = serde_json::from_slice(
            &fs::read(&snapshot.settings_path).expect("read empty settings"),
        )
        .expect("parse empty settings");
        assert_eq!(settings, serde_json::json!({"amp.mcpServers": {}}));
        drop(prepared);
    }
}
