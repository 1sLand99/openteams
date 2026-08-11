use std::{borrow::Cow, collections::HashSet, fmt, sync::OnceLock};

use db::models::workflow_types::WorkflowPlanJson;
use executors::env::ExecutionEnv;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::services::{
    chat_runner::{
        AgentProtocolError, AgentProtocolMessage, AgentProtocolMessageType, ChatProtocolNoticeCode,
        ChatRunner, PROTOCOL_OUTPUT_SCHEMA_JSON, PROTOCOL_OUTPUT_SCHEMA_JSON_WORKFLOW_PLAN,
    },
    workflow_compiler::WorkflowCompiler,
    workflow_loop_executor::protocol::{
        LoopReviewCriterion, LoopReviewProtocolMessage, loop_review_protocol_json_schema,
        parse_loop_review_protocol_output,
    },
    workflow_runtime::{
        WorkflowReviewCriterion, WorkflowReviewProtocolMessage, WorkflowRuntimeError,
        WorkflowStepProtocolMessage, extract_json_payload, extract_last_json_payload,
        parse_plan_output, parse_step_review_protocol_output, parse_task_protocol_output,
        prompt_builders::plan_generation::plan_output_schema, step_review_protocol_json_schema,
        task_protocol_json_schema,
    },
    workflow_validator,
};

pub const OUTPUT_VALIDATION_URL_ENV: &str = "OPENTEAMS_OUTPUT_VALIDATION_URL";
pub const OUTPUT_VALIDATION_ROUTE: &str = "/api/output-validation";
pub const MAX_OUTPUT_VALIDATION_REQUEST_BYTES: usize = 4 * 1024 * 1024;

const MAX_REPORTED_ERRORS: usize = 32;

static OUTPUT_VALIDATION_URL: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputValidationKind {
    ChatProtocol,
    ChatWorkflowProtocol,
    WorkflowPlan,
    WorkflowTask,
    WorkflowStepReview,
    WorkflowLoopReview,
}

impl OutputValidationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatProtocol => "chat_protocol",
            Self::ChatWorkflowProtocol => "chat_workflow_protocol",
            Self::WorkflowPlan => "workflow_plan",
            Self::WorkflowTask => "workflow_task",
            Self::WorkflowStepReview => "workflow_step_review",
            Self::WorkflowLoopReview => "workflow_loop_review",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatProtocolValidationContext {
    pub allowed_targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatWorkflowProtocolValidationContext {
    pub allowed_targets: Vec<String>,
    pub workflow_generation_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPlanValidationContext {
    pub lead_agent_id: String,
    pub available_agent_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTaskValidationContext {
    pub execution_id: Uuid,
    pub step_key: String,
    pub allow_interaction_requests: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStepReviewValidationContext {
    pub execution_id: Uuid,
    pub step_key: String,
    pub criteria: Vec<WorkflowReviewCriterion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowLoopReviewValidationContext {
    pub execution_id: Uuid,
    pub loop_key: String,
    pub criteria: Vec<LoopReviewCriterion>,
    pub allowed_step_keys: Vec<String>,
}

/// One local HTTP contract for every model-produced structured output.
///
/// `output` may be either the candidate JSON value itself or a string holding
/// the exact raw model output. Supporting both forms keeps shell callers
/// simple while letting runtime consumers validate their unmodified output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputValidationRequest {
    ChatProtocol {
        output: Value,
        context: ChatProtocolValidationContext,
    },
    ChatWorkflowProtocol {
        output: Value,
        context: ChatWorkflowProtocolValidationContext,
    },
    WorkflowPlan {
        output: Value,
        context: WorkflowPlanValidationContext,
    },
    WorkflowTask {
        output: Value,
        context: WorkflowTaskValidationContext,
    },
    WorkflowStepReview {
        output: Value,
        context: WorkflowStepReviewValidationContext,
    },
    WorkflowLoopReview {
        output: Value,
        context: WorkflowLoopReviewValidationContext,
    },
}

impl OutputValidationRequest {
    pub const fn kind(&self) -> OutputValidationKind {
        match self {
            Self::ChatProtocol { .. } => OutputValidationKind::ChatProtocol,
            Self::ChatWorkflowProtocol { .. } => OutputValidationKind::ChatWorkflowProtocol,
            Self::WorkflowPlan { .. } => OutputValidationKind::WorkflowPlan,
            Self::WorkflowTask { .. } => OutputValidationKind::WorkflowTask,
            Self::WorkflowStepReview { .. } => OutputValidationKind::WorkflowStepReview,
            Self::WorkflowLoopReview { .. } => OutputValidationKind::WorkflowLoopReview,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputValidationError {
    pub code: String,
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputValidationResponse {
    pub valid: bool,
    pub kind: OutputValidationKind,
    pub errors: Vec<OutputValidationError>,
}

#[derive(Debug, Clone)]
pub(crate) struct OutputValidationFailure {
    pub(crate) errors: Vec<OutputValidationError>,
}

impl OutputValidationFailure {
    fn one(code: &str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            errors: vec![OutputValidationError {
                code: code.to_string(),
                path: path.into(),
                message: message.into(),
            }],
        }
    }
}

impl fmt::Display for OutputValidationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = self
            .errors
            .iter()
            .map(|error| {
                if error.path.is_empty() || error.path == "/" {
                    format!("{}: {}", error.code, error.message)
                } else {
                    format!("{} at {}: {}", error.code, error.path, error.message)
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        formatter.write_str(&message)
    }
}

impl std::error::Error for OutputValidationFailure {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputValidationReturnMode {
    JsonOnly,
    PlanTwoPhase,
}

pub fn configure_output_validation_url(url: impl Into<String>) -> Result<(), String> {
    let url = url.into();
    let trimmed = url.trim();
    let parsed = url::Url::parse(trimmed)
        .map_err(|error| format!("invalid output validation URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("output validation URL must be an absolute HTTP(S) URL".to_string());
    }
    OUTPUT_VALIDATION_URL
        .set(trimmed.to_string())
        .map_err(|_| "output validation URL is already configured".to_string())
}

pub(crate) fn inject_output_validation_url(env: &mut ExecutionEnv) {
    if let Some(url) = OUTPUT_VALIDATION_URL.get() {
        env.insert(OUTPUT_VALIDATION_URL_ENV, url.clone());
    }
}

pub(crate) fn render_output_validation_instructions<T: Serialize>(
    kind: OutputValidationKind,
    context: &T,
    return_mode: OutputValidationReturnMode,
) -> String {
    let context_json = serde_json::to_string_pretty(context).unwrap_or_else(|_| "{}".to_string());
    let (validated_instruction, exhausted_instruction) = match return_mode {
        OutputValidationReturnMode::JsonOnly => (
            "If any response has `valid: true`, return the exact validated candidate JSON, unchanged, as the final output.",
            "After the third retry, if validation still has not returned `valid: true`, stop validating and return the current candidate JSON directly as the final output.",
        ),
        OutputValidationReturnMode::PlanTwoPhase => (
            "If any response has `valid: true`, finish the two-phase response and place the exact validated candidate JSON object, unchanged, at the end.",
            "After the third retry, if validation still has not returned `valid: true`, stop validating, finish the two-phase response, and place the current candidate JSON object directly at the end.",
        ),
    };

    format!(
        r#"## Mandatory output validation

Before returning the final answer, you MUST validate the complete candidate output through the local OpenTeams HTTP validator.

1. Build the complete candidate JSON required by the schema above.
2. Send `POST $OPENTEAMS_OUTPUT_VALIDATION_URL` with `Content-Type: application/json` and the request body below. Replace `null` with the candidate JSON value itself; alternatively, set `output` to a JSON string containing the exact raw candidate output.
3. If the request fails or the response has `valid: false`, fix every reported error you received and validate the full candidate again. You may retry validation at most 3 times after the initial request (4 total validation requests).
4. {validated_instruction}
5. {exhausted_instruction}

```text
{{
  "kind": "{}",
  "output": null,
  "context": {}
}}
```"#,
        kind.as_str(),
        context_json
    )
}

pub fn validate_output_request(request: &OutputValidationRequest) -> OutputValidationResponse {
    let kind = request.kind();
    let result =
        match request {
            OutputValidationRequest::ChatProtocol { output, context } => raw_output(output)
                .and_then(|raw| {
                    validate_chat_protocol_output(&raw, &context.allowed_targets, false).map(|_| ())
                }),
            OutputValidationRequest::ChatWorkflowProtocol { output, context } => raw_output(output)
                .and_then(|raw| {
                    validate_chat_protocol_output(
                        &raw,
                        &context.allowed_targets,
                        context.workflow_generation_allowed,
                    )
                    .map(|_| ())
                }),
            OutputValidationRequest::WorkflowPlan { output, context } => raw_output(output)
                .and_then(|raw| {
                    validate_workflow_plan_output(
                        &raw,
                        &context.lead_agent_id,
                        &context.available_agent_ids,
                    )
                    .map(|_| ())
                }),
            OutputValidationRequest::WorkflowTask { output, context } => raw_output(output)
                .and_then(|raw| {
                    validate_workflow_task_output(
                        context.execution_id,
                        &context.step_key,
                        context.allow_interaction_requests,
                        &raw,
                    )
                    .map(|_| ())
                }),
            OutputValidationRequest::WorkflowStepReview { output, context } => raw_output(output)
                .and_then(|raw| {
                    validate_workflow_step_review_output(
                        context.execution_id,
                        &context.step_key,
                        &context.criteria,
                        &raw,
                    )
                    .map(|_| ())
                }),
            OutputValidationRequest::WorkflowLoopReview { output, context } => raw_output(output)
                .and_then(|raw| {
                    validate_workflow_loop_review_output(
                        context.execution_id,
                        &context.loop_key,
                        &context.criteria,
                        &context.allowed_step_keys,
                        &raw,
                    )
                    .map(|_| ())
                }),
        };

    match result {
        Ok(()) => OutputValidationResponse {
            valid: true,
            kind,
            errors: Vec::new(),
        },
        Err(failure) => OutputValidationResponse {
            valid: false,
            kind,
            errors: failure.errors,
        },
    }
}

fn raw_output(output: &Value) -> Result<Cow<'_, str>, OutputValidationFailure> {
    match output {
        Value::String(raw) => Ok(Cow::Borrowed(raw)),
        value => serde_json::to_string(value)
            .map(Cow::Owned)
            .map_err(|error| {
                OutputValidationFailure::one(
                    "invalid_output",
                    "/output",
                    format!("failed to serialize candidate output: {error}"),
                )
            }),
    }
}

pub(crate) fn validate_chat_protocol_output(
    raw_output: &str,
    allowed_targets: &[String],
    workflow_generation_allowed: bool,
) -> Result<Vec<AgentProtocolMessage>, OutputValidationFailure> {
    let candidate =
        ChatRunner::extract_json_from_content(raw_output).map_err(chat_protocol_failure)?;
    let instance = parse_json_candidate(&candidate, "chat output")?;
    let schema_text = if workflow_generation_allowed {
        PROTOCOL_OUTPUT_SCHEMA_JSON_WORKFLOW_PLAN
    } else {
        PROTOCOL_OUTPUT_SCHEMA_JSON
    };
    let schema = parse_internal_schema(schema_text, "chat protocol")?;
    validate_json_schema(&schema, &instance)?;
    let messages = ChatRunner::parse_agent_protocol_messages_from_json(&candidate)
        .map_err(chat_protocol_failure)?;
    validate_chat_protocol_context(&messages, allowed_targets, workflow_generation_allowed)?;
    Ok(messages)
}

pub(crate) fn validate_chat_protocol_context(
    messages: &[AgentProtocolMessage],
    allowed_targets: &[String],
    workflow_generation_allowed: bool,
) -> Result<(), OutputValidationFailure> {
    let allowed_targets = allowed_targets
        .iter()
        .filter_map(|target| ChatRunner::normalize_protocol_target(target))
        .map(|target| target.to_ascii_lowercase())
        .collect::<HashSet<_>>();
    let mut seen_targets = HashSet::new();
    let mut workflow_generate_count = 0usize;

    for (index, message) in messages.iter().enumerate() {
        match message.message_type {
            AgentProtocolMessageType::Send => {
                let target = message.to.as_deref().unwrap_or_default();
                let normalized = ChatRunner::normalize_protocol_target(target)
                    .unwrap_or_else(|| target.trim().to_string());
                let comparison_key = normalized.to_ascii_lowercase();
                if !allowed_targets.contains(&comparison_key) {
                    return Err(OutputValidationFailure::one(
                        "invalid_send_target",
                        format!("/{index}/to"),
                        format!("send target '{normalized}' is not available in this chat"),
                    ));
                }
                if !seen_targets.insert(comparison_key) {
                    return Err(OutputValidationFailure::one(
                        "duplicate_send_target",
                        format!("/{index}/to"),
                        format!("send target '{normalized}' may appear at most once"),
                    ));
                }
            }
            AgentProtocolMessageType::WorkflowGenerate => {
                workflow_generate_count += 1;
                if !workflow_generation_allowed {
                    return Err(OutputValidationFailure::one(
                        "workflow_generate_not_allowed",
                        format!("/{index}/type"),
                        "workflow_generate is not available for this message",
                    ));
                }
                if workflow_generate_count > 1 {
                    return Err(OutputValidationFailure::one(
                        "duplicate_workflow_generate",
                        format!("/{index}/type"),
                        "workflow_generate may appear at most once",
                    ));
                }
            }
            AgentProtocolMessageType::Record
            | AgentProtocolMessageType::Artifact
            | AgentProtocolMessageType::Conclusion => {}
        }
    }
    Ok(())
}

pub(crate) fn chat_validation_failure_as_protocol_error(
    failure: OutputValidationFailure,
) -> AgentProtocolError {
    let invalid_target = failure
        .errors
        .iter()
        .find(|error| error.code == "invalid_send_target");
    AgentProtocolError {
        code: if invalid_target.is_some() {
            ChatProtocolNoticeCode::InvalidSendTarget
        } else {
            ChatProtocolNoticeCode::InvalidJson
        },
        target: invalid_target.map(|error| error.message.clone()),
        detail: Some(failure.to_string()),
    }
}

pub(crate) fn validate_workflow_plan_output(
    raw_output: &str,
    lead_agent_id: &str,
    available_agent_ids: &[String],
) -> Result<WorkflowPlanJson, OutputValidationFailure> {
    let candidate = extract_last_json_payload(raw_output).ok_or_else(|| {
        OutputValidationFailure::one(
            "invalid_json",
            "/output",
            "workflow plan output does not contain a complete JSON object",
        )
    })?;
    let instance = parse_json_candidate(&candidate, "workflow plan")?;
    let schema = plan_output_schema(lead_agent_id, available_agent_ids);
    validate_json_schema(&schema, &instance)?;
    let plan = parse_plan_output(&candidate).map_err(workflow_runtime_failure)?;

    let validation = workflow_validator::validate_plan(&plan, available_agent_ids);
    if !validation.is_valid {
        return Err(OutputValidationFailure {
            errors: validation
                .errors
                .into_iter()
                .take(MAX_REPORTED_ERRORS)
                .map(|error| OutputValidationError {
                    code: "workflow_plan_validation".to_string(),
                    path: workflow_field_path(&error.field),
                    message: error.message,
                })
                .collect(),
        });
    }
    WorkflowCompiler::compile(&plan, available_agent_ids).map_err(|error| {
        OutputValidationFailure::one("workflow_plan_compile", "/output", error.to_string())
    })?;
    Ok(plan)
}

pub(crate) fn validate_workflow_task_output(
    execution_id: Uuid,
    step_key: &str,
    allow_interaction_requests: bool,
    raw_output: &str,
) -> Result<WorkflowStepProtocolMessage, OutputValidationFailure> {
    let candidate = extract_json_payload(raw_output).ok_or_else(|| {
        OutputValidationFailure::one(
            "invalid_json",
            "/output",
            "workflow task output does not contain a JSON object",
        )
    })?;
    let instance = parse_json_candidate(&candidate, "workflow task")?;
    let schema = parse_internal_schema(
        &task_protocol_json_schema(execution_id, step_key, allow_interaction_requests),
        "workflow task",
    )?;
    validate_json_schema(&schema, &instance)?;
    parse_task_protocol_output(execution_id, step_key, &candidate).map_err(workflow_runtime_failure)
}

pub(crate) fn validate_workflow_step_review_output(
    execution_id: Uuid,
    step_key: &str,
    criteria: &[WorkflowReviewCriterion],
    raw_output: &str,
) -> Result<WorkflowReviewProtocolMessage, OutputValidationFailure> {
    let candidate = raw_output.trim();
    let instance = parse_json_candidate(candidate, "workflow step review")?;
    let schema = parse_internal_schema(
        &step_review_protocol_json_schema(execution_id, step_key, criteria),
        "workflow step review",
    )?;
    validate_json_schema(&schema, &instance)?;
    parse_step_review_protocol_output(execution_id, step_key, criteria, candidate)
        .map_err(workflow_runtime_failure)
}

pub(crate) fn validate_workflow_loop_review_output(
    execution_id: Uuid,
    loop_key: &str,
    criteria: &[LoopReviewCriterion],
    allowed_step_keys: &[String],
    raw_output: &str,
) -> Result<LoopReviewProtocolMessage, OutputValidationFailure> {
    let candidate = raw_output.trim();
    let instance = parse_json_candidate(candidate, "workflow loop review")?;
    let schema = parse_internal_schema(
        &loop_review_protocol_json_schema(execution_id, loop_key, criteria, allowed_step_keys),
        "workflow loop review",
    )?;
    validate_json_schema(&schema, &instance)?;
    parse_loop_review_protocol_output(
        execution_id,
        loop_key,
        criteria,
        allowed_step_keys,
        candidate,
    )
    .map_err(workflow_runtime_failure)
}

fn parse_internal_schema(
    schema: &str,
    protocol_name: &str,
) -> Result<Value, OutputValidationFailure> {
    serde_json::from_str(schema).map_err(|error| {
        OutputValidationFailure::one(
            "validator_configuration",
            "/",
            format!("{protocol_name} schema is invalid: {error}"),
        )
    })
}

fn parse_json_candidate(
    candidate: &str,
    protocol_name: &str,
) -> Result<Value, OutputValidationFailure> {
    serde_json::from_str(candidate).map_err(|error| {
        OutputValidationFailure::one(
            "invalid_json",
            "/output",
            format!("{protocol_name} is not valid JSON: {error}"),
        )
    })
}

fn validate_json_schema(schema: &Value, instance: &Value) -> Result<(), OutputValidationFailure> {
    let validator = jsonschema::validator_for(schema).map_err(|error| {
        OutputValidationFailure::one(
            "validator_configuration",
            "/",
            format!("failed to compile output schema: {error}"),
        )
    })?;
    let errors = validator
        .iter_errors(instance)
        .take(MAX_REPORTED_ERRORS)
        .map(|error| OutputValidationError {
            code: "schema_validation".to_string(),
            path: if error.instance_path().as_str().is_empty() {
                "/".to_string()
            } else {
                error.instance_path().to_string()
            },
            message: error.to_string(),
        })
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(OutputValidationFailure { errors })
    }
}

fn chat_protocol_failure(error: AgentProtocolError) -> OutputValidationFailure {
    let code = match error.code {
        ChatProtocolNoticeCode::InvalidJson => "invalid_json",
        ChatProtocolNoticeCode::NotJsonArray => "not_json_array",
        ChatProtocolNoticeCode::EmptyMessage => "empty_message",
        ChatProtocolNoticeCode::MissingSendTarget => "missing_send_target",
        ChatProtocolNoticeCode::InvalidSendTarget => "invalid_send_target",
        ChatProtocolNoticeCode::InvalidSendIntent => "invalid_send_intent",
    };
    OutputValidationFailure::one(
        code,
        "/output",
        error
            .detail
            .or(error.target)
            .unwrap_or_else(|| "chat protocol validation failed".to_string()),
    )
}

fn workflow_runtime_failure(error: WorkflowRuntimeError) -> OutputValidationFailure {
    OutputValidationFailure::one("semantic_validation", "/output", error.to_string())
}

fn workflow_field_path(field: &str) -> String {
    if field.starts_with('/') {
        field.to_string()
    } else {
        format!("/{}", field.replace('.', "/"))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn chat_validation_rejects_duplicate_targets_after_normalization() {
        let response = validate_output_request(&OutputValidationRequest::ChatProtocol {
            output: json!([
                {"type": "send", "to": "you", "content": "first"},
                {"type": "send", "to": "USER", "content": "second"}
            ]),
            context: ChatProtocolValidationContext {
                allowed_targets: vec!["you".to_string()],
            },
        });

        assert!(!response.valid);
        assert_eq!(response.errors[0].code, "duplicate_send_target");
    }

    #[test]
    fn task_validation_applies_schema_before_semantics() {
        let execution_id = Uuid::new_v4();
        let response = validate_output_request(&OutputValidationRequest::WorkflowTask {
            output: json!({
                "type": "error",
                "step_key": "task-a",
                "execution_id": execution_id,
                "message": "failed",
                "unexpected": true
            }),
            context: WorkflowTaskValidationContext {
                execution_id,
                step_key: "task-a".to_string(),
                allow_interaction_requests: true,
            },
        });

        assert!(!response.valid);
        assert!(
            response
                .errors
                .iter()
                .any(|error| error.code == "schema_validation")
        );
    }
}
