use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex as StdMutex},
};

use agent_client_protocol::{
    Agent, ConnectionTo, Lines,
    schema::{
        ProtocolVersion,
        v1::{
            AuthenticateRequest, BooleanConfigOptionCapabilities, CancelNotification,
            ClientCapabilities, ClientSessionCapabilities, CreateTerminalRequest,
            FileSystemCapabilities, Implementation, InitializeRequest, KillTerminalRequest,
            LoadSessionRequest, McpServer, NewSessionRequest, PromptRequest, ReadTextFileRequest,
            ReleaseTerminalRequest, RequestPermissionRequest, ResumeSessionRequest,
            SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
            SessionConfigOptionValue, SessionConfigOptionsCapabilities, SessionConfigSelectOptions,
            SessionId, SessionNotification, SetSessionConfigOptionRequest, TerminalOutputRequest,
            TextContent, WaitForTerminalExitRequest, WriteTextFileRequest,
        },
    },
};
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use futures::{AsyncBufReadExt, sink};
use tokio::{io::AsyncWriteExt, process::Command};
use tokio_util::{compat::TokioAsyncReadCompatExt, sync::CancellationToken};

use super::{
    AcpApprovalPolicy, AcpClient, AcpEvent, AcpRunConfig, AcpSessionPreferences,
    mcp::validate_mcp_servers, output::AcpOutput,
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandParts},
    env::ExecutionEnv,
    executors::{ExecutorError, ExecutorExitResult, SpawnedChild},
};

#[derive(Debug)]
enum BootstrapError {
    FollowUpNotSupported(String),
    AuthRequired(String),
    Other(String),
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

    pub fn with_mode(mut self, mode: impl Into<String>) -> Self {
        self.config.session.mode = Some(mode.into());
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
        self.spawn_internal(
            current_dir,
            prompt,
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
        self.spawn_internal(
            current_dir,
            prompt,
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
        prompt: String,
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
        self.bootstrap_acp_connection(
            &mut child,
            current_dir.to_path_buf(),
            existing_session,
            prompt,
            exit_tx,
            approvals,
            env.vars.clone(),
            cancel.clone(),
        )
        .await?;

        Ok(SpawnedChild {
            child,
            exit_signal: Some(exit_rx),
            cancel: Some(cancel),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn bootstrap_acp_connection(
        &self,
        child: &mut AsyncGroupChild,
        cwd: PathBuf,
        existing_session: Option<String>,
        prompt: String,
        exit_signal: tokio::sync::oneshot::Sender<ExecutorExitResult>,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
        terminal_env: std::collections::HashMap<String, String>,
        cancel: CancellationToken,
    ) -> Result<(), ExecutorError> {
        let stdout = child.inner().stdout.take().ok_or_else(|| {
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
        let output_writer = crate::stdout_dup::create_stdout_pipe_writer(child)?;
        let (output, output_task) = AcpOutput::start(output_writer);
        let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
        let startup_tx = Arc::new(StdMutex::new(Some(startup_tx)));

        let config = self.config.clone();
        let output_for_runtime = output.clone();
        tokio::task::spawn_blocking(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build ACP runtime");
            runtime.block_on(async move {
                let incoming = futures::io::BufReader::new(stdout.compat()).lines();
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
                    cancel.clone(),
                    cwd.clone(),
                    config.additional_directories.clone(),
                    config.client_services,
                    terminal_env,
                );
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
                let cancel_for_connection = cancel.clone();

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
                let was_cancelled = cancel.is_cancelled();

                if let Err(error) = &result
                    && !was_cancelled
                {
                    let startup_error =
                        if error.code == agent_client_protocol::ErrorCode::AuthRequired {
                            BootstrapError::AuthRequired(error.to_string())
                        } else {
                            BootstrapError::Other(error.to_string())
                        };
                    send_startup(&startup_tx, Err(startup_error));
                    let _ = output_for_runtime
                        .send(AcpEvent::Error(protocol_error_message(error)))
                        .await;
                }

                drop(output_for_runtime);
                if let Err(error) = output_task.await {
                    tracing::error!("ACP output task failed: {error}");
                }
                let _ = exit_signal.send(if result.is_ok() || was_cancelled {
                    ExecutorExitResult::Success
                } else {
                    ExecutorExitResult::Failure
                });
            });
        });

        match startup_rx.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(BootstrapError::FollowUpNotSupported(message))) => {
                Err(ExecutorError::FollowUpNotSupported(message))
            }
            Ok(Err(BootstrapError::AuthRequired(message))) => {
                Err(ExecutorError::AuthRequired(message))
            }
            Ok(Err(BootstrapError::Other(message))) => Err(ExecutorError::Io(
                std::io::Error::other(format!("ACP startup failed: {message}")),
            )),
            Err(_) => Err(ExecutorError::Io(std::io::Error::other(
                "ACP startup task exited before initialization",
            ))),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_connection(
    connection: &ConnectionTo<Agent>,
    client: &AcpClient,
    cwd: &Path,
    existing_session: Option<String>,
    prompt: String,
    config: AcpRunConfig,
    startup_tx: StartupSender,
    output: AcpOutput,
    cancel: CancellationToken,
) -> agent_client_protocol::Result<()> {
    let client_capabilities = ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(config.client_services.read_text_file)
            .write_text_file(config.client_services.write_text_file))
        .terminal(config.client_services.terminal)
        .session(ClientSessionCapabilities::new().config_options(
            SessionConfigOptionsCapabilities::new().boolean(BooleanConfigOptionCapabilities::new()),
        ));
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

    let (session_id, mut config_options) = match existing_session {
        None => {
            let response = connection
                .send_request(
                    NewSessionRequest::new(cwd)
                        .additional_directories(config.additional_directories.clone())
                        .mcp_servers(config.mcp_servers.clone()),
                )
                .block_task()
                .await?;
            (
                response.session_id,
                response.config_options.unwrap_or_default(),
            )
        }
        Some(existing) => {
            let session_id = SessionId::new(existing);
            if negotiated
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some()
            {
                let response = connection
                    .send_request(
                        ResumeSessionRequest::new(session_id.clone(), cwd)
                            .additional_directories(config.additional_directories.clone())
                            .mcp_servers(config.mcp_servers.clone()),
                    )
                    .block_task()
                    .await?;
                (session_id, response.config_options.unwrap_or_default())
            } else if negotiated.agent_capabilities.load_session {
                let response = connection
                    .send_request(
                        LoadSessionRequest::new(session_id.clone(), cwd)
                            .additional_directories(config.additional_directories.clone())
                            .mcp_servers(config.mcp_servers.clone()),
                    )
                    .block_task()
                    .await?;
                (session_id, response.config_options.unwrap_or_default())
            } else {
                let message =
                    "Agent advertises neither session/resume nor session/load".to_string();
                send_startup(
                    &startup_tx,
                    Err(BootstrapError::FollowUpNotSupported(message.clone())),
                );
                return Err(agent_client_protocol::Error::method_not_found().data(message));
            }
        }
    };

    output
        .send(AcpEvent::SessionStart(session_id.0.to_string()))
        .await
        .map_err(|_| agent_client_protocol::Error::internal_error())?;
    apply_session_preferences(
        connection,
        &session_id,
        &config.session,
        &mut config_options,
        &output,
    )
    .await?;
    client.record_user_prompt_event(&prompt).await;
    send_startup(&startup_tx, Ok(()));

    let request = connection
        .send_request(PromptRequest::new(
            session_id.clone(),
            vec![agent_client_protocol::schema::v1::ContentBlock::Text(
                TextContent::new(prompt),
            )],
        ))
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
    output
        .send(AcpEvent::Done(
            serde_json::to_string(&response.stop_reason).unwrap_or_default(),
        ))
        .await
        .map_err(|_| agent_client_protocol::Error::internal_error())?;
    Ok(())
}

async fn apply_session_preferences(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    preferences: &AcpSessionPreferences,
    options: &mut Vec<SessionConfigOption>,
    output: &AcpOutput,
) -> agent_client_protocol::Result<()> {
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
        (
            SessionConfigOptionCategory::Mode,
            preferences.mode.as_ref(),
            "mode",
        ),
    ];

    for (category, desired, label) in category_preferences {
        let Some(desired) = desired else {
            continue;
        };
        let matches = options
            .iter()
            .filter(|option| option.category.as_ref() == Some(&category))
            .cloned()
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            let _ = output
                .send(AcpEvent::Warning(format!(
                    "ACP {label} preference ignored: expected one matching config option, found {}",
                    matches.len()
                )))
                .await;
            continue;
        }
        let value = SessionConfigOptionValue::value_id(desired.clone());
        if !config_value_supported(&matches[0], &value) {
            let _ = output
                .send(AcpEvent::Warning(format!(
                    "ACP {label} preference `{desired}` was not advertised"
                )))
                .await;
            continue;
        }
        let response = connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                matches[0].id.clone(),
                value,
            ))
            .block_task()
            .await?;
        *options = response.config_options;
    }

    for selection in &preferences.options {
        let Some(option) = options
            .iter()
            .find(|option| option.id.0.as_ref() == selection.option_id)
        else {
            let _ = output
                .send(AcpEvent::Warning(format!(
                    "ACP config option `{}` was not advertised",
                    selection.option_id
                )))
                .await;
            continue;
        };
        if !config_value_supported(option, &selection.value) {
            let _ = output
                .send(AcpEvent::Warning(format!(
                    "ACP config value for `{}` was not advertised",
                    selection.option_id
                )))
                .await;
            continue;
        }
        let response = connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id.clone(),
                selection.value.clone(),
            ))
            .block_task()
            .await?;
        *options = response.config_options;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::SessionConfigSelectOption;

    use super::*;

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
}
