// Executable logic tests for loadSessionRunningIndicators pending-approval detection.
//
// Run with:
//     pnpm exec tsx src/context/workspace/sessionApprovalIndicators.test.ts

import { loadSessionRunningIndicators } from './workspaceContextUtils';

let failures = 0;
const check = (label: string, cond: boolean, detail?: unknown) => {
  if (cond) {
    console.log(`  ok  ${label}`);
  } else {
    failures += 1;
    console.error(`  FAIL ${label}`, detail ?? '');
  }
};

const originalFetch = globalThis.fetch;

const makeAgent = (
  id: string,
  state: string,
): {
  id: string;
  session_id: string;
  agent_id: string;
  state: string;
  workspace_path: string | null;
  pty_session_key: string | null;
  agent_session_id: string | null;
  agent_message_id: string | null;
  project_member_id: string | null;
  execution_config: null;
  allowed_skill_ids: string[];
  created_at: string;
  updated_at: string;
} => ({
  id,
  session_id: 'sess-1',
  agent_id: `agent-${id}`,
  state,
  workspace_path: null,
  pty_session_key: null,
  agent_session_id: null,
  agent_message_id: null,
  project_member_id: null,
  execution_config: null,
  allowed_skill_ids: [],
  created_at: '2026-01-01T00:00:00Z',
  updated_at: '2026-01-01T00:00:00Z',
});

const idleWorkflowStatus = {
  sidebar_workflow_state: 'idle',
  has_running_workflow: false,
  pending_workflow_input_id: null,
  pending_workflow_review_id: null,
};

const apiSuccess = (data: unknown) =>
  Promise.resolve(
    new Response(JSON.stringify({ success: true, data }), {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    }),
  );

console.log('sessionApprovalIndicators');

// Test 1: agent with waitingapproval state -> hasPendingApproval = true
{
  const agentsBySession: Record<string, unknown[]> = {
    'sess-1': [makeAgent('sa-1', 'running'), makeAgent('sa-2', 'waitingapproval')],
    'sess-2': [makeAgent('sa-3', 'running')],
  };

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes('/agents')) {
      const match = url.match(/\/sessions\/([^/]+)\/agents/);
      const sid = match?.[1] ?? '';
      return apiSuccess(agentsBySession[sid] ?? []);
    }
    return apiSuccess(idleWorkflowStatus);
  }) as typeof globalThis.fetch;

  const result = await loadSessionRunningIndicators(['sess-1', 'sess-2']);

  check(
    'detects waitingapproval agent as hasPendingApproval=true',
    result.get('sess-1')?.hasPendingApproval === true,
    result.get('sess-1'),
  );
  check(
    'session without waitingapproval agent has hasPendingApproval=false',
    result.get('sess-2')?.hasPendingApproval === false,
    result.get('sess-2'),
  );
}

// Test 2: no agents -> hasPendingApproval = false
{
  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes('/agents')) {
      return apiSuccess([]);
    }
    return apiSuccess(idleWorkflowStatus);
  }) as typeof globalThis.fetch;

  const result = await loadSessionRunningIndicators(['sess-empty']);

  check(
    'empty agent list yields hasPendingApproval=false',
    result.get('sess-empty')?.hasPendingApproval === false,
    result.get('sess-empty'),
  );
}

// Test 3: ignored agent IDs are excluded from waitingapproval check
{
  const agentsBySession: Record<string, unknown[]> = {
    'sess-1': [makeAgent('sa-1', 'waitingapproval')],
  };

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes('/agents')) {
      const match = url.match(/\/sessions\/([^/]+)\/agents/);
      const sid = match?.[1] ?? '';
      return apiSuccess(agentsBySession[sid] ?? []);
    }
    return apiSuccess(idleWorkflowStatus);
  }) as typeof globalThis.fetch;

  const result = await loadSessionRunningIndicators(
    ['sess-1'],
    new Set(['sa-1']),
  );

  check(
    'ignored waitingapproval agent is excluded from hasPendingApproval',
    result.get('sess-1')?.hasPendingApproval === false,
    result.get('sess-1'),
  );
}

// Test 4: active session (no skip) also gets waitingapproval detection
{
  const agentsBySession: Record<string, unknown[]> = {
    'sess-active': [makeAgent('sa-active', 'waitingapproval')],
    'sess-other': [makeAgent('sa-other', 'running')],
  };

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes('/agents')) {
      const match = url.match(/\/sessions\/([^/]+)\/agents/);
      const sid = match?.[1] ?? '';
      return apiSuccess(agentsBySession[sid] ?? []);
    }
    return apiSuccess(idleWorkflowStatus);
  }) as typeof globalThis.fetch;

  const result = await loadSessionRunningIndicators([
    'sess-active',
    'sess-other',
  ]);

  check(
    'active session with waitingapproval is detected (no skip)',
    result.get('sess-active')?.hasPendingApproval === true,
    result.get('sess-active'),
  );
  check(
    'non-active session without waitingapproval is false',
    result.get('sess-other')?.hasPendingApproval === false,
    result.get('sess-other'),
  );
}

// Test 5: multiple waitingapproval agents in same session
{
  const agentsBySession: Record<string, unknown[]> = {
    'sess-1': [
      makeAgent('sa-1', 'waitingapproval'),
      makeAgent('sa-2', 'waitingapproval'),
      makeAgent('sa-3', 'running'),
    ],
  };

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes('/agents')) {
      const match = url.match(/\/sessions\/([^/]+)\/agents/);
      const sid = match?.[1] ?? '';
      return apiSuccess(agentsBySession[sid] ?? []);
    }
    return apiSuccess(idleWorkflowStatus);
  }) as typeof globalThis.fetch;

  const result = await loadSessionRunningIndicators(['sess-1']);

  check(
    'multiple waitingapproval agents detected correctly',
    result.get('sess-1')?.hasPendingApproval === true,
    result.get('sess-1'),
  );
}

// Test 6: running agent + waitingapproval coexist -> both hasRunningAgent and hasPendingApproval true
{
  const agentsBySession: Record<string, unknown[]> = {
    'sess-1': [
      makeAgent('sa-1', 'running'),
      makeAgent('sa-2', 'waitingapproval'),
    ],
  };

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes('/agents')) {
      const match = url.match(/\/sessions\/([^/]+)\/agents/);
      const sid = match?.[1] ?? '';
      return apiSuccess(agentsBySession[sid] ?? []);
    }
    return apiSuccess(idleWorkflowStatus);
  }) as typeof globalThis.fetch;

  const result = await loadSessionRunningIndicators(['sess-1']);

  check(
    'running + waitingapproval: hasRunningAgent=true AND hasPendingApproval=true',
    result.get('sess-1')?.hasRunningAgent === true &&
      result.get('sess-1')?.hasPendingApproval === true,
    result.get('sess-1'),
  );
}

// Test 7: state migration — waiting=true -> terminal event -> recompute to false
{
  let callCount = 0;
  const agentsPhase1 = [makeAgent('sa-1', 'waitingapproval')];
  const agentsPhase2 = [makeAgent('sa-1', 'running')];

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes('/agents')) {
      callCount += 1;
      const agents = callCount <= 1 ? agentsPhase1 : agentsPhase2;
      return apiSuccess(agents);
    }
    return apiSuccess(idleWorkflowStatus);
  }) as typeof globalThis.fetch;

  const initial = await loadSessionRunningIndicators(['sess-1']);
  const wasPending = initial.get('sess-1')?.hasPendingApproval === true;

  const afterTerminal = await loadSessionRunningIndicators(['sess-1']);
  const nowPending = afterTerminal.get('sess-1')?.hasPendingApproval === false;

  check(
    'state migration: waiting=true -> terminal event -> recompute to false',
    wasPending && nowPending,
    { initial: initial.get('sess-1'), afterTerminal: afterTerminal.get('sess-1') },
  );
}

// Test 8: state migration — waiting=true -> terminal event -> other agent still waiting -> stays true
{
  let callCount = 0;
  const agentsPhase1 = [
    makeAgent('sa-1', 'waitingapproval'),
    makeAgent('sa-2', 'waitingapproval'),
  ];
  const agentsPhase2 = [
    makeAgent('sa-1', 'running'),
    makeAgent('sa-2', 'waitingapproval'),
  ];

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes('/agents')) {
      callCount += 1;
      const agents = callCount <= 1 ? agentsPhase1 : agentsPhase2;
      return apiSuccess(agents);
    }
    return apiSuccess(idleWorkflowStatus);
  }) as typeof globalThis.fetch;

  const initial = await loadSessionRunningIndicators(['sess-1']);
  const wasPending = initial.get('sess-1')?.hasPendingApproval === true;

  const afterTerminal = await loadSessionRunningIndicators(['sess-1']);
  const stillPending =
    afterTerminal.get('sess-1')?.hasPendingApproval === true;

  check(
    'state migration: terminal event but other agent still waiting -> stays true',
    wasPending && stillPending,
    { initial: initial.get('sess-1'), afterTerminal: afterTerminal.get('sess-1') },
  );
}

// Test 9: state migration — no inbox/archived inbox, only agent waitingapproval -> detected
{
  const agentsBySession: Record<string, unknown[]> = {
    'sess-1': [makeAgent('sa-1', 'waitingapproval')],
  };

  globalThis.fetch = (async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes('/agents')) {
      const match = url.match(/\/sessions\/([^/]+)\/agents/);
      const sid = match?.[1] ?? '';
      return apiSuccess(agentsBySession[sid] ?? []);
    }
    return apiSuccess(idleWorkflowStatus);
  }) as typeof globalThis.fetch;

  const result = await loadSessionRunningIndicators(['sess-1']);

  check(
    'no inbox source, agent-only waitingapproval still detected',
    result.get('sess-1')?.hasPendingApproval === true,
    result.get('sess-1'),
  );
}

globalThis.fetch = originalFetch;

if (failures > 0) {
  console.error(`\n${failures} assertion(s) FAILED`);
  process.exit(1);
} else {
  console.log('\nAll sessionApprovalIndicators assertions passed.');
}