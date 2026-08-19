import assert from 'node:assert/strict';
import {
  buildTemplateMemberSpecs,
  resolveTemplateMemberRuntime,
} from './teamTemplateRuntime';
import type { AgentRuntimeStatus } from '@/types';
import {
  AcpAccessMode,
  AcpApprovalMode,
  BaseCodingAgent,
  type ChatMemberPreset,
  type ChatTeamPreset,
  type MemberExecutionConfig,
} from '../../../shared/types';

const runtime = (
  runnerType: string,
  models: string[],
  configuredModel?: string,
): AgentRuntimeStatus =>
  ({
    runner_type: runnerType,
    installed: true,
    executable: true,
    availability: { type: 'INSTALLATION_FOUND', path: null, source: null },
    discovered_models: models,
    model_source: 'discovered',
    version: null,
    last_checked_at: null,
    last_error: null,
    run_mode: 'cli',
    env_summary: [],
    executor_options: configuredModel ? { model: configuredModel } : {},
  }) as unknown as AgentRuntimeStatus;

const member = (patch: Partial<ChatMemberPreset>): ChatMemberPreset => ({
  id: patch.id ?? 'lead',
  name: patch.name ?? 'Lead Agent',
  description: patch.description ?? 'Coordinates delivery.',
  runner_type: patch.runner_type ?? 'codex',
  recommended_model: patch.recommended_model ?? 'gpt-5',
  system_prompt: patch.system_prompt ?? 'Lead the work.',
  default_workspace_path: patch.default_workspace_path ?? null,
  selected_skill_ids: patch.selected_skill_ids ?? [],
  tools_enabled: patch.tools_enabled ?? {},
  execution_config: patch.execution_config,
  is_builtin: patch.is_builtin ?? true,
  enabled: patch.enabled ?? true,
});

const team = (members: ChatMemberPreset[]): ChatTeamPreset => ({
  id: 'fullstack_delivery_team',
  name: 'Full-stack delivery team',
  description: 'Ship product work.',
  members,
  lead_member_id: members[0]?.id ?? null,
  workflow_steps: [],
  team_protocol: '',
  is_builtin: true,
  enabled: true,
  tier: 'standard',
});

const runtimes = [
  runtime('CLAUDE_CODE', ['claude-sonnet-4-20250514']),
  runtime('CODEX', ['gpt-4.1'], 'gpt-5'),
];

const availableSpec = resolveTemplateMemberRuntime(
  member({ runner_type: 'CODEX', recommended_model: 'gpt-5' }),
  runtimes,
);
assert.equal(availableSpec?.runnerType, 'CODEX');
assert.equal(availableSpec?.modelName, 'gpt-5');

const fallbackSpec = resolveTemplateMemberRuntime(
  member({ runner_type: 'GEMINI', recommended_model: 'gemini-2.5-pro' }),
  runtimes,
);
assert.equal(fallbackSpec?.runnerType, 'CLAUDE_CODE');
assert.equal(fallbackSpec?.modelName, 'claude-sonnet-4-20250514');

const specs = buildTemplateMemberSpecs(
  team([
    member({ id: 'lead', name: 'Lead Agent', runner_type: 'CODEX' }),
    member({ id: 'disabled', name: 'Disabled', enabled: false }),
  ]),
  'E:\\workspace',
  runtimes,
);
assert.equal(specs.length, 1);
assert.equal(specs[0]?.role, 'lead');
assert.equal(specs[0]?.workspacePath, 'E:\\workspace');

const completeExecutionConfig: MemberExecutionConfig = {
  runner_type: BaseCodingAgent.GEMINI,
  model_name: 'template-model',
  thinking_effort: 'high',
  model_variant: 'fake-variant',
  acp: {
    access_mode: AcpAccessMode.workspace_only,
    approval_mode: AcpApprovalMode.auto_reject,
    additional_directories: ['/fake/template/context'],
  },
  mcp: {
    mcpServers: {
      fake_local: {
        command: 'fake-mcp-server',
        env: { API_TOKEN: 'fake-template-token' },
        headers: { Authorization: 'Bearer fake-template-header' },
      },
    },
  },
};
const configuredMember = member({
  id: 'configured',
  runner_type: 'GEMINI',
  recommended_model: 'gemini-template-model',
  execution_config: completeExecutionConfig,
  tools_enabled: { web_search: true },
});
const configuredTeam = team([configuredMember]);
const configuredSpec = buildTemplateMemberSpecs(
  configuredTeam,
  '/workspace',
  runtimes,
)[0];
assert.ok(configuredSpec);
assert.deepEqual(configuredSpec.executionConfig, {
  ...completeExecutionConfig,
  runner_type: BaseCodingAgent.CLAUDE_CODE,
  model_name: 'claude-sonnet-4-20250514',
});
assert.deepEqual(configuredSpec.toolsEnabled, { web_search: true });

const independentlyBuiltSpec = buildTemplateMemberSpecs(
  configuredTeam,
  '/workspace',
  runtimes,
)[0];
assert.ok(independentlyBuiltSpec);
const configuredServer = configuredSpec.executionConfig.mcp?.mcpServers
  .fake_local as { env: { API_TOKEN: string } };
configuredServer.env.API_TOKEN = 'mutated-token';
configuredSpec.executionConfig.acp?.additional_directories?.push('/mutated');
assert.equal(
  (
    completeExecutionConfig.mcp?.mcpServers.fake_local as {
      env: { API_TOKEN: string };
    }
  ).env.API_TOKEN,
  'fake-template-token',
);
assert.deepEqual(completeExecutionConfig.acp?.additional_directories, [
  '/fake/template/context',
]);
assert.equal(
  (
    independentlyBuiltSpec.executionConfig.mcp?.mcpServers.fake_local as {
      env: { API_TOKEN: string };
    }
  ).env.API_TOKEN,
  'fake-template-token',
);
assert.deepEqual(
  independentlyBuiltSpec.executionConfig.acp?.additional_directories,
  ['/fake/template/context'],
);

const legacySpec = buildTemplateMemberSpecs(
  team([
    member({
      id: 'legacy',
      execution_config: undefined,
      tools_enabled: {
        mcpServers: { must_not_migrate: { command: 'legacy-tool-value' } },
      },
    }),
  ]),
  null,
  runtimes,
)[0];
assert.ok(legacySpec);
assert.deepEqual(legacySpec.executionConfig.mcp, { mcpServers: {} });
assert.deepEqual(legacySpec.toolsEnabled, {
  mcpServers: { must_not_migrate: { command: 'legacy-tool-value' } },
});
