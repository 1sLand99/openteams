use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use super::{AcpAgentHarness, AcpApprovalPolicy};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuilder},
    env::ExecutionEnv,
    executors::{ExecutorError, SpawnedChild, StandardCodingAgentExecutor},
};

/// Hidden executor used to exercise the generic ACP path in tests and `qa-mode`.
///
/// It is intentionally not a `CodingAgent` variant and therefore cannot appear in
/// production profiles, APIs, generated types, or the configuration UI.
#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct AcpQaExecutor {
    pub command: String,
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
            cmd: CmdOverrides::default(),
            approvals: None,
        }
    }
}

impl AcpQaExecutor {
    fn command(&self, follow_up: bool) -> Result<crate::command::CommandParts, ExecutorError> {
        if self.cmd == CmdOverrides::default() {
            return Ok(crate::command::CommandParts::new(
                self.command.clone(),
                Vec::new(),
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
        AcpAgentHarness::new()
            .with_approval_policy(AcpApprovalPolicy::AutoAllow)
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
        AcpAgentHarness::new()
            .with_approval_policy(AcpApprovalPolicy::AutoAllow)
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
        None
    }
}
