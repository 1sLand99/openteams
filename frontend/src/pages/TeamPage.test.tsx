// Behavioral tests for the member-scoped MCP editor state machine.
//
// No test runner is installed. Run with:
//     pnpm exec tsx src/pages/TeamPage.test.tsx

import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import type {
  MemberExecutionConfig,
  ProjectMemberWithRuntime,
  UpdateProjectMemberRequest,
} from '../../../shared/types';

const dom = new JSDOM('<!doctype html><html><body></body></html>', {
  url: 'http://localhost',
});
Object.defineProperties(globalThis, {
  window: { value: dom.window, configurable: true },
  document: { value: dom.window.document, configurable: true },
  navigator: { value: dom.window.navigator, configurable: true },
  HTMLElement: { value: dom.window.HTMLElement, configurable: true },
  Event: { value: dom.window.Event, configurable: true },
});
(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

const React = await import('react');
const { act } = React;
const { createRoot } = await import('react-dom/client');
const { useMemberMcpEditor } = await import('./team/useMemberMcpEditor');

type EditorSnapshot = ReturnType<typeof useMemberMcpEditor>;
type Member = {
  id: string;
  execution_config?: MemberExecutionConfig | null;
};
type UpdateMemberCall = {
  projectId: string;
  memberId: string;
  data: UpdateProjectMemberRequest;
};

const AUTO_SAVE_DELAY_MS = 25;
const flush = async (ms = 60) => {
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, ms));
  });
};

const memberA: Member = {
  id: 'member-a',
  execution_config: {
    runner_type: 'CODEX' as never,
    model_name: 'gpt-5',
    mcp: {
      mcpServers: {
        github: {
          command: 'npx',
          args: ['github-mcp'],
          env: { GITHUB_TOKEN: 'placeholder-token-a' },
        },
      },
    },
  } as MemberExecutionConfig,
};
const memberB: Member = {
  id: 'member-b',
  execution_config: {
    runner_type: 'GEMINI' as never,
    mcp: {
      mcpServers: { linear: { url: 'https://mcp.linear.app/sse' } },
    },
  } as MemberExecutionConfig,
};

const canonicalJson = (servers: Record<string, unknown>) =>
  JSON.stringify({ mcpServers: servers }, null, 2);
const memberAJson = canonicalJson(memberA.execution_config!.mcp!.mcpServers);
const memberBJson = canonicalJson(memberB.execution_config!.mcp!.mcpServers);

type Harness = {
  render: (member: Member | null, runnerType?: string) => Promise<void>;
  unmount: () => Promise<void>;
  snapshot: () => EditorSnapshot;
  calls: UpdateMemberCall[];
  updated: ProjectMemberWithRuntime[];
  setUpdateMember: (
    fn: (call: UpdateMemberCall) => Promise<ProjectMemberWithRuntime>,
  ) => void;
};

const mountEditor = (): Harness => {
  const container = document.createElement('div');
  document.body.appendChild(container);
  const root = createRoot(container);
  let snapshot: EditorSnapshot | null = null;
  const calls: UpdateMemberCall[] = [];
  const updated: ProjectMemberWithRuntime[] = [];
  let updateMemberImpl: (
    call: UpdateMemberCall,
  ) => Promise<ProjectMemberWithRuntime> = (call) =>
    Promise.resolve({
      id: call.memberId,
      execution_config: call.data.execution_config!,
    } as ProjectMemberWithRuntime);

  const HarnessComponent = ({
    member,
    runnerType,
  }: {
    member: Member | null;
    runnerType: string;
  }) => {
    snapshot = useMemberMcpEditor({
      member,
      projectId: 'project-1',
      runnerType,
      updateMember: (projectId, memberId, data) => {
        const call = { projectId, memberId, data };
        calls.push(call);
        return updateMemberImpl(call);
      },
      onMemberUpdated: (member) => {
        updated.push(member);
      },
      autoSaveDelayMs: AUTO_SAVE_DELAY_MS,
      t: (key: string) => key,
    });
    return null;
  };

  return {
    render: async (member, runnerType = 'CODEX') => {
      await act(async () => {
        root.render(<HarnessComponent member={member} runnerType={runnerType} />);
      });
    },
    unmount: async () => {
      await act(async () => {
        root.unmount();
      });
    },
    snapshot: () => {
      assert.ok(snapshot, 'editor snapshot available');
      return snapshot;
    },
    calls,
    updated,
    setUpdateMember: (fn) => {
      updateMemberImpl = fn;
    },
  };
};

const changeJson = (harness: Harness, value: string) => {
  act(() => {
    harness.snapshot().handleMcpServersChange(value);
  });
};

// --- editor data comes from the selected member's execution_config.mcp ---
{
  const harness = mountEditor();
  await harness.render(memberA);
  assert.equal(harness.snapshot().mcpServersJson, memberAJson);
  assert.equal(harness.snapshot().mcpDirty, false);
  assert.equal(harness.snapshot().mcpError, null);
  await harness.unmount();
  console.log('  ok  loads canonical JSON from the selected member');
}

// --- switching A -> B before the debounce fires cancels the pending save ---
{
  const harness = mountEditor();
  await harness.render(memberA);
  changeJson(harness, canonicalJson({ a: { command: 'server-a' } }));
  await flush(10); // debounce (25ms) still pending
  await harness.render(memberB);
  await flush(80);
  assert.equal(harness.calls.length, 0, 'member A draft must never be saved');
  assert.equal(harness.snapshot().mcpServersJson, memberBJson);
  assert.equal(harness.snapshot().mcpDirty, false);
  await harness.unmount();
  console.log('  ok  fast A/B member switch cancels the pending debounce');
}

// --- a late save response must not overwrite the current member ---
{
  const harness = mountEditor();
  let resolveSave: ((member: ProjectMemberWithRuntime) => void) | null = null;
  harness.setUpdateMember(
    (call) =>
      new Promise<ProjectMemberWithRuntime>((resolve) => {
        resolveSave = resolve;
      }).then(() => ({
        id: call.memberId,
        execution_config: call.data.execution_config!,
      }) as ProjectMemberWithRuntime),
  );
  await harness.render(memberA);
  const draftA = canonicalJson({ a: { command: 'server-a' } });
  changeJson(harness, draftA);
  await flush(60); // save for A is now in flight
  assert.equal(harness.calls.length, 1);
  assert.equal(harness.calls[0].memberId, 'member-a');
  await harness.render(memberB); // user moves on before the response arrives
  assert.equal(harness.snapshot().mcpServersJson, memberBJson);
  await act(async () => {
    resolveSave!({} as ProjectMemberWithRuntime);
  });
  await flush(20);
  assert.equal(
    harness.snapshot().mcpServersJson,
    memberBJson,
    'late response must not touch member B editor JSON',
  );
  assert.equal(harness.snapshot().mcpSuccess, false);
  assert.equal(harness.snapshot().mcpError, null);
  assert.equal(harness.snapshot().mcpApplying, false);
  assert.equal(
    harness.updated.length,
    1,
    'accepted write still folds into the members list',
  );
  assert.equal(harness.updated[0].id, 'member-a');
  await harness.unmount();
  console.log('  ok  late save response cannot overwrite the current member');
}

// --- empty editor saves an explicit empty member MCP config ---
{
  const harness = mountEditor();
  const legacyMember: Member = {
    id: 'member-legacy',
    execution_config: {
      runner_type: 'CODEX' as never,
      model_name: 'gpt-5',
    } as MemberExecutionConfig,
  };
  await harness.render(legacyMember);
  changeJson(harness, '   ');
  await flush(80);
  assert.equal(harness.calls.length, 1);
  const call = harness.calls[0];
  assert.equal(call.projectId, 'project-1');
  assert.equal(call.memberId, 'member-legacy');
  assert.deepEqual(call.data.execution_config?.mcp, { mcpServers: {} });
  assert.equal(
    call.data.execution_config?.runner_type,
    'CODEX',
    'save must carry the full execution config, not a runner-only key',
  );
  assert.equal(call.data.execution_config?.model_name, 'gpt-5');
  assert.equal(harness.snapshot().mcpSuccess, true);
  await harness.unmount();
  console.log(
    '  ok  empty config saves { mcpServers: {} } through the member update API',
  );
}

// --- runner switch cancels the pending debounce but keeps the JSON draft ---
{
  const harness = mountEditor();
  await harness.render(memberA);
  const draft = canonicalJson({ kept: { command: 'keep-me' } });
  changeJson(harness, draft);
  await flush(10); // debounce pending; draft not yet saved
  const switchedRunner: Member = {
    id: memberA.id,
    execution_config: {
      ...memberA.execution_config,
      runner_type: 'GEMINI' as never,
    } as MemberExecutionConfig,
  };
  await harness.render(switchedRunner, 'GEMINI');
  assert.equal(
    harness.snapshot().mcpServersJson,
    draft,
    'runner switch must preserve the canonical JSON draft',
  );
  await flush(20);
  assert.equal(
    harness.calls.length,
    0,
    'runner switch cancels the previous debounce before it fires',
  );
  await flush(60);
  assert.equal(
    harness.calls.length,
    1,
    'draft is re-scheduled and still auto-saves afterwards',
  );
  assert.equal(harness.calls[0].memberId, 'member-a');
  assert.deepEqual(
    harness.calls[0].data.execution_config?.mcp?.mcpServers,
    { kept: { command: 'keep-me' } },
  );
  await harness.unmount();
  console.log(
    '  ok  runner switch cancels the old debounce and preserves the draft',
  );
}

// --- save errors name the server/field and never leak secret values ---
{
  const harness = mountEditor();
  harness.setUpdateMember(() =>
    Promise.reject(
      new Error(
        'invalid member MCP config: member `member-a`, server `github`, field `mcpServers.github.env.GITHUB_TOKEN`',
      ),
    ),
  );
  await harness.render(memberA);
  changeJson(
    harness,
    canonicalJson({
      github: { env: { GITHUB_TOKEN: 'placeholder-token-a' }, enabled: 'yes' },
    }),
  );
  await flush(80);
  const shown = harness.snapshot().mcpError ?? '';
  assert.ok(shown.includes('github'), 'error names the server');
  assert.ok(
    shown.includes('mcpServers.github.env.GITHUB_TOKEN'),
    'error names the field',
  );
  assert.ok(
    !shown.includes('placeholder-token-a'),
    'error must not contain the secret value',
  );
  await harness.unmount();

  const echoHarness = mountEditor();
  echoHarness.setUpdateMember(() =>
    Promise.reject(
      new Error(
        'backend failed: {"mcpServers":{"github":{"env":{"GITHUB_TOKEN":"placeholder-token-a"}}}}',
      ),
    ),
  );
  await echoHarness.render(memberA);
  changeJson(echoHarness, canonicalJson({ github: { command: 'x' } }));
  await flush(80);
  assert.equal(
    echoHarness.snapshot().mcpError,
    'teamPage.error.saveMcpConfig',
    'raw JSON error payloads fall back to the generic message',
  );
  await echoHarness.unmount();

  const syntaxHarness = mountEditor();
  await syntaxHarness.render(memberA);
  changeJson(syntaxHarness, '{ "mcpServers": ');
  await flush(80);
  assert.equal(
    syntaxHarness.snapshot().mcpError,
    'teamPage.error.invalidJson',
  );
  assert.equal(
    syntaxHarness.calls.length,
    0,
    'invalid JSON never triggers a save',
  );
  await syntaxHarness.unmount();
  console.log('  ok  MCP errors show server/field names without secrets');
}

// --- unmounting the page cancels the pending debounce ---
{
  const harness = mountEditor();
  await harness.render(memberA);
  changeJson(harness, canonicalJson({ a: { command: 'server-a' } }));
  await harness.unmount();
  await flush(80);
  assert.equal(harness.calls.length, 0, 'unmount cancels the pending save');
  console.log('  ok  unmount cancels the pending debounce');
}

// --- a slower earlier save cannot overwrite a fresher saved draft ---
{
  const harness = mountEditor();
  const deferred: Array<() => void> = [];
  harness.setUpdateMember(
    (call) =>
      new Promise<ProjectMemberWithRuntime>((resolve) => {
        deferred.push(() =>
          resolve({
            id: call.memberId,
            execution_config: call.data.execution_config!,
          } as ProjectMemberWithRuntime),
        );
      }),
  );
  await harness.render(memberA);
  const draft1 = canonicalJson({ one: { command: 'first' } });
  const draft2 = canonicalJson({ two: { command: 'second' } });
  changeJson(harness, draft1);
  await flush(60); // save 1 in flight
  assert.equal(harness.calls.length, 1);
  changeJson(harness, draft2); // newer draft while save 1 is in flight
  await act(async () => {
    deferred[0](); // save 1 resolves against a newer draft
  });
  await flush(10);
  assert.equal(
    harness.snapshot().mcpSuccess,
    false,
    'stale save must not mark the newer draft as saved',
  );
  assert.equal(harness.snapshot().mcpDirty, true);
  await flush(60); // save 2 fires
  assert.equal(harness.calls.length, 2);
  await act(async () => {
    deferred[1](); // save 2 resolves with the freshest draft
  });
  await flush(20);
  assert.equal(harness.snapshot().mcpSuccess, true);
  assert.equal(harness.snapshot().mcpDirty, false);
  await harness.unmount();
  console.log('  ok  interleaved saves keep the freshest draft authoritative');
}

console.log('\nAll TeamPage member MCP editor tests passed.');