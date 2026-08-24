import type { Message } from '@/types';
import type {
  ChatActiveRunStatus,
  ChatSessionRuntimeSnapshot,
  MemberQueueSnapshot,
} from '@/types';
import type { ChatStreamEvent } from './workspaceChatStreamTypes';
import { orderMessagesForConversation } from './workspaceContextUtils';

/**
 * Authoritative chat delivery runtime, partitioned by session.
 *
 * A delivery represents one `source_message × session_agent` unit of work. Its
 * lifecycle is `queued → starting → running ↔ waiting_approval → stopping →
 * completed | failed | cancelled`. This reducer is the single writer for chat
 * run-time state; snapshots, member-queue snapshots and stream events all flow
 * through it.
 *
 * Identity: the backend delivery id (`chat_message_queue.id`, keyed as
 * `queue:<id>`) is the single canonical identity for starting and running
 * deliveries. Versioned queue deltas and runtime snapshots both carry it.
 * Observations correlate via the persisted `runId` binding or the exact
 * (sessionAgentId, sourceMessageId) pair — never same-agent guessing.
 *
 * Monotonicity guarantees:
 * - Status only advances along the rank order; terminal states are sticky.
 * - A strictly-older-revision snapshot is ignored entirely. An equal-revision
 *   snapshot hydrates idempotently (merge-only: no deletes, no downgrades).
 *   A newer snapshot fully replaces the projection when it is fresh by the
 *   local clock; otherwise it merges without deleting or downgrading.
 * - Snapshot/transport failures keep the current state untouched; unknown
 *   delivery statuses and ambiguous terminal transitions flag `needsResync`,
 *   which callers must consume by fetching an authoritative snapshot.
 */

export type ChatDeliveryStatus =
  | 'queued'
  | 'starting'
  | 'running'
  | 'waiting_approval'
  | 'stopping'
  | 'completed'
  | 'failed'
  | 'cancelled';

export const TERMINAL_CHAT_DELIVERY_STATUSES: ReadonlySet<ChatDeliveryStatus> =
  new Set(['completed', 'failed', 'cancelled']);

export const isTerminalChatDeliveryStatus = (
  status: ChatDeliveryStatus,
): boolean => TERMINAL_CHAT_DELIVERY_STATUSES.has(status);

const DELIVERY_STATUS_RANK: Record<ChatDeliveryStatus, number> = {
  queued: 0,
  starting: 1,
  running: 2,
  waiting_approval: 2,
  stopping: 3,
  completed: 4,
  failed: 4,
  cancelled: 4,
};

const CANONICAL_QUEUE_ID_PREFIX = 'queue:';

export interface ChatDelivery {
  /** Canonical identity: `queue:<backend delivery id>`. */
  deliveryId: string;
  sessionId: string;
  sessionAgentId: string;
  agentId?: string;
  agentName?: string;
  displayName?: string;
  avatar?: string;
  model?: string | null;
  sourceMessageId?: string;
  clientMessageId?: string;
  runId?: string;
  status: ChatDeliveryStatus;
  createdAt: string;
  updatedAt: string;
}

export interface SessionDeliveryRuntime {
  /** True once any snapshot has been accepted for this session. */
  hydrated: boolean;
  /** Highest backend runtime revision applied; -1 while unknown. */
  revision: number;
  /** Local clock (ms) of the newest streamed event applied. */
  lastEventAt: number;
  deliveries: Record<string, ChatDelivery>;
  /** Set when a revision gap or ambiguity is detected; callers must resync. */
  needsResync: boolean;
  lastError?: string;
}

export interface ChatDeliveryRuntimeState {
  sessions: Record<string, SessionDeliveryRuntime>;
}

export const EMPTY_CHAT_DELIVERY_RUNTIME_STATE: ChatDeliveryRuntimeState = {
  sessions: {},
};

export type ChatDeliveryTerminalStatus = 'completed' | 'failed' | 'cancelled';

export type ChatDeliveryRuntimeAction =
  | {
      type: 'delivery_upsert';
      delivery: ChatDelivery;
      revision?: number;
      receivedAt: number;
    }
  | {
      type: 'delivery_terminal';
      sessionId: string;
      sessionAgentId: string;
      runId?: string;
      status: ChatDeliveryTerminalStatus;
      revision?: number;
      receivedAt: number;
    }
  | {
      /** Authoritative per-member queue replace (queue_updated). */
      type: 'member_queue_snapshot';
      sessionId: string;
      sessionAgentId: string;
      deliveries: ChatDelivery[];
      revision?: number;
      receivedAt: number;
    }
  | {
      type: 'snapshot_received';
      sessionId: string;
      deliveries: ChatDelivery[];
      revision?: number;
      /** Local clock captured when the snapshot request was issued. */
      requestedAt: number;
      receivedAt: number;
    }
  | {
      type: 'snapshot_failed';
      sessionId: string;
      error?: string;
      receivedAt: number;
    }
  | {
      type: 'replay_completed';
      sessionId: string;
      revision: number;
      receivedAt: number;
    }
  | {
      type: 'mark_needs_resync';
      sessionId: string;
      reason?: string;
    };

const EMPTY_SESSION_RUNTIME: SessionDeliveryRuntime = {
  hydrated: false,
  revision: -1,
  lastEventAt: 0,
  deliveries: {},
  needsResync: false,
};

const sessionRuntimeOf = (
  state: ChatDeliveryRuntimeState,
  sessionId: string,
): SessionDeliveryRuntime => state.sessions[sessionId] ?? EMPTY_SESSION_RUNTIME;

const withSession = (
  state: ChatDeliveryRuntimeState,
  sessionId: string,
  next: SessionDeliveryRuntime,
): ChatDeliveryRuntimeState => ({
  sessions: { ...state.sessions, [sessionId]: next },
});

const mergeDefined = <T>(preferred: T | undefined, fallback: T | undefined) =>
  preferred !== undefined ? preferred : fallback;

/**
 * Merge an incoming delivery observation into the existing one. Status never
 * downgrades, terminal states are sticky, and already-known fields are only
 * filled in, never blanked out.
 */
export const mergeChatDelivery = (
  existing: ChatDelivery,
  incoming: ChatDelivery,
): ChatDelivery => {
  if (isTerminalChatDeliveryStatus(existing.status)) return existing;
  const status =
    DELIVERY_STATUS_RANK[incoming.status] >=
    DELIVERY_STATUS_RANK[existing.status]
      ? incoming.status
      : existing.status;
  return {
    ...existing,
    agentId: mergeDefined(incoming.agentId, existing.agentId),
    agentName: mergeDefined(incoming.agentName, existing.agentName),
    displayName: mergeDefined(incoming.displayName, existing.displayName),
    avatar: mergeDefined(incoming.avatar, existing.avatar),
    model: mergeDefined(incoming.model, existing.model),
    sourceMessageId: mergeDefined(
      incoming.sourceMessageId,
      existing.sourceMessageId,
    ),
    clientMessageId: mergeDefined(
      incoming.clientMessageId,
      existing.clientMessageId,
    ),
    runId: mergeDefined(incoming.runId, existing.runId),
    status,
    updatedAt:
      incoming.updatedAt >= existing.updatedAt
        ? incoming.updatedAt
        : existing.updatedAt,
  };
};

/**
 * Locate the existing delivery an incoming observation belongs to:
 * exact identity, then the persisted `runId` binding, then the exact
 * (sessionAgentId, sourceMessageId) pair of a non-terminal delivery.
 */
const findExistingDeliveryKey = (
  deliveries: Record<string, ChatDelivery>,
  incoming: ChatDelivery,
): string | null => {
  if (deliveries[incoming.deliveryId]) return incoming.deliveryId;
  if (incoming.runId) {
    for (const [key, delivery] of Object.entries(deliveries)) {
      if (delivery.runId === incoming.runId) return key;
    }
  }
  if (incoming.sourceMessageId) {
    for (const [key, delivery] of Object.entries(deliveries)) {
      if (
        delivery.sessionAgentId === incoming.sessionAgentId &&
        delivery.sourceMessageId === incoming.sourceMessageId &&
        !isTerminalChatDeliveryStatus(delivery.status)
      ) {
        return key;
      }
    }
  }
  return null;
};

const withStreamedRevision = (
  session: SessionDeliveryRuntime,
  revision: number | undefined,
  receivedAt: number,
): Pick<SessionDeliveryRuntime, 'lastEventAt' | 'revision' | 'needsResync'> => ({
  lastEventAt: Math.max(session.lastEventAt, receivedAt),
  revision:
    revision !== undefined
      ? Math.max(session.revision, revision)
      : session.revision,
  needsResync:
    session.needsResync ||
    (revision !== undefined &&
      session.revision >= 0 &&
      revision > session.revision + 1),
});

const applyDeliveryUpsert = (
  session: SessionDeliveryRuntime,
  action: Extract<ChatDeliveryRuntimeAction, { type: 'delivery_upsert' }>,
): SessionDeliveryRuntime => {
  const incoming = action.delivery;
  const existingKey = findExistingDeliveryKey(session.deliveries, incoming);
  const existing = existingKey ? session.deliveries[existingKey] : undefined;
  const merged = existing
    ? mergeChatDelivery(existing, incoming)
    : incoming;
  const key = existingKey ?? incoming.deliveryId;
  merged.deliveryId = key;
  if (
    existing &&
    existing.status === merged.status &&
    existing.runId === merged.runId &&
    existing.agentId === merged.agentId &&
    existing.agentName === merged.agentName &&
    existing.displayName === merged.displayName &&
    existing.avatar === merged.avatar &&
    existing.model === merged.model &&
    existing.updatedAt >= merged.updatedAt
  ) {
    // Duplicate observation; only clocks/revision move. Display fields must
    // be compared too: an `agent_run_started` whose timestamp ties or lags
    // the queue row still carries the agent name the queue snapshot lacks,
    // and dropping it would strand the card on the 'agent' fallback.
    return { ...session, ...withStreamedRevision(session, action.revision, action.receivedAt) };
  }
  const deliveries = { ...session.deliveries, [key]: merged };
  return {
    ...session,
    ...withStreamedRevision(session, action.revision, action.receivedAt),
    deliveries,
  };
};

const applyDeliveryTerminal = (
  session: SessionDeliveryRuntime,
  action: Extract<ChatDeliveryRuntimeAction, { type: 'delivery_terminal' }>,
): SessionDeliveryRuntime => {
  let targets: [string, ChatDelivery][];
  if (action.runId) {
    targets = Object.entries(session.deliveries).filter(
      ([, delivery]) =>
        delivery.runId === action.runId &&
        !isTerminalChatDeliveryStatus(delivery.status),
    );
  } else {
    // Without a run id the transition may only end the single active
    // delivery of this agent; queued deliveries are never touched, and an
    // ambiguous match (zero or several actives) requests a resync instead.
    targets = Object.entries(session.deliveries).filter(
      ([, delivery]) =>
        delivery.sessionAgentId === action.sessionAgentId &&
        delivery.status !== 'queued' &&
        !isTerminalChatDeliveryStatus(delivery.status),
    );
    if (targets.length !== 1) {
      return {
        ...session,
        ...withStreamedRevision(session, action.revision, action.receivedAt),
        needsResync: true,
        lastError: `ambiguous delivery_terminal for ${action.sessionAgentId}`,
      };
    }
  }
  if (targets.length === 0) {
    return {
      ...session,
      ...withStreamedRevision(session, action.revision, action.receivedAt),
    };
  }
  const deliveries = { ...session.deliveries };
  for (const [key, delivery] of targets) {
    deliveries[key] = {
      ...delivery,
      status: action.status,
      updatedAt: new Date(action.receivedAt).toISOString(),
    };
  }
  return {
    ...session,
    ...withStreamedRevision(session, action.revision, action.receivedAt),
    deliveries,
  };
};

const applyMemberQueueSnapshot = (
  session: SessionDeliveryRuntime,
  action: Extract<ChatDeliveryRuntimeAction, { type: 'member_queue_snapshot' }>,
): SessionDeliveryRuntime => {
  if (
    action.revision !== undefined &&
    session.revision >= 0 &&
    action.revision < session.revision
  ) {
    return session;
  }
  let deliveries = session.deliveries;
  const representedKeys = new Set<string>();
  const representedRunIds = new Set<string>();
  for (const incoming of action.deliveries) {
    const existingKey = findExistingDeliveryKey(deliveries, incoming);
    const existing = existingKey ? deliveries[existingKey] : undefined;
    const merged = existing
      ? mergeChatDelivery(existing, incoming)
      : incoming;
    const key = existingKey ?? incoming.deliveryId;
    merged.deliveryId = key;
    deliveries = { ...deliveries, [key]: merged };
    representedKeys.add(key);
    if (merged.runId) representedRunIds.add(merged.runId);
  }
  // Authoritative per-member replace: non-terminal deliveries of this member
  // that the snapshot no longer represents have ended.
  let changed = false;
  const next: Record<string, ChatDelivery> = {};
  for (const [key, delivery] of Object.entries(deliveries)) {
    if (
      delivery.sessionAgentId === action.sessionAgentId &&
      !isTerminalChatDeliveryStatus(delivery.status) &&
      !representedKeys.has(key) &&
      !(delivery.runId && representedRunIds.has(delivery.runId))
    ) {
      next[key] = {
        ...delivery,
        status: 'cancelled',
        updatedAt: new Date(action.receivedAt).toISOString(),
      };
      changed = true;
    } else {
      next[key] = delivery;
    }
  }
  const version = withStreamedRevision(
    session,
    action.revision,
    action.receivedAt,
  );
  return {
    ...session,
    ...version,
    lastError: version.needsResync ? session.lastError : undefined,
    deliveries: changed ? next : deliveries,
  };
};

const upsertMergeOnly = (
  deliveries: Record<string, ChatDelivery>,
  incoming: ChatDelivery,
): Record<string, ChatDelivery> => {
  const existingKey = findExistingDeliveryKey(deliveries, incoming);
  const existing = existingKey ? deliveries[existingKey] : undefined;
  const merged = existing ? mergeChatDelivery(existing, incoming) : incoming;
  if (isTerminalChatDeliveryStatus(merged.status) && !existing) {
    return deliveries;
  }
  merged.deliveryId = existingKey ?? incoming.deliveryId;
  return { ...deliveries, [merged.deliveryId]: merged };
};

const applySnapshotReceived = (
  session: SessionDeliveryRuntime,
  action: Extract<ChatDeliveryRuntimeAction, { type: 'snapshot_received' }>,
): SessionDeliveryRuntime => {
  // A strictly-older backend revision makes the whole snapshot stale.
  if (
    action.revision !== undefined &&
    session.revision >= 0 &&
    action.revision < session.revision
  ) {
    return session;
  }

  // Freshness decides the application mode for any accepted snapshot:
  // fresh (requested at/after the newest applied stream event) means the
  // read reflects every applied change — authoritative full replacement,
  // which is also what lets an equal-revision resync answer delete ghost
  // deliveries. Stale snapshots always merge without deleting or
  // downgrading.
  const isFresh = action.requestedAt >= session.lastEventAt;

  const base = {
    ...session,
    hydrated: true,
    revision:
      action.revision !== undefined
        ? Math.max(session.revision, action.revision)
        : session.revision,
  };

  if (!isFresh) {
    // A stale snapshot merges without deleting or downgrading, and it must
    // NOT clear needsResync: the flag may have been raised by a gap or
    // ambiguity that happened while this snapshot was in flight.
    let deliveries = session.deliveries;
    for (const delivery of action.deliveries) {
      deliveries = upsertMergeOnly(deliveries, delivery);
    }
    return { ...base, deliveries };
  }

  // Authoritative full replace; an accepted fresh snapshot settles every
  // pending resync need.
  const deliveries: Record<string, ChatDelivery> = {};
  for (const delivery of action.deliveries) {
    if (isTerminalChatDeliveryStatus(delivery.status)) continue;
    const key = findExistingDeliveryKey(deliveries, delivery);
    if (key) {
      deliveries[key] = mergeChatDelivery(deliveries[key], delivery);
    } else {
      deliveries[delivery.deliveryId] = delivery;
    }
  }
  return { ...base, needsResync: false, lastError: undefined, deliveries };
};

export const chatDeliveryRuntimeReducer = (
  state: ChatDeliveryRuntimeState,
  action: ChatDeliveryRuntimeAction,
): ChatDeliveryRuntimeState => {
  switch (action.type) {
    case 'delivery_upsert': {
      const sessionId = action.delivery.sessionId;
      const session = sessionRuntimeOf(state, sessionId);
      return withSession(
        state,
        sessionId,
        applyDeliveryUpsert(session, action),
      );
    }
    case 'delivery_terminal': {
      const session = sessionRuntimeOf(state, action.sessionId);
      return withSession(
        state,
        action.sessionId,
        applyDeliveryTerminal(session, action),
      );
    }
    case 'member_queue_snapshot': {
      const session = sessionRuntimeOf(state, action.sessionId);
      return withSession(
        state,
        action.sessionId,
        applyMemberQueueSnapshot(session, action),
      );
    }
    case 'snapshot_received': {
      const session = sessionRuntimeOf(state, action.sessionId);
      return withSession(
        state,
        action.sessionId,
        applySnapshotReceived(session, action),
      );
    }
    case 'snapshot_failed': {
      const session = sessionRuntimeOf(state, action.sessionId);
      // Failures must preserve the current projection; only record the error.
      return withSession(state, action.sessionId, {
        ...session,
        lastError: action.error ?? session.lastError,
      });
    }
    case 'replay_completed': {
      const session = sessionRuntimeOf(state, action.sessionId);
      if (action.revision < session.revision) return state;
      return withSession(state, action.sessionId, {
        ...session,
        revision: action.revision,
        lastEventAt: Math.max(session.lastEventAt, action.receivedAt),
        needsResync: false,
        lastError: undefined,
      });
    }
    case 'mark_needs_resync': {
      const session = sessionRuntimeOf(state, action.sessionId);
      return withSession(state, action.sessionId, {
        ...session,
        needsResync: true,
        lastError: action.reason ?? session.lastError,
      });
    }
  }
};

/** Sessions whose runtime flagged a revision gap or ambiguity. */
export const sessionsNeedingResync = (
  state: ChatDeliveryRuntimeState,
): string[] =>
  Object.entries(state.sessions)
    .filter(([, session]) => session.needsResync)
    .map(([sessionId]) => sessionId);

// ---------------------------------------------------------------------------
// Adapters: backend contract → deliveries.
// ---------------------------------------------------------------------------

const activeRunStatusToDeliveryStatus = (
  status: ChatActiveRunStatus,
): ChatDeliveryStatus => status;

export const deliveryFromAgentRunStarted = (
  event: Extract<ChatStreamEvent, { type: 'agent_run_started' }>,
): ChatDelivery => ({
  deliveryId: `${CANONICAL_QUEUE_ID_PREFIX}${event.delivery_id}`,
  sessionId: event.session_id,
  sessionAgentId: event.session_agent_id,
  agentId: event.agent_id,
  agentName: event.agent_name,
  displayName: event.agent_name.startsWith('@')
    ? event.agent_name
    : `@${event.agent_name}`,
  model: event.model,
  sourceMessageId: event.source_message_id,
  clientMessageId: event.client_message_id ?? undefined,
  runId: event.run_id,
  status: 'running',
  createdAt: event.started_at ?? new Date().toISOString(),
  updatedAt: event.started_at ?? new Date().toISOString(),
});

const QUEUED_MESSAGE_STATUS_MAP: Record<string, ChatDeliveryStatus> = {
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

/**
 * Exhaustive status mapping. Unknown values throw — callers must catch and
 * request a resync instead of guessing a terminal state.
 */
export const queuedMessageStatusToDeliveryStatus = (
  status: string,
): ChatDeliveryStatus => {
  const mapped = QUEUED_MESSAGE_STATUS_MAP[status];
  if (!mapped) {
    throw new Error(`unknown queued message status: ${status}`);
  }
  return mapped;
};

export const deliveriesFromMemberQueue = (
  queue: MemberQueueSnapshot,
): ChatDelivery[] =>
  queue.items.map((item) => ({
    deliveryId: `${CANONICAL_QUEUE_ID_PREFIX}${item.message.id}`,
    sessionId: queue.session_id,
    sessionAgentId: queue.session_agent_id,
    agentId: queue.agent_id,
    sourceMessageId: item.message.chat_message_id,
    runId: item.message.run_id ?? undefined,
    status: queuedMessageStatusToDeliveryStatus(String(item.message.status)),
    createdAt: item.message.created_at,
    updatedAt: item.message.updated_at,
  }));

export const deliveriesFromRuntimeSnapshot = (
  snapshot: ChatSessionRuntimeSnapshot,
): ChatDelivery[] => {
  const deliveries: ChatDelivery[] = [];
  // Queue deliveries first so display-rich active runs merge into the
  // canonical queue identity via the shared runId/delivery id binding.
  for (const queue of snapshot.queues) {
    deliveries.push(...deliveriesFromMemberQueue(queue));
  }
  for (const run of snapshot.active_runs) {
    const status = activeRunStatusToDeliveryStatus(run.status);
    deliveries.push({
      deliveryId: `${CANONICAL_QUEUE_ID_PREFIX}${run.delivery_id}`,
      sessionId: run.session_id,
      sessionAgentId: run.session_agent_id,
      agentId: run.agent_id,
      agentName: run.agent_name,
      displayName: run.display_name,
      avatar: run.avatar,
      model: run.model,
      sourceMessageId: run.source_message_id ?? undefined,
      clientMessageId: run.client_message_id ?? undefined,
      runId: run.run_id,
      status,
      createdAt: run.created_at,
      updatedAt: run.created_at,
    });
  }
  return deliveries;
};

// ---------------------------------------------------------------------------
// Selectors and the DeliveryCard conversation projection.
// ---------------------------------------------------------------------------

/** Non-terminal, non-queued deliveries of a session, oldest first. */
export const deliveryCardsForSession = (
  state: ChatDeliveryRuntimeState,
  sessionId: string,
): ChatDelivery[] => {
  const session = state.sessions[sessionId];
  if (!session) return [];
  return Object.values(session.deliveries)
    .filter(
      (delivery) =>
        delivery.status !== 'queued' &&
        !isTerminalChatDeliveryStatus(delivery.status),
    )
    .sort((a, b) => a.createdAt.localeCompare(b.createdAt));
};

/** Run ids whose activity streams should stay subscribed. */
export const activityRunIdsForSession = (
  state: ChatDeliveryRuntimeState,
  sessionId: string,
): string[] =>
  deliveryCardsForSession(state, sessionId)
    .filter(
      (delivery) =>
        delivery.status !== 'starting' && delivery.runId !== undefined,
    )
    .map((delivery) => delivery.runId as string);

export const hasInflightDeliveryForSession = (
  state: ChatDeliveryRuntimeState,
  sessionId: string,
): boolean => deliveryCardsForSession(state, sessionId).length > 0;

const monogramFromName = (name: string): string => {
  const monogram = name
    .trimStart()
    .replace(/^@/, '')
    .split('')
    .filter((ch) => /[a-z0-9]/i.test(ch))
    .slice(0, 2)
    .join('')
    .toUpperCase();
  return monogram || 'AG';
};

export const DELIVERY_CARD_MESSAGE_PREFIX = 'delivery-card-';

/**
 * Render a delivery as a conversation card. The pill/activity contract of the
 * existing UI is preserved: no visible `runId` while starting (label stays
 * "正在启动" and activity stays unsubscribed), real `runId` once running.
 */
export const deliveryCardToMessage = (delivery: ChatDelivery): Message => {
  const displayName =
    delivery.displayName ?? delivery.agentName ?? 'agent';
  const sender = displayName.startsWith('@')
    ? displayName
    : `@${displayName}`;
  return {
    id: `${DELIVERY_CARD_MESSAGE_PREFIX}${delivery.deliveryId}`,
    sessionId: delivery.sessionId,
    avatar: delivery.avatar || monogramFromName(displayName),
    sender,
    model: delivery.model ?? undefined,
    time: 'just now',
    createdAt: delivery.createdAt,
    text: '',
    isAgent: true,
    isThinking: true,
    isAgentRunning: true,
    runId: delivery.status === 'starting' ? undefined : delivery.runId,
    sessionAgentId: delivery.sessionAgentId,
    sourceMessageId: delivery.sourceMessageId,
    clientMessageId: delivery.clientMessageId,
  };
};

/**
 * Conversation projection: persisted messages plus delivery cards. A card is
 * hidden once a persisted message already carries its run id (the final reply
 * replaces the card); terminal deliveries never reach this point.
 */
export const mergePersistedWithDeliveryCards = (
  persisted: Message[],
  cards: Message[],
): Message[] => {
  const persistedRunIds = new Set(
    persisted
      .map((message) => message.runId)
      .filter((runId): runId is string => Boolean(runId)),
  );
  const visibleCards = cards.filter(
    (card) => !card.runId || !persistedRunIds.has(card.runId),
  );
  return orderMessagesForConversation([...persisted, ...visibleCards]);
};
