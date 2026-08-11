# Pi Coding Agent

Pi is a first-class coding agent in OpenTeams, integrated via the Agent Client Protocol (ACP).

## Architecture

### Overview

Pi is launched through a fixed-version NPX process tree:

```
npx --yes \
  --package pi-acp@0.0.33 \
  --package @earendil-works/pi-coding-agent@0.83.0 \
  --package pi-mcp-adapter@2.18.0 \
  pi-acp
```

The `pi-acp` binary is the ACP server that wraps the Pi coding agent. OpenTeams embeds a `launcher.mjs` that:

1. Validates the pinned NPX environment (package versions in `node_modules`)
2. Enforces `--no-skills` (disabling Pi's own skill discovery)
3. Injects only member-authorized skill paths
4. Installs the approval extension (single gate for all tool calls)
5. Optionally installs the MCP extension for isolated MCP servers
6. Monitors the ACP parent process and terminates the tree on abnormal exit

### Process Tree

```
ACP Harness (Rust)
  └─ npx (resolved via PATH)
       └─ pi-acp (ACP server)
            └─ launcher.mjs
                 └─ pi (coding agent)
                      └─ pi-mcp-adapter (MCP bridge)
```

### Version Pins

All three npm packages use centrally-defined exact versions:

| Package | Version |
|---|---|
| `pi-acp` | `0.0.33` |
| `@earendil-works/pi-coding-agent` | `0.83.0` |
| `pi-mcp-adapter` | `2.18.0` |

These are defined in `crates/executors/src/executors/pi.rs` as constants:
- `PI_ACP_VERSION`
- `PI_CODING_AGENT_VERSION`
- `PI_MCP_ADAPTER_VERSION`

### Member Skill/MCP Isolation

- **Skills**: Pi always receives `--no-skills`. Only skills from the member's `allowed_skill_ids` (validated against the Skill Registry) are passed as `--skill <path>`. Paths must be canonical `SKILL.md` files within discovery roots (`~/.agents/skills` or `~/.pi/agent/skills`).
- **MCP**: Pi follows the shared CLI policy. When the member does not define `tools_enabled.mcpServers`, every enabled server in `~/.pi/agent/mcp.json` is included. An explicit member allowlist filters that set, and an explicit empty allowlist disables all MCP servers. The isolated snapshot always forces `hostConfigDiscovery` to `off`.
- **Secrets**: The `PiRuntimeSnapshot` Debug impl redacts MCP server details. Runtime files use `0600` permissions (launcher uses `0700`).

### Provider Config Sync

Pi model configuration is synchronized from OpenTeams provider settings to `~/.pi/agent/models.json`:

- Only `openteams-` prefixed providers are managed
- API keys are encoded as Pi literal values to prevent `$ENV`/`${ENV}`/`!command` injection
- Atomic file writes with `0600` permissions
- Invalid JSON, write failures, and rename failures preserve the original file
- Sync failures do not roll back settings saves

### Approval Policies

All tool calls (native and MCP) pass through a single approval gate in `approval_extension.mjs`:

| Mode | Behavior |
|---|---|
| `Ask` | Prompts user via `ctx.ui.confirm()` |
| `AutoAllow` | All tool calls allowed |
| `AutoReject` | All tool calls blocked |

## Offline Testing

### Default CI Tests (No npm, No Network)

The default test suite uses repository-local fake fixtures in `crates/executors/tests/fixtures/pi_acp/`:

| Fixture | Purpose |
|---|---|
| `fake_npx.sh` | Shell script that resolves commands from local bin without contacting npm |
| `fake_pi_acp.mjs` | Node.js ACP server implementing JSON-RPC 2.0 over stdio |
| `fake_pi.mjs` | Fake Pi coding agent for process tree testing |
| `fake_pi_mcp_adapter.mjs` | Fake MCP adapter that stays alive |

#### Running Offline Tests

```bash
# Run all Pi tests with empty npm cache (proves no npm dependency)
NPM_CONFIG_CACHE=/tmp/empty-cache cargo test -p executors --features qa-mode pi -- --nocapture

# Run the offline fixture integration test
cargo test -p executors --features qa-mode --test pi_acp_fixture -- --nocapture

# Run services-level Pi tests
cargo test -p services --features qa-mode pi -- --nocapture
cargo test -p services --features qa-mode chat_runner
cargo test -p services --features qa-mode workflow::runtime
cargo test -p services --features qa-mode workflow::orchestrator
```

#### What the Offline Tests Cover

- **Initialize**: Protocol version, agent capabilities, auth methods
- **Model refresh**: ACP probe discovers `offline-model`
- **New session**: `session/new` with config options (model selector)
- **Streaming**: Agent message chunks, usage updates, tool call events
- **Follow-up**: `session/resume` with session ID reuse
- **Cancel**: `session/cancel` notification with `stopReason: "cancelled"`
- **Startup failure**: Non-executable `pi-acp` produces typed error
- **Approval policies**: Ask, AutoAllow, AutoReject all produce consistent results
- **MCP policy/isolation**: Missing allowlist includes all configured servers; explicit allowlists filter them; an explicit empty allowlist produces an empty snapshot
- **Secret safety**: Fixture files contain no API keys or secrets
- **Token usage**: Usage and TokenUsage events projected correctly
- **No npm access**: Fake npx creates no `.npm` cache directory

### Fixed Version Upgrade

To upgrade Pi package versions:

1. Update the constants in `crates/executors/src/executors/pi.rs`:
   ```rust
   pub const PI_ACP_VERSION: &str = "0.0.34";      // new version
   pub const PI_CODING_AGENT_VERSION: &str = "0.84.0";
   pub const PI_MCP_ADAPTER_VERSION: &str = "2.19.0";
   ```

2. Update the launcher.mjs version constants:
   ```javascript
   const PI_VERSION = "0.84.0";
   const MCP_VERSION = "2.19.0";
   ```

3. Update the offline fixture test assertions if needed.

4. Run offline tests to verify:
   ```bash
   cargo test -p executors --features qa-mode pi -- --nocapture
   ```

5. Run the real NPX smoke test (see below) with the new versions.

## Error Troubleshooting

### ACP Startup Failed

| Error | Cause | Fix |
|---|---|---|
| `ACP startup failed: connection refused` | `pi-acp` process exited before responding | Check `node` and `npx` are available; check pinned versions |
| `ACP startup failed: Unknown sessionId` | Follow-up session ID not recognized by agent | Expected if session was from a previous process; mapped to `FollowUpNotSupported` |
| `ACP startup failed: timed out after 12 seconds` | Probe timed out | Check if `pi-acp` is hanging; verify fake fixtures are executable |
| `ACP startup failed: AuthRequired` | Authentication method not advertised or expired | Configure `acp.auth.method_id` or provide credentials |

### Process Tree Cleanup

If Pi processes survive after cleanup:

1. Check that `kill_on_drop(true)` is set on the process
2. Verify the launcher's orphan cleanup (watches `process.ppid` every 250ms)
3. Use `kill_process_group` to reap the entire process tree
4. Check `pids.json` for recorded process IDs

### Provider Config Sync

If `~/.pi/agent/models.json` is not updated:

1. Check that the provider has an `openteams-` prefix
2. Verify the API key is not empty
3. Check `PiModelsSyncDiagnostic` for structured error info
4. Ensure `~/.pi/agent/` directory is writable
5. Verify file permissions are `0600`

## Real NPX Smoke Testing

**These tests are NOT part of default CI. They require npm cache or network access.**

### Prerequisites

- Node.js and npx installed and on PATH
- npm cache populated with the pinned packages, OR network access to npm registry
- At least one provider API key configured (Anthropic, OpenAI, Google, or OpenRouter)

### Running Real Smoke Tests

```bash
# Pi models round-trip with real Pi 0.83 resolver (requires npm cache)
cargo test -p services --features qa-mode pi_models::tests::pi_api_keys_round_trip_with_real_fixed_pi_083_resolver -- --ignored --nocapture

# Set up isolated HOME for real NPX testing
export PI_SMOKE_HOME=$(mktemp -d)
export HOME=$PI_SMOKE_HOME

# Populate npm cache (requires network)
npx --yes --package pi-acp@0.0.33 --package @earendil-works/pi-coding-agent@0.83.0 --package pi-mcp-adapter@2.18.0 pi-acp --version

# Run real Pi ACP lifecycle (requires provider credentials)
cargo test -p executors --features qa-mode --test pi_acp_fixture -- --ignored --nocapture
```

### Smoke Test Checklist

When credentials are available, verify:

1. **Free Chat**: New session, prompt, streaming response, token usage, file changes
2. **Workflow**: Pi node execution, events, reducer transitions
3. **Follow-up**: Session resume with previous session ID
4. **Cancel**: Mid-prompt cancellation with `session/cancel`
5. **Approval**: All three policies (Ask, AutoAllow, AutoReject)
6. **Skill**: One authorized skill passed to Pi
7. **MCP**: One authorized MCP server in isolated snapshot

### Credential Blocker

If no provider API keys are available:

- Real NPX smoke tests cannot produce model responses
- The `pi_api_keys_round_trip_with_real_fixed_pi_083_resolver` test remains `#[ignore]`d
- Offline fixture tests provide complete coverage of the ACP protocol lifecycle
- Provider config sync is tested with the local `pi_0_83_resolve_config_value_fixture.mjs` (no network)
