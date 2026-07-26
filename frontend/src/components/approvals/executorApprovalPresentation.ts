import type { ChatExecutorApprovalOption } from '../../../../shared/types';

export type ApprovalTranslate = (
  key: string,
  fallback: string,
  replacements?: Record<string, string | number>,
) => string;

const OPTION_LABELS: Record<string, [string, string]> = {
  allow_always: ['approvals.option.allowAlways', 'Always allow'],
  allow_once: ['approvals.option.allowOnce', 'Allow once'],
  reject_always: ['approvals.option.rejectAlways', 'Always deny'],
  reject_once: ['approvals.option.rejectOnce', 'Deny once'],
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
