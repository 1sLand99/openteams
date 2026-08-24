#[derive(Clone)]
struct RunLifecycleControl {
    run_id: Uuid,
    stop: CancellationToken,
}

#[cfg(feature = "qa-mode")]
#[derive(Default)]
struct ChatRunnerQaClaimGate {
    armed: AtomicBool,
    reached: AtomicBool,
    reached_notify: tokio::sync::Notify,
    release_notify: tokio::sync::Notify,
}

#[cfg(feature = "qa-mode")]
impl ChatRunnerQaClaimGate {
    async fn checkpoint(&self) {
        if !self.armed.swap(false, Ordering::SeqCst) {
            return;
        }
        self.reached.store(true, Ordering::SeqCst);
        self.reached_notify.notify_waiters();
        self.release_notify.notified().await;
        self.reached.store(false, Ordering::SeqCst);
    }
}

#[cfg(any(test, feature = "qa-mode"))]
#[derive(Default)]
struct ChatRunnerStopTransitionGate {
    armed: AtomicBool,
    reached: AtomicBool,
    reached_notify: tokio::sync::Notify,
    release_notify: tokio::sync::Notify,
}

#[cfg(any(test, feature = "qa-mode"))]
impl ChatRunnerStopTransitionGate {
    async fn checkpoint(&self) {
        if !self.armed.swap(false, Ordering::SeqCst) {
            return;
        }
        self.reached.store(true, Ordering::SeqCst);
        self.reached_notify.notify_waiters();
        self.release_notify.notified().await;
        self.reached.store(false, Ordering::SeqCst);
    }

    fn arm(&self) {
        self.reached.store(false, Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }

    async fn wait_until_reached(&self) {
        loop {
            let reached = self.reached_notify.notified();
            if self.reached.load(Ordering::SeqCst) {
                return;
            }
            reached.await;
        }
    }

    fn release(&self) {
        self.release_notify.notify_one();
    }
}

#[derive(Clone)]
struct DeliveryAwareApprovalBridge {
    inner: Arc<ExecutorApprovalBridge>,
    runner: ChatRunner,
    session_id: Uuid,
    session_agent_id: Uuid,
    run_id: Uuid,
}

impl DeliveryAwareApprovalBridge {
    async fn transition_delivery(
        &self,
        expected_status: QueuedMessageStatus,
        next_status: QueuedMessageStatus,
    ) -> Result<(), ExecutorApprovalError> {
        let service = QueuedMessageService::new();
        let delivery = service
            .find_by_run_id(&self.runner.db.pool, self.run_id)
            .await
            .map_err(ExecutorApprovalError::request_failed)?
            .ok_or_else(|| {
                ExecutorApprovalError::RequestFailed(format!(
                    "run {} has no bound delivery",
                    self.run_id
                ))
            })?;
        if delivery.status != expected_status {
            // Stop and terminal transitions win over a delayed approval callback.
            return Ok(());
        }
        let updated = service
            .transition_status_cas(
                &self.runner.db.pool,
                delivery.id,
                delivery.revision,
                expected_status,
                next_status,
            )
            .await
            .map_err(ExecutorApprovalError::request_failed)?;
        if updated.is_none() {
            return Err(ExecutorApprovalError::RequestFailed(format!(
                "delivery {} changed during approval transition",
                delivery.id
            )));
        }
        self.runner
            .emit_member_queue_update(self.session_id, self.session_agent_id)
            .await;
        Ok(())
    }

    async fn enter_waiting(&self) -> Result<(), ExecutorApprovalError> {
        self.transition_delivery(
            QueuedMessageStatus::Running,
            QueuedMessageStatus::WaitingApproval,
        )
        .await
    }

    async fn leave_waiting(&self) -> Result<(), ExecutorApprovalError> {
        self.transition_delivery(
            QueuedMessageStatus::WaitingApproval,
            QueuedMessageStatus::Running,
        )
        .await
    }
}

#[async_trait]
impl ExecutorApprovalService for DeliveryAwareApprovalBridge {
    async fn request_tool_approval(
        &self,
        tool_name: &str,
        tool_input: serde_json::Value,
        tool_call_id: &str,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ApprovalStatus, ExecutorApprovalError> {
        self.enter_waiting().await?;
        let result = self
            .inner
            .request_tool_approval(tool_name, tool_input, tool_call_id, cancel)
            .await;
        self.leave_waiting().await?;
        result
    }

    async fn request_acp_tool_approval(
        &self,
        request: ExecutorApprovalRequest,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, ExecutorApprovalError> {
        self.enter_waiting().await?;
        let result = self.inner.request_acp_tool_approval(request, cancel).await;
        self.leave_waiting().await?;
        result
    }
}

enum LifecycleEvent {
    ProcessExited(std::io::Result<std::process::ExitStatus>),
    ExitSignal(executors::executors::ExecutorExitResult),
    StopRequested,
    SessionInactivityTimeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedSessionMember {
    session_id: Uuid,
    session_agent_id: Uuid,
    agent_id: Uuid,
    project_member_id: Option<Uuid>,
    member_name: String,
}

#[derive(Debug, Clone)]
struct ResolvedMessageDelivery {
    member: ResolvedSessionMember,
    ordinal: i64,
    route_kind: db::models::chat_message_target::ChatMessageTargetRouteKind,
    resolution_status:
        db::models::chat_message_target::ChatMessageTargetResolutionStatus,
}

#[derive(Debug, Clone)]
pub struct PersistedChatMessageDeliveryBundle {
    pub message: ChatMessage,
    pub deliveries: Vec<QueuedMessage>,
    pub revision: i64,
    pub created: bool,
}

struct AgentRunTarget {
    member: ResolvedSessionMember,
    claimed_queue_id: Option<Uuid>,
}

#[derive(Debug)]
enum DispatchOutcome {
    Started { run_id: Uuid },
    Queued { queue_id: Uuid },
    Rejected { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunCompletionStatus {
    Succeeded,
    Failed,
    Stopped,
}

impl RunCompletionStatus {
    fn as_u8(self) -> u8 {
        match self {
            Self::Succeeded => 0,
            Self::Failed => 1,
            Self::Stopped => 2,
        }
    }

    fn from_atomic(value: &AtomicU8) -> Self {
        match value.load(Ordering::Relaxed) {
            1 => Self::Failed,
            2 => Self::Stopped,
            _ => Self::Succeeded,
        }
    }

    fn store(self, value: &AtomicU8) {
        value.store(self.as_u8(), Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct ChatRunner {
    db: DBService,
    analytics: Option<AnalyticsService>,
    analytics_enabled: Arc<AtomicBool>,
    streams: Arc<DashMap<Uuid, broadcast::Sender<ChatStreamEvent>>>,
    // Store per-run lifecycle controls, key = session_agent_id
    run_controls: Arc<DashMap<Uuid, RunLifecycleControl>>,
    // Session-level background context compaction dedupe.
    // At most one compaction task per session is allowed at a time.
    background_compaction_inflight: Arc<DashMap<Uuid, ()>>,
    workspace_live_log_bytes: Arc<DashMap<String, u64>>,
    workspace_janitor_locks: Arc<DashMap<String, Arc<Mutex<()>>>>,
    #[cfg(any(test, feature = "qa-mode"))]
    stop_after_queue_binding: Arc<AtomicBool>,
    #[cfg(feature = "qa-mode")]
    qa_claim_gate: Arc<ChatRunnerQaClaimGate>,
    #[cfg(test)]
    executor_spawn_attempts: Arc<AtomicUsize>,
    #[cfg(test)]
    block_executor_spawn: Arc<AtomicBool>,
    #[cfg(any(test, feature = "qa-mode"))]
    stop_transition_gate: Arc<ChatRunnerStopTransitionGate>,
    #[cfg(test)]
    mcp_preparation_diagnostic: Arc<StdMutex<Option<String>>>,
}

impl ChatRunner {
    pub fn new(db: DBService) -> Self {
        Self::with_analytics(db, None, Arc::new(AtomicBool::new(true)))
    }

    pub fn with_analytics(
        db: DBService,
        analytics: Option<AnalyticsService>,
        analytics_enabled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            db,
            analytics,
            analytics_enabled,
            streams: Arc::new(DashMap::new()),
            run_controls: Arc::new(DashMap::new()),
            background_compaction_inflight: Arc::new(DashMap::new()),
            workspace_live_log_bytes: Arc::new(DashMap::new()),
            workspace_janitor_locks: Arc::new(DashMap::new()),
            #[cfg(any(test, feature = "qa-mode"))]
            stop_after_queue_binding: Arc::new(AtomicBool::new(false)),
            #[cfg(feature = "qa-mode")]
            qa_claim_gate: Arc::new(ChatRunnerQaClaimGate::default()),
            #[cfg(test)]
            executor_spawn_attempts: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            block_executor_spawn: Arc::new(AtomicBool::new(false)),
            #[cfg(any(test, feature = "qa-mode"))]
            stop_transition_gate: Arc::new(ChatRunnerStopTransitionGate::default()),
            #[cfg(test)]
            mcp_preparation_diagnostic: Arc::new(StdMutex::new(None)),
        }
    }

    /// Arm the qa-mode runner immediately after it has claimed a delivery and registered its
    /// run control, but before any run binding can commit.
    #[cfg(feature = "qa-mode")]
    pub fn qa_pause_after_delivery_claim(&self) {
        self.qa_claim_gate.reached.store(false, Ordering::SeqCst);
        self.qa_claim_gate.armed.store(true, Ordering::SeqCst);
    }

    #[cfg(feature = "qa-mode")]
    pub async fn qa_wait_for_delivery_claim(&self) {
        loop {
            let reached = self.qa_claim_gate.reached_notify.notified();
            if self.qa_claim_gate.reached.load(Ordering::SeqCst) {
                return;
            }
            reached.await;
        }
    }

    #[cfg(feature = "qa-mode")]
    pub fn qa_release_delivery_claim(&self) {
        self.qa_claim_gate.release_notify.notify_one();
    }

    /// Stop the qa-mode lifecycle after the atomic bind commit, before an executor is spawned.
    #[cfg(feature = "qa-mode")]
    pub fn qa_stop_after_delivery_bind(&self, enabled: bool) {
        self.stop_after_queue_binding
            .store(enabled, Ordering::SeqCst);
    }

    /// Pause qa-mode stop after it reads the active delivery and run control, before the atomic
    /// delivery/member stopping transition. This makes terminal-vs-stop ordering deterministic.
    #[cfg(feature = "qa-mode")]
    pub fn qa_pause_before_stop_transition(&self) {
        self.stop_transition_gate.arm();
    }

    #[cfg(feature = "qa-mode")]
    pub async fn qa_wait_for_stop_transition(&self) {
        self.stop_transition_gate.wait_until_reached().await;
    }

    #[cfg(feature = "qa-mode")]
    pub fn qa_release_stop_transition(&self) {
        self.stop_transition_gate.release();
    }

    #[cfg(test)]
    pub(crate) fn set_mcp_preparation_diagnostic_for_test(&self, diagnostic: Option<String>) {
        *self
            .mcp_preparation_diagnostic
            .lock()
            .expect("test MCP preparation diagnostic lock") = diagnostic;
    }

    #[cfg(test)]
    pub(crate) fn inject_mcp_preparation_diagnostic_for_test(&self, env: &mut ExecutionEnv) {
        if let Some(diagnostic) = self
            .mcp_preparation_diagnostic
            .lock()
            .expect("test MCP preparation diagnostic lock")
            .clone()
        {
            env.insert(
                crate::services::member_execution::TEST_MCP_PREPARATION_DIAGNOSTIC_ENV,
                diagnostic,
            );
        }
    }

    pub fn analytics_service(&self) -> Option<&AnalyticsService> {
        workflow_analytics::analytics_if_enabled(
            self.analytics.as_ref(),
            self.analytics_enabled.load(Ordering::Relaxed),
        )
    }

    fn analytics_projector(&self) -> AnalyticsProjector<'_> {
        AnalyticsProjector::new(
            &self.db.pool,
            self.analytics.as_ref(),
            self.analytics_enabled.load(Ordering::Relaxed),
        )
    }

    async fn ensure_openteams_ignored_for_git_workspace(
        workspace_path: &Path,
    ) -> Result<(), ChatRunnerError> {
        let output = Command::new("git")
            .arg("-C")
            .arg(workspace_path)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .await?;

        if !output.status.success() {
            return Ok(());
        }

        let repo_root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if repo_root.is_empty() {
            return Ok(());
        }

        let gitignore_path = PathBuf::from(repo_root).join(".gitignore");
        let existing = match fs::read_to_string(&gitignore_path).await {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err.into()),
        };

        let already_present = existing.lines().map(str::trim).any(|line| {
            matches!(
                line,
                ".openteams/" | "/.openteams/" | ".openteams" | "/.openteams"
            )
        });

        if already_present {
            return Ok(());
        }

        let mut updated = existing;
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(OPENTEAMS_GITIGNORE_ENTRY);
        updated.push('\n');

        fs::write(&gitignore_path, updated).await?;
        Ok(())
    }

    pub async fn recover_orphaned_session_agents(&self) -> Result<usize, ChatRunnerError> {
        let active_agents = ChatSessionAgent::find_all_active(&self.db.pool).await?;
        let unbound_processing = QueuedMessageService::new()
            .list_unbound_processing(&self.db.pool)
            .await?;
        let queued_members = QueuedMessageService::new()
            .list_members_with_queued(&self.db.pool)
            .await?;
        let mut runtime_reset_targets = active_agents
            .iter()
            .map(|session_agent| session_agent.id)
            .collect::<HashSet<_>>();
        let mut recovery_targets: HashMap<Uuid, ChatSessionAgent> = active_agents
            .into_iter()
            .map(|session_agent| (session_agent.id, session_agent))
            .collect();

        for entry in unbound_processing {
            runtime_reset_targets.insert(entry.session_agent_id);
            if recovery_targets.contains_key(&entry.session_agent_id) {
                continue;
            }
            let Some(session_agent) =
                ChatSessionAgent::find_by_id(&self.db.pool, entry.session_agent_id).await?
            else {
                tracing::warn!(
                    queue_id = %entry.id,
                    session_agent_id = %entry.session_agent_id,
                    "unbound processing queue row references a missing session agent"
                );
                continue;
            };
            recovery_targets.insert(session_agent.id, session_agent);
        }

        for session_agent_id in queued_members {
            if recovery_targets.contains_key(&session_agent_id) {
                continue;
            }
            let Some(session_agent) =
                ChatSessionAgent::find_by_id(&self.db.pool, session_agent_id).await?
            else {
                tracing::warn!(
                    session_agent_id = %session_agent_id,
                    "queued message references a missing session agent"
                );
                continue;
            };
            recovery_targets.insert(session_agent.id, session_agent);
        }

        for session_agent in recovery_targets.values() {
            let reset_runtime = runtime_reset_targets.contains(&session_agent.id);
            let recovered = if reset_runtime {
                ChatSessionAgent::reset_runtime_state(
                    &self.db.pool,
                    session_agent.id,
                    ChatSessionAgentState::Idle,
                )
                .await?
            } else {
                session_agent.clone()
            };
            self.run_controls.remove(&session_agent.id);

            // A run that was in flight when the backend died left its queue row stranded in
            // `processing`/`running`; reset it to `queued` so the persisted queue can resume.
            if reset_runtime {
                match QueuedMessageService::new()
                    .requeue_stale_inflight(&self.db.pool, recovered.id)
                    .await
                {
                    Ok(rows) if rows > 0 => {
                        self.emit_member_queue_update(recovered.session_id, recovered.id)
                            .await;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(
                            session_agent_id = %recovered.id,
                            error = %err,
                            "failed to requeue stale in-flight queue rows during recovery"
                        );
                    }
                }
            }

            if reset_runtime {
                tracing::warn!(
                    session_id = %recovered.session_id,
                    session_agent_id = %recovered.id,
                    agent_id = %recovered.agent_id,
                    previous_state = ?session_agent.state,
                    "Recovered orphaned chat session agent left active after backend interruption"
                );
            }

            // Resume the persisted member queue from the database.
            let runner = self.clone();
            let session_id = recovered.session_id;
            let session_agent_id = recovered.id;
            tokio::spawn(async move {
                runner
                    .dispatch_next_queued_message(session_id, session_agent_id)
                    .await;
            });
        }

        Ok(recovery_targets.len())
    }

    pub fn subscribe(&self, session_id: Uuid) -> broadcast::Receiver<ChatStreamEvent> {
        self.sender_for(session_id).subscribe()
    }

    pub fn emit_message_new(&self, session_id: Uuid, message: ChatMessage) {
        self.emit(session_id, ChatStreamEvent::MessageNew { message });
    }

    pub fn emit_message_updated(&self, session_id: Uuid, message: ChatMessage) {
        self.emit(session_id, ChatStreamEvent::MessageUpdated { message });
    }

    pub fn emit_work_item_new(&self, session_id: Uuid, work_item: ChatWorkItem) {
        self.emit(session_id, ChatStreamEvent::WorkItemNew { work_item });
    }

    pub fn emit_queue_update(&self, session_id: Uuid, queue: MemberQueueSnapshot) {
        self.emit(
            session_id,
            ChatStreamEvent::QueueUpdated {
                session_id,
                session_agent_id: queue.session_agent_id,
                queue,
            },
        );
    }

    async fn emit_member_queue_update(&self, session_id: Uuid, session_agent_id: Uuid) {
        let Some(session_agent) =
            (match ChatSessionAgent::find_by_id(&self.db.pool, session_agent_id).await {
                Ok(agent) => agent,
                Err(err) => {
                    tracing::warn!(
                        session_id = %session_id,
                        session_agent_id = %session_agent_id,
                        error = %err,
                        "failed to load member before queue update event"
                    );
                    return;
                }
            })
        else {
            return;
        };

        match QueuedMessageService::new()
            .snapshot_for_member(
                &self.db.pool,
                session_id,
                session_agent.id,
                session_agent.agent_id,
            )
            .await
        {
            Ok(snapshot) => self.emit_queue_update(session_id, snapshot),
            Err(err) => {
                tracing::warn!(
                    session_id = %session_id,
                    session_agent_id = %session_agent_id,
                    error = %err,
                    "failed to build queue update event"
                );
            }
        }
    }

    /// Emit a one-shot file-change refresh signal after an agent message
    /// completes. Fired exactly once per run (at the terminal completion point),
    /// so a single agent message triggers a single refresh.
    pub fn emit_file_change_refresh(
        &self,
        session_id: Uuid,
        session_agent_id: Uuid,
        agent_id: Uuid,
        run_id: Uuid,
        message_id: Uuid,
        changed_files: Vec<FileChangeEntry>,
    ) {
        self.emit(
            session_id,
            ChatStreamEvent::FileChangeRefresh {
                session_id,
                session_agent_id,
                agent_id,
                run_id,
                message_id,
                changed_files,
                ts: Utc::now(),
            },
        );
    }

    pub fn emit_workflow_execution_updated(&self, session_id: Uuid, execution_id: Uuid) {
        self.emit(
            session_id,
            ChatStreamEvent::WorkflowExecutionUpdated {
                session_id,
                execution_id,
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn emit_workflow_graph_updated(
        &self,
        session_id: Uuid,
        execution_id: Uuid,
        graph_version: String,
        reason: String,
        nodes: Vec<WorkflowPlanNode>,
        edges: Vec<WorkflowPlanEdge>,
        changed_step_ids: Vec<String>,
    ) {
        self.emit(
            session_id,
            ChatStreamEvent::WorkflowGraphUpdated {
                session_id,
                execution_id,
                graph_version,
                reason,
                nodes,
                edges,
                changed_step_ids,
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn emit_workflow_runtime_line(
        &self,
        line_id: Uuid,
        session_id: Uuid,
        execution_id: Uuid,
        workflow_agent_session_id: Option<Uuid>,
        step_id: Uuid,
        step_key: String,
        agent_id: Uuid,
        agent_name: String,
        stream_type: ChatStreamDeltaType,
        content: String,
        created_at: String,
    ) {
        self.emit(
            session_id,
            ChatStreamEvent::WorkflowRuntimeLine {
                line_id,
                session_id,
                execution_id,
                workflow_agent_session_id,
                step_id,
                step_key,
                agent_id,
                agent_name,
                stream_type,
                content,
                created_at,
            },
        );
    }

    /// Update the mention_statuses field in a message's meta
    async fn update_mention_status(
        &self,
        message_id: Uuid,
        agent_name: &str,
        status: &str,
        session_agent: Option<&ChatSessionAgent>,
    ) {
        // Fetch the current message
        let Ok(Some(message)) = ChatMessage::find_by_id(&self.db.pool, message_id).await else {
            tracing::warn!(
                message_id = %message_id,
                "failed to fetch message for mention status update"
            );
            return;
        };

        // Update the meta with new mention status
        let mut meta = message.meta.0.clone();
        let mention_statuses = meta
            .get_mut("mention_statuses")
            .and_then(|v| v.as_object_mut());

        if let Some(statuses) = mention_statuses {
            statuses.insert(agent_name.to_string(), serde_json::json!(status));
        } else {
            let mut new_statuses = serde_json::Map::new();
            new_statuses.insert(agent_name.to_string(), serde_json::json!(status));
            meta["mention_statuses"] = serde_json::Value::Object(new_statuses);
        }
        if let Some(session_agent) = session_agent
            && let Some(meta_object) = meta.as_object_mut()
        {
            let targets = meta_object
                .entry("mention_targets")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(targets) = targets.as_object_mut() {
                targets.insert(
                    session_agent.id.to_string(),
                    serde_json::json!({
                        "agent_id": session_agent.agent_id,
                        "project_member_id": session_agent.project_member_id,
                        "member_name": session_agent.member_name,
                        "status": status,
                        "error": null,
                    }),
                );
            }
        }

        // Persist the updated meta
        if let Err(err) = ChatMessage::update_meta(&self.db.pool, message_id, meta).await {
            tracing::warn!(
                message_id = %message_id,
                error = %err,
                "failed to update message mention status"
            );
        }
    }

    fn mention_status_as_str(status: &MentionStatus) -> &'static str {
        match status {
            MentionStatus::Received => "received",
            MentionStatus::Running => "running",
            MentionStatus::Completed => "completed",
            MentionStatus::Failed => "failed",
        }
    }

    async fn set_mention_status(
        &self,
        session_id: Uuid,
        message_id: Uuid,
        agent_name: &str,
        agent_id: Option<Uuid>,
        session_agent: Option<&ChatSessionAgent>,
        status: MentionStatus,
    ) {
        self.update_mention_status(
            message_id,
            agent_name,
            Self::mention_status_as_str(&status),
            session_agent,
        )
        .await;

        if let Some(agent_id) = agent_id {
            self.emit(
                session_id,
                ChatStreamEvent::MentionAcknowledged {
                    session_id,
                    message_id,
                    session_agent_id: session_agent.map(|member| member.id),
                    project_member_id: session_agent.and_then(|member| member.project_member_id),
                    mentioned_agent: agent_name.to_string(),
                    agent_id,
                    status,
                },
            );
        }
    }

    async fn report_missing_member_mention(
        &self,
        session_id: Uuid,
        message: &ChatMessage,
        mention: &str,
    ) {
        self.update_mention_status(
            message.id,
            mention,
            Self::mention_status_as_str(&MentionStatus::Failed),
            None,
        )
        .await;
        self.emit(
            session_id,
            ChatStreamEvent::MentionError {
                session_id,
                message_id: message.id,
                client_message_id: Self::extract_client_message_id(&message.meta),
                session_agent_id: None,
                project_member_id: None,
                agent_name: mention.to_string(),
                agent_id: None,
                reason: "member_not_found".to_string(),
            },
        );
        let member_handle = if mention.starts_with('@') {
            mention.to_string()
        } else {
            format!("@{mention}")
        };
        let system_meta = serde_json::json!({
            "mention_failure": {
                "source_message_id": message.id,
                "mentioned_agent": mention,
                "reason": "member_not_found",
            },
            "i18n": {
                "key": "message.memberNotFound",
                "params": {
                    "member": member_handle,
                },
            },
        });
        match chat::create_message(
            &self.db.pool,
            session_id,
            ChatSenderType::System,
            None,
            format!("Member {member_handle} does not exist."),
            Some(system_meta),
        )
        .await
        {
            Ok(system_message) => self.emit_message_new(session_id, system_message),
            Err(err) => tracing::warn!(
                error = %err,
                mention = mention,
                session_id = %session_id,
                "failed to persist missing-member system message"
            ),
        }
        tracing::warn!(
            mention = mention,
            session_id = %session_id,
            "mentioned chat member does not exist"
        );
    }

    async fn report_mention_failure(
        &self,
        session_id: Uuid,
        message_id: Uuid,
        agent_name: &str,
        agent_id: Option<Uuid>,
        session_agent: Option<&ChatSessionAgent>,
        reason: impl Into<String>,
    ) {
        let reason = reason.into();
        let compact_reason = reason
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let compact_reason = if compact_reason.is_empty() {
            "Unknown error".to_string()
        } else {
            compact_reason.clone()
        };

        tracing::debug!(
            session_id = %session_id,
            message_id = %message_id,
            agent_name = %agent_name,
            agent_id = ?agent_id,
            compact_reason = %compact_reason,
            full_reason_len = reason.len(),
            "[chat_runner] Reporting mention failure"
        );

        self.set_mention_status(
            session_id,
            message_id,
            agent_name,
            agent_id,
            session_agent,
            MentionStatus::Failed,
        )
        .await;

        let mut client_message_id = None;
        if let Ok(Some(msg)) = ChatMessage::find_by_id(&self.db.pool, message_id).await {
            client_message_id = Self::extract_client_message_id(&msg.meta);
            let mut meta = msg.meta.0.clone();
            if let Some(meta_obj) = meta.as_object_mut() {
                let mention_errors = meta_obj
                    .entry("mention_errors")
                    .or_insert_with(|| serde_json::json!({}));
                if let Some(errors) = mention_errors.as_object_mut() {
                    let mut error_info = serde_json::json!({
                        "reason": compact_reason.clone(),
                    });
                    if let Some(aid) = agent_id {
                        error_info["agent_id"] = serde_json::json!(aid);
                    }
                    errors.insert(agent_name.to_string(), error_info);
                }
                if let Some(session_agent) = session_agent {
                    let targets = meta_obj
                        .entry("mention_targets")
                        .or_insert_with(|| serde_json::json!({}));
                    if let Some(target) = targets
                        .as_object_mut()
                        .and_then(|targets| targets.get_mut(&session_agent.id.to_string()))
                        .and_then(serde_json::Value::as_object_mut)
                    {
                        target.insert(
                            "error".to_string(),
                            serde_json::Value::String(compact_reason.clone()),
                        );
                    }
                }
            }
            let _ = ChatMessage::update_meta(&self.db.pool, message_id, meta).await;
        }

        self.emit(
            session_id,
            ChatStreamEvent::MentionError {
                session_id,
                message_id,
                client_message_id,
                session_agent_id: session_agent.map(|member| member.id),
                project_member_id: session_agent.and_then(|member| member.project_member_id),
                agent_name: agent_name.to_string(),
                agent_id,
                reason: compact_reason.clone(),
            },
        );

        InboxService::new()
            .notify_chat_mention_failed(
                &self.db.pool,
                session_id,
                message_id,
                agent_name,
                agent_id,
                &compact_reason,
            )
            .await;

        let mut failure_meta = serde_json::json!({
            "mention_failure": {
                "source_message_id": message_id,
                "mentioned_agent": agent_name,
                "reason": compact_reason.clone(),
            }
        });

        if let Some(value) = agent_id {
            failure_meta["mention_failure"]["agent_id"] = serde_json::json!(value);
        }

        let system_content = format!(
            "Agent \"{}\" failed to execute this mention: {}",
            agent_name, compact_reason
        );

        match chat::create_message(
            &self.db.pool,
            session_id,
            ChatSenderType::System,
            None,
            system_content,
            Some(failure_meta),
        )
        .await
        {
            Ok(message) => self.emit_message_new(session_id, message),
            Err(err) => {
                tracing::warn!(
                    session_id = %session_id,
                    message_id = %message_id,
                    agent_name = %agent_name,
                    error = %err,
                    "failed to emit mention failure system message"
                );
            }
        }
    }

    async fn resolve_default_member_for_user_payload(
        &self,
        session: &ChatSession,
        mentions: &[String],
        meta: &serde_json::Value,
    ) -> Result<Option<ResolvedSessionMember>, ChatRunnerError> {
        if !mentions.is_empty()
            || meta
                .get("chat_input_mode")
                .and_then(serde_json::Value::as_str)
                != Some("workflow")
        {
            return Ok(None);
        }

        let session_agents =
            ChatSessionAgent::find_all_for_session(&self.db.pool, session.id).await?;
        if session_agents.is_empty() {
            return Ok(None);
        }
        let agents = ChatAgent::find_all(&self.db.pool).await?;
        match resolve_lead_agent(session, &session_agents, &agents) {
            Ok((_lead_agent, lead_session_agent)) => {
                Ok(Some(Self::resolved_session_member(lead_session_agent)))
            }
            Err(_) => Ok(None),
        }
    }

    async fn resolve_message_deliveries(
        &self,
        session: &ChatSession,
        sender_type: ChatSenderType,
        sender_session_agent_id: Option<Uuid>,
        mentions: &[String],
        meta: &serde_json::Value,
    ) -> Result<(Vec<ResolvedMessageDelivery>, Vec<String>), ChatRunnerError> {
        let chain_depth = meta
            .get("chain_depth")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0) as u32;
        let max_agent_chain_depth = config::load_config_from_file(&config_path())
            .await
            .max_agent_chain_depth
            .max(1);
        if chain_depth >= max_agent_chain_depth {
            return Ok((Vec::new(), Vec::new()));
        }

        if sender_type == ChatSenderType::User
            && let Some(default_member) = self
                .resolve_default_member_for_user_payload(session, mentions, meta)
                .await?
        {
            return Ok((
                vec![ResolvedMessageDelivery {
                    member: default_member,
                    ordinal: 0,
                    route_kind:
                        db::models::chat_message_target::ChatMessageTargetRouteKind::DefaultLead,
                    resolution_status: db::models::chat_message_target::ChatMessageTargetResolutionStatus::Resolved,
                }],
                Vec::new(),
            ));
        }

        let mut resolved = Vec::with_capacity(mentions.len());
        let mut missing = Vec::new();
        let mut seen_members = HashSet::with_capacity(mentions.len());
        for (ordinal, mention) in mentions.iter().enumerate() {
            if sender_type == ChatSenderType::Agent
                && mention.eq_ignore_ascii_case(RESERVED_USER_HANDLE)
            {
                continue;
            }
            match self
                .resolve_session_agent_for_mention(session.id, mention)
                .await?
            {
                Some(member) if seen_members.insert(member.session_agent_id) => {
                    let resolution_status = if sender_type == ChatSenderType::Agent
                        && sender_session_agent_id == Some(member.session_agent_id)
                    {
                        db::models::chat_message_target::ChatMessageTargetResolutionStatus::Rejected
                    } else {
                        db::models::chat_message_target::ChatMessageTargetResolutionStatus::Resolved
                    };
                    resolved.push(ResolvedMessageDelivery {
                        member,
                        ordinal: ordinal as i64,
                        route_kind: if sender_type == ChatSenderType::Agent {
                            db::models::chat_message_target::ChatMessageTargetRouteKind::AgentProtocol
                        } else {
                            db::models::chat_message_target::ChatMessageTargetRouteKind::ExplicitMention
                        },
                        resolution_status,
                    });
                }
                Some(_) => {}
                None => missing.push(mention.clone()),
            }
        }
        Ok((resolved, missing))
    }

    fn delivery_target_inputs(
        resolved: &[ResolvedMessageDelivery],
    ) -> Vec<chat::ResolvedChatMessageTarget> {
        resolved
            .iter()
            .map(|target| chat::ResolvedChatMessageTarget {
                ordinal: target.ordinal,
                session_agent_id: target.member.session_agent_id,
                project_member_id: target.member.project_member_id,
                agent_id: target.member.agent_id,
                member_name_snapshot: target.member.member_name.clone(),
                route_kind: target.route_kind,
                resolution_status: target.resolution_status,
            })
            .collect()
    }

    fn activate_persisted_message_bundle(
        &self,
        session_id: Uuid,
        message: ChatMessage,
        resolved: Vec<ResolvedMessageDelivery>,
        missing: Vec<String>,
        dispatch_deliveries: Vec<db::models::chat_message_queue::ChatMessageQueue>,
        created: bool,
    ) {
        if created {
            self.emit_message_new(session_id, message.clone());
        }

        let runner = self.clone();
        tokio::spawn(async move {
            let mut member_ids = HashSet::new();
            for target in &resolved {
                if target.resolution_status
                    != db::models::chat_message_target::ChatMessageTargetResolutionStatus::Resolved
                {
                    continue;
                }
                member_ids.insert(target.member.session_agent_id);
                if created {
                    match ChatSessionAgent::find_by_id(
                        &runner.db.pool,
                        target.member.session_agent_id,
                    )
                    .await
                    {
                        Ok(Some(session_agent)) => {
                            runner
                                .set_mention_status(
                                    session_id,
                                    message.id,
                                    &target.member.member_name,
                                    Some(target.member.agent_id),
                                    Some(&session_agent),
                                    MentionStatus::Received,
                                )
                                .await;
                        }
                        Ok(None) => {}
                        Err(err) => tracing::warn!(
                            session_agent_id = %target.member.session_agent_id,
                            error = %err,
                            "failed to load persisted delivery target after message commit"
                        ),
                    }
                }
            }
            for delivery in &dispatch_deliveries {
                member_ids.insert(delivery.session_agent_id);
            }
            for session_agent_id in member_ids {
                runner
                    .emit_member_queue_update(session_id, session_agent_id)
                    .await;
            }

            for delivery in dispatch_deliveries {
                let runner = runner.clone();
                let entry = QueuedMessageService::from_row(delivery);
                tokio::spawn(async move {
                    runner
                        .dispatch_queued_entry(entry.session_id, entry.session_agent_id, entry)
                        .await;
                });
            }
            if created {
                for mention in missing {
                    runner
                        .report_missing_member_mention(session_id, &message, &mention)
                        .await;
                }
            }
        });
    }

    /// Persist the message, resolved targets and delivery ledger rows atomically, then wake the
    /// runner after commit. User idempotency replays repair any legacy partial delivery bundle.
    pub async fn persist_and_dispatch_message(
        &self,
        session: &ChatSession,
        sender_type: ChatSenderType,
        sender_id: Option<Uuid>,
        content: String,
        mut meta: Option<serde_json::Value>,
        message_id: Uuid,
    ) -> Result<PersistedChatMessageDeliveryBundle, ChatRunnerError> {
        let client_message_id = if sender_type == ChatSenderType::User {
            chat::normalized_client_message_id(meta.as_ref())?
        } else {
            None
        };
        if let Some(client_message_id) = client_message_id.as_ref()
            && let Some(meta_object) = meta.as_mut().and_then(serde_json::Value::as_object_mut)
        {
            meta_object.insert(
                "client_message_id".to_string(),
                serde_json::Value::String(client_message_id.clone()),
            );
        }

        if let Some(client_message_id) = client_message_id.as_ref()
            && let Some(existing) = ChatMessage::find_idempotent_user_message(
                &self.db.pool,
                session.id,
                client_message_id,
            )
            .await?
        {
            let (resolved, missing) = self
                .resolve_message_deliveries(
                    session,
                    existing.sender_type.clone(),
                    existing.sender_session_agent_id,
                    &existing.mentions.0,
                    &existing.meta.0,
                )
                .await?;
            let bundle = chat::ensure_message_delivery_bundle(
                &self.db.pool,
                &existing,
                &Self::delivery_target_inputs(&resolved),
            )
            .await?;
            self.activate_persisted_message_bundle(
                session.id,
                bundle.message.clone(),
                resolved,
                missing,
                bundle.dispatch_deliveries,
                false,
            );
            return Ok(PersistedChatMessageDeliveryBundle {
                message: bundle.message,
                deliveries: bundle
                    .deliveries
                    .into_iter()
                    .map(QueuedMessageService::from_row)
                    .collect(),
                revision: bundle.runtime_revision,
                created: false,
            });
        }

        let data = chat::prepare_chat_message(
            &self.db.pool,
            session.id,
            sender_type,
            sender_id,
            content,
            meta,
        )
        .await?;
        let sender_session_agent_id = data
            .meta
            .get("session_agent_id")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok());
        let (initial_resolved, initial_missing) = self
            .resolve_message_deliveries(
                session,
                data.sender_type.clone(),
                sender_session_agent_id,
                &data.mentions,
                &data.meta,
            )
            .await?;
        let mut bundle = chat::create_message_delivery_bundle_with_id(
            &self.db.pool,
            &data,
            &Self::delivery_target_inputs(&initial_resolved),
            client_message_id.as_deref(),
            message_id,
        )
        .await?;
        let (resolved, missing) = if bundle.created {
            (initial_resolved, initial_missing)
        } else {
            let resolved = self
                .resolve_message_deliveries(
                    session,
                    bundle.message.sender_type.clone(),
                    bundle.message.sender_session_agent_id,
                    &bundle.message.mentions.0,
                    &bundle.message.meta.0,
                )
                .await?;
            bundle = chat::ensure_message_delivery_bundle(
                &self.db.pool,
                &bundle.message,
                &Self::delivery_target_inputs(&resolved.0),
            )
            .await?;
            resolved
        };
        let created = bundle.created;
        self.activate_persisted_message_bundle(
            session.id,
            bundle.message.clone(),
            resolved,
            missing,
            bundle.dispatch_deliveries,
            created,
        );
        Ok(PersistedChatMessageDeliveryBundle {
            message: bundle.message,
            deliveries: bundle
                .deliveries
                .into_iter()
                .map(QueuedMessageService::from_row)
                .collect(),
            revision: bundle.runtime_revision,
            created,
        })
    }

    pub async fn handle_message(&self, session: &ChatSession, message: &ChatMessage) {
        self.emit_message_new(session.id, message.clone());

        // Check chain depth to prevent infinite loops
        let chain_depth = self.extract_chain_depth(&message.meta);
        let max_agent_chain_depth = config::load_config_from_file(&config_path())
            .await
            .max_agent_chain_depth
            .max(1);
        if chain_depth >= max_agent_chain_depth {
            tracing::warn!(
                session_id = %session.id,
                chain_depth = chain_depth,
                max_agent_chain_depth = max_agent_chain_depth,
                "agent chain depth limit reached; not triggering further agents"
            );
            return;
        }

        let session_id = session.id;
        let mentions = message.mentions.0.clone();
        if mentions.is_empty() {
            match self
                .resolve_default_mention_for_unmentioned_user_message(session, message)
                .await
            {
                Ok(Some(default_member)) => {
                    tracing::debug!(
                        session_id = %session_id,
                        message_id = %message.id,
                        session_agent_id = %default_member.session_agent_id,
                        "routing unmentioned user message to lead session agent"
                    );
                    if let Err(err) = self
                        .dispatch_resolved_message_member(
                            default_member,
                            0,
                            db::models::chat_message_target::ChatMessageTargetRouteKind::DefaultLead,
                            message,
                        )
                        .await
                    {
                        tracing::warn!(
                            session_id = %session_id,
                            message_id = %message.id,
                            error = %err,
                            "failed to dispatch default lead session member"
                        );
                    }
                    return;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        session_id = %session_id,
                        message_id = %message.id,
                        error = %err,
                        "failed to resolve default session agent for unmentioned user message"
                    );
                }
            }
        }

        for (ordinal, mention) in mentions.into_iter().enumerate() {
            if message.sender_type == ChatSenderType::Agent
                && mention.eq_ignore_ascii_case(RESERVED_USER_HANDLE)
            {
                tracing::debug!(
                    session_id = %session_id,
                    message_id = %message.id,
                    mention = mention,
                    "skipping reserved user mention in agent message"
                );
                continue;
            }

            match self
                .run_agent_for_mention(session_id, &mention, ordinal as i64, message)
                .await
            {
                Ok(DispatchOutcome::Rejected { reason }) if reason == "member_not_found" => {
                    self.report_missing_member_mention(session_id, message, &mention)
                        .await;
                }
                Ok(DispatchOutcome::Rejected { reason }) => tracing::debug!(
                    mention = mention,
                    session_id = %session_id,
                    reason = reason,
                    "chat message target rejected"
                ),
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        mention = mention,
                        session_id = %session_id,
                        "chat runner failed for mention"
                    );
                }
            }
        }
    }

    async fn resolve_default_mention_for_unmentioned_user_message(
        &self,
        session: &ChatSession,
        message: &ChatMessage,
    ) -> Result<Option<ResolvedSessionMember>, ChatRunnerError> {
        if message.sender_type != ChatSenderType::User || !message.mentions.0.is_empty() {
            return Ok(None);
        }

        let is_workflow_mode = message
            .meta
            .get("chat_input_mode")
            .and_then(|v| v.as_str())
            .map(|v| v == "workflow")
            .unwrap_or(false);
        if !is_workflow_mode {
            return Ok(None);
        }

        let session_agents =
            ChatSessionAgent::find_all_for_session(&self.db.pool, session.id).await?;
        if session_agents.is_empty() {
            return Ok(None);
        }

        let agents = ChatAgent::find_all(&self.db.pool).await?;
        tracing::debug!(
            session_id = %session.id,
            message_id = %message.id,
            "attempting to resolve lead agent for workflow mode message"
        );
        match resolve_lead_agent(session, &session_agents, &agents) {
            Ok((_lead_agent, lead_session_agent)) => {
                Ok(Some(Self::resolved_session_member(lead_session_agent)))
            }
            Err(_) => Ok(None),
        }
    }

    fn extract_chain_depth(&self, meta: &sqlx::types::Json<serde_json::Value>) -> u32 {
        meta.get("chain_depth")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(0)
    }

    /// Extract the frontend-supplied `client_message_id` from a source message's
    /// metadata. Used to correlate an agent run and its final message back to the
    /// pending placeholder the frontend optimistically rendered.
    pub(super) fn extract_client_message_id(
        meta: &sqlx::types::Json<serde_json::Value>,
    ) -> Option<String> {
        meta.get("client_message_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    /// Extract the protocol retry attempt count from a source message's metadata.
    /// Returns 0 if the message is not a retry (normal first attempt).
    fn extract_protocol_retry_attempt(meta: &sqlx::types::Json<serde_json::Value>) -> u32 {
        meta.get("protocol_retry")
            .and_then(|v| v.get("attempt"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .unwrap_or(0)
    }

    fn emit(&self, session_id: Uuid, event: ChatStreamEvent) {
        let sender = self.sender_for(session_id);
        let _ = sender.send(event);
    }

    pub(crate) fn sender_for(&self, session_id: Uuid) -> broadcast::Sender<ChatStreamEvent> {
        if let Some(entry) = self.streams.get(&session_id) {
            return entry.clone();
        }

        let (sender, _) = broadcast::channel(1024);
        self.streams.insert(session_id, sender.clone());
        sender
    }

    /// Claim and dispatch the next queued message for a member after it becomes idle.
    ///
    /// The queue is the persistent `chat_message_queue` table, so this resumes correctly after a
    /// restart. `QueuedMessageService::claim_next` atomically picks the oldest `queued` entry and
    /// is a no-op when the member is busy or blocked by a failed entry (stop-on-failure).
    pub async fn dispatch_next_queued_message(&self, session_id: Uuid, session_agent_id: Uuid) {
        let entry = match QueuedMessageService::new()
            .claim_next(&self.db.pool, session_agent_id)
            .await
        {
            Ok(Some(entry)) => entry,
            Ok(None) => return,
            Err(err) => {
                tracing::warn!(
                    session_agent_id = %session_agent_id,
                    error = %err,
                    "failed to claim next queued message"
                );
                return;
            }
        };
        self.emit_member_queue_update(session_id, session_agent_id)
            .await;

        self.dispatch_queued_entry(session_id, session_agent_id, entry)
            .await;
    }

    async fn dispatch_queued_entry(
        &self,
        session_id: Uuid,
        session_agent_id: Uuid,
        entry: QueuedMessage,
    ) {
        // Resolve the persisted references back into the data the runner needs.
        let message = match ChatMessage::find_by_id(&self.db.pool, entry.chat_message_id).await {
            Ok(Some(message)) => message,
            other => {
                if let Err(err) = other {
                    tracing::warn!(error = %err, "failed to load queued chat message");
                }
                self.fail_or_skip_queue_entry(
                    &entry,
                    Some("queued chat message no longer exists".to_string()),
                )
                .await;
                return;
            }
        };
        tracing::info!(
            session_agent_id = %session_agent_id,
            message_id = %message.id,
            queue_id = %entry.id,
            agent_id = %entry.agent_id,
            "processing queued message for agent"
        );

        // The queue row already carries the authoritative runtime target. Do not resolve the
        // backing ChatAgent name again: project members may have a different effective name, and
        // name re-resolution can select a different session member.
        let member = match self
            .resolve_session_agent_by_id(session_id, session_agent_id)
            .await
        {
            Ok(Some(member)) => member,
            Ok(None) => {
                self.fail_or_skip_queue_entry(
                    &entry,
                    Some("queued session member no longer exists".to_string()),
                )
                .await;
                return;
            }
            Err(err) => {
                self.fail_or_skip_queue_entry(&entry, Some(err.to_string()))
                    .await;
                return;
            }
        };
        let dispatch_result = self
            .run_agent_internal(
                AgentRunTarget {
                    member,
                    claimed_queue_id: Some(entry.id),
                },
                &message,
                true,
            )
            .await;
        match dispatch_result {
            Ok(DispatchOutcome::Started { run_id }) => {
                tracing::debug!(
                    session_agent_id = %session_agent_id,
                    queue_id = %entry.id,
                    run_id = %run_id,
                    "queued message started"
                );
            }
            Ok(DispatchOutcome::Queued { queue_id }) => {
                tracing::warn!(
                    session_agent_id = %session_agent_id,
                    queue_id = %entry.id,
                    unexpected_queue_id = %queue_id,
                    agent_id = %entry.agent_id,
                    "claimed queued message was queued again instead of started"
                );
                self.fail_or_skip_queue_entry(
                    &entry,
                    Some(format!(
                        "claimed queued message was requeued as {queue_id} instead of started"
                    )),
                )
                .await;
            }
            Ok(DispatchOutcome::Rejected { reason }) => {
                self.fail_or_skip_queue_entry(&entry, Some(reason)).await;
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    session_agent_id = %session_agent_id,
                    queue_id = %entry.id,
                    agent_id = %entry.agent_id,
                    "failed to dispatch queued message"
                );
                // The run never started (or failed before binding), so finalize the claimed entry.
                // `fail_or_skip_queue_entry` blocks when queued messages remain or auto-skips
                // when nothing is waiting, keeping the queue clean.
                self.fail_or_skip_queue_entry(
                    &entry,
                    Some(format!("failed to dispatch queued message: {err}")),
                )
                .await;
            }
        }
    }

    /// Finalize a failed queue entry, choosing between `failed` (block) and `skipped`
    /// (auto-skip) based on whether queued messages are waiting behind it.
    ///
    /// "Continue execution" is only meaningful when queued messages are waiting. If nothing is
    /// queued, the failed entry is auto-skipped so the queue stays clean and the next message
    /// runs directly instead of being blocked by a stale failure.
    async fn fail_or_skip_queue_entry(
        &self,
        entry: &QueuedMessage,
        failure_reason: Option<String>,
    ) {
        let service = QueuedMessageService::new();
        let has_queued = match service
            .has_queued(&self.db.pool, entry.session_agent_id)
            .await
        {
            Ok(has_queued) => has_queued,
            Err(err) => {
                tracing::warn!(
                    session_agent_id = %entry.session_agent_id,
                    entry_id = %entry.id,
                    error = %err,
                    "failed to check for queued messages; defaulting to fail-and-block"
                );
                true
            }
        };

        let next_status = if has_queued {
            QueuedMessageStatus::Failed
        } else {
            QueuedMessageStatus::Skipped
        };
        match service
            .fail_or_skip_inflight_cas(
                &self.db.pool,
                entry.id,
                entry.revision,
                entry.status,
                next_status,
                failure_reason.clone(),
            )
            .await
        {
            Ok(Some(_)) => {
                self.emit_member_queue_update(entry.session_id, entry.session_agent_id)
                    .await;
            }
            Ok(None) => tracing::debug!(
                entry_id = %entry.id,
                expected_revision = entry.revision,
                expected_status = ?entry.status,
                failure_reason = ?failure_reason,
                "stale queue failure did not finalize a newer delivery attempt"
            ),
            Err(err) => tracing::warn!(
                entry_id = %entry.id,
                error = %err,
                "failed to finalize queue entry"
            ),
        }
    }

    async fn resolve_session_agent_for_mention(
        &self,
        session_id: Uuid,
        mention: &str,
    ) -> Result<Option<ResolvedSessionMember>, ChatRunnerError> {
        let session_agents =
            ChatSessionAgent::find_all_for_session(&self.db.pool, session_id).await?;
        if !session_agents.is_empty() {
            let Some(session_agent) = ChatSessionAgent::find_by_session_and_member_name(
                &self.db.pool,
                session_id,
                mention,
            )
            .await?
            else {
                return Ok(None);
            };

            if session_agent.workspace_path.is_none() {
                // respects "优先保留显式 agent workspace" because a user-set
                // Isolated sessions resolve through the worktree reducer during
                // the run. That path also syncs all session members to the
                // isolated worktree once it exists.
                let session = ChatSession::find_by_id(&self.db.pool, session_id).await?;
                if let Some(ref session) = session
                    && session.worktree_mode == ChatSessionWorktreeMode::Isolated
                {
                    return Ok(Some(Self::resolved_session_member(&session_agent)));
                }

                let workspace_path = self
                    .resolve_workspace_path_for_agent(session_id, session_agent.agent_id, None)
                    .await?;
                let updated = ChatSessionAgent::update_workspace_path(
                    &self.db.pool,
                    session_agent.id,
                    Some(workspace_path),
                )
                .await?;
                return Ok(Some(Self::resolved_session_member(&updated)));
            }

            return Ok(Some(Self::resolved_session_member(&session_agent)));
        }

        self.materialize_project_member_for_mention(session_id, mention)
            .await
    }

    async fn resolve_session_agent_by_id(
        &self,
        session_id: Uuid,
        session_agent_id: Uuid,
    ) -> Result<Option<ResolvedSessionMember>, ChatRunnerError> {
        let Some(session_agent) =
            ChatSessionAgent::find_by_id(&self.db.pool, session_agent_id).await?
        else {
            return Ok(None);
        };
        if session_agent.session_id != session_id {
            return Ok(None);
        }

        if ChatAgent::find_by_id(&self.db.pool, session_agent.agent_id)
            .await?
            .is_none()
        {
            tracing::warn!(
                session_id = %session_id,
                session_agent_id = %session_agent_id,
                agent_id = %session_agent.agent_id,
                "chat session agent missing backing agent"
            );
            return Ok(None);
        }
        Ok(Some(Self::resolved_session_member(&session_agent)))
    }

    fn resolved_session_member(session_agent: &ChatSessionAgent) -> ResolvedSessionMember {
        ResolvedSessionMember {
            session_id: session_agent.session_id,
            session_agent_id: session_agent.id,
            agent_id: session_agent.agent_id,
            project_member_id: session_agent.project_member_id,
            member_name: session_agent.member_name.clone(),
        }
    }

    async fn materialize_project_member_for_mention(
        &self,
        session_id: Uuid,
        mention: &str,
    ) -> Result<Option<ResolvedSessionMember>, ChatRunnerError> {
        let Some(session) = ChatSession::find_by_id(&self.db.pool, session_id).await? else {
            return Ok(None);
        };
        let Some(project_id) = session.project_id else {
            return Ok(None);
        };

        let project_members = ProjectMember::find_by_project(&self.db.pool, project_id).await?;
        let agents = ChatAgent::find_all(&self.db.pool).await?;
        let agent_map: HashMap<Uuid, ChatAgent> =
            agents.into_iter().map(|agent| (agent.id, agent)).collect();

        let mut exact_member_match = None;

        for member in project_members {
            if member.member_type != ProjectMemberType::Agent {
                continue;
            }
            let Some(agent_id) = member.agent_id else {
                continue;
            };
            let Some(agent) = agent_map.get(&agent_id) else {
                continue;
            };

            let effective_name = chat::effective_agent_name(agent, member.member_name.as_deref());
            let candidate = (member, agent.clone(), effective_name.clone());

            if effective_name == mention {
                if exact_member_match.is_some() {
                    tracing::warn!(
                        session_id = %session_id,
                        mention = mention,
                        "multiple project agents have the same exact member name; skipping auto-configuration"
                    );
                    return Ok(None);
                }
                exact_member_match = Some(candidate);
            }
        }

        let Some((member, _agent, _effective_name)) = exact_member_match else {
            return Ok(None);
        };

        let Some(agent_id) = member.agent_id else {
            return Ok(None);
        };

        if let Some(existing) = ChatSessionAgent::find_by_session_and_project_member(
            &self.db.pool,
            session_id,
            member.id,
        )
        .await?
        {
            return Ok(Some(Self::resolved_session_member(&existing)));
        }

        let workspace_path = self
            .resolve_workspace_path_for_agent(
                session_id,
                agent_id,
                member
                    .default_workspace_path
                    .clone()
                    .or_else(|| session.default_workspace_path.clone()),
            )
            .await?;
        let create = CreateChatSessionAgent {
            session_id,
            agent_id,
            member_name: member.member_name.clone(),
            workspace_path: Some(workspace_path),
            allowed_skill_ids: member.allowed_skill_ids.0.clone(),
            project_member_id: Some(member.id),
            execution_config: member.execution_config.0.clone(),
        };
        let session_agent =
            match ChatSessionAgent::create(&self.db.pool, &create, Uuid::new_v4()).await {
                Ok(created) => created,
                Err(err) => {
                    if let Some(existing) = ChatSessionAgent::find_by_session_and_project_member(
                        &self.db.pool,
                        session_id,
                        member.id,
                    )
                    .await?
                    {
                        existing
                    } else {
                        return Err(err.into());
                    }
                }
            };

        tracing::info!(
            session_id = %session_id,
            project_member_id = %member.id,
            agent_id = %agent_id,
            mention = mention,
            "auto-configured project member in chat session for first mention"
        );

        Ok(Some(Self::resolved_session_member(&session_agent)))
    }

    async fn run_agent_for_mention(
        &self,
        session_id: Uuid,
        mention: &str,
        ordinal: i64,
        source_message: &ChatMessage,
    ) -> Result<DispatchOutcome, ChatRunnerError> {
        let Some(member) = self
            .resolve_session_agent_for_mention(session_id, mention)
            .await?
        else {
            return Ok(DispatchOutcome::Rejected {
                reason: "member_not_found".to_string(),
            });
        };
        let route_kind = if source_message.sender_type == ChatSenderType::Agent {
            db::models::chat_message_target::ChatMessageTargetRouteKind::AgentProtocol
        } else {
            db::models::chat_message_target::ChatMessageTargetRouteKind::ExplicitMention
        };
        self.dispatch_resolved_message_member(member, ordinal, route_kind, source_message)
            .await
    }

    async fn dispatch_resolved_message_member(
        &self,
        member: ResolvedSessionMember,
        ordinal: i64,
        route_kind: db::models::chat_message_target::ChatMessageTargetRouteKind,
        source_message: &ChatMessage,
    ) -> Result<DispatchOutcome, ChatRunnerError> {
        let session_id = member.session_id;
        let is_self_mention = source_message.sender_type == ChatSenderType::Agent
            && source_message.sender_session_agent_id == Some(member.session_agent_id);
        db::models::chat_message_target::ChatMessageTarget::create(
            &self.db.pool,
            &db::models::chat_message_target::CreateChatMessageTarget {
                message_id: source_message.id,
                ordinal,
                session_id,
                session_agent_id: Some(member.session_agent_id),
                project_member_id: member.project_member_id,
                agent_id: member.agent_id,
                member_name_snapshot: member.member_name.clone(),
                route_kind,
                resolution_status: if is_self_mention {
                    db::models::chat_message_target::ChatMessageTargetResolutionStatus::Rejected
                } else {
                    db::models::chat_message_target::ChatMessageTargetResolutionStatus::Resolved
                },
            },
        )
        .await?;
        if is_self_mention {
            return Ok(DispatchOutcome::Rejected {
                reason: "self_mention".to_string(),
            });
        }
        if source_message.sender_type == ChatSenderType::Agent {
            return self
                .enqueue_agent_protocol_message(member, source_message)
                .await;
        }
        self.run_agent_internal(
            AgentRunTarget {
                member,
                claimed_queue_id: None,
            },
            source_message,
            true,
        )
        .await
    }

    /// Persist inter-agent delivery before starting the target. The source Agent only waits for
    /// this durable handoff; target startup runs in its own task and cannot hold the source run's
    /// terminal state open.
    async fn enqueue_agent_protocol_message(
        &self,
        member: ResolvedSessionMember,
        source_message: &ChatMessage,
    ) -> Result<DispatchOutcome, ChatRunnerError> {
        let Some(session_agent) =
            ChatSessionAgent::find_by_id(&self.db.pool, member.session_agent_id).await?
        else {
            return Ok(DispatchOutcome::Rejected {
                reason: "member_not_found".to_string(),
            });
        };
        let queued = QueuedMessageService::new()
            .create_queued(
                &self.db.pool,
                &CreateQueuedMessage {
                    session_id: member.session_id,
                    session_agent_id: member.session_agent_id,
                    agent_id: member.agent_id,
                    chat_message_id: source_message.id,
                },
            )
            .await?;
        self.emit_member_queue_update(member.session_id, member.session_agent_id)
            .await;
        self.emit(
            member.session_id,
            ChatStreamEvent::MentionAcknowledged {
                session_id: member.session_id,
                message_id: source_message.id,
                session_agent_id: Some(member.session_agent_id),
                project_member_id: member.project_member_id,
                mentioned_agent: member.member_name.clone(),
                agent_id: member.agent_id,
                status: MentionStatus::Received,
            },
        );
        self.update_mention_status(
            source_message.id,
            &member.member_name,
            "received",
            Some(&session_agent),
        )
        .await;

        if !member_state_accepts_queued_messages(&session_agent.state) {
            let runner = self.clone();
            tokio::spawn(async move {
                runner
                    .dispatch_next_queued_message(member.session_id, member.session_agent_id)
                    .await;
            });
        }

        Ok(DispatchOutcome::Queued {
            queue_id: queued.id,
        })
    }

    async fn sync_session_agent_execution_config_before_run(
        &self,
        session_id: Uuid,
        session_agent: ChatSessionAgent,
        agent_id: Uuid,
    ) -> Result<ChatSessionAgent, ChatRunnerError> {
        let Some(session) = ChatSession::find_by_id(&self.db.pool, session_id).await? else {
            return Ok(session_agent);
        };
        let refresh = refresh_session_agent_execution_config_before_run(
            &self.db.pool,
            &session,
            session_agent,
            agent_id,
            None,
        )
        .await?;
        if refresh.changed {
            tracing::info!(
                session_id = %session_id,
                session_agent_id = %refresh.session_agent.id,
                agent_id = %agent_id,
                project_member_id = ?refresh.session_agent.project_member_id,
                "Synced project member execution config immediately before agent run"
            );
        }
        Ok(refresh.session_agent)
    }

    async fn run_agent_internal(
        &self,
        target: AgentRunTarget,
        source_message: &ChatMessage,
        track_source_message: bool,
    ) -> Result<DispatchOutcome, ChatRunnerError> {
        let member = target.member;
        let session_id = member.session_id;
        let claimed_queue_id = target.claimed_queue_id;
        let Some(session_agent) =
            ChatSessionAgent::find_by_id(&self.db.pool, member.session_agent_id).await?
        else {
            return Err(ChatRunnerError::AgentNotFound(member.member_name));
        };
        if session_agent.session_id != session_id
            || session_agent.agent_id != member.agent_id
            || session_agent.project_member_id != member.project_member_id
        {
            return Err(ChatRunnerError::AgentNotFound(member.member_name));
        }
        let Some(mut agent) = ChatAgent::find_by_id(&self.db.pool, member.agent_id).await? else {
            return Err(ChatRunnerError::AgentNotFound(member.member_name));
        };
        agent.name = member.member_name;

        let queue_service = QueuedMessageService::new();
        let starting_delivery = if let Some(queue_id) = claimed_queue_id {
            let Some(entry) = queue_service.find_by_id(&self.db.pool, queue_id).await? else {
                return Err(ChatRunnerError::Io(std::io::Error::other(format!(
                    "claimed delivery {queue_id} no longer exists"
                ))));
            };
            if !matches!(
                entry.status,
                QueuedMessageStatus::Starting | QueuedMessageStatus::Processing
            ) || entry.session_id != session_id
                || entry.session_agent_id != session_agent.id
                || entry.agent_id != agent.id
                || entry.chat_message_id != source_message.id
            {
                return Err(ChatRunnerError::Io(std::io::Error::other(format!(
                    "claimed delivery {queue_id} target or state mismatch"
                ))));
            }
            entry
        } else {
            // Direct dispatch uses the same durable ledger as queued dispatch. Creating the row
            // is idempotent on (chat_message_id, session_agent_id), so an HTTP retry cannot
            // manufacture a second delivery or a second run.
            let delivery = queue_service
                .create_queued(
                    &self.db.pool,
                    &CreateQueuedMessage {
                        session_id,
                        session_agent_id: session_agent.id,
                        agent_id: agent.id,
                        chat_message_id: source_message.id,
                    },
                )
                .await?;
            self.emit_member_queue_update(session_id, session_agent.id)
                .await;

            match delivery.status {
                QueuedMessageStatus::Queued => {
                    let Some(claimed) = queue_service
                        .claim_next(&self.db.pool, session_agent.id)
                        .await?
                    else {
                        if track_source_message {
                            self.set_mention_status(
                                session_id,
                                source_message.id,
                                &agent.name,
                                Some(agent.id),
                                Some(&session_agent),
                                MentionStatus::Received,
                            )
                            .await;
                        }
                        return Ok(DispatchOutcome::Queued {
                            queue_id: delivery.id,
                        });
                    };
                    self.emit_member_queue_update(session_id, session_agent.id)
                        .await;
                    if claimed.id != delivery.id {
                        // An older delivery won the FIFO claim. Start that exact row and leave
                        // this message queued behind it.
                        if track_source_message {
                            self.set_mention_status(
                                session_id,
                                source_message.id,
                                &agent.name,
                                Some(agent.id),
                                Some(&session_agent),
                                MentionStatus::Received,
                            )
                            .await;
                        }
                        let claimed_session_agent_id = session_agent.id;
                        Box::pin(self.dispatch_queued_entry(
                            session_id,
                            claimed_session_agent_id,
                            claimed,
                        ))
                        .await;
                        return Ok(DispatchOutcome::Queued {
                            queue_id: delivery.id,
                        });
                    }
                    claimed
                }
                QueuedMessageStatus::Running
                | QueuedMessageStatus::WaitingApproval
                | QueuedMessageStatus::Stopping => {
                    return match delivery.run_id {
                        Some(run_id) => Ok(DispatchOutcome::Started { run_id }),
                        None => Ok(DispatchOutcome::Queued {
                            queue_id: delivery.id,
                        }),
                    };
                }
                QueuedMessageStatus::Starting | QueuedMessageStatus::Processing => {
                    return Ok(DispatchOutcome::Queued {
                        queue_id: delivery.id,
                    });
                }
                QueuedMessageStatus::Failed
                | QueuedMessageStatus::Cancelled
                | QueuedMessageStatus::Skipped
                | QueuedMessageStatus::Completed => {
                    return Ok(DispatchOutcome::Rejected {
                        reason: "delivery_already_terminal".to_string(),
                    });
                }
            }
        };

        let session_agent_id = session_agent.id;
        let agent_id = agent.id;
        let run_id = Uuid::new_v4();
        let startup_timing = Arc::new(startup_timing::RunStartupTiming::new(
            startup_timing::RunStartupIdentity {
                session_id,
                session_agent_id,
                agent_id,
                run_id,
                source_message_id: source_message.id,
                runner_type: agent.runner_type.clone(),
            },
        ));
        startup_timing.mark(startup_timing::StartupMilestoneName::RunScheduled, None);

        let mut session_agent = session_agent;
        let mut run_started_at = session_agent.updated_at;
        // Correlation ids that let the frontend stitch "user message -> run ->
        // final agent message" together precisely instead of guessing by
        // `session_agent_id`.
        let client_message_id = Self::extract_client_message_id(&source_message.meta);
        // Register stop control while the delivery is `starting`. A stop request can cancel the
        // durable delivery before run binding, and the later bind CAS will then roll back.
        let stop = self.register_run_control(session_agent_id, run_id);
        #[cfg(feature = "qa-mode")]
        self.qa_claim_gate.checkpoint().await;

        let chain_depth = self.extract_chain_depth(&source_message.meta);
        let protocol_retry_attempt = Self::extract_protocol_retry_attempt(&source_message.meta);
        let protocol_retry_meta = source_message.meta.get("protocol_retry").cloned();

        let result = async {
            session_agent = self
                .sync_session_agent_execution_config_before_run(
                    session_id,
                    session_agent.clone(),
                    agent.id,
                )
                .await?;
            let run_model = resolve_effective_member_execution_config(&agent, &session_agent)
                .map_err(|err| ChatRunnerError::Io(std::io::Error::other(err.to_string())))?
                .model_name;
            let workspace_path = self
                .resolve_workspace_path_for_agent(
                    session_id,
                    agent_id,
                    session_agent.workspace_path.clone(),
                )
                .await?;
            session_agent.workspace_path = Some(workspace_path.clone());
            startup_timing.mark(
                startup_timing::StartupMilestoneName::WorkspaceResolved,
                Some(workspace_path.clone()),
            );
            fs::create_dir_all(&workspace_path).await?;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::WorkspaceDirectoryReady,
                None,
            );
            if let Err(err) =
                Self::ensure_openteams_ignored_for_git_workspace(Path::new(&workspace_path)).await
            {
                tracing::warn!(
                    workspace_path = %workspace_path,
                    error = %err,
                    "Failed to ensure .openteams is gitignored for workspace"
                );
            }
            startup_timing.mark(
                startup_timing::StartupMilestoneName::GitignorePrepared,
                None,
            );
            let workspace_change_baseline =
                capture_workspace_change_baseline(PathBuf::from(&workspace_path).as_path()).await;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::WorkspaceBaselineCaptured,
                Some(format!(
                    "has_git_tree={},untracked_count={}",
                    workspace_change_baseline.git_tree.is_some(),
                    workspace_change_baseline.untracked_files.len()
                )),
            );
            tracing::debug!(
                session_id = %session_id,
                run_id = %run_id,
                session_agent_id = %session_agent_id,
                agent_id = %agent_id,
                workspace_path = %workspace_path,
                baseline_has_git_tree = workspace_change_baseline.git_tree.is_some(),
                baseline_untracked_count = workspace_change_baseline.untracked_files.len(),
                "[chat_runner] Captured workspace change baseline for agent run"
            );
            let run_records_dir = Self::workspace_run_records_dir(
                PathBuf::from(&workspace_path).as_path(),
                session_id,
            );
            fs::create_dir_all(&run_records_dir).await?;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::RunRecordsDirectoryReady,
                Some(run_records_dir.to_string_lossy().to_string()),
            );
            tracing::info!(
                session_id = %session_id,
                workspace_path = %workspace_path,
                runs_dir = %run_records_dir.display(),
                "Using workspace runs directory"
            );

            let run_index = ChatRun::next_run_index(&self.db.pool, session_agent_id).await?;
            let run_dir =
                run_records_dir.join(Self::run_records_prefix(session_agent_id, run_index));
            fs::create_dir_all(&run_dir).await?;
            startup_timing
                .set_artifact_path(run_dir.join(startup_timing::STARTUP_TIMING_FILE_NAME));
            startup_timing
                .mark_and_persist(
                    startup_timing::StartupMilestoneName::RunDirectoryReady,
                    Some(run_dir.to_string_lossy().to_string()),
                )
                .await;

            tracing::debug!(
                session_id = %session_id,
                run_id = %run_id,
                run_index = run_index,
                run_dir = %run_dir.display(),
                "[chat_runner] Created run directory for agent execution"
            );

            let input_path = run_dir.join("input.md");
            let output_path = run_dir.join("output.md");
            let tail_log_path = run_dir.join("raw.tail.log");
            let meta_path = run_dir.join("meta.json");
            let live_spool_path =
                Self::workspace_live_spool_path(PathBuf::from(&workspace_path).as_path(), run_id);

            let context_snapshot = self
                .build_context_snapshot(session_id, &workspace_path)
                .await?;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::ContextSnapshotBuilt,
                Some(format!(
                    "context_compacted={},path={}",
                    context_snapshot.context_compacted,
                    context_snapshot.workspace_path.to_string_lossy()
                )),
            );
            let context_dir = context_snapshot
                .workspace_path
                .parent()
                .map(|path| path.to_path_buf())
                .unwrap_or_else(|| PathBuf::from(&workspace_path));
            let reference_context = self
                .build_reference_context(session_id, source_message, &context_dir)
                .await?;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::ReferenceContextBuilt,
                Some(format!("has_reference={}", reference_context.is_some())),
            );
            let message_attachments = self
                .build_message_attachment_context(source_message, &context_dir)
                .await?;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::AttachmentContextBuilt,
                Some(format!(
                    "attachment_count={}",
                    message_attachments
                        .as_ref()
                        .map(|context| context.attachments.len())
                        .unwrap_or(0)
                )),
            );
            let session_agents = self.build_session_agent_summaries(session_id).await?;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::SessionAgentSummariesBuilt,
                Some(format!("member_count={}", session_agents.len())),
            );
            let session = ChatSession::find_by_id(&self.db.pool, session_id).await?;
            let team_protocol =
                if let Some(project_id) = session.as_ref().and_then(|session| session.project_id) {
                    db::models::project_team_protocol::ProjectTeamProtocol::find_by_project(
                        &self.db.pool,
                        project_id,
                    )
                    .await?
                    .and_then(|protocol| protocol.content_if_enabled().map(str::to_string))
                } else {
                    None
                };

            // Resolve builtin + user-configured skills for this agent.
            let prompt_context = if is_workflow_chat_input_mode(&source_message.meta.0) {
                crate::services::agent_skill_policy::AgentPromptContext::WorkflowChat
            } else {
                crate::services::agent_skill_policy::AgentPromptContext::FreeChat
            };
            let workflow_generation_blocked = if matches!(
                prompt_context,
                crate::services::agent_skill_policy::AgentPromptContext::WorkflowChat
            ) {
                !db::models::workflow_execution::WorkflowExecution::find_generation_blocking_by_session(
                    &self.db.pool,
                    session_id,
                )
                .await?
                .is_empty()
            } else {
                false
            };
            let agent_skills = self
                .prepare_and_resolve_agent_skills(&mut session_agent, &agent, prompt_context)
                .await?;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::AgentSkillsResolved,
                Some(format!("skill_count={}", agent_skills.len())),
            );

            // Load UI language setting for agent response language
            let ui_config = config::load_config_from_file(&config_path()).await;
            let ui_language = ui_config.language;
            let prompt_language = Self::resolve_prompt_language(source_message, &ui_language);

            let prompt = self.build_prompt(
                &agent,
                source_message,
                &context_snapshot.workspace_path,
                Path::new(&workspace_path),
                &session_agents,
                message_attachments.as_ref(),
                reference_context.as_ref(),
                &agent_skills,
                prompt_language,
                team_protocol.as_deref(),
                workflow_generation_blocked,
            );
            let executor_prompt = ExecutorPrompt {
                text: prompt.clone(),
                images: match message_attachments.as_ref() {
                    Some(attachments) => attachments.executor_images().await,
                    None => Vec::new(),
                },
            };
            startup_timing.mark(
                startup_timing::StartupMilestoneName::PromptBuilt,
                Some(format!("prompt_bytes={}", prompt.len())),
            );
            fs::write(&input_path, &prompt).await?;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::PromptInputWritten,
                Some(input_path.to_string_lossy().to_string()),
            );

            let run_data = CreateChatRun {
                session_id,
                session_agent_id,
                workspace_path: Some(workspace_path.clone()),
                run_index,
                run_dir: run_dir.to_string_lossy().to_string(),
                input_path: Some(input_path.to_string_lossy().to_string()),
                output_path: Some(output_path.to_string_lossy().to_string()),
                raw_log_path: Some(live_spool_path.to_string_lossy().to_string()),
                meta_path: Some(meta_path.to_string_lossy().to_string()),
            };
            let Some(binding) = queue_service
                .bind_delivery_to_new_run(
                    &self.db.pool,
                    starting_delivery.id,
                    starting_delivery.revision,
                    &run_data,
                    run_id,
                )
                .await?
            else {
                return if stop.is_cancelled() {
                    Err(ChatRunnerError::StartupStopped)
                } else {
                    Err(ChatRunnerError::Io(std::io::Error::other(format!(
                        "delivery {} changed before run binding",
                        starting_delivery.id
                    ))))
                };
            };
            session_agent = binding.member;
            run_started_at = session_agent.updated_at;
            startup_timing.mark(startup_timing::StartupMilestoneName::ChatRunCreated, None);
            startup_timing.mark(
                startup_timing::StartupMilestoneName::AgentStateRunningPersisted,
                Some(format!("runtime_revision={}", binding.runtime_revision)),
            );
            startup_timing.mark(startup_timing::StartupMilestoneName::QueueBoundToRun, None);

            // Every runtime projection below is emitted only after the atomic bind transaction
            // committed the run, delivery, member state, session revision, and outbox row.
            self.emit_member_queue_update(session_id, session_agent_id)
                .await;
            self.emit(
                session_id,
                ChatStreamEvent::AgentState {
                    session_agent_id,
                    agent_id,
                    state: ChatSessionAgentState::Running,
                    run_id: Some(run_id),
                    started_at: Some(run_started_at),
                },
            );
            startup_timing.mark(
                startup_timing::StartupMilestoneName::AgentStateRunningEmitted,
                None,
            );
            // The activity endpoint resolves `run_id` through `chat_runs`, so do not
            // expose the run to clients until that row is queryable.
            self.emit(
                session_id,
                ChatStreamEvent::AgentRunStarted {
                    session_id,
                    session_agent_id,
                    agent_id,
                    agent_name: agent.name.clone(),
                    model: run_model.clone(),
                    delivery_id: binding.delivery.id,
                    run_id,
                    source_message_id: source_message.id,
                    client_message_id: client_message_id.clone(),
                    started_at: Some(run_started_at),
                },
            );
            startup_timing.mark(
                startup_timing::StartupMilestoneName::AgentRunStartedEmitted,
                client_message_id
                    .as_ref()
                    .map(|id| format!("client_message_id={id}")),
            );

            if let Some(warning) = context_snapshot.compression_warning.clone() {
                self.emit(
                    session_id,
                    ChatStreamEvent::CompressionWarning {
                        session_id,
                        warning: warning.into(),
                    },
                );
            }
            if track_source_message {
                self.emit(
                    session_id,
                    ChatStreamEvent::MentionAcknowledged {
                        session_id,
                        message_id: source_message.id,
                        session_agent_id: Some(session_agent_id),
                        project_member_id: session_agent.project_member_id,
                        mentioned_agent: agent.name.clone(),
                        agent_id,
                        status: MentionStatus::Running,
                    },
                );
                self.update_mention_status(
                    source_message.id,
                    &agent.name,
                    "running",
                    Some(&session_agent),
                )
                .await;
            }

            #[cfg(any(test, feature = "qa-mode"))]
            if self.stop_after_queue_binding.load(Ordering::Relaxed) {
                return Ok(());
            }

            let repo_context = RepoContext::new(PathBuf::from(&workspace_path), Vec::new());
            let mut env = ExecutionEnv::new(repo_context, false, String::new());
            crate::services::output_validation::inject_output_validation_url(&mut env);
            env.insert("VK_CHAT_SESSION_ID", session_id.to_string());
            env.insert("VK_CHAT_AGENT_ID", agent_id.to_string());
            env.insert("VK_CHAT_SESSION_AGENT_ID", session_agent_id.to_string());
            env.insert("VK_CHAT_RUN_ID", run_id.to_string());
            env.insert(
                "VK_CHAT_CONTEXT_PATH",
                context_snapshot
                    .workspace_path
                    .to_string_lossy()
                    .to_string(),
            );
            #[cfg(test)]
            self.inject_mcp_preparation_diagnostic_for_test(&mut env);
            let (effective_execution, mut executor, prepared_mcp) =
                build_effective_member_executor_for_run(
                    &self.db.pool,
                    &agent,
                    &session_agent,
                    Path::new(&workspace_path),
                    run_id,
                    &mut env,
                )
                    .await
                    .map_err(|err| ChatRunnerError::Io(std::io::Error::other(err.to_string())))?;
            let acp_full_access = executor_acp_full_access_enabled(&executor);
            if acp_full_access {
                tracing::warn!(
                    session_id = %session_id,
                    session_agent_id = %session_agent_id,
                    run_id = %run_id,
                    "ACP Full Access enabled for chat run"
                );
            }
            let approval_bridge = ExecutorApprovalBridge::new(
                self.db.clone(),
                ExecutorApprovalScope {
                    session_id,
                    session_agent_id,
                    run_id,
                    runner: effective_execution.runner_type.to_string(),
                    workflow_execution_id: None,
                    workflow_step_id: None,
                },
            );
            executor.use_approvals(Arc::new(DeliveryAwareApprovalBridge {
                inner: approval_bridge,
                runner: self.clone(),
                session_id,
                session_agent_id,
                run_id,
            }));
            startup_timing.mark(
                startup_timing::StartupMilestoneName::ExecutorConfigured,
                Some(effective_execution.analytics_profile_label()),
            );

            let spawn_kind = if session_agent.state != ChatSessionAgentState::Dead
                && session_agent.agent_session_id.is_some()
            {
                "follow_up"
            } else {
                "initial"
            };
            startup_timing
                .mark_and_persist(
                    startup_timing::StartupMilestoneName::ExecutorSpawnStarted,
                    Some(format!("spawn_kind={spawn_kind}")),
                )
                .await;
            let spawn = async {
                #[cfg(test)]
                {
                    self.executor_spawn_attempts.fetch_add(1, Ordering::Relaxed);
                    if self.block_executor_spawn.load(Ordering::Relaxed) {
                        return Err(ExecutorError::Io(std::io::Error::other(
                            "test blocked executor spawn",
                        )));
                    }
                }
                if session_agent.state != ChatSessionAgentState::Dead {
                    if let Some(agent_session_id) = session_agent.agent_session_id.as_deref() {
                        executor
                            .spawn_follow_up_structured(
                                PathBuf::from(&workspace_path).as_path(),
                                &executor_prompt,
                                agent_session_id,
                                session_agent.agent_message_id.as_deref(),
                                &env,
                            )
                            .await
                    } else {
                        executor
                            .spawn_structured(
                                PathBuf::from(&workspace_path).as_path(),
                                &executor_prompt,
                                &env,
                            )
                            .await
                    }
                } else {
                    executor
                        .spawn_structured(
                            PathBuf::from(&workspace_path).as_path(),
                            &executor_prompt,
                            &env,
                        )
                        .await
                }
            };
            let mut spawned = Self::wait_for_executor_startup(
                spawn,
                &stop,
                EXECUTOR_STARTUP_TIMEOUT,
            )
            .await?;
            spawned.cleanup = ExecutorRunCleanup::combine(
                prepared_mcp.into_cleanup(),
                spawned.cleanup.take(),
            );
            startup_timing
                .mark_and_persist(
                    startup_timing::StartupMilestoneName::ExecutorSpawnReturned,
                    Some(format!("spawn_kind={spawn_kind}")),
                )
                .await;

            let msg_store = Arc::new(MsgStore::new());
            let raw_log_spool = Arc::new(Mutex::new(
                runtime::RunLogSpool::new(
                    live_spool_path,
                    run_id,
                    self.db.pool.clone(),
                    workspace_path.clone(),
                    self.workspace_live_log_bytes.clone(),
                )
                .await?,
            ));
            startup_timing.mark(startup_timing::StartupMilestoneName::RawLogSpoolReady, None);

            self.analytics_projector()
                .record_or_warn(
                    AnalyticsEvent::new(AnalyticsEventPayload::AgentRunStarted {
                        agent_id,
                        run_kind: "chat".to_string(),
                        executor_profile: Some(
                            effective_execution.analytics_profile_label().to_string(),
                        ),
                    })
                        .with_session(session_id)
                        .with_run(run_id),
                )
                .await;

            let log_forwarders = self.spawn_log_forwarders(
                &mut spawned,
                msg_store.clone(),
                raw_log_spool.clone(),
            )?;
            startup_timing.mark(
                startup_timing::StartupMilestoneName::LogForwardersStarted,
                None,
            );
            executor.normalize_logs(msg_store.clone(), PathBuf::from(&workspace_path).as_path());
            startup_timing.mark(
                startup_timing::StartupMilestoneName::LogNormalizationStarted,
                None,
            );

            let completion_status = Arc::new(AtomicU8::new(RunCompletionStatus::Succeeded.as_u8()));
            let terminal_failure_reason = Arc::new(Mutex::new(None));

            startup_timing
                .mark_and_persist(
                    startup_timing::StartupMilestoneName::StreamBridgeScheduled,
                    None,
                )
                .await;
            self.spawn_stream_bridge(
                msg_store.clone(),
                session_id,
                agent_id,
                session_agent_id,
                run_index,
                run_id,
                output_path,
                meta_path,
                PathBuf::from(&workspace_path),
                run_dir,
                tail_log_path,
                raw_log_spool,
                completion_status.clone(),
                terminal_failure_reason.clone(),
                workspace_change_baseline,
                chain_depth,
                context_snapshot.context_compacted,
                context_snapshot.compression_warning.clone(),
                self.clone(),
                source_message.id,
                client_message_id.clone(),
                run_model,
                source_message.created_at,
                source_message.content.clone(),
                agent.name.clone(),
                prompt_language,
                run_started_at,
                protocol_retry_attempt,
                protocol_retry_meta,
                track_source_message,
                startup_timing.clone(),
                effective_execution.runner_type == BaseCodingAgent::Codex,
                acp_full_access,
            );

            self.spawn_exit_watcher(
                runtime::ExitWatcherArgs {
                    child: spawned.child,
                    stop,
                    executor_cancel: spawned.cancel,
                    exit_signal: spawned.exit_signal,
                    cleanup: spawned.cleanup,
                    msg_store,
                    completion_status,
                    terminal_failure_reason,
                    log_forwarders,
                },
                session_agent_id,
                run_id,
            );
            startup_timing
                .mark_and_persist(
                    startup_timing::StartupMilestoneName::ExitWatcherStarted,
                    None,
                )
                .await;

            Ok::<(), ChatRunnerError>(())
        }
        .await;

        if result.is_err() {
            let startup_stopped = matches!(&result, Err(ChatRunnerError::StartupStopped));
            let final_state = if startup_stopped {
                ChatSessionAgentState::Idle
            } else {
                ChatSessionAgentState::Dead
            };
            let should_remove_control = self
                .run_controls
                .get(&session_agent_id)
                .is_some_and(|control| control.run_id == run_id);
            if should_remove_control {
                self.run_controls.remove(&session_agent_id);
            }
            startup_timing
                .mark_and_persist(
                    startup_timing::StartupMilestoneName::StartupFailed,
                    result.as_ref().err().map(|err| err.to_string()),
                )
                .await;
            let delivery_service = QueuedMessageService::new();
            let current_delivery = delivery_service
                .find_by_id(&self.db.pool, starting_delivery.id)
                .await;
            let (terminal_state_applied, run_was_bound) = match current_delivery {
                Ok(Some(delivery)) if delivery.run_id == Some(run_id) => {
                    let finalization = if startup_stopped {
                        delivery_service
                            .finalize_completed_run_cas(
                                &self.db.pool,
                                run_id,
                                session_agent_id,
                                delivery.revision,
                                false,
                            )
                            .await
                    } else {
                        delivery_service
                            .finalize_failed_run_cas(
                                &self.db.pool,
                                run_id,
                                session_agent_id,
                                delivery.revision,
                                result
                                    .as_ref()
                                    .err()
                                    .map(|err| format!("failed to start agent run: {err}")),
                            )
                            .await
                    };
                    match finalization {
                        Ok(finalization) => (finalization.applied, true),
                        Err(err) => {
                            tracing::warn!(
                                session_agent_id = %session_agent_id,
                                run_id = %run_id,
                                delivery_id = %delivery.id,
                                delivery_revision = delivery.revision,
                                error = %err,
                                "failed to CAS-finalize bound startup"
                            );
                            (false, true)
                        }
                    }
                }
                Ok(Some(delivery))
                    if matches!(
                        delivery.status,
                        QueuedMessageStatus::Starting | QueuedMessageStatus::Processing
                    ) =>
                {
                    let next_status = if startup_stopped {
                        QueuedMessageStatus::Cancelled
                    } else if delivery_service
                        .has_queued(&self.db.pool, session_agent_id)
                        .await
                        .unwrap_or(true)
                    {
                        QueuedMessageStatus::Failed
                    } else {
                        QueuedMessageStatus::Skipped
                    };
                    let transition = if startup_stopped {
                        delivery_service
                            .transition_status_cas(
                                &self.db.pool,
                                delivery.id,
                                delivery.revision,
                                delivery.status,
                                next_status,
                            )
                            .await
                    } else {
                        delivery_service
                            .fail_or_skip_inflight_cas(
                                &self.db.pool,
                                delivery.id,
                                delivery.revision,
                                delivery.status,
                                next_status,
                                result
                                    .as_ref()
                                    .err()
                                    .map(|err| format!("failed to start agent run: {err}")),
                            )
                            .await
                    };
                    match transition {
                        Ok(Some(_)) => (true, false),
                        Ok(None) => (false, false),
                        Err(err) => {
                            tracing::warn!(
                                session_agent_id = %session_agent_id,
                                delivery_id = %delivery.id,
                                delivery_revision = delivery.revision,
                                error = %err,
                                "failed to CAS-finalize pre-bind startup"
                            );
                            (false, false)
                        }
                    }
                }
                Ok(Some(delivery)) if delivery.status.is_terminal() => (false, false),
                Ok(Some(delivery)) => {
                    tracing::warn!(
                        session_agent_id = %session_agent_id,
                        delivery_id = %delivery.id,
                        delivery_revision = delivery.revision,
                        status = ?delivery.status,
                        "startup ended with a delivery that cannot be finalized"
                    );
                    (false, delivery.run_id.is_some())
                }
                Ok(None) => (false, false),
                Err(err) => {
                    tracing::warn!(
                        session_agent_id = %session_agent_id,
                        delivery_id = %starting_delivery.id,
                        error = %err,
                        "failed to load delivery for startup finalization"
                    );
                    (false, false)
                }
            };
            if terminal_state_applied {
                self.emit_member_queue_update(session_id, session_agent_id)
                    .await;
            }
            if terminal_state_applied {
                self.emit(
                    session_id,
                    ChatStreamEvent::AgentState {
                        session_agent_id,
                        agent_id,
                        state: if run_was_bound {
                            final_state
                        } else {
                            ChatSessionAgentState::Idle
                        },
                        run_id: run_was_bound.then_some(run_id),
                        started_at: None,
                    },
                );
            }
            if let Err(err) = &result
                && !startup_stopped
            {
                self.analytics_projector()
                    .record_or_warn(
                        AnalyticsEvent::new(AnalyticsEventPayload::AgentError {
                            run_kind: Some("chat".to_string()),
                            phase: Some("setup".to_string()),
                            error_code: "agent_startup_failed".to_string(),
                            agent_id: Some(agent_id),
                            agent_role: None,
                        })
                        .with_session(session_id)
                        .with_run(run_id),
                    )
                    .await;
                let failure_detail = format!("Failed to start agent run: {err}");
                InboxService::new()
                    .notify_chat_agent_failed(
                        &self.db.pool,
                        session_id,
                        run_id,
                        &agent.name,
                        Some(&failure_detail),
                    )
                    .await;
                if track_source_message {
                    self.report_mention_failure(
                        session_id,
                        source_message.id,
                        &agent.name,
                        Some(agent_id),
                        Some(&session_agent),
                        failure_detail,
                    )
                    .await;
                }
            }
            if startup_stopped && terminal_state_applied && track_source_message {
                self.update_mention_status(
                    source_message.id,
                    &agent.name,
                    "completed",
                    Some(&session_agent),
                )
                .await;
                self.emit(
                    session_id,
                    ChatStreamEvent::MentionAcknowledged {
                        session_id,
                        message_id: source_message.id,
                        session_agent_id: Some(session_agent_id),
                        project_member_id: session_agent.project_member_id,
                        mentioned_agent: agent.name.clone(),
                        agent_id,
                        status: MentionStatus::Completed,
                    },
                );
            }
        }

        result.map(|()| DispatchOutcome::Started { run_id })
    }
}

pub(super) fn member_state_accepts_queued_messages(state: &ChatSessionAgentState) -> bool {
    matches!(
        state,
        ChatSessionAgentState::Running
            | ChatSessionAgentState::WaitingApproval
            | ChatSessionAgentState::Stopping
    )
}
