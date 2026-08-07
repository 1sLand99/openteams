//! Loop review typed input and prompt rendering (design §6.5、§11.4).
//!
//! The Loop review prompt audits the tasks inside `reviewScope` as a whole.
//! Its overall acceptance criteria come from the review node's plan-declared
//! tiered `acceptance` (§2 decision 14); the builder never invents criteria.
//! Self-check lists of scoped tasks are never included (§2 decision 12).

use db::models::workflow_types::AcceptanceCriteria;
use uuid::Uuid;

use super::protocol::loop_review_protocol_json_schema;

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
    /// The task's plan-declared tiered acceptance (at least `required`).
    pub acceptance: AcceptanceCriteria,
    pub summary: String,
    pub outputs: Vec<String>,
    /// Verification evidence lines reported by the worker.
    pub evidence: Vec<String>,
    /// User-approved skip waiver for this step, if any.
    pub user_skip_waiver: Option<String>,
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
    /// Loop-level overall acceptance declared by the review node in the plan.
    pub loop_acceptance: AcceptanceCriteria,
    /// Exactly the compiler-produced `reviewScope`; never widened here.
    pub review_scope: Vec<LoopReviewTaskInput>,
    /// (step_key, summary) pairs of out-of-scope upstream results required to
    /// understand the in-scope hand-offs.
    pub required_upstream_results: Vec<(String, String)>,
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
2. 每条验收标准必须恰好返回一个结论、级别和证据；覆盖 Loop 整体验收与 scope 内各 task 的验收项。
3. approved 要求所有 required 级验收项 passed（或 not_applicable 且证据充分）；partial 级未通过必须在 feedback 中给出明确外部归因；recommended 级不影响结论。
4. 任一 required 级未通过，或 partial 级未通过且无正当外部归因，必须 rejected。
5. 驳回时一次性列出本轮能发现的全部问题和具体修改方向；同一问题在后续审核中复用同一 issue_id。
6. step_feedbacks 只列需要返工的 task；空数组或省略表示整个 reviewScope 返工。
7. 用户批准的 skip waiver 是明确豁免，不得因被豁免步骤未重新执行而单独驳回。";

/// Builds the Loop review prompt following the §11.4 example and the §7.1
/// cache-friendly layout.
pub fn build_loop_review_prompt(input: &LoopReviewPromptInput) -> String {
    let mut sections: Vec<String> = Vec::new();

    // Category 1-2: fixed duty copy and run-level content.
    sections.push(
        "# 审核任务闭环\n\n将 reviewScope 内任务的最新有效结果作为一个整体进行审核。独立检查实际产物和任务间交接，不直接相信执行者总结。"
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

    let mut review_requirements = String::from("## 审核要求\n\n");
    review_requirements.push_str(input.review_instructions.trim());
    review_requirements.push_str("\n\n");
    review_requirements.push_str(LOOP_REVIEW_RULES);
    sections.push(review_requirements);

    if let Some(tiers) = render_acceptance_tiers(&input.loop_acceptance) {
        sections.push(format!(
            "## Loop 整体验收标准\n\n以下来自计划中 review 节点声明的验收标准，逐条审核，不得自创额外标准。\n\n{tiers}"
        ));
    }

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

    let upstreams = input
        .required_upstream_results
        .iter()
        .filter(|(step_key, summary)| !step_key.trim().is_empty() && !summary.trim().is_empty())
        .map(|(step_key, summary)| format!("- `{}`：{}", step_key.trim(), summary.trim()))
        .collect::<Vec<_>>();
    if !upstreams.is_empty() {
        sections.push(format!("## 必要上游结果\n\n{}", upstreams.join("\n")));
    }

    // Category 4: the single output JSON Schema.
    let allowed_step_keys = input
        .review_scope
        .iter()
        .map(|task| task.step_key.clone())
        .collect::<Vec<_>>();
    sections.push(format!(
        "## 输出 JSON Schema\n\n```json\n{}\n```",
        loop_review_protocol_json_schema(input.execution_id, &input.loop_key, &allowed_step_keys)
            .trim()
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

/// Renders one scoped task sub-section (§11.4): 任务说明、验收标准（分级）、
/// 最新结果、实际产物、验证证据、豁免说明。自检列表不出现。
fn render_scope_task(task: &LoopReviewTaskInput) -> String {
    let mut lines = vec![format!("- 任务说明：{}", task.instructions.trim())];
    if let Some(tiers) = render_task_acceptance(&task.acceptance) {
        lines.push(format!("- 验收标准：\n{tiers}"));
    }
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

/// Renders a task's acceptance with tier labels (§11.4 shows required 级为主，
/// partial/recommended 标注级别）。
fn render_task_acceptance(acceptance: &AcceptanceCriteria) -> Option<String> {
    let mut lines = Vec::new();
    for item in &acceptance.required {
        if !item.trim().is_empty() {
            lines.push(format!("  - （required）{}", item.trim()));
        }
    }
    for item in &acceptance.partial {
        if !item.trim().is_empty() {
            lines.push(format!("  - （partial）{}", item.trim()));
        }
    }
    for item in &acceptance.recommended {
        if !item.trim().is_empty() {
            lines.push(format!("  - （recommended）{}", item.trim()));
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// Renders the three acceptance tier groups for the loop-level criteria;
/// empty groups are omitted. Returns `None` when every tier is empty.
fn render_acceptance_tiers(acceptance: &AcceptanceCriteria) -> Option<String> {
    let mut groups: Vec<String> = Vec::new();
    let required = render_bullet_lines(&acceptance.required);
    if !required.is_empty() {
        groups.push(format!(
            "必须满足（required）——全部通过才能 approved：\n\n{required}"
        ));
    }
    let partial = render_bullet_lines(&acceptance.partial);
    if !partial.is_empty() {
        groups.push(format!(
            "允许外部归因（partial）——未通过时凭明确、可验证的外部归因放行并在 feedback 中记录：\n\n{partial}"
        ));
    }
    let recommended = render_bullet_lines(&acceptance.recommended);
    if !recommended.is_empty() {
        groups.push(format!(
            "建议满足（recommended）——不影响结论：\n\n{recommended}"
        ));
    }
    if groups.is_empty() {
        None
    } else {
        Some(groups.join("\n\n"))
    }
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

    fn acceptance(required: &[&str], partial: &[&str], recommended: &[&str]) -> AcceptanceCriteria {
        AcceptanceCriteria {
            required: required.iter().map(|item| item.to_string()).collect(),
            partial: partial.iter().map(|item| item.to_string()).collect(),
            recommended: recommended.iter().map(|item| item.to_string()).collect(),
        }
    }

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
            loop_acceptance: acceptance(
                &["cargo test -p executors --features qa-mode pi 通过"],
                &["真实冒烟缺凭据可记录为外部阻塞"],
                &[],
            ),
            review_scope: vec![
                LoopReviewTaskInput {
                    step_key: "backend_pi_types_runtime".to_string(),
                    title: "建立 Pi 强类型".to_string(),
                    instructions: "注册强类型与默认 profile。".to_string(),
                    acceptance: acceptance(&["cargo test pi 通过"], &[], &[]),
                    summary: "已完成 Pi 强类型 executor。".to_string(),
                    outputs: vec!["crates/executors/src/executors/pi.rs".to_string()],
                    evidence: vec!["20 项测试通过".to_string()],
                    user_skip_waiver: None,
                },
                LoopReviewTaskInput {
                    step_key: "backend_pi_provider_sync".to_string(),
                    title: "实现 models.json 同步".to_string(),
                    instructions: "原子写入并防泄密。".to_string(),
                    acceptance: acceptance(&["cargo test pi_models 通过"], &[], &[]),
                    summary: "已完成协调与原子写入。".to_string(),
                    outputs: vec!["crates/services/src/services/pi_models.rs".to_string()],
                    evidence: vec![],
                    user_skip_waiver: Some("用户批准保留跳过。".to_string()),
                },
            ],
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
        input.required_upstream_results =
            vec![("upstream_step".to_string(), "上游摘要".to_string())];
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
                "## Loop 整体验收标准",
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
    fn loop_acceptance_comes_from_review_node_declaration() {
        let prompt = build_loop_review_prompt(&sample_input());
        assert!(prompt.contains("以下来自计划中 review 节点声明的验收标准"));
        assert!(prompt.contains("必须满足（required）"));
        assert!(prompt.contains("cargo test -p executors --features qa-mode pi 通过"));
        assert!(prompt.contains("允许外部归因（partial）"));
        assert!(!prompt.contains("建议满足（recommended）"));
    }

    #[test]
    fn scope_tasks_show_acceptance_without_self_check() {
        let prompt = build_loop_review_prompt(&sample_input());
        assert!(prompt.contains("### backend_pi_types_runtime：建立 Pi 强类型"));
        assert!(prompt.contains("（required）cargo test pi 通过"));
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
        assert!(prompt.contains("\"loop_review_result\""));
        assert!(prompt.contains("\"level\""));
    }

    #[test]
    fn empty_optional_sections_are_fully_omitted() {
        let mut input = sample_input();
        input.loop_acceptance = AcceptanceCriteria::default();
        input.scope_edges = vec![];
        let prompt = build_loop_review_prompt(&input);
        assert!(!prompt.contains("## Loop 整体验收标准"));
        assert!(!prompt.contains("## reviewScope 内依赖关系"));
        assert!(!prompt.contains("## 必要上游结果"));
        assert!(!prompt.contains("## 最近一次 Loop 反馈"));
    }
}
