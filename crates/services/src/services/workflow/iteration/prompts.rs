pub fn build_iteration_plan_prompt(
    original_goal: &str,
    current_state_summary: &str,
    user_feedback_json: &str,
    _iteration_round: i32,
    _history: &[WorkflowIterationFeedback],
    lead_agent_id: &str,
    available_agents: &[WorkflowPlanningAgent],
    previous_plan: &WorkflowPlanJson,
    response_language_instruction: &str,
) -> String {
    use super::workflow_runtime::prompt_builders::plan_generation::{
        PlanGenerationMode, PlanGenerationPromptInput, PlanningMemberInput,
        build_plan_generation_prompt,
    };

    let members = available_agents
        .iter()
        .map(|agent| PlanningMemberInput {
            agent_id: agent.agent_id.clone(),
            name: agent.name.clone(),
            role: agent
                .member_role
                .clone()
                .unwrap_or_else(|| agent.workflow_role.clone()),
            responsibilities: agent.responsibilities.clone(),
            skills: agent.skills.clone(),
            tools: agent.tools_enabled.clone(),
        })
        .collect();
    build_plan_generation_prompt(&PlanGenerationPromptInput {
        summary: original_goal.trim().to_string(),
        design_doc_paths: Vec::new(),
        lead_agent_id: lead_agent_id.to_string(),
        members,
        response_language: response_language_instruction.trim().to_string(),
        mode: PlanGenerationMode::Iteration {
            previous_plan: previous_plan.clone(),
            current_state_summary: current_state_summary.trim().to_string(),
            latest_user_feedback: format_iteration_feedback(user_feedback_json),
        },
    })
}

fn format_iteration_feedback(user_feedback_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(user_feedback_json) else {
        return user_feedback_json.trim().to_string();
    };
    let Some(feedback) = value.get("feedback").and_then(|item| item.as_object()) else {
        return serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| user_feedback_json.trim().to_string());
    };

    let mut lines = Vec::new();
    for key in ["what_wrong", "expected", "priority", "additional_notes"] {
        if let Some(text) = feedback.get(key).and_then(|item| item.as_str())
            && !text.trim().is_empty()
        {
            lines.push(format!("- {key}: {}", text.trim()));
        }
    }

    if lines.is_empty() {
        serde_json::to_string_pretty(&value)
            .unwrap_or_else(|_| user_feedback_json.trim().to_string())
    } else {
        lines.join("\n")
    }
}