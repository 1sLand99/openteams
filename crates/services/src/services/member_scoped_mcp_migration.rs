use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use async_trait::async_trait;
use db::models::{
    application_data_migration::ApplicationDataMigration,
    member_execution_config::MemberExecutionConfig,
};
use executors::{
    executors::{BaseCodingAgent, ExecutorError, StandardCodingAgentExecutor},
    mcp_config::{McpConfig, MemberMcpConfig, read_canonical_mcp_config_if_exists},
    profile::{ExecutorConfigs, ExecutorProfileId},
};
use serde_json::Value;
use sqlx::{FromRow, SqlitePool, types::Json};
use thiserror::Error;
use uuid::Uuid;

use crate::services::member_execution::parse_runner_type;

pub const MEMBER_SCOPED_MCP_MIGRATION_NAME: &str = "member_scoped_mcp_v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemberScopedMcpMigrationReport {
    pub already_completed: bool,
    pub migrated_members: usize,
    pub runner_reads: usize,
}

#[derive(Debug, Error)]
pub enum MemberScopedMcpMigrationError {
    #[error("member-scoped MCP migration database operation failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("{summary}")]
    LegacyConfig { summary: String },
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error(
    "member MCP configuration is not initialized; restart OpenTeams after repairing the legacy MCP configuration"
)]
pub struct MemberScopedMcpMigrationPending;

#[derive(Debug, FromRow)]
struct LegacyMember {
    id: Uuid,
    execution_config: Json<MemberExecutionConfig>,
    agent_runner_type: Option<String>,
}

#[derive(Debug, Clone)]
struct LegacyMcpReadError {
    summary: String,
}

#[async_trait]
trait LegacyMcpConfigSource: Send + Sync {
    async fn read_for_runner(
        &self,
        runner: BaseCodingAgent,
    ) -> Result<MemberMcpConfig, LegacyMcpReadError>;
}

struct ExecutorProfileLegacyMcpConfigSource;

#[async_trait]
impl LegacyMcpConfigSource for ExecutorProfileLegacyMcpConfigSource {
    async fn read_for_runner(
        &self,
        runner: BaseCodingAgent,
    ) -> Result<MemberMcpConfig, LegacyMcpReadError> {
        let profiles = ExecutorConfigs::get_cached();
        let profile_id = ExecutorProfileId::new(runner.clone());
        let Some(executor) = profiles.get_coding_agent(&profile_id) else {
            return Err(legacy_read_error(
                &runner,
                Path::new("<unresolved>"),
                "executor profile",
            ));
        };
        let Some(path) = executor.default_mcp_config_path() else {
            return Ok(MemberMcpConfig::default());
        };
        read_legacy_mcp_config(&runner, &path, &executor.get_mcp_config()).await
    }
}

pub fn ensure_member_scoped_mcp_initialized(
    execution_config: &MemberExecutionConfig,
) -> Result<(), MemberScopedMcpMigrationPending> {
    execution_config
        .mcp
        .as_ref()
        .map(|_| ())
        .ok_or(MemberScopedMcpMigrationPending)
}

pub async fn run_member_scoped_mcp_migration(
    pool: &SqlitePool,
) -> Result<MemberScopedMcpMigrationReport, MemberScopedMcpMigrationError> {
    run_member_scoped_mcp_migration_with_source(pool, &ExecutorProfileLegacyMcpConfigSource).await
}

async fn run_member_scoped_mcp_migration_with_source(
    pool: &SqlitePool,
    source: &impl LegacyMcpConfigSource,
) -> Result<MemberScopedMcpMigrationReport, MemberScopedMcpMigrationError> {
    if !ApplicationDataMigration::begin_attempt(pool, MEMBER_SCOPED_MCP_MIGRATION_NAME).await? {
        return Ok(MemberScopedMcpMigrationReport {
            already_completed: true,
            migrated_members: 0,
            runner_reads: 0,
        });
    }

    match migrate_pending_members(pool, source).await {
        Ok(report) => Ok(report),
        Err(error) => {
            let marker_summary = match &error {
                MemberScopedMcpMigrationError::LegacyConfig { summary } => summary.clone(),
                MemberScopedMcpMigrationError::Database(_) => {
                    "member-scoped MCP migration database transaction failed".to_string()
                }
            };
            ApplicationDataMigration::mark_failed(
                pool,
                MEMBER_SCOPED_MCP_MIGRATION_NAME,
                &marker_summary,
            )
            .await?;
            Err(error)
        }
    }
}

async fn migrate_pending_members(
    pool: &SqlitePool,
    source: &impl LegacyMcpConfigSource,
) -> Result<MemberScopedMcpMigrationReport, MemberScopedMcpMigrationError> {
    let mut legacy_members = sqlx::query_as::<_, LegacyMember>(
        r#"SELECT pm.id,
                  COALESCE(pm.execution_config, '{}') AS execution_config,
                  ca.runner_type AS agent_runner_type
           FROM project_members pm
           LEFT JOIN chat_agents ca ON ca.id = pm.agent_id
           ORDER BY pm.id"#,
    )
    .fetch_all(pool)
    .await?;
    legacy_members.retain(|member| member.execution_config.0.mcp.is_none());

    let mut members_by_runner: HashMap<BaseCodingAgent, Vec<Uuid>> = HashMap::new();
    let mut configs_by_member = HashMap::new();
    for member in legacy_members {
        let runner = match member.execution_config.0.runner_type {
            Some(runner) => Some(runner),
            None => match member.agent_runner_type.as_deref() {
                Some(raw) => Some(parse_runner_type(raw).map_err(|_| {
                    MemberScopedMcpMigrationError::LegacyConfig {
                        summary: format!(
                            "runner {}; config path <unresolved>; structure runner_type",
                            sanitized_runner_label(raw)
                        ),
                    }
                })?),
                None => None,
            },
        };
        if let Some(runner) = runner {
            members_by_runner.entry(runner).or_default().push(member.id);
        } else {
            configs_by_member.insert(member.id, MemberMcpConfig::default());
        }
    }

    let mut runner_reads = 0;
    for (runner, member_ids) in members_by_runner {
        runner_reads += 1;
        let config = source.read_for_runner(runner).await.map_err(|error| {
            MemberScopedMcpMigrationError::LegacyConfig {
                summary: error.summary,
            }
        })?;
        for member_id in member_ids {
            configs_by_member.insert(member_id, config.clone());
        }
    }

    let mut member_configs = configs_by_member.into_iter().collect::<Vec<_>>();
    member_configs.sort_by_key(|(member_id, _)| *member_id);
    let mut transaction = pool.begin().await?;
    let mut migrated_members = 0;
    for (member_id, config) in member_configs {
        let serialized = serde_json::to_string(&config).map_err(|_| {
            MemberScopedMcpMigrationError::LegacyConfig {
                summary: "runner <canonical>; config path <database>; structure serialization"
                    .to_string(),
            }
        })?;
        let result = sqlx::query(
            r#"UPDATE project_members
               SET execution_config = json_set(
                       COALESCE(execution_config, '{}'),
                       '$.mcp',
                       json(?2)
                   ),
                   updated_at = datetime('now', 'subsec')
               WHERE id = ?1
                 AND (
                     json_type(COALESCE(execution_config, '{}'), '$.mcp') IS NULL
                     OR json_type(COALESCE(execution_config, '{}'), '$.mcp') = 'null'
                 )"#,
        )
        .bind(member_id)
        .bind(serialized)
        .execute(&mut *transaction)
        .await?;
        migrated_members += result.rows_affected() as usize;
    }
    ApplicationDataMigration::mark_completed_in_transaction(
        &mut transaction,
        MEMBER_SCOPED_MCP_MIGRATION_NAME,
    )
    .await?;
    transaction.commit().await?;

    Ok(MemberScopedMcpMigrationReport {
        already_completed: false,
        migrated_members,
        runner_reads,
    })
}

async fn read_legacy_mcp_config(
    runner: &BaseCodingAgent,
    path: &Path,
    mcp_config: &McpConfig,
) -> Result<MemberMcpConfig, LegacyMcpReadError> {
    let canonical = read_canonical_mcp_config_if_exists(path, mcp_config)
        .await
        .map_err(|error| summarize_executor_error(runner, path, &error))?
        .unwrap_or_else(|| serde_json::json!({"mcpServers": {}}));
    canonicalize_legacy_member_mcp(runner, path, canonical)
}

fn canonicalize_legacy_member_mcp(
    runner: &BaseCodingAgent,
    path: &Path,
    canonical: Value,
) -> Result<MemberMcpConfig, LegacyMcpReadError> {
    let Some(servers) = canonical.get("mcpServers").and_then(Value::as_object) else {
        return Err(legacy_read_error(
            runner,
            path,
            "mcpServers must be an object",
        ));
    };
    let mut normalized = BTreeMap::new();
    for (server_name, definition) in servers {
        let mut definition = definition.clone();
        if let Some(server) = definition.as_object_mut() {
            if let Some(command) = server.get("command").and_then(Value::as_array).cloned() {
                server.remove("command");
                let mut command = command.into_iter();
                if let Some(program) = command.next() {
                    server.insert("command".to_string(), program);
                    let args = command.collect::<Vec<_>>();
                    if !args.is_empty() {
                        server.insert("args".to_string(), Value::Array(args));
                    }
                } else {
                    server.insert("command".to_string(), Value::Array(Vec::new()));
                }
            }
            if !server.contains_key("env")
                && let Some(environment) = server.remove("environment")
            {
                server.insert("env".to_string(), environment);
            }
        }
        normalized.insert(server_name.clone(), definition);
    }
    let config = MemberMcpConfig {
        mcp_servers: normalized,
    };
    config
        .validate("legacy migration")
        .map_err(|error| legacy_read_error(runner, path, &format!("field {}", error.field_path)))?;
    Ok(config)
}

fn summarize_executor_error(
    runner: &BaseCodingAgent,
    path: &Path,
    error: &ExecutorError,
) -> LegacyMcpReadError {
    let structure = match error {
        ExecutorError::Json(error) => {
            format!("JSON line {} column {}", error.line(), error.column())
        }
        ExecutorError::TomlDeserialize(error) => error
            .span()
            .map(|span| format!("TOML byte {}", span.start))
            .unwrap_or_else(|| "TOML document".to_string()),
        ExecutorError::Yaml(error) => error
            .location()
            .map(|location| format!("YAML line {} column {}", location.line(), location.column()))
            .unwrap_or_else(|| "YAML document".to_string()),
        ExecutorError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            "mcp server definitions".to_string()
        }
        ExecutorError::Io(error) => format!("filesystem {:?}", error.kind()),
        _ => "configuration document".to_string(),
    };
    legacy_read_error(runner, path, &structure)
}

fn legacy_read_error(runner: &BaseCodingAgent, path: &Path, structure: &str) -> LegacyMcpReadError {
    LegacyMcpReadError {
        summary: format!(
            "runner {runner}; config path {}; structure {structure}",
            path.to_string_lossy()
        ),
    }
}

fn sanitized_runner_label(raw: &str) -> String {
    let normalized = raw
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
        .take(64)
        .collect::<String>();
    if normalized.is_empty() {
        "<invalid>".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Mutex};

    use db::models::application_data_migration::{
        ApplicationDataMigration, ApplicationDataMigrationStatus,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    struct CountingPathSource {
        configs: Mutex<HashMap<BaseCodingAgent, (PathBuf, McpConfig)>>,
        calls: Mutex<Vec<BaseCodingAgent>>,
    }

    #[async_trait]
    impl LegacyMcpConfigSource for CountingPathSource {
        async fn read_for_runner(
            &self,
            runner: BaseCodingAgent,
        ) -> Result<MemberMcpConfig, LegacyMcpReadError> {
            self.calls.lock().expect("calls lock").push(runner.clone());
            let (path, config) = self
                .configs
                .lock()
                .expect("configs lock")
                .get(&runner)
                .cloned()
                .expect("runner fixture");
            read_legacy_mcp_config(&runner, &path, &config).await
        }
    }

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect database");
        sqlx::migrate!("../db/migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        pool
    }

    async fn insert_legacy_member(
        pool: &SqlitePool,
        agent_id: Uuid,
        runner: &str,
        execution_config: MemberExecutionConfig,
    ) -> Uuid {
        sqlx::query(
            "INSERT OR IGNORE INTO chat_agents (id, name, runner_type) VALUES (?1, ?2, ?3)",
        )
        .bind(agent_id)
        .bind(format!("agent-{agent_id}"))
        .bind(runner)
        .execute(pool)
        .await
        .expect("insert agent");
        let project_id = Uuid::new_v4();
        sqlx::query("INSERT INTO projects (id, name) VALUES (?1, ?2)")
            .bind(project_id)
            .bind(format!("project-{project_id}"))
            .execute(pool)
            .await
            .expect("insert project");
        let member_id = Uuid::new_v4();
        sqlx::query(
            r#"INSERT INTO project_members (
                   id, project_id, member_type, agent_id, allowed_skill_ids,
                   execution_config, is_default
               ) VALUES (?1, ?2, 'agent', ?3, '[]', ?4, 0)"#,
        )
        .bind(member_id)
        .bind(project_id)
        .bind(agent_id)
        .bind(Json(execution_config))
        .execute(pool)
        .await
        .expect("insert project member");
        member_id
    }

    async fn member_config(pool: &SqlitePool, member_id: Uuid) -> MemberExecutionConfig {
        sqlx::query_scalar::<_, Json<MemberExecutionConfig>>(
            "SELECT execution_config FROM project_members WHERE id = ?1",
        )
        .bind(member_id)
        .fetch_one(pool)
        .await
        .expect("read member config")
        .0
    }

    fn canonical_acp() -> McpConfig {
        McpConfig::canonical_acp()
    }

    #[tokio::test]
    async fn canonicalizes_json_jsonc_toml_and_yaml_vendor_configs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let fake_secret = "fake-migration-secret";
        let fixtures = [
            (
                BaseCodingAgent::ClaudeCode,
                temp.path().join("claude.json"),
                canonical_acp(),
                format!(
                    r#"{{"mcpServers":{{"server":{{"command":"tool","env":{{"TOKEN":"{fake_secret}"}}}}}}}}"#
                ),
            ),
            (
                BaseCodingAgent::Opencode,
                temp.path().join("opencode.jsonc"),
                McpConfig::new(
                    vec!["mcp".to_string()],
                    serde_json::json!({"mcp": {}}),
                    serde_json::json!({}),
                    false,
                ),
                format!(
                    "{{ // comment\n \"mcp\": {{\"server\": {{\"type\":\"local\",\"command\":[\"tool\",\"serve\"],\"environment\":{{\"TOKEN\":\"{fake_secret}\"}}}}}} }}"
                ),
            ),
            (
                BaseCodingAgent::Codex,
                temp.path().join("config.toml"),
                McpConfig::new(
                    vec!["mcp_servers".to_string()],
                    serde_json::json!({"mcp_servers": {}}),
                    serde_json::json!({}),
                    true,
                ),
                format!(
                    "[mcp_servers.server]\ncommand = \"tool\"\nargs = [\"serve\"]\n[mcp_servers.server.env]\nTOKEN = \"{fake_secret}\"\n"
                ),
            ),
            (
                BaseCodingAgent::Hermes,
                temp.path().join("config.yaml"),
                McpConfig::hermes(),
                format!(
                    "mcp_servers:\n  server:\n    command: tool\n    args: [serve]\n    env:\n      TOKEN: {fake_secret}\n"
                ),
            ),
        ];

        for (runner, path, mcp_config, content) in fixtures {
            tokio::fs::write(&path, content)
                .await
                .expect("write fixture");
            let canonical = read_legacy_mcp_config(&runner, &path, &mcp_config)
                .await
                .expect("canonicalize vendor config");
            assert_eq!(canonical.mcp_servers["server"]["env"]["TOKEN"], fake_secret);
            assert_eq!(canonical.mcp_servers["server"]["command"], "tool");
        }
    }

    #[tokio::test]
    async fn migrates_each_runner_once_and_completed_restart_does_not_overwrite() {
        let pool = setup_pool().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("mcp.json");
        tokio::fs::write(
            &path,
            r#"{"mcpServers":{"shared":{"command":"tool","env":{"TOKEN":"fake-secret"}}}}"#,
        )
        .await
        .expect("write config");
        let source = CountingPathSource {
            configs: Mutex::new(HashMap::from([(
                BaseCodingAgent::Codex,
                (path, canonical_acp()),
            )])),
            calls: Mutex::new(Vec::new()),
        };
        let agent_id = Uuid::new_v4();
        let first =
            insert_legacy_member(&pool, agent_id, "CODEX", MemberExecutionConfig::default()).await;
        let second =
            insert_legacy_member(&pool, agent_id, "CODEX", MemberExecutionConfig::default()).await;
        let preserved = MemberMcpConfig {
            mcp_servers: BTreeMap::from([(
                "preserved".to_string(),
                serde_json::json!({"command": "existing"}),
            )]),
        };
        let initialized = insert_legacy_member(
            &pool,
            agent_id,
            "CODEX",
            MemberExecutionConfig {
                mcp: Some(preserved.clone()),
                ..Default::default()
            },
        )
        .await;

        let report = run_member_scoped_mcp_migration_with_source(&pool, &source)
            .await
            .expect("run migration");
        assert_eq!(report.migrated_members, 2);
        assert_eq!(report.runner_reads, 1);
        assert_eq!(source.calls.lock().expect("calls").len(), 1);
        let first_config = member_config(&pool, first).await;
        let second_config = member_config(&pool, second).await;
        assert_eq!(first_config.mcp, second_config.mcp);
        assert_eq!(member_config(&pool, initialized).await.mcp, Some(preserved));

        let replacement = MemberMcpConfig {
            mcp_servers: BTreeMap::from([(
                "replacement".to_string(),
                serde_json::json!({"command": "other"}),
            )]),
        };
        sqlx::query(
            "UPDATE project_members SET execution_config = json_set(execution_config, '$.mcp', json(?2)) WHERE id = ?1",
        )
        .bind(first)
        .bind(serde_json::to_string(&replacement).expect("serialize replacement"))
        .execute(&pool)
        .await
        .expect("modify migrated member");

        let restart = run_member_scoped_mcp_migration_with_source(&pool, &source)
            .await
            .expect("restart migration");
        assert!(restart.already_completed);
        assert_eq!(source.calls.lock().expect("calls").len(), 1);
        assert_eq!(member_config(&pool, first).await.mcp, Some(replacement));
        assert_eq!(member_config(&pool, second).await.mcp, second_config.mcp);

        let new_member = insert_legacy_member(
            &pool,
            Uuid::new_v4(),
            "CODEX",
            MemberExecutionConfig {
                mcp: Some(MemberMcpConfig::default()),
                ..Default::default()
            },
        )
        .await;
        let final_restart = run_member_scoped_mcp_migration_with_source(&pool, &source)
            .await
            .expect("restart after new member");
        assert!(final_restart.already_completed);
        assert_eq!(source.calls.lock().expect("calls").len(), 1);
        assert_eq!(
            member_config(&pool, new_member).await.mcp,
            Some(MemberMcpConfig::default())
        );
    }

    #[tokio::test]
    async fn missing_file_writes_empty_config_without_reading_raw_content() {
        let pool = setup_pool().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let source = CountingPathSource {
            configs: Mutex::new(HashMap::from([(
                BaseCodingAgent::Hermes,
                (temp.path().join("missing.yaml"), McpConfig::hermes()),
            )])),
            calls: Mutex::new(Vec::new()),
        };
        let member = insert_legacy_member(
            &pool,
            Uuid::new_v4(),
            "HERMES",
            MemberExecutionConfig::default(),
        )
        .await;

        run_member_scoped_mcp_migration_with_source(&pool, &source)
            .await
            .expect("migrate missing file");
        assert_eq!(
            member_config(&pool, member).await.mcp,
            Some(MemberMcpConfig::default())
        );
    }

    #[tokio::test]
    async fn malformed_file_is_secret_safe_and_retries_after_repair() {
        let pool = setup_pool().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("mcp.json");
        let fake_secret = "fake-secret-must-not-leak";
        tokio::fs::write(
            &path,
            format!(
                r#"{{"mcpServers":{{"broken":{{"command":"tool","env":{{"TOKEN":"{fake_secret}"}}}},}}}}"#
            ),
        )
        .await
        .expect("write malformed config");
        let source = CountingPathSource {
            configs: Mutex::new(HashMap::from([(
                BaseCodingAgent::ClaudeCode,
                (path.clone(), canonical_acp()),
            )])),
            calls: Mutex::new(Vec::new()),
        };
        let member = insert_legacy_member(
            &pool,
            Uuid::new_v4(),
            "CLAUDE_CODE",
            MemberExecutionConfig::default(),
        )
        .await;

        let error = run_member_scoped_mcp_migration_with_source(&pool, &source)
            .await
            .expect_err("malformed config must fail");
        assert!(!error.to_string().contains(fake_secret));
        assert!(member_config(&pool, member).await.mcp.is_none());
        let failed =
            ApplicationDataMigration::find_by_name(&pool, MEMBER_SCOPED_MCP_MIGRATION_NAME)
                .await
                .expect("read marker")
                .expect("failed marker");
        assert_eq!(failed.status, ApplicationDataMigrationStatus::Failed);
        assert!(!failed.error_summary.expect("summary").contains(fake_secret));

        tokio::fs::write(&path, r#"{"mcpServers":{"fixed":{"command":"tool"}}}"#)
            .await
            .expect("repair config");
        run_member_scoped_mcp_migration_with_source(&pool, &source)
            .await
            .expect("retry repaired config");
        assert!(member_config(&pool, member).await.mcp.is_some());
        let completed =
            ApplicationDataMigration::find_by_name(&pool, MEMBER_SCOPED_MCP_MIGRATION_NAME)
                .await
                .expect("read marker")
                .expect("completed marker");
        assert_eq!(completed.status, ApplicationDataMigrationStatus::Completed);
    }

    #[tokio::test]
    async fn transaction_failure_leaves_no_partial_member_or_completed_marker() {
        let pool = setup_pool().await;
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("mcp.json");
        tokio::fs::write(&path, r#"{"mcpServers":{"server":{"command":"tool"}}}"#)
            .await
            .expect("write config");
        let source = CountingPathSource {
            configs: Mutex::new(HashMap::from([(
                BaseCodingAgent::Codex,
                (path, canonical_acp()),
            )])),
            calls: Mutex::new(Vec::new()),
        };
        let agent_id = Uuid::new_v4();
        let first =
            insert_legacy_member(&pool, agent_id, "CODEX", MemberExecutionConfig::default()).await;
        let second =
            insert_legacy_member(&pool, agent_id, "CODEX", MemberExecutionConfig::default()).await;
        let failing_member = std::cmp::max(first, second);
        sqlx::query(&format!(
            "CREATE TRIGGER fail_member_mcp_update BEFORE UPDATE ON project_members \
             WHEN hex(OLD.id) = '{}' BEGIN SELECT RAISE(FAIL, 'forced migration failure'); END",
            failing_member.simple().to_string().to_ascii_uppercase()
        ))
        .execute(&pool)
        .await
        .expect("create failure trigger");

        run_member_scoped_mcp_migration_with_source(&pool, &source)
            .await
            .expect_err("transaction must fail");
        assert!(member_config(&pool, first).await.mcp.is_none());
        assert!(member_config(&pool, second).await.mcp.is_none());
        let marker =
            ApplicationDataMigration::find_by_name(&pool, MEMBER_SCOPED_MCP_MIGRATION_NAME)
                .await
                .expect("read marker")
                .expect("failed marker");
        assert_eq!(marker.status, ApplicationDataMigrationStatus::Failed);
    }

    #[test]
    fn public_preflight_blocks_only_legacy_none() {
        assert_eq!(
            ensure_member_scoped_mcp_initialized(&MemberExecutionConfig::default()),
            Err(MemberScopedMcpMigrationPending)
        );
        assert!(
            ensure_member_scoped_mcp_initialized(&MemberExecutionConfig {
                mcp: Some(MemberMcpConfig::default()),
                ..Default::default()
            })
            .is_ok()
        );
    }
}
