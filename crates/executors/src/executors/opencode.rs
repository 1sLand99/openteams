use std::{
    collections::{BTreeSet, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use derivative::Derivative;
use futures::StreamExt;
use jsonc_parser::ParseOptions;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncBufReadExt, process::Command};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, ExecutorExitResult, SpawnedChild,
        StandardCodingAgentExecutor, opencode::types::OpencodeExecutorEvent,
    },
    logs::utils::patch,
    skill_config::NativeSkillConfigBackend,
    stdout_dup::create_stdout_pipe_writer,
};

mod models;
mod normalize_logs;
mod sdk;
mod slash_commands;
mod types;

use sdk::{LogWriter, RunConfig, generate_server_password, run_session, run_slash_command};
use slash_commands::{OpencodeSlashCommand, hardcoded_slash_commands};

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Opencode {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "mode")]
    pub agent: Option<String>,
    /// Auto-approve agent actions
    #[serde(default = "default_to_true")]
    pub auto_approve: bool,
    /// Enable auto-compaction when the context length approaches the model's context window limit
    #[serde(default = "default_to_true")]
    pub auto_compact: bool,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

/// Represents a spawned OpenCode server used by agent and slash-command execution.
struct OpencodeServer {
    #[allow(unused)]
    child: Option<AsyncGroupChild>,
    base_url: String,
    server_password: ServerPassword,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    _runtime_lease: OpencodeRuntimeLease,
}

impl Drop for OpencodeServer {
    fn drop(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        if let Some(mut child) = self.child.take() {
            tokio::spawn(async move {
                let _ = workspace_utils::process::kill_process_group(&mut child).await;
            });
        }
    }
}

type ServerPassword = String;
const MAX_SERVER_LOG_LINES: usize = 200;
const MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OPENCODE_RUNTIME_DIRS: usize = 16;
const MAX_OPENCODE_RUNTIME_BYTES: u64 = 256 * 1024 * 1024;
const OPENCODE_RUNTIME_LAST_USED_MARKER: &str = ".last_used";

impl Opencode {
    pub const PACKAGE_VERSION: &'static str = "1.17.18";
    const BASE_COMMAND: &'static str = "npx -y opencode-ai@1.17.18";

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        let builder = CommandBuilder::new(Self::BASE_COMMAND)
            // Pass hostname/port as separate args so OpenCode treats them as explicitly set
            // (it checks `process.argv.includes(\"--port\")` / `\"--hostname\"`).
            .extend_params(["serve", "--hostname", "127.0.0.1", "--port", "0"]);
        apply_overrides(builder, &self.cmd)
    }

    /// Compute a cache key for model context windows based on configuration that can affect the list of available models.
    fn compute_models_cache_key(&self) -> String {
        serde_json::json!({
            "cmd": &self.cmd,
            "opencode_version": Self::PACKAGE_VERSION,
        })
        .to_string()
    }

    pub async fn list_models(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<Vec<String>, ExecutorError> {
        let configured_provider_ids = user_opencode_configured_provider_ids(current_dir);
        self.list_models_via_cli(current_dir, env, &configured_provider_ids)
            .await
    }

    async fn list_models_via_cli(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
        configured_provider_ids: &HashSet<String>,
    ) -> Result<Vec<String>, ExecutorError> {
        let mut provider_ids = BTreeSet::new();
        provider_ids.insert("opencode".to_string());
        provider_ids.extend(configured_provider_ids.iter().cloned());

        let mut models = BTreeSet::new();
        let mut errors = Vec::new();
        for provider_id in provider_ids {
            let include_paid = configured_provider_ids.contains(provider_id.as_str());
            match self
                .run_models_command(current_dir, env, &provider_id)
                .await
            {
                Ok(output) => {
                    models.extend(parse_models_command_output(&output, include_paid));
                }
                Err(err) => {
                    errors.push(format!("{provider_id}: {err}"));
                }
            }
        }

        if models.is_empty() && !errors.is_empty() {
            return Err(ExecutorError::Io(io::Error::other(format!(
                "OpenCode CLI model discovery failed: {}",
                errors.join("; ")
            ))));
        }

        Ok(models.into_iter().collect())
    }

    async fn run_models_command(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
        provider_id: &str,
    ) -> Result<String, ExecutorError> {
        let command_parts = apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params([
                "models",
                provider_id,
                "--verbose",
            ]),
            &self.cmd,
        )?
        .build_initial()?;
        let (program_path, args) = command_parts.into_resolved().await?;
        let mut command = Command::new(program_path);
        command
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .env("NODE_NO_WARNINGS", "1")
            .env("NO_COLOR", "1")
            .args(&args);

        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut command);
        let _runtime_lease =
            apply_isolated_opencode_env(&mut command, current_dir, Self::PACKAGE_VERSION)?;

        let output = tokio::time::timeout(MODEL_DISCOVERY_TIMEOUT, command.output())
            .await
            .map_err(|_| {
                ExecutorError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("OpenCode CLI model discovery timed out for provider {provider_id}"),
                ))
            })?
            .map_err(ExecutorError::Io)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ExecutorError::Io(io::Error::other(format!(
                "OpenCode CLI model discovery failed for provider {provider_id}: {}",
                stderr.trim()
            ))));
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Common boilerplate for spawning an OpenCode server process.
    async fn spawn_server_process(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<
        (
            AsyncGroupChild,
            ServerPassword,
            String,
            OpencodeRuntimeLease,
        ),
        ExecutorError,
    > {
        let command_parts = self.build_command_builder()?.build_initial()?;
        let (program_path, args) = command_parts.into_resolved().await?;

        let server_password = generate_server_password();
        let startup_command = format_command_for_log(&program_path, &args);
        tracing::info!(
            opencode_startup_command = %startup_command,
            current_dir = %current_dir.display(),
            "Starting OpenCode server process"
        );

        let mut command = Command::new(program_path);
        command
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .current_dir(current_dir)
            .env("NPM_CONFIG_LOGLEVEL", "error")
            .env("NODE_NO_WARNINGS", "1")
            .env("NO_COLOR", "1")
            .env("OPENCODE_SERVER_USERNAME", "opencode")
            .env("OPENCODE_SERVER_PASSWORD", &server_password)
            .args(&args);

        env.clone()
            .with_profile(&self.cmd)
            .apply_to_command(&mut command);
        let runtime_lease =
            apply_isolated_opencode_env(&mut command, current_dir, Self::PACKAGE_VERSION)?;

        let child = command.group_spawn()?;

        Ok((child, server_password, startup_command, runtime_lease))
    }

    /// Handles process spawning and waits for the server URL used by slash commands.
    async fn spawn_server(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<OpencodeServer, ExecutorError> {
        let (mut child, server_password, startup_command, runtime_lease) =
            self.spawn_server_process(current_dir, env).await?;
        let Some(server_stdout) = child.inner().stdout.take() else {
            let _ = workspace_utils::process::kill_process_group(&mut child).await;
            return Err(ExecutorError::Io(std::io::Error::other(
                "OpenCode server missing stdout",
            )));
        };
        let (stderr_lines, stderr_task) = collect_server_stderr(child.inner().stderr.take());

        let base_url = match wait_for_server_url(server_stdout, None).await {
            Ok(base_url) => base_url,
            Err(err) => {
                let server_logs = {
                    let lines = stderr_lines.lock().await;
                    format_server_log_tail(&lines)
                };
                if let Some(task) = stderr_task {
                    task.abort();
                }
                let _ = workspace_utils::process::kill_process_group(&mut child).await;
                return Err(opencode_server_error(err, &startup_command, &server_logs));
            }
        };

        Ok(OpencodeServer {
            child: Some(child),
            base_url,
            server_password,
            stderr_task,
            _runtime_lease: runtime_lease,
        })
    }

    async fn spawn_inner(
        &self,
        current_dir: &Path,
        prompt: &str,
        resume_session: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let slash_command = OpencodeSlashCommand::parse(prompt);
        let combined_prompt = if slash_command.is_some() {
            prompt.to_string()
        } else {
            self.append_prompt.combine_prompt(prompt)
        };

        let (mut child, server_password, _startup_command, runtime_lease) =
            self.spawn_server_process(current_dir, env).await?;
        let Some(server_stdout) = child.inner().stdout.take() else {
            let _ = workspace_utils::process::kill_process_group(&mut child).await;
            return Err(ExecutorError::Io(std::io::Error::other(
                "OpenCode server missing stdout",
            )));
        };

        let stdout = create_stdout_pipe_writer(&mut child)?;
        let log_writer = LogWriter::new(stdout);

        let (exit_signal_tx, exit_signal_rx) = tokio::sync::oneshot::channel();
        let cancel = tokio_util::sync::CancellationToken::new();

        // Prepare config values that will be moved into the spawned task
        let directory = current_dir.to_string_lossy().to_string();
        let approvals = if self.auto_approve {
            None
        } else {
            self.approvals.clone()
        };
        let model = self.model.clone();
        let model_variant = self.variant.clone();
        let agent = self.agent.clone();
        let auto_approve = self.auto_approve;
        let resume_session_id = resume_session.map(|s| s.to_string());
        let models_cache_key = self.compute_models_cache_key();
        let cancel_for_task = cancel.clone();
        let commit_reminder = env.commit_reminder;
        let commit_reminder_prompt = env.commit_reminder_prompt.clone();
        let repo_context = env.repo_context.clone();

        tokio::spawn(async move {
            let _runtime_lease = runtime_lease;
            // Wait for server to print listening URL
            let base_url = match wait_for_server_url(server_stdout, Some(log_writer.clone())).await
            {
                Ok(url) => url,
                Err(err) => {
                    let _ = log_writer
                        .log_error(format!("OpenCode startup error: {err}"))
                        .await;
                    let _ = exit_signal_tx.send(ExecutorExitResult::Failure);
                    return;
                }
            };

            let config = RunConfig {
                base_url,
                directory,
                prompt: combined_prompt,
                resume_session_id,
                model,
                model_variant,
                agent,
                approvals,
                auto_approve,
                server_password,
                expected_version: Self::PACKAGE_VERSION.to_string(),
                models_cache_key,
                commit_reminder,
                commit_reminder_prompt,
                repo_context,
            };

            let result = match slash_command {
                Some(command) => {
                    run_slash_command(config, log_writer.clone(), command, cancel_for_task).await
                }
                None => run_session(config, log_writer.clone(), cancel_for_task).await,
            };
            let exit_result = match result {
                Ok(()) => ExecutorExitResult::Success,
                Err(err) => {
                    let _ = log_writer
                        .log_error(format!("OpenCode executor error: {err}"))
                        .await;
                    ExecutorExitResult::Failure
                }
            };
            let _ = exit_signal_tx.send(exit_result);
        });

        Ok(SpawnedChild {
            child,
            exit_signal: Some(exit_signal_rx),
            cancel: Some(cancel),
        })
    }
}

fn collect_server_stderr(
    stderr: Option<tokio::process::ChildStderr>,
) -> (
    Arc<tokio::sync::Mutex<Vec<String>>>,
    Option<tokio::task::JoinHandle<()>>,
) {
    let lines = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let Some(stderr) = stderr else {
        return (lines, None);
    };

    let task_lines = Arc::clone(&lines);
    let task = tokio::spawn(async move {
        let mut stderr_lines = tokio::io::BufReader::new(stderr).lines();
        loop {
            match stderr_lines.next_line().await {
                Ok(Some(line)) => {
                    tracing::debug!(line = %line, "OpenCode server stderr");
                    let mut captured = task_lines.lock().await;
                    captured.push(line);
                    if captured.len() > MAX_SERVER_LOG_LINES {
                        let excess = captured.len() - MAX_SERVER_LOG_LINES;
                        captured.drain(0..excess);
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    tracing::debug!("Failed to read OpenCode server stderr: {err}");
                    break;
                }
            }
        }
    });

    (lines, Some(task))
}

fn apply_isolated_opencode_env(
    command: &mut Command,
    current_dir: &Path,
    package_version: &str,
) -> io::Result<OpencodeRuntimeLease> {
    let opencode_root = openteams_home_dir()?.join("opencode");
    let runtime_root = opencode_root
        .join(format!(
            "opencode-ai-{}",
            sanitize_path_segment(package_version)
        ))
        .join(workspace_runtime_key(current_dir));
    let data_home = runtime_root.join("data");
    let state_home = runtime_root.join("state");
    let opencode_data_dir = data_home.join("opencode");

    let runtime_lease = OpencodeRuntimeLease::acquire(&opencode_root, &runtime_root)?;
    std::fs::create_dir_all(&opencode_data_dir)?;
    std::fs::create_dir_all(&state_home)?;
    mirror_user_opencode_auth(&opencode_data_dir)?;
    if let Err(err) = touch_opencode_runtime(&runtime_root) {
        tracing::warn!(
            runtime_dir = %runtime_root.display(),
            "Failed to update OpenCode runtime usage marker: {err}"
        );
    }
    match prune_opencode_runtime_dirs(
        &opencode_root,
        &runtime_root,
        MAX_OPENCODE_RUNTIME_DIRS,
        MAX_OPENCODE_RUNTIME_BYTES,
    ) {
        Ok(result) => {
            if result.removed_dirs > 0 {
                tracing::info!(
                    removed_dirs = result.removed_dirs,
                    removed_bytes = result.removed_bytes,
                    remaining_dirs = result.remaining_dirs,
                    remaining_bytes = result.remaining_bytes,
                    "Pruned isolated OpenCode runtime directories"
                );
            }
            if result.remaining_dirs > MAX_OPENCODE_RUNTIME_DIRS
                || result.remaining_bytes > MAX_OPENCODE_RUNTIME_BYTES
            {
                tracing::warn!(
                    remaining_dirs = result.remaining_dirs,
                    remaining_bytes = result.remaining_bytes,
                    "OpenCode runtime directory remains over its retention limit"
                );
            }
        }
        Err(err) => {
            tracing::warn!("Failed to prune isolated OpenCode runtime directories: {err}");
        }
    }

    command
        .env("XDG_DATA_HOME", data_home)
        .env("XDG_STATE_HOME", state_home);
    Ok(runtime_lease)
}

#[derive(Debug)]
struct OpencodeRuntimeLease {
    _lock_file: fs::File,
}

impl OpencodeRuntimeLease {
    fn acquire(opencode_root: &Path, runtime_dir: &Path) -> io::Result<Self> {
        let lock_path = opencode_runtime_lock_path(opencode_root, runtime_dir);
        let lock_parent = lock_path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "OpenCode lock path has no parent",
            )
        })?;
        fs::create_dir_all(lock_parent)?;
        let lock_file = open_opencode_runtime_lock(&lock_path)?;
        lock_file.lock_shared()?;
        Ok(Self {
            _lock_file: lock_file,
        })
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OpencodeRuntimePruneResult {
    removed_dirs: usize,
    removed_bytes: u64,
    remaining_dirs: usize,
    remaining_bytes: u64,
}

#[derive(Debug)]
struct OpencodeRuntimeDir {
    path: PathBuf,
    version_dir: PathBuf,
    last_used: SystemTime,
    bytes: u64,
    protected: bool,
}

fn touch_opencode_runtime(runtime_dir: &Path) -> io::Result<()> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    let mut marker = fs::File::create(runtime_dir.join(OPENCODE_RUNTIME_LAST_USED_MARKER))?;
    marker.write_all(timestamp.as_bytes())
}

fn prune_opencode_runtime_dirs(
    opencode_root: &Path,
    protected_runtime: &Path,
    max_dirs: usize,
    max_bytes: u64,
) -> io::Result<OpencodeRuntimePruneResult> {
    fs::create_dir_all(opencode_root.join(".locks"))?;
    let mut runtimes = discover_opencode_runtime_dirs(opencode_root, protected_runtime)?;
    runtimes.sort_by(|left, right| {
        left.last_used
            .cmp(&right.last_used)
            .then_with(|| left.path.cmp(&right.path))
    });

    let mut result = OpencodeRuntimePruneResult {
        remaining_dirs: runtimes.len(),
        remaining_bytes: runtimes
            .iter()
            .fold(0_u64, |total, runtime| total.saturating_add(runtime.bytes)),
        ..Default::default()
    };
    let mut version_dirs = HashSet::new();

    for runtime in runtimes {
        if result.remaining_dirs <= max_dirs && result.remaining_bytes <= max_bytes {
            break;
        }
        if runtime.protected {
            continue;
        }

        let lock_path = opencode_runtime_lock_path(opencode_root, &runtime.path);
        let lock_file = match open_opencode_runtime_lock(&lock_path) {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(
                    runtime_dir = %runtime.path.display(),
                    "Failed to open OpenCode runtime retention lock: {err}"
                );
                continue;
            }
        };
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => continue,
            Err(fs::TryLockError::Error(err)) => {
                tracing::warn!(
                    runtime_dir = %runtime.path.display(),
                    "Failed to lock stale OpenCode runtime directory: {err}"
                );
                continue;
            }
        }

        match fs::remove_dir_all(&runtime.path) {
            Ok(()) => {
                result.removed_dirs += 1;
                result.removed_bytes = result.removed_bytes.saturating_add(runtime.bytes);
                result.remaining_dirs = result.remaining_dirs.saturating_sub(1);
                result.remaining_bytes = result.remaining_bytes.saturating_sub(runtime.bytes);
                version_dirs.insert(runtime.version_dir);
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                result.remaining_dirs = result.remaining_dirs.saturating_sub(1);
                result.remaining_bytes = result.remaining_bytes.saturating_sub(runtime.bytes);
            }
            Err(err) => {
                tracing::warn!(
                    runtime_dir = %runtime.path.display(),
                    "Failed to remove stale OpenCode runtime directory: {err}"
                );
            }
        }
    }

    for version_dir in version_dirs {
        match fs::remove_dir(&version_dir) {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                ) => {}
            Err(err) => {
                tracing::debug!(
                    version_dir = %version_dir.display(),
                    "Failed to remove empty OpenCode version directory: {err}"
                );
            }
        }
    }

    Ok(result)
}

fn open_opencode_runtime_lock(lock_path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
}

fn opencode_runtime_lock_path(opencode_root: &Path, runtime_dir: &Path) -> PathBuf {
    let relative_runtime = runtime_dir
        .strip_prefix(opencode_root)
        .unwrap_or(runtime_dir);
    let mut hasher = Sha256::new();
    hasher.update(relative_runtime.to_string_lossy().as_bytes());
    opencode_root
        .join(".locks")
        .join(format!("{:x}.lock", hasher.finalize()))
}

fn discover_opencode_runtime_dirs(
    opencode_root: &Path,
    protected_runtime: &Path,
) -> io::Result<Vec<OpencodeRuntimeDir>> {
    let mut runtimes = Vec::new();
    let version_entries = match fs::read_dir(opencode_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(runtimes),
        Err(err) => return Err(err),
    };

    for version_entry in version_entries.flatten() {
        let version_path = version_entry.path();
        let Ok(file_type) = version_entry.file_type() else {
            continue;
        };
        if !file_type.is_dir()
            || !version_entry
                .file_name()
                .to_string_lossy()
                .starts_with("opencode-ai-")
        {
            continue;
        }
        let Ok(workspace_entries) = fs::read_dir(&version_path) else {
            continue;
        };

        for workspace_entry in workspace_entries.flatten() {
            let Ok(file_type) = workspace_entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() || !is_workspace_runtime_dir_name(&workspace_entry.file_name()) {
                continue;
            }

            let path = workspace_entry.path();
            let (bytes, newest_modified) = opencode_runtime_usage(&path);
            let marker_modified = fs::metadata(path.join(OPENCODE_RUNTIME_LAST_USED_MARKER))
                .and_then(|metadata| metadata.modified())
                .ok();
            let directory_modified = workspace_entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(UNIX_EPOCH);
            let last_used = marker_modified
                .into_iter()
                .chain(newest_modified)
                .max()
                .unwrap_or(directory_modified);

            runtimes.push(OpencodeRuntimeDir {
                protected: path == protected_runtime,
                path,
                version_dir: version_path.clone(),
                last_used,
                bytes,
            });
        }
    }

    Ok(runtimes)
}

fn is_workspace_runtime_dir_name(name: &std::ffi::OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.len() == 16 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn opencode_runtime_usage(runtime_dir: &Path) -> (u64, Option<SystemTime>) {
    let mut bytes = 0_u64;
    let mut newest_modified = None;

    for entry in walkdir::WalkDir::new(runtime_dir)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_file() {
            bytes = bytes.saturating_add(metadata.len());
        }
        if let Ok(modified) = metadata.modified() {
            newest_modified =
                Some(newest_modified.map_or(modified, |current: SystemTime| current.max(modified)));
        }
    }

    (bytes, newest_modified)
}

fn openteams_home_dir() -> io::Result<PathBuf> {
    dirs::home_dir()
        .map(|home| home.join(".openteams"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))
}

fn workspace_runtime_key(current_dir: &Path) -> String {
    let canonical =
        std::fs::canonicalize(current_dir).unwrap_or_else(|_| current_dir.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    format!("{:x}", digest)[..16].to_string()
}

fn mirror_user_opencode_auth(isolated_opencode_data_dir: &Path) -> io::Result<()> {
    let target_auth = isolated_opencode_data_dir.join("auth.json");
    let Some(source_auth) = user_opencode_auth_path().filter(|path| path.is_file()) else {
        return remove_file_if_present(&target_auth);
    };
    if source_auth == target_auth {
        return Ok(());
    }

    remove_file_if_present(&target_auth)?;
    symlink_or_copy_file(&source_auth, &target_auth)
}

fn user_opencode_auth_path() -> Option<PathBuf> {
    opencode_auth_path(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn opencode_auth_path(
    xdg_data_home: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    // OpenCode 1.17.8 uses xdg-basedir on every platform, including Windows.
    // Its fallback is ~/.local/share rather than %LOCALAPPDATA%.
    xdg_data_home
        .or_else(|| home_dir.map(|home| home.join(".local").join("share")))
        .map(|data| data.join("opencode").join("auth.json"))
}

fn user_opencode_config_paths(current_dir: &Path) -> Vec<PathBuf> {
    opencode_config_paths(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        dirs::home_dir(),
        Some(current_dir.to_path_buf()),
    )
}

fn opencode_config_paths(
    xdg_config_home: Option<PathBuf>,
    home_dir: Option<PathBuf>,
    current_dir: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let config_home =
        xdg_config_home.or_else(|| home_dir.as_ref().map(|home| home.join(".config")));
    if let Some(config_home) = config_home {
        let config_dir = config_home.join("opencode");
        paths.extend([
            config_dir.join("config.json"),
            config_dir.join("opencode.json"),
            config_dir.join("opencode.jsonc"),
        ]);
    }

    if let Some(home) = home_dir.as_ref() {
        let legacy_dir = home.join(".opencode");
        paths.extend([
            legacy_dir.join("opencode.json"),
            legacy_dir.join("opencode.jsonc"),
        ]);
    }

    if let Some(current_dir) = current_dir {
        let current_dir = std::fs::canonicalize(&current_dir).unwrap_or(current_dir);
        let stop_dir = home_dir.and_then(|home| std::fs::canonicalize(home).ok());
        for dir in current_dir.ancestors() {
            paths.extend([dir.join("opencode.jsonc"), dir.join("opencode.json")]);
            let local_config_dir = dir.join(".opencode");
            paths.extend([
                local_config_dir.join("opencode.json"),
                local_config_dir.join("opencode.jsonc"),
            ]);

            if stop_dir.as_deref().is_some_and(|stop| stop == dir) {
                break;
            }
        }
    }

    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn user_opencode_configured_provider_ids(current_dir: &Path) -> HashSet<String> {
    let mut ids = user_opencode_auth_provider_ids();
    ids.extend(user_opencode_config_provider_ids(current_dir));
    ids
}

fn user_opencode_auth_provider_ids() -> HashSet<String> {
    let Some(path) = user_opencode_auth_path() else {
        return HashSet::new();
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return HashSet::new(),
        Err(err) => {
            tracing::warn!(path = %path.display(), "Failed to read OpenCode auth store: {err}");
            return HashSet::new();
        }
    };

    match serde_json::from_str::<Value>(&content) {
        Ok(value) => opencode_auth_provider_ids(&value),
        Err(err) => {
            tracing::warn!(path = %path.display(), "Failed to parse OpenCode auth store: {err}");
            HashSet::new()
        }
    }
}

fn user_opencode_config_provider_ids(current_dir: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    for path in user_opencode_config_paths(current_dir) {
        let Some(value) = read_opencode_config_value(&path) else {
            continue;
        };
        ids.extend(opencode_config_provider_ids(&value));
    }
    ids
}

fn user_opencode_global_config_provider_ids() -> HashSet<String> {
    let mut ids = HashSet::new();
    for path in opencode_config_paths(
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        dirs::home_dir(),
        None,
    ) {
        let Some(value) = read_opencode_config_value(&path) else {
            continue;
        };
        ids.extend(opencode_config_provider_ids(&value));
    }
    ids
}

fn opencode_auth_provider_ids(value: &Value) -> HashSet<String> {
    value
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, auth)| valid_opencode_auth(auth))
        .map(|(provider_id, _)| provider_id.clone())
        .collect()
}

fn valid_opencode_auth(value: &Value) -> bool {
    let Some(auth) = value.as_object() else {
        return false;
    };
    let has_value = |key: &str| {
        auth.get(key)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    };

    match auth.get("type").and_then(Value::as_str) {
        Some("api") => has_value("key"),
        Some("oauth") => has_value("access") || has_value("refresh"),
        Some("wellknown") => has_value("key") && has_value("token"),
        _ => false,
    }
}

fn read_opencode_config_value(path: &Path) -> Option<Value> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return None,
        Err(err) => {
            tracing::warn!(path = %path.display(), "Failed to read OpenCode config: {err}");
            return None;
        }
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Some(Value::Object(Default::default()));
    }

    if let Ok(Some(value)) = jsonc_parser::parse_to_serde_value(trimmed, &ParseOptions::default()) {
        return Some(value);
    }

    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => Some(value),
        Err(err) => {
            tracing::warn!(path = %path.display(), "Failed to parse OpenCode config: {err}");
            None
        }
    }
}

fn opencode_config_provider_ids(value: &Value) -> HashSet<String> {
    value
        .get("provider")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|providers| providers.keys())
        .filter_map(|provider_id| {
            let provider_id = provider_id.trim();
            (!provider_id.is_empty()).then(|| provider_id.to_string())
        })
        .collect()
}

fn symlink_or_copy_file(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(source, target).is_ok() {
            return Ok(());
        }
    }
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_file(source, target).is_ok() {
            return Ok(());
        }
    }

    std::fs::copy(source, target).map(|_| ())
}

fn sanitize_path_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn format_server_log_tail(captured: &[String]) -> String {
    captured
        .iter()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

fn opencode_server_error(
    err: ExecutorError,
    startup_command: &str,
    server_logs: &str,
) -> ExecutorError {
    let mut message = format!("{err}\nOpenCode startup command:\n{startup_command}");
    if !server_logs.trim().is_empty() {
        message.push_str("\nOpenCode server logs:\n");
        message.push_str(server_logs);
    }
    ExecutorError::Io(io::Error::other(message))
}

fn format_command_for_log(program: &Path, args: &[String]) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.push(quote_command_part(&program.display().to_string()));

    let mut redact_next = false;
    for arg in args {
        let formatted = if redact_next {
            redact_next = false;
            "<redacted>".to_string()
        } else if let Some(redacted) = redact_sensitive_arg(arg) {
            if !arg.contains('=') {
                redact_next = true;
            }
            redacted
        } else {
            arg.clone()
        };
        parts.push(quote_command_part(&formatted));
    }

    parts.join(" ")
}

fn redact_sensitive_arg(arg: &str) -> Option<String> {
    let lower = arg.to_ascii_lowercase();
    let is_sensitive =
        lower.contains("key") || lower.contains("token") || lower.contains("password");
    if !is_sensitive {
        return None;
    }

    match arg.split_once('=') {
        Some((name, _value)) => Some(format!("{name}=<redacted>")),
        None => Some(arg.to_string()),
    }
}

fn quote_command_part(value: &str) -> String {
    if value.is_empty()
        || value
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '"' | '\''))
    {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

fn opencode_model_is_free(info: &Value) -> bool {
    let Some(cost) = info.get("cost").and_then(Value::as_object) else {
        return false;
    };
    number_value(cost.get("input")) == Some(0.0) && number_value(cost.get("output")) == Some(0.0)
}

fn number_value(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

fn parse_models_command_output(output: &str, include_paid: bool) -> Vec<String> {
    let lines = output.lines().collect::<Vec<_>>();
    let mut models = BTreeSet::new();
    let mut index = 0;

    while index < lines.len() {
        let model_name = lines[index].trim();
        index += 1;

        if model_name.is_empty() || !model_name.contains('/') || model_name.starts_with('{') {
            continue;
        }

        while index < lines.len() && lines[index].trim().is_empty() {
            index += 1;
        }

        if index >= lines.len() || !lines[index].trim_start().starts_with('{') {
            if include_paid {
                models.insert(model_name.to_string());
            }
            continue;
        }

        let mut json_block = String::new();
        let mut parsed_info = None;
        while index < lines.len() {
            json_block.push_str(lines[index]);
            json_block.push('\n');
            index += 1;

            match serde_json::from_str::<Value>(&json_block) {
                Ok(value) => {
                    parsed_info = Some(value);
                    break;
                }
                Err(err) if err.is_eof() => continue,
                Err(_) => break,
            }
        }

        if include_paid || parsed_info.as_ref().is_some_and(opencode_model_is_free) {
            models.insert(model_name.to_string());
        }
    }

    models.into_iter().collect()
}

fn format_tail(captured: Vec<String>) -> String {
    captured
        .into_iter()
        .rev()
        .take(12)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}

async fn wait_for_server_url(
    stdout: tokio::process::ChildStdout,
    log_writer: Option<LogWriter>,
) -> Result<String, ExecutorError> {
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    let mut captured: Vec<String> = Vec::new();

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(ExecutorError::Io(std::io::Error::other(format!(
                "Timed out waiting for OpenCode server to print listening URL.\nServer output tail:\n{}",
                format_tail(captured)
            ))));
        }

        let line = match tokio::time::timeout_at(deadline, lines.next_line()).await {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => {
                return Err(ExecutorError::Io(std::io::Error::other(format!(
                    "OpenCode server exited before printing listening URL.\nServer output tail:\n{}",
                    format_tail(captured)
                ))));
            }
            Ok(Err(err)) => return Err(ExecutorError::Io(err)),
            Err(_) => continue,
        };

        if let Some(log_writer) = &log_writer {
            log_writer
                .log_event(&OpencodeExecutorEvent::StartupLog {
                    message: line.clone(),
                })
                .await?;
        }
        if captured.len() < 64 {
            captured.push(line.clone());
        }

        if let Some(url) = line.trim().strip_prefix("opencode server listening on ") {
            // Keep draining stdout to avoid backpressure on the server, but don't block startup.
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(lines.into_inner()).lines();
                while let Ok(Some(_)) = lines.next_line().await {}
            });
            return Ok(url.trim().to_string());
        }
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for Opencode {
    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        let cli_auth = !user_opencode_auth_provider_ids().is_empty()
            || !user_opencode_global_config_provider_ids().is_empty()
            || env
                .get("OPENCODE_CONFIG_CONTENT")
                .and_then(|content| serde_json::from_str::<Value>(content).ok())
                .is_some_and(|value| !opencode_config_provider_ids(&value).is_empty());
        self.authentication_detected(
            &env,
            &[
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "OPENROUTER_API_KEY",
                "GOOGLE_API_KEY",
                "GEMINI_API_KEY",
                "GITHUB_TOKEN",
                "COPILOT_TOKEN",
                "DASHSCOPE_API_KEY",
                "QWEN_API_KEY",
                "MOONSHOT_API_KEY",
                "KIMI_API_KEY",
            ],
            cli_auth,
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
        Opencode::list_models(self, current_dir, env)
            .await
            .map(Some)
    }

    async fn available_slash_commands(
        &self,
        current_dir: &Path,
    ) -> Result<futures::stream::BoxStream<'static, json_patch::Patch>, ExecutorError> {
        let defaults = hardcoded_slash_commands();
        let this = self.clone();
        let current_dir = current_dir.to_path_buf();

        let initial = patch::slash_commands(defaults.clone(), true, None);

        let discovery_stream = futures::stream::once(async move {
            match this.discover_slash_commands(&current_dir).await {
                Ok(commands) => patch::slash_commands(commands, false, None),
                Err(e) => {
                    tracing::warn!("Failed to discover OpenCode slash commands: {}", e);
                    patch::slash_commands(defaults, false, Some(e.to_string()))
                }
            }
        });

        Ok(Box::pin(
            futures::stream::once(async move { initial }).chain(discovery_stream),
        ))
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let env = setup_permissions_env(self.auto_approve, env);
        let env = setup_compaction_env(self.auto_compact, &env);
        self.spawn_inner(current_dir, prompt, None, &env).await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let env = setup_permissions_env(self.auto_approve, env);
        let env = setup_compaction_env(self.auto_compact, &env);
        self.spawn_inner(current_dir, prompt, Some(session_id), &env)
            .await
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        normalize_logs::normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        #[cfg(not(windows))]
        {
            let base_dirs = xdg::BaseDirectories::with_prefix("opencode");
            // First try opencode.json, then opencode.jsonc
            base_dirs
                .get_config_file("opencode.json")
                .filter(|p| p.exists())
                .or_else(|| base_dirs.get_config_file("opencode.jsonc"))
        }
        #[cfg(windows)]
        {
            let config_dir = std::env::var("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .ok()
                .or_else(|| dirs::home_dir().map(|p| p.join(".config")))
                .map(|p| p.join("opencode"))?;

            let path = Some(config_dir.join("opencode.json"))
                .filter(|p| p.exists())
                .unwrap_or_else(|| config_dir.join("opencode.jsonc"));
            Some(path)
        }
    }

    fn default_skill_config_path(&self) -> Option<std::path::PathBuf> {
        self.default_mcp_config_path()
    }

    fn native_skill_discovery_roots(&self) -> Vec<std::path::PathBuf> {
        let mut roots = Vec::new();

        if let Some(home) = dirs::home_dir() {
            roots.push(home.join(".opencode").join("skills"));
            roots.push(home.join(".claude").join("skills"));
            roots.push(home.join(".agents").join("skills"));
        }

        roots
    }

    fn native_skill_config_backend(&self) -> NativeSkillConfigBackend {
        NativeSkillConfigBackend::Opencode
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        let mcp_config_found = self
            .default_mcp_config_path()
            .map(|p| p.exists())
            .unwrap_or(false);

        // Check multiple installation indicator paths:
        // 1. XDG config dir: $XDG_CONFIG_HOME/opencode
        // 2. XDG data dir: $XDG_DATA_HOME/opencode
        // 3. XDG state dir: $XDG_STATE_HOME/opencode
        // 4. OpenCode CLI home: ~/.opencode
        #[cfg(not(windows))]
        let installation_indicator_found = {
            let base_dirs = xdg::BaseDirectories::with_prefix("opencode");

            let config_dir_exists = base_dirs
                .get_config_home()
                .map(|config| config.exists())
                .unwrap_or(false);

            let data_dir_exists = base_dirs
                .get_data_home()
                .map(|data| data.exists())
                .unwrap_or(false);

            let state_dir_exists = base_dirs
                .get_state_home()
                .map(|state| state.exists())
                .unwrap_or(false);

            config_dir_exists || data_dir_exists || state_dir_exists
        };

        #[cfg(windows)]
        let installation_indicator_found = std::env::var("XDG_CONFIG_HOME")
            .ok()
            .map(std::path::PathBuf::from)
            .and_then(|p| p.join("opencode").exists().then_some(()))
            .or_else(|| {
                dirs::home_dir()
                    .and_then(|p| p.join(".config").join("opencode").exists().then_some(()))
            })
            .is_some();

        let home_opencode_exists = dirs::home_dir()
            .map(|home| home.join(".opencode").exists())
            .unwrap_or(false);

        if mcp_config_found || installation_indicator_found || home_opencode_exists {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

fn default_to_true() -> bool {
    true
}

fn setup_permissions_env(auto_approve: bool, env: &ExecutionEnv) -> ExecutionEnv {
    let mut env = env.clone();

    let permissions = match env.get("OPENCODE_PERMISSION") {
        Some(existing) => merge_question_deny(existing),
        None => build_default_permissions(auto_approve),
    };

    env.insert("OPENCODE_PERMISSION", &permissions);
    env
}

fn build_default_permissions(auto_approve: bool) -> String {
    if auto_approve {
        r#"{"question":"deny"}"#.to_string()
    } else {
        r#"{"edit":"ask","bash":"ask","webfetch":"ask","doom_loop":"ask","external_directory":"ask","question":"deny"}"#.to_string()
    }
}

fn merge_question_deny(existing_json: &str) -> String {
    let mut permissions: Map<String, serde_json::Value> =
        serde_json::from_str(existing_json.trim()).unwrap_or_default();

    permissions.insert(
        "question".to_string(),
        serde_json::Value::String("deny".to_string()),
    );

    serde_json::to_string(&permissions).unwrap_or_else(|_| r#"{"question":"deny"}"#.to_string())
}

fn setup_compaction_env(auto_compact: bool, env: &ExecutionEnv) -> ExecutionEnv {
    if !auto_compact {
        return env.clone();
    }

    let mut env = env.clone();
    let merged = merge_compaction_config(env.get("OPENCODE_CONFIG_CONTENT").map(String::as_str));
    env.insert("OPENCODE_CONFIG_CONTENT", merged);
    env
}

fn merge_compaction_config(existing_json: Option<&str>) -> String {
    let mut config: Map<String, Value> = existing_json
        .and_then(|value| serde_json::from_str(value.trim()).ok())
        .unwrap_or_default();

    let mut compaction = config
        .remove("compaction")
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    compaction.insert("auto".to_string(), Value::Bool(true));
    config.insert("compaction".to_string(), Value::Object(compaction));

    serde_json::to_string(&config).unwrap_or_else(|_| r#"{"compaction":{"auto":true}}"#.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        fs,
        path::{Path, PathBuf},
        time::{Duration, SystemTime},
    };

    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        OPENCODE_RUNTIME_LAST_USED_MARKER, OpencodeRuntimeLease, opencode_auth_path,
        opencode_auth_provider_ids, opencode_config_paths, opencode_config_provider_ids,
        opencode_runtime_usage, parse_models_command_output, prune_opencode_runtime_dirs,
        touch_opencode_runtime,
    };

    fn create_runtime(
        root: &Path,
        version: &str,
        workspace_key: &str,
        payload_bytes: usize,
        last_used: SystemTime,
    ) -> PathBuf {
        let runtime = root.join(version).join(workspace_key);
        fs::create_dir_all(runtime.join("data").join("opencode"))
            .expect("create runtime directory");
        fs::create_dir_all(runtime.join("state")).expect("create runtime state directory");
        fs::write(
            runtime.join("data").join("opencode").join("payload.bin"),
            vec![b'x'; payload_bytes],
        )
        .expect("write runtime payload");
        touch_opencode_runtime(&runtime).expect("create last-used marker");
        fs::File::options()
            .write(true)
            .open(runtime.join(OPENCODE_RUNTIME_LAST_USED_MARKER))
            .expect("open last-used marker")
            .set_times(fs::FileTimes::new().set_modified(last_used))
            .expect("set last-used marker timestamp");
        runtime
    }

    #[test]
    fn opencode_runtime_pruning_applies_directory_limit_across_versions_by_lru() {
        let temp = TempDir::new().expect("create temp directory");
        let root = temp.path().join("opencode");
        let now = SystemTime::now();
        let oldest = create_runtime(
            &root,
            "opencode-ai-1.16.0",
            "0000000000000001",
            8,
            now + Duration::from_secs(1),
        );
        let newer = create_runtime(
            &root,
            "opencode-ai-1.17.0",
            "0000000000000002",
            8,
            now + Duration::from_secs(2),
        );
        let protected = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "0000000000000003",
            8,
            now + Duration::from_secs(3),
        );

        let result =
            prune_opencode_runtime_dirs(&root, &protected, 2, u64::MAX).expect("prune runtimes");

        assert_eq!(result.removed_dirs, 1);
        assert_eq!(result.remaining_dirs, 2);
        assert!(!oldest.exists(), "oldest runtime should be evicted first");
        assert!(
            !root.join("opencode-ai-1.16.0").exists(),
            "empty old version directory should be removed"
        );
        assert!(newer.exists());
        assert!(protected.exists());
    }

    #[test]
    fn opencode_runtime_pruning_applies_total_byte_limit() {
        let temp = TempDir::new().expect("create temp directory");
        let root = temp.path().join("opencode");
        let now = SystemTime::now();
        let oldest = create_runtime(
            &root,
            "opencode-ai-1.16.0",
            "1000000000000001",
            128,
            now + Duration::from_secs(1),
        );
        let retained = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "1000000000000002",
            64,
            now + Duration::from_secs(2),
        );
        let protected = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "1000000000000003",
            32,
            now + Duration::from_secs(3),
        );
        let oldest_bytes = opencode_runtime_usage(&oldest).0;
        let retained_bytes = opencode_runtime_usage(&retained).0;
        let protected_bytes = opencode_runtime_usage(&protected).0;
        let byte_limit = retained_bytes + protected_bytes;

        let result = prune_opencode_runtime_dirs(&root, &protected, usize::MAX, byte_limit)
            .expect("prune runtimes");

        assert_eq!(result.removed_dirs, 1);
        assert_eq!(result.removed_bytes, oldest_bytes);
        assert_eq!(result.remaining_bytes, byte_limit);
        assert!(!oldest.exists());
        assert!(retained.exists());
        assert!(protected.exists());
    }

    #[test]
    fn opencode_runtime_pruning_never_removes_protected_runtime() {
        let temp = TempDir::new().expect("create temp directory");
        let root = temp.path().join("opencode");
        let now = SystemTime::now();
        let protected = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "2000000000000001",
            128,
            now + Duration::from_secs(1),
        );
        let newer = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "2000000000000002",
            8,
            now + Duration::from_secs(2),
        );

        let result = prune_opencode_runtime_dirs(&root, &protected, 1, 1).expect("prune runtimes");

        assert_eq!(result.removed_dirs, 1);
        assert_eq!(result.remaining_dirs, 1);
        assert!(
            result.remaining_bytes > 1,
            "a protected runtime may keep the soft byte cap exceeded"
        );
        assert!(protected.exists());
        assert!(!newer.exists());
    }

    #[test]
    fn opencode_runtime_pruning_skips_runtime_with_active_lease() {
        let temp = TempDir::new().expect("create temp directory");
        let root = temp.path().join("opencode");
        let now = SystemTime::now();
        let leased = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "2500000000000001",
            8,
            now + Duration::from_secs(1),
        );
        let removable = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "2500000000000002",
            8,
            now + Duration::from_secs(2),
        );
        let protected = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "2500000000000003",
            8,
            now + Duration::from_secs(3),
        );
        let _lease =
            OpencodeRuntimeLease::acquire(&root, &leased).expect("acquire shared runtime lease");

        let result =
            prune_opencode_runtime_dirs(&root, &protected, 2, u64::MAX).expect("prune runtimes");

        assert_eq!(result.removed_dirs, 1);
        assert_eq!(result.remaining_dirs, 2);
        assert!(leased.exists(), "leased runtime must not be deleted");
        assert!(
            !removable.exists(),
            "pruning should continue to the next unlocked runtime"
        );
        assert!(protected.exists());
    }

    #[cfg(unix)]
    #[test]
    fn opencode_runtime_pruning_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("create temp directory");
        let root = temp.path().join("opencode");
        let external = temp.path().join("external");
        let external_nested = external.join("nested");
        let external_candidate = external.join("candidate");
        fs::create_dir_all(&external_nested).expect("create external nested directory");
        fs::create_dir_all(&external_candidate).expect("create external candidate directory");
        let nested_sentinel = external_nested.join("nested-sentinel.txt");
        let candidate_sentinel = external_candidate.join("candidate-sentinel.txt");
        fs::write(&nested_sentinel, vec![b's'; 4096]).expect("write nested sentinel");
        fs::write(&candidate_sentinel, vec![b's'; 4096]).expect("write candidate sentinel");

        let now = SystemTime::now();
        let stale = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "3000000000000001",
            8,
            now + Duration::from_secs(1),
        );
        symlink(&external_nested, stale.join("external-link"))
            .expect("link runtime to external directory");
        let protected = create_runtime(
            &root,
            "opencode-ai-1.17.18",
            "3000000000000002",
            8,
            now + Duration::from_secs(2),
        );
        symlink(
            &external_candidate,
            root.join("opencode-ai-1.17.18").join("3000000000000003"),
        )
        .expect("link candidate runtime to external directory");
        fs::create_dir_all(root.join("manual-layout")).expect("create unknown layout");

        let stale_bytes = opencode_runtime_usage(&stale).0;
        assert!(
            stale_bytes < 4096,
            "external symlink target must not contribute to runtime size"
        );

        let result =
            prune_opencode_runtime_dirs(&root, &protected, 1, u64::MAX).expect("prune runtimes");

        assert_eq!(result.removed_dirs, 1);
        assert!(!stale.exists());
        assert!(protected.exists());
        assert!(
            nested_sentinel.exists(),
            "deleting a runtime must not delete a nested symlink target"
        );
        assert!(
            candidate_sentinel.exists(),
            "a symlink masquerading as a runtime directory must be ignored"
        );
        assert!(
            root.join("manual-layout").exists(),
            "unknown top-level layouts must remain untouched"
        );
    }

    #[test]
    fn opencode_models_command_parser_filters_free_models() {
        let output = r#"
opencode/hy3-free
{
  "id": "hy3-free",
  "providerID": "opencode",
  "cost": {
    "input": 0,
    "output": 0
  }
}
opencode/gpt-5
{
  "id": "gpt-5",
  "providerID": "opencode",
  "cost": {
    "input": 1.25,
    "output": 10
  }
}
opencode/missing-cost-free
{
  "id": "missing-cost-free",
  "providerID": "opencode"
}
"#;

        assert_eq!(
            parse_models_command_output(output, false),
            vec!["opencode/hy3-free"]
        );
    }

    #[test]
    fn opencode_models_command_parser_keeps_configured_provider_models() {
        let output = r#"
CPA/claude-opus-4-7
{
  "id": "claude-opus-4-7",
  "providerID": "CPA",
  "cost": {
    "input": 5,
    "output": 25
  }
}
CPA/gpt-5.3-codex
{
  "id": "gpt-5.3-codex",
  "providerID": "CPA"
}
"#;

        assert_eq!(
            parse_models_command_output(output, true),
            vec!["CPA/claude-opus-4-7", "CPA/gpt-5.3-codex"]
        );
    }

    #[test]
    fn opencode_auth_store_selects_only_valid_credentials() {
        let value = json!({
            "zai-coding-plan": { "type": "api", "key": "zai-key" },
            "github-copilot": {
                "type": "oauth",
                "access": "access-token",
                "refresh": "refresh-token",
                "expires": 1
            },
            "well-known": { "type": "wellknown", "key": "key", "token": "token" },
            "empty": { "type": "api", "key": " " },
            "malformed": { "key": "secret" }
        });

        assert_eq!(
            opencode_auth_provider_ids(&value),
            HashSet::from([
                "github-copilot".to_string(),
                "well-known".to_string(),
                "zai-coding-plan".to_string(),
            ])
        );
    }

    #[test]
    fn opencode_config_provider_ids_reads_custom_provider_keys() {
        let value = json!({
            "provider": {
                "CPA": {
                    "name": "CPA",
                    "options": { "apiKey": "secret" },
                    "models": {
                        "gpt-5.3-codex": {}
                    }
                },
                "zai-coding-plan": {}
            }
        });

        assert_eq!(
            opencode_config_provider_ids(&value),
            HashSet::from(["CPA".to_string(), "zai-coding-plan".to_string()])
        );
    }

    #[test]
    fn opencode_auth_path_matches_xdg_basedir_on_windows_and_unix() {
        assert_eq!(
            opencode_auth_path(None, Some(PathBuf::from("home"))),
            Some(
                PathBuf::from("home")
                    .join(".local")
                    .join("share")
                    .join("opencode")
                    .join("auth.json")
            )
        );
        assert_eq!(
            opencode_auth_path(
                Some(PathBuf::from("custom-data")),
                Some(PathBuf::from("home"))
            ),
            Some(
                PathBuf::from("custom-data")
                    .join("opencode")
                    .join("auth.json")
            )
        );
    }

    #[test]
    fn opencode_config_paths_match_xdg_config_locations() {
        assert_eq!(
            opencode_config_paths(None, Some(PathBuf::from("home")), None),
            vec![
                PathBuf::from("home")
                    .join(".config")
                    .join("opencode")
                    .join("config.json"),
                PathBuf::from("home")
                    .join(".config")
                    .join("opencode")
                    .join("opencode.json"),
                PathBuf::from("home")
                    .join(".config")
                    .join("opencode")
                    .join("opencode.jsonc"),
                PathBuf::from("home")
                    .join(".opencode")
                    .join("opencode.json"),
                PathBuf::from("home")
                    .join(".opencode")
                    .join("opencode.jsonc"),
            ]
        );
        assert_eq!(
            opencode_config_paths(
                Some(PathBuf::from("custom-config")),
                Some(PathBuf::from("home")),
                Some(PathBuf::from("project"))
            )[0],
            PathBuf::from("custom-config")
                .join("opencode")
                .join("config.json")
        );
        assert!(
            opencode_config_paths(
                None,
                Some(PathBuf::from("home")),
                Some(PathBuf::from("project"))
            )
            .iter()
            .any(|path| path.ends_with(PathBuf::from(".opencode").join("opencode.json")))
        );
    }
}
