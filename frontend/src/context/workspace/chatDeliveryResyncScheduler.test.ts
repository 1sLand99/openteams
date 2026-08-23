// Integration test for the single-flight resync scheduler driving the real
// delivery reducer: fake clock + fake timer queue + controllable getSnapshot.
import assert from 'node:assert/strict';

import type { ChatSessionRuntimeSnapshot } from '@/types';
import {
  EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
  chatDeliveryRuntimeReducer,
  deliveriesFromRuntimeSnapshot,
  deliveryCardsForSession,
  sessionsNeedingResync,
  type ChatDelivery,
  type ChatDeliveryRuntimeState,
} from './chatDeliveryRuntime';
import { ChatDeliveryResyncScheduler } from './chatDeliveryResyncScheduler';

const T0 = 1_000_000;
const ISO_T0 = new Date(T0).toISOString();

// ---- fake clock + timer queue ----------------------------------------------
let now = 0;
let nextHandle = 1;
const timers: Array<{
  handle: number;
  at: number;
  cb: () => void;
  cancelled: boolean;
}> = [];
const setTimeoutFn = (cb: () => void, ms: number) => {
  const handle = nextHandle++;
  timers.push({ handle, at: now + ms, cb, cancelled: false });
  return handle;
};
const clearTimeoutFn = (handle: unknown) => {
  const timer = timers.find((entry) => entry.handle === handle);
  if (timer) timer.cancelled = true;
};
const advanceTime = (ms: number) => {
  now += ms;
  for (const timer of [...timers]) {
    if (!timer.cancelled && timer.at <= now) {
      timer.cancelled = true;
      timer.cb();
    }
  }
};
const flushMicrotasks = async () => {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
};

// ---- controllable snapshot fetches -----------------------------------------
interface PendingFetch {
  sessionId: string;
  resolve: (snapshot: ChatSessionRuntimeSnapshot) => void;
  reject: (error: Error) => void;
}
const pendingFetches: PendingFetch[] = [];
const getSnapshotCalls: string[] = [];
const getSnapshot = (sessionId: string) => {
  getSnapshotCalls.push(sessionId);
  return new Promise<ChatSessionRuntimeSnapshot>((resolve, reject) => {
    pendingFetches.push({ sessionId, resolve, reject });
  });
};

// ---- production-like consumption: reducer + sync dispatch + scheduler ------
let state: ChatDeliveryRuntimeState = EMPTY_CHAT_DELIVERY_RUNTIME_STATE;
const dispatchSync = (
  action: Parameters<typeof chatDeliveryRuntimeReducer>[1],
): ChatDeliveryRuntimeState => {
  state = chatDeliveryRuntimeReducer(state, action);
  return state;
};

const scheduler = new ChatDeliveryResyncScheduler({
  getSnapshot,
  applySnapshot: (snapshot, requestedAt) => {
    dispatchSync({
      type: 'snapshot_received',
      sessionId: snapshot.session_id,
      deliveries: deliveriesFromRuntimeSnapshot(snapshot),
      revision: Number(snapshot.revision),
      requestedAt,
      receivedAt: now,
    });
  },
  onError: (sessionId) => {
    dispatchSync({ type: 'snapshot_failed', sessionId, receivedAt: now });
  },
  shouldResync: (sessionId) => sessionsNeedingResync(state).includes(sessionId),
  now: () => now,
  setTimeoutFn,
  clearTimeoutFn,
  baseDelayMs: 100,
  maxDelayMs: 1000,
});

const requestFlaggedSessions = () => {
  for (const sessionId of sessionsNeedingResync(state)) {
    scheduler.request(sessionId);
  }
};

const makeDelivery = (overrides: Partial<ChatDelivery> = {}): ChatDelivery => ({
  deliveryId: 'queue:delivery-1',
  sessionId: 'session-1',
  sessionAgentId: 'agent-alpha',
  agentName: 'Alpha',
  displayName: '@Alpha',
  sourceMessageId: 'msg-1',
  runId: 'run-1',
  status: 'running',
  createdAt: ISO_T0,
  updatedAt: ISO_T0,
  ...overrides,
});

const makeSnapshot = (
  sessionId: string,
  revision: number,
  runIds: string[],
): ChatSessionRuntimeSnapshot => ({
  session_id: sessionId,
  revision: BigInt(revision),
  messages: null,
  active_runs: runIds.map((runId, index) => ({
    delivery_id: `delivery-${runId}`,
    run_id: runId,
    session_id: sessionId,
    session_agent_id: 'agent-alpha',
    agent_id: 'agent-alpha-id',
    agent_name: 'Alpha',
    display_name: '@Alpha',
    avatar: 'AL',
    model: null,
    status: 'running' as const,
    source_message_id: `msg-${index}`,
    client_message_id: `client-${index}`,
    created_at: ISO_T0,
  })),
  queues: [],
});

const main = async () => {
  // Seed a run and open a revision gap (7 → 9) → needsResync.
  dispatchSync({
    type: 'delivery_upsert',
    delivery: makeDelivery(),
    revision: 7,
    receivedAt: now,
  });
  dispatchSync({
    type: 'delivery_upsert',
    delivery: makeDelivery({
      deliveryId: 'queue:ghost',
      runId: 'run-ghost',
      sourceMessageId: 'msg-ghost',
    }),
    revision: 9,
    receivedAt: now,
  });
  assert.equal(state.sessions['session-1']?.needsResync, true, 'gap flags resync');

  // 1. Repeated gap notifications coalesce into a single in-flight request.
  requestFlaggedSessions();
  requestFlaggedSessions();
  requestFlaggedSessions();
  assert.equal(
    getSnapshotCalls.length,
    1,
    'repeated gaps trigger only one in-flight snapshot request',
  );

  // 2. First fetch fails: projection is preserved, needsResync stays set,
  //    and a timed retry is scheduled (no immediate second request).
  pendingFetches[0].reject(new Error('network down'));
  await flushMicrotasks();
  assert.equal(
    deliveryCardsForSession(state, 'session-1').length,
    2,
    'first resync failure preserves the projection',
  );
  assert.equal(
    state.sessions['session-1']?.needsResync,
    true,
    'first resync failure keeps needsResync set',
  );
  assert.equal(
    getSnapshotCalls.length,
    1,
    'failure does not refetch immediately',
  );
  requestFlaggedSessions();
  assert.equal(
    getSnapshotCalls.length,
    1,
    'effect re-runs during backoff do not double-request',
  );

  // 3. The backoff timer fires and retries automatically; the retried fetch
  //    succeeds, clears needsResync and replaces the projection (ghost gone).
  //    attempts=1 → delay = base(100ms) * 2^1 = 200ms.
  advanceTime(200);
  await flushMicrotasks();
  assert.equal(
    getSnapshotCalls.length,
    2,
    'backoff expiry retries the snapshot request automatically',
  );
  pendingFetches[0 + 1].resolve(makeSnapshot('session-1', 9, ['run-1']));
  await flushMicrotasks();
  assert.equal(
    state.sessions['session-1']?.needsResync,
    false,
    'successful resync clears needsResync',
  );
  assert.deepEqual(
    deliveryCardsForSession(state, 'session-1').map((card) => card.deliveryId),
    ['queue:delivery-run-1'],
    'authoritative resync replaces the projection and deletes the ghost delivery',
  );
  assert.equal(
    state.sessions['session-1']?.revision,
    9,
    'resync converges to the gapped revision',
  );

  // 4. A new gap after convergence starts a fresh single-flight cycle.
  dispatchSync({
    type: 'delivery_upsert',
    delivery: makeDelivery({ deliveryId: 'queue:delivery-2', runId: 'run-2', sourceMessageId: 'msg-2' }),
    revision: 12,
    receivedAt: now,
  });
  assert.equal(state.sessions['session-1']?.needsResync, true);
  requestFlaggedSessions();
  assert.equal(getSnapshotCalls.length, 3, 'a new gap starts a fresh cycle');
  pendingFetches[2].resolve(makeSnapshot('session-1', 12, ['run-2']));
  await flushMicrotasks();
  assert.equal(state.sessions['session-1']?.needsResync, false);

  // 5. A gap raised while a snapshot is in flight: the stale response must
  //    NOT clear the flag, and the scheduler automatically fetches again and
  //    converges on the second, fresh response.
  dispatchSync({
    type: 'delivery_upsert',
    delivery: makeDelivery({ deliveryId: 'queue:delivery-2b', runId: 'run-2b', sourceMessageId: 'msg-2b' }),
    revision: 14,
    receivedAt: now,
  });
  requestFlaggedSessions();
  const callsBeforeMidFlightGap = getSnapshotCalls.length;
  assert.equal(
    getSnapshotCalls.length,
    callsBeforeMidFlightGap,
    'a new gap starts a fresh cycle',
  );
  // Mid-flight gap: later than the in-flight request's requestedAt, so the
  // in-flight response will be judged stale by the local clock.
  advanceTime(10);
  dispatchSync({
    type: 'delivery_upsert',
    delivery: makeDelivery({ deliveryId: 'queue:delivery-2c', runId: 'run-2c', sourceMessageId: 'msg-2c' }),
    revision: 16,
    receivedAt: now,
  });
  requestFlaggedSessions();
  assert.equal(
    getSnapshotCalls.length,
    callsBeforeMidFlightGap,
    'mid-flight gap coalesces into the in-flight request',
  );
  const midFlightFetch = pendingFetches[pendingFetches.length - 1];
  midFlightFetch.resolve(makeSnapshot('session-1', 15, ['run-2b']));
  await flushMicrotasks();
  assert.equal(
    state.sessions['session-1']?.needsResync,
    true,
    'stale response must not clear needsResync after a mid-flight gap',
  );
  await flushMicrotasks();
  assert.equal(
    getSnapshotCalls.length,
    callsBeforeMidFlightGap + 1,
    'scheduler automatically fetches again after a stale response',
  );
  const secondFetch = pendingFetches[pendingFetches.length - 1];
  secondFetch.resolve(makeSnapshot('session-1', 16, ['run-2c']));
  await flushMicrotasks();
  assert.equal(
    state.sessions['session-1']?.needsResync,
    false,
    'the second, fresh response clears the flag and converges',
  );
  assert.deepEqual(
    deliveryCardsForSession(state, 'session-1').map((card) => card.deliveryId),
    ['queue:delivery-run-2c'],
    'projection converges to the authoritative state after the second fetch',
  );

  // 6. dispose() cancels a pending retry and swallows late responses.
  dispatchSync({
    type: 'delivery_upsert',
    delivery: makeDelivery({ deliveryId: 'queue:delivery-3', runId: 'run-3', sourceMessageId: 'msg-3' }),
    revision: 18,
    receivedAt: now,
  });
  requestFlaggedSessions();
  const callsBeforeDisposeRetry = getSnapshotCalls.length;
  pendingFetches[pendingFetches.length - 1].reject(new Error('still down'));
  await flushMicrotasks();
  scheduler.dispose();
  advanceTime(10000);
  await flushMicrotasks();
  assert.equal(
    getSnapshotCalls.length,
    callsBeforeDisposeRetry,
    'dispose cancels the scheduled retry',
  );
  assert.equal(
    state.sessions['session-1']?.needsResync,
    true,
    'needsResync stays set for the next scheduler instance',
  );

  console.log('chatDeliveryResyncScheduler tests passed');
};

await main();