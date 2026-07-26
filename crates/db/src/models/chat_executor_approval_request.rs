use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool, Type, types::Json};
use ts_rs::TS;
use uuid::Uuid;

const COLUMNS: &str = r#"
    id, session_id, session_agent_id, run_id, workflow_execution_id,
    workflow_step_id, runner, tool_call_id, tool_name, display_input,
    options, status, selected_option_id, processed_by, expires_at,
    resolved_at, created_at, updated_at
"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct ChatExecutorApprovalOption {
    pub option_id: String,
    pub kind: String,
    pub label: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Type, TS)]
#[sqlx(type_name = "chat_executor_approval_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum ChatExecutorApprovalStatus {
    Pending,
    Selected,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct ChatExecutorApprovalRequest {
    pub id: Uuid,
    pub session_id: Uuid,
    pub session_agent_id: Uuid,
    pub run_id: Uuid,
    pub workflow_execution_id: Option<Uuid>,
    pub workflow_step_id: Option<Uuid>,
    pub runner: String,
    pub tool_call_id: String,
    pub tool_name: String,
    #[ts(type = "JsonValue")]
    pub display_input: Json<serde_json::Value>,
    #[ts(type = "Array<ChatExecutorApprovalOption>")]
    pub options: Json<Vec<ChatExecutorApprovalOption>>,
    pub status: ChatExecutorApprovalStatus,
    pub selected_option_id: Option<String>,
    pub processed_by: Option<String>,
    #[ts(type = "Date")]
    pub expires_at: DateTime<Utc>,
    #[ts(type = "Date | null")]
    pub resolved_at: Option<DateTime<Utc>>,
    #[ts(type = "Date")]
    pub created_at: DateTime<Utc>,
    #[ts(type = "Date")]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct CreateChatExecutorApprovalRequest {
    pub session_id: Uuid,
    pub session_agent_id: Uuid,
    pub run_id: Uuid,
    pub workflow_execution_id: Option<Uuid>,
    pub workflow_step_id: Option<Uuid>,
    pub runner: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub display_input: serde_json::Value,
    pub options: Vec<ChatExecutorApprovalOption>,
    pub expires_at: DateTime<Utc>,
}

impl ChatExecutorApprovalRequest {
    pub async fn create_or_find(
        pool: &SqlitePool,
        input: &CreateChatExecutorApprovalRequest,
        id: Uuid,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query(
            r#"
            INSERT OR IGNORE INTO chat_executor_approval_requests (
                id, session_id, session_agent_id, run_id, workflow_execution_id,
                workflow_step_id, runner, tool_call_id, tool_name, display_input,
                options, expires_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
            "#,
        )
        .bind(id)
        .bind(input.session_id)
        .bind(input.session_agent_id)
        .bind(input.run_id)
        .bind(input.workflow_execution_id)
        .bind(input.workflow_step_id)
        .bind(&input.runner)
        .bind(&input.tool_call_id)
        .bind(&input.tool_name)
        .bind(Json(input.display_input.clone()))
        .bind(Json(input.options.clone()))
        .bind(input.expires_at)
        .execute(pool)
        .await?;

        sqlx::query_as::<_, Self>(&format!(
            "SELECT {COLUMNS} FROM chat_executor_approval_requests \
             WHERE run_id = ?1 AND tool_call_id = ?2"
        ))
        .bind(input.run_id)
        .bind(&input.tool_call_id)
        .fetch_one(pool)
        .await
    }

    pub async fn find_by_id(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {COLUMNS} FROM chat_executor_approval_requests WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(pool)
        .await
    }

    pub async fn list_pending(
        pool: &SqlitePool,
        session_id: Uuid,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "SELECT {COLUMNS} FROM chat_executor_approval_requests \
             WHERE session_id = ?1 AND status = 'pending' \
             ORDER BY created_at ASC, id ASC"
        ))
        .bind(session_id)
        .fetch_all(pool)
        .await
    }

    pub async fn select(
        pool: &SqlitePool,
        session_id: Uuid,
        id: Uuid,
        option_id: &str,
        processed_by: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            r#"
            UPDATE chat_executor_approval_requests
            SET status = 'selected',
                selected_option_id = ?3,
                processed_by = ?4,
                resolved_at = datetime('now', 'subsec'),
                updated_at = datetime('now', 'subsec')
            WHERE id = ?1 AND session_id = ?2 AND status = 'pending'
              AND expires_at > datetime('now', 'subsec')
              AND EXISTS (
                SELECT 1 FROM json_each(options)
                WHERE json_extract(value, '$.option_id') = ?3
              )
            RETURNING {COLUMNS}
            "#
        ))
        .bind(id)
        .bind(session_id)
        .bind(option_id)
        .bind(processed_by)
        .fetch_optional(pool)
        .await
    }

    pub async fn finish_pending(
        pool: &SqlitePool,
        id: Uuid,
        status: ChatExecutorApprovalStatus,
    ) -> Result<Option<Self>, sqlx::Error> {
        debug_assert!(matches!(
            status,
            ChatExecutorApprovalStatus::Cancelled | ChatExecutorApprovalStatus::Expired
        ));
        sqlx::query_as::<_, Self>(&format!(
            r#"
            UPDATE chat_executor_approval_requests
            SET status = ?2,
                resolved_at = datetime('now', 'subsec'),
                updated_at = datetime('now', 'subsec')
            WHERE id = ?1 AND status = 'pending'
            RETURNING {COLUMNS}
            "#
        ))
        .bind(id)
        .bind(status)
        .fetch_optional(pool)
        .await
    }

    pub async fn expire_orphaned(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "UPDATE chat_executor_approval_requests \
             SET status = 'expired', resolved_at = datetime('now', 'subsec'), \
                 updated_at = datetime('now', 'subsec') \
             WHERE status = 'pending' \
             RETURNING {COLUMNS}"
        ))
        .fetch_all(pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect");
        sqlx::query(
            r#"
            CREATE TABLE chat_executor_approval_requests (
                id BLOB PRIMARY KEY,
                session_id BLOB NOT NULL,
                session_agent_id BLOB NOT NULL,
                run_id BLOB NOT NULL,
                workflow_execution_id BLOB,
                workflow_step_id BLOB,
                runner TEXT NOT NULL,
                tool_call_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                display_input TEXT NOT NULL,
                options TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                selected_option_id TEXT,
                processed_by TEXT,
                expires_at TEXT NOT NULL,
                resolved_at TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
                UNIQUE(run_id, tool_call_id)
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create approval table");
        pool
    }

    #[tokio::test]
    async fn selection_is_cas_and_rejects_unknown_option_ids() {
        let pool = setup_pool().await;
        let session_id = Uuid::new_v4();
        let row = ChatExecutorApprovalRequest::create_or_find(
            &pool,
            &CreateChatExecutorApprovalRequest {
                session_id,
                session_agent_id: Uuid::new_v4(),
                run_id: Uuid::new_v4(),
                workflow_execution_id: None,
                workflow_step_id: None,
                runner: "gemini".to_string(),
                tool_call_id: "tool-1".to_string(),
                tool_name: "write_file".to_string(),
                display_input: serde_json::json!({"path": "README.md"}),
                options: vec![ChatExecutorApprovalOption {
                    option_id: "allow-once".to_string(),
                    kind: "allow_once".to_string(),
                    label: "Allow once".to_string(),
                }],
                expires_at: Utc::now() + TimeDelta::minutes(5),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create request");

        assert!(
            ChatExecutorApprovalRequest::select(
                &pool,
                session_id,
                row.id,
                "invented-option",
                "user",
            )
            .await
            .expect("invalid select")
            .is_none()
        );
        assert!(
            ChatExecutorApprovalRequest::select(&pool, session_id, row.id, "allow-once", "user",)
                .await
                .expect("valid select")
                .is_some()
        );
        assert!(
            ChatExecutorApprovalRequest::select(&pool, session_id, row.id, "allow-once", "user",)
                .await
                .expect("duplicate select")
                .is_none()
        );
    }
}
