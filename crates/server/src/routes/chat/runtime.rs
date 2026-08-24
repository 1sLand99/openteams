use std::collections::HashMap;

use axum::{
    Extension,
    extract::{Query, State},
    response::Json as ResponseJson,
};
use chrono::{DateTime, Utc};
use db::models::{
    chat_agent::ChatAgent, chat_message::ChatMessage, chat_message_queue::QueuedMessageStatus,
    chat_run::ChatRun, chat_session::ChatSession, chat_session_agent::ChatSessionAgent,
    project_member::ProjectMember,
};
use deployment::Deployment;
use serde::{Deserialize, Serialize};
use services::services::{
    chat::should_include_message_in_history,
    chat_runtime_outbox::{CHAT_RUNTIME_REPLAY_LIMIT, ChatRuntimeDelta, ChatRuntimeOutboxService},
    member_execution::resolve_effective_member_execution_config,
    queued_message::{MemberQueueSnapshot, QueuedMessageService},
};
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ChatActiveRunStatus {
    Starting,
    Running,
    Stopping,
    WaitingApproval,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ChatActiveRun {
    pub delivery_id: Uuid,
    pub run_id: Uuid,
    pub session_id: Uuid,
    pub session_agent_id: Uuid,
    pub agent_id: Uuid,
    pub agent_name: String,
    pub display_name: String,
    pub avatar: String,
    pub model: Option<String>,
    pub status: ChatActiveRunStatus,
    pub source_message_id: Option<Uuid>,
    pub client_message_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ChatSessionRuntimeSnapshot {
    pub session_id: Uuid,
    pub revision: i64,
    pub messages: Option<Vec<ChatMessage>>,
    pub active_runs: Vec<ChatActiveRun>,
    pub queues: Vec<MemberQueueSnapshot>,
}

#[derive(Debug, Clone, Deserialize, TS)]
#[ts(export)]
pub struct ChatSessionRuntimeQuery {
    pub include_messages: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, TS)]
#[ts(export)]
pub struct ChatRuntimeReplayQuery {
    pub after_revision: i64,
    pub limit: Option<i64>,
    pub include_messages: Option<bool>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct ChatRuntimeReplayResponse {
    pub session_id: Uuid,
    pub after_revision: i64,
    pub current_revision: i64,
    pub next_revision: i64,
    pub has_more: bool,
    pub events: Vec<ChatRuntimeDelta>,
    pub snapshot: Option<ChatSessionRuntimeSnapshot>,
}

pub async fn get_session_runtime_snapshot(
    Extension(session): Extension<ChatSession>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<ChatSessionRuntimeQuery>,
) -> Result<ResponseJson<ApiResponse<ChatSessionRuntimeSnapshot>>, ApiError> {
    let snapshot = build_session_runtime_snapshot(
        &deployment,
        &session,
        query.include_messages.unwrap_or(false),
        None,
    )
    .await?;
    Ok(ResponseJson(ApiResponse::success(snapshot)))
}

pub async fn replay_session_runtime(
    Extension(session): Extension<ChatSession>,
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<ChatRuntimeReplayQuery>,
) -> Result<ResponseJson<ApiResponse<ChatRuntimeReplayResponse>>, ApiError> {
    let limit = query.limit.unwrap_or(CHAT_RUNTIME_REPLAY_LIMIT);
    if !(1..=CHAT_RUNTIME_REPLAY_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!(
            "runtime replay limit must be between 1 and {CHAT_RUNTIME_REPLAY_LIMIT}"
        )));
    }
    let page = ChatRuntimeOutboxService::new()
        .replay_after(
            &deployment.db().pool,
            session.id,
            query.after_revision,
            limit,
        )
        .await?;

    let snapshot = if page.requires_snapshot {
        Some(
            build_session_runtime_snapshot(
                &deployment,
                &session,
                query.include_messages.unwrap_or(false),
                None,
            )
            .await?,
        )
    } else {
        None
    };
    let next_revision = snapshot
        .as_ref()
        .map(|snapshot| snapshot.revision)
        .or_else(|| page.events.last().map(|event| event.revision))
        .unwrap_or(query.after_revision);
    let has_more = snapshot.is_none() && next_revision < page.current_revision;

    Ok(ResponseJson(ApiResponse::success(
        ChatRuntimeReplayResponse {
            session_id: session.id,
            after_revision: query.after_revision,
            current_revision: page.current_revision,
            next_revision,
            has_more,
            events: page.events,
            snapshot,
        },
    )))
}

pub async fn build_session_runtime_snapshot(
    deployment: &DeploymentImpl,
    session: &ChatSession,
    include_messages: bool,
    source_message: Option<&ChatMessage>,
) -> Result<ChatSessionRuntimeSnapshot, ApiError> {
    let queue_service = QueuedMessageService::new();
    for _ in 0..3 {
        let revision = queue_service
            .current_runtime_revision(&deployment.db().pool, session.id)
            .await?;
        let snapshot = build_session_runtime_snapshot_once(
            deployment,
            session,
            include_messages,
            source_message,
            revision,
        )
        .await?;
        let current_revision = queue_service
            .current_runtime_revision(&deployment.db().pool, session.id)
            .await?;
        if current_revision == revision {
            return Ok(snapshot);
        }
    }

    Err(ApiError::Conflict(
        "Chat runtime changed while the snapshot was being built; retry the request.".to_string(),
    ))
}

async fn build_session_runtime_snapshot_once(
    deployment: &DeploymentImpl,
    session: &ChatSession,
    include_messages: bool,
    source_message: Option<&ChatMessage>,
    revision: i64,
) -> Result<ChatSessionRuntimeSnapshot, ApiError> {
    let pool = &deployment.db().pool;
    let session_agents = ChatSessionAgent::find_all_for_session(pool, session.id).await?;
    let agents = ChatAgent::find_visible_for_project(pool, session.project_id).await?;
    let project_members = match session.project_id {
        Some(project_id) => ProjectMember::find_by_project(pool, project_id).await?,
        None => Vec::new(),
    };
    let agent_by_id: HashMap<Uuid, ChatAgent> =
        agents.into_iter().map(|agent| (agent.id, agent)).collect();
    let project_member_by_id: HashMap<Uuid, ProjectMember> = project_members
        .iter()
        .cloned()
        .map(|member| (member.id, member))
        .collect();
    let project_member_name_by_agent_id: HashMap<Uuid, String> = project_members
        .into_iter()
        .filter_map(|member| {
            let agent_id = member.agent_id?;
            let name = member.member_name?.trim().to_string();
            if name.is_empty() {
                None
            } else {
                Some((agent_id, name))
            }
        })
        .collect();

    let queue_service = QueuedMessageService::new();
    let mut queues = Vec::with_capacity(session_agents.len());
    for session_agent in &session_agents {
        queues.push(
            queue_service
                .snapshot_for_member(pool, session.id, session_agent.id, session_agent.agent_id)
                .await?,
        );
    }

    let mut active_runs = Vec::new();
    for session_agent in &session_agents {
        let Some(delivery) = queues
            .iter()
            .find(|queue| queue.session_agent_id == session_agent.id)
            .and_then(|queue| {
                queue
                    .items
                    .iter()
                    .map(|item| &item.message)
                    .find(|message| message.status.is_active() && message.run_id.is_some())
            })
        else {
            continue;
        };
        let Some(status) = active_run_status(delivery.status) else {
            continue;
        };
        let Some(run_id) = delivery.run_id else {
            continue;
        };
        let Some(run) = ChatRun::find_by_id(pool, run_id)
            .await?
            .filter(|run| run.session_id == session.id && run.session_agent_id == session_agent.id)
        else {
            continue;
        };
        let agent = agent_by_id.get(&session_agent.agent_id);
        let display_name = display_name_for_session_agent(
            session_agent,
            agent,
            &project_member_by_id,
            &project_member_name_by_agent_id,
        );
        let agent_name = agent
            .map(|agent| agent.name.clone())
            .unwrap_or_else(|| display_name.trim_start_matches('@').to_string());
        let (source_message_id, client_message_id) =
            source_message_identity(pool, source_message, session.id, delivery.chat_message_id)
                .await?;

        active_runs.push(ChatActiveRun {
            delivery_id: delivery.id,
            run_id: run.id,
            session_id: session.id,
            session_agent_id: session_agent.id,
            agent_id: session_agent.agent_id,
            agent_name,
            display_name: ensure_agent_handle(&display_name),
            avatar: monogram_from_name(&display_name),
            model: active_run_model(agent, session_agent),
            status,
            source_message_id,
            client_message_id,
            created_at: run.created_at,
        });
    }

    active_runs.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.run_id.cmp(&b.run_id))
    });

    let messages = if include_messages {
        Some(
            ChatMessage::find_by_session_id_lightweight(pool, session.id, None)
                .await?
                .into_iter()
                .filter(should_include_message_in_history)
                .collect(),
        )
    } else {
        None
    };

    Ok(ChatSessionRuntimeSnapshot {
        session_id: session.id,
        revision,
        messages,
        active_runs,
        queues,
    })
}

fn active_run_status(status: QueuedMessageStatus) -> Option<ChatActiveRunStatus> {
    match status {
        QueuedMessageStatus::Starting | QueuedMessageStatus::Processing => {
            Some(ChatActiveRunStatus::Starting)
        }
        QueuedMessageStatus::Running => Some(ChatActiveRunStatus::Running),
        QueuedMessageStatus::Stopping => Some(ChatActiveRunStatus::Stopping),
        QueuedMessageStatus::WaitingApproval => Some(ChatActiveRunStatus::WaitingApproval),
        QueuedMessageStatus::Queued
        | QueuedMessageStatus::Failed
        | QueuedMessageStatus::Cancelled
        | QueuedMessageStatus::Skipped
        | QueuedMessageStatus::Completed => None,
    }
}

fn active_run_model(agent: Option<&ChatAgent>, session_agent: &ChatSessionAgent) -> Option<String> {
    let agent = agent?;
    match resolve_effective_member_execution_config(agent, session_agent) {
        Ok(config) => config.model_name,
        Err(err) => {
            tracing::warn!(
                agent_id = %agent.id,
                session_agent_id = %session_agent.id,
                error = %err,
                "Failed to resolve active run model from member execution config"
            );
            agent.model_name.clone()
        }
    }
}

fn display_name_for_session_agent(
    session_agent: &ChatSessionAgent,
    agent: Option<&ChatAgent>,
    project_member_by_id: &HashMap<Uuid, ProjectMember>,
    project_member_name_by_agent_id: &HashMap<Uuid, String>,
) -> String {
    session_agent
        .project_member_id
        .and_then(|project_member_id| project_member_by_id.get(&project_member_id))
        .and_then(|member| member.member_name.as_deref())
        .filter(|name| !name.trim().is_empty())
        .map(ToString::to_string)
        .or_else(|| {
            project_member_name_by_agent_id
                .get(&session_agent.agent_id)
                .cloned()
        })
        .or_else(|| agent.map(|agent| agent.name.clone()))
        .unwrap_or_else(|| session_agent.agent_id.to_string())
}

fn ensure_agent_handle(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.starts_with('@') {
        trimmed.to_string()
    } else {
        format!("@{trimmed}")
    }
}

fn monogram_from_name(name: &str) -> String {
    let monogram: String = name
        .trim_start_matches('@')
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .take(2)
        .collect::<String>()
        .to_ascii_uppercase();
    if monogram.is_empty() {
        "AG".to_string()
    } else {
        monogram
    }
}

async fn source_message_identity(
    pool: &sqlx::SqlitePool,
    source_message: Option<&ChatMessage>,
    session_id: Uuid,
    chat_message_id: Uuid,
) -> Result<(Option<Uuid>, Option<String>), sqlx::Error> {
    let message = match source_message
        .filter(|message| message.session_id == session_id && message.id == chat_message_id)
    {
        Some(message) => Some(message.clone()),
        None => ChatMessage::find_by_id(pool, chat_message_id).await?,
    };
    let Some(message) = message.filter(|message| message.session_id == session_id) else {
        return Ok((None, None));
    };
    let client_message_id = message
        .meta
        .get("client_message_id")
        .and_then(|value| value.as_str())
        .map(ToString::to_string);
    Ok((Some(message.id), client_message_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_run_projection_serializes_stable_delivery_id() {
        let delivery_id = Uuid::new_v4();
        let value = serde_json::to_value(ChatActiveRun {
            delivery_id,
            run_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            session_agent_id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            agent_name: "Agent".to_string(),
            display_name: "@Agent".to_string(),
            avatar: "A".to_string(),
            model: Some("test-model".to_string()),
            status: ChatActiveRunStatus::Running,
            source_message_id: Some(Uuid::new_v4()),
            client_message_id: Some("client-message".to_string()),
            created_at: Utc::now(),
        })
        .expect("serialize active run projection");

        assert_eq!(value["delivery_id"], serde_json::json!(delivery_id));
    }

    #[test]
    fn active_run_projection_uses_persisted_delivery_status() {
        assert!(matches!(
            active_run_status(QueuedMessageStatus::Starting),
            Some(ChatActiveRunStatus::Starting)
        ));
        assert!(matches!(
            active_run_status(QueuedMessageStatus::Running),
            Some(ChatActiveRunStatus::Running)
        ));
        assert!(matches!(
            active_run_status(QueuedMessageStatus::WaitingApproval),
            Some(ChatActiveRunStatus::WaitingApproval)
        ));
        assert!(matches!(
            active_run_status(QueuedMessageStatus::Stopping),
            Some(ChatActiveRunStatus::Stopping)
        ));
        assert!(active_run_status(QueuedMessageStatus::Completed).is_none());
        assert!(active_run_status(QueuedMessageStatus::Failed).is_none());
    }
}
