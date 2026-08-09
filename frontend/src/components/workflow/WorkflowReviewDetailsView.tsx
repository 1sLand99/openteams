import { useState } from 'react';
import {
  AlertTriangle,
  CheckCircle2,
  ChevronRight,
  ClipboardList,
  FileText,
  ListChecks,
  MinusCircle,
  XCircle,
  type LucideIcon,
} from 'lucide-react';
import { useAppTranslation } from '@/hooks/useAppTranslation';
import { cn } from '@/lib/utils';
import { parseWorkflowTranscriptMeta } from './WorkflowFinalReviewCard';

/**
 * One acceptance item of the normalized review projection written by the
 * backend into the transcript meta. `stepKey` is only present on loop review
 * items, whose criteria span multiple steps.
 *
 * Note: this is the backend-derived projection — the frontend must never
 * parse the agent's raw protocol output (e.g. `loop_review_result` with its
 * `results` map) directly.
 */
export type WorkflowReviewAcceptanceResult = {
  criterion: string;
  level: string;
  verdict: string;
  evidence: string;
  stepKey?: string;
};

export type WorkflowReviewTranscriptDetails = {
  acceptanceResults: WorkflowReviewAcceptanceResult[];
  evidence: string[];
  risks: string[];
  unfinishedItems: string[];
};

function readNonEmptyStringArray(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter(
        (item): item is string =>
          typeof item === 'string' && item.trim().length > 0
      )
    : [];
}

/**
 * Parses the backend's normalized review projection from a review transcript
 * entry's meta (`lead_review` / `step_review` / `loop_review`). This meta is
 * the single data source for review details: `acceptance_results[]`
 * (`step_key`/`criterion`/`level`/`verdict`/`evidence`), `evidence[]`,
 * `risks[]` and `unfinished_items[]`. Raw agent protocol payloads are never
 * read here.
 */
export function parseWorkflowReviewTranscriptDetails(
  metaJson: string | null | undefined
): WorkflowReviewTranscriptDetails | null {
  const meta = parseWorkflowTranscriptMeta(metaJson);
  if (!meta) return null;

  const acceptanceResults = Array.isArray(meta.acceptance_results)
    ? meta.acceptance_results.flatMap((item) => {
        if (!item || typeof item !== 'object' || Array.isArray(item)) return [];
        const result = item as Record<string, unknown>;
        const criterion =
          typeof result.criterion === 'string' ? result.criterion.trim() : '';
        const level = typeof result.level === 'string' ? result.level.trim() : '';
        const verdict =
          typeof result.verdict === 'string' ? result.verdict.trim() : '';
        const evidence =
          typeof result.evidence === 'string' ? result.evidence.trim() : '';
        const stepKey =
          typeof result.step_key === 'string' ? result.step_key.trim() : '';
        if (!criterion || !level || !verdict) return [];
        return [
          {
            criterion,
            level,
            verdict,
            evidence,
            ...(stepKey ? { stepKey } : {}),
          },
        ];
      })
    : [];
  const details = {
    acceptanceResults,
    evidence: readNonEmptyStringArray(meta.evidence),
    risks: readNonEmptyStringArray(meta.risks),
    unfinishedItems: readNonEmptyStringArray(meta.unfinished_items),
  };
  return details.acceptanceResults.length > 0 ||
    details.evidence.length > 0 ||
    details.risks.length > 0 ||
    details.unfinishedItems.length > 0
    ? details
    : null;
}

function normalizeReviewToken(value: string): string {
  return value
    .trim()
    .toLowerCase()
    .replace(/[\s-]+/g, '_');
}

function getAcceptanceVerdictPresentation(verdict: string): {
  icon: LucideIcon;
  iconClassName: string;
  badgeClassName: string;
  labelKey: string;
  defaultLabel: string;
} {
  switch (normalizeReviewToken(verdict)) {
    case 'passed':
    case 'pass':
    case 'approved':
      return {
        icon: CheckCircle2,
        iconClassName: 'text-emerald-500',
        badgeClassName:
          'border-emerald-500/30 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400',
        labelKey: 'workflow.reviewDetails.itemVerdict.passed',
        defaultLabel: 'Passed',
      };
    case 'failed':
    case 'fail':
    case 'rejected':
      return {
        icon: XCircle,
        iconClassName: 'text-[#E5484D]',
        badgeClassName: 'border-[#E5484D]/30 bg-[#E5484D]/10 text-[#E5484D]',
        labelKey: 'workflow.reviewDetails.itemVerdict.failed',
        defaultLabel: 'Failed',
      };
    case 'not_applicable':
    case 'na':
      return {
        icon: MinusCircle,
        iconClassName: 'text-[var(--ink-tertiary)]',
        badgeClassName:
          'border-[var(--hairline)] bg-[var(--surface-2)] text-[var(--ink-tertiary)]',
        labelKey: 'workflow.reviewDetails.itemVerdict.notApplicable',
        defaultLabel: 'N/A',
      };
    default:
      return {
        icon: MinusCircle,
        iconClassName: 'text-[var(--ink-tertiary)]',
        badgeClassName:
          'border-[var(--hairline)] bg-[var(--surface-2)] text-[var(--ink-subtle)]',
        labelKey: '',
        defaultLabel: verdict,
      };
  }
}

function getAcceptanceLevelPresentation(level: string): {
  badgeClassName: string;
  labelKey: string;
  defaultLabel: string;
} {
  switch (normalizeReviewToken(level)) {
    case 'required':
      return {
        badgeClassName: 'border-[#E5484D]/30 bg-[#E5484D]/10 text-[#E5484D]',
        labelKey: 'workflow.reviewDetails.level.required',
        defaultLabel: 'Required',
      };
    case 'partial':
      return {
        badgeClassName:
          'border-amber-500/30 bg-amber-500/10 text-amber-600 dark:text-amber-400',
        labelKey: 'workflow.reviewDetails.level.partial',
        defaultLabel: 'Partial',
      };
    case 'recommended':
      return {
        badgeClassName:
          'border-sky-500/30 bg-sky-500/10 text-sky-600 dark:text-sky-400',
        labelKey: 'workflow.reviewDetails.level.recommended',
        defaultLabel: 'Recommended',
      };
    default:
      return {
        badgeClassName:
          'border-[var(--hairline)] bg-[var(--surface-2)] text-[var(--ink-subtle)]',
        labelKey: '',
        defaultLabel: level,
      };
  }
}

function ReviewDetailsSection({
  icon: Icon,
  title,
  children,
}: {
  icon: LucideIcon;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section className="space-y-1.5">
      <h4 className="flex items-center gap-1.5 text-[12px] font-bold uppercase tracking-wider text-[var(--ink)]">
        <Icon className="h-3.5 w-3.5 text-[var(--ink-subtle)]" />
        {title}
      </h4>
      {children}
    </section>
  );
}

function SimpleReviewItemList({ items }: { items: string[] }) {
  return (
    <ul className="space-y-1">
      {items.map((item, index) => (
        <li
          key={index}
          className="flex items-start gap-2 text-[13px] leading-snug text-[var(--ink-subtle)]"
        >
          <span className="mt-[7px] h-1 w-1 shrink-0 rounded-full bg-[var(--ink-tertiary)]" />
          <span className="min-w-0 whitespace-pre-wrap break-words">
            {item}
          </span>
        </li>
      ))}
    </ul>
  );
}

function AcceptanceResultRow({
  result,
}: {
  result: WorkflowReviewAcceptanceResult;
}) {
  const { t } = useAppTranslation();
  const [expanded, setExpanded] = useState(false);
  const verdictPresentation = getAcceptanceVerdictPresentation(result.verdict);
  const levelPresentation = getAcceptanceLevelPresentation(result.level);
  const VerdictIcon = verdictPresentation.icon;
  const canExpand = result.evidence.length > 0;
  const levelLabel = levelPresentation.labelKey
    ? t(levelPresentation.labelKey, {
        defaultValue: levelPresentation.defaultLabel,
      })
    : result.level;
  const evidenceLabel = t('workflow.reviewDetails.evidence', {
    defaultValue: 'Evidence',
  });

  return (
    <li className="border-b border-[var(--hairline)] last:border-b-0">
      <button
        type="button"
        onClick={() => canExpand && setExpanded((value) => !value)}
        aria-expanded={canExpand ? expanded : undefined}
        aria-label={
          !canExpand
            ? undefined
            : expanded
              ? t('workflow.reviewDetails.collapseEvidence', {
                  defaultValue: 'Collapse evidence',
                })
              : t('workflow.reviewDetails.expandEvidence', {
                  defaultValue: 'Expand evidence',
                })
        }
        className={cn(
          'flex w-full items-center gap-2 py-1.5 text-left transition-colors',
          canExpand && 'hover:bg-[var(--surface-2)]'
        )}
      >
        <VerdictIcon
          className={cn(
            'h-3.5 w-3.5 shrink-0',
            verdictPresentation.iconClassName
          )}
        />
        {result.stepKey && (
          <span className="shrink-0 rounded-[3px] border border-[var(--hairline)] bg-[var(--surface-2)] px-1 py-px font-mono text-[10px] font-medium text-[var(--ink-tertiary)]">
            {result.stepKey}
          </span>
        )}
        <span className="min-w-0 flex-1 truncate text-[13px] font-medium leading-snug text-[var(--ink)]">
          {result.criterion}
        </span>
        <span
          className={cn(
            'shrink-0 rounded-[3px] border px-1 py-px text-[10px] font-semibold uppercase tracking-wide',
            levelPresentation.badgeClassName
          )}
        >
          {levelLabel}
        </span>
        {canExpand && (
          <ChevronRight
            className={cn(
              'h-3 w-3 shrink-0 text-[var(--ink-tertiary)] transition-transform',
              expanded && 'rotate-90'
            )}
          />
        )}
      </button>
      {canExpand && expanded && (
        <div className="flex items-start gap-1.5 pb-2 pl-[22px] pr-1 text-[12px] leading-snug">
          <span className="shrink-0 font-semibold text-[var(--ink-tertiary)]">
            {evidenceLabel}:
          </span>
          <span className="min-w-0 whitespace-pre-wrap break-words text-[var(--ink-subtle)]">
            {result.evidence}
          </span>
        </div>
      )}
    </li>
  );
}

export function WorkflowReviewDetailsView({
  details,
  className,
}: {
  details: WorkflowReviewTranscriptDetails;
  className?: string;
}) {
  const { t } = useAppTranslation();

  return (
    <div className={cn('space-y-3 select-text', className)}>
      {details.acceptanceResults.length > 0 && (
        <ReviewDetailsSection
          icon={ListChecks}
          title={t('workflow.reviewDetails.acceptanceResults', {
            defaultValue: 'Acceptance Results',
          })}
        >
          <ul>
            {details.acceptanceResults.map((result, index) => (
              <AcceptanceResultRow key={index} result={result} />
            ))}
          </ul>
        </ReviewDetailsSection>
      )}
      {details.evidence.length > 0 && (
        <ReviewDetailsSection
          icon={FileText}
          title={t('workflow.reviewDetails.evidence', {
            defaultValue: 'Evidence',
          })}
        >
          <SimpleReviewItemList items={details.evidence} />
        </ReviewDetailsSection>
      )}
      {details.risks.length > 0 && (
        <ReviewDetailsSection
          icon={AlertTriangle}
          title={t('workflow.reviewDetails.risks', {
            defaultValue: 'Risks',
          })}
        >
          <SimpleReviewItemList items={details.risks} />
        </ReviewDetailsSection>
      )}
      {details.unfinishedItems.length > 0 && (
        <ReviewDetailsSection
          icon={ClipboardList}
          title={t('workflow.reviewDetails.unfinishedItems', {
            defaultValue: 'Unfinished Items',
          })}
        >
          <SimpleReviewItemList items={details.unfinishedItems} />
        </ReviewDetailsSection>
      )}
    </div>
  );
}