#!/usr/bin/env node
/**
 * Shared offline MCP client used by the member-scoped MCP E2E fake CLIs.
 *
 * Given a canonical server map ({name: {command, args, env}}), spawns each
 * stdio MCP server (the local offline mcp-server), performs initialize +
 * tools/list, and returns the connection results. Every emitted log line is
 * redacted of the fixed fake secret before it is written anywhere.
 */
import { appendFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";
import { spawn } from "node:child_process";

export function redact(value, fakeSecret) {
  if (!fakeSecret) return String(value);
  return String(value).replaceAll(fakeSecret, "[REDACTED]");
}

export function logProtocol(protocolLog, fakeSecret, event) {
  if (protocolLog) {
    try {
      mkdirSync(dirname(protocolLog), { recursive: true });
      appendFileSync(protocolLog, redact(JSON.stringify(event), fakeSecret) + "\n");
    } catch {}
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

export async function connectToMcpServers(servers, options) {
  const {
    protocolLog,
    fakeSecret,
    noMcpMode = false,
    runner = "fixture",
  } = options || {};
  const results = [];
  for (const [name, definition] of Object.entries(servers || {})) {
    const record = { server: name, connected: false, tools: [] };
    try {
      if (noMcpMode) {
        logProtocol(protocolLog, fakeSecret, { event: "mcp_skipped", server: name, reason: "no_mcp_mode" });
        continue;
      }
      const command = definition?.command;
      const args = Array.isArray(definition?.args) ? definition.args : [];
      const env = definition?.env || {};
      if (typeof command !== "string" || command.length === 0) {
        logProtocol(protocolLog, fakeSecret, { event: "mcp_skipped", server: name, reason: "missing command" });
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
          clientInfo: { name: runner, version: "1.0.0" },
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
      logProtocol(protocolLog, fakeSecret, {
        event: "mcp_connected",
        server: name,
        connected: record.connected,
        tools: record.tools,
      });
    } catch (error) {
      record.error = redact(error.message, fakeSecret);
      logProtocol(protocolLog, fakeSecret, { event: "mcp_connect_error", server: name, error: record.error });
    }
    results.push(record);
  }
  return results;
}
