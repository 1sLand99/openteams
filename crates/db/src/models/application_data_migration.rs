use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction, Type};

#[derive(Debug, Clone, Copy, Type, Serialize, Deserialize, PartialEq, Eq)]
#[sqlx(type_name = "TEXT", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ApplicationDataMigrationStatus {
    Pending,
    Completed,
    Failed,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApplicationDataMigration {
    pub name: String,
    pub status: ApplicationDataMigrationStatus,
    pub error_summary: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ApplicationDataMigration {
    pub async fn find_by_name(pool: &SqlitePool, name: &str) -> Result<Option<Self>, sqlx::Error> {
        sqlx::query_as!(
            ApplicationDataMigration,
            r#"SELECT name,
                      status AS "status!: ApplicationDataMigrationStatus",
                      error_summary,
                      created_at AS "created_at!: DateTime<Utc>",
                      updated_at AS "updated_at!: DateTime<Utc>"
               FROM application_data_migrations
               WHERE name = $1"#,
            name
        )
        .fetch_optional(pool)
        .await
    }

    /// Starts or retries a migration without ever reopening a completed marker.
    /// Returns `false` when another completed attempt already owns the marker.
    pub async fn begin_attempt(pool: &SqlitePool, name: &str) -> Result<bool, sqlx::Error> {
        let row = sqlx::query!(
            r#"INSERT INTO application_data_migrations (name, status, error_summary)
               VALUES ($1, 'pending', NULL)
               ON CONFLICT(name) DO UPDATE
               SET status = 'pending',
                   error_summary = NULL,
                   updated_at = datetime('now', 'subsec')
               WHERE application_data_migrations.status <> 'completed'
               RETURNING name"#,
            name
        )
        .fetch_optional(pool)
        .await?;
        Ok(row.is_some())
    }

    pub async fn mark_failed(
        pool: &SqlitePool,
        name: &str,
        error_summary: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"INSERT INTO application_data_migrations (name, status, error_summary)
               VALUES ($1, 'failed', $2)
               ON CONFLICT(name) DO UPDATE
               SET status = 'failed',
                   error_summary = excluded.error_summary,
                   updated_at = datetime('now', 'subsec')
               WHERE application_data_migrations.status <> 'completed'"#,
            name,
            error_summary
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn mark_completed_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        name: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"INSERT INTO application_data_migrations (name, status, error_summary)
               VALUES ($1, 'completed', NULL)
               ON CONFLICT(name) DO UPDATE
               SET status = 'completed',
                   error_summary = NULL,
                   updated_at = datetime('now', 'subsec')"#,
            name
        )
        .execute(&mut **transaction)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    #[tokio::test]
    async fn marker_transitions_retry_failures_but_never_reopens_completed() {
        let pool = setup_pool().await;
        let name = "member_scoped_mcp_v1";

        assert!(
            ApplicationDataMigration::begin_attempt(&pool, name)
                .await
                .expect("begin first attempt")
        );
        let pending = ApplicationDataMigration::find_by_name(&pool, name)
            .await
            .expect("find pending marker")
            .expect("pending marker exists");
        assert_eq!(pending.status, ApplicationDataMigrationStatus::Pending);
        assert_eq!(pending.error_summary, None);

        ApplicationDataMigration::mark_failed(&pool, name, "runner CODEX: config.toml: byte 12")
            .await
            .expect("mark failed");
        let failed = ApplicationDataMigration::find_by_name(&pool, name)
            .await
            .expect("find failed marker")
            .expect("failed marker exists");
        assert_eq!(failed.status, ApplicationDataMigrationStatus::Failed);
        assert!(failed.error_summary.is_some());

        assert!(
            ApplicationDataMigration::begin_attempt(&pool, name)
                .await
                .expect("retry failed marker")
        );
        let mut transaction = pool.begin().await.expect("begin transaction");
        ApplicationDataMigration::mark_completed_in_transaction(&mut transaction, name)
            .await
            .expect("mark completed");
        transaction.commit().await.expect("commit completed marker");

        assert!(
            !ApplicationDataMigration::begin_attempt(&pool, name)
                .await
                .expect("completed marker stays closed")
        );
        ApplicationDataMigration::mark_failed(&pool, name, "must not replace completed")
            .await
            .expect("failed update is ignored");
        let completed = ApplicationDataMigration::find_by_name(&pool, name)
            .await
            .expect("find completed marker")
            .expect("completed marker exists");
        assert_eq!(completed.status, ApplicationDataMigrationStatus::Completed);
        assert_eq!(completed.error_summary, None);
    }
}
