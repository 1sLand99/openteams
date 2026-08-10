#!/usr/bin/env node
import { appendFileSync, existsSync, readFileSync } from "node:fs";
import { delimiter, isAbsolute, join, relative, resolve } from "node:path";
import { spawn } from "node:child_process";

const PI_PACKAGE = "@earendil-works/pi-coding-agent";
const PI_VERSION = "0.83.0";
const MCP_PACKAGE = "pi-mcp-adapter";
const MCP_VERSION = "2.18.0";
const diagnosticLog = process.env.OPENTEAMS_PI_DIAGNOSTIC_LOG;

function logDiagnostic(event, details = {}) {
  if (!diagnosticLog || !isAbsolute(diagnosticLog)) return;
  try {
    appendFileSync(diagnosticLog, `${JSON.stringify({
      timestamp: new Date().toISOString(),
      event,
      launcherPid: process.pid,
      launcherParentPid: process.ppid,
      ...details,
    })}\n`, "utf8");
  } catch {
    // Diagnostics must never change the launcher lifecycle.
  }
}

function packageVersion(packageRoot) {
  try {
    return JSON.parse(readFileSync(join(packageRoot, "package.json"), "utf8")).version;
  } catch {
    return undefined;
  }
}

function packageBin(packageRoot, command) {
  let metadata;
  try {
    metadata = JSON.parse(readFileSync(join(packageRoot, "package.json"), "utf8"));
  } catch {
    return undefined;
  }
  const configured = typeof metadata?.bin === "string" ? metadata.bin : metadata?.bin?.[command];
  if (typeof configured !== "string" || !configured) return undefined;
  const entry = resolve(packageRoot, configured);
  const packageRelative = relative(packageRoot, entry);
  if (packageRelative.startsWith("..") || isAbsolute(packageRelative) || !existsSync(entry)) {
    return undefined;
  }
  return entry;
}

function locatePinnedNpxEnvironment() {
  for (const entry of (process.env.PATH ?? "").split(delimiter)) {
    if (!entry || !entry.endsWith(`${delimiter === ";" ? "\\" : "/"}.bin`)) continue;
    const nodeModules = resolve(entry, "..");
    const piRoot = join(nodeModules, PI_PACKAGE);
    const mcpRoot = join(nodeModules, MCP_PACKAGE);
    if (packageVersion(piRoot) !== PI_VERSION || packageVersion(mcpRoot) !== MCP_VERSION) continue;
    const pi = process.platform === "win32"
      ? packageBin(piRoot, "pi")
      : join(entry, "pi");
    const mcpEntry = join(mcpRoot, "index.ts");
    if (pi && existsSync(pi) && existsSync(mcpEntry)) return { pi, piRoot, nodeModules };
  }
  throw new Error("Pinned Pi packages were not found in the current NPX environment");
}

function requiredAbsoluteFile(name) {
  const value = process.env[name];
  if (!value || !isAbsolute(value) || !existsSync(value)) {
    throw new Error(`${name} must name an existing absolute file`);
  }
  return value;
}

function isolatedSkillPaths() {
  const raw = process.env.OPENTEAMS_PI_SKILL_PATHS_JSON ?? "[]";
  let values;
  try {
    values = JSON.parse(raw);
  } catch {
    throw new Error("OPENTEAMS_PI_SKILL_PATHS_JSON must be valid JSON");
  }
  if (!Array.isArray(values)) throw new Error("Pi skill snapshot must be an array");
  for (const value of values) {
    if (typeof value !== "string" || !isAbsolute(value) || !value.endsWith("SKILL.md")) {
      throw new Error("Pi skill snapshot contains an invalid path");
    }
  }
  return values;
}

logDiagnostic("launcher_start", {
  cwd: process.cwd(),
  argv: process.argv,
  path: process.env.PATH,
});

let pinnedEnvironment;
try {
  pinnedEnvironment = locatePinnedNpxEnvironment();
} catch (error) {
  logDiagnostic("pinned_environment_error", {
    error: error instanceof Error ? error.stack ?? error.message : String(error),
  });
  throw error;
}
const { pi, piRoot, nodeModules } = pinnedEnvironment;
logDiagnostic("pinned_environment_resolved", { pi, piRoot, nodeModules });
process.env.NODE_PATH = [nodeModules, join(piRoot, "node_modules"), process.env.NODE_PATH]
  .filter(Boolean)
  .join(delimiter);

const args = [...process.argv.slice(2), "--no-skills"];
for (const skillPath of isolatedSkillPaths()) args.push("--skill", skillPath);
args.push("--no-extensions", "--extension", requiredAbsoluteFile("OPENTEAMS_PI_APPROVAL_EXTENSION"));
if (process.env.OPENTEAMS_PI_ENABLE_MCP_EXTENSION === "1") {
  args.push("--extension", requiredAbsoluteFile("OPENTEAMS_PI_MCP_EXTENSION"));
}

const childProgram = process.platform === "win32" ? process.execPath : pi;
const childArgs = process.platform === "win32" ? [pi, ...args] : args;
const child = spawn(childProgram, childArgs, {
  env: process.env,
  stdio: ["inherit", "inherit", "pipe"],
  shell: false
});
logDiagnostic("pi_spawn_requested", { childProgram, childArgs });
const acpParentPid = process.ppid;
let orphanCleanupStarted = false;

function terminateOrphanedTree() {
  if (orphanCleanupStarted) return;
  orphanCleanupStarted = true;
  logDiagnostic("orphan_cleanup_started", { childPid: child.pid });
  if (process.platform === "win32") {
    child.kill("SIGTERM");
    setTimeout(() => child.kill("SIGKILL"), 1000);
    return;
  }
  try {
    process.kill(-acpParentPid, "SIGTERM");
  } catch {
    child.kill("SIGTERM");
  }
  setTimeout(() => {
    try {
      process.kill(-acpParentPid, "SIGKILL");
    } catch {
      child.kill("SIGKILL");
    }
  }, 1000);
}

const parentWatcher = setInterval(() => {
  try {
    process.kill(acpParentPid, 0);
  } catch {
    terminateOrphanedTree();
  }
}, 250);
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => {
    logDiagnostic("launcher_signal", { signal, childPid: child.pid });
    child.kill(signal);
  });
}
child.on("spawn", () => {
  logDiagnostic("pi_spawned", { childPid: child.pid });
});
child.stderr?.on("data", (chunk) => {
  const text = chunk.toString();
  logDiagnostic("pi_stderr", { childPid: child.pid, text });
  process.stderr.write(chunk);
});
child.on("error", (error) => {
  clearInterval(parentWatcher);
  logDiagnostic("pi_spawn_error", {
    childPid: child.pid,
    error: error instanceof Error ? error.stack ?? error.message : String(error),
  });
  process.stderr.write(`Pi launcher failed: ${error.message}\n`);
  process.exitCode = 1;
});
child.on("exit", (code, signal) => {
  clearInterval(parentWatcher);
  logDiagnostic("pi_exit", { childPid: child.pid, code, signal });
  process.exitCode = code ?? (signal ? 1 : 0);
});
