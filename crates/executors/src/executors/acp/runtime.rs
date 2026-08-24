use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use agent_client_protocol::{
    Agent, ConnectionTo, ErrorCode, Lines, UntypedMessage,
    schema::{
        ProtocolVersion,
        v1::{
            AuthenticateRequest, BooleanConfigOptionCapabilities, CancelNotification,
            ClientCapabilities, ClientSessionCapabilities, CloseSessionRequest, ContentBlock,
            CreateTerminalRequest, DeleteSessionRequest, FileSystemCapabilities, ImageContent,
            Implementation, InitializeRequest, KillTerminalRequest, LoadSessionRequest, McpServer,
            NewSessionRequest, PromptRequest, ReadTextFileRequest, ReleaseTerminalRequest,
            RequestPermissionRequest, ResumeSessionRequest, SessionConfigKind, SessionConfigOption,
            SessionConfigOptionCategory, SessionConfigOptionValue,
            SessionConfigOptionsCapabilities, SessionConfigSelectOptions, SessionId,
            SessionNotification, SetSessionConfigOptionRequest, StopReason, TerminalOutputRequest,
            TextContent, WaitForTerminalExitRequest, WriteTextFileRequest,
        },
    },
};
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use futures::{AsyncBufReadExt, sink};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use tokio_util::{compat::TokioAsyncReadCompatExt, sync::CancellationToken};

use super::{
    AcpApprovalPolicy, AcpAuthMethodInfo, AcpCapabilityProbe, AcpClient, AcpConfigChoice,
    AcpConfigOptionKind, AcpConfigOptionSnapshot, AcpConfigSource, AcpEvent, AcpResumePolicy,
    AcpRunConfig, AcpSessionPreferences, config::is_session_mode_key, mcp::validate_mcp_servers,
    output::AcpOutput,
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandParts},
    env::ExecutionEnv,
    executors::{ExecutorError, ExecutorExitResult, ExecutorOutput, ExecutorPrompt, SpawnedChild},
    model_identity::model_id_match_score,
};

#[derive(Debug)]
enum BootstrapError {
    FollowUpNotSupported(String),
    AuthRequired(String),
    Configuration(String),
    Other(String),
}

const INVALID_SESSION_RECOVERY_MESSAGE: &str = "ACP session recovery reused the requested session ID and the prompt was refused; the session is invalid";
const ACP_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacySessionModelState {
    current_model_id: String,
    #[serde(default)]
    available_models: Vec<LegacyModelInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyModelInfo {
    model_id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug)]
struct AcpSessionConfigState {
    session_id: SessionId,
    session_id_was_fallback: bool,
    config_options: Vec<SessionConfigOption>,
    legacy_models: Option<LegacySessionModelState>,
}

/// Generic stable ACP v1 process and protocol harness.
pub struct AcpAgentHarness {
    config: AcpRunConfig,
}

impl Default for AcpAgentHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpAgentHarness {
    pub fn new() -> Self {
        Self {
            config: AcpRunConfig::default(),
        }
    }

    pub fn with_approval_policy(mut self, approval_policy: AcpApprovalPolicy) -> Self {
        self.config.approval_policy = approval_policy;
        self
    }

    pub fn with_resume_policy(mut self, resume_policy: AcpResumePolicy) -> Self {
        self.config.resume_policy = resume_policy;
        self
    }

    /// Promote an empty successful `end_turn` to an authentication error for
    /// Agents known to use that response when their credentials are rejected.
    pub fn with_empty_end_turn_auth_error(mut self, message: impl Into<String>) -> Self {
        self.config.empty_end_turn_auth_error = Some(message.into());
        self
    }

    pub fn with_auth_method_id(mut self, method_id: impl Into<String>) -> Self {
        self.config.auth_method_id = Some(method_id.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.config.session.model = Some(model.into());
        self
    }

    pub fn with_thought_level(mut self, thought_level: impl Into<String>) -> Self {
        self.config.session.thought_level = Some(thought_level.into());
        self
    }

    pub fn with_native_thought_level_fallback(mut self, thought_level: impl Into<String>) -> Self {
        self.config.session.thought_level = Some(thought_level.into());
        self.config.session.native_thought_level_fallback = true;
        self
    }

    /// Enforce an adapter-owned safe mode before applying user preferences.
    ///
    /// This setting is intentionally separate from ordinary config overrides,
    /// which are never allowed to control an Agent's permission mode.
    pub fn with_required_session_mode(
        mut self,
        option_id: impl Into<String>,
        value_id: impl Into<String>,
    ) -> Self {
        self.config.session.required_session_mode = Some(super::AcpConfigSelection {
            option_id: option_id.into(),
            value: SessionConfigOptionValue::value_id(value_id.into()),
        });
        self
    }

    pub fn with_config_override(mut self, selection: &super::AcpConfigOverride) -> Self {
        if selection.controls_session_mode() {
            return self;
        }
        self.config.session.options.push(super::AcpConfigSelection {
            option_id: selection.option_id.clone(),
            value: selection.value.to_protocol(),
        });
        self
    }

    pub fn with_additional_directories(mut self, directories: Vec<PathBuf>) -> Self {
        self.config.additional_directories = directories;
        self
    }

    pub fn with_mcp_servers(mut self, servers: Vec<McpServer>) -> Self {
        self.config.mcp_servers = servers;
        self
    }

    pub fn with_client_services(mut self, services: super::AcpClientServicePolicy) -> Self {
        self.config.client_services = services;
        self
    }

    #[cfg(test)]
    pub(crate) fn required_session_mode(&self) -> Option<(&str, &SessionConfigOptionValue)> {
        self.config
            .session
            .required_session_mode
            .as_ref()
            .map(|selection| (selection.option_id.as_str(), &selection.value))
    }

    #[cfg(test)]
    pub(crate) fn empty_end_turn_auth_error(&self) -> Option<&str> {
        self.config.empty_end_turn_auth_error.as_deref()
    }

    pub fn with_full_access(mut self, full_access: bool) -> Self {
        self.config.client_services.full_access = full_access;
        self
    }

    pub async fn spawn_with_command(
        &self,
        current_dir: &Path,
        prompt: String,
        command_parts: CommandParts,
        env: &ExecutionEnv,
        cmd_overrides: &CmdOverrides,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
    ) -> Result<SpawnedChild, ExecutorError> {
        let display_text = prompt.clone();
        self.spawn_internal(
            current_dir,
            vec![ContentBlock::Text(TextContent::new(prompt))],
            display_text,
            None,
            command_parts,
            env,
            cmd_overrides,
            approvals,
        )
        .await
    }

    pub async fn spawn_structured_with_command(
        &self,
        current_dir: &Path,
        prompt: ExecutorPrompt,
        command_parts: CommandParts,
        env: &ExecutionEnv,
        cmd_overrides: &CmdOverrides,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
    ) -> Result<SpawnedChild, ExecutorError> {
        let (blocks, display_text) = structured_prompt_blocks(prompt);
        self.spawn_internal(
            current_dir,
            blocks,
            display_text,
            None,
            command_parts,
            env,
            cmd_overrides,
            approvals,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_follow_up_with_command(
        &self,
        current_dir: &Path,
        prompt: String,
        session_id: &str,
        command_parts: CommandParts,
        env: &ExecutionEnv,
        cmd_overrides: &CmdOverrides,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
    ) -> Result<SpawnedChild, ExecutorError> {
        let display_text = prompt.clone();
        self.spawn_internal(
            current_dir,
            vec![ContentBlock::Text(TextContent::new(prompt))],
            display_text,
            Some(session_id.to_string()),
            command_parts,
            env,
            cmd_overrides,
            approvals,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn spawn_follow_up_structured_with_command(
        &self,
        current_dir: &Path,
        prompt: ExecutorPrompt,
        session_id: &str,
        command_parts: CommandParts,
        env: &ExecutionEnv,
        cmd_overrides: &CmdOverrides,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
    ) -> Result<SpawnedChild, ExecutorError> {
        let (blocks, display_text) = structured_prompt_blocks(prompt);
        self.spawn_internal(
            current_dir,
            blocks,
            display_text,
            Some(session_id.to_string()),
            command_parts,
            env,
            cmd_overrides,
            approvals,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_internal(
        &self,
        current_dir: &Path,
        prompt: Vec<ContentBlock>,
        prompt_text: String,
        existing_session: Option<String>,
        command_parts: CommandParts,
        env: &ExecutionEnv,
        cmd_overrides: &CmdOverrides,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
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
            .env("NODE_NO_WARNINGS", "1")
            .args(&args);
        env.clone()
            .with_profile(cmd_overrides)
            .apply_to_command(&mut command);

        let mut child = command.group_spawn()?;
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
        let cancel = CancellationToken::new();
        let stdout = self
            .bootstrap_acp_connection(
                &mut child,
                current_dir.to_path_buf(),
                existing_session,
                prompt,
                prompt_text,
                exit_tx,
                approvals,
                env.vars.clone(),
                cancel.clone(),
            )
            .await?;

        Ok(SpawnedChild {
            child,
            stdout: Some(stdout),
            exit_signal: Some(exit_rx),
            cancel: Some(cancel),
            cleanup: None,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn bootstrap_acp_connection(
        &self,
        child: &mut AsyncGroupChild,
        cwd: PathBuf,
        existing_session: Option<String>,
        prompt: Vec<ContentBlock>,
        prompt_text: String,
        exit_signal: tokio::sync::oneshot::Sender<ExecutorExitResult>,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
        terminal_env: std::collections::HashMap<String, String>,
        cancel: CancellationToken,
    ) -> Result<ExecutorOutput, ExecutorError> {
        let protocol_stdout = child.inner().stdout.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Child process has no stdout",
            ))
        })?;
        let stdin = child.inner().stdin.take().ok_or_else(|| {
            ExecutorError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Child process has no stdin",
            ))
        })?;
        let (output, executor_stdout) = AcpOutput::channel();
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let startup_tx = Arc::new(StdMutex::new(Some(startup_tx)));

        let config = self.config.clone();
        let output_for_runtime = output.clone();
        let runtime_cancel = cancel.clone();
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build ACP runtime");
            runtime.block_on(async move {
                let incoming = futures::io::BufReader::new(protocol_stdout.compat()).lines();
                let outgoing = sink::unfold(stdin, |mut stdin, line: String| async move {
                    stdin.write_all(line.as_bytes()).await?;
                    const LINE_ENDING: &[u8] = if cfg!(windows) { b"\r\n" } else { b"\n" };
                    stdin.write_all(LINE_ENDING).await?;
                    stdin.flush().await?;
                    Ok::<_, std::io::Error>(stdin)
                });
                let transport = Lines::new(outgoing, incoming);

                let client = AcpClient::new(
                    output_for_runtime.clone(),
                    approvals,
                    config.approval_policy,
                    runtime_cancel.clone(),
                    cwd.clone(),
                    config.additional_directories.clone(),
                    config.client_services,
                    terminal_env,
                );
                client.begin_session_replay();
                let notify_client = client.clone();
                let permission_client = client.clone();
                let read_client = client.clone();
                let write_client = client.clone();
                let create_terminal_client = client.clone();
                let terminal_output_client = client.clone();
                let wait_terminal_client = client.clone();
                let kill_terminal_client = client.clone();
                let release_terminal_client = client.clone();
                let cleanup_client = client.clone();
                let startup_for_connection = startup_tx.clone();
                let output_for_connection = output_for_runtime.clone();
                let config_for_connection = config.clone();
                let cancel_for_connection = runtime_cancel.clone();

                let result = agent_client_protocol::Client
                    .builder()
                    .on_receive_notification(
                        async move |notification: SessionNotification, _cx| {
                            notify_client.handle_notification(notification).await
                        },
                        agent_client_protocol::on_receive_notification!(),
                    )
                    .on_receive_request(
                        async move |request: RequestPermissionRequest, responder, _cx| {
                            responder.respond_with_result(
                                permission_client.request_permission(request).await,
                            )
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |request: ReadTextFileRequest, responder, _cx| {
                            responder.respond_with_result(read_client.read_text_file(request).await)
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |request: WriteTextFileRequest, responder, _cx| {
                            responder
                                .respond_with_result(write_client.write_text_file(request).await)
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |request: CreateTerminalRequest, responder, _cx| {
                            responder.respond_with_result(
                                create_terminal_client.create_terminal(request).await,
                            )
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |request: TerminalOutputRequest, responder, _cx| {
                            responder.respond_with_result(
                                terminal_output_client.terminal_output(request).await,
                            )
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |request: WaitForTerminalExitRequest, responder, _cx| {
                            responder.respond_with_result(
                                wait_terminal_client.wait_for_terminal_exit(request).await,
                            )
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |request: KillTerminalRequest, responder, _cx| {
                            responder.respond_with_result(
                                kill_terminal_client.kill_terminal(request).await,
                            )
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .on_receive_request(
                        async move |request: ReleaseTerminalRequest, responder, _cx| {
                            responder.respond_with_result(
                                release_terminal_client.release_terminal(request).await,
                            )
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
                        run_connection(
                            &connection,
                            &client,
                            &cwd,
                            existing_session,
                            prompt,
                            prompt_text,
                            config_for_connection,
                            startup_for_connection,
                            output_for_connection,
                            cancel_for_connection,
                        )
                        .await
                    })
                    .await;
                cleanup_client.shutdown_terminals().await;
                drop(cleanup_client);
                let was_cancelled = runtime_cancel.is_cancelled();

                if let Err(error) = &result
                    && !was_cancelled
                {
                    let startup_error = match error.code {
                        agent_client_protocol::ErrorCode::AuthRequired => {
                            BootstrapError::AuthRequired(error.to_string())
                        }
                        agent_client_protocol::ErrorCode::InvalidParams => {
                            BootstrapError::Configuration(error.to_string())
                        }
                        _ => BootstrapError::Other(error.to_string()),
                    };
                    send_startup(&startup_tx, Err(startup_error));
                    let _ = output_for_runtime
                        .send(AcpEvent::Error(protocol_error_message(error)))
                        .await;
                }

                drop(output_for_runtime);
                let failure = result.as_ref().err().and_then(|error| {
                    is_invalid_session_recovery_error(error).then(|| protocol_error_message(error))
                });
                let _ = exit_signal.send(if result.is_ok() || was_cancelled {
                    ExecutorExitResult::Success
                } else if let Some(message) = failure {
                    ExecutorExitResult::FailureWithError(message)
                } else {
                    ExecutorExitResult::Failure
                });
            });
        });

        let startup_result = tokio::time::timeout(ACP_STARTUP_TIMEOUT, startup_rx).await;
        match startup_result {
            Ok(Ok(Ok(()))) => Ok(executor_stdout),
            Ok(Ok(Err(BootstrapError::FollowUpNotSupported(message)))) => {
                Err(ExecutorError::FollowUpNotSupported(message))
            }
            Ok(Ok(Err(BootstrapError::AuthRequired(message)))) => {
                Err(ExecutorError::AuthRequired(message))
            }
            Ok(Ok(Err(BootstrapError::Configuration(message)))) => {
                Err(ExecutorError::Configuration(message))
            }
            Ok(Ok(Err(BootstrapError::Other(message)))) => Err(ExecutorError::Io(
                std::io::Error::other(format!("ACP startup failed: {message}")),
            )),
            Ok(Err(_)) => Err(ExecutorError::Io(std::io::Error::other(
                "ACP startup task exited before initialization",
            ))),
            Err(_) => {
                cancel.cancel();
                Err(ExecutorError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!(
                        "ACP startup timed out after {} seconds",
                        ACP_STARTUP_TIMEOUT.as_secs()
                    ),
                )))
            }
        }
    }
}

pub async fn probe_acp_command(
    command_parts: CommandParts,
    current_dir: &Path,
    env: &ExecutionEnv,
    cmd_overrides: &CmdOverrides,
    auth_method_id: Option<String>,
) -> Result<AcpCapabilityProbe, ExecutorError> {
    probe_acp_command_inner(
        command_parts,
        current_dir,
        env,
        cmd_overrides,
        auth_method_id,
        true,
    )
    .await
}

/// Probe ACP initialization without creating a session.
///
/// Some Agents cannot close or delete a probe session. Callers can use this
/// variant and populate model/config metadata from an Agent-specific local
/// source to avoid leaving empty sessions behind.
pub async fn probe_acp_command_without_session(
    command_parts: CommandParts,
    current_dir: &Path,
    env: &ExecutionEnv,
    cmd_overrides: &CmdOverrides,
    auth_method_id: Option<String>,
) -> Result<AcpCapabilityProbe, ExecutorError> {
    probe_acp_command_inner(
        command_parts,
        current_dir,
        env,
        cmd_overrides,
        auth_method_id,
        false,
    )
    .await
}

async fn probe_acp_command_inner(
    command_parts: CommandParts,
    current_dir: &Path,
    env: &ExecutionEnv,
    cmd_overrides: &CmdOverrides,
    auth_method_id: Option<String>,
    create_probe_session: bool,
) -> Result<AcpCapabilityProbe, ExecutorError> {
    let command_display = command_parts.redacted_display();
    let (program_path, args) = command_parts.into_resolved().await.map_err(|error| {
        acp_probe_diagnostic_error(&command_display, "resolve ACP probe executable", error)
    })?;
    let mut command = Command::new(program_path);
    command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(current_dir)
        .args(args);
    env.clone()
        .with_profile(cmd_overrides)
        .apply_to_command(&mut command);
    let mut child = command.group_spawn().map_err(|error| {
        acp_probe_diagnostic_error(&command_display, "start ACP probe process", error)
    })?;
    let stdout = child.inner().stdout.take().ok_or_else(|| {
        acp_probe_diagnostic_error(
            &command_display,
            "open ACP probe stdout",
            "stdout pipe unavailable",
        )
    })?;
    let stdin = child.inner().stdin.take().ok_or_else(|| {
        acp_probe_diagnostic_error(
            &command_display,
            "open ACP probe stdin",
            "stdin pipe unavailable",
        )
    })?;
    let stderr = child.inner().stderr.take().ok_or_else(|| {
        acp_probe_diagnostic_error(
            &command_display,
            "open ACP probe stderr",
            "stderr pipe unavailable",
        )
    })?;
    let stderr_task = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut captured = Vec::new();
        let read_result = stderr.read_to_end(&mut captured).await;
        (captured, read_result)
    });
    let (probe_tx, probe_rx) =
        tokio::sync::oneshot::channel::<Result<AcpCapabilityProbe, String>>();
    let probe_tx = Arc::new(StdMutex::new(Some(probe_tx)));
    let probe_cwd = current_dir.to_path_buf();

    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build ACP probe runtime");
        runtime.block_on(async move {
            let incoming = futures::io::BufReader::new(stdout.compat()).lines();
            let outgoing = sink::unfold(stdin, |mut stdin, line: String| async move {
                stdin.write_all(line.as_bytes()).await?;
                stdin
                    .write_all(if cfg!(windows) { b"\r\n" } else { b"\n" })
                    .await?;
                stdin.flush().await?;
                Ok::<_, std::io::Error>(stdin)
            });
            let transport = Lines::new(outgoing, incoming);
            let probe_tx_for_connection = Arc::clone(&probe_tx);
            let connection_result = agent_client_protocol::Client
                .builder()
                .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
                    let initialize = connection
                        .send_request(
                            InitializeRequest::new(ProtocolVersion::V1)
                                .client_capabilities(acp_client_capabilities(false, false, false))
                                .client_info(Implementation::new(
                                    "openteams-probe",
                                    env!("CARGO_PKG_VERSION"),
                                )),
                        )
                        .block_task()
                        .await?;
                    if initialize.protocol_version != ProtocolVersion::V1 {
                        return Err(
                            agent_client_protocol::Error::invalid_request().data(format!(
                                "unsupported ACP protocol version {}",
                                initialize.protocol_version
                            )),
                        );
                    }
                    let session = &initialize.agent_capabilities.session_capabilities;
                    let supports_close = session.close.is_some();
                    let supports_delete = session.delete.is_some();
                    if let Some(method_id) = auth_method_id.as_deref() {
                        if !initialize
                            .auth_methods
                            .iter()
                            .any(|method| method.id().0.as_ref() == method_id)
                        {
                            return Err(agent_client_protocol::Error::auth_required().data(
                                format!(
                                    "ACP authentication method `{method_id}` was not advertised"
                                ),
                            ));
                        }
                        connection
                            .send_request(AuthenticateRequest::new(method_id.to_string()))
                            .block_task()
                            .await?;
                    }
                    let (config_source, config_options) = if create_probe_session {
                        let session_state = send_session_start_request(
                            &connection,
                            "session/new",
                            NewSessionRequest::new(probe_cwd)
                                .additional_directories(Vec::new())
                                .mcp_servers(Vec::new()),
                            None,
                        )
                        .await?;
                        let config = match &session_state {
                            state if !state.config_options.is_empty() => (
                                AcpConfigSource::Stable,
                                snapshot_config_options(&state.config_options),
                            ),
                            state if state.legacy_models.is_some() => (
                                AcpConfigSource::LegacyModel,
                                vec![legacy_model_config_snapshot(
                                    state.legacy_models.as_ref().expect("checked legacy models"),
                                )],
                            ),
                            _ => (AcpConfigSource::None, Vec::new()),
                        };
                        if supports_close {
                            let _ = connection
                                .send_request(CloseSessionRequest::new(session_state.session_id))
                                .block_task()
                                .await;
                        } else if supports_delete {
                            let _ = connection
                                .send_request(DeleteSessionRequest::new(session_state.session_id))
                                .block_task()
                                .await;
                        }
                        config
                    } else {
                        (AcpConfigSource::None, Vec::new())
                    };
                    let probe = AcpCapabilityProbe {
                        protocol_version: initialize.protocol_version.to_string(),
                        agent_name: initialize.agent_info.as_ref().map(|info| info.name.clone()),
                        agent_version: initialize
                            .agent_info
                            .as_ref()
                            .map(|info| info.version.clone()),
                        auth_methods: initialize
                            .auth_methods
                            .iter()
                            .map(|method| AcpAuthMethodInfo {
                                id: method.id().to_string(),
                                name: method.name().to_string(),
                                description: method.description().map(str::to_string),
                            })
                            .collect(),
                        supports_session_list: session.list.is_some(),
                        supports_session_resume: session.resume.is_some(),
                        supports_session_load: initialize.agent_capabilities.load_session,
                        supports_session_close: supports_close,
                        supports_session_delete: supports_delete,
                        supports_additional_directories: session.additional_directories.is_some(),
                        agent_capabilities: serde_json::to_value(&initialize.agent_capabilities)
                            .unwrap_or(serde_json::Value::Null),
                        config_source,
                        config_options,
                    };
                    if let Some(sender) = probe_tx_for_connection
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take()
                    {
                        let _ = sender.send(Ok(probe));
                    }
                    Ok(())
                })
                .await;
            if let Err(error) = connection_result
                && let Some(sender) = probe_tx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
            {
                let _ = sender.send(Err(error.to_string()));
            }
        });
    });

    let result = tokio::time::timeout(Duration::from_secs(12), probe_rx)
        .await
        .map_err(|_| {
            acp_probe_diagnostic_error(
                &command_display,
                "initialize ACP connection",
                "timed out after 12 seconds",
            )
        })?
        .map_err(|_| {
            acp_probe_diagnostic_error(
                &command_display,
                "initialize ACP connection",
                "probe process exited without a response",
            )
        })?
        .map_err(|error| {
            acp_probe_diagnostic_error(&command_display, "initialize ACP connection", error)
        });
    let process_status = match child.inner().try_wait() {
        Ok(Some(status)) => format!("exited ({status})"),
        Ok(None) => "running".to_string(),
        Err(error) => format!("status unavailable ({error})"),
    };
    let _ = child.kill().await;
    let stderr_diagnostics = match tokio::time::timeout(Duration::from_secs(2), stderr_task).await {
        Ok(Ok((captured, Ok(_)))) => String::from_utf8_lossy(&captured).trim_end().to_string(),
        Ok(Ok((captured, Err(error)))) => format!(
            "{}\n[stderr read failed: {error}]",
            String::from_utf8_lossy(&captured).trim_end()
        ),
        Ok(Err(error)) => format!("[stderr capture task failed: {error}]"),
        Err(_) => "[stderr capture timed out]".to_string(),
    };
    result.map_err(|error| {
        ExecutorError::Io(std::io::Error::other(format!(
            "{error}; outer_process_status={process_status}; outer_stderr={stderr_diagnostics:?}"
        )))
    })
}

fn acp_probe_diagnostic_error(
    command: &str,
    operation: &str,
    result: impl std::fmt::Display,
) -> ExecutorError {
    ExecutorError::Io(std::io::Error::other(format!(
        "command=`{command}`; operation={operation}; result={}",
        result.to_string().trim()
    )))
}

fn acp_client_capabilities(
    read_text_file: bool,
    write_text_file: bool,
    terminal: bool,
) -> ClientCapabilities {
    ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(read_text_file)
            .write_text_file(write_text_file))
        .terminal(terminal)
        .session(ClientSessionCapabilities::new().config_options(
            SessionConfigOptionsCapabilities::new().boolean(BooleanConfigOptionCapabilities::new()),
        ))
}

async fn send_session_start_request(
    connection: &ConnectionTo<Agent>,
    method: &str,
    params: impl Serialize,
    fallback_session_id: Option<SessionId>,
) -> agent_client_protocol::Result<AcpSessionConfigState> {
    let request = UntypedMessage::new(method, params)?;
    let response = connection.send_request(request).block_task().await?;
    parse_session_start_response(method, response, fallback_session_id)
}

fn parse_session_start_response(
    method: &str,
    response: serde_json::Value,
    fallback_session_id: Option<SessionId>,
) -> agent_client_protocol::Result<AcpSessionConfigState> {
    let response_session_id = response
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(SessionId::new);
    let session_id_was_fallback = response_session_id.is_none() && fallback_session_id.is_some();
    let session_id = response_session_id.or(fallback_session_id).ok_or_else(|| {
        agent_client_protocol::Error::internal_error()
            .data(format!("ACP `{method}` response omitted sessionId"))
    })?;
    let config_options = response
        .get("configOptions")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(|value| {
                    serde_json::from_value(value.clone()).map_err(|error| {
                        agent_client_protocol::Error::internal_error().data(format!(
                            "ACP `{method}` returned an invalid config option: {error}"
                        ))
                    })
                })
                .collect::<agent_client_protocol::Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    let legacy_models = match response.get("models") {
        Some(value) => Some(serde_json::from_value(value.clone()).map_err(|error| {
            agent_client_protocol::Error::internal_error().data(format!(
                "ACP `{method}` returned invalid legacy models: {error}"
            ))
        })?),
        None => None,
    };

    Ok(AcpSessionConfigState {
        session_id,
        session_id_was_fallback,
        config_options,
        legacy_models,
    })
}

fn is_unknown_session_error(error: &agent_client_protocol::Error) -> bool {
    matches!(error.code, ErrorCode::ResourceNotFound)
        || matches!(error.code, ErrorCode::InvalidParams) && {
            let message = error.message.to_ascii_lowercase();
            message.contains("unknown session") || message.contains("session not found")
        }
}

fn snapshot_config_options(options: &[SessionConfigOption]) -> Vec<AcpConfigOptionSnapshot> {
    options.iter().filter_map(snapshot_config_option).collect()
}

fn snapshot_config_option(option: &SessionConfigOption) -> Option<AcpConfigOptionSnapshot> {
    let category = option.category.as_ref().and_then(|category| {
        serde_json::to_value(category)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
    });
    let kind = match &option.kind {
        SessionConfigKind::Select(select) => {
            let options = match &select.options {
                SessionConfigSelectOptions::Ungrouped(options) => options
                    .iter()
                    .map(|choice| AcpConfigChoice {
                        value: choice.value.0.to_string(),
                        name: choice.name.clone(),
                        description: choice.description.clone(),
                    })
                    .collect(),
                SessionConfigSelectOptions::Grouped(groups) => groups
                    .iter()
                    .flat_map(|group| group.options.iter())
                    .map(|choice| AcpConfigChoice {
                        value: choice.value.0.to_string(),
                        name: choice.name.clone(),
                        description: choice.description.clone(),
                    })
                    .collect(),
                _ => return None,
            };
            AcpConfigOptionKind::Select {
                current_value: select.current_value.0.to_string(),
                options,
            }
        }
        SessionConfigKind::Boolean(boolean) => AcpConfigOptionKind::Boolean {
            current_value: boolean.current_value,
        },
        _ => return None,
    };
    Some(AcpConfigOptionSnapshot {
        id: option.id.0.to_string(),
        name: option.name.clone(),
        description: option.description.clone(),
        category,
        kind,
    })
}

fn legacy_model_config_snapshot(state: &LegacySessionModelState) -> AcpConfigOptionSnapshot {
    let mut options = state
        .available_models
        .iter()
        .map(|model| AcpConfigChoice {
            value: model.model_id.clone(),
            name: model.name.clone(),
            description: model.description.clone(),
        })
        .collect::<Vec<_>>();
    if !options
        .iter()
        .any(|option| option.value == state.current_model_id)
    {
        options.push(AcpConfigChoice {
            value: state.current_model_id.clone(),
            name: state.current_model_id.clone(),
            description: None,
        });
    }
    AcpConfigOptionSnapshot {
        id: "model".to_string(),
        name: "Model".to_string(),
        description: Some("Legacy ACP session model selector".to_string()),
        category: Some("model".to_string()),
        kind: AcpConfigOptionKind::Select {
            current_value: state.current_model_id.clone(),
            options,
        },
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_connection(
    connection: &ConnectionTo<Agent>,
    client: &AcpClient,
    cwd: &Path,
    existing_session: Option<String>,
    prompt: Vec<ContentBlock>,
    prompt_text: String,
    config: AcpRunConfig,
    startup_tx: StartupSender,
    output: AcpOutput,
    cancel: CancellationToken,
) -> agent_client_protocol::Result<()> {
    let client_capabilities = acp_client_capabilities(
        config.client_services.read_text_file,
        config.client_services.write_text_file,
        config.client_services.terminal,
    );
    let initialize = connection
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_capabilities(client_capabilities)
                .client_info(Implementation::new("openteams", env!("CARGO_PKG_VERSION"))),
        )
        .block_task()
        .await?;
    if initialize.protocol_version != ProtocolVersion::V1 {
        return Err(
            agent_client_protocol::Error::invalid_request().data(format!(
                "unsupported ACP protocol version {}",
                initialize.protocol_version
            )),
        );
    }
    let negotiated = super::session::AcpNegotiatedState::from_initialize(&initialize);
    if prompt
        .iter()
        .any(|block| matches!(block, ContentBlock::Image(_)))
        && !negotiated.agent_capabilities.prompt_capabilities.image
    {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("ACP Agent does not advertise image prompt support"));
    }
    if let Some(agent_info) = &negotiated.agent_info {
        tracing::debug!(
            agent_name = %agent_info.name,
            agent_version = %agent_info.version,
            "negotiated ACP Agent"
        );
    }
    if let Some(method_id) = config.auth_method_id.as_deref() {
        if !negotiated.advertises_auth_method(method_id) {
            let message = format!("ACP authentication method `{method_id}` was not advertised");
            send_startup(
                &startup_tx,
                Err(BootstrapError::AuthRequired(message.clone())),
            );
            return Err(agent_client_protocol::Error::auth_required().data(message));
        }
        connection
            .send_request(AuthenticateRequest::new(method_id.to_string()))
            .block_task()
            .await?;
    }
    validate_mcp_servers(&config.mcp_servers, &negotiated.agent_capabilities)
        .map_err(|message| agent_client_protocol::Error::invalid_params().data(message))?;
    if !config.additional_directories.is_empty()
        && negotiated
            .agent_capabilities
            .session_capabilities
            .additional_directories
            .is_none()
    {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("ACP Agent does not support additionalDirectories"));
    }

    let mut resumed_existing_session = false;
    let mut session_state = match existing_session {
        None => {
            send_session_start_request(
                connection,
                "session/new",
                NewSessionRequest::new(cwd)
                    .additional_directories(config.additional_directories.clone())
                    .mcp_servers(config.mcp_servers.clone()),
                None,
            )
            .await?
        }
        Some(existing) => {
            let session_id = SessionId::new(existing);
            let resume_result = if negotiated
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some()
            {
                send_session_start_request(
                    connection,
                    "session/resume",
                    ResumeSessionRequest::new(session_id.clone(), cwd)
                        .additional_directories(config.additional_directories.clone())
                        .mcp_servers(config.mcp_servers.clone()),
                    Some(session_id.clone()),
                )
                .await
            } else if negotiated.agent_capabilities.load_session {
                send_session_start_request(
                    connection,
                    "session/load",
                    LoadSessionRequest::new(session_id.clone(), cwd)
                        .additional_directories(config.additional_directories.clone())
                        .mcp_servers(config.mcp_servers.clone()),
                    Some(session_id.clone()),
                )
                .await
            } else {
                let message =
                    "Agent advertises neither session/resume nor session/load".to_string();
                send_startup(
                    &startup_tx,
                    Err(BootstrapError::FollowUpNotSupported(message.clone())),
                );
                return Err(agent_client_protocol::Error::method_not_found().data(message));
            };
            match resume_result {
                Ok(state) => {
                    resumed_existing_session = true;
                    state
                }
                Err(error)
                    if config.resume_policy == AcpResumePolicy::UnknownSessionStartsNew
                        && is_unknown_session_error(&error) =>
                {
                    tracing::warn!(
                        requested_session_id = %session_id.0,
                        "ACP Agent no longer knows the persisted session; starting a replacement session"
                    );
                    send_session_start_request(
                        connection,
                        "session/new",
                        NewSessionRequest::new(cwd)
                            .additional_directories(config.additional_directories.clone())
                            .mcp_servers(config.mcp_servers.clone()),
                        None,
                    )
                    .await?
                }
                Err(error) => return Err(error),
            }
        }
    };
    let session_id = session_state.session_id.clone();
    let session_id_was_fallback = session_state.session_id_was_fallback;

    output
        .send(AcpEvent::SessionStart(session_id.0.to_string()))
        .await
        .map_err(|_| agent_client_protocol::Error::internal_error())?;
    let effective_model = apply_session_preferences(
        connection,
        &session_id,
        &config.session,
        &mut session_state.config_options,
        session_state.legacy_models.as_ref(),
    )
    .await?;
    client
        .set_token_usage_identity(
            negotiated
                .agent_info
                .as_ref()
                .map(|agent| agent.name.clone()),
            effective_model,
            session_id.0.to_string(),
        )
        .await;
    if !session_state.config_options.is_empty() {
        output
            .send(AcpEvent::ConfigOptions(
                session_state.config_options.clone(),
            ))
            .await
            .map_err(|_| agent_client_protocol::Error::internal_error())?;
    }
    client.begin_token_usage_turn().await;
    client.record_user_prompt_event(&prompt_text).await;
    send_startup(&startup_tx, Ok(()));

    let request = connection
        .send_request(PromptRequest::new(session_id.clone(), prompt))
        .block_task();
    tokio::pin!(request);
    let response = tokio::select! {
        response = &mut request => response?,
        _ = cancel.cancelled() => {
            connection.send_notification(CancelNotification::new(session_id.clone()))?;
            tokio::time::timeout(std::time::Duration::from_secs(10), &mut request)
            .await
                .map_err(|_| agent_client_protocol::Error::request_cancelled())??
        }
    };
    if config.resume_policy == AcpResumePolicy::RefusalMeansInvalidSession
        && resumed_existing_session
        && session_id_was_fallback
        && response.stop_reason == StopReason::Refusal
    {
        return Err(
            agent_client_protocol::Error::invalid_params().data(INVALID_SESSION_RECOVERY_MESSAGE)
        );
    }
    if let Some(usage) = client.finish_turn_token_usage(&response).await {
        output
            .send(AcpEvent::TokenUsage(usage))
            .await
            .map_err(|_| agent_client_protocol::Error::internal_error())?;
    }
    if let Some(error) = client.take_terminal_api_error().await {
        return Err(agent_client_protocol::Error::internal_error().data(error.message));
    }
    if response.stop_reason == StopReason::EndTurn
        && !client.current_turn_had_activity()
        && let Some(message) = config.empty_end_turn_auth_error
    {
        return Err(agent_client_protocol::Error::auth_required().data(message));
    }
    output
        .send(AcpEvent::Done(
            serde_json::to_string(&response.stop_reason).unwrap_or_default(),
        ))
        .await
        .map_err(|_| agent_client_protocol::Error::internal_error())?;
    Ok(())
}

fn structured_prompt_blocks(prompt: ExecutorPrompt) -> (Vec<ContentBlock>, String) {
    let display_text = prompt.text.clone();
    let mut blocks = Vec::with_capacity(1 + prompt.images.len());
    blocks.push(ContentBlock::Text(TextContent::new(prompt.text)));
    blocks.extend(prompt.images.into_iter().map(|image| {
        ContentBlock::Image(ImageContent::new(image.data, image.mime_type).uri(image.uri))
    }));
    (blocks, display_text)
}

async fn apply_session_preferences(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    preferences: &AcpSessionPreferences,
    options: &mut Vec<SessionConfigOption>,
    legacy_models: Option<&LegacySessionModelState>,
) -> agent_client_protocol::Result<Option<String>> {
    if let Some(required_mode) = &preferences.required_session_mode {
        let option = options
            .iter()
            .find(|option| option.id.0.as_ref() == required_mode.option_id)
            .cloned()
            .ok_or_else(|| {
                invalid_config(format!(
                    "required ACP session mode `{}` was not advertised",
                    required_mode.option_id
                ))
            })?;
        if !protocol_option_controls_session_mode(&option) {
            return Err(invalid_config(format!(
                "required ACP session mode `{}` does not identify a mode option",
                required_mode.option_id
            )));
        }
        if !config_value_supported(&option, &required_mode.value) {
            return Err(invalid_config(format!(
                "required ACP session mode value for `{}` was not advertised",
                required_mode.option_id
            )));
        }
        set_config_option_and_verify(
            connection,
            session_id,
            &option,
            required_mode.value.clone(),
            options,
        )
        .await?;
    }

    if options.is_empty() {
        return apply_legacy_session_preferences(
            connection,
            session_id,
            preferences,
            legacy_models,
        )
        .await;
    }

    let category_preferences = [
        (
            SessionConfigOptionCategory::Model,
            preferences.model.as_ref(),
            "model",
        ),
        (
            SessionConfigOptionCategory::ThoughtLevel,
            preferences.thought_level.as_ref(),
            "thought level",
        ),
    ];
    let mut applied_overrides = vec![false; preferences.options.len()];

    for (category, desired, label) in category_preferences {
        let category_override_indices = preferences
            .options
            .iter()
            .enumerate()
            .filter_map(|(index, selection)| {
                options
                    .iter()
                    .find(|option| option.id.0.as_ref() == selection.option_id)
                    .is_some_and(|option| option_matches_category(option, &category))
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        if !category_override_indices.is_empty() {
            for index in category_override_indices {
                let selection = &preferences.options[index];
                let option = validated_config_option_for_selection(options, selection)?;
                set_config_option_and_verify(
                    connection,
                    session_id,
                    &option,
                    selection.value.clone(),
                    options,
                )
                .await?;
                applied_overrides[index] = true;
            }
            continue;
        }
        let Some(desired) = desired else {
            continue;
        };
        let Some(option) = find_category_option(options, &category) else {
            if category == SessionConfigOptionCategory::ThoughtLevel
                && preferences.native_thought_level_fallback
            {
                continue;
            }
            return Err(invalid_config(format!(
                "ACP {label} preference `{desired}` cannot be applied: the Agent did not advertise one unambiguous option"
            )));
        };
        let value = resolve_preference_value(&option, desired, &category).ok_or_else(|| {
            invalid_config(format!(
                "ACP {label} preference `{desired}` was not advertised or matched multiple values"
            ))
        })?;
        set_config_option_and_verify(connection, session_id, &option, value, options).await?;
    }

    for (index, selection) in preferences.options.iter().enumerate() {
        if applied_overrides[index] {
            continue;
        }
        let option = validated_config_option_for_selection(options, selection)?;
        set_config_option_and_verify(
            connection,
            session_id,
            &option,
            selection.value.clone(),
            options,
        )
        .await?;
    }
    Ok(effective_model_from_options(options))
}

fn validated_config_option_for_selection(
    options: &[SessionConfigOption],
    selection: &super::AcpConfigSelection,
) -> agent_client_protocol::Result<SessionConfigOption> {
    let option = options
        .iter()
        .find(|option| option.id.0.as_ref() == selection.option_id)
        .cloned()
        .ok_or_else(|| {
            invalid_config(format!(
                "ACP config option `{}` was not advertised",
                selection.option_id
            ))
        })?;
    if protocol_option_controls_session_mode(&option) {
        return Err(invalid_config(format!(
            "ACP config option `{}` controls the reserved session mode",
            selection.option_id
        )));
    }
    if !config_value_supported(&option, &selection.value) {
        return Err(invalid_config(format!(
            "ACP config value for `{}` was not advertised",
            selection.option_id
        )));
    }
    Ok(option)
}

async fn apply_legacy_session_preferences(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    preferences: &AcpSessionPreferences,
    legacy_models: Option<&LegacySessionModelState>,
) -> agent_client_protocol::Result<Option<String>> {
    if preferences.thought_level.is_some() && !preferences.native_thought_level_fallback {
        return Err(invalid_config(
            "ACP Agent only advertises the legacy model selector; other requested session preferences are unsupported",
        ));
    }
    let mut explicit_model = None;
    for selection in &preferences.options {
        if selection.option_id != "model" {
            return Err(invalid_config(format!(
                "ACP config option `{}` is unavailable because the Agent only advertises the legacy model selector",
                selection.option_id
            )));
        }
        let Some(value) = selection.value.as_value_id() else {
            return Err(invalid_config(
                "legacy ACP model selection requires a value_id",
            ));
        };
        explicit_model = Some(value.0.to_string());
    }
    let Some(desired) = explicit_model.or_else(|| preferences.model.clone()) else {
        return Ok(legacy_models.map(|models| models.current_model_id.clone()));
    };
    let models = legacy_models.ok_or_else(|| {
        invalid_config(format!(
            "ACP model preference `{desired}` cannot be applied: the Agent advertises neither configOptions nor legacy models"
        ))
    })?;
    let selected = resolve_legacy_model(models, &desired).ok_or_else(|| {
        invalid_config(format!(
            "ACP model preference `{desired}` was not advertised or matched multiple legacy models"
        ))
    })?;
    if selected == models.current_model_id {
        return Ok(Some(selected));
    }
    let request = UntypedMessage::new(
        "session/set_model",
        serde_json::json!({
            "sessionId": session_id.0.as_ref(),
            "modelId": selected,
        }),
    )?;
    connection.send_request(request).block_task().await?;
    Ok(Some(selected))
}

fn effective_model_from_options(options: &[SessionConfigOption]) -> Option<String> {
    let option = find_category_option(options, &SessionConfigOptionCategory::Model)?;
    let SessionConfigKind::Select(select) = option.kind else {
        return None;
    };
    Some(select.current_value.0.to_string())
}

fn resolve_legacy_model(state: &LegacySessionModelState, desired: &str) -> Option<String> {
    if state.current_model_id == desired {
        return Some(state.current_model_id.clone());
    }
    if let Some(model) = state
        .available_models
        .iter()
        .find(|model| model.model_id == desired)
    {
        return Some(model.model_id.clone());
    }

    let mut best: Option<(u8, &str)> = None;
    let mut ambiguous = false;
    for model in &state.available_models {
        let score = model_id_match_score(desired, &model.model_id).or_else(|| {
            model_id_match_score(desired, &model.name).map(|score| score.saturating_sub(10))
        });
        let Some(score) = score else {
            continue;
        };
        match best {
            Some((best_score, _)) if score < best_score => {}
            Some((best_score, _)) if score == best_score => ambiguous = true,
            _ => {
                best = Some((score, model.model_id.as_str()));
                ambiguous = false;
            }
        }
    }
    (!ambiguous)
        .then(|| best.map(|(_, model_id)| model_id.to_string()))
        .flatten()
}

fn find_category_option(
    options: &[SessionConfigOption],
    category: &SessionConfigOptionCategory,
) -> Option<SessionConfigOption> {
    let exact = options
        .iter()
        .filter(|option| option.category.as_ref() == Some(category))
        .collect::<Vec<_>>();
    if exact.len() == 1 {
        return Some(exact[0].clone());
    }
    if !exact.is_empty() {
        return None;
    }
    let semantic = options
        .iter()
        .filter(|option| option_matches_category(option, category))
        .collect::<Vec<_>>();
    (semantic.len() == 1).then(|| semantic[0].clone())
}

fn option_matches_category(
    option: &SessionConfigOption,
    category: &SessionConfigOptionCategory,
) -> bool {
    if option.category.as_ref() == Some(category) {
        return true;
    }
    let expected = match category {
        SessionConfigOptionCategory::Model => "model",
        SessionConfigOptionCategory::ThoughtLevel => "thoughtlevel",
        _ => return false,
    };
    [&*option.id.0, option.name.as_str()].iter().any(|value| {
        let key = semantic_config_key(value);
        key == expected || key.ends_with(expected)
    })
}

fn semantic_config_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn protocol_option_controls_session_mode(option: &SessionConfigOption) -> bool {
    let category_is_mode = option.category.as_ref().is_some_and(|category| {
        serde_json::to_value(category)
            .ok()
            .and_then(|value| value.as_str().map(is_session_mode_key))
            .unwrap_or(false)
    });
    category_is_mode
        || is_session_mode_key(option.id.0.as_ref())
        || is_session_mode_key(&option.name)
}

async fn set_config_option_and_verify(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    option: &SessionConfigOption,
    value: SessionConfigOptionValue,
    options: &mut Vec<SessionConfigOption>,
) -> agent_client_protocol::Result<()> {
    let response = connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id.clone(),
            option.id.clone(),
            value.clone(),
        ))
        .block_task()
        .await?;
    let effective = response
        .config_options
        .iter()
        .find(|candidate| candidate.id == option.id)
        .ok_or_else(|| {
            invalid_config(format!(
                "ACP Agent omitted config option `{}` after setting it",
                option.id
            ))
        })?;
    if !config_current_value_matches(effective, &value) {
        return Err(invalid_config(format!(
            "ACP config option `{}` requested {}, but the Agent activated {}; the requested value may be unsupported",
            option.id,
            config_value_display(&value),
            config_current_value_display(effective),
        )));
    }
    *options = response.config_options;
    Ok(())
}

fn config_value_display(value: &SessionConfigOptionValue) -> String {
    match value {
        SessionConfigOptionValue::ValueId { value } => format!("`{value}`"),
        SessionConfigOptionValue::Boolean { value } => format!("`{value}`"),
        _ => "an unsupported value type".to_string(),
    }
}

fn config_current_value_display(option: &SessionConfigOption) -> String {
    match &option.kind {
        SessionConfigKind::Select(select) => format!("`{}`", select.current_value),
        SessionConfigKind::Boolean(boolean) => format!("`{}`", boolean.current_value),
        _ => "an unsupported value type".to_string(),
    }
}

fn config_current_value_matches(
    option: &SessionConfigOption,
    value: &SessionConfigOptionValue,
) -> bool {
    match (&option.kind, value) {
        (SessionConfigKind::Select(select), SessionConfigOptionValue::ValueId { value }) => {
            select.current_value == *value
        }
        (SessionConfigKind::Boolean(boolean), SessionConfigOptionValue::Boolean { value }) => {
            boolean.current_value == *value
        }
        _ => false,
    }
}

fn invalid_config(message: impl Into<String>) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(message.into())
}

fn config_value_supported(option: &SessionConfigOption, value: &SessionConfigOptionValue) -> bool {
    match (&option.kind, value) {
        (SessionConfigKind::Select(select), SessionConfigOptionValue::ValueId { value }) => {
            match &select.options {
                SessionConfigSelectOptions::Ungrouped(options) => {
                    options.iter().any(|option| option.value == *value)
                }
                SessionConfigSelectOptions::Grouped(groups) => groups
                    .iter()
                    .flat_map(|group| group.options.iter())
                    .any(|option| option.value == *value),
                _ => false,
            }
        }
        (SessionConfigKind::Boolean(_), SessionConfigOptionValue::Boolean { .. }) => true,
        _ => false,
    }
}

fn resolve_preference_value(
    option: &SessionConfigOption,
    desired: &str,
    category: &SessionConfigOptionCategory,
) -> Option<SessionConfigOptionValue> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let candidates = match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect::<Vec<_>>(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .collect(),
        _ => return None,
    };

    if let Some(exact) = candidates
        .iter()
        .find(|candidate| candidate.value.0.as_ref() == desired)
    {
        return Some(SessionConfigOptionValue::value_id(exact.value.clone()));
    }
    if category != &SessionConfigOptionCategory::Model {
        return None;
    }

    let mut best: Option<(u8, &str)> = None;
    let mut ambiguous = false;
    for candidate in candidates {
        let score = model_id_match_score(desired, candidate.value.0.as_ref()).or_else(|| {
            model_id_match_score(desired, candidate.name.as_ref())
                .map(|score| score.saturating_sub(10))
        });
        let Some(score) = score else {
            continue;
        };
        match best {
            Some((best_score, _)) if score < best_score => {}
            Some((best_score, _)) if score == best_score => ambiguous = true,
            _ => {
                best = Some((score, candidate.value.0.as_ref()));
                ambiguous = false;
            }
        }
    }

    (!ambiguous)
        .then(|| best.map(|(_, value)| SessionConfigOptionValue::value_id(value.to_string())))
        .flatten()
}

type StartupSender =
    Arc<StdMutex<Option<tokio::sync::oneshot::Sender<Result<(), BootstrapError>>>>>;

fn send_startup(sender: &StartupSender, result: Result<(), BootstrapError>) {
    if let Some(sender) = sender.lock().expect("ACP startup mutex poisoned").take() {
        let _ = sender.send(result);
    }
}

fn protocol_error_message(error: &agent_client_protocol::Error) -> String {
    match &error.data {
        Some(data) => format!("{error}: {data}"),
        None => error.to_string(),
    }
}

fn is_invalid_session_recovery_error(error: &agent_client_protocol::Error) -> bool {
    error.code == agent_client_protocol::ErrorCode::InvalidParams
        && error
            .data
            .as_ref()
            .is_some_and(|data| data.as_str() == Some(INVALID_SESSION_RECOVERY_MESSAGE))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::SessionConfigSelectOption;

    use super::*;
    use crate::executors::ExecutorPromptImage;

    #[test]
    fn structured_prompt_preserves_text_and_image_blocks() {
        let (blocks, display) = structured_prompt_blocks(ExecutorPrompt {
            text: "inspect this".to_string(),
            images: vec![ExecutorPromptImage {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
                uri: Some("attachment.png".to_string()),
            }],
        });
        assert_eq!(display, "inspect this");
        assert!(matches!(&blocks[0], ContentBlock::Text(text) if text.text == "inspect this"));
        assert!(matches!(&blocks[1], ContentBlock::Image(image)
            if image.mime_type == "image/png" && image.uri.as_deref() == Some("attachment.png")));
    }

    #[test]
    fn config_value_must_be_advertised_by_select() {
        let option = SessionConfigOption::select(
            "model",
            "Model",
            "default",
            vec![
                SessionConfigSelectOption::new("default", "Default"),
                SessionConfigSelectOption::new("fast", "Fast"),
            ],
        );
        assert!(config_value_supported(
            &option,
            &SessionConfigOptionValue::value_id("fast")
        ));
        assert!(!config_value_supported(
            &option,
            &SessionConfigOptionValue::value_id("missing")
        ));
    }

    #[test]
    fn config_value_shape_must_match_option_kind() {
        let option = SessionConfigOption::boolean("thinking", "Thinking", true);
        assert!(config_value_supported(
            &option,
            &SessionConfigOptionValue::boolean(false)
        ));
        assert!(!config_value_supported(
            &option,
            &SessionConfigOptionValue::value_id("false")
        ));
    }

    #[test]
    fn protocol_mode_category_is_reserved_even_with_an_unrelated_id() {
        let option = SessionConfigOption::select(
            "execution-profile",
            "Profile",
            "default",
            vec![
                SessionConfigSelectOption::new("default", "Default"),
                SessionConfigSelectOption::new("yolo", "YOLO"),
            ],
        )
        .category(SessionConfigOptionCategory::Other("mode".into()));

        assert!(protocol_option_controls_session_mode(&option));
    }

    #[test]
    fn native_thought_level_fallback_keeps_acp_preference_optional() {
        let harness = AcpAgentHarness::new().with_native_thought_level_fallback("high");

        assert_eq!(
            harness.config.session.thought_level.as_deref(),
            Some("high")
        );
        assert!(harness.config.session.native_thought_level_fallback);
    }

    #[test]
    fn resume_policy_is_an_explicit_adapter_capability() {
        let harness =
            AcpAgentHarness::new().with_resume_policy(AcpResumePolicy::RefusalMeansInvalidSession);

        assert_eq!(
            harness.config.resume_policy,
            AcpResumePolicy::RefusalMeansInvalidSession
        );
        assert_eq!(
            AcpAgentHarness::new().config.resume_policy,
            AcpResumePolicy::PreserveRefusal
        );

        let harness =
            AcpAgentHarness::new().with_resume_policy(AcpResumePolicy::UnknownSessionStartsNew);
        assert_eq!(
            harness.config.resume_policy,
            AcpResumePolicy::UnknownSessionStartsNew
        );
    }

    #[test]
    fn empty_end_turn_auth_error_is_an_explicit_adapter_capability() {
        let harness = AcpAgentHarness::new()
            .with_empty_end_turn_auth_error("fixture authentication guidance");

        assert_eq!(
            harness.empty_end_turn_auth_error(),
            Some("fixture authentication guidance")
        );
        assert_eq!(AcpAgentHarness::new().empty_end_turn_auth_error(), None);
    }

    #[test]
    fn unknown_session_detection_is_narrow_to_resume_lookup_errors() {
        let unknown =
            agent_client_protocol::Error::new(-32602, "Unknown sessionId: session_fixture");
        let missing = agent_client_protocol::Error::resource_not_found(None);
        let unrelated = agent_client_protocol::Error::invalid_params().data("invalid cwd");

        assert!(is_unknown_session_error(&unknown));
        assert!(is_unknown_session_error(&missing));
        assert!(!is_unknown_session_error(&unrelated));
    }

    #[test]
    fn model_preference_resolves_unique_provider_qualified_value() {
        let option = SessionConfigOption::select(
            "model",
            "Model",
            "default",
            vec![
                SessionConfigSelectOption::new("default", "Default"),
                SessionConfigSelectOption::new("gpt-5.6-luna(openai)", "GPT 5.6 Luna"),
            ],
        );

        assert_eq!(
            resolve_preference_value(&option, "gpt-5.6-luna", &SessionConfigOptionCategory::Model,),
            Some(SessionConfigOptionValue::value_id("gpt-5.6-luna(openai)"))
        );
    }

    #[test]
    fn model_preference_rejects_ambiguous_adaptive_matches() {
        let option = SessionConfigOption::select(
            "model",
            "Model",
            "default",
            vec![
                SessionConfigSelectOption::new("gpt-5.6-luna(openai)", "GPT 5.6 Luna (OpenAI)"),
                SessionConfigSelectOption::new("gpt-5.6-luna(other)", "GPT 5.6 Luna (Other)"),
            ],
        );

        assert_eq!(
            resolve_preference_value(&option, "gpt-5.6-luna", &SessionConfigOptionCategory::Model,),
            None
        );
    }

    #[test]
    fn parses_legacy_models_without_reconstructing_route_ids() {
        let state = parse_session_start_response(
            "session/new",
            serde_json::json!({
                "sessionId": "session-1",
                "models": {
                    "currentModelId": "gemini-3.1-pro-preview",
                    "availableModels": [{
                        "modelId": "gpt-5.6-luna(openai)",
                        "name": "GPT 5.6 Luna"
                    }]
                }
            }),
            None,
        )
        .expect("legacy session response should parse");

        let models = state.legacy_models.expect("legacy models");
        assert_eq!(
            resolve_legacy_model(&models, "gpt-5.6-luna").as_deref(),
            Some("gpt-5.6-luna(openai)")
        );
        assert_eq!(
            legacy_model_config_snapshot(&models).kind,
            AcpConfigOptionKind::Select {
                current_value: "gemini-3.1-pro-preview".to_string(),
                options: vec![
                    AcpConfigChoice {
                        value: "gpt-5.6-luna(openai)".to_string(),
                        name: "GPT 5.6 Luna".to_string(),
                        description: None,
                    },
                    AcpConfigChoice {
                        value: "gemini-3.1-pro-preview".to_string(),
                        name: "gemini-3.1-pro-preview".to_string(),
                        description: None,
                    },
                ],
            }
        );
    }

    #[test]
    fn marks_missing_resume_session_id_when_using_requested_id_fallback() {
        let state = parse_session_start_response(
            "session/resume",
            serde_json::json!({
                "models": {"currentModelId": "model-1", "availableModels": []}
            }),
            Some(SessionId::new("session-1")),
        )
        .expect("resume response should use the requested ID for the wire follow-up");

        assert_eq!(state.session_id.0.as_ref(), "session-1");
        assert!(state.session_id_was_fallback);
    }

    #[test]
    fn legacy_model_resolution_fails_when_canonical_value_is_ambiguous() {
        let state = LegacySessionModelState {
            current_model_id: "default".to_string(),
            available_models: vec![
                LegacyModelInfo {
                    model_id: "gpt-5.6-luna(openai)".to_string(),
                    name: "OpenAI".to_string(),
                    description: None,
                },
                LegacyModelInfo {
                    model_id: "gpt-5.6-luna(other)".to_string(),
                    name: "Other".to_string(),
                    description: None,
                },
            ],
        };

        assert_eq!(resolve_legacy_model(&state, "gpt-5.6-luna"), None);
    }

    #[test]
    fn effective_model_uses_category_semantics_when_category_is_absent() {
        let option = SessionConfigOption::select(
            "session-model",
            "Session model",
            "gpt-5.6-luna(openai)",
            vec![SessionConfigSelectOption::new(
                "gpt-5.6-luna(openai)",
                "GPT 5.6 Luna",
            )],
        );

        assert_eq!(
            effective_model_from_options(&[option]).as_deref(),
            Some("gpt-5.6-luna(openai)")
        );
    }

    #[test]
    fn effective_value_must_equal_the_value_acknowledged_by_agent() {
        let option = SessionConfigOption::boolean("thinking", "Thinking", true);
        assert!(config_current_value_matches(
            &option,
            &SessionConfigOptionValue::boolean(true)
        ));
        assert!(!config_current_value_matches(
            &option,
            &SessionConfigOptionValue::boolean(false)
        ));
    }
}
