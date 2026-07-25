use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use super::{
    AcpAgentHarness, AcpApprovalPolicy, AcpClientServicePolicy,
    mcp::{AcpMcpPolicy, resolve_effective_mcp_config},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuilder},
    env::ExecutionEnv,
    executors::{ExecutorError, SpawnedChild, StandardCodingAgentExecutor},
    mcp_config::{McpConfig, read_canonical_mcp_config},
};

/// Hidden executor used to exercise the generic ACP path in tests and `qa-mode`.
///
/// It is intentionally not a `CodingAgent` variant and therefore cannot appear in
/// production profiles, APIs, generated types, or the configuration UI.
#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct AcpQaExecutor {
    pub command: String,
    #[serde(default)]
    pub full_access: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method_id: Option<String>,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub mcp_config_path: Option<std::path::PathBuf>,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub acp_mcp_policy: AcpMcpPolicy,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl Default for AcpQaExecutor {
    fn default() -> Self {
        Self {
            command: "acp-qa-agent".to_string(),
            full_access: false,
            auth_method_id: None,
            mcp_config_path: None,
            acp_mcp_policy: AcpMcpPolicy::default(),
            cmd: CmdOverrides::default(),
            approvals: None,
        }
    }
}

impl AcpQaExecutor {
    /// Resolve the hidden QA fixture from process-only configuration. These
    /// values are intentionally absent from profiles, APIs and generated types.
    pub fn from_qa_environment() -> Self {
        let mut executor = Self::default();
        if let Some(command) = std::env::var_os("OPENTEAMS_ACP_QA_AGENT_COMMAND") {
            executor.command = command.to_string_lossy().into_owned();
        }
        if let Some(argument) = std::env::var_os("OPENTEAMS_ACP_QA_AGENT_ARGUMENT") {
            executor.cmd.additional_params = Some(vec![argument.to_string_lossy().into_owned()]);
        }
        if let Some(path) = std::env::var_os("OPENTEAMS_ACP_QA_MCP_CONFIG_PATH") {
            executor.mcp_config_path = Some(path.into());
        }
        executor.full_access = std::env::var_os("OPENTEAMS_ACP_QA_FULL_ACCESS").is_some();
        executor
    }

    fn command(&self, follow_up: bool) -> Result<crate::command::CommandParts, ExecutorError> {
        if self.cmd.base_command_override.is_none() && self.cmd.env.is_none() {
            return Ok(crate::command::CommandParts::new(
                self.command.clone(),
                self.cmd.additional_params.clone().unwrap_or_default(),
            ));
        }
        let builder = CommandBuilder::new(&self.command);
        let builder = crate::command::apply_overrides(builder, &self.cmd)?;
        if follow_up {
            Ok(builder.build_follow_up(&[])?)
        } else {
            Ok(builder.build_initial()?)
        }
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for AcpQaExecutor {
    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.harness()
            .await?
            .spawn_with_command(
                current_dir,
                prompt.to_string(),
                self.command(false)?,
                env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    async fn spawn_follow_up(
        &self,
        current_dir: &Path,
        prompt: &str,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.harness()
            .await?
            .spawn_follow_up_with_command(
                current_dir,
                prompt.to_string(),
                session_id,
                self.command(true)?,
                env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        super::normalize_logs(msg_store, worktree_path);
    }

    fn default_mcp_config_path(&self) -> Option<std::path::PathBuf> {
        self.mcp_config_path.clone()
    }
}

impl AcpQaExecutor {
    async fn harness(&self) -> Result<AcpAgentHarness, ExecutorError> {
        let mut harness = AcpAgentHarness::new()
            .with_approval_policy(AcpApprovalPolicy::AutoAllow)
            .with_client_services(self.client_services());
        if let Some(method_id) = &self.auth_method_id {
            harness = harness.with_auth_method_id(method_id);
        }
        let canonical = match &self.mcp_config_path {
            Some(path) => read_canonical_mcp_config(path, &McpConfig::canonical_acp()).await?,
            None => serde_json::json!({ "mcpServers": {} }),
        };
        let effective = resolve_effective_mcp_config(&canonical, &self.acp_mcp_policy)?;
        Ok(harness.with_mcp_servers(effective.servers))
    }

    fn client_services(&self) -> AcpClientServicePolicy {
        AcpClientServicePolicy {
            read_text_file: true,
            write_text_file: true,
            terminal: true,
            full_access: self.full_access,
            ..AcpClientServicePolicy::default()
        }
    }
}
