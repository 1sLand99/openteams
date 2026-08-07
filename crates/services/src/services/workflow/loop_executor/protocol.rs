//! Loop review protocol types, JSON schema and parsing (design §9.3).
//!
//! Split out of `workflow/review.rs` so the Loop domain is owned by
//! `loop_executor`. Compared with the legacy version, every
//! `acceptance_results` item carries a `level` tier, and the parse entry
//! performs declared-acceptance coverage and tier-aware verdict consistency
//! checks in one place.

use db::models::workflow_types::{AcceptanceCriterionLevel, ReviewVerdict};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::super::workflow_runtime::{
    WorkflowAcceptanceVerdict, WorkflowRuntimeError, extract_json_payload,
};

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
    /// Tier declared in the plan. Defaults to `required` so loop review JSON
    /// persisted before tiers existed still deserializes.
    #[serde(default)]
    pub level: AcceptanceCriterionLevel,
    pub verdict: WorkflowAcceptanceVerdict,
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
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["step_key", "criterion", "level", "verdict", "evidence"],
                    "additionalProperties": false,
                    "properties": {
                        "step_key": { "enum": allowed_step_keys },
                        "criterion": { "type": "string", "minLength": 1 },
                        "level": { "enum": ["required", "partial", "recommended"] },
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

/// Parse and validate a `loop_review_result` model output.
///
/// Validation order: fixed ids → feedback non-empty → rejection requirements
/// (issue_id, step_feedbacks) → declared-acceptance coverage → tier-aware
/// verdict consistency.
pub fn parse_loop_review_protocol_output(
    execution_id: Uuid,
    loop_key: &str,
    allowed_step_keys: &[String],
    declared_acceptance: &[(String, AcceptanceCriterionLevel, String)],
    raw_output: &str,
) -> Result<LoopReviewProtocolMessage, WorkflowRuntimeError> {
    let payload = extract_json_payload(raw_output).ok_or_else(|| {
        WorkflowRuntimeError::Validation("loop review 输出中未找到 JSON 对象".to_string())
    })?;
    let message: LoopReviewProtocolMessage = serde_json::from_str(&payload)?;

    let LoopReviewProtocolMessage::LoopReviewResult {
        loop_key: actual_loop_key,
        execution_id: actual_execution_id,
        verdict,
        feedback,
        acceptance_results,
        issue_id,
        step_feedbacks,
        ..
    } = &message;

    if actual_loop_key != loop_key {
        return Err(WorkflowRuntimeError::Validation(format!(
            "loop review 的 loop_key 非法，期望 '{loop_key}'，实际 '{actual_loop_key}'"
        )));
    }
    if actual_execution_id != &execution_id.to_string() {
        return Err(WorkflowRuntimeError::Validation(format!(
            "loop review 的 execution_id 非法，期望 '{execution_id}'，实际 '{actual_execution_id}'"
        )));
    }
    if feedback.trim().is_empty() {
        return Err(WorkflowRuntimeError::Validation(
            "loop review 的 feedback 不能为空".to_string(),
        ));
    }

    let rejected = matches!(verdict, ReviewVerdict::Rejected);
    if rejected {
        if issue_id.as_deref().map(str::trim).is_none_or(str::is_empty) {
            return Err(WorkflowRuntimeError::Validation(
                "loop review rejected 时 issue_id 不能为空".to_string(),
            ));
        }
        if let Some(item) = step_feedbacks
            .iter()
            .find(|item| !allowed_step_keys.contains(&item.step_key))
        {
            return Err(WorkflowRuntimeError::Validation(format!(
                "loop review 的 step_feedbacks 引用了 reviewScope 外的 step_key '{}'",
                item.step_key
            )));
        }
        if step_feedbacks
            .iter()
            .any(|item| item.issue_id.trim().is_empty() || item.feedback.trim().is_empty())
        {
            return Err(WorkflowRuntimeError::Validation(
                "loop review rejected 时 step_feedbacks.issue_id/feedback 不能为空".to_string(),
            ));
        }
    }

    validate_acceptance_coverage(declared_acceptance, acceptance_results)?;

    let has_failed = acceptance_results
        .iter()
        .any(|item| matches!(item.verdict, WorkflowAcceptanceVerdict::Failed));
    let has_required_failed = acceptance_results.iter().any(|item| {
        matches!(item.verdict, WorkflowAcceptanceVerdict::Failed)
            && matches!(item.level, AcceptanceCriterionLevel::Required)
    });
    if matches!(verdict, ReviewVerdict::Approved) && has_required_failed {
        return Err(WorkflowRuntimeError::Validation(
            "approved loop review 不允许存在 required 级 failed 验收项".to_string(),
        ));
    }
    // The loop protocol has no `risks` field: partial-level failures are
    // attributed through the non-empty feedback, so no extra check here.
    if rejected
        && !has_failed
        && step_feedbacks.is_empty()
        && issue_id.as_deref().map(str::trim).is_none_or(str::is_empty)
    {
        return Err(WorkflowRuntimeError::Validation(
            "rejected loop review 必须至少包含一个 failed 验收项、step_feedbacks 或 issue_id"
                .to_string(),
        ));
    }

    Ok(message)
}

/// Declared criteria must be covered exactly once, no more, no less, each at
/// the declared tier. Criteria compare after whitespace collapsing.
fn validate_acceptance_coverage(
    declared_acceptance: &[(String, AcceptanceCriterionLevel, String)],
    acceptance_results: &[LoopReviewAcceptanceResult],
) -> Result<(), WorkflowRuntimeError> {
    let normalize = |value: &str| value.split_whitespace().collect::<Vec<_>>().join(" ");

    let mut expected: Vec<(&str, AcceptanceCriterionLevel, String)> = Vec::new();
    for (step_key, level, criterion) in declared_acceptance {
        let criterion = normalize(criterion);
        if criterion.is_empty() {
            continue;
        }
        let entry = (step_key.as_str(), *level, criterion);
        if !expected.contains(&entry) {
            expected.push(entry);
        }
    }

    let mut seen: Vec<(&str, AcceptanceCriterionLevel, String)> = Vec::new();
    for result in acceptance_results {
        let entry = (
            result.step_key.as_str(),
            result.level,
            normalize(&result.criterion),
        );
        if seen.contains(&entry) {
            return Err(WorkflowRuntimeError::Validation(format!(
                "loop review acceptance_results 重复覆盖验收标准 '{}'",
                entry.2
            )));
        }
        if !expected.contains(&entry) {
            return Err(WorkflowRuntimeError::Validation(format!(
                "loop review acceptance_results 包含未声明的验收标准或 level 与声明不符：{} / '{}'",
                entry.0, entry.2
            )));
        }
        seen.push(entry);
    }
    if seen.len() != expected.len() {
        return Err(WorkflowRuntimeError::Validation(format!(
            "loop review acceptance_results 必须恰好覆盖每条已声明验收标准一次（声明 {} 条，实际 {} 条）",
            expected.len(),
            seen.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed_step_keys() -> Vec<String> {
        vec!["draft".to_string(), "revise".to_string()]
    }

    fn declared_acceptance() -> Vec<(String, AcceptanceCriterionLevel, String)> {
        vec![
            (
                "draft".to_string(),
                AcceptanceCriterionLevel::Required,
                "cargo test 全部通过".to_string(),
            ),
            (
                "draft".to_string(),
                AcceptanceCriterionLevel::Partial,
                "外部服务冒烟可用".to_string(),
            ),
            (
                "revise".to_string(),
                AcceptanceCriterionLevel::Required,
                "格式符合规范".to_string(),
            ),
            (
                "revise".to_string(),
                AcceptanceCriterionLevel::Recommended,
                "附带截图".to_string(),
            ),
        ]
    }

    /// Criteria cover the declared set exactly once; the first one uses
    /// collapsed-whitespace variants on purpose.
    fn valid_acceptance_results() -> &'static str {
        r#"[
    { "step_key": "draft", "criterion": "cargo   test\n全部通过", "level": "required", "verdict": "passed", "evidence": "cargo test 输出正常" },
    { "step_key": "draft", "criterion": "外部服务冒烟可用", "level": "partial", "verdict": "failed", "evidence": "沙箱缺少凭据，归因外部" },
    { "step_key": "revise", "criterion": "格式符合规范", "level": "required", "verdict": "passed", "evidence": "lint 通过" },
    { "step_key": "revise", "criterion": "附带截图", "level": "recommended", "verdict": "failed", "evidence": "未附截图" }
  ]"#
    }

    fn raw_output(
        execution_id: Uuid,
        verdict: &str,
        acceptance_results: &str,
        extra_fields: &str,
    ) -> String {
        format!(
            r#"{{
  "type": "loop_review_result",
  "loop_key": "loop-a",
  "execution_id": "{execution_id}",
  "verdict": "{verdict}",
  "feedback": "整体审核反馈",
  "acceptance_results": {acceptance_results},
  "evidence": ["已检查 docs/draft.md"]{extra_fields}
}}"#
        )
    }

    #[test]
    fn parse_accepts_approved_with_partial_level_failure() {
        let execution_id = Uuid::new_v4();
        let raw = raw_output(execution_id, "approved", valid_acceptance_results(), "");
        let parsed = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw,
        )
        .expect("approved with attributed partial/recommended failures is allowed");
        let LoopReviewProtocolMessage::LoopReviewResult {
            verdict,
            acceptance_results,
            issue_id,
            step_feedbacks,
            ..
        } = parsed;
        assert_eq!(verdict, ReviewVerdict::Approved);
        assert_eq!(acceptance_results.len(), 4);
        assert_eq!(
            acceptance_results[1].level,
            AcceptanceCriterionLevel::Partial
        );
        assert_eq!(
            acceptance_results[3].level,
            AcceptanceCriterionLevel::Recommended
        );
        assert_eq!(issue_id, None);
        assert!(step_feedbacks.is_empty());
    }

    #[test]
    fn parse_accepts_rejected_with_or_without_step_feedbacks() {
        let execution_id = Uuid::new_v4();
        let extra = r#",
  "issue_id": "loop-quality-gate",
  "step_feedbacks": [
    { "step_key": "draft", "issue_id": "draft-missing-background", "feedback": "请补充背景" }
  ]"#;
        let parsed = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "rejected", valid_acceptance_results(), extra),
        )
        .expect("rejected with step_feedbacks is accepted");
        let LoopReviewProtocolMessage::LoopReviewResult {
            verdict,
            issue_id,
            step_feedbacks,
            ..
        } = parsed;
        assert_eq!(verdict, ReviewVerdict::Rejected);
        assert_eq!(issue_id.as_deref(), Some("loop-quality-gate"));
        assert_eq!(step_feedbacks.len(), 1);
        assert_eq!(step_feedbacks[0].step_key, "draft");

        // Empty step_feedbacks means the whole reviewScope is reworked.
        let parsed = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(
                execution_id,
                "rejected",
                valid_acceptance_results(),
                ",\n  \"issue_id\": \"loop-quality-gate\"",
            ),
        )
        .expect("rejected without step_feedbacks is accepted");
        let LoopReviewProtocolMessage::LoopReviewResult { step_feedbacks, .. } = parsed;
        assert!(step_feedbacks.is_empty());
    }

    #[test]
    fn parse_rejects_mismatched_fixed_ids() {
        let execution_id = Uuid::new_v4();
        let raw = raw_output(execution_id, "approved", valid_acceptance_results(), "")
            .replace("\"loop_key\": \"loop-a\"", "\"loop_key\": \"other-loop\"");
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw,
        )
        .expect_err("mismatched loop_key must fail");
        assert!(error.to_string().contains("loop_key"));

        let other_execution = Uuid::new_v4();
        let error = parse_loop_review_protocol_output(
            other_execution,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "approved", valid_acceptance_results(), ""),
        )
        .expect_err("mismatched execution_id must fail");
        assert!(error.to_string().contains("execution_id"));
    }

    #[test]
    fn parse_rejects_rejection_without_issue_id() {
        let execution_id = Uuid::new_v4();
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "rejected", valid_acceptance_results(), ""),
        )
        .expect_err("missing issue_id must fail");
        assert!(error.to_string().contains("issue_id"));
    }

    #[test]
    fn parse_rejects_step_feedbacks_with_unknown_step_key() {
        let execution_id = Uuid::new_v4();
        let extra = r#",
  "issue_id": "loop-quality-gate",
  "step_feedbacks": [
    { "step_key": "ghost", "issue_id": "ghost-issue", "feedback": "越界反馈" }
  ]"#;
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "rejected", valid_acceptance_results(), extra),
        )
        .expect_err("step_feedbacks outside the review scope must fail");
        assert!(error.to_string().contains("step_feedbacks"));
    }

    #[test]
    fn coverage_rejects_missing_criterion() {
        let execution_id = Uuid::new_v4();
        let results = r#"[
    { "step_key": "draft", "criterion": "cargo test 全部通过", "level": "required", "verdict": "passed", "evidence": "通过" },
    { "step_key": "draft", "criterion": "外部服务冒烟可用", "level": "partial", "verdict": "passed", "evidence": "通过" },
    { "step_key": "revise", "criterion": "格式符合规范", "level": "required", "verdict": "passed", "evidence": "通过" }
  ]"#;
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "approved", results, ""),
        )
        .expect_err("a missing declared criterion must fail");
        assert!(error.to_string().contains("acceptance_results"));
    }

    #[test]
    fn coverage_rejects_undeclared_criterion_or_step_key() {
        let execution_id = Uuid::new_v4();
        let extra_criterion = r#"[
    { "step_key": "draft", "criterion": "cargo test 全部通过", "level": "required", "verdict": "passed", "evidence": "通过" },
    { "step_key": "draft", "criterion": "外部服务冒烟可用", "level": "partial", "verdict": "passed", "evidence": "通过" },
    { "step_key": "revise", "criterion": "格式符合规范", "level": "required", "verdict": "passed", "evidence": "通过" },
    { "step_key": "revise", "criterion": "附带截图", "level": "recommended", "verdict": "passed", "evidence": "通过" },
    { "step_key": "revise", "criterion": "现场自创的标准", "level": "required", "verdict": "passed", "evidence": "通过" }
  ]"#;
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "approved", extra_criterion, ""),
        )
        .expect_err("an undeclared criterion must fail");
        assert!(error.to_string().contains("未声明"));

        let undeclared_step = extra_criterion.replace("现场自创的标准", "附带截图2");
        let undeclared_step = undeclared_step.replace(
            "{ \"step_key\": \"revise\", \"criterion\": \"附带截图2\"",
            "{ \"step_key\": \"ghost\", \"criterion\": \"附带截图2\"",
        );
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "approved", &undeclared_step, ""),
        )
        .expect_err("an undeclared step_key must fail");
        assert!(error.to_string().contains("未声明"));
    }

    #[test]
    fn coverage_rejects_duplicate_coverage() {
        let execution_id = Uuid::new_v4();
        let results = r#"[
    { "step_key": "draft", "criterion": "cargo test 全部通过", "level": "required", "verdict": "passed", "evidence": "通过" },
    { "step_key": "draft", "criterion": "cargo test 全部通过", "level": "required", "verdict": "passed", "evidence": "再次报告" },
    { "step_key": "draft", "criterion": "外部服务冒烟可用", "level": "partial", "verdict": "passed", "evidence": "通过" },
    { "step_key": "revise", "criterion": "格式符合规范", "level": "required", "verdict": "passed", "evidence": "通过" },
    { "step_key": "revise", "criterion": "附带截图", "level": "recommended", "verdict": "passed", "evidence": "通过" }
  ]"#;
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "approved", results, ""),
        )
        .expect_err("duplicate coverage must fail");
        assert!(error.to_string().contains("重复"));
    }

    #[test]
    fn coverage_rejects_level_mismatch() {
        let execution_id = Uuid::new_v4();
        let results = valid_acceptance_results().replace(
            "{ \"step_key\": \"draft\", \"criterion\": \"cargo   test\\n全部通过\", \"level\": \"required\"",
            "{ \"step_key\": \"draft\", \"criterion\": \"cargo   test\\n全部通过\", \"level\": \"partial\"",
        );
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "approved", &results, ""),
        )
        .expect_err("level different from the declaration must fail");
        assert!(error.to_string().contains("level"));
    }

    #[test]
    fn parse_rejects_approved_with_required_level_failure() {
        let execution_id = Uuid::new_v4();
        let results = valid_acceptance_results().replace(
            "\"criterion\": \"cargo   test\\n全部通过\", \"level\": \"required\", \"verdict\": \"passed\"",
            "\"criterion\": \"cargo   test\\n全部通过\", \"level\": \"required\", \"verdict\": \"failed\"",
        );
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared_acceptance(),
            &raw_output(execution_id, "approved", &results, ""),
        )
        .expect_err("approved with a required-level failure must fail");
        assert!(error.to_string().contains("required"));
    }

    #[test]
    fn missing_level_defaults_to_required_for_legacy_json() {
        let execution_id = Uuid::new_v4();
        let declared = vec![
            (
                "draft".to_string(),
                AcceptanceCriterionLevel::Required,
                "cargo test 全部通过".to_string(),
            ),
            (
                "revise".to_string(),
                AcceptanceCriterionLevel::Required,
                "格式符合规范".to_string(),
            ),
        ];
        // Legacy stored shape: acceptance_results items carry no `level`.
        let results = r#"[
    { "step_key": "draft", "criterion": "cargo test 全部通过", "verdict": "passed", "evidence": "通过" },
    { "step_key": "revise", "criterion": "格式符合规范", "verdict": "passed", "evidence": "通过" }
  ]"#;
        let parsed = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &allowed_step_keys(),
            &declared,
            &raw_output(execution_id, "approved", results, ""),
        )
        .expect("legacy items without level default to required");
        let LoopReviewProtocolMessage::LoopReviewResult {
            acceptance_results, ..
        } = parsed;
        assert!(
            acceptance_results
                .iter()
                .all(|item| item.level == AcceptanceCriterionLevel::Required)
        );
    }
}
