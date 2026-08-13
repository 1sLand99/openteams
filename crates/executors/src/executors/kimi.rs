use std::{
    collections::BTreeSet,
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
    AcpCapabilityProbe, AcpClientServicePolicy, AcpConfigChoice, AcpConfigOptionKind,
    AcpConfigOptionSnapshot, AcpConfigSource, AcpExecutionOptions,
    mcp::{AcpMcpPolicy, resolve_effective_mcp_config},
};
use crate::{
    approvals::ExecutorApprovalService,
    command::{
        CmdOverrides, CommandBuildError, CommandBuilder, apply_overrides, command_is_available,
    },
    env::ExecutionEnv,
    executors::{
        AppendPrompt, AvailabilityInfo, ExecutorError, ExecutorPrompt, SpawnedChild,
        StandardCodingAgentExecutor,
        utils::{json_has_nonempty_string, read_json_file},
    },
    mcp_config::{McpConfig, read_canonical_mcp_config},
    model_discovery::{
        discover_model_map_from_cli_command, model_ids_from_model_map_json, read_config_value,
    },
};

#[derive(Derivative, Clone, Serialize, Deserialize, TS, JsonSchema)]
#[derivative(Debug, PartialEq)]
pub struct KimiCode {
    #[serde(default)]
    pub append_prompt: AppendPrompt,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Kimi model alias configured in Kimi Code CLI")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Per-run Kimi thinking effort: low, high, or max")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpExecutionOptions>,
    #[serde(flatten)]
    pub cmd: CmdOverrides,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub acp_mcp_policy: AcpMcpPolicy,
    #[serde(skip)]
    #[ts(skip)]
    #[derivative(Debug = "ignore", PartialEq = "ignore")]
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
}

impl KimiCode {
    const BASE_COMMAND: &'static str = "kimi";
    const TERMINAL_AUTH_METHOD: &'static str = "login";

    pub fn base_command() -> &'static str {
        Self::BASE_COMMAND
    }

    fn build_command_builder(&self) -> Result<CommandBuilder, CommandBuildError> {
        apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params(["acp"]),
            &self.cmd,
        )
    }

    fn provider_list_command(&self) -> Result<CommandBuilder, CommandBuildError> {
        apply_overrides(
            CommandBuilder::new(Self::BASE_COMMAND).extend_params(["provider", "list", "--json"]),
            &self.cmd,
        )
    }

    fn validate_auth_selection(&self) -> Result<(), ExecutorError> {
        match self.acp.as_ref().and_then(|options| options.auth.as_ref()) {
            Some(AcpAuthSelection::MethodId { method_id }) => {
                Err(unsupported_terminal_auth(method_id))
            }
            Some(AcpAuthSelection::Auto) | None => Ok(()),
        }
    }

    async fn acp_harness(&self, env: &ExecutionEnv) -> Result<AcpAgentHarness, ExecutorError> {
        let options = self.acp.clone().unwrap_or_default();
        let approval_policy = match options.approval_mode.unwrap_or_default() {
            AcpApprovalMode::Ask => AcpApprovalPolicy::Ask,
            AcpApprovalMode::AutoAllow => AcpApprovalPolicy::AutoAllow,
            AcpApprovalMode::AutoReject => AcpApprovalPolicy::AutoReject,
        };
        let additional_directories = options
            .validated_directories()
            .await
            .map_err(ExecutorError::Io)?;
        let full_access = options.access_mode.unwrap_or_default() == AcpAccessMode::FullAccess;
        let mut harness = AcpAgentHarness::new()
            .with_approval_policy(approval_policy)
            .with_required_session_mode("mode", "default")
            .with_additional_directories(additional_directories)
            .with_client_services(AcpClientServicePolicy {
                read_text_file: true,
                write_text_file: true,
                terminal: true,
                full_access,
                ..AcpClientServicePolicy::default()
            });
        self.validate_auth_selection()?;

        let config_overrides = options.config_overrides.as_deref().unwrap_or_default();
        let has_model_override = config_overrides.iter().any(|selection| {
            selection.category_snapshot.as_deref() == Some("model")
                || selection.option_id.eq_ignore_ascii_case("model")
        });
        let has_thought_override = config_overrides.iter().any(|selection| {
            selection.category_snapshot.as_deref() == Some("thought_level")
                || matches!(
                    normalized_config_key(&selection.option_id).as_str(),
                    "thinking" | "thoughtlevel"
                )
        });
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|_| !has_model_override)
        {
            harness = harness.with_model(model);
        }
        if let Some(effort) = self
            .thinking_effort
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|_| !has_thought_override)
        {
            harness = harness.with_native_thought_level_fallback(effort);
        }
        for selection in config_overrides {
            harness = harness.with_config_override(selection);
        }

        let canonical = match kimi_mcp_config_path(Some(env)) {
            Some(path) => read_canonical_mcp_config(&path, &McpConfig::canonical_acp()).await?,
            None => serde_json::json!({ "mcpServers": {} }),
        };
        let effective = resolve_effective_mcp_config(&canonical, &self.acp_mcp_policy)?;
        tracing::debug!(
            server_count = effective.servers.len(),
            config_hash = %effective.config_hash,
            "resolved effective Kimi ACP MCP configuration"
        );
        Ok(harness.with_mcp_servers(effective.servers))
    }

    async fn configured_default_model(&self, env: &ExecutionEnv) -> Option<String> {
        for home in kimi_code_homes(Some(env)) {
            let path = home.join("config.toml");
            let config = match read_config_value(&path).await {
                Ok(Some(config)) => config,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        path = %path.display(),
                        "ignoring unreadable Kimi config"
                    );
                    continue;
                }
            };
            if let Some(model) = config
                .get("default_model")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                return Some(model.to_string());
            }
        }
        None
    }

    async fn configured_models(&self, env: &ExecutionEnv) -> Vec<String> {
        let mut models = BTreeSet::new();
        for home in kimi_code_homes(Some(env)) {
            let path = home.join("config.toml");
            match read_config_value(&path).await {
                Ok(Some(config)) => models.extend(model_ids_from_model_map_json(&config)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        path = %path.display(),
                        "ignoring unreadable Kimi config"
                    );
                }
            }
        }
        models.into_iter().collect()
    }

    fn discovered_model_option(
        &self,
        models: Vec<String>,
        default_model: Option<&str>,
    ) -> AcpConfigOptionSnapshot {
        let current_value = self
            .model
            .as_deref()
            .or(default_model)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .filter(|selected| models.iter().any(|model| model == selected))
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| models[0].clone());
        AcpConfigOptionSnapshot {
            id: "model".to_string(),
            name: "Model".to_string(),
            description: Some("Models reported by Kimi Code CLI".to_string()),
            category: Some("model".to_string()),
            kind: AcpConfigOptionKind::Select {
                current_value,
                options: models
                    .into_iter()
                    .map(|model| AcpConfigChoice {
                        name: model.clone(),
                        value: model,
                        description: None,
                    })
                    .collect(),
            },
        }
    }
}

#[async_trait]
impl StandardCodingAgentExecutor for KimiCode {
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

    fn is_authenticated(&self, env: &ExecutionEnv) -> bool {
        let env = env.clone().with_profile(&self.cmd);
        let homes = kimi_code_homes(Some(&env));
        let oauth_login = homes.iter().any(|home| {
            read_json_file(&home.join("credentials").join("kimi-code.json")).is_some_and(|value| {
                json_has_nonempty_string(&value, &["/access_token", "/refresh_token"])
            })
        });
        let provider_configured = homes.iter().any(|home| {
            std::fs::read_to_string(home.join("config.toml"))
                .ok()
                .and_then(|content| toml::from_str::<toml::Value>(&content).ok())
                .is_some_and(|value| kimi_provider_configured(&value))
        });
        self.authentication_detected(
            &env,
            &["KIMI_API_KEY", "MOONSHOT_API_KEY"],
            oauth_login || provider_configured,
        )
    }

    fn use_approvals(&mut self, approvals: Arc<dyn ExecutorApprovalService>) {
        self.approvals = Some(approvals);
    }

    async fn list_models(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
    ) -> Result<Option<Vec<String>>, ExecutorError> {
        let mut models = BTreeSet::new();
        match discover_model_map_from_cli_command(
            current_dir,
            env,
            &self.cmd,
            self.provider_list_command()?,
        )
        .await
        {
            Ok(Some(discovered)) => models.extend(discovered),
            Ok(None) => {}
            Err(error) => tracing::debug!(
                error = %error,
                "Kimi provider listing is unavailable; falling back to configured models"
            ),
        }
        models.extend(self.configured_models(env).await);
        if let Some(model) = self
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            models.insert(model.to_string());
        }
        Ok((!models.is_empty()).then(|| models.into_iter().collect()))
    }

    async fn probe_acp(
        &self,
        current_dir: &Path,
        env: &ExecutionEnv,
        auth_method_id: Option<&str>,
    ) -> Result<Option<AcpCapabilityProbe>, ExecutorError> {
        if let Some(method_id) = auth_method_id {
            return Err(unsupported_terminal_auth(method_id));
        }
        let mut probe = super::acp::runtime::probe_acp_command_without_session(
            self.build_command_builder()?.build_initial()?,
            current_dir,
            env,
            &self.cmd,
            None,
        )
        .await?;
        if let Some(models) = self.list_models(current_dir, env).await?
            && !models.is_empty()
        {
            let default_model = self.configured_default_model(env).await;
            probe.config_source = AcpConfigSource::Stable;
            probe
                .config_options
                .push(self.discovered_model_option(models, default_model.as_deref()));
        }
        Ok(Some(probe))
    }

    async fn spawn(
        &self,
        current_dir: &Path,
        prompt: &str,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let harness = self.acp_harness(env).await?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let command = self.build_command_builder()?.build_initial()?;
        harness
            .spawn_with_command(
                current_dir,
                combined_prompt,
                command,
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
        let harness = self.acp_harness(env).await?;
        let combined_prompt = self.append_prompt.combine_prompt(prompt);
        let command = self.build_command_builder()?.build_follow_up(&[])?;
        harness
            .spawn_follow_up_with_command(
                current_dir,
                combined_prompt,
                session_id,
                command,
                env,
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
        let harness = self.acp_harness(env).await?;
        let command = self.build_command_builder()?.build_initial()?;
        let mut prompt = prompt.clone();
        prompt.text = self.append_prompt.combine_prompt(&prompt.text);
        harness
            .spawn_structured_with_command(
                current_dir,
                prompt,
                command,
                env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    async fn spawn_follow_up_structured(
        &self,
        current_dir: &Path,
        prompt: &ExecutorPrompt,
        session_id: &str,
        _reset_to_message_id: Option<&str>,
        env: &ExecutionEnv,
    ) -> Result<SpawnedChild, ExecutorError> {
        let harness = self.acp_harness(env).await?;
        let command = self.build_command_builder()?.build_follow_up(&[])?;
        let mut prompt = prompt.clone();
        prompt.text = self.append_prompt.combine_prompt(&prompt.text);
        harness
            .spawn_follow_up_structured_with_command(
                current_dir,
                prompt,
                session_id,
                command,
                env,
                &self.cmd,
                self.approvals.clone(),
            )
            .await
    }

    fn normalize_logs(&self, msg_store: Arc<MsgStore>, worktree_path: &Path) {
        super::acp::normalize_logs(msg_store, worktree_path);
    }

    fn default_runtime_config_path(&self) -> Option<PathBuf> {
        kimi_runtime_config_path(None)
    }

    fn default_mcp_config_path(&self) -> Option<PathBuf> {
        kimi_mcp_config_path(None)
    }

    fn native_skill_discovery_roots(&self) -> Vec<PathBuf> {
        let mut roots = dirs::home_dir()
            .map(|home| vec![home.join(".agents").join("skills")])
            .unwrap_or_default();
        for home in kimi_code_homes(None) {
            roots.push(home.join("skills"));
        }
        roots.sort();
        roots.dedup();
        roots
    }

    fn get_availability_info(&self) -> AvailabilityInfo {
        if command_is_available(Self::BASE_COMMAND, &self.cmd) {
            AvailabilityInfo::InstallationFound
        } else {
            AvailabilityInfo::NotFound
        }
    }
}

fn kimi_provider_configured(value: &toml::Value) -> bool {
    value
        .get("providers")
        .and_then(toml::Value::as_table)
        .is_some_and(|providers| !providers.is_empty())
        || value
            .get("services")
            .and_then(toml::Value::as_table)
            .is_some_and(|services| services.values().any(toml_value_has_credential))
}

fn toml_value_has_credential(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => table.iter().any(|(key, value)| {
            matches!(
                key.as_str(),
                "api_key" | "apiKey" | "token" | "access_token"
            ) && value.as_str().is_some_and(|value| !value.trim().is_empty())
                || toml_value_has_credential(value)
        }),
        toml::Value::Array(values) => values.iter().any(toml_value_has_credential),
        _ => false,
    }
}

fn normalized_config_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn unsupported_terminal_auth(method_id: &str) -> ExecutorError {
    let message = if method_id == KimiCode::TERMINAL_AUTH_METHOD {
        "Kimi ACP login is terminal-based; run `kimi login` and keep ACP auth set to auto"
            .to_string()
    } else {
        format!(
            "Kimi ACP authentication method `{method_id}` is not supported; run `kimi login` and keep ACP auth set to auto"
        )
    };
    ExecutorError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        message,
    ))
}

fn kimi_code_homes(env: Option<&ExecutionEnv>) -> Vec<PathBuf> {
    let mut configured = Vec::new();
    for key in ["KIMI_CODE_HOME", "KIMI_SHARE_DIR"] {
        if let Some(path) = env
            .and_then(|env| env.get(key).cloned())
            .or_else(|| std::env::var(key).ok())
            .and_then(|value| {
                let value = value.trim();
                (!value.is_empty()).then(|| PathBuf::from(value))
            })
            && !configured.contains(&path)
        {
            configured.push(path);
        }
    }
    if !configured.is_empty() {
        return configured;
    }

    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let modern = home.join(".kimi-code");
    let legacy = home.join(".kimi");
    match (modern.exists(), legacy.exists()) {
        (false, true) => vec![legacy, modern],
        _ => vec![modern, legacy],
    }
}

fn kimi_code_home(env: Option<&ExecutionEnv>) -> Option<PathBuf> {
    kimi_code_homes(env).into_iter().next()
}

fn kimi_mcp_config_path(env: Option<&ExecutionEnv>) -> Option<PathBuf> {
    kimi_code_home(env).map(|home| home.join("mcp.json"))
}

fn kimi_runtime_config_path(env: Option<&ExecutionEnv>) -> Option<PathBuf> {
    kimi_code_home(env).map(|home| home.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::env::RepoContext;

    fn kimi() -> KimiCode {
        KimiCode {
            append_prompt: AppendPrompt::default(),
            model: Some("kimi-code/k3".to_string()),
            thinking_effort: Some("high".to_string()),
            acp: None,
            cmd: CmdOverrides::default(),
            acp_mcp_policy: AcpMcpPolicy::default(),
            approvals: None,
        }
    }

    #[test]
    fn provider_or_service_credentials_count_as_authentication() {
        let provider: toml::Value = toml::from_str(
            r#"
                [providers.custom]
                type = "openai"
                base_url = "http://localhost:11434/v1"
            "#,
        )
        .unwrap();
        let service: toml::Value = toml::from_str(
            r#"
                [services.search]
                api_key = "service-token"
            "#,
        )
        .unwrap();

        assert!(kimi_provider_configured(&provider));
        assert!(kimi_provider_configured(&service));
        assert!(!kimi_provider_configured(&toml::Value::Table(
            Default::default()
        )));
    }

    #[test]
    fn command_builder_uses_kimi_acp_subcommand() {
        let (_program, args) = kimi()
            .build_command_builder()
            .unwrap()
            .build_initial()
            .unwrap()
            .into_parts_for_test();
        assert!(args.iter().any(|arg| arg == "acp"));
        assert!(!args.iter().any(|arg| arg == "--print"));
        assert!(!args.iter().any(|arg| arg == "--output-format"));
        assert!(!args.iter().any(|arg| arg == "--model"));
    }

    #[test]
    fn model_discovery_uses_provider_list_json() {
        let (_program, args) = kimi()
            .provider_list_command()
            .unwrap()
            .build_initial()
            .unwrap()
            .into_parts_for_test();
        assert_eq!(args, vec!["provider", "list", "--json"]);
    }

    #[test]
    fn discovered_models_become_stable_acp_model_option() {
        let option = kimi().discovered_model_option(
            vec![
                "kimi-code/kimi-for-coding".to_string(),
                "kimi-code/k3".to_string(),
            ],
            Some("kimi-code/k3"),
        );
        assert_eq!(option.category.as_deref(), Some("model"));
        let AcpConfigOptionKind::Select {
            current_value,
            options,
        } = option.kind
        else {
            panic!("expected select option");
        };
        assert_eq!(current_value, "kimi-code/k3");
        assert_eq!(options.len(), 2);
    }

    #[test]
    fn explicit_terminal_auth_is_rejected_with_login_guidance() {
        let mut executor = kimi();
        executor.acp = Some(AcpExecutionOptions {
            auth: Some(AcpAuthSelection::MethodId {
                method_id: "login".to_string(),
            }),
            ..Default::default()
        });
        let error = executor.validate_auth_selection().unwrap_err().to_string();
        assert!(error.contains("kimi login"));
        assert!(error.contains("auth set to auto"));
    }

    #[tokio::test]
    async fn every_approval_policy_enforces_default_agent_mode() {
        let temp = TempDir::new().expect("create Kimi test home");
        let mut env = ExecutionEnv::new(
            RepoContext::new(temp.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        );
        env.insert("KIMI_CODE_HOME", temp.path().to_string_lossy().into_owned());

        for approval_mode in [
            AcpApprovalMode::Ask,
            AcpApprovalMode::AutoAllow,
            AcpApprovalMode::AutoReject,
        ] {
            let mut executor = kimi();
            executor.acp = Some(AcpExecutionOptions {
                approval_mode: Some(approval_mode),
                config_overrides: Some(vec![super::super::acp::AcpConfigOverride {
                    option_id: "mode".to_string(),
                    value: super::super::acp::AcpConfigValue::ValueId {
                        value: "yolo".to_string(),
                    },
                    label_snapshot: Some("Mode".to_string()),
                    category_snapshot: Some("mode".to_string()),
                }]),
                ..Default::default()
            });

            let harness = executor
                .acp_harness(&env)
                .await
                .expect("build Kimi harness");
            let (option_id, value) = harness.required_session_mode().expect("required Kimi mode");
            assert_eq!(option_id, "mode");
            assert_eq!(
                value,
                &agent_client_protocol::schema::v1::SessionConfigOptionValue::value_id("default")
            );
        }
    }

    #[tokio::test]
    async fn legacy_kimi_share_dir_provides_models_and_default() {
        let temp = TempDir::new().expect("create legacy Kimi home");
        tokio::fs::write(
            temp.path().join("config.toml"),
            r#"
                default_model = "kimi-code/k3"

                [models."kimi-code/kimi-for-coding"]
                model = "kimi-for-coding"

                [models."kimi-code/k3"]
                model = "k3"
            "#,
        )
        .await
        .expect("write legacy Kimi config");
        let mut env = ExecutionEnv::new(
            RepoContext::new(temp.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        );
        env.insert("KIMI_SHARE_DIR", temp.path().to_string_lossy().into_owned());

        assert_eq!(
            kimi().configured_models(&env).await,
            vec![
                "kimi-code/k3".to_string(),
                "kimi-code/kimi-for-coding".to_string()
            ]
        );
        assert_eq!(
            kimi().configured_default_model(&env).await.as_deref(),
            Some("kimi-code/k3")
        );
        assert_eq!(
            kimi_mcp_config_path(Some(&env)),
            Some(temp.path().join("mcp.json"))
        );
        assert_eq!(
            kimi_runtime_config_path(Some(&env)),
            Some(temp.path().join("config.toml"))
        );
    }

    #[tokio::test]
    async fn unsupported_provider_command_falls_back_to_configured_models() {
        let temp = TempDir::new().expect("create legacy Kimi home");
        tokio::fs::write(
            temp.path().join("config.toml"),
            r#"
                [models."kimi-code/kimi-for-coding"]
                model = "kimi-for-coding"
            "#,
        )
        .await
        .expect("write legacy Kimi config");
        let mut env = ExecutionEnv::new(
            RepoContext::new(temp.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        );
        env.insert("KIMI_SHARE_DIR", temp.path().to_string_lossy().into_owned());
        let mut executor = kimi();
        executor.model = None;
        #[cfg(windows)]
        {
            executor.cmd.base_command_override = Some("cmd.exe /c exit /b 2".to_string());
        }
        #[cfg(not(windows))]
        {
            executor.cmd.base_command_override = Some("sh -c 'exit 2'".to_string());
        }

        assert_eq!(
            executor.list_models(temp.path(), &env).await.unwrap(),
            Some(vec!["kimi-code/kimi-for-coding".to_string()])
        );
    }

    #[tokio::test]
    async fn malformed_config_is_skipped_without_failing_probe() {
        let temp = TempDir::new().expect("create Kimi home with malformed config");
        tokio::fs::write(temp.path().join("config.toml"), "default_model = [broken")
            .await
            .expect("write malformed Kimi config");
        let mut env = ExecutionEnv::new(
            RepoContext::new(temp.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        );
        env.insert("KIMI_SHARE_DIR", temp.path().to_string_lossy().into_owned());

        assert!(kimi().configured_models(&env).await.is_empty());
        assert_eq!(kimi().configured_default_model(&env).await, None);
        let mut executor = kimi();
        executor.model = None;
        #[cfg(windows)]
        {
            executor.cmd.base_command_override = Some("cmd.exe /c exit /b 2".to_string());
        }
        #[cfg(not(windows))]
        {
            executor.cmd.base_command_override = Some("sh -c 'exit 2'".to_string());
        }
        assert_eq!(executor.list_models(temp.path(), &env).await.unwrap(), None);
    }
}
