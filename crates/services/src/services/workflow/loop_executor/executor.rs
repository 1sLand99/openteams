impl<'a> LoopExecutor<'a> {
    pub(crate) async fn execute_ready_review(
        &self,
        workflow_loop: &WorkflowLoop,
        loop_def: &CompiledLoopDef,
    ) -> Result<LoopOutcome, OrchestratorError> {
        let active_loop = if workflow_loop.status == WorkflowLoopStatus::Running {
            workflow_loop.clone()
        } else {
            WorkflowLoop::update_status_if_current(
                self.pool,
                workflow_loop.id,
                workflow_loop.status.clone(),
                WorkflowLoopStatus::Running,
                workflow_loop.rejection_reason.clone(),
            )
            .await?
            .ok_or_else(|| {
                OrchestratorError::IllegalTransition(format!(
                    "loop {} changed before review execution",
                    workflow_loop.id
                ))
            })?
        };

        let review_decision = match self.execute_loop_review(&active_loop, loop_def).await {
            Ok(decision) => decision,
            Err(OrchestratorError::Runtime(
                crate::services::workflow_runtime::WorkflowRuntimeError::Interrupted(reason),
            )) => {
                let review_step =
                    WorkflowStep::find_by_id(self.pool, active_loop.review_step_id)
                        .await?
                        .ok_or_else(|| {
                            OrchestratorError::NotFound(format!(
                                "loop review step {} not found",
                                active_loop.review_step_id
                            ))
                        })?;
                let interrupted_step = match review_step.status {
                    WorkflowStepStatus::InterruptRequested => {
                        WorkflowOrchestrator::transition_step_and_sync(
                            self.pool,
                            self.chat_runner,
                            self.execution,
                            &review_step,
                            WorkflowStepStatus::Interrupted,
                            "loop_review_interrupted",
                        )
                        .await?
                    }
                    WorkflowStepStatus::Running => {
                        let requested = WorkflowOrchestrator::transition_step_and_sync(
                            self.pool,
                            self.chat_runner,
                            self.execution,
                            &review_step,
                            WorkflowStepStatus::InterruptRequested,
                            "loop_review_interrupt_recovered_by_work_item_guard",
                        )
                        .await?;
                        WorkflowOrchestrator::transition_step_and_sync(
                            self.pool,
                            self.chat_runner,
                            self.execution,
                            &requested,
                            WorkflowStepStatus::Interrupted,
                            "loop_review_interrupted",
                        )
                        .await?
                    }
                    WorkflowStepStatus::Interrupted => review_step,
                    _ => return Err(OrchestratorError::Runtime(
                        crate::services::workflow_runtime::WorkflowRuntimeError::Interrupted(
                            reason,
                        ),
                    )),
                };
                let _ = WorkflowOrchestrator::write_transcript(
                    self.pool,
                    self.execution.id,
                    Some(interrupted_step.round_id),
                    interrupted_step.assigned_workflow_agent_session_id,
                    Some(interrupted_step.id),
                    "system",
                    "message",
                    &format!(
                        "Loop review \"{}\" interrupted: {}",
                        active_loop.loop_key, reason
                    ),
                    None,
                )
                .await;
                self.refresh_loop_projection(&active_loop, "loop_review_interrupted")
                    .await?;
                return Ok(LoopOutcome::Parked);
            }
            Err(error) => {
                let reason = error.to_string();
                let review_step =
                    WorkflowStep::find_by_id(self.pool, active_loop.review_step_id).await?;
                let failed_step = if let Some(review_step) = review_step {
                    if matches!(
                        review_step.status,
                        WorkflowStepStatus::Running | WorkflowStepStatus::Revising
                    ) {
                        Some(
                            WorkflowOrchestrator::transition_step_and_sync(
                                self.pool,
                                self.chat_runner,
                                self.execution,
                                &review_step,
                                WorkflowStepStatus::Failed,
                                "loop_review_failed_by_work_item_guard",
                            )
                            .await?,
                        )
                    } else {
                        Some(review_step)
                    }
                } else {
                    None
                };
                if let Some(failed_step) = failed_step.as_ref() {
                    let _ = WorkflowStep::record_execution_result(
                        self.pool,
                        failed_step.id,
                        Uuid::new_v4(),
                        Some(
                            serde_json::to_string(&SummaryPayload {
                                summary: reason.clone(),
                                content: None,
                                outputs: Vec::new(),
                            })
                            .unwrap_or_else(|_| reason.clone()),
                        ),
                        None,
                    )
                    .await;
                }
                let failed_loop = WorkflowLoop::update_status_if_current(
                    self.pool,
                    active_loop.id,
                    WorkflowLoopStatus::Running,
                    WorkflowLoopStatus::Failed,
                    Some(reason.clone()),
                )
                .await?
                .ok_or_else(|| {
                    OrchestratorError::IllegalTransition(format!(
                        "loop {} changed while settling review failure",
                        active_loop.id
                    ))
                })?;
                Self::emit_loop_event(
                    self.pool,
                    self.execution,
                    &failed_loop,
                    WorkflowEventType::LoopFailed,
                    Some(serde_json::json!({
                        "reason": "loop_review_work_item_error",
                        "error": reason.clone(),
                    })),
                )
                .await?;
                self.refresh_loop_projection(&failed_loop, "loop_review_failed")
                    .await?;
                tracing::error!(
                    execution_id = %self.execution.id,
                    loop_id = %active_loop.id,
                    step_id = %active_loop.review_step_id,
                    error = %error,
                    "loop review work-item guard converted an unhandled error into terminal state"
                );
                return Ok(LoopOutcome::Failed(reason));
            }
        };

        match review_decision {
            LoopReviewDecision::Passed => {
                if requires_user_acceptance_checkpoint(&active_loop) {
                    self.park_for_loop_user_review(&active_loop).await?;
                    return Ok(LoopOutcome::Parked);
                }

                let completed_loop = WorkflowLoop::update_status(
                    self.pool,
                    active_loop.id,
                    WorkflowLoopStatus::Completed,
                    None,
                )
                .await?;
                Self::emit_loop_event(
                    self.pool,
                    self.execution,
                    &completed_loop,
                    WorkflowEventType::LoopPassed,
                    None,
                )
                .await?;
                self.refresh_loop_projection(&completed_loop, "loop_passed")
                    .await?;
                Ok(LoopOutcome::Completed)
            }
            LoopReviewDecision::PassedByUserWaiver {
                feedback,
                review_step,
            } => {
                // A lead waiver match only settles the lead review. When this
                // loop requires user review, final acceptance remains an
                // explicit user decision exactly as in the normal Passed path.
                if requires_user_acceptance_checkpoint(&active_loop) {
                    self.park_for_loop_user_review(&active_loop).await?;
                    return Ok(LoopOutcome::Parked);
                }

                let mut transaction = self.pool.begin().await?;
                let completed_review_step = reducer::transition_step_in_transaction(
                    &mut transaction,
                    self.execution,
                    &review_step,
                    WorkflowStepStatus::Completed,
                    Some(serde_json::json!({
                        "reason": "loop_review_rejection_covered_by_user_waiver",
                    })),
                )
                .await?
                .entity;
                let completed_loop = WorkflowLoop::update_status_if_current_in_transaction(
                    &mut transaction,
                    active_loop.id,
                    WorkflowLoopStatus::Running,
                    WorkflowLoopStatus::Completed,
                    None,
                )
                .await?
                .ok_or_else(|| {
                    OrchestratorError::IllegalTransition(format!(
                        "loop {} changed while applying skipped waiver",
                        active_loop.id
                    ))
                })?;
                WorkflowEvent::create_in_transaction(
                    &mut transaction,
                    &CreateWorkflowEvent {
                        execution_id: self.execution.id,
                        round_id: Some(completed_loop.round_id),
                        step_id: Some(completed_review_step.id),
                        agent_session_id: completed_review_step
                            .assigned_workflow_agent_session_id,
                        event_type: WorkflowEventType::LoopPassed,
                        status_before: Some(to_workflow_wire_value(&WorkflowLoopStatus::Running)),
                        status_after: Some(to_workflow_wire_value(&WorkflowLoopStatus::Completed)),
                        detail_json: Some(
                            serde_json::json!({
                                "reason": "rejected_targets_covered_by_user_waiver",
                                "feedback": feedback,
                            })
                            .to_string(),
                        ),
                    },
                    Uuid::new_v4(),
                )
                .await?;
                transaction.commit().await?;
                if let Err(error) = self
                    .refresh_loop_projection(&completed_loop, "loop_passed_by_user_waiver")
                    .await
                {
                    tracing::error!(
                        execution_id = %self.execution.id,
                        loop_id = %completed_loop.id,
                        error = %error,
                        "waived loop completion committed but projection refresh failed"
                    );
                }
                Ok(LoopOutcome::Completed)
            }
            LoopReviewDecision::Rejected {
                feedback,
                feedback_targets,
                feedback_source,
            } => {
                let skipped_targets = feedback_targets
                    .iter()
                    .filter(|target| target.step.status == WorkflowStepStatus::Skipped)
                    .cloned()
                    .collect::<Vec<_>>();
                if !skipped_targets.is_empty() {
                    self.park_for_skipped_retry_decision(
                        &active_loop,
                        &feedback,
                        &feedback_targets,
                        &skipped_targets,
                        feedback_source,
                    )
                    .await?;
                    return Ok(LoopOutcome::Parked);
                }
                self.inject_feedback_to_steps(
                    &active_loop,
                    feedback_source,
                    &feedback,
                    &feedback_map_from_targets(&feedback_targets),
                )
                .await?;
                let retry_loop = WorkflowLoop::increment_retry(
                    self.pool,
                    active_loop.id,
                    WorkflowLoopStatus::Running,
                    Some(feedback.clone()),
                )
                .await?;
                Self::emit_loop_event(
                    self.pool,
                    self.execution,
                    &retry_loop,
                    WorkflowEventType::LoopRetrying,
                    Some(serde_json::json!({
                        "feedback": feedback,
                        "retry_count": retry_loop.retry_count,
                        "max_retry": retry_loop.max_retry,
                        "review_attempt": retry_loop.retry_count,
                        "max_review_attempts": max_loop_review_attempts(&retry_loop),
                    })),
                )
                .await?;
                self.refresh_loop_projection(&retry_loop, "loop_retrying")
                    .await?;
                Ok(LoopOutcome::Progressed)
            }
            LoopReviewDecision::LimitReached {
                feedback,
                review_attempt,
            } => {
                let max_review_attempts = max_loop_review_attempts(&active_loop);
                let reason = format!(
                    "Loop review \"{}\" was rejected on the final allowed review attempt ({}/{}): {}",
                    active_loop.loop_key,
                    review_attempt,
                    max_review_attempts,
                    feedback
                );
                let failed_loop = WorkflowLoop::update_status(
                    self.pool,
                    active_loop.id,
                    WorkflowLoopStatus::Failed,
                    Some(reason.clone()),
                )
                .await?;
                Self::emit_loop_event(
                    self.pool,
                    self.execution,
                    &failed_loop,
                    WorkflowEventType::LoopFailed,
                    Some(serde_json::json!({
                        "reason": "review_limit_reached",
                        "feedback": feedback,
                        "review_attempt": review_attempt,
                        "max_retry": active_loop.max_retry,
                        "max_review_attempts": max_review_attempts,
                    })),
                )
                .await?;
                self.refresh_loop_projection(&failed_loop, "loop_review_limit_reached")
                    .await?;
                Ok(LoopOutcome::Failed(reason))
            }
        }
    }

    async fn refresh_loop_projection(
        &self,
        workflow_loop: &WorkflowLoop,
        reason: &str,
    ) -> Result<(), OrchestratorError> {
        WorkflowOrchestrator::refresh_execution_projection_with_reason(
            self.pool,
            self.chat_runner,
            self.execution.id,
            None,
            reason,
            vec![workflow_loop.review_step_id.to_string()],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn reset_loop_steps(
        &self,
        workflow_loop: &WorkflowLoop,
    ) -> Result<Vec<WorkflowStep>, OrchestratorError> {
        let member_ids = parse_member_step_ids(&workflow_loop.member_step_ids_json)?;
        let mut reset_steps = Vec::new();
        let mut has_pending_loop_feedback = false;
        for step_id in member_ids {
            let step = WorkflowStep::find_by_id(self.pool, step_id)
                .await?
                .ok_or_else(|| {
                    OrchestratorError::NotFound(format!("step {} not found", step_id))
                })?;
            let pending_loop_feedback = has_pending_feedback_for_loop(&step, workflow_loop);
            has_pending_loop_feedback |= pending_loop_feedback;
            let prepared_for_retry = pending_loop_feedback
                && matches!(
                    step.status,
                    WorkflowStepStatus::Completed
                        | WorkflowStepStatus::Failed
                        | WorkflowStepStatus::Interrupted
                        | WorkflowStepStatus::Blocked
                        | WorkflowStepStatus::Revising
                );
            let step = if prepared_for_retry {
                reducer::prepare_step_retry(
                    self.pool,
                    self.execution,
                    &step,
                    Some(serde_json::json!({
                        "reason": "loop_member_feedback_retry_prepared",
                        "loop_id": workflow_loop.id,
                    })),
                )
                .await?
                .entity
            } else {
                step
            };

            reset_steps.push(step);
        }

        let review_step = WorkflowStep::find_by_id(self.pool, workflow_loop.review_step_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "loop review step {} not found",
                    workflow_loop.review_step_id
                ))
            })?;
        if has_pending_loop_feedback
            && matches!(
                review_step.status,
                WorkflowStepStatus::Completed
                    | WorkflowStepStatus::Failed
                    | WorkflowStepStatus::Interrupted
                    | WorkflowStepStatus::Blocked
                    | WorkflowStepStatus::Revising
            )
        {
            let review_step = reducer::prepare_step_retry(
                self.pool,
                self.execution,
                &review_step,
                Some(serde_json::json!({
                    "reason": "loop_review_feedback_retry_prepared",
                    "loop_id": workflow_loop.id,
                })),
            )
            .await?
            .entity;
            reset_steps.push(review_step);
        }

        Ok(reset_steps)
    }

    pub(crate) async fn inject_feedback_to_steps(
        &self,
        workflow_loop: &WorkflowLoop,
        source: WorkflowRevisionFeedbackSource,
        loop_feedback: &str,
        step_feedbacks: &HashMap<String, String>,
    ) -> Result<(), OrchestratorError> {
        inject_feedback_to_steps(
            self.pool,
            workflow_loop,
            source,
            loop_feedback,
            step_feedbacks,
        )
        .await
    }

    pub(crate) async fn record_user_skip_waiver_in_transaction(
        connection: &mut SqliteConnection,
        workflow_loop: &WorkflowLoop,
        step: &WorkflowStep,
        issue_scope_id: &str,
        feedback: &str,
    ) -> Result<WorkflowStep, OrchestratorError> {
        let context = merge_loop_skip_waiver_context_for_issue(
            step.revision_context.as_deref(),
            workflow_loop,
            step,
            issue_scope_id,
            feedback,
        );
        WorkflowStep::update_revision_context_in_transaction(
            connection,
            step.id,
            Some(context),
        )
        .await
        .map_err(OrchestratorError::Database)
    }

    pub(crate) async fn emit_loop_event(
        pool: &SqlitePool,
        execution: &WorkflowExecution,
        workflow_loop: &WorkflowLoop,
        event_type: WorkflowEventType,
        detail_json: Option<serde_json::Value>,
    ) -> Result<WorkflowEvent, OrchestratorError> {
        WorkflowEvent::create(
            pool,
            &CreateWorkflowEvent {
                execution_id: execution.id,
                round_id: Some(workflow_loop.round_id),
                step_id: Some(workflow_loop.review_step_id),
                agent_session_id: None,
                event_type,
                status_before: None,
                status_after: Some(to_workflow_wire_value(&workflow_loop.status)),
                detail_json: detail_json.map(|value| value.to_string()),
            },
            Uuid::new_v4(),
        )
        .await
        .map_err(OrchestratorError::Database)
    }

    async fn execute_loop_review(
        &self,
        workflow_loop: &WorkflowLoop,
        loop_def: &CompiledLoopDef,
    ) -> Result<LoopReviewDecision, OrchestratorError> {
        let review_step = WorkflowStep::find_by_id(self.pool, workflow_loop.review_step_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "loop review step {} not found",
                    workflow_loop.review_step_id
                ))
            })?;
        let review_step = if review_step.status == WorkflowStepStatus::Ready {
            review_step
        } else {
            WorkflowOrchestrator::transition_step_and_sync(
                self.pool,
                self.chat_runner,
                self.execution,
                &review_step,
                WorkflowStepStatus::Ready,
                "loop_review_ready",
            )
            .await?
        };
        let running_review_step = WorkflowOrchestrator::guarded_transition_step_and_sync(
            self.pool,
            self.chat_runner,
            self.execution,
            &review_step,
            WorkflowStepStatus::Running,
            "loop_review_started",
        )
        .await?
        .ok_or_else(|| {
            OrchestratorError::IllegalTransition(format!(
                "loop review step {} was already claimed",
                review_step.id
            ))
        })?;

        let workflow_session = resolve_step_workflow_session(
            self.execution,
            self.workflow_agent_sessions,
            &running_review_step,
        )?;
        let session_agent = self
            .session_agents
            .iter()
            .find(|item| item.id == workflow_session.session_agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "session agent {} not found",
                    workflow_session.session_agent_id
                ))
            })?;
        let agent = self
            .agents
            .iter()
            .find(|item| item.id == session_agent.agent_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!("agent {} not found", session_agent.agent_id))
            })?;
        let reviewer_type = if workflow_session.role == WorkflowAgentSessionRole::Lead {
            ReviewerType::Lead
        } else {
            ReviewerType::Reviewer
        };
        let reviewer_type_label = match &reviewer_type {
            ReviewerType::Lead => "lead",
            ReviewerType::Reviewer => "reviewer",
            ReviewerType::User => "user",
        };
        let feedback_source = match &reviewer_type {
            ReviewerType::Lead => WorkflowRevisionFeedbackSource::Lead,
            ReviewerType::Reviewer | ReviewerType::User => WorkflowRevisionFeedbackSource::Reviewer,
        };

        let workflow_goal = self
            .plan
            .summary_text
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| self.plan.title.clone());
        let review_attempt = WorkflowOrchestrator::next_review_attempt(
            self.pool,
            running_review_step.id,
            reviewer_type.clone(),
        )
        .await?;
        let max_review_attempts = max_loop_review_attempts(workflow_loop);
        if review_attempt > max_review_attempts {
            let feedback = format!(
                "Loop review \"{}\" cannot run again: the maximum of {} review attempts has been reached.",
                loop_def.loop_key, max_review_attempts
            );
            WorkflowOrchestrator::transition_step_and_sync(
                self.pool,
                self.chat_runner,
                self.execution,
                &running_review_step,
                WorkflowStepStatus::Failed,
                "loop_review_limit_reached",
            )
            .await?;
            return Ok(LoopReviewDecision::LimitReached {
                feedback,
                review_attempt: max_review_attempts,
            });
        }
        let (review_inputs, review_context) = self
            .review_prompt_inputs(
                loop_def,
                &running_review_step,
                workflow_loop,
                &session_agent.member_name,
                &reviewer_type,
            )
            .await?;
        let ui_config = config::load_config_from_file(&config_path()).await;
        let response_language_instruction =
            resolve_workflow_response_language_instruction(&ui_config.language);
        let loop_acceptance = WorkflowOrchestrator::acceptance_criteria_object_for_step(
            self.plan,
            &running_review_step,
        );
        let declared_acceptance = loop_acceptance
            .leveled()
            .into_iter()
            .filter(|(_, criterion)| !criterion.trim().is_empty())
            .enumerate()
            .map(|(index, (level, criterion))| protocol::LoopReviewCriterion {
                id: format!("c{}", index + 1),
                level,
                criterion,
            })
            .collect::<Vec<_>>();
        let prompt = prompts::build_loop_review_prompt(&prompts::LoopReviewPromptInput {
            execution_id: self.execution.id,
            loop_key: loop_def.loop_key.clone(),
            workflow_goal: workflow_goal.clone(),
            reviewer: prompts::ReviewerInput {
                name: review_context.reviewer_name.clone(),
                role: review_context.reviewer_role.clone(),
            },
            review_instructions: review_context.review_step_instructions.clone(),
            acceptance_criteria: declared_acceptance.clone(),
            review_scope: review_inputs
                .iter()
                .map(|input| prompts::LoopReviewTaskInput {
                    step_key: input.step_key.clone(),
                    title: input.title.clone(),
                    instructions: input.instructions.clone(),
                    summary: input.summary.clone(),
                    outputs: input.outputs.clone(),
                    evidence: input.evidence.clone(),
                    user_skip_waiver: input.user_skip_waiver.clone(),
                })
                .collect(),
            rework_acceptance: review_inputs
                .iter()
                .filter_map(|input| {
                    input.rework_requirement.as_ref().map(|requirement| {
                        prompts::LoopReworkAcceptanceInput {
                            step_key: input.step_key.clone(),
                            title: input.title.clone(),
                            requirement: requirement.clone(),
                            summary: input.summary.clone(),
                            outputs: input.outputs.clone(),
                            evidence: input.evidence.clone(),
                        }
                    })
                })
                .collect(),
            required_upstream_results: review_context.required_upstream_results.clone(),
            scope_edges: review_context.review_scope_edges.clone(),
            current_round: review_context.current_round,
            review_attempt,
            retry_count: review_context.loop_retry_count,
            retry_budget: review_context.retry_budget,
            latest_loop_feedback: workflow_loop.rejection_reason.clone(),
            response_language: response_language_instruction.to_string(),
        });
        let allowed_step_keys = review_inputs
            .iter()
            .map(|input| input.step_key.clone())
            .collect::<Vec<_>>();
        let (review_message, raw_output) = self
            .run_loop_review_protocol_with_retry(
                agent,
                session_agent,
                workflow_session,
                workflow_loop,
                &running_review_step,
                &prompt,
                &declared_acceptance,
                &allowed_step_keys,
            )
            .await?;
        let LoopReviewProtocolMessage::LoopReviewResult {
            summary: feedback,
            results,
            rework,
            ..
        } = review_message;
        let verdict = protocol::derive_loop_review_verdict(&declared_acceptance, &results);
        let acceptance_results = declared_acceptance
            .iter()
            .map(|criterion| {
                let result = &results[&criterion.id];
                serde_json::json!({
                    "step_key": running_review_step.step_key,
                    "criterion": criterion.criterion,
                    "level": criterion.level,
                    "verdict": if result.passed { "passed" } else { "failed" },
                    "evidence": result.evidence,
                })
            })
            .collect::<Vec<_>>();
        let evidence = declared_acceptance
            .iter()
            .map(|criterion| results[&criterion.id].evidence.clone())
            .collect::<Vec<_>>();
        let result_summary = SummaryPayload {
            summary: feedback.clone(),
            content: Some(raw_output.clone()),
            outputs: Vec::new(),
        };
        let recorded_review_step = WorkflowStep::record_execution_result(
            self.pool,
            running_review_step.id,
            Uuid::new_v4(),
            Some(serde_json::to_string(&result_summary)?),
            Some(feedback.clone()),
        )
        .await?;
        let persisted_review = WorkflowOrchestrator::save_step_review(
            self.pool,
            &recorded_review_step,
            reviewer_type.clone(),
            Some(workflow_session.session_agent_id.to_string()),
            verdict.clone(),
            &feedback,
        )
        .await?;
        let _ = WorkflowOrchestrator::write_transcript(
            self.pool,
            self.execution.id,
            Some(recorded_review_step.round_id),
            Some(workflow_session.id),
            Some(recorded_review_step.id),
            "agent",
            "loop_review",
            &feedback,
            Some(
                &serde_json::json!({
                    "source": "workflow_structured_loop_review_result",
                    "resolved": true,
                    "review_kind": "loop_agent_review",
                    "reviewer_type": to_workflow_wire_value(&reviewer_type),
                    "reviewer_id": workflow_session.session_agent_id,
                    "review_round": persisted_review.review_round,
                    "verdict": verdict,
                    "acceptance_results": acceptance_results,
                    "evidence": evidence,
                    "structured_result": {
                        "type": "loop_review_result",
                        "loop_key": workflow_loop.loop_key,
                        "execution_id": self.execution.id,
                        "verdict": verdict,
                        "feedback": feedback,
                        "acceptance_results": acceptance_results,
                        "evidence": evidence,
                        "rework": rework,
                    },
                })
                .to_string(),
            ),
        )
        .await?;

        match verdict {
            ReviewVerdict::Approved => {
                if !workflow_loop.user_review_required {
                    WorkflowOrchestrator::transition_step_and_sync(
                        self.pool,
                        self.chat_runner,
                        self.execution,
                        &recorded_review_step,
                        WorkflowStepStatus::Completed,
                        "loop_review_completed",
                    )
                    .await?;
                }
                WorkflowLoop::update_status(
                    self.pool,
                    workflow_loop.id,
                    WorkflowLoopStatus::Passed,
                    None,
                )
                .await?;
                Ok(LoopReviewDecision::Passed)
            }
            ReviewVerdict::Rejected => {
                let event = loop_reviewer_review_rejected_analytics_parts(
                    self.execution,
                    recorded_review_step.id,
                    reviewer_type_label,
                );
                if let Some(analytics) = self.chat_runner.analytics_service() {
                    analytics.record_event(event);
                }
                let failed_required = declared_acceptance
                    .iter()
                    .filter(|criterion| {
                        criterion.level == AcceptanceCriterionLevel::Required
                            && !results[&criterion.id].passed
                    })
                    .collect::<Vec<_>>();
                let overall_issue_id = format!(
                    "criteria-{}",
                    failed_required
                        .iter()
                        .map(|criterion| criterion.id.as_str())
                        .collect::<Vec<_>>()
                        .join("-")
                );
                let step_feedbacks = rework.into_iter().collect::<HashMap<_, _>>();
                let step_issue_ids = step_feedbacks
                    .iter()
                    .map(|(step_key, feedback)| {
                        (step_key.clone(), feedback_issue_id(feedback))
                    })
                    .collect::<HashMap<_, _>>();
                let feedback_targets = loop_feedback_targets(
                    self.pool,
                    workflow_loop,
                    &step_feedbacks,
                    &step_issue_ids,
                    &feedback,
                    &overall_issue_id,
                )
                .await?;
                let disposition = rejected_loop_review_disposition(
                    review_attempt,
                    max_review_attempts,
                    &feedback_targets,
                );
                match disposition {
                    RejectedLoopReviewDisposition::PassedByUserWaiver => {
                        return Ok(LoopReviewDecision::PassedByUserWaiver {
                            feedback,
                            review_step: Box::new(recorded_review_step),
                        });
                    }
                    RejectedLoopReviewDisposition::NeedsSkippedDecision => {
                        return Ok(LoopReviewDecision::Rejected {
                            feedback,
                            feedback_targets,
                            feedback_source,
                        });
                    }
                    RejectedLoopReviewDisposition::LimitReached
                    | RejectedLoopReviewDisposition::Retry => {}
                }
                let limit_reached = disposition == RejectedLoopReviewDisposition::LimitReached;
                let terminal_review_status = if limit_reached {
                    WorkflowStepStatus::Failed
                } else {
                    WorkflowStepStatus::Completed
                };
                let _ = WorkflowOrchestrator::transition_step_and_sync(
                    self.pool,
                    self.chat_runner,
                    self.execution,
                    &recorded_review_step,
                    terminal_review_status,
                    if limit_reached {
                        "loop_review_limit_reached"
                    } else {
                        "loop_review_rejected"
                    },
                )
                .await?;
                if limit_reached {
                    return Ok(LoopReviewDecision::LimitReached {
                        feedback,
                        review_attempt,
                    });
                }
                WorkflowLoop::update_status(
                    self.pool,
                    workflow_loop.id,
                    WorkflowLoopStatus::Rejected,
                    Some(feedback.clone()),
                )
                .await?;
                Ok(LoopReviewDecision::Rejected {
                    feedback,
                    feedback_targets,
                    feedback_source,
                })
            }
        }
    }

    async fn park_for_loop_user_review(
        &self,
        workflow_loop: &WorkflowLoop,
    ) -> Result<(), OrchestratorError> {
        let review_step = WorkflowStep::find_by_id(self.pool, workflow_loop.review_step_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "loop review step {} not found",
                    workflow_loop.review_step_id
                ))
            })?;
        let waiting_step = if review_step.status == WorkflowStepStatus::WaitingInput {
            review_step
        } else {
            WorkflowOrchestrator::transition_step_and_sync(
                self.pool,
                self.chat_runner,
                self.execution,
                &review_step,
                WorkflowStepStatus::WaitingInput,
                "loop_waiting_user_review",
            )
            .await?
        };
        let workflow_session = resolve_step_workflow_session(
            self.execution,
            self.workflow_agent_sessions,
            &waiting_step,
        )?;
        let waiting_loop = WorkflowLoop::update_status(
            self.pool,
            workflow_loop.id,
            WorkflowLoopStatus::WaitingUser,
            None,
        )
        .await?;
        let transcript = WorkflowOrchestrator::write_transcript(
            self.pool,
            self.execution.id,
            Some(waiting_step.round_id),
            Some(workflow_session.id),
            Some(waiting_step.id),
            "control",
            "loop_review",
            &format!("Please review loop \"{}\".", waiting_loop.loop_key),
            Some(
                &serde_json::json!({
                    "resolved": false,
                    "review_kind": "loop_user_review",
                    "loop_id": waiting_loop.id,
                    "loop_key": waiting_loop.loop_key,
                    "summary": waiting_loop.rejection_reason,
                })
                .to_string(),
            ),
        )
        .await?;
        InboxService::new()
            .notify_workflow_user_action(
                self.pool,
                self.execution,
                &transcript,
                Some(&transcript.content),
            )
            .await;
        WorkflowOrchestrator::synchronize_runtime_state(self.pool, self.execution.id, false)
            .await?;
        WorkflowOrchestrator::refresh_execution_projection(
            self.pool,
            self.chat_runner,
            self.execution.id,
            None,
        )
        .await?;
        Ok(())
    }

    async fn park_for_skipped_retry_decision(
        &self,
        workflow_loop: &WorkflowLoop,
        loop_feedback: &str,
        feedback_targets: &[LoopFeedbackTarget],
        skipped_targets: &[LoopFeedbackTarget],
        feedback_source: WorkflowRevisionFeedbackSource,
    ) -> Result<(), OrchestratorError> {
        let review_step = WorkflowStep::find_by_id(self.pool, workflow_loop.review_step_id)
            .await?
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "loop review step {} not found",
                    workflow_loop.review_step_id
                ))
            })?;
        if review_step.status != WorkflowStepStatus::Running {
            return Err(OrchestratorError::IllegalTransition(format!(
                "loop review step {} is {:?}, expected running",
                review_step.id, review_step.status
            )));
        }
        let workflow_session = resolve_step_workflow_session(
            self.execution,
            self.workflow_agent_sessions,
            &review_step,
        )?;
        let skipped_steps = skipped_targets
            .iter()
            .map(|target| {
                serde_json::json!({
                    "step_id": target.step.id,
                    "step_key": target.step.step_key,
                    "title": target.step.title,
                    "issue_scope_id": target.issue_scope_id,
                    "feedback": target.feedback,
                })
            })
            .collect::<Vec<_>>();
        let retryable_step_ids = feedback_targets
            .iter()
            .filter(|target| target.step.status != WorkflowStepStatus::Skipped)
            .map(|target| target.step.id)
            .collect::<Vec<_>>();
        let skipped_titles = skipped_targets
            .iter()
            .map(|target| target.step.title.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let transcript_meta = serde_json::json!({
            "resolved": false,
            "review_kind": "loop_skipped_retry_decision",
            "loop_id": workflow_loop.id,
            "loop_key": workflow_loop.loop_key,
            "feedback": loop_feedback,
            "skipped_steps": skipped_steps,
            "skipped_step_titles": skipped_titles,
            "retryable_step_ids": retryable_step_ids,
            "restart_effect": "rerun_skipped_steps_then_review",
            "keep_effect": if feedback_targets.len() == skipped_targets.len() {
                "waive_skipped_scope_and_complete_loop"
            } else {
                "waive_skipped_scope_and_retry_remaining_targets"
            },
        })
        .to_string();

        let mut transaction = self.pool.begin().await?;
        inject_feedback_to_steps_in_transaction(
            &mut transaction,
            workflow_loop,
            feedback_source,
            loop_feedback,
            &feedback_map_from_targets(feedback_targets),
        )
        .await?;
        let waiting_step = reducer::transition_step_in_transaction(
            &mut transaction,
            self.execution,
            &review_step,
            WorkflowStepStatus::WaitingInput,
            Some(serde_json::json!({
                "reason": "loop_waiting_skipped_retry_decision",
            })),
        )
        .await?
        .entity;
        let waiting_loop = WorkflowLoop::update_status_if_current_in_transaction(
            &mut transaction,
            workflow_loop.id,
            WorkflowLoopStatus::Running,
            WorkflowLoopStatus::WaitingUser,
            Some(loop_feedback.to_string()),
        )
        .await?
        .ok_or_else(|| {
            OrchestratorError::IllegalTransition(format!(
                "loop {} changed before waiting_user transition",
                workflow_loop.id
            ))
        })?;
        WorkflowEvent::create_in_transaction(
            &mut transaction,
            &CreateWorkflowEvent {
                execution_id: self.execution.id,
                round_id: Some(waiting_loop.round_id),
                step_id: Some(waiting_step.id),
                agent_session_id: waiting_step.assigned_workflow_agent_session_id,
                event_type: WorkflowEventType::LoopWaitingUser,
                status_before: Some(to_workflow_wire_value(&WorkflowLoopStatus::Running)),
                status_after: Some(to_workflow_wire_value(&WorkflowLoopStatus::WaitingUser)),
                detail_json: Some(
                    serde_json::json!({
                        "loop_id": waiting_loop.id,
                        "reason": "skipped_retry_decision",
                        "skipped_step_ids": skipped_targets
                            .iter()
                            .map(|target| target.step.id)
                            .collect::<Vec<_>>(),
                    })
                    .to_string(),
                ),
            },
            Uuid::new_v4(),
        )
        .await?;
        let transcript = WorkflowTranscript::create_in_transaction(
            &mut transaction,
            &CreateWorkflowTranscript {
                execution_id: self.execution.id,
                round_id: Some(waiting_step.round_id),
                workflow_agent_session_id: Some(workflow_session.id),
                step_id: Some(waiting_step.id),
                sender_type: "control".to_string(),
                entry_type: "loop_review".to_string(),
                content: "workflow.loop_skipped_retry_decision.request".to_string(),
                meta_json: Some(transcript_meta),
            },
            Uuid::new_v4(),
        )
        .await?;
        transaction.commit().await?;
        let inbox_message =
            WorkflowOrchestrator::localized_loop_skipped_decision_inbox_message().await;
        InboxService::new()
            .notify_workflow_user_action(
                self.pool,
                self.execution,
                &transcript,
                Some(&inbox_message),
            )
            .await;
        if let Err(error) = WorkflowOrchestrator::synchronize_runtime_state(
            self.pool,
            self.execution.id,
            false,
        )
        .await
        {
            tracing::error!(
                execution_id = %self.execution.id,
                error = %error,
                "skipped decision transaction committed but runtime synchronization failed"
            );
        }
        if let Err(error) = WorkflowOrchestrator::refresh_execution_projection_with_reason(
            self.pool,
            self.chat_runner,
            self.execution.id,
            None,
            "loop_waiting_skipped_retry_decision",
            skipped_targets
                .iter()
                .map(|target| target.step.id.to_string())
                .collect(),
        )
        .await
        {
            tracing::error!(
                execution_id = %self.execution.id,
                error = %error,
                "skipped decision transaction committed but projection refresh failed"
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_loop_review_protocol_with_retry(
        &self,
        agent: &ChatAgent,
        session_agent: &ChatSessionAgent,
        workflow_session: &WorkflowAgentSession,
        workflow_loop: &WorkflowLoop,
        review_step: &WorkflowStep,
        prompt: &str,
        declared_acceptance: &[protocol::LoopReviewCriterion],
        allowed_step_keys: &[String],
    ) -> Result<(LoopReviewProtocolMessage, String), OrchestratorError> {
        let mut attempt = 0;
        let mut run_as_follow_up = false;
        let mut prompt_to_send = prompt.to_string();

        loop {
            let active_workflow_session = if run_as_follow_up {
                WorkflowAgentSession::find_by_id(self.pool, workflow_session.id)
                    .await?
                    .ok_or_else(|| {
                        OrchestratorError::NotFound(format!(
                            "workflow session {} not found",
                            workflow_session.id
                        ))
                    })?
            } else {
                workflow_session.clone()
            };

            let agent_output = if run_as_follow_up {
                run_workflow_step_agent_follow_up(
                    self.db,
                    self.chat_runner,
                    self.session,
                    agent,
                    session_agent,
                    &active_workflow_session,
                    &prompt_to_send,
                    review_step,
                )
                .await?
            } else {
                run_workflow_step_agent_prompt(
                    self.db,
                    self.chat_runner,
                    self.session,
                    agent,
                    session_agent,
                    Some(&active_workflow_session),
                    &prompt_to_send,
                    review_step,
                )
                .await?
            };
            let raw_output = agent_output.output;

            match protocol::parse_loop_review_protocol_output(
                self.execution.id,
                &workflow_loop.loop_key,
                declared_acceptance,
                allowed_step_keys,
                &raw_output,
            ) {
                Ok(message) => return Ok((message, raw_output)),
                Err(err)
                    if attempt < WORKFLOW_PROTOCOL_PARSE_MAX_RETRIES
                        && should_retry_workflow_protocol_parse_failure(&raw_output) =>
                {
                    tracing::warn!(
                        loop_id = %workflow_loop.id,
                        loop_key = %workflow_loop.loop_key,
                        attempt,
                        error = %err,
                        "workflow loop review protocol parse failed; retrying"
                    );
                    prompt_to_send =
                        prompt_builders::common::append_protocol_error_section(
                            prompt,
                            &err.to_string(),
                        );
                    attempt += 1;
                    run_as_follow_up = true;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    async fn review_prompt_inputs(
        &self,
        loop_def: &CompiledLoopDef,
        review_step: &WorkflowStep,
        workflow_loop: &WorkflowLoop,
        reviewer_name: &str,
        reviewer_type: &ReviewerType,
    ) -> Result<(Vec<LoopReviewPromptStepInput>, LoopReviewPromptContext), OrchestratorError> {
        let steps = WorkflowStep::find_by_execution(self.pool, self.execution.id).await?;
        let step_by_key = steps
            .iter()
            .map(|step| (step.step_key.as_str(), step))
            .collect::<HashMap<_, _>>();
        let plan_json: db::models::workflow_types::WorkflowPlanJson =
            serde_json::from_str(&self.plan.plan_json)?;

        let review_scope = loop_def
            .review_scope_step_keys
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let ordered_review_scope_step_keys = review_scope_step_keys_in_dag_order(
            &loop_def.review_scope_step_keys,
            &plan_json.edges,
        );

        let inputs = ordered_review_scope_step_keys
            .iter()
            .map(|step_key| {
                let step = step_by_key.get(step_key.as_str()).ok_or_else(|| {
                    OrchestratorError::NotFound(format!("review scope step {} not found", step_key))
                })?;
                let payload =
                    parse_summary_payload(step.summary_text.as_deref()).unwrap_or(SummaryPayload {
                        summary: step.summary_text.clone().unwrap_or_default(),
                        content: step.content.clone(),
                        outputs: Vec::new(),
                    });
                Ok(LoopReviewPromptStepInput {
                    step_key: step.step_key.clone(),
                    title: step.title.clone(),
                    instructions: step.instructions.clone(),
                    rework_requirement: current_loop_rework_requirement(
                        step.revision_context.as_deref(),
                        &loop_def.loop_key,
                        workflow_loop.retry_count,
                    ),
                    summary: payload.summary,
                    outputs: payload.outputs,
                    evidence: step
                        .content
                        .as_deref()
                        .and_then(|content| serde_json::from_str::<serde_json::Value>(content).ok())
                        .and_then(|value| value.get("evidence").cloned())
                        .and_then(|value| serde_json::from_value(value).ok())
                        .unwrap_or_default(),
                    user_skip_waiver: loop_skip_waiver(step, &loop_def.loop_key),
                })
            })
            .collect::<Result<Vec<_>, OrchestratorError>>()?;
        let review_scope_edges = plan_json
            .edges
            .iter()
            .filter(|edge| {
                review_scope.contains(edge.source.as_str()) && review_scope.contains(edge.target.as_str())
            })
            .map(|edge| {
                format!(
                    "- `{}` -> `{}` ({})",
                    edge.source,
                    edge.target,
                    edge.data.as_ref().map(|data| data.kind.as_str()).unwrap_or("hard")
                )
            })
            .collect::<Vec<_>>();

        let mut seen_upstream = HashSet::new();
        let required_upstream_results = plan_json
            .edges
            .iter()
            .filter(|edge| {
                review_scope.contains(edge.target.as_str())
                    && !review_scope.contains(edge.source.as_str())
                    && seen_upstream.insert(edge.source.as_str())
            })
            .map(|edge| {
                let step = step_by_key.get(edge.source.as_str()).ok_or_else(|| {
                    OrchestratorError::NotFound(format!(
                        "Loop '{}' 的必要上游步骤 '{}' 不存在",
                        loop_def.loop_key, edge.source
                    ))
                })?;
                let result = workflow_runtime::result_aggregation::final_node_result_from_step(step)
                    .ok_or_else(|| {
                        OrchestratorError::Runtime(
                            workflow_runtime::WorkflowRuntimeError::Validation(format!(
                                "Loop '{}' 的必要上游步骤 '{}' 缺少最新有效结果",
                                loop_def.loop_key, edge.source
                            )),
                        )
                    })?;
                if matches!(
                    result.status,
                    workflow_runtime::WorkflowTaskCompletionStatus::Blocked
                        | workflow_runtime::WorkflowTaskCompletionStatus::NeedsContext
                ) {
                    return Err(OrchestratorError::Runtime(
                        workflow_runtime::WorkflowRuntimeError::Validation(format!(
                            "Loop '{}' 的必要上游步骤 '{}' 尚未形成可审核结果",
                            loop_def.loop_key, edge.source
                        )),
                    ));
                }
                Ok(prompt_builders::common::UpstreamResultInput {
                    step_key: result.step_key,
                    summary: result.summary,
                    outputs: result.outputs,
                })
            })
            .collect::<Result<Vec<_>, OrchestratorError>>()?;

        Ok((
            inputs,
            LoopReviewPromptContext {
                reviewer_name: reviewer_name.to_string(),
                reviewer_role: match reviewer_type {
                    ReviewerType::Lead => "Lead".to_string(),
                    ReviewerType::Reviewer => "Reviewer".to_string(),
                    ReviewerType::User => "User".to_string(),
                },
                review_step_instructions: review_step.instructions.clone(),
                current_round: review_step.round_index,
                loop_retry_count: workflow_loop.retry_count,
                retry_budget: workflow_loop.max_retry,
                review_scope_edges,
                required_upstream_results,
            },
        ))
    }
}

pub(crate) fn parse_member_step_ids(raw: &str) -> Result<Vec<Uuid>, OrchestratorError> {
    serde_json::from_str::<Vec<Uuid>>(raw).map_err(OrchestratorError::Json)
}

fn has_pending_feedback_for_loop(step: &WorkflowStep, workflow_loop: &WorkflowLoop) -> bool {
    step.revision_context
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|context| context.get("pending_feedback").cloned())
        .is_some_and(|pending| {
            pending.get("scope").and_then(|value| value.as_str()) == Some("loop")
                && pending.get("loop_key").and_then(|value| value.as_str())
                    == Some(workflow_loop.loop_key.as_str())
        })
}

async fn loop_feedback_targets(
    pool: &SqlitePool,
    workflow_loop: &WorkflowLoop,
    step_feedbacks: &HashMap<String, String>,
    step_issue_ids: &HashMap<String, String>,
    loop_feedback: &str,
    loop_issue_id: &str,
) -> Result<Vec<LoopFeedbackTarget>, OrchestratorError> {
    let member_ids = parse_member_step_ids(&workflow_loop.member_step_ids_json)?;
    let member_id_set = member_ids.into_iter().collect::<HashSet<_>>();
    let all_steps = WorkflowStep::find_by_execution(pool, workflow_loop.execution_id).await?;
    let feedback_by_step_id =
        loop_feedback_by_step_id(&all_steps, &member_id_set, step_feedbacks, loop_feedback);

    Ok(all_steps
        .into_iter()
        .filter_map(|step| {
            let feedback = feedback_by_step_id.get(&step.id)?.clone();
            let raw_issue_id = if step_feedbacks.is_empty() {
                loop_issue_id
            } else {
                step_issue_ids.get(&step.step_key)?.as_str()
            };
            let issue_scope_id = loop_skip_issue_scope_id(workflow_loop, &step, raw_issue_id);
            if has_matching_active_skip_waiver_for_issue(
                &step,
                workflow_loop,
                &issue_scope_id,
                &feedback,
            ) {
                None
            } else {
                Some(LoopFeedbackTarget {
                    step,
                    issue_scope_id,
                    feedback,
                })
            }
        })
        .collect())
}

fn feedback_map_from_targets(
    feedback_targets: &[LoopFeedbackTarget],
) -> HashMap<String, String> {
    feedback_targets
        .iter()
        .map(|target| (target.step.step_key.clone(), target.feedback.clone()))
        .collect()
}

fn review_scope_step_keys_in_dag_order(
    review_scope_step_keys: &[String],
    edges: &[db::models::workflow_types::WorkflowPlanEdge],
) -> Vec<String> {
    let scope_keys = review_scope_step_keys
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let original_position = review_scope_step_keys
        .iter()
        .enumerate()
        .map(|(index, key)| (key.as_str(), index))
        .collect::<HashMap<_, _>>();
    let mut indegree = review_scope_step_keys
        .iter()
        .map(|key| (key.as_str(), 0_usize))
        .collect::<HashMap<_, _>>();
    let mut outgoing = HashMap::<&str, Vec<&str>>::new();

    for edge in edges.iter().filter(|edge| {
        scope_keys.contains(edge.source.as_str()) && scope_keys.contains(edge.target.as_str())
    }) {
        outgoing
            .entry(edge.source.as_str())
            .or_default()
            .push(edge.target.as_str());
        *indegree.entry(edge.target.as_str()).or_default() += 1;
    }

    let mut ready = indegree
        .iter()
        .filter_map(|(key, degree)| (*degree == 0).then_some(*key))
        .collect::<Vec<_>>();
    ready.sort_by_key(|key| original_position.get(key).copied().unwrap_or(usize::MAX));

    let mut ordered = Vec::with_capacity(review_scope_step_keys.len());
    while !ready.is_empty() {
        let step_key = ready.remove(0);
        ordered.push(step_key.to_string());
        for successor in outgoing.remove(step_key).unwrap_or_default() {
            let degree = indegree
                .get_mut(successor)
                .expect("review scope successor must have indegree");
            *degree -= 1;
            if *degree == 0 {
                ready.push(successor);
            }
        }
        ready.sort_by_key(|key| original_position.get(key).copied().unwrap_or(usize::MAX));
    }

    if ordered.len() == review_scope_step_keys.len() {
        ordered
    } else {
        // The compiler rejects cycles. Preserve the declared order defensively
        // if an invalid historical plan reaches runtime.
        review_scope_step_keys.to_vec()
    }
}

fn requires_user_acceptance_checkpoint(workflow_loop: &WorkflowLoop) -> bool {
    workflow_loop.user_review_required
}

fn rejected_loop_review_disposition(
    review_attempt: i32,
    max_review_attempts: i32,
    feedback_targets: &[LoopFeedbackTarget],
) -> RejectedLoopReviewDisposition {
    // A user waiver is authoritative only for its stable skipped-step issue
    // scope. When it covers every target there is nothing left to fail, even
    // on the final configured attempt.
    if feedback_targets.is_empty() {
        return RejectedLoopReviewDisposition::PassedByUserWaiver;
    }
    if loop_review_attempt_limit_reached(review_attempt, max_review_attempts) {
        return RejectedLoopReviewDisposition::LimitReached;
    }
    if feedback_targets
        .iter()
        .any(|target| target.step.status == WorkflowStepStatus::Skipped)
    {
        return RejectedLoopReviewDisposition::NeedsSkippedDecision;
    }
    RejectedLoopReviewDisposition::Retry
}

/// `max_retry` is the number of rework executions after the initial review.
/// Clamp historical negative values so old/corrupt rows still get exactly one
/// initial review rather than an impossible zero-attempt budget.
fn max_loop_review_attempts(workflow_loop: &WorkflowLoop) -> i32 {
    workflow_loop.max_retry.max(0).saturating_add(1)
}

fn loop_review_attempt_limit_reached(review_attempt: i32, max_review_attempts: i32) -> bool {
    review_attempt >= max_review_attempts
}

pub(crate) fn loop_skip_waiver(step: &WorkflowStep, loop_key: &str) -> Option<String> {
    if step.status != WorkflowStepStatus::Skipped {
        return None;
    }
    let active_waivers = loop_skip_waivers(step.revision_context.as_deref())
        .into_iter()
        .filter(|waiver| {
            waiver.get("loop_key").and_then(|value| value.as_str()) == Some(loop_key)
                && waiver.get("step_id").and_then(|value| value.as_str())
                    == Some(step.id.to_string().as_str())
                && waiver.get("status").and_then(|value| value.as_str()) == Some("active")
        })
        .filter_map(|waiver| {
            let feedback = waiver
                .get("feedback")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let issue_scope_id = waiver
                .get("issue_scope_id")
                .and_then(|value| value.as_str())
                .unwrap_or("legacy-waiver");
            (!feedback.is_empty())
                .then(|| format!("Issue scope `{issue_scope_id}` — {feedback}"))
        })
        .collect::<Vec<_>>();
    (!active_waivers.is_empty()).then(|| active_waivers.join("\n"))
}

#[cfg(test)]
pub(crate) fn merge_loop_skip_waiver_context(
    existing_revision_context: Option<&str>,
    workflow_loop: &WorkflowLoop,
    step: &WorkflowStep,
    feedback: &str,
) -> String {
    let issue_scope_id = loop_skip_issue_scope_id(
        workflow_loop,
        step,
        &feedback_issue_id(feedback),
    );
    merge_loop_skip_waiver_context_for_issue(
        existing_revision_context,
        workflow_loop,
        step,
        &issue_scope_id,
        feedback,
    )
}

fn merge_loop_skip_waiver_context_for_issue(
    existing_revision_context: Option<&str>,
    workflow_loop: &WorkflowLoop,
    step: &WorkflowStep,
    issue_scope_id: &str,
    feedback: &str,
) -> String {
    let mut context = existing_revision_context
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !context.is_object() {
        context = serde_json::json!({});
    }
    let object = context.as_object_mut().expect("revision context object");
    object.remove("pending_feedback");
    let waivers = object
        .entry("loop_skip_waivers")
        .or_insert_with(|| serde_json::json!([]));
    if !waivers.is_array() {
        *waivers = serde_json::json!([]);
    }
    let waiver_items = waivers.as_array_mut().expect("loop skip waivers array");
    let now = chrono::Utc::now().to_rfc3339();
    for waiver in waiver_items.iter_mut().filter(|waiver| {
        waiver.get("loop_id").and_then(|value| value.as_str())
            == Some(workflow_loop.id.to_string().as_str())
            && waiver.get("step_id").and_then(|value| value.as_str())
                == Some(step.id.to_string().as_str())
            && waiver
                .get("issue_scope_id")
                .and_then(|value| value.as_str())
                == Some(issue_scope_id)
            && waiver.get("status").and_then(|value| value.as_str()) == Some("active")
    }) {
        waiver["status"] = serde_json::json!("superseded");
        waiver["superseded_at"] = serde_json::json!(now.clone());
    }
    waiver_items.push(serde_json::json!({
            "loop_id": workflow_loop.id,
            "loop_key": workflow_loop.loop_key,
            "step_id": step.id,
            "step_key": step.step_key,
            "status": "active",
            "scope_id": loop_skip_scope_id(workflow_loop, step),
            "issue_scope_id": issue_scope_id,
            "feedback": feedback.trim(),
            "feedback_fingerprint": feedback_fingerprint(feedback),
            "decided_at": now,
        }));
    context.to_string()
}

pub(crate) fn supersede_loop_skip_waiver_context(
    existing_revision_context: Option<&str>,
    workflow_loop: &WorkflowLoop,
    step: &WorkflowStep,
) -> Option<String> {
    let mut context = existing_revision_context
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())?;
    let waivers = context.get_mut("loop_skip_waivers")?.as_array_mut()?;
    let now = chrono::Utc::now().to_rfc3339();
    let mut changed = false;
    for waiver in waivers.iter_mut().filter(|waiver| {
        waiver.get("loop_id").and_then(|value| value.as_str())
            == Some(workflow_loop.id.to_string().as_str())
            && waiver.get("step_id").and_then(|value| value.as_str())
                == Some(step.id.to_string().as_str())
            && waiver.get("status").and_then(|value| value.as_str()) == Some("active")
    }) {
        waiver["status"] = serde_json::json!("superseded");
        waiver["superseded_at"] = serde_json::json!(now);
        changed = true;
    }
    changed.then(|| context.to_string())
}

fn loop_skip_waivers(revision_context: Option<&str>) -> Vec<serde_json::Value> {
    revision_context
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|context| context.get("loop_skip_waivers").cloned())
        .and_then(|waivers| waivers.as_array().cloned())
        .unwrap_or_default()
}

fn feedback_fingerprint(feedback: &str) -> String {
    let normalized = feedback
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    format!("{:x}", Sha256::digest(normalized.as_bytes()))
}

pub(crate) fn has_matching_active_skip_waiver(
    step: &WorkflowStep,
    workflow_loop: &WorkflowLoop,
    feedback: &str,
) -> bool {
    let issue_scope_id = loop_skip_issue_scope_id(
        workflow_loop,
        step,
        &feedback_issue_id(feedback),
    );
    has_matching_active_skip_waiver_for_issue(
        step,
        workflow_loop,
        &issue_scope_id,
        feedback,
    )
}

fn has_matching_active_skip_waiver_for_issue(
    step: &WorkflowStep,
    workflow_loop: &WorkflowLoop,
    issue_scope_id: &str,
    feedback: &str,
) -> bool {
    if step.status != WorkflowStepStatus::Skipped {
        return false;
    }
    let expected_fingerprint = feedback_fingerprint(feedback);
    loop_skip_waivers(step.revision_context.as_deref())
        .into_iter()
        .any(|waiver| {
            let same_step = waiver.get("loop_id").and_then(|value| value.as_str())
                == Some(workflow_loop.id.to_string().as_str())
                && waiver.get("step_id").and_then(|value| value.as_str())
                    == Some(step.id.to_string().as_str());
            let same_issue = waiver
                .get("issue_scope_id")
                .and_then(|value| value.as_str())
                .map(|stored| stored == issue_scope_id)
                // Legacy waivers had no issue scope; keep them narrow by
                // requiring the original feedback fingerprint.
                .unwrap_or_else(|| {
                    waiver
                        .get("feedback_fingerprint")
                        .and_then(|value| value.as_str())
                        == Some(expected_fingerprint.as_str())
                });
            same_step
                && same_issue
                && waiver.get("status").and_then(|value| value.as_str()) == Some("active")
        })
}

fn loop_skip_scope_id(workflow_loop: &WorkflowLoop, step: &WorkflowStep) -> String {
    format!("loop:{}:step:{}", workflow_loop.id, step.id)
}

fn feedback_issue_id(feedback: &str) -> String {
    format!("feedback-{}", feedback_fingerprint(feedback))
}

pub(crate) fn loop_skip_issue_scope_id_for_feedback(
    workflow_loop: &WorkflowLoop,
    step: &WorkflowStep,
    feedback: &str,
) -> String {
    loop_skip_issue_scope_id(workflow_loop, step, &feedback_issue_id(feedback))
}

fn loop_skip_issue_scope_id(
    workflow_loop: &WorkflowLoop,
    step: &WorkflowStep,
    raw_issue_id: &str,
) -> String {
    let prefix = format!("loop:{}:step:{}:issue:", workflow_loop.id, step.id);
    if raw_issue_id.starts_with(&prefix) {
        return raw_issue_id.to_string();
    }
    let normalized = raw_issue_id
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let normalized = normalized.trim_matches('-');
    let compact = if normalized.is_empty() {
        format!("issue-{}", &feedback_fingerprint(raw_issue_id)[..40])
    } else if normalized.len() > 64 {
        format!(
            "{}-{}",
            &normalized[..48],
            &feedback_fingerprint(raw_issue_id)[..12]
        )
    } else {
        normalized.to_string()
    };
    format!("{prefix}{compact}")
}

async fn inject_feedback_to_steps(
    pool: &SqlitePool,
    workflow_loop: &WorkflowLoop,
    source: WorkflowRevisionFeedbackSource,
    loop_feedback: &str,
    step_feedbacks: &HashMap<String, String>,
) -> Result<(), OrchestratorError> {
    let member_ids = parse_member_step_ids(&workflow_loop.member_step_ids_json)?;
    let member_id_set = member_ids.iter().copied().collect::<HashSet<_>>();
    let all_steps = WorkflowStep::find_by_execution(pool, workflow_loop.execution_id).await?;
    let feedback_by_step_id =
        loop_feedback_by_step_id(&all_steps, &member_id_set, step_feedbacks, loop_feedback);

    for step in all_steps
        .iter()
        .filter(|step| member_id_set.contains(&step.id))
        .filter(|step| feedback_by_step_id.contains_key(&step.id))
    {
        let previous_payload =
            parse_summary_payload(step.summary_text.as_deref()).unwrap_or(SummaryPayload {
                summary: step.title.clone(),
                content: None,
                outputs: Vec::new(),
            });
        let feedback = feedback_by_step_id
            .get(&step.id)
            .cloned()
            .unwrap_or_else(|| loop_feedback.to_string());
        let context = merge_loop_revision_context(
            step.revision_context.as_deref(),
            source,
            &feedback,
            &previous_payload.summary,
            &previous_payload.outputs,
            workflow_loop.retry_count + 1,
            &workflow_loop.loop_key,
            loop_feedback,
            &other_loop_feedback_summary(step.id, &all_steps, &feedback_by_step_id),
        );
        WorkflowStep::update_revision_context(pool, step.id, Some(context)).await?;
    }

    Ok(())
}

pub(crate) async fn inject_feedback_to_steps_in_transaction(
    connection: &mut SqliteConnection,
    workflow_loop: &WorkflowLoop,
    source: WorkflowRevisionFeedbackSource,
    loop_feedback: &str,
    step_feedbacks: &HashMap<String, String>,
) -> Result<(), OrchestratorError> {
    let member_ids = parse_member_step_ids(&workflow_loop.member_step_ids_json)?;
    let member_id_set = member_ids.iter().copied().collect::<HashSet<_>>();
    let all_steps = sqlx::query_as::<_, WorkflowStep>(
        r#"
        SELECT id, execution_id, round_id, compiled_revision_id, step_key,
               step_type, title, instructions, assigned_workflow_agent_session_id,
               status, retry_count, max_retry, round_index, display_order,
               latest_run_id, summary_text, content, loop_id,
               lead_review_required, user_review_required, revision_context,
               created_at, updated_at, started_at, completed_at
        FROM chat_workflow_steps
        WHERE execution_id = ?1
        "#,
    )
    .bind(workflow_loop.execution_id)
    .fetch_all(&mut *connection)
    .await?;
    let feedback_by_step_id =
        loop_feedback_by_step_id(&all_steps, &member_id_set, step_feedbacks, loop_feedback);

    for step in all_steps
        .iter()
        .filter(|step| member_id_set.contains(&step.id))
        .filter(|step| feedback_by_step_id.contains_key(&step.id))
    {
        let previous_payload =
            parse_summary_payload(step.summary_text.as_deref()).unwrap_or(SummaryPayload {
                summary: step.title.clone(),
                content: None,
                outputs: Vec::new(),
            });
        let feedback = feedback_by_step_id
            .get(&step.id)
            .cloned()
            .unwrap_or_else(|| loop_feedback.to_string());
        let context = merge_loop_revision_context(
            step.revision_context.as_deref(),
            source,
            &feedback,
            &previous_payload.summary,
            &previous_payload.outputs,
            workflow_loop.retry_count + 1,
            &workflow_loop.loop_key,
            loop_feedback,
            &other_loop_feedback_summary(step.id, &all_steps, &feedback_by_step_id),
        );
        WorkflowStep::update_revision_context_in_transaction(
            &mut *connection,
            step.id,
            Some(context),
        )
        .await?;
    }

    Ok(())
}

fn loop_feedback_by_step_id(
    all_steps: &[WorkflowStep],
    member_id_set: &HashSet<Uuid>,
    step_feedbacks: &HashMap<String, String>,
    loop_feedback: &str,
) -> HashMap<Uuid, String> {
    all_steps
        .iter()
        .filter(|step| member_id_set.contains(&step.id))
        .filter_map(|step| {
            if step_feedbacks.is_empty() {
                return Some((step.id, loop_feedback.to_string()));
            }

            step_feedbacks
                .get(&step.step_key)
                .map(|feedback| (step.id, feedback.clone()))
        })
        .collect()
}

fn current_loop_rework_requirement(
    revision_context: Option<&str>,
    loop_key: &str,
    retry_count: i32,
) -> Option<String> {
    revision_context
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .and_then(|context| context.get("feedback_history").cloned())
        .and_then(|history| history.as_array().cloned())
        .and_then(|history| {
            history.into_iter().rev().find_map(|entry| {
                let matches_current_retry = entry.get("scope").and_then(|value| value.as_str())
                    == Some("loop")
                    && entry.get("loop_key").and_then(|value| value.as_str()) == Some(loop_key)
                    && entry.get("round").and_then(|value| value.as_i64())
                        == Some(i64::from(retry_count));
                matches_current_retry
                    .then(|| {
                        entry
                            .get("feedback")
                            .and_then(|value| value.as_str())
                            .map(str::trim)
                            .filter(|feedback| !feedback.is_empty())
                            .map(str::to_string)
                    })
                    .flatten()
            })
        })
}

#[allow(clippy::too_many_arguments)]
fn merge_loop_revision_context(
    existing_revision_context: Option<&str>,
    source: WorkflowRevisionFeedbackSource,
    feedback: &str,
    previous_summary: &str,
    previous_outputs: &[String],
    review_round: i32,
    loop_key: &str,
    loop_rejection_reason: &str,
    other_steps_feedback_summary: &[String],
) -> String {
    let mut context = existing_revision_context
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !context.is_object() {
        context = serde_json::json!({});
    }
    let source = match source {
            WorkflowRevisionFeedbackSource::Lead => "lead",
            WorkflowRevisionFeedbackSource::Reviewer => "reviewer",
            WorkflowRevisionFeedbackSource::User => "user",
    };
    let entry = serde_json::json!({
        "round": review_round,
        "source": source,
        "scope": "loop",
        "loop_key": loop_key,
        "feedback": feedback.trim(),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let object = context.as_object_mut().expect("revision context object");
    let history = object
        .entry("feedback_history")
        .or_insert_with(|| serde_json::json!([]));
    if !history.is_array() {
        *history = serde_json::json!([]);
    }
    history
        .as_array_mut()
        .expect("feedback history array")
        .push(entry);
    object.insert(
        "previous_summary".to_string(),
        serde_json::json!(previous_summary.trim()),
    );
    object.insert(
        "previous_outputs".to_string(),
        serde_json::json!(previous_outputs),
    );
    object.insert(
        "pending_feedback".to_string(),
        serde_json::json!({
            "source": source,
            "feedback": feedback.trim(),
            "previous_summary": previous_summary.trim(),
            "previous_outputs": previous_outputs,
            "review_round": review_round,
            "scope": "loop",
            "loop_key": loop_key,
            "loop_rejection_reason": loop_rejection_reason.trim(),
            "other_steps_feedback_summary": other_steps_feedback_summary,
        }),
    );
    context.to_string()
}

fn other_loop_feedback_summary(
    current_step_id: Uuid,
    all_steps: &[WorkflowStep],
    feedback_by_step_id: &HashMap<Uuid, String>,
) -> Vec<String> {
    all_steps
        .iter()
        .filter(|step| step.id != current_step_id)
        .filter_map(|step| {
            feedback_by_step_id
                .get(&step.id)
                .map(|feedback| format!("{}: {}", step.title, feedback))
        })
        .collect()
}
