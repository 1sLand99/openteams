use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqliteConnection, SqlitePool};
use ts_rs::TS;
use uuid::Uuid;

use super::workflow_types::{ReviewVerdict, ReviewerType};

const REVIEW_SELECT: &str = r#"
    SELECT id, step_id, execution_id, reviewer_type, reviewer_id, verdict, feedback,
           review_round, created_at
    FROM chat_workflow_step_reviews
"#;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, TS)]
pub struct WorkflowStepReview {
    pub id: Uuid,
    pub step_id: Uuid,
    pub execution_id: Uuid,
    pub reviewer_type: ReviewerType,
    pub reviewer_id: Option<String>,
    pub verdict: ReviewVerdict,
    pub feedback: String,
    pub review_round: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct CreateWorkflowStepReview {
    pub step_id: Uuid,
    pub execution_id: Uuid,
    pub reviewer_type: ReviewerType,
    pub reviewer_id: Option<String>,
    pub verdict: ReviewVerdict,
    pub feedback: String,
    pub review_round: Option<i32>,
}

impl WorkflowStepReview {
    pub async fn find_by_id(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!("{REVIEW_SELECT}\nWHERE id = ?1"))
            .bind(id)
            .fetch_optional(pool)
            .await
    }

    pub async fn find_by_step(pool: &SqlitePool, step_id: Uuid) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "{REVIEW_SELECT}\nWHERE step_id = ?1\nORDER BY created_at ASC"
        ))
        .bind(step_id)
        .fetch_all(pool)
        .await
    }

    pub async fn find_by_execution(
        pool: &SqlitePool,
        execution_id: Uuid,
    ) -> Result<Vec<Self>, sqlx::Error> {
        sqlx::query_as::<_, Self>(&format!(
            "{REVIEW_SELECT}\nWHERE execution_id = ?1\nORDER BY created_at ASC"
        ))
        .bind(execution_id)
        .fetch_all(pool)
        .await
    }

    pub async fn count_reviews_in_current_cycle(
        pool: &SqlitePool,
        step_id: Uuid,
        reviewer_type: ReviewerType,
    ) -> Result<i32, sqlx::Error> {
        sqlx::query_scalar::<_, i32>(
            r#"
            SELECT MAX(
                (
                    SELECT COUNT(*)
                    FROM chat_workflow_step_reviews
                    WHERE step_id = ?1 AND reviewer_type = ?2
                ) - lead_review_attempt_offset,
                0
            )
            FROM chat_workflow_steps
            WHERE id = ?1
            "#,
        )
        .bind(step_id)
        .bind(reviewer_type)
        .fetch_one(pool)
        .await
    }

    pub async fn count_lead_reviews_in_current_cycle(
        pool: &SqlitePool,
        step_id: Uuid,
    ) -> Result<i32, sqlx::Error> {
        Self::count_reviews_in_current_cycle(pool, step_id, ReviewerType::Lead).await
    }

    pub async fn create(
        pool: &SqlitePool,
        data: &CreateWorkflowStepReview,
        id: Uuid,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as::<_, Self>(
            r#"
            INSERT INTO chat_workflow_step_reviews (
                id, step_id, execution_id, reviewer_type, reviewer_id, verdict, feedback,
                review_round
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, COALESCE(?8, 0))
            RETURNING id, step_id, execution_id, reviewer_type, reviewer_id, verdict,
                      feedback, review_round, created_at
            "#,
        )
        .bind(id)
        .bind(data.step_id)
        .bind(data.execution_id)
        .bind(&data.reviewer_type)
        .bind(&data.reviewer_id)
        .bind(&data.verdict)
        .bind(&data.feedback)
        .bind(data.review_round)
        .fetch_one(pool)
        .await
    }

    pub async fn create_in_transaction(
        connection: &mut SqliteConnection,
        data: &CreateWorkflowStepReview,
        id: Uuid,
    ) -> Result<Self, sqlx::Error> {
        sqlx::query_as::<_, Self>(
            r#"
            INSERT INTO chat_workflow_step_reviews (
                id, step_id, execution_id, reviewer_type, reviewer_id, verdict, feedback,
                review_round
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, COALESCE(?8, 0))
            RETURNING id, step_id, execution_id, reviewer_type, reviewer_id, verdict,
                      feedback, review_round, created_at
            "#,
        )
        .bind(id)
        .bind(data.step_id)
        .bind(data.execution_id)
        .bind(&data.reviewer_type)
        .bind(&data.reviewer_id)
        .bind(&data.verdict)
        .bind(&data.feedback)
        .bind(data.review_round)
        .fetch_one(connection)
        .await
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;

    use super::*;
    use crate::run_migrations;

    #[tokio::test]
    async fn migrations_allow_reviewer_type() {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create sqlite database");
        run_migrations(&pool).await.expect("run migrations");
        let mut connection = pool.acquire().await.expect("acquire connection");
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *connection)
            .await
            .expect("disable foreign keys for isolated constraint test");

        let reviewer_id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            INSERT INTO chat_workflow_step_reviews (
                id, step_id, execution_id, reviewer_type, reviewer_id, verdict, feedback,
                review_round
            ) VALUES (?1, ?2, ?3, 'reviewer', ?4, 'approved', 'verified', 1)
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .bind(&reviewer_id)
        .execute(&mut *connection)
        .await
        .expect("reviewer must satisfy the migrated CHECK constraint");

        let stored: String =
            sqlx::query_scalar("SELECT reviewer_type FROM chat_workflow_step_reviews LIMIT 1")
                .fetch_one(&mut *connection)
                .await
                .expect("read reviewer type");
        assert_eq!(stored, "reviewer");
    }
}
