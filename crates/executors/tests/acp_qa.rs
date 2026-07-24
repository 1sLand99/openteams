#![cfg(feature = "qa-mode")]

use std::time::Duration;

use executors::{
    env::{ExecutionEnv, RepoContext},
    executors::{
        ExecutorError, StandardCodingAgentExecutor,
        acp::{AcpEvent, AcpQaExecutor, events::AcpRuntimeEvent},
    },
};
use tokio::io::{AsyncBufReadExt, BufReader};

async fn run_turn(
    executor: &AcpQaExecutor,
    workspace: &std::path::Path,
    session_id: Option<&str>,
) -> Vec<AcpEvent> {
    let env = ExecutionEnv::new(
        RepoContext::new(workspace.to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    let mut spawned = match session_id {
        Some(session_id) => executor
            .spawn_follow_up(workspace, "follow-up", session_id, None, &env)
            .await
            .expect("spawn ACP follow-up"),
        None => executor
            .spawn(workspace, "first turn", &env)
            .await
            .expect("spawn ACP turn"),
    };
    let stdout = spawned
        .child
        .inner()
        .stdout
        .take()
        .expect("replacement stdout");
    let mut lines = BufReader::new(stdout).lines();
    let mut events = Vec::new();
    while let Ok(Ok(Some(line))) =
        tokio::time::timeout(Duration::from_secs(5), lines.next_line()).await
    {
        let event = serde_json::from_str::<AcpRuntimeEvent>(&line)
            .expect("typed ACP event")
            .payload;
        let done = matches!(event, AcpEvent::Done(_));
        events.push(event);
        if done {
            break;
        }
    }
    let exit = spawned.exit_signal.take().expect("exit signal");
    tokio::time::timeout(Duration::from_secs(5), exit)
        .await
        .expect("exit signal timeout")
        .expect("exit signal sender");
    events
}

#[tokio::test]
async fn hidden_runner_completes_new_and_native_resume_turns() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let executor = AcpQaExecutor {
        command: env!("CARGO_BIN_EXE_acp-qa-agent").to_string(),
        ..AcpQaExecutor::default()
    };

    let first = run_turn(&executor, &workspace, None).await;
    let session_id = first
        .iter()
        .find_map(|event| match event {
            AcpEvent::SessionStart(session_id) => Some(session_id.clone()),
            _ => None,
        })
        .expect("session id");
    assert!(
        first
            .iter()
            .any(|event| matches!(event, AcpEvent::Message(_)))
    );
    assert!(first.iter().any(|event| matches!(event, AcpEvent::Done(_))));

    let follow_up = run_turn(&executor, &workspace, Some(&session_id)).await;
    assert!(
        follow_up
            .iter()
            .any(|event| { matches!(event, AcpEvent::SessionStart(id) if id == &session_id) })
    );
    assert!(
        follow_up
            .iter()
            .any(|event| matches!(event, AcpEvent::Message(_)))
    );

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn follow_up_without_agent_capability_returns_typed_error() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let executor = AcpQaExecutor {
        command: env!("CARGO_BIN_EXE_acp-qa-agent").to_string(),
        ..AcpQaExecutor::default()
    };
    let mut env = ExecutionEnv::new(
        RepoContext::new(workspace.clone(), Vec::new()),
        false,
        String::new(),
    );
    env.insert("ACP_QA_DISABLE_FOLLOW_UP", "1");

    let error = executor
        .spawn_follow_up(&workspace, "follow-up", "opaque-session", None, &env)
        .await
        .expect_err("follow-up must be rejected");
    assert!(matches!(error, ExecutorError::FollowUpNotSupported(_)));

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}
