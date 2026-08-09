//! Deterministic result-node aggregation; no model call (design §6.6、§9.4、§10).
//!
//! The result node is the single terminal step of a workflow graph. Instead of
//! invoking an agent, the runtime constructs its `result_review_result` payload
//! from the latest valid results of all passed predecessors using the fixed
//! rules in design §9.4 and the fixed Markdown template in §11.5. This module
//! is a pure function: no IO, no database access, no model calls.

use std::collections::HashSet;

use db::models::workflow_types::AcceptanceCriterionLevel;
use uuid::Uuid;

use super::{
    WorkflowAcceptanceResult, WorkflowAcceptanceVerdict, WorkflowResultOverallStatus,
    WorkflowRuntimeError, WorkflowTaskCompletionStatus,
};

/// Latest valid result of a single passed predecessor node (design §6.6).
///
/// Only completed, review-passed, or explicitly user-waived results may enter
/// aggregation; failed, superseded, pending-rework, or still-rejected attempts
/// must be filtered out by the caller before constructing this input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalNodeResultInput {
    pub step_key: String,
    pub status: WorkflowTaskCompletionStatus,
    pub summary: String,
    pub outputs: Vec<String>,
    pub evidence: Vec<String>,
    /// Acceptance conclusions already produced by step review / Loop review
    /// nodes; merged verbatim into the aggregated output.
    pub acceptance_results: Vec<WorkflowAcceptanceResult>,
    pub risks: Vec<String>,
    pub unfinished_items: Vec<String>,
    pub issues: Vec<String>,
}

/// Typed input for deterministic result aggregation (design §6.6).
#[derive(Debug, Clone)]
pub struct ResultAggregationInput {
    pub execution_id: Uuid,
    pub step_key: String,
    pub workflow_goal: String,
    pub title: String,
    pub instructions: String,
    /// Latest valid results of all passed predecessors, in topological order.
    pub latest_node_results: Vec<FinalNodeResultInput>,
}

/// Deterministically constructed result-node output (design §9.4).
#[derive(Debug, Clone, PartialEq)]
pub struct ResultAggregationOutput {
    pub overall_status: WorkflowResultOverallStatus,
    /// One-paragraph plain-text summary derived from `content`.
    pub summary: String,
    /// Full Markdown aggregation rendered from the fixed §11.5 template.
    pub content: String,
    pub deliverables: Vec<String>,
    pub acceptance_results: Vec<WorkflowAcceptanceResult>,
    pub evidence: Vec<String>,
    pub risks: Vec<String>,
    pub unfinished_items: Vec<String>,
}

/// Union that preserves first-seen order while dropping duplicates.
fn union_preserving_order<I>(items: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            merged.push(item);
        }
    }
    merged
}

/// Snake-case wire value of a task completion status, matching the serde
/// representation used on the protocol.
fn completion_status_wire_value(status: WorkflowTaskCompletionStatus) -> &'static str {
    match status {
        WorkflowTaskCompletionStatus::Done => "done",
        WorkflowTaskCompletionStatus::DoneWithConcerns => "done_with_concerns",
        WorkflowTaskCompletionStatus::Blocked => "blocked",
        WorkflowTaskCompletionStatus::NeedsContext => "needs_context",
    }
}

/// Derive the overall status from node statuses, merged acceptance results,
/// and merged risk/unfinished lists (design §9.4).
fn derive_overall_status(
    node_results: &[FinalNodeResultInput],
    acceptance_results: &[WorkflowAcceptanceResult],
    risks: &[String],
    unfinished_items: &[String],
) -> WorkflowResultOverallStatus {
    let has_blocked_node = node_results.iter().any(|node| {
        matches!(
            node.status,
            WorkflowTaskCompletionStatus::Blocked | WorkflowTaskCompletionStatus::NeedsContext
        )
    });
    let has_required_failure = acceptance_results.iter().any(|result| {
        result.level == AcceptanceCriterionLevel::Required
            && result.verdict == WorkflowAcceptanceVerdict::Failed
    });
    if has_blocked_node || has_required_failure {
        return WorkflowResultOverallStatus::Blocked;
    }

    let has_concerned_node = node_results
        .iter()
        .any(|node| node.status == WorkflowTaskCompletionStatus::DoneWithConcerns);
    let has_partial_failure = acceptance_results.iter().any(|result| {
        result.level == AcceptanceCriterionLevel::Partial
            && result.verdict == WorkflowAcceptanceVerdict::Failed
    });
    if has_concerned_node
        || has_partial_failure
        || !risks.is_empty()
        || !unfinished_items.is_empty()
    {
        return WorkflowResultOverallStatus::CompletedWithConcerns;
    }

    WorkflowResultOverallStatus::Completed
}

/// Render the fixed Markdown aggregation template (design §11.5).
fn render_content(
    input: &ResultAggregationInput,
    deliverables: &[String],
    risks: &[String],
    unfinished_items: &[String],
) -> String {
    let mut content = String::new();
    content.push_str(&format!("# {}\n\n", input.title.trim()));
    content.push_str("## 工作总目标\n\n");
    content.push_str(input.workflow_goal.trim());
    content.push_str("\n\n## 节点结果\n\n");
    for node in &input.latest_node_results {
        content.push_str(&format!(
            "- `{}`（{}）：{}\n",
            node.step_key,
            completion_status_wire_value(node.status),
            node.summary.trim()
        ));
    }
    content.push_str("\n## 交付物\n\n");
    if deliverables.is_empty() {
        content.push_str("无\n");
    } else {
        for deliverable in deliverables {
            content.push_str(&format!("- {deliverable}\n"));
        }
    }
    content.push_str("\n## 风险与未完成项\n\n");
    if risks.is_empty() && unfinished_items.is_empty() {
        content.push_str("无\n");
    } else {
        for item in risks.iter().chain(unfinished_items.iter()) {
            content.push_str(&format!("- {item}\n"));
        }
    }
    content
}

/// Render the one-paragraph plain-text summary (completion/concern/blocked
/// status plus the aggregated node count).
fn render_summary(overall_status: WorkflowResultOverallStatus, node_count: usize) -> String {
    match overall_status {
        WorkflowResultOverallStatus::Completed => {
            format!("全部 {node_count} 个节点均已完成，整体状态：已完成。")
        }
        WorkflowResultOverallStatus::CompletedWithConcerns => {
            format!(
                "{node_count} 个节点已完成，但存在需关注的风险或未完成项，整体状态：已完成但有需关注事项。"
            )
        }
        WorkflowResultOverallStatus::Blocked => {
            format!("{node_count} 个节点中存在阻塞或未通过的必要验收项，整体状态：阻塞。")
        }
    }
}

/// Construct the result-node output deterministically from the latest valid
/// predecessor results, without any model call (design §9.4、§10.3).
///
/// The constructed result must pass the same structural self-check the wire
/// `result_review_result` would (design §14): a non-empty summary and at least
/// one evidence entry. When the check fails a `Validation` error is returned
/// so the caller routes it through the executor error channel instead of
/// writing the step result.
pub fn construct_result_review_output(
    input: &ResultAggregationInput,
) -> Result<ResultAggregationOutput, WorkflowRuntimeError> {
    let deliverables = union_preserving_order(
        input
            .latest_node_results
            .iter()
            .flat_map(|node| node.outputs.iter().cloned()),
    );
    // Workflows without any review node legitimately produce an empty merge.
    let acceptance_results: Vec<WorkflowAcceptanceResult> = input
        .latest_node_results
        .iter()
        .flat_map(|node| node.acceptance_results.iter().cloned())
        .collect();
    let evidence = union_preserving_order(
        input
            .latest_node_results
            .iter()
            .flat_map(|node| node.evidence.iter().cloned()),
    );
    let risks = union_preserving_order(
        input
            .latest_node_results
            .iter()
            .flat_map(|node| node.risks.iter().cloned()),
    );
    let unfinished_items =
        union_preserving_order(input.latest_node_results.iter().flat_map(|node| {
            node.unfinished_items
                .iter()
                .chain(node.issues.iter())
                .cloned()
        }));

    let overall_status = derive_overall_status(
        &input.latest_node_results,
        &acceptance_results,
        &risks,
        &unfinished_items,
    );
    let summary = render_summary(overall_status, input.latest_node_results.len());
    let content = render_content(input, &deliverables, &risks, &unfinished_items);

    // Structural self-check before the result may be written to the step
    // (design §14): failures go through the error channel.
    if summary.trim().is_empty() {
        return Err(WorkflowRuntimeError::Validation(
            "result aggregation produced an empty summary".to_string(),
        ));
    }
    if evidence.is_empty() {
        return Err(WorkflowRuntimeError::Validation(
            "result aggregation requires at least one evidence entry from predecessor nodes"
                .to_string(),
        ));
    }

    Ok(ResultAggregationOutput {
        overall_status,
        summary,
        content,
        deliverables,
        acceptance_results,
        evidence,
        risks,
        unfinished_items,
    })
}

/// Builds the aggregation input for one predecessor step from its persisted
/// latest valid result (design §6.6、§13). Returns `None` when the step has no
/// stored result; the caller decides how a missing predecessor affects the
/// result node (design §14).
pub fn final_node_result_from_step(
    step: &db::models::workflow_step::WorkflowStep,
) -> Option<FinalNodeResultInput> {
    let payload = super::parse_summary_payload(step.summary_text.as_deref())?;
    let structured = payload
        .content
        .as_deref()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(content).ok());

    let status = structured
        .as_ref()
        .and_then(structured_status)
        .unwrap_or_else(|| fallback_status(step));

    let outputs = if payload.outputs.is_empty() {
        structured_string_array(structured.as_ref(), "outputs")
    } else {
        payload.outputs.clone()
    };

    Some(FinalNodeResultInput {
        step_key: step.step_key.clone(),
        status,
        summary: payload.summary,
        outputs,
        evidence: structured_string_array(structured.as_ref(), "evidence"),
        acceptance_results: structured_acceptance_results(structured.as_ref()),
        risks: structured_string_array(structured.as_ref(), "risks"),
        unfinished_items: structured_string_array(structured.as_ref(), "unfinished_items"),
        issues: structured_string_array(structured.as_ref(), "issues"),
    })
}

/// Derives a task-style status from the persisted structured result JSON:
/// task steps carry `status`, review steps `verdict`, result/loop steps
/// `overall_status`/their own shapes.
fn structured_status(value: &serde_json::Value) -> Option<WorkflowTaskCompletionStatus> {
    if let Some(status) = value
        .get("status")
        .and_then(|raw| serde_json::from_value::<WorkflowTaskCompletionStatus>(raw.clone()).ok())
    {
        return Some(status);
    }
    if let Some(verdict) = value.get("verdict").and_then(|raw| raw.as_str()) {
        return Some(match verdict {
            "approved" => WorkflowTaskCompletionStatus::Done,
            "rejected" => WorkflowTaskCompletionStatus::Blocked,
            _ => WorkflowTaskCompletionStatus::DoneWithConcerns,
        });
    }
    if let Some(overall) = value.get("overall_status").and_then(|raw| raw.as_str()) {
        return Some(match overall {
            "completed" => WorkflowTaskCompletionStatus::Done,
            "completed_with_concerns" => WorkflowTaskCompletionStatus::DoneWithConcerns,
            _ => WorkflowTaskCompletionStatus::Blocked,
        });
    }
    None
}

/// Falls back to the step row status when no structured result JSON exists
/// (e.g. user-waived skipped steps).
fn fallback_status(step: &db::models::workflow_step::WorkflowStep) -> WorkflowTaskCompletionStatus {
    match step.status {
        db::models::workflow_types::WorkflowStepStatus::Completed => {
            WorkflowTaskCompletionStatus::Done
        }
        db::models::workflow_types::WorkflowStepStatus::Skipped => {
            WorkflowTaskCompletionStatus::DoneWithConcerns
        }
        _ => WorkflowTaskCompletionStatus::Blocked,
    }
}

fn structured_string_array(value: Option<&serde_json::Value>, key: &str) -> Vec<String> {
    value
        .and_then(|value| value.get(key))
        .and_then(|items| serde_json::from_value::<Vec<String>>(items.clone()).ok())
        .unwrap_or_default()
}

/// Reads `acceptance_results` from a persisted structured result. Loop review
/// items carry a `step_key`, which has no field on `WorkflowAcceptanceResult`;
/// it is folded into the criterion text so the merged conclusions stay
/// traceable (design §9.4).
fn structured_acceptance_results(
    value: Option<&serde_json::Value>,
) -> Vec<WorkflowAcceptanceResult> {
    let Some(value) = value else {
        return Vec::new();
    };
    let Some(items) = value
        .get("acceptance_results")
        .and_then(|items| items.as_array())
    else {
        return Vec::new();
    };
    let is_loop = value.get("type").and_then(|raw| raw.as_str()) == Some("loop_review_result");
    items
        .iter()
        .filter_map(|item| {
            let mut result: WorkflowAcceptanceResult = serde_json::from_value(item.clone()).ok()?;
            if is_loop
                && let Some(step_key) = item.get("step_key").and_then(|raw| raw.as_str())
            {
                result.criterion = format!("[{step_key}] {}", result.criterion);
            }
            Some(result)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acceptance_result(
        criterion: &str,
        level: AcceptanceCriterionLevel,
        verdict: WorkflowAcceptanceVerdict,
    ) -> WorkflowAcceptanceResult {
        WorkflowAcceptanceResult {
            criterion: criterion.to_string(),
            level,
            verdict,
            evidence: format!("{criterion} 的验证证据"),
        }
    }

    fn node_result(step_key: &str, status: WorkflowTaskCompletionStatus) -> FinalNodeResultInput {
        FinalNodeResultInput {
            step_key: step_key.to_string(),
            status,
            summary: format!("{step_key} 摘要"),
            outputs: Vec::new(),
            evidence: vec![format!("{step_key} 证据")],
            acceptance_results: Vec::new(),
            risks: Vec::new(),
            unfinished_items: Vec::new(),
            issues: Vec::new(),
        }
    }

    fn aggregation_input(latest_node_results: Vec<FinalNodeResultInput>) -> ResultAggregationInput {
        ResultAggregationInput {
            execution_id: Uuid::new_v4(),
            step_key: "workflow_result".to_string(),
            workflow_goal: "完成多节点并行任务并汇总结果".to_string(),
            title: "汇总多节点任务结果".to_string(),
            instructions: "汇总全部传递前驱的最新有效结果".to_string(),
            latest_node_results,
        }
    }

    fn construct(
        latest_node_results: Vec<FinalNodeResultInput>,
    ) -> Result<ResultAggregationOutput, WorkflowRuntimeError> {
        construct_result_review_output(&aggregation_input(latest_node_results))
    }

    #[test]
    fn merges_unions_with_order_preserving_dedup() {
        let mut first = node_result("node_a", WorkflowTaskCompletionStatus::DoneWithConcerns);
        first.outputs = vec!["a.rs".to_string(), "b.rs".to_string()];
        first.evidence = vec!["证据1".to_string(), "证据2".to_string()];
        first.risks = vec!["风险1".to_string(), "风险2".to_string()];
        first.unfinished_items = vec!["未完1".to_string()];
        first.issues = vec!["问题1".to_string()];

        let mut second = node_result("node_b", WorkflowTaskCompletionStatus::DoneWithConcerns);
        second.outputs = vec!["b.rs".to_string(), "c.rs".to_string()];
        second.evidence = vec!["证据2".to_string(), "证据3".to_string()];
        second.risks = vec!["风险2".to_string(), "风险3".to_string()];
        second.unfinished_items = vec!["未完1".to_string(), "未完2".to_string()];
        second.issues = vec!["问题2".to_string()];

        let output = construct(vec![first, second]).expect("aggregation should succeed");

        assert_eq!(output.deliverables, vec!["a.rs", "b.rs", "c.rs"]);
        assert_eq!(output.evidence, vec!["证据1", "证据2", "证据3"]);
        assert_eq!(output.risks, vec!["风险1", "风险2", "风险3"]);
        // unfinished_items 与 issues 的并集，保序去重。
        assert_eq!(
            output.unfinished_items,
            vec!["未完1", "问题1", "未完2", "问题2"]
        );
    }

    #[test]
    fn merges_acceptance_results_in_node_order_preserving_fields() {
        let mut first = node_result("review_a", WorkflowTaskCompletionStatus::Done);
        first.acceptance_results = vec![acceptance_result(
            "标准1",
            AcceptanceCriterionLevel::Required,
            WorkflowAcceptanceVerdict::Passed,
        )];
        let mut second = node_result("review_b", WorkflowTaskCompletionStatus::Done);
        second.acceptance_results = vec![
            acceptance_result(
                "标准2",
                AcceptanceCriterionLevel::Partial,
                WorkflowAcceptanceVerdict::Failed,
            ),
            acceptance_result(
                "标准3",
                AcceptanceCriterionLevel::Recommended,
                WorkflowAcceptanceVerdict::NotApplicable,
            ),
        ];

        let expected = vec![
            first.acceptance_results[0].clone(),
            second.acceptance_results[0].clone(),
            second.acceptance_results[1].clone(),
        ];
        let output = construct(vec![first, second]).expect("aggregation should succeed");

        assert_eq!(output.acceptance_results, expected);
        assert_eq!(
            output.acceptance_results[1].level,
            AcceptanceCriterionLevel::Partial
        );
    }

    #[test]
    fn derives_blocked_from_node_status() {
        for status in [
            WorkflowTaskCompletionStatus::Blocked,
            WorkflowTaskCompletionStatus::NeedsContext,
        ] {
            let output = construct(vec![
                node_result("node_a", WorkflowTaskCompletionStatus::Done),
                node_result("node_b", status),
            ])
            .expect("aggregation should succeed");
            assert_eq!(
                output.overall_status,
                WorkflowResultOverallStatus::Blocked,
                "node status {status:?} must block the overall result"
            );
        }
    }

    #[test]
    fn derives_blocked_from_required_level_failed_acceptance() {
        let mut node = node_result("node_a", WorkflowTaskCompletionStatus::Done);
        node.acceptance_results = vec![
            acceptance_result(
                "必要标准",
                AcceptanceCriterionLevel::Required,
                WorkflowAcceptanceVerdict::Failed,
            ),
            acceptance_result(
                "推荐标准",
                AcceptanceCriterionLevel::Recommended,
                WorkflowAcceptanceVerdict::Passed,
            ),
        ];
        let output = construct(vec![node]).expect("aggregation should succeed");
        assert_eq!(output.overall_status, WorkflowResultOverallStatus::Blocked);
    }

    #[test]
    fn blocked_takes_precedence_over_concerns() {
        let mut blocked = node_result("node_b", WorkflowTaskCompletionStatus::Blocked);
        blocked.risks = vec!["风险".to_string()];
        let output = construct(vec![
            node_result("node_a", WorkflowTaskCompletionStatus::DoneWithConcerns),
            blocked,
        ])
        .expect("aggregation should succeed");
        assert_eq!(output.overall_status, WorkflowResultOverallStatus::Blocked);
    }

    #[test]
    fn derives_completed_with_concerns_from_each_concern_source() {
        // done_with_concerns 节点。
        let output = construct(vec![node_result(
            "node_a",
            WorkflowTaskCompletionStatus::DoneWithConcerns,
        )])
        .expect("aggregation should succeed");
        assert_eq!(
            output.overall_status,
            WorkflowResultOverallStatus::CompletedWithConcerns
        );

        // partial 级 failed。
        let mut partial_failed = node_result("node_a", WorkflowTaskCompletionStatus::Done);
        partial_failed.acceptance_results = vec![acceptance_result(
            "部分标准",
            AcceptanceCriterionLevel::Partial,
            WorkflowAcceptanceVerdict::Failed,
        )];
        let output = construct(vec![partial_failed]).expect("aggregation should succeed");
        assert_eq!(
            output.overall_status,
            WorkflowResultOverallStatus::CompletedWithConcerns
        );

        // 非空 risks。
        let mut with_risks = node_result("node_a", WorkflowTaskCompletionStatus::Done);
        with_risks.risks = vec!["外部依赖未验证".to_string()];
        let output = construct(vec![with_risks]).expect("aggregation should succeed");
        assert_eq!(
            output.overall_status,
            WorkflowResultOverallStatus::CompletedWithConcerns
        );

        // 非空 unfinished_items / issues。
        let mut with_unfinished = node_result("node_a", WorkflowTaskCompletionStatus::Done);
        with_unfinished.unfinished_items = vec!["文档未补齐".to_string()];
        let output = construct(vec![with_unfinished]).expect("aggregation should succeed");
        assert_eq!(
            output.overall_status,
            WorkflowResultOverallStatus::CompletedWithConcerns
        );

        let mut with_issues = node_result("node_a", WorkflowTaskCompletionStatus::Done);
        with_issues.issues = vec!["遗留问题".to_string()];
        let output = construct(vec![with_issues]).expect("aggregation should succeed");
        assert_eq!(
            output.overall_status,
            WorkflowResultOverallStatus::CompletedWithConcerns
        );
    }

    #[test]
    fn derives_completed_when_all_clean() {
        let mut node = node_result("node_a", WorkflowTaskCompletionStatus::Done);
        node.acceptance_results = vec![
            acceptance_result(
                "必要标准",
                AcceptanceCriterionLevel::Required,
                WorkflowAcceptanceVerdict::Passed,
            ),
            acceptance_result(
                "不适用标准",
                AcceptanceCriterionLevel::Required,
                WorkflowAcceptanceVerdict::NotApplicable,
            ),
            // recommended 级 failed 不影响整体状态（设计 §9.4 未列入派生条件）。
            acceptance_result(
                "推荐标准",
                AcceptanceCriterionLevel::Recommended,
                WorkflowAcceptanceVerdict::Failed,
            ),
        ];
        let output = construct(vec![
            node,
            node_result("node_b", WorkflowTaskCompletionStatus::Done),
        ])
        .expect("aggregation should succeed");
        assert_eq!(
            output.overall_status,
            WorkflowResultOverallStatus::Completed
        );
        assert!(output.summary.contains('2'));
        assert!(output.summary.contains("已完成"));
    }

    #[test]
    fn allows_empty_acceptance_results_for_workflows_without_review_nodes() {
        let output = construct(vec![node_result(
            "node_a",
            WorkflowTaskCompletionStatus::Done,
        )])
        .expect("aggregation should succeed");
        assert!(output.acceptance_results.is_empty());
        assert_eq!(
            output.overall_status,
            WorkflowResultOverallStatus::Completed
        );
    }

    #[test]
    fn fails_when_no_node_results() {
        let result = construct(Vec::new());
        assert!(matches!(result, Err(WorkflowRuntimeError::Validation(_))));
    }

    #[test]
    fn fails_when_no_evidence() {
        let mut node = node_result("node_a", WorkflowTaskCompletionStatus::Done);
        node.evidence = Vec::new();
        let result = construct(vec![node]);
        assert!(matches!(result, Err(WorkflowRuntimeError::Validation(_))));
    }

    #[test]
    fn renders_all_template_sections_and_none_for_empty_risks() {
        let mut first = node_result("node_a", WorkflowTaskCompletionStatus::Done);
        first.outputs = vec!["crates/a.rs".to_string()];
        let mut second = node_result("node_b", WorkflowTaskCompletionStatus::DoneWithConcerns);
        second.outputs = vec!["docs/b.md".to_string()];

        let output = construct(vec![first, second]).expect("aggregation should succeed");

        for section in [
            "# 汇总多节点任务结果",
            "## 工作总目标",
            "完成多节点并行任务并汇总结果",
            "## 节点结果",
            "## 交付物",
            "## 风险与未完成项",
        ] {
            assert!(
                output.content.contains(section),
                "content missing section: {section}\n{}",
                output.content
            );
        }
        assert!(output.content.contains("- `node_a`（done）：node_a 摘要"));
        assert!(
            output
                .content
                .contains("- `node_b`（done_with_concerns）：node_b 摘要")
        );
        assert!(output.content.contains("- crates/a.rs"));
        assert!(output.content.contains("- docs/b.md"));
        // 风险与未完成项为空时写「无」。
        assert!(output.content.contains("## 风险与未完成项\n\n无\n"));
        // 摘要为包含状态与节点数的单段中文文本。
        assert!(output.summary.contains('2'));
        assert!(output.summary.contains("需关注"));
    }

    #[test]
    fn renders_risk_and_unfinished_items_when_present() {
        let mut node = node_result("node_a", WorkflowTaskCompletionStatus::DoneWithConcerns);
        node.risks = vec!["真实冒烟缺少凭据".to_string()];
        node.unfinished_items = vec!["补齐凭据后复跑".to_string()];
        let output = construct(vec![node]).expect("aggregation should succeed");
        assert!(output.content.contains("- 真实冒烟缺少凭据\n"));
        assert!(output.content.contains("- 补齐凭据后复跑\n"));
        assert!(!output.content.contains("## 风险与未完成项\n\n无\n"));
    }
}
