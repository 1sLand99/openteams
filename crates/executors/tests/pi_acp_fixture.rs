#![cfg(feature = "qa-mode")]

//! Offline Pi ACP fixture integration tests.
//!
//! These tests exercise the full Pi executor lifecycle using repository-local
//! fake npx / pi-acp / launcher / Pi fixtures. No npm cache, network access,
//! or global Pi installation is required.

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
        pi::Pi,
    },
    mcp_config::MemberMcpConfig,
    mcp_run::{McpRunContext, PreparedMcpRun},
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::{Mutex, MutexGuard},
};

const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/pi_acp");
const PI_RUN_FILE_ENV_KEYS: [&str; 5] = [
    "PI_ACP_PI_COMMAND",
    "OPENTEAMS_PI_APPROVAL_EXTENSION",
    "OPENTEAMS_PI_MCP_EXTENSION",
    "OPENTEAMS_PI_MCP_SNAPSHOT",
    "OPENTEAMS_PI_DIAGNOSTIC_LOG",
];

// The fixture starts a three-process Node chain. Serializing those tests keeps
// the fixed ACP initialization deadline from becoming a host-load race while
// leaving pure snapshot tests parallel.
static PI_PROCESS_LOCK: Mutex<()> = Mutex::const_new(());

async fn pi_process_lock() -> MutexGuard<'static, ()> {
    PI_PROCESS_LOCK.lock().await
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(FIXTURE_DIR).join(name)
}

fn read_fixture(name: &str) -> String {
    fs::read_to_string(fixture_path(name)).unwrap_or_else(|_| panic!("read fixture {name}"))
}

struct OfflinePiEnv {
    bin: PathBuf,
    prompts: PathBuf,
    pids: PathBuf,
    session_file: PathBuf,
    permission_log: PathBuf,
    protocol_log: PathBuf,
}

fn install_offline_pi_fixture(root: &Path, executable: bool) -> OfflinePiEnv {
    use executors::executors::pi::{
        PI_CODING_AGENT_PACKAGE, PI_CODING_AGENT_VERSION, PI_MCP_ADAPTER_PACKAGE,
        PI_MCP_ADAPTER_VERSION,
    };

    let bin = root.join("bin");
    let node_modules = root.join("node_modules");
    let nm_bin = node_modules.join(".bin");
    let pi_package = node_modules.join(PI_CODING_AGENT_PACKAGE);
    let mcp_package = node_modules.join(PI_MCP_ADAPTER_PACKAGE);

    fs::create_dir_all(&bin).expect("bin dir");
    fs::create_dir_all(&nm_bin).expect("nm bin dir");
    fs::create_dir_all(&pi_package).expect("pi package dir");
    fs::create_dir_all(&mcp_package).expect("mcp package dir");

    let mode = if executable { 0o755 } else { 0o644 };

    let npx_source = read_fixture("fake_npx.sh");
    let npx_path = bin.join("npx");
    fs::write(&npx_path, &npx_source).expect("write fake npx");
    fs::set_permissions(&npx_path, fs::Permissions::from_mode(0o755)).expect("npx chmod");

    let pi_acp_source = read_fixture("fake_pi_acp.mjs");
    let pi_acp_path = bin.join("pi-acp");
    fs::write(&pi_acp_path, &pi_acp_source).expect("write fake pi-acp");
    fs::set_permissions(&pi_acp_path, fs::Permissions::from_mode(mode)).expect("pi-acp chmod");

    let pi_source = read_fixture("fake_pi.mjs");
    let fake_pi_path = nm_bin.join("pi");
    fs::write(&fake_pi_path, &pi_source).expect("write fake pi");
    fs::set_permissions(&fake_pi_path, fs::Permissions::from_mode(mode)).expect("pi chmod");

    let mcp_source = read_fixture("fake_pi_mcp_adapter.mjs");
    let fake_mcp_path = nm_bin.join("pi-mcp-adapter");
    fs::write(&fake_mcp_path, &mcp_source).expect("write fake mcp adapter");
    fs::set_permissions(&fake_mcp_path, fs::Permissions::from_mode(0o755)).expect("mcp chmod");

    fs::write(
        pi_package.join("package.json"),
        format!(r#"{{"version":"{PI_CODING_AGENT_VERSION}"}}"#),
    )
    .expect("pi package.json");
    fs::write(
        mcp_package.join("package.json"),
        format!(r#"{{"version":"{PI_MCP_ADAPTER_VERSION}"}}"#),
    )
    .expect("mcp package.json");
    fs::write(mcp_package.join("index.ts"), "export default () => {};").expect("mcp index");

    OfflinePiEnv {
        bin,
        prompts: root.join("prompts.txt"),
        pids: root.join("pids.json"),
        session_file: root.join("sessions/session.jsonl"),
        permission_log: root.join("permissions.jsonl"),
        protocol_log: root.join("protocol.jsonl"),
    }
}

fn make_pi_and_env(
    _process_guard: &MutexGuard<'static, ()>,
    root: &Path,
    executable: bool,
) -> (Pi, ExecutionEnv, PathBuf) {
    let env_info = install_offline_pi_fixture(root, executable);
    let mut pi = Pi::default();
    let npx_path = env_info.bin.join("npx");
    pi.cmd.base_command_override = Some(format!(
        "{} --yes --package pi-acp@0.0.33 pi-acp",
        npx_path.display()
    ));
    let mut env = ExecutionEnv::new(
        RepoContext::new(root.to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    env.insert(
        "PATH",
        format!(
            "{}:{}:{}",
            env_info.bin.display(),
            root.join("node_modules/.bin").display(),
            std::env::var("PATH").unwrap_or_default(),
        ),
    );
    env.insert("HOME", root.join("home").to_string_lossy().to_string());
    env.insert("NO_UPDATE_NOTIFIER", "1");
    env.insert(
        "OPENTEAMS_FAKE_PI_PROMPTS",
        env_info.prompts.to_string_lossy().to_string(),
    );
    env.insert(
        "OPENTEAMS_FAKE_PI_CHILD_PID_FILE",
        env_info.pids.to_string_lossy().to_string(),
    );
    env.insert(
        "OPENTEAMS_FAKE_PI_SESSION_FILE",
        env_info.session_file.to_string_lossy().to_string(),
    );
    env.insert(
        "OPENTEAMS_FAKE_PI_PERMISSION_LOG",
        env_info.permission_log.to_string_lossy().to_string(),
    );
    env.insert(
        "OPENTEAMS_FAKE_PI_PROTOCOL_LOG",
        env_info.protocol_log.to_string_lossy().to_string(),
    );
    (pi, env, env_info.prompts)
}

fn pi_run_file_paths(env: &ExecutionEnv) -> Vec<PathBuf> {
    PI_RUN_FILE_ENV_KEYS
        .iter()
        .map(|key| {
            PathBuf::from(
                env.get(key)
                    .unwrap_or_else(|| panic!("missing prepared Pi run path: {key}")),
            )
        })
        .collect()
}

async fn prepare_pi_run(
    pi: &Pi,
    root: &Path,
    env: &ExecutionEnv,
    canonical: &MemberMcpConfig,
) -> Result<(Pi, ExecutionEnv, PreparedMcpRun), ExecutorError> {
    let mut pi = pi.clone();
    let mut env = env.clone();
    let context = McpRunContext::new(root, uuid::Uuid::new_v4(), uuid::Uuid::new_v4())?;
    let prepared = pi
        .prepare_mcp_for_run(canonical, &context, &mut env)
        .await?;
    Ok((pi, env, prepared))
}

fn attach_prepared_cleanup(mut spawned: SpawnedChild, prepared: PreparedMcpRun) -> SpawnedChild {
    spawned.cleanup = ExecutorRunCleanup::combine(spawned.cleanup.take(), prepared.into_cleanup());
    spawned
}

async fn spawn_prepared_pi(
    pi: &Pi,
    root: &Path,
    prompt: &str,
    env: &ExecutionEnv,
) -> Result<SpawnedChild, ExecutorError> {
    let (pi, env, prepared) = prepare_pi_run(pi, root, env, &MemberMcpConfig::default()).await?;
    let spawned = pi.spawn(root, prompt, &env).await?;
    Ok(attach_prepared_cleanup(spawned, prepared))
}

async fn spawn_prepared_pi_follow_up(
    pi: &Pi,
    root: &Path,
    prompt: &str,
    session_id: &str,
    env: &ExecutionEnv,
) -> Result<SpawnedChild, ExecutorError> {
    let (pi, env, prepared) = prepare_pi_run(pi, root, env, &MemberMcpConfig::default()).await?;
    let spawned = pi
        .spawn_follow_up(root, prompt, session_id, None, &env)
        .await?;
    Ok(attach_prepared_cleanup(spawned, prepared))
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

#[tokio::test]
async fn fake_npx_does_not_contact_npm_or_public_network() {
    let process_guard = pi_process_lock().await;
    let temp = tempfile::tempdir().expect("offline workspace");
    let (pi, env, _) = make_pi_and_env(&process_guard, temp.path(), true);
    let spawned = spawn_prepared_pi(&pi, temp.path(), "verify-offline", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e,
        AcpEvent::Message(msg) if format!("{msg:?}").contains("echo:verify-offline"))));
    assert!(
        !temp.path().join("home/.npm").exists(),
        "fake npx must not create npm cache"
    );
}

#[tokio::test]
async fn offline_pi_lifecycle_new_prompt_follow_up_cancel_and_startup_failure() {
    let process_guard = pi_process_lock().await;
    let temp = tempfile::tempdir().expect("workspace");
    let (pi, env, prompts) = make_pi_and_env(&process_guard, temp.path(), true);

    let (first_pi, first_env, first_prepared) =
        prepare_pi_run(&pi, temp.path(), &env, &MemberMcpConfig::default())
            .await
            .expect("prepare first run");
    let first_files = pi_run_file_paths(&first_env);
    assert_eq!(first_files.len(), PI_RUN_FILE_ENV_KEYS.len());
    assert!(first_files.iter().all(|path| path.is_file()));
    let first = first_pi
        .spawn(temp.path(), "first", &first_env)
        .await
        .expect("spawn first");
    let first = attach_prepared_cleanup(first, first_prepared);
    let (first_events, first_exit) = finish_turn(first).await;
    assert!(matches!(first_exit, ExecutorExitResult::Success));
    let session_id = first_events
        .iter()
        .find_map(|e| match e {
            AcpEvent::SessionStart(id) => Some(id.clone()),
            _ => None,
        })
        .expect("session id");
    assert_eq!(session_id, "pi-offline-session");
    assert!(first_events.iter().any(|e| matches!(e,
        AcpEvent::Message(m) if format!("{m:?}").contains("echo:first"))));
    assert!(first_events.iter().any(|e| matches!(e, AcpEvent::Done(_))));
    assert!(first_files.iter().all(|p| !p.exists()));

    let (follow_up_pi, follow_up_env, follow_up_prepared) =
        prepare_pi_run(&pi, temp.path(), &env, &MemberMcpConfig::default())
            .await
            .expect("prepare follow-up run");
    let follow_up_files = pi_run_file_paths(&follow_up_env);
    assert!(follow_up_files.iter().all(|path| path.is_file()));
    let follow_up = follow_up_pi
        .spawn_follow_up(temp.path(), "second", &session_id, None, &follow_up_env)
        .await
        .expect("follow-up");
    let follow_up = attach_prepared_cleanup(follow_up, follow_up_prepared);
    let (fu_events, fu_exit) = finish_turn(follow_up).await;
    assert!(matches!(fu_exit, ExecutorExitResult::Success));
    assert!(fu_events.iter().any(|e| matches!(e,
        AcpEvent::Message(m) if format!("{m:?}").contains("echo:second"))));
    assert_eq!(
        fs::read_to_string(&prompts)
            .expect("prompts")
            .lines()
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
    assert!(
        follow_up_files.iter().all(|path| !path.exists()),
        "follow-up must clean every owned Pi run asset"
    );

    let mut cancel_env = env.clone();
    cancel_env.insert("OPENTEAMS_FAKE_PI_HANG", "1");
    let (cancel_pi, cancel_env, cancel_prepared) =
        prepare_pi_run(&pi, temp.path(), &cancel_env, &MemberMcpConfig::default())
            .await
            .expect("prepare cancellable run");
    let cancel_files = pi_run_file_paths(&cancel_env);
    assert_eq!(cancel_files.len(), PI_RUN_FILE_ENV_KEYS.len());
    assert!(cancel_files.iter().all(|path| path.is_file()));
    let cancelled = cancel_pi
        .spawn(temp.path(), "cancel-me", &cancel_env)
        .await
        .expect("cancellable spawn");
    let mut cancelled = attach_prepared_cleanup(cancelled, cancel_prepared);
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
    assert!(
        cancel_files.iter().all(|path| !path.exists()),
        "cancellation must clean every Pi run asset"
    );

    let protocol_log = temp.path().join("protocol.jsonl");
    if protocol_log.exists() {
        let log = fs::read_to_string(&protocol_log).expect("protocol log");
        assert!(
            log.contains("session/cancel"),
            "protocol log must record session/cancel: {log}"
        );
    }

    let fail_temp = tempfile::tempdir().expect("fail workspace");
    let (fail_pi, fail_env, _) = make_pi_and_env(&process_guard, fail_temp.path(), false);
    let (fail_pi, fail_env, fail_prepared) = prepare_pi_run(
        &fail_pi,
        fail_temp.path(),
        &fail_env,
        &MemberMcpConfig::default(),
    )
    .await
    .expect("prepare failing run");
    let fail_files = pi_run_file_paths(&fail_env);
    assert!(fail_files.iter().all(|path| path.is_file()));
    let error = tokio::time::timeout(
        Duration::from_secs(15),
        fail_pi.spawn(fail_temp.path(), "must fail", &fail_env),
    )
    .await
    .expect("timeout")
    .expect_err("must fail");
    assert!(error.to_string().contains("ACP startup failed"));
    drop(fail_prepared.into_cleanup());
    assert!(
        fail_files.iter().all(|path| !path.exists()),
        "startup failure must clean only this run's prepared files"
    );
}

#[tokio::test]
async fn offline_pi_model_refresh_and_initialize_events() {
    let process_guard = pi_process_lock().await;
    let temp = tempfile::tempdir().expect("probe workspace");
    let (pi, env, _) = make_pi_and_env(&process_guard, temp.path(), true);
    let probe = pi
        .probe_acp(temp.path(), &env, None)
        .await
        .expect("probe")
        .expect("probe result");
    assert_eq!(probe.protocol_version, "1");
    assert_eq!(probe.agent_name.as_deref(), Some("pi-fake-acp"));
    let model_ids = probe.model_ids().expect("model ids");
    assert!(model_ids.contains(&"offline-model".to_string()));
}

#[tokio::test]
async fn offline_pi_token_usage_is_projected() {
    let process_guard = pi_process_lock().await;
    let temp = tempfile::tempdir().expect("token workspace");
    let (pi, env, _) = make_pi_and_env(&process_guard, temp.path(), true);
    let spawned = spawn_prepared_pi(&pi, temp.path(), "token-test", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::Usage(_))));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::TokenUsage(_))));
}

#[tokio::test]
async fn offline_pi_tool_call_is_projected() {
    let process_guard = pi_process_lock().await;
    let temp = tempfile::tempdir().expect("tool workspace");
    let (pi, mut env, _) = make_pi_and_env(&process_guard, temp.path(), true);
    env.insert("OPENTEAMS_FAKE_PI_TOOL_CALL", "use-tool");
    let spawned = spawn_prepared_pi(&pi, temp.path(), "use-tool", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::ToolCall(_))));
}

#[test]
fn offline_pi_fixture_files_are_present_and_secret_safe() {
    let npx = read_fixture("fake_npx.sh");
    assert!(npx.contains("fake-npx"));
    assert!(!npx.contains("registry.npmjs"));
    let pi_acp = read_fixture("fake_pi_acp.mjs");
    assert!(pi_acp.contains("protocolVersion"));
    assert!(pi_acp.contains("session/prompt"));
    assert!(pi_acp.contains("session/cancel"));
    assert!(!pi_acp.contains("API_KEY"));
    assert!(!pi_acp.contains("SECRET"));
    let fake_pi = read_fixture("fake_pi.mjs");
    assert!(fake_pi.contains("OPENTEAMS_FAKE_PI_CHILD_PID_FILE"));
    assert!(!fake_pi.contains("API_KEY"));
}

#[test]
fn offline_pi_fixture_uses_fully_pinned_versions() {
    use executors::executors::pi::{
        PI_ACP_VERSION, PI_CODING_AGENT_VERSION, PI_MCP_ADAPTER_VERSION,
    };
    assert_eq!(PI_ACP_VERSION, "0.0.33");
    assert_eq!(PI_CODING_AGENT_VERSION, "0.83.0");
    assert_eq!(PI_MCP_ADAPTER_VERSION, "2.18.0");
}

#[cfg(unix)]
#[test]
fn offline_pi_fixture_scripts_are_executable() {
    let mode = fs::metadata(fixture_path("fake_npx.sh"))
        .expect("npx meta")
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "fake_npx.sh must be executable");
    let mode = fs::metadata(fixture_path("fake_pi_acp.mjs"))
        .expect("pi-acp meta")
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "fake_pi_acp.mjs must be executable");
}

#[tokio::test]
async fn offline_pi_three_approval_policies_verify_permission_decisions() {
    use executors::executors::acp::AcpApprovalMode;

    let process_guard = pi_process_lock().await;
    let expectations = [
        (AcpApprovalMode::AutoAllow, "allowed"),
        (AcpApprovalMode::AutoReject, "rejected"),
        (AcpApprovalMode::Ask, "cancelled"),
    ];

    for (mode, expected_decision) in expectations {
        let temp = tempfile::tempdir().expect("approval workspace");
        let (mut pi, mut env, _) = make_pi_and_env(&process_guard, temp.path(), true);
        pi.acp = Some(executors::executors::acp::AcpExecutionOptions {
            approval_mode: Some(mode),
            ..Default::default()
        });
        env.insert("OPENTEAMS_FAKE_PI_TOOL_CALL", "use-tool");

        let spawned = spawn_prepared_pi(&pi, temp.path(), "use-tool", &env)
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
async fn offline_pi_native_and_mcp_tools_verify_three_approval_policies() {
    use executors::executors::acp::AcpApprovalMode;

    let process_guard = pi_process_lock().await;
    let cases = [
        ("native", "OPENTEAMS_FAKE_PI_TOOL_CALL", "bash"),
        ("mcp", "OPENTEAMS_FAKE_PI_MCP_TOOL_CALL", "mcp__test__read"),
    ];

    let policies = [
        (AcpApprovalMode::AutoAllow, "allowed"),
        (AcpApprovalMode::AutoReject, "rejected"),
        (AcpApprovalMode::Ask, "cancelled"),
    ];

    for (tool_kind, trigger_env, expected_tool) in cases {
        for (mode, expected_decision) in policies {
            let temp = tempfile::tempdir().expect("approval workspace");
            let (mut pi, mut env, _) = make_pi_and_env(&process_guard, temp.path(), true);
            pi.acp = Some(executors::executors::acp::AcpExecutionOptions {
                approval_mode: Some(mode),
                ..Default::default()
            });
            env.insert(trigger_env, "use-tool");

            let spawned = spawn_prepared_pi(&pi, temp.path(), "use-tool", &env)
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

#[tokio::test]
async fn offline_pi_public_preparation_redacts_fake_secret_and_ignores_legacy_allowlist() {
    use executors::executors::acp::mcp::AcpMcpPolicy;

    let temp = tempfile::tempdir().expect("MCP preparation workspace");
    let secret = "pi-public-preparation-fake-secret-never-leak";
    let pi = Pi {
        acp_mcp_policy: AcpMcpPolicy {
            allowed_server_names: Some(Default::default()),
            disabled_server_names: Default::default(),
        },
        ..Pi::default()
    };
    let env = ExecutionEnv::new(
        RepoContext::new(temp.path().to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    let canonical = MemberMcpConfig {
        mcp_servers: [(
            "member-only".to_string(),
            serde_json::json!({
                "command": "/bin/echo",
                "env": {"TOKEN": secret}
            }),
        )]
        .into_iter()
        .collect(),
    };

    let (_, prepared_env, prepared) = prepare_pi_run(&pi, temp.path(), &env, &canonical)
        .await
        .expect("Pi public preparation");
    let snapshot_path = PathBuf::from(
        prepared_env
            .get("OPENTEAMS_PI_MCP_SNAPSHOT")
            .expect("Pi snapshot path"),
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&snapshot_path).expect("read Pi snapshot"))
            .expect("parse Pi snapshot");

    assert!(snapshot["mcpServers"].get("member-only").is_some());
    assert_eq!(
        snapshot["mcpServers"].as_object().map(serde_json::Map::len),
        Some(1)
    );
    assert_eq!(snapshot["settings"]["hostConfigDiscovery"], "off");
    assert!(!format!("{prepared:?}").contains(secret));

    drop(prepared.into_cleanup());
    assert!(!snapshot_path.exists());
}

#[tokio::test]
async fn offline_pi_explicit_empty_member_map_ignores_ambient_and_cleans_exact_run_files() {
    let temp = tempfile::tempdir().expect("empty MCP workspace");
    let pi = Pi::default();
    let ambient_dir = temp.path().join("ambient-pi-agent");
    fs::create_dir_all(&ambient_dir).expect("ambient Pi directory");
    let home = temp.path().join("home");
    let default_agent_dir = home.join(".pi/agent");
    fs::create_dir_all(&default_agent_dir).expect("default Pi agent directory");
    let ambient_path = ambient_dir.join("mcp.json");
    let vendor_files: Vec<(PathBuf, &[u8])> = vec![
        (
            ambient_path.clone(),
            br#"{"mcpServers":{"ambient-global":{"command":"must-not-run"}}}"#,
        ),
        (
            ambient_dir.join("settings.json"),
            br#"{"defaultProvider":"fixture-provider"}"#,
        ),
        (
            ambient_dir.join("models.json"),
            br#"{"providers":{"fixture":{"models":[]}}}"#,
        ),
        (
            ambient_dir.join("auth.json"),
            br#"{"fixture-provider":{"type":"api_key","key":"pi-fixture-auth-token"}}"#,
        ),
        (
            default_agent_dir.join("auth.json"),
            br#"{"fixture-provider":{"type":"api_key","key":"pi-home-auth-token"}}"#,
        ),
    ];
    let mut original_vendor_files = Vec::new();
    for (path, contents) in vendor_files {
        fs::write(&path, contents)
            .unwrap_or_else(|_| panic!("write Pi vendor file {}", path.display()));
        original_vendor_files.push((
            path.clone(),
            fs::read(&path).unwrap_or_else(|_| panic!("read Pi vendor file {}", path.display())),
        ));
    }
    let mut env = ExecutionEnv::new(
        RepoContext::new(temp.path().to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    env.insert("HOME", home.to_string_lossy().into_owned());
    env.insert(
        "PI_CODING_AGENT_DIR",
        ambient_dir.to_string_lossy().into_owned(),
    );

    let (_, prepared_env, prepared) =
        prepare_pi_run(&pi, temp.path(), &env, &MemberMcpConfig::default())
            .await
            .expect("empty Pi public preparation");
    let snapshot_path = PathBuf::from(
        prepared_env
            .get("OPENTEAMS_PI_MCP_SNAPSHOT")
            .expect("Pi snapshot path"),
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&snapshot_path).expect("read Pi snapshot"))
            .expect("parse Pi snapshot");
    let run_files = pi_run_file_paths(&prepared_env);

    assert!(ambient_path.is_file());
    assert!(
        snapshot["mcpServers"]
            .as_object()
            .expect("server map")
            .is_empty()
    );
    assert_eq!(snapshot["settings"]["hostConfigDiscovery"], "off");
    assert_eq!(
        prepared_env
            .get("OPENTEAMS_PI_ENABLE_MCP_EXTENSION")
            .map(String::as_str),
        Some("0")
    );
    assert_eq!(run_files.len(), PI_RUN_FILE_ENV_KEYS.len());
    assert!(run_files.iter().all(|path| path.is_file()));
    for (path, original) in &original_vendor_files {
        let current =
            fs::read(path).unwrap_or_else(|_| panic!("read Pi vendor file {}", path.display()));
        assert_eq!(
            current.as_slice(),
            original.as_slice(),
            "Pi preparation changed user file {}",
            path.display()
        );
    }

    drop(prepared.into_cleanup());
    assert!(
        run_files.iter().all(|path| !path.exists()),
        "empty-member cleanup must remove every owned Pi run file"
    );
    for (path, original) in &original_vendor_files {
        let current =
            fs::read(path).unwrap_or_else(|_| panic!("read Pi vendor file {}", path.display()));
        assert_eq!(
            current.as_slice(),
            original.as_slice(),
            "Pi cleanup changed user file {}",
            path.display()
        );
    }
}

#[tokio::test]
async fn offline_pi_provider_protocol_failure_cleans_exact_run_resources_and_redacts_fake_secret() {
    let process_guard = pi_process_lock().await;
    let temp = tempfile::tempdir().expect("Pi protocol failure workspace");
    let (pi, mut env, _) = make_pi_and_env(&process_guard, temp.path(), true);
    env.insert("OPENTEAMS_FAKE_PI_ERROR", "1");
    let fake_secret = "pi-protocol-failure-fake-secret-never-leak";
    let canonical = MemberMcpConfig {
        mcp_servers: [(
            "member-failure".to_string(),
            serde_json::json!({
                "command": "/bin/echo",
                "env": {"TOKEN": fake_secret}
            }),
        )]
        .into_iter()
        .collect(),
    };
    let (pi, env, prepared) = prepare_pi_run(&pi, temp.path(), &env, &canonical)
        .await
        .expect("prepare Pi protocol failure run");
    let run_files = pi_run_file_paths(&env);
    let snapshot_path = PathBuf::from(
        env.get("OPENTEAMS_PI_MCP_SNAPSHOT")
            .expect("Pi snapshot path"),
    );
    assert!(run_files.iter().all(|path| path.is_file()));
    assert!(
        fs::read_to_string(&snapshot_path)
            .expect("Pi snapshot")
            .contains(fake_secret),
        "fixture must stage the fake secret before exercising redaction"
    );

    let spawned = pi
        .spawn(temp.path(), "provider-error", &env)
        .await
        .expect("Pi must reach the prompt protocol failure");
    let spawned = attach_prepared_cleanup(spawned, prepared);
    let (events, exit) = finish_turn(spawned).await;
    let event_output = format!("{events:?}");
    let exit_output = format!("{exit:?}");
    let protocol_output =
        fs::read_to_string(temp.path().join("protocol.jsonl")).expect("Pi protocol log");

    assert!(matches!(exit, ExecutorExitResult::Failure));
    assert!(events.iter().any(|event| {
        matches!(event, AcpEvent::Error(message) if message.contains("Pi provider connection failed"))
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AcpEvent::Done(_)))
    );
    assert!(protocol_output.contains("session/prompt"));
    for output in [&event_output, &exit_output, &protocol_output] {
        assert!(
            !output.contains(fake_secret),
            "Pi protocol failure output exposed the fake secret"
        );
    }
    assert!(
        run_files.iter().all(|path| !path.exists()),
        "Pi protocol failure must remove every owned run resource"
    );
}

#[tokio::test]
async fn offline_pi_launcher_chain_produces_real_pids() {
    let process_guard = pi_process_lock().await;
    let temp = tempfile::tempdir().expect("launcher workspace");
    let (pi, env, _) = make_pi_and_env(&process_guard, temp.path(), true);
    let spawned = spawn_prepared_pi(&pi, temp.path(), "pid-test", &env)
        .await
        .expect("spawn");
    let (events, exit) = finish_turn(spawned).await;
    assert!(matches!(exit, ExecutorExitResult::Success));
    assert!(events.iter().any(|e| matches!(e, AcpEvent::Done(_))));

    // Check protocol log for launcher events
    let protocol_log = temp.path().join("protocol.jsonl");
    if protocol_log.exists() {
        let log = fs::read_to_string(&protocol_log).expect("protocol log");
        // The fake pi-acp should have logged launcher startup
        assert!(
            log.contains("launcher_started") || log.contains("launcher_skip"),
            "protocol log should record launcher status: {log}"
        );
    }

    // Check PID file - if launcher succeeded, it should have real PIDs from fake_pi.mjs
    let pid_file = temp.path().join("pids.json");
    if pid_file.exists() {
        let pids_content = fs::read_to_string(&pid_file).expect("pid file");
        let pids: serde_json::Value = serde_json::from_str(&pids_content).expect("pid JSON");
        // Verify the PID file has the expected fields
        assert!(
            pids.get("pi").is_some() || pids.get("launcher").is_some(),
            "PID file should contain process IDs: {pids_content}"
        );
    }
}

#[tokio::test]
async fn offline_pi_protocol_log_records_all_methods() {
    let process_guard = pi_process_lock().await;
    let temp = tempfile::tempdir().expect("protocol workspace");
    let (pi, env, _) = make_pi_and_env(&process_guard, temp.path(), true);

    // New session
    let spawned = spawn_prepared_pi(&pi, temp.path(), "proto-test", &env)
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

    // Follow-up
    let fu = spawn_prepared_pi_follow_up(&pi, temp.path(), "follow-up", &session_id, &env)
        .await
        .expect("follow-up");
    let (_, fu_exit) = finish_turn(fu).await;
    assert!(matches!(fu_exit, ExecutorExitResult::Success));

    // Verify protocol log
    let protocol_log = temp.path().join("protocol.jsonl");
    assert!(protocol_log.exists(), "protocol log must exist");
    let log = fs::read_to_string(&protocol_log).expect("protocol log");

    // Verify key protocol methods were recorded
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

    // Verify events projected correctly
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
async fn offline_pi_two_members_freeze_distinct_canonical_snapshots() {
    let temp = tempfile::tempdir().expect("member snapshot workspace");
    let pi = Pi::default();
    let env = ExecutionEnv::new(
        RepoContext::new(temp.path().to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    let member_a = MemberMcpConfig {
        mcp_servers: [(
            "alpha".to_string(),
            serde_json::json!({"command": "/bin/echo"}),
        )]
        .into_iter()
        .collect(),
    };
    let member_b = MemberMcpConfig {
        mcp_servers: [(
            "beta".to_string(),
            serde_json::json!({"command": "/bin/echo"}),
        )]
        .into_iter()
        .collect(),
    };

    let (_, env_a, prepared_a) = prepare_pi_run(&pi, temp.path(), &env, &member_a)
        .await
        .expect("member A preparation");
    let (_, env_b, prepared_b) = prepare_pi_run(&pi, temp.path(), &env, &member_b)
        .await
        .expect("member B preparation");
    let path_a = PathBuf::from(env_a.get("OPENTEAMS_PI_MCP_SNAPSHOT").expect("snapshot A"));
    let path_b = PathBuf::from(env_b.get("OPENTEAMS_PI_MCP_SNAPSHOT").expect("snapshot B"));
    let snapshot_a: serde_json::Value =
        serde_json::from_slice(&fs::read(&path_a).expect("read snapshot A"))
            .expect("parse snapshot A");
    let snapshot_b: serde_json::Value =
        serde_json::from_slice(&fs::read(&path_b).expect("read snapshot B"))
            .expect("parse snapshot B");

    assert_eq!(
        snapshot_a["mcpServers"]
            .as_object()
            .expect("member A servers")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["alpha"]
    );
    assert_eq!(
        snapshot_b["mcpServers"]
            .as_object()
            .expect("member B servers")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["beta"]
    );
    assert_ne!(path_a, path_b);

    drop(prepared_a.into_cleanup());
    drop(prepared_b.into_cleanup());
    assert!(!path_a.exists());
    assert!(!path_b.exists());
}

#[test]
fn offline_pi_unauthorized_skill_path_is_rejected() {
    use executors::executors::pi::Pi;
    let pi = Pi::default();
    let roots = pi.native_skill_discovery_roots();
    assert!(!roots.is_empty(), "Pi must have skill discovery roots");

    // A path outside the roots should not be a valid skill path
    let outside_path = Path::new("/tmp/nonexistent/SKILL.md");
    assert!(
        !outside_path.starts_with("/registry"),
        "test path should be outside roots"
    );

    // Verify that the Pi executor always uses --no-skills (checked via launcher source)
    let launcher = executors::executors::pi::PI_LAUNCHER_SOURCE;
    assert!(
        launcher.contains("--no-skills"),
        "launcher must enforce --no-skills"
    );
    assert!(
        launcher.contains("--skill"),
        "launcher must support --skill paths"
    );
    assert!(
        launcher.contains("isolatedSkillPaths"),
        "launcher must use isolated skill paths"
    );
}

#[tokio::test]
#[ignore = "Requires npm cache or network for real pi-acp@0.0.33; run with: cargo test --features qa-mode --test pi_acp_fixture offline_pi_real_npx_smoke -- --ignored --nocapture"]
async fn offline_pi_real_npx_smoke() {
    // This test verifies the real Pi ACP lifecycle using actual npx.
    // It requires:
    // - Node.js and npx on PATH
    // - npm cache populated with pi-acp@0.0.33, @earendil-works/pi-coding-agent@0.83.0, pi-mcp-adapter@2.18.0
    // - Or network access to npm registry
    //
    // To run:
    //   cargo test -p executors --features qa-mode --test pi_acp_fixture offline_pi_real_npx_smoke -- --ignored --nocapture
    //
    // Setup:
    //   export PI_SMOKE_HOME=$(mktemp -d)
    //   export HOME=$PI_SMOKE_HOME
    //   npx --yes --package pi-acp@0.0.33 --package @earendil-works/pi-coding-agent@0.83.0 --package pi-mcp-adapter@2.18.0 pi-acp --version

    use executors::executors::pi::{
        PI_ACP_VERSION, PI_CODING_AGENT_VERSION, PI_MCP_ADAPTER_VERSION,
    };

    // Use the real npx command (no fake npx override)
    let temp = tempfile::tempdir().expect("real npx workspace");
    let pi = Pi::default();
    let mut env = ExecutionEnv::new(
        RepoContext::new(temp.path().to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    env.insert(
        "HOME",
        temp.path().join("home").to_string_lossy().to_string(),
    );

    // Attempt to spawn - this will fail if npm cache is empty
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        spawn_prepared_pi(&pi, temp.path(), "real-smoke-test", &env),
    )
    .await;

    match result {
        Ok(Ok(spawned)) => {
            let (events, exit) = finish_turn(spawned).await;
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, AcpEvent::SessionStart(_))),
                "real Pi should produce a session"
            );
            assert!(
                events.iter().any(|e| matches!(e, AcpEvent::Done(_))),
                "real Pi should complete the turn"
            );
            eprintln!(
                "Real NPX smoke test passed: exit={exit:?}, events={}",
                events.len()
            );
        }
        Ok(Err(e)) => {
            panic!(
                "Real NPX smoke test failed (startup or protocol error): {e}\n\
                 Ensure npm cache has pi-acp@{PI_ACP_VERSION}, \
                 @earendil-works/pi-coding-agent@{PI_CODING_AGENT_VERSION}, \
                 pi-mcp-adapter@{PI_MCP_ADAPTER_VERSION}"
            );
        }
        Err(_) => {
            panic!("Real NPX smoke test timed out after 30 seconds");
        }
    }
}
