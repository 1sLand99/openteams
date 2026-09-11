#![cfg(unix)]

//! Deterministic Kiro CLI 2.20.1 ACP v1 adapter integration tests.
//!
//! The repository-local fake implements only the standard methods and update
//! shapes observed by the Kiro probe. These tests never execute a real Kiro
//! binary, inspect local Kiro state, or require network access.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use executors::{
    env::{ExecutionEnv, RepoContext},
    executors::{
        ExecutorError, ExecutorExitResult, ExecutorRunCleanup, SpawnedChild,
        StandardCodingAgentExecutor,
        acp::{AcpEvent, events::AcpRuntimeEvent},
        kiro::KiroCli,
    },
    mcp_config::MemberMcpConfig,
    mcp_run::{McpRunContext, PreparedMcpRun},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/kiro_acp/fake_kiro_acp.mjs"
);
const API_KEY: &str = "k3!";
const SHORT_MCP_ENV_SECRETS: [&str; 3] = ["~", "e!", "u#1"];
const SHORT_MCP_HEADER_SECRETS: [&str; 3] = ["^", "h?", "v%2"];

struct OfflineKiroEnv {
    protocol_log: PathBuf,
}

fn install_fixture(root: &Path) -> PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("fixture bin directory");
    let executable = bin.join("kiro-cli");
    fs::copy(FIXTURE, &executable).expect("copy fake Kiro ACP fixture");
    let mut permissions = fs::metadata(&executable)
        .expect("fake Kiro metadata")
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&executable, permissions).expect("make fake Kiro executable");
    executable
}

fn make_kiro_and_env(root: &Path) -> (KiroCli, ExecutionEnv, OfflineKiroEnv) {
    let executable = install_fixture(root);
    let protocol_log = root.join("kiro-protocol.jsonl");
    let mut kiro = KiroCli::default();
    kiro.cmd.base_command_override = Some(executable.to_string_lossy().into_owned());
    let mut env = ExecutionEnv::new(
        RepoContext::new(root.to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    env.insert("KIRO_API_KEY", API_KEY);
    env.insert(
        "OPENTEAMS_FAKE_KIRO_PROTOCOL_LOG",
        protocol_log.to_string_lossy().into_owned(),
    );
    (kiro, env, OfflineKiroEnv { protocol_log })
}

async fn prepare_run(
    kiro: &KiroCli,
    root: &Path,
    env: &ExecutionEnv,
    canonical: &MemberMcpConfig,
) -> Result<(KiroCli, ExecutionEnv, PreparedMcpRun), ExecutorError> {
    let mut kiro = kiro.clone();
    let mut env = env.clone();
    let context = McpRunContext::new(root, uuid::Uuid::new_v4(), uuid::Uuid::new_v4())?;
    let prepared = kiro
        .prepare_mcp_for_run(canonical, &context, &mut env)
        .await?;
    Ok((kiro, env, prepared))
}

fn attach_cleanup(mut spawned: SpawnedChild, prepared: PreparedMcpRun) -> SpawnedChild {
    spawned.cleanup = ExecutorRunCleanup::combine(spawned.cleanup.take(), prepared.into_cleanup());
    spawned
}

async fn spawn_prepared(
    kiro: &KiroCli,
    root: &Path,
    prompt: &str,
    env: &ExecutionEnv,
    canonical: &MemberMcpConfig,
) -> Result<SpawnedChild, ExecutorError> {
    let (kiro, env, prepared) = prepare_run(kiro, root, env, canonical).await?;
    let spawned = kiro.spawn(root, prompt, &env).await?;
    Ok(attach_cleanup(spawned, prepared))
}

async fn spawn_prepared_follow_up(
    kiro: &KiroCli,
    root: &Path,
    prompt: &str,
    session_id: &str,
    env: &ExecutionEnv,
    canonical: &MemberMcpConfig,
) -> Result<SpawnedChild, ExecutorError> {
    let (kiro, env, prepared) = prepare_run(kiro, root, env, canonical).await?;
    let spawned = kiro
        .spawn_follow_up(root, prompt, session_id, None, &env)
        .await?;
    Ok(attach_cleanup(spawned, prepared))
}

async fn finish_turn(mut spawned: SpawnedChild) -> (Vec<AcpEvent>, ExecutorExitResult, String) {
    let stderr = spawned.take_stderr().expect("redacted ACP stderr");
    let stderr_task = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut body = String::new();
        stderr.read_to_string(&mut body).await.expect("read stderr");
        body
    });

    let stdout = spawned.take_stdout().expect("ACP event output");
    let mut lines = BufReader::new(stdout).lines();
    let mut events = Vec::new();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await
            .expect("ACP output timeout")
            .expect("read ACP output");
        let Some(line) = line else { break };
        events.push(
            serde_json::from_str::<AcpRuntimeEvent>(&line)
                .expect("typed ACP runtime event")
                .payload,
        );
    }

    let exit = spawned.exit_signal.take().expect("ACP exit signal");
    let result = tokio::time::timeout(Duration::from_secs(15), exit)
        .await
        .expect("ACP exit timeout")
        .expect("ACP exit result");
    let stderr = tokio::time::timeout(Duration::from_secs(15), stderr_task)
        .await
        .expect("stderr timeout")
        .expect("stderr task");
    (events, result, stderr)
}

fn member_mcp_with_header(name: &str, env_secret: &str, header_secret: &str) -> MemberMcpConfig {
    MemberMcpConfig {
        mcp_servers: [
            (
                name.to_string(),
                serde_json::json!({
                    "command": "/bin/echo",
                    "env": {"TOKEN": env_secret}
                }),
            ),
            (
                format!("{name}-http"),
                serde_json::json!({
                    "httpUrl": "https://example.test/mcp",
                    "headers": {"Authorization": header_secret}
                }),
            ),
        ]
        .into_iter()
        .collect(),
    }
}

fn member_mcp_short_secret_matrix(name: &str) -> MemberMcpConfig {
    let stdio_servers = SHORT_MCP_ENV_SECRETS
        .into_iter()
        .enumerate()
        .map(|(index, secret)| {
            (
                format!("{name}-env-{}", index + 1),
                serde_json::json!({
                    "command": "/bin/echo",
                    "env": {"TOKEN": secret}
                }),
            )
        });
    let http_servers = SHORT_MCP_HEADER_SECRETS
        .into_iter()
        .enumerate()
        .map(|(index, secret)| {
            (
                format!("{name}-header-{}", index + 1),
                serde_json::json!({
                    "httpUrl": "https://example.test/mcp",
                    "headers": {"Authorization": secret}
                }),
            )
        });

    MemberMcpConfig {
        mcp_servers: stdio_servers.chain(http_servers).collect(),
    }
}

fn protocol_entries(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .expect("Kiro protocol log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("Kiro protocol entry"))
        .collect()
}

fn assert_consumer_redacts(label: &str, output: &str, secrets: &[&str]) {
    let placeholder_count = output.matches("[redacted]").count();
    assert!(
        placeholder_count >= secrets.len(),
        "{label} must contain at least one redaction placeholder per secret: {output}"
    );
    for secret in secrets {
        assert!(
            !output.contains(secret),
            "{label} leaked {secret:?}: {output}"
        );
    }
}

fn serialized_events(events: &[AcpEvent]) -> String {
    serde_json::to_string(events).expect("serialize consumed ACP events")
}

async fn wait_for_protocol_prompt(path: &Path, prompt: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fs::read_to_string(path).is_ok_and(|body| body.contains(prompt)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("fake Kiro must receive prompt before cancellation");
}

#[test]
fn fixture_is_repository_local_and_contains_no_fixture_credentials() {
    let source = fs::read_to_string(FIXTURE).expect("read fake Kiro ACP fixture");
    assert!(source.contains("protocolVersion: 1"));
    assert!(source.contains("session/new"));
    assert!(source.contains("session/load"));
    assert!(source.contains("session/prompt"));
    assert!(source.contains("session/cancel"));
    assert!(source.contains("user_message_chunk"));
    assert!(source.contains("agent_thought_chunk"));
    assert!(source.contains("agent_message_chunk"));
    assert!(source.contains("tool_call_update"));
    assert!(!source.contains(API_KEY));
}

#[tokio::test]
async fn offline_kiro_uses_v1_new_load_prompt_cancel_updates_and_run_scoped_mcp() {
    let workspace = tempfile::tempdir().expect("offline Kiro workspace");
    let (kiro, env, fixture) = make_kiro_and_env(workspace.path());

    let probe = kiro
        .probe_acp(workspace.path(), &env, None)
        .await
        .expect("Kiro ACP probe")
        .expect("Kiro ACP capability result");
    assert_eq!(probe.protocol_version, "1");
    assert_eq!(probe.agent_name.as_deref(), Some("Kiro CLI Agent"));
    assert_eq!(probe.agent_version.as_deref(), Some("2.20.1"));
    assert!(probe.auth_methods.is_empty());
    assert!(probe.supports_session_load);
    assert!(!probe.supports_session_resume);
    assert_eq!(probe.model_ids(), Some(vec!["auto".to_string()]));

    let first_mcp_secret = "e!";
    let first_mcp_header_secret = "h?";
    let first = spawn_prepared(
        &kiro,
        workspace.path(),
        "first prompt",
        &env,
        &member_mcp_with_header("alpha", first_mcp_secret, first_mcp_header_secret),
    )
    .await
    .expect("spawn first Kiro turn");
    let (first_events, first_exit, first_stderr) = finish_turn(first).await;
    assert!(matches!(first_exit, ExecutorExitResult::Success));
    let session_id = first_events
        .iter()
        .find_map(|event| match event {
            AcpEvent::SessionStart(session_id) => Some(session_id.clone()),
            _ => None,
        })
        .expect("Kiro session id");
    assert_eq!(session_id, "kiro-fixture-session");
    assert!(
        first_events
            .iter()
            .any(|event| matches!(event, AcpEvent::UserBlock(_)))
    );
    assert!(
        first_events
            .iter()
            .any(|event| matches!(event, AcpEvent::Thought(_)))
    );
    assert!(
        first_events
            .iter()
            .any(|event| matches!(event, AcpEvent::Message(_)))
    );
    assert!(
        first_events
            .iter()
            .any(|event| matches!(event, AcpEvent::ToolCall(_)))
    );
    assert!(
        first_events
            .iter()
            .any(|event| matches!(event, AcpEvent::ToolUpdate(_)))
    );
    assert!(
        first_events
            .iter()
            .any(|event| matches!(event, AcpEvent::Done(reason) if reason == "\"end_turn\""))
    );
    assert!(
        !first_events
            .iter()
            .any(|event| matches!(event, AcpEvent::Other(_))),
        "Kiro private notifications must not reach the standard event consumer"
    );
    let first_secrets = [API_KEY, first_mcp_secret, first_mcp_header_secret];
    assert_consumer_redacts(
        "standard session/update consumer",
        &serialized_events(&first_events),
        &first_secrets,
    );
    assert_consumer_redacts("chunked stderr consumer", &first_stderr, &first_secrets);
    let stderr_chunk_sequence = protocol_entries(&fixture.protocol_log)
        .into_iter()
        .filter(|entry| entry["event"] == "stderr_secret_chunk")
        .map(|entry| (entry["secret_index"].clone(), entry["part"].clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        stderr_chunk_sequence,
        vec![
            (serde_json::json!(0), serde_json::json!(1)),
            (serde_json::json!(0), serde_json::json!(2)),
            (serde_json::json!(1), serde_json::json!(1)),
            (serde_json::json!(1), serde_json::json!(2)),
            (serde_json::json!(2), serde_json::json!(1)),
            (serde_json::json!(2), serde_json::json!(2)),
        ],
        "fixture must write every secret in two ordered stderr chunks"
    );

    let second_mcp_secret = "u#";
    let second_mcp_header_secret = "v%";
    let follow_up = spawn_prepared_follow_up(
        &kiro,
        workspace.path(),
        "second prompt",
        &session_id,
        &env,
        &member_mcp_with_header("beta", second_mcp_secret, second_mcp_header_secret),
    )
    .await
    .expect("spawn Kiro follow-up");
    let (follow_up_events, follow_up_exit, follow_up_stderr) = finish_turn(follow_up).await;
    assert!(matches!(follow_up_exit, ExecutorExitResult::Success));
    assert!(
        follow_up_events.iter().any(
            |event| matches!(event, AcpEvent::SessionStart(id) if id == "kiro-fixture-session")
        )
    );
    let follow_up_secrets = [API_KEY, second_mcp_secret, second_mcp_header_secret];
    assert_consumer_redacts(
        "follow-up session/update consumer",
        &serialized_events(&follow_up_events),
        &follow_up_secrets,
    );
    assert_consumer_redacts(
        "follow-up chunked stderr consumer",
        &follow_up_stderr,
        &follow_up_secrets,
    );

    let mut cancel_env = env.clone();
    cancel_env.insert("OPENTEAMS_FAKE_KIRO_HANG", "1");
    let cancelled = spawn_prepared(
        &kiro,
        workspace.path(),
        "cancel prompt",
        &cancel_env,
        &MemberMcpConfig::default(),
    )
    .await
    .expect("spawn cancellable Kiro turn");
    wait_for_protocol_prompt(&fixture.protocol_log, "cancel prompt").await;
    cancelled.cancel.as_ref().expect("cancel token").cancel();
    let (cancel_events, cancel_exit, cancel_stderr) = finish_turn(cancelled).await;
    assert!(matches!(cancel_exit, ExecutorExitResult::Success));
    assert!(
        cancel_events
            .iter()
            .any(|event| matches!(event, AcpEvent::Done(reason) if reason == "\"cancelled\""))
    );
    assert_consumer_redacts("cancel stderr consumer", &cancel_stderr, &[API_KEY]);

    let entries = protocol_entries(&fixture.protocol_log);
    assert!(
        entries
            .iter()
            .any(|entry| { entry["method"] == "initialize" && entry["protocol_version"] == 1 })
    );
    assert!(entries.iter().any(|entry| {
        entry["method"] == "session/new"
            && entry["mcp_servers"] == serde_json::json!(["alpha", "alpha-http"])
    }));
    assert!(entries.iter().any(|entry| {
        entry["method"] == "session/new"
            && entry["mcp_servers"] == serde_json::json!([])
            && entry["has_mcp_servers"] == true
    }));
    assert!(entries.iter().any(|entry| {
        entry["method"] == "session/load"
            && entry["session_id"] == "kiro-fixture-session"
            && entry["mcp_servers"] == serde_json::json!(["beta", "beta-http"])
    }));
    assert!(
        !entries
            .iter()
            .any(|entry| entry["method"] == "session/resume")
    );
    assert!(entries.iter().any(|entry| {
        entry["method"] == "session/prompt"
            && entry["prompt_text"] == "first prompt"
            && entry["prompt_types"] == serde_json::json!(["text"])
            && entry["has_prompt"] == true
            && entry["has_content"] == false
    }));
    assert!(
        entries
            .iter()
            .any(|entry| { entry["method"] == "session/cancel" && entry["has_id"] == false })
    );
    let protocol_log = fs::read_to_string(&fixture.protocol_log).expect("protocol log body");
    for secret in [
        API_KEY,
        first_mcp_secret,
        first_mcp_header_secret,
        second_mcp_secret,
        second_mcp_header_secret,
    ] {
        assert!(
            !protocol_log.contains(secret),
            "protocol log leaked a secret"
        );
    }

    let runtime_root = workspace.path().join(".openteams/tmp");
    assert!(
        !runtime_root.exists()
            || fs::read_dir(runtime_root)
                .expect("Kiro runtime directory")
                .next()
                .is_none(),
        "completed Kiro turns must clean their private MCP snapshots"
    );
}

#[tokio::test]
async fn offline_kiro_redacts_one_two_three_character_member_mcp_values_for_every_consumer() {
    for (index, secret) in SHORT_MCP_ENV_SECRETS.iter().enumerate() {
        assert_eq!(secret.chars().count(), index + 1, "env secret matrix");
    }
    for (index, secret) in SHORT_MCP_HEADER_SECRETS.iter().enumerate() {
        assert_eq!(secret.chars().count(), index + 1, "header secret matrix");
    }
    let all_secrets = std::iter::once(API_KEY)
        .chain(SHORT_MCP_ENV_SECRETS)
        .chain(SHORT_MCP_HEADER_SECRETS)
        .collect::<Vec<_>>();

    let standard_workspace = tempfile::tempdir().expect("standard update workspace");
    let (kiro, env, fixture) = make_kiro_and_env(standard_workspace.path());
    let spawned = spawn_prepared(
        &kiro,
        standard_workspace.path(),
        "short secret standard update",
        &env,
        &member_mcp_short_secret_matrix("standard"),
    )
    .await
    .expect("spawn short secret standard update");
    let (events, exit, stderr) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AcpEvent::Message(_)))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AcpEvent::ToolUpdate(_)))
    );
    assert_consumer_redacts(
        "short secret standard session/update consumer",
        &serialized_events(&events),
        &all_secrets,
    );
    assert_consumer_redacts(
        "short secret chunked stderr consumer",
        &stderr,
        &all_secrets,
    );
    let stderr_chunk_sequence = protocol_entries(&fixture.protocol_log)
        .into_iter()
        .filter(|entry| entry["event"] == "stderr_secret_chunk")
        .map(|entry| (entry["secret_index"].clone(), entry["part"].clone()))
        .collect::<Vec<_>>();
    let expected_chunk_sequence = (0..all_secrets.len())
        .flat_map(|index| {
            [
                (serde_json::json!(index), serde_json::json!(1)),
                (serde_json::json!(index), serde_json::json!(2)),
            ]
        })
        .collect::<Vec<_>>();
    assert_eq!(stderr_chunk_sequence, expected_chunk_sequence);
    let protocol_log = fs::read_to_string(&fixture.protocol_log).expect("standard protocol log");
    for secret in &all_secrets {
        assert!(
            !protocol_log.contains(secret),
            "protocol log leaked {secret:?}"
        );
    }

    let prompt_workspace = tempfile::tempdir().expect("prompt error workspace");
    let (kiro, mut env, fixture) = make_kiro_and_env(prompt_workspace.path());
    env.insert("OPENTEAMS_FAKE_KIRO_PROMPT_ERROR", "1");
    let spawned = spawn_prepared(
        &kiro,
        prompt_workspace.path(),
        "short secret prompt error",
        &env,
        &member_mcp_short_secret_matrix("prompt"),
    )
    .await
    .expect("spawn short secret prompt error");
    let (events, exit, stderr) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Failure));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AcpEvent::Error(_)))
    );
    assert_consumer_redacts(
        "short secret prompt error consumer",
        &serialized_events(&events),
        &all_secrets,
    );
    assert_consumer_redacts("short secret prompt stderr consumer", &stderr, &all_secrets);
    let protocol_log = fs::read_to_string(&fixture.protocol_log).expect("prompt protocol log");
    for secret in &all_secrets {
        assert!(
            !protocol_log.contains(secret),
            "protocol log leaked {secret:?}"
        );
    }

    let protocol_workspace = tempfile::tempdir().expect("session error workspace");
    let (kiro, mut env, fixture) = make_kiro_and_env(protocol_workspace.path());
    env.insert("OPENTEAMS_FAKE_KIRO_SESSION_ERROR", "1");
    let error = spawn_prepared(
        &kiro,
        protocol_workspace.path(),
        "short secret session error",
        &env,
        &member_mcp_short_secret_matrix("protocol"),
    )
    .await
    .expect_err("short secret session error");
    let ExecutorError::Configuration(message) = error else {
        panic!("invalid session request must remain a configuration error");
    };
    assert_consumer_redacts(
        "short secret session protocol error consumer",
        &message,
        &all_secrets,
    );
    let protocol_log = fs::read_to_string(&fixture.protocol_log).expect("session protocol log");
    for secret in &all_secrets {
        assert!(
            !protocol_log.contains(secret),
            "protocol log leaked {secret:?}"
        );
    }
}

#[tokio::test]
async fn offline_kiro_auth_and_prompt_errors_are_typed_and_redacted() {
    let workspace = tempfile::tempdir().expect("offline Kiro error workspace");
    let (kiro, mut env, fixture) = make_kiro_and_env(workspace.path());
    env.remove("KIRO_API_KEY");
    let error = kiro
        .spawn(workspace.path(), "must not start ACP", &env)
        .await
        .expect_err("missing local login and API key must fail");
    assert!(matches!(error, ExecutorError::AuthRequired(_)));
    let auth_log = fs::read_to_string(&fixture.protocol_log).expect("authentication protocol log");
    assert!(auth_log.contains(r#""argv":["whoami","--format","json"]"#));
    assert!(!auth_log.contains(r#""method":"initialize""#));

    let login_workspace = tempfile::tempdir().expect("local login workspace");
    let (kiro, mut env, _) = make_kiro_and_env(login_workspace.path());
    env.remove("KIRO_API_KEY");
    env.insert("OPENTEAMS_FAKE_KIRO_LOCAL_LOGIN", "1");
    assert!(
        kiro.probe_authentication(login_workspace.path(), &env)
            .await
            .expect("fixture local login probe")
    );

    let error_workspace = tempfile::tempdir().expect("prompt error workspace");
    let (kiro, mut env, _) = make_kiro_and_env(error_workspace.path());
    env.insert("OPENTEAMS_FAKE_KIRO_PROMPT_ERROR", "1");
    let prompt_env_secret = "p&";
    let prompt_header_secret = "q*";
    let spawned = spawn_prepared(
        &kiro,
        error_workspace.path(),
        "prompt error",
        &env,
        &member_mcp_with_header("prompt", prompt_env_secret, prompt_header_secret),
    )
    .await
    .expect("spawn prompt error fixture");
    let (events, exit, stderr) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Failure));
    assert!(events.iter().any(|event| matches!(
        event,
        AcpEvent::Error(message)
            if message.contains("fixture prompt failure") && message.contains("[redacted]")
    )));
    let prompt_secrets = [API_KEY, prompt_env_secret, prompt_header_secret];
    assert_consumer_redacts(
        "prompt error event consumer",
        &serialized_events(&events),
        &prompt_secrets,
    );
    assert_consumer_redacts("prompt error stderr consumer", &stderr, &prompt_secrets);
}

#[tokio::test]
async fn offline_kiro_probe_redacts_a_short_api_key() {
    let workspace = tempfile::tempdir().expect("offline Kiro probe error workspace");
    let (kiro, mut env, _) = make_kiro_and_env(workspace.path());
    let short_api_key = API_KEY;
    env.insert("KIRO_API_KEY", short_api_key);
    env.insert("OPENTEAMS_FAKE_KIRO_PROBE_ERROR", "1");

    let error = kiro
        .probe_acp(workspace.path(), &env, None)
        .await
        .expect_err("fixture probe authentication error");

    let ExecutorError::AuthRequired(message) = error else {
        panic!("probe authentication error must remain structured");
    };
    assert_consumer_redacts("probe error consumer", &message, &[short_api_key]);
}

#[tokio::test]
async fn offline_kiro_protocol_error_redacts_api_key_and_member_mcp_values() {
    let workspace = tempfile::tempdir().expect("offline Kiro protocol error workspace");
    let (kiro, mut env, fixture) = make_kiro_and_env(workspace.path());
    env.insert("OPENTEAMS_FAKE_KIRO_SESSION_ERROR", "1");
    let env_secret = "r+";
    let header_secret = "s=";

    let error = spawn_prepared(
        &kiro,
        workspace.path(),
        "session failure",
        &env,
        &member_mcp_with_header("failure", env_secret, header_secret),
    )
    .await
    .expect_err("fixture session protocol error");

    let ExecutorError::Configuration(message) = error else {
        panic!("invalid session request must remain a configuration error");
    };
    assert_consumer_redacts(
        "session protocol error consumer",
        &message,
        &[API_KEY, env_secret, header_secret],
    );
    let protocol_log = fs::read_to_string(&fixture.protocol_log).expect("protocol error log");
    for secret in [API_KEY, env_secret, header_secret] {
        assert!(
            !protocol_log.contains(secret),
            "protocol log leaked a secret"
        );
    }
}
