#!/usr/bin/env node
/**
 * Local offline MCP server (stdio transport) for member-scoped MCP E2E.
 *
 * Speaks newline-delimited JSON-RPC 2.0 over stdio, exactly like a real
 * `command`-type MCP server spawned by a coding agent. No network, no real
 * server, no accounts. Implements:
 *   - initialize
 *   - notifications/initialized
 *   - tools/list
 *   - tools/call
 *   - ping
 *
 * Every received request is appended to the connection log path supplied in
 * MCP_SERVER_LOG. The MCP_SERVER_TAG environment value is echoed into each log
 * line so the E2E can prove which member's server was actually connected.
 *
 * The server MUST NOT embed real secrets. Caller-supplied environment values
 * (including a fixed fake secret for redaction assertions) are only written to
 * the connection log in a redacted form.
 */
import { appendFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import readline from "node:readline";

const TAG = process.env.MCP_SERVER_TAG || "unnamed";
const LOG_PATH = process.env.MCP_SERVER_LOG;
const FAKE_SECRET = process.env.MCP_SERVER_FAKE_SECRET;

function logConnection(event) {
  if (!LOG_PATH) return;
  try {
    mkdirSync(dirname(LOG_PATH), { recursive: true });
    const redacted = FAKE_SECRET ? String(event).replaceAll(FAKE_SECRET, "[REDACTED]") : String(event);
    appendFileSync(LOG_PATH, `${TAG} ${redacted}\n`);
  } catch {}
}

const TOOL_DEFINITION = {
  name: "e2e_echo",
  description: "Offline E2E echo tool.",
  inputSchema: {
    type: "object",
    properties: { text: { type: "string" } },
    required: ["text"],
  },
};

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let message;
  try {
    message = JSON.parse(trimmed);
  } catch {
    return;
  }
  logConnection(`request:${trimmed}`);

  if (message.id === undefined) {
    // notification
    return;
  }

  const { id, method } = message;
  switch (method) {
    case "initialize":
      send({
        jsonrpc: "2.0",
        id,
        result: {
          protocolVersion: "2024-11-05",
          capabilities: { tools: { listChanged: false } },
          serverInfo: { name: "member-scoped-e2e-mcp", version: "1.0.0" },
        },
      });
      break;
    case "tools/list":
      send({ jsonrpc: "2.0", id, result: { tools: [TOOL_DEFINITION] } });
      break;
    case "tools/call": {
      const args = message.params?.arguments || {};
      const text = String(args.text || "");
      const redacted = FAKE_SECRET ? text.replaceAll(FAKE_SECRET, "[REDACTED]") : text;
      send({
        jsonrpc: "2.0",
        id,
        result: {
          content: [
            {
              type: "text",
              text: `echo:${redacted}:${TAG}`,
            },
          ],
          isError: false,
        },
      });
      break;
    }
    case "ping":
      send({ jsonrpc: "2.0", id, result: {} });
      break;
    default:
      send({
        jsonrpc: "2.0",
        id,
        error: { code: -32601, message: `method not found: ${method}` },
      });
  }
});

rl.on("close", () => process.exit(0));
