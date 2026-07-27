import { useCallback, useEffect, useRef, useState } from "react";
import type { ChatExecutorApprovalRequest } from "../../../shared/types";
import { executorApprovalsApi } from "@/lib/api";
import {
  EXECUTOR_APPROVAL_CHANGED_EVENT,
  type ExecutorApprovalChangedDetail,
} from "@/lib/executorApprovalEvents";

const approvalTime = (value: Date): number => new Date(value).getTime();

const sortApprovalRequests = (
  requests: ChatExecutorApprovalRequest[],
): ChatExecutorApprovalRequest[] =>
  [...requests].sort(
    (left, right) =>
      approvalTime(left.created_at) - approvalTime(right.created_at),
  );

const sameApprovalRequest = (
  left: ChatExecutorApprovalRequest,
  right: ChatExecutorApprovalRequest,
): boolean =>
  left.id === right.id &&
  String(left.updated_at) === String(right.updated_at) &&
  left.status === right.status &&
  left.selected_option_id === right.selected_option_id;

export const reconcileApprovalRequests = (
  current: ChatExecutorApprovalRequest[],
  incoming: ChatExecutorApprovalRequest[],
): ChatExecutorApprovalRequest[] => {
  const currentById = new Map(current.map((request) => [request.id, request]));
  const next = sortApprovalRequests(incoming).map((request) => {
    const existing = currentById.get(request.id);
    return existing && sameApprovalRequest(existing, request)
      ? existing
      : request;
  });
  const unchanged =
    current.length === next.length &&
    current.every((request, index) => request === next[index]);
  return unchanged ? current : next;
};

export const upsertApprovalRequest = (
  current: ChatExecutorApprovalRequest[],
  request: ChatExecutorApprovalRequest,
): ChatExecutorApprovalRequest[] => {
  const index = current.findIndex((candidate) => candidate.id === request.id);
  if (index < 0) return sortApprovalRequests([...current, request]);
  if (sameApprovalRequest(current[index], request)) return current;
  const next = [...current];
  next[index] = request;
  return sortApprovalRequests(next);
};

export const removeApprovalRequest = (
  current: ChatExecutorApprovalRequest[],
  requestId: string,
): ChatExecutorApprovalRequest[] => {
  const next = current.filter((request) => request.id !== requestId);
  return next.length === current.length ? current : next;
};

export const useExecutorApprovals = (sessionId: string | null) => {
  const [requests, setRequests] = useState<ChatExecutorApprovalRequest[]>([]);
  const [resolvingIds, setResolvingIds] = useState<ReadonlySet<string>>(
    () => new Set(),
  );
  const [requestErrors, setRequestErrors] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const refreshGenerationRef = useRef(0);
  const resolvedRequestIdsRef = useRef(new Set<string>());
  const resolvingIdsRef = useRef(new Set<string>());

  const removeRequest = useCallback((requestId: string) => {
    resolvedRequestIdsRef.current.add(requestId);
    refreshGenerationRef.current += 1;
    setRequests((current) => removeApprovalRequest(current, requestId));
  }, []);

  const upsertRequest = useCallback(
    (request: ChatExecutorApprovalRequest) => {
      resolvedRequestIdsRef.current.delete(request.id);
      refreshGenerationRef.current += 1;
      setRequests((current) => upsertApprovalRequest(current, request));
    },
    [],
  );

  const refresh = useCallback(async () => {
    const generation = ++refreshGenerationRef.current;
    if (!sessionId) {
      setRequests([]);
      return;
    }
    try {
      const pending = await executorApprovalsApi.listPending(sessionId);
      if (generation !== refreshGenerationRef.current) return;
      const visiblePending = pending.filter(
        (request) => !resolvedRequestIdsRef.current.has(request.id),
      );
      setRequests((current) =>
        reconcileApprovalRequests(current, visiblePending),
      );
      const pendingIds = new Set(pending.map((request) => request.id));
      for (const requestId of resolvedRequestIdsRef.current) {
        if (!pendingIds.has(requestId)) {
          resolvedRequestIdsRef.current.delete(requestId);
        }
      }
      setError(null);
    } catch (cause) {
      if (generation !== refreshGenerationRef.current) return;
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, [sessionId]);

  useEffect(() => {
    refreshGenerationRef.current += 1;
    resolvedRequestIdsRef.current.clear();
    resolvingIdsRef.current.clear();
    setRequests([]);
    setResolvingIds(new Set());
    setRequestErrors({});
    setError(null);
    void refresh();
    if (!sessionId) return;

    const interval = window.setInterval(() => void refresh(), 10_000);
    const onChanged = (event: Event) => {
      const detail = (event as CustomEvent<ExecutorApprovalChangedDetail>)
        .detail;
      if (detail?.sessionId !== sessionId) return;
      if (detail.type === "executor_approval_requested") {
        upsertRequest(detail.request);
      } else {
        removeRequest(detail.requestId);
      }
    };
    window.addEventListener(EXECUTOR_APPROVAL_CHANGED_EVENT, onChanged);
    return () => {
      window.clearInterval(interval);
      window.removeEventListener(EXECUTOR_APPROVAL_CHANGED_EVENT, onChanged);
    };
  }, [refresh, removeRequest, sessionId, upsertRequest]);

  const resolve = useCallback(
    async (requestId: string, optionId: string) => {
      if (!sessionId || resolvingIdsRef.current.has(requestId)) return;
      resolvingIdsRef.current.add(requestId);
      setResolvingIds(new Set(resolvingIdsRef.current));
      setRequestErrors((current) => {
        if (!(requestId in current)) return current;
        const next = { ...current };
        delete next[requestId];
        return next;
      });
      try {
        await executorApprovalsApi.resolve(sessionId, requestId, optionId);
        removeRequest(requestId);
        setError(null);
      } catch (cause) {
        const message = cause instanceof Error ? cause.message : String(cause);
        setRequestErrors((current) => ({
          ...current,
          [requestId]: message,
        }));
        throw cause;
      } finally {
        resolvingIdsRef.current.delete(requestId);
        setResolvingIds(new Set(resolvingIdsRef.current));
      }
    },
    [removeRequest, sessionId],
  );

  return {
    requests,
    resolvingIds,
    requestErrors,
    error,
    refresh,
    resolve,
  };
};
