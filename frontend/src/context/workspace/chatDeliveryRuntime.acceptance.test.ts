// Acceptance entry for the chat delivery runtime (frontend side of the
// cross-layer CDD matrix). Behavioral assertions only — no source-string
// matching. CDD-005 drives the production resync consumer (the single-flight
// scheduler + getSnapshot) rather than hand-applying snapshots. Run with:
//     pnpm exec tsx src/context/workspace/chatDeliveryRuntime.acceptance.test.ts --case CDD-004
//     pnpm exec tsx src/context/workspace/chatDeliveryRuntime.acceptance.test.ts --case CDD-005
// Exits 0 on pass, 2 on usage error, non-zero on the first failed assertion.

import assert from 'node:assert/strict';

import type { ChatSessionRuntimeSnapshot, MemberQueueSnapshot } from '@/types';
import {
  EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
  chatDeliveryRuntimeReducer,
  deliveriesFromMemberQueue,
  deliveriesFromRuntimeSnapshot,
  deliveryCardToMessage,
  deliveryCardsForSession,
  mergePersistedWithDeliveryCards,
  sessionsNeedingResync,
  type ChatDelivery,
  type ChatDeliveryRuntimeAction,
  type ChatDeliveryRuntimeState,
} from './chatDeliveryRuntime';
import { ChatDeliveryResyncScheduler } from './chatDeliveryResyncScheduler';

const T0 = 1_700_000_000_000;
const iso = (ms: number) => new Date(ms).toISOString();

interface EvidenceEntry {
  step: string;
  input: Record<string, unknown>;
}

interface CaseReport {
  case: string;
  evidence: EvidenceEntry[];
  projection: Record<string, unknown>;
  assertions: number;
}

let assertionCount = 0;
const check = (cond: boolean, message: string) => {
  assertionCount += 1;
  assert.ok(cond, message);
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const runDelivery = (
  sessionId: string,
  overrides: Partial<ChatDelivery> = {},
): ChatDelivery => ({
  deliveryId: 'queue:delivery-1',
  sessionId,
  sessionAgentId: 'agent-alpha',
  agentId: 'agent-alpha-id',
  agentName: 'Alpha',
  displayName: '@Alpha',
  clientMessageId: 'client-msg-1',
  sourceMessageId: 'msg-1',
  runId: 'run-1',
  status: 'running',
  createdAt: iso(T0),
  updatedAt: iso(T0),
  ...overrides,
});

const snapshotAction = (
  sessionId: string,
  revision: number | undefined,
  deliveries: ChatDelivery[],
  requestedAt: number,
  receivedAt: number,
): Extract<ChatDeliveryRuntimeAction, { type: 'snapshot_received' }> => ({
  type: 'snapshot_received',
  sessionId,
  deliveries,
  revision,
  requestedAt,
  receivedAt,
});

const upsertAction = (
  delivery: ChatDelivery,
  revision: number | undefined,
  receivedAt: number,
): Extract<ChatDeliveryRuntimeAction, { type: 'delivery_upsert' }> => ({
  type: 'delivery_upsert',
  delivery,
  revision,
  receivedAt,
});

const terminalAction = (
  sessionId: string,
  sessionAgentId: string,
  runId: string | undefined,
  revision: number | undefined,
  receivedAt: number,
): Extract<ChatDeliveryRuntimeAction, { type: 'delivery_terminal' }> => ({
  type: 'delivery_terminal',
  sessionId,
  sessionAgentId,
  runId,
  status: 'completed',
  revision,
  receivedAt,
});

const makeRawSnapshot = (
  sessionId: string,
  revision: number,
  runs: Array<{ deliveryId: string; runId: string; status: 'starting' | 'running' }>,
): ChatSessionRuntimeSnapshot => ({
  session_id: sessionId,
  revision: BigInt(revision),
  messages: null,
  active_runs: runs.map((run) => ({
    delivery_id: run.deliveryId,
    run_id: run.runId,
    session_id: sessionId,
    session_agent_id: 'agent-alpha',
    agent_id: 'agent-alpha-id',
    agent_name: 'Alpha',
    display_name: '@Alpha',
    avatar: 'AL',
    model: null,
    status: run.status,
    source_message_id: 'msg-1',
    client_message_id: 'client-msg-1',
    created_at: iso(T0),
  })),
  queues: [],
});

const makeQueue = (
  sessionId: string,
  revision: number,
  items: MemberQueueSnapshot['items'],
): MemberQueueSnapshot => ({
  session_id: sessionId,
  revision: BigInt(revision),
  session_agent_id: 'agent-alpha',
  agent_id: 'agent-alpha-id',
  status: 'running',
  blocked: false,
  paused: false,
  can_continue: false,
  queued_count: BigInt(items.length),
  items,
});

const makeQueueItem = (
  status: string,
  overrides: Record<string, unknown> = {},
): MemberQueueSnapshot['items'][number] =>
  ({
    can_delete: false,
    message: {
      id: 'delivery-1',
      session_id: 'session-A',
      session_agent_id: 'agent-alpha',
      agent_id: 'agent-alpha-id',
      chat_message_id: 'msg-1',
      status,
      revision: 0n,
      attempt_no: 0n,
      created_at: iso(T0),
      updated_at: iso(T0),
      processing_started_at: iso(T0),
      run_id: 'run-1',
      failure_reason: null,
      ...overrides,
    },
  }) as MemberQueueSnapshot['items'][number];

const describeDelivery = (delivery: ChatDelivery) => ({
  deliveryId: delivery.deliveryId,
  sessionAgentId: delivery.sessionAgentId,
  runId: delivery.runId ?? null,
  status: delivery.status,
  clientMessageId: delivery.clientMessageId ?? null,
});

const describeProjection = (
  state: ChatDeliveryRuntimeState,
  sessionId: string,
) =>
  deliveryCardsForSession(state, sessionId)
    .map(deliveryCardToMessage)
    .map((card) => ({
      cardId: card.id,
      sessionAgentId: card.sessionAgentId ?? null,
      runId: card.runId ?? null,
      clientMessageId: card.clientMessageId ?? null,
    }));

// ---------------------------------------------------------------------------
// Fake clock/timer + controllable getSnapshot for the production consumer
// ---------------------------------------------------------------------------

interface PendingFetch {
  sessionId: string;
  resolve: (snapshot: ChatSessionRuntimeSnapshot) => void;
  reject: (error: Error) => void;
}

interface ConsumerHarness {
  state: () => ChatDeliveryRuntimeState;
  dispatchSync: (
    action: ChatDeliveryRuntimeAction,
  ) => ChatDeliveryRuntimeState;
  requestFlaggedSessions: () => void;
  getSnapshotCalls: string[];
  pendingFetches: PendingFetch[];
  advanceTime: (ms: number) => void;
  flush: () => Promise<void>;
  dispose: () => void;
}

const createConsumerHarness = (
  initial: ChatDeliveryRuntimeState,
): ConsumerHarness => {
  let state = initial;
  // Start the fake clock past every fixture event timestamp so consumer
  // snapshot requests are fresh by the local clock (authoritative replace).
  let now = T0 + 1000;
  let nextHandle = 1;
  const timers: Array<{
    handle: number;
    at: number;
    cb: () => void;
    cancelled: boolean;
  }> = [];
  const pendingFetches: PendingFetch[] = [];
  const getSnapshotCalls: string[] = [];

  const harness: ConsumerHarness = {
    state: () => state,
    dispatchSync: (action) => {
      state = chatDeliveryRuntimeReducer(state, action);
      return state;
    },
    requestFlaggedSessions: () => {
      for (const sessionId of sessionsNeedingResync(state)) {
        scheduler.request(sessionId);
      }
    },
    getSnapshotCalls,
    pendingFetches,
    advanceTime: (ms) => {
      now += ms;
      for (const timer of [...timers]) {
        if (!timer.cancelled && timer.at <= now) {
          timer.cancelled = true;
          timer.cb();
        }
      }
    },
    flush: async () => {
      await Promise.resolve();
      await Promise.resolve();
      await Promise.resolve();
    },
    dispose: () => scheduler.dispose(),
  };

  const scheduler = new ChatDeliveryResyncScheduler({
    recover: async (sessionId, requestedAt) => {
      getSnapshotCalls.push(sessionId);
      const snapshot = await new Promise<ChatSessionRuntimeSnapshot>((resolve, reject) => {
        pendingFetches.push({ sessionId, resolve, reject });
      });
      harness.dispatchSync({
        type: 'snapshot_received',
        sessionId: snapshot.session_id,
        deliveries: deliveriesFromRuntimeSnapshot(snapshot),
        revision: Number(snapshot.revision),
        requestedAt,
        receivedAt: now,
      });
    },
    onError: (sessionId) => {
      harness.dispatchSync({
        type: 'snapshot_failed',
        sessionId,
        receivedAt: now,
      });
    },
    now: () => now,
    setTimeoutFn: (cb, ms) => {
      const handle = nextHandle++;
      timers.push({ handle, at: now + ms, cb, cancelled: false });
      return handle;
    },
    clearTimeoutFn: (handle) => {
      const timer = timers.find((entry) => entry.handle === handle);
      if (timer) timer.cancelled = true;
    },
    baseDelayMs: 100,
    maxDelayMs: 1000,
  });

  return harness;
};

// ---------------------------------------------------------------------------
// CDD-004 刷新与切换 session：运行态可恢复
// ---------------------------------------------------------------------------

const runCdd004 = (): CaseReport => {
  const evidence: EvidenceEntry[] = [];
  let state = EMPTY_CHAT_DELIVERY_RUNTIME_STATE;

  // 1. Session A baseline: empty snapshot at revision 3.
  state = chatDeliveryRuntimeReducer(
    state,
    snapshotAction('session-A', 3, [], T0, T0 + 1),
  );
  evidence.push({
    step: 'A baseline snapshot',
    input: { session: 'A', revision: 3, deliveries: [], requestedAt: T0 },
  });

  // 2. Session A: backend reports the run as `starting` (snapshot rev 4).
  const startingRun = runDelivery('session-A', { status: 'starting' });
  state = chatDeliveryRuntimeReducer(
    state,
    snapshotAction('session-A', 4, [startingRun], T0 + 10, T0 + 11),
  );
  evidence.push({
    step: 'A snapshot: run starting',
    input: {
      session: 'A',
      revision: 4,
      deliveries: [describeDelivery(startingRun)],
      requestedAt: T0 + 10,
    },
  });
  check(
    describeProjection(state, 'session-A').length === 1 &&
      describeProjection(state, 'session-A')[0].runId === null,
    'starting refresh shows one card with the run id hidden',
  );

  // 3. Stream delta upgrades the same delivery to running (rev 5).
  state = chatDeliveryRuntimeReducer(
    state,
    upsertAction(runDelivery('session-A'), 5, T0 + 20),
  );
  evidence.push({
    step: 'A stream delta: run running',
    input: {
      session: 'A',
      revision: 5,
      delta: describeDelivery(runDelivery('session-A')),
    },
  });
  check(
    describeProjection(state, 'session-A').length === 1 &&
      describeProjection(state, 'session-A')[0].runId === 'run-1',
    'the same card upgrades starting → running in place (single stable identity)',
  );
  check(
    describeProjection(state, 'session-A')[0].clientMessageId ===
      'client-msg-1',
    'activity anchor (client_message_id) survives the upgrade',
  );

  // 4. Unversioned fallback: a snapshot requested before the rev-5 stream
  //    event (the classic refreshMessages race) merges without deleting or
  //    downgrading, because the local clock marks it stale.
  const unversioned = chatDeliveryRuntimeReducer(
    state,
    snapshotAction('session-A', undefined, [], T0, T0 + 25),
  );
  evidence.push({
    step: 'unversioned stale snapshot (local-clock guard)',
    input: { session: 'A', revision: null, deliveries: [], requestedAt: T0 },
  });
  check(
    describeProjection(unversioned, 'session-A')[0]?.runId === 'run-1',
    'unversioned stale snapshot cannot clear the running delivery',
  );

  // 5. Page refresh: in-memory state is rebuilt from a fresh snapshot (rev 6).
  state = chatDeliveryRuntimeReducer(
    EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
    snapshotAction(
      'session-A',
      6,
      [runDelivery('session-A')],
      T0 + 30,
      T0 + 31,
    ),
  );
  evidence.push({
    step: 'page refresh rehydrates from snapshot',
    input: {
      session: 'A',
      revision: 6,
      deliveries: [describeDelivery(runDelivery('session-A'))],
      requestedAt: T0 + 30,
    },
  });
  check(
    describeProjection(state, 'session-A')[0]?.runId === 'run-1',
    'refresh recovery keeps the running delivery',
  );

  // 6. Switch to session B: its own run appears, session A is untouched.
  const betaRun = runDelivery('session-B', {
    deliveryId: 'queue:delivery-b1',
    sessionAgentId: 'agent-beta',
    runId: 'run-b1',
    clientMessageId: 'client-msg-b1',
    sourceMessageId: 'msg-b1',
  });
  state = chatDeliveryRuntimeReducer(
    state,
    snapshotAction('session-B', 2, [betaRun], T0 + 40, T0 + 41),
  );
  evidence.push({
    step: 'switch A→B: session B snapshot',
    input: {
      session: 'B',
      revision: 2,
      deliveries: [describeDelivery(betaRun)],
      requestedAt: T0 + 40,
    },
  });
  check(
    describeProjection(state, 'session-B')[0]?.runId === 'run-b1',
    'session B projection is independent',
  );
  check(
    describeProjection(state, 'session-A')[0]?.runId === 'run-1',
    'session A projection survives the switch',
  );

  // 7. Switch back B→A: a stale snapshot (rev 4, still saying `starting`)
  //    must not roll back the running delivery at revision 6.
  const before = state;
  state = chatDeliveryRuntimeReducer(
    state,
    snapshotAction('session-A', 4, [startingRun], T0 + 50, T0 + 51),
  );
  evidence.push({
    step: 'switch B→A: stale snapshot rev 4 (starting) must be ignored',
    input: {
      session: 'A',
      revision: 4,
      deliveries: [describeDelivery(startingRun)],
      requestedAt: T0 + 50,
    },
  });
  assert.deepEqual(
    state,
    before,
    'older-revision snapshot is ignored entirely after switching back',
  );
  assertionCount += 1;
  check(
    describeProjection(state, 'session-A')[0]?.runId === 'run-1',
    'stale snapshot cannot downgrade running back to starting',
  );

  // 8. Equal-revision stale snapshot: a rev-6 snapshot whose request started
  //    BEFORE the newest stream event (requestedAt < lastEventAt) merges
  //    idempotently — no deletes, no downgrades. (The fresh equal-revision
  //    resync path is covered in CDD-005 and the scheduler integration test.)
  state = chatDeliveryRuntimeReducer(
    state,
    upsertAction(runDelivery('session-A'), 6, T0 + 55),
  );
  const lastEventAtBeforeStaleSnapshot =
    state.sessions['session-A']?.lastEventAt;
  const sameRevisionStale = chatDeliveryRuntimeReducer(
    state,
    snapshotAction('session-A', 6, [startingRun], T0 + 50, T0 + 60),
  );
  evidence.push({
    step: 'equal-revision stale snapshot (rev 6) must merge, not replace',
    input: {
      session: 'A',
      revision: 6,
      deliveries: [describeDelivery(startingRun)],
      requestedAt: T0 + 50,
      lastEventAt: lastEventAtBeforeStaleSnapshot,
      staleJudgement: `requestedAt(${T0 + 50}) < lastEventAt(${lastEventAtBeforeStaleSnapshot})`,
    },
  });
  check(
    (lastEventAtBeforeStaleSnapshot ?? 0) > T0 + 50,
    'fixture guarantees requestedAt < lastEventAt for the stale judgement',
  );
  check(
    describeProjection(sameRevisionStale, 'session-A')[0]?.runId === 'run-1',
    'equal-revision stale snapshot does not downgrade the running delivery',
  );
  check(
    describeProjection(sameRevisionStale, 'session-A').length === 1,
    'equal-revision stale snapshot does not duplicate the card',
  );

  return {
    case: 'CDD-004',
    evidence,
    projection: {
      sessionA: describeProjection(state, 'session-A'),
      sessionB: describeProjection(state, 'session-B'),
      sessionARevision: state.sessions['session-A']?.revision,
    },
    assertions: assertionCount,
  };
};

// ---------------------------------------------------------------------------
// CDD-005 WS 重连、重复、乱序与缺口收敛（驱动生产 resync 消费器）
// ---------------------------------------------------------------------------

const runCdd005 = async (): Promise<CaseReport> => {
  const evidence: EvidenceEntry[] = [];
  let state = EMPTY_CHAT_DELIVERY_RUNTIME_STATE;

  // 1. Baseline: snapshot rev 10 with run-1 running.
  const run1 = runDelivery('session-A');
  state = chatDeliveryRuntimeReducer(
    state,
    snapshotAction('session-A', 10, [run1], T0, T0 + 1),
  );
  evidence.push({
    step: 'baseline snapshot rev 10',
    input: {
      session: 'A',
      revision: 10,
      deliveries: [describeDelivery(run1)],
      requestedAt: T0,
    },
  });

  // 2. WS reconnect replays a duplicate delta (rev 10, same content).
  const beforeDuplicate = state;
  state = chatDeliveryRuntimeReducer(
    state,
    upsertAction(runDelivery('session-A'), 10, T0 + 10),
  );
  evidence.push({
    step: 'WS reconnect: duplicate delta rev 10',
    input: {
      session: 'A',
      revision: 10,
      delta: describeDelivery(runDelivery('session-A')),
    },
  });
  assert.deepEqual(
    describeProjection(state, 'session-A'),
    describeProjection(beforeDuplicate, 'session-A'),
    'duplicate delta leaves the projection unchanged (no second card)',
  );
  assertionCount += 1;

  // 3. Out-of-order deltas: the terminal transition (rev 12) is followed by
  //    a late running observation (rev 11). Terminal must stay sticky.
  state = chatDeliveryRuntimeReducer(
    state,
    terminalAction('session-A', 'agent-alpha', 'run-1', 12, T0 + 20),
  );
  evidence.push({
    step: 'terminal delta rev 12 (run-1 completed)',
    input: { session: 'A', revision: 12, runId: 'run-1', status: 'completed' },
  });
  state = chatDeliveryRuntimeReducer(
    state,
    upsertAction(runDelivery('session-A'), 11, T0 + 21),
  );
  evidence.push({
    step: 'late running delta rev 11 (out of order)',
    input: {
      session: 'A',
      revision: 11,
      delta: describeDelivery(runDelivery('session-A')),
    },
  });
  check(
    describeProjection(state, 'session-A').length === 0,
    'out-of-order running delta cannot resurrect a completed run',
  );

  // 4. Revision gap (12 → 14) flags the session; the production consumer
  //    (single-flight scheduler + getSnapshot) is driven from here on.
  const harness = createConsumerHarness(state);
  const run2 = runDelivery('session-A', {
    deliveryId: 'queue:delivery-2',
    runId: 'run-2',
    sourceMessageId: 'msg-2',
    clientMessageId: 'client-msg-2',
  });
  harness.dispatchSync(upsertAction(run2, 14, T0 + 30));
  // A ghost delivery the missing rev-13 event would have terminated; only a
  // fresh authoritative resync may delete it.
  const ghost = runDelivery('session-A', {
    deliveryId: 'queue:ghost',
    runId: 'run-ghost',
    sourceMessageId: 'msg-ghost',
    clientMessageId: 'client-msg-ghost',
  });
  harness.dispatchSync(upsertAction(ghost, 14, T0 + 31));
  evidence.push({
    step: 'delta rev 14 leaves a gap (13 missing) plus a ghost delivery',
    input: {
      session: 'A',
      revision: 14,
      delta: [describeDelivery(run2), describeDelivery(ghost)],
    },
  });
  assert.deepEqual(
    sessionsNeedingResync(harness.state()),
    ['session-A'],
    'resync consumer sees exactly the gapped session',
  );
  assertionCount += 1;
  check(
    describeProjection(harness.state(), 'session-A').length === 2,
    'ghost delivery is visible before the authoritative resync',
  );

  // 5. Repeated gap notifications coalesce into one in-flight request; the
  //    first fetch fails, the projection is preserved, the backoff timer
  //    retries automatically, and the retried fetch converges the session.
  harness.requestFlaggedSessions();
  harness.requestFlaggedSessions();
  check(
    harness.getSnapshotCalls.length === 1,
    'repeated gaps trigger only one in-flight snapshot request',
  );
  harness.pendingFetches[0].reject(new Error('network down'));
  await harness.flush();
  check(
    describeProjection(harness.state(), 'session-A').length === 2 &&
      sessionsNeedingResync(harness.state()).length === 1,
    'first resync failure preserves the projection and keeps needsResync',
  );
  check(
    harness.getSnapshotCalls.length === 1,
    'failure does not refetch immediately',
  );
  evidence.push({
    step: 'consumer: first getSnapshot fails (network down)',
    input: { session: 'A', outcome: 'rejected', getSnapshotCalls: 1 },
  });
  // attempts=1 → backoff = base(100ms) * 2^1 = 200ms.
  harness.advanceTime(200);
  await harness.flush();
  check(
    harness.getSnapshotCalls.length === 2,
    'backoff expiry retries the snapshot request automatically',
  );
  harness.pendingFetches[1].resolve(
    makeRawSnapshot('session-A', 14, [
      { deliveryId: 'delivery-2', runId: 'run-2', status: 'running' },
    ]),
  );
  await harness.flush();
  evidence.push({
    step: 'consumer: retried getSnapshot succeeds (rev 14)',
    input: { session: 'A', revision: 14, getSnapshotCalls: 2 },
  });
  check(
    sessionsNeedingResync(harness.state()).length === 0,
    'successful resync clears needsResync via the production consumer',
  );
  check(
    harness.state().sessions['session-A']?.revision === 14,
    'client converges exactly to the newest revision',
  );
  assert.deepEqual(
    describeProjection(harness.state(), 'session-A'),
    [
      {
        cardId: 'delivery-card-queue:delivery-2',
        sessionAgentId: 'agent-alpha',
        runId: 'run-2',
        clientMessageId: 'client-msg-1',
      },
    ],
    'fresh equal-revision resync replaces precisely and deletes the ghost card',
  );
  assertionCount += 1;

  // 6. An unknown queue status must not be guessed: the adapter throws, the
  //    caller marks the session for resync, and the production consumer
  //    actually refetches and converges the session again.
  const unknownQueue = makeQueue('session-A', 15, [
    makeQueueItem('mystery-status'),
  ]);
  let adapterError: string | null = null;
  try {
    deliveriesFromMemberQueue(unknownQueue);
  } catch (error) {
    adapterError = error instanceof Error ? error.message : String(error);
  }
  check(
    adapterError !== null && adapterError.includes('mystery-status'),
    'unknown queue status throws instead of defaulting to completed',
  );
  harness.dispatchSync({
    type: 'mark_needs_resync',
    sessionId: 'session-A',
    reason: adapterError ?? undefined,
  });
  evidence.push({
    step: 'unknown queue status → mark_needs_resync',
    input: { session: 'A', error: adapterError },
  });
  assert.deepEqual(
    sessionsNeedingResync(harness.state()),
    ['session-A'],
    'unknown status routes through the resync consumer',
  );
  assertionCount += 1;
  harness.requestFlaggedSessions();
  check(
    harness.getSnapshotCalls.length === 3,
    'the consumer refetches after the unknown status',
  );
  harness.pendingFetches[2].resolve(
    makeRawSnapshot('session-A', 15, [
      { deliveryId: 'delivery-2', runId: 'run-2', status: 'running' },
    ]),
  );
  await harness.flush();
  evidence.push({
    step: 'consumer: getSnapshot succeeds (rev 15) after unknown status',
    input: { session: 'A', revision: 15, getSnapshotCalls: 3 },
  });
  check(
    sessionsNeedingResync(harness.state()).length === 0 &&
      describeProjection(harness.state(), 'session-A').length === 1,
    'the consumer actually recovers from the unknown status',
  );

  state = harness.state();
  // 7. Conversation projection: the card disappears once the run's final
  //    reply is persisted (intermediate messages never ended the run above).
  const cards = deliveryCardsForSession(state, 'session-A').map(
    deliveryCardToMessage,
  );
  const withFinal = mergePersistedWithDeliveryCards(
    [
      {
        id: 'persisted-final',
        avatar: 'AL',
        sender: '@Alpha',
        time: 'just now',
        text: 'done',
        isAgent: true,
        runId: 'run-2',
        sessionAgentId: 'agent-alpha',
      },
    ],
    cards,
  );
  check(
    withFinal.length === 1 && withFinal[0].id === 'persisted-final',
    'persisted final reply replaces the delivery card',
  );

  harness.dispose();

  return {
    case: 'CDD-005',
    evidence,
    projection: {
      sessionA: describeProjection(state, 'session-A'),
      sessionARevision: state.sessions['session-A']?.revision,
      needsResync: sessionsNeedingResync(state),
      getSnapshotCalls: harness.getSnapshotCalls.length,
    },
    assertions: assertionCount,
  };
};

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

const CASES: Record<string, () => CaseReport | Promise<CaseReport>> = {
  'CDD-004': runCdd004,
  'CDD-005': runCdd005,
};

const main = async () => {
  const flagIndex = process.argv.indexOf('--case');
  const requested =
    flagIndex >= 0 ? process.argv[flagIndex + 1] : undefined;
  if (requested !== undefined && !CASES[requested]) {
    console.error(
      `unknown --case ${requested}; expected one of ${Object.keys(CASES).join(', ')}`,
    );
    process.exit(2);
  }
  const caseIds = requested ? [requested] : Object.keys(CASES);
  for (const caseId of caseIds) {
    assertionCount = 0;
    const report = await CASES[caseId]();
    console.log(JSON.stringify(report, null, 2));
    console.log(
      `${caseId}: PASS (${report.assertions} assertions, evidence above)`,
    );
  }
};

await main();
