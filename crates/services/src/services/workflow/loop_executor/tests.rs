#[cfg(test)]
mod tests {
    use chrono::Utc;
    use db::models::workflow_types::WorkflowExecutionStatus;

    use super::*;

    fn sample_execution() -> WorkflowExecution {
        let now = Utc::now();
        WorkflowExecution {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            plan_id: Uuid::new_v4(),
            active_revision_id: Some(Uuid::new_v4()),
            active_round_id: Some(Uuid::new_v4()),
            workflow_card_message_id: None,
            lead_session_agent_id: None,
            status: WorkflowExecutionStatus::Running,
            current_round: 1,
            title: "Loop execution".to_string(),
            compiled_graph_hash: None,
            started_at: Some(now),
            completed_at: None,
            cleaned_at: None,
            cleaned_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_loop(loop_key: &str) -> WorkflowLoop {
        let now = Utc::now();
        WorkflowLoop {
            id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            round_id: Uuid::new_v4(),
            loop_key: loop_key.to_string(),
            review_step_id: Uuid::new_v4(),
            member_step_ids_json: "[]".to_string(),
            status: WorkflowLoopStatus::Running,
            retry_count: 1,
            max_retry: 1,
            user_review_required: false,
            rejection_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn sample_loop_step(workflow_loop: &WorkflowLoop, step_key: &str) -> WorkflowStep {
        let now = Utc::now();
        WorkflowStep {
            id: Uuid::new_v4(),
            execution_id: workflow_loop.execution_id,
            round_id: workflow_loop.round_id,
            compiled_revision_id: None,
            step_key: step_key.to_string(),
            step_type: db::models::workflow_types::WorkflowStepType::Task,
            title: step_key.to_string(),
            instructions: String::new(),
            assigned_workflow_agent_session_id: None,
            status: WorkflowStepStatus::Completed,
            retry_count: 0,
            max_retry: 1,
            round_index: 1,
            display_order: 1,
            latest_run_id: None,
            summary_text: None,
            content: None,
            loop_id: Some(workflow_loop.id),
            lead_review_required: false,
            user_review_required: false,
            revision_context: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            completed_at: Some(now),
        }
    }

    #[test]
    fn loop_reviewer_rejection_event_uses_actual_reviewer_context() {
        let execution = sample_execution();
        let step_id = Uuid::new_v4();

        let event = loop_reviewer_review_rejected_event(&execution, step_id, "reviewer");

        assert_eq!(event.session_id, execution.session_id);
        assert_eq!(event.execution_id, execution.id);
        assert_eq!(event.plan_id, execution.plan_id);
        assert_eq!(event.step_id, step_id);
        assert_eq!(event.reviewer_type, "reviewer");
    }

    #[test]
    fn loop_reviewer_rejection_runtime_path_sets_reviewer_analytics() {
        let execution = sample_execution();
        let step_id = Uuid::new_v4();
        let session_id = execution.session_id.to_string();
        let execution_id = execution.id.to_string();
        let plan_id = execution.plan_id.to_string();
        let task_id = step_id.to_string();

        let event =
            loop_reviewer_review_rejected_analytics_parts(&execution, step_id, "reviewer");

        assert_eq!(event.payload.event_name(), "quality.review_decision_recorded");
        assert_eq!(event.context.session_id.map(|id| id.to_string()).as_deref(), Some(session_id.as_str()));
        assert_eq!(event.context.workflow_execution_id.map(|id| id.to_string()).as_deref(), Some(execution_id.as_str()));
        assert_eq!(event.context.plan_id.map(|id| id.to_string()).as_deref(), Some(plan_id.as_str()));
        assert_eq!(event.context.step_id.map(|id| id.to_string()).as_deref(), Some(task_id.as_str()));
        let properties = event.payload.properties();
        assert_eq!(properties["review_verdict"], serde_json::json!("rejected"));
        assert_eq!(properties["reviewer_type"], serde_json::json!("reviewer"));
        assert_eq!(
            properties["resolution"],
            serde_json::json!("review_node_rejected")
        );
    }

    #[test]
    fn review_scope_steps_are_ordered_by_internal_dag_edges() {
        let review_scope = vec!["publish".to_string(), "draft".to_string(), "review".to_string()];
        let edges = vec![
            db::models::workflow_types::WorkflowPlanEdge {
                id: "draft-review".to_string(),
                source: "draft".to_string(),
                target: "review".to_string(),
                edge_type: None,
                data: None,
            },
            db::models::workflow_types::WorkflowPlanEdge {
                id: "review-publish".to_string(),
                source: "review".to_string(),
                target: "publish".to_string(),
                edge_type: None,
                data: None,
            },
        ];

        assert_eq!(
            review_scope_step_keys_in_dag_order(&review_scope, &edges),
            vec!["draft", "review", "publish"]
        );
    }

    #[test]
    fn pending_loop_feedback_is_independent_from_step_retry_count() {
        let workflow_loop = sample_loop("loop-a");
        let mut step = sample_loop_step(&workflow_loop, "member");
        step.retry_count = 5;
        step.revision_context = Some(
            serde_json::json!({
                "pending_feedback": {
                    "scope": "loop",
                    "loop_key": "loop-a",
                    "feedback": "revise",
                    "review_round": 1
                }
            })
            .to_string(),
        );

        assert!(has_pending_feedback_for_loop(&step, &workflow_loop));
    }

    #[test]
    fn pending_loop_feedback_ignores_other_loops() {
        let workflow_loop = sample_loop("loop-a");
        let mut step = sample_loop_step(&workflow_loop, "member");
        step.revision_context = Some(
            serde_json::json!({
                "pending_feedback": {
                    "scope": "loop",
                    "loop_key": "loop-b",
                    "feedback": "revise",
                    "review_round": 1
                }
            })
            .to_string(),
        );

        assert!(!has_pending_feedback_for_loop(&step, &workflow_loop));

        step.revision_context = Some(
            serde_json::json!({
                "pending_feedback": {
                    "scope": "step",
                    "loop_key": "loop-a",
                    "feedback": "revise",
                    "review_round": 1
                }
            })
            .to_string(),
        );
        assert!(!has_pending_feedback_for_loop(&step, &workflow_loop));
    }

    #[test]
    fn loop_feedback_targets_only_named_steps_when_specific_feedback_exists() {
        let workflow_loop = sample_loop("loop-a");
        let step_a = sample_loop_step(&workflow_loop, "a");
        let step_b = sample_loop_step(&workflow_loop, "b");
        let steps = vec![step_a.clone(), step_b.clone()];
        let member_ids = [step_a.id, step_b.id].into_iter().collect::<HashSet<_>>();
        let step_feedbacks =
            HashMap::from([("b".to_string(), "only b needs revision".to_string())]);

        let feedback_by_step_id =
            loop_feedback_by_step_id(&steps, &member_ids, &step_feedbacks, "whole loop issue");

        assert_eq!(feedback_by_step_id.len(), 1);
        assert!(!feedback_by_step_id.contains_key(&step_a.id));
        assert_eq!(
            feedback_by_step_id.get(&step_b.id).map(String::as_str),
            Some("only b needs revision")
        );
    }

    #[test]
    fn loop_feedback_targets_all_members_when_specific_feedback_is_empty() {
        let workflow_loop = sample_loop("loop-a");
        let step_a = sample_loop_step(&workflow_loop, "a");
        let step_b = sample_loop_step(&workflow_loop, "b");
        let steps = vec![step_a.clone(), step_b.clone()];
        let member_ids = [step_a.id, step_b.id].into_iter().collect::<HashSet<_>>();
        let step_feedbacks = HashMap::new();

        let feedback_by_step_id =
            loop_feedback_by_step_id(&steps, &member_ids, &step_feedbacks, "whole loop issue");

        assert_eq!(feedback_by_step_id.len(), 2);
        assert_eq!(
            feedback_by_step_id.get(&step_a.id).map(String::as_str),
            Some("whole loop issue")
        );
        assert_eq!(
            feedback_by_step_id.get(&step_b.id).map(String::as_str),
            Some("whole loop issue")
        );
    }

    #[test]
    fn filtered_targets_are_the_only_feedback_injected_on_normal_retry() {
        let workflow_loop = sample_loop("loop-a");
        let active_target = LoopFeedbackTarget {
            step: sample_loop_step(&workflow_loop, "active"),
            issue_scope_id: "active-issue".to_string(),
            feedback: "retry active".to_string(),
        };
        let map = feedback_map_from_targets(&[active_target]);

        assert_eq!(map.len(), 1);
        assert_eq!(map.get("active").map(String::as_str), Some("retry active"));
        assert!(!map.contains_key("waived-skipped"));
    }

    #[test]
    fn persisted_loop_retry_budget_drives_review_attempt_limit() {
        let mut workflow_loop = sample_loop("loop-a");
        workflow_loop.max_retry = 0;
        assert_eq!(max_loop_review_attempts(&workflow_loop), 1);
        assert!(loop_review_attempt_limit_reached(1, 1));

        workflow_loop.max_retry = 2;
        assert_eq!(max_loop_review_attempts(&workflow_loop), 3);
        assert!(!loop_review_attempt_limit_reached(2, 3));
        assert!(loop_review_attempt_limit_reached(3, 3));
    }

    #[test]
    fn all_waived_targets_pass_before_the_dynamic_attempt_limit_is_applied() {
        assert_eq!(
            rejected_loop_review_disposition(2, 2, &[]),
            RejectedLoopReviewDisposition::PassedByUserWaiver
        );

        let workflow_loop = sample_loop("loop-a");
        let remaining_target = LoopFeedbackTarget {
            step: sample_loop_step(&workflow_loop, "remaining"),
            issue_scope_id: "remaining-issue".to_string(),
            feedback: "still unresolved".to_string(),
        };
        assert_eq!(
            rejected_loop_review_disposition(2, 2, &[remaining_target]),
            RejectedLoopReviewDisposition::LimitReached
        );
        assert_eq!(
            rejected_loop_review_disposition(1, 2, &[LoopFeedbackTarget {
                step: sample_loop_step(&sample_loop("loop-a"), "retry"),
                issue_scope_id: "retry-issue".to_string(),
                feedback: "retry".to_string(),
            }]),
            RejectedLoopReviewDisposition::Retry
        );
    }

    #[test]
    fn waiver_covered_lead_pass_preserves_required_user_acceptance_checkpoint() {
        let mut workflow_loop = sample_loop("loop-a");
        workflow_loop.user_review_required = true;
        assert!(requires_user_acceptance_checkpoint(&workflow_loop));

        workflow_loop.user_review_required = false;
        assert!(!requires_user_acceptance_checkpoint(&workflow_loop));
    }

    #[test]
    fn keeping_skipped_step_clears_pending_feedback_and_records_waiver() {
        let workflow_loop = sample_loop("loop-a");
        let mut step = sample_loop_step(&workflow_loop, "step-a");
        step.status = WorkflowStepStatus::Skipped;
        let existing = serde_json::json!({
            "pending_feedback": {
                "scope": "loop",
                "loop_key": "loop-a",
                "feedback": "restart this work"
            }
        })
        .to_string();

        let merged = merge_loop_skip_waiver_context(
            Some(&existing),
            &workflow_loop,
            &step,
            "User accepts the skipped scope.",
        );
        step.revision_context = Some(merged.clone());
        let parsed: serde_json::Value = serde_json::from_str(&merged).expect("valid context");

        assert!(parsed.get("pending_feedback").is_none());
        assert!(
            loop_skip_waiver(&step, "loop-a")
                .as_deref()
                .is_some_and(|waiver| waiver.contains("User accepts the skipped scope."))
        );
        assert!(loop_skip_waiver(&step, "loop-b").is_none());
        assert!(has_matching_active_skip_waiver(
            &step,
            &workflow_loop,
            " User accepts   the skipped scope. "
        ));
        assert!(!has_matching_active_skip_waiver(
            &step,
            &workflow_loop,
            "the reviewer rephrased the same skipped-step concern"
        ));

        let stable_issue_scope = loop_skip_issue_scope_id(
            &workflow_loop,
            &step,
            "skipped-dependency-not-needed",
        );
        step.revision_context = Some(merge_loop_skip_waiver_context_for_issue(
            step.revision_context.as_deref(),
            &workflow_loop,
            &step,
            &stable_issue_scope,
            "The skipped dependency is not needed.",
        ));
        let prompt_waivers = loop_skip_waiver(&step, "loop-a").expect("active prompt waivers");
        assert!(prompt_waivers.contains("User accepts the skipped scope."));
        assert!(prompt_waivers.contains(stable_issue_scope.as_str()));
        assert!(has_matching_active_skip_waiver_for_issue(
            &step,
            &workflow_loop,
            &stable_issue_scope,
            "Rephrased: this dependency remains unnecessary."
        ));
        let new_issue_scope = loop_skip_issue_scope_id(
            &workflow_loop,
            &step,
            "new-security-regression",
        );
        assert!(!has_matching_active_skip_waiver_for_issue(
            &step,
            &workflow_loop,
            &new_issue_scope,
            "A new security regression was found."
        ));

        let waiver = parsed["loop_skip_waivers"]
            .as_array()
            .and_then(|waivers| waivers.last())
            .expect("active waiver");
        let expected_scope_id = format!("loop:{}:step:{}", workflow_loop.id, step.id);
        assert_eq!(
            waiver.get("scope_id").and_then(|value| value.as_str()),
            Some(expected_scope_id.as_str())
        );

        let superseded = supersede_loop_skip_waiver_context(
            step.revision_context.as_deref(),
            &workflow_loop,
            &step,
        )
        .expect("supersede active waiver");
        step.revision_context = Some(superseded);
        assert!(loop_skip_waiver(&step, "loop-a").is_none());
        assert!(!has_matching_active_skip_waiver(
            &step,
            &workflow_loop,
            "User accepts the skipped scope."
        ));
    }

    #[test]
    fn waiver_prompt_only_applies_while_step_is_still_skipped() {
        let workflow_loop = sample_loop("loop-a");
        let mut step = sample_loop_step(&workflow_loop, "step-a");
        step.status = WorkflowStepStatus::Skipped;
        step.revision_context = Some(merge_loop_skip_waiver_context(
            None,
            &workflow_loop,
            &step,
            "accepted issue",
        ));
        assert!(
            loop_skip_waiver(&step, "loop-a")
                .as_deref()
                .is_some_and(|waiver| waiver.contains("accepted issue"))
        );
        step.status = WorkflowStepStatus::Pending;
        assert!(loop_skip_waiver(&step, "loop-a").is_none());
    }
}
