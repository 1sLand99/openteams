#[cfg(test)]
mod tests {
    use chrono::Utc;
    use db::models::workflow_types::{
        WorkflowPlanAgents, WorkflowPlanJson, WorkflowRoundStatus, WorkflowStepType,
    };

    use super::*;

    fn sample_round() -> WorkflowRound {
        let now = Utc::now();
        WorkflowRound {
            id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            round_index: 1,
            source_revision_id: Some(Uuid::new_v4()),
            status: WorkflowRoundStatus::Rejected,
            result_step_id: None,
            user_decision_summary: None,
            started_at: Some(now),
            completed_at: None,
            archived_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_step(
        round: &WorkflowRound,
        step_key: &str,
        step_type: WorkflowStepType,
    ) -> WorkflowStep {
        let now = Utc::now();
        WorkflowStep {
            id: Uuid::new_v4(),
            execution_id: round.execution_id,
            round_id: round.id,
            compiled_revision_id: round.source_revision_id,
            step_key: step_key.to_string(),
            step_type,
            title: format!("Step {step_key}"),
            instructions: "Do the work".to_string(),
            assigned_workflow_agent_session_id: None,
            status: WorkflowStepStatus::Completed,
            retry_count: 0,
            max_retry: 1,
            round_index: round.round_index,
            display_order: 0,
            latest_run_id: Some(Uuid::new_v4()),
            summary_text: Some(
                serde_json::json!({
                    "summary": format!("{step_key} summary"),
                    "content": format!("{step_key} content"),
                    "outputs": [format!("out/{step_key}.md")]
                })
                .to_string(),
            ),
            content: Some(format!("{step_key} content")),
            loop_id: None,
            lead_review_required: true,
            user_review_required: false,
            revision_context: None,
            created_at: now,
            updated_at: now,
            started_at: Some(now),
            completed_at: Some(now),
        }
    }

    fn sample_plan() -> WorkflowPlanJson {
        WorkflowPlanJson {
            version: "1".to_string(),
            title: "Iteration Plan".to_string(),
            goal: "Ship the improved result".to_string(),
            agents: WorkflowPlanAgents {
                lead: "lead-agent".to_string(),
                available: vec!["lead-agent".to_string(), "worker-agent".to_string()],
            },
            globals: None,
            viewport: None,
            nodes: Vec::new(),
            edges: Vec::new(),
            loops: None,
            policies: None,
        }
    }

    fn sample_planning_agents() -> Vec<WorkflowPlanningAgent> {
        vec![
            WorkflowPlanningAgent {
                agent_id: "lead-session-agent".to_string(),
                session_agent_id: "lead-session-agent".to_string(),
                underlying_agent_id: "lead-agent".to_string(),
                name: "Lead".to_string(),
                workflow_role: "lead".to_string(),
                member_role: Some("技术负责人".to_string()),
                runner_type: "claude_code".to_string(),
                model_name: None,
                tools_enabled: vec![],
                skills: vec!["writing-plans".to_string()],
                capability_profile: Some("Owns planning and review.".to_string()),
                responsibilities: "Owns the workflow plan.".to_string(),
            },
            WorkflowPlanningAgent {
                agent_id: "worker-session-agent".to_string(),
                session_agent_id: "worker-session-agent".to_string(),
                underlying_agent_id: "worker-agent".to_string(),
                name: "Worker".to_string(),
                workflow_role: "worker".to_string(),
                member_role: Some("后端工程师".to_string()),
                runner_type: "codex".to_string(),
                model_name: None,
                tools_enabled: vec![],
                skills: vec!["code-guidelines".to_string()],
                capability_profile: Some("Implements backend tasks.".to_string()),
                responsibilities: "Executes assigned steps.".to_string(),
            },
        ]
    }

    fn build_test_iteration_prompt(
        original_goal: &str,
        current_state_summary: &str,
        user_feedback_json: &str,
        lead_agent_id: &str,
        available_agents: &[WorkflowPlanningAgent],
        previous_plan: &WorkflowPlanJson,
        response_language_instruction: &str,
    ) -> String {
        let input = build_iteration_plan_generation_input(
            original_goal,
            current_state_summary,
            user_feedback_json,
            lead_agent_id,
            available_agents,
            previous_plan,
            response_language_instruction,
        );
        crate::services::workflow_runtime::prompt_builders::plan_generation::build_plan_generation_prompt(
            &input,
        )
    }

    #[test]
    fn summarize_round_results_collects_steps_result_and_outputs() {
        let round = sample_round();
        let steps = vec![
            sample_step(&round, "draft", WorkflowStepType::Task),
            sample_step(&round, "result", WorkflowStepType::Result),
        ];

        let summary = summarize_round_results(&round, &steps);

        assert_eq!(summary.round_index, 1);
        assert_eq!(summary.result_summary.as_deref(), Some("result summary"));
        assert!(
            summary
                .step_summaries
                .iter()
                .any(|line| line.contains("[draft]"))
        );
        assert!(summary.outputs.contains(&"out/draft.md".to_string()));
        assert!(summary.outputs.contains(&"out/result.md".to_string()));
    }

    #[test]
    fn iteration_plan_builder_includes_feedback_history_and_agents() {
        let round = sample_round();
        let feedback = WorkflowIterationFeedback {
            id: Uuid::new_v4(),
            execution_id: round.execution_id,
            from_round_id: round.id,
            to_round_id: None,
            user_feedback_json: serde_json::json!({
                "action": "reject",
                "feedback": {
                    "what_wrong": "Missing tests",
                    "expected": "Add regression coverage",
                    "priority": "high"
                }
            })
            .to_string(),
            current_status_summary: "Round 1 completed without tests".to_string(),
            new_plan_diff: None,
            created_at: Utc::now(),
        };

        let prompt = build_test_iteration_prompt(
            "Ship a stable workflow",
            &feedback.current_status_summary,
            &feedback.user_feedback_json,
            "lead-agent",
            &sample_planning_agents(),
            &sample_plan(),
            "You MUST write human-readable JSON string values in English.",
        );

        assert!(prompt.contains("# 根据用户反馈重新生成工作流计划"));
        assert!(prompt.contains("## 输出方式（两阶段）"));
        assert!(prompt.contains("Ship a stable workflow"));
        assert!(prompt.contains("Round 1 completed without tests"));
        assert!(prompt.contains("- what_wrong: Missing tests"));
        assert!(prompt.contains("- expected: Add regression coverage"));
        assert!(prompt.contains("lead-session-agent"));
        assert!(prompt.contains("worker-session-agent"));
        assert!(prompt.contains("writing-plans"));
        assert!(prompt.contains("code-guidelines"));
        assert!(prompt.contains("## 编译规则"));
        assert!(prompt.contains("## 节点字段说明"));
        assert!(prompt.contains("## 输出 JSON Schema"));
        assert!(prompt.contains("## 上一版计划"));
        assert!(!prompt.contains("openteams_untrusted_data"));
        assert!(!prompt.contains("Data Boundary"));
        assert!(prompt.trim_end().ends_with("先以 Markdown 简要说明调整内容，再在末尾输出一个匹配 Schema 的完整 JSON 对象。"));
    }

    #[test]
    fn iteration_plan_builder_injects_full_previous_plan_json() {
        let mut previous_plan = sample_plan();
        previous_plan.nodes = vec![
            db::models::workflow_types::WorkflowPlanNode {
                id: "draft".to_string(),
                node_type: "workflowStep".to_string(),
                position: db::models::workflow_types::WorkflowNodePosition { x: 0.0, y: 0.0 },
                data: db::models::workflow_types::WorkflowNodeData {
                    step_type: "task".to_string(),
                    agent_id: Some("worker-session-agent".to_string()),
                    title: "Draft".to_string(),
                    instructions: "Draft the feature".to_string(),
                    acceptance: Some(db::models::workflow_types::AcceptanceCriteria {
                        required: vec!["Draft accepted".to_string()],
                        ..Default::default()
                    }),
                    outputs: Some(vec!["out/draft.md".to_string()]),
                    self_check: Some(vec!["Draft written".to_string()]),
                    verification_commands: Some(vec!["cargo test draft".to_string()]),
                    completion_evidence: Some(vec!["test output".to_string()]),
                    interruptible: true,
                    max_retry: None,
                    status: None,
                    loop_key: None,
                    review_scope: None,
                },
            },
            db::models::workflow_types::WorkflowPlanNode {
                id: "result".to_string(),
                node_type: "workflowStep".to_string(),
                position: db::models::workflow_types::WorkflowNodePosition { x: 0.0, y: 140.0 },
                data: db::models::workflow_types::WorkflowNodeData {
                    step_type: "result".to_string(),
                    agent_id: None,
                    title: "Result".to_string(),
                    instructions: "Summarize".to_string(),
                    acceptance: None,
                    outputs: None,
                    self_check: None,
                    verification_commands: None,
                    completion_evidence: None,
                    interruptible: true,
                    max_retry: None,
                    status: None,
                    loop_key: None,
                    review_scope: None,
                },
            },
        ];
        previous_plan.edges = vec![db::models::workflow_types::WorkflowPlanEdge {
            id: "draft->result".to_string(),
            source: "draft".to_string(),
            target: "result".to_string(),
            edge_type: None,
            data: None,
        }];

        let prompt = build_test_iteration_prompt(
            "Ship a stable workflow",
            "Round 1 completed",
            r#"{"action":"reject","feedback":{"what_wrong":"Missing tests"}}"#,
            "lead-session-agent",
            &sample_planning_agents(),
            &previous_plan,
            "You MUST write human-readable JSON string values in English.",
        );

        assert!(prompt.contains("## 上一版计划"));
        // Full previous plan content must be present: node ids, edges, and
        // the original acceptance/outputs contract fields.
        assert!(prompt.contains("\"id\": \"draft\""));
        assert!(prompt.contains("\"id\": \"draft->result\""));
        assert!(prompt.contains("Draft accepted"));
        assert!(prompt.contains("out/draft.md"));
        assert!(prompt.contains("cargo test draft"));
        assert!(prompt.contains("保留用户未要求变更的节点 ID"));
        assert!(prompt.contains("## 最新用户反馈"));
    }
}
