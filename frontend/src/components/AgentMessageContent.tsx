import React, { useEffect, useMemo, useRef, useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { ThinkingOrb } from "thinking-orbs";
import { AgentActivityPanel } from "@/components/AgentActivityPanel";
import { AgentArtifactFileList } from "@/components/AgentArtifactFileList";
import { AgentMarkdown } from "@/components/AgentMarkdown";
import { AgentRunStatusPill } from "@/components/AgentRunStatusPill";
import { WorkflowCard } from "@/components/workflow/WorkflowCard";
import { chatRunsApi } from "@/lib/api";
import {
  flattenRunFileChanges,
  type AgentFileRow,
} from "@/lib/agentFileRows";
import type { ActivityLoadState, Message } from "@/types";
import { useRunActivity } from "@/context/RunActivityContext";

/**
 * Module-level cache of per-run changed-file rows keyed by session id + run id.
 * A run's captured diff is immutable once the run completes, so the rows are
 * safe to reuse across message re-renders and avoid refetching on every scroll.
 */
interface RunFileRowsCacheEntry {
  rows: AgentFileRow[];
}

const runFileRowsCache = new Map<string, RunFileRowsCacheEntry>();
const runFileRowsPending = new Map<string, Promise<RunFileRowsCacheEntry>>();

const runFileRowsCacheKey = (
  sessionId: string | undefined,
  runId: string | undefined,
): string | null => (runId ? `${sessionId ?? "unknown"}:${runId}` : null);

/**
 * Activity pulses inside this window drive the thinking-orb speed: more log
 * lines per second → faster animation (clamped to [1, 3]).
 */
const ORB_SPEED_WINDOW_MS = 5000;
const ORB_SPEED_MIN = 1;
const ORB_SPEED_MAX = 3;

/**
 * thinking-orbs ships no error-state variant, so a failed run keeps the orb
 * frozen and tints it red with a CSS filter (works for both light- and
 * dark-theme ink).
 */
const ORB_ERROR_STYLE: React.CSSProperties = {
  filter:
    "invert(32%) sepia(83%) saturate(3200%) hue-rotate(338deg) brightness(96%)",
};

interface AgentMessageContentProps {
  message: Message;
  t: (key: string, replacements?: Record<string, string | number>) => string;
  messageFontSize?: number;
  /** Open a file row (e.g. into a diff tab). */
  onOpenArtifact?: (file: AgentFileRow) => void;
}

export const AgentMessageContent: React.FC<AgentMessageContentProps> = ({
  message,
  t,
  messageFontSize,
  onOpenArtifact,
}) => {
  const isRunning = Boolean(message.isAgentRunning || message.isThinking);
  const [expanded, setExpanded] = useState(isRunning);
  const wasRunningRef = useRef(isRunning);
  const activity = useRunActivity(message.runId, {
    enabled: Boolean(message.runId) && (isRunning || expanded),
  });

  useEffect(() => {
    if (isRunning) {
      setExpanded(true);
      wasRunningRef.current = true;
      return;
    }

    if (wasRunningRef.current) {
      setExpanded(false);
      wasRunningRef.current = false;
    }
  }, [isRunning]);

  const visibleActivityLines = useMemo(
    () =>
      isRunning
        ? activity.lines
        : activity.lines.filter((line) => line.line_type !== "assistant"),
    [activity.lines, isRunning],
  );
  const loadState: ActivityLoadState =
    activity.status === "pruned"
      ? "pruned"
      : activity.status === "error"
        ? "error"
        : activity.status === "loading"
          ? "idle"
          : activity.status === "idle"
            ? "idle"
            : "loaded";
  const hasVisibleActivityLines = visibleActivityLines.length > 0;
  const hasActivityPanelState =
    loadState === "pruned" || loadState === "error";
  // Stderr noise from a healthy run is also logged as error lines, so only a
  // run that *ends* on an error line (or fails to load) counts as failed.
  const lastActivityLineType =
    activity.lines[activity.lines.length - 1]?.line_type;
  const hasActivityError =
    loadState === "error" || lastActivityLineType === "error";

  // ---- Thinking-orb speed ----------------------------------------------------
  // Each activity update (new line or streamed content growth) counts as one
  // pulse; a 1s ticker turns the recent pulse rate into a clamped speed
  // multiplier for the running orb. The idle orb stays frozen (`paused`).
  const activityPulseRef = useRef<number[]>([]);
  const [orbSpeed, setOrbSpeed] = useState(ORB_SPEED_MIN);
  const lastActivityLine = activity.lines[activity.lines.length - 1];
  const activitySignature = `${activity.lines.length}:${lastActivityLine?.content.length ?? 0}`;

  useEffect(() => {
    if (!isRunning) return;
    activityPulseRef.current.push(Date.now());
  }, [activitySignature, isRunning]);

  useEffect(() => {
    if (!isRunning) {
      activityPulseRef.current = [];
      setOrbSpeed(ORB_SPEED_MIN);
      return;
    }
    const updateOrbSpeed = () => {
      const now = Date.now();
      const pulses = activityPulseRef.current.filter(
        (ts) => now - ts <= ORB_SPEED_WINDOW_MS,
      );
      activityPulseRef.current = pulses;
      const rate = pulses.length / (ORB_SPEED_WINDOW_MS / 1000);
      const next = Math.min(
        ORB_SPEED_MAX,
        Math.max(ORB_SPEED_MIN, ORB_SPEED_MIN + rate * 0.5),
      );
      setOrbSpeed((current) =>
        Math.abs(current - next) >= 0.25 ? next : current,
      );
    };
    updateOrbSpeed();
    const timer = window.setInterval(updateOrbSpeed, 1000);
    return () => window.clearInterval(timer);
  }, [isRunning]);

  // ---- Per-run changed files ------------------------------------------------
  // The file list pinned to the message bottom is sourced from the run's own
  // file capture. Diff bodies are loaded lazily when a row is opened.
  const cacheKey = runFileRowsCacheKey(message.sessionId, message.runId);
  const [runFileRows, setRunFileRows] = useState<AgentFileRow[]>(() =>
    cacheKey ? (runFileRowsCache.get(cacheKey)?.rows ?? []) : [],
  );

  useEffect(() => {
    const nextCacheKey = runFileRowsCacheKey(message.sessionId, message.runId);
    if (isRunning || !message.runId || !nextCacheKey) return;
    const runId = message.runId;
    const cached = runFileRowsCache.get(nextCacheKey);
    if (cached) {
      setRunFileRows(cached.rows);
      return;
    }

    let cancelled = false;
    let pending = runFileRowsPending.get(nextCacheKey);
    if (!pending) {
      pending = chatRunsApi
        .getFiles(runId, { includeDiff: false })
        .then((response) => {
          const rows = flattenRunFileChanges(response);
          const entry = {
            rows,
          };
          runFileRowsCache.set(nextCacheKey, entry);
          return entry;
        })
        .finally(() => {
          runFileRowsPending.delete(nextCacheKey);
        });
      runFileRowsPending.set(nextCacheKey, pending);
    }

    pending
      .then((entry) => {
        if (cancelled) return;
        setRunFileRows(entry.rows);
      })
      .catch(() => {
        if (cancelled) return;
        setRunFileRows([]);
      });
    return () => {
      cancelled = true;
    };
  }, [message.sessionId, message.runId, isRunning]);

  const fileRows = runFileRows;

  const panelLabels = {
    loading: t("agentActivity.loading"),
    cleaned: t("agentActivity.cleaned"),
    error: t("agentActivity.error"),
    empty: t("agentActivity.empty"),
  };

  const showActivityPanel =
    (isRunning || expanded) &&
    (hasVisibleActivityLines || hasActivityPanelState);

  const hasFileRows = !isRunning && fileRows.length > 0;
  const hasWorkflowCard = Boolean(message.workflowCard && message.sessionId);
  const translatedMessageText = useMemo(() => {
    if (!message.i18nKey) return undefined;
    const translated = t(message.i18nKey, message.i18nParams);
    return translated && translated !== message.i18nKey
      ? translated
      : undefined;
  }, [message.i18nKey, message.i18nParams, t]);
  const visibleMessageText = translatedMessageText ?? message.text;
  // Structured agent replies carry a derived body (send text, or the
  // conclusion when there is no send). Plain replies leave `replyText`
  // undefined and we fall back to the raw/i18n-localized text.
  const replyText = message.replyText ?? visibleMessageText;
  const hasReplyText = replyText.trim().length > 0;
  const displayReplyText =
    hasReplyText || isRunning || hasFileRows || hasWorkflowCard
      ? replyText
      : t("agent.runCompletedNoOutput");

  return (
    <div className="min-w-0 max-w-full space-y-2">
      {isRunning && (
        <AgentRunStatusPill
          speed={orbSpeed}
          label={t(
            message.runId
              ? "agentActivity.running"
              : "agentActivity.starting",
          )}
        />
      )}

      {hasWorkflowCard && message.workflowCard && message.sessionId && (
        <WorkflowCard
          sessionId={message.sessionId}
          messageId={message.workflowCard.messageId}
          cardType={message.workflowCard.cardType}
          planGenerationMeta={message.workflowCard.planGeneration}
        />
      )}

      {message.runId && !isRunning && (
        <button
          type="button"
          onClick={() => setExpanded((current) => !current)}
          className="inline-flex items-center gap-1 rounded-sm px-1 py-0.5 text-[12px] text-[var(--ink-tertiary)] transition hover:bg-[var(--surface-1)] hover:text-[var(--ink-subtle)]"
          aria-expanded={expanded}
        >
          {expanded ? (
            <ChevronDown className="h-3.5 w-3.5 shrink-0" />
          ) : (
            <ChevronRight className="h-3.5 w-3.5 shrink-0" />
          )}
          <ThinkingOrb
            state="solving"
            size={20}
            paused
            className="shrink-0"
            style={hasActivityError ? ORB_ERROR_STYLE : undefined}
          />
          <span>{t("agentActivity.toggle")}</span>
        </button>
      )}

      {showActivityPanel && (
        <AgentActivityPanel
          lines={visibleActivityLines}
          state={loadState}
          labels={panelLabels}
          translate={t}
        />
      )}

      {displayReplyText.trim().length > 0 && (
        <AgentMarkdown content={displayReplyText} fontSize={messageFontSize} />
      )}

      {hasFileRows && (
        <AgentArtifactFileList
          files={fileRows}
          onOpen={onOpenArtifact ?? (() => undefined)}
          title={t("agentArtifacts.title")}
          moreLabel={(count) => t("agentArtifacts.more", { count })}
        />
      )}
    </div>
  );
};