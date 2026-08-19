import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type {
  ProjectMemberWithRuntime,
  UpdateProjectMemberRequest,
} from "../../../../shared/types";
import {
  buildMemberMcpUpdate,
  memberMcpServersJson,
  parseMemberMcpServers,
  presentMemberMcpError,
  type MemberMcpSource,
} from "./memberMcpConfig";

type TranslateFn = (
  key: string,
  replacements?: Record<string, string | number>,
) => string;

type UpdateMemberFn = (
  projectId: string,
  memberId: string,
  data: UpdateProjectMemberRequest,
) => Promise<ProjectMemberWithRuntime>;

type UseMemberMcpEditorOptions = {
  member: MemberMcpSource | null;
  projectId: string | null;
  /** Runner switches cancel the pending debounce (the draft itself is
   * preserved and re-scheduled) so a save never fires against a stale
   * editing context. */
  runnerType: string;
  updateMember: UpdateMemberFn;
  onMemberUpdated: (updated: ProjectMemberWithRuntime) => void;
  autoSaveDelayMs: number;
  /** Pause auto-save while the member form save is in flight so the two
   * writers of `execution_config` stay sequential. */
  pauseAutoSave?: boolean;
  t: TranslateFn;
};

/**
 * Member-scoped MCP editor state machine.
 *
 * - Editor data comes only from the selected member's `execution_config.mcp`.
 * - Saves always go through the project member update API carrying the
 *   current member id; each save is bound to that id plus a sequence number,
 *   so a late response can never overwrite another member's editor state.
 * - The debounce timer is cancelled whenever the member, project or page
 *   changes and on unmount; runner switches cancel the pending timer too,
 *   but the canonical JSON draft itself is preserved and re-scheduled.
 */
export const useMemberMcpEditor = ({
  member,
  projectId,
  runnerType,
  updateMember,
  onMemberUpdated,
  autoSaveDelayMs,
  pauseAutoSave = false,
  t,
}: UseMemberMcpEditorOptions) => {
  const [mcpServersJson, setMcpServersJson] = useState("{}");
  const [originalMcpServersJson, setOriginalMcpServersJson] = useState("{}");
  const [mcpApplying, setMcpApplying] = useState(false);
  const [mcpError, setMcpError] = useState<string | null>(null);
  const [mcpSuccess, setMcpSuccess] = useState(false);

  const autoSaveTimerRef = useRef<number | null>(null);
  const saveSeqRef = useRef(0);
  const memberIdRef = useRef<string | null>(member?.id ?? null);
  const memberSnapshotRef = useRef<MemberMcpSource | null>(member);
  const projectIdRef = useRef<string | null>(projectId);
  const latestJsonRef = useRef(mcpServersJson);
  const updateMemberRef = useRef(updateMember);
  const onMemberUpdatedRef = useRef(onMemberUpdated);
  const tRef = useRef(t);

  memberIdRef.current = member?.id ?? null;
  memberSnapshotRef.current = member;
  projectIdRef.current = projectId;
  latestJsonRef.current = mcpServersJson;
  updateMemberRef.current = updateMember;
  onMemberUpdatedRef.current = onMemberUpdated;
  tRef.current = t;

  const clearAutoSaveTimer = useCallback(() => {
    if (autoSaveTimerRef.current !== null) {
      window.clearTimeout(autoSaveTimerRef.current);
      autoSaveTimerRef.current = null;
    }
  }, []);

  const save = useCallback(async () => {
    const memberId = memberIdRef.current;
    const project = projectIdRef.current;
    const snapshot = memberSnapshotRef.current;
    if (!memberId || !project || !snapshot) return;
    const seq = ++saveSeqRef.current;
    const isCurrent = () =>
      memberIdRef.current === memberId && saveSeqRef.current === seq;
    setMcpApplying(true);
    setMcpError(null);
    setMcpSuccess(false);
    const draftJson = latestJsonRef.current;
    try {
      const servers = parseMemberMcpServers(draftJson);
      const updated = await updateMemberRef.current(
        project,
        memberId,
        buildMemberMcpUpdate(snapshot, servers),
      );
      // The server accepted the write: fold it into the members list (scoped
      // by member id) even when the editor has moved on, but never let a late
      // response touch the current member's editor state.
      onMemberUpdatedRef.current(updated);
      if (!isCurrent()) return;
      setOriginalMcpServersJson(draftJson);
      setMcpSuccess(latestJsonRef.current === draftJson);
    } catch (err) {
      if (!isCurrent()) return;
      setMcpError(
        presentMemberMcpError(
          err,
          tRef.current("teamPage.error.invalidJson"),
          tRef.current("teamPage.error.saveMcpConfig"),
        ),
      );
    } finally {
      if (isCurrent()) setMcpApplying(false);
    }
  }, []);

  const memberId = member?.id ?? null;

  // Sync the editor from the selected member. Keyed on the member id only:
  // runner switches and member data refreshes keep the draft, while a member
  // switch cancels the pending debounce and invalidates in-flight saves.
  useEffect(() => {
    clearAutoSaveTimer();
    saveSeqRef.current += 1;
    setMcpApplying(false);
    setMcpError(null);
    setMcpSuccess(false);
    if (!memberId) {
      setMcpServersJson("{}");
      setOriginalMcpServersJson("{}");
      return;
    }
    const snapshot = memberSnapshotRef.current;
    const json = snapshot ? memberMcpServersJson(snapshot) : "{}";
    setMcpServersJson(json);
    setOriginalMcpServersJson(json);
  }, [memberId, clearAutoSaveTimer]);

  const mcpDirty = mcpServersJson !== originalMcpServersJson;

  useEffect(() => {
    clearAutoSaveTimer();
    if (
      !mcpDirty ||
      mcpApplying ||
      !!mcpError ||
      !memberId ||
      !projectId ||
      pauseAutoSave
    ) {
      return clearAutoSaveTimer;
    }
    autoSaveTimerRef.current = window.setTimeout(() => {
      autoSaveTimerRef.current = null;
      void save();
    }, autoSaveDelayMs);
    return clearAutoSaveTimer;
  }, [
    autoSaveDelayMs,
    clearAutoSaveTimer,
    memberId,
    mcpApplying,
    mcpDirty,
    mcpError,
    mcpServersJson,
    pauseAutoSave,
    projectId,
    runnerType,
    save,
  ]);

  useEffect(() => {
    if (!mcpSuccess) return;
    const timeoutId = window.setTimeout(() => setMcpSuccess(false), 2000);
    return () => window.clearTimeout(timeoutId);
  }, [mcpSuccess]);

  const handleMcpServersChange = useCallback((value: string) => {
    setMcpServersJson(value);
    setMcpSuccess(false);
    setMcpError(null);
    if (!value.trim()) return;
    try {
      parseMemberMcpServers(value);
    } catch (err) {
      setMcpError(
        presentMemberMcpError(
          err,
          tRef.current("teamPage.error.invalidJson"),
          tRef.current("teamPage.error.invalidMcpConfig"),
        ),
      );
    }
  }, []);

  return useMemo(
    () => ({
      mcpServersJson,
      mcpDirty,
      mcpApplying,
      mcpError,
      mcpSuccess,
      handleMcpServersChange,
    }),
    [
      mcpServersJson,
      mcpDirty,
      mcpApplying,
      mcpError,
      mcpSuccess,
      handleMcpServersChange,
    ],
  );
};