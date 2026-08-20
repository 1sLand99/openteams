#!/usr/bin/env node
/**
 * Offline stdio-streaming fake for the CURSOR_AGENT, COPILOT and DROID
 * runners in the member-scoped MCP E2E.
 *
 * These production executors write the prompt to stdin and read a JSON
 * stream-json output from stdout; the run completes when the process exits.
 * This fixture replaces that external CLI protocol; it only replaces the
 * external CLI protocol implementation.
 *
 * MCP isolation carrier (per runner, read exactly where production wrote it):
 *   amp:          $AMP_SETTINGS_FILE -> settings.json with the amp.mcpServers key
 *   cursor-agent: $HOME/.cursor/mcp.json        (HOME pinned to the run home)
 *   copilot:      $COPILOT_HOME/mcp-config.json (COPILOT_HOME pinned to run home)
 *   droid:        $HOME/.factory/mcp.json       (HOME pinned to the run home)
 *
 * This fixture parses the carrier, spawns each configured stdio MCP server
 * (the local offline mcp-server), performs initialize + tools/list, and
 * records every server it connected to.
 *
 * Per-run control environment:
 *   FAKE_STDIO_RUNNER         - "amp" | "cursor" | "copilot" | "droid"
 *   FAKE_STDIO_PROTOCOL_LOG   - JSONL protocol log (redacted of the fake secret)
 *   FAKE_STDIO_FAKE_SECRET    - fixed fake secret used by redaction assertions
 *   FAKE_STDIO_HANG=1         - never exit (cancel/cleanup testing)
 *   FAKE_STDIO_FAIL=1         - exit nonzero (failure cleanup)
 *   FAKE_STDIO_NO_MCP=1       - never connect to MCP servers
 *   FAKE_STDIO_STDIO_MCP_COMMAND - node binary used to launch the MCP server
 */
import { readFileSync } from "node:fs";
import { connectToMcpServers, logProtocol, redact } from "./mcp_client.mjs";

const RUNNER = process.env.FAKE_STDIO_RUNNER || "cursor";
const protocolLog = process.env.FAKE_STDIO_PROTOCOL_LOG;
const fakeSecret = process.env.FAKE_STDIO_FAKE_SECRET;
const hangMode = process.env.FAKE_STDIO_HANG === "1";
const failMode = process.env.FAKE_STDIO_FAIL === "1";
const noMcpMode = process.env.FAKE_STDIO_NO_MCP === "1";

function carrierPath() {
  if (RUNNER === "amp") {
    return process.env.AMP_SETTINGS_FILE || "";
  }
  const home = process.env.HOME || "";
  if (RUNNER === "copilot") {
    const copilotHome = process.env.COPILOT_HOME || home;
    return `${copilotHome}/mcp-config.json`;
  }
  if (RUNNER === "droid") {
    return `${home}/.factory/mcp.json`;
  }
  return `${home}/.cursor/mcp.json`;
}

const configPath = carrierPath();
logProtocol(protocolLog, fakeSecret, {
  event: "process_start",
  runner: RUNNER,
  config_path: configPath,
  argv: process.argv.slice(2),
});

function readConfigServers() {
  try {
    const raw = JSON.parse(readFileSync(configPath, "utf8"));
    const servers = RUNNER === "amp" ? raw?.["amp.mcpServers"] || {} : raw?.mcpServers || {};
    logProtocol(protocolLog, fakeSecret, {
      event: "config_read",
      runner: RUNNER,
      server_names: Object.keys(servers),
    });
    return servers;
  } catch (error) {
    logProtocol(protocolLog, fakeSecret, {
      event: "config_read_error",
      runner: RUNNER,
      error: redact(error.message, fakeSecret),
    });
    return {};
  }
}

let input = "";
process.stdin.on("data", (chunk) => {
  input += chunk.toString();
});
process.stdin.on("end", () => {
  logProtocol(protocolLog, fakeSecret, {
    event: "prompt_received",
    runner: RUNNER,
    prompt_len: input.length,
  });
  if (hangMode) return;
  const servers = readConfigServers();
  void connectToMcpServers(servers, {
    protocolLog,
    fakeSecret,
    noMcpMode,
    runner: RUNNER,
  }).then((connected) => {
    const names = connected.filter((item) => item.connected).map((item) => item.server);
    logProtocol(protocolLog, fakeSecret, {
      event: "run_complete",
      runner: RUNNER,
      mcp_connected: names,
    });
    if (failMode) {
      process.exit(4);
    }
    process.exit(0);
  });
});

process.on("SIGTERM", () => process.exit(0));
process.on("SIGINT", () => process.exit(0));
