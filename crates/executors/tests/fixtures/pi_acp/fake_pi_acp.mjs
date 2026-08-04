#!/usr/bin/env node
/**
 * Fake pi-acp: deterministic ACP server for offline Pi fixture testing.
 *
 * Implements the Agent Client Protocol (JSON-RPC 2.0 over line-delimited stdio)
 * without contacting npm or any external service. Handles:
 *   - initialize / authenticate
 *   - session/new, session/load, session/resume
 *   - session/set_config_option
 *   - session/prompt (with streaming notifications, tool calls, permissions)
 *   - session/cancel (notification)
 *   - session/request_permission (agent -> client)
 *
 * Launches the real Pi launcher (via PI_ACP_PI_COMMAND) which starts the
 * fake Pi binary and fake MCP adapter. Records real PIDs, startup parameters,
 * and protocol events for test verification.
 */
import { spawn } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import readline from "node:readline";

const SESSION_ID = "pi-offline-session";
const MODEL_ID = "offline-model";
const MODEL_NAME = "Offline Model";

const promptsFile = process.env.OPENTEAMS_FAKE_PI_PROMPTS;
const pidFile = process.env.OPENTEAMS_FAKE_PI_CHILD_PID_FILE;
const permissionLog = process.env.OPENTEAMS_FAKE_PI_PERMISSION_LOG;
const protocolLog = process.env.OPENTEAMS_FAKE_PI_PROTOCOL_LOG;
const hangMode = process.env.OPENTEAMS_FAKE_PI_HANG === "1";
const toolCallTrigger = process.env.OPENTEAMS_FAKE_PI_TOOL_CALL;
const mcpToolCallTrigger = process.env.OPENTEAMS_FAKE_PI_MCP_TOOL_CALL;

let cancelled = false;
let launcherChild = null;
let launcherStderr = [];
let realPids = null;

let nextPermId = 1000;
let pendingPerm = null;

function logProtocol(event) {
  if (protocolLog) {
    try { appendFileSync(protocolLog, JSON.stringify(event) + "\n"); } catch {}
  }
}

function sleepMs(ms) {
  const start = Date.now();
  while (Date.now() - start < ms) {}
}

function launchLauncher() {
  const cmd = process.env.PI_ACP_PI_COMMAND;
  if (!cmd) {
    logProtocol({ event: "launcher_skip", reason: "no PI_ACP_PI_COMMAND" });
    return;
  }
  try {
    launcherChild = spawn(cmd, [], {
      stdio: ["ignore", "ignore", "pipe"],
      env: { ...process.env },
      detached: false,
    });
    launcherChild.stderr?.on("data", (data) => {
      const text = data.toString();
      launcherStderr.push(text);
      logProtocol({ event: "launcher_stderr", text });
    });
    launcherChild.on("error", (err) => {
      launcherStderr.push(`spawn error: ${err.message}`);
      logProtocol({ event: "launcher_error", error: err.message });
    });
    launcherChild.on("exit", (code, signal) => {
      logProtocol({ event: "launcher_exit", code, signal });
    });
    logProtocol({ event: "launcher_started", pid: launcherChild.pid, cmd });
  } catch (err) {
    launcherStderr.push(`launch error: ${err.message}`);
    logProtocol({ event: "launcher_catch", error: err.message });
  }
}

function waitForPidFile(maxMs) {
  if (!pidFile) return null;
  const start = Date.now();
  while (Date.now() - start < maxMs) {
    if (existsSync(pidFile)) {
      try {
        const data = readFileSync(pidFile, "utf8");
        const pids = JSON.parse(data);
        logProtocol({ event: "pid_file_read", pids });
        return pids;
      } catch (err) {
        logProtocol({ event: "pid_file_parse_error", error: err.message });
      }
    }
    sleepMs(50);
  }
  logProtocol({
    event: "pid_file_timeout",
    stderr: launcherStderr.join(""),
  });
  return null;
}

function killLauncher() {
  if (launcherChild) {
    try { launcherChild.kill("SIGTERM"); } catch {}
    try { launcherChild.kill("SIGKILL"); } catch {}
    launcherChild = null;
  }
}

function makeConfigOptions(currentModel) {
  return [{
    id: "session-model",
    name: "Model",
    category: "model",
    type: "select",
    currentValue: currentModel || MODEL_ID,
    options: [{ value: MODEL_ID, name: MODEL_NAME }],
  }];
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
      messageId: "pi-message",
    },
  });
}

function sendUsageUpdate(sessionId) {
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "usage_update",
      used: 10,
      size: 200000,
    },
  });
}

function sendToolCallNotification(sessionId, toolName, toolCallId) {
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "tool_call",
      toolCallId: toolCallId || "tc-fake",
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
        toolCallId: toolCallId || "tc-fake",
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

function recordPermissionDecision(toolName, outcome) {
  let decision = "unknown";
  if (outcome?.outcome === "selected") {
    decision = outcome.optionId?.includes("allow") ? "allowed" : "rejected";
  } else if (outcome?.outcome === "cancelled") {
    decision = "cancelled";
  }
  if (permissionLog) {
    try {
      appendFileSync(permissionLog, JSON.stringify({ toolName, decision, raw: outcome }) + "\n");
    } catch {}
  }
  logProtocol({ event: "permission_decision", toolName, decision });
}

launchLauncher();
realPids = waitForPidFile(3000);

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  logProtocol({ event: "recv", method: msg.method, id: msg.id });

  // Check if this is a response to a pending permission request
  if (pendingPerm && msg.id === pendingPerm.permId && msg.result !== undefined) {
    const { promptId, promptText, sessionId, toolName } = pendingPerm;
    pendingPerm = null;
    recordPermissionDecision(toolName, msg.result?.outcome);
    sendAgentMessage(sessionId, `echo:${promptText}`);
    sendUsageUpdate(sessionId);
    respond(promptId, {
      stopReason: cancelled ? "cancelled" : "end_turn",
      usage: { totalTokens: 35, inputTokens: 10, outputTokens: 20, thoughtTokens: 5 },
    });
    return;
  }

  const { id, method, params } = msg;
  switch (method) {
    case "initialize": {
      respond(id, {
        protocolVersion: 1,
        agentCapabilities: {
          loadSession: true,
          promptCapabilities: { image: true, audio: false, embeddedContext: false },
          mcpCapabilities: { http: false, sse: false },
          sessionCapabilities: {
            resume: {}, close: {}, delete: {}, additionalDirectories: {},
          },
          auth: {},
        },
        agentInfo: { name: "pi-fake-acp", version: "0.0.33" },
        authMethods: [],
      });
      break;
    }
    case "authenticate": { respond(id, {}); break; }
    case "session/new": {
      respond(id, { sessionId: SESSION_ID, configOptions: makeConfigOptions(MODEL_ID) });
      break;
    }
    case "session/load":
    case "session/resume": {
      respond(id, {
        sessionId: params?.sessionId || SESSION_ID,
        configOptions: makeConfigOptions(MODEL_ID),
      });
      break;
    }
    case "session/set_config_option": {
      const valueId = params?.value?.valueId || MODEL_ID;
      respond(id, { configOptions: makeConfigOptions(valueId) });
      break;
    }
    case "session/prompt": {
      const text = params?.prompt?.find((b) => b.type === "text")?.text || "";
      const sid = params?.sessionId || SESSION_ID;
      if (promptsFile) { appendFileSync(promptsFile, text + "\n"); }
      if (hangMode) { break; }
      if (toolCallTrigger && text.includes(toolCallTrigger)) {
        sendToolCallNotification(sid, "bash", "tc-fake");
        const permId = sendPermissionRequest(sid, "bash", "tc-fake");
        pendingPerm = { permId, promptId: id, promptText: text, sessionId: sid, toolName: "bash" };
        break;
      }
      if (mcpToolCallTrigger && text.includes(mcpToolCallTrigger)) {
        sendToolCallNotification(sid, "mcp__test__read", "tc-mcp");
        const permId = sendPermissionRequest(sid, "mcp__test__read", "tc-mcp");
        pendingPerm = { permId, promptId: id, promptText: text, sessionId: sid, toolName: "mcp__test__read" };
        break;
      }
      sendAgentMessage(sid, `echo:${text}`);
      sendUsageUpdate(sid);
      respond(id, {
        stopReason: cancelled ? "cancelled" : "end_turn",
        usage: { totalTokens: 35, inputTokens: 10, outputTokens: 20, thoughtTokens: 5 },
      });
      break;
    }
    case "session/cancel": { cancelled = true; break; }
    case "session/close":
    case "session/delete": { respond(id, {}); break; }
    default: { if (id !== undefined) { respond(id, {}); } }
  }
});

rl.on("close", () => { killLauncher(); process.exit(0); });
process.on("SIGTERM", () => { killLauncher(); process.exit(0); });
process.on("SIGINT", () => { killLauncher(); process.exit(0); });
