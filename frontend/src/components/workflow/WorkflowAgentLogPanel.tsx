import { useMemo } from 'react';
import { ChevronRight } from 'lucide-react';
import { AgentActivityPanel } from '@/components/AgentActivityPanel';
import {
  parseToolActivityContent,
  type AgentActivityTranslator,
} from '@/lib/agentActivityFormatter';
import type { ChatRunActivityLine, ChatRunActivityLineType } from '@/types';
import './WorkflowAgentLogPanel.css';

type AgentLogLine = {
  key: string;
  /** Raw ISO `created_at` of the transcript entry (drives elapsed time). */
  timestamp: string;
  content: string;
  entryType?: string;
  /** Workflow agent session id; each run of an agent gets its own session. */
  runId?: string | null;
};

type AgentLogGroup = {
  key: string;
  agentName: string;
  lines: AgentLogLine[];
};

type AgentLogRun = {
  runId: string;
  lines: AgentLogLine[];
};

const workflowLineTypeForLogLine = (
  line: AgentLogLine
): ChatRunActivityLineType => {
  if (line.entryType === 'error') return 'error';
  if (parseToolActivityContent(line.content)) return 'tool';
  return 'thinking';
};

const workflowLogLinesToActivityLines = (
  lines: AgentLogLine[],
  group: AgentLogGroup
): ChatRunActivityLine[] =>
  lines.map((line, index) => {
    const lineType = workflowLineTypeForLogLine(line);

    return {
      line_id: line.key,
      run_id: `workflow-log-${group.key}`,
      session_id: 'workflow',
      session_agent_id: group.key,
      agent_id: group.key,
      agent_name: group.agentName,
      sequence: index,
      line_type: lineType,
      stream_type: lineType === 'error' ? 'error' : 'thinking',
      content: line.content,
      created_at: line.timestamp,
    };
  });

/** Entries arrive chronologically, so first-seen run order is run order. */
const splitLogLinesIntoRuns = (lines: AgentLogLine[]): AgentLogRun[] => {
  const runsById = new Map<string, AgentLogRun>();
  for (const line of lines) {
    const runId = line.runId?.trim() || 'default';
    const existing = runsById.get(runId);
    if (existing) {
      existing.lines.push(line);
    } else {
      runsById.set(runId, { runId, lines: [line] });
    }
  }
  return Array.from(runsById.values());
};

export type WorkflowAgentLogPanelProps = {
  agentLogGroups: AgentLogGroup[];
  isLoading: boolean;
  emptyMessage?: string;
  loadingMessage?: string;
  stepStatus?: string;
  translate?: AgentActivityTranslator;
};

type PanelLabels = {
  loading: string;
  cleaned: string;
  error: string;
  empty: string;
};

function AgentLogGroupView({
  group,
  labels,
  translate,
}: {
  group: AgentLogGroup;
  labels: PanelLabels;
  translate?: AgentActivityTranslator;
}) {
  const runs = useMemo(() => splitLogLinesIntoRuns(group.lines), [group.lines]);
  const latestRun = runs[runs.length - 1];
  const previousRuns = runs.slice(0, -1);
  const latestLines = useMemo(
    () => workflowLogLinesToActivityLines(latestRun?.lines ?? [], group),
    [latestRun, group]
  );

  const previousRunLabel = (index: number): string => {
    const key = 'agentActivity.runLabel';
    const translated = translate?.(key, { count: index });
    if (translated && translated !== key) return translated;
    return `Run ${index}`;
  };

  return (
    <div className="wf-log-pane-body">
      {previousRuns.map((run, index) => (
        <details key={run.runId} className="wf-log-run-disclosure">
          <summary className="wf-log-run-summary">
            <span className="wf-log-task-status">
              <ChevronRight className="wf-log-task-chevron" />
            </span>
            <span className="wf-log-collapsed-label">
              {previousRunLabel(index + 1)}
            </span>
          </summary>
          <AgentActivityPanel
            lines={workflowLogLinesToActivityLines(run.lines, group)}
            state="loaded"
            labels={labels}
            translate={translate}
            variant="panel"
            stripEmptyHtmlCommentPrefixes
          />
        </details>
      ))}
      <div className="wf-log-pane-latest">
        <AgentActivityPanel
          lines={latestLines}
          state="loaded"
          labels={labels}
          translate={translate}
          variant="panel"
          stripEmptyHtmlCommentPrefixes
        />
      </div>
    </div>
  );
}

export function WorkflowAgentLogPanel({
  agentLogGroups,
  isLoading,
  emptyMessage = 'No logs for this step yet.',
  loadingMessage = 'Loading logs...',
  translate,
}: WorkflowAgentLogPanelProps) {
  const labels: PanelLabels = {
    loading: loadingMessage,
    cleaned: emptyMessage,
    error: emptyMessage,
    empty: emptyMessage,
  };

  if (isLoading || agentLogGroups.length === 0) {
    return (
      <AgentActivityPanel
        lines={[]}
        state={isLoading ? 'loading' : 'loaded'}
        labels={labels}
        translate={translate}
        variant="panel"
        stripEmptyHtmlCommentPrefixes
      />
    );
  }

  return (
    <div className="wf-log-split">
      {agentLogGroups.map((group) => (
        <div key={group.key} className="wf-log-pane">
          <AgentLogGroupView
            group={group}
            labels={labels}
            translate={translate}
          />
        </div>
      ))}
    </div>
  );
}
