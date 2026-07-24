use agent_client_protocol::{
    Agent, Client, ConnectionTo, Dispatch, Stdio,
    schema::{
        ProtocolVersion,
        v1::{
            AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
            LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse,
            PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
            SessionCapabilities, SessionNotification, SessionResumeCapabilities, SessionUpdate,
            StopReason, TextContent,
        },
    },
};

#[tokio::main]
async fn main() -> agent_client_protocol::Result<()> {
    let disable_follow_up = std::env::var_os("ACP_QA_DISABLE_FOLLOW_UP").is_some();
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
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(capabilities),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: NewSessionRequest, responder, _connection| {
                responder.respond(NewSessionResponse::new(format!(
                    "qa-{}",
                    uuid::Uuid::new_v4()
                )))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: ResumeSessionRequest, responder, _connection| {
                responder.respond(ResumeSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: LoadSessionRequest, responder, _connection| {
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection: ConnectionTo<Client>| {
                let text = request
                    .prompt
                    .iter()
                    .find_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .unwrap_or_default();
                connection.send_notification(SessionNotification::new(
                    request.session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(format!("QA ACP response: {text}")),
                    ))),
                ))?;
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_dispatch(
            async move |message: Dispatch, connection: ConnectionTo<Client>| {
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled QA ACP message"),
                    connection,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
}
