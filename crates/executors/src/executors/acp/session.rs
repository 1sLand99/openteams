use std::path::PathBuf;

use agent_client_protocol::{
    Agent, ConnectionTo,
    schema::v1::{
        AgentCapabilities, AuthMethod, CloseSessionRequest, CloseSessionResponse,
        DeleteSessionRequest, DeleteSessionResponse, Implementation, InitializeResponse,
        ListSessionsRequest, ListSessionsResponse, SessionId,
    },
};

/// Immutable initialize result used to gate optional ACP lifecycle methods.
#[derive(Debug, Clone)]
pub struct AcpNegotiatedState {
    pub agent_info: Option<Implementation>,
    pub auth_methods: Vec<AuthMethod>,
    pub agent_capabilities: AgentCapabilities,
}

impl AcpNegotiatedState {
    pub fn from_initialize(response: &InitializeResponse) -> Self {
        Self {
            agent_info: response.agent_info.clone(),
            auth_methods: response.auth_methods.clone(),
            agent_capabilities: response.agent_capabilities.clone(),
        }
    }

    pub fn advertises_auth_method(&self, method_id: &str) -> bool {
        self.auth_methods
            .iter()
            .any(|method| method.id().0.as_ref() == method_id)
    }

    pub async fn list_sessions(
        &self,
        connection: &ConnectionTo<Agent>,
        cwd: Option<PathBuf>,
        cursor: Option<String>,
    ) -> agent_client_protocol::Result<ListSessionsResponse> {
        if self.agent_capabilities.session_capabilities.list.is_none() {
            return Err(unsupported_method("session/list"));
        }
        connection
            .send_request(ListSessionsRequest::new().cwd(cwd).cursor(cursor))
            .block_task()
            .await
    }

    pub async fn close_session(
        &self,
        connection: &ConnectionTo<Agent>,
        session_id: SessionId,
    ) -> agent_client_protocol::Result<CloseSessionResponse> {
        if self.agent_capabilities.session_capabilities.close.is_none() {
            return Err(unsupported_method("session/close"));
        }
        connection
            .send_request(CloseSessionRequest::new(session_id))
            .block_task()
            .await
    }

    pub async fn delete_session(
        &self,
        connection: &ConnectionTo<Agent>,
        session_id: SessionId,
    ) -> agent_client_protocol::Result<DeleteSessionResponse> {
        if self
            .agent_capabilities
            .session_capabilities
            .delete
            .is_none()
        {
            return Err(unsupported_method("session/delete"));
        }
        connection
            .send_request(DeleteSessionRequest::new(session_id))
            .block_task()
            .await
    }
}

fn unsupported_method(method: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::method_not_found()
        .data(format!("ACP Agent did not advertise {method}"))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::{
        ProtocolVersion,
        v1::{
            AuthMethodAgent, SessionCapabilities, SessionCloseCapabilities,
            SessionDeleteCapabilities, SessionListCapabilities,
        },
    };

    use super::*;

    #[test]
    fn initialize_snapshot_preserves_identity_auth_and_capabilities() {
        let response = InitializeResponse::new(ProtocolVersion::V1)
            .agent_info(Implementation::new("agent", "1.0"))
            .auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                "browser", "Browser",
            ))])
            .agent_capabilities(
                AgentCapabilities::new().session_capabilities(
                    SessionCapabilities::new()
                        .list(SessionListCapabilities::new())
                        .close(SessionCloseCapabilities::new())
                        .delete(SessionDeleteCapabilities::new()),
                ),
            );
        let snapshot = AcpNegotiatedState::from_initialize(&response);
        assert_eq!(
            snapshot.agent_info.as_ref().map(|info| info.name.as_str()),
            Some("agent")
        );
        assert!(snapshot.advertises_auth_method("browser"));
        assert!(!snapshot.advertises_auth_method("missing"));
        assert!(
            snapshot
                .agent_capabilities
                .session_capabilities
                .list
                .is_some()
        );
    }
}
