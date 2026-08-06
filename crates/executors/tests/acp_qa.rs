#![cfg(feature = "qa-mode")]

use std::time::Duration;

use executors::{
    command::{CmdOverrides, CommandParts},
    env::{ExecutionEnv, RepoContext},
    executors::{
        ExecutorError, ExecutorExitResult, StandardCodingAgentExecutor,
        acp::{
            AcpAgentHarness, AcpApprovalPolicy, AcpClientServicePolicy, AcpConfigOverride,
            AcpConfigSource, AcpConfigValue, AcpEvent, AcpQaExecutor,
            events::AcpRuntimeEvent,
            runtime::{probe_acp_command, probe_acp_command_without_session},
        },
    },
};
use tokio::io::{AsyncBufReadExt, BufReader};

async fn run_turn(
    executor: &AcpQaExecutor,
    workspace: &std::path::Path,
    session_id: Option<&str>,
    env_vars: &[(&str, &str)],
    prompt: &str,
) -> (Vec<AcpEvent>, ExecutorExitResult) {
    let mut env = ExecutionEnv::new(
        RepoContext::new(workspace.to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    for (name, value) in env_vars {
        env.insert(*name, *value);
    }
    let mut spawned = match session_id {
        Some(session_id) => executor
            .spawn_follow_up(workspace, prompt, session_id, None, &env)
            .await
            .expect("spawn ACP follow-up"),
        None => executor
            .spawn(workspace, prompt, &env)
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
    let result = tokio::time::timeout(Duration::from_secs(5), exit)
        .await
        .expect("exit signal timeout")
        .expect("exit signal sender");
    (events, result)
}

async fn read_spawned_turn(
    mut spawned: executors::executors::SpawnedChild,
) -> (Vec<AcpEvent>, ExecutorExitResult) {
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
    let result = tokio::time::timeout(Duration::from_secs(5), exit)
        .await
        .expect("exit signal timeout")
        .expect("exit signal sender");
    (events, result)
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

    let (first, first_exit) = run_turn(&executor, &workspace, None, &[], "first turn").await;
    assert!(matches!(first_exit, ExecutorExitResult::Success));
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

    let (follow_up, follow_up_exit) =
        run_turn(&executor, &workspace, Some(&session_id), &[], "follow-up").await;
    assert!(matches!(follow_up_exit, ExecutorExitResult::Success));
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

#[tokio::test]
async fn configured_authentication_runs_before_session_creation() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let executor = AcpQaExecutor {
        command: env!("CARGO_BIN_EXE_acp-qa-agent").to_string(),
        auth_method_id: Some("qa-auth".to_string()),
        ..AcpQaExecutor::default()
    };

    let (events, exit) = run_turn(
        &executor,
        &workspace,
        None,
        &[("ACP_QA_REQUIRE_AUTH", "1")],
        "authenticated turn",
    )
    .await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AcpEvent::Done(_)))
    );

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn expired_configured_authentication_is_typed() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let executor = AcpQaExecutor {
        command: env!("CARGO_BIN_EXE_acp-qa-agent").to_string(),
        auth_method_id: Some("qa-auth".to_string()),
        ..AcpQaExecutor::default()
    };
    let mut env = ExecutionEnv::new(
        RepoContext::new(workspace.clone(), Vec::new()),
        false,
        String::new(),
    );
    env.insert("ACP_QA_REQUIRE_AUTH", "1");
    env.insert("ACP_QA_EXPIRE_AUTH", "1");

    let error = executor
        .spawn(&workspace, "expired authentication", &env)
        .await
        .expect_err("expired authentication should be required again");
    assert!(matches!(error, ExecutorError::AuthRequired(_)));

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn prompt_error_and_abnormal_exit_are_reported_without_hanging() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let executor = AcpQaExecutor {
        command: env!("CARGO_BIN_EXE_acp-qa-agent").to_string(),
        ..AcpQaExecutor::default()
    };

    let (prompt_error_events, prompt_error_exit) =
        run_turn(&executor, &workspace, None, &[], "[qa:error]").await;
    assert!(matches!(prompt_error_exit, ExecutorExitResult::Failure));
    assert!(
        prompt_error_events
            .iter()
            .any(|event| matches!(event, AcpEvent::Error(_)))
    );

    let (exit_events, abnormal_exit) =
        run_turn(&executor, &workspace, None, &[], "[qa:exit]").await;
    assert!(matches!(
        abnormal_exit,
        ExecutorExitResult::Failure | ExecutorExitResult::FailureWithError(_)
    ));
    assert!(
        exit_events
            .iter()
            .any(|event| matches!(event, AcpEvent::Error(_)))
            || exit_events.is_empty()
    );

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn cancellation_notifies_agent_and_finishes_promptly() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let executor = AcpQaExecutor {
        command: env!("CARGO_BIN_EXE_acp-qa-agent").to_string(),
        ..AcpQaExecutor::default()
    };
    let env = ExecutionEnv::new(
        RepoContext::new(workspace.clone(), Vec::new()),
        false,
        String::new(),
    );
    let mut spawned = executor
        .spawn(&workspace, "[qa:sleep]", &env)
        .await
        .expect("spawn cancellable ACP turn");
    spawned
        .cancel
        .take()
        .expect("ACP cancellation token")
        .cancel();
    let exit = spawned.exit_signal.take().expect("exit signal");
    let result = tokio::time::timeout(Duration::from_secs(3), exit)
        .await
        .expect("cancellation timeout")
        .expect("exit signal sender");
    assert!(matches!(result, ExecutorExitResult::Success));

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn required_authentication_without_configuration_is_typed() {
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
    env.insert("ACP_QA_REQUIRE_AUTH", "1");

    let error = executor
        .spawn(&workspace, "first turn", &env)
        .await
        .expect_err("authentication should be required");
    assert!(matches!(error, ExecutorError::AuthRequired(_)));

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn stable_config_option_is_applied_verified_and_used_for_usage_identity() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let executor = AcpQaExecutor {
        command: env!("CARGO_BIN_EXE_acp-qa-agent").to_string(),
        model: Some("gpt-5.6-luna".to_string()),
        ..AcpQaExecutor::default()
    };

    let (events, exit) = run_turn(
        &executor,
        &workspace,
        None,
        &[("ACP_QA_CONFIG_OPTIONS", "1")],
        "configured turn",
    )
    .await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AcpEvent::ConfigOptions(options)
                if serde_json::to_string(options)
                    .is_ok_and(|json| json.contains("gpt-5.6-luna(openai)"))
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AcpEvent::Message(chunk)
                if serde_json::to_string(chunk)
                    .is_ok_and(|json| json.contains("model=gpt-5.6-luna(openai)"))
        )
    }));
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AcpEvent::TokenUsage(usage)
                if usage.runtime_model_id.as_deref() == Some("gpt-5.6-luna")
        )
    }));

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn required_session_mode_is_enforced_for_new_and_resumed_sessions() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let mut env = ExecutionEnv::new(
        RepoContext::new(workspace.clone(), Vec::new()),
        false,
        String::new(),
    );
    env.insert("ACP_QA_MODE_OPTIONS", "1");

    let unsafe_override = AcpConfigOverride {
        option_id: "mode".to_string(),
        value: AcpConfigValue::ValueId {
            value: "yolo".to_string(),
        },
        label_snapshot: Some("Mode".to_string()),
        category_snapshot: Some("mode".to_string()),
    };
    let harness = AcpAgentHarness::new()
        .with_approval_policy(AcpApprovalPolicy::Ask)
        .with_required_session_mode("mode", "default")
        .with_config_override(&unsafe_override)
        .with_client_services(AcpClientServicePolicy {
            terminal: true,
            ..AcpClientServicePolicy::default()
        });
    let command = || CommandParts::new(env!("CARGO_BIN_EXE_acp-qa-agent").to_string(), Vec::new());

    let first = harness
        .spawn_with_command(
            &workspace,
            "first safe turn".to_string(),
            command(),
            &env,
            &CmdOverrides::default(),
            None,
        )
        .await
        .expect("spawn first safe turn");
    let (first_events, first_exit) = read_spawned_turn(first).await;
    assert!(matches!(first_exit, ExecutorExitResult::Success));
    assert!(first_events.iter().any(|event| {
        matches!(
            event,
            AcpEvent::Message(chunk)
                if serde_json::to_string(chunk)
                    .is_ok_and(|json| json.contains("mode=default"))
        )
    }));
    let session_id = first_events
        .iter()
        .find_map(|event| match event {
            AcpEvent::SessionStart(session_id) => Some(session_id.clone()),
            _ => None,
        })
        .expect("session id");

    let resumed = harness
        .spawn_follow_up_with_command(
            &workspace,
            "resumed safe turn".to_string(),
            &session_id,
            command(),
            &env,
            &CmdOverrides::default(),
            None,
        )
        .await
        .expect("spawn resumed safe turn");
    let (resumed_events, resumed_exit) = read_spawned_turn(resumed).await;
    assert!(matches!(resumed_exit, ExecutorExitResult::Success));
    assert!(resumed_events.iter().any(|event| {
        matches!(
            event,
            AcpEvent::Message(chunk)
                if serde_json::to_string(chunk)
                    .is_ok_and(|json| json.contains("mode=default"))
        )
    }));

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn required_session_mode_rejects_an_unverified_agent_response() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let mut env = ExecutionEnv::new(
        RepoContext::new(workspace.clone(), Vec::new()),
        false,
        String::new(),
    );
    env.insert("ACP_QA_MODE_OPTIONS", "1");
    env.insert("ACP_QA_REFUSE_MODE_SET", "1");
    let harness = AcpAgentHarness::new().with_required_session_mode("mode", "default");

    let error = harness
        .spawn_with_command(
            &workspace,
            "this prompt must not be sent".to_string(),
            CommandParts::new(env!("CARGO_BIN_EXE_acp-qa-agent").to_string(), Vec::new()),
            &env,
            &CmdOverrides::default(),
            None,
        )
        .await
        .expect_err("unverified mode response must fail startup");
    assert!(matches!(error, ExecutorError::Configuration(_)));
    let message = error.to_string();
    assert!(message.contains("ACP config option `mode` requested `default`"));
    assert!(message.contains("Agent activated `yolo`"));
    assert!(!message.contains("I/O error"));
    assert!(!message.contains("ACP startup failed"));

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn capability_probe_discovers_stable_config_options() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let mut env = ExecutionEnv::new(
        RepoContext::new(workspace.clone(), Vec::new()),
        false,
        String::new(),
    );
    env.insert("ACP_QA_CONFIG_OPTIONS", "1");

    let probe = probe_acp_command(
        CommandParts::new(env!("CARGO_BIN_EXE_acp-qa-agent").to_string(), Vec::new()),
        &workspace,
        &env,
        &CmdOverrides::default(),
        None,
    )
    .await
    .expect("probe stable ACP config");

    assert_eq!(probe.config_source, AcpConfigSource::Stable);
    assert_eq!(probe.config_options.len(), 1);
    assert_eq!(probe.config_options[0].id, "session-model");

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}

#[tokio::test]
async fn initialize_only_capability_probe_skips_session_config_options() {
    let workspace = std::env::temp_dir().join(format!("openteams-acp-qa-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&workspace)
        .await
        .expect("create workspace");
    let mut env = ExecutionEnv::new(
        RepoContext::new(workspace.clone(), Vec::new()),
        false,
        String::new(),
    );
    env.insert("ACP_QA_CONFIG_OPTIONS", "1");

    let probe = probe_acp_command_without_session(
        CommandParts::new(env!("CARGO_BIN_EXE_acp-qa-agent").to_string(), Vec::new()),
        &workspace,
        &env,
        &CmdOverrides::default(),
        None,
    )
    .await
    .expect("probe ACP initialization");

    assert_eq!(probe.protocol_version, "1");
    assert_eq!(probe.config_source, AcpConfigSource::None);
    assert!(probe.config_options.is_empty());

    tokio::fs::remove_dir_all(workspace)
        .await
        .expect("remove workspace");
}
