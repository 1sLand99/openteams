use agent_client_protocol::schema::v1::{SessionNotification, SessionUpdate};
use serde::{Deserialize, Serialize};

use super::AcpEvent;

/// Ordered product event emitted by one ACP connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcpRuntimeEvent {
    pub connection_id: String,
    pub session_id: Option<String>,
    pub sequence: u64,
    #[serde(default)]
    pub message_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub payload: AcpEvent,
}

/// Convert a protocol notification exactly once at the ACP boundary.
pub fn event_from_notification(notification: SessionNotification) -> AcpEvent {
    match notification.update {
        SessionUpdate::UserMessageChunk(chunk) => AcpEvent::UserBlock(chunk),
        SessionUpdate::AgentMessageChunk(chunk) => AcpEvent::Message(chunk),
        SessionUpdate::AgentThoughtChunk(chunk) => AcpEvent::Thought(chunk),
        SessionUpdate::ToolCall(tool_call) => AcpEvent::ToolCall(tool_call),
        SessionUpdate::ToolCallUpdate(update) => AcpEvent::ToolUpdate(update),
        SessionUpdate::Plan(plan) => AcpEvent::Plan(plan),
        SessionUpdate::AvailableCommandsUpdate(update) => {
            AcpEvent::AvailableCommands(update.available_commands)
        }
        SessionUpdate::CurrentModeUpdate(update) => AcpEvent::CurrentMode(update.current_mode_id),
        SessionUpdate::ConfigOptionUpdate(update) => AcpEvent::ConfigOptions(update.config_options),
        SessionUpdate::SessionInfoUpdate(update) => AcpEvent::SessionInfo(update),
        SessionUpdate::UsageUpdate(update) => AcpEvent::Usage(update),
        _ => AcpEvent::Other(notification),
    }
}
