import assert from "node:assert/strict";
import type { AcpConfigOptionSnapshot } from "../../../../shared/types";
import {
  acpOptionSemanticCategory,
  canonicalRuntimeModelId,
  effectiveAcpConfigValue,
  findAcpSelectConfigOption,
  resolveUniqueAcpChoice,
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

console.log("Team ACP config matching: PASS");
