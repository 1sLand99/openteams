#[derive(Debug)]
pub(crate) enum LoopOutcome {
    Progressed,
    Completed,
    Parked,
    Failed(String),
}

pub(crate) struct LoopExecutor<'a> {
    pub db: &'a DBService,
    pub pool: &'a SqlitePool,
    pub chat_runner: &'a ChatRunner,
    pub execution: &'a WorkflowExecution,
    pub workflow_agent_sessions: &'a [WorkflowAgentSession],
    pub session: &'a ChatSession,
    pub session_agents: &'a [ChatSessionAgent],
    pub agents: &'a [ChatAgent],
    pub plan: &'a WorkflowPlan,
}

enum LoopReviewDecision {
    Passed,
    PassedByUserWaiver {
        feedback: String,
        review_step: Box<WorkflowStep>,
    },
    Rejected {
        feedback: String,
        feedback_targets: Vec<LoopFeedbackTarget>,
    },
    LimitReached {
        feedback: String,
        review_attempt: i32,
    },
}

#[derive(Debug, Clone)]
struct LoopFeedbackTarget {
    step: WorkflowStep,
    issue_scope_id: String,
    feedback: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectedLoopReviewDisposition {
    PassedByUserWaiver,
    NeedsSkippedDecision,
    LimitReached,
    Retry,
}
