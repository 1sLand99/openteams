import React, { useMemo } from 'react';
import { AlertCircle, Shield } from 'lucide-react';
import type {
  ChatExecutorApprovalOption,
  ChatExecutorApprovalRequest,
} from '../../../../shared/types';
import type { Member } from '@/types';
import { useWorkspace } from '@/context/WorkspaceContext';
import { useExecutorApprovals } from '@/hooks/useExecutorApprovals';
import { useAppTranslation } from '@/hooks/useAppTranslation';
import { cn } from '@/lib/utils';
import {
  approvalOptionLabel,
  type ApprovalTranslate,
} from './executorApprovalPresentation';

type FreeChatApprovalTrayProps = {
  sessionId: string;
  members: Member[];
  onError: (message: string) => void;
  workflowExecutionId?: string;
};

const optionTone = (option: ChatExecutorApprovalOption) => {
  if (option.kind === 'allow_once') {
    return 'bg-[var(--primary)] text-[var(--on-primary)] hover:bg-[var(--primary-hover)]';
  }
  return 'bg-transparent text-[var(--ink-subtle)] hover:bg-[var(--surface-2)] hover:text-[var(--ink)]';
};

const RequestCard: React.FC<{
  request: ChatExecutorApprovalRequest;
  member?: Member;
  disabled: boolean;
  onResolve: (optionId: string) => void;
  locale: string;
  translate: ApprovalTranslate;
}> = ({ request, member, disabled, onResolve, locale, translate }) => {
  const memberName = member?.name ?? request.runner;
  const memberHandle = memberName.startsWith('@')
    ? memberName
    : `@${memberName}`;
  const optionPriority = (option: ChatExecutorApprovalOption) => {
    if (option.kind === 'allow_once') return 0;
    if (option.kind.startsWith('reject')) return 1;
    if (option.kind === 'allow_always') return 2;
    return 1;
  };
  const orderedOptions = [...request.options].sort(
    (left, right) => optionPriority(left) - optionPriority(right),
  );

  const renderOption = (option: ChatExecutorApprovalOption) => (
    <button
      key={option.option_id}
      type="button"
      disabled={disabled}
      onClick={() => onResolve(option.option_id)}
      className={cn(
        'inline-flex h-7 items-center rounded-md px-2.5 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--primary-focus)] focus-visible:ring-offset-2 focus-visible:ring-offset-[var(--surface-1)] disabled:cursor-wait disabled:opacity-50',
        optionTone(option),
      )}
    >
      {approvalOptionLabel(option, translate)}
    </button>
  );

  return (
    <article className="px-3.5 py-3">
      <div className="flex items-start gap-3">
        <div className="flex h-9 w-9 shrink-0 items-center justify-center overflow-hidden rounded-lg bg-[var(--surface-2)] text-xs font-semibold text-[var(--ink-muted)]">
          {member?.avatar &&
          (/^(?:https?:|data:|blob:|\/)/u.test(member.avatar)) ? (
            <img
              src={member.avatar}
              alt=""
              className="h-full w-full object-cover"
            />
          ) : (
            member?.avatar ||
            memberName.slice(0, 2).toUpperCase()
          )}
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex items-baseline justify-between gap-3">
            <p className="min-w-0 text-sm leading-5">
              <span className="font-medium text-[var(--ink)]">
                {memberHandle}
              </span>{' '}
              <span className="text-[var(--ink-subtle)]">
                {translate('approvals.requestAction', 'requests to run')}
              </span>{' '}
              <code className="break-all font-mono text-[12px] text-[var(--ink-muted)]">
                {request.tool_name}
              </code>{' '}
              <span className="inline-flex rounded-sm bg-[color-mix(in_srgb,var(--ink)_5%,transparent)] px-1.5 py-0.5 align-middle text-[9px] font-semibold tracking-[0.02em] text-[var(--ink-tertiary)]">
                {request.runner}
              </span>
            </p>
            <time
              dateTime={new Date(request.created_at).toISOString()}
              className="shrink-0 font-mono text-[10px] text-[color-mix(in_srgb,var(--ink-tertiary)_72%,transparent)]"
            >
              {new Intl.DateTimeFormat(locale, {
                hour: '2-digit',
                minute: '2-digit',
              }).format(new Date(request.created_at))}
            </time>
          </div>
        </div>
      </div>

      <div className="mt-2.5 flex flex-wrap items-center justify-end gap-1 pl-12">
        {orderedOptions.map(renderOption)}
      </div>
    </article>
  );
};

export const FreeChatApprovalTray: React.FC<FreeChatApprovalTrayProps> = ({
  sessionId,
  members,
  onError,
  workflowExecutionId,
}) => {
  const { t } = useAppTranslation();
  const { locale } = useWorkspace();
  const { requests: allRequests, resolvingId, error, resolve } =
    useExecutorApprovals(sessionId);
  const requests = workflowExecutionId
    ? allRequests.filter(
        (request) => request.workflow_execution_id === workflowExecutionId,
      )
    : allRequests.filter((request) => request.workflow_execution_id === null);
  const membersBySessionAgentId = useMemo(
    () => new Map(members.map((member) => [member.id, member])),
    [members],
  );

  if (requests.length === 0 && !error) return null;
  const memberCount = new Set(
    requests.map((request) => request.session_agent_id),
  ).size;
  const translate: ApprovalTranslate = (key, fallback, replacements = {}) =>
    t(key, { defaultValue: fallback, ...replacements });

  return (
    <section className="mx-3 mb-2 overflow-hidden rounded-xl border border-[var(--hairline)] bg-[var(--surface-1)]">
      <div className="flex items-center gap-2.5 px-3.5 py-3">
        <Shield className="h-4 w-4 shrink-0 text-[var(--ink-tertiary)]" />
        <div className="min-w-0">
          <h3 className="text-sm font-medium text-[var(--ink)]">
            {translate('approvals.title', 'Permission requests')}
          </h3>
          <p className="mt-0.5 text-[11px] text-[var(--ink-tertiary)]">
            {translate(
              'approvals.summary',
              '{count} pending · {members} members',
              { count: requests.length, members: memberCount },
            )}
          </p>
        </div>
      </div>
      {error && (
        <div className="flex items-start gap-2 border-t border-[color-mix(in_srgb,var(--hairline)_62%,transparent)] px-3.5 py-2 text-xs text-[var(--ink-muted)]">
          <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <p>{error}</p>
        </div>
      )}
      <div className="max-h-[28rem] divide-y divide-[color-mix(in_srgb,var(--hairline)_62%,transparent)] overflow-y-auto border-t border-[color-mix(in_srgb,var(--hairline)_62%,transparent)]">
        {requests.map((request) => (
          <RequestCard
            key={request.id}
            request={request}
            member={membersBySessionAgentId.get(request.session_agent_id)}
            disabled={resolvingId === request.id}
            locale={locale}
            translate={translate}
            onResolve={(optionId) => {
              void resolve(request.id, optionId).catch((cause) =>
                onError(cause instanceof Error ? cause.message : String(cause)),
              );
            }}
          />
        ))}
      </div>
    </section>
  );
};
