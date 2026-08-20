#!/usr/bin/env node
/**
 * Offline local-HTTP server for the OPENCODE and OPEN_TEAMS_CLI runners.
 *
 * The production executor launches `opencode serve --hostname 127.0.0.1 --port 0`
 * (or the OpenTeams CLI equivalent), parses the listening URL from stdout, and
 * then drives the run over the OpenCode HTTP SDK. This fixture is that server;
 * it only replaces the external CLI protocol.
 *
 * Endpoints implemented for the production SDK client:
 *   GET  /global/health                    -> healthy + version
 *   POST /session                         -> create session
 *   POST /session/{id}/message            -> session.message (prompt)
 *   POST /session/{id}/abort              -> cancel
 *   GET  /event                           -> SSE stream (emits session.idle)
 *   GET  /config, /mcp, /command, /agent  -> inert defaults
 *
 * MCP isolation carrier: the production adapter pins the frozen member config
 * in OPENCODE_CONFIG_CONTENT or OPENTEAMS_CONFIG_CONTENT (and mirrors it
 * through XDG_CONFIG_HOME plus its runner-specific project-config guard).
 * This fixture parses that content, spawns each configured stdio MCP server (the local offline
 * mcp-server), performs initialize + tools/list, and records every server it
 * connected to.
 *
 * Per-run control environment:
 *   FAKE_HTTP_PROTOCOL_LOG   - JSONL protocol log (redacted of the fake secret)
 *   FAKE_HTTP_FAKE_SECRET    - fixed fake secret used by redaction assertions
 *   FAKE_HTTP_RUNNER         - "opencode" | "openteams-cli"
 *   FAKE_HTTP_VERSION        - version reported by /global/health
 *   FAKE_HTTP_HANG=1         - never send session.idle (cancel testing)
 *   FAKE_HTTP_FAIL=1         - fail the session with a session.error event
 *   FAKE_HTTP_NO_MCP=1       - never connect to MCP servers
 *   FAKE_HTTP_STDIO_MCP_COMMAND - node binary used to launch the MCP server
 */
import http from "node:http";
import { appendFileSync, mkdirSync, readFileSync } from "node:fs";
import { dirname } from "node:path";
import { spawn } from "node:child_process";

const RUNNER = process.env.FAKE_HTTP_RUNNER || "opencode";
const protocolLog = process.env.FAKE_HTTP_PROTOCOL_LOG;
const fakeSecret = process.env.FAKE_HTTP_FAKE_SECRET;
const hangMode = process.env.FAKE_HTTP_HANG === "1";
const failMode = process.env.FAKE_HTTP_FAIL === "1";
const noMcpMode = process.env.FAKE_HTTP_NO_MCP === "1";
const version = process.env.FAKE_HTTP_VERSION || "1.17.18";
const configContent = process.env.OPENCODE_CONFIG_CONTENT || process.env.OPENTEAMS_CONFIG_CONTENT || "";
const nodeBin = process.env.FAKE_HTTP_STDIO_MCP_COMMAND || "node";

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

function parseConfigMcp() {
  try {
    const config = JSON.parse(configContent);
    const servers = config?.mcp || {};
    logProtocol({
      event: "config_content_read",
      mcp_server_names: Object.keys(servers),
      has_project_config_disabled: process.env.OPENCODE_DISABLE_PROJECT_CONFIG === "true",
      has_openteams_project_config_disabled: process.env.OPENTEAMS_DISABLE_PROJECT_CONFIG === "true",
      xdg_config_home: process.env.XDG_CONFIG_HOME || "",
    });
    return servers;
  } catch (error) {
    logProtocol({ event: "config_content_parse_error", error: redact(error.message) });
    return {};
  }
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
      const commandParts = Array.isArray(definition?.command)
        ? definition.command
        : [definition?.command, ...(Array.isArray(definition?.args) ? definition.args : [])];
      const [command, ...args] = commandParts;
      const env = definition?.environment || definition?.env || {};
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
      const initializeResponse = await waiter(1);
      if (initializeResponse && initializeResponse.result !== undefined) {
        sendLine({ jsonrpc: "2.0", method: "notifications/initialized" });
        sendLine({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
        const toolListResponse = await waiter(2);
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

const eventStreams = new Set();

function broadcastSessionIdle(sessionId) {
  for (const res of eventStreams) {
    try {
      res.write(
        `id: 1\ndata: ${JSON.stringify({
          type: "session.idle",
          properties: { sessionID: sessionId },
        })}\n\n`
      );
    } catch {}
  }
}

function broadcastSessionError(sessionId) {
  for (const res of eventStreams) {
    try {
      res.write(
        `id: 2\ndata: ${JSON.stringify({
          type: "session.error",
          properties: {
            sessionID: sessionId,
            error: { name: "E2EFailure", message: "offline e2e forced failure" },
          },
        })}\n\n`
      );
    } catch {}
  }
}

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `${"http"}://localhost`);
  const path = url.pathname;
  logProtocol({ event: "http_request", method: req.method, path });

  if (req.method === "GET" && path === "/global/health") {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ healthy: true, version }));
    return;
  }

  if (req.method === "POST" && path === "/session") {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      const sessionId = `offline-${RUNNER}-session`;
      logProtocol({ event: "session_created", session_id: sessionId });
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ id: sessionId }));
    });
    return;
  }

  const messageMatch = path.match(/^\/session\/([^/]+)\/message$/);
  if (req.method === "POST" && messageMatch) {
    const sessionId = messageMatch[1];
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      let promptText = "";
      try {
        const parsed = JSON.parse(body);
        const part = (parsed?.parts || []).find((item) => item?.type === "text");
        promptText = part?.text || "";
      } catch {}
      logProtocol({ event: "prompt_received", session_id: sessionId, prompt_len: promptText.length });
      if (!hangMode) {
        const servers = parseConfigMcp();
        void connectToMcpServers(servers).then((connected) => {
          const names = connected.filter((item) => item.connected).map((item) => item.server);
          logProtocol({ event: "run_complete", session_id: sessionId, mcp_connected: names });
          if (failMode) {
            broadcastSessionError(sessionId);
          } else {
            broadcastSessionIdle(sessionId);
          }
        });
      }
      res.writeHead(200, { "Content-Type": "application/json" });
      res.end(
        JSON.stringify({
          info: { id: sessionId },
          parts: [
            {
              type: "text",
              text: "offline e2e echo",
            },
          ],
        })
      );
    });
    return;
  }

  if (req.method === "POST" && /^\/session\/[^/]+\/abort$/.test(path)) {
    logProtocol({ event: "session_abort" });
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end("{}");
    return;
  }

  if (req.method === "GET" && path === "/event") {
    res.writeHead(200, {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
      Connection: "keep-alive",
    });
    res.write(": connected\n\n");
    eventStreams.add(res);
    req.on("close", () => eventStreams.delete(res));
    return;
  }

  if (req.method === "GET" && ["/config", "/mcp", "/command", "/agent", "/provider"].includes(path)) {
    res.writeHead(200, { "Content-Type": "application/json" });
    if (path === "/config") {
      res.end(JSON.stringify({ model: null }));
    } else {
      res.end("[]");
    }
    return;
  }

  res.writeHead(404, { "Content-Type": "application/json" });
  res.end(JSON.stringify({ name: "NotFoundError", data: { message: "offline fixture 404" } }));
});

server.listen(0, "127.0.0.1", () => {
  const address = server.address();
  const port = address.port;
  const scheme = "http";
  const listeningUrl = `${scheme}://127.0.0.1:${port}`;
  process.stdout.write(`opencode server listening on ${listeningUrl}\n`);
  process.stdout.flush?.();
  logProtocol({ event: "listening", url: listeningUrl });
});

function shutdown() {
  server.close(() => process.exit(0));
  setTimeout(() => process.exit(0), 200).unref();
}
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
