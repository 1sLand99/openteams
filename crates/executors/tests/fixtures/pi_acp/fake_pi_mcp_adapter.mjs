#!/usr/bin/env node
/**
 * Fake pi-mcp-adapter: minimal MCP adapter for process tree testing.
 *
 * Stays alive until killed, simulating a real MCP adapter process
 * that would be spawned by the Pi coding agent.
 */
setInterval(() => {}, 1000);
