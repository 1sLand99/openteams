import assert from 'node:assert/strict';

import { localizeWorkflowGeneratedText } from './workflowGeneratedText';

const t = (key: string, options?: Record<string, unknown>) =>
  `${key}:${JSON.stringify(options ?? {})}`;

assert.match(
  localizeWorkflowGeneratedText(
    'workflow.loop_skipped_retry_decision.request',
    t,
  ),
  /workflow\.generatedText\.skippedRetryDecision/,
);

const context = localizeWorkflowGeneratedText(
  `workflow.loop_skipped_retry_decision.context:${JSON.stringify({
    step_titles: 'Build, Test',
    feedback: 'Missing evidence',
    keep_effect: 'waive_skipped_scope_and_complete_loop',
  })}`,
  t,
);
assert.match(context, /skippedRetryContextDetailed/);
assert.match(context, /Build, Test/);
assert.match(context, /Missing evidence/);
assert.match(context, /skippedRetryKeepComplete/);

assert.match(
  localizeWorkflowGeneratedText(
    'workflow.loop_skipped_retry_decision.result.restart_skipped',
    t,
  ),
  /skippedRetryResultRestarted/,
);
assert.match(
  localizeWorkflowGeneratedText(
    'workflow.loop_skipped_retry_decision.result.keep_skipped',
    t,
  ),
  /skippedRetryResultKept/,
);

console.log('Workflow generated skipped-decision text: PASS');
