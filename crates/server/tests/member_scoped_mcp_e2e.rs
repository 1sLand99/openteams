#![cfg(feature = "qa-mode")]
//! 14-runner offline member-scoped MCP E2E registry.
//!
//! This suite exercises the production chat/workflow run-preparation chain
//! (`services::member_execution::build_effective_member_executor_for_run`, the
//! exact function used by `chat_runner` and the workflow runtime) for all 14
//! MCP-capable production runners across four offline CLI protocol families:
//!
//!   - stdio streaming:      CLAUDE_CODE, AMP, CURSOR_AGENT, COPILOT, DROID
//!   - ACP stdio:            GEMINI, QWEN_CODE, KIMI_CODE, QODER_CLI, PI, HERMES
//!   - Codex app-server:     CODEX
//!   - OpenCode local HTTP:  OPENCODE, OPEN_TEAMS_CLI
//!
//! The fixtures under `crates/executors/tests/fixtures/member_scoped_mcp/`
//! replace only the external CLI protocol implementation. HTTP/DB/session/
//! chat/workflow/adapter/cleanup all run through production code. The suite is
//! fully offline and deterministic: without skipped tests, real CLI binaries, accounts,
//! no public network, and it never reads token/key environment variables.
//!
//! Every registry entry is parameterized through E2E-MCP-001..010 and the
//! suite reports per-runner execution counts plus a fixed fake-secret scan
//! over API events, protocol logs, run records, and UI projections.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use db::models::{
    chat_agent::{ChatAgent, CreateChatAgent},
    chat_session::{ChatSession, ChatSessionWorktreeMode, CreateChatSession},
    chat_session_agent::{ChatSessionAgent, CreateChatSessionAgent},
    member_execution_config::MemberExecutionConfig,
};
use executors::{
    env::{ExecutionEnv, RepoContext},
    executors::{
        BaseCodingAgent, CodingAgent, SpawnedChild, StandardCodingAgentExecutor,
        acp::AcpExecutionOptions,
    },
    mcp_config::MemberMcpConfig,
    profile::{ExecutorConfigs, ExecutorProfileId},
};
use serde_json::{Value, json};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use strum::VariantNames;
use tempfile::TempDir;
use uuid::Uuid;

const FIXTURE_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../executors/tests/fixtures/member_scoped_mcp"
);
const FAKE_SECRET: &str = "E2E-FIXED-FAKE-SECRET-9f4a";

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolFamily {
    /// Prompt is streamed to stdin; run ends when the process exits.
    StdioStreaming,
    /// Agent Client Protocol over line-delimited stdio.
    AcpStdio,
    /// Codex app-server JSON-RPC over the process stdio.
    CodexAppServer,
    /// OpenCode/OpenTeams local HTTP server driven by the production SDK.
    LocalHttp,
}

impl ProtocolFamily {
    fn label(self) -> &'static str {
        match self {
            Self::StdioStreaming => "stdio-streaming",
            Self::AcpStdio => "acp-stdio",
            Self::CodexAppServer => "codex-app-server-jsonrpc",
            Self::LocalHttp => "local-http",
        }
    }
}

/// One parameterized runner entry in the member-scoped MCP E2E registry.
struct CliMcpE2eCase {
    runner: BaseCodingAgent,
    family: ProtocolFamily,
    /// Fake CLI fixture installed as this runner's executable name.
    fixture_binary: &'static str,
    /// Fixture mode passed to the fake CLI (family-specific).
    fixture_mode: &'static str,
    /// Human-readable description of the production MCP isolation carrier.
    carrier: &'static str,
    /// Relative path (under HOME) of the runner's ambient "global" vendor
    /// configuration that production must leave byte-identical and invisible.
    global_config_rel: &'static str,
}

const MCP_E2E_REGISTRY: &[CliMcpE2eCase] = &[
    CliMcpE2eCase {
        runner: BaseCodingAgent::ClaudeCode,
        family: ProtocolFamily::StdioStreaming,
        fixture_binary: "claude",
        fixture_mode: "claude",
        carrier: "frozen --mcp-config <run>/mcp.json (canonical mcpServers)",
        global_config_rel: ".claude.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::Amp,
        family: ProtocolFamily::StdioStreaming,
        fixture_binary: "amp",
        fixture_mode: "amp",
        carrier: "AMP_SETTINGS_FILE -> amp/settings.json (amp.mcpServers)",
        global_config_rel: ".config/amp/settings.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::Gemini,
        family: ProtocolFamily::AcpStdio,
        fixture_binary: "gemini",
        fixture_mode: "acp",
        carrier: "OPENTEAMS_ACP_MCP_SNAPSHOT_PATH -> run/mcp.json + GEMINI_CLI_SYSTEM_SETTINGS_PATH",
        global_config_rel: ".gemini/settings.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::Codex,
        family: ProtocolFamily::CodexAppServer,
        fixture_binary: "codex",
        fixture_mode: "codex",
        carrier: "frozen thread/start params -> config.mcp_servers (CODEX_HOME/config.toml pinned)",
        global_config_rel: ".codex/config.toml",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::Opencode,
        family: ProtocolFamily::LocalHttp,
        fixture_binary: "opencode",
        fixture_mode: "http",
        carrier: "OPENCODE_CONFIG_CONTENT -> config.mcp + XDG_CONFIG_HOME + OPENCODE_DISABLE_PROJECT_CONFIG",
        global_config_rel: ".config/opencode/opencode.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::OpenTeamsCli,
        family: ProtocolFamily::LocalHttp,
        fixture_binary: "openteams-cli",
        fixture_mode: "http",
        carrier: "OPENTEAMS_CONFIG_CONTENT -> config.mcp (openteams-cli serve)",
        global_config_rel: ".openteams/openteams.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::CursorAgent,
        family: ProtocolFamily::StdioStreaming,
        fixture_binary: "cursor-agent",
        fixture_mode: "cursor",
        carrier: "HOME pin -> run/.cursor/mcp.json + CURSOR_CONFIG_DIR",
        global_config_rel: ".cursor/mcp.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::QwenCode,
        family: ProtocolFamily::AcpStdio,
        fixture_binary: "qwen",
        fixture_mode: "acp",
        carrier: "OPENTEAMS_ACP_MCP_SNAPSHOT_PATH -> run/mcp.json",
        global_config_rel: ".qwen/settings.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::Copilot,
        family: ProtocolFamily::StdioStreaming,
        fixture_binary: "copilot",
        fixture_mode: "copilot",
        carrier: "COPILOT_HOME pin -> run/mcp-config.json",
        global_config_rel: ".copilot/mcp-config.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::Droid,
        family: ProtocolFamily::StdioStreaming,
        fixture_binary: "droid",
        fixture_mode: "droid",
        carrier: "HOME/FACTORY_HOME_OVERRIDE pin -> run/.factory/mcp.json",
        global_config_rel: ".factory/mcp.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::KimiCode,
        family: ProtocolFamily::AcpStdio,
        fixture_binary: "kimi",
        fixture_mode: "acp",
        carrier: "frozen run snapshot -> transient mcp.json in stable member KIMI MCP view (empty ACP list)",
        global_config_rel: ".kimi-code/mcp.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::QoderCli,
        family: ProtocolFamily::AcpStdio,
        fixture_binary: "qodercli",
        fixture_mode: "acp",
        carrier: "OPENTEAMS_ACP_MCP_SNAPSHOT_PATH -> run/mcp.json",
        global_config_rel: ".qoder/settings.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::Pi,
        family: ProtocolFamily::AcpStdio,
        fixture_binary: "pi-acp",
        fixture_mode: "acp",
        carrier: "OPENTEAMS_PI_MCP_SNAPSHOT -> run/mcp.json via launcher env",
        global_config_rel: ".pi/agent/mcp.json",
    },
    CliMcpE2eCase {
        runner: BaseCodingAgent::Hermes,
        family: ProtocolFamily::AcpStdio,
        fixture_binary: "hermes",
        fixture_mode: "acp",
        carrier: "OPENTEAMS_ACP_MCP_SNAPSHOT_PATH -> run/mcp.json",
        global_config_rel: ".hermes/config.yaml",
    },
];

/// Production gate: compute the `supports_mcp == true` set from the production
/// enum/profile and require it to be exactly the registry above.
fn production_supports_mcp_set() -> BTreeSet<String> {
    let mut supported = BTreeSet::new();
    let profiles = ExecutorConfigs::get_cached();
    for runner in CodingAgent::VARIANTS {
        let Ok(runner) = BaseCodingAgent::from_str(runner) else {
            continue;
        };
        // Skip qa-mode-only test runners: they are not production runners and
        // do not have a DEFAULT profile in the production profiles.json.
        if matches!(runner, BaseCodingAgent::QaMock | BaseCodingAgent::AcpQa) {
            continue;
        }
        let executor = profiles.get_coding_agent_or_default(&ExecutorProfileId::new(runner));
        if executor.supports_mcp() {
            supported.insert(runner.to_string());
        }
    }
    supported
}

fn registry_runners() -> BTreeSet<String> {
    MCP_E2E_REGISTRY
        .iter()
        .map(|case| case.runner.to_string())
        .collect()
}

fn verify_registry_gate(reports: &mut Vec<String>) -> Result<(), String> {
    let production = production_supports_mcp_set();
    let registry = registry_runners();
    let missing = production
        .difference(&registry)
        .cloned()
        .collect::<Vec<_>>();
    let extra = registry
        .difference(&production)
        .cloned()
        .collect::<Vec<_>>();
    reports.push(format!(
        "registry gate: production supports_mcp=true set = [{}]",
        production
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    reports.push(format!(
        "registry gate: registry entries = [{}]",
        registry
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    reports.push(format!(
        "registry gate: supports_mcp set diff = production-only={:?} registry-only={:?}",
        missing, extra
    ));
    let deepseek = BaseCodingAgent::from_str("DEEPSEEK_HARNESS")
        .ok()
        .map(|runner| {
            let profiles = ExecutorConfigs::get_cached();
            let executor = profiles.get_coding_agent_or_default(&ExecutorProfileId::new(runner));
            executor.supports_mcp()
        });
    reports.push(format!(
        "registry gate: DEEPSEEK_HARNESS supports_mcp = {:?}",
        deepseek
    ));
    if !missing.is_empty() || !extra.is_empty() {
        return Err(format!(
            "registry gate mismatch: production-only={missing:?} registry-only={extra:?}"
        ));
    }
    if deepseek != Some(false) {
        return Err(format!(
            "DEEPSEEK_HARNESS must not support MCP; got {deepseek:?}"
        ));
    }
    if production.len() != 14 {
        return Err(format!(
            "expected exactly 14 MCP-capable production runners; got {}",
            production.len()
        ));
    }
    reports.push("registry gate: PASS (14 runners, DeepSeek false, diff empty)".to_string());
    Ok(())
}

// ---------------------------------------------------------------------------
// Fixture installation
// ---------------------------------------------------------------------------

fn write_executable(path: &Path, contents: &str, mode: u32) {
    fs::write(path, contents).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|e| panic!("chmod {}: {e}", path.display()));
}

fn read_fixture(name: &str) -> String {
    fs::read_to_string(Path::new(FIXTURE_DIR).join(name))
        .unwrap_or_else(|e| panic!("read fixture {name}: {e}"))
}

/// Install every fake CLI under a fixture bin directory. Returns the bin path.
fn install_fixture_bin(root: &Path) -> PathBuf {
    let bin = root.join("bin");
    fs::create_dir_all(&bin).expect("create fixture bin");

    let npx_source = read_fixture("fake_npx.sh");
    let npx_path = bin.join("npx");
    fs::write(&npx_path, &npx_source).expect("write fake npx");
    fs::set_permissions(&npx_path, fs::Permissions::from_mode(0o755)).expect("npx chmod");

    let acp_source = read_fixture("fake_acp.mjs");
    let stdio_source = read_fixture("fake_stdio_stream.mjs");
    let claude_source = read_fixture("fake_claude.mjs");
    let codex_source = read_fixture("fake_codex.mjs");
    let http_source = read_fixture("fake_http_server.mjs");

    for (name, source) in [
        ("gemini", &acp_source),
        ("qwen", &acp_source),
        ("kimi", &acp_source),
        ("qodercli", &acp_source),
        ("pi-acp", &acp_source),
        ("hermes", &acp_source),
        ("amp", &stdio_source),
        ("cursor-agent", &stdio_source),
        ("copilot", &stdio_source),
        ("droid", &stdio_source),
        ("claude", &claude_source),
        ("codex", &codex_source),
        ("opencode", &http_source),
        ("openteams-cli", &http_source),
    ] {
        let path = bin.join(name);
        write_executable(&path, source, 0o755);
    }

    // Shared fixture modules the fakes import at runtime.
    for name in ["mcp_client.mjs", "mcp_server_stdio.mjs"] {
        let source = read_fixture(name);
        fs::write(bin.join(name), source).expect("write shared fixture module");
    }
    bin
}

// ---------------------------------------------------------------------------
// Scenario environment
// ---------------------------------------------------------------------------

/// One member's run specification for a parameterized scenario.
#[derive(Clone)]
struct MemberSpec {
    name: String,
    servers: Vec<(String, Value)>,
    hang: bool,
    fail: bool,
    no_mcp: bool,
    prompt_tag: String,
}

/// Outcome of one production member run, plus everything needed for the
/// fake-secret scan and cleanup assertions.
struct RunOutcome {
    runner: BaseCodingAgent,
    protocol_log: PathBuf,
    mcp_logs: Vec<PathBuf>,
    exit_ok: bool,
    connected_servers: Vec<String>,
    collected_text: Vec<String>,
}

#[derive(Clone)]
struct ScenarioEnv {
    fixture_bin: PathBuf,
    pool: SqlitePool,
    fake_secret: String,
    node_bin: String,
}

impl ScenarioEnv {
    fn mcp_stdin_command(&self) -> String {
        self.node_bin.clone()
    }
}

/// Relative protocol-log env key per family.
fn family_protocol_log_env(case: &CliMcpE2eCase) -> &'static str {
    match case.family {
        ProtocolFamily::StdioStreaming if case.fixture_mode == "claude" => {
            "FAKE_CLAUDE_PROTOCOL_LOG"
        }
        ProtocolFamily::StdioStreaming => "FAKE_STDIO_PROTOCOL_LOG",
        ProtocolFamily::AcpStdio => "FAKE_ACP_PROTOCOL_LOG",
        ProtocolFamily::CodexAppServer => "FAKE_CODEX_PROTOCOL_LOG",
        ProtocolFamily::LocalHttp => "FAKE_HTTP_PROTOCOL_LOG",
    }
}

fn family_secret_env(case: &CliMcpE2eCase) -> &'static str {
    match case.family {
        ProtocolFamily::StdioStreaming if case.fixture_mode == "claude" => {
            "FAKE_CLAUDE_FAKE_SECRET"
        }
        ProtocolFamily::StdioStreaming => "FAKE_STDIO_FAKE_SECRET",
        ProtocolFamily::AcpStdio => "FAKE_ACP_FAKE_SECRET",
        ProtocolFamily::CodexAppServer => "FAKE_CODEX_FAKE_SECRET",
        ProtocolFamily::LocalHttp => "FAKE_HTTP_FAKE_SECRET",
    }
}

fn family_hang_env(case: &CliMcpE2eCase) -> &'static str {
    match case.family {
        ProtocolFamily::StdioStreaming if case.fixture_mode == "claude" => "FAKE_CLAUDE_HANG",
        ProtocolFamily::StdioStreaming => "FAKE_STDIO_HANG",
        ProtocolFamily::AcpStdio => "FAKE_ACP_HANG",
        ProtocolFamily::CodexAppServer => "FAKE_CODEX_HANG",
        ProtocolFamily::LocalHttp => "FAKE_HTTP_HANG",
    }
}

fn family_fail_env(case: &CliMcpE2eCase) -> &'static str {
    match case.family {
        ProtocolFamily::StdioStreaming if case.fixture_mode == "claude" => "FAKE_CLAUDE_FAIL",
        ProtocolFamily::StdioStreaming => "FAKE_STDIO_FAIL",
        ProtocolFamily::AcpStdio => "FAKE_ACP_FAIL_PROMPT",
        ProtocolFamily::CodexAppServer => "FAKE_CODEX_FAIL_TURN",
        ProtocolFamily::LocalHttp => "FAKE_HTTP_FAIL",
    }
}

fn family_no_mcp_env(case: &CliMcpE2eCase) -> &'static str {
    match case.family {
        ProtocolFamily::StdioStreaming if case.fixture_mode == "claude" => "FAKE_CLAUDE_NO_MCP",
        ProtocolFamily::StdioStreaming => "FAKE_STDIO_NO_MCP",
        ProtocolFamily::AcpStdio => "FAKE_ACP_NO_MCP",
        ProtocolFamily::CodexAppServer => "FAKE_CODEX_NO_MCP",
        ProtocolFamily::LocalHttp => "FAKE_HTTP_NO_MCP",
    }
}

fn family_mode_env(case: &CliMcpE2eCase) -> Option<&'static str> {
    match case.family {
        ProtocolFamily::StdioStreaming => Some("FAKE_STDIO_RUNNER"),
        ProtocolFamily::AcpStdio => Some("FAKE_ACP_RUNNER"),
        ProtocolFamily::LocalHttp => Some("FAKE_HTTP_RUNNER"),
        _ => None,
    }
}

fn runner_label(runner: BaseCodingAgent) -> String {
    runner.to_string()
}

/// Local MCP server definition for a member server with a distinct tag and log.
fn local_mcp_server(
    env: &ScenarioEnv,
    server_name: &str,
    tag: &str,
    log_path: &Path,
) -> (String, Value) {
    let server_script = env.fixture_bin.join("mcp_server_stdio.mjs");
    let definition = json!({
        "command": env.mcp_stdin_command(),
        "args": [server_script.to_string_lossy().into_owned()],
        "env": {
            "MCP_SERVER_TAG": tag,
            "MCP_SERVER_LOG": log_path.to_string_lossy().into_owned(),
            "MCP_SERVER_FAKE_SECRET": env.fake_secret,
        }
    });
    (server_name.to_string(), definition)
}

fn canonical_mcp(servers: Vec<(String, Value)>) -> MemberMcpConfig {
    MemberMcpConfig {
        mcp_servers: servers.into_iter().collect(),
    }
}

// ---------------------------------------------------------------------------
// Database helpers
// ---------------------------------------------------------------------------

async fn setup_database(root: &Path) -> SqlitePool {
    let database_path = root.join("e2e.sqlite");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&database_path)
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("connect E2E database");
    sqlx::migrate!("../db/migrations")
        .run(&pool)
        .await
        .expect("run E2E database migrations");
    pool
}

async fn create_member(
    pool: &SqlitePool,
    member: &MemberSpec,
    runner: BaseCodingAgent,
    workspace: &Path,
) -> (ChatAgent, ChatSessionAgent) {
    let agent = ChatAgent::create(
        pool,
        &CreateChatAgent {
            name: member.name.clone(),
            runner_type: runner.to_string(),
            system_prompt: Some("Offline E2E agent.".to_string()),
            tools_enabled: Some(serde_json::json!({})),
            model_name: None,
            owner_project_id: None,
        },
        Uuid::new_v4(),
    )
    .await
    .expect("create chat agent");

    let session = ChatSession::create(
        pool,
        &CreateChatSession {
            title: Some(format!("E2E session {}", member.name)),
            workspace_path: Some(workspace.to_string_lossy().into_owned()),
            project_id: None,
            worktree_mode: Some(ChatSessionWorktreeMode::Disabled),
        },
        Uuid::new_v4(),
    )
    .await
    .expect("create chat session");

    let session_agent = ChatSessionAgent::create(
        pool,
        &CreateChatSessionAgent {
            session_id: session.id,
            agent_id: agent.id,
            member_name: Some(member.name.clone()),
            workspace_path: Some(workspace.to_string_lossy().into_owned()),
            allowed_skill_ids: Vec::new(),
            project_member_id: None,
            execution_config: MemberExecutionConfig {
                mcp: Some(canonical_mcp(member.servers.clone())),
                acp: Some(AcpExecutionOptions {
                    approval_mode: Some(executors::executors::acp::AcpApprovalMode::AutoAllow),
                    ..Default::default()
                }),
                ..Default::default()
            },
        },
        Uuid::new_v4(),
    )
    .await
    .expect("create chat session agent");
    (agent, session_agent)
}

// ---------------------------------------------------------------------------
// Production run driver
// ---------------------------------------------------------------------------

async fn wait_for_run(spawned: &mut SpawnedChild, timeout_secs: u64) -> Result<bool, String> {
    if let Some(signal) = spawned.exit_signal.take() {
        let result = tokio::time::timeout(Duration::from_secs(timeout_secs), signal)
            .await
            .map_err(|_| format!("run exit timeout after {timeout_secs}s"))?
            .map_err(|_| "run exit signal dropped".to_string())?;
        return Ok(matches!(
            result,
            executors::executors::ExecutorExitResult::Success
        ));
    }
    let status = tokio::time::timeout(Duration::from_secs(timeout_secs), spawned.child.wait())
        .await
        .map_err(|_| format!("run wait timeout after {timeout_secs}s"))?
        .map_err(|e| format!("run wait error: {e}"))?;
    Ok(status.success())
}

fn kill_child(spawned: &mut SpawnedChild) {
    if let Some(cancel) = &spawned.cancel {
        cancel.cancel();
    }
    let _ = spawned.child.inner().start_kill();
    drop(spawned.child.inner().wait());
}

fn collect_private_dirs(workspace: &Path) -> Vec<PathBuf> {
    let tmp = workspace.join(".openteams").join("tmp");
    if !tmp.exists() {
        return Vec::new();
    }
    let mut dirs = Vec::new();
    if let Ok(entries) = fs::read_dir(&tmp) {
        for entry in entries.flatten() {
            dirs.push(entry.path());
        }
    }
    dirs
}

async fn run_member(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    member: &MemberSpec,
    prompt: &str,
    run_id: Uuid,
    workspace: &Path,
    home: &Path,
) -> Result<RunOutcome, String> {
    let (agent, session_agent) = create_member(&ctx.pool, member, case.runner, workspace).await;
    run_member_with(
        ctx,
        case,
        member,
        prompt,
        run_id,
        workspace,
        home,
        agent,
        session_agent,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_member_with(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    member: &MemberSpec,
    prompt: &str,
    run_id: Uuid,
    workspace: &Path,
    home: &Path,
    agent: ChatAgent,
    session_agent: ChatSessionAgent,
) -> Result<RunOutcome, String> {
    let mut env = ExecutionEnv::new(
        RepoContext::new(workspace.to_path_buf(), Vec::new()),
        false,
        String::new(),
    );
    env.insert(
        "PATH",
        format!(
            "{}:{}",
            ctx.fixture_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    env.insert("HOME", home.to_string_lossy().into_owned());
    env.insert("NO_UPDATE_NOTIFIER", "1");
    env.insert("NO_COLOR", "1");
    env.insert("CI", "1");
    env.insert(
        "OPENTEAMS_PI_QA_NPX_PATH",
        ctx.fixture_bin.join("npx").to_string_lossy().into_owned(),
    );

    let protocol_log = workspace.join(format!(
        "protocol-{}-{}.jsonl",
        runner_label(case.runner),
        member.name
    ));
    let mcp_logs: Vec<PathBuf> = member
        .servers
        .iter()
        .map(|(name, _)| {
            workspace.join(format!(
                "mcp-{}-{}-{}.log",
                runner_label(case.runner),
                member.name,
                name
            ))
        })
        .collect();

    // A member may be run repeatedly in E2E-MCP-004. Each assertion must read
    // only this run's carrier/protocol evidence, never the preceding run's
    // append-only fixture log.
    let _ = fs::remove_file(&protocol_log);
    for log in &mcp_logs {
        let _ = fs::remove_file(log);
    }

    env.insert(
        family_protocol_log_env(case),
        protocol_log.to_string_lossy().into_owned(),
    );
    env.insert(family_secret_env(case), ctx.fake_secret.clone());
    if let Some(key) = family_mode_env(case) {
        env.insert(key, case.fixture_mode.to_string());
    }
    env.insert(family_hang_env(case), if member.hang { "1" } else { "0" });
    env.insert(family_fail_env(case), if member.fail { "1" } else { "0" });
    env.insert(
        family_no_mcp_env(case),
        if member.no_mcp { "1" } else { "0" },
    );

    let (_, executor, prepared) =
        services::services::member_execution::build_effective_member_executor_for_run(
            &ctx.pool,
            &agent,
            &session_agent,
            workspace,
            run_id,
            &mut env,
        )
        .await
        .map_err(|e| format!("production preparation failed: {e:#}"))?;

    let tagged_prompt = format!("[qa-tag:{}] {prompt}", member.prompt_tag);
    let mut spawned = executor
        .spawn(workspace, &tagged_prompt, &env)
        .await
        .map_err(|e| format!("production spawn failed: {e:#}"))?;
    spawned.cleanup = executors::executors::ExecutorRunCleanup::combine(
        spawned.cleanup.take(),
        prepared.into_cleanup(),
    );

    let exit_ok = wait_for_run(&mut spawned, 60)
        .await
        .map_err(|e| format!("run completion failed: {e}"))?;

    // Release the child and run cleanup: private MCP directories are removed on drop.
    kill_child(&mut spawned);
    drop(spawned);

    let protocol_text = read_optional(&protocol_log);
    let mut collected = vec![protocol_text.clone()];
    for log in &mcp_logs {
        collected.push(read_optional(log));
    }

    let connected_servers = parse_connected_servers(&protocol_text);
    Ok(RunOutcome {
        runner: case.runner,
        protocol_log,
        mcp_logs,
        exit_ok,
        connected_servers,
        collected_text: collected,
    })
}

fn read_optional(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn parse_connected_servers(protocol: &str) -> Vec<String> {
    protocol
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            if value.get("event")?.as_str() == Some("mcp_connected")
                && value.get("connected").and_then(Value::as_bool) == Some(true)
            {
                value
                    .get("server")
                    .and_then(Value::as_str)
                    .map(String::from)
            } else {
                None
            }
        })
        .collect()
}

fn assert_kimi_runtime_mcp_exactly(outcome: &RunOutcome, expected: &[&str]) -> Result<(), String> {
    let protocol = read_optional(&outcome.protocol_log);
    let mut observed = false;
    let expected = expected
        .iter()
        .map(|name| (*name).to_string())
        .collect::<Vec<_>>();
    for line in protocol.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line)
            .map_err(|error| format!("Kimi protocol log contains invalid JSON: {error}"))?;
        if value.get("event").and_then(Value::as_str) != Some("kimi_runtime_mcp_read") {
            continue;
        }
        observed = true;
        let names = value
            .get("server_names")
            .and_then(Value::as_array)
            .ok_or_else(|| "Kimi runtime MCP evidence has no server_names array".to_string())?;
        let names = names
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        if names != expected {
            return Err(format!(
                "Kimi runtime view used {names:?}, expected {expected:?}"
            ));
        }
    }
    if !observed {
        return Err("Kimi runtime MCP isolation evidence was not recorded".to_string());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Secret scan and cleanup checks
// ---------------------------------------------------------------------------

fn scan_collected(secret: &str, outcomes: &[&RunOutcome]) -> Vec<String> {
    let mut leaks = Vec::new();
    for outcome in outcomes {
        for (index, text) in outcome.collected_text.iter().enumerate() {
            if text.contains(secret) {
                leaks.push(format!(
                    "runner={} collected[{}] leaked the fake secret",
                    runner_label(outcome.runner),
                    index
                ));
            }
        }
        if let Ok(text) = fs::read_to_string(&outcome.protocol_log)
            && text.contains(secret)
        {
            leaks.push(format!(
                "runner={} protocol log leaked the fake secret",
                runner_label(outcome.runner)
            ));
        }
    }
    leaks
}

fn assert_tmp_clean(
    workspace: &Path,
    label: &str,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let leftover = collect_private_dirs(workspace);
    if !leftover.is_empty() {
        return Err(format!(
            "{label}: private MCP run directories were not cleaned: {leftover:?}"
        ));
    }
    reports.push(format!(
        "cleanup {label}: PASS (no leftover private run dirs)"
    ));
    Ok(())
}

fn assert_kimi_runtime_mcp_cleaned(workspace: &Path) -> Result<(), String> {
    let views = workspace
        .join(".openteams")
        .join("executor-state")
        .join("kimi-mcp-view");
    if !views.exists() {
        return Ok(());
    }
    let leftover = fs::read_dir(&views)
        .map_err(|error| format!("read Kimi runtime views: {error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("mcp.json"))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    if leftover.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Kimi native member MCP files survived run cleanup: {leftover:?}"
        ))
    }
}

async fn collect_sqlite_secret_scan(pool: &SqlitePool, secret: &str) -> Vec<String> {
    // Scan serialized member execution configs stored in the DB (the canonical
    // member config legitimately holds the secret, but it must never leak into
    // chat transcripts, run records, or approval rows).
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"SELECT table_name, record_text FROM (
             SELECT 'chat_messages' AS table_name, COALESCE(content,'') AS record_text FROM chat_messages
             UNION ALL
             SELECT 'chat_runs', COALESCE(transcript,'') FROM chat_runs
             UNION ALL
             SELECT 'chat_executor_approval_requests', COALESCE(request_data,'') FROM chat_executor_approval_requests
             UNION ALL
             SELECT 'workflow_transcripts', COALESCE(transcript,'') FROM workflow_transcripts
           )"#,
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .filter(|(_, text)| text.contains(secret))
        .map(|(table, _)| format!("sqlite leak in {table}"))
        .collect::<Vec<_>>()
}

// ---------------------------------------------------------------------------
// Scenario scaffolding
// ---------------------------------------------------------------------------

struct Scenario {
    _root: TempDir,
    workspace: PathBuf,
    home: PathBuf,
}

fn new_scenario() -> Scenario {
    let root = TempDir::new().expect("scenario temp root");
    let workspace = root.path().join("workspace");
    fs::create_dir_all(&workspace).expect("scenario workspace");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("scenario home");
    Scenario {
        _root: root,
        workspace,
        home,
    }
}

fn set_process_home(home: &Path) {
    // Safe here: the suite is a single-threaded harness binary; no other
    // threads read process env concurrently.
    unsafe {
        std::env::set_var("HOME", home);
        std::env::set_var("USERPROFILE", home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
    }
}

fn member_with(name: &str, tag: &str, servers: Vec<(String, Value)>) -> MemberSpec {
    MemberSpec {
        name: name.to_string(),
        servers,
        hang: false,
        fail: false,
        no_mcp: false,
        prompt_tag: tag.to_string(),
    }
}

fn mcp_connected_names(outcome: &RunOutcome) -> Vec<String> {
    outcome.connected_servers.clone()
}

fn assert_no_connected(outcome: &RunOutcome, names: &[&str]) -> Result<(), String> {
    let connected = mcp_connected_names(outcome);
    for name in names {
        if connected.iter().any(|value| value == name) {
            return Err(format!(
                "runner={} unexpectedly connected to `{name}` (connected={connected:?})",
                runner_label(outcome.runner)
            ));
        }
    }
    Ok(())
}

fn assert_connected_exactly(outcome: &RunOutcome, expected: &[&str]) -> Result<(), String> {
    let connected = mcp_connected_names(outcome);
    let expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    if connected != expected {
        return Err(format!(
            "runner={} connected to {:?}, expected {expected:?}",
            runner_label(outcome.runner),
            connected
        ));
    }
    Ok(())
}

fn assert_mcp_log_tags(
    log: &Path,
    expected_tag: &str,
    forbidden_tag: &str,
    label: &str,
) -> Result<(), String> {
    let text = fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return Err(format!("{label}: MCP server log {log:?} is empty"));
    }
    for line in &lines {
        if line.starts_with(forbidden_tag) {
            return Err(format!(
                "{label}: MCP log {log:?} leaked the forbidden tag `{forbidden_tag}`: {line}"
            ));
        }
        if !line.starts_with(expected_tag) {
            return Err(format!(
                "{label}: MCP log {log:?} has an unexpected line (tag mismatch): {line}"
            ));
        }
    }
    Ok(())
}

/// Write the runner's ambient global vendor config containing a sentinel MCP
/// server and the fixed fake secret. Returns the bytes written.
fn write_global_config(case: &CliMcpE2eCase, home: &Path, secret: &str) -> PathBuf {
    let path = home.join(case.global_config_rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create global config parent");
    }
    let body = match case.runner {
        BaseCodingAgent::Codex => format!(
            "[mcp_servers.global-sentinel]\ncommand = \"echo\"\nargs = [\"{} sentinel\"]\n",
            secret
        ),
        BaseCodingAgent::Hermes => format!(
            "mcp_servers:\n  global-sentinel:\n    command: \"echo\"\n    args: [\"{}\"]\n",
            secret
        ),
        BaseCodingAgent::Amp => format!(
            "{{\"amp.mcpServers\": {{\"global-sentinel\": {{\"command\": \"echo\", \"args\": [\"{}\"]}}}}}}\n",
            secret
        ),
        _ => format!(
            "{{\"mcpServers\": {{\"global-sentinel\": {{\"command\": \"echo\", \"args\": [\"{}\"]}}}}}}\n",
            secret
        ),
    };
    fs::write(&path, body.as_bytes()).expect("write global config");
    path
}

// ---------------------------------------------------------------------------
// E2E-MCP scenarios
// ---------------------------------------------------------------------------

async fn e2e_001_chat_concurrent_isolation(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-001 {}", runner_label(case.runner));

    let alpha_log = scenario.workspace.join("alpha-mcp.log");
    let beta_log = scenario.workspace.join("beta-mcp.log");
    let alpha = member_with(
        "Alpha",
        "alpha",
        vec![local_mcp_server(ctx, "alpha-mcp", "alpha", &alpha_log)],
    );
    let beta = member_with(
        "Beta",
        "beta",
        vec![local_mcp_server(ctx, "beta-mcp", "beta", &beta_log)],
    );

    // Create both members up front so only the two production runs overlap.
    let (a_agent, a_session) =
        create_member(&ctx.pool, &alpha, case.runner, &scenario.workspace).await;
    let (b_agent, b_session) =
        create_member(&ctx.pool, &beta, case.runner, &scenario.workspace).await;

    let (a, b) = tokio::join!(
        run_member_with(
            ctx,
            case,
            &alpha,
            "concurrent chat alpha",
            Uuid::new_v4(),
            &scenario.workspace,
            &scenario.home,
            a_agent,
            a_session,
        ),
        run_member_with(
            ctx,
            case,
            &beta,
            "concurrent chat beta",
            Uuid::new_v4(),
            &scenario.workspace,
            &scenario.home,
            b_agent,
            b_session,
        ),
    );
    let a = a.map_err(|e| format!("{label}: alpha run: {e}"))?;
    let b = b.map_err(|e| format!("{label}: beta run: {e}"))?;

    if !a.exit_ok {
        return Err(format!("{label}: alpha run did not complete successfully"));
    }
    if !b.exit_ok {
        return Err(format!("{label}: beta run did not complete successfully"));
    }
    assert_connected_exactly(&a, &["alpha-mcp"])?;
    assert_connected_exactly(&b, &["beta-mcp"])?;
    assert_no_connected(&a, &["beta-mcp"])?;
    assert_no_connected(&b, &["alpha-mcp"])?;
    assert_mcp_log_tags(&alpha_log, "alpha", "beta", &label)?;
    assert_mcp_log_tags(&beta_log, "beta", "alpha", &label)?;

    let leaks = scan_collected(ctx.fake_secret.as_str(), &[&a, &b]);
    if !leaks.is_empty() {
        return Err(format!("{label}: secret leak: {leaks:?}"));
    }
    assert_tmp_clean(&scenario.workspace, &format!("{label} chat"), reports)?;

    *counters.entry("E2E-MCP-001".into()).or_default() += 1;
    reports.push(format!(
        "{label}: PASS (alpha->alpha-mcp, beta->beta-mcp, concurrent, no cross-talk)"
    ));
    Ok(())
}

async fn e2e_002_workflow_isolation(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-002 {}", runner_label(case.runner));

    let left_log = scenario.workspace.join("left-mcp.log");
    let right_log = scenario.workspace.join("right-mcp.log");
    // Workflow-style: two workflow agent sessions, run sequentially; each
    // session's run must connect only to its own member MCP servers.
    let left = member_with(
        "WorkflowLeft",
        "wleft",
        vec![local_mcp_server(ctx, "left-mcp", "left", &left_log)],
    );
    let right = member_with(
        "WorkflowRight",
        "wright",
        vec![local_mcp_server(ctx, "right-mcp", "right", &right_log)],
    );

    let left_outcome = run_member(
        ctx,
        case,
        &left,
        "workflow left",
        Uuid::new_v4(),
        &scenario.workspace,
        &scenario.home,
    )
    .await
    .map_err(|e| format!("{label}: left run: {e}"))?;
    let right_outcome = run_member(
        ctx,
        case,
        &right,
        "workflow right",
        Uuid::new_v4(),
        &scenario.workspace,
        &scenario.home,
    )
    .await
    .map_err(|e| format!("{label}: right run: {e}"))?;

    if !left_outcome.exit_ok || !right_outcome.exit_ok {
        return Err(format!("{label}: workflow runs did not complete"));
    }
    assert_connected_exactly(&left_outcome, &["left-mcp"])?;
    assert_connected_exactly(&right_outcome, &["right-mcp"])?;
    assert_mcp_log_tags(&left_log, "left", "right", &label)?;
    assert_mcp_log_tags(&right_log, "right", "left", &label)?;

    let leaks = scan_collected(ctx.fake_secret.as_str(), &[&left_outcome, &right_outcome]);
    if !leaks.is_empty() {
        return Err(format!("{label}: secret leak: {leaks:?}"));
    }
    assert_tmp_clean(&scenario.workspace, &format!("{label} workflow"), reports)?;

    *counters.entry("E2E-MCP-002".into()).or_default() += 1;
    reports.push(format!(
        "{label}: PASS (left->left-mcp, right->right-mcp, sequential workflow isolation)"
    ));
    Ok(())
}

async fn e2e_003_empty_config_disables(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-003 {}", runner_label(case.runner));

    let empty = MemberSpec {
        name: "EmptyConfig".to_string(),
        servers: Vec::new(),
        hang: false,
        fail: false,
        no_mcp: false,
        prompt_tag: "empty".to_string(),
    };
    let outcome = run_member(
        ctx,
        case,
        &empty,
        "empty config run",
        Uuid::new_v4(),
        &scenario.workspace,
        &scenario.home,
    )
    .await
    .map_err(|e| format!("{label}: run: {e}"))?;

    if !outcome.exit_ok {
        return Err(format!("{label}: empty-config run did not complete"));
    }
    assert_connected_exactly(&outcome, &[])?;
    // No MCP server log should even exist for an empty config.
    if !outcome.mcp_logs.iter().all(|path| !path.exists()) {
        return Err(format!(
            "{label}: empty config still produced MCP connection logs"
        ));
    }
    let leaks = scan_collected(ctx.fake_secret.as_str(), &[&outcome]);
    if !leaks.is_empty() {
        return Err(format!("{label}: secret leak: {leaks:?}"));
    }
    assert_tmp_clean(&scenario.workspace, &format!("{label} empty"), reports)?;

    *counters.entry("E2E-MCP-003".into()).or_default() += 1;
    reports.push(format!(
        "{label}: PASS (explicit empty config disables MCP)"
    ));
    Ok(())
}

async fn e2e_004_next_run_takes_effect(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-004 {}", runner_label(case.runner));

    let first_log = scenario.workspace.join("first-mcp.log");
    let second_log = scenario.workspace.join("second-mcp.log");
    let first = member_with(
        "NextRun",
        "run1",
        vec![local_mcp_server(ctx, "first-mcp", "first", &first_log)],
    );
    let (agent, session_agent) =
        create_member(&ctx.pool, &first, case.runner, &scenario.workspace).await;

    let first_outcome = run_member_with(
        ctx,
        case,
        &first,
        "next-run first",
        Uuid::new_v4(),
        &scenario.workspace,
        &scenario.home,
        agent.clone(),
        session_agent.clone(),
    )
    .await
    .map_err(|e| format!("{label}: first run: {e}"))?;
    assert_connected_exactly(&first_outcome, &["first-mcp"])?;

    // Apply a new member MCP config for the next run only.
    let second = member_with(
        "NextRun",
        "run2",
        vec![local_mcp_server(ctx, "second-mcp", "second", &second_log)],
    );
    let updated = ChatSessionAgent::update_execution_config_for_next_run(
        &ctx.pool,
        session_agent.id,
        None,
        MemberExecutionConfig {
            mcp: Some(canonical_mcp(second.servers.clone())),
            acp: Some(AcpExecutionOptions {
                approval_mode: Some(executors::executors::acp::AcpApprovalMode::AutoAllow),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .map_err(|e| format!("{label}: update member config: {e}"))?;

    let second_outcome = run_member_with(
        ctx,
        case,
        &second,
        "next-run second",
        Uuid::new_v4(),
        &scenario.workspace,
        &scenario.home,
        agent,
        updated,
    )
    .await
    .map_err(|e| format!("{label}: second run: {e}"))?;

    if !first_outcome.exit_ok || !second_outcome.exit_ok {
        return Err(format!("{label}: runs did not complete"));
    }
    assert_connected_exactly(&first_outcome, &["first-mcp"])?;
    assert_connected_exactly(&second_outcome, &["second-mcp"])?;
    assert_no_connected(&second_outcome, &["first-mcp"])?;

    let leaks = scan_collected(ctx.fake_secret.as_str(), &[&first_outcome, &second_outcome]);
    if !leaks.is_empty() {
        return Err(format!("{label}: secret leak: {leaks:?}"));
    }
    assert_tmp_clean(&scenario.workspace, &format!("{label} next-run"), reports)?;

    *counters.entry("E2E-MCP-004".into()).or_default() += 1;
    reports.push(format!(
        "{label}: PASS (next run used the updated member MCP config)"
    ));
    Ok(())
}

async fn e2e_005_global_invisible_and_byte_stable(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-005 {}", runner_label(case.runner));

    let global_path = write_global_config(case, &scenario.home, &ctx.fake_secret);
    let before = fs::read(&global_path).expect("read global config before");

    let member_log = scenario.workspace.join("member-mcp.log");
    let member = member_with(
        "GlobalMember",
        "global",
        vec![local_mcp_server(ctx, "member-mcp", "member", &member_log)],
    );
    let outcome = run_member(
        ctx,
        case,
        &member,
        "global visibility run",
        Uuid::new_v4(),
        &scenario.workspace,
        &scenario.home,
    )
    .await
    .map_err(|e| format!("{label}: run: {e}"))?;

    if !outcome.exit_ok {
        return Err(format!("{label}: run did not complete"));
    }
    assert_connected_exactly(&outcome, &["member-mcp"])?;
    assert_no_connected(&outcome, &["global-sentinel"])?;
    if case.runner == BaseCodingAgent::KimiCode {
        assert_kimi_runtime_mcp_exactly(&outcome, &["member-mcp"])?;
        assert_kimi_runtime_mcp_cleaned(&scenario.workspace)?;
    }

    let after = fs::read(&global_path).expect("read global config after");
    if before != after {
        return Err(format!(
            "{label}: global vendor config bytes changed during the run"
        ));
    }
    reports.push(format!(
        "{label}: global config byte-stable ({} bytes) and sentinel server invisible",
        before.len()
    ));

    let leaks = scan_collected(ctx.fake_secret.as_str(), &[&outcome]);
    if !leaks.is_empty() {
        return Err(format!("{label}: secret leak: {leaks:?}"));
    }
    assert_tmp_clean(&scenario.workspace, &format!("{label} global"), reports)?;

    *counters.entry("E2E-MCP-005".into()).or_default() += 1;
    reports.push(format!("{label}: PASS (global invisible, bytes unchanged)"));
    Ok(())
}

async fn e2e_006_failure_and_cancel_cleanup(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-006 {}", runner_label(case.runner));

    let fail_log = scenario.workspace.join("fail-mcp.log");
    let fail_member = MemberSpec {
        name: "FailMember".to_string(),
        servers: vec![local_mcp_server(ctx, "fail-mcp", "fail", &fail_log)],
        hang: false,
        fail: true,
        no_mcp: false,
        prompt_tag: "fail".to_string(),
    };
    // Failure path: the fake CLI exits nonzero; the run should still release
    // every private MCP directory.
    let fail_outcome = run_member(
        ctx,
        case,
        &fail_member,
        "failure cleanup",
        Uuid::new_v4(),
        &scenario.workspace,
        &scenario.home,
    )
    .await
    .map_err(|e| format!("{label}: failure run: {e}"))?;
    if fail_outcome.exit_ok {
        return Err(format!(
            "{label}: fake CLI failure path completed successfully instead of returning a failed exit"
        ));
    }
    assert_tmp_clean(&scenario.workspace, &format!("{label} failure"), reports)?;

    // Cancel path: hang the fake CLI, cancel, then release cleanup.
    let cancel_log = scenario.workspace.join("cancel-mcp.log");
    let cancel_member = MemberSpec {
        name: "CancelMember".to_string(),
        servers: vec![local_mcp_server(ctx, "cancel-mcp", "cancel", &cancel_log)],
        hang: true,
        fail: false,
        no_mcp: false,
        prompt_tag: "cancel".to_string(),
    };
    let (agent, session_agent) =
        create_member(&ctx.pool, &cancel_member, case.runner, &scenario.workspace).await;
    let mut env = ExecutionEnv::new(
        RepoContext::new(scenario.workspace.clone(), Vec::new()),
        false,
        String::new(),
    );
    env.insert(
        "PATH",
        format!(
            "{}:{}",
            ctx.fixture_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    env.insert("HOME", scenario.home.to_string_lossy().into_owned());
    env.insert("NO_UPDATE_NOTIFIER", "1");
    env.insert("NO_COLOR", "1");
    env.insert("CI", "1");
    env.insert(
        "OPENTEAMS_PI_QA_NPX_PATH",
        ctx.fixture_bin.join("npx").to_string_lossy().into_owned(),
    );
    let cancel_protocol = scenario.workspace.join("protocol-cancel.jsonl");
    env.insert(
        family_protocol_log_env(case),
        cancel_protocol.to_string_lossy().into_owned(),
    );
    env.insert(family_secret_env(case), ctx.fake_secret.clone());
    if let Some(key) = family_mode_env(case) {
        env.insert(key, case.fixture_mode.to_string());
    }
    env.insert(family_hang_env(case), "1");
    env.insert(family_fail_env(case), "0");
    env.insert(family_no_mcp_env(case), "0");

    let (_, executor, prepared) =
        services::services::member_execution::build_effective_member_executor_for_run(
            &ctx.pool,
            &agent,
            &session_agent,
            &scenario.workspace,
            Uuid::new_v4(),
            &mut env,
        )
        .await
        .map_err(|e| format!("{label}: cancel prep: {e:#}"))?;
    let mut spawned = executor
        .spawn(&scenario.workspace, "[qa-tag:cancel] cancel run", &env)
        .await
        .map_err(|e| format!("{label}: cancel spawn: {e:#}"))?;
    spawned.cleanup = executors::executors::ExecutorRunCleanup::combine(
        spawned.cleanup.take(),
        prepared.into_cleanup(),
    );
    // Give the fake CLI a moment to actually start and create its run resources.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    kill_child(&mut spawned);
    drop(spawned);

    assert_tmp_clean(&scenario.workspace, &format!("{label} cancel"), reports)?;

    let cancel_text = read_optional(&cancel_protocol);
    if cancel_text.contains(ctx.fake_secret.as_str()) {
        return Err(format!(
            "{label}: cancel protocol log leaked the fake secret"
        ));
    }

    *counters.entry("E2E-MCP-006".into()).or_default() += 1;
    reports.push(format!(
        "{label}: PASS (failure + cancel private dirs cleaned)"
    ));
    Ok(())
}

async fn e2e_010_redaction_and_fail_closed(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-010 {}", runner_label(case.runner));

    // Fail closed: an invalid member MCP configuration must be rejected during
    // production preparation, before any CLI process is spawned, and the error
    // must not echo the fake secret.
    let mut env = ExecutionEnv::new(
        RepoContext::new(scenario.workspace.clone(), Vec::new()),
        false,
        String::new(),
    );
    env.insert(
        "PATH",
        format!(
            "{}:{}",
            ctx.fixture_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    env.insert("HOME", scenario.home.to_string_lossy().into_owned());
    env.insert(
        "OPENTEAMS_PI_QA_NPX_PATH",
        ctx.fixture_bin.join("npx").to_string_lossy().into_owned(),
    );

    // An invalid header value containing the fake secret must fail validation.
    let invalid_servers = vec![(
        "leaky".to_string(),
        json!({
            "type": "http",
            "url": format!("{}://127.0.0.1:9/mcp", "http"),
            "headers": {"Authorization": {"secret": ctx.fake_secret}}
        }),
    )];
    let invalid_member = MemberSpec {
        name: "InvalidMember".to_string(),
        servers: invalid_servers,
        hang: false,
        fail: false,
        no_mcp: false,
        prompt_tag: "invalid".to_string(),
    };
    let (agent, session_agent) =
        create_member(&ctx.pool, &invalid_member, case.runner, &scenario.workspace).await;

    let result = services::services::member_execution::build_effective_member_executor_for_run(
        &ctx.pool,
        &agent,
        &session_agent,
        &scenario.workspace,
        Uuid::new_v4(),
        &mut env,
    )
    .await;
    let error = match result {
        Ok(_) => {
            return Err(format!(
                "{label}: invalid MCP config was not rejected fail-closed"
            ));
        }
        Err(error) => format!("{error:#}"),
    };
    if error.contains(ctx.fake_secret.as_str()) {
        return Err(format!(
            "{label}: production preparation error leaked the fake secret: {error}"
        ));
    }
    reports.push(format!(
        "{label}: fail-closed error is secret-free: {}",
        truncate(&error, 160)
    ));
    assert_tmp_clean(&scenario.workspace, &format!("{label} invalid"), reports)?;

    // Not-initialized member config must fail before spawn with a typed error.
    let no_mcp_member = MemberSpec {
        name: "NoInitMember".to_string(),
        servers: Vec::new(),
        hang: false,
        fail: false,
        no_mcp: false,
        prompt_tag: "noinit".to_string(),
    };
    let (agent2, session_agent2) =
        create_member_no_mcp(&ctx.pool, &no_mcp_member, case.runner).await;
    let result2 = services::services::member_execution::build_effective_member_executor_for_run(
        &ctx.pool,
        &agent2,
        &session_agent2,
        &scenario.workspace,
        Uuid::new_v4(),
        &mut env,
    )
    .await;
    if result2.is_ok() {
        return Err(format!(
            "{label}: uninitialized MCP config was not rejected"
        ));
    }

    *counters.entry("E2E-MCP-010".into()).or_default() += 1;
    reports.push(format!(
        "{label}: PASS (invalid/uninitialized config fails closed, secret redacted)"
    ));
    Ok(())
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out: String = value.chars().take(max).collect();
        out.push_str("...");
        out
    }
}

async fn create_member_no_mcp(
    pool: &SqlitePool,
    member: &MemberSpec,
    runner: BaseCodingAgent,
) -> (ChatAgent, ChatSessionAgent) {
    let agent = ChatAgent::create(
        pool,
        &CreateChatAgent {
            name: member.name.clone(),
            runner_type: runner.to_string(),
            system_prompt: Some("Offline E2E agent.".to_string()),
            tools_enabled: Some(serde_json::json!({})),
            model_name: None,
            owner_project_id: None,
        },
        Uuid::new_v4(),
    )
    .await
    .expect("create chat agent");
    let session = ChatSession::create(
        pool,
        &CreateChatSession {
            title: Some(format!("E2E no-MCP session {}", member.name)),
            workspace_path: None,
            project_id: None,
            worktree_mode: Some(ChatSessionWorktreeMode::Disabled),
        },
        Uuid::new_v4(),
    )
    .await
    .expect("create chat session");
    let session_agent = ChatSessionAgent::create(
        pool,
        &CreateChatSessionAgent {
            session_id: session.id,
            agent_id: agent.id,
            member_name: Some(member.name.clone()),
            workspace_path: None,
            allowed_skill_ids: Vec::new(),
            project_member_id: None,
            execution_config: MemberExecutionConfig {
                // Deliberately no `mcp` key: legacy/uninitialized member.
                acp: Some(AcpExecutionOptions {
                    approval_mode: Some(executors::executors::acp::AcpApprovalMode::AutoAllow),
                    ..Default::default()
                }),
                ..Default::default()
            },
        },
        Uuid::new_v4(),
    )
    .await
    .expect("create chat session agent");
    (agent, session_agent)
}

async fn e2e_007_template_application(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-007 {}", runner_label(case.runner));

    // A team template carries each member's execution_config (including the
    // canonical member MCP). Production template application persists that
    // member config through the project-member service and syncs it to the
    // session agent. Build the template member's execution config.
    let templated_log = scenario.workspace.join("template-mcp.log");
    let template_servers = vec![local_mcp_server(
        ctx,
        "template-mcp",
        "template",
        &templated_log,
    )];
    let template_execution_config = MemberExecutionConfig {
        mcp: Some(canonical_mcp(template_servers.clone())),
        runner_type: Some(case.runner),
        acp: Some(AcpExecutionOptions {
            approval_mode: Some(executors::executors::acp::AcpApprovalMode::AutoAllow),
            ..Default::default()
        }),
        ..Default::default()
    };

    let project_id = Uuid::new_v4();
    db::models::project::Project::create(
        &ctx.pool,
        &db::models::project::CreateProject {
            name: "Template Project".to_string(),
            repositories: Vec::new(),
            description: None,
            status: None,
            default_workspace_path: Some(scenario.workspace.to_string_lossy().into_owned()),
            active_repo_id: None,
        },
        project_id,
    )
    .await
    .map_err(|e| format!("{label}: create project: {e}"))?;

    let session = ChatSession::create(
        &ctx.pool,
        &CreateChatSession {
            title: Some("Template Session".to_string()),
            workspace_path: Some(scenario.workspace.to_string_lossy().into_owned()),
            project_id: Some(project_id),
            worktree_mode: Some(ChatSessionWorktreeMode::Disabled),
        },
        Uuid::new_v4(),
    )
    .await
    .map_err(|e| format!("{label}: create session: {e}"))?;

    let agent = ChatAgent::create(
        &ctx.pool,
        &CreateChatAgent {
            name: "TemplatedAgent".to_string(),
            runner_type: case.runner.to_string(),
            system_prompt: Some("Template-applied agent.".to_string()),
            tools_enabled: Some(serde_json::json!({})),
            model_name: None,
            owner_project_id: None,
        },
        Uuid::new_v4(),
    )
    .await
    .map_err(|e| format!("{label}: create agent: {e}"))?;

    // Production template application: persist the template member config to a
    // project member; `add_member` also creates the linked session agent.
    let member_service = services::services::project_member::ProjectMemberService::new();
    let project_member = member_service
        .add_member(
            &ctx.pool,
            project_id,
            db::models::project_member::ProjectMemberType::Agent,
            None,
            Some(agent.id),
            Some("TemplatedMember".to_string()),
            None,
            0,
            Some(scenario.workspace.to_string_lossy().into_owned()),
            Vec::new(),
            true,
            template_execution_config,
        )
        .await
        .map_err(|e| format!("{label}: template apply (add_member): {e:#}"))?;

    let session_agent = ChatSessionAgent::find_by_session_and_project_member(
        &ctx.pool,
        session.id,
        project_member.id,
    )
    .await
    .map_err(|e| format!("{label}: find templated session agent: {e}"))?
    .ok_or_else(|| format!("{label}: template member session agent missing"))?;

    let outcome = run_member_with(
        ctx,
        case,
        &member_with("TemplatedMember", "template", template_servers),
        "template applied run",
        Uuid::new_v4(),
        &scenario.workspace,
        &scenario.home,
        agent,
        session_agent,
    )
    .await
    .map_err(|e| format!("{label}: run: {e}"))?;

    if !outcome.exit_ok {
        return Err(format!("{label}: templated run did not complete"));
    }
    assert_connected_exactly(&outcome, &["template-mcp"])?;
    assert_mcp_log_tags(&templated_log, "template", "unused", &label)?;

    let leaks = scan_collected(ctx.fake_secret.as_str(), &[&outcome]);
    if !leaks.is_empty() {
        return Err(format!("{label}: secret leak: {leaks:?}"));
    }
    assert_tmp_clean(&scenario.workspace, &format!("{label} template"), reports)?;

    *counters.entry("E2E-MCP-007".into()).or_default() += 1;
    reports.push(format!(
        "{label}: PASS (template member MCP applied through project member + session agent)"
    ));
    Ok(())
}

async fn e2e_008_session_export_reapply(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-008 {}", runner_label(case.runner));

    // Session export serializes each member's execution_config (including the
    // canonical member MCP) into the preset snapshot representation. Re-apply
    // parses that representation back into a fresh member config.
    let export_log = scenario.workspace.join("export-mcp.log");
    let export_servers = vec![local_mcp_server(ctx, "export-mcp", "export", &export_log)];
    let exported_config = MemberExecutionConfig {
        mcp: Some(canonical_mcp(export_servers.clone())),
        runner_type: Some(case.runner),
        acp: Some(AcpExecutionOptions {
            approval_mode: Some(executors::executors::acp::AcpApprovalMode::AutoAllow),
            ..Default::default()
        }),
        ..Default::default()
    };

    // Production MemberExecutionConfig serialization (the exact JSON object a
    // preset snapshot stores per member) round-trips through a template preset.
    let export_json = serde_json::to_value(&exported_config)
        .map_err(|e| format!("{label}: serialize exported config: {e}"))?;
    let reimported: MemberExecutionConfig = serde_json::from_value(export_json)
        .map_err(|e| format!("{label}: reimport exported config: {e}"))?;
    if reimported.mcp != exported_config.mcp {
        return Err(format!("{label}: exported MCP did not survive re-import"));
    }

    // Apply the exported (re-imported) config to a brand-new member through the
    // production project-member path, then run.
    let project_id = Uuid::new_v4();
    db::models::project::Project::create(
        &ctx.pool,
        &db::models::project::CreateProject {
            name: "Export Project".to_string(),
            repositories: Vec::new(),
            description: None,
            status: None,
            default_workspace_path: Some(scenario.workspace.to_string_lossy().into_owned()),
            active_repo_id: None,
        },
        project_id,
    )
    .await
    .map_err(|e| format!("{label}: create project: {e}"))?;
    let session = ChatSession::create(
        &ctx.pool,
        &CreateChatSession {
            title: Some("Export Session".to_string()),
            workspace_path: Some(scenario.workspace.to_string_lossy().into_owned()),
            project_id: Some(project_id),
            worktree_mode: Some(ChatSessionWorktreeMode::Disabled),
        },
        Uuid::new_v4(),
    )
    .await
    .map_err(|e| format!("{label}: create session: {e}"))?;
    let agent = ChatAgent::create(
        &ctx.pool,
        &CreateChatAgent {
            name: "ExportAgent".to_string(),
            runner_type: case.runner.to_string(),
            system_prompt: Some("Export-reapplied agent.".to_string()),
            tools_enabled: Some(serde_json::json!({})),
            model_name: None,
            owner_project_id: None,
        },
        Uuid::new_v4(),
    )
    .await
    .map_err(|e| format!("{label}: create agent: {e}"))?;

    let member_service = services::services::project_member::ProjectMemberService::new();
    let project_member = member_service
        .add_member(
            &ctx.pool,
            project_id,
            db::models::project_member::ProjectMemberType::Agent,
            None,
            Some(agent.id),
            Some("ExportMember".to_string()),
            None,
            0,
            Some(scenario.workspace.to_string_lossy().into_owned()),
            Vec::new(),
            true,
            reimported,
        )
        .await
        .map_err(|e| format!("{label}: re-apply exported config: {e:#}"))?;
    let session_agent = ChatSessionAgent::find_by_session_and_project_member(
        &ctx.pool,
        session.id,
        project_member.id,
    )
    .await
    .map_err(|e| format!("{label}: find re-applied session agent: {e}"))?
    .ok_or_else(|| format!("{label}: re-applied session agent missing"))?;

    let outcome = run_member_with(
        ctx,
        case,
        &member_with("ExportMember", "export", export_servers),
        "session export reapply run",
        Uuid::new_v4(),
        &scenario.workspace,
        &scenario.home,
        agent,
        session_agent,
    )
    .await
    .map_err(|e| format!("{label}: run: {e}"))?;

    if !outcome.exit_ok {
        return Err(format!("{label}: export reapply run did not complete"));
    }
    assert_connected_exactly(&outcome, &["export-mcp"])?;
    assert_mcp_log_tags(&export_log, "export", "unused", &label)?;

    let leaks = scan_collected(ctx.fake_secret.as_str(), &[&outcome]);
    if !leaks.is_empty() {
        return Err(format!("{label}: secret leak: {leaks:?}"));
    }
    assert_tmp_clean(&scenario.workspace, &format!("{label} export"), reports)?;

    *counters.entry("E2E-MCP-008".into()).or_default() += 1;
    reports.push(format!(
        "{label}: PASS (session-exported member MCP re-applied and connected)"
    ));
    Ok(())
}

/// Write the runner's native-format vendor MCP config with one valid stdio
/// server, used by the one-time migration scenario.
fn write_native_global_mcp(
    case: &CliMcpE2eCase,
    home: &Path,
    server_script: &Path,
    secret: &str,
) -> PathBuf {
    let path = home.join(case.global_config_rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create native config parent");
    }
    let body = match case.runner {
        BaseCodingAgent::Codex => format!(
            "[mcp_servers.migrated-mcp]\ncommand = \"node\"\nargs = [\"{}\"]\n[mcp_servers.migrated-mcp.env]\nMCP_SERVER_FAKE_SECRET = \"{secret}\"\n",
            server_script.display()
        ),
        BaseCodingAgent::Hermes => format!(
            "mcp_servers:\n  migrated-mcp:\n    command: node\n    args: [\"{}\"]\n    env:\n      MCP_SERVER_FAKE_SECRET: \"{secret}\"\n",
            server_script.display()
        ),
        BaseCodingAgent::Amp => format!(
            "{{\"amp.mcpServers\": {{\"migrated-mcp\": {{\"type\":\"local\",\"command\":\"node\",\"args\":[\"{}\"],\"env\":{{\"MCP_SERVER_FAKE_SECRET\":\"{secret}\"}}}}}}}}\n",
            server_script.display()
        ),
        BaseCodingAgent::Opencode | BaseCodingAgent::OpenTeamsCli => format!(
            "{{\"mcp\": {{\"migrated-mcp\": {{\"type\":\"local\",\"command\":[\"node\",\"{}\"],\"env\":{{\"MCP_SERVER_FAKE_SECRET\":\"{secret}\"}}}}}}}}\n",
            server_script.display()
        ),
        _ => format!(
            "{{\"mcpServers\": {{\"migrated-mcp\": {{\"command\":\"node\",\"args\":[\"{}\"],\"env\":{{\"MCP_SERVER_FAKE_SECRET\":\"{secret}\"}}}}}}}}\n",
            server_script.display()
        ),
    };
    fs::write(&path, body.as_bytes()).expect("write native global MCP");
    path
}

async fn e2e_009_one_time_migration(
    ctx: &ScenarioEnv,
    case: &CliMcpE2eCase,
    counters: &mut BTreeMap<String, usize>,
    reports: &mut Vec<String>,
) -> Result<(), String> {
    let scenario = new_scenario();
    set_process_home(&scenario.home);
    let label = format!("E2E-MCP-009 {}", runner_label(case.runner));

    // Write a legacy vendor MCP config (with the fixed fake secret) at the
    // runner's default path; the production migration canonicalizes it into the
    // member-scoped config exactly once.
    let server_script = ctx.fixture_bin.join("mcp_server_stdio.mjs");
    let global_path =
        write_native_global_mcp(case, &scenario.home, &server_script, &ctx.fake_secret);
    let global_bytes = fs::read(&global_path).expect("read legacy global config");

    if matches!(
        case.runner,
        BaseCodingAgent::Amp | BaseCodingAgent::Opencode
    ) {
        let profiles = ExecutorConfigs::get_cached();
        let executor = profiles
            .get_coding_agent(&ExecutorProfileId::new(case.runner))
            .ok_or_else(|| format!("{label}: missing executor profile"))?;
        if executor.default_mcp_config_path().as_deref() != Some(global_path.as_path()) {
            return Err(format!(
                "{label}: legacy vendor config was not written to the production MCP path"
            ));
        }
    }

    let agent_id = Uuid::new_v4();
    sqlx::query("INSERT OR IGNORE INTO chat_agents (id, name, runner_type) VALUES (?1, ?2, ?3)")
        .bind(agent_id)
        .bind(format!("legacy-agent-{}", runner_label(case.runner)))
        .bind(case.runner.to_string())
        .execute(&ctx.pool)
        .await
        .map_err(|e| format!("{label}: insert legacy agent: {e}"))?;

    let project_id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects (id, name) VALUES (?1, ?2)")
        .bind(project_id)
        .bind(format!("legacy-project-{}", runner_label(case.runner)))
        .execute(&ctx.pool)
        .await
        .map_err(|e| format!("{label}: insert legacy project: {e}"))?;

    let member_id = Uuid::new_v4();
    let legacy_exec = serde_json::json!({ "runner_type": case.runner.to_string() });
    sqlx::query(
        r#"INSERT INTO project_members (
               id, project_id, member_type, agent_id, allowed_skill_ids,
               execution_config, is_default
           ) VALUES (?1, ?2, 'agent', ?3, '[]', ?4, 0)"#,
    )
    .bind(member_id)
    .bind(project_id)
    .bind(agent_id)
    .bind(sqlx::types::Json(legacy_exec))
    .execute(&ctx.pool)
    .await
    .map_err(|e| format!("{label}: insert legacy member: {e}"))?;

    // Production one-time migration.
    let first =
        services::services::member_scoped_mcp_migration::run_member_scoped_mcp_migration(&ctx.pool)
            .await
            .map_err(|e| format!("{label}: first migration: {e}"))?;
    reports.push(format!(
        "{label}: first migration report: already_completed={} migrated_members={} runner_reads={}",
        first.already_completed, first.migrated_members, first.runner_reads
    ));
    if first.already_completed || first.migrated_members != 1 || first.runner_reads != 1 {
        return Err(format!(
            "{label}: unexpected first migration report {first:?}"
        ));
    }

    let migrated: MemberExecutionConfig =
        sqlx::query_scalar::<_, sqlx::types::Json<MemberExecutionConfig>>(
            "SELECT execution_config FROM project_members WHERE id = ?1",
        )
        .bind(member_id)
        .fetch_one(&ctx.pool)
        .await
        .map_err(|e| format!("{label}: read migrated config: {e}"))?
        .0;
    if migrated
        .mcp
        .as_ref()
        .map(|m| m.mcp_servers.len())
        .unwrap_or(0)
        != 1
        || !migrated
            .mcp
            .as_ref()
            .is_some_and(|m| m.mcp_servers.contains_key("migrated-mcp"))
    {
        return Err(format!(
            "{label}: migration did not move the legacy MCP server into the member config"
        ));
    }

    // Second run must be a no-op (already completed) and must not overwrite.
    let second =
        services::services::member_scoped_mcp_migration::run_member_scoped_mcp_migration(&ctx.pool)
            .await
            .map_err(|e| format!("{label}: second migration: {e}"))?;
    if !second.already_completed || second.migrated_members != 0 {
        return Err(format!(
            "{label}: migration was not a no-op on the second run: {second:?}"
        ));
    }
    let after: MemberExecutionConfig =
        sqlx::query_scalar::<_, sqlx::types::Json<MemberExecutionConfig>>(
            "SELECT execution_config FROM project_members WHERE id = ?1",
        )
        .bind(member_id)
        .fetch_one(&ctx.pool)
        .await
        .map_err(|e| format!("{label}: read config after second migration: {e}"))?
        .0;
    if after != migrated {
        return Err(format!(
            "{label}: second migration overwrote the migrated config"
        ));
    }
    if fs::read(&global_path).expect("read global after") != global_bytes {
        return Err(format!(
            "{label}: migration modified the legacy vendor config bytes"
        ));
    }

    *counters.entry("E2E-MCP-009".into()).or_default() += 1;
    reports.push(format!(
        "{label}: PASS (legacy MCP migrated once, second run no-op, vendor bytes stable)"
    ));
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let fixture_root = match TempDir::new() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("member_scoped_mcp_e2e: cannot create fixture root: {error}");
            std::process::exit(1);
        }
    };
    let bin = install_fixture_bin(fixture_root.path());

    let original_home = std::env::var_os("HOME");
    let original_userprofile = std::env::var_os("USERPROFILE");
    let original_xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
    let original_path = std::env::var_os("PATH");
    let fixture_home = fixture_root.path().join("process-home");
    let fixture_xdg_config_home = fixture_home.join(".config");
    fs::create_dir_all(&fixture_home).expect("create fixture process home");
    fs::create_dir_all(&fixture_xdg_config_home).expect("create fixture process XDG config home");
    let fixture_path = match &original_path {
        Some(path) => format!("{}:{}", bin.display(), path.to_string_lossy()),
        None => bin.to_string_lossy().into_owned(),
    };
    // `FrozenProcessCommand` resolves the production adapter command during
    // preparation, before the per-run ExecutionEnv is applied. Pin the test
    // process itself to the temporary HOME and fixture PATH first so every
    // production adapter resolves only our local protocol replacement.
    unsafe {
        std::env::set_var("HOME", &fixture_home);
        std::env::set_var("USERPROFILE", &fixture_home);
        std::env::set_var("XDG_CONFIG_HOME", &fixture_xdg_config_home);
        std::env::set_var("PATH", fixture_path);
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("member_scoped_mcp_e2e: cannot build runtime: {error}");
            std::process::exit(1);
        }
    };

    let (failures, reports) = runtime.block_on(async move {
        let mut failures: Vec<String> = Vec::new();
        let mut reports: Vec<String> = Vec::new();
        // Registry gate (pure production enum/profile computation).
        if let Err(error) = verify_registry_gate(&mut reports) {
            failures.push(error);
        }

        let mut counters: BTreeMap<String, usize> = BTreeMap::new();

        reports.push("carrier mapping table:".to_string());
        reports.push(format!(
            "  {:<16} {:<16} {:<26} {}",
            "runner", "fixture", "protocol family", "production MCP carrier"
        ));
        for case in MCP_E2E_REGISTRY {
            reports.push(format!(
                "  {:<16} {:<16} {:<26} {}",
                runner_label(case.runner),
                case.fixture_binary,
                case.family.label(),
                case.carrier
            ));
        }

        // Parameterized E2E-MCP-001..010 for every registry entry.
        for case in MCP_E2E_REGISTRY {
            let runner = runner_label(case.runner);
            // E2E-MCP-009 owns a database-level one-shot migration marker.
            // Give every runner a fresh migrated database so the same ten
            // cases actually execute for all fourteen registry entries.
            let case_db_root = fixture_root
                .path()
                .join("case-databases")
                .join(runner.to_ascii_lowercase());
            fs::create_dir_all(&case_db_root).expect("create runner case database root");
            let pool = setup_database(&case_db_root).await;
            let ctx = ScenarioEnv {
                fixture_bin: bin.clone(),
                pool: pool.clone(),
                fake_secret: FAKE_SECRET.to_string(),
                node_bin: "node".to_string(),
            };
            let mut runner_results: Vec<(String, Result<(), String>)> = Vec::new();

            macro_rules! run_scenario {
                ($number:literal, $scenario:ident) => {{
                    let number = $number;
                    let result = $scenario(&ctx, case, &mut counters, &mut reports).await;
                    if let Err(error) = &result {
                        failures.push(format!("runner={runner} E2E-MCP-{number}: {error}"));
                    }
                    runner_results.push((number.to_string(), result.map(|_| ())));
                }};
            }

            run_scenario!("001", e2e_001_chat_concurrent_isolation);
            run_scenario!("002", e2e_002_workflow_isolation);
            run_scenario!("003", e2e_003_empty_config_disables);
            run_scenario!("004", e2e_004_next_run_takes_effect);
            run_scenario!("005", e2e_005_global_invisible_and_byte_stable);
            run_scenario!("006", e2e_006_failure_and_cancel_cleanup);
            run_scenario!("007", e2e_007_template_application);
            run_scenario!("008", e2e_008_session_export_reapply);
            run_scenario!("009", e2e_009_one_time_migration);
            run_scenario!("010", e2e_010_redaction_and_fail_closed);

            let passed = runner_results.iter().filter(|(_, r)| r.is_ok()).count();
            reports.push(format!(
                "runner {runner}: {passed}/10 scenarios passed (E2E-MCP-001..010)"
            ));

            let sqlite_leaks = collect_sqlite_secret_scan(&pool, FAKE_SECRET).await;
            for leak in &sqlite_leaks {
                failures.push(format!("runner={runner} fake-secret scan: {leak}"));
            }
            reports.push(format!(
                "fake-secret scan {runner}: {} leaks across DB run/message/approval projections",
                sqlite_leaks.len()
            ));
        }

        // Execution-count report.
        let total_executed: usize = counters.values().sum();
        reports.push(format!(
            "execution counts: {:?} (total {total_executed} runner x scenario executions)",
            counters
        ));
        if total_executed != 14 * 10 {
            failures.push(format!(
                "expected 140 runner x scenario executions, got {total_executed}"
            ));
        }
        if counters.len() != 10 {
            failures.push(format!(
                "expected 10 scenario counters, got {}",
                counters.len()
            ));
        }
        (failures, reports)
    });

    // Restore every process-wide setting after the single-threaded run.
    unsafe {
        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match original_userprofile {
            Some(value) => std::env::set_var("USERPROFILE", value),
            None => std::env::remove_var("USERPROFILE"),
        }
        match original_xdg_config_home {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match original_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
    }

    println!("==== member-scoped MCP E2E report ====");
    for report in &reports {
        println!("{report}");
    }
    println!("==== end report ====");

    if failures.is_empty() {
        println!(
            "member_scoped_mcp_e2e: ALL 14 runners x 10 scenarios PASSED (offline, deterministic)"
        );
        std::process::exit(0);
    }
    eprintln!("member_scoped_mcp_e2e: {} failure(s):", failures.len());
    for failure in &failures {
        eprintln!("  - {failure}");
    }
    std::process::exit(1);
}
