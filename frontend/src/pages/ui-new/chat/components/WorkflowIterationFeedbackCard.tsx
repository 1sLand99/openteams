import { useEffect, useMemo, useState } from 'react';
import { AlertCircle } from 'lucide-react';
import type { WorkflowIterationSummaryData } from '@/lib/api';
import { cn } from '@/lib/utils';

type WorkflowIterationFeedbackPayload = {
  action: 'accept' | 'reject';
  feedback?: {
    what_wrong: string;
    expected: string;
    priority: 'high' | 'medium' | 'low';
    additional_notes?: string;
  };
};

type WorkflowIterationFeedbackCardProps = {
  currentRound: number;
  iterationHistory: WorkflowIterationSummaryData[];
  canReviewCurrentRound?: boolean;
  pendingActionId?: string | null;
  onSubmit?: (payload: WorkflowIterationFeedbackPayload) => void;
};

function roundStatusTone(status: string) {
  switch (status) {
    case 'accepted':
      return 'border-emerald-300 bg-emerald-50 text-emerald-700';
    case 'rejected':
      return 'border-rose-300 bg-rose-50 text-rose-700';
    case 'running':
      return 'border-blue-300 bg-blue-50 text-blue-700';
    default:
      return 'border-slate-200 bg-slate-50 text-slate-600';
  }
}

export function WorkflowIterationFeedbackCard({
  currentRound,
  iterationHistory,
  canReviewCurrentRound: canReviewCurrentRoundProp = false,
  pendingActionId,
  onSubmit,
}: WorkflowIterationFeedbackCardProps) {
  const [selectedRound, setSelectedRound] = useState<number | null>(null);
  const [expandedReject, setExpandedReject] = useState(false);
  const [whatWrong, setWhatWrong] = useState('');
  const [expected, setExpected] = useState('');
  const [priority, setPriority] = useState<'high' | 'medium' | 'low'>('high');
  const [additionalNotes, setAdditionalNotes] = useState('');
  const [validationError, setValidationError] = useState<string | null>(null);

  const orderedHistory = useMemo(
    () =>
      [...iterationHistory].sort(
        (left, right) => right.round_index - left.round_index
      ),
    [iterationHistory]
  );

  useEffect(() => {
    if (orderedHistory.length === 0) {
      setSelectedRound(null);
      return;
    }

    setSelectedRound((previous) => {
      if (
        previous != null &&
        orderedHistory.some((item) => item.round_index === previous)
      ) {
        return previous;
      }
      return orderedHistory[0].round_index;
    });
  }, [orderedHistory]);

  const selectedIteration =
    orderedHistory.find((item) => item.round_index === selectedRound) ?? null;
  const canSubmit = !!onSubmit;
  const disabled = !!pendingActionId;
  const latestIteration = orderedHistory[0] ?? null;
  const canReviewCurrentRound =
    canReviewCurrentRoundProp &&
    currentRound > 0 &&
    latestIteration?.round_index === currentRound;

  const handleAccept = () => {
    setExpandedReject(false);
    setValidationError(null);
    onSubmit?.({ action: 'accept' });
  };

  const handleReject = () => {
    if (!expandedReject) {
      setExpandedReject(true);
      return;
    }

    const nextWhatWrong = whatWrong.trim();
    const nextExpected = expected.trim();
    if (!nextWhatWrong || !nextExpected) {
      setValidationError('Reject 需要填写 what_wrong 和 expected。');
      return;
    }

    setValidationError(null);
    onSubmit?.({
      action: 'reject',
      feedback: {
        what_wrong: nextWhatWrong,
        expected: nextExpected,
        priority,
        additional_notes: additionalNotes.trim() || undefined,
      },
    });
  };

  return (
    <div className="bg-white border border-slate-200 p-4 rounded-xl shadow-sm">
      <div className="text-xs font-bold text-blue-700 flex items-center gap-2 mb-3">
        <AlertCircle className="w-4 h-4" /> Iteration History
        <span className="rounded-full border border-blue-200 bg-blue-50 px-2 py-0.5 text-[10px] font-bold uppercase tracking-widest text-blue-600">
          Round {currentRound}
        </span>
      </div>

      {orderedHistory.length > 0 ? (
        <>
          <div className="flex flex-wrap gap-2 mb-3">
            {orderedHistory.map((item) => (
              <button
                key={item.round_index}
                type="button"
                onClick={() => setSelectedRound(item.round_index)}
                className={cn(
                  'rounded-full border px-3 py-1 text-xs font-semibold transition-colors',
                  selectedRound === item.round_index
                    ? 'border-indigo-400 bg-indigo-50 text-indigo-700'
                    : 'border-slate-200 bg-white text-slate-500 hover:bg-slate-50'
                )}
              >
                Round {item.round_index}
              </button>
            ))}
          </div>

          {selectedIteration && (
            <div className="rounded-lg border border-slate-200 bg-slate-50 p-3 mb-3">
              <span
                className={cn(
                  'inline-block rounded-full border px-2.5 py-1 text-[10px] font-bold uppercase tracking-widest mb-2',
                  roundStatusTone(selectedIteration.status)
                )}
              >
                {selectedIteration.status}
              </span>
              {selectedIteration.result_summary && (
                <div className="text-[11px] text-slate-600 leading-relaxed mb-2">
                  {selectedIteration.result_summary}
                </div>
              )}
              {selectedIteration.user_feedback && (
                <div className="rounded-lg border border-rose-200 bg-white p-2.5 text-[11px] text-rose-700 leading-relaxed">
                  {selectedIteration.user_feedback}
                </div>
              )}
            </div>
          )}
        </>
      ) : (
        <div className="text-[11px] text-slate-400 mb-3">
          No iteration history yet.
        </div>
      )}

      {canReviewCurrentRound && (
        <div className="bg-amber-50 border border-amber-300 rounded-lg p-3">
          <div className="text-[10px] font-bold uppercase tracking-widest text-amber-800 mb-2">
            Current Round Decision
          </div>
          <p className="text-[11px] text-slate-600 mb-3 leading-relaxed">
            Accept the current round or reject it with structured feedback.
          </p>

          {expandedReject && (
            <div className="grid gap-2 mb-3">
              <textarea
                value={whatWrong}
                onChange={(e) => setWhatWrong(e.target.value)}
                rows={2}
                disabled={disabled || !canSubmit}
                placeholder="what_wrong"
                className="w-full rounded-lg border border-rose-200 bg-white px-3 py-2 text-xs text-slate-700 outline-none placeholder:text-slate-400 focus:border-rose-400 disabled:opacity-60"
              />
              <textarea
                value={expected}
                onChange={(e) => setExpected(e.target.value)}
                rows={2}
                disabled={disabled || !canSubmit}
                placeholder="expected"
                className="w-full rounded-lg border border-rose-200 bg-white px-3 py-2 text-xs text-slate-700 outline-none placeholder:text-slate-400 focus:border-rose-400 disabled:opacity-60"
              />
              <select
                value={priority}
                onChange={(e) =>
                  setPriority(e.target.value as 'high' | 'medium' | 'low')
                }
                disabled={disabled || !canSubmit}
                className="rounded-lg border border-rose-200 bg-white px-3 py-2 text-xs text-slate-700 outline-none focus:border-rose-400 disabled:opacity-60"
              >
                <option value="high">high</option>
                <option value="medium">medium</option>
                <option value="low">low</option>
              </select>
              <textarea
                value={additionalNotes}
                onChange={(e) => setAdditionalNotes(e.target.value)}
                rows={2}
                disabled={disabled || !canSubmit}
                placeholder="additional_notes"
                className="w-full rounded-lg border border-rose-200 bg-white px-3 py-2 text-xs text-slate-700 outline-none placeholder:text-slate-400 focus:border-rose-400 disabled:opacity-60"
              />
              {validationError && (
                <div className="text-[10px] text-rose-600">
                  {validationError}
                </div>
              )}
            </div>
          )}

          <div className="flex gap-2">
            <button
              type="button"
              onClick={handleAccept}
              disabled={disabled || !canSubmit}
              className="flex-1 py-1.5 bg-emerald-600 text-white rounded text-[10px] font-bold hover:bg-emerald-700 transition-colors shadow-sm disabled:opacity-50"
            >
              ACCEPT
            </button>
            <button
              type="button"
              onClick={handleReject}
              disabled={disabled || !canSubmit}
              className={`flex-1 py-1.5 rounded text-[10px] font-bold transition-colors disabled:opacity-50 ${
                expandedReject
                  ? 'bg-rose-50 border border-rose-200 text-rose-700'
                  : 'bg-white border border-slate-300 text-slate-700 hover:bg-slate-50'
              }`}
            >
              {expandedReject ? 'SUBMIT REJECT' : 'REJECT'}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
