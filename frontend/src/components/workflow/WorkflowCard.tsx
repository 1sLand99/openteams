import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { SkipForward } from 'lucide-react';
import { useAppTranslation } from '@/hooks/useAppTranslation';
import { useQuery } from '@/lib/queryCompat';
import { chatMessagesApi, workflowApi } from '@/lib/api';
import { notifySourceControlRefreshRequested } from '@/lib/sourceControlEvents';
import {
  WORKFLOW_GRAPH_UPDATED_EVENT,
  type WorkflowGraphUpdatedDetail,
} from '@/lib/workflowEvents';
import {
  shouldPollWorkflowProjection,
  WORKFLOW_CARD_REFETCH_INTERVAL_MS,
} from '@/lib/workflowRequestPolicy';
import type {
  UserIterationFeedbackRequest,
  WorkflowCardMessageType,
  WorkflowCardProjection,
  WorkflowPlanGenerationMeta,
  WorkflowTranscriptEntry,
} from '@/types';
import { useWorkspace } from '@/context/WorkspaceContext';
import { ConfirmationDialog } from '@/components/ConfirmationDialog';
import {
  clearInboxWorkflowFocus,
  getPendingInboxWorkflowFocus,
  INBOX_WORKFLOW_FOCUS_EVENT,
  type InboxWorkflowFocusTarget,
} from '@/lib/inboxNavigation';
import { ChatWorkflowCard } from './ChatWorkflowCard';
import {
  WorkflowReviewSettingsDialog,
  type WorkflowReviewSettingOverride,
} from './WorkflowReviewSettingsDialog';
import { toWorkflowFinalReviewAction } from './WorkflowFinalReviewCard';
import { WorkflowWindow } from './WorkflowWindow';
import {
  useCommandHandler,
  useShortcutScope,
} from '@/shortcuts/ShortcutProvider';

interface WorkflowCardProps {
  sessionId: string;
  messageId: string;
  cardType: WorkflowCardMessageType;
  planGenerationMeta?: WorkflowPlanGenerationMeta;
}

type OldTranscriptEntry = WorkflowTranscriptEntry & {
  message_type: 'system' | 'agent' | 'user' | 'control';
};

const senderToMessageType = (
  senderType: string,
): 'system' | 'agent' | 'user' | 'control' => {
  if (senderType === 'agent' || senderType === 'user' || senderType === 'system') {
    return senderType;
  }
  return 'control';
};

const toOldTranscriptEntry = (
  entry: WorkflowTranscriptEntry,
): OldTranscriptEntry => ({
  ...entry,
  message_type: senderToMessageType(entry.sender_type),
});

const isWorkflowInboxActionTranscript = (
  entry: WorkflowTranscriptEntry,
): boolean =>
  entry.entry_type === 'input_request' ||
  entry.entry_type === 'continue_confirmation' ||
  entry.entry_type === 'approval_request' ||
  entry.entry_type === 'permission_request';

const RESUME_START_TIMEOUT_MS = 15_000;
const RESUME_REFETCH_INTERVAL_MS = 500;
const RESUME_STARTED_REASONS = new Set([
  'step_started',
  'loop_review_started',
]);

type ResumePending = {
  executionId: string;
  requestToken: number;
  phase: 'requesting' | 'accepted';
};

export function WorkflowCard({
  sessionId,
  messageId,
  cardType,
  planGenerationMeta,
}: WorkflowCardProps) {
  const { t } = useAppTranslation();
  const {
    sessionsAsync,
    workflowRuntimeLinesByExecution,
    refreshSessionWorkflowStatus,
    showToast,
  } = useWorkspace();
  const sessionTitle = useMemo(() => {
    const session = sessionsAsync.data.find((s) => s.id === sessionId);
    return session?.title ?? null;
  }, [sessionsAsync.data, sessionId]);

  const [projection, setProjection] = useState<WorkflowCardProjection | null>(
    null,
  );
  const [windowOpen, setWindowOpen] = useState(false);
  const workflowCardRef = useRef<HTMLDivElement>(null);
  const [pendingActionId, setPendingActionId] = useState<string | null>(null);
  const [pendingActionType, setPendingActionType] = useState<string | null>(
    null,
  );
  const [resumePending, setResumePending] = useState<ResumePending | null>(
    null,
  );
  const resumeRequestTokenRef = useRef(0);
  const [skipConfirmationStepId, setSkipConfirmationStepId] = useState<
    string | null
  >(null);
  const [retryPlanGenerationError, setRetryPlanGenerationError] = useState<
    string | null
  >(null);
  const [executeReviewProjection, setExecuteReviewProjection] =
    useState<WorkflowCardProjection | null>(null);
  const [executeReviewError, setExecuteReviewError] = useState<string | null>(
    null,
  );

  const message = useMemo(
    () =>
      ({
        id: messageId,
        meta: {
          card_type: cardType,
          workflow_plan_generation: planGenerationMeta ?? null,
        },
      }),
    [cardType, messageId, planGenerationMeta],
  );

  const loadProjection = useCallback(async () => {
    try {
      const data = await chatMessagesApi.getWorkflowCard(messageId, 'full');
      setProjection(data);
      void refreshSessionWorkflowStatus(sessionId);
      return true;
    } catch {
      if (cardType === 'workflow_plan_generation') {
        setProjection(null);
      }
      return false;
    }
  }, [
    cardType,
    messageId,
    refreshSessionWorkflowStatus,
    sessionId,
  ]);

  useEffect(() => {
    void loadProjection();
  }, [loadProjection]);

  useEffect(() => {
    if (!shouldPollWorkflowProjection(projection)) return undefined;
    const intervalId = window.setInterval(() => {
      void loadProjection();
    }, WORKFLOW_CARD_REFETCH_INTERVAL_MS);
    return () => window.clearInterval(intervalId);
  }, [loadProjection, projection]);

  const refreshAll = async () => {
    await loadProjection();
  };

  const clearResumePending = useCallback((requestToken?: number) => {
    setResumePending((current) => {
      if (!current) return null;
      if (
        requestToken !== undefined &&
        current.requestToken !== requestToken
      ) {
        return current;
      }
      return null;
    });
  }, []);

  useEffect(() => {
    if (!resumePending) return undefined;
    const intervalId = window.setInterval(() => {
      void loadProjection();
    }, RESUME_REFETCH_INTERVAL_MS);
    return () => window.clearInterval(intervalId);
  }, [loadProjection, resumePending?.executionId]);

  useEffect(() => {
    if (!resumePending) return undefined;
    const timeoutId = window.setTimeout(() => {
      clearResumePending(resumePending.requestToken);
      showToast(
        t('workflow.resume.startTimeout', {
          defaultValue:
            'The workflow did not start in time. Check blocked reviews or retry.',
        }),
        'error',
      );
      void loadProjection();
    }, RESUME_START_TIMEOUT_MS);
    return () => window.clearTimeout(timeoutId);
  }, [
    clearResumePending,
    loadProjection,
    resumePending?.executionId,
    resumePending?.requestToken,
    showToast,
    t,
  ]);

  useEffect(() => {
    if (!resumePending) return undefined;
    const handleWorkflowGraphUpdated = (event: Event) => {
      const detail = (event as CustomEvent<WorkflowGraphUpdatedDetail>).detail;
      if (
        detail.sessionId !== sessionId ||
        detail.executionId !== resumePending.executionId
      ) {
        return;
      }

      if (RESUME_STARTED_REASONS.has(detail.reason)) {
        void loadProjection().then((loaded) => {
          if (loaded) {
            clearResumePending(resumePending.requestToken);
          }
        });
        return;
      }

      if (detail.reason === 'scheduler_blocked_by_review') {
        clearResumePending(resumePending.requestToken);
        showToast(
          t('workflow.resume.blockedByReview', {
            defaultValue:
              'Resolve the pending review before resuming the workflow.',
          }),
          'error',
        );
        void loadProjection();
        return;
      }

      if (detail.reason === 'scheduler_no_schedulable_work') {
        clearResumePending(resumePending.requestToken);
        showToast(
          t('workflow.resume.noSchedulableWork', {
            defaultValue: 'No workflow node is ready to run.',
          }),
          'error',
        );
        void loadProjection();
      }
    };
    window.addEventListener(
      WORKFLOW_GRAPH_UPDATED_EVENT,
      handleWorkflowGraphUpdated,
    );
    return () =>
      window.removeEventListener(
        WORKFLOW_GRAPH_UPDATED_EVENT,
        handleWorkflowGraphUpdated,
      );
  }, [
    clearResumePending,
    loadProjection,
    resumePending?.executionId,
    resumePending?.requestToken,
    sessionId,
    showToast,
    t,
  ]);

  useEffect(() => {
    if (
      !resumePending ||
      resumePending.phase !== 'accepted' ||
      projection?.execution_id !== resumePending.executionId
    ) {
      return;
    }
    const executionSettled =
      projection.steps.some((step) => step.status === 'running') ||
      projection.execution_status === 'waiting' ||
      projection.execution_status === 'completed' ||
      projection.state === 'waiting' ||
      projection.state === 'completed' ||
      projection.stopped_by_user;
    if (executionSettled) {
      clearResumePending(resumePending.requestToken);
    }
  }, [clearResumePending, projection, resumePending]);

  const withPending = async (
    id: string,
    actionType: string,
    action: () => Promise<unknown>,
  ) => {
    setPendingActionId(id);
    setPendingActionType(actionType);
    try {
      await action();
      await refreshAll();
    } finally {
      setPendingActionId(null);
      setPendingActionType(null);
    }
  };

  const shouldLoadFinalReviewAction =
    !!sessionId &&
    !!projection?.execution_id &&
    !(
      projection.state === 'preview_ready' ||
      projection.state === 'preview_invalid'
    ) &&
    (projection.state === 'waiting' ||
      projection.execution_status === 'waiting');
  const { data: finalReviewTranscripts = [] } = useQuery({
    queryKey: [
      'workflowFinalReviewAction',
      sessionId,
      projection?.execution_id,
    ],
    queryFn: () => {
      if (!sessionId || !projection?.execution_id) return [];
      return workflowApi
        .getExecutionTranscripts(sessionId, projection.execution_id, {
          entryType: 'final_review',
          unresolved: true,
          limit: 1,
        })
        .then((entries) => entries.map(toOldTranscriptEntry));
    },
    enabled: shouldLoadFinalReviewAction,
    staleTime: 30_000,
    gcTime: 5 * 60 * 1000,
    refetchInterval: shouldPollWorkflowProjection(projection)
      ? WORKFLOW_CARD_REFETCH_INTERVAL_MS
      : false,
  });
  const finalReviewAction =
    shouldLoadFinalReviewAction && projection?.execution_id
      ? toWorkflowFinalReviewAction(
          projection.execution_id,
          finalReviewTranscripts,
        )
      : null;
  const shouldLoadWorkflowInboxActionTranscripts =
    !!sessionId &&
    !!projection?.execution_id &&
    projection.has_transcripts !== false &&
    !(
      projection.state === 'preview_ready' ||
      projection.state === 'preview_invalid'
    );
  const { data: workflowInboxActionTranscripts = [] } = useQuery({
    queryKey: [
      'workflowInboxActionTranscripts',
      sessionId,
      projection?.execution_id,
    ],
    queryFn: () => {
      if (!sessionId || !projection?.execution_id) return [];
      return workflowApi
        .getExecutionTranscripts(sessionId, projection.execution_id, {
          unresolved: true,
          limit: 50,
        })
        .then((entries) => entries.filter(isWorkflowInboxActionTranscript));
    },
    enabled: shouldLoadWorkflowInboxActionTranscripts,
    staleTime: 30_000,
    gcTime: 5 * 60 * 1000,
    refetchInterval: shouldPollWorkflowProjection(projection)
      ? WORKFLOW_CARD_REFETCH_INTERVAL_MS
      : false,
  });
  const workflowInboxActionIds = useMemo(() => {
    const ids = new Set<string>();
    if (projection?.pending_input?.input_id) {
      ids.add(projection.pending_input.input_id);
    }
    for (const review of projection?.pending_reviews ?? []) {
      ids.add(review.review_id);
    }
    if (projection?.pending_review?.review_id) {
      ids.add(projection.pending_review.review_id);
    }
    if (finalReviewAction?.transcriptId) {
      ids.add(finalReviewAction.transcriptId);
    }
    for (const entry of workflowInboxActionTranscripts) {
      ids.add(entry.id);
    }
    return ids;
  }, [
    finalReviewAction?.transcriptId,
    projection?.pending_input,
    projection?.pending_review,
    projection?.pending_reviews,
    workflowInboxActionTranscripts,
  ]);

  useEffect(() => {
    const shouldOpenForTarget = (target: InboxWorkflowFocusTarget | null) => {
      if (!target || target.sessionId !== sessionId) return;
      const sourceId = target.sourceId ?? null;
      if (sourceId && workflowInboxActionIds.has(sourceId)) {
        setWindowOpen(true);
        clearInboxWorkflowFocus(target);
      }
    };
    shouldOpenForTarget(getPendingInboxWorkflowFocus(sessionId));
    const handleInboxFocus = (event: Event) => {
      shouldOpenForTarget(
        (event as CustomEvent<InboxWorkflowFocusTarget>).detail,
      );
    };
    window.addEventListener(INBOX_WORKFLOW_FOCUS_EVENT, handleInboxFocus);
    return () =>
      window.removeEventListener(INBOX_WORKFLOW_FOCUS_EVENT, handleInboxFocus);
  }, [sessionId, workflowInboxActionIds]);

  const workflowRuntimeMessages = useMemo(() => {
    if (!projection?.execution_id) return [];
    return workflowRuntimeLinesByExecution[projection.execution_id] ?? [];
  }, [projection?.execution_id, workflowRuntimeLinesByExecution]);

  useShortcutScope('workflow-session', {
    active: Boolean(projection),
    rootRef: workflowCardRef,
  });
  useCommandHandler('workflow.open', {
    scope: 'page',
    enabled: Boolean(projection),
    execute: () => setWindowOpen(true),
  });

  const handleExecute = (nextProjection: WorkflowCardProjection) => {
    setExecuteReviewError(null);
    setExecuteReviewProjection(nextProjection);
  };

  const handleCloseExecuteReviewSettings = () => {
    if (pendingActionId === 'execute-plan') return;
    setExecuteReviewProjection(null);
    setExecuteReviewError(null);
  };

  const handleConfirmExecute = async (
    overrides: WorkflowReviewSettingOverride[],
  ) => {
    if (!executeReviewProjection) return;
    setExecuteReviewError(null);
    try {
      await withPending('execute-plan', 'execute-plan', () =>
        workflowApi.executePlan(sessionId, executeReviewProjection.plan_id, {
          plan: null,
          stepReviewOverrides: overrides,
        }),
      );
      setExecuteReviewProjection(null);
    } catch (error) {
      setExecuteReviewError(
        error instanceof Error
          ? error.message
          : t('workflow.reviewSettings.executeError', {
              defaultValue: 'Unable to start workflow execution.',
            }),
      );
    }
  };

  const handleResume = async (executionId: string) => {
    if (pendingActionId || resumePending) return;
    const requestToken = resumeRequestTokenRef.current + 1;
    resumeRequestTokenRef.current = requestToken;
    setResumePending({
      executionId,
      requestToken,
      phase: 'requesting',
    });
    try {
      await workflowApi.resumeExecution(sessionId, executionId);
      setResumePending((current) =>
        current?.requestToken === requestToken
          ? { ...current, phase: 'accepted' }
          : current,
      );
      await refreshAll();
    } catch (error) {
      clearResumePending(requestToken);
      showToast(
        error instanceof Error
          ? error.message
          : t('workflow.resume.requestFailed', {
              defaultValue: 'Unable to resume the workflow.',
            }),
        'error',
      );
    }
  };

  const handleRetryStep = (stepId: string, retryTarget?: 'task' | 'review') =>
    void withPending(stepId, `retry-${retryTarget ?? 'task'}`, () =>
      workflowApi.retryStep(sessionId, stepId, retryTarget),
    );

  const handleRequestSkipStep = (stepId: string) =>
    setSkipConfirmationStepId(stepId);

  const confirmSkipStep = () => {
    const stepId = skipConfirmationStepId;
    if (!stepId) return;
    setSkipConfirmationStepId(null);
    void withPending(stepId, 'skip-step', () =>
      workflowApi.skipStep(sessionId, stepId),
    );
  };

  const handleInterruptStep = (stepId: string) =>
    void withPending(stepId, 'terminate-step', () =>
      workflowApi.interruptStepById(sessionId, stepId),
    );

  const handleStopStep = (stepId: string) =>
    void withPending(stepId, 'terminate-step', () =>
      workflowApi.stopStep(sessionId, stepId),
    );

  const handleStopExecution = (executionId: string) =>
    void withPending(executionId, 'stop-execution', () =>
      workflowApi.stopExecution(sessionId, executionId),
    );

  const handleMarkExecutionCompleted = (executionId: string) =>
    void withPending(executionId, 'complete-execution', () =>
      workflowApi.markExecutionCompleted(sessionId, executionId),
    );

  const handleApproval = (
    stepId: string,
    action: string,
    transcriptId: string,
    inputText?: string,
  ) =>
    void withPending(transcriptId, 'resolve-transcript', () =>
      workflowApi.approveStep(sessionId, stepId, {
        transcriptId,
        action,
        inputText,
      }),
    );

  const handlePendingReview = (
    reviewId: string,
    action: string,
    feedback?: string,
    expectedStepId?: string,
  ) =>
    void withPending(reviewId, 'respond-review', () =>
      workflowApi.respondToReview({
        review_id: reviewId,
        action,
        feedback: feedback ?? null,
        expected_step_id: expectedStepId ?? null,
      }),
    );

  const handleStepInput = (stepId: string, inputText: string) =>
    void withPending(stepId, 'submit-input', () =>
      workflowApi.submitStepInput(sessionId, stepId, inputText),
    );

  const handleIterationFeedback = (payload: {
    executionId: string;
    action: 'accept' | 'reject';
    feedback?: {
      what_wrong: string;
      expected: string;
      priority: 'low' | 'medium' | 'high';
      additional_notes?: string | null;
    };
  }) =>
    void withPending(payload.executionId, 'iteration-feedback', async () => {
      await workflowApi.submitIterationFeedback({
        execution_id: payload.executionId,
        action: payload.action,
        feedback: payload.feedback
          ? {
              ...payload.feedback,
              additional_notes: payload.feedback.additional_notes ?? null,
            }
          : null,
      });
      if (payload.action === 'accept') {
        notifySourceControlRefreshRequested({ sessionId });
      }
    });

  const handleUpdateReviewSettings = (
    executionId: string,
    overrides: Array<{
      stepId: string;
      leadReview: boolean | null;
      userReview: boolean | null;
    }>,
  ) =>
    withPending('review-settings', 'review-settings', () =>
      workflowApi.updateReviewSettings(sessionId, executionId, {
        stepReviewOverrides: overrides,
      }),
    );

  const handleRetryPlanGeneration = (retryMessageId: string) => {
    setRetryPlanGenerationError(null);
    void withPending(retryMessageId, 'retry-plan-generation', () =>
      workflowApi.retryPlanGeneration(sessionId, retryMessageId).catch((error) => {
        setRetryPlanGenerationError(
          error instanceof Error ? error.message : 'Retry request failed',
        );
        throw error;
      }),
    );
  };

  const effectivePendingActionId =
    resumePending?.executionId ?? pendingActionId;
  const effectivePendingActionType = resumePending
    ? 'resume-execution'
    : pendingActionType;

  return (
    <>
      <div ref={workflowCardRef}>
      <ChatWorkflowCard
        message={message}
        projection={projection}
        onExecute={handleExecute}
        onResume={handleResume}
        onMarkExecutionCompleted={handleMarkExecutionCompleted}
        onRetryStep={handleRetryStep}
        onSkipStep={handleRequestSkipStep}
        onOpenWindow={() => setWindowOpen(true)}
        onRetryPlanGeneration={handleRetryPlanGeneration}
        retryPlanGenerationPending={pendingActionId === messageId}
        retryPlanGenerationError={retryPlanGenerationError}
        finalReviewAction={finalReviewAction}
        onRespondPendingReview={handlePendingReview}
        onSubmitStepInput={handleStepInput}
        onSubmitIterationFeedback={handleIterationFeedback}
        pendingActionId={effectivePendingActionId}
        pendingActionType={effectivePendingActionType}
      />
      </div>

      {projection && (
        <WorkflowWindow
          sessionId={sessionId}
          sessionTitle={sessionTitle}
          projection={projection}
          finalReviewAction={finalReviewAction}
          runtimeMessages={workflowRuntimeMessages}
          isOpen={windowOpen}
          onClose={() => setWindowOpen(false)}
          onExecute={handleExecute}
          onResume={handleResume}
          onInterruptStep={handleInterruptStep}
          onStopStep={handleStopStep}
          onStopExecution={handleStopExecution}
          onMarkExecutionCompleted={handleMarkExecutionCompleted}
          onRetryStep={handleRetryStep}
          onSkipStep={handleRequestSkipStep}
          onUpdateReviewSettings={handleUpdateReviewSettings}
          onSubmitStepInput={handleStepInput}
          onApproval={handleApproval}
          onRespondPendingReview={handlePendingReview}
          onSubmitIterationFeedback={handleIterationFeedback}
          pendingActionId={effectivePendingActionId}
          pendingActionType={effectivePendingActionType}
        />
      )}

      {executeReviewProjection && (
        <WorkflowReviewSettingsDialog
          projection={executeReviewProjection}
          isOpen
          onClose={handleCloseExecuteReviewSettings}
          onSubmit={handleConfirmExecute}
          submitLabel={t('workflow.reviewSettings.startExecution', {
            defaultValue: 'Start Execution',
          })}
          submittingLabel={t('workflow.reviewSettings.startingExecution', {
            defaultValue: 'Starting...',
          })}
          isSubmitting={pendingActionId === 'execute-plan'}
          disabled={pendingActionId === 'execute-plan'}
          error={executeReviewError}
          variant="modal"
        />
      )}

      {skipConfirmationStepId && (
        <ConfirmationDialog
          title={t('workflow.confirm.skipStepTitle', {
            defaultValue: 'Skip this node?',
          })}
          description={t('workflow.confirm.skipStepDescription', {
            defaultValue:
              'The node will be treated as completed, and dependent downstream nodes may continue. This action cannot be undone.',
          })}
          confirmLabel={t('workflow.controls.skipStep', {
            defaultValue: 'Skip',
          })}
          cancelLabel={t('cancel', { defaultValue: 'Cancel' })}
          escLabel={t('escToCancel', { defaultValue: 'Esc to cancel' })}
          confirmIcon={<SkipForward />}
          idPrefix="workflow-skip-step-confirm"
          onCancel={() => setSkipConfirmationStepId(null)}
          onConfirm={confirmSkipStep}
        />
      )}
    </>
  );
}
