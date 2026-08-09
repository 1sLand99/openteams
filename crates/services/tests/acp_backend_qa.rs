#![cfg(feature = "qa-mode")]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use db::{
    DBService,
    models::{
        chat_agent::{ChatAgent, CreateChatAgent},
        chat_executor_approval_request::{ChatExecutorApprovalRequest, ChatExecutorApprovalStatus},
        chat_message::{ChatMessage, ChatSenderType},
        chat_run::{ChatRun, ChatRunRetentionSummary},
        chat_session::{ChatSession, ChatSessionWorktreeMode, CreateChatSession},
        chat_session_agent::{ChatSessionAgent, ChatSessionAgentState, CreateChatSessionAgent},
        member_execution_config::MemberExecutionConfig,
        workflow_agent_session::{CreateWorkflowAgentSession, WorkflowAgentSession},
        workflow_execution::{CreateWorkflowExecution, WorkflowExecution},
        workflow_plan::{CreateWorkflowPlan, WorkflowPlan},
        workflow_plan_revision::{CreateWorkflowPlanRevision, WorkflowPlanRevision},
        workflow_round::{CreateWorkflowRound, WorkflowRound},
        workflow_step::{CreateWorkflowStep, WorkflowStep},
        workflow_transcript::WorkflowTranscript,
        workflow_types::{
            WorkflowAgentSessionRole, WorkflowRevisionEditor, WorkflowStepType,
            WorkflowValidationStatus,
        },
    },
};
use services::services::{
    approvals::executor_approvals::{ExecutorApprovalBridge, ExecutorApprovalEvent},
    chat,
    chat_runner::{ChatRunner, ChatStreamEvent},
    workflow_runtime::{
        WorkflowRuntimeError, cancel_running_step, run_workflow_step_agent_follow_up,
        run_workflow_step_agent_prompt,
    },
};
use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use uuid::Uuid;

struct HermesFixturePaths {
    prompts: PathBuf,
    permission_log: PathBuf,
    protocol_log: PathBuf,
}

#[derive(Clone)]
struct HermesMember {
    agent: ChatAgent,
    session_agent: ChatSessionAgent,
    allowed_mcp: &'static str,
}

#[derive(Debug, serde::Deserialize)]
struct HermesProtocolEvent {
    event: String,
    method: Option<String>,
    session_id: Option<String>,
    prompt_tag: Option<String>,
    mcp_servers: Option<Vec<String>>,
}

fn main() {
    let fixture = TempDir::new().expect("create ACP backend QA fixture");
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create QA workspace");
    init_git_repo(&workspace);
    let hermes = install_hermes_fixture(fixture.path());
    let home = fixture.path().join("home");
    fs::create_dir_all(home.join(".hermes")).expect("create Hermes home");
    fs::write(
        home.join(".hermes/config.yaml"),
        "mcp_servers:\n  alpha:\n    command: \"true\"\n  beta:\n    command: \"true\"\n",
    )
    .expect("write Hermes MCP configuration");

    // The custom test harness is single-threaded and sets fixture-only process
    // configuration before starting Tokio or any worker process. The fake
    // `hermes` is first on PATH, so this test cannot resolve a real CLI.
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    unsafe {
        std::env::set_var(
            "PATH",
            format!(
                "{}:{}",
                fixture.path().join("bin").display(),
                inherited_path.to_string_lossy()
            ),
        );
        std::env::set_var("HOME", home);
        std::env::set_var("OPENTEAMS_FAKE_HERMES_PROMPTS", &hermes.prompts);
        std::env::set_var(
            "OPENTEAMS_FAKE_HERMES_PERMISSION_LOG",
            &hermes.permission_log,
        );
        std::env::set_var("OPENTEAMS_FAKE_HERMES_PROTOCOL_LOG", &hermes.protocol_log);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build ACP backend QA runtime");
    if let Err(error) = runtime.block_on(run_acceptance(fixture.path(), &workspace, &hermes)) {
        eprintln!("ACP backend QA acceptance failed: {error:#}");
        std::process::exit(1);
    }
    println!("ACP backend QA acceptance passed");
}

fn install_hermes_fixture(root: &Path) -> HermesFixturePaths {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create Hermes fixture bin");
    let source = include_str!("../../executors/tests/fixtures/hermes_acp/fake_hermes_acp.mjs");
    let executable = bin.join("hermes");
    fs::write(&executable, source).expect("write fake Hermes executable");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
        .expect("make fake Hermes executable");
    HermesFixturePaths {
        prompts: root.join("hermes-prompts.txt"),
        permission_log: root.join("hermes-permissions.jsonl"),
        protocol_log: root.join("hermes-protocol.jsonl"),
    }
}

async fn run_acceptance(root: &Path, workspace: &Path, hermes: &HermesFixturePaths) -> Result<()> {
    let db = setup_database(root).await?;
    let (session, alpha, beta) = setup_chat_members(&db, workspace).await?;

    verify_free_chat(&db, &session, &alpha, &beta, hermes).await?;
    verify_workflow(&db, &session, &alpha, &beta, hermes).await?;
    Ok(())
}

async fn setup_database(root: &Path) -> Result<DBService> {
    let database_path = root.join("qa.sqlite");
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .context("connect QA database")?;
    sqlx::migrate!("../db/migrations")
        .run(&pool)
        .await
        .context("run QA database migrations")?;
    Ok(DBService { pool })
}

async fn setup_chat_members(
    db: &DBService,
    workspace: &Path,
) -> Result<(ChatSession, HermesMember, HermesMember)> {
    let session = ChatSession::create(
        &db.pool,
        &CreateChatSession {
            title: Some("ACP backend QA".to_string()),
            workspace_path: Some(workspace.to_string_lossy().into_owned()),
            project_id: None,
            worktree_mode: Some(ChatSessionWorktreeMode::Disabled),
        },
        Uuid::new_v4(),
    )
    .await?;
    let alpha_agent = ChatAgent::create(
        &db.pool,
        &CreateChatAgent {
            name: "HermesAlpha".to_string(),
            runner_type: "HERMES".to_string(),
            system_prompt: Some("Use the generic ACP contract.".to_string()),
            tools_enabled: Some(serde_json::json!({
                "mcpServers": {
                    "alpha": true,
                    "beta": false
                }
            })),
            model_name: None,
            owner_project_id: None,
        },
        Uuid::new_v4(),
    )
    .await?;
    let alpha_session_agent = create_chat_session_agent(
        &db.pool,
        &session,
        &alpha_agent,
        workspace,
        executors::executors::acp::AcpApprovalMode::AutoAllow,
        Uuid::new_v4(),
    )
    .await?;
    let beta_agent = ChatAgent::create(
        &db.pool,
        &CreateChatAgent {
            name: "HermesBeta".to_string(),
            runner_type: "HERMES".to_string(),
            system_prompt: Some("Use the generic ACP contract.".to_string()),
            tools_enabled: Some(serde_json::json!({
                "mcpServers": {
                    "alpha": false,
                    "beta": true
                }
            })),
            model_name: None,
            owner_project_id: None,
        },
        Uuid::new_v4(),
    )
    .await?;
    let beta_session_agent = create_chat_session_agent(
        &db.pool,
        &session,
        &beta_agent,
        workspace,
        executors::executors::acp::AcpApprovalMode::AutoAllow,
        Uuid::new_v4(),
    )
    .await?;
    sqlx::query(
        "UPDATE chat_sessions SET lead_agent_id = ?2, lead_session_agent_id = ?3 WHERE id = ?1",
    )
    .bind(session.id)
    .bind(alpha_agent.id)
    .bind(alpha_session_agent.id)
    .execute(&db.pool)
    .await?;
    let session = ChatSession::find_by_id(&db.pool, session.id)
        .await?
        .context("reload QA session")?;
    Ok((
        session,
        HermesMember {
            agent: alpha_agent,
            session_agent: alpha_session_agent,
            allowed_mcp: "alpha",
        },
        HermesMember {
            agent: beta_agent,
            session_agent: beta_session_agent,
            allowed_mcp: "beta",
        },
    ))
}

async fn create_chat_session_agent(
    pool: &SqlitePool,
    session: &ChatSession,
    agent: &ChatAgent,
    workspace: &Path,
    approval_mode: executors::executors::acp::AcpApprovalMode,
    id: Uuid,
) -> Result<ChatSessionAgent> {
    Ok(ChatSessionAgent::create(
        pool,
        &CreateChatSessionAgent {
            session_id: session.id,
            agent_id: agent.id,
            member_name: Some(agent.name.clone()),
            workspace_path: Some(workspace.to_string_lossy().into_owned()),
            allowed_skill_ids: Vec::new(),
            project_member_id: None,
            execution_config: MemberExecutionConfig {
                acp: Some(executors::executors::acp::AcpExecutionOptions {
                    approval_mode: Some(approval_mode),
                    ..Default::default()
                }),
                ..Default::default()
            },
        },
        id,
    )
    .await?)
}

async fn configure_approval_mode(
    db: &DBService,
    member: &HermesMember,
    approval_mode: executors::executors::acp::AcpApprovalMode,
) -> Result<HermesMember> {
    let session_agent = ChatSessionAgent::update_execution_config_for_next_run(
        &db.pool,
        member.session_agent.id,
        None,
        MemberExecutionConfig {
            acp: Some(executors::executors::acp::AcpExecutionOptions {
                approval_mode: Some(approval_mode),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;
    Ok(HermesMember {
        agent: member.agent.clone(),
        session_agent,
        allowed_mcp: member.allowed_mcp,
    })
}

async fn verify_free_chat(
    db: &DBService,
    session: &ChatSession,
    alpha: &HermesMember,
    beta: &HermesMember,
    hermes: &HermesFixturePaths,
) -> Result<()> {
    let runner = ChatRunner::new(db.clone());
    let allow = configure_approval_mode(
        db,
        alpha,
        executors::executors::acp::AcpApprovalMode::AutoAllow,
    )
    .await?;
    let (first_run, persisted_session_id) = run_free_chat_prompt(
        db,
        session,
        &runner,
        &allow,
        "free-allow",
        true,
        "allowed",
        true,
    )
    .await?;
    let (second_run, _) = run_free_chat_prompt(
        db,
        session,
        &runner,
        &allow,
        "free-followup",
        false,
        "allowed",
        true,
    )
    .await?;
    ensure!(
        second_run.run_index > first_run.run_index,
        "Free Chat follow-up did not create a later run"
    );
    let after_follow_up = ChatSessionAgent::find_by_id(&db.pool, allow.session_agent.id)
        .await?
        .context("Free Chat member missing after follow-up")?;
    ensure!(
        after_follow_up.agent_session_id.as_deref() == Some(persisted_session_id.as_str()),
        "Free Chat follow-up changed the persisted ACP session ID"
    );
    assert_session_resume_for_tag(hermes, "free-followup", &persisted_session_id)?;
    assert_mcp_policy_for_tag(hermes, "free-allow", allow.allowed_mcp)?;
    assert_mcp_policy_for_tag(hermes, "free-followup", allow.allowed_mcp)?;
    assert_permission_log_case(hermes, "free-allow", &persisted_session_id, "allowed")?;

    let reject = configure_approval_mode(
        db,
        &allow,
        executors::executors::acp::AcpApprovalMode::AutoReject,
    )
    .await?;
    let (_, reject_session_id) = run_free_chat_prompt(
        db,
        session,
        &runner,
        &reject,
        "free-reject",
        true,
        "rejected",
        false,
    )
    .await?;
    assert_permission_log_case(hermes, "free-reject", &reject_session_id, "rejected")?;

    let ask = configure_approval_mode(db, &reject, executors::executors::acp::AcpApprovalMode::Ask)
        .await?;
    let approval_task = spawn_approval_resolver(
        db.pool.clone(),
        session.id,
        ask.session_agent.id,
        "allow-once",
    );
    let (ask_run, ask_session_id) = run_free_chat_prompt(
        db, session, &runner, &ask, "free-ask", true, "allowed", true,
    )
    .await?;
    let approval_request_id = approval_task.await??;
    assert_approval_request(
        db,
        approval_request_id,
        ask_run.id,
        ask.session_agent.id,
        None,
        "allow-once",
    )
    .await?;
    assert_permission_log_case(hermes, "free-ask", &ask_session_id, "allowed")?;

    let beta = configure_approval_mode(
        db,
        beta,
        executors::executors::acp::AcpApprovalMode::AutoAllow,
    )
    .await?;
    let (_, beta_session_id) = run_free_chat_prompt(
        db,
        session,
        &runner,
        &beta,
        "free-mcp-beta",
        false,
        "allowed",
        true,
    )
    .await?;
    assert_mcp_policy_for_tag(hermes, "free-mcp-beta", beta.allowed_mcp)?;
    ensure!(
        beta.allowed_mcp != allow.allowed_mcp,
        "Free Chat QA members must use distinct MCP allowlists"
    );
    ensure!(
        !beta_session_id.is_empty(),
        "Free Chat beta member session ID is empty"
    );

    let cancel_message = chat::create_message(
        &db.pool,
        session.id,
        ChatSenderType::User,
        None,
        format!(
            "@{} [qa-tag:free-cancel] [qa:sleep] cancellation",
            beta.agent.name
        ),
        None,
    )
    .await?;
    runner.handle_message(session, &cancel_message).await;
    wait_for_agent_state(&db.pool, beta.session_agent.id, true).await?;
    let cancel_session_id = ChatSessionAgent::find_by_id(&db.pool, beta.session_agent.id)
        .await?
        .and_then(|member| member.agent_session_id)
        .context("Free Chat cancellation did not persist an ACP session ID")?;
    runner.stop_agent(session.id, beta.session_agent.id).await?;
    let stopped = wait_for_agent_state(&db.pool, beta.session_agent.id, false).await?;
    ensure!(
        matches!(
            stopped.state,
            ChatSessionAgentState::Idle | ChatSessionAgentState::Dead
        ),
        "Free Chat cancellation left the member active"
    );
    let cancel_run = ChatRun::find_latest_for_session_agent(&db.pool, beta.session_agent.id)
        .await?
        .context("Free Chat cancellation run record missing")?;
    assert_cancelled_run(&cancel_run)?;
    let messages_after_cancel = ChatMessage::find_by_session_id(&db.pool, session.id, None).await?;
    ensure!(
        !messages_after_cancel.iter().any(|message| {
            message.sender_type == ChatSenderType::Agent && message.content.contains("free-cancel")
        }),
        "Free Chat cancellation produced a post-cancel assistant output"
    );
    assert_session_cancel(hermes, &cancel_session_id)?;
    Ok(())
}

async fn verify_workflow(
    db: &DBService,
    session: &ChatSession,
    alpha: &HermesMember,
    beta: &HermesMember,
    hermes: &HermesFixturePaths,
) -> Result<()> {
    let runner = ChatRunner::new(db.clone());
    let allow = configure_approval_mode(
        db,
        alpha,
        executors::executors::acp::AcpApprovalMode::AutoAllow,
    )
    .await?;
    let (workflow_session, step) =
        create_workflow_fixture(db, session, &allow.session_agent).await?;
    let first = run_workflow_step_agent_prompt(
        db,
        &runner,
        session,
        &allow.agent,
        &allow.session_agent,
        Some(&workflow_session),
        "[qa-tag:workflow-allow] [qa:approval] workflow worker",
        &step,
    )
    .await?;
    ensure!(
        first.run_id.is_some(),
        "Workflow run record was not created"
    );
    let first_run_id = first.run_id.context("Workflow initial run ID missing")?;
    assert_workflow_output(&first.output, "workflow-allow", "alpha", "allowed", true)?;
    assert_workflow_usage(&first)?;
    let persisted = WorkflowAgentSession::find_by_id(&db.pool, workflow_session.id)
        .await?
        .context("reload workflow agent session")?;
    let workflow_acp_session = persisted
        .agent_session_id
        .clone()
        .context("Workflow did not persist ACP session ID")?;

    let follow_up = run_workflow_step_agent_follow_up(
        db,
        &runner,
        session,
        &allow.agent,
        &allow.session_agent,
        &persisted,
        "[qa-tag:workflow-followup] workflow retry/resume",
        &step,
    )
    .await?;
    assert_workflow_output(
        &follow_up.output,
        "workflow-followup",
        "alpha",
        "allowed",
        true,
    )?;
    let follow_up_run_id = follow_up
        .run_id
        .context("Workflow follow-up run ID missing")?;
    assert_workflow_usage(&follow_up)?;
    let after_follow_up = WorkflowAgentSession::find_by_id(&db.pool, workflow_session.id)
        .await?
        .context("reload workflow session after follow-up")?;
    ensure!(
        after_follow_up.agent_session_id.as_deref() == Some(workflow_acp_session.as_str()),
        "Workflow retry/resume changed the ACP session ID"
    );
    assert_session_resume_for_tag(hermes, "workflow-followup", &workflow_acp_session)?;
    assert_mcp_policy_for_tag(hermes, "workflow-allow", allow.allowed_mcp)?;
    assert_mcp_policy_for_tag(hermes, "workflow-followup", allow.allowed_mcp)?;
    assert_permission_log_case(hermes, "workflow-allow", &workflow_acp_session, "allowed")?;
    assert_workflow_transcript(
        db,
        &workflow_session,
        &step,
        &["workflow-allow", "workflow-followup"],
        "allowed",
        &[first_run_id, follow_up_run_id],
    )
    .await?;

    let reject = configure_approval_mode(
        db,
        &allow,
        executors::executors::acp::AcpApprovalMode::AutoReject,
    )
    .await?;
    let (reject_session, reject_step) =
        create_workflow_fixture(db, session, &reject.session_agent).await?;
    let rejected = run_workflow_step_agent_prompt(
        db,
        &runner,
        session,
        &reject.agent,
        &reject.session_agent,
        Some(&reject_session),
        "[qa-tag:workflow-reject] [qa:approval] workflow reject",
        &reject_step,
    )
    .await?;
    assert_workflow_output(
        &rejected.output,
        "workflow-reject",
        "alpha",
        "rejected",
        false,
    )?;
    assert_workflow_usage(&rejected)?;
    let rejected_run_id = rejected.run_id.context("Workflow reject run ID missing")?;
    let reject_id = WorkflowAgentSession::find_by_id(&db.pool, reject_session.id)
        .await?
        .and_then(|item| item.agent_session_id)
        .context("Workflow reject did not persist ACP session ID")?;
    assert_permission_log_case(hermes, "workflow-reject", &reject_id, "rejected")?;
    assert_workflow_transcript(
        db,
        &reject_session,
        &reject_step,
        &["workflow-reject"],
        "rejected",
        &[rejected_run_id],
    )
    .await?;

    let ask = configure_approval_mode(db, &reject, executors::executors::acp::AcpApprovalMode::Ask)
        .await?;
    let (ask_session, ask_step) = create_workflow_fixture(db, session, &ask.session_agent).await?;
    let approval_task = spawn_approval_resolver(
        db.pool.clone(),
        session.id,
        ask.session_agent.id,
        "allow-once",
    );
    let asked = run_workflow_step_agent_prompt(
        db,
        &runner,
        session,
        &ask.agent,
        &ask.session_agent,
        Some(&ask_session),
        "[qa-tag:workflow-ask] [qa:approval] workflow ask",
        &ask_step,
    )
    .await?;
    let approval_request_id = approval_task.await??;
    let asked_run_id = asked.run_id.context("Workflow Ask run ID missing")?;
    assert_approval_request(
        db,
        approval_request_id,
        asked_run_id,
        ask.session_agent.id,
        Some(ask_step.id),
        "allow-once",
    )
    .await?;
    assert_workflow_output(&asked.output, "workflow-ask", "alpha", "allowed", true)?;
    assert_workflow_usage(&asked)?;
    let ask_id = WorkflowAgentSession::find_by_id(&db.pool, ask_session.id)
        .await?
        .and_then(|item| item.agent_session_id)
        .context("Workflow Ask did not persist ACP session ID")?;
    assert_permission_log_case(hermes, "workflow-ask", &ask_id, "allowed")?;
    assert_workflow_transcript(
        db,
        &ask_session,
        &ask_step,
        &["workflow-ask"],
        "allowed",
        &[asked_run_id],
    )
    .await?;

    let beta = configure_approval_mode(
        db,
        beta,
        executors::executors::acp::AcpApprovalMode::AutoAllow,
    )
    .await?;
    let (beta_session, beta_step) =
        create_workflow_fixture(db, session, &beta.session_agent).await?;
    let beta_result = run_workflow_step_agent_prompt(
        db,
        &runner,
        session,
        &beta.agent,
        &beta.session_agent,
        Some(&beta_session),
        "[qa-tag:workflow-mcp-beta] workflow member switch",
        &beta_step,
    )
    .await?;
    assert_workflow_output(
        &beta_result.output,
        "workflow-mcp-beta",
        "beta",
        "allowed",
        true,
    )?;
    ensure!(
        !beta_result.output.contains("mcp=alpha"),
        "Workflow beta member received alpha MCP policy"
    );
    assert_mcp_policy_for_tag(hermes, "workflow-mcp-beta", beta.allowed_mcp)?;

    let interrupt_db = db.clone();
    let interrupt_runner = runner.clone();
    let interrupt_session = session.clone();
    let interrupt_agent = beta.agent.clone();
    let interrupt_session_agent = beta.session_agent.clone();
    let (interrupt_workflow_session, interrupt_step) =
        create_workflow_fixture(db, session, &beta.session_agent).await?;
    let interrupt_workflow_session_for_task = interrupt_workflow_session.clone();
    let interrupt_step_for_task = interrupt_step.clone();
    let interrupt_task = tokio::spawn(async move {
        run_workflow_step_agent_prompt(
            &interrupt_db,
            &interrupt_runner,
            &interrupt_session,
            &interrupt_agent,
            &interrupt_session_agent,
            Some(&interrupt_workflow_session_for_task),
            "[qa-tag:workflow-cancel] [qa:sleep] workflow interrupt",
            &interrupt_step_for_task,
        )
        .await
    });
    sleep(Duration::from_millis(500)).await;
    let cancel_session_id =
        WorkflowAgentSession::find_by_id(&db.pool, interrupt_workflow_session.id)
            .await?
            .and_then(|item| item.agent_session_id)
            .context("Workflow cancellation did not persist ACP session ID")?;
    cancel_running_step(interrupt_step.id, 0);
    let interrupted = timeout(Duration::from_secs(10), interrupt_task)
        .await
        .context("Workflow interrupt timed out")??;
    ensure!(
        matches!(interrupted, Err(WorkflowRuntimeError::Interrupted(_))),
        "Workflow interrupt did not return the typed interrupted error"
    );
    let cancel_run = ChatRun::find_latest_for_session_agent(&db.pool, beta.session_agent.id)
        .await?
        .context("Workflow cancellation run record missing")?;
    assert_cancelled_run(&cancel_run)?;
    let cancel_run_id = cancel_run.id.to_string();
    let cancel_transcripts = WorkflowTranscript::find_by_step(&db.pool, interrupt_step.id).await?;
    ensure!(
        !cancel_transcripts
            .iter()
            .any(|entry| entry.content.contains("workflow-cancel")),
        "Workflow cancellation persisted a post-cancel assistant output"
    );
    ensure!(
        cancel_transcripts.iter().any(|entry| {
            entry.entry_type == "error"
                && entry
                    .meta_json
                    .as_deref()
                    .and_then(|meta| serde_json::from_str::<serde_json::Value>(meta).ok())
                    .is_some_and(|meta| {
                        meta.get("outcome").and_then(serde_json::Value::as_str)
                            == Some("interrupted")
                            && meta.get("run_id").and_then(serde_json::Value::as_str)
                                == Some(cancel_run_id.as_str())
                    })
        }),
        "Workflow cancellation did not persist its interrupted terminal event in the step transcript"
    );
    assert_session_cancel(hermes, &cancel_session_id)?;
    Ok(())
}

async fn run_free_chat_prompt(
    db: &DBService,
    session: &ChatSession,
    runner: &ChatRunner,
    member: &HermesMember,
    tag: &str,
    requires_approval: bool,
    expected_permission: &str,
    expect_echo: bool,
) -> Result<(ChatRun, String)> {
    let mut events = runner.subscribe(session.id);
    let approval = if requires_approval {
        " [qa:approval]"
    } else {
        ""
    };
    let message = chat::create_message(
        &db.pool,
        session.id,
        ChatSenderType::User,
        None,
        format!(
            "@{} [qa-tag:{tag}]{approval} free chat {tag}",
            member.agent.name
        ),
        None,
    )
    .await?;
    runner.handle_message(session, &message).await;
    wait_for_chat_run(&mut events, member.session_agent.id).await?;
    let after = wait_for_agent_state(&db.pool, member.session_agent.id, false).await?;
    let persisted_session_id = after
        .agent_session_id
        .clone()
        .context("Free Chat did not persist ACP session ID")?;
    let run = ChatRun::find_latest_for_session_agent(&db.pool, member.session_agent.id)
        .await?
        .context("Free Chat run record missing")?;
    assert_run_usage(&run, 36)?;
    let messages = ChatMessage::find_by_session_id(&db.pool, session.id, None).await?;
    let output = messages
        .iter()
        .filter(|message| message.sender_type == ChatSenderType::Agent)
        .find(|message| message.content.contains(&format!("tag={tag}")))
        .map(|message| message.content.as_str())
        .context("Free Chat assistant output for QA tag missing")?;
    ensure!(
        output.contains(&format!("mcp={}", member.allowed_mcp))
            && !output.contains("mcp=alpha,beta")
            && !output.contains("mcp=beta,alpha"),
        "Free Chat output did not carry only the member MCP server: {output}"
    );
    ensure!(
        output.contains(&format!("permission={expected_permission}")),
        "Free Chat output did not carry permission={expected_permission}: {output}"
    );
    ensure!(
        output.contains("echo:") == expect_echo,
        "Free Chat echo expectation for {tag} was {expect_echo}: {output}"
    );
    Ok((run, persisted_session_id))
}

fn spawn_approval_resolver(
    pool: SqlitePool,
    session_id: Uuid,
    session_agent_id: Uuid,
    option_id: &'static str,
) -> tokio::task::JoinHandle<Result<Uuid>> {
    tokio::spawn(async move {
        let mut events = ExecutorApprovalBridge::subscribe(session_id);
        timeout(Duration::from_secs(20), async move {
            loop {
                let event = events
                    .recv()
                    .await
                    .context("approval event stream closed before Ask request")?;
                if let ExecutorApprovalEvent::ExecutorApprovalRequested { request, .. } = event
                    && request.session_agent_id == session_agent_id
                {
                    let request_id = request.id;
                    let resolved = ExecutorApprovalBridge::resolve(
                        &pool,
                        session_id,
                        request_id,
                        option_id,
                        "hermes-acp-qa",
                    )
                    .await?;
                    ensure!(
                        resolved.is_some(),
                        "Ask approval request disappeared before QA resolution"
                    );
                    return Ok::<_, anyhow::Error>(request_id);
                }
            }
        })
        .await
        .context("timed out waiting for cross-layer Ask approval")?
    })
}

async fn assert_approval_request(
    db: &DBService,
    request_id: Uuid,
    run_id: Uuid,
    session_agent_id: Uuid,
    workflow_step_id: Option<Uuid>,
    option_id: &str,
) -> Result<()> {
    let request = ChatExecutorApprovalRequest::find_by_id(&db.pool, request_id)
        .await?
        .context("cross-layer approval record missing")?;
    ensure!(
        request.run_id == run_id,
        "approval record is associated with another run"
    );
    ensure!(
        request.session_agent_id == session_agent_id,
        "approval record is associated with another member"
    );
    ensure!(
        request.workflow_step_id == workflow_step_id,
        "approval record workflow step association is incorrect"
    );
    ensure!(
        request.status == ChatExecutorApprovalStatus::Selected
            && request.selected_option_id.as_deref() == Some(option_id),
        "approval record did not persist selected option {option_id}: {request:?}"
    );
    Ok(())
}

fn read_protocol_events(hermes: &HermesFixturePaths) -> Result<Vec<HermesProtocolEvent>> {
    let log = fs::read_to_string(&hermes.protocol_log).context("Hermes protocol log missing")?;
    let events = log
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<HermesProtocolEvent>(line)
                .with_context(|| format!("invalid structured Hermes protocol event: {line}"))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        events.iter().all(|event| !event.event.is_empty()),
        "Hermes protocol events must carry a structured event kind"
    );
    Ok(events)
}

fn assert_session_resume_for_tag(
    hermes: &HermesFixturePaths,
    prompt_tag: &str,
    expected_session_id: &str,
) -> Result<()> {
    let events = read_protocol_events(hermes)?;
    let prompt_index = events
        .iter()
        .position(|event| {
            event.method.as_deref() == Some("session/prompt")
                && event.prompt_tag.as_deref() == Some(prompt_tag)
        })
        .with_context(|| format!("no structured session/prompt event for {prompt_tag}"))?;
    let prompt = &events[prompt_index];
    ensure!(
        prompt.session_id.as_deref() == Some(expected_session_id),
        "session/prompt for {prompt_tag} used {:?}, expected persisted ID {expected_session_id}",
        prompt.session_id
    );
    let resume = events[..prompt_index]
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.method.as_deref(),
                Some("session/resume") | Some("session/load")
            )
        })
        .with_context(|| format!("no session/resume or session/load before {prompt_tag}"))?;
    ensure!(
        resume.session_id.as_deref() == Some(expected_session_id),
        "session resume before {prompt_tag} used {:?}, expected persisted ID {expected_session_id}",
        resume.session_id
    );
    Ok(())
}

fn assert_mcp_policy_for_tag(
    hermes: &HermesFixturePaths,
    prompt_tag: &str,
    expected_server: &str,
) -> Result<()> {
    let events = read_protocol_events(hermes)?;
    let prompt_index = events
        .iter()
        .position(|event| {
            event.method.as_deref() == Some("session/prompt")
                && event.prompt_tag.as_deref() == Some(prompt_tag)
        })
        .with_context(|| format!("no structured MCP prompt event for {prompt_tag}"))?;
    let event = events[..prompt_index]
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.method.as_deref(),
                Some("session/new") | Some("session/resume") | Some("session/load")
            )
        })
        .with_context(|| format!("no session bootstrap event before {prompt_tag}"))?;
    let servers = event.mcp_servers.as_deref().unwrap_or_default();
    ensure!(
        servers.len() == 1 && servers[0] == expected_server,
        "MCP policy for {prompt_tag} was {:?}, expected only {expected_server}; events: {:?}",
        event.mcp_servers,
        events
    );
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct HermesPermissionEvent {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "promptTag")]
    prompt_tag: String,
    decision: String,
}

fn assert_permission_log_case(
    hermes: &HermesFixturePaths,
    prompt_tag: &str,
    expected_session_id: &str,
    expected_decision: &str,
) -> Result<()> {
    let log = fs::read_to_string(&hermes.permission_log)
        .context("Hermes permission log missing for cross-layer case")?;
    let event = log
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<HermesPermissionEvent>)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .find(|event| event.prompt_tag == prompt_tag && event.session_id == expected_session_id)
        .with_context(|| format!("permission record for {prompt_tag} is missing"))?;
    ensure!(
        event.decision == expected_decision,
        "permission record for {prompt_tag} was {}, expected {expected_decision}",
        event.decision
    );
    Ok(())
}

fn assert_session_cancel(hermes: &HermesFixturePaths, expected_session_id: &str) -> Result<()> {
    let events = read_protocol_events(hermes)?;
    ensure!(
        events.iter().any(|event| {
            event.method.as_deref() == Some("session/cancel")
                && event.session_id.as_deref() == Some(expected_session_id)
        }),
        "Hermes protocol log has no required session/cancel for {expected_session_id}"
    );
    Ok(())
}

fn assert_cancelled_run(run: &ChatRun) -> Result<()> {
    let summary: ChatRunRetentionSummary = serde_json::from_str(
        run.retention_summary_json
            .as_deref()
            .context("cancelled run retention summary missing")?,
    )?;
    let error = summary
        .error_summary
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if error.contains("interrupt") || error.contains("cancel") || error.contains("stop") {
        return Ok(());
    }
    ensure!(
        summary.assistant_excerpt.is_none(),
        "cancelled run persisted assistant output instead of a stopped terminal summary: {summary:?}"
    );
    ensure!(
        summary
            .token_usage
            .as_ref()
            .is_some_and(|usage| usage.is_estimated),
        "cancelled run did not retain the stopped-run usage marker: {summary:?}"
    );
    Ok(())
}

fn assert_workflow_output(
    output: &str,
    tag: &str,
    expected_mcp: &str,
    expected_permission: &str,
    expect_echo: bool,
) -> Result<()> {
    ensure!(
        output.contains(&format!("tag={tag}")),
        "workflow output lost QA tag {tag}"
    );
    ensure!(
        output.contains(&format!("mcp={expected_mcp}")) && !output.contains("mcp=alpha,beta"),
        "workflow output did not carry only MCP {expected_mcp}: {output}"
    );
    ensure!(
        output.contains(&format!("permission={expected_permission}")),
        "workflow output did not carry permission={expected_permission}: {output}"
    );
    ensure!(
        output.contains("echo:") == expect_echo,
        "workflow echo expectation for {tag} was {expect_echo}: {output}"
    );
    Ok(())
}

fn assert_workflow_usage(
    output: &services::services::workflow_runtime::WorkflowAgentRunOutput,
) -> Result<()> {
    let usage = output
        .token_usage
        .as_ref()
        .context("Workflow ACP token usage missing")?;
    ensure!(
        usage.total_tokens == 36
            && usage.input_tokens == Some(12)
            && usage.output_tokens == Some(24)
            && !usage.is_estimated,
        "Workflow ACP usage was not the exact non-estimated fixture usage: {usage:?}"
    );
    Ok(())
}

async fn assert_workflow_transcript(
    db: &DBService,
    workflow_session: &WorkflowAgentSession,
    step: &WorkflowStep,
    tags: &[&str],
    expected_permission: &str,
    expected_run_ids: &[Uuid],
) -> Result<()> {
    let transcripts = WorkflowTranscript::find_by_step(&db.pool, step.id).await?;
    ensure!(
        !transcripts.is_empty(),
        "workflow transcript for QA step is empty"
    );
    ensure!(
        transcripts.iter().all(|entry| {
            entry.step_id == Some(step.id)
                && entry.workflow_agent_session_id == Some(workflow_session.id)
        }),
        "workflow transcript contains an entry from another step or run"
    );
    let assistant = transcripts.iter().find(|entry| {
        entry.sender_type == "agent"
            && entry.entry_type == "message"
            && tags
                .iter()
                .any(|tag| entry.content.contains(&format!("tag={tag}")))
            && entry
                .content
                .contains(&format!("permission={expected_permission}"))
    });
    ensure!(
        assistant.is_some(),
        "workflow transcript did not persist this Hermes assistant output and permission result: {:?}",
        transcripts
            .iter()
            .map(|entry| (
                &entry.sender_type,
                &entry.entry_type,
                &entry.content,
                &entry.meta_json
            ))
            .collect::<Vec<_>>()
    );
    ensure!(
        transcripts
            .iter()
            .any(|entry| entry.content.contains("Tool: bash")),
        "workflow transcript for this step did not persist the associated tool event"
    );
    ensure!(
        transcripts.iter().any(|entry| {
            entry
                .meta_json
                .as_deref()
                .is_some_and(|meta| meta.contains("workflow_runtime_stream"))
        }),
        "workflow transcript did not retain the runtime stream association"
    );
    let terminal_entries = transcripts
        .iter()
        .filter_map(|entry| {
            let meta = entry.meta_json.as_deref()?;
            let value = serde_json::from_str::<serde_json::Value>(meta).ok()?;
            (value.get("source").and_then(serde_json::Value::as_str)
                == Some("workflow_runtime_terminal"))
            .then_some(value)
        })
        .collect::<Vec<_>>();
    for run_id in expected_run_ids {
        ensure!(
            terminal_entries.iter().any(|meta| {
                meta.get("run_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| Uuid::parse_str(value).ok())
                    == Some(*run_id)
                    && meta.get("token_usage").is_some()
            }),
            "workflow transcript did not persist terminal usage for run {run_id}"
        );
    }
    Ok(())
}

async fn create_workflow_fixture(
    db: &DBService,
    session: &ChatSession,
    session_agent: &ChatSessionAgent,
) -> Result<(WorkflowAgentSession, WorkflowStep)> {
    let plan_json = r#"{"nodes":[],"edges":[],"loops":[]}"#.to_string();
    let plan = WorkflowPlan::create(
        &db.pool,
        &CreateWorkflowPlan {
            session_id: session.id,
            source_message_id: None,
            created_by_session_agent_id: Some(session_agent.id),
            title: "ACP QA workflow".to_string(),
            summary_text: None,
            plan_json: plan_json.clone(),
            plan_schema_version: 1,
            plan_hash: "qa-plan".to_string(),
            validation_status: WorkflowValidationStatus::Valid,
            validation_errors_json: None,
        },
        Uuid::new_v4(),
    )
    .await?;
    let revision = WorkflowPlanRevision::create(
        &db.pool,
        &CreateWorkflowPlanRevision {
            plan_id: plan.id,
            revision_no: 1,
            edited_by: WorkflowRevisionEditor::System,
            editor_session_agent_id: Some(session_agent.id),
            reason: Some("QA fixture".to_string()),
            plan_json,
            plan_hash: "qa-plan".to_string(),
            validation_status: WorkflowValidationStatus::Valid,
            validation_errors_json: None,
        },
        Uuid::new_v4(),
    )
    .await?;
    let execution = WorkflowExecution::create(
        &db.pool,
        &CreateWorkflowExecution {
            session_id: session.id,
            plan_id: plan.id,
            active_revision_id: Some(revision.id),
            lead_session_agent_id: Some(session_agent.id),
            title: "ACP QA execution".to_string(),
        },
        Uuid::new_v4(),
    )
    .await?;
    let round = WorkflowRound::create(
        &db.pool,
        &CreateWorkflowRound {
            execution_id: execution.id,
            round_index: 1,
            source_revision_id: Some(revision.id),
        },
        Uuid::new_v4(),
    )
    .await?;
    sqlx::query("UPDATE chat_workflow_executions SET active_round_id = ?2 WHERE id = ?1")
        .bind(execution.id)
        .bind(round.id)
        .execute(&db.pool)
        .await?;
    let workflow_session = WorkflowAgentSession::create(
        &db.pool,
        &CreateWorkflowAgentSession {
            workflow_execution_id: execution.id,
            session_agent_id: session_agent.id,
            role: WorkflowAgentSessionRole::Worker,
        },
        Uuid::new_v4(),
    )
    .await?;
    let step = WorkflowStep::create(
        &db.pool,
        &CreateWorkflowStep {
            execution_id: execution.id,
            round_id: round.id,
            compiled_revision_id: Some(revision.id),
            step_key: "qa-worker".to_string(),
            step_type: WorkflowStepType::Task,
            title: "ACP QA worker".to_string(),
            instructions: "Run the ACP QA fixture".to_string(),
            assigned_workflow_agent_session_id: Some(workflow_session.id),
            max_retry: 2,
            round_index: 1,
            display_order: 0,
            loop_id: None,
            lead_review_required: Some(false),
            user_review_required: Some(false),
            revision_context: None,
        },
        Uuid::new_v4(),
    )
    .await?;
    Ok((workflow_session, step))
}

async fn wait_for_chat_run(
    events: &mut tokio::sync::broadcast::Receiver<ChatStreamEvent>,
    session_agent_id: Uuid,
) -> Result<()> {
    let mut saw_run = false;
    timeout(Duration::from_secs(20), async {
        loop {
            match events.recv().await? {
                ChatStreamEvent::AgentRunStarted {
                    session_agent_id: event_session_agent_id,
                    ..
                } if event_session_agent_id == session_agent_id => {
                    saw_run = true;
                }
                ChatStreamEvent::AgentState {
                    session_agent_id: event_session_agent_id,
                    state: ChatSessionAgentState::Idle,
                    ..
                } if event_session_agent_id == session_agent_id && saw_run => {
                    return Ok::<_, anyhow::Error>(());
                }
                ChatStreamEvent::MentionError { reason, .. } => {
                    anyhow::bail!("Free Chat mention failed: {reason}");
                }
                ChatStreamEvent::ProtocolNotice { code, detail, .. } => {
                    anyhow::bail!("Free Chat protocol failed: {code:?}: {detail:?}");
                }
                ChatStreamEvent::AgentState {
                    state: ChatSessionAgentState::Dead,
                    ..
                } => {
                    anyhow::bail!("Free Chat agent entered dead state");
                }
                _ => {}
            }
        }
    })
    .await
    .context("timed out waiting for Free Chat completion")?
}

async fn wait_for_agent_state(
    pool: &SqlitePool,
    session_agent_id: Uuid,
    active: bool,
) -> Result<ChatSessionAgent> {
    timeout(Duration::from_secs(20), async {
        loop {
            let agent = ChatSessionAgent::find_by_id(pool, session_agent_id)
                .await?
                .context("session agent disappeared")?;
            let is_active = matches!(
                agent.state,
                ChatSessionAgentState::Running | ChatSessionAgentState::Stopping
            );
            if is_active == active {
                return Ok::<_, anyhow::Error>(agent);
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("timed out waiting for session agent state")?
}

fn assert_run_usage(run: &ChatRun, expected: u32) -> Result<()> {
    let summary: ChatRunRetentionSummary = serde_json::from_str(
        run.retention_summary_json
            .as_deref()
            .context("run retention summary missing")?,
    )?;
    ensure!(
        summary.total_tokens == Some(expected),
        "expected {expected} tokens, got {:?}",
        summary.total_tokens
    );
    let usage = summary
        .token_usage
        .as_ref()
        .context("run token usage missing")?;
    ensure!(
        usage.input_tokens == Some(12) && usage.output_tokens == Some(24),
        "expected canonical ACP input/output usage, got {:?}/{:?}",
        usage.input_tokens,
        usage.output_tokens
    );
    ensure!(!usage.is_estimated, "ACP token usage must not be estimated");
    Ok(())
}

fn init_git_repo(path: &Path) {
    run_git(path, &["init"]);
    run_git(path, &["config", "user.email", "qa@openteams.local"]);
    run_git(path, &["config", "user.name", "OpenTeams QA"]);
    std::fs::write(path.join("README.md"), "qa workspace\n").expect("seed QA repository");
    run_git(path, &["add", "README.md"]);
    run_git(path, &["commit", "-m", "qa baseline"]);
}

fn run_git(path: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
}
