#!/usr/bin/env node
import { existsSync, readFileSync } from "node:fs";
import { delimiter, isAbsolute, join, resolve } from "node:path";
import { spawn } from "node:child_process";

const PI_PACKAGE = "@earendil-works/pi-coding-agent";
const PI_VERSION = "0.83.0";
const MCP_PACKAGE = "pi-mcp-adapter";
const MCP_VERSION = "2.18.0";

function packageVersion(packageRoot) {
  try {
    return JSON.parse(readFileSync(join(packageRoot, "package.json"), "utf8")).version;
  } catch {
    return undefined;
  }
}

function locatePinnedNpxEnvironment() {
  for (const entry of (process.env.PATH ?? "").split(delimiter)) {
    if (!entry || !entry.endsWith(`${delimiter === ";" ? "\\" : "/"}.bin`)) continue;
    const nodeModules = resolve(entry, "..");
    const piRoot = join(nodeModules, PI_PACKAGE);
    const mcpRoot = join(nodeModules, MCP_PACKAGE);
    if (packageVersion(piRoot) !== PI_VERSION || packageVersion(mcpRoot) !== MCP_VERSION) continue;
    const pi = join(entry, process.platform === "win32" ? "pi.cmd" : "pi");
    const mcpEntry = join(mcpRoot, "index.ts");
    if (existsSync(pi) && existsSync(mcpEntry)) return { pi, piRoot, nodeModules };
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

const { pi, piRoot, nodeModules } = locatePinnedNpxEnvironment();
process.env.NODE_PATH = [nodeModules, join(piRoot, "node_modules"), process.env.NODE_PATH]
  .filter(Boolean)
  .join(delimiter);

const args = [...process.argv.slice(2), "--no-skills"];
for (const skillPath of isolatedSkillPaths()) args.push("--skill", skillPath);
args.push("--no-extensions", "--extension", requiredAbsoluteFile("OPENTEAMS_PI_APPROVAL_EXTENSION"));
if (process.env.OPENTEAMS_PI_ENABLE_MCP_EXTENSION === "1") {
  args.push("--extension", requiredAbsoluteFile("OPENTEAMS_PI_MCP_EXTENSION"));
}

const child = spawn(pi, args, { env: process.env, stdio: "inherit", shell: false });
const acpParentPid = process.ppid;
let orphanCleanupStarted = false;

function terminateOrphanedTree() {
  if (orphanCleanupStarted) return;
  orphanCleanupStarted = true;
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
  process.on(signal, () => child.kill(signal));
}
child.on("error", (error) => {
  clearInterval(parentWatcher);
  process.stderr.write(`Pi launcher failed: ${error.message}\n`);
  process.exitCode = 1;
});
child.on("exit", (code, signal) => {
  clearInterval(parentWatcher);
  process.exitCode = code ?? (signal ? 1 : 0);
});
