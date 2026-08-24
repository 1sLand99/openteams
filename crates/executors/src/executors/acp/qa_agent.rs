//! Deterministic ACP Agent fixture used only by `qa-mode` acceptance tests.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{
    Agent, Client, ConnectionTo, Dispatch, Stdio,
    schema::{
        ProtocolVersion,
        v1::{
            AgentCapabilities, AuthMethod, AuthMethodAgent, AuthenticateRequest,
            AuthenticateResponse, CancelNotification, ContentBlock, ContentChunk,
            InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
            McpServer, MessageId, NewSessionRequest, NewSessionResponse, PermissionOption,
            PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse,
            RequestPermissionOutcome, RequestPermissionRequest, ResumeSessionRequest,
            ResumeSessionResponse, SessionCapabilities, SessionConfigOption,
            SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOption,
            SessionNotification, SessionResumeCapabilities, SessionUpdate,
            SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, TextContent,
            ToolCallUpdate, ToolCallUpdateFields, Usage, UsageUpdate, WriteTextFileRequest,
        },
    },
};
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
struct QaSession {
    cwd: PathBuf,
    mcp_names: Vec<String>,
    model: String,
    thought_level: String,
    mode: String,
}

fn model_config_option(current_model: &str) -> SessionConfigOption {
    SessionConfigOption::select(
        "session-model",
        "Model",
        current_model.to_string(),
        vec![
            SessionConfigSelectOption::new("gpt-5.6-luna(openai)", "GPT 5.6 Luna (OpenAI)"),
            SessionConfigSelectOption::new("gemini-2.5-flash", "Gemini 2.5 Flash"),
            SessionConfigSelectOption::new("gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview"),
        ],
    )
    .category(SessionConfigOptionCategory::Model)
}

fn mode_config_option(current_mode: &str) -> SessionConfigOption {
    SessionConfigOption::select(
        "mode",
        "Mode",
        current_mode.to_string(),
        vec![
            SessionConfigSelectOption::new("default", "Default"),
            SessionConfigSelectOption::new("auto", "Auto"),
            SessionConfigSelectOption::new("yolo", "YOLO"),
        ],
    )
    .category(SessionConfigOptionCategory::Other("mode".into()))
}

fn thought_levels_for_model(model: &str) -> Vec<SessionConfigSelectOption> {
    let levels = if model == "gpt-5.6-luna(openai)" {
        vec!["low", "high", "max", "on"]
    } else {
        vec!["on"]
    };
    levels
        .into_iter()
        .map(|level| SessionConfigSelectOption::new(level, format!("Thinking {level}")))
        .collect()
}

fn thought_config_option(session: &QaSession) -> SessionConfigOption {
    SessionConfigOption::select(
        "thinking",
        "Thinking",
        session.thought_level.clone(),
        thought_levels_for_model(&session.model),
    )
    .category(SessionConfigOptionCategory::ThoughtLevel)
}

fn session_config_options(
    session: &QaSession,
    advertise_model: bool,
    advertise_thought: bool,
    advertise_mode: bool,
) -> Vec<SessionConfigOption> {
    let mut options = Vec::new();
    if advertise_model {
        options.push(model_config_option(&session.model));
    }
    if advertise_thought {
        options.push(thought_config_option(session));
    }
    if advertise_mode {
        options.push(mode_config_option(&session.mode));
    }
    options
}

fn mcp_names(servers: &[McpServer]) -> Vec<String> {
    servers
        .iter()
        .filter_map(|server| match server {
            McpServer::Stdio(server) => Some(server.name.clone()),
            McpServer::Http(server) => Some(server.name.clone()),
            McpServer::Sse(server) => Some(server.name.clone()),
            _ => None,
        })
        .collect()
}

fn session_key(session_id: &agent_client_protocol::schema::v1::SessionId) -> String {
    session_id.0.to_string()
}

fn send_replayed_session_history(
    connection: &ConnectionTo<Client>,
    session_id: &agent_client_protocol::schema::v1::SessionId,
    replay_count: usize,
) -> agent_client_protocol::Result<()> {
    for index in 0..replay_count {
        connection.send_notification(SessionNotification::new(
            session_id.clone(),
            SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(format!(
                    "replayed-history-{index:04}-{}",
                    "x".repeat(128)
                ))))
                .message_id(MessageId::new(format!("replayed-message-{index:04}"))),
            ),
        ))?;
    }
    Ok(())
}

/// Serve the fake Agent over stdio. The prompt can select deterministic
/// scenarios with `[qa:write]`, `[qa:approval]`, `[qa:sleep]`, `[qa:empty]`
/// and `[qa:error]`.
pub async fn run_stdio_agent() -> agent_client_protocol::Result<()> {
    let disable_follow_up = std::env::var_os("ACP_QA_DISABLE_FOLLOW_UP").is_some();
    let require_auth = std::env::var_os("ACP_QA_REQUIRE_AUTH").is_some();
    let expire_auth = std::env::var_os("ACP_QA_EXPIRE_AUTH").is_some();
    let advertise_config = std::env::var_os("ACP_QA_CONFIG_OPTIONS").is_some();
    let advertise_thought = std::env::var_os("ACP_QA_THOUGHT_OPTIONS").is_some();
    let advertise_mode = std::env::var_os("ACP_QA_MODE_OPTIONS").is_some();
    let refuse_mode_set = std::env::var_os("ACP_QA_REFUSE_MODE_SET").is_some();
    let replay_count = std::env::var("ACP_QA_REPLAY_NOTIFICATION_COUNT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let authenticated = Arc::new(AtomicBool::new(false));
    let cancel_requested = Arc::new(AtomicBool::new(false));
    let cancel_notify = Arc::new(Notify::new());
    let sessions = Arc::new(Mutex::new(HashMap::<String, QaSession>::new()));

    let authenticated_for_initialize = authenticated.clone();
    let authenticated_for_auth = authenticated.clone();
    let authenticated_for_new = authenticated.clone();
    let authenticated_for_resume = authenticated.clone();
    let authenticated_for_load = authenticated.clone();
    let sessions_for_new = sessions.clone();
    let sessions_for_resume = sessions.clone();
    let sessions_for_load = sessions.clone();
    let resume_replay_count = replay_count;
    let load_replay_count = replay_count;
    let sessions_for_config = sessions.clone();
    let sessions_for_prompt = sessions.clone();
    let cancel_requested_for_notification = cancel_requested.clone();
    let cancel_notify_for_notification = cancel_notify.clone();
    let cancel_requested_for_prompt = cancel_requested.clone();
    let cancel_notify_for_prompt = cancel_notify.clone();

    Agent
        .builder()
        .name("openteams-acp-qa-agent")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                if request.protocol_version != ProtocolVersion::V1 {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::invalid_request());
                }
                let capabilities = if disable_follow_up {
                    AgentCapabilities::new()
                } else {
                    AgentCapabilities::new()
                        .prompt_capabilities(PromptCapabilities::new().image(true))
                        .load_session(true)
                        .session_capabilities(
                            SessionCapabilities::new().resume(SessionResumeCapabilities::new()),
                        )
                };
                let mut response =
                    InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(capabilities);
                if require_auth {
                    response = response.auth_methods(vec![AuthMethod::Agent(
                        AuthMethodAgent::new("qa-auth", "QA Authentication"),
                    )]);
                } else {
                    authenticated_for_initialize.store(true, Ordering::SeqCst);
                }
                responder.respond(response)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_notification: CancelNotification, _connection| {
                cancel_requested_for_notification.store(true, Ordering::SeqCst);
                cancel_notify_for_notification.notify_waiters();
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: AuthenticateRequest, responder, _connection| {
                if request.method_id.0.as_ref() != "qa-auth" || expire_auth {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::auth_required());
                }
                authenticated_for_auth.store(true, Ordering::SeqCst);
                responder.respond(AuthenticateResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, _connection| {
                if !authenticated_for_new.load(Ordering::SeqCst) {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::auth_required());
                }
                let session_id = format!("qa-{}", uuid::Uuid::new_v4());
                let session = QaSession {
                    cwd: request.cwd,
                    mcp_names: mcp_names(&request.mcp_servers),
                    model: "gemini-3.1-pro-preview".to_string(),
                    thought_level: "on".to_string(),
                    mode: "yolo".to_string(),
                };
                let config_options = session_config_options(
                    &session,
                    advertise_config,
                    advertise_thought,
                    advertise_mode,
                );
                sessions_for_new
                    .lock()
                    .await
                    .insert(session_id.clone(), session);
                let response = NewSessionResponse::new(session_id);
                responder.respond(if config_options.is_empty() {
                    response
                } else {
                    response.config_options(config_options)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, connection| {
                if !authenticated_for_resume.load(Ordering::SeqCst) {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::auth_required());
                }
                let session = QaSession {
                    cwd: request.cwd,
                    mcp_names: mcp_names(&request.mcp_servers),
                    model: "gemini-3.1-pro-preview".to_string(),
                    thought_level: "on".to_string(),
                    mode: "yolo".to_string(),
                };
                let config_options = session_config_options(
                    &session,
                    advertise_config,
                    advertise_thought,
                    advertise_mode,
                );
                sessions_for_resume
                    .lock()
                    .await
                    .insert(session_key(&request.session_id), session);
                send_replayed_session_history(
                    &connection,
                    &request.session_id,
                    resume_replay_count,
                )?;
                let response = ResumeSessionResponse::new();
                responder.respond(if config_options.is_empty() {
                    response
                } else {
                    response.config_options(config_options)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, connection| {
                if !authenticated_for_load.load(Ordering::SeqCst) {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::auth_required());
                }
                let session = QaSession {
                    cwd: request.cwd,
                    mcp_names: mcp_names(&request.mcp_servers),
                    model: "gemini-3.1-pro-preview".to_string(),
                    thought_level: "on".to_string(),
                    mode: "yolo".to_string(),
                };
                let config_options = session_config_options(
                    &session,
                    advertise_config,
                    advertise_thought,
                    advertise_mode,
                );
                sessions_for_load
                    .lock()
                    .await
                    .insert(session_key(&request.session_id), session);
                send_replayed_session_history(
                    &connection,
                    &request.session_id,
                    load_replay_count,
                )?;
                let response = LoadSessionResponse::new();
                responder.respond(if config_options.is_empty() {
                    response
                } else {
                    response.config_options(config_options)
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: SetSessionConfigOptionRequest, responder, _connection| {
                let SessionConfigOptionValue::ValueId { value } = request.value else {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::invalid_params());
                };
                let selected = value.0.to_string();
                let mut sessions = sessions_for_config.lock().await;
                let Some(session) = sessions.get_mut(&session_key(&request.session_id)) else {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::invalid_params());
                };
                match request.config_id.0.as_ref() {
                    "session-model"
                        if [
                            "gpt-5.6-luna(openai)",
                            "gemini-2.5-flash",
                            "gemini-3.1-pro-preview",
                        ]
                        .contains(&selected.as_str()) =>
                    {
                        session.model.clone_from(&selected);
                    }
                    "thinking"
                        if thought_levels_for_model(&session.model)
                            .iter()
                            .any(|level| level.value.0.as_ref() == selected.as_str()) =>
                    {
                        session.thought_level.clone_from(&selected);
                    }
                    "mode" if ["default", "auto", "yolo"].contains(&selected.as_str()) => {
                        if !refuse_mode_set {
                            session.mode.clone_from(&selected);
                        }
                    }
                    _ => {
                        return responder
                            .respond_with_error(agent_client_protocol::Error::invalid_params());
                    }
                }
                responder.respond(SetSessionConfigOptionResponse::new(
                    session_config_options(
                        session,
                        advertise_config,
                        advertise_thought,
                        advertise_mode,
                    ),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection: ConnectionTo<Client>| {
                let task_connection = connection.clone();
                let sessions_for_prompt = sessions_for_prompt.clone();
                let cancel_requested_for_prompt = cancel_requested_for_prompt.clone();
                let cancel_notify_for_prompt = cancel_notify_for_prompt.clone();
                connection.spawn(async move {
                    let connection = task_connection;
                    let text = request
                        .prompt
                        .iter()
                        .find_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    if text.contains("[qa:error]") {
                        return responder
                            .respond_with_error(agent_client_protocol::Error::invalid_request());
                    }
                    if text.contains("[qa:exit]") {
                        std::process::exit(17);
                    }
                    if text.contains("[qa:sleep]") {
                        if !cancel_requested_for_prompt.load(Ordering::SeqCst) {
                            tokio::select! {
                                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                                _ = cancel_notify_for_prompt.notified() => {}
                            }
                        }
                        if cancel_requested_for_prompt.load(Ordering::SeqCst) {
                            return responder.respond(PromptResponse::new(StopReason::Cancelled));
                        }
                    }

                    let session = sessions_for_prompt
                        .lock()
                        .await
                        .get(&session_key(&request.session_id))
                        .cloned();
                    if text.contains("[qa:write]")
                        && let Some(session) = &session
                    {
                        connection
                            .send_request(WriteTextFileRequest::new(
                                request.session_id.clone(),
                                session.cwd.join("qa-changed.txt"),
                                "written by ACP QA fixture",
                            ))
                            .block_task()
                            .await?;
                    }
                    let mut approval = "not_requested".to_string();
                    if text.contains("[qa:approval]") {
                        let response = connection
                            .send_request(RequestPermissionRequest::new(
                                request.session_id.clone(),
                                ToolCallUpdate::new(
                                    "qa-tool",
                                    ToolCallUpdateFields::new().title("QA approval"),
                                ),
                                vec![
                                    PermissionOption::new(
                                        "allow-once",
                                        "Allow once",
                                        PermissionOptionKind::AllowOnce,
                                    ),
                                    PermissionOption::new(
                                        "reject-once",
                                        "Reject once",
                                        PermissionOptionKind::RejectOnce,
                                    ),
                                ],
                            ))
                            .block_task()
                            .await?;
                        approval = match response.outcome {
                            RequestPermissionOutcome::Selected(selected) => {
                                selected.option_id.0.to_string()
                            }
                            RequestPermissionOutcome::Cancelled => "cancelled".to_string(),
                            _ => "unknown".to_string(),
                        };
                    }

                    connection.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::UsageUpdate(UsageUpdate::new(37, 128)),
                    ))?;
                    if text.contains("[qa:empty]") {
                        return responder.respond(
                            PromptResponse::new(StopReason::EndTurn).usage(Usage::new(37, 30, 7)),
                        );
                    }
                    let (mcp, model, thought_level, mode) = session
                        .map(|session| {
                            (
                                session.mcp_names.join(","),
                                session.model,
                                session.thought_level,
                                session.mode,
                            )
                        })
                        .unwrap_or_default();
                    let content = if text.contains("[OPENTEAMS_SOURCE=openteams]") {
                        serde_json::json!([{
                            "type": "send",
                            "to": "you",
                            "intent": "reply",
                            "content": format!(
                                "ACP QA completed; approval={approval}; mcp={mcp}"
                            )
                        }])
                        .to_string()
                    } else {
                        format!(
                            "QA ACP response: {text}; approval={approval}; mcp={mcp}; model={model}; thought={thought_level}; mode={mode}"
                        )
                    };
                    connection.send_notification(SessionNotification::new(
                        request.session_id.clone(),
                        SessionUpdate::AgentMessageChunk(
                            ContentChunk::new(ContentBlock::Text(TextContent::new(content)))
                                .message_id("qa-message"),
                        ),
                    ))?;
                    connection.send_notification(SessionNotification::new(
                        request.session_id,
                        SessionUpdate::AgentMessageChunk(
                            ContentChunk::new(ContentBlock::Text(TextContent::new(" ")))
                                .message_id("qa-message"),
                        ),
                    ))?;
                    responder.respond(
                        PromptResponse::new(StopReason::EndTurn).usage(Usage::new(37, 30, 7)),
                    )
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, connection: ConnectionTo<Client>| match message {
                Dispatch::Response(result, router) => router.respond_with_result(result),
                message => message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled QA ACP message"),
                    connection,
                ),
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_with(Stdio::new(), async |connection| {
            connection.incoming_closed().await;
            Ok(())
        })
        .await
}
