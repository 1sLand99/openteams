const WORKFLOW_EXECUTION_TIMEOUT: Duration = Duration::from_secs(4800);
const WORKFLOW_DRAIN_TIMEOUT: Duration = Duration::from_millis(1000);
const WORKFLOW_RUNTIME_STREAM_TAIL_DRAIN_TIMEOUT: Duration = Duration::from_millis(350);
const WORKFLOW_SESSION_ID_DRAIN_TIMEOUT: Duration = Duration::from_millis(350);
const WORKFLOW_EXIT_SIGNAL_DRAIN_TIMEOUT: Duration = Duration::from_millis(350);
const WORKFLOW_REAP_TIMEOUT: Duration = Duration::from_secs(3);
const WORKFLOW_EXECUTOR_ERROR_MAX_CHARS: usize = 1600;
const WORKFLOW_EXECUTOR_ERROR_MAX_LINES: usize = 16;
pub const WORKFLOW_PROTOCOL_PARSE_MAX_RETRIES: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RunningStepKey {
    step_id: Uuid,
    retry_count: i32,
}

/// Global registry: workflow attempt → container-owned cancellation token.
///
/// The retry count is part of the key so a delayed cleanup or cancellation
/// request from an interrupted attempt cannot cancel a newly retried attempt
/// for the same workflow step.
static RUNNING_STEPS: Lazy<DashMap<RunningStepKey, CancellationToken>> =
    Lazy::new(DashMap::new);
static STEP_CANCEL_REQUESTS: Lazy<DashSet<RunningStepKey>> = Lazy::new(DashSet::new);
