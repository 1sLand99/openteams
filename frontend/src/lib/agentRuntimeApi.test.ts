// Agent runtime diagnostics request coordination tests.
//
// Run with:
//     pnpm exec tsx src/lib/agentRuntimeApi.test.ts

import { agentRuntimeApi } from './api';

let failures = 0;
const check = (label: string, condition: boolean, detail?: unknown) => {
  if (condition) {
    // eslint-disable-next-line no-console
    console.log(`  ok  ${label}`);
  } else {
    failures += 1;
    // eslint-disable-next-line no-console
    console.error(`  FAIL ${label}`, detail ?? '');
  }
};

const successResponse = (data: unknown) =>
  new Response(
    JSON.stringify({
      success: true,
      data,
      error_data: null,
      message: null,
    }),
    {
      status: 200,
      headers: { 'Content-Type': 'application/json' },
    },
  );

const originalFetch = globalThis.fetch;
let diagnosticsFetches = 0;
const refreshUrls: string[] = [];
let releaseFirstDiagnostics: () => void = () => {
  throw new Error('First diagnostics request was not started');
};

globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = String(input);
  if (url.includes('/diagnostics')) {
    diagnosticsFetches += 1;
    if (diagnosticsFetches === 1) {
      await new Promise<void>((resolve) => {
        releaseFirstDiagnostics = resolve;
      });
    }
    return successResponse({
      runner_type: 'GEMINI',
      acp_probe: { agent_version: '0.53.0' },
    });
  }
  if (init?.method === 'PATCH') {
    return successResponse({ runner_type: 'GEMINI' });
  }
  if (init?.method === 'POST' && url.includes('/agents/runtime/refresh/light')) {
    return successResponse({ runners: [], errors: [] });
  }
  if (init?.method === 'POST' && url.includes('/agents/runtime/refresh')) {
    refreshUrls.push(url);
    return successResponse({ runners: [], errors: [] });
  }
  throw new Error(`Unexpected request: ${url}`);
}) as typeof fetch;

// eslint-disable-next-line no-console
console.log('agentRuntimeApi diagnostics coordination');

const first = agentRuntimeApi.getDiagnostics('GEMINI', {
  workspacePath: '/workspace/a',
});
const duplicate = agentRuntimeApi.getDiagnostics('GEMINI', {
  workspacePath: '/workspace/a',
});
check(
  'concurrent requests with the same key share one fetch',
  diagnosticsFetches === 1,
  diagnosticsFetches,
);
releaseFirstDiagnostics();
const [firstResult, duplicateResult] = await Promise.all([first, duplicate]);
check(
  'shared callers receive the same diagnostics result',
  firstResult.acp_probe?.agent_version === '0.53.0' &&
    duplicateResult.acp_probe?.agent_version === '0.53.0',
);

await agentRuntimeApi.getDiagnostics('GEMINI', {
  workspacePath: '/workspace/a',
});
check(
  'a successful result is reused during the short cache window',
  diagnosticsFetches === 1,
  diagnosticsFetches,
);

await agentRuntimeApi.getDiagnostics('GEMINI', {
  workspacePath: '/workspace/b',
});
check(
  'a different workspace uses a distinct diagnostics key',
  diagnosticsFetches === 2,
  diagnosticsFetches,
);

await agentRuntimeApi.updateConfig('GEMINI', {
  run_mode: null,
  env_json: null,
  executor_options: null,
});
await agentRuntimeApi.getDiagnostics('GEMINI', {
  workspacePath: '/workspace/a',
});
check(
  'saving runtime config invalidates cached diagnostics',
  diagnosticsFetches === 3,
  diagnosticsFetches,
);

await agentRuntimeApi.getDiagnostics('GEMINI', {
  workspacePath: '/workspace/b',
});
check(
  'diagnostics are cached again before a light refresh',
  diagnosticsFetches === 4,
  diagnosticsFetches,
);

await agentRuntimeApi.refreshLight();
await agentRuntimeApi.getDiagnostics('GEMINI', {
  workspacePath: '/workspace/b',
});
check(
  'light refresh reuses cached diagnostics without a new probe',
  diagnosticsFetches === 4,
  diagnosticsFetches,
);

await agentRuntimeApi.refresh('/workspace/current');
check(
  'heavy refresh passes the active workspace path',
  refreshUrls[0]?.includes('workspace_path=%2Fworkspace%2Fcurrent') === true,
  refreshUrls,
);
await agentRuntimeApi.getDiagnostics('GEMINI', {
  workspacePath: '/workspace/b',
});
check(
  'explicit heavy refresh invalidates cached diagnostics',
  diagnosticsFetches === 5,
  diagnosticsFetches,
);

globalThis.fetch = originalFetch;

if (failures > 0) {
  process.exitCode = 1;
}
