#![cfg(feature = "qa-mode")]

use std::{path::Path, process::Command, time::Duration};

use anyhow::{Context, Result, ensure};
use db::{
    DBService,
    models::{
        chat_agent::{ChatAgent, CreateChatAgent},
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

const AGENT_CHILD_ARGUMENT: &str = "--openteams-acp-qa-agent";

fn main() {
    if std::env::args().any(|argument| argument == AGENT_CHILD_ARGUMENT) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build ACP QA child runtime");
        if let Err(error) = runtime.block_on(executors::executors::acp::qa_agent::run_stdio_agent())
        {
            eprintln!("ACP QA child failed: {error}");
            std::process::exit(1);
        }
        return;
    }

    let fixture = TempDir::new().expect("create ACP backend QA fixture");
    let workspace = fixture.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create QA workspace");
    init_git_repo(&workspace);
    let mcp_path = fixture.path().join("mcp.json");
    std::fs::write(
        &mcp_path,
        r#"{
  "mcpServers": {
    "allowed": { "command": "true" },
    "blocked": { "command": "true" }
  }
}"#,
    )
    .expect("write QA MCP configuration");

    let current_exe = std::env::current_exe().expect("resolve QA test executable");
    // The custom test harness is single-threaded and sets fixture-only process
    // configuration before starting Tokio or any worker process.
    unsafe {
        std::env::set_var("OPENTEAMS_ACP_QA_AGENT_COMMAND", current_exe);
        std::env::set_var("OPENTEAMS_ACP_QA_AGENT_ARGUMENT", AGENT_CHILD_ARGUMENT);
        std::env::set_var("OPENTEAMS_ACP_QA_MCP_CONFIG_PATH", &mcp_path);
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build ACP backend QA runtime");
    if let Err(error) = runtime.block_on(run_acceptance(fixture.path(), &workspace)) {
        eprintln!("ACP backend QA acceptance failed: {error:#}");
        std::process::exit(1);
    }
    println!("ACP backend QA acceptance passed");
}

async fn run_acceptance(root: &Path, workspace: &Path) -> Result<()> {
    let db = setup_database(root).await?;
    let (session, agent, session_agent) = setup_chat_member(&db, workspace).await?;

    verify_free_chat(&db, &session, &agent, &session_agent, workspace).await?;
    verify_workflow(&db, &session, &agent, &session_agent).await?;
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

async fn setup_chat_member(
    db: &DBService,
    workspace: &Path,
) -> Result<(ChatSession, ChatAgent, ChatSessionAgent)> {
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
    let agent = ChatAgent::create(
        &db.pool,
        &CreateChatAgent {
            name: "AcpQa".to_string(),
            runner_type: "ACP_QA".to_string(),
            system_prompt: Some("Return the required OpenTeams envelope.".to_string()),
            tools_enabled: Some(serde_json::json!({
                "mcpServers": {
                    "allowed": true,
                    "blocked": false
                }
            })),
            model_name: None,
            owner_project_id: None,
        },
        Uuid::new_v4(),
    )
    .await?;
    let session_agent = ChatSessionAgent::create(
        &db.pool,
        &CreateChatSessionAgent {
            session_id: session.id,
            agent_id: agent.id,
            member_name: Some(agent.name.clone()),
            workspace_path: Some(workspace.to_string_lossy().into_owned()),
            allowed_skill_ids: Vec::new(),
            project_member_id: None,
            execution_config: MemberExecutionConfig::default(),
        },
        Uuid::new_v4(),
    )
    .await?;
    sqlx::query(
        "UPDATE chat_sessions SET lead_agent_id = ?2, lead_session_agent_id = ?3 WHERE id = ?1",
    )
    .bind(session.id)
    .bind(agent.id)
    .bind(session_agent.id)
    .execute(&db.pool)
    .await?;
    let session = ChatSession::find_by_id(&db.pool, session.id)
        .await?
        .context("reload QA session")?;
    Ok((session, agent, session_agent))
}

async fn verify_free_chat(
    db: &DBService,
    session: &ChatSession,
    _agent: &ChatAgent,
    session_agent: &ChatSessionAgent,
    workspace: &Path,
) -> Result<()> {
    let runner = ChatRunner::new(db.clone());
    let mut events = runner.subscribe(session.id);
    let first_message = chat::create_message(
        &db.pool,
        session.id,
        ChatSenderType::User,
        None,
        "@AcpQa [qa:write] [qa:approval]".to_string(),
        None,
    )
    .await?;
    runner.handle_message(session, &first_message).await;
    let changed_files = match wait_for_file_refresh(&mut events).await {
        Ok(files) => files,
        Err(error) => {
            let messages = ChatMessage::find_by_session_id(&db.pool, session.id, None).await?;
            let diagnostic = messages
                .iter()
                .map(|message| message.content.as_str())
                .collect::<Vec<_>>()
                .join(" | ");
            anyhow::bail!("{error:#}; messages={diagnostic}");
        }
    };
    ensure!(
        changed_files.iter().any(|path| path == "qa-changed.txt"),
        "Free Chat did not project qa-changed.txt"
    );
    ensure!(
        workspace.join("qa-changed.txt").is_file(),
        "ACP client write did not reach the session workspace"
    );
    let after_first = wait_for_agent_state(&db.pool, session_agent.id, false).await?;
    let persisted_session_id = after_first
        .agent_session_id
        .clone()
        .context("Free Chat did not persist ACP session ID")?;
    let first_run = ChatRun::find_latest_for_session_agent(&db.pool, session_agent.id)
        .await?
        .context("Free Chat run record missing")?;
    assert_run_usage(&first_run, 37)?;
    let messages = ChatMessage::find_by_session_id(&db.pool, session.id, None).await?;
    ensure!(
        messages.iter().any(|message| {
            message.sender_type == ChatSenderType::Agent
                && message.content.contains("approval=allow-once")
                && message.content.contains("mcp=allowed")
                && !message.content.contains("blocked")
        }),
        "Free Chat output did not prove approval and MCP allowlist"
    );

    let follow_up = chat::create_message(
        &db.pool,
        session.id,
        ChatSenderType::User,
        None,
        "@AcpQa follow-up".to_string(),
        None,
    )
    .await?;
    runner.handle_message(session, &follow_up).await;
    let _ = wait_for_file_refresh(&mut events).await?;
    let after_follow_up = wait_for_agent_state(&db.pool, session_agent.id, false).await?;
    ensure!(
        after_follow_up.agent_session_id.as_deref() == Some(persisted_session_id.as_str()),
        "Free Chat follow-up did not reuse the persisted ACP session"
    );
    let second_run = ChatRun::find_latest_for_session_agent(&db.pool, session_agent.id)
        .await?
        .context("Free Chat follow-up run missing")?;
    ensure!(
        second_run.run_index >= 2,
        "Free Chat follow-up did not create a second run"
    );

    let cancel_message = chat::create_message(
        &db.pool,
        session.id,
        ChatSenderType::User,
        None,
        "@AcpQa [qa:sleep]".to_string(),
        None,
    )
    .await?;
    runner.handle_message(session, &cancel_message).await;
    wait_for_agent_state(&db.pool, session_agent.id, true).await?;
    runner.stop_agent(session.id, session_agent.id).await?;
    let stopped = wait_for_agent_state(&db.pool, session_agent.id, false).await?;
    ensure!(
        matches!(
            stopped.state,
            ChatSessionAgentState::Idle | ChatSessionAgentState::Dead
        ),
        "Free Chat cancellation left the member active"
    );
    Ok(())
}

async fn verify_workflow(
    db: &DBService,
    session: &ChatSession,
    agent: &ChatAgent,
    session_agent: &ChatSessionAgent,
) -> Result<()> {
    let (workflow_session, step) = create_workflow_fixture(db, session, session_agent).await?;
    let runner = ChatRunner::new(db.clone());
    let first = run_workflow_step_agent_prompt(
        db,
        &runner,
        session,
        agent,
        session_agent,
        Some(&workflow_session),
        "[qa:approval] workflow worker",
        &step,
    )
    .await?;
    ensure!(
        first.run_id.is_some(),
        "Workflow run record was not created"
    );
    ensure!(
        first.output.contains("approval=allow-once"),
        "Workflow approval request was not exercised"
    );
    ensure!(
        first.token_usage.as_ref().map(|usage| usage.total_tokens) == Some(37),
        "Workflow usage update was not projected"
    );
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
        agent,
        session_agent,
        &persisted,
        "workflow retry/resume",
        &step,
    )
    .await?;
    ensure!(
        follow_up.output.contains("ACP QA completed"),
        "Workflow follow-up did not complete"
    );
    let after_follow_up = WorkflowAgentSession::find_by_id(&db.pool, workflow_session.id)
        .await?
        .context("reload workflow session after follow-up")?;
    ensure!(
        after_follow_up.agent_session_id.as_deref() == Some(workflow_acp_session.as_str()),
        "Workflow retry/resume changed the ACP session ID"
    );
    let transcripts = WorkflowTranscript::find_by_step(&db.pool, step.id).await?;
    if !transcripts
        .iter()
        .any(|entry| entry.content.contains("QA approval"))
    {
        let diagnostic = transcripts
            .iter()
            .map(|entry| {
                format!(
                    "{}:{}:{}",
                    entry.sender_type, entry.entry_type, entry.content
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        anyhow::bail!("Workflow runtime transcript was not persisted: {diagnostic}");
    }

    let (reviewer_session, review_step) =
        create_workflow_reviewer_fixture(db, session_agent, &step).await?;
    let reviewer = run_workflow_step_agent_prompt(
        db,
        &runner,
        session,
        agent,
        session_agent,
        Some(&reviewer_session),
        "workflow reviewer",
        &review_step,
    )
    .await?;
    ensure!(
        reviewer.output.contains("ACP QA completed"),
        "Workflow reviewer path did not complete"
    );

    let interrupt_db = db.clone();
    let interrupt_runner = runner.clone();
    let interrupt_session = session.clone();
    let interrupt_agent = agent.clone();
    let interrupt_session_agent = session_agent.clone();
    let interrupt_workflow_session = after_follow_up.clone();
    let interrupt_step = step.clone();
    let interrupt_task = tokio::spawn(async move {
        run_workflow_step_agent_prompt(
            &interrupt_db,
            &interrupt_runner,
            &interrupt_session,
            &interrupt_agent,
            &interrupt_session_agent,
            Some(&interrupt_workflow_session),
            "[qa:sleep] workflow interrupt",
            &interrupt_step,
        )
        .await
    });
    sleep(Duration::from_millis(500)).await;
    cancel_running_step(step.id, 0);
    let interrupted = timeout(Duration::from_secs(10), interrupt_task)
        .await
        .context("Workflow interrupt timed out")??;
    ensure!(
        matches!(interrupted, Err(WorkflowRuntimeError::Interrupted(_))),
        "Workflow interrupt did not return the typed interrupted error"
    );
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

async fn create_workflow_reviewer_fixture(
    db: &DBService,
    session_agent: &ChatSessionAgent,
    worker_step: &WorkflowStep,
) -> Result<(WorkflowAgentSession, WorkflowStep)> {
    let workflow_session = WorkflowAgentSession::create(
        &db.pool,
        &CreateWorkflowAgentSession {
            workflow_execution_id: worker_step.execution_id,
            session_agent_id: session_agent.id,
            role: WorkflowAgentSessionRole::Reviewer,
        },
        Uuid::new_v4(),
    )
    .await?;
    let step = WorkflowStep::create(
        &db.pool,
        &CreateWorkflowStep {
            execution_id: worker_step.execution_id,
            round_id: worker_step.round_id,
            compiled_revision_id: worker_step.compiled_revision_id,
            step_key: "qa-reviewer".to_string(),
            step_type: WorkflowStepType::Review,
            title: "ACP QA reviewer".to_string(),
            instructions: "Review the ACP QA worker output".to_string(),
            assigned_workflow_agent_session_id: Some(workflow_session.id),
            max_retry: 1,
            round_index: 1,
            display_order: 1,
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

async fn wait_for_file_refresh(
    events: &mut tokio::sync::broadcast::Receiver<ChatStreamEvent>,
) -> Result<Vec<String>> {
    timeout(Duration::from_secs(20), async {
        loop {
            match events.recv().await? {
                ChatStreamEvent::FileChangeRefresh { changed_files, .. } => {
                    return Ok::<_, anyhow::Error>(
                        changed_files.into_iter().map(|entry| entry.path).collect(),
                    );
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
        usage.input_tokens == Some(30) && usage.output_tokens == Some(7),
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
