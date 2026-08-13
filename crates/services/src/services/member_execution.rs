use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{Result, anyhow};
use db::models::{
    chat_agent::ChatAgent,
    chat_session::ChatSession,
    chat_session_agent::ChatSessionAgent,
    member_execution_config::MemberExecutionConfig,
    project_member::{ProjectMember, ProjectMemberType},
    workflow_agent_session::WorkflowAgentSession,
};
use executors::{
    env::ExecutionEnv,
    executors::{
        BaseCodingAgent, CodingAgent, StandardCodingAgentExecutor,
        acp::{AcpExecutionOptions, mcp::AcpMcpPolicy},
    },
    model_sync::with_member_execution_overrides,
    profile::{ExecutorConfigs, ExecutorProfileId, canonical_variant_key},
};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::services::{
    agent_runtime::apply_agent_runtime_config, native_skills::list_native_skills_for_runner,
};

pub const EXECUTOR_PROFILE_VARIANT_KEY: &str = "executor_profile_variant";

#[derive(Debug, Clone)]
pub struct EffectiveMemberExecutionConfig {
    pub runner_type: BaseCodingAgent,
    pub profile_id: ExecutorProfileId,
    pub model_name: Option<String>,
    pub thinking_effort: Option<String>,
    pub model_variant: Option<String>,
    pub acp: Option<AcpExecutionOptions>,
    pub has_member_config: bool,
}

#[derive(Debug, Clone)]
pub struct SessionAgentExecutionConfigRefresh {
    pub session_agent: ChatSessionAgent,
    pub changed: bool,
}

pub async fn refresh_session_agent_execution_config_before_run(
    pool: &SqlitePool,
    session: &ChatSession,
    session_agent: ChatSessionAgent,
    agent_id: Uuid,
    workflow_agent_session_id: Option<Uuid>,
) -> Result<SessionAgentExecutionConfigRefresh, sqlx::Error> {
    let mut project_member = if let Some(project_member_id) = session_agent.project_member_id {
        ProjectMember::find_by_id(pool, project_member_id).await?
    } else {
        None
    };

    if project_member.as_ref().is_some_and(|member| {
        member.member_type != ProjectMemberType::Agent || member.agent_id != Some(agent_id)
    }) {
        project_member = None;
    }

    if project_member.is_none()
        && let Some(project_id) = session.project_id
    {
        project_member = ProjectMember::find_by_project(pool, project_id)
            .await?
            .into_iter()
            .find(|member| {
                member.member_type == ProjectMemberType::Agent && member.agent_id == Some(agent_id)
            });
    }

    let Some(project_member) = project_member else {
        return Ok(SessionAgentExecutionConfigRefresh {
            session_agent,
            changed: false,
        });
    };

    let current_config = session_agent.execution_config.0.clone().normalized();
    let next_config = project_member.execution_config.0.clone().normalized();
    let changed =
        current_config != next_config || session_agent.project_member_id != Some(project_member.id);
    if !changed {
        return Ok(SessionAgentExecutionConfigRefresh {
            session_agent,
            changed: false,
        });
    }

    if let Some(workflow_agent_session_id) = workflow_agent_session_id {
        WorkflowAgentSession::clear_runtime_ids(pool, workflow_agent_session_id).await?;
    }
    let session_agent = ChatSessionAgent::update_execution_config_for_next_run(
        pool,
        session_agent.id,
        Some(project_member.id),
        next_config,
    )
    .await?;

    Ok(SessionAgentExecutionConfigRefresh {
        session_agent,
        changed: true,
    })
}

impl EffectiveMemberExecutionConfig {
    pub fn analytics_profile_label(&self) -> String {
        if self.has_member_config {
            format!("{}:MEMBER", self.runner_type)
        } else {
            self.profile_id.to_string()
        }
    }
}

pub fn resolve_effective_member_execution_config(
    agent: &ChatAgent,
    session_agent: &ChatSessionAgent,
) -> Result<EffectiveMemberExecutionConfig> {
    let member_config = session_agent.execution_config.0.clone().normalized();
    let has_member_config = member_config.has_overrides();
    let fallback_runner = parse_runner_type(&agent.runner_type)?;
    let runner_type = member_config.runner_type.unwrap_or(fallback_runner);

    let profile_id = if has_member_config {
        ExecutorProfileId::new(runner_type)
    } else {
        match extract_executor_profile_variant(&agent.tools_enabled.0) {
            Some(variant) => ExecutorProfileId::with_variant(runner_type, variant),
            None => ExecutorProfileId::new(runner_type),
        }
    };

    Ok(EffectiveMemberExecutionConfig {
        runner_type,
        profile_id,
        model_name: resolve_model_name(agent, &member_config, has_member_config),
        thinking_effort: if has_member_config {
            member_config.thinking_effort
        } else {
            None
        },
        model_variant: if has_member_config {
            member_config.model_variant
        } else {
            None
        },
        acp: member_config.acp,
        has_member_config,
    })
}

pub fn build_effective_member_executor(
    agent: &ChatAgent,
    session_agent: &ChatSessionAgent,
    env: &mut ExecutionEnv,
) -> Result<(EffectiveMemberExecutionConfig, CodingAgent)> {
    let resolved = resolve_effective_member_execution_config(agent, session_agent)?;
    let mut executor =
        ExecutorConfigs::get_cached().get_coding_agent_or_default(&resolved.profile_id);
    apply_agent_runtime_config(resolved.runner_type, &mut executor, env)?;
    executor = with_member_execution_overrides(
        &executor,
        resolved.model_name.as_deref(),
        resolved.thinking_effort.as_deref(),
        resolved.model_variant.as_deref(),
    );
    if let Some(member_acp) = &resolved.acp {
        executor.overlay_acp_execution_options(member_acp);
    }
    let mcp_policy = resolve_acp_mcp_policy(&agent.tools_enabled.0);
    executor.set_acp_mcp_policy(mcp_policy);
    Ok((resolved, executor))
}

pub async fn build_effective_member_executor_for_run(
    pool: &SqlitePool,
    agent: &ChatAgent,
    session_agent: &ChatSessionAgent,
    env: &mut ExecutionEnv,
) -> Result<(EffectiveMemberExecutionConfig, CodingAgent)> {
    let (resolved, mut executor) = build_effective_member_executor(agent, session_agent, env)?;
    if let CodingAgent::Pi(config) = &mut executor {
        let skill_paths = resolve_pi_member_skill_paths(pool, session_agent, config).await?;
        config
            .freeze_runtime_snapshot(skill_paths)
            .await
            .map_err(|error| anyhow!(error.to_string()))?;
    }
    Ok((resolved, executor))
}

async fn resolve_pi_member_skill_paths(
    pool: &SqlitePool,
    session_agent: &ChatSessionAgent,
    config: &executors::executors::pi::Pi,
) -> Result<Vec<PathBuf>> {
    let mut requested = Vec::new();
    let mut seen = HashSet::new();
    for raw_id in &session_agent.allowed_skill_ids.0 {
        let id = raw_id.trim();
        if id.is_empty() || Uuid::parse_str(id).is_err() {
            return Err(anyhow!(
                "invalid Pi member configuration: allowed skill ID is invalid"
            ));
        }
        if seen.insert(id.to_string()) {
            requested.push(id.to_string());
        }
    }
    if requested.is_empty() {
        return Ok(Vec::new());
    }

    let installed = list_native_skills_for_runner(pool, BaseCodingAgent::Pi)
        .await
        .map_err(|error| {
            anyhow!("invalid Pi member configuration: failed to resolve Skill Registry: {error}")
        })?;
    let roots = canonical_existing_roots(config.native_skill_discovery_roots()).await?;
    let installed = installed
        .into_iter()
        .filter(|item| item.enabled)
        .map(|item| (item.skill.id.to_string(), PathBuf::from(item.native_path)))
        .collect::<Vec<_>>();
    resolve_requested_pi_skill_paths(&requested, &installed, &roots).await
}

async fn resolve_requested_pi_skill_paths(
    requested: &[String],
    installed: &[(String, PathBuf)],
    roots: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(requested.len());
    for skill_id in requested {
        let item = installed
            .iter()
            .find(|(installed_id, _)| installed_id == skill_id)
            .ok_or_else(|| anyhow!(
                "invalid Pi member configuration: allowed skill `{skill_id}` is missing from the Skill Registry"
            ))?;
        paths.push(validate_pi_skill_path(&item.1, roots).await?);
    }
    Ok(paths)
}

async fn canonical_existing_roots(roots: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    let mut canonical = Vec::new();
    for root in roots {
        match tokio::fs::canonicalize(&root).await {
            Ok(root) if root.is_absolute() => canonical.push(root),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow!(
                    "invalid Pi member configuration: failed to validate Skill Registry root: {error}"
                ));
            }
        }
    }
    Ok(canonical)
}

async fn validate_pi_skill_path(path: &Path, roots: &[PathBuf]) -> Result<PathBuf> {
    let canonical = tokio::fs::canonicalize(path).await.map_err(|_| {
        anyhow!("invalid Pi member configuration: authorized SKILL.md path is missing or invalid")
    })?;
    let metadata = tokio::fs::metadata(&canonical).await.map_err(|_| {
        anyhow!("invalid Pi member configuration: authorized SKILL.md path is missing or invalid")
    })?;
    if !canonical.is_absolute()
        || !metadata.is_file()
        || canonical.file_name().and_then(|name| name.to_str()) != Some("SKILL.md")
    {
        return Err(anyhow!(
            "invalid Pi member configuration: authorized Skill path is not an absolute SKILL.md file"
        ));
    }
    if !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err(anyhow!(
            "invalid Pi member configuration: authorized SKILL.md path escapes the Skill Registry"
        ));
    }
    Ok(canonical)
}

pub fn executor_acp_full_access_enabled(executor: &CodingAgent) -> bool {
    executor.acp_full_access_enabled()
}

fn resolve_acp_mcp_policy(tools_enabled: &serde_json::Value) -> AcpMcpPolicy {
    let Some(servers) = tools_enabled
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
    else {
        return AcpMcpPolicy::default();
    };
    let mut allowed_server_names = std::collections::BTreeSet::new();
    let mut disabled_server_names = std::collections::BTreeSet::new();
    for (name, setting) in servers {
        let enabled = match setting {
            serde_json::Value::Bool(enabled) => *enabled,
            serde_json::Value::Object(setting) => setting
                .get("enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            _ => false,
        };
        if enabled {
            allowed_server_names.insert(name.clone());
        } else {
            disabled_server_names.insert(name.clone());
        }
    }
    AcpMcpPolicy {
        allowed_server_names: Some(allowed_server_names),
        disabled_server_names,
    }
}

pub fn parse_runner_type(raw: &str) -> Result<BaseCodingAgent> {
    let trimmed = raw.trim();
    let mut normalized = trimmed.replace(['-', ' '], "_").to_ascii_uppercase();
    if normalized == "OPENTEAMS_CLI" {
        normalized = "OPEN_TEAMS_CLI".to_string();
    }
    BaseCodingAgent::from_str(&normalized).map_err(|_| anyhow!("unknown runner type: {trimmed}"))
}

pub fn extract_executor_profile_variant(tools_enabled: &serde_json::Value) -> Option<String> {
    let variant = tools_enabled
        .as_object()
        .and_then(|value| value.get(EXECUTOR_PROFILE_VARIANT_KEY))
        .and_then(serde_json::Value::as_str)?
        .trim();
    if variant.is_empty() || variant.eq_ignore_ascii_case("DEFAULT") {
        return None;
    }
    Some(canonical_variant_key(variant))
}

fn resolve_model_name(
    agent: &ChatAgent,
    member_config: &MemberExecutionConfig,
    has_member_config: bool,
) -> Option<String> {
    if has_member_config {
        member_config.model_name.clone()
    } else {
        normalize_optional_string(agent.model_name.clone())
    }
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use db::models::{
        chat_agent::ChatAgent,
        chat_session_agent::{ChatSessionAgent, ChatSessionAgentState},
    };
    use executors::executors::{BaseCodingAgent, acp::AcpAccessMode};
    use sqlx::types::Json;
    use uuid::Uuid;

    use super::*;

    fn agent() -> ChatAgent {
        ChatAgent {
            id: Uuid::new_v4(),
            name: "coder".to_string(),
            runner_type: "codex".to_string(),
            system_prompt: String::new(),
            tools_enabled: Json(serde_json::json!({
                "executor_profile_variant": "HIGH_REASONING"
            })),
            model_name: Some("legacy-model".to_string()),
            owner_project_id: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn session_agent(config: MemberExecutionConfig) -> ChatSessionAgent {
        ChatSessionAgent {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            agent_id: Uuid::new_v4(),
            state: ChatSessionAgentState::Idle,
            workspace_path: None,
            pty_session_key: None,
            agent_session_id: None,
            agent_message_id: None,
            project_member_id: None,
            member_name: "member".to_string(),
            execution_config: Json(config),
            allowed_skill_ids: Json(Vec::new()),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn uses_legacy_profile_only_when_member_config_is_empty() {
        let resolved =
            resolve_effective_member_execution_config(&agent(), &session_agent(Default::default()))
                .expect("resolve config");

        assert!(!resolved.has_member_config);
        assert_eq!(resolved.runner_type, BaseCodingAgent::Codex);
        assert_eq!(resolved.model_name.as_deref(), Some("legacy-model"));
        assert!(resolved.profile_id.to_string().contains("HIGH_REASONING"));
    }

    #[test]
    fn member_config_disables_legacy_profile_fallback() {
        let resolved = resolve_effective_member_execution_config(
            &agent(),
            &session_agent(MemberExecutionConfig {
                runner_type: Some(BaseCodingAgent::Gemini),
                model_name: Some("gemini-3-pro-preview".to_string()),
                thinking_effort: Some("high".to_string()),
                model_variant: None,
                acp: None,
            }),
        )
        .expect("resolve config");

        assert!(resolved.has_member_config);
        assert_eq!(resolved.runner_type, BaseCodingAgent::Gemini);
        assert_eq!(resolved.model_name.as_deref(), Some("gemini-3-pro-preview"));
        assert_eq!(resolved.thinking_effort.as_deref(), Some("high"));
        assert_eq!(resolved.profile_id.to_string(), "GEMINI");
    }

    #[test]
    fn parses_openteams_cli_runner_aliases() {
        for raw in ["OPEN_TEAMS_CLI", "OPENTEAMS_CLI", "openteams-cli"] {
            assert_eq!(
                parse_runner_type(raw).expect("parse runner"),
                BaseCodingAgent::OpenTeamsCli
            );
        }
    }

    #[test]
    fn member_mcp_settings_form_an_explicit_allowlist() {
        let policy = resolve_acp_mcp_policy(&serde_json::json!({
            "mcpServers": {
                "filesystem": true,
                "browser": {"enabled": true},
                "disabled": false
            }
        }));
        assert_eq!(
            policy.allowed_server_names,
            Some(
                ["browser".to_string(), "filesystem".to_string()]
                    .into_iter()
                    .collect()
            )
        );
        assert!(policy.disabled_server_names.contains("disabled"));
    }

    #[test]
    fn missing_member_mcp_settings_preserve_configured_servers() {
        let policy = resolve_acp_mcp_policy(&serde_json::json!({}));
        assert!(policy.allowed_server_names.is_none());
        assert!(policy.disabled_server_names.is_empty());
    }

    #[test]
    fn explicit_empty_member_mcp_settings_disable_all_servers() {
        let policy = resolve_acp_mcp_policy(&serde_json::json!({
            "mcpServers": {}
        }));
        assert_eq!(policy.allowed_server_names, Some(Default::default()),);
        assert!(policy.disabled_server_names.is_empty());
    }

    #[test]
    fn pi_member_without_mcp_selection_preserves_configured_servers() {
        let mut pi_agent = agent();
        pi_agent.runner_type = "PI".to_string();
        pi_agent.tools_enabled = Json(serde_json::json!({}));
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());

        let (_, executor) = build_effective_member_executor(
            &pi_agent,
            &session_agent(Default::default()),
            &mut env,
        )
        .expect("Pi executor");

        let CodingAgent::Pi(pi) = executor else {
            panic!("expected Pi");
        };
        assert!(pi.acp_mcp_policy.allowed_server_names.is_none());
        assert!(pi.acp_mcp_policy.disabled_server_names.is_empty());
    }

    #[tokio::test]
    async fn different_pi_members_receive_only_their_registry_skill_paths() {
        let temp = tempfile::tempdir().expect("skill registry");
        let root = temp.path().join("skills");
        let alpha = root.join("alpha").join("SKILL.md");
        let beta = root.join("beta").join("SKILL.md");
        tokio::fs::create_dir_all(alpha.parent().expect("alpha parent"))
            .await
            .expect("alpha directory");
        tokio::fs::create_dir_all(beta.parent().expect("beta parent"))
            .await
            .expect("beta directory");
        tokio::fs::write(&alpha, "alpha").await.expect("alpha");
        tokio::fs::write(&beta, "beta").await.expect("beta");
        let roots = canonical_existing_roots(vec![root]).await.expect("roots");
        let alpha_id = Uuid::new_v4().to_string();
        let beta_id = Uuid::new_v4().to_string();
        let installed = vec![
            (alpha_id.clone(), alpha.clone()),
            (beta_id.clone(), beta.clone()),
        ];

        let member_alpha =
            resolve_requested_pi_skill_paths(std::slice::from_ref(&alpha_id), &installed, &roots)
                .await
                .expect("alpha snapshot");
        let member_beta =
            resolve_requested_pi_skill_paths(std::slice::from_ref(&beta_id), &installed, &roots)
                .await
                .expect("beta snapshot");

        assert_eq!(
            member_alpha,
            [tokio::fs::canonicalize(&alpha).await.unwrap()]
        );
        assert_eq!(member_beta, [tokio::fs::canonicalize(&beta).await.unwrap()]);
        assert!(!member_alpha.contains(&member_beta[0]));
        let missing_id = Uuid::new_v4().to_string();
        let error =
            resolve_requested_pi_skill_paths(std::slice::from_ref(&missing_id), &installed, &roots)
                .await
                .expect_err("unauthorized skill must be absent");
        assert!(
            error
                .to_string()
                .contains("missing from the Skill Registry")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pi_skill_snapshot_rejects_symlink_escape() {
        let temp = tempfile::tempdir().expect("skill registry");
        let root = temp.path().join("skills");
        let outside = temp.path().join("outside");
        tokio::fs::create_dir_all(&root).await.expect("root");
        tokio::fs::create_dir_all(&outside).await.expect("outside");
        let outside_skill = outside.join("SKILL.md");
        tokio::fs::write(&outside_skill, "outside")
            .await
            .expect("outside skill");
        std::os::unix::fs::symlink(&outside_skill, root.join("SKILL.md")).expect("skill symlink");
        let roots = canonical_existing_roots(vec![root.clone()])
            .await
            .expect("roots");

        let error = validate_pi_skill_path(&root.join("SKILL.md"), &roots)
            .await
            .expect_err("escape must fail");
        assert!(error.to_string().contains("escapes the Skill Registry"));
    }

    #[test]
    fn acp_runners_default_to_full_access() {
        let profiles = ExecutorConfigs::from_defaults();

        for runner in [
            BaseCodingAgent::Gemini,
            BaseCodingAgent::QwenCode,
            BaseCodingAgent::KimiCode,
            BaseCodingAgent::QoderCli,
            BaseCodingAgent::Hermes,
            BaseCodingAgent::DeepseekHarness,
        ] {
            let executor = profiles.get_coding_agent_or_default(&ExecutorProfileId::new(runner));
            assert!(executor_acp_full_access_enabled(&executor), "{runner}");
        }
    }

    #[test]
    fn deepseek_member_acp_override_uses_executor_capability() {
        let mut deepseek_agent = agent();
        deepseek_agent.runner_type = "DEEPSEEK_HARNESS".to_string();
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        let (_, executor) = build_effective_member_executor(
            &deepseek_agent,
            &session_agent(MemberExecutionConfig {
                runner_type: Some(BaseCodingAgent::DeepseekHarness),
                acp: Some(AcpExecutionOptions {
                    access_mode: Some(AcpAccessMode::WorkspaceOnly),
                    ..AcpExecutionOptions::default()
                }),
                ..MemberExecutionConfig::default()
            }),
            &mut env,
        )
        .expect("DeepSeek Harness executor");

        let CodingAgent::DeepseekHarness(harness) = executor else {
            panic!("expected DeepSeek Harness");
        };
        assert_eq!(
            harness.acp.and_then(|options| options.access_mode),
            Some(AcpAccessMode::WorkspaceOnly)
        );
    }
}
