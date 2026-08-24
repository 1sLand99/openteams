import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useReducer,
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
  ChatActiveRun,
  ChatRuntimeDelta,
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
import { createConfigPatchQueue } from '../configPatchQueue';
import { useRunActivityStore } from '../RunActivityContext';

import {
  DEFAULT_CHAT_INPUT_MODE,
  chatSessionUpdatePayload,
  resolveChatInputMode,
  toSessionChatInputMode,
  type ListUpdater,
  type WorkflowRuntimeLine,
  ChatInputMode,
  ToastTone,
  WorkspaceToast,
} from './workspaceContextTypes';
import {
  ACTIVE_SESSION_ID_STORAGE_KEY,
  SELECTED_PROJECT_ID_STORAGE_KEY,
  RUNNING_AGENT_SESSION_IDS_STORAGE_KEY,
  UNREAD_AGENT_COMPLETION_SESSION_IDS_STORAGE_KEY,
  ACKED_WORKFLOW_INPUT_IDS_STORAGE_KEY,
  ACKED_WORKFLOW_ERROR_SESSION_IDS_STORAGE_KEY,
  CHAT_MESSAGE_FONT_SIZE_DEFAULT,
  EMPTY_INBOX_SUMMARY,
  resolveSystemTheme,
  readSessionIdSet,
  writeSessionIdSet,
  normalizeChatMessageFontSize,
  themePreferenceToConfig,
  resolveBrowserLocale,
  localeToConfig,
  chatMessageFontSizeToConfig,
  isOptimisticUserMessage,
  userMessageClientId,
  filterQueuedUserMessagesFromSnapshot,
  filterMessagesForSession,
} from './workspaceContextUtils';
import {
  EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
  chatDeliveryRuntimeReducer,
  deliveriesFromRuntimeSnapshot,
  deliveriesFromMemberQueue,
  deliveryCardToMessage,
  deliveryCardsForSession,
  hasInflightDeliveryForSession,
  mergePersistedWithDeliveryCards,
  sessionsNeedingResync,
  type ChatDeliveryRuntimeAction,
  type ChatDeliveryRuntimeState,
} from './chatDeliveryRuntime';
import { ChatDeliveryResyncScheduler } from './chatDeliveryResyncScheduler';

export const useWorkspaceState = () => {
  const runActivityStore = useRunActivityStore();
  const [themePreference, setThemePreferenceState] =
    useState<ThemePreference>('system');
  const [systemTheme, setSystemTheme] = useState<Theme>(resolveSystemTheme);
  const theme: Theme =
    themePreference === 'system' ? systemTheme : themePreference;

  const [locale, setLocaleState] = useState<Locale>(resolveBrowserLocale);
  const [chatMessageFontSize, setChatMessageFontSizeState] =
    useState<number>(CHAT_MESSAGE_FONT_SIZE_DEFAULT);
  const [activeSessionId, setActiveSessionId] = useState<string>(() => {
    try {
      return localStorage.getItem(ACTIVE_SESSION_ID_STORAGE_KEY) ?? '';
    } catch {
      return '';
    }
  });
  const mockBootstrapRef = useRef<WorkspaceBootstrapMock | null>(null);
  const toastDurationMsRef = useRef(3000);

  // Async-backed primary resources. Each is seeded with the existing mock so
  // the UI renders before the first API response arrives (or if the backend
  // is unreachable / has a contract gap).
  const [sessionsAsync, setSessionsAsync] = useState<
    AsyncResourceState<Session[]>
  >(() => initialAsync([]));
  const [archivedSessionsAsync, setArchivedSessionsAsync] = useState<
    AsyncResourceState<Session[]>
  >(() => initialAsync([]));
  const [projectsAsync, setProjectsAsync] = useState<
    AsyncResourceState<Project[]>
  >(() => initialAsync([]));
  const [selectedProjectId, setSelectedProjectIdState] = useState<string>(() => {
    try {
      return localStorage.getItem(SELECTED_PROJECT_ID_STORAGE_KEY) ?? '';
    } catch {
      return '';
    }
  });
  const [allMessages, setAllMessages] = useState<Record<string, Message[]>>({});
  const allMessagesRef = useRef<Record<string, Message[]>>({});
  const [memberQueuesBySessionAgentId, setMemberQueuesBySessionAgentId] =
    useState<MemberQueuesBySessionAgentId>({});
  // The delivery runtime is the single writer for chat run state. Snapshots,
  // member-queue snapshots and stream events all flow through its reducer,
  // which guarantees monotonic status transitions and that stale snapshots
  // can never wipe newer streamed runs.
  const [chatDeliveryRuntime, dispatchChatDelivery] = useReducer(
    chatDeliveryRuntimeReducer,
    EMPTY_CHAT_DELIVERY_RUNTIME_STATE,
  );
  const chatDeliveryRuntimeRef = useRef(chatDeliveryRuntime);
  useEffect(() => {
    chatDeliveryRuntimeRef.current = chatDeliveryRuntime;
  }, [chatDeliveryRuntime]);
  // The single dispatch entry point: computes the next state, updates the ref
  // synchronously (so same-tick events never read a stale base) and returns
  // the next state for callers that need an immediate preview.
  const dispatchChatDeliverySync = useCallback(
    (action: ChatDeliveryRuntimeAction): ChatDeliveryRuntimeState => {
      const next = chatDeliveryRuntimeReducer(
        chatDeliveryRuntimeRef.current,
        action,
      );
      chatDeliveryRuntimeRef.current = next;
      dispatchChatDelivery(action);
      return next;
    },
    [],
  );
  const [workflowRuntimeLinesByExecution, setWorkflowRuntimeLinesByExecution] =
    useState<Record<string, WorkflowRuntimeLine[]>>({});
  const [messagesAsync, setMessagesAsync] = useState<
    AsyncResourceState<Message[]>
  >(() => initialAsync([]));
  const [membersAsync, setMembersAsync] = useState<
    AsyncResourceState<Member[]>
  >(() => initialAsync([]));
  const [mainAgentName, setMainAgentName] = useState<string | null>(null);
  const [providersAsync, setProvidersAsync] = useState<
    AsyncResourceState<Provider[]>
  >(() => initialAsync([]));
  const [skillsAsync, setSkillsAsync] = useState<
    AsyncResourceState<BackendChatSkill[]>
  >(() => initialAsync([]));
  const [configAsync, setConfigAsync] = useState<
    AsyncResourceState<Config | null>
  >(() => initialAsync(null));
  const [environment, setEnvironment] = useState<Environment | null>(null);
  const latestConfigRef = useRef<Config | null>(null);
  const configPatchQueueRef = useRef<ReturnType<
    typeof createConfigPatchQueue<Config>
  > | null>(null);
  const publishVisibleConfig = useCallback((visible: Config) => {
    latestConfigRef.current = visible;
    setConfigAsync(succeed(visible));
  }, []);
  const ensureConfigPatchQueue = useCallback(
    (initial: Config) => {
      if (!configPatchQueueRef.current) {
        configPatchQueueRef.current = createConfigPatchQueue<Config>(
          initial,
          systemApi.saveConfig,
          publishVisibleConfig,
        );
      } else {
        configPatchQueueRef.current.replaceAcknowledged(initial);
      }
      return configPatchQueueRef.current;
    },
    [publishVisibleConfig],
  );
  const saveConfigPatch = useCallback((patch: Partial<Config>) => {
    const queue = configPatchQueueRef.current;
    if (!queue) return Promise.reject(new Error('Config is not loaded'));
    return queue.enqueue(patch, { optimistic: false });
  }, []);
  const [inboxSummaryAsync, setInboxSummaryAsync] = useState<
    AsyncResourceState<InboxSummary>
  >(() => initialAsync(EMPTY_INBOX_SUMMARY));
  const [inboxItemsAsync, setInboxItemsAsync] = useState<
    AsyncResourceState<InboxItem[]>
  >(() => initialAsync([]));
  const [pendingApprovalSessionIds, setPendingApprovalSessionIds] =
    useState<ReadonlySet<string>>(() => new Set());
  const pendingApprovalSessionIdsRef = useRef<ReadonlySet<string>>(
    new Set(),
  );
  const [waitingApprovalSessionIds, setWaitingApprovalSessionIds] =
    useState<ReadonlySet<string>>(() => new Set());
  const waitingApprovalSessionIdsRef = useRef<ReadonlySet<string>>(
    new Set(),
  );
  const [workflowCardAsync, setWorkflowCardAsync] = useState<
    AsyncResourceState<WorkflowCardProjection | null>
  >(() => initialAsync(null));
  const [workspaceChangesAsync, setWorkspaceChangesAsync] = useState<
    AsyncResourceState<WorkspaceChangesResponse | null>
  >(() => initialAsync(null));
  const messagesRequestIdRef = useRef(0);
  const queueRequestIdRef = useRef(0);
  const inboxRequestIdRef = useRef(0);
  const inboxLightRefreshTimerRef = useRef<ReturnType<typeof setTimeout> | null>(
    null,
  );
  const inboxSoundProjectIdRef = useRef<string | null>(null);
  const inboxSoundSettingsSignatureRef = useRef<string | null>(null);
  const inboxSoundPrimedRef = useRef(false);
  const inboxUnreadSoundIdsRef = useRef<Set<string>>(new Set());
  const inboxAutoReadProjectIdRef = useRef<string | null>(null);
  const inboxInitialUnreadItemIdsRef = useRef<Set<string>>(new Set());
  const autoMarkedInboxItemIdsRef = useRef<Set<string>>(new Set());
  const workspaceChangesRequestIdRef = useRef(0);
  const initialRefreshStartedRef = useRef(false);
  const initialRefreshCompletedRef = useRef(false);
  const sessionRunningIndicatorRequestsRef = useRef<Map<string, Promise<void>>>(
    new Map(),
  );
  const sessionWorkflowStatusRequestsRef = useRef<
    Map<string, Promise<WorkflowSessionStatusResponse | null>>
  >(new Map());
  const [chatInputModeBySessionId, setChatInputModeBySessionId] = useState<
    Record<string, ChatInputMode>
  >({});

  const [strategies, setStrategies] = useState<Strategy[]>([]);
  const [mockAgentRepliesByMention, setMockAgentRepliesByMention] = useState<
    Record<string, string[]>
  >({ default: ['Working on it.'] });
  const [selectedStrategyId, setSelectedStrategyId] = useState<string>('');
  const [selectedOnboardType, setSelectedOnboardType] = useState<
    'saas' | 'cli' | 'game' | 'ai'
  >('saas');

  // Global Settings Switches
  const [smartRouting, setSmartRouting] = useState<boolean>(true);
  const [showCost, setShowCost] = useState<boolean>(true);
  const [showExplanation, setShowExplanation] = useState<boolean>(true);
  const [warnOverDollar, setWarnOverDollar] = useState<boolean>(false);

  // Stats (LOCAL / MOCK-FALLBACK per backend_contract_audit 搂5.1)
  const [weeklyCost, setWeeklyCost] = useState<number>(0);
  const [weeklySaved, setWeeklySaved] = useState<number>(0);
  const [earlyBirdLeft, setEarlyBirdLeft] = useState<number>(0);

  // Settings view controller
  const [activeSettingsTab, setActiveSettingsTab] =
    useState<string>('providers');

  // Modal Switches
  const [isAddMemberModalOpen, setIsAddMemberModalOpen] =
    useState<boolean>(false);
  const [isAddProviderModalOpen, setIsAddProviderModalOpen] =
    useState<boolean>(false);

  // Toast
  const [toast, setToast] = useState<WorkspaceToast | null>(null);

  // Cache the latest activeSessionId so async callbacks see the live value.
  const activeSessionIdRef = useRef(activeSessionId);
  const selectedProjectIdRef = useRef(selectedProjectId);
  // Cache the active session's workspace path so the WebSocket
  // `file_change_refresh` handler can refresh workspace changes without a stale
  // closure (the socket effect does not re-subscribe on every sessions update).
  const activeWorkspacePathRef = useRef<string | null>(null);
  const sessionLeadAgentIdBySessionIdRef = useRef<Record<
    string,
    string | null
  >>({});
  const workflowRouteAgentIdRef = useRef<string | null>(null);
  const agentNamesByIdRef = useRef<Record<string, string>>({});
  const agentModelsByIdRef = useRef<Record<string, string | null>>({});
  const notifiedTokenUsageSignaturesRef = useRef<Record<string, string>>({});
  // Session agents the user has just requested to stop. While an agent is in
  // this set, keep any existing visible placeholder until the persisted stop
  // notice replaces it, but do not re-hydrate a separate running placeholder.
  // Cleared when a new run starts or after the terminal stop notice replaces
  // the placeholder.
  const optimisticallyStoppedSessionAgentIdsRef = useRef<Set<string>>(
    new Set(),
  );
  const runningAgentSessionIdsRef = useRef<Set<string>>(
    readSessionIdSet(RUNNING_AGENT_SESSION_IDS_STORAGE_KEY),
  );
  const unreadAgentCompletionSessionIdsRef = useRef<Set<string>>(
    readSessionIdSet(UNREAD_AGENT_COMPLETION_SESSION_IDS_STORAGE_KEY),
  );
  const acknowledgedWorkflowInputIdsRef = useRef<Set<string>>(
    readSessionIdSet(ACKED_WORKFLOW_INPUT_IDS_STORAGE_KEY),
  );
  const acknowledgedWorkflowErrorSessionIdsRef = useRef<Set<string>>(
    readSessionIdSet(ACKED_WORKFLOW_ERROR_SESSION_IDS_STORAGE_KEY),
  );
  useEffect(() => {
    allMessagesRef.current = allMessages;
  }, [allMessages]);

  const persistAgentSessionActivityStorage = useCallback(() => {
    writeSessionIdSet(
      RUNNING_AGENT_SESSION_IDS_STORAGE_KEY,
      runningAgentSessionIdsRef.current,
    );
    writeSessionIdSet(
      UNREAD_AGENT_COMPLETION_SESSION_IDS_STORAGE_KEY,
      unreadAgentCompletionSessionIdsRef.current,
    );
  }, []);
  const persistWorkflowInputAcknowledgementStorage = useCallback(() => {
    writeSessionIdSet(
      ACKED_WORKFLOW_INPUT_IDS_STORAGE_KEY,
      acknowledgedWorkflowInputIdsRef.current,
    );
  }, []);
  const persistWorkflowErrorAcknowledgementStorage = useCallback(() => {
    writeSessionIdSet(
      ACKED_WORKFLOW_ERROR_SESSION_IDS_STORAGE_KEY,
      acknowledgedWorkflowErrorSessionIdsRef.current,
    );
  }, []);

  const syncSessionAgentActivityIndicator = useCallback(
    (sessionId: string, hasRunningAgent: boolean): boolean => {
      if (!sessionId) return false;

      let changed = false;
      if (hasRunningAgent) {
        if (!runningAgentSessionIdsRef.current.has(sessionId)) {
          runningAgentSessionIdsRef.current.add(sessionId);
          changed = true;
        }
        if (unreadAgentCompletionSessionIdsRef.current.delete(sessionId)) {
          changed = true;
        }
      } else {
        const wasRunning = runningAgentSessionIdsRef.current.delete(sessionId);
        if (wasRunning) {
          changed = true;
        }
        if (activeSessionIdRef.current === sessionId) {
          if (unreadAgentCompletionSessionIdsRef.current.delete(sessionId)) {
            changed = true;
          }
        } else if (wasRunning) {
          unreadAgentCompletionSessionIdsRef.current.add(sessionId);
          changed = true;
        }
      }

      if (changed) {
        persistAgentSessionActivityStorage();
      }
      return unreadAgentCompletionSessionIdsRef.current.has(sessionId);
    },
    [persistAgentSessionActivityStorage],
  );

  const acknowledgeWorkflowInput = useCallback(
    (inputId: string | null | undefined) => {
      if (!inputId || acknowledgedWorkflowInputIdsRef.current.has(inputId)) {
        return;
      }
      acknowledgedWorkflowInputIdsRef.current.add(inputId);
      persistWorkflowInputAcknowledgementStorage();
    },
    [persistWorkflowInputAcknowledgementStorage],
  );

  const syncSessionWorkflowInputIndicator = useCallback(
    (
      sessionId: string,
      pendingWorkflowInputId: string | null | undefined,
    ): boolean => {
      if (!sessionId || !pendingWorkflowInputId) return false;
      if (activeSessionIdRef.current === sessionId) {
        acknowledgeWorkflowInput(pendingWorkflowInputId);
        return false;
      }
      return !acknowledgedWorkflowInputIdsRef.current.has(
        pendingWorkflowInputId,
      );
    },
    [acknowledgeWorkflowInput],
  );

  const acknowledgeWorkflowError = useCallback(
    (sessionId: string | null | undefined) => {
      if (!sessionId) return;
      if (acknowledgedWorkflowErrorSessionIdsRef.current.has(sessionId)) {
        return;
      }
      acknowledgedWorkflowErrorSessionIdsRef.current.add(sessionId);
      persistWorkflowErrorAcknowledgementStorage();
    },
    [persistWorkflowErrorAcknowledgementStorage],
  );

  const syncSessionWorkflowErrorIndicator = useCallback(
    (sessionId: string, workflowSidebarState: WorkflowSidebarState): boolean => {
      if (!sessionId) return false;
      if (workflowSidebarState !== 'failed') {
        if (acknowledgedWorkflowErrorSessionIdsRef.current.delete(sessionId)) {
          persistWorkflowErrorAcknowledgementStorage();
        }
        return false;
      }
      if (activeSessionIdRef.current === sessionId) {
        acknowledgeWorkflowError(sessionId);
        return false;
      }
      return !acknowledgedWorkflowErrorSessionIdsRef.current.has(sessionId);
    },
    [acknowledgeWorkflowError, persistWorkflowErrorAcknowledgementStorage],
  );

  const clearUnreadAgentCompletion = useCallback(
    (sessionId: string) => {
      if (!sessionId) return;
      if (!unreadAgentCompletionSessionIdsRef.current.delete(sessionId)) {
        return;
      }

      persistAgentSessionActivityStorage();
      setSessionsAsync((prev) => {
        let changed = false;
        const data = prev.data.map((session) => {
          if (
            session.id !== sessionId ||
            !session.hasUnreadAgentCompletion
          ) {
            return session;
          }
          changed = true;
          return { ...session, hasUnreadAgentCompletion: false };
        });
        return changed ? { ...prev, data } : prev;
      });
    },
    [persistAgentSessionActivityStorage],
  );

  const clearPendingWorkflowInput = useCallback(
    (sessionId: string) => {
      if (!sessionId) return;
      setSessionsAsync((prev) => {
        let changed = false;
        const data = prev.data.map((session) => {
          if (session.id !== sessionId) return session;
          acknowledgeWorkflowInput(session.pendingWorkflowInputId);
          if (!session.hasPendingWorkflowInput) return session;
          changed = true;
          return { ...session, hasPendingWorkflowInput: false };
        });
        return changed ? { ...prev, data } : prev;
      });
    },
    [acknowledgeWorkflowInput],
  );

  const clearWorkflowErrorAttention = useCallback(
    (sessionId: string) => {
      if (!sessionId) return;
      setSessionsAsync((prev) => {
        let changed = false;
        const data = prev.data.map((session) => {
          if (session.id !== sessionId) {
            return session;
          }
          if (session.workflowSidebarState === 'failed') {
            acknowledgeWorkflowError(sessionId);
          }
          if (!session.hasWorkflowError) return session;
          changed = true;
          return { ...session, hasWorkflowError: false };
        });
        return changed ? { ...prev, data } : prev;
      });
    },
    [acknowledgeWorkflowError],
  );

  useEffect(() => {
    activeSessionIdRef.current = activeSessionId;
    try {
      if (activeSessionId) {
        localStorage.setItem(ACTIVE_SESSION_ID_STORAGE_KEY, activeSessionId);
      } else {
        localStorage.removeItem(ACTIVE_SESSION_ID_STORAGE_KEY);
      }
    } catch {}
    clearUnreadAgentCompletion(activeSessionId);
    clearPendingWorkflowInput(activeSessionId);
    clearWorkflowErrorAttention(activeSessionId);
  }, [
    activeSessionId,
    clearPendingWorkflowInput,
    clearUnreadAgentCompletion,
    clearWorkflowErrorAttention,
  ]);

  useEffect(() => {
    messagesRequestIdRef.current += 1;
    const sessionQueues = activeSessionId
      ? Object.values(memberQueuesBySessionAgentId).filter(
          (queue) => queue.session_id === activeSessionId,
        )
      : [];
    const sessionMessages = activeSessionId
      ? filterMessagesForSession(
          activeSessionId,
          allMessagesRef.current[activeSessionId] ?? [],
        )
      : [];
    const sessionDeliveryCards = activeSessionId
      ? deliveryCardsForSession(chatDeliveryRuntime, activeSessionId).map(
          deliveryCardToMessage,
        )
      : [];
    const sessionSnapshot = activeSessionId
      ? mergePersistedWithDeliveryCards(
          sessionMessages.filter((message) => !message.isAgentRunning),
          sessionDeliveryCards,
        )
      : [];
    setMessagesAsync(
      succeed(
        activeSessionId
          ? filterQueuedUserMessagesFromSnapshot(
              sessionSnapshot,
              sessionQueues,
              activeSessionId,
            )
          : [],
      ),
    );
  }, [chatDeliveryRuntime, activeSessionId, memberQueuesBySessionAgentId]);

  // Keep the cached workspace path in sync with the active session so the
  // WebSocket `file_change_refresh` handler always refreshes the right path.
  // Mirrors FreeChatWorkspace's `reloadRelatedFiles` (currentProject workspace).
  useEffect(() => {
    activeWorkspacePathRef.current = selectedProjectId
      ? projectsAsync.data?.find(
          (project) => project.id === selectedProjectId,
        )?.default_workspace_path ?? null
      : null;
  }, [selectedProjectId, projectsAsync]);
  useEffect(() => {
    setWorkflowRuntimeLinesByExecution({});
  }, [activeSessionId]);
  useEffect(() => {
    selectedProjectIdRef.current = selectedProjectId;
  }, [selectedProjectId]);

  const chatInputMode =
    activeSessionId !== ''
      ? (chatInputModeBySessionId[activeSessionId] ??
        DEFAULT_CHAT_INPUT_MODE)
      : DEFAULT_CHAT_INPUT_MODE;

  const showToast = (msg: string, tone: ToastTone = 'info') => {
    setToast({ message: msg, tone });
    setTimeout(() => {
      setToast(null);
    }, toastDurationMsRef.current);
  };

  const persistUiPreference = (patch: Partial<Config>) => {
    const queue = configPatchQueueRef.current;
    if (!queue) {
      showToast('Settings are still loading. Please try again.', 'error');
      return;
    }
    void queue.enqueue(patch, { optimistic: true }).catch((error) => {
      console.error('Failed to save UI preferences', error);
      showToast('Failed to save settings to config.json.', 'error');
    });
  };

  const setTheme = (t: ThemePreference) => {
    setThemePreferenceState(t);
    persistUiPreference({ theme: themePreferenceToConfig(t) });
  };

  const setLocale = (l: Locale) => {
    setLocaleState(l);
    persistUiPreference({ language: localeToConfig(l) });
  };

  const setChatMessageFontSize = (size: number) => {
    const normalized = normalizeChatMessageFontSize(size);
    setChatMessageFontSizeState(normalized);
    persistUiPreference({
      chat_bubble_font_size: chatMessageFontSizeToConfig(normalized),
    });
  };

  useEffect(() => {
    if (typeof window === 'undefined') return;
    const mediaQuery = window.matchMedia?.('(prefers-color-scheme: light)');
    if (!mediaQuery) return;

    const updateSystemTheme = () => {
      setSystemTheme(mediaQuery.matches ? 'light' : 'dark');
    };

    updateSystemTheme();
    mediaQuery.addEventListener?.('change', updateSystemTheme);
    return () => {
      mediaQuery.removeEventListener?.('change', updateSystemTheme);
    };
  }, []);

  useEffect(() => {
    document.body.setAttribute('data-mode', theme);
  }, [theme]);

  const makeListSetter =
    <T,>(
      setAsync: React.Dispatch<React.SetStateAction<AsyncResourceState<T[]>>>,
    ) =>
    (next: ListUpdater<T>) => {
      setAsync((prev) => {
        const newData =
          typeof next === 'function'
            ? (next as (p: T[]) => T[])(prev.data)
            : next;
        return { ...prev, data: newData, empty: newData.length === 0 };
      });
    };

  const setSessions = useCallback(
    makeListSetter<Session>(setSessionsAsync),
    [],
  );
  const setMembers = useCallback(makeListSetter<Member>(setMembersAsync), []);
  const setProviders = useCallback(
    makeListSetter<Provider>(setProvidersAsync),
    [],
  );
  const setSessionRunningIndicator = useCallback(
    (sessionId: string, hasRunningAgent: boolean) => {
      if (!sessionId) return;
      const hasUnreadAgentCompletion = syncSessionAgentActivityIndicator(
        sessionId,
        hasRunningAgent,
      );
      setSessionsAsync((prev) => {
        let changed = false;
        const data = prev.data.map((session) => {
          if (
            session.id !== sessionId ||
            (session.hasRunningAgent === hasRunningAgent &&
              session.hasUnreadAgentCompletion === hasUnreadAgentCompletion)
          ) {
            return session;
          }
          changed = true;
          return {
            ...session,
            hasRunningAgent,
            hasUnreadAgentCompletion,
          };
        });
        return changed ? { ...prev, data } : prev;
      });
    },
    [syncSessionAgentActivityIndicator],
  );
  const applyChatRuntimeSnapshot = useCallback(
    (
      snapshot: ChatSessionRuntimeSnapshot,
      options?: { requestedAt?: number },
    ): ChatDeliveryRuntimeState => {
      const sid = snapshot.session_id;
      setMemberQueuesBySessionAgentId((prev) => {
        const next = { ...prev };
        for (const [sessionAgentId, queue] of Object.entries(next)) {
          if (queue.session_id === sid) {
            delete next[sessionAgentId];
          }
        }
        for (const queue of snapshot.queues) {
          next[queue.session_agent_id] = queue;
        }
        return next;
      });
      // Route the snapshot through the delivery reducer. Revision and
      // local-clock guards inside the reducer decide between authoritative
      // replacement and merge-only hydration, so stale snapshots can never
      // wipe runs reported by stream events.
      let snapshotDeliveries;
      try {
        snapshotDeliveries = deliveriesFromRuntimeSnapshot(snapshot);
      } catch (error) {
        // Unknown delivery status: request an authoritative resync instead
        // of guessing a terminal state.
        return dispatchChatDeliverySync({
          type: 'mark_needs_resync',
          sessionId: sid,
          reason: error instanceof Error ? error.message : String(error),
        });
      }
      const nextDeliveryRuntime = dispatchChatDeliverySync({
        type: 'snapshot_received',
        sessionId: sid,
        deliveries: snapshotDeliveries,
        revision: Number(snapshot.revision),
        requestedAt: options?.requestedAt ?? Date.now(),
        receivedAt: Date.now(),
      });
      runActivityStore.syncRuns(
        snapshot.active_runs
          .filter((run) => run.status !== 'starting')
          .map((run) => run.run_id),
      );
      setSessionRunningIndicator(
        sid,
        hasInflightDeliveryForSession(nextDeliveryRuntime, sid),
      );
      if (snapshot.messages) {
        const mapped = mapMessages(snapshot.messages as unknown as BackendChatMessage[], {
          agentNamesById: agentNamesByIdRef.current,
          agentModelsById: agentModelsByIdRef.current,
        });
        setAllMessages((prev) => {
          const current = filterMessagesForSession(sid, prev[sid] ?? []);
          const persistedClientIds = new Set(
            mapped
              .map(userMessageClientId)
              .filter((id): id is string => Boolean(id)),
          );
          const carried = current.filter((message) => {
            if (message.isAgentRunning || !isOptimisticUserMessage(message)) {
              return false;
            }
            const clientId = userMessageClientId(message);
            return clientId !== undefined && !persistedClientIds.has(clientId);
          });
          return {
            ...prev,
            [sid]: resolveMessageReferences([...mapped, ...carried]),
          };
        });
      }
      return nextDeliveryRuntime;
    },
    [
      dispatchChatDeliverySync,
      runActivityStore,
      setSessionRunningIndicator,
    ],
  );

  const applyChatRuntimeDelta = useCallback(
    (
      delta: ChatRuntimeDelta,
      receivedAt = Date.now(),
    ): ChatDeliveryRuntimeState => {
      const sessionId = delta.session_id;
      const revision = Number(delta.revision);
      const current = chatDeliveryRuntimeRef.current.sessions[sessionId];
      if (!Number.isSafeInteger(revision)) {
        return dispatchChatDeliverySync({
          type: 'mark_needs_resync',
          sessionId,
          reason: 'runtime revision exceeds JavaScript safe integer range',
        });
      }
      // A live delta cannot establish history from an unknown cursor, and a
      // gap must not advance the cursor. Replay therefore always starts from
      // the last contiguous revision rather than the highest observed one.
      if (!current || current.revision < 0) {
        return dispatchChatDeliverySync({
          type: 'mark_needs_resync',
          sessionId,
          reason: 'runtime delta received before initial hydration',
        });
      }
      if (revision <= current.revision) {
        return chatDeliveryRuntimeRef.current;
      }
      if (revision !== current.revision + 1) {
        return dispatchChatDeliverySync({
          type: 'mark_needs_resync',
          sessionId,
          reason: `runtime revision gap ${current.revision}->${revision}`,
        });
      }
      const queue = delta.payload.queue;
      if (!queue) {
        return dispatchChatDeliverySync({
          type: 'mark_needs_resync',
          sessionId,
          reason: `runtime delta ${revision} requires a full snapshot`,
        });
      }
      setMemberQueuesBySessionAgentId((prev) => ({
        ...prev,
        [queue.session_agent_id]: queue,
      }));
      let deliveries;
      try {
        deliveries = deliveriesFromMemberQueue(queue).map((delivery) => {
          const name = delivery.agentId
            ? agentNamesByIdRef.current[delivery.agentId]
            : undefined;
          if (!name || delivery.agentName) return delivery;
          return {
            ...delivery,
            agentName: name,
            displayName: name.startsWith('@') ? name : `@${name}`,
            model:
              delivery.model ??
              (delivery.agentId
                ? agentModelsByIdRef.current[delivery.agentId]
                : undefined),
          };
        });
      } catch (error) {
        return dispatchChatDeliverySync({
          type: 'mark_needs_resync',
          sessionId,
          reason: error instanceof Error ? error.message : String(error),
        });
      }
      const next = dispatchChatDeliverySync({
        type: 'member_queue_snapshot',
        sessionId,
        sessionAgentId: queue.session_agent_id,
        deliveries,
        revision,
        receivedAt,
      });
      setSessionRunningIndicator(
        sessionId,
        hasInflightDeliveryForSession(next, sessionId),
      );
      return next;
    },
    [dispatchChatDeliverySync, setSessionRunningIndicator],
  );

  // needsResync consumer: a revision gap, an ambiguous terminal transition or
  // an unknown delivery status triggers the single-flight resync scheduler,
  // which fetches an authoritative snapshot with timed backoff retries until
  // the reducer accepts one and clears the flag.
  const applyChatRuntimeSnapshotRef = useRef(applyChatRuntimeSnapshot);
  applyChatRuntimeSnapshotRef.current = applyChatRuntimeSnapshot;
  const applyChatRuntimeDeltaRef = useRef(applyChatRuntimeDelta);
  applyChatRuntimeDeltaRef.current = applyChatRuntimeDelta;
  const dispatchChatDeliverySyncRef = useRef(dispatchChatDeliverySync);
  dispatchChatDeliverySyncRef.current = dispatchChatDeliverySync;
  const recoverChatRuntime = useCallback(
    async (sessionId: string, requestedAt: number): Promise<void> => {
      let cursor =
        chatDeliveryRuntimeRef.current.sessions[sessionId]?.revision ?? -1;
      if (cursor < 0) {
        const snapshot = await chatRuntimeApi.getSnapshot(sessionId);
        applyChatRuntimeSnapshotRef.current(snapshot, { requestedAt });
        return;
      }

      for (;;) {
        const replay = await chatRuntimeApi.replay(sessionId, cursor);
        if (replay.snapshot) {
          applyChatRuntimeSnapshotRef.current(replay.snapshot, { requestedAt });
          return;
        }
        for (const delta of replay.events) {
          applyChatRuntimeDeltaRef.current(delta);
        }
        const nextRevision = Number(replay.next_revision);
        const currentRevision = Number(replay.current_revision);
        if (
          !Number.isSafeInteger(nextRevision) ||
          !Number.isSafeInteger(currentRevision) ||
          nextRevision < cursor
        ) {
          throw new Error('invalid chat runtime replay cursor');
        }
        cursor = nextRevision;
        if (!replay.has_more) {
          dispatchChatDeliverySyncRef.current({
            type: 'replay_completed',
            sessionId,
            revision: currentRevision,
            receivedAt: Date.now(),
          });
          return;
        }
        if (replay.events.length === 0) {
          throw new Error('chat runtime replay made no progress');
        }
      }
    },
    [],
  );
  const recoverChatRuntimeRef = useRef(recoverChatRuntime);
  recoverChatRuntimeRef.current = recoverChatRuntime;
  const resyncSchedulerRef = useRef<ChatDeliveryResyncScheduler | null>(null);
  if (resyncSchedulerRef.current === null) {
    resyncSchedulerRef.current = new ChatDeliveryResyncScheduler({
      recover: (sessionId, requestedAt) =>
        recoverChatRuntimeRef.current(sessionId, requestedAt),
      onError: (sessionId) => {
        dispatchChatDeliverySyncRef.current({
          type: 'snapshot_failed',
          sessionId,
          receivedAt: Date.now(),
        });
      },
      shouldResync: (sessionId) =>
        sessionsNeedingResync(chatDeliveryRuntimeRef.current).includes(
          sessionId,
        ),
    });
  }
  useEffect(() => {
    const scheduler = resyncSchedulerRef.current;
    return () => scheduler?.dispose();
  }, []);
  useEffect(() => {
    const scheduler = resyncSchedulerRef.current;
    if (!scheduler) return;
    for (const sessionId of sessionsNeedingResync(chatDeliveryRuntime)) {
      scheduler.request(sessionId);
    }
  }, [chatDeliveryRuntime]);
  const setSessionWorkflowRunningIndicator = useCallback(
    (sessionId: string, hasRunningWorkflow: boolean) => {
      if (!sessionId) return;
      const workflowSidebarState: WorkflowSidebarState = hasRunningWorkflow
        ? 'running'
        : 'idle';
      const hasWorkflowError = syncSessionWorkflowErrorIndicator(
        sessionId,
        workflowSidebarState,
      );
      setSessionsAsync((prev) => {
        let changed = false;
        const data = prev.data.map((session) => {
          if (
            session.id !== sessionId ||
            (session.hasRunningWorkflow === hasRunningWorkflow &&
              session.workflowSidebarState === workflowSidebarState &&
              session.hasWorkflowError === hasWorkflowError)
          ) {
            return session;
          }
          changed = true;
          return {
            ...session,
            hasRunningWorkflow,
            workflowSidebarState,
            hasWorkflowError,
          };
        });
        return changed ? { ...prev, data } : prev;
      });
    },
    [syncSessionWorkflowErrorIndicator],
  );
  const setSessionWorkflowStatusIndicators = useCallback(
    (
      sessionId: string,
      status: {
        sidebar_workflow_state?: WorkflowSidebarState | null;
        has_running_workflow: boolean;
        pending_workflow_input_id?: string | null;
        pending_workflow_review_id?: string | null;
      },
    ) => {
      if (!sessionId) return;
      const workflowSidebarState = resolveWorkflowSidebarState(status);
      const hasRunningWorkflow = isWorkflowSidebarRunning(workflowSidebarState);
      const pendingWorkflowInputId = status.pending_workflow_input_id ?? null;
      const pendingWorkflowReviewId = status.pending_workflow_review_id ?? null;
      const hasPendingWorkflowInput = syncSessionWorkflowInputIndicator(
        sessionId,
        pendingWorkflowInputId,
      );
      const hasPendingWorkflowReview = Boolean(pendingWorkflowReviewId);
      const hasWorkflowError = syncSessionWorkflowErrorIndicator(
        sessionId,
        workflowSidebarState,
      );
      setSessionsAsync((prev) => {
        let changed = false;
        const data = prev.data.map((session) => {
          if (
            session.id !== sessionId ||
            (session.hasRunningWorkflow === hasRunningWorkflow &&
              session.workflowSidebarState === workflowSidebarState &&
              session.pendingWorkflowInputId === pendingWorkflowInputId &&
              session.hasPendingWorkflowInput === hasPendingWorkflowInput &&
              session.pendingWorkflowReviewId === pendingWorkflowReviewId &&
              session.hasPendingWorkflowReview === hasPendingWorkflowReview &&
              session.hasWorkflowError === hasWorkflowError)
          ) {
            return session;
          }
          changed = true;
          return {
            ...session,
            hasRunningWorkflow,
            workflowSidebarState,
            pendingWorkflowInputId,
            hasPendingWorkflowInput,
            pendingWorkflowReviewId,
            hasPendingWorkflowReview,
            hasWorkflowError,
          };
        });
        return changed ? { ...prev, data } : prev;
      });
    },
    [syncSessionWorkflowErrorIndicator, syncSessionWorkflowInputIndicator],
  );

  const clearSessionScopedState = useCallback(() => {
    activeSessionIdRef.current = '';
    setActiveSessionId('');
    setMessagesAsync(succeed([]));
    setMembersAsync(succeed([]));
    setMemberQueuesBySessionAgentId({});
    setMainAgentName(null);
  }, []);

  const setSelectedProjectId = useCallback(
    (id: string) => {
      const previousProjectId = selectedProjectIdRef.current;
      selectedProjectIdRef.current = id;
      setSelectedProjectIdState(id);
      try {
        if (id) {
          localStorage.setItem(SELECTED_PROJECT_ID_STORAGE_KEY, id);
        } else {
          localStorage.removeItem(SELECTED_PROJECT_ID_STORAGE_KEY);
        }
      } catch {}

      if (previousProjectId !== id) {
        inboxAutoReadProjectIdRef.current = null;
        inboxInitialUnreadItemIdsRef.current = new Set();
        autoMarkedInboxItemIdsRef.current = new Set();
        setSessionsAsync(succeed([]));
        setArchivedSessionsAsync(succeed([]));
        clearSessionScopedState();
      }
    },
    [clearSessionScopedState],
  );

  const syncSessionLeadAgent = useCallback(
    async (sessionId: string, agentId: string | null): Promise<void> => {
      if (!sessionId || !agentId) return;

      const currentLeadAgentId =
        sessionLeadAgentIdBySessionIdRef.current[sessionId] ?? null;
      if (currentLeadAgentId === agentId) return;

      sessionLeadAgentIdBySessionIdRef.current = {
        ...sessionLeadAgentIdBySessionIdRef.current,
        [sessionId]: agentId,
      };

      try {
        const updatedSession = await chatSessionsApi.update(
          sessionId,
          chatSessionUpdatePayload({ lead_agent_id: agentId }),
        );
        sessionLeadAgentIdBySessionIdRef.current = {
          ...sessionLeadAgentIdBySessionIdRef.current,
          [updatedSession.id]: updatedSession.lead_agent_id,
        };
      } catch (err) {
        sessionLeadAgentIdBySessionIdRef.current = {
          ...sessionLeadAgentIdBySessionIdRef.current,
          [sessionId]: currentLeadAgentId,
        };
        console.warn('Failed to sync workflow lead agent', err);
      }
    },
    [],
  );

  const ensureWorkflowRouteToMainAgent = useCallback(async (): Promise<void> => {
    const sid = activeSessionIdRef.current;
    const agentId = workflowRouteAgentIdRef.current;
    await syncSessionLeadAgent(sid, agentId);
  }, [syncSessionLeadAgent]);

  const setSessionChatInputMode = useCallback(
    (sessionId: string, mode: ChatInputMode) => {
      if (!sessionId) return;
      setChatInputModeBySessionId((prev) => ({
        ...prev,
        [sessionId]: mode,
      }));
    },
    [],
  );

  const setChatInputMode = useCallback(
    (mode?: ChatInputMode) => {
      const sid = activeSessionIdRef.current;
      if (!sid) return;

      const previousMode =
        chatInputModeBySessionId[sid] ?? DEFAULT_CHAT_INPUT_MODE;
      const nextMode =
        mode ?? (previousMode === 'workflow' ? 'free' : 'workflow');

      setChatInputModeBySessionId((prev) => ({
        ...prev,
        [sid]: nextMode,
      }));
      if (nextMode === 'workflow') {
        void ensureWorkflowRouteToMainAgent();
      }

      if (sessionsAsync.source !== 'api') return;

      chatSessionsApi
        .update(sid, {
          ...chatSessionUpdatePayload({
            chat_input_mode: toSessionChatInputMode(nextMode),
          }),
        })
        .then((updatedSession) => {
          setChatInputModeBySessionId((prev) => ({
            ...prev,
            [updatedSession.id]: resolveChatInputMode(
              updatedSession.chat_input_mode,
            ),
          }));
        })
        .catch((err) => {
          setChatInputModeBySessionId((prev) => ({
            ...prev,
            [sid]: previousMode,
          }));
          showToast(
            err instanceof Error
              ? `Mode switch failed: ${err.message}`
              : 'Mode switch failed.',
            'error',
          );
        });
    },
    [chatInputModeBySessionId, ensureWorkflowRouteToMainAgent, sessionsAsync.source],
  );

  return {
    themePreference,
    setThemePreferenceState,
    systemTheme,
    setSystemTheme,
    locale,
    setLocaleState,
    chatMessageFontSize,
    setChatMessageFontSizeState,
    activeSessionId,
    setActiveSessionId,
    sessionsAsync,
    setSessionsAsync,
    archivedSessionsAsync,
    setArchivedSessionsAsync,
    projectsAsync,
    setProjectsAsync,
    selectedProjectId,
    setSelectedProjectIdState,
    allMessages,
    setAllMessages,
    memberQueuesBySessionAgentId,
    setMemberQueuesBySessionAgentId,
    chatDeliveryRuntime,
    dispatchChatDeliverySync,
    chatDeliveryRuntimeRef,
    workflowRuntimeLinesByExecution,
    setWorkflowRuntimeLinesByExecution,
    messagesAsync,
    setMessagesAsync,
    membersAsync,
    setMembersAsync,
    mainAgentName,
    setMainAgentName,
    providersAsync,
    setProvidersAsync,
    skillsAsync,
    setSkillsAsync,
    configAsync,
    setConfigAsync,
    environment,
    setEnvironment,
    inboxSummaryAsync,
    setInboxSummaryAsync,
    inboxItemsAsync,
    setInboxItemsAsync,
    pendingApprovalSessionIds,
    setPendingApprovalSessionIds,
    pendingApprovalSessionIdsRef,
    waitingApprovalSessionIds,
    setWaitingApprovalSessionIds,
    waitingApprovalSessionIdsRef,
    workflowCardAsync,
    setWorkflowCardAsync,
    workspaceChangesAsync,
    setWorkspaceChangesAsync,
    chatInputModeBySessionId,
    setChatInputModeBySessionId,
    strategies,
    setStrategies,
    mockAgentRepliesByMention,
    setMockAgentRepliesByMention,
    selectedStrategyId,
    setSelectedStrategyId,
    selectedOnboardType,
    setSelectedOnboardType,
    smartRouting,
    setSmartRouting,
    showCost,
    setShowCost,
    showExplanation,
    setShowExplanation,
    warnOverDollar,
    setWarnOverDollar,
    weeklyCost,
    setWeeklyCost,
    weeklySaved,
    setWeeklySaved,
    earlyBirdLeft,
    setEarlyBirdLeft,
    activeSettingsTab,
    setActiveSettingsTab,
    isAddMemberModalOpen,
    setIsAddMemberModalOpen,
    isAddProviderModalOpen,
    setIsAddProviderModalOpen,
    toast,
    setToast,
    runActivityStore,
    theme,
    mockBootstrapRef,
    toastDurationMsRef,
    allMessagesRef,
    latestConfigRef,
    configPatchQueueRef,
    publishVisibleConfig,
    ensureConfigPatchQueue,
    saveConfigPatch,
    messagesRequestIdRef,
    queueRequestIdRef,
    inboxRequestIdRef,
    inboxLightRefreshTimerRef,
    inboxSoundProjectIdRef,
    inboxSoundSettingsSignatureRef,
    inboxSoundPrimedRef,
    inboxUnreadSoundIdsRef,
    inboxAutoReadProjectIdRef,
    inboxInitialUnreadItemIdsRef,
    autoMarkedInboxItemIdsRef,
    workspaceChangesRequestIdRef,
    initialRefreshStartedRef,
    initialRefreshCompletedRef,
    sessionRunningIndicatorRequestsRef,
    sessionWorkflowStatusRequestsRef,
    activeSessionIdRef,
    selectedProjectIdRef,
    activeWorkspacePathRef,
    sessionLeadAgentIdBySessionIdRef,
    workflowRouteAgentIdRef,
    agentNamesByIdRef,
    agentModelsByIdRef,
    notifiedTokenUsageSignaturesRef,
    optimisticallyStoppedSessionAgentIdsRef,
    runningAgentSessionIdsRef,
    unreadAgentCompletionSessionIdsRef,
    acknowledgedWorkflowInputIdsRef,
    acknowledgedWorkflowErrorSessionIdsRef,
    persistAgentSessionActivityStorage,
    persistWorkflowInputAcknowledgementStorage,
    persistWorkflowErrorAcknowledgementStorage,
    syncSessionAgentActivityIndicator,
    acknowledgeWorkflowInput,
    syncSessionWorkflowInputIndicator,
    acknowledgeWorkflowError,
    syncSessionWorkflowErrorIndicator,
    clearUnreadAgentCompletion,
    clearPendingWorkflowInput,
    clearWorkflowErrorAttention,
    chatInputMode,
    showToast,
    persistUiPreference,
    setTheme,
    setLocale,
    setChatMessageFontSize,
    makeListSetter,
    setSessions,
    setMembers,
    setProviders,
    setSessionRunningIndicator,
    applyChatRuntimeSnapshot,
    applyChatRuntimeDelta,
    setSessionWorkflowRunningIndicator,
    setSessionWorkflowStatusIndicators,
    clearSessionScopedState,
    setSelectedProjectId,
    syncSessionLeadAgent,
    ensureWorkflowRouteToMainAgent,
    setSessionChatInputMode,
    setChatInputMode,
  };
};
