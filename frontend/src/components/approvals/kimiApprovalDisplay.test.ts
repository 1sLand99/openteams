import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import {
  approvalCommand,
  type ApprovalTranslate,
} from './executorApprovalPresentation';
import type { ChatExecutorApprovalRequest } from '../../../../shared/types';

const translate: ApprovalTranslate = (_key, fallback) => fallback;

// ── Kimi Code Bash approval with a command > 50 characters ──────────────

// A realistic long Bash command that Kimi Code might request to run.
const longCommand =
  'find . -name "*.rs" -not -path "./target/*" | xargs grep -l "approval" | head -20';

assert.ok(
  longCommand.length > 50,
  `command should exceed 50 characters (got ${longCommand.length})`,
);

// Shape 1: Post-normalization display_input (what the backend persists after
// sanitize_display_input hoists `command` to the top level).
const normalizedRequest = {
  id: 'kimi-approval-1',
  session_id: 'session-1',
  session_agent_id: 'agent-1',
  run_id: 'run-1',
  workflow_execution_id: null,
  workflow_step_id: null,
  runner: 'KIMI_CODE',
  tool_call_id: 'kimi-tool-1',
  tool_name: 'Run command',
  display_input: {
    command: longCommand,
    tool_call: {
      title: 'Run command',
      rawInput: {
        command: longCommand,
      },
    },
  },
  options: [
    { option_id: 'allow-once', kind: 'allow_once', label: 'Allow once' },
    { option_id: 'allow-always', kind: 'allow_always', label: 'Allow always' },
    { option_id: 'deny-once', kind: 'reject_once', label: 'Deny' },
  ],
  status: 'pending',
  selected_option_id: null,
  processed_by: null,
  expires_at: '2026-08-01T12:00:00Z',
  resolved_at: null,
  created_at: '2026-08-01T09:00:00Z',
  updated_at: '2026-08-01T09:00:00Z',
} as unknown as ChatExecutorApprovalRequest;

// Shape 2: Raw ACP display_input (only tool_call.rawInput.command, no
// top-level `command` — this is what the frontend would receive if backend
// normalization hadn't run yet, or for an older persisted row).
const rawAcpRequest = {
  ...normalizedRequest,
  display_input: {
    tool_call: {
      title: 'Run command',
      rawInput: {
        command: longCommand,
      },
    },
  },
} as unknown as ChatExecutorApprovalRequest;

// Shape 3: snake_case variant (raw_input instead of rawInput).
const snakeCaseRequest = {
  ...normalizedRequest,
  display_input: {
    tool_call: {
      title: 'Run command',
      raw_input: {
        command: longCommand,
      },
    },
  },
} as unknown as ChatExecutorApprovalRequest;

for (const [label, request] of [
  ['normalized', normalizedRequest],
  ['raw-acp', rawAcpRequest],
  ['snake-case', snakeCaseRequest],
] as const) {
  const cmd = approvalCommand(request);

  // 1. The full command must be extracted — not null.
  assert.ok(cmd !== null, `[${label}] approvalCommand must not return null`);

  // 2. The extracted command must be the full long command, not "Bash" or
  //    "Run command" or any truncated summary.
  assert.equal(
    cmd,
    longCommand,
    `[${label}] approvalCommand must return the full command, not a label/truncation`,
  );
  assert.ok(
    cmd.length > 50,
    `[${label}] extracted command must exceed 50 characters (got ${cmd?.length})`,
  );

  // 3. The tool_name is NOT the command — it's a separate label.
  assert.notEqual(
    request.tool_name,
    cmd,
    `[${label}] tool_name must differ from the extracted command`,
  );
}

// ── Verify FreeChatApprovalTray renders the command inline ──────────────

const traySource = readFileSync(
  fileURLToPath(new URL('./FreeChatApprovalTray.tsx', import.meta.url)),
  'utf8',
);

assert.ok(
  traySource.includes('const displayedAction = command ?? request.tool_name;'),
  'tray must prefer the extracted command over the tool name',
);
assert.ok(
  traySource.includes('title={displayedAction}'),
  'tray must expose the full command as the inline value title',
);
assert.ok(
  traySource.includes('{displayedAction}'),
  'tray must render the command where the tool name previously appeared',
);
assert.ok(
  traySource.includes('min-w-0 flex-1 truncate whitespace-nowrap'),
  'tray must truncate long commands inside the flexible action column',
);
assert.ok(
  traySource.includes('grid grid-cols-[minmax(0,1fr)_auto]'),
  'tray must reserve a separate auto-width column for approval buttons',
);
assert.ok(
  !traySource.includes('commandExpanded'),
  'tray must not add expand/collapse state',
);
assert.ok(
  !traySource.includes('writeClipboardViaBridge(command)'),
  'tray must not add command copy controls',
);
assert.ok(
  !traySource.includes('{command && ('),
  'tray must not render a separate command row',
);

console.log('Kimi Code >50-char Bash approval display tests passed');
