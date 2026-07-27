# Repository Guidelines

## Project

OpenTeams is a local-first multi-agent collaboration workspace with four main
surfaces:

- Free-form multi-agent chat with streaming, queues, approvals, attachments,
  diffs, and team protocols.
- Workflow execution built from React Flow plans and run by an orchestrator.
- Projects/issues with GitHub, source-control, delivery, and analytics views.
- Optional isolated Git worktrees for individual chat sessions.

Supported runtimes include Claude Code, Gemini CLI, Codex, Qwen Code,
OpenCode, Amp, and the bundled OpenTeams CLI.

## Repository Map

- `crates/db/`: SQLx models, migrations, and offline query cache.
- `crates/server/`: Axum routes and API binaries. Keep route handlers thin.
- `crates/services/`: business logic for chat, workflows, worktrees, projects,
  GitHub, source control, agents, and analytics.
- `crates/executors/`, `crates/git/`, `crates/review/`, `crates/utils/`:
  supporting Rust crates.
- `frontend/`: React 19, TypeScript, Vite, and Tailwind v4 app.
- `shared/types.ts`: generated Rust-to-TypeScript declarations; never edit it
  manually.
- `openteams-cli/`: separate Bun workspace for the bundled CLI.
- `npx/`: NPX package wrappers.
- `docs/`: Mintlify and architecture/debugging documentation.
- `src-tauri/`: desktop app configuration; excluded from the main Rust
  workspace together with `crates/remote/`.

## Core Architecture

### Chat

- `crates/services/src/services/chat_runner/` owns free-chat execution,
  queues, streaming, prompts, run records, token metadata, and file-change
  capture.
- Chat routes live in `crates/server/src/routes/chat/`.
- Runtime files live under `<workspace>/.openteams/`; workflow events and
  transcripts live in database tables.

### Workflow

- Plan JSON and revisions in `chat_workflow_plans` and
  `chat_workflow_plan_revisions` are the truth source.
- `workflow/compiler/` validates plans and materializes execution graphs.
- `workflow/orchestrator/` owns scheduling, commands, retries, and runtime
  transitions.
- `workflow/orchestrator/reducer.rs` is the only legal writer for workflow
  runtime state.
- `workflow/runtime/` builds prompts, executes agents, persists transcripts,
  and produces frontend projections.
- `frontend/src/components/workflow/` renders workflow state and controls.

Every meaningful workflow transition must use the reducer, guard the expected
previous state where possible, write a typed event, and refresh the runtime/card
projection. Frontend controls must match backend-accepted states. Final
acceptance always belongs to the user.

### Session Worktrees

- `crates/services/src/services/session_worktree.rs` is the authoritative
  worktree state machine.
- `crates/db/src/models/chat_session_worktree.rs` owns persisted types and
  compare-and-swap helpers.
- `crates/services/src/services/worktree_manager.rs` is the low-level Git
  adapter.
- `crates/server/src/routes/chat/worktree.rs` must remain a thin service
  adapter.

Use compare-and-swap for status changes. Automated cleanup must never remove
unmerged active/conflicted worktrees; only explicit discard may move them
toward cleanup. Validate conflict paths as relative paths inside the resolved
workspace before filesystem access.

### Projects and Source Control

- Project routes: `crates/server/src/routes/projects.rs`,
  `project_source_control.rs`, and `project_github.rs`.
- Project/GitHub services: `crates/services/src/services/project/` and
  `crates/services/src/services/github/`.
- `services/project/source_control.rs` is authoritative for status, diffs,
  staging, discard, commits, safety guards, caching, and worktree-aware
  workspace selection.

Source-control data and mutations must stay scoped to the selected
project/session/worktree. Workspace resolution is currently duplicated across
chat, workflow, source control, and session routes; keep their behavior aligned
and prefer a shared resolver over adding another copy.

## Non-Negotiable Rules

- Use typed enums and serde snake_case values for persisted/wire statuses. Do
  not add `format!("{:?}", status).to_lowercase()`.
- Restrict plan JSON writes to lead/system code paths.
- Do not bypass workflow or worktree reducers with ad-hoc state updates.
- Never fall back to process cwd for user source-control or worktree actions.
- Treat `.openteams/` as runtime data, not user source.
- Rust types exposed to the frontend must derive `TS`, be registered in
  `crates/server/src/bin/generate_types.rs`, and be regenerated.
- Preserve existing service boundaries and compatibility paths unless the task
  explicitly changes them.
- Do not commit secrets or local `.env` values.

## Coding and Testing

- Rust: use `rustfmt`, snake_case functions/modules, PascalCase types, and
  small service methods with clear ownership.
- TypeScript/React: use 2-space indentation, PascalCase components, camelCase
  identifiers, and existing API/mapping patterns.
- Keep scoped fixes narrow; avoid unrelated refactors.
- Add tests for state machines, shared logic, migrations, security-sensitive
  paths, source-control scoping, and likely regressions.
- Run the narrowest meaningful verification first, then broaden when shared
  behavior is affected.

Key test locations:

- Workflow: `crates/services/src/services/workflow/orchestrator/tests.rs`
- Worktrees: `crates/services/src/services/session_worktree/tests.rs`
- Frontend: colocated `*.test.ts` and `*.test.tsx`

## Common Commands

```bash
pnpm install
pnpm run frontend:check
pnpm run frontend:build
pnpm run backend:check
pnpm run backend:lint
pnpm run check
pnpm run lint
pnpm run format
pnpm run format:check
pnpm run generate-types
pnpm run generate-types:check
pnpm run prepare-db
pnpm run prepare-db:check
```

Notes:

- `pnpm run format` only runs `cargo fmt --all`.
- `frontend:check` runs TypeScript with `--noEmit`.
- `backend:lint` runs Clippy with `--features qa-mode`.
- Use the local scripts inside `openteams-cli/` when changing that workspace.
