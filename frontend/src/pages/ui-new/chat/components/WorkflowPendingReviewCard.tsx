import { useMemo, useState } from 'react';
import { AlertCircle } from 'lucide-react';
import type { WorkflowPendingReviewData } from '@/lib/api';

type WorkflowPendingReviewCardProps = {
  pendingReview: WorkflowPendingReviewData;
  pendingActionId?: string | null;
  onSubmit?: (action: 'approve' | 'reject', feedback?: string) => void;
};

function getReviewTypeLabel(reviewType: string) {
  switch (reviewType) {
    case 'step_user_review':
      return 'Step Review';
    case 'loop_user_review':
      return 'Loop Review';
    case 'iteration_acceptance':
      return 'Final Review';
    default:
      return reviewType;
  }
}

export function WorkflowPendingReviewCard({
  pendingReview,
  pendingActionId,
  onSubmit,
}: WorkflowPendingReviewCardProps) {
  const [expandedReject, setExpandedReject] = useState(false);
  const [feedback, setFeedback] = useState('');
  const [validationError, setValidationError] = useState<string | null>(null);
  const feedbackField = useMemo(
    () =>
      pendingReview.prompt_template.fields.find(
        (field) => field.key === 'feedback' || field.field_type === 'textarea'
      ) ?? null,
    [pendingReview.prompt_template.fields]
  );
  const disabled = pendingActionId === pendingReview.review_id;

  const handleApprove = () => {
    setExpandedReject(false);
    setValidationError(null);
    onSubmit?.('approve');
  };

  const handleReject = () => {
    if (!expandedReject) {
      setExpandedReject(true);
      return;
    }

    const trimmedFeedback = feedback.trim();
    if (!trimmedFeedback) {
      setValidationError('Reject 需要填写反馈意见。');
      return;
    }

    setValidationError(null);
    onSubmit?.('reject', trimmedFeedback);
  };

  return (
    <div className="bg-white border-2 border-amber-400 p-4 rounded-xl shadow-lg">
      <div className="text-xs font-bold text-amber-800 flex items-center gap-2 mb-2">
        <AlertCircle className="w-4 h-4" /> Pending Review
      </div>

      <div className="flex flex-wrap items-center gap-2 mb-3">
        <span className="rounded-full bg-amber-50 border border-amber-200 px-2.5 py-1 text-[10px] font-bold uppercase tracking-widest text-amber-700">
          {getReviewTypeLabel(pendingReview.review_type)}
        </span>
        <span className="rounded-full bg-slate-50 border border-slate-200 px-2.5 py-1 text-[10px] font-bold uppercase tracking-widest text-slate-600">
          {pendingReview.target_title}
        </span>
      </div>

      <p className="text-[11px] text-slate-600 mb-3 leading-relaxed font-medium">
        {pendingReview.prompt_template.message || '请审核当前结果。'}
      </p>

      {pendingReview.context_summary && (
        <div className="p-3 rounded-lg bg-slate-50 border border-slate-200 mb-3">
          <div className="text-[10px] font-bold uppercase tracking-widest text-slate-500 mb-1">
            Context
          </div>
          <div className="text-[11px] text-slate-600 leading-relaxed whitespace-pre-wrap">
            {pendingReview.context_summary}
          </div>
        </div>
      )}

      {expandedReject && (
        <div className="mb-3">
          <div className="text-[10px] font-bold uppercase tracking-widest text-rose-700 mb-1">
            {feedbackField?.label ?? 'Feedback'}
          </div>
          <textarea
            value={feedback}
            onChange={(event) => setFeedback(event.target.value)}
            rows={3}
            disabled={disabled}
            placeholder={feedbackField?.placeholder ?? '请填写具体修改意见'}
            className="w-full rounded-lg border border-rose-200 bg-white px-3 py-2 text-xs text-slate-700 outline-none transition-colors placeholder:text-slate-400 focus:border-rose-400 focus:ring-2 focus:ring-rose-400/20 disabled:cursor-not-allowed disabled:opacity-60"
          />
          {validationError && (
            <div className="mt-1 text-[10px] text-rose-600">
              {validationError}
            </div>
          )}
        </div>
      )}

      <div className="flex gap-2">
        <button
          type="button"
          onClick={handleApprove}
          disabled={disabled || !onSubmit}
          className="flex-1 py-1.5 bg-emerald-600 text-white rounded text-[10px] font-bold hover:bg-emerald-700 transition-colors shadow-sm disabled:opacity-50 disabled:cursor-not-allowed"
        >
          APPROVE
        </button>
        <button
          type="button"
          onClick={handleReject}
          disabled={disabled || !onSubmit}
          className={`flex-1 py-1.5 rounded text-[10px] font-bold transition-colors disabled:opacity-50 disabled:cursor-not-allowed ${
            expandedReject
              ? 'bg-rose-50 border border-rose-200 text-rose-700'
              : 'bg-white border border-slate-300 text-slate-700 hover:bg-slate-50'
          }`}
        >
          {expandedReject ? 'SUBMIT REJECT' : 'REJECT'}
        </button>
      </div>
    </div>
  );
}
