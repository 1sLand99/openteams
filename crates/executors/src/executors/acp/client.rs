use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
};

use agent_client_protocol::schema::v1::{
    ContentBlock, CreateTerminalRequest, CreateTerminalResponse, Error, KillTerminalRequest,
    KillTerminalResponse, PermissionOptionKind, PromptResponse, ReadTextFileRequest,
    ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, SessionUpdate, TerminalExitStatus, TerminalId,
    TerminalOutputRequest, TerminalOutputResponse, ToolCallContent, ToolCallStatus, ToolCallUpdate,
    WaitForTerminalExitRequest, WaitForTerminalExitResponse, WriteTextFileRequest,
    WriteTextFileResponse,
};
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::Mutex,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use workspace_utils::approvals::ApprovalStatus;

use crate::{
    approvals::{
        ExecutorApprovalError, ExecutorApprovalOption, ExecutorApprovalRequest,
        ExecutorApprovalService,
    },
    executors::acp::{
        AcpApprovalPolicy, AcpClientServicePolicy, AcpEvent, ApprovalResponse, events,
        output::AcpOutput, usage::AcpTokenUsageAccumulator,
    },
    logs::{
        TokenUsageInfo,
        api_errors::{DetectedApiError, detect_api_error},
    },
};

/// State shared by stable ACP client callbacks.
#[derive(Clone)]
pub struct AcpClient {
    output: AcpOutput,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    approval_policy: AcpApprovalPolicy,
    cancel: CancellationToken,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    services: AcpClientServicePolicy,
    terminal_env: HashMap<String, String>,
    terminals: Arc<Mutex<HashMap<String, TerminalRecord>>>,
    tool_calls: Arc<Mutex<HashMap<String, ToolCallUpdate>>>,
    token_usage: Arc<Mutex<AcpTokenUsageAccumulator>>,
    terminal_api_error: Arc<Mutex<Option<DetectedApiError>>>,
}

impl AcpClient {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        output: AcpOutput,
        approvals: Option<Arc<dyn ExecutorApprovalService>>,
        approval_policy: AcpApprovalPolicy,
        cancel: CancellationToken,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        services: AcpClientServicePolicy,
        terminal_env: HashMap<String, String>,
    ) -> Self {
        let mut safe_terminal_env = std::env::vars()
            .filter(|(name, _)| !is_sensitive_env_name(name))
            .collect::<HashMap<_, _>>();
        safe_terminal_env.extend(
            terminal_env
                .into_iter()
                .filter(|(name, _)| !is_sensitive_env_name(name)),
        );
        Self {
            output,
            approvals,
            approval_policy,
            cancel,
            cwd,
            additional_directories,
            services,
            terminal_env: safe_terminal_env,
            terminals: Arc::new(Mutex::new(HashMap::new())),
            tool_calls: Arc::new(Mutex::new(HashMap::new())),
            token_usage: Arc::new(Mutex::new(AcpTokenUsageAccumulator::default())),
            terminal_api_error: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn record_user_prompt_event(&self, prompt: &str) {
        self.send_event(AcpEvent::User(prompt.to_string())).await;
    }

    async fn send_event(&self, event: AcpEvent) {
        if self.output.send(event).await.is_err() {
            warn!("ACP output channel closed");
        }
    }

    pub async fn handle_notification(
        &self,
        notification: SessionNotification,
    ) -> Result<(), Error> {
        self.token_usage
            .lock()
            .await
            .observe_session_update(&notification.update);
        let pi_agent_error = detect_pi_agent_error_notification(&notification.update);
        if let Some(error) = pi_agent_error
            .clone()
            .or_else(|| detect_tool_result_api_error(&notification.update))
        {
            let mut terminal_api_error = self.terminal_api_error.lock().await;
            if terminal_api_error.is_none() {
                *terminal_api_error = Some(error);
            }
        }
        if pi_agent_error.is_some() {
            return Ok(());
        }
        self.observe_tool_call(&notification.update).await;
        self.send_event(events::event_from_notification(notification))
            .await;
        Ok(())
    }

    pub async fn set_token_usage_identity(
        &self,
        runtime_agent: Option<String>,
        runtime_model_id: Option<String>,
        runtime_thread_id: String,
    ) {
        self.token_usage.lock().await.set_runtime_identity(
            runtime_agent,
            runtime_model_id,
            runtime_thread_id,
        );
    }

    pub async fn finish_turn_token_usage(
        &self,
        response: &PromptResponse,
    ) -> Option<TokenUsageInfo> {
        let usage = self.token_usage.lock().await.finish_turn(response);
        self.tool_calls.lock().await.clear();
        usage
    }

    pub async fn begin_token_usage_turn(&self) {
        self.token_usage.lock().await.begin_turn();
        *self.terminal_api_error.lock().await = None;
        self.tool_calls.lock().await.clear();
    }

    pub async fn take_terminal_api_error(&self) -> Option<DetectedApiError> {
        self.terminal_api_error.lock().await.take()
    }

    pub async fn request_permission(
        &self,
        mut request: RequestPermissionRequest,
    ) -> Result<RequestPermissionResponse, Error> {
        request.tool_call = self.enrich_tool_call(request.tool_call).await;
        self.send_event(AcpEvent::RequestPermission(request.clone()))
            .await;

        match self.approval_policy {
            AcpApprovalPolicy::AutoAllow => {
                return Ok(select_option(
                    &request,
                    &[
                        PermissionOptionKind::AllowAlways,
                        PermissionOptionKind::AllowOnce,
                    ],
                ));
            }
            AcpApprovalPolicy::AutoReject => {
                return Ok(select_option(
                    &request,
                    &[
                        PermissionOptionKind::RejectAlways,
                        PermissionOptionKind::RejectOnce,
                    ],
                ));
            }
            AcpApprovalPolicy::Ask => {}
        }

        let tool_call_id = request.tool_call.tool_call_id.0.to_string();
        let Some(approval_service) = self.approvals.as_ref() else {
            warn!("ACP approval service unavailable; cancelling permission request");
            return Ok(RequestPermissionResponse::new(
                RequestPermissionOutcome::Cancelled,
            ));
        };

        let selected_option_id = match approval_service
            .request_acp_tool_approval(
                ExecutorApprovalRequest {
                    tool_name: request
                        .tool_call
                        .fields
                        .title
                        .clone()
                        .unwrap_or_else(|| "tool".to_string()),
                    tool_input: serde_json::json!({ "tool_call": request.tool_call }),
                    tool_call_id: tool_call_id.clone(),
                    options: request
                        .options
                        .iter()
                        .map(|option| ExecutorApprovalOption {
                            option_id: option.option_id.0.to_string(),
                            kind: permission_option_kind_wire(option.kind).to_string(),
                            label: option.name.clone(),
                        })
                        .collect(),
                },
                self.cancel.clone(),
            )
            .await
        {
            Ok(option_id) => option_id,
            Err(ExecutorApprovalError::Cancelled) => {
                return Ok(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ));
            }
            Err(error) => {
                tracing::error!("ACP approval failed for tool_call_id={tool_call_id}: {error}");
                return Err(Error::internal_error());
            }
        };

        let Some(selected) = request
            .options
            .iter()
            .find(|option| option.option_id.0.as_ref() == selected_option_id)
        else {
            return Err(Error::invalid_params()
                .data("approval service selected an option not advertised by the ACP Agent"));
        };
        let status = match selected.kind {
            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways => {
                ApprovalStatus::Approved
            }
            PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => {
                ApprovalStatus::Denied { reason: None }
            }
            _ => return Err(Error::invalid_params()),
        };
        let outcome = RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(selected.option_id.clone()),
        ));

        self.send_event(AcpEvent::ApprovalResponse(ApprovalResponse {
            tool_call_id,
            status,
        }))
        .await;

        Ok(outcome)
    }

    async fn observe_tool_call(&self, update: &SessionUpdate) {
        let mut tool_calls = self.tool_calls.lock().await;
        match update {
            SessionUpdate::ToolCall(tool_call) => {
                let tool_call_id = tool_call.tool_call_id.0.to_string();
                if is_terminal_tool_call_status(tool_call.status) {
                    tool_calls.remove(&tool_call_id);
                } else {
                    tool_calls.insert(tool_call_id, tool_call.clone().into());
                }
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let tool_call_id = update.tool_call_id.0.to_string();
                if update
                    .fields
                    .status
                    .is_some_and(is_terminal_tool_call_status)
                {
                    tool_calls.remove(&tool_call_id);
                    return;
                }

                match tool_calls.get_mut(&tool_call_id) {
                    Some(cached) => merge_tool_call_update(cached, update),
                    None => {
                        tool_calls.insert(tool_call_id, update.clone());
                    }
                }
            }
            _ => {}
        }
    }

    async fn enrich_tool_call(&self, update: ToolCallUpdate) -> ToolCallUpdate {
        let tool_call_id = update.tool_call_id.0.as_ref();
        let cached = self.tool_calls.lock().await.get(tool_call_id).cloned();
        if let Some(mut cached) = cached {
            let recovered_raw_input =
                if cached.fields.raw_input.is_none() && update.fields.raw_input.is_none() {
                    cached
                        .fields
                        .content
                        .as_deref()
                        .and_then(raw_input_from_json_content)
                } else {
                    None
                };
            merge_tool_call_update(&mut cached, &update);
            if cached.fields.raw_input.is_none() {
                cached.fields.raw_input = recovered_raw_input;
            }
            cached
        } else {
            update
        }
    }

    pub async fn read_text_file(
        &self,
        request: ReadTextFileRequest,
    ) -> Result<ReadTextFileResponse, Error> {
        if !self.services.read_text_file {
            return Err(Error::method_not_found());
        }
        let path = self.resolve_existing_path(&request.path).await?;
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|_| Error::resource_not_found(None))?;
        if content.len() > self.services.max_file_bytes {
            return Err(Error::invalid_request().data("file exceeds ACP read limit"));
        }
        let content = select_lines(&content, request.line, request.limit);
        Ok(ReadTextFileResponse::new(content))
    }

    pub async fn write_text_file(
        &self,
        request: WriteTextFileRequest,
    ) -> Result<WriteTextFileResponse, Error> {
        if !self.services.write_text_file {
            return Err(Error::method_not_found());
        }
        if request.content.len() > self.services.max_file_bytes {
            return Err(Error::invalid_request().data("file exceeds ACP write limit"));
        }
        let path = self.resolve_write_path(&request.path).await?;
        tokio::fs::write(path, request.content)
            .await
            .map_err(|_| Error::internal_error())?;
        Ok(WriteTextFileResponse::new())
    }

    pub async fn create_terminal(
        &self,
        request: CreateTerminalRequest,
    ) -> Result<CreateTerminalResponse, Error> {
        if !self.services.terminal {
            return Err(Error::method_not_found());
        }
        let cwd = match request.cwd.as_deref() {
            Some(path) => self.resolve_existing_path(path).await?,
            None => self.cwd.clone(),
        };
        {
            let terminals = self.terminals.lock().await;
            if terminals.len() >= self.services.max_terminals {
                return Err(Error::request_cancelled().data("ACP terminal limit reached"));
            }
        }

        let mut command = Command::new(&request.command);
        command
            .args(&request.args)
            .current_dir(cwd)
            .kill_on_drop(true)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &self.terminal_env {
            command.env(key, value);
        }
        for variable in request.env {
            if !is_sensitive_env_name(&variable.name) {
                command.env(variable.name, variable.value);
            }
        }
        let mut child = command.group_spawn().map_err(|_| Error::internal_error())?;
        let stdout = child.inner().stdout.take();
        let stderr = child.inner().stderr.take();
        let terminal_id = uuid::Uuid::new_v4().to_string();
        let max_output_bytes = request
            .output_byte_limit
            .map(|value| value.min(self.services.max_terminal_output_bytes as u64) as usize)
            .unwrap_or(self.services.max_terminal_output_bytes);
        let output = Arc::new(Mutex::new(TerminalBuffer::new(max_output_bytes)));
        let mut readers = Vec::new();
        if let Some(stdout) = stdout {
            readers.push(tokio::spawn(capture_terminal_output(
                stdout,
                output.clone(),
            )));
        }
        if let Some(stderr) = stderr {
            readers.push(tokio::spawn(capture_terminal_output(
                stderr,
                output.clone(),
            )));
        }

        self.terminals.lock().await.insert(
            terminal_id.clone(),
            TerminalRecord {
                session_id: request.session_id.0.to_string(),
                child: Arc::new(Mutex::new(child)),
                output,
                exit_status: Arc::new(Mutex::new(None)),
                readers: Arc::new(Mutex::new(readers)),
            },
        );
        Ok(CreateTerminalResponse::new(TerminalId::new(terminal_id)))
    }

    pub async fn terminal_output(
        &self,
        request: TerminalOutputRequest,
    ) -> Result<TerminalOutputResponse, Error> {
        let record = self
            .terminal_record(&request.session_id.0, &request.terminal_id.0)
            .await?;
        refresh_terminal_status(&record).await?;
        if record.exit_status.lock().await.is_some() {
            drain_terminal_readers(&record).await;
        }
        let output = record.output.lock().await;
        Ok(
            TerminalOutputResponse::new(output.content.clone(), output.truncated)
                .exit_status(record.exit_status.lock().await.clone()),
        )
    }

    pub async fn wait_for_terminal_exit(
        &self,
        request: WaitForTerminalExitRequest,
    ) -> Result<WaitForTerminalExitResponse, Error> {
        let record = self
            .terminal_record(&request.session_id.0, &request.terminal_id.0)
            .await?;
        if let Some(status) = record.exit_status.lock().await.clone() {
            drain_terminal_readers(&record).await;
            return Ok(WaitForTerminalExitResponse::new(status));
        }
        let status = record
            .child
            .lock()
            .await
            .wait()
            .await
            .map_err(|_| Error::internal_error())?;
        let status = terminal_exit_status(status);
        *record.exit_status.lock().await = Some(status.clone());
        drain_terminal_readers(&record).await;
        Ok(WaitForTerminalExitResponse::new(status))
    }

    pub async fn kill_terminal(
        &self,
        request: KillTerminalRequest,
    ) -> Result<KillTerminalResponse, Error> {
        let record = {
            let terminals = self.terminals.lock().await;
            terminals.get(request.terminal_id.0.as_ref()).cloned()
        };
        let Some(record) = record else {
            return Ok(KillTerminalResponse::new());
        };
        if record.session_id != request.session_id.0.as_ref() {
            return Err(Error::invalid_params());
        }
        let mut child = record.child.lock().await;
        if child
            .try_wait()
            .map_err(|_| Error::internal_error())?
            .is_none()
        {
            child.start_kill().map_err(|_| Error::internal_error())?;
        }
        Ok(KillTerminalResponse::new())
    }

    pub async fn release_terminal(
        &self,
        request: ReleaseTerminalRequest,
    ) -> Result<ReleaseTerminalResponse, Error> {
        let record = {
            let mut terminals = self.terminals.lock().await;
            let Some(record) = terminals.get(request.terminal_id.0.as_ref()) else {
                return Ok(ReleaseTerminalResponse::new());
            };
            if record.session_id != request.session_id.0.as_ref() {
                return Err(Error::invalid_params());
            }
            terminals.remove(request.terminal_id.0.as_ref())
        };
        let Some(record) = record else {
            return Ok(ReleaseTerminalResponse::new());
        };
        let mut child = record.child.lock().await;
        if child
            .try_wait()
            .map_err(|_| Error::internal_error())?
            .is_none()
        {
            let _ = child.kill().await;
        }
        drop(child);
        drain_terminal_readers(&record).await;
        Ok(ReleaseTerminalResponse::new())
    }

    pub async fn shutdown_terminals(&self) {
        let records = self
            .terminals
            .lock()
            .await
            .drain()
            .map(|(_, record)| record)
            .collect::<Vec<_>>();
        for record in records {
            let mut child = record.child.lock().await;
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill().await;
            }
            drop(child);
            drain_terminal_readers(&record).await;
        }
    }

    async fn terminal_record(
        &self,
        session_id: &str,
        terminal_id: &str,
    ) -> Result<TerminalRecord, Error> {
        if !self.services.terminal {
            return Err(Error::method_not_found());
        }
        let record = self
            .terminals
            .lock()
            .await
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| Error::resource_not_found(None))?;
        if record.session_id != session_id {
            return Err(Error::invalid_params());
        }
        Ok(record)
    }

    async fn resolve_existing_path(&self, requested: &Path) -> Result<PathBuf, Error> {
        if !self.services.full_access {
            reject_parent_components(requested)?;
        }
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.cwd.join(requested)
        };
        let canonical = tokio::fs::canonicalize(candidate)
            .await
            .map_err(|_| Error::resource_not_found(None))?;
        if self.services.full_access {
            return Ok(canonical);
        }
        self.require_allowed(&canonical).await
    }

    async fn resolve_write_path(&self, requested: &Path) -> Result<PathBuf, Error> {
        if !self.services.full_access {
            reject_parent_components(requested)?;
        }
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.cwd.join(requested)
        };
        if self.services.full_access {
            return Ok(candidate);
        }
        if tokio::fs::symlink_metadata(&candidate).await.is_ok() {
            let canonical = tokio::fs::canonicalize(&candidate)
                .await
                .map_err(|_| Error::resource_not_found(None))?;
            return self.require_allowed(&canonical).await;
        }
        let Some(parent) = candidate.parent() else {
            return Err(Error::invalid_params());
        };
        let canonical_parent = tokio::fs::canonicalize(parent)
            .await
            .map_err(|_| Error::resource_not_found(None))?;
        let canonical_parent = self.require_allowed(&canonical_parent).await?;
        let Some(name) = candidate.file_name() else {
            return Err(Error::invalid_params());
        };
        Ok(canonical_parent.join(name))
    }

    async fn require_allowed(&self, path: &Path) -> Result<PathBuf, Error> {
        for root in std::iter::once(&self.cwd).chain(self.additional_directories.iter()) {
            let canonical_root = tokio::fs::canonicalize(root)
                .await
                .map_err(|_| Error::invalid_params())?;
            if path.starts_with(&canonical_root) {
                return Ok(path.to_path_buf());
            }
        }
        Err(Error::invalid_params().data("path is outside the ACP workspace roots"))
    }
}

fn detect_pi_agent_error_notification(update: &SessionUpdate) -> Option<DetectedApiError> {
    let SessionUpdate::AgentMessageChunk(chunk) = update else {
        return None;
    };
    let level = chunk
        .meta
        .as_ref()?
        .get("piAcp")?
        .get("notify")?
        .get("level")?
        .as_str()?;
    if level != "error" {
        return None;
    }
    let ContentBlock::Text(text) = &chunk.content else {
        return None;
    };
    let message = text.text.trim();
    if message.is_empty() {
        return None;
    }
    Some(
        detect_api_error(message).unwrap_or_else(|| DetectedApiError {
            error_type: crate::logs::NormalizedEntryError::Other,
            message: message.to_string(),
        }),
    )
}

fn merge_tool_call_update(cached: &mut ToolCallUpdate, update: &ToolCallUpdate) {
    if let Some(kind) = update.fields.kind {
        cached.fields.kind = Some(kind);
    }
    if let Some(status) = update.fields.status {
        cached.fields.status = Some(status);
    }
    if let Some(title) = &update.fields.title {
        cached.fields.title = Some(title.clone());
    }
    if let Some(content) = &update.fields.content {
        cached.fields.content = Some(content.clone());
    }
    if let Some(locations) = &update.fields.locations {
        cached.fields.locations = Some(locations.clone());
    }
    if let Some(raw_input) = &update.fields.raw_input {
        cached.fields.raw_input = Some(raw_input.clone());
    }
    if let Some(raw_output) = &update.fields.raw_output {
        cached.fields.raw_output = Some(raw_output.clone());
    }
    if let Some(meta) = &update.meta {
        cached.meta = Some(meta.clone());
    }
}

fn is_terminal_tool_call_status(status: ToolCallStatus) -> bool {
    matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed)
}

fn raw_input_from_json_content(content: &[ToolCallContent]) -> Option<serde_json::Value> {
    content.iter().rev().find_map(|content| {
        let ToolCallContent::Content(content) = content else {
            return None;
        };
        let ContentBlock::Text(text) = &content.content else {
            return None;
        };
        let value = serde_json::from_str::<serde_json::Value>(&text.text).ok()?;
        let command = value.as_object()?.get("command")?.as_str()?.trim();
        (!command.is_empty()).then_some(value)
    })
}

fn detect_tool_result_api_error(update: &SessionUpdate) -> Option<DetectedApiError> {
    let (raw_output, failed_content) = match update {
        SessionUpdate::ToolCall(tool_call) => (
            tool_call.raw_output.as_ref(),
            (tool_call.status == ToolCallStatus::Failed).then_some(&tool_call.content),
        ),
        SessionUpdate::ToolCallUpdate(update) => (
            update.fields.raw_output.as_ref(),
            (update.fields.status == Some(ToolCallStatus::Failed))
                .then_some(update.fields.content.as_ref())
                .flatten(),
        ),
        _ => return None,
    };

    raw_output
        .and_then(detect_explicit_api_error_value)
        .or_else(|| {
            failed_content
                .and_then(|value| serde_json::to_string(value).ok())
                .and_then(|value| detect_api_error(&value))
        })
}

fn detect_explicit_api_error_value(value: &serde_json::Value) -> Option<DetectedApiError> {
    match value {
        serde_json::Value::Object(fields) => fields.iter().find_map(|(key, value)| {
            let normalized_key = key.to_ascii_lowercase();
            let is_error_field = normalized_key == "error"
                || normalized_key == "errors"
                || normalized_key.ends_with("_error")
                || normalized_key.ends_with("_errors")
                || normalized_key == "failure"
                || normalized_key == "failures";
            if is_error_field && let Some(error) = detect_api_error(&value.to_string()) {
                return Some(error);
            }
            detect_explicit_api_error_value(value)
        }),
        serde_json::Value::Array(values) => values.iter().find_map(detect_explicit_api_error_value),
        serde_json::Value::String(message)
            if message
                .to_ascii_lowercase()
                .contains("[provider.api_error]") =>
        {
            detect_api_error(message)
        }
        _ => None,
    }
}

fn permission_option_kind_wire(kind: PermissionOptionKind) -> &'static str {
    match kind {
        PermissionOptionKind::AllowOnce => "allow_once",
        PermissionOptionKind::AllowAlways => "allow_always",
        PermissionOptionKind::RejectOnce => "reject_once",
        PermissionOptionKind::RejectAlways => "reject_always",
        _ => "other",
    }
}

#[derive(Clone)]
struct TerminalRecord {
    session_id: String,
    child: Arc<Mutex<AsyncGroupChild>>,
    output: Arc<Mutex<TerminalBuffer>>,
    exit_status: Arc<Mutex<Option<TerminalExitStatus>>>,
    readers: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

struct TerminalBuffer {
    content: String,
    max_bytes: usize,
    truncated: bool,
}

impl TerminalBuffer {
    fn new(max_bytes: usize) -> Self {
        Self {
            content: String::new(),
            max_bytes,
            truncated: false,
        }
    }

    fn push(&mut self, text: &str) {
        self.content.push_str(text);
        if self.content.len() <= self.max_bytes {
            return;
        }
        self.truncated = true;
        let mut start = self.content.len().saturating_sub(self.max_bytes);
        while start < self.content.len() && !self.content.is_char_boundary(start) {
            start += 1;
        }
        self.content.drain(..start);
    }
}

async fn capture_terminal_output(
    mut reader: impl AsyncRead + Unpin,
    output: Arc<Mutex<TerminalBuffer>>,
) {
    let mut chunk = [0_u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(count) => {
                output
                    .lock()
                    .await
                    .push(&String::from_utf8_lossy(&chunk[..count]));
            }
            Err(_) => break,
        }
    }
}

async fn refresh_terminal_status(record: &TerminalRecord) -> Result<(), Error> {
    if record.exit_status.lock().await.is_some() {
        return Ok(());
    }
    if let Some(status) = record
        .child
        .lock()
        .await
        .try_wait()
        .map_err(|_| Error::internal_error())?
    {
        *record.exit_status.lock().await = Some(terminal_exit_status(status));
    }
    Ok(())
}

async fn drain_terminal_readers(record: &TerminalRecord) {
    let readers = record.readers.lock().await.drain(..).collect::<Vec<_>>();
    for reader in readers {
        let _ = reader.await;
    }
}

fn terminal_exit_status(status: ExitStatus) -> TerminalExitStatus {
    let code = status.code().and_then(|code| u32::try_from(code).ok());
    TerminalExitStatus::new().exit_code(code)
}

fn is_sensitive_env_name(name: &str) -> bool {
    let normalized = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "PRIVATE_KEY",
        "CREDENTIAL",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|fragment| normalized.contains(fragment))
}

fn select_option(
    request: &RequestPermissionRequest,
    preference: &[PermissionOptionKind],
) -> RequestPermissionResponse {
    let option = preference
        .iter()
        .find_map(|kind| request.options.iter().find(|option| option.kind == *kind));
    match option {
        Some(option) => {
            debug!(
                option_kind = ?option.kind,
                "resolved ACP permission request"
            );
            RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(option.option_id.clone()),
            ))
        }
        None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
    }
}

fn reject_parent_components(path: &Path) -> Result<(), Error> {
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Error::invalid_params().data("parent path components are not allowed"));
    }
    Ok(())
}

fn select_lines(content: &str, line: Option<u32>, limit: Option<u32>) -> String {
    let start = line.unwrap_or(1).saturating_sub(1) as usize;
    let limit = limit.map(|value| value as usize).unwrap_or(usize::MAX);
    content
        .lines()
        .skip(start)
        .take(limit)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agent_client_protocol::schema::v1::{
        ContentChunk, CreateTerminalRequest, PermissionOption, SessionId, SessionUpdate,
        TerminalOutputRequest, TextContent, ToolCall, ToolCallId, ToolCallUpdate,
        ToolCallUpdateFields, WaitForTerminalExitRequest,
    };

    use super::*;
    use crate::logs::NormalizedEntryError;

    fn request(kinds: &[PermissionOptionKind]) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            SessionId::new("session"),
            ToolCallUpdate::new(ToolCallId::new("tool"), ToolCallUpdateFields::new()),
            kinds
                .iter()
                .enumerate()
                .map(|(index, kind)| {
                    PermissionOption::new(format!("option-{index}"), "choice", *kind)
                })
                .collect(),
        )
    }

    #[test]
    fn pi_error_notifications_become_terminal_agent_errors() {
        let error_meta = serde_json::json!({
            "piAcp": { "notify": { "level": "error" } }
        })
        .as_object()
        .cloned()
        .expect("Pi ACP error metadata");
        let error = SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(
                "Pi provider connection failed.",
            )))
            .meta(error_meta),
        );
        let detected = detect_pi_agent_error_notification(&error).expect("Pi terminal error");
        assert_eq!(detected.error_type, NormalizedEntryError::Other);
        assert_eq!(detected.message, "Pi provider connection failed.");

        let info_meta = serde_json::json!({
            "piAcp": { "notify": { "level": "info" } }
        })
        .as_object()
        .cloned()
        .expect("Pi ACP info metadata");
        let info = SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("Pi is ready."))).meta(info_meta),
        );
        assert!(detect_pi_agent_error_notification(&info).is_none());
    }

    #[derive(Default)]
    struct CapturingApprovalService {
        request: Mutex<Option<ExecutorApprovalRequest>>,
    }

    #[derive(Default)]
    struct CountingApprovalService {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ExecutorApprovalService for CountingApprovalService {
        async fn request_tool_approval(
            &self,
            _tool_name: &str,
            _tool_input: serde_json::Value,
            _tool_call_id: &str,
            _cancel: CancellationToken,
        ) -> Result<ApprovalStatus, ExecutorApprovalError> {
            unreachable!("ACP requests use request_acp_tool_approval")
        }

        async fn request_acp_tool_approval(
            &self,
            _request: ExecutorApprovalRequest,
            _cancel: CancellationToken,
        ) -> Result<String, ExecutorApprovalError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok("allow".to_string())
        }
    }

    #[tokio::test]
    async fn pi_native_and_mcp_tools_share_ask_auto_allow_auto_reject_policy_matrix() {
        for title in ["Run Pi tool: bash", "Run Pi tool: docs_lookup"] {
            for (policy, expected_option, expected_calls) in [
                (AcpApprovalPolicy::Ask, "allow", 1),
                (AcpApprovalPolicy::AutoAllow, "allow", 0),
                (AcpApprovalPolicy::AutoReject, "reject", 0),
            ] {
                let (output, output_task) = AcpOutput::start(tokio::io::sink());
                let approvals = Arc::new(CountingApprovalService::default());
                let client = AcpClient::new(
                    output.clone(),
                    Some(approvals.clone()),
                    policy,
                    CancellationToken::new(),
                    PathBuf::from("/workspace"),
                    Vec::new(),
                    AcpClientServicePolicy::default(),
                    HashMap::new(),
                );
                let permission = RequestPermissionRequest::new(
                    SessionId::new("pi-session"),
                    ToolCallUpdate::new(
                        ToolCallId::new(format!("{title}-id")),
                        ToolCallUpdateFields::new().title(title),
                    ),
                    vec![
                        PermissionOption::new("allow", "Allow", PermissionOptionKind::AllowOnce),
                        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
                    ],
                );

                let response = client
                    .request_permission(permission)
                    .await
                    .expect("permission response");
                let RequestPermissionOutcome::Selected(selected) = response.outcome else {
                    panic!("expected selected permission for {title} and {policy:?}");
                };
                assert_eq!(selected.option_id.0.as_ref(), expected_option);
                assert_eq!(approvals.calls.load(Ordering::Relaxed), expected_calls);

                drop(client);
                drop(output);
                output_task
                    .await
                    .expect("output task")
                    .expect("output flush");
            }
        }
    }

    #[async_trait::async_trait]
    impl ExecutorApprovalService for CapturingApprovalService {
        async fn request_tool_approval(
            &self,
            _tool_name: &str,
            _tool_input: serde_json::Value,
            _tool_call_id: &str,
            _cancel: CancellationToken,
        ) -> Result<ApprovalStatus, ExecutorApprovalError> {
            unreachable!("ACP requests use request_acp_tool_approval")
        }

        async fn request_acp_tool_approval(
            &self,
            request: ExecutorApprovalRequest,
            _cancel: CancellationToken,
        ) -> Result<String, ExecutorApprovalError> {
            *self.request.lock().await = Some(request);
            Ok("allow-once".to_string())
        }
    }

    #[tokio::test]
    async fn kimi_permission_update_recovers_cached_command() {
        let (output, output_task) = AcpOutput::start(tokio::io::sink());
        let approvals = Arc::new(CapturingApprovalService::default());
        let client = AcpClient::new(
            output.clone(),
            Some(approvals.clone()),
            AcpApprovalPolicy::Ask,
            CancellationToken::new(),
            PathBuf::from("/workspace"),
            Vec::new(),
            AcpClientServicePolicy::default(),
            HashMap::new(),
        );
        let command = format!("echo {}", "x".repeat(254));
        assert_eq!(command.len(), 259);

        client
            .handle_notification(SessionNotification::new(
                SessionId::new("session"),
                SessionUpdate::ToolCall(ToolCall::new(ToolCallId::new("kimi-tool"), "Shell")),
            ))
            .await
            .expect("cache initial tool call");
        let snapshots = [
            r#"{"command":"echo "#.to_string(),
            format!(r#"{{"command":"{}"#, &command[..128]),
            serde_json::to_string(&serde_json::json!({ "command": command.clone() }))
                .expect("serialize complete arguments"),
        ];
        for snapshot in snapshots {
            client
                .handle_notification(SessionNotification::new(
                    SessionId::new("session"),
                    SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                        ToolCallId::new("kimi-tool"),
                        ToolCallUpdateFields::new().content(vec![snapshot.into()]),
                    )),
                ))
                .await
                .expect("cache argument snapshot");
        }

        let permission = RequestPermissionRequest::new(
            SessionId::new("session"),
            ToolCallUpdate::new(
                ToolCallId::new("kimi-tool"),
                ToolCallUpdateFields::new()
                    .title("Run command")
                    .content(vec!["Run command: echo xxxxxxxxx…".into()]),
            ),
            vec![PermissionOption::new(
                "allow-once",
                "Allow once",
                PermissionOptionKind::AllowOnce,
            )],
        );
        client
            .request_permission(permission)
            .await
            .expect("request permission");

        let captured = approvals
            .request
            .lock()
            .await
            .clone()
            .expect("approval request");
        assert_eq!(captured.tool_name, "Run command");
        assert_eq!(
            captured
                .tool_input
                .pointer("/tool_call/rawInput/command")
                .and_then(serde_json::Value::as_str),
            Some(command.as_str())
        );

        client
            .handle_notification(SessionNotification::new(
                SessionId::new("session"),
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new("kimi-tool"),
                    ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
                )),
            ))
            .await
            .expect("observe terminal update");
        assert!(client.tool_calls.lock().await.is_empty());

        drop(client);
        drop(output);
        output_task
            .await
            .expect("output task")
            .expect("output flush");
    }

    #[test]
    fn natural_language_preview_is_not_promoted_to_raw_input() {
        for preview in [
            "Run command: cargo test…",
            r#""cargo test""#,
            r#"{"command":""}"#,
        ] {
            let content = vec![preview.into()];
            assert!(raw_input_from_json_content(&content).is_none());
        }
    }

    #[test]
    fn auto_allow_never_falls_back_to_reject() {
        let response = select_option(
            &request(&[PermissionOptionKind::RejectOnce]),
            &[
                PermissionOptionKind::AllowAlways,
                PermissionOptionKind::AllowOnce,
            ],
        );
        assert!(matches!(
            response.outcome,
            RequestPermissionOutcome::Cancelled
        ));
    }

    #[test]
    fn tool_result_usage_limit_is_promoted_to_terminal_api_error() {
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new("agent-swarm"),
            ToolCallUpdateFields::new().raw_output(serde_json::json!({
                "subagents": [{
                    "status": "failed",
                    "error": "[provider.api_error] 403 You've reached your usage limit for this billing cycle"
                }]
            })),
        ));

        let error = detect_tool_result_api_error(&update)
            .expect("nested provider error should be detected");
        assert!(matches!(
            error.error_type,
            NormalizedEntryError::QuotaExceeded { .. }
        ));
    }

    #[test]
    fn successful_tool_content_that_mentions_rate_limits_is_not_terminal() {
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new("read-file"),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .raw_output(serde_json::json!({
                    "content": "This source file handles rate limit exceeded errors."
                })),
        ));

        assert!(detect_tool_result_api_error(&update).is_none());
    }

    #[test]
    fn auto_reject_prefers_reject_always() {
        let response = select_option(
            &request(&[
                PermissionOptionKind::RejectOnce,
                PermissionOptionKind::RejectAlways,
            ]),
            &[
                PermissionOptionKind::RejectAlways,
                PermissionOptionKind::RejectOnce,
            ],
        );
        let RequestPermissionOutcome::Selected(selected) = response.outcome else {
            panic!("expected selected response");
        };
        assert_eq!(selected.option_id.0.as_ref(), "option-1");
    }

    #[test]
    fn parent_path_is_rejected() {
        assert!(reject_parent_components(Path::new("../secret")).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_guard_rejects_symlink_escape() {
        let root = std::env::temp_dir().join(format!("openteams-acp-fs-{}", uuid::Uuid::new_v4()));
        let outside =
            std::env::temp_dir().join(format!("openteams-acp-outside-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.expect("root");
        tokio::fs::create_dir_all(&outside).await.expect("outside");
        tokio::fs::write(outside.join("secret"), "secret")
            .await
            .expect("secret");
        std::os::unix::fs::symlink(&outside, root.join("escape")).expect("symlink");

        let (output, output_task) = AcpOutput::start(tokio::io::sink());
        let client = AcpClient::new(
            output.clone(),
            None,
            AcpApprovalPolicy::AutoReject,
            CancellationToken::new(),
            root.clone(),
            Vec::new(),
            AcpClientServicePolicy {
                read_text_file: true,
                ..AcpClientServicePolicy::default()
            },
            HashMap::new(),
        );
        let result = client
            .read_text_file(ReadTextFileRequest::new(
                SessionId::new("session"),
                root.join("escape").join("secret"),
            ))
            .await;
        assert!(result.is_err());

        let outside_target = outside.join("write-target");
        tokio::fs::write(&outside_target, "unchanged")
            .await
            .expect("outside target");
        let write_link = root.join("write-link");
        std::os::unix::fs::symlink(&outside_target, &write_link).expect("write symlink");
        let workspace_client = AcpClient::new(
            output.clone(),
            None,
            AcpApprovalPolicy::AutoReject,
            CancellationToken::new(),
            root.clone(),
            Vec::new(),
            AcpClientServicePolicy {
                write_text_file: true,
                ..AcpClientServicePolicy::default()
            },
            HashMap::new(),
        );
        let workspace_write = workspace_client
            .write_text_file(WriteTextFileRequest::new(
                SessionId::new("session"),
                &write_link,
                "blocked",
            ))
            .await;
        assert!(workspace_write.is_err());
        assert_eq!(
            tokio::fs::read_to_string(&outside_target)
                .await
                .expect("outside content"),
            "unchanged"
        );

        let full_access_client = AcpClient::new(
            output.clone(),
            None,
            AcpApprovalPolicy::AutoReject,
            CancellationToken::new(),
            root.clone(),
            Vec::new(),
            AcpClientServicePolicy {
                write_text_file: true,
                full_access: true,
                ..AcpClientServicePolicy::default()
            },
            HashMap::new(),
        );
        full_access_client
            .write_text_file(WriteTextFileRequest::new(
                SessionId::new("session"),
                &write_link,
                "allowed",
            ))
            .await
            .expect("full access write");
        assert_eq!(
            tokio::fs::read_to_string(&outside_target)
                .await
                .expect("outside content"),
            "allowed"
        );

        drop(full_access_client);
        drop(workspace_client);
        drop(client);
        drop(output);
        output_task
            .await
            .expect("output task")
            .expect("output flush");
        tokio::fs::remove_dir_all(root).await.expect("remove root");
        tokio::fs::remove_dir_all(outside)
            .await
            .expect("remove outside");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_lifecycle_is_bounded_and_idempotent() {
        let root =
            std::env::temp_dir().join(format!("openteams-acp-terminal-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.expect("root");
        let (output, output_task) = AcpOutput::start(tokio::io::sink());
        let client = AcpClient::new(
            output.clone(),
            None,
            AcpApprovalPolicy::AutoReject,
            CancellationToken::new(),
            root.clone(),
            Vec::new(),
            AcpClientServicePolicy {
                terminal: true,
                max_terminal_output_bytes: 5,
                ..AcpClientServicePolicy::default()
            },
            HashMap::new(),
        );
        let session_id = SessionId::new("session");
        let created = client
            .create_terminal(
                CreateTerminalRequest::new(session_id.clone(), "/bin/sh")
                    .args(vec!["-c".to_string(), "printf 123456789".to_string()]),
            )
            .await
            .expect("create terminal");
        let wrong_release = client
            .release_terminal(ReleaseTerminalRequest::new(
                SessionId::new("other-session"),
                created.terminal_id.clone(),
            ))
            .await;
        assert!(wrong_release.is_err());
        let waited = client
            .wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                created.terminal_id.clone(),
            ))
            .await
            .expect("wait terminal");
        assert_eq!(waited.exit_status.exit_code, Some(0));
        let terminal_output = client
            .terminal_output(TerminalOutputRequest::new(
                session_id.clone(),
                created.terminal_id.clone(),
            ))
            .await
            .expect("terminal output");
        assert!(terminal_output.truncated);
        assert_eq!(terminal_output.output, "56789");
        let release = ReleaseTerminalRequest::new(session_id, created.terminal_id);
        client
            .release_terminal(release.clone())
            .await
            .expect("release terminal");
        client
            .release_terminal(release)
            .await
            .expect("release terminal twice");

        drop(client);
        drop(output);
        output_task
            .await
            .expect("output task")
            .expect("output flush");
        tokio::fs::remove_dir_all(root).await.expect("remove root");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminal_process_does_not_receive_agent_credentials() {
        let root = std::env::temp_dir().join(format!(
            "openteams-acp-terminal-env-{}",
            uuid::Uuid::new_v4()
        ));
        tokio::fs::create_dir_all(&root).await.expect("root");
        let (output, output_task) = AcpOutput::start(tokio::io::sink());
        let client = AcpClient::new(
            output.clone(),
            None,
            AcpApprovalPolicy::AutoReject,
            CancellationToken::new(),
            root.clone(),
            Vec::new(),
            AcpClientServicePolicy {
                terminal: true,
                ..AcpClientServicePolicy::default()
            },
            HashMap::from([
                (
                    "QODER_PERSONAL_ACCESS_TOKEN".to_string(),
                    "must-not-leak".to_string(),
                ),
                ("SAFE_MARKER".to_string(), "visible".to_string()),
            ]),
        );
        let session_id = SessionId::new("session");
        let created = client
            .create_terminal(
                CreateTerminalRequest::new(session_id.clone(), "/bin/sh").args(vec![
                    "-c".to_string(),
                    "printf '%s|%s' \"$QODER_PERSONAL_ACCESS_TOKEN\" \"$SAFE_MARKER\"".to_string(),
                ]),
            )
            .await
            .expect("create terminal");
        client
            .wait_for_terminal_exit(WaitForTerminalExitRequest::new(
                session_id.clone(),
                created.terminal_id.clone(),
            ))
            .await
            .expect("wait terminal");
        let terminal_output = client
            .terminal_output(TerminalOutputRequest::new(
                session_id.clone(),
                created.terminal_id.clone(),
            ))
            .await
            .expect("terminal output");
        assert_eq!(terminal_output.output, "|visible");
        client
            .release_terminal(ReleaseTerminalRequest::new(session_id, created.terminal_id))
            .await
            .expect("release terminal");
        drop(client);
        drop(output);
        output_task.await.expect("output task").expect("flush");
        tokio::fs::remove_dir_all(root).await.expect("remove root");
    }
}
