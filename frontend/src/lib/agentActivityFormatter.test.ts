// Smoke tests for tool activity UI formatting.
//
// Run with:
//     pnpm exec tsx src/lib/agentActivityFormatter.test.ts

import type { ChatRunActivityLine } from "@/types";
import {
  formatAgentActivityLines,
  isThinkingHeaderContent,
  truncatePathMiddle,
} from "./agentActivityFormatter";

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

const line = (
  sequence: number,
  line_type: ChatRunActivityLine["line_type"],
  content: string,
  agent_name = "codex",
  created_at = "2026-06-02T00:00:00.000Z",
): ChatRunActivityLine => ({
  line_id: `line-${sequence}`,
  run_id: "run-1",
  session_id: "session-1",
  session_agent_id: "session-agent-1",
  agent_id: "agent-1",
  agent_name,
  sequence,
  line_type,
  stream_type: line_type === "error" ? "error" : "thinking",
  content,
  created_at,
});

console.log("agentActivityFormatter");

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started command: cargo test -p services"),
    line(2, "tool", "Completed command: cargo test -p services"),
  ]);

  check("merges started/completed command into one row", rows.length === 1, rows);
  check("shows completed command copy", rows[0]?.title === "Command completed", rows);
  check("keeps command detail visible", rows[0]?.detail === "cargo test -p services", rows);
}

{
  const zh = (key: string): string =>
    ({
      "agentActivity.tool.file_read.completed": "文件已读取",
    })[key] ?? key;
  const rows = formatAgentActivityLines(
    [
      line(1, "tool", "start read: frontend-new/src/App.tsx"),
      line(2, "tool", "end read: frontend-new/src/App.tsx"),
    ],
    zh,
  );

  check("merges legacy start/end read into one row", rows.length === 1, rows);
  check("uses translated file read copy", rows[0]?.title === "文件已读取", rows);
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started command: pnpm run frontend-new:check"),
  ]);

  check("keeps running-only start as in-progress", rows[0]?.title === "Running command", rows);
}

{
  const failedRows = formatAgentActivityLines([
    line(1, "tool", "Started command: pnpm test"),
    line(2, "tool", "Failed command: pnpm test"),
  ]);
  const deniedRows = formatAgentActivityLines([
    line(1, "tool", "Started tool: ApplyPatch"),
    line(2, "tool", "Denied tool: ApplyPatch"),
  ]);
  const timedOutRows = formatAgentActivityLines([
    line(1, "tool", "Started command: pnpm build"),
    line(2, "tool", "Timed out command: pnpm build"),
  ]);

  check("failed status overrides running status", failedRows[0]?.title === "Command failed", failedRows);
  check("denied status overrides running status", deniedRows[0]?.title === "Tool call denied", deniedRows);
  check("timed out status overrides running status", timedOutRows[0]?.title === "Command timed out", timedOutRows);
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started command: pnpm test"),
    line(2, "tool", "Completed command: pnpm test"),
    line(3, "tool", "Started command: pnpm test"),
    line(4, "tool", "Completed command: pnpm test"),
  ]);

  check("does not merge repeated same command rounds into one row", rows.length === 2, rows);
  check(
    "keeps both repeated command rounds completed",
    rows.every((row) => row.title === "Command completed"),
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started Tool: ApplyPatch"),
    line(2, "tool", "Completed Tool: ApplyPatch: patch applied"),
  ]);

  check("merges tool completion lines with result previews", rows.length === 1, rows);
  check("keeps the completed tool target visible", rows[0]?.detail === "ApplyPatch", rows);
  check("separates the completed tool result for disclosure", rows[0]?.resultDetail === "patch applied", rows);
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started command: pnpm test"),
    line(2, "tool", "Completed command: pnpm test: All tests passed"),
  ]);

  check("keeps the completed command visible", rows[0]?.detail === "pnpm test", rows);
  check("separates command output for disclosure", rows[0]?.resultDetail === "All tests passed", rows);
}

{
  const rows = formatAgentActivityLines([
    line(1, "thinking", "I am checking the workspace."),
    line(2, "tool", "Raw tool log without a known prefix"),
  ]);

  check("leaves non-tool lines unchanged", rows[0]?.content === "I am checking the workspace.", rows);
  check("leaves unparsed tool lines unchanged", rows[1]?.content === "Raw tool log without a known prefix", rows);
}

{
  const rows = formatAgentActivityLines([
    line(1, "thinking", "<!-- -->**Planning code inspection**"),
    line(2, "thinking", "<!--  -->"),
  ]);

  check("strips Codex empty HTML comment prefixes from thinking lines", rows.length === 1, rows);
  check("keeps the visible thinking markdown after stripping", rows[0]?.content === "**Planning code inspection**", rows);
}

{
  const rows = formatAgentActivityLines([
    line(1, "thinking", "<!-- -->visible non-codex text", "Claude"),
  ]);

  check("does not strip empty HTML comments for non-Codex activity lines", rows[0]?.content === "<!-- -->visible non-codex text", rows);
}

{
  const rows = formatAgentActivityLines([
    line(
      1,
      "thinking",
      "Skill descriptions were shortened to fit the 2% skills context budget. Codex can still see every skill.",
    ),
    line(2, "thinking", "Real reasoning step"),
  ]);

  check("drops harness-internal notices from the activity log", rows.length === 1, rows);
  check("keeps genuine thinking lines", rows[0]?.content === "Real reasoning step", rows);
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started command: /bin/zsh -lc 'cd /tmp/repo && rg -n \"prompt\" crates/services'"),
    line(2, "tool", "Completed command: /bin/zsh -lc 'cd /tmp/repo && rg -n \"prompt\" crates/services'"),
  ]);

  check("merges shell-wrapped command into one row", rows.length === 1, rows);
  check(
    "strips the shell wrapper and cd prefix from command details",
    rows[0]?.detail === 'rg -n "prompt" crates/services',
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", 'Started command: bash -lc "pnpm test"'),
  ]);

  check(
    "unwraps double-quoted bash wrappers",
    rows[0]?.detail === "pnpm test",
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", `Started command: /bin/zsh -lc 'rg -n "prompt" crates'`),
    line(
      2,
      "tool",
      `Completed command: /bin/zsh -lc 'rg -n "prompt" crates': crates/a.rs:1: match`,
    ),
  ]);

  check("merges quoted command with result suffix", rows.length === 1, rows);
  check(
    "strips quotes while keeping the result preview split",
    rows[0]?.detail === 'rg -n "prompt" crates' &&
      rows[0]?.resultDetail === "crates/a.rs:1: match",
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(
      1,
      "tool",
      `Completed command: "sed -n '1,240p' a.ts; sed -n '700,850p' b.tsx": use std::fmt`,
    ),
  ]);

  check(
    "strips double quotes around semicolon chains with result suffix",
    rows[0]?.detail === "sed -n '1,240p' a.ts; sed -n '700,850p' b.tsx: use std::fmt",
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(
      1,
      "error",
      "2026-08-01T01:34:06.922061Z ERROR codex_models_manager::cache: failed to load models cache: missing field `supports_reasoning_summaries`",
    ),
    line(2, "thinking", "Real reasoning step"),
  ]);

  check(
    "drops timestamped harness log lines from the activity log",
    rows.length === 1 && rows[0]?.content === "Real reasoning step",
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started command: pnpm test", "codex", "2026-06-02T00:00:00.000Z"),
    line(2, "tool", "Completed command: pnpm test", "codex", "2026-06-02T00:00:02.500Z"),
  ]);

  check(
    "computes elapsed time from start to completion",
    rows[0]?.durationMs === 2500,
    rows,
  );
  check(
    "keeps the start timestamp on the merged row",
    rows[0]?.startedAt === "2026-06-02T00:00:00.000Z" &&
      rows[0]?.endedAt === "2026-06-02T00:00:02.500Z",
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started command: pnpm test", "codex", "2026-06-02T00:00:00.000Z"),
    line(2, "tool", "Completed command: pnpm test", "codex", "not-a-date"),
  ]);

  check(
    "leaves duration undefined when timestamps are unparseable",
    rows[0]?.durationMs === undefined,
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started file edit: frontend/src/App.tsx"),
    line(2, "tool", "Completed file edit: frontend/src/App.tsx (1 edit)"),
  ]);

  check(
    "merges file edit when only the completed line carries a change summary",
    rows.length === 1,
    rows,
  );
  check(
    "strips the change summary so the path stays the row target",
    rows[0]?.toolKind === "file_edit" &&
      rows[0]?.title === "File edit completed" &&
      rows[0]?.detail === "frontend/src/App.tsx",
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started file edit: crates/a.rs (1 write)"),
    line(2, "tool", "Completed file edit: crates/a.rs (1 write)"),
  ]);

  check(
    "merges file edit lines that both carry a change summary",
    rows.length === 1 && rows[0]?.detail === "crates/a.rs",
    rows,
  );
}

{
  // Persisted before the ACP/Kimi path recovery fix: the path never reached
  // the activity line, so the content ends right after the colon.
  const rows = formatAgentActivityLines([
    line(1, "tool", "Started file edit: "),
    line(2, "tool", "Completed file edit: "),
  ]);

  check("merges empty-path file edit history into one row", rows.length === 1, rows);
  check(
    "keeps a visible title for empty-path file edit history",
    rows[0]?.toolKind === "file_edit" &&
      rows[0]?.toolStatus === "completed" &&
      rows[0]?.title === "File edit completed",
    rows,
  );
  check(
    "leaves the detail empty so the panel falls back to the title",
    rows[0]?.detail === undefined && rows[0]?.content === "File edit completed",
    rows,
  );
}

{
  const rows = formatAgentActivityLines([
    line(1, "tool", "Completed file edit:  (1 edit)"),
    line(2, "tool", "Completed file read: "),
  ]);

  check(
    "treats a summary-only file edit target as an empty path",
    rows[0]?.title === "File edit completed" && rows[0]?.detail === undefined,
    rows,
  );
  check(
    "keeps a visible title for empty-path file read history",
    rows[1]?.toolKind === "file_read" &&
      rows[1]?.title === "File read" &&
      rows[1]?.detail === undefined,
    rows,
  );
}

{
  check(
    "keeps short paths untouched by middle truncation",
    truncatePathMiddle("frontend/src/App.tsx") === "frontend/src/App.tsx",
  );
  const longPath =
    "frontend/src/components/very/deeply/nested/folder/structure/AgentActivityPanel.tsx";
  const truncated = truncatePathMiddle(longPath, 40);
  check(
    "truncates long paths in the middle so the file name stays visible",
    truncated.length === 40 &&
      truncated.includes("…") &&
      truncated.endsWith("AgentActivityPanel.tsx"),
    truncated,
  );
}

{
  check(
    "promotes fully-bold summary lines to thinking headers",
    isThinkingHeaderContent("**Planning file inspection**"),
  );
  check(
    "keeps prose thinking lines as body text",
    !isThinkingHeaderContent("让我写回复。由于内容较多，我会尽量精炼但完整。"),
  );
  check(
    "keeps partially bold lines as body text",
    !isThinkingHeaderContent("**Header** followed by trailing prose"),
  );
}

if (failures > 0) {
  // eslint-disable-next-line no-console
  console.error(`\n${failures} agentActivityFormatter assertion(s) failed.`);
  process.exit(1);
} else {
  // eslint-disable-next-line no-console
  console.log("\nAll agentActivityFormatter assertions passed.");
}