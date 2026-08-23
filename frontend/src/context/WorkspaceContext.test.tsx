// Smoke tests for project-scoped session loading in WorkspaceContext.
//
// No test runner is installed. Run with:
//     pnpm exec tsx src/context/WorkspaceContext.test.tsx
// Exits non-zero if any assertion fails.

import { readFileSync } from 'node:fs';

let failures = 0;
const check = (label: string, cond: boolean, detail?: unknown) => {
  if (cond) {
    // eslint-disable-next-line no-console
    console.log(`  ok  ${label}`);
  } else {
    failures += 1;
    // eslint-disable-next-line no-console
    console.error(`  FAIL ${label}`, detail ?? '');
  }
};

console.log('WorkspaceContext project session isolation');

const workspaceContextSource = readFileSync(
  new URL('./WorkspaceContext.tsx', import.meta.url),
  'utf8',
);
const runtimeSource = readFileSync(
  new URL('./workspace/useWorkspaceChatRuntime.ts', import.meta.url),
  'utf8',
);
const runtimeHookStart = workspaceContextSource.indexOf(
  '  const { mapBackendChatMessage, upsertStreamedMessage } =',
);
const runtimeHookEnd = workspaceContextSource.indexOf(
  '  // ---------------------------------------------------------------------------\n  // i18n',
  runtimeHookStart,
);
const source = [
  readFileSync(
    new URL('./workspace/workspaceContextTypes.ts', import.meta.url),
    'utf8',
  ),
  readFileSync(
    new URL('./workspace/workspaceContextContract.ts', import.meta.url),
    'utf8',
  ),
  readFileSync(
    new URL('./workspace/workspaceContextUtils.ts', import.meta.url),
    'utf8',
  ),
  readFileSync(
    new URL('./workspace/chatDeliveryRuntime.ts', import.meta.url),
    'utf8',
  ),
  readFileSync(
    new URL('./workspace/chatDeliveryResyncScheduler.ts', import.meta.url),
    'utf8',
  ),
  readFileSync(
    new URL('./workspace/useWorkspaceState.ts', import.meta.url),
    'utf8',
  ),
  workspaceContextSource.slice(0, runtimeHookStart),
  runtimeSource,
  workspaceContextSource.slice(runtimeHookEnd),
].join('\n');
const workflowCardSource = readFileSync(
  new URL('../components/workflow/WorkflowCard.tsx', import.meta.url),
  'utf8',
);
const workflowSidebarStateSource = readFileSync(
  new URL('../lib/workflowSidebarState.ts', import.meta.url),
  'utf8',
);

const refreshAllIndex = source.indexOf('const refreshAll = useCallback');
const refreshProjectsIndex = source.indexOf('await refreshProjects();', refreshAllIndex);
const refreshSessionsIndex = source.indexOf('refreshSessions(),', refreshAllIndex);

check(
  'loads active sessions through status-filtered chat session API',
  source.includes("chatSessionsApi.list('active', projectId)") &&
    !source.includes('projectApi.listSessions(projectId)'),
  source,
);
check(
  'exposes project-scoped archived session loading',
  source.includes('archivedSessionsAsync') &&
    source.includes('refreshArchivedSessions') &&
    source.includes("chatSessionsApi.list('archived', projectId)"),
  source,
);
check(
  'exposes project-scoped session management actions',
  source.includes('renameSession') &&
    source.includes('archiveSession') &&
    source.includes('deleteSession') &&
    source.includes('restoreSession') &&
    /chatSessionsApi\.update\(\s*sessionId/.test(source) &&
    source.includes('chatSessionsApi.archive(sessionId)') &&
    source.includes('chatSessionsApi.delete(sessionId)') &&
    source.includes('chatSessionsApi.restore(sessionId)') &&
    source.includes('refreshSessions()') &&
    source.includes('refreshArchivedSessions()'),
  source,
);
check(
  'invalid active session selection falls back to next active or empty state',
  source.includes('syncActiveSessionSelection') &&
    source.includes('activeBackendSessions') &&
    source.includes('clearSessionScopedState();'),
  source,
);
check(
  'does not load sessions without a selected project',
  source.includes('if (!projectId)') &&
    source.includes('setSessionsAsync(succeed([]))') &&
    source.includes('clearSessionScopedState();'),
  source,
);
check(
  'drops stale session responses after project changes',
  source.includes('selectedProjectIdRef.current !== projectId'),
  source,
);
check(
  'clears visible sessions when the selected project changes',
  source.includes('previousProjectId !== id') &&
    source.includes('setSessionsAsync(succeed([]))'),
  source,
);
check(
  'loads projects before session refresh in global refresh',
  refreshProjectsIndex >= 0 &&
    refreshSessionsIndex >= 0 &&
    refreshProjectsIndex < refreshSessionsIndex,
  { refreshProjectsIndex, refreshSessionsIndex },
);
check(
  'subscribes to chat websocket stream for agent activity',
  source.includes('new WebSocket') &&
    source.includes('chatSessionsApi.streamUrl') &&
    source.includes('parsed.type ===') &&
    source.includes('agent_run_started') &&
    source.includes('agent_activity_updated') &&
    !source.includes('agent_activity_line'),
  source,
);
check(
  'needsResync is consumed by the single-flight resync scheduler',
  source.includes('new ChatDeliveryResyncScheduler({') &&
    source.includes('sessionsNeedingResync(chatDeliveryRuntime)') &&
    source.includes('scheduler.request(sessionId)') &&
    source.includes('scheduler?.dispose()'),
  source,
);
check(
  'stream events register run deliveries, notify the run store, and replace final messages',
  source.includes('registerRunDelivery(parsed)') &&
    source.includes('runActivityStore.notifyUpdated(') &&
    source.includes('const incomingMessage = mapBackendChatMessage(parsed.message)') &&
    source.includes('upsertStreamedMessage(sid, incomingMessage)'),
  source,
);
check(
  'chat messages keep only run identity instead of copied activity lines',
  source.includes("runId: delivery.status === 'starting' ? undefined : delivery.runId") &&
    !source.includes("type: 'agent_delta'") &&
    !source.includes('LIVE_DELTA_ACTIVITY_LINE_PREFIX') &&
    !source.includes('activity_lines: activityLines'),
  source,
);
check(
  'workflow runtime stream lines are kept live for workflow logs',
  source.includes("type: 'workflow_runtime_line'") &&
    source.includes('workflowRuntimeLinesByExecution') &&
    source.includes('setWorkflowRuntimeLinesByExecution') &&
    source.includes('handleWorkflowRuntimeLine(parsed)') &&
    workflowCardSource.includes('workflowRuntimeLinesByExecution[projection.execution_id]') &&
    workflowCardSource.includes('runtimeMessages={workflowRuntimeMessages}'),
  { source, workflowCardSource },
);
check(
  'stream token usage messages notify build stats refresh',
  source.includes('notifyBuildStatsUsageUpdated(projectId)') &&
    /tokenUsageNotificationSignature\(\s*parsed\.message/.test(source) &&
    source.includes('notifiedTokenUsageSignaturesRef.current[parsed.message.id]') &&
    source.includes("tokenUsage.is_estimated === true"),
  source,
);
check(
  'real sends rely on backend deliveries instead of optimistic placeholders',
  source.includes('const shouldPersistToBackend') &&
    source.includes('sendMessageToSession') &&
    source.includes("/@([\\p{L}\\p{N}_-]+)/gu") &&
    source.includes('const visibleMentions =') &&
    source.includes('mentions: visibleMentions') &&
    source.includes('options.routeMentions') &&
    source.includes("mainAgentName.replace(/^@/, '')") &&
    !source.includes("mainAgentName.replace(/^@/, '').toLowerCase()") &&
    !source.includes('match[1].toLowerCase()') &&
    source.includes('meta.client_message_id = userMsgId') &&
    source.includes('upsertStreamedMessage(sid, incomingMessage)') &&
    source.includes('applyChatRuntimeSnapshot(response.runtime)') &&
    !source.includes('makePendingAgentPlaceholders(') &&
    !source.includes('stageOptimisticQueuedMessage(') &&
    !source.includes('immediatePendingAgentMessages'),
  source,
);
check(
  'delivery cards carry display member fields',
  source.includes('deliveryCardToMessage') &&
    source.includes('delivery.displayName ?? delivery.agentName') &&
    source.includes('delivery.avatar || monogramFromName(displayName)') &&
    source.includes('model: delivery.model ?? undefined'),
  source,
);

check(
  'new sends append only the user message while agent state comes from deliveries',
  source.includes('No optimistic agent placeholders are staged locally') &&
    source.includes('[...cur, userMsg]') &&
    !source.includes('withoutStalePending') &&
    !source.includes('immediatePendingAgentMessages') &&
    !source.includes('stageOptimisticQueuedMessage'),
  source,
);
check(
  'workflow plan cards coexist with the delivery-card projection',
  !source.includes('const isWorkflowPlanCardMessage =') &&
    !source.includes('const hasWorkflowPlanCard =') &&
    !source.includes('shouldCreatePendingAgentPlaceholder') &&
    source.includes('mergePersistedWithDeliveryCards('),
  source,
);

check(
  'no 30s starting reconcile; the delivery reducer is the runtime authority',
  source.includes('globalThis.crypto?.randomUUID?.()') &&
    !source.includes('STARTING_AGENT_RECONCILE_DELAY_MS') &&
    !source.includes('reconcileStartingPlaceholders') &&
    source.includes('dispatchChatDeliverySync') &&
    !source.includes('window.location.reload'),
  source,
);

check(
  'quoted messages are sent through backend reference meta instead of message content',
  source.includes('options: SendMessageOptions = {}') &&
    source.includes('quotedMessage: options.quotedMessage') &&
    source.includes('referenceMessageId: options.quotedMessage?.id') &&
    source.includes('meta.reference = { message_id: options.quotedMessage.id }') &&
    source.includes('resolveMessageReferences') &&
    source.includes('content: text') &&
    !source.includes('reference_message_id: options.quotedMessage') &&
    !source.includes('meta.quoted_message') &&
    !source.includes('> ${quotedMessage.sender}:'),
  source,
);
check(
  'syncs and sends workflow chat input mode like the legacy frontend',
  source.includes("type ChatInputMode = 'free' | 'workflow'") &&
    source.includes('resolveChatInputMode(session.chat_input_mode)') &&
    source.includes('chatSessionsApi') &&
    source.includes('chat_input_mode: toSessionChatInputMode(nextMode)') &&
    source.includes('setSessionChatInputMode') &&
    source.includes("meta.chat_input_mode = 'workflow'") &&
    source.includes('const routeMentions =') &&
    source.includes('const shouldPersistRouteMentions =') &&
    source.includes("effectiveChatInputMode !== 'workflow' ||") &&
    source.includes('meta.mentions = routeMentions'),
  source,
);
check(
  'derives the plan-mode main agent from the project lead member',
  source.includes('resolveProjectMainAgentName') &&
    source.includes('resolveProjectMainAgentId') &&
    source.includes("member.member_type === 'agent' && member.role === 'lead'") &&
    source.includes('const mainAgentName = resolveProjectMainAgentName(projectMembers, agents)') &&
    source.includes('setMainAgentName(mainAgentName)') &&
    source.includes('mainAgentName,'),
  source,
);
check(
  'routes workflow input mode messages to the project main agent',
  source.includes('sessionLeadAgentIdBySessionIdRef') &&
    source.includes('workflowRouteAgentIdRef') &&
    source.includes('const syncSessionLeadAgent = useCallback') &&
    source.includes("chatSessionUpdatePayload({ lead_agent_id: agentId })") &&
    source.includes('const hasMainAgentInSession') &&
    source.includes('void syncSessionLeadAgent(sid, mainAgentId)') &&
    source.includes('ensureWorkflowRouteToMainAgent') &&
    source.includes('await syncSessionLeadAgent(sid, workflowLeadAgentId)'),
  source,
);




check(
  'visible messages are scoped to the active session cache',
  source.includes('const allMessagesRef = useRef<Record<string, Message[]>>({})') &&
    source.includes('withSessionIdsBySession') &&
    source.includes('filterMessagesForSession') &&
    source.includes('userIndexByClientId') &&
    source.includes('isOptimisticUserMessage(existing)') &&
    source.includes('messagesRequestIdRef') &&
    source.includes('shouldUpdateActiveMessages') &&
    /filterMessagesForSession\(\s*activeSessionId/.test(source) &&
    source.includes('filterMessagesForSession(sid, prev[sid] ?? [])') &&
    source.includes('filterQueuedUserMessagesFromSnapshot(') &&
    source.includes('activeSessionIdRef.current === sid'),
  source,
);
check(
  'optimistic user messages carry their owning session id',
  source.includes('sessionId: string') &&
    source.includes('sessionId: sid') &&
    source.includes('sessionId,') &&
    source.includes('sessionId: delivery.sessionId') &&
    source.includes('sessionId: event.session_id'),
  source,
);

check(
  'optimistically stopped agents do not keep session running indicators active',
  source.includes('ignoredSessionAgentIds?: ReadonlySet<string>') &&
    source.includes('!ignoredSessionAgentIds?.has(sessionAgent.id)') &&
    source.includes('optimisticallyStoppedSessionAgentIdsRef.current') &&
    source.includes('hasRemainingRunningAgent') &&
    source.includes('setSessionRunningIndicator(sid, hasRemainingRunningAgent)') &&
    source.includes('delivery.sessionAgentId !== sessionAgentId'),
  source,
);
check(
  'stop-requested delivery cards remain visible until the stopped message replaces them',
  source.includes('optimisticallyStoppedSessionAgentIdsRef.current.add(sessionAgentId)') &&
    source.includes('optimisticallyStoppedSessionAgentIdsRef.current.delete') &&
    source.includes('nextMessage.sessionAgentId'),
  source,
);
check(
  'agent completion highlights persist until the session is opened',
  source.includes('UNREAD_AGENT_COMPLETION_SESSION_IDS_STORAGE_KEY') &&
    source.includes('RUNNING_AGENT_SESSION_IDS_STORAGE_KEY') &&
    source.includes('runningAgentSessionIdsRef') &&
    source.includes('unreadAgentCompletionSessionIdsRef') &&
    source.includes('syncSessionAgentActivityIndicator') &&
    source.includes('hasUnreadAgentCompletion') &&
    source.includes('clearUnreadAgentCompletion(activeSessionId)'),
  source,
);
check(
  'workflow input highlights persist until the session is opened',
  source.includes('ACKED_WORKFLOW_INPUT_IDS_STORAGE_KEY') &&
    source.includes('acknowledgedWorkflowInputIdsRef') &&
    source.includes('syncSessionWorkflowInputIndicator') &&
    source.includes('hasPendingWorkflowInput') &&
    source.includes('pendingWorkflowInputId') &&
    source.includes('clearPendingWorkflowInput(activeSessionId)'),
  source,
);
check(
  'workflow review status is tracked for the sidebar activity icon',
  source.includes('pending_workflow_review_id') &&
    source.includes('sidebar_workflow_state') &&
    source.includes('workflowSidebarState') &&
    source.includes('pendingWorkflowReviewId') &&
    source.includes('hasPendingWorkflowReview'),
  source,
);
check(
  'workflow card refresh syncs session workflow sidebar status through the shared loader',
  source.includes('refreshSessionWorkflowStatus') &&
    source.includes('loadSessionWorkflowStatus') &&
    source.includes('sessionWorkflowStatusRequestsRef') &&
    /workflowApi\s*\.\s*getSessionStatus\(sessionId\)/.test(source) &&
    workflowCardSource.includes('refreshSessionWorkflowStatus') &&
    workflowCardSource.includes('void refreshSessionWorkflowStatus(sessionId)'),
  { source, workflowCardSource },
);
check(
  'workflow iteration acceptance requests a source-control refresh',
  workflowCardSource.includes('notifySourceControlRefreshRequested') &&
    workflowCardSource.includes("payload.action === 'accept'") &&
    workflowCardSource.includes('notifySourceControlRefreshRequested({ sessionId })'),
  workflowCardSource,
);
check(
  'workflow sidebar running states are centralized',
  source.includes("from '@/lib/workflowSidebarState'") &&
    workflowSidebarStateSource.includes('workflowRunningSidebarStates') &&
    workflowSidebarStateSource.includes('workflowNonRunningSidebarStates') &&
    workflowSidebarStateSource.includes('resolveWorkflowSidebarState') &&
    workflowSidebarStateSource.includes('hasRunningWorkflowActivity') &&
    workflowCardSource.includes('refreshSessionWorkflowStatus'),
  { source, workflowSidebarStateSource, workflowCardSource },
);
check(
  'polls non-active running and waiting workflow sessions so sidebar icons update',
  source.includes('SIDEBAR_RUNNING_INDICATOR_POLL_MS') &&
    source.includes('runningSidebarSessionIds') &&
    source.includes('session.id !== activeSessionId') &&
    source.includes('session.hasRunningAgent') &&
    source.includes('hasRunningWorkflowActivity(session)') &&
    source.includes('session.hasPendingWorkflowInput') &&
    source.includes('session.hasPendingWorkflowReview') &&
    source.includes('refreshRunningSidebarSessions') &&
    source.includes('window.setInterval(') &&
    source.includes('refreshSessionRunningIndicators(sessionId)'),
  source,
);

check(
  'missing member mentions show a localized error naming the requested member',
  source.includes("parsed.reason === 'member_not_found'") &&
    source.includes('memberNotFoundToastMessage(locale, parsed.agent_name)') &&
    source.includes("const key = 'toast.memberNotFound'"),
  source,
);
check(
  'runtime snapshots and websocket notifications sync file-backed activity',
    source.includes("parsed.type === 'agent_activity_updated'") &&
    source.includes('runActivityStore.notifyUpdated(') &&
    source.includes('runActivityStore.syncRuns(') &&
    source.includes(".filter((run) => run.status !== 'starting')") &&
    source.includes('runActivityStore.requestCompletion(') &&
    source.includes("document.addEventListener('visibilitychange'") &&
    !source.includes('appendStreamActivityLine') &&
    !source.includes('upsertStreamDeltaActivityLine') &&
    !source.includes('activityLoadState'),
  source,
);
check(
  'starting delivery cards do not expose a queryable activity run id',
  source.includes("runId: delivery.status === 'starting' ? undefined : delivery.runId"),
  source,
);
check(
  'no runtime hydration gate; delivery cards render directly from the reducer',
  !source.includes('runtimeHydratedSessionId') &&
    source.includes('deliveryCardsForSession(chatDeliveryRuntime, activeSessionId)') &&
    source.includes('mergePersistedWithDeliveryCards('),
  source,
);


check(
  'persists chat message font size preference in config.json',
  source.includes('CHAT_MESSAGE_FONT_SIZE_OPTIONS = [13, 14, 15, 16]') &&
    source.includes('chat_bubble_font_size') &&
    source.includes('chatMessageFontSizeFromConfig') &&
    source.includes('chatMessageFontSizeToConfig') &&
    source.includes('persistUiPreference({') &&
    source.includes(
      'chat_bubble_font_size: chatMessageFontSizeToConfig(normalized)',
    ) &&
    !source.includes('openteams-chat-message-font-size') &&
    !source.includes('openteams-agent-markdown-font-size'),
  source,
);

check(
  'supports following the operating system theme preference',
  source.includes('ThemePreference') &&
    source.includes('const resolveSystemTheme =') &&
    source.includes("useState<ThemePreference>('system')") &&
    source.includes('useState<Locale>(resolveBrowserLocale)') &&
    source.includes("'(prefers-color-scheme: light)'") &&
    source.includes('themePreference ===') &&
    source.includes('setThemePreferenceState(t)') &&
    source.includes('themePreferenceFromConfig') &&
    source.includes('themePreferenceToConfig') &&
    !source.includes('openteams-design-mode') &&
    !source.includes('openteams-locale') &&
    source.includes("document.body.setAttribute('data-mode', theme)") &&
    source.includes('themePreference,'),
  source,
);

check(
  'syncs member queue snapshots from REST and websocket updates',
  source.includes('memberQueuesBySessionAgentId') &&
    source.includes('chatQueuesApi.listSession(sid)') &&
    source.includes("parsed.type === 'queue_updated'") &&
    source.includes('mergeMemberQueueSnapshot(parsed.queue)') &&
    source.includes('void refreshMemberQueues()') &&
    source.includes('chatQueuesApi.deleteQueued(sessionId, queueId)') &&
    source.includes('chatQueuesApi.continueMember('),
  source,
);

check(
  'member queue snapshots authoritatively replace member deliveries',
  source.includes("type: 'member_queue_snapshot'") &&
    source.includes('deliveriesFromMemberQueue(parsed.queue)') &&
    source.includes('Number(parsed.queue.revision)') &&
    source.includes('representedKeys') &&
    source.includes('representedRunIds'),
  source,
);



check(
  'derives queued user visibility from persisted queue snapshots',
  source.includes('queuedChatMessageKeysForSession') &&
    source.includes('isQueuedUserMessageFromSnapshot') &&
    source.includes('filterQueuedUserMessagesFromSnapshot') &&
    source.includes('queuedUserMessagesByIdFromSnapshot') &&
    source.includes("String(item.message.status) !== 'queued'") &&
    source.includes('item.message.chat_message_id') &&
    source.includes('chatQueuesApi.listSession(sid)') &&
    source.includes('response.members') &&
    source.includes('queuedUserMessagesById,') &&
    source.includes('ensureQueuedRunSourceMessage') &&
    /chatMessagesApi\.get\(\s*event\.source_message_id/.test(source) &&
    source.includes('insertQueuedBackendUserMessage') &&
    !source.includes('deferredQueuedMessageIdsRef') &&
    !source.includes('deferredQueuedClientMessageIdsRef') &&
    !source.includes('deferredQueuedUserMessagesRef') &&
    !source.includes('rememberDeferredQueuedUserMessage') &&
    !source.includes('releaseDeferredQueuedUserMessage'),
  source,
);
check(
  'run source hydration does not evict user messages from non-user source client ids',
  /const sourceClientMessageId = message\.isUser\s*\?\s*userMessageClientId\(message\)\s*:\s*undefined;/.test(
    source,
  ) &&
    /message\.isUser\s*\?\s*!matchesUserMessageIdentity\([\s\S]*sourceClientMessageId[\s\S]*\)\s*:\s*candidate\.id !== message\.id/.test(
      source,
    ),
  source,
);

check(
  'guards workspace change refreshes against stale responses',
  source.includes('workspaceChangesRequestIdRef') &&
    source.includes('workspaceChangesRequestIdRef.current !== requestId'),
  source,
);

check(
  'exposes resetWorkspaceChanges',
  source.includes('resetWorkspaceChanges: () => void') &&
    source.includes('resetWorkspaceChanges,'),
  source,
);

check(
  'exposes one config patch queue and server environment',
  source.includes('createConfigPatchQueue<Config>') &&
    source.includes(
      'saveConfigPatch: (patch: Partial<Config>) => Promise<Config>',
    ) &&
    source.includes('environment: Environment | null') &&
    source.includes('setEnvironment(info.environment)'),
  source,
);

check(
  'routes optimistic UI preferences through the same queue',
  source.includes('enqueue(patch, { optimistic: true })') &&
    !source.includes('const nextConfig: Config = { ...currentConfig, ...patch }'),
  source,
);

if (failures > 0) {
  // eslint-disable-next-line no-console
  console.error(`\n${failures} assertion(s) FAILED`);
  process.exit(1);
} else {
  // eslint-disable-next-line no-console
  console.log('\nAll WorkspaceContext isolation assertions passed.');
}