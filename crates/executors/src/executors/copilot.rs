use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use command_group::AsyncCommandGroup;
use derivative::Derivative;
use futures::StreamExt;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    io::AsyncWriteExt,
    process::Command,
    time::{interval, timeout},
};
use ts_rs::TS;
use uuid::Uuid;
use workspace_utils::msg_store::MsgStore;

use crate::{
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides, command_is_available,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
        acp::mcp::pin_mcp_run_environment,
        opencode::FrozenProcessCommand,
        utils::{json_has_nonempty_string, read_json_file},
    },
    logs::{
        NormalizedEntry, NormalizedEntryType, plain_text_processor::PlainTextLogProcessor,
        stderr_processor::normalize_stderr_logs, utils::EntryIndexProvider,
    },
    mcp_config::MemberMcpConfig,
    mcp_run::{McpRunContext, PreparedMcpRun, PrivateMcpRunDirectory},
    model_discovery::{
        ProviderKind, cli_model_commands, discover_from_sources, runner_config_paths,
    },
    stdout_dup::{self, StdoutAppender},
};

#[derive(Clone)]
struct CopilotMcpRuntimeSnapshot {
    copilot_home: PathBuf,
    config_path: PathBuf,
    log_dir: PathBuf,
    disabled_servers: Vec<String>,
    process_command: FrozenProcessCommand,
    server_count: usize,
}

impl std::fmt::Debug for CopilotMcpRuntimeSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CopilotMcpRuntimeSnapshot")
            .field("copilot_home", &self.copilot_home)
            .field("config_path", &self.config_path)
            .field("log_dir", &self.log_dir)
            .field("disabled_servers", &self.disabled_servers)
            .field("server_count", &self.server_count)
            .finish()
    }
}

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Copilot {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all_tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny_tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_dir: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disable_mcp_server: Option<Vec<String>>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,

    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    runtime_mcp_snapshot: Option<Arc<CopilotMcpRuntimeSnapshot>>,

    #[cfg(test)]
    #[serde(skip)]
    #[ts(skip)]
    #[schemars(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    test_base_command: Option<String>,
}

impl Copilot {
    const BASE_COMMAND: &'static str = "copilot";

    fn build_command_builder(
        &self,
        log_dir: &str,
        defensive_disabled_servers: &[String],
    ) -> Result<CommandBuilder, CommandBuildError> {
        let configured_base = Self::BASE_COMMAND;
        #[cfg(test)]
        let configured_base = self.test_base_command.as_deref().unwrap_or(configured_base);
        let mut builder = CommandBuilder::new(configured_base).params([
            "--no-color",
            "--log-level",
            "debug",
            "--log-dir",
            log_dir,
        ]);

        if self.allow_all_tools.unwrap_or(false) {
            builder = builder.extend_params(["--allow-all-tools"]);
        }

        if let Some(model) = &self.model {
            builder = builder.extend_params(["--model", model]);
        }

        if let Some(tool) = &self.allow_tool {
            builder = builder.extend_params(["--allow-tool", tool]);
        }

        if let Some(tool) = &self.deny_tool {
            builder = builder.extend_params(["--deny-tool", tool]);
        }

        if let Some(dirs) = &self.add_dir {
            for dir in dirs {
                builder = builder.extend_params(["--add-dir", dir]);
            }
        }

        if let Some(servers) = &self.disable_mcp_server {
            for server in servers {
                builder = builder.extend_params(["--disable-mcp-server", server]);
            }
        }
        for server in defensive_disabled_servers {
            if self
                .disable_mcp_server
                .as_ref()
                .is_none_or(|configured| !configured.contains(server))
            {
                builder = builder.extend_params(["--disable-mcp-server", server]);
            }
        }

        apply_overrides(builder, &self.cmd)
    }

    async fn prepare_mcp_for_run_from(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
        source_home: Option<&Path>,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        self.runtime_mcp_snapshot = None;
        if self.cmd.base_command_override.is_some() {
            return Err(ExecutorError::Configuration(
                "Copilot run-scoped MCP isolation cannot be verified for a custom base command"
                    .to_string(),
            ));
        }
        let prepared = PreparedMcpRun::new(canonical)?;
        let directory = PrivateMcpRunDirectory::create(context, "copilot-mcp")?;
        let config_path =
            directory.write_file("mcp-config.json", &serde_json::to_vec_pretty(canonical)?)?;
        bridge_copilot_auth_and_session_state(&directory, source_home).await?;
        let disabled_servers =
            copilot_defensive_disabled_servers(context.current_dir(), canonical).await?;
        let copilot_home = directory.path().to_path_buf();
        let log_dir = directory.create_directory("logs")?;
        pin_mcp_run_environment(
            env,
            &mut self.cmd,
            "COPILOT_HOME",
            copilot_home.to_string_lossy().into_owned(),
        );
        let process_command = FrozenProcessCommand::resolve(
            self.build_command_builder(&log_dir.to_string_lossy(), &disabled_servers)?
                .build_initial()?,
        )
        .await?;
        self.runtime_mcp_snapshot = Some(Arc::new(CopilotMcpRuntimeSnapshot {
            copilot_home,
            config_path,
            log_dir,
            disabled_servers,
            process_command,
            server_count: prepared.server_count(),
        }));
        Ok(prepared.with_cleanup(directory.into_cleanup()))
    }
}

fn copilot_auth_value_has_credentials(value: &serde_json::Value) -> bool {
    value.get("loggedInUsers").is_some_and(|users| match users {
        serde_json::Value::Array(users) => !users.is_empty(),
        serde_json::Value::Object(users) => !users.is_empty(),
        _ => false,
    }) || json_has_nonempty_string(value, &["/token", "/oauthToken", "/githubToken"])
}

fn copilot_auth_only_config(value: &serde_json::Value) -> serde_json::Value {
    let mut auth = serde_json::Map::new();
    if let Some(source) = value.as_object() {
        for key in ["loggedInUsers", "token", "oauthToken", "githubToken"] {
            if let Some(value) = source.get(key) {
                auth.insert(key.to_string(), value.clone());
            }
        }
    }
    serde_json::Value::Object(auth)
}

async fn bridge_copilot_auth_and_session_state(
    directory: &PrivateMcpRunDirectory,
    source_home: Option<&Path>,
) -> Result<(), ExecutorError> {
    let Some(source_home) = source_home else {
        return Ok(());
    };
    match tokio::fs::read(source_home.join("config.json")).await {
        Ok(contents) => {
            let source: serde_json::Value = serde_json::from_slice(&contents)?;
            let auth = copilot_auth_only_config(&source);
            if auth.as_object().is_some_and(|object| !object.is_empty()) {
                directory.write_file("config.json", &serde_json::to_vec_pretty(&auth)?)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(ExecutorError::Io(error)),
    }
    for name in ["session-state", "mcp-oauth-config", "mcp-secrets"] {
        directory.link_session_resource(name, source_home.join(name))?;
    }
    for name in [
        "session-store.db",
        "session-store.db-wal",
        "session-store.db-shm",
    ] {
        directory.link_session_resource(name, source_home.join(name))?;
    }
    Ok(())
}

async fn copilot_defensive_disabled_servers(
    current_dir: &Path,
    canonical: &MemberMcpConfig,
) -> Result<Vec<String>, ExecutorError> {
    let mut disabled = BTreeSet::new();
    if !canonical.mcp_servers.contains_key("github") {
        disabled.insert("github".to_string());
    }
    for path in [
        current_dir.join(".mcp.json"),
        current_dir.join(".github").join("mcp.json"),
    ] {
        let contents = match tokio::fs::read(&path).await {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(ExecutorError::Io(error)),
        };
        let value: serde_json::Value = serde_json::from_slice(&contents)?;
        let root = value.as_object().ok_or_else(|| {
            ExecutorError::Configuration(
                "Copilot project MCP configuration must be an object".to_string(),
            )
        })?;
        let servers = match root.get("mcpServers") {
            Some(value) => value.as_object().ok_or_else(|| {
                ExecutorError::Configuration(
                    "Copilot project mcpServers must be an object".to_string(),
                )
            })?,
            None => root,
        };
        for (name, definition) in servers {
            if canonical.mcp_servers.get(name) != Some(definition) {
                disabled.insert(name.clone());
            }
        }
    }
    Ok(disabled.into_iter().collect())
}

#[async_trait]
impl StandardCodingAgentExecutor for Copilot {
    async fn prepare_mcp_for_run(
        &mut self,
        canonical: &MemberMcpConfig,
        context: &McpRunContext,
        env: &mut ExecutionEnv,
    ) -> Result<PreparedMcpRun, ExecutorError> {
        let source_env = env.clone().with_profile(&self.cmd);
        let source_home = source_env
            .get("COPILOT_HOME")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .or_else(copilot_home);
        self.prepare_mcp_for_run_from(canonical, context, env, source_home.as_deref())
            .await
    }

    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        let cli_login = copilot_config_path().is_some_and(|path| {
            read_json_file(&path).is_some_and(|value| copilot_auth_value_has_credentials(&value))
        });
        self.authentication_detected(
            &env,
            &[
                "COPILOT_GITHUB_TOKEN",
                "COPILOT_PROVIDER_API_KEY",
                "GH_TOKEN",
                "GITHUB_TOKEN",
            ],
            cli_login,
        )
    }

    async fn list_models(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        let config_paths = runner_config_paths([
            self.default_mcp_config_path(),
            dirs::home_dir().map(|home| home.join(".copilot").join("config.json")),
            dirs::home_dir().map(|home| home.join(".copilot").join("settings.json")),
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
                "Copilot command changed after run-scoped MCP preparation".to_string(),
            ));
        }
        let (program_path, args) = snapshot.process_command.parts();
        let log_dir = snapshot.log_dir.clone();

        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        let mut command = Command::new(program_path);
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .env("NODE_NO_WARNINGS", "1")
            .args(args);

        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut command);
        command.env("COPILOT_HOME", &snapshot.copilot_home);

        let mut child = command.group_spawn()?;

        // Write prompt to stdin
        if let Some(mut stdin) = child.inner().stdin.take() {
            stdin.write_all(combined_prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let (_, appender) = stdout_dup::tee_stdout_with_appender(&mut child)?;
        Self::send_session_id(log_dir, appender);

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
        let snapshot = self
            .runtime_mcp_snapshot
            .as_ref()
            .ok_or(ExecutorError::McpIsolationNotImplemented)?;
        if self.cmd.base_command_override.is_some() {
            return Err(ExecutorError::Configuration(
                "Copilot command changed after run-scoped MCP preparation".to_string(),
            ));
        }
        let (program_path, frozen_args) = snapshot.process_command.parts();
        let mut args = frozen_args.to_vec();
        args.extend(["--resume".to_string(), session_id.to_string()]);
        let log_dir = snapshot.log_dir.clone();

        let combined_prompt = self.append_prompt.combine_prompt(prompt);

        let mut command = Command::new(program_path);

        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .env("NODE_NO_WARNINGS", "1")
            .args(&args);

        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut command);
        command.env("COPILOT_HOME", &snapshot.copilot_home);

        let mut child = command.group_spawn()?;

        // Write comprehensive prompt to stdin
        if let Some(mut stdin) = child.inner().stdin.take() {
            stdin.write_all(combined_prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let (_, appender) = stdout_dup::tee_stdout_with_appender(&mut child)?;
        Self::send_session_id(log_dir, appender);

        Ok(child.into())
    }

    /// Parses both stderr and stdout logs for Copilot executor using PlainTextLogProcessor.
    ///
    /// Each entry is converted into an `AssistantMessage` or `ErrorMessage` and emitted as patches.
    fn normalize_logs(&self, msg_store: Arc<MsgStore>, _worktree_path: &Path) {
        let entry_index_counter = EntryIndexProvider::start_from(&msg_store);
        normalize_stderr_logs(msg_store.clone(), entry_index_counter.clone());

        // Normalize Agent logs
        tokio::spawn(async move {
            // Use stdout_lines_stream_until_close to ensure we process all stdout,
            // including error messages that may arrive just before Finished signal.
            let mut stdout_lines = msg_store.stdout_lines_stream_until_close();

            let mut processor = Self::create_simple_stdout_normalizer(entry_index_counter);

            while let Some(Ok(line)) = stdout_lines.next().await {
                if let Some(session_id) = line.strip_prefix(Self::SESSION_PREFIX) {
                    msg_store.push_session_id(session_id.trim().to_string());
                    continue;
                }

                for patch in processor.process(line + "\n") {
                    msg_store.push_patch(patch);
                }
            }
        });
    }

    fn default_runtime_config_path(&self) -> Option<std::path::PathBuf> {
        copilot_home().map(|home| home.join("settings.json"))
    }

    // MCP configuration methods
    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        copilot_home().map(|home| home.join("mcp-config.json"))
    }

    fn native_skill_discovery_roots(&self) -> Vec<std::path::PathBuf> {
        dirs::home_dir()
            .map(|home| vec![home.join(".github").join("skills")])
            .unwrap_or_default()
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if command_is_available(Self::BASE_COMMAND, &self.cmd) {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

fn copilot_home() -> Option<PathBuf> {
    copilot_home_from(
        std::env::var_os("COPILOT_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn copilot_home_from(
    configured_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
) -> Option<PathBuf> {
    configured_home.or_else(|| user_home.map(|home| home.join(".copilot")))
}

fn copilot_config_path() -> Option<PathBuf> {
    copilot_home().map(|home| home.join("config.json"))
}

impl Copilot {
    fn create_simple_stdout_normalizer(
        index_provider: EntryIndexProvider,
    ) -> PlainTextLogProcessor {
        PlainTextLogProcessor::builder()
            .normalized_entry_producer(Box::new(|content: String| NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::AssistantMessage,
                content,
                metadata: None,
            }))
            .transform_lines(Box::new(|lines| {
                lines.iter_mut().for_each(|line| {
                    *line = strip_ansi_escapes::strip_str(&line);
                })
            }))
            .index_provider(index_provider)
            .build()
    }

    // Scan the log directory for a file named `<UUID>.log` or `session-<UUID>.log` and extract the UUID as session ID.
    async fn watch_session_id(log_dir_path: PathBuf) -> Result<String, String> {
        let session_regex =
            Regex::new(r"events to session ([0-9a-fA-F-]{36})").map_err(|e| e.to_string())?;

        let log_dir_clone = log_dir_path.clone();
        timeout(Duration::from_secs(600), async move {
            let mut ticker = interval(Duration::from_millis(200));
            loop {
                if let Ok(mut rd) = fs::read_dir(&log_dir_clone).await {
                    while let Ok(Some(entry)) = rd.next_entry().await {
                        let path = entry.path();
                        if path.extension().map(|e| e == "log").unwrap_or(false)
                            && let Ok(content) = fs::read_to_string(&path).await
                            && let Some(caps) = session_regex.captures(&content)
                            && let Some(matched) = caps.get(1)
                        {
                            let uuid_str = matched.as_str();
                            if Uuid::parse_str(uuid_str).is_ok() {
                                return Ok(uuid_str.to_string());
                            }
                        }
                    }
                }
                ticker.tick().await;
            }
        })
        .await
        .map_err(|_| format!("No session ID found in log files at {log_dir_path:?}"))?
    }

    const SESSION_PREFIX: &'static str = "[copilot-session] ";

    // Find session id and write it to stdout prefixed
    fn send_session_id(log_dir_path: PathBuf, stdout_appender: StdoutAppender) {
        tokio::spawn(async move {
            match Self::watch_session_id(log_dir_path).await {
                Ok(session_id) => {
                    let session_line = format!("{}{}\n", Self::SESSION_PREFIX, session_id);
                    stdout_appender.append_line(&session_line);
                }
                Err(e) => {
                    tracing::error!("Failed to find session ID: {}", e);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{Copilot, copilot_auth_value_has_credentials, copilot_home_from};
    use crate::{
        command::CmdOverrides,
        env::{ExecutionEnv, RepoContext},
        executors::{AppendPrompt, StandardCodingAgentExecutor},
        mcp_config::MemberMcpConfig,
        mcp_run::McpRunContext,
    };

    fn test_copilot() -> Copilot {
        let mut copilot: Copilot =
            serde_json::from_value(serde_json::json!({})).expect("deserialize Copilot test config");
        copilot.test_base_command = Some(
            std::env::current_exe()
                .expect("resolve current test executable")
                .to_string_lossy()
                .into_owned(),
        );
        copilot
    }

    fn run_context(workspace: &TempDir) -> McpRunContext {
        McpRunContext::new(workspace.path(), Uuid::new_v4(), Uuid::new_v4())
            .expect("create Copilot MCP run context")
    }

    fn execution_env(workspace: &TempDir) -> ExecutionEnv {
        ExecutionEnv::new(
            RepoContext::new(workspace.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        )
    }

    #[test]
    fn command_builder_uses_native_copilot_command() {
        let copilot = Copilot {
            append_prompt: AppendPrompt::default(),
            model: None,
            allow_all_tools: None,
            allow_tool: None,
            deny_tool: None,
            add_dir: None,
            disable_mcp_server: None,
            cmd: CmdOverrides::default(),
            runtime_mcp_snapshot: None,
            test_base_command: None,
        };
        let (program, args) = copilot
            .build_command_builder("copilot-logs", &[])
            .expect("build Copilot command")
            .build_initial()
            .expect("build initial Copilot command")
            .into_parts_for_test();

        assert_eq!(program, "copilot");
        assert_eq!(
            args,
            [
                "--no-color",
                "--log-level",
                "debug",
                "--log-dir",
                "copilot-logs"
            ]
        );
    }

    #[test]
    fn copilot_auth_requires_a_logged_in_user_or_token() {
        assert!(!copilot_auth_value_has_credentials(
            &serde_json::json!({"loggedInUsers": [], "githubToken": " "})
        ));
        assert!(copilot_auth_value_has_credentials(
            &serde_json::json!({"loggedInUsers": [{"login": "octocat"}]})
        ));
    }

    #[test]
    fn copilot_home_prefers_configured_directory_and_has_default() {
        let user_home = PathBuf::from("/users/tester");
        let configured_home = PathBuf::from("/configured/copilot");

        assert_eq!(
            copilot_home_from(Some(configured_home.clone()), Some(user_home.clone())),
            Some(configured_home)
        );
        assert_eq!(
            copilot_home_from(None, Some(user_home.clone())),
            Some(user_home.join(".copilot"))
        );
    }

    #[tokio::test]
    async fn copilot_private_home_bridges_auth_session_and_disables_ambient_servers() {
        let workspace = TempDir::new().expect("create workspace");
        let source = TempDir::new().expect("create source Copilot home");
        let source_config = br#"{"loggedInUsers":[{"login":"octocat"}],"theme":"dark"}"#;
        fs::write(source.path().join("config.json"), source_config)
            .expect("write auth config fixture");
        let global_mcp = br#"{"mcpServers":{"ambient-global":{"command":"must-not-run"}}}"#;
        fs::write(source.path().join("mcp-config.json"), global_mcp)
            .expect("write global MCP fixture");
        fs::create_dir(source.path().join("session-state")).expect("create session fixture");
        fs::write(source.path().join("session-state/events.jsonl"), b"session")
            .expect("write session fixture");
        fs::write(source.path().join("session-store.db"), b"database")
            .expect("write session database fixture");
        fs::write(
            workspace.path().join(".mcp.json"),
            br#"{"mcpServers":{"ambient-project":{"command":"must-not-run"}}}"#,
        )
        .expect("write project MCP fixture");
        let mut copilot = test_copilot();
        copilot.disable_mcp_server = Some(vec!["profile-disabled".to_string()]);
        let mut env = execution_env(&workspace);
        let canonical: MemberMcpConfig = serde_json::from_value(serde_json::json!({
            "mcpServers": {
                "member-only": {"command": "/bin/echo", "env": {"TOKEN": "copilot-mcp-secret"}}
            }
        }))
        .expect("deserialize canonical MCP config");

        let prepared = copilot
            .prepare_mcp_for_run_from(
                &canonical,
                &run_context(&workspace),
                &mut env,
                Some(source.path()),
            )
            .await
            .expect("prepare Copilot MCP run");
        let snapshot = copilot
            .runtime_mcp_snapshot
            .as_ref()
            .expect("runtime snapshot");
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(&snapshot.config_path).unwrap()).unwrap();
        let auth: serde_json::Value =
            serde_json::from_slice(&fs::read(snapshot.copilot_home.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(config["mcpServers"].as_object().unwrap().len(), 1);
        assert!(config["mcpServers"].get("ambient-global").is_none());
        assert!(auth.get("loggedInUsers").is_some());
        assert!(auth.get("theme").is_none());
        assert_eq!(snapshot.disabled_servers, ["ambient-project", "github"]);
        let args = snapshot.process_command.parts().1;
        for disabled in ["profile-disabled", "ambient-project", "github"] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "--disable-mcp-server" && pair[1] == disabled)
            );
        }
        assert_eq!(
            env.get("COPILOT_HOME"),
            Some(&snapshot.copilot_home.to_string_lossy().into_owned())
        );
        assert_eq!(
            fs::read(source.path().join("config.json")).unwrap(),
            source_config
        );
        assert_eq!(
            fs::read(source.path().join("mcp-config.json")).unwrap(),
            global_mcp
        );
        assert!(!format!("{snapshot:?}").contains("copilot-mcp-secret"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&snapshot.copilot_home)
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
                fs::symlink_metadata(snapshot.copilot_home.join("session-state"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }

        let run_home = snapshot.copilot_home.clone();
        drop(prepared);
        assert!(
            !run_home.exists(),
            "cancelled run must remove Copilot overlay"
        );
        assert!(source.path().join("session-state/events.jsonl").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn copilot_spawn_failure_releases_private_mcp_overlay() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TempDir::new().expect("create workspace");
        let command_path = workspace.path().join("copilot-test-command");
        fs::write(&command_path, b"#!/bin/sh\nexit 0\n").expect("write command fixture");
        let mut permissions = fs::metadata(&command_path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&command_path, permissions).expect("make command executable");
        let mut copilot = test_copilot();
        copilot.test_base_command = Some(command_path.to_string_lossy().into_owned());
        let mut env = execution_env(&workspace);
        let prepared = copilot
            .prepare_mcp_for_run_from(
                &MemberMcpConfig::default(),
                &run_context(&workspace),
                &mut env,
                None,
            )
            .await
            .expect("prepare Copilot MCP run");
        let run_root = copilot
            .runtime_mcp_snapshot
            .as_ref()
            .unwrap()
            .copilot_home
            .clone();

        fs::remove_file(command_path).expect("remove command before spawn");
        assert!(
            copilot
                .spawn(workspace.path(), "prompt", &env)
                .await
                .is_err()
        );
        drop(prepared);
        assert!(!run_root.exists());
    }

    #[tokio::test]
    async fn copilot_empty_config_still_writes_private_mcp_file() {
        let workspace = TempDir::new().expect("create workspace");
        fs::write(
            workspace.path().join(".mcp.json"),
            br#"{"mcpServers":{"ambient":{"command":"must-not-run"}}}"#,
        )
        .expect("write project MCP fixture");
        let mut copilot = test_copilot();
        let mut env = execution_env(&workspace);
        let prepared = copilot
            .prepare_mcp_for_run_from(
                &MemberMcpConfig::default(),
                &run_context(&workspace),
                &mut env,
                None,
            )
            .await
            .expect("prepare empty Copilot MCP run");
        let snapshot = copilot
            .runtime_mcp_snapshot
            .as_ref()
            .expect("runtime snapshot");
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(&snapshot.config_path).unwrap()).unwrap();
        assert_eq!(config, serde_json::json!({"mcpServers": {}}));
        assert!(snapshot.disabled_servers.contains(&"github".to_string()));
        assert!(snapshot.disabled_servers.contains(&"ambient".to_string()));
        drop(prepared);
    }
}
