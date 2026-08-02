import type {
  ChatExecutorApprovalOption,
  ChatExecutorApprovalRequest,
  JsonValue,
} from '../../../../shared/types';

export type ApprovalTranslate = (
  key: string,
  fallback: string,
  replacements?: Record<string, string | number>,
) => string;

const OPTION_LABELS: Record<string, [string, string]> = {
  allow_always: ['approvals.option.allowAlways', 'Always allow'],
  allow_once: ['approvals.option.allowOnce', 'Allow'],
  reject_always: ['approvals.option.rejectAlways', 'Always deny'],
  reject_once: ['approvals.option.rejectOnce', 'Deny'],
};

export const approvalOptionLabel = (
  option: ChatExecutorApprovalOption,
  translate: ApprovalTranslate,
) => {
  const knownOption = OPTION_LABELS[option.kind];
  return knownOption
    ? translate(knownOption[0], knownOption[1])
    : option.label;
};

export const partitionApprovalOptions = (
  options: ChatExecutorApprovalOption[],
) => ({
  allowOnce: options.find((option) => option.kind === 'allow_once'),
  allowAlways: options.find((option) => option.kind === 'allow_always'),
  otherOptions: options.filter(
    (option) =>
      option.kind !== 'allow_once' && option.kind !== 'allow_always',
  ),
});

type JsonObject = { [key: string]: JsonValue | undefined };

const asJsonObject = (value: JsonValue | undefined): JsonObject | null =>
  value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value
    : null;

export const approvalCommand = (
  request: ChatExecutorApprovalRequest,
): string | null => {
  const displayInput = asJsonObject(request.display_input);
  const metadata = asJsonObject(displayInput?.metadata);
  const toolCall = asJsonObject(
    displayInput?.tool_call ?? displayInput?.toolCall,
  );
  const rawInput = asJsonObject(
    toolCall?.rawInput ?? toolCall?.raw_input,
  );
  const patterns = Array.isArray(displayInput?.patterns)
    ? displayInput.patterns
    : [];
  const permission =
    typeof displayInput?.permission === 'string'
      ? displayInput.permission.toLowerCase()
      : null;
  const patternCommand =
    permission && ['bash', 'shell', 'command', 'execute'].includes(permission)
      ? patterns[0]
      : undefined;
  const command = [
    displayInput?.command,
    metadata?.command,
    rawInput?.command,
    toolCall?.command,
    patternCommand,
  ].find(
    (candidate): candidate is string =>
      typeof candidate === 'string' && Boolean(candidate.trim()),
  );
  return command ?? null;
};

export type ApprovalRequestGroup = {
  sessionAgentId: string;
  requests: ChatExecutorApprovalRequest[];
};

export const groupApprovalRequests = (
  requests: ChatExecutorApprovalRequest[],
): ApprovalRequestGroup[] => {
  const groups = new Map<string, ApprovalRequestGroup>();
  requests.forEach((request) => {
    const existing = groups.get(request.session_agent_id);
    if (existing) {
      existing.requests.push(request);
      return;
    }
    groups.set(request.session_agent_id, {
      sessionAgentId: request.session_agent_id,
      requests: [request],
    });
  });
  return Array.from(groups.values());
};

export const shouldShowApprovalSummary = (
  requestCount: number,
  memberCount: number,
) => requestCount > 1 || memberCount > 1;
