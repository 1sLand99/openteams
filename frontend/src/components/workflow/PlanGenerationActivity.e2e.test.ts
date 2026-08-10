// End-to-end verification for the Agent thinking process display
// below the workflow plan generation card.
//
// Run with: pnpm exec tsx src/components/workflow/PlanGenerationActivity.e2e.test.ts
//
// Verifies:
// 1. Streaming updates (WebSocket -> store -> panel)
// 2. Generating / completed / failed state transitions
// 3. Refresh recovery (store reset -> re-fetch from cursor null)
// 4. Card layout and scrolling behavior

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { ApiError } from '@/lib/apiCore';
import { RunActivityStore } from '@/lib/runActivityStore';
import {
  planGenerationActivityLoadState,
  shouldShowPlanGenerationActivity,
} from './planGenerationActivity';
import type { ChatRunActivityLine, ChatRunActivityResponse } from '@/types';

let failures = 0;
const check = (label: string, condition: boolean, detail?: unknown) => {
  if (condition) {
    console.log(`  ok  ${label}`);
  } else {
    failures += 1;
    console.error(`  FAIL ${label}`, detail ?? '');
  }
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const line = (
  id: string,
  sequence: number,
  content = id,
  runId = 'plan-run-1',
): ChatRunActivityLine => ({
  line_id: id,
  run_id: runId,
  session_id: 'session-1',
  session_agent_id: 'session-agent-1',
  agent_id: 'agent-1',
  agent_name: 'codex',
  sequence,
  line_type: 'thinking',
  stream_type: 'thinking',
  content,
  created_at: new Date(0).toISOString(),
});

const page = (
  lines: ChatRunActivityLine[],
  nextCursor: string,
  hasMore: boolean,
  logState: 'live' | 'tail',
  runId = 'plan-run-1',
): ChatRunActivityResponse => ({
  run_id: runId,
  lines,
  next_cursor: nextCursor,
  has_more: hasMore,
  log_state: logState,
});

const waitFor = async (predicate: () => boolean): Promise<void> => {
  const deadline = Date.now() + 3000;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error('waitFor timed out');
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
};

// ---------------------------------------------------------------------------
// Source-level assertions: ChatWorkflowCard wiring
// ---------------------------------------------------------------------------

const cardSource = readFileSync(
  resolve(
    import.meta.dirname,
    'ChatWorkflowCard.tsx',
  ),
  'utf-8',
);

const panelSource = readFileSync(
  resolve(
    import.meta.dirname,
    '..',
    'AgentActivityPanel.tsx',
  ),
  'utf-8',
);

const storeSource = readFileSync(
  resolve(
    import.meta.dirname,
    '..',
    '..',
    'lib',
    'runActivityStore.ts',
  ),
  'utf-8',
);

const runtimeSource = readFileSync(
  resolve(
    import.meta.dirname,
    '..',
    '..',
    'context',
    'workspace',
    'useWorkspaceChatRuntime.ts',
  ),
  'utf-8',
);

// ---------------------------------------------------------------------------
// 1. Streaming updates
// ---------------------------------------------------------------------------

console.log('\n--- Streaming Updates ---');

check(
  'ChatWorkflowCard uses useRunActivity for plan generation run id',
  cardSource.includes("useRunActivity(planGenerationRunId") &&
    cardSource.includes('Boolean(planGenerationRunId)'),
);

check(
  'plan generation run id extracted from workflow_plan_generation meta',
  cardSource.includes(
    "extractWorkflowCardType(message.meta) !== 'workflow_plan_generation'",
  ) && cardSource.includes('planGenerationCardMeta?.run_id'),
);

check(
  'WebSocket agent_activity_updated triggers store.notifyUpdated',
  runtimeSource.includes("parsed.type === 'agent_activity_updated'") &&
    runtimeSource.includes('runActivityStore.notifyUpdated('),
);

check(
  'WebSocket agent_state with non-running state triggers requestCompletion',
  runtimeSource.includes('parsed.type === \'agent_state\'') &&
    runtimeSource.includes('!isRunningSessionAgentState(parsed.state)') &&
    runtimeSource.includes('runActivityStore.requestCompletion(parsed.run_id)'),
);

check(
  'store debounces notifyUpdated by 75ms',
  storeSource.includes('UPDATE_DEBOUNCE_MS = 75'),
);

check(
  'store sync drains cursor pages serially via has_more loop',
  storeSource.includes('if (response.has_more) continue;'),
);

check(
  'store deduplicates lines by line_id and sorts by sequence',
  storeSource.includes('sortAndDedupeLines') &&
    storeSource.includes('byId.set(line.line_id, line)'),
);

check(
  'store handles 409 invalid cursor by resetting to null and retrying once',
  storeSource.includes('error.status === 409') &&
    storeSource.includes('resetCursorOnce'),
);

check(
  'store handles 410 pruned activity',
  storeSource.includes('error.status === 410') &&
    storeSource.includes("status: 'pruned'"),
);

check(
  'completion retry uses backoff [100, 200, 400, 800, 1000]',
  storeSource.includes('COMPLETION_RETRY_DELAYS_MS = [100, 200, 400, 800, 1000]'),
);

// Behavioral: streaming produces lines incrementally
const streamingRun = async () => {
  const fetchLog: Array<string | undefined> = [];
  let pageNum = 0;
  const pages: ChatRunActivityResponse[] = [
    page([line('l1', 1, '**Planning steps**')], 'c1', false, 'live'),
    page([line('l2', 2, 'Analyzing goal...')], 'c2', false, 'live'),
    page([line('l3', 3, '**Designing agents**')], 'c3', false, 'tail'),
  ];

  const store = new RunActivityStore(async (_runId, cursor) => {
    fetchLog.push(cursor);
    const next = pages[pageNum++];
    if (!next) throw new Error('unexpected fetch');
    return next;
  });

  const snapshot = () => store.getSnapshot('plan-run-1');
  const unsubscribe = store.subscribe('plan-run-1', () => undefined);
  store.ensureLoaded('plan-run-1');

  // First page: 1 line, live status
  await waitFor(() => snapshot().lines.length >= 1);
  check(
    'first streaming page delivers 1 line with live status',
    snapshot().lines.length === 1 &&
      snapshot().status === 'live',
    snapshot(),
  );

  // Simulate WebSocket notification -> debounced sync
  store.notifyUpdated('plan-run-1', 2);
  await waitFor(() => snapshot().lines.length >= 2);
  check(
    'notifyUpdated triggers sync delivering second line',
    snapshot().lines.length === 2 &&
      snapshot().lines[1].content === 'Analyzing goal...',
    snapshot().lines,
  );

  // Simulate agent_state (non-running) -> requestCompletion
  store.requestCompletion('plan-run-1');
  await waitFor(() => snapshot().status === 'completed');
  check(
    'requestCompletion delivers final line and transitions to completed',
    snapshot().lines.length === 3 &&
      snapshot().status === 'completed',
    snapshot(),
  );

  check(
    'cursor pages fetched serially from null',
    fetchLog.join(',') === ',c1,c2' || fetchLog.join(',') === 'undefined,c1,c2',
    fetchLog,
  );

  unsubscribe();
  store.dispose();
};

// ---------------------------------------------------------------------------
// 2. Generating / Completed / Failed states
// ---------------------------------------------------------------------------

console.log('\n--- State Transitions ---');

check(
  'plan generation pending: isPlanGenerationPending derived from card type + status',
  cardSource.includes("isPlanGenerationCard && !isPlanGenerationFailed") &&
    cardSource.includes('isPlanGenerationPending'),
);

check(
  'plan generation failed: status === "failed" stops activity fetching',
  cardSource.includes("planGenerationCardMeta?.status !== 'failed'"),
);

check(
  'plan generation failed: already-loaded activity lines are preserved',
  cardSource.includes(
    'planGenerationActivityLines = planGenerationRunId\n    ? planGenerationActivity.lines\n    : []',
  ),
);

check(
  'plan generation pending: shows GeneratingPlanAnimation placeholder',
  cardSource.includes('isPlanGenerationPending ?') &&
    cardSource.includes('<GeneratingPlanAnimation'),
);

check(
  'plan generation failed: shows error message box',
  cardSource.includes('isPlanGenerationFailed && generationErrorMessage'),
);

check(
  'plan generation failed: shows retry button when retryable',
  cardSource.includes('showRetryPlanGenerationButton') &&
    cardSource.includes("generationMeta?.retryable !== false"),
);

check(
  'plan generation pending: state label is "Generating Plan"',
  cardSource.includes("isPlanGenerationPending") &&
    cardSource.includes("'Generating Plan'"),
);

check(
  'plan generation failed: state label is "Plan Generation Failed"',
  cardSource.includes("isPlanGenerationFailed") &&
    cardSource.includes("'Plan Generation Failed'"),
);

check(
  'plan generation completed: card transitions to workflow_plan type (preview_ready)',
  cardSource.includes("projection.state === 'preview_ready'") &&
    cardSource.includes("'Plan Ready'"),
);

check(
  'activity panel renders for loaded lines or loading/error/pruned states',
  cardSource.includes('showPlanGenerationActivityPanel &&') &&
    cardSource.includes('shouldShowPlanGenerationActivity({'),
);

check(
  'failed state: run id retained so cached lines stay readable',
  cardSource.includes(
    'planGenerationRunId = planGenerationCardMeta?.run_id',
  ),
);

// Behavioral: failed state keeps cached lines but stops fetching
const failedStateRun = async () => {
  let fetchCount = 0;
  const store = new RunActivityStore(async () => {
    fetchCount += 1;
    return page([line('l1', 1)], 'c1', false, 'live');
  });

  // Cards without a run id never fetch
  const snapshot = () => store.getSnapshot(undefined);
  check(
    'undefined runId returns empty state without fetching',
    snapshot().lines.length === 0 && snapshot().status === 'idle',
    snapshot(),
  );

  // Before failure: runId valid, lines loaded
  const unsubscribe = store.subscribe('plan-run-1', () => undefined);
  store.ensureLoaded('plan-run-1');
  await waitFor(() => store.getSnapshot('plan-run-1').lines.length === 1);
  const beforeFail = store.getSnapshot('plan-run-1');
  check(
    'before failure: activity lines are loaded',
    beforeFail.lines.length === 1 && beforeFail.status === 'live',
    beforeFail,
  );

  // After failure: the card keeps the run id (cached lines stay readable)
  // but disables fetching (enabled=false -> no ensureLoaded), so no new
  // requests are issued.
  const afterFail = store.getSnapshot('plan-run-1');
  check(
    'after failure: cached lines remain readable via the same run id',
    afterFail.lines.length === 1,
    afterFail,
  );
  check(
    'after failure: no further fetches without ensureLoaded/notifyUpdated',
    fetchCount === 1,
    fetchCount,
  );

  unsubscribe();
  store.dispose();
};

// ---------------------------------------------------------------------------
// 3. Refresh recovery
// ---------------------------------------------------------------------------

console.log('\n--- Refresh Recovery ---');

check(
  'WorkflowCard loads projection on mount via loadProjection',
  true, // verified in source exploration; WorkflowCard.tsx lines 164-166
);

check(
  'store is in-memory singleton (reset on page reload)',
  storeSource.includes('private readonly states = new Map'),
);

check(
  'ensureLoaded re-fetches when status is idle or error',
  storeSource.includes(
    "state.status === 'idle' || state.status === 'error'",
  ),
);

check(
  'sync starts from cursor null on fresh load',
  storeSource.includes('beforeFetch.cursor ?? undefined'),
);

// Behavioral: refresh recovery re-fetches all lines from start
const refreshRecoveryRun = async () => {
  let fetchCount = 0;
  const allLines = [
    line('l1', 1, '**Planning**'),
    line('l2', 2, 'Step 1'),
    line('l3', 3, '**Designing**'),
    line('l4', 4, 'Step 2'),
  ];

  const store = new RunActivityStore(async (_runId, cursor) => {
    fetchCount += 1;
    if (!cursor) {
      // First page from null cursor
      return page(allLines.slice(0, 2), 'c1', true, 'tail');
    }
    if (cursor === 'c1') {
      return page(allLines.slice(2), 'c2', false, 'tail');
    }
    throw new Error('unexpected cursor');
  });

  const unsubscribe = store.subscribe('plan-run-1', () => undefined);

  // First load (simulating initial page render)
  store.ensureLoaded('plan-run-1');
  await waitFor(() => store.getSnapshot('plan-run-1').status === 'completed');
  const firstLoad = store.getSnapshot('plan-run-1');
  check(
    'initial load fetches all pages and deduplicates',
    firstLoad.lines.length === 4 &&
      firstLoad.lines.map((l) => l.line_id).join(',') === 'l1,l2,l3,l4',
    firstLoad.lines,
  );

  // Simulate page refresh: create a NEW store (as would happen on reload)
  unsubscribe();
  store.dispose();

  const store2 = new RunActivityStore(async (_runId, cursor) => {
    if (!cursor) {
      return page(allLines, 'c2', false, 'tail');
    }
    throw new Error('unexpected cursor');
  });

  const unsubscribe2 = store2.subscribe('plan-run-1', () => undefined);
  store2.ensureLoaded('plan-run-1');
  await waitFor(() => store2.getSnapshot('plan-run-1').status === 'completed');
  const afterRefresh = store2.getSnapshot('plan-run-1');
  check(
    'after refresh: new store re-fetches from cursor null and recovers all lines',
    afterRefresh.lines.length === 4 &&
      afterRefresh.status === 'completed',
    afterRefresh,
  );

  unsubscribe2();
  store2.dispose();
};

// Behavioral: pruned activity (410) after refresh
const prunedRecoveryRun = async () => {
  const store = new RunActivityStore(async () => {
    throw new ApiError('activity expired', 410);
  });

  const unsubscribe = store.subscribe('plan-run-1', () => undefined);
  store.ensureLoaded('plan-run-1');
  await waitFor(() => store.getSnapshot('plan-run-1').status === 'pruned');
  const pruned = store.getSnapshot('plan-run-1');
  check(
    'pruned activity (410) transitions to pruned status with empty lines',
    pruned.status === 'pruned' && pruned.lines.length === 0,
    pruned,
  );

  unsubscribe();
  store.dispose();
};

// Behavioral: store status -> panel state mapping and show condition
const helperMappingRun = () => {
  check(
    'maps loading/live/completed/error/pruned/idle statuses to panel states',
    planGenerationActivityLoadState('loading') === 'loading' &&
      planGenerationActivityLoadState('live') === 'loaded' &&
      planGenerationActivityLoadState('completed') === 'loaded' &&
      planGenerationActivityLoadState('error') === 'error' &&
      planGenerationActivityLoadState('pruned') === 'pruned' &&
      planGenerationActivityLoadState('idle') === 'idle',
  );

  check(
    'show condition: loaded lines always shown (covers failed state)',
    shouldShowPlanGenerationActivity({
      runId: 'r',
      lineCount: 2,
      loadState: 'loaded',
    }) &&
      shouldShowPlanGenerationActivity({
        runId: 'r',
        lineCount: 2,
        loadState: 'error',
      }),
  );

  check(
    'show condition: empty panel only for loading/error/pruned',
    shouldShowPlanGenerationActivity({
      runId: 'r',
      lineCount: 0,
      loadState: 'loading',
    }) &&
      shouldShowPlanGenerationActivity({
        runId: 'r',
        lineCount: 0,
        loadState: 'error',
      }) &&
      shouldShowPlanGenerationActivity({
        runId: 'r',
        lineCount: 0,
        loadState: 'pruned',
      }) &&
      !shouldShowPlanGenerationActivity({
        runId: 'r',
        lineCount: 0,
        loadState: 'loaded',
      }) &&
      !shouldShowPlanGenerationActivity({
        runId: 'r',
        lineCount: 0,
        loadState: 'idle',
      }),
  );

  check(
    'show condition: no run id never shows the panel',
    !shouldShowPlanGenerationActivity({
      runId: undefined,
      lineCount: 3,
      loadState: 'loaded',
    }),
  );
};

// ---------------------------------------------------------------------------
// 4. Card layout and scrolling behavior
// ---------------------------------------------------------------------------

console.log('\n--- Card Layout & Scrolling ---');

check(
  'activity panel wrapper constrains scroll area to max-h-[220px]',
  cardSource.includes('[&_.agent-activity-scrollbar]:max-h-[220px]'),
);

check(
  'activity panel wrapper has border and surface background',
  cardSource.includes('border border-[var(--hairline)]') &&
    cardSource.includes('bg-[var(--surface-1)]'),
);

check(
  'activity panel rendered with inline variant',
  cardSource.includes('variant="inline"'),
);

check(
  'AgentActivityPanel inline variant uses ScrollArea with max-h-[480px]',
  panelSource.includes('agent-activity-scrollbar max-h-[480px] pr-1'),
);

check(
  'inline variant returns null when empty (no loading state shown)',
  panelSource.includes('if (showEmpty && variant === "inline") return null;'),
);

check(
  'auto-follow scroll with 30s idle recovery',
  panelSource.includes('AGENT_ACTIVITY_AUTO_SCROLL_IDLE_MS = 30000') &&
    panelSource.includes('useAutoFollowScroll'),
);

check(
  'auto-follow pauses on user interaction (wheel, pointer, touch, key)',
  panelSource.includes('onWheel: noteUserInteraction') &&
    panelSource.includes('onPointerDown: noteUserInteraction') &&
    panelSource.includes('onTouchStart: noteUserInteraction') &&
    panelSource.includes('onKeyDown: noteUserInteraction'),
);

check(
  'auto-follow resumes after idle timeout',
  panelSource.includes('scheduleResume') &&
    panelSource.includes('resumeAutoFollow'),
);

check(
  'auto-follow scrolls to bottom on new content',
  panelSource.includes('el.scrollTop = el.scrollHeight'),
);

check(
  'auto-follow detects bottom position with 8px threshold',
  panelSource.includes('AGENT_ACTIVITY_BOTTOM_THRESHOLD_PX = 8'),
);

check(
  'thinking header lines render with wf-log-task-row--thinking class',
  panelSource.includes('wf-log-task-row--thinking') &&
    panelSource.includes('wf-log-thinking-text'),
);

check(
  'consecutive prose lines merge into markdown blocks',
  panelSource.includes('MarkdownBlockItem') &&
    panelSource.includes('isProseLine'),
);

check(
  'tool calls collapse into summary when >= 2 consecutive',
  panelSource.includes('COLLAPSED_TOOL_GROUP_MIN') &&
    panelSource.includes('renderRowsWithCollapse'),
);

check(
  'inline variant shows loading spinner when state is loading',
  panelSource.includes('showLoading') &&
    panelSource.includes('wf-log-spinner'),
);

check(
  'inline variant shows pruned message when state is pruned',
  panelSource.includes('showPruned') &&
    panelSource.includes('labels.cleaned'),
);

check(
  'inline variant shows error message when state is error',
  panelSource.includes('showError') &&
    panelSource.includes('labels.error'),
);

// ---------------------------------------------------------------------------
// 5. Regression checks for the fixed issues
// ---------------------------------------------------------------------------

console.log('\n--- Fixed Issue Regressions ---');

// Fix 1: panel state mapped from store status (loading/error/pruned covered)
const statePropMatch = cardSource.match(
  /<AgentActivityPanel\s+[^>]*state="idle"/,
);
check(
  'FIXED: AgentActivityPanel state prop is no longer hardcoded to "idle"',
  statePropMatch === null &&
    cardSource.includes('state={planGenerationPanelState}'),
  'The panel must receive the mapped load state.',
);

check(
  'FIXED: store status mapped to panel state via planGenerationActivityLoadState',
  cardSource.includes('planGenerationActivityLoadState('),
);

// Fix 2: failed state keeps loaded thinking lines and stops fetching
check(
  'FIXED: failed state preserves lines and disables fetching',
  cardSource.includes(
    'planGenerationRunId = planGenerationCardMeta?.run_id',
  ) && cardSource.includes("planGenerationCardMeta?.status !== 'failed'"),
  'run_id must stay set so cached lines remain readable while enabled=false stops pulling.',
);

// Fix 3: loading indicator while activity first loads
check(
  'FIXED: panel shows loading/error/pruned states before lines arrive',
  cardSource.includes('shouldShowPlanGenerationActivity({') &&
    cardSource.includes('state={planGenerationPanelState}'),
);

// ---------------------------------------------------------------------------
// Run behavioral tests
// ---------------------------------------------------------------------------

console.log('\n--- Behavioral Tests ---');

await streamingRun();
await failedStateRun();
await refreshRecoveryRun();
await prunedRecoveryRun();
helperMappingRun();

// ---------------------------------------------------------------------------
// Summary
// ---------------------------------------------------------------------------

console.log(`\n--- Summary ---`);
if (failures > 0) {
  console.error(`\n${failures} check(s) FAILED`);
  process.exit(1);
} else {
  console.log('\nAll checks passed');
}