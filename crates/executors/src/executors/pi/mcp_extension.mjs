import { readFileSync } from "node:fs";
import { isAbsolute } from "node:path";
import { createMcpAdapter } from "pi-mcp-adapter";

export function readIsolatedConfig(snapshotPath) {
  if (!snapshotPath || !isAbsolute(snapshotPath)) throw new Error("Pi MCP snapshot path must be absolute");
  const config = JSON.parse(readFileSync(snapshotPath, "utf8"));
  if (!config || typeof config !== "object" || Array.isArray(config)) {
    throw new Error("Pi MCP snapshot must be an object");
  }
  if (!config.mcpServers || typeof config.mcpServers !== "object" || Array.isArray(config.mcpServers)) {
    throw new Error("Pi MCP snapshot mcpServers must be an object");
  }
  return config;
}

export default async function openteamsMcpExtension(pi) {
  const config = readIsolatedConfig(process.env.OPENTEAMS_PI_MCP_SNAPSHOT);
  return createMcpAdapter({ config })(pi);
}
