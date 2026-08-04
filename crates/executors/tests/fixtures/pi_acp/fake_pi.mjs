#!/usr/bin/env node
/**
 * Fake Pi coding agent: minimal RPC responder for process tree testing.
 *
 * This script is launched by the Pi launcher (launcher.mjs) and records
 * process topology to a PID file. It implements just enough of Pi's
 * internal RPC protocol to be spawned, record PIDs, and exit cleanly.
 *
 * The ACP protocol is handled by fake_pi_acp.mjs - this script exists
 * solely to exercise the launcher -> pi -> mcp-adapter process tree.
 */
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { spawn } from "node:child_process";
import readline from "node:readline";

const sessionFile = process.env.OPENTEAMS_FAKE_PI_SESSION_FILE;
if (sessionFile) {
  mkdirSync(dirname(sessionFile), { recursive: true });
  writeFileSync(sessionFile, "");
}

let mcpChild = null;
if (process.env.OPENTEAMS_FAKE_PI_CHILD_PID_FILE) {
  try {
    mcpChild = spawn("pi-mcp-adapter", [], { stdio: "ignore" });
  } catch {
    // mcp-adapter spawn is best-effort
  }
  writeFileSync(
    process.env.OPENTEAMS_FAKE_PI_CHILD_PID_FILE,
    JSON.stringify({
      pi: process.pid,
      launcher: process.ppid,
      mcp: mcpChild ? mcpChild.pid : null,
    })
  );
}

const send = (value) => process.stdout.write(JSON.stringify(value) + "\n");
const respond = (request, data = {}) =>
  send({ type: "response", id: request.id, success: true, data });

const state = {
  sessionId: "pi-offline-session",
  model: { provider: "offline-provider", id: "offline-model" },
  thinkingLevel: "off",
};

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let request;
  try { request = JSON.parse(line); } catch { return; }
  switch (request.type) {
    case "get_state":
      respond(request, state);
      break;
    case "get_available_models":
      respond(request, {
        models: [{
          provider: "offline-provider",
          id: "offline-model",
          name: "Offline Model",
        }],
      });
      break;
    case "get_commands":
      respond(request, { commands: [] });
      break;
    case "get_messages":
      respond(request, { messages: [] });
      break;
    case "prompt":
      if (process.env.OPENTEAMS_FAKE_PI_PROMPTS) {
        appendFileSync(process.env.OPENTEAMS_FAKE_PI_PROMPTS, `${request.message}\n`);
      }
      respond(request);
      if (process.env.OPENTEAMS_FAKE_PI_HANG !== "1") {
        send({ type: "agent_start" });
        send({
          type: "message_update",
          assistantMessageEvent: {
            type: "text_delta",
            delta: `echo:${request.message}`,
          },
        });
        send({ type: "agent_end" });
        send({ type: "agent_settled" });
      }
      break;
    case "abort":
      respond(request);
      send({ type: "agent_settled" });
      break;
    default:
      respond(request);
  }
});

rl.on("close", () => {
  if (mcpChild) {
    try { mcpChild.kill("SIGTERM"); } catch {}
  }
  process.exit(0);
});
