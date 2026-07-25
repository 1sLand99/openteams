//! Iteration feedback + step/loop review action handlers.

use std::collections::HashSet;

use chrono::Utc;
use db::{
    DBService,
    models::{
        chat_session::ChatSession,
        chat_session_agent::ChatSessionAgent,
        workflow_agent_session::WorkflowAgentSession,
        workflow_event::{CreateWorkflowEvent, WorkflowEvent},
        workflow_execution::WorkflowExecution,
        workflow_loop::WorkflowLoop,
        workflow_plan::WorkflowPlan,
        workflow_plan_revision::WorkflowPlanRevision,
        workflow_round::WorkflowRound,
        workflow_step::WorkflowStep,
        workflow_step_review::{CreateWorkflowStepReview, WorkflowStepReview},
        workflow_transcript::{CreateWorkflowTranscript, WorkflowTranscript},
        workflow_types::*,
    },
};
use sqlx::{SqliteConnection, SqlitePool};
use utils::assets::config_path;
use uuid::Uuid;

use super::{
    super::{
        chat_runner::ChatRunner,
        config::{self, UiLanguage},
        workflow_analytics,
        workflow_iteration::IterationManager,
        workflow_loop_executor::{
            LoopExecutor, has_matching_active_skip_waiver, loop_skip_issue_scope_id_for_feedback,
            supersede_loop_skip_waiver_context,
        },
        workflow_runtime::{SummaryPayload, WorkflowRevisionFeedbackSource, parse_summary_payload},
    },
    IterationFeedbackOutcome, OrchestratorError, ResolvedTranscriptAction, WorkflowOrchestrator,
    load_agents_for_session,
};
use crate::services::{inbox::InboxService, project::source_control::SourceControlService};

fn skipped_retry_keep_effect(retryable_step_ids: &[Uuid]) -> &'static str {
    if retryable_step_ids.is_empty() {
        "waive_skipped_scope_and_complete_loop"
    } else {
        "waive_skipped_scope_and_retry_remaining_targets"
    }
}

impl WorkflowOrchestrator {
    pub async fn handle_iteration_feedback(
        db: &DBService,
        chat_runner: &ChatRunner,
        execution_id: Uuid,
        action: &str,
        feedback: Option<super::super::workflow_iteration::UserIterationFeedbackDetail>,
    ) -> Result<IterationFeedbackOutcome, OrchestratorError> {
        let pool = &db.pool;
        let execution = WorkflowExecution::find_by_id(pool, execution_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("execution {execution_id} not found"))
            })?;
        let normalized_action = action.trim().to_ascii_lowercase();

        match normalized_action.as_str() {
            "accept" | "accepted" => {
                let execution =
                    Self::accept_iteration_result(pool, chat_runner, &execution).await?;
                Ok(IterationFeedbackOutcome {
                    execution,
                    should_wake_scheduler: false,
                })
            }
            "reject" | "rejected" => {
                let feedback = feedback.ok_or_else(|| {
                    OrchestratorError::IllegalTransition(
                        "feedback is required when rejecting an iteration result".to_string(),
                    )
                })?;
                let execution =
                    Self::reject_iteration_result(db, chat_runner, &execution, feedback).await?;
                Ok(IterationFeedbackOutcome {
                    execution,
                    should_wake_scheduler: false,
                })
            }
            _ => Err(OrchestratorError::IllegalTransition(format!(
                "unsupported iteration feedback action '{}'",
                action
            ))),
        }
    }

    async fn accept_iteration_result(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
    ) -> Result<WorkflowExecution, OrchestratorError> {
        if execution.status == WorkflowExecutionStatus::Completed {
            SourceControlService::invalidate_session_caches(execution.session_id);
            Self::refresh_execution_projection_with_reason(
                pool,
                chat_runner,
                execution.id,
                None,
                "iteration_accept_completed",
                Vec::new(),
            )
            .await?;
            return WorkflowExecution::find_by_id(pool, execution.id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("execution {} not found", execution.id))
                });
        }
        if execution.status != WorkflowExecutionStatus::Waiting {
            return Err(OrchestratorError::IllegalTransition(format!(
                "execution {} is {:?}, expected waiting",
                execution.id, execution.status
            )));
        }

        if let Some(transcript) =
            WorkflowTranscript::find_unresolved_final_review_by_execution(pool, execution.id)
                .await?
        {
            let updated_meta_json = Self::merge_transcript_meta(
                transcript.meta_json.as_deref(),
                serde_json::json!({
                    "resolved": true,
                    "resolved_action": "accepted",
                    "resolved_at": Utc::now().to_rfc3339(),
                }),
            );
            WorkflowTranscript::update_meta_json(pool, transcript.id, &updated_meta_json).await?;
        }

        if let Some(round_id) = execution.active_round_id {
            WorkflowRound::update_status(pool, round_id, WorkflowRoundStatus::Accepted).await?;
        }
        Self::emit_final_review_decision_event(
            pool,
            execution,
            WorkflowEventType::UserAccepted,
            "user_accepted",
        )
        .await?;
        workflow_analytics::track_final_review_decision(
            chat_runner.analytics_service(),
            execution.session_id,
            execution.id,
            execution.plan_id,
            true,
        );
        let completed_execution = Self::transition_execution_and_sync(
            pool,
            chat_runner,
            execution,
            WorkflowExecutionStatus::Completed,
            "iteration_accepted",
            None,
        )
        .await?;

        let workflow_agent_sessions =
            WorkflowAgentSession::find_by_execution(pool, completed_execution.id).await?;
        let session_agents =
            ChatSessionAgent::find_all_for_session(pool, completed_execution.session_id).await?;
        let agents = load_agents_for_session(pool, &session_agents).await?;
        Self::persist_completion_work_items(
            pool,
            chat_runner,
            &completed_execution,
            &WorkflowStep::find_by_execution(pool, completed_execution.id).await?,
            &workflow_agent_sessions,
            &session_agents,
            &agents,
        )
        .await?;
        SourceControlService::invalidate_session_caches(completed_execution.session_id);

        Ok(completed_execution)
    }

    async fn reject_iteration_result(
        db: &DBService,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        feedback: super::super::workflow_iteration::UserIterationFeedbackDetail,
    ) -> Result<WorkflowExecution, OrchestratorError> {
        let pool = &db.pool;
        if execution.status != WorkflowExecutionStatus::Waiting {
            return Err(OrchestratorError::IllegalTransition(format!(
                "execution {} is {:?}, expected waiting",
                execution.id, execution.status
            )));
        }

        if let Some(transcript) =
            WorkflowTranscript::find_unresolved_final_review_by_execution(pool, execution.id)
                .await?
        {
            let updated_meta_json = Self::merge_transcript_meta(
                transcript.meta_json.as_deref(),
                serde_json::json!({
                    "resolved": true,
                    "resolved_action": "rejected",
                    "resolved_at": Utc::now().to_rfc3339(),
                    "input_text": feedback.what_wrong.trim(),
                    "feedback": feedback.clone(),
                }),
            );
            WorkflowTranscript::update_meta_json(pool, transcript.id, &updated_meta_json).await?;
        }

        Self::emit_final_review_decision_event(
            pool,
            execution,
            WorkflowEventType::UserRejected,
            "user_rejected",
        )
        .await?;
        workflow_analytics::track_final_review_decision(
            chat_runner.analytics_service(),
            execution.session_id,
            execution.id,
            execution.plan_id,
            false,
        );

        let recompiling_execution = Self::transition_execution_and_sync(
            pool,
            chat_runner,
            execution,
            WorkflowExecutionStatus::Recompiling,
            "iteration_recompiling",
            None,
        )
        .await?;
        let recompile_result = async {
            let plan = WorkflowPlan::find_by_id(pool, recompiling_execution.plan_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!(
                        "plan {} not found",
                        recompiling_execution.plan_id
                    ))
                })?;
            let revision_id = recompiling_execution.active_revision_id.ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "execution {} missing active revision",
                    recompiling_execution.id
                ))
            })?;
            let active_revision = WorkflowPlanRevision::find_by_id(pool, revision_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("revision {revision_id} not found"))
                })?;
            let round_id = recompiling_execution.active_round_id.ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "execution {} missing active round",
                    recompiling_execution.id
                ))
            })?;
            let from_round = WorkflowRound::find_by_id(pool, round_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("round {round_id} not found"))
                })?;
            let session = ChatSession::find_by_id(pool, recompiling_execution.session_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!(
                        "session {} not found",
                        recompiling_execution.session_id
                    ))
                })?;
            let session_agents =
                ChatSessionAgent::find_all_for_session(pool, recompiling_execution.session_id)
                    .await?;
            let agents = load_agents_for_session(pool, &session_agents).await?;
            let iteration_manager = IterationManager {
                db,
                pool,
                chat_runner,
                session: &session,
                session_agents: &session_agents,
                agents: &agents,
            };
            let user_feedback = super::super::workflow_iteration::UserIterationFeedback {
                execution_id: recompiling_execution.id.to_string(),
                round_id: from_round.id.to_string(),
                action: "reject".to_string(),
                feedback: Some(feedback),
            };
            let iteration_feedback = iteration_manager
                .collect_user_feedback(&recompiling_execution, &from_round, &user_feedback)
                .await?;
            let new_plan_json = iteration_manager
                .generate_new_plan(
                    &recompiling_execution,
                    &plan,
                    &active_revision,
                    &from_round,
                    &iteration_feedback,
                )
                .await?;
            let result = iteration_manager
                .create_new_round(
                    &recompiling_execution,
                    &plan,
                    &active_revision,
                    &from_round,
                    &iteration_feedback,
                    &new_plan_json,
                )
                .await?;

            Ok::<WorkflowExecution, OrchestratorError>(result.execution)
        }
        .await;

        match recompile_result {
            Ok(execution) => Ok(execution),
            Err(err) => {
                let error_message = err.to_string();
                tracing::error!(
                    execution_id = %recompiling_execution.id,
                    error = %error_message,
                    "workflow iteration recompilation failed; restoring execution to waiting"
                );
                if let Err(compensation_err) = Self::transition_execution_and_sync(
                    pool,
                    chat_runner,
                    &recompiling_execution,
                    WorkflowExecutionStatus::Waiting,
                    "iteration_recompile_failed",
                    Some(error_message.clone()),
                )
                .await
                {
                    tracing::error!(
                        execution_id = %recompiling_execution.id,
                        error = %compensation_err,
                        original_error = %error_message,
                        "failed to restore workflow iteration recompilation to waiting"
                    );
                }
                Err(err)
            }
        }
    }

    async fn emit_final_review_decision_event(
        pool: &SqlitePool,
        execution: &WorkflowExecution,
        event_type: WorkflowEventType,
        resolution: &str,
    ) -> Result<WorkflowEvent, OrchestratorError> {
        WorkflowEvent::create(
            pool,
            &CreateWorkflowEvent {
                execution_id: execution.id,
                round_id: execution.active_round_id,
                step_id: None,
                agent_session_id: None,
                event_type,
                status_before: Some(to_workflow_wire_value(&execution.status)),
                status_after: Some(resolution.to_string()),
                detail_json: Some(
                    serde_json::json!({
                        "resolution": resolution,
                    })
                    .to_string(),
                ),
            },
            Uuid::new_v4(),
        )
        .await
        .map_err(OrchestratorError::Database)
    }

    async fn save_step_review_in_transaction(
        connection: &mut SqliteConnection,
        step: &WorkflowStep,
        reviewer_type: ReviewerType,
        reviewer_id: Option<String>,
        verdict: ReviewVerdict,
        feedback: &str,
    ) -> Result<WorkflowStepReview, OrchestratorError> {
        let review_round = sqlx::query_scalar::<_, i32>(
            r#"
            SELECT COUNT(*) + 1
            FROM chat_workflow_step_reviews
            WHERE step_id = ?1 AND reviewer_type = ?2
            "#,
        )
        .bind(step.id)
        .bind(&reviewer_type)
        .fetch_one(&mut *connection)
        .await?;
        WorkflowStepReview::create_in_transaction(
            connection,
            &CreateWorkflowStepReview {
                step_id: step.id,
                execution_id: step.execution_id,
                reviewer_type,
                reviewer_id,
                verdict,
                feedback: feedback.trim().to_string(),
                review_round: Some(review_round),
            },
            Uuid::new_v4(),
        )
        .await
        .map_err(OrchestratorError::Database)
    }

    async fn emit_step_domain_event_in_transaction(
        connection: &mut SqliteConnection,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        event_type: WorkflowEventType,
        detail_json: Option<serde_json::Value>,
    ) -> Result<WorkflowEvent, OrchestratorError> {
        WorkflowEvent::create_in_transaction(
            connection,
            &CreateWorkflowEvent {
                execution_id: execution.id,
                round_id: Some(step.round_id),
                step_id: Some(step.id),
                agent_session_id: step.assigned_workflow_agent_session_id,
                event_type,
                status_before: None,
                status_after: Some(to_workflow_wire_value(&step.status)),
                detail_json: detail_json.map(|value| value.to_string()),
            },
            Uuid::new_v4(),
        )
        .await
        .map_err(OrchestratorError::Database)
    }

    async fn write_transcript_in_transaction(
        connection: &mut SqliteConnection,
        data: CreateWorkflowTranscript,
    ) -> Result<WorkflowTranscript, OrchestratorError> {
        WorkflowTranscript::create_in_transaction(connection, &data, Uuid::new_v4())
            .await
            .map_err(OrchestratorError::Database)
    }

    async fn claim_review_transcript_in_transaction(
        connection: &mut SqliteConnection,
        transcript: &WorkflowTranscript,
        updated_meta_json: &str,
    ) -> Result<WorkflowTranscript, OrchestratorError> {
        WorkflowTranscript::update_meta_json_if_unresolved_in_transaction(
            connection,
            transcript.id,
            updated_meta_json,
        )
        .await?
        .ok_or_else(|| {
            OrchestratorError::IllegalTransition(format!(
                "transcript {} already resolved",
                transcript.id
            ))
        })
    }

    pub(super) async fn resolve_step_review_action(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        transcript: &WorkflowTranscript,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        workflow_session: &WorkflowAgentSession,
        resolved_action: &str,
        input_text: Option<&str>,
    ) -> Result<ResolvedTranscriptAction, OrchestratorError> {
        let existing_meta: serde_json::Value = transcript
            .meta_json
            .as_deref()
            .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if matches!(
            existing_meta.get("resolved"),
            Some(serde_json::Value::Bool(true))
        ) {
            return Err(OrchestratorError::IllegalTransition(format!(
                "transcript {} already resolved",
                transcript.id
            )));
        }

        if !matches!(
            resolved_action,
            "approved" | "approve" | "rejected" | "reject"
        ) {
            return Err(OrchestratorError::IllegalTransition(format!(
                "unsupported action '{}' for step_review",
                resolved_action
            )));
        }
        let feedback = input_text
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if matches!(resolved_action, "rejected" | "reject") && feedback.is_none() {
            return Err(OrchestratorError::IllegalTransition(
                "step review rejection requires feedback".to_string(),
            ));
        }

        let updated_meta_json = Self::merge_transcript_meta(
            transcript.meta_json.as_deref(),
            serde_json::json!({
                "resolved": true,
                "resolved_action": resolved_action,
                "resolved_at": Utc::now().to_rfc3339(),
                "input_text": feedback,
            }),
        );
        let is_approved = matches!(resolved_action, "approved" | "approve");
        let decision_feedback = if is_approved {
            feedback.unwrap_or_else(|| "User approved the step result.".to_string())
        } else {
            feedback.expect("rejected feedback validated before transaction")
        };
        let previous_payload =
            parse_summary_payload(step.summary_text.as_deref()).unwrap_or(SummaryPayload {
                summary: step.title.clone(),
                content: None,
                outputs: Vec::new(),
            });
        let previous_content = previous_payload
            .content
            .as_deref()
            .or(step.content.as_deref());
        let rejected_context = (!is_approved).then(|| {
            Self::merge_revision_context(
                step.revision_context.as_deref(),
                WorkflowRevisionFeedbackSource::User,
                &decision_feedback,
                &previous_payload.summary,
                previous_content,
                &previous_payload.outputs,
                step.retry_count + 1,
            )
        });

        let mut transaction = pool.begin().await?;
        let updated_transcript = Self::claim_review_transcript_in_transaction(
            &mut transaction,
            transcript,
            &updated_meta_json,
        )
        .await?;
        Self::save_step_review_in_transaction(
            &mut transaction,
            step,
            ReviewerType::User,
            None,
            if is_approved {
                ReviewVerdict::Approved
            } else {
                ReviewVerdict::Rejected
            },
            &decision_feedback,
        )
        .await?;
        Self::emit_step_domain_event_in_transaction(
            &mut transaction,
            execution,
            step,
            if is_approved {
                WorkflowEventType::StepUserReviewPassed
            } else {
                WorkflowEventType::StepUserReviewRejected
            },
            Some(serde_json::json!({ "feedback": decision_feedback })),
        )
        .await?;

        let final_step = if is_approved {
            let precompleted = super::reducer::transition_step_in_transaction(
                &mut transaction,
                execution,
                step,
                WorkflowStepStatus::PreCompleted,
                Some(serde_json::json!({ "reason": "step_precompleted" })),
            )
            .await?
            .entity;
            super::reducer::transition_step_in_transaction(
                &mut transaction,
                execution,
                &precompleted,
                WorkflowStepStatus::Completed,
                Some(serde_json::json!({ "reason": "step_completed" })),
            )
            .await?
            .entity
        } else {
            let revising = super::reducer::transition_step_in_transaction(
                &mut transaction,
                execution,
                step,
                WorkflowStepStatus::Revising,
                Some(serde_json::json!({ "reason": "step_revising" })),
            )
            .await?
            .entity;
            let revising = WorkflowStep::update_revision_context_in_transaction(
                &mut transaction,
                revising.id,
                rejected_context,
            )
            .await?;
            let retried =
                WorkflowStep::prepare_retry_in_transaction(&mut transaction, revising.id).await?;
            super::reducer::transition_step_in_transaction(
                &mut transaction,
                execution,
                &retried,
                WorkflowStepStatus::Ready,
                Some(serde_json::json!({ "reason": "step_resumed" })),
            )
            .await?
            .entity
        };
        Self::write_transcript_in_transaction(
            &mut transaction,
            CreateWorkflowTranscript {
                execution_id: execution.id,
                round_id: Some(final_step.round_id),
                workflow_agent_session_id: Some(workflow_session.id),
                step_id: Some(final_step.id),
                sender_type: "user".to_string(),
                entry_type: "message".to_string(),
                content: decision_feedback.clone(),
                meta_json: Some(
                    serde_json::json!({
                        "source_transcript_id": updated_transcript.id,
                        "action": resolved_action,
                    })
                    .to_string(),
                ),
            },
        )
        .await?;
        transaction.commit().await?;

        workflow_analytics::track_review_decision_recorded(
            chat_runner.analytics_service(),
            execution.session_id,
            execution.id,
            step.id,
            if is_approved { "approved" } else { "rejected" },
            "user",
        );
        let refreshed_execution = Self::refresh_review_state_after_commit(
            pool,
            chat_runner,
            execution,
            if is_approved {
                "step_completed"
            } else {
                "step_resumed"
            },
            vec![final_step.id.to_string()],
        )
        .await;
        let should_wake_scheduler =
            Self::should_wake_after_committed_review(pool, execution.id, updated_transcript.id)
                .await;

        Ok(ResolvedTranscriptAction {
            transcript: updated_transcript,
            execution: refreshed_execution,
            should_wake_scheduler,
        })
    }

    pub(super) async fn resolve_loop_review_action(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        transcript: &WorkflowTranscript,
        execution: &WorkflowExecution,
        step: &WorkflowStep,
        workflow_session: &WorkflowAgentSession,
        resolved_action: &str,
        input_text: Option<&str>,
    ) -> Result<ResolvedTranscriptAction, OrchestratorError> {
        let existing_meta: serde_json::Value = transcript
            .meta_json
            .as_deref()
            .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if matches!(
            existing_meta.get("resolved"),
            Some(serde_json::Value::Bool(true))
        ) {
            return Err(OrchestratorError::IllegalTransition(format!(
                "transcript {} already resolved",
                transcript.id
            )));
        }

        let loop_id = existing_meta
            .get("loop_id")
            .and_then(|value| value.as_str())
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("transcript {} missing loop_id", transcript.id))
            })?;
        let workflow_loop = WorkflowLoop::find_by_id(pool, loop_id)
            .await?
            .ok_or_else(|| OrchestratorError::NotFound(format!("loop {} not found", loop_id)))?;
        if existing_meta
            .get("review_kind")
            .and_then(|value| value.as_str())
            == Some("loop_skipped_retry_decision")
        {
            return Self::resolve_loop_skipped_retry_decision(
                pool,
                chat_runner,
                transcript,
                execution,
                step,
                workflow_session,
                &workflow_loop,
                &existing_meta,
                resolved_action,
            )
            .await;
        }
        if !matches!(
            resolved_action,
            "approved" | "approve" | "rejected" | "reject"
        ) {
            return Err(OrchestratorError::IllegalTransition(format!(
                "unsupported action '{}' for loop_review",
                resolved_action
            )));
        }
        let feedback = input_text
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        if matches!(resolved_action, "rejected" | "reject") && feedback.is_none() {
            return Err(OrchestratorError::IllegalTransition(
                "loop review rejection requires feedback".to_string(),
            ));
        }

        let updated_meta_json = Self::merge_transcript_meta(
            transcript.meta_json.as_deref(),
            serde_json::json!({
                "resolved": true,
                "resolved_action": resolved_action,
                "resolved_at": Utc::now().to_rfc3339(),
                "input_text": feedback,
            }),
        );
        let is_approved = matches!(resolved_action, "approved" | "approve");
        let decision_feedback = if is_approved {
            feedback.unwrap_or(Self::localized_user_approved_loop_result_message().await)
        } else {
            feedback.expect("loop rejection feedback validated before transaction")
        };
        let member_step_ids =
            serde_json::from_str::<Vec<Uuid>>(&workflow_loop.member_step_ids_json)?
                .into_iter()
                .collect::<HashSet<_>>();
        let member_steps = WorkflowStep::find_by_execution(pool, execution.id)
            .await?
            .into_iter()
            .filter(|member| member_step_ids.contains(&member.id))
            .collect::<Vec<_>>();
        let eligible_member_steps = member_steps
            .iter()
            .filter(|member| {
                !has_matching_active_skip_waiver(member, &workflow_loop, &decision_feedback)
            })
            .cloned()
            .collect::<Vec<_>>();
        let skipped_steps = eligible_member_steps
            .iter()
            .filter(|member| member.status == WorkflowStepStatus::Skipped)
            .cloned()
            .collect::<Vec<_>>();
        let retryable_step_ids = eligible_member_steps
            .iter()
            .filter(|member| member.status != WorkflowStepStatus::Skipped)
            .map(|member| member.id)
            .collect::<Vec<_>>();
        let all_rejected_targets_waived =
            !is_approved && !member_steps.is_empty() && eligible_member_steps.is_empty();

        let mut transaction = pool.begin().await?;
        let updated_transcript = Self::claim_review_transcript_in_transaction(
            &mut transaction,
            transcript,
            &updated_meta_json,
        )
        .await?;
        Self::save_step_review_in_transaction(
            &mut transaction,
            step,
            ReviewerType::User,
            None,
            if is_approved {
                ReviewVerdict::Approved
            } else {
                ReviewVerdict::Rejected
            },
            &decision_feedback,
        )
        .await?;

        let (final_step, final_loop, decision_transcript, projection_reason) = if is_approved
            || all_rejected_targets_waived
        {
            let precompleted = super::reducer::transition_step_in_transaction(
                &mut transaction,
                execution,
                step,
                WorkflowStepStatus::PreCompleted,
                Some(serde_json::json!({ "reason": "loop_user_review_precompleted" })),
            )
            .await?
            .entity;
            let completed = super::reducer::transition_step_in_transaction(
                &mut transaction,
                execution,
                &precompleted,
                WorkflowStepStatus::Completed,
                Some(serde_json::json!({ "reason": "loop_user_review_passed" })),
            )
            .await?
            .entity;
            let completed_loop = WorkflowLoop::update_status_if_current_in_transaction(
                &mut transaction,
                workflow_loop.id,
                WorkflowLoopStatus::WaitingUser,
                WorkflowLoopStatus::Completed,
                None,
            )
            .await?
            .ok_or_else(|| {
                OrchestratorError::IllegalTransition(format!(
                    "loop {} changed before approval",
                    workflow_loop.id
                ))
            })?;
            WorkflowEvent::create_in_transaction(
                &mut transaction,
                &CreateWorkflowEvent {
                    execution_id: execution.id,
                    round_id: Some(completed_loop.round_id),
                    step_id: Some(completed.id),
                    agent_session_id: completed.assigned_workflow_agent_session_id,
                    event_type: WorkflowEventType::LoopPassed,
                    status_before: Some(to_workflow_wire_value(&WorkflowLoopStatus::WaitingUser)),
                    status_after: Some(to_workflow_wire_value(&WorkflowLoopStatus::Completed)),
                    detail_json: Some(
                        serde_json::json!({
                            "feedback": decision_feedback,
                            "reason": if all_rejected_targets_waived {
                                "rejected_targets_covered_by_user_waiver"
                            } else {
                                "user_approved"
                            },
                        })
                        .to_string(),
                    ),
                },
                Uuid::new_v4(),
            )
            .await?;
            (
                completed,
                completed_loop,
                None,
                if all_rejected_targets_waived {
                    "loop_passed_by_user_waiver"
                } else {
                    "loop_user_review_passed"
                },
            )
        } else {
            let eligible_feedbacks = eligible_member_steps
                .iter()
                .map(|member| (member.step_key.clone(), decision_feedback.clone()))
                .collect::<std::collections::HashMap<_, _>>();
            if !eligible_feedbacks.is_empty() {
                super::super::workflow_loop_executor::inject_feedback_to_steps_in_transaction(
                    &mut transaction,
                    &workflow_loop,
                    WorkflowRevisionFeedbackSource::User,
                    &decision_feedback,
                    &eligible_feedbacks,
                )
                .await?;
            }
            if skipped_steps.is_empty() {
                let ready = super::reducer::transition_step_in_transaction(
                    &mut transaction,
                    execution,
                    step,
                    WorkflowStepStatus::Ready,
                    Some(serde_json::json!({ "reason": "loop_user_review_retry_prepared" })),
                )
                .await?
                .entity;
                let retry_loop = WorkflowLoop::increment_retry_if_current_in_transaction(
                    &mut transaction,
                    workflow_loop.id,
                    WorkflowLoopStatus::WaitingUser,
                    WorkflowLoopStatus::Running,
                    Some(decision_feedback.clone()),
                )
                .await?
                .ok_or_else(|| {
                    OrchestratorError::IllegalTransition(format!(
                        "loop {} changed before retry",
                        workflow_loop.id
                    ))
                })?;
                WorkflowEvent::create_in_transaction(
                    &mut transaction,
                    &CreateWorkflowEvent {
                        execution_id: execution.id,
                        round_id: Some(retry_loop.round_id),
                        step_id: Some(ready.id),
                        agent_session_id: ready.assigned_workflow_agent_session_id,
                        event_type: WorkflowEventType::LoopRetrying,
                        status_before: Some(to_workflow_wire_value(
                            &WorkflowLoopStatus::WaitingUser,
                        )),
                        status_after: Some(to_workflow_wire_value(&WorkflowLoopStatus::Running)),
                        detail_json: Some(
                            serde_json::json!({
                                "feedback": decision_feedback,
                                "retry_count": retry_loop.retry_count,
                            })
                            .to_string(),
                        ),
                    },
                    Uuid::new_v4(),
                )
                .await?;
                (ready, retry_loop, None, "loop_user_review_retry_prepared")
            } else {
                let waiting_loop = WorkflowLoop::update_status_if_current_in_transaction(
                    &mut transaction,
                    workflow_loop.id,
                    WorkflowLoopStatus::WaitingUser,
                    WorkflowLoopStatus::WaitingUser,
                    Some(decision_feedback.clone()),
                )
                .await?
                .ok_or_else(|| {
                    OrchestratorError::IllegalTransition(format!(
                        "loop {} changed before skipped decision",
                        workflow_loop.id
                    ))
                })?;
                let skipped_step_meta = skipped_steps
                    .iter()
                    .map(|member| {
                        serde_json::json!({
                            "step_id": member.id,
                            "step_key": member.step_key,
                            "title": member.title,
                            "issue_scope_id": loop_skip_issue_scope_id_for_feedback(
                                &workflow_loop,
                                member,
                                &decision_feedback,
                            ),
                            "feedback": decision_feedback,
                        })
                    })
                    .collect::<Vec<_>>();
                let decision = Self::write_transcript_in_transaction(
                    &mut transaction,
                    CreateWorkflowTranscript {
                        execution_id: execution.id,
                        round_id: Some(step.round_id),
                        workflow_agent_session_id: Some(workflow_session.id),
                        step_id: Some(step.id),
                        sender_type: "control".to_string(),
                        entry_type: "loop_review".to_string(),
                        content: "workflow.loop_skipped_retry_decision.request".to_string(),
                        meta_json: Some(serde_json::json!({
                            "resolved": false,
                            "review_kind": "loop_skipped_retry_decision",
                            "loop_id": waiting_loop.id,
                            "loop_key": waiting_loop.loop_key,
                            "feedback": decision_feedback,
                            "skipped_steps": skipped_step_meta,
                            "skipped_step_titles": skipped_steps.iter().map(|step| step.title.as_str()).collect::<Vec<_>>().join(", "),
                            "retryable_step_ids": retryable_step_ids,
                            "restart_effect": "rerun_skipped_steps_then_review",
                            "keep_effect": skipped_retry_keep_effect(&retryable_step_ids),
                            "source_transcript_id": updated_transcript.id,
                        }).to_string()),
                    },
                )
                .await?;
                WorkflowEvent::create_in_transaction(
                    &mut transaction,
                    &CreateWorkflowEvent {
                        execution_id: execution.id,
                        round_id: Some(waiting_loop.round_id),
                        step_id: Some(step.id),
                        agent_session_id: step.assigned_workflow_agent_session_id,
                        event_type: WorkflowEventType::LoopWaitingUser,
                        status_before: Some(to_workflow_wire_value(&WorkflowLoopStatus::WaitingUser)),
                        status_after: Some(to_workflow_wire_value(&WorkflowLoopStatus::WaitingUser)),
                        detail_json: Some(serde_json::json!({
                            "loop_id": waiting_loop.id,
                            "review_transcript_id": decision.id,
                            "reason": "skipped_retry_decision",
                            "skipped_step_ids": skipped_steps.iter().map(|step| step.id).collect::<Vec<_>>(),
                        }).to_string()),
                    },
                    Uuid::new_v4(),
                )
                .await?;
                (
                    step.clone(),
                    waiting_loop,
                    Some(decision),
                    "loop_waiting_skipped_retry_decision",
                )
            }
        };

        Self::write_transcript_in_transaction(
            &mut transaction,
            CreateWorkflowTranscript {
                execution_id: execution.id,
                round_id: Some(final_step.round_id),
                workflow_agent_session_id: Some(workflow_session.id),
                step_id: Some(final_step.id),
                sender_type: "user".to_string(),
                entry_type: "message".to_string(),
                content: decision_feedback.clone(),
                meta_json: Some(
                    serde_json::json!({
                        "source_transcript_id": updated_transcript.id,
                        "action": resolved_action,
                        "loop_id": final_loop.id,
                    })
                    .to_string(),
                ),
            },
        )
        .await?;
        transaction.commit().await?;

        workflow_analytics::track_review_decision_recorded(
            chat_runner.analytics_service(),
            execution.session_id,
            execution.id,
            step.id,
            if is_approved { "approved" } else { "rejected" },
            "user",
        );
        if let Some(decision) = decision_transcript.as_ref() {
            let inbox_message = Self::localized_loop_skipped_decision_inbox_message().await;
            InboxService::new()
                .notify_workflow_user_action(pool, execution, decision, Some(&inbox_message))
                .await;
        }
        let refreshed_execution = Self::refresh_review_state_after_commit(
            pool,
            chat_runner,
            execution,
            projection_reason,
            if skipped_steps.is_empty() {
                vec![final_step.id.to_string()]
            } else {
                skipped_steps
                    .iter()
                    .map(|member| member.id.to_string())
                    .collect()
            },
        )
        .await;
        let should_wake_scheduler =
            Self::should_wake_after_committed_review(pool, execution.id, updated_transcript.id)
                .await;

        Ok(ResolvedTranscriptAction {
            transcript: updated_transcript,
            execution: refreshed_execution,
            should_wake_scheduler,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn resolve_loop_skipped_retry_decision(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        transcript: &WorkflowTranscript,
        execution: &WorkflowExecution,
        review_step: &WorkflowStep,
        workflow_session: &WorkflowAgentSession,
        workflow_loop: &WorkflowLoop,
        meta: &serde_json::Value,
        resolved_action: &str,
    ) -> Result<ResolvedTranscriptAction, OrchestratorError> {
        if workflow_loop.status != WorkflowLoopStatus::WaitingUser {
            return Err(OrchestratorError::IllegalTransition(format!(
                "loop {} is {:?}, expected waiting_user",
                workflow_loop.id, workflow_loop.status
            )));
        }
        if review_step.status != WorkflowStepStatus::WaitingInput {
            return Err(OrchestratorError::IllegalTransition(format!(
                "loop review step {} is {:?}, expected waiting_input",
                review_step.id, review_step.status
            )));
        }
        if !matches!(resolved_action, "restart_skipped" | "keep_skipped") {
            return Err(OrchestratorError::IllegalTransition(format!(
                "unsupported action '{}' for skipped loop retry decision",
                resolved_action
            )));
        }

        let member_step_ids =
            serde_json::from_str::<Vec<Uuid>>(&workflow_loop.member_step_ids_json)?
                .into_iter()
                .collect::<HashSet<_>>();
        let skipped_targets = meta
            .get("skipped_steps")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .map(|item| {
                let step_id = item
                    .get("step_id")
                    .and_then(|value| value.as_str())
                    .and_then(|value| Uuid::parse_str(value).ok())
                    .ok_or_else(|| {
                        OrchestratorError::IllegalTransition(
                            "skipped retry decision contains an invalid step_id".to_string(),
                        )
                    })?;
                let feedback = item
                    .get("feedback")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string();
                let issue_scope_id = item
                    .get("issue_scope_id")
                    .and_then(|value| value.as_str())
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| {
                        OrchestratorError::IllegalTransition(
                            "skipped retry decision contains no issue_scope_id".to_string(),
                        )
                    })?
                    .to_string();
                Ok((step_id, issue_scope_id, feedback))
            })
            .collect::<Result<Vec<_>, OrchestratorError>>()?;
        if skipped_targets.is_empty() {
            return Err(OrchestratorError::IllegalTransition(
                "skipped retry decision has no skipped steps".to_string(),
            ));
        }
        let retryable_step_ids = meta
            .get("retryable_step_ids")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str())
            .filter_map(|value| Uuid::parse_str(value).ok())
            .filter(|step_id| member_step_ids.contains(step_id))
            .collect::<Vec<_>>();
        let loop_feedback = meta
            .get("feedback")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();

        let mut skipped_steps = Vec::with_capacity(skipped_targets.len());
        for (step_id, issue_scope_id, feedback) in &skipped_targets {
            if !member_step_ids.contains(step_id) {
                return Err(OrchestratorError::IllegalTransition(format!(
                    "step {} does not belong to loop {}",
                    step_id, workflow_loop.id
                )));
            }
            let step = WorkflowStep::find_by_id(pool, *step_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("step {} not found", step_id))
                })?;
            if step.status != WorkflowStepStatus::Skipped {
                return Err(OrchestratorError::IllegalTransition(format!(
                    "step {} is {:?}, expected skipped",
                    step.id, step.status
                )));
            }
            skipped_steps.push((step, issue_scope_id.clone(), feedback.clone()));
        }

        let changed_step_ids = skipped_steps
            .iter()
            .map(|(step, _, _)| step.id.to_string())
            .collect::<Vec<_>>();
        let updated_meta_json = Self::merge_transcript_meta(
            transcript.meta_json.as_deref(),
            serde_json::json!({
                "resolved": true,
                "resolved_action": resolved_action,
                "resolved_at": Utc::now().to_rfc3339(),
            }),
        );
        let superseded_contexts = skipped_steps
            .iter()
            .map(|(step, _, _)| {
                (
                    step.id,
                    supersede_loop_skip_waiver_context(
                        step.revision_context.as_deref(),
                        workflow_loop,
                        step,
                    )
                    .or_else(|| step.revision_context.clone()),
                )
            })
            .collect::<std::collections::HashMap<_, _>>();

        let mut transaction = pool.begin().await?;
        let updated_transcript = Self::claim_review_transcript_in_transaction(
            &mut transaction,
            transcript,
            &updated_meta_json,
        )
        .await?;
        let (resolved_review_step, resolved_loop, projection_reason) = match resolved_action {
            "restart_skipped" => {
                for (skipped_step, _, _) in &skipped_steps {
                    super::reducer::reopen_skipped_step_by_user(
                        &mut transaction,
                        execution,
                        workflow_loop,
                        skipped_step,
                        transcript,
                        superseded_contexts.get(&skipped_step.id).cloned().flatten(),
                    )
                    .await?;
                }
                let ready = super::reducer::transition_step_in_transaction(
                    &mut transaction,
                    execution,
                    review_step,
                    WorkflowStepStatus::Ready,
                    Some(serde_json::json!({
                        "reason": "loop_skipped_retry_review_prepared",
                    })),
                )
                .await?
                .entity;
                let retry_loop = WorkflowLoop::increment_retry_if_current_in_transaction(
                    &mut transaction,
                    workflow_loop.id,
                    WorkflowLoopStatus::WaitingUser,
                    WorkflowLoopStatus::Running,
                    Some(loop_feedback.clone()),
                )
                .await?
                .ok_or_else(|| {
                    OrchestratorError::IllegalTransition(format!(
                        "loop {} changed before restart decision",
                        workflow_loop.id
                    ))
                })?;
                WorkflowEvent::create_in_transaction(
                    &mut transaction,
                    &CreateWorkflowEvent {
                        execution_id: execution.id,
                        round_id: Some(retry_loop.round_id),
                        step_id: Some(ready.id),
                        agent_session_id: ready.assigned_workflow_agent_session_id,
                        event_type: WorkflowEventType::LoopRetrying,
                        status_before: Some(to_workflow_wire_value(
                            &WorkflowLoopStatus::WaitingUser,
                        )),
                        status_after: Some(to_workflow_wire_value(&WorkflowLoopStatus::Running)),
                        detail_json: Some(
                            serde_json::json!({
                                "reason": "skipped_steps_restarted_by_user",
                                "step_ids": changed_step_ids,
                                "retry_count": retry_loop.retry_count,
                            })
                            .to_string(),
                        ),
                    },
                    Uuid::new_v4(),
                )
                .await?;
                (ready, retry_loop, "loop_skipped_steps_restarted")
            }
            "keep_skipped" => {
                for (skipped_step, issue_scope_id, feedback) in &skipped_steps {
                    LoopExecutor::record_user_skip_waiver_in_transaction(
                        &mut transaction,
                        workflow_loop,
                        skipped_step,
                        issue_scope_id,
                        feedback,
                    )
                    .await?;
                }
                if retryable_step_ids.is_empty() {
                    let precompleted = super::reducer::transition_step_in_transaction(
                        &mut transaction,
                        execution,
                        review_step,
                        WorkflowStepStatus::PreCompleted,
                        Some(serde_json::json!({ "reason": "loop_skipped_waiver_precompleted" })),
                    )
                    .await?
                    .entity;
                    let completed = super::reducer::transition_step_in_transaction(
                        &mut transaction,
                        execution,
                        &precompleted,
                        WorkflowStepStatus::Completed,
                        Some(serde_json::json!({ "reason": "loop_skipped_waiver_completed" })),
                    )
                    .await?
                    .entity;
                    let completed_loop = WorkflowLoop::update_status_if_current_in_transaction(
                        &mut transaction,
                        workflow_loop.id,
                        WorkflowLoopStatus::WaitingUser,
                        WorkflowLoopStatus::Completed,
                        None,
                    )
                    .await?
                    .ok_or_else(|| {
                        OrchestratorError::IllegalTransition(format!(
                            "loop {} changed before keep decision",
                            workflow_loop.id
                        ))
                    })?;
                    WorkflowEvent::create_in_transaction(
                        &mut transaction,
                        &CreateWorkflowEvent {
                            execution_id: execution.id,
                            round_id: Some(completed_loop.round_id),
                            step_id: Some(completed.id),
                            agent_session_id: completed.assigned_workflow_agent_session_id,
                            event_type: WorkflowEventType::LoopPassed,
                            status_before: Some(to_workflow_wire_value(
                                &WorkflowLoopStatus::WaitingUser,
                            )),
                            status_after: Some(to_workflow_wire_value(
                                &WorkflowLoopStatus::Completed,
                            )),
                            detail_json: Some(
                                serde_json::json!({
                                    "reason": "all_rejected_skipped_steps_kept_by_user",
                                    "waived_step_ids": changed_step_ids,
                                    "forced": true,
                                })
                                .to_string(),
                            ),
                        },
                        Uuid::new_v4(),
                    )
                    .await?;
                    (
                        completed,
                        completed_loop,
                        "loop_skipped_steps_kept_completed",
                    )
                } else {
                    let ready = super::reducer::transition_step_in_transaction(
                        &mut transaction,
                        execution,
                        review_step,
                        WorkflowStepStatus::Ready,
                        Some(serde_json::json!({ "reason": "loop_kept_skipped_review_prepared" })),
                    )
                    .await?
                    .entity;
                    let retry_loop = WorkflowLoop::increment_retry_if_current_in_transaction(
                        &mut transaction,
                        workflow_loop.id,
                        WorkflowLoopStatus::WaitingUser,
                        WorkflowLoopStatus::Running,
                        Some(loop_feedback.clone()),
                    )
                    .await?
                    .ok_or_else(|| {
                        OrchestratorError::IllegalTransition(format!(
                            "loop {} changed before keep-and-retry decision",
                            workflow_loop.id
                        ))
                    })?;
                    WorkflowEvent::create_in_transaction(
                        &mut transaction,
                        &CreateWorkflowEvent {
                            execution_id: execution.id,
                            round_id: Some(retry_loop.round_id),
                            step_id: Some(ready.id),
                            agent_session_id: ready.assigned_workflow_agent_session_id,
                            event_type: WorkflowEventType::LoopRetrying,
                            status_before: Some(to_workflow_wire_value(
                                &WorkflowLoopStatus::WaitingUser,
                            )),
                            status_after: Some(to_workflow_wire_value(
                                &WorkflowLoopStatus::Running,
                            )),
                            detail_json: Some(
                                serde_json::json!({
                                    "reason": "skipped_steps_kept_by_user",
                                    "waived_step_ids": changed_step_ids,
                                    "retryable_step_ids": retryable_step_ids,
                                    "retry_count": retry_loop.retry_count,
                                })
                                .to_string(),
                            ),
                        },
                        Uuid::new_v4(),
                    )
                    .await?;
                    (ready, retry_loop, "loop_skipped_steps_kept_retrying_others")
                }
            }
            _ => unreachable!("skipped loop retry action validated before claiming transcript"),
        };

        WorkflowEvent::create_in_transaction(
            &mut transaction,
            &CreateWorkflowEvent {
                execution_id: execution.id,
                round_id: Some(resolved_loop.round_id),
                step_id: Some(resolved_review_step.id),
                agent_session_id: resolved_review_step.assigned_workflow_agent_session_id,
                event_type: WorkflowEventType::LoopUserDecisionRecorded,
                status_before: Some(to_workflow_wire_value(&WorkflowLoopStatus::WaitingUser)),
                status_after: Some(to_workflow_wire_value(&resolved_loop.status)),
                detail_json: Some(
                    serde_json::json!({
                        "loop_id": resolved_loop.id,
                        "review_transcript_id": updated_transcript.id,
                        "action": resolved_action,
                        "target_step_ids": changed_step_ids,
                    })
                    .to_string(),
                ),
            },
            Uuid::new_v4(),
        )
        .await?;
        Self::write_transcript_in_transaction(
            &mut transaction,
            CreateWorkflowTranscript {
                execution_id: execution.id,
                round_id: Some(resolved_review_step.round_id),
                workflow_agent_session_id: Some(workflow_session.id),
                step_id: Some(resolved_review_step.id),
                sender_type: "user".to_string(),
                entry_type: "message".to_string(),
                content: format!("workflow.loop_skipped_retry_decision.result.{resolved_action}"),
                meta_json: Some(
                    serde_json::json!({
                        "display_key": "workflow.loop_skipped_retry_decision.result",
                        "source_transcript_id": updated_transcript.id,
                        "action": resolved_action,
                        "loop_id": resolved_loop.id,
                        "target_step_ids": changed_step_ids,
                    })
                    .to_string(),
                ),
            },
        )
        .await?;
        transaction.commit().await?;

        let refreshed_execution = Self::refresh_review_state_after_commit(
            pool,
            chat_runner,
            execution,
            projection_reason,
            changed_step_ids,
        )
        .await;
        let should_wake_scheduler =
            Self::should_wake_after_committed_review(pool, execution.id, updated_transcript.id)
                .await;

        Ok(ResolvedTranscriptAction {
            transcript: updated_transcript,
            execution: refreshed_execution,
            should_wake_scheduler,
        })
    }

    async fn localized_user_approved_loop_result_message() -> String {
        let ui_config = config::load_config_from_file(&config_path()).await;
        Self::localized_user_approved_loop_result_message_for_language(&ui_config.language)
            .to_string()
    }

    pub(crate) async fn localized_loop_skipped_decision_inbox_message() -> String {
        let ui_config = config::load_config_from_file(&config_path()).await;
        Self::localized_loop_skipped_decision_inbox_message_for_language(&ui_config.language)
            .to_string()
    }

    async fn has_other_unresolved_step_or_loop_reviews(
        pool: &SqlitePool,
        execution_id: Uuid,
        resolved_transcript_id: Uuid,
    ) -> Result<bool, OrchestratorError> {
        Self::has_unresolved_step_or_loop_reviews(pool, execution_id, Some(resolved_transcript_id))
            .await
    }

    async fn refresh_review_state_after_commit(
        pool: &SqlitePool,
        chat_runner: &ChatRunner,
        execution: &WorkflowExecution,
        reason: &str,
        changed_step_ids: Vec<String>,
    ) -> WorkflowExecution {
        let refreshed = match Self::synchronize_runtime_state(pool, execution.id, false).await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                tracing::error!(
                    execution_id = %execution.id,
                    reason,
                    error = %error,
                    "review transaction committed but runtime synchronization failed"
                );
                execution.clone()
            }
        };
        if let Err(error) = Self::refresh_execution_projection_with_reason(
            pool,
            chat_runner,
            execution.id,
            None,
            reason,
            changed_step_ids,
        )
        .await
        {
            tracing::error!(
                execution_id = %execution.id,
                reason,
                error = %error,
                "review transaction committed but projection refresh failed; DB truth remains recoverable"
            );
        }
        refreshed
    }

    async fn should_wake_after_committed_review(
        pool: &SqlitePool,
        execution_id: Uuid,
        transcript_id: Uuid,
    ) -> bool {
        match Self::has_other_unresolved_step_or_loop_reviews(pool, execution_id, transcript_id)
            .await
        {
            Ok(has_other) => {
                if has_other {
                    tracing::debug!(
                        execution_id = %execution_id,
                        transcript_id = %transcript_id,
                        "committed review still has an unresolved review; scheduler recovery will refresh and park"
                    );
                }
            }
            Err(error) => {
                tracing::error!(
                    execution_id = %execution_id,
                    transcript_id = %transcript_id,
                    error = %error,
                    "failed to check unresolved reviews after committed decision; waking scheduler for recovery"
                );
            }
        }
        // Waking is safe even when another review remains unresolved: the
        // scheduler checks that durable guard before dispatching work and
        // refreshes the projection before parking. Always waking also recovers
        // post-commit synchronization/projection failures.
        true
    }

    fn localized_user_approved_loop_result_message_for_language(
        language: &UiLanguage,
    ) -> &'static str {
        match language {
            UiLanguage::Browser => sys_locale::get_locale()
                .as_deref()
                .and_then(Self::localized_user_approved_loop_result_message_for_locale)
                .unwrap_or("User approved the loop result."),
            UiLanguage::En => "User approved the loop result.",
            UiLanguage::Fr => "L'utilisateur a approuvé le résultat de la boucle.",
            UiLanguage::Ja => "ユーザーがループ結果を承認しました。",
            UiLanguage::Es => "El usuario aprobó el resultado del bucle.",
            UiLanguage::Ko => "사용자가 루프 결과를 승인했습니다.",
            UiLanguage::ZhHans => "用户已批准循环结果。",
            UiLanguage::ZhHant => "用戶已批准循環結果。",
        }
    }

    fn localized_loop_skipped_decision_inbox_message_for_language(
        language: &UiLanguage,
    ) -> &'static str {
        match language {
            UiLanguage::Browser => sys_locale::get_locale()
                .as_deref()
                .and_then(Self::localized_loop_skipped_decision_inbox_message_for_locale)
                .unwrap_or("Choose how to handle the skipped loop steps."),
            UiLanguage::En => "Choose how to handle the skipped loop steps.",
            UiLanguage::Fr => "Choisissez comment traiter les étapes ignorées de la boucle.",
            UiLanguage::Ja => "スキップされたループ手順の処理方法を選択してください。",
            UiLanguage::Es => "Elige cómo gestionar los pasos omitidos del bucle.",
            UiLanguage::Ko => "건너뛴 루프 단계를 처리할 방법을 선택하세요.",
            UiLanguage::ZhHans => "请选择如何处理循环中已跳过的节点。",
            UiLanguage::ZhHant => "請選擇如何處理循環中已跳過的節點。",
        }
    }

    fn localized_user_approved_loop_result_message_for_locale(
        locale: &str,
    ) -> Option<&'static str> {
        let normalized = locale.trim().to_ascii_lowercase().replace('_', "-");
        if normalized.is_empty() {
            return None;
        }
        if normalized.starts_with("zh-hant")
            || normalized.starts_with("zh-tw")
            || normalized.starts_with("zh-hk")
            || normalized.starts_with("zh-mo")
        {
            return Some("用戶已批准循環結果。");
        }
        if normalized.starts_with("zh") {
            return Some("用户已批准循环结果。");
        }
        if normalized.starts_with("ja") {
            return Some("ユーザーがループ結果を承認しました。");
        }
        if normalized.starts_with("ko") {
            return Some("사용자가 루프 결과를 승인했습니다.");
        }
        if normalized.starts_with("fr") {
            return Some("L'utilisateur a approuvé le résultat de la boucle.");
        }
        if normalized.starts_with("es") {
            return Some("El usuario aprobó el resultado del bucle.");
        }
        if normalized.starts_with("en") {
            return Some("User approved the loop result.");
        }
        None
    }

    fn localized_loop_skipped_decision_inbox_message_for_locale(
        locale: &str,
    ) -> Option<&'static str> {
        let normalized = locale.trim().to_ascii_lowercase().replace('_', "-");
        if normalized.is_empty() {
            return None;
        }
        if normalized.starts_with("zh-hant")
            || normalized.starts_with("zh-tw")
            || normalized.starts_with("zh-hk")
            || normalized.starts_with("zh-mo")
        {
            return Some("請選擇如何處理循環中已跳過的節點。");
        }
        if normalized.starts_with("zh") {
            return Some("请选择如何处理循环中已跳过的节点。");
        }
        if normalized.starts_with("ja") {
            return Some("スキップされたループ手順の処理方法を選択してください。");
        }
        if normalized.starts_with("ko") {
            return Some("건너뛴 루프 단계를 처리할 방법을 선택하세요.");
        }
        if normalized.starts_with("fr") {
            return Some("Choisissez comment traiter les étapes ignorées de la boucle.");
        }
        if normalized.starts_with("es") {
            return Some("Elige cómo gestionar los pasos omitidos del bucle.");
        }
        if normalized.starts_with("en") {
            return Some("Choose how to handle the skipped loop steps.");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sqlx::SqlitePool;

    use super::{
        super::super::workflow_loop_executor::{loop_skip_waiver, merge_loop_skip_waiver_context},
        *,
    };

    async fn setup_workflow_events_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create sqlite memory pool");
        sqlx::query(
            r#"
            CREATE TABLE chat_workflow_events (
                id TEXT PRIMARY KEY,
                execution_id TEXT NOT NULL,
                round_id TEXT,
                step_id TEXT,
                agent_session_id TEXT,
                event_type TEXT NOT NULL,
                status_before TEXT,
                status_after TEXT,
                detail_json TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create workflow events table");
        pool
    }

    async fn setup_skipped_decision_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("create sqlite memory pool");
        sqlx::query(
            r#"
            CREATE TABLE chat_workflow_steps (
                id BLOB PRIMARY KEY, execution_id BLOB NOT NULL, round_id BLOB NOT NULL,
                compiled_revision_id BLOB, step_key TEXT NOT NULL, step_type TEXT NOT NULL,
                title TEXT NOT NULL, instructions TEXT NOT NULL,
                assigned_workflow_agent_session_id BLOB, status TEXT NOT NULL,
                retry_count INTEGER NOT NULL DEFAULT 0, max_retry INTEGER NOT NULL DEFAULT 1,
                round_index INTEGER NOT NULL, display_order INTEGER NOT NULL,
                latest_run_id BLOB, summary_text TEXT, content TEXT, loop_id BLOB,
                lead_review_required INTEGER NOT NULL DEFAULT 0,
                user_review_required INTEGER NOT NULL DEFAULT 0,
                revision_context TEXT, created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL, started_at TEXT, completed_at TEXT
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create workflow steps");
        sqlx::query(
            r#"
            CREATE TABLE chat_workflow_events (
                id BLOB PRIMARY KEY, execution_id BLOB NOT NULL, round_id BLOB,
                step_id BLOB, agent_session_id BLOB, event_type TEXT NOT NULL,
                status_before TEXT, status_after TEXT, detail_json TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create workflow events");
        sqlx::query(
            r#"
            CREATE TABLE chat_workflow_transcripts (
                id BLOB PRIMARY KEY, execution_id BLOB NOT NULL, round_id BLOB,
                workflow_agent_session_id BLOB, step_id BLOB, sender_type TEXT NOT NULL,
                entry_type TEXT NOT NULL, content TEXT NOT NULL, meta_json TEXT,
                created_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create workflow transcripts");
        sqlx::query(
            r#"
            CREATE TABLE chat_workflow_loops (
                id BLOB PRIMARY KEY, execution_id BLOB NOT NULL, round_id BLOB NOT NULL,
                loop_key TEXT NOT NULL, review_step_id BLOB NOT NULL,
                member_step_ids_json TEXT NOT NULL, status TEXT NOT NULL,
                retry_count INTEGER NOT NULL DEFAULT 0, max_retry INTEGER NOT NULL DEFAULT 3,
                user_review_required INTEGER NOT NULL DEFAULT 0, rejection_reason TEXT,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create workflow loops");
        sqlx::query(
            r#"
            CREATE TABLE chat_workflow_executions (
                id BLOB PRIMARY KEY, session_id BLOB NOT NULL, plan_id BLOB NOT NULL,
                active_revision_id BLOB, active_round_id BLOB, workflow_card_message_id BLOB,
                lead_session_agent_id BLOB, status TEXT NOT NULL, current_round INTEGER NOT NULL,
                title TEXT NOT NULL, compiled_graph_hash TEXT, started_at TEXT, completed_at TEXT,
                cleaned_at TEXT, cleaned_reason TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create workflow executions");
        sqlx::query(
            r#"
            CREATE TABLE chat_workflow_step_reviews (
                id BLOB PRIMARY KEY, step_id BLOB NOT NULL, execution_id BLOB NOT NULL,
                reviewer_type TEXT NOT NULL, reviewer_id TEXT, verdict TEXT NOT NULL,
                feedback TEXT NOT NULL, review_round INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now', 'subsec'))
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("create workflow step reviews");
        pool
    }

    async fn insert_skipped_step(
        pool: &SqlitePool,
        execution_id: Uuid,
        round_id: Uuid,
        loop_id: Uuid,
    ) -> WorkflowStep {
        let step_id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO chat_workflow_steps (
                id, execution_id, round_id, step_key, step_type, title, instructions,
                status, retry_count, max_retry, round_index, display_order, loop_id,
                lead_review_required, user_review_required, created_at, updated_at, completed_at
            ) VALUES (?1, ?2, ?3, 'skipped-task', 'task', 'Skipped task', '',
                      'skipped', 0, 2, 1, 1, ?4, 0, 0, ?5, ?5, ?5)
            "#,
        )
        .bind(step_id)
        .bind(execution_id)
        .bind(round_id)
        .bind(loop_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert skipped step");
        WorkflowStep::find_by_id(pool, step_id)
            .await
            .expect("find skipped step")
            .expect("skipped step exists")
    }

    async fn insert_review_step(
        pool: &SqlitePool,
        execution_id: Uuid,
        round_id: Uuid,
        loop_id: Uuid,
    ) -> WorkflowStep {
        let step_id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO chat_workflow_steps (
                id, execution_id, round_id, step_key, step_type, title, instructions,
                status, retry_count, max_retry, round_index, display_order, loop_id,
                lead_review_required, user_review_required, created_at, updated_at, started_at
            ) VALUES (?1, ?2, ?3, 'review', 'review', 'Review', '',
                      'waiting_input', 0, 3, 1, 2, ?4, 1, 1, ?5, ?5, ?5)
            "#,
        )
        .bind(step_id)
        .bind(execution_id)
        .bind(round_id)
        .bind(loop_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert review step");
        WorkflowStep::find_by_id(pool, step_id)
            .await
            .expect("find review step")
            .expect("review step exists")
    }

    async fn insert_completed_step(
        pool: &SqlitePool,
        execution_id: Uuid,
        round_id: Uuid,
        loop_id: Uuid,
        step_key: &str,
    ) -> WorkflowStep {
        let step_id = Uuid::new_v4();
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO chat_workflow_steps (
                id, execution_id, round_id, step_key, step_type, title, instructions,
                status, retry_count, max_retry, round_index, display_order, loop_id,
                lead_review_required, user_review_required, created_at, updated_at,
                started_at, completed_at
            ) VALUES (?1, ?2, ?3, ?4, 'task', ?4, '',
                      'completed', 0, 2, 1, 1, ?5, 0, 0, ?6, ?6, ?6, ?6)
            "#,
        )
        .bind(step_id)
        .bind(execution_id)
        .bind(round_id)
        .bind(step_key)
        .bind(loop_id)
        .bind(now)
        .execute(pool)
        .await
        .expect("insert completed step");
        WorkflowStep::find_by_id(pool, step_id)
            .await
            .expect("find completed step")
            .expect("completed step exists")
    }

    async fn persist_waiting_execution(pool: &SqlitePool, execution: &WorkflowExecution) {
        sqlx::query(
            r#"
            INSERT INTO chat_workflow_executions (
                id, session_id, plan_id, active_revision_id, active_round_id,
                workflow_card_message_id, lead_session_agent_id, status, current_round,
                title, compiled_graph_hash, started_at, completed_at, cleaned_at,
                cleaned_reason, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
            "#,
        )
        .bind(execution.id)
        .bind(execution.session_id)
        .bind(execution.plan_id)
        .bind(execution.active_revision_id)
        .bind(execution.active_round_id)
        .bind(execution.workflow_card_message_id)
        .bind(execution.lead_session_agent_id)
        .bind(&execution.status)
        .bind(execution.current_round)
        .bind(&execution.title)
        .bind(&execution.compiled_graph_hash)
        .bind(execution.started_at)
        .bind(execution.completed_at)
        .bind(execution.cleaned_at)
        .bind(&execution.cleaned_reason)
        .bind(execution.created_at)
        .bind(execution.updated_at)
        .execute(pool)
        .await
        .expect("persist execution");
    }

    async fn persist_waiting_loop(pool: &SqlitePool, workflow_loop: &WorkflowLoop) {
        sqlx::query(
            r#"
            INSERT INTO chat_workflow_loops (
                id, execution_id, round_id, loop_key, review_step_id,
                member_step_ids_json, status, retry_count, max_retry,
                user_review_required, rejection_reason, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            "#,
        )
        .bind(workflow_loop.id)
        .bind(workflow_loop.execution_id)
        .bind(workflow_loop.round_id)
        .bind(&workflow_loop.loop_key)
        .bind(workflow_loop.review_step_id)
        .bind(&workflow_loop.member_step_ids_json)
        .bind(&workflow_loop.status)
        .bind(workflow_loop.retry_count)
        .bind(workflow_loop.max_retry)
        .bind(workflow_loop.user_review_required)
        .bind(&workflow_loop.rejection_reason)
        .bind(workflow_loop.created_at)
        .bind(workflow_loop.updated_at)
        .execute(pool)
        .await
        .expect("persist workflow loop");
    }

    fn sample_waiting_execution(round_id: Uuid) -> WorkflowExecution {
        let now = Utc::now();
        WorkflowExecution {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            plan_id: Uuid::new_v4(),
            active_revision_id: Some(Uuid::new_v4()),
            active_round_id: Some(round_id),
            workflow_card_message_id: None,
            lead_session_agent_id: None,
            status: WorkflowExecutionStatus::Waiting,
            current_round: 1,
            title: "Review execution".to_string(),
            compiled_graph_hash: None,
            started_at: Some(now),
            completed_at: None,
            cleaned_at: None,
            cleaned_reason: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn final_review_decision_domain_events_are_persisted_with_consumable_status() {
        let pool = setup_workflow_events_pool().await;
        let round_id = Uuid::new_v4();
        let execution = sample_waiting_execution(round_id);

        let accepted = WorkflowOrchestrator::emit_final_review_decision_event(
            &pool,
            &execution,
            WorkflowEventType::UserAccepted,
            "user_accepted",
        )
        .await
        .expect("persist accepted event");
        let rejected = WorkflowOrchestrator::emit_final_review_decision_event(
            &pool,
            &execution,
            WorkflowEventType::UserRejected,
            "user_rejected",
        )
        .await
        .expect("persist rejected event");

        assert_eq!(accepted.event_type, WorkflowEventType::UserAccepted);
        assert_eq!(accepted.round_id, Some(round_id));
        assert_eq!(accepted.status_before.as_deref(), Some("waiting"));
        assert_eq!(accepted.status_after.as_deref(), Some("user_accepted"));
        assert_eq!(rejected.event_type, WorkflowEventType::UserRejected);
        assert_eq!(rejected.status_after.as_deref(), Some("user_rejected"));
        assert_eq!(
            accepted.detail_json.as_deref(),
            Some(r#"{"resolution":"user_accepted"}"#)
        );
        assert_eq!(
            rejected.detail_json.as_deref(),
            Some(r#"{"resolution":"user_rejected"}"#)
        );
    }

    #[tokio::test]
    async fn skipped_reopen_and_transcript_claim_roll_back_together() {
        let pool = setup_skipped_decision_pool().await;
        let round_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let execution = sample_waiting_execution(round_id);
        let step = insert_skipped_step(&pool, execution.id, round_id, loop_id).await;
        let workflow_loop = WorkflowLoop {
            id: loop_id,
            execution_id: execution.id,
            round_id,
            loop_key: "loop-a".to_string(),
            review_step_id: Uuid::new_v4(),
            member_step_ids_json: serde_json::to_string(&vec![step.id]).expect("member JSON"),
            status: WorkflowLoopStatus::WaitingUser,
            retry_count: 0,
            max_retry: 3,
            user_review_required: false,
            rejection_reason: Some("feedback".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let transcript = WorkflowTranscript::create(
            &pool,
            &CreateWorkflowTranscript {
                execution_id: execution.id,
                round_id: Some(round_id),
                workflow_agent_session_id: None,
                step_id: Some(workflow_loop.review_step_id),
                sender_type: "control".to_string(),
                entry_type: "loop_review".to_string(),
                content: "workflow.loop_skipped_retry_decision.request".to_string(),
                meta_json: Some(
                    serde_json::json!({
                        "resolved": false,
                        "review_kind": "loop_skipped_retry_decision",
                        "loop_id": loop_id,
                    })
                    .to_string(),
                ),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create decision transcript");

        let mut transaction = pool.begin().await.expect("begin transaction");
        WorkflowTranscript::update_meta_json_if_unresolved_in_transaction(
            &mut transaction,
            transcript.id,
            &serde_json::json!({ "resolved": true }).to_string(),
        )
        .await
        .expect("claim transcript")
        .expect("unresolved transcript");
        super::super::reducer::reopen_skipped_step_by_user(
            &mut transaction,
            &execution,
            &workflow_loop,
            &step,
            &transcript,
            None,
        )
        .await
        .expect("reopen skipped step");
        sqlx::query("INSERT INTO table_that_does_not_exist DEFAULT VALUES")
            .execute(&mut *transaction)
            .await
            .expect_err("force transaction failure");
        transaction.rollback().await.expect("rollback transaction");

        let persisted_step = WorkflowStep::find_by_id(&pool, step.id)
            .await
            .expect("find step after rollback")
            .expect("step remains");
        assert_eq!(persisted_step.status, WorkflowStepStatus::Skipped);
        assert!(
            WorkflowTranscript::update_meta_json_if_unresolved(
                &pool,
                transcript.id,
                &serde_json::json!({ "resolved": true }).to_string(),
            )
            .await
            .expect("claim transcript after rollback")
            .is_some()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM chat_workflow_events")
                .fetch_one(&pool)
                .await
                .expect("count events"),
            0
        );
    }

    #[tokio::test]
    async fn keep_skipped_completes_loop_and_persists_waiver_atomically() {
        let pool = setup_skipped_decision_pool().await;
        let round_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let execution = sample_waiting_execution(round_id);
        persist_waiting_execution(&pool, &execution).await;
        let skipped_step = insert_skipped_step(&pool, execution.id, round_id, loop_id).await;
        let review_step = insert_review_step(&pool, execution.id, round_id, loop_id).await;
        let workflow_loop = WorkflowLoop {
            id: loop_id,
            execution_id: execution.id,
            round_id,
            loop_key: "loop-a".to_string(),
            review_step_id: review_step.id,
            member_step_ids_json: serde_json::to_string(&vec![skipped_step.id])
                .expect("member JSON"),
            status: WorkflowLoopStatus::WaitingUser,
            retry_count: 1,
            max_retry: 3,
            user_review_required: false,
            rejection_reason: Some("missing evidence".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        persist_waiting_loop(&pool, &workflow_loop).await;
        let transcript_meta = serde_json::json!({
            "resolved": false,
            "review_kind": "loop_skipped_retry_decision",
            "loop_id": loop_id,
            "feedback": "missing evidence",
            "skipped_steps": [{
                "step_id": skipped_step.id,
                "issue_scope_id": loop_skip_issue_scope_id_for_feedback(
                    &workflow_loop,
                    &skipped_step,
                    "missing evidence",
                ),
                "feedback": "missing evidence",
            }],
            "retryable_step_ids": [],
        });
        let transcript = WorkflowTranscript::create(
            &pool,
            &CreateWorkflowTranscript {
                execution_id: execution.id,
                round_id: Some(round_id),
                workflow_agent_session_id: None,
                step_id: Some(review_step.id),
                sender_type: "control".to_string(),
                entry_type: "loop_review".to_string(),
                content: "workflow.loop_skipped_retry_decision.request".to_string(),
                meta_json: Some(transcript_meta.to_string()),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create decision transcript");
        let workflow_session = WorkflowAgentSession {
            id: Uuid::new_v4(),
            workflow_execution_id: execution.id,
            session_agent_id: Uuid::new_v4(),
            role: WorkflowAgentSessionRole::Reviewer,
            agent_session_id: None,
            agent_message_id: None,
            state: WorkflowAgentSessionState::Idle,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let chat_runner = ChatRunner::new(DBService { pool: pool.clone() });

        let outcome = WorkflowOrchestrator::resolve_loop_skipped_retry_decision(
            &pool,
            &chat_runner,
            &transcript,
            &execution,
            &review_step,
            &workflow_session,
            &workflow_loop,
            &transcript_meta,
            "keep_skipped",
        )
        .await
        .expect("keep skipped decision");

        assert!(
            outcome
                .transcript
                .meta_json
                .as_deref()
                .is_some_and(|meta| meta.contains("keep_skipped"))
        );
        let persisted_review = WorkflowStep::find_by_id(&pool, review_step.id)
            .await
            .expect("find review step")
            .expect("review step exists");
        assert_eq!(persisted_review.status, WorkflowStepStatus::Completed);
        let persisted_skipped = WorkflowStep::find_by_id(&pool, skipped_step.id)
            .await
            .expect("find skipped step")
            .expect("skipped step exists");
        assert_eq!(persisted_skipped.status, WorkflowStepStatus::Skipped);
        assert!(has_matching_active_skip_waiver(
            &persisted_skipped,
            &workflow_loop,
            "missing evidence"
        ));
        let persisted_loop = WorkflowLoop::find_by_id(&pool, loop_id)
            .await
            .expect("find loop")
            .expect("loop exists");
        assert_eq!(persisted_loop.status, WorkflowLoopStatus::Completed);
        let events = WorkflowEvent::find_by_execution(&pool, execution.id)
            .await
            .expect("find events");
        assert!(
            events
                .iter()
                .any(|event| { event.event_type == WorkflowEventType::LoopUserDecisionRecorded })
        );
        assert!(
            events
                .iter()
                .any(|event| event.event_type == WorkflowEventType::LoopPassed)
        );
    }

    #[tokio::test]
    async fn user_loop_rejection_with_skipped_member_atomically_creates_decision_entry() {
        let pool = setup_skipped_decision_pool().await;
        let round_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let execution = sample_waiting_execution(round_id);
        persist_waiting_execution(&pool, &execution).await;
        let skipped_step = insert_skipped_step(&pool, execution.id, round_id, loop_id).await;
        let review_step = insert_review_step(&pool, execution.id, round_id, loop_id).await;
        let workflow_loop = WorkflowLoop {
            id: loop_id,
            execution_id: execution.id,
            round_id,
            loop_key: "loop-a".to_string(),
            review_step_id: review_step.id,
            member_step_ids_json: serde_json::to_string(&vec![skipped_step.id])
                .expect("member JSON"),
            status: WorkflowLoopStatus::WaitingUser,
            retry_count: 0,
            max_retry: 3,
            user_review_required: true,
            rejection_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        persist_waiting_loop(&pool, &workflow_loop).await;
        let transcript = WorkflowTranscript::create(
            &pool,
            &CreateWorkflowTranscript {
                execution_id: execution.id,
                round_id: Some(round_id),
                workflow_agent_session_id: None,
                step_id: Some(review_step.id),
                sender_type: "control".to_string(),
                entry_type: "loop_review".to_string(),
                content: "review loop".to_string(),
                meta_json: Some(
                    serde_json::json!({
                        "resolved": false,
                        "review_kind": "loop_user_review",
                        "loop_id": loop_id,
                    })
                    .to_string(),
                ),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create loop review transcript");
        let workflow_session = WorkflowAgentSession {
            id: Uuid::new_v4(),
            workflow_execution_id: execution.id,
            session_agent_id: Uuid::new_v4(),
            role: WorkflowAgentSessionRole::Reviewer,
            agent_session_id: None,
            agent_message_id: None,
            state: WorkflowAgentSessionState::Idle,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let chat_runner = ChatRunner::new(DBService { pool: pool.clone() });

        let outcome = WorkflowOrchestrator::resolve_loop_review_action(
            &pool,
            &chat_runner,
            &transcript,
            &execution,
            &review_step,
            &workflow_session,
            "reject",
            Some("missing evidence"),
        )
        .await
        .expect("reject loop review");

        assert!(outcome.should_wake_scheduler);
        assert_eq!(
            WorkflowLoop::find_by_id(&pool, loop_id)
                .await
                .expect("find loop")
                .expect("loop exists")
                .status,
            WorkflowLoopStatus::WaitingUser
        );
        assert_eq!(
            WorkflowStep::find_by_id(&pool, review_step.id)
                .await
                .expect("find review step")
                .expect("review step exists")
                .status,
            WorkflowStepStatus::WaitingInput
        );
        assert!(
            WorkflowStep::find_by_id(&pool, skipped_step.id)
                .await
                .expect("find skipped step")
                .expect("skipped step exists")
                .revision_context
                .as_deref()
                .is_some_and(|context| context.contains("pending_feedback"))
        );
        let unresolved =
            WorkflowTranscript::find_unresolved_reviews_by_execution(&pool, execution.id)
                .await
                .expect("find unresolved reviews");
        assert_eq!(unresolved.len(), 1);
        assert!(
            unresolved[0]
                .meta_json
                .as_deref()
                .is_some_and(|meta| meta.contains("loop_skipped_retry_decision"))
        );
        assert!(
            WorkflowEvent::find_by_execution(&pool, execution.id)
                .await
                .expect("find events")
                .iter()
                .any(|event| event.event_type == WorkflowEventType::LoopWaitingUser)
        );
    }

    #[tokio::test]
    async fn restart_skipped_reopens_only_through_decision_and_rejects_duplicate_submit() {
        let pool = setup_skipped_decision_pool().await;
        let round_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let execution = sample_waiting_execution(round_id);
        persist_waiting_execution(&pool, &execution).await;
        let mut skipped_step = insert_skipped_step(&pool, execution.id, round_id, loop_id).await;
        let review_step = insert_review_step(&pool, execution.id, round_id, loop_id).await;
        let workflow_loop = WorkflowLoop {
            id: loop_id,
            execution_id: execution.id,
            round_id,
            loop_key: "loop-a".to_string(),
            review_step_id: review_step.id,
            member_step_ids_json: serde_json::to_string(&vec![skipped_step.id])
                .expect("member JSON"),
            status: WorkflowLoopStatus::WaitingUser,
            retry_count: 1,
            max_retry: 3,
            user_review_required: false,
            rejection_reason: Some("new issue".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        persist_waiting_loop(&pool, &workflow_loop).await;
        skipped_step.revision_context = Some(merge_loop_skip_waiver_context(
            None,
            &workflow_loop,
            &skipped_step,
            "old issue",
        ));
        WorkflowStep::update_revision_context(
            &pool,
            skipped_step.id,
            skipped_step.revision_context.clone(),
        )
        .await
        .expect("seed old waiver");
        let transcript_meta = serde_json::json!({
            "resolved": false,
            "review_kind": "loop_skipped_retry_decision",
            "loop_id": loop_id,
            "feedback": "new issue",
            "skipped_steps": [{
                "step_id": skipped_step.id,
                "issue_scope_id": loop_skip_issue_scope_id_for_feedback(
                    &workflow_loop,
                    &skipped_step,
                    "new issue",
                ),
                "feedback": "new issue",
            }],
            "retryable_step_ids": [],
        });
        let transcript = WorkflowTranscript::create(
            &pool,
            &CreateWorkflowTranscript {
                execution_id: execution.id,
                round_id: Some(round_id),
                workflow_agent_session_id: None,
                step_id: Some(review_step.id),
                sender_type: "control".to_string(),
                entry_type: "loop_review".to_string(),
                content: "workflow.loop_skipped_retry_decision.request".to_string(),
                meta_json: Some(transcript_meta.to_string()),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create decision transcript");
        let workflow_session = WorkflowAgentSession {
            id: Uuid::new_v4(),
            workflow_execution_id: execution.id,
            session_agent_id: Uuid::new_v4(),
            role: WorkflowAgentSessionRole::Reviewer,
            agent_session_id: None,
            agent_message_id: None,
            state: WorkflowAgentSessionState::Idle,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let chat_runner = ChatRunner::new(DBService { pool: pool.clone() });

        WorkflowOrchestrator::resolve_loop_skipped_retry_decision(
            &pool,
            &chat_runner,
            &transcript,
            &execution,
            &review_step,
            &workflow_session,
            &workflow_loop,
            &transcript_meta,
            "restart_skipped",
        )
        .await
        .expect("restart skipped decision");

        let reopened = WorkflowStep::find_by_id(&pool, skipped_step.id)
            .await
            .expect("find reopened step")
            .expect("reopened step exists");
        assert_eq!(reopened.status, WorkflowStepStatus::Pending);
        assert_eq!(reopened.retry_count, 1);
        assert!(loop_skip_waiver(&reopened, "loop-a").is_none());
        assert!(
            reopened
                .revision_context
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .and_then(|context| context.get("loop_skip_waivers").cloned())
                .and_then(|waivers| waivers.as_array().cloned())
                .is_some_and(|waivers| waivers.iter().all(|waiver| {
                    waiver.get("status").and_then(|value| value.as_str()) == Some("superseded")
                }))
        );
        let persisted_review = WorkflowStep::find_by_id(&pool, review_step.id)
            .await
            .expect("find review step")
            .expect("review step exists");
        assert_eq!(persisted_review.status, WorkflowStepStatus::Ready);
        let persisted_loop = WorkflowLoop::find_by_id(&pool, loop_id)
            .await
            .expect("find loop")
            .expect("loop exists");
        assert_eq!(persisted_loop.status, WorkflowLoopStatus::Running);
        assert_eq!(persisted_loop.retry_count, 2);

        let duplicate = WorkflowOrchestrator::resolve_loop_skipped_retry_decision(
            &pool,
            &chat_runner,
            &transcript,
            &execution,
            &review_step,
            &workflow_session,
            &workflow_loop,
            &transcript_meta,
            "restart_skipped",
        )
        .await;
        assert!(duplicate.is_err());
    }

    #[tokio::test]
    async fn keep_skipped_with_mixed_targets_retries_remaining_scope() {
        let pool = setup_skipped_decision_pool().await;
        let round_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let execution = sample_waiting_execution(round_id);
        persist_waiting_execution(&pool, &execution).await;
        let skipped_step = insert_skipped_step(&pool, execution.id, round_id, loop_id).await;
        let completed_step =
            insert_completed_step(&pool, execution.id, round_id, loop_id, "completed-task").await;
        let review_step = insert_review_step(&pool, execution.id, round_id, loop_id).await;
        let workflow_loop = WorkflowLoop {
            id: loop_id,
            execution_id: execution.id,
            round_id,
            loop_key: "loop-a".to_string(),
            review_step_id: review_step.id,
            member_step_ids_json: serde_json::to_string(&vec![skipped_step.id, completed_step.id])
                .expect("member JSON"),
            status: WorkflowLoopStatus::WaitingUser,
            retry_count: 1,
            max_retry: 3,
            user_review_required: false,
            rejection_reason: Some("mixed feedback".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        persist_waiting_loop(&pool, &workflow_loop).await;
        let transcript_meta = serde_json::json!({
            "resolved": false,
            "review_kind": "loop_skipped_retry_decision",
            "loop_id": loop_id,
            "feedback": "mixed feedback",
            "skipped_steps": [{
                "step_id": skipped_step.id,
                "issue_scope_id": loop_skip_issue_scope_id_for_feedback(
                    &workflow_loop,
                    &skipped_step,
                    "mixed feedback",
                ),
                "feedback": "mixed feedback",
            }],
            "retryable_step_ids": [completed_step.id],
        });
        let transcript = WorkflowTranscript::create(
            &pool,
            &CreateWorkflowTranscript {
                execution_id: execution.id,
                round_id: Some(round_id),
                workflow_agent_session_id: None,
                step_id: Some(review_step.id),
                sender_type: "control".to_string(),
                entry_type: "loop_review".to_string(),
                content: "workflow.loop_skipped_retry_decision.request".to_string(),
                meta_json: Some(transcript_meta.to_string()),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create decision transcript");
        let workflow_session = WorkflowAgentSession {
            id: Uuid::new_v4(),
            workflow_execution_id: execution.id,
            session_agent_id: Uuid::new_v4(),
            role: WorkflowAgentSessionRole::Reviewer,
            agent_session_id: None,
            agent_message_id: None,
            state: WorkflowAgentSessionState::Idle,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let chat_runner = ChatRunner::new(DBService { pool: pool.clone() });

        WorkflowOrchestrator::resolve_loop_skipped_retry_decision(
            &pool,
            &chat_runner,
            &transcript,
            &execution,
            &review_step,
            &workflow_session,
            &workflow_loop,
            &transcript_meta,
            "keep_skipped",
        )
        .await
        .expect("keep skipped mixed decision");

        assert_eq!(
            WorkflowLoop::find_by_id(&pool, loop_id)
                .await
                .expect("find loop")
                .expect("loop exists")
                .status,
            WorkflowLoopStatus::Running
        );
        assert_eq!(
            WorkflowStep::find_by_id(&pool, review_step.id)
                .await
                .expect("find review step")
                .expect("review step exists")
                .status,
            WorkflowStepStatus::Ready
        );
        assert_eq!(
            WorkflowStep::find_by_id(&pool, completed_step.id)
                .await
                .expect("find completed step")
                .expect("completed step exists")
                .status,
            WorkflowStepStatus::Completed
        );
        let persisted_skipped = WorkflowStep::find_by_id(&pool, skipped_step.id)
            .await
            .expect("find skipped step")
            .expect("skipped step exists");
        assert!(has_matching_active_skip_waiver(
            &persisted_skipped,
            &workflow_loop,
            "mixed feedback"
        ));
    }

    #[tokio::test]
    async fn ordinary_loop_rejection_still_retries_without_skipped_decision() {
        let pool = setup_skipped_decision_pool().await;
        let round_id = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let execution = sample_waiting_execution(round_id);
        persist_waiting_execution(&pool, &execution).await;
        let completed_step =
            insert_completed_step(&pool, execution.id, round_id, loop_id, "completed-task").await;
        let review_step = insert_review_step(&pool, execution.id, round_id, loop_id).await;
        let workflow_loop = WorkflowLoop {
            id: loop_id,
            execution_id: execution.id,
            round_id,
            loop_key: "loop-a".to_string(),
            review_step_id: review_step.id,
            member_step_ids_json: serde_json::to_string(&vec![completed_step.id])
                .expect("member JSON"),
            status: WorkflowLoopStatus::WaitingUser,
            retry_count: 0,
            max_retry: 3,
            user_review_required: true,
            rejection_reason: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        persist_waiting_loop(&pool, &workflow_loop).await;
        let transcript = WorkflowTranscript::create(
            &pool,
            &CreateWorkflowTranscript {
                execution_id: execution.id,
                round_id: Some(round_id),
                workflow_agent_session_id: None,
                step_id: Some(review_step.id),
                sender_type: "control".to_string(),
                entry_type: "loop_review".to_string(),
                content: "review loop".to_string(),
                meta_json: Some(
                    serde_json::json!({
                        "resolved": false,
                        "review_kind": "loop_user_review",
                        "loop_id": loop_id,
                    })
                    .to_string(),
                ),
            },
            Uuid::new_v4(),
        )
        .await
        .expect("create loop review transcript");
        let workflow_session = WorkflowAgentSession {
            id: Uuid::new_v4(),
            workflow_execution_id: execution.id,
            session_agent_id: Uuid::new_v4(),
            role: WorkflowAgentSessionRole::Reviewer,
            agent_session_id: None,
            agent_message_id: None,
            state: WorkflowAgentSessionState::Idle,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let chat_runner = ChatRunner::new(DBService { pool: pool.clone() });

        WorkflowOrchestrator::resolve_loop_review_action(
            &pool,
            &chat_runner,
            &transcript,
            &execution,
            &review_step,
            &workflow_session,
            "reject",
            Some("retry task"),
        )
        .await
        .expect("ordinary loop rejection");

        let persisted_loop = WorkflowLoop::find_by_id(&pool, loop_id)
            .await
            .expect("find loop")
            .expect("loop exists");
        assert_eq!(persisted_loop.status, WorkflowLoopStatus::Running);
        assert_eq!(persisted_loop.retry_count, 1);
        assert_eq!(
            WorkflowStep::find_by_id(&pool, review_step.id)
                .await
                .expect("find review step")
                .expect("review step exists")
                .status,
            WorkflowStepStatus::Ready
        );
        assert!(
            WorkflowStep::find_by_id(&pool, completed_step.id)
                .await
                .expect("find completed step")
                .expect("completed step exists")
                .revision_context
                .as_deref()
                .is_some_and(|context| context.contains("pending_feedback"))
        );
        assert!(
            WorkflowTranscript::find_unresolved_reviews_by_execution(&pool, execution.id)
                .await
                .expect("find unresolved reviews")
                .is_empty()
        );
        assert!(
            WorkflowEvent::find_by_execution(&pool, execution.id)
                .await
                .expect("find events")
                .iter()
                .any(|event| event.event_type == WorkflowEventType::LoopRetrying)
        );
    }

    #[test]
    fn user_approved_loop_result_message_is_localized_by_language() {
        assert_eq!(
            WorkflowOrchestrator::localized_user_approved_loop_result_message_for_language(
                &UiLanguage::En
            ),
            "User approved the loop result."
        );
        assert_eq!(
            WorkflowOrchestrator::localized_user_approved_loop_result_message_for_language(
                &UiLanguage::ZhHans
            ),
            "用户已批准循环结果。"
        );
        assert_eq!(
            WorkflowOrchestrator::localized_user_approved_loop_result_message_for_language(
                &UiLanguage::Ja
            ),
            "ユーザーがループ結果を承認しました。"
        );
    }

    #[test]
    fn skipped_decision_inbox_message_is_localized_and_never_exposes_protocol_key() {
        for language in [
            UiLanguage::En,
            UiLanguage::Fr,
            UiLanguage::Ja,
            UiLanguage::Es,
            UiLanguage::Ko,
            UiLanguage::ZhHans,
            UiLanguage::ZhHant,
        ] {
            let message =
                WorkflowOrchestrator::localized_loop_skipped_decision_inbox_message_for_language(
                    &language,
                );
            assert!(!message.contains("workflow."));
            assert!(!message.trim().is_empty());
        }
        assert_eq!(
            WorkflowOrchestrator::localized_loop_skipped_decision_inbox_message_for_locale("zh-TW"),
            Some("請選擇如何處理循環中已跳過的節點。")
        );
    }

    #[test]
    fn skipped_decision_keep_effect_follows_actual_retryable_targets() {
        assert_eq!(
            skipped_retry_keep_effect(&[]),
            "waive_skipped_scope_and_complete_loop"
        );
        assert_eq!(
            skipped_retry_keep_effect(&[Uuid::new_v4()]),
            "waive_skipped_scope_and_retry_remaining_targets"
        );
    }

    #[test]
    fn browser_locale_maps_loop_approval_message() {
        assert_eq!(
            WorkflowOrchestrator::localized_user_approved_loop_result_message_for_locale("zh-TW"),
            Some("用戶已批准循環結果。")
        );
        assert_eq!(
            WorkflowOrchestrator::localized_user_approved_loop_result_message_for_locale("fr-FR"),
            Some("L'utilisateur a approuvé le résultat de la boucle.")
        );
        assert_eq!(
            WorkflowOrchestrator::localized_user_approved_loop_result_message_for_locale("de-DE"),
            None
        );
    }

    #[tokio::test]
    async fn committed_review_wakes_scheduler_when_unresolved_review_query_fails() {
        let pool = setup_skipped_decision_pool().await;
        pool.close().await;

        assert!(
            WorkflowOrchestrator::should_wake_after_committed_review(
                &pool,
                Uuid::new_v4(),
                Uuid::new_v4(),
            )
            .await
        );
    }
}
