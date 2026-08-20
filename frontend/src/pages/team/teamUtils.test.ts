import assert from "node:assert/strict";
import type { AcpConfigOptionSnapshot } from "../../../../shared/types";
import type { BaseCodingAgent } from "@/types";
import {
  acpOptionSemanticCategory,
  canonicalRuntimeModelId,
  effectiveAcpConfigValue,
  findAcpSelectConfigOption,
  normalizeRunnerType,
  resolveUniqueAcpChoice,
  runnerSupportsAcp,
  withoutAcpModeOverrides,
  withoutAcpThoughtLevelOverrides,
} from "./teamUtils";

const modelOption: AcpConfigOptionSnapshot = {
  id: "session-model",
  name: "Model",
  description: null,
  category: null,
  type: "select",
  current_value: "gemini-3.1-pro-preview",
  options: [
    {
      value: "gpt-5.6-luna(openai)",
      name: "GPT 5.6 Luna",
      description: null,
    },
    {
      value: "gemini-2.5-flash",
      name: "Gemini 2.5 Flash",
      description: null,
    },
  ],
};

assert.equal(
  canonicalRuntimeModelId("gpt-5.6-luna(openai)[fast]"),
  "gpt-5.6-luna",
);
assert.equal(acpOptionSemanticCategory(modelOption), "model");
assert.equal(
  acpOptionSemanticCategory({
    ...modelOption,
    id: "session-mode",
    name: "Mode",
    category: "mode",
  }),
  null,
);
assert.equal(
  findAcpSelectConfigOption([modelOption], "model")?.id,
  "session-model",
);
assert.equal(
  resolveUniqueAcpChoice("gpt-5.6-luna", modelOption.options)?.value,
  "gpt-5.6-luna(openai)",
);
assert.deepEqual(
  effectiveAcpConfigValue(modelOption, [], "gemini-2.5-flash", ""),
  { type: "value_id", value: "gemini-2.5-flash" },
);

const ambiguous = [
  {
    value: "gpt-5.6-luna(openai)",
    name: "OpenAI",
    description: null,
  },
  {
    value: "gpt-5.6-luna(other)",
    name: "Other",
    description: null,
  },
];
assert.equal(resolveUniqueAcpChoice("gpt-5.6-luna", ambiguous), null);

const overrides = [
  {
    option_id: "session-mode",
    value: { type: "value_id" as const, value: "plan" },
    label_snapshot: "Mode",
    category_snapshot: "mode",
  },
  {
    option_id: "thought-level",
    value: { type: "value_id" as const, value: "high" },
    label_snapshot: "Thought Level",
    category_snapshot: "thought_level",
  },
  {
    option_id: "session-model",
    value: { type: "value_id" as const, value: "gemini-2.5-flash" },
    label_snapshot: "Model",
    category_snapshot: "model",
  },
];
assert.deepEqual(
  withoutAcpModeOverrides(overrides).map((override) => override.option_id),
  ["thought-level", "session-model"],
);
assert.deepEqual(
  withoutAcpThoughtLevelOverrides(overrides).map(
    (override) => override.option_id,
  ),
  ["session-mode", "session-model"],
);

// --- Pi runner registration and exact ACP model values -------------------

assert.equal(normalizeRunnerType("PI"), "PI");
assert.equal(normalizeRunnerType("pi"), "PI");
assert.equal(normalizeRunnerType("Pi"), "PI");

const piModelOption: AcpConfigOptionSnapshot = {
  ...modelOption,
  id: "model",
  current_value: "openai/gpt-5.3-codex(high)",
  options: [
    {
      value: "openai/gpt-5.3-codex(high)",
      name: "GPT 5.3 Codex (high)",
      description: null,
    },
    {
      value: "anthropic/claude-opus-4-6",
      name: "Claude Opus 4.6",
      description: null,
    },
  ],
};
assert.equal(
  resolveUniqueAcpChoice(
    "openai/gpt-5.3-codex(high)",
    piModelOption.options,
  )?.value,
  "openai/gpt-5.3-codex(high)",
);
assert.deepEqual(
  effectiveAcpConfigValue(piModelOption, [], "anthropic/claude-opus-4-6", ""),
  { type: "value_id", value: "anthropic/claude-opus-4-6" },
);

console.log("Team ACP config matching: PASS");

// --- Runner ACP support classification ---------------------------------

const acpRunners = [
  "DEEPSEEK_HARNESS",
  "GEMINI",
  "HERMES",
  "QWEN_CODE",
  "KIMI_CODE",
  "QODER_CLI",
  "PI",
];
const nonAcpRunners = [
  "CLAUDE_CODE",
  "AMP",
  "CODEX",
  "OPENCODE",
  "OPEN_TEAMS_CLI",
  "CURSOR_AGENT",
  "COPILOT",
  "DROID",
];

for (const runner of acpRunners) {
  assert.equal(
    runnerSupportsAcp(runner as BaseCodingAgent),
    true,
    `${runner} should support ACP`,
  );
}
for (const runner of nonAcpRunners) {
  assert.equal(
    runnerSupportsAcp(runner as BaseCodingAgent),
    false,
    `${runner} should not support ACP`,
  );
}

console.log("Runner ACP support classification: PASS");
