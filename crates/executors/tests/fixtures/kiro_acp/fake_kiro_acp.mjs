#!/usr/bin/env node

import { appendFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import readline from "node:readline";

const SESSION_ID = "kiro-fixture-session";
const protocolLog = process.env.OPENTEAMS_FAKE_KIRO_PROTOCOL_LOG;
const apiKey = process.env.KIRO_API_KEY || "";
const hangPrompt = process.env.OPENTEAMS_FAKE_KIRO_HANG === "1";
const failPrompt = process.env.OPENTEAMS_FAKE_KIRO_PROMPT_ERROR === "1";
const failProbe = process.env.OPENTEAMS_FAKE_KIRO_PROBE_ERROR === "1";
const failSessionStart = process.env.OPENTEAMS_FAKE_KIRO_SESSION_ERROR === "1";

function logProtocol(entry) {
  if (!protocolLog) return;
  mkdirSync(dirname(protocolLog), { recursive: true });
  appendFileSync(protocolLog, `${JSON.stringify(entry)}\n`);
}

logProtocol({ event: "process_start", argv: process.argv.slice(2) });

const argv = process.argv.slice(2).join(" ");
if (argv === "--version") {
  process.stdout.write("Kiro CLI 2.20.1\n");
  process.exit(0);
}
if (argv === "whoami --format json") {
  const loggedIn = process.env.OPENTEAMS_FAKE_KIRO_LOCAL_LOGIN === "1";
  process.stdout.write(loggedIn ? '{"authenticated":true}\n' : "{}\n");
  process.exit(0);
}
if (argv !== "acp") {
  process.stderr.write(`unexpected fixture arguments: ${argv}\n`);
  process.exit(2);
}

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function respond(id, result) {
  send({ jsonrpc: "2.0", id, result });
}

function respondError(id, code, message) {
  send({ jsonrpc: "2.0", id, error: { code, message } });
}

function notify(method, params) {
  send({ jsonrpc: "2.0", method, params });
}

function mcpServerNames(params) {
  return Array.isArray(params?.mcpServers)
    ? params.mcpServers.map((server) => server?.name).filter(Boolean)
    : [];
}

function mcpSecretValues(params) {
  if (!Array.isArray(params?.mcpServers)) return [];
  return params.mcpServers.flatMap((server) => [
    ...(Array.isArray(server?.env)
      ? server.env.map((variable) => variable?.value)
      : []),
    ...(Array.isArray(server?.headers)
      ? server.headers.map((header) => header?.value)
      : []),
  ]).filter(Boolean);
}

async function writeSecretsAcrossStderrReads(secrets) {
  for (const [secretIndex, secret] of secrets.entries()) {
    const split = Math.floor(secret.length / 2);
    logProtocol({ event: "stderr_secret_chunk", secret_index: secretIndex, part: 1 });
    process.stderr.write(`fixture stderr secret=${secret.slice(0, split)}`);
    await new Promise((resolve) => setTimeout(resolve, 10));
    logProtocol({ event: "stderr_secret_chunk", secret_index: secretIndex, part: 2 });
    process.stderr.write(`${secret.slice(split)}\n`);
  }
}

function legacyModels() {
  return {
    currentModelId: "auto",
    availableModels: [{ modelId: "auto", name: "Auto" }],
  };
}

function legacyModes() {
  return {
    currentModeId: "kiro_default",
    availableModes: [{ id: "kiro_default", name: "Kiro" }],
  };
}

function sendStandardKiroUpdates(sessionId, promptText, mcpSecrets) {
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "user_message_chunk",
      content: { type: "text", text: promptText },
      messageId: "kiro-user-message",
    },
  });
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "agent_thought_chunk",
      content: { type: "text", text: "fixture thought" },
      messageId: "kiro-thought",
    },
  });
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "agent_message_chunk",
      content: {
        type: "text",
        text: `fixture reply; apiKey=${apiKey}; mcp=${mcpSecrets.join("|")}`,
      },
      messageId: "kiro-agent-message",
    },
  });
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "tool_call",
      toolCallId: "kiro-tool-call",
      toolName: "read",
      title: "Read fixture",
      rawInput: { path: "fixture.txt" },
    },
  });
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "tool_call_update",
      toolCallId: "kiro-tool-call",
      status: "completed",
      rawOutput: { apiKey, mcpSecrets },
    },
  });
  notify("_kiro.dev/session/update", {
    sessionId,
    update: { type: "tool_call_chunk", ignored: true },
  });
}

let pendingPrompt;
let activeMcpSecrets = [];
const input = readline.createInterface({ input: process.stdin });
input.on("line", async (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }

  const { id, method, params } = message;
  const promptText = Array.isArray(params?.prompt)
    ? params.prompt.find((block) => block?.type === "text")?.text || ""
    : "";
  logProtocol({
    event: "receive",
    method,
    has_id: Object.hasOwn(message, "id"),
    protocol_version: params?.protocolVersion,
    session_id: params?.sessionId,
    mcp_servers: mcpServerNames(params),
    has_mcp_servers: Array.isArray(params?.mcpServers),
    prompt_text: promptText || undefined,
    prompt_types: Array.isArray(params?.prompt)
      ? params.prompt.map((block) => block?.type)
      : undefined,
    has_prompt: Array.isArray(params?.prompt),
    has_content: Object.hasOwn(params || {}, "content"),
  });

  switch (method) {
    case "initialize":
      if (failProbe) {
        respondError(id, -32000, `fixture probe authentication failure ${apiKey}`);
        break;
      }
      respond(id, {
        protocolVersion: 1,
        agentCapabilities: {
          loadSession: true,
          promptCapabilities: {
            image: true,
            audio: false,
            embeddedContext: false,
          },
          mcpCapabilities: { http: true, sse: false },
        },
        agentInfo: { name: "Kiro CLI Agent", version: "2.20.1" },
        authMethods: [],
      });
      break;
    case "session/new":
      activeMcpSecrets = mcpSecretValues(params);
      if (failSessionStart) {
        respondError(
          id,
          -32602,
          `fixture session failure ${apiKey}|${activeMcpSecrets.join("|")}`,
        );
        break;
      }
      respond(id, {
        sessionId: SESSION_ID,
        models: legacyModels(),
        modes: legacyModes(),
      });
      break;
    case "session/load":
      activeMcpSecrets = mcpSecretValues(params);
      respond(id, { models: legacyModels(), modes: legacyModes() });
      break;
    case "session/set_model":
      respond(id, {});
      break;
    case "session/prompt":
      pendingPrompt = { id, sessionId: params?.sessionId || SESSION_ID };
      await writeSecretsAcrossStderrReads([apiKey, ...activeMcpSecrets]);
      if (failPrompt) {
        send({
          jsonrpc: "2.0",
          id,
          error: {
            code: -32603,
            message: `fixture prompt failure ${apiKey}|${activeMcpSecrets.join("|")}`,
          },
        });
        pendingPrompt = undefined;
      } else if (!hangPrompt) {
        sendStandardKiroUpdates(
          pendingPrompt.sessionId,
          promptText,
          activeMcpSecrets,
        );
        respond(id, { stopReason: "end_turn" });
        pendingPrompt = undefined;
      }
      break;
    case "session/cancel":
      if (pendingPrompt) {
        respond(pendingPrompt.id, { stopReason: "cancelled" });
        pendingPrompt = undefined;
      }
      break;
    default:
      if (id !== undefined) respond(id, {});
  }
});

input.on("close", () => process.exit(0));
process.on("SIGTERM", () => process.exit(0));
process.on("SIGINT", () => process.exit(0));
