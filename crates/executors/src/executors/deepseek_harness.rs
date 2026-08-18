use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use workspace_utils::msg_store::MsgStore;

use super::acp::{
    AcpAccessMode, AcpAgentHarness, AcpApprovalMode, AcpApprovalPolicy, AcpAuthSelection,
    AcpCapabilityProbe, AcpClientServicePolicy, AcpExecutionOptions,
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{CmdOverrides, CommandBuilder, CommandParts, apply_overrides, command_is_available},
    env::ExecutionEnv,
    executors::{
        AcpModelFallback, AppendPrompt, AvailabilityInfo, ExecutorError, ExecutorPrompt,
        SpawnedChild, StandardCodingAgentExecutor,
    },
};

pub const DEEPSEEK_HARNESS_REVISION: &str = "99f6f02fecdb7dff40c3fbc9470f5907c29f74ca";

const DEEPSEEK_AUTH_ENV_VARS: &[&str] = &["DEEPSEEK_API_KEY"];
const DEFAULT_CHECKOUT_DIR: &str = "deepseek-harness";
const ACP_BIN_RELATIVE_PATH: &str = "packages/examples/acp-demo/lib/bin.js";
const ACP_CONFIG_RELATIVE_PATH: &str = "examples/acp-agent/cordis.yml";
const PERMISSION_MODE_ENV: &str = "DSH_PERMISSION_MODE";
const PERSISTENCE_ROOT_ENV: &str = "DSH_SNAPSHOT_SESSIONS_ROOT";
const OPENTEAMS_PERSISTENCE_RELATIVE_PATH: &str = ".openteams/deepseek-harness/sessions";

#[derive(Derivative, Clone, Default, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct DeepseekHarness {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "DeepSeek Harness Checkout",
        description = "Absolute path to a built deepseek-ai/deepseek-harness checkout. Defaults to ~/deepseek-harness"
    )]
    pub harness_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        title = "ACP Composition",
        description = "Absolute path to the Cordis ACP composition. Defaults to examples/acp-agent/cordis.yml inside the checkout"
    )]
    pub acp_config_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpExecutionOptions>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl DeepseekHarness {
    fn checkout_path(&self) -> Option<PathBuf> {
        self.harness_path
            .clone()
            .or_else(|| dirs::home_dir().map(|home| home.join(DEFAULT_CHECKOUT_DIR)))
    }

    fn config_path(&self) -> Option<PathBuf> {
        self.acp_config_path.clone().or_else(|| {
            self.checkout_path()
                .map(|root| root.join(ACP_CONFIG_RELATIVE_PATH))
        })
    }

    fn validate_path(path: &Path, label: &str) -> Result<(), ExecutorError> {
        if !path.is_absolute() {
            return Err(ExecutorError::Configuration(format!(
                "DeepSeek Harness {label} must be an absolute path: {}",
                path.display()
            )));
        }
        if !path.is_file() {
            return Err(ExecutorError::Configuration(format!(
                "DeepSeek Harness {label} was not found: {}",
                path.display()
            )));
        }
        Ok(())
    }

    fn validate_structured_options(&self) -> Result<(), ExecutorError> {
        if let Some(controlled_key) = self.cmd.env.as_ref().and_then(|env| {
            [PERMISSION_MODE_ENV, PERSISTENCE_ROOT_ENV]
                .into_iter()
                .find(|key| env.contains_key(*key))
        }) {
            return Err(ExecutorError::Configuration(match controlled_key {
                PERMISSION_MODE_ENV => {
                    format!("{PERMISSION_MODE_ENV} is controlled by the structured ACP access mode")
                }
                _ => format!(
                    "{PERSISTENCE_ROOT_ENV} is controlled by the OpenTeams runtime data boundary"
                ),
            }));
        }

        let Some(options) = self.acp.as_ref() else {
            return Ok(());
        };
        if options
            .additional_directories
            .as_ref()
            .is_some_and(|directories| !directories.is_empty())
        {
            return Err(ExecutorError::Configuration(
                "DeepSeek Harness ACP supports one workspace and no additional directories"
                    .to_string(),
            ));
        }
        if options
            .config_overrides
            .as_ref()
            .is_some_and(|overrides| !overrides.is_empty())
        {
            return Err(ExecutorError::Configuration(
                "DeepSeek Harness ACP does not advertise session config options; configure provider and model in cordis.yml"
                    .to_string(),
            ));
        }
        if let Some(AcpAuthSelection::MethodId { method_id }) = options.auth.as_ref() {
            return Err(ExecutorError::AuthRequired(format!(
                "DeepSeek Harness ACP advertises no auth methods; remove `{method_id}` and provide DEEPSEEK_API_KEY"
            )));
        }
        Ok(())
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, ExecutorError> {
        self.validate_structured_options()?;
        if self.cmd.base_command_override.is_some() {
            return Ok(apply_overrides(CommandBuilder::new("node"), &self.cmd)?);
        }

        let checkout = self.checkout_path().ok_or_else(|| {
            ExecutorError::Configuration(
                "DeepSeek Harness checkout path is unavailable; configure harness_path".to_string(),
            )
        })?;
        if !checkout.is_absolute() {
            return Err(ExecutorError::Configuration(format!(
                "DeepSeek Harness checkout must be an absolute path: {}",
                checkout.display()
            )));
        }
        let bin = checkout.join(ACP_BIN_RELATIVE_PATH);
        let config = self.config_path().ok_or_else(|| {
            ExecutorError::Configuration(
                "DeepSeek Harness ACP composition path is unavailable; configure acp_config_path"
                    .to_string(),
            )
        })?;
        Self::validate_path(&bin, "ACP executable")?;
        Self::validate_path(&config, "ACP composition")?;

        Ok(apply_overrides(
            CommandBuilder::new("node").extend_params([
                bin.to_string_lossy().to_string(),
                "--config".to_string(),
                config.to_string_lossy().to_string(),
            ]),
            &self.cmd,
        )?)
    }

    fn runtime_env(&self, env: &ExecutionEnv, current_dir: &Path) -> ExecutionEnv {
        let mut runtime_env = env.clone();
        let access_mode = self
            .acp
            .as_ref()
            .and_then(|options| options.access_mode)
            .unwrap_or_default();
        runtime_env.insert(
            PERMISSION_MODE_ENV,
            match access_mode {
                AcpAccessMode::WorkspaceOnly => "workspace-write",
                AcpAccessMode::FullAccess => "danger-full-access",
            },
        );
        runtime_env.insert(
            PERSISTENCE_ROOT_ENV,
            current_dir
                .join(OPENTEAMS_PERSISTENCE_RELATIVE_PATH)
                .to_string_lossy()
                .to_string(),
        );
        runtime_env
    }

    fn acp_harness(&self) -> Result<AcpAgentHarness, ExecutorError> {
        self.validate_structured_options()?;
        let options = self.acp.clone().unwrap_or_default();
        let approval_policy = match options.approval_mode.unwrap_or_default() {
            AcpApprovalMode::Ask => AcpApprovalPolicy::Ask,
            AcpApprovalMode::AutoAllow => AcpApprovalPolicy::AutoAllow,
            AcpApprovalMode::AutoReject => AcpApprovalPolicy::AutoReject,
        };
        let full_access = options.access_mode.unwrap_or_default() == AcpAccessMode::FullAccess;
        Ok(AcpAgentHarness::new()
            .with_approval_policy(approval_policy)
            .with_client_services(AcpClientServicePolicy {
                full_access,
                ..AcpClientServicePolicy::default()
            })
            .with_mcp_servers(Vec::new()))
    }

    fn checkout_is_ready(&self) -> bool {
        let Some(checkout) = self.checkout_path() else {
            return false;
        };
        let Some(config) = self.config_path() else {
            return false;
        };
        checkout.is_absolute()
            && checkout.join(ACP_BIN_RELATIVE_PATH).is_file()
            && config.is_absolute()
            && config.is_file()
    }

    fn follow_up_not_supported() -> ExecutorError {
        ExecutorError::FollowUpNotSupported(
            "DeepSeek Harness ACP developer preview supports fresh sessions only; start a new OpenTeams chat for the next prompt"
                .to_string(),
        )
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for DeepseekHarness {
    fn overlay_acp_execution_options(&mut self, higher_priority: &AcpExecutionOptions) {
        let inherited = self.acp.clone().unwrap_or_default();
        self.acp = Some(inherited.overlay(higher_priority));
    }

    fn acp_full_access_enabled(&self) -> bool {
        self.acp
            .as_ref()
            .and_then(|options| options.access_mode)
            .unwrap_or_default()
            == AcpAccessMode::FullAccess
    }

    fn acp_model_fallback(&self) -> AcpModelFallback {
        AcpModelFallback::Disabled
    }

    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        self.authentication_detected(&env, DEEPSEEK_AUTH_ENV_VARS, false)
    }

    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    fn runtime_command_for_diagnostics(&self) -> Result<Option<CommandParts>, ExecutorError> {
        Ok(Some(self.build_command_builder()?.build_initial()?))
    }

    async fn probe_acp(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
        auth_method_id: Option<&str>,
    ) -> Result<Option<AcpCapabilityProbe>, ExecutorError> {
        if let Some(method_id) = auth_method_id {
            return Err(ExecutorError::AuthRequired(format!(
                "DeepSeek Harness ACP advertises no auth methods; remove `{method_id}` and provide DEEPSEEK_API_KEY"
            )));
        }
        let runtime_env = self.runtime_env(env, current_dir);
        Ok(Some(
            super::acp::runtime::probe_acp_command_without_session(
                self.build_command_builder()?.build_initial()?,
                current_dir,
                &runtime_env,
                &self.cmd,
                None,
            )
            .await?,
        ))
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let runtime_env = self.runtime_env(env, current_dir);
        self.acp_harness()?
            .spawn_with_command(
                current_dir,
                self.append_prompt.combine_prompt(prompt),
                self.build_command_builder()?.build_initial()?,
                &runtime_env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    async fn spawn_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let runtime_env = self.runtime_env(env, current_dir);
        let mut prompt = prompt.clone();
        prompt.text = self.append_prompt.combine_prompt(&prompt.text);
        self.acp_harness()?
            .spawn_structured_with_command(
                current_dir,
                prompt,
                self.build_command_builder()?.build_initial()?,
                &runtime_env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    async fn spawn_follow_up(
        &self,
        _current_dir: &Path,
        _prompt: &str,
        _session_id: &str,
        _reset_to_message_id: Option<&str>,
        _env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        Err(Self::follow_up_not_supported())
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        super::acp::normalize_logs(msg_store, worktree_path);
    }

    fn default_runtime_config_path(&self) -> Option<PathBuf> {
        self.cmd
            .base_command_override
            .is_none()
            .then(|| self.config_path())
            .flatten()
    }

    fn default_mcp_config_path(&self) -> Option<PathBuf> {
        None
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        let command_available = command_is_available("node", &self.cmd);
        if command_available
            && (self.cmd.base_command_override.is_some() || self.checkout_is_ready())
        {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn checkout_fixture() -> (tempfile::TempDir, DeepseekHarness) {
        let temp = tempfile::tempdir().expect("temporary checkout");
        let bin = temp.path().join(ACP_BIN_RELATIVE_PATH);
        let config = temp.path().join(ACP_CONFIG_RELATIVE_PATH);
        fs::create_dir_all(bin.parent().expect("bin parent")).expect("create bin parent");
        fs::create_dir_all(config.parent().expect("config parent")).expect("create config parent");
        fs::write(&bin, "export {}\n").expect("write bin");
        fs::write(&config, "[]\n").expect("write config");
        let harness = DeepseekHarness {
            harness_path: Some(temp.path().to_path_buf()),
            ..DeepseekHarness::default()
        };
        (temp, harness)
    }

    #[test]
    fn source_checkout_command_uses_official_acp_composition() {
        let (_temp, harness) = checkout_fixture();
        let (program, args) = harness
            .build_command_builder()
            .expect("command builder")
            .build_initial()
            .expect("initial command")
            .into_parts_for_test();

        assert_eq!(program, "node");
        assert!(args[0].ends_with(ACP_BIN_RELATIVE_PATH));
        assert_eq!(args[1], "--config");
        assert!(args[2].ends_with(ACP_CONFIG_RELATIVE_PATH));
    }

    #[test]
    fn command_override_is_a_complete_acp_command() {
        let harness = DeepseekHarness {
            cmd: CmdOverrides {
                base_command_override: Some("custom-dsh-acp --stdio".to_string()),
                additional_params: Some(vec!["--verbose=false".to_string()]),
                env: None,
            },
            ..DeepseekHarness::default()
        };
        let (program, args) = harness
            .build_command_builder()
            .expect("command builder")
            .build_initial()
            .expect("initial command")
            .into_parts_for_test();

        assert_eq!(program, "custom-dsh-acp");
        assert_eq!(args, ["--stdio", "--verbose=false"]);
    }

    #[test]
    fn structured_access_mode_controls_dsh_sandbox() {
        let harness = DeepseekHarness {
            acp: Some(AcpExecutionOptions {
                access_mode: Some(AcpAccessMode::WorkspaceOnly),
                ..AcpExecutionOptions::default()
            }),
            ..DeepseekHarness::default()
        };
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let env = harness.runtime_env(
            &ExecutionEnv::new(Default::default(), false, String::new()),
            workspace.path(),
        );
        assert_eq!(
            env.get(PERMISSION_MODE_ENV),
            Some(&"workspace-write".to_string())
        );
        assert_eq!(
            env.get(PERSISTENCE_ROOT_ENV),
            Some(
                &workspace
                    .path()
                    .join(OPENTEAMS_PERSISTENCE_RELATIVE_PATH)
                    .to_string_lossy()
                    .to_string()
            )
        );
    }

    #[test]
    fn unsupported_acp_features_fail_before_process_start() {
        let harness = DeepseekHarness {
            acp: Some(AcpExecutionOptions {
                additional_directories: Some(vec!["/tmp/extra".to_string()]),
                ..AcpExecutionOptions::default()
            }),
            ..DeepseekHarness::default()
        };
        let error = harness
            .validate_structured_options()
            .expect_err("additional directory must fail");
        assert!(error.to_string().contains("no additional directories"));

        let harness = DeepseekHarness {
            cmd: CmdOverrides {
                env: Some(std::collections::HashMap::from([(
                    PERMISSION_MODE_ENV.to_string(),
                    "danger-full-access".to_string(),
                )])),
                ..CmdOverrides::default()
            },
            ..DeepseekHarness::default()
        };
        let error = harness
            .validate_structured_options()
            .expect_err("permission env override must fail");
        assert!(error.to_string().contains("structured ACP access mode"));

        let harness = DeepseekHarness {
            cmd: CmdOverrides {
                env: Some(std::collections::HashMap::from([(
                    PERSISTENCE_ROOT_ENV.to_string(),
                    "/tmp/outside-openteams".to_string(),
                )])),
                ..CmdOverrides::default()
            },
            ..DeepseekHarness::default()
        };
        let error = harness
            .validate_structured_options()
            .expect_err("persistence root override must fail");
        assert!(error.to_string().contains("runtime data boundary"));
    }

    #[test]
    fn follow_up_limitation_is_explicit() {
        assert!(matches!(
            DeepseekHarness::follow_up_not_supported(),
            ExecutorError::FollowUpNotSupported(_)
        ));
    }
}
