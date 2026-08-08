async fn persist_workflow_runtime_transcript_line(
    pool: &SqlitePool,
    transcript_id: Uuid,
    execution_id: Uuid,
    workflow_agent_session_id: Option<Uuid>,
    step_id: Uuid,
    stream_type: &ChatStreamDeltaType,
    content: &str,
) -> Result<WorkflowTranscript, sqlx::Error> {
    WorkflowTranscript::create(
        pool,
        &CreateWorkflowTranscript {
            execution_id,
            round_id: None,
            workflow_agent_session_id,
            step_id: Some(step_id),
            sender_type: "agent".to_string(),
            entry_type: workflow_runtime_transcript_entry_type(stream_type).to_string(),
            content: content.to_string(),
            meta_json: Some(
                serde_json::json!({
                    "source": "workflow_runtime_stream",
                })
                .to_string(),
            ),
        },
        transcript_id,
    )
    .await
}

fn workflow_runtime_transcript_entry_type(
    stream_type: &ChatStreamDeltaType,
) -> &'static str {
    match stream_type {
        ChatStreamDeltaType::Assistant => "message",
        ChatStreamDeltaType::Thinking => "thinking",
        ChatStreamDeltaType::Error => "error",
    }
}

fn is_workflow_runtime_activity_stream_type(stream_type: &ChatStreamDeltaType) -> bool {
    matches!(
        stream_type,
        ChatStreamDeltaType::Thinking | ChatStreamDeltaType::Error
    )
}

fn extract_workflow_activity_lines_from_history(
    history: &[LogMsg],
) -> Vec<(ChatStreamDeltaType, String)> {
    let mut state = WorkflowRuntimeStreamState::default();
    let mut activity_lines = Vec::new();

    for message in history {
        let LogMsg::JsonPatch(patch) = message else {
            continue;
        };

        for (stream_type, line) in state.drain_patch_lines(patch) {
            if is_workflow_runtime_activity_stream_type(&stream_type) {
                activity_lines.push((stream_type, line));
            }
        }
    }

    for (stream_type, line) in state.flush_pending_lines() {
        if is_workflow_runtime_activity_stream_type(&stream_type) {
            activity_lines.push((stream_type, line));
        }
    }

    activity_lines
}

async fn persist_missing_workflow_runtime_activity_transcripts(
    pool: &SqlitePool,
    execution_id: Uuid,
    workflow_agent_session_id: Option<Uuid>,
    step_id: Uuid,
    history: &[LogMsg],
) -> Result<(), WorkflowRuntimeError> {
    let activity_lines = extract_workflow_activity_lines_from_history(history);
    if activity_lines.is_empty() {
        return Ok(());
    }

    let persisted_activity_types = WorkflowTranscript::find_by_step(pool, step_id)
        .await?
        .into_iter()
        .filter(|entry| {
            entry.workflow_agent_session_id == workflow_agent_session_id
                && entry.sender_type == "agent"
                && transcript_meta_value(entry)
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    == Some("workflow_runtime_stream")
        })
        .map(|entry| entry.entry_type)
        .collect::<HashSet<_>>();

    // Runtime history has no stable per-line persistence key. Replay only an
    // activity category that is entirely absent to avoid duplicating live rows.
    for (stream_type, line) in activity_lines {
        if persisted_activity_types.contains(workflow_runtime_transcript_entry_type(&stream_type)) {
            continue;
        }
        persist_workflow_runtime_transcript_line(
            pool,
            Uuid::new_v4(),
            execution_id,
            workflow_agent_session_id,
            step_id,
            &stream_type,
            &line,
        )
        .await?;
    }

    Ok(())
}

pub fn overlay_step_statuses(
    plan: &WorkflowPlanJson,
    steps: &[WorkflowStep],
) -> Vec<WorkflowPlanNode> {
    let step_by_key: HashMap<&str, &WorkflowStep> = steps
        .iter()
        .map(|step| (step.step_key.as_str(), step))
        .collect();

    plan.nodes
        .iter()
        .cloned()
        .map(|mut node| {
            if let Some(step) = step_by_key.get(node.id.as_str()) {
                node.data.status = Some(to_workflow_wire_value(&step.status));
            }
            node
        })
        .collect()
}

pub fn parse_summary_payload(summary_text: Option<&str>) -> Option<SummaryPayload> {
    let summary_text = summary_text?.trim();
    if summary_text.is_empty() {
        return None;
    }

    serde_json::from_str::<SummaryPayload>(summary_text)
        .ok()
        .or_else(|| {
            Some(SummaryPayload {
                summary: summary_text.to_string(),
                content: None,
                outputs: Vec::new(),
            })
        })
}

fn transcript_meta_value(transcript: &WorkflowTranscript) -> serde_json::Value {
    transcript
        .meta_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}
