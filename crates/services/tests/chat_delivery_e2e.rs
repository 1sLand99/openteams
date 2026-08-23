#![cfg(feature = "qa-mode")]

use std::{
    collections::HashSet,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use db::{
    DBService,
    models::{
        chat_agent::{ChatAgent, CreateChatAgent},
        chat_message::{ChatMessage, ChatSenderType},
        chat_message_queue::QueuedMessageStatus,
        chat_run::{ChatRun, CreateChatRun},
        chat_session::{ChatSession, ChatSessionWorktreeMode, CreateChatSession},
        chat_session_agent::{ChatSessionAgent, ChatSessionAgentState, CreateChatSessionAgent},
        member_execution_config::MemberExecutionConfig,
    },
};
use serde_json::{Value, json};
use services::services::{
    chat,
    queued_message::{
        CreateQueuedMessage, MemberQueueSnapshot, QueuedMessage, QueuedMessageService,
        RunQueueFinalization,
    },
};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use tempfile::TempDir;
use tokio::time::sleep;
use uuid::Uuid;

/// A session member addressable by a durable chat delivery.
///
/// This type and the fixture below are intentionally public within this integration-test crate so
/// the runner-owned CDD cases can append to this file without cloning database setup or SQL.
#[derive(Debug, Clone)]
pub struct DeliveryMember {
    pub agent: ChatAgent,
    pub session_agent: ChatSessionAgent,
}

#[derive(Debug, Clone)]
pub struct DeliverySend {
    pub message: ChatMessage,
    pub created: bool,
    pub deliveries: Vec<QueuedMessage>,
}

#[derive(Debug, Clone)]
pub struct ControlledRun {
    pub delivery: QueuedMessage,
    pub run: ChatRun,
    pub runtime_revision: i64,
}

pub struct ControlledCompletion {
    pub finalization: RunQueueFinalization,
    pub output: Option<ChatMessage>,
}

/// Real migrated SQLite plus the public delivery service. No production queue SQL is duplicated
/// by the fake executor: claim, bind, transition and finalize all cross the service boundary.
pub struct ChatDeliveryFixture {
    pub db: DBService,
    pub session: ChatSession,
    pub delivery_service: QueuedMessageService,
    pub workspace_path: String,
    pub root: TempDir,
}

impl ChatDeliveryFixture {
    pub async fn new(case_name: &str) -> Result<Self> {
        let root = TempDir::new().context("create chat delivery fixture root")?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).context("create fixture workspace")?;
        let database_path = root.path().join("chat-delivery.sqlite");
        let options = SqliteConnectOptions::new()
            .filename(database_path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await
            .context("connect temporary delivery database")?;
        sqlx::migrate!("../db/migrations")
            .run(&pool)
            .await
            .context("run delivery migrations")?;
        let workspace_path = workspace.to_string_lossy().into_owned();
        let session = ChatSession::create(
            &pool,
            &CreateChatSession {
                title: Some(format!("Chat delivery {case_name}")),
                workspace_path: Some(workspace_path.clone()),
                project_id: None,
                worktree_mode: Some(ChatSessionWorktreeMode::Disabled),
            },
            Uuid::new_v4(),
        )
        .await
        .context("create delivery session")?;

        Ok(Self {
            db: DBService { pool },
            session,
            delivery_service: QueuedMessageService::new(),
            workspace_path,
            root,
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.db.pool
    }

    pub async fn add_member(&self, name: &str) -> Result<DeliveryMember> {
        let agent = ChatAgent::create(
            self.pool(),
            &CreateChatAgent {
                name: name.to_string(),
                runner_type: "ACP_QA".to_string(),
                system_prompt: Some("Deterministic chat delivery QA member".to_string()),
                tools_enabled: Some(json!({})),
                model_name: None,
                owner_project_id: None,
            },
            Uuid::new_v4(),
        )
        .await
        .context("create delivery agent")?;
        let session_agent = ChatSessionAgent::create(
            self.pool(),
            &CreateChatSessionAgent {
                session_id: self.session.id,
                agent_id: agent.id,
                member_name: Some(name.to_string()),
                workspace_path: Some(self.workspace_path.clone()),
                allowed_skill_ids: Vec::new(),
                project_member_id: None,
                execution_config: MemberExecutionConfig::default(),
            },
            Uuid::new_v4(),
        )
        .await
        .context("create delivery session member")?;
        Ok(DeliveryMember {
            agent,
            session_agent,
        })
    }

    /// Create the user message exactly once and materialize one authoritative delivery per target.
    /// Repeating the same client id returns the original message and stable delivery ids.
    pub async fn send_to_targets(
        &self,
        client_message_id: &str,
        content: &str,
        targets: &[DeliveryMember],
    ) -> Result<DeliverySend> {
        let result = chat::create_message_idempotent(
            self.pool(),
            self.session.id,
            ChatSenderType::User,
            None,
            content.to_string(),
            Some(json!({ "client_message_id": client_message_id })),
        )
        .await
        .context("create idempotent delivery source message")?;
        let mut deliveries = Vec::with_capacity(targets.len());
        for target in targets {
            deliveries.push(
                self.delivery_service
                    .create_queued(
                        self.pool(),
                        &CreateQueuedMessage {
                            session_id: self.session.id,
                            session_agent_id: target.session_agent.id,
                            agent_id: target.agent.id,
                            chat_message_id: result.message.id,
                        },
                    )
                    .await
                    .context("create authoritative target delivery")?,
            );
        }
        Ok(DeliverySend {
            message: result.message,
            created: result.created,
            deliveries,
        })
    }

    pub async fn snapshot(&self, member: &DeliveryMember) -> Result<MemberQueueSnapshot> {
        self.delivery_service
            .snapshot_for_member(
                self.pool(),
                self.session.id,
                member.session_agent.id,
                member.agent.id,
            )
            .await
            .context("read member delivery snapshot")
    }

    pub async fn delivery(&self, delivery_id: Uuid) -> Result<QueuedMessage> {
        self.delivery_service
            .find_by_id(self.pool(), delivery_id)
            .await
            .context("read delivery")?
            .context("delivery missing")
    }

    pub async fn member_state(&self, member: &DeliveryMember) -> Result<ChatSessionAgentState> {
        Ok(
            ChatSessionAgent::find_by_id(self.pool(), member.session_agent.id)
                .await
                .context("read session member")?
                .context("session member missing")?
                .state,
        )
    }

    pub async fn revision(&self) -> Result<i64> {
        self.delivery_service
            .current_runtime_revision(self.pool(), self.session.id)
            .await
            .context("read runtime revision")
    }

    pub async fn assert_single_active(&self, member: &DeliveryMember) -> Result<()> {
        let snapshot = self.snapshot(member).await?;
        let active = snapshot
            .items
            .iter()
            .filter(|item| item.message.status.is_active())
            .count();
        ensure!(active <= 1, "member has {active} active deliveries");
        Ok(())
    }
}

/// Boundary-controlled fake executor. `claim`, `bind`, and terminal release are separate calls,
/// so tests can inspect durable state at the exact crash/reload boundaries used by the matrix.
pub struct ControlledFakeExecutor {
    next_run_index: AtomicI64,
}

impl Default for ControlledFakeExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlledFakeExecutor {
    pub fn new() -> Self {
        Self {
            next_run_index: AtomicI64::new(1),
        }
    }

    pub async fn claim(
        &self,
        fixture: &ChatDeliveryFixture,
        member: &DeliveryMember,
    ) -> Result<Option<QueuedMessage>> {
        fixture
            .delivery_service
            .claim_next(fixture.pool(), member.session_agent.id)
            .await
            .context("claim next delivery")
    }

    pub async fn bind(
        &self,
        fixture: &ChatDeliveryFixture,
        member: &DeliveryMember,
        starting: &QueuedMessage,
    ) -> Result<ControlledRun> {
        ensure!(
            starting.status == QueuedMessageStatus::Starting,
            "fake executor can only bind a starting delivery"
        );
        ensure!(
            starting.session_agent_id == member.session_agent.id,
            "delivery/member mismatch"
        );
        let run_id = Uuid::new_v4();
        let run_index = self.next_run_index.fetch_add(1, Ordering::SeqCst);
        let run_dir = fixture.root.path().join(format!("run-{run_index}"));
        std::fs::create_dir_all(&run_dir).context("create controlled run directory")?;
        let binding = fixture
            .delivery_service
            .bind_delivery_to_new_run(
                fixture.pool(),
                starting.id,
                starting.revision,
                &CreateChatRun {
                    session_id: fixture.session.id,
                    session_agent_id: member.session_agent.id,
                    workspace_path: Some(fixture.workspace_path.clone()),
                    run_index,
                    run_dir: run_dir.to_string_lossy().into_owned(),
                    input_path: None,
                    output_path: None,
                    raw_log_path: None,
                    meta_path: None,
                },
                run_id,
            )
            .await
            .context("bind delivery to controlled run")?
            .context("delivery bind rejected as stale")?;
        Ok(ControlledRun {
            delivery: binding.delivery,
            run: binding.run,
            runtime_revision: binding.runtime_revision,
        })
    }

    pub async fn complete(
        &self,
        fixture: &ChatDeliveryFixture,
        member: &DeliveryMember,
        running: &QueuedMessage,
        claim_next: bool,
        output: Option<&str>,
    ) -> Result<ControlledCompletion> {
        ensure!(running.run_id.is_some(), "running delivery has no run id");
        let output = match output {
            Some(content) => Some(
                chat::create_message(
                    fixture.pool(),
                    fixture.session.id,
                    ChatSenderType::Agent,
                    Some(member.agent.id),
                    content.to_string(),
                    Some(json!({ "session_agent_id": member.session_agent.id })),
                )
                .await
                .context("persist controlled executor output")?,
            ),
            None => None,
        };
        let finalization = fixture
            .delivery_service
            .finalize_completed_run_cas(
                fixture.pool(),
                running.run_id.context("running delivery has no run id")?,
                member.session_agent.id,
                running.revision,
                claim_next,
            )
            .await
            .context("finalize controlled run")?;
        ensure!(finalization.applied, "completion CAS was not applied");
        Ok(ControlledCompletion {
            finalization,
            output,
        })
    }

    pub async fn fail(
        &self,
        fixture: &ChatDeliveryFixture,
        member: &DeliveryMember,
        running: &QueuedMessage,
        reason: &str,
    ) -> Result<RunQueueFinalization> {
        let result = fixture
            .delivery_service
            .finalize_failed_run_cas(
                fixture.pool(),
                running.run_id.context("failed delivery has no run id")?,
                member.session_agent.id,
                running.revision,
                Some(reason.to_string()),
            )
            .await
            .context("fail controlled run")?;
        ensure!(result.applied, "failure CAS was not applied");
        Ok(result)
    }
}

pub fn emit_evidence(case_id: &str, evidence: Value) {
    println!(
        "CDD_EVIDENCE {}",
        json!({ "case": case_id, "evidence": evidence })
    );
}

async fn allow_distinct_created_at() {
    // SQLite's subsecond clock is millisecond-granular. A short boundary keeps this acceptance
    // test deterministic without rewriting production ordering columns from the fixture.
    sleep(Duration::from_millis(5)).await;
}

fn delivery_ids(deliveries: &[QueuedMessage]) -> HashSet<Uuid> {
    deliveries.iter().map(|delivery| delivery.id).collect()
}

#[tokio::test]
async fn delivery_idle_send_transitions_starting_running_final() -> Result<()> {
    let fixture = ChatDeliveryFixture::new("CDD-001").await?;
    let alpha = fixture.add_member("Alpha").await?;
    let executor = ControlledFakeExecutor::new();
    let sent = fixture
        .send_to_targets("cdd-001-send", "run once", std::slice::from_ref(&alpha))
        .await?;
    ensure!(sent.created, "first send must create the source message");
    let stable_delivery_id = sent.deliveries[0].id;

    let starting = executor
        .claim(&fixture, &alpha)
        .await?
        .context("idle delivery was not claimed")?;
    ensure!(
        starting.id == stable_delivery_id,
        "claim changed delivery id"
    );
    ensure!(
        starting.status == QueuedMessageStatus::Starting,
        "claim did not persist starting"
    );
    let starting_snapshot = fixture.snapshot(&alpha).await?;
    ensure!(
        starting_snapshot.items[0].message.id == stable_delivery_id
            && starting_snapshot.items[0].message.status == QueuedMessageStatus::Starting,
        "starting snapshot does not expose the authoritative delivery"
    );

    let controlled_run = executor.bind(&fixture, &alpha, &starting).await?;
    ensure!(
        controlled_run.delivery.id == stable_delivery_id,
        "run bind changed delivery id"
    );
    ensure!(
        controlled_run.delivery.status == QueuedMessageStatus::Running
            && controlled_run.delivery.run_id == Some(controlled_run.run.id),
        "run binding was not durable"
    );
    let running_snapshot = fixture.snapshot(&alpha).await?;
    ensure!(
        running_snapshot.items[0].message.run_id == Some(controlled_run.run.id)
            && running_snapshot.items[0].message.status == QueuedMessageStatus::Running,
        "running snapshot lost run identity"
    );

    let completion = executor
        .complete(
            &fixture,
            &alpha,
            &controlled_run.delivery,
            false,
            Some("Alpha final output"),
        )
        .await?;
    let completed = fixture.delivery(stable_delivery_id).await?;
    ensure!(
        completed.status == QueuedMessageStatus::Completed,
        "delivery did not reach completed"
    );
    ensure!(
        fixture.member_state(&alpha).await? == ChatSessionAgentState::Idle,
        "member did not return to idle"
    );
    let final_snapshot = fixture.snapshot(&alpha).await?;
    ensure!(
        final_snapshot
            .items
            .iter()
            .all(|item| !item.message.status.is_active()),
        "final snapshot still exposes an active delivery"
    );
    let output = completion.output.context("controlled output missing")?;
    ensure!(
        ChatMessage::find_by_id(fixture.pool(), output.id)
            .await?
            .is_some(),
        "final output message was not persisted"
    );
    ensure!(
        starting_snapshot.revision < running_snapshot.revision
            && running_snapshot.revision < final_snapshot.revision,
        "snapshot revisions are not monotonic"
    );
    emit_evidence(
        "CDD-001",
        json!({
            "delivery_id": stable_delivery_id,
            "run_id": controlled_run.run.id,
            "statuses": [starting.status, controlled_run.delivery.status, completed.status],
            "snapshot_revisions": [
                starting_snapshot.revision,
                running_snapshot.revision,
                final_snapshot.revision
            ],
            "final_message_id": output.id,
            "member_state": fixture.member_state(&alpha).await?,
        }),
    );
    Ok(())
}

#[tokio::test]
async fn delivery_busy_member_queues_fifo() -> Result<()> {
    let fixture = ChatDeliveryFixture::new("CDD-002").await?;
    let alpha = fixture.add_member("Alpha").await?;
    let executor = ControlledFakeExecutor::new();

    let sent_a = fixture
        .send_to_targets("cdd-002-a", "A", std::slice::from_ref(&alpha))
        .await?;
    let starting_a = executor
        .claim(&fixture, &alpha)
        .await?
        .context("A was not claimed")?;
    let run_a = executor.bind(&fixture, &alpha, &starting_a).await?;

    allow_distinct_created_at().await;
    let sent_b = fixture
        .send_to_targets("cdd-002-b", "B", std::slice::from_ref(&alpha))
        .await?;
    allow_distinct_created_at().await;
    let sent_c = fixture
        .send_to_targets("cdd-002-c", "C", std::slice::from_ref(&alpha))
        .await?;
    let busy_snapshot = fixture.snapshot(&alpha).await?;
    ensure!(busy_snapshot.queued_count == 2, "B and C were not queued");
    ensure!(
        busy_snapshot
            .items
            .iter()
            .filter(|item| item.message.status.is_active())
            .count()
            == 1,
        "busy member has more than one in-flight delivery"
    );
    ensure!(
        executor.claim(&fixture, &alpha).await?.is_none(),
        "a second claim won while A was active"
    );

    let complete_a = executor
        .complete(&fixture, &alpha, &run_a.delivery, true, None)
        .await?;
    let starting_b = complete_a
        .finalization
        .next
        .context("B was not claimed after A")?;
    ensure!(
        starting_b.id == sent_b.deliveries[0].id,
        "FIFO violation: B was not second"
    );
    fixture.assert_single_active(&alpha).await?;
    let run_b = executor.bind(&fixture, &alpha, &starting_b).await?;

    let complete_b = executor
        .complete(&fixture, &alpha, &run_b.delivery, true, None)
        .await?;
    let starting_c = complete_b
        .finalization
        .next
        .context("C was not claimed after B")?;
    ensure!(
        starting_c.id == sent_c.deliveries[0].id,
        "FIFO violation: C was not third"
    );
    fixture.assert_single_active(&alpha).await?;
    let run_c = executor.bind(&fixture, &alpha, &starting_c).await?;
    let complete_c = executor
        .complete(&fixture, &alpha, &run_c.delivery, true, None)
        .await?;
    ensure!(
        complete_c.finalization.next.is_none(),
        "unexpected fourth delivery was claimed"
    );

    let all = fixture
        .delivery_service
        .list_for_member(fixture.pool(), alpha.session_agent.id)
        .await?;
    ensure!(
        all.iter()
            .all(|delivery| delivery.status == QueuedMessageStatus::Completed),
        "FIFO sequence did not terminate all deliveries"
    );
    let all_ids = delivery_ids(&all);
    ensure!(all_ids.len() == 3, "delivery identity was duplicated");
    ensure!(
        all_ids.contains(&sent_a.deliveries[0].id)
            && all_ids.contains(&sent_b.deliveries[0].id)
            && all_ids.contains(&sent_c.deliveries[0].id),
        "a source delivery disappeared"
    );
    emit_evidence(
        "CDD-002",
        json!({
            "claim_order": [starting_a.id, starting_b.id, starting_c.id],
            "run_order": [run_a.run.id, run_b.run.id, run_c.run.id],
            "delivery_count": all.len(),
            "final_statuses": all,
            "runtime_revision": fixture.revision().await?,
        }),
    );
    Ok(())
}

#[tokio::test]
async fn delivery_multi_agent_targets_are_independent() -> Result<()> {
    let fixture = ChatDeliveryFixture::new("CDD-003").await?;
    let alpha = fixture.add_member("Alpha").await?;
    let beta = fixture.add_member("Beta").await?;
    let executor = ControlledFakeExecutor::new();
    let sent = fixture
        .send_to_targets(
            "cdd-003-shared",
            "run for Alpha and Beta",
            &[alpha.clone(), beta.clone()],
        )
        .await?;
    ensure!(sent.deliveries.len() == 2, "target fan-out was incomplete");
    ensure!(
        sent.deliveries[0].chat_message_id == sent.message.id
            && sent.deliveries[1].chat_message_id == sent.message.id,
        "target deliveries do not share the source message"
    );
    ensure!(
        sent.deliveries[0].id != sent.deliveries[1].id,
        "targets reused one delivery id"
    );

    let starting_alpha = executor
        .claim(&fixture, &alpha)
        .await?
        .context("Alpha was not claimed")?;
    let starting_beta = executor
        .claim(&fixture, &beta)
        .await?
        .context("Beta was not claimed")?;
    let run_alpha = executor.bind(&fixture, &alpha, &starting_alpha).await?;
    let run_beta = executor.bind(&fixture, &beta, &starting_beta).await?;
    ensure!(
        run_alpha.run.id != run_beta.run.id,
        "targets reused one run id"
    );

    let alpha_completion = executor
        .complete(
            &fixture,
            &alpha,
            &run_alpha.delivery,
            false,
            Some("Alpha output"),
        )
        .await?;
    ensure!(alpha_completion.output.is_some(), "Alpha output missing");
    let beta_after_alpha = fixture.delivery(run_beta.delivery.id).await?;
    ensure!(
        beta_after_alpha.status == QueuedMessageStatus::Running
            && beta_after_alpha.run_id == Some(run_beta.run.id),
        "Alpha finalization changed Beta"
    );
    let alpha_snapshot = fixture.snapshot(&alpha).await?;
    let beta_snapshot = fixture.snapshot(&beta).await?;
    ensure!(
        alpha_snapshot
            .items
            .iter()
            .all(|item| !item.message.status.is_active()),
        "Alpha remained active after completion"
    );
    ensure!(
        beta_snapshot.items.iter().any(|item| {
            item.message.id == run_beta.delivery.id
                && item.message.status == QueuedMessageStatus::Running
                && item.message.run_id == Some(run_beta.run.id)
        }),
        "Beta active card disappeared"
    );

    executor
        .complete(
            &fixture,
            &beta,
            &beta_after_alpha,
            false,
            Some("Beta output"),
        )
        .await?;
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
        .bind(fixture.session.id)
        .fetch_one(fixture.pool())
        .await?;
    ensure!(run_count == 2, "multi-target run count is {run_count}");
    emit_evidence(
        "CDD-003",
        json!({
            "source_message_id": sent.message.id,
            "deliveries": [run_alpha.delivery.id, run_beta.delivery.id],
            "runs": [run_alpha.run.id, run_beta.run.id],
            "alpha_after_finalize": alpha_snapshot,
            "beta_after_alpha_finalize": beta_snapshot,
            "run_count": run_count,
        }),
    );
    Ok(())
}

#[tokio::test]
async fn delivery_send_retry_is_idempotent() -> Result<()> {
    let fixture = ChatDeliveryFixture::new("CDD-007").await?;
    let alpha = fixture.add_member("Alpha").await?;
    let beta = fixture.add_member("Beta").await?;
    let targets = [alpha.clone(), beta.clone()];
    let executor = ControlledFakeExecutor::new();

    // The caller deliberately ignores this successful commit, simulating a response timeout.
    let first = fixture
        .send_to_targets("cdd-007-timeout-key", "original request body", &targets)
        .await?;
    ensure!(first.created, "first request was not committed");
    let starting_alpha = executor
        .claim(&fixture, &alpha)
        .await?
        .context("Alpha was not claimed")?;
    let starting_beta = executor
        .claim(&fixture, &beta)
        .await?
        .context("Beta was not claimed")?;
    let run_alpha = executor.bind(&fixture, &alpha, &starting_alpha).await?;
    let run_beta = executor.bind(&fixture, &beta, &starting_beta).await?;
    let revision_before_retry = fixture.revision().await?;
    let retry = fixture
        .send_to_targets(
            "cdd-007-timeout-key",
            "retry body must not replace original",
            &targets,
        )
        .await?;
    ensure!(!retry.created, "retry created a second source message");
    ensure!(
        retry.message.id == first.message.id && retry.message.content == "original request body",
        "retry did not return the original source message"
    );
    ensure!(
        retry
            .deliveries
            .iter()
            .map(|delivery| delivery.id)
            .eq(first.deliveries.iter().map(|delivery| delivery.id)),
        "retry changed target delivery ids"
    );
    ensure!(
        fixture.revision().await? == revision_before_retry,
        "idempotent replay advanced runtime revision"
    );
    ensure!(
        executor.claim(&fixture, &alpha).await?.is_none()
            && executor.claim(&fixture, &beta).await?.is_none(),
        "retry produced a second executable claim"
    );

    let message_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_messages WHERE id = ?1")
        .bind(first.message.id)
        .fetch_one(fixture.pool())
        .await?;
    let idempotency_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_message_idempotency
         WHERE session_id = ?1 AND client_message_id = 'cdd-007-timeout-key'",
    )
    .bind(fixture.session.id)
    .fetch_one(fixture.pool())
    .await?;
    let delivery_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chat_message_queue WHERE chat_message_id = ?1")
            .bind(first.message.id)
            .fetch_one(fixture.pool())
            .await?;
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
        .bind(fixture.session.id)
        .fetch_one(fixture.pool())
        .await?;
    ensure!(message_count == 1, "source message was duplicated");
    ensure!(idempotency_count == 1, "idempotency mapping was duplicated");
    ensure!(delivery_count == 2, "target deliveries were duplicated");
    ensure!(run_count == 2, "runs were duplicated");
    emit_evidence(
        "CDD-007",
        json!({
            "client_message_id": "cdd-007-timeout-key",
            "message_id": first.message.id,
            "delivery_ids": first.deliveries.iter().map(|delivery| delivery.id).collect::<Vec<_>>(),
            "run_ids": [run_alpha.run.id, run_beta.run.id],
            "counts": {
                "messages": message_count,
                "idempotency_keys": idempotency_count,
                "deliveries": delivery_count,
                "runs": run_count,
            },
            "revision_before_retry": revision_before_retry,
            "revision_after_retry": fixture.revision().await?,
        }),
    );

    executor
        .complete(&fixture, &alpha, &run_alpha.delivery, false, None)
        .await?;
    executor
        .complete(&fixture, &beta, &run_beta.delivery, false, None)
        .await?;
    Ok(())
}

#[tokio::test]
async fn delivery_delete_removes_only_queued() -> Result<()> {
    let fixture = ChatDeliveryFixture::new("CDD-010").await?;
    let alpha = fixture.add_member("Alpha").await?;
    let beta = fixture.add_member("Beta").await?;
    let executor = ControlledFakeExecutor::new();

    let current = fixture
        .send_to_targets("cdd-010-current", "current", std::slice::from_ref(&alpha))
        .await?;
    let current_starting = executor
        .claim(&fixture, &alpha)
        .await?
        .context("current item was not claimed")?;
    let current_run = executor.bind(&fixture, &alpha, &current_starting).await?;

    allow_distinct_created_at().await;
    let queued_b = fixture
        .send_to_targets(
            "cdd-010-b",
            "B shared with Beta",
            &[alpha.clone(), beta.clone()],
        )
        .await?;
    allow_distinct_created_at().await;
    let queued_c = fixture
        .send_to_targets("cdd-010-c", "C", std::slice::from_ref(&alpha))
        .await?;
    let b_for_alpha = queued_b
        .deliveries
        .iter()
        .find(|delivery| delivery.session_agent_id == alpha.session_agent.id)
        .context("Alpha B delivery missing")?;
    let b_for_beta = queued_b
        .deliveries
        .iter()
        .find(|delivery| delivery.session_agent_id == beta.session_agent.id)
        .context("Beta B delivery missing")?;
    let c_for_alpha = &queued_c.deliveries[0];

    let deleted = fixture
        .delivery_service
        .delete_queued_cas(fixture.pool(), b_for_alpha.id, b_for_alpha.revision)
        .await?;
    ensure!(deleted == 1, "queued B was not deleted");
    ensure!(
        fixture
            .delivery_service
            .find_by_id(fixture.pool(), b_for_alpha.id)
            .await?
            .is_none(),
        "deleted B delivery still exists"
    );
    let shared_references = fixture
        .delivery_service
        .other_reference_count_for_chat_message(fixture.pool(), queued_b.message.id, b_for_alpha.id)
        .await?;
    ensure!(shared_references == 1, "Beta shared reference was lost");
    ensure!(
        ChatMessage::find_by_id(fixture.pool(), queued_b.message.id)
            .await?
            .is_some(),
        "shared source message was incorrectly deleted"
    );
    ensure!(
        fixture.delivery(b_for_beta.id).await?.status == QueuedMessageStatus::Queued,
        "deleting Alpha B changed Beta B"
    );

    let current_completion = executor
        .complete(&fixture, &alpha, &current_run.delivery, true, None)
        .await?;
    let starting_c = current_completion
        .finalization
        .next
        .context("C did not start after current completion")?;
    ensure!(
        starting_c.id == c_for_alpha.id,
        "deleted B was claimed or C ordering changed"
    );
    let delete_inflight = fixture
        .delivery_service
        .delete_queued_cas(fixture.pool(), starting_c.id, starting_c.revision)
        .await?;
    ensure!(
        delete_inflight == 0,
        "in-flight delivery was incorrectly deleted"
    );
    let c_after_delete_attempt = fixture.delivery(starting_c.id).await?;
    ensure!(
        c_after_delete_attempt.status == QueuedMessageStatus::Starting
            && c_after_delete_attempt.revision == starting_c.revision,
        "failed in-flight delete mutated C"
    );
    ensure!(
        ChatMessage::find_by_id(fixture.pool(), queued_c.message.id)
            .await?
            .is_some(),
        "failed in-flight delete removed C source message"
    );
    let run_c = executor
        .bind(&fixture, &alpha, &c_after_delete_attempt)
        .await?;
    executor
        .complete(&fixture, &alpha, &run_c.delivery, false, None)
        .await?;

    let alpha_rows = fixture
        .delivery_service
        .list_for_member(fixture.pool(), alpha.session_agent.id)
        .await?;
    ensure!(
        alpha_rows.len() == 2
            && alpha_rows
                .iter()
                .all(|delivery| delivery.status == QueuedMessageStatus::Completed),
        "Alpha queue retained or lost the wrong delivery"
    );
    emit_evidence(
        "CDD-010",
        json!({
            "current_delivery_id": current.deliveries[0].id,
            "deleted_delivery_id": b_for_alpha.id,
            "shared_beta_delivery_id": b_for_beta.id,
            "next_claimed_delivery_id": starting_c.id,
            "inflight_delete_rows": delete_inflight,
            "shared_reference_count": shared_references,
            "alpha_final_rows": alpha_rows,
            "beta_shared_delivery": fixture.delivery(b_for_beta.id).await?,
            "runtime_revision": fixture.revision().await?,
        }),
    );
    Ok(())
}

// Serial ownership handoff: Backend_2 appends CDD-006/008/009/011 below this marker and reuses
// the public fixture above. Do not duplicate migrations, delivery SQL, or reorder the five cases.

// Retained as a service-boundary reference; the accepted CDD cases below drive ChatRunner itself.
#[cfg(any())]
impl ControlledFakeExecutor {
    async fn emit_intermediate(
        &self,
        fixture: &ChatDeliveryFixture,
        member: &DeliveryMember,
        running: &QueuedMessage,
        content: &str,
    ) -> Result<ChatMessage> {
        ensure!(
            running.status == QueuedMessageStatus::Running && running.run_id.is_some(),
            "intermediate output requires a bound running delivery"
        );
        chat::create_message(
            fixture.pool(),
            fixture.session.id,
            ChatSenderType::Agent,
            Some(member.agent.id),
            content.to_string(),
            Some(json!({
                "session_agent_id": member.session_agent.id,
                "run_id": running.run_id,
                "delivery_id": running.id,
                "terminal": false,
            })),
        )
        .await
        .context("persist controlled intermediate output")
    }
}

#[cfg(any())]
#[tokio::test]
async fn delivery_intermediate_agent_send_does_not_finalize_run() -> Result<()> {
    let fixture = ChatDeliveryFixture::new("CDD-006").await?;
    let alpha = fixture.add_member("Alpha").await?;
    let executor = ControlledFakeExecutor::new();
    fixture
        .send_to_targets(
            "cdd-006-send",
            "produce intermediate and final output",
            std::slice::from_ref(&alpha),
        )
        .await?;
    let starting = executor
        .claim(&fixture, &alpha)
        .await?
        .context("CDD-006 delivery was not claimed")?;
    let controlled_run = executor.bind(&fixture, &alpha, &starting).await?;
    let revision_before_message = fixture.revision().await?;

    let intermediate = executor
        .emit_intermediate(
            &fixture,
            &alpha,
            &controlled_run.delivery,
            "Alpha intermediate output",
        )
        .await?;
    let after_intermediate = fixture.delivery(controlled_run.delivery.id).await?;
    let snapshot_after_intermediate = fixture.snapshot(&alpha).await?;
    ensure!(
        after_intermediate.status == QueuedMessageStatus::Running
            && after_intermediate.run_id == Some(controlled_run.run.id),
        "intermediate agent message finalized or detached the delivery"
    );
    ensure!(
        fixture.member_state(&alpha).await? == ChatSessionAgentState::Running,
        "intermediate agent message changed the member projection"
    );
    ensure!(
        ChatRun::find_by_id(fixture.pool(), controlled_run.run.id)
            .await?
            .is_some(),
        "intermediate agent message removed the durable run"
    );
    ensure!(
        snapshot_after_intermediate.revision == revision_before_message,
        "ordinary agent message advanced the delivery runtime revision"
    );

    let completion = executor
        .complete(
            &fixture,
            &alpha,
            &after_intermediate,
            false,
            Some("Alpha final output"),
        )
        .await?;
    let completed = fixture.delivery(controlled_run.delivery.id).await?;
    ensure!(
        completed.status == QueuedMessageStatus::Completed,
        "terminal completion did not finalize the delivery"
    );
    let message_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_messages WHERE session_id = ?1 AND sender_type = 'agent'",
    )
    .bind(fixture.session.id)
    .fetch_one(fixture.pool())
    .await?;
    let run_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_runs WHERE id = ?1 AND session_agent_id = ?2",
    )
    .bind(controlled_run.run.id)
    .bind(alpha.session_agent.id)
    .fetch_one(fixture.pool())
    .await?;
    ensure!(
        message_count == 2,
        "intermediate/final message count is {message_count}"
    );
    ensure!(run_count == 1, "durable run count is {run_count}");
    emit_evidence(
        "CDD-006",
        json!({
            "delivery": completed,
            "run": controlled_run.run,
            "intermediate_message_id": intermediate.id,
            "final_message_id": completion.output.map(|message| message.id),
            "snapshot_after_intermediate": snapshot_after_intermediate,
            "revision_before_message": revision_before_message,
            "revision_after_terminal": fixture.revision().await?,
            "database": { "agent_messages": message_count, "runs": run_count },
        }),
    );
    Ok(())
}

#[cfg(any())]
#[tokio::test]
async fn delivery_stop_is_safe_for_starting_and_running() -> Result<()> {
    let fixture = ChatDeliveryFixture::new("CDD-008").await?;
    let starting_member = fixture.add_member("Starting").await?;
    let running_member = fixture.add_member("Running").await?;
    let executor = ControlledFakeExecutor::new();

    let starting_send = fixture
        .send_to_targets(
            "cdd-008-starting",
            "stop before bind",
            std::slice::from_ref(&starting_member),
        )
        .await?;
    let starting = executor
        .claim(&fixture, &starting_member)
        .await?
        .context("starting stop delivery was not claimed")?;
    let cancelled = fixture
        .delivery_service
        .transition_status_cas(
            fixture.pool(),
            starting.id,
            starting.revision,
            QueuedMessageStatus::Starting,
            QueuedMessageStatus::Cancelled,
        )
        .await?
        .context("starting stop CAS did not apply")?;
    ensure!(
        fixture
            .delivery_service
            .transition_status_cas(
                fixture.pool(),
                starting.id,
                starting.revision,
                QueuedMessageStatus::Starting,
                QueuedMessageStatus::Cancelled,
            )
            .await?
            .is_none(),
        "stale starting stop applied twice"
    );
    ensure!(
        cancelled.status == QueuedMessageStatus::Cancelled
            && cancelled.run_id.is_none()
            && fixture.member_state(&starting_member).await? == ChatSessionAgentState::Idle,
        "starting stop did not leave a cancelled delivery and idle member"
    );

    let running_send = fixture
        .send_to_targets(
            "cdd-008-running",
            "stop after bind",
            std::slice::from_ref(&running_member),
        )
        .await?;
    let running_starting = executor
        .claim(&fixture, &running_member)
        .await?
        .context("running stop delivery was not claimed")?;
    let controlled_run = executor
        .bind(&fixture, &running_member, &running_starting)
        .await?;
    let stopping = fixture
        .delivery_service
        .transition_status_cas(
            fixture.pool(),
            controlled_run.delivery.id,
            controlled_run.delivery.revision,
            QueuedMessageStatus::Running,
            QueuedMessageStatus::Stopping,
        )
        .await?
        .context("running stop CAS did not apply")?;
    ensure!(
        fixture
            .delivery_service
            .transition_status_cas(
                fixture.pool(),
                controlled_run.delivery.id,
                controlled_run.delivery.revision,
                QueuedMessageStatus::Running,
                QueuedMessageStatus::Stopping,
            )
            .await?
            .is_none(),
        "stale running stop applied twice"
    );
    let stopped = executor
        .complete(&fixture, &running_member, &stopping, false, None)
        .await?;
    ensure!(
        stopped.finalization.applied,
        "stopping finalizer was rejected"
    );
    let completed = fixture.delivery(controlled_run.delivery.id).await?;
    ensure!(
        completed.status == QueuedMessageStatus::Completed
            && fixture.member_state(&running_member).await? == ChatSessionAgentState::Idle,
        "running stop did not finalize atomically"
    );
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
        .bind(fixture.session.id)
        .fetch_one(fixture.pool())
        .await?;
    ensure!(run_count == 1, "starting stop created an unexpected run");
    emit_evidence(
        "CDD-008",
        json!({
            "starting": {
                "source_message_id": starting_send.message.id,
                "delivery": cancelled,
                "member_state": fixture.member_state(&starting_member).await?,
            },
            "running": {
                "source_message_id": running_send.message.id,
                "run": controlled_run.run,
                "stopping_revision": stopping.revision,
                "delivery": completed,
                "member_state": fixture.member_state(&running_member).await?,
            },
            "database": { "runs": run_count },
            "runtime_revision": fixture.revision().await?,
        }),
    );
    Ok(())
}

#[cfg(any())]
#[tokio::test]
async fn delivery_failure_blocks_continue_and_starts_next() -> Result<()> {
    let fixture = ChatDeliveryFixture::new("CDD-009").await?;
    let alpha = fixture.add_member("Alpha").await?;
    let executor = ControlledFakeExecutor::new();
    let first_send = fixture
        .send_to_targets("cdd-009-first", "fail first", std::slice::from_ref(&alpha))
        .await?;
    let first_starting = executor
        .claim(&fixture, &alpha)
        .await?
        .context("first delivery was not claimed")?;
    let first_run = executor.bind(&fixture, &alpha, &first_starting).await?;
    allow_distinct_created_at().await;
    let second_send = fixture
        .send_to_targets(
            "cdd-009-second",
            "run after continue",
            std::slice::from_ref(&alpha),
        )
        .await?;

    let failure = executor
        .fail(
            &fixture,
            &alpha,
            &first_run.delivery,
            "controlled executor failure",
        )
        .await?;
    let failed = fixture.delivery(first_run.delivery.id).await?;
    let blocked_snapshot = fixture.snapshot(&alpha).await?;
    ensure!(
        failed.status == QueuedMessageStatus::Failed
            && failed.failure_reason.as_deref() == Some("controlled executor failure"),
        "failed run did not persist failure evidence"
    );
    ensure!(
        blocked_snapshot.blocked
            && blocked_snapshot.can_continue
            && blocked_snapshot.queued_count == 1,
        "failed delivery did not block the queued successor"
    );
    ensure!(
        executor.claim(&fixture, &alpha).await?.is_none(),
        "blocked queue claimed a successor before continue"
    );

    let skipped = fixture
        .delivery_service
        .skip_failed_for_member(fixture.pool(), alpha.session_agent.id)
        .await?;
    ensure!(
        skipped == 1,
        "continue did not skip exactly one failed delivery"
    );
    let second_starting = executor
        .claim(&fixture, &alpha)
        .await?
        .context("successor was not claimed after continue")?;
    ensure!(
        second_starting.id == second_send.deliveries[0].id,
        "continue claimed the wrong successor"
    );
    let second_run = executor.bind(&fixture, &alpha, &second_starting).await?;
    executor
        .complete(&fixture, &alpha, &second_run.delivery, false, None)
        .await?;
    let first_terminal = fixture.delivery(first_run.delivery.id).await?;
    let second_terminal = fixture.delivery(second_run.delivery.id).await?;
    ensure!(
        first_terminal.status == QueuedMessageStatus::Skipped
            && second_terminal.status == QueuedMessageStatus::Completed,
        "continue did not preserve skipped failure and completed successor"
    );
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
        .bind(fixture.session.id)
        .fetch_one(fixture.pool())
        .await?;
    ensure!(run_count == 2, "failure/continue run count is {run_count}");
    emit_evidence(
        "CDD-009",
        json!({
            "source_messages": [first_send.message.id, second_send.message.id],
            "failed_run": first_run.run,
            "successor_run": second_run.run,
            "failed_delivery_after_continue": first_terminal,
            "successor_delivery": second_terminal,
            "failure_finalization_revision": failure.runtime_revision,
            "blocked_snapshot": blocked_snapshot,
            "database": { "runs": run_count },
            "runtime_revision": fixture.revision().await?,
        }),
    );
    Ok(())
}

#[cfg(any())]
#[tokio::test]
async fn delivery_recovers_claim_bind_and_finalize_boundaries() -> Result<()> {
    let fixture = ChatDeliveryFixture::new("CDD-011").await?;
    let alpha = fixture.add_member("Alpha").await?;
    let executor = ControlledFakeExecutor::new();
    let sent = fixture
        .send_to_targets(
            "cdd-011-send",
            "survive every durable boundary",
            std::slice::from_ref(&alpha),
        )
        .await?;
    let starting = executor
        .claim(&fixture, &alpha)
        .await?
        .context("recovery delivery was not claimed")?;

    let recovered_after_claim = QueuedMessageService::new();
    let unbound = recovered_after_claim
        .list_unbound_processing(fixture.pool())
        .await?;
    ensure!(
        unbound.iter().any(|delivery| {
            delivery.id == starting.id
                && delivery.status == QueuedMessageStatus::Starting
                && delivery.run_id.is_none()
        }),
        "cold recovery lost the committed claim boundary"
    );
    let runs_after_claim: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
            .bind(fixture.session.id)
            .fetch_one(fixture.pool())
            .await?;
    ensure!(runs_after_claim == 0, "claim boundary leaked a chat run");

    let controlled_run = executor.bind(&fixture, &alpha, &starting).await?;
    let recovered_after_bind = QueuedMessageService::new();
    let running = recovered_after_bind
        .find_by_run_id(fixture.pool(), controlled_run.run.id)
        .await?
        .context("cold recovery lost the bound run")?;
    ensure!(
        running.id == sent.deliveries[0].id
            && running.status == QueuedMessageStatus::Running
            && fixture.member_state(&alpha).await? == ChatSessionAgentState::Running,
        "bound recovery disagreed across delivery/run/member state"
    );
    let duplicate_run_id = Uuid::new_v4();
    let duplicate_bind = recovered_after_bind
        .bind_delivery_to_new_run(
            fixture.pool(),
            starting.id,
            starting.revision,
            &CreateChatRun {
                session_id: fixture.session.id,
                session_agent_id: alpha.session_agent.id,
                workspace_path: Some(fixture.workspace_path.clone()),
                run_index: 99,
                run_dir: fixture
                    .root
                    .path()
                    .join("duplicate-run")
                    .to_string_lossy()
                    .into_owned(),
                input_path: None,
                output_path: None,
                raw_log_path: None,
                meta_path: None,
            },
            duplicate_run_id,
        )
        .await?;
    ensure!(
        duplicate_bind.is_none(),
        "stale bind created a duplicate run"
    );
    ensure!(
        ChatRun::find_by_id(fixture.pool(), duplicate_run_id)
            .await?
            .is_none(),
        "stale bind did not roll back its ChatRun insert"
    );

    let completion = executor
        .complete(&fixture, &alpha, &running, false, Some("recovered final"))
        .await?;
    let recovered_after_finalize = QueuedMessageService::new();
    let completed = recovered_after_finalize
        .find_by_id(fixture.pool(), running.id)
        .await?
        .context("cold recovery lost the finalized delivery")?;
    ensure!(
        completed.status == QueuedMessageStatus::Completed
            && fixture.member_state(&alpha).await? == ChatSessionAgentState::Idle,
        "finalize recovery disagreed across delivery/member state"
    );
    let repeated_finalize = recovered_after_finalize
        .finalize_completed_run_cas(
            fixture.pool(),
            controlled_run.run.id,
            alpha.session_agent.id,
            running.revision,
            false,
        )
        .await?;
    ensure!(
        !repeated_finalize.applied && repeated_finalize.next.is_none(),
        "replayed finalizer applied more than once"
    );
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
        .bind(fixture.session.id)
        .fetch_one(fixture.pool())
        .await?;
    let delivery_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_message_queue WHERE chat_message_id = ?1 AND session_agent_id = ?2",
    )
    .bind(sent.message.id)
    .bind(alpha.session_agent.id)
    .fetch_one(fixture.pool())
    .await?;
    ensure!(run_count == 1, "recovery created {run_count} chat runs");
    ensure!(
        delivery_count == 1,
        "recovery created {delivery_count} deliveries"
    );
    emit_evidence(
        "CDD-011",
        json!({
            "claim_boundary": {
                "delivery": starting,
                "unbound_recovered": unbound,
                "runs": runs_after_claim,
            },
            "bind_boundary": {
                "delivery": running,
                "run": controlled_run.run,
                "runtime_revision": controlled_run.runtime_revision,
                "duplicate_run_rolled_back": true,
            },
            "finalize_boundary": {
                "delivery": completed,
                "final_message_id": completion.output.map(|message| message.id),
                "runtime_revision": completion.finalization.runtime_revision,
                "replayed_applied": repeated_finalize.applied,
            },
            "database": { "runs": run_count, "deliveries": delivery_count },
        }),
    );
    Ok(())
}

use services::services::chat_runner::{ChatRunner, ChatStreamEvent};

static RUNNER_QA_ENVIRONMENT_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

struct RunnerQaEnvironment {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
    gate_path: std::path::PathBuf,
}

impl RunnerQaEnvironment {
    fn install(fixture: &ChatDeliveryFixture) -> Result<Self> {
        let node = which::which("node").context("locate node for controlled ACP executor")?;
        let gate_path = fixture.root.path().join("controlled-executor.gate");
        let script_path = fixture.root.path().join("controlled-fake-acp.mjs");
        let original =
            include_str!("../../executors/tests/fixtures/hermes_acp/fake_hermes_acp.mjs");
        let claim = r#"    case "session/prompt": {
      const text = params?.prompt?.find((b) => b.type === "text")?.text || "";
      const sid = params?.sessionId || SESSION_ID;
"#;
        let replacement = r#"    case "session/prompt": {
      const text = params?.prompt?.find((b) => b.type === "text")?.text || "";
      const sid = params?.sessionId || SESSION_ID;
      if (text.includes("[qa:gate]") || text.includes("[qa:crash-gated]")) {
        const gate = process.env.OPENTEAMS_FAKE_HERMES_GATE;
        while (gate && !existsSync(gate)) {
          sleepMs(5);
        }
        if (text.includes("[qa:crash-gated]")) {
          process.exit(17);
        }
      }
"#;
        let controlled = original.replacen(claim, replacement, 1);
        ensure!(
            controlled != original,
            "controlled ACP fixture injection point changed"
        );
        std::fs::write(&script_path, controlled).context("write controlled ACP fixture")?;

        let settings = [
            (
                "OPENTEAMS_ACP_QA_AGENT_COMMAND",
                Some(node.into_os_string()),
            ),
            (
                "OPENTEAMS_ACP_QA_AGENT_ARGUMENT",
                Some(script_path.into_os_string()),
            ),
            (
                "OPENTEAMS_FAKE_HERMES_GATE",
                Some(gate_path.clone().into_os_string()),
            ),
            ("OPENTEAMS_ACP_QA_MCP_CONFIG_PATH", None),
            ("OPENTEAMS_FAKE_HERMES_ERROR", None),
            ("OPENTEAMS_FAKE_HERMES_HANG", None),
            ("OPENTEAMS_FAKE_HERMES_PROBE_FAIL", None),
            ("OPENTEAMS_FAKE_HERMES_SESSION_PROBE_FAIL", None),
        ];
        let previous = settings
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in settings {
            // The four runner-backed cases hold RUNNER_QA_ENVIRONMENT_LOCK for the complete
            // lifetime of these process-only executor settings.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
        Ok(Self {
            previous,
            gate_path,
        })
    }

    fn release_gate(&self) -> Result<()> {
        std::fs::write(&self.gate_path, b"release").context("release controlled ACP gate")
    }
}

impl Drop for RunnerQaEnvironment {
    fn drop(&mut self) {
        for (key, value) in self.previous.drain(..) {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

async fn create_runner_user_message(
    fixture: &ChatDeliveryFixture,
    member: &DeliveryMember,
    client_message_id: &str,
    directive: &str,
) -> Result<ChatMessage> {
    Ok(chat::create_message_idempotent(
        fixture.pool(),
        fixture.session.id,
        ChatSenderType::User,
        None,
        format!("@{} {directive}", member.agent.name),
        Some(json!({ "client_message_id": client_message_id })),
    )
    .await?
    .message)
}

async fn add_runner_member(fixture: &ChatDeliveryFixture, name: &str) -> Result<DeliveryMember> {
    let mut member = fixture.add_member(name).await?;
    member.session_agent = ChatSessionAgent::update_execution_config_for_next_run(
        fixture.pool(),
        member.session_agent.id,
        None,
        MemberExecutionConfig {
            mcp: Some(executors::mcp_config::MemberMcpConfig::default()),
            ..Default::default()
        },
    )
    .await?;
    Ok(member)
}

async fn wait_for_delivery_matching(
    fixture: &ChatDeliveryFixture,
    member: &DeliveryMember,
    chat_message_id: Uuid,
    description: &str,
    predicate: impl Fn(&QueuedMessage) -> bool,
) -> Result<QueuedMessage> {
    let waited = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let deliveries = fixture
                .delivery_service
                .list_for_member(fixture.pool(), member.session_agent.id)
                .await?;
            if let Some(delivery) = deliveries
                .into_iter()
                .find(|delivery| delivery.chat_message_id == chat_message_id && predicate(delivery))
            {
                return Ok::<_, sqlx::Error>(delivery);
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    match waited {
        Ok(result) => result.map_err(Into::into),
        Err(_) => {
            let deliveries = fixture
                .delivery_service
                .list_for_member(fixture.pool(), member.session_agent.id)
                .await?;
            let member_state = fixture.member_state(member).await?;
            let runs = ChatRun::list_all(fixture.pool()).await?;
            anyhow::bail!(
                "timed out waiting for {description}; deliveries={deliveries:?}; member_state={member_state:?}; runs={runs:?}"
            );
        }
    }
}

async fn wait_for_delivery_status(
    fixture: &ChatDeliveryFixture,
    member: &DeliveryMember,
    chat_message_id: Uuid,
    status: QueuedMessageStatus,
) -> Result<QueuedMessage> {
    wait_for_delivery_matching(
        fixture,
        member,
        chat_message_id,
        &format!("delivery status {status:?}"),
        |delivery| delivery.status == status,
    )
    .await
}

#[tokio::test]
async fn delivery_intermediate_agent_send_does_not_finalize_run() -> Result<()> {
    let _environment_lock = RUNNER_QA_ENVIRONMENT_LOCK.lock().await;
    let fixture = ChatDeliveryFixture::new("CDD-006-runner").await?;
    let environment = RunnerQaEnvironment::install(&fixture)?;
    let alpha = add_runner_member(&fixture, "Alpha").await?;
    let runner = ChatRunner::new(fixture.db.clone());
    let mut events = runner.subscribe(fixture.session.id);
    let source = create_runner_user_message(
        &fixture,
        &alpha,
        "cdd-006-runner",
        "[qa:gate] hold the real executor",
    )
    .await?;

    runner.handle_message(&fixture.session, &source).await;
    let running =
        wait_for_delivery_status(&fixture, &alpha, source.id, QueuedMessageStatus::Running).await?;
    let run_id = running.run_id.context("runner did not bind CDD-006 run")?;
    let revision_before_intermediate = fixture.revision().await?;
    let intermediate = chat::create_message(
        fixture.pool(),
        fixture.session.id,
        ChatSenderType::Agent,
        Some(alpha.agent.id),
        "controlled intermediate agent message".to_string(),
        Some(json!({ "session_agent_id": alpha.session_agent.id })),
    )
    .await?;
    runner.handle_message(&fixture.session, &intermediate).await;

    let after_intermediate = fixture.delivery(running.id).await?;
    ensure!(
        after_intermediate.status == QueuedMessageStatus::Running
            && after_intermediate.run_id == Some(run_id)
            && after_intermediate.revision == running.revision,
        "production message_new path finalized or mutated the running delivery"
    );
    ensure!(
        fixture.revision().await? == revision_before_intermediate,
        "ordinary message_new advanced the delivery runtime revision"
    );
    ensure!(
        fixture.member_state(&alpha).await? == ChatSessionAgentState::Running,
        "ordinary message_new changed the running member projection"
    );
    let observed_intermediate = std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
        matches!(event, ChatStreamEvent::MessageNew { message } if message.id == intermediate.id)
    });
    ensure!(
        observed_intermediate,
        "production stream did not broadcast the intermediate message"
    );

    environment.release_gate()?;
    let completed =
        wait_for_delivery_status(&fixture, &alpha, source.id, QueuedMessageStatus::Completed)
            .await?;
    ensure!(
        fixture.member_state(&alpha).await? == ChatSessionAgentState::Idle,
        "terminal lifecycle did not return the member to idle"
    );
    let run = ChatRun::find_by_id(fixture.pool(), run_id)
        .await?
        .context("CDD-006 durable run missing")?;
    let agent_message_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_messages WHERE session_id = ?1 AND sender_type = 'agent'",
    )
    .bind(fixture.session.id)
    .fetch_one(fixture.pool())
    .await?;
    ensure!(
        agent_message_count >= 2,
        "expected intermediate and terminal agent messages"
    );
    emit_evidence(
        "CDD-006",
        json!({
            "source_message_id": source.id,
            "intermediate_message_id": intermediate.id,
            "delivery_after_intermediate": after_intermediate,
            "terminal_delivery": completed,
            "run": run,
            "revision_before_intermediate": revision_before_intermediate,
            "revision_after_terminal": fixture.revision().await?,
            "database": { "agent_messages": agent_message_count },
        }),
    );
    Ok(())
}

#[tokio::test]
async fn delivery_stop_is_safe_for_starting_and_running() -> Result<()> {
    let _environment_lock = RUNNER_QA_ENVIRONMENT_LOCK.lock().await;
    let fixture = ChatDeliveryFixture::new("CDD-008-runner").await?;
    let environment = RunnerQaEnvironment::install(&fixture)?;
    let starting_member = add_runner_member(&fixture, "Starting").await?;
    let running_member = add_runner_member(&fixture, "Running").await?;
    let runner = ChatRunner::new(fixture.db.clone());

    let starting_source = create_runner_user_message(
        &fixture,
        &starting_member,
        "cdd-008-starting-runner",
        "pause after claim",
    )
    .await?;
    runner.qa_pause_after_delivery_claim();
    let starting_runner = runner.clone();
    let starting_session = fixture.session.clone();
    let starting_message = starting_source.clone();
    let starting_task = tokio::spawn(async move {
        starting_runner
            .handle_message(&starting_session, &starting_message)
            .await;
    });
    tokio::time::timeout(Duration::from_secs(10), runner.qa_wait_for_delivery_claim())
        .await
        .context("runner did not reach starting claim boundary")?;
    let starting = wait_for_delivery_status(
        &fixture,
        &starting_member,
        starting_source.id,
        QueuedMessageStatus::Starting,
    )
    .await?;
    runner
        .stop_agent(fixture.session.id, starting_member.session_agent.id)
        .await?;
    let cancelled = wait_for_delivery_status(
        &fixture,
        &starting_member,
        starting_source.id,
        QueuedMessageStatus::Cancelled,
    )
    .await?;
    runner.qa_release_delivery_claim();
    starting_task.await.context("join starting stop dispatch")?;
    ensure!(
        cancelled.revision == starting.revision + 1
            && cancelled.run_id.is_none()
            && fixture.member_state(&starting_member).await? == ChatSessionAgentState::Idle,
        "starting stop was not a single safe CAS transition"
    );

    let running_source = create_runner_user_message(
        &fixture,
        &running_member,
        "cdd-008-running-runner",
        "[qa:gate] let executor finalization race production stop",
    )
    .await?;
    runner
        .handle_message(&fixture.session, &running_source)
        .await;
    let running = wait_for_delivery_status(
        &fixture,
        &running_member,
        running_source.id,
        QueuedMessageStatus::Running,
    )
    .await?;
    runner.qa_pause_before_stop_transition();
    let stop_runner = runner.clone();
    let stop_session_id = fixture.session.id;
    let stop_member_id = running_member.session_agent.id;
    let stop_task = tokio::spawn(async move {
        stop_runner
            .stop_agent(stop_session_id, stop_member_id)
            .await
    });
    tokio::time::timeout(
        Duration::from_secs(10),
        runner.qa_wait_for_stop_transition(),
    )
    .await
    .context("production stop did not reach the pre-CAS barrier")?;
    environment.release_gate()?;
    let completed = wait_for_delivery_status(
        &fixture,
        &running_member,
        running_source.id,
        QueuedMessageStatus::Completed,
    )
    .await?;
    let revision_after_finalize = fixture.revision().await?;
    runner.qa_release_stop_transition();
    stop_task
        .await
        .context("join stop/finalize race")?
        .context("production stop failed after losing finalize race")?;
    sleep(Duration::from_millis(50)).await;
    let after_stale_stop = fixture.delivery(running.id).await?;
    ensure!(
        completed.run_id == running.run_id
            && completed.revision == running.revision + 1
            && after_stale_stop.status == QueuedMessageStatus::Completed
            && after_stale_stop.revision == completed.revision
            && fixture.revision().await? == revision_after_finalize
            && fixture.member_state(&running_member).await? == ChatSessionAgentState::Idle,
        "stale stop mutated the executor's committed terminal finalization"
    );
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
        .bind(fixture.session.id)
        .fetch_one(fixture.pool())
        .await?;
    ensure!(
        run_count == 1,
        "starting stop created an unexpected ChatRun"
    );
    let final_message_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_messages WHERE session_id = ?1 AND sender_type = 'agent' AND sender_id = ?2",
    )
    .bind(fixture.session.id)
    .bind(running_member.agent.id)
    .fetch_one(fixture.pool())
    .await?;
    ensure!(
        final_message_count == 1,
        "stop/finalize race produced {final_message_count} final agent messages"
    );
    emit_evidence(
        "CDD-008",
        json!({
            "starting": {
                "source_message_id": starting_source.id,
                "starting_delivery": starting,
                "cancelled_delivery": cancelled,
                "member_state": fixture.member_state(&starting_member).await?,
            },
            "running": {
                "source_message_id": running_source.id,
                "running_delivery": running,
                "terminal_delivery": completed,
                "delivery_after_stale_stop": after_stale_stop,
                "member_state": fixture.member_state(&running_member).await?,
                "stop_cas_applied": false,
            },
            "database": {
                "runs": run_count,
                "final_agent_messages": final_message_count,
            },
            "runtime_revision": {
                "after_finalize": revision_after_finalize,
                "after_stale_stop": fixture.revision().await?,
            },
        }),
    );
    Ok(())
}

#[tokio::test]
async fn delivery_failure_blocks_continue_and_starts_next() -> Result<()> {
    let _environment_lock = RUNNER_QA_ENVIRONMENT_LOCK.lock().await;
    let fixture = ChatDeliveryFixture::new("CDD-009-runner").await?;
    let environment = RunnerQaEnvironment::install(&fixture)?;
    let alpha = add_runner_member(&fixture, "Alpha").await?;
    let runner = ChatRunner::new(fixture.db.clone());
    let first_source = create_runner_user_message(
        &fixture,
        &alpha,
        "cdd-009-first-runner",
        "[qa:crash-gated] fail after successor is durable",
    )
    .await?;
    runner.handle_message(&fixture.session, &first_source).await;
    let first_running = wait_for_delivery_status(
        &fixture,
        &alpha,
        first_source.id,
        QueuedMessageStatus::Running,
    )
    .await?;

    let second_source = create_runner_user_message(
        &fixture,
        &alpha,
        "cdd-009-second-runner",
        "run after continue",
    )
    .await?;
    runner
        .handle_message(&fixture.session, &second_source)
        .await;
    let second_queued = wait_for_delivery_status(
        &fixture,
        &alpha,
        second_source.id,
        QueuedMessageStatus::Queued,
    )
    .await?;
    environment.release_gate()?;
    let first_failed = wait_for_delivery_status(
        &fixture,
        &alpha,
        first_source.id,
        QueuedMessageStatus::Failed,
    )
    .await?;
    let blocked_snapshot = fixture.snapshot(&alpha).await?;
    ensure!(
        blocked_snapshot.blocked
            && blocked_snapshot.can_continue
            && blocked_snapshot.queued_count == 1,
        "production failure did not block the durable successor"
    );
    ensure!(
        fixture
            .delivery_service
            .claim_next(fixture.pool(), alpha.session_agent.id)
            .await?
            .is_none(),
        "blocked production queue admitted a claim before continue"
    );

    let skipped = fixture
        .delivery_service
        .skip_failed_for_member(fixture.pool(), alpha.session_agent.id)
        .await?;
    ensure!(skipped == 1, "continue did not skip the failed delivery");
    runner
        .dispatch_next_queued_message(fixture.session.id, alpha.session_agent.id)
        .await;
    let second_completed = wait_for_delivery_status(
        &fixture,
        &alpha,
        second_source.id,
        QueuedMessageStatus::Completed,
    )
    .await?;
    let first_skipped = fixture.delivery(first_failed.id).await?;
    ensure!(
        first_skipped.status == QueuedMessageStatus::Skipped
            && second_completed.id == second_queued.id
            && second_completed.run_id.is_some(),
        "continue did not preserve identity and start the queued successor"
    );
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
        .bind(fixture.session.id)
        .fetch_one(fixture.pool())
        .await?;
    ensure!(
        run_count == 2,
        "production failure/continue run count is {run_count}"
    );
    emit_evidence(
        "CDD-009",
        json!({
            "first_source_message_id": first_source.id,
            "second_source_message_id": second_source.id,
            "first_running_delivery": first_running,
            "failed_delivery": first_failed,
            "blocked_snapshot": blocked_snapshot,
            "failed_delivery_after_continue": first_skipped,
            "successor_delivery": second_completed,
            "database": { "runs": run_count },
            "runtime_revision": fixture.revision().await?,
        }),
    );
    Ok(())
}

#[tokio::test]
async fn delivery_recovers_claim_bind_and_finalize_boundaries() -> Result<()> {
    let _environment_lock = RUNNER_QA_ENVIRONMENT_LOCK.lock().await;
    let fixture = ChatDeliveryFixture::new("CDD-011-runner").await?;
    let environment = RunnerQaEnvironment::install(&fixture)?;
    let alpha = add_runner_member(&fixture, "Alpha").await?;
    let source = create_runner_user_message(
        &fixture,
        &alpha,
        "cdd-011-runner",
        "[qa:gate] survive claim bind and finalize crashes",
    )
    .await?;

    let claim_runner = ChatRunner::new(fixture.db.clone());
    claim_runner.qa_pause_after_delivery_claim();
    let claim_task_runner = claim_runner.clone();
    let claim_session = fixture.session.clone();
    let claim_message = source.clone();
    let claim_task = tokio::spawn(async move {
        claim_task_runner
            .handle_message(&claim_session, &claim_message)
            .await;
    });
    tokio::time::timeout(
        Duration::from_secs(10),
        claim_runner.qa_wait_for_delivery_claim(),
    )
    .await
    .context("runner did not reach recoverable claim boundary")?;
    let claimed =
        wait_for_delivery_status(&fixture, &alpha, source.id, QueuedMessageStatus::Starting)
            .await?;
    let claim_snapshot = fixture.snapshot(&alpha).await?;
    claim_task.abort();
    let _ = claim_task.await;
    drop(claim_runner);
    let runs_after_claim: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
            .bind(fixture.session.id)
            .fetch_one(fixture.pool())
            .await?;
    ensure!(runs_after_claim == 0, "claim crash leaked a ChatRun");

    let bind_runner = ChatRunner::new(fixture.db.clone());
    bind_runner.qa_stop_after_delivery_bind(true);
    ensure!(
        bind_runner.recover_orphaned_session_agents().await? == 1,
        "claim-boundary recovery did not find the member"
    );
    let bound =
        wait_for_delivery_status(&fixture, &alpha, source.id, QueuedMessageStatus::Running).await?;
    let bound_run_id = bound.run_id.context("bind recovery did not create a run")?;
    let bind_snapshot = fixture.snapshot(&alpha).await?;
    ensure!(
        bound.id == claimed.id && bound.attempt_no == claimed.attempt_no + 1,
        "claim recovery changed delivery identity or attempt sequence"
    );
    sleep(Duration::from_millis(50)).await;
    drop(bind_runner);

    let finalize_runner = ChatRunner::new(fixture.db.clone());
    ensure!(
        finalize_runner.recover_orphaned_session_agents().await? == 1,
        "bind-boundary recovery did not find the active run"
    );
    let rebound = wait_for_delivery_matching(
        &fixture,
        &alpha,
        source.id,
        "rebound running delivery",
        |delivery| {
            delivery.status == QueuedMessageStatus::Running
                && delivery.run_id.is_some()
                && delivery.run_id != Some(bound_run_id)
        },
    )
    .await?;
    let rebound_run_id = rebound
        .run_id
        .context("finalize recovery did not bind a run")?;
    environment.release_gate()?;
    let completed =
        wait_for_delivery_status(&fixture, &alpha, source.id, QueuedMessageStatus::Completed)
            .await?;
    let finalize_snapshot = fixture.snapshot(&alpha).await?;
    let revision_after_finalize = fixture.revision().await?;
    drop(finalize_runner);

    let post_finalize_runner = ChatRunner::new(fixture.db.clone());
    ensure!(
        post_finalize_runner
            .recover_orphaned_session_agents()
            .await?
            == 0,
        "finalize-boundary recovery found terminal work"
    );
    let stale_finalize = fixture
        .delivery_service
        .finalize_completed_run_cas(
            fixture.pool(),
            rebound_run_id,
            alpha.session_agent.id,
            rebound.revision,
            false,
        )
        .await?;
    ensure!(
        !stale_finalize.applied
            && stale_finalize.next.is_none()
            && fixture.revision().await? == revision_after_finalize,
        "stale terminal CAS mutated a recovered completed run"
    );
    let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_runs WHERE session_id = ?1")
        .bind(fixture.session.id)
        .fetch_one(fixture.pool())
        .await?;
    let delivery_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM chat_message_queue WHERE chat_message_id = ?1 AND session_agent_id = ?2",
    )
    .bind(source.id)
    .bind(alpha.session_agent.id)
    .fetch_one(fixture.pool())
    .await?;
    ensure!(run_count == 2, "recovery created {run_count} durable runs");
    ensure!(
        delivery_count == 1,
        "recovery duplicated the stable delivery"
    );
    emit_evidence(
        "CDD-011",
        json!({
            "claim_boundary": {
                "delivery": claimed,
                "snapshot": claim_snapshot,
                "runs": runs_after_claim,
            },
            "bind_boundary": {
                "delivery": bound,
                "run_id": bound_run_id,
                "snapshot": bind_snapshot,
            },
            "finalize_boundary": {
                "rebound_delivery": rebound,
                "rebound_run_id": rebound_run_id,
                "terminal_delivery": completed,
                "snapshot": finalize_snapshot,
                "stale_finalize_applied": stale_finalize.applied,
            },
            "database": { "runs": run_count, "deliveries": delivery_count },
            "runtime_revision": revision_after_finalize,
        }),
    );
    Ok(())
}
