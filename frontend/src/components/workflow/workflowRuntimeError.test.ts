// Run with:
//     pnpm exec tsx src/components/workflow/workflowRuntimeError.test.ts

import { strict as assert } from 'node:assert';
import { localizeWorkflowRuntimeError } from './workflowRuntimeError';

const interpolate = (
  _key: string,
  fallback: string,
  replacements?: Record<string, string | number>,
) =>
  Object.entries(replacements ?? {}).reduce(
    (message, [key, value]) => message.replace(`{${key}}`, String(value)),
    fallback,
  );

const inactivity =
  'openteams.workflow_runtime_error:{"code":"session_inactivity_timeout","agent_name":"opencode","inactivity_minutes":40}';
assert.equal(
  localizeWorkflowRuntimeError(inactivity, interpolate),
  'Workflow stopped because opencode had no session activity for 40 minutes.',
);
assert.equal(
  localizeWorkflowRuntimeError(
    `运行时错误: workflow validation error: ${inactivity}`,
    interpolate,
  ),
  'Workflow stopped because opencode had no session activity for 40 minutes.',
);

const failureWithDetail =
  'openteams.workflow_runtime_error:{"code":"execution_failed","agent_name":"codex"}\n\nopenteams.workflow_runtime_error_detail:ERROR: provider overloaded';
assert.equal(
  localizeWorkflowRuntimeError(failureWithDetail, interpolate),
  'Workflow execution failed for codex.\n\nExecutor error:\nERROR: provider overloaded',
);

const compactedInboxFailure = failureWithDetail.replace(/\s+/g, ' ');
assert.equal(
  localizeWorkflowRuntimeError(compactedInboxFailure, interpolate),
  'Workflow execution failed for codex.\n\nExecutor error:\nERROR: provider overloaded',
);

const legacyError = 'legacy workflow failure';
assert.equal(
  localizeWorkflowRuntimeError(legacyError, interpolate),
  legacyError,
);

const unknownCode =
  'openteams.workflow_runtime_error:{"code":"future_error","agent_name":"codex"}';
assert.equal(
  localizeWorkflowRuntimeError(unknownCode, interpolate),
  unknownCode,
);

// eslint-disable-next-line no-console
console.log('workflowRuntimeError: ok');
