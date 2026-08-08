//! Step execution core: lead review feedback loop, protocol message handling.

use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};

use chrono::Utc;
use db::{
    DBService,
    models::{
        chat_agent::ChatAgent,
        chat_session::ChatSession,
        chat_session_agent::ChatSessionAgent,
        workflow_agent_session::WorkflowAgentSession,
        workflow_event::{CreateWorkflowEvent, WorkflowEvent},
        workflow_execution::WorkflowExecution,
        workflow_plan::WorkflowPlan,
        workflow_step::WorkflowStep,
        workflow_step_edge::WorkflowStepEdge,
        workflow_step_review::{CreateWorkflowStepReview, WorkflowStepReview},
        workflow_types::*,
    },
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utils::assets::config_path;
use uuid::Uuid;

use super::{
    super::{
        chat_runner::ChatRunner,
        config,
        workflow_runtime::{
            self, SummaryPayload, WORKFLOW_PROTOCOL_PARSE_MAX_RETRIES, WorkflowAgentRunOutput,
            WorkflowReviewProtocolMessage, WorkflowRevisionFeedbackSource, WorkflowRuntimeError,
            WorkflowStepExecutionContract, WorkflowStepProtocolMessage, WorkflowStepRunResult,
            parse_step_review_protocol_output, parse_task_protocol_output, prompt_builders,
            resolve_workflow_response_language_instruction, result_aggregation,
            run_workflow_step_agent_follow_up, run_workflow_step_agent_prompt,
            should_retry_workflow_protocol_parse_failure, workflow_review_attempt_limit_reached,
        },
    },
    OrchestratorError, StepOutcome, WorkflowOrchestrator, resolve_step_workflow_session,
};
use crate::services::agent_skill_policy::AgentPromptContext;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(super) enum StepUserReviewResolution {
    Approved { feedback: String },
    Rejected { feedback: String },
    Parked,
}

#[derive(Debug, Clone)]
struct ActiveFrontierWorkspaceConflict {
    workspace_path: String,
    members: Vec<ActiveFrontierWorkspaceMember>,
}

#[derive(Debug, Clone)]
struct ActiveFrontierWorkspaceMember {
    session_agent_id: Uuid,
    agent_id: Uuid,
    agent_name: String,
    step_key: String,
}

fn detect_active_frontier_workspace_conflicts(
    session: &ChatSession,
    running_step: &WorkflowStep,
    current_steps: &[WorkflowStep],
    edges: &[WorkflowStepEdge],
    workflow_agent_sessions: &[WorkflowAgentSession],
    session_agents: &[ChatSessionAgent],
    agents: &[ChatAgent],
) -> Vec<ActiveFrontierWorkspaceConflict> {
    let mut step_by_id: HashMap<Uuid, WorkflowStep> = current_steps
        .iter()
        .cloned()
        .map(|step| (step.id, step))
        .collect();
    step_by_id.insert(running_step.id, running_step.clone());

    let predecessors_by_step = edges.iter().fold(
        HashMap::<Uuid, Vec<Uuid>>::new(),
        |mut predecessors, edge| {
            predecessors
                .entry(edge.to_step_id)
                .or_default()
                .push(edge.from_step_id);
            predecessors
        },
    );
    let workflow_session_by_id: HashMap<Uuid, &WorkflowAgentSession> = workflow_agent_sessions
        .iter()
        .map(|workflow_session| (workflow_session.id, workflow_session))
        .collect();
    let session_agent_by_id: HashMap<Uuid, &ChatSessionAgent> = session_agents
        .iter()
        .map(|session_agent| (session_agent.id, session_agent))
        .collect();
    let agent_by_id: HashMap<Uuid, &ChatAgent> =
        agents.iter().map(|agent| (agent.id, agent)).collect();
    let mut members_by_workspace: BTreeMap<String, BTreeMap<Uuid, ActiveFrontierWorkspaceMember>> =
        BTreeMap::new();

    for step in step_by_id.values() {
        if step.step_type != WorkflowStepType::Task || !is_active_frontier_step(step) {
            continue;
        }
        let predecessors_completed = predecessors_by_step
            .get(&step.id)
            .map(|predecessors| {
                predecessors.iter().all(|predecessor_id| {
                    step_by_id
                        .get(predecessor_id)
                        .is_some_and(is_completed_like_step)
                })
            })
            .unwrap_or(true);
        if !predecessors_completed {
            continue;
        }

        let Some(workflow_session_id) = step.assigned_workflow_agent_session_id else {
            continue;
        };
        let Some(workflow_session) = workflow_session_by_id.get(&workflow_session_id) else {
            continue;
        };
        let Some(session_agent) = session_agent_by_id.get(&workflow_session.session_agent_id)
        else {
            continue;
        };
        let Some(agent) = agent_by_id.get(&session_agent.agent_id) else {
            continue;
        };
        let workspace_path = normalize_workspace_path(
            &workflow_runtime::resolve_workspace_path_snapshot(session, agent, session_agent),
        );
        if workspace_path.is_empty() {
            continue;
        }

        members_by_workspace
            .entry(workspace_path)
            .or_default()
            .entry(session_agent.id)
            .or_insert_with(|| ActiveFrontierWorkspaceMember {
                session_agent_id: session_agent.id,
                agent_id: agent.id,
                agent_name: session_agent.member_name.clone(),
                step_key: step.step_key.clone(),
            });
    }

    members_by_workspace
        .into_iter()
        .filter_map(|(workspace_path, members)| {
            if members.len() <= 1 {
                return None;
            }
            Some(ActiveFrontierWorkspaceConflict {
                workspace_path,
                members: members.into_values().collect(),
            })
        })
        .collect()
}

fn is_active_frontier_step(step: &WorkflowStep) -> bool {
    matches!(
        step.status,
        WorkflowStepStatus::Ready | WorkflowStepStatus::Running | WorkflowStepStatus::Revising
    )
}

fn is_completed_like_step(step: &WorkflowStep) -> bool {
    matches!(
        step.status,
        WorkflowStepStatus::Completed | WorkflowStepStatus::Skipped
    )
}

fn markdown_inline_value(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('`', "ˋ")
}

fn render_workspace_isolation_section(conflict: &ActiveFrontierWorkspaceConflict) -> String {
    let members = conflict
        .members
        .iter()
        .map(|member| {
            format!(
                "  - `{}`（Agent ID：`{}`）正在执行步骤 `{}`",
                markdown_inline_value(&member.agent_name),
                member.agent_id,
                markdown_inline_value(&member.step_key),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "## 工作区隔离要求\n\n当前工作流前沿有多个成员正在同一工作区并行执行：\n\n- 共享工作区：`{}`\n- 并行成员：\n{}\n\n执行要求：\n\n1. Git 可用时，修改文件前为当前步骤创建独立 worktree。\n2. 所有编辑和验证都必须在该隔离 worktree 中完成。\n3. 返回 `final_result` 前，将完成的变更合并或同步回原工作流工作区，清理临时 worktree，并在结构化证据中记录合并结果。\n4. Git worktree 不可用时，报告阻塞原因，不得虚构 skill 或隔离机制。",
        markdown_inline_value(&conflict.workspace_path),
        members,
    )
}

#[cfg(test)]
mod workspace_isolation_prompt_tests {
    use super::*;

    #[test]
    fn workspace_isolation_section_is_plain_markdown() {
        let conflict = ActiveFrontierWorkspaceConflict {
            workspace_path: "/tmp/shared workspace".to_string(),
            members: vec![ActiveFrontierWorkspaceMember {
                session_agent_id: Uuid::new_v4(),
                agent_id: Uuid::nil(),
                agent_name: "Backend\nMember`Injected".to_string(),
                step_key: "backend-task".to_string(),
            }],
        };

        let section = render_workspace_isolation_section(&conflict);

        assert!(section.starts_with("## 工作区隔离要求\n\n"));
        assert!(section.contains("- 共享工作区：`/tmp/shared workspace`"));
        assert!(section.contains("`Backend MemberˋInjected`"));
        assert!(section.contains("1. Git 可用时"));
        assert!(!section.contains("Workspace Isolation Requirement"));
        assert!(!section.contains("openteams_untrusted_data"));
        assert!(!section.contains("Data Boundary"));
    }
}

fn normalize_workspace_path(path: &Path) -> String {
    let mut normalized = path.to_string_lossy().trim().replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    #[cfg(windows)]
    {
        normalized = normalized.to_ascii_lowercase();
    }
    normalized
}

fn inject_step_prompt_section_before_schema(prompt: &mut String, section: &str) {
    if let Some(index) = prompt
        .find("\n\n## 输出 JSON Schema")
        .or_else(|| prompt.find("\n\nRequired JSON Schema:"))
    {
        prompt.insert_str(index, section);
    } else {
        prompt.push_str(section);
    }
}

#[derive(Debug, Clone)]
pub(super) struct PersistedWorkerAttempt {
    pub(super) step: WorkflowStep,
    pub(super) result: WorkflowStepRunResult,
}

#[allow(clippy::too_many_arguments)]
fn workflow_step_run_result_from_task_report(
    run_id: Uuid,
    status: workflow_runtime::WorkflowTaskCompletionStatus,
    summary: String,
    content: String,
    verification: Vec<workflow_runtime::WorkflowVerificationResult>,
    files_changed: Vec<String>,
    self_review: Vec<String>,
    issues: Vec<String>,
    evidence: Vec<String>,
    outputs: Vec<String>,
) -> WorkflowStepRunResult {
    let structured_report = serde_json::json!({
        "type": "final_result",
        "status": status,
        "summary": summary,
        "content": content,
        "verification": verification,
        "files_changed": files_changed,
        "self_review": self_review,
        "issues": issues,
        "evidence": evidence,
        "outputs": outputs,
    })
    .to_string();
    WorkflowStepRunResult {
        run_id,
        summary,
        content,
        outputs,
        structured_report: Some(structured_report),
    }
}

#[cfg(test)]
mod structured_task_report_tests {
    use super::*;

    #[test]
    fn task_review_handoff_preserves_all_structured_completion_fields() {
        let result = workflow_step_run_result_from_task_report(
            Uuid::new_v4(),
            workflow_runtime::WorkflowTaskCompletionStatus::DoneWithConcerns,
            "implemented".to_string(),
            "details".to_string(),
            vec![workflow_runtime::WorkflowVerificationResult {
                name: "cargo test".to_string(),
                command: Some("cargo test -p services".to_string()),
                status: workflow_runtime::WorkflowVerificationStatus::Passed,
                evidence: "all tests passed".to_string(),
            }],
            vec!["src/lib.rs".to_string()],
            vec!["reviewed error handling".to_string()],
            vec!["known compatibility concern".to_string()],
            vec!["test log".to_string()],
            vec!["src/lib.rs".to_string()],
        );
        let report: serde_json::Value = serde_json::from_str(
            result
                .structured_report
                .as_deref()
                .expect("structured report must be retained"),
        )
        .expect("structured report JSON");

        assert_eq!(report["verification"][0]["name"], "cargo test");
        assert_eq!(report["files_changed"][0], "src/lib.rs");
        assert_eq!(report["self_review"][0], "reviewed error handling");
        assert_eq!(report["issues"][0], "known compatibility concern");
        assert_eq!(report["evidence"][0], "test log");
        assert_eq!(report["outputs"][0], "src/lib.rs");
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(super) struct PendingRevisionFeedback {
    pub(super) source: WorkflowRevisionFeedbackSource,
    pub(super) feedback: String,
    pub(super) previous_summary: String,
    pub(super) previous_content: Option<String>,
    pub(super) previous_outputs: Vec<String>,
    pub(super) review_details: Option<RevisionReviewDetails>,
    pub(super) review_round: i32,
    pub(super) loop_key: Option<String>,
    pub(super) loop_rejection_reason: Option<String>,
    pub(super) other_steps_feedback_summary: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct RevisionReviewDetails {
    pub(super) acceptance_results: Vec<workflow_runtime::WorkflowAcceptanceResult>,
    pub(super) evidence: Vec<String>,
    pub(super) risks: Vec<String>,
    pub(super) unfinished_items: Vec<String>,
}

impl WorkflowOrchestrator {
    pub(super) fn parse_step_output_message(
        execution_id: Uuid,
        step: &WorkflowStep,
        raw_output: &str,
    ) -> Result<WorkflowStepProtocolMessage, OrchestratorError> {
        tracing::debug!(
            "Parsing protocol output for step {}: {}",
            step.step_key,
            raw_output
        );

        parse_task_protocol_output(execution_id, &step.step_key, raw_output)
            .map_err(OrchestratorError::from)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_step_agent_protocol_with_retry(
        db: &DBService,
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        session: &ChatSession,
        agent: &ChatAgent,
        session_agent: &ChatSessionAgent,
        workflow_session: &WorkflowAgentSession,
        prompt: &str,
        step: &WorkflowStep,
        first_run_is_follow_up: bool,
    ) -> Result<(WorkflowStepProtocolMessage, WorkflowAgentRunOutput), OrchestratorError> {
        let mut attempt = 0;
        let mut run_as_follow_up = first_run_is_follow_up;
        let mut prompt_to_send = prompt.to_string();

        loop {
            let active_workflow_session = if run_as_follow_up {
                WorkflowAgentSession::find_by_id(pool, workflow_session.id)
                    .await?
                    .ok_or_else(|| {
                        OrchestratorError::NotFound(format!(
                            "workflow session {} not found",
                            workflow_session.id
                        ))
                    })?
            } else {
                workflow_session.clone()
            };

            let agent_output = if run_as_follow_up {
                run_workflow_step_agent_follow_up(
                    db,
                    chat_runner,
                    session,
                    agent,
                    session_agent,
                    &active_workflow_session,
                    &prompt_to_send,
                    step,
                )
                .await?
            } else {
                run_workflow_step_agent_prompt(
                    db,
                    chat_runner,
                    session,
                    agent,
                    session_agent,
                    Some(&active_workflow_session),
                    &prompt_to_send,
                    step,
                )
                .await?
            };
            let raw_output = &agent_output.output;

            match Self::parse_step_output_message(step.execution_id, step, raw_output) {
                Ok(message) => return Ok((message, agent_output)),
                Err(err)
                    if attempt < WORKFLOW_PROTOCOL_PARSE_MAX_RETRIES
                        && should_retry_workflow_protocol_parse_failure(raw_output) =>
                {
                    tracing::warn!(
                        step_id = %step.id,
                        step_key = %step.step_key,
                        attempt,
                        error = %err,
                        "workflow step protocol parse failed; retrying"
                    );
                    prompt_to_send = prompt_builders::common::append_protocol_error_section(
                        prompt,
                        &err.to_string(),
                    );
                    attempt += 1;
                    run_as_follow_up = true;
                }
                Err(err) => return Err(err),
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_step_review_protocol_with_retry(
        db: &DBService,
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        session: &ChatSession,
        agent: &ChatAgent,
        session_agent: &ChatSessionAgent,
        workflow_session: &WorkflowAgentSession,
        prompt: &str,
        step: &WorkflowStep,
        declared_acceptance: &[workflow_runtime::WorkflowReviewCriterion],
    ) -> Result<(WorkflowReviewProtocolMessage, WorkflowAgentRunOutput), OrchestratorError> {
        let mut attempt = 0;
        let mut run_as_follow_up = false;
        let mut prompt_to_send = prompt.to_string();

        loop {
            let active_workflow_session = if run_as_follow_up {
                WorkflowAgentSession::find_by_id(pool, workflow_session.id)
                    .await?
                    .ok_or_else(|| {
                        OrchestratorError::NotFound(format!(
                            "workflow session {} not found",
                            workflow_session.id
                        ))
                    })?
            } else {
                workflow_session.clone()
            };

            let agent_output = if run_as_follow_up {
                run_workflow_step_agent_follow_up(
                    db,
                    chat_runner,
                    session,
                    agent,
                    session_agent,
                    &active_workflow_session,
                    &prompt_to_send,
                    step,
                )
                .await?
            } else {
                run_workflow_step_agent_prompt(
                    db,
                    chat_runner,
                    session,
                    agent,
                    session_agent,
                    Some(&active_workflow_session),
                    &prompt_to_send,
                    step,
                )
                .await?
            };
            match parse_step_review_protocol_output(
                execution.id,
                &step.step_key,
                declared_acceptance,
                &agent_output.output,
            ) {
                Ok(message) => return Ok((message, agent_output)),
                Err(err)
                    if attempt < WORKFLOW_PROTOCOL_PARSE_MAX_RETRIES
                        && should_retry_workflow_protocol_parse_failure(&agent_output.output) =>
                {
                    tracing::warn!(
                        step_id = %step.id,
                        step_key = %step.step_key,
                        attempt,
                        error = %err,
                        "workflow review protocol parse failed; retrying"
                    );
                    prompt_to_send = prompt_builders::common::append_protocol_error_section(
                        prompt,
                        &err.to_string(),
                    );
                    attempt += 1;
                    run_as_follow_up = true;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    pub(super) fn step_message_error(
        message: String,
        content: Option<String>,
    ) -> OrchestratorError {
        OrchestratorError::Runtime(WorkflowRuntimeError::Validation(
            content
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("{message}: {value}"))
                .unwrap_or(message),
        ))
    }

    /// result 节点的确定性执行（设计 §9.4、§10.3）：聚合全部传递前驱的最新
    /// 有效结果，构造并写入 `result_review_result`，不发起任何 agent 调用。
    async fn run_result_step_deterministic(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        workflow_session: &WorkflowAgentSession,
        plan: &WorkflowPlan,
        running_step: WorkflowStep,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
    ) -> Result<StepOutcome, OrchestratorError> {
        let workflow_goal = plan
            .summary_text
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| plan.title.clone());
        let input = workflow_runtime::result_aggregation::ResultAggregationInput {
            execution_id: execution.id,
            step_key: running_step.step_key.clone(),
            workflow_goal,
            title: running_step.title.clone(),
            instructions: running_step.instructions.clone(),
            latest_node_results: Self::collect_result_aggregation_inputs(
                &running_step,
                current_steps,
                edges,
            )?,
        };
        let output = workflow_runtime::result_aggregation::construct_result_review_output(&input)
            .map_err(OrchestratorError::from)?;
        let overall_status = output.overall_status;
        let summary = output.summary.clone();
        let structured_content = serde_json::json!({
            "type": "result_review_result",
            "step_key": running_step.step_key,
            "execution_id": execution.id,
            "overall_status": overall_status,
            "summary": output.summary,
            "content": output.content,
            "deliverables": output.deliverables,
            "acceptance_results": output.acceptance_results,
            "evidence": output.evidence,
            "risks": output.risks,
            "unfinished_items": output.unfinished_items,
        })
        .to_string();
        let recorded_step = WorkflowStep::record_execution_result(
            pool,
            running_step.id,
            Uuid::new_v4(),
            Some(
                serde_json::to_string(&SummaryPayload {
                    summary: summary.clone(),
                    content: Some(structured_content.clone()),
                    outputs: Vec::new(),
                })
                .unwrap_or_else(|_| summary.clone()),
            ),
            Some(structured_content.clone()),
        )
        .await?;
        let _ = Self::write_transcript(
            pool,
            execution.id,
            recorded_step.round_id.into(),
            Some(workflow_session.id),
            Some(recorded_step.id),
            "system",
            "result_review",
            &summary,
            Some(
                &serde_json::json!({
                    "source": "workflow_result_aggregation",
                    "structured_result": serde_json::from_str::<serde_json::Value>(&structured_content)
                        .unwrap_or(serde_json::Value::Null),
                })
                .to_string(),
            ),
        )
        .await;
        let completed = !matches!(
            overall_status,
            workflow_runtime::WorkflowResultOverallStatus::Blocked
        );
        Self::transition_step_and_sync(
            pool,
            chat_runner,
            execution,
            &recorded_step,
            if completed {
                WorkflowStepStatus::Completed
            } else {
                WorkflowStepStatus::Failed
            },
            if completed {
                "result_step_completed"
            } else {
                "result_step_blocked"
            },
        )
        .await?;
        if completed {
            Ok(StepOutcome::Completed)
        } else {
            Ok(StepOutcome::Failed(summary))
        }
    }

    /// 收集 result 节点全部传递前驱的最新有效结果（设计 §6.6、§13）。
    /// 任一传递前驱缺少有效结果或仍处于阻塞态时直接返回构造错误，result
    /// 节点不得以占位数据执行汇总（§14）。
    fn collect_result_aggregation_inputs(
        result_step: &WorkflowStep,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
    ) -> Result<Vec<workflow_runtime::result_aggregation::FinalNodeResultInput>, OrchestratorError>
    {
        let mut upstream_ids = std::collections::HashSet::new();
        let mut stack = vec![result_step.id];
        while let Some(step_id) = stack.pop() {
            for edge in edges.iter().filter(|edge| edge.to_step_id == step_id) {
                if upstream_ids.insert(edge.from_step_id) {
                    stack.push(edge.from_step_id);
                }
            }
        }
        let mut upstream_steps = current_steps
            .iter()
            .filter(|step| upstream_ids.contains(&step.id))
            .collect::<Vec<_>>();
        upstream_steps.sort_by_key(|step| step.display_order);

        upstream_steps
            .into_iter()
            .map(|step| {
                let result =
                    workflow_runtime::result_aggregation::final_node_result_from_step(step)
                        .ok_or_else(|| {
                            OrchestratorError::Runtime(WorkflowRuntimeError::Validation(format!(
                                "result 节点 '{}' 的传递前驱 '{}' 缺少最新有效结果",
                                result_step.step_key, step.step_key
                            )))
                        })?;
                if matches!(
                    result.status,
                    workflow_runtime::WorkflowTaskCompletionStatus::Blocked
                        | workflow_runtime::WorkflowTaskCompletionStatus::NeedsContext
                ) {
                    return Err(OrchestratorError::Runtime(
                        WorkflowRuntimeError::Validation(format!(
                            "result 节点 '{}' 的传递前驱 '{}' 尚未形成可汇总结果",
                            result_step.step_key, step.step_key
                        )),
                    ));
                }
                Ok(result)
            })
            .collect()
    }

    fn execution_contract_for_step(
        plan: &WorkflowPlan,
        step: &WorkflowStep,
    ) -> WorkflowStepExecutionContract {
        let clean = |items: Option<Vec<String>>| {
            items
                .unwrap_or_default()
                .into_iter()
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        };
        serde_json::from_str::<WorkflowPlanJson>(&plan.plan_json)
            .ok()
            .and_then(|plan_json| {
                plan_json
                    .nodes
                    .into_iter()
                    .find(|node| node.id == step.step_key)
                    .map(|node| node.data)
            })
            .map(|data| {
                let acceptance = data.acceptance;
                let acceptance_leveled = acceptance
                    .clone()
                    .map(|value| value.leveled())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(level, item)| (level, item.trim().to_string()))
                    .filter(|(_, item)| !item.is_empty())
                    .collect();
                WorkflowStepExecutionContract {
                    acceptance_leveled,
                    expected_outputs: clean(data.outputs),
                    self_check: clean(data.self_check),
                    verification_commands: clean(data.verification_commands),
                    completion_evidence: clean(data.completion_evidence),
                }
            })
            .unwrap_or_default()
    }

    pub(super) fn acceptance_criteria_for_step(
        plan: &WorkflowPlan,
        step: &WorkflowStep,
    ) -> Vec<(AcceptanceCriterionLevel, String)> {
        Self::execution_contract_for_step(plan, step).acceptance_leveled
    }

    /// 当前节点的分级验收标准对象（计划 JSON 直读，未经扁平化）。
    pub(crate) fn acceptance_criteria_object_for_step(
        plan: &WorkflowPlan,
        step: &WorkflowStep,
    ) -> AcceptanceCriteria {
        serde_json::from_str::<WorkflowPlanJson>(&plan.plan_json)
            .ok()
            .and_then(|plan_json| {
                plan_json
                    .nodes
                    .into_iter()
                    .find(|node| node.id == step.step_key)
            })
            .and_then(|node| node.data.acceptance)
            .unwrap_or_default()
    }

    /// 当前节点的必要上游结果（设计 §6.1、§13）：仅直接前驱的最新有效结果。
    fn upstream_results_for_step(
        step: &WorkflowStep,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
    ) -> Result<Vec<prompt_builders::common::UpstreamResultInput>, OrchestratorError> {
        let predecessor_ids = edges
            .iter()
            .filter(|edge| edge.to_step_id == step.id)
            .map(|edge| edge.from_step_id)
            .collect::<std::collections::HashSet<_>>();
        current_steps
            .iter()
            .filter(|candidate| predecessor_ids.contains(&candidate.id))
            .map(|candidate| {
                let result = result_aggregation::final_node_result_from_step(candidate)
                    .ok_or_else(|| {
                        OrchestratorError::Runtime(WorkflowRuntimeError::Validation(format!(
                            "步骤 '{}' 的必要上游 '{}' 缺少最新有效结果",
                            step.step_key, candidate.step_key
                        )))
                    })?;
                if matches!(
                    result.status,
                    workflow_runtime::WorkflowTaskCompletionStatus::Blocked
                        | workflow_runtime::WorkflowTaskCompletionStatus::NeedsContext
                ) {
                    return Err(OrchestratorError::Runtime(
                        WorkflowRuntimeError::Validation(format!(
                            "步骤 '{}' 的必要上游 '{}' 尚未形成可用结果",
                            step.step_key, candidate.step_key
                        )),
                    ));
                }
                Ok(prompt_builders::common::UpstreamResultInput {
                    step_key: result.step_key,
                    summary: result.summary,
                    outputs: result.outputs,
                })
            })
            .collect()
    }

    /// 普通 review 节点的"执行者最新结果"（设计 §6.4）：其直接上游最新有效
    /// 结果的合并视图。
    fn review_worker_result_for_step(
        step: &WorkflowStep,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
    ) -> Result<prompt_builders::step_review::TaskResultInput, OrchestratorError> {
        let predecessor_ids = edges
            .iter()
            .filter(|edge| edge.to_step_id == step.id)
            .map(|edge| edge.from_step_id)
            .collect::<std::collections::HashSet<_>>();
        let results = current_steps
            .iter()
            .filter(|candidate| predecessor_ids.contains(&candidate.id))
            .map(|candidate| {
                let result = result_aggregation::final_node_result_from_step(candidate)
                    .ok_or_else(|| {
                        OrchestratorError::Runtime(WorkflowRuntimeError::Validation(format!(
                            "review 步骤 '{}' 的待审上游 '{}' 缺少最新有效结果",
                            step.step_key, candidate.step_key
                        )))
                    })?;
                if matches!(
                    result.status,
                    workflow_runtime::WorkflowTaskCompletionStatus::Blocked
                        | workflow_runtime::WorkflowTaskCompletionStatus::NeedsContext
                ) {
                    return Err(OrchestratorError::Runtime(
                        WorkflowRuntimeError::Validation(format!(
                            "review 步骤 '{}' 的待审上游 '{}' 尚未形成可审核结果",
                            step.step_key, candidate.step_key
                        )),
                    ));
                }
                Ok(result)
            })
            .collect::<Result<Vec<_>, OrchestratorError>>()?;
        Ok(prompt_builders::step_review::TaskResultInput {
            status: String::new(),
            summary: results
                .iter()
                .map(|result| format!("`{}`：{}", result.step_key, result.summary))
                .collect::<Vec<_>>()
                .join("\n"),
            outputs: results
                .iter()
                .flat_map(|result| result.outputs.clone())
                .collect(),
            verification: results
                .iter()
                .flat_map(|result| result.evidence.clone())
                .collect(),
            self_review: Vec::new(),
            issues: results
                .iter()
                .flat_map(|result| result.issues.clone())
                .collect(),
        })
    }

    /// 返工 prompt 的统一构造（设计 §6.3、§11.2）：首次执行与
    /// Lead/Reviewer/User 反馈返工共用 task builder。
    #[allow(clippy::too_many_arguments)]
    fn build_task_revision_prompt(
        plan: &WorkflowPlan,
        step: &WorkflowStep,
        workflow_goal: &str,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
        source: WorkflowRevisionFeedbackSource,
        feedback: &str,
        previous_summary: &str,
        previous_outputs: &[String],
        review_details: Option<&RevisionReviewDetails>,
        response_language: &str,
    ) -> Result<String, OrchestratorError> {
        let contract = Self::execution_contract_for_step(plan, step);
        Ok(
            prompt_builders::task_execution::build_task_execution_prompt(
                &prompt_builders::task_execution::TaskExecutionPromptInput {
                    identity: prompt_builders::common::PromptIdentity {
                        execution_id: step.execution_id,
                        step_key: step.step_key.clone(),
                    },
                    workflow_goal: workflow_goal.to_string(),
                    title: step.title.clone(),
                    instructions: step.instructions.clone(),
                    contract: prompt_builders::task_execution::TaskExecutionContractInput {
                        outputs: contract.expected_outputs,
                        self_check: contract.self_check,
                        verification_methods: contract.verification_commands,
                        completion_evidence: contract.completion_evidence,
                    },
                    upstream_results: Self::upstream_results_for_step(step, current_steps, edges)?,
                    revision: Some(prompt_builders::task_execution::RevisionContextInput {
                        source,
                        attempt: step.retry_count.saturating_add(1),
                        feedback: feedback.to_string(),
                        previous_summary: previous_summary.to_string(),
                        previous_outputs: previous_outputs.to_vec(),
                        review_outcome: review_details.map(|details| {
                            prompt_builders::task_execution::ReviewOutcomeInput {
                                acceptance_results: details.acceptance_results.clone(),
                                evidence: details.evidence.clone(),
                                risks: details.risks.clone(),
                                unfinished_items: details.unfinished_items.clone(),
                            }
                        }),
                    }),
                    response_language: response_language.to_string(),
                },
            ),
        )
    }

    /// Lead 内审的执行者结果视图（设计 §6.4）：来自持久化的结构化任务报告，
    /// 自检结论仅作为报告的一部分呈现，审核者不得直接采信。
    fn task_result_input_from_run_result(
        result: &WorkflowStepRunResult,
    ) -> prompt_builders::step_review::TaskResultInput {
        let structured = result
            .structured_report
            .as_deref()
            .and_then(|report| serde_json::from_str::<serde_json::Value>(report).ok());
        let string_array = |key: &str| {
            structured
                .as_ref()
                .and_then(|value| value.get(key))
                .and_then(|items| serde_json::from_value::<Vec<String>>(items.clone()).ok())
                .unwrap_or_default()
        };
        let status = structured
            .as_ref()
            .and_then(|value| value.get("status"))
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let verification = structured
            .as_ref()
            .and_then(|value| value.get("verification"))
            .and_then(|items| items.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let name = item.get("name")?.as_str()?;
                        let status = item
                            .get("status")
                            .and_then(|value| value.as_str())
                            .unwrap_or("not_run");
                        Some(format!("{name}（{status}）"))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        prompt_builders::step_review::TaskResultInput {
            status,
            summary: result.summary.clone(),
            outputs: result.outputs.clone(),
            verification,
            self_review: string_array("self_review"),
            issues: string_array("issues"),
        }
    }

    pub(super) fn merge_revision_context(
        existing_revision_context: Option<&str>,
        feedback_source: WorkflowRevisionFeedbackSource,
        feedback_content: &str,
        previous_summary: &str,
        previous_content: Option<&str>,
        previous_outputs: &[String],
        review_round: i32,
        review_details: Option<&RevisionReviewDetails>,
    ) -> String {
        let mut context = existing_revision_context
            .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if !context.is_object() {
            context = serde_json::json!({});
        }

        let source = match feedback_source {
            WorkflowRevisionFeedbackSource::Lead => "lead",
            WorkflowRevisionFeedbackSource::Reviewer => "reviewer",
            WorkflowRevisionFeedbackSource::User => "user",
        };

        let entry = serde_json::json!({
            "round": review_round,
            "source": source,
            "feedback": feedback_content.trim(),
            "timestamp": Utc::now().to_rfc3339(),
        });

        let context_obj = context.as_object_mut().expect("revision context object");
        let history = context_obj
            .entry("feedback_history")
            .or_insert_with(|| serde_json::json!([]));
        if !history.is_array() {
            *history = serde_json::json!([]);
        }
        history
            .as_array_mut()
            .expect("feedback history array")
            .push(entry);

        context_obj.insert(
            "previous_summary".to_string(),
            serde_json::json!(previous_summary.trim()),
        );
        context_obj.insert(
            "previous_content".to_string(),
            serde_json::json!(previous_content.unwrap_or_default().trim()),
        );
        context_obj.insert(
            "previous_outputs".to_string(),
            serde_json::json!(previous_outputs),
        );
        context_obj.insert(
            "pending_feedback".to_string(),
            serde_json::json!({
                "source": source,
                "feedback": feedback_content.trim(),
                "previous_summary": previous_summary.trim(),
                "previous_content": previous_content.unwrap_or_default().trim(),
                "previous_outputs": previous_outputs,
                "review_round": review_round,
                "review_details": review_details,
            }),
        );

        context.to_string()
    }

    pub(super) fn parse_pending_revision_feedback(
        revision_context: Option<&str>,
    ) -> Option<PendingRevisionFeedback> {
        let value =
            revision_context.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())?;
        let pending = value.get("pending_feedback")?;
        let source = match pending.get("source")?.as_str()? {
            "lead" => WorkflowRevisionFeedbackSource::Lead,
            "reviewer" => WorkflowRevisionFeedbackSource::Reviewer,
            "user" => WorkflowRevisionFeedbackSource::User,
            _ => return None,
        };

        Some(PendingRevisionFeedback {
            source,
            feedback: pending.get("feedback")?.as_str()?.trim().to_string(),
            previous_summary: pending
                .get("previous_summary")
                .and_then(|item| item.as_str())
                .unwrap_or_default()
                .trim()
                .to_string(),
            previous_content: pending
                .get("previous_content")
                .and_then(|item| item.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            previous_outputs: pending
                .get("previous_outputs")
                .and_then(|item| item.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str())
                        .map(|item| item.to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
            review_details: pending
                .get("review_details")
                .cloned()
                .and_then(|value| serde_json::from_value(value).ok()),
            review_round: pending
                .get("review_round")
                .and_then(|item| item.as_i64())
                .unwrap_or_default() as i32,
            loop_key: pending
                .get("loop_key")
                .and_then(|item| item.as_str())
                .map(str::to_string),
            loop_rejection_reason: pending
                .get("loop_rejection_reason")
                .and_then(|item| item.as_str())
                .map(str::to_string),
            other_steps_feedback_summary: pending
                .get("other_steps_feedback_summary")
                .and_then(|item| item.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    pub(super) fn pending_revision_feedback_is_loop(revision_context: Option<&str>) -> bool {
        revision_context
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|value| value.get("pending_feedback").cloned())
            .is_some_and(|pending| {
                pending.get("scope").and_then(|value| value.as_str()) == Some("loop")
            })
    }

    pub(super) fn clear_pending_revision_feedback(
        existing_revision_context: Option<&str>,
    ) -> Option<String> {
        let mut value = existing_revision_context
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())?;
        let object = value.as_object_mut()?;
        object.remove("pending_feedback");
        Some(value.to_string())
    }

    pub(super) async fn emit_step_domain_event(
        pool: &SqlitePool,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        event_type: WorkflowEventType,
        detail_json: Option<serde_json::Value>,
    ) -> Result<WorkflowEvent, OrchestratorError> {
        WorkflowEvent::create(
            pool,
            &CreateWorkflowEvent {
                execution_id: execution.id,
                round_id: Some(step.round_id),
                step_id: Some(step.id),
                agent_session_id: step.assigned_workflow_agent_session_id,
                event_type,
                status_before: None,
                status_after: Some(to_workflow_wire_value(&step.status)),
                detail_json: detail_json.map(|value| value.to_string()),
            },
            Uuid::new_v4(),
        )
        .await
        .map_err(OrchestratorError::Database)
    }

    pub(crate) async fn save_step_review(
        pool: &SqlitePool,
        step: &WorkflowStep,
        reviewer_type: ReviewerType,
        reviewer_id: Option<String>,
        verdict: ReviewVerdict,
        feedback: &str,
    ) -> Result<WorkflowStepReview, OrchestratorError> {
        let review_round = WorkflowStepReview::find_by_step(pool, step.id)
            .await?
            .iter()
            .filter(|review| review.reviewer_type == reviewer_type)
            .count() as i32
            + 1;
        WorkflowStepReview::create(
            pool,
            &CreateWorkflowStepReview {
                step_id: step.id,
                execution_id: step.execution_id,
                reviewer_type,
                reviewer_id,
                verdict,
                feedback: feedback.trim().to_string(),
                review_round: Some(review_round),
            },
            Uuid::new_v4(),
        )
        .await
        .map_err(OrchestratorError::Database)
    }

    pub(crate) async fn next_lead_review_attempt(
        pool: &SqlitePool,
        step_id: Uuid,
    ) -> Result<i32, OrchestratorError> {
        Self::next_review_attempt(pool, step_id, ReviewerType::Lead).await
    }

    pub(crate) async fn next_review_attempt(
        pool: &SqlitePool,
        step_id: Uuid,
        reviewer_type: ReviewerType,
    ) -> Result<i32, OrchestratorError> {
        Ok(
            WorkflowStepReview::count_reviews_in_current_cycle(pool, step_id, reviewer_type)
                .await?
                + 1,
        )
    }

    fn resolve_lead_review_targets<'a>(
        execution: &WorkflowExecution,
        workflow_sessions: &'a [WorkflowAgentSession],
        session_agents: &'a [ChatSessionAgent],
        agents: &'a [ChatAgent],
    ) -> Result<
        (
            &'a WorkflowAgentSession,
            &'a ChatSessionAgent,
            &'a ChatAgent,
        ),
        OrchestratorError,
    > {
        let lead_session_agent_id = execution.lead_session_agent_id.ok_or_else(|| {
            OrchestratorError::NotFound(format!(
                "execution {} 缺少 lead session agent",
                execution.id
            ))
        })?;
        let workflow_session = workflow_sessions
            .iter()
            .find(|session| session.session_agent_id == lead_session_agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "execution {} 的 lead workflow session 未找到",
                    execution.id
                ))
            })?;
        let session_agent = session_agents
            .iter()
            .find(|item| item.id == workflow_session.session_agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "lead session agent {} 未找到",
                    workflow_session.session_agent_id
                ))
            })?;
        let agent = agents
            .iter()
            .find(|item| item.id == session_agent.agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("agent {} 未找到", session_agent.agent_id))
            })?;

        Ok((workflow_session, session_agent, agent))
    }

    async fn persist_worker_attempt_result(
        pool: &SqlitePool,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        workflow_session: &WorkflowAgentSession,
        result: WorkflowStepRunResult,
    ) -> Result<PersistedWorkerAttempt, OrchestratorError> {
        let persisted_content = result
            .structured_report
            .as_deref()
            .unwrap_or(&result.content)
            .to_string();
        let recorded_step = WorkflowStep::record_execution_result(
            pool,
            step.id,
            result.run_id,
            Some(
                serde_json::to_string(&SummaryPayload {
                    summary: result.summary.clone(),
                    content: Some(persisted_content.clone()),
                    outputs: result.outputs.clone(),
                })
                .unwrap_or_else(|_| result.summary.clone()),
            ),
            Some(persisted_content.clone()),
        )
        .await?;
        let _ = Self::write_transcript(
            pool,
            execution.id,
            Some(recorded_step.round_id),
            Some(workflow_session.id),
            Some(recorded_step.id),
            "agent",
            "message",
            &result.content,
            Some(
                &serde_json::json!({
                    "summary": result.summary.clone(),
                    "outputs": result.outputs.clone(),
                    "source": "workflow_protocol_final_result",
                    "structured_result": result
                        .structured_report
                        .as_deref()
                        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok()),
                })
                .to_string(),
            ),
        )
        .await;

        Ok(PersistedWorkerAttempt {
            step: recorded_step,
            result,
        })
    }

    async fn wait_for_step_user_review_stub(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        workflow_session: &WorkflowAgentSession,
        result: &WorkflowStepRunResult,
    ) -> Result<StepUserReviewResolution, OrchestratorError> {
        Self::emit_step_domain_event(
            pool,
            execution,
            step,
            WorkflowEventType::StepUserReviewStarted,
            Some(serde_json::json!({
                "step_key": step.step_key,
                "summary": result.summary,
            })),
        )
        .await?;

        Self::park_for_user_action(
            pool,
            chat_runner,
            execution,
            step,
            workflow_session,
            "step_review",
            &format!("请审核步骤「{}」的执行结果", step.title),
            Some(result.summary.clone()),
            WorkflowStepStatus::WaitingInput,
            WorkflowAgentSessionState::Paused,
            Some(serde_json::json!({
                "summary": result.summary,
                "outputs": result.outputs,
                "review_kind": "step_user_review",
                "structured_result": result
                    .structured_report
                    .as_deref()
                    .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok()),
            })),
        )
        .await?;

        Ok(StepUserReviewResolution::Parked)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_step_with_feedback(
        db: &DBService,
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        workflow_session: &WorkflowAgentSession,
        session: &ChatSession,
        session_agent: &ChatSessionAgent,
        agent: &ChatAgent,
        workflow_agent_sessions: &[WorkflowAgentSession],
        session_agents: &[ChatSessionAgent],
        agents: &[ChatAgent],
        plan: &WorkflowPlan,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
        initial_result: WorkflowStepRunResult,
        skip_initial_lead_review: bool,
    ) -> Result<StepOutcome, OrchestratorError> {
        let acceptance_criteria = workflow_runtime::build_workflow_review_criteria(
            &Self::acceptance_criteria_for_step(plan, step),
            None,
        );
        let workflow_goal = plan
            .summary_text
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| plan.title.clone());
        let ui_config = config::load_config_from_file(&config_path()).await;
        let response_language_instruction =
            resolve_workflow_response_language_instruction(&ui_config.language);
        let (lead_workflow_session, lead_session_agent, lead_agent) =
            Self::resolve_lead_review_targets(
                execution,
                workflow_agent_sessions,
                session_agents,
                agents,
            )?;

        let mut active_step = step.clone();
        let mut current_result = initial_result;
        let mut skip_lead_review_for_current_attempt = skip_initial_lead_review;

        loop {
            let persisted = Self::persist_worker_attempt_result(
                pool,
                execution,
                &active_step,
                workflow_session,
                current_result.clone(),
            )
            .await?;
            let waiting_review_step = Self::transition_step_and_sync(
                pool,
                chat_runner,
                execution,
                &persisted.step,
                WorkflowStepStatus::WaitingReview,
                "step_waiting_review",
            )
            .await?;

            let skip_lead_review_this_attempt =
                std::mem::take(&mut skip_lead_review_for_current_attempt);
            let should_run_lead_review =
                waiting_review_step.lead_review_required && !skip_lead_review_this_attempt;

            if !should_run_lead_review {
                if waiting_review_step.user_review_required {
                    match Self::wait_for_step_user_review_stub(
                        pool,
                        chat_runner,
                        execution,
                        &waiting_review_step,
                        workflow_session,
                        &persisted.result,
                    )
                    .await?
                    {
                        StepUserReviewResolution::Parked => return Ok(StepOutcome::Parked),
                        StepUserReviewResolution::Approved { .. }
                        | StepUserReviewResolution::Rejected { .. } => {
                            return Err(OrchestratorError::IllegalTransition(
                                "step user review resolved synchronously".to_string(),
                            ));
                        }
                    }
                }

                Self::transition_step_and_sync(
                    pool,
                    chat_runner,
                    execution,
                    &waiting_review_step,
                    WorkflowStepStatus::Completed,
                    "step_completed",
                )
                .await?;
                return Ok(StepOutcome::Completed);
            }

            let review_attempt =
                Self::next_lead_review_attempt(pool, waiting_review_step.id).await?;
            let max_review_attempts = waiting_review_step.max_retry.saturating_add(1).max(1);
            if review_attempt > max_review_attempts {
                let reason = format!(
                    "Step \"{}\" cannot be reviewed again: the maximum of {} review attempts has been reached.",
                    waiting_review_step.title, max_review_attempts
                );
                let failed_step = Self::transition_step_and_sync(
                    pool,
                    chat_runner,
                    execution,
                    &waiting_review_step,
                    WorkflowStepStatus::Failed,
                    "step_review_limit_reached",
                )
                .await?;
                let _ = Self::write_transcript(
                    pool,
                    execution.id,
                    Some(failed_step.round_id),
                    Some(lead_workflow_session.id),
                    Some(failed_step.id),
                    "system",
                    "message",
                    &reason,
                    Some(
                        &serde_json::json!({
                            "reason": "review_limit_reached",
                            "max_retry": waiting_review_step.max_retry,
                            "max_review_attempts": max_review_attempts,
                        })
                        .to_string(),
                    ),
                )
                .await;
                return Ok(StepOutcome::Failed(reason));
            }

            Self::emit_step_domain_event(
                pool,
                execution,
                &waiting_review_step,
                WorkflowEventType::StepLeadReviewStarted,
                Some(serde_json::json!({
                    "step_key": waiting_review_step.step_key,
                    "summary": persisted.result.summary,
                    "review_round": review_attempt,
                    "max_retry": waiting_review_step.max_retry,
                    "max_review_attempts": max_review_attempts,
                })),
            )
            .await?;

            let latest_review_feedback = if review_attempt > 1 {
                WorkflowStepReview::find_by_step(pool, waiting_review_step.id)
                    .await?
                    .into_iter()
                    .max_by_key(|review| review.review_round)
                    .map(|review| review.feedback)
            } else {
                None
            };
            let review_prompt = prompt_builders::step_review::build_step_review_prompt(
                &prompt_builders::step_review::StepReviewPromptInput {
                    identity: prompt_builders::common::PromptIdentity {
                        execution_id: execution.id,
                        step_key: waiting_review_step.step_key.clone(),
                    },
                    workflow_goal: workflow_goal.clone(),
                    title: waiting_review_step.title.clone(),
                    instructions: waiting_review_step.instructions.clone(),
                    acceptance_criteria: acceptance_criteria.clone(),
                    review_rules: Vec::new(),
                    worker_result: Self::task_result_input_from_run_result(&persisted.result),
                    upstream_results: Self::upstream_results_for_step(
                        &waiting_review_step,
                        current_steps,
                        edges,
                    )?,
                    latest_review_feedback,
                    response_language: response_language_instruction.to_string(),
                },
            );

            let (review_message, _raw_review_output) =
                match Self::run_step_review_protocol_with_retry(
                    db,
                    pool,
                    chat_runner,
                    execution,
                    session,
                    lead_agent,
                    lead_session_agent,
                    lead_workflow_session,
                    &review_prompt,
                    &waiting_review_step,
                    &acceptance_criteria,
                )
                .await
                {
                    Ok(raw_output) => raw_output,
                    Err(OrchestratorError::Runtime(WorkflowRuntimeError::Interrupted(reason))) => {
                        let interrupted_step = Self::acknowledge_step_interrupted(
                            pool,
                            chat_runner,
                            execution,
                            waiting_review_step.id,
                            "step_interrupted",
                        )
                        .await?;
                        let _ = Self::write_transcript(
                            pool,
                            execution.id,
                            Some(interrupted_step.round_id),
                            Some(lead_workflow_session.id),
                            Some(interrupted_step.id),
                            "system",
                            "message",
                            &format!(
                                "Lead review interrupted for step \"{}\": {}",
                                interrupted_step.title, reason
                            ),
                            None,
                        )
                        .await;
                        return Ok(StepOutcome::Interrupted);
                    }
                    Err(err) => {
                        let failed_step = Self::transition_step_and_sync(
                            pool,
                            chat_runner,
                            execution,
                            &waiting_review_step,
                            WorkflowStepStatus::Failed,
                            "step_failed",
                        )
                        .await?;
                        let _ = Self::write_transcript(
                            pool,
                            execution.id,
                            Some(failed_step.round_id),
                            Some(lead_workflow_session.id),
                            Some(failed_step.id),
                            "system",
                            "message",
                            &format!(
                                "Lead review failed for step \"{}\": {}",
                                failed_step.title, err
                            ),
                            None,
                        )
                        .await;
                        return Ok(StepOutcome::Failed(err.to_string()));
                    }
                };

            let WorkflowReviewProtocolMessage::ReviewResult {
                summary: feedback,
                results,
                ..
            } = review_message;
            let derived_review =
                workflow_runtime::derive_workflow_review(&acceptance_criteria, &results);
            let verdict = derived_review.verdict;
            let acceptance_results = derived_review.acceptance_results;
            let evidence = derived_review.evidence;
            let risks = derived_review.risks;
            let unfinished_items = derived_review.unfinished_items;

            Self::save_step_review(
                pool,
                &waiting_review_step,
                ReviewerType::Lead,
                Some(lead_session_agent.id.to_string()),
                verdict.clone(),
                &feedback,
            )
            .await?;
            let _ = Self::write_transcript(
                pool,
                execution.id,
                Some(waiting_review_step.round_id),
                Some(lead_workflow_session.id),
                Some(waiting_review_step.id),
                "agent",
                "lead_review",
                &feedback,
                Some(
                    &serde_json::json!({
                        "verdict": verdict,
                        "reviewer_type": "lead",
                        "review_round": review_attempt,
                        "max_retry": waiting_review_step.max_retry,
                        "max_review_attempts": max_review_attempts,
                        "acceptance_results": acceptance_results,
                        "evidence": evidence,
                        "risks": risks,
                        "unfinished_items": unfinished_items,
                    })
                    .to_string(),
                ),
            )
            .await;

            match verdict {
                ReviewVerdict::Approved => {
                    Self::emit_step_domain_event(
                        pool,
                        execution,
                        &waiting_review_step,
                        WorkflowEventType::StepLeadReviewPassed,
                        Some(serde_json::json!({
                            "feedback": feedback,
                            "review_round": review_attempt,
                            "max_retry": waiting_review_step.max_retry,
                            "max_review_attempts": max_review_attempts,
                        })),
                    )
                    .await?;

                    if waiting_review_step.user_review_required {
                        match Self::wait_for_step_user_review_stub(
                            pool,
                            chat_runner,
                            execution,
                            &waiting_review_step,
                            workflow_session,
                            &persisted.result,
                        )
                        .await?
                        {
                            StepUserReviewResolution::Parked => return Ok(StepOutcome::Parked),
                            StepUserReviewResolution::Approved { feedback } => {
                                Self::save_step_review(
                                    pool,
                                    &waiting_review_step,
                                    ReviewerType::User,
                                    None,
                                    ReviewVerdict::Approved,
                                    &feedback,
                                )
                                .await?;
                                Self::emit_step_domain_event(
                                    pool,
                                    execution,
                                    &waiting_review_step,
                                    WorkflowEventType::StepUserReviewPassed,
                                    Some(serde_json::json!({ "feedback": feedback })),
                                )
                                .await?;
                            }
                            StepUserReviewResolution::Rejected { feedback } => {
                                Self::save_step_review(
                                    pool,
                                    &waiting_review_step,
                                    ReviewerType::User,
                                    None,
                                    ReviewVerdict::Rejected,
                                    &feedback,
                                )
                                .await?;
                                Self::emit_step_domain_event(
                                    pool,
                                    execution,
                                    &waiting_review_step,
                                    WorkflowEventType::StepUserReviewRejected,
                                    Some(serde_json::json!({ "feedback": feedback })),
                                )
                                .await?;

                                let revising_step = Self::transition_step_and_sync(
                                    pool,
                                    chat_runner,
                                    execution,
                                    &waiting_review_step,
                                    WorkflowStepStatus::Revising,
                                    "step_revising",
                                )
                                .await?;
                                let merged_context = Self::merge_revision_context(
                                    revising_step.revision_context.as_deref(),
                                    WorkflowRevisionFeedbackSource::User,
                                    &feedback,
                                    &persisted.result.summary,
                                    Some(&persisted.result.content),
                                    &persisted.result.outputs,
                                    revising_step.retry_count + 1,
                                    None,
                                );
                                let revising_step = WorkflowStep::update_revision_context(
                                    pool,
                                    revising_step.id,
                                    Some(merged_context),
                                )
                                .await?;

                                let revised_step =
                                    WorkflowStep::prepare_retry(pool, revising_step.id).await?;
                                let running_revision_step = Self::transition_step_and_sync(
                                    pool,
                                    chat_runner,
                                    execution,
                                    &revised_step,
                                    WorkflowStepStatus::Running,
                                    "step_revising_running",
                                )
                                .await?;
                                let mut sa_clone = session_agent.clone();
                                let agent_skill_names: Vec<String> = chat_runner
                                    .prepare_and_resolve_agent_skills(
                                        &mut sa_clone,
                                        agent,
                                        AgentPromptContext::StepRevision,
                                    )
                                    .await
                                    .unwrap_or_default()
                                    .iter()
                                    .map(|s| s.name.clone())
                                    .collect();
                                let mut revision_prompt = Self::build_task_revision_prompt(
                                    plan,
                                    &running_revision_step,
                                    &workflow_goal,
                                    current_steps,
                                    edges,
                                    WorkflowRevisionFeedbackSource::User,
                                    &feedback,
                                    &persisted.result.summary,
                                    &persisted.result.outputs,
                                    None,
                                    response_language_instruction,
                                )?;
                                if let Some(section) =
                                    crate::services::agent_skill_policy::format_skills_prompt_section(
                                        &agent_skill_names,
                                    )
                                {
                                    inject_step_prompt_section_before_schema(
                                        &mut revision_prompt,
                                        &section,
                                    );
                                }

                                let (protocol_message, agent_output) =
                                    match Self::run_step_agent_protocol_with_retry(
                                        db,
                                        pool,
                                        chat_runner,
                                        session,
                                        agent,
                                        session_agent,
                                        workflow_session,
                                        &revision_prompt,
                                        &running_revision_step,
                                        true,
                                    )
                                    .await
                                    {
                                        Ok(result) => result,
                                        Err(err) => {
                                            let failed_step = Self::transition_step_and_sync(
                                                pool,
                                                chat_runner,
                                                execution,
                                                &running_revision_step,
                                                WorkflowStepStatus::Failed,
                                                "step_failed",
                                            )
                                            .await?;
                                            let _ = Self::write_transcript(
                                                pool,
                                                execution.id,
                                                Some(failed_step.round_id),
                                                Some(workflow_session.id),
                                                Some(failed_step.id),
                                                "system",
                                                "message",
                                                &format!(
                                                    "Step \"{}\" failed during user revision: {}",
                                                    failed_step.title, err
                                                ),
                                                None,
                                            )
                                            .await;
                                            return Ok(StepOutcome::Failed(err.to_string()));
                                        }
                                    };

                                match protocol_message {
                                    WorkflowStepProtocolMessage::FinalResult {
                                        status:
                                            status @ (workflow_runtime::WorkflowTaskCompletionStatus::Done
                                            | workflow_runtime::WorkflowTaskCompletionStatus::DoneWithConcerns),
                                        summary,
                                        content,
                                        verification,
                                        files_changed,
                                        self_review,
                                        issues,
                                        evidence,
                                        outputs,
                                        ..
                                    } => {
                                        active_step = running_revision_step;
                                        current_result = workflow_step_run_result_from_task_report(
                                            agent_output.run_id.unwrap_or_else(Uuid::new_v4),
                                            status,
                                            summary,
                                            content,
                                            verification,
                                            files_changed,
                                            self_review,
                                            issues,
                                            evidence,
                                            outputs,
                                        );
                                        skip_lead_review_for_current_attempt = true;
                                        continue;
                                    }
                                    other => {
                                        return Self::handle_step_protocol_message(
                                            pool,
                                            chat_runner,
                                            execution,
                                            &running_revision_step,
                                            workflow_session,
                                            Some(agent.id.to_string()),
                                            other,
                                            agent_output.run_id,
                                        )
                                        .await;
                                    }
                                }
                            }
                        }
                    }

                    Self::transition_step_and_sync(
                        pool,
                        chat_runner,
                        execution,
                        &waiting_review_step,
                        WorkflowStepStatus::Completed,
                        "step_completed",
                    )
                    .await?;
                    return Ok(StepOutcome::Completed);
                }
                ReviewVerdict::Rejected => {
                    Self::emit_step_domain_event(
                        pool,
                        execution,
                        &waiting_review_step,
                        WorkflowEventType::StepLeadReviewRejected,
                        Some(serde_json::json!({
                            "feedback": feedback,
                            "review_round": review_attempt,
                            "max_retry": waiting_review_step.max_retry,
                            "max_review_attempts": max_review_attempts,
                        })),
                    )
                    .await?;

                    if workflow_review_attempt_limit_reached(review_attempt, max_review_attempts) {
                        let reason = format!(
                            "Step \"{}\" was rejected on the final allowed review attempt ({}/{}): {}",
                            waiting_review_step.title,
                            review_attempt,
                            max_review_attempts,
                            feedback
                        );
                        let failed_step = Self::transition_step_and_sync(
                            pool,
                            chat_runner,
                            execution,
                            &waiting_review_step,
                            WorkflowStepStatus::Failed,
                            "step_review_limit_reached",
                        )
                        .await?;
                        let _ = Self::write_transcript(
                            pool,
                            execution.id,
                            Some(failed_step.round_id),
                            Some(lead_workflow_session.id),
                            Some(failed_step.id),
                            "system",
                            "message",
                            &reason,
                            Some(
                                &serde_json::json!({
                                    "reason": "review_limit_reached",
                                    "review_attempt": review_attempt,
                                    "max_retry": waiting_review_step.max_retry,
                                    "max_review_attempts": max_review_attempts,
                                })
                                .to_string(),
                            ),
                        )
                        .await;
                        return Ok(StepOutcome::Failed(reason));
                    }

                    let revising_step = Self::transition_step_and_sync(
                        pool,
                        chat_runner,
                        execution,
                        &waiting_review_step,
                        WorkflowStepStatus::Revising,
                        "step_revising",
                    )
                    .await?;
                    let review_details = RevisionReviewDetails {
                        acceptance_results: acceptance_results.clone(),
                        evidence: evidence.clone(),
                        risks: risks.clone(),
                        unfinished_items: unfinished_items.clone(),
                    };
                    let merged_context = Self::merge_revision_context(
                        revising_step.revision_context.as_deref(),
                        WorkflowRevisionFeedbackSource::Lead,
                        &feedback,
                        &persisted.result.summary,
                        Some(&persisted.result.content),
                        &persisted.result.outputs,
                        revising_step.retry_count + 1,
                        Some(&review_details),
                    );
                    let revising_step = WorkflowStep::update_revision_context(
                        pool,
                        revising_step.id,
                        Some(merged_context),
                    )
                    .await?;

                    let revised_step = WorkflowStep::prepare_retry(pool, revising_step.id).await?;
                    let running_revision_step = Self::transition_step_and_sync(
                        pool,
                        chat_runner,
                        execution,
                        &revised_step,
                        WorkflowStepStatus::Running,
                        "step_revising_running",
                    )
                    .await?;
                    let mut sa_clone = session_agent.clone();
                    let agent_skill_names: Vec<String> = chat_runner
                        .prepare_and_resolve_agent_skills(
                            &mut sa_clone,
                            agent,
                            AgentPromptContext::StepRevision,
                        )
                        .await
                        .unwrap_or_default()
                        .iter()
                        .map(|s| s.name.clone())
                        .collect();
                    let mut revision_prompt = Self::build_task_revision_prompt(
                        plan,
                        &running_revision_step,
                        &workflow_goal,
                        current_steps,
                        edges,
                        WorkflowRevisionFeedbackSource::Lead,
                        &feedback,
                        &persisted.result.summary,
                        &persisted.result.outputs,
                        Some(&review_details),
                        response_language_instruction,
                    )?;
                    if let Some(section) =
                        crate::services::agent_skill_policy::format_skills_prompt_section(
                            &agent_skill_names,
                        )
                    {
                        inject_step_prompt_section_before_schema(&mut revision_prompt, &section);
                    }

                    let (protocol_message, agent_output) =
                        match Self::run_step_agent_protocol_with_retry(
                            db,
                            pool,
                            chat_runner,
                            session,
                            agent,
                            session_agent,
                            workflow_session,
                            &revision_prompt,
                            &running_revision_step,
                            true,
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(err) => {
                                let failed_step = Self::transition_step_and_sync(
                                    pool,
                                    chat_runner,
                                    execution,
                                    &running_revision_step,
                                    WorkflowStepStatus::Failed,
                                    "step_failed",
                                )
                                .await?;
                                let _ = Self::write_transcript(
                                    pool,
                                    execution.id,
                                    Some(failed_step.round_id),
                                    Some(workflow_session.id),
                                    Some(failed_step.id),
                                    "system",
                                    "message",
                                    &format!(
                                        "Step \"{}\" failed during revision: {}",
                                        failed_step.title, err
                                    ),
                                    None,
                                )
                                .await;
                                return Ok(StepOutcome::Failed(err.to_string()));
                            }
                        };

                    match protocol_message {
                        WorkflowStepProtocolMessage::FinalResult {
                            status:
                                status @ (workflow_runtime::WorkflowTaskCompletionStatus::Done
                                | workflow_runtime::WorkflowTaskCompletionStatus::DoneWithConcerns),
                            summary,
                            content,
                            verification,
                            files_changed,
                            self_review,
                            issues,
                            evidence,
                            outputs,
                            ..
                        } => {
                            active_step = running_revision_step;
                            current_result = workflow_step_run_result_from_task_report(
                                agent_output.run_id.unwrap_or_else(Uuid::new_v4),
                                status,
                                summary,
                                content,
                                verification,
                                files_changed,
                                self_review,
                                issues,
                                evidence,
                                outputs,
                            );
                            skip_lead_review_for_current_attempt = false;
                        }
                        other => {
                            return Self::handle_step_protocol_message(
                                pool,
                                chat_runner,
                                execution,
                                &running_revision_step,
                                workflow_session,
                                Some(agent.id.to_string()),
                                other,
                                agent_output.run_id,
                            )
                            .await;
                        }
                    }
                }
            }
        }
    }

    async fn handle_step_review_protocol_message(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        running_step: &WorkflowStep,
        workflow_session: &WorkflowAgentSession,
        review_message: WorkflowReviewProtocolMessage,
        review_criteria: &[workflow_runtime::WorkflowReviewCriterion],
        run_id: Option<Uuid>,
    ) -> Result<StepOutcome, OrchestratorError> {
        let WorkflowReviewProtocolMessage::ReviewResult {
            summary: feedback,
            results,
            ..
        } = review_message;
        let derived_review = workflow_runtime::derive_workflow_review(review_criteria, &results);
        let verdict = derived_review.verdict;
        let acceptance_results = derived_review.acceptance_results;
        let evidence = derived_review.evidence;
        let risks = derived_review.risks;
        let unfinished_items = derived_review.unfinished_items;
        if running_step.step_type != WorkflowStepType::Review {
            return Err(OrchestratorError::IllegalTransition(format!(
                "step {} returned review_result but is not a Review node",
                running_step.step_key
            )));
        }

        let reviewer_type = if workflow_session.role == WorkflowAgentSessionRole::Lead {
            ReviewerType::Lead
        } else {
            ReviewerType::Reviewer
        };
        let structured_content = serde_json::json!({
            "type": "review_result",
            "step_key": running_step.step_key,
            "execution_id": execution.id,
            "verdict": verdict,
            "feedback": feedback,
            "acceptance_results": acceptance_results,
            "evidence": evidence,
            "risks": risks,
            "unfinished_items": unfinished_items,
        })
        .to_string();
        let recorded_step = WorkflowStep::record_execution_result(
            pool,
            running_step.id,
            run_id.unwrap_or_else(Uuid::new_v4),
            Some(
                serde_json::to_string(&SummaryPayload {
                    summary: feedback.clone(),
                    content: Some(structured_content.clone()),
                    outputs: Vec::new(),
                })
                .unwrap_or_else(|_| feedback.clone()),
            ),
            Some(structured_content.clone()),
        )
        .await?;
        let persisted_review = Self::save_step_review(
            pool,
            &recorded_step,
            reviewer_type.clone(),
            Some(workflow_session.session_agent_id.to_string()),
            verdict.clone(),
            &feedback,
        )
        .await?;
        let _ = Self::write_transcript(
            pool,
            execution.id,
            recorded_step.round_id.into(),
            Some(workflow_session.id),
            Some(recorded_step.id),
            "agent",
            "review",
            &feedback,
            Some(
                &serde_json::json!({
                    "source": "workflow_structured_review_result",
                    "reviewer_type": to_workflow_wire_value(&reviewer_type),
                    "reviewer_id": workflow_session.session_agent_id,
                    "review_round": persisted_review.review_round,
                    "verdict": verdict,
                    "structured_result": serde_json::from_str::<serde_json::Value>(&structured_content)
                        .unwrap_or(serde_json::Value::Null),
                })
                .to_string(),
            ),
        )
        .await;
        let approved = verdict == ReviewVerdict::Approved;
        Self::transition_step_and_sync(
            pool,
            chat_runner,
            execution,
            &recorded_step,
            if approved {
                WorkflowStepStatus::Completed
            } else {
                WorkflowStepStatus::Failed
            },
            if approved {
                "review_step_completed"
            } else {
                "review_step_rejected"
            },
        )
        .await?;
        if approved {
            Ok(StepOutcome::Completed)
        } else {
            Ok(StepOutcome::Failed(feedback))
        }
    }

    pub(super) async fn handle_step_protocol_message(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        running_step: &WorkflowStep,
        workflow_session: &WorkflowAgentSession,
        _reviewer_id: Option<String>,
        protocol_message: WorkflowStepProtocolMessage,
        run_id_hint: Option<Uuid>,
    ) -> Result<StepOutcome, OrchestratorError> {
        match protocol_message {
            WorkflowStepProtocolMessage::ApprovalRequest {
                title, description, ..
            } => {
                Self::park_for_user_action(
                    pool,
                    chat_runner,
                    execution,
                    running_step,
                    workflow_session,
                    "approval_request",
                    &title,
                    description,
                    WorkflowStepStatus::WaitingReview,
                    WorkflowAgentSessionState::Paused,
                    None,
                )
                .await?;
                Ok(StepOutcome::Parked)
            }
            WorkflowStepProtocolMessage::PermissionRequest {
                title, description, ..
            } => {
                Self::park_for_user_action(
                    pool,
                    chat_runner,
                    execution,
                    running_step,
                    workflow_session,
                    "permission_request",
                    &title,
                    description,
                    WorkflowStepStatus::WaitingReview,
                    WorkflowAgentSessionState::Paused,
                    None,
                )
                .await?;
                Ok(StepOutcome::Parked)
            }
            WorkflowStepProtocolMessage::ContinueConfirmation {
                message,
                description,
                ..
            } => {
                Self::park_for_user_action(
                    pool,
                    chat_runner,
                    execution,
                    running_step,
                    workflow_session,
                    "continue_confirmation",
                    &message,
                    description,
                    WorkflowStepStatus::WaitingInput,
                    WorkflowAgentSessionState::Paused,
                    None,
                )
                .await?;
                Ok(StepOutcome::Parked)
            }
            WorkflowStepProtocolMessage::InputRequest {
                prompt,
                description,
                placeholder,
                ..
            } => {
                Self::park_for_user_action(
                    pool,
                    chat_runner,
                    execution,
                    running_step,
                    workflow_session,
                    "input_request",
                    &prompt,
                    description,
                    WorkflowStepStatus::WaitingInput,
                    WorkflowAgentSessionState::Paused,
                    Some(serde_json::json!({
                        "placeholder": placeholder,
                    })),
                )
                .await?;
                Ok(StepOutcome::Parked)
            }
            WorkflowStepProtocolMessage::Error {
                message, content, ..
            } => {
                let error_message = message.trim().to_string();
                let error_detail = content
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let err = Self::step_message_error(error_message.clone(), error_detail.clone());
                let failed_step = WorkflowStep::record_execution_result(
                    pool,
                    running_step.id,
                    Uuid::new_v4(),
                    Some(
                        serde_json::to_string(&SummaryPayload {
                            summary: err.to_string(),
                            content: None,
                            outputs: vec![],
                        })
                        .unwrap_or_else(|_| err.to_string()),
                    ),
                    None,
                )
                .await?;
                let error_meta = serde_json::json!({
                    "description": error_detail,
                    "source": "workflow_protocol_error",
                })
                .to_string();
                let _ = Self::write_transcript(
                    pool,
                    execution.id,
                    failed_step.round_id.into(),
                    Some(workflow_session.id),
                    Some(failed_step.id),
                    "control",
                    "error",
                    &error_message,
                    Some(&error_meta),
                )
                .await;
                Self::transition_step_and_sync(
                    pool,
                    chat_runner,
                    execution,
                    &failed_step,
                    WorkflowStepStatus::Failed,
                    "step_failed",
                )
                .await?;
                Ok(StepOutcome::Failed(err.to_string()))
            }
            WorkflowStepProtocolMessage::FinalResult {
                status,
                summary,
                content,
                verification,
                files_changed,
                self_review,
                issues,
                evidence,
                outputs,
                ..
            } => {
                if matches!(
                    running_step.step_type,
                    WorkflowStepType::Review | WorkflowStepType::Result
                ) {
                    return Err(OrchestratorError::Runtime(
                        WorkflowRuntimeError::Validation(format!(
                            "{:?} node must return its structured review protocol, not final_result",
                            running_step.step_type
                        )),
                    ));
                }
                let execution_result = workflow_step_run_result_from_task_report(
                    run_id_hint.unwrap_or_else(Uuid::new_v4),
                    status,
                    summary.clone(),
                    content,
                    verification,
                    files_changed,
                    self_review,
                    issues.clone(),
                    evidence,
                    outputs,
                );
                let structured_content = execution_result
                    .structured_report
                    .clone()
                    .expect("task report helper always returns structured content");
                let recorded_step = WorkflowStep::record_execution_result(
                    pool,
                    running_step.id,
                    execution_result.run_id,
                    Some(
                        serde_json::to_string(&SummaryPayload {
                            summary: execution_result.summary.clone(),
                            content: Some(structured_content.clone()),
                            outputs: execution_result.outputs.clone(),
                        })
                        .unwrap_or_else(|_| execution_result.summary.clone()),
                    ),
                    Some(structured_content.clone()),
                )
                .await?;
                let _ = Self::write_transcript(
                    pool,
                    execution.id,
                    recorded_step.round_id.into(),
                    Some(workflow_session.id),
                    Some(recorded_step.id),
                    "agent",
                    "message",
                    &execution_result.content,
                    Some(
                        &serde_json::json!({
                            "summary": execution_result.summary.clone(),
                            "outputs": execution_result.outputs.clone(),
                            "source": "workflow_protocol_final_result",
                            "status": status,
                            "structured_result": serde_json::from_str::<serde_json::Value>(&structured_content)
                                .unwrap_or(serde_json::Value::Null),
                        })
                        .to_string(),
                    ),
                )
                .await;
                match status {
                    workflow_runtime::WorkflowTaskCompletionStatus::Done
                    | workflow_runtime::WorkflowTaskCompletionStatus::DoneWithConcerns => {
                        Self::transition_step_and_sync(
                            pool,
                            chat_runner,
                            execution,
                            &recorded_step,
                            WorkflowStepStatus::Completed,
                            "step_completed",
                        )
                        .await?;
                        Ok(StepOutcome::Completed)
                    }
                    workflow_runtime::WorkflowTaskCompletionStatus::Blocked => {
                        let reason = issues.join("; ");
                        Self::transition_step_and_sync(
                            pool,
                            chat_runner,
                            execution,
                            &recorded_step,
                            WorkflowStepStatus::Failed,
                            "step_blocked",
                        )
                        .await?;
                        Ok(StepOutcome::Failed(reason))
                    }
                    workflow_runtime::WorkflowTaskCompletionStatus::NeedsContext => {
                        let prompt = issues.join("; ");
                        Self::park_for_user_action(
                            pool,
                            chat_runner,
                            execution,
                            &recorded_step,
                            workflow_session,
                            "task_needs_context",
                            &prompt,
                            Some(summary),
                            WorkflowStepStatus::WaitingInput,
                            WorkflowAgentSessionState::Paused,
                            Some(serde_json::json!({"structured_result": structured_content})),
                        )
                        .await?;
                        Ok(StepOutcome::Parked)
                    }
                }
            }
        }
    }

    fn active_frontier_workspace_isolation_prompt(
        session: &ChatSession,
        running_step: &WorkflowStep,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
        workflow_agent_sessions: &[WorkflowAgentSession],
        current_session_agent: &ChatSessionAgent,
        session_agents: &[ChatSessionAgent],
        agents: &[ChatAgent],
    ) -> Option<String> {
        let conflicts = detect_active_frontier_workspace_conflicts(
            session,
            running_step,
            current_steps,
            edges,
            workflow_agent_sessions,
            session_agents,
            agents,
        );
        let conflict = conflicts.iter().find(|conflict| {
            conflict
                .members
                .iter()
                .any(|member| member.session_agent_id == current_session_agent.id)
        })?;

        Some(format!(
            "\n\n{}",
            render_workspace_isolation_section(conflict)
        ))
    }

    /// Execute a single step: resolve context, transition to Running, run agent
    /// prompt, process the result.
    pub(crate) async fn prepare_and_run_step(
        db: &DBService,
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        workflow_agent_sessions: &[WorkflowAgentSession],
        session: &ChatSession,
        session_agents: &[ChatSessionAgent],
        agents: &[ChatAgent],
        plan: &WorkflowPlan,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
    ) -> Result<StepOutcome, OrchestratorError> {
        let result = Self::prepare_and_run_step_inner(
            db,
            pool,
            chat_runner,
            execution,
            step,
            workflow_agent_sessions,
            session,
            session_agents,
            agents,
            plan,
            current_steps,
            edges,
        )
        .await;

        let Err(error) = result else {
            return result;
        };

        let current_step = WorkflowStep::find_by_id(pool, step.id)
            .await?
            .ok_or_else(|| OrchestratorError::NotFound(format!("step {} 未找到", step.id)))?;
        let interrupted_reason = match &error {
            OrchestratorError::Runtime(WorkflowRuntimeError::Interrupted(reason)) => {
                Some(reason.clone())
            }
            _ => None,
        };

        if current_step.status == WorkflowStepStatus::InterruptRequested {
            let interrupted_step = Self::acknowledge_step_interrupted(
                pool,
                chat_runner,
                execution,
                current_step.id,
                "step_interrupted_after_work_item_error",
            )
            .await?;
            tracing::info!(
                execution_id = %execution.id,
                step_id = %interrupted_step.id,
                error = %error,
                "workflow work-item guard settled requested interrupt"
            );
            return Ok(StepOutcome::Interrupted);
        }
        if current_step.status == WorkflowStepStatus::Interrupted {
            return Ok(StepOutcome::Interrupted);
        }
        if let Some(reason) = interrupted_reason
            && current_step.status == WorkflowStepStatus::Running
        {
            let requested_step = Self::transition_step_and_sync(
                pool,
                chat_runner,
                execution,
                &current_step,
                WorkflowStepStatus::InterruptRequested,
                "step_interrupt_recovered_by_work_item_guard",
            )
            .await?;
            let interrupted_step = Self::transition_step_and_sync(
                pool,
                chat_runner,
                execution,
                &requested_step,
                WorkflowStepStatus::Interrupted,
                "step_interrupted_by_work_item_guard",
            )
            .await?;
            let _ = Self::write_transcript(
                pool,
                execution.id,
                Some(interrupted_step.round_id),
                interrupted_step.assigned_workflow_agent_session_id,
                Some(interrupted_step.id),
                "system",
                "message",
                &format!("Step \"{}\" interrupted: {reason}", interrupted_step.title),
                None,
            )
            .await;
            return Ok(StepOutcome::Interrupted);
        }
        if matches!(
            current_step.status,
            WorkflowStepStatus::Running | WorkflowStepStatus::Revising
        ) {
            let reason = error.to_string();
            let failed_step = Self::transition_step_and_sync(
                pool,
                chat_runner,
                execution,
                &current_step,
                WorkflowStepStatus::Failed,
                "step_failed_by_work_item_guard",
            )
            .await?;
            let _ = WorkflowStep::record_execution_result(
                pool,
                failed_step.id,
                Uuid::new_v4(),
                Some(
                    serde_json::to_string(&SummaryPayload {
                        summary: reason.clone(),
                        content: None,
                        outputs: Vec::new(),
                    })
                    .unwrap_or_else(|_| reason.clone()),
                ),
                None,
            )
            .await;
            let _ = Self::write_transcript(
                pool,
                execution.id,
                Some(failed_step.round_id),
                failed_step.assigned_workflow_agent_session_id,
                Some(failed_step.id),
                "system",
                "message",
                &format!("Step \"{}\" failed: {reason}", failed_step.title),
                None,
            )
            .await;
            tracing::error!(
                execution_id = %execution.id,
                step_id = %failed_step.id,
                error = %error,
                "workflow work-item guard converted an unhandled error into a terminal step state"
            );
            return Ok(StepOutcome::Failed(reason));
        }

        Err(error)
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_and_run_step_inner(
        db: &DBService,
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        workflow_agent_sessions: &[WorkflowAgentSession],
        session: &ChatSession,
        session_agents: &[ChatSessionAgent],
        agents: &[ChatAgent],
        plan: &WorkflowPlan,
        current_steps: &[WorkflowStep],
        edges: &[WorkflowStepEdge],
    ) -> Result<StepOutcome, OrchestratorError> {
        let workflow_session =
            resolve_step_workflow_session(execution, workflow_agent_sessions, step)?;
        let session_agent = session_agents
            .iter()
            .find(|item| item.id == workflow_session.session_agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "session agent {} 未找到",
                    workflow_session.session_agent_id
                ))
            })?;
        let agent = agents
            .iter()
            .find(|item| item.id == session_agent.agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("agent {} 未找到", session_agent.agent_id))
            })?;

        let running_step = match Self::guarded_transition_step_and_sync(
            pool,
            chat_runner,
            execution,
            step,
            WorkflowStepStatus::Running,
            "step_started",
        )
        .await?
        {
            Some(s) => s,
            None => {
                return Ok(StepOutcome::Completed);
            }
        };

        // result 节点不调用模型：确定性聚合全部传递前驱的最新有效结果（设计 §9.4、§10.3）。
        if running_step.step_type == WorkflowStepType::Result {
            return Self::run_result_step_deterministic(
                pool,
                chat_runner,
                execution,
                &workflow_session,
                plan,
                running_step,
                current_steps,
                edges,
            )
            .await;
        }

        let workflow_goal = plan
            .summary_text
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| plan.title.clone());
        let pending_revision_feedback =
            Self::parse_pending_revision_feedback(running_step.revision_context.as_deref());
        let pending_loop_revision_feedback =
            Self::pending_revision_feedback_is_loop(running_step.revision_context.as_deref());
        let skip_initial_lead_review = pending_loop_revision_feedback
            || pending_revision_feedback
                .as_ref()
                .is_some_and(|feedback| feedback.source == WorkflowRevisionFeedbackSource::User);
        let prompt_context = if pending_revision_feedback.is_some() {
            AgentPromptContext::StepRevision
        } else {
            AgentPromptContext::StepExecution
        };
        let mut sa_clone = session_agent.clone();
        let agent_skill_names: Vec<String> = chat_runner
            .prepare_and_resolve_agent_skills(&mut sa_clone, agent, prompt_context)
            .await
            .unwrap_or_default()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        let workspace_isolation_prompt = (running_step.step_type == WorkflowStepType::Task)
            .then(|| {
                Self::active_frontier_workspace_isolation_prompt(
                    session,
                    &running_step,
                    current_steps,
                    edges,
                    workflow_agent_sessions,
                    session_agent,
                    session_agents,
                    agents,
                )
            })
            .flatten();
        let ui_config = config::load_config_from_file(&config_path()).await;
        let response_language_instruction =
            resolve_workflow_response_language_instruction(&ui_config.language);
        let contract = Self::execution_contract_for_step(plan, &running_step);
        let review_criteria = (running_step.step_type == WorkflowStepType::Review)
            .then(|| {
                workflow_runtime::build_workflow_review_criteria(
                    &contract.acceptance_leveled,
                    Some(&running_step.instructions),
                )
            })
            .unwrap_or_default();
        let mut prompt = if running_step.step_type == WorkflowStepType::Review {
            // 普通 review 节点（无 reviewScope）复用 step_review builder（设计 §6.4）。
            prompt_builders::step_review::build_step_review_prompt(
                &prompt_builders::step_review::StepReviewPromptInput {
                    identity: prompt_builders::common::PromptIdentity {
                        execution_id: running_step.execution_id,
                        step_key: running_step.step_key.clone(),
                    },
                    workflow_goal: workflow_goal.clone(),
                    title: running_step.title.clone(),
                    instructions: running_step.instructions.clone(),
                    acceptance_criteria: review_criteria.clone(),
                    review_rules: Vec::new(),
                    worker_result: Self::review_worker_result_for_step(
                        &running_step,
                        current_steps,
                        edges,
                    )?,
                    // Direct predecessors are already represented by
                    // `worker_result`; do not duplicate them as upstream
                    // context for an ordinary review node.
                    upstream_results: Vec::new(),
                    latest_review_feedback: pending_revision_feedback
                        .as_ref()
                        .map(|feedback| feedback.feedback.clone()),
                    response_language: response_language_instruction.to_string(),
                },
            )
        } else {
            // task 首次执行与 Lead/Reviewer/User/Loop 反馈返工共用 task builder
            // （设计 §6.3、§11.2）。
            let revision = pending_revision_feedback.as_ref().map(|feedback| {
                prompt_builders::task_execution::RevisionContextInput {
                    source: feedback.source,
                    attempt: running_step.retry_count.saturating_add(1),
                    feedback: feedback.feedback.clone(),
                    previous_summary: feedback.previous_summary.clone(),
                    previous_outputs: feedback.previous_outputs.clone(),
                    review_outcome: feedback.review_details.as_ref().map(|details| {
                        prompt_builders::task_execution::ReviewOutcomeInput {
                            acceptance_results: details.acceptance_results.clone(),
                            evidence: details.evidence.clone(),
                            risks: details.risks.clone(),
                            unfinished_items: details.unfinished_items.clone(),
                        }
                    }),
                }
            });
            prompt_builders::task_execution::build_task_execution_prompt(
                &prompt_builders::task_execution::TaskExecutionPromptInput {
                    identity: prompt_builders::common::PromptIdentity {
                        execution_id: running_step.execution_id,
                        step_key: running_step.step_key.clone(),
                    },
                    workflow_goal: workflow_goal.clone(),
                    title: running_step.title.clone(),
                    instructions: running_step.instructions.clone(),
                    contract: prompt_builders::task_execution::TaskExecutionContractInput {
                        outputs: contract.expected_outputs.clone(),
                        self_check: contract.self_check.clone(),
                        verification_methods: contract.verification_commands.clone(),
                        completion_evidence: contract.completion_evidence.clone(),
                    },
                    upstream_results: Self::upstream_results_for_step(
                        &running_step,
                        current_steps,
                        edges,
                    )?,
                    revision,
                    response_language: response_language_instruction.to_string(),
                },
            )
        };
        if let Some(section) =
            crate::services::agent_skill_policy::format_skills_prompt_section(&agent_skill_names)
        {
            inject_step_prompt_section_before_schema(&mut prompt, &section);
        }
        if let Some(section) = workspace_isolation_prompt.as_deref() {
            inject_step_prompt_section_before_schema(&mut prompt, section);
        }

        let running_step = if pending_revision_feedback.is_some() {
            WorkflowStep::update_revision_context(
                pool,
                running_step.id,
                Self::clear_pending_revision_feedback(running_step.revision_context.as_deref()),
            )
            .await?
        } else {
            running_step
        };

        if running_step.step_type == WorkflowStepType::Review {
            let (review_message, review_agent_output) =
                match Self::run_step_review_protocol_with_retry(
                    db,
                    pool,
                    chat_runner,
                    execution,
                    session,
                    agent,
                    session_agent,
                    workflow_session,
                    &prompt,
                    &running_step,
                    &review_criteria,
                )
                .await
                {
                    Ok(result) => result,
                    Err(OrchestratorError::Runtime(WorkflowRuntimeError::Interrupted(reason))) => {
                        let interrupted_step = Self::acknowledge_step_interrupted(
                            pool,
                            chat_runner,
                            execution,
                            running_step.id,
                            "step_interrupted",
                        )
                        .await?;
                        let _ = Self::write_transcript(
                            pool,
                            execution.id,
                            interrupted_step.round_id.into(),
                            Some(workflow_session.id),
                            Some(interrupted_step.id),
                            "system",
                            "message",
                            &format!(
                                "Review step \"{}\" interrupted: {}",
                                interrupted_step.title, reason
                            ),
                            None,
                        )
                        .await;
                        return Ok(StepOutcome::Interrupted);
                    }
                    Err(err) => {
                        let err_message = err.to_string();
                        let failed_step = WorkflowStep::record_execution_result(
                            pool,
                            running_step.id,
                            Uuid::new_v4(),
                            Some(
                                serde_json::to_string(&SummaryPayload {
                                    summary: err_message.clone(),
                                    content: None,
                                    outputs: Vec::new(),
                                })
                                .unwrap_or_else(|_| err_message.clone()),
                            ),
                            None,
                        )
                        .await?;
                        Self::transition_step_and_sync(
                            pool,
                            chat_runner,
                            execution,
                            &failed_step,
                            WorkflowStepStatus::Failed,
                            "step_failed",
                        )
                        .await?;
                        return Ok(StepOutcome::Failed(err_message));
                    }
                };
            return Self::handle_step_review_protocol_message(
                pool,
                chat_runner,
                execution,
                &running_step,
                workflow_session,
                review_message,
                &review_criteria,
                review_agent_output.run_id,
            )
            .await;
        }

        let (protocol_message, agent_output) = match Self::run_step_agent_protocol_with_retry(
            db,
            pool,
            chat_runner,
            session,
            agent,
            session_agent,
            workflow_session,
            &prompt,
            &running_step,
            pending_revision_feedback.is_some(),
        )
        .await
        {
            Ok((message, agent_output)) => (message, agent_output),
            Err(OrchestratorError::Runtime(WorkflowRuntimeError::Interrupted(reason))) => {
                let interrupted_step = Self::acknowledge_step_interrupted(
                    pool,
                    chat_runner,
                    execution,
                    running_step.id,
                    "step_interrupted",
                )
                .await?;
                let _ = Self::write_transcript(
                    pool,
                    execution.id,
                    interrupted_step.round_id.into(),
                    Some(workflow_session.id),
                    Some(interrupted_step.id),
                    "system",
                    "message",
                    &format!(
                        "Step \"{}\" interrupted: {}",
                        interrupted_step.title, reason
                    ),
                    None,
                )
                .await;
                return Ok(StepOutcome::Interrupted);
            }
            Err(err) => {
                let err_message = err.to_string();
                let failed_step = WorkflowStep::record_execution_result(
                    pool,
                    running_step.id,
                    Uuid::new_v4(),
                    Some(
                        serde_json::to_string(&SummaryPayload {
                            summary: err_message.clone(),
                            content: None,
                            outputs: vec![],
                        })
                        .unwrap_or_else(|_| err_message.clone()),
                    ),
                    None,
                )
                .await?;
                Self::transition_step_and_sync(
                    pool,
                    chat_runner,
                    execution,
                    &failed_step,
                    WorkflowStepStatus::Failed,
                    "step_failed",
                )
                .await?;
                let _ = Self::write_transcript(
                    pool,
                    execution.id,
                    failed_step.round_id.into(),
                    Some(workflow_session.id),
                    Some(failed_step.id),
                    "system",
                    "message",
                    &format!("Step \"{}\" failed: {}", failed_step.title, err),
                    None,
                )
                .await;
                return Ok(StepOutcome::Failed(err_message));
            }
        };

        let latest_running_step = WorkflowStep::find_by_id(pool, running_step.id)
            .await?
            .unwrap_or_else(|| running_step.clone());

        match protocol_message {
            WorkflowStepProtocolMessage::FinalResult {
                status:
                    status @ (workflow_runtime::WorkflowTaskCompletionStatus::Done
                    | workflow_runtime::WorkflowTaskCompletionStatus::DoneWithConcerns),
                summary,
                content,
                verification,
                files_changed,
                self_review,
                issues,
                evidence,
                outputs,
                ..
            } if latest_running_step.step_type == WorkflowStepType::Task
                && (latest_running_step.lead_review_required
                    || latest_running_step.user_review_required) =>
            {
                Self::execute_step_with_feedback(
                    db,
                    pool,
                    chat_runner,
                    execution,
                    &latest_running_step,
                    workflow_session,
                    session,
                    session_agent,
                    agent,
                    workflow_agent_sessions,
                    session_agents,
                    agents,
                    plan,
                    current_steps,
                    edges,
                    workflow_step_run_result_from_task_report(
                        agent_output.run_id.unwrap_or_else(Uuid::new_v4),
                        status,
                        summary,
                        content,
                        verification,
                        files_changed,
                        self_review,
                        issues,
                        evidence,
                        outputs,
                    ),
                    skip_initial_lead_review,
                )
                .await
            }
            other => {
                Self::handle_step_protocol_message(
                    pool,
                    chat_runner,
                    execution,
                    &running_step,
                    workflow_session,
                    Some(agent.id.to_string()),
                    other,
                    agent_output.run_id,
                )
                .await
            }
        }
    }
}
