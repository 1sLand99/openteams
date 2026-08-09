//! Loop review protocol types, JSON schema and parsing.
//!
//! The backend owns the acceptance contract. The reviewer returns only one
//! boolean result and one evidence string for each backend-assigned criterion
//! id. The reviewer separately names only the review-scope steps that need
//! rework; the backend derives the verdict and validates those targets.

use std::collections::{BTreeMap, BTreeSet};

use db::models::workflow_types::{AcceptanceCriterionLevel, ReviewVerdict};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::super::workflow_runtime::WorkflowRuntimeError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoopReviewCriterion {
    pub id: String,
    pub level: AcceptanceCriterionLevel,
    pub criterion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LoopReviewCriterionResult {
    pub passed: bool,
    pub evidence: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LoopReviewProtocolMessage {
    LoopReviewResult {
        loop_key: String,
        execution_id: String,
        summary: String,
        results: BTreeMap<String, LoopReviewCriterionResult>,
        rework: BTreeMap<String, String>,
    },
}

pub fn loop_review_protocol_json_schema(
    execution_id: Uuid,
    loop_key: &str,
    criteria: &[LoopReviewCriterion],
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
    let rework_properties = allowed_step_keys
        .iter()
        .map(|step_key| {
            (
                step_key.clone(),
                serde_json::json!({ "type": "string", "pattern": "\\S" }),
            )
        })
        .collect::<serde_json::Map<_, _>>();

    serde_json::to_string_pretty(&serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["type", "loop_key", "execution_id", "summary", "results", "rework"],
        "additionalProperties": false,
        "properties": {
            "type": { "const": "loop_review_result" },
            "loop_key": { "const": loop_key },
            "execution_id": execution_id_schema,
            "summary": { "type": "string", "pattern": "\\S" },
            "results": {
                "type": "object",
                "required": result_ids,
                "additionalProperties": false,
                "properties": result_properties
            },
            "rework": {
                "type": "object",
                "additionalProperties": false,
                "properties": rework_properties
            }
        }
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

/// Parses one strict JSON object and validates the acceptance and rework
/// contract. The overall verdict is always derived by the backend.
pub fn parse_loop_review_protocol_output(
    execution_id: Uuid,
    loop_key: &str,
    criteria: &[LoopReviewCriterion],
    allowed_step_keys: &[String],
    raw_output: &str,
) -> Result<LoopReviewProtocolMessage, WorkflowRuntimeError> {
    let message: LoopReviewProtocolMessage = serde_json::from_str(raw_output.trim())?;

    let LoopReviewProtocolMessage::LoopReviewResult {
        loop_key: actual_loop_key,
        execution_id: actual_execution_id,
        summary,
        results,
        rework,
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
    if summary.trim().is_empty() {
        return Err(WorkflowRuntimeError::Validation(
            "loop review 的 summary 不能为空".to_string(),
        ));
    }

    let expected_ids = criteria
        .iter()
        .map(|criterion| criterion.id.as_str())
        .collect::<BTreeSet<_>>();
    let actual_ids = results.keys().map(String::as_str).collect::<BTreeSet<_>>();
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
            "loop review results 与验收清单不一致（缺少 {missing:?}，多余 {extra:?}）"
        )));
    }
    if let Some((id, _)) = results
        .iter()
        .find(|(_, result)| result.evidence.trim().is_empty())
    {
        return Err(WorkflowRuntimeError::Validation(format!(
            "loop review result '{id}' 的 evidence 不能为空"
        )));
    }

    let allowed_step_keys = allowed_step_keys
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if let Some(step_key) = rework
        .keys()
        .find(|key| !allowed_step_keys.contains(key.as_str()))
    {
        return Err(WorkflowRuntimeError::Validation(format!(
            "loop review rework 包含 reviewScope 外的 step_key '{step_key}'"
        )));
    }
    if let Some((step_key, _)) = rework
        .iter()
        .find(|(_, feedback)| feedback.trim().is_empty())
    {
        return Err(WorkflowRuntimeError::Validation(format!(
            "loop review rework '{step_key}' 的反馈不能为空"
        )));
    }
    let verdict = derive_loop_review_verdict(criteria, results);
    if verdict == ReviewVerdict::Rejected && rework.is_empty() {
        return Err(WorkflowRuntimeError::Validation(
            "loop review 驳回时 rework 必须至少指定一个 reviewScope step".to_string(),
        ));
    }
    if verdict == ReviewVerdict::Approved && !rework.is_empty() {
        return Err(WorkflowRuntimeError::Validation(
            "loop review 通过时 rework 必须为空".to_string(),
        ));
    }

    Ok(message)
}

pub(super) fn derive_loop_review_verdict(
    criteria: &[LoopReviewCriterion],
    results: &BTreeMap<String, LoopReviewCriterionResult>,
) -> ReviewVerdict {
    if criteria.iter().any(|criterion| {
        criterion.level == AcceptanceCriterionLevel::Required
            && results
                .get(&criterion.id)
                .is_none_or(|result| !result.passed)
    }) {
        ReviewVerdict::Rejected
    } else {
        ReviewVerdict::Approved
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criteria() -> Vec<LoopReviewCriterion> {
        vec![
            LoopReviewCriterion {
                id: "c1".to_string(),
                level: AcceptanceCriterionLevel::Required,
                criterion: "cargo test 通过".to_string(),
            },
            LoopReviewCriterion {
                id: "c2".to_string(),
                level: AcceptanceCriterionLevel::Partial,
                criterion: "完成外部冒烟".to_string(),
            },
        ]
    }

    fn raw_output(execution_id: Uuid, results: &str, rework: &str) -> String {
        format!(
            r#"{{"type":"loop_review_result","loop_key":"loop-a","execution_id":"{execution_id}","summary":"审核完成","results":{results},"rework":{rework}}}"#
        )
    }

    fn allowed_step_keys() -> Vec<String> {
        vec!["draft".to_string(), "implement".to_string()]
    }

    #[test]
    fn schema_requires_the_exact_criterion_ids() {
        let schema: serde_json::Value = serde_json::from_str(&loop_review_protocol_json_schema(
            Uuid::new_v4(),
            "loop-a",
            &criteria(),
            &allowed_step_keys(),
        ))
        .unwrap();

        assert_eq!(
            schema["properties"]["results"]["required"],
            serde_json::json!(["c1", "c2"])
        );
        assert_eq!(
            schema["properties"]["results"]["additionalProperties"],
            false
        );
        assert!(schema["properties"]["rework"]["properties"]["draft"].is_object());
    }

    #[test]
    fn parse_accepts_exact_results() {
        let execution_id = Uuid::new_v4();
        let parsed = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &criteria(),
            &allowed_step_keys(),
            &raw_output(
                execution_id,
                r#"{"c1":{"passed":true,"evidence":"tests passed"},"c2":{"passed":false,"evidence":"missing credentials"}}"#,
                "{}",
            ),
        )
        .unwrap();

        let LoopReviewProtocolMessage::LoopReviewResult { results, .. } = parsed;
        assert!(results["c1"].passed);
        assert!(!results["c2"].passed);
    }

    #[test]
    fn verdict_is_derived_only_from_required_results() {
        let criteria = criteria();
        let partial_failed = BTreeMap::from([
            (
                "c1".to_string(),
                LoopReviewCriterionResult {
                    passed: true,
                    evidence: "ok".to_string(),
                },
            ),
            (
                "c2".to_string(),
                LoopReviewCriterionResult {
                    passed: false,
                    evidence: "external service unavailable".to_string(),
                },
            ),
        ]);
        assert_eq!(
            derive_loop_review_verdict(&criteria, &partial_failed),
            ReviewVerdict::Approved
        );

        let required_failed = BTreeMap::from([
            (
                "c1".to_string(),
                LoopReviewCriterionResult {
                    passed: false,
                    evidence: "test failed".to_string(),
                },
            ),
            (
                "c2".to_string(),
                LoopReviewCriterionResult {
                    passed: true,
                    evidence: "ok".to_string(),
                },
            ),
        ]);
        assert_eq!(
            derive_loop_review_verdict(&criteria, &required_failed),
            ReviewVerdict::Rejected
        );
    }

    #[test]
    fn parse_rejects_missing_or_extra_ids() {
        let execution_id = Uuid::new_v4();
        let error = parse_loop_review_protocol_output(
            execution_id,
            "loop-a",
            &criteria(),
            &allowed_step_keys(),
            &raw_output(
                execution_id,
                r#"{"c1":{"passed":true,"evidence":"ok"},"c3":{"passed":true,"evidence":"extra"}}"#,
                "{}",
            ),
        )
        .unwrap_err();

        assert!(error.to_string().contains("缺少"));
        assert!(error.to_string().contains("c2"));
        assert!(error.to_string().contains("c3"));
    }

    #[test]
    fn parse_rejects_blank_evidence_and_extra_fields() {
        let execution_id = Uuid::new_v4();
        let blank = raw_output(
            execution_id,
            r#"{"c1":{"passed":true,"evidence":" "},"c2":{"passed":true,"evidence":"ok"}}"#,
            "{}",
        );
        assert!(
            parse_loop_review_protocol_output(
                execution_id,
                "loop-a",
                &criteria(),
                &allowed_step_keys(),
                &blank,
            )
            .unwrap_err()
            .to_string()
            .contains("evidence")
        );

        let extra = raw_output(
            execution_id,
            r#"{"c1":{"passed":true,"evidence":"ok","verdict":"passed"},"c2":{"passed":true,"evidence":"ok"}}"#,
            "{}",
        );
        assert!(
            parse_loop_review_protocol_output(
                execution_id,
                "loop-a",
                &criteria(),
                &allowed_step_keys(),
                &extra,
            )
            .is_err()
        );
    }

    #[test]
    fn parse_rejects_wrapping_text() {
        let execution_id = Uuid::new_v4();
        let raw = format!(
            "```json\n{}\n```",
            raw_output(
                execution_id,
                r#"{"c1":{"passed":true,"evidence":"ok"},"c2":{"passed":true,"evidence":"ok"}}"#,
                "{}",
            )
        );

        assert!(
            parse_loop_review_protocol_output(
                execution_id,
                "loop-a",
                &criteria(),
                &allowed_step_keys(),
                &raw,
            )
            .is_err()
        );
    }

    #[test]
    fn rejected_result_requires_only_valid_explicit_rework_targets() {
        let execution_id = Uuid::new_v4();
        let rejected_results = r#"{"c1":{"passed":false,"evidence":"integration failed"},"c2":{"passed":true,"evidence":"ok"}}"#;

        let missing = raw_output(execution_id, rejected_results, "{}");
        assert!(
            parse_loop_review_protocol_output(
                execution_id,
                "loop-a",
                &criteria(),
                &allowed_step_keys(),
                &missing,
            )
            .unwrap_err()
            .to_string()
            .contains("至少指定一个")
        );

        let outside = raw_output(
            execution_id,
            rejected_results,
            r#"{"outside":"修复集成问题"}"#,
        );
        assert!(
            parse_loop_review_protocol_output(
                execution_id,
                "loop-a",
                &criteria(),
                &allowed_step_keys(),
                &outside,
            )
            .unwrap_err()
            .to_string()
            .contains("reviewScope 外")
        );

        let targeted = raw_output(
            execution_id,
            rejected_results,
            r#"{"implement":"修复集成问题"}"#,
        );
        assert!(
            parse_loop_review_protocol_output(
                execution_id,
                "loop-a",
                &criteria(),
                &allowed_step_keys(),
                &targeted,
            )
            .is_ok()
        );
    }
}
