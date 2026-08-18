#![cfg(feature = "qa-mode")]

//! Real DeepSeek Harness rc.7 checkout e2e regression tests.
//!
//! These tests exercise two production-level scenarios against the pinned
//! official source checkout (see `DEEPSEEK_HARNESS_REVISION`):
//!
//! 1. The same `DeepSeekHarnessAgent` mentioned twice consecutively must start
//!    a fresh ACP session on the second mention instead of failing with
//!    "Follow-up is not supported" (the preview ACP advertises neither
//!    session/resume nor session/load).
//! 2. Selecting `deepseek-v4-pro` / `deepseek-v4-flash` must drive the run
//!    through a cached Cordis include overlay that selects the model without
//!    mutating the official `cordis.yml`, and the overlay must boot the real
//!    rc.7 ACP server.
//!
//! The checkout path is read from `DEEPSEEK_HARNESS_CHECKOUT`, defaulting to
//! `~/deepseek-harness`. Tests skip when the checkout is absent so the suite
//! stays green in CI without a local checkout.

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use executors::{
    env::{ExecutionEnv, RepoContext},
    executors::{
        ExecutorError, SpawnedChild, StandardCodingAgentExecutor,
        acp::{AcpEvent, events::AcpRuntimeEvent},
        deepseek_harness::{DEEPSEEK_HARNESS_REVISION, DeepseekHarness},
    },
};
use tokio::io::{AsyncBufReadExt, BufReader};

const DEFAULT_CHECKOUT: &str = "deepseek-harness";
const SOURCE_CONFIG_RELATIVE_PATH: &str = "examples/acp-agent/cordis.yml";
const MODEL_CONFIG_CACHE_RELATIVE_PATH: &str =
    "examples/node_modules/.cache/openteams/deepseek-harness";

fn checkout_path() -> Option<PathBuf> {
    std::env::var_os("DEEPSEEK_HARNESS_CHECKOUT")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(DEFAULT_CHECKOUT)))
}

fn real_checkout() -> Option<PathBuf> {
    let checkout = checkout_path()?;
    if !checkout.join(SOURCE_CONFIG_RELATIVE_PATH).is_file() {
        return None;
    }
    Some(checkout)
}

fn harness(checkout: &Path) -> DeepseekHarness {
    DeepseekHarness {
        harness_path: Some(checkout.to_path_buf()),
        ..DeepseekHarness::default()
    }
}

fn env(workspace: &Path) -> ExecutionEnv {
    ExecutionEnv::new(
        RepoContext::new(workspace.to_path_buf(), Vec::new()),
        false,
        String::new(),
    )
}

fn is_follow_up_not_supported(error: &ExecutorError) -> bool {
    matches!(error, ExecutorError::FollowUpNotSupported(_))
}

/// Scenario 1: the same `DeepSeekHarnessAgent` mentioned twice consecutively.
///
/// The preview rc.7 ACP advertises neither session/resume nor session/load, so
/// the second mention must fall back to a fresh session. Even when no
/// `DEEPSEEK_API_KEY` is available (prompt fails on the real model call), the
/// error must never be `FollowUpNotSupported`.
#[tokio::test]
async fn real_rc7_double_mention_falls_back_to_fresh_sessions() {
    let Some(checkout) = real_checkout() else {
        eprintln!("SKIP: DeepSeek Harness checkout not found");
        return;
    };
    let harness = harness(&checkout);
    let workspace = tempfile::tempdir().expect("workspace");
    let env = env(workspace.path());
    let probe = harness
        .probe_acp(workspace.path(), &env, None)
        .await
        .expect("rc.7 initialize must succeed")
        .expect("probe result");
    assert_eq!(probe.protocol_version, "1");
    assert_eq!(probe.agent_name.as_deref(), Some("deepseek-harness-acp"));
    assert!(
        !probe.supports_session_resume,
        "rc.7 must not advertise session/resume"
    );
    assert!(
        !probe.supports_session_load,
        "rc.7 must not advertise session/load"
    );

    // First mention: a brand new session is requested.
    let first = match harness.spawn(workspace.path(), "first mention", &env).await {
        Ok(spawned) => spawned,
        Err(error) => {
            assert!(
                !is_follow_up_not_supported(&error),
                "first mention must not report follow-up not supported: {error}"
            );
            eprintln!("first mention stopped before model call (no API key): {error}");
            return;
        }
    };
    let first_session = read_session_start(first).await;

    // Second mention (consecutive): must fall back to a fresh session.
    let second = match harness
        .spawn_follow_up(
            workspace.path(),
            "second mention",
            &first_session,
            None,
            &env,
        )
        .await
    {
        Ok(spawned) => spawned,
        Err(error) => {
            assert!(
                !is_follow_up_not_supported(&error),
                "second mention must fall back to a fresh session, not report follow-up not supported: {error}"
            );
            eprintln!("second mention stopped before model call (no API key): {error}");
            return;
        }
    };
    let second_session = read_session_start(second).await;

    // Both mentions must have started real sessions, and the consecutive
    // mention must be a *fresh* session (never a resume/load of the first).
    assert_ne!(
        first_session, second_session,
        "consecutive mention must start a fresh session, not resume the first"
    );
    assert!(
        !second_session.is_empty(),
        "second mention must report a fresh session id"
    );
    eprintln!(
        "double mention created fresh sessions: first={first_session} second={second_session}"
    );

    // The structured follow-up path (used by the chat runtime) must behave the
    // same: a fresh session, never FollowUpNotSupported.
    let structured = match harness
        .spawn_follow_up_structured(
            workspace.path(),
            &executors::executors::ExecutorPrompt::text("third mention"),
            &second_session,
            None,
            &env,
        )
        .await
    {
        Ok(spawned) => spawned,
        Err(error) => {
            assert!(
                !is_follow_up_not_supported(&error),
                "structured follow-up must fall back to a fresh session: {error}"
            );
            eprintln!("structured follow-up stopped before model call (no API key): {error}");
            return;
        }
    };
    let structured_session = read_session_start(structured).await;
    assert!(
        !structured_session.is_empty() && structured_session != second_session,
        "structured follow-up must start yet another fresh session, got {structured_session}"
    );
    eprintln!("structured follow-up created fresh session: {structured_session}");

    // Fresh sessions must never persist under a resume/load key.
    let session_root = workspace
        .path()
        .join(".openteams/deepseek-harness/sessions");
    if session_root.exists() {
        for entry in fs::read_dir(&session_root).expect("session root") {
            let entry = entry.expect("session entry");
            assert_ne!(
                entry.file_name().to_string_lossy(),
                "stale-session",
                "follow-up must not reuse a stale session"
            );
        }
    }
}

/// Scenario 2: selecting `deepseek-v4-pro` and `deepseek-v4-flash` before
/// starting a run. Each selection must produce a cached Cordis include overlay
/// that boots the real rc.7 ACP server without mutating the official config.
#[tokio::test]
async fn real_rc7_each_model_selects_overlay_and_boots() {
    let Some(checkout) = real_checkout() else {
        eprintln!("SKIP: DeepSeek Harness checkout not found");
        return;
    };
    let workspace = tempfile::tempdir().expect("workspace");
    let env = env(workspace.path());
    let source_config = checkout.join(SOURCE_CONFIG_RELATIVE_PATH);
    let source_before = fs::read_to_string(&source_config).expect("read source cordis.yml");

    let available = harness(&checkout)
        .list_models(workspace.path(), &env)
        .await
        .expect("model discovery")
        .expect("models");
    assert_eq!(available, ["deepseek-v4-flash", "deepseek-v4-pro"]);

    for selected in ["deepseek-v4-flash", "deepseek-v4-pro"] {
        let mut harness = harness(&checkout);
        harness.model = Some(selected.to_string());

        // The effective runtime command must select the model through the
        // Cordis include overlay when it differs from the composition default,
        // and must never mutate the official cordis.yml.
        let diagnostics = harness
            .runtime_command_for_diagnostics()
            .expect("runtime diagnostics")
            .expect("runtime command");
        let rendered = diagnostics.redacted_display();
        assert!(
            rendered.contains("--config"),
            "runtime command must pass an ACP composition: {rendered}"
        );
        let effective_config = overlay_from_diagnostics(&rendered).expect("config path in command");
        let effective = fs::read_to_string(&effective_config).expect("effective config");
        if selected == "deepseek-v4-flash" {
            // Non-default model: a cached include overlay must select it.
            assert!(
                effective_config.starts_with(checkout.join(MODEL_CONFIG_CACHE_RELATIVE_PATH)),
                "flash must use a cached overlay, got: {}",
                effective_config.display()
            );
            assert!(
                effective.contains("model: 'deepseek-v4-flash'"),
                "overlay must select deepseek-v4-flash"
            );
            assert!(
                effective.contains("persistenceRoot: !!js"),
                "overlay must preserve tagged !!js values"
            );
        } else {
            // `deepseek-v4-pro` is the composition default; the source config
            // already selects it, so no overlay is required.
            assert_eq!(
                effective_config,
                checkout.join(SOURCE_CONFIG_RELATIVE_PATH),
                "deepseek-v4-pro is the default and must not need an overlay"
            );
            assert!(
                effective.contains("model: deepseek-v4-pro"),
                "effective config must select deepseek-v4-pro"
            );
        }

        // Official source config must remain untouched.
        assert_eq!(
            fs::read_to_string(&source_config).expect("read source cordis.yml"),
            source_before,
            "selection for {selected} must not mutate the official cordis.yml"
        );

        // The overlay must boot the real rc.7 ACP server.
        let probe = harness
            .probe_acp(workspace.path(), &env, None)
            .await
            .unwrap_or_else(|error| {
                panic!("rc.7 initialize with {selected} overlay failed: {error}")
            })
            .expect("probe result");
        assert_eq!(
            probe.agent_name.as_deref(),
            Some("deepseek-harness-acp"),
            "overlay for {selected} must boot the official ACP server"
        );
    }
}

/// The official rc.7 checkout must advertise its pinned revision and expose
/// the two composition models through the ACP probe interpretation.
#[tokio::test]
async fn real_rc7_revision_and_probe_models() {
    let Some(checkout) = real_checkout() else {
        eprintln!("SKIP: DeepSeek Harness checkout not found");
        return;
    };
    let harness = harness(&checkout);
    let workspace = tempfile::tempdir().expect("workspace");
    let env = env(workspace.path());

    let revision = git_revision(&checkout);
    assert_eq!(
        revision.as_deref(),
        Some(DEEPSEEK_HARNESS_REVISION),
        "checkout must be the pinned rc.7 revision"
    );

    let probe = harness
        .probe_acp(workspace.path(), &env, None)
        .await
        .expect("rc.7 initialize")
        .expect("probe result");
    let interpretation = harness.interpret_acp_probe(&probe);
    assert_eq!(
        interpretation.models.as_deref(),
        Some(
            &[
                "deepseek-v4-flash".to_string(),
                "deepseek-v4-pro".to_string()
            ][..]
        )
    );
}

/// Read the first `SessionStart` event id from a spawned ACP child, then drop
/// the child. Bounded so a prompt that stalls on a missing API key cannot hang
/// the test.
async fn read_session_start(mut spawned: SpawnedChild) -> String {
    let stdout = spawned.take_stdout().expect("ACP stdout");
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("ACP output timeout")
            .expect("ACP output read");
        let Some(line) = line else { break };
        let Ok(event) = serde_json::from_str::<AcpRuntimeEvent>(&line) else {
            continue;
        };
        match event.payload {
            AcpEvent::SessionStart(id) => return id,
            AcpEvent::Done(_) | AcpEvent::Error(_) => break,
            _ => {}
        }
    }
    String::new()
}

fn git_revision(checkout: &Path) -> Option<String> {
    use std::process::Command;
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(checkout)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Extract the `--config` argument from the redacted runtime command.
fn overlay_from_diagnostics(rendered: &str) -> Option<PathBuf> {
    let mut tokens = rendered.split_whitespace();
    while let Some(token) = tokens.next() {
        if token == "--config"
            && let Some(path) = tokens.next()
        {
            return Some(PathBuf::from(path.trim_matches('\'')));
        }
    }
    None
}
