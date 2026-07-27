import assert from "node:assert/strict";
import type { AcpConfigOptionSnapshot } from "../../../../shared/types";
import {
  acpOptionSemanticCategory,
  canonicalRuntimeModelId,
  effectiveAcpConfigValue,
  resolveUniqueAcpChoice,
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

console.log("Team ACP config matching: PASS");
