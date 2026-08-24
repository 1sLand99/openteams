import { useCallback, useEffect } from 'react';
import type {
  BackendChatMessage,
  ChatRuntimeDelta,
  MemberQueueSnapshot,
  Message,
} from '@/types';
import {
  chatMessagesApi,
  chatSessionsApi,
} from '@/lib/api';
import { mapMessage } from '@/lib/mappers';
import { resolveMessageReferences } from '@/lib/messageReferences';
import { notifyBuildStatsUsageUpdated } from '@/lib/buildStatsEvents';
import { notifySourceControlRefreshRequested } from '@/lib/sourceControlEvents';
import { notifyWorkflowGraphUpdated } from '@/lib/workflowEvents';
import { notifyExecutorApprovalChanged } from '@/lib/executorApprovalEvents';
import type { WorkspaceContextProps } from './workspaceContextContract';
import type { ChatStreamEvent } from './workspaceChatStreamTypes';
import {
  activityRunIdsForSession,
  deliveriesFromMemberQueue,
  hasInflightDeliveryForSession,
} from './chatDeliveryRuntime';
import { useWorkspaceState } from './useWorkspaceState';
import {
  CHAT_STREAM_RECONNECT_BASE_DELAY_MS,
  CHAT_STREAM_RECONNECT_MAX_DELAY_MS,
  chatStreamWebSocketUrl,
  filterMessagesForSession,
  isRunningSessionAgentState,
  matchesUserMessageIdentity,
  memberNotFoundToastMessage,
  orderMessagesForConversation,
  tokenUsageNotificationSignature,
  userMessageClientId,
} from './workspaceContextUtils';

type WorkspaceState = ReturnType<typeof useWorkspaceState>;

type ChatRuntimeOptions = WorkspaceState & {
  mergeMemberQueueSnapshot: (queue: MemberQueueSnapshot) => void;
  refreshMemberQueues: WorkspaceContextProps['refreshMemberQueues'];
  refreshMembers: WorkspaceContextProps['refreshMembers'];
  refreshMessages: WorkspaceContextProps['refreshMessages'];
  refreshSessionRunningIndicators: (sessionId: string) => Promise<void>;
  refreshSessionWorkflowStatus: WorkspaceContextProps['refreshSessionWorkflowStatus'];
  refreshSessions: WorkspaceContextProps['refreshSessions'];
  refreshWorkspaceChanges: WorkspaceContextProps['refreshWorkspaceChanges'];
  scheduleInboxRefresh: () => void;
};

export const useWorkspaceChatRuntime = (options: ChatRuntimeOptions) => {
  const {
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
    chatDeliveryRuntimeRef,
    dispatchChatDeliverySync,
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
    mergeMemberQueueSnapshot,
    refreshMemberQueues,
    refreshMembers,
    refreshMessages,
    refreshSessionRunningIndicators,
    refreshSessionWorkflowStatus,
    refreshSessions,
    refreshWorkspaceChanges,
    scheduleInboxRefresh,
  } = options;
  const mapBackendChatMessage = useCallback(
    (message: BackendChatMessage): Message =>
      mapMessage(message, {
        agentNamesById: agentNamesByIdRef.current,
        agentModelsById: agentModelsByIdRef.current,
      }),
    [],
  );

  const insertQueuedBackendUserMessage = useCallback(
    (sid: string, runId: string, message: Message) => {
      setAllMessages((prev) => {
        const current = filterMessagesForSession(sid, prev[sid] ?? []);
        const sourceClientMessageId = message.isUser
          ? userMessageClientId(message)
          : undefined;
        const withoutExistingSourceMessage = current.filter((candidate) =>
          message.isUser
            ? !matchesUserMessageIdentity(
                candidate,
                message.id,
                sourceClientMessageId,
              )
            : candidate.id !== message.id,
        );

        const runIndex = withoutExistingSourceMessage.findIndex(
          (candidate) => candidate.isAgentRunning && candidate.runId === runId,
        );
        const next = [...withoutExistingSourceMessage];
        next.splice(runIndex >= 0 ? runIndex : next.length, 0, message);
        return { ...prev, [sid]: resolveMessageReferences(next) };
      });
    },
    [],
  );

  const ensureQueuedRunSourceMessage = useCallback(
    async (
      event: Extract<ChatStreamEvent, { type: 'agent_run_started' }>,
    ): Promise<void> => {
      try {
        const backendMessage = await chatMessagesApi.get(
          event.source_message_id,
        );
        insertQueuedBackendUserMessage(
          event.session_id,
          event.run_id,
          mapBackendChatMessage(backendMessage),
        );
      } catch {
        // Source-message hydration is best-effort; the running placeholder still shows.
      }
    },
    [insertQueuedBackendUserMessage, mapBackendChatMessage],
  );

  const upsertStreamedMessage = useCallback(
    (sid: string, incoming: Message) => {
      setAllMessages((prev) => {
        const current = filterMessagesForSession(sid, prev[sid] ?? []);
        const nextMessage: Message = {
          ...incoming,
          isAgentRunning: undefined,
          isThinking: undefined,
        };
        if (!nextMessage.isUser && nextMessage.sessionAgentId) {
          optimisticallyStoppedSessionAgentIdsRef.current.delete(
            nextMessage.sessionAgentId,
          );
        }
        const nextClientMessageId = userMessageClientId(nextMessage);
        const existingIndex = current.findIndex((message) => {
          if (message.id === nextMessage.id) return true;
          return (
            nextMessage.isUser &&
            nextClientMessageId !== undefined &&
            userMessageClientId(message) === nextClientMessageId
          );
        });
        const next =
          existingIndex >= 0
            ? current.map((message, index) =>
                index === existingIndex ? nextMessage : message,
              )
            : [...current, nextMessage];
        return {
          ...prev,
          [sid]: resolveMessageReferences(orderMessagesForConversation(next)),
        };
      });
    },
    [],
  );

  const registerRunDelivery = useCallback(
    (event: Extract<ChatStreamEvent, { type: 'agent_run_started' }>) => {
      // A new run for this agent supersedes any optimistic-stop suppression.
      optimisticallyStoppedSessionAgentIdsRef.current.delete(
        event.session_agent_id,
      );
      setSessionRunningIndicator(event.session_id, true);
      void ensureQueuedRunSourceMessage(event);
      // Compatibility metadata only. Durable runtime state is exclusively
      // applied from versioned outbox deltas/snapshots.
    },
    [ensureQueuedRunSourceMessage, setSessionRunningIndicator],
  );

  const handleWorkflowRuntimeLine = useCallback(
    (event: Extract<ChatStreamEvent, { type: 'workflow_runtime_line' }>) => {
      setWorkflowRuntimeLinesByExecution((prev) => {
        const executionLines = prev[event.execution_id] ?? [];
        if (executionLines.some((line) => line.id === event.line_id)) {
          return prev;
        }

        return {
          ...prev,
          [event.execution_id]: [
            ...executionLines,
            {
              id: event.line_id,
              executionId: event.execution_id,
              workflowAgentSessionId: event.workflow_agent_session_id,
              stepId: event.step_id,
              stepKey: event.step_key,
              agentId: event.agent_id,
              agentName: event.agent_name,
              streamType: event.stream_type,
              content: event.content,
              createdAt: event.created_at,
            },
          ],
        };
      });
    },
    [],
  );

  // When the active session changes, re-fetch its scoped data.
  useEffect(() => {
    if (!activeSessionId) return;
    void refreshMessages();
    void refreshMembers();
    void refreshMemberQueues();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeSessionId]);

  useEffect(() => {
    if (!activeSessionId || sessionsAsync.source !== 'api') return;

    const sid = activeSessionId;
    let socket: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    let reconnectAttempt = 0;
    let hasConnectedOnce = false;
    let disposed = false;

    const handleMessage = (event: MessageEvent) => {
      let parsed: ChatStreamEvent;
      try {
        parsed = JSON.parse(event.data) as ChatStreamEvent;
      } catch {
        return;
      }

      if (parsed.type === 'runtime_delta' && parsed.session_id === sid) {
        applyChatRuntimeDelta(parsed as unknown as ChatRuntimeDelta);
        return;
      }

      if (
        parsed.type === 'runtime_resync_required' &&
        parsed.session_id === sid
      ) {
        dispatchChatDeliverySync({
          type: 'mark_needs_resync',
          sessionId: sid,
          reason: parsed.reason,
        });
        return;
      }

      if (parsed.type === 'agent_run_started' && parsed.session_id === sid) {
        registerRunDelivery(parsed);
        return;
      }

      if (
        parsed.type === 'agent_activity_updated' &&
        parsed.session_id === sid
      ) {
        runActivityStore.notifyUpdated(
          parsed.run_id,
          parsed.latest_sequence,
        );
        return;
      }

      if (
        parsed.type === 'workflow_runtime_line' &&
        parsed.session_id === sid
      ) {
        setSessionWorkflowRunningIndicator(sid, true);
        handleWorkflowRuntimeLine(parsed);
        return;
      }

      if (
        parsed.type === 'workflow_execution_updated' &&
        parsed.session_id === sid
      ) {
        void refreshSessionRunningIndicators(sid);
        scheduleInboxRefresh();
        return;
      }

      if (
        parsed.type === 'workflow_graph_updated' &&
        parsed.session_id === sid
      ) {
        notifyWorkflowGraphUpdated({
          sessionId: parsed.session_id,
          executionId: parsed.execution_id,
          graphVersion: parsed.graph_version,
          reason: parsed.reason,
          changedStepIds: parsed.changed_step_ids,
        });
        return;
      }

      if (
        parsed.type === 'file_change_refresh' &&
        parsed.session_id === sid
      ) {
        const projectId = selectedProjectIdRef.current;
        notifySourceControlRefreshRequested({
          projectId,
          sessionId: sid,
        });
        const workspacePath = activeWorkspacePathRef.current;
        if (!projectId && workspacePath) {
          void refreshWorkspaceChanges(sid, workspacePath, true);
        }
        return;
      }

      if (parsed.type === 'queue_updated' && parsed.session_id === sid) {
        mergeMemberQueueSnapshot(parsed.queue);
        // Rolling-upgrade path for servers that only emit queue snapshots.
        // The member queue snapshot is authoritative per member and carries
        // the durable delivery/run binding. Unknown statuses request a resync
        // instead of guessing a terminal state.
        try {
          const revision = Number(parsed.queue.revision);
          const current = chatDeliveryRuntimeRef.current.sessions[sid];
          if (!current || current.revision < 0) {
            dispatchChatDeliverySync({
              type: 'mark_needs_resync',
              sessionId: sid,
              reason: 'legacy queue event received before runtime hydration',
            });
            return;
          }
          if (revision <= current.revision) return;
          if (revision !== current.revision + 1) {
            dispatchChatDeliverySync({
              type: 'mark_needs_resync',
              sessionId: sid,
              reason: `legacy queue revision gap ${current.revision}->${revision}`,
            });
            return;
          }
          // Fill display metadata from the member directory; the durable
          // queue projection intentionally stores identity, not presentation.
          const queueDeliveries = deliveriesFromMemberQueue(parsed.queue).map(
            (delivery) => {
              const name = delivery.agentId
                ? agentNamesByIdRef.current[delivery.agentId]
                : undefined;
              if (!name || delivery.agentName) return delivery;
              return {
                ...delivery,
                agentName: name,
                displayName: name.startsWith('@') ? name : `@${name}`,
              };
            },
          );
          dispatchChatDeliverySync({
            type: 'member_queue_snapshot',
            sessionId: sid,
            sessionAgentId: parsed.session_agent_id,
            deliveries: queueDeliveries,
            revision: Number(parsed.queue.revision),
            receivedAt: Date.now(),
          });
        } catch (error) {
          dispatchChatDeliverySync({
            type: 'mark_needs_resync',
            sessionId: sid,
            reason: error instanceof Error ? error.message : String(error),
          });
        }
        return;
      }

      if (
        (parsed.type === 'executor_approval_requested' ||
          parsed.type === 'executor_approval_resolved' ||
          parsed.type === 'executor_approval_cancelled' ||
          parsed.type === 'executor_approval_expired') &&
        parsed.session_id === sid
      ) {
        notifyExecutorApprovalChanged({
          sessionId: sid,
          type: parsed.type,
          requestId: parsed.request_id,
          request: parsed.request,
        });
        void refreshMembers();
        scheduleInboxRefresh();
        return;
      }

      if (
        (parsed.type === 'message_new' || parsed.type === 'message_updated') &&
        parsed.message.session_id === sid
      ) {
        const tokenUsageSignature = tokenUsageNotificationSignature(
          parsed.message,
        );
        if (
          tokenUsageSignature &&
          notifiedTokenUsageSignaturesRef.current[parsed.message.id] !==
            tokenUsageSignature
        ) {
          notifiedTokenUsageSignaturesRef.current[parsed.message.id] =
            tokenUsageSignature;
          const projectId = selectedProjectIdRef.current;
          if (projectId) {
            notifyBuildStatsUsageUpdated(projectId);
          }
        }
        const incomingMessage = mapBackendChatMessage(parsed.message);
        upsertStreamedMessage(sid, incomingMessage);
        // Note: a message carrying a runId (e.g. an intermediate agent
        // protocol send) must NOT remove the active run. Only terminal
        // `agent_state` events and fresh snapshots may end the projection.
        scheduleInboxRefresh();
        return;
      }

      if (parsed.type === 'agent_state') {
        if (
          parsed.run_id &&
          !isRunningSessionAgentState(parsed.state)
        ) {
          runActivityStore.requestCompletion(parsed.run_id);
        }
        if (isRunningSessionAgentState(parsed.state)) {
          setSessionRunningIndicator(sid, true);
          // No delivery upsert here: cards are created exclusively from the
          // versioned outbox delta/snapshot. An agent_state event is only a
          // member scheduling projection and has no delivery revision.
        } else {
          // Member state is only a scheduling projection. The delivery card
          // ends when a versioned outbox delta removes/finalises it.
          void refreshSessionWorkflowStatus(sid);
        }
        void refreshMembers();
        return;
      }

      if (parsed.type === 'mention_error' && parsed.session_id === sid) {
        if (parsed.reason === 'member_not_found') {
          showToast(
            memberNotFoundToastMessage(locale, parsed.agent_name),
            'error',
          );
        }
        // No placeholder cleanup needed: there are no optimistic agent
        // placeholders anymore; deliveries come from the backend.
      }
    };

    // Open the stream and keep it alive across transient drops. The stream has
    // no server-side replay, so on every *re*connect we re-hydrate the session
    // via REST to recover any persisted messages emitted while we were down.
    const connect = () => {
      if (disposed) return;
      const ws = new WebSocket(
        chatStreamWebSocketUrl(chatSessionsApi.streamUrl(sid)),
      );
      socket = ws;
      ws.onmessage = handleMessage;
      ws.onopen = () => {
        reconnectAttempt = 0;
        if (hasConnectedOnce) {
          dispatchChatDeliverySync({
            type: 'mark_needs_resync',
            sessionId: sid,
            reason: 'chat websocket reconnected',
          });
          void refreshMessages();
          void refreshMembers();
          void refreshMemberQueues();
          const projectId = selectedProjectIdRef.current;
          if (projectId) {
            notifySourceControlRefreshRequested({
              projectId,
              sessionId: sid,
            });
          }
          const workspacePath = activeWorkspacePathRef.current;
          if (!projectId && workspacePath) {
            void refreshWorkspaceChanges(sid, workspacePath, true);
          }
        }
        hasConnectedOnce = true;
      };
      ws.onclose = () => {
        // Ignore the close of a superseded socket or one closed by cleanup.
        if (disposed || socket !== ws) return;
        const delay = Math.min(
          CHAT_STREAM_RECONNECT_BASE_DELAY_MS * 2 ** reconnectAttempt,
          CHAT_STREAM_RECONNECT_MAX_DELAY_MS,
        );
        reconnectAttempt += 1;
        reconnectTimer = setTimeout(connect, delay);
      };
      // Let onclose drive the reconnect; just tear the socket down on error.
      ws.onerror = () => {
        ws.close();
      };
    };

    connect();

    return () => {
      disposed = true;
      if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = null;
      }
      socket?.close();
    };
  }, [
    activeSessionId,
    dispatchChatDeliverySync,
    handleWorkflowRuntimeLine,
    registerRunDelivery,
    applyChatRuntimeDelta,
    locale,
    mapBackendChatMessage,
    mergeMemberQueueSnapshot,
    refreshMessages,
    refreshMemberQueues,
    refreshSessionRunningIndicators,
    refreshSessionWorkflowStatus,
    refreshWorkspaceChanges,
    refreshMembers,
    runActivityStore,
    scheduleInboxRefresh,
    setSessionRunningIndicator,
    setSessionWorkflowRunningIndicator,
    sessionsAsync.source,
    upsertStreamedMessage,
  ]);

  useEffect(() => {
    const syncVisibleRuns = () => {
      if (document.visibilityState !== 'visible' || !activeSessionId) return;
      runActivityStore.syncRuns(
        activityRunIdsForSession(chatDeliveryRuntime, activeSessionId),
      );
    };
    document.addEventListener('visibilitychange', syncVisibleRuns);
    return () => {
      document.removeEventListener('visibilitychange', syncVisibleRuns);
    };
  }, [chatDeliveryRuntime, activeSessionId, runActivityStore]);

  useEffect(() => {
    if (!initialRefreshCompletedRef.current) return;
    void refreshSessions();
  }, [refreshSessions, selectedProjectId]);

  return {
    mapBackendChatMessage,
    upsertStreamedMessage,
  };
};
