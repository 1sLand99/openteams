import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import type { WorkflowPendingReviewData } from '@/lib/api';

const dom = new JSDOM('<!doctype html><html><body><div id="root"></div></body></html>', {
  url: 'http://localhost',
});
Object.defineProperties(globalThis, {
  window: { value: dom.window, configurable: true },
  document: { value: dom.window.document, configurable: true },
  navigator: { value: dom.window.navigator, configurable: true },
  HTMLElement: { value: dom.window.HTMLElement, configurable: true },
  HTMLTextAreaElement: {
    value: dom.window.HTMLTextAreaElement,
    configurable: true,
  },
  Event: { value: dom.window.Event, configurable: true },
  MouseEvent: { value: dom.window.MouseEvent, configurable: true },
});
(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

const React = await import('react');
const { act } = React;
const { createRoot } = await import('react-dom/client');
const { WorkspaceContext } = await import('@/context/WorkspaceContext');
const { WorkflowPendingReviewCard } = await import(
  './WorkflowPendingReviewCard'
);

const container = document.querySelector<HTMLDivElement>('#root');
assert.ok(container);
const root = createRoot(container);
const translations = {
  t: (key: string) => key,
} as React.ComponentProps<typeof WorkspaceContext.Provider>['value'];

const review = (
  reviewId: string,
  actions: WorkflowPendingReviewData['prompt_template']['actions'],
): WorkflowPendingReviewData => ({
  review_id: reviewId,
  review_type: 'loop_user_review',
  target_id: `target-${reviewId}`,
  target_title: `Review ${reviewId}`,
  context_summary: null,
  prompt_template: {
    message: 'Please review.',
    fields: [
      {
        key: 'feedback',
        label: 'Feedback',
        field_type: 'textarea',
        required: false,
        placeholder: 'Explain the rejection',
      },
    ],
    actions,
  },
});

const ordinaryReview = review('review-1', [
  {
    action: 'approve',
    label: 'Approve',
    style: 'primary',
    requires_feedback: false,
  },
  {
    action: 'reject',
    label: 'Reject',
    style: 'danger',
    requires_feedback: true,
  },
]);
const skippedReview = review('review-2', [
  {
    action: 'restart_skipped',
    label: 'Restart skipped',
    style: 'primary',
    requires_feedback: false,
  },
  {
    action: 'keep_skipped',
    label: 'Keep skipped',
    style: 'secondary',
    requires_feedback: false,
  },
]);
const submissions: Array<{ action: string; feedback?: string }> = [];
const onSubmit = (action: string, feedback?: string) => {
  submissions.push({ action, feedback });
};

const renderCard = async (
  pendingReview: WorkflowPendingReviewData,
  pendingActionId: string | null = null,
) => {
  await act(async () => {
    root.render(
      <WorkspaceContext.Provider value={translations}>
        <WorkflowPendingReviewCard
          pendingReview={pendingReview}
          pendingActionId={pendingActionId}
          onSubmit={onSubmit}
        />
      </WorkspaceContext.Provider>,
    );
  });
};

const button = (label: string) => {
  const match = Array.from(container.querySelectorAll('button')).find((item) =>
    item.textContent?.includes(label),
  );
  assert.ok(match, `button ${label} should exist`);
  return match;
};

const click = async (element: Element) => {
  await act(async () => {
    element.dispatchEvent(
      new dom.window.MouseEvent('click', { bubbles: true, cancelable: true }),
    );
  });
};

const enterFeedback = async (value: string) => {
  const textarea = container.querySelector<HTMLTextAreaElement>('textarea');
  assert.ok(textarea, 'feedback textarea should be visible');
  const valueSetter = Object.getOwnPropertyDescriptor(
    dom.window.HTMLTextAreaElement.prototype,
    'value',
  )?.set;
  assert.ok(valueSetter);
  await act(async () => {
    valueSetter.call(textarea, value);
    textarea.dispatchEvent(new dom.window.Event('input', { bubbles: true }));
  });
};

await renderCard(ordinaryReview);
await click(button('REJECT'));
await enterFeedback('  missing evidence  ');
await click(button('SUBMIT REJECT'));
assert.deepEqual(submissions.at(-1), {
  action: 'reject',
  feedback: 'missing evidence',
});

// A no-feedback action must not inherit text entered for a previous action.
await click(button('APPROVE'));
assert.deepEqual(submissions.at(-1), { action: 'approve', feedback: undefined });

// Switching reviews resets expanded state, textarea content, and validation.
await click(button('REJECT'));
await enterFeedback('stale feedback');
await renderCard(skippedReview);
assert.equal(container.querySelector('textarea'), null);
await click(button('RESTART SKIPPED'));
assert.deepEqual(submissions.at(-1), {
  action: 'restart_skipped',
  feedback: undefined,
});

// Loading disables all actions and prevents duplicate submission.
const submissionCount = submissions.length;
await renderCard(skippedReview, skippedReview.review_id);
for (const actionButton of container.querySelectorAll('button')) {
  assert.equal(actionButton.disabled, true);
  await click(actionButton);
}
assert.equal(submissions.length, submissionCount);

await act(async () => root.unmount());
dom.window.close();
console.log('Workflow pending review interactions: PASS');
