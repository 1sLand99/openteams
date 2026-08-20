#!/usr/bin/env node
/**
 * Offline Codex app-server (JSON-RPC 2.0 over line-delimited stdio).
 *
 * The production Codex executor spawns `codex app-server` with a frozen
 * command and drives the run over the codex-app-server JSON-RPC protocol on
 * the process's stdio. This fixture implements the server side of that
 * protocol without contacting the network, a real Codex binary, user login
 * state, or user-level Codex configuration.
 *
 * Protocol handled for the production AppServerClient:
 *   initialize -> InitializeResponse
 *   initialized (notification)
 *   getAuthStatus -> requiresOpenaiAuth:false
 *   thread/start -> ThreadStartResponse (captures config.mcp_servers)
 *   turn/start  -> TurnStartResponse
 *
 * MCP isolation carrier: the production adapter freezes the member snapshot
 * into the frozen thread-start params (config.mcp_servers). This fixture
 * reads those params, spawns each configured stdio MCP server (the local
 * offline mcp-server), performs initialize + tools/list, and records every
 * server it connected to.
 *
 * Per-run control environment:
 *   FAKE_CODEX_PROTOCOL_LOG   - JSONL protocol log (redacted of the fake secret)
 *   FAKE_CODEX_FAKE_SECRET    - fixed fake secret used by redaction assertions
 *   FAKE_CODEX_HANG=1         - hang on turn/start (cancel/cleanup testing)
 *   FAKE_CODEX_FAIL_INIT=1    - fail initialize with an error (probe validation)
 *   FAKE_CODEX_FAIL_TURN=1    - fail an already-started turn (cleanup testing)
 *   FAKE_CODEX_NO_MCP=1       - never connect to MCP servers
 *   FAKE_CODEX_STDIO_MCP_COMMAND - node binary used to launch the MCP server
 */
import { appendFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { spawn } from "node:child_process";
import readline from "node:readline";

const protocolLog = process.env.FAKE_CODEX_PROTOCOL_LOG;
const fakeSecret = process.env.FAKE_CODEX_FAKE_SECRET;
const hangMode = process.env.FAKE_CODEX_HANG === "1";
const failInitMode = process.env.FAKE_CODEX_FAIL_INIT === "1";
const failTurnMode = process.env.FAKE_CODEX_FAIL_TURN === "1";
const noMcpMode = process.env.FAKE_CODEX_NO_MCP === "1";
const nodeBin = process.env.FAKE_CODEX_STDIO_MCP_COMMAND || "node";

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

logProtocol({ event: "process_start", argv: process.argv.slice(2) });

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}
const respond = (id, result) => send({ jsonrpc: "2.0", id, result });

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
  };
}

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
        logProtocol({ event: "mcp_skipped", server: name, reason: "missing command" });
        results.push(record);
        continue;
      }
      const childEnv = { ...process.env };
      for (const [key, value] of Object.entries(env)) {
        childEnv[key] = String(value);
      }
      const child = spawn(command, args, { stdio: ["pipe", "pipe", "inherit"], env: childEnv });
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
      const reader = createJsonLineReader();
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
        })
      );
      const sendLine = (msg) => child.stdin.write(JSON.stringify(msg) + "\n");
      const initializeWaiter = waiter(1);
      sendLine({
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2024-11-05",
          capabilities: {},
          clientInfo: { name: "codex-fixture", version: "1.0.0" },
        },
      });
      const initializeResponse = await initializeWaiter;
      if (initializeResponse && initializeResponse.result !== undefined) {
        sendLine({ jsonrpc: "2.0", method: "notifications/initialized" });
        const toolListWaiter = waiter(2);
        sendLine({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
        const toolListResponse = await toolListWaiter;
        record.connected = true;
        if (toolListResponse?.result?.tools) {
          record.tools = toolListResponse.result.tools.map((tool) => tool.name);
        }
      }
      try {
        child.stdin.end();
      } catch {}
      logProtocol({
        event: "mcp_connected",
        server: name,
        connected: record.connected,
        tools: record.tools,
      });
    } catch (error) {
      record.error = redact(error.message);
      logProtocol({ event: "mcp_connect_error", server: name, error: record.error });
    }
    results.push(record);
  }
  return results;
}

let threadMcpServers = {};
let threadConfig = null;

function threadResponse(threadId = "thread-1") {
  return {
    thread: {
      id: threadId,
      sessionId: "session-1",
      preview: "",
      ephemeral: false,
      modelProvider: "offline",
      createdAt: 0,
      updatedAt: 0,
      status: { type: "idle" },
      cwd: process.cwd(),
      cliVersion: "0.147.0",
      source: "appServer",
      turns: [],
    },
    model: "offline-model",
    modelProvider: "offline",
    cwd: process.cwd(),
    approvalPolicy: "never",
    approvalsReviewer: "user",
    sandbox: { type: "dangerFullAccess" },
  };
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let msg;
  try {
    msg = JSON.parse(trimmed);
  } catch {
    logProtocol({ event: "non_json_line" });
    return;
  }
  logProtocol({ event: "recv", method: msg.method, id: msg.id });

  if (msg.id === undefined) {
    return; // notification
  }

  const { id, method, params } = msg;
  switch (method) {
    case "initialize": {
      if (failInitMode) {
        send({ jsonrpc: "2.0", id, error: { code: -32603, message: "codex probe forced failure" } });
        break;
      }
      respond(id, {
        userAgent: "codex-offline-fixture",
        codexHome: process.env.CODEX_HOME || "/tmp/offline-codex-home",
        platformFamily: "unix",
        platformOs: "macos",
      });
      break;
    }
    case "getAuthStatus": {
      respond(id, { requiresOpenaiAuth: false, authMethod: "apikey", authToken: null });
      break;
    }
    case "thread/start": {
      threadConfig = params?.config || {};
      threadMcpServers = threadConfig?.mcp_servers || {};
      logProtocol({
        event: "thread_start",
        thread_id: params?.threadId || "",
        mcp_server_names: Object.keys(threadMcpServers),
        sandbox: threadConfig?.sandbox,
        approval_policy: threadConfig?.approval_policy,
      });
      respond(id, threadResponse());
      break;
    }
    case "thread/resume": {
      respond(id, threadResponse(params?.threadId || "thread-1"));
      break;
    }
    case "turn/start": {
      logProtocol({ event: "turn_start", thread_id: params?.threadId || "" });
      if (hangMode) {
        break;
      }
      if (!failTurnMode) {
        respond(id, {
          turn: {
            id: "turn-1",
            items: [],
            status: "inProgress",
            error: null,
            startedAt: 0,
            completedAt: null,
            durationMs: null,
          },
        });
      }
      void connectToMcpServers(threadMcpServers).then((connected) => {
        const names = connected.filter((item) => item.connected).map((item) => item.server);
        logProtocol({ event: "run_complete", mcp_connected: names });
        if (failTurnMode) {
          send({
            jsonrpc: "2.0",
            id,
            error: { code: -32603, message: "offline Codex forced turn failure" },
          });
          return;
        }
        send({
          jsonrpc: "2.0",
          method: "turn/started",
          params: {
            threadId: "thread-1",
            turn: {
              id: "turn-1",
              items: [],
              itemsView: "full",
              status: "inProgress",
              error: null,
              startedAt: null,
              completedAt: null,
              durationMs: null,
            },
          },
        });
        send({
          jsonrpc: "2.0",
          method: "turn/completed",
          params: {
            threadId: "thread-1",
            turn: {
              id: "turn-1",
              items: [],
              itemsView: "full",
              status: "completed",
              error: null,
              startedAt: null,
              completedAt: null,
              durationMs: null,
            },
          },
        });
      });
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
