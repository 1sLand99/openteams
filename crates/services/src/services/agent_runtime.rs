use std::{
    collections::{BTreeSet, HashMap},
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use dashmap::DashMap;
use executors::{
    command::{CmdOverrides, CommandBuilder, redacted_command},
    env::ExecutionEnv,
    executors::{
        AvailabilityInfo, BaseCodingAgent, CodingAgent, ExecutorError, StandardCodingAgentExecutor,
        acp::AcpCapabilityProbe, opencode::Opencode,
    },
    profile::{ExecutorConfig, ExecutorConfigs, ProfileError},
};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{process::Command, time::timeout};
use ts_rs::TS;

const STORE_FILE_NAME: &str = "agent_runtime_config.json";
const DISCOVERY_TTL: ChronoDuration = ChronoDuration::hours(24);
const RUNTIME_DISCOVERY_CONCURRENCY: usize = 4;
const ACP_PROBE_CACHE_TTL: Duration = Duration::from_secs(30);

static BACKGROUND_RUNTIME_REFRESH_RUNNING: AtomicBool = AtomicBool::new(false);
static RUNTIME_REFRESH_LOCK: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));
static RUNTIME_STORE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
static ACP_PROBE_GATES: LazyLock<DashMap<BaseCodingAgent, Arc<tokio::sync::Mutex<()>>>> =
    LazyLock::new(DashMap::new);
static ACP_PROBE_CACHE: LazyLock<DashMap<AcpProbeCacheKey, CachedAcpProbe>> =
    LazyLock::new(DashMap::new);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcpProbeCachePolicy {
    Reuse,
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AcpProbeCacheKey {
    runner: BaseCodingAgent,
    current_dir: PathBuf,
    auth_method_id: Option<String>,
    execution_fingerprint: [u8; 32],
}

#[derive(Debug, Clone)]
struct CachedAcpProbe {
    completed_at: Instant,
    probe: Option<AcpCapabilityProbe>,
}

#[derive(Debug, Error)]
pub enum AgentRuntimeError {
    #[error("invalid environment variable key: {0}")]
    InvalidEnvKey(String),
    #[error("unknown runner: {0}")]
    UnknownRunner(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Profile(#[from] ProfileError),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AgentRunMode {
    #[default]
    Auto,
    Local,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct AgentRuntimeConfig {
    pub runner_type: BaseCodingAgent,
    pub run_mode: AgentRunMode,
    pub env_json: HashMap<String, String>,
    #[serde(default)]
    #[ts(type = "JsonValue")]
    pub executor_options: Value,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct UpdateAgentRuntimeConfig {
    pub run_mode: Option<AgentRunMode>,
    pub env_json: Option<HashMap<String, String>>,
    #[ts(type = "JsonValue | null")]
    pub executor_options: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct AgentRuntimeEnvSummary {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AgentRuntimeModelSource {
    Runner,
    ProfileFallback,
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum AgentRuntimeAuthState {
    Authenticated,
    Unauthenticated,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentRuntimeStatus {
    pub runner_type: BaseCodingAgent,
    pub installed: bool,
    pub executable: bool,
    pub availability: AvailabilityInfo,
    pub auth_state: AgentRuntimeAuthState,
    /// Whether a Node.js runtime was detected on this machine. Drives the
    /// "install Node.js" guidance for Node-based runners.
    pub node_available: bool,
    pub discovered_models: Vec<String>,
    pub model_source: AgentRuntimeModelSource,
    pub version: Option<String>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub run_mode: AgentRunMode,
    pub env_summary: Vec<AgentRuntimeEnvSummary>,
    #[ts(type = "JsonValue")]
    pub executor_options: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum AgentRuntimeReasoningCapability {
    Effort { options: Vec<String> },
    Variant { options: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentRuntimeListResponse {
    pub runners: Vec<AgentRuntimeStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct AgentRuntimeRefreshError {
    pub runner_type: BaseCodingAgent,
    pub message: String,
    pub preserved_models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentRuntimeRefreshResponse {
    pub runners: Vec<AgentRuntimeStatus>,
    pub errors: Vec<AgentRuntimeRefreshError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct AgentRuntimeDiagnostics {
    pub runner_type: BaseCodingAgent,
    pub installed: bool,
    pub executable: bool,
    pub availability: AvailabilityInfo,
    pub auth_state: AgentRuntimeAuthState,
    pub node_available: bool,
    pub config_path: String,
    pub install_indicator_path: Option<String>,
    pub resolved_command: Option<String>,
    pub command_source: Option<String>,
    pub acp_probe: Option<AcpCapabilityProbe>,
    pub acp_probe_error: Option<String>,
    pub discovered_models: Vec<String>,
    pub model_source: AgentRuntimeModelSource,
    pub version: Option<String>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub run_mode: AgentRunMode,
    pub env_summary: Vec<AgentRuntimeEnvSummary>,
    #[ts(type = "JsonValue")]
    pub executor_options: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct AgentRuntimeDiscovery {
    models: Vec<String>,
    version: Option<String>,
    last_checked_at: DateTime<Utc>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
struct AgentRuntimeStore {
    #[serde(default)]
    configs: HashMap<BaseCodingAgent, AgentRuntimeConfig>,
    #[serde(default)]
    discoveries: HashMap<BaseCodingAgent, AgentRuntimeDiscovery>,
}

pub fn store_path() -> PathBuf {
    utils::assets::asset_dir().join(STORE_FILE_NAME)
}

pub fn list_runtime_statuses() -> Result<AgentRuntimeListResponse, AgentRuntimeError> {
    let store = read_store(&store_path())?;
    let profiles = ExecutorConfigs::get_cached();
    Ok(AgentRuntimeListResponse {
        runners: build_statuses(&profiles, &store),
    })
}

pub async fn list_runtime_statuses_with_discovery(
    current_dir: &Path,
) -> Result<AgentRuntimeListResponse, AgentRuntimeError> {
    let store = read_store(&store_path())?;
    let profiles = ExecutorConfigs::get_cached();
    let runners = build_statuses(&profiles, &store);

    if runtime_discovery_needs_refresh(&profiles, &store) {
        spawn_background_runtime_discovery(current_dir.to_path_buf());
    }

    Ok(AgentRuntimeListResponse { runners })
}

pub async fn refresh_runtime_discovery(
    current_dir: &Path,
) -> Result<AgentRuntimeRefreshResponse, AgentRuntimeError> {
    let _guard = RUNTIME_REFRESH_LOCK.lock().await;
    refresh_runtime_discovery_unlocked(current_dir).await
}

async fn refresh_runtime_discovery_unlocked(
    current_dir: &Path,
) -> Result<AgentRuntimeRefreshResponse, AgentRuntimeError> {
    let path = store_path();
    let store = read_store(&path)?;
    let profiles = ExecutorConfigs::get_cached();
    let current_dir = current_dir.to_path_buf();
    let store_snapshot = Arc::new(store.clone());
    let discovery_inputs = profiles
        .executors
        .iter()
        .map(|(runner, executor_config)| (*runner, executor_config.clone()))
        .collect::<Vec<_>>();

    let outcomes = stream::iter(
        discovery_inputs
            .into_iter()
            .map(|(runner, executor_config)| {
                let current_dir = current_dir.clone();
                let store = Arc::clone(&store_snapshot);
                async move {
                    discover_runner_runtime(runner, &executor_config, &store, &current_dir).await
                }
            }),
    )
    .buffer_unordered(RUNTIME_DISCOVERY_CONCURRENCY)
    .collect::<Vec<_>>()
    .await;

    let outcomes = outcomes.into_iter().collect::<Result<Vec<_>, _>>()?;
    let (store, errors) = update_store(&path, |latest| {
        Ok(apply_discovery_outcomes(latest, outcomes))
    })?;
    Ok(AgentRuntimeRefreshResponse {
        runners: build_statuses(&profiles, &store),
        errors,
    })
}

fn spawn_background_runtime_discovery(current_dir: PathBuf) {
    if BACKGROUND_RUNTIME_REFRESH_RUNNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }

    tokio::spawn(async move {
        let _refresh_guard = BackgroundRefreshGuard;
        let result = match RUNTIME_REFRESH_LOCK.try_lock() {
            Ok(_guard) => refresh_runtime_discovery_unlocked(&current_dir).await,
            Err(_) => return,
        };

        if let Err(err) = result {
            tracing::warn!("Failed to refresh agent runtime discovery in background: {err}");
        }
    });
}

struct BackgroundRefreshGuard;

impl Drop for BackgroundRefreshGuard {
    fn drop(&mut self) {
        BACKGROUND_RUNTIME_REFRESH_RUNNING.store(false, Ordering::Release);
    }
}

enum RunnerDiscoveryOutcome {
    Skipped,
    ModelsDiscovered {
        runner: BaseCodingAgent,
        models: Vec<String>,
        detected_version: Option<String>,
        version_error: Option<String>,
    },
    VersionOnly {
        runner: BaseCodingAgent,
        detected_version: Option<String>,
        version_error: Option<String>,
    },
    Failed {
        runner: BaseCodingAgent,
        message: String,
        detected_version: Option<String>,
        preserved_models: Vec<String>,
    },
}

fn apply_discovery_outcomes(
    store: &mut AgentRuntimeStore,
    outcomes: Vec<RunnerDiscoveryOutcome>,
) -> Vec<AgentRuntimeRefreshError> {
    let mut errors = Vec::new();
    for outcome in outcomes {
        match outcome {
            RunnerDiscoveryOutcome::Skipped => {}
            RunnerDiscoveryOutcome::ModelsDiscovered {
                runner,
                models,
                detected_version,
                version_error,
            } => {
                let version = version_for_discovery_update(store, runner, detected_version);
                let last_error =
                    version_error.map(|error| status_error_detail("version_check", error));
                if let Some(message) = last_error.clone() {
                    errors.push(AgentRuntimeRefreshError {
                        runner_type: runner,
                        message,
                        preserved_models: models.clone(),
                    });
                }
                store.discoveries.insert(
                    runner,
                    AgentRuntimeDiscovery {
                        models,
                        version,
                        last_checked_at: Utc::now(),
                        last_error,
                    },
                );
            }
            RunnerDiscoveryOutcome::VersionOnly {
                runner,
                detected_version,
                version_error,
            } => {
                let preserved_models = store
                    .discoveries
                    .get(&runner)
                    .map(|entry| entry.models.clone())
                    .unwrap_or_default();
                if let Some(message) = version_error
                    .clone()
                    .map(|error| status_error_detail("version_check", error))
                {
                    errors.push(AgentRuntimeRefreshError {
                        runner_type: runner,
                        message,
                        preserved_models,
                    });
                }
                cache_version_only_discovery(store, runner, detected_version, version_error);
            }
            RunnerDiscoveryOutcome::Failed {
                runner,
                message,
                detected_version,
                preserved_models,
            } => {
                store
                    .discoveries
                    .entry(runner)
                    .and_modify(|entry| {
                        entry.last_checked_at = Utc::now();
                        entry.last_error = Some(message.clone());
                        if let Some(version) = detected_version.clone() {
                            entry.version = Some(version);
                        }
                    })
                    .or_insert_with(|| AgentRuntimeDiscovery {
                        models: Vec::new(),
                        version: detected_version.clone(),
                        last_checked_at: Utc::now(),
                        last_error: Some(message.clone()),
                    });
                errors.push(AgentRuntimeRefreshError {
                    runner_type: runner,
                    message,
                    preserved_models,
                });
            }
        }
    }
    errors
}

async fn discover_runner_runtime(
    runner: BaseCodingAgent,
    executor_config: &ExecutorConfig,
    store: &AgentRuntimeStore,
    current_dir: &Path,
) -> Result<RunnerDiscoveryOutcome, AgentRuntimeError> {
    let Some(mut base) = executor_config
        .get_default()
        .or_else(|| executor_config.configurations.values().next())
        .cloned()
    else {
        return Ok(RunnerDiscoveryOutcome::Skipped);
    };

    let mut env = ExecutionEnv::new(Default::default(), false, String::new());
    apply_config_to_executor_and_env(runner, &mut base, &mut env, store)?;
    if !base.get_availability_info().is_available() {
        return Ok(RunnerDiscoveryOutcome::Skipped);
    }

    let (version_result, discovered_models) = tokio::join!(
        detect_cli_version(&base, &env),
        discover_models_for_executor(runner, &base, current_dir, &env)
    );
    let (detected_version, version_error) = split_probe_result(version_result);

    Ok(match discovered_models {
        Ok(Some(models)) => RunnerDiscoveryOutcome::ModelsDiscovered {
            runner,
            models,
            detected_version,
            version_error,
        },
        Ok(None) => RunnerDiscoveryOutcome::VersionOnly {
            runner,
            detected_version,
            version_error,
        },
        Err(message) => RunnerDiscoveryOutcome::Failed {
            runner,
            message: merge_status_error_details([
                Some(status_error_detail("model_discovery", message)),
                version_error.map(|error| status_error_detail("version_check", error)),
            ])
            .expect("model discovery failure always produces an error detail"),
            detected_version,
            preserved_models: models_for_runner(runner, executor_config, store),
        },
    })
}

pub fn update_runtime_config(
    runner: BaseCodingAgent,
    payload: UpdateAgentRuntimeConfig,
) -> Result<AgentRuntimeStatus, AgentRuntimeError> {
    let path = store_path();
    let profiles = ExecutorConfigs::get_cached();

    if !profiles.executors.contains_key(&runner) {
        return Err(AgentRuntimeError::UnknownRunner(runner.to_string()));
    }

    let (store, ()) = update_store(&path, |store| {
        let mut config = store
            .configs
            .get(&runner)
            .cloned()
            .unwrap_or_else(|| default_config(runner));

        if let Some(run_mode) = payload.run_mode {
            config.run_mode = run_mode;
        }
        if let Some(env_json) = payload.env_json {
            validate_env_json(&env_json)?;
            config.env_json = env_json;
        }
        if let Some(executor_options) = payload.executor_options {
            let mut executor = profiles
                .executors
                .get(&runner)
                .and_then(|entry| {
                    entry
                        .get_default()
                        .or_else(|| entry.configurations.values().next())
                })
                .cloned()
                .ok_or_else(|| AgentRuntimeError::UnknownRunner(runner.to_string()))?;
            apply_executor_options(runner, &mut executor, &executor_options)?;
            config.executor_options = executor_options;
        }
        config.updated_at = Utc::now();

        store.configs.insert(runner, config);
        Ok(())
    })?;

    let status = build_statuses(&profiles, &store)
        .into_iter()
        .find(|status| status.runner_type == runner)
        .ok_or_else(|| AgentRuntimeError::UnknownRunner(runner.to_string()))?;
    Ok(status)
}

pub async fn runtime_diagnostics(
    runner: BaseCodingAgent,
    probe_dir: &Path,
    auth_method_id: Option<&str>,
) -> Result<AgentRuntimeDiagnostics, AgentRuntimeError> {
    let path = store_path();
    let store = read_store(&path)?;
    let profiles = ExecutorConfigs::get_cached();
    let config = profiles
        .executors
        .get(&runner)
        .ok_or_else(|| AgentRuntimeError::UnknownRunner(runner.to_string()))?;
    let Some(base) = config
        .get_default()
        .or_else(|| config.configurations.values().next())
    else {
        return Err(AgentRuntimeError::UnknownRunner(runner.to_string()));
    };

    let cli_config_path = base
        .default_mcp_config_path()
        .map(|path| path.display().to_string());
    let node_available = detect_node_available();
    let status = build_status(runner, config, base, &store, node_available);
    let mut runtime_executor = base.clone();
    let mut env = ExecutionEnv::new(Default::default(), false, String::new());
    apply_config_to_executor_and_env(runner, &mut runtime_executor, &mut env, &store)?;

    let version_result = if status.installed {
        detect_cli_version(&runtime_executor, &env).await
    } else {
        Ok(None)
    };
    let (detected_version, version_error) = split_probe_result(version_result);
    let (resolved_runtime_command, command_error) = split_probe_result(
        resolve_runtime_command_for_diagnostics(status.installed, &runtime_executor).await,
    );
    let command_source = resolved_runtime_command.as_ref().map(|_| {
        if cmd_overrides_for_executor(&runtime_executor)
            .and_then(|cmd| cmd.base_command_override.as_deref())
            .is_some_and(|command| !command.trim().is_empty())
        {
            "override".to_string()
        } else {
            match &runtime_executor {
                CodingAgent::Gemini(_)
                | CodingAgent::QwenCode(_)
                | CodingAgent::KimiCode(_)
                | CodingAgent::QoderCli(_) => "native",
                _ => "default",
            }
            .to_string()
        }
    });
    let install_indicator_path = resolved_runtime_command
        .as_ref()
        .map(|command| command.executable_path.clone());
    let resolved_command = resolved_runtime_command.map(|command| command.rendered);
    let (acp_probe, acp_probe_error, acp_probe_succeeded) = if status.installed {
        match coordinated_probe_acp(
            runner,
            &runtime_executor,
            probe_dir,
            &env,
            auth_method_id,
            AcpProbeCachePolicy::Reuse,
        )
        .await
        {
            Ok(probe) => {
                let succeeded = probe.is_some();
                (probe, None, succeeded)
            }
            Err(error) => (None, Some(error.to_string()), false),
        }
    } else {
        (None, None, false)
    };
    let latest_store = if detected_version.is_some() || acp_probe_succeeded {
        update_store(&path, |latest| {
            if let Some(version) = detected_version.as_deref() {
                cache_runner_version(latest, runner, version.to_string());
            }
            if acp_probe_succeeded {
                clear_cached_authentication_required_error(latest, runner);
            }
            Ok(())
        })?
        .0
    } else {
        read_store(&path)?
    };
    let latest_status = build_status(runner, config, base, &latest_store, node_available);
    let version = detected_version.or(latest_status.version.clone());
    let last_error = merge_status_error_details([
        latest_status.last_error.clone(),
        version_error.map(|error| status_error_detail("version_check", error)),
        command_error.map(|error| status_error_detail("command_resolution", error)),
        acp_probe_error
            .clone()
            .map(|error| status_error_detail("acp_probe", error)),
    ]);

    Ok(AgentRuntimeDiagnostics {
        runner_type: latest_status.runner_type,
        installed: latest_status.installed,
        executable: latest_status.executable,
        availability: latest_status.availability,
        auth_state: latest_status.auth_state,
        node_available: latest_status.node_available,
        config_path: cli_config_path
            .clone()
            .unwrap_or_else(|| path.display().to_string()),
        install_indicator_path,
        resolved_command,
        command_source,
        acp_probe,
        acp_probe_error,
        discovered_models: latest_status.discovered_models,
        model_source: latest_status.model_source,
        version,
        last_checked_at: latest_status.last_checked_at,
        last_error,
        run_mode: latest_status.run_mode,
        env_summary: latest_status.env_summary,
        executor_options: latest_status.executor_options,
    })
}

struct ResolvedRuntimeCommand {
    executable_path: String,
    rendered: String,
}

async fn resolve_runtime_command_for_diagnostics(
    installed: bool,
    executor: &CodingAgent,
) -> Result<Option<ResolvedRuntimeCommand>, String> {
    if !installed {
        return Ok(None);
    }
    resolve_runtime_command(executor).await
}

async fn resolve_runtime_command(
    executor: &CodingAgent,
) -> Result<Option<ResolvedRuntimeCommand>, String> {
    let Some(base) = version_command_base(executor) else {
        return Ok(None);
    };
    let parts = CommandBuilder::new(base).build_initial().map_err(|error| {
        command_failure_detail(
            "<configured command could not be parsed>",
            "parse runtime command",
            error,
        )
    })?;
    let unresolved_command = parts.redacted_display();
    let (executable, args) = parts.into_resolved().await.map_err(|error| {
        command_failure_detail(&unresolved_command, "resolve runtime executable", error)
    })?;
    let executable_path = executable.display().to_string();
    let rendered = redacted_command(&executable_path, &args);
    Ok(Some(ResolvedRuntimeCommand {
        executable_path,
        rendered,
    }))
}

pub fn apply_agent_runtime_config(
    runner: BaseCodingAgent,
    executor: &mut CodingAgent,
    env: &mut ExecutionEnv,
) -> Result<(), AgentRuntimeError> {
    let store = read_store(&store_path())?;
    apply_config_to_executor_and_env(runner, executor, env, &store)?;
    Ok(())
}

fn apply_config_to_executor_and_env(
    runner: BaseCodingAgent,
    executor: &mut CodingAgent,
    env: &mut ExecutionEnv,
    store: &AgentRuntimeStore,
) -> Result<(), AgentRuntimeError> {
    if let Some(config) = store.configs.get(&runner) {
        merge_agent_env_without_overwriting_session(env, &config.env_json);
        apply_executor_options(runner, executor, &config.executor_options)?;
    }
    Ok(())
}

fn apply_executor_options(
    runner: BaseCodingAgent,
    executor: &mut CodingAgent,
    executor_options: &Value,
) -> Result<(), AgentRuntimeError> {
    let Some(options) = executor_options
        .as_object()
        .filter(|options| !options.is_empty())
    else {
        return Ok(());
    };

    let tag = serde_json::to_value(runner)?
        .as_str()
        .unwrap_or_default()
        .to_string();
    let mut wrapped = serde_json::to_value(&*executor)?;
    let Value::Object(root) = &mut wrapped else {
        return Ok(());
    };
    let Some(inner) = root.get_mut(&tag) else {
        return Ok(());
    };

    merge_json_object(inner, &Value::Object(options.clone()));
    *executor = serde_json::from_value(wrapped)?;
    Ok(())
}

fn merge_json_object(target: &mut Value, source: &Value) {
    match (target, source) {
        (Value::Object(target_map), Value::Object(source_map)) => {
            for (key, value) in source_map {
                match (target_map.get_mut(key), value) {
                    (Some(existing @ Value::Object(_)), Value::Object(_)) => {
                        merge_json_object(existing, value);
                    }
                    _ => {
                        target_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (target, source) => {
            *target = source.clone();
        }
    }
}

fn merge_agent_env_without_overwriting_session(
    env: &mut ExecutionEnv,
    agent_env: &HashMap<String, String>,
) {
    for (key, value) in agent_env {
        if !env.contains_key(key) {
            env.insert(key.clone(), value.clone());
        }
    }
}

async fn detect_cli_version(
    executor: &CodingAgent,
    env: &ExecutionEnv,
) -> Result<Option<String>, String> {
    let Some(base) = version_command_base(executor) else {
        return Ok(None);
    };
    let parts = CommandBuilder::new(base)
        .extend_params(["--version"])
        .build_initial()
        .map_err(|error| {
            command_failure_detail(
                "<configured command could not be parsed>",
                "parse version command",
                error,
            )
        })?;
    let unresolved_command = parts.redacted_display();
    let parts = parts.into_resolved().await.map_err(|error| {
        command_failure_detail(&unresolved_command, "resolve version executable", error)
    })?;
    let (executable_path, args) = parts;
    let command_display = redacted_command(&executable_path.display().to_string(), &args);

    let mut command = Command::new(executable_path);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(cmd_overrides) = cmd_overrides_for_executor(executor) {
        env.clone()
            .with_profile(cmd_overrides)
            .apply_to_command(&mut command);
    } else {
        env.apply_to_command(&mut command);
    }

    let output = timeout(Duration::from_secs(12), command.output())
        .await
        .map_err(|_| {
            command_failure_detail(
                &command_display,
                "execute version command",
                "timed out after 12 seconds",
            )
        })?
        .map_err(|error| {
            command_failure_detail(&command_display, "execute version command", error)
        })?;
    if !output.status.success() {
        let status = output
            .status
            .code()
            .map(|code| format!("exit code {code}"))
            .unwrap_or_else(|| "terminated by signal".to_string());
        let evidence = normalize_cli_version_output(&output.stderr, &output.stdout)
            .map(|line| format!(": {line}"))
            .unwrap_or_default();
        return Err(command_failure_detail(
            &command_display,
            "execute version command",
            format!("process failed with {status}{evidence}"),
        ));
    }

    normalize_cli_version_output(&output.stdout, &output.stderr)
        .map(Some)
        .ok_or_else(|| {
            command_failure_detail(
                &command_display,
                "parse version output",
                "process exited successfully but produced no version output",
            )
        })
}

fn command_failure_detail(
    command: &str,
    operation: &str,
    result: impl std::fmt::Display,
) -> String {
    format!(
        "command=`{command}`; operation={operation}; result={}",
        result.to_string().trim()
    )
}

fn version_command_base(executor: &CodingAgent) -> Option<String> {
    if let Some(base_override) = cmd_overrides_for_executor(executor)
        .and_then(|cmd| cmd.base_command_override.as_deref())
        .map(str::trim)
        .filter(|base| !base.is_empty())
    {
        return Some(base_override.to_string());
    }

    Some(match executor {
        CodingAgent::ClaudeCode(config) => {
            if config.claude_code_router.unwrap_or(false) {
                "npx -y @musistudio/claude-code-router@2.0.0".to_string()
            } else {
                "npx -y @anthropic-ai/claude-code@2.1.161".to_string()
            }
        }
        CodingAgent::Amp(_) => "amp".to_string(),
        CodingAgent::Gemini(_) => "gemini".to_string(),
        CodingAgent::Codex(_) => "npx -y @openai/codex@0.144.1".to_string(),
        CodingAgent::Opencode(_) => {
            format!("npx -y opencode-ai@{}", Opencode::PACKAGE_VERSION)
        }
        CodingAgent::OpenTeamsCli(_) => openteams_cli_binary_base(),
        CodingAgent::CursorAgent(_) => "cursor-agent".to_string(),
        CodingAgent::QwenCode(_) => "qwen".to_string(),
        CodingAgent::Copilot(_) => "copilot".to_string(),
        CodingAgent::Droid(_) => "droid".to_string(),
        CodingAgent::KimiCode(_) => "kimi".to_string(),
        CodingAgent::QoderCli(_) => "qodercli".to_string(),
        #[cfg(feature = "qa-mode")]
        CodingAgent::QaMock(_) => return None,
        #[cfg(feature = "qa-mode")]
        CodingAgent::AcpQa(config) => config.command.clone(),
    })
}

fn cmd_overrides_for_executor(executor: &CodingAgent) -> Option<&CmdOverrides> {
    match executor {
        CodingAgent::ClaudeCode(config) => Some(&config.cmd),
        CodingAgent::Amp(config) => Some(&config.cmd),
        CodingAgent::Gemini(config) => Some(&config.cmd),
        CodingAgent::Codex(config) => Some(&config.cmd),
        CodingAgent::Opencode(config) => Some(&config.cmd),
        CodingAgent::OpenTeamsCli(config) => Some(&config.cmd),
        CodingAgent::CursorAgent(config) => Some(&config.cmd),
        CodingAgent::QwenCode(config) => Some(&config.cmd),
        CodingAgent::Copilot(config) => Some(&config.cmd),
        CodingAgent::Droid(config) => Some(&config.cmd),
        CodingAgent::KimiCode(config) => Some(&config.cmd),
        CodingAgent::QoderCli(config) => Some(&config.cmd),
        #[cfg(feature = "qa-mode")]
        CodingAgent::QaMock(_) => None,
        #[cfg(feature = "qa-mode")]
        CodingAgent::AcpQa(config) => Some(&config.cmd),
    }
}

fn openteams_cli_binary_base() -> String {
    let binary_name = if cfg!(windows) {
        "openteams-cli.exe"
    } else {
        "openteams-cli"
    };

    if let Ok(path) = std::env::var("OPENTEAMS_CLI_PATH") {
        let path = PathBuf::from(path);
        if path.exists() {
            return command_base_from_path(path);
        }
    }

    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let bundled = exe_dir.join(binary_name);
        if bundled.exists() {
            return command_base_from_path(bundled);
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        let dev_binary = cwd.join("binaries").join(binary_name);
        if dev_binary.exists() {
            return command_base_from_path(dev_binary);
        }
    }

    if let Some(home) = dirs::home_dir() {
        let bundled = home.join(".openteams").join("bin").join(binary_name);
        if bundled.exists() {
            return command_base_from_path(bundled);
        }
    }

    which::which("openteams-cli")
        .ok()
        .map(command_base_from_path)
        .unwrap_or_else(|| "openteams-cli".to_string())
}

fn command_base_from_path(path: PathBuf) -> String {
    let raw = path.to_string_lossy();
    if raw.contains(' ') {
        format!("\"{raw}\"")
    } else {
        raw.to_string()
    }
}

fn normalize_cli_version_output(stdout: &[u8], stderr: &[u8]) -> Option<String> {
    let stdout = String::from_utf8_lossy(stdout);
    first_version_line(&stdout).or_else(|| {
        let stderr = String::from_utf8_lossy(stderr);
        first_version_line(&stderr)
    })
}

fn first_version_line(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(160).collect())
}

fn cache_runner_version(store: &mut AgentRuntimeStore, runner: BaseCodingAgent, version: String) {
    let now = Utc::now();
    store
        .discoveries
        .entry(runner)
        .and_modify(|entry| {
            entry.version = Some(version.clone());
            entry.last_checked_at = now;
            entry.last_error =
                remove_status_error_stage(entry.last_error.as_deref(), "version_check");
        })
        .or_insert_with(|| AgentRuntimeDiscovery {
            models: Vec::new(),
            version: Some(version),
            last_checked_at: now,
            last_error: None,
        });
}

fn cache_version_only_discovery(
    store: &mut AgentRuntimeStore,
    runner: BaseCodingAgent,
    detected_version: Option<String>,
    version_error: Option<String>,
) {
    let now = Utc::now();
    let last_error = version_error.map(|error| status_error_detail("version_check", error));
    store
        .discoveries
        .entry(runner)
        .and_modify(|entry| {
            entry.last_checked_at = now;
            entry.last_error.clone_from(&last_error);
            if let Some(version) = detected_version.clone() {
                entry.version = Some(version);
            }
        })
        .or_insert_with(|| AgentRuntimeDiscovery {
            models: Vec::new(),
            version: detected_version,
            last_checked_at: now,
            last_error,
        });
}

fn version_for_discovery_update(
    store: &AgentRuntimeStore,
    runner: BaseCodingAgent,
    detected_version: Option<String>,
) -> Option<String> {
    detected_version.or_else(|| {
        store
            .discoveries
            .get(&runner)
            .and_then(|entry| entry.version.clone())
    })
}

fn acp_probe_cache_key(
    runner: BaseCodingAgent,
    executor: &CodingAgent,
    current_dir: &Path,
    env: &ExecutionEnv,
    auth_method_id: Option<&str>,
) -> Result<AcpProbeCacheKey, ExecutorError> {
    let mut hasher = Sha256::new();
    let executor_json = serde_json::to_vec(executor)?;
    hasher.update((executor_json.len() as u64).to_le_bytes());
    hasher.update(executor_json);

    let mut env_vars = env.vars.iter().collect::<Vec<_>>();
    env_vars.sort_unstable_by(|left, right| left.0.cmp(right.0));
    for (key, value) in env_vars {
        hasher.update((key.len() as u64).to_le_bytes());
        hasher.update(key.as_bytes());
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }

    Ok(AcpProbeCacheKey {
        runner,
        current_dir: current_dir.to_path_buf(),
        auth_method_id: auth_method_id.map(str::to_string),
        execution_fingerprint: hasher.finalize().into(),
    })
}

fn reusable_cached_acp_probe(
    key: &AcpProbeCacheKey,
    policy: AcpProbeCachePolicy,
    requested_at: Instant,
) -> Option<Option<AcpCapabilityProbe>> {
    let cached = ACP_PROBE_CACHE.get(key)?;
    let completed_during_request = cached.completed_at >= requested_at;
    let within_ttl = cached.completed_at.elapsed() <= ACP_PROBE_CACHE_TTL;
    (completed_during_request || (policy == AcpProbeCachePolicy::Reuse && within_ttl))
        .then(|| cached.probe.clone())
}

async fn run_coordinated_acp_probe<F, Fut>(
    key: AcpProbeCacheKey,
    policy: AcpProbeCachePolicy,
    probe: F,
) -> Result<Option<AcpCapabilityProbe>, ExecutorError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Option<AcpCapabilityProbe>, ExecutorError>>,
{
    let requested_at = Instant::now();
    if let Some(cached) = reusable_cached_acp_probe(&key, policy, requested_at) {
        return Ok(cached);
    }

    // Gemini and other ACP runners share user-level state. Serialize all probes
    // for one runner, then let identical waiters reuse the result produced while
    // they were waiting instead of starting another CLI process.
    let gate = ACP_PROBE_GATES
        .entry(key.runner)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = gate.lock().await;

    if let Some(cached) = reusable_cached_acp_probe(&key, policy, requested_at) {
        return Ok(cached);
    }

    let result = probe().await?;
    ACP_PROBE_CACHE.retain(|_, cached| cached.completed_at.elapsed() <= ACP_PROBE_CACHE_TTL);
    ACP_PROBE_CACHE.insert(
        key,
        CachedAcpProbe {
            completed_at: Instant::now(),
            probe: result.clone(),
        },
    );
    Ok(result)
}

async fn coordinated_probe_acp(
    runner: BaseCodingAgent,
    executor: &CodingAgent,
    current_dir: &Path,
    env: &ExecutionEnv,
    auth_method_id: Option<&str>,
    policy: AcpProbeCachePolicy,
) -> Result<Option<AcpCapabilityProbe>, ExecutorError> {
    let key = acp_probe_cache_key(runner, executor, current_dir, env, auth_method_id)?;
    run_coordinated_acp_probe(key, policy, || {
        executor.probe_acp(current_dir, env, auth_method_id)
    })
    .await
}

async fn discover_models_for_executor(
    runner: BaseCodingAgent,
    executor: &CodingAgent,
    current_dir: &Path,
    env: &ExecutionEnv,
) -> Result<Option<Vec<String>>, String> {
    if let Some(probe) = coordinated_probe_acp(
        runner,
        executor,
        current_dir,
        env,
        None,
        AcpProbeCachePolicy::Refresh,
    )
    .await
    .map_err(|err| err.to_string())?
        && let Some(models) = probe.model_ids()
    {
        return Ok(Some(models));
    }

    executor
        .list_models(current_dir, env)
        .await
        .map_err(|err| err.to_string())
}

fn build_statuses(
    profiles: &ExecutorConfigs,
    store: &AgentRuntimeStore,
) -> Vec<AgentRuntimeStatus> {
    let node_available = detect_node_available();
    let mut runners = profiles
        .executors
        .iter()
        .filter_map(|(runner, config)| {
            let base = config
                .get_default()
                .or_else(|| config.configurations.values().next())?;
            Some(build_status(*runner, config, base, store, node_available))
        })
        .collect::<Vec<_>>();
    runners.sort_by_key(|status| status.runner_type.to_string());
    runners
}

fn runtime_discovery_needs_refresh(profiles: &ExecutorConfigs, store: &AgentRuntimeStore) -> bool {
    let now = Utc::now();
    profiles.executors.iter().any(|(runner, executor_config)| {
        let Some(mut base) = executor_config
            .get_default()
            .or_else(|| executor_config.configurations.values().next())
            .cloned()
        else {
            return false;
        };
        if let Some(config) = store.configs.get(runner)
            && apply_executor_options(*runner, &mut base, &config.executor_options).is_err()
        {
            return true;
        }
        if !base.get_availability_info().is_available() {
            return false;
        }
        store
            .discoveries
            .get(runner)
            .map(|discovery| now - discovery.last_checked_at >= DISCOVERY_TTL)
            .unwrap_or(true)
    })
}

fn build_status(
    runner: BaseCodingAgent,
    executor_config: &ExecutorConfig,
    base: &CodingAgent,
    store: &AgentRuntimeStore,
    node_available: bool,
) -> AgentRuntimeStatus {
    let config = store
        .configs
        .get(&runner)
        .cloned()
        .unwrap_or_else(|| default_config(runner));
    let discovery = store.discoveries.get(&runner);
    let mut configured_base = base.clone();
    let configuration_error = if let Err(error) =
        apply_executor_options(runner, &mut configured_base, &config.executor_options)
    {
        tracing::warn!(
            runner = %runner,
            error = %error,
            "failed to apply runtime config while checking availability"
        );
        Some(status_error_detail("runtime_configuration", error))
    } else {
        None
    };
    let availability = configured_base.get_availability_info();
    let installed = availability.is_available();
    let executable = installed && config.run_mode != AgentRunMode::Disabled;
    let mut auth_env = ExecutionEnv::new(Default::default(), false, String::new());
    auth_env.merge(&config.env_json);
    let auth_state = if configured_base.is_authenticated(&auth_env) {
        AgentRuntimeAuthState::Authenticated
    } else {
        AgentRuntimeAuthState::Unauthenticated
    };

    AgentRuntimeStatus {
        runner_type: runner,
        installed,
        executable,
        availability,
        auth_state,
        node_available,
        discovered_models: models_for_runner(runner, executor_config, store),
        model_source: model_source_for_runner(runner, executor_config, store),
        version: discovery.and_then(|entry| entry.version.clone()),
        last_checked_at: discovery.map(|entry| entry.last_checked_at),
        last_error: merge_status_error_details([
            discovery.and_then(|entry| entry.last_error.clone()),
            configuration_error,
        ]),
        run_mode: config.run_mode,
        env_summary: summarize_env(&config.env_json),
        executor_options: config.executor_options,
    }
}

fn status_error_detail(stage: &str, error: impl std::fmt::Display) -> String {
    format!("[{stage}] {}", error.to_string().trim())
}

fn merge_status_error_details(errors: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let mut details = Vec::new();
    for error in errors.into_iter().flatten() {
        let error = error.trim();
        if !error.is_empty() && !details.iter().any(|existing| existing == error) {
            details.push(error.to_string());
        }
    }
    (!details.is_empty()).then(|| details.join("\n"))
}

fn remove_status_error_stage(error: Option<&str>, stage: &str) -> Option<String> {
    let prefix = format!("[{stage}]");
    merge_status_error_details(
        error
            .into_iter()
            .flat_map(str::lines)
            .map(|line| (!line.trim_start().starts_with(&prefix)).then(|| line.to_string())),
    )
}

fn clear_cached_authentication_required_error(
    store: &mut AgentRuntimeStore,
    runner: BaseCodingAgent,
) {
    let Some(discovery) = store.discoveries.get_mut(&runner) else {
        return;
    };
    discovery.last_error = merge_status_error_details(
        discovery
            .last_error
            .as_deref()
            .into_iter()
            .flat_map(str::lines)
            .map(|line| {
                (!line
                    .to_ascii_lowercase()
                    .contains("authentication required"))
                .then(|| line.to_string())
            }),
    );
}

fn split_probe_result<T>(result: Result<Option<T>, String>) -> (Option<T>, Option<String>) {
    match result {
        Ok(value) => (value, None),
        Err(error) => (None, Some(error)),
    }
}

/// Whether a `node` executable resolves on this machine. Resolution
/// refreshes the login-shell PATH, so Node installed from a regular
/// terminal is found without restarting the app.
fn detect_node_available() -> bool {
    utils::shell::resolve_executable_path_blocking("node").is_some()
}

fn reasoning_capability_for_runner(
    runner: BaseCodingAgent,
) -> Option<AgentRuntimeReasoningCapability> {
    match runner {
        BaseCodingAgent::ClaudeCode => Some(AgentRuntimeReasoningCapability::Effort {
            options: strings(["low", "medium", "high"]),
        }),
        BaseCodingAgent::Codex => Some(AgentRuntimeReasoningCapability::Effort {
            options: strings(["low", "medium", "high", "xhigh", "max"]),
        }),
        BaseCodingAgent::Droid => Some(AgentRuntimeReasoningCapability::Effort {
            options: strings(["none", "dynamic", "off", "low", "medium", "high"]),
        }),
        BaseCodingAgent::Gemini => Some(AgentRuntimeReasoningCapability::Effort {
            options: strings(["low", "medium", "high"]),
        }),
        BaseCodingAgent::Opencode | BaseCodingAgent::OpenTeamsCli => {
            Some(AgentRuntimeReasoningCapability::Effort {
                options: strings(["thinking-low", "thinking-medium", "thinking-high"]),
            })
        }
        BaseCodingAgent::QwenCode => Some(AgentRuntimeReasoningCapability::Effort {
            options: strings(["low", "medium", "high", "xhigh", "max"]),
        }),
        BaseCodingAgent::KimiCode => Some(AgentRuntimeReasoningCapability::Effort {
            options: strings(["low", "high", "max"]),
        }),
        BaseCodingAgent::QoderCli => None,
        BaseCodingAgent::Amp | BaseCodingAgent::CursorAgent | BaseCodingAgent::Copilot => None,
        #[cfg(feature = "qa-mode")]
        BaseCodingAgent::QaMock | BaseCodingAgent::AcpQa => None,
    }
}

pub fn reasoning_capability_for_runner_type(
    runner: BaseCodingAgent,
) -> Option<AgentRuntimeReasoningCapability> {
    reasoning_capability_for_runner(runner)
}

fn strings<const N: usize>(values: [&str; N]) -> Vec<String> {
    values.into_iter().map(String::from).collect()
}

fn models_for_runner(
    runner: BaseCodingAgent,
    executor_config: &ExecutorConfig,
    store: &AgentRuntimeStore,
) -> Vec<String> {
    if let Some(discovery) = store.discoveries.get(&runner)
        && !discovery.models.is_empty()
    {
        return discovery.models.clone();
    }

    configured_models(executor_config)
}

fn model_source_for_runner(
    runner: BaseCodingAgent,
    executor_config: &ExecutorConfig,
    store: &AgentRuntimeStore,
) -> AgentRuntimeModelSource {
    if let Some(discovery) = store.discoveries.get(&runner)
        && !discovery.models.is_empty()
    {
        return AgentRuntimeModelSource::Runner;
    }

    if configured_models(executor_config).is_empty() {
        AgentRuntimeModelSource::None
    } else {
        AgentRuntimeModelSource::ProfileFallback
    }
}

fn configured_models(executor_config: &ExecutorConfig) -> Vec<String> {
    let mut models = BTreeSet::new();
    for config in executor_config.configurations.values() {
        if let Some(model) = model_name(config) {
            models.insert(model.to_string());
        }
    }
    models.into_iter().collect()
}

fn model_name(config: &CodingAgent) -> Option<&str> {
    match config {
        CodingAgent::Codex(config) => config.model.as_deref(),
        CodingAgent::ClaudeCode(config) => config.model.as_deref(),
        CodingAgent::Gemini(config) => config.model.as_deref(),
        CodingAgent::Opencode(config) => config.model.as_deref(),
        CodingAgent::OpenTeamsCli(config) => config.model.as_deref(),
        CodingAgent::QwenCode(config) => config.model.as_deref(),
        CodingAgent::CursorAgent(config) => config.model.as_deref(),
        CodingAgent::Copilot(config) => config.model.as_deref(),
        CodingAgent::Droid(config) => config.model.as_deref(),
        CodingAgent::KimiCode(config) => config.model.as_deref(),
        CodingAgent::QoderCli(config) => config.model.as_deref(),
        #[cfg(feature = "qa-mode")]
        CodingAgent::QaMock(_) | CodingAgent::AcpQa(_) => None,
        _ => None,
    }
}

fn default_config(runner: BaseCodingAgent) -> AgentRuntimeConfig {
    AgentRuntimeConfig {
        runner_type: runner,
        run_mode: AgentRunMode::Auto,
        env_json: HashMap::new(),
        executor_options: serde_json::json!({}),
        updated_at: Utc::now(),
    }
}

fn summarize_env(env: &HashMap<String, String>) -> Vec<AgentRuntimeEnvSummary> {
    let mut summaries = env
        .iter()
        .map(|(key, value)| AgentRuntimeEnvSummary {
            key: key.clone(),
            value: value.clone(),
        })
        .collect::<Vec<_>>();
    summaries.sort_by(|a, b| a.key.cmp(&b.key));
    summaries
}

fn validate_env_json(env: &HashMap<String, String>) -> Result<(), AgentRuntimeError> {
    for key in env.keys() {
        validate_env_key(key)?;
    }
    Ok(())
}

fn validate_env_key(key: &str) -> Result<(), AgentRuntimeError> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return Err(AgentRuntimeError::InvalidEnvKey(key.to_string()));
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(AgentRuntimeError::InvalidEnvKey(key.to_string()));
    }
    if chars.any(|ch| !(ch == '_' || ch.is_ascii_alphanumeric())) {
        return Err(AgentRuntimeError::InvalidEnvKey(key.to_string()));
    }
    Ok(())
}

fn read_store_unlocked(path: &Path) -> Result<AgentRuntimeStore, AgentRuntimeError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(serde_json::from_str(&content)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(AgentRuntimeStore::default()),
        Err(err) => Err(err.into()),
    }
}

fn write_store_unlocked(path: &Path, store: &AgentRuntimeStore) -> Result<(), AgentRuntimeError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(store)?)?;
    Ok(())
}

fn read_store(path: &Path) -> Result<AgentRuntimeStore, AgentRuntimeError> {
    let _guard = RUNTIME_STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    read_store_unlocked(path)
}

#[cfg(test)]
fn write_store(path: &Path, store: &AgentRuntimeStore) -> Result<(), AgentRuntimeError> {
    let _guard = RUNTIME_STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    write_store_unlocked(path, store)
}

fn update_store<T>(
    path: &Path,
    update: impl FnOnce(&mut AgentRuntimeStore) -> Result<T, AgentRuntimeError>,
) -> Result<(AgentRuntimeStore, T), AgentRuntimeError> {
    let _guard = RUNTIME_STORE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read_store_unlocked(path)?;
    let result = update(&mut store)?;
    write_store_unlocked(path, &store)?;
    Ok((store, result))
}

#[cfg(test)]
mod tests {
    use executors::executors::{AppendPrompt, kimi::KimiCode, qoder::QoderCli};

    use super::*;

    fn model_agent(model: Option<&str>) -> CodingAgent {
        CodingAgent::KimiCode(KimiCode {
            append_prompt: AppendPrompt::default(),
            model: model.map(str::to_string),
            thinking_effort: None,
            acp: None,
            cmd: Default::default(),
            acp_mcp_policy: Default::default(),
            approvals: None,
        })
    }

    fn test_probe_key(runner: BaseCodingAgent, salt: u8) -> AcpProbeCacheKey {
        let mut fingerprint = [0; 32];
        fingerprint[0] = salt;
        AcpProbeCacheKey {
            runner,
            current_dir: PathBuf::from(format!("/openteams-probe-test-{salt}")),
            auth_method_id: None,
            execution_fingerprint: fingerprint,
        }
    }

    #[tokio::test]
    async fn concurrent_identical_acp_probes_share_one_execution() {
        let key = test_probe_key(BaseCodingAgent::Gemini, 201);
        ACP_PROBE_CACHE.remove(&key);
        let starts = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let first_starts = Arc::clone(&starts);
        let first = run_coordinated_acp_probe(
            key.clone(),
            AcpProbeCachePolicy::Reuse,
            move || async move {
                first_starts.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok::<Option<AcpCapabilityProbe>, ExecutorError>(None)
            },
        );
        let second_starts = Arc::clone(&starts);
        let second = run_coordinated_acp_probe(
            key.clone(),
            AcpProbeCachePolicy::Refresh,
            move || async move {
                second_starts.fetch_add(1, Ordering::SeqCst);
                Ok::<Option<AcpCapabilityProbe>, ExecutorError>(None)
            },
        );

        let (first_result, second_result) = tokio::join!(first, second);

        assert!(first_result.unwrap().is_none());
        assert!(second_result.unwrap().is_none());
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        ACP_PROBE_CACHE.remove(&key);
    }

    #[tokio::test]
    async fn different_acp_probe_keys_for_one_runner_are_serialized() {
        let first_key = test_probe_key(BaseCodingAgent::Gemini, 202);
        let second_key = test_probe_key(BaseCodingAgent::Gemini, 203);
        ACP_PROBE_CACHE.remove(&first_key);
        ACP_PROBE_CACHE.remove(&second_key);
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let maximum_active = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let make_probe =
            |active: Arc<std::sync::atomic::AtomicUsize>,
             maximum_active: Arc<std::sync::atomic::AtomicUsize>| async move {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum_active.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(40)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok::<Option<AcpCapabilityProbe>, ExecutorError>(None)
            };
        let first = run_coordinated_acp_probe(first_key.clone(), AcpProbeCachePolicy::Refresh, {
            let active = Arc::clone(&active);
            let maximum_active = Arc::clone(&maximum_active);
            move || make_probe(active, maximum_active)
        });
        let second = run_coordinated_acp_probe(second_key.clone(), AcpProbeCachePolicy::Refresh, {
            let active = Arc::clone(&active);
            let maximum_active = Arc::clone(&maximum_active);
            move || make_probe(active, maximum_active)
        });

        let (first_result, second_result) = tokio::join!(first, second);

        assert!(first_result.is_ok());
        assert!(second_result.is_ok());
        assert_eq!(maximum_active.load(Ordering::SeqCst), 1);
        ACP_PROBE_CACHE.remove(&first_key);
        ACP_PROBE_CACHE.remove(&second_key);
    }

    #[test]
    fn env_key_validation_accepts_shell_safe_names() {
        let mut env = HashMap::new();
        env.insert("OPENAI_API_KEY".to_string(), "secret".to_string());
        env.insert("_CUSTOM_1".to_string(), "secret".to_string());

        assert!(validate_env_json(&env).is_ok());
    }

    #[test]
    fn env_key_validation_rejects_invalid_names() {
        let mut env = HashMap::new();
        env.insert("BAD-NAME".to_string(), "secret".to_string());

        assert!(matches!(
            validate_env_json(&env),
            Err(AgentRuntimeError::InvalidEnvKey(key)) if key == "BAD-NAME"
        ));
    }

    #[test]
    fn env_summary_includes_values() {
        let mut env = HashMap::new();
        env.insert("OPENAI_API_KEY".to_string(), "sk-test".to_string());

        let summary = summarize_env(&env);

        assert_eq!(summary[0].key, "OPENAI_API_KEY");
        assert_eq!(summary[0].value, "sk-test");
    }

    #[test]
    fn cli_version_output_prefers_stdout_and_trims() {
        let version =
            normalize_cli_version_output(b"\n codex-cli 0.125.0 \n", b"npm notice ignored\n");

        assert_eq!(version.as_deref(), Some("codex-cli 0.125.0"));
    }

    #[test]
    fn cli_version_output_falls_back_to_stderr() {
        let version = normalize_cli_version_output(b"", b"\nclaude-code 2.1.74\n");

        assert_eq!(version.as_deref(), Some("claude-code 2.1.74"));
    }

    #[test]
    fn discovery_update_version_prefers_detected_then_cached() {
        let runner = BaseCodingAgent::Opencode;
        let mut store = AgentRuntimeStore::default();
        store.discoveries.insert(
            runner,
            AgentRuntimeDiscovery {
                models: vec!["openai/gpt-5.2-codex".to_string()],
                version: Some("opencode 1.2.23".to_string()),
                last_checked_at: Utc::now(),
                last_error: None,
            },
        );

        assert_eq!(
            version_for_discovery_update(&store, runner, Some("opencode 1.2.24".to_string()))
                .as_deref(),
            Some("opencode 1.2.24")
        );
        assert_eq!(
            version_for_discovery_update(&store, runner, None).as_deref(),
            Some("opencode 1.2.23")
        );
    }

    #[test]
    fn version_only_discovery_clears_stale_error_and_preserves_models() {
        let runner = BaseCodingAgent::Opencode;
        let mut store = AgentRuntimeStore::default();
        store.discoveries.insert(
            runner,
            AgentRuntimeDiscovery {
                models: vec!["openai/gpt-5.2-codex".to_string()],
                version: Some("opencode 1.2.23".to_string()),
                last_checked_at: Utc::now(),
                last_error: Some("temporary provider failure".to_string()),
            },
        );

        cache_version_only_discovery(
            &mut store,
            runner,
            Some("opencode 1.2.24".to_string()),
            None,
        );

        let discovery = store
            .discoveries
            .get(&runner)
            .expect("runtime discovery should remain cached");
        assert_eq!(discovery.models, vec!["openai/gpt-5.2-codex"]);
        assert_eq!(discovery.version.as_deref(), Some("opencode 1.2.24"));
        assert_eq!(discovery.last_error, None);
    }

    #[test]
    fn version_only_discovery_reports_version_probe_error() {
        let runner = BaseCodingAgent::Opencode;
        let mut store = AgentRuntimeStore::default();

        cache_version_only_discovery(
            &mut store,
            runner,
            None,
            Some("failed to resolve version executable: opencode not found".to_string()),
        );

        let discovery = store
            .discoveries
            .get(&runner)
            .expect("version failure should be cached for status reporting");
        assert_eq!(
            discovery.last_error.as_deref(),
            Some("[version_check] failed to resolve version executable: opencode not found")
        );
    }

    #[test]
    fn refresh_response_reports_version_probe_error() {
        let runner = BaseCodingAgent::Opencode;
        let mut store = AgentRuntimeStore::default();

        let errors = apply_discovery_outcomes(
            &mut store,
            vec![RunnerDiscoveryOutcome::VersionOnly {
                runner,
                detected_version: None,
                version_error: Some("version command timed out after 12 seconds".to_string()),
            }],
        );

        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].runner_type, runner);
        assert_eq!(
            errors[0].message,
            "[version_check] version command timed out after 12 seconds"
        );
        assert_eq!(
            store.discoveries[&runner].last_error,
            Some("[version_check] version command timed out after 12 seconds".to_string())
        );
    }

    #[test]
    fn successful_version_probe_clears_only_version_error() {
        let runner = BaseCodingAgent::Opencode;
        let mut store = AgentRuntimeStore::default();
        store.discoveries.insert(
            runner,
            AgentRuntimeDiscovery {
                models: Vec::new(),
                version: None,
                last_checked_at: Utc::now(),
                last_error: Some(
                    "[model_discovery] provider unavailable\n[version_check] executable not found"
                        .to_string(),
                ),
            },
        );

        cache_runner_version(&mut store, runner, "opencode 1.2.24".to_string());

        let discovery = &store.discoveries[&runner];
        assert_eq!(discovery.version.as_deref(), Some("opencode 1.2.24"));
        assert_eq!(
            discovery.last_error.as_deref(),
            Some("[model_discovery] provider unavailable")
        );
    }

    #[test]
    fn successful_acp_probe_clears_cached_authentication_required_error() {
        let runner = BaseCodingAgent::QoderCli;
        let mut store = AgentRuntimeStore::default();
        store.discoveries.insert(
            runner,
            AgentRuntimeDiscovery {
                models: Vec::new(),
                version: Some("qodercli 1.2.3".to_string()),
                last_checked_at: Utc::now(),
                last_error: Some(
                    "[model_discovery] I/O error: Authentication required: Authentication is required\n[version_check] temporary warning"
                        .to_string(),
                ),
            },
        );

        clear_cached_authentication_required_error(&mut store, runner);

        assert_eq!(
            store.discoveries[&runner].last_error.as_deref(),
            Some("[version_check] temporary warning")
        );
    }

    #[test]
    fn status_error_details_preserve_each_failed_stage() {
        let merged = merge_status_error_details([
            Some(status_error_detail(
                "model_discovery",
                "provider request failed",
            )),
            Some(status_error_detail(
                "version_check",
                "version command timed out after 12 seconds",
            )),
        ])
        .unwrap();

        assert_eq!(
            merged,
            "[model_discovery] provider request failed\n[version_check] version command timed out after 12 seconds"
        );
    }

    #[test]
    fn command_failure_identifies_command_operation_and_result() {
        let detail = command_failure_detail(
            "/Users/test/.local/bin/copilot --version",
            "execute version command",
            "process failed with exit code 1: authentication required",
        );

        assert_eq!(
            detail,
            "command=`/Users/test/.local/bin/copilot --version`; operation=execute version command; result=process failed with exit code 1: authentication required"
        );
    }

    #[tokio::test]
    async fn missing_runner_skips_runtime_command_resolution() {
        let mut executor = model_agent(None);
        let CodingAgent::KimiCode(config) = &mut executor else {
            panic!("expected KimiCode executor");
        };
        config.cmd.base_command_override =
            Some("openteams-test-command-that-must-not-exist-8dd8c9e9".to_string());

        let result = resolve_runtime_command_for_diagnostics(false, &executor)
            .await
            .expect("uninstalled runner should not resolve its command");

        assert!(result.is_none());
    }

    #[test]
    fn successful_refresh_replaces_cached_cli_model_list() {
        let runner = BaseCodingAgent::Gemini;
        let mut store = AgentRuntimeStore::default();
        store.discoveries.insert(
            runner,
            AgentRuntimeDiscovery {
                models: vec!["gemini-old".to_string()],
                version: Some("gemini 1.0.0".to_string()),
                last_checked_at: Utc::now(),
                last_error: None,
            },
        );

        let errors = apply_discovery_outcomes(
            &mut store,
            vec![RunnerDiscoveryOutcome::ModelsDiscovered {
                runner,
                models: vec!["gemini-new".to_string()],
                detected_version: Some("gemini 1.1.0".to_string()),
                version_error: None,
            }],
        );

        let discovery = store.discoveries.get(&runner).unwrap();
        assert!(errors.is_empty());
        assert_eq!(discovery.models, vec!["gemini-new"]);
        assert_eq!(discovery.version.as_deref(), Some("gemini 1.1.0"));
    }

    #[test]
    fn refresh_failure_preserves_old_discovery_models() {
        let runner = BaseCodingAgent::Opencode;
        let mut configs = HashMap::new();
        configs.insert(
            runner,
            AgentRuntimeDiscovery {
                models: vec!["openai/gpt-5.2-codex".to_string()],
                version: None,
                last_checked_at: Utc::now(),
                last_error: None,
            },
        );
        let store = AgentRuntimeStore {
            configs: HashMap::new(),
            discoveries: configs,
        };
        let executor_config = ExecutorConfig::new_with_default(model_agent(None));

        let models = models_for_runner(runner, &executor_config, &store);

        assert_eq!(models, vec!["openai/gpt-5.2-codex"]);
    }

    #[test]
    fn aggregation_returns_runner_config_and_models() {
        let runner = BaseCodingAgent::KimiCode;
        let mut executors = HashMap::new();
        executors.insert(
            runner,
            ExecutorConfig::new_with_default(model_agent(Some("kimi-k2.5"))),
        );
        let profiles = ExecutorConfigs { executors };
        let mut runtime = default_config(runner);
        runtime.run_mode = AgentRunMode::Local;
        runtime
            .env_json
            .insert("KIMI_API_KEY".to_string(), "secret".to_string());
        let mut store = AgentRuntimeStore::default();
        store.configs.insert(runner, runtime);

        let statuses = build_statuses(&profiles, &store);

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].runner_type, runner);
        assert_eq!(statuses[0].run_mode, AgentRunMode::Local);
        assert_eq!(statuses[0].discovered_models, vec!["kimi-k2.5"]);
        assert_eq!(
            statuses[0].model_source,
            AgentRuntimeModelSource::ProfileFallback
        );
        assert_eq!(statuses[0].env_summary[0].value, "secret");
        assert_eq!(statuses[0].auth_state, AgentRuntimeAuthState::Authenticated);
    }

    #[test]
    fn qoder_runtime_status_recognizes_local_auth_state_file() {
        let temp = tempfile::tempdir().expect("temporary Qoder home");
        let auth_dir = temp.path().join(".auth");
        std::fs::create_dir(&auth_dir).expect("create auth directory");
        std::fs::write(auth_dir.join("user"), "encrypted-login-state")
            .expect("write Qoder auth state");

        let runner = BaseCodingAgent::QoderCli;
        let executor = CodingAgent::QoderCli(QoderCli {
            append_prompt: AppendPrompt::default(),
            model: Some("auto".to_string()),
            acp: None,
            cmd: CmdOverrides::default(),
            acp_mcp_policy: Default::default(),
            approvals: None,
        });
        let executor_config = ExecutorConfig::new_with_default(executor);
        let mut runtime = default_config(runner);
        runtime.env_json.insert(
            "QODER_CONFIG_DIR".to_string(),
            temp.path().to_string_lossy().into_owned(),
        );
        let mut store = AgentRuntimeStore::default();
        store.configs.insert(runner, runtime);
        let base = executor_config.get_default().expect("Qoder default config");

        let status = build_status(runner, &executor_config, base, &store, true);

        assert_eq!(status.auth_state, AgentRuntimeAuthState::Authenticated);
    }

    #[test]
    fn status_reports_invalid_runtime_configuration_detail() {
        let runner = BaseCodingAgent::KimiCode;
        let executor_config = ExecutorConfig::new_with_default(model_agent(None));
        let mut runtime = default_config(runner);
        runtime.executor_options = serde_json::json!({ "model": 42 });
        let mut store = AgentRuntimeStore::default();
        store.configs.insert(runner, runtime);
        let base = executor_config.get_default().unwrap();

        let status = build_status(runner, &executor_config, base, &store, true);

        let error = status
            .last_error
            .expect("invalid runtime configuration should be reported");
        assert!(error.starts_with("[runtime_configuration]"));
        assert!(error.contains("invalid type"));
        assert!(error.contains("expected a string"));
    }

    #[test]
    fn model_source_prefers_runner_discovery_over_profile_fallback() {
        let runner = BaseCodingAgent::Opencode;
        let mut store = AgentRuntimeStore::default();
        store.discoveries.insert(
            runner,
            AgentRuntimeDiscovery {
                models: vec!["opencode/free-model".to_string()],
                version: None,
                last_checked_at: Utc::now(),
                last_error: None,
            },
        );
        let executor_config =
            ExecutorConfig::new_with_default(model_agent(Some("profile/fallback-model")));

        assert_eq!(
            models_for_runner(runner, &executor_config, &store),
            vec!["opencode/free-model"]
        );
        assert_eq!(
            model_source_for_runner(runner, &executor_config, &store),
            AgentRuntimeModelSource::Runner
        );
    }

    #[test]
    fn model_source_reports_none_when_no_models_are_available() {
        let runner = BaseCodingAgent::OpenTeamsCli;
        let store = AgentRuntimeStore::default();
        let executor_config = ExecutorConfig::new_with_default(model_agent(None));

        assert_eq!(
            model_source_for_runner(runner, &executor_config, &store),
            AgentRuntimeModelSource::None
        );
    }

    #[test]
    fn config_store_round_trips_runtime_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.json");
        let runner = BaseCodingAgent::KimiCode;
        let mut runtime = default_config(runner);
        runtime.run_mode = AgentRunMode::Disabled;
        runtime.executor_options = serde_json::json!({
            "model": "kimi-k2.6",
            "cmd": {
                "base_command_override": "kimi-dev"
            }
        });
        runtime
            .env_json
            .insert("KIMI_API_KEY".to_string(), "secret".to_string());
        let mut store = AgentRuntimeStore::default();
        store.configs.insert(runner, runtime);

        write_store(&path, &store).unwrap();
        let restored = read_store(&path).unwrap();

        let restored_config = restored.configs.get(&runner).unwrap();
        assert_eq!(restored_config.runner_type, runner);
        assert_eq!(restored_config.run_mode, AgentRunMode::Disabled);
        assert_eq!(restored_config.env_json["KIMI_API_KEY"], "secret");
        assert_eq!(restored_config.executor_options["model"], "kimi-k2.6");
    }

    #[test]
    fn concurrent_config_and_discovery_updates_preserve_both() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.json");
        let runner = BaseCodingAgent::KimiCode;
        write_store(&path, &AgentRuntimeStore::default()).unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(2));
        let config_path = path.clone();
        let config_barrier = Arc::clone(&barrier);
        let config_update = std::thread::spawn(move || {
            config_barrier.wait();
            update_store(&config_path, |store| {
                let mut config = default_config(runner);
                config.run_mode = AgentRunMode::Local;
                config
                    .env_json
                    .insert("KIMI_API_KEY".to_string(), "new-secret".to_string());
                config.executor_options = serde_json::json!({ "model": "kimi-k2.6" });
                store.configs.insert(runner, config);
                Ok(())
            })
            .unwrap();
        });

        let discovery_path = path.clone();
        let discovery_barrier = Arc::clone(&barrier);
        let discovery_update = std::thread::spawn(move || {
            discovery_barrier.wait();
            update_store(&discovery_path, |store| {
                Ok(apply_discovery_outcomes(
                    store,
                    vec![RunnerDiscoveryOutcome::ModelsDiscovered {
                        runner,
                        models: vec!["kimi-k2.6".to_string()],
                        detected_version: Some("kimi 1.0.0".to_string()),
                        version_error: None,
                    }],
                ))
            })
            .unwrap();
        });

        config_update.join().unwrap();
        discovery_update.join().unwrap();

        let restored = read_store(&path).unwrap();
        let config = restored.configs.get(&runner).unwrap();
        assert_eq!(config.run_mode, AgentRunMode::Local);
        assert_eq!(config.env_json["KIMI_API_KEY"], "new-secret");
        assert_eq!(config.executor_options["model"], "kimi-k2.6");
        let discovery = restored.discoveries.get(&runner).unwrap();
        assert_eq!(discovery.models, vec!["kimi-k2.6"]);
        assert_eq!(discovery.version.as_deref(), Some("kimi 1.0.0"));
    }

    #[test]
    fn executor_options_merge_into_default_executor() {
        let runner = BaseCodingAgent::KimiCode;
        let mut runtime = default_config(runner);
        runtime.executor_options = serde_json::json!({
            "model": "kimi-k2.6"
        });
        let mut store = AgentRuntimeStore::default();
        store.configs.insert(runner, runtime);
        let mut executor = model_agent(Some("gpt-5.2-codex"));
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());

        apply_config_to_executor_and_env(runner, &mut executor, &mut env, &store).unwrap();

        assert_eq!(model_name(&executor), Some("kimi-k2.6"));
        let CodingAgent::KimiCode(config) = executor else {
            panic!("expected KimiCode executor");
        };
        assert_eq!(config.model.as_deref(), Some("kimi-k2.6"));
    }

    #[test]
    fn session_env_wins_over_agent_env_on_conflict() {
        let runner = BaseCodingAgent::KimiCode;
        let mut runtime = default_config(runner);
        runtime
            .env_json
            .insert("VK_CHAT_SESSION_ID".to_string(), "agent".to_string());
        runtime
            .env_json
            .insert("OPENAI_API_KEY".to_string(), "agent-key".to_string());
        let mut store = AgentRuntimeStore::default();
        store.configs.insert(runner, runtime);
        let mut executor = model_agent(None);
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert("VK_CHAT_SESSION_ID", "session");

        apply_config_to_executor_and_env(runner, &mut executor, &mut env, &store).unwrap();

        assert_eq!(
            env.get("VK_CHAT_SESSION_ID").map(String::as_str),
            Some("session")
        );
        assert_eq!(
            env.get("OPENAI_API_KEY").map(String::as_str),
            Some("agent-key")
        );
    }

    #[test]
    fn serialized_runtime_status_has_no_model_override_or_reasoning_level() {
        let status = AgentRuntimeStatus {
            runner_type: BaseCodingAgent::Codex,
            installed: true,
            executable: true,
            availability: AvailabilityInfo::InstallationFound,
            auth_state: AgentRuntimeAuthState::Authenticated,
            node_available: true,
            discovered_models: vec!["gpt-5.2-codex".to_string()],
            model_source: AgentRuntimeModelSource::Runner,
            version: None,
            last_checked_at: None,
            last_error: None,
            run_mode: AgentRunMode::Auto,
            env_summary: Vec::new(),
            executor_options: serde_json::json!({ "ask_for_approval": "never" }),
        };

        let value = serde_json::to_value(status).unwrap();

        assert!(value.get("model_override").is_none());
        assert!(value.get("reasoning_level").is_none());
        assert!(value.get("model_reasoning_effort").is_none());
        assert_eq!(value["executor_options"]["ask_for_approval"], "never");
    }

    #[test]
    fn reasoning_capabilities_include_opencode_family_effort() {
        for runner in [BaseCodingAgent::Opencode, BaseCodingAgent::OpenTeamsCli] {
            let capability = reasoning_capability_for_runner(runner)
                .unwrap_or_else(|| panic!("{runner} should expose reasoning capability"));
            assert_eq!(
                capability,
                AgentRuntimeReasoningCapability::Effort {
                    options: vec![
                        "thinking-low".to_string(),
                        "thinking-medium".to_string(),
                        "thinking-high".to_string(),
                    ],
                }
            );
        }
    }

    #[test]
    fn reasoning_capabilities_match_current_acp_controls() {
        assert_eq!(
            reasoning_capability_for_runner(BaseCodingAgent::QwenCode),
            Some(AgentRuntimeReasoningCapability::Effort {
                options: strings(["low", "medium", "high", "xhigh", "max"]),
            })
        );
        assert_eq!(
            reasoning_capability_for_runner(BaseCodingAgent::Gemini),
            Some(AgentRuntimeReasoningCapability::Effort {
                options: strings(["low", "medium", "high"]),
            })
        );
        assert_eq!(
            reasoning_capability_for_runner(BaseCodingAgent::KimiCode),
            Some(AgentRuntimeReasoningCapability::Effort {
                options: strings(["low", "high", "max"]),
            })
        );
    }
}
