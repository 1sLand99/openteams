use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type};
use ts_rs::TS;
use uuid::Uuid;

const CHAT_RUNTIME_OUTBOX_COLUMNS: &str = r#"
    sequence,
    session_id,
    revision,
    delivery_id,
    delivery_revision,
    event_type,
    session_agent_id,
    agent_id,
    created_at,
    published_at
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type, TS)]
#[sqlx(
    type_name = "chat_runtime_outbox_event_type",
    rename_all = "snake_case"
)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ChatRuntimeOutboxEventType {
    DeliveryCreated,
    DeliveryUpdated,
    DeliveryDeleted,
}

/// One durable, session-versioned delivery mutation waiting to be handed to
/// the live chat stream. Rows remain replayable after publication.
#[derive(Debug, Clone, FromRow)]
pub struct ChatRuntimeOutboxRecord {
    pub sequence: i64,
    pub session_id: Uuid,
    pub revision: i64,
    pub delivery_id: Uuid,
    pub delivery_revision: i64,
    pub event_type: ChatRuntimeOutboxEventType,
    pub session_agent_id: Option<Uuid>,
    pub agent_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
}

impl ChatRuntimeOutboxRecord {
    pub async fn list_unpublished(pool: &SqlitePool, limit: i64) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_RUNTIME_OUTBOX_COLUMNS}
             FROM chat_runtime_outbox
             WHERE published_at IS NULL
             ORDER BY session_id ASC, revision ASC
             LIMIT ?1"
        ))
        .bind(limit)
        .fetch_all(pool)
        .await
    }

    pub async fn list_for_session_after(
        pool: &SqlitePool,
        session_id: Uuid,
        after_revision: i64,
        limit: i64,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {CHAT_RUNTIME_OUTBOX_COLUMNS}
             FROM chat_runtime_outbox
             WHERE session_id = ?1 AND revision > ?2
             ORDER BY revision ASC
             LIMIT ?3"
        ))
        .bind(session_id)
        .bind(after_revision)
        .bind(limit)
        .fetch_all(pool)
        .await
    }

    pub async fn mark_published(pool: &SqlitePool, sequence: i64) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE chat_runtime_outbox
             SET published_at = datetime('now', 'subsec')
             WHERE sequence = ?1 AND published_at IS NULL",
        )
        .bind(sequence)
        .execute(pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn delete_published_before(
        pool: &SqlitePool,
        cutoff: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM chat_runtime_outbox
             WHERE published_at IS NOT NULL AND published_at < ?1",
        )
        .bind(cutoff)
        .execute(pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query(
            r#"CREATE TABLE chat_runtime_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id BLOB NOT NULL,
                revision INTEGER NOT NULL,
                delivery_id BLOB NOT NULL,
                delivery_revision INTEGER NOT NULL,
                event_type TEXT NOT NULL,
                session_agent_id BLOB,
                agent_id BLOB,
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
                published_at TEXT
            )"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    async fn insert_event(pool: &SqlitePool, session_id: Uuid, revision: i64) {
        sqlx::query(
            "INSERT INTO chat_runtime_outbox (
                 session_id, revision, delivery_id, delivery_revision, event_type,
                 session_agent_id, agent_id
             ) VALUES (?1, ?2, ?3, 1, 'delivery_updated', ?4, ?5)",
        )
        .bind(session_id)
        .bind(revision)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn replay_is_session_scoped_and_published_rows_remain_available() {
        let pool = test_pool().await;
        let session_id = Uuid::new_v4();
        insert_event(&pool, session_id, 1).await;
        insert_event(&pool, session_id, 2).await;
        insert_event(&pool, Uuid::new_v4(), 1).await;

        let unpublished = ChatRuntimeOutboxRecord::list_unpublished(&pool, 10)
            .await
            .unwrap();
        let first = unpublished
            .iter()
            .find(|event| event.session_id == session_id && event.revision == 1)
            .unwrap();
        assert!(
            ChatRuntimeOutboxRecord::mark_published(&pool, first.sequence)
                .await
                .unwrap()
        );

        let replay = ChatRuntimeOutboxRecord::list_for_session_after(&pool, session_id, 0, 10)
            .await
            .unwrap();
        assert_eq!(
            replay
                .iter()
                .map(|event| event.revision)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(replay[0].published_at.is_some());
    }
}
