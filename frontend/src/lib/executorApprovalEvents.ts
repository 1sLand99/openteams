export const EXECUTOR_APPROVAL_CHANGED_EVENT =
  "openteams:executor-approval-changed";

export type ExecutorApprovalChangedDetail = {
  sessionId: string;
};

export const notifyExecutorApprovalChanged = (sessionId: string) => {
  window.dispatchEvent(
    new CustomEvent<ExecutorApprovalChangedDetail>(
      EXECUTOR_APPROVAL_CHANGED_EVENT,
      { detail: { sessionId } },
    ),
  );
};
