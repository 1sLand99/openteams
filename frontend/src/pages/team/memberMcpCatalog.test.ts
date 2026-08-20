import assert from "node:assert/strict";
import type { McpConfig } from "../../types";
import {
  configuredMemberMcpCatalogServerKeys,
  toggleMemberMcpCatalogServer,
} from "./memberMcpConfig";

const catalog: McpConfig = {
  servers: {},
  servers_path: ["mcpServers"],
  template: { mcpServers: {} },
  preconfigured: {
    playwright: {
      command: "npx",
      args: ["@playwright/mcp@latest"],
    },
    meta: {
      playwright: { name: "Playwright" },
    },
  },
  is_toml_config: false,
};

const empty = JSON.stringify({ mcpServers: {} }, null, 2);
const added = toggleMemberMcpCatalogServer(catalog, empty, "playwright");
assert.deepEqual(JSON.parse(added), {
  mcpServers: {
    playwright: {
      command: "npx",
      args: ["@playwright/mcp@latest"],
    },
  },
});
assert.deepEqual(configuredMemberMcpCatalogServerKeys(catalog, added), [
  "playwright",
]);

const removed = toggleMemberMcpCatalogServer(catalog, added, "playwright");
assert.deepEqual(JSON.parse(removed), { mcpServers: {} });
assert.deepEqual(configuredMemberMcpCatalogServerKeys(catalog, removed), []);

assert.throws(
  () => toggleMemberMcpCatalogServer(catalog, empty, "unknown"),
  /Unknown preconfigured server/u,
);

console.log("Member MCP catalog tests passed.");
