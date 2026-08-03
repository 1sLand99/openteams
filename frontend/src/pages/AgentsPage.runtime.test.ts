// Lightweight regression checks for the agent runtime configuration page.
// Run with: pnpm exec tsx src/pages/AgentsPage.runtime.test.ts

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const source = readFileSync(new URL('./AgentsPage.tsx', import.meta.url), 'utf8');
const apiSource = readFileSync(new URL('../lib/api.ts', import.meta.url), 'utf8');

assert.match(source, /useEffect\(\(\) => \{[\s\S]*?agentRuntimeApi[\s\S]*?\.getDiagnostics\(runner\.runner_type\)[\s\S]*?\}, \[refreshRevision, runner\.runner_type\]\);/u);
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
assert.match(
  source,
  /<AgentInstallGuide[\s\S]*?runner=\{runner\}[\s\S]*?rechecking=\{rechecking\}[\s\S]*?onRecheck=\{onRecheck\}/u,
);
assert.doesNotMatch(source, /npx/iu);
assert.match(source, /const envSummary = runner\.env_summary;/u);
assert.doesNotMatch(
  source,
  /const envSummary = currentDiagnostics\?\.env_summary/u,
);
const diagnosticsMerge = source.slice(
  source.indexOf('const handleDiagnosticsLoaded'),
  source.indexOf('const handleSave'),
);
assert.doesNotMatch(
  diagnosticsMerge,
  /run_mode|env_summary|executor_options/u,
);
const modelField = source.slice(
  source.indexOf('function ModelConfigField'),
  source.indexOf('/* ---------- Embedded agent configuration sidebar ---------- */'),
);
assert.doesNotMatch(modelField, /agentRuntimeApi\.(addModel|renameModel)/u);
assert.doesNotMatch(modelField, /<input|<form|agents\.model\.add/u);
assert.match(modelField, /options=\{options\}/u);
assert.doesNotMatch(modelField, /onRefreshModels|agents\.model\.refresh/u);
assert.doesNotMatch(modelField, /<button/u);
assert.match(
  source,
  /findAcpSelectConfigOption\([\s\S]*?currentDiagnostics\?\.acp_probe\?\.config_options[\s\S]*?"model"/u,
);
assert.match(
  source,
  /const isAcpRunner[\s\S]*?runner === "GEMINI"[\s\S]*?runner === "QWEN_CODE"[\s\S]*?runner === "KIMI_CODE"[\s\S]*?runner === "QODER_CLI"/u,
);
assert.match(
  source,
  /import qoderCliSchema from "..\/..\/..\/shared\/schemas\/qoder_cli\.json";/u,
);
assert.match(source, /QODER_CLI: qoderCliSchema,/u);
assert.match(
  source,
  /QODER_CLI: \{[\s\S]*?title: "Qoder"[\s\S]*?logoSrc: "\/logos\/qoder-logo\.svg"/u,
);
assert.match(
  source,
  /agentRuntimeApi[\s\S]*?\.getDiagnostics\(runner\.runner_type\)[\s\S]*?\[refreshRevision, runner\.runner_type\]/u,
);
assert.doesNotMatch(source, /\[\.\.\.models, selectedModel\]/u);
assert.match(
  source,
  /const handleRefreshConfig = async \(\) => \{[\s\S]*?await agentRuntimeApi\.refresh\(\)[\s\S]*?setRefreshRevision/u,
);
const focusEffect = source.slice(
  source.indexOf('const handleWindowFocus'),
  source.indexOf('window.addEventListener("focus"'),
);
assert.match(focusEffect, /agentRuntimeApi\.refreshLight\(\)/u);
assert.doesNotMatch(focusEffect, /agentRuntimeApi\.refresh\(\)/u);
assert.doesNotMatch(focusEffect, /setRefreshRevision/u);
assert.match(source, /sidebarDiagnosticsStore\.get\(runner\.runner_type\)/u);
assert.match(
  source,
  /sidebarDiagnosticsStore\.set\(runner\.runner_type, result\)/u,
);
const explicitRefreshHandler = source.slice(
  source.indexOf('const handleRefreshConfig'),
  source.indexOf('const handleDiagnosticsLoaded'),
);
assert.match(explicitRefreshHandler, /invalidateSidebarDiagnostics\(\)/u);
const saveHandler = source.slice(
  source.indexOf('const handleSave'),
  source.indexOf('const handleOpenConfig'),
);
assert.match(saveHandler, /invalidateSidebarDiagnostics\(runner\)/u);
const headerRefreshButton = source.slice(
  source.indexOf('onClick={() => void handleRefreshConfig()}'),
  source.indexOf('</button>', source.indexOf('onClick={() => void handleRefreshConfig()}')),
);
assert.match(headerRefreshButton, /t\("agents\.refreshConfig"\)/u);
assert.match(headerRefreshButton, /data-tooltip-nowrap/u);
assert.doesNotMatch(headerRefreshButton, /agents\.model\.refresh/u);
assert.match(apiSource, /"\/api\/agents\/runtime\/refresh"/u);
assert.doesNotMatch(apiSource, /\/agents\/runtime\/\$\{[^}]+\}\/models/u);

console.log('AgentsPage runtime configuration: PASS');