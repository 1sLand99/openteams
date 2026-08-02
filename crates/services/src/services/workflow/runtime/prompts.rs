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

/// WorkflowPlanJson schema shared by initial and iteration plan generation
/// prompts. Keep a single definition so the two entries cannot drift.
pub(crate) static PLAN_SCHEMA_DEFINITION: &str = r#"{
  "version": "1",
  "title": "string",
  "goal": "string",
  "agents": {
    "lead": "string",
    "available": ["string"]
  },
  "globals": {
    "interrupt_mode": "cooperative",
    "default_retry": 1,
    "global_pause_supported": true
  },
  "nodes": [
    {
      "id": "unique_step_key",
      "type": "workflowStep",
      "data": {
        "stepType": "task | review | result",
        "agentId": "optional string",
        "title": "string",
        "instructions": "string",
        "acceptance": ["string, required non-empty for task nodes"],
        "outputs": ["string, required non-empty for task nodes"],
        "checklist": ["string, required non-empty for task nodes"],
        "verificationCommands": ["string, required non-empty for task nodes"],
        "completionEvidence": ["string, required non-empty for task nodes"],
        "interruptible": true,
        "maxRetry": 1,
        "status": "optional string",
        "reviewScope": ["optional node_id list, review nodes only"]
      }
    }
  ],
  "edges": [
    {
      "id": "unique_edge_id",
      "source": "node_id",
      "target": "node_id",
      "type": "optional string",
      "data": {
        "kind": "hard"
      }
    }
  ]
}"#;

/// Stable output contract shared by initial and iteration plan generation.
pub(crate) static PLAN_STABLE_OUTPUT_CONTRACT: &str = r#"## Stable Output Contract

Return exactly one workflow plan JSON object.

Hard requirements:
1. Top-level structure must match the WorkflowPlanJson schema and include at least `version`, `title`, `goal`, `agents`, `nodes`, and `edges`.
2. `version` must be the string `"1"`.
3. Every `nodes[].type` must be `"workflowStep"`.
4. `nodes[].data.stepType` may only be `"task"`, `"review"`, or `"result"`.
5. There must be exactly one `result` node, and that result node must have no outgoing edges.
6. All node ids, edge ids, and step keys must be unique.
7. The graph must be a directed acyclic graph. Dependencies must be represented only through `edges`.
8. `agents.lead`, `agents.available`, and `nodes[].data.agentId` may only use the `agent_id` values from the provided Available agents JSON.
9. Leave `nodes[].data.agentId` empty or omit it only when a step does not need a specific agent. Never invent agent ids.
10. Node `title` and `instructions` must be concrete, actionable, and specific enough for an agent to execute.
11. Prefer the smallest executable closed loop that can satisfy the goal. Avoid unnecessary step expansion.
12. A `review` node without a non-empty `reviewScope` is one independent review step. It does not create a structured rejection-to-rework loop.
13. Only a review node with a non-empty `reviewScope` creates a retry loop. `reviewScope` is the list of **task** node ids to re-run on rejection. All listed tasks must be upstream predecessors; include any intermediate tasks between a scoped task and the review. Each task may appear in at most one `reviewScope`. Never include result/review/unknown ids or downstream nodes.
14. Do not output or infer `leadReview` or `userReview`. The system writes those fields from frontend card selections.
15. Retry budgets are controlled by `globals.default_retry` and optional node `maxRetry`. Both must be integers from 0 through 10. `maxRetry` overrides the global value for that node. A retry budget counts rework after the initial execution/review: `0` means one initial attempt and no rework. For a loop review node, this gives one initial review plus at most `maxRetry` rework attempts.
16. Every edge must use `data.kind: "hard"` or omit `data`; soft dependencies are not supported by the scheduler.
17. Do not output top-level `policies` or `loops`; they are legacy compatibility fields with no runtime consumer. Your output is validated, compiled, and may start execution directly.
18. Every `task` node MUST define a verifiable contract in `nodes[].data`: non-empty `acceptance` (acceptance criteria), `outputs` (expected deliverable paths), `checklist` (verifiable work items), `verificationCommands` (commands or methods that prove the work, e.g. test/build commands), and `completionEvidence` (evidence the executor must produce, e.g. test output summaries). `review` and `result` nodes are exempt from these field requirements.

"#;

/// Static constraints shared by initial and iteration plan generation.
pub(crate) static PLAN_STATIC_CONSTRAINTS: &str = r#"## Additional Static Constraints

- `version` must be string `"1"`.
- `agents.available` and `nodes[].data.agentId` may only use the `agent_id` values from the provided Available agents JSON.
- `globals` and optional node/edge fields may be omitted when unnecessary. Omitted retry values inherit `globals.default_retry`, which defaults to 1.
- Do not emit top-level `policies` or `loops`.
- Edge dependency kind is hard-only; omit `data` or use `{ "kind": "hard" }`.
- Required `task` contract fields may not be omitted.
- `reviewScope` rules: task-only ids, upstream predecessors only, include intermediates, each task in at most one scope, no result/review/unknown/downstream ids. If two loops need similar work, split into separate tasks or keep shared setup outside `reviewScope`.
- when multiple agents need to edit the same file or directory in parallel, use git worktree for isolation and merge changes back to the mainline afterward. If Git is not available, use alternative isolation methods.

"#;

/// Dynamic-skills guidance shared by initial and iteration plan generation.
/// Skills are never hardcoded here; only the per-member resolved skills listed
/// in the Available agents JSON may be referenced.
pub(crate) static PLAN_SKILLS_GUIDANCE: &str = r#"## Agent Skills

- Each entry in the Available agents JSON lists the skills actually enabled for that session member in its `skills` field, along with its effective runner, model, tools, and responsibility boundary.
- Each entry's `member_role` and `capability_profile` describe the member's declared expertise (sourced from its linked project member role and the agent system prompt); use them — never the member name — when deciding which member fits a step.
- When a task benefits from a skill, assign the step to a member whose `skills` include it and name that skill explicitly in the step instructions. Never reference or recommend skills that are not listed for the assigned member.
- In case of any discrepancy with a skill's format, the specified JSON schema shall prevail.
- Store the generated plan details in the nodes[].data.instructions field of the workflow plan JSON, using Markdown format.

"#;

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

/// Appends the shared lead/available-agents dynamic section to a plan
/// generation prompt.
pub(crate) fn push_plan_agent_context(
    prompt: &mut String,
    lead_agent_id: &str,
    available_agents: &[WorkflowPlanningAgent],
) {
    let available_agents_json =
        serde_json::to_string_pretty(available_agents).unwrap_or_else(|_| "[]".to_string());
    prompt.push_str("Lead agent id:\n");
    prompt.push_str(lead_agent_id);
    prompt.push_str("\n\nAvailable agents JSON:\n");
    prompt.push_str(&available_agents_json);
}

pub fn build_plan_generation_prompt(
    plan_goal: &str,
    lead_agent_id: &str,
    available_agents: &[WorkflowPlanningAgent],
    previous_failure_reason: Option<&str>,
    previous_plan_json: Option<&str>,
    response_language_instruction: &str,
    design_doc_paths: Option<&[String]>,
) -> String {
    let available_agents_json =
        serde_json::to_string_pretty(available_agents).unwrap_or_else(|_| "[]".to_string());
    let mut prompt = String::new();
    prompt.push_str(
        r#"# Workflow Plan Generation

You are generating an executable workflow plan from a confirmed implementation brief.
The output source of truth is React Flow compatible workflow JSON. Do not output Markdown, YAML, comments, explanations, or prose outside the JSON object.

"#,
    );
    prompt.push_str(PLAN_STABLE_OUTPUT_CONTRACT);
    prompt.push_str("## WorkflowPlanJson Schema Reference\n\n");
    prompt.push_str(PLAN_SCHEMA_DEFINITION);
    prompt.push_str("\n\n");
    prompt.push_str(PLAN_STATIC_CONSTRAINTS);
    prompt.push_str(PLAN_SKILLS_GUIDANCE);
    prompt.push_str("## Dynamic Inputs\n\n");

    let prev_failure = previous_failure_reason
        .map(str::trim)
        .filter(|r| !r.is_empty());
    let prev_plan = previous_plan_json
        .map(str::trim)
        .filter(|p| !p.is_empty());

    let doc_paths_text = design_doc_paths
        .filter(|paths| !paths.is_empty())
        .map(|paths| {
            paths
                .iter()
                .map(|p| format!("- {}", p.trim()))
                .collect::<Vec<_>>()
                .join("\n")
        });

    let mut builder = PromptDataBuilder::new(MAX_DYNAMIC_CONTENT_BUDGET_BYTES)
        .add("plan_goal", plan_goal.trim(), 2)
        .add("available_agents_json", &available_agents_json, 1);
    builder = builder.add_optional("previous_failure_reason", prev_failure, 1);
    builder = builder.add_optional("previous_plan_json", prev_plan, 3);
    builder = builder.add_optional("design_doc_paths", doc_paths_text.as_deref(), 1);
    let data = builder.build();

    if !data.get("previous_failure_reason").is_empty() {
        prompt.push_str("Previous generation failed. Regenerate the workflow plan.\n");
        prompt.push_str(data.get("previous_failure_reason"));
        prompt.push_str(
            "\n\nFix the error above in this regeneration request. Do not repeat the same failure.\n\n",
        );
    }
    prompt.push_str("Response language requirement:\n");
    prompt.push_str(response_language_instruction.trim());
    prompt.push_str("\n\nPlan goal brief:\n");
    prompt.push_str(data.get("plan_goal"));
    if !data.get("previous_plan_json").is_empty() {
        prompt.push_str(data.get("previous_plan_json"));
        prompt.push_str(
            "\nUse this existing plan as the baseline. Apply the requested changes from the plan goal brief, preserve correct unchanged work, and return the complete revised workflow plan JSON.",
        );
    }
    prompt.push_str("\n\nLead agent id:\n");
    prompt.push_str(lead_agent_id);
    prompt.push_str("\n\nAvailable agents JSON:\n");
    prompt.push_str(data.get("available_agents_json"));
    if !data.get("design_doc_paths").is_empty() {
        prompt.push_str("\n\nDesign document paths:\n");
        prompt.push_str(data.get("design_doc_paths"));
        prompt.push_str(
            "\nMUST read these design documents for full context when generating the plan.",
        );
    }
    prompt.push_str("\n\nFinal instruction: return the workflow plan JSON object only.");
    prompt = maybe_prepend_safety_preamble(&prompt);
    prompt
}

static STEP_EXECUTION_PROMPT_PREFIX: &str = r#"## Output Format

Return exactly one JSON object — no Markdown, no comments, no prose outside the JSON.

### final_result
```json
{"type": "final_result", "step_key": "...", "execution_id": "...", "summary": "one-line summary", "content": "full result", "outputs": ["relative/path"]}
```

### error
```json
{"type": "error", "step_key": "...", "execution_id": "...", "message": "failure reason", "content": "optional detail"}
```

### approval_request
```json
{"type": "approval_request", "step_key": "...", "execution_id": "...", "title": "needs user approval", "description": "optional detail"}
```

### permission_request
```json
{"type": "permission_request", "step_key": "...", "execution_id": "...", "title": "needs user authorization", "description": "optional detail"}
```

### continue_confirmation
```json
{"type": "continue_confirmation", "step_key": "...", "execution_id": "...", "message": "confirm to continue", "description": "optional detail"}
```

### input_request
```json
{"type": "input_request", "step_key": "...", "execution_id": "...", "prompt": "what you need from user", "description": "optional detail", "placeholder": "placeholder text"}
```

### Constraints
1. `step_key` and `execution_id` must be filled with the values provided below.
2. Task steps use `final_result`; Review steps MUST use `review_result`; Result steps MUST use `result_review_result`. `error`, `approval_request`, `permission_request`, `continue_confirmation`, and `input_request` remain available when applicable.
3. `outputs` contains workspace-relative paths only.
4. Use interactive requests sparingly — only when genuinely blocked without user action.
5. Follow existing codebase patterns. Improve code you touch, but do not restructure outside your task.
6. If a file grows beyond the plan's intent, report DONE_WITH_CONCERNS rather than splitting on your own.
7. Stop and report BLOCKED or NEEDS_CONTEXT when: multiple valid architectures exist, you cannot gain clarity after reading files, or the plan did not anticipate the restructuring needed.
8. Self-review before reporting: check completeness, naming clarity, YAGNI, and test quality. Fix issues before submitting.
9. Always include test files in `outputs` alongside implementation files.

## Language Requirement
You MUST respond in the same language as the Instructions field below.
The `summary`, `content`, and `message` fields in your JSON output must use the same language as the step instructions.

"#;

static STEP_EXECUTION_CODE_GUIDELINES_PROMPT: &str = r#"## Coding Task Skill Requirement

If this task involves writing, modifying, reviewing, or refactoring code, you MUST use the `code-guidelines` skill before editing code.

"#;

// static STEP_EXECUTION_TDD_WORKFLOW_FOR_TASK_TYPE: &str = r#"

// ### TDD Workflow

// If it is a coding task, follow Test-Driven Development for every implementation step:
// 1. **Red** — Write failing tests first that define the expected behavior. Run them to confirm they fail.
// 2. **Green** — Write the minimum implementation to make all tests pass. No extra features.
// 3. **Refactor** — Clean up code while keeping tests green. Improve naming, remove duplication, simplify logic.
// 4. If no test framework exists in the project, create minimal verification scripts that assert expected behavior before implementing.

// For non-coding tasks, it's not necessary to strictly follow the TDD pattern.
// "#;

static STEP_EXECUTION_TDD_WORKFLOW_FOR_REVIEW_TYPE: &str = r#"

## Review Discipline

Verify the worker's output independently; do not rely on their report.

Check:
- Read changed files from `outputs` and compare them with instructions and acceptance criteria.
- Reject missing requirements, unrequested scope, obvious bugs, edge-case gaps, or broken shared contracts.
- Ensure the result fits the workflow goal and predecessor outputs.

Workflow review is capped at five attempts. Complete the entire review now. If
rejecting, cite every issue you can identify in this single response, with
file/line evidence and concrete revision guidance when available. Do not hold
back, defer, or drip-feed issues into later review attempts.
"#;

static STEP_EXECUTION_RESULT_REVIEW_WORKFLOW: &str = r#"

## Final Workflow Result Review Discipline

You are responsible for the final review of the entire workflow plan, not only
the current result step.

Follow this review method in order:
1. Reconstruct the workflow goal, this result step's instructions, and every
   predecessor summary before writing the final result.
2. Check each task, review, and retry loop as part of one plan. Treat rejected
   or superseded attempts as history only; use the latest accepted/completed
   round as the source of truth.
3. Verify that every required workflow output is present, consistent with the
   plan goal, and supported by the predecessor work and review evidence.
4. Validate integration across steps: no missing handoff, conflicting result,
   stale assumption, unreviewed rejection, or incomplete retry may be hidden in
   the final result.
5. If any required step is missing, blocked, failed, rejected without a
   successful retry, or not supported by evidence, report BLOCKED or
   DONE_WITH_CONCERNS instead of DONE.
6. Produce a concise final result that explains what was completed, what was
   verified, what deliverables exist, and any remaining risks or follow-up work.

Do not invent evidence. If predecessor summaries are insufficient, say exactly
what is missing and how it affects the final workflow result.
"#;

pub fn build_step_execution_prompt(
    execution: &WorkflowExecution,
    workflow_goal: &str,
    step: &WorkflowStep,
    completed_dependency_summaries: &[String],
    _step_transcript_context: Option<&str>,
) -> String {
    let dependency_text = if completed_dependency_summaries.is_empty() {
        "None".to_string()
    } else {
        completed_dependency_summaries.join("\n\n")
    };

    let data = PromptDataBuilder::new(MAX_DYNAMIC_CONTENT_BUDGET_BYTES)
        .add("step_title", &step.title, 1)
        .add("step_instructions", &step.instructions, 2)
        .add("workflow_goal", workflow_goal, 1)
        .add("predecessor_summaries", &dependency_text, 1)
        .build();

    let mut prompt = String::with_capacity(4096);
    if step.step_type == WorkflowStepType::Task {
        prompt.push_str("You are implementing a task in an workflow step.\n\n");
        prompt.push_str(STEP_EXECUTION_CODE_GUIDELINES_PROMPT);
    } else if step.step_type == WorkflowStepType::Review {
        prompt.push_str("You are reviewing the output of the workers' implementation.\n\n");
    } else if step.step_type == WorkflowStepType::Result {
        prompt.push_str("You are reviewing the results of the current workflow execution.\n\n");
    }

    if step.step_type == WorkflowStepType::Review {
        prompt.push_str(STEP_EXECUTION_TDD_WORKFLOW_FOR_REVIEW_TYPE);
        prompt.push_str(
            "\n## Structured Review Response\nReturn `review_result`, not `final_result`. Include a verdict, a result for every acceptance criterion, evidence from actual artifacts or checks, risks, and unfinished items.\n",
        );
    } else if step.step_type == WorkflowStepType::Result {
        prompt.push_str(STEP_EXECUTION_RESULT_REVIEW_WORKFLOW);
        prompt.push_str(
            "\n## Structured Final Result Response\nReturn `result_review_result`, not `final_result`. `overall_status` must be `completed`, `completed_with_concerns`, or `blocked`; include every acceptance conclusion, evidence, risks, and unfinished items.\n",
        );
    }

    prompt.push_str(STEP_EXECUTION_PROMPT_PREFIX);

    prompt.push_str(&format!(
        r#"## Task Description

Step: {step_title}
Type: {step_type}
{instructions}
## Context

{goal}
{deps}
## Report

Return one JSON object. Fill `step_key` with `{step_key}`, `execution_id` with `{execution_id}`.
Status: DONE | DONE_WITH_CONCERNS | BLOCKED | NEEDS_CONTEXT.
Report must include: what tests were written first, what was implemented, test results (pass/fail), files changed, self-review findings, issues.
"#,
        step_key = step.step_key,
        execution_id = execution.id,
        step_type = to_workflow_wire_value(&step.step_type),
        step_title = data.get("step_title"),
        instructions = data.get("step_instructions"),
        goal = data.get("workflow_goal"),
        deps = data.get("predecessor_summaries"),
    ));
    prompt = maybe_prepend_safety_preamble(&prompt);
    prompt
}

pub fn build_step_execution_prompt_with_schema(
    execution: &WorkflowExecution,
    workflow_goal: &str,
    step: &WorkflowStep,
    completed_dependency_summaries: &[String],
    step_transcript_context: Option<&str>,
    agent_skill_names: &[String],
) -> String {
    let mut prompt = build_step_execution_prompt(
        execution,
        workflow_goal,
        step,
        completed_dependency_summaries,
        step_transcript_context,
    );
    if let Some(section) =
        crate::services::agent_skill_policy::format_skills_prompt_section(agent_skill_names)
    {
        prompt.push_str(&section);
    }
    prompt.push_str("\n\nRequired JSON Schema:\n```json\n");
    prompt.push_str(&workflow_step_protocol_json_schema_for_step(
        execution.id,
        &step.step_key,
        true,
        &step.step_type,
    ));
    prompt.push_str("\n```\n");
    prompt.push_str("Return ONLY one JSON object matching this schema.\n");
    prompt
}

static LEAD_REVIEW_PROMPT_PREFIX: &str = r#"You are reviewing a worker's step task output.

## CRITICAL: Do Not Trust the Report

The worker's report may be incomplete, inaccurate, or optimistic. You MUST verify
everything independently by reading the actual code and output.

**DO NOT:**
- Take their word for what they implemented
- Trust their claims about completeness or test results
- Accept their interpretation of requirements without checking

**DO:**
- Read the actual code they wrote (use outputs file list to locate files)
- Compare actual implementation to step instructions line by line
- Check for missing pieces they claimed to implement
- Look for extra features they didn't mention (YAGNI violations)
- Run or inspect tests to confirm they actually pass

## Review Dimensions

**Missing requirements:**
- Did they implement everything the step instructions requested?
- Are there acceptance criteria they skipped or missed?
- Did they claim something works but didn't actually implement it?

**Extra/unneeded work:**
- Did they build things that weren't requested?
- Did they over-engineer or add unnecessary features?
- Did they add "nice to haves" that weren't in spec?

**Correctness:**
- Does the implementation correctly solve the stated problem?
- Are there obvious bugs, edge cases, or error handling gaps?
- Does it follow existing codebase patterns and conventions?

**Test quality:**
- Do tests verify real behavior (not just mock behavior)?
- Are test cases comprehensive for the scope of changes?

**Consistency:**
- Is the result consistent with the overall workflow goal?
- Does it integrate properly with predecessor step outputs?

## Output Format

Return exactly one JSON object — no Markdown, no comments, no prose outside the JSON.

Approved:
```json
{"type": "review_result", "step_key": "...", "execution_id": "...", "verdict": "approved", "feedback": "brief approval note", "acceptance_results": [{"criterion": "criterion", "verdict": "passed", "evidence": "file:line or test output"}], "evidence": ["independent verification evidence"], "risks": [], "unfinished_items": []}
```

Rejected:
```json
{"type": "review_result", "step_key": "...", "execution_id": "...", "verdict": "rejected", "feedback": "specific issues: missing X, extra Y at file:line, wrong Z", "acceptance_results": [{"criterion": "criterion", "verdict": "failed", "evidence": "file:line or failed test output"}], "evidence": ["independent verification evidence"], "risks": ["risk"], "unfinished_items": ["missing work"]}
```

## Language Requirement
You MUST respond in the same language as the step Instructions below.
The `feedback` field in your JSON output must use the same language as the step instructions.
"#;

pub const MAX_WORKFLOW_REVIEW_ATTEMPTS: i32 = 5;

pub fn workflow_review_attempt_limit_reached(review_attempt: i32) -> bool {
    review_attempt >= MAX_WORKFLOW_REVIEW_ATTEMPTS
}

pub fn build_lead_review_prompt(
    workflow_goal: &str,
    step: &WorkflowStep,
    result: &WorkflowStepRunResult,
    dependency_summaries: &[String],
    acceptance_criteria: &[String],
    review_attempt: i32,
) -> String {
    let dependency_text = if dependency_summaries.is_empty() {
        "None".to_string()
    } else {
        dependency_summaries.join("\n\n")
    };
    let acceptance_text = if acceptance_criteria.is_empty() {
        "None".to_string()
    } else {
        acceptance_criteria
            .iter()
            .map(|item| format!("- {}", item.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let outputs_text = if result.outputs.is_empty() {
        "None".to_string()
    } else {
        result
            .outputs
            .iter()
            .map(|item| format!("- {}", item.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let data = PromptDataBuilder::new(MAX_DYNAMIC_CONTENT_BUDGET_BYTES)
        .add("step_title", &step.title, 1)
        .add("step_instructions", &step.instructions, 2)
        .add("workflow_goal", workflow_goal, 1)
        .add("worker_content", &result.content, 2)
        .add("worker_summary", &result.summary, 1)
        .add("predecessor_summaries", &dependency_text, 1)
        .add("acceptance_criteria", &acceptance_text, 1)
        .add("worker_outputs", &outputs_text, 1)
        .build();

    let mut prompt = String::with_capacity(4096);
    prompt.push_str(LEAD_REVIEW_PROMPT_PREFIX);
    prompt.push_str(&format!(
        r#"## Step Under Review

- Title: {step_title}
{instructions}
- Acceptance criteria:
{acceptance}

## Worker's Report

{summary}
{content}
- Output files:
{outputs}

## Context

{goal}
Review attempt: {review_attempt} of at most {max_review_attempts}.

This workflow permits no more than {max_review_attempts} review attempts. Perform the complete review now. If rejecting, report every issue you can identify in this single response, with concrete evidence and revision guidance. Do not hold back, defer, or drip-feed issues into later review attempts.

{deps}
## Report

Return one JSON object. Fill `step_key` with `{step_key}`, `execution_id` with `{execution_id}`.
Based on your independent verification of the actual code, verdict: approved or rejected."#,
        step_key = step.step_key,
        execution_id = step.execution_id,
        step_title = data.get("step_title"),
        instructions = data.get("step_instructions"),
        acceptance = data.get("acceptance_criteria"),
        summary = data.get("worker_summary"),
        content = data.get("worker_content"),
        outputs = data.get("worker_outputs"),
        goal = data.get("workflow_goal"),
        review_attempt = review_attempt,
        max_review_attempts = MAX_WORKFLOW_REVIEW_ATTEMPTS,
        deps = data.get("predecessor_summaries"),
    ));
    prompt = maybe_prepend_safety_preamble(&prompt);
    prompt
}

pub fn build_lead_review_prompt_with_schema(
    workflow_goal: &str,
    step: &WorkflowStep,
    result: &WorkflowStepRunResult,
    dependency_summaries: &[String],
    acceptance_criteria: &[String],
    review_attempt: i32,
) -> String {
    let mut prompt = build_lead_review_prompt(
        workflow_goal,
        step,
        result,
        dependency_summaries,
        acceptance_criteria,
        review_attempt,
    );
    prompt.push_str("\n\nRequired JSON Schema:\n```json\n");
    prompt.push_str(&workflow_review_protocol_json_schema(
        step.execution_id,
        &step.step_key,
    ));
    prompt.push_str("\n```\n");
    prompt.push_str("Return ONLY one JSON object matching this schema.\n");
    prompt
}

/// Static prefix for step revision prompts. Placed first for input cache hit rate.
static STEP_REVISION_PROMPT_PREFIX: &str = r#"You are revising a step in an workflow based on review feedback.

## Output Format

Return exactly one JSON object — no Markdown, no comments, no prose outside the JSON.

Use the same `final_result` / `error` / `approval_request` / `permission_request` / `continue_confirmation` / `input_request` types as the original step execution.

## Revision Guidelines

1. Read the review feedback carefully and understand the issues raised.
2. Fix only the issues identified in the feedback — preserve correct parts from your previous result.
3. Priority order is: user goal and explicit user feedback, then the original task scope and acceptance contract, then Lead or Reviewer feedback. Lead or Reviewer feedback may refine implementation only within the original task; it must not override the user goal or expand scope.
4. If Lead or Reviewer feedback conflicts with the original task or requires material scope expansion, return an `input_request` that explains the conflict instead of silently replacing the task.
5. Self-review before submitting: verify completeness, correctness, and that all feedback points are addressed.
6. Respond in the same language as the step instructions below.

"#;

pub fn build_step_revision_prompt(
    step: &WorkflowStep,
    feedback_source: WorkflowRevisionFeedbackSource,
    feedback_content: &str,
    previous_summary: &str,
    previous_content: Option<&str>,
    retry_count: i32,
) -> String {
    let feedback_label = match feedback_source {
        WorkflowRevisionFeedbackSource::Lead => "review_feedback",
        WorkflowRevisionFeedbackSource::User => "user_feedback",
    };

    let prev_content_trimmed = previous_content
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != previous_summary.trim());

    let mut builder = PromptDataBuilder::new(MAX_DYNAMIC_CONTENT_BUDGET_BYTES)
        .add(feedback_label, feedback_content.trim(), 2)
        .add("previous_result_summary", previous_summary.trim(), 1)
        .add("step_instructions", &step.instructions, 2);
    builder = builder.add_optional("previous_full_result", prev_content_trimmed, 2);
    let data = builder.build();

    let mut prompt = String::with_capacity(4096);

    // Static prefix first for cache hit rate
    prompt.push_str(STEP_REVISION_PROMPT_PREFIX);

    // Dynamic section: feedback source
    match feedback_source {
        WorkflowRevisionFeedbackSource::Lead => {
            prompt.push_str(&format!(
                "## Revision Required (attempt #{retry_count})\n\n"
            ));
            prompt.push_str(
                "Your previous execution did not pass review. Revise your work based on the feedback below.\n\n",
            );
            prompt.push_str(data.get(feedback_label));
            prompt.push_str(data.get("previous_result_summary"));
        }
        WorkflowRevisionFeedbackSource::Reviewer => {
            prompt.push_str(&format!(
                "## Reviewer Revision Required (attempt #{retry_count})\n\n"
            ));
            prompt.push_str(
                "Your previous execution did not pass Reviewer review. Revise your work based on the feedback below.\n\n",
            );
            prompt.push_str("### Reviewer Feedback\n");
            prompt.push_str(feedback_content.trim());
            prompt.push_str("\n\n### Your Previous Result Summary\n");
            prompt.push_str(previous_summary.trim());
            prompt.push('\n');
        }
        WorkflowRevisionFeedbackSource::User => {
            prompt.push_str(&format!(
                "## User Revision Required (attempt #{retry_count})\n\n"
            ));
            prompt.push_str(
                "Your previous execution did not pass user review. Revise based on user feedback.\n\n",
            );
            prompt.push_str(
                "**User feedback has the highest priority.** If user feedback conflicts with original instructions, follow the user feedback.\n\n",
            );
            prompt.push_str(data.get(feedback_label));
            prompt.push_str(data.get("previous_result_summary"));
        }
    }

    let prev_full = data.get("previous_full_result");
    if !prev_full.is_empty() {
        prompt.push_str(prev_full);
    }

    // Original task context
    prompt.push_str("\n### Original Task Instructions\n");
    prompt.push_str("- Title: ");
    prompt.push_str(&step.title);
    prompt.push_str(data.get("step_instructions"));
    prompt.push('\n');

    prompt = maybe_prepend_safety_preamble(&prompt);
    prompt
}

pub fn build_step_revision_prompt_with_schema(
    step: &WorkflowStep,
    feedback_source: WorkflowRevisionFeedbackSource,
    feedback_content: &str,
    previous_summary: &str,
    previous_content: Option<&str>,
    retry_count: i32,
    agent_skill_names: &[String],
) -> String {
    let mut prompt = build_step_revision_prompt(
        step,
        feedback_source,
        feedback_content,
        previous_summary,
        previous_content,
        retry_count,
    );
    if let Some(section) =
        crate::services::agent_skill_policy::format_skills_prompt_section(agent_skill_names)
    {
        prompt.push_str(&section);
    }
    prompt.push_str("\n\nRequired JSON Schema:\n```json\n");
    prompt.push_str(&workflow_step_protocol_json_schema(
        step.execution_id,
        &step.step_key,
        true,
    ));
    prompt.push_str("\n```\n");
    prompt.push_str("Return ONLY one JSON object matching this schema.\n");
    prompt
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
