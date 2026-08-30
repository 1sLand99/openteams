import type { AgentRuntimeStatus, BaseCodingAgent } from "@/types";

/**
 * Static install/sign-in guidance for agent runtimes.
 *
 * OpenTeams never installs or authenticates agents on the user's behalf:
 * it only detects state, shows the official commands, copies them and opens
 * a system terminal. Commands below mirror the pinned packages in
 * `crates/executors` and the official docs under `docs/agents/`.
 */

export type InstallGuidePlatform = "macos" | "linux" | "windows";
export type WindowsSupport = "supported" | "wsl_only";
export type InstallGuideStepKind = "node" | "npm" | "npx" | "install" | "auth";

/** Runtime tools the backend probes per runner (node/npm/npx availability). */
export type MissingRuntimeTool = "node" | "npm" | "npx";

export interface AgentInstallGuideEntry {
  requiresNode: boolean;
  /** Installed via `npm install -g`; cannot run without npm. */
  requiresNpm: boolean;
  /** Executed through `npx`; cannot run without npx (implies npm). */
  requiresNpx: boolean;
  documentationUrl: string;
  /** macOS/Linux are always supported; Windows may require WSL. */
  windowsSupport: WindowsSupport;
  installCommands: {
    posix: string[];
    /** Null when Windows is only supported through WSL (reuse posix). */
    windows: string[] | null;
  };
  /** Null when the agent does not need an interactive sign-in. */
  authCommands: string[] | null;
  /** Command typed inside the running CLI to finish sign-in, e.g. "/login". */
  authFollowUpCommand?: string;
  /** Env vars (e.g. a PAT) that authenticate non-interactively; shown as a hint. */
  authEnvVars?: string[];
  /** Locale key overriding `agents.setup.installHint` for this runner. */
  installHintKey?: string;
}

export interface InstallGuideStep {
  kind: InstallGuideStepKind;
  commands: string[];
  authFollowUpCommand?: string;
}

export interface ResolvedInstallGuide {
  entry: AgentInstallGuideEntry;
  platform: InstallGuidePlatform;
  windowsSupport: WindowsSupport;
  steps: InstallGuideStep[];
}

const NODE_INSTALL_COMMANDS: Record<"posix" | "windows", string[]> = {
  posix: [
    "curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash",
    "nvm install --lts",
  ],
  windows: ["winget install OpenJS.NodeJS.LTS"],
};

const npmInstall = (pkg: string): string[] => [`npm install -g ${pkg}`];

const DEEPSEEK_HARNESS_REVISION =
  "99f6f02fecdb7dff40c3fbc9470f5907c29f74ca";
const DEEPSEEK_HARNESS_PNPM_VERSION = "11.7.0";

const INSTALL_GUIDES: Partial<Record<BaseCodingAgent, AgentInstallGuideEntry>> =
  {
    CLAUDE_CODE: {
      requiresNode: true,
      requiresNpm: true,
      requiresNpx: true,
      documentationUrl: "https://docs.claude.com/en/docs/claude-code/quickstart",
      windowsSupport: "supported",
      installCommands: {
        posix: npmInstall("@anthropic-ai/claude-code"),
        windows: npmInstall("@anthropic-ai/claude-code"),
      },
      authCommands: ["claude"],
      authFollowUpCommand: "/login",
    },
    CODEX: {
      requiresNode: true,
      requiresNpm: true,
      requiresNpx: true,
      documentationUrl: "https://github.com/openai/codex",
      windowsSupport: "supported",
      installCommands: {
        posix: npmInstall("@openai/codex"),
        windows: npmInstall("@openai/codex"),
      },
      authCommands: ["codex login"],
    },
    OPENCODE: {
      requiresNode: true,
      requiresNpm: true,
      requiresNpx: true,
      documentationUrl: "https://opencode.ai",
      windowsSupport: "supported",
      installCommands: {
        posix: npmInstall("opencode-ai"),
        windows: npmInstall("opencode-ai"),
      },
      authCommands: ["opencode auth login"],
    },
    AMP: {
      requiresNode: true,
      requiresNpm: true,
      requiresNpx: false,
      documentationUrl: "https://ampcode.com/manual",
      windowsSupport: "supported",
      installCommands: {
        posix: npmInstall("@sourcegraph/amp"),
        windows: npmInstall("@sourcegraph/amp"),
      },
      authCommands: ["amp login"],
    },
    COPILOT: {
      requiresNode: true,
      requiresNpm: true,
      requiresNpx: false,
      documentationUrl:
        "https://docs.github.com/en/copilot/how-tos/use-copilot-agents/use-copilot-cli",
      windowsSupport: "supported",
      installCommands: {
        posix: npmInstall("@github/copilot"),
        windows: npmInstall("@github/copilot"),
      },
      authCommands: ["copilot"],
      authFollowUpCommand: "/login",
    },
    DEEPSEEK_HARNESS: {
      requiresNode: true,
      requiresNpm: false,
      requiresNpx: false,
      documentationUrl: "https://github.com/deepseek-ai/deepseek-harness",
      windowsSupport: "supported",
      installCommands: {
        posix: [
          `npm install --global pnpm@${DEEPSEEK_HARNESS_PNPM_VERSION}`,
          'git clone https://github.com/deepseek-ai/deepseek-harness.git "$HOME/deepseek-harness"',
          `git -C "$HOME/deepseek-harness" checkout ${DEEPSEEK_HARNESS_REVISION}`,
          'pnpm --dir "$HOME/deepseek-harness" install --frozen-lockfile',
          'pnpm --dir "$HOME/deepseek-harness" run build',
        ],
        windows: [
          `npm install --global pnpm@${DEEPSEEK_HARNESS_PNPM_VERSION}`,
          'git clone https://github.com/deepseek-ai/deepseek-harness.git "$HOME\\deepseek-harness"',
          `git -C "$HOME\\deepseek-harness" checkout ${DEEPSEEK_HARNESS_REVISION}`,
          'pnpm --dir "$HOME\\deepseek-harness" install --frozen-lockfile',
          'pnpm --dir "$HOME\\deepseek-harness" run build',
        ],
      },
      authCommands: null,
      authEnvVars: ["DEEPSEEK_API_KEY"],
    },
    HERMES: {
      requiresNode: false,
      requiresNpm: false,
      requiresNpx: false,
      documentationUrl: "https://github.com/NousResearch/hermes-agent",
      windowsSupport: "wsl_only",
      installCommands: {
        posix: ["pip install hermes-agent"],
        windows: null,
      },
      authCommands: ["hermes acp --setup"],
    },
    GEMINI: {
      requiresNode: true,
      requiresNpm: true,
      requiresNpx: false,
      documentationUrl: "https://github.com/google-gemini/gemini-cli",
      windowsSupport: "supported",
      installCommands: {
        posix: npmInstall("@google/gemini-cli"),
        windows: npmInstall("@google/gemini-cli"),
      },
      authCommands: ["gemini"],
    },
    QWEN_CODE: {
      requiresNode: true,
      requiresNpm: true,
      requiresNpx: false,
      documentationUrl: "https://github.com/QwenLM/qwen-code",
      windowsSupport: "supported",
      installCommands: {
        posix: npmInstall("@qwen-code/qwen-code"),
        windows: npmInstall("@qwen-code/qwen-code"),
      },
      authCommands: ["qwen"],
      authFollowUpCommand: "/auth",
    },
    CURSOR_AGENT: {
      requiresNode: false,
      requiresNpm: false,
      requiresNpx: false,
      documentationUrl: "https://docs.cursor.com/en/cli/installation",
      windowsSupport: "wsl_only",
      installCommands: {
        posix: ["curl https://cursor.com/install -fsS | bash"],
        windows: null,
      },
      authCommands: ["cursor-agent login"],
    },
    DROID: {
      requiresNode: false,
      requiresNpm: false,
      requiresNpx: false,
      documentationUrl: "https://docs.factory.ai/droid-cli/cli-reference",
      windowsSupport: "wsl_only",
      installCommands: {
        posix: ["curl -fsSL https://app.factory.ai/cli | sh"],
        windows: null,
      },
      authCommands: ["droid"],
      authFollowUpCommand: "/login",
    },
    KIMI_CODE: {
      requiresNode: false,
      requiresNpm: false,
      requiresNpx: false,
      documentationUrl:
        "https://moonshotai.github.io/kimi-cli/en/guides/getting-started.html",
      windowsSupport: "wsl_only",
      installCommands: {
        posix: ["curl -LsSf https://code.kimi.com/install.sh | bash"],
        windows: null,
      },
      authCommands: ["kimi login"],
    },
    KIRO_CLI: {
      requiresNode: false,
      requiresNpm: false,
      requiresNpx: false,
      documentationUrl: "https://kiro.dev/docs/cli/",
      windowsSupport: "supported",
      installCommands: {
        posix: ["curl -fsSL https://cli.kiro.dev/install | bash"],
        windows: ["irm https://cli.kiro.dev/install.ps1 | iex"],
      },
      authCommands: ["kiro-cli login"],
      authEnvVars: ["KIRO_API_KEY"],
    },
    // Pi runs through OpenTeams' pinned npx package set, so there is no
    // global install or interactive login step: Node.js with npm/npx (both
    // bundled with Node) is the only prerequisite.
    PI: {
      requiresNode: true,
      requiresNpm: true,
      requiresNpx: true,
      documentationUrl: "https://github.com/badlogic/pi-mono",
      windowsSupport: "supported",
      installCommands: {
        posix: [],
        windows: [],
      },
      authCommands: null,
      installHintKey: "agents.setup.piInstallHint",
    },
    QODER_CLI: {
      requiresNode: false,
      requiresNpm: false,
      requiresNpx: false,
      documentationUrl: "https://docs.qoder.com/en/cli/install",
      windowsSupport: "supported",
      installCommands: {
        posix: ["curl -fsSL https://qoder.com/install | bash"],
        windows: ["irm https://qoder.com/install.ps1 | iex"],
      },
      authCommands: ["qodercli"],
      authFollowUpCommand: "/login",
      authEnvVars: ["QODER_PERSONAL_ACCESS_TOKEN"],
    },
  };

export const getInstallGuideEntry = (
  runner: BaseCodingAgent,
): AgentInstallGuideEntry | null => INSTALL_GUIDES[runner] ?? null;

/**
 * Runtime tools the runner needs but the backend did not detect. Reports
 * every missing tool explicitly: an npx executor missing npm and npx lists
 * both, even though a single Node.js reinstall restores them.
 */
export const getMissingRuntimeTools = (
  runner: Pick<
    AgentRuntimeStatus,
    "runner_type" | "node_available" | "npm_available" | "npx_available"
  >,
): MissingRuntimeTool[] => {
  const entry = getInstallGuideEntry(runner.runner_type);
  if (!entry) return [];
  const missing: MissingRuntimeTool[] = [];
  if (entry.requiresNode && !runner.node_available) missing.push("node");
  if (entry.requiresNpm && !runner.npm_available) missing.push("npm");
  if (entry.requiresNpx && !runner.npx_available) missing.push("npx");
  return missing;
};

/** Bundled runtimes (OpenTeams CLI) ship with the app and need no setup. */
export const runnerNeedsInstall = (
  runner: Pick<AgentRuntimeStatus, "installed">,
): boolean => !runner.installed;

export const runnerNeedsAuth = (
  runner: Pick<AgentRuntimeStatus, "auth_state">,
): boolean => runner.auth_state !== "authenticated";

export function detectClientPlatform(input?: {
  userAgentDataPlatform?: string | null;
  platform?: string | null;
  userAgent?: string | null;
}): InstallGuidePlatform {
  const nav = typeof navigator !== "undefined" ? navigator : null;
  const navUserAgentData = (
    nav as (Navigator & { userAgentData?: { platform?: string } }) | null
  )?.userAgentData;
  const candidates = input
    ? [input.userAgentDataPlatform, input.platform, input.userAgent]
    : [navUserAgentData?.platform, nav?.platform, nav?.userAgent];

  for (const candidate of candidates) {
    const value = candidate?.toLowerCase();
    if (!value) continue;
    if (value.includes("mac")) return "macos";
    if (value.includes("win")) return "windows";
    if (value.includes("linux")) return "linux";
  }
  return "linux";
}

export function resolveInstallGuide(
  runner: Pick<
    AgentRuntimeStatus,
    | "runner_type"
    | "installed"
    | "auth_state"
    | "node_available"
    | "npm_available"
    | "npx_available"
  >,
  platform: InstallGuidePlatform,
): ResolvedInstallGuide | null {
  const entry = getInstallGuideEntry(runner.runner_type);
  if (!entry) return null;

  const commandSet: "posix" | "windows" =
    platform === "windows" && entry.installCommands.windows !== null
      ? "windows"
      : "posix";
  const steps: InstallGuideStep[] = [];

  // Missing runtime tools block execution even when the agent itself is
  // installed, so remediation comes first regardless of install state. A
  // single step is enough: Node.js bundles npm, and npm bundles npx, so
  // reinstalling the Node.js LTS release restores whichever tool is gone.
  const missingTools = getMissingRuntimeTools(runner);
  if (missingTools.includes("node")) {
    steps.push({ kind: "node", commands: NODE_INSTALL_COMMANDS[commandSet] });
  } else if (missingTools.includes("npm")) {
    steps.push({ kind: "npm", commands: NODE_INSTALL_COMMANDS[commandSet] });
  } else if (missingTools.includes("npx")) {
    steps.push({ kind: "npx", commands: NODE_INSTALL_COMMANDS[commandSet] });
  }

  if (runnerNeedsInstall(runner)) {
    const installCommands =
      commandSet === "windows"
        ? (entry.installCommands.windows ?? entry.installCommands.posix)
        : entry.installCommands.posix;
    if (installCommands.length > 0) {
      steps.push({ kind: "install", commands: installCommands });
    }
  }

  if (
    runnerNeedsAuth(runner) &&
    (entry.authCommands || (entry.authEnvVars?.length ?? 0) > 0)
  ) {
    steps.push({
      kind: "auth",
      commands: entry.authCommands ?? [],
      authFollowUpCommand: entry.authFollowUpCommand,
    });
  }

  if (steps.length === 0) return null;
  return { entry, platform, windowsSupport: entry.windowsSupport, steps };
}

/** Flattened commands joined for the clipboard (CRLF on Windows). */
export function joinGuideCommands(
  steps: InstallGuideStep[],
  platform: InstallGuidePlatform,
): string {
  const separator = platform === "windows" ? "\r\n" : "\n";
  return steps.flatMap((step) => step.commands).join(separator);
}