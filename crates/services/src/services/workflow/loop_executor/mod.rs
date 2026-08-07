use std::collections::{HashMap, HashSet};

use db::{
    DBService,
    models::{
        chat_agent::ChatAgent,
        chat_session::ChatSession,
        chat_session_agent::ChatSessionAgent,
        workflow_agent_session::WorkflowAgentSession,
        workflow_event::{CreateWorkflowEvent, WorkflowEvent},
        workflow_execution::WorkflowExecution,
        workflow_loop::WorkflowLoop,
        workflow_plan::WorkflowPlan,
        workflow_step::WorkflowStep,
        workflow_transcript::{CreateWorkflowTranscript, WorkflowTranscript},
        workflow_types::{
            AcceptanceCriterionLevel, CompiledLoopDef, ReviewVerdict, ReviewerType,
            WorkflowAgentSessionRole, WorkflowEventType, WorkflowLoopStatus, WorkflowStepStatus,
            to_workflow_wire_value,
        },
    },
};
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use utils::assets::config_path;
use uuid::Uuid;

use super::{
    chat_runner::ChatRunner,
    config, workflow_analytics,
    workflow_orchestrator::{
        OrchestratorError, WorkflowOrchestrator, reducer, resolve_step_workflow_session,
    },
    workflow_runtime::{
        SummaryPayload, WORKFLOW_PROTOCOL_PARSE_MAX_RETRIES, WorkflowRevisionFeedbackSource,
        build_workflow_protocol_retry_prompt, parse_summary_payload,
        resolve_workflow_response_language_instruction, run_workflow_step_agent_follow_up,
        run_workflow_step_agent_prompt, should_retry_workflow_protocol_parse_failure,
    },
};
use crate::services::inbox::InboxService;

pub mod prompts;
pub mod protocol;

#[derive(Debug, Clone)]
struct LoopReviewPromptStepInput {
    step_key: String,
    title: String,
    instructions: String,
    acceptance: db::models::workflow_types::AcceptanceCriteria,
    summary: String,
    outputs: Vec<String>,
    evidence: Vec<String>,
    predecessor_handoffs: Vec<String>,
    user_skip_waiver: Option<String>,
}

#[derive(Debug, Clone)]
struct LoopReviewPromptContext {
    reviewer_name: String,
    reviewer_role: String,
    review_step_instructions: String,
    current_round: i32,
    loop_retry_count: i32,
    retry_budget: i32,
    review_scope_edges: Vec<String>,
}

use protocol::LoopReviewProtocolMessage;

include!("types.rs");
include!("review.rs");
include!("executor.rs");
include!("tests.rs");
