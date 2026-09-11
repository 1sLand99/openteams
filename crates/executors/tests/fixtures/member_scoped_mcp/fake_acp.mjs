#!/usr/bin/env node
/**
 * Generic offline ACP server for the seven ACP-stdio member-scoped MCP E2E
 * runners (GEMINI, KIRO_CLI, QWEN_CODE, KIMI_CODE, QODER_CLI, PI, HERMES).
 *
 * Implements the Agent Client Protocol (JSON-RPC 2.0 over line-delimited
 * stdio) without contacting the network, any real CLI, user login state, or
 * user-level configuration. OpenTeams' production ACP client drives this
 * process; this fixture only replaces the external CLI protocol.
 *
 * MCP isolation carrier: most ACP adapters receive the frozen member snapshot
 * through ACP session parameters. Kimi 0.38 cannot accept standard ACP stdio
 * entries, so its production adapter installs the same snapshot in a native,
 * member-scoped KIMI_CODE_HOME view and sends an empty ACP MCP list.
 *
 * Per-run control environment:
 *   FAKE_ACP_PROTOCOL_LOG    - JSONL protocol log (redacted of the fake secret)
 *   FAKE_ACP_FAKE_SECRET     - fixed fake secret used by redaction assertions
 *   FAKE_ACP_HANG=1          - hang on session/prompt (cancel/cleanup testing)
 *   FAKE_ACP_FAIL_INIT=1     - fail initialize with an error (probe validation)
 *   FAKE_ACP_FAIL_PROMPT=1   - fail an already-started prompt (cleanup testing)
 *   FAKE_ACP_RUNNER          - runner label echoed into the protocol log
 *   FAKE_ACP_NO_MCP=1        - never connect to MCP servers (empty-config proof)
 *   FAKE_ACP_STDIO_MCP_COMMAND - node binary used to launch the MCP server
 */
import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { spawn } from "node:child_process";
import readline from "node:readline";

const RUNNER = process.env.FAKE_ACP_RUNNER || "acp";
const SESSION_ID = `offline-${RUNNER}-session`;
const AGENT_NAME = `${RUNNER}-fake-acp`;
const AGENT_VERSION = "0.0.1-fixture";
const protocolLog = process.env.FAKE_ACP_PROTOCOL_LOG;
const fakeSecret = process.env.FAKE_ACP_FAKE_SECRET;
const hangMode = process.env.FAKE_ACP_HANG === "1";
const failInitMode = process.env.FAKE_ACP_FAIL_INIT === "1";
const failPromptMode = process.env.FAKE_ACP_FAIL_PROMPT === "1";
const noMcpMode = process.env.FAKE_ACP_NO_MCP === "1";
const snapshotPath = process.env.OPENTEAMS_ACP_MCP_SNAPSHOT_PATH || process.env.OPENTEAMS_PI_MCP_SNAPSHOT;
const nodeBin = process.env.FAKE_ACP_STDIO_MCP_COMMAND || "node";

let cancelled = false;
const isKimiRuntime = basename(process.argv[1] || "").replace(/\.exe$/i, "") === "kimi";

function redact(value) {
  if (!fakeSecret) return String(value);
  return String(value).replaceAll(fakeSecret, "[REDACTED]");
}

function logProtocol(event) {
  if (protocolLog) {
    try {
      mkdirSync(dirname(protocolLog), { recursive: true });
      appendFileSync(protocolLog, redact(JSON.stringify(event)) + "\n");
    } catch {}
  }
}

function inspectKimiRuntimeMcp() {
  if (!isKimiRuntime) {
    return {};
  }
  const runtimeHome = process.env.KIMI_CODE_HOME || join(process.env.HOME || "", ".kimi-code");
  try {
    const raw = JSON.parse(readFileSync(join(runtimeHome, "mcp.json"), "utf8"));
    const servers = raw?.mcpServers || {};
    logProtocol({
      event: "kimi_runtime_mcp_read",
      server_names: Object.keys(servers),
    });
    return servers;
  } catch (error) {
    logProtocol({ event: "kimi_runtime_mcp_read_error", error: redact(error.message) });
    return {};
  }
}

logProtocol({
  event: "process_start",
  runner: RUNNER,
  argv: process.argv.slice(2),
  snapshot_path: snapshotPath || "",
});
const kimiRuntimeServers = inspectKimiRuntimeMcp();

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}
const respond = (id, result) => send({ jsonrpc: "2.0", id, result });
const notify = (method, params) => send({ jsonrpc: "2.0", method, params });

function sendAgentMessage(sessionId, text) {
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "agent_message_chunk",
      content: { type: "text", text },
      messageId: `${RUNNER}-message`,
    },
  });
}

function sendUsageUpdate(sessionId) {
  notify("session/update", {
    sessionId,
    update: {
      sessionUpdate: "usage_update",
      used: 11,
      size: 150000,
    },
  });
}

const MODEL_IDS = [
  "offline-model",
  "gemini-2.5-flash",
  "gemini-2.5-flash-lite",
  "gemini-2.5-pro",
  "gemini-3-flash-preview",
  "gemini-3-pro-preview",
  "qwen3-coder-plus",
  "qwen3-coder-flash",
  "lite",
  "efficient",
  "auto",
  "performance",
  "ultimate",
  "moonshot-cn/kimi-k2.6,thinking",
  "moonshot-cn/kimi-k2.5,thinking",
  "moonshot-cn/kimi-k2.5",
  "kimi-code/kimi-for-coding,thinking",
  "kimi-code/kimi-for-coding",
  "moonshot-cn/kimi-k2.6",
];
let activeModel = MODEL_IDS[0];
let activeMode = "default";

function makeConfigOptions() {
  return [
    {
      id: "model",
      name: "Model",
      category: "model",
      type: "select",
      currentValue: activeModel,
      options: MODEL_IDS.map((value) => ({ value, name: value })),
    },
    {
      id: "mode",
      name: "Mode",
      category: "mode",
      type: "select",
      currentValue: activeMode,
      options: [{ value: "default", name: "Default" }],
    },
  ];
}

function mcpNamesFromParams(params) {
  return (Array.isArray(params?.mcpServers) ? params.mcpServers : [])
    .map((server) => server?.name)
    .filter((name) => typeof name === "string" && name.length > 0);
}

function rejectKimiAcpStdio(id, params) {
  if (
    !isKimiRuntime ||
    !(params?.mcpServers || []).some((server) => !("type" in server))
  ) {
    return false;
  }
  send({
    jsonrpc: "2.0",
    id,
    error: {
      code: -32603,
      message: "ACP stdio MCP server does not declare a runtime identity",
    },
  });
  return true;
}

function readSnapshotServers() {
  try {
    if (!snapshotPath) return {};
    const raw = JSON.parse(readFileSync(snapshotPath, "utf8"));
    const servers = raw?.mcpServers || {};
    logProtocol({ event: "snapshot_read", server_names: Object.keys(servers) });
    return servers;
  } catch (error) {
    logProtocol({ event: "snapshot_read_error", error: redact(error.message) });
    return {};
  }
}

function promptTag(text) {
  return text.match(/\[qa-tag:([A-Za-z0-9_-]+)\]/)?.[1] || "unknown";
}

function createJsonLineReader() {
  let buffer = "";
  return {
    push(data, onLine) {
      buffer += data.toString();
      let idx;
      while ((idx = buffer.indexOf("\n")) !== -1) {
        const line = buffer.slice(0, idx).trim();
        buffer = buffer.slice(idx + 1);
        if (line) onLine(line);
      }
    },
    drain(onLine) {
      if (buffer.trim()) {
        onLine(buffer.trim());
        buffer = "";
      }
    },
  };
}

async function runMcpHandshake(child, sendLine, serverName) {
  const reader = createJsonLineReader();
  const responses = new Map();
  const pending = new Map();
  const waiter = (id) =>
    new Promise((resolve) => {
      pending.set(id, resolve);
      setTimeout(() => {
        if (pending.has(id)) {
          pending.delete(id);
          resolve(null);
        }
      }, 4000);
    });
  child.stdout.on("data", (data) =>
    reader.push(data, (line) => {
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        return;
      }
      if (message.id !== undefined && pending.has(message.id)) {
        const resolve = pending.get(message.id);
        pending.delete(message.id);
        resolve(message);
      }
      responses.set(serverName, responses.get(serverName) || []);
      responses.get(serverName).push(line);
    })
  );
  return { waiter, responses };
}

/**
 * Connect to every configured stdio MCP server and record the result. This is
 * what makes the "connected to the local MCP server" assertion meaningful: the
 * fixture proves it reached the exact server the member config declared.
 */
async function connectToMcpServers(servers) {
  const results = [];
  for (const [name, definition] of Object.entries(servers || {})) {
    const record = { server: name, connected: false, tools: [] };
    try {
      if (noMcpMode) {
        logProtocol({ event: "mcp_skipped", server: name, reason: "no_mcp_mode" });
        continue;
      }
      const command = definition?.command;
      const args = Array.isArray(definition?.args) ? definition.args : [];
      const env = definition?.env || {};
      if (typeof command !== "string" || command.length === 0) {
        record.error = "missing command";
        logProtocol({ event: "mcp_skipped", server: name, reason: "missing command" });
        results.push(record);
        continue;
      }
      // Pass the declared environment verbatim: the MCP server (and only the
      // MCP server) legitimately needs it; every log path redacts the secret.
      const childEnv = { ...process.env };
      for (const [key, value] of Object.entries(env)) {
        childEnv[key] = String(value);
      }
      const child = spawn(command, args, {
        stdio: ["pipe", "pipe", "inherit"],
        env: childEnv,
      });
      const sendLine = (msg) => child.stdin.write(JSON.stringify(msg) + "\n");
      const { waiter } = await runMcpHandshake(child, sendLine, name);
      const initializeWaiter = waiter(1);
      sendLine({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2024-11-05",
          capabilities: {},
          clientInfo: { name: RUNNER, version: "1.0.0" },
        },
      });
      const initializeResponse = await initializeWaiter;
      let toolListResponse = null;
      if (initializeResponse && initializeResponse.result !== undefined) {
        sendLine({ jsonrpc: "2.0", method: "notifications/initialized" });
        const toolListWaiter = waiter(2);
        sendLine({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
        toolListResponse = await toolListWaiter;
        record.connected = true;
        if (toolListResponse?.result?.tools) {
          record.tools = toolListResponse.result.tools.map((tool) => tool.name);
        }
      } else if (initializeResponse && initializeResponse.error) {
        record.error = redact(initializeResponse.error.message);
      }
      // Gracefully close: the MCP server exits on stdin EOF.
      try {
        child.stdin.end();
      } catch {}
      logProtocol({ event: "mcp_connected", server: name, connected: record.connected, tools: record.tools, tool_list_ok: Boolean(toolListResponse) });
    } catch (error) {
      record.error = redact(error.message);
      logProtocol({ event: "mcp_connect_error", server: name, error: record.error });
    }
    results.push(record);
  }
  return results;
}

let connectedServers = [];

async function handlePrompt(id, params) {
  const text = params?.prompt?.find((block) => block.type === "text")?.text || "";
  const sid = params?.sessionId || SESSION_ID;
  logProtocol({ event: "prompt_received", prompt_tag: promptTag(text) });
  if (hangMode) {
    return;
  }
  const servers = isKimiRuntime ? kimiRuntimeServers : readSnapshotServers();
  connectedServers = await connectToMcpServers(servers);
  const names = connectedServers.filter((r) => r.connected).map((r) => r.server);
  if (failPromptMode) {
    send({
      jsonrpc: "2.0",
      id,
      error: { code: -32603, message: `${RUNNER} forced prompt failure` },
    });
    return;
  }
  const content = `tag=${promptTag(text)}; echo:${text}; mcp=${names.join(",")}; runner=${RUNNER}`;
  sendAgentMessage(sid, JSON.stringify([{ type: "send", to: "you", intent: "reply", content }]));
  sendUsageUpdate(sid);
  respond(id, {
    stopReason: cancelled ? "cancelled" : "end_turn",
    usage: { totalTokens: 30, inputTokens: 10, outputTokens: 20, thoughtTokens: 5 },
  });
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
    mcp_servers: mcpNamesFromParams(msg.params),
    prompt_tag: msg.params?.prompt
      ? promptTag(msg.params.prompt.find((block) => block.type === "text")?.text || "")
      : undefined,
  });

  const { id, method, params } = msg;
  switch (method) {
    case "initialize": {
      if (failInitMode) {
        send({
          jsonrpc: "2.0",
          id,
          error: { code: -32603, message: `${RUNNER} probe forced failure` },
        });
        break;
      }
      respond(id, {
        protocolVersion: 1,
        agentCapabilities: {
          loadSession: true,
          promptCapabilities: { image: true, audio: false, embeddedContext: false },
          mcpCapabilities: { http: false, sse: false },
          sessionCapabilities: { list: {}, resume: {} },
          auth: {},
        },
        agentInfo: { name: AGENT_NAME, version: AGENT_VERSION },
        authMethods: [
          { id: `${RUNNER}-offline`, name: "Offline", description: "Offline E2E auth" },
        ],
      });
      break;
    }
    case "authenticate": {
      respond(id, {});
      break;
    }
    case "session/new": {
      if (rejectKimiAcpStdio(id, params)) {
        break;
      }
      respond(id, {
        sessionId: SESSION_ID,
        configOptions: makeConfigOptions(),
      });
      break;
    }
    case "session/load":
    case "session/resume": {
      if (rejectKimiAcpStdio(id, params)) {
        break;
      }
      respond(id, {
        sessionId: params?.sessionId || SESSION_ID,
        configOptions: makeConfigOptions(),
      });
      break;
    }
    case "session/set_model": {
      activeModel = params?.modelId || params?.model || activeModel;
      respond(id, { configOptions: makeConfigOptions() });
      break;
    }
    case "session/set_config_option": {
      const optionId = params?.configId || params?.optionId || params?.id;
      const value = params?.value?.valueId || params?.value?.value || params?.value;
      if (optionId === "model" && typeof value === "string") activeModel = value;
      if (optionId === "mode" && typeof value === "string") activeMode = value;
      respond(id, { configOptions: makeConfigOptions() });
      break;
    }
    case "session/prompt": {
      void handlePrompt(id, params);
      break;
    }
    case "session/cancel": {
      cancelled = true;
      logProtocol({ event: "cancel_received", session_id: params?.sessionId });
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

rl.on("close", () => process.exit(0));
process.on("SIGTERM", () => process.exit(0));
process.on("SIGINT", () => process.exit(0));
