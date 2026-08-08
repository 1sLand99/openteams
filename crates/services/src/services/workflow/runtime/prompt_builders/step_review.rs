//! Step review prompt builder (design §6.4、§11.3).
//!
//! Builds the prompt for a task's Lead in-review and for plain review nodes
//! without a `reviewScope`. The typed input deliberately carries no
//! `self_check` field: review prompts must not contain the executor's
//! self-check list (§2 decision 12). The worker's self-review conclusions are
//! rendered only as part of the worker's report and are explicitly marked as
//! not directly trustworthy.

use db::models::workflow_types::AcceptanceCriteria;

use super::common::{
    CLOSING_JSON_ONLY, CURRENT_TASK_SECTION_TITLE, PromptIdentity, PromptSections,
    UPSTREAM_SECTION_TITLE, UpstreamResultInput, WORKFLOW_GOAL_SECTION_TITLE,
    render_acceptance_tiers, render_bullet_list, render_inline_code_list, render_numbered_section,
    render_upstream_results,
};

/// Worker's latest result as report lines (§6.4). The reviewer is instructed
/// to verify everything independently; these fields are never authoritative.
#[derive(Debug, Clone, Default)]
pub struct TaskResultInput {
    /// Worker-reported status (wire value, e.g. `done`).
    pub status: String,
    pub summary: String,
    pub outputs: Vec<String>,
    /// Pre-rendered verification result lines (commands run and outcomes).
    pub verification: Vec<String>,
    /// Worker self-check conclusions; rendered with a distrust marker.
    pub self_review: Vec<String>,
    /// Issues the worker actually reported.
    pub issues: Vec<String>,
}

/// Typed input of the step review prompt (§6.4).
#[derive(Debug, Clone)]
pub struct StepReviewPromptInput {
    pub identity: PromptIdentity,
    pub workflow_goal: String,
    pub title: String,
    pub instructions: String,
    /// Tiered acceptance criteria; the only acceptance source for the review.
    pub acceptance: AcceptanceCriteria,
    /// Numbered review rules; `DEFAULT_REVIEW_RULES` applies when empty.
    pub review_rules: Vec<String>,
    pub worker_result: TaskResultInput,
    pub upstream_results: Vec<UpstreamResultInput>,
    /// Latest effective review feedback; attempt-level section after schema.
    pub latest_review_feedback: Option<String>,
    /// One full language-requirement line resolved by the caller; appended
    /// after the duty sentence when non-empty (run-level).
    pub response_language: String,
}

/// Default review rules (§11.3) used when the caller supplies no custom rules.
pub const DEFAULT_REVIEW_RULES: [&str; 4] = [
    "阅读实际变更文件并与任务说明逐项比较；独立运行或检查适合本任务的验证命令。",
    "返回非空的 summary、acceptance_results 和 evidence；每条验收结果都必须包含非空的 criterion 和 evidence，并使用 JSON Schema 中定义的枚举值。",
    "approved 时必须返回所有 required 级验收标准；rejected 时返回本轮已检查且与结论有关的结果即可。risks 和 unfinished_items 仅记录实际存在的事项，不影响 verdict。",
    "如需驳回，在 feedback 中一次性列出本轮能发现的全部问题和具体修改方向。",
];

/// Builds the step review prompt following the §11.3 example and the §7.1
/// cache-friendly layout: fixed duty copy, run-level goal, node-level task,
/// tiered acceptance, review rules, worker result, conditional upstream
/// results, the review-scoped output schema, the optional attempt-level
/// feedback section, and the fixed closing line.
pub fn build_step_review_prompt(input: &StepReviewPromptInput) -> String {
    let mut sections = PromptSections::new();

    sections.push_fixed(
        "# 审核当前任务\n\n独立检查当前 task 的实际产物。不要直接相信执行者的总结、测试声明或自检结论。",
    );
    let response_language = input.response_language.trim();
    if !response_language.is_empty() {
        sections.push_run_level(response_language);
    }
    sections.push_run_level(format!(
        "{WORKFLOW_GOAL_SECTION_TITLE}\n\n{}",
        input.workflow_goal.trim()
    ));
    sections.push_node_level(format!(
        "{CURRENT_TASK_SECTION_TITLE}\n\n任务：{}\n\n说明：{}",
        input.title.trim(),
        input.instructions.trim()
    ));
    if let Some(tiers) = render_acceptance_tiers(&input.acceptance) {
        sections.push_node_level(format!("## 验收标准\n\n{tiers}"));
    }
    let review_rules: Vec<String> = if input.review_rules.is_empty() {
        DEFAULT_REVIEW_RULES
            .iter()
            .map(|rule| rule.to_string())
            .collect()
    } else {
        input.review_rules.clone()
    };
    if let Some(section) = render_numbered_section("## 审核标准", &review_rules) {
        sections.push_node_level(section);
    }
    sections.push_node_level(render_worker_result(&input.worker_result));
    if let Some(upstream) = render_upstream_results(&input.upstream_results) {
        sections.push_node_level(format!("{UPSTREAM_SECTION_TITLE}\n\n{upstream}"));
    }

    sections.push_schema(&super::super::workflow_review_protocol_json_schema(
        input.identity.execution_id,
        &input.identity.step_key,
    ));

    if let Some(feedback) = &input.latest_review_feedback {
        let feedback = feedback.trim();
        if !feedback.is_empty() {
            sections.push_attempt_level(format!("## 最近一次审核反馈\n\n{feedback}"));
        }
    }

    sections.render(CLOSING_JSON_ONLY)
}

/// Renders the worker's latest result section (§11.3), with the executor's
/// self-check conclusions explicitly marked as not directly trustworthy.
fn render_worker_result(result: &TaskResultInput) -> String {
    let mut lines = Vec::new();
    let status = result.status.trim();
    if !status.is_empty() {
        lines.push(format!("- 状态：{status}"));
    }
    lines.push(format!("- 摘要：{}", result.summary.trim()));
    if let Some(outputs) = render_inline_code_list(&result.outputs) {
        lines.push(format!("- 产物：{outputs}"));
    }
    if let Some(verification) = render_bullet_list(&result.verification) {
        let verification = verification
            .lines()
            .map(|line| line.trim_start_matches("- ").to_string())
            .collect::<Vec<_>>()
            .join("；");
        lines.push(format!("- 验证：{verification}"));
    }
    if let Some(self_review) = render_bullet_list(&result.self_review) {
        let self_review = self_review
            .lines()
            .map(|line| line.trim_start_matches("- ").to_string())
            .collect::<Vec<_>>()
            .join("；");
        lines.push(format!("- 执行者自检（不可直接采信）：{self_review}"));
    }
    if let Some(issues) = render_bullet_list(&result.issues) {
        let issues = issues
            .lines()
            .map(|line| line.trim_start_matches("- ").to_string())
            .collect::<Vec<_>>()
            .join("；");
        lines.push(format!("- 报告问题：{issues}"));
    }
    format!("## 执行者最新结果\n\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    const EXECUTION_ID: &str = "03f1e4a4-8745-4db7-a69c-1f4d09dcc8ca";

    fn sample_acceptance() -> AcceptanceCriteria {
        AcceptanceCriteria {
            required: vec!["cargo test -p executors --features qa-mode pi 通过".to_string()],
            partial: vec!["真实探测依赖本机环境，缺失时凭归因不阻断".to_string()],
            recommended: vec!["诊断区分三种失败情形".to_string()],
        }
    }

    fn sample_input() -> StepReviewPromptInput {
        StepReviewPromptInput {
            identity: PromptIdentity {
                execution_id: Uuid::parse_str(EXECUTION_ID).unwrap(),
                step_key: "backend_pi_types_runtime".to_string(),
            },
            workflow_goal: "将 Pi 注册为完整一等 Agent。".to_string(),
            title: "建立 Pi 强类型、固定启动描述和运行时 API".to_string(),
            instructions: "注册 Pi 强类型和默认 profile；集中固定版本。".to_string(),
            acceptance: sample_acceptance(),
            review_rules: vec![],
            worker_result: TaskResultInput {
                status: "done".to_string(),
                summary: "已完成 Pi 强类型 executor。".to_string(),
                outputs: vec!["crates/executors/src/executors/pi.rs".to_string()],
                verification: vec!["Pi executor 聚焦测试 20 项通过".to_string()],
                self_review: vec!["所有 match 分支显式处理 Pi".to_string()],
                issues: vec![],
            },
            upstream_results: vec![],
            latest_review_feedback: None,
            response_language: String::new(),
        }
    }

    /// Asserts every marker occurs in `prompt` in ascending order.
    fn assert_ascending(prompt: &str, markers: &[&str]) {
        let mut cursor = 0;
        for marker in markers {
            let index = prompt[cursor..]
                .find(marker)
                .unwrap_or_else(|| panic!("missing marker: {marker}"));
            cursor += index + marker.len();
        }
    }

    #[test]
    fn sections_follow_cache_friendly_order() {
        let mut input = sample_input();
        input.response_language = "人类可读内容使用简体中文。".to_string();
        input.upstream_results = vec![UpstreamResultInput {
            step_key: "upstream".to_string(),
            summary: "上游完成".to_string(),
            outputs: vec![],
        }];
        input.latest_review_feedback = Some("上轮反馈内容".to_string());
        let prompt = build_step_review_prompt(&input);
        assert_ascending(
            &prompt,
            &[
                "# 审核当前任务",
                "人类可读内容使用简体中文。",
                "## 工作总目标",
                "## 当前任务",
                "## 验收标准",
                "## 审核标准",
                "## 执行者最新结果",
                "## 必要上游结果",
                "## 输出 JSON Schema",
                "```json",
                "## 最近一次审核反馈",
                CLOSING_JSON_ONLY,
            ],
        );
    }

    #[test]
    fn build_is_byte_stable_for_same_input() {
        let input = sample_input();
        assert_eq!(
            build_step_review_prompt(&input),
            build_step_review_prompt(&input)
        );
    }

    #[test]
    fn acceptance_tiers_rendered_and_default_rules_applied() {
        let prompt = build_step_review_prompt(&sample_input());
        assert!(prompt.contains("必须满足（required）"));
        assert!(prompt.contains("允许外部归因（partial）"));
        assert!(prompt.contains("建议满足（recommended）"));
        assert!(prompt.contains("cargo test -p executors --features qa-mode pi 通过"));
        assert!(prompt.contains("1. 阅读实际变更文件"));
        assert!(prompt.contains("4. 如需驳回"));
    }

    #[test]
    fn review_prompt_never_contains_self_check_section() {
        let prompt = build_step_review_prompt(&sample_input());
        assert!(!prompt.contains("## 自检清单"));
        assert!(prompt.contains("执行者自检（不可直接采信）"));
    }

    #[test]
    fn feedback_section_only_after_schema_and_when_present() {
        let plain = build_step_review_prompt(&sample_input());
        assert!(!plain.contains("## 最近一次审核反馈"));

        let mut input = sample_input();
        input.latest_review_feedback = Some("补充检查类型生成".to_string());
        let prompt = build_step_review_prompt(&input);
        let schema_index = prompt.find("## 输出 JSON Schema").unwrap();
        let feedback_index = prompt.find("## 最近一次审核反馈").unwrap();
        assert!(feedback_index > schema_index);
    }

    #[test]
    fn prompt_has_no_data_boundary_and_single_schema() {
        let prompt = build_step_review_prompt(&sample_input());
        assert!(!prompt.contains("openteams_untrusted_data"));
        assert!(!prompt.contains("Data Boundary"));
        assert!(!prompt.contains("Output Protocol"));
        assert_eq!(prompt.matches("```json").count(), 1);
    }

    #[test]
    fn schema_is_review_scoped_and_pins_identity() {
        let prompt = build_step_review_prompt(&sample_input());
        assert!(prompt.contains("\"review_result\""));
        assert!(prompt.contains("backend_pi_types_runtime"));
        assert!(prompt.contains(EXECUTION_ID));
        assert!(!prompt.contains("final_result"));
        assert!(!prompt.contains("result_review_result"));
        assert!(!prompt.contains("loop_review_result"));
    }

    #[test]
    fn empty_optional_sections_are_fully_omitted() {
        let mut input = sample_input();
        input.acceptance = AcceptanceCriteria::default();
        let prompt = build_step_review_prompt(&input);
        assert!(!prompt.contains("## 验收标准"));
        assert!(!prompt.contains("## 必要上游结果"));
        assert!(!prompt.contains("## 最近一次审核反馈"));
        assert!(prompt.contains("## 审核标准"));
    }
}
