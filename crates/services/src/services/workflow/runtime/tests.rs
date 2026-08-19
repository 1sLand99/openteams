#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

    use chrono::Utc;
    use db::models::{
        chat_agent::ChatAgent,
        chat_session::{ChatSession, ChatSessionStatus, ChatSessionWorktreeMode},
        chat_session_agent::{ChatSessionAgent, ChatSessionAgentState},
        member_execution_config::MemberExecutionConfig,
        workflow_plan::WorkflowPlan,
        workflow_plan_revision::WorkflowPlanRevision,
        workflow_types::{
            WorkflowAgentSessionRole, WorkflowAgentSessionState, WorkflowPlanStatus,
            WorkflowRevisionEditor, WorkflowValidationStatus, to_workflow_wire_value,
        },
    };
    use executors::logs::{FileChange, ToolResult};
    use sqlx::{SqlitePool, types::Json};

    use super::*;

    async fn setup_runtime_worktree_db() -> DBService {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create sqlite memory pool");
        sqlx::query(
            r#"
            CREATE TABLE chat_session_worktrees (
                id                    BLOB    NOT NULL PRIMARY KEY,
                session_id            BLOB    NOT NULL,
                project_id            BLOB,
                base_workspace_path   TEXT    NOT NULL,
                repo_path             TEXT    NOT NULL,
                base_branch           TEXT    NOT NULL,
                base_commit           TEXT,
                branch_name           TEXT    NOT NULL,
                worktree_path         TEXT    NOT NULL,
                mode                  TEXT    NOT NULL DEFAULT 'session'
                                            CHECK (mode IN ('session')),
                status                TEXT    NOT NULL DEFAULT 'creating'
                                            CHECK (status IN (
                                                'creating', 'active', 'dirty', 'merging',
                                                'needs_conflict_resolution', 'merged',
                                                'archived', 'cleanup_pending', 'cleanup_failed'
                                            )),
                merge_target_branch   TEXT,
                merge_operation       TEXT
                                            CHECK (merge_operation IS NULL
                                                   OR merge_operation IN (
                                                       'merge', 'squash_merge', 'cherry_pick', 'rebase'
                                                   )),
                conflict_files_json   TEXT    NOT NULL DEFAULT '[]',
                operation_started_at  TEXT,
                cleanup_error         TEXT,
                last_used_at          TEXT,
                merged_at             TEXT,
                archived_at           TEXT,
                created_at            TEXT    NOT NULL DEFAULT (datetime('now', 'subsec')),
                updated_at            TEXT    NOT NULL DEFAULT (datetime('now', 'subsec'))
            );

            CREATE UNIQUE INDEX idx_chat_session_worktrees_active_session
                ON chat_session_worktrees(session_id)
                WHERE status IN ('creating', 'active', 'dirty', 'merging',
                                 'needs_conflict_resolution', 'merged', 'cleanup_pending');

            CREATE TABLE chat_session_agents (
                id                  BLOB    NOT NULL PRIMARY KEY,
                session_id          BLOB    NOT NULL,
                agent_id            BLOB    NOT NULL,
                state               TEXT    NOT NULL DEFAULT 'idle',
                workspace_path      TEXT,
                pty_session_key     TEXT,
                agent_session_id    TEXT,
                agent_message_id    BLOB,
                project_member_id   BLOB,
                execution_config    TEXT    NOT NULL DEFAULT '{}',
                allowed_skill_ids   TEXT    NOT NULL DEFAULT '[]',
                created_at          TEXT    NOT NULL DEFAULT (datetime('now', 'subsec')),
                updated_at          TEXT    NOT NULL DEFAULT (datetime('now', 'subsec'))
            );

            CREATE TABLE chat_runs (
                id                       BLOB    NOT NULL PRIMARY KEY,
                session_id               BLOB    NOT NULL,
                session_agent_id         BLOB    NOT NULL,
                workspace_path           TEXT,
                run_index                INTEGER NOT NULL,
                run_dir                  TEXT    NOT NULL,
                input_path               TEXT,
                output_path              TEXT,
                raw_log_path             TEXT,
                meta_path                TEXT,
                log_state                TEXT    NOT NULL DEFAULT 'live',
                artifact_state           TEXT    NOT NULL DEFAULT 'full',
                log_truncated            INTEGER NOT NULL DEFAULT 0,
                log_capture_degraded     INTEGER NOT NULL DEFAULT 0,
                pruned_at                TEXT,
                prune_reason             TEXT,
                retention_summary_json   TEXT,
                created_at               TEXT    NOT NULL DEFAULT (datetime('now', 'subsec'))
            );

            CREATE UNIQUE INDEX idx_chat_runs_unique
                ON chat_runs(session_agent_id, run_index);
            "#,
        )
        .execute(&pool)
        .await
        .expect("create chat_session_worktrees test schema");
        DBService { pool }
    }

    fn sample_chat_session(
        worktree_mode: ChatSessionWorktreeMode,
        default_workspace_path: Option<String>,
    ) -> ChatSession {
        let now = Utc::now();
        ChatSession {
            id: Uuid::new_v4(),
            title: Some("workflow runtime test".to_string()),
            status: ChatSessionStatus::Active,
            lead_agent_id: None,
            lead_session_agent_id: None,
            summary_text: None,
            archive_ref: None,
            last_seen_diff_key: None,
            default_workspace_path,
            chat_input_mode: None,
            project_id: None,
            worktree_mode,
            pinned_at: None,
            created_at: now,
            updated_at: now,
            archived_at: None,
        }
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout={}\nstderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_git_repo(repo: &Path) {
        std::fs::create_dir_all(repo).expect("create repo dir");
        let output = Command::new("git")
            .arg("init")
            .arg("-b")
            .arg("main")
            .arg(repo)
            .output()
            .expect("git init");
        assert!(
            output.status.success(),
            "git init failed\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        git(repo, &["config", "user.email", "workflow@example.test"]);
        git(repo, &["config", "user.name", "Workflow Runtime"]);
        std::fs::write(repo.join("README.md"), "base\n").expect("write seed file");
        git(repo, &["add", "."]);
        git(repo, &["commit", "-m", "initial"]);
    }

    fn sample_plan_json() -> String {
        serde_json::json!({
            "version": "1",
            "title": "Projection Contract",
            "goal": "Verify projection statuses",
            "agents": {
                "lead": "agent-1",
                "available": ["agent-1"]
            },
            "nodes": [
                {
                    "id": "step-1",
                    "type": "workflowStep",
                    "position": { "x": 0.0, "y": 0.0 },
                    "data": {
                        "stepType": "task",
                        "agentId": "agent-1",
                        "title": "Step 1",
                        "instructions": "Run step 1"
                    }
                }
            ],
            "edges": []
        })
        .to_string()
    }

    #[tokio::test]
    async fn plan_generation_without_stream_context_persists_run_record() {
        let db = setup_runtime_worktree_db().await;
        let workspace = tempfile::TempDir::new().expect("create workflow workspace");
        let session = sample_chat_session(
            ChatSessionWorktreeMode::Inherit,
            Some(workspace.path().to_string_lossy().to_string()),
        );
        let (session_agents, _) = sample_agent_views();
        let mut session_agent = session_agents[0].clone();
        session_agent.session_id = session.id;
        let prompt = "# Workflow Plan Generation\n\nReturn a workflow plan.";

        let record = start_workflow_runtime_run_record(
            &db,
            &session,
            &session_agent,
            workspace.path(),
            prompt,
            None,
            None,
        )
        .await
        .expect("persist plan-generation run")
        .expect("plan generation must have a run record");

        assert_eq!(record.execution_id, None);
        assert_eq!(record.workflow_agent_session_id, None);
        assert_eq!(record.step_id, None);
        assert_eq!(record.step_key, None);

        let persisted = ChatRun::find_by_id(&db.pool, record.run_id)
            .await
            .expect("query plan-generation run")
            .expect("plan-generation run exists");
        assert_eq!(persisted.session_id, session.id);
        assert_eq!(persisted.session_agent_id, session_agent.id);
        assert_eq!(persisted.workspace_path.as_deref(), workspace.path().to_str());
        assert_eq!(
            std::fs::read_to_string(
                persisted
                    .input_path
                    .expect("plan-generation input path must be recorded")
            )
            .expect("read plan-generation input"),
            prompt
        );
    }

    #[test]
    fn workflow_prompt_debug_kind_covers_iteration_and_reviews() {
        assert_eq!(
            infer_workflow_prompt_debug_kind(
                "# Workflow Plan Generation\n\n## Iteration Context\nfeedback",
                false,
            ),
            "iteration_feedback_plan_generation"
        );
        assert_eq!(
            infer_workflow_prompt_debug_kind(
                "You are reviewing a worker's step task output.\n\n## Step Under Review",
                false,
            ),
            "lead_review"
        );
        assert_eq!(
            infer_workflow_prompt_debug_kind(
                "You are revising a step in an workflow based on review feedback.\n\n## User Revision Required",
                true,
            ),
            "step_revision_user_feedback"
        );
        assert_eq!(
            infer_workflow_prompt_debug_kind(
                "Your previous workflow loop review output response did not match the required JSON protocol.",
                true,
            ),
            "protocol_retry_loop_review_output"
        );
    }

    #[test]
    fn workflow_prompt_debug_step_key_can_be_extracted_from_prompt() {
        assert_eq!(
            extract_workflow_prompt_step_key(
                "Return one JSON object. Fill `step_key` with `build_ui`, `execution_id` with `abc`."
            ),
            Some("build_ui".to_string())
        );
        assert_eq!(
            extract_workflow_prompt_step_key("Rules:\n- step_key: qa_review\n- execution_id: abc"),
            Some("qa_review".to_string())
        );
    }

    fn sample_execution(status: WorkflowExecutionStatus) -> WorkflowExecution {
        let now = Utc::now();
        WorkflowExecution {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            plan_id: Uuid::new_v4(),
            active_revision_id: Some(Uuid::new_v4()),
            active_round_id: Some(Uuid::new_v4()),
            workflow_card_message_id: None,
            lead_session_agent_id: None,
            status,
            current_round: 1,
            title: "Projection Contract".to_string(),
            compiled_graph_hash: Some("hash".to_string()),
            started_at: None,
            completed_at: None,
            cleaned_at: None,
            cleaned_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_plan(plan_id: Uuid) -> WorkflowPlan {
        let now = Utc::now();
        WorkflowPlan {
            id: plan_id,
            session_id: Uuid::new_v4(),
            source_message_id: None,
            created_by_session_agent_id: None,
            status: WorkflowPlanStatus::Ready,
            title: "Projection Contract".to_string(),
            summary_text: Some("Verify projection statuses".to_string()),
            plan_json: sample_plan_json(),
            plan_schema_version: 1,
            plan_hash: "hash".to_string(),
            validation_status: WorkflowValidationStatus::Valid,
            validation_errors_json: None,
            workflow_card_message_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_revision(plan_id: Uuid, plan_json: String) -> WorkflowPlanRevision {
        WorkflowPlanRevision {
            id: Uuid::new_v4(),
            plan_id,
            revision_no: 1,
            edited_by: WorkflowRevisionEditor::Lead,
            editor_session_agent_id: None,
            reason: None,
            plan_json,
            plan_hash: "hash".to_string(),
            validation_status: WorkflowValidationStatus::Valid,
            validation_errors_json: None,
            created_at: Utc::now(),
        }
    }

    fn sample_step(status: WorkflowStepStatus) -> WorkflowStep {
        let now = Utc::now();
        WorkflowStep {
            id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            round_id: Uuid::new_v4(),
            compiled_revision_id: None,
            step_key: "step-1".to_string(),
            step_type: WorkflowStepType::Task,
            title: "Step 1".to_string(),
            instructions: "Run step 1".to_string(),
            assigned_workflow_agent_session_id: None,
            status,
            retry_count: 0,
            max_retry: 1,
            round_index: 1,
            display_order: 0,
            latest_run_id: None,
            summary_text: None,
            content: None,
            loop_id: None,
            lead_review_required: true,
            user_review_required: false,
            revision_context: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: None,
        }
    }

    fn sample_agent_views() -> (Vec<ChatSessionAgent>, Vec<ChatAgent>) {
        let now = Utc::now();
        let agent_id = Uuid::new_v4();
        let session_agent = ChatSessionAgent {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            agent_id,
            state: ChatSessionAgentState::Idle,
            workspace_path: None,
            pty_session_key: None,
            agent_session_id: None,
            agent_message_id: None,
            project_member_id: None,
            member_name: "member".to_string(),
            execution_config: Json(MemberExecutionConfig::default()),
            allowed_skill_ids: Json(Vec::new()),
            created_at: now,
            updated_at: now,
        };
        let agent = ChatAgent {
            id: agent_id,
            name: "Agent 1".to_string(),
            runner_type: "codex".to_string(),
            system_prompt: String::new(),
            tools_enabled: Json(serde_json::json!({})),
            model_name: None,
            owner_project_id: None,
            created_at: now,
            updated_at: now,
        };

        (vec![session_agent], vec![agent])
    }

    #[test]
    fn lead_resolution_prefers_session_member_id_for_shared_backing_agent() {
        let (mut session_agents, mut agents) = sample_agent_views();
        let mut second_member = session_agents[0].clone();
        second_member.id = Uuid::new_v4();
        second_member.member_name = "reviewer".to_string();
        let mut second_agent_view = agents[0].clone();
        second_agent_view.name = second_member.member_name.clone();
        session_agents.push(second_member.clone());
        agents.push(second_agent_view);

        let mut session = sample_chat_session(ChatSessionWorktreeMode::Inherit, None);
        session.id = second_member.session_id;
        session.lead_agent_id = Some(second_member.agent_id);
        session.lead_session_agent_id = Some(second_member.id);

        let (lead_agent, lead_member) =
            resolve_lead_agent(&session, &session_agents, &agents).expect("resolve lead member");
        assert_eq!(lead_member.id, second_member.id);
        assert_eq!(lead_agent.name, "reviewer");
    }

    #[test]
    fn changed_execution_config_forces_workflow_follow_up_to_spawn_fresh() {
        let (session_agents, _) = sample_agent_views();
        let session_agent = &session_agents[0];
        let workflow_session = WorkflowAgentSession {
            id: Uuid::new_v4(),
            workflow_execution_id: Uuid::new_v4(),
            session_agent_id: session_agent.id,
            role: WorkflowAgentSessionRole::Worker,
            agent_session_id: None,
            agent_message_id: None,
            state: WorkflowAgentSessionState::Running,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let (resume_session_id, reset_to_message_id) =
            resolve_workflow_resume(true, true, Some(&workflow_session), session_agent)
                .expect("changed config should not require an old runtime session");

        assert_eq!(resume_session_id, None);
        assert_eq!(reset_to_message_id, None);
    }

    #[test]
    fn unchanged_execution_config_preserves_workflow_follow_up_session() {
        let (mut session_agents, _) = sample_agent_views();
        let session_agent = &mut session_agents[0];
        session_agent.agent_session_id = Some("session-agent-fallback".to_string());
        let workflow_session = WorkflowAgentSession {
            id: Uuid::new_v4(),
            workflow_execution_id: Uuid::new_v4(),
            session_agent_id: session_agent.id,
            role: WorkflowAgentSessionRole::Worker,
            agent_session_id: Some("workflow-session".to_string()),
            agent_message_id: Some("workflow-message".to_string()),
            state: WorkflowAgentSessionState::Running,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let (resume_session_id, reset_to_message_id) =
            resolve_workflow_resume(true, false, Some(&workflow_session), session_agent)
                .expect("unchanged config should preserve follow-up session");

        assert_eq!(resume_session_id, Some("workflow-session"));
        assert_eq!(reset_to_message_id, Some("workflow-message"));
    }

    #[tokio::test]
    async fn common_member_mcp_preparation_returns_workflow_metadata() {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("test database");
        let workspace = tempfile::tempdir().expect("workspace");
        let (mut session_agents, mut agents) = sample_agent_views();
        let mut session_agent = session_agents.remove(0);
        session_agent.execution_config.0.mcp = Some(Default::default());
        let mut agent = agents.remove(0);
        agent.runner_type = "DEEPSEEK_HARNESS".to_string();
        let mut env = ExecutionEnv::new(
            RepoContext::new(workspace.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        );

        let (effective, _executor, prepared) = build_effective_member_executor_for_run(
            &pool,
            &agent,
            &session_agent,
            workspace.path(),
            Uuid::new_v4(),
            &mut env,
        )
        .await
        .expect("workflow preparation");

        assert_eq!(effective.runner_type.to_string(), "DEEPSEEK_HARNESS");
        assert_eq!(prepared.server_count(), 0);
    }

    #[tokio::test]
    async fn workflow_resolve_workspace_path_lazy_creates_worktree_for_first_isolated_run() {
        let db = setup_runtime_worktree_db().await;
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let base = tmp.path().join("base");
        init_git_repo(&base);
        let base_workspace = base.to_string_lossy().to_string();
        let session = sample_chat_session(
            ChatSessionWorktreeMode::Isolated,
            Some(base_workspace.clone()),
        );
        let (session_agents, agents) = sample_agent_views();
        let mut session_agent = session_agents[0].clone();
        session_agent.session_id = session.id;
        session_agent.workspace_path = Some(base_workspace.clone());
        let agent = &agents[0];

        let resolved = resolve_workspace_path(&db, &session, agent, &session_agent)
            .await
            .expect("resolve isolated workflow workspace");

        assert_ne!(resolved, base);
        assert!(resolved.exists());

        let active_worktree_path: String = sqlx::query_scalar(
            "SELECT worktree_path FROM chat_session_worktrees WHERE session_id = ?1 AND status = 'active'",
        )
        .bind(session.id)
        .fetch_one(&db.pool)
        .await
        .expect("active worktree row");
        assert_eq!(resolved.to_string_lossy(), active_worktree_path);

        SessionWorktreeService::new(db.pool.clone())
            .discard_worktree(session.id)
            .await
            .expect("discard test worktree");

        let after_discard = resolve_workspace_path(&db, &session, agent, &session_agent)
            .await
            .expect("resolve after discard");
        assert_eq!(after_discard, base);
    }

    #[tokio::test]
    async fn workflow_resolve_workspace_path_uses_existing_active_worktree() {
        let db = setup_runtime_worktree_db().await;
        let base_workspace = "E:/workspace/base";
        let worktree_workspace = "E:/workspace/.openteams/worktrees/session";
        let session = sample_chat_session(
            ChatSessionWorktreeMode::Isolated,
            Some(base_workspace.to_string()),
        );
        let (session_agents, agents) = sample_agent_views();
        let session_agent = &session_agents[0];
        let agent = &agents[0];

        sqlx::query(
            r#"
            INSERT INTO chat_session_worktrees (
                id, session_id, base_workspace_path, repo_path, base_branch,
                branch_name, worktree_path, mode, status
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'session', 'active')
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(session.id)
        .bind(base_workspace)
        .bind(base_workspace)
        .bind("main")
        .bind("openteams/session/test")
        .bind(worktree_workspace)
        .execute(&db.pool)
        .await
        .expect("insert active worktree");

        let resolved = resolve_workspace_path(&db, &session, agent, session_agent)
            .await
            .expect("resolve active worktree");

        assert_eq!(resolved, PathBuf::from(worktree_workspace));
    }

    #[tokio::test]
    async fn workflow_resolve_workspace_path_returns_base_after_terminal_worktree() {
        let db = setup_runtime_worktree_db().await;
        let base_workspace = "E:/workspace/base";
        let worktree_workspace = "E:/workspace/.openteams/worktrees/session";
        let session = sample_chat_session(
            ChatSessionWorktreeMode::Isolated,
            Some(base_workspace.to_string()),
        );
        let (session_agents, agents) = sample_agent_views();
        let session_agent = &session_agents[0];
        let agent = &agents[0];

        sqlx::query(
            r#"
            INSERT INTO chat_session_worktrees (
                id, session_id, base_workspace_path, repo_path, base_branch,
                branch_name, worktree_path, mode, status, archived_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'session', 'archived', datetime('now', 'subsec'))
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(session.id)
        .bind(base_workspace)
        .bind(base_workspace)
        .bind("main")
        .bind("openteams/session/test")
        .bind(worktree_workspace)
        .execute(&db.pool)
        .await
        .expect("insert archived worktree");

        let resolved = resolve_workspace_path(&db, &session, agent, session_agent)
            .await
            .expect("resolve archived worktree");

        assert_eq!(resolved, PathBuf::from(base_workspace));
    }

    fn sample_step_review(step: &WorkflowStep) -> WorkflowStepReview {
        WorkflowStepReview {
            id: Uuid::new_v4(),
            step_id: step.id,
            execution_id: step.execution_id,
            reviewer_type: db::models::workflow_types::ReviewerType::Lead,
            reviewer_id: Some(Uuid::new_v4().to_string()),
            verdict: ReviewVerdict::Approved,
            feedback: "Looks good".to_string(),
            review_round: 1,
            created_at: Utc::now(),
        }
    }

    fn sample_step_review_transcript(step: &WorkflowStep) -> WorkflowTranscript {
        WorkflowTranscript {
            id: Uuid::new_v4(),
            execution_id: step.execution_id,
            round_id: Some(step.round_id),
            workflow_agent_session_id: Some(Uuid::new_v4()),
            step_id: Some(step.id),
            sender_type: "control".to_string(),
            entry_type: "step_review".to_string(),
            content: format!("请审核步骤「{}」的执行结果", step.title),
            meta_json: Some(
                serde_json::json!({
                    "summary": "Need user confirmation",
                    "resolved": false,
                })
                .to_string(),
            ),
            created_at: Utc::now().to_rfc3339(),
        }
    }

    #[test]
    fn planning_agent_descriptors_distinguish_members_sharing_one_underlying_agent() {
        use executors::executors::BaseCodingAgent;

        let shared_agent_id = Uuid::new_v4();
        let agent = ChatAgent {
            id: shared_agent_id,
            name: "shared-agent".to_string(),
            runner_type: "codex".to_string(),
            system_prompt: "You are a polyglot engineer.\n\n  You own delivery quality.".to_string(),
            tools_enabled: Json(serde_json::json!({
                "mcpServers": { "filesystem": true, "browser": { "enabled": false } }
            })),
            model_name: Some("gpt-5-codex".to_string()),
            owner_project_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let session_id = Uuid::new_v4();
        let make_member = |name: &str, config: MemberExecutionConfig, skills: Vec<String>| {
            ChatSessionAgent {
                id: Uuid::new_v4(),
                session_id,
                agent_id: shared_agent_id,
                state: ChatSessionAgentState::Idle,
                workspace_path: None,
                pty_session_key: None,
                agent_session_id: None,
                agent_message_id: None,
                project_member_id: None,
                member_name: name.to_string(),
                execution_config: Json(config),
                allowed_skill_ids: Json(skills),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            }
        };

        // Two session members backed by the same underlying agent, with
        // different member roles, runner/model overrides, and allowed skills.
        let lead_member = make_member(
            "Planner",
            MemberExecutionConfig {
                runner_type: Some(BaseCodingAgent::ClaudeCode),
                model_name: Some("claude-sonnet-4".to_string()),
                ..Default::default()
            },
            vec!["skill-plan".to_string()],
        );
        let worker_member = make_member(
            "Implementer",
            MemberExecutionConfig::default(),
            vec!["skill-code".to_string()],
        );

        let lead_effective = crate::services::member_execution::resolve_effective_member_execution_config(&agent, &lead_member)
            .expect("resolve lead effective config");
        let worker_effective = crate::services::member_execution::resolve_effective_member_execution_config(&agent, &worker_member)
            .expect("resolve worker effective config");

        // Enabled native skills differ per effective runner.
        let lead_runner_skills = vec![
            ("skill-plan".to_string(), "writing-plans".to_string()),
            ("skill-code".to_string(), "code-guidelines".to_string()),
        ];
        let worker_runner_skills = vec![("skill-code".to_string(), "code-guidelines".to_string())];

        let lead = compose_workflow_planning_agent(
            &lead_member,
            &agent,
            &lead_effective,
            true,
            Some("技术负责人".to_string()),
            &lead_runner_skills,
        );
        let worker = compose_workflow_planning_agent(
            &worker_member,
            &agent,
            &worker_effective,
            false,
            Some("后端工程师".to_string()),
            &worker_runner_skills,
        );

        // Session-member planning ids stay unique and never collapse onto the
        // shared underlying agent id.
        assert_eq!(lead.agent_id, lead_member.id.to_string());
        assert_eq!(worker.agent_id, worker_member.id.to_string());
        assert_ne!(lead.agent_id, worker.agent_id);
        assert_eq!(lead.underlying_agent_id, shared_agent_id.to_string());
        assert_eq!(worker.underlying_agent_id, shared_agent_id.to_string());

        // Workflow duty and declared member role stay separate.
        assert_eq!(lead.workflow_role, "lead");
        assert_eq!(worker.workflow_role, "worker");
        assert_eq!(lead.member_role.as_deref(), Some("技术负责人"));
        assert_eq!(worker.member_role.as_deref(), Some("后端工程师"));

        // Effective runner/model honor the member execution config override.
        assert_eq!(lead.runner_type, BaseCodingAgent::ClaudeCode.to_string());
        assert_eq!(lead.model_name.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(worker.runner_type, BaseCodingAgent::Codex.to_string());
        assert_eq!(worker.model_name.as_deref(), Some("gpt-5-codex"));

        // Skills come from the effective runner's enabled set intersected
        // with the member's allowed skill ids — no cross-member leakage.
        assert_eq!(lead.skills, vec!["writing-plans".to_string()]);
        assert_eq!(worker.skills, vec!["code-guidelines".to_string()]);

        // Capability profile is sourced from the shared system prompt
        // (whitespace-normalized); tools reflect actual enablement.
        let capability = lead.capability_profile.expect("capability profile");
        assert!(capability.contains("polyglot engineer"));
        assert!(!capability.contains('\n'));
        assert_eq!(lead.tools_enabled, vec!["mcp:filesystem".to_string()]);
        assert_eq!(worker.tools_enabled, vec!["mcp:filesystem".to_string()]);
    }

    #[test]
    fn capability_profile_from_system_prompt_is_length_capped() {
        let long_prompt = "word ".repeat(1000);
        let profile = capability_profile_from_system_prompt(&long_prompt).expect("profile");
        assert!(profile.chars().count() <= CAPABILITY_PROFILE_MAX_CHARS);
        assert!(profile.ends_with('…'));
        assert!(capability_profile_from_system_prompt("  \n\t  ").is_none());
    }

    struct PlanningRolesFixture {
        pool: SqlitePool,
        project: db::models::project::Project,
        agent: ChatAgent,
        backend_member: db::models::project_member::ProjectMember,
        frontend_member: db::models::project_member::ProjectMember,
    }

    /// Builds a project with two agent project members sharing one underlying
    /// ChatAgent, each with a distinct declared role.
    async fn setup_planning_roles_fixture() -> PlanningRolesFixture {
        use db::models::{
            chat_agent::CreateChatAgent,
            project::{CreateProject, Project},
            project_member::{ProjectMember, ProjectMemberType},
        };

        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create sqlite memory pool");
        sqlx::migrate!("../db/migrations")
            .run(&pool)
            .await
            .expect("run migrations");

        let project = Project::create(
            &pool,
            &CreateProject {
                name: "roles-project".to_string(),
                repositories: vec![],
                description: None,
                status: None,
                default_workspace_path: None,
                active_repo_id: None,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create project");
        let agent = ChatAgent::create(
            &pool,
            &CreateChatAgent {
                name: "shared-agent".to_string(),
                runner_type: "codex".to_string(),
                system_prompt: None,
                tools_enabled: None,
                model_name: None,
                owner_project_id: None,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create shared agent");

        let backend_member = ProjectMember::create(
            &pool,
            project.id,
            ProjectMemberType::Agent,
            None,
            Some(agent.id),
            Some("Backend".to_string()),
            Some("后端工程师".to_string()),
            0,
            None,
            vec![],
            MemberExecutionConfig::default(),
            false,
        )
        .await
        .expect("create backend project member");
        let frontend_member = ProjectMember::create(
            &pool,
            project.id,
            ProjectMemberType::Agent,
            None,
            Some(agent.id),
            Some("Frontend".to_string()),
            Some("前端工程师".to_string()),
            1,
            None,
            vec![],
            MemberExecutionConfig::default(),
            false,
        )
        .await
        .expect("create frontend project member");

        PlanningRolesFixture {
            pool,
            project,
            agent,
            backend_member,
            frontend_member,
        }
    }

    async fn add_session_member(
        fixture: &PlanningRolesFixture,
        session: &ChatSession,
        name: &str,
        project_member_id: Option<Uuid>,
    ) -> ChatSessionAgent {
        use db::models::chat_session_agent::CreateChatSessionAgent;

        ChatSessionAgent::create(
            &fixture.pool,
            &CreateChatSessionAgent {
                session_id: session.id,
                agent_id: fixture.agent.id,
                member_name: Some(name.to_string()),
                workspace_path: None,
                allowed_skill_ids: vec![],
                project_member_id,
                execution_config: MemberExecutionConfig {
                    mcp: Some(Default::default()),
                    ..Default::default()
                },
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create session member")
    }

    async fn create_session(
        fixture: &PlanningRolesFixture,
        project_id: Option<Uuid>,
    ) -> ChatSession {
        use db::models::chat_session::CreateChatSession;

        ChatSession::create(
            &fixture.pool,
            &CreateChatSession {
                title: None,
                workspace_path: None,
                project_id,
                worktree_mode: None,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create chat session")
    }

    #[tokio::test]
    async fn planning_member_roles_separate_shared_agent_members_via_explicit_links() {
        let fixture = setup_planning_roles_fixture().await;
        let session = create_session(&fixture, Some(fixture.project.id)).await;
        // Two session members reuse the same underlying agent but link to
        // different project members.
        let backend = add_session_member(&fixture, &session, "Backend", Some(fixture.backend_member.id)).await;
        let frontend = add_session_member(&fixture, &session, "Frontend", Some(fixture.frontend_member.id)).await;

        let roles = resolve_planning_member_roles(&fixture.pool, &session, &[backend.clone(), frontend.clone()])
            .await
            .expect("resolve member roles");

        assert_eq!(
            roles.get(&backend.id).map(String::as_str),
            Some("后端工程师")
        );
        assert_eq!(
            roles.get(&frontend.id).map(String::as_str),
            Some("前端工程师")
        );
    }

    #[tokio::test]
    async fn planning_member_roles_never_guess_on_ambiguous_or_invalid_links() {
        let fixture = setup_planning_roles_fixture().await;
        let session = create_session(&fixture, Some(fixture.project.id)).await;

        // Unlinked members: two project members share the underlying agent,
        // so no role may be assigned to either session member.
        let unlinked_one = add_session_member(&fixture, &session, "MemberOne", None).await;
        let unlinked_two = add_session_member(&fixture, &session, "MemberTwo", None).await;
        let roles = resolve_planning_member_roles(
            &fixture.pool,
            &session,
            &[unlinked_one.clone(), unlinked_two.clone()],
        )
        .await
        .expect("resolve member roles");
        assert!(!roles.contains_key(&unlinked_one.id));
        assert!(!roles.contains_key(&unlinked_two.id));

        // A link pointing at a member of a different project is rejected.
        let foreign_project = db::models::project::Project::create(
            &fixture.pool,
            &db::models::project::CreateProject {
                name: "foreign-project".to_string(),
                repositories: vec![],
                description: None,
                status: None,
                default_workspace_path: None,
                active_repo_id: None,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create foreign project");
        let foreign_member = db::models::project_member::ProjectMember::create(
            &fixture.pool,
            foreign_project.id,
            db::models::project_member::ProjectMemberType::Agent,
            None,
            Some(fixture.agent.id),
            Some("Foreign".to_string()),
            Some("外部角色".to_string()),
            0,
            None,
            vec![],
            MemberExecutionConfig::default(),
            false,
        )
        .await
        .expect("create foreign project member");
        let cross_linked =
            add_session_member(&fixture, &session, "CrossLinked", Some(foreign_member.id)).await;
        let roles =
            resolve_planning_member_roles(&fixture.pool, &session, std::slice::from_ref(&cross_linked))
                .await
                .expect("resolve member roles");
        assert!(!roles.contains_key(&cross_linked.id));
    }

    #[tokio::test]
    async fn planning_member_roles_fall_back_only_on_unique_project_match() {
        use db::models::project_member::{ProjectMember, ProjectMemberType};

        let fixture = setup_planning_roles_fixture().await;
        // A second project where exactly one project member matches the
        // shared underlying agent.
        let solo_project = db::models::project::Project::create(
            &fixture.pool,
            &db::models::project::CreateProject {
                name: "solo-project".to_string(),
                repositories: vec![],
                description: None,
                status: None,
                default_workspace_path: None,
                active_repo_id: None,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create solo project");
        ProjectMember::create(
            &fixture.pool,
            solo_project.id,
            ProjectMemberType::Agent,
            None,
            Some(fixture.agent.id),
            Some("Solo".to_string()),
            Some("独立工程师".to_string()),
            0,
            None,
            vec![],
            MemberExecutionConfig::default(),
            false,
        )
        .await
        .expect("create solo project member");
        let solo_session = create_session(&fixture, Some(solo_project.id)).await;
        let solo_member = add_session_member(&fixture, &solo_session, "Solo", None).await;

        let roles = resolve_planning_member_roles(
            &fixture.pool,
            &solo_session,
            std::slice::from_ref(&solo_member),
        )
        .await
        .expect("resolve member roles");
        assert_eq!(
            roles.get(&solo_member.id).map(String::as_str),
            Some("独立工程师")
        );
    }

    #[test]
    fn workflow_response_language_instruction_follows_ui_language() {
        assert_eq!(
            resolve_workflow_response_language_instruction(&UiLanguage::ZhHans),
            "You MUST write human-readable JSON string values in Simplified Chinese."
        );
        assert_eq!(
            resolve_workflow_response_language_instruction(&UiLanguage::En),
            "You MUST write human-readable JSON string values in English."
        );
    }

    #[test]
    fn workflow_review_attempt_limit_uses_persisted_budget() {
        assert!(!workflow_review_attempt_limit_reached(4, 5));
        assert!(workflow_review_attempt_limit_reached(5, 5));
        assert!(workflow_review_attempt_limit_reached(6, 5));
    }

    #[test]
    fn build_workspace_scoped_workflow_prompt_declares_active_workspace_as_project_repo() {
        let workspace_path = Path::new(
            r"C:\Users\Admin\AppData\Local\Temp\openteams-dev\worktrees\sessions\34a8ed29",
        );

        let prompt =
            build_workspace_scoped_workflow_prompt("Run the workflow step.", workspace_path);

        assert!(prompt.starts_with("[OPENTEAMS_SOURCE=openteams]\n\n"));
        assert_eq!(prompt.matches("[OPENTEAMS_SOURCE=openteams]").count(), 1);
        assert!(prompt.contains("## Workspace"));
        assert!(prompt.contains("Active workspace path"));
        assert!(prompt.contains("Treat this active workspace path as the project repository"));
        assert!(prompt.contains(
            r"C:\Users\Admin\AppData\Local\Temp\openteams-dev\worktrees\sessions\34a8ed29"
        ));
        assert!(prompt.ends_with("Run the workflow step."));
    }

    #[tokio::test]
    async fn workflow_prompt_after_worktree_discard_declares_base_workspace() {
        let db = setup_runtime_worktree_db().await;
        let base_workspace = "E:/workspace/base";
        let worktree_workspace = "E:/workspace/.openteams/worktrees/session";
        let session = sample_chat_session(
            ChatSessionWorktreeMode::Isolated,
            Some(base_workspace.to_string()),
        );
        let (session_agents, agents) = sample_agent_views();
        let mut session_agent = session_agents[0].clone();
        session_agent.session_id = session.id;
        session_agent.workspace_path = Some(worktree_workspace.to_string());
        let agent = &agents[0];

        sqlx::query(
            r#"
            INSERT INTO chat_session_agents (
                id, session_id, agent_id, state, workspace_path
            )
            VALUES (?1, ?2, ?3, 'idle', ?4)
            "#,
        )
        .bind(session_agent.id)
        .bind(session.id)
        .bind(agent.id)
        .bind(worktree_workspace)
        .execute(&db.pool)
        .await
        .expect("insert workflow session agent");

        sqlx::query(
            r#"
            INSERT INTO chat_session_worktrees (
                id, session_id, base_workspace_path, repo_path, base_branch,
                branch_name, worktree_path, mode, status, merged_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'session', 'merged', datetime('now', 'subsec'))
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(session.id)
        .bind(base_workspace)
        .bind(base_workspace)
        .bind("main")
        .bind("openteams/session/test")
        .bind(worktree_workspace)
        .execute(&db.pool)
        .await
        .expect("insert merged worktree");

        SessionWorktreeService::new(db.pool.clone())
            .discard_worktree(session.id)
            .await
            .expect("discard worktree");

        let resolved = resolve_workspace_path(&db, &session, agent, &session_agent)
            .await
            .expect("resolve workflow workspace");
        assert_eq!(resolved, PathBuf::from(base_workspace));

        let prompt = build_workspace_scoped_workflow_prompt("Run the workflow step.", &resolved);

        assert!(prompt.contains("Active workspace path: `E:/workspace/base`"));
        assert!(!prompt.contains(worktree_workspace));
    }

    #[test]
    fn wire_value_consistency_for_step_type() {
        assert_eq!(to_workflow_wire_value(&WorkflowStepType::Task), "task");
        assert_eq!(to_workflow_wire_value(&WorkflowStepType::Review), "review");
        assert_eq!(to_workflow_wire_value(&WorkflowStepType::Result), "result");
    }

    #[test]
    fn wire_value_consistency_for_execution_status() {
        assert_eq!(
            to_workflow_wire_value(&WorkflowExecutionStatus::Running),
            "running"
        );
    }

    #[test]
    fn parse_step_review_protocol_output_accepts_approved_review() {
        let step = sample_step(WorkflowStepStatus::WaitingReview);
        let criteria = build_workflow_review_criteria(
            &[(AcceptanceCriterionLevel::Required, "验收标准".to_string())],
            None,
        );
        let raw_output = format!(
            r#"{{
  "type": "review_result",
  "step_key": "{}",
  "execution_id": "{}",
  "summary": "结果满足验收标准。",
  "results": {{ "c1": {{ "passed": true, "evidence": "cargo test passed" }} }}
}}"#,
            step.step_key, step.execution_id
        );

        let message = parse_step_review_protocol_output(
            step.execution_id,
            &step.step_key,
            &criteria,
            &raw_output,
        )
        .expect("parse");

        assert_eq!(
            message,
            WorkflowReviewProtocolMessage::ReviewResult {
                step_key: step.step_key,
                execution_id: step.execution_id.to_string(),
                summary: "结果满足验收标准。".to_string(),
                results: std::collections::BTreeMap::from([(
                    "c1".to_string(),
                    WorkflowReviewCriterionResult {
                        passed: true,
                        evidence: "cargo test passed".to_string(),
                    },
                )]),
            }
        );
    }

    #[test]
    fn parse_step_review_protocol_output_accepts_rejected_review() {
        let step = sample_step(WorkflowStepStatus::WaitingReview);
        let criteria = build_workflow_review_criteria(
            &[
                (AcceptanceCriterionLevel::Required, "回归测试".to_string()),
                (AcceptanceCriterionLevel::Partial, "环境服务".to_string()),
            ],
            None,
        );
        let raw_output = format!(
            r#"{{
  "type": "review_result",
  "step_key": "{}",
  "execution_id": "{}",
  "summary": "还缺少回归测试。",
  "results": {{
    "c1": {{ "passed": false, "evidence": "no test output" }},
    "c2": {{ "passed": false, "evidence": "service unavailable" }}
  }}
}}"#,
            step.step_key, step.execution_id
        );

        let message = parse_step_review_protocol_output(
            step.execution_id,
            &step.step_key,
            &criteria,
            &raw_output,
        )
        .expect("review covers all declared criteria");
        let WorkflowReviewProtocolMessage::ReviewResult { results, .. } = message;
        let derived = derive_workflow_review(&criteria, &results);
        assert_eq!(derived.verdict, ReviewVerdict::Rejected);
        assert_eq!(derived.risks, vec!["service unavailable"]);
        assert_eq!(derived.unfinished_items, vec!["回归测试"]);
    }

    #[test]
    fn parse_step_review_protocol_output_rejects_invalid_review_payload() {
        let step = sample_step(WorkflowStepStatus::WaitingReview);
        let raw_output = format!(
            r#"{{
  "type": "review_result",
  "step_key": "{}",
  "execution_id": "{}",
  "summary": "   ",
  "results": {{ "c1": {{ "passed": true, "evidence": "cargo test passed" }} }}
}}"#,
            step.step_key, step.execution_id
        );

        let criteria = build_workflow_review_criteria(
            &[(AcceptanceCriterionLevel::Required, "验收标准".to_string())],
            None,
        );
        let err = parse_step_review_protocol_output(
            step.execution_id,
            &step.step_key,
            &criteria,
            &raw_output,
        )
        .expect_err("invalid");

        assert!(matches!(err, WorkflowRuntimeError::Validation(_)));
    }

    #[test]
    fn step_review_schema_and_parser_share_the_exact_contract() {
        let execution_id = Uuid::new_v4();
        let criteria = build_workflow_review_criteria(
            &[(AcceptanceCriterionLevel::Required, "required work".to_string())],
            None,
        );
        let schema: serde_json::Value = serde_json::from_str(&step_review_protocol_json_schema(
            execution_id,
            "review",
            &criteria,
        ))
        .unwrap();
        assert_eq!(
            schema["properties"]["results"]["required"],
            serde_json::json!(["c1"])
        );
        assert!(schema["properties"].get("verdict").is_none());

        let raw = format!(
            "```json\n{{\"type\":\"review_result\",\"step_key\":\"review\",\"execution_id\":\"{execution_id}\",\"summary\":\"reviewed\",\"results\":{{\"c1\":{{\"passed\":true,\"evidence\":\"checked\"}}}}}}\n```"
        );
        assert!(
            parse_step_review_protocol_output(execution_id, "review", &criteria, &raw).is_err()
        );
    }

    #[test]
    fn task_protocol_requires_structured_status_and_evidence() {
        let execution_id = Uuid::new_v4();
        let raw = format!(
            r#"{{"type":"final_result","step_key":"task","execution_id":"{execution_id}","summary":"done","content":"done","outputs":[]}}"#
        );
        assert!(parse_task_protocol_output(execution_id, "task", &raw).is_err());
    }

    #[test]
    fn task_protocol_preserves_blocked_as_a_typed_status() {
        let execution_id = Uuid::new_v4();
        let raw = format!(
            r#"{{"type":"final_result","step_key":"task","execution_id":"{execution_id}","status":"blocked","summary":"blocked","content":"cannot continue","verification":[{{"name":"dependency check","command":null,"status":"not_run","evidence":"credential missing"}}],"files_changed":[],"self_review":["scope checked"],"issues":["credential missing"],"evidence":["dependency check"],"outputs":[]}}"#
        );
        assert!(matches!(
            parse_task_protocol_output(execution_id, "task", &raw)
            .expect("blocked task result"),
            WorkflowStepProtocolMessage::FinalResult {
                status: WorkflowTaskCompletionStatus::Blocked,
                ..
            }
        ));
    }

    #[test]
    fn approved_review_rejects_missing_required_acceptance_results() {
        let execution_id = Uuid::new_v4();
        let criteria = build_workflow_review_criteria(
            &[
                (AcceptanceCriterionLevel::Required, "first".to_string()),
                (AcceptanceCriterionLevel::Required, "second".to_string()),
                (AcceptanceCriterionLevel::Partial, "optional".to_string()),
            ],
            None,
        );
        let raw = format!(
            r#"{{"type":"review_result","step_key":"review","execution_id":"{execution_id}","summary":"reviewed","results":{{"c1":{{"passed":true,"evidence":"checked"}}}}}}"#
        );
        assert!(parse_step_review_protocol_output(
            execution_id,
            "review",
            &criteria,
            &raw,
        )
        .is_err());
    }

    #[test]
    fn partial_failure_is_derived_as_approved_with_risk() {
        let execution_id = Uuid::new_v4();
        let criteria = build_workflow_review_criteria(
            &[
                (AcceptanceCriterionLevel::Required, "required work".to_string()),
                (AcceptanceCriterionLevel::Partial, "环境依赖".to_string()),
            ],
            None,
        );
        let raw = format!(
            r#"{{"type":"review_result","step_key":"review","execution_id":"{execution_id}","summary":"reviewed","results":{{"c1":{{"passed":true,"evidence":"checked"}},"c2":{{"passed":false,"evidence":"service unavailable"}}}}}}"#
        );
        let message = parse_step_review_protocol_output(
            execution_id,
            "review",
            &criteria,
            &raw,
        )
        .unwrap();
        let WorkflowReviewProtocolMessage::ReviewResult { results, .. } = message;
        let derived = derive_workflow_review(&criteria, &results);
        assert_eq!(derived.verdict, ReviewVerdict::Approved);
        assert_eq!(derived.risks, vec!["service unavailable"]);
    }

    #[test]
    fn review_without_declared_acceptance_uses_instruction_fallback() {
        let criteria = build_workflow_review_criteria(&[], Some("检查整体交付"));
        assert_eq!(criteria.len(), 1);
        assert_eq!(criteria[0].id, "c1");
        assert_eq!(criteria[0].level, AcceptanceCriterionLevel::Required);
        assert_eq!(criteria[0].criterion, "检查整体交付");
    }

    #[test]
    fn recommended_failure_does_not_reject_review() {
        let execution_id = Uuid::new_v4();
        let criteria = build_workflow_review_criteria(
            &[
                (AcceptanceCriterionLevel::Required, "required work".to_string()),
                (AcceptanceCriterionLevel::Recommended, "attach screenshot".to_string()),
            ],
            None,
        );
        let raw = format!(
            r#"{{"type":"review_result","step_key":"review","execution_id":"{execution_id}","summary":"reviewed","results":{{"c1":{{"passed":true,"evidence":"checked"}},"c2":{{"passed":false,"evidence":"no screenshot"}}}}}}"#
        );
        let message = parse_step_review_protocol_output(
            execution_id,
            "review",
            &criteria,
            &raw,
        )
        .unwrap();
        let WorkflowReviewProtocolMessage::ReviewResult { results, .. } = message;
        assert_eq!(
            derive_workflow_review(&criteria, &results).verdict,
            ReviewVerdict::Approved
        );
    }

    #[test]
    fn review_protocol_rejects_extra_fields() {
        let execution_id = Uuid::new_v4();
        let criteria = build_workflow_review_criteria(
            &[(AcceptanceCriterionLevel::Required, "required work".to_string())],
            None,
        );
        let raw = format!(
            r#"{{"type":"review_result","step_key":"review","execution_id":"{execution_id}","summary":"reviewed","results":{{"c1":{{"passed":true,"evidence":"checked","verdict":"passed"}}}}}}"#
        );
        assert!(parse_step_review_protocol_output(
            execution_id,
            "review",
            &criteria,
            &raw,
        )
        .is_err());
    }

    #[test]
    fn review_protocol_rejects_blank_evidence() {
        let execution_id = Uuid::new_v4();
        let criteria = build_workflow_review_criteria(
            &[(AcceptanceCriterionLevel::Required, "required work".to_string())],
            None,
        );
        let raw = format!(
            r#"{{"type":"review_result","step_key":"review","execution_id":"{execution_id}","summary":"reviewed","results":{{"c1":{{"passed":true,"evidence":" "}}}}}}"#
        );
        assert!(parse_step_review_protocol_output(
            execution_id,
            "review",
            &criteria,
            &raw,
        )
        .is_err());
    }

    #[test]
    fn parse_task_protocol_output_accepts_approval_request() {
        let execution_id = Uuid::new_v4();
        let step_key = "review";
        let raw_output = format!(
            r#"{{
  "type": "approval_request",
  "step_key": "{step_key}",
  "execution_id": "{execution_id}",
  "title": "Need approval",
  "description": "Please confirm the patch."
}}"#
        );

        let message =
            parse_task_protocol_output(execution_id, step_key, &raw_output).expect("parse");

        match message {
            WorkflowStepProtocolMessage::ApprovalRequest {
                title, description, ..
            } => {
                assert_eq!(title, "Need approval");
                assert_eq!(description.as_deref(), Some("Please confirm the patch."));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn parse_task_protocol_output_accepts_continue_confirmation() {
        let execution_id = Uuid::new_v4();
        let step_key = "review";
        let raw_output = format!(
            r#"{{
  "type": "continue_confirmation",
  "step_key": "{step_key}",
  "execution_id": "{execution_id}",
  "message": "Continue with deployment?"
}}"#
        );

        let message =
            parse_task_protocol_output(execution_id, step_key, &raw_output).expect("parse");

        match message {
            WorkflowStepProtocolMessage::ContinueConfirmation { message, .. } => {
                assert_eq!(message, "Continue with deployment?");
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn parse_task_protocol_output_accepts_input_request() {
        let execution_id = Uuid::new_v4();
        let step_key = "clarify";
        let raw_output = format!(
            r#"{{
  "type": "input_request",
  "step_key": "{step_key}",
  "execution_id": "{execution_id}",
  "prompt": "Please provide the release tag",
  "placeholder": "v1.2.3"
}}"#
        );

        let message =
            parse_task_protocol_output(execution_id, step_key, &raw_output).expect("parse");

        match message {
            WorkflowStepProtocolMessage::InputRequest {
                prompt,
                placeholder,
                ..
            } => {
                assert_eq!(prompt, "Please provide the release tag");
                assert_eq!(placeholder.as_deref(), Some("v1.2.3"));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn parse_task_protocol_output_rejects_wrong_execution_id() {
        let execution_id = Uuid::new_v4();
        let raw_output = format!(
            r#"{{
  "type": "permission_request",
  "step_key": "review",
  "execution_id": "{}",
  "title": "Need permission"
}}"#,
            Uuid::new_v4()
        );

        let err = parse_task_protocol_output(execution_id, "review", &raw_output)
            .expect_err("invalid");

        assert!(matches!(err, WorkflowRuntimeError::Validation(_)));
    }

    #[test]
    fn workflow_runtime_line_keeps_assistant_for_final_protocol_only() {
        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::AssistantMessage,
            content: r#"{"type":"final_result","summary":"done"}"#.to_string(),
            metadata: None,
        };

        assert!(workflow_runtime_line_for_entry(&entry).is_none());
    }

    #[test]
    fn workflow_executor_failure_prefers_error_lines_from_stderr() {
        let history = vec![
            LogMsg::Stdout("normal progress\nmore normal progress\n".to_string()),
            LogMsg::Stderr(
                "debug detail that should not be surfaced\nERROR: model overloaded\n".to_string(),
            ),
        ];

        let message = workflow_executor_failure_message(
            "codex",
            WorkflowRuntimeErrorCode::ExecutionFailed,
            &history,
        );
        let (payload, detail) = message
            .strip_prefix(WORKFLOW_RUNTIME_ERROR_PREFIX)
            .expect("runtime error prefix")
            .split_once(WORKFLOW_RUNTIME_ERROR_DETAIL_PREFIX)
            .expect("executor detail");
        let payload: serde_json::Value = serde_json::from_str(payload).expect("runtime payload");

        assert_eq!(payload["code"], "execution_failed");
        assert_eq!(payload["agent_name"], "codex");
        assert!(detail.trim().contains("ERROR: model overloaded"));
        assert!(!detail.contains("debug detail that should not be surfaced"));
    }

    #[test]
    fn workflow_executor_failure_extracts_structured_json_error() {
        let history = vec![LogMsg::Stdout(
            serde_json::json!({
                "type": "error",
                "error": {
                    "message": "Gemini API key is invalid",
                    "debug": "large payload omitted"
                }
            })
            .to_string(),
        )];

        let message = workflow_executor_failure_message(
            "gemini",
            WorkflowRuntimeErrorCode::ExecutionFailed,
            &history,
        );

        assert!(message.contains("Gemini API key is invalid"));
        assert!(!message.contains("large payload omitted"));
    }

    #[test]
    fn workflow_executor_signal_failure_uses_authoritative_reason_without_log_excerpt() {
        let history = vec![LogMsg::Stderr(
            "unrelated debug log\nERROR: stale provider error\n".to_string(),
        )];

        let message = workflow_executor_signal_failure_message(
            "backend",
            Some("OpenTeamsCli request timed out after 2400s without session activity"),
            &history,
        );

        let (payload, detail) = message
            .strip_prefix(WORKFLOW_RUNTIME_ERROR_PREFIX)
            .expect("runtime error prefix")
            .split_once(WORKFLOW_RUNTIME_ERROR_DETAIL_PREFIX)
            .expect("executor detail");
        let payload: serde_json::Value = serde_json::from_str(payload).expect("runtime payload");

        assert_eq!(payload["code"], "execution_failed");
        assert_eq!(payload["agent_name"], "backend");
        assert_eq!(
            detail.trim(),
            "OpenTeamsCli request timed out after 2400s without session activity"
        );
        assert!(!message.contains("stale provider error"));
    }

    #[test]
    fn workflow_runtime_inactivity_error_uses_localizable_payload() {
        let message = workflow_runtime_error_message(
            WorkflowRuntimeErrorCode::SessionInactivityTimeout,
            Some("opencode"),
            Some(40),
            None,
        );
        let payload = message
            .strip_prefix(WORKFLOW_RUNTIME_ERROR_PREFIX)
            .expect("runtime error prefix");
        let payload: serde_json::Value = serde_json::from_str(payload).expect("runtime payload");

        assert_eq!(payload["code"], "session_inactivity_timeout");
        assert_eq!(payload["agent_name"], "opencode");
        assert_eq!(payload["inactivity_minutes"], 40);
    }

    #[test]
    fn cancel_running_step_is_scoped_to_the_interrupted_attempt() {
        let step_id = Uuid::new_v4();
        clear_running_step(step_id, 0);
        clear_running_step(step_id, 1);

        cancel_running_step(step_id, 0);

        let token = executors::executors::CancellationToken::new();
        register_running_step(step_id, 0, token.clone());
        assert!(token.is_cancelled());

        let next_token = executors::executors::CancellationToken::new();
        register_running_step(step_id, 1, next_token.clone());
        assert!(!next_token.is_cancelled());

        clear_running_step(step_id, 0);
        assert!(!next_token.is_cancelled());
        clear_running_step(step_id, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workflow_executor_wait_observes_child_exit_before_sdk_signal() {
        use command_group::AsyncCommandGroup;

        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "exit 17"]);
        let mut child = command.group_spawn().expect("spawn test child");
        let (_signal_tx, mut signal_rx) = tokio::sync::oneshot::channel();
        let msg_store = MsgStore::new();

        let event =
            wait_for_executor_exit_or_cancel(&mut child, &mut signal_rx, None, &msg_store)
            .await
            .expect("wait for child exit");

        match event {
            ExecutorWaitEvent::ProcessExited(Ok(status)) => {
                assert_eq!(status.code(), Some(17));
            }
            other => panic!("expected child exit, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workflow_executor_wait_times_out_after_session_inactivity() {
        use command_group::AsyncCommandGroup;

        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let mut child = command.group_spawn().expect("spawn test child");
        let (_signal_tx, mut signal_rx) = tokio::sync::oneshot::channel();
        let msg_store = MsgStore::new();

        let result = wait_for_executor_exit_or_cancel_with_inactivity_timeout(
            &mut child,
            &mut signal_rx,
            None,
            &msg_store,
            Duration::from_millis(25),
        )
        .await;

        assert!(matches!(result, Err(SessionInactivityTimeout)));
        let _ = child.kill().await;
    }

    #[test]
    fn workflow_runtime_line_maps_reasoning_to_thinking() {
        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::Thinking,
            content: "Checking the workflow state machine".to_string(),
            metadata: None,
        };

        let line = workflow_runtime_line_for_entry(&entry).expect("thinking line");

        assert!(matches!(line.stream_type, ChatStreamDeltaType::Thinking));
        assert_eq!(line.content, "Checking the workflow state machine");
        assert!(!line.immediate);
    }

    #[test]
    fn workflow_runtime_line_maps_file_edit_activity_to_thinking() {
        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: "edit".to_string(),
                action_type: ActionType::FileEdit {
                    path: "frontend/src/pages/ui-new/chat/components/WorkflowWindow.tsx"
                        .to_string(),
                    changes: vec![FileChange::Edit {
                        unified_diff: "@@ -1 +1 @@\n-old\n+new\n".to_string(),
                        has_line_numbers: true,
                    }],
                },
                status: ToolStatus::Created,
            },
            content: "WorkflowWindow.tsx".to_string(),
            metadata: None,
        };

        let line = workflow_runtime_line_for_entry(&entry).expect("file edit line");

        assert!(matches!(line.stream_type, ChatStreamDeltaType::Thinking));
        assert!(line.immediate);
        assert!(line.content.contains("Started file edit"));
        assert!(line.content.contains("WorkflowWindow.tsx"));
        assert!(line.content.contains("1 edit"));
    }

    #[test]
    fn workflow_runtime_line_maps_mcp_progress_to_thinking_preview() {
        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: "mcp:github:search_issues".to_string(),
                action_type: ActionType::Tool {
                    tool_name: "github.search_issues".to_string(),
                    arguments: None,
                    result: Some(ToolResult::markdown(
                        "Fetched 3 matching issues\nmore detail",
                    )),
                },
                status: ToolStatus::Created,
            },
            content: "search_issues".to_string(),
            metadata: None,
        };

        let line = workflow_runtime_line_for_entry(&entry).expect("mcp progress line");

        assert!(matches!(line.stream_type, ChatStreamDeltaType::Thinking));
        assert!(line.immediate);
        assert_eq!(
            line.content,
            "Started MCP tool: github.search_issues: Fetched 3 matching issues"
        );
    }

    #[test]
    fn workflow_projection_uses_canonical_wire_statuses() {
        let plan_json = sample_plan_json();
        let mut expected_step_statuses = [
            WorkflowStepStatus::Pending,
            WorkflowStepStatus::Ready,
            WorkflowStepStatus::Running,
            WorkflowStepStatus::InterruptRequested,
            WorkflowStepStatus::Interrupted,
            WorkflowStepStatus::WaitingInput,
            WorkflowStepStatus::WaitingReview,
            WorkflowStepStatus::Blocked,
            WorkflowStepStatus::Completed,
            WorkflowStepStatus::Failed,
            WorkflowStepStatus::Skipped,
        ]
        .into_iter()
        .map(|status| {
            let execution = sample_execution(WorkflowExecutionStatus::Running);
            let plan = sample_plan(execution.plan_id);
            let revision = sample_revision(plan.id, plan_json.clone());
            let (session_agents, agents) = sample_agent_views();
            let projection = build_workflow_card_projection(
                &execution,
                &plan,
                &revision,
                std::slice::from_ref(&revision),
                &[sample_step(status.clone())],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &session_agents,
                &agents,
                false,
                None,
            )
            .expect("build projection");

            let expected_status = to_workflow_wire_value(&status);
            assert_eq!(projection.steps[0].status, expected_status);
            assert_eq!(
                projection.plan.nodes[0].data.status.as_deref(),
                Some(expected_status.as_str())
            );

            projection.steps[0].status.clone()
        })
        .collect::<Vec<_>>();
        expected_step_statuses.sort();

        assert!(expected_step_statuses.contains(&"waiting_input".to_string()));
        assert!(expected_step_statuses.contains(&"waiting_review".to_string()));
        assert!(expected_step_statuses.contains(&"interrupt_requested".to_string()));

        for status in [
            WorkflowExecutionStatus::Pending,
            WorkflowExecutionStatus::Running,
            WorkflowExecutionStatus::Failed,
            WorkflowExecutionStatus::Paused,
            WorkflowExecutionStatus::Recompiling,
            WorkflowExecutionStatus::Completed,
            WorkflowExecutionStatus::Waiting,
        ] {
            let execution = sample_execution(status.clone());
            let plan = sample_plan(execution.plan_id);
            let revision = sample_revision(plan.id, plan_json.clone());
            let (session_agents, agents) = sample_agent_views();
            let projection = build_workflow_card_projection(
                &execution,
                &plan,
                &revision,
                std::slice::from_ref(&revision),
                &[sample_step(WorkflowStepStatus::Completed)],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &session_agents,
                &agents,
                false,
                None,
            )
            .expect("build projection");

            assert_eq!(projection.execution_status, to_workflow_wire_value(&status));
            if matches!(status, WorkflowExecutionStatus::Recompiling) {
                assert!(matches!(projection.state, WorkflowCardState::Running));
            }
        }
    }

    #[test]
    fn workflow_projection_includes_pending_review_and_latest_review_fields() {
        let execution = sample_execution(WorkflowExecutionStatus::Waiting);
        let plan_json = sample_plan_json();
        let plan = sample_plan(execution.plan_id);
        let revision = sample_revision(plan.id, plan_json);
        let (session_agents, agents) = sample_agent_views();
        let mut step = sample_step(WorkflowStepStatus::WaitingInput);
        step.execution_id = execution.id;
        step.user_review_required = true;
        step.retry_count = 1;
        step.max_retry = 3;
        step.summary_text = Some(
            serde_json::json!({
                "summary": "Need user confirmation",
                "content": "Draft ready",
                "outputs": ["src/handler.rs"]
            })
            .to_string(),
        );
        let review = sample_step_review(&step);
        let transcript = sample_step_review_transcript(&step);

        let projection = build_workflow_card_projection(
            &execution,
            &plan,
            &revision,
            std::slice::from_ref(&revision),
            &[step.clone()],
            &[],
            &[],
            &[],
            &[],
            &[review],
            std::slice::from_ref(&transcript),
            &[],
            &session_agents,
            &agents,
            false,
            None,
        )
        .expect("build projection");

        assert_eq!(
            projection.steps[0].review_phase.as_deref(),
            Some("user_review")
        );
        assert_eq!(projection.steps[0].retry_count, 1);
        assert_eq!(projection.steps[0].max_retry, 3);
        assert_eq!(
            projection.steps[0]
                .latest_review
                .as_ref()
                .map(|item| item.verdict.as_str()),
            Some("approved")
        );
        assert_eq!(
            projection
                .pending_review
                .as_ref()
                .map(|item| item.review_type.as_str()),
            Some("step_user_review")
        );
        assert_eq!(
            projection
                .pending_review
                .as_ref()
                .map(|item| item.target_id.as_str()),
            Some(projection.steps[0].id.as_str())
        );
        assert_eq!(projection.pending_reviews.len(), 1);
        assert_eq!(
            projection.pending_reviews[0].review_id,
            transcript.id.to_string()
        );
    }

    #[test]
    fn workflow_projection_includes_all_pending_step_reviews() {
        let execution = sample_execution(WorkflowExecutionStatus::Waiting);
        let plan_json = sample_plan_json();
        let plan = sample_plan(execution.plan_id);
        let revision = sample_revision(plan.id, plan_json);
        let (session_agents, agents) = sample_agent_views();
        let mut first_step = sample_step(WorkflowStepStatus::WaitingInput);
        first_step.execution_id = execution.id;
        first_step.title = "First step".to_string();
        let mut second_step = sample_step(WorkflowStepStatus::WaitingInput);
        second_step.execution_id = execution.id;
        second_step.title = "Second step".to_string();
        let first_transcript = sample_step_review_transcript(&first_step);
        let second_transcript = sample_step_review_transcript(&second_step);

        let projection = build_workflow_card_projection(
            &execution,
            &plan,
            &revision,
            std::slice::from_ref(&revision),
            &[first_step.clone(), second_step.clone()],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[first_transcript.clone(), second_transcript.clone()],
            &[],
            &session_agents,
            &agents,
            false,
            None,
        )
        .expect("build projection");

        assert_eq!(projection.pending_reviews.len(), 2);
        assert_eq!(
            projection
                .pending_review
                .as_ref()
                .map(|review| review.review_id.clone()),
            Some(first_transcript.id.to_string())
        );
        assert_eq!(
            projection
                .pending_reviews
                .iter()
                .map(|review| review.target_id.clone())
                .collect::<Vec<_>>(),
            vec![first_step.id.to_string(), second_step.id.to_string()]
        );
    }

    #[test]
    fn lightweight_projection_excludes_step_content() {
        let execution = sample_execution(WorkflowExecutionStatus::Completed);
        let plan_json = sample_plan_json();
        let plan = sample_plan(execution.plan_id);
        let revision = sample_revision(plan.id, plan_json);
        let (session_agents, agents) = sample_agent_views();
        let mut step = sample_step(WorkflowStepStatus::Completed);
        step.execution_id = execution.id;
        step.content = Some("Detailed implementation content".to_string());
        step.summary_text = Some(r#"{"summary":"Fixed the bug","outputs":[]}"#.to_string());

        let projection = build_workflow_card_projection_lightweight(
            &execution,
            &plan,
            &revision,
            std::slice::from_ref(&revision),
            &[step.clone()],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &[],
            &session_agents,
            &agents,
            Some(42i64),
            false,
            None,
        )
        .expect("build lightweight projection");
        assert_eq!(projection.has_transcripts, Some(true));
        assert_eq!(projection.round_graphs.len(), 1);
        assert!(projection.round_graphs[0].steps[0].content.is_none());
        assert!(projection.steps[0].content.is_none());
        assert_eq!(
            projection.steps[0].summary_text.as_deref(),
            Some("Fixed the bug")
        );
    }

    #[test]
    fn is_terminal_true_for_completed_and_failed() {
        for (status, expected_terminal) in [
            (WorkflowExecutionStatus::Completed, true),
            (WorkflowExecutionStatus::Failed, true),
            (WorkflowExecutionStatus::Running, false),
            (WorkflowExecutionStatus::Pending, false),
            (WorkflowExecutionStatus::Paused, false),
            (WorkflowExecutionStatus::Waiting, false),
        ] {
            let execution = sample_execution(status);
            let plan_json = sample_plan_json();
            let plan = sample_plan(execution.plan_id);
            let revision = sample_revision(plan.id, plan_json);
            let (session_agents, agents) = sample_agent_views();
            let projection = build_workflow_card_projection_lightweight(
                &execution,
                &plan,
                &revision,
                std::slice::from_ref(&revision),
                &[sample_step(WorkflowStepStatus::Completed)],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &session_agents,
                &agents,
                None,
                false,
                None,
            )
            .expect("build lightweight projection");
            assert_eq!(
                projection.is_terminal, expected_terminal,
                "is_terminal mismatch for status {:?}",
                execution.status
            );
        }
    }

    #[test]
    fn workflow_projection_separates_user_stop_marker_from_error() {
        let execution = sample_execution(WorkflowExecutionStatus::Failed);
        let plan = sample_plan(execution.plan_id);
        let revision = sample_revision(plan.id, sample_plan_json());
        let steps = vec![sample_step(WorkflowStepStatus::Completed)];
        let (session_agents, agents) = sample_agent_views();

        for (stopped_by_user, error_message) in [
            (false, Some("ordinary failure".to_string())),
            (true, None),
        ] {
            let full = build_workflow_card_projection(
                &execution,
                &plan,
                &revision,
                std::slice::from_ref(&revision),
                &steps,
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &session_agents,
                &agents,
                stopped_by_user,
                error_message.clone(),
            )
            .expect("build full projection");
            let lightweight = build_workflow_card_projection_lightweight(
                &execution,
                &plan,
                &revision,
                std::slice::from_ref(&revision),
                &steps,
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &[],
                &session_agents,
                &agents,
                Some(0),
                stopped_by_user,
                error_message.clone(),
            )
            .expect("build lightweight projection");

            for projection in [&full, &lightweight] {
                assert_eq!(projection.stopped_by_user, stopped_by_user);
                assert_eq!(projection.error_message, error_message);
                let json = serde_json::to_string(projection).expect("serialize projection");
                assert!(!json.contains("\"error_message\":\"stopped_by_user\""));
            }
        }
    }

    #[test]
    fn pi_node_execution_accepts_pi_agent_type() {
        use executors::executors::BaseCodingAgent;
        let runner_type = "PI".to_string();
        let parsed = runner_type.parse::<BaseCodingAgent>();
        assert!(parsed.is_ok());
        assert_eq!(parsed.unwrap(), BaseCodingAgent::Pi);
    }

    #[test]
    fn pi_node_events_preserve_acp_error_mapping() {
        use executors::executors::ExecutorError;
        use std::io;

        let startup_error = ExecutorError::Io(io::Error::other(
            "ACP startup failed: connection refused",
        ));
        let display = format!("{startup_error}");
        assert!(
            display.contains("ACP startup failed"),
            "ACP startup failure must preserve prefix: {display}"
        );
        assert!(
            !matches!(startup_error, ExecutorError::FollowUpNotSupported(_)),
            "ACP startup failure must remain Io, not FollowUpNotSupported"
        );

        let follow_up_error = ExecutorError::FollowUpNotSupported(
            "Pi ACP could not reuse the requested session: Unknown sessionId".to_string(),
        );
        assert!(format!("{follow_up_error}").contains("Unknown sessionId"));
    }

    #[test]
    fn pi_node_cancel_uses_acp_cancel_notification() {
        use crate::services::workflow::workflow_orchestrator::reducer;

        assert!(reducer::validate_step_transition(
            &WorkflowStepStatus::Running,
            &WorkflowStepStatus::InterruptRequested,
        )
        .is_ok());
        assert!(reducer::validate_step_transition(
            &WorkflowStepStatus::Running,
            &WorkflowStepStatus::Interrupted,
        )
        .is_err());
        assert!(reducer::validate_step_transition(
            &WorkflowStepStatus::InterruptRequested,
            &WorkflowStepStatus::Interrupted,
        )
        .is_ok());
        assert!(reducer::validate_step_transition(
            &WorkflowStepStatus::InterruptRequested,
            &WorkflowStepStatus::Failed,
        )
        .is_ok());

        let step_id = Uuid::new_v4();
        clear_running_step(step_id, 0);
        cancel_running_step(step_id, 0);
        let token = executors::executors::CancellationToken::new();
        register_running_step(step_id, 0, token.clone());
        assert!(
            token.is_cancelled(),
            "pre-registered cancel must fire on register"
        );
        clear_running_step(step_id, 0);
    }

    #[test]
    fn pi_reducer_state_transitions_do_not_bypass_reducer() {
        use crate::services::workflow::workflow_orchestrator::reducer;

        assert!(reducer::validate_step_transition(
            &WorkflowStepStatus::Running,
            &WorkflowStepStatus::InterruptRequested,
        )
        .is_ok());
        assert!(reducer::validate_step_transition(
            &WorkflowStepStatus::InterruptRequested,
            &WorkflowStepStatus::Interrupted,
        )
        .is_ok());
        assert!(reducer::validate_step_transition(
            &WorkflowStepStatus::WaitingReview,
            &WorkflowStepStatus::InterruptRequested,
        )
        .is_ok());
        assert!(reducer::validate_step_transition(
            &WorkflowStepStatus::Running,
            &WorkflowStepStatus::Interrupted,
        )
        .is_err());
        assert!(reducer::validate_step_transition(
            &WorkflowStepStatus::Running,
            &WorkflowStepStatus::Skipped,
        )
        .is_err());
        assert!(reducer::validate_execution_transition(
            &WorkflowExecutionStatus::Running,
            &WorkflowExecutionStatus::Failed,
        )
        .is_ok());
        assert!(reducer::validate_execution_transition(
            &WorkflowExecutionStatus::Completed,
            &WorkflowExecutionStatus::Failed,
        )
        .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workflow_runner_prepares_mcp_before_spawn_and_projects_pi_records() {
        use crate::services::chat_runner::ChatRunner;
        use db::models::{
            chat_agent::{ChatAgent, CreateChatAgent},
            chat_session::{ChatSession, ChatSessionWorktreeMode},
            chat_session_agent::{ChatSessionAgent, CreateChatSessionAgent},
            member_execution_config::MemberExecutionConfig,
            workflow_agent_session::{CreateWorkflowAgentSession, WorkflowAgentSession},
            workflow_execution::{CreateWorkflowExecution, WorkflowExecution},
            workflow_plan::{CreateWorkflowPlan, WorkflowPlan},
            workflow_plan_revision::{CreateWorkflowPlanRevision, WorkflowPlanRevision},
            workflow_round::{CreateWorkflowRound, WorkflowRound},
            workflow_step::{CreateWorkflowStep, WorkflowStep},
            workflow_types::*,
        };
        use std::os::unix::fs::PermissionsExt;

        let _fixture_lock =
            crate::services::chat_runner::PI_FIXTURE_TEST_LOCK.lock().await;

        const PI_FIXTURE_DIR: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../executors/tests/fixtures/pi_acp"
        );

        let temp = tempfile::tempdir().expect("pi workflow workspace");
        let root = temp.path();
        let bin = root.join("bin");
        let nm_bin = root.join("node_modules/.bin");
        let pi_pkg = root.join("node_modules/@earendil-works/pi-coding-agent");
        let mcp_pkg = root.join("node_modules/pi-mcp-adapter");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&nm_bin).unwrap();
        std::fs::create_dir_all(&pi_pkg).unwrap();
        std::fs::create_dir_all(&mcp_pkg).unwrap();
        std::fs::write(pi_pkg.join("package.json"), r#"{"version":"0.83.0"}"#).unwrap();
        std::fs::write(mcp_pkg.join("package.json"), r#"{"version":"2.18.0"}"#).unwrap();
        std::fs::write(mcp_pkg.join("index.ts"), "export default () => {};").unwrap();

        let mode = 0o755;
        let npx_path = bin.join("npx");
        std::fs::write(&npx_path, std::fs::read_to_string(format!("{PI_FIXTURE_DIR}/fake_npx.sh")).unwrap()).unwrap();
        std::fs::set_permissions(&npx_path, std::fs::Permissions::from_mode(mode)).unwrap();
        let pi_acp_path = bin.join("pi-acp");
        std::fs::write(&pi_acp_path, std::fs::read_to_string(format!("{PI_FIXTURE_DIR}/fake_pi_acp.mjs")).unwrap()).unwrap();
        std::fs::set_permissions(&pi_acp_path, std::fs::Permissions::from_mode(mode)).unwrap();
        let pi_bin_path = nm_bin.join("pi");
        std::fs::write(&pi_bin_path, std::fs::read_to_string(format!("{PI_FIXTURE_DIR}/fake_pi.mjs")).unwrap()).unwrap();
        std::fs::set_permissions(&pi_bin_path, std::fs::Permissions::from_mode(mode)).unwrap();
        let mcp_bin_path = nm_bin.join("pi-mcp-adapter");
        std::fs::write(&mcp_bin_path, std::fs::read_to_string(format!("{PI_FIXTURE_DIR}/fake_pi_mcp_adapter.mjs")).unwrap()).unwrap();
        std::fs::set_permissions(&mcp_bin_path, std::fs::Permissions::from_mode(mode)).unwrap();

        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();

        let prompts = root.join("prompts.txt");
        let pids = root.join("pids.json");
        let session_file = root.join("sessions/session.jsonl");
        let perm_log = root.join("permissions.jsonl");
        let proto_log = root.join("protocol.jsonl");

        unsafe {
            std::env::set_var("OPENTEAMS_PI_QA_NPX_PATH", &npx_path);
            std::env::set_var("PATH", format!("{}:{}:{}", bin.display(), nm_bin.display(), std::env::var("PATH").unwrap_or_default()));
            std::env::set_var("HOME", root.join("home"));
            std::env::set_var("OPENTEAMS_FAKE_PI_PROMPTS", &prompts);
            std::env::set_var("OPENTEAMS_FAKE_PI_CHILD_PID_FILE", &pids);
            std::env::set_var("OPENTEAMS_FAKE_PI_SESSION_FILE", &session_file);
            std::env::set_var("OPENTEAMS_FAKE_PI_PERMISSION_LOG", &perm_log);
            std::env::set_var("OPENTEAMS_FAKE_PI_PROTOCOL_LOG", &proto_log);
        }

        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("connect sqlite");
        sqlx::migrate!("../db/migrations")
            .run(&pool)
            .await
            .expect("run migrations");
        let db = DBService { pool: pool.clone() };

        let session_id = Uuid::new_v4();
        let session = ChatSession::create(
            &db.pool,
            &db::models::chat_session::CreateChatSession {
                title: Some("pi workflow test".to_string()),
                workspace_path: Some(workspace.to_string_lossy().to_string()),
                project_id: None,
                worktree_mode: Some(ChatSessionWorktreeMode::Disabled),
            },
            session_id,
        )
        .await
        .expect("create session");

        let agent = ChatAgent::create(
            &db.pool,
            &CreateChatAgent {
                name: "Pi Worker".to_string(),
                runner_type: "PI".to_string(),
                system_prompt: Some("You are Pi.".to_string()),
                tools_enabled: Some(serde_json::json!({})),
                model_name: None,
                owner_project_id: None,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create agent");

        let session_agent = ChatSessionAgent::create(
            &db.pool,
            &CreateChatSessionAgent {
                session_id,
                agent_id: agent.id,
                member_name: Some("PiWorker".to_string()),
                workspace_path: Some(workspace.to_string_lossy().to_string()),
                allowed_skill_ids: Vec::new(),
                project_member_id: None,
                execution_config: MemberExecutionConfig::default(),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create session agent");

        let plan_json = r#"{"nodes":[],"edges":[],"loops":[]}"#.to_string();
        let plan = WorkflowPlan::create(
            &db.pool,
            &CreateWorkflowPlan {
                session_id,
                source_message_id: None,
                created_by_session_agent_id: Some(session_agent.id),
                title: "Pi workflow".to_string(),
                summary_text: None,
                plan_json: plan_json.clone(),
                plan_schema_version: 1,
                plan_hash: "pi-plan".to_string(),
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create plan");

        let revision = WorkflowPlanRevision::create(
            &db.pool,
            &CreateWorkflowPlanRevision {
                plan_id: plan.id,
                revision_no: 1,
                edited_by: WorkflowRevisionEditor::System,
                editor_session_agent_id: Some(session_agent.id),
                reason: Some("Pi fixture".to_string()),
                plan_json,
                plan_hash: "pi-plan".to_string(),
                validation_status: WorkflowValidationStatus::Valid,
                validation_errors_json: None,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create revision");

        let execution = WorkflowExecution::create(
            &db.pool,
            &CreateWorkflowExecution {
                session_id,
                plan_id: plan.id,
                active_revision_id: Some(revision.id),
                lead_session_agent_id: Some(session_agent.id),
                title: "Pi execution".to_string(),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create execution");

        let round = WorkflowRound::create(
            &db.pool,
            &CreateWorkflowRound {
                execution_id: execution.id,
                round_index: 1,
                source_revision_id: Some(revision.id),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create round");

        sqlx::query("UPDATE chat_workflow_executions SET active_round_id = ?2 WHERE id = ?1")
            .bind(execution.id)
            .bind(round.id)
            .execute(&db.pool)
            .await
            .expect("set active round");

        let workflow_session = WorkflowAgentSession::create(
            &db.pool,
            &CreateWorkflowAgentSession {
                workflow_execution_id: execution.id,
                session_agent_id: session_agent.id,
                role: WorkflowAgentSessionRole::Worker,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create workflow session");

        let step = WorkflowStep::create(
            &db.pool,
            &CreateWorkflowStep {
                execution_id: execution.id,
                round_id: round.id,
                compiled_revision_id: Some(revision.id),
                step_key: "pi-worker".to_string(),
                step_type: WorkflowStepType::Task,
                title: "Pi worker".to_string(),
                instructions: "Run the Pi fixture".to_string(),
                assigned_workflow_agent_session_id: Some(workflow_session.id),
                max_retry: 2,
                round_index: 1,
                display_order: 0,
                loop_id: None,
                lead_review_required: Some(false),
                user_review_required: Some(false),
                revision_context: None,
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create step");

        let chat_runner = ChatRunner::new(db.clone());

        let preparation_error = super::run_workflow_step_agent_prompt(
            &db,
            &chat_runner,
            &session,
            &agent,
            &session_agent,
            Some(&workflow_session),
            "workflow pi test",
            &step,
        )
        .await
        .expect_err("mcp=None must fail before workflow spawn");
        assert!(preparation_error.to_string().contains("not initialized"));
        assert!(
            !proto_log.exists(),
            "workflow executor was spawned before MCP preparation"
        );
        assert!(!prompts.exists(), "workflow prompt reached executor before MCP preparation");
        assert!(!pids.exists(), "workflow child process started before MCP preparation");

        let session_agent = ChatSessionAgent::update_execution_config_for_next_run(
            &db.pool,
            session_agent.id,
            None,
            MemberExecutionConfig {
                mcp: Some(Default::default()),
                runner_type: Some(executors::executors::BaseCodingAgent::Codex),
                ..Default::default()
            },
        )
        .await
        .expect("initialize MCP for adapter refusal");

        let isolation_error = super::run_workflow_step_agent_prompt(
            &db,
            &chat_runner,
            &session,
            &agent,
            &session_agent,
            Some(&workflow_session),
            "workflow pi test",
            &step,
        )
        .await
        .expect_err("an adapter without isolation must fail before workflow spawn");
        assert!(isolation_error.to_string().contains("isolation is not implemented"));
        assert!(
            !proto_log.exists(),
            "workflow executor was spawned after adapter preparation failed"
        );
        assert!(!prompts.exists(), "workflow prompt reached a rejected adapter");
        assert!(!pids.exists(), "workflow child process started for a rejected adapter");

        let session_agent = ChatSessionAgent::update_execution_config_for_next_run(
            &db.pool,
            session_agent.id,
            None,
            MemberExecutionConfig {
                mcp: Some(Default::default()),
                runner_type: Some(executors::executors::BaseCodingAgent::Pi),
                ..Default::default()
            },
        )
        .await
        .expect("initialize MCP before the successful workflow run");

        let result = super::run_workflow_step_agent_prompt(
            &db,
            &chat_runner,
            &session,
            &agent,
            &session_agent,
            Some(&workflow_session),
            "workflow pi test",
            &step,
        )
        .await;

        assert!(result.is_ok(), "workflow step should succeed: {:?}", result.err());

        let output = result.unwrap();
        assert!(
            output.output.contains("echo:") && output.output.contains("workflow pi test"),
            "output must contain echo response with workflow prompt: {}",
            output.output
        );
        assert!(
            output.token_usage.is_some(),
            "token usage must be recorded"
        );
        assert_eq!(
            output.token_usage.as_ref().unwrap().total_tokens,
            30,
            "billable total must equal Pi input plus output tokens"
        );
        assert!(
            output.run_id.is_some(),
            "run record ID must be persisted"
        );

        let updated_ws = WorkflowAgentSession::find_by_id(&db.pool, workflow_session.id)
            .await
            .expect("find ws")
            .expect("ws exists");
        assert!(
            updated_ws.agent_session_id.is_some(),
            "workflow session must persist agent_session_id"
        );
        assert_eq!(
            updated_ws.agent_session_id.as_deref(),
            Some("pi-offline-session")
        );

        let prompts_content = std::fs::read_to_string(&prompts).expect("read prompts");
        assert!(
            prompts_content.contains("workflow pi test"),
            "prompts file must record workflow prompt"
        );

        let follow_up = super::run_workflow_step_agent_follow_up(
            &db,
            &chat_runner,
            &session,
            &agent,
            &session_agent,
            &updated_ws,
            "workflow follow-up",
            &step,
        )
        .await;

        assert!(follow_up.is_ok(), "follow-up should succeed: {:?}", follow_up.err());
        let fu_output = follow_up.unwrap();
        assert!(
            fu_output.output.contains("echo:") && fu_output.output.contains("workflow follow-up"),
            "follow-up output must contain echo response"
        );

        let prompts_after = std::fs::read_to_string(&prompts).expect("read prompts after");
        assert!(
            prompts_after.contains("workflow follow-up"),
            "prompts file must record follow-up prompt"
        );

        unsafe {
            std::env::remove_var("OPENTEAMS_PI_QA_NPX_PATH");
        }
    }
}
