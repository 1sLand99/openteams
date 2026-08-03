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
export type InstallGuideStepKind = "node" | "install" | "auth";

export interface AgentInstallGuideEntry {
  requiresNode: boolean;
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

const INSTALL_GUIDES: Partial<Record<BaseCodingAgent, AgentInstallGuideEntry>> =
  {
    CLAUDE_CODE: {
      requiresNode: true,
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
    GEMINI: {
      requiresNode: true,
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
      documentationUrl:
        "https://moonshotai.github.io/kimi-cli/en/guides/getting-started.html",
      windowsSupport: "wsl_only",
      installCommands: {
        posix: ["curl -LsSf https://code.kimi.com/install.sh | bash"],
        windows: null,
      },
      authCommands: ["kimi login"],
    },
    QODER_CLI: {
      requiresNode: false,
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
    "runner_type" | "installed" | "auth_state" | "node_available"
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

  if (runnerNeedsInstall(runner)) {
    if (entry.requiresNode && !runner.node_available) {
      steps.push({ kind: "node", commands: NODE_INSTALL_COMMANDS[commandSet] });
    }
    const installCommands =
      commandSet === "windows"
        ? (entry.installCommands.windows ?? entry.installCommands.posix)
        : entry.installCommands.posix;
    steps.push({ kind: "install", commands: installCommands });
  }

  if (runnerNeedsAuth(runner) && entry.authCommands) {
    steps.push({
      kind: "auth",
      commands: entry.authCommands,
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