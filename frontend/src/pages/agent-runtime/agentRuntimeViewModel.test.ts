// Lightweight runtime state tests. Run with:
//     pnpm exec tsx src/pages/agent-runtime/agentRuntimeViewModel.test.ts

import type { AgentRuntimeStatus } from "@/types";
import {
  AGENT_RUNTIME_EDITABLE_FIELDS,
  buildLocalMachineSummary,
  envSummaryToText,
  filterRuntimeRunners,
  getRunnerLabel,
  getRuntimeDisplayState,
  parseEnvText,
  parseRuntimeErrorDetails,
  RUNTIME_TOOL_LABELS,
} from "./agentRuntimeViewModel";

let failures = 0;

const check = (label: string, condition: boolean, detail?: unknown) => {
  if (condition) {
    // eslint-disable-next-line no-console
    console.log(`  ok  ${label}`);
    return;
  }
  failures += 1;
  // eslint-disable-next-line no-console
  console.error(`  FAIL ${label}`, detail ?? "");
};

const same = (actual: unknown, expected: unknown) =>
  JSON.stringify(actual) === JSON.stringify(expected);

const baseRunner = {
  runner_type: "CODEX",
  installed: true,
  executable: true,
  availability: { type: "INSTALLATION_FOUND" },
  auth_state: "authenticated",
  node_available: true,
  npm_available: true,
  npx_available: true,
  discovered_models: ["gpt-5.2-codex"],
  model_source: "runner",
  version: "1.2.3",
  last_checked_at: "2026-06-02T00:00:00Z",
  last_error: null,
  run_mode: "auto",
  env_summary: [{ key: "OPENAI_API_KEY", value: "sk-live-test" }],
  executor_options: {},
} satisfies AgentRuntimeStatus;

const runners = [
  baseRunner,
  {
    ...baseRunner,
    runner_type: "GEMINI",
    installed: true,
    executable: false,
    discovered_models: [],
    last_error: "command not found",
  },
  {
    ...baseRunner,
    runner_type: "QWEN_CODE",
    installed: false,
    executable: false,
    availability: { type: "NOT_FOUND" },
    discovered_models: [],
    version: null,
    env_summary: [],
  },
] satisfies AgentRuntimeStatus[];

console.log("Agent runtime view model");

check(
  "classifies runtime states",
  same(
    runners.map((runner) => getRuntimeDisplayState(runner)),
    ["available", "error", "not_installed"],
  ),
);
check(
  "filters by query and status",
  same(
    filterRuntimeRunners(runners, "codex", "available").map(
      (runner) => runner.runner_type,
    ),
    ["CODEX"],
  ),
);
check(
  "filters error runners",
  same(
    filterRuntimeRunners(runners, "", "error").map(
      (runner) => runner.runner_type,
    ),
    ["GEMINI"],
  ),
);
check(
  "filters by discovered model name",
  same(
    filterRuntimeRunners(runners, "gpt-5.2-codex", "all").map(
      (runner) => runner.runner_type,
    ),
    ["CODEX"],
  ),
);
check(
  "builds local machine summary",
  same(buildLocalMachineSummary(runners), {
    name: "Localhost",
    total: 3,
    online: 1,
    errors: 1,
    notInstalled: 1,
    workloadLabel: "2 env keys configured",
  }),
);
check(
  "renders env summaries with raw values",
  envSummaryToText([
    { key: "OPENAI_API_KEY", value: "sk-live-test" },
    { key: "DEBUG", value: "true" },
  ]) === "OPENAI_API_KEY=sk-live-test\nDEBUG=true",
);
check(
  "parses complete env text",
  same(parseEnvText("OPENAI_API_KEY=secret\nEMPTY_VALUE=\nURL=a=b"), {
    ok: true,
    value: {
      OPENAI_API_KEY: "secret",
      EMPTY_VALUE: "",
      URL: "a=b",
    },
  }),
);
check(
  "rejects incomplete env lines",
  same(parseEnvText("OPENAI_API_KEY=secret\nHTTP_PROXY"), {
    ok: false,
    error: { line: 2, code: "missing_equals" },
  }),
);
check(
  "rejects invalid env keys",
  same(parseEnvText("export HTTP_PROXY=http://localhost:7890"), {
    ok: false,
    error: { line: 1, code: "invalid_key" },
  }),
);
check(
  "rejects empty env keys",
  same(parseEnvText("=http://localhost:7890"), {
    ok: false,
    error: { line: 1, code: "invalid_key" },
  }),
);
check(
  "editable fields exclude unsupported model-tuning controls",
  same(AGENT_RUNTIME_EDITABLE_FIELDS, [
    "run_mode",
    "env_json",
    "executor_options",
  ]),
);

check(
  "parses staged runtime error lines",
  same(
    parseRuntimeErrorDetails(
      "[model_discovery] provider unavailable\n[version_check] executable not found",
    ),
    [
      { stage: "model_discovery", message: "provider unavailable" },
      { stage: "version_check", message: "executable not found" },
    ],
  ),
);
check(
  "keeps unstaged runtime error lines verbatim",
  same(parseRuntimeErrorDetails("spawn claude failed"), [
    { stage: null, message: "spawn claude failed" },
  ]),
);
check(
  "empty runtime error yields no detail rows",
  same(parseRuntimeErrorDetails(null), []) &&
    same(parseRuntimeErrorDetails("  \n "), []),
);

// --- Pi runtime states -------------------------------------------------

const piAcpModels = ["openai/gpt-5.3-codex(high)", "anthropic/claude-opus-4-6"];
const piAvailable: AgentRuntimeStatus = {
  ...baseRunner,
  runner_type: "PI",
  discovered_models: piAcpModels,
};

check(
  "pi with node is available without a global install",
  getRuntimeDisplayState(piAvailable) === "available",
);
const piWithoutNode: AgentRuntimeStatus = {
  ...piAvailable,
  installed: false,
  executable: false,
  availability: { type: "NOT_FOUND" },
  node_available: false,
};
check(
  "pi without node is not installed",
  getRuntimeDisplayState(piWithoutNode) === "not_installed",
);
const piProbeFailed: AgentRuntimeStatus = {
  ...piAvailable,
  last_error: "[model_discovery] ACP initialize failed: probe timed out",
};
check(
  "pi ACP probe failure keeps it installed but surfaces an error",
  piProbeFailed.installed && getRuntimeDisplayState(piProbeFailed) === "error",
);
check(
  "pi layered error keeps the model discovery stage",
  same(parseRuntimeErrorDetails(piProbeFailed.last_error), [
    {
      stage: "model_discovery",
      message: "ACP initialize failed: probe timed out",
    },
  ]),
);
check(
  "pi model filter matches the exact ACP probe values",
  same(
    filterRuntimeRunners([piAvailable], "openai/gpt-5.3-codex(high)", "all").map(
      (runner) => runner.runner_type,
    ),
    ["PI"],
  ),
);
check(
  "getRunnerLabel renders Pi",
  getRunnerLabel("PI") === "Pi",
);
check(
  "getRunnerLabel renders Hermes",
  getRunnerLabel("HERMES") === "Hermes",
);
check(
  "getRunnerLabel preserves the DeepSeek brand spelling",
  getRunnerLabel("DEEPSEEK_HARNESS") === "DeepSeek Harness",
);

check(
  "getRunnerLabel keeps the Qoder CLI acronym uppercase",
  getRunnerLabel("QODER_CLI") === "Qoder CLI",
);
check(
  "getRunnerLabel keeps the Kiro CLI acronym uppercase",
  getRunnerLabel("KIRO_CLI") === "Kiro CLI",
);
check(
  "getRunnerLabel title-cases other runners",
  getRunnerLabel("KIMI_CODE") === "Kimi Code",
);

// --- Missing npm/npx dependency display -----------------------------------

check(
  "runtime tool labels render Node.js/npm/npx",
  same(RUNTIME_TOOL_LABELS, { node: "Node.js", npm: "npm", npx: "npx" }),
);

const geminiMissingNpm: AgentRuntimeStatus = {
  ...baseRunner,
  runner_type: "GEMINI",
  executable: false,
  npm_available: false,
};
check(
  "npm executor without npm stays installed but shows an error",
  geminiMissingNpm.installed &&
    getRuntimeDisplayState(geminiMissingNpm) === "error",
);

const claudeMissingNpx: AgentRuntimeStatus = {
  ...baseRunner,
  runner_type: "CLAUDE_CODE",
  executable: false,
  npx_available: false,
};
check(
  "npx executor without npx stays installed but shows an error",
  claudeMissingNpx.installed &&
    getRuntimeDisplayState(claudeMissingNpx) === "error",
);
check(
  "missing-dependency runners surface through the error filter",
  same(
    filterRuntimeRunners([geminiMissingNpm, claudeMissingNpx], "", "error").map(
      (runner) => runner.runner_type,
    ),
    ["GEMINI", "CLAUDE_CODE"],
  ),
);

if (failures > 0) {
  // eslint-disable-next-line no-console
  console.error(`\n${failures} assertion(s) FAILED`);
  process.exit(1);
}

// eslint-disable-next-line no-console
console.log("\nAll agent runtime view model assertions passed.");