use chrono::{DateTime, Utc};
use db::models::{
    chat_message_queue::{ChatMessageQueue, CreateChatMessageQueue, QueuedMessageStatus},
    chat_run::{ChatRun, CreateChatRun},
    chat_session_agent::{ChatSessionAgent, ChatSessionAgentState},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use ts_rs::TS;
use uuid::Uuid;

/// Durable queued message for one chat session member.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct QueuedMessage {
    pub id: Uuid,
    pub session_id: Uuid,
    pub session_agent_id: Uuid,
    pub agent_id: Uuid,
    pub chat_message_id: Uuid,
    pub status: QueuedMessageStatus,
    pub revision: i64,
    pub attempt_no: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub processing_started_at: Option<DateTime<Utc>>,
    pub run_id: Option<Uuid>,
    pub failure_reason: Option<String>,
    pub failure_resolved_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum MemberQueueStatus {
    Empty,
    Queued,
    Starting,
    /// Legacy projection value accepted during rolling upgrades.
    Processing,
    Running,
    WaitingApproval,
    Stopping,
    Blocked,
    Paused,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct QueuedMessageListItem {
    pub message: QueuedMessage,
    pub can_delete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct MemberQueueSnapshot {
    pub session_id: Uuid,
    /// Session-scoped monotonic runtime revision used to discard stale snapshots/events.
    pub revision: i64,
    pub session_agent_id: Uuid,
    pub agent_id: Uuid,
    pub status: MemberQueueStatus,
    pub blocked: bool,
    pub paused: bool,
    pub can_continue: bool,
    pub queued_count: i64,
    pub items: Vec<QueuedMessageListItem>,
}

/// Frontend-facing queue state derived from durable member queue rows.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(export)]
pub enum QueueStatus {
    Empty,
    Queued {
        messages: Vec<QueuedMessage>,
    },
    Processing {
        message: QueuedMessage,
        queued_count: i64,
    },
    Starting {
        message: QueuedMessage,
        queued_count: i64,
    },
    Running {
        message: QueuedMessage,
        queued_count: i64,
    },
    WaitingApproval {
        message: QueuedMessage,
        queued_count: i64,
    },
    Stopping {
        message: QueuedMessage,
        queued_count: i64,
    },
    /// A failed item is blocking the member queue until the user chooses to continue.
    Blocked {
        message: QueuedMessage,
        queued_count: i64,
    },
    /// Alias for UIs that display failed queues as paused rather than blocked.
    Paused {
        message: QueuedMessage,
        queued_count: i64,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateQueuedMessage {
    pub session_id: Uuid,
    pub session_agent_id: Uuid,
    pub agent_id: Uuid,
    pub chat_message_id: Uuid,
}

pub struct RunQueueFinalization {
    pub applied: bool,
    pub next: Option<QueuedMessage>,
    pub runtime_revision: i64,
}

pub struct DeliveryRunBinding {
    pub delivery: QueuedMessage,
    pub run: ChatRun,
    pub member: ChatSessionAgent,
    pub runtime_revision: i64,
}

pub struct RunStoppingTransition {
    pub delivery: QueuedMessage,
    pub member: ChatSessionAgent,
    pub runtime_revision: i64,
}

/// Database-backed service for managing member-scoped queued chat messages.
///
/// The service keeps no in-memory queue state. Every operation delegates to the
/// `chat_message_queue` table, where each row references the existing `chat_messages` source row
/// and is scoped to one `session_agent_id`.
#[derive(Clone, Default)]
pub struct QueuedMessageService;

impl QueuedMessageService {
    pub fn new() -> Self {
        Self
    }

    pub(crate) fn from_row(row: ChatMessageQueue) -> QueuedMessage {
        QueuedMessage {
            id: row.id,
            session_id: row.session_id,
            session_agent_id: row.session_agent_id,
            agent_id: row.agent_id,
            chat_message_id: row.chat_message_id,
            status: row.status,
            revision: row.revision,
            attempt_no: row.attempt_no,
            created_at: row.created_at,
            updated_at: row.updated_at,
            processing_started_at: row.processing_started_at,
            run_id: row.run_id,
            failure_reason: row.failure_reason,
            failure_resolved_at: row.failure_resolved_at,
        }
    }

    fn active_messages(messages: Vec<QueuedMessage>) -> Vec<QueuedMessage> {
        messages
            .into_iter()
            .filter(|message| {
                matches!(
                    message.status,
                    QueuedMessageStatus::Queued
                        | QueuedMessageStatus::Starting
                        | QueuedMessageStatus::Processing
                        | QueuedMessageStatus::Running
                        | QueuedMessageStatus::WaitingApproval
                        | QueuedMessageStatus::Stopping
                ) || (message.status == QueuedMessageStatus::Failed
                    && message.failure_resolved_at.is_none())
            })
            .collect()
    }

    fn snapshot_from_messages(
        session_id: Uuid,
        revision: i64,
        session_agent_id: Uuid,
        agent_id: Uuid,
        messages: Vec<QueuedMessage>,
    ) -> MemberQueueSnapshot {
        let active_messages = Self::active_messages(messages);
        let queued_count = active_messages
            .iter()
            .filter(|message| message.status == QueuedMessageStatus::Queued)
            .count() as i64;
        let has_failed = active_messages
            .iter()
            .any(|message| message.status == QueuedMessageStatus::Failed);
        let status = if has_failed && queued_count > 0 {
            MemberQueueStatus::Blocked
        } else if has_failed {
            MemberQueueStatus::Paused
        } else if active_messages
            .iter()
            .any(|message| message.status == QueuedMessageStatus::Stopping)
        {
            MemberQueueStatus::Stopping
        } else if active_messages
            .iter()
            .any(|message| message.status == QueuedMessageStatus::WaitingApproval)
        {
            MemberQueueStatus::WaitingApproval
        } else if active_messages
            .iter()
            .any(|message| message.status == QueuedMessageStatus::Running)
        {
            MemberQueueStatus::Running
        } else if active_messages.iter().any(|message| {
            matches!(
                message.status,
                QueuedMessageStatus::Starting | QueuedMessageStatus::Processing
            )
        }) {
            MemberQueueStatus::Starting
        } else if queued_count > 0 {
            MemberQueueStatus::Queued
        } else {
            MemberQueueStatus::Empty
        };

        MemberQueueSnapshot {
            session_id,
            revision,
            session_agent_id,
            agent_id,
            status,
            blocked: has_failed,
            paused: status == MemberQueueStatus::Paused,
            can_continue: has_failed && queued_count > 0,
            queued_count,
            items: active_messages
                .into_iter()
                .map(|message| QueuedMessageListItem {
                    can_delete: message.status == QueuedMessageStatus::Queued,
                    message,
                })
                .collect(),
        }
    }

    fn create_data(data: &CreateQueuedMessage) -> CreateChatMessageQueue {
        CreateChatMessageQueue {
            session_id: data.session_id,
            session_agent_id: data.session_agent_id,
            agent_id: data.agent_id,
            chat_message_id: data.chat_message_id,
        }
    }

    pub async fn find_by_id(
        &self,
        pool: &SqlitePool,
        id: Uuid,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(ChatMessageQueue::find_by_id(pool, id)
            .await?
            .map(Self::from_row))
    }

    /// Persist a queued row for a member. The user message itself remains in `chat_messages`.
    pub async fn create_queued(
        &self,
        pool: &SqlitePool,
        data: &CreateQueuedMessage,
    ) -> Result<QueuedMessage, sqlx::Error> {
        let row =
            ChatMessageQueue::create_queued(pool, &Self::create_data(data), Uuid::new_v4()).await?;
        Ok(Self::from_row(row))
    }

    /// Return all queue rows for one member, oldest first, for recovery/display.
    pub async fn list_for_member(
        &self,
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<Vec<QueuedMessage>, sqlx::Error> {
        let rows = ChatMessageQueue::list_for_member(pool, session_agent_id).await?;
        Ok(rows.into_iter().map(Self::from_row).collect())
    }

    /// Return claimed rows that never reached run binding, regardless of member runtime state.
    pub async fn list_unbound_processing(
        &self,
        pool: &SqlitePool,
    ) -> Result<Vec<QueuedMessage>, sqlx::Error> {
        let rows = ChatMessageQueue::list_unbound_processing(pool).await?;
        Ok(rows.into_iter().map(Self::from_row).collect())
    }

    pub async fn list_members_with_queued(
        &self,
        pool: &SqlitePool,
    ) -> Result<Vec<Uuid>, sqlx::Error> {
        ChatMessageQueue::list_members_with_queued(pool).await
    }

    /// Return the member's currently claimed or running queue row, if one exists.
    pub async fn find_active_for_member(
        &self,
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(
            ChatMessageQueue::find_active_for_member(pool, session_agent_id)
                .await?
                .map(Self::from_row),
        )
    }

    /// Check whether a member has queued rows that have not started yet.
    pub async fn has_queued(
        &self,
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        Ok(ChatMessageQueue::count_queued_for_member(pool, session_agent_id).await? > 0)
    }

    pub async fn has_blocking_failure(
        &self,
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        ChatMessageQueue::has_blocking_failure(pool, session_agent_id).await
    }

    /// Atomically claim the oldest queued row for a member and move it to `starting`.
    pub async fn claim_next(
        &self,
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(ChatMessageQueue::claim_next(pool, session_agent_id)
            .await?
            .map(Self::from_row))
    }

    pub async fn start_or_create_running(
        &self,
        pool: &SqlitePool,
        data: &CreateQueuedMessage,
        id: Uuid,
        run_id: Uuid,
    ) -> Result<QueuedMessage, sqlx::Error> {
        ChatMessageQueue::start_or_create_running(pool, &Self::create_data(data), id, run_id)
            .await
            .map(Self::from_row)
    }

    pub async fn find_by_run_id(
        &self,
        pool: &SqlitePool,
        run_id: Uuid,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(ChatMessageQueue::find_by_run_id(pool, run_id)
            .await?
            .map(Self::from_row))
    }

    /// Bind a `starting` row to a run and move it to `running`.
    pub async fn bind_run(
        &self,
        pool: &SqlitePool,
        id: Uuid,
        run_id: Uuid,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(ChatMessageQueue::bind_run(pool, id, run_id)
            .await?
            .map(Self::from_row))
    }

    /// Atomically create the durable run, bind the claimed delivery, update the member runtime
    /// projection, and advance the session revision. The caller must emit stream events only
    /// after this method returns.
    pub async fn bind_delivery_to_new_run(
        &self,
        pool: &SqlitePool,
        delivery_id: Uuid,
        expected_delivery_revision: i64,
        run_data: &CreateChatRun,
        run_id: Uuid,
    ) -> Result<Option<DeliveryRunBinding>, sqlx::Error> {
        let mut transaction = pool.begin().await?;
        let run = ChatRun::create_in_transaction(&mut transaction, run_data, run_id).await?;
        let Some(delivery) = ChatMessageQueue::bind_run_cas_in_transaction(
            &mut transaction,
            delivery_id,
            expected_delivery_revision,
            run_id,
        )
        .await?
        else {
            transaction.rollback().await?;
            return Ok(None);
        };

        if delivery.session_id != run_data.session_id
            || delivery.session_agent_id != run_data.session_agent_id
        {
            transaction.rollback().await?;
            return Err(sqlx::Error::Protocol(
                "delivery and run target do not match".to_string(),
            ));
        }
        let Some(member) = ChatSessionAgent::update_state_for_run_in_transaction(
            &mut transaction,
            run_data.session_agent_id,
            run_id,
            ChatSessionAgentState::Running,
        )
        .await?
        else {
            transaction.rollback().await?;
            return Ok(None);
        };
        if member.session_id != delivery.session_id || member.agent_id != delivery.agent_id {
            transaction.rollback().await?;
            return Err(sqlx::Error::Protocol(
                "delivery and member target do not match".to_string(),
            ));
        }
        let runtime_revision = ChatMessageQueue::current_runtime_revision_in_transaction(
            &mut transaction,
            run_data.session_id,
        )
        .await?;
        transaction.commit().await?;

        Ok(Some(DeliveryRunBinding {
            delivery: Self::from_row(delivery),
            run,
            member,
            runtime_revision,
        }))
    }

    pub async fn transition_status_cas(
        &self,
        pool: &SqlitePool,
        id: Uuid,
        expected_revision: i64,
        expected_status: QueuedMessageStatus,
        next_status: QueuedMessageStatus,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(ChatMessageQueue::transition_status_cas(
            pool,
            id,
            expected_revision,
            expected_status,
            next_status,
        )
        .await?
        .map(Self::from_row))
    }

    /// Atomically fail or auto-skip one exact in-flight attempt and retain its diagnostic.
    /// A CAS miss returns `None`; callers must publish only after receiving `Some`.
    pub async fn fail_or_skip_inflight_cas(
        &self,
        pool: &SqlitePool,
        delivery_id: Uuid,
        expected_delivery_revision: i64,
        expected_delivery_status: QueuedMessageStatus,
        next_status: QueuedMessageStatus,
        failure_reason: Option<String>,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(ChatMessageQueue::fail_or_skip_inflight_cas(
            pool,
            delivery_id,
            expected_delivery_revision,
            expected_delivery_status,
            next_status,
            failure_reason,
        )
        .await?
        .map(Self::from_row))
    }

    /// Atomically project an active run as stopping in both the delivery ledger and member row.
    /// A stale delivery, run, status, revision, or inactive member returns `None` with no writes.
    pub async fn transition_run_to_stopping_cas(
        &self,
        pool: &SqlitePool,
        delivery_id: Uuid,
        expected_delivery_revision: i64,
        expected_delivery_status: QueuedMessageStatus,
        run_id: Uuid,
        session_agent_id: Uuid,
    ) -> Result<Option<RunStoppingTransition>, sqlx::Error> {
        let mut transaction = pool.begin().await?;
        let Some(delivery) = ChatMessageQueue::transition_run_to_stopping_cas_in_transaction(
            &mut transaction,
            delivery_id,
            expected_delivery_revision,
            expected_delivery_status,
            run_id,
            session_agent_id,
        )
        .await?
        else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let Some(member) = ChatSessionAgent::mark_stopping_for_delivery_in_transaction(
            &mut transaction,
            session_agent_id,
            delivery.id,
            delivery.revision,
            run_id,
        )
        .await?
        else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let runtime_revision = ChatMessageQueue::current_runtime_revision_in_transaction(
            &mut transaction,
            delivery.session_id,
        )
        .await?;
        transaction.commit().await?;

        Ok(Some(RunStoppingTransition {
            delivery: Self::from_row(delivery),
            member,
            runtime_revision,
        }))
    }

    pub async fn current_runtime_revision(
        &self,
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        ChatMessageQueue::current_runtime_revision(pool, session_id).await
    }

    pub async fn complete_run_and_claim_next(
        &self,
        pool: &SqlitePool,
        run_id: Uuid,
        session_agent_id: Uuid,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        let (_completed, claimed) =
            ChatMessageQueue::complete_run_and_claim_next(pool, run_id, session_agent_id).await?;
        Ok(claimed.map(Self::from_row))
    }

    /// Atomically move the member to idle, complete the queue row guarded by `run_id`, and
    /// optionally claim its next durable message.
    pub async fn finalize_completed_run(
        &self,
        pool: &SqlitePool,
        run_id: Uuid,
        session_agent_id: Uuid,
        claim_next: bool,
    ) -> Result<RunQueueFinalization, sqlx::Error> {
        let Some(delivery) = self.find_by_run_id(pool, run_id).await? else {
            let session_id = ChatSessionAgent::find_by_id(pool, session_agent_id)
                .await?
                .ok_or(sqlx::Error::RowNotFound)?
                .session_id;
            return Ok(RunQueueFinalization {
                applied: false,
                next: None,
                runtime_revision: self.current_runtime_revision(pool, session_id).await?,
            });
        };
        self.finalize_completed_run_cas(
            pool,
            run_id,
            session_agent_id,
            delivery.revision,
            claim_next,
        )
        .await
    }

    pub async fn finalize_completed_run_cas(
        &self,
        pool: &SqlitePool,
        run_id: Uuid,
        session_agent_id: Uuid,
        expected_delivery_revision: i64,
        claim_next: bool,
    ) -> Result<RunQueueFinalization, sqlx::Error> {
        let session_id = ChatSessionAgent::find_by_id(pool, session_agent_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?
            .session_id;
        let mut transaction = pool.begin().await?;
        let updated_agent = ChatSessionAgent::update_state_for_run_in_transaction(
            &mut transaction,
            session_agent_id,
            run_id,
            ChatSessionAgentState::Idle,
        )
        .await?;
        if updated_agent.is_none() {
            transaction.rollback().await?;
            let runtime_revision = self.current_runtime_revision(pool, session_id).await?;
            return Ok(RunQueueFinalization {
                applied: false,
                next: None,
                runtime_revision,
            });
        }
        let completed = ChatMessageQueue::mark_completed_by_run_cas_in_transaction(
            &mut transaction,
            run_id,
            expected_delivery_revision,
        )
        .await?;
        if completed.is_none() {
            transaction.rollback().await?;
            let runtime_revision = self.current_runtime_revision(pool, session_id).await?;
            return Ok(RunQueueFinalization {
                applied: false,
                next: None,
                runtime_revision,
            });
        }
        let next = if claim_next {
            ChatMessageQueue::claim_next_in_transaction(&mut transaction, session_agent_id).await?
        } else {
            None
        };
        let runtime_revision =
            ChatMessageQueue::current_runtime_revision_in_transaction(&mut transaction, session_id)
                .await?;
        transaction.commit().await?;
        Ok(RunQueueFinalization {
            applied: true,
            next: next.map(Self::from_row),
            runtime_revision,
        })
    }

    /// Atomically move the member to dead and finalize the queue row guarded by `run_id`.
    pub async fn finalize_failed_run(
        &self,
        pool: &SqlitePool,
        run_id: Uuid,
        session_agent_id: Uuid,
        failure_reason: Option<String>,
    ) -> Result<bool, sqlx::Error> {
        let Some(delivery) = self.find_by_run_id(pool, run_id).await? else {
            return Ok(false);
        };
        Ok(self
            .finalize_failed_run_cas(
                pool,
                run_id,
                session_agent_id,
                delivery.revision,
                failure_reason,
            )
            .await?
            .applied)
    }

    pub async fn finalize_failed_run_cas(
        &self,
        pool: &SqlitePool,
        run_id: Uuid,
        session_agent_id: Uuid,
        expected_delivery_revision: i64,
        failure_reason: Option<String>,
    ) -> Result<RunQueueFinalization, sqlx::Error> {
        let session_id = ChatSessionAgent::find_by_id(pool, session_agent_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?
            .session_id;
        let mut transaction = pool.begin().await?;
        let updated_agent = ChatSessionAgent::update_state_for_run_in_transaction(
            &mut transaction,
            session_agent_id,
            run_id,
            ChatSessionAgentState::Dead,
        )
        .await?;
        if updated_agent.is_none() {
            transaction.rollback().await?;
            return Ok(RunQueueFinalization {
                applied: false,
                next: None,
                runtime_revision: self.current_runtime_revision(pool, session_id).await?,
            });
        }
        let finalized = ChatMessageQueue::mark_failed_or_skipped_by_run_cas_in_transaction(
            &mut transaction,
            run_id,
            session_agent_id,
            expected_delivery_revision,
            failure_reason,
        )
        .await?;
        if finalized.is_none() {
            transaction.rollback().await?;
            return Ok(RunQueueFinalization {
                applied: false,
                next: None,
                runtime_revision: self.current_runtime_revision(pool, session_id).await?,
            });
        }
        let runtime_revision =
            ChatMessageQueue::current_runtime_revision_in_transaction(&mut transaction, session_id)
                .await?;
        transaction.commit().await?;
        Ok(RunQueueFinalization {
            applied: true,
            next: None,
            runtime_revision,
        })
    }

    /// Mark `processing` or `running` as `completed` after success or a normal stop.
    pub async fn mark_completed(
        &self,
        pool: &SqlitePool,
        id: Uuid,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(ChatMessageQueue::mark_completed(pool, id)
            .await?
            .map(Self::from_row))
    }

    /// Mark `processing` or `running` as `failed`. Remaining queued rows are left intact.
    pub async fn mark_failed(
        &self,
        pool: &SqlitePool,
        id: Uuid,
        failure_reason: Option<String>,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(ChatMessageQueue::mark_failed(pool, id, failure_reason)
            .await?
            .map(Self::from_row))
    }

    /// Continue a blocked member queue by marking failed rows as `skipped`.
    pub async fn skip_failed_for_member(
        &self,
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        ChatMessageQueue::skip_failed_for_member(pool, session_agent_id).await
    }

    /// Skip an in-flight entry directly when a run fails but no queued messages remain for the
    /// member. Keeps the queue clean so the next message runs instead of being blocked.
    pub async fn skip_inflight(
        &self,
        pool: &SqlitePool,
        id: Uuid,
        failure_reason: Option<String>,
    ) -> Result<Option<QueuedMessage>, sqlx::Error> {
        Ok(ChatMessageQueue::skip_inflight(pool, id, failure_reason)
            .await?
            .map(Self::from_row))
    }

    /// Delete a queued row that has not started yet.
    pub async fn delete_queued(&self, pool: &SqlitePool, id: Uuid) -> Result<u64, sqlx::Error> {
        ChatMessageQueue::delete_queued(pool, id).await
    }

    pub async fn delete_queued_cas(
        &self,
        pool: &SqlitePool,
        id: Uuid,
        expected_revision: i64,
    ) -> Result<u64, sqlx::Error> {
        ChatMessageQueue::delete_queued_cas(pool, id, expected_revision).await
    }

    /// Count other queue rows that reference the same source `chat_messages` row, excluding the
    /// given queue id. Used to decide whether the source message can be cleaned up after a queued
    /// row is deleted.
    pub async fn other_reference_count_for_chat_message(
        &self,
        pool: &SqlitePool,
        chat_message_id: Uuid,
        exclude_queue_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        ChatMessageQueue::count_other_references_for_chat_message(
            pool,
            chat_message_id,
            exclude_queue_id,
        )
        .await
    }

    pub async fn requeue_stale_inflight(
        &self,
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        ChatMessageQueue::requeue_stale_inflight(pool, session_agent_id).await
    }

    pub async fn snapshot_for_member(
        &self,
        pool: &SqlitePool,
        session_id: Uuid,
        session_agent_id: Uuid,
        agent_id: Uuid,
    ) -> Result<MemberQueueSnapshot, sqlx::Error> {
        let mut transaction = pool.begin().await?;
        let revision =
            ChatMessageQueue::current_runtime_revision_in_transaction(&mut transaction, session_id)
                .await?;
        let messages =
            ChatMessageQueue::list_for_member_in_transaction(&mut transaction, session_agent_id)
                .await?
                .into_iter()
                .map(Self::from_row)
                .collect();
        transaction.commit().await?;
        Ok(Self::snapshot_from_messages(
            session_id,
            revision,
            session_agent_id,
            agent_id,
            messages,
        ))
    }

    /// Derive member queue state from persisted rows. Failed rows take precedence because they
    /// block later queued messages until skipped by the continue action.
    pub async fn get_status(
        &self,
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<QueueStatus, sqlx::Error> {
        let messages = self.list_for_member(pool, session_agent_id).await?;
        let queued_count = messages
            .iter()
            .filter(|message| message.status == QueuedMessageStatus::Queued)
            .count() as i64;

        if let Some(message) = messages
            .iter()
            .find(|message| {
                message.status == QueuedMessageStatus::Failed
                    && message.failure_resolved_at.is_none()
            })
            .cloned()
        {
            return if queued_count > 0 {
                Ok(QueueStatus::Blocked {
                    message,
                    queued_count,
                })
            } else {
                Ok(QueueStatus::Paused {
                    message,
                    queued_count,
                })
            };
        }

        if let Some(message) = messages
            .iter()
            .find(|message| message.status == QueuedMessageStatus::Stopping)
            .cloned()
        {
            return Ok(QueueStatus::Stopping {
                message,
                queued_count,
            });
        }

        if let Some(message) = messages
            .iter()
            .find(|message| message.status == QueuedMessageStatus::WaitingApproval)
            .cloned()
        {
            return Ok(QueueStatus::WaitingApproval {
                message,
                queued_count,
            });
        }

        if let Some(message) = messages
            .iter()
            .find(|message| message.status == QueuedMessageStatus::Running)
            .cloned()
        {
            return Ok(QueueStatus::Running {
                message,
                queued_count,
            });
        }

        if let Some(message) = messages
            .iter()
            .find(|message| {
                matches!(
                    message.status,
                    QueuedMessageStatus::Starting | QueuedMessageStatus::Processing
                )
            })
            .cloned()
        {
            return Ok(QueueStatus::Starting {
                message,
                queued_count,
            });
        }

        let queued: Vec<QueuedMessage> = messages
            .into_iter()
            .filter(|message| message.status == QueuedMessageStatus::Queued)
            .collect();
        if queued.is_empty() {
            Ok(QueueStatus::Empty)
        } else {
            Ok(QueueStatus::Queued { messages: queued })
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use super::*;

    async fn setup_runtime_revision_schema(pool: &SqlitePool) {
        sqlx::query(
            "CREATE TABLE chat_session_runtime_revisions (
                 session_id BLOB PRIMARY KEY,
                 revision INTEGER NOT NULL DEFAULT 0,
                 updated_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
             )",
        )
        .execute(pool)
        .await
        .expect("create runtime revisions");
        for (name, timing, row) in [
            ("queue_revision_insert", "INSERT", "NEW"),
            ("queue_revision_update", "UPDATE", "NEW"),
            ("queue_revision_delete", "DELETE", "OLD"),
        ] {
            let trigger = format!(
                "CREATE TRIGGER {name} AFTER {timing} ON chat_message_queue BEGIN
                     INSERT INTO chat_session_runtime_revisions (session_id, revision)
                     VALUES ({row}.session_id, 1)
                     ON CONFLICT(session_id) DO UPDATE SET revision = revision + 1;
                 END"
            );
            sqlx::query(&trigger)
                .execute(pool)
                .await
                .expect("create queue revision trigger");
        }
    }

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create sqlite memory pool");
        sqlx::query(
            r#"
            CREATE TABLE chat_message_queue (
                id                    BLOB PRIMARY KEY,
                session_id            BLOB NOT NULL,
                session_agent_id      BLOB NOT NULL,
                agent_id              BLOB NOT NULL,
                chat_message_id       BLOB NOT NULL,
                status                TEXT NOT NULL DEFAULT 'queued'
                                        CHECK (status IN (
                                            'queued','starting','processing','running',
                                            'waiting_approval','stopping','failed','cancelled',
                                            'skipped','completed'
                                        )),
                revision              INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
                attempt_no            INTEGER NOT NULL DEFAULT 0 CHECK (attempt_no >= 0),
                processing_started_at TEXT,
                run_id                BLOB,
                failure_reason        TEXT,
                failure_resolved_at   TEXT,
                created_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
                updated_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create chat_message_queue table");
        sqlx::query(
            r#"
            CREATE UNIQUE INDEX idx_chat_message_queue_one_active
                ON chat_message_queue(session_agent_id)
                WHERE status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
            "#,
        )
        .execute(&pool)
        .await
        .expect("create partial unique index");
        sqlx::query(
            "CREATE UNIQUE INDEX idx_chat_message_queue_delivery_key
             ON chat_message_queue(chat_message_id, session_agent_id)",
        )
        .execute(&pool)
        .await
        .expect("create delivery key index");
        setup_runtime_revision_schema(&pool).await;
        sqlx::query(
            r#"
            CREATE TABLE chat_runs (
                id BLOB PRIMARY KEY,
                session_id BLOB NOT NULL,
                session_agent_id BLOB NOT NULL,
                workspace_path TEXT,
                run_index INTEGER NOT NULL,
                run_dir TEXT NOT NULL,
                input_path TEXT,
                output_path TEXT,
                raw_log_path TEXT,
                meta_path TEXT,
                log_state TEXT NOT NULL DEFAULT 'live',
                artifact_state TEXT NOT NULL DEFAULT 'full',
                log_truncated INTEGER NOT NULL DEFAULT 0,
                log_capture_degraded INTEGER NOT NULL DEFAULT 0,
                pruned_at TEXT,
                prune_reason TEXT,
                retention_summary_json TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create chat_runs table");
        sqlx::query(
            r#"
            CREATE TABLE chat_session_agents (
                id BLOB PRIMARY KEY,
                session_id BLOB NOT NULL,
                agent_id BLOB NOT NULL,
                state TEXT NOT NULL
                    CHECK (state IN ('idle','running','stopping','waitingapproval','dead')),
                workspace_path TEXT,
                pty_session_key TEXT,
                agent_session_id TEXT,
                agent_message_id TEXT,
                project_member_id BLOB,
                member_name TEXT NOT NULL DEFAULT '',
                execution_config TEXT NOT NULL DEFAULT '{}',
                allowed_skill_ids TEXT NOT NULL DEFAULT '[]',
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create chat_session_agents table");
        pool
    }

    async fn insert_running_member(pool: &SqlitePool, member: Uuid, data: &CreateQueuedMessage) {
        sqlx::query(
            "INSERT INTO chat_session_agents (
                 id, session_id, agent_id, state, member_name
             ) VALUES (?1, ?2, ?3, 'running', 'Target')",
        )
        .bind(member)
        .bind(data.session_id)
        .bind(data.agent_id)
        .execute(pool)
        .await
        .expect("insert running member");
    }

    async fn insert_idle_member(pool: &SqlitePool, member: Uuid, data: &CreateQueuedMessage) {
        sqlx::query(
            "INSERT INTO chat_session_agents (
                 id, session_id, agent_id, state, member_name
             ) VALUES (?1, ?2, ?3, 'idle', 'Target')",
        )
        .bind(member)
        .bind(data.session_id)
        .bind(data.agent_id)
        .execute(pool)
        .await
        .expect("insert idle member");
    }

    fn sample_create(session_agent_id: Uuid) -> CreateQueuedMessage {
        CreateQueuedMessage {
            session_id: Uuid::new_v4(),
            session_agent_id,
            agent_id: Uuid::new_v4(),
            chat_message_id: Uuid::new_v4(),
        }
    }

    async fn create_with_order(
        service: &QueuedMessageService,
        pool: &SqlitePool,
        session_agent_id: Uuid,
        seq: i64,
    ) -> QueuedMessage {
        let message = service
            .create_queued(pool, &sample_create(session_agent_id))
            .await
            .expect("create queued");
        sqlx::query("UPDATE chat_message_queue SET created_at = ?2 WHERE id = ?1")
            .bind(message.id)
            .bind(format!("2026-06-17T00:00:0{seq}.000"))
            .execute(pool)
            .await
            .expect("set created_at");
        message
    }

    #[tokio::test]
    async fn service_recovers_member_queue_from_database() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();

        let first = create_with_order(&service, &pool, member, 1).await;
        let second = create_with_order(&service, &pool, member, 2).await;

        let recovered = QueuedMessageService::new()
            .list_for_member(&pool, member)
            .await
            .expect("recover queue");

        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0].id, first.id);
        assert_eq!(recovered[1].id, second.id);
        assert!(service.has_queued(&pool, member).await.unwrap());
    }

    #[tokio::test]
    async fn service_reuses_delivery_and_versions_snapshots_monotonically() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let data = sample_create(member);
        let created = service
            .create_queued(&pool, &data)
            .await
            .expect("create delivery");
        let replay = service
            .create_queued(&pool, &data)
            .await
            .expect("replay delivery creation");
        assert_eq!(replay.id, created.id);

        let queued_snapshot = service
            .snapshot_for_member(&pool, data.session_id, member, data.agent_id)
            .await
            .expect("queued snapshot");
        let starting = service
            .claim_next(&pool, member)
            .await
            .expect("claim")
            .expect("delivery starts");
        let starting_snapshot = service
            .snapshot_for_member(&pool, data.session_id, member, data.agent_id)
            .await
            .expect("starting snapshot");

        assert_eq!(starting.id, created.id);
        assert_eq!(starting_snapshot.status, MemberQueueStatus::Starting);
        assert!(starting_snapshot.revision > queued_snapshot.revision);
        // Applying the responses in reverse order must leave the newer revision authoritative.
        let mut projected = starting_snapshot.clone();
        if queued_snapshot.revision > projected.revision {
            projected = queued_snapshot;
        }
        assert_eq!(projected.revision, starting_snapshot.revision);
        assert_eq!(projected.status, MemberQueueStatus::Starting);
    }

    #[tokio::test]
    async fn service_claims_binds_and_completes_with_cas_states() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let first = create_with_order(&service, &pool, member, 1).await;
        let second = create_with_order(&service, &pool, member, 2).await;

        let claimed = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim first");
        assert_eq!(claimed.id, first.id);
        assert_eq!(claimed.status, QueuedMessageStatus::Starting);
        assert!(service.claim_next(&pool, member).await.unwrap().is_none());

        let run_id = Uuid::new_v4();
        let running = service
            .bind_run(&pool, claimed.id, run_id)
            .await
            .unwrap()
            .expect("bind run");
        assert_eq!(running.status, QueuedMessageStatus::Running);
        assert_eq!(running.run_id, Some(run_id));

        let completed = service
            .mark_completed(&pool, claimed.id)
            .await
            .unwrap()
            .expect("complete");
        assert_eq!(completed.status, QueuedMessageStatus::Completed);

        let next = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim next");
        assert_eq!(next.id, second.id);
    }

    #[tokio::test]
    async fn run_creation_delivery_binding_member_state_and_revision_are_atomic() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let data = sample_create(member);
        insert_idle_member(&pool, member, &data).await;
        service
            .create_queued(&pool, &data)
            .await
            .expect("create delivery");
        let starting = service
            .claim_next(&pool, member)
            .await
            .expect("claim")
            .expect("starting delivery");
        let before_revision = service
            .current_runtime_revision(&pool, data.session_id)
            .await
            .unwrap();
        let run_id = Uuid::new_v4();
        let run_data = CreateChatRun {
            session_id: data.session_id,
            session_agent_id: member,
            workspace_path: Some("/tmp/workspace".to_string()),
            run_index: 1,
            run_dir: "/tmp/run".to_string(),
            input_path: None,
            output_path: None,
            raw_log_path: None,
            meta_path: None,
        };

        let binding = service
            .bind_delivery_to_new_run(&pool, starting.id, starting.revision, &run_data, run_id)
            .await
            .expect("bind transaction")
            .expect("binding applied");

        assert_eq!(binding.delivery.status, QueuedMessageStatus::Running);
        assert_eq!(binding.delivery.run_id, Some(run_id));
        assert_eq!(binding.member.state, ChatSessionAgentState::Running);
        assert_eq!(binding.run.id, run_id);
        assert!(binding.runtime_revision > before_revision);
        let stale_run_id = Uuid::new_v4();
        assert!(
            service
                .bind_delivery_to_new_run(
                    &pool,
                    starting.id,
                    starting.revision,
                    &CreateChatRun {
                        run_index: 2,
                        ..run_data
                    },
                    stale_run_id,
                )
                .await
                .expect("stale bind is not an error")
                .is_none()
        );
        assert!(
            ChatRun::find_by_id(&pool, stale_run_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn stop_transition_atomically_updates_waiting_delivery_member_and_revision() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let data = sample_create(member);
        insert_running_member(&pool, member, &data).await;
        service
            .create_queued(&pool, &data)
            .await
            .expect("create delivery");
        let starting = service
            .claim_next(&pool, member)
            .await
            .expect("claim delivery")
            .expect("starting delivery");
        let run_id = Uuid::new_v4();
        let running = service
            .bind_run(&pool, starting.id, run_id)
            .await
            .expect("bind run")
            .expect("running delivery");
        let waiting = service
            .transition_status_cas(
                &pool,
                running.id,
                running.revision,
                QueuedMessageStatus::Running,
                QueuedMessageStatus::WaitingApproval,
            )
            .await
            .expect("enter approval")
            .expect("waiting delivery");
        ChatSessionAgent::update_state(&pool, member, ChatSessionAgentState::WaitingApproval)
            .await
            .expect("project waiting member");
        let before_revision = service
            .current_runtime_revision(&pool, data.session_id)
            .await
            .expect("runtime revision before stop");

        assert!(
            service
                .transition_run_to_stopping_cas(
                    &pool,
                    waiting.id,
                    waiting.revision,
                    QueuedMessageStatus::WaitingApproval,
                    Uuid::new_v4(),
                    member,
                )
                .await
                .expect("wrong run is a CAS miss")
                .is_none()
        );
        assert_eq!(
            service
                .current_runtime_revision(&pool, data.session_id)
                .await
                .expect("revision after rejected stop"),
            before_revision
        );

        let stopped = service
            .transition_run_to_stopping_cas(
                &pool,
                waiting.id,
                waiting.revision,
                QueuedMessageStatus::WaitingApproval,
                run_id,
                member,
            )
            .await
            .expect("stop transaction")
            .expect("stop applied");

        assert_eq!(stopped.delivery.id, waiting.id);
        assert_eq!(stopped.delivery.status, QueuedMessageStatus::Stopping);
        assert_eq!(stopped.delivery.revision, waiting.revision + 1);
        assert_eq!(stopped.delivery.run_id, Some(run_id));
        assert_eq!(stopped.member.id, member);
        assert_eq!(stopped.member.state, ChatSessionAgentState::Stopping);
        assert_eq!(stopped.runtime_revision, before_revision + 1);
        assert!(
            service
                .transition_run_to_stopping_cas(
                    &pool,
                    waiting.id,
                    waiting.revision,
                    QueuedMessageStatus::WaitingApproval,
                    run_id,
                    member,
                )
                .await
                .expect("stale repeated stop")
                .is_none()
        );
    }

    #[tokio::test]
    async fn stop_transition_rolls_back_delivery_when_member_update_fails() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let data = sample_create(member);
        insert_running_member(&pool, member, &data).await;
        service
            .create_queued(&pool, &data)
            .await
            .expect("create delivery");
        let starting = service
            .claim_next(&pool, member)
            .await
            .expect("claim delivery")
            .expect("starting delivery");
        let run_id = Uuid::new_v4();
        let running = service
            .bind_run(&pool, starting.id, run_id)
            .await
            .expect("bind run")
            .expect("running delivery");
        sqlx::query(
            "CREATE TRIGGER reject_member_stopping
             BEFORE UPDATE OF state ON chat_session_agents
             WHEN NEW.state = 'stopping'
             BEGIN
                 SELECT RAISE(ABORT, 'reject stopping projection');
             END",
        )
        .execute(&pool)
        .await
        .expect("install member failure trigger");
        let before_revision = service
            .current_runtime_revision(&pool, data.session_id)
            .await
            .expect("runtime revision before failed stop");

        assert!(
            service
                .transition_run_to_stopping_cas(
                    &pool,
                    running.id,
                    running.revision,
                    QueuedMessageStatus::Running,
                    run_id,
                    member,
                )
                .await
                .is_err()
        );

        let persisted = service
            .find_by_id(&pool, running.id)
            .await
            .expect("load delivery after rollback")
            .expect("delivery exists");
        assert_eq!(persisted.status, QueuedMessageStatus::Running);
        assert_eq!(persisted.revision, running.revision);
        assert_eq!(
            ChatSessionAgent::find_by_id(&pool, member)
                .await
                .expect("load member after rollback")
                .expect("member exists")
                .state,
            ChatSessionAgentState::Running
        );
        assert_eq!(
            service
                .current_runtime_revision(&pool, data.session_id)
                .await
                .expect("runtime revision after rollback"),
            before_revision
        );
    }

    #[tokio::test]
    async fn terminal_finalize_wins_over_stale_stop_projection() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let data = sample_create(member);
        insert_running_member(&pool, member, &data).await;
        service
            .create_queued(&pool, &data)
            .await
            .expect("create delivery");
        let starting = service
            .claim_next(&pool, member)
            .await
            .expect("claim delivery")
            .expect("starting delivery");
        let run_id = Uuid::new_v4();
        let running = service
            .bind_run(&pool, starting.id, run_id)
            .await
            .expect("bind run")
            .expect("running delivery");

        // Deterministically schedule the old two-write race: its delivery write lands, terminal
        // finalization wins next, then the stale stop request resumes with its original CAS data.
        let legacy_stopping = service
            .transition_status_cas(
                &pool,
                running.id,
                running.revision,
                QueuedMessageStatus::Running,
                QueuedMessageStatus::Stopping,
            )
            .await
            .expect("legacy delivery stop")
            .expect("delivery moved to stopping");
        let terminal = service
            .finalize_completed_run_cas(&pool, run_id, member, legacy_stopping.revision, false)
            .await
            .expect("terminal finalize wins race");
        assert!(terminal.applied);

        assert!(
            service
                .transition_run_to_stopping_cas(
                    &pool,
                    running.id,
                    running.revision,
                    QueuedMessageStatus::Running,
                    run_id,
                    member,
                )
                .await
                .expect("stale stop is a CAS miss")
                .is_none()
        );
        assert_eq!(
            ChatSessionAgent::find_by_id(&pool, member)
                .await
                .expect("load member after race")
                .expect("member exists")
                .state,
            ChatSessionAgentState::Idle
        );
        let completed = service
            .find_by_id(&pool, running.id)
            .await
            .expect("load delivery after race")
            .expect("delivery exists");
        assert_eq!(completed.status, QueuedMessageStatus::Completed);
        assert_eq!(
            service
                .current_runtime_revision(&pool, data.session_id)
                .await
                .expect("runtime revision after stale stop"),
            terminal.runtime_revision
        );
    }

    #[tokio::test]
    async fn fail_or_skip_inflight_cas_persists_reason_and_revision() {
        for next_status in [QueuedMessageStatus::Failed, QueuedMessageStatus::Skipped] {
            let pool = setup_pool().await;
            let service = QueuedMessageService::new();
            let member = Uuid::new_v4();
            let data = sample_create(member);
            service
                .create_queued(&pool, &data)
                .await
                .expect("create delivery");
            let starting = service
                .claim_next(&pool, member)
                .await
                .expect("claim delivery")
                .expect("starting delivery");
            let before_revision = service
                .current_runtime_revision(&pool, data.session_id)
                .await
                .expect("runtime revision before failure");
            let reason = format!("{next_status:?} diagnostic");

            let finalized = service
                .fail_or_skip_inflight_cas(
                    &pool,
                    starting.id,
                    starting.revision,
                    QueuedMessageStatus::Starting,
                    next_status,
                    Some(reason.clone()),
                )
                .await
                .expect("finalize exact attempt")
                .expect("failure CAS applied");

            assert_eq!(finalized.status, next_status);
            assert_eq!(finalized.revision, starting.revision + 1);
            assert_eq!(finalized.failure_reason.as_deref(), Some(reason.as_str()));
            assert_eq!(
                service
                    .current_runtime_revision(&pool, data.session_id)
                    .await
                    .expect("runtime revision after failure"),
                before_revision + 1
            );
        }
    }

    #[tokio::test]
    async fn fail_or_skip_inflight_cas_miss_has_no_side_effects() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let data = sample_create(member);
        service
            .create_queued(&pool, &data)
            .await
            .expect("create delivery");
        let starting = service
            .claim_next(&pool, member)
            .await
            .expect("claim delivery")
            .expect("starting delivery");
        let before_revision = service
            .current_runtime_revision(&pool, data.session_id)
            .await
            .expect("runtime revision before invalid target");

        assert!(
            service
                .fail_or_skip_inflight_cas(
                    &pool,
                    starting.id,
                    starting.revision,
                    QueuedMessageStatus::Starting,
                    QueuedMessageStatus::Completed,
                    Some("must not persist".to_string()),
                )
                .await
                .is_err()
        );
        assert_eq!(
            service
                .current_runtime_revision(&pool, data.session_id)
                .await
                .expect("revision after invalid target"),
            before_revision
        );

        let failed = service
            .fail_or_skip_inflight_cas(
                &pool,
                starting.id,
                starting.revision,
                QueuedMessageStatus::Starting,
                QueuedMessageStatus::Failed,
                Some("first attempt failed".to_string()),
            )
            .await
            .expect("first failure transition")
            .expect("first failure applied");
        let applied_revision = service
            .current_runtime_revision(&pool, data.session_id)
            .await
            .expect("revision after first failure");

        assert!(
            service
                .fail_or_skip_inflight_cas(
                    &pool,
                    starting.id,
                    starting.revision,
                    QueuedMessageStatus::Starting,
                    QueuedMessageStatus::Skipped,
                    Some("stale attempt must not overwrite".to_string()),
                )
                .await
                .expect("stale failure CAS")
                .is_none()
        );
        let persisted = service
            .find_by_id(&pool, starting.id)
            .await
            .expect("load finalized delivery")
            .expect("delivery exists");
        assert_eq!(persisted.status, QueuedMessageStatus::Failed);
        assert_eq!(persisted.revision, failed.revision);
        assert_eq!(
            persisted.failure_reason.as_deref(),
            Some("first attempt failed")
        );
        assert_eq!(
            service
                .current_runtime_revision(&pool, data.session_id)
                .await
                .expect("revision after stale failure"),
            applied_revision
        );
    }

    #[tokio::test]
    async fn failure_blocks_until_continue_resolves_failed_item() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        create_with_order(&service, &pool, member, 1).await;
        create_with_order(&service, &pool, member, 2).await;

        let claimed = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim first");
        let failed = service
            .mark_failed(&pool, claimed.id, Some("boom".to_string()))
            .await
            .unwrap()
            .expect("fail");
        assert_eq!(failed.status, QueuedMessageStatus::Failed);

        match service.get_status(&pool, member).await.unwrap() {
            QueueStatus::Blocked {
                message,
                queued_count,
            } => {
                assert_eq!(message.id, claimed.id);
                assert_eq!(queued_count, 1);
            }
            other => panic!("expected blocked status, got {other:?}"),
        }
        assert!(service.claim_next(&pool, member).await.unwrap().is_none());

        assert_eq!(
            service.skip_failed_for_member(&pool, member).await.unwrap(),
            1
        );
        let resolved_failure = service
            .find_by_id(&pool, failed.id)
            .await
            .unwrap()
            .expect("resolved failure remains persisted");
        assert_eq!(resolved_failure.status, QueuedMessageStatus::Failed);
        assert!(resolved_failure.failure_resolved_at.is_some());
        let next = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim after continue");
        assert_eq!(next.status, QueuedMessageStatus::Starting);
    }

    #[tokio::test]
    async fn snapshot_exposes_blocked_state_and_item_actions() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let first = create_with_order(&service, &pool, member, 1).await;
        let second = create_with_order(&service, &pool, member, 2).await;

        let claimed = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim first");
        service
            .mark_failed(&pool, claimed.id, Some("boom".to_string()))
            .await
            .unwrap()
            .expect("fail");

        let snapshot = service
            .snapshot_for_member(&pool, first.session_id, member, first.agent_id)
            .await
            .expect("snapshot");

        assert_eq!(snapshot.status, MemberQueueStatus::Blocked);
        assert!(snapshot.blocked);
        assert!(!snapshot.paused);
        assert!(snapshot.can_continue);
        assert_eq!(snapshot.queued_count, 1);
        assert_eq!(snapshot.items.len(), 2);
        assert_eq!(snapshot.items[0].message.id, first.id);
        assert!(!snapshot.items[0].can_delete);
        assert_eq!(snapshot.items[1].message.id, second.id);
        assert!(snapshot.items[1].can_delete);
    }

    #[tokio::test]
    async fn complete_run_and_claim_next_returns_processing_entry() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let first = create_with_order(&service, &pool, member, 1).await;
        let second = create_with_order(&service, &pool, member, 2).await;
        let claimed = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim first");
        assert_eq!(claimed.id, first.id);
        let run_id = Uuid::new_v4();
        service
            .bind_run(&pool, claimed.id, run_id)
            .await
            .unwrap()
            .expect("bind run");

        let next = service
            .complete_run_and_claim_next(&pool, run_id, member)
            .await
            .unwrap()
            .expect("claim next");

        assert_eq!(next.id, second.id);
        assert_eq!(next.status, QueuedMessageStatus::Starting);
    }

    #[tokio::test]
    async fn completed_run_atomically_updates_member_and_claims_next() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let first_data = sample_create(member);
        insert_running_member(&pool, member, &first_data).await;
        let first = service
            .create_queued(&pool, &first_data)
            .await
            .expect("create first queue row");
        let second = service
            .create_queued(&pool, &sample_create(member))
            .await
            .expect("create second queue row");
        sqlx::query(
            "UPDATE chat_message_queue
             SET created_at = CASE id WHEN ?1 THEN ?3 WHEN ?2 THEN ?4 END
             WHERE id IN (?1, ?2)",
        )
        .bind(first.id)
        .bind(second.id)
        .bind("2026-08-12T00:00:01.000")
        .bind("2026-08-12T00:00:02.000")
        .execute(&pool)
        .await
        .expect("order queue rows");
        let claimed = service
            .claim_next(&pool, member)
            .await
            .expect("claim first")
            .expect("first claimed");
        assert_eq!(claimed.id, first.id);
        let run_id = Uuid::new_v4();
        let running = service
            .bind_run(&pool, claimed.id, run_id)
            .await
            .expect("bind run")
            .expect("run bound");

        let stale = service
            .finalize_completed_run_cas(&pool, run_id, member, running.revision - 1, true)
            .await
            .expect("stale CAS is handled");
        assert!(!stale.applied);
        assert!(stale.next.is_none());

        let finalized = service
            .finalize_completed_run_cas(&pool, run_id, member, running.revision, true)
            .await
            .expect("finalize completed run");

        assert!(finalized.applied);
        assert_eq!(
            finalized.next.as_ref().map(|entry| entry.id),
            Some(second.id)
        );
        let member_state: String =
            sqlx::query_scalar("SELECT state FROM chat_session_agents WHERE id = ?1")
                .bind(member)
                .fetch_one(&pool)
                .await
                .expect("load member state");
        assert_eq!(member_state, "idle");
        assert_eq!(
            service
                .find_by_id(&pool, first.id)
                .await
                .expect("load first")
                .expect("first exists")
                .status,
            QueuedMessageStatus::Completed
        );

        let stale = service
            .finalize_completed_run(&pool, run_id, member, true)
            .await
            .expect("repeat stale finalization");
        assert!(!stale.applied);
        assert!(stale.next.is_none());
    }

    #[tokio::test]
    async fn failed_run_atomically_updates_member_and_blocks_waiting_work() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let first_data = sample_create(member);
        insert_running_member(&pool, member, &first_data).await;
        let first = service
            .create_queued(&pool, &first_data)
            .await
            .expect("create first queue row");
        let second = service
            .create_queued(&pool, &sample_create(member))
            .await
            .expect("create second queue row");
        sqlx::query(
            "UPDATE chat_message_queue
             SET created_at = CASE id WHEN ?1 THEN ?3 WHEN ?2 THEN ?4 END
             WHERE id IN (?1, ?2)",
        )
        .bind(first.id)
        .bind(second.id)
        .bind("2026-08-12T00:00:01.000")
        .bind("2026-08-12T00:00:02.000")
        .execute(&pool)
        .await
        .expect("order queue rows");
        let claimed = service
            .claim_next(&pool, member)
            .await
            .expect("claim first")
            .expect("first claimed");
        let run_id = Uuid::new_v4();
        service
            .bind_run(&pool, claimed.id, run_id)
            .await
            .expect("bind run")
            .expect("run bound");

        let applied = service
            .finalize_failed_run(&pool, run_id, member, Some("startup failed".to_string()))
            .await
            .expect("finalize failed run");

        assert!(applied);
        let member_state: String =
            sqlx::query_scalar("SELECT state FROM chat_session_agents WHERE id = ?1")
                .bind(member)
                .fetch_one(&pool)
                .await
                .expect("load member state");
        assert_eq!(member_state, "dead");
        let failed = service
            .find_by_id(&pool, first.id)
            .await
            .expect("load first")
            .expect("first exists");
        assert_eq!(failed.status, QueuedMessageStatus::Failed);
        assert_eq!(failed.failure_reason.as_deref(), Some("startup failed"));
        assert!(
            service
                .claim_next(&pool, member)
                .await
                .expect("blocked claim")
                .is_none()
        );

        assert!(
            !service
                .finalize_failed_run(&pool, run_id, member, None)
                .await
                .expect("repeat stale finalization")
        );
    }

    #[tokio::test]
    async fn failed_member_without_remaining_queue_is_paused() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        create_with_order(&service, &pool, member, 1).await;

        let claimed = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim only item");
        service
            .mark_failed(&pool, claimed.id, Some("boom".to_string()))
            .await
            .unwrap()
            .expect("fail");

        match service.get_status(&pool, member).await.unwrap() {
            QueueStatus::Paused {
                message,
                queued_count,
            } => {
                assert_eq!(message.id, claimed.id);
                assert_eq!(queued_count, 0);
            }
            other => panic!("expected paused status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delete_only_removes_rows_that_are_still_queued() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let queued = create_with_order(&service, &pool, member, 1).await;
        let to_claim = create_with_order(&service, &pool, member, 2).await;

        assert_eq!(service.delete_queued(&pool, queued.id).await.unwrap(), 1);

        let claimed = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim remaining");
        assert_eq!(claimed.id, to_claim.id);
        assert_eq!(service.delete_queued(&pool, claimed.id).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn paused_snapshot_without_queued_messages_hides_continue() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let entry = create_with_order(&service, &pool, member, 1).await;

        let claimed = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim only item");
        service
            .mark_failed(&pool, claimed.id, Some("boom".to_string()))
            .await
            .unwrap()
            .expect("fail");

        let snapshot = service
            .snapshot_for_member(&pool, entry.session_id, member, entry.agent_id)
            .await
            .expect("snapshot");

        assert_eq!(snapshot.status, MemberQueueStatus::Paused);
        assert!(snapshot.blocked);
        assert!(snapshot.paused);
        // No queued messages waiting → "continue execution" is not meaningful.
        assert!(!snapshot.can_continue);
        assert_eq!(snapshot.queued_count, 0);
    }

    #[tokio::test]
    async fn blocked_snapshot_with_queued_messages_shows_continue() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let first = create_with_order(&service, &pool, member, 1).await;
        create_with_order(&service, &pool, member, 2).await;

        let claimed = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim first");
        service
            .mark_failed(&pool, claimed.id, Some("boom".to_string()))
            .await
            .unwrap()
            .expect("fail");

        let snapshot = service
            .snapshot_for_member(&pool, first.session_id, member, first.agent_id)
            .await
            .expect("snapshot");

        assert_eq!(snapshot.status, MemberQueueStatus::Blocked);
        assert!(snapshot.blocked);
        // Queued messages are waiting → "continue execution" is meaningful.
        assert!(snapshot.can_continue);
        assert_eq!(snapshot.queued_count, 1);
    }

    #[tokio::test]
    async fn skip_inflight_cleans_up_lone_failure() {
        let pool = setup_pool().await;
        let service = QueuedMessageService::new();
        let member = Uuid::new_v4();
        let entry = create_with_order(&service, &pool, member, 1).await;

        let claimed = service
            .claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim");

        let skipped = service
            .skip_inflight(&pool, claimed.id, Some("auto-skip".to_string()))
            .await
            .unwrap()
            .expect("skip inflight");
        assert_eq!(skipped.status, QueuedMessageStatus::Skipped);

        // No blocking failure and no queued messages — the queue is clean.
        assert!(!service.has_blocking_failure(&pool, member).await.unwrap());
        assert!(!service.has_queued(&pool, member).await.unwrap());

        let snapshot = service
            .snapshot_for_member(&pool, entry.session_id, member, entry.agent_id)
            .await
            .expect("snapshot");
        assert_eq!(snapshot.status, MemberQueueStatus::Empty);
        assert!(!snapshot.can_continue);
    }
}
