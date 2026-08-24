use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction, Type};
use ts_rs::TS;
use uuid::Uuid;

const CHAT_MESSAGE_QUEUE_COLUMNS: &str = r#"
    id,
    session_id,
    session_agent_id,
    agent_id,
    chat_message_id,
    status,
    revision,
    attempt_no,
    processing_started_at,
    run_id,
    failure_reason,
    failure_resolved_at,
    created_at,
    updated_at
"#;

/// Lifecycle of a single queued member message.
///
/// `queued` -> `starting` (claimed atomically) -> `running` (bound to a run) -> a terminal
/// state. `waiting_approval` and `stopping` are persisted runtime states, rather than frontend
/// guesses. `processing` remains readable during rolling upgrades but new claims use `starting`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Type, Serialize, Deserialize, TS)]
#[sqlx(type_name = "chat_message_queue_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[ts(use_ts_enum)]
pub enum QueuedMessageStatus {
    Queued,
    Starting,
    /// Legacy spelling for a claimed delivery. New writes use [`Self::Starting`].
    Processing,
    Running,
    WaitingApproval,
    Stopping,
    Failed,
    Cancelled,
    Skipped,
    Completed,
}

impl QueuedMessageStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Starting
                | Self::Processing
                | Self::Running
                | Self::WaitingApproval
                | Self::Stopping
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::Cancelled | Self::Skipped | Self::Completed
        )
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        match self {
            Self::Queued => matches!(next, Self::Starting | Self::Cancelled),
            Self::Starting | Self::Processing => matches!(
                next,
                Self::Running | Self::Failed | Self::Cancelled | Self::Skipped
            ),
            Self::Running => matches!(
                next,
                Self::WaitingApproval
                    | Self::Stopping
                    | Self::Completed
                    | Self::Failed
                    | Self::Cancelled
                    | Self::Skipped
            ),
            Self::WaitingApproval => matches!(
                next,
                Self::Running | Self::Stopping | Self::Completed | Self::Failed | Self::Cancelled
            ),
            Self::Stopping => matches!(next, Self::Completed | Self::Failed | Self::Cancelled),
            Self::Failed | Self::Cancelled | Self::Skipped | Self::Completed => false,
        }
    }

    /// Recovery is deliberately outside the normal delivery graph. Only a supervisor that has
    /// established the owning executor is orphaned may return an active attempt to `queued`.
    pub fn can_recover_to_queued(self) -> bool {
        self.is_active()
    }
}

/// A durable, member-scoped queue entry referencing an existing `chat_messages` row.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct ChatMessageQueue {
    pub id: Uuid,
    pub session_id: Uuid,
    pub session_agent_id: Uuid,
    pub agent_id: Uuid,
    pub chat_message_id: Uuid,
    pub status: QueuedMessageStatus,
    /// Monotonic compare-and-set version for this delivery row.
    pub revision: i64,
    /// Number of times this delivery has been claimed from `queued`.
    pub attempt_no: i64,
    pub processing_started_at: Option<DateTime<Utc>>,
    pub run_id: Option<Uuid>,
    pub failure_reason: Option<String>,
    /// Set when the user continues past a failed terminal delivery.
    pub failure_resolved_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateChatMessageQueue {
    pub session_id: Uuid,
    pub session_agent_id: Uuid,
    pub agent_id: Uuid,
    pub chat_message_id: Uuid,
}

impl ChatMessageQueue {
    pub async fn find_by_id(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_MESSAGE_QUEUE_COLUMNS} FROM chat_message_queue WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
    }

    pub async fn current_runtime_revision(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COALESCE((
                 SELECT revision
                 FROM chat_session_runtime_revisions
                 WHERE session_id = ?1
             ), 0)",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
    }

    pub async fn current_runtime_revision_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        session_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COALESCE((
                 SELECT revision
                 FROM chat_session_runtime_revisions
                 WHERE session_id = ?1
             ), 0)",
        )
        .bind(session_id)
        .fetch_one(&mut **transaction)
        .await
    }

    /// All entries for a member, oldest first. Used to recover and display the queue.
    pub async fn list_for_member(
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_MESSAGE_QUEUE_COLUMNS}
             FROM chat_message_queue
             WHERE session_agent_id = ?1
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(session_agent_id)
        .fetch_all(pool)
        .await
    }

    pub async fn list_for_member_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        session_agent_id: Uuid,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_MESSAGE_QUEUE_COLUMNS}
             FROM chat_message_queue
             WHERE session_agent_id = ?1
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(session_agent_id)
        .fetch_all(&mut **transaction)
        .await
    }

    pub async fn list_for_message(
        pool: &SqlitePool,
        chat_message_id: Uuid,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_MESSAGE_QUEUE_COLUMNS}
             FROM chat_message_queue
             WHERE chat_message_id = ?1
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(chat_message_id)
        .fetch_all(pool)
        .await
    }

    pub async fn list_for_message_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        chat_message_id: Uuid,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_MESSAGE_QUEUE_COLUMNS}
             FROM chat_message_queue
             WHERE chat_message_id = ?1
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(chat_message_id)
        .fetch_all(&mut **transaction)
        .await
    }

    /// The currently active delivery for a member, if any.
    pub async fn find_active_for_member(
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_MESSAGE_QUEUE_COLUMNS}
             FROM chat_message_queue
             WHERE session_agent_id = ?1
               AND status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
             LIMIT 1"
        ))
        .bind(session_agent_id)
        .fetch_optional(pool)
        .await
    }

    /// Starting entries that were claimed but never bound to a run.
    ///
    /// These rows can survive a backend interruption even when the session-agent state was
    /// already persisted as `idle` or `dead`, so recovery cannot discover them from agent state
    /// alone.
    pub async fn list_unbound_processing(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_MESSAGE_QUEUE_COLUMNS}
             FROM chat_message_queue
             WHERE status IN ('starting', 'processing') AND run_id IS NULL
             ORDER BY created_at ASC, id ASC"
        ))
        .fetch_all(pool)
        .await
    }

    /// Members with durable work waiting to be claimed. Used at backend startup so a crash after
    /// enqueue but before task scheduling cannot strand an otherwise idle member.
    pub async fn list_members_with_queued(pool: &SqlitePool) -> Result<Vec<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT DISTINCT session_agent_id
             FROM chat_message_queue
             WHERE status = 'queued'
             ORDER BY session_agent_id",
        )
        .fetch_all(pool)
        .await
    }

    pub async fn count_queued_for_member(
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM chat_message_queue
             WHERE session_agent_id = ?1 AND status = 'queued'",
        )
        .bind(session_agent_id)
        .fetch_one(pool)
        .await?;
        Ok(count)
    }

    /// True when the member queue is blocked: a `failed` entry is awaiting user action.
    pub async fn has_blocking_failure(
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let (exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(
                 SELECT 1 FROM chat_message_queue
                 WHERE session_agent_id = ?1
                   AND status = 'failed'
                   AND failure_resolved_at IS NULL
             )",
        )
        .bind(session_agent_id)
        .fetch_one(pool)
        .await?;
        Ok(exists)
    }

    pub async fn create_queued(
        pool: &SqlitePool,
        data: &CreateChatMessageQueue,
        id: Uuid,
    ) -> Result<Self, sqlx::Error> {
        let mut transaction = pool.begin().await?;
        let row = Self::create_queued_in_transaction(&mut transaction, data, id).await?;
        transaction.commit().await?;
        Ok(row)
    }

    pub async fn create_queued_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        data: &CreateChatMessageQueue,
        id: Uuid,
    ) -> Result<Self, sqlx::Error> {
        let inserted = sqlx::query_as::<_, Self>(&format!(
            "INSERT INTO chat_message_queue (
                 id, session_id, session_agent_id, agent_id, chat_message_id, status
             )
             VALUES (?1, ?2, ?3, ?4, ?5, 'queued')
             ON CONFLICT(chat_message_id, session_agent_id) DO NOTHING
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .bind(data.session_id)
        .bind(data.session_agent_id)
        .bind(data.agent_id)
        .bind(data.chat_message_id)
        .fetch_optional(&mut **transaction)
        .await?;

        if let Some(row) = inserted {
            return Ok(row);
        }

        let existing = sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_MESSAGE_QUEUE_COLUMNS}
             FROM chat_message_queue
             WHERE chat_message_id = ?1 AND session_agent_id = ?2"
        ))
        .bind(data.chat_message_id)
        .bind(data.session_agent_id)
        .fetch_one(&mut **transaction)
        .await?;
        if existing.session_id != data.session_id || existing.agent_id != data.agent_id {
            return Err(sqlx::Error::Protocol(
                "delivery key belongs to a different session or agent".to_string(),
            ));
        }
        Ok(existing)
    }

    /// Atomically claim the oldest `queued` entry for a member and move it to `starting`.
    ///
    /// Returns `None` when there is nothing to claim, the member already has an in-flight entry,
    /// or the member is blocked by a `failed` entry. The `NOT EXISTS` guard (backed by the
    /// partial unique index) keeps a member to a single in-flight entry, and the `failed` clause
    /// enforces "stop on failure": a failed entry must first be resolved via
    /// [`Self::skip_failed_for_member`] before the queue can advance.
    pub async fn claim_next(
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'starting',
                 revision = revision + 1,
                 attempt_no = attempt_no + 1,
                 processing_started_at = datetime('now', 'subsec'),
                 updated_at = datetime('now', 'subsec')
             WHERE id = (
                 SELECT id FROM chat_message_queue
                 WHERE session_agent_id = ?1 AND status = 'queued'
                 ORDER BY created_at ASC, id ASC
                 LIMIT 1
             )
             AND NOT EXISTS (
                 SELECT 1 FROM chat_message_queue
                 WHERE session_agent_id = ?1
                   AND (
                       status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
                       OR (status = 'failed' AND failure_resolved_at IS NULL)
                   )
             )
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(session_agent_id)
        .fetch_optional(pool)
        .await
    }

    /// Look up the queue entry currently bound to a run.
    pub async fn find_by_run_id(
        pool: &SqlitePool,
        run_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_MESSAGE_QUEUE_COLUMNS}
             FROM chat_message_queue
             WHERE run_id = ?1
             LIMIT 1"
        ))
        .bind(run_id)
        .fetch_optional(pool)
        .await
    }

    /// Bind a message to a starting run and move it to `running`, creating the row if it does not
    /// already exist.
    ///
    /// The delivery row is created idempotently before it is advanced. Repeating this operation
    /// with the same source/target and run returns the same stable delivery without advancing its
    /// revision again.
    pub async fn start_or_create_running(
        pool: &SqlitePool,
        data: &CreateChatMessageQueue,
        id: Uuid,
        run_id: Uuid,
    ) -> Result<Self, sqlx::Error> {
        let mut transaction = pool.begin().await?;
        let delivery = Self::create_queued_in_transaction(&mut transaction, data, id).await?;
        if delivery.status == QueuedMessageStatus::Running && delivery.run_id == Some(run_id) {
            transaction.commit().await?;
            return Ok(delivery);
        }

        let running = sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'running',
                 revision = revision + 1,
                 attempt_no = CASE WHEN status = 'queued' THEN attempt_no + 1 ELSE attempt_no END,
                 run_id = ?3,
                 processing_started_at = COALESCE(processing_started_at, datetime('now', 'subsec')),
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1
               AND revision = ?2
               AND status IN ('queued', 'starting', 'processing')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(delivery.id)
        .bind(delivery.revision)
        .bind(run_id)
        .fetch_optional(&mut *transaction)
        .await?;

        let Some(running) = running else {
            transaction.rollback().await?;
            return Err(sqlx::Error::RowNotFound);
        };
        transaction.commit().await?;
        Ok(running)
    }

    pub async fn recover_inflight_cas(
        pool: &SqlitePool,
        id: Uuid,
        expected_revision: i64,
        expected_status: QueuedMessageStatus,
    ) -> Result<Option<Self>, sqlx::Error> {
        if !expected_status.can_recover_to_queued() {
            return Err(sqlx::Error::Protocol(format!(
                "delivery status {expected_status:?} cannot be recovered to queued"
            )));
        }
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'queued',
                 revision = revision + 1,
                 run_id = NULL,
                 processing_started_at = NULL,
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1 AND revision = ?2 AND status = ?3
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .bind(expected_revision)
        .bind(expected_status)
        .fetch_optional(pool)
        .await
    }

    /// Supervisor-only recovery wrapper. Each candidate is guarded by its observed revision and
    /// state so a concurrent stop or finalization wins over recovery.
    pub async fn requeue_stale_inflight(
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let candidates = Self::list_for_member(pool, session_agent_id).await?;
        let mut recovered = 0;
        for candidate in candidates
            .into_iter()
            .filter(|candidate| candidate.status.can_recover_to_queued())
        {
            if Self::recover_inflight_cas(pool, candidate.id, candidate.revision, candidate.status)
                .await?
                .is_some()
            {
                recovered += 1;
            }
        }
        Ok(recovered)
    }

    /// Bind a claimed (`starting`) entry to its run and move it to `running`.
    pub async fn bind_run(
        pool: &SqlitePool,
        id: Uuid,
        run_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'running',
                 revision = revision + 1,
                 run_id = ?2,
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1 AND status IN ('starting', 'processing')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .bind(run_id)
        .fetch_optional(pool)
        .await
    }

    pub async fn bind_run_cas_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        id: Uuid,
        expected_revision: i64,
        run_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'running',
                 revision = revision + 1,
                 run_id = ?3,
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1
               AND revision = ?2
               AND status IN ('starting', 'processing')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .bind(expected_revision)
        .bind(run_id)
        .fetch_optional(&mut **transaction)
        .await
    }

    /// Apply a legal delivery state transition guarded by the row revision and prior status.
    pub async fn transition_status_cas(
        pool: &SqlitePool,
        id: Uuid,
        expected_revision: i64,
        expected_status: QueuedMessageStatus,
        next_status: QueuedMessageStatus,
    ) -> Result<Option<Self>, sqlx::Error> {
        if !expected_status.can_transition_to(next_status) {
            return Err(sqlx::Error::Protocol(format!(
                "illegal delivery transition: {expected_status:?} -> {next_status:?}"
            )));
        }

        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = ?4,
                 revision = revision + 1,
                 attempt_no = CASE
                     WHEN status = 'queued' AND ?4 = 'starting' THEN attempt_no + 1
                     ELSE attempt_no
                 END,
                 run_id = CASE WHEN ?4 = 'queued' THEN NULL ELSE run_id END,
                 processing_started_at = CASE
                     WHEN ?4 = 'queued' THEN NULL
                     WHEN ?4 = 'starting' THEN COALESCE(
                         processing_started_at,
                         datetime('now', 'subsec')
                     )
                     ELSE processing_started_at
                 END,
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1 AND revision = ?2 AND status = ?3
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .bind(expected_revision)
        .bind(expected_status)
        .bind(next_status)
        .fetch_optional(pool)
        .await
    }

    /// Finalize one exact in-flight delivery attempt while preserving its failure reason.
    pub async fn fail_or_skip_inflight_cas(
        pool: &SqlitePool,
        id: Uuid,
        expected_revision: i64,
        expected_status: QueuedMessageStatus,
        next_status: QueuedMessageStatus,
        failure_reason: Option<String>,
    ) -> Result<Option<Self>, sqlx::Error> {
        if !expected_status.is_active() {
            return Err(sqlx::Error::Protocol(format!(
                "cannot fail terminal or queued delivery from {expected_status:?}"
            )));
        }
        if !matches!(
            next_status,
            QueuedMessageStatus::Failed | QueuedMessageStatus::Skipped
        ) {
            return Err(sqlx::Error::Protocol(format!(
                "failure transition target must be failed or skipped, got {next_status:?}"
            )));
        }

        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = ?4,
                 revision = revision + 1,
                 failure_reason = ?5,
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1
               AND revision = ?2
               AND status = ?3
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .bind(expected_revision)
        .bind(expected_status)
        .bind(next_status)
        .bind(failure_reason)
        .fetch_optional(pool)
        .await
    }

    /// Move a run-bound delivery to `stopping` while its owning member is still active.
    ///
    /// This is transaction-only so callers can update the member projection in the same commit.
    pub async fn transition_run_to_stopping_cas_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        id: Uuid,
        expected_revision: i64,
        expected_status: QueuedMessageStatus,
        run_id: Uuid,
        session_agent_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        if !matches!(
            expected_status,
            QueuedMessageStatus::Running | QueuedMessageStatus::WaitingApproval
        ) {
            return Err(sqlx::Error::Protocol(format!(
                "illegal stop transition from {expected_status:?}"
            )));
        }

        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'stopping',
                 revision = revision + 1,
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1
               AND revision = ?2
               AND status = ?3
               AND run_id = ?4
               AND session_agent_id = ?5
               AND EXISTS (
                   SELECT 1
                   FROM chat_session_agents member
                   WHERE member.id = chat_message_queue.session_agent_id
                     AND member.session_id = chat_message_queue.session_id
                     AND member.agent_id = chat_message_queue.agent_id
                     AND member.state IN ('running', 'waitingapproval')
               )
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .bind(expected_revision)
        .bind(expected_status)
        .bind(run_id)
        .bind(session_agent_id)
        .fetch_optional(&mut **transaction)
        .await
    }

    /// Mark an in-flight entry `completed` on success or normal stop.
    pub async fn mark_completed(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'completed',
                 revision = revision + 1,
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1
               AND status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
    }

    /// Complete the row bound to `run_id` and atomically claim this member's next queued row.
    pub async fn complete_run_and_claim_next(
        pool: &SqlitePool,
        run_id: Uuid,
        session_agent_id: Uuid,
    ) -> Result<(Option<Self>, Option<Self>), sqlx::Error> {
        let mut tx = pool.begin().await?;

        let completed = sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'completed',
                 revision = revision + 1,
                 updated_at = datetime('now', 'subsec')
             WHERE run_id = ?1
               AND status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(run_id)
        .fetch_optional(&mut *tx)
        .await?;

        let claimed = sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'starting',
                 revision = revision + 1,
                 attempt_no = attempt_no + 1,
                 processing_started_at = datetime('now', 'subsec'),
                 updated_at = datetime('now', 'subsec')
             WHERE id = (
                 SELECT id FROM chat_message_queue
                 WHERE session_agent_id = ?1 AND status = 'queued'
                 ORDER BY created_at ASC, id ASC
                 LIMIT 1
             )
             AND NOT EXISTS (
                 SELECT 1 FROM chat_message_queue
                 WHERE session_agent_id = ?1
                   AND (
                       status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
                       OR (status = 'failed' AND failure_resolved_at IS NULL)
                   )
             )
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(session_agent_id)
        .fetch_optional(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok((completed, claimed))
    }

    pub async fn mark_completed_by_run_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        run_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'completed',
                 revision = revision + 1,
                 updated_at = datetime('now', 'subsec')
             WHERE run_id = ?1
               AND status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(run_id)
        .fetch_optional(&mut **transaction)
        .await
    }

    pub async fn mark_completed_by_run_cas_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        run_id: Uuid,
        expected_revision: i64,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'completed',
                 revision = revision + 1,
                 updated_at = datetime('now', 'subsec')
             WHERE run_id = ?1
               AND revision = ?2
               AND status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(run_id)
        .bind(expected_revision)
        .fetch_optional(&mut **transaction)
        .await
    }

    pub async fn claim_next_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        session_agent_id: Uuid,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'starting',
                 revision = revision + 1,
                 attempt_no = attempt_no + 1,
                 processing_started_at = datetime('now', 'subsec'),
                 updated_at = datetime('now', 'subsec')
             WHERE id = (
                 SELECT id FROM chat_message_queue
                 WHERE session_agent_id = ?1 AND status = 'queued'
                 ORDER BY created_at ASC, id ASC
                 LIMIT 1
             )
             AND NOT EXISTS (
                 SELECT 1 FROM chat_message_queue
                 WHERE session_agent_id = ?1
                   AND (
                       status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
                       OR (status = 'failed' AND failure_resolved_at IS NULL)
                   )
             )
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(session_agent_id)
        .fetch_optional(&mut **transaction)
        .await
    }

    /// Mark a failed run `failed` when work is waiting behind it, otherwise `skipped` so the
    /// member is not left permanently paused by a lone failure.
    pub async fn mark_failed_or_skipped_by_run_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        run_id: Uuid,
        session_agent_id: Uuid,
        failure_reason: Option<String>,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = CASE
                     WHEN EXISTS (
                         SELECT 1 FROM chat_message_queue queued
                         WHERE queued.session_agent_id = ?2 AND queued.status = 'queued'
                     ) THEN 'failed'
                     ELSE 'skipped'
                 END,
                 revision = revision + 1,
                 failure_reason = ?3,
                 updated_at = datetime('now', 'subsec')
             WHERE run_id = ?1
               AND status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(run_id)
        .bind(session_agent_id)
        .bind(failure_reason)
        .fetch_optional(&mut **transaction)
        .await
    }

    pub async fn mark_failed_or_skipped_by_run_cas_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        run_id: Uuid,
        session_agent_id: Uuid,
        expected_revision: i64,
        failure_reason: Option<String>,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = CASE
                     WHEN EXISTS (
                         SELECT 1 FROM chat_message_queue queued
                         WHERE queued.session_agent_id = ?2 AND queued.status = 'queued'
                     ) THEN 'failed'
                     ELSE 'skipped'
                 END,
                 revision = revision + 1,
                 failure_reason = ?4,
                 updated_at = datetime('now', 'subsec')
             WHERE run_id = ?1
               AND revision = ?3
               AND status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(run_id)
        .bind(session_agent_id)
        .bind(expected_revision)
        .bind(failure_reason)
        .fetch_optional(&mut **transaction)
        .await
    }

    /// Mark an in-flight entry `failed`. Remaining `queued` entries are left untouched so the
    /// member queue is blocked rather than drained.
    pub async fn mark_failed(
        pool: &SqlitePool,
        id: Uuid,
        failure_reason: Option<String>,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'failed',
                 revision = revision + 1,
                 failure_reason = ?2,
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1
               AND status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .bind(failure_reason)
        .fetch_optional(pool)
        .await
    }

    /// Skip an in-flight (`processing`/`running`) entry directly, transitioning it to `skipped`.
    ///
    /// Used when a run fails but there are no queued messages waiting behind it, so the queue
    /// stays clean for the next message instead of being blocked by a stale `failed` row.
    pub async fn skip_inflight(
        pool: &SqlitePool,
        id: Uuid,
        failure_reason: Option<String>,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_message_queue
             SET status = 'skipped',
                 revision = revision + 1,
                 failure_reason = ?2,
                 updated_at = datetime('now', 'subsec')
             WHERE id = ?1
               AND status IN ('starting', 'processing', 'running', 'waiting_approval', 'stopping')
             RETURNING {CHAT_MESSAGE_QUEUE_COLUMNS}"
        ))
        .bind(id)
        .bind(failure_reason)
        .fetch_optional(pool)
        .await
    }

    /// Continue execution after a failure without rewriting terminal history. The legacy method
    /// name is retained for API compatibility; it now resolves the failed blockers in place.
    pub async fn skip_failed_for_member(
        pool: &SqlitePool,
        session_agent_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE chat_message_queue
             SET failure_resolved_at = datetime('now', 'subsec'),
                 revision = revision + 1,
                 updated_at = datetime('now', 'subsec')
             WHERE session_agent_id = ?1
               AND status = 'failed'
               AND failure_resolved_at IS NULL",
        )
        .bind(session_agent_id)
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Delete a `queued` entry (user removed it before it started). Only `queued` rows can be
    /// deleted; in-flight or terminal rows are preserved. Returns the number of deleted rows.
    pub async fn delete_queued(pool: &SqlitePool, id: Uuid) -> Result<u64, sqlx::Error> {
        let result =
            sqlx::query("DELETE FROM chat_message_queue WHERE id = ?1 AND status = 'queued'")
                .bind(id)
                .execute(pool)
                .await?;
        Ok(result.rows_affected())
    }

    pub async fn delete_queued_cas(
        pool: &SqlitePool,
        id: Uuid,
        expected_revision: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM chat_message_queue
             WHERE id = ?1 AND revision = ?2 AND status = 'queued'",
        )
        .bind(id)
        .bind(expected_revision)
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Count queue rows that reference the same `chat_message_id`, excluding the given queue id.
    ///
    /// Used by the delete-queue flow to decide whether the underlying `chat_messages` row can be
    /// removed safely: when no other queue entry (any member, any status) references it, the source
    /// message was never visible to any agent run and should be cleaned up so it does not
    /// reappear on refresh.
    pub async fn count_other_references_for_chat_message(
        pool: &SqlitePool,
        chat_message_id: Uuid,
        exclude_queue_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM chat_message_queue
             WHERE chat_message_id = ?1 AND id <> ?2",
        )
        .bind(chat_message_id)
        .bind(exclude_queue_id)
        .fetch_one(pool)
        .await?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use uuid::Uuid;

    use super::{ChatMessageQueue, CreateChatMessageQueue, QueuedMessageStatus};

    #[test]
    fn normal_transition_graph_keeps_recovery_and_terminal_history_explicit() {
        for status in [
            QueuedMessageStatus::Starting,
            QueuedMessageStatus::Processing,
            QueuedMessageStatus::Running,
            QueuedMessageStatus::WaitingApproval,
            QueuedMessageStatus::Stopping,
        ] {
            assert!(!status.can_transition_to(QueuedMessageStatus::Queued));
            assert!(status.can_recover_to_queued());
        }
        assert!(QueuedMessageStatus::Processing.can_transition_to(QueuedMessageStatus::Running));
        assert!(
            QueuedMessageStatus::Running.can_transition_to(QueuedMessageStatus::WaitingApproval)
        );
        assert!(
            QueuedMessageStatus::WaitingApproval.can_transition_to(QueuedMessageStatus::Running)
        );
        for terminal in [
            QueuedMessageStatus::Failed,
            QueuedMessageStatus::Cancelled,
            QueuedMessageStatus::Skipped,
            QueuedMessageStatus::Completed,
        ] {
            assert!(!terminal.can_transition_to(QueuedMessageStatus::Queued));
            assert!(!terminal.can_transition_to(QueuedMessageStatus::Starting));
            assert!(!terminal.can_transition_to(QueuedMessageStatus::Skipped));
            assert!(!terminal.can_recover_to_queued());
        }
    }

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
        sqlx::query(
            "CREATE TRIGGER queue_revision_insert AFTER INSERT ON chat_message_queue BEGIN
                 INSERT INTO chat_session_runtime_revisions (session_id, revision)
                 VALUES (NEW.session_id, 1)
                 ON CONFLICT(session_id) DO UPDATE SET revision = revision + 1;
             END",
        )
        .execute(pool)
        .await
        .expect("create insert revision trigger");
        sqlx::query(
            "CREATE TRIGGER queue_revision_update AFTER UPDATE ON chat_message_queue BEGIN
                 INSERT INTO chat_session_runtime_revisions (session_id, revision)
                 VALUES (NEW.session_id, 1)
                 ON CONFLICT(session_id) DO UPDATE SET revision = revision + 1;
             END",
        )
        .execute(pool)
        .await
        .expect("create update revision trigger");
        sqlx::query(
            "CREATE TRIGGER queue_revision_delete AFTER DELETE ON chat_message_queue BEGIN
                 INSERT INTO chat_session_runtime_revisions (session_id, revision)
                 VALUES (OLD.session_id, 1)
                 ON CONFLICT(session_id) DO UPDATE SET revision = revision + 1;
             END",
        )
        .execute(pool)
        .await
        .expect("create delete revision trigger");
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
        pool
    }

    async fn setup_pool_with_foreign_keys() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create sqlite memory pool");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        sqlx::query("CREATE TABLE chat_sessions (id BLOB PRIMARY KEY)")
            .execute(&pool)
            .await
            .expect("create chat_sessions");
        sqlx::query("CREATE TABLE chat_agents (id BLOB PRIMARY KEY)")
            .execute(&pool)
            .await
            .expect("create chat_agents");
        sqlx::query(
            r#"
            CREATE TABLE chat_session_agents (
                id         BLOB PRIMARY KEY,
                session_id BLOB NOT NULL REFERENCES chat_sessions(id),
                agent_id   BLOB NOT NULL REFERENCES chat_agents(id)
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create chat_session_agents");
        sqlx::query(
            r#"
            CREATE TABLE chat_messages (
                id         BLOB PRIMARY KEY,
                session_id BLOB NOT NULL REFERENCES chat_sessions(id)
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create chat_messages");
        sqlx::query(
            r#"
            CREATE TABLE chat_runs (
                id                        BLOB PRIMARY KEY,
                session_id                BLOB NOT NULL,
                session_agent_id          BLOB NOT NULL,
                workspace_path            TEXT,
                run_index                 INTEGER NOT NULL,
                run_dir                   TEXT NOT NULL,
                input_path                TEXT,
                output_path               TEXT,
                raw_log_path              TEXT,
                meta_path                 TEXT,
                log_state                 TEXT NOT NULL DEFAULT 'live',
                artifact_state            TEXT NOT NULL DEFAULT 'full',
                log_truncated             INTEGER NOT NULL DEFAULT 0,
                log_capture_degraded      INTEGER NOT NULL DEFAULT 0,
                pruned_at                 TEXT,
                prune_reason              TEXT,
                retention_summary_json    TEXT,
                created_at                TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create chat_runs");
        sqlx::query(
            r#"
            CREATE TABLE chat_message_queue (
                id                    BLOB PRIMARY KEY,
                session_id            BLOB NOT NULL REFERENCES chat_sessions(id) ON DELETE CASCADE,
                session_agent_id      BLOB NOT NULL REFERENCES chat_session_agents(id) ON DELETE CASCADE,
                agent_id              BLOB NOT NULL REFERENCES chat_agents(id) ON DELETE CASCADE,
                chat_message_id       BLOB NOT NULL REFERENCES chat_messages(id) ON DELETE CASCADE,
                status                TEXT NOT NULL DEFAULT 'queued'
                                        CHECK (status IN (
                                            'queued','starting','processing','running',
                                            'waiting_approval','stopping','failed','cancelled',
                                            'skipped','completed'
                                        )),
                revision              INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
                attempt_no            INTEGER NOT NULL DEFAULT 0 CHECK (attempt_no >= 0),
                processing_started_at TEXT,
                run_id                BLOB REFERENCES chat_runs(id) ON DELETE SET NULL,
                failure_reason        TEXT,
                failure_resolved_at   TEXT,
                created_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
                updated_at            TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create chat_message_queue");
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
        pool
    }

    async fn seed_referenced_chat_rows(
        pool: &SqlitePool,
        data: &CreateChatMessageQueue,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT INTO chat_sessions (id) VALUES (?1)")
            .bind(data.session_id)
            .execute(pool)
            .await?;
        sqlx::query("INSERT INTO chat_agents (id) VALUES (?1)")
            .bind(data.agent_id)
            .execute(pool)
            .await?;
        sqlx::query(
            "INSERT INTO chat_session_agents (id, session_id, agent_id) VALUES (?1, ?2, ?3)",
        )
        .bind(data.session_agent_id)
        .bind(data.session_id)
        .bind(data.agent_id)
        .execute(pool)
        .await?;
        sqlx::query("INSERT INTO chat_messages (id, session_id) VALUES (?1, ?2)")
            .bind(data.chat_message_id)
            .bind(data.session_id)
            .execute(pool)
            .await?;
        Ok(())
    }

    async fn insert_chat_run(
        pool: &SqlitePool,
        run_id: Uuid,
        data: &CreateChatMessageQueue,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO chat_runs (id, session_id, session_agent_id, run_index, run_dir)
            VALUES (?1, ?2, ?3, 1, 'run-dir')
            "#,
        )
        .bind(run_id)
        .bind(data.session_id)
        .bind(data.session_agent_id)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn insert_legacy_delivery(
        pool: &SqlitePool,
        id: Uuid,
        data: &CreateChatMessageQueue,
        status: &str,
        run_id: Option<Uuid>,
        updated_at: &str,
    ) {
        sqlx::query(
            "INSERT INTO chat_message_queue (
                 id, session_id, session_agent_id, agent_id, chat_message_id,
                 status, run_id, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        )
        .bind(id)
        .bind(data.session_id)
        .bind(data.session_agent_id)
        .bind(data.agent_id)
        .bind(data.chat_message_id)
        .bind(status)
        .bind(run_id)
        .bind(updated_at)
        .execute(pool)
        .await
        .expect("insert legacy delivery");
    }

    /// Enqueue an entry. `seq` makes `created_at` strictly ordering across entries so the
    /// in-memory clock granularity never makes the test flaky.
    async fn enqueue(pool: &SqlitePool, session_agent_id: Uuid, seq: i64) -> ChatMessageQueue {
        let entry = ChatMessageQueue::create_queued(
            pool,
            &CreateChatMessageQueue {
                session_id: Uuid::new_v4(),
                session_agent_id,
                agent_id: Uuid::new_v4(),
                chat_message_id: Uuid::new_v4(),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("enqueue");
        sqlx::query("UPDATE chat_message_queue SET created_at = ?2 WHERE id = ?1")
            .bind(entry.id)
            .bind(format!("2026-06-17T00:00:0{seq}.000"))
            .execute(pool)
            .await
            .expect("set created_at");
        entry
    }

    #[tokio::test]
    async fn claim_next_takes_oldest_and_blocks_second_claim() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let first = enqueue(&pool, member, 1).await;
        let _second = enqueue(&pool, member, 2).await;

        let claimed = ChatMessageQueue::claim_next(&pool, member)
            .await
            .expect("claim")
            .expect("an entry to claim");
        assert_eq!(claimed.id, first.id);
        assert_eq!(claimed.status, QueuedMessageStatus::Starting);
        assert!(claimed.processing_started_at.is_some());

        // A second claim while one is in-flight returns nothing.
        let none = ChatMessageQueue::claim_next(&pool, member)
            .await
            .expect("claim");
        assert!(none.is_none());
    }

    #[tokio::test]
    async fn bind_run_then_complete_advances_to_next() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let first = enqueue(&pool, member, 1).await;
        let second = enqueue(&pool, member, 2).await;

        let claimed = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .unwrap();
        let run_id = Uuid::new_v4();
        let running = ChatMessageQueue::bind_run(&pool, claimed.id, run_id)
            .await
            .unwrap()
            .expect("bind run");
        assert_eq!(running.status, QueuedMessageStatus::Running);
        assert_eq!(running.run_id, Some(run_id));

        let completed = ChatMessageQueue::mark_completed(&pool, claimed.id)
            .await
            .unwrap()
            .expect("complete");
        assert_eq!(completed.status, QueuedMessageStatus::Completed);

        // Member is now idle, so the next entry can be claimed.
        let next = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim next");
        assert_eq!(next.id, second.id);
        assert_eq!(first.session_agent_id, member);
    }

    #[tokio::test]
    async fn complete_run_and_claim_next_is_atomic_for_member_queue() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let first = enqueue(&pool, member, 1).await;
        let second = enqueue(&pool, member, 2).await;

        let claimed = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, first.id);
        let run_id = Uuid::new_v4();
        ChatMessageQueue::bind_run(&pool, claimed.id, run_id)
            .await
            .unwrap()
            .expect("bind run");

        let (completed, next) =
            ChatMessageQueue::complete_run_and_claim_next(&pool, run_id, member)
                .await
                .expect("complete and claim");

        let completed = completed.expect("completed row");
        assert_eq!(completed.id, first.id);
        assert_eq!(completed.status, QueuedMessageStatus::Completed);
        let next = next.expect("next queued row");
        assert_eq!(next.id, second.id);
        assert_eq!(next.status, QueuedMessageStatus::Starting);
    }

    #[tokio::test]
    async fn failure_blocks_queue_until_resolved_without_rewriting_terminal_status() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let first = enqueue(&pool, member, 1).await;
        let _second = enqueue(&pool, member, 2).await;

        let claimed = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, first.id);

        let failed = ChatMessageQueue::mark_failed(&pool, claimed.id, Some("boom".into()))
            .await
            .unwrap()
            .expect("fail");
        assert_eq!(failed.status, QueuedMessageStatus::Failed);
        assert_eq!(failed.failure_reason.as_deref(), Some("boom"));

        // Remaining queued entry is untouched and the member is blocked.
        assert_eq!(
            ChatMessageQueue::count_queued_for_member(&pool, member)
                .await
                .unwrap(),
            1
        );
        assert!(
            ChatMessageQueue::has_blocking_failure(&pool, member)
                .await
                .unwrap()
        );
        // While blocked by a failed entry, claim_next must NOT advance the queue.
        assert!(
            ChatMessageQueue::claim_next(&pool, member)
                .await
                .unwrap()
                .is_none()
        );

        let skipped = ChatMessageQueue::skip_failed_for_member(&pool, member)
            .await
            .unwrap();
        assert_eq!(skipped, 1);
        let resolved_failure = ChatMessageQueue::find_by_id(&pool, failed.id)
            .await
            .unwrap()
            .expect("resolved failure remains persisted");
        assert_eq!(resolved_failure.status, QueuedMessageStatus::Failed);
        assert!(resolved_failure.failure_resolved_at.is_some());
        assert!(
            !ChatMessageQueue::has_blocking_failure(&pool, member)
                .await
                .unwrap()
        );

        let next = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim after resume");
        assert_eq!(next.status, QueuedMessageStatus::Starting);
    }

    #[tokio::test]
    async fn skip_inflight_transitions_processing_to_skipped() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let first = enqueue(&pool, member, 1).await;

        let claimed = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, first.id);

        let skipped = ChatMessageQueue::skip_inflight(&pool, claimed.id, Some("auto-skip".into()))
            .await
            .unwrap()
            .expect("skip inflight");
        assert_eq!(skipped.status, QueuedMessageStatus::Skipped);
        assert_eq!(skipped.failure_reason.as_deref(), Some("auto-skip"));

        // No blocking failure remains — the queue is clean for the next message.
        assert!(
            !ChatMessageQueue::has_blocking_failure(&pool, member)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn skip_inflight_leaves_queued_entries_claimable() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let first = enqueue(&pool, member, 1).await;
        let second = enqueue(&pool, member, 2).await;

        let claimed = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, first.id);

        // Auto-skipping the in-flight entry does not touch queued entries.
        let skipped = ChatMessageQueue::skip_inflight(&pool, claimed.id, Some("auto-skip".into()))
            .await
            .unwrap()
            .expect("skip inflight");
        assert_eq!(skipped.status, QueuedMessageStatus::Skipped);

        // The remaining queued entry is claimable immediately (no failed row blocking it).
        let next = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim remaining");
        assert_eq!(next.id, second.id);
    }

    #[tokio::test]
    async fn skip_inflight_only_affects_in_flight_rows() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let queued = enqueue(&pool, member, 1).await;

        // A queued (not in-flight) row is not affected by skip_inflight.
        let result = ChatMessageQueue::skip_inflight(&pool, queued.id, Some("nope".into()))
            .await
            .unwrap();
        assert!(result.is_none());
        assert_eq!(
            ChatMessageQueue::find_by_id(&pool, queued.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            QueuedMessageStatus::Queued
        );
    }

    #[tokio::test]
    async fn delete_only_removes_queued_entries() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let queued = enqueue(&pool, member, 1).await;
        let to_run = enqueue(&pool, member, 2).await;

        // Deleting a queued entry succeeds.
        assert_eq!(
            ChatMessageQueue::delete_queued(&pool, queued.id)
                .await
                .unwrap(),
            1
        );
        assert!(
            ChatMessageQueue::find_by_id(&pool, queued.id)
                .await
                .unwrap()
                .is_none()
        );

        // An in-flight entry cannot be deleted via delete_queued.
        let claimed = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, to_run.id);
        assert_eq!(
            ChatMessageQueue::delete_queued(&pool, to_run.id)
                .await
                .unwrap(),
            0
        );
        assert!(
            ChatMessageQueue::find_by_id(&pool, to_run.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn count_other_references_for_chat_message_reflects_remaining_rows() {
        let pool = setup_pool().await;
        // Two members share the same source chat_message (e.g. multi-agent mention).
        let shared_message_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let agent_a = Uuid::new_v4();
        let agent_b = Uuid::new_v4();
        let row_a = ChatMessageQueue::create_queued(
            &pool,
            &CreateChatMessageQueue {
                session_id,
                session_agent_id: agent_a,
                agent_id: Uuid::new_v4(),
                chat_message_id: shared_message_id,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("enqueue a");
        let row_b = ChatMessageQueue::create_queued(
            &pool,
            &CreateChatMessageQueue {
                session_id,
                session_agent_id: agent_b,
                agent_id: Uuid::new_v4(),
                chat_message_id: shared_message_id,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("enqueue b");

        // From row_a's perspective, row_b still references the source message.
        assert_eq!(
            ChatMessageQueue::count_other_references_for_chat_message(
                &pool,
                shared_message_id,
                row_a.id
            )
            .await
            .unwrap(),
            1
        );
        // Once row_b is gone, no other references remain.
        ChatMessageQueue::delete_queued(&pool, row_b.id)
            .await
            .unwrap();
        assert_eq!(
            ChatMessageQueue::count_other_references_for_chat_message(
                &pool,
                shared_message_id,
                row_a.id
            )
            .await
            .unwrap(),
            0
        );
        // An unrelated message has no references at all.
        assert_eq!(
            ChatMessageQueue::count_other_references_for_chat_message(
                &pool,
                Uuid::new_v4(),
                row_a.id
            )
            .await
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn start_or_create_running_inserts_then_advances_in_place() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let message_id = Uuid::new_v4();
        let data = CreateChatMessageQueue {
            session_id,
            session_agent_id: member,
            agent_id,
            chat_message_id: message_id,
        };

        // No row yet -> a fresh running row is inserted (direct mention while idle).
        let run_id = Uuid::new_v4();
        let created =
            ChatMessageQueue::start_or_create_running(&pool, &data, Uuid::new_v4(), run_id)
                .await
                .expect("create running");
        assert_eq!(created.status, QueuedMessageStatus::Running);
        assert_eq!(created.run_id, Some(run_id));
        let found = ChatMessageQueue::find_by_run_id(&pool, run_id)
            .await
            .unwrap()
            .expect("find by run id");
        assert_eq!(found.id, created.id);

        // A previously queued message is advanced in place (no duplicate row).
        let queued = enqueue(&pool, member, 5).await;
        // mark the active one complete so the unique in-flight index is free
        ChatMessageQueue::mark_completed(&pool, created.id)
            .await
            .unwrap();
        let run_id2 = Uuid::new_v4();
        let advanced = ChatMessageQueue::start_or_create_running(
            &pool,
            &CreateChatMessageQueue {
                session_id: queued.session_id,
                session_agent_id: member,
                agent_id: queued.agent_id,
                chat_message_id: queued.chat_message_id,
            },
            Uuid::new_v4(),
            run_id2,
        )
        .await
        .expect("advance running");
        assert_eq!(advanced.id, queued.id);
        assert_eq!(advanced.status, QueuedMessageStatus::Running);
        assert_eq!(
            ChatMessageQueue::list_for_member(&pool, member)
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn start_or_create_running_requires_existing_chat_run_fk() {
        let pool = setup_pool_with_foreign_keys().await;
        let data = CreateChatMessageQueue {
            session_id: Uuid::new_v4(),
            session_agent_id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            chat_message_id: Uuid::new_v4(),
        };
        seed_referenced_chat_rows(&pool, &data)
            .await
            .expect("seed parent rows");
        let run_id = Uuid::new_v4();

        let err = ChatMessageQueue::start_or_create_running(&pool, &data, Uuid::new_v4(), run_id)
            .await
            .expect_err("run_id FK should reject binding before chat_runs insert");
        assert!(matches!(err, sqlx::Error::Database(_)));

        insert_chat_run(&pool, run_id, &data)
            .await
            .expect("insert chat run");
        let running =
            ChatMessageQueue::start_or_create_running(&pool, &data, Uuid::new_v4(), run_id)
                .await
                .expect("bind after chat run exists");
        assert_eq!(running.status, QueuedMessageStatus::Running);
        assert_eq!(running.run_id, Some(run_id));
    }

    #[tokio::test]
    async fn requeue_stale_inflight_resets_in_flight_rows() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let _first = enqueue(&pool, member, 1).await;
        let claimed = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .unwrap();
        ChatMessageQueue::bind_run(&pool, claimed.id, Uuid::new_v4())
            .await
            .unwrap();

        let requeued = ChatMessageQueue::requeue_stale_inflight(&pool, member)
            .await
            .unwrap();
        assert_eq!(requeued, 1);

        // The reset row is claimable again and has no lingering run binding.
        let reclaimed = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .expect("reclaim after requeue");
        assert_eq!(reclaimed.id, claimed.id);
        assert!(reclaimed.run_id.is_none());
    }

    #[tokio::test]
    async fn recovery_cas_does_not_overwrite_a_newer_delivery_transition() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        enqueue(&pool, member, 1).await;
        let starting = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .unwrap();
        let running = ChatMessageQueue::bind_run(&pool, starting.id, Uuid::new_v4())
            .await
            .unwrap()
            .unwrap();

        let stale = ChatMessageQueue::recover_inflight_cas(
            &pool,
            running.id,
            starting.revision,
            QueuedMessageStatus::Starting,
        )
        .await
        .unwrap();
        assert!(stale.is_none());
        let recovered = ChatMessageQueue::recover_inflight_cas(
            &pool,
            running.id,
            running.revision,
            QueuedMessageStatus::Running,
        )
        .await
        .unwrap()
        .expect("recover exact orphan attempt");
        assert_eq!(recovered.status, QueuedMessageStatus::Queued);
        assert!(recovered.run_id.is_none());
    }

    #[tokio::test]
    async fn list_unbound_processing_excludes_rows_already_bound_to_runs() {
        let pool = setup_pool().await;
        let unbound_member = Uuid::new_v4();
        let bound_member = Uuid::new_v4();
        let unbound = enqueue(&pool, unbound_member, 1).await;
        let bound = enqueue(&pool, bound_member, 2).await;

        let claimed_unbound = ChatMessageQueue::claim_next(&pool, unbound_member)
            .await
            .unwrap()
            .unwrap();
        let claimed_bound = ChatMessageQueue::claim_next(&pool, bound_member)
            .await
            .unwrap()
            .unwrap();
        ChatMessageQueue::bind_run(&pool, claimed_bound.id, Uuid::new_v4())
            .await
            .unwrap()
            .expect("bind second row");

        let rows = ChatMessageQueue::list_unbound_processing(&pool)
            .await
            .expect("list unbound processing");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, claimed_unbound.id);
        assert_eq!(rows[0].id, unbound.id);
        assert_ne!(rows[0].id, bound.id);
    }

    #[tokio::test]
    async fn create_queued_is_idempotent_for_delivery_key_and_keeps_stable_id() {
        let pool = setup_pool().await;
        let data = CreateChatMessageQueue {
            session_id: Uuid::new_v4(),
            session_agent_id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            chat_message_id: Uuid::new_v4(),
        };
        let first_id = Uuid::new_v4();
        let first = ChatMessageQueue::create_queued(&pool, &data, first_id)
            .await
            .expect("create delivery");
        let replay = ChatMessageQueue::create_queued(&pool, &data, Uuid::new_v4())
            .await
            .expect("replay delivery create");

        assert_eq!(first.id, first_id);
        assert_eq!(replay.id, first.id);
        assert_eq!(replay.revision, 1);
        assert_eq!(
            ChatMessageQueue::list_for_member(&pool, data.session_agent_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            ChatMessageQueue::current_runtime_revision(&pool, data.session_id)
                .await
                .unwrap(),
            1
        );
        let conflicting_target = CreateChatMessageQueue {
            agent_id: Uuid::new_v4(),
            ..data
        };
        assert!(matches!(
            ChatMessageQueue::create_queued(&pool, &conflicting_target, Uuid::new_v4()).await,
            Err(sqlx::Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn revision_cas_rejects_stale_binding_and_advances_monotonically() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        let queued = enqueue(&pool, member, 1).await;
        assert_eq!(queued.revision, 1);
        let starting = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim delivery");
        assert_eq!(starting.revision, 2);
        assert_eq!(starting.attempt_no, 1);

        let mut stale_transaction = pool.begin().await.unwrap();
        assert!(
            ChatMessageQueue::bind_run_cas_in_transaction(
                &mut stale_transaction,
                starting.id,
                starting.revision - 1,
                Uuid::new_v4()
            )
            .await
            .unwrap()
            .is_none()
        );
        stale_transaction.commit().await.unwrap();
        let mut binding_transaction = pool.begin().await.unwrap();
        let running = ChatMessageQueue::bind_run_cas_in_transaction(
            &mut binding_transaction,
            starting.id,
            starting.revision,
            Uuid::new_v4(),
        )
        .await
        .unwrap()
        .expect("bind current revision");
        binding_transaction.commit().await.unwrap();
        assert_eq!(running.revision, 3);

        let waiting = ChatMessageQueue::transition_status_cas(
            &pool,
            running.id,
            running.revision,
            QueuedMessageStatus::Running,
            QueuedMessageStatus::WaitingApproval,
        )
        .await
        .unwrap()
        .expect("enter waiting approval");
        assert_eq!(waiting.revision, 4);
        assert_eq!(waiting.status, QueuedMessageStatus::WaitingApproval);
    }

    #[tokio::test]
    async fn rolled_back_transition_does_not_publish_revision() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        enqueue(&pool, member, 1).await;
        let starting = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .expect("claim delivery");
        let runtime_revision =
            ChatMessageQueue::current_runtime_revision(&pool, starting.session_id)
                .await
                .unwrap();

        let mut transaction = pool.begin().await.unwrap();
        let running = ChatMessageQueue::bind_run_cas_in_transaction(
            &mut transaction,
            starting.id,
            starting.revision,
            Uuid::new_v4(),
        )
        .await
        .unwrap();
        assert!(running.is_some());
        transaction.rollback().await.unwrap();

        let persisted = ChatMessageQueue::find_by_id(&pool, starting.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, QueuedMessageStatus::Starting);
        assert_eq!(persisted.revision, starting.revision);
        assert_eq!(
            ChatMessageQueue::current_runtime_revision(&pool, starting.session_id)
                .await
                .unwrap(),
            runtime_revision
        );
    }

    #[tokio::test]
    async fn terminal_delivery_cannot_be_reopened() {
        let pool = setup_pool().await;
        let member = Uuid::new_v4();
        enqueue(&pool, member, 1).await;
        let starting = ChatMessageQueue::claim_next(&pool, member)
            .await
            .unwrap()
            .unwrap();
        let completed = ChatMessageQueue::mark_completed(&pool, starting.id)
            .await
            .unwrap()
            .unwrap();
        assert!(completed.status.is_terminal());
        assert!(
            ChatMessageQueue::transition_status_cas(
                &pool,
                completed.id,
                completed.revision,
                QueuedMessageStatus::Completed,
                QueuedMessageStatus::Running,
            )
            .await
            .is_err()
        );
        assert!(
            ChatMessageQueue::mark_completed(&pool, completed.id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn delivery_migration_normalizes_legacy_identity_conflicts() {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create sqlite memory pool");
        sqlx::raw_sql(
            r#"
            CREATE TABLE chat_sessions (id BLOB PRIMARY KEY);
            CREATE TABLE chat_agents (id BLOB PRIMARY KEY);
            CREATE TABLE chat_session_agents (
                id BLOB PRIMARY KEY,
                session_id BLOB NOT NULL,
                agent_id BLOB NOT NULL
            );
            CREATE TABLE chat_messages (
                id BLOB PRIMARY KEY,
                session_id BLOB NOT NULL,
                sender_type TEXT NOT NULL,
                meta TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE chat_runs (id BLOB PRIMARY KEY);
            CREATE TABLE chat_message_queue (
                id BLOB PRIMARY KEY,
                session_id BLOB NOT NULL,
                session_agent_id BLOB NOT NULL,
                agent_id BLOB NOT NULL,
                chat_message_id BLOB NOT NULL,
                status TEXT NOT NULL,
                processing_started_at TEXT,
                run_id BLOB,
                failure_reason TEXT,
                failure_resolved_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .execute(&pool)
        .await
        .expect("create legacy schema");

        let session_id = Uuid::new_v4();
        let agent_id = Uuid::new_v4();
        let member_a = Uuid::new_v4();
        let member_b = Uuid::new_v4();
        sqlx::query("INSERT INTO chat_sessions (id) VALUES (?1)")
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO chat_agents (id) VALUES (?1)")
            .bind(agent_id)
            .execute(&pool)
            .await
            .unwrap();
        for member_id in [member_a, member_b] {
            sqlx::query(
                "INSERT INTO chat_session_agents (id, session_id, agent_id) VALUES (?1, ?2, ?3)",
            )
            .bind(member_id)
            .bind(session_id)
            .bind(agent_id)
            .execute(&pool)
            .await
            .unwrap();
        }

        let completed_message_id = Uuid::new_v4();
        let retry_message_id = Uuid::new_v4();
        let running_message_id = Uuid::new_v4();
        let conflicting_active_message_id = Uuid::new_v4();
        for message_id in [
            completed_message_id,
            retry_message_id,
            running_message_id,
            conflicting_active_message_id,
        ] {
            sqlx::query(
                "INSERT INTO chat_messages (
                     id, session_id, sender_type, meta, created_at
                 ) VALUES (?1, ?2, 'user', '{}', '2026-08-21T00:00:00.000')",
            )
            .bind(message_id)
            .bind(session_id)
            .execute(&pool)
            .await
            .unwrap();
        }
        let shared_run_id = Uuid::new_v4();
        let conflicting_run_id = Uuid::new_v4();
        for run_id in [shared_run_id, conflicting_run_id] {
            sqlx::query("INSERT INTO chat_runs (id) VALUES (?1)")
                .bind(run_id)
                .execute(&pool)
                .await
                .unwrap();
        }

        let completed_id = Uuid::new_v4();
        let completed_data = CreateChatMessageQueue {
            session_id,
            session_agent_id: member_a,
            agent_id,
            chat_message_id: completed_message_id,
        };
        insert_legacy_delivery(
            &pool,
            completed_id,
            &completed_data,
            "completed",
            Some(shared_run_id),
            "2026-08-21T00:00:01.000",
        )
        .await;
        insert_legacy_delivery(
            &pool,
            Uuid::new_v4(),
            &completed_data,
            "queued",
            None,
            "2026-08-21T00:00:09.000",
        )
        .await;

        let retry_data = CreateChatMessageQueue {
            session_id,
            session_agent_id: member_b,
            agent_id,
            chat_message_id: retry_message_id,
        };
        insert_legacy_delivery(
            &pool,
            Uuid::new_v4(),
            &retry_data,
            "failed",
            None,
            "2026-08-21T00:00:08.000",
        )
        .await;
        let queued_retry_id = Uuid::new_v4();
        insert_legacy_delivery(
            &pool,
            queued_retry_id,
            &retry_data,
            "queued",
            None,
            "2026-08-21T00:00:02.000",
        )
        .await;

        let running_id = Uuid::new_v4();
        insert_legacy_delivery(
            &pool,
            running_id,
            &CreateChatMessageQueue {
                session_id,
                session_agent_id: member_b,
                agent_id,
                chat_message_id: running_message_id,
            },
            "running",
            Some(shared_run_id),
            "2026-08-21T00:00:03.000",
        )
        .await;
        let conflicting_active_id = Uuid::new_v4();
        insert_legacy_delivery(
            &pool,
            conflicting_active_id,
            &CreateChatMessageQueue {
                session_id,
                session_agent_id: member_b,
                agent_id,
                chat_message_id: conflicting_active_message_id,
            },
            "processing",
            Some(conflicting_run_id),
            "2026-08-21T00:00:04.000",
        )
        .await;

        sqlx::raw_sql(include_str!(
            "../../migrations/20260821090000_promote_chat_queue_to_deliveries.sql"
        ))
        .execute(&pool)
        .await
        .expect("migrate legacy delivery conflicts");

        let completed: (Uuid, String, Option<Uuid>) = sqlx::query_as(
            "SELECT id, status, run_id FROM chat_message_queue
             WHERE chat_message_id = ?1 AND session_agent_id = ?2",
        )
        .bind(completed_message_id)
        .bind(member_a)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(completed, (completed_id, "completed".to_string(), None));

        let retry: (Uuid, String) = sqlx::query_as(
            "SELECT id, status FROM chat_message_queue
             WHERE chat_message_id = ?1 AND session_agent_id = ?2",
        )
        .bind(retry_message_id)
        .bind(member_b)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(retry, (queued_retry_id, "queued".to_string()));

        let shared_run_owner: (Uuid, String) =
            sqlx::query_as("SELECT id, status FROM chat_message_queue WHERE run_id = ?1")
                .bind(shared_run_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(shared_run_owner, (running_id, "running".to_string()));
        let conflicting_active: (String, Option<Uuid>, Option<String>) = sqlx::query_as(
            "SELECT status, run_id, failure_reason FROM chat_message_queue WHERE id = ?1",
        )
        .bind(conflicting_active_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(conflicting_active.0, "skipped");
        assert!(conflicting_active.1.is_none());
        assert!(conflicting_active.2.is_some());
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM chat_message_queue
             WHERE session_agent_id = ?1 AND status IN (
                 'starting', 'processing', 'running', 'waiting_approval', 'stopping'
             )",
        )
        .bind(member_b)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(active_count, 1);
    }

    #[tokio::test]
    async fn members_are_isolated() {
        let pool = setup_pool().await;
        let member_a = Uuid::new_v4();
        let member_b = Uuid::new_v4();
        enqueue(&pool, member_a, 1).await;
        enqueue(&pool, member_b, 1).await;

        // Claiming for A does not affect B's queue.
        ChatMessageQueue::claim_next(&pool, member_a)
            .await
            .unwrap()
            .unwrap();
        let b_entries = ChatMessageQueue::list_for_member(&pool, member_b)
            .await
            .unwrap();
        assert_eq!(b_entries.len(), 1);
        assert_eq!(b_entries[0].status, QueuedMessageStatus::Queued);
        let b_claim = ChatMessageQueue::claim_next(&pool, member_b)
            .await
            .unwrap()
            .expect("B can still claim independently");
        assert_eq!(b_claim.status, QueuedMessageStatus::Starting);
    }
}
