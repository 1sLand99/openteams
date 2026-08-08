#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowStepProtocolMessage {
    FinalResult {
        step_key: String,
        execution_id: String,
        status: WorkflowTaskCompletionStatus,
        summary: String,
        content: String,
        verification: Vec<WorkflowVerificationResult>,
        #[serde(default)]
        files_changed: Vec<String>,
        self_review: Vec<String>,
        #[serde(default)]
        issues: Vec<String>,
        evidence: Vec<String>,
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
    /// Criterion tier declared by the plan. Legacy stored results without a
    /// level deserialize as `Required`.
    #[serde(default)]
    pub level: AcceptanceCriterionLevel,
    pub verdict: WorkflowAcceptanceVerdict,
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowAcceptanceVerdict {
    Passed,
    Failed,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTaskCompletionStatus {
    Done,
    DoneWithConcerns,
    Blocked,
    NeedsContext,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowVerificationStatus {
    Passed,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowVerificationResult {
    pub name: String,
    #[serde(default)]
    pub command: Option<String>,
    pub status: WorkflowVerificationStatus,
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
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

/// Extracts the LAST balanced JSON object in the output. Plan generation uses
/// two-phase output (Markdown draft followed by the final plan JSON), so the
/// final object is the one that matters. Drafts must not contain complete
/// JSON objects; if they do, extraction still yields the last one.
pub fn extract_last_json_payload(raw_output: &str) -> Option<String> {
    let trimmed = raw_output.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed.to_string());
    }

    let bytes = trimmed.as_bytes();
    let mut last_span: Option<(usize, usize)> = None;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'{' {
            if let Some(end) = balanced_object_end(trimmed, index) {
                last_span = Some((index, end));
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }
    last_span.map(|(start, end)| trimmed[start..=end].to_string())
}

fn balanced_object_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Parses a two-phase plan generation output: takes the last complete JSON
/// object and deserializes it into the plan. Structural validation and
/// compilation happen downstream (validator + compiler).
pub fn parse_plan_output(raw_output: &str) -> Result<WorkflowPlanJson, WorkflowRuntimeError> {
    let payload = extract_last_json_payload(raw_output).ok_or_else(|| {
        WorkflowRuntimeError::Validation(
            "计划输出中未找到完整 JSON 对象；两阶段输出的草案部分不得包含完整 JSON 对象"
                .to_string(),
        )
    })?;
    Ok(serde_json::from_str(&payload)?)
}

fn acceptance_results_schema_def() -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "minItems": 1,
        "items": {
            "type": "object",
            "required": ["criterion", "level", "verdict", "evidence"],
            "additionalProperties": false,
            "properties": {
                "criterion": { "type": "string", "minLength": 1 },
                "level": { "enum": ["required", "partial", "recommended"] },
                "verdict": { "enum": ["passed", "failed", "not_applicable"] },
                "evidence": { "type": "string", "minLength": 1 }
            }
        }
    })
}

pub fn workflow_step_protocol_json_schema(
    execution_id: Uuid,
    step_key: &str,
    allow_interaction_requests: bool,
) -> String {
    let mut variants = vec![
        serde_json::json!({
            "type": "object",
            "required": ["type", "step_key", "execution_id", "status", "summary", "content", "verification", "self_review", "evidence"],
            "additionalProperties": false,
            "properties": {
                "type": { "const": "final_result" },
                "step_key": { "const": step_key },
                "execution_id": { "const": execution_id.to_string() },
                "status": { "enum": ["done", "done_with_concerns", "blocked", "needs_context"] },
                "summary": { "type": "string", "minLength": 1 },
                "content": { "type": "string" },
                "verification": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "required": ["name", "status", "evidence"],
                        "additionalProperties": false,
                        "properties": {
                            "name": { "type": "string", "minLength": 1 },
                            "command": { "type": ["string", "null"] },
                            "status": { "enum": ["passed", "failed", "not_run"] },
                            "evidence": { "type": "string", "minLength": 1 }
                        }
                    }
                },
                "files_changed": { "type": "array", "items": { "type": "string" }, "default": [] },
                "self_review": { "type": "array", "minItems": 1, "items": { "type": "string", "minLength": 1 } },
                "issues": { "type": "array", "items": { "type": "string", "minLength": 1 }, "default": [] },
                "evidence": { "$ref": "#/$defs/evidence" },
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
                "risks": { "type": "array", "items": { "type": "string", "minLength": 1 }, "default": [] },
                "unfinished_items": { "type": "array", "items": { "type": "string", "minLength": 1 }, "default": [] }
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
                "risks": { "type": "array", "items": { "type": "string", "minLength": 1 }, "default": [] },
                "unfinished_items": { "type": "array", "items": { "type": "string", "minLength": 1 }, "default": [] }
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
            "acceptance_results": acceptance_results_schema_def(),
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
        WorkflowStepType::Task => "final_result",
        WorkflowStepType::Review => "review_result",
        WorkflowStepType::Result => "result_review_result",
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
    // Drop the acceptance_results $def when no remaining variant references it
    // (e.g. task steps), so each scenario's schema only carries what it uses.
    let references_acceptance = variants.iter().any(|variant| {
        serde_json::to_string(variant)
            .map(|text| text.contains("#/$defs/acceptance_results"))
            .unwrap_or(false)
    });
    if !references_acceptance {
        if let Some(defs) = schema.get_mut("$defs").and_then(|value| value.as_object_mut()) {
            defs.remove("acceptance_results");
        }
    }
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
            "risks": { "type": "array", "items": { "type": "string", "minLength": 1 }, "default": [] },
            "unfinished_items": { "type": "array", "items": { "type": "string", "minLength": 1 }, "default": [] }
        },
        "$defs": {
            "acceptance_results": acceptance_results_schema_def(),
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
    let data = PromptDataBuilder::new()
        .add("protocol_parse_error", error)
        .add("previous_workflow_request", previous_input)
        .add("previous_invalid_response", previous_output)
        .build();

    let prompt = format!(
        r#"Your previous workflow {protocol_name} response did not match the required JSON protocol.
Error: {parse_error}

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
        parse_error = data.get("protocol_parse_error"),
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
        WorkflowStepProtocolMessage::FinalResult {
            status,
            summary,
            verification,
            self_review,
            issues,
            evidence,
            ..
        } => validate_task_result_fields(
            *status,
            summary,
            verification,
            self_review,
            issues,
            evidence,
        )?,
        WorkflowStepProtocolMessage::ReviewResult {
            summary,
            acceptance_results,
            evidence,
            risks,
            unfinished_items,
            ..
        } => validate_structured_review_fields(
            summary,
            acceptance_results,
            evidence,
            risks,
            unfinished_items,
        )?,
        WorkflowStepProtocolMessage::ResultReviewResult {
            overall_status,
            summary,
            acceptance_results,
            evidence,
            risks,
            unfinished_items,
            ..
        } => validate_structured_result_fields(
            overall_status,
            summary,
            acceptance_results,
            evidence,
            risks,
            unfinished_items,
        )?,
        _ => {}
    }

    Ok(message)
}

/// Dedicated task parse entry (design §12.1): accepts `final_result` plus the
/// interaction/error variants, and rejects review/result success types at
/// deserialization time instead of filtering a shared union afterwards.
pub fn parse_task_protocol_output(
    execution_id: Uuid,
    step_key: &str,
    raw_output: &str,
) -> Result<WorkflowStepProtocolMessage, WorkflowRuntimeError> {
    let message = parse_step_protocol_output(execution_id, step_key, raw_output)?;
    let valid_type = matches!(
        &message,
        WorkflowStepProtocolMessage::FinalResult { .. }
            | WorkflowStepProtocolMessage::Error { .. }
            | WorkflowStepProtocolMessage::ApprovalRequest { .. }
            | WorkflowStepProtocolMessage::PermissionRequest { .. }
            | WorkflowStepProtocolMessage::ContinueConfirmation { .. }
            | WorkflowStepProtocolMessage::InputRequest { .. }
    );
    if !valid_type {
        return Err(WorkflowRuntimeError::Validation(
            "task step returned an incompatible success protocol message".to_string(),
        ));
    }
    Ok(message)
}

pub fn parse_step_protocol_output_for_step(
    execution_id: Uuid,
    step_key: &str,
    step_type: &WorkflowStepType,
    declared_acceptance: &[(AcceptanceCriterionLevel, String)],
    raw_output: &str,
) -> Result<WorkflowStepProtocolMessage, WorkflowRuntimeError> {
    let message = parse_step_protocol_output(execution_id, step_key, raw_output)?;
    let valid_type = matches!(
        (step_type, &message),
        (WorkflowStepType::Task, WorkflowStepProtocolMessage::FinalResult { .. })
            | (WorkflowStepType::Review, WorkflowStepProtocolMessage::ReviewResult { .. })
            | (WorkflowStepType::Result, WorkflowStepProtocolMessage::ResultReviewResult { .. })
            | (_, WorkflowStepProtocolMessage::Error { .. })
            | (_, WorkflowStepProtocolMessage::ApprovalRequest { .. })
            | (_, WorkflowStepProtocolMessage::PermissionRequest { .. })
            | (_, WorkflowStepProtocolMessage::ContinueConfirmation { .. })
            | (_, WorkflowStepProtocolMessage::InputRequest { .. })
    );
    if !valid_type {
        return Err(WorkflowRuntimeError::Validation(format!(
            "step type '{:?}' returned an incompatible success protocol message",
            step_type
        )));
    }
    match &message {
        WorkflowStepProtocolMessage::ReviewResult {
            verdict,
            summary,
            acceptance_results,
            evidence,
            risks,
            unfinished_items,
            ..
        } => {
            if matches!(verdict, ReviewVerdict::Approved) {
                validate_approved_required_acceptance_coverage(
                    declared_acceptance,
                    acceptance_results,
                )?;
            }
            validate_structured_review_fields(
                summary,
                acceptance_results,
                evidence,
                risks,
                unfinished_items,
            )?;
        }
        WorkflowStepProtocolMessage::ResultReviewResult {
            acceptance_results, ..
        } => validate_acceptance_coverage(declared_acceptance, acceptance_results)?,
        _ => {}
    }
    Ok(message)
}

fn validate_task_result_fields(
    status: WorkflowTaskCompletionStatus,
    summary: &str,
    verification: &[WorkflowVerificationResult],
    self_review: &[String],
    issues: &[String],
    evidence: &[String],
) -> Result<(), WorkflowRuntimeError> {
    if summary.trim().is_empty()
        || verification.is_empty()
        || self_review.is_empty()
        || evidence.is_empty()
        || verification.iter().any(|item| {
            item.name.trim().is_empty()
                || item.evidence.trim().is_empty()
                || item.command.as_deref().is_some_and(|value| value.trim().is_empty())
        })
        || self_review.iter().any(|item| item.trim().is_empty())
        || issues.iter().any(|item| item.trim().is_empty())
        || evidence.iter().any(|item| item.trim().is_empty())
    {
        return Err(WorkflowRuntimeError::Validation(
            "task final_result structured report fields are invalid".to_string(),
        ));
    }
    let has_failed_or_unrun = verification.iter().any(|item| {
        matches!(
            item.status,
            WorkflowVerificationStatus::Failed | WorkflowVerificationStatus::NotRun
        )
    });
    if matches!(status, WorkflowTaskCompletionStatus::Done)
        && (has_failed_or_unrun || !issues.is_empty())
    {
        return Err(WorkflowRuntimeError::Validation(
            "status=done cannot include failed/not_run verification or issues".to_string(),
        ));
    }
    if matches!(status, WorkflowTaskCompletionStatus::DoneWithConcerns)
        && !has_failed_or_unrun
        && issues.is_empty()
    {
        return Err(WorkflowRuntimeError::Validation(
            "status=done_with_concerns must identify an issue or incomplete verification"
                .to_string(),
        ));
    }
    if matches!(
        status,
        WorkflowTaskCompletionStatus::Blocked | WorkflowTaskCompletionStatus::NeedsContext
    ) && issues.is_empty()
    {
        return Err(WorkflowRuntimeError::Validation(
            "blocked/needs_context final_result must explain at least one issue".to_string(),
        ));
    }
    Ok(())
}

fn validate_structured_review_fields(
    summary: &str,
    acceptance_results: &[WorkflowAcceptanceResult],
    evidence: &[String],
    risks: &[String],
    unfinished_items: &[String],
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
    if unfinished_items.iter().any(|item| item.trim().is_empty())
        || risks.iter().any(|item| item.trim().is_empty())
    {
        return Err(WorkflowRuntimeError::Validation(
            "structured review risks/unfinished_items may not contain blank entries".to_string(),
        ));
    }
    Ok(())
}

fn validate_structured_result_fields(
    overall_status: &WorkflowResultOverallStatus,
    summary: &str,
    acceptance_results: &[WorkflowAcceptanceResult],
    evidence: &[String],
    risks: &[String],
    unfinished_items: &[String],
) -> Result<(), WorkflowRuntimeError> {
    if summary.trim().is_empty()
        || evidence.is_empty()
        || evidence.iter().any(|item| item.trim().is_empty())
    {
        return Err(WorkflowRuntimeError::Validation(
            "structured result fields are invalid".to_string(),
        ));
    }
    if acceptance_results
        .iter()
        .any(|item| item.criterion.trim().is_empty() || item.evidence.trim().is_empty())
    {
        return Err(WorkflowRuntimeError::Validation(
            "structured result acceptance_results are invalid".to_string(),
        ));
    }
    let has_failed = acceptance_results
        .iter()
        .any(|item| matches!(item.verdict, WorkflowAcceptanceVerdict::Failed));
    let has_required_failed = acceptance_results.iter().any(|item| {
        matches!(item.verdict, WorkflowAcceptanceVerdict::Failed)
            && item.level == AcceptanceCriterionLevel::Required
    });
    let completed = matches!(overall_status, WorkflowResultOverallStatus::Completed);
    if completed && (has_required_failed || !unfinished_items.is_empty()) {
        return Err(WorkflowRuntimeError::Validation(
            "overall_status=completed cannot contain failed required criteria or unfinished work"
                .to_string(),
        ));
    }
    if completed && !risks.is_empty() {
        return Err(WorkflowRuntimeError::Validation(
            "overall_status=completed cannot contain unresolved risks".to_string(),
        ));
    }
    if risks.iter().any(|item| item.trim().is_empty())
        || unfinished_items.iter().any(|item| item.trim().is_empty())
    {
        return Err(WorkflowRuntimeError::Validation(
            "structured result risks and unfinished_items may not contain blank entries"
                .to_string(),
        ));
    }
    if matches!(overall_status, WorkflowResultOverallStatus::CompletedWithConcerns)
        && risks.is_empty()
        && unfinished_items.is_empty()
        && !has_failed
    {
        return Err(WorkflowRuntimeError::Validation(
            "overall_status=completed_with_concerns must identify a risk, failed criterion, or unfinished item"
                .to_string(),
        ));
    }
    if matches!(overall_status, WorkflowResultOverallStatus::Blocked)
        && unfinished_items.is_empty()
        && !has_failed
    {
        return Err(WorkflowRuntimeError::Validation(
            "overall_status=blocked must identify failed or unfinished work".to_string(),
        ));
    }
    Ok(())
}

fn validate_approved_required_acceptance_coverage(
    declared_acceptance: &[(AcceptanceCriterionLevel, String)],
    results: &[WorkflowAcceptanceResult],
) -> Result<(), WorkflowRuntimeError> {
    let required_acceptance = declared_acceptance
        .iter()
        .filter(|(level, criterion)| {
            *level == AcceptanceCriterionLevel::Required && !criterion.trim().is_empty()
        })
        .collect::<Vec<_>>();
    if required_acceptance.is_empty() {
        return Ok(());
    }
    let returned_required_count = results
        .iter()
        .filter(|result| result.level == AcceptanceCriterionLevel::Required)
        .count();
    if returned_required_count != required_acceptance.len() {
        return Err(WorkflowRuntimeError::Validation(format!(
            "approved review required acceptance result count mismatch: expected {}, actual {}",
            required_acceptance.len(),
            returned_required_count
        )));
    }
    Ok(())
}

fn validate_acceptance_coverage(
    declared_acceptance: &[(AcceptanceCriterionLevel, String)],
    results: &[WorkflowAcceptanceResult],
) -> Result<(), WorkflowRuntimeError> {
    let normalize = |value: &str| value.split_whitespace().collect::<Vec<_>>().join(" ");
    let declared = declared_acceptance
        .iter()
        .map(|(level, item)| (*level, normalize(item)))
        .filter(|(_, item)| !item.is_empty())
        .collect::<Vec<_>>();
    if !declared.is_empty() && results.len() != declared.len() {
        return Err(WorkflowRuntimeError::Validation(format!(
            "acceptance result count mismatch: expected {}, actual {}",
            declared.len(),
            results.len()
        )));
    }
    let normalized_results = results
        .iter()
        .map(|result| (result.level, normalize(&result.criterion)))
        .collect::<Vec<_>>();
    if normalized_results
        .iter()
        .any(|(_, criterion)| criterion.is_empty())
    {
        return Err(WorkflowRuntimeError::Validation(
            "acceptance result criterion may not be blank".to_string(),
        ));
    }
    let unique_results = normalized_results.iter().collect::<std::collections::HashSet<_>>();
    if unique_results.len() != normalized_results.len() {
        return Err(WorkflowRuntimeError::Validation(
            "acceptance results contain duplicate criteria".to_string(),
        ));
    }
    if !declared.is_empty()
        && normalized_results
            .iter()
            .any(|item| !declared.contains(item))
    {
        return Err(WorkflowRuntimeError::Validation(
            "acceptance results contain an undeclared criterion or wrong level".to_string(),
        ));
    }
    for criterion in declared {
        let count = normalized_results
            .iter()
            .filter(|item| *item == &criterion)
            .count();
        if count != 1 {
            return Err(WorkflowRuntimeError::Validation(format!(
                "declared acceptance criterion must be covered exactly once: {}",
                criterion.1
            )));
        }
    }
    Ok(())
}

pub fn parse_review_protocol_output(
    execution_id: Uuid,
    step_key: &str,
    declared_acceptance: &[(AcceptanceCriterionLevel, String)],
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
            verdict,
            acceptance_results,
            evidence,
            risks,
            unfinished_items,
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
            if matches!(verdict, ReviewVerdict::Approved) {
                validate_approved_required_acceptance_coverage(
                    declared_acceptance,
                    acceptance_results,
                )?;
            }
            validate_structured_review_fields(
                feedback,
                acceptance_results,
                evidence,
                risks,
                unfinished_items,
            )?;
        }
    }

    Ok(message)
}
