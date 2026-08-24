import assert from 'node:assert/strict';

import type { ChatSessionRuntimeSnapshot, MemberQueueSnapshot } from '@/types';
import {
  EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
  chatDeliveryRuntimeReducer,
  deliveriesFromMemberQueue,
  deliveriesFromRuntimeSnapshot,
  deliveryCardsForSession,
  deliveryFromAgentRunStarted,
  hasInflightDeliveryForSession,
  mergePersistedWithDeliveryCards,
  deliveryCardToMessage,
  queuedMessageStatusToDeliveryStatus,
  sessionsNeedingResync,
  type ChatDelivery,
  type ChatDeliveryRuntimeState,
} from './chatDeliveryRuntime';
import type { ChatStreamEvent } from './workspaceChatStreamTypes';

const T0 = 1_000_000;
const ISO_T0 = new Date(T0).toISOString();

const runStartedEvent = (
  overrides: Partial<
    Extract<ChatStreamEvent, { type: 'agent_run_started' }>
  > = {},
): Extract<ChatStreamEvent, { type: 'agent_run_started' }> => ({
  type: 'agent_run_started',
  session_id: 'session-1',
  session_agent_id: 'agent-alpha',
  agent_id: 'agent-alpha-id',
  agent_name: 'Alpha',
  model: null,
  delivery_id: 'delivery-1',
  run_id: 'run-1',
  source_message_id: 'msg-1',
  client_message_id: 'client-msg-1',
  started_at: ISO_T0,
  ...overrides,
});

const makeDelivery = (overrides: Partial<ChatDelivery> = {}): ChatDelivery => ({
  deliveryId: 'queue:delivery-1',
  sessionId: 'session-1',
  sessionAgentId: 'agent-alpha',
  runId: 'run-1',
  status: 'running',
  createdAt: ISO_T0,
  updatedAt: ISO_T0,
  ...overrides,
});

const makeQueueMessage = (
  overrides: Record<string, unknown> = {},
): MemberQueueSnapshot['items'][number]['message'] =>
  ({
    id: 'delivery-1',
    session_id: 'session-1',
    session_agent_id: 'agent-alpha',
    agent_id: 'agent-alpha-id',
    chat_message_id: 'msg-1',
    status: 'running',
    revision: 0n,
    attempt_no: 0n,
    created_at: ISO_T0,
    updated_at: ISO_T0,
    processing_started_at: ISO_T0,
    run_id: 'run-1',
    failure_reason: null,
    ...overrides,
  }) as MemberQueueSnapshot['items'][number]['message'];

const makeQueue = (
  overrides: Partial<MemberQueueSnapshot> = {},
): MemberQueueSnapshot => ({
  session_id: 'session-1',
  revision: 0n,
  session_agent_id: 'agent-alpha',
  agent_id: 'agent-alpha-id',
  status: 'queued',
  blocked: false,
  paused: false,
  can_continue: false,
  queued_count: 1n,
  items: [],
  ...overrides,
});

const makeActiveRun = (
  overrides: Record<string, unknown> = {},
): ChatSessionRuntimeSnapshot['active_runs'][number] =>
  ({
    delivery_id: 'delivery-1',
    run_id: 'run-1',
    session_id: 'session-1',
    session_agent_id: 'agent-alpha',
    agent_id: 'agent-alpha-id',
    agent_name: 'Alpha',
    display_name: '@Alpha',
    avatar: 'AL',
    model: null,
    status: 'running',
    source_message_id: 'msg-1',
    client_message_id: 'client-msg-1',
    created_at: ISO_T0,
    ...overrides,
  }) as ChatSessionRuntimeSnapshot['active_runs'][number];

const dispatch = (
  state: ChatDeliveryRuntimeState,
  ...actions: Parameters<typeof chatDeliveryRuntimeReducer>[1][]
): ChatDeliveryRuntimeState =>
  actions.reduce(chatDeliveryRuntimeReducer, state);

const cardsFor = (state: ChatDeliveryRuntimeState, sessionId = 'session-1') =>
  deliveryCardsForSession(state, sessionId);

// ---------------------------------------------------------------------------
// Streamed run events and the canonical delivery identity
// ---------------------------------------------------------------------------

{
  const state = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 10,
  });
  const cards = cardsFor(state);
  assert.equal(cards.length, 1, 'run start surfaces one delivery card');
  assert.equal(
    cards[0].deliveryId,
    'queue:delivery-1',
    'run events use the backend delivery id as the canonical identity',
  );
  assert.equal(cards[0].status, 'running');
  assert.equal(cards[0].runId, 'run-1');
}

{
  // The run event and the queue snapshot share one canonical identity:
  // starting/running never produce an old card plus a new card.
  const queue = makeQueue({
    items: [
      { can_delete: false, message: makeQueueMessage({ status: 'starting', run_id: null }) },
    ],
  });
  const state = dispatch(
    EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
    {
      type: 'member_queue_snapshot',
      sessionId: 'session-1',
      sessionAgentId: 'agent-alpha',
      deliveries: deliveriesFromMemberQueue(queue),
      revision: 5,
      receivedAt: T0 + 10,
    },
    {
      type: 'delivery_upsert',
      delivery: deliveryFromAgentRunStarted(runStartedEvent()),
      receivedAt: T0 + 11,
    },
  );
  const cards = cardsFor(state);
  assert.equal(cards.length, 1, 'starting and running stay a single card');
  assert.equal(cards[0].status, 'running', 'the run event upgrades starting in place');
  assert.equal(cards[0].runId, 'run-1');
  assert.equal(cards[0].clientMessageId, 'client-msg-1');
}

{
  // Duplicate events are idempotent.
  const once = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 10,
  });
  const twice = dispatch(once, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 10,
  });
  assert.deepEqual(cardsFor(twice), cardsFor(once), 'duplicate run_started keeps one card');
}

{
  // Out-of-order: a terminal transition must stay terminal even if a delayed
  // running observation arrives afterwards.
  const running = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 10,
  });
  const terminated = dispatch(running, {
    type: 'delivery_terminal',
    sessionId: 'session-1',
    sessionAgentId: 'agent-alpha',
    runId: 'run-1',
    status: 'completed',
    receivedAt: T0 + 20,
  });
  assert.equal(cardsFor(terminated).length, 0, 'terminal transition ends the card');
  const lateRunning = dispatch(terminated, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 5,
  });
  assert.equal(
    cardsFor(lateRunning).length,
    0,
    'out-of-order running event cannot resurrect a terminal delivery',
  );
}

{
  // Status never downgrades.
  const running = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 10,
  });
  const downgraded = dispatch(running, {
    type: 'delivery_upsert',
    delivery: makeDelivery({ status: 'starting' }),
    receivedAt: T0 + 20,
  });
  assert.equal(
    cardsFor(downgraded)[0]?.status,
    'running',
    'starting observation cannot downgrade a running delivery',
  );
}

{
  // No same-agent guessing: a new run for a different message must NOT cancel
  // the in-flight delivery of the same agent.
  const state = dispatch(
    EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
    {
      type: 'delivery_upsert',
      delivery: deliveryFromAgentRunStarted(runStartedEvent()),
      receivedAt: T0 + 10,
    },
    {
      type: 'delivery_upsert',
      delivery: deliveryFromAgentRunStarted(
        runStartedEvent({
          delivery_id: 'delivery-2',
          run_id: 'run-2',
          source_message_id: 'msg-2',
        }),
      ),
      receivedAt: T0 + 11,
    },
  );
  assert.equal(
    cardsFor(state).length,
    2,
    'a new run never cancels another in-flight delivery by agent guesswork',
  );
}

// ---------------------------------------------------------------------------
// delivery_terminal without runId
// ---------------------------------------------------------------------------

{
  const queue = makeQueue({
    items: [
      { can_delete: true, message: makeQueueMessage({ id: 'delivery-queued', chat_message_id: 'msg-2', status: 'queued', run_id: null }) },
    ],
  });
  const state = dispatch(
    EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
    {
      type: 'delivery_upsert',
      delivery: deliveryFromAgentRunStarted(runStartedEvent()),
      receivedAt: T0 + 10,
    },
    {
      type: 'delivery_upsert',
      delivery: deliveriesFromMemberQueue(queue)[0],
      receivedAt: T0 + 11,
    },
    {
      type: 'delivery_terminal',
      sessionId: 'session-1',
      sessionAgentId: 'agent-alpha',
      status: 'completed',
      receivedAt: T0 + 12,
    },
  );
  const deliveries = state.sessions['session-1']?.deliveries ?? {};
  assert.equal(
    deliveries['queue:delivery-1']?.status,
    'completed',
    'the unique active delivery ends',
  );
  assert.equal(
    deliveries['queue:delivery-queued']?.status,
    'queued',
    'queued deliveries are never terminated by a run-id-less terminal event',
  );
}

{
  // Ambiguous run-id-less terminal transitions request a resync.
  const state = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_terminal',
    sessionId: 'session-1',
    sessionAgentId: 'agent-alpha',
    status: 'completed',
    receivedAt: T0 + 10,
  });
  assert.equal(state.sessions['session-1']?.needsResync, true);
  assert.deepEqual(sessionsNeedingResync(state), ['session-1']);
}

// ---------------------------------------------------------------------------
// Snapshot revision and freshness semantics
// ---------------------------------------------------------------------------

{
  // Stale (requested before the newest event) snapshots merge only.
  const withRun = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 100,
  });
  const stale = dispatch(withRun, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [],
    requestedAt: T0,
    receivedAt: T0 + 200,
  });
  assert.equal(
    cardsFor(stale)[0]?.status,
    'running',
    'stale snapshot cannot delete a run reported by a newer stream event',
  );
  assert.equal(stale.sessions['session-1']?.hydrated, true);
}

{
  // Fresh newer-revision snapshots replace the whole projection.
  const withRun = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 10,
  });
  const fresh = dispatch(withRun, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [],
    revision: 7,
    requestedAt: T0 + 100,
    receivedAt: T0 + 200,
  });
  assert.deepEqual(cardsFor(fresh), [], 'fresh newer snapshot drops finished runs');
}

{
  // Strictly-older revision snapshots are ignored entirely.
  const revised = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [makeDelivery()],
    revision: 5,
    requestedAt: T0,
    receivedAt: T0 + 10,
  });
  const olderIgnored = dispatch(revised, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [],
    revision: 3,
    requestedAt: T0 + 20,
    receivedAt: T0 + 30,
  });
  assert.deepEqual(olderIgnored, revised, 'older-revision snapshot is ignored');
}

{
  // Equal-revision snapshots: stale ones merge-only (no delete, no
  // downgrade); fresh ones are the authoritative resync answer and replace.
  const withRun = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    revision: 5,
    receivedAt: T0 + 100,
  });
  const sameStale = dispatch(withRun, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [],
    revision: 5,
    requestedAt: T0,
    receivedAt: T0 + 200,
  });
  assert.equal(
    cardsFor(sameStale).length,
    1,
    'equal-revision stale snapshot never deletes known deliveries',
  );
  const sameStaleDowngrade = dispatch(withRun, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [makeDelivery({ status: 'starting' })],
    revision: 5,
    requestedAt: T0,
    receivedAt: T0 + 200,
  });
  assert.equal(
    cardsFor(sameStaleDowngrade)[0]?.status,
    'running',
    'equal-revision stale snapshot never downgrades known deliveries',
  );
  const ghost = makeDelivery({
    deliveryId: 'queue:ghost',
    runId: 'run-ghost',
    sourceMessageId: 'msg-ghost',
  });
  const withGhost = dispatch(withRun, {
    type: 'delivery_upsert',
    delivery: ghost,
    revision: 5,
    receivedAt: T0 + 110,
  });
  const sameFresh = dispatch(withGhost, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [makeDelivery()],
    revision: 5,
    requestedAt: T0 + 120,
    receivedAt: T0 + 200,
  });
  assert.deepEqual(
    cardsFor(sameFresh).map((card) => card.deliveryId),
    ['queue:delivery-1'],
    'equal-revision fresh snapshot replaces and deletes ghost deliveries',
  );
}

{
  // Revision gap detection and equal-revision hydration.
  const withRevision = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    revision: 7,
    receivedAt: T0 + 10,
  });
  const gapped = dispatch(withRevision, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(
      runStartedEvent({ delivery_id: 'delivery-2', run_id: 'run-2', source_message_id: 'msg-2' }),
    ),
    revision: 9,
    receivedAt: T0 + 20,
  });
  assert.equal(gapped.sessions['session-1']?.needsResync, true, 'gap flags resync');
  const hydrated = dispatch(gapped, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [
      makeDelivery({
        deliveryId: 'queue:delivery-2',
        runId: 'run-2',
        sourceMessageId: 'msg-2',
      }),
    ],
    revision: 9,
    requestedAt: T0 + 30,
    receivedAt: T0 + 31,
  });
  assert.equal(
    hydrated.sessions['session-1']?.needsResync,
    false,
    'equal-revision hydration snapshot clears the resync flag',
  );
  assert.deepEqual(
    cardsFor(hydrated).map((card) => card.deliveryId),
    ['queue:delivery-2'],
    'hydration replaces the gapped projection with the authoritative state',
  );
}

{
  // Snapshot failure preserves the current projection.
  const withRun = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 10,
  });
  const failed = dispatch(withRun, {
    type: 'snapshot_failed',
    sessionId: 'session-1',
    error: 'network down',
    receivedAt: T0 + 20,
  });
  assert.equal(cardsFor(failed).length, 1, 'snapshot failure keeps deliveries');
  assert.equal(failed.sessions['session-1']?.lastError, 'network down');
}

// ---------------------------------------------------------------------------
// Member queue snapshots (authoritative per-member replace)
// ---------------------------------------------------------------------------

{
  const queue = makeQueue({
    items: [{ can_delete: false, message: makeQueueMessage() }],
  });
  const deliveries = deliveriesFromMemberQueue(queue);
  assert.equal(deliveries[0].runId, 'run-1', 'queue snapshot carries the bound run id');
  assert.equal(deliveries[0].deliveryId, 'queue:delivery-1');
  const state = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'member_queue_snapshot',
    sessionId: 'session-1',
    sessionAgentId: 'agent-alpha',
    deliveries,
    revision: 6,
    receivedAt: T0 + 10,
  });
  assert.equal(
    cardsFor(state)[0]?.status,
    'running',
    'queue-bound running delivery upgrades the projection without agent_run_started',
  );
}

{
  // A member queue snapshot terminates deliveries it no longer represents;
  // a stale one (older revision) changes nothing.
  const queue = makeQueue({
    items: [{ can_delete: false, message: makeQueueMessage() }],
  });
  const withRun = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'member_queue_snapshot',
    sessionId: 'session-1',
    sessionAgentId: 'agent-alpha',
    deliveries: deliveriesFromMemberQueue(queue),
    revision: 6,
    receivedAt: T0 + 10,
  });
  const emptied = dispatch(withRun, {
    type: 'member_queue_snapshot',
    sessionId: 'session-1',
    sessionAgentId: 'agent-alpha',
    deliveries: [],
    revision: 7,
    receivedAt: T0 + 20,
  });
  assert.equal(
    cardsFor(emptied).length,
    0,
    'member queue snapshot cancels deliveries it no longer represents',
  );
  const staleIgnored = dispatch(withRun, {
    type: 'member_queue_snapshot',
    sessionId: 'session-1',
    sessionAgentId: 'agent-alpha',
    deliveries: [],
    revision: 5,
    receivedAt: T0 + 15,
  });
  assert.equal(
    cardsFor(staleIgnored).length,
    1,
    'older-revision member queue snapshot is ignored',
  );
}

// ---------------------------------------------------------------------------
// Exhaustive queue status mapping
// ---------------------------------------------------------------------------

{
  const expected: Record<string, string> = {
    queued: 'queued',
    starting: 'starting',
    processing: 'starting',
    running: 'running',
    waiting_approval: 'waiting_approval',
    stopping: 'stopping',
    failed: 'failed',
    cancelled: 'cancelled',
    skipped: 'cancelled',
    completed: 'completed',
  };
  for (const [input, output] of Object.entries(expected)) {
    assert.equal(
      queuedMessageStatusToDeliveryStatus(input),
      output,
      `status ${input} maps to ${output}`,
    );
  }
  assert.throws(
    () => queuedMessageStatusToDeliveryStatus('mystery-status'),
    /unknown queued message status/,
    'unknown statuses throw instead of guessing a terminal state',
  );
}

// ---------------------------------------------------------------------------
// Multi-agent and session partitioning
// ---------------------------------------------------------------------------

{
  const state = dispatch(
    EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
    {
      type: 'delivery_upsert',
      delivery: deliveryFromAgentRunStarted(runStartedEvent()),
      receivedAt: T0 + 10,
    },
    {
      type: 'delivery_upsert',
      delivery: deliveryFromAgentRunStarted(
        runStartedEvent({
          delivery_id: 'delivery-b1',
          session_agent_id: 'agent-beta',
          agent_id: 'agent-beta-id',
          agent_name: 'Beta',
          run_id: 'run-beta-1',
        }),
      ),
      receivedAt: T0 + 11,
    },
  );
  assert.equal(cardsFor(state).length, 2, 'multiple agents run independently');
}

{
  const state = dispatch(
    EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
    {
      type: 'delivery_upsert',
      delivery: deliveryFromAgentRunStarted(runStartedEvent()),
      receivedAt: T0 + 10,
    },
    {
      type: 'delivery_upsert',
      delivery: deliveryFromAgentRunStarted(
        runStartedEvent({ session_id: 'session-2', run_id: 'run-other', delivery_id: 'delivery-other' }),
      ),
      receivedAt: T0 + 11,
    },
    {
      type: 'delivery_terminal',
      sessionId: 'session-2',
      sessionAgentId: 'agent-alpha',
      runId: 'run-other',
      status: 'completed',
      receivedAt: T0 + 12,
    },
  );
  assert.equal(hasInflightDeliveryForSession(state, 'session-1'), true);
  assert.equal(hasInflightDeliveryForSession(state, 'session-2'), false);
}

// ---------------------------------------------------------------------------
// Snapshot adapter details and the card projection
// ---------------------------------------------------------------------------

{
  const snapshot: ChatSessionRuntimeSnapshot = {
    session_id: 'session-1',
    revision: 6n,
    messages: null,
    active_runs: [makeActiveRun({ status: 'starting' })],
    queues: [
      makeQueue({
        items: [
          {
            can_delete: false,
            message: makeQueueMessage({ status: 'starting' }),
          },
        ],
      }),
    ],
  };
  const deliveries = deliveriesFromRuntimeSnapshot(snapshot);
  const state = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries,
    revision: 6,
    requestedAt: T0,
    receivedAt: T0 + 10,
  });
  const deliveries_ = state.sessions['session-1']?.deliveries ?? {};
  assert.deepEqual(
    Object.keys(deliveries_),
    ['queue:delivery-1'],
    'snapshot active run and queue item share one canonical identity',
  );
  assert.equal(deliveries_['queue:delivery-1']?.status, 'starting');
  assert.equal(deliveries_['queue:delivery-1']?.displayName, '@Alpha');

  const cards = cardsFor(state).map(deliveryCardToMessage);
  assert.equal(cards.length, 1);
  assert.equal(cards[0].runId, undefined, 'starting cards hide the run id');
  assert.equal(cards[0].isAgentRunning, true);

  const upgraded = dispatch(state, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 20,
  });
  const runningCards = cardsFor(upgraded).map(deliveryCardToMessage);
  assert.equal(runningCards.length, 1, 'starting and running stay a single card');
  assert.equal(runningCards[0].runId, 'run-1', 'running cards expose the run id');
}

{
  // mergePersistedWithDeliveryCards hides a card once its run's final reply
  // is persisted.
  const state = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    receivedAt: T0 + 10,
  });
  const cards = cardsFor(state).map(deliveryCardToMessage);
  const merged = mergePersistedWithDeliveryCards([], cards);
  assert.equal(merged.length, 1, 'card shows while the run is in flight');
  const withFinal = mergePersistedWithDeliveryCards(
    [
      {
        id: 'persisted-final',
        avatar: 'AL',
        sender: '@Alpha',
        time: 'just now',
        text: 'done',
        isAgent: true,
        runId: 'run-1',
        sessionAgentId: 'agent-alpha',
      },
    ],
    cards,
  );
  assert.equal(withFinal.length, 1, 'persisted final reply replaces the card');
  assert.equal(withFinal[0].id, 'persisted-final');
}

{
  // Stale snapshots must NOT clear needsResync (a gap may have opened while
  // the snapshot was in flight); fresh ones settle it.
  const gapped = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(runStartedEvent()),
    revision: 7,
    receivedAt: T0 + 10,
  });
  const gappedMore = dispatch(gapped, {
    type: 'delivery_upsert',
    delivery: deliveryFromAgentRunStarted(
      runStartedEvent({ delivery_id: 'delivery-2', run_id: 'run-2', source_message_id: 'msg-2' }),
    ),
    revision: 9,
    receivedAt: T0 + 20,
  });
  assert.equal(gappedMore.sessions['session-1']?.needsResync, true);
  const staleResponse = dispatch(gappedMore, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [makeDelivery()],
    revision: 8,
    requestedAt: T0,
    receivedAt: T0 + 30,
  });
  assert.equal(
    staleResponse.sessions['session-1']?.needsResync,
    true,
    'stale snapshot preserves needsResync',
  );
  const freshResponse = dispatch(gappedMore, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [makeDelivery()],
    revision: 9,
    requestedAt: T0 + 40,
    receivedAt: T0 + 50,
  });
  assert.equal(
    freshResponse.sessions['session-1']?.needsResync,
    false,
    'fresh snapshot clears needsResync',
  );
}

{
  // A member queue delta with a revision gap must not clear the resync flag;
  // only replay completion at the newest observed revision may settle it.
  const hydrated = dispatch(EMPTY_CHAT_DELIVERY_RUNTIME_STATE, {
    type: 'snapshot_received',
    sessionId: 'session-1',
    deliveries: [],
    revision: 7,
    requestedAt: T0,
    receivedAt: T0 + 1,
  });
  const gapped = dispatch(hydrated, {
    type: 'member_queue_snapshot',
    sessionId: 'session-1',
    sessionAgentId: 'agent-alpha',
    deliveries: deliveriesFromMemberQueue(
      makeQueue({ revision: 9n, items: [] }),
    ),
    revision: 9,
    receivedAt: T0 + 2,
  });
  assert.equal(gapped.sessions['session-1']?.needsResync, true);
  const staleReplay = dispatch(gapped, {
    type: 'replay_completed',
    sessionId: 'session-1',
    revision: 8,
    receivedAt: T0 + 3,
  });
  assert.equal(staleReplay.sessions['session-1']?.needsResync, true);
  const recovered = dispatch(staleReplay, {
    type: 'replay_completed',
    sessionId: 'session-1',
    revision: 9,
    receivedAt: T0 + 4,
  });
  assert.equal(recovered.sessions['session-1']?.needsResync, false);
}

console.log('chatDeliveryRuntime tests passed');
