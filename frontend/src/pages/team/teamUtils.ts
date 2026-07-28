import type {
  BackendChatAgent,
  BackendChatSessionAgent,
  BaseCodingAgent,
  ChatSessionAgentState,
} from "@/types";
import type {
  AcpConfigChoice,
  AcpConfigOptionSnapshot,
  AcpConfigOverride,
  AcpConfigValue,
  AcpExecutionOptions,
  ProjectMemberWithRuntime,
} from "../../../../shared/types";
import { getRunnerLabel } from "../agent-runtime/agentRuntimeViewModel";

export type MemberExecutionConfig = {
  runner_type?: BaseCodingAgent | null;
  model_name?: string | null;
  thinking_effort?: string | null;
  model_variant?: string | null;
  acp?: AcpExecutionOptions | null;
};

export type ProjectMemberWithExecution = ProjectMemberWithRuntime & {
  execution_config?: MemberExecutionConfig | null;
};

export type SessionAgentLookup = {
  byAgentId: Map<string, BackendChatSessionAgent>;
  byMemberId: Map<string, BackendChatSessionAgent>;
};

export type MemberRunState = "idle" | "running" | "dead";

export const defaultOptionId = "__openteams_default__";
export const nonLeadRole = "member";

export const cx = (...classes: Array<string | false | null | undefined>) =>
  classes.filter(Boolean).join(" ");

export const normalizeRunnerType = (
  value?: string | null,
): BaseCodingAgent | null => {
  if (!value) return null;
  let normalized = value.trim().replaceAll("-", "_").toUpperCase();
  if (normalized === "OPENTEAMS_CLI") {
    normalized = "OPEN_TEAMS_CLI";
  }
  const known: BaseCodingAgent[] = [
    "CLAUDE_CODE",
    "AMP",
    "GEMINI",
    "CODEX",
    "OPENCODE",
    "OPEN_TEAMS_CLI",
    "CURSOR_AGENT",
    "QWEN_CODE",
    "COPILOT",
    "DROID",
    "KIMI_CODE",
  ];
  return known.includes(normalized as BaseCodingAgent)
    ? (normalized as BaseCodingAgent)
    : null;
};

export const trimOrNull = (value: string): string | null => {
  const trimmed = value.trim();
  return trimmed ? trimmed : null;
};

export const canonicalRuntimeModelId = (modelId: string): string => {
  let canonical = modelId.trim();
  let previous = "";
  while (canonical !== previous) {
    previous = canonical;
    canonical = canonical
      .replace(/\([^()\r\n]+\)$/u, "")
      .replace(/\[[^[\]\r\n]+\]$/u, "")
      .trimEnd();
  }
  return canonical;
};

const semanticModelKey = (modelId: string) =>
  canonicalRuntimeModelId(modelId)
    .toLocaleLowerCase()
    .split(/[^\p{L}\p{N}]+/u)
    .filter(Boolean)
    .join("\u0000");

const bareModelId = (modelId: string) =>
  canonicalRuntimeModelId(modelId).split("/").at(-1) ?? "";

const modelIdMatchScore = (
  expected: string,
  candidate: string,
): number | null => {
  const left = expected.trim();
  const right = candidate.trim();
  if (!left || !right) return null;
  if (left === right) return 100;
  if (left.toLocaleLowerCase() === right.toLocaleLowerCase()) return 95;

  const leftCanonical = canonicalRuntimeModelId(left);
  const rightCanonical = canonicalRuntimeModelId(right);
  if (leftCanonical === rightCanonical) return 90;
  if (
    leftCanonical.toLocaleLowerCase() === rightCanonical.toLocaleLowerCase()
  ) {
    return 85;
  }
  if (
    bareModelId(leftCanonical).toLocaleLowerCase() ===
    bareModelId(rightCanonical).toLocaleLowerCase()
  ) {
    return 80;
  }
  const leftKey = semanticModelKey(leftCanonical);
  const rightKey = semanticModelKey(rightCanonical);
  if (leftKey && leftKey === rightKey) {
    return 70;
  }
  const leftBareKey = semanticModelKey(bareModelId(leftCanonical));
  const rightBareKey = semanticModelKey(bareModelId(rightCanonical));
  if (leftBareKey && leftBareKey === rightBareKey) {
    return 60;
  }
  return null;
};

export const resolveUniqueAcpChoice = (
  desired: string,
  choices: AcpConfigChoice[],
): AcpConfigChoice | null => {
  const exact = choices.find((choice) => choice.value === desired);
  if (exact) return exact;

  let best: { choice: AcpConfigChoice; score: number } | null = null;
  let ambiguous = false;
  for (const choice of choices) {
    const valueScore = modelIdMatchScore(desired, choice.value);
    const nameScore = modelIdMatchScore(desired, choice.name);
    const score =
      valueScore ??
      (nameScore === null ? null : Math.max(0, nameScore - 10));
    if (score === null) continue;
    if (!best || score > best.score) {
      best = { choice, score };
      ambiguous = false;
    } else if (score === best.score) {
      ambiguous = true;
    }
  }
  return ambiguous ? null : (best?.choice ?? null);
};

export type AcpConfigSemanticCategory = "model" | "thought_level";

const semanticConfigKey = (value: string) =>
  value
    .toLocaleLowerCase()
    .split(/[^\p{L}\p{N}]+/u)
    .filter(Boolean)
    .join("");

export const acpOptionSemanticCategory = (
  option: AcpConfigOptionSnapshot,
): AcpConfigSemanticCategory | null => {
  const categoryKey = semanticConfigKey(option.category ?? "");
  if (categoryKey === "model") return "model";
  if (categoryKey === "thoughtlevel") return "thought_level";
  if (categoryKey) return null;

  const keys = [option.id, option.name].map(semanticConfigKey);
  if (keys.some((key) => key === "model" || key.endsWith("model"))) {
    return "model";
  }
  if (
    keys.some(
      (key) => key === "thoughtlevel" || key.endsWith("thoughtlevel"),
    )
  ) {
    return "thought_level";
  }
  return null;
};

export type AcpSelectConfigOptionSnapshot = AcpConfigOptionSnapshot & {
  type: "select";
  current_value: string;
  options: AcpConfigChoice[];
};

export const findAcpSelectConfigOption = (
  options: AcpConfigOptionSnapshot[],
  category: AcpConfigSemanticCategory,
): AcpSelectConfigOptionSnapshot | null => {
  const matches = options.filter(
    (option): option is AcpSelectConfigOptionSnapshot =>
      option.type === "select" &&
      acpOptionSemanticCategory(option) === category,
  );
  return matches.length === 1 ? matches[0] : null;
};

const isModeConfigKey = (value?: string | null) => {
  const key = semanticConfigKey(value ?? "");
  return (
    key === "mode" ||
    key === "sessionmode" ||
    key === "agentmode" ||
    key === "workmode" ||
    key === "workingmode"
  );
};

export const withoutAcpModeOverrides = (
  overrides: AcpConfigOverride[],
): AcpConfigOverride[] =>
  overrides.filter(
    (override) =>
      !isModeConfigKey(override.category_snapshot) &&
      !isModeConfigKey(override.option_id) &&
      !isModeConfigKey(override.label_snapshot),
  );

export const withoutAcpThoughtLevelOverrides = (
  overrides: AcpConfigOverride[],
): AcpConfigOverride[] =>
  overrides.filter((override) => {
    const categoryKey = semanticConfigKey(override.category_snapshot ?? "");
    if (categoryKey === "thoughtlevel") return false;
    if (categoryKey) return true;
    return ![override.option_id, override.label_snapshot ?? ""]
      .map(semanticConfigKey)
      .some(
        (key) => key === "thoughtlevel" || key.endsWith("thoughtlevel"),
      );
  });

export const effectiveAcpConfigValue = (
  option: AcpConfigOptionSnapshot,
  overrides: AcpConfigOverride[],
  legacyModelName: string,
  legacyThinkingEffort: string,
): AcpConfigValue => {
  const persisted = overrides.find(
    (override) => override.option_id === option.id,
  );
  if (persisted) return persisted.value;
  if (option.type === "boolean") {
    return { type: "boolean", value: option.current_value };
  }

  const semanticCategory = acpOptionSemanticCategory(option);
  const legacyValue =
    semanticCategory === "model"
      ? legacyModelName
      : semanticCategory === "thought_level"
        ? legacyThinkingEffort
        : "";
  const migrated = legacyValue
    ? resolveUniqueAcpChoice(legacyValue, option.options)
    : null;
  return {
    type: "value_id",
    value: migrated?.value ?? option.current_value,
  };
};

export const compactRunnerLabel = (
  runner?: BaseCodingAgent | null,
  fallback = "Runtime",
) => (runner ? getRunnerLabel(runner) : fallback);

export const memberName = (
  member: ProjectMemberWithExecution,
  agent?: BackendChatAgent | null,
) => member.member_name?.trim() || agent?.name || member.role || "Member";

export const normalizeMemberRunState = (
  state?: ChatSessionAgentState | null,
): MemberRunState => {
  if (state === "dead") return "dead";
  if (
    state === "running" ||
    state === "stopping" ||
    state === "waitingapproval"
  ) {
    return "running";
  }
  return "idle";
};

export const buildSessionAgentLookup = (
  sessionAgents: BackendChatSessionAgent[],
): SessionAgentLookup => {
  const byMemberId = new Map<string, BackendChatSessionAgent>();
  const byAgentId = new Map<string, BackendChatSessionAgent>();
  for (const sessionAgent of sessionAgents) {
    if (sessionAgent.project_member_id) {
      byMemberId.set(sessionAgent.project_member_id, sessionAgent);
    }
    byAgentId.set(sessionAgent.agent_id, sessionAgent);
  }
  return { byAgentId, byMemberId };
};
