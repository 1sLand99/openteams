// Install guide resolution tests. Run with:
//     pnpm exec tsx src/pages/agent-runtime/installGuidance.test.ts

import type { AgentRuntimeStatus, BaseCodingAgent } from "@/types";
import {
  detectClientPlatform,
  getInstallGuideEntry,
  getMissingRuntimeTools,
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
    npm_available: true,
    npx_available: true,
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
  "DEEPSEEK_HARNESS",
  "DROID",
  "GEMINI",
  "HERMES",
  "KIMI_CODE",
  "OPENCODE",
  "PI",
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
    node_available: true,
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

const hermesOnLinux = resolveInstallGuide(makeRunner("HERMES"), "linux");
check(
  "Hermes has a local CLI installation guide",
  hermesOnLinux?.steps[0]?.commands[0] === "pip install hermes-agent",
);
check(
  "Hermes guide directs provider setup through its terminal command",
  getInstallGuideEntry("HERMES")?.authCommands?.[0] === "hermes acp --setup",
);
const hermesNeedsSetup = resolveInstallGuide(
  makeRunner("HERMES", {
    installed: true,
    executable: true,
    availability: { type: "INSTALLATION_FOUND" },
    auth_state: "unauthenticated",
    version: "Hermes Agent v0.20.0",
  }),
  "linux",
);
check(
  "installed Hermes preserves availability while offering provider setup",
  hermesNeedsSetup?.steps.length === 1 &&
    hermesNeedsSetup.steps[0]?.kind === "auth" &&
    hermesNeedsSetup.steps[0]?.commands[0] === "hermes acp --setup",
);

const deepseekOnLinux = resolveInstallGuide(
  makeRunner("DEEPSEEK_HARNESS", { node_available: true }),
  "linux",
);
const deepseekInstallCommands = deepseekOnLinux?.steps[0]?.commands ?? [];
check(
  "DeepSeek Harness installs from the official source checkout",
  deepseekOnLinux?.steps[0]?.kind === "install" &&
    deepseekInstallCommands.includes(
      'git clone https://github.com/deepseek-ai/deepseek-harness.git "$HOME/deepseek-harness"',
    ),
  deepseekOnLinux?.steps,
);
check(
  "DeepSeek Harness setup pins the rc.7 revision and pnpm release",
  deepseekInstallCommands.includes(
    'git -C "$HOME/deepseek-harness" checkout 99f6f02fecdb7dff40c3fbc9470f5907c29f74ca',
  ) &&
    deepseekInstallCommands.includes(
      "npm install --global pnpm@11.7.0",
    ),
  deepseekOnLinux?.steps,
);
check(
  "DeepSeek Harness setup and launch do not use npx",
  deepseekInstallCommands.every((command) => !command.includes("npx")),
  deepseekOnLinux?.steps,
);
check(
  "DeepSeek Harness uses env-only authentication without a fake login command",
  deepseekOnLinux?.steps[1]?.kind === "auth" &&
    deepseekOnLinux.steps[1]?.commands.length === 0 &&
    getInstallGuideEntry("DEEPSEEK_HARNESS")?.authEnvVars?.includes(
      "DEEPSEEK_API_KEY",
    ) === true,
  deepseekOnLinux?.steps,
);
check(
  "DeepSeek Harness runtime requires Node but not npm or npx after build",
  getMissingRuntimeTools(
    makeRunner("DEEPSEEK_HARNESS", {
      node_available: true,
      npm_available: false,
      npx_available: false,
    }),
  ).length === 0,
);
const deepseekOnWindows = resolveInstallGuide(
  makeRunner("DEEPSEEK_HARNESS", { node_available: true }),
  "windows",
);
check(
  "DeepSeek Harness uses the pinned source checkout on Windows without npx",
  deepseekOnWindows?.steps[0]?.commands.includes(
    'git clone https://github.com/deepseek-ai/deepseek-harness.git "$HOME\\deepseek-harness"',
  ) === true &&
    deepseekOnWindows.steps[0]?.commands.every(
      (command) => !command.includes("npx"),
    ) === true,
  deepseekOnWindows?.steps,
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

// --- Pi (pinned npx environment, no global install) -------------------

const piMissingBoth = resolveInstallGuide(makeRunner("PI"), "macos");
check(
  "pi without node offers only the node step",
  piMissingBoth?.steps.map((step) => step.kind).join(",") === "node",
  piMissingBoth?.steps,
);

check(
  "pi never offers global install or auth steps",
  piMissingBoth?.steps.every(
    (step) => step.kind !== "install" && step.kind !== "auth",
  ) === true,
);

const piReady = resolveInstallGuide(
  makeRunner("PI", {
    installed: true,
    executable: true,
    availability: { type: "INSTALLATION_FOUND" },
    auth_state: "authenticated",
    node_available: true,
  }),
  "macos",
);
check(
  "pi with node is usable without any global pi install",
  piReady === null,
);
check(
  "pi points at the pinned-environment install hint",
  getInstallGuideEntry("PI")?.installHintKey === "agents.setup.piInstallHint",
);

const piWindows = resolveInstallGuide(makeRunner("PI"), "windows");
check(
  "pi is supported natively on windows via winget Node.js",
  piWindows?.windowsSupport === "supported" &&
    piWindows?.steps[0]?.commands[0] === "winget install OpenJS.NodeJS.LTS",
  piWindows?.steps,
);

const droidOnMac = resolveInstallGuide(makeRunner("DROID"), "macos");
check(
  "no auth commands are ever empty for guided runners",
  droidOnMac?.steps.some((step) => step.kind === "auth") === true,
);

// --- Missing npm/npx gating ----------------------------------------------

const ampMissingNpm = resolveInstallGuide(
  makeRunner("AMP", {
    installed: true,
    availability: { type: "INSTALLATION_FOUND" },
    auth_state: "authenticated",
    node_available: true,
    npm_available: false,
  }),
  "macos",
);
check(
  "npm executor installed but missing npm offers the npm step",
  ampMissingNpm?.steps.map((step) => step.kind).join(",") === "npm",
  ampMissingNpm?.steps,
);
check(
  "npm step reinstalls Node.js to restore npm",
  ampMissingNpm?.steps[0]?.commands[0]?.includes("nvm") === true,
  ampMissingNpm?.steps,
);
check(
  "npm executor reports only npm as missing",
  getMissingRuntimeTools(
    makeRunner("AMP", { node_available: true, npm_available: false }),
  ).join(",") === "npm",
);

const claudeMissingNpmAndNpx = makeRunner("CLAUDE_CODE", {
  installed: true,
  availability: { type: "INSTALLATION_FOUND" },
  auth_state: "authenticated",
  node_available: true,
  npm_available: false,
  npx_available: false,
});
check(
  "npx executor missing npm and npx reports both",
  getMissingRuntimeTools(claudeMissingNpmAndNpx).join(",") === "npm,npx",
);
const claudeMissingNpmGuide = resolveInstallGuide(
  claudeMissingNpmAndNpx,
  "windows",
);
check(
  "npx executor missing npm and npx offers a single npm remediation step",
  claudeMissingNpmGuide?.steps.map((step) => step.kind).join(",") === "npm",
  claudeMissingNpmGuide?.steps,
);
check(
  "windows npm step uses the winget Node.js installer",
  claudeMissingNpmGuide?.steps[0]?.commands[0] ===
    "winget install OpenJS.NodeJS.LTS",
);

const codexMissingNpxOnly = makeRunner("CODEX", {
  installed: true,
  availability: { type: "INSTALLATION_FOUND" },
  auth_state: "authenticated",
  node_available: true,
  npm_available: true,
  npx_available: false,
});
check(
  "npx executor missing only npx offers the npx step",
  resolveInstallGuide(codexMissingNpxOnly, "linux")
    ?.steps.map((step) => step.kind)
    .join(",") === "npx",
);
check(
  "npx executor missing only npx reports npx",
  getMissingRuntimeTools(codexMissingNpxOnly).join(",") === "npx",
);

const piMissingNode = makeRunner("PI", {
  node_available: false,
  npm_available: false,
  npx_available: false,
});
check(
  "missing node lists every unavailable required tool",
  getMissingRuntimeTools(piMissingNode).join(",") === "node,npm,npx",
);
check(
  "missing node collapses remediation to the node step",
  resolveInstallGuide(piMissingNode, "macos")
    ?.steps.map((step) => step.kind)
    .join(",") === "node",
);

check(
  "native-binary agents never report missing node tools",
  getMissingRuntimeTools(
    makeRunner("KIMI_CODE", {
      node_available: false,
      npm_available: false,
      npx_available: false,
    }),
  ).length === 0,
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
