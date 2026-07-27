import type { ChatExecutorApprovalRequest } from "../../../shared/types";

export const EXECUTOR_APPROVAL_CHANGED_EVENT =
  "openteams:executor-approval-changed";

export type ExecutorApprovalChangedType =
  | "executor_approval_requested"
  | "executor_approval_resolved"
  | "executor_approval_cancelled"
  | "executor_approval_expired";

export type ExecutorApprovalChangedDetail = {
  sessionId: string;
  type: ExecutorApprovalChangedType;
  requestId: string;
  request: ChatExecutorApprovalRequest;
};

export const notifyExecutorApprovalChanged = (
  detail: ExecutorApprovalChangedDetail,
) => {
  window.dispatchEvent(
    new CustomEvent<ExecutorApprovalChangedDetail>(
      EXECUTOR_APPROVAL_CHANGED_EVENT,
      { detail },
    ),
  );
};
