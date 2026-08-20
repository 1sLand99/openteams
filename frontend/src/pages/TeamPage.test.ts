// Smoke tests for TeamPage member/session-agent synchronization.
//
// No test runner is installed. Run with:
//     pnpm exec tsx src/pages/TeamPage.test.ts

import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

let failures = 0;
const check = (label: string, cond: boolean, detail?: unknown) => {
  if (cond) {
    // eslint-disable-next-line no-console
    console.log(`  ok  ${label}`);
  } else {
    failures += 1;
    // eslint-disable-next-line no-console
    console.error(`  FAIL ${label}`, detail ?? "");
  }
};

console.log("TeamPage member removal");

const source = readFileSync(new URL("./TeamPage.tsx", import.meta.url), "utf8");
const sidebarSource = readFileSync(
  new URL("./team/TeamMemberSidebar.tsx", import.meta.url),
  "utf8",
);
const configTabsSource = readFileSync(
  new URL("./team/TeamConfigTabs.tsx", import.meta.url),
  "utf8",
);
const memberMcpEditorSource = readFileSync(
  new URL("./team/useMemberMcpEditor.ts", import.meta.url),
  "utf8",
);
const memberMcpConfigSource = readFileSync(
  new URL("./team/memberMcpConfig.ts", import.meta.url),
  "utf8",
);
const apiSource = readFileSync(new URL("../lib/api.ts", import.meta.url), "utf8");
const enTeamLocale = readFileSync(
  new URL("../locales/en/team.json", import.meta.url),
  "utf8",
);
const configTabStart = configTabsSource.indexOf("function ConfigTab(");
const permissionsTabStart = configTabsSource.indexOf(
  "function PermissionsTab(",
);
const permissionsTabEnd = configTabsSource.indexOf("function SkillsTab(");
const configTabSource = configTabsSource.slice(
  configTabStart,
  permissionsTabStart,
);
const permissionsTabSource = configTabsSource.slice(
  permissionsTabStart,
  permissionsTabEnd,
);
const appSource = readFileSync(new URL("../App.tsx", import.meta.url), "utf8");
const teamNavigationSource = readFileSync(
  new URL("../lib/teamNavigation.ts", import.meta.url),
  "utf8",
);
const removeProjectMemberIndex = source.indexOf(
  "await projectApi.removeMember(selectedProjectId, member.id)",
);
const removeSessionAgentIndex = source.indexOf(
  "await removeAgentFromProjectSessions(selectedProjectId, member.agent_id)",
);

check(
  "removes matching agent from every project session after project member deletion",
  source.includes("const removeAgentFromProjectSessions = async") &&
    source.includes("projectApi.listSessions(projectId)") &&
    source.includes("sessionAgentsApi.list(session.id)") &&
    source.includes("sessionMember.agent_id === agentId") &&
    source.includes("sessionAgentsApi.remove(session.id, sessionMember.id)") &&
    removeProjectMemberIndex >= 0 &&
    removeSessionAgentIndex > removeProjectMemberIndex,
  { removeProjectMemberIndex, removeSessionAgentIndex },
);

check(
  "loads and creates agents within the selected project scope",
  source.includes("chatAgentsApi.list({ projectId })") &&
    source.includes("owner_project_id: selectedProjectId"),
  { source },
);

check(
  "add member menu includes available runtimes and unassigned legacy agents",
  source.includes("const addableRuntimeOptions = useMemo(") &&
    source.includes(
      ".filter((runner) => getRuntimeDisplayState(runner) === \"available\")",
    ) &&
    source.includes("const addableLegacyAgents = useMemo(") &&
    source.includes("agent.owner_project_id === null") &&
    source.includes("!memberAgentIds.has(agent.id)") &&
    source.includes("const addLegacyMember = async (agentId: string)") &&
    source.includes("await addProjectMemberForAgent(agent);") &&
    sidebarSource.includes("filteredLegacyAgents.map((agent) => (") &&
    sidebarSource.includes("filteredRuntimeOptions.map((option) => (") &&
    sidebarSource.includes("filteredLegacyAgents.length > 0 || filteredRuntimeOptions.length > 0") &&
    sidebarSource.includes("runtimeOptions = []") &&
    sidebarSource.includes("legacyAgents = []") &&
    sidebarSource.includes("onAddLegacyMember(agent.id)"),
  { source, sidebarSource },
);

check(
  "failed member runs use warning presentation instead of error presentation",
  sidebarSource.includes(
    'state === "dead" &&\n          "border-[var(--notification-warning-border)] bg-[var(--notification-warning-bg)] text-[var(--notification-warning)]"',
  ) &&
    sidebarSource.includes(
      'state === "dead" && "bg-[var(--notification-warning)]"',
    ) &&
    !sidebarSource.includes('state === "dead" && "border-red-500') &&
    !sidebarSource.includes('state === "dead" && "bg-red-500"'),
  sidebarSource,
);

check(
  "member invite navigation opens the team page add-member menu",
  appSource.includes("TEAM_MEMBER_INVITE_NAVIGATION_EVENT") &&
    appSource.includes('openPageTab("team", getPageTabLabel("team"))') &&
    source.includes("readTeamMemberInviteTarget()") &&
    source.includes("clearTeamMemberInviteTarget()") &&
    source.includes("setAddMemberMenuRequestId((value) => value + 1)") &&
    source.includes("openRequestKey={addMemberMenuRequestId}") &&
    sidebarSource.includes("openRequestKey?: number") &&
    sidebarSource.includes("setShowAddMenu(true)") &&
    teamNavigationSource.includes("window.sessionStorage.setItem") &&
    teamNavigationSource.includes("openteams:navigate-team-member-invite"),
  { appSource, source, sidebarSource, teamNavigationSource },
);

check(
  "team member configuration changes are auto-saved without a manual action footer",
  source.includes("const autoSaveDelayMs = 700") &&
    source.includes("memberAutoSaveTimerRef.current = window.setTimeout") &&
    source.includes("void saveMember()") &&
    memberMcpEditorSource.includes(
      "autoSaveTimerRef.current = window.setTimeout",
    ) &&
    memberMcpEditorSource.includes("void save()") &&
    source.includes(
      "teamProtocolAutoSaveTimerRef.current = window.setTimeout",
    ) &&
    source.includes("void saveTeamProtocol()") &&
    !configTabsSource.includes("MemberSaveActions") &&
    !configTabsSource.includes("McpSaveActions") &&
    !configTabsSource.includes("TeamProtocolSaveActions") &&
    !configTabsSource.includes("shouldShowActionFooter") &&
    !configTabsSource.includes(
      'border-t border-[var(--hairline)] bg-[var(--surface-1)]',
    ),
  { source, configTabsSource },
);

check(
  "team protocol loads and saves against the selected project",
  source.includes("projectApi\n      .getTeamProtocol(selectedProjectId)") &&
    source.includes("projectApi.updateTeamProtocol(selectedProjectId") &&
    !source.includes("chatSessionsApi.getTeamProtocol(") &&
    !source.includes("teamProtocolSessionId") &&
    !configTabsSource.includes("onTeamProtocolSave") &&
    !configTabsSource.includes('t("teamPage.action.saveTeamProtocol")'),
  { source, configTabsSource },
);

check(
  "team protocol and role definition render markdown and switch to editing on interaction",
  configTabsSource.includes("function MarkdownEditableField") &&
    configTabsSource.includes("value.trim() ?") &&
    configTabsSource.includes("<AgentMarkdown content={value} fontSize={14} />") &&
    configTabsSource.includes('value={roleDefinition}') &&
    configTabsSource.includes('value={\n            teamProtocolLoading') &&
    configTabsSource.includes("if (!disabled) setEditing(true)") &&
    configTabsSource.includes("onBlur={() => setEditing(false)}"),
  configTabsSource,
);

check(
  "empty role prompt preserves placeholder line breaks and fills its panel",
  configTabsSource.includes(
    '<span className="whitespace-pre-wrap text-[var(--ink-muted)]">',
  ) &&
    configTabsSource.includes('bodyClassName="!p-0 flex flex-col"') &&
    configTabsSource.includes(
      'minHeightClassName="min-h-[360px] flex-1"',
    ),
  configTabsSource,
);

check(
  "team protocol remains available when the project has no selected member",
  configTabsSource.includes(
    "const effectiveActiveTab = selectedMember",
  ) &&
    configTabsSource.includes(': "teamProtocol";') &&
    configTabsSource.includes(
      "return selectedMember ? [...memberTabs, protocolTab] : [protocolTab]",
    ) &&
    !configTabsSource.includes(
      "if (!selectedMember) return <EmptyMemberState t={t} />",
    ),
  { configTabsSource },
);

check(
  "ACP member permissions render in a dedicated permissions tab",
  configTabsSource.includes('| "permissions"') &&
    configTabsSource.includes('id: "permissions" as const') &&
    configTabsSource.includes('t("teamPage.tabs.permissions")') &&
    configTabsSource.includes("<PermissionsTab {...props} />") &&
    permissionsTabStart > configTabStart &&
    permissionsTabEnd > permissionsTabStart &&
    !configTabSource.includes("权限与审批") &&
    permissionsTabSource.includes('title="权限与审批"') &&
    (permissionsTabSource.match(/<DropdownSelect/g) ?? []).length === 3 &&
    !permissionsTabSource.includes("<select"),
  { configTabSource, configTabsSource, permissionsTabSource },
);

check(
  "ACP member permissions appear immediately without waiting for diagnostics",
  configTabsSource.includes(
    "const supportsAcpPermissions = runnerSupportsAcp(props.runnerType);",
  ) &&
    !configTabsSource.includes(
      "const supportsAcpPermissions = props.acpProbeAvailable",
    ),
  configTabsSource,
);

check(
  "risky ACP member permissions use the in-app confirmation dialog",
  permissionsTabSource.includes("<ConfirmationDialog") &&
    permissionsTabSource.includes(
      'idPrefix="member-acp-permission-confirmation"',
    ) &&
    permissionsTabSource.includes(
      't("permissions.fullAccessHighRisk")',
    ) &&
    permissionsTabSource.includes(
      't("permissions.fullAccessMemberConfirmTitle")',
    ) &&
    permissionsTabSource.includes(
      't("permissions.fullAccessMemberConfirmDescription")',
    ) &&
    !permissionsTabSource.includes("Full Access") &&
    !permissionsTabSource.includes("window.confirm"),
  permissionsTabSource,
);

check(
  "member name field keeps the inherited agent name as placeholder only",
  source.includes('memberName: member.member_name?.trim() ?? ""') &&
    source.includes(
      'selectedAgent?.name ?? t("teamPage.form.memberName")',
    ) &&
    configTabsSource.includes('autoComplete="off"') &&
    !source.includes(
      'memberName: member.member_name?.trim() || agent?.name?.trim() || ""',
    ),
  { source, configTabsSource },
);

check(
  "skill selection does not reload the installed skills list after save",
  source.includes(".listNative(runnerType)") &&
    source.includes("}, [runnerType, selectedMember?.id]);") &&
    !source.includes("}, [runnerType, selectedMember]);"),
  { source },
);

const teamUtilsSource = readFileSync(
  new URL("./team/teamUtils.ts", import.meta.url),
  "utf8",
);

check(
  "member runtime normalization recognizes DeepSeek Harness and PI",
  /"DEEPSEEK_HARNESS",[\s\S]*?"QODER_CLI",[\s\S]*?"PI",[\s\S]*?\];/u.test(
    teamUtilsSource,
  ),
  teamUtilsSource,
);

check(
  "DeepSeek Harness member permissions hide unsupported auth methods and extra directories",
  configTabsSource.includes('runnerType === "DEEPSEEK_HARNESS"') &&
    configTabsSource.includes(
      'runnerType !== "HERMES" && runnerType !== "DEEPSEEK_HARNESS"',
    ),
  configTabsSource,
);

check(
  "member runtime options keep coming from the generic available-runtime filter",
  source.includes(
    ".filter((runner) => getRuntimeDisplayState(runner) === \"available\")",
  ) &&
    source.includes("id: runner.runner_type") &&
    configTabsSource.includes('value as BaseCodingAgent'),
  { source, configTabsSource },
);

check(
  "member-level skill, MCP and ACP approval entries stay generic for PI",
  source.includes(".listNative(runnerType)") &&
    source.includes("useMemberMcpEditor({") &&
    !/pi[-_ ]?(skill|mcp)/iu.test(source) &&
    !/pi[-_ ]?(skill|mcp)/iu.test(configTabsSource) &&
    !/pi[-_ ]?(skill|mcp)/iu.test(memberMcpEditorSource),
  { source, configTabsSource, memberMcpEditorSource },
);

check(
  "MCP editor data flows from the selected member to the member update API",
  source.includes("useMemberMcpEditor({") &&
    source.includes("member: selectedMember") &&
    source.includes("updateMember: projectApi.updateMember") &&
    memberMcpEditorSource.includes("buildMemberMcpUpdate(snapshot, servers)") &&
    memberMcpConfigSource.includes(
      "...member.execution_config,\n    mcp: { mcpServers: servers },",
    ) &&
    !source.includes("mcpServersApi") &&
    !source.includes("/api/mcp-config") &&
    !apiSource.includes("mcpServersApi") &&
    !apiSource.includes("/api/mcp-config"),
  { source, apiSource },
);

check(
  "member profile saves preserve the persisted member MCP config",
  source.includes("mcp: selectedMember.execution_config?.mcp ?? null"),
  { source },
);

check(
  "member MCP saves are bound to member id and sequence with debounce cancellation",
  memberMcpEditorSource.includes("const seq = ++saveSeqRef.current") &&
    memberMcpEditorSource.includes("memberIdRef.current === memberId") &&
    memberMcpEditorSource.includes("saveSeqRef.current === seq") &&
    memberMcpEditorSource.includes("window.clearTimeout(autoSaveTimerRef.current)") &&
    memberMcpEditorSource.includes("}, [memberId, clearAutoSaveTimer]);") &&
    memberMcpEditorSource.includes("return clearAutoSaveTimer;") &&
    memberMcpEditorSource.includes("runnerType,\n    save,") &&
    source.includes("runnerType,\n    updateMember: projectApi.updateMember,"),
  { memberMcpEditorSource },
);

check(
  "MCP editor errors never interpolate raw config JSON",
  memberMcpConfigSource.includes(
    "if (/[{}]/u.test(err.message)) return fallbackMessage;",
  ) &&
    !memberMcpEditorSource.includes("console.log") &&
    !memberMcpConfigSource.includes("console.log"),
  { memberMcpConfigSource, memberMcpEditorSource },
);

check(
  "built-in MCP choices write only to the selected member's canonical JSON",
  !configTabsSource.includes("mcpConfigPath") &&
    !configTabsSource.includes("savedToFile") &&
    configTabsSource.includes("builtinTitle") &&
    configTabsSource.includes("onToggleMcpServer") &&
    configTabsSource.includes("mcpCatalog?.preconfigured") &&
    source.includes("memberMcpCatalogApi") &&
    source.includes("toggleMemberMcpCatalogServer") &&
    apiSource.includes("/api/member-mcp-catalog") &&
    configTabsSource.includes('t("teamPage.mcp.savedToMember")') &&
    !enTeamLocale.includes("savedToFile") &&
    enTeamLocale.includes("teamPage.mcp.savedToMember") &&
    enTeamLocale.includes("teamPage.mcp.builtinTitle"),
  { source, apiSource, configTabsSource, enTeamLocale },
);

// Workflow nodes reference team members (whose runtime may be PI), so no
// workflow component may hardcode a runner allowlist that would exclude Pi.
const workflowRoot = fileURLToPath(
  new URL("../components/workflow/", import.meta.url),
);
const collectFiles = (dir: string): string[] =>
  readdirSync(dir).flatMap((entry): string[] => {
    const full = join(dir, entry);
    return statSync(full).isDirectory() ? collectFiles(full) : [full];
  });
const workflowRunnerAllowlists = collectFiles(workflowRoot)
  .filter((file) => /\.(ts|tsx)$/u.test(file) && !/\.test\./u.test(file))
  .filter((file) =>
    /runner_type\s*===\s*"(GEMINI|QWEN_CODE|KIMI_CODE|QODER_CLI)"/u.test(
      readFileSync(file, "utf8"),
    ),
  );
check(
  "workflow components hold no hardcoded runner allowlist excluding Pi",
  workflowRunnerAllowlists.length === 0,
  workflowRunnerAllowlists,
);

if (failures > 0) {
  // eslint-disable-next-line no-console
  console.error(`\n${failures} TeamPage assertion(s) failed.`);
  process.exit(1);
} else {
  // eslint-disable-next-line no-console
  console.log("\nAll TeamPage assertions passed.");
}
