// Behavioral tests for the capability-gated ACP runtime config field.
// The gates consume the backend ACP probe; a runner like Kiro CLI (no
// advertised auth methods, no additional directories) must hide those
// controls, and the session/load support line must follow the probe.
//
// Run with: pnpm exec tsx src/pages/AgentsPage.acpCapability.test.tsx

import assert from 'node:assert/strict';
import { JSDOM } from 'jsdom';
import type { AcpCapabilityProbe, JsonValue } from '@/types';

const dom = new JSDOM('<!doctype html><html><body></body></html>', {
  url: 'http://localhost',
});
Object.defineProperties(globalThis, {
  window: { value: dom.window, configurable: true },
  document: { value: dom.window.document, configurable: true },
  navigator: { value: dom.window.navigator, configurable: true },
  HTMLElement: { value: dom.window.HTMLElement, configurable: true },
  Event: { value: dom.window.Event, configurable: true },
  FocusEvent: { value: dom.window.FocusEvent, configurable: true },
});
(globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean })
  .IS_REACT_ACT_ENVIRONMENT = true;

const React = await import('react');
const { act } = React;
const { createRoot } = await import('react-dom/client');
const { AcpRuntimeConfigField } = await import('./AgentsPage');

let failures = 0;
const check = (label: string, condition: boolean, detail?: unknown) => {
  if (condition) {
    // eslint-disable-next-line no-console
    console.log(`  ok  ${label}`);
    return;
  }
  failures += 1;
  // eslint-disable-next-line no-console
  console.error(`  FAIL ${label}`, detail ?? '');
};

const t = (key: string) => key;

const probeWith = (
  overrides: Partial<AcpCapabilityProbe>,
): AcpCapabilityProbe => ({
  protocol_version: '1',
  agent_name: 'kiro-cli',
  agent_version: '2.20.1',
  auth_methods: [],
  supports_session_list: false,
  supports_session_resume: false,
  supports_session_load: false,
  supports_session_close: false,
  supports_session_delete: false,
  supports_additional_directories: false,
  agent_capabilities: null,
  config_source: 'none',
  config_options: [],
  ...overrides,
});

// Kiro 2.20.1 ACP v1: no advertised auth methods, no additional
// directories, session/load supported.
const kiroProbe = probeWith({ supports_session_load: true });
const permissiveProbe = probeWith({
  auth_methods: [{ id: 'oauth', name: 'OAuth', description: null }],
  supports_additional_directories: true,
});

type Rendered = {
  container: HTMLElement;
  changes: Array<[string, JsonValue | undefined]>;
  commits: () => number;
  unmount: () => Promise<void>;
};

const renderField = async (
  acpProbe: AcpCapabilityProbe | null,
  value: JsonValue | undefined,
): Promise<Rendered> => {
  const container = document.createElement('div');
  document.body.appendChild(container);
  const root = createRoot(container);
  const changes: Array<[string, JsonValue | undefined]> = [];
  let commitCount = 0;
  await act(async () => {
    root.render(
      React.createElement(AcpRuntimeConfigField, {
        runner: 'KIRO_CLI',
        acpProbe,
        value,
        onChange: (key: string, next: JsonValue | undefined) =>
          changes.push([key, next]),
        onCommit: () => {
          commitCount += 1;
        },
        t,
      }),
    );
  });
  return {
    container,
    changes,
    commits: () => commitCount,
    unmount: async () => {
      await act(async () => {
        root.unmount();
      });
      container.remove();
    },
  };
};

const methodIdInput = (container: HTMLElement) =>
  container.querySelector('input[placeholder="auth_method_id"]');

const openAuthDropdown = async (container: HTMLElement) => {
  const triggers = container.querySelectorAll('button[aria-expanded]');
  // accessMode, approval, auth — the auth dropdown is the third one.
  const trigger = triggers[2];
  assert.ok(trigger, 'auth dropdown trigger renders');
  await act(async () => {
    (trigger as HTMLButtonElement).click();
  });
};

const storedMethodIdConfig = {
  auth: { type: 'method_id', method_id: 'stored-method' },
  additional_directories: ['/extra/workspace'],
};

// --- Kiro probe: capability gates closed --------------------------------

const kiro = await renderField(kiroProbe, storedMethodIdConfig);
check(
  'kiro probe hides the stored auth method_id input',
  methodIdInput(kiro.container) === null,
);
check(
  'kiro probe hides the additional directories field',
  kiro.container.querySelector('textarea') === null,
);
check(
  'kiro probe reports session resume support from supports_session_load',
  kiro.container.textContent?.includes('agents.acp.sessionResume.supported') ===
    true,
);
await openAuthDropdown(kiro.container);
check(
  'kiro probe removes method_id from the auth dropdown options',
  !document.body.textContent?.includes('agents.acp.auth.methodId'),
);
await kiro.unmount();

// --- Permissive probe: capability gates open -----------------------------

const open = await renderField(permissiveProbe, storedMethodIdConfig);
check(
  'probe with advertised auth methods keeps the stored method_id input',
  (methodIdInput(open.container) as HTMLInputElement | null)?.value ===
    'stored-method',
);
check(
  'probe with additional directories shows the stored directories',
  (open.container.querySelector('textarea') as HTMLTextAreaElement | null)
    ?.value === '/extra/workspace',
);
check(
  'probe without session/load reports follow-ups start a new session',
  open.container.textContent?.includes(
    'agents.acp.sessionResume.unsupported',
  ) === true,
);
await openAuthDropdown(open.container);
check(
  'probe with advertised auth methods offers method_id in the dropdown',
  document.body.textContent?.includes('agents.acp.auth.methodId') === true,
);

const textarea = open.container.querySelector('textarea');
assert.ok(textarea, 'directories textarea renders');
const valueSetter = Object.getOwnPropertyDescriptor(
  dom.window.HTMLTextAreaElement.prototype,
  'value',
)?.set;
assert.ok(valueSetter, 'textarea value setter available');
await act(async () => {
  valueSetter.call(textarea, '/one\n/two');
  textarea.dispatchEvent(new dom.window.Event('input', { bubbles: true }));
});
const lastChange = open.changes.at(-1);
const directoriesChangeOk = (() => {
  if (!lastChange) return false;
  const [key, value] = lastChange;
  const record = value as Record<string, JsonValue> | undefined;
  return (
    key === 'acp' &&
    JSON.stringify(record?.additional_directories) ===
      JSON.stringify(['/one', '/two']) &&
    JSON.stringify(record?.auth) === JSON.stringify(storedMethodIdConfig.auth)
  );
})();
check('editing directories writes the parsed list to acp config', directoriesChangeOk);
await act(async () => {
  textarea.dispatchEvent(new dom.window.FocusEvent('focusout', { bubbles: true }));
});
check(
  'blurring the directories field commits the change',
  open.commits() > 0,
);
await open.unmount();

// --- Missing probe: nothing assumed --------------------------------------

const unknown = await renderField(null, storedMethodIdConfig);
check(
  'missing probe hides gated controls instead of assuming support',
  methodIdInput(unknown.container) === null &&
    unknown.container.querySelector('textarea') === null,
);
check(
  'missing probe reports session resume as not probed yet',
  unknown.container.textContent?.includes(
    'agents.acp.sessionResume.unknown',
  ) === true,
);
await unknown.unmount();

if (failures > 0) {
  // eslint-disable-next-line no-console
  console.error(`\n${failures} ACP capability assertion(s) failed.`);
  process.exit(1);
}
// eslint-disable-next-line no-console
console.log('\nAll ACP capability field assertions passed.');