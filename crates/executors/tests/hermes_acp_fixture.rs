#![cfg(feature = "qa-mode")]

//! Offline Hermes ACP fixture integration tests.
//!
//! These tests exercise the Hermes executor lifecycle using a repository-local
//! fake `hermes acp` fixture. No network access, real Hermes CLI, user login
//! state, or user-level Hermes configuration is required.
//!
//! The tests assert the generic ACP contract (initialize, capability probe,
//! model/option mapping, prompt, follow-up, structured prompt, cancel, tool
//! call, token usage, event projection, approval policies, MCP allowlist,
//! secret redaction) rather than Hermes-specific implementation details.
//!
//! Process-level HOME isolation: the Hermes adapter resolves
//! `~/.hermes/config.yaml` via `dirs::home_dir()`, which reads the *process*
//! `HOME` environment variable rather than the child `ExecutionEnv`. To keep
//! tests fully offline and deterministic regardless of the host machine's
//! `~/.hermes` contents, `make_hermes_and_env` acquires a global mutex and
//! points the process `HOME` at a per-test temporary directory for the
//! duration of the returned guard. This serializes process-HOME mutation
//! across parallel tests while preserving correctness.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration,
};

use executors::{
    env::{ExecutionEnv, RepoContext},
    executors::{
        AcpProbeAuthState, ExecutorExitResult, ExecutorPrompt, ExecutorPromptImage,
        StandardCodingAgentExecutor,
        acp::{
            AcpEvent, AcpExecutionOptions,
            events::AcpRuntimeEvent,
            mcp::{AcpMcpPolicy, resolve_isolated_mcp_snapshot},
        },
        hermes::Hermes,
    },
};
use tokio::io::{AsyncBufReadExt, BufReader};

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hermes_acp");

/// Global mutex serializing process-level `HOME` mutation so parallel tests do
/// not race on `dirs::home_dir()` resolution. A test holds this guard for the
/// duration of its spawn/probe calls.
static HOME_ISOLATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn home_lock() -> &'static Mutex<()> {
    HOME_ISOLATION_LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard that sets the process `HOME` to an isolated temporary directory
/// and restores the original value on drop. The guard also holds the global
/// `HOME_ISOLATION_LOCK` so parallel tests cannot observe each other's `HOME`.
///
/// This is required because the Hermes adapter resolves its MCP config path via
/// `dirs::home_dir()`, which reads the process `HOME`, not the child
/// `ExecutionEnv` that `make_hermes_and_env` populates. Without this guard, a
/// host machine with a real `~/.hermes/config.yaml` would be read by the test.
pub(crate) struct HomeIsolationGuard {
    _lock: MutexGuard<'static, ()>,
    saved_home: Option<std::ffi::OsString>,
}

impl HomeIsolationGuard {
    fn acquire(isolated_home: &Path) -> Self {
        let lock = home_lock().lock().expect("HOME isolation lock poisoned");
        let saved_home = std::env::var_os("HOME");
        // Safety: edition 2024 marks `set_var` as unsafe because env mutation
        // is not thread-safe. We serialize all mutation through
        // `HOME_ISOLATION_LOCK` so no other test thread touches `HOME` while
        // this guard is alive. The test process does not spawn additional
        // threads that read `HOME` concurrently with this mutation.
        unsafe {
            std::env::set_var("HOME", isolated_home);
        }
        Self {
            _lock: lock,
            saved_home,
        }
    }
}

impl Drop for HomeIsolationGuard {
    fn drop(&mut self) {
        // Safety: same serialization guarantee as `acquire`.
        unsafe {
            match self.saved_home.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(FIXTURE_DIR).join(name)
}

fn read_fixture(name: &str) -> String {
    fs::read_to_string(fixture_path(name)).unwrap_or_else(|_| panic!("read fixture {name}"))
}

struct OfflineHermesEnv {
    bin: PathBuf,
    home: PathBuf,
    prompts: PathBuf,
    permission_log: PathBuf,
    protocol_log: PathBuf,
}

fn install_offline_hermes_fixture(root: &Path, executable: bool) -> OfflineHermesEnv {
    let bin = root.join("bin");
    let home = root.join("home");
    fs::create_dir_all(&bin).expect("bin dir");
    fs::create_dir_all(&home).expect("isolated home dir");

    let mode = if executable { 0o755 } else { 0o644 };

    let hermes_source = read_fixture("fake_hermes_acp.mjs");
    let hermes_path = bin.join("hermes");
    fs::write(&hermes_path, &hermes_source).expect("write fake hermes");
    fs::set_permissions(&hermes_path, fs::Permissions::from_mode(mode)).expect("hermes chmod");

    OfflineHermesEnv {
        bin,
        home,
        prompts: root.join("prompts.txt"),
        permission_log: root.join("permissions.jsonl"),
        protocol_log: root.join("protocol.jsonl"),
    }
}

fn make_hermes_and_env(
    root: &Path,
    executable: bool,
) -> (Hermes, ExecutionEnv, OfflineHermesEnv, HomeIsolationGuard) {
    let env_info = install_offline_hermes_fixture(root, executable);
    // Isolate the process-level HOME so `dirs::home_dir()` resolves to the
    // per-test temporary directory. Without this, the Hermes adapter would
    // read the host machine's `~/.hermes/config.yaml` instead of the isolated
    // config, breaking offline guarantees on machines with a real Hermes
    // installation.
    let home_guard = HomeIsolationGuard::acquire(&env_info.home);
    let mut hermes = Hermes::default();
    let hermes_path = env_info.bin.join("hermes");
    hermes.cmd.base_command_override = Some(hermes_path.display().to_string());
    let mut env = ExecutionEnv::new(
        RepoContext::new(root.to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    env.insert(
        "PATH",
        format!(
            "{}:{}",
            env_info.bin.display(),
            std::env::var("PATH").unwrap_or_default(),
        ),
    );
    env.insert("HOME", env_info.home.to_string_lossy().to_string());
    env.insert(
        "OPENTEAMS_FAKE_HERMES_PROMPTS",
        env_info.prompts.to_string_lossy().to_string(),
    );
    env.insert(
        "OPENTEAMS_FAKE_HERMES_PERMISSION_LOG",
        env_info.permission_log.to_string_lossy().to_string(),
    );
    env.insert(
        "OPENTEAMS_FAKE_HERMES_PROTOCOL_LOG",
        env_info.protocol_log.to_string_lossy().to_string(),
    );
    (hermes, env, env_info, home_guard)
}

async fn finish_turn(
    mut spawned: executors::executors::SpawnedChild,
) -> (Vec<AcpEvent>, ExecutorExitResult) {
    let stdout = spawned.take_stdout().expect("ACP stdout");
    let mut lines = BufReader::new(stdout).lines();
    let mut events = Vec::new();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await
            .expect("ACP output timeout")
            .expect("ACP output read");
        let Some(line) = line else { break };
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
    let result = tokio::time::timeout(Duration::from_secs(15), exit)
        .await
        .expect("exit timeout")
        .expect("exit result");
    (events, result)
}

#[test]
fn fake_hermes_fixture_files_are_present_and_secret_safe() {
    let hermes_acp = read_fixture("fake_hermes_acp.mjs");
    assert!(hermes_acp.contains("protocolVersion"));
    assert!(hermes_acp.contains("session/prompt"));
    assert!(hermes_acp.contains("session/cancel"));
    assert!(hermes_acp.contains("session/request_permission"));
    assert!(hermes_acp.contains("session/resume"));
    assert!(hermes_acp.contains("session/load"));
    assert!(hermes_acp.contains("STALE_RESUME_REJECT"));
    assert!(hermes_acp.contains("STALE_RESUME_REFUSAL"));
    assert!(!hermes_acp.contains("API_KEY"));
    assert!(!hermes_acp.contains("SECRET"));
    assert!(!hermes_acp.contains("TOKEN"));
    assert!(!hermes_acp.contains("registry.npmjs"));
    assert!(hermes_acp.contains("OPENTEAMS_FAKE_HERMES_PROMPTS"));
}

#[cfg(unix)]
#[test]
fn fake_hermes_fixture_script_is_executable() {
    let mode = fs::metadata(fixture_path("fake_hermes_acp.mjs"))
        .expect("hermes fixture meta")
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "fake_hermes_acp.mjs must be executable");
}

#[tokio::test]
async fn fake_hermes_does_not_contact_network_or_real_cli() {
    let temp = tempfile::tempdir().expect("offline workspace");
    let (hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    let spawned = hermes
        .spawn(temp.path(), "verify-offline", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e,
        AcpEvent::Message(msg) if format!("{msg:?}").contains("echo:verify-offline"))));
    assert!(
        !temp.path().join("home/.hermes").exists(),
        "fake hermes must not touch user-level config"
    );
}

#[tokio::test]
async fn offline_hermes_lifecycle_new_prompt_follow_up_cancel_and_startup_failure() {
    let temp = tempfile::tempdir().expect("workspace");
    let (hermes, env, env_info, _home_guard) = make_hermes_and_env(temp.path(), true);

    let first = hermes
        .spawn(temp.path(), "first", &env)
        .await
        .expect("spawn first");
    let (first_events, first_exit) = finish_turn(first).await;
    assert!(matches!(first_exit, ExecutorExitResult::Success));
    let session_id = first_events
        .iter()
        .find_map(|e| match e {
            AcpEvent::SessionStart(id) => Some(id.clone()),
            _ => None,
        })
        .expect("session id");
    assert_eq!(session_id, "hermes-offline-session");
    assert!(first_events.iter().any(|e| matches!(e,
        AcpEvent::Message(m) if format!("{m:?}").contains("echo:first"))));
    assert!(first_events.iter().any(|e| matches!(e, AcpEvent::Done(_))));

    let follow_up = hermes
        .spawn_follow_up(temp.path(), "second", &session_id, None, &env)
        .await
        .expect("follow-up");
    let (fu_events, fu_exit) = finish_turn(follow_up).await;
    assert!(matches!(fu_exit, ExecutorExitResult::Success));
    assert!(fu_events.iter().any(|e| matches!(e,
        AcpEvent::Message(m) if format!("{m:?}").contains("echo:second"))));
    assert_eq!(
        fs::read_to_string(&env_info.prompts)
            .expect("prompts")
            .lines()
            .collect::<Vec<_>>(),
        ["first", "second"]
    );

    let mut cancel_env = env.clone();
    cancel_env.insert("OPENTEAMS_FAKE_HERMES_HANG", "1");
    let mut cancelled = hermes
        .spawn(temp.path(), "cancel-me", &cancel_env)
        .await
        .expect("cancellable spawn");
    cancelled.cancel.as_ref().expect("cancel token").cancel();
    let cancel_exit = cancelled.exit_signal.take().expect("cancel exit");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(15), cancel_exit)
            .await
            .expect("cancel timeout")
            .expect("cancel result"),
        ExecutorExitResult::Success
    ));
    drop(cancelled);

    let protocol_log = temp.path().join("protocol.jsonl");
    if protocol_log.exists() {
        let log = fs::read_to_string(&protocol_log).expect("protocol log");
        assert!(
            log.contains("session/cancel"),
            "protocol log must record session/cancel: {log}"
        );
    }

    let fail_temp = tempfile::tempdir().expect("fail workspace");
    // Reuse the already-isolated process HOME from the first guard; do not call
    // make_hermes_and_env here because that would try to re-acquire the
    // HOME_ISOLATION_LOCK and deadlock. The fail fixture only needs a
    // non-executable hermes binary; it does not need its own HOME isolation
    // because the process HOME is already isolated by _home_guard above.
    let fail_env_info = install_offline_hermes_fixture(fail_temp.path(), false);
    let mut fail_hermes = Hermes::default();
    let fail_hermes_path = fail_env_info.bin.join("hermes");
    fail_hermes.cmd.base_command_override = Some(fail_hermes_path.display().to_string());
    let mut fail_env = ExecutionEnv::new(
        RepoContext::new(fail_temp.path().to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    fail_env.insert("HOME", fail_env_info.home.to_string_lossy().to_string());
    let error = tokio::time::timeout(
        Duration::from_secs(15),
        fail_hermes.spawn(fail_temp.path(), "must fail", &fail_env),
    )
    .await
    .expect("timeout")
    .expect_err("must fail");
    let error_string = error.to_string();
    assert!(
        error_string.contains("Permission denied")
            || error_string.contains("ACP startup failed")
            || error_string.contains("Permission"),
        "non-executable hermes must surface a startup/spawn error: {error_string}"
    );
    let failed_runtime_dir = fail_temp.path().join(".openteams/tmp");
    assert!(
        !failed_runtime_dir.exists()
            || fs::read_dir(&failed_runtime_dir)
                .expect("runtime dir")
                .next()
                .is_none(),
        "failed startup must not leave runtime files behind"
    );
}

#[tokio::test]
async fn offline_hermes_probe_initializes_and_reports_protocol_and_models() {
    let temp = tempfile::tempdir().expect("probe workspace");
    let (hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    let probe = hermes
        .probe_acp(temp.path(), &env, None)
        .await
        .expect("probe")
        .expect("probe result");
    assert_eq!(probe.protocol_version, "1");
    assert_eq!(probe.agent_name.as_deref(), Some("hermes-fake-acp"));
    assert!(probe.supports_session_resume);
    assert!(probe.supports_session_load);
    assert!(!probe.supports_session_close);
    assert!(!probe.supports_session_delete);
    assert!(!probe.supports_additional_directories);
    let model_ids = probe.model_ids().expect("model ids");
    assert!(model_ids.contains(&"openrouter:hermes-pro".to_string()));
    assert!(model_ids.contains(&"nous:hermes-flash".to_string()));
}

#[tokio::test]
async fn offline_hermes_probe_reports_setup_without_creating_a_session() {
    let temp = tempfile::tempdir().expect("setup workspace");
    let (hermes, mut env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    env.insert("OPENTEAMS_FAKE_HERMES_NEEDS_SETUP", "1");
    let probe = hermes
        .probe_acp(temp.path(), &env, None)
        .await
        .expect("probe")
        .expect("probe result");
    assert_eq!(
        hermes.interpret_acp_probe(&probe).auth_state,
        Some(AcpProbeAuthState::Unauthenticated)
    );
    assert!(probe.model_ids().is_none());

    let log = fs::read_to_string(temp.path().join("protocol.jsonl")).expect("protocol log");
    assert!(log.contains("hermes-setup"));
    assert!(!log.contains("session/new"));
    assert!(log.contains(r#""argv":["acp"]"#), "exact argv: {log}");
    assert!(
        log.contains(r#""skip_configured_mcp":"1""#),
        "ambient MCP opt-out must be set: {log}"
    );
}

#[tokio::test]
async fn offline_hermes_session_metadata_failure_keeps_initialize_metadata() {
    let temp = tempfile::tempdir().expect("metadata failure workspace");
    let (hermes, mut env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    env.insert("OPENTEAMS_FAKE_HERMES_SESSION_PROBE_FAIL", "1");
    let probe = hermes
        .probe_acp(temp.path(), &env, None)
        .await
        .expect("initialize must remain successful")
        .expect("probe result");
    assert_eq!(probe.agent_version.as_deref(), Some("0.0.1-fixture"));
    assert_eq!(
        hermes.interpret_acp_probe(&probe).auth_state,
        Some(AcpProbeAuthState::Authenticated)
    );
    assert!(probe.model_ids().is_none());
}

#[tokio::test]
async fn offline_hermes_rejects_unsupported_security_options_and_setup_auth() {
    use executors::executors::acp::{AcpAccessMode, AcpAuthSelection};

    let temp = tempfile::tempdir().expect("unsupported options workspace");
    let (mut hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    hermes.acp = Some(AcpExecutionOptions {
        access_mode: Some(AcpAccessMode::WorkspaceOnly),
        ..Default::default()
    });
    let error = hermes
        .spawn(temp.path(), "workspace-only", &env)
        .await
        .expect_err("workspace-only must be rejected");
    assert!(error.to_string().contains("workspace-only"));

    hermes.acp = Some(AcpExecutionOptions {
        additional_directories: Some(vec![temp.path().join("extra").display().to_string()]),
        ..Default::default()
    });
    let error = hermes
        .spawn(temp.path(), "additional-directory", &env)
        .await
        .expect_err("additional directories must be rejected");
    assert!(error.to_string().contains("additional directories"));

    hermes.acp = Some(AcpExecutionOptions {
        auth: Some(AcpAuthSelection::MethodId {
            method_id: "hermes-setup".to_string(),
        }),
        ..Default::default()
    });
    let error = hermes
        .spawn(temp.path(), "setup", &env)
        .await
        .expect_err("setup marker must not be authenticated over ACP");
    assert!(error.to_string().contains("hermes acp --setup"));
}

#[tokio::test]
async fn offline_hermes_probe_model_and_dynamic_option_mapping_is_exact() {
    let temp = tempfile::tempdir().expect("config workspace");
    let (hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    let probe = hermes
        .probe_acp(temp.path(), &env, None)
        .await
        .expect("probe")
        .expect("probe result");
    let model_option = probe
        .config_options
        .iter()
        .find(|o| o.category.as_deref() == Some("model"))
        .expect("model config option");
    match &model_option.kind {
        executors::executors::acp::AcpConfigOptionKind::Select { options, .. } => {
            let values: Vec<&str> = options.iter().map(|c| c.value.as_str()).collect();
            assert_eq!(values, vec!["openrouter:hermes-pro", "nous:hermes-flash"]);
            let names: Vec<&str> = options.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, vec!["Hermes Pro", "Hermes Flash"]);
        }
        other => panic!("expected select model option, got {other:?}"),
    }
    assert_eq!(model_option.id, "model");
    assert_eq!(model_option.name, "Model");
    assert_eq!(probe.config_options.len(), 1);
}

#[tokio::test]
async fn offline_hermes_list_models_returns_acp_probe_models() {
    let temp = tempfile::tempdir().expect("list models workspace");
    let (hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    let models = hermes
        .list_models(temp.path(), &env)
        .await
        .expect("list models")
        .expect("models present");
    assert_eq!(
        models,
        vec![
            "openrouter:hermes-pro".to_string(),
            "nous:hermes-flash".to_string(),
        ]
    );
}

#[tokio::test]
async fn offline_hermes_token_usage_is_projected() {
    let temp = tempfile::tempdir().expect("token workspace");
    let (hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    let spawned = hermes
        .spawn(temp.path(), "token-test", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::Usage(_))));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::TokenUsage(_))));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::Thought(_))));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::Plan(_))));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AcpEvent::AvailableCommands(_)))
    );
    assert!(events.iter().any(|e| matches!(e, AcpEvent::SessionInfo(_))));
}

#[tokio::test]
async fn offline_hermes_tool_call_is_projected() {
    let temp = tempfile::tempdir().expect("tool workspace");
    let (hermes, mut env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    env.insert("OPENTEAMS_FAKE_HERMES_TOOL_CALL", "use-tool");
    let spawned = hermes
        .spawn(temp.path(), "use-tool", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::ToolCall(_))));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AcpEvent::RequestPermission(_)))
    );
}

#[tokio::test]
async fn offline_hermes_structured_prompt_completes_and_echoes() {
    let temp = tempfile::tempdir().expect("structured workspace");
    let (hermes, env, env_info, _home_guard) = make_hermes_and_env(temp.path(), true);
    let prompt = ExecutorPrompt {
        text: "structured-turn".to_string(),
        images: vec![ExecutorPromptImage {
            data: "aGVybWVzLWltYWdl".to_string(),
            mime_type: "image/png".to_string(),
            uri: Some("fixture://hermes-image".to_string()),
        }],
    };
    let spawned = hermes
        .spawn_structured(temp.path(), &prompt, &env)
        .await
        .expect("spawn structured");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e,
        AcpEvent::Message(m) if format!("{m:?}").contains("echo:structured-turn"))));
    assert_eq!(
        fs::read_to_string(&env_info.prompts)
            .expect("prompts")
            .trim(),
        "structured-turn"
    );
    let log = fs::read_to_string(temp.path().join("protocol.jsonl")).expect("protocol log");
    assert!(
        log.contains(r#""prompt_types":["text","image"]"#),
        "structured prompt must retain its image block: {log}"
    );
}

#[tokio::test]
async fn offline_hermes_follow_up_structured_prompt_completes() {
    let temp = tempfile::tempdir().expect("structured follow-up workspace");
    let (hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    let first = hermes
        .spawn(temp.path(), "initial", &env)
        .await
        .expect("spawn initial");
    let (first_events, _) = finish_turn(first).await;
    let session_id = first_events
        .iter()
        .find_map(|e| match e {
            AcpEvent::SessionStart(id) => Some(id.clone()),
            _ => None,
        })
        .expect("session id");

    let prompt = ExecutorPrompt {
        text: "structured-follow-up".to_string(),
        images: Vec::new(),
    };
    let spawned = hermes
        .spawn_follow_up_structured(temp.path(), &prompt, &session_id, None, &env)
        .await
        .expect("spawn follow-up structured");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e,
        AcpEvent::Message(m) if format!("{m:?}").contains("echo:structured-follow-up"))));
}

#[tokio::test]
async fn offline_hermes_probe_failure_is_classified_as_startup_error() {
    let temp = tempfile::tempdir().expect("probe fail workspace");
    let (hermes, mut env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    env.insert("OPENTEAMS_FAKE_HERMES_PROBE_FAIL", "1");
    let error = hermes
        .probe_acp(temp.path(), &env, None)
        .await
        .expect_err("probe must fail");
    assert!(
        error.to_string().contains("hermes probe forced failure")
            || error.to_string().contains("initialize ACP connection"),
        "probe failure must be surfaced: {error}"
    );
}

#[tokio::test]
async fn offline_hermes_probe_timeout_is_reported_when_fixture_hangs() {
    let temp = tempfile::tempdir().expect("probe timeout workspace");
    let (hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    // Force the fixture to hang on initialize by replacing it with a sleeper.
    let bin = temp.path().join("bin");
    let sleeper = bin.join("hermes");
    fs::write(&sleeper, "#!/bin/sh\nsleep 60\n").expect("write sleeper");
    fs::set_permissions(&sleeper, fs::Permissions::from_mode(0o755)).expect("sleeper chmod");
    let mut hang_env = env.clone();
    hang_env.insert(
        "PATH",
        format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default(),
        ),
    );
    let error = tokio::time::timeout(
        Duration::from_secs(25),
        hermes.probe_acp(temp.path(), &hang_env, None),
    )
    .await
    .expect("probe must not exceed outer timeout")
    .expect_err("probe must fail");
    assert!(
        error.to_string().contains("timed out") || error.to_string().contains("initialize"),
        "probe timeout must be reported: {error}"
    );
}

#[tokio::test]
async fn offline_hermes_cli_missing_reports_not_found_diagnostics() {
    // Use a non-existent base_command_override so the availability check is
    // deterministic regardless of whether a real `hermes` is installed on the
    // host machine. This keeps the test fully isolated from the host PATH and
    // proves the diagnostic never contacts a real CLI.
    let mut hermes = Hermes::default();
    hermes.cmd.base_command_override =
        Some("openteams-hermes-cli-not-installed-never-real".to_string());
    let info = hermes.get_availability_info();
    assert!(
        matches!(info, executors::executors::AvailabilityInfo::NotFound),
        "Hermes must be NotFound when the configured command does not resolve: {info:?}"
    );
}

#[tokio::test]
async fn offline_hermes_cli_missing_with_absolute_nonexistent_path_is_not_found() {
    // An absolute path that does not exist must also report NotFound. This is
    // fully isolated from the host PATH and deterministic on every machine,
    // including CI environments where a real `hermes` may be installed.
    let temp = tempfile::tempdir().expect("nonexistent path workspace");
    let absent = temp.path().join("hermes-binary-does-not-exist");
    let mut hermes = Hermes::default();
    hermes.cmd.base_command_override = Some(absent.to_string_lossy().to_string());
    let info = hermes.get_availability_info();
    assert!(
        matches!(info, executors::executors::AvailabilityInfo::NotFound),
        "Hermes must be NotFound when the resolved absolute path does not exist: {info:?}"
    );
}

#[tokio::test]
async fn offline_hermes_stale_session_id_is_not_reused_verbatim() {
    let temp = tempfile::tempdir().expect("stale workspace");
    let (hermes, mut env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    env.insert("OPENTEAMS_FAKE_HERMES_STALE_SESSION", "1");
    let spawned = hermes
        .spawn(temp.path(), "stale-check", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    let session_id = events
        .iter()
        .find_map(|e| match e {
            AcpEvent::SessionStart(id) => Some(id.clone()),
            _ => None,
        })
        .expect("session id");
    assert!(
        session_id.starts_with("hermes-stale-"),
        "stale session id must be reported as-is from the agent: {session_id}"
    );
}

#[tokio::test]
async fn offline_hermes_stale_session_follow_up_is_classified_as_configuration_error() {
    // Drive session/resume with an unknown session id. The fixture is put into
    // STALE_RESUME_REJECT mode, which returns invalid_params (-32602) for any
    // session id that was never created via session/new. The ACP runtime maps
    // invalid_params to BootstrapError::Configuration -> ExecutorError::Configuration,
    // which is the documented error classification for an unrecoverable session.
    let temp = tempfile::tempdir().expect("stale follow-up workspace");
    let (hermes, mut env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    env.insert("OPENTEAMS_FAKE_HERMES_STALE_RESUME_REJECT", "1");
    let stale_session_id = "hermes-session-that-never-existed";
    let error = tokio::time::timeout(
        Duration::from_secs(15),
        hermes.spawn_follow_up(temp.path(), "follow-up-stale", stale_session_id, None, &env),
    )
    .await
    .expect("follow-up must not exceed outer timeout")
    .expect_err("follow-up with a stale session id must fail");
    let error_string = error.to_string();
    assert!(
        error_string.contains("session not found")
            || error_string.contains("hermes-session-that-never-existed"),
        "stale session follow-up must surface the agent's session-not-found error: {error_string}"
    );
    assert!(
        matches!(error, executors::executors::ExecutorError::Configuration(_)),
        "stale session follow-up must be classified as Configuration error (invalid_params), got: {error:?}"
    );

    // Verify the protocol log recorded the rejection so diagnostics are auditable.
    let protocol_log = temp.path().join("protocol.jsonl");
    assert!(protocol_log.exists(), "protocol log must exist");
    let log = fs::read_to_string(&protocol_log).expect("protocol log");
    assert!(
        log.contains("stale_session_rejected"),
        "protocol log must record the stale session rejection: {log}"
    );
}

#[tokio::test]
async fn offline_hermes_stale_resume_refusal_is_not_reported_as_success() {
    // Hermes can acknowledge session/resume without a sessionId and only
    // reveal that the session is invalid when the following prompt is refused.
    // The adapter must preserve the old ID only for the wire request, never as
    // evidence that the follow-up succeeded.
    let temp = tempfile::tempdir().expect("stale refusal workspace");
    let (hermes, mut env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    env.insert("OPENTEAMS_FAKE_HERMES_STALE_RESUME_REFUSAL", "1");
    let stale_session_id = "hermes-session-that-was-expired";
    let spawned = hermes
        .spawn_follow_up(
            temp.path(),
            "follow-up-stale-refusal",
            stale_session_id,
            None,
            &env,
        )
        .await
        .expect("ACP startup must complete before the prompt reveals the stale session");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(
        exit,
        ExecutorExitResult::FailureWithError(ref message)
            if message.contains("session recovery") && message.contains("session is invalid")
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        AcpEvent::Error(message)
            if message.contains("session recovery") && message.contains("session is invalid")
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AcpEvent::Done(_)))
    );

    let protocol_log = temp.path().join("protocol.jsonl");
    let log = fs::read_to_string(protocol_log).expect("protocol log");
    assert!(log.contains("stale_session_resumed_without_id"));
    assert!(log.contains("stale_session_prompt_refused"));
}

#[tokio::test]
async fn offline_hermes_three_approval_policies_verify_permission_decisions() {
    use executors::executors::acp::AcpApprovalMode;

    let expectations = [
        (AcpApprovalMode::AutoAllow, "allowed"),
        (AcpApprovalMode::AutoReject, "rejected"),
        (AcpApprovalMode::Ask, "cancelled"),
    ];

    for (mode, expected_decision) in expectations {
        let temp = tempfile::tempdir().expect("approval workspace");
        let (mut hermes, mut env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
        hermes.acp = Some(AcpExecutionOptions {
            approval_mode: Some(mode),
            ..Default::default()
        });
        env.insert("OPENTEAMS_FAKE_HERMES_TOOL_CALL", "use-tool");

        let spawned = hermes
            .spawn(temp.path(), "use-tool", &env)
            .await
            .expect("spawn");
        let (events, exit) = finish_turn(spawned).await;
        assert!(
            matches!(exit, ExecutorExitResult::Success),
            "exit for {mode:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, AcpEvent::ToolCall(_))),
            "expected ToolCall event for {mode:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AcpEvent::RequestPermission(_))),
            "expected RequestPermission event for {mode:?}"
        );

        let permission_log = temp.path().join("permissions.jsonl");
        assert!(
            permission_log.exists(),
            "permission log must exist for {mode:?}"
        );
        let log_content = fs::read_to_string(&permission_log).expect("permission log");
        assert!(
            log_content.contains(expected_decision),
            "permission log should contain '{expected_decision}' for {mode:?}: {log_content}"
        );
        assert!(
            events.iter().any(|e| matches!(e, AcpEvent::Done(_))),
            "expected Done event for {mode:?}"
        );
    }
}

#[tokio::test]
async fn offline_hermes_native_and_mcp_tools_verify_three_approval_policies() {
    use executors::executors::acp::AcpApprovalMode;

    let cases = [
        ("native", "OPENTEAMS_FAKE_HERMES_TOOL_CALL", "bash"),
        (
            "mcp",
            "OPENTEAMS_FAKE_HERMES_MCP_TOOL_CALL",
            "mcp__test__read",
        ),
    ];

    let policies = [
        (AcpApprovalMode::AutoAllow, "allowed"),
        (AcpApprovalMode::AutoReject, "rejected"),
        (AcpApprovalMode::Ask, "cancelled"),
    ];

    for (tool_kind, trigger_env, expected_tool) in cases {
        for (mode, expected_decision) in policies {
            let temp = tempfile::tempdir().expect("approval workspace");
            let (mut hermes, mut env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
            hermes.acp = Some(AcpExecutionOptions {
                approval_mode: Some(mode),
                ..Default::default()
            });
            env.insert(trigger_env, "use-tool");

            let spawned = hermes
                .spawn(temp.path(), "use-tool", &env)
                .await
                .expect("spawn");
            let (events, exit) = finish_turn(spawned).await;
            assert!(
                matches!(exit, ExecutorExitResult::Success),
                "exit for {tool_kind}/{mode:?}"
            );
            assert!(
                events.iter().any(|e| matches!(e, AcpEvent::ToolCall(_))),
                "expected ToolCall event for {tool_kind}/{mode:?}"
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, AcpEvent::RequestPermission(_))),
                "expected RequestPermission event for {tool_kind}/{mode:?}"
            );

            let permission_log = temp.path().join("permissions.jsonl");
            let log_content =
                fs::read_to_string(&permission_log).expect("permission log must exist");
            assert!(
                log_content.contains(expected_tool),
                "permission log must record tool '{expected_tool}' for {tool_kind}/{mode:?}: {log_content}"
            );
            assert!(
                log_content.contains(expected_decision),
                "permission log must contain '{expected_decision}' for {tool_kind}/{mode:?}: {log_content}"
            );
            assert!(
                events.iter().any(|e| matches!(e, AcpEvent::Done(_))),
                "expected Done event for {tool_kind}/{mode:?}"
            );
        }
    }
}

#[test]
fn offline_hermes_mcp_snapshot_filters_unauthorized_and_disables_vendor_controls() {
    let canonical = serde_json::json!({
        "mcpServers": {
            "authorized": {"command": "/bin/echo", "env": {"TOKEN": "fixture-token"}},
            "unauthorized": {"command": "/bin/echo", "env": {"KEY": "fixture-key"}}
        },
        "settings": {"hostConfigDiscovery": "off"}
    });
    let policy = AcpMcpPolicy {
        allowed_server_names: Some(["authorized".to_string()].into_iter().collect()),
        disabled_server_names: Default::default(),
    };
    let snapshot = resolve_isolated_mcp_snapshot(&canonical, &policy).expect("snapshot");
    let servers = snapshot.get("mcpServers").unwrap().as_object().unwrap();
    assert!(
        servers.contains_key("authorized"),
        "authorized server must be present"
    );
    assert!(
        !servers.contains_key("unauthorized"),
        "unauthorized server must be filtered out"
    );
    assert_eq!(
        snapshot
            .get("settings")
            .and_then(|s| s.get("hostConfigDiscovery"))
            .and_then(|v| v.as_str()),
        Some("off"),
        "hostConfigDiscovery must be off"
    );
}

#[test]
fn offline_hermes_empty_mcp_allowlist_produces_empty_snapshot() {
    let canonical = serde_json::json!({
        "mcpServers": {
            "server1": {"command": "/bin/echo"},
            "server2": {"command": "/bin/echo"}
        },
        "settings": {"hostConfigDiscovery": "off"}
    });
    let policy = AcpMcpPolicy {
        allowed_server_names: Some(Default::default()),
        disabled_server_names: Default::default(),
    };
    let snapshot = resolve_isolated_mcp_snapshot(&canonical, &policy).expect("snapshot");
    let servers = snapshot
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .expect("servers");
    assert!(
        servers.is_empty(),
        "empty allowlist must produce empty snapshot"
    );
}

#[test]
fn offline_hermes_two_members_have_different_mcp_snapshots() {
    let secret = "hermes-member-secret-never-leak";
    let canonical = serde_json::json!({
        "mcpServers": {
            "alpha": {"command": "/bin/echo", "env": {"TOKEN": secret}},
            "beta": {"command": "/bin/echo", "env": {"KEY": secret}},
            "gamma": {"command": "/bin/echo"}
        },
        "settings": {"hostConfigDiscovery": "off"}
    });

    let policy_a = AcpMcpPolicy {
        allowed_server_names: Some(["alpha".to_string()].into_iter().collect()),
        disabled_server_names: Default::default(),
    };
    let snapshot_a = resolve_isolated_mcp_snapshot(&canonical, &policy_a).expect("snapshot A");
    let servers_a = snapshot_a.get("mcpServers").unwrap().as_object().unwrap();
    assert!(
        servers_a.contains_key("alpha"),
        "member A should have alpha"
    );
    assert!(
        !servers_a.contains_key("beta"),
        "member A should NOT have beta"
    );
    assert!(
        !servers_a.contains_key("gamma"),
        "member A should NOT have gamma"
    );

    let policy_b = AcpMcpPolicy {
        allowed_server_names: Some(["beta".to_string()].into_iter().collect()),
        disabled_server_names: Default::default(),
    };
    let snapshot_b = resolve_isolated_mcp_snapshot(&canonical, &policy_b).expect("snapshot B");
    let servers_b = snapshot_b.get("mcpServers").unwrap().as_object().unwrap();
    assert!(servers_b.contains_key("beta"), "member B should have beta");
    assert!(
        !servers_b.contains_key("alpha"),
        "member B should NOT have alpha"
    );
    assert!(
        !servers_b.contains_key("gamma"),
        "member B should NOT have gamma"
    );

    assert_ne!(
        snapshot_a.get("mcpServers"),
        snapshot_b.get("mcpServers"),
        "member snapshots must differ"
    );

    for snapshot in [&snapshot_a, &snapshot_b] {
        assert_eq!(
            snapshot
                .get("settings")
                .and_then(|s| s.get("hostConfigDiscovery"))
                .and_then(|v| v.as_str()),
            Some("off"),
            "hostConfigDiscovery must be off"
        );
    }
}

#[tokio::test]
async fn offline_hermes_protocol_log_records_all_methods() {
    let temp = tempfile::tempdir().expect("protocol workspace");
    let (hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);

    let spawned = hermes
        .spawn(temp.path(), "proto-test", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    let session_id = events
        .iter()
        .find_map(|e| match e {
            AcpEvent::SessionStart(id) => Some(id.clone()),
            _ => None,
        })
        .expect("session id");

    let fu = hermes
        .spawn_follow_up(temp.path(), "follow-up", &session_id, None, &env)
        .await
        .expect("follow-up");
    let (_, fu_exit) = finish_turn(fu).await;
    assert!(matches!(fu_exit, ExecutorExitResult::Success));

    let protocol_log = temp.path().join("protocol.jsonl");
    assert!(protocol_log.exists(), "protocol log must exist");
    let log = fs::read_to_string(&protocol_log).expect("protocol log");

    for method in [
        "initialize",
        "session/new",
        "session/prompt",
        "session/resume",
    ] {
        assert!(
            log.contains(method),
            "protocol log should record method '{method}': {log}"
        );
    }

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AcpEvent::SessionStart(_)))
    );
    assert!(events.iter().any(|e| matches!(e, AcpEvent::Message(_))));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::Usage(_))));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::TokenUsage(_))));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::Done(_))));
}

#[tokio::test]
async fn offline_hermes_config_override_applies_legacy_session_model() {
    use executors::executors::acp::{AcpConfigOverride, AcpConfigValue};

    let temp = tempfile::tempdir().expect("override workspace");
    let (mut hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);
    hermes.acp = Some(AcpExecutionOptions {
        config_overrides: Some(vec![AcpConfigOverride {
            option_id: "model".to_string(),
            value: AcpConfigValue::ValueId {
                value: "nous:hermes-flash".to_string(),
            },
            label_snapshot: Some("Model".to_string()),
            category_snapshot: Some("model".to_string()),
        }]),
        ..Default::default()
    });

    let spawned = hermes
        .spawn(temp.path(), "override-turn", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e,
        AcpEvent::Message(m) if format!("{m:?}").contains("echo:override-turn"))));
    let protocol_log = temp.path().join("protocol.jsonl");
    if protocol_log.exists() {
        let log = fs::read_to_string(&protocol_log).expect("protocol log");
        assert!(
            log.contains("session/set_model"),
            "config override must drive session/set_model: {log}"
        );
    }
}

#[tokio::test]
async fn offline_hermes_isolated_home_reads_per_test_mcp_config_not_host() {
    // Regression: the Hermes adapter resolves `~/.hermes/config.yaml` via
    // `dirs::home_dir()`, which reads the *process* HOME. This test proves the
    // process HOME is isolated to the per-test temporary directory by writing a
    // marker MCP config there and verifying the adapter reads it (the spawn
    // fails with a parse error for the deliberately-broken server), while the
    // host machine's `~/.hermes/config.yaml` is never touched.
    let temp = tempfile::tempdir().expect("isolated home workspace");
    let env_info = install_offline_hermes_fixture(temp.path(), true);
    let home_guard = HomeIsolationGuard::acquire(&env_info.home);

    // Write a marker config into the isolated ~/.hermes/config.yaml. The server
    // command does not exist, so `parse_mcp_servers` returns an error. If the
    // adapter were reading the host config instead, this error would not
    // appear (the host config is either absent or different).
    let hermes_config_dir = env_info.home.join(".hermes");
    fs::create_dir_all(&hermes_config_dir).expect("hermes config dir");
    let marker_secret = "hermes-host-config-secret-never-leak";
    fs::write(
        hermes_config_dir.join("config.yaml"),
        format!(
            "mcp_servers:\n  marker-server:\n    command: openteams-marker-command-not-real\n    env:\n      TOKEN: {marker_secret}\n"
        ),
    )
    .expect("write marker mcp config");

    let mut hermes = Hermes::default();
    let hermes_path = env_info.bin.join("hermes");
    hermes.cmd.base_command_override = Some(hermes_path.display().to_string());
    let mut env = ExecutionEnv::new(
        RepoContext::new(temp.path().to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    env.insert(
        "PATH",
        format!(
            "{}:{}",
            env_info.bin.display(),
            std::env::var("PATH").unwrap_or_default(),
        ),
    );
    env.insert("HOME", env_info.home.to_string_lossy().to_string());

    let error = hermes
        .spawn(temp.path(), "isolation-check", &env)
        .await
        .expect_err("spawn must fail because the marker server command is not found");
    let error_string = error.to_string();

    // The adapter read the isolated config (proving HOME isolation works).
    assert!(
        error_string.contains("marker-server")
            || error_string.contains("openteams-marker-command-not-real")
            || error_string.contains("command was not found"),
        "spawn must surface the isolated marker-server MCP parse error: {error_string}"
    );
    // The sensitive token in the marker config must NOT leak into the error.
    assert!(
        !error_string.contains(marker_secret),
        "sensitive MCP env token must not leak into the spawn error: {error_string}"
    );

    drop(home_guard);
}

#[tokio::test]
async fn offline_hermes_spawn_succeeds_when_host_mcp_config_would_be_illegal() {
    // Regression: even if the *host* machine had an illegal `~/.hermes/config.yaml`,
    // the test must not read it because the process HOME is isolated. This test
    // proves the isolation by running a normal spawn under an isolated HOME
    // that has NO `~/.hermes/config.yaml`. If the adapter were reading the host
    // config, a host with an illegal config would break this test. Since the
    // isolated HOME is empty, the adapter falls back to the empty template and
    // the spawn succeeds.
    let temp = tempfile::tempdir().expect("empty home workspace");
    let (hermes, env, _, _home_guard) = make_hermes_and_env(temp.path(), true);

    // Verify the isolated home has no .hermes/config.yaml.
    assert!(
        !temp.path().join("home/.hermes/config.yaml").exists(),
        "isolated home must not have a .hermes/config.yaml by default"
    );

    let spawned = hermes
        .spawn(temp.path(), "empty-home-check", &env)
        .await
        .expect("spawn must succeed with empty isolated home");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e,
        AcpEvent::Message(m) if format!("{m:?}").contains("echo:empty-home-check"))));
}
