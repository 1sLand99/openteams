// Test-only fixture for @earendil-works/pi-coding-agent@0.83.0.
// Mirrors dist/core/resolve-config-value.js without loading the npm package.
// The command handler records command references instead of executing them.

import { readFileSync, writeFileSync } from "node:fs";

const ENV_VAR_NAME_RE = /^[A-Za-z_][A-Za-z0-9_]*$/;
const ENV_VAR_NAME_PREFIX_RE = /^[A-Za-z_][A-Za-z0-9_]*/;

function appendLiteral(parts, value) {
  if (!value) return;
  const previousPart = parts[parts.length - 1];
  if (previousPart?.type === "literal") {
    previousPart.value += value;
    return;
  }
  parts.push({ type: "literal", value });
}

function parseConfigValueTemplate(config) {
  const parts = [];
  let index = 0;
  while (index < config.length) {
    const dollarIndex = config.indexOf("$", index);
    if (dollarIndex < 0) {
      appendLiteral(parts, config.slice(index));
      break;
    }
    appendLiteral(parts, config.slice(index, dollarIndex));
    const nextChar = config[dollarIndex + 1];
    if (nextChar === "$" || nextChar === "!") {
      appendLiteral(parts, nextChar);
      index = dollarIndex + 2;
      continue;
    }
    if (nextChar === "{") {
      const endIndex = config.indexOf("}", dollarIndex + 2);
      if (endIndex < 0) {
        appendLiteral(parts, "$");
        index = dollarIndex + 1;
        continue;
      }
      const name = config.slice(dollarIndex + 2, endIndex);
      if (ENV_VAR_NAME_RE.test(name)) {
        parts.push({ type: "env", name });
      } else {
        appendLiteral(parts, config.slice(dollarIndex, endIndex + 1));
      }
      index = endIndex + 1;
      continue;
    }
    const match = config.slice(dollarIndex + 1).match(ENV_VAR_NAME_PREFIX_RE);
    if (match) {
      parts.push({ type: "env", name: match[0] });
      index = dollarIndex + 1 + match[0].length;
      continue;
    }
    appendLiteral(parts, "$");
    index = dollarIndex + 1;
  }
  return parts;
}

function resolveConfigValueUncached(config, env, executeCommand) {
  if (config.startsWith("!")) {
    return executeCommand(config.slice(1));
  }
  let resolved = "";
  for (const part of parseConfigValueTemplate(config)) {
    if (part.type === "literal") {
      resolved += part.value;
      continue;
    }
    const envValue = env[part.name];
    if (envValue === undefined) return undefined;
    resolved += envValue;
  }
  return resolved;
}

const [inputPath, outputPath] = process.argv.slice(2);
const values = JSON.parse(readFileSync(inputPath, "utf8"));
const commands = [];
const resolved = values.map((value) =>
  resolveConfigValueUncached(value, { ENV: "expanded-by-pi" }, (command) => {
    commands.push(command);
    return "unexpected-command-result";
  }),
);
writeFileSync(outputPath, JSON.stringify({ resolved, commands }), { mode: 0o600 });
