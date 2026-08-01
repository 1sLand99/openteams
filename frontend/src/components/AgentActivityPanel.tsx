import React, {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
} from "react";
import {
  Activity,
  Ban,
  Bot,
  Check,
  ChevronRight,
  ClipboardList,
  FilePenLine,
  FileText,
  Globe,
  ListChecks,
  Loader2,
  Search,
  Terminal,
  TimerOff,
  Wrench,
  XCircle,
} from "lucide-react";
import { ScrollArea } from "@/components/ScrollArea";
import {
  formatAgentActivityLines,
  isThinkingHeaderContent,
  type AgentActivityDisplayRow,
  type AgentActivityToolKind,
  type AgentActivityToolStatus,
  type AgentActivityTranslator,
} from "@/lib/agentActivityFormatter";
import type { ActivityLoadState, ChatRunActivityLine } from "@/types";
import "@/components/workflow/WorkflowAgentLogPanel.css";

interface AgentActivityPanelLabels {
  loading: string;
  cleaned: string;
  error: string;
  empty: string;
}

interface AgentActivityPanelProps {
  lines?: ChatRunActivityLine[];
  state?: ActivityLoadState;
  labels: AgentActivityPanelLabels;
  translate?: AgentActivityTranslator;
  variant?: "inline" | "panel";
  stripEmptyHtmlCommentPrefixes?: boolean;
}

const AGENT_ACTIVITY_AUTO_SCROLL_IDLE_MS = 30000;
const AGENT_ACTIVITY_BOTTOM_THRESHOLD_PX = 8;

/**
 * Runs of at least this many consecutive tool calls collapse into a single
 * summary row; single calls stay inline.
 */
const COLLAPSED_TOOL_GROUP_MIN = 2;

/** Display order for tool kinds inside a collapsed-group summary. */
const GROUP_KIND_ORDER: AgentActivityToolKind[] = [
  "file_edit",
  "file_read",
  "command",
  "search",
  "web_fetch",
  "mcp_tool",
  "tool",
  "task",
  "plan",
  "activity",
];

const DEFAULT_GROUP_KIND_LABELS: Record<
  AgentActivityToolKind,
  { one: (count: number) => string; many: (count: number) => string }
> = {
  file_edit: {
    one: () => "Edited a file",
    many: (count) => `Edited ${count} files`,
  },
  file_read: {
    one: () => "Read a file",
    many: (count) => `Read ${count} files`,
  },
  command: {
    one: () => "Ran a command",
    many: (count) => `Ran ${count} commands`,
  },
  search: {
    one: () => "Searched",
    many: (count) => `Searched ${count} times`,
  },
  web_fetch: {
    one: () => "Fetched a page",
    many: (count) => `Fetched ${count} pages`,
  },
  mcp_tool: {
    one: () => "Called an MCP tool",
    many: (count) => `Made ${count} MCP calls`,
  },
  tool: {
    one: () => "Called a tool",
    many: (count) => `Made ${count} tool calls`,
  },
  task: {
    one: () => "Started a subtask",
    many: (count) => `Started ${count} subtasks`,
  },
  plan: {
    one: () => "Updated the plan",
    many: (count) => `Updated the plan ${count} times`,
  },
  activity: {
    one: () => "Performed an action",
    many: (count) => `Performed ${count} actions`,
  },
};

/** Details longer than this are truncated on screen and expandable on click. */
const TOOL_DETAIL_EXPAND_THRESHOLD = 72;

/**
 * Durations below this are almost always artifacts of batched started/
 * completed lines rather than real elapsed time, so they stay hidden.
 */
const MIN_VISIBLE_DURATION_MS = 1000;

const toolIconByKind: Record<
  AgentActivityToolKind,
  React.ComponentType<{ className?: string }>
> = {
  command: Terminal,
  file_read: FileText,
  file_edit: FilePenLine,
  search: Search,
  web_fetch: Globe,
  tool: Wrench,
  mcp_tool: Wrench,
  task: ListChecks,
  plan: ClipboardList,
  activity: Activity,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Tool-call types that should be hidden while running */
const TOOL_CALL_KINDS = new Set<AgentActivityToolKind>([
  "command",
  "file_read",
  "file_edit",
  "search",
  "web_fetch",
  "tool",
  "mcp_tool",
]);

function isToolCallLine(line: AgentActivityDisplayRow): boolean {
  return line.line_type === "tool" && !!line.toolKind && TOOL_CALL_KINDS.has(line.toolKind);
}

function isToolRunning(line: AgentActivityDisplayRow): boolean {
  return line.toolStatus === "running" || line.toolStatus === "waiting_approval";
}

const panelGroupKeyForLine = (line: AgentActivityDisplayRow): string =>
  line.agentName.trim() || "Agent";

const renderSimpleBoldMarkdown = (content: string): React.ReactNode => {
  const parts: React.ReactNode[] = [];
  let cursor = 0;
  let partIndex = 0;

  while (cursor < content.length) {
    const start = content.indexOf("**", cursor);
    if (start < 0) {
      parts.push(content.slice(cursor));
      break;
    }

    const end = content.indexOf("**", start + 2);
    if (end < 0) {
      parts.push(content.slice(cursor));
      break;
    }

    if (start > cursor) {
      parts.push(content.slice(cursor, start));
    }

    const boldText = content.slice(start + 2, end);
    parts.push(
      boldText ? (
        <strong key={`bold-${partIndex}`} className="font-semibold">
          {boldText}
        </strong>
      ) : (
        "**"
      ),
    );
    partIndex += 1;
    cursor = end + 2;
  }

  return parts.length > 0 ? parts : content;
};

// ---------------------------------------------------------------------------
// Auto-scroll hook
// ---------------------------------------------------------------------------

const isScrolledToBottom = (el: HTMLElement): boolean =>
  el.scrollHeight - el.scrollTop - el.clientHeight <=
  AGENT_ACTIVITY_BOTTOM_THRESHOLD_PX;

const useAutoFollowScroll = (scrollSignal: string) => {
  const scrollRef = useRef<HTMLDivElement>(null);
  const autoFollowRef = useRef(true);
  const userInteractingRef = useRef(false);
  const ignoreScrollRef = useRef(false);
  const resumeTimerRef = useRef<number | undefined>(undefined);

  const clearResumeTimer = useCallback(() => {
    if (resumeTimerRef.current === undefined) return;
    window.clearTimeout(resumeTimerRef.current);
    resumeTimerRef.current = undefined;
  }, []);

  const scrollToBottom = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    ignoreScrollRef.current = true;
    el.scrollTop = el.scrollHeight;
    window.requestAnimationFrame(() => {
      ignoreScrollRef.current = false;
    });
  }, []);

  const resumeAutoFollow = useCallback(() => {
    autoFollowRef.current = true;
    userInteractingRef.current = false;
    scrollToBottom();
  }, [scrollToBottom]);

  const scheduleResume = useCallback(() => {
    clearResumeTimer();
    resumeTimerRef.current = window.setTimeout(
      resumeAutoFollow,
      AGENT_ACTIVITY_AUTO_SCROLL_IDLE_MS,
    );
  }, [clearResumeTimer, resumeAutoFollow]);

  const noteUserInteraction = useCallback(() => {
    userInteractingRef.current = true;
    scheduleResume();
  }, [scheduleResume]);

  const handleScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el || ignoreScrollRef.current) return;

    if (isScrolledToBottom(el)) {
      autoFollowRef.current = true;
      userInteractingRef.current = false;
      clearResumeTimer();
      return;
    }

    if (userInteractingRef.current) {
      autoFollowRef.current = false;
      scheduleResume();
    }
  }, [clearResumeTimer, scheduleResume]);

  useLayoutEffect(() => {
    if (autoFollowRef.current) {
      scrollToBottom();
    }
  }, [scrollSignal, scrollToBottom]);

  useEffect(() => clearResumeTimer, [clearResumeTimer]);

  return {
    scrollRef,
    scrollHandlers: {
      onKeyDown: noteUserInteraction,
      onPointerDown: noteUserInteraction,
      onScroll: handleScroll,
      onTouchStart: noteUserInteraction,
      onWheel: noteUserInteraction,
    },
  };
};

// ---------------------------------------------------------------------------
// LineItem — Linear-style minimal row
// ---------------------------------------------------------------------------

const statusIconByStatus: Partial<
  Record<AgentActivityToolStatus, React.ComponentType<{ className?: string }>>
> = {
  completed: Check,
  failed: XCircle,
  denied: Ban,
  timed_out: TimerOff,
  running: Loader2,
  waiting_approval: Loader2,
};

const formatDurationMs = (ms: number): string => {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1000;
  if (seconds < 10) return `${seconds.toFixed(1)}s`;
  if (seconds < 60) return `${Math.round(seconds)}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${Math.round(seconds % 60)}s`;
};

const ToolLineItem: React.FC<{
  line: AgentActivityDisplayRow;
}> = ({ line }) => {
  const ToolIcon = line.toolKind ? toolIconByKind[line.toolKind] : Wrench;
  const status = line.toolStatus;
  const StatusIcon = status ? statusIconByStatus[status] : undefined;
  const hasLongDetail =
    (line.detail?.length ?? 0) > TOOL_DETAIL_EXPAND_THRESHOLD;
  const expandable = Boolean(line.resultDetail) || hasLongDetail;
  // Successful rows rely on the status icon alone; only failures and other
  // noteworthy statuses keep the text label.
  const showLabel = Boolean(line.title) && status !== "completed";
  const rowClass = `wf-log-task-row${status ? ` wf-log-task-row--${status}` : ""}`;

  const row = (
    <div className={rowClass}>
      <span className="wf-log-task-status">
        {StatusIcon ? (
          <StatusIcon
            className={`wf-log-task-status-icon wf-log-task-status-icon--${status}`}
          />
        ) : null}
      </span>
      <span className="wf-log-task-tool-icon">
        <ToolIcon className="w-3 h-3" />
      </span>
      {showLabel && <span className="wf-log-task-label">{line.title}</span>}
      {line.detail && (
        <span className="wf-log-task-target" title={line.detail}>
          {line.detail}
        </span>
      )}
      {typeof line.durationMs === "number" &&
        line.durationMs >= MIN_VISIBLE_DURATION_MS && (
          <span className="wf-log-task-duration">
            {formatDurationMs(line.durationMs)}
          </span>
        )}
      {expandable && (
        <ChevronRight className="wf-log-task-chevron wf-log-task-chevron--end" />
      )}
    </div>
  );

  if (!expandable) return row;

  return (
    <details className="wf-log-task-disclosure">
      <summary>{row}</summary>
      {hasLongDetail && line.detail && (
        <pre className="wf-log-task-result">{line.detail}</pre>
      )}
      {line.resultDetail && (
        <pre className="wf-log-task-result">{line.resultDetail}</pre>
      )}
    </details>
  );
};

/**
 * Only fully-bold one-line summaries (Codex-style `**Planning …**`) are
 * promoted to section headers. Agents like Claude stream prose thinking
 * split across many lines; treating each line as a header would fragment
 * the log, so those stay regular body text.
 */
const ContentLineItem: React.FC<{
  line: AgentActivityDisplayRow;
}> = ({ line }) => {
  // Header thinking lines act as section titles: everything below them until
  // the next header reads as one step of the agent's work.
  const isThinkingHeader =
    line.line_type === "thinking" && isThinkingHeaderContent(line.content);
  return (
    <div
      className={`wf-log-task-row wf-log-task-row--content${
        isThinkingHeader ? " wf-log-task-row--thinking" : ""
      }`}
    >
      <span className="wf-log-task-status" />
      <span
        className={
          isThinkingHeader ? "wf-log-thinking-text" : "wf-log-task-content-text"
        }
      >
        {renderSimpleBoldMarkdown(line.content)}
      </span>
    </div>
  );
};

const ErrorLineItem: React.FC<{
  line: AgentActivityDisplayRow;
}> = ({ line }) => {
  const content = line.content.trim();
  const [preview = content, ...detailLines] = content.split(/\r\n|\r|\n/u);
  const detail = detailLines.join("\n").trim();

  if (!detail) {
    return (
      <div className="wf-log-error-block wf-log-error-block--single">
        <span className="wf-log-error-status" aria-hidden="true" />
        <span className="wf-log-task-tool-icon wf-log-error-tool-icon">
          <Terminal className="w-3 h-3" aria-hidden="true" />
        </span>
        <pre className="wf-log-error-preview wf-log-error-preview--single">
          {preview}
        </pre>
      </div>
    );
  }

  return (
    <details className="wf-log-error-block">
      <summary className="wf-log-error-block-summary">
        <ChevronRight className="wf-log-error-chevron" aria-hidden="true" />
        <span className="wf-log-task-tool-icon wf-log-error-tool-icon">
          <Terminal className="w-3 h-3" aria-hidden="true" />
        </span>
        <code className="wf-log-error-preview" title={preview}>
          {preview}
        </code>
      </summary>
      <pre className="wf-log-error-detail">{detail}</pre>
    </details>
  );
};

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

export const AgentActivityPanel: React.FC<AgentActivityPanelProps> = ({
  lines = [],
  state = "idle",
  labels,
  translate,
  variant = "inline",
  stripEmptyHtmlCommentPrefixes = false,
}) => {
  const displayRows = useMemo(
    () =>
      formatAgentActivityLines(lines, translate, {
        stripEmptyHtmlCommentPrefixes,
      }),
    [lines, stripEmptyHtmlCommentPrefixes, translate],
  );

  // Filter: hide tool calls that are still running
  const visibleRows = useMemo(
    () =>
      displayRows.filter(
        (line) => !(isToolCallLine(line) && isToolRunning(line)),
      ),
    [displayRows],
  );
  const panelRowGroups = useMemo(() => {
    const groups: Array<{
      key: string;
      agentName: string;
      rows: AgentActivityDisplayRow[];
    }> = [];

    for (const line of visibleRows) {
      const agentName = panelGroupKeyForLine(line);
      const lastGroup = groups[groups.length - 1];
      if (lastGroup?.agentName === agentName) {
        lastGroup.rows.push(line);
        continue;
      }

      groups.push({
        key: `${agentName}-${groups.length}`,
        agentName,
        rows: [line],
      });
    }

    return groups;
  }, [visibleRows]);

  const lastDisplayRow = visibleRows[visibleRows.length - 1];
  const scrollSignal = `${visibleRows.length}:${lastDisplayRow?.row_id ?? ""}:${
    lastDisplayRow?.content.length ?? 0
  }`;
  const { scrollRef, scrollHandlers } = useAutoFollowScroll(scrollSignal);
  const showLoading = state === "loading";
  const showPruned = state === "pruned";
  const showError = state === "error";
  const showEmpty =
    !showLoading && !showPruned && !showError && visibleRows.length === 0;

  const summarizeToolGroup = (rows: AgentActivityDisplayRow[]) => {
    const counts = new Map<AgentActivityToolKind, number>();
    let failures = 0;
    for (const row of rows) {
      const kind = row.toolKind ?? "activity";
      counts.set(kind, (counts.get(kind) ?? 0) + 1);
      if (
        row.toolStatus === "failed" ||
        row.toolStatus === "denied" ||
        row.toolStatus === "timed_out"
      ) {
        failures += 1;
      }
    }
    return { counts, failures };
  };

  const groupKindLabel = (
    kind: AgentActivityToolKind,
    count: number,
  ): string => {
    const key = `agentActivity.group.${kind}.${count === 1 ? "one" : "many"}`;
    const translated = translate?.(key, { count });
    if (translated && translated !== key) return translated;
    return DEFAULT_GROUP_KIND_LABELS[kind][count === 1 ? "one" : "many"](
      count,
    );
  };

  const groupFailuresLabel = (count: number): string => {
    const key = "agentActivity.group.failures";
    const translated = translate?.(key, { count });
    if (translated && translated !== key) return translated;
    return `${count} failed`;
  };

  const renderLine = (line: AgentActivityDisplayRow) =>
    isToolCallLine(line) ? (
      <ToolLineItem
        key={line.row_id}
        line={line}
      />
    ) : line.line_type === "error" ? (
      <ErrorLineItem
        key={line.row_id}
        line={line}
      />
    ) : (
      <ContentLineItem
        key={line.row_id}
        line={line}
      />
    );

  // Consecutive tool calls collapse into a single Codex-style summary row
  // ("Edited 2 files · Ran 3 commands"); content rows break a run.
  const renderRowsWithCollapse = (rows: AgentActivityDisplayRow[]) => {
    const nodes: React.ReactNode[] = [];
    let index = 0;
    while (index < rows.length) {
      const line = rows[index];
      if (!isToolCallLine(line)) {
        nodes.push(renderLine(line));
        index += 1;
        continue;
      }

      let end = index;
      while (end < rows.length && isToolCallLine(rows[end])) end += 1;
      const group = rows.slice(index, end);
      if (group.length < COLLAPSED_TOOL_GROUP_MIN) {
        group.forEach((entry) => nodes.push(renderLine(entry)));
      } else {
        const { counts, failures } = summarizeToolGroup(group);
        const summaryLabel = GROUP_KIND_ORDER.filter((kind) =>
          counts.has(kind),
        )
          .map((kind) => groupKindLabel(kind, counts.get(kind) ?? 0))
          .join(" · ");
        nodes.push(
          <details
            key={`collapsed-${group[0]?.row_id ?? index}`}
            className="wf-log-collapsed-group"
          >
            <summary className="wf-log-collapsed-summary">
              <span className="wf-log-task-status">
                <ChevronRight className="wf-log-task-chevron" />
              </span>
              <span className="wf-log-collapsed-label">{summaryLabel}</span>
              {failures > 0 && (
                <span className="wf-log-collapsed-failures">
                  {groupFailuresLabel(failures)}
                </span>
              )}
            </summary>
            {group.map(renderLine)}
          </details>,
        );
      }
      index = end;
    }
    return nodes;
  };

  if (showEmpty && variant === "inline") return null;

  if (variant === "panel") {
    if (showLoading) {
      return (
        <div className="wf-log-panel wf-log-panel--empty">
          <span className="wf-log-spinner" />
          <span className="wf-log-panel-message">{labels.loading}</span>
        </div>
      );
    }

    if (showPruned) {
      return (
        <div className="wf-log-panel wf-log-panel--empty">
          <span className="wf-log-panel-message">{labels.cleaned}</span>
        </div>
      );
    }

    if (showError) {
      return (
        <div className="wf-log-panel wf-log-panel--empty">
          <span className="wf-log-panel-message wf-log-panel-message--error">
            {labels.error}
          </span>
        </div>
      );
    }

    if (showEmpty) {
      return (
        <div className="wf-log-panel wf-log-panel--empty">
          <span className="wf-log-panel-message">{labels.empty}</span>
        </div>
      );
    }

    return (
      <div
        ref={scrollRef}
        className="wf-log-panel"
        {...scrollHandlers}
      >
        {panelRowGroups.map((group) => (
          <div
            key={group.key}
            className="wf-log-group"
          >
            <div className="wf-log-group-header">
              <Bot
                className="wf-log-group-agent-icon"
                aria-hidden="true"
              />
              <span className="wf-log-group-agent">{group.agentName}</span>
            </div>
            <div className="wf-log-group-tasks">
              {renderRowsWithCollapse(group.rows)}
            </div>
          </div>
        ))}
      </div>
    );
  }

  return (
    <div className="wf-log-panel-inline">
      {showLoading ? (
        <div className="wf-log-panel wf-log-panel--empty" style={{ height: "auto", padding: "8px 0" }}>
          <span className="wf-log-spinner" />
          <span className="wf-log-panel-message">{labels.loading}</span>
        </div>
      ) : showPruned ? (
        <div className="wf-log-panel-message" style={{ padding: "4px 0" }}>
          {labels.cleaned}
        </div>
      ) : showError ? (
        <div className="wf-log-panel-message" style={{ padding: "4px 0", color: "#e5484d" }}>
          {labels.error}
        </div>
      ) : (
        <ScrollArea
          ref={scrollRef}
          className="agent-activity-scrollbar max-h-[480px] pr-1"
          scrollbar="styled"
          {...scrollHandlers}
        >
          <div className="wf-log-group-tasks">
            {renderRowsWithCollapse(visibleRows)}
          </div>
        </ScrollArea>
      )}
    </div>
  );
};
