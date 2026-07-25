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
            AuthenticateResponse, ContentBlock, ContentChunk, InitializeRequest,
            InitializeResponse, LoadSessionRequest, LoadSessionResponse, McpServer,
            NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
            PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest,
            ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionNotification,
            SessionResumeCapabilities, SessionUpdate, StopReason, TextContent, ToolCallUpdate,
            ToolCallUpdateFields, UsageUpdate, WriteTextFileRequest,
        },
    },
};
use tokio::sync::Mutex;

#[derive(Clone)]
struct QaSession {
    cwd: PathBuf,
    mcp_names: Vec<String>,
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

/// Serve the fake Agent over stdio. The prompt can select deterministic
/// scenarios with `[qa:write]`, `[qa:approval]`, `[qa:sleep]` and `[qa:error]`.
pub async fn run_stdio_agent() -> agent_client_protocol::Result<()> {
    let disable_follow_up = std::env::var_os("ACP_QA_DISABLE_FOLLOW_UP").is_some();
    let require_auth = std::env::var_os("ACP_QA_REQUIRE_AUTH").is_some();
    let authenticated = Arc::new(AtomicBool::new(false));
    let sessions = Arc::new(Mutex::new(HashMap::<String, QaSession>::new()));

    let authenticated_for_initialize = authenticated.clone();
    let authenticated_for_auth = authenticated.clone();
    let authenticated_for_new = authenticated.clone();
    let authenticated_for_resume = authenticated.clone();
    let authenticated_for_load = authenticated.clone();
    let sessions_for_new = sessions.clone();
    let sessions_for_resume = sessions.clone();
    let sessions_for_load = sessions.clone();
    let sessions_for_prompt = sessions.clone();

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
        .on_receive_request(
            async move |request: AuthenticateRequest, responder, _connection| {
                if request.method_id.0.as_ref() != "qa-auth" {
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
                sessions_for_new.lock().await.insert(
                    session_id.clone(),
                    QaSession {
                        cwd: request.cwd,
                        mcp_names: mcp_names(&request.mcp_servers),
                    },
                );
                responder.respond(NewSessionResponse::new(session_id))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ResumeSessionRequest, responder, _connection| {
                if !authenticated_for_resume.load(Ordering::SeqCst) {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::auth_required());
                }
                sessions_for_resume.lock().await.insert(
                    session_key(&request.session_id),
                    QaSession {
                        cwd: request.cwd,
                        mcp_names: mcp_names(&request.mcp_servers),
                    },
                );
                responder.respond(ResumeSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, _connection| {
                if !authenticated_for_load.load(Ordering::SeqCst) {
                    return responder
                        .respond_with_error(agent_client_protocol::Error::auth_required());
                }
                sessions_for_load.lock().await.insert(
                    session_key(&request.session_id),
                    QaSession {
                        cwd: request.cwd,
                        mcp_names: mcp_names(&request.mcp_servers),
                    },
                );
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection: ConnectionTo<Client>| {
                let task_connection = connection.clone();
                let sessions_for_prompt = sessions_for_prompt.clone();
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
                        tokio::time::sleep(Duration::from_secs(30)).await;
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
                    let mcp = session
                        .map(|session| session.mcp_names.join(","))
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
                        format!("QA ACP response: {text}; approval={approval}; mcp={mcp}")
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
                    responder.respond(PromptResponse::new(StopReason::EndTurn))
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
