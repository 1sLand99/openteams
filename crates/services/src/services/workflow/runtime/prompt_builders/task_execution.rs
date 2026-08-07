//! Task execution prompt builder (design §6.3、§11.2).
//!
//! Builds the prompt for a task node's first run (`revision = None`) and for
//! rework after Lead / Loop reviewer / user feedback (`revision = Some`).
//! The typed contract deliberately carries no acceptance criteria, so they
//! cannot leak into the executor prompt (§2 decision 12).

use db::models::workflow_types::WorkflowStepType;

use super::{
    super::{WorkflowRevisionFeedbackSource, workflow_step_protocol_json_schema_for_step},
    common::{
        CLOSING_JSON_ONLY, CURRENT_TASK_SECTION_TITLE, PromptIdentity, PromptSections,
        UPSTREAM_SECTION_TITLE, UpstreamResultInput, WORKFLOW_GOAL_SECTION_TITLE,
        render_bullet_list, render_bullet_section, render_inline_code_list,
        render_upstream_results,
    },
};

/// Task execution contract (§6.3): deliberately has no `acceptance` field so
/// acceptance criteria cannot reach the executor at the type level.
#[derive(Debug, Clone, Default)]
pub struct TaskExecutionContractInput {
    /// Expected output paths or deliverables.
    pub outputs: Vec<String>,
    /// Self-check list the executor walks through before reporting.
    pub self_check: Vec<String>,
    /// Objective verification commands or methods.
    pub verification_methods: Vec<String>,
    /// Evidence the executor must provide on completion.
    pub completion_evidence: Vec<String>,
}

/// Revision context for rework (§6.3): only the latest effective feedback,
/// never the full review history.
#[derive(Debug, Clone)]
pub struct RevisionContextInput {
    /// Who produced the feedback that triggered this rework.
    pub source: WorkflowRevisionFeedbackSource,
    /// Current attempt number (attempt-level content, rendered after the schema).
    pub attempt: i32,
    /// Latest effective feedback.
    pub feedback: String,
    /// Result summary of the previous attempt.
    pub previous_summary: String,
    /// Outputs of the previous attempt.
    pub previous_outputs: Vec<String>,
}

/// Typed input of the task execution prompt (§6.3).
#[derive(Debug, Clone)]
pub struct TaskExecutionPromptInput {
    pub identity: PromptIdentity,
    pub workflow_goal: String,
    pub title: String,
    pub instructions: String,
    pub contract: TaskExecutionContractInput,
    pub upstream_results: Vec<UpstreamResultInput>,
    /// `None` on the first run; the whole revision section is omitted then.
    pub revision: Option<RevisionContextInput>,
    /// One full language-requirement line resolved by the caller from the UI
    /// language; appended after the duty sentence when non-empty (run-level).
    pub response_language: String,
}

/// Builds the task execution prompt following the §11.2 example and the §7.1
/// cache-friendly layout: fixed duty copy, run-level goal, node-level task and
/// contract, conditional upstream results, the task-scoped output schema, the
/// optional attempt-level revision section, and the fixed closing line.
pub fn build_task_execution_prompt(input: &TaskExecutionPromptInput) -> String {
    let mut sections = PromptSections::new();

    sections.push_fixed(
        "# 执行任务\n\n完成当前工作流 task。只处理当前任务范围，完成后按 Schema 返回实际结果。",
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
        "{CURRENT_TASK_SECTION_TITLE}\n\n任务：{}\n\n{}",
        input.title.trim(),
        input.instructions.trim()
    ));
    if let Some(section) = render_bullet_section("## 预期产物", &input.contract.outputs) {
        sections.push_node_level(section);
    }
    if let Some(self_check) = render_bullet_list(&input.contract.self_check) {
        sections.push_node_level(format!("## 自检清单\n\n完成前逐项自检：\n\n{self_check}"));
    }
    if let Some(section) =
        render_bullet_section("## 验证方式", &input.contract.verification_methods)
    {
        sections.push_node_level(section);
    }
    if let Some(section) = render_bullet_section("## 完成证据", &input.contract.completion_evidence)
    {
        sections.push_node_level(section);
    }
    if let Some(upstream) = render_upstream_results(&input.upstream_results) {
        sections.push_node_level(format!("{UPSTREAM_SECTION_TITLE}\n\n{upstream}"));
    }

    sections.push_schema(&workflow_step_protocol_json_schema_for_step(
        input.identity.execution_id,
        &input.identity.step_key,
        true,
        &WorkflowStepType::Task,
    ));

    if let Some(revision) = &input.revision {
        sections.push_attempt_level(render_revision_section(revision));
    }

    sections.render(CLOSING_JSON_ONLY)
}

/// Renders the attempt-level revision section (§11.2), placed after the
/// output schema so retry/rework calls share the same byte-stable prefix.
fn render_revision_section(revision: &RevisionContextInput) -> String {
    let mut lines = vec![
        format!("- 来源：{}", revision_source_label(&revision.source)),
        format!("- 当前尝试：{}", revision.attempt),
        format!("- 最近反馈：{}", revision.feedback.trim()),
        format!("- 上次结果摘要：{}", revision.previous_summary.trim()),
    ];
    if let Some(outputs) = render_inline_code_list(&revision.previous_outputs) {
        lines.push(format!("- 上次产物：{outputs}"));
    }
    format!(
        "## 本次修订\n\n{}\n\n按照审核意见完整修复：逐条解决反馈指出的全部问题，不得只修复部分问题、不得用解释代替修复；未受反馈影响的既有正确成果保持不变。",
        lines.join("\n")
    )
}

/// Maps the revision feedback source to its Chinese label. `Reviewer` is only
/// produced by the loop executor today, hence 「Loop 审核」 (§11.2 example).
fn revision_source_label(source: &WorkflowRevisionFeedbackSource) -> &'static str {
    match source {
        WorkflowRevisionFeedbackSource::Lead => "Lead 审核",
        WorkflowRevisionFeedbackSource::Reviewer => "Loop 审核",
        WorkflowRevisionFeedbackSource::User => "用户反馈",
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    const EXECUTION_ID: &str = "03f1e4a4-8745-4db7-a69c-1f4d09dcc8ca";

    fn sample_input() -> TaskExecutionPromptInput {
        TaskExecutionPromptInput {
            identity: PromptIdentity {
                execution_id: Uuid::parse_str(EXECUTION_ID).unwrap(),
                step_key: "backend_pi_types_runtime".to_string(),
            },
            workflow_goal: "将 Pi 注册为完整一等 Agent，实现供应商配置同步与能力隔离。".to_string(),
            title: "建立 Pi 强类型、固定启动描述和运行时 API".to_string(),
            instructions: "1. 注册 Pi 强类型变体。\n2. 集中定义三个精确版本，禁止 latest。"
                .to_string(),
            contract: TaskExecutionContractInput {
                outputs: vec!["crates/executors/src/executors/pi.rs".to_string()],
                self_check: vec!["先添加 Pi 序列化的失败测试。".to_string()],
                verification_methods: vec![
                    "cargo test -p executors --features qa-mode pi".to_string(),
                ],
                completion_evidence: vec!["提供固定版本命令和聚焦测试摘要".to_string()],
            },
            upstream_results: vec![UpstreamResultInput {
                step_key: "backend_pi_provider_sync".to_string(),
                summary: "已完成 models.json 原子协调与安全触发。".to_string(),
                outputs: vec!["crates/services/src/services/pi_models.rs".to_string()],
            }],
            revision: None,
            response_language: String::new(),
        }
    }

    fn sample_revision(source: WorkflowRevisionFeedbackSource) -> RevisionContextInput {
        RevisionContextInput {
            source,
            attempt: 2,
            feedback: "在 models.json 写入边界增加字面量编码的往返测试。".to_string(),
            previous_summary: "已完成 Pi 模型协调返工。".to_string(),
            previous_outputs: vec![
                "crates/services/src/services/pi_models.rs".to_string(),
                "crates/services/src/services/cli_config.rs".to_string(),
            ],
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
        input.revision = Some(sample_revision(WorkflowRevisionFeedbackSource::Reviewer));
        let prompt = build_task_execution_prompt(&input);
        assert_ascending(
            &prompt,
            &[
                "# 执行任务",
                "完成当前工作流 task。只处理当前任务范围，完成后按 Schema 返回实际结果。",
                "人类可读内容使用简体中文。",
                "## 工作总目标",
                "## 当前任务",
                "## 预期产物",
                "## 自检清单",
                "## 验证方式",
                "## 完成证据",
                "## 必要上游结果",
                "## 输出 JSON Schema",
                "```json",
                "## 本次修订",
                "按照审核意见完整修复",
                CLOSING_JSON_ONLY,
            ],
        );
        assert!(prompt.ends_with(&format!("{CLOSING_JSON_ONLY}\n")));
    }

    #[test]
    fn build_is_byte_stable_for_same_input() {
        let input = sample_input();
        assert_eq!(
            build_task_execution_prompt(&input),
            build_task_execution_prompt(&input)
        );
    }

    #[test]
    fn revision_prefix_is_byte_identical_to_first_run() {
        let first_run = build_task_execution_prompt(&sample_input());
        let mut revised_input = sample_input();
        revised_input.revision = Some(sample_revision(WorkflowRevisionFeedbackSource::Lead));
        let rework = build_task_execution_prompt(&revised_input);

        let prefix = first_run
            .strip_suffix(&format!("{CLOSING_JSON_ONLY}\n"))
            .expect("first run ends with closing line");
        let revision_start = rework.find("## 本次修订").expect("revision section");
        assert_eq!(&rework[..revision_start], prefix);
    }

    #[test]
    fn task_prompt_never_contains_acceptance_criteria() {
        let mut input = sample_input();
        input.revision = Some(sample_revision(WorkflowRevisionFeedbackSource::User));
        let prompt = build_task_execution_prompt(&input);
        assert!(!prompt.contains("验收标准"));
        assert!(!prompt.contains("acceptance"));
    }

    #[test]
    fn empty_optional_sections_are_fully_omitted() {
        let mut input = sample_input();
        input.upstream_results = vec![];
        input.contract.outputs = vec![];
        input.contract.self_check = vec![];
        input.contract.verification_methods = vec![];
        input.contract.completion_evidence = vec![];
        let prompt = build_task_execution_prompt(&input);
        for absent in [
            "## 预期产物",
            "## 自检清单",
            "## 验证方式",
            "## 完成证据",
            "## 必要上游结果",
            "## 本次修订",
        ] {
            assert!(!prompt.contains(absent), "unexpected section: {absent}");
        }
        for present in [
            "# 执行任务",
            "## 工作总目标",
            "## 当前任务",
            "## 输出 JSON Schema",
        ] {
            assert!(prompt.contains(present), "missing section: {present}");
        }
    }

    #[test]
    fn prompt_has_no_data_boundary_or_protocol_examples() {
        let mut input = sample_input();
        input.revision = Some(sample_revision(WorkflowRevisionFeedbackSource::Lead));
        let prompt = build_task_execution_prompt(&input);
        assert!(!prompt.contains("openteams_untrusted_data"));
        assert!(!prompt.contains("Data Boundary"));
        assert!(!prompt.contains("Output Protocol"));
        assert_eq!(prompt.matches("```json").count(), 1);
    }

    #[test]
    fn schema_is_task_scoped_and_pins_identity() {
        let prompt = build_task_execution_prompt(&sample_input());
        assert!(prompt.contains("\"final_result\""));
        assert!(prompt.contains("backend_pi_types_runtime"));
        assert!(prompt.contains(EXECUTION_ID));
        assert!(!prompt.contains("review_result"));
        assert!(!prompt.contains("result_review_result"));
        assert!(!prompt.contains("loop_review_result"));
    }

    #[test]
    fn revision_section_maps_source_labels_and_content() {
        let cases = [
            (WorkflowRevisionFeedbackSource::Lead, "Lead 审核"),
            (WorkflowRevisionFeedbackSource::Reviewer, "Loop 审核"),
            (WorkflowRevisionFeedbackSource::User, "用户反馈"),
        ];
        for (source, label) in cases {
            let mut input = sample_input();
            input.revision = Some(sample_revision(source));
            let prompt = build_task_execution_prompt(&input);
            let schema_index = prompt.find("## 输出 JSON Schema").unwrap();
            let revision_index = prompt.find("## 本次修订").unwrap();
            assert!(revision_index > schema_index);
            let revision = &prompt[revision_index..];
            assert!(revision.contains(&format!("- 来源：{label}")));
            assert!(revision.contains("- 当前尝试：2"));
            assert!(
                revision.contains("- 最近反馈：在 models.json 写入边界增加字面量编码的往返测试。")
            );
            assert!(revision.contains("- 上次结果摘要：已完成 Pi 模型协调返工。"));
            assert!(revision.contains("- 上次产物：`crates/services/src/services/pi_models.rs`、`crates/services/src/services/cli_config.rs`"));
            assert!(revision.contains(
                "按照审核意见完整修复：逐条解决反馈指出的全部问题，不得只修复部分问题、不得用解释代替修复；未受反馈影响的既有正确成果保持不变。"
            ));
        }
    }

    #[test]
    fn response_language_line_follows_duty_sentence_when_non_empty() {
        let mut input = sample_input();
        input.response_language = "  人类可读内容使用简体中文。 ".to_string();
        let prompt = build_task_execution_prompt(&input);
        assert_ascending(
            &prompt,
            &["实际结果。", "人类可读内容使用简体中文。", "## 工作总目标"],
        );

        let plain = build_task_execution_prompt(&sample_input());
        assert!(plain.contains("实际结果。\n\n## 工作总目标"));
    }

    #[test]
    fn dynamic_content_is_not_truncated() {
        let mut input = sample_input();
        let long_instructions = "长".repeat(2000);
        input.instructions = long_instructions.clone();
        let prompt = build_task_execution_prompt(&input);
        assert!(prompt.contains(&long_instructions));
    }
}
