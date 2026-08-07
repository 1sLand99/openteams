//! Shared rendering helpers for typed prompt builders (design §7、§7.1).
//!
//! Owns the cache-friendly section ordering of §7.1 (builder fixed copy ->
//! run-level -> node-level -> schema -> attempt-level -> fixed closing line)
//! so individual builders cannot drift from it, plus neutral Markdown render
//! helpers shared by the builders. Scenario-specific copy lives in the
//! builders, not here.

use db::models::workflow_types::AcceptanceCriteria;
use uuid::Uuid;

/// Run identity injected into the output JSON Schema as `const` fields (§7 rule 7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptIdentity {
    pub execution_id: Uuid,
    pub step_key: String,
}

/// Latest valid result of a required upstream node (§6.1). Carries only the
/// hand-off content the current node genuinely needs; full transcripts,
/// superseded attempts and unrelated summaries must not enter this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamResultInput {
    pub step_key: String,
    pub summary: String,
    pub outputs: Vec<String>,
}

/// Fixed closing line for JSON-only prompts (§7.1 category 6), byte-stable.
pub const CLOSING_JSON_ONLY: &str = "只返回一个匹配 Schema 的 JSON 对象。";

/// Section titles shared byte-identically across builders.
pub const WORKFLOW_GOAL_SECTION_TITLE: &str = "## 工作总目标";
pub const CURRENT_TASK_SECTION_TITLE: &str = "## 当前任务";
pub const UPSTREAM_SECTION_TITLE: &str = "## 必要上游结果";
pub const SCHEMA_SECTION_TITLE: &str = "## 输出 JSON Schema";

/// Acceptance tier group headers (§8.4、§11.3).
pub const ACCEPTANCE_REQUIRED_HEADER: &str = "必须满足（required）——全部通过才能 approved：";
pub const ACCEPTANCE_PARTIAL_HEADER: &str =
    "允许外部归因（partial）——未通过时凭明确、可验证的外部归因放行并记入 risks：";
pub const ACCEPTANCE_RECOMMENDED_HEADER: &str = "建议满足（recommended）——不影响结论：";

/// §7.1 stability category of a prompt section. Declaration order is the
/// render order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SectionStability {
    /// Builder-level fixed copy; byte-stable, no runtime data.
    Fixed,
    /// Run-level stable content (workflow goal, plan summary, members).
    RunLevel,
    /// Node-level content (current task, contract/acceptance, upstreams).
    NodeLevel,
    /// The single output JSON Schema.
    Schema,
    /// Attempt-level content (revision, latest feedback, last output error).
    AttemptLevel,
}

/// Collects prompt sections and renders them in §7.1 stability order.
///
/// Sections pushed at the same stability level keep their insertion order;
/// blank sections are dropped entirely (§7 rule 4). `render` appends the
/// fixed closing line so the prompt tail is byte-stable across calls.
#[derive(Debug, Default)]
pub struct PromptSections {
    sections: Vec<(SectionStability, String)>,
}

impl PromptSections {
    pub fn new() -> Self {
        Self::default()
    }

    /// Category 1: builder-level fixed copy. Must not interpolate runtime data.
    pub fn push_fixed(&mut self, section: impl Into<String>) {
        self.push(SectionStability::Fixed, section);
    }

    /// Category 2: run-level stable content.
    pub fn push_run_level(&mut self, section: impl Into<String>) {
        self.push(SectionStability::RunLevel, section);
    }

    /// Category 3: node-level content.
    pub fn push_node_level(&mut self, section: impl Into<String>) {
        self.push(SectionStability::NodeLevel, section);
    }

    /// Category 4: the single output JSON Schema, rendered as one ```json
    /// code block under the shared schema section title (§7 rules 5-7).
    pub fn push_schema(&mut self, schema: &str) {
        self.push(
            SectionStability::Schema,
            format!("{SCHEMA_SECTION_TITLE}\n\n{}", render_schema_block(schema)),
        );
    }

    /// Category 5: attempt-level content; always rendered after the schema.
    pub fn push_attempt_level(&mut self, section: impl Into<String>) {
        self.push(SectionStability::AttemptLevel, section);
    }

    fn push(&mut self, stability: SectionStability, section: impl Into<String>) {
        let section = section.into();
        let trimmed = section.trim();
        if !trimmed.is_empty() {
            self.sections.push((stability, trimmed.to_string()));
        }
    }

    /// Renders sections in §7.1 stability order (insertion order preserved
    /// within a level) joined by blank lines, then the fixed closing line.
    pub fn render(mut self, closing_line: &str) -> String {
        self.sections.sort_by_key(|(stability, _)| *stability);
        let mut prompt = self
            .sections
            .iter()
            .map(|(_, section)| section.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        prompt.push_str("\n\n");
        prompt.push_str(closing_line.trim());
        prompt.push('\n');
        prompt
    }
}

/// Renders a plain bullet list, trimming items and skipping blank ones.
/// Returns `None` when nothing renderable remains.
pub fn render_bullet_list(items: &[String]) -> Option<String> {
    let bullets = items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>();
    if bullets.is_empty() {
        None
    } else {
        Some(bullets.join("\n"))
    }
}

/// Renders a "title + bullet list" section. Returns `None` (section omitted
/// entirely, §7 rule 4) when there is no renderable item.
pub fn render_bullet_section(title: &str, items: &[String]) -> Option<String> {
    render_bullet_list(items).map(|list| format!("{title}\n\n{list}"))
}

/// Renders a "title + numbered list" section. Returns `None` when there is no
/// renderable item.
pub fn render_numbered_section(title: &str, items: &[String]) -> Option<String> {
    let entries = items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .enumerate()
        .map(|(index, item)| format!("{}. {item}", index + 1))
        .collect::<Vec<_>>();
    if entries.is_empty() {
        None
    } else {
        Some(format!("{title}\n\n{}", entries.join("\n")))
    }
}

/// Renders the single ```json schema code block (§7 rule 5: the schema is the
/// only field truth source; no duplicated JSON examples).
pub fn render_schema_block(schema: &str) -> String {
    format!("```json\n{}\n```", schema.trim())
}

/// Protocol-retry helper (design §12.2): reuses the builder-rendered prompt
/// verbatim and inserts a short error summary as an attempt-level section
/// right before the fixed closing line. The full invalid model output is
/// never appended.
pub fn append_protocol_error_section(base_prompt: &str, error: &str) -> String {
    let section = format!(
        "## 上次输出错误\n\n上次返回未通过当前 Schema 校验：{}。重新完成同一任务，并只返回一个匹配上方 Schema 的 JSON 对象。",
        error.trim()
    );
    let trimmed = base_prompt.trim_end();
    if let Some(index) = trimmed.rfind(CLOSING_JSON_ONLY) {
        let prefix = trimmed[..index].trim_end();
        format!("{prefix}\n\n{section}\n\n{CLOSING_JSON_ONLY}\n")
    } else {
        format!("{trimmed}\n\n{section}\n")
    }
}

/// Renders items as an inline code list joined by "、" (e.g. `a`、`b`).
/// Returns `None` when there is no renderable item.
pub fn render_inline_code_list(items: &[String]) -> Option<String> {
    let rendered = items
        .iter()
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join("、"))
    }
}

/// Renders tiered acceptance criteria as the three Chinese groups
/// 「必须满足（required）/允许外部归因（partial）/建议满足（recommended）」;
/// empty groups are omitted. Returns `None` when every tier is empty so the
/// caller omits the whole acceptance section.
pub fn render_acceptance_tiers(acceptance: &AcceptanceCriteria) -> Option<String> {
    let groups = [
        (ACCEPTANCE_REQUIRED_HEADER, &acceptance.required),
        (ACCEPTANCE_PARTIAL_HEADER, &acceptance.partial),
        (ACCEPTANCE_RECOMMENDED_HEADER, &acceptance.recommended),
    ];
    let rendered = groups
        .iter()
        .filter_map(|(header, items)| {
            render_bullet_list(items).map(|list| format!("{header}\n\n{list}"))
        })
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join("\n\n"))
    }
}

/// Renders the body of the upstream-results section (without the section
/// title): one `###` sub-section per upstream step, the 产物 line omitted when
/// the step reported no outputs. Returns `None` when there is nothing to
/// render so the caller omits the whole section.
pub fn render_upstream_results(upstream_results: &[UpstreamResultInput]) -> Option<String> {
    let rendered = upstream_results
        .iter()
        .filter_map(|result| {
            let step_key = result.step_key.trim();
            let summary = result.summary.trim();
            let outputs = render_inline_code_list(&result.outputs);
            if step_key.is_empty() || (summary.is_empty() && outputs.is_none()) {
                return None;
            }
            let mut lines = Vec::new();
            if !summary.is_empty() {
                lines.push(format!("- 摘要：{summary}"));
            }
            if let Some(outputs) = outputs {
                lines.push(format!("- 产物：{outputs}"));
            }
            Some(format!("### `{step_key}`\n\n{}", lines.join("\n")))
        })
        .collect::<Vec<_>>();
    if rendered.is_empty() {
        None
    } else {
        Some(rendered.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acceptance(required: &[&str], partial: &[&str], recommended: &[&str]) -> AcceptanceCriteria {
        AcceptanceCriteria {
            required: required.iter().map(|item| item.to_string()).collect(),
            partial: partial.iter().map(|item| item.to_string()).collect(),
            recommended: recommended.iter().map(|item| item.to_string()).collect(),
        }
    }

    fn items(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
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
    fn bullet_section_is_omitted_when_empty() {
        assert!(render_bullet_section("## 标题", &[]).is_none());
        assert!(render_bullet_section("## 标题", &items(&["  "])).is_none());
    }

    #[test]
    fn bullet_section_trims_and_lists_items() {
        let section = render_bullet_section("## 标题", &items(&[" 甲 ", "乙"])).unwrap();
        assert_eq!(section, "## 标题\n\n- 甲\n- 乙");
    }

    #[test]
    fn numbered_section_numbers_items_from_one() {
        let section = render_numbered_section("## 审核标准", &items(&["甲", " 乙 "])).unwrap();
        assert_eq!(section, "## 审核标准\n\n1. 甲\n2. 乙");
        assert!(render_numbered_section("## 审核标准", &[]).is_none());
    }

    #[test]
    fn acceptance_tiers_render_in_order_and_skip_empty_groups() {
        let rendered = render_acceptance_tiers(&acceptance(&["r1", "r2"], &[], &["m1"])).unwrap();
        assert_eq!(
            rendered,
            format!(
                "{ACCEPTANCE_REQUIRED_HEADER}\n\n- r1\n- r2\n\n{ACCEPTANCE_RECOMMENDED_HEADER}\n\n- m1"
            )
        );
        assert!(!rendered.contains("partial"));
    }

    #[test]
    fn acceptance_tiers_is_none_when_all_empty() {
        assert!(render_acceptance_tiers(&AcceptanceCriteria::default()).is_none());
    }

    #[test]
    fn sections_render_in_stability_order_regardless_of_push_order() {
        let mut sections = PromptSections::new();
        sections.push_attempt_level("尝试级");
        sections.push_schema("{ \"type\": \"object\" }");
        sections.push_fixed("固定文案");
        sections.push_node_level("节点级");
        sections.push_run_level("运行级");
        let prompt = sections.render(CLOSING_JSON_ONLY);
        assert_ascending(
            &prompt,
            &[
                "固定文案",
                "运行级",
                "节点级",
                SCHEMA_SECTION_TITLE,
                "```json",
                "尝试级",
                CLOSING_JSON_ONLY,
            ],
        );
    }

    #[test]
    fn sections_preserve_insertion_order_within_same_level() {
        let mut sections = PromptSections::new();
        sections.push_node_level("节点甲");
        sections.push_node_level("节点乙");
        sections.push_run_level("运行甲");
        sections.push_run_level("运行乙");
        let prompt = sections.render(CLOSING_JSON_ONLY);
        assert!(prompt.find("运行甲").unwrap() < prompt.find("运行乙").unwrap());
        assert!(prompt.find("节点甲").unwrap() < prompt.find("节点乙").unwrap());
        assert!(prompt.find("运行乙").unwrap() < prompt.find("节点甲").unwrap());
    }

    #[test]
    fn blank_sections_are_dropped() {
        let mut sections = PromptSections::new();
        sections.push_fixed("   ");
        sections.push_node_level("节点");
        let prompt = sections.render(CLOSING_JSON_ONLY);
        assert_eq!(prompt, format!("节点\n\n{CLOSING_JSON_ONLY}\n"));
    }

    #[test]
    fn render_is_byte_stable_and_ends_with_closing_line() {
        let build = || {
            let mut sections = PromptSections::new();
            sections.push_fixed("固定");
            sections.push_schema("{\"type\":\"object\"}");
            sections.render(CLOSING_JSON_ONLY)
        };
        assert_eq!(build(), build());
        assert!(build().ends_with(&format!("{CLOSING_JSON_ONLY}\n")));
        assert_eq!(build().matches("```json").count(), 1);
    }

    #[test]
    fn inline_code_list_joins_with_pause_mark() {
        assert_eq!(
            render_inline_code_list(&items(&["a.rs", " b.rs "])),
            Some("`a.rs`、`b.rs`".to_string())
        );
        assert!(render_inline_code_list(&[]).is_none());
    }

    #[test]
    fn upstream_results_render_per_step_and_skip_empty_parts() {
        let rendered = render_upstream_results(&[
            UpstreamResultInput {
                step_key: "step_a".to_string(),
                summary: "完成甲".to_string(),
                outputs: items(&["a.rs"]),
            },
            UpstreamResultInput {
                step_key: "step_b".to_string(),
                summary: "完成乙".to_string(),
                outputs: vec![],
            },
        ])
        .unwrap();
        assert_eq!(
            rendered,
            "### `step_a`\n\n- 摘要：完成甲\n- 产物：`a.rs`\n\n### `step_b`\n\n- 摘要：完成乙"
        );
        assert!(render_upstream_results(&[]).is_none());
    }
}
