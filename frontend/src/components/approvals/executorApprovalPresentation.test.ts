import assert from 'node:assert/strict';
import {
  approvalOptionLabel,
  groupApprovalRequests,
  partitionApprovalOptions,
  type ApprovalTranslate,
} from './executorApprovalPresentation';
import type { ChatExecutorApprovalRequest } from '../../../../shared/types';

const translate: ApprovalTranslate = (key, fallback, replacements = {}) =>
  Object.entries(replacements).reduce(
    (value, [name, replacement]) =>
      value.replaceAll(`{${name}}`, String(replacement)),
    fallback || key,
  );

assert.equal(
  approvalOptionLabel(
    { option_id: 'once', kind: 'allow_once', label: 'Proceed' },
    translate,
  ),
  'Allow once',
);
assert.equal(
  approvalOptionLabel(
    { option_id: 'always', kind: 'allow_always', label: 'Proceed always' },
    translate,
  ),
  'Always allow',
);
assert.equal(
  approvalOptionLabel(
    { option_id: 'custom', kind: 'other', label: 'Ask agent' },
    translate,
  ),
  'Ask agent',
);

const partitionedOptions = partitionApprovalOptions([
  { option_id: 'deny', kind: 'reject_once', label: 'Deny' },
  { option_id: 'always', kind: 'allow_always', label: 'Always' },
  { option_id: 'once', kind: 'allow_once', label: 'Once' },
]);
assert.equal(partitionedOptions.allowOnce?.option_id, 'once');
assert.equal(partitionedOptions.allowAlways?.option_id, 'always');
assert.deepEqual(
  partitionedOptions.otherOptions.map((option) => option.option_id),
  ['deny'],
);

const requests = [
  { id: 'a-1', session_agent_id: 'member-a' },
  { id: 'b-1', session_agent_id: 'member-b' },
  { id: 'a-2', session_agent_id: 'member-a' },
] as unknown as ChatExecutorApprovalRequest[];
const groups = groupApprovalRequests(requests);

assert.deepEqual(
  groups.map((group) => ({
    member: group.sessionAgentId,
    requests: group.requests.map((request) => request.id),
  })),
  [
    { member: 'member-a', requests: ['a-1', 'a-2'] },
    { member: 'member-b', requests: ['b-1'] },
  ],
);

console.log('executor approval presentation tests passed');
