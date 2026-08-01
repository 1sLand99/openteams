import { useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  Check,
  Copy,
  ExternalLink,
  RefreshCw,
  Terminal,
} from "lucide-react";
import type { AgentRuntimeStatus } from "@/types";
import { copyPlainText } from "@/lib/clipboard";
import {
  canOpenSystemTerminal,
  openSystemTerminal,
} from "@/lib/openSystemTerminal";
import { openExternalUrlInDesktop } from "@/lib/openExternalUrl";
import {
  detectClientPlatform,
  joinGuideCommands,
  resolveInstallGuide,
  type InstallGuideStep,
} from "./installGuidance";
import { getRunnerLabel } from "./agentRuntimeViewModel";

type TranslateFn = (
  key: string,
  replacements?: Record<string, string | number>,
) => string;

const cx = (...classes: Array<string | false | null | undefined>) =>
  classes.filter(Boolean).join(" ");

type TerminalFeedback = "copied" | "unavailable" | "copyFailed";

const actionButtonClass =
  "inline-flex h-7 items-center gap-1.5 rounded-full border px-3 text-[12px] font-medium transition focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-[var(--primary)]/40 disabled:cursor-not-allowed disabled:opacity-60";
const primaryActionClass =
  "border-[var(--primary)]/30 bg-[var(--primary-tint)] text-[var(--primary)] hover:bg-[var(--primary)]/20";
const ghostActionClass =
  "border-[var(--hairline)] bg-[var(--surface-2)] text-[var(--ink-muted)] hover:bg-[var(--surface-3)] hover:text-[var(--ink)]";

function GuideCommandBlock({
  step,
  copied,
  onCopy,
  copyLabel,
}: {
  step: InstallGuideStep;
  copied: boolean;
  onCopy: () => void;
  copyLabel: string;
}) {
  return (
    <div className="relative rounded-[6px] border border-[var(--mono-border)] bg-[var(--mono-bg)] px-2.5 py-2">
      {step.commands.map((command) => (
        <p
          key={command}
          className="break-all pr-6 font-mono text-[12px] leading-[1.6] text-[var(--ink-muted)]"
        >
          {command}
        </p>
      ))}
      <button
        type="button"
        onClick={onCopy}
        aria-label={copyLabel}
        title={copyLabel}
        className={cx(
          "absolute right-1.5 top-1.5 rounded-[5px] p-1 transition-colors",
          copied
            ? "text-[var(--success)]"
            : "text-[var(--ink-tertiary)] hover:bg-[var(--surface-3)] hover:text-[var(--ink)]",
        )}
      >
        {copied ? (
          <Check className="h-3.5 w-3.5" />
        ) : (
          <Copy className="h-3.5 w-3.5" />
        )}
      </button>
    </div>
  );
}

export function AgentInstallGuide({
  runner,
  rechecking,
  onRecheck,
  t,
}: {
  runner: AgentRuntimeStatus;
  rechecking: boolean;
  onRecheck: () => void;
  t: TranslateFn;
}) {
  const [platform] = useState(() => detectClientPlatform());
  const [desktopTerminalAvailable] = useState(() => canOpenSystemTerminal());
  const [copiedStep, setCopiedStep] = useState<number | "all" | null>(null);
  const [terminalFeedback, setTerminalFeedback] =
    useState<TerminalFeedback | null>(null);
  const [openingTerminal, setOpeningTerminal] = useState(false);
  const copiedTimerRef = useRef<number | null>(null);

  const guide = useMemo(
    () => resolveInstallGuide(runner, platform),
    [runner, platform],
  );
  const runnerLabel = getRunnerLabel(runner.runner_type);

  useEffect(() => {
    setCopiedStep(null);
    setTerminalFeedback(null);
  }, [runner.runner_type]);

  useEffect(
    () => () => {
      if (copiedTimerRef.current !== null) {
        window.clearTimeout(copiedTimerRef.current);
      }
    },
    [],
  );

  if (!guide) return null;

  const showAuthBanner =
    runner.installed && runner.auth_state !== "authenticated";
  const needsInstall = !runner.installed;
  const allCommandsText = joinGuideCommands(guide.steps, platform);

  const flashCopied = (key: number | "all") => {
    if (copiedTimerRef.current !== null) {
      window.clearTimeout(copiedTimerRef.current);
    }
    setCopiedStep(key);
    copiedTimerRef.current = window.setTimeout(() => {
      setCopiedStep(null);
      copiedTimerRef.current = null;
    }, 1500);
  };

  const handleCopyStep = async (step: InstallGuideStep, index: number) => {
    const copied = await copyPlainText(
      joinGuideCommands([step], platform),
    );
    if (copied) flashCopied(index);
  };

  const handleCopyAll = async () => {
    const copied = await copyPlainText(allCommandsText);
    if (copied) {
      flashCopied("all");
      setTerminalFeedback(null);
    } else {
      setTerminalFeedback("copyFailed");
    }
  };

  const handleOpenTerminal = async () => {
    if (openingTerminal) return;
    setOpeningTerminal(true);
    setTerminalFeedback(null);
    try {
      const copied = await copyPlainText(allCommandsText);
      const opened = await openSystemTerminal();
      if (!opened) {
        setTerminalFeedback("unavailable");
      } else if (!copied) {
        setTerminalFeedback("copyFailed");
      } else {
        setTerminalFeedback("copied");
      }
    } finally {
      setOpeningTerminal(false);
    }
  };

  const stepTitle = (step: InstallGuideStep): string => {
    switch (step.kind) {
      case "node":
        return t("agents.setup.step.node");
      case "install":
        return t("agents.setup.step.install", { agent: runnerLabel });
      case "auth":
        return t("agents.setup.step.auth", { agent: runnerLabel });
    }
  };

  return (
    <section className="rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-1)] p-4">
      <div className="mb-3 flex items-center justify-between gap-2">
        <h3 className="text-[11px] font-semibold uppercase tracking-[0.12em] text-[var(--ink-subtle)]">
          {t("agents.setup.title")}
        </h3>
        <a
          href={guide.entry.documentationUrl}
          target="_blank"
          rel="noreferrer"
          onClick={(event) => {
            if (openExternalUrlInDesktop(guide.entry.documentationUrl)) {
              event.preventDefault();
            }
          }}
          className="inline-flex shrink-0 items-center gap-1 text-[12px] text-[var(--ink-tertiary)] transition-colors hover:text-[var(--ink)]"
        >
          {t("agents.setup.docs")}
          <ExternalLink className="h-3 w-3" />
        </a>
      </div>

      {showAuthBanner && (
        <div className="mb-3 flex items-start gap-2 rounded-[8px] border border-amber-500/20 bg-amber-500/5 p-3 text-[12px] leading-relaxed text-amber-400">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <div>
            <p className="font-medium">{t("agents.setup.authRequired")}</p>
            <p className="mt-0.5 text-amber-400/80">
              {t("agents.setup.authHint")}
            </p>
          </div>
        </div>
      )}

      {platform === "windows" && guide.windowsSupport === "wsl_only" && (
        <div className="mb-3 flex items-start gap-2 rounded-[8px] border border-amber-500/20 bg-amber-500/5 p-3 text-[12px] leading-relaxed text-amber-400">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          {t("agents.setup.wslOnly")}
        </div>
      )}

      <p className="mb-3 text-[12px] leading-relaxed text-[var(--ink-tertiary)]">
        {needsInstall
          ? t("agents.setup.installHint")
          : t("agents.setup.authHint")}
      </p>

      <div className="space-y-3">
        {guide.steps.map((step, index) => (
          <div key={step.kind}>
            <p className="mb-1.5 text-[12px] font-medium text-[var(--ink-subtle)]">
              {index + 1}. {stepTitle(step)}
            </p>
            <GuideCommandBlock
              step={step}
              copied={copiedStep === index}
              onCopy={() => void handleCopyStep(step, index)}
              copyLabel={t("agents.setup.copy")}
            />
            {step.kind === "node" && (
              <p className="mt-1.5 text-[11px] leading-relaxed text-[var(--ink-tertiary)]">
                {t("agents.setup.nodeHint")}
              </p>
            )}
            {step.authFollowUpCommand && (
              <p className="mt-1.5 text-[11px] leading-relaxed text-[var(--ink-tertiary)]">
                {t("agents.setup.followUpHint", {
                  command: step.authFollowUpCommand,
                })}
              </p>
            )}
          </div>
        ))}
      </div>

      <div className="mt-4 flex flex-wrap items-center gap-2">
        {desktopTerminalAvailable && (
          <button
            type="button"
            onClick={() => void handleOpenTerminal()}
            disabled={openingTerminal}
            className={cx(actionButtonClass, primaryActionClass)}
          >
            <Terminal className="h-3.5 w-3.5" />
            {needsInstall
              ? t("agents.setup.openTerminal")
              : t("agents.setup.openTerminalAuth")}
          </button>
        )}
        <button
          type="button"
          onClick={() => void handleCopyAll()}
          className={cx(actionButtonClass, ghostActionClass)}
        >
          {copiedStep === "all" ? (
            <Check className="h-3.5 w-3.5 text-[var(--success)]" />
          ) : (
            <Copy className="h-3.5 w-3.5" />
          )}
          {copiedStep === "all"
            ? t("agents.setup.copied")
            : t("agents.setup.copyAll")}
        </button>
        <button
          type="button"
          onClick={onRecheck}
          disabled={rechecking}
          className={cx(actionButtonClass, ghostActionClass)}
        >
          <RefreshCw
            className={cx("h-3.5 w-3.5", rechecking && "animate-spin")}
          />
          {rechecking
            ? t("agents.setup.rechecking")
            : t("agents.setup.recheck")}
        </button>
      </div>

      {terminalFeedback && (
        <div
          role="status"
          className={cx(
            "mt-3 rounded-[8px] border p-3 text-[12px] leading-relaxed",
            terminalFeedback === "copied"
              ? "border-[var(--primary)]/30 bg-[var(--primary-tint)] text-[var(--primary)]"
              : "border-amber-500/20 bg-amber-500/5 text-amber-400",
          )}
        >
          {terminalFeedback === "copied"
            ? t("agents.setup.terminalCopied")
            : terminalFeedback === "unavailable"
              ? t("agents.setup.terminalUnavailable")
              : t("agents.setup.copyFailed")}
        </div>
      )}
    </section>
  );
}