import { useCallback, useEffect, useState } from "react";
import type { ChatExecutorApprovalRequest } from "../../../shared/types";
import { executorApprovalsApi } from "@/lib/api";
import {
  EXECUTOR_APPROVAL_CHANGED_EVENT,
  type ExecutorApprovalChangedDetail,
} from "@/lib/executorApprovalEvents";

export const useExecutorApprovals = (sessionId: string | null) => {
  const [requests, setRequests] = useState<ChatExecutorApprovalRequest[]>([]);
  const [resolvingId, setResolvingId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    if (!sessionId) {
      setRequests([]);
      return;
    }
    try {
      const pending = await executorApprovalsApi.listPending(sessionId);
      setRequests(
        pending.sort(
          (left, right) =>
            new Date(left.created_at).getTime() -
            new Date(right.created_at).getTime(),
        ),
      );
      setError(null);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, [sessionId]);

  useEffect(() => {
    void refresh();
    if (!sessionId) return;

    const interval = window.setInterval(() => void refresh(), 10_000);
    const onChanged = (event: Event) => {
      const detail = (event as CustomEvent<ExecutorApprovalChangedDetail>)
        .detail;
      if (detail?.sessionId === sessionId) void refresh();
    };
    window.addEventListener(EXECUTOR_APPROVAL_CHANGED_EVENT, onChanged);
    return () => {
      window.clearInterval(interval);
      window.removeEventListener(EXECUTOR_APPROVAL_CHANGED_EVENT, onChanged);
    };
  }, [refresh, sessionId]);

  const resolve = useCallback(
    async (requestId: string, optionId: string) => {
      if (!sessionId) return;
      setResolvingId(requestId);
      try {
        await executorApprovalsApi.resolve(sessionId, requestId, optionId);
        setRequests((current) =>
          current.filter((request) => request.id !== requestId),
        );
        setError(null);
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
        throw cause;
      } finally {
        setResolvingId(null);
      }
    },
    [sessionId],
  );

  return { requests, resolvingId, error, refresh, resolve };
};
