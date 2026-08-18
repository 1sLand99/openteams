use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use derivative::Derivative;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};
use ts_rs::TS;
use uuid::Uuid;
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
        AcpModelFallback, AcpProbeInterpretation, AppendPrompt, AvailabilityInfo, ExecutorError,
        ExecutorPrompt, SpawnedChild, StandardCodingAgentExecutor,
    },
};

pub const DEEPSEEK_HARNESS_REVISION: &str = "99f6f02fecdb7dff40c3fbc9470f5907c29f74ca";

const DEEPSEEK_AUTH_ENV_VARS: &[&str] = &["DEEPSEEK_API_KEY"];
const DEFAULT_CHECKOUT_DIR: &str = "deepseek-harness";
const ACP_BIN_RELATIVE_PATH: &str = "packages/examples/acp-demo/lib/bin.js";
const ACP_CONFIG_RELATIVE_PATH: &str = "examples/acp-agent/cordis.yml";
const ACP_AGENT_ENTRY_ID: &str = "acp-agent";
const DEEPSEEK_LLM_ENTRY_ID: &str = "llm-deepseek";
const CORDIS_INCLUDE_PLUGIN: &str = "@deepseek-ai/cordis-plugin-include";
const MODEL_CONFIG_CACHE_RELATIVE_PATH: &str =
    "examples/node_modules/.cache/openteams/deepseek-harness";
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
        title = "Model",
        description = "Model ID declared by the configured DeepSeek Harness Cordis composition"
    )]
    pub model: Option<String>,
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
        if self.selected_model().is_some() && self.cmd.base_command_override.is_some() {
            return Err(ExecutorError::Configuration(
                "DeepSeek Harness model selection requires the structured source-checkout command; clear base_command_override or configure the model in that custom ACP command"
                    .to_string(),
            ));
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

    fn selected_model(&self) -> Option<&str> {
        self.model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
    }

    fn mapping_value<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a Value> {
        mapping.get(Value::String(key.to_string()))
    }

    fn composition_entry<'a>(composition: &'a Value, id: &str) -> Option<&'a Mapping> {
        composition.as_sequence()?.iter().find_map(|entry| {
            let mapping = entry.as_mapping()?;
            (Self::mapping_value(mapping, "id").and_then(Value::as_str) == Some(id))
                .then_some(mapping)
        })
    }

    fn composition_models(composition: &Value) -> Result<Vec<String>, ExecutorError> {
        let mut models = BTreeSet::new();
        if let Some(config) = Self::composition_entry(composition, DEEPSEEK_LLM_ENTRY_ID)
            .and_then(|entry| Self::mapping_value(entry, "config"))
            .and_then(Value::as_mapping)
            && let Some(declared_models) =
                Self::mapping_value(config, "models").and_then(Value::as_sequence)
        {
            for declared_model in declared_models {
                let model = declared_model.as_str().or_else(|| {
                    declared_model
                        .as_mapping()
                        .and_then(|model| Self::mapping_value(model, "id"))
                        .and_then(Value::as_str)
                });
                if let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) {
                    models.insert(model.to_string());
                }
            }
        }

        if let Some(model) = Self::composition_entry(composition, ACP_AGENT_ENTRY_ID)
            .and_then(|entry| Self::mapping_value(entry, "config"))
            .and_then(Value::as_mapping)
            .and_then(|config| Self::mapping_value(config, "model"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            models.insert(model.to_string());
        }

        if models.is_empty() {
            return Err(ExecutorError::Configuration(
                "DeepSeek Harness Cordis composition declares no selectable models".to_string(),
            ));
        }
        Ok(models.into_iter().collect())
    }

    fn load_composition(&self) -> Result<(PathBuf, String, Value), ExecutorError> {
        let config_path = self.config_path().ok_or_else(|| {
            ExecutorError::Configuration(
                "DeepSeek Harness ACP composition path is unavailable; configure acp_config_path"
                    .to_string(),
            )
        })?;
        Self::validate_path(&config_path, "ACP composition")?;
        let source = fs::read_to_string(&config_path)?;
        let composition = serde_yaml::from_str(&source)?;
        Ok((config_path, source, composition))
    }

    fn yaml_single_quoted(value: &str) -> String {
        format!("'{}'", value.replace('\'', "''"))
    }

    fn render_model_overlay(
        config_path: &Path,
        source: &str,
        selected_model: &str,
        app_name: Option<&str>,
    ) -> Result<String, ExecutorError> {
        // Cordis patches replace the entire `config` mapping. Preserve the
        // source block verbatim so tagged `!!js` values keep their semantics.
        let lines = source.lines().collect::<Vec<_>>();
        let entry_start = lines
            .iter()
            .position(|line| {
                line.strip_prefix("- id:")
                    .is_some_and(|id| id.trim() == ACP_AGENT_ENTRY_ID)
            })
            .ok_or_else(|| {
                ExecutorError::Configuration(format!(
                    "DeepSeek Harness Cordis composition has no `{ACP_AGENT_ENTRY_ID}` entry"
                ))
            })?;
        let entry_end = lines[entry_start + 1..]
            .iter()
            .position(|line| line.starts_with("- id:"))
            .map(|offset| entry_start + 1 + offset)
            .unwrap_or(lines.len());
        let config_start = lines[entry_start + 1..entry_end]
            .iter()
            .position(|line| *line == "  config:")
            .map(|offset| entry_start + 1 + offset)
            .ok_or_else(|| {
                ExecutorError::Configuration(format!(
                    "DeepSeek Harness Cordis `{ACP_AGENT_ENTRY_ID}` entry has no block config"
                ))
            })?;
        let config_end = lines[config_start + 1..entry_end]
            .iter()
            .position(|line| {
                let trimmed = line.trim_start();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    return false;
                }
                line.len() - trimmed.len() <= 2
            })
            .map(|offset| config_start + 1 + offset)
            .unwrap_or(entry_end);

        let mut model_replaced = false;
        let mut rendered_config = String::new();
        for line in &lines[config_start + 1..config_end] {
            let trimmed = line.trim_start();
            let indentation = line.len() - trimmed.len();
            rendered_config.push_str("      ");
            if indentation == 4 && trimmed.starts_with("model:") {
                rendered_config.push_str("    model: ");
                rendered_config.push_str(&Self::yaml_single_quoted(selected_model));
                model_replaced = true;
            } else {
                rendered_config.push_str(line);
            }
            rendered_config.push('\n');
        }
        if !model_replaced {
            return Err(ExecutorError::Configuration(format!(
                "DeepSeek Harness Cordis `{ACP_AGENT_ENTRY_ID}` config has no block `model` field"
            )));
        }

        let mut rendered = format!(
            "- id: base\n  name: {}\n  config:\n    path: {}\n    patches:\n      - id: {ACP_AGENT_ENTRY_ID}\n",
            Self::yaml_single_quoted(CORDIS_INCLUDE_PLUGIN),
            Self::yaml_single_quoted(&config_path.to_string_lossy()),
        );
        if let Some(app_name) = app_name {
            rendered.push_str("        name: ");
            rendered.push_str(&Self::yaml_single_quoted(app_name));
            rendered.push('\n');
        }
        rendered.push_str("        config:\n");
        rendered.push_str(&rendered_config);
        Ok(rendered)
    }

    fn model_config_overlay(&self) -> Result<Option<(PathBuf, String)>, ExecutorError> {
        let Some(selected_model) = self.selected_model() else {
            return Ok(None);
        };
        let (config_path, source, composition) = self.load_composition()?;
        let available_models = Self::composition_models(&composition)?;
        if !available_models.iter().any(|model| model == selected_model) {
            return Err(ExecutorError::Configuration(format!(
                "DeepSeek Harness model `{selected_model}` is not declared by the Cordis composition; available models: {}",
                available_models.join(", ")
            )));
        }

        let app_entry =
            Self::composition_entry(&composition, ACP_AGENT_ENTRY_ID).ok_or_else(|| {
                ExecutorError::Configuration(format!(
                    "DeepSeek Harness Cordis composition has no `{ACP_AGENT_ENTRY_ID}` entry"
                ))
            })?;
        let app_name = Self::mapping_value(app_entry, "name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let app_config = Self::mapping_value(app_entry, "config")
            .and_then(Value::as_mapping)
            .ok_or_else(|| {
                ExecutorError::Configuration(format!(
                    "DeepSeek Harness Cordis `{ACP_AGENT_ENTRY_ID}` entry has no mapping config"
                ))
            })?;
        let current_model = Self::mapping_value(app_config, "model")
            .and_then(Value::as_str)
            .map(str::trim);
        if current_model == Some(selected_model) {
            return Ok(None);
        }
        let rendered =
            Self::render_model_overlay(&config_path, &source, selected_model, app_name.as_deref())?;

        let checkout = self.checkout_path().ok_or_else(|| {
            ExecutorError::Configuration(
                "DeepSeek Harness checkout path is unavailable; configure harness_path".to_string(),
            )
        })?;
        let mut hasher = Sha256::new();
        hasher.update(rendered.as_bytes());
        let digest = hasher.finalize();
        let file_name = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let overlay_path = checkout
            .join(MODEL_CONFIG_CACHE_RELATIVE_PATH)
            .join(format!("{file_name}.yml"));
        Ok(Some((overlay_path, rendered)))
    }

    fn effective_config_path(&self) -> Result<PathBuf, ExecutorError> {
        let Some((overlay_path, rendered)) = self.model_config_overlay()? else {
            return self.config_path().ok_or_else(|| {
                ExecutorError::Configuration(
                    "DeepSeek Harness ACP composition path is unavailable; configure acp_config_path"
                        .to_string(),
                )
            });
        };
        if fs::read_to_string(&overlay_path).ok().as_deref() != Some(rendered.as_str()) {
            let parent = overlay_path.parent().ok_or_else(|| {
                ExecutorError::Configuration(
                    "DeepSeek Harness model config cache path has no parent".to_string(),
                )
            })?;
            fs::create_dir_all(parent)?;
            let temporary_path = overlay_path.with_extension(format!("{}.tmp", Uuid::new_v4()));
            fs::write(&temporary_path, &rendered)?;
            if let Err(error) = fs::rename(&temporary_path, &overlay_path) {
                let target_matches =
                    fs::read_to_string(&overlay_path).ok().as_deref() == Some(rendered.as_str());
                let _ = fs::remove_file(&temporary_path);
                if !target_matches {
                    return Err(error.into());
                }
            }
        }
        Ok(overlay_path)
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
        let config = self.effective_config_path()?;
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

    fn interpret_acp_probe(&self, probe: &AcpCapabilityProbe) -> AcpProbeInterpretation {
        let mut interpretation = AcpProbeInterpretation::from_probe(probe);
        if interpretation.models.is_none() && self.cmd.base_command_override.is_none() {
            match self
                .load_composition()
                .and_then(|(_, _, composition)| Self::composition_models(&composition))
            {
                Ok(models) => interpretation.models = Some(models),
                Err(error) => tracing::warn!(
                    error = %error,
                    "failed to read DeepSeek Harness models from Cordis composition"
                ),
            }
        }
        interpretation.model_fallback = AcpModelFallback::Disabled;
        interpretation
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

    async fn list_models(
        &self,
        _current_dir: &Path,
        _env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        if self.cmd.base_command_override.is_some() {
            return Ok(None);
        }
        let (_, _, composition) = self.load_composition()?;
        Ok(Some(Self::composition_models(&composition)?))
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
        current_dir: &Path,
        prompt: &str,
        _session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        // The preview ACP server advertises neither session/resume nor
        // session/load, so continue the OpenTeams member with a fresh session.
        self.spawn(current_dir, prompt, env).await
    }

    async fn spawn_follow_up_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        _session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        self.spawn_structured(current_dir, prompt, env).await
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

    const COMPOSITION_FIXTURE: &str = r#"
- id: llm-deepseek
  name: '@deepseek-ai/dsh-llm-deepseek'
  config:
    models:
      - id: deepseek-v4-flash
      - id: deepseek-v4-pro
- id: acp-agent
  name: '@deepseek-ai/dsh-acp-demo'
  config:
    provider: deepseek-official
    model: deepseek-v4-pro
    persistenceRoot: !!js process.env.DSH_SNAPSHOT_SESSIONS_ROOT ?? './.sessions'
    persistenceCompression: !!js "process.env.DSH_SNAPSHOT === undefined ? 'zstd' : 'none'"
    workspaceContext:
      maxBytes: 65536
    persona: |
      You are a coding assistant powered by the {{model}} model.
"#;

    fn checkout_fixture() -> (tempfile::TempDir, DeepseekHarness) {
        let temp = tempfile::tempdir().expect("temporary checkout");
        let bin = temp.path().join(ACP_BIN_RELATIVE_PATH);
        let config = temp.path().join(ACP_CONFIG_RELATIVE_PATH);
        fs::create_dir_all(bin.parent().expect("bin parent")).expect("create bin parent");
        fs::create_dir_all(config.parent().expect("config parent")).expect("create config parent");
        fs::write(&bin, "export {}\n").expect("write bin");
        fs::write(&config, COMPOSITION_FIXTURE).expect("write config");
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

    #[tokio::test]
    async fn official_composition_models_are_discoverable() {
        let (_temp, harness) = checkout_fixture();
        let models = harness
            .list_models(
                Path::new("/tmp/workspace"),
                &ExecutionEnv::new(Default::default(), false, String::new()),
            )
            .await
            .expect("model discovery")
            .expect("models");

        assert_eq!(models, ["deepseek-v4-flash", "deepseek-v4-pro"]);

        let interpretation = harness.interpret_acp_probe(&AcpCapabilityProbe {
            protocol_version: "1".to_string(),
            agent_name: Some("DeepSeek Harness".to_string()),
            agent_version: None,
            auth_methods: Vec::new(),
            supports_session_list: false,
            supports_session_resume: false,
            supports_session_load: false,
            supports_session_close: false,
            supports_session_delete: false,
            supports_additional_directories: false,
            agent_capabilities: serde_json::json!({}),
            config_source: crate::executors::acp::AcpConfigSource::None,
            config_options: Vec::new(),
        });
        assert_eq!(
            interpretation.models,
            Some(vec![
                "deepseek-v4-flash".to_string(),
                "deepseek-v4-pro".to_string()
            ])
        );
        assert_eq!(interpretation.model_fallback, AcpModelFallback::Disabled);
    }

    #[test]
    fn selected_model_uses_cached_include_overlay_without_mutating_source() {
        let (temp, mut harness) = checkout_fixture();
        harness.model = Some("deepseek-v4-flash".to_string());
        let (program, args) = harness
            .build_command_builder()
            .expect("command builder")
            .build_initial()
            .expect("initial command")
            .into_parts_for_test();

        assert_eq!(program, "node");
        let overlay_path = PathBuf::from(&args[2]);
        assert!(overlay_path.starts_with(temp.path().join(MODEL_CONFIG_CACHE_RELATIVE_PATH)));
        assert_ne!(overlay_path, temp.path().join(ACP_CONFIG_RELATIVE_PATH));
        let overlay = fs::read_to_string(&overlay_path).expect("model overlay");
        assert!(overlay.contains("model: 'deepseek-v4-flash'"));
        assert!(overlay.contains("persistenceRoot: !!js"));
        assert!(
            fs::read_to_string(temp.path().join(ACP_CONFIG_RELATIVE_PATH))
                .expect("source config")
                .contains("model: deepseek-v4-pro")
        );

        let wrapper: Value = serde_yaml::from_str(&overlay).expect("valid wrapper yaml");
        let include = DeepseekHarness::composition_entry(&wrapper, "base").expect("base include");
        let include_config = DeepseekHarness::mapping_value(include, "config")
            .and_then(Value::as_mapping)
            .expect("include config");
        assert_eq!(
            DeepseekHarness::mapping_value(include_config, "path").and_then(Value::as_str),
            Some(
                temp.path()
                    .join(ACP_CONFIG_RELATIVE_PATH)
                    .to_string_lossy()
                    .as_ref()
            )
        );
        let patch = DeepseekHarness::mapping_value(include_config, "patches")
            .and_then(Value::as_sequence)
            .and_then(|patches| patches.first())
            .and_then(Value::as_mapping)
            .expect("app patch");
        let patched_model = DeepseekHarness::mapping_value(patch, "config")
            .and_then(Value::as_mapping)
            .and_then(|config| DeepseekHarness::mapping_value(config, "model"))
            .and_then(Value::as_str);
        assert_eq!(patched_model, Some("deepseek-v4-flash"));
    }

    #[test]
    fn unknown_model_is_rejected_before_process_start() {
        let (_temp, mut harness) = checkout_fixture();
        harness.model = Some("deepseek-v4-unknown".to_string());
        let error = harness
            .build_command_builder()
            .expect_err("unknown model must fail");

        assert!(error.to_string().contains("is not declared"));
        assert!(error.to_string().contains("deepseek-v4-flash"));
        assert!(error.to_string().contains("deepseek-v4-pro"));
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

    #[tokio::test]
    async fn follow_up_falls_back_to_a_fresh_session() {
        let workspace = tempfile::tempdir().expect("temporary workspace");
        let harness = DeepseekHarness {
            cmd: CmdOverrides {
                base_command_override: Some(
                    "openteams-deepseek-harness-command-that-does-not-exist".to_string(),
                ),
                ..CmdOverrides::default()
            },
            ..DeepseekHarness::default()
        };
        let error = match harness
            .spawn_follow_up(
                workspace.path(),
                "continue",
                "old-session",
                None,
                &ExecutionEnv::new(Default::default(), false, String::new()),
            )
            .await
        {
            Ok(_) => panic!("missing command must fail"),
            Err(error) => error,
        };

        assert!(!matches!(error, ExecutorError::FollowUpNotSupported(_)));
    }
}
