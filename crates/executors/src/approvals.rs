use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use workspace_utils::approvals::ApprovalStatus;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutorApprovalOption {
    pub option_id: String,
    pub kind: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutorApprovalRequest {
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_call_id: String,
    pub options: Vec<ExecutorApprovalOption>,
}

/// Errors emitted by executor approval services.
#[derive(Debug, Error)]
pub enum ExecutorApprovalError {
    #[error("executor approval session not registered")]
    SessionNotRegistered,
    #[error("executor approval request failed: {0}")]
    RequestFailed(String),
    #[error("executor approval service unavailable")]
    ServiceUnavailable,
    #[error("executor approval request cancelled")]
    Cancelled,
}

impl ExecutorApprovalError {
    pub fn request_failed<E: fmt::Display>(err: E) -> Self {
        Self::RequestFailed(err.to_string())
    }
}

/// Abstraction for executor approval backends.
#[async_trait]
pub trait ExecutorApprovalService: Send + Sync {
    /// Requests approval for a tool invocation and waits for the final decision.
    ///
    /// The `cancel` token allows the caller to cancel the approval request early.
    /// When cancelled, implementations should return `ExecutorApprovalError::Cancelled`.
    async fn request_tool_approval(
        &self,
        tool_name: &str,
        tool_input: Value,
        tool_call_id: &str,
        cancel: CancellationToken,
    ) -> Result<ApprovalStatus, ExecutorApprovalError>;

    /// Requests an ACP permission decision while preserving the Agent's opaque
    /// option IDs. Non-ACP backends keep working through the compatibility
    /// implementation below.
    async fn request_acp_tool_approval(
        &self,
        request: ExecutorApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<String, ExecutorApprovalError> {
        let status = self
            .request_tool_approval(
                &request.tool_name,
                request.tool_input,
                &request.tool_call_id,
                cancel,
            )
            .await?;
        let accepted_kinds: &[&str] = match status {
            ApprovalStatus::Approved => &["allow_once", "allow_always"],
            ApprovalStatus::Denied { .. } => &["reject_once", "reject_always"],
            ApprovalStatus::TimedOut | ApprovalStatus::Pending => {
                return Err(ExecutorApprovalError::Cancelled);
            }
        };
        request
            .options
            .iter()
            .find(|option| accepted_kinds.contains(&option.kind.as_str()))
            .map(|option| option.option_id.clone())
            .ok_or(ExecutorApprovalError::Cancelled)
    }
}

#[derive(Debug, Default)]
pub struct NoopExecutorApprovalService;

#[async_trait]
impl ExecutorApprovalService for NoopExecutorApprovalService {
    async fn request_tool_approval(
        &self,
        _tool_name: &str,
        _tool_input: Value,
        _tool_call_id: &str,
        _cancel: CancellationToken,
    ) -> Result<ApprovalStatus, ExecutorApprovalError> {
        Ok(ApprovalStatus::Approved)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallMetadata {
    pub tool_call_id: String,
}
