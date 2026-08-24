import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
} from 'react';
import {
  Theme,
  ThemePreference,
  Locale,
  Member,
  Session,
  Message,
  BackendChatAgent,
  BackendChatMessage,
  BackendChatSession,
  BackendChatSessionAgent,
  ChatSessionRuntimeSnapshot,
  QuotedMessageReference,
  Provider,
  Strategy,
  BackendChatSkill,
  Config,
  Environment,
  MemberQueuesBySessionAgentId,
  MemberQueueSnapshot,
  QueuedMessageStatus,
  UpdateChatSession,
  WorkflowCardProjection,
  WorkflowSessionStatusResponse,
  WorkflowSidebarState,
  WorkspaceChangesResponse,
  JsonValue,
} from '@/types';
import { i18nDict } from '@/i18n';
import { mockFrontendApi } from '@/lib/mockFrontendApi';
import type { WorkspaceBootstrapMock } from '@/mockApiData';
import {
  chatAgentsApi,
  chatMessagesApi,
  chatQueuesApi,
  chatRuntimeApi,
  chatSessionsApi,
  cliConfigApi,
  inboxApi,
  projectApi,
  sessionAgentsApi,
  skillsApi,
  systemApi,
  workflowApi,
} from '@/lib/api';
import {
  type CreateProjectRequest,
  type InboxItem,
  type InboxSummary,
  SoundFile,
  type NotificationConfig,
  type NotificationInboxSourcesConfig,
  type Project,
  type ProjectMemberWithRuntime,
} from '../../../../shared/types';
import {
  effectiveSessionAgentModelName,
  mapMessage,
  mapMessages,
  monogramFromName,
  mapProviders,
  mapSessionAgentsToMembers,
  mapSessions,
} from '@/lib/mappers';
import { resolveMessageReferences } from '@/lib/messageReferences';
import {
  AsyncResourceState,
  beginLoad,
  fail,
  initialAsync,
  succeed,
} from '@/lib/asyncResource';
import { notifyBuildStatsUsageUpdated } from '@/lib/buildStatsEvents';
import { notifySourceControlRefreshRequested } from '@/lib/sourceControlEvents';
import {
  hasRunningWorkflowActivity,
  idleWorkflowSessionStatus,
  isWorkflowSidebarRunning,
  resolveWorkflowSidebarState,
} from '@/lib/workflowSidebarState';

export const chatStreamWebSocketUrl = (path: string): string => {
  const base =
    typeof window === 'undefined' ? 'http://localhost' : window.location.href;
  const url = new URL(path, base);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  return url.toString();
};

export const OPTIMISTIC_USER_MESSAGE_PREFIX = 'msg-user-';
export const createClientMessageId = (): string => {
  const uuid = globalThis.crypto?.randomUUID?.();
  return `${OPTIMISTIC_USER_MESSAGE_PREFIX}${
    uuid ?? `${Date.now()}-${Math.random().toString(36).slice(2)}`
  }`;
};
// Persist the user's last-viewed project/session so a page refresh restores the
// same context (and therefore reconnects the WS stream to the same session)
// instead of always falling back to the first session in the list.
export const ACTIVE_SESSION_ID_STORAGE_KEY = 'openteams-active-session-id';
export const SELECTED_PROJECT_ID_STORAGE_KEY = 'openteams-selected-project-id';
export const RUNNING_AGENT_SESSION_IDS_STORAGE_KEY =
  'openteams-running-agent-session-ids';
export const UNREAD_AGENT_COMPLETION_SESSION_IDS_STORAGE_KEY =
  'openteams-unread-agent-completion-session-ids';
export const ACKED_WORKFLOW_INPUT_IDS_STORAGE_KEY =
  'openteams-acked-workflow-input-ids';
export const ACKED_WORKFLOW_ERROR_SESSION_IDS_STORAGE_KEY =
  'openteams-acked-workflow-error-session-ids';
// WebSocket auto-reconnect backoff bounds (ms).
export const CHAT_STREAM_RECONNECT_BASE_DELAY_MS = 1000;
export const CHAT_STREAM_RECONNECT_MAX_DELAY_MS = 30000;
export const SIDEBAR_RUNNING_INDICATOR_POLL_MS = 5000;
export const INBOX_REFRESH_INTERVAL_MS = 30000;
export const INBOX_LIGHT_REFRESH_DELAY_MS = 800;
export const INBOX_LIST_LIMIT = 25;
export const CHAT_MESSAGE_FONT_SIZE_DEFAULT = 14;
export const CHAT_MESSAGE_FONT_SIZE_OPTIONS = [13, 14, 15, 16] as const;

export const EMPTY_INBOX_SUMMARY: InboxSummary = {
  unread_count: 0n,
  unread_by_severity: [],
  unread_by_kind: [],
};

export type InboxNotificationSource = keyof NotificationInboxSourcesConfig;

export const DEFAULT_NOTIFICATION_INBOX_SOURCES: NotificationInboxSourcesConfig = {
  chat_message: true,
  workflow_action: true,
  approval: true,
  worktree: true,
  failure: true,
};

export const DEFAULT_NOTIFICATION_CONFIG: NotificationConfig = {
  sound_enabled: true,
  push_enabled: true,
  sound_file: SoundFile.ABSTRACT_SOUND3,
  inbox_sources: DEFAULT_NOTIFICATION_INBOX_SOURCES,
};

export const SOUND_FILE_VALUES = new Set<string>(Object.values(SoundFile));

export const notificationInboxSourcesFrom = (
  value: unknown,
): NotificationInboxSourcesConfig => {
  const candidate =
    value && typeof value === 'object' && !Array.isArray(value)
      ? (value as Partial<Record<InboxNotificationSource, unknown>>)
      : {};
  return {
    chat_message:
      typeof candidate.chat_message === 'boolean'
        ? candidate.chat_message
        : DEFAULT_NOTIFICATION_INBOX_SOURCES.chat_message,
    workflow_action:
      typeof candidate.workflow_action === 'boolean'
        ? candidate.workflow_action
        : DEFAULT_NOTIFICATION_INBOX_SOURCES.workflow_action,
    approval:
      typeof candidate.approval === 'boolean'
        ? candidate.approval
        : DEFAULT_NOTIFICATION_INBOX_SOURCES.approval,
    worktree:
      typeof candidate.worktree === 'boolean'
        ? candidate.worktree
        : DEFAULT_NOTIFICATION_INBOX_SOURCES.worktree,
    failure:
      typeof candidate.failure === 'boolean'
        ? candidate.failure
        : DEFAULT_NOTIFICATION_INBOX_SOURCES.failure,
  };
};

export const notificationConfigFromWorkspaceConfig = (
  config: Config | null,
): NotificationConfig => {
  const value = config?.notifications;
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    return DEFAULT_NOTIFICATION_CONFIG;
  }

  const candidate = value as {
    sound_enabled?: unknown;
    push_enabled?: unknown;
    sound_file?: unknown;
    inbox_sources?: unknown;
  };
  return {
    sound_enabled:
      typeof candidate.sound_enabled === 'boolean'
        ? candidate.sound_enabled
        : DEFAULT_NOTIFICATION_CONFIG.sound_enabled,
    push_enabled:
      typeof candidate.push_enabled === 'boolean'
        ? candidate.push_enabled
        : DEFAULT_NOTIFICATION_CONFIG.push_enabled,
    sound_file:
      typeof candidate.sound_file === 'string' &&
      SOUND_FILE_VALUES.has(candidate.sound_file)
        ? (candidate.sound_file as SoundFile)
        : DEFAULT_NOTIFICATION_CONFIG.sound_file,
    inbox_sources: notificationInboxSourcesFrom(candidate.inbox_sources),
  };
};

export const inboxNotificationSourceForKind = (
  kind: string,
): InboxNotificationSource | null => {
  if (kind === 'chat_message') return 'chat_message';
  if (
    kind === 'chat_agent_failed' ||
    kind === 'chat_mention_failed' ||
    kind === 'workflow_execution_failed'
  ) {
    return 'failure';
  }
  if (kind === 'executor_approval') return 'approval';
  if (kind.startsWith('worktree_')) return 'worktree';
  if (kind.startsWith('workflow_')) return 'workflow_action';
  return null;
};

export const inboxSourceEnabledForKind = (
  sources: NotificationInboxSourcesConfig,
  kind: string,
): boolean => {
  const source = inboxNotificationSourceForKind(kind);
  return source ? sources[source] : true;
};

export const filterInboxItemsForEnabledSources = (
  config: Config | null,
  items: InboxItem[],
): InboxItem[] => {
  if (!config) return items;
  const notificationConfig = notificationConfigFromWorkspaceConfig(config);
  return items.filter((item) =>
    inboxSourceEnabledForKind(notificationConfig.inbox_sources, item.kind),
  );
};

export const inboxCountValue = (value: bigint | number): bigint =>
  typeof value === 'bigint' ? value : BigInt(value);

export const decrementInboxSummaryEntries = (
  entries: InboxSummary['unread_by_kind'],
  decrements: Map<string, number>,
): InboxSummary['unread_by_kind'] =>
  entries
    .map((entry) => {
      const decrement = decrements.get(entry.key) ?? 0;
      if (decrement <= 0) return entry;
      const nextCount = inboxCountValue(entry.count) - BigInt(decrement);
      return {
        ...entry,
        count: nextCount > 0n ? nextCount : 0n,
      };
    })
    .filter((entry) => inboxCountValue(entry.count) > 0n);

export const countUnreadInboxItems = (items: InboxItem[]) => {
  const byKind = new Map<string, number>();
  const bySeverity = new Map<string, number>();
  let total = 0;
  for (const item of items) {
    if (item.read_at !== null || item.archived_at !== null) continue;
    total += 1;
    byKind.set(item.kind, (byKind.get(item.kind) ?? 0) + 1);
    const severity = String(item.severity);
    bySeverity.set(severity, (bySeverity.get(severity) ?? 0) + 1);
  }
  return { total, byKind, bySeverity };
};

export const filterInboxSummaryForEnabledSources = (
  config: Config | null,
  summary: InboxSummary,
): InboxSummary => {
  if (!config) return summary;
  const notificationConfig = notificationConfigFromWorkspaceConfig(config);
  const unread_by_kind = summary.unread_by_kind.filter((entry) =>
    inboxSourceEnabledForKind(notificationConfig.inbox_sources, entry.key),
  );
  const unread_count = unread_by_kind.reduce(
    (total, entry) => total + inboxCountValue(entry.count),
    0n,
  );
  return {
    ...summary,
    unread_count,
    unread_by_kind,
    unread_by_severity: [],
  };
};

export const inboxNotificationSettingsSignature = (config: Config | null): string => {
  const notificationConfig = notificationConfigFromWorkspaceConfig(config);
  return JSON.stringify({
    push_enabled: notificationConfig.push_enabled,
    sound_enabled: notificationConfig.sound_enabled,
    sound_file: notificationConfig.sound_file,
    inbox_sources: notificationConfig.inbox_sources,
  });
};

export const browserNotificationApi = (): typeof Notification | null => {
  if (typeof window === 'undefined' || !('Notification' in window)) {
    return null;
  }
  return window.Notification;
};

export const showInboxSystemNotification = (
  config: Config | null,
  newItems: InboxItem[],
) => {
  if (!config || newItems.length === 0) return;

  const notificationConfig = notificationConfigFromWorkspaceConfig(config);
  if (!notificationConfig.push_enabled) return;

  const BrowserNotification = browserNotificationApi();
  if (!BrowserNotification || BrowserNotification.permission !== 'granted') {
    return;
  }

  newItems.forEach((item) => {
    const notification = new BrowserNotification(
      item.title || 'OpenTeams notification',
      {
        body: item.body?.trim() || undefined,
        tag: `openteams-inbox-${item.id}`,
        silent: true,
      },
    );
    notification.onclick = () => {
      window.focus();
      notification.close();
    };
  });
};

export const playInboxNotificationSound = (
  config: Config | null,
  newItems: InboxItem[],
) => {
  if (!config) return;
  if (typeof Audio === 'undefined' || newItems.length === 0) return;

  const notificationConfig = notificationConfigFromWorkspaceConfig(config);
  if (!notificationConfig.sound_enabled) return;

  const audio = new Audio(
    `/api/sounds/${encodeURIComponent(notificationConfig.sound_file)}`,
  );
  void audio.play().catch(() => undefined);
};

export const isAutoReadableInboxItem = (item: InboxItem): boolean =>
  item.kind === 'chat_message' || item.source_type === 'chat_message';

export const isThemePreference = (
  value: string | null | undefined,
): value is ThemePreference =>
  value === 'light' || value === 'dark' || value === 'system';

export const resolveSystemTheme = (): Theme => {
  if (typeof window === 'undefined') return 'dark';
  return window.matchMedia?.('(prefers-color-scheme: light)').matches
    ? 'light'
    : 'dark';
};

export const readSessionIdSet = (storageKey: string): Set<string> => {
  if (typeof localStorage === 'undefined') return new Set();
  try {
    const raw = localStorage.getItem(storageKey);
    const parsed = raw ? JSON.parse(raw) : [];
    if (!Array.isArray(parsed)) return new Set();
    return new Set(
      parsed.filter(
        (value): value is string =>
          typeof value === 'string' && value.trim().length > 0,
      ),
    );
  } catch {
    return new Set();
  }
};

export const writeSessionIdSet = (storageKey: string, sessionIds: Set<string>) => {
  if (typeof localStorage === 'undefined') return;
  try {
    if (sessionIds.size === 0) {
      localStorage.removeItem(storageKey);
      return;
    }
    localStorage.setItem(storageKey, JSON.stringify([...sessionIds]));
  } catch {}
};

export const normalizeChatMessageFontSize = (value: number | string | null): number => {
  const numeric = typeof value === 'number' ? value : Number(value);
  if (!Number.isFinite(numeric)) return CHAT_MESSAGE_FONT_SIZE_DEFAULT;

  const rounded = Math.round(numeric);
  return (
    CHAT_MESSAGE_FONT_SIZE_OPTIONS.find((option) => option === rounded) ??
    CHAT_MESSAGE_FONT_SIZE_DEFAULT
  );
};

export const themePreferenceFromConfig = (theme: Config['theme']): ThemePreference => {
  const normalized = theme.toLowerCase();
  return isThemePreference(normalized) ? normalized : 'dark';
};

export const themePreferenceToConfig = (
  theme: ThemePreference,
): Config['theme'] => theme.toUpperCase() as Config['theme'];

export const resolveBrowserLocale = (): Locale => {
  if (typeof navigator === 'undefined') return 'zh';
  const language = navigator.language.toLowerCase();
  if (language.startsWith('en')) return 'en';
  if (language.startsWith('ja')) return 'ja';
  if (language.startsWith('ko')) return 'ko';
  if (language.startsWith('fr')) return 'fr';
  if (language.startsWith('es')) return 'es';
  return 'zh';
};

export const localeFromConfig = (language: Config['language']): Locale => {
  switch (language) {
    case 'EN':
      return 'en';
    case 'JA':
      return 'ja';
    case 'KO':
      return 'ko';
    case 'FR':
      return 'fr';
    case 'ES':
      return 'es';
    case 'ZH_HANS':
    case 'ZH_HANT':
      return 'zh';
    case 'BROWSER':
    default:
      return resolveBrowserLocale();
  }
};

export const localeToConfig = (locale: Locale): Config['language'] => {
  switch (locale) {
    case 'en':
      return 'EN';
    case 'ja':
      return 'JA';
    case 'ko':
      return 'KO';
    case 'fr':
      return 'FR';
    case 'es':
      return 'ES';
    case 'zh':
    default:
      return 'ZH_HANS';
  }
};

export const chatMessageFontSizeFromConfig = (
  value: Config['chat_bubble_font_size'],
): number => normalizeChatMessageFontSize(value.replace(/^px/, ''));

export const chatMessageFontSizeToConfig = (
  value: number,
): Config['chat_bubble_font_size'] =>
  `px${normalizeChatMessageFontSize(value)}` as Config['chat_bubble_font_size'];

export const isActiveAgentState = (state: string | undefined): boolean =>
  state === 'running' || state === 'stopping' || state === 'waitingapproval';

export const isRunningSessionAgentState = (state: string | undefined): boolean =>
  state === 'running' || state === 'stopping';

export const hasRunningSessionAgent = (
  sessionAgents: BackendChatSessionAgent[],
  ignoredSessionAgentIds?: ReadonlySet<string>,
): boolean =>
  sessionAgents.some((sessionAgent) =>
    !ignoredSessionAgentIds?.has(sessionAgent.id) &&
    isRunningSessionAgentState(sessionAgent.state),
  );

export type SessionRunningIndicators = {
  hasRunningAgent: boolean;
  hasRunningWorkflow: boolean;
  workflowSidebarState: WorkflowSidebarState;
  pendingWorkflowInputId: string | null;
  pendingWorkflowReviewId: string | null;
  hasPendingApproval: boolean;
};

export const loadSessionRunningIndicators = async (
  sessionIds: string[],
  ignoredSessionAgentIds?: ReadonlySet<string>,
  options?: { skipAgentSessionIds?: ReadonlySet<string> },
): Promise<Map<string, SessionRunningIndicators>> => {
  const entries = await Promise.all(
    sessionIds.map(async (sessionId) => {
      const shouldSkipAgents =
        options?.skipAgentSessionIds?.has(sessionId) ?? false;
      const [sessionAgents, workflowStatus] = await Promise.all([
        shouldSkipAgents
          ? Promise.resolve<BackendChatSessionAgent[]>([])
          : sessionAgentsApi.list(sessionId).catch(() => []),
        workflowApi
          .getSessionStatus(sessionId)
          .catch(() => idleWorkflowSessionStatus),
      ]);
      const workflowSidebarState = resolveWorkflowSidebarState(workflowStatus);
      return [
        sessionId,
        {
          hasRunningAgent: hasRunningSessionAgent(
            sessionAgents,
            ignoredSessionAgentIds,
          ),
          hasRunningWorkflow: isWorkflowSidebarRunning(workflowSidebarState),
          workflowSidebarState,
          pendingWorkflowInputId:
            workflowStatus.pending_workflow_input_id ?? null,
          pendingWorkflowReviewId:
            workflowStatus.pending_workflow_review_id ?? null,
          hasPendingApproval: sessionAgents.some(
            (sa) =>
              !ignoredSessionAgentIds?.has(sa.id) &&
              sa.state === 'waitingapproval',
          ),
        },
      ] as const;
    }),
  );

  return new Map(entries);
};

export const isOptimisticUserMessage = (message: Message): boolean =>
  Boolean(
    message.isUser &&
    message.id.startsWith(OPTIMISTIC_USER_MESSAGE_PREFIX),
  );

export const userMessageClientId = (message: Message): string | undefined =>
  message.clientMessageId ??
  (isOptimisticUserMessage(message) ? message.id : undefined);

export const messageIdentityKeys = (message: Message): string[] => {
  const keys = new Set<string>();
  if (message.id) keys.add(message.id);
  const clientMessageId = userMessageClientId(message);
  if (clientMessageId) keys.add(clientMessageId);
  return [...keys];
};

export const firstMessageSourceKey = (
  message: Message,
  sourceKeys: Set<string>,
): string | null => {
  if (message.isUser) return null;
  if (message.sourceMessageId && sourceKeys.has(message.sourceMessageId)) {
    return message.sourceMessageId;
  }
  if (message.clientMessageId && sourceKeys.has(message.clientMessageId)) {
    return message.clientMessageId;
  }
  return null;
};

export const orderMessagesForConversation = (messages: Message[]): Message[] => {
  const sourceKeys = new Set<string>();
  for (const message of messages) {
    if (!message.isUser) continue;
    for (const key of messageIdentityKeys(message)) {
      sourceKeys.add(key);
    }
  }

  if (sourceKeys.size === 0) return messages;

  const anchoredMessages = new Set<Message>();
  const anchoredBySourceKey = new Map<string, Message[]>();
  for (const message of messages) {
    const sourceKey = firstMessageSourceKey(message, sourceKeys);
    if (!sourceKey) continue;
    anchoredMessages.add(message);
    const anchored = anchoredBySourceKey.get(sourceKey) ?? [];
    anchored.push(message);
    anchoredBySourceKey.set(sourceKey, anchored);
  }

  if (anchoredMessages.size === 0) return messages;

  const emittedAnchored = new Set<Message>();
  const ordered: Message[] = [];
  for (const message of messages) {
    if (anchoredMessages.has(message)) continue;

    ordered.push(message);
    if (!message.isUser) continue;

    for (const key of messageIdentityKeys(message)) {
      const anchored = anchoredBySourceKey.get(key);
      if (!anchored) continue;
      for (const anchoredMessage of anchored) {
        if (emittedAnchored.has(anchoredMessage)) continue;
        ordered.push(anchoredMessage);
        emittedAnchored.add(anchoredMessage);
      }
    }
  }

  for (const message of messages) {
    if (anchoredMessages.has(message) && !emittedAnchored.has(message)) {
      ordered.push(message);
    }
  }

  return ordered;
};

export const messageCreatedAtMs = (message: Message): number | null => {
  if (!message.createdAt) return null;
  const value = Date.parse(message.createdAt);
  return Number.isNaN(value) ? null : value;
};

export const insertMessageByCreatedAt = (
  messages: Message[],
  message: Message,
): Message[] => {
  const messageAt = messageCreatedAtMs(message);
  if (messageAt === null) return [...messages, message];

  const next = [...messages];
  const index = next.findIndex((candidate) => {
    const candidateAt = messageCreatedAtMs(candidate);
    return candidateAt !== null && candidateAt > messageAt;
  });
  next.splice(index >= 0 ? index : next.length, 0, message);
  return next;
};

export const matchesUserMessageIdentity = (
  message: Message,
  messageId: string,
  clientMessageId?: string,
): boolean =>
  Boolean(
    message.isUser &&
      (message.id === messageId ||
        (clientMessageId && userMessageClientId(message) === clientMessageId)),
  );

export const queuedChatMessageKeysForSession = (
  queues: ReadonlyArray<MemberQueueSnapshot>,
  sessionId: string,
): Set<string> => {
  const keys = new Set<string>();
  for (const queue of queues) {
    if (queue.session_id !== sessionId) continue;
    for (const item of queue.items) {
      if (String(item.message.status) !== 'queued') continue;
      keys.add(item.message.chat_message_id);
    }
  }
  return keys;
};

export const queuedSessionAgentIdsByMessageKey = (
  queues: ReadonlyArray<MemberQueueSnapshot>,
  sessionId: string,
): Map<string, Set<string>> => {
  const targets = new Map<string, Set<string>>();
  for (const queue of queues) {
    if (queue.session_id !== sessionId) continue;
    for (const item of queue.items) {
      if (String(item.message.status) !== 'queued') continue;
      const key = item.message.chat_message_id;
      const sessionAgentIds = targets.get(key) ?? new Set<string>();
      sessionAgentIds.add(queue.session_agent_id);
      targets.set(key, sessionAgentIds);
    }
  }
  return targets;
};

export const isQueuedUserMessageFromSnapshot = (
  message: Message,
  queuedMessageKeys: ReadonlySet<string>,
): boolean => {
  if (!message.isUser || queuedMessageKeys.size === 0) return false;
  const clientMessageId = userMessageClientId(message);
  return (
    queuedMessageKeys.has(message.id) ||
    Boolean(clientMessageId && queuedMessageKeys.has(clientMessageId))
  );
};

export const filterQueuedUserMessagesFromSnapshot = (
  messages: Message[],
  queues: ReadonlyArray<MemberQueueSnapshot>,
  sessionId: string,
): Message[] => {
  const queuedMessageKeys = queuedChatMessageKeysForSession(queues, sessionId);
  if (queuedMessageKeys.size === 0) return messages;
  const queuedTargetsByMessageKey = queuedSessionAgentIdsByMessageKey(
    queues,
    sessionId,
  );
  return messages.filter(
    (message) => {
      if (!isQueuedUserMessageFromSnapshot(message, queuedMessageKeys)) {
        return true;
      }

      const clientMessageId = userMessageClientId(message);
      const messageKey = queuedMessageKeys.has(message.id)
        ? message.id
        : clientMessageId;
      const queuedTargets = messageKey
        ? queuedTargetsByMessageKey.get(messageKey)
        : undefined;
      return messages.some(
        (candidate) =>
          candidate.isAgentRunning &&
          candidate.clientMessageId === clientMessageId &&
          (!candidate.sessionAgentId ||
            !queuedTargets?.has(candidate.sessionAgentId)),
      );
    },
  );
};

export const queuedUserMessagesByIdFromSnapshot = (
  messages: Message[],
  queues: ReadonlyArray<MemberQueueSnapshot>,
  sessionId: string,
): Record<string, Message> => {
  const queuedMessageKeys = queuedChatMessageKeysForSession(queues, sessionId);
  if (queuedMessageKeys.size === 0) return {};

  const result: Record<string, Message> = {};
  for (const message of messages) {
    if (!isQueuedUserMessageFromSnapshot(message, queuedMessageKeys)) continue;
    result[message.id] = message;
    const clientMessageId = userMessageClientId(message);
    if (clientMessageId) {
      result[clientMessageId] = message;
    }
  }
  return result;
};

export const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

export const hasNonNegativeNumberField = (
  value: Record<string, unknown>,
  fieldNames: string[],
): boolean =>
  fieldNames.some((fieldName) => {
    const raw = value[fieldName];
    return typeof raw === 'number' && Number.isFinite(raw) && raw >= 0;
  });

export const hasCompleteTokenUsageBreakdown = (value: unknown): boolean => {
  if (!isRecord(value)) return false;
  return (
    hasNonNegativeNumberField(value, ['input_tokens', 'snapshot_input_tokens']) &&
    hasNonNegativeNumberField(value, [
      'output_tokens',
      'snapshot_output_tokens',
    ])
  );
};

export const hasRealCompleteTokenUsage = (message: BackendChatMessage): boolean => {
  if (message.sender_type !== 'agent' || !isRecord(message.meta)) return false;
  const tokenUsage = message.meta.token_usage;
  if (!isRecord(tokenUsage)) return false;
  if (tokenUsage.is_estimated === true) return false;
  return (
    hasCompleteTokenUsageBreakdown(tokenUsage) ||
    hasCompleteTokenUsageBreakdown(tokenUsage.last_token_usage) ||
    hasCompleteTokenUsageBreakdown(tokenUsage.total_token_usage)
  );
};

export const firstNumberField = (
  value: Record<string, unknown>,
  fieldNames: string[],
): number | null => {
  for (const fieldName of fieldNames) {
    const raw = value[fieldName];
    if (typeof raw === 'number' && Number.isFinite(raw)) return raw;
  }
  return null;
};

export const tokenUsageBreakdownSignature = (value: unknown) => {
  if (!isRecord(value)) return null;
  return {
    input: firstNumberField(value, ['input_tokens', 'snapshot_input_tokens']),
    output: firstNumberField(value, [
      'output_tokens',
      'snapshot_output_tokens',
    ]),
    cacheRead: firstNumberField(value, [
      'cache_read_tokens',
      'snapshot_cache_read_tokens',
    ]),
    reasoningOutput: firstNumberField(value, [
      'reasoning_output_tokens',
      'snapshot_reasoning_output_tokens',
    ]),
    total: firstNumberField(value, ['total_tokens', 'snapshot_total_tokens']),
  };
};

export const tokenUsageNotificationSignature = (
  message: BackendChatMessage,
): string | null => {
  if (!hasRealCompleteTokenUsage(message) || !isRecord(message.meta)) {
    return null;
  }
  const tokenUsage = message.meta.token_usage;
  if (!isRecord(tokenUsage)) return null;

  return JSON.stringify({
    direct: tokenUsageBreakdownSignature(tokenUsage),
    last: tokenUsageBreakdownSignature(tokenUsage.last_token_usage),
    total: tokenUsageBreakdownSignature(tokenUsage.total_token_usage),
  });
};

export const extractAgentMentions = (text: string): string[] =>
  Array.from(text.matchAll(/@([\p{L}\p{N}_-]+)/gu), (match) =>
    match[1],
  );

export const asAgentHandle = (name: string): string =>
  name.startsWith('@') ? name : `@${name}`;

export const memberNotFoundToastMessage = (
  locale: Locale,
  memberName: string,
): string => {
  const key = 'toast.memberNotFound';
  const template = i18nDict[locale]?.[key] ?? i18nDict.en[key] ?? key;
  return template.replace('{member}', asAgentHandle(memberName));
};

export const resolveProjectMainAgentMember = (
  projectMembers: ProjectMemberWithRuntime[],
): ProjectMemberWithRuntime | null =>
  projectMembers.find(
    (member) => member.member_type === 'agent' && member.role === 'lead',
  ) ??
  projectMembers.find((member) => member.member_type === 'agent') ??
  null;

export const resolveProjectMainAgentId = (
  projectMembers: ProjectMemberWithRuntime[],
): string | null =>
  resolveProjectMainAgentMember(projectMembers)?.agent_id ?? null;

export const resolveProjectMainAgentName = (
  projectMembers: ProjectMemberWithRuntime[],
  agents: BackendChatAgent[],
): string | null => {
  const mainMember = resolveProjectMainAgentMember(projectMembers);
  if (!mainMember) return null;

  const agent = mainMember.agent_id
    ? agents.find((candidate) => candidate.id === mainMember.agent_id)
    : undefined;
  const displayName = mainMember.member_name?.trim() || agent?.name?.trim();
  return displayName ? asAgentHandle(displayName) : null;
};

export const withSessionId = (sessionId: string, message: Message): Message =>
  message.sessionId === sessionId ? message : { ...message, sessionId };

export const withSessionIdsBySession = (
  messagesBySession: Record<string, Message[]>,
): Record<string, Message[]> =>
  Object.fromEntries(
    Object.entries(messagesBySession).map(([sessionId, messages]) => [
      sessionId,
      messages.map((message) => withSessionId(sessionId, message)),
    ]),
  );

export const filterMessagesForSession = (
  sessionId: string,
  messages: Message[],
): Message[] => {
  const scoped = messages.filter((message) => message.sessionId === sessionId);
  const userIndexByClientId = new Map<string, number>();
  const deduped: Message[] = [];

  for (const message of scoped) {
    if (message.isUser) {
      const clientMessageId = userMessageClientId(message);
      if (clientMessageId) {
        const existingIndex = userIndexByClientId.get(clientMessageId);
        if (existingIndex !== undefined) {
          const existing = deduped[existingIndex];
          deduped[existingIndex] =
            isOptimisticUserMessage(existing) &&
            !isOptimisticUserMessage(message)
              ? message
              : existing;
          continue;
        }
        userIndexByClientId.set(clientMessageId, deduped.length);
      }
    }
    deduped.push(message);
  }

  return orderMessagesForConversation(deduped);
};
