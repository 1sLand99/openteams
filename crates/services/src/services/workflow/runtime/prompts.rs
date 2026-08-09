/// Resolves the effective lead agent for a session.
/// Returns (lead_agent, lead_session_agent) or error if no agents exist.
///
/// Resolution logic:
/// 1. If `session.lead_session_agent_id` references a valid session member, use it.
/// 2. During the compatibility window, fall back to `lead_agent_id`.
/// 3. Otherwise, fall back to the first session agent.
pub fn resolve_lead_agent<'a>(
    session: &ChatSession,
    session_agents: &'a [ChatSessionAgent],
    agents: &'a [ChatAgent],
) -> Result<(&'a ChatAgent, &'a ChatSessionAgent), WorkflowRuntimeError> {
    let agent_for_member = |session_agent: &ChatSessionAgent| {
        session_agents
            .iter()
            .position(|candidate| candidate.id == session_agent.id)
            .and_then(|index| agents.get(index))
            .filter(|agent| agent.id == session_agent.agent_id)
            .or_else(|| agents.iter().find(|agent| agent.id == session_agent.agent_id))
    };
    if let Some(lead_session_agent_id) = session.lead_session_agent_id
        && let Some(sa) = session_agents
            .iter()
            .find(|sa| sa.id == lead_session_agent_id)
        && let Some(agent) = agent_for_member(sa)
    {
        return Ok((agent, sa));
    }
    // Compatibility fallback for sessions not yet migrated.
    if let Some(lead_id) = session.lead_agent_id {
        let mut matching_members = session_agents
            .iter()
            .filter(|session_agent| session_agent.agent_id == lead_id);
        if let Some(session_agent) = matching_members.next()
            && matching_members.next().is_none()
            && let Some(agent) = agent_for_member(session_agent)
        {
            return Ok((agent, session_agent));
        }
    }
    // 2. Fallback to first session agent
    let first_sa = session_agents
        .first()
        .ok_or_else(|| WorkflowRuntimeError::Validation("No agents in session".into()))?;
    let agent = agent_for_member(first_sa)
        .ok_or_else(|| WorkflowRuntimeError::Validation("Lead agent record not found".into()))?;
    Ok((agent, first_sa))
}

pub fn resolve_workflow_goal(
    explicit_goal: Option<&str>,
    messages: &[ChatMessage],
) -> Option<String> {
    if let Some(goal) = explicit_goal.map(str::trim).filter(|goal| !goal.is_empty()) {
        return Some(goal.to_string());
    }

    messages
        .iter()
        .rev()
        .find(|message| message.sender_type == ChatSenderType::User)
        .map(|message| message.content.trim())
        .filter(|goal| !goal.is_empty())
        .map(ToOwned::to_owned)
}

fn workflow_response_language_instruction_from_value(value: &str) -> Option<&'static str> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    if normalized.starts_with("zh-hant")
        || normalized.starts_with("zh-tw")
        || normalized.starts_with("zh-hk")
        || normalized.starts_with("zh-mo")
    {
        return Some("You MUST write human-readable JSON string values in Traditional Chinese.");
    }
    if normalized.starts_with("zh")
        || normalized.starts_with("zh-hans")
        || normalized.starts_with("zh-cn")
    {
        return Some("You MUST write human-readable JSON string values in Simplified Chinese.");
    }
    if normalized.starts_with("ja") {
        return Some("You MUST write human-readable JSON string values in Japanese.");
    }
    if normalized.starts_with("ko") {
        return Some("You MUST write human-readable JSON string values in Korean.");
    }
    if normalized.starts_with("fr") {
        return Some("You MUST write human-readable JSON string values in French.");
    }
    if normalized.starts_with("es") {
        return Some("You MUST write human-readable JSON string values in Spanish.");
    }
    if normalized.starts_with("en") {
        return Some("You MUST write human-readable JSON string values in English.");
    }
    None
}

pub fn resolve_workflow_response_language_instruction(
    configured_language: &UiLanguage,
) -> &'static str {
    match configured_language {
        UiLanguage::Browser => sys_locale::get_locale()
            .as_deref()
            .and_then(workflow_response_language_instruction_from_value)
            .unwrap_or("You MUST write human-readable JSON string values in English."),
        UiLanguage::En => "You MUST write human-readable JSON string values in English.",
        UiLanguage::ZhHans => {
            "You MUST write human-readable JSON string values in Simplified Chinese."
        }
        UiLanguage::ZhHant => {
            "You MUST write human-readable JSON string values in Traditional Chinese."
        }
        UiLanguage::Ja => "You MUST write human-readable JSON string values in Japanese.",
        UiLanguage::Ko => "You MUST write human-readable JSON string values in Korean.",
        UiLanguage::Fr => "You MUST write human-readable JSON string values in French.",
        UiLanguage::Es => "You MUST write human-readable JSON string values in Spanish.",
    }
}

/// Extract the enabled tool entries from an agent's `tools_enabled` config.
/// Enabled MCP servers are reported as `mcp:<name>`; other boolean-true flags
/// are reported by key.
fn extract_enabled_tools(tools_enabled: &serde_json::Value) -> Vec<String> {
    let mut tools = Vec::new();
    if let Some(servers) = tools_enabled
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
    {
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
                tools.push(format!("mcp:{name}"));
            }
        }
    }
    if let Some(entries) = tools_enabled.as_object() {
        for (key, value) in entries {
            if key == "mcpServers" {
                continue;
            }
            if value.as_bool() == Some(true) {
                tools.push(key.clone());
            }
        }
    }
    tools.sort();
    tools.dedup();
    tools
}

/// Maximum length (in chars) of the capability profile embedded in a
/// planning agent descriptor.
pub(crate) const CAPABILITY_PROFILE_MAX_CHARS: usize = 600;

/// Builds a whitespace-normalized capability profile from the underlying
/// agent's system prompt, capped at `CAPABILITY_PROFILE_MAX_CHARS` including
/// the trailing ellipsis. Returns `None` when the prompt is blank.
pub(crate) fn capability_profile_from_system_prompt(system_prompt: &str) -> Option<String> {
    let normalized = system_prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    if normalized.chars().count() <= CAPABILITY_PROFILE_MAX_CHARS {
        return Some(normalized);
    }
    // Reserve one char for the ellipsis so the total stays within the cap.
    let truncated: String = normalized
        .chars()
        .take(CAPABILITY_PROFILE_MAX_CHARS - 1)
        .collect();
    Some(format!("{}…", truncated.trim_end()))
}

/// Composes one planning agent descriptor from already-resolved facts.
///
/// `effective` must come from `resolve_effective_member_execution_config` so
/// runner/model match the real execution semantics; `enabled_runner_skills`
/// lists `(skill_id, skill_name)` pairs enabled for the effective runner and
/// is intersected with the member's `allowed_skill_ids`.
pub(crate) fn compose_workflow_planning_agent(
    session_agent: &ChatSessionAgent,
    agent: &ChatAgent,
    effective: &crate::services::member_execution::EffectiveMemberExecutionConfig,
    is_lead: bool,
    member_role: Option<String>,
    enabled_runner_skills: &[(String, String)],
) -> WorkflowPlanningAgent {
    use crate::services::workflow::workflow_orchestrator::workflow_plan_agent_id;

    let allowed_skill_ids: HashSet<&str> = session_agent
        .allowed_skill_ids
        .0
        .iter()
        .map(|skill_id| skill_id.trim())
        .filter(|skill_id| !skill_id.is_empty())
        .collect();
    let mut skills: Vec<String> = enabled_runner_skills
        .iter()
        .filter(|(skill_id, _)| allowed_skill_ids.contains(skill_id.as_str()))
        .map(|(_, name)| name.clone())
        .collect();
    skills.sort();

    let (workflow_role, responsibilities) = if is_lead {
        (
            "lead",
            "Owns the workflow plan, reviews worker outputs, and produces the final result. Plan generation and final acceptance support stay with the lead; do not assign them to workers.",
        )
    } else {
        (
            "worker",
            "Executes only the steps assigned via nodes[].data.agentId. Must not redefine the plan, reassign work, or take over lead duties.",
        )
    };

    WorkflowPlanningAgent {
        agent_id: workflow_plan_agent_id(session_agent),
        session_agent_id: session_agent.id.to_string(),
        underlying_agent_id: session_agent.agent_id.to_string(),
        name: session_agent.member_name.clone(),
        workflow_role: workflow_role.to_string(),
        member_role,
        runner_type: effective.runner_type.to_string(),
        model_name: effective.model_name.clone(),
        tools_enabled: extract_enabled_tools(&agent.tools_enabled.0),
        skills,
        capability_profile: capability_profile_from_system_prompt(&agent.system_prompt),
        responsibilities: responsibilities.to_string(),
    }
}

/// Resolves the declared professional role (`ProjectMember.role`) for each
/// session member. Returns a map keyed by session agent id.
///
/// Resolution rules (never "first match"):
/// - An explicit `project_member_id` link is honored only when the member
///   belongs to the session's project, is an agent member, and matches the
///   session member's underlying agent. An invalid link yields no role.
/// - Without a link, the role is taken from the project roster only when
///   exactly one agent member matches the underlying agent; zero or multiple
///   matches yield no role, so members sharing one underlying agent can never
///   inherit the same role by accident.
async fn resolve_planning_member_roles(
    pool: &SqlitePool,
    session: &ChatSession,
    session_agents: &[ChatSessionAgent],
) -> Result<HashMap<Uuid, String>, WorkflowRuntimeError> {
    use db::models::project_member::{ProjectMember, ProjectMemberType};

    fn usable_role(member: Option<ProjectMember>) -> Option<String> {
        member
            .and_then(|member| member.role)
            .map(|role| role.trim().to_string())
            .filter(|role| !role.is_empty())
    }

    let mut roles = HashMap::new();
    let mut project_members: Option<Vec<ProjectMember>> = None;
    for session_agent in session_agents {
        if let Some(project_member_id) = session_agent.project_member_id {
            let member = ProjectMember::find_by_id(pool, project_member_id).await?;
            let link_valid = member.as_ref().is_some_and(|member| {
                member.member_type == ProjectMemberType::Agent
                    && member.agent_id == Some(session_agent.agent_id)
                    && session.project_id == Some(member.project_id)
            });
            if link_valid {
                if let Some(role) = usable_role(member) {
                    roles.insert(session_agent.id, role);
                }
            } else {
                tracing::warn!(
                    session_agent_id = %session_agent.id,
                    project_member_id = %project_member_id,
                    "[plan_generation] ignoring project member role link: project, type, or agent mismatch"
                );
            }
            continue;
        }

        let Some(project_id) = session.project_id else {
            continue;
        };
        if project_members.is_none() {
            project_members = Some(ProjectMember::find_by_project(pool, project_id).await?);
        }
        let matches: Vec<&ProjectMember> = project_members
            .as_ref()
            .map(|members| {
                members
                    .iter()
                    .filter(|member| {
                        member.member_type == ProjectMemberType::Agent
                            && member.agent_id == Some(session_agent.agent_id)
                    })
                    .collect()
            })
            .unwrap_or_default();
        match matches.as_slice() {
            [only] => {
                if let Some(role) = usable_role(Some((*only).clone())) {
                    roles.insert(session_agent.id, role);
                }
            }
            [] => {}
            _ => {
                tracing::warn!(
                    session_agent_id = %session_agent.id,
                    agent_id = %session_agent.agent_id,
                    match_count = matches.len(),
                    "[plan_generation] ambiguous project member role: multiple project members share the underlying agent; leaving role unset"
                );
            }
        }
    }
    Ok(roles)
}

/// Builds plan-generation agent descriptors for every session member.
///
/// Capabilities are taken from the real configuration: the effective
/// execution config from `resolve_effective_member_execution_config`
/// (member runner/model overrides applied), `ChatAgent.tools_enabled` and
/// system prompt, `ProjectMember.role`, and the native skills enabled for the
/// effective runner intersected with `allowed_skill_ids` — never inferred
/// from member names. Resolution failures are returned as errors instead of
/// being silently disguised as empty capability lists.
pub async fn build_workflow_planning_agents(
    pool: &SqlitePool,
    session: &ChatSession,
    session_agents: &[ChatSessionAgent],
    agents: &[ChatAgent],
    lead_session_agent_id: Uuid,
) -> Result<Vec<WorkflowPlanningAgent>, WorkflowRuntimeError> {
    use crate::services::{
        member_execution::resolve_effective_member_execution_config,
        native_skills::list_native_skills_for_runner,
    };

    let member_roles = resolve_planning_member_roles(pool, session, session_agents).await?;
    // effective runner (Display string) -> [(skill_id, skill_name)] enabled
    let mut enabled_skills_by_runner: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let mut planning_agents = Vec::with_capacity(session_agents.len());

    for session_agent in session_agents {
        let agent = agents
            .iter()
            .find(|agent| agent.id == session_agent.agent_id)
            .ok_or_else(|| {
                WorkflowRuntimeError::PlanningAgentResolution(format!(
                    "agent {} for session member '{}' not found",
                    session_agent.agent_id, session_agent.member_name
                ))
            })?;
        let effective = resolve_effective_member_execution_config(agent, session_agent).map_err(
            |err| {
                WorkflowRuntimeError::PlanningAgentResolution(format!(
                    "member '{}': {err}",
                    session_agent.member_name
                ))
            },
        )?;
        let runner_key = effective.runner_type.to_string();
        if !enabled_skills_by_runner.contains_key(&runner_key) {
            let enabled = list_native_skills_for_runner(pool, effective.runner_type)
                .await
                .map_err(|err| {
                    WorkflowRuntimeError::PlanningAgentResolution(format!(
                        "member '{}': failed to resolve native skills for runner {runner_key}: {err}",
                        session_agent.member_name
                    ))
                })?
                .into_iter()
                .filter(|item| item.enabled)
                .map(|item| (item.skill.id.to_string(), item.skill.name))
                .collect::<Vec<_>>();
            enabled_skills_by_runner.insert(runner_key.clone(), enabled);
        }
        let enabled_runner_skills = enabled_skills_by_runner
            .get(&runner_key)
            .map(Vec::as_slice)
            .unwrap_or(&[]);

        planning_agents.push(compose_workflow_planning_agent(
            session_agent,
            agent,
            &effective,
            session_agent.id == lead_session_agent_id,
            member_roles.get(&session_agent.id).cloned(),
            enabled_runner_skills,
        ));
    }

    Ok(planning_agents)
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowStepExecutionContract {
    pub acceptance_leveled: Vec<(AcceptanceCriterionLevel, String)>,
    pub expected_outputs: Vec<String>,
    pub self_check: Vec<String>,
    pub verification_commands: Vec<String>,
    pub completion_evidence: Vec<String>,
}

pub fn workflow_review_attempt_limit_reached(review_attempt: i32, max_attempts: i32) -> bool {
    review_attempt >= max_attempts
}

pub(crate) fn resolve_workspace_path_snapshot(
    session: &ChatSession,
    agent: &ChatAgent,
    session_agent: &ChatSessionAgent,
) -> PathBuf {
    if let Some(path) = session_agent.workspace_path.as_deref() {
        PathBuf::from(path)
    } else if let Some(path) = session.default_workspace_path.as_deref() {
        PathBuf::from(path)
    } else {
        PathBuf::from("assets")
            .join("chat")
            .join(format!("session_{}", session.id))
            .join("agents")
            .join(agent.id.to_string())
    }
}

pub(crate) async fn resolve_workspace_path(
    db: &DBService,
    session: &ChatSession,
    agent: &ChatAgent,
    session_agent: &ChatSessionAgent,
) -> Result<PathBuf, WorkflowRuntimeError> {
    if session.worktree_mode == ChatSessionWorktreeMode::Isolated {
        let worktree_service = SessionWorktreeService::new(db.pool.clone());
        if let Some(worktree) = worktree_service.get_latest_for_session(session.id).await? {
            if worktree.status.is_active_for_workspace() {
                return Ok(PathBuf::from(worktree.worktree_path));
            }
            return Ok(PathBuf::from(worktree.base_workspace_path));
        }

        if let Some(default_workspace) = session.default_workspace_path.as_ref() {
            let input = EnsureWorktreeInput::new(session.id, default_workspace.into())
                .with_project(session.project_id);
            let outcome = worktree_service.ensure_for_session(input).await?;
            let worktree = match outcome {
                EnsureOutcome::Created(w) => w,
                EnsureOutcome::Existing(w) => w,
            };
            return Ok(PathBuf::from(worktree.worktree_path));
        }
    }

    Ok(resolve_workspace_path_snapshot(session, agent, session_agent))
}
