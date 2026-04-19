use std::collections::HashMap;

use axum::{
    Extension, Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json as ResponseJson, Response},
};
use db::models::{
    chat_agent::ChatAgent,
    chat_message::ChatMessage,
    chat_session::ChatSession,
    chat_session_agent::ChatSessionAgent,
    workflow_execution::WorkflowExecution,
    workflow_plan::{CreateWorkflowPlan, WorkflowPlan},
    workflow_plan_revision::{CreateWorkflowPlanRevision, WorkflowPlanRevision},
    workflow_types::{WorkflowPlanStatus, WorkflowRevisionEditor, WorkflowValidationStatus},
};
use deployment::Deployment;
use serde::{Deserialize, Serialize};
use services::services::{
    workflow_compiler::WorkflowCompiler,
    workflow_orchestrator::WorkflowOrchestrator,
    workflow_runtime::{
        WorkflowCardAgent, build_plan_generation_prompt, extract_json_payload,
        resolve_workflow_goal, run_workflow_agent_prompt,
    },
    workflow_validator,
};
use ts_rs::TS;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError};

#[derive(Debug, Deserialize, TS)]
pub struct GeneratePlanAndRunRequest {
    pub user_goal: Option<String>,
}

#[derive(Debug, Serialize, TS)]
pub struct GeneratePlanAndRunResponse {
    pub execution_id: Uuid,
    pub workflow_card_message: db::models::chat_message::ChatMessage,
}

pub async fn generate_plan_and_run(
    Extension(session): Extension<ChatSession>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<GeneratePlanAndRunRequest>,
) -> Result<Response, ApiError> {
    let pool = &deployment.db().pool;

    if !WorkflowExecution::find_active_by_session(pool, session.id)
        .await?
        .is_empty()
    {
        return Ok((
            StatusCode::CONFLICT,
            ResponseJson(ApiResponse::<GeneratePlanAndRunResponse>::error(
                "A workflow execution is already active in this session.",
            )),
        )
            .into_response());
    }

    let messages = ChatMessage::find_by_session_id(pool, session.id, None).await?;
    let user_goal =
        resolve_workflow_goal(payload.user_goal.as_deref(), &messages).ok_or_else(|| {
            ApiError::BadRequest(
                "Workflow goal is required. Add a user message first or provide user_goal."
                    .to_string(),
            )
        })?;
    let source_message_id = messages
        .iter()
        .rev()
        .find(|message| message.sender_type == db::models::chat_message::ChatSenderType::User)
        .map(|message| message.id);

    let session_agents = ChatSessionAgent::find_all_for_session(pool, session.id).await?;
    if session_agents.is_empty() {
        return Err(ApiError::BadRequest(
            "At least one session agent is required before running a workflow.".to_string(),
        ));
    }

    let mut agents = Vec::with_capacity(session_agents.len());
    for session_agent in &session_agents {
        let agent = ChatAgent::find_by_id(pool, session_agent.agent_id)
            .await?
            .ok_or_else(|| {
                ApiError::BadRequest("Session agent is missing its agent record.".to_string())
            })?;
        agents.push(agent);
    }

    let lead_session_agent = session_agents
        .first()
        .ok_or_else(|| ApiError::BadRequest("Lead session agent was not found.".to_string()))?;
    let lead_agent = agents
        .iter()
        .find(|agent| agent.id == lead_session_agent.agent_id)
        .ok_or_else(|| ApiError::BadRequest("Lead agent was not found.".to_string()))?;

    let available_agents = session_agents
        .iter()
        .filter_map(|session_agent| {
            let agent = agents
                .iter()
                .find(|agent| agent.id == session_agent.agent_id)?;
            Some(WorkflowCardAgent {
                session_agent_id: session_agent.id.to_string(),
                agent_id: agent.id.to_string(),
                name: agent.name.clone(),
            })
        })
        .collect::<Vec<_>>();

    let prompt =
        build_plan_generation_prompt(&user_goal, &lead_agent.id.to_string(), &available_agents);
    let raw_plan_output = run_workflow_agent_prompt(
        deployment.db(),
        &session,
        lead_agent,
        lead_session_agent,
        &prompt,
    )
    .await
    .map_err(|err| ApiError::BadRequest(err.to_string()))?;
    let plan_json = extract_json_payload(&raw_plan_output).ok_or_else(|| {
        ApiError::BadRequest("Lead agent did not return a workflow JSON object.".to_string())
    })?;

    let parsed_plan: db::models::workflow_types::WorkflowPlanJson =
        serde_json::from_str(&plan_json).map_err(|err| {
            ApiError::BadRequest(format!("Lead agent returned invalid workflow JSON: {err}"))
        })?;
    let valid_agent_ids = agents
        .iter()
        .map(|agent| agent.id.to_string())
        .collect::<Vec<_>>();
    let validation = workflow_validator::validate_plan(&parsed_plan, &valid_agent_ids);
    if !validation.is_valid {
        persist_invalid_plan(
            pool,
            session.id,
            source_message_id,
            lead_session_agent.id,
            &parsed_plan,
            &plan_json,
            &validation.errors,
        )
        .await?;

        let validation_message = validation
            .errors
            .iter()
            .map(|error| format!("{}: {}", error.field, error.message))
            .collect::<Vec<_>>()
            .join("; ");

        return Ok((
            StatusCode::BAD_REQUEST,
            ResponseJson(ApiResponse::<GeneratePlanAndRunResponse>::error(
                &validation_message,
            )),
        )
            .into_response());
    }

    let (plan, revision, workflow_card_message) =
        WorkflowOrchestrator::create_workflow_plan_and_card(
            pool,
            deployment.chat_runner(),
            &session,
            source_message_id,
            lead_session_agent,
            &plan_json,
        )
        .await
        .map_err(|err| ApiError::BadRequest(err.to_string()))?;

    let agent_id_map = session_agents
        .iter()
        .map(|session_agent| (session_agent.agent_id.to_string(), session_agent.id))
        .collect::<HashMap<_, _>>();
    let bootstrap = WorkflowOrchestrator::bootstrap_execution(
        pool,
        &plan,
        &revision,
        Some(lead_session_agent.id),
        &valid_agent_ids,
        &agent_id_map,
    )
    .await
    .map_err(|err| ApiError::BadRequest(err.to_string()))?;

    let execution = WorkflowExecution::update_workflow_card_message_id(
        pool,
        bootstrap.execution.id,
        workflow_card_message.id,
    )
    .await?;

    WorkflowOrchestrator::refresh_workflow_card(
        pool,
        deployment.chat_runner(),
        &execution,
        &plan,
        &revision,
        &session_agents,
        &agents,
        bootstrap.failure_reason.clone(),
    )
    .await
    .map_err(|err| ApiError::BadRequest(err.to_string()))?;

    let workflow_card_message = ChatMessage::find_by_id(pool, workflow_card_message.id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Workflow card message was not found.".to_string()))?;

    let deployment_clone = deployment.clone();
    let execution_id = execution.id;
    tokio::spawn(async move {
        if let Err(err) = WorkflowOrchestrator::wake_scheduler(
            deployment_clone.db(),
            deployment_clone.chat_runner(),
            execution_id,
        )
        .await
        {
            tracing::error!(execution_id = %execution_id, error = %err, "workflow scheduler failed");
        }
    });

    Ok((
        StatusCode::OK,
        ResponseJson(ApiResponse::<GeneratePlanAndRunResponse>::success(
            GeneratePlanAndRunResponse {
                execution_id: execution.id,
                workflow_card_message,
            },
        )),
    )
        .into_response())
}

// -----------------------------------------------------------------------
// Execute Plan (idempotent)
// -----------------------------------------------------------------------

#[derive(Debug, Serialize, TS)]
pub struct ExecutePlanResponse {
    pub execution_id: Uuid,
}

pub async fn execute_plan(
    Extension(session): Extension<ChatSession>,
    State(deployment): State<DeploymentImpl>,
    axum::extract::Path(plan_id): axum::extract::Path<Uuid>,
) -> Result<Response, ApiError> {
    let pool = &deployment.db().pool;

    // Verify the plan belongs to this session
    let plan = WorkflowPlan::find_by_id(pool, plan_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Plan not found.".to_string()))?;
    if plan.session_id != session.id {
        return Err(ApiError::BadRequest("Plan not found in this session.".to_string()));
    }

    let bootstrap = WorkflowOrchestrator::execute_plan(
        pool,
        deployment.chat_runner(),
        plan_id,
    )
    .await
    .map_err(|err| ApiError::BadRequest(err.to_string()))?;

    // Wake the scheduler to start executing steps
    let deployment_clone = deployment.clone();
    let execution_id = bootstrap.execution.id;
    tokio::spawn(async move {
        if let Err(err) = WorkflowOrchestrator::wake_scheduler(
            deployment_clone.db(),
            deployment_clone.chat_runner(),
            execution_id,
        )
        .await
        {
            tracing::error!(execution_id = %execution_id, error = %err, "workflow scheduler failed");
        }
    });

    Ok((
        StatusCode::OK,
        ResponseJson(ApiResponse::<ExecutePlanResponse>::success(
            ExecutePlanResponse {
                execution_id: bootstrap.execution.id,
            },
        )),
    )
        .into_response())
}

// -----------------------------------------------------------------------
// Pause All
// -----------------------------------------------------------------------

#[derive(Debug, Deserialize, TS)]
pub struct PauseAllRequest {
    pub execution_id: Uuid,
}

#[derive(Debug, Serialize, TS)]
pub struct PauseAllResponse {
    pub status: String,
}

pub async fn pause_all(
    Extension(session): Extension<ChatSession>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<PauseAllRequest>,
) -> Result<Response, ApiError> {
    let pool = &deployment.db().pool;

    let execution = WorkflowExecution::find_by_id(pool, payload.execution_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Execution not found.".to_string()))?;
    if execution.session_id != session.id {
        return Err(ApiError::BadRequest("Execution not found in this session.".to_string()));
    }

    let execution = WorkflowOrchestrator::pause_all(pool, payload.execution_id)
        .await
        .map_err(|err| ApiError::BadRequest(err.to_string()))?;

    Ok((
        StatusCode::OK,
        ResponseJson(ApiResponse::<PauseAllResponse>::success(PauseAllResponse {
            status: format!("{:?}", execution.status),
        })),
    )
        .into_response())
}

// -----------------------------------------------------------------------
// Interrupt Step
// -----------------------------------------------------------------------

#[derive(Debug, Deserialize, TS)]
pub struct InterruptStepRequest {
    pub execution_id: Uuid,
    pub step_id: Uuid,
}

#[derive(Debug, Serialize, TS)]
pub struct InterruptStepResponse {
    pub status: String,
}

pub async fn interrupt_step(
    Extension(session): Extension<ChatSession>,
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<InterruptStepRequest>,
) -> Result<Response, ApiError> {
    let pool = &deployment.db().pool;

    let execution = WorkflowExecution::find_by_id(pool, payload.execution_id)
        .await?
        .ok_or_else(|| ApiError::BadRequest("Execution not found.".to_string()))?;
    if execution.session_id != session.id {
        return Err(ApiError::BadRequest("Execution not found in this session.".to_string()));
    }

    let step = WorkflowOrchestrator::interrupt_step(
        pool,
        payload.execution_id,
        payload.step_id,
    )
    .await
    .map_err(|err| ApiError::BadRequest(err.to_string()))?;

    Ok((
        StatusCode::OK,
        ResponseJson(ApiResponse::<InterruptStepResponse>::success(
            InterruptStepResponse {
                status: format!("{:?}", step.status),
            },
        )),
    )
        .into_response())
}

async fn persist_invalid_plan(
    pool: &sqlx::SqlitePool,
    session_id: Uuid,
    source_message_id: Option<Uuid>,
    lead_session_agent_id: Uuid,
    parsed_plan: &db::models::workflow_types::WorkflowPlanJson,
    plan_json: &str,
    errors: &[workflow_validator::ValidationError],
) -> Result<(WorkflowPlan, WorkflowPlanRevision), ApiError> {
    let validation_errors_json = serde_json::to_string(errors).map_err(|err| {
        ApiError::BadRequest(format!("Failed to serialize validation errors: {err}"))
    })?;
    let plan_hash = WorkflowCompiler::compute_hash(parsed_plan);

    let plan = WorkflowPlan::create(
        pool,
        &CreateWorkflowPlan {
            session_id,
            source_message_id,
            created_by_session_agent_id: Some(lead_session_agent_id),
            title: parsed_plan.title.clone(),
            summary_text: Some(parsed_plan.goal.clone()),
            plan_json: plan_json.to_string(),
            plan_schema_version: parsed_plan.version as i32,
            plan_hash: plan_hash.clone(),
            validation_status: WorkflowValidationStatus::Invalid,
            validation_errors_json: Some(validation_errors_json.clone()),
        },
        Uuid::new_v4(),
    )
    .await?;
    let plan = WorkflowPlan::update_status(pool, plan.id, WorkflowPlanStatus::Draft).await?;

    let revision = WorkflowPlanRevision::create(
        pool,
        &CreateWorkflowPlanRevision {
            plan_id: plan.id,
            revision_no: 1,
            edited_by: WorkflowRevisionEditor::Lead,
            editor_session_agent_id: Some(lead_session_agent_id),
            reason: Some("generate-plan-and-run-invalid".to_string()),
            plan_json: plan_json.to_string(),
            plan_hash,
            validation_status: WorkflowValidationStatus::Invalid,
            validation_errors_json: Some(validation_errors_json),
        },
        Uuid::new_v4(),
    )
    .await?;

    Ok((plan, revision))
}
