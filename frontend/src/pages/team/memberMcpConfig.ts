import { McpConfigStrategyGeneral } from "@/lib/mcpConfigStrategy";
import type { JsonValue, McpConfig } from "@/types";
import type {
  MemberExecutionConfig,
  UpdateProjectMemberRequest,
} from "../../../../shared/types";

// Canonical adapter-neutral member MCP layout: servers live under the single
// `mcpServers` key. The editor strategy only needs the path — there is no
// vendor template or preconfigured gallery for member-scoped config.
const MEMBER_MCP_STRATEGY_CONFIG: McpConfig = {
  servers_path: ["mcpServers"],
  template: {},
  servers: {},
  preconfigured: {},
  is_toml_config: false,
};

export type MemberMcpSource = {
  id: string;
  execution_config?: MemberExecutionConfig | null;
};

/**
 * Serialize the member's canonical MCP config for the editor. Both an absent
 * config (legacy member) and an explicit empty config render as
 * `{ "mcpServers": {} }`.
 */
export const memberMcpServersJson = (member: MemberMcpSource): string => {
  const servers = (member.execution_config?.mcp?.mcpServers ??
    {}) as JsonValue;
  return JSON.stringify({ mcpServers: servers }, null, 2);
};

/**
 * Parse and validate editor JSON into the canonical servers map. An empty
 * document is treated as an explicit empty server set. Throws `SyntaxError`
 * for malformed JSON and `Error` with a path-only message for shape issues;
 * neither ever contains config values.
 */
export const parseMemberMcpServers = (
  json: string,
): Record<string, JsonValue> => {
  const parsed = json.trim()
    ? (JSON.parse(json) as JsonValue)
    : { mcpServers: {} };
  McpConfigStrategyGeneral.validateFullConfig(
    MEMBER_MCP_STRATEGY_CONFIG,
    parsed,
  );
  return McpConfigStrategyGeneral.extractServersForApi(
    MEMBER_MCP_STRATEGY_CONFIG,
    parsed,
  );
};

/**
 * Build the project-member update that persists `servers`. The rest of the
 * execution config is copied from the current member snapshot because the
 * backend replaces `execution_config` wholesale.
 */
export const buildMemberMcpUpdate = (
  member: MemberMcpSource,
  servers: Record<string, JsonValue>,
): UpdateProjectMemberRequest => ({
  role: null,
  display_order: null,
  default_workspace_path: null,
  is_default: null,
  allowed_skill_ids: null,
  execution_config: {
    ...member.execution_config,
    mcp: { mcpServers: servers },
  },
});

/**
 * Present an MCP save/validation error without leaking secrets. Backend
 * member MCP errors only name the member, server and field — never values —
 * so anything that looks like raw JSON (e.g. an echoed request body) is
 * refused and replaced with the generic fallback.
 */
export const presentMemberMcpError = (
  err: unknown,
  invalidJsonMessage: string,
  fallbackMessage: string,
): string => {
  if (err instanceof SyntaxError) return invalidJsonMessage;
  if (!(err instanceof Error)) return fallbackMessage;
  if (/[{}]/u.test(err.message)) return fallbackMessage;
  return err.message;
};