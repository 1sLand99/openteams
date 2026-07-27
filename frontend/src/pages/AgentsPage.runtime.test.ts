// Lightweight regression checks for the agent runtime configuration page.
// Run with: pnpm exec tsx src/pages/AgentsPage.runtime.test.ts

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./AgentsPage.tsx', import.meta.url), 'utf8');

assert.match(source, /useEffect\(\(\) => \{[\s\S]*?agentRuntimeApi[\s\S]*?\.getDiagnostics\(runner\.runner_type\)[\s\S]*?\}, \[runner\.runner_type\]\);/u);
assert.doesNotMatch(source, /diagnosticsRefreshKey|refreshKey/iu);
assert.match(
  source,
  /const parsedEnv = draft\.envDirty \? parseEnvText\(draft\.envText\) : null;[\s\S]*?if \(parsedEnv && !parsedEnv\.ok\) \{[\s\S]*?return;/u,
);
assert.match(
  source,
  /savedRevision === draftRevisionRef\.current[\s\S]*?setEnvDirty\(false\)/u,
);
assert.match(
  source,
  /if \(envDirty \|\| envInputFocused\) return;[\s\S]*?\[envDirty, envInputFocused, envSummaryText\]/u,
);
assert.match(
  source,
  /<textarea[\s\S]*?onFocus=\{\(\) => \{[\s\S]*?setEnvInputFocused\(true\)[\s\S]*?onBlur=\{\(\) => \{[\s\S]*?setEnvInputFocused\(false\)/u,
);
assert.match(source, /role="alert"[\s\S]*?\{envValidationError\}/u);
assert.equal(
  source.match(/text-\[var\(--ink-subtle\)\] uppercase/g)?.length,
  3,
);
assert.doesNotMatch(source, /text-white\/40 uppercase/u);
assert.match(
  source,
  /idPrefix="agent-acp-permission-confirmation"[\s\S]*?<ConfirmationDialog|<ConfirmationDialog[\s\S]*?idPrefix="agent-acp-permission-confirmation"/u,
);
assert.match(source, /label: t\("permissions\.fullAccessHighRisk"\)/u);
assert.match(
  source,
  /t\("permissions\.fullAccessAgentConfirmTitle"\)/u,
);
assert.match(
  source,
  /t\("permissions\.fullAccessAgentConfirmDescription"\)/u,
);
assert.doesNotMatch(source, /Full Access/u);
assert.doesNotMatch(source, /window\.confirm/u);
assert.doesNotMatch(source, /\{currentDiagnostics\?\.command_source &&/u);
assert.doesNotMatch(source, /\{currentDiagnostics\?\.resolved_command &&/u);
assert.match(
  source,
  /const commandSource = diagnosticsLoading[\s\S]*?t\("agents\.details\.notReported"\);/u,
);
assert.match(
  source,
  /const baseCommand = diagnosticsLoading[\s\S]*?t\("agents\.details\.notReported"\)\);/u,
);
assert.match(
  source,
  /label=\{t\("agents\.details\.commandSource"\)\}[\s\S]*?value=\{commandSource\}/u,
);
assert.match(
  source,
  /label=\{t\("agents\.details\.baseCommand"\)\}[\s\S]*?value=\{baseCommand\}/u,
);

console.log('AgentsPage runtime configuration: PASS');
