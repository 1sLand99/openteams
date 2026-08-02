#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowStepProtocolMessage {
    FinalResult {
        step_key: String,
        execution_id: String,
        summary: String,
        content: String,
        #[serde(default)]
        outputs: Vec<String>,
    },
    Error {
        step_key: String,
        execution_id: String,
        message: String,
        #[serde(default)]
        content: Option<String>,
    },
    ApprovalRequest {
        step_key: String,
        execution_id: String,
        title: String,
        #[serde(default)]
        description: Option<String>,
    },
    PermissionRequest {
        step_key: String,
        execution_id: String,
        title: String,
        #[serde(default)]
        description: Option<String>,
    },
    ContinueConfirmation {
        step_key: String,
        execution_id: String,
        message: String,
        #[serde(default)]
        description: Option<String>,
    },
    InputRequest {
        step_key: String,
        execution_id: String,
        prompt: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        placeholder: Option<String>,
    },
    ReviewResult {
        step_key: String,
        execution_id: String,
        verdict: ReviewVerdict,
        summary: String,
        content: String,
        acceptance_results: Vec<WorkflowAcceptanceResult>,
        evidence: Vec<String>,
        #[serde(default)]
        risks: Vec<String>,
        #[serde(default)]
        unfinished_items: Vec<String>,
    },
    ResultReviewResult {
        step_key: String,
        execution_id: String,
        overall_status: WorkflowResultOverallStatus,
        summary: String,
        content: String,
        acceptance_results: Vec<WorkflowAcceptanceResult>,
        evidence: Vec<String>,
        #[serde(default)]
        risks: Vec<String>,
        #[serde(default)]
        unfinished_items: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowAcceptanceResult {
    pub criterion: String,
    pub verdict: String,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowResultOverallStatus {
    Completed,
    CompletedWithConcerns,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowReviewProtocolMessage {
    ReviewResult {
        step_key: String,
        execution_id: String,
        verdict: ReviewVerdict,
        feedback: String,
        acceptance_results: Vec<WorkflowAcceptanceResult>,
        evidence: Vec<String>,
        #[serde(default)]
        risks: Vec<String>,
        #[serde(default)]
        unfinished_items: Vec<String>,
    },
}

pub fn extract_json_payload(raw_output: &str) -> Option<String> {
    let trimmed = raw_output.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed.to_string());
    }

    for pattern in ["```json", "```"] {
        if let Some(start) = trimmed.find(pattern) {
            let remainder = &trimmed[start + pattern.len()..];
            if let Some(end) = remainder.find("```") {
                let candidate = remainder[..end].trim();
                if candidate.starts_with('{') && candidate.ends_with('}') {
                    return Some(candidate.to_string());
                }
            }
        }
    }

    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (start < end).then(|| trimmed[start..=end].to_string())
}

pub fn workflow_step_protocol_json_schema(
    execution_id: Uuid,
    step_key: &str,
    allow_interaction_requests: bool,
) -> String {
    let mut variants = vec![
        serde_json::json!({
            "type": "object",
            "required": ["type", "step_key", "execution_id", "summary", "content"],
            "additionalProperties": false,
            "properties": {
                "type": { "const": "final_result" },
                "step_key": { "const": step_key },
                "execution_id": { "const": execution_id.to_string() },
                "summary": { "type": "string", "minLength": 1 },
                "content": { "type": "string" },
                "outputs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "default": []
                }
            }
        }),
        serde_json::json!({
            "type": "object",
            "required": ["type", "step_key", "execution_id", "verdict", "summary", "content", "acceptance_results", "evidence"],
            "additionalProperties": false,
            "properties": {
                "type": { "const": "review_result" },
                "step_key": { "const": step_key },
                "execution_id": { "const": execution_id.to_string() },
                "verdict": { "enum": ["approved", "rejected"] },
                "summary": { "type": "string", "minLength": 1 },
                "content": { "type": "string" },
                "acceptance_results": { "$ref": "#/$defs/acceptance_results" },
                "evidence": { "$ref": "#/$defs/evidence" },
                "risks": { "type": "array", "items": { "type": "string" }, "default": [] },
                "unfinished_items": { "type": "array", "items": { "type": "string" }, "default": [] }
            }
        }),
        serde_json::json!({
            "type": "object",
            "required": ["type", "step_key", "execution_id", "overall_status", "summary", "content", "acceptance_results", "evidence"],
            "additionalProperties": false,
            "properties": {
                "type": { "const": "result_review_result" },
                "step_key": { "const": step_key },
                "execution_id": { "const": execution_id.to_string() },
                "overall_status": { "enum": ["completed", "completed_with_concerns", "blocked"] },
                "summary": { "type": "string", "minLength": 1 },
                "content": { "type": "string" },
                "acceptance_results": { "$ref": "#/$defs/acceptance_results" },
                "evidence": { "$ref": "#/$defs/evidence" },
                "risks": { "type": "array", "items": { "type": "string" }, "default": [] },
                "unfinished_items": { "type": "array", "items": { "type": "string" }, "default": [] }
            }
        }),
        serde_json::json!({
            "type": "object",
            "required": ["type", "step_key", "execution_id", "message"],
            "additionalProperties": false,
            "properties": {
                "type": { "const": "error" },
                "step_key": { "const": step_key },
                "execution_id": { "const": execution_id.to_string() },
                "message": { "type": "string", "minLength": 1 },
                "content": { "type": ["string", "null"] }
            }
        }),
    ];

    if allow_interaction_requests {
        variants.extend([
            serde_json::json!({
                "type": "object",
                "required": ["type", "step_key", "execution_id", "title"],
                "additionalProperties": false,
                "properties": {
                    "type": { "enum": ["approval_request", "permission_request"] },
                    "step_key": { "const": step_key },
                    "execution_id": { "const": execution_id.to_string() },
                    "title": { "type": "string", "minLength": 1 },
                    "description": { "type": ["string", "null"] }
                }
            }),
            serde_json::json!({
                "type": "object",
                "required": ["type", "step_key", "execution_id", "message"],
                "additionalProperties": false,
                "properties": {
                    "type": { "const": "continue_confirmation" },
                    "step_key": { "const": step_key },
                    "execution_id": { "const": execution_id.to_string() },
                    "message": { "type": "string", "minLength": 1 },
                    "description": { "type": ["string", "null"] }
                }
            }),
            serde_json::json!({
                "type": "object",
                "required": ["type", "step_key", "execution_id", "prompt"],
                "additionalProperties": false,
                "properties": {
                    "type": { "const": "input_request" },
                    "step_key": { "const": step_key },
                    "execution_id": { "const": execution_id.to_string() },
                    "prompt": { "type": "string", "minLength": 1 },
                    "description": { "type": ["string", "null"] },
                    "placeholder": { "type": ["string", "null"] }
                }
            }),
        ]);
    }

    serde_json::to_string_pretty(&serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {
            "acceptance_results": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["criterion", "verdict", "evidence"],
                    "additionalProperties": false,
                    "properties": {
                        "criterion": { "type": "string", "minLength": 1 },
                        "verdict": { "enum": ["passed", "failed", "not_applicable"] },
                        "evidence": { "type": "string", "minLength": 1 }
                    }
                }
            },
            "evidence": {
                "type": "array",
                "minItems": 1,
                "items": { "type": "string", "minLength": 1 }
            }
        },
        "oneOf": variants
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

pub fn workflow_step_protocol_json_schema_for_step(
    execution_id: Uuid,
    step_key: &str,
    allow_interaction_requests: bool,
    step_type: &WorkflowStepType,
) -> String {
    let base = workflow_step_protocol_json_schema(
        execution_id,
        step_key,
        allow_interaction_requests,
    );
    let Ok(mut schema) = serde_json::from_str::<serde_json::Value>(&base) else {
        return base;
    };
    let required_type = match step_type {
        WorkflowStepType::Review => Some("review_result"),
        WorkflowStepType::Result => Some("result_review_result"),
        WorkflowStepType::Task => None,
    };
    let Some(required_type) = required_type else {
        return base;
    };
    let Some(variants) = schema.get_mut("oneOf").and_then(|value| value.as_array_mut()) else {
        return base;
    };
    variants.retain(|variant| {
        let type_property = variant
            .get("properties")
            .and_then(|properties| properties.get("type"));
        let const_type = type_property
            .and_then(|value| value.get("const"))
            .and_then(|value| value.as_str());
        matches!(
            const_type,
            Some(value)
                if value == required_type
                    || matches!(
                        value,
                        "error" | "approval_request" | "permission_request"
                            | "continue_confirmation" | "input_request"
                    )
        ) || type_property
            .and_then(|value| value.get("enum"))
            .is_some()
    });
    serde_json::to_string_pretty(&schema).unwrap_or(base)
}

pub fn workflow_review_protocol_json_schema(execution_id: Uuid, step_key: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["type", "step_key", "execution_id", "verdict", "feedback", "acceptance_results", "evidence"],
        "additionalProperties": false,
        "properties": {
            "type": { "const": "review_result" },
            "step_key": { "const": step_key },
            "execution_id": { "const": execution_id.to_string() },
            "verdict": { "enum": ["approved", "rejected"] },
            "feedback": { "type": "string", "minLength": 1 },
            "acceptance_results": { "$ref": "#/$defs/acceptance_results" },
            "evidence": { "$ref": "#/$defs/evidence" },
            "risks": { "type": "array", "items": { "type": "string" }, "default": [] },
            "unfinished_items": { "type": "array", "items": { "type": "string" }, "default": [] }
        },
        "$defs": {
            "acceptance_results": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["criterion", "verdict", "evidence"],
                    "additionalProperties": false,
                    "properties": {
                        "criterion": { "type": "string", "minLength": 1 },
                        "verdict": { "enum": ["passed", "failed", "not_applicable"] },
                        "evidence": { "type": "string", "minLength": 1 }
                    }
                }
            },
            "evidence": {
                "type": "array",
                "minItems": 1,
                "items": { "type": "string", "minLength": 1 }
            }
        }
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

pub fn build_workflow_protocol_retry_prompt(
    protocol_name: &str,
    schema: &str,
    error: &str,
    previous_input: &str,
    previous_output: &str,
) -> String {
    let data = PromptDataBuilder::new(MAX_DYNAMIC_CONTENT_BUDGET_BYTES)
        .add("previous_workflow_request", previous_input, 1)
        .add("previous_invalid_response", previous_output, 1)
        .build();

    let prompt = format!(
        r#"Your previous workflow {protocol_name} response did not match the required JSON protocol.
Error: {error}

Retry the same workflow request. Respond with ONLY one JSON object. Do not include Markdown fences, prose, explanations, or extra text.

Required JSON Schema:
```json
{schema}
```

Previous workflow request:
{prev_input}

Previous invalid response:
{prev_output}"#,
        prev_input = data.get("previous_workflow_request"),
        prev_output = data.get("previous_invalid_response"),
    );
    maybe_prepend_safety_preamble(&prompt)
}

pub fn should_retry_workflow_protocol_parse_failure(raw_output: &str) -> bool {
    !raw_output.trim().is_empty()
}

pub fn parse_step_protocol_output(
    execution_id: Uuid,
    step_key: &str,
    raw_output: &str,
) -> Result<WorkflowStepProtocolMessage, WorkflowRuntimeError> {
    let payload = extract_json_payload(raw_output).ok_or_else(|| {
        WorkflowRuntimeError::Validation("step 输出中未找到 JSON 对象".to_string())
    })?;

    let message: WorkflowStepProtocolMessage = serde_json::from_str(&payload)?;
    match &message {
        WorkflowStepProtocolMessage::FinalResult {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            ..
        }
        | WorkflowStepProtocolMessage::Error {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            ..
        }
        | WorkflowStepProtocolMessage::ApprovalRequest {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            ..
        }
        | WorkflowStepProtocolMessage::PermissionRequest {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            ..
        }
        | WorkflowStepProtocolMessage::ContinueConfirmation {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            ..
        }
        | WorkflowStepProtocolMessage::InputRequest {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            ..
        }
        | WorkflowStepProtocolMessage::ReviewResult {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            ..
        }
        | WorkflowStepProtocolMessage::ResultReviewResult {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            ..
        } => {
            if actual_step_key != step_key {
                return Err(WorkflowRuntimeError::Validation(format!(
                    "step protocol 的 step_key 非法，期望 '{}'，实际 '{}'",
                    step_key, actual_step_key
                )));
            }
            if actual_execution_id != &execution_id.to_string() {
                return Err(WorkflowRuntimeError::Validation(format!(
                    "step protocol 的 execution_id 非法，期望 '{}'，实际 '{}'",
                    execution_id, actual_execution_id
                )));
            }
        }
    }

    match &message {
        WorkflowStepProtocolMessage::ReviewResult {
            summary,
            acceptance_results,
            evidence,
            ..
        }
        | WorkflowStepProtocolMessage::ResultReviewResult {
            summary,
            acceptance_results,
            evidence,
            ..
        } => validate_structured_review_fields(summary, acceptance_results, evidence)?,
        _ => {}
    }

    Ok(message)
}

fn validate_structured_review_fields(
    summary: &str,
    acceptance_results: &[WorkflowAcceptanceResult],
    evidence: &[String],
) -> Result<(), WorkflowRuntimeError> {
    if summary.trim().is_empty() {
        return Err(WorkflowRuntimeError::Validation(
            "structured review summary 不能为空".to_string(),
        ));
    }
    if acceptance_results.is_empty()
        || acceptance_results.iter().any(|item| {
            item.criterion.trim().is_empty()
                || item.evidence.trim().is_empty()
                || !matches!(item.verdict.as_str(), "passed" | "failed" | "not_applicable")
        })
    {
        return Err(WorkflowRuntimeError::Validation(
            "structured review acceptance_results 非法".to_string(),
        ));
    }
    if evidence.is_empty() || evidence.iter().any(|item| item.trim().is_empty()) {
        return Err(WorkflowRuntimeError::Validation(
            "structured review evidence 不能为空".to_string(),
        ));
    }
    Ok(())
}

pub fn parse_review_protocol_output(
    execution_id: Uuid,
    step_key: &str,
    raw_output: &str,
) -> Result<WorkflowReviewProtocolMessage, WorkflowRuntimeError> {
    tracing::debug!(
        "解析 review protocol 输出，execution_id: {}, step_key: {}, raw_output: {}",
        execution_id,
        step_key,
        raw_output
    );

    let payload = extract_json_payload(raw_output).ok_or_else(|| {
        WorkflowRuntimeError::Validation("review 输出中未找到 JSON 对象".to_string())
    })?;

    let message: WorkflowReviewProtocolMessage = serde_json::from_str(&payload)?;
    match &message {
        WorkflowReviewProtocolMessage::ReviewResult {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            feedback,
            acceptance_results,
            evidence,
            ..
        } => {
            if actual_step_key != step_key {
                return Err(WorkflowRuntimeError::Validation(format!(
                    "review protocol 的 step_key 非法，期望 '{}'，实际 '{}'",
                    step_key, actual_step_key
                )));
            }
            if actual_execution_id != &execution_id.to_string() {
                return Err(WorkflowRuntimeError::Validation(format!(
                    "review protocol 的 execution_id 非法，期望 '{}'，实际 '{}'",
                    execution_id, actual_execution_id
                )));
            }
            if feedback.trim().is_empty() {
                return Err(WorkflowRuntimeError::Validation(
                    "review protocol 的 feedback 不能为空".to_string(),
                ));
            }
            validate_structured_review_fields(feedback, acceptance_results, evidence)?;
        }
    }

    Ok(message)
}
