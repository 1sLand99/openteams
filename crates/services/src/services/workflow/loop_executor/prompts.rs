//! Loop review typed input and prompt rendering (design §6.5、§11.4).
//!
//! The Loop review prompt evaluates the review node's own acceptance cases.
//! Task results inside `reviewScope` are context only. The backend-provided
//! criterion list is the single source used by both this prompt and parser.

use db::models::workflow_types::AcceptanceCriterionLevel;
use uuid::Uuid;

use super::protocol::{LoopReviewCriterion, loop_review_protocol_json_schema};
use crate::services::{
    output_validation::{
        OutputValidationKind, OutputValidationReturnMode, WorkflowLoopReviewValidationContext,
        render_output_validation_instructions,
    },
    workflow_runtime::prompt_builders::common::{
        UPSTREAM_SECTION_TITLE, UpstreamResultInput, render_upstream_results,
    },
};

/// Reviewer identity shown in the prompt header (§6.5).
#[derive(Debug, Clone)]
pub struct ReviewerInput {
    pub name: String,
    pub role: String,
}

/// Latest valid result of one task inside the review scope (§6.5).
#[derive(Debug, Clone)]
pub struct LoopReviewTaskInput {
    pub step_key: String,
    pub title: String,
    pub instructions: String,
    pub summary: String,
    pub outputs: Vec<String>,
    /// Verification evidence lines reported by the worker.
    pub evidence: Vec<String>,
    /// User-approved skip waiver for this step, if any.
    pub user_skip_waiver: Option<String>,
}

/// Latest result of a step explicitly reworked in the current Loop round.
#[derive(Debug, Clone)]
pub struct LoopReworkAcceptanceInput {
    pub step_key: String,
    pub title: String,
    pub requirement: String,
    pub summary: String,
    pub outputs: Vec<String>,
    pub evidence: Vec<String>,
}

/// Typed input of the Loop review prompt (§6.5).
#[derive(Debug, Clone)]
pub struct LoopReviewPromptInput {
    pub execution_id: Uuid,
    pub loop_key: String,
    pub workflow_goal: String,
    pub reviewer: ReviewerInput,
    /// The review node's instructions (audit focus and scope notes).
    pub review_instructions: String,
    /// The sole acceptance contract shared with the protocol parser.
    pub acceptance_criteria: Vec<LoopReviewCriterion>,
    /// Exactly the compiler-produced `reviewScope`; never widened here.
    pub review_scope: Vec<LoopReviewTaskInput>,
    /// Steps reworked in the current Loop retry, with their current requirement
    /// and latest post-rework result.
    pub rework_acceptance: Vec<LoopReworkAcceptanceInput>,
    /// Latest valid results of direct predecessors outside `reviewScope` that
    /// are needed to understand the in-scope hand-offs.
    pub required_upstream_results: Vec<UpstreamResultInput>,
    /// Dependency edges inside the scope, rendered one per line.
    pub scope_edges: Vec<String>,
    pub current_round: i32,
    pub review_attempt: i32,
    pub retry_count: i32,
    pub retry_budget: i32,
    /// Latest effective loop feedback; attempt-level section after the schema.
    pub latest_loop_feedback: Option<String>,
    /// One full language-requirement line resolved by the caller.
    pub response_language: String,
}

const CLOSING_LINE: &str = "只返回一个匹配 Schema 的 JSON 对象。";

/// Fixed review rules appended to 审核要求 (byte-stable builder copy).
const LOOP_REVIEW_RULES: &str = "审核规则：
1. 独立检查实际产物和任务间交接，不直接相信执行者总结或测试声明。
2. results 必须恰好包含验收清单中的每个 id，不得遗漏或添加 id。
3. 只有实际验证通过才返回 passed=true；每项 evidence 必须说明判断依据。
4. required 的失败会由后端自动驳回；partial 和 recommended 只记录结果，不影响总结论。
5. 通过时 rework 必须是空对象；驳回时只把确实需要修改的 reviewScope step_key 写入 rework，值为该步骤的具体返工要求。
6. 用户批准的 skip waiver 是明确豁免，不得因被豁免步骤未重新执行而判定失败。";

/// Builds the Loop review prompt following the §11.4 example and the §7.1
/// cache-friendly layout.
pub fn build_loop_review_prompt(input: &LoopReviewPromptInput) -> String {
    let mut sections: Vec<String> = Vec::new();

    // Category 1-2: fixed duty copy and run-level content.
    sections.push(
        "# 审核任务闭环\n\n只验证当前 Loop Review 节点的验收清单。reviewScope 内任务的最新有效结果仅作为判断上下文，不重复验收任务节点自己的验收项。"
            .to_string(),
    );
    let response_language = input.response_language.trim();
    if !response_language.is_empty() {
        sections.push(response_language.to_string());
    }
    sections.push(format!("## 工作总目标\n\n{}", input.workflow_goal.trim()));

    // Category 3: node-level content.
    sections.push(format!(
        "## Loop 状态\n\n- Loop key：`{}`\n- 当前工作流轮次：{}\n- 当前审核尝试：{} / {}\n- 当前返工次数：{} / {}",
        input.loop_key.trim(),
        input.current_round,
        input.review_attempt,
        input.retry_budget.saturating_add(1),
        input.retry_count,
        input.retry_budget,
    ));

    if !input.rework_acceptance.is_empty() {
        sections.push(format!(
            "## 当前返工验收\n\n当前为返工验收环节。以下步骤是本轮返工目标，请依据返工要求和返工后的最新结果进行验收。\n\n{}",
            input
                .rework_acceptance
                .iter()
                .map(render_rework_acceptance)
                .collect::<Vec<_>>()
                .join("\n\n")
        ));
    }

    let mut review_requirements = String::from("## 审核要求\n\n");
    review_requirements.push_str(input.review_instructions.trim());
    review_requirements.push_str("\n\n");
    review_requirements.push_str(LOOP_REVIEW_RULES);
    sections.push(review_requirements);

    sections.push(format!(
        "## 验收清单\n\n这是唯一验收清单。`results` 必须使用下列 id 作为 key，每个 id 恰好一次。\n\n{}",
        input
            .acceptance_criteria
            .iter()
            .map(|item| {
                let level = match item.level {
                    AcceptanceCriterionLevel::Required => "required",
                    AcceptanceCriterionLevel::Partial => "partial",
                    AcceptanceCriterionLevel::Recommended => "recommended",
                };
                format!("- `{}` | `{}` | {}", item.id, level, item.criterion)
            })
            .collect::<Vec<_>>()
            .join("\n")
    ));

    let mut scope_sections: Vec<String> = Vec::new();
    for task in &input.review_scope {
        scope_sections.push(render_scope_task(task));
    }
    if !scope_sections.is_empty() {
        sections.push(format!("## reviewScope\n\n{}", scope_sections.join("\n\n")));
    }

    let edges = render_bullet_lines(&input.scope_edges);
    if !edges.is_empty() {
        sections.push(format!("## reviewScope 内依赖关系\n\n{edges}"));
    }

    if let Some(upstream) = render_upstream_results(&input.required_upstream_results) {
        sections.push(format!("{UPSTREAM_SECTION_TITLE}\n\n{upstream}"));
    }

    // Category 4: the single output JSON Schema.
    sections.push(format!(
        "## 输出 JSON Schema\n\n```json\n{}\n```",
        loop_review_protocol_json_schema(
            input.execution_id,
            &input.loop_key,
            &input.acceptance_criteria,
            &input
                .review_scope
                .iter()
                .map(|task| task.step_key.clone())
                .collect::<Vec<_>>(),
        )
        .trim()
    ));
    sections.push(render_output_validation_instructions(
        OutputValidationKind::WorkflowLoopReview,
        &WorkflowLoopReviewValidationContext {
            execution_id: input.execution_id,
            loop_key: input.loop_key.clone(),
            criteria: input.acceptance_criteria.clone(),
            allowed_step_keys: input
                .review_scope
                .iter()
                .map(|task| task.step_key.clone())
                .collect(),
        },
        OutputValidationReturnMode::JsonOnly,
    ));

    // Category 5: attempt-level content, after the schema.
    if let Some(feedback) = &input.latest_loop_feedback {
        let feedback = feedback.trim();
        if !feedback.is_empty() {
            sections.push(format!("## 最近一次 Loop 反馈\n\n{feedback}"));
        }
    }

    // Category 6: fixed closing line.
    format!("{}\n\n{CLOSING_LINE}\n", sections.join("\n\n"))
}

/// Renders one scoped task as context. Task acceptance is intentionally absent
/// because it was already evaluated by the task node.
fn render_scope_task(task: &LoopReviewTaskInput) -> String {
    let mut lines = vec![format!("- 任务说明：{}", task.instructions.trim())];
    lines.push(format!("- 最新结果：{}", task.summary.trim()));
    let outputs = render_inline_code_list(&task.outputs);
    if !outputs.is_empty() {
        lines.push(format!("- 实际产物：{outputs}"));
    }
    let evidence = render_bullet_lines(&task.evidence);
    if !evidence.is_empty() {
        lines.push(format!("- 验证证据：\n{evidence}"));
    }
    if let Some(waiver) = &task.user_skip_waiver {
        let waiver = waiver.trim();
        if !waiver.is_empty() {
            lines.push(format!("- 用户豁免：{waiver}"));
        }
    }
    format!(
        "### {}：{}\n\n{}",
        task.step_key.trim(),
        task.title.trim(),
        lines.join("\n")
    )
}

fn render_rework_acceptance(item: &LoopReworkAcceptanceInput) -> String {
    let mut lines = vec![format!("- 当前返工要求：{}", item.requirement.trim())];
    lines.push(format!("- 返工后的最新结果：{}", item.summary.trim()));
    let outputs = render_inline_code_list(&item.outputs);
    if !outputs.is_empty() {
        lines.push(format!("- 返工后的实际产物：{outputs}"));
    }
    let evidence = render_bullet_lines(&item.evidence);
    if !evidence.is_empty() {
        lines.push(format!("- 返工后的验证证据：\n{evidence}"));
    }
    format!(
        "### {}：{}\n\n{}",
        item.step_key.trim(),
        item.title.trim(),
        lines.join("\n")
    )
}

fn render_bullet_lines(items: &[String]) -> String {
    items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_inline_code_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join("、")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXECUTION_ID: &str = "03f1e4a4-8745-4db7-a69c-1f4d09dcc8ca";

    fn sample_input() -> LoopReviewPromptInput {
        LoopReviewPromptInput {
            execution_id: Uuid::parse_str(EXECUTION_ID).unwrap(),
            loop_key: "loop-lead_backend_review".to_string(),
            workflow_goal: "将 Pi 注册为完整一等 Agent。".to_string(),
            reviewer: ReviewerInput {
                name: "Lead".to_string(),
                role: "负责后端审核".to_string(),
            },
            review_instructions: "对照设计审核前三项后端交付。".to_string(),
            acceptance_criteria: vec![
                LoopReviewCriterion {
                    id: "c1".to_string(),
                    level: AcceptanceCriterionLevel::Required,
                    criterion: "cargo test -p executors --features qa-mode pi 通过".to_string(),
                },
                LoopReviewCriterion {
                    id: "c2".to_string(),
                    level: AcceptanceCriterionLevel::Partial,
                    criterion: "真实冒烟缺凭据可记录为外部阻塞".to_string(),
                },
            ],
            review_scope: vec![
                LoopReviewTaskInput {
                    step_key: "backend_pi_types_runtime".to_string(),
                    title: "建立 Pi 强类型".to_string(),
                    instructions: "注册强类型与默认 profile。".to_string(),
                    summary: "已完成 Pi 强类型 executor。".to_string(),
                    outputs: vec!["crates/executors/src/executors/pi.rs".to_string()],
                    evidence: vec!["20 项测试通过".to_string()],
                    user_skip_waiver: None,
                },
                LoopReviewTaskInput {
                    step_key: "backend_pi_provider_sync".to_string(),
                    title: "实现 models.json 同步".to_string(),
                    instructions: "原子写入并防泄密。".to_string(),
                    summary: "已完成协调与原子写入。".to_string(),
                    outputs: vec!["crates/services/src/services/pi_models.rs".to_string()],
                    evidence: vec![],
                    user_skip_waiver: Some("用户批准保留跳过。".to_string()),
                },
            ],
            rework_acceptance: vec![],
            required_upstream_results: vec![],
            scope_edges: vec![
                "`backend_pi_types_runtime` → `backend_pi_provider_sync`".to_string(),
            ],
            current_round: 1,
            review_attempt: 1,
            retry_count: 0,
            retry_budget: 3,
            latest_loop_feedback: None,
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
        input.required_upstream_results = vec![UpstreamResultInput {
            step_key: "upstream_step".to_string(),
            summary: "上游摘要".to_string(),
            outputs: vec!["upstream/output.md".to_string()],
        }];
        input.latest_loop_feedback = Some("上轮 loop 反馈".to_string());
        let prompt = build_loop_review_prompt(&input);
        assert_ascending(
            &prompt,
            &[
                "# 审核任务闭环",
                "人类可读内容使用简体中文。",
                "## 工作总目标",
                "## Loop 状态",
                "## 审核要求",
                "## 验收清单",
                "## reviewScope",
                "## reviewScope 内依赖关系",
                "## 必要上游结果",
                "## 输出 JSON Schema",
                "```json",
                "## 最近一次 Loop 反馈",
                CLOSING_LINE,
            ],
        );
        assert!(prompt.ends_with(&format!("{CLOSING_LINE}\n")));
    }

    #[test]
    fn build_is_byte_stable_for_same_input() {
        let input = sample_input();
        assert_eq!(
            build_loop_review_prompt(&input),
            build_loop_review_prompt(&input)
        );
    }

    #[test]
    fn acceptance_contract_is_rendered_once_with_ids() {
        let prompt = build_loop_review_prompt(&sample_input());
        assert!(prompt.contains("这是唯一验收清单"));
        assert!(prompt.contains("`c1` | `required`"));
        assert!(prompt.contains("`c2` | `partial`"));
        assert!(prompt.contains("cargo test -p executors --features qa-mode pi 通过"));
    }

    #[test]
    fn current_rework_acceptance_is_rendered_in_a_dedicated_section() {
        let mut input = sample_input();
        input.rework_acceptance = vec![LoopReworkAcceptanceInput {
            step_key: "backend_pi_provider_sync".to_string(),
            title: "实现 models.json 同步".to_string(),
            requirement: "补充原子写入失败回滚".to_string(),
            summary: "已补充临时文件清理和回滚".to_string(),
            outputs: vec!["crates/services/src/services/pi_models.rs".to_string()],
            evidence: vec!["pi_models 回滚测试通过".to_string()],
        }];

        let prompt = build_loop_review_prompt(&input);
        assert!(prompt.contains("## 当前返工验收"));
        assert!(prompt.contains("当前为返工验收环节"));
        assert!(prompt.contains("- 当前返工要求：补充原子写入失败回滚"));
        assert!(prompt.contains("- 返工后的最新结果：已补充临时文件清理和回滚"));
        assert!(prompt.contains("- 返工后的验证证据：\n- pi_models 回滚测试通过"));
    }

    #[test]
    fn scope_tasks_show_results_without_repeating_acceptance() {
        let prompt = build_loop_review_prompt(&sample_input());
        assert!(prompt.contains("### backend_pi_types_runtime：建立 Pi 强类型"));
        assert!(!prompt.contains("cargo test pi 通过"));
        assert!(!prompt.contains("cargo test pi_models 通过"));
        assert!(!prompt.contains("自检"));
        assert!(prompt.contains("- 用户豁免：用户批准保留跳过。"));
    }

    #[test]
    fn feedback_section_only_after_schema_and_when_present() {
        let plain = build_loop_review_prompt(&sample_input());
        assert!(!plain.contains("## 最近一次 Loop 反馈"));

        let mut input = sample_input();
        input.latest_loop_feedback = Some("补充审核点".to_string());
        let prompt = build_loop_review_prompt(&input);
        let schema_index = prompt.find("## 输出 JSON Schema").unwrap();
        let feedback_index = prompt.find("## 最近一次 Loop 反馈").unwrap();
        assert!(feedback_index > schema_index);
    }

    #[test]
    fn loop_state_counters_render_with_budget() {
        let prompt = build_loop_review_prompt(&sample_input());
        assert!(prompt.contains("- 当前审核尝试：1 / 4"));
        assert!(prompt.contains("- 当前返工次数：0 / 3"));
    }

    #[test]
    fn prompt_has_no_data_boundary_and_single_schema() {
        let prompt = build_loop_review_prompt(&sample_input());
        assert!(!prompt.contains("openteams_untrusted_data"));
        assert!(!prompt.contains("Data Boundary"));
        assert!(!prompt.contains("Output Protocol"));
        assert_eq!(prompt.matches("```json").count(), 1);
        assert!(prompt.contains("\"kind\": \"workflow_loop_review\""));
        assert!(prompt.contains("POST $OPENTEAMS_OUTPUT_VALIDATION_URL"));
        assert!(prompt.contains("\"loop_review_result\""));
        assert!(prompt.contains("\"passed\""));
        assert!(prompt.contains("\"rework\""));
        assert!(!prompt.contains("\"verdict\""));
    }

    #[test]
    fn empty_optional_sections_are_fully_omitted() {
        let mut input = sample_input();
        input.scope_edges = vec![];
        let prompt = build_loop_review_prompt(&input);
        assert!(!prompt.contains("## reviewScope 内依赖关系"));
        assert!(!prompt.contains("## 必要上游结果"));
        assert!(!prompt.contains("## 最近一次 Loop 反馈"));
    }
}
