use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::{msg_store::MsgStore, shell::resolve_executable_path_blocking};

use super::acp::{
    AcpAccessMode, AcpAgentHarness, AcpApprovalMode, AcpApprovalPolicy, AcpAuthSelection,
    AcpCapabilityProbe, AcpClientServicePolicy, AcpExecutionOptions,
    mcp::{AcpMcpPolicy, resolve_isolated_mcp_snapshot},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides},
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, ExecutorPrompt, ExecutorRunCleanup,
        SpawnedChild, StandardCodingAgentExecutor,
    },
    mcp_config::{McpConfig, read_canonical_mcp_config},
};

mod approval;

pub const PI_ACP_VERSION: &str = "0.0.33";
pub const PI_CODING_AGENT_VERSION: &str = "0.83.0";
pub const PI_MCP_ADAPTER_VERSION: &str = "2.18.0";
pub const PI_ACP_PACKAGE: &str = "pi-acp";
pub const PI_CODING_AGENT_PACKAGE: &str = "@earendil-works/pi-coding-agent";
pub const PI_MCP_ADAPTER_PACKAGE: &str = "pi-mcp-adapter";

#[cfg(windows)]
const NPX_COMMAND: &str = "npx.cmd";
#[cfg(not(windows))]
const NPX_COMMAND: &str = "npx";

pub const PI_LAUNCHER_SOURCE: &str = include_str!("pi/launcher.mjs");
const PI_MCP_EXTENSION_SOURCE: &str = include_str!("pi/mcp_extension.mjs");
const PI_APPROVAL_EXTENSION_SOURCE: &str = include_str!("pi/approval_extension.mjs");

const PI_AUTH_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENROUTER_API_KEY",
];

#[derive(Clone)]
struct PiRuntimeSnapshot {
    skill_paths: Vec<PathBuf>,
    mcp_config: serde_json::Value,
}

impl std::fmt::Debug for PiRuntimeSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PiRuntimeSnapshot")
            .field("skill_count", &self.skill_paths.len())
            .field(
                "mcp_server_count",
                &self
                    .mcp_config
                    .get("mcpServers")
                    .and_then(serde_json::Value::as_object)
                    .map_or(0, serde_json::Map::len),
            )
            .finish()
    }
}

impl PiRuntimeSnapshot {
    fn empty() -> Self {
        Self {
            skill_paths: Vec::new(),
            mcp_config: serde_json::json!({
                "mcpServers": {},
                "settings": {"hostConfigDiscovery": "off"}
            }),
        }
    }

    fn has_mcp_servers(&self) -> bool {
        self.mcp_config
            .get("mcpServers")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|servers| !servers.is_empty())
    }
}

struct PiRunFiles {
    launcher_command: PathBuf,
    launcher_module: Option<PathBuf>,
    mcp_extension: PathBuf,
    approval_extension: PathBuf,
    mcp_snapshot: PathBuf,
    armed: bool,
}

impl PiRunFiles {
    fn create(current_dir: &Path, snapshot: &PiRuntimeSnapshot) -> Result<Self, ExecutorError> {
        let directory = current_dir.join(".openteams").join("tmp");
        fs::create_dir_all(&directory).map_err(ExecutorError::Io)?;
        let run_id = uuid::Uuid::new_v4();
        #[cfg(unix)]
        let (launcher_command, launcher_module) =
            (directory.join(format!("pi-{run_id}-launcher.mjs")), None);
        #[cfg(windows)]
        let (launcher_command, launcher_module) = (
            directory.join(format!("pi-{run_id}-launcher.cmd")),
            Some(directory.join(format!("pi-{run_id}-launcher.mjs"))),
        );
        #[cfg(not(any(unix, windows)))]
        let (launcher_command, launcher_module) =
            (directory.join(format!("pi-{run_id}-launcher.mjs")), None);
        let files = Self {
            launcher_command,
            launcher_module,
            mcp_extension: directory.join(format!("pi-{run_id}-mcp-extension.ts")),
            approval_extension: directory.join(format!("pi-{run_id}-approval-extension.mjs")),
            mcp_snapshot: directory.join(format!("pi-{run_id}-mcp.json")),
            armed: true,
        };
        let result = (|| {
            #[cfg(unix)]
            write_executable_file(&files.launcher_command, PI_LAUNCHER_SOURCE.as_bytes())?;
            #[cfg(windows)]
            {
                let launcher_module = files
                    .launcher_module
                    .as_ref()
                    .expect("Windows Pi launcher module path");
                write_private_file(launcher_module, PI_LAUNCHER_SOURCE.as_bytes())?;
                write_private_file(
                    &files.launcher_command,
                    windows_launcher_wrapper(launcher_module).as_bytes(),
                )?;
            }
            #[cfg(not(any(unix, windows)))]
            write_private_file(&files.launcher_command, PI_LAUNCHER_SOURCE.as_bytes())?;
            write_private_file(&files.mcp_extension, PI_MCP_EXTENSION_SOURCE.as_bytes())?;
            write_private_file(
                &files.approval_extension,
                PI_APPROVAL_EXTENSION_SOURCE.as_bytes(),
            )?;
            write_private_file(
                &files.mcp_snapshot,
                &serde_json::to_vec(&snapshot.mcp_config)?,
            )?;
            Ok::<(), ExecutorError>(())
        })();
        if let Err(error) = result {
            drop(files);
            return Err(error);
        }
        Ok(files)
    }

    fn apply_to_env(&self, env: &ExecutionEnv, snapshot: &PiRuntimeSnapshot) -> ExecutionEnv {
        let mut env = env.clone();
        env.insert(
            "PI_ACP_PI_COMMAND",
            self.launcher_command.to_string_lossy().to_string(),
        );
        env.insert(
            "OPENTEAMS_PI_APPROVAL_EXTENSION",
            self.approval_extension.to_string_lossy().to_string(),
        );
        env.insert(
            "OPENTEAMS_PI_MCP_EXTENSION",
            self.mcp_extension.to_string_lossy().to_string(),
        );
        env.insert(
            "OPENTEAMS_PI_MCP_SNAPSHOT",
            self.mcp_snapshot.to_string_lossy().to_string(),
        );
        env.insert(
            "OPENTEAMS_PI_ENABLE_MCP_EXTENSION",
            if snapshot.has_mcp_servers() { "1" } else { "0" },
        );
        env.insert(
            "OPENTEAMS_PI_SKILL_PATHS_JSON",
            serde_json::to_string(&snapshot.skill_paths).unwrap_or_else(|_| "[]".to_string()),
        );
        env
    }

    fn paths(&self) -> Vec<&Path> {
        let mut paths = vec![self.launcher_command.as_path()];
        if let Some(module) = self.launcher_module.as_deref() {
            paths.push(module);
        }
        paths.extend([
            self.mcp_extension.as_path(),
            self.approval_extension.as_path(),
            self.mcp_snapshot.as_path(),
        ]);
        paths
    }

    fn into_cleanup(mut self) -> ExecutorRunCleanup {
        let paths = self.paths().into_iter().map(Path::to_path_buf).collect();
        self.armed = false;
        ExecutorRunCleanup::new(paths)
    }
}

impl Drop for PiRunFiles {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for path in self.paths() {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn write_executable_file(path: &Path, contents: &[u8]) -> Result<(), ExecutorError> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(path)
        .map_err(ExecutorError::Io)?;
    file.write_all(contents).map_err(ExecutorError::Io)?;
    file.sync_all().map_err(ExecutorError::Io)
}

#[cfg(any(windows, test))]
fn windows_launcher_wrapper(launcher_module: &Path) -> String {
    let file_name = launcher_module
        .file_name()
        .and_then(|name| name.to_str())
        .expect("generated Pi launcher module filename is valid UTF-8");
    format!("@echo off\r\nnode \"%~dp0{file_name}\" %*\r\n")
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), ExecutorError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(ExecutorError::Io)?;
    file.write_all(contents).map_err(ExecutorError::Io)?;
    file.sync_all().map_err(ExecutorError::Io)
}

#[derive(Derivative, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct Pi {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Exact provider/model value advertised by pi-acp")]
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
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    runtime_snapshot: Option<Arc<PiRuntimeSnapshot>>,
}

impl Pi {
    pub fn default_command() -> String {
        format!(
            "{NPX_COMMAND} --yes --package {PI_ACP_PACKAGE}@{PI_ACP_VERSION} --package {PI_CODING_AGENT_PACKAGE}@{PI_CODING_AGENT_VERSION} --package {PI_MCP_ADAPTER_PACKAGE}@{PI_MCP_ADAPTER_VERSION} pi-acp"
        )
    }

    pub fn version_command() -> String {
        format!(
            "{NPX_COMMAND} --yes --package {PI_CODING_AGENT_PACKAGE}@{PI_CODING_AGENT_VERSION} pi"
        )
    }

    #[cfg(feature = "qa-mode")]
    pub fn from_qa_environment() -> Self {
        let mut pi = Self::default();
        if let Some(npx_path) = std::env::var_os("OPENTEAMS_PI_QA_NPX_PATH") {
            pi.cmd.base_command_override = Some(format!(
                "{} --yes --package {PI_ACP_PACKAGE}@{PI_ACP_VERSION} {PI_ACP_PACKAGE}",
                npx_path.to_string_lossy()
            ));
        }
        pi
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        apply_overrides(CommandBuilder::new(Self::default_command()), &self.cmd)
    }

    pub async fn freeze_runtime_snapshot(
        &mut self,
        skill_paths: Vec<PathBuf>,
    ) -> Result<(), ExecutorError> {
        let canonical = match self.default_mcp_config_path() {
            Some(path) => read_canonical_mcp_config(&path, &McpConfig::canonical_acp()).await?,
            None => serde_json::json!({ "mcpServers": {} }),
        };
        validate_member_mcp_allowlist(&canonical, &self.acp_mcp_policy)?;
        let mcp_config = resolve_isolated_mcp_snapshot(&canonical, &self.acp_mcp_policy)?;
        self.runtime_snapshot = Some(Arc::new(PiRuntimeSnapshot {
            skill_paths,
            mcp_config,
        }));
        Ok(())
    }

    fn runtime_snapshot(&self) -> PiRuntimeSnapshot {
        self.runtime_snapshot
            .as_deref()
            .cloned()
            .unwrap_or_else(PiRuntimeSnapshot::empty)
    }

    async fn acp_harness(&self) -> Result<AcpAgentHarness, ExecutorError> {
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
            .with_client_services(AcpClientServicePolicy {
                read_text_file: true,
                write_text_file: true,
                terminal: true,
                full_access,
                ..AcpClientServicePolicy::default()
            });
        if let Some(AcpAuthSelection::MethodId { method_id }) = options.auth {
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
            .filter(|value| !value.trim().is_empty())
            .filter(|_| !has_model_override)
        {
            harness = harness.with_model(model);
        }
        for selection in config_overrides {
            harness = harness.with_config_override(selection);
        }
        Ok(harness.with_mcp_servers(Vec::new()))
    }
}

fn validate_member_mcp_allowlist(
    canonical: &serde_json::Value,
    policy: &AcpMcpPolicy,
) -> Result<(), ExecutorError> {
    let Some(allowed) = policy.allowed_server_names.as_ref() else {
        return Ok(());
    };
    let configured = canonical
        .get("mcpServers")
        .and_then(serde_json::Value::as_object);
    if let Some(missing) = allowed
        .iter()
        .find(|name| configured.is_none_or(|servers| !servers.contains_key(*name)))
    {
        return Err(ExecutorError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid Pi member configuration: MCP server `{missing}` is not configured"),
        )));
    }
    Ok(())
}

fn map_pi_follow_up_error(error: ExecutorError) -> ExecutorError {
    if matches!(&error, ExecutorError::Io(io_error) if {
        let message = io_error.to_string();
        message.contains("Unknown sessionId") || message.contains("Unknown session ID")
    }) {
        ExecutorError::FollowUpNotSupported(format!(
            "Pi ACP could not reuse the requested session: {error}"
        ))
    } else {
        error
    }
}

fn prerequisites_available_on_path(mut resolve: impl FnMut(&str) -> bool) -> bool {
    resolve("node")
}

#[async_trait]
impl StandardCodingAgentExecutor for Pi {
    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        let auth_file_exists = dirs::home_dir()
            .map(|home| home.join(".pi").join("agent").join("auth.json"))
            .is_some_and(|path| path.is_file());
        self.authentication_detected(&env, PI_AUTH_ENV_VARS, auth_file_exists)
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
        let snapshot = self.runtime_snapshot();
        let files = PiRunFiles::create(current_dir, &snapshot)?;
        let runtime_env = files.apply_to_env(env, &snapshot);
        let result = super::acp::runtime::probe_acp_command(
            self.build_command_builder()?.build_initial()?,
            current_dir,
            &runtime_env,
            &self.cmd,
            auth_method_id
                .map(str::to_string)
                .or(configured_auth_method_id),
        )
        .await?;
        Ok(Some(result))
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let snapshot = self.runtime_snapshot();
        let files = PiRunFiles::create(current_dir, &snapshot)?;
        let runtime_env = files.apply_to_env(env, &snapshot);
        let mut spawned = self
            .acp_harness()
            .await?
            .spawn_with_command(
                current_dir,
                self.append_prompt.combine_prompt(prompt),
                self.build_command_builder()?.build_initial()?,
                &runtime_env,
                &self.cmd,
                approval::wrap(self.approvals.clone()),
            )
            .await?;
        spawned.cleanup = Some(files.into_cleanup());
        Ok(spawned)
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let snapshot = self.runtime_snapshot();
        let files = PiRunFiles::create(current_dir, &snapshot)?;
        let runtime_env = files.apply_to_env(env, &snapshot);
        let mut spawned = self
            .acp_harness()
            .await?
            .spawn_follow_up_with_command(
                current_dir,
                self.append_prompt.combine_prompt(prompt),
                session_id,
                self.build_command_builder()?.build_follow_up(&[])?,
                &runtime_env,
                &self.cmd,
                approval::wrap(self.approvals.clone()),
            )
            .await
            .map_err(map_pi_follow_up_error)?;
        spawned.cleanup = Some(files.into_cleanup());
        Ok(spawned)
    }

    async fn spawn_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let mut prompt = prompt.clone();
        prompt.text = self.append_prompt.combine_prompt(&prompt.text);
        let snapshot = self.runtime_snapshot();
        let files = PiRunFiles::create(current_dir, &snapshot)?;
        let runtime_env = files.apply_to_env(env, &snapshot);
        let mut spawned = self
            .acp_harness()
            .await?
            .spawn_structured_with_command(
                current_dir,
                prompt,
                self.build_command_builder()?.build_initial()?,
                &runtime_env,
                &self.cmd,
                approval::wrap(self.approvals.clone()),
            )
            .await?;
        spawned.cleanup = Some(files.into_cleanup());
        Ok(spawned)
    }

    async fn spawn_follow_up_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let mut prompt = prompt.clone();
        prompt.text = self.append_prompt.combine_prompt(&prompt.text);
        let snapshot = self.runtime_snapshot();
        let files = PiRunFiles::create(current_dir, &snapshot)?;
        let runtime_env = files.apply_to_env(env, &snapshot);
        let mut spawned = self
            .acp_harness()
            .await?
            .spawn_follow_up_structured_with_command(
                current_dir,
                prompt,
                session_id,
                self.build_command_builder()?.build_follow_up(&[])?,
                &runtime_env,
                &self.cmd,
                approval::wrap(self.approvals.clone()),
            )
            .await
            .map_err(map_pi_follow_up_error)?;
        spawned.cleanup = Some(files.into_cleanup());
        Ok(spawned)
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        super::acp::normalize_logs(msg_store, worktree_path);
    }

    fn default_runtime_config_path(&self) -> Option<PathBuf> {
        pi_coding_agent_dir().map(|directory| directory.join("settings.json"))
    }

    fn default_mcp_config_path(&self) -> Option<PathBuf> {
        dirs::home_dir().map(|home| home.join(".pi").join("agent").join("mcp.json"))
    }

    fn native_skill_discovery_roots(&self) -> Vec<PathBuf> {
        dirs::home_dir()
            .map(|home| {
                vec![
                    home.join(".agents").join("skills"),
                    home.join(".pi").join("agent").join("skills"),
                ]
            })
            .unwrap_or_default()
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if prerequisites_available_on_path(|program| {
            resolve_executable_path_blocking(program).is_some()
        }) {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

fn pi_coding_agent_dir() -> Option<PathBuf> {
    pi_coding_agent_dir_from(
        std::env::var_os("PI_CODING_AGENT_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        dirs::home_dir(),
    )
}

fn pi_coding_agent_dir_from(
    configured_directory: Option<PathBuf>,
    user_home: Option<PathBuf>,
) -> Option<PathBuf> {
    configured_directory.or_else(|| user_home.map(|home| home.join(".pi").join("agent")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_agent_directory_prefers_configured_directory_and_has_default() {
        let user_home = PathBuf::from("/users/tester");
        let configured_directory = PathBuf::from("/configured/pi-agent");

        assert_eq!(
            pi_coding_agent_dir_from(Some(configured_directory.clone()), Some(user_home.clone())),
            Some(configured_directory)
        );
        assert_eq!(
            pi_coding_agent_dir_from(None, Some(user_home.clone())),
            Some(user_home.join(".pi").join("agent"))
        );
    }

    #[cfg(unix)]
    async fn wait_for_pi_process_handshake(
        child: &mut command_group::AsyncGroupChild,
        pid_file: &Path,
        timeout: std::time::Duration,
    ) -> Result<Vec<String>, String> {
        use tokio::{io::AsyncReadExt, time::Instant};

        async fn stderr_byte_count(child: &mut command_group::AsyncGroupChild) -> usize {
            let Some(mut stderr) = child.inner().stderr.take() else {
                return 0;
            };
            let mut captured = Vec::new();
            let _ = stderr.read_to_end(&mut captured).await;
            captured.len()
        }

        let deadline = Instant::now() + timeout;
        let mut observed_pid_file_bytes = 0;
        loop {
            match child.inner().try_wait() {
                Ok(Some(status)) => {
                    let stderr_bytes = stderr_byte_count(child).await;
                    return Err(format!(
                        "launcher exited before Pi PID handshake (exit_code={:?}, stderr_bytes={stderr_bytes})",
                        status.code()
                    ));
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(format!(
                        "launcher status check failed before Pi PID handshake (kind={:?})",
                        error.kind()
                    ));
                }
            }

            if let Ok(contents) = fs::read_to_string(pid_file) {
                observed_pid_file_bytes = contents.len();
                let pids = contents
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                if pids.len() == 2 && pids.iter().all(|pid| pid.parse::<u32>().is_ok()) {
                    return Ok(pids);
                }
            }

            if Instant::now() >= deadline {
                let _ = workspace_utils::process::kill_process_group(child).await;
                let stderr_bytes = stderr_byte_count(child).await;
                return Err(format!(
                    "timed out waiting for Pi PID handshake (timeout_ms={}, pid_file_bytes={observed_pid_file_bytes}, stderr_bytes={stderr_bytes})",
                    timeout.as_millis()
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(unix)]
    #[allow(dead_code)]
    const FAKE_PI_RPC_SOURCE: &str = r#"#!/usr/bin/env node
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { spawn } from "node:child_process";
import readline from "node:readline";

const sessionFile = process.env.OPENTEAMS_FAKE_PI_SESSION_FILE;
mkdirSync(dirname(sessionFile), { recursive: true });
writeFileSync(sessionFile, "");
let child;
if (process.env.OPENTEAMS_FAKE_PI_CHILD_PID_FILE) {
  child = spawn("pi-mcp-adapter", [], { stdio: "ignore" });
  writeFileSync(process.env.OPENTEAMS_FAKE_PI_CHILD_PID_FILE,
    JSON.stringify({ pi: process.pid, launcher: process.ppid, mcp: child.pid }));
}
const send = (value) => process.stdout.write(`${JSON.stringify(value)}\n`);
const respond = (request, data = {}) => send({ type: "response", id: request.id, success: true, data });
const state = {
  sessionId: "pi-offline-session",
  sessionFile,
  model: { provider: "offline-provider", id: "offline-model" },
  thinkingLevel: "off"
};
const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const request = JSON.parse(line);
  switch (request.type) {
    case "get_state":
      respond(request, state);
      break;
    case "get_available_models":
      respond(request, { models: [{ provider: "offline-provider", id: "offline-model", name: "Offline Model" }] });
      break;
    case "get_commands":
      respond(request, { commands: [] });
      break;
    case "get_messages":
      respond(request, { messages: [] });
      break;
    case "prompt":
      appendFileSync(process.env.OPENTEAMS_FAKE_PI_PROMPTS, `${request.message}\n`);
      respond(request);
      if (process.env.OPENTEAMS_FAKE_PI_HANG !== "1") {
        send({ type: "agent_start" });
        send({ type: "message_update", assistantMessageEvent: { type: "text_delta", delta: `echo:${request.message}` } });
        send({ type: "agent_end" });
        send({ type: "agent_settled" });
      }
      break;
    case "abort":
      respond(request);
      send({ type: "agent_settled" });
      break;
    default:
      respond(request);
  }
});
rl.on("close", () => {
  if (child) child.kill("SIGTERM");
  process.exit(0);
});
"#;

    fn pi() -> Pi {
        Pi::default()
    }

    const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pi_acp");

    fn read_fixture(name: &str) -> String {
        fs::read_to_string(format!("{FIXTURE_DIR}/{name}"))
            .unwrap_or_else(|_| panic!("read fixture {name}"))
    }

    #[cfg(unix)]
    fn install_fake_pi_npx_environment(
        root: &Path,
        executable: bool,
    ) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let fake_bin = root.join("bin");
        let node_modules = root.join("node_modules");
        let bin = node_modules.join(".bin");
        let pi_package = node_modules.join(PI_CODING_AGENT_PACKAGE);
        let mcp_package = node_modules.join(PI_MCP_ADAPTER_PACKAGE);
        fs::create_dir_all(&fake_bin).expect("fake bin");
        fs::create_dir_all(&bin).expect("NPX bin");
        fs::create_dir_all(&pi_package).expect("Pi package");
        fs::create_dir_all(&mcp_package).expect("MCP package");
        fs::write(
            pi_package.join("package.json"),
            format!(r#"{{"version":"{PI_CODING_AGENT_VERSION}"}}"#),
        )
        .expect("Pi metadata");
        fs::write(
            mcp_package.join("package.json"),
            format!(r#"{{"version":"{PI_MCP_ADAPTER_VERSION}"}}"#),
        )
        .expect("MCP metadata");
        fs::write(mcp_package.join("index.ts"), "export default () => {};").expect("MCP entry");

        let mode = if executable { 0o755 } else { 0o644 };

        let fake_npx = fake_bin.join("npx");
        fs::write(&fake_npx, read_fixture("fake_npx.sh")).expect("fake npx");
        fs::set_permissions(&fake_npx, fs::Permissions::from_mode(0o755)).expect("fake npx mode");

        let fake_pi_acp = fake_bin.join("pi-acp");
        fs::write(&fake_pi_acp, read_fixture("fake_pi_acp.mjs")).expect("fake pi-acp");
        fs::set_permissions(&fake_pi_acp, fs::Permissions::from_mode(mode))
            .expect("fake pi-acp mode");

        let fake_pi = bin.join("pi");
        fs::write(&fake_pi, read_fixture("fake_pi.mjs")).expect("fake Pi RPC");
        fs::set_permissions(&fake_pi, fs::Permissions::from_mode(mode)).expect("fake Pi mode");
        let fake_mcp = bin.join("pi-mcp-adapter");
        fs::write(&fake_mcp, read_fixture("fake_pi_mcp_adapter.mjs")).expect("fake MCP adapter");
        fs::set_permissions(&fake_mcp, fs::Permissions::from_mode(0o755))
            .expect("fake MCP adapter mode");
        (
            fake_bin,
            bin,
            root.join("prompts.txt"),
            root.join("sessions/session.jsonl"),
        )
    }

    #[cfg(unix)]
    fn offline_pi_executor(root: &Path, executable: bool) -> (Pi, ExecutionEnv, PathBuf) {
        let (fake_bin, bin, prompts, session_file) =
            install_fake_pi_npx_environment(root, executable);
        let mut pi = Pi::default();
        let npx_path = fake_bin.join("npx");
        pi.cmd.base_command_override = Some(format!(
            "{} --yes --package pi-acp@0.0.33 pi-acp",
            npx_path.display()
        ));
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert(
            "PATH",
            format!(
                "{}:{}:{}",
                fake_bin.display(),
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        env.insert("HOME", root.join("home").to_string_lossy().to_string());
        env.insert("NO_UPDATE_NOTIFIER", "1");
        env.insert(
            "OPENTEAMS_FAKE_PI_SESSION_FILE",
            session_file.to_string_lossy().to_string(),
        );
        env.insert(
            "OPENTEAMS_FAKE_PI_PROMPTS",
            prompts.to_string_lossy().to_string(),
        );
        env.insert(
            "OPENTEAMS_FAKE_PI_CHILD_PID_FILE",
            root.join("pids.json").to_string_lossy().to_string(),
        );
        env.insert(
            "OPENTEAMS_FAKE_PI_PERMISSION_LOG",
            root.join("permissions.jsonl").to_string_lossy().to_string(),
        );
        env.insert(
            "OPENTEAMS_FAKE_PI_PROTOCOL_LOG",
            root.join("protocol.jsonl").to_string_lossy().to_string(),
        );
        (pi, env, prompts)
    }

    #[test]
    fn exact_versions_are_centralized_and_command_is_fully_pinned() {
        assert_eq!(PI_ACP_VERSION, "0.0.33");
        assert_eq!(PI_CODING_AGENT_VERSION, "0.83.0");
        assert_eq!(PI_MCP_ADAPTER_VERSION, "2.18.0");
        let command = pi()
            .build_command_builder()
            .unwrap()
            .build_initial()
            .unwrap();
        assert_eq!(
            command.redacted_display(),
            format!(
                "{NPX_COMMAND} --yes --package pi-acp@0.0.33 --package @earendil-works/pi-coding-agent@0.83.0 --package pi-mcp-adapter@2.18.0 pi-acp"
            )
        );
        let (program, args) = command.into_parts_for_test();
        assert_eq!(program, NPX_COMMAND);
        assert_eq!(
            args,
            [
                "--yes",
                "--package",
                "pi-acp@0.0.33",
                "--package",
                "@earendil-works/pi-coding-agent@0.83.0",
                "--package",
                "pi-mcp-adapter@2.18.0",
                "pi-acp",
            ]
        );
        assert!(!Pi::default_command().contains("latest"));
        assert_eq!(
            Pi::version_command(),
            format!("{NPX_COMMAND} --yes --package @earendil-works/pi-coding-agent@0.83.0 pi")
        );
        assert!(!Pi::version_command().contains("latest"));
    }

    #[test]
    fn embedded_runtime_files_are_present_and_never_embed_snapshot_secrets() {
        assert!(super::PI_LAUNCHER_SOURCE.starts_with("#!/usr/bin/env node\n"));
        assert!(super::PI_LAUNCHER_SOURCE.contains("--no-skills"));
        assert!(super::PI_LAUNCHER_SOURCE.contains("--no-extensions"));
        assert!(super::PI_LAUNCHER_SOURCE.contains("--skill"));
        assert!(super::PI_LAUNCHER_SOURCE.contains("process.ppid"));
        assert!(super::PI_LAUNCHER_SOURCE.contains("terminateOrphanedTree"));
        assert!(super::PI_MCP_EXTENSION_SOURCE.contains("createMcpAdapter"));
        assert!(super::PI_APPROVAL_EXTENSION_SOURCE.contains("tool_call"));
        for source in [
            super::PI_LAUNCHER_SOURCE,
            super::PI_MCP_EXTENSION_SOURCE,
            super::PI_APPROVAL_EXTENSION_SOURCE,
        ] {
            assert!(!source.contains("openteams-pi-secret-never-log"));
        }
    }

    #[test]
    fn windows_launcher_wrapper_is_directly_spawnable_and_forwards_all_arguments() {
        assert_eq!(
            windows_launcher_wrapper(Path::new("C:/runtime/pi-run-launcher.mjs")),
            "@echo off\r\nnode \"%~dp0pi-run-launcher.mjs\" %*\r\n"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_pinned_command_uses_a_spawnable_npx_cmd_shim() {
        let temp = tempfile::tempdir().expect("temporary NPX shim");
        let shim = temp.path().join(NPX_COMMAND);
        fs::write(&shim, "@echo off\r\nexit /b 0\r\n").expect("fake npx.cmd");

        let status = std::process::Command::new(shim)
            .status()
            .expect("spawn npx.cmd directly");

        assert!(status.success());
    }

    #[cfg(windows)]
    #[test]
    fn windows_launcher_executes_pi_javascript_entry_without_cmd_shim() {
        use std::process::Command;

        let node = resolve_executable_path_blocking("node.exe").expect("Node.js executable");
        let temp = tempfile::tempdir().expect("temporary NPX environment");
        let node_modules = temp.path().join("node_modules");
        let bin = node_modules.join(".bin");
        let pi_package = node_modules.join(PI_CODING_AGENT_PACKAGE);
        let mcp_package = node_modules.join(PI_MCP_ADAPTER_PACKAGE);
        fs::create_dir_all(&bin).expect("NPX bin");
        fs::create_dir_all(&pi_package).expect("Pi package");
        fs::create_dir_all(&mcp_package).expect("MCP package");
        fs::write(
            pi_package.join("package.json"),
            format!(r#"{{"version":"{PI_CODING_AGENT_VERSION}","bin":{{"pi":"pi.mjs"}}}}"#),
        )
        .expect("Pi package metadata");
        fs::write(
            mcp_package.join("package.json"),
            format!(r#"{{"version":"{PI_MCP_ADAPTER_VERSION}"}}"#),
        )
        .expect("MCP package metadata");
        fs::write(mcp_package.join("index.ts"), "export default () => {};").expect("MCP entry");
        let args_output = temp.path().join("args.json");
        let args_output_json = serde_json::to_string(&args_output).expect("serialize args path");
        fs::write(
            pi_package.join("pi.mjs"),
            format!(
                "import {{ writeFileSync }} from \"node:fs\";\nwriteFileSync({args_output_json}, JSON.stringify(process.argv.slice(2)));\n"
            ),
        )
        .expect("fake Pi entry");
        let launcher = temp.path().join("launcher.mjs");
        let approval = temp.path().join("approval_extension.mjs");
        fs::write(&launcher, PI_LAUNCHER_SOURCE).expect("launcher");
        fs::write(&approval, "export default () => {};").expect("approval extension");
        let path = std::env::join_paths(std::iter::once(bin.clone()).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .expect("test PATH");

        let status = Command::new(node)
            .arg(&launcher)
            .args(["--mode", "rpc"])
            .env("PATH", path)
            .env("OPENTEAMS_PI_SKILL_PATHS_JSON", "[]")
            .env("OPENTEAMS_PI_APPROVAL_EXTENSION", &approval)
            .env("OPENTEAMS_PI_ENABLE_MCP_EXTENSION", "0")
            .status()
            .expect("run launcher");

        assert!(status.success());
        let args: Vec<String> =
            serde_json::from_slice(&fs::read(args_output).expect("captured Pi arguments"))
                .expect("Pi argument JSON");
        assert_eq!(
            args,
            [
                "--mode",
                "rpc",
                "--no-skills",
                "--no-extensions",
                "--extension",
                approval.to_str().expect("approval path"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn pi_acp_direct_spawn_contract_executes_launcher_and_preserves_arguments() {
        use std::{fs, os::unix::fs::PermissionsExt, process::Command};

        assert!(resolve_executable_path_blocking("node").is_some());
        let temp = tempfile::tempdir().expect("temporary NPX environment");
        let node_modules = temp.path().join("node_modules");
        let bin = node_modules.join(".bin");
        let pi_package = node_modules.join(PI_CODING_AGENT_PACKAGE);
        let mcp_package = node_modules.join(PI_MCP_ADAPTER_PACKAGE);
        fs::create_dir_all(&bin).expect("NPX bin");
        fs::create_dir_all(&pi_package).expect("Pi package");
        fs::create_dir_all(&mcp_package).expect("MCP package");
        fs::write(
            pi_package.join("package.json"),
            format!(r#"{{"version":"{PI_CODING_AGENT_VERSION}"}}"#),
        )
        .expect("Pi package metadata");
        fs::write(
            mcp_package.join("package.json"),
            format!(r#"{{"version":"{PI_MCP_ADAPTER_VERSION}"}}"#),
        )
        .expect("MCP package metadata");
        fs::write(mcp_package.join("index.ts"), "export default () => {};").expect("MCP entry");
        let args_output = temp.path().join("args.json");
        let fake_pi = bin.join("pi");
        fs::write(
            &fake_pi,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                args_output.display()
            ),
        )
        .expect("fake Pi");
        fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o700)).expect("Pi mode");
        let launcher = temp.path().join("launcher.mjs");
        write_executable_file(&launcher, super::PI_LAUNCHER_SOURCE.as_bytes()).expect("launcher");
        let approval = temp.path().join("approval_extension.mjs");
        let mcp = temp.path().join("mcp_extension.mjs");
        fs::write(&approval, "export default () => {};").expect("approval extension");
        fs::write(&mcp, "export default () => {};").expect("MCP extension");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let status = Command::new(&launcher)
            .args(["--mode", "rpc", "--no-themes"])
            .env("PATH", path)
            .env(
                "OPENTEAMS_PI_SKILL_PATHS_JSON",
                r#"["/registry/alpha/SKILL.md","/registry/beta/SKILL.md"]"#,
            )
            .env("OPENTEAMS_PI_APPROVAL_EXTENSION", &approval)
            .env("OPENTEAMS_PI_MCP_EXTENSION", &mcp)
            .env("OPENTEAMS_PI_ENABLE_MCP_EXTENSION", "1")
            .status()
            .expect("run launcher");
        assert!(status.success());
        let args = fs::read_to_string(args_output).expect("captured arguments");
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "--mode",
                "rpc",
                "--no-themes",
                "--no-skills",
                "--skill",
                "/registry/alpha/SKILL.md",
                "--skill",
                "/registry/beta/SKILL.md",
                "--no-extensions",
                "--extension",
                approval.to_str().expect("approval path"),
                "--extension",
                mcp.to_str().expect("MCP path"),
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fixed_pi_acp_offline_runs_new_prompt_follow_up_cancel_and_startup_failure() {
        use std::time::Duration;

        use tokio::io::{AsyncBufReadExt, BufReader};

        use super::super::acp::{AcpEvent, events::AcpRuntimeEvent};

        async fn finish_turn(
            mut spawned: SpawnedChild,
        ) -> (Vec<AcpEvent>, crate::executors::ExecutorExitResult) {
            let stdout = spawned
                .child
                .inner()
                .stdout
                .take()
                .expect("ACP runtime stdout");
            let mut lines = BufReader::new(stdout).lines();
            let mut events = Vec::new();
            loop {
                let line = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
                    .await
                    .expect("ACP output timeout")
                    .expect("ACP output read");
                let Some(line) = line else {
                    break;
                };
                let event = serde_json::from_str::<AcpRuntimeEvent>(&line)
                    .expect("typed ACP runtime event")
                    .payload;
                let done = matches!(event, AcpEvent::Done(_));
                events.push(event);
                if done {
                    break;
                }
            }
            let exit = spawned.exit_signal.take().expect("ACP exit signal");
            let result = tokio::time::timeout(Duration::from_secs(15), exit)
                .await
                .expect("ACP exit timeout")
                .expect("ACP exit result");
            workspace_utils::process::kill_process_group(&mut spawned.child)
                .await
                .expect("reap fixed pi-acp process tree");
            (events, result)
        }

        async fn assert_recorded_tree_terminated(path: &Path) {
            let pids = {
                let mut bytes = None;
                for _ in 0..100 {
                    if let Ok(data) = fs::read(path) {
                        bytes = Some(data);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let bytes = bytes.unwrap_or_else(|| {
                    panic!("recorded Pi topology not found: {}", path.display())
                });
                serde_json::from_slice::<serde_json::Value>(&bytes).expect("Pi topology JSON")
            };
            let pids = ["launcher", "pi", "mcp"].map(|name| {
                pids[name]
                    .as_u64()
                    .expect("recorded process ID")
                    .to_string()
            });
            for _ in 0..100 {
                let any_alive = pids.iter().any(|pid| {
                    std::process::Command::new("/bin/kill")
                        .args(["-0", pid])
                        .status()
                        .is_ok_and(|status| status.success())
                });
                if !any_alive {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("fixed NPX/launcher/Pi/MCP process tree survived cleanup: {pids:?}");
        }

        let temp = tempfile::tempdir().expect("offline pi-acp workspace");
        let (pi, env, prompts) = offline_pi_executor(temp.path(), true);

        let first = pi
            .spawn(temp.path(), "first", &env)
            .await
            .expect("fixed pi-acp session/new and prompt");
        let runtime_dir = temp.path().join(".openteams/tmp");
        let first_runtime_files = fs::read_dir(&runtime_dir)
            .expect("first runtime files")
            .map(|entry| entry.expect("runtime entry").path())
            .collect::<Vec<_>>();
        assert_eq!(first_runtime_files.len(), 4);
        assert!(first_runtime_files.iter().all(|path| path.exists()));
        let (first_events, first_exit) = finish_turn(first).await;
        assert!(matches!(
            first_exit,
            crate::executors::ExecutorExitResult::Success
        ));
        let session_id = first_events
            .iter()
            .find_map(|event| match event {
                AcpEvent::SessionStart(id) => Some(id.clone()),
                _ => None,
            })
            .expect("Pi ACP session id");
        assert_eq!(session_id, "pi-offline-session");
        assert!(first_events.iter().any(|event| {
            matches!(event, AcpEvent::Message(message) if format!("{message:?}").contains("echo:first"))
        }));
        assert!(first_runtime_files.iter().all(|path| !path.exists()));
        assert_recorded_tree_terminated(&temp.path().join("pids.json")).await;

        let follow_up = pi
            .spawn_follow_up(temp.path(), "second", &session_id, None, &env)
            .await
            .expect("fixed pi-acp session/load follow-up");
        let (follow_up_events, follow_up_exit) = finish_turn(follow_up).await;
        assert!(matches!(
            follow_up_exit,
            crate::executors::ExecutorExitResult::Success
        ));
        assert!(follow_up_events.iter().any(|event| {
            matches!(event, AcpEvent::Message(message) if format!("{message:?}").contains("echo:second"))
        }));
        assert_recorded_tree_terminated(&temp.path().join("pids.json")).await;
        assert_eq!(
            fs::read_to_string(&prompts)
                .expect("captured Pi prompts")
                .lines()
                .collect::<Vec<_>>(),
            ["first", "second"]
        );

        let mut error_env = env.clone();
        error_env.insert("OPENTEAMS_FAKE_PI_ERROR", "1");
        let failed = pi
            .spawn(temp.path(), "provider-error", &error_env)
            .await
            .expect("fixed pi-acp provider error prompt");
        let (failed_events, failed_exit) = finish_turn(failed).await;
        assert!(
            matches!(failed_exit, crate::executors::ExecutorExitResult::Failure),
            "unexpected Pi provider error result: {failed_exit:?}, events: {failed_events:?}"
        );
        assert!(failed_events.iter().any(|event| {
            matches!(event, AcpEvent::Error(message) if message.contains("Pi provider connection failed"))
        }));
        assert!(
            !failed_events
                .iter()
                .any(|event| matches!(event, AcpEvent::Done(_)))
        );
        assert_recorded_tree_terminated(&temp.path().join("pids.json")).await;

        let mut cancel_env = env.clone();
        cancel_env.insert("OPENTEAMS_FAKE_PI_HANG", "1");
        let mut cancelled = pi
            .spawn(temp.path(), "cancel-me", &cancel_env)
            .await
            .expect("fixed pi-acp cancellable prompt");
        cancelled
            .cancel
            .as_ref()
            .expect("ACP cancellation token")
            .cancel();
        let cancelled_exit = cancelled.exit_signal.take().expect("cancel exit signal");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(15), cancelled_exit)
                .await
                .expect("cancel timeout")
                .expect("cancel result"),
            crate::executors::ExecutorExitResult::Success
        ));
        workspace_utils::process::kill_process_group(&mut cancelled.child)
            .await
            .expect("reap cancelled pi-acp tree");
        drop(cancelled);
        assert_recorded_tree_terminated(&temp.path().join("pids.json")).await;
        assert!(
            fs::read_dir(&runtime_dir)
                .expect("runtime directory after cancellation")
                .next()
                .is_none()
        );

        let failed_temp = tempfile::tempdir().expect("offline startup failure workspace");
        let (failed_pi, failed_env, _) = offline_pi_executor(failed_temp.path(), false);
        let error = tokio::time::timeout(
            Duration::from_secs(15),
            failed_pi.spawn(failed_temp.path(), "must fail", &failed_env),
        )
        .await
        .expect("startup failure timeout")
        .expect_err("non-executable Pi must fail session/new");
        assert!(error.to_string().contains("ACP startup failed"));
        assert!(
            fs::read_dir(failed_temp.path().join(".openteams/tmp"))
                .expect("failed runtime directory")
                .next()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pi_process_handshake_is_bounded_and_redacts_early_exit_stderr() {
        use std::{process::Stdio, time::Duration};

        use command_group::AsyncCommandGroup;
        use tokio::process::Command;

        let temp = tempfile::tempdir().expect("handshake diagnostics");
        let secret = "pi-handshake-secret-never-log";
        let mut early_exit = Command::new("/bin/sh");
        early_exit
            .args(["-c", "printf '%s' \"$HANDSHAKE_SECRET\" >&2; exit 17"])
            .env("HANDSHAKE_SECRET", secret)
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = early_exit.group_spawn().expect("spawn early exit");
        let error = wait_for_pi_process_handshake(
            &mut child,
            &temp.path().join("missing-pids"),
            Duration::from_secs(2),
        )
        .await
        .expect_err("early exit must fail the handshake");
        assert!(error.contains("exit_code=Some(17)"));
        assert!(error.contains("stderr_bytes="));
        assert!(!error.contains(secret));

        let mut stalled = Command::new("/bin/sh");
        stalled
            .args(["-c", "sleep 60"])
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = stalled.group_spawn().expect("spawn stalled launcher");
        let error = wait_for_pi_process_handshake(
            &mut child,
            &temp.path().join("still-missing-pids"),
            Duration::from_millis(100),
        )
        .await
        .expect_err("stalled startup must time out");
        assert!(error.contains("timed out"));
        assert!(error.contains("timeout_ms=100"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_group_cancellation_terminates_launcher_pi_and_pi_children() {
        use std::{fs, os::unix::fs::PermissionsExt, process::Stdio};

        use command_group::AsyncCommandGroup;
        use tokio::{
            process::Command,
            time::{Duration, sleep},
        };

        assert!(resolve_executable_path_blocking("node").is_some());
        let temp = tempfile::tempdir().expect("temporary NPX environment");
        let node_modules = temp.path().join("node_modules");
        let bin = node_modules.join(".bin");
        let pi_package = node_modules.join(PI_CODING_AGENT_PACKAGE);
        let mcp_package = node_modules.join(PI_MCP_ADAPTER_PACKAGE);
        fs::create_dir_all(&bin).expect("NPX bin");
        fs::create_dir_all(&pi_package).expect("Pi package");
        fs::create_dir_all(&mcp_package).expect("MCP package");
        fs::write(
            pi_package.join("package.json"),
            format!(r#"{{"version":"{PI_CODING_AGENT_VERSION}"}}"#),
        )
        .expect("Pi metadata");
        fs::write(
            mcp_package.join("package.json"),
            format!(r#"{{"version":"{PI_MCP_ADAPTER_VERSION}"}}"#),
        )
        .expect("MCP metadata");
        fs::write(mcp_package.join("index.ts"), "export default () => {};").expect("MCP entry");
        let pid_file = temp.path().join("pids");
        let fake_pi = bin.join("pi");
        fs::write(
            &fake_pi,
            format!(
                "#!/bin/sh\nsleep 60 &\nprintf '%s\\n%s\\n' \"$$\" \"$!\" > '{}'\nwait\n",
                pid_file.display()
            ),
        )
        .expect("fake Pi");
        fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o700)).expect("Pi mode");
        let launcher = temp.path().join("launcher.mjs");
        let approval = temp.path().join("approval_extension.mjs");
        let mcp = temp.path().join("mcp_extension.ts");
        write_executable_file(&launcher, PI_LAUNCHER_SOURCE.as_bytes()).expect("launcher");
        fs::write(&approval, "export default () => {};").expect("approval");
        fs::write(&mcp, "export default () => {};").expect("MCP");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new(&launcher);
        command
            .env("PATH", path)
            .env("OPENTEAMS_PI_SKILL_PATHS_JSON", "[]")
            .env("OPENTEAMS_PI_APPROVAL_EXTENSION", &approval)
            .env("OPENTEAMS_PI_MCP_EXTENSION", &mcp)
            .env("OPENTEAMS_PI_ENABLE_MCP_EXTENSION", "0")
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.group_spawn().expect("spawn launcher group");
        let pids = wait_for_pi_process_handshake(&mut child, &pid_file, Duration::from_secs(15))
            .await
            .expect("Pi process startup handshake");

        workspace_utils::process::kill_process_group(&mut child)
            .await
            .expect("kill process group");
        for _ in 0..100 {
            let any_alive = pids.iter().any(|pid| {
                std::process::Command::new("/bin/kill")
                    .args(["-0", pid])
                    .status()
                    .is_ok_and(|status| status.success())
            });
            if !any_alive {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        panic!("Pi process tree survived cancellation: {pids:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launcher_terminates_pi_tree_when_acp_parent_exits_abnormally() {
        use std::{fs, os::unix::fs::PermissionsExt};

        use command_group::AsyncCommandGroup;
        use tokio::{
            process::Command,
            time::{Duration, sleep},
        };

        assert!(resolve_executable_path_blocking("node").is_some());
        let temp = tempfile::tempdir().expect("temporary NPX environment");
        let node_modules = temp.path().join("node_modules");
        let bin = node_modules.join(".bin");
        let pi_package = node_modules.join(PI_CODING_AGENT_PACKAGE);
        let mcp_package = node_modules.join(PI_MCP_ADAPTER_PACKAGE);
        fs::create_dir_all(&bin).expect("NPX bin");
        fs::create_dir_all(&pi_package).expect("Pi package");
        fs::create_dir_all(&mcp_package).expect("MCP package");
        fs::write(
            pi_package.join("package.json"),
            format!(r#"{{"version":"{PI_CODING_AGENT_VERSION}"}}"#),
        )
        .expect("Pi metadata");
        fs::write(
            mcp_package.join("package.json"),
            format!(r#"{{"version":"{PI_MCP_ADAPTER_VERSION}"}}"#),
        )
        .expect("MCP metadata");
        fs::write(mcp_package.join("index.ts"), "export default () => {};").expect("MCP entry");
        let pid_file = temp.path().join("pids");
        let fake_pi = bin.join("pi");
        fs::write(
            &fake_pi,
            format!(
                "#!/bin/sh\nsleep 60 &\nprintf '%s\\n%s\\n' \"$$\" \"$!\" > '{}'\nwait\n",
                pid_file.display()
            ),
        )
        .expect("fake Pi");
        fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o700)).expect("Pi mode");
        let launcher = temp.path().join("launcher.mjs");
        let approval = temp.path().join("approval_extension.mjs");
        write_executable_file(&launcher, PI_LAUNCHER_SOURCE.as_bytes()).expect("launcher");
        fs::write(&approval, "export default () => {};").expect("approval");
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "\"$PI_LAUNCHER\" & while [ ! -f \"$PI_PID_FILE\" ]; do sleep 0.02; done; exit 23",
            ])
            .env("PI_LAUNCHER", &launcher)
            .env("PI_PID_FILE", &pid_file)
            .env("PATH", path)
            .env("OPENTEAMS_PI_SKILL_PATHS_JSON", "[]")
            .env("OPENTEAMS_PI_APPROVAL_EXTENSION", &approval)
            .env("OPENTEAMS_PI_ENABLE_MCP_EXTENSION", "0")
            .kill_on_drop(true);
        let mut parent = command.group_spawn().expect("spawn fake ACP parent");
        let group_id = parent.inner().id().expect("parent process ID");
        let status = parent.wait().await.expect("wait for abnormal parent exit");
        assert_eq!(status.code(), Some(23));
        let pids = fs::read_to_string(&pid_file)
            .expect("Pi child PIDs")
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();

        for _ in 0..200 {
            let any_alive = pids.iter().any(|pid| {
                std::process::Command::new("/bin/kill")
                    .args(["-0", pid])
                    .status()
                    .is_ok_and(|status| status.success())
            });
            if !any_alive {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", &format!("-{group_id}")])
            .status();
        panic!("Pi process tree survived abnormal ACP parent exit: {pids:?}");
    }

    #[test]
    fn availability_requires_node_and_ignores_npx_and_global_pi() {
        let mut queried_programs = Vec::new();
        assert!(prerequisites_available_on_path(|program| {
            queried_programs.push(program.to_string());
            matches!(program, "node" | "npx" | "pi")
        }));
        assert_eq!(queried_programs, ["node"]);

        let mut missing_node_queries = Vec::new();
        assert!(!prerequisites_available_on_path(|program| {
            missing_node_queries.push(program.to_string());
            matches!(program, "npx" | "pi")
        }));
        assert_eq!(missing_node_queries, ["node"]);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_snapshots_are_private_secret_safe_and_cleaned_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("runtime directory");
        let secret = "openteams-pi-secret-never-log";
        let snapshot = PiRuntimeSnapshot {
            skill_paths: vec![PathBuf::from("/registry/only/SKILL.md")],
            mcp_config: serde_json::json!({
                "mcpServers": {"only": {"command": "/bin/echo", "env": {"TOKEN": secret}}},
                "settings": {"hostConfigDiscovery": "off"}
            }),
        };
        assert!(!format!("{snapshot:?}").contains(secret));
        let files = PiRunFiles::create(temp.path(), &snapshot).expect("runtime files");
        let paths = files
            .paths()
            .into_iter()
            .map(Path::to_path_buf)
            .collect::<Vec<_>>();
        for path in &paths {
            let expected_mode = if path == &files.launcher_command {
                0o700
            } else {
                0o600
            };
            assert_eq!(
                fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                expected_mode
            );
        }
        let cleanup = files.into_cleanup();
        for path in &paths {
            assert!(
                path.exists(),
                "{} was cleaned before run end",
                path.display()
            );
        }
        drop(cleanup);
        for path in paths {
            assert!(!path.exists(), "{} was not cleaned", path.display());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_acp_startup_removes_all_pi_runtime_files() {
        let temp = tempfile::tempdir().expect("runtime directory");
        let mut pi = Pi::default();
        pi.cmd.base_command_override = Some("/bin/sh -c 'exit 7'".to_string());
        let env = ExecutionEnv::new(Default::default(), false, String::new());

        pi.spawn(temp.path(), "test", &env)
            .await
            .expect_err("ACP startup must fail");

        let runtime_dir = temp.path().join(".openteams").join("tmp");
        let remaining = fs::read_dir(runtime_dir)
            .expect("runtime directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("runtime files");
        assert!(
            remaining.is_empty(),
            "Pi runtime files survived startup failure"
        );
    }

    #[test]
    fn missing_allowlisted_mcp_server_is_a_member_configuration_error() {
        let policy = AcpMcpPolicy {
            allowed_server_names: Some(["missing".to_string()].into_iter().collect()),
            disabled_server_names: Default::default(),
        };
        let error = validate_member_mcp_allowlist(&serde_json::json!({"mcpServers": {}}), &policy)
            .expect_err("missing server must fail");
        assert!(
            error
                .to_string()
                .contains("invalid Pi member configuration")
        );
        assert!(!error.to_string().contains("mcpServers"));
    }

    #[test]
    fn unknown_pi_follow_up_session_uses_the_existing_compatibility_error() {
        let error = map_pi_follow_up_error(ExecutorError::Io(std::io::Error::other(
            "ACP startup failed: Unknown sessionId: missing",
        )));
        assert!(matches!(error, ExecutorError::FollowUpNotSupported(_)));

        let unrelated = map_pi_follow_up_error(ExecutorError::Io(std::io::Error::other(
            "ACP startup failed: transport closed",
        )));
        assert!(matches!(unrelated, ExecutorError::Io(_)));
    }

    #[test]
    fn approval_extension_uses_one_identical_gate_for_native_and_mcp_tools() {
        use std::process::Command;

        let Some(node) = resolve_executable_path_blocking("node") else {
            panic!("node is required for the Pi approval extension test");
        };
        let extension =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("src/executors/pi/approval_extension.mjs");
        let script = r#"
          const { default: install } = await import(process.argv[1]);
          const handlers = {};
          install({ on(name, fn) { handlers[name] = fn; } });
          const calls = [];
          for (const [toolName, allowed] of [['bash', true], ['docs_lookup', false]]) {
            let confirms = 0;
            let prompt;
            const result = await handlers.tool_call(
              {
                toolName,
                toolCallId: `${toolName}-id`,
                input: toolName === 'bash'
                  ? { command: 'cargo test -p executors --features qa-mode pi' }
                  : { value: 1 },
              },
              { ui: { async confirm(title, message) {
                confirms += 1;
                prompt = { title, message: JSON.parse(message) };
                return allowed;
              } } },
            );
            calls.push({ toolName, confirms, prompt, blocked: result?.block === true });
          }
          const notifications = [];
          const ctx = { ui: { notify(message, level) { notifications.push({ message, level }); } } };
          await handlers.agent_end({
            willRetry: true,
            messages: [{ role: 'assistant', stopReason: 'error', errorMessage: 'Connection error.' }],
          }, ctx);
          await handlers.agent_end({
            willRetry: false,
            messages: [
              { role: 'assistant', stopReason: 'error', errorMessage: 'Connection error.' },
              { role: 'assistant', stopReason: 'stop', content: [{ type: 'text', text: 'recovered' }] },
            ],
          }, ctx);
          await handlers.agent_end({
            willRetry: false,
            messages: [{ role: 'assistant', stopReason: 'error', errorMessage: 'Connection error.' }],
          }, ctx);
          await handlers.agent_end({
            willRetry: false,
            messages: [{ role: 'assistant', stopReason: 'error', errorMessage: 'secret-value' }],
          }, ctx);
          process.stdout.write(JSON.stringify({ calls, notifications }));
        "#;
        let output = Command::new(node)
            .args(["--input-type=module", "--eval", script])
            .arg(extension)
            .output()
            .expect("run extension test");
        assert!(output.status.success());
        let result: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("extension result");
        let calls = &result["calls"];
        assert_eq!(calls[0]["confirms"], 1);
        assert_eq!(calls[0]["blocked"], false);
        assert_eq!(calls[0]["prompt"]["message"]["toolName"], "bash");
        assert_eq!(
            calls[0]["prompt"]["message"]["input"]["command"],
            "cargo test -p executors --features qa-mode pi"
        );
        assert_eq!(calls[1]["confirms"], 1);
        assert_eq!(calls[1]["blocked"], true);
        let notifications = &result["notifications"];
        assert_eq!(notifications.as_array().map(Vec::len), Some(2));
        assert_eq!(
            notifications[0]["message"],
            "Pi provider connection failed."
        );
        assert_eq!(notifications[0]["level"], "error");
        assert_eq!(notifications[1]["message"], "Pi provider request failed.");
        assert!(
            !output
                .stdout
                .windows(12)
                .any(|bytes| bytes == b"secret-value")
        );
    }
}
