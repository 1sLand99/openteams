import { readFileSync } from 'node:fs';
import {
  collectProjectIssueLabels,
  flattenIssueItems,
  githubOAuthPollDelay,
  issueDisplayIdFontSizePx,
  issueSourceProviderId,
  markIssueIntegrationsAuthorized,
  projectIssueIdPrefix,
  projectWorkItemDisplayId,
  projectWorkItemIssueStatus,
  projectWorkItemsToIssueGroups,
  shouldAutoFallbackToDevice,
  sortIssueItems,
} from './IssuePage';
import {
  composeIssueCommentBody,
  defaultIssueUserIdentity,
  findGitHubIssueLink,
  formatFileSize,
  labelDraftToList,
  labelDisplayName,
  projectWorkItemLabelList,
} from './IssueDetailPage';
import type { ProjectWorkItem, ProjectWorkItemDetailResponse } from '@/types';

let failures = 0;

const check = (label: string, condition: boolean, detail?: unknown) => {
  if (!condition) {
    failures += 1;
    console.error(`FAIL ${label}`, detail ?? '');
  } else {
    console.log(`ok ${label}`);
  }
};

check(
  'GitHub OAuth polling respects the adaptive 1s to 15s bounds',
  githubOAuthPollDelay(100) === 1_000 &&
    githubOAuthPollDelay(5_000) === 5_000 &&
    githubOAuthPollDelay(60_000) === 15_000,
);
check(
  'GitHub OAuth only auto-falls back when the backend marks the error recoverable',
  shouldAutoFallbackToDevice({
    status: 'error',
    account: null,
    error: 'broker_unavailable',
    retry_after_ms: null,
    fallback_to_device: true,
  }) &&
    !shouldAutoFallbackToDevice({
      status: 'denied',
      account: null,
      error: 'oauth_denied',
      retry_after_ms: null,
      fallback_to_device: false,
    }),
);

const authorizedAccount = {
  login: 'octocat',
  id: 1,
  avatar_url: null,
  html_url: null,
  scopes: ['repo'],
  connected_at: '2026-07-19T00:00:00Z',
};
const authorizedIntegrationState = markIssueIntegrationsAuthorized(
  {
    providers: [
      {
        id: 'github',
        name: 'GitHub',
        supported: true,
        status: 'auth_required',
      },
    ],
    github_account: null,
    github_repositories: [],
    linked_repositories: [],
    primary_repository: null,
  },
  authorizedAccount,
);
check(
  'successful GitHub authorization immediately switches integration UI state',
  authorizedIntegrationState?.github_account?.login === 'octocat' &&
    authorizedIntegrationState.providers[0]?.status === 'authorized',
  authorizedIntegrationState,
);

const item = (
  id: string,
  status: ProjectWorkItem['status'],
  source: ProjectWorkItem['source'] = 'manual',
): ProjectWorkItem => ({
  id,
  project_id: 'project-1',
  parent_id: null,
  type: status === 'blocked' ? 'bug' : 'task',
  status,
  title: `${status} item`,
  description: null,
  priority: status === 'blocked' ? 'urgent' : 'medium',
  source,
  created_by: null,
  created_at: '2026-06-07T00:00:00Z',
  updated_at: '2026-06-07T00:00:00Z',
});

const issueDetailSource = readFileSync(
  new URL('./IssueDetailPage.tsx', import.meta.url),
  'utf8',
);
const commandSelectMenuSource = readFileSync(
  new URL('../components/CommandSelectMenu.tsx', import.meta.url),
  'utf8',
);
const confirmationDialogSource = readFileSync(
  new URL('../components/ConfirmationDialog.tsx', import.meta.url),
  'utf8',
);
const apiSource = readFileSync(
  new URL('../lib/api.ts', import.meta.url),
  'utf8',
);
const projectGithubRouteSource = readFileSync(
  new URL(
    '../../../crates/server/src/routes/project_github.rs',
    import.meta.url,
  ),
  'utf8',
);
const projectWorkItemServiceSource = readFileSync(
  new URL(
    '../../../crates/services/src/services/project/work_item.rs',
    import.meta.url,
  ),
  'utf8',
);
const githubIssueServiceSource = readFileSync(
  new URL(
    '../../../crates/services/src/services/github/issue.rs',
    import.meta.url,
  ),
  'utf8',
);
const githubRestClientSource = readFileSync(
  new URL(
    '../../../crates/services/src/services/github/rest_client.rs',
    import.meta.url,
  ),
  'utf8',
);
const detailPanelUsages =
  issueDetailSource.match(/<DetailPanel/g)?.length ?? 0;
const sourceBetween = (start: string, end: string) =>
  issueDetailSource.slice(
    issueDetailSource.indexOf(start),
    issueDetailSource.indexOf(end),
  );
const sessionOptionTypeSource = sourceBetween(
  'type SessionMenuOption',
  'export type IssueCommentAttachment',
);
const sessionMenuOptionsSource = sourceBetween(
  'const sessionMenuOptions',
  'useEffect(() => {\n    setLabelDraft',
);
const sessionDropdownSource = sourceBetween(
  'function SessionDropdown',
  'function LabelSearchRow',
);

check(
  'issue detail right panels use collapsible detail panels',
  detailPanelUsages >= 4 &&
    issueDetailSource.includes('const [open, setOpen] = useState(true)') &&
    issueDetailSource.includes('aria-expanded={open}') &&
    issueDetailSource.includes(
      'onClick={() => setOpen((current) => !current)}',
    ),
  { detailPanelUsages },
);

check(
  'issue detail project panel links and creates sessions from issue context',
  issueDetailSource.includes('function SessionDropdown') &&
    issueDetailSource.includes("from '@/components/CommandSelectMenu'") &&
    issueDetailSource.includes('issue.detail.linkSessionPlaceholder') &&
    commandSelectMenuSource.includes('max-h-[220px]') &&
    issueDetailSource.includes('options={filteredSessionOptions}') &&
    issueDetailSource.includes('linkedSessionLinks.length === 0 &&') &&
    issueDetailSource.includes('projectApi.createSession(projectId') &&
    issueDetailSource.includes('linkSession(createdSession.id)') &&
    issueDetailSource.includes('buildIssueSessionPrompt') &&
    issueDetailSource.includes('const descriptionForPrompt = descriptionEditing') &&
    issueDetailSource.includes('description: descriptionForPrompt') &&
    issueDetailSource.includes('shouldUseWorkflowModeForIssue') &&
    issueDetailSource.includes('workspace_path: projectWorkspacePath') &&
    issueDetailSource.includes('notifyChatInputPrefill') &&
    issueDetailSource.includes('notifyLinkedWorkItemsChanged') &&
    issueDetailSource.includes("'openteams:navigate-session'") &&
    !issueDetailSource.includes(
      'Create session from issue detail is coming soon',
    ) &&
    !issueDetailSource.includes('<select'),
  issueDetailSource,
);
check(
  'issue detail link session menu omits unexplained numeric shortcuts',
  !sessionOptionTypeSource.includes('shortcut') &&
    !sessionMenuOptionsSource.includes('shortcut') &&
    !sessionDropdownSource.includes('option.shortcut') &&
    !issueDetailSource.includes(
      "if (openPropertyMenu === 'session') {\n        const option = sessionMenuOptions.find",
    ),
  {
    sessionOptionTypeSource,
    sessionMenuOptionsSource,
    sessionDropdownSource,
  },
);

const statusMutationSource = sourceBetween(
  'const handleStatusChange',
  'const handlePriorityChange',
);
const labelMutationSource = sourceBetween(
  'const handleSaveLabels',
  'const handleLabelMenuSelect',
);
const sessionLinkSource = sourceBetween(
  'const linkSession',
  'const handleAssignSession',
);
const sessionUnlinkSource = sourceBetween(
  'const handleUnlinkSession',
  'const commentBody',
);

check(
  'issue detail right-side mutations patch local detail without reloading',
  !statusMutationSource.includes('await loadDetail()') &&
    !labelMutationSource.includes('await loadDetail()') &&
    !sessionLinkSource.includes('await loadDetail()') &&
    !sessionUnlinkSource.includes('await loadDetail()') &&
    statusMutationSource.includes('summary: githubSummary') &&
    labelMutationSource.includes('labels: nextLabels') &&
    sessionLinkSource.includes(
      'execution_links: [...existing.execution_links, executionLink]',
    ) &&
    sessionUnlinkSource.includes(
      'execution_links: existing.execution_links.filter',
    ),
  {
    statusMutationSource,
    labelMutationSource,
    sessionLinkSource,
    sessionUnlinkSource,
  },
);
check(
  'issue detail syncs pending local status changes to github on entry',
  issueDetailSource.includes('pendingStatusSyncAttemptRef') &&
    issueDetailSource.includes(
      'getPendingIssueStatusSync(projectId, current.id)',
    ) &&
    issueDetailSource.includes("setAction('status-sync')") &&
    issueDetailSource.includes('projectGithubApi.updateIssueState(') &&
    issueDetailSource.includes(
      'clearPendingIssueStatusSync(projectId, current.id)',
    ) &&
    issueDetailSource.includes('GitHub issue status synced'),
  issueDetailSource,
);

check(
  'issue detail loads cached local work item detail without github request',
  sourceBetween('const loadDetail', 'useEffect(() => {').includes(
    'includeGithubDetail: false',
  ) &&
    !sourceBetween('const loadDetail', 'useEffect(() => {').includes(
      'projectGithubApi.getIssue(',
    ) &&
    issueDetailSource.includes('setDetailLoading(false);') &&
    apiSource.includes('options?: { includeGithubDetail?: boolean }') &&
    apiSource.includes('include_github_detail: options?.includeGithubDetail'),
  { issueDetailSource, apiSource },
);

check(
  'issue detail edits description and syncs description/comment explicitly',
  issueDetailSource.includes('const [descriptionDraft, setDescriptionDraft]') &&
    issueDetailSource.includes('handleSaveDescriptionDraft') &&
    issueDetailSource.includes('onBlur={() => {') &&
    issueDetailSource.includes('void handleSaveDescriptionDraft();') &&
    issueDetailSource.includes('projectGithubApi.updateIssueBody') &&
    issueDetailSource.includes('projectGithubApi.refreshIssue') &&
    issueDetailSource.includes('Sync description to GitHub') &&
    issueDetailSource.includes('Sync comments from GitHub') &&
    issueDetailSource.includes('handleSyncActivity') &&
    issueDetailSource.includes('value={descriptionDraft}') &&
    issueDetailSource.includes('<CloudUpload') &&
    issueDetailSource.includes('<RefreshCw'),
  issueDetailSource,
);

const activitySyncButtonSource = sourceBetween(
  "disabled={action === 'activity-sync' || !canSyncActivity}",
  'onClick={() => void handleSyncActivity()}',
);
const canSyncActivitySource = sourceBetween(
  'const canSyncActivity = Boolean(',
  'const issueTitle',
);
check(
  'issue detail activity sync does not require a comment draft',
  canSyncActivitySource.includes(
    'targetRepoIntegrationId && linkedGitHubIssueNumber',
  ) &&
    activitySyncButtonSource.includes("action === 'activity-sync'") &&
    !activitySyncButtonSource.includes('commentBody'),
  { canSyncActivitySource, activitySyncButtonSource },
);

const externalLinkPanelStart = issueDetailSource.indexOf(
  'panelId="external-link"',
);
const externalLinkPanelEnd = issueDetailSource.indexOf(
  '</DetailPanel>',
  externalLinkPanelStart,
);
const externalLinkPanelSource = issueDetailSource.slice(
  externalLinkPanelStart,
  externalLinkPanelEnd,
);
check(
  'issue detail external link shows github icon with issue number only',
  externalLinkPanelSource.includes('<DetailStaticRow icon={Github}>') &&
    externalLinkPanelSource.includes(
      'issue.detail.githubIssueNumber',
    ) &&
    !externalLinkPanelSource.includes('<DetailStaticRow icon={Box}>') &&
    !externalLinkPanelSource.includes('titleCaseToken(current.source)'),
  externalLinkPanelSource,
);

check(
  'project work item detail serves cached github issue content',
  projectWorkItemServiceSource.includes('github_issue_detail') &&
    projectWorkItemServiceSource.includes('cached_github_issue_detail') &&
    projectGithubRouteSource.includes('include_github_detail') &&
    projectGithubRouteSource.includes('update_github_issue_detail_cache') &&
    projectGithubRouteSource.includes(
      'update_linked_github_issue_title_cache',
    ) &&
    projectGithubRouteSource.includes('sync_linked_github_issue_title') &&
    projectGithubRouteSource.includes('.update_issue_title(') &&
    githubRestClientSource.includes('pub async fn update_issue_title(') &&
    githubRestClientSource.includes('serde_json::json!({ "title": title })') &&
    projectGithubRouteSource.includes(
      'update_linked_github_issue_body_cache',
    ) &&
    projectGithubRouteSource.includes('update_github_issue_summary_cache') &&
    projectGithubRouteSource.includes('update_github_issue_labels_cache') &&
    githubIssueServiceSource.includes('cache_issue_detail') &&
    githubIssueServiceSource.includes('Some(serde_json::to_string(detail)?)'),
  {
    projectWorkItemServiceSource,
    projectGithubRouteSource,
    githubIssueServiceSource,
    githubRestClientSource,
  },
);

check(
  'issue detail supports inline rename and deletes project work items',
  issueDetailSource.includes('handleRenameIssue') &&
    issueDetailSource.includes('handleDeleteIssue') &&
    issueDetailSource.includes('onIssueDeleted?.(current.id)') &&
    issueDetailSource.includes("onClick={() => setTitleEditing(true)}") &&
    issueDetailSource.includes('void handleSaveTitleDraft();') &&
    issueDetailSource.includes('onBlur={() => {') &&
    issueDetailSource.includes('className="w-full min-w-0"') &&
    issueDetailSource.includes('className="h-10 w-full min-w-0') &&
    !issueDetailSource.includes('<Save aria-hidden="true"') &&
    issueDetailSource.includes('role="menu"') &&
    issueDetailSource.includes('border-[var(--hairline-strong)]') &&
    issueDetailSource.includes('bg-[var(--surface-1)]') &&
    issueDetailSource.includes('hover:bg-[var(--surface-3)]') &&
    issueDetailSource.includes('<ConfirmationDialog') &&
    confirmationDialogSource.includes('role="alertdialog"') &&
    issueDetailSource.includes('<Trash2') &&
    apiSource.includes(
      'delete: async (projectId: string, workItemId: string)',
    ) &&
    (apiSource.includes('{ method: "DELETE" }') ||
      apiSource.includes("{ method: 'DELETE' }")) &&
    projectGithubRouteSource.includes('.delete(delete_work_item)') &&
    projectWorkItemServiceSource.includes('pub async fn delete('),
  {
    issueDetailSource,
    confirmationDialogSource,
    apiSource,
    projectGithubRouteSource,
    projectWorkItemServiceSource,
  },
);

check(
  'open work items map to todo',
  projectWorkItemIssueStatus('open') === 'todo',
);
check(
  'in progress work items map to in progress',
  projectWorkItemIssueStatus('in_progress') === 'in_progress',
);
check(
  'blocked work items map to backlog',
  projectWorkItemIssueStatus('blocked') === 'backlog',
);
check(
  'ready to merge work items keep their issue status',
  projectWorkItemIssueStatus('ready_to_merge') === 'ready_to_merge',
);
check(
  'merging work items keep their issue status',
  projectWorkItemIssueStatus('merging') === 'merging',
);
check(
  'done work items map to done',
  projectWorkItemIssueStatus('done') === 'done',
);
check(
  'cancelled work items map to canceled issue status',
  projectWorkItemIssueStatus('cancelled') === 'cancelled',
);
check(
  'duplicate work items map to duplicate issue status',
  projectWorkItemIssueStatus('duplicate') === 'duplicate',
);

const groups = projectWorkItemsToIssueGroups(
  [
    item('22222222-2222-4222-8222-222222222222', 'blocked'),
    item('33333333-3333-4333-8333-333333333333', 'done'),
    item('11111111-1111-4111-8111-111111111111', 'open', 'github_issue'),
  ],
  'all',
  'OpenTeams',
);

check('all filter returns three populated groups', groups.length === 3, groups);
check(
  'active filter excludes done items',
  projectWorkItemsToIssueGroups(
    [
      item('11111111-1111-4111-8111-111111111111', 'open'),
      item('33333333-3333-4333-8333-333333333333', 'done'),
    ],
    'active',
    'OpenTeams',
  ).every((group) => group.id !== 'done'),
);

const syncedGroups = projectWorkItemsToIssueGroups(
  [item('44444444-4444-4444-8444-444444444444', 'open')],
  'all',
  'OpenTeams',
  {
    '44444444-4444-4444-8444-444444444444': {
      status: 'done',
      priority: 'urgent',
      externalLabels: [{ name: 'bug', color: 'red' }],
    },
  },
);
check(
  'issue row overrides sync status into groups',
  syncedGroups[0]?.id === 'done',
  syncedGroups,
);
check(
  'issue row shows only synced labels in row chips',
  syncedGroups[0]?.items[0]?.labels?.map((label) => label.name).join(',') ===
    'bug',
  syncedGroups[0]?.items[0]?.labels,
);

const localLabelGroups = projectWorkItemsToIssueGroups(
  [
    {
      ...item('55555555-5555-4555-8555-555555555555', 'open'),
      labels_json: JSON.stringify(['bug', 'feature', 'enhancement']),
    },
  ],
  'all',
  'OpenTeams',
);
check(
  'local work item labels render as issue row chips',
  localLabelGroups[0]?.items[0]?.labels
    ?.map((label) => label.name)
    .join(',') === 'bug,feature,enhancement',
  localLabelGroups[0]?.items[0]?.labels,
);
check(
  'feature and improvement labels use different issue row colors',
  localLabelGroups[0]?.items[0]?.labels
    ?.filter((label) => label.name !== 'bug')
    .map((label) => label.color)
    .join(',') === 'purple,blue',
  localLabelGroups[0]?.items[0]?.labels,
);
check(
  'issue label catalog shares local and synced labels',
  collectProjectIssueLabels(
    [
      {
        ...item('66666666-6666-4666-8666-666666666666', 'open'),
        labels_json: JSON.stringify(['bug', 'custom']),
      },
      item('77777777-7777-4777-8777-777777777777', 'done'),
    ],
    {
      '77777777-7777-4777-8777-777777777777': {
        externalLabels: [{ name: 'help wanted', color: 'cyan' }],
      },
    },
  ).join(',') === 'bug,custom,help wanted',
);

const sortableWorkItem = (
  id: string,
  fields: Partial<ProjectWorkItem>,
): ProjectWorkItem => ({
  ...item(id, 'open'),
  ...fields,
});

const sortFixtures = [
  sortableWorkItem('aaaaaaaa-1111-4111-8111-111111111111', {
    title: 'banana',
    priority: 'low',
    created_at: '2026-06-01T00:00:00Z',
    updated_at: '2026-06-03T00:00:00Z',
  }),
  sortableWorkItem('aaaaaaaa-2222-4222-8222-222222222222', {
    title: 'Apple',
    priority: 'urgent',
    created_at: '2026-06-03T00:00:00Z',
    updated_at: '2026-06-01T00:00:00Z',
  }),
  sortableWorkItem('aaaaaaaa-3333-4333-8333-333333333333', {
    title: 'cherry',
    priority: 'medium',
    created_at: '2026-06-02T00:00:00Z',
    updated_at: '2026-06-02T00:00:00Z',
  }),
];

const flatIssueTitles = (sortId: 'updated' | 'created' | 'priority' | 'title') =>
  projectWorkItemsToIssueGroups(sortFixtures, 'all', 'OpenTeams', {}, {
    groupBy: 'none',
    sortId,
  })[0]
    ?.items.map((issue) => issue.title)
    .join(',');

check(
  'issues sort by most recently updated by default',
  flatIssueTitles('updated') === 'banana,cherry,Apple',
  flatIssueTitles('updated'),
);
check(
  'issues sort by creation time descending',
  flatIssueTitles('created') === 'Apple,cherry,banana',
  flatIssueTitles('created'),
);
check(
  'issues sort by priority urgent first',
  flatIssueTitles('priority') === 'Apple,cherry,banana',
  flatIssueTitles('priority'),
);
check(
  'issues sort by title with locale compare',
  flatIssueTitles('title') === 'Apple,banana,cherry',
  flatIssueTitles('title'),
);
check(
  'sortIssueItems orders an existing list by title',
  sortIssueItems(
    projectWorkItemsToIssueGroups(sortFixtures, 'all', 'OpenTeams')[0]?.items ??
      [],
    'title',
  )
    .map((issue) => issue.title)
    .join(',') === 'Apple,banana,cherry',
);

const priorityGroups = projectWorkItemsToIssueGroups(
  sortFixtures,
  'all',
  'OpenTeams',
  {},
  { groupBy: 'priority' },
);
check(
  'priority grouping orders groups from urgent to low',
  priorityGroups.map((group) => group.id).join(',') ===
    'priority:urgent,priority:medium,priority:low',
  priorityGroups,
);
check(
  'priority groups reuse priority i18n keys',
  priorityGroups[0]?.titleKey === 'issue.priority.urgent' &&
    priorityGroups[0]?.title === 'Urgent',
  priorityGroups[0],
);
check(
  'priority grouping keeps backlog filter semantics',
  projectWorkItemsToIssueGroups(
    [
      item('bbbbbbbb-1111-4111-8111-111111111111', 'blocked'),
      sortableWorkItem('bbbbbbbb-2222-4222-8222-222222222222', {
        priority: 'urgent',
      }),
    ],
    'backlog',
    'OpenTeams',
    {},
    { groupBy: 'priority' },
  ).flatMap((group) => group.items).length === 1,
);

const labelGroups = projectWorkItemsToIssueGroups(
  [
    sortableWorkItem('cccccccc-1111-4111-8111-111111111111', {
      labels_json: JSON.stringify(['bug', 'feature']),
    }),
    sortableWorkItem('cccccccc-2222-4222-8222-222222222222', {
      labels_json: JSON.stringify(['Bug']),
    }),
    sortableWorkItem('cccccccc-3333-4333-8333-333333333333', {}),
  ],
  'all',
  'OpenTeams',
  {},
  { groupBy: 'label' },
);
check(
  'label grouping buckets by first label with an unlabeled group',
  labelGroups.map((group) => group.id).join(',') === 'label:bug,label:unlabeled',
  labelGroups,
);
check(
  'label groups use the label name and mark the unlabeled group',
  labelGroups[0]?.title === 'bug' &&
    labelGroups[0]?.titleKey === undefined &&
    labelGroups[0]?.count === 2 &&
    labelGroups[1]?.titleKey === 'issue.group.noLabel' &&
    labelGroups[1]?.count === 1,
  labelGroups,
);

check(
  'no grouping returns a single flat section',
  projectWorkItemsToIssueGroups(sortFixtures, 'all', 'OpenTeams', {}, {
    groupBy: 'none',
  }).map((group) => `${group.id}:${group.count}`)
    .join(',') === 'all:3',
);
check(
  'no grouping returns no sections for an empty list',
  projectWorkItemsToIssueGroups([], 'all', 'OpenTeams', {}, { groupBy: 'none' })
    .length === 0,
);
check(
  'status grouping without options keeps the default behavior',
  projectWorkItemsToIssueGroups(sortFixtures, 'all', 'OpenTeams').map(
    (group) => group.id,
  ).join(',') === 'todo',
);

const parentWorkItemId = 'dddddddd-0000-4000-8000-000000000000';
const nestedGroups = projectWorkItemsToIssueGroups(
  [
    sortableWorkItem(parentWorkItemId, { title: 'parent', status: 'open' }),
    sortableWorkItem('dddddddd-0000-4000-8000-000000000001', {
      title: 'child beta',
      status: 'blocked',
      parent_id: parentWorkItemId,
      updated_at: '2026-06-02T00:00:00Z',
    }),
    sortableWorkItem('dddddddd-0000-4000-8000-000000000002', {
      title: 'child alpha',
      status: 'in_progress',
      parent_id: parentWorkItemId,
      updated_at: '2026-06-03T00:00:00Z',
    }),
  ],
  'all',
  'OpenTeams',
);
const nestedParent = nestedGroups[0]?.items[0];
check(
  'sub-issues nest under their parent instead of their own status group',
  nestedGroups.length === 1 &&
    nestedGroups[0]?.id === 'todo' &&
    nestedGroups[0]?.items.length === 1 &&
    nestedParent?.children.length === 2,
  nestedGroups,
);
check(
  'sub-issues follow the current sort order',
  nestedParent?.children.map((child) => child.title).join(',') ===
    'child alpha,child beta',
  nestedParent?.children,
);
check(
  'sub-issues sort by title when requested',
  projectWorkItemsToIssueGroups(
    [
      sortableWorkItem(parentWorkItemId, { title: 'parent', status: 'open' }),
      sortableWorkItem('dddddddd-0000-4000-8000-000000000001', {
        title: 'child beta',
        parent_id: parentWorkItemId,
      }),
      sortableWorkItem('dddddddd-0000-4000-8000-000000000002', {
        title: 'child alpha',
        parent_id: parentWorkItemId,
      }),
    ],
    'all',
    'OpenTeams',
    {},
    { sortId: 'title' },
  )[0]?.items[0]?.children
    .map((child) => child.title)
    .join(',') === 'child alpha,child beta',
);
check(
  'sub-issues record their parent work item id',
  nestedParent?.children.every(
    (child) => child.parentWorkItemId === parentWorkItemId,
  ) === true && nestedParent?.parentWorkItemId === null,
);
check(
  'orphaned sub-issues stay top-level when the parent is filtered out',
  (() => {
    const orphanGroups = projectWorkItemsToIssueGroups(
      [
        sortableWorkItem(parentWorkItemId, {
          title: 'done parent',
          status: 'done',
        }),
        sortableWorkItem('dddddddd-0000-4000-8000-000000000001', {
          title: 'orphan child',
          status: 'open',
          parent_id: parentWorkItemId,
        }),
      ],
      'active',
      'OpenTeams',
    );
    return (
      orphanGroups.length === 1 &&
      orphanGroups[0]?.id === 'todo' &&
      orphanGroups[0]?.items.length === 1 &&
      orphanGroups[0]?.items[0]?.title === 'orphan child' &&
      orphanGroups[0]?.items[0]?.children.length === 0
    );
  })(),
);
check(
  'sub-issues follow the parent group under priority grouping',
  (() => {
    const groups = projectWorkItemsToIssueGroups(
      [
        sortableWorkItem(parentWorkItemId, {
          title: 'parent',
          priority: 'low',
        }),
        sortableWorkItem('dddddddd-0000-4000-8000-000000000001', {
          title: 'child',
          priority: 'urgent',
          parent_id: parentWorkItemId,
        }),
      ],
      'all',
      'OpenTeams',
      {},
      { groupBy: 'priority' },
    );
    return (
      groups.length === 1 &&
      groups[0]?.id === 'priority:low' &&
      groups[0]?.items[0]?.children[0]?.title === 'child'
    );
  })(),
);
check(
  'flattenIssueItems omits children of collapsed parents',
  (() => {
    const items = nestedGroups[0]?.items ?? [];
    return (
      flattenIssueItems(items, new Set()).map((issue) => issue.title).join(',') ===
        'parent,child alpha,child beta' &&
      flattenIssueItems(items, new Set([parentWorkItemId]))
        .map((issue) => issue.title)
        .join(',') === 'parent'
    );
  })(),
);
check(
  'known issue labels translate through the shared label helper',
  labelDisplayName('bug', (key, fallback) =>
    key === 'issue.detail.labelBug' ? 'Translated Bug' : fallback,
  ) === 'Translated Bug',
);
check(
  'issue row labels do not include derived type or priority chips',
  groups[0]?.items[0]?.labels?.some((label) =>
    ['Task', 'Bug', 'Medium', 'Urgent'].includes(label.name),
  ) === false,
  groups[0]?.items[0]?.labels,
);
check(
  'github imported work items do not duplicate source as a label',
  groups[0]?.items[0]?.labels?.some((label) => label.name === 'GitHub') ===
    false,
  groups[0],
);
check(
  'github issue source maps to github icon provider',
  issueSourceProviderId('github_issue') === 'github',
);
check(
  'linear issue source maps to linear icon provider',
  issueSourceProviderId('linear_issue') === 'linear',
);
check(
  'jira issue source maps to jira icon provider',
  issueSourceProviderId('jira_issue') === 'jira',
);
check(
  'manual source maps to local icon provider',
  issueSourceProviderId('manual') === 'local',
);
check(
  'project issue prefix uses the first three project name characters',
  projectIssueIdPrefix('OpenTeams') === 'OPE',
);
check(
  'project issue prefix ignores whitespace before taking three characters',
  projectIssueIdPrefix('AI App') === 'AIA',
);
check(
  'project issue display ids use project prefix and sequence number',
  projectWorkItemDisplayId('OpenTeams', 3) === 'OPE-3',
);
check(
  'work item issue ids increment in page display order',
  groups
    .flatMap((group) => group.items)
    .map((issue) => issue.id)
    .join(',') === 'OPE-1,OPE-2,OPE-3',
  groups,
);
check(
  'short issue ids keep the default font size',
  issueDisplayIdFontSizePx('ISS-1') === 16,
);
check(
  'generated work item ids shrink to fit the issue id column',
  issueDisplayIdFontSizePx('PWI-111111') <= 12,
);
check(
  'long issue ids continue shrinking instead of wrapping',
  issueDisplayIdFontSizePx('PWI-11111122223333') <
    issueDisplayIdFontSizePx('PWI-111111'),
);
check('file sizes render bytes', formatFileSize(512) === '512 B');
check('file sizes render kibibytes', formatFileSize(1536) === '1.5 KB');
check(
  'comment body appends attachment names and sizes',
  composeIssueCommentBody('Please review', [
    { name: 'trace.log', size: 1536 },
  ]) === 'Please review\n\nAttachments:\n- trace.log (1.5 KB)',
);
check(
  'manual issue comments use local work item comment endpoint',
  apiSource.includes('projectWorkItemsApi = {') &&
    apiSource.includes('comment: async (') &&
    apiSource.includes('/comments') &&
    issueDetailSource.includes('const canComment = hasGitHubIssue') &&
    issueDetailSource.includes('projectWorkItemsApi.comment(') &&
    issueDetailSource.includes('comments: [...existing.comments, comment]') &&
    issueDetailSource.includes('!commentBody || !canComment'),
);
check(
  'label draft trims and dedupes case-insensitively',
  labelDraftToList('bug, feature, Bug, enhancement').join(',') ===
    'bug,feature,enhancement',
);
check(
  'project work item labels parse from json arrays',
  projectWorkItemLabelList('["bug"," feature "]').join(',') === 'bug,feature',
);

const detailWithGithubLink = {
  external_links: [
    {
      provider: 'github',
      external_type: 'github_issue',
      number: 42,
      url: 'https://github.com/openteams/app/issues/42',
    },
  ],
} as ProjectWorkItemDetailResponse;

check(
  'issue detail helper finds github issue external link',
  findGitHubIssueLink(detailWithGithubLink)?.number === 42,
);

check(
  'local issue default identity uses you without github auth',
  defaultIssueUserIdentity(null).name === 'you' &&
    defaultIssueUserIdentity(null).fallback === 'user',
);

const authorizedIdentity = defaultIssueUserIdentity({
  login: 'octocat',
  id: 1,
  avatar_url: 'https://avatars.githubusercontent.com/u/1?v=4',
  html_url: 'https://github.com/octocat',
  scopes: [],
  connected_at: '2026-06-07T00:00:00Z',
});
check(
  'local issue default identity switches to github account after auth',
  authorizedIdentity.name === 'octocat' &&
    authorizedIdentity.avatarUrl ===
      'https://avatars.githubusercontent.com/u/1?v=4',
);

if (failures > 0) process.exit(1);
