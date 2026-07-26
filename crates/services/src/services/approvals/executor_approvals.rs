use std::{
    sync::{Arc, LazyLock},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{TimeDelta, Utc};
use dashmap::{DashMap, mapref::entry::Entry};
use db::{
    DBService,
    models::{
        chat_executor_approval_request::{
            ChatExecutorApprovalOption, ChatExecutorApprovalRequest, ChatExecutorApprovalStatus,
            CreateChatExecutorApprovalRequest,
        },
        chat_session_agent::ChatSessionAgentState,
    },
};
use executors::approvals::{
    ExecutorApprovalError, ExecutorApprovalOption, ExecutorApprovalRequest, ExecutorApprovalService,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;
use utils::approvals::{ApprovalRequest, ApprovalStatus};
use uuid::Uuid;

use crate::services::inbox::InboxService;

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_DISPLAY_INPUT_BYTES: usize = 16 * 1024;

static WAITERS: LazyLock<DashMap<Uuid, oneshot::Sender<String>>> = LazyLock::new(DashMap::new);
static EVENTS: LazyLock<DashMap<Uuid, broadcast::Sender<ExecutorApprovalEvent>>> =
    LazyLock::new(DashMap::new);

#[derive(Debug, Clone)]
pub struct ExecutorApprovalScope {
    pub session_id: Uuid,
    pub session_agent_id: Uuid,
    pub run_id: Uuid,
    pub runner: String,
    pub workflow_execution_id: Option<Uuid>,
    pub workflow_step_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutorApprovalEvent {
    ExecutorApprovalRequested {
        session_id: Uuid,
        request_id: Uuid,
        session_agent_id: Uuid,
        request: ChatExecutorApprovalRequest,
    },
    ExecutorApprovalResolved {
        session_id: Uuid,
        request_id: Uuid,
        session_agent_id: Uuid,
        request: ChatExecutorApprovalRequest,
    },
    ExecutorApprovalCancelled {
        session_id: Uuid,
        request_id: Uuid,
        session_agent_id: Uuid,
        request: ChatExecutorApprovalRequest,
    },
    ExecutorApprovalExpired {
        session_id: Uuid,
        request_id: Uuid,
        session_agent_id: Uuid,
        request: ChatExecutorApprovalRequest,
    },
}

#[derive(Clone)]
pub struct ExecutorApprovalBridge {
    db: DBService,
    scope: ExecutorApprovalScope,
}

impl ExecutorApprovalBridge {
    pub fn new(db: DBService, scope: ExecutorApprovalScope) -> Arc<Self> {
        Arc::new(Self { db, scope })
    }

    pub fn subscribe(session_id: Uuid) -> broadcast::Receiver<ExecutorApprovalEvent> {
        event_sender(session_id).subscribe()
    }

    pub async fn list_pending(
        pool: &sqlx::SqlitePool,
        session_id: Uuid,
    ) -> Result<Vec<ChatExecutorApprovalRequest>, sqlx::Error> {
        ChatExecutorApprovalRequest::list_pending(pool, session_id).await
    }

    pub async fn resolve(
        pool: &sqlx::SqlitePool,
        session_id: Uuid,
        request_id: Uuid,
        option_id: &str,
        processed_by: &str,
    ) -> Result<Option<ChatExecutorApprovalRequest>, sqlx::Error> {
        let resolved = ChatExecutorApprovalRequest::select(
            pool,
            session_id,
            request_id,
            option_id,
            processed_by,
        )
        .await?;
        if let Some(request) = &resolved {
            if let Some((_, waiter)) = WAITERS.remove(&request.id) {
                let _ = waiter.send(option_id.to_string());
            }
            restore_agent_if_no_pending(pool, request.session_agent_id).await?;
            InboxService::new()
                .resolve_executor_approval(pool, request.id)
                .await;
            emit(
                request.session_id,
                ExecutorApprovalEvent::ExecutorApprovalResolved {
                    session_id: request.session_id,
                    request_id: request.id,
                    session_agent_id: request.session_agent_id,
                    request: request.clone(),
                },
            );
        }
        Ok(resolved)
    }

    pub async fn expire_orphaned(pool: &sqlx::SqlitePool) -> Result<u64, sqlx::Error> {
        let expired = ChatExecutorApprovalRequest::expire_orphaned(pool).await?;
        for request in &expired {
            WAITERS.remove(&request.id);
            sqlx::query(
                "UPDATE chat_session_agents SET state = 'idle', \
                 updated_at = datetime('now', 'subsec') \
                 WHERE id = ?1 AND state = ?2",
            )
            .bind(request.session_agent_id)
            .bind(ChatSessionAgentState::WaitingApproval)
            .execute(pool)
            .await?;
            InboxService::new()
                .resolve_executor_approval(pool, request.id)
                .await;
            emit(
                request.session_id,
                ExecutorApprovalEvent::ExecutorApprovalExpired {
                    session_id: request.session_id,
                    request_id: request.id,
                    session_agent_id: request.session_agent_id,
                    request: request.clone(),
                },
            );
        }
        Ok(expired.len() as u64)
    }

    pub async fn cancel_for_run(pool: &sqlx::SqlitePool, run_id: Uuid) -> Result<u64, sqlx::Error> {
        let ids = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM chat_executor_approval_requests \
             WHERE run_id = ?1 AND status = 'pending'",
        )
        .bind(run_id)
        .fetch_all(pool)
        .await?;
        let mut count = 0;
        for id in ids {
            if finish_request(pool, id, ChatExecutorApprovalStatus::Cancelled)
                .await?
                .is_some()
            {
                count += 1;
            }
        }
        Ok(count)
    }

    pub async fn cancel_for_session(
        pool: &sqlx::SqlitePool,
        session_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let ids = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM chat_executor_approval_requests \
             WHERE session_id = ?1 AND status = 'pending'",
        )
        .bind(session_id)
        .fetch_all(pool)
        .await?;
        let mut count = 0;
        for id in ids {
            if finish_request(pool, id, ChatExecutorApprovalStatus::Cancelled)
                .await?
                .is_some()
            {
                count += 1;
            }
        }
        Ok(count)
    }

    pub async fn cancel_for_session_agent(
        pool: &sqlx::SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let ids = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM chat_executor_approval_requests \
             WHERE session_agent_id = ?1 AND status = 'pending'",
        )
        .bind(session_agent_id)
        .fetch_all(pool)
        .await?;
        let mut count = 0;
        for id in ids {
            if finish_request(pool, id, ChatExecutorApprovalStatus::Cancelled)
                .await?
                .is_some()
            {
                count += 1;
            }
        }
        Ok(count)
    }

    async fn request_and_wait(
        &self,
        request: ExecutorApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<String, ExecutorApprovalError> {
        if request.options.is_empty() {
            return Err(ExecutorApprovalError::Cancelled);
        }
        let expires_at = Utc::now()
            + TimeDelta::from_std(APPROVAL_TIMEOUT)
                .map_err(ExecutorApprovalError::request_failed)?;
        let row = ChatExecutorApprovalRequest::create_or_find(
            &self.db.pool,
            &CreateChatExecutorApprovalRequest {
                session_id: self.scope.session_id,
                session_agent_id: self.scope.session_agent_id,
                run_id: self.scope.run_id,
                workflow_execution_id: self.scope.workflow_execution_id,
                workflow_step_id: self.scope.workflow_step_id,
                runner: self.scope.runner.clone(),
                tool_call_id: request.tool_call_id,
                tool_name: request.tool_name,
                display_input: sanitize_display_input(request.tool_input),
                options: request
                    .options
                    .into_iter()
                    .map(|option| ChatExecutorApprovalOption {
                        option_id: option.option_id,
                        kind: option.kind,
                        label: option.label,
                    })
                    .collect(),
                expires_at,
            },
            Uuid::new_v4(),
        )
        .await
        .map_err(ExecutorApprovalError::request_failed)?;

        match row.status {
            ChatExecutorApprovalStatus::Selected => {
                return row
                    .selected_option_id
                    .ok_or(ExecutorApprovalError::Cancelled);
            }
            ChatExecutorApprovalStatus::Cancelled | ChatExecutorApprovalStatus::Expired => {
                return Err(ExecutorApprovalError::Cancelled);
            }
            ChatExecutorApprovalStatus::Pending => {}
        }

        InboxService::new()
            .notify_executor_approval_requested(
                &self.db.pool,
                &ApprovalRequest {
                    id: row.id.to_string(),
                    tool_name: row.tool_name.clone(),
                    tool_input: row.display_input.0.clone(),
                    tool_call_id: row.tool_call_id.clone(),
                    execution_process_id: row.run_id,
                    created_at: row.created_at,
                    timeout_at: row.expires_at,
                },
            )
            .await;

        let (tx, rx) = oneshot::channel();
        match WAITERS.entry(row.id) {
            Entry::Vacant(entry) => {
                entry.insert(tx);
            }
            Entry::Occupied(_) => {
                return Err(ExecutorApprovalError::RequestFailed(
                    "duplicate pending approval waiter".to_string(),
                ));
            }
        }
        mark_agent_waiting(&self.db.pool, row.session_agent_id, row.id)
            .await
            .map_err(ExecutorApprovalError::request_failed)?;
        let current = ChatExecutorApprovalRequest::find_by_id(&self.db.pool, row.id)
            .await
            .map_err(ExecutorApprovalError::request_failed)?
            .ok_or_else(|| {
                ExecutorApprovalError::RequestFailed(
                    "approval request disappeared after waiter registration".to_string(),
                )
            })?;
        match current.status {
            ChatExecutorApprovalStatus::Selected => {
                WAITERS.remove(&row.id);
                InboxService::new()
                    .resolve_executor_approval(&self.db.pool, row.id)
                    .await;
                return current
                    .selected_option_id
                    .ok_or(ExecutorApprovalError::Cancelled);
            }
            ChatExecutorApprovalStatus::Cancelled | ChatExecutorApprovalStatus::Expired => {
                WAITERS.remove(&row.id);
                InboxService::new()
                    .resolve_executor_approval(&self.db.pool, row.id)
                    .await;
                return Err(ExecutorApprovalError::Cancelled);
            }
            ChatExecutorApprovalStatus::Pending => {}
        }
        if let Some(_waiter) = WAITERS.get(&row.id) {
            emit(
                row.session_id,
                ExecutorApprovalEvent::ExecutorApprovalRequested {
                    session_id: row.session_id,
                    request_id: row.id,
                    session_agent_id: row.session_agent_id,
                    request: row.clone(),
                },
            );
        }

        let pool = self.db.pool.clone();
        let request_id = row.id;
        tokio::spawn(async move {
            tokio::time::sleep(APPROVAL_TIMEOUT).await;
            let _ = finish_request(&pool, request_id, ChatExecutorApprovalStatus::Expired).await;
        });

        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = finish_request(
                    &self.db.pool,
                    row.id,
                    ChatExecutorApprovalStatus::Cancelled,
                ).await;
                Err(ExecutorApprovalError::Cancelled)
            }
            selected = rx => selected.map_err(|_| ExecutorApprovalError::Cancelled),
        }
    }
}

#[async_trait]
impl ExecutorApprovalService for ExecutorApprovalBridge {
    async fn request_tool_approval(
        &self,
        tool_name: &str,
        tool_input: Value,
        tool_call_id: &str,
        cancel: CancellationToken,
    ) -> Result<ApprovalStatus, ExecutorApprovalError> {
        let selected = self
            .request_and_wait(
                ExecutorApprovalRequest {
                    tool_name: tool_name.to_string(),
                    tool_input,
                    tool_call_id: tool_call_id.to_string(),
                    options: vec![
                        ExecutorApprovalOption {
                            option_id: "approve".to_string(),
                            kind: "allow_once".to_string(),
                            label: "Approve".to_string(),
                        },
                        ExecutorApprovalOption {
                            option_id: "deny".to_string(),
                            kind: "reject_once".to_string(),
                            label: "Deny".to_string(),
                        },
                    ],
                },
                cancel,
            )
            .await?;
        Ok(if selected == "approve" {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Denied { reason: None }
        })
    }

    async fn request_acp_tool_approval(
        &self,
        request: ExecutorApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<String, ExecutorApprovalError> {
        self.request_and_wait(request, cancel).await
    }
}

fn event_sender(session_id: Uuid) -> broadcast::Sender<ExecutorApprovalEvent> {
    EVENTS
        .entry(session_id)
        .or_insert_with(|| broadcast::channel(128).0)
        .clone()
}

fn emit(session_id: Uuid, event: ExecutorApprovalEvent) {
    let _ = event_sender(session_id).send(event);
}

async fn finish_request(
    pool: &sqlx::SqlitePool,
    id: Uuid,
    status: ChatExecutorApprovalStatus,
) -> Result<Option<ChatExecutorApprovalRequest>, sqlx::Error> {
    let finished = ChatExecutorApprovalRequest::finish_pending(pool, id, status).await?;
    if let Some(request) = &finished {
        WAITERS.remove(&request.id);
        restore_agent_if_no_pending(pool, request.session_agent_id).await?;
        InboxService::new()
            .resolve_executor_approval(pool, request.id)
            .await;
        let event = match status {
            ChatExecutorApprovalStatus::Cancelled => {
                ExecutorApprovalEvent::ExecutorApprovalCancelled {
                    session_id: request.session_id,
                    request_id: request.id,
                    session_agent_id: request.session_agent_id,
                    request: request.clone(),
                }
            }
            ChatExecutorApprovalStatus::Expired => ExecutorApprovalEvent::ExecutorApprovalExpired {
                session_id: request.session_id,
                request_id: request.id,
                session_agent_id: request.session_agent_id,
                request: request.clone(),
            },
            _ => unreachable!("finish_request only accepts terminal non-selected states"),
        };
        emit(request.session_id, event);
    }
    Ok(finished)
}

async fn mark_agent_waiting(
    pool: &sqlx::SqlitePool,
    session_agent_id: Uuid,
    request_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE chat_session_agents SET state = 'waitingapproval', \
         updated_at = datetime('now', 'subsec') \
         WHERE id = ?1 AND state = 'running' \
           AND EXISTS (
             SELECT 1 FROM chat_executor_approval_requests \
             WHERE id = ?2 AND status = 'pending'
           )",
    )
    .bind(session_agent_id)
    .bind(request_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn restore_agent_if_no_pending(
    pool: &sqlx::SqlitePool,
    session_agent_id: Uuid,
) -> Result<(), sqlx::Error> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM chat_executor_approval_requests \
         WHERE session_agent_id = ?1 AND status = 'pending'",
    )
    .bind(session_agent_id)
    .fetch_one(pool)
    .await?;
    if count == 0 {
        sqlx::query(
            "UPDATE chat_session_agents SET state = 'running', \
             updated_at = datetime('now', 'subsec') \
             WHERE id = ?1 AND state = ?2",
        )
        .bind(session_agent_id)
        .bind(ChatSessionAgentState::WaitingApproval)
        .execute(pool)
        .await?;
    }
    Ok(())
}

fn sanitize_display_input(value: Value) -> Value {
    fn redact(value: Value) -> Value {
        match value {
            Value::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| {
                        let normalized = key.to_ascii_lowercase();
                        let sensitive = [
                            "token",
                            "authorization",
                            "password",
                            "secret",
                            "api_key",
                            "headers",
                            "env",
                        ]
                        .iter()
                        .any(|needle| normalized.contains(needle));
                        (
                            key,
                            if sensitive {
                                Value::String("[REDACTED]".into())
                            } else {
                                redact(value)
                            },
                        )
                    })
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.into_iter().map(redact).collect()),
            other => other,
        }
    }
    let redacted = redact(value);
    let encoded = serde_json::to_vec(&redacted).unwrap_or_default();
    if encoded.len() <= MAX_DISPLAY_INPUT_BYTES {
        redacted
    } else {
        Value::String("[TRUNCATED]".to_string())
    }
}

#[cfg(test)]
mod approval_flow_tests {
    use db::models::{
        chat_executor_approval_request::ChatExecutorApprovalStatus,
        chat_session_agent::{ChatSessionAgent, ChatSessionAgentState},
    };
    use executors::approvals::{
        ExecutorApprovalOption, ExecutorApprovalRequest, ExecutorApprovalService,
    };
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use tokio::time::{Duration, sleep};

    use super::*;

    async fn setup_bridge() -> (DBService, ExecutorApprovalScope) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect approval test database");
        sqlx::migrate!("../db/migrations")
            .run(&pool)
            .await
            .expect("run approval test migrations");

        let session_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let session_agent_id = Uuid::new_v4();
        let run_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO chat_sessions (id, title, status) VALUES (?1, 'approval test', 'active')",
        )
        .bind(session_id)
        .execute(&pool)
        .await
        .expect("insert session");
        sqlx::query(
            "INSERT INTO chat_agents (id, name, runner_type) VALUES (?1, 'Qwen', 'QWEN_CODE')",
        )
        .bind(agent_id)
        .execute(&pool)
        .await
        .expect("insert agent");
        sqlx::query(
            "INSERT INTO chat_session_agents (id, session_id, agent_id, state) \
             VALUES (?1, ?2, ?3, 'running')",
        )
        .bind(session_agent_id)
        .bind(session_id)
        .bind(agent_id)
        .execute(&pool)
        .await
        .expect("insert session agent");
        sqlx::query(
            "INSERT INTO chat_runs (id, session_id, session_agent_id, run_index, run_dir) \
             VALUES (?1, ?2, ?3, 1, '/tmp/qwen-approval-test')",
        )
        .bind(run_id)
        .bind(session_id)
        .bind(session_agent_id)
        .execute(&pool)
        .await
        .expect("insert chat run");

        (
            DBService { pool },
            ExecutorApprovalScope {
                session_id,
                session_agent_id,
                run_id,
                runner: "QWEN_CODE".to_string(),
                workflow_execution_id: None,
                workflow_step_id: None,
            },
        )
    }

    async fn resolve_and_wait(option_id: &str) {
        let (db, scope) = setup_bridge().await;
        let bridge = ExecutorApprovalBridge::new(db.clone(), scope.clone());
        let waiter_option_id = option_id.to_string();
        let waiter = tokio::spawn(async move {
            bridge
                .request_acp_tool_approval(
                    ExecutorApprovalRequest {
                        tool_name: "write_file".to_string(),
                        tool_input: json!({"path": "approval.txt"}),
                        tool_call_id: format!("tool-{waiter_option_id}"),
                        options: vec![
                            ExecutorApprovalOption {
                                option_id: "proceed_once".to_string(),
                                kind: "allow_once".to_string(),
                                label: "Proceed once".to_string(),
                            },
                            ExecutorApprovalOption {
                                option_id: "cancel".to_string(),
                                kind: "reject_once".to_string(),
                                label: "Cancel".to_string(),
                            },
                        ],
                    },
                    CancellationToken::new(),
                )
                .await
        });

        let request = loop {
            let pending = ExecutorApprovalBridge::list_pending(&db.pool, scope.session_id)
                .await
                .expect("list pending approvals");
            if let Some(request) = pending.into_iter().next() {
                break request;
            }
            sleep(Duration::from_millis(10)).await;
        };
        let waiting_agent = ChatSessionAgent::find_by_id(&db.pool, scope.session_agent_id)
            .await
            .expect("load waiting agent")
            .expect("waiting agent exists");
        assert_eq!(waiting_agent.state, ChatSessionAgentState::WaitingApproval);

        let selected = ExecutorApprovalBridge::resolve(
            &db.pool,
            scope.session_id,
            request.id,
            option_id,
            "test-user",
        )
        .await
        .expect("resolve approval")
        .expect("pending approval selected");
        assert_eq!(selected.status, ChatExecutorApprovalStatus::Selected);
        assert_eq!(selected.selected_option_id.as_deref(), Some(option_id));
        assert_eq!(
            waiter
                .await
                .expect("join approval waiter")
                .expect("waiter result"),
            option_id
        );

        let restored_agent = ChatSessionAgent::find_by_id(&db.pool, scope.session_agent_id)
            .await
            .expect("load restored agent")
            .expect("restored agent exists");
        assert_eq!(restored_agent.state, ChatSessionAgentState::Running);
    }

    #[tokio::test]
    async fn reject_selection_wakes_waiter_and_restores_agent() {
        resolve_and_wait("cancel").await;
    }

    #[tokio::test]
    async fn allow_selection_wakes_waiter_and_restores_agent() {
        resolve_and_wait("proceed_once").await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_input_is_recursively_redacted() {
        let redacted = sanitize_display_input(serde_json::json!({
            "command": "cargo test",
            "headers": {"Authorization": "Bearer secret"},
            "nested": {"api_key": "secret"}
        }));
        assert_eq!(redacted["headers"], "[REDACTED]");
        assert_eq!(redacted["nested"]["api_key"], "[REDACTED]");
        assert_eq!(redacted["command"], "cargo test");
    }
}
