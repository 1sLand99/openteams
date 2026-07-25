pub mod client;
pub mod config;
pub mod events;
pub mod harness;
pub mod mcp;
pub mod normalize_logs;
pub mod output;
#[cfg(feature = "qa-mode")]
pub mod qa;
#[cfg(feature = "qa-mode")]
pub mod qa_agent;
pub mod runtime;
pub mod session;

use std::{fmt::Display, str::FromStr};

pub use client::AcpClient;
pub use config::{
    AcpApprovalPolicy, AcpClientServicePolicy, AcpConfigSelection, AcpRunConfig,
    AcpSessionPreferences,
};
pub use harness::AcpAgentHarness;
pub use normalize_logs::*;
#[cfg(feature = "qa-mode")]
pub use qa::AcpQaExecutor;
use serde::{Deserialize, Serialize};
pub use session::AcpNegotiatedState;
use workspace_utils::approvals::ApprovalStatus;

/// Parsed event types for internal processing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AcpEvent {
    User(String),
    UserBlock(agent_client_protocol::schema::v1::ContentChunk),
    SessionStart(String),
    Message(agent_client_protocol::schema::v1::ContentChunk),
    Thought(agent_client_protocol::schema::v1::ContentChunk),
    ToolCall(agent_client_protocol::schema::v1::ToolCall),
    ToolUpdate(agent_client_protocol::schema::v1::ToolCallUpdate),
    Plan(agent_client_protocol::schema::v1::Plan),
    AvailableCommands(Vec<agent_client_protocol::schema::v1::AvailableCommand>),
    CurrentMode(agent_client_protocol::schema::v1::SessionModeId),
    ConfigOptions(Vec<agent_client_protocol::schema::v1::SessionConfigOption>),
    SessionInfo(agent_client_protocol::schema::v1::SessionInfoUpdate),
    Usage(agent_client_protocol::schema::v1::UsageUpdate),
    RequestPermission(agent_client_protocol::schema::v1::RequestPermissionRequest),
    ApprovalResponse(ApprovalResponse),
    Warning(String),
    Error(String),
    Done(String),
    Other(agent_client_protocol::schema::v1::SessionNotification),
}

impl Display for AcpEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", serde_json::to_string(self).unwrap_or_default())
    }
}

impl FromStr for AcpEvent {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub tool_call_id: String,
    pub status: ApprovalStatus,
}
