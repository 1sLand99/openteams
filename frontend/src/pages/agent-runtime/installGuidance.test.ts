// Install guide resolution tests. Run with:
//     pnpm exec tsx src/pages/agent-runtime/installGuidance.test.ts

import type { AgentRuntimeStatus, BaseCodingAgent } from "@/types";
import {
  detectClientPlatform,
  getInstallGuideEntry,
  joinGuideCommands,
  resolveInstallGuide,
} from "./installGuidance";

let failures = 0;

const check = (label: string, condition: boolean, detail?: unknown) => {
  if (condition) {
    // eslint-disable-next-line no-console
    console.log(`  ok  ${label}`);
    return;
  }
  failures += 1;
  // eslint-disable-next-line no-console
  console.error(`  FAIL ${label}`, detail ?? "");
};

const makeRunner = (
  runnerType: BaseCodingAgent,
  overrides: Partial<AgentRuntimeStatus> = {},
): AgentRuntimeStatus =>
  ({
    runner_type: runnerType,
    installed: false,
    executable: false,
    availability: { type: "NOT_FOUND" },
    auth_state: "unauthenticated",
    node_available: false,
    discovered_models: [],
    model_source: "none",
    version: null,
    last_checked_at: null,
    last_error: null,
    run_mode: "local",
    env_summary: [],
    executor_options: {},
    ...overrides,
  }) as AgentRuntimeStatus;

// --- Registry coverage ------------------------------------------------

const guidedRunners: BaseCodingAgent[] = [
  "AMP",
  "CLAUDE_CODE",
  "CODEX",
  "COPILOT",
  "CURSOR_AGENT",
  "DROID",
  "GEMINI",
  "KIMI_CODE",
  "OPENCODE",
  "QODER_CLI",
  "QWEN_CODE",
];
for (const runner of guidedRunners) {
  check(
    `${runner} has an install guide entry`,
    getInstallGuideEntry(runner) !== null,
  );
}
check(
  "bundled OPEN_TEAMS_CLI has no guide entry",
  getInstallGuideEntry("OPEN_TEAMS_CLI") === null,
);

// --- Platform detection ------------------------------------------------

check(
  "detects macOS from navigator.platform",
  detectClientPlatform({ platform: "MacIntel" }) === "macos",
);
check(
  "detects Windows from userAgentData",
  detectClientPlatform({ userAgentDataPlatform: "Windows" }) === "windows",
);
check(
  "detects Linux from user agent",
  detectClientPlatform({ userAgent: "X11; Linux x86_64" }) === "linux",
);

// --- Step resolution ---------------------------------------------------

const notInstalledClaude = resolveInstallGuide(
  makeRunner("CLAUDE_CODE"),
  "macos",
);
check(
  "not-installed npm agent offers node then install steps",
  notInstalledClaude?.steps.map((step) => step.kind).join(",") ===
    "node,install,auth",
  notInstalledClaude?.steps,
);
check(
  "claude install uses the global npm package",
  notInstalledClaude?.steps[1]?.commands[0] ===
    "npm install -g @anthropic-ai/claude-code",
);
check(
  "claude auth falls back to /login inside the CLI",
  notInstalledClaude?.steps[2]?.authFollowUpCommand === "/login",
);

const claudeWithNode = resolveInstallGuide(
  makeRunner("CLAUDE_CODE", { node_available: true }),
  "macos",
);
check(
  "node step is hidden when node is already installed",
  claudeWithNode?.steps.map((step) => step.kind).join(",") === "install,auth",
  claudeWithNode?.steps,
);

const installedAuthedClaude = resolveInstallGuide(
  makeRunner("CLAUDE_CODE", {
    installed: true,
    executable: true,
    availability: { type: "LOGIN_DETECTED", last_auth_timestamp: BigInt(1) },
    auth_state: "authenticated",
    node_available: true,
  }),
  "linux",
);
check(
  "installed and authenticated runner needs no guide",
  installedAuthedClaude === null,
);

const installedUnauthedCodex = resolveInstallGuide(
  makeRunner("CODEX", {
    installed: true,
    executable: true,
    availability: { type: "INSTALLATION_FOUND" },
  }),
  "windows",
);
check(
  "installed unauthenticated runner only offers the auth step",
  installedUnauthedCodex?.steps.map((step) => step.kind).join(",") === "auth",
  installedUnauthedCodex?.steps,
);
check(
  "codex auth command is codex login",
  installedUnauthedCodex?.steps[0]?.commands[0] === "codex login",
);

const cursorOnWindows = resolveInstallGuide(
  makeRunner("CURSOR_AGENT"),
  "windows",
);
check(
  "curl-script agents are WSL-only on Windows",
  cursorOnWindows?.windowsSupport === "wsl_only",
);
check(
  "WSL-only agents reuse the posix install command on Windows",
  cursorOnWindows?.steps[0]?.commands[0] ===
    "curl https://cursor.com/install -fsS | bash",
);

const kimiOnLinux = resolveInstallGuide(makeRunner("KIMI_CODE"), "linux");
check(
  "native-binary agents skip the node step",
  kimiOnLinux?.steps.every((step) => step.kind !== "node") === true,
);
check(
  "kimi install uses the official install script",
  kimiOnLinux?.steps[0]?.commands[0] ===
    "curl -LsSf https://code.kimi.com/install.sh | bash",
);

const qoderOnMac = resolveInstallGuide(makeRunner("QODER_CLI"), "macos");
check(
  "qoder install uses the official install script",
  qoderOnMac?.steps[0]?.commands[0] ===
    "curl -fsSL https://qoder.com/install | bash",
);
check(
  "qoder skips the node step for script installs",
  qoderOnMac?.steps.every((step) => step.kind !== "node") === true,
);
check(
  "qoder auth starts the CLI and finishes with /login",
  qoderOnMac?.steps[1]?.commands[0] === "qodercli" &&
    qoderOnMac?.steps[1]?.authFollowUpCommand === "/login",
);
const qoderOnWindows = resolveInstallGuide(
  makeRunner("QODER_CLI"),
  "windows",
);
check(
  "qoder installs natively on Windows via PowerShell",
  qoderOnWindows?.windowsSupport === "supported" &&
    qoderOnWindows?.steps[0]?.commands[0] ===
      "irm https://qoder.com/install.ps1 | iex",
);
check(
  "qoder documents the PAT env var for headless auth",
  getInstallGuideEntry("QODER_CLI")?.authEnvVars?.includes(
    "QODER_PERSONAL_ACCESS_TOKEN",
  ) === true,
);

const droidOnMac = resolveInstallGuide(makeRunner("DROID"), "macos");
check(
  "no auth commands are ever empty for guided runners",
  droidOnMac?.steps.some((step) => step.kind === "auth") === true,
);

// --- Clipboard text ----------------------------------------------------

const windowsText = joinGuideCommands(
  [
    { kind: "node", commands: ["winget install OpenJS.NodeJS.LTS"] },
    { kind: "install", commands: ["npm install -g @openai/codex"] },
  ],
  "windows",
);
check(
  "windows clipboard text uses CRLF",
  windowsText === "winget install OpenJS.NodeJS.LTS\r\nnpm install -g @openai/codex",
  windowsText,
);
const posixText = joinGuideCommands(
  [
    { kind: "install", commands: ["a", "b"] },
    { kind: "auth", commands: ["c"] },
  ],
  "linux",
);
check("posix clipboard text uses LF", posixText === "a\nb\nc", posixText);
check(
  "clipboard text never embeds secrets",
  !guidedRunners.some((runner) => {
    const entry = getInstallGuideEntry(runner);
    const text = [
      ...(entry?.installCommands.posix ?? []),
      ...(entry?.installCommands.windows ?? []),
      ...(entry?.authCommands ?? []),
    ].join(" ");
    return /api[_-]?key|token|secret/i.test(text);
  }),
);

if (failures > 0) {
  process.exit(1);
}