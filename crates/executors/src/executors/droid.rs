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
use strum_macros::AsRefStr;
use tokio::{io::AsyncWriteExt, process::Command};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use crate::{
    command::{CommandBuildError, CommandBuilder, command_is_available},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
        acp::mcp::pin_mcp_run_environment,
        opencode::FrozenProcessCommand,
        utils::{json_has_nonempty_string, read_json_file},
    },
    logs::utils::EntryIndexProvider,
    mcp_config::MemberMcpConfig,
    mcp_run::{McpRunContext, PreparedMcpRun, PrivateMcpRunDirectory},
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
#[derive(Clone)]
struct DroidMcpRuntimeSnapshot {
    run_home: PathBuf,
    factory_home: PathBuf,
    config_path: PathBuf,
    process_command: FrozenProcessCommand,
    server_count: usize,
}

impl std::fmt::Debug for DroidMcpRuntimeSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DroidMcpRuntimeSnapshot")
            .field("run_home", &self.run_home)
            .field("factory_home", &self.factory_home)
            .field("config_path", &self.config_path)
            .field("server_count", &self.server_count)
            .finish()
    }
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
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

    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    runtime_mcp_snapshot: Option<Arc<DroidMcpRuntimeSnapshot>>,

    #[cfg(test)]
    #[serde(skip)]
    #[ts(skip)]
    #[schemars(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    test_base_command: Option<String>,
}

impl Droid {
    pub fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        use crate::command::{CommandBuilder, apply_overrides};
        let configured_base = "droid exec";
        #[cfg(test)]
        let configured_base = self.test_base_command.as_deref().unwrap_or(configured_base);
        let mut builder =
            CommandBuilder::new(configured_base).params(["--output-format", "stream-json"]);
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

    fn validate_mcp_command_overrides(&self) -> Result<(), CommandBuildError> {
        for value in self.cmd.additional_params.as_deref().unwrap_or_default() {
            let normalized = value.replace(['=', '\t', '\n'], " ");
            if normalized
                .split_ascii_whitespace()
                .any(|token| token == "--settings")
            {
                return Err(CommandBuildError::InvalidShellParams(
                    "Droid --settings is controlled by run-scoped isolation".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn prepare_mcp_for_run_from(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
        source_factory_home: Option<&Path>,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        self.runtime_mcp_snapshot = None;
        if self.cmd.base_command_override.is_some() {
            return Err(ExecutorError::Configuration(
                "Droid run-scoped MCP isolation cannot be verified for a custom base command"
                    .to_string(),
            ));
        }
        self.validate_mcp_command_overrides()?;
        ensure_droid_project_mcp_is_compatible(context.current_dir(), canonical).await?;
        let prepared = PreparedMcpRun::new(canonical)?;
        let directory = PrivateMcpRunDirectory::create(context, "droid-mcp")?;
        let config_path = directory.write_file(
            Path::new(".factory").join("mcp.json"),
            &serde_json::to_vec_pretty(canonical)?,
        )?;
        directory.write_file(Path::new(".factory").join("settings.json"), br#"{}"#)?;
        bridge_droid_auth_and_session_state(&directory, source_factory_home).await?;
        let run_home = directory.path().to_path_buf();
        let factory_home = run_home.join(".factory");
        for key in ["HOME", "USERPROFILE", "FACTORY_HOME_OVERRIDE"] {
            pin_mcp_run_environment(
                env,
                &mut self.cmd,
                key,
                run_home.to_string_lossy().into_owned(),
            );
        }
        let process_command =
            FrozenProcessCommand::resolve(self.build_command_builder()?.build_initial()?).await?;
        self.runtime_mcp_snapshot = Some(Arc::new(DroidMcpRuntimeSnapshot {
            run_home,
            factory_home,
            config_path,
            process_command,
            server_count: prepared.server_count(),
        }));
        Ok(prepared.with_cleanup(directory.into_cleanup()))
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

fn droid_auth_only_config(value: &serde_json::Value) -> serde_json::Value {
    let mut auth = serde_json::Map::new();
    if let Some(source) = value.as_object() {
        for key in [
            "apiKey",
            "api_key",
            "token",
            "accessToken",
            "refreshToken",
            "auth",
        ] {
            if let Some(value) = source.get(key) {
                auth.insert(key.to_string(), value.clone());
            }
        }
    }
    serde_json::Value::Object(auth)
}

async fn ensure_droid_project_mcp_is_compatible(
    current_dir: &Path,
    canonical: &MemberMcpConfig,
) -> Result<(), ExecutorError> {
    let path = current_dir.join(".factory").join("mcp.json");
    let contents = match tokio::fs::read(&path).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ExecutorError::Io(error)),
    };
    let value: serde_json::Value = serde_json::from_slice(&contents)?;
    let servers = value
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            ExecutorError::Configuration(
                "Droid project MCP server definitions must be an object".to_string(),
            )
        })?;
    let incompatible = servers
        .iter()
        .any(|(name, definition)| canonical.mcp_servers.get(name) != Some(definition));
    if incompatible {
        return Err(ExecutorError::Configuration(
            "Droid project MCP configuration contains servers outside the frozen member snapshot"
                .to_string(),
        ));
    }
    Ok(())
}

async fn copy_optional_droid_file(
    source: &Path,
    relative_target: &Path,
    directory: &PrivateMcpRunDirectory,
) -> Result<(), ExecutorError> {
    match tokio::fs::read(source).await {
        Ok(contents) => {
            directory.write_file(relative_target, &contents)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ExecutorError::Io(error)),
    }
}

async fn bridge_droid_auth_and_session_state(
    directory: &PrivateMcpRunDirectory,
    source_home: Option<&Path>,
) -> Result<(), ExecutorError> {
    let Some(source_home) = source_home else {
        return Ok(());
    };
    for name in [
        "auth.json",
        "credentials.json",
        "auth.v2.loginkeychain",
        "host.json",
    ] {
        copy_optional_droid_file(
            &source_home.join(name),
            &Path::new(".factory").join(name),
            directory,
        )
        .await?;
    }
    match tokio::fs::read(source_home.join("config.json")).await {
        Ok(contents) => {
            let source: serde_json::Value = serde_json::from_slice(&contents)?;
            let auth = droid_auth_only_config(&source);
            if auth.as_object().is_some_and(|object| !object.is_empty()) {
                directory.write_file(
                    Path::new(".factory").join("config.json"),
                    &serde_json::to_vec_pretty(&auth)?,
                )?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ExecutorError::Io(error)),
    }
    directory.link_session_resource(
        Path::new(".factory").join("sessions"),
        source_home.join("sessions"),
    )?;
    Ok(())
}

async fn spawn_droid(
    snapshot: &DroidMcpRuntimeSnapshot,
    additional_args: &[String],
    prompt: &String,
    current_dir: &Path,
    env: &ExecutionEnv,
    cmd_overrides: &crate::command::CmdOverrides,
) -> Result<SpawnedChild, ExecutorError> {
    let (program_path, frozen_args) = snapshot.process_command.parts();
    let mut args = frozen_args.to_vec();
    args.extend_from_slice(additional_args);

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
    command
        .env("HOME", &snapshot.run_home)
        .env("USERPROFILE", &snapshot.run_home)
        .env("FACTORY_HOME_OVERRIDE", &snapshot.run_home);

    let mut child = command.group_spawn()?;

    if let Some(mut stdin) = child.inner().stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    Ok(child.into())
}

#[async_trait]
impl StandardCodingAgentExecutor for Droid {
    async fn prepare_mcp_for_run(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        let source_env = env.clone().with_profile(&self.cmd);
        let source_factory_home = source_env
            .get("FACTORY_HOME_OVERRIDE")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                source_env
                    .get("HOME")
                    .filter(|value| !value.trim().is_empty())
                    .map(PathBuf::from)
            })
            .or_else(dirs::home_dir)
            .map(|home| home.join(".factory"));
        self.prepare_mcp_for_run_from(canonical, context, env, source_factory_home.as_deref())
            .await
    }

    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        let cli_login = dirs::home_dir().is_some_and(|home| {
            let factory_home = home.join(".factory");
            let json_login = [
                factory_home.join("auth.json"),
                factory_home.join("credentials.json"),
                factory_home.join("config.json"),
            ]
            .iter()
            .filter_map(|path| read_json_file(path))
            .any(|value| droid_auth_value_has_credentials(&value));
            let keychain_login = std::fs::metadata(factory_home.join("auth.v2.loginkeychain"))
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
            json_login || keychain_login
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
        let snapshot = self
            .runtime_mcp_snapshot
            .as_ref()
            .ok_or(ExecutorError::McpIsolationNotImplemented)?;
        if self.cmd.base_command_override.is_some() {
            return Err(ExecutorError::Configuration(
                "Droid command changed after run-scoped MCP preparation".to_string(),
            ));
        }
        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        spawn_droid(snapshot, &[], &combined_prompt, current_dir, env, &self.cmd).await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let snapshot = self
            .runtime_mcp_snapshot
            .as_ref()
            .ok_or(ExecutorError::McpIsolationNotImplemented)?;
        if self.cmd.base_command_override.is_some() {
            return Err(ExecutorError::Configuration(
                "Droid command changed after run-scoped MCP preparation".to_string(),
            ));
        }
        let forked_session_id = fork_session(&snapshot.factory_home.join("sessions"), session_id)
            .map_err(|e| {
            ExecutorError::FollowUpNotSupported(format!(
                "Failed to fork Droid session {session_id}: {e}"
            ))
        })?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        spawn_droid(
            snapshot,
            &["--session-id".to_string(), forked_session_id],
            &combined_prompt,
            current_dir,
            env,
            &self.cmd,
        )
        .await
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

    fn default_runtime_config_path(&self) -> Option<std::path::PathBuf> {
        dirs::home_dir().map(|home| home.join(".factory").join("settings.json"))
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
    use std::fs;

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{Droid, droid_auth_value_has_credentials};
    use crate::{
        env::{ExecutionEnv, RepoContext},
        executors::StandardCodingAgentExecutor,
        mcp_config::MemberMcpConfig,
        mcp_run::McpRunContext,
    };

    fn test_droid() -> Droid {
        let mut droid: Droid =
            serde_json::from_value(serde_json::json!({})).expect("deserialize Droid test config");
        droid.test_base_command = Some(
            std::env::current_exe()
                .expect("resolve current test executable")
                .to_string_lossy()
                .into_owned(),
        );
        droid
    }

    fn run_context(workspace: &TempDir) -> McpRunContext {
        McpRunContext::new(workspace.path(), Uuid::new_v4(), Uuid::new_v4())
            .expect("create Droid MCP run context")
    }

    fn execution_env(workspace: &TempDir) -> ExecutionEnv {
        ExecutionEnv::new(
            RepoContext::new(workspace.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        )
    }

    #[test]
    fn droid_auth_requires_nonempty_credentials() {
        assert!(!droid_auth_value_has_credentials(
            &serde_json::json!({"settings": {}, "apiKey": " "})
        ));
        assert!(droid_auth_value_has_credentials(
            &serde_json::json!({"auth": {"refreshToken": "token"}})
        ));
    }

    #[tokio::test]
    async fn droid_private_factory_home_bridges_only_auth_and_sessions() {
        let workspace = TempDir::new().expect("create workspace");
        let source_home = TempDir::new().expect("create source home");
        let source = source_home.path().join(".factory");
        fs::create_dir(&source).expect("create source Factory home");
        let source_config = br#"{"auth":{"refreshToken":"droid-auth"},"theme":"dark"}"#;
        fs::write(source.join("config.json"), source_config).expect("write source config");
        fs::write(source.join("auth.v2.loginkeychain"), b"keychain")
            .expect("write source keychain");
        let global_mcp = br#"{"mcpServers":{"ambient":{"command":"must-not-run"}}}"#;
        fs::write(source.join("mcp.json"), global_mcp).expect("write global MCP fixture");
        fs::create_dir(source.join("sessions")).expect("create sessions fixture");
        fs::write(source.join("sessions/session.jsonl"), b"session")
            .expect("write session fixture");
        let mut droid = test_droid();
        let mut env = execution_env(&workspace);
        env.insert(
            "FACTORY_HOME_OVERRIDE",
            source_home.path().to_string_lossy().into_owned(),
        );
        let canonical: MemberMcpConfig = serde_json::from_value(serde_json::json!({
            "mcpServers": {
                "member-only": {"command": "/bin/echo", "env": {"TOKEN": "droid-mcp-secret"}}
            }
        }))
        .expect("deserialize canonical MCP config");

        let prepared = droid
            .prepare_mcp_for_run(&canonical, &run_context(&workspace), &mut env)
            .await
            .expect("prepare Droid MCP run");
        let snapshot = droid
            .runtime_mcp_snapshot
            .as_ref()
            .expect("runtime snapshot");
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(&snapshot.config_path).unwrap()).unwrap();
        let auth: serde_json::Value =
            serde_json::from_slice(&fs::read(snapshot.factory_home.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(config["mcpServers"].as_object().unwrap().len(), 1);
        assert!(config["mcpServers"].get("ambient").is_none());
        assert!(auth.get("auth").is_some());
        assert!(auth.get("theme").is_none());
        assert_eq!(
            env.get("HOME"),
            Some(&snapshot.run_home.to_string_lossy().into_owned())
        );
        assert_eq!(
            env.get("USERPROFILE"),
            Some(&snapshot.run_home.to_string_lossy().into_owned())
        );
        assert_eq!(
            env.get("FACTORY_HOME_OVERRIDE"),
            Some(&snapshot.run_home.to_string_lossy().into_owned())
        );
        assert_eq!(fs::read(source.join("config.json")).unwrap(), source_config);
        assert_eq!(fs::read(source.join("mcp.json")).unwrap(), global_mcp);
        assert!(!format!("{snapshot:?}").contains("droid-mcp-secret"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&snapshot.run_home)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&snapshot.config_path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert!(
                fs::symlink_metadata(snapshot.factory_home.join("sessions"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }

        let run_home = snapshot.run_home.clone();
        drop(prepared);
        assert!(
            !run_home.exists(),
            "cancelled run must remove Droid overlay"
        );
        assert!(source.join("sessions/session.jsonl").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn droid_spawn_failure_releases_private_mcp_overlay() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TempDir::new().expect("create workspace");
        let command_path = workspace.path().join("droid-test-command");
        fs::write(&command_path, b"#!/bin/sh\nexit 0\n").expect("write command fixture");
        let mut permissions = fs::metadata(&command_path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&command_path, permissions).expect("make command executable");
        let mut droid = test_droid();
        droid.test_base_command = Some(command_path.to_string_lossy().into_owned());
        let mut env = execution_env(&workspace);
        let prepared = droid
            .prepare_mcp_for_run_from(
                &MemberMcpConfig::default(),
                &run_context(&workspace),
                &mut env,
                None,
            )
            .await
            .expect("prepare Droid MCP run");
        let run_root = droid
            .runtime_mcp_snapshot
            .as_ref()
            .unwrap()
            .run_home
            .clone();

        fs::remove_file(command_path).expect("remove command before spawn");
        assert!(droid.spawn(workspace.path(), "prompt", &env).await.is_err());
        drop(prepared);
        assert!(!run_root.exists());
    }

    #[tokio::test]
    async fn droid_empty_config_still_writes_private_factory_mcp_file() {
        let workspace = TempDir::new().expect("create workspace");
        let mut droid = test_droid();
        let mut env = execution_env(&workspace);
        let prepared = droid
            .prepare_mcp_for_run_from(
                &MemberMcpConfig::default(),
                &run_context(&workspace),
                &mut env,
                None,
            )
            .await
            .expect("prepare empty Droid MCP run");
        let snapshot = droid
            .runtime_mcp_snapshot
            .as_ref()
            .expect("runtime snapshot");
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(&snapshot.config_path).unwrap()).unwrap();
        assert_eq!(config, serde_json::json!({"mcpServers": {}}));
        drop(prepared);
    }

    #[tokio::test]
    async fn droid_rejects_project_ambient_mcp_before_spawn() {
        let workspace = TempDir::new().expect("create workspace");
        fs::create_dir(workspace.path().join(".factory")).expect("create project config directory");
        let project_config = br#"{"mcpServers":{"ambient":{"command":"must-not-run"}}}"#;
        let project_config_path = workspace.path().join(".factory/mcp.json");
        fs::write(&project_config_path, project_config).expect("write project MCP fixture");
        let mut droid = test_droid();
        let mut env = execution_env(&workspace);

        let error = droid
            .prepare_mcp_for_run(
                &MemberMcpConfig::default(),
                &run_context(&workspace),
                &mut env,
            )
            .await
            .expect_err("ambient Droid project MCP must fail closed");

        assert!(error.to_string().contains("frozen member snapshot"));
        assert_eq!(fs::read(project_config_path).unwrap(), project_config);
        assert!(droid.runtime_mcp_snapshot.is_none());
    }
}
