import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  approvalCommand,
  approvalOptionLabel,
  groupApprovalRequests,
  partitionApprovalOptions,
  shouldShowApprovalSummary,
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
  'Allow',
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
    { option_id: 'deny', kind: 'reject_once', label: 'Stop' },
    translate,
  ),
  'Deny',
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

const commandRequest = {
  display_input: {
    tool_call: {
      rawInput: {
        command: 'pnpm run frontend:check',
      },
    },
  },
} as unknown as ChatExecutorApprovalRequest;
assert.equal(approvalCommand(commandRequest), 'pnpm run frontend:check');
assert.equal(
  approvalCommand({
    display_input: { command: 'cargo test' },
  } as unknown as ChatExecutorApprovalRequest),
  'cargo test',
);
assert.equal(
  approvalCommand({
    display_input: {
      command: '   ',
      permission: 'bash',
      metadata: { command: 'rg -n approval crates frontend' },
      patterns: ['rg -n approval crates frontend'],
    },
  } as unknown as ChatExecutorApprovalRequest),
  'rg -n approval crates frontend',
);
assert.equal(
  approvalCommand({
    display_input: {
      permission: 'bash',
      patterns: ['pnpm run frontend:check'],
    },
  } as unknown as ChatExecutorApprovalRequest),
  'pnpm run frontend:check',
);
assert.equal(
  approvalCommand({
    display_input: {
      permission: 'edit',
      patterns: ['frontend/src/**/*.tsx'],
    },
  } as unknown as ChatExecutorApprovalRequest),
  null,
);
assert.equal(
  approvalCommand({
    display_input: { tool_call: { rawInput: {} } },
  } as unknown as ChatExecutorApprovalRequest),
  null,
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

assert.equal(shouldShowApprovalSummary(1, 1), false);
assert.equal(shouldShowApprovalSummary(2, 1), true);
assert.equal(shouldShowApprovalSummary(2, 2), true);

const approvalTraySource = readFileSync(
  fileURLToPath(new URL('./FreeChatApprovalTray.tsx', import.meta.url)),
  'utf8',
);
assert.ok(
  approvalTraySource.includes(
    'const displayedAction = command ?? request.tool_name;',
  ) &&
    approvalTraySource.includes('title={displayedAction}') &&
    approvalTraySource.includes('{displayedAction}') &&
    approvalTraySource.includes('min-w-0 flex-1 truncate whitespace-nowrap') &&
    !approvalTraySource.includes('commandExpanded') &&
    !approvalTraySource.includes('writeClipboardViaBridge(command)'),
  'approval command replaces the tool label inline and truncates within the action column',
);

console.log('executor approval presentation tests passed');
