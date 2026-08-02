use db::models::{
    workflow_step::WorkflowStep,
    workflow_types::{CompiledLoopDef, ReviewVerdict},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::workflow_runtime::{
    MAX_DYNAMIC_CONTENT_BUDGET_BYTES, PromptDataBuilder,
    WorkflowRuntimeError, extract_json_payload, maybe_prepend_safety_preamble,
};

#[derive(Debug, Clone)]
pub struct LoopReviewPromptStepInput {
    pub step_key: String,
    pub title: String,
    pub instructions: String,
    pub acceptance: Vec<String>,
    pub expected_outputs: Vec<String>,
    pub summary: String,
    pub content: String,
    pub outputs: Vec<String>,
    pub predecessor_handoffs: Vec<String>,
    pub successor_contracts: Vec<String>,
    pub user_skip_waiver: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoopReviewPromptContext {
    pub reviewer_name: String,
    pub reviewer_role: String,
    pub review_step_instructions: String,
    pub current_round: i32,
    pub loop_retry_count: i32,
    pub retry_budget: i32,
    pub review_scope_edges: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoopReviewStepFeedback {
    pub step_key: String,
    pub issue_id: String,
    pub feedback: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoopReviewAcceptanceResult {
    pub step_key: String,
    pub criterion: String,
    pub verdict: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LoopReviewProtocolMessage {
    LoopReviewResult {
        loop_key: String,
        execution_id: String,
        verdict: ReviewVerdict,
        feedback: String,
        acceptance_results: Vec<LoopReviewAcceptanceResult>,
        evidence: Vec<String>,
        #[serde(default)]
        issue_id: Option<String>,
        #[serde(default)]
        step_feedbacks: Vec<LoopReviewStepFeedback>,
    },
}

pub fn build_loop_review_prompt(
    workflow_goal: &str,
    loop_def: &CompiledLoopDef,
    execution_id: Uuid,
    review_attempt: i32,
    max_review_attempts: i32,
    review_steps: &[LoopReviewPromptStepInput],
    review_context: &LoopReviewPromptContext,
    response_language_instruction: &str,
) -> String {
    let review_scope_step_titles = review_steps
        .iter()
        .map(|step| step.title.clone())
        .collect::<Vec<_>>()
        .join(", ");
    let scope_edges = if review_context.review_scope_edges.is_empty() {
        "No edges inside the review scope.".to_string()
    } else {
        review_context.review_scope_edges.join("\n")
    };

    let mut budget_items = vec![
        ("workflow_goal".to_string(), workflow_goal.to_string(), 2),
        (
            "review_scope_step_titles".to_string(),
            review_scope_step_titles.clone(),
            1,
        ),
        (
            "reviewer_name".to_string(),
            review_context.reviewer_name.clone(),
            1,
        ),
        (
            "reviewer_role".to_string(),
            review_context.reviewer_role.clone(),
            1,
        ),
        (
            "review_step_instructions".to_string(),
            review_context.review_step_instructions.clone(),
            2,
        ),
        ("review_scope_edges".to_string(), scope_edges.clone(), 1),
    ];

    let prepared_steps: Vec<_> = review_steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let lbl = format!("s{index}");
            let acceptance = if step.acceptance.is_empty() {
                "None".to_string()
            } else {
                step.acceptance.join("; ")
            };
            let expected_outputs = if step.expected_outputs.is_empty() {
                "None declared".to_string()
            } else {
                step.expected_outputs.join(", ")
            };
            let predecessor_handoffs = if step.predecessor_handoffs.is_empty() {
                "None".to_string()
            } else {
                step.predecessor_handoffs.join("; ")
            };
            let successor_contracts = if step.successor_contracts.is_empty() {
                "None".to_string()
            } else {
                step.successor_contracts.join("; ")
            };
            let outputs = if step.outputs.is_empty() {
                "None".to_string()
            } else {
                step.outputs.join(", ")
            };
            let fields = [
                ("title", step.title.clone(), 1),
                ("instructions", step.instructions.clone(), 2),
                ("acceptance", acceptance, 1),
                ("expected_outputs", expected_outputs, 1),
                ("predecessor_handoffs", predecessor_handoffs, 1),
                ("successor_contracts", successor_contracts, 1),
                ("summary", step.summary.clone(), 1),
                ("content", step.content.clone(), 2),
                ("outputs", outputs, 1),
            ];
            for (field, content, weight) in fields {
                budget_items.push((format!("step_{lbl}_{field}"), content, weight));
            }
            if let Some(waiver) = step.user_skip_waiver.as_deref() {
                budget_items.push((format!("step_{lbl}_skip_waiver"), waiver.to_string(), 1));
            }
            (index, step, lbl)
        })
        .collect();

    let mut builder = PromptDataBuilder::new(MAX_DYNAMIC_CONTENT_BUDGET_BYTES);
    for (label, content, weight) in budget_items {
        builder = builder.add(label, content, weight);
    }
    let data = builder.build();

    let step_sections = prepared_steps
        .iter()
        .map(|(index, step, lbl)| {
            let waiver = data.get(&format!("step_{lbl}_skip_waiver"));
            let waiver_section = if waiver.is_empty() {
                String::new()
            } else {
                format!(
                    "\n- User-approved skip waiver: {waiver}\n- Review constraint: Do not reject this loop solely because of the waived skipped work."
                )
            };
            format!(
                "#### [{}] {} (`{}`)\n- Instructions: {}\n- Acceptance criteria: {}\n- Expected output contract: {}\n- Predecessor handoffs: {}\n- Successor contracts: {}\n- Execution summary: {}\n- Detailed content: {}\n- Actual outputs: {}{}",
                index + 1,
                data.get(&format!("step_{lbl}_title")),
                step.step_key,
                data.get(&format!("step_{lbl}_instructions")),
                data.get(&format!("step_{lbl}_acceptance")),
                data.get(&format!("step_{lbl}_expected_outputs")),
                data.get(&format!("step_{lbl}_predecessor_handoffs")),
                data.get(&format!("step_{lbl}_successor_contracts")),
                data.get(&format!("step_{lbl}_summary")),
                data.get(&format!("step_{lbl}_content")),
                data.get(&format!("step_{lbl}_outputs")),
                waiver_section,
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let rejected_feedback_template = review_steps
        .iter()
        .map(|step| {
            format!(
                r#"    {{ "step_key": "{}", "issue_id": "{}-stable-issue-slug", "feedback": "Specific revision feedback for this step" }}"#,
                step.step_key,
                step.step_key
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    let allowed_step_keys = review_steps
        .iter()
        .map(|step| step.step_key.clone())
        .collect::<Vec<_>>();
    let json_schema =
        loop_review_protocol_json_schema(execution_id, &loop_def.loop_key, &allowed_step_keys);
    let mut prompt = format!(
        r#"## Loop Review Task

You are {reviewer_name}, the {reviewer_role} assigned to this workflow's Review node. Review all execution results in the following loop or stage as one coherent unit. Do not represent yourself as the Lead unless your assigned role is Lead.

### Workflow Goal
{goal}

### Loop Information
- Loop key: {loop_key}
- Review attempt: {review_attempt} of at most {max_review_attempts}
- Current workflow round: {current_round}
- Current loop retry: {loop_retry_count} of retry budget {retry_budget}
- Review scope: {review_scope_step_titles}

### Review Node Stage Contract
- Review node instructions: {review_step_instructions}
- Review-scope DAG order: {review_scope_step_titles}
- Review-scope DAG edges:
{scope_edges}

### Execution Results by Step

{step_sections}

### Review Requirements
Evaluate the loop's execution quality from an overall perspective:
1. Whether the step results are mutually consistent and logically connected.
2. Whether the loop achieved this stage's goal overall.
3. Whether outputs from one step correctly connect to the next step.
4. Whether there are systemic issues that require broader rework.
5. Independently verify actual outputs before reaching a verdict: read the listed files or artifacts, inspect relevant code or deliverables, run applicable tests or checks, and compare every acceptance criterion with the expected-output and handoff contracts. Do not decide from worker content alone.
6. Report one acceptance_results item for every checked criterion, with a passed, failed, or not_applicable verdict and concrete evidence. Include evidence entries for files, commands, output, or inspected artifacts. If verification cannot be performed, say so as a risk and reject or request user input when it blocks a reliable verdict.
7. This workflow permits no more than {max_review_attempts} review attempts. Perform the complete review now. If rejecting, report every issue you can identify across the whole review scope in this single response, with concrete revision guidance. Do not hold back, defer, or drip-feed issues into later review attempts.
8. A user-approved skip waiver is an explicit scope decision. Do not reject solely because the waived skipped step was not re-executed. Continue to review all non-waived work normally.
9. Every rejection issue MUST have a stable issue_id. Reuse exactly the same issue_id when reporting the same underlying issue in a later review, even if wording changes. Use a new issue_id only for a genuinely different issue. When a skipped step shows a user-approved waiver issue scope, do not report that same issue scope again.

### Response Language Requirement
{response_language_instruction}

### Return Format
When approved, return:
{{
  "type": "loop_review_result",
  "loop_key": "{loop_key}",
  "execution_id": "{execution_id}",
  "verdict": "approved",
  "feedback": "Overall evaluation explaining why the loop review passed",
  "acceptance_results": [{{ "step_key": "step-key", "criterion": "acceptance criterion", "verdict": "passed", "evidence": "file:line or test command and result" }}],
  "evidence": ["Evidence collected from actual outputs and checks"]
}}

When rejected, return:
If only some steps need rework, list only those steps in step_feedbacks; steps not listed will keep their current completed state.
If the entire loop needs rework, omit step_feedbacks or return an empty array.
{{
  "type": "loop_review_result",
  "loop_key": "{loop_key}",
  "execution_id": "{execution_id}",
  "verdict": "rejected",
  "issue_id": "stable-overall-issue-slug",
  "feedback": "Detailed explanation of the overall issues and the concrete revision guidance for each step that needs changes",
  "acceptance_results": [{{ "step_key": "step-key", "criterion": "acceptance criterion", "verdict": "failed", "evidence": "file:line or failed test output" }}],
  "evidence": ["Evidence collected from actual outputs and checks"],
  "step_feedbacks": [
{rejected_feedback_template}
  ]
}}"#,
        goal = data.get("workflow_goal"),
        loop_key = loop_def.loop_key,
        execution_id = execution_id,
        review_attempt = review_attempt,
        max_review_attempts = max_review_attempts,
        review_scope_step_titles = data.get("review_scope_step_titles"),
        reviewer_name = data.get("reviewer_name"),
        reviewer_role = data.get("reviewer_role"),
        review_step_instructions = data.get("review_step_instructions"),
        current_round = review_context.current_round,
        loop_retry_count = review_context.loop_retry_count,
        retry_budget = review_context.retry_budget,
        scope_edges = data.get("review_scope_edges"),
        step_sections = step_sections,
        rejected_feedback_template = rejected_feedback_template,
        response_language_instruction = response_language_instruction.trim(),
    );
    prompt.push_str("\n\nRequired JSON Schema:\n```json\n");
    prompt.push_str(&json_schema);
    prompt.push_str("\n```\nReturn ONLY one JSON object matching this schema.\n");
    prompt = maybe_prepend_safety_preamble(&prompt);
    prompt
}

pub fn loop_review_protocol_json_schema(
    execution_id: Uuid,
    loop_key: &str,
    allowed_step_keys: &[String],
) -> String {
    let execution_id_schema = if execution_id.is_nil() {
        serde_json::json!({
            "type": "string",
            "description": "Must be the current workflow execution id"
        })
    } else {
        serde_json::json!({ "const": execution_id.to_string() })
    };

    serde_json::to_string_pretty(&serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["type", "loop_key", "execution_id", "verdict", "feedback", "acceptance_results", "evidence"],
        "additionalProperties": false,
        "properties": {
            "type": { "const": "loop_review_result" },
            "loop_key": { "const": loop_key },
            "execution_id": execution_id_schema,
            "verdict": { "enum": ["approved", "rejected"] },
            "feedback": { "type": "string", "minLength": 1 },
            "acceptance_results": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["step_key", "criterion", "verdict", "evidence"],
                    "additionalProperties": false,
                    "properties": {
                        "step_key": { "enum": allowed_step_keys },
                        "criterion": { "type": "string", "minLength": 1 },
                        "verdict": { "enum": ["passed", "failed", "not_applicable"] },
                        "evidence": { "type": "string", "minLength": 1 }
                    }
                }
            },
            "evidence": { "type": "array", "minItems": 1, "items": { "type": "string", "minLength": 1 } },
            "issue_id": { "type": "string", "minLength": 1, "maxLength": 160 },
            "step_feedbacks": {
                "type": "array",
                "default": [],
                "items": {
                    "type": "object",
                    "required": ["step_key", "issue_id", "feedback"],
                    "additionalProperties": false,
                    "properties": {
                        "step_key": { "enum": allowed_step_keys },
                        "issue_id": { "type": "string", "minLength": 1, "maxLength": 160 },
                        "feedback": { "type": "string", "minLength": 1 }
                    }
                }
            }
        },
        "allOf": [{
            "if": { "properties": { "verdict": { "const": "rejected" } } },
            "then": { "required": ["issue_id"] }
        }]
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

pub struct LoopRejectionPromptInput<'a> {
    pub workflow_goal: &'a str,
    pub loop_retry_count: i32,
    pub loop_retry_budget: i32,
    pub loop_current_state_summary: &'a str,
    pub loop_rejection_reason: &'a str,
    pub step_specific_feedback: &'a str,
    pub other_steps_feedback_summary: &'a [String],
    pub your_previous_summary: &'a str,
    pub your_previous_outputs: &'a [String],
    pub step: &'a WorkflowStep,
    pub acceptance: &'a [String],
    pub expected_outputs: &'a [String],
    pub external_dependency_text: &'a [String],
    pub response_language_instruction: &'a str,
}

pub fn build_loop_rejection_prompt(input: LoopRejectionPromptInput<'_>) -> String {
    let other_steps_feedback_summary = if input.other_steps_feedback_summary.is_empty() {
        "None".to_string()
    } else {
        input.other_steps_feedback_summary.join("\n")
    };
    let external_dependency_text = if input.external_dependency_text.is_empty() {
        "None".to_string()
    } else {
        input.external_dependency_text.join("\n")
    };
    let acceptance = if input.acceptance.is_empty() {
        "None declared".to_string()
    } else {
        input.acceptance.join("; ")
    };
    let expected_outputs = if input.expected_outputs.is_empty() {
        "None declared".to_string()
    } else {
        input.expected_outputs.join(", ")
    };
    let previous_outputs = if input.your_previous_outputs.is_empty() {
        "None recorded".to_string()
    } else {
        input.your_previous_outputs.join(", ")
    };

    let data = PromptDataBuilder::new(MAX_DYNAMIC_CONTENT_BUDGET_BYTES)
        .add("workflow_goal", input.workflow_goal, 2)
        .add("loop_state_summary", input.loop_current_state_summary, 1)
        .add("loop_rejection_reason", input.loop_rejection_reason, 2)
        .add("step_specific_feedback", input.step_specific_feedback, 2)
        .add("other_steps_feedback", &other_steps_feedback_summary, 1)
        .add("your_previous_summary", input.your_previous_summary, 1)
        .add("previous_outputs", &previous_outputs, 1)
        .add("acceptance", &acceptance, 1)
        .add("expected_outputs", &expected_outputs, 1)
        .add("step_title", &input.step.title, 1)
        .add("step_instructions", &input.step.instructions, 2)
        .add("external_dependencies", &external_dependency_text, 1)
        .build();

    let prompt = format!(
        r#"## Loop Rework Request (loop retry {loop_retry_count})

The overall loop review did not pass. Re-run your task according to the feedback below.

### Workflow Goal
{workflow_goal}

### Current Loop State
Retry {loop_retry_count} of {loop_retry_budget}. {loop_current_state_summary}

### Loop Review Decision
{rejection_reason}
### Revision Feedback for Your Step
{step_feedback}
### Other Steps' Revision Direction (for reference)
{other_feedback}
### Your Previous Execution Result
Summary: {your_previous_summary}
Outputs: {previous_outputs}

### Original Acceptance and Output Contract
- Acceptance criteria: {acceptance}
- Expected outputs: {expected_outputs}

### Requirements
1. Focus on the "Revision Feedback for Your Step" section.
2. Keep your changes consistent with the revision direction for other steps.
3. Preserve any correct work from your previous result and revise only what needs changes.
4. Reviewer feedback may clarify the task only within the workflow goal, original acceptance, and expected-output contract. Do not let it override those boundaries or expand scope.
5. If reviewer feedback conflicts with the original task or requires a material scope expansion, return an input_request explaining the conflict instead of silently choosing a new scope.
6. After completing the revision, return the result in the standard format.

### Response Language Requirement
{response_language_instruction}

### Original Task Instructions
Step title: {step_title}
{step_instructions}
### Completed Upstream Step Summaries (outside the loop)
{ext_deps}"#,
        loop_retry_count = input.loop_retry_count,
        loop_retry_budget = input.loop_retry_budget,
        loop_current_state_summary = data.get("loop_state_summary"),
        workflow_goal = data.get("workflow_goal"),
        rejection_reason = data.get("loop_rejection_reason"),
        step_feedback = data.get("step_specific_feedback"),
        other_feedback = data.get("other_steps_feedback"),
        your_previous_summary = data.get("your_previous_summary"),
        previous_outputs = data.get("previous_outputs"),
        acceptance = data.get("acceptance"),
        expected_outputs = data.get("expected_outputs"),
        step_title = data.get("step_title"),
        step_instructions = data.get("step_instructions"),
        ext_deps = data.get("external_dependencies"),
        response_language_instruction = input.response_language_instruction.trim(),
    );
    maybe_prepend_safety_preamble(&prompt)
}

pub fn build_loop_user_rejection_prompt(
    workflow_goal: &str,
    loop_retry_count: i32,
    loop_retry_budget: i32,
    user_feedback: &str,
    loop_current_state_summary: &str,
    your_previous_summary: &str,
    your_previous_outputs: &[String],
    step: &WorkflowStep,
    acceptance: &[String],
    expected_outputs: &[String],
    response_language_instruction: &str,
) -> String {
    let previous_outputs = if your_previous_outputs.is_empty() {
        "None recorded".to_string()
    } else {
        your_previous_outputs.join(", ")
    };
    let acceptance = if acceptance.is_empty() {
        "None declared".to_string()
    } else {
        acceptance.join("; ")
    };
    let expected_outputs = if expected_outputs.is_empty() {
        "None declared".to_string()
    } else {
        expected_outputs.join(", ")
    };
    let data = PromptDataBuilder::new(MAX_DYNAMIC_CONTENT_BUDGET_BYTES)
        .add("workflow_goal", workflow_goal, 2)
        .add("user_feedback", user_feedback, 2)
        .add("loop_state_summary", loop_current_state_summary, 1)
        .add("your_previous_summary", your_previous_summary, 1)
        .add("previous_outputs", &previous_outputs, 1)
        .add("acceptance", &acceptance, 1)
        .add("expected_outputs", &expected_outputs, 1)
        .add("step_title", &step.title, 1)
        .add("step_instructions", &step.instructions, 2)
        .build();
    let prompt = format!(
        r#"## User Loop Rework Request (loop retry {loop_retry_count})

The overall loop result did not pass user review. Re-run your task according to the user feedback.

### Workflow Goal
{workflow_goal}

### Retry Budget
Current retry {loop_retry_count} of {loop_retry_budget}

### User Feedback
{user_feedback}
### Current Loop State Summary
{loop_state}
### Your Previous Execution Result
Summary: {your_previous_summary}
Outputs: {previous_outputs}

### Original Acceptance and Output Contract
- Acceptance criteria: {acceptance}
- Expected outputs: {expected_outputs}

### Requirements
1. Treat the user feedback as the highest priority.
2. Understand how the user feedback affects the overall loop and adjust your work accordingly.
3. {response_language_instruction}
4. After completing the revision, return the result in the standard format.

### Original Task Instructions
Step title: {step_title}
{step_instructions}"#,
        loop_retry_count = loop_retry_count,
        loop_retry_budget = loop_retry_budget,
        workflow_goal = data.get("workflow_goal"),
        user_feedback = data.get("user_feedback"),
        loop_state = data.get("loop_state_summary"),
        your_previous_summary = data.get("your_previous_summary"),
        previous_outputs = data.get("previous_outputs"),
        acceptance = data.get("acceptance"),
        expected_outputs = data.get("expected_outputs"),
        step_title = data.get("step_title"),
        step_instructions = data.get("step_instructions"),
        response_language_instruction = response_language_instruction.trim(),
    );
    maybe_prepend_safety_preamble(&prompt)
}

pub fn parse_loop_review_output(
    execution_id: Uuid,
    loop_key: &str,
    raw_output: &str,
) -> Result<LoopReviewProtocolMessage, WorkflowRuntimeError> {
    let payload = extract_json_payload(raw_output).ok_or_else(|| {
        WorkflowRuntimeError::Validation("loop review 输出中未找到 JSON 对象".to_string())
    })?;
    let message: LoopReviewProtocolMessage = serde_json::from_str(&payload)?;

    match &message {
        LoopReviewProtocolMessage::LoopReviewResult {
            loop_key: actual_loop_key,
            execution_id: actual_execution_id,
            verdict,
            feedback,
            acceptance_results,
            evidence,
            issue_id,
            step_feedbacks,
        } => {
            if actual_loop_key != loop_key {
                return Err(WorkflowRuntimeError::Validation(format!(
                    "loop review 的 loop_key 非法，期望 '{}'，实际 '{}'",
                    loop_key, actual_loop_key
                )));
            }
            if actual_execution_id != &execution_id.to_string() {
                return Err(WorkflowRuntimeError::Validation(format!(
                    "loop review 的 execution_id 非法，期望 '{}'，实际 '{}'",
                    execution_id, actual_execution_id
                )));
            }
            if feedback.trim().is_empty() {
                return Err(WorkflowRuntimeError::Validation(
                    "loop review 的 feedback 不能为空".to_string(),
                ));
            }
            if evidence.iter().any(|item| item.trim().is_empty()) || evidence.is_empty() {
                return Err(WorkflowRuntimeError::Validation(
                    "loop review 的 evidence 不能为空".to_string(),
                ));
            }
            if acceptance_results.iter().any(|item| {
                item.step_key.trim().is_empty()
                    || item.criterion.trim().is_empty()
                    || item.evidence.trim().is_empty()
                    || !matches!(
                        item.verdict.as_str(),
                        "passed" | "failed" | "not_applicable"
                    )
            }) {
                return Err(WorkflowRuntimeError::Validation(
                    "loop review 的 acceptance_results 非法".to_string(),
                ));
            }
            if matches!(verdict, ReviewVerdict::Rejected)
                && issue_id.as_deref().map(str::trim).is_none_or(str::is_empty)
            {
                return Err(WorkflowRuntimeError::Validation(
                    "loop review rejected 时 issue_id 不能为空".to_string(),
                ));
            }
            if matches!(verdict, ReviewVerdict::Rejected)
                && step_feedbacks
                    .iter()
                    .any(|item| item.feedback.trim().is_empty() || item.issue_id.trim().is_empty())
            {
                return Err(WorkflowRuntimeError::Validation(
                    "loop review rejected 时 step_feedbacks.issue_id/feedback 不能为空".to_string(),
                ));
            }
        }
    }

    Ok(message)
}

#[cfg(test)]
mod tests {
    use db::models::workflow_types::WorkflowStepType;

    use super::*;

    fn sample_loop_def() -> CompiledLoopDef {
        CompiledLoopDef {
            loop_key: "loop-a".to_string(),
            member_step_keys: vec!["draft".to_string(), "revise".to_string()],
            review_step_key: "review".to_string(),
            review_scope_step_keys: vec!["draft".to_string(), "revise".to_string()],
            max_retry: 2,
            user_review_required: true,
        }
    }

    fn sample_worker_step() -> WorkflowStep {
        let now = chrono::Utc::now();
        WorkflowStep {
            id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            round_id: Uuid::new_v4(),
            compiled_revision_id: None,
            step_key: "draft".to_string(),
            step_type: WorkflowStepType::Task,
            title: "Draft".to_string(),
            instructions: "Write the first draft".to_string(),
            assigned_workflow_agent_session_id: None,
            status: db::models::workflow_types::WorkflowStepStatus::Revising,
            retry_count: 1,
            max_retry: 3,
            round_index: 1,
            display_order: 0,
            latest_run_id: None,
            summary_text: None,
            content: None,
            loop_id: None,
            lead_review_required: true,
            user_review_required: false,
            revision_context: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
        }
    }

    #[test]
    fn build_loop_review_prompt_includes_all_required_sections() {
        let prompt = build_loop_review_prompt(
            "Deliver a coherent feature",
            &sample_loop_def(),
            Uuid::nil(),
            1,
            3,
            &[
                LoopReviewPromptStepInput {
                    step_key: "draft".to_string(),
                    title: "Draft".to_string(),
                    instructions: "Write the draft".to_string(),
                    acceptance: vec!["Complete the initial scope".to_string()],
                    expected_outputs: vec!["docs/draft.md".to_string()],
                    summary: "Draft ready".to_string(),
                    content: "Produced a draft document".to_string(),
                    outputs: vec!["docs/draft.md".to_string()],
                    predecessor_handoffs: vec!["from predecessor `research`".to_string()],
                    successor_contracts: vec!["to successor `revise`".to_string()],
                    user_skip_waiver: Some(
                        "User chose to keep this skipped step after review feedback.".to_string(),
                    ),
                },
                LoopReviewPromptStepInput {
                    step_key: "revise".to_string(),
                    title: "Revise".to_string(),
                    instructions: "Improve the draft".to_string(),
                    acceptance: vec![],
                    expected_outputs: vec!["docs/final.md".to_string()],
                    summary: "Revision ready".to_string(),
                    content: "Added missing details".to_string(),
                    outputs: vec![],
                    predecessor_handoffs: vec!["from predecessor `draft`".to_string()],
                    successor_contracts: vec![],
                    user_skip_waiver: None,
                },
            ],
            &LoopReviewPromptContext {
                reviewer_name: "ReviewMember".to_string(),
                reviewer_role: "Reviewer".to_string(),
                review_step_instructions: "Audit the stage contract".to_string(),
                current_round: 2,
                loop_retry_count: 1,
                retry_budget: 2,
                review_scope_edges: vec!["- `draft` -> `revise` (hard)".to_string()],
            },
            "You MUST write human-readable JSON string values in Simplified Chinese.",
        );

        assert!(prompt.contains("## Loop Review Task"));
        assert!(prompt.contains("User-approved skip waiver"));
        assert!(prompt.contains("Do not reject this loop solely"));
        assert!(prompt.contains("Response Language Requirement"));
        assert!(
            prompt.contains(
                "You MUST write human-readable JSON string values in Simplified Chinese."
            )
        );
        assert!(prompt.contains("Deliver a coherent feature"));
        assert!(prompt.contains("loop-a"));
        assert!(prompt.contains("Draft"));
        assert!(prompt.contains("Revise"));
        assert!(prompt.contains("docs/draft.md"));
        assert!(prompt.contains("\"type\": \"loop_review_result\""));
        assert!(prompt.contains("Review attempt: 1 of at most 3"));
        assert!(
            prompt.contains("report every issue you can identify across the whole review scope")
        );
        assert!(prompt.contains("Every rejection issue MUST have a stable issue_id"));
        assert!(prompt.contains("ReviewMember, the Reviewer"));
        assert!(prompt.contains("Review-scope DAG edges"));
        assert!(prompt.contains("Expected output contract"));
        assert!(prompt.contains("Independently verify actual outputs"));
    }

    #[test]
    fn build_loop_rejection_prompt_contains_feedback_sections() {
        let step = sample_worker_step();
        let prompt = build_loop_rejection_prompt(LoopRejectionPromptInput {
            workflow_goal: "Deliver a coherent feature",
            loop_retry_count: 2,
            loop_retry_budget: 3,
            loop_current_state_summary: "loop is retrying",
            loop_rejection_reason: "整体结构不一致",
            step_specific_feedback: "请统一术语",
            other_steps_feedback_summary: &["其他节点需要同步命名".to_string()],
            your_previous_summary: "Old summary",
            your_previous_outputs: &["docs/draft.md".to_string()],
            step: &step,
            acceptance: &["完成范围".to_string()],
            expected_outputs: &["docs/draft.md".to_string()],
            external_dependency_text: &["外部依赖 A 已完成".to_string()],
            response_language_instruction: "You MUST write human-readable JSON string values in Simplified Chinese.",
        });

        assert!(prompt.contains("loop retry 2"));
        assert!(prompt.contains("整体结构不一致"));
        assert!(prompt.contains("请统一术语"));
        assert!(prompt.contains("其他节点需要同步命名"));
        assert!(prompt.contains("外部依赖 A 已完成"));
        assert!(prompt.contains("Response Language Requirement"));
        assert!(
            prompt.contains(
                "You MUST write human-readable JSON string values in Simplified Chinese."
            )
        );
    }

    #[test]
    fn build_loop_user_rejection_prompt_contains_user_feedback() {
        let step = sample_worker_step();
        let prompt = build_loop_user_rejection_prompt(
            "交付中文文档",
            1,
            2,
            "用户要求改为中文输出",
            "当前回路已生成英文文档",
            "Old summary",
            &["docs/draft.md".to_string()],
            &step,
            &["完成范围".to_string()],
            &["docs/draft.md".to_string()],
            "You MUST write human-readable JSON string values in Simplified Chinese.",
        );

        assert!(prompt.contains("User Loop Rework Request"));
        assert!(
            prompt.contains(
                "You MUST write human-readable JSON string values in Simplified Chinese."
            )
        );
        assert!(prompt.contains("用户要求改为中文输出"));
        assert!(prompt.contains("当前回路已生成英文文档"));
    }

    #[test]
    fn parse_loop_review_output_accepts_approved_result() {
        let execution_id = Uuid::new_v4();
        let raw = format!(
            r#"{{
  "type": "loop_review_result",
  "loop_key": "loop-a",
  "execution_id": "{}",
  "verdict": "approved",
  "feedback": "整体通过",
  "acceptance_results": [{{ "step_key": "draft", "criterion": "范围", "verdict": "passed", "evidence": "docs/draft.md" }}],
  "evidence": ["inspected docs/draft.md"]
}}"#,
            execution_id
        );

        let parsed = parse_loop_review_output(execution_id, "loop-a", &raw).expect("parse");
        assert_eq!(
            parsed,
            LoopReviewProtocolMessage::LoopReviewResult {
                loop_key: "loop-a".to_string(),
                execution_id: execution_id.to_string(),
                verdict: ReviewVerdict::Approved,
                feedback: "整体通过".to_string(),
                acceptance_results: vec![LoopReviewAcceptanceResult {
                    step_key: "draft".to_string(),
                    criterion: "范围".to_string(),
                    verdict: "passed".to_string(),
                    evidence: "docs/draft.md".to_string(),
                }],
                evidence: vec!["inspected docs/draft.md".to_string()],
                issue_id: None,
                step_feedbacks: vec![],
            }
        );
    }

    #[test]
    fn parse_loop_review_output_accepts_rejected_result() {
        let execution_id = Uuid::new_v4();
        let raw = format!(
            r#"{{
  "type": "loop_review_result",
  "loop_key": "loop-a",
  "execution_id": "{}",
  "verdict": "rejected",
  "issue_id": "overall-missing-background",
  "feedback": "需要整体返工",
  "acceptance_results": [{{ "step_key": "draft", "criterion": "背景", "verdict": "failed", "evidence": "missing in docs/draft.md" }}],
  "evidence": ["inspected docs/draft.md"],
  "step_feedbacks": [
    {{ "step_key": "draft", "issue_id": "draft-missing-background", "feedback": "请补充背景" }}
  ]
}}"#,
            execution_id
        );

        let parsed = parse_loop_review_output(execution_id, "loop-a", &raw).expect("parse");
        assert!(matches!(
            parsed,
            LoopReviewProtocolMessage::LoopReviewResult {
                verdict: ReviewVerdict::Rejected,
                ..
            }
        ));
    }

    #[test]
    fn parse_loop_review_output_rejects_invalid_payload() {
        let execution_id = Uuid::new_v4();
        let raw = format!(
            r#"{{
  "type": "loop_review_result",
  "loop_key": "other-loop",
  "execution_id": "{}",
  "verdict": "approved",
  "feedback": "ok",
  "acceptance_results": [],
  "evidence": []
}}"#,
            execution_id
        );

        let err = parse_loop_review_output(execution_id, "loop-a", &raw).expect_err("invalid");
        assert!(matches!(err, WorkflowRuntimeError::Validation(_)));
    }

    #[test]
    fn parse_loop_review_output_requires_stable_issue_id_for_rejection() {
        let execution_id = Uuid::new_v4();
        let raw = format!(
            r#"{{
  "type": "loop_review_result",
  "loop_key": "loop-a",
  "execution_id": "{}",
  "verdict": "rejected",
  "feedback": "发现了新问题",
  "acceptance_results": [{{ "step_key": "draft", "criterion": "完整性", "verdict": "failed", "evidence": "检查 docs/draft.md 后发现缺失" }}],
  "evidence": ["检查 docs/draft.md"]
}}"#,
            execution_id
        );

        let error = parse_loop_review_output(execution_id, "loop-a", &raw)
            .expect_err("missing issue_id must fail");
        assert!(error.to_string().contains("issue_id"));
    }
}
