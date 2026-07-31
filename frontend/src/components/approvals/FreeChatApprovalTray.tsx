import React, { useEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { AlertCircle, ChevronDown, Shield } from 'lucide-react';
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
  approvalCommand,
  approvalOptionLabel,
  groupApprovalRequests,
  partitionApprovalOptions,
  shouldShowApprovalSummary,
  type ApprovalTranslate,
} from './executorApprovalPresentation';

type FreeChatApprovalTrayProps = {
  sessionId: string;
  members: Member[];
  onError: (message: string) => void;
  workflowExecutionId?: string;
};

const ApprovalRequestRow: React.FC<{
  request: ChatExecutorApprovalRequest;
  disabled: boolean;
  error?: string;
  onResolve: (requestId: string, optionId: string) => void;
  locale: string;
  translate: ApprovalTranslate;
}> = React.memo(({ request, disabled, error, onResolve, locale, translate }) => {
  const [allowMenuOpen, setAllowMenuOpen] = useState(false);
  const [allowMenuPosition, setAllowMenuPosition] = useState<{
    left: number;
    top: number;
  } | null>(null);
  const allowMenuRef = useRef<HTMLDivElement>(null);
  const allowMenuButtonRef = useRef<HTMLButtonElement>(null);
  const allowMenuPopupRef = useRef<HTMLDivElement>(null);
  const { allowOnce, allowAlways, otherOptions } = partitionApprovalOptions(
    request.options,
  );
  const command = approvalCommand(request);

  useEffect(() => {
    if (!allowMenuOpen) return;
    const handlePointerDown = (event: PointerEvent) => {
      if (
        event.target instanceof Node &&
        !allowMenuRef.current?.contains(event.target) &&
        !allowMenuPopupRef.current?.contains(event.target)
      ) {
        setAllowMenuOpen(false);
      }
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setAllowMenuOpen(false);
    };
    document.addEventListener('pointerdown', handlePointerDown);
    document.addEventListener('keydown', handleKeyDown);
    return () => {
      document.removeEventListener('pointerdown', handlePointerDown);
      document.removeEventListener('keydown', handleKeyDown);
    };
  }, [allowMenuOpen]);

  useEffect(() => {
    if (!allowMenuOpen) {
      setAllowMenuPosition(null);
      return;
    }
    const updatePosition = () => {
      const trigger = allowMenuButtonRef.current;
      if (!trigger) return;
      const rect = trigger.getBoundingClientRect();
      const menuWidth = 144;
      const gap = 4;
      setAllowMenuPosition({
        left: Math.max(
          8,
          Math.min(
            rect.right - menuWidth,
            window.innerWidth - menuWidth - 8,
          ),
        ),
        top: rect.bottom + gap,
      });
    };
    updatePosition();
    window.addEventListener('resize', updatePosition);
    window.addEventListener('scroll', updatePosition, true);
    return () => {
      window.removeEventListener('resize', updatePosition);
      window.removeEventListener('scroll', updatePosition, true);
    };
  }, [allowMenuOpen]);

  useEffect(() => {
    if (disabled) setAllowMenuOpen(false);
  }, [disabled]);

  const resolveOption = (optionId: string) => {
    setAllowMenuOpen(false);
    onResolve(request.id, optionId);
  };

  const renderGhostOption = (option: ChatExecutorApprovalOption) => (
    <button
      key={option.option_id}
      type="button"
      disabled={disabled}
      onClick={() => resolveOption(option.option_id)}
      className="inline-flex h-7 items-center rounded-md bg-transparent px-2.5 text-xs font-medium text-[var(--ink-muted)] transition-colors hover:bg-[var(--surface-2)] hover:text-[var(--ink)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--primary-focus)] focus-visible:ring-offset-2 focus-visible:ring-offset-[var(--surface-1)] disabled:cursor-wait disabled:opacity-50"
    >
      {approvalOptionLabel(option, translate)}
    </button>
  );

  return (
    <article className="px-3.5 py-2.5">
      <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-x-3 gap-y-2">
        <div className="flex min-w-0 items-baseline gap-2 overflow-hidden">
          <span className="shrink-0 text-xs text-[var(--ink-subtle)]">
            {translate('approvals.requestAction', 'requests to run')}
          </span>
          <div className="flex min-w-0 flex-1 items-baseline gap-2">
            <code
              title={request.tool_name}
              className="shrink-0 font-mono text-[12px] font-medium text-[var(--ink)]"
            >
              {request.tool_name}
            </code>
            {command && (
              <code
                title={command}
                className="min-w-0 truncate font-mono text-[12px] text-[var(--ink-muted)]"
              >
                {command}
              </code>
            )}
          </div>
          <time
            dateTime={new Date(request.created_at).toISOString()}
            className="ml-auto shrink-0 font-mono text-[10px] text-[var(--ink-tertiary)]"
          >
            {new Intl.DateTimeFormat(locale, {
              hour: '2-digit',
              minute: '2-digit',
            }).format(new Date(request.created_at))}
          </time>
        </div>

        <div className="flex shrink-0 flex-wrap items-center justify-end gap-1">
          {otherOptions.map(renderGhostOption)}
          {allowOnce ? (
            <div ref={allowMenuRef} className="relative ml-1 inline-flex">
              <button
                type="button"
                disabled={disabled}
                onClick={() => resolveOption(allowOnce.option_id)}
                className={cn(
                  'inline-flex h-7 items-center bg-[var(--primary)] px-2.5 text-xs font-medium text-[var(--on-primary)] transition-colors hover:bg-[var(--primary-hover)] focus-visible:z-[1] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--primary-focus)] focus-visible:ring-offset-2 focus-visible:ring-offset-[var(--surface-1)] disabled:cursor-wait disabled:opacity-50',
                  allowAlways ? 'rounded-l-md' : 'rounded-md',
                )}
              >
                {approvalOptionLabel(allowOnce, translate)}
              </button>
              {allowAlways && (
                <>
                  <button
                    ref={allowMenuButtonRef}
                    type="button"
                    disabled={disabled}
                    aria-haspopup="menu"
                    aria-expanded={allowMenuOpen}
                    aria-label={translate(
                      'approvals.moreAllowOptions',
                      'More allow options',
                    )}
                    onClick={() => setAllowMenuOpen((open) => !open)}
                    className="inline-flex h-7 w-7 items-center justify-center rounded-r-md border-l border-[color-mix(in_srgb,var(--on-primary)_22%,transparent)] bg-[var(--primary)] text-[var(--on-primary)] transition-colors hover:bg-[var(--primary-hover)] focus-visible:z-[1] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--primary-focus)] focus-visible:ring-offset-2 focus-visible:ring-offset-[var(--surface-1)] disabled:cursor-wait disabled:opacity-50"
                  >
                    <ChevronDown
                      className="h-3.5 w-3.5"
                    />
                  </button>
                  {allowMenuOpen &&
                    allowMenuPosition &&
                    createPortal(
                      <div
                        ref={allowMenuPopupRef}
                        role="menu"
                        style={{
                          left: allowMenuPosition.left,
                          top: allowMenuPosition.top,
                        }}
                        className="fixed z-[100] min-w-44 rounded-md border border-[var(--hairline)] bg-[var(--surface-1)] p-1"
                      >
                        <button
                          type="button"
                          role="menuitem"
                          disabled={disabled}
                          onClick={() => resolveOption(allowAlways.option_id)}
                          className="flex h-7 w-full items-center rounded-md bg-transparent px-2.5 text-left text-xs font-medium text-[var(--ink-muted)] transition-colors hover:bg-[var(--surface-2)] hover:text-[var(--ink)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--primary-focus)] disabled:opacity-50"
                        >
                          {approvalOptionLabel(allowAlways, translate)}
                        </button>
                      </div>,
                      document.body,
                    )}
                </>
              )}
            </div>
          ) : (
            allowAlways && renderGhostOption(allowAlways)
          )}
        </div>
      </div>
      {error && (
        <p className="mt-2 text-xs text-red-400" role="alert">
          {error}
        </p>
      )}
    </article>
  );
});

export const FreeChatApprovalTray: React.FC<FreeChatApprovalTrayProps> = ({
  sessionId,
  members,
  onError,
  workflowExecutionId,
}) => {
  const { t } = useAppTranslation();
  const { locale } = useWorkspace();
  const {
    requests: allRequests,
    resolvingIds,
    requestErrors,
    error,
    resolve,
  } =
    useExecutorApprovals(sessionId);
  const [trayVisible, setTrayVisible] = useState(false);
  const emptyDismissTimerRef = useRef<number | null>(null);
  const requests = useMemo(
    () =>
      workflowExecutionId
        ? allRequests.filter(
            (request) =>
              request.workflow_execution_id === workflowExecutionId,
          )
        : allRequests.filter(
            (request) => request.workflow_execution_id === null,
          ),
    [allRequests, workflowExecutionId],
  );
  const membersBySessionAgentId = useMemo(
    () => new Map(members.map((member) => [member.id, member])),
    [members],
  );
  const requestGroups = useMemo(
    () => groupApprovalRequests(requests),
    [requests],
  );
  const translate = React.useCallback<ApprovalTranslate>(
    (key, fallback, replacements = {}) =>
      t(key, { defaultValue: fallback, ...replacements }),
    [t],
  );
  const handleResolve = React.useCallback(
    (requestId: string, optionId: string) => {
      void resolve(requestId, optionId).catch((cause) =>
        onError(cause instanceof Error ? cause.message : String(cause)),
      );
    },
    [onError, resolve],
  );

  useEffect(() => {
    setTrayVisible(false);
  }, [sessionId]);

  useEffect(() => {
    if (emptyDismissTimerRef.current !== null) {
      window.clearTimeout(emptyDismissTimerRef.current);
      emptyDismissTimerRef.current = null;
    }
    if (requests.length > 0 || error) {
      setTrayVisible(true);
      return;
    }
    if (!trayVisible) return;
    emptyDismissTimerRef.current = window.setTimeout(() => {
      setTrayVisible(false);
      emptyDismissTimerRef.current = null;
    }, 500);
    return () => {
      if (emptyDismissTimerRef.current !== null) {
        window.clearTimeout(emptyDismissTimerRef.current);
        emptyDismissTimerRef.current = null;
      }
    };
  }, [error, requests.length, trayVisible]);

  if (!trayVisible && requests.length === 0 && !error) return null;
  const memberCount = new Set(
    requests.map((request) => request.session_agent_id),
  ).size;

  return (
    <section className="mx-3 mb-2 overflow-hidden rounded-xl border border-[var(--hairline)] bg-[var(--surface-1)]">
      <div className="flex items-center gap-2.5 px-3.5 py-3">
        <Shield className="h-4 w-4 shrink-0 text-[var(--ink-tertiary)]" />
        <div className="min-w-0">
          <h3 className="text-sm font-medium text-[var(--ink)]">
            {translate('approvals.title', 'Permission requests')}
          </h3>
          {shouldShowApprovalSummary(requests.length, memberCount) && (
            <p className="mt-0.5 text-[11px] text-[var(--ink-tertiary)]">
              {translate(
                'approvals.summary',
                '{count} pending · {members} members',
                { count: requests.length, members: memberCount },
              )}
            </p>
          )}
        </div>
      </div>
      {error && (
        <div className="flex items-start gap-2 border-t border-[color-mix(in_srgb,var(--hairline)_62%,transparent)] px-3.5 py-2 text-xs text-[var(--ink-muted)]">
          <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0" />
          <p>{error}</p>
        </div>
      )}
      <div className="max-h-[28rem] divide-y divide-[var(--hairline)] overflow-y-auto border-t border-[color-mix(in_srgb,var(--hairline)_62%,transparent)]">
        {requests.length === 0 && !error ? (
          <div
            className="px-3.5 py-3 text-xs text-[var(--ink-tertiary)]"
            role="status"
          >
            {translate(
              'approvals.processingNext',
              'Approval processed. Waiting for the next request…',
            )}
          </div>
        ) : requestGroups.map((group) => {
          const firstRequest = group.requests[0];
          const groupMember = membersBySessionAgentId.get(group.sessionAgentId);
          const memberName = groupMember?.name ?? firstRequest.runner;
          const memberHandle = memberName.startsWith('@')
            ? memberName
            : `@${memberName}`;
          return (
            <section key={group.sessionAgentId}>
              <div className="sticky top-0 z-[1] flex items-center gap-2.5 bg-[color-mix(in_srgb,var(--surface-2)_42%,var(--surface-1))] px-3.5 py-2">
                <span className="min-w-0 truncate text-xs font-medium text-[var(--ink)]">
                  {memberHandle}
                </span>
                <span className="rounded-sm bg-[color-mix(in_srgb,var(--ink)_5%,transparent)] px-1.5 py-0.5 font-mono text-[9px] font-semibold tracking-[0.02em] text-[var(--ink-tertiary)]">
                  {firstRequest.runner}
                </span>
                <span className="ml-auto shrink-0 font-mono text-[10px] text-[var(--ink-tertiary)]">
                  {translate(
                    'approvals.memberPending',
                    '{count} pending',
                    { count: group.requests.length },
                  )}
                </span>
              </div>
              <div className="divide-y divide-[color-mix(in_srgb,var(--hairline)_62%,transparent)]">
                {group.requests.map((request) => (
                  <ApprovalRequestRow
                    key={request.id}
                    request={request}
                    disabled={resolvingIds.has(request.id)}
                    error={requestErrors[request.id]}
                    locale={locale}
                    translate={translate}
                    onResolve={handleResolve}
                  />
                ))}
              </div>
            </section>
          );
        })}
      </div>
    </section>
  );
};
