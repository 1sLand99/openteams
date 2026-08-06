use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use command_group::AsyncGroupChild;
use enum_dispatch::enum_dispatch;
use futures::stream::BoxStream;
use futures_io::Error as FuturesIoError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::Type;
use strum_macros::{Display, EnumDiscriminants, EnumString, VariantNames};
use thiserror::Error;
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

#[cfg(feature = "qa-mode")]
use crate::executors::acp::AcpQaExecutor;
#[cfg(feature = "qa-mode")]
use crate::executors::qa_mock::QaMockExecutor;
use crate::{
    actions::{ExecutorAction, review::RepoReviewContext},
    approvals::ExecutorApprovalService,
    command::CommandBuildError,
    env::ExecutionEnv,
    executors::{
        amp::Amp, claude::ClaudeCode, codex::Codex, copilot::Copilot, cursor::CursorAgent,
        droid::Droid, gemini::Gemini, kimi::KimiCode, opencode::Opencode,
        openteams_cli::OpenTeamsCli, pi::Pi, qoder::QoderCli, qwen::QwenCode,
    },
    logs::utils::patch,
    mcp_config::McpConfig,
    skill_config::{
        NativeDiscoveredSkill, NativeSkillConfigBackend, list_native_skills,
        set_native_skill_enabled,
    },
};

pub mod acp;
pub mod amp;
pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod droid;
pub mod gemini;
pub mod kimi;
pub mod opencode;
pub mod openteams_cli;
pub mod pi;
#[cfg(feature = "qa-mode")]
pub mod qa_mock;
pub mod qoder;
pub mod qwen;
pub mod utils;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct SlashCommandDescription {
    /// Command name without the leading slash, e.g. `help` for `/help`.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Provider-neutral structured user prompt. Executors that support rich
/// content can forward image blocks; text-only executors retain the exact
/// markdown prompt through the default trait methods.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorPrompt {
    pub text: String,
    pub images: Vec<ExecutorPromptImage>,
}

impl ExecutorPrompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorPromptImage {
    pub data: String,
    pub mime_type: String,
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(use_ts_enum)]
pub enum BaseAgentCapability {
    SessionFork,
    /// Agent requires a setup script before it can run (e.g., login, installation)
    SetupHelper,
    /// Agent reports context/token usage information
    ContextUsage,
}

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("Follow-up is not supported: {0}")]
    FollowUpNotSupported(String),
    #[error(transparent)]
    SpawnError(#[from] FuturesIoError),
    #[error("Unknown executor type: {0}")]
    UnknownExecutorType(String),
    #[error("I/O error: {0}")]
    Io(std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    TomlSerialize(#[from] toml::ser::Error),
    #[error(transparent)]
    TomlDeserialize(#[from] toml::de::Error),
    #[error(transparent)]
    ExecutorApprovalError(#[from] crate::approvals::ExecutorApprovalError),
    #[error(transparent)]
    CommandBuild(#[from] CommandBuildError),
    #[error("Executable `{program}` not found in PATH")]
    ExecutableNotFound { program: String },
    #[error("Setup helper not supported")]
    SetupHelperNotSupported,
    #[error("Auth required: {0}")]
    AuthRequired(String),
    #[error("Configuration error: {0}")]
    Configuration(String),
}

#[enum_dispatch]
#[derive(
    Debug, Clone, Serialize, Deserialize, PartialEq, TS, Display, EnumDiscriminants, VariantNames,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
#[strum_discriminants(
    name(BaseCodingAgent),
    // Only add Hash; Eq/PartialEq are already provided by EnumDiscriminants.
    derive(EnumString, Hash, strum_macros::Display, Serialize, Deserialize, TS, Type),
    strum(serialize_all = "SCREAMING_SNAKE_CASE"),
    ts(use_ts_enum),
    serde(rename_all = "SCREAMING_SNAKE_CASE"),
    sqlx(type_name = "TEXT", rename_all = "SCREAMING_SNAKE_CASE")
)]
pub enum CodingAgent {
    ClaudeCode,
    Amp,
    Gemini,
    Codex,
    Opencode,
    OpenTeamsCli,
    #[serde(alias = "CURSOR")]
    #[strum_discriminants(serde(alias = "CURSOR"))]
    #[strum_discriminants(strum(serialize = "CURSOR", serialize = "CURSOR_AGENT"))]
    CursorAgent,
    QwenCode,
    Copilot,
    Droid,
    KimiCode,
    QoderCli,
    Pi,
    #[cfg(feature = "qa-mode")]
    QaMock(QaMockExecutor),
    #[cfg(feature = "qa-mode")]
    AcpQa(AcpQaExecutor),
}

impl CodingAgent {
    pub fn set_acp_mcp_policy(&mut self, policy: acp::mcp::AcpMcpPolicy) {
        match self {
            Self::Gemini(config) => config.acp_mcp_policy = policy,
            Self::QwenCode(config) => config.acp_mcp_policy = policy,
            Self::KimiCode(config) => config.acp_mcp_policy = policy,
            Self::QoderCli(config) => config.acp_mcp_policy = policy,
            Self::Pi(config) => config.acp_mcp_policy = policy,
            #[cfg(feature = "qa-mode")]
            Self::AcpQa(config) => config.acp_mcp_policy = policy,
            _ => {}
        }
    }

    pub fn get_mcp_config(&self) -> McpConfig {
        match self {
            Self::Codex(_) => McpConfig::new(
                vec!["mcp_servers".to_string()],
                serde_json::json!({
                    "mcp_servers": {}
                }),
                self.preconfigured_mcp(),
                true,
            ),
            Self::Amp(_) => McpConfig::new(
                vec!["amp.mcpServers".to_string()],
                serde_json::json!({
                    "amp.mcpServers": {}
                }),
                self.preconfigured_mcp(),
                false,
            ),
            Self::Opencode(_) => McpConfig::new(
                vec!["mcp".to_string()],
                serde_json::json!({
                    "mcp": {},
                    "$schema": "https://opencode.ai/config.json"
                }),
                self.preconfigured_mcp(),
                false,
            ),
            Self::OpenTeamsCli(_) => McpConfig::new(
                vec!["mcp".to_string()],
                serde_json::json!({
                    "mcp": {}
                }),
                self.preconfigured_mcp(),
                false,
            ),
            Self::Droid(_) => McpConfig::new(
                vec!["mcpServers".to_string()],
                serde_json::json!({
                    "mcpServers": {}
                }),
                self.preconfigured_mcp(),
                false,
            ),
            _ => McpConfig::new(
                vec!["mcpServers".to_string()],
                serde_json::json!({
                    "mcpServers": {}
                }),
                self.preconfigured_mcp(),
                false,
            ),
        }
    }

    pub fn supports_mcp(&self) -> bool {
        self.default_mcp_config_path().is_some()
    }

    pub fn capabilities(&self) -> Vec<BaseAgentCapability> {
        match self {
            Self::ClaudeCode(_) => vec![
                BaseAgentCapability::SessionFork,
                BaseAgentCapability::ContextUsage,
            ],
            Self::Opencode(_) | Self::OpenTeamsCli(_) => vec![
                BaseAgentCapability::SessionFork,
                BaseAgentCapability::ContextUsage,
            ],
            Self::Codex(_) => vec![
                BaseAgentCapability::SessionFork,
                BaseAgentCapability::SetupHelper,
                BaseAgentCapability::ContextUsage,
            ],
            Self::Amp(_) | Self::Gemini(_) | Self::QwenCode(_) | Self::Droid(_) => {
                vec![BaseAgentCapability::SessionFork]
            }
            Self::CursorAgent(_) => vec![BaseAgentCapability::SetupHelper],
            Self::Copilot(_) => vec![],
            Self::KimiCode(_) | Self::QoderCli(_) | Self::Pi(_) => vec![
                BaseAgentCapability::SessionFork,
                BaseAgentCapability::SetupHelper,
            ],
            #[cfg(feature = "qa-mode")]
            Self::QaMock(_) | Self::AcpQa(_) => vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(export)]
pub enum AvailabilityInfo {
    LoginDetected { last_auth_timestamp: i64 },
    InstallationFound,
    NotFound,
}

impl AvailabilityInfo {
    pub fn is_available(&self) -> bool {
        matches!(
            self,
            AvailabilityInfo::LoginDetected { .. } | AvailabilityInfo::InstallationFound
        )
    }
}

fn authentication_detected(
    env: &ExecutionEnv,
    auth_env_vars: &[&str],
    cli_auth_detected: bool,
) -> bool {
    cli_auth_detected
        || auth_env_vars.iter().any(|key| {
            env.get(key).is_some_and(|value| !value.trim().is_empty())
                || std::env::var_os(key).is_some_and(|value| !value.is_empty())
        })
}

#[async_trait]
#[enum_dispatch(CodingAgent)]
pub trait StandardCodingAgentExecutor {
    fn use_approvals(&mut self, _approvals: Arc<dyn ExecutorApprovalService>) {}

    async fn available_slash_commands(
        &self,
        _workdir: &Path,
    ) -> Result<BoxStream<'static, json_patch::Patch>, ExecutorError> {
        Ok(Box::pin(futures::stream::once(async move {
            patch::slash_commands(Vec::new(), false, None)
        })))
    }

    async fn list_models(
        &self,
        _current_dir: &Path,
        _env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        Ok(None)
    }

    async fn probe_acp(
        &self,
        _current_dir: &Path,
        _env: &ExecutionEnv,
        _auth_method_id: Option<&str>,
    ) -> Result<Option<acp::AcpCapabilityProbe>, ExecutorError> {
        Ok(None)
    }

    /// Report whether this CLI can currently authenticate model requests.
    /// Production executors override this with their CLI-specific OAuth and
    /// provider configuration rules.
    fn is_authenticated(&self, _env: &ExecutionEnv) -> bool {
        matches!(
            self.get_availability_info(),
            AvailabilityInfo::LoginDetected { .. }
        )
    }

    /// Shared authentication primitive used by executor-specific detectors.
    /// A CLI login artifact, an executor runtime/profile variable, or a
    /// process environment variable with a non-blank value is sufficient.
    fn authentication_detected(
        &self,
        env: &ExecutionEnv,
        auth_env_vars: &[&str],
        cli_auth_detected: bool,
    ) -> bool {
        authentication_detected(env, auth_env_vars, cli_auth_detected)
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError>;

    async fn spawn_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.spawn(current_dir, &prompt.text, env).await
    }

    /// Continue a session, optionally resetting to a specific message.
    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError>;

    async fn spawn_follow_up_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        session_id: &str,
        reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.spawn_follow_up(
            current_dir,
            &prompt.text,
            session_id,
            reset_to_message_id,
            env,
        )
        .await
    }

    async fn spawn_review(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        match session_id {
            Some(id) => {
                self.spawn_follow_up(current_dir, prompt, id, None, env)
                    .await
            }
            None => self.spawn(current_dir, prompt, env).await,
        }
    }

    fn normalize_logs(&self, _raw_logs_event_store: Arc<MsgStore>, _worktree_path: &Path);

    /// Primary runner settings file shown by runtime diagnostics.
    ///
    /// Most runners keep their MCP servers in the primary settings file, so
    /// the compatibility default matches the MCP path. Runners with a separate
    /// MCP file override this independently.
    fn default_runtime_config_path(&self) -> Option<std::path::PathBuf> {
        self.default_mcp_config_path()
    }

    // MCP configuration methods
    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf>;

    fn default_skill_config_path(&self) -> Option<std::path::PathBuf> {
        None
    }

    fn native_skill_discovery_roots(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    fn native_skill_config_backend(&self) -> NativeSkillConfigBackend {
        NativeSkillConfigBackend::Unsupported
    }

    async fn list_native_skills(&self) -> Result<Vec<NativeDiscoveredSkill>, ExecutorError> {
        list_native_skills(
            self.native_skill_config_backend(),
            self.default_skill_config_path(),
            self.native_skill_discovery_roots(),
        )
        .await
    }

    async fn set_native_skill_enabled(
        &self,
        skill_name: &str,
        skill_path: &Path,
        enabled: bool,
    ) -> Result<(), ExecutorError> {
        set_native_skill_enabled(
            self.native_skill_config_backend(),
            self.default_skill_config_path(),
            skill_name,
            skill_path,
            enabled,
        )
        .await
    }

    async fn get_setup_helper_action(&self) -> Result<ExecutorAction, ExecutorError> {
        Err(ExecutorError::SetupHelperNotSupported)
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        AvailabilityInfo::NotFound
    }
}

/// Result communicated through the exit signal
#[derive(Debug, Clone)]
pub enum ExecutorExitResult {
    /// Process completed successfully (exit code 0)
    Success,
    /// Process should be marked as failed (non-zero exit)
    Failure,
    /// Process failed with a specific error message (e.g., from turn/completed)
    FailureWithError(String),
}

/// Optional exit notification from an executor.
/// When this receiver resolves, the container should gracefully stop the process
/// and mark it according to the result.
pub type ExecutorExitSignal = tokio::sync::oneshot::Receiver<ExecutorExitResult>;

/// Cancellation token for requesting graceful shutdown of an executor.
/// When cancelled, the executor should attempt to cancel gracefully before being killed.
pub type CancellationToken = tokio_util::sync::CancellationToken;

/// Files whose lifetime must match the executor process rather than command startup.
#[derive(Debug)]
pub struct ExecutorRunCleanup {
    paths: Vec<PathBuf>,
}

impl ExecutorRunCleanup {
    pub(crate) fn new(paths: Vec<PathBuf>) -> Self {
        Self { paths }
    }
}

impl Drop for ExecutorRunCleanup {
    fn drop(&mut self) {
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Debug)]
pub struct SpawnedChild {
    pub child: AsyncGroupChild,
    /// Executor → Container: signals when executor wants to exit
    pub exit_signal: Option<ExecutorExitSignal>,
    /// Container → Executor: signals when container wants to cancel the execution
    pub cancel: Option<CancellationToken>,
    /// Runtime resources retained until the container finishes process-tree cleanup.
    pub cleanup: Option<ExecutorRunCleanup>,
}

impl From<AsyncGroupChild> for SpawnedChild {
    fn from(child: AsyncGroupChild) -> Self {
        Self {
            child,
            exit_signal: None,
            cancel: None,
            cleanup: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
#[serde(transparent)]
#[schemars(
    title = "Append Prompt",
    description = "Extra text appended to the prompt",
    extend("format" = "textarea")
)]
#[derive(Default)]
pub struct AppendPrompt(pub Option<String>);

impl AppendPrompt {
    pub fn get(&self) -> Option<String> {
        self.0.clone()
    }

    pub fn combine_prompt(&self, prompt: &str) -> String {
        match self {
            AppendPrompt(Some(value)) => format!("{prompt}{value}"),
            AppendPrompt(None) => prompt.to_string(),
        }
    }
}

pub fn build_review_prompt(
    context: Option<&[RepoReviewContext]>,
    additional_prompt: Option<&str>,
) -> String {
    let mut prompt = String::from("Please review the code changes.\n\n");

    if let Some(repos) = context {
        for repo in repos {
            prompt.push_str(&format!("Repository: {}\n", repo.repo_name));
            prompt.push_str(&format!(
                "Review all changes from base commit {} to HEAD.\n",
                repo.base_commit
            ));
            prompt.push_str(&format!(
                "Use `git diff {}..HEAD` to see the changes.\n",
                repo.base_commit
            ));
            prompt.push('\n');
        }
    }

    if let Some(additional) = additional_prompt {
        prompt.push_str(additional);
    }

    prompt
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn login_state_remains_available_for_legacy_executors() {
        assert!(
            AvailabilityInfo::LoginDetected {
                last_auth_timestamp: 1,
            }
            .is_available()
        );
        assert!(AvailabilityInfo::InstallationFound.is_available());
        assert!(!AvailabilityInfo::NotFound.is_available());
    }

    #[test]
    fn test_cursor_agent_deserialization() {
        // Test that CURSOR_AGENT is accepted
        let result = BaseCodingAgent::from_str("CURSOR_AGENT");
        assert!(result.is_ok(), "CURSOR_AGENT should be valid");
        assert_eq!(result.unwrap(), BaseCodingAgent::CursorAgent);

        // Test that legacy CURSOR is still accepted for backwards compatibility
        let result = BaseCodingAgent::from_str("CURSOR");
        assert!(
            result.is_ok(),
            "CURSOR should be valid for backwards compatibility"
        );
        assert_eq!(result.unwrap(), BaseCodingAgent::CursorAgent);

        // Test serde deserialization for CURSOR_AGENT
        let result: Result<BaseCodingAgent, _> = serde_json::from_str(r#""CURSOR_AGENT""#);
        assert!(result.is_ok(), "CURSOR_AGENT should deserialize via serde");
        assert_eq!(result.unwrap(), BaseCodingAgent::CursorAgent);

        // Test serde deserialization for legacy CURSOR
        let result: Result<BaseCodingAgent, _> = serde_json::from_str(r#""CURSOR""#);
        assert!(result.is_ok(), "CURSOR should deserialize via serde");
        assert_eq!(result.unwrap(), BaseCodingAgent::CursorAgent);
    }

    #[test]
    fn generic_acp_runner_is_not_a_production_variant() {
        #[cfg(not(feature = "qa-mode"))]
        assert!(BaseCodingAgent::from_str("ACP_QA").is_err());

        #[cfg(feature = "qa-mode")]
        assert_eq!(
            BaseCodingAgent::from_str("ACP_QA").unwrap(),
            BaseCodingAgent::AcpQa
        );
    }

    #[test]
    fn pi_is_a_strongly_typed_production_variant() {
        assert_eq!(
            BaseCodingAgent::from_str("PI").unwrap(),
            BaseCodingAgent::Pi
        );
        assert_eq!(
            serde_json::to_string(&BaseCodingAgent::Pi).unwrap(),
            r#""PI""#
        );
    }

    #[test]
    fn authentication_accepts_cli_login_or_nonblank_runtime_key() {
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert("OPENTEAMS_TEST_AUTH_KEY", "secret");

        assert!(authentication_detected(
            &env,
            &["OPENTEAMS_TEST_AUTH_KEY"],
            false
        ));
        assert!(authentication_detected(&env, &[], true));
    }

    #[test]
    fn authentication_rejects_blank_and_foreign_runtime_keys() {
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        env.insert("OPENTEAMS_TEST_AUTH_KEY", "   ");
        env.insert("OPENTEAMS_FOREIGN_AUTH_KEY", "secret");

        assert!(!authentication_detected(
            &env,
            &["OPENTEAMS_TEST_AUTH_KEY"],
            false
        ));
    }
}
