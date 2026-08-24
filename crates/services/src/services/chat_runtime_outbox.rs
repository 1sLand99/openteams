use db::models::{
    chat_message_queue::ChatMessageQueue,
    chat_runtime_outbox::{ChatRuntimeOutboxEventType, ChatRuntimeOutboxRecord},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use ts_rs::TS;
use uuid::Uuid;

use crate::services::queued_message::{MemberQueueSnapshot, QueuedMessageService};

pub const CHAT_RUNTIME_REPLAY_LIMIT: i64 = 256;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ChatRuntimeDeltaPayload {
    pub delivery_id: Uuid,
    pub delivery_revision: i64,
    /// Authoritative projection for the affected member at publication time.
    /// Legacy outbox rows that predate persisted member identity leave this
    /// empty, which explicitly tells the client to request a full snapshot.
    pub queue: Option<MemberQueueSnapshot>,
}

/// Versioned runtime envelope shared by live WebSocket delivery and REST
/// replay. The database outbox row, not the WebSocket, owns its revision.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ChatRuntimeDelta {
    pub session_id: Uuid,
    pub revision: i64,
    pub event_type: ChatRuntimeOutboxEventType,
    pub payload: ChatRuntimeDeltaPayload,
}

#[derive(Debug, Clone)]
pub struct ChatRuntimeReplayPage {
    pub current_revision: i64,
    pub events: Vec<ChatRuntimeDelta>,
    pub requires_snapshot: bool,
}

#[derive(Clone, Default)]
pub struct ChatRuntimeOutboxService;

impl ChatRuntimeOutboxService {
    pub fn new() -> Self {
        Self
    }

    pub async fn unpublished(
        &self,
        pool: &SqlitePool,
        limit: i64,
    ) -> Result<Vec<ChatRuntimeOutboxRecord>, sqlx::Error> {
        ChatRuntimeOutboxRecord::list_unpublished(pool, limit).await
    }

    pub async fn mark_published(
        &self,
        pool: &SqlitePool,
        sequence: i64,
    ) -> Result<bool, sqlx::Error> {
        ChatRuntimeOutboxRecord::mark_published(pool, sequence).await
    }

    pub async fn delete_published_before(
        &self,
        pool: &SqlitePool,
        cutoff: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, sqlx::Error> {
        ChatRuntimeOutboxRecord::delete_published_before(pool, cutoff).await
    }

    pub async fn delta_for_record(
        &self,
        pool: &SqlitePool,
        record: &ChatRuntimeOutboxRecord,
    ) -> Result<ChatRuntimeDelta, sqlx::Error> {
        let mut member_identity = record.session_agent_id.zip(record.agent_id);
        if member_identity.is_none()
            && let Some(delivery) = QueuedMessageService::new()
                .find_by_id(pool, record.delivery_id)
                .await?
        {
            member_identity = Some((delivery.session_agent_id, delivery.agent_id));
        }

        let queue = match member_identity {
            Some((session_agent_id, agent_id)) => {
                let mut queue = QueuedMessageService::new()
                    .snapshot_for_member(pool, record.session_id, session_agent_id, agent_id)
                    .await?;
                // The envelope is the version boundary. Keeping the nested
                // compatibility snapshot on the same value prevents clients
                // from observing two competing revision numbers.
                queue.revision = record.revision;
                Some(queue)
            }
            None => None,
        };

        Ok(ChatRuntimeDelta {
            session_id: record.session_id,
            revision: record.revision,
            event_type: record.event_type,
            payload: ChatRuntimeDeltaPayload {
                delivery_id: record.delivery_id,
                delivery_revision: record.delivery_revision,
                queue,
            },
        })
    }

    pub async fn replay_after(
        &self,
        pool: &SqlitePool,
        session_id: Uuid,
        after_revision: i64,
        limit: i64,
    ) -> Result<ChatRuntimeReplayPage, sqlx::Error> {
        let current_revision = ChatMessageQueue::current_runtime_revision(pool, session_id).await?;
        if after_revision == current_revision {
            return Ok(ChatRuntimeReplayPage {
                current_revision,
                events: Vec::new(),
                requires_snapshot: false,
            });
        }
        if after_revision < 0 || after_revision > current_revision {
            return Ok(ChatRuntimeReplayPage {
                current_revision,
                events: Vec::new(),
                requires_snapshot: true,
            });
        }

        let records = ChatRuntimeOutboxRecord::list_for_session_after(
            pool,
            session_id,
            after_revision,
            limit.clamp(1, CHAT_RUNTIME_REPLAY_LIMIT),
        )
        .await?;
        if current_revision > after_revision
            && records
                .first()
                .is_none_or(|record| record.revision != after_revision + 1)
        {
            return Ok(ChatRuntimeReplayPage {
                current_revision,
                events: Vec::new(),
                requires_snapshot: true,
            });
        }

        let mut expected_revision = after_revision + 1;
        let mut events = Vec::with_capacity(records.len());
        for record in records {
            if record.revision != expected_revision {
                return Ok(ChatRuntimeReplayPage {
                    current_revision,
                    events: Vec::new(),
                    requires_snapshot: true,
                });
            }
            let delta = self.delta_for_record(pool, &record).await?;
            if delta.payload.queue.is_none() {
                return Ok(ChatRuntimeReplayPage {
                    current_revision,
                    events: Vec::new(),
                    requires_snapshot: true,
                });
            }
            events.push(delta);
            expected_revision += 1;
        }

        Ok(ChatRuntimeReplayPage {
            current_revision,
            events,
            requires_snapshot: false,
        })
    }
}
