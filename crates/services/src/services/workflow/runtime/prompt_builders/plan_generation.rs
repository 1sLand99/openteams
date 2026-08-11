//! Plan generation prompt builder (design §6.2, §8, §11.1).
//!
//! Single source of truth for the initial / regeneration / iteration plan
//! prompts: mode-specific fixed copy, graph-semantic compile rules, node field
//! descriptions, two-phase output guidance, and the enhanced plan JSON Schema.
//! Sections follow the cache-friendly layout of §7.1: builder-level fixed copy
//! first, run-level stable content next, the output Schema, then attempt-level
//! sections, and finally the byte-stable closing line.
//!
//! This module is intentionally self-contained: it does not depend on
//! `prompt_builders::common` so parallel builder tasks cannot conflict.

use db::models::workflow_types::WorkflowPlanJson;

use crate::services::output_validation::{
    OutputValidationKind, OutputValidationReturnMode, WorkflowPlanValidationContext,
    render_output_validation_instructions,
};

/// How plan generation was triggered (design §6.2, §8.1).
#[derive(Debug, Clone)]
pub enum PlanGenerationMode {
    /// Initial generation, triggered by the lead model's first-round
    /// `workflow_generate` protocol output.
    Initial,
    /// The user retries from a failed plan card; `failure_reason` comes from
    /// the card `error_message` and `previous_plan` is attached when available.
    Regeneration {
        failure_reason: String,
        previous_plan: Option<WorkflowPlanJson>,
    },
    /// The user rejected a round of results; feedback arrives flattened as
    /// text. There is no structured "adjustment request" field.
    Iteration {
        previous_plan: WorkflowPlanJson,
        current_state_summary: String,
        latest_user_feedback: String,
    },
}

/// A planning member. Only information that affects task assignment may enter
/// the prompt (design §6.2).
#[derive(Debug, Clone)]
pub struct PlanningMemberInput {
    pub agent_id: String,
    pub name: String,
    pub role: String,
    pub responsibilities: String,
    pub skills: Vec<String>,
    pub tools: Vec<String>,
}

/// Typed input for [`build_plan_generation_prompt`] (design §6.2).
#[derive(Debug, Clone)]
pub struct PlanGenerationPromptInput {
    pub summary: String,
    pub design_doc_paths: Vec<String>,
    pub lead_agent_id: String,
    pub members: Vec<PlanningMemberInput>,
    pub response_language: String,
    pub mode: PlanGenerationMode,
}

// ---------------------------------------------------------------------------
// Mode-specific fixed copy (§11.1.1 / §11.1.2 / §11.1.3, byte-stable)
// ---------------------------------------------------------------------------

const INITIAL_HEADING: &str = r#"# 生成工作流计划

根据已确认的实现设计生成一个可执行工作流计划。只规划实现与验证步骤，不重新讨论需求。"#;

const INITIAL_TWO_PHASE_GUIDANCE: &str = r#"第一阶段：先用 Markdown 逐步写出计划草案——依次说明每个节点的目标、执行要点、合同要点和节点间依赖，只使用文字和列表，不写任何 JSON。
第二阶段：在草案之后输出最终的完整计划 JSON。
草案中不得包含任何完整 JSON 对象或 JSON 代码块；解析器只提取输出中最后一个完整 JSON 对象。"#;

const INITIAL_CLOSING_LINE: &str =
    "先以 Markdown 输出计划草案，再在末尾输出一个匹配 Schema 的完整 JSON 对象。";

const REGENERATION_HEADING: &str = r#"# 重新生成工作流计划

上一版计划未通过运行时校验。只修复错误明确指出的问题，保留未受影响的节点 ID、任务合同和依赖关系，并返回完整计划。"#;

const REGENERATION_TWO_PHASE_GUIDANCE: &str = r#"第一阶段：先用 Markdown 简要说明本次修复了哪些校验问题以及修复方式，只使用文字和列表，不写任何 JSON。
第二阶段：在说明之后输出修复后的完整计划 JSON。
说明部分不得包含任何完整 JSON 对象或 JSON 代码块；解析器只提取输出中最后一个完整 JSON 对象。"#;

const REGENERATION_CLOSING_LINE: &str =
    "先以 Markdown 简要说明修复内容，再在末尾输出一个匹配 Schema 的完整 JSON 对象。";

const ITERATION_HEADING: &str = r#"# 根据用户反馈重新生成工作流计划

上一版计划结构合法，但用户要求调整后续执行方式。保留用户未要求变更的节点 ID、任务合同和已完成工作，返回一份完整的新计划。"#;

const ITERATION_TWO_PHASE_GUIDANCE: &str = r#"第一阶段：先用 Markdown 简要说明本次按用户反馈调整了哪些节点和依赖、保留了哪些已完成工作，只使用文字和列表，不写任何 JSON。
第二阶段：在说明之后输出完整的新计划 JSON。
说明部分不得包含任何完整 JSON 对象或 JSON 代码块；解析器只提取输出中最后一个完整 JSON 对象。"#;

const ITERATION_CLOSING_LINE: &str =
    "先以 Markdown 简要说明调整内容，再在末尾输出一个匹配 Schema 的完整 JSON 对象。";

// ---------------------------------------------------------------------------
// Mode-independent fixed copy (byte-stable across all modes)
// ---------------------------------------------------------------------------

/// Graph-semantic compile rules that the Schema cannot express (§8.2, rendered
/// per §11.1.1). Structural constraints are expressed by the Schema only and
/// must not be restated here.
const COMPILE_RULES_SECTION: &str = r#"## 编译规则

结构约束（字段类型、枚举、取值范围、ID 格式、必填与按 stepType 的条件必填）全部由下方 Schema 表达，以 Schema 为准。以下图语义规则无法由 Schema 表达，违反任一条都会导致编译失败：

1. 节点 ID、边 ID 全图唯一。
2. 边的 `source`、`target` 必须引用存在的节点；图必须无环。
3. 必须且只能有一个 `stepType: "result"` 节点，且该节点没有任何出边。
4. 节点 `data.agentId` 必须出现在 `agents.available` 中。
5. `reviewScope` 条目不重复，必须引用存在的 task 节点；每个条目必须是该 review 节点的上游（存在有向路径）；从 scope 内节点到 review 节点的路径不得穿过另一个 review 节点；路径上的中间 task 节点必须一并包含在 scope 内；每个 task 最多归属一个 Loop。"#;

/// Per-field semantics for plan nodes (§8.3, rendered per §11.1.1): every task
/// field explained, review/result nodes kept brief, with the three-tier
/// acceptance semantics and the role of `selfCheck` inlined.
const NODE_FIELDS_SECTION: &str = r#"## 节点字段说明

task 节点：
- `stepType`：固定 `"task"`。
- `agentId`：负责执行的成员 ID。
- `title`：一句话任务名称。
- `instructions`：执行者看到的完整任务说明。执行者看不到验收标准，因此 instructions 包含：范围、步骤、约束和需要产出的内容都要写清楚。
- `acceptance`：分级验收标准，供审核者使用，不下发给执行者：
  - `required`：必须全部满足才能通过。只写能通过外部手段得到确定结果的客观项（测试、编译、lint、命令输出、文件检查），不写主观判断。
  - `partial`：允许未满足。因环境、凭据、外部服务等不可抗力未满足时，审核可凭明确的外部归因放行并记录风险。
  - `recommended`：建议满足。满足更好，未满足不影响通过。
- `outputs`：预期产物路径或交付物清单。
- `selfCheck`：执行者完成前的自检清单。主观性、质量性要求写在这里。
- `verificationCommands`：客观验证命令或方法。
- `completionEvidence`：完成时必须提供的证据。
- `interruptible`、`maxRetry`：运行控制参数。

review 节点：
- `reviewScope`：需要整体审核的上游 task 列表；提供后构成 Loop 审核，省略则为独立审核节点。
- `acceptance`：带 `reviewScope` 时必填，是 Loop 整体验收标准，同样按三级分级且必须为客观可验证项。
- `instructions`：审核重点和范围说明。

result 节点：
- `title`、`instructions`：结果汇总的标题与说明。"#;

/// Planning principles; soft guidance, not validity constraints (§8.7).
const PLANNING_PRINCIPLES_SECTION: &str = r#"## 规划原则

- 生成满足目标的最小可执行闭环。
- 按成员职责分配节点。
- 不生成没有运行时消费者的字段。"#;

/// Build the complete plan generation prompt for the given typed input.
///
/// Section order (§7.1 cache-friendly layout):
///
/// 1. Builder-level fixed copy: heading, two-phase output guidance, compile
///    rules, node field descriptions, planning principles.
/// 2. Run-level stable content: language requirement, plan summary, design
///    documents, members and lead.
/// 3. The output JSON Schema (the single source of truth for structure).
/// 4. Attempt-level sections (after the Schema): last failure / current
///    state / latest user feedback / previous plan, depending on the mode.
/// 5. The byte-stable closing line.
pub fn build_plan_generation_prompt(input: &PlanGenerationPromptInput) -> String {
    let (heading, two_phase_guidance, closing_line) = mode_fixed_text(&input.mode);

    let mut sections: Vec<String> = vec![
        heading.to_string(),
        format!("## 输出方式（两阶段）\n\n{two_phase_guidance}"),
        COMPILE_RULES_SECTION.to_string(),
        NODE_FIELDS_SECTION.to_string(),
        PLANNING_PRINCIPLES_SECTION.to_string(),
    ];

    let response_language = input.response_language.trim();
    if !response_language.is_empty() {
        sections.push(format!("## 语言要求\n\n{response_language}"));
    }

    sections.push(format!("## 计划概要\n\n{}", input.summary.trim()));

    if !input.design_doc_paths.is_empty() {
        let doc_list = input
            .design_doc_paths
            .iter()
            .map(|path| format!("- `{}`", path.trim()))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "## 详细设计文档\n\n{doc_list}\n\n生成计划前必须阅读上述设计文档。"
        ));
    }

    sections.push(render_members_section(input));
    sections.push(render_schema_section(input));
    sections.push(render_output_validation_instructions(
        OutputValidationKind::WorkflowPlan,
        &WorkflowPlanValidationContext {
            lead_agent_id: input.lead_agent_id.clone(),
            available_agent_ids: input
                .members
                .iter()
                .map(|member| member.agent_id.trim().to_string())
                .collect(),
        },
        OutputValidationReturnMode::PlanTwoPhase,
    ));

    // Attempt-level content always lives after the Schema (§7.1).
    match &input.mode {
        PlanGenerationMode::Initial => {}
        PlanGenerationMode::Regeneration {
            failure_reason,
            previous_plan,
        } => {
            sections.push(format!("## 上次生成错误\n\n{}", failure_reason.trim()));
            if let Some(plan) = previous_plan {
                sections.push(render_previous_plan_section(plan));
            }
        }
        PlanGenerationMode::Iteration {
            previous_plan,
            current_state_summary,
            latest_user_feedback,
        } => {
            sections.push(format!("## 当前状态\n\n{}", current_state_summary.trim()));
            sections.push(format!(
                "## 最新用户反馈\n\n{}",
                latest_user_feedback.trim()
            ));
            sections.push(render_previous_plan_section(previous_plan));
        }
    }

    sections.push(closing_line.to_string());

    let mut prompt = sections.join("\n\n");
    prompt.push('\n');
    prompt
}

/// Heading block, two-phase output guidance body, and closing line for the
/// given mode (§11.1.1 / §11.1.2 / §11.1.3).
fn mode_fixed_text(mode: &PlanGenerationMode) -> (&'static str, &'static str, &'static str) {
    match mode {
        PlanGenerationMode::Initial => (
            INITIAL_HEADING,
            INITIAL_TWO_PHASE_GUIDANCE,
            INITIAL_CLOSING_LINE,
        ),
        PlanGenerationMode::Regeneration { .. } => (
            REGENERATION_HEADING,
            REGENERATION_TWO_PHASE_GUIDANCE,
            REGENERATION_CLOSING_LINE,
        ),
        PlanGenerationMode::Iteration { .. } => (
            ITERATION_HEADING,
            ITERATION_TWO_PHASE_GUIDANCE,
            ITERATION_CLOSING_LINE,
        ),
    }
}

fn render_members_section(input: &PlanGenerationPromptInput) -> String {
    let member_lines = input
        .members
        .iter()
        .map(|member| {
            let mut details = vec![
                member.name.trim().to_string(),
                member.role.trim().to_string(),
                member.responsibilities.trim().to_string(),
            ];
            if !member.skills.is_empty() {
                details.push(format!("技能：{}", member.skills.join("、")));
            }
            if !member.tools.is_empty() {
                details.push(format!("工具：{}", member.tools.join("、")));
            }
            format!("- `{}`：{}。", member.agent_id.trim(), details.join("；"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "## 参与成员与职责\n\n{member_lines}\n\nLead agent id：`{}`",
        input.lead_agent_id.trim()
    )
}

fn render_schema_section(input: &PlanGenerationPromptInput) -> String {
    let member_ids = input
        .members
        .iter()
        .map(|member| member.agent_id.trim().to_string())
        .collect::<Vec<_>>();
    let schema = plan_output_schema(input.lead_agent_id.trim(), &member_ids);
    let schema_json = serde_json::to_string_pretty(&schema).unwrap_or_else(|_| "{}".to_string());
    format!("## 输出 JSON Schema\n\n```json\n{schema_json}\n```")
}

fn render_previous_plan_section(plan: &WorkflowPlanJson) -> String {
    let plan_json = serde_json::to_string_pretty(plan).unwrap_or_else(|_| "{}".to_string());
    format!("## 上一版计划\n\n```json\n{plan_json}\n```")
}

/// The enhanced plan output Schema (§11.1.1), generated programmatically so
/// the lead `const` and the member `enum` lists always match the typed input.
/// Structural constraints (node id `pattern`, non-empty `agents.available`,
/// per-`stepType` conditional requirements, tiered acceptance) live here and
/// nowhere else.
pub(crate) fn plan_output_schema(lead_agent_id: &str, member_ids: &[String]) -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["version", "title", "goal", "agents", "nodes", "edges"],
        "additionalProperties": false,
        "properties": {
            "version": { "const": "1" },
            "title": { "type": "string", "minLength": 1 },
            "goal": { "type": "string", "minLength": 1 },
            "agents": {
                "type": "object",
                "required": ["lead", "available"],
                "additionalProperties": false,
                "properties": {
                    "lead": { "const": lead_agent_id },
                    "available": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "enum": member_ids },
                        "uniqueItems": true
                    }
                }
            },
            "globals": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "interrupt_mode": { "const": "cooperative" },
                    "default_retry": { "type": "integer", "minimum": 0, "maximum": 10 },
                    "global_pause_supported": { "type": "boolean" }
                }
            },
            "nodes": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["id", "type", "data"],
                    "additionalProperties": false,
                    "properties": {
                        "id": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 128,
                            "pattern": "^[A-Za-z0-9][A-Za-z0-9._-]*$"
                        },
                        "type": { "const": "workflowStep" },
                        "data": {
                            "type": "object",
                            "required": ["stepType", "title", "instructions"],
                            "additionalProperties": false,
                            "properties": {
                                "stepType": { "enum": ["task", "review", "result"] },
                                "agentId": { "enum": member_ids },
                                "title": { "type": "string", "minLength": 1 },
                                "instructions": { "type": "string", "minLength": 1 },
                                "acceptance": {
                                    "type": "object",
                                    "required": ["required"],
                                    "additionalProperties": false,
                                    "properties": {
                                        "required": {
                                            "type": "array",
                                            "minItems": 1,
                                            "items": { "type": "string", "minLength": 1 }
                                        },
                                        "partial": {
                                            "type": "array",
                                            "items": { "type": "string", "minLength": 1 }
                                        },
                                        "recommended": {
                                            "type": "array",
                                            "items": { "type": "string", "minLength": 1 }
                                        }
                                    }
                                },
                                "outputs": {
                                    "type": "array",
                                    "items": { "type": "string", "minLength": 1 }
                                },
                                "selfCheck": {
                                    "type": "array",
                                    "items": { "type": "string", "minLength": 1 }
                                },
                                "verificationCommands": {
                                    "type": "array",
                                    "items": { "type": "string", "minLength": 1 }
                                },
                                "completionEvidence": {
                                    "type": "array",
                                    "items": { "type": "string", "minLength": 1 }
                                },
                                "interruptible": { "type": "boolean" },
                                "maxRetry": { "type": "integer", "minimum": 0, "maximum": 10 },
                                "reviewScope": {
                                    "type": "array",
                                    "items": { "type": "string", "minLength": 1 },
                                    "uniqueItems": true
                                }
                            },
                            "allOf": [
                                {
                                    "if": { "properties": { "stepType": { "const": "task" } } },
                                    "then": {
                                        "required": [
                                            "acceptance",
                                            "outputs",
                                            "selfCheck",
                                            "verificationCommands",
                                            "completionEvidence"
                                        ],
                                        "properties": {
                                            "outputs": { "minItems": 1 },
                                            "selfCheck": { "minItems": 1 },
                                            "verificationCommands": { "minItems": 1 },
                                            "completionEvidence": { "minItems": 1 }
                                        }
                                    }
                                },
                                {
                                    "if": { "required": ["reviewScope"] },
                                    "then": { "required": ["acceptance"] }
                                },
                                {
                                    "if": {
                                        "properties": { "stepType": { "enum": ["task", "result"] } }
                                    },
                                    "then": { "not": { "required": ["reviewScope"] } }
                                }
                            ]
                        }
                    }
                }
            },
            "edges": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["id", "source", "target"],
                    "additionalProperties": false,
                    "properties": {
                        "id": { "type": "string", "minLength": 1 },
                        "source": { "type": "string", "minLength": 1 },
                        "target": { "type": "string", "minLength": 1 },
                        "type": { "type": "string" },
                        "data": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": { "kind": { "const": "hard" } }
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use db::models::workflow_types::{
        AcceptanceCriteria, WorkflowNodeData, WorkflowNodePosition, WorkflowPlanAgents,
        WorkflowPlanEdge, WorkflowPlanNode,
    };

    use super::*;

    const LEAD_ID: &str = "fb7a0834-c2a4-4658-a763-80127e9c3eec";
    const BACKEND_ID: &str = "b6e31e00-c61f-4dfd-a09c-4f4d993360ba";
    const DESIGN_DOC: &str =
        ".openteams/specs/2026-08-03-pi-coding-agent-acp-integration-design.md";
    const LANGUAGE_INSTRUCTION: &str =
        "You MUST write human-readable JSON string values in Simplified Chinese.";
    const FAILURE_REASON: &str = "`WorkflowCompiler` 校验失败：review 节点 `lead_backend_review` 的 `reviewScope` 遗漏了必要中间 task。";
    const STATE_SUMMARY: &str = "`backend_pi_types_runtime` 已完成。前端与离线回归节点尚未开始。";
    const USER_FEEDBACK: &str =
        "- what_wrong: 前端与离线回归串行执行太慢。\n- expected: 两者并行执行。\n- priority: 高";

    fn test_members() -> Vec<PlanningMemberInput> {
        vec![
            PlanningMemberInput {
                agent_id: LEAD_ID.to_string(),
                name: "Lead".to_string(),
                role: "技术负责人".to_string(),
                responsibilities: "负责后端审核和最终结果把关".to_string(),
                skills: vec!["code-review".to_string()],
                tools: vec!["bash".to_string()],
            },
            PlanningMemberInput {
                agent_id: BACKEND_ID.to_string(),
                name: "Backend".to_string(),
                role: "后端工程师".to_string(),
                responsibilities: "负责 Pi 强类型、供应商同步和 ACP 生命周期".to_string(),
                skills: vec!["rust".to_string()],
                tools: vec!["bash".to_string()],
            },
        ]
    }

    fn input_with_mode(mode: PlanGenerationMode) -> PlanGenerationPromptInput {
        PlanGenerationPromptInput {
            summary: "将 Pi 注册为完整一等 Agent，实现供应商配置同步与成员能力隔离。".to_string(),
            design_doc_paths: vec![DESIGN_DOC.to_string()],
            lead_agent_id: LEAD_ID.to_string(),
            members: test_members(),
            response_language: LANGUAGE_INSTRUCTION.to_string(),
            mode,
        }
    }

    fn regeneration_input() -> PlanGenerationPromptInput {
        input_with_mode(PlanGenerationMode::Regeneration {
            failure_reason: FAILURE_REASON.to_string(),
            previous_plan: Some(sample_plan()),
        })
    }

    fn iteration_input() -> PlanGenerationPromptInput {
        input_with_mode(PlanGenerationMode::Iteration {
            previous_plan: sample_plan(),
            current_state_summary: STATE_SUMMARY.to_string(),
            latest_user_feedback: USER_FEEDBACK.to_string(),
        })
    }

    fn sample_plan() -> WorkflowPlanJson {
        WorkflowPlanJson {
            version: "1".to_string(),
            title: "Pi 后端一等 Agent 集成".to_string(),
            goal: "完成 Pi 强类型与供应商配置同步".to_string(),
            agents: WorkflowPlanAgents {
                lead: LEAD_ID.to_string(),
                available: vec![LEAD_ID.to_string(), BACKEND_ID.to_string()],
            },
            globals: None,
            viewport: None,
            nodes: vec![
                WorkflowPlanNode {
                    id: "backend_pi_types_runtime".to_string(),
                    node_type: "workflowStep".to_string(),
                    position: WorkflowNodePosition::default(),
                    data: WorkflowNodeData {
                        step_type: "task".to_string(),
                        agent_id: Some(BACKEND_ID.to_string()),
                        title: "建立 Pi 强类型与运行时 API".to_string(),
                        instructions: "注册 Pi 强类型和默认 profile。".to_string(),
                        acceptance: Some(AcceptanceCriteria {
                            required: vec![
                                "cargo test -p executors --features qa-mode pi 通过".to_string(),
                            ],
                            ..Default::default()
                        }),
                        outputs: Some(vec!["crates/executors/src/executors/pi.rs".to_string()]),
                        self_check: Some(vec!["Pi 出现在强类型枚举与默认 profile 中".to_string()]),
                        verification_commands: Some(vec![
                            "cargo test -p executors --features qa-mode pi".to_string(),
                        ]),
                        completion_evidence: Some(vec!["提供聚焦测试摘要".to_string()]),
                        interruptible: true,
                        max_retry: None,
                        status: None,
                        loop_key: None,
                        review_scope: None,
                    },
                },
                WorkflowPlanNode {
                    id: "pi_integration_result".to_string(),
                    node_type: "workflowStep".to_string(),
                    position: WorkflowNodePosition::default(),
                    data: WorkflowNodeData {
                        step_type: "result".to_string(),
                        agent_id: Some(LEAD_ID.to_string()),
                        title: "汇总 Pi 后端集成结果".to_string(),
                        instructions: "汇总最新有效结果、交付物与风险。".to_string(),
                        acceptance: None,
                        outputs: None,
                        self_check: None,
                        verification_commands: None,
                        completion_evidence: None,
                        interruptible: true,
                        max_retry: None,
                        status: None,
                        loop_key: None,
                        review_scope: None,
                    },
                },
            ],
            edges: vec![WorkflowPlanEdge {
                id: "e1".to_string(),
                source: "backend_pi_types_runtime".to_string(),
                target: "pi_integration_result".to_string(),
                edge_type: None,
                data: None,
            }],
            loops: None,
            policies: None,
        }
    }

    /// Extract the `## heading` section (heading included) up to the next
    /// `## ` section or the end of the prompt, without trailing whitespace.
    /// The heading argument may be passed with or without the `## ` prefix.
    fn section(prompt: &str, heading: &str) -> String {
        let marker = section_marker(heading);
        let start = prompt
            .find(&marker)
            .unwrap_or_else(|| panic!("section `{marker}` missing"));
        let rest = &prompt[start + marker.len()..];
        let end = rest
            .find("\n## ")
            .map(|offset| start + marker.len() + offset)
            .unwrap_or(prompt.len());
        prompt[start..end].trim_end().to_string()
    }

    fn section_marker(heading: &str) -> String {
        if let Some(stripped) = heading.strip_prefix("## ") {
            format!("## {stripped}")
        } else {
            format!("## {heading}")
        }
    }

    fn section_position(prompt: &str, heading: &str) -> usize {
        let marker = section_marker(heading);
        prompt
            .find(&marker)
            .unwrap_or_else(|| panic!("section `{marker}` missing"))
    }

    /// Parse the JSON block of the `## 输出 JSON Schema` section.
    fn extract_schema_json(prompt: &str) -> serde_json::Value {
        let schema_section = section(prompt, "输出 JSON Schema");
        let fence = "```json\n";
        let json_start = schema_section
            .find(fence)
            .expect("schema code fence missing")
            + fence.len();
        let json_end = schema_section[json_start..]
            .find("\n```")
            .map(|offset| json_start + offset)
            .expect("schema code fence not closed");
        serde_json::from_str(&schema_section[json_start..json_end])
            .expect("output schema must be valid JSON")
    }

    #[test]
    fn initial_mode_renders_full_layout_without_attempt_sections() {
        let prompt = build_plan_generation_prompt(&input_with_mode(PlanGenerationMode::Initial));

        assert!(prompt.starts_with(INITIAL_HEADING));
        for heading in [
            "## 上次生成错误",
            "## 当前状态",
            "## 最新用户反馈",
            "## 上一版计划",
        ] {
            assert!(
                !prompt.contains(heading),
                "initial prompt must not contain attempt-level section {heading}"
            );
        }
        assert!(prompt.ends_with(&format!("{INITIAL_CLOSING_LINE}\n")));

        // §7.1 section order: fixed copy, run-level content, Schema, closing.
        let ordered = [
            "## 输出方式（两阶段）",
            "## 编译规则",
            "## 节点字段说明",
            "## 规划原则",
            "## 语言要求",
            "## 计划概要",
            "## 详细设计文档",
            "## 参与成员与职责",
            "## 输出 JSON Schema",
        ];
        let positions = ordered
            .iter()
            .map(|heading| section_position(&prompt, heading))
            .collect::<Vec<_>>();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "sections out of §7.1 order: {positions:?}"
        );
        let closing_position = prompt
            .rfind(INITIAL_CLOSING_LINE)
            .expect("closing line missing");
        assert!(closing_position > section_position(&prompt, "## 输出 JSON Schema"));

        // The full Schema appears exactly once.
        assert_eq!(prompt.matches("```json").count(), 1);
        assert!(prompt.contains("## Mandatory output validation"));
        assert!(prompt.contains("POST $OPENTEAMS_OUTPUT_VALIDATION_URL"));
        assert!(prompt.contains("\"kind\": \"workflow_plan\""));
        assert!(
            prompt.contains("If any response has `valid: true`, finish the two-phase response")
        );
        assert!(prompt.contains(
            "After the third retry, if validation still has not returned `valid: true`, stop validating, finish the two-phase response"
        ));
    }

    #[test]
    fn regeneration_mode_adds_failure_sections_after_schema() {
        let prompt = build_plan_generation_prompt(&regeneration_input());

        assert!(prompt.starts_with(REGENERATION_HEADING));
        assert!(prompt.contains(FAILURE_REASON));
        assert!(!prompt.contains("## 当前状态"));
        assert!(!prompt.contains("## 最新用户反馈"));
        assert!(!prompt.contains("调整要求"));

        let schema_position = section_position(&prompt, "## 输出 JSON Schema");
        let failure_position = section_position(&prompt, "## 上次生成错误");
        let previous_plan_position = section_position(&prompt, "## 上一版计划");
        assert!(failure_position > schema_position);
        assert!(previous_plan_position > failure_position);

        // The previous plan is embedded as pretty JSON.
        let plan_json = serde_json::to_string_pretty(&sample_plan()).expect("plan serializes");
        assert!(prompt.contains(&plan_json));

        assert!(prompt.ends_with(&format!("{REGENERATION_CLOSING_LINE}\n")));

        // Without a previous plan, that section is omitted entirely.
        let input_without_plan = input_with_mode(PlanGenerationMode::Regeneration {
            failure_reason: FAILURE_REASON.to_string(),
            previous_plan: None,
        });
        let prompt_without_plan = build_plan_generation_prompt(&input_without_plan);
        assert!(prompt_without_plan.contains("## 上次生成错误"));
        assert!(!prompt_without_plan.contains("## 上一版计划"));
    }

    #[test]
    fn iteration_mode_adds_state_feedback_and_previous_plan() {
        let prompt = build_plan_generation_prompt(&iteration_input());

        assert!(prompt.starts_with(ITERATION_HEADING));
        // The keep-completed-nodes rule must appear explicitly.
        assert!(prompt.contains("保留用户未要求变更的节点 ID、任务合同和已完成工作"));
        assert!(prompt.contains(STATE_SUMMARY));
        assert!(prompt.contains(USER_FEEDBACK));
        assert!(!prompt.contains("调整要求"));
        assert!(!prompt.contains("## 上次生成错误"));

        let schema_position = section_position(&prompt, "## 输出 JSON Schema");
        let state_position = section_position(&prompt, "## 当前状态");
        let feedback_position = section_position(&prompt, "## 最新用户反馈");
        let previous_plan_position = section_position(&prompt, "## 上一版计划");
        assert!(schema_position < state_position);
        assert!(state_position < feedback_position);
        assert!(feedback_position < previous_plan_position);

        let plan_json = serde_json::to_string_pretty(&sample_plan()).expect("plan serializes");
        assert!(prompt.contains(&plan_json));

        assert!(prompt.ends_with(&format!("{ITERATION_CLOSING_LINE}\n")));
    }

    #[test]
    fn build_is_byte_deterministic() {
        let input = iteration_input();
        assert_eq!(
            build_plan_generation_prompt(&input),
            build_plan_generation_prompt(&input)
        );
    }

    #[test]
    fn fixed_sections_are_byte_stable_across_modes() {
        let inputs = [
            input_with_mode(PlanGenerationMode::Initial),
            regeneration_input(),
            iteration_input(),
        ];
        for input in &inputs {
            let prompt = build_plan_generation_prompt(input);
            assert_eq!(section(&prompt, "编译规则"), COMPILE_RULES_SECTION);
            assert_eq!(section(&prompt, "节点字段说明"), NODE_FIELDS_SECTION);
            assert_eq!(section(&prompt, "规划原则"), PLANNING_PRINCIPLES_SECTION);
        }
    }

    #[test]
    fn retries_share_byte_identical_prefix_until_attempt_sections() {
        let prompt_a = build_plan_generation_prompt(&regeneration_input());
        let prompt_b =
            build_plan_generation_prompt(&input_with_mode(PlanGenerationMode::Regeneration {
                failure_reason: "另一个校验失败原因。".to_string(),
                previous_plan: None,
            }));
        let marker = "## 上次生成错误";
        let prefix_a = &prompt_a[..prompt_a.find(marker).expect("marker missing")];
        let prefix_b = &prompt_b[..prompt_b.find(marker).expect("marker missing")];
        assert_eq!(
            prefix_a, prefix_b,
            "attempt-level changes must not alter sections up to the Schema"
        );
    }

    #[test]
    fn schema_carries_graph_and_contract_constraints() {
        let prompt = build_plan_generation_prompt(&input_with_mode(PlanGenerationMode::Initial));
        let schema = extract_schema_json(&prompt);
        let member_enum = serde_json::json!([LEAD_ID, BACKEND_ID]);

        // Node id pattern / maxLength.
        let node_id = &schema["properties"]["nodes"]["items"]["properties"]["id"];
        assert_eq!(node_id["pattern"], "^[A-Za-z0-9][A-Za-z0-9._-]*$");
        assert_eq!(node_id["maxLength"], 128);
        assert_eq!(node_id["minLength"], 1);

        // agents: lead const, available minItems / uniqueItems / member enum.
        let agents = &schema["properties"]["agents"]["properties"];
        assert_eq!(agents["lead"]["const"], LEAD_ID);
        assert_eq!(agents["available"]["minItems"], 1);
        assert_eq!(agents["available"]["uniqueItems"], true);
        assert_eq!(agents["available"]["items"]["enum"], member_enum);

        // agentId enum mirrors the member list.
        let data = &schema["properties"]["nodes"]["items"]["properties"]["data"];
        assert_eq!(data["properties"]["agentId"]["enum"], member_enum);

        // Tiered acceptance: `required` tier mandatory and non-empty.
        let acceptance = &data["properties"]["acceptance"];
        assert_eq!(acceptance["required"], serde_json::json!(["required"]));
        assert_eq!(acceptance["properties"]["required"]["minItems"], 1);

        // Conditional requirements by stepType / reviewScope.
        let all_of = data["allOf"].as_array().expect("allOf must be an array");
        assert_eq!(all_of.len(), 3);

        let task_rule = all_of
            .iter()
            .find(|rule| rule["if"]["properties"]["stepType"]["const"] == "task")
            .expect("task conditional rule missing");
        assert_eq!(
            task_rule["then"]["required"],
            serde_json::json!([
                "acceptance",
                "outputs",
                "selfCheck",
                "verificationCommands",
                "completionEvidence"
            ])
        );
        for field in [
            "outputs",
            "selfCheck",
            "verificationCommands",
            "completionEvidence",
        ] {
            assert_eq!(task_rule["then"]["properties"][field]["minItems"], 1);
        }

        let scope_rule = all_of
            .iter()
            .find(|rule| rule["if"]["required"] == serde_json::json!(["reviewScope"]))
            .expect("reviewScope conditional rule missing");
        assert_eq!(
            scope_rule["then"]["required"],
            serde_json::json!(["acceptance"])
        );

        let no_scope_rule = all_of
            .iter()
            .find(|rule| {
                rule["if"]["properties"]["stepType"]["enum"]
                    == serde_json::json!(["task", "result"])
            })
            .expect("task/result reviewScope ban missing");
        assert_eq!(
            no_scope_rule["then"]["not"]["required"],
            serde_json::json!(["reviewScope"])
        );
    }

    #[test]
    fn two_phase_guidance_and_schema_appear_exactly_once() {
        let inputs = [
            input_with_mode(PlanGenerationMode::Initial),
            regeneration_input(),
            iteration_input(),
        ];
        for input in &inputs {
            let prompt = build_plan_generation_prompt(input);
            assert_eq!(prompt.matches("## 输出方式（两阶段）").count(), 1);
            assert_eq!(
                prompt
                    .matches("解析器只提取输出中最后一个完整 JSON 对象")
                    .count(),
                1
            );
            assert_eq!(prompt.matches("## 输出 JSON Schema").count(), 1);
        }
    }

    #[test]
    fn optional_sections_are_omitted_when_empty() {
        let mut input = input_with_mode(PlanGenerationMode::Initial);
        input.response_language = String::new();
        input.design_doc_paths = Vec::new();
        let prompt = build_plan_generation_prompt(&input);
        assert!(!prompt.contains("## 语言要求"));
        assert!(!prompt.contains("## 详细设计文档"));
        assert!(!prompt.contains("生成计划前必须阅读上述设计文档。"));
        assert!(!prompt.contains("None"));
        assert!(!prompt.contains("N/A"));

        let prompt = build_plan_generation_prompt(&input_with_mode(PlanGenerationMode::Initial));
        assert!(prompt.contains(&format!("## 语言要求\n\n{LANGUAGE_INSTRUCTION}")));
        assert!(prompt.contains(&format!(
            "## 详细设计文档\n\n- `{DESIGN_DOC}`\n\n生成计划前必须阅读上述设计文档。"
        )));
    }

    #[test]
    fn members_and_lead_render_in_input_order() {
        let prompt = build_plan_generation_prompt(&input_with_mode(PlanGenerationMode::Initial));
        assert!(prompt.contains(&format!(
            "- `{LEAD_ID}`：Lead；技术负责人；负责后端审核和最终结果把关；技能：code-review；工具：bash。"
        )));
        assert!(prompt.contains(&format!(
            "- `{BACKEND_ID}`：Backend；后端工程师；负责 Pi 强类型、供应商同步和 ACP 生命周期；技能：rust；工具：bash。"
        )));
        assert!(prompt.contains(&format!("Lead agent id：`{LEAD_ID}`")));
    }

    #[test]
    fn no_xml_boundaries_or_foreign_discriminators() {
        let prompt = build_plan_generation_prompt(&iteration_input());
        for forbidden in [
            "openteams_untrusted_data",
            "Data Boundary",
            "final_result",
            "review_result",
            "loop_review_result",
            "调整要求",
        ] {
            assert!(
                !prompt.contains(forbidden),
                "prompt must not contain {forbidden}"
            );
        }
    }

    #[test]
    fn compile_rules_stay_graph_semantic_only() {
        // Structural constraints expressible by the Schema must not be
        // restated as text (§8.2).
        for forbidden in [
            "minLength",
            "maxLength",
            "pattern",
            "minItems",
            "uniqueItems",
            "additionalProperties",
            "version",
            "title 非空",
        ] {
            assert!(
                !COMPILE_RULES_SECTION.contains(forbidden),
                "compile rules must not restate Schema constraint `{forbidden}`"
            );
        }
        for expected in [
            "全图唯一",
            "无环",
            "result",
            "agents.available",
            "reviewScope",
        ] {
            assert!(COMPILE_RULES_SECTION.contains(expected));
        }
    }
}
