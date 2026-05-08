import { AlertCircle } from 'lucide-react';

type FinalReviewTranscriptLike = {
  id: string;
  entry_type: string;
  content: string;
  round_id?: string | null;
  meta_json?: string | null;
};

export type WorkflowFinalReviewActionData = {
  executionId: string;
  transcriptId: string;
  roundId?: string | null;
  message: string;
  description?: string;
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return !!value && typeof value === 'object' && !Array.isArray(value);
}

export function parseWorkflowTranscriptMeta(
  metaJson: string | null | undefined
): Record<string, unknown> | null {
  if (!metaJson) return null;
  try {
    const parsed = JSON.parse(metaJson) as unknown;
    return isRecord(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

export function findPendingFinalReviewTranscript<
  T extends FinalReviewTranscriptLike,
>(entries: T[]): T | null {
  return (
    entries.find((entry) => {
      if (entry.entry_type !== 'final_review') {
        return false;
      }
      const meta = parseWorkflowTranscriptMeta(entry.meta_json);
      return meta?.resolved === false;
    }) ?? null
  );
}

export function toWorkflowFinalReviewAction<
  T extends FinalReviewTranscriptLike,
>(
  executionId: string | null | undefined,
  entries: T[]
): WorkflowFinalReviewActionData | null {
  if (!executionId) {
    return null;
  }

  const transcript = findPendingFinalReviewTranscript(entries);
  if (!transcript) {
    return null;
  }

  const meta = parseWorkflowTranscriptMeta(transcript.meta_json);
  return {
    executionId,
    transcriptId: transcript.id,
    roundId: transcript.round_id ?? null,
    message: transcript.content || '任务已完成，是否接受结果？',
    description:
      typeof meta?.description === 'string' ? meta.description : undefined,
  };
}

type WorkflowFinalReviewCardProps = {
  message?: string;
  description?: string;
  onAccept: () => void;
  onReject: () => void;
  disabled?: boolean;
};

export function WorkflowFinalReviewCard({
  message = '任务已完成，是否接受结果？',
  description,
  onAccept,
  onReject,
  disabled,
}: WorkflowFinalReviewCardProps) {
  return (
    <div className="bg-white border-2 border-amber-400 p-4 rounded-xl shadow-lg animate-in fade-in slide-in-from-bottom-4">
      <div className="text-xs font-bold text-amber-800 flex items-center gap-2 mb-2">
        <AlertCircle className="w-4 h-4" /> Final Review
      </div>
      <p className="text-[11px] text-slate-600 mb-3 leading-relaxed font-medium">
        {message}
      </p>
      {description && (
        <p className="text-[11px] text-slate-500 mb-3 leading-relaxed">
          {description}
        </p>
      )}
      <div className="flex gap-2">
        <button
          type="button"
          onClick={onAccept}
          disabled={disabled}
          className="flex-1 py-1.5 bg-emerald-600 text-white rounded text-[10px] font-bold hover:bg-emerald-700 transition-colors shadow-sm disabled:opacity-50 disabled:cursor-not-allowed"
        >
          ACCEPT
        </button>
        <button
          type="button"
          onClick={onReject}
          disabled={disabled}
          className="flex-1 py-1.5 bg-white border border-slate-300 text-slate-700 rounded text-[10px] font-bold hover:bg-slate-50 transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
        >
          REJECT
        </button>
      </div>
    </div>
  );
}
