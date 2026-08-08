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

pub fn task_protocol_json_schema(
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

pub fn step_review_protocol_json_schema(execution_id: Uuid, step_key: &str) -> String {
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

pub fn should_retry_workflow_protocol_parse_failure(raw_output: &str) -> bool {
    !raw_output.trim().is_empty()
}

/// Dedicated task parse entry (design §12.1): accepts `final_result` plus the
/// interaction/error variants. Review/result success discriminators fail
/// during deserialization because they are not members of this enum.
pub fn parse_task_protocol_output(
    execution_id: Uuid,
    step_key: &str,
    raw_output: &str,
) -> Result<WorkflowStepProtocolMessage, WorkflowRuntimeError> {
    let payload = extract_json_payload(raw_output).ok_or_else(|| {
        WorkflowRuntimeError::Validation("task 输出中未找到 JSON 对象".to_string())
    })?;
    let message: WorkflowStepProtocolMessage = serde_json::from_str(&payload)?;

    match &message {
        WorkflowStepProtocolMessage::FinalResult {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            status,
            summary,
            verification,
            self_review,
            issues,
            evidence,
            ..
        } => {
            validate_protocol_identity(
                execution_id,
                step_key,
                actual_execution_id,
                actual_step_key,
                "task",
            )?;
            validate_task_result_fields(
                *status,
                summary,
                verification,
                self_review,
                issues,
                evidence,
            )?;
        }
        WorkflowStepProtocolMessage::Error {
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
        } => validate_protocol_identity(
            execution_id,
            step_key,
            actual_execution_id,
            actual_step_key,
            "task",
        )?,
    }
    Ok(message)
}

fn validate_protocol_identity(
    execution_id: Uuid,
    step_key: &str,
    actual_execution_id: &str,
    actual_step_key: &str,
    protocol_name: &str,
) -> Result<(), WorkflowRuntimeError> {
    if actual_step_key != step_key {
        return Err(WorkflowRuntimeError::Validation(format!(
            "{protocol_name} protocol 的 step_key 非法，期望 '{step_key}'，实际 '{actual_step_key}'"
        )));
    }
    if actual_execution_id != execution_id.to_string() {
        return Err(WorkflowRuntimeError::Validation(format!(
            "{protocol_name} protocol 的 execution_id 非法，期望 '{execution_id}'，实际 '{actual_execution_id}'"
        )));
    }
    Ok(())
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

fn validate_review_verdict_consistency(
    verdict: &ReviewVerdict,
    results: &[WorkflowAcceptanceResult],
    risks: &[String],
) -> Result<(), WorkflowRuntimeError> {
    if *verdict == ReviewVerdict::Approved
        && results.iter().any(|result| {
            result.level == AcceptanceCriterionLevel::Required
                && result.verdict == WorkflowAcceptanceVerdict::Failed
        })
    {
        return Err(WorkflowRuntimeError::Validation(
            "approved review cannot contain a failed required criterion".to_string(),
        ));
    }
    if results.iter().any(|result| {
        result.level == AcceptanceCriterionLevel::Partial
            && result.verdict == WorkflowAcceptanceVerdict::Failed
    }) && risks.is_empty()
    {
        return Err(WorkflowRuntimeError::Validation(
            "failed partial criteria require an externally attributable risk".to_string(),
        ));
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

pub fn parse_step_review_protocol_output(
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
            validate_acceptance_coverage(declared_acceptance, acceptance_results)?;
            validate_review_verdict_consistency(verdict, acceptance_results, risks)?;
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
