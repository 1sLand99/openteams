//! Workflow Orchestrator 骨架
//!
//! Phase 1a 职责：
//! - command handler: 接收 bootstrap 命令
//! - state reducer: 集中管理执行/步骤/agent session 状态迁移
//! - scheduler loop 骨架: 接口预留
//! - event projector: 审计事件由 reducer 自动写入

pub mod reducer;

use std::collections::HashMap;

use db::{
    DBService,
    models::{
        chat_agent::ChatAgent,
        chat_message::{ChatMessage, ChatSenderType},
        chat_session::ChatSession,
        chat_session_agent::ChatSessionAgent,
        chat_work_item::{ChatWorkItem, ChatWorkItemType},
        workflow_agent_session::{CreateWorkflowAgentSession, WorkflowAgentSession},
        workflow_event::{CreateWorkflowEvent, WorkflowEvent},
        workflow_execution::{CreateWorkflowExecution, WorkflowExecution},
        workflow_plan::{CreateWorkflowPlan, WorkflowPlan},
        workflow_plan_revision::{CreateWorkflowPlanRevision, WorkflowPlanRevision},
        workflow_round::{CreateWorkflowRound, WorkflowRound},
        workflow_step::{CreateWorkflowStep, WorkflowStep},
        workflow_step_edge::{CreateWorkflowStepEdge, WorkflowStepEdge},
        workflow_types::*,
    },
};
use sqlx::SqlitePool;
use uuid::Uuid;

use super::{
    chat,
    chat_runner::{ChatRunner, ChatRunnerError},
    workflow_compiler::WorkflowCompiler,
    workflow_runtime::{
        SummaryPayload, WorkflowRuntimeError, WorkflowStepProtocolMessage, WorkflowStepRunResult,
        build_step_execution_prompt, build_workflow_card_projection, parse_summary_payload,
        predecessor_summaries, run_workflow_agent_prompt,
    },
};

/// Orchestrator 错误
#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("数据库错误: {0}")]
    Database(#[from] sqlx::Error),
    #[error("编译错误: {0}")]
    Compile(#[from] super::workflow_compiler::CompileError),
    #[error("运行时错误: {0}")]
    Runtime(#[from] WorkflowRuntimeError),
    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    #[error("聊天服务错误: {0}")]
    Chat(#[from] super::chat::ChatServiceError),
    #[error("聊天运行器错误: {0}")]
    ChatRunner(#[from] ChatRunnerError),
    #[error("状态迁移非法: {0}")]
    IllegalTransition(String),
    #[error("未找到资源: {0}")]
    NotFound(String),
}

impl From<reducer::TransitionError> for OrchestratorError {
    fn from(e: reducer::TransitionError) -> Self {
        OrchestratorError::IllegalTransition(e.to_string())
    }
}

/// Orchestrator 是 workflow mode 的核心调度组件
pub struct WorkflowOrchestrator;

impl WorkflowOrchestrator {
    // -----------------------------------------------------------------------
    // Command Handler: bootstrap
    // -----------------------------------------------------------------------

    /// 从一个已校验的 plan revision 创建 execution 并 bootstrap
    ///
    /// 流程:
    /// 1. 创建 execution (pending)
    /// 2. 通过 reducer 迁移到 bootstrapping
    /// 3. 编译 plan → compiled graph
    /// 4. 创建 agent sessions, round, steps, edges
    /// 5. 通过 reducer 迁移 ready steps
    /// 6. 通过 reducer 迁移到 running 或 failed
    ///
    /// `agent_id_map`: 将 plan JSON 中的 string agent_id 映射到实际的 session_agent UUID
    pub async fn bootstrap_execution(
        pool: &SqlitePool,
        plan: &WorkflowPlan,
        revision: &WorkflowPlanRevision,
        lead_session_agent_id: Option<Uuid>,
        valid_agent_ids: &[String],
        agent_id_map: &HashMap<String, Uuid>,
    ) -> Result<BootstrapResult, OrchestratorError> {
        let execution_id = Uuid::new_v4();

        // 1. 创建 execution (pending)
        let execution = WorkflowExecution::create(
            pool,
            &CreateWorkflowExecution {
                session_id: plan.session_id,
                plan_id: plan.id,
                active_revision_id: Some(revision.id),
                lead_session_agent_id,
                title: plan.title.clone(),
            },
            execution_id,
        )
        .await?;

        // 2. 通过 reducer 迁移到 bootstrapping（校验 + 持久化 + 审计事件）
        let tr =
            reducer::transition_execution(pool, &execution, WorkflowExecutionStatus::Bootstrapping)
                .await?;
        let execution = tr.entity;

        // 3. 编译 plan
        let compiled =
            match WorkflowCompiler::compile_from_json(&revision.plan_json, valid_agent_ids) {
                Ok(graph) => graph,
                Err(e) => {
                    // bootstrapping -> failed（通过 reducer，含审计事件）
                    let tr = reducer::transition_execution_with_context(
                        pool,
                        &execution,
                        WorkflowExecutionStatus::Failed,
                        None,
                        Some(&format!("编译失败: {}", e)),
                    )
                    .await?;

                    return Ok(BootstrapResult {
                        execution: tr.entity,
                        round: None,
                        steps: vec![],
                        edges: vec![],
                        agent_sessions: vec![],
                        events: vec![],
                        failed: true,
                        failure_reason: Some(format!("{}", e)),
                    });
                }
            };

        // 4. 更新 compiled graph hash（数据字段更新，非状态迁移）
        let execution = WorkflowExecution::update_compiled_graph_hash(
            pool,
            execution.id,
            &compiled.compiled_graph_hash,
            revision.id,
        )
        .await?;

        // 5. 创建 round
        let round_id = Uuid::new_v4();
        let round = WorkflowRound::create(
            pool,
            &CreateWorkflowRound {
                execution_id: execution.id,
                round_index: 1,
                source_revision_id: Some(revision.id),
            },
            round_id,
        )
        .await?;

        // 更新 execution 的 active round（数据字段更新，非状态迁移）
        let execution =
            WorkflowExecution::update_active_round(pool, execution.id, round.id, 1).await?;

        // 6. 创建 workflow agent sessions（去重：每个 agent 只创建一个 session）
        let mut agent_session_map: HashMap<String, Uuid> = HashMap::new();
        let mut created_agent_sessions = Vec::new();
        let mut lead_workflow_agent_session_id = None;

        if let Some(lead_session_agent_id) = lead_session_agent_id {
            let ws = WorkflowAgentSession::create(
                pool,
                &CreateWorkflowAgentSession {
                    workflow_execution_id: execution.id,
                    session_agent_id: lead_session_agent_id,
                    role: WorkflowAgentSessionRole::Lead,
                },
                Uuid::new_v4(),
            )
            .await?;
            lead_workflow_agent_session_id = Some(ws.id);
            created_agent_sessions.push(ws);
        }

        for compiled_step in &compiled.steps {
            if let Some(ref agent_id_str) = compiled_step.assigned_agent_id {
                if agent_session_map.contains_key(agent_id_str) {
                    continue;
                }
                if let Some(&session_agent_uuid) = agent_id_map.get(agent_id_str) {
                    if lead_session_agent_id == Some(session_agent_uuid) {
                        if let Some(lead_workflow_agent_session_id) = lead_workflow_agent_session_id
                        {
                            agent_session_map
                                .insert(agent_id_str.clone(), lead_workflow_agent_session_id);
                        }
                        continue;
                    }
                    let role = if lead_session_agent_id == Some(session_agent_uuid) {
                        WorkflowAgentSessionRole::Lead
                    } else {
                        WorkflowAgentSessionRole::Worker
                    };
                    let ws_id = Uuid::new_v4();
                    let ws = WorkflowAgentSession::create(
                        pool,
                        &CreateWorkflowAgentSession {
                            workflow_execution_id: execution.id,
                            session_agent_id: session_agent_uuid,
                            role,
                        },
                        ws_id,
                    )
                    .await?;
                    agent_session_map.insert(agent_id_str.clone(), ws.id);
                    created_agent_sessions.push(ws);
                }
            }
        }

        // 7. 创建 steps 并绑定 agent session
        let mut step_id_map: HashMap<String, Uuid> = HashMap::new();
        let mut created_steps = Vec::new();

        for compiled_step in &compiled.steps {
            let step_id = Uuid::new_v4();
            step_id_map.insert(compiled_step.step_key.clone(), step_id);

            let assigned_ws_id = compiled_step
                .assigned_agent_id
                .as_ref()
                .and_then(|aid| agent_session_map.get(aid))
                .copied()
                .or(lead_workflow_agent_session_id);

            let step = WorkflowStep::create(
                pool,
                &CreateWorkflowStep {
                    execution_id: execution.id,
                    round_id: round.id,
                    compiled_revision_id: Some(revision.id),
                    step_key: compiled_step.step_key.clone(),
                    step_type: compiled_step.step_type.clone(),
                    title: compiled_step.title.clone(),
                    instructions: compiled_step.instructions.clone(),
                    assigned_workflow_agent_session_id: assigned_ws_id,
                    max_retry: compiled_step.max_retry as i32,
                    round_index: 1,
                    display_order: compiled_step.display_order,
                },
                step_id,
            )
            .await?;

            created_steps.push(step);
        }

        // 8. 创建 edges
        let mut created_edges = Vec::new();
        for compiled_edge in &compiled.edges {
            let from_id = step_id_map
                .get(&compiled_edge.from_step_key)
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!(
                        "步骤 {} 未找到",
                        compiled_edge.from_step_key
                    ))
                })?;
            let to_id = step_id_map.get(&compiled_edge.to_step_key).ok_or_else(|| {
                OrchestratorError::NotFound(format!("步骤 {} 未找到", compiled_edge.to_step_key))
            })?;

            let edge = WorkflowStepEdge::create(
                pool,
                &CreateWorkflowStepEdge {
                    execution_id: execution.id,
                    compiled_revision_id: Some(revision.id),
                    from_step_id: *from_id,
                    to_step_id: *to_id,
                    edge_kind: compiled_edge.edge_kind.clone(),
                },
                Uuid::new_v4(),
            )
            .await?;

            created_edges.push(edge);
        }

        // 9. 将无前驱的 step 标记为 ready（通过 reducer，含组合约束校验 + 审计事件）
        for ready_key in &compiled.ready_step_keys {
            if let Some(&step_id) = step_id_map.get(ready_key) {
                let step = created_steps.iter().find(|s| s.id == step_id);
                if let Some(step) = step {
                    let tr =
                        reducer::transition_step(pool, &execution, step, WorkflowStepStatus::Ready)
                            .await?;
                    // 更新 created_steps 中的 step 状态
                    if let Some(s) = created_steps.iter_mut().find(|s| s.id == step_id) {
                        *s = tr.entity;
                    }
                }
            }
        }

        // 10. 通过 reducer 迁移到 running（校验 + 持久化 + 审计事件）
        let tr = reducer::transition_execution_with_context(
            pool,
            &execution,
            WorkflowExecutionStatus::Running,
            Some(round.id),
            None,
        )
        .await?;
        let execution = tr.entity;
        // 设置 started_at 时间戳（数据字段更新，非状态迁移）
        let execution = WorkflowExecution::set_started(pool, execution.id).await?;

        // 写入 round 启动事件（非状态迁移事件，由 orchestrator 直接写入）
        WorkflowEvent::create(
            pool,
            &CreateWorkflowEvent {
                execution_id: execution.id,
                round_id: Some(round.id),
                step_id: None,
                agent_session_id: None,
                event_type: WorkflowEventType::RoundStarted,
                status_before: None,
                status_after: Some("running".to_string()),
                detail_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;

        let events = WorkflowEvent::find_by_execution(pool, execution.id).await?;

        Ok(BootstrapResult {
            execution,
            round: Some(round),
            steps: created_steps,
            edges: created_edges,
            agent_sessions: created_agent_sessions,
            events,
            failed: false,
            failure_reason: None,
        })
    }

    // -----------------------------------------------------------------------
    // Scheduler Loop 骨架 (Phase 1b 实现)
    // -----------------------------------------------------------------------

    /// Phase 1b: 唤醒调度循环，找到 ready steps 并触发 agent run
    pub async fn wake_scheduler(
        db: &DBService,
        chat_runner: &ChatRunner,
        execution_id: Uuid,
    ) -> Result<(), OrchestratorError> {
        let pool = &db.pool;
        let mut execution = WorkflowExecution::find_by_id(pool, execution_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("execution {} 未找到", execution_id))
            })?;

        loop {
            let plan = WorkflowPlan::find_by_id(pool, execution.plan_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("plan {} 未找到", execution.plan_id))
                })?;
            let revision_id = execution.active_revision_id.ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "execution {} 缺少 active revision",
                    execution.id
                ))
            })?;
            let revision = WorkflowPlanRevision::find_by_id(pool, revision_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("revision {} 未找到", revision_id))
                })?;
            let session = ChatSession::find_by_id(pool, execution.session_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("session {} 未找到", execution.session_id))
                })?;
            let session_agents = ChatSessionAgent::find_all_for_session(pool, session.id).await?;
            let workflow_agent_sessions =
                WorkflowAgentSession::find_by_execution(pool, execution.id).await?;
            let steps = WorkflowStep::find_by_execution(pool, execution.id).await?;
            let edges = WorkflowStepEdge::find_by_execution(pool, execution.id).await?;
            let agents = load_agents_for_session(pool, &session_agents).await?;

            if steps
                .iter()
                .all(|step| step.status == WorkflowStepStatus::Completed)
            {
                for workflow_session in &workflow_agent_sessions {
                    if workflow_session.state == WorkflowAgentSessionState::Running {
                        reducer::transition_agent_session(
                            pool,
                            &execution,
                            workflow_session,
                            WorkflowAgentSessionState::Completed,
                        )
                        .await?;
                    }
                }
                let completing = reducer::transition_execution(
                    pool,
                    &execution,
                    WorkflowExecutionStatus::Completing,
                )
                .await?;
                execution = completing.entity;
                let completed = reducer::transition_execution(
                    pool,
                    &execution,
                    WorkflowExecutionStatus::Completed,
                )
                .await?;
                execution = WorkflowExecution::set_completed(pool, completed.entity.id).await?;
                Self::persist_completion_work_items(
                    pool,
                    chat_runner,
                    &execution,
                    &steps,
                    &workflow_agent_sessions,
                    &session_agents,
                    &agents,
                )
                .await?;
                Self::refresh_workflow_card(
                    pool,
                    chat_runner,
                    &execution,
                    &plan,
                    &revision,
                    &session_agents,
                    &agents,
                    None,
                )
                .await?;
                return Ok(());
            }

            let mut ready_promotions = Vec::new();
            for step in &steps {
                if step.status != WorkflowStepStatus::Pending {
                    continue;
                }

                let blocked = edges
                    .iter()
                    .filter(|edge| edge.to_step_id == step.id)
                    .any(|edge| {
                        steps
                            .iter()
                            .find(|candidate| candidate.id == edge.from_step_id)
                            .map(|candidate| candidate.status != WorkflowStepStatus::Completed)
                            .unwrap_or(true)
                    });

                if !blocked {
                    ready_promotions.push(step.id);
                }
            }

            let mut current_steps = steps;
            for step_id in ready_promotions {
                if let Some(step) = current_steps
                    .iter()
                    .find(|step| step.id == step_id)
                    .cloned()
                {
                    let transitioned = reducer::transition_step(
                        pool,
                        &execution,
                        &step,
                        WorkflowStepStatus::Ready,
                    )
                    .await?;
                    if let Some(existing) = current_steps.iter_mut().find(|item| item.id == step_id)
                    {
                        *existing = transitioned.entity;
                    }
                }
            }

            let next_step = current_steps
                .iter()
                .filter(|step| step.status == WorkflowStepStatus::Ready)
                .min_by_key(|step| step.display_order)
                .cloned();

            let Some(step) = next_step else {
                Self::refresh_workflow_card(
                    pool,
                    chat_runner,
                    &execution,
                    &plan,
                    &revision,
                    &session_agents,
                    &agents,
                    None,
                )
                .await?;
                return Ok(());
            };

            let workflow_session =
                resolve_step_workflow_session(&execution, &workflow_agent_sessions, &step)?;
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

            if workflow_session.state == WorkflowAgentSessionState::Idle {
                reducer::transition_agent_session(
                    pool,
                    &execution,
                    workflow_session,
                    WorkflowAgentSessionState::Running,
                )
                .await?;
            }

            let running_step =
                reducer::transition_step(pool, &execution, &step, WorkflowStepStatus::Running)
                    .await?
                    .entity;

            Self::refresh_workflow_card(
                pool,
                chat_runner,
                &execution,
                &plan,
                &revision,
                &session_agents,
                &agents,
                None,
            )
            .await?;

            let dependency_summaries = predecessor_summaries(&running_step, &current_steps, &edges);
            let workflow_goal = plan
                .summary_text
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| plan.title.clone());
            let prompt = build_step_execution_prompt(
                &execution,
                &workflow_goal,
                &running_step,
                &dependency_summaries,
            );

            let execution_result = match run_workflow_agent_prompt(
                db,
                &session,
                agent,
                session_agent,
                &prompt,
            )
            .await
            {
                Ok(raw_output) => {
                    match Self::convert_step_output(execution.id, &running_step, &raw_output) {
                        Ok(result) => result,
                        Err(err) => {
                            let failed_step = WorkflowStep::record_execution_result(
                                pool,
                                running_step.id,
                                Uuid::new_v4(),
                                Some(
                                    serde_json::to_string(&SummaryPayload {
                                        summary: err.to_string(),
                                        content: Some(raw_output),
                                        outputs: vec![],
                                    })
                                    .unwrap_or_else(|_| err.to_string()),
                                ),
                            )
                            .await?;
                            reducer::transition_step(
                                pool,
                                &execution,
                                &failed_step,
                                WorkflowStepStatus::Failed,
                            )
                            .await?;
                            if workflow_session.state == WorkflowAgentSessionState::Running {
                                reducer::transition_agent_session(
                                    pool,
                                    &execution,
                                    workflow_session,
                                    WorkflowAgentSessionState::Failed,
                                )
                                .await?;
                            }
                            execution = reducer::transition_execution_with_context(
                                pool,
                                &execution,
                                WorkflowExecutionStatus::Failed,
                                Some(running_step.round_id),
                                Some(&err.to_string()),
                            )
                            .await?
                            .entity;
                            Self::refresh_workflow_card(
                                pool,
                                chat_runner,
                                &execution,
                                &plan,
                                &revision,
                                &session_agents,
                                &agents,
                                Some(err.to_string()),
                            )
                            .await?;
                            return Ok(());
                        }
                    }
                }
                Err(err) => {
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
                    )
                    .await?;
                    reducer::transition_step(
                        pool,
                        &execution,
                        &failed_step,
                        WorkflowStepStatus::Failed,
                    )
                    .await?;
                    if workflow_session.state == WorkflowAgentSessionState::Running {
                        reducer::transition_agent_session(
                            pool,
                            &execution,
                            workflow_session,
                            WorkflowAgentSessionState::Failed,
                        )
                        .await?;
                    }
                    execution = reducer::transition_execution_with_context(
                        pool,
                        &execution,
                        WorkflowExecutionStatus::Failed,
                        Some(running_step.round_id),
                        Some(&err.to_string()),
                    )
                    .await?
                    .entity;
                    Self::refresh_workflow_card(
                        pool,
                        chat_runner,
                        &execution,
                        &plan,
                        &revision,
                        &session_agents,
                        &agents,
                        Some(err.to_string()),
                    )
                    .await?;
                    return Ok(());
                }
            };

            let recorded_step = WorkflowStep::record_execution_result(
                pool,
                running_step.id,
                execution_result.run_id,
                Some(
                    serde_json::to_string(&SummaryPayload {
                        summary: execution_result.summary.clone(),
                        content: Some(execution_result.content.clone()),
                        outputs: execution_result.outputs.clone(),
                    })
                    .unwrap_or_else(|_| execution_result.summary.clone()),
                ),
            )
            .await?;
            reducer::transition_step(
                pool,
                &execution,
                &recorded_step,
                WorkflowStepStatus::Completed,
            )
            .await?;

            Self::refresh_workflow_card(
                pool,
                chat_runner,
                &execution,
                &plan,
                &revision,
                &session_agents,
                &agents,
                None,
            )
            .await?;

            execution = WorkflowExecution::find_by_id(pool, execution.id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("execution {} 未找到", execution.id))
                })?;
        }
    }
}

fn resolve_step_workflow_session<'a>(
    execution: &WorkflowExecution,
    workflow_sessions: &'a [WorkflowAgentSession],
    step: &WorkflowStep,
) -> Result<&'a WorkflowAgentSession, OrchestratorError> {
    if let Some(workflow_session_id) = step.assigned_workflow_agent_session_id {
        return workflow_sessions
            .iter()
            .find(|session| session.id == workflow_session_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "workflow agent session {} 未找到",
                    workflow_session_id
                ))
            });
    }

    let lead_session_agent_id = execution.lead_session_agent_id.ok_or_else(|| {
        OrchestratorError::NotFound(format!(
            "execution {} 缺少 lead session agent",
            execution.id
        ))
    })?;

    workflow_sessions
        .iter()
        .find(|session| session.session_agent_id == lead_session_agent_id)
        .ok_or_else(|| {
            OrchestratorError::NotFound(format!(
                "execution {} 的 lead workflow session 未找到",
                execution.id
            ))
        })
}

async fn load_agents_for_session(
    pool: &SqlitePool,
    session_agents: &[ChatSessionAgent],
) -> Result<Vec<ChatAgent>, OrchestratorError> {
    let mut agents = Vec::new();
    for session_agent in session_agents {
        let agent = ChatAgent::find_by_id(pool, session_agent.agent_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("agent {} 未找到", session_agent.agent_id))
            })?;
        agents.push(agent);
    }
    Ok(agents)
}

impl WorkflowOrchestrator {
    fn convert_step_output(
        execution_id: Uuid,
        step: &WorkflowStep,
        raw_output: &str,
    ) -> Result<WorkflowStepRunResult, OrchestratorError> {
        match super::workflow_runtime::parse_step_protocol_output(
            execution_id,
            &step.step_key,
            raw_output,
        )? {
            WorkflowStepProtocolMessage::FinalResult {
                summary,
                content,
                outputs,
                ..
            } => Ok(WorkflowStepRunResult {
                run_id: Uuid::new_v4(),
                summary,
                content,
                outputs,
            }),
            WorkflowStepProtocolMessage::Error {
                message, content, ..
            } => Err(OrchestratorError::Runtime(
                WorkflowRuntimeError::Validation(
                    content
                        .filter(|value| !value.trim().is_empty())
                        .map(|value| format!("{message}: {value}"))
                        .unwrap_or(message),
                ),
            )),
        }
    }

    pub async fn refresh_workflow_card(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        plan: &WorkflowPlan,
        revision: &WorkflowPlanRevision,
        session_agents: &[ChatSessionAgent],
        agents: &[ChatAgent],
        error_message: Option<String>,
    ) -> Result<(), OrchestratorError> {
        let Some(message_id) = execution.workflow_card_message_id else {
            return Ok(());
        };

        let message = ChatMessage::find_by_id(pool, message_id)
            .await?
            .ok_or_else(|| OrchestratorError::NotFound(format!("message {} 未找到", message_id)))?;
        let workflow_sessions = WorkflowAgentSession::find_by_execution(pool, execution.id).await?;
        let steps = WorkflowStep::find_by_execution(pool, execution.id).await?;
        let edges = WorkflowStepEdge::find_by_execution(pool, execution.id).await?;

        let projection = build_workflow_card_projection(
            execution,
            plan,
            revision,
            &steps,
            &edges,
            &workflow_sessions,
            session_agents,
            agents,
            error_message,
        )?;
        let mut meta = message.meta.0.clone();
        meta["card_type"] = serde_json::json!("workflow_execution");
        meta["workflow_card"] = serde_json::to_value(&projection)?;

        let updated =
            ChatMessage::update_content_and_meta(pool, message.id, "Workflow execution", meta)
                .await?;
        chat_runner.emit_message_updated(updated.session_id, updated);
        Ok(())
    }

    async fn persist_completion_work_items(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        steps: &[WorkflowStep],
        workflow_sessions: &[WorkflowAgentSession],
        session_agents: &[ChatSessionAgent],
        agents: &[ChatAgent],
    ) -> Result<(), OrchestratorError> {
        if !ChatWorkItem::find_by_run_id(pool, execution.id)
            .await?
            .is_empty()
        {
            return Ok(());
        }

        let result_step = steps
            .iter()
            .find(|step| step.step_type == WorkflowStepType::Result)
            .ok_or_else(|| {
                OrchestratorError::NotFound("workflow result step 未找到".to_string())
            })?;
        let payload =
            parse_summary_payload(result_step.summary_text.as_deref()).ok_or_else(|| {
                OrchestratorError::Runtime(WorkflowRuntimeError::Validation(
                    "workflow result step 缺少可持久化的完成摘要".to_string(),
                ))
            })?;
        let workflow_session =
            resolve_step_workflow_session(execution, workflow_sessions, result_step)?;
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

        let conclusion = match payload.content.as_deref().map(str::trim) {
            Some(content) if !content.is_empty() && content != payload.summary.trim() => {
                format!("{}\n\n{}", payload.summary, content)
            }
            _ => payload.summary.clone(),
        };

        chat_runner
            .persist_work_item(
                execution.session_id,
                session_agent.id,
                agent.id,
                execution.id,
                &agent.name,
                ChatWorkItemType::Conclusion,
                conclusion,
            )
            .await?;

        for output in payload.outputs {
            let output = output.trim();
            if output.is_empty() {
                continue;
            }

            chat_runner
                .persist_work_item(
                    execution.session_id,
                    session_agent.id,
                    agent.id,
                    execution.id,
                    &agent.name,
                    ChatWorkItemType::Artifact,
                    format!("`{output}`"),
                )
                .await?;
        }

        Ok(())
    }

    pub async fn create_workflow_plan_and_card(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        session: &ChatSession,
        source_message_id: Option<Uuid>,
        lead_session_agent: &ChatSessionAgent,
        plan_json: &str,
    ) -> Result<(WorkflowPlan, WorkflowPlanRevision, ChatMessage), OrchestratorError> {
        let parsed_plan: WorkflowPlanJson = serde_json::from_str(plan_json)?;
        let plan_hash = WorkflowCompiler::compute_hash(&parsed_plan);
        let plan = WorkflowPlan::create(
            pool,
            &CreateWorkflowPlan {
                session_id: session.id,
                source_message_id,
                created_by_session_agent_id: Some(lead_session_agent.id),
                title: parsed_plan.title.clone(),
                summary_text: Some(parsed_plan.goal.clone()),
                plan_json: plan_json.to_string(),
                plan_schema_version: parsed_plan.version as i32,
                plan_hash: plan_hash.clone(),
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;
        let plan = WorkflowPlan::update_status(pool, plan.id, WorkflowPlanStatus::Ready).await?;
        let revision = WorkflowPlanRevision::create(
            pool,
            &CreateWorkflowPlanRevision {
                plan_id: plan.id,
                revision_no: 1,
                edited_by: WorkflowRevisionEditor::Lead,
                editor_session_agent_id: Some(lead_session_agent.id),
                reason: Some("generate-plan-and-run".to_string()),
                plan_json: plan_json.to_string(),
                plan_hash,
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;
        let message = chat::create_message(
            pool,
            session.id,
            ChatSenderType::System,
            None,
            "Workflow execution".to_string(),
            Some(serde_json::json!({
                "card_type": "workflow_execution"
            })),
        )
        .await?;
        chat_runner.emit_message_new(message.session_id, message.clone());
        Ok((plan, revision, message))
    }

    /// Create a plan in `ready` state and a preview card (no execution created).
    /// Used by the `workflow_generate` -> plan_generation pipeline.
    pub async fn create_workflow_plan_preview_card(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        session: &ChatSession,
        source_message_id: Option<Uuid>,
        lead_session_agent: &ChatSessionAgent,
        plan_json: &str,
    ) -> Result<(WorkflowPlan, WorkflowPlanRevision, ChatMessage), OrchestratorError> {
        let parsed_plan: WorkflowPlanJson = serde_json::from_str(plan_json)?;
        let plan_hash = WorkflowCompiler::compute_hash(&parsed_plan);
        let plan = WorkflowPlan::create(
            pool,
            &CreateWorkflowPlan {
                session_id: session.id,
                source_message_id,
                created_by_session_agent_id: Some(lead_session_agent.id),
                title: parsed_plan.title.clone(),
                summary_text: Some(parsed_plan.goal.clone()),
                plan_json: plan_json.to_string(),
                plan_schema_version: parsed_plan.version as i32,
                plan_hash: plan_hash.clone(),
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;
        let plan = WorkflowPlan::update_status(pool, plan.id, WorkflowPlanStatus::Ready).await?;
        let revision = WorkflowPlanRevision::create(
            pool,
            &CreateWorkflowPlanRevision {
                plan_id: plan.id,
                revision_no: 1,
                edited_by: WorkflowRevisionEditor::Lead,
                editor_session_agent_id: Some(lead_session_agent.id),
                reason: Some("workflow_generate".to_string()),
                plan_json: plan_json.to_string(),
                plan_hash,
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await?;

        // Build preview projection
        let session_agents =
            ChatSessionAgent::find_all_for_session(pool, session.id).await?;
        let agents = load_agents_for_session(pool, &session_agents).await?;
        let agent_views: Vec<super::workflow_runtime::WorkflowCardAgent> = session_agents
            .iter()
            .filter_map(|sa| {
                let agent = agents.iter().find(|a| a.id == sa.agent_id)?;
                Some(super::workflow_runtime::WorkflowCardAgent {
                    session_agent_id: sa.id.to_string(),
                    agent_id: agent.id.to_string(),
                    name: agent.name.clone(),
                })
            })
            .collect();

        let step_views: Vec<super::workflow_runtime::WorkflowCardStep> = parsed_plan
            .nodes
            .iter()
            .map(|n| {
                let step_type_str = if n.data.step_type.is_empty() {
                    "task".to_string()
                } else {
                    n.data.step_type.to_lowercase()
                };
                super::workflow_runtime::WorkflowCardStep {
                    id: n.id.clone(),
                    title: n.data.title.clone(),
                    step_type: step_type_str,
                    status: "pending".to_string(),
                    agent_name: n.data.agent_id.clone(),
                    summary_text: None,
                }
            })
            .collect();

        let preview = super::workflow_runtime::WorkflowCardProjection {
            execution_id: None,
            plan_id: plan.id.to_string(),
            revision_id: revision.id.to_string(),
            title: plan.title.clone(),
            goal: plan
                .summary_text
                .clone()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| plan.title.clone()),
            state: super::workflow_runtime::WorkflowCardState::PreviewReady,
            execution_status: "preview".to_string(),
            error_message: None,
            completed_step_count: 0,
            total_step_count: parsed_plan.nodes.len(),
            result_summary: None,
            outputs: Vec::new(),
            agents: agent_views,
            steps: step_views,
            plan: parsed_plan,
            started_at: None,
            completed_at: None,
            validation_errors: None,
        };

        let card_meta = serde_json::json!({
            "card_type": "workflow_plan",
            "workflow_plan_id": plan.id,
            "active_revision_id": revision.id,
            "display_state": "preview_ready",
            "workflow_card": serde_json::to_value(&preview)?,
        });

        let message = chat::create_message(
            pool,
            session.id,
            ChatSenderType::System,
            None,
            "Workflow Plan".to_string(),
            Some(card_meta),
        )
        .await?;
        chat_runner.emit_message_new(message.session_id, message.clone());

        // Update plan with the card message id for later reference (e.g. execute_plan)
        let plan =
            WorkflowPlan::update_workflow_card_message_id(pool, plan.id, message.id).await?;

        Ok((plan, revision, message))
    }

    // -----------------------------------------------------------------------
    // Execute API: create execution from an existing ready plan (idempotent)
    // -----------------------------------------------------------------------

    /// Execute a plan that is in `ready` status.
    /// Idempotent: if an active execution already exists for this plan, returns it.
    pub async fn execute_plan(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        plan_id: Uuid,
    ) -> Result<BootstrapResult, OrchestratorError> {
        let plan = WorkflowPlan::find_by_id(pool, plan_id)
            .await?
            .ok_or_else(|| OrchestratorError::NotFound(format!("plan {} 未找到", plan_id)))?;

        if plan.status != WorkflowPlanStatus::Ready {
            return Err(OrchestratorError::IllegalTransition(format!(
                "plan {} status is {:?}, expected Ready",
                plan_id, plan.status
            )));
        }

        // Idempotent check: if active execution exists for this plan, return early
        let active_executions =
            WorkflowExecution::find_active_by_session(pool, plan.session_id).await?;
        for existing in &active_executions {
            if existing.plan_id == plan_id {
                // Already executing this plan
                let steps = WorkflowStep::find_by_execution(pool, existing.id).await?;
                let edges = WorkflowStepEdge::find_by_execution(pool, existing.id).await?;
                let agent_sessions =
                    WorkflowAgentSession::find_by_execution(pool, existing.id).await?;
                let round = existing
                    .active_round_id
                    .and_then(|_| {
                        // We can't do async in and_then easily, so skip
                        None::<WorkflowRound>
                    });
                let events = WorkflowEvent::find_by_execution(pool, existing.id).await?;
                return Ok(BootstrapResult {
                    execution: existing.clone(),
                    round,
                    steps,
                    edges,
                    agent_sessions,
                    events,
                    failed: false,
                    failure_reason: None,
                });
            }
        }
        if !active_executions.is_empty() {
            return Err(OrchestratorError::IllegalTransition(
                "another workflow execution is already active in this session".to_string(),
            ));
        }

        let revision = WorkflowPlanRevision::find_latest_by_plan(pool, plan_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("plan {} 缺少 revision", plan_id))
            })?;

        let session = ChatSession::find_by_id(pool, plan.session_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("session {} 未找到", plan.session_id))
            })?;
        let session_agents =
            ChatSessionAgent::find_all_for_session(pool, session.id).await?;
        let agents = load_agents_for_session(pool, &session_agents).await?;

        let lead_session_agent_id = plan
            .created_by_session_agent_id
            .or_else(|| session_agents.first().map(|sa| sa.id));

        let valid_agent_ids: Vec<String> =
            agents.iter().map(|a| a.id.to_string()).collect();
        let agent_id_map: HashMap<String, Uuid> = session_agents
            .iter()
            .map(|sa| (sa.agent_id.to_string(), sa.id))
            .collect();

        let bootstrap = Self::bootstrap_execution(
            pool,
            &plan,
            &revision,
            lead_session_agent_id,
            &valid_agent_ids,
            &agent_id_map,
        )
        .await?;

        // Update the workflow card if it exists
        if let Some(card_msg_id) = plan.workflow_card_message_id {
            let _ = WorkflowExecution::update_workflow_card_message_id(
                pool,
                bootstrap.execution.id,
                card_msg_id,
            )
            .await;

            // Refresh the card to show running state
            Self::refresh_workflow_card(
                pool,
                chat_runner,
                &bootstrap.execution,
                &plan,
                &revision,
                &session_agents,
                &agents,
                bootstrap.failure_reason.clone(),
            )
            .await?;
        }

        Ok(bootstrap)
    }

    // -----------------------------------------------------------------------
    // Pause / Interrupt controls
    // -----------------------------------------------------------------------

    /// Pause all running steps in the execution.
    pub async fn pause_all(
        pool: &SqlitePool,
        execution_id: Uuid,
    ) -> Result<WorkflowExecution, OrchestratorError> {
        let execution = WorkflowExecution::find_by_id(pool, execution_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("execution {} 未找到", execution_id))
            })?;

        if execution.status != WorkflowExecutionStatus::Running {
            return Err(OrchestratorError::IllegalTransition(format!(
                "cannot pause: execution is {:?}",
                execution.status
            )));
        }

        // Transition to pausing
        let tr = reducer::transition_execution(
            pool,
            &execution,
            WorkflowExecutionStatus::Pausing,
        )
        .await?;

        // For MVP, immediately go to paused (no async convergence needed yet)
        let tr = reducer::transition_execution(
            pool,
            &tr.entity,
            WorkflowExecutionStatus::Paused,
        )
        .await?;

        Ok(tr.entity)
    }

    /// Interrupt a specific step.
    pub async fn interrupt_step(
        pool: &SqlitePool,
        execution_id: Uuid,
        step_id: Uuid,
    ) -> Result<WorkflowStep, OrchestratorError> {
        let execution = WorkflowExecution::find_by_id(pool, execution_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("execution {} 未找到", execution_id))
            })?;

        let step = WorkflowStep::find_by_id(pool, step_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("step {} 未找到", step_id))
            })?;

        if step.status != WorkflowStepStatus::Running {
            return Err(OrchestratorError::IllegalTransition(format!(
                "cannot interrupt: step is {:?}",
                step.status
            )));
        }

        let tr = reducer::transition_step(
            pool,
            &execution,
            &step,
            WorkflowStepStatus::InterruptRequested,
        )
        .await?;

        // For MVP, immediately transition to interrupted
        let tr = reducer::transition_step(
            pool,
            &execution,
            &tr.entity,
            WorkflowStepStatus::Interrupted,
        )
        .await?;

        Ok(tr.entity)
    }
}

/// Bootstrap 结果
#[derive(Debug)]
pub struct BootstrapResult {
    pub execution: WorkflowExecution,
    pub round: Option<WorkflowRound>,
    pub steps: Vec<WorkflowStep>,
    pub edges: Vec<WorkflowStepEdge>,
    pub agent_sessions: Vec<WorkflowAgentSession>,
    pub events: Vec<WorkflowEvent>,
    pub failed: bool,
    pub failure_reason: Option<String>,
}
