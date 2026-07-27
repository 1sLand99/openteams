import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import {
  ChatExecutorApprovalStatus,
  type ChatExecutorApprovalRequest,
} from '../../../shared/types';
import {
  reconcileApprovalRequests,
  removeApprovalRequest,
  upsertApprovalRequest,
} from './useExecutorApprovals';

const request = (
  id: string,
  createdAt: string,
  updatedAt = createdAt,
): ChatExecutorApprovalRequest => ({
  id,
  session_id: 'session-1',
  session_agent_id: 'member-1',
  run_id: 'run-1',
  workflow_execution_id: null,
  workflow_step_id: null,
  runner: 'QWEN_CODE',
  tool_call_id: `tool-call-${id}`,
  tool_name: 'write_file',
  display_input: {},
  options: [
    { option_id: 'reject', kind: 'reject_once', label: 'Reject' },
    { option_id: 'allow', kind: 'allow_once', label: 'Allow once' },
  ],
  status: ChatExecutorApprovalStatus.pending,
  selected_option_id: null,
  processed_by: null,
  expires_at: new Date('2026-07-27T04:00:00Z'),
  resolved_at: null,
  created_at: new Date(createdAt),
  updated_at: new Date(updatedAt),
});

const first = request('first', '2026-07-27T03:00:00Z');
const second = request('second', '2026-07-27T03:01:00Z');
const current = [first, second];

const identicalRefresh = reconcileApprovalRequests(current, [
  { ...second },
  { ...first },
]);
assert.equal(
  identicalRefresh,
  current,
  'an identical full reconciliation preserves the list and row identities',
);

const changedSecond = {
  ...second,
  updated_at: new Date('2026-07-27T03:02:00Z'),
};
const partiallyChanged = reconcileApprovalRequests(current, [
  changedSecond,
  { ...first },
]);
assert.equal(partiallyChanged[0], first);
assert.equal(partiallyChanged[1], changedSecond);

const afterRemoval = removeApprovalRequest(current, first.id);
assert.deepEqual(afterRemoval.map((item) => item.id), [second.id]);
assert.equal(
  afterRemoval[0],
  second,
  'removing one approval preserves every unaffected row identity',
);
assert.equal(
  removeApprovalRequest(afterRemoval, first.id),
  afterRemoval,
  'duplicate terminal events are idempotent',
);

const earlier = request('earlier', '2026-07-27T02:59:00Z');
const afterInsert = upsertApprovalRequest(current, earlier);
assert.deepEqual(
  afterInsert.map((item) => item.id),
  [earlier.id, first.id, second.id],
);
assert.equal(afterInsert[1], first);
assert.equal(afterInsert[2], second);

const hookSource = readFileSync(
  new URL('./useExecutorApprovals.ts', import.meta.url),
  'utf8',
);
const traySource = readFileSync(
  new URL('../components/approvals/FreeChatApprovalTray.tsx', import.meta.url),
  'utf8',
);
const eventHandlerSource = hookSource.slice(
  hookSource.indexOf('const onChanged ='),
  hookSource.indexOf(
    'window.addEventListener(EXECUTOR_APPROVAL_CHANGED_EVENT',
  ),
);
assert.ok(eventHandlerSource.includes('upsertRequest(detail.request)'));
assert.ok(eventHandlerSource.includes('removeRequest(detail.requestId)'));
assert.ok(!eventHandlerSource.includes('refresh()'));
assert.ok(traySource.includes('React.memo'));
assert.ok(traySource.includes('resolvingIds.has(request.id)'));
assert.ok(traySource.includes('emptyDismissTimerRef'));

console.log('executor approval incremental state tests passed');
