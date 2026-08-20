#!/usr/bin/env node
/**
 * Offline Claude Code control-protocol fake for the member-scoped MCP E2E.
 *
 * The production Claude executor spawns the CLI (via fake npx) with a frozen
 * `--settings <private>/mcp.json` argument and drives the control protocol on
 * stdin/stdout (initialize, set_permission_mode, user message, result). This
 * fixture implements the CLI side of that protocol offline; it only replaces
 * the external CLI protocol implementation.
 *
 * MCP isolation carrier: the production adapter writes the frozen canonical
 * member snapshot to the private mcp.json referenced by --settings. This
 * fixture parses that file, spawns each configured stdio MCP server (the local
 * offline mcp-server), performs initialize + tools/list, and records every
 * server it connected to.
 *
 * Per-run control environment:
 *   FAKE_CLAUDE_PROTOCOL_LOG   - JSONL protocol log (redacted of the fake secret)
 *   FAKE_CLAUDE_FAKE_SECRET    - fixed fake secret used by redaction assertions
 *   FAKE_CLAUDE_HANG=1         - never emit result (cancel/cleanup testing)
 *   FAKE_CLAUDE_FAIL=1         - exit nonzero after user message (failure cleanup)
 *   FAKE_CLAUDE_NO_MCP=1       - never connect to MCP servers
 *   FAKE_CLAUDE_STDIO_MCP_COMMAND - node binary used to launch the MCP server
 */
import readline from "node:readline";
import { readFileSync } from "node:fs";
import { connectToMcpServers, logProtocol, redact } from "./mcp_client.mjs";

const protocolLog = process.env.FAKE_CLAUDE_PROTOCOL_LOG;
const fakeSecret = process.env.FAKE_CLAUDE_FAKE_SECRET;
const hangMode = process.env.FAKE_CLAUDE_HANG === "1";
const failMode = process.env.FAKE_CLAUDE_FAIL === "1";
const noMcpMode = process.env.FAKE_CLAUDE_NO_MCP === "1";

function mcpConfigPathFromArgv(argv) {
  const args = Array.isArray(argv) ? argv : [];
  for (let i = 0; i < args.length; i += 1) {
    if ((args[i] === "--mcp-config" || args[i] === "--settings") && args[i + 1]) {
      return args[i + 1];
    }
    if (args[i].startsWith("--mcp-config=") || args[i].startsWith("--settings=")) {
      return args[i].slice(args[i].indexOf("=") + 1);
    }
  }
  return "";
}

const settingsPath = mcpConfigPathFromArgv(process.argv.slice(2));
logProtocol(protocolLog, fakeSecret, {
  event: "process_start",
  settings_path: settingsPath,
});

function readSettingsServers() {
  try {
    if (!settingsPath) return {};
    const raw = JSON.parse(readFileSync(settingsPath, "utf8"));
    const servers = raw?.mcpServers || {};
    logProtocol(protocolLog, fakeSecret, {
      event: "settings_read",
      server_names: Object.keys(servers),
    });
    return servers;
  } catch (error) {
    logProtocol(protocolLog, fakeSecret, { event: "settings_read_error", error: redact(error.message, fakeSecret) });
    return {};
  }
}

let done = false;

async function onUserMessage(text) {
  if (hangMode) return;
  const servers = readSettingsServers();
  const connected = await connectToMcpServers(servers, {
    protocolLog,
    fakeSecret,
    noMcpMode,
    runner: "claude",
  });
  const names = connected.filter((item) => item.connected).map((item) => item.server);
  logProtocol(protocolLog, fakeSecret, {
    event: "user_message",
    echo: text,
    mcp_connected: names,
  });
  if (failMode) {
    process.exit(3);
  }
  if (!done) {
    done = true;
    process.stdout.write(
      JSON.stringify({
        type: "result",
        subtype: "success",
        is_error: false,
        result: "offline e2e complete",
        session_id: "claude-offline-session",
      }) + "\n"
    );
    process.exit(0);
  }
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  const trimmed = line.trim();
  if (!trimmed) return;
  let msg;
  try {
    msg = JSON.parse(trimmed);
  } catch {
    return;
  }
  if (msg.type === "control_request") {
    logProtocol(protocolLog, fakeSecret, {
      event: "control_request",
      subtype: msg.request?.subtype,
    });
    return;
  }
  if (msg.type === "user") {
    const content = msg.message?.content || "";
    const text = Array.isArray(content)
      ? content.filter((part) => part?.type === "text").map((part) => part.text || "").join("\n")
      : content;
    void onUserMessage(text);
  }
});

rl.on("close", () => process.exit(0));
process.on("SIGTERM", () => process.exit(0));
process.on("SIGINT", () => process.exit(0));
