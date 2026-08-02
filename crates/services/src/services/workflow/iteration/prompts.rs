pub fn build_iteration_plan_prompt(
    original_goal: &str,
    current_state_summary: &str,
    user_feedback_json: &str,
    iteration_round: i32,
    history: &[WorkflowIterationFeedback],
    lead_agent_id: &str,
    available_agents: &[WorkflowPlanningAgent],
    previous_plan: &WorkflowPlanJson,
    response_language_instruction: &str,
) -> String {
    let history_text = if history.is_empty() {
        "None".to_string()
    } else {
        history
            .iter()
            .map(|item| {
                format!(
                    "- feedback_id={} from_round={} to_round={:?}: {}",
                    item.id, item.from_round_id, item.to_round_id, item.user_feedback_json
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let feedback_text = format_iteration_feedback(user_feedback_json);
    let previous_plan_json = serde_json::to_string_pretty(previous_plan)
        .unwrap_or_else(|_| "{}".to_string());
    let available_agents_json =
        serde_json::to_string_pretty(available_agents).unwrap_or_else(|_| "[]".to_string());
    let data = PromptDataBuilder::new()
        .add("original_goal", original_goal.trim())
        .add("current_state_summary", current_state_summary.trim())
        .add("user_feedback", &feedback_text)
        .add("iteration_history", &history_text)
        .add("available_agents_json", &available_agents_json)
        .add("previous_plan_json", &previous_plan_json)
        .build();

    let next_round = iteration_round + 1;

    let mut prompt = String::new();
    prompt.push_str(&format!(
        r#"# Workflow Plan Generation

You are generating an executable workflow plan from a confirmed implementation brief.
This generation is for workflow iteration round {next_round}: the previous workflow round completed but the user rejected the result.
The output source of truth is React Flow compatible workflow JSON. Do not output Markdown, YAML, comments, explanations, or prose outside the JSON object.

"#
    ));
    prompt.push_str(PLAN_STABLE_OUTPUT_CONTRACT);
    prompt.push_str("## WorkflowPlanJson Schema Reference\n\n");
    prompt.push_str(PLAN_SCHEMA_DEFINITION);
    prompt.push_str("\n\n");
    prompt.push_str(PLAN_STATIC_CONSTRAINTS);
    prompt.push_str(PLAN_SKILLS_GUIDANCE);
    prompt.push_str("## Dynamic Inputs\n\n");

    prompt.push_str("Response language requirement:\n");
    prompt.push_str(response_language_instruction.trim());
    prompt.push_str("\n\nPlan goal brief:\n");
    prompt.push_str(data.get("original_goal"));
    prompt.push_str("\n\nLead agent id:\n");
    prompt.push_str(lead_agent_id);
    prompt.push_str("\n\nAvailable agents JSON:\n");
    prompt.push_str(data.get("available_agents_json"));

    prompt.push_str("\n\n## Iteration Context\n\n");
    prompt.push_str(&format!(
        "Iteration request: user rejected the previous round and requested a revised plan for round {next_round}. Preserve correct work; change only what the feedback requires.\n"
    ));
    prompt.push_str("\n### Previous Round State\n");
    prompt.push_str(data.get("current_state_summary"));
    prompt.push_str("\n\n### User Feedback (reason for rejection)\n");
    prompt.push_str(data.get("user_feedback"));
    prompt.push_str("\n\n### Iteration History\n");
    prompt.push_str(data.get("iteration_history"));

    prompt.push_str("\n\n### Previous Round Workflow Plan JSON\n```json\n");
    prompt.push_str(data.get("previous_plan_json"));
    prompt.push_str(
        r#"
```

This is the complete plan JSON that produced the rejected round. Use it as the baseline:
- Preserve existing node ids, edge ids, and edge structure for work you keep, so completed steps stay traceable across rounds.
- Preserve the original `acceptance`, `outputs`, `checklist`, `verificationCommands`, and `completionEvidence` of nodes you keep; only adjust them when the user feedback requires it.
- Do not discard completed work that the feedback does not mention; add or revise nodes only where the feedback demands changes.
- Every new or revised `task` node must still satisfy the full task contract fields.

"#,
    );

    prompt.push_str("\nFinal instruction: return the workflow plan JSON object only.");
    prompt = maybe_prepend_safety_preamble(&prompt);
    prompt
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
