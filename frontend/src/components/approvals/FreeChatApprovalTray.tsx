import React, { useMemo } from "react";
import { AlertTriangle, ShieldCheck, ShieldX, Wrench } from "lucide-react";
import type {
  ChatExecutorApprovalOption,
  ChatExecutorApprovalRequest,
} from "../../../../shared/types";
import type { Member } from "@/types";
import { useExecutorApprovals } from "@/hooks/useExecutorApprovals";

type FreeChatApprovalTrayProps = {
  sessionId: string;
  members: Member[];
  onError: (message: string) => void;
  workflowExecutionId?: string;
};

const optionTone = (option: ChatExecutorApprovalOption) => {
  if (option.kind.startsWith("reject")) {
    return "border-[var(--danger)] text-[var(--danger)] hover:bg-[color-mix(in_srgb,var(--danger)_10%,transparent)]";
  }
  if (option.kind === "allow_always") {
    return "border-[var(--warning)] text-[var(--warning)] hover:bg-[color-mix(in_srgb,var(--warning)_10%,transparent)]";
  }
  return "border-[var(--accent)] text-[var(--accent)] hover:bg-[color-mix(in_srgb,var(--accent)_10%,transparent)]";
};

const RequestCard: React.FC<{
  request: ChatExecutorApprovalRequest;
  member?: Member;
  disabled: boolean;
  onResolve: (optionId: string) => void;
}> = ({ request, member, disabled, onResolve }) => {
  const summary =
    typeof request.display_input === "string"
      ? request.display_input
      : JSON.stringify(request.display_input, null, 2);

  return (
    <article className="rounded-lg border border-[var(--hairline)] bg-[var(--surface-1)] p-3">
      <div className="flex items-start gap-2">
        <div className="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-full bg-[var(--surface-2)] text-xs font-semibold">
          {member?.avatar &&
          (/^(?:https?:|data:|blob:|\/)/u.test(member.avatar)) ? (
            <img src={member.avatar} alt="" className="h-full w-full object-cover" />
          ) : (
            member?.avatar ||
            (member?.name ?? request.runner).slice(0, 2).toUpperCase()
          )}
        </div>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <span className="font-medium text-[var(--ink)]">
              {member?.name ?? request.runner}
            </span>
            <span className="text-xs text-[var(--ink-tertiary)]">
              {request.runner}
            </span>
          </div>
          <div className="mt-1 flex items-center gap-1.5 text-sm text-[var(--ink-subtle)]">
            <Wrench className="h-3.5 w-3.5" />
            <span>{request.tool_name}</span>
          </div>
        </div>
        <time className="text-[11px] text-[var(--ink-tertiary)]">
          {new Date(request.created_at).toLocaleTimeString([], {
            hour: "2-digit",
            minute: "2-digit",
          })}
        </time>
      </div>
      {summary && (
        <pre className="mt-2 max-h-28 overflow-auto whitespace-pre-wrap break-all rounded bg-[var(--surface-2)] p-2 text-xs text-[var(--ink-subtle)]">
          {summary}
        </pre>
      )}
      <div className="mt-3 flex flex-wrap justify-end gap-2">
        {request.options.map((option) => (
          <button
            key={option.option_id}
            type="button"
            disabled={disabled}
            onClick={() => onResolve(option.option_id)}
            className={`inline-flex h-8 items-center gap-1 rounded-md border px-3 text-xs font-medium transition disabled:cursor-wait disabled:opacity-50 ${optionTone(option)}`}
          >
            {option.kind.startsWith("reject") ? (
              <ShieldX className="h-3.5 w-3.5" />
            ) : (
              <ShieldCheck className="h-3.5 w-3.5" />
            )}
            {option.label}
          </button>
        ))}
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

  return (
    <section className="mx-3 mb-2 rounded-xl border border-[color-mix(in_srgb,var(--warning)_45%,var(--hairline))] bg-[color-mix(in_srgb,var(--warning)_5%,var(--canvas))] p-2">
      <div className="mb-2 flex items-center gap-2 px-1 text-sm font-medium text-[var(--ink)]">
        <AlertTriangle className="h-4 w-4 text-[var(--warning)]" />
        <span>
          待审批 {requests.length} 项 · {memberCount} 位成员
        </span>
      </div>
      {error && (
        <p className="mb-2 px-1 text-xs text-[var(--danger)]">{error}</p>
      )}
      <div className="max-h-72 space-y-2 overflow-y-auto">
        {requests.map((request) => (
          <RequestCard
            key={request.id}
            request={request}
            member={membersBySessionAgentId.get(request.session_agent_id)}
            disabled={resolvingId === request.id}
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
