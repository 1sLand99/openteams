export const WORKFLOW_GRAPH_UPDATED_EVENT =
  'openteams:workflow-graph-updated';

export type WorkflowGraphUpdatedDetail = {
  sessionId: string;
  executionId: string;
  graphVersion: string;
  reason: string;
  changedStepIds: string[];
};

export function notifyWorkflowGraphUpdated(
  detail: WorkflowGraphUpdatedDetail,
) {
  if (typeof window === 'undefined') return;
  window.dispatchEvent(
    new CustomEvent<WorkflowGraphUpdatedDetail>(
      WORKFLOW_GRAPH_UPDATED_EVENT,
      { detail },
    ),
  );
}
