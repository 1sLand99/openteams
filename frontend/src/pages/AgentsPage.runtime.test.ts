// Lightweight regression checks for the agent runtime configuration page.
// Run with: pnpm exec tsx src/pages/AgentsPage.runtime.test.ts

import assert from 'node:assert/strict';
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const source = readFileSync(new URL('./AgentsPage.tsx', import.meta.url), 'utf8');
const apiSource = readFileSync(new URL('../lib/api.ts', import.meta.url), 'utf8');
const hermesSchema = JSON.parse(
  readFileSync(new URL('../../../shared/schemas/hermes.json', import.meta.url), 'utf8'),
) as { properties?: Record<string, { properties?: Record<string, unknown> }> };

assert.match(source, /useEffect\(\(\) => \{[\s\S]*?agentRuntimeApi[\s\S]*?\.getDiagnostics\(runner\.runner_type, \{ workspacePath \}\)[\s\S]*?\}, \[refreshRevision, runner\.runner_type, workspacePath\]\);/u);
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
assert.doesNotMatch(source, /npx --|npx -y|install -g pi\b/iu);
// Runtime status/diagnostics merges must carry the npm/npx probes so the
// install guide and error toasts see fresh dependency state.
assert.match(
  source,
  /node_available: runner\.node_available,[\s\S]*?npm_available: runner\.npm_available,[\s\S]*?npx_available: runner\.npx_available,/u,
);
assert.match(
  source,
  /node_available: diagnostics\.node_available,[\s\S]*?npm_available: diagnostics\.npm_available,[\s\S]*?npx_available: diagnostics\.npx_available,/u,
);
// Installed-but-not-executable runners explain which tools are missing.
assert.match(source, /getMissingRuntimeTools\(runner\)/u);
assert.match(source, /t\("agents\.status\.missingDependencies"/u);
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
  /const discoveredCliVersion = currentDiagnostics\?\.version;/u,
);
assert.match(
  source,
  /const acpVersion =[\s\S]*?currentDiagnostics\?\.acp_probe\?\.agent_version/u,
);
assert.match(
  source,
  /label=\{t\("agents\.details\.acpVersion"\)\}[\s\S]*?value=\{acpVersion\}/u,
);
assert.match(
  source,
  /const isAcpRunner[\s\S]*?runner === (?:\("HERMES" as BaseCodingAgent\)|"HERMES")[\s\S]*?runner === "GEMINI"[\s\S]*?runner === "QWEN_CODE"[\s\S]*?runner === "KIMI_CODE"[\s\S]*?runner === "QODER_CLI"[\s\S]*?runner === "PI"/u,
);
assert.match(
  source,
  /import qoderCliSchema from "..\/..\/..\/shared\/schemas\/qoder_cli\.json";/u,
);
assert.match(source, /QODER_CLI: qoderCliSchema,/u);
assert.match(
  source,
  /import piSchema from "..\/..\/..\/shared\/schemas\/pi\.json";/u,
);
assert.match(source, /PI: piSchema,/u);
assert.match(
  source,
  /import hermesSchema from "..\/..\/..\/shared\/schemas\/hermes\.json";/u,
);
assert.match(source, /HERMES: hermesSchema,/u);
assert.match(
  source,
  /HERMES: \{[\s\S]*?title: "Hermes"/u,
);
assert.deepEqual(Object.keys(hermesSchema.properties ?? {}).sort(), [
  'acp',
  'additional_params',
  'append_prompt',
  'base_command_override',
  'env',
  'model',
]);
assert.deepEqual(Object.keys(hermesSchema.properties?.acp?.properties ?? {}).sort(), [
  'access_mode',
  'additional_directories',
  'approval_mode',
  'auth',
  'config_overrides',
]);
assert.doesNotMatch(source, /hermes[_-]?(auth|token|api[_-]?key|config)/iu);
assert.match(
  source,
  /PI: \{[\s\S]*?title: "Pi"[\s\S]*?logoSrc: "\/logos\/pi-logo\.svg"/u,
);
assert.match(
  source,
  /QODER_CLI: \{[\s\S]*?title: "Qoder"[\s\S]*?logoSrc: "\/logos\/qoder-logo\.svg"/u,
);
assert.match(
  source,
  /agentRuntimeApi[\s\S]*?\.getDiagnostics\(runner\.runner_type, \{ workspacePath \}\)[\s\S]*?\[refreshRevision, runner\.runner_type, workspacePath\]/u,
);
assert.doesNotMatch(source, /\[\.\.\.models, selectedModel\]/u);
assert.match(
  source,
  /const handleRefreshConfig = async \(\) => \{[\s\S]*?await agentRuntimeApi\.refresh\(activeWorkspacePath\)[\s\S]*?setRefreshRevision/u,
);
const focusEffect = source.slice(
  source.indexOf('const handleWindowFocus'),
  source.indexOf('window.addEventListener("focus"'),
);
assert.match(focusEffect, /agentRuntimeApi\.refreshLight\(\)/u);
assert.doesNotMatch(focusEffect, /agentRuntimeApi\.refresh\(\)/u);
assert.doesNotMatch(focusEffect, /setRefreshRevision/u);
assert.match(source, /sidebarDiagnosticsStore\.get\(diagnosticsKey\)/u);
assert.match(
  source,
  /sidebarDiagnosticsStore\.set\(diagnosticsKey, result\)/u,
);
assert.match(
  source,
  /const activeWorkspacePath = useMemo\([\s\S]*?default_workspace_path\?\.trim\(\)/u,
);
assert.match(source, /workspacePath=\{activeWorkspacePath\}/u);
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
assert.match(apiSource, /\/api\/agents\/runtime\/refresh\$\{suffix\}/u);
assert.doesNotMatch(apiSource, /\/agents\/runtime\/\$\{[^}]+\}\/models/u);

// --- Pi agent surface -----------------------------------------------------

// The ACP model dropdown must pass probe values through verbatim so Pi model
// ids stay exactly what pi-acp advertised.
assert.match(
  source,
  /acpModelOption\.options\.map\(\(choice\) => \(\{[\s\S]*?id: choice\.value,/u,
);

// First Pi refresh can trigger a pinned-package download; the page surfaces
// that in-progress state while refreshing.
assert.match(
  source,
  /const piDownloadInProgress =[\s\S]*?refreshingConfig[\s\S]*?runner_type === "PI"/u,
);
assert.match(
  source,
  /\{piDownloadInProgress && \([\s\S]*?t\("agents\.refreshConfig\.piDownloadInProgress"\)/u,
);

// Pi provider/model sync failures surface as a separate, retryable warning
// fed by the backend `pi_models_sync` diagnostic.
assert.match(
  source,
  /setPiModelsSync\(response\.pi_models_sync\)/u,
);
assert.equal(
  source.match(/setPiModelsSync\(response\.pi_models_sync\)/gu)?.length,
  3,
);
assert.match(
  source,
  /const piModelsSyncWarning =[\s\S]*?!piModelsSync\.synchronized/u,
);
assert.match(source, /agentRuntimeApi\.retryPiModelsSync\(retryPath\)/u);
assert.match(source, /t\("agents\.piSync\.failedTitle"\)/u);
assert.match(source, /t\("agents\.piSync\.retry"\)/u);
assert.match(apiSource, /retryPiModelsSync/u);

// Pi reuses the generic member-level skill/MCP/ACP approval entries: no
// Pi-specific skill or MCP management UI may exist anywhere in the frontend.
assert.doesNotMatch(source, /pi[-_ ]?(skill|mcp)/iu);
const srcRoot = fileURLToPath(new URL('../', import.meta.url));
const collectFiles = (dir: string): string[] =>
  readdirSync(dir).flatMap((entry): string[] => {
    const full = join(dir, entry);
    return statSync(full).isDirectory() ? collectFiles(full) : [full];
  });
const piSkillMcpFiles = collectFiles(srcRoot).filter((file) =>
  /pi[-_]?(skill|mcp)|(skill|mcp)[-_]?pi/iu.test(file),
);
assert.deepEqual(piSkillMcpFiles, []);

console.log('AgentsPage runtime configuration: PASS');
