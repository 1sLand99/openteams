#!/usr/bin/env node
/**
 * Fake Hermes ACP: deterministic ACP server for offline Hermes fixture testing.
 *
 * Implements the Agent Client Protocol (JSON-RPC 2.0 over line-delimited stdio)
 * without contacting the network, a real Hermes CLI, user login state, or any
 * user-level Hermes configuration. Handles:
 *   - initialize / authenticate
 *   - session/new, session/load, session/resume
 *   - session/set_model
 *   - session/prompt (with streaming notifications, tool calls, permissions)
 *   - session/cancel (notification)
 *   - session/request_permission (agent -> client)
 *
 * The fake is launched directly as `hermes acp` (no npx/launcher chain), so it
 * records prompts, permission decisions, and protocol events to caller-supplied
 * paths for test verification. It MUST NOT embed real secrets or tokens.
 *
 * Modes selectable via environment variables:
 *   - OPENTEAMS_FAKE_HERMES_HANG=1: hang on session/prompt (cancel testing)
 *   - OPENTEAMS_FAKE_HERMES_ERROR=1: emit a provider error then end_turn
 *   - OPENTEAMS_FAKE_HERMES_PROBE_FAIL=1: fail initialize with an error
 *   - OPENTEAMS_FAKE_HERMES_NEEDS_SETUP=1: advertise setup without a provider
 *   - OPENTEAMS_FAKE_HERMES_SESSION_PROBE_FAIL=1: fail session/new metadata
 *   - OPENTEAMS_FAKE_HERMES_INITIALIZE_DELAY_MS=<ms>: delay initialize
 *   - OPENTEAMS_FAKE_HERMES_TOOL_CALL=<trigger>: emit a native tool call + permission
 *   - OPENTEAMS_FAKE_HERMES_MCP_TOOL_CALL=<trigger>: emit an MCP tool call + permission
 *   - OPENTEAMS_FAKE_HERMES_STALE_SESSION=1: return a unique stale session id from session/new
 *   - OPENTEAMS_FAKE_HERMES_STALE_RESUME_REJECT=1: reject session/load|resume for unknown
 *     session ids with invalid_params (-32602), modeling a stale/missing session
 */
import { appendFileSync, existsSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import readline from "node:readline";

const SESSION_ID = "hermes-offline-session";
const MODEL_ID = "openrouter:hermes-pro";
const MODEL_NAME = "Hermes Pro";
const SECOND_MODEL_ID = "nous:hermes-flash";
const SECOND_MODEL_NAME = "Hermes Flash";
const AGENT_NAME = "hermes-fake-acp";
const AGENT_VERSION = "0.0.1-fixture";

if (process.argv.slice(2).join(" ") === "--version") {
  process.stdout.write("Hermes Agent v0.20.0 (fixture)\n");
  process.exit(0);
}
if (process.argv.slice(2).join(" ") === "acp --version") {
  process.stdout.write(`${AGENT_VERSION}\n`);
  process.exit(0);
}

const promptsFile = process.env.OPENTEAMS_FAKE_HERMES_PROMPTS;
const permissionLog = process.env.OPENTEAMS_FAKE_HERMES_PERMISSION_LOG;
const protocolLog = process.env.OPENTEAMS_FAKE_HERMES_PROTOCOL_LOG;
const hangMode = process.env.OPENTEAMS_FAKE_HERMES_HANG === "1";
const errorMode = process.env.OPENTEAMS_FAKE_HERMES_ERROR === "1";
const probeFailMode = process.env.OPENTEAMS_FAKE_HERMES_PROBE_FAIL === "1";
const needsSetupMode = process.env.OPENTEAMS_FAKE_HERMES_NEEDS_SETUP === "1";
const sessionProbeFailMode = process.env.OPENTEAMS_FAKE_HERMES_SESSION_PROBE_FAIL === "1";
const initializeDelayMs = Number(process.env.OPENTEAMS_FAKE_HERMES_INITIALIZE_DELAY_MS || "0");
const toolCallTrigger = process.env.OPENTEAMS_FAKE_HERMES_TOOL_CALL;
const mcpToolCallTrigger = process.env.OPENTEAMS_FAKE_HERMES_MCP_TOOL_CALL;
const staleSessionMode = process.env.OPENTEAMS_FAKE_HERMES_STALE_SESSION === "1";
const staleResumeRejectMode =
  process.env.OPENTEAMS_FAKE_HERMES_STALE_RESUME_REJECT === "1";
const KNOWN_SESSION_IDS = new Set([SESSION_ID]);
const sessionMcpNames = new Map();

let cancelled = false;
let nextPermId = 2000;
let pendingPerm = null;

function logProtocol(event) {
  if (protocolLog) {
    try {
      mkdirSync(dirname(protocolLog), { recursive: true });
      appendFileSync(protocolLog, JSON.stringify(event) + "\n");
    } catch {}
  }
}

logProtocol({
  event: "process_start",
  argv: process.argv.slice(2),
  skip_configured_mcp: process.env.HERMES_ACP_SKIP_CONFIGURED_MCP,
});

function sleepMs(ms) {
  const start = Date.now();
  while (Date.now() - start < ms) {}
}

function makeLegacyModels(currentModel) {
  return {
    currentModelId: currentModel || MODEL_ID,
    availableModels: [
      { modelId: MODEL_ID, name: MODEL_NAME },
      { modelId: SECOND_MODEL_ID, name: SECOND_MODEL_NAME },
    ],
  };
}

function makeLegacyModes() {
  return {
    currentModeId: "default",
    availableModes: [
      { id: "default", name: "Default" },
      { id: "accept_edits", name: "Accept edits" },
      { id: "dont_ask", name: "Don't ask" },
    ],
  };
}

const send = (msg) => {
  process.stdout.write(JSON.stringify(msg) + "\n");
  logProtocol({ event: "send", method: msg.method, id: msg.id });
};
const respond = (id, result) => send({ jsonrpc: "2.0", id, result });
const notify = (method, params) => send({ jsonrpc: "2.0", method, params });

function sendAgentMessage(sessionId, text) {
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text },
      messageId: "hermes-message",
    },
  });
}

function sendHermesMetadataUpdates(sessionId) {
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "available_commands_update",
      availableCommands: [
        { name: "help", description: "Show Hermes fixture help" },
      ],
    },
  });
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "agent_thought_chunk",
      content: { type: "text", text: "fixture reasoning" },
      messageId: "hermes-thought",
    },
  });
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "plan",
      entries: [
        { content: "Complete the fixture turn", priority: "medium", status: "in_progress" },
      ],
    },
  });
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "session_info_update",
      title: "Hermes fixture session",
      updatedAt: "2026-08-09T00:00:00Z",
    },
  });
}

function sendUsageUpdate(sessionId) {
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "usage_update",
      used: 12,
      size: 180000,
    },
  });
}

function sendToolCallNotification(sessionId, toolName, toolCallId) {
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "tool_call",
      toolCallId: toolCallId || "tc-hermes",
      toolName: toolName || "bash",
      title: `Tool: ${toolName || "bash"}`,
    },
  });
}

function sendPermissionRequest(sessionId, toolName, toolCallId) {
  const reqId = nextPermId++;
  send({
    jsonrpc: "2.0",
    id: reqId,
    method: "session/request_permission",
    params: {
      sessionId,
      toolCall: {
        toolCallId: toolCallId || "tc-hermes",
        toolName: toolName || "bash",
        title: `Tool: ${toolName || "bash"}`,
      },
      options: [
        { optionId: "allow-once", name: "Allow once", kind: "allow_once" },
        { optionId: "reject-once", name: "Reject once", kind: "reject_once" },
      ],
    },
  });
  return reqId;
}

function mcpNames(params) {
  return (Array.isArray(params?.mcpServers) ? params.mcpServers : [])
    .map((server) => server?.name)
    .filter((name) => typeof name === "string" && name.length > 0);
}

function rememberSessionMcpNames(sessionId, params) {
  sessionMcpNames.set(sessionId, mcpNames(params));
}

function promptTag(text) {
  return text.match(/\[qa-tag:([A-Za-z0-9_-]+)\]/)?.[1] || "unknown";
}

function recordPermissionDecision(sessionId, toolName, promptText, outcome) {
  let decision = "unknown";
  if (outcome?.outcome === "selected") {
    decision = outcome.optionId?.includes("allow") ? "allowed" : "rejected";
  } else if (outcome?.outcome === "cancelled") {
    decision = "cancelled";
  }
  if (permissionLog) {
    try {
      mkdirSync(dirname(permissionLog), { recursive: true });
      appendFileSync(
        permissionLog,
        JSON.stringify({
          sessionId,
          promptTag: promptTag(promptText),
          toolName,
          decision,
          raw: outcome,
        }) + "\n"
      );
    } catch {}
  }
  logProtocol({
    event: "permission_decision",
    session_id: sessionId,
    prompt_tag: promptTag(promptText),
    tool_name: toolName,
    decision,
  });
  return decision;
}

function finalizePrompt(id, sid, text) {
  respond(id, {
    stopReason: cancelled ? "cancelled" : "end_turn",
    usage: {
      totalTokens: 42,
      inputTokens: 12,
      outputTokens: 24,
      thoughtTokens: 6,
    },
  });
}

function sendEcho(sessionId, text) {
  const mcp = sessionMcpNames.get(sessionId) || [];
  const content = `tag=${promptTag(text)}; permission=allowed; echo:${text}; mcp=${mcp.join(",")}`;
  sendHermesMetadataUpdates(sessionId);
  sendAgentMessage(
    sessionId,
    JSON.stringify([
      { type: "send", to: "you", intent: "reply", content },
    ]),
  );
}

function sendPermissionOutcome(sessionId, text, decision) {
  const mcp = sessionMcpNames.get(sessionId) || [];
  const content = `tag=${promptTag(text)}; permission=${decision}; mcp=${mcp.join(",")}`;
  sendAgentMessage(
    sessionId,
    JSON.stringify([
      { type: "send", to: "you", intent: "reply", content },
    ]),
  );
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  logProtocol({
    event: "recv",
    method: msg.method,
    id: msg.id,
    session_id: msg.params?.sessionId,
    mcp_servers: mcpNames(msg.params),
    prompt_tag: msg.params?.prompt
      ? promptTag(msg.params.prompt.find((block) => block.type === "text")?.text || "")
      : undefined,
    prompt_types: Array.isArray(msg.params?.prompt)
      ? msg.params.prompt.map((block) => block.type)
      : undefined,
  });

  if (pendingPerm && msg.id === pendingPerm.permId && msg.result !== undefined) {
    const { promptId, promptText, sessionId, toolName } = pendingPerm;
    pendingPerm = null;
    const decision = recordPermissionDecision(
      sessionId,
      toolName,
      promptText,
      msg.result?.outcome,
    );
    if (decision === "allowed") {
      sendEcho(sessionId, promptText);
    } else {
      sendPermissionOutcome(sessionId, promptText, decision);
    }
    sendUsageUpdate(sessionId);
    finalizePrompt(promptId, sessionId, promptText);
    return;
  }

  const { id, method, params } = msg;
  switch (method) {
    case "initialize": {
      if (initializeDelayMs > 0) {
        sleepMs(initializeDelayMs);
      }
      if (probeFailMode) {
        send({
          jsonrpc: "2.0",
          id,
          error: { code: -32603, message: "hermes probe forced failure" },
        });
        break;
      }
      logProtocol({
        event: "provider_state",
        auth_methods: needsSetupMode
          ? ["hermes-setup"]
          : ["hermes-setup", "openrouter"],
      });
      respond(id, {
        protocolVersion: 1,
        agentCapabilities: {
          loadSession: true,
          promptCapabilities: { image: true, audio: false, embeddedContext: false },
          mcpCapabilities: { http: false, sse: false },
          sessionCapabilities: {
            list: {},
            resume: {},
          },
          auth: {},
        },
        agentInfo: { name: AGENT_NAME, version: AGENT_VERSION },
        authMethods: needsSetupMode
          ? [
              {
                id: "hermes-setup",
                name: "Configure Hermes provider",
                description: "Run hermes acp --setup in a terminal",
              },
            ]
          : [
              {
                id: "hermes-setup",
                name: "Configure Hermes provider",
                description: "Run hermes acp --setup in a terminal",
              },
              {
                id: "openrouter",
                name: "OpenRouter",
                description: "Configured Hermes provider",
              },
            ],
      });
      break;
    }
    case "authenticate": {
      respond(id, {});
      break;
    }
    case "session/new": {
      if (sessionProbeFailMode) {
        send({
          jsonrpc: "2.0",
          id,
          error: { code: -32603, message: "Hermes provider metadata unavailable" },
        });
        break;
      }
      const sid = staleSessionMode
        ? `hermes-stale-${Date.now()}`
        : SESSION_ID;
      rememberSessionMcpNames(sid, params);
      respond(id, {
        sessionId: sid,
        models: makeLegacyModels(MODEL_ID),
        modes: makeLegacyModes(),
      });
      break;
    }
    case "session/load":
    case "session/resume": {
      const sid = params?.sessionId || SESSION_ID;
      if (
        staleResumeRejectMode &&
        !KNOWN_SESSION_IDS.has(sid)
      ) {
        send({
          jsonrpc: "2.0",
          id,
          error: {
            code: -32602,
            message: `session not found: ${sid}`,
          },
        });
        logProtocol({
          event: "stale_session_rejected",
          method,
          session_id: sid,
        });
        break;
      }
      rememberSessionMcpNames(sid, params);
      respond(id, {
        sessionId: sid,
        models: makeLegacyModels(MODEL_ID),
        modes: makeLegacyModes(),
      });
      break;
    }
    case "session/set_model": {
      respond(id, {});
      break;
    }
    case "session/prompt": {
      const text = params?.prompt?.find((b) => b.type === "text")?.text || "";
      const sid = params?.sessionId || SESSION_ID;
      if (promptsFile) {
        try {
          mkdirSync(dirname(promptsFile), { recursive: true });
          appendFileSync(promptsFile, text + "\n");
        } catch {}
      }
      if (hangMode || text.includes("[qa:sleep]")) {
        break;
      }
      if (
        (toolCallTrigger && text.includes(toolCallTrigger)) ||
        text.includes("[qa:approval]")
      ) {
        sendToolCallNotification(sid, "bash", "tc-hermes");
        const permId = sendPermissionRequest(sid, "bash", "tc-hermes");
        pendingPerm = {
          permId,
          promptId: id,
          promptText: text,
          sessionId: sid,
          toolName: "bash",
        };
        break;
      }
      if (mcpToolCallTrigger && text.includes(mcpToolCallTrigger)) {
        sendToolCallNotification(sid, "mcp__test__read", "tc-hermes-mcp");
        const permId = sendPermissionRequest(sid, "mcp__test__read", "tc-hermes-mcp");
        pendingPerm = {
          permId,
          promptId: id,
          promptText: text,
          sessionId: sid,
          toolName: "mcp__test__read",
        };
        break;
      }
      if (errorMode) {
        sendAgentMessage(sid, "Hermes provider connection failed.");
        respond(id, { stopReason: "end_turn" });
        break;
      }
      sendEcho(sid, text);
      sendUsageUpdate(sid);
      finalizePrompt(id, sid, text);
      break;
    }
    case "session/cancel": {
      cancelled = true;
      logProtocol({
        event: "cancel_received",
        method: "session/cancel",
        session_id: params?.sessionId,
      });
      break;
    }
    case "session/close":
    case "session/delete": {
      respond(id, {});
      break;
    }
    default: {
      if (id !== undefined) {
        respond(id, {});
      }
    }
  }
});

rl.on("close", () => {
  process.exit(0);
});
process.on("SIGTERM", () => {
  process.exit(0);
});
process.on("SIGINT", () => {
  process.exit(0);
});
