#![allow(clippy::too_many_arguments)]

use std::collections::{HashMap, HashSet};

use db::{
    DBService,
    models::{
        chat_agent::ChatAgent,
        chat_session::ChatSession,
        chat_session_agent::ChatSessionAgent,
        workflow_agent_session::{CreateWorkflowAgentSession, WorkflowAgentSession},
        workflow_event::{CreateWorkflowEvent, WorkflowEvent},
        workflow_execution::WorkflowExecution,
        workflow_iteration_feedback::{CreateWorkflowIterationFeedback, WorkflowIterationFeedback},
        workflow_loop::{CreateWorkflowLoop, WorkflowLoop},
        workflow_plan::WorkflowPlan,
        workflow_plan_revision::{CreateWorkflowPlanRevision, WorkflowPlanRevision},
        workflow_round::{CreateWorkflowRound, WorkflowRound},
        workflow_step::{CreateWorkflowStep, WorkflowStep},
        workflow_step_edge::{CreateWorkflowStepEdge, WorkflowStepEdge},
        workflow_types::{
            WorkflowEventType, WorkflowPlanJson, WorkflowRevisionEditor, WorkflowRoundStatus,
            WorkflowStepStatus, WorkflowStepType, WorkflowValidationStatus, to_workflow_wire_value,
        },
    },
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use ts_rs::TS;
use utils::assets::config_path;
use uuid::Uuid;

use super::{
    chat_runner::ChatRunner,
    config,
    workflow_compiler::WorkflowCompiler,
    workflow_orchestrator::{
        OrchestratorError, WorkflowOrchestrator, reducer, workflow_agent_id_map,
        workflow_agent_session_role_for_assignment, workflow_plan_agent_id,
        workflow_valid_agent_ids,
    },
    workflow_runtime::{
        MAX_DYNAMIC_CONTENT_BUDGET_BYTES, PLAN_SCHEMA_DEFINITION, PLAN_SKILLS_GUIDANCE,
        PLAN_STABLE_OUTPUT_CONTRACT, PLAN_STATIC_CONSTRAINTS, PromptDataBuilder, SummaryPayload,
        WorkflowPlanningAgent, build_workflow_planning_agents, extract_json_payload,
        maybe_prepend_safety_preamble, parse_summary_payload,
        resolve_workflow_response_language_instruction, run_workflow_agent_prompt,
    },
};

include!("types.rs");
include!("control.rs");
include!("aggregation.rs");
include!("prompts.rs");
include!("tests.rs");
