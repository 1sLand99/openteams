const WORKFLOW_RUNTIME_ERROR_PREFIX = 'openteams.workflow_runtime_error:';
const WORKFLOW_RUNTIME_ERROR_DETAIL_PREFIX =
  'openteams.workflow_runtime_error_detail:';

type WorkflowRuntimeErrorCode =
  | 'session_inactivity_timeout'
  | 'step_interrupted'
  | 'execution_failed'
  | 'missing_assistant_output'
  | 'child_stdout_missing'
  | 'child_stderr_missing';

type WorkflowRuntimeErrorPayload = {
  code: WorkflowRuntimeErrorCode;
  agent_name?: string;
  inactivity_minutes?: number;
};

export type WorkflowRuntimeErrorTranslate = (
  key: string,
  fallback: string,
  replacements?: Record<string, string | number>,
) => string;

const isRecord = (value: unknown): value is Record<string, unknown> =>
  !!value && typeof value === 'object' && !Array.isArray(value);

const parseWorkflowRuntimeError = (
  message: string,
): { payload: WorkflowRuntimeErrorPayload; detail: string | null } | null => {
  const normalized = message.trim();
  const errorPrefix = normalized.indexOf(WORKFLOW_RUNTIME_ERROR_PREFIX);
  if (errorPrefix < 0) return null;

  const encodedPayloadAndDetail = normalized.slice(
    errorPrefix + WORKFLOW_RUNTIME_ERROR_PREFIX.length,
  );
  const detailSeparator = encodedPayloadAndDetail.indexOf(
    WORKFLOW_RUNTIME_ERROR_DETAIL_PREFIX,
  );
  const encodedPayload = (
    detailSeparator >= 0
      ? encodedPayloadAndDetail.slice(0, detailSeparator)
      : encodedPayloadAndDetail
  ).trim();
  const detail =
    detailSeparator >= 0
      ? encodedPayloadAndDetail
          .slice(detailSeparator + WORKFLOW_RUNTIME_ERROR_DETAIL_PREFIX.length)
          .trim() || null
      : null;

  try {
    const value: unknown = JSON.parse(encodedPayload);
    if (!isRecord(value) || typeof value.code !== 'string') return null;
    return {
      payload: value as WorkflowRuntimeErrorPayload,
      detail,
    };
  } catch {
    return null;
  }
};

const translatedRuntimeError = (
  payload: WorkflowRuntimeErrorPayload,
  t: WorkflowRuntimeErrorTranslate,
): string | null => {
  const agentName =
    typeof payload.agent_name === 'string'
      ? payload.agent_name.trim()
      : undefined;

  switch (payload.code) {
    case 'session_inactivity_timeout':
      if (!agentName || !Number.isFinite(payload.inactivity_minutes)) return null;
      return t(
        'workflow.runtimeErrors.sessionInactivityTimeout',
        'Workflow stopped because {agentName} had no session activity for {minutes} minutes.',
        {
          agentName,
          minutes: payload.inactivity_minutes as number,
        },
      );
    case 'step_interrupted':
      if (!agentName) return null;
      return t(
        'workflow.runtimeErrors.stepInterrupted',
        'Workflow step for {agentName} was interrupted.',
        { agentName },
      );
    case 'execution_failed':
      if (!agentName) return null;
      return t(
        'workflow.runtimeErrors.executionFailed',
        'Workflow execution failed for {agentName}.',
        { agentName },
      );
    case 'missing_assistant_output':
      if (!agentName) return null;
      return t(
        'workflow.runtimeErrors.missingAssistantOutput',
        'Workflow agent {agentName} returned no assistant output.',
        { agentName },
      );
    case 'child_stdout_missing':
      return t(
        'workflow.runtimeErrors.childStdoutMissing',
        'The workflow executor child process is missing stdout.',
      );
    case 'child_stderr_missing':
      return t(
        'workflow.runtimeErrors.childStderrMissing',
        'The workflow executor child process is missing stderr.',
      );
    default:
      return null;
  }
};

export const localizeWorkflowRuntimeError = (
  message: string,
  t: WorkflowRuntimeErrorTranslate,
): string => {
  const parsed = parseWorkflowRuntimeError(message);
  if (!parsed) return message.trim();

  const translated = translatedRuntimeError(parsed.payload, t);
  if (!translated) return message.trim();
  if (!parsed.detail) return translated;

  const detailHeading = t(
    'workflow.runtimeErrors.executorDetailHeading',
    'Executor error:',
  );
  return `${translated}\n\n${detailHeading}\n${parsed.detail}`;
};
