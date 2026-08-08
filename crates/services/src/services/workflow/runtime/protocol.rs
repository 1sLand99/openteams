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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowReviewProtocolMessage {
    ReviewResult {
        step_key: String,
        execution_id: String,
        summary: String,
        results: std::collections::BTreeMap<String, WorkflowReviewCriterionResult>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowReviewCriterion {
    pub id: String,
    pub level: AcceptanceCriterionLevel,
    pub criterion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowReviewCriterionResult {
    pub passed: bool,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DerivedWorkflowReview {
    pub verdict: ReviewVerdict,
    pub acceptance_results: Vec<WorkflowAcceptanceResult>,
    pub evidence: Vec<String>,
    pub risks: Vec<String>,
    pub unfinished_items: Vec<String>,
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

pub fn step_review_protocol_json_schema(
    execution_id: Uuid,
    step_key: &str,
    criteria: &[WorkflowReviewCriterion],
) -> String {
    let result_ids = criteria
        .iter()
        .map(|criterion| criterion.id.clone())
        .collect::<Vec<_>>();
    let result_properties = criteria
        .iter()
        .map(|criterion| {
            (
                criterion.id.clone(),
                serde_json::json!({
                    "type": "object",
                    "required": ["passed", "evidence"],
                    "additionalProperties": false,
                    "properties": {
                        "passed": { "type": "boolean" },
                        "evidence": { "type": "string", "pattern": "\\S" }
                    }
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();

    serde_json::to_string_pretty(&serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["type", "step_key", "execution_id", "summary", "results"],
        "additionalProperties": false,
        "properties": {
            "type": { "const": "review_result" },
            "step_key": { "const": step_key },
            "execution_id": { "const": execution_id.to_string() },
            "summary": { "type": "string", "pattern": "\\S" },
            "results": {
                "type": "object",
                "required": result_ids,
                "additionalProperties": false,
                "properties": result_properties
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

pub fn build_workflow_review_criteria(
    declared_acceptance: &[(AcceptanceCriterionLevel, String)],
    fallback_criterion: Option<&str>,
) -> Vec<WorkflowReviewCriterion> {
    let mut criteria = declared_acceptance
        .iter()
        .filter(|(_, criterion)| !criterion.trim().is_empty())
        .enumerate()
        .map(|(index, (level, criterion))| WorkflowReviewCriterion {
            id: format!("c{}", index + 1),
            level: *level,
            criterion: criterion.clone(),
        })
        .collect::<Vec<_>>();
    if criteria.is_empty()
        && let Some(criterion) = fallback_criterion.filter(|value| !value.trim().is_empty())
    {
        criteria.push(WorkflowReviewCriterion {
            id: "c1".to_string(),
            level: AcceptanceCriterionLevel::Required,
            criterion: criterion.trim().to_string(),
        });
    }
    criteria
}

pub fn derive_workflow_review(
    criteria: &[WorkflowReviewCriterion],
    results: &std::collections::BTreeMap<String, WorkflowReviewCriterionResult>,
) -> DerivedWorkflowReview {
    let acceptance_results = criteria
        .iter()
        .map(|criterion| {
            let result = &results[&criterion.id];
            WorkflowAcceptanceResult {
                criterion: criterion.criterion.clone(),
                level: criterion.level,
                verdict: if result.passed {
                    WorkflowAcceptanceVerdict::Passed
                } else {
                    WorkflowAcceptanceVerdict::Failed
                },
                evidence: result.evidence.clone(),
            }
        })
        .collect::<Vec<_>>();
    let verdict = if criteria.iter().any(|criterion| {
        criterion.level == AcceptanceCriterionLevel::Required
            && results
                .get(&criterion.id)
                .is_none_or(|result| !result.passed)
    }) {
        ReviewVerdict::Rejected
    } else {
        ReviewVerdict::Approved
    };
    let evidence = criteria
        .iter()
        .map(|criterion| results[&criterion.id].evidence.clone())
        .collect::<Vec<_>>();
    let risks = criteria
        .iter()
        .filter(|criterion| {
            criterion.level == AcceptanceCriterionLevel::Partial
                && !results[&criterion.id].passed
        })
        .map(|criterion| results[&criterion.id].evidence.clone())
        .collect::<Vec<_>>();
    let unfinished_items = criteria
        .iter()
        .filter(|criterion| {
            criterion.level == AcceptanceCriterionLevel::Required
                && !results[&criterion.id].passed
        })
        .map(|criterion| criterion.criterion.clone())
        .collect::<Vec<_>>();

    DerivedWorkflowReview {
        verdict,
        acceptance_results,
        evidence,
        risks,
        unfinished_items,
    }
}

pub fn parse_step_review_protocol_output(
    execution_id: Uuid,
    step_key: &str,
    criteria: &[WorkflowReviewCriterion],
    raw_output: &str,
) -> Result<WorkflowReviewProtocolMessage, WorkflowRuntimeError> {
    tracing::debug!(
        "解析 review protocol 输出，execution_id: {}, step_key: {}, raw_output: {}",
        execution_id,
        step_key,
        raw_output
    );

    let message: WorkflowReviewProtocolMessage = serde_json::from_str(raw_output.trim())?;
    match &message {
        WorkflowReviewProtocolMessage::ReviewResult {
            step_key: actual_step_key,
            execution_id: actual_execution_id,
            summary,
            results,
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
            if summary.trim().is_empty() {
                return Err(WorkflowRuntimeError::Validation(
                    "review protocol 的 summary 不能为空".to_string(),
                ));
            }
            let expected_ids = criteria
                .iter()
                .map(|criterion| criterion.id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let actual_ids = results
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>();
            if actual_ids != expected_ids {
                let missing = expected_ids
                    .difference(&actual_ids)
                    .copied()
                    .collect::<Vec<_>>();
                let extra = actual_ids
                    .difference(&expected_ids)
                    .copied()
                    .collect::<Vec<_>>();
                return Err(WorkflowRuntimeError::Validation(format!(
                    "review results 与验收清单不一致（缺少 {missing:?}，多余 {extra:?}）"
                )));
            }
            if let Some((id, _)) = results
                .iter()
                .find(|(_, result)| result.evidence.trim().is_empty())
            {
                return Err(WorkflowRuntimeError::Validation(format!(
                    "review result '{id}' 的 evidence 不能为空"
                )));
            }
        }
    }

    Ok(message)
}
