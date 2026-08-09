use std::fmt::Debug;

use serde::{Deserialize, Deserializer, Serialize};
use sqlx::Type;
use ts_rs::TS;

/// Deserializes version field accepting both integer (e.g. `1`) and string (e.g. `"1.0.0"`)
fn deserialize_version_flexible<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de;

    struct VersionVisitor;

    impl<'de> de::Visitor<'de> for VersionVisitor {
        type Value = String;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("an integer or string version")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
            Ok(v.to_string())
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<String, E> {
            Ok(v)
        }
    }

    deserializer.deserialize_any(VersionVisitor)
}

pub fn to_workflow_wire_value<T>(value: &T) -> String
where
    T: Serialize + Debug,
{
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| format!("{value:?}").to_lowercase())
}

// ---------------------------------------------------------------------------
// Plan-level enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_plan_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowPlanStatus {
    Draft,
    Ready,
    Superseded,
    Cancelled,
}

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_validation_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowValidationStatus {
    Pending,
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_revision_editor", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowRevisionEditor {
    Lead,
    System,
}

// ---------------------------------------------------------------------------
// Execution-level enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_execution_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowExecutionStatus {
    Pending,
    Running,
    Failed,
    Paused,
    Recompiling,
    Completed,
    Waiting,
}

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_round_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowRoundStatus {
    Running,
    #[sqlx(rename = "waiting_user_acceptance")]
    #[serde(rename = "waiting_user_acceptance")]
    WaitingUserAcceptance,
    Accepted,
    Rejected,
    Archived,
}

// ---------------------------------------------------------------------------
// Step-level enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_step_type", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowStepType {
    Task,
    Review,
    Result,
}

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_step_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowStepStatus {
    Pending,
    Ready,
    Running,
    #[sqlx(rename = "pre_completed")]
    #[serde(rename = "pre_completed")]
    PreCompleted,
    #[sqlx(rename = "interrupt_requested")]
    #[serde(rename = "interrupt_requested")]
    InterruptRequested,
    Interrupted,
    #[sqlx(rename = "waiting_input")]
    #[serde(rename = "waiting_input")]
    WaitingInput,
    #[sqlx(rename = "waiting_review")]
    #[serde(rename = "waiting_review")]
    WaitingReview,
    Blocked,
    Revising,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_loop_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[ts(use_ts_enum)]
pub enum WorkflowLoopStatus {
    Pending,
    Running,
    WaitingReview,
    Passed,
    Rejected,
    WaitingUser,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "review_verdict", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum ReviewVerdict {
    Approved,
    Rejected,
}

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "reviewer_type", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum ReviewerType {
    Lead,
    Reviewer,
    User,
}

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_edge_kind", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowEdgeKind {
    Hard,
    /// Kept so previously persisted compiled graphs can still be deserialized.
    /// New plan submissions reject soft edges until the scheduler implements
    /// distinct soft-dependency semantics.
    Soft,
}

// ---------------------------------------------------------------------------
// Agent session enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_agent_session_role", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowAgentSessionRole {
    Lead,
    Worker,
    Reviewer,
}

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_agent_session_state", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[ts(use_ts_enum)]
pub enum WorkflowAgentSessionState {
    Idle,
    Running,
    #[sqlx(rename = "interrupt_requested")]
    #[serde(rename = "interrupt_requested")]
    InterruptRequested,
    Interrupted,
    Paused,
    Completed,
    Failed,
    Expired,
}

// ---------------------------------------------------------------------------
// Event enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Type, Serialize, Deserialize, PartialEq, TS)]
#[sqlx(type_name = "workflow_event_type", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
#[ts(use_ts_enum)]
pub enum WorkflowEventType {
    ExecutionCreated,
    ExecutionRunning,
    ExecutionFailed,
    ExecutionCompleted,
    ExecutionPaused,
    ExecutionWaiting,
    RoundStarted,
    RoundResultReady,
    UserAccepted,
    UserRejected,
    RoundArchived,
    PlanRevisionCreated,
    PlanRecompiled,
    StepStatusChanged,
    AgentSessionStateChanged,
    StepLeadReviewStarted,
    StepLeadReviewPassed,
    StepLeadReviewRejected,
    StepUserReviewStarted,
    StepUserReviewPassed,
    StepUserReviewRejected,
    LoopStarted,
    LoopRetrying,
    LoopWaitingUser,
    LoopUserDecisionRecorded,
    LoopPassed,
    LoopFailed,
    IterationFeedbackReceived,
    IterationNewPlanGenerated,
}

// ---------------------------------------------------------------------------
// Acceptance criteria
// ---------------------------------------------------------------------------

/// Tiered acceptance criteria for plan nodes.
///
/// - `required`: objective, externally verifiable items that must all pass.
/// - `partial`: items that may fail for external reasons (environment,
///   credentials, third-party outages) given a documented justification.
/// - `recommended`: nice-to-have items that never block approval.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, TS)]
pub struct AcceptanceCriteria {
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub partial: Vec<String>,
    #[serde(default)]
    pub recommended: Vec<String>,
}

impl AcceptanceCriteria {
    /// All criteria flattened in tier order (required, partial, recommended).
    pub fn all(&self) -> Vec<String> {
        self.required
            .iter()
            .chain(self.partial.iter())
            .chain(self.recommended.iter())
            .cloned()
            .collect()
    }

    /// All criteria with their tier, in tier order.
    pub fn leveled(&self) -> Vec<(AcceptanceCriterionLevel, String)> {
        self.required
            .iter()
            .map(|item| (AcceptanceCriterionLevel::Required, item.clone()))
            .chain(
                self.partial
                    .iter()
                    .map(|item| (AcceptanceCriterionLevel::Partial, item.clone())),
            )
            .chain(
                self.recommended
                    .iter()
                    .map(|item| (AcceptanceCriterionLevel::Recommended, item.clone())),
            )
            .collect()
    }

    /// Whether `required` contains at least one non-blank item.
    pub fn has_non_empty_required(&self) -> bool {
        self.required.iter().any(|item| !item.trim().is_empty())
    }
}

/// Acceptance criterion tier, mirroring the [`AcceptanceCriteria`] structure.
/// Review protocols carry this on every `acceptance_results` item so verdict
/// consistency can be checked mechanically.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceCriterionLevel {
    /// Must pass for approval.
    #[default]
    Required,
    /// May fail with a documented external justification.
    Partial,
    /// Nice to have; never blocks approval.
    Recommended,
}

/// Legacy plans stored `acceptance` as a flat string array; new plans use the
/// tiered object. Both forms deserialize into [`AcceptanceCriteria`]; only the
/// tiered form is written back out.
#[derive(Deserialize)]
#[serde(untagged)]
enum AcceptanceCriteriaWire {
    Tiered(AcceptanceCriteria),
    Legacy(Vec<String>),
}

impl From<AcceptanceCriteriaWire> for AcceptanceCriteria {
    fn from(wire: AcceptanceCriteriaWire) -> Self {
        match wire {
            AcceptanceCriteriaWire::Tiered(criteria) => criteria,
            AcceptanceCriteriaWire::Legacy(items) => Self {
                required: items,
                ..Self::default()
            },
        }
    }
}

fn deserialize_acceptance_compat<'de, D>(
    deserializer: D,
) -> Result<Option<AcceptanceCriteria>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<AcceptanceCriteriaWire>::deserialize(deserializer)?.map(Into::into))
}

// ---------------------------------------------------------------------------
// Workflow Plan JSON types (React Flow compatible)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct WorkflowPlanJson {
    #[serde(deserialize_with = "deserialize_version_flexible")]
    pub version: String,
    pub title: String,
    pub goal: String,
    pub agents: WorkflowPlanAgents,
    #[serde(default)]
    pub globals: Option<WorkflowPlanGlobals>,
    #[serde(default)]
    pub viewport: Option<WorkflowPlanViewport>,
    pub nodes: Vec<WorkflowPlanNode>,
    pub edges: Vec<WorkflowPlanEdge>,
    #[serde(default)]
    pub loops: Option<Vec<WorkflowLoopDef>>,
    #[serde(default)]
    pub policies: Option<WorkflowPlanPolicies>,
}

impl WorkflowPlanJson {
    pub fn plan_schema_version(&self) -> Result<i32, String> {
        let normalized = self.version.trim().trim_start_matches('v');
        let major = normalized.split('.').next().unwrap_or_default().trim();

        if major.is_empty() {
            return Err("Workflow plan version cannot be empty.".to_string());
        }

        major.parse::<i32>().map_err(|_| {
            format!(
                "Invalid workflow plan version '{}'. Expected an integer-like string such as '1' or '1.0.0'.",
                self.version
            )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct WorkflowPlanAgents {
    pub lead: String,
    pub available: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct WorkflowPlanGlobals {
    #[serde(default = "default_interrupt_mode")]
    pub interrupt_mode: String,
    #[serde(default = "default_retry")]
    pub default_retry: u32,
    #[serde(default = "default_true")]
    pub global_pause_supported: bool,
}

fn default_interrupt_mode() -> String {
    "cooperative".to_string()
}

fn default_retry() -> u32 {
    DEFAULT_WORKFLOW_RETRY
}

/// Retry budgets count rework attempts after the initial execution/review.
/// Zero therefore means one initial attempt and no automatic rework.
pub const DEFAULT_WORKFLOW_RETRY: u32 = 3;
pub const MAX_WORKFLOW_RETRY: u32 = 10;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct WorkflowPlanViewport {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default = "default_zoom")]
    pub zoom: f64,
}

fn default_zoom() -> f64 {
    1.0
}

impl Default for WorkflowPlanViewport {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            zoom: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct WorkflowPlanNode {
    pub id: String,
    #[serde(rename = "type")]
    pub node_type: String,
    #[serde(default)]
    pub position: WorkflowNodePosition,
    pub data: WorkflowNodeData,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct WorkflowNodePosition {
    pub x: f64,
    pub y: f64,
}

impl Default for WorkflowNodePosition {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0 }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowNodeData {
    pub step_type: String,
    #[serde(default)]
    pub agent_id: Option<String>,
    pub title: String,
    pub instructions: String,
    #[serde(default)]
    pub acceptance: Option<AcceptanceCriteria>,
    #[serde(default)]
    pub outputs: Option<Vec<String>>,
    /// Self-check items the executor must verify before reporting completion
    /// (task nodes only). Replaces the legacy `checklist` field.
    #[serde(default)]
    pub self_check: Option<Vec<String>>,
    /// Verification/test commands or methods used to prove the task is done
    /// (task nodes only).
    #[serde(default)]
    pub verification_commands: Option<Vec<String>>,
    /// Evidence the task must produce on completion (task nodes only).
    #[serde(default)]
    pub completion_evidence: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub interruptible: bool,
    #[serde(default)]
    pub max_retry: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub loop_key: Option<String>,
    #[serde(default)]
    pub review_scope: Option<Vec<String>>,
}

/// Wire shape kept for backward-compatible deserialization: legacy plans may
/// still carry a flat string-array `acceptance` and a `checklist` field. The
/// legacy array maps into `acceptance.required`; `checklist` items merge into
/// `self_check`. Neither legacy form is written back out.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowNodeDataWire {
    step_type: String,
    #[serde(default)]
    agent_id: Option<String>,
    title: String,
    instructions: String,
    #[serde(default)]
    acceptance: Option<AcceptanceCriteriaWire>,
    #[serde(default)]
    outputs: Option<Vec<String>>,
    #[serde(default)]
    checklist: Option<Vec<String>>,
    #[serde(default)]
    self_check: Option<Vec<String>>,
    #[serde(default)]
    verification_commands: Option<Vec<String>>,
    #[serde(default)]
    completion_evidence: Option<Vec<String>>,
    #[serde(default = "default_true")]
    interruptible: bool,
    #[serde(default)]
    max_retry: Option<u32>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    loop_key: Option<String>,
    #[serde(default)]
    review_scope: Option<Vec<String>>,
}

impl From<WorkflowNodeDataWire> for WorkflowNodeData {
    fn from(wire: WorkflowNodeDataWire) -> Self {
        let self_check = match (wire.self_check, wire.checklist) {
            (Some(mut self_check), Some(legacy)) => {
                self_check.extend(legacy);
                Some(self_check)
            }
            (Some(self_check), None) => Some(self_check),
            (None, legacy) => legacy,
        };
        Self {
            step_type: wire.step_type,
            agent_id: wire.agent_id,
            title: wire.title,
            instructions: wire.instructions,
            acceptance: wire.acceptance.map(Into::into),
            outputs: wire.outputs,
            self_check,
            verification_commands: wire.verification_commands,
            completion_evidence: wire.completion_evidence,
            interruptible: wire.interruptible,
            max_retry: wire.max_retry,
            status: wire.status,
            loop_key: wire.loop_key,
            review_scope: wire.review_scope,
        }
    }
}

impl<'de> Deserialize<'de> for WorkflowNodeData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        WorkflowNodeDataWire::deserialize(deserializer).map(Into::into)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowLoopDef {
    pub loop_key: String,
    pub member_steps: Vec<String>,
    pub review_step: String,
    #[serde(default)]
    pub max_retry: Option<u32>,
    #[serde(default)]
    pub user_review_required: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct WorkflowPlanEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    #[serde(rename = "type", default)]
    pub edge_type: Option<String>,
    #[serde(default)]
    pub data: Option<WorkflowEdgeData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct WorkflowEdgeData {
    #[serde(default = "default_edge_kind")]
    pub kind: String,
}

fn default_edge_kind() -> String {
    "hard".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct WorkflowPlanPolicies {
    #[serde(default)]
    pub approval_required_on: Option<Vec<String>>,
    #[serde(default)]
    pub permission_required_on: Option<Vec<String>>,
    #[serde(default)]
    pub on_failure: Option<String>,
    #[serde(default = "default_true")]
    pub allow_plan_revision: bool,
}

// ---------------------------------------------------------------------------
// Compiled graph DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct CompiledGraph {
    pub plan_hash: String,
    pub compiled_graph_hash: String,
    pub steps: Vec<CompiledStep>,
    pub edges: Vec<CompiledEdge>,
    pub ready_step_keys: Vec<String>,
    #[serde(default)]
    pub loops: Option<Vec<CompiledLoopDef>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct CompiledStep {
    pub step_key: String,
    pub step_type: WorkflowStepType,
    pub title: String,
    pub instructions: String,
    pub assigned_agent_id: Option<String>,
    /// Tiered acceptance criteria. Legacy compiled graphs stored this as a
    /// flat string array; deserialization maps that form into `required`.
    #[serde(default, deserialize_with = "deserialize_acceptance_compat")]
    pub acceptance: Option<AcceptanceCriteria>,
    pub outputs: Option<Vec<String>>,
    pub interruptible: bool,
    pub max_retry: u32,
    pub display_order: i32,
    #[serde(default)]
    pub loop_key: Option<String>,
    #[serde(default)]
    pub review_scope: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct CompiledEdge {
    pub edge_id: String,
    pub from_step_key: String,
    pub to_step_key: String,
    pub edge_kind: WorkflowEdgeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct CompiledLoopDef {
    pub loop_key: String,
    pub member_step_keys: Vec<String>,
    pub review_step_key: String,
    #[serde(default)]
    pub review_scope_step_keys: Vec<String>,
    pub max_retry: u32,
    pub user_review_required: bool,
}
