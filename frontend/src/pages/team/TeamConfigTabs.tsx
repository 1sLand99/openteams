import {
  useMemo,
  useState,
  type ReactNode,
} from "react";
import {
  AlertCircle,
  Check,
  CheckCircle2,
  CircleAlert,
  FileText,
  FolderGit2,
  Loader2,
  PackagePlus,
  RefreshCw,
  Server,
  Settings,
  ShieldCheck,
  X,
} from "lucide-react";
import { AgentMarkdown } from "@/components/AgentMarkdown";
import {
  DropdownSelect,
  type DropdownSelectOption,
} from "@/components/DropdownSelect";
import { ConfirmationDialog } from "@/components/ConfirmationDialog";
import { AgentBrandAvatar } from "../agent-runtime/agentRuntimeBrand";
import type {
  AgentRuntimeReasoningCapability,
  BackendChatSkill,
  BaseCodingAgent,
  JsonValue,
  McpConfig,
} from "@/types";
import type {
  AcpConfigOptionSnapshot,
  AcpConfigOverride,
  AcpConfigValue,
} from "../../../../shared/types";
import {
  defaultOptionId,
  cx,
  effectiveAcpConfigValue,
  findAcpSelectConfigOption,
  type ProjectMemberWithExecution,
} from "./teamUtils";

type MemberConfigTab =
  | "config"
  | "permissions"
  | "skills"
  | "mcp"
  | "teamProtocol";

type TranslateFn = (
  key: string,
  replacements?: Record<string, string | number>,
) => string;

type TeamConfigTabsProps = {
  acpAccessMode: string;
  acpAdditionalDirectories: string;
  acpAdditionalDirectoriesOverride: boolean;
  acpApprovalMode: string;
  acpAuthMode: string;
  acpAuthMethodId: string;
  acpConfigOptions: AcpConfigOptionSnapshot[];
  acpConfigOverrides: AcpConfigOverride[];
  acpProbeLoading: boolean;
  reasoningUnsupported: boolean;
  allowedSkillIds: string[];
  capability: AgentRuntimeReasoningCapability | null;
  configuredMcpServerKeys: string[];
  isLeader: boolean;
  legacyModelName: string;
  legacyThinkingEffort: string;
  memberName: string;
  memberNamePlaceholder: string;
  memberDirty: boolean;
  memberSuccess: boolean;
  mcpApplying: boolean;
  mcpConfig: McpConfig | null;
  mcpConfigPath: string;
  mcpDirty: boolean;
  mcpError: string | null;
  mcpLoading: boolean;
  mcpServersJson: string;
  mcpSuccess: boolean;
  modelOptions: DropdownSelectOption[];
  reasoningOptions: DropdownSelectOption[];
  roleDefinition: string;
  runnerType: BaseCodingAgent;
  runtimeOptions: DropdownSelectOption[];
  saving: boolean;
  selectedMember: ProjectMemberWithExecution | null;
  selectedModelValue: string;
  selectedReasoningValue: string;
  skillLookup: BackendChatSkill[];
  skills: BackendChatSkill[];
  skillsError: string | null;
  skillsLoading: boolean;
  teamProtocolContent: string;
  teamProtocolDirty: boolean;
  teamProtocolError: string | null;
  teamProtocolLoading: boolean;
  teamProtocolSaving: boolean;
  teamProtocolAvailable: boolean;
  teamProtocolSuccess: boolean;
  t: TranslateFn;
  workspacePath: string;
  onMcpServersChange: (value: string) => void;
  onAcpConfigValueChange: (
    option: AcpConfigOptionSnapshot,
    value: AcpConfigValue,
  ) => void;
  onTeamProtocolChange: (value: string) => void;
  onToggleMcpServer: (serverKey: string) => void;
  setAllowedSkillIds: (ids: string[]) => void;
  setAcpAccessMode: (value: string) => void;
  setAcpAdditionalDirectories: (value: string) => void;
  setAcpAdditionalDirectoriesOverride: (value: boolean) => void;
  setAcpApprovalMode: (value: string) => void;
  setAcpAuthMode: (value: string) => void;
  setAcpAuthMethodId: (value: string) => void;
  setIsLeader: (value: boolean | ((current: boolean) => boolean)) => void;
  setMemberName: (value: string) => void;
  setModelName: (value: string) => void;
  setModelVariant: (value: string) => void;
  setRoleDefinition: (value: string) => void;
  setRunnerType: (runnerType: BaseCodingAgent) => void;
  setThinkingEffort: (value: string) => void;
  setWorkspacePath: (value: string) => void;
};

function ConfigSection({
  bodyClassName,
  children,
  className,
  description,
  title,
}: {
  bodyClassName?: string;
  children: ReactNode;
  className?: string;
  description?: string;
  title: string;
}) {
  return (
    <section className={cx("flex flex-col pb-10", className)}>
      <div className="mb-3 px-0.5">
        <h3 className="text-[14px] font-semibold leading-[1.3] text-[var(--ink)]">
          {title}
        </h3>
        {description && (
          <p className="mt-1 max-w-[640px] text-[12.5px] leading-[1.55] text-[var(--ink-subtle)]">
            {description}
          </p>
        )}
      </div>
      <div className={cx("flex-1", bodyClassName)}>{children}</div>
    </section>
  );
}

function SettingRow({
  children,
  description,
  title,
  wide = false,
}: {
  children: ReactNode;
  description?: string;
  title: string;
  wide?: boolean;
}) {
  return (
    <div className="grid gap-2 border-t border-[var(--hairline)] py-4 first:border-t-0 first:pt-1 md:grid-cols-[minmax(150px,190px)_minmax(0,1fr)] md:items-start md:gap-6">
      <div className="min-w-0 pt-0.5">
        <p className="text-[13px] font-medium leading-[1.4] text-[var(--ink)]">
          {title}
        </p>
        {description && (
          <p className="mt-1 text-[12px] leading-[1.5] text-[var(--ink-subtle)]">
            {description}
          </p>
        )}
      </div>
      <div className={cx("min-w-0", !wide && "md:max-w-[400px]")}>
        {children}
      </div>
    </div>
  );
}

function MarkdownEditableField({
  disabled = false,
  minHeightClassName,
  onChange,
  placeholder,
  value,
}: {
  disabled?: boolean;
  minHeightClassName: string;
  onChange: (value: string) => void;
  placeholder: string;
  value: string;
}) {
  const [editing, setEditing] = useState(false);

  if (editing && !disabled) {
    return (
      <textarea
        autoFocus
        value={value}
        onChange={(event) => onChange(event.target.value)}
        onBlur={() => setEditing(false)}
        spellCheck={false}
        placeholder={placeholder}
        className={cx(
          "block w-full resize-y overflow-y-auto rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-1)] px-4 py-4 font-mono text-[14px] leading-relaxed text-[var(--ink)] outline-none transition-colors placeholder:text-[var(--ink-muted)] focus:border-[var(--hairline-strong)] focus:ring-2 focus:ring-[var(--primary-focus)]/35",
          minHeightClassName,
        )}
      />
    );
  }

  return (
    <div
      role={disabled ? undefined : "button"}
      tabIndex={disabled ? undefined : 0}
      onClick={() => {
        if (!disabled) setEditing(true);
      }}
      onKeyDown={(event) => {
        if (!disabled && (event.key === "Enter" || event.key === " ")) {
          event.preventDefault();
          setEditing(true);
        }
      }}
      className={cx(
        "w-full overflow-y-auto rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-1)] px-4 py-4 text-[14px] leading-relaxed transition-colors",
        minHeightClassName,
        disabled
          ? "cursor-not-allowed opacity-70"
          : "cursor-text hover:border-[var(--hairline-strong)] focus-visible:border-[var(--hairline-strong)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--primary-focus)]/35",
      )}
    >
      {value.trim() ? (
        <AgentMarkdown content={value} fontSize={14} />
      ) : (
        <span className="whitespace-pre-wrap text-[var(--ink-muted)]">
          {placeholder}
        </span>
      )}
    </div>
  );
}

function SkillSettingBlock({
  children,
  description,
  title,
}: {
  children: ReactNode;
  description?: string;
  title: string;
}) {
  return (
    <div className="space-y-3">
      <div>
        <p className="text-[13px] font-semibold leading-[1.35] text-[var(--ink)]">
          {title}
        </p>
        {description && (
          <p className="mt-1 text-[12px] leading-[1.5] text-[var(--ink-subtle)]">
            {description}
          </p>
        )}
      </div>
      <div className="min-w-0">{children}</div>
    </div>
  );
}

const inputClassName =
  "h-9 w-full rounded-[6px] border border-[var(--hairline)] bg-[var(--surface-1)] px-3 font-mono text-[13px] text-[var(--ink)] outline-none transition-colors placeholder:text-[var(--ink-tertiary)] focus:border-[var(--hairline-strong)] focus:ring-2 focus:ring-[var(--primary-focus)]/35";

function SkillsSection({
  allowedSkillIds,
  skillLookup,
  skills,
  skillsError,
  skillsLoading,
  setAllowedSkillIds,
  t,
}: {
  allowedSkillIds: string[];
  skillLookup: BackendChatSkill[];
  skills: BackendChatSkill[];
  skillsError: string | null;
  skillsLoading: boolean;
  setAllowedSkillIds: (ids: string[]) => void;
  t: TranslateFn;
}) {
  const [detailSkillId, setDetailSkillId] = useState<string | null>(null);
  const detailSkill =
    skills.find((skill) => skill.id === detailSkillId) ?? null;
  const selectedSkillIds = new Set(allowedSkillIds);
  const selectedSkills = allowedSkillIds.map((skillId) => ({
    id: skillId,
    skill:
      skills.find((item) => item.id === skillId) ??
      skillLookup.find((item) => item.id === skillId) ??
      null,
  }));

  const toggleSkill = (skill: BackendChatSkill) => {
    if (selectedSkillIds.has(skill.id)) {
      setAllowedSkillIds(allowedSkillIds.filter((id) => id !== skill.id));
      return;
    }

    setAllowedSkillIds([...allowedSkillIds, skill.id]);
  };

  const removeSkill = (skillId: string) => {
    setAllowedSkillIds(allowedSkillIds.filter((id) => id !== skillId));
  };

  return (
    <>
      <SkillSettingBlock title={t("teamPage.skills.addTitle")}>
        {selectedSkills.length === 0 ? (
          <p className="rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-1)] p-3 text-[14px] text-[var(--ink-subtle)]">
            {t("teamPage.skills.noneAdded")}
          </p>
        ) : (
          <div className="flex flex-wrap gap-2">
            {selectedSkills.map(({ id, skill }) => (
              <button
                key={id}
                type="button"
                onClick={() => removeSkill(id)}
                className="inline-flex h-8 max-w-full items-center gap-2 rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-1)] px-2.5 text-[13px] font-medium text-[var(--ink-muted)] transition-colors hover:border-[var(--hairline-strong)] hover:text-[var(--ink)]"
              >
                <span className="truncate">{skill?.name ?? id}</span>
                <X className="h-3.5 w-3.5 shrink-0 text-[var(--ink-tertiary)]" />
              </button>
            ))}
          </div>
        )}
      </SkillSettingBlock>

      <SkillSettingBlock title={t("teamPage.skills.installedTitle")}>
        {skillsLoading ? (
          <p className="rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-1)] p-3 text-[14px] text-[var(--ink-subtle)]">
            {t("teamPage.skills.loading")}
          </p>
        ) : skillsError ? (
          <p className="rounded-[8px] border border-red-500/20 bg-red-500/10 p-3 text-[14px] text-red-400">
            {skillsError}
          </p>
        ) : skills.length === 0 ? (
          <p className="rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-1)] p-3 text-[14px] text-[var(--ink-subtle)]">
            {t("teamPage.skills.noneInstalled")}
          </p>
        ) : (
          <div
            className={cx(
              "grid gap-4",
              detailSkill &&
                "xl:grid-cols-[minmax(420px,1fr)_minmax(320px,0.85fr)]",
            )}
          >
            <div className="min-w-0">
              <div className="grid gap-2 md:grid-cols-2 xl:grid-cols-3">
                {skills.map((skill) => {
                  const selected = selectedSkillIds.has(skill.id);
                  return (
                    <div
                      key={skill.id}
                      role="button"
                      tabIndex={0}
                      onClick={() => setDetailSkillId(skill.id)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter" || event.key === " ") {
                          event.preventDefault();
                          setDetailSkillId(skill.id);
                        }
                      }}
                      className={cx(
                        "flex min-h-[64px] min-w-0 cursor-pointer overflow-hidden rounded-[8px] border bg-[var(--surface-1)] p-2.5 text-left transition-colors",
                        selected
                          ? "border-[var(--primary)]/35 bg-[var(--primary-tint)]"
                          : "border-[var(--hairline)] hover:border-[var(--hairline-strong)] hover:bg-[var(--surface-3)]",
                      )}
                    >
                      <div className="flex min-w-0 flex-1 items-start gap-2">
                        <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-[8px] border border-[var(--mono-border)] bg-[var(--mono-bg)] text-[var(--ink-muted)]">
                          <FolderGit2 className="h-3.5 w-3.5" />
                        </span>
                        <div className="min-w-0 flex-1">
                          <p className="truncate text-[13px] font-medium text-[var(--ink)]">
                            {skill.name}
                          </p>
                          <p className="mt-1 truncate text-[12px] leading-[1.35] text-[var(--ink-subtle)]">
                            {skill.description || t("teamPage.fallback.noDesc")}
                          </p>
                        </div>
                        <div className="flex shrink-0 items-center gap-1.5">
                          <button
                            type="button"
                            onClick={(event) => {
                              event.stopPropagation();
                              toggleSkill(skill);
                            }}
                            className={cx(
                              "inline-flex h-7 w-7 items-center justify-center rounded-[8px] border transition-colors",
                              selected
                                ? "border-[var(--primary)]/35 bg-[var(--primary-tint)] text-[var(--primary)]"
                                : "border-[var(--hairline)] bg-[var(--surface-2)] text-[var(--ink-subtle)] hover:text-[var(--ink)]",
                            )}
                            aria-label={
                              selected
                                ? t("teamPage.action.added")
                                : t("teamPage.action.add")
                            }
                          >
                            {selected ? (
                              <Check className="h-3.5 w-3.5" />
                            ) : (
                              <PackagePlus className="h-3.5 w-3.5" />
                            )}
                          </button>
                          <button
                            type="button"
                            onClick={(event) => {
                              event.stopPropagation();
                              setDetailSkillId(
                                detailSkillId === skill.id ? null : skill.id,
                              );
                            }}
                            className={cx(
                              "flex h-7 w-7 shrink-0 items-center justify-center rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-2)] text-[var(--ink-tertiary)] transition-colors hover:text-[var(--primary)]",
                              detailSkillId === skill.id &&
                                "text-[var(--primary)]",
                            )}
                            aria-label={t("teamPage.aria.viewSkill", {
                              name: skill.name,
                            })}
                          >
                            <CircleAlert className="h-3.5 w-3.5" />
                          </button>
                        </div>
                      </div>
                    </div>
                  );
                })}
              </div>
            </div>

            {detailSkill && (
              <SkillMarkdownPanel
                skill={detailSkill}
                onClose={() => setDetailSkillId(null)}
                t={t}
              />
            )}
          </div>
        )}
      </SkillSettingBlock>
    </>
  );
}

function SkillMarkdownPanel({
  skill,
  t,
  onClose,
}: {
  skill: BackendChatSkill;
  t: TranslateFn;
  onClose: () => void;
}) {
  const tags = skill.tags ?? [];
  const triggerKeywords = skill.trigger_keywords ?? [];

  return (
    <div className="min-w-0 rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-1)] p-4">
      <div className="flex min-w-0 items-start justify-between gap-3">
        <div className="min-w-0">
          <p className="truncate text-[13px] font-medium text-[var(--ink)]">
            {skill.name}
          </p>
          <p className="mt-1 text-[12px] leading-[1.45] text-[var(--ink-subtle)]">
            {skill.description || t("teamPage.fallback.noDesc")}
          </p>
        </div>
        <button
          type="button"
          onClick={onClose}
          className="flex h-8 w-8 shrink-0 items-center justify-center rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-2)] text-[var(--ink-tertiary)] transition-colors hover:text-[var(--ink)]"
          aria-label={t("teamPage.aria.closeSkill", { name: skill.name })}
        >
          <X className="h-4 w-4" />
        </button>
      </div>
      {(tags.length > 0 || triggerKeywords.length > 0) && (
        <div className="mt-3 flex flex-wrap gap-1.5">
          {[...tags, ...triggerKeywords].slice(0, 8).map((tag, index) => (
            <span
              key={`${tag}-${index}`}
              className="rounded-[4px] border border-[var(--hairline)] px-1.5 py-0.5 font-mono text-[11px] text-[var(--ink-tertiary)]"
            >
              {tag}
            </span>
          ))}
        </div>
      )}
      <div className="mt-4 max-h-[420px] overflow-auto rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-2)] p-4 text-[12px] leading-relaxed text-[var(--ink-muted)] ot-scroll-area-styled">
        <AgentMarkdown
          content={skill.content || t("teamPage.fallback.noSkillContent")}
          fontSize={12}
        />
      </div>
    </div>
  );
}

function ConfigTab({
  acpConfigOptions,
  acpConfigOverrides,
  acpProbeLoading,
  reasoningUnsupported,
  capability,
  isLeader,
  legacyModelName,
  legacyThinkingEffort,
  modelOptions,
  reasoningOptions,
  roleDefinition,
  runnerType,
  runtimeOptions,
  selectedModelValue,
  selectedReasoningValue,
  workspacePath,
  onAcpConfigValueChange,
  setIsLeader,
  setMemberName,
  setModelName,
  setModelVariant,
  setRoleDefinition,
  setRunnerType,
  setThinkingEffort,
  setWorkspacePath,
  t,
  memberName,
  memberNamePlaceholder,
}: Omit<
  TeamConfigTabsProps,
  | "configuredMcpServerKeys"
  | "mcpApplying"
  | "mcpConfig"
  | "mcpConfigPath"
  | "mcpDirty"
  | "mcpError"
  | "mcpLoading"
  | "mcpServersJson"
  | "mcpSuccess"
  | "onMcpServersChange"
  | "onToggleMcpServer"
  | "teamProtocolContent"
  | "teamProtocolDirty"
  | "teamProtocolError"
  | "teamProtocolLoading"
  | "teamProtocolSaving"
  | "teamProtocolAvailable"
  | "teamProtocolSuccess"
  | "onTeamProtocolChange"
  | "memberDirty"
  | "memberSuccess"
  | "saving"
  | "selectedMember"
  | "allowedSkillIds"
  | "setAllowedSkillIds"
  | "skillLookup"
  | "skills"
  | "skillsError"
  | "skillsLoading"
>) {
  const acpModelOption = findAcpSelectConfigOption(
    acpConfigOptions,
    "model",
  );
  const acpThoughtLevelOption = findAcpSelectConfigOption(
    acpConfigOptions,
    "thought_level",
  );
  const acpModelValue = acpModelOption
    ? effectiveAcpConfigValue(
        acpModelOption,
        acpConfigOverrides,
        legacyModelName,
        legacyThinkingEffort,
      )
    : null;
  const acpThoughtLevelValue = acpThoughtLevelOption
    ? effectiveAcpConfigValue(
        acpThoughtLevelOption,
        acpConfigOverrides,
        legacyModelName,
        legacyThinkingEffort,
      )
    : null;

  return (
    <div className="space-y-0 xl:grid xl:grid-cols-[minmax(0,1fr)_minmax(0,1fr)] xl:gap-12">
      <ConfigSection
        title={t("teamPage.config.title")}
        description={t("teamPage.config.desc")}
        className="xl:pb-0"
      >
          <SettingRow
            title={t("teamPage.form.memberName")}
            description={t("teamPage.form.memberNameDesc")}
          >
            <input
              value={memberName}
              onChange={(event) => setMemberName(event.target.value)}
              placeholder={memberNamePlaceholder}
              autoComplete="off"
              className={inputClassName}
            />
          </SettingRow>

          <SettingRow
            title={t("teamPage.form.runtime")}
            description={t("teamPage.form.runtimeDesc")}
          >
            <DropdownSelect
              value={runnerType}
              options={runtimeOptions}
              searchPlaceholder={t("teamPage.search.runtimes")}
              className="[&>button]:h-9 [&>button]:bg-[var(--surface-1)] [&>button]:font-mono [&>button]:text-[13px]"
              triggerIcon={
                <AgentBrandAvatar
                  runner={runnerType}
                  framed={false}
                  className="h-4 w-4 text-[var(--ink-tertiary)]"
                  iconClassName="h-3.5 w-3.5"
                />
              }
              onChange={(value) => setRunnerType(value as BaseCodingAgent)}
            />
          </SettingRow>

          <SettingRow
            title={t("teamPage.form.model")}
            description={t("teamPage.form.modelDesc")}
          >
            {acpModelOption ? (
              <DropdownSelect
                value={
                  acpModelValue?.type === "value_id"
                    ? acpModelValue.value
                    : acpModelOption.current_value
                }
                options={acpModelOption.options.map((choice) => ({
                  id: choice.value,
                  label: choice.name,
                  description: choice.description ?? undefined,
                }))}
                searchPlaceholder={t("teamPage.search.models")}
                className="[&>button]:h-9 [&>button]:bg-[var(--surface-1)] [&>button]:font-mono [&>button]:text-[13px]"
                onChange={(value) =>
                  onAcpConfigValueChange(acpModelOption, {
                    type: "value_id",
                    value,
                  })
                }
              />
            ) : acpProbeLoading && modelOptions.length <= 1 ? (
              <DropdownSelect
                value="__openteams_model_loading__"
                options={[
                  {
                    id: "__openteams_model_loading__",
                    label: t("teamPage.form.modelLoading"),
                  },
                ]}
                disabled
                showSearch={false}
                triggerIcon={
                  <Loader2 className="h-3.5 w-3.5 animate-spin text-[var(--ink-tertiary)]" />
                }
                className="[&>button]:h-9 [&>button]:bg-[var(--surface-1)] [&>button]:font-mono [&>button]:text-[13px]"
                onChange={() => undefined}
              />
            ) : (
              <DropdownSelect
                value={selectedModelValue}
                options={modelOptions}
                searchPlaceholder={t("teamPage.search.models")}
                className="[&>button]:h-9 [&>button]:bg-[var(--surface-1)] [&>button]:font-mono [&>button]:text-[13px]"
                onChange={(value) =>
                  setModelName(value === defaultOptionId ? "" : value)
                }
              />
            )}
          </SettingRow>

          <SettingRow
            title={t("teamPage.form.reasoning")}
            description={t("teamPage.form.reasoningDesc")}
          >
            {acpThoughtLevelOption ? (
              <DropdownSelect
                value={
                  acpThoughtLevelValue?.type === "value_id"
                    ? acpThoughtLevelValue.value
                    : acpThoughtLevelOption.current_value
                }
                options={acpThoughtLevelOption.options.map((choice) => ({
                  id: choice.value,
                  label: choice.name,
                  description: choice.description ?? undefined,
                }))}
                showSearch={false}
                className="[&>button]:h-9 [&>button]:bg-[var(--surface-1)] [&>button]:font-mono [&>button]:text-[13px]"
                onChange={(value) =>
                  onAcpConfigValueChange(acpThoughtLevelOption, {
                    type: "value_id",
                    value,
                  })
                }
              />
            ) : reasoningUnsupported ? (
              <DropdownSelect
                value="__openteams_reasoning_unsupported__"
                options={[
                  {
                    id: "__openteams_reasoning_unsupported__",
                    label: t("teamPage.options.reasoningUnsupported"),
                  },
                ]}
                disabled
                showSearch={false}
                className="[&>button]:h-9 [&>button]:bg-[var(--surface-1)] [&>button]:font-mono [&>button]:text-[13px]"
                onChange={() => undefined}
              />
            ) : (
              <DropdownSelect
                value={selectedReasoningValue}
                options={reasoningOptions}
                showSearch={false}
                className="[&>button]:h-9 [&>button]:bg-[var(--surface-1)] [&>button]:font-mono [&>button]:text-[13px]"
                onChange={(value) => {
                  const nextValue =
                    value === defaultOptionId ? "" : value;
                  if (capability?.kind === "variant") {
                    setModelVariant(nextValue);
                  } else {
                    setThinkingEffort(nextValue);
                  }
                }}
              />
            )}
          </SettingRow>

          <SettingRow
            title={t("teamPage.form.workspacePath")}
            description={t("teamPage.form.workspacePathDesc")}
          >
            <input
              value={workspacePath}
              onChange={(event) => setWorkspacePath(event.target.value)}
              placeholder={t("teamPage.placeholder.workspacePath")}
              className={inputClassName}
            />
          </SettingRow>

          <SettingRow
            title={t("teamPage.form.mainAgent")}
            description={t("teamPage.form.mainAgentDesc")}
          >
            <button
              type="button"
              onClick={() => setIsLeader((value) => !value)}
              aria-label={t("teamPage.aria.toggleMainAgent")}
              aria-pressed={isLeader}
              className={cx(
                "relative h-[22px] w-10 rounded-full border transition-colors",
                isLeader
                  ? "border-[var(--primary)] bg-[var(--primary)]"
                  : "border-[var(--hairline-strong)] bg-[var(--surface-1)]",
              )}
            >
              <span
                className={cx(
                  "absolute left-0.5 top-0.5 h-4 w-4 rounded-full bg-white shadow-sm transition-transform",
                  isLeader ? "translate-x-[18px]" : "translate-x-0",
                )}
              />
            </button>
          </SettingRow>
        </ConfigSection>

      <ConfigSection
        title={t("teamPage.systemPrompt.title")}
        description={t("teamPage.systemPrompt.desc")}
        bodyClassName="!p-0 flex flex-col"
      >
        <MarkdownEditableField
          value={roleDefinition}
          onChange={setRoleDefinition}
          placeholder={t("teamPage.systemPrompt.placeholder")}
          minHeightClassName="min-h-[360px] flex-1"
        />
      </ConfigSection>
    </div>
  );
}

function PermissionsTab({
  acpAccessMode,
  acpAdditionalDirectories,
  acpAdditionalDirectoriesOverride,
  acpApprovalMode,
  acpAuthMode,
  acpAuthMethodId,
  setAcpAccessMode,
  setAcpAdditionalDirectories,
  setAcpAdditionalDirectoriesOverride,
  setAcpApprovalMode,
  setAcpAuthMode,
  setAcpAuthMethodId,
  t,
}: Pick<
  TeamConfigTabsProps,
  | "acpAccessMode"
  | "acpAdditionalDirectories"
  | "acpAdditionalDirectoriesOverride"
  | "acpApprovalMode"
  | "acpAuthMode"
  | "acpAuthMethodId"
  | "setAcpAccessMode"
  | "setAcpAdditionalDirectories"
  | "setAcpAdditionalDirectoriesOverride"
  | "setAcpApprovalMode"
  | "setAcpAuthMode"
  | "setAcpAuthMethodId"
  | "t"
>) {
  const [pendingRiskyChange, setPendingRiskyChange] = useState<
    "full_access" | "auto_allow" | null
  >(null);
  const dropdownClassName =
    "[&>button]:h-9 [&>button]:bg-[var(--surface-1)] [&>button]:font-mono [&>button]:text-[13px]";

  return (
    <>
      <ConfigSection
        title="权限与审批"
        description="成员设置会覆盖 Agents 页面中的全局默认值。"
      >
      <SettingRow
        title="文件权限"
        description={t("permissions.fullAccessDescription")}
      >
        <DropdownSelect
          value={acpAccessMode}
          options={[
            { id: "", label: "继承全局设置" },
            { id: "workspace_only", label: "仅工作区" },
            {
              id: "full_access",
              label: t("permissions.fullAccessHighRisk"),
            },
          ]}
          showSearch={false}
          className={dropdownClassName}
          onChange={(value) => {
            if (value === "full_access") {
              setPendingRiskyChange("full_access");
              return;
            }
            setAcpAccessMode(value);
          }}
        />
      </SettingRow>

      <SettingRow
        title="审批策略"
        description="自动允许会跳过用户确认，请仅用于可信成员。"
      >
        <DropdownSelect
          value={acpApprovalMode}
          options={[
            { id: "", label: "继承全局设置" },
            { id: "ask", label: "每次询问" },
            { id: "auto_allow", label: "自动允许（高风险）" },
            { id: "auto_reject", label: "自动拒绝" },
          ]}
          showSearch={false}
          className={dropdownClassName}
          onChange={(value) => {
            if (value === "auto_allow") {
              setPendingRiskyChange("auto_allow");
              return;
            }
            setAcpApprovalMode(value);
          }}
        />
      </SettingRow>

      <SettingRow
        title="认证方法"
        description="留空时自动使用 CLI 已有登录状态；指定值必须由 ACP Agent 公布。"
      >
        <div className="space-y-2">
          <DropdownSelect
            value={acpAuthMode}
            options={[
              { id: "", label: "继承全局设置" },
              { id: "auto", label: "自动（使用 CLI 登录态）" },
              { id: "method_id", label: "指定 auth_method_id" },
            ]}
            showSearch={false}
            className={dropdownClassName}
            onChange={setAcpAuthMode}
          />
          {acpAuthMode === "method_id" && (
            <input
              value={acpAuthMethodId}
              onChange={(event) => setAcpAuthMethodId(event.target.value)}
              placeholder="auth_method_id"
              className={inputClassName}
            />
          )}
        </div>
      </SettingRow>

      <SettingRow
        title="附加目录"
        description="启用覆盖后，每行一个绝对目录；空列表会显式清除全局附加目录。"
      >
        <div className="space-y-2">
          <label className="flex items-center gap-2 text-xs text-[var(--ink-subtle)]">
            <input
              type="checkbox"
              checked={acpAdditionalDirectoriesOverride}
              onChange={(event) =>
                setAcpAdditionalDirectoriesOverride(event.target.checked)
              }
            />
            覆盖全局附加目录
          </label>
          <textarea
            value={acpAdditionalDirectories}
            disabled={!acpAdditionalDirectoriesOverride}
            onChange={(event) =>
              setAcpAdditionalDirectories(event.target.value)
            }
            rows={4}
            placeholder={"/absolute/path/one\n/absolute/path/two"}
            className={`${inputClassName} h-auto resize-y font-mono text-xs disabled:opacity-50`}
          />
        </div>
      </SettingRow>
      </ConfigSection>
      {pendingRiskyChange && (
        <ConfirmationDialog
          idPrefix="member-acp-permission-confirmation"
          title={
            pendingRiskyChange === "full_access"
              ? t("permissions.fullAccessMemberConfirmTitle")
              : "为成员启用自动允许？"
          }
          description={
            pendingRiskyChange === "full_access"
              ? t("permissions.fullAccessMemberConfirmDescription")
              : "自动允许会跳过所有可允许的 ACP 工具审批。请仅对可信成员启用。"
          }
          confirmLabel="确认启用"
          cancelLabel="取消"
          escLabel="Esc 取消"
          tone="warning"
          onCancel={() => setPendingRiskyChange(null)}
          onConfirm={() => {
            const next = pendingRiskyChange;
            setPendingRiskyChange(null);
            if (next === "full_access") {
              setAcpAccessMode(next);
            } else {
              setAcpApprovalMode(next);
            }
          }}
        />
      )}
    </>
  );
}

function SkillsTab({
  allowedSkillIds,
  setAllowedSkillIds,
  skillLookup,
  skills,
  skillsError,
  skillsLoading,
  t,
}: Pick<
  TeamConfigTabsProps,
  | "allowedSkillIds"
  | "setAllowedSkillIds"
  | "skillLookup"
  | "skills"
  | "skillsError"
  | "skillsLoading"
  | "t"
>) {
  return (
    <div className="space-y-0">
      <ConfigSection
        title={t("teamPage.skills.title")}
        description={t("teamPage.skills.desc")}
        bodyClassName="space-y-6"
      >
        <SkillsSection
          allowedSkillIds={allowedSkillIds}
          skillLookup={skillLookup}
          skills={skills}
          skillsError={skillsError}
          skillsLoading={skillsLoading}
          setAllowedSkillIds={setAllowedSkillIds}
          t={t}
        />
      </ConfigSection>
    </div>
  );
}

type McpMeta = {
  description?: string;
  icon?: string;
  name?: string;
  url?: string;
};

const getMcpIconSrc = (icon?: string) =>
  icon ? `/${icon.replace(/^\/+/u, "")}` : null;

function McpConfigTab({
  configuredMcpServerKeys,
  mcpConfig,
  mcpConfigPath,
  mcpError,
  mcpLoading,
  mcpServersJson,
  onMcpServersChange,
  onToggleMcpServer,
  t,
}: Pick<
  TeamConfigTabsProps,
  | "configuredMcpServerKeys"
  | "mcpConfig"
  | "mcpConfigPath"
  | "mcpError"
  | "mcpLoading"
  | "mcpServersJson"
  | "onMcpServersChange"
  | "onToggleMcpServer"
  | "t"
>) {
  const preconfiguredObj = (mcpConfig?.preconfigured ?? {}) as Record<
    string,
    JsonValue | undefined
  >;
  const meta =
    typeof preconfiguredObj.meta === "object" &&
    preconfiguredObj.meta !== null &&
    !Array.isArray(preconfiguredObj.meta)
      ? (preconfiguredObj.meta as Record<string, McpMeta>)
      : {};
  const servers = Object.fromEntries(
    Object.entries(preconfiguredObj).filter(([key]) => key !== "meta"),
  );
  const unsupported = mcpError?.includes("support MCP") ?? false;

  return (
    <div className="space-y-6">
      {mcpError && !unsupported && (
        <div className="rounded-[8px] border border-red-500/20 bg-red-500/10 p-3 text-[14px] text-red-400">
          {t("teamPage.mcp.error", { error: mcpError })}
        </div>
      )}

      <ConfigSection
        title={t("teamPage.mcp.title")}
        description={t("teamPage.mcp.desc")}
      >
        {unsupported ? (
          <div className="m-4 rounded-[8px] border border-amber-500/30 bg-amber-500/10 p-4 text-[14px] leading-[1.5] text-amber-300">
            <p className="font-medium">{t("teamPage.mcp.unsupported")}</p>
            <p className="mt-1 text-[13px]">{mcpError}</p>
          </div>
        ) : (
          <>
            <SettingRow
              title={t("teamPage.mcp.serverConfig")}
              wide
              description={
                mcpLoading
                  ? t("teamPage.mcp.loadingCurrent")
                  : t("teamPage.mcp.savedToFile")
              }
            >
              <textarea
                value={
                  mcpLoading
                    ? t("teamPage.mcp.loadingTextarea")
                    : mcpServersJson
                }
                onChange={(event) => onMcpServersChange(event.target.value)}
                disabled={mcpLoading}
                rows={16}
                spellCheck={false}
                placeholder='{
  "mcpServers": {
    "server-name": {
      "command": "npx",
      "args": ["your-mcp-server"]
    }
  }
}'
                className="block w-full resize-y rounded-[8px] border border-[var(--hairline)] bg-[var(--surface-1)] px-4 py-3 font-mono text-[13px] leading-relaxed text-[var(--ink)] outline-none transition-colors placeholder:text-[var(--ink-tertiary)] focus:ring-2 focus:ring-[var(--primary-focus)]/50 disabled:opacity-70"
              />
              {mcpConfigPath && !mcpLoading && (
                <p className="mt-2 truncate font-mono text-[12px] text-[var(--ink-tertiary)]">
                  {mcpConfigPath}
                </p>
              )}
            </SettingRow>

            {mcpConfig?.preconfigured &&
              typeof mcpConfig.preconfigured === "object" &&
              Object.keys(servers).length > 0 && (
                <SettingRow
                  title={t("teamPage.mcp.builtinTitle")}
                  wide
                  description={t("teamPage.mcp.builtinDesc")}
                >
                  <div className="grid gap-2 md:grid-cols-2 xl:grid-cols-3">
                    {Object.entries(servers).map(([key]) => {
                      const metaObj = meta[key] ?? {};
                      const name = metaObj.name || key;
                      const description =
                        metaObj.description || t("teamPage.fallback.noDesc");
                      const icon = getMcpIconSrc(metaObj.icon);
                      const selected = configuredMcpServerKeys.includes(key);
                      return (
                        <button
                          key={key}
                          type="button"
                          onClick={() => onToggleMcpServer(key)}
                          className={cx(
                            "group flex min-w-0 items-start gap-3 rounded-[8px] border p-3 text-left transition-colors",
                            selected
                              ? "border-[var(--primary)]/45 bg-[var(--primary-tint)]"
                              : "border-[var(--hairline)] bg-[var(--surface-1)] hover:border-[var(--hairline-strong)] hover:bg-[var(--surface-3)]",
                          )}
                        >
                          <span className="flex h-8 w-8 shrink-0 items-center justify-center overflow-hidden rounded-[8px] bg-[var(--surface-1)]">
                            {icon ? (
                              <img
                                src={icon}
                                alt=""
                                className="h-full w-full object-contain"
                              />
                            ) : (
                              <Server className="h-4 w-4 text-[var(--ink-tertiary)]" />
                            )}
                          </span>
                          <span className="min-w-0 flex-1">
                            <span className="block truncate text-[14px] font-medium text-[var(--ink)]">
                              {name}
                            </span>
                            <span className="mt-1 line-clamp-2 block text-[12px] leading-[1.4] text-[var(--ink-subtle)]">
                              {description}
                            </span>
                          </span>
                          {selected ? (
                            <Check className="mt-1 h-3.5 w-3.5 shrink-0 text-[var(--primary)]" />
                          ) : (
                            <PackagePlus className="mt-1 h-3.5 w-3.5 shrink-0 text-[var(--ink-tertiary)] transition-colors group-hover:text-[var(--primary)]" />
                          )}
                        </button>
                      );
                    })}
                  </div>
                </SettingRow>
              )}
          </>
        )}
      </ConfigSection>
    </div>
  );
}

function TeamProtocolTab({
  onTeamProtocolChange,
  t,
  teamProtocolContent,
  teamProtocolError,
  teamProtocolLoading,
  teamProtocolAvailable,
}: Pick<
  TeamConfigTabsProps,
  | "onTeamProtocolChange"
  | "t"
  | "teamProtocolContent"
  | "teamProtocolError"
  | "teamProtocolLoading"
  | "teamProtocolAvailable"
>) {
  return (
    <div className="space-y-6">
      {teamProtocolError && (
        <div className="rounded-[8px] border border-red-500/20 bg-red-500/10 p-3 text-[14px] text-red-400">
          {t("teamPage.teamProtocol.error", { error: teamProtocolError })}
        </div>
      )}

      {!teamProtocolAvailable && (
        <div className="rounded-[8px] border border-amber-500/30 bg-amber-500/10 p-4 text-[14px] leading-[1.5] text-amber-300">
          {t("teamPage.teamProtocol.noProject")}
        </div>
      )}

      <ConfigSection
        title={t("teamPage.teamProtocol.title")}
        description={t("teamPage.teamProtocol.desc")}
        bodyClassName="!p-0"
      >
        <MarkdownEditableField
          value={
            teamProtocolLoading
              ? t("teamPage.teamProtocol.loading")
              : teamProtocolContent
          }
          onChange={onTeamProtocolChange}
          disabled={teamProtocolLoading || !teamProtocolAvailable}
          placeholder={t("teamPage.teamProtocol.placeholder")}
          minHeightClassName="min-h-[480px]"
        />
      </ConfigSection>
    </div>
  );
}

export function TeamConfigTabs(props: TeamConfigTabsProps) {
  const [activeTab, setActiveTab] = useState<MemberConfigTab>("config");
  const { selectedMember, t } = props;
  const supportsAcpPermissions =
    props.runnerType === "GEMINI" ||
    props.runnerType === "QWEN_CODE" ||
    props.runnerType === "KIMI_CODE" ||
    props.runnerType === "QODER_CLI" ||
    props.runnerType === "PI";
  const effectiveActiveTab = selectedMember
    ? activeTab === "permissions" && !supportsAcpPermissions
      ? "config"
      : activeTab
    : "teamProtocol";
  const dirtyNotice =
    props.teamProtocolDirty
      ? t("teamPage.notice.unsavedTeamProtocol")
      : props.memberDirty && props.mcpDirty
        ? t("teamPage.notice.unsavedBoth")
        : props.memberDirty
          ? t("teamPage.notice.unsavedMember")
          : props.mcpDirty
            ? t("teamPage.notice.unsavedMcp")
            : null;
  const savedNotice =
    props.teamProtocolSuccess
      ? t("teamPage.notice.savedTeamProtocol")
      : props.memberSuccess && props.mcpSuccess
        ? t("teamPage.notice.savedBoth")
        : props.memberSuccess
          ? t("teamPage.notice.savedMember")
          : props.mcpSuccess
            ? t("teamPage.notice.savedMcp")
            : null;
  const savingNotice =
    props.saving || props.mcpApplying || props.teamProtocolSaving
      ? t("teamPage.action.saving")
      : null;
  const statusNotice = savingNotice ?? dirtyNotice ?? savedNotice;
  const statusKind = savingNotice
    ? "saving"
    : dirtyNotice
      ? "dirty"
      : savedNotice
        ? "saved"
        : null;
  const tabItems = useMemo(() => {
    const memberTabs = [
      {
        id: "config" as const,
        label: t("teamPage.tabs.config"),
        icon: Settings,
      },
      ...(supportsAcpPermissions
        ? [
            {
              id: "permissions" as const,
              label: t("teamPage.tabs.permissions"),
              icon: ShieldCheck,
            },
          ]
        : []),
      {
        id: "skills" as const,
        label: t("teamPage.tabs.skills"),
        icon: FolderGit2,
      },
      { id: "mcp" as const, label: t("teamPage.tabs.mcp"), icon: Server },
    ];
    const protocolTab = {
      id: "teamProtocol" as const,
      label: t("teamPage.tabs.teamProtocol"),
      icon: FileText,
    };
    return selectedMember ? [...memberTabs, protocolTab] : [protocolTab];
  }, [selectedMember, supportsAcpPermissions, t]);

  return (
    <div className="flex h-full min-h-0 flex-col bg-[var(--surface-2)]">
      <div className="sticky top-0 z-20 flex shrink-0 items-end justify-between gap-4 border-b border-[var(--hairline)] bg-[var(--surface-2)] px-6">
        <div className="flex min-w-0 items-center gap-1">
          {tabItems.map((item) => {
            const Icon = item.icon;
            const active = effectiveActiveTab === item.id;
            return (
              <button
                key={item.id}
                type="button"
                onClick={() => setActiveTab(item.id)}
                className={cx(
                  "relative inline-flex h-10 items-center gap-1.5 px-3 text-[13px] font-medium transition-colors focus-visible:outline-none",
                  active
                    ? "text-[var(--ink)]"
                    : "text-[var(--ink-subtle)] hover:text-[var(--ink)]",
                )}
              >
                <Icon className="h-3.5 w-3.5" />
                {item.label}
                <span
                  className={cx(
                    "absolute inset-x-3 -bottom-px h-[2px] rounded-full transition-colors",
                    active ? "bg-[var(--primary)]" : "bg-transparent",
                  )}
                />
              </button>
            );
          })}
        </div>
        <div className="hidden min-w-0 items-center gap-2 pb-2.5 text-[13px] text-[var(--ink-subtle)] sm:flex">
          {statusNotice && (
            <span
              className={cx(
                "inline-flex min-w-0 items-center gap-1.5 text-[12px] font-medium",
                statusKind === "saved"
                  ? "text-[var(--success)]"
                  : "text-[var(--primary)]",
              )}
            >
              {statusKind === "saved" ? (
                <CheckCircle2 className="h-3.5 w-3.5 shrink-0" />
              ) : statusKind === "saving" ? (
                <RefreshCw className="h-3.5 w-3.5 shrink-0 animate-spin" />
              ) : (
                <AlertCircle className="h-3.5 w-3.5 shrink-0" />
              )}
              <span className="truncate">{statusNotice}</span>
            </span>
          )}
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-6 py-6 ot-scroll-area-styled">
        {effectiveActiveTab === "config" ? (
          <ConfigTab {...props} />
        ) : effectiveActiveTab === "permissions" ? (
          <PermissionsTab {...props} />
        ) : effectiveActiveTab === "skills" ? (
          <SkillsTab
            allowedSkillIds={props.allowedSkillIds}
            skillLookup={props.skillLookup}
            skills={props.skills}
            skillsError={props.skillsError}
            skillsLoading={props.skillsLoading}
            setAllowedSkillIds={props.setAllowedSkillIds}
            t={t}
          />
        ) : effectiveActiveTab === "mcp" ? (
          <McpConfigTab
            configuredMcpServerKeys={props.configuredMcpServerKeys}
            mcpConfig={props.mcpConfig}
            mcpConfigPath={props.mcpConfigPath}
            mcpError={props.mcpError}
            mcpLoading={props.mcpLoading}
            mcpServersJson={props.mcpServersJson}
            onMcpServersChange={props.onMcpServersChange}
            onToggleMcpServer={props.onToggleMcpServer}
            t={t}
          />
        ) : (
          <TeamProtocolTab
            teamProtocolContent={props.teamProtocolContent}
            teamProtocolError={props.teamProtocolError}
            teamProtocolLoading={props.teamProtocolLoading}
            teamProtocolAvailable={props.teamProtocolAvailable}
            onTeamProtocolChange={props.onTeamProtocolChange}
            t={t}
          />
        )}
      </div>
    </div>
  );
}