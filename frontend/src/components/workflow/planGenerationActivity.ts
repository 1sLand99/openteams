import type { RunActivityStatus } from '@/lib/runActivityStore';
import type { ActivityLoadState } from '@/types';

/**
 * Maps the run-activity store status onto the AgentActivityPanel load state
 * so loading / error / pruned are surfaced instead of being flattened to idle.
 */
export const planGenerationActivityLoadState = (
  status: RunActivityStatus,
): ActivityLoadState => {
  switch (status) {
    case 'pruned':
      return 'pruned';
    case 'error':
      return 'error';
    case 'loading':
      return 'loading';
    case 'live':
    case 'completed':
      return 'loaded';
    default:
      return 'idle';
  }
};

/**
 * The plan-generation activity panel renders when there is something to show:
 * already-loaded thinking lines (kept visible after a failure), or an
 * explicit panel state (loading spinner, error, pruned notice).
 */
export const shouldShowPlanGenerationActivity = (args: {
  runId: string | undefined;
  lineCount: number;
  loadState: ActivityLoadState;
}): boolean => {
  if (!args.runId) return false;
  if (args.lineCount > 0) return true;
  return (
    args.loadState === 'loading' ||
    args.loadState === 'error' ||
    args.loadState === 'pruned'
  );
};
