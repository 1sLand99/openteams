use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io,
    path::Path,
    sync::Arc,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use rand::{Rng, distributions::Alphanumeric};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt, BufWriter},
    sync::{Mutex as AsyncMutex, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use workspace_utils::{approvals::ApprovalStatus, msg_store::SESSION_INACTIVITY_TIMEOUT};

use super::{
    slash_commands,
    types::{OpencodeExecutorEvent, OpencodeSessionMetadata},
};
use crate::{
    approvals::{
        ExecutorApprovalError, ExecutorApprovalOption, ExecutorApprovalRequest,
        ExecutorApprovalService,
    },
    env::RepoContext,
    executors::{
        ExecutorError,
        opencode::{OpencodeServer, models::maybe_emit_token_usage},
    },
};

#[derive(Clone)]
pub struct LogWriter {
    writer: Arc<AsyncMutex<BufWriter<Box<dyn AsyncWrite + Send + Unpin>>>>,
}

impl LogWriter {
    pub fn new(writer: impl AsyncWrite + Send + Unpin + 'static) -> Self {
        Self {
            writer: Arc::new(AsyncMutex::new(BufWriter::new(Box::new(writer)))),
        }
    }

    pub async fn log_event(&self, event: &OpencodeExecutorEvent) -> Result<(), ExecutorError> {
        let raw =
            serde_json::to_string(event).map_err(|err| ExecutorError::Io(io::Error::other(err)))?;
        self.log_raw(&raw).await
    }

    pub async fn log_error(&self, message: String) -> Result<(), ExecutorError> {
        self.log_event(&OpencodeExecutorEvent::Error { message })
            .await
    }

    pub async fn log_slash_command_result(&self, message: String) -> Result<(), ExecutorError> {
        self.log_event(&OpencodeExecutorEvent::SlashCommandResult { message })
            .await
    }

    async fn log_raw(&self, raw: &str) -> Result<(), ExecutorError> {
        let mut guard = self.writer.lock().await;
        guard
            .write_all(raw.as_bytes())
            .await
            .map_err(ExecutorError::Io)?;
        guard.write_all(b"\n").await.map_err(ExecutorError::Io)?;
        guard.flush().await.map_err(ExecutorError::Io)?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct RunConfig {
    pub base_url: String,
    pub directory: String,
    pub prompt: String,
    pub resume_session_id: Option<String>,
    pub model: Option<String>,
    pub model_variant: Option<String>,
    pub agent: Option<String>,
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
    pub auto_approve: bool,
    pub server_password: String,
    pub expected_version: String,
    /// Cache key for model context windows. Should be derived from configuration
    /// that affects available models (e.g., env vars, base command).
    pub models_cache_key: String,
    pub commit_reminder: bool,
    pub commit_reminder_prompt: String,
    pub repo_context: RepoContext,
}

/// Generate a cryptographically secure random password for OpenCode server auth.
pub fn generate_server_password() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

#[derive(Debug, Deserialize)]
struct HealthResponse {
    healthy: bool,
    version: String,
}

#[derive(Debug, Deserialize)]
struct SessionResponse {
    id: String,
}

/// Information about a discovered command.
#[derive(Debug, Deserialize, Clone)]
pub struct CommandInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Information about an agent.
#[derive(Debug, Deserialize, Clone)]
pub struct AgentInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// Configuration response from the server.
#[derive(Debug, Deserialize)]
pub struct ConfigResponse {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub plugin: Vec<String>,
}

/// Provider configuration response.
#[derive(Debug, Deserialize)]
pub struct ConfigProvidersResponse {
    pub providers: Vec<ProviderInfo>,
    pub default: HashMap<String, String>,
}

/// Information about a provider.
#[derive(Debug, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub name: String,
    #[serde(default)]
    pub models: HashMap<String, Value>,
}

/// Provider list response.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ProviderListResponse {
    pub all: Vec<ProviderInfo>,
    pub default: HashMap<String, String>,
    pub connected: Vec<String>,
}

/// LSP server status.
#[derive(Debug, Deserialize, Clone)]
pub struct LspStatus {
    pub name: String,
    pub root: String,
    pub status: String,
}

/// Formatter status.
#[derive(Debug, Deserialize, Clone)]
pub struct FormatterStatus {
    pub name: String,
    pub extensions: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
struct PromptRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<ModelSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
    parts: Vec<TextPartInput>,
}

#[derive(Debug, Serialize, Clone)]
pub struct ModelSpec {
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(rename = "modelID")]
    pub model_id: String,
}

#[derive(Debug, Serialize)]
struct TextPartInput {
    r#type: &'static str,
    text: String,
}

#[derive(Debug, Clone)]
pub enum ControlEvent {
    Activity,
    Idle,
    AuthRequired { message: String },
    SessionError { message: String },
    Disconnected,
}

/// If OpenCode keeps retrying the same request (e.g. provider rate-limit) and never
/// reaches `session.idle`, fail the run instead of waiting forever.
const SESSION_RETRY_LIMIT_BEFORE_FAIL: u64 = 6;

/// Local executor traffic must never inherit user/system proxy settings. A proxy may accept the
/// loopback connection without forwarding it, leaving the run stuck before session creation.
const LOCAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LOCAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

fn local_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(LOCAL_CONNECT_TIMEOUT)
}

pub async fn run_session(
    config: RunConfig,
    log_writer: LogWriter,
    cancel: CancellationToken,
) -> Result<(), ExecutorError> {
    let client = local_client_builder()
        .default_headers(build_default_headers(
            &config.directory,
            &config.server_password,
        ))
        .build()
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    run_session_inner(config, log_writer, client, cancel).await
}

pub(super) async fn discover_commands(
    server: &OpencodeServer,
    directory: &Path,
    expected_version: &str,
) -> Result<Vec<CommandInfo>, ExecutorError> {
    let directory = directory.to_string_lossy();
    let client = local_client_builder()
        .default_headers(build_default_headers(&directory, &server.server_password))
        .build()
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    let version = wait_for_health(&client, &server.base_url).await?;
    ensure_expected_version(&version, expected_version)?;
    list_commands(&client, &server.base_url, &directory).await
}

pub async fn run_slash_command(
    config: RunConfig,
    log_writer: LogWriter,
    command: slash_commands::OpencodeSlashCommand,
    cancel: CancellationToken,
) -> Result<(), ExecutorError> {
    let client = local_client_builder()
        .default_headers(build_default_headers(
            &config.directory,
            &config.server_password,
        ))
        .build()
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    slash_commands::execute(config, command, log_writer, client, cancel.clone()).await
}

async fn run_session_inner(
    config: RunConfig,
    log_writer: LogWriter,
    client: reqwest::Client,
    cancel: CancellationToken,
) -> Result<(), ExecutorError> {
    let version = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        res = wait_for_health(&client, &config.base_url) => res?,
    };
    ensure_expected_version(&version, &config.expected_version)?;

    let session_id = match config.resume_session_id.as_deref() {
        Some(existing) => {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                res = fork_session(&client, &config.base_url, &config.directory, existing) => res?,
            }
        }
        None => tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            res = create_session(&client, &config.base_url, &config.directory) => res?,
        },
    };

    log_writer
        .log_event(&OpencodeExecutorEvent::SessionStart {
            session_id: session_id.clone(),
        })
        .await?;

    let model = config.model.as_deref().and_then(parse_model);

    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ControlEvent>();

    let event_resp = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        res = connect_event_stream(&client, &config.base_url, &config.directory, None) => res?,
    };
    let event_handle = tokio::spawn(spawn_event_listener(
        EventListenerConfig {
            client: client.clone(),
            base_url: config.base_url.clone(),
            directory: config.directory.clone(),
            session_id: session_id.clone(),
            log_writer: log_writer.clone(),
            approvals: config.approvals.clone(),
            auto_approve: config.auto_approve,
            control_tx,
            models_cache_key: config.models_cache_key.clone(),
            cancel: cancel.clone(),
        },
        event_resp,
    ));

    let prompt_fut = Box::pin(prompt(
        &client,
        &config.base_url,
        &config.directory,
        &session_id,
        &config.prompt,
        model.clone(),
        config.model_variant.clone(),
        config.agent.clone(),
    ));
    let prompt_result = run_request_with_control(prompt_fut, &mut control_rx, cancel.clone()).await;

    if cancel.is_cancelled() {
        send_abort(&client, &config.base_url, &config.directory, &session_id).await;
        event_handle.abort();
        return Ok(());
    }

    if let Err(err) = prompt_result {
        event_handle.abort();
        return Err(err);
    }

    // Handle commit reminder if enabled
    if config.commit_reminder
        && !cancel.is_cancelled()
        && let status = config.repo_context.check_uncommitted_changes().await
        && !status.is_empty()
    {
        let reminder_prompt = format!("{}\n{}", config.commit_reminder_prompt, status);
        tracing::debug!("Sending commit reminder prompt to OpenCode session");

        // Log as system message so it's visible in the UI (user_message gets filtered out)
        let _ = log_writer
            .log_event(&OpencodeExecutorEvent::SystemMessage {
                content: reminder_prompt.clone(),
            })
            .await;

        let reminder_fut = Box::pin(prompt(
            &client,
            &config.base_url,
            &config.directory,
            &session_id,
            &reminder_prompt,
            model,
            config.model_variant.clone(),
            config.agent.clone(),
        ));
        let reminder_result =
            run_request_with_control(reminder_fut, &mut control_rx, cancel.clone()).await;

        if let Err(e) = reminder_result {
            // Log but don't fail the session on commit reminder errors
            tracing::warn!("Commit reminder prompt failed: {e}");
        }
    }

    if cancel.is_cancelled() {
        send_abort(&client, &config.base_url, &config.directory, &session_id).await;
    }

    event_handle.abort();

    log_writer.log_event(&OpencodeExecutorEvent::Done).await?;

    Ok(())
}

pub(super) fn build_default_headers(directory: &str, password: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(directory) {
        headers.insert("x-opencode-directory", value);
    }
    let credentials = BASE64.encode(format!("opencode:{password}"));
    if let Ok(value) = HeaderValue::from_str(&format!("Basic {credentials}")) {
        headers.insert(AUTHORIZATION, value);
    }
    headers
}

pub async fn run_request_with_control<F>(
    mut request_fut: F,
    control_rx: &mut mpsc::UnboundedReceiver<ControlEvent>,
    cancel: CancellationToken,
) -> Result<(), ExecutorError>
where
    F: Future<Output = Result<(), ExecutorError>> + Unpin,
{
    run_request_with_control_timeout(
        &mut request_fut,
        control_rx,
        cancel,
        SESSION_INACTIVITY_TIMEOUT,
    )
    .await
}

async fn run_request_with_control_timeout<F>(
    request_fut: &mut F,
    control_rx: &mut mpsc::UnboundedReceiver<ControlEvent>,
    cancel: CancellationToken,
    activity_timeout: Duration,
) -> Result<(), ExecutorError>
where
    F: Future<Output = Result<(), ExecutorError>> + Unpin,
{
    let mut idle_seen = false;
    let activity_error = || {
        ExecutorError::Io(io::Error::other(format!(
            "OpenCode request timed out after {}s without session activity",
            activity_timeout.as_secs()
        )))
    };
    let activity_deadline = tokio::time::sleep(activity_timeout);
    tokio::pin!(activity_deadline);

    let request_result = loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = &mut activity_deadline => return Err(activity_error()),
            res = &mut *request_fut => break res,
            event = control_rx.recv() => match event {
                Some(ControlEvent::Activity) => {
                    activity_deadline.as_mut().reset(Instant::now() + activity_timeout);
                }
                Some(ControlEvent::AuthRequired { message }) => return Err(ExecutorError::AuthRequired(message)),
                Some(ControlEvent::SessionError { message }) => {
                    return Err(ExecutorError::Io(io::Error::other(message)));
                }
                Some(ControlEvent::Disconnected) if !cancel.is_cancelled() => {
                    return Err(ExecutorError::Io(io::Error::other("OpenCode event stream disconnected while request was running")));
                }
                Some(ControlEvent::Disconnected) => return Ok(()),
                Some(ControlEvent::Idle) => {
                    idle_seen = true;
                    break Ok(());
                }
                None => {}
            }
        }
    };

    if let Err(err) = request_result {
        if cancel.is_cancelled() {
            return Ok(());
        }
        if let Some(control_err) =
            wait_for_control_error(control_rx, cancel.clone(), Duration::from_millis(500)).await
        {
            return Err(control_err);
        }
        return Err(err);
    }

    if !idle_seen {
        // The OpenCode server streams events independently; wait for `session.idle` so we capture
        // tail updates reliably (e.g. final tool completion events).
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = &mut activity_deadline => return Err(activity_error()),
                event = control_rx.recv() => match event {
                    Some(ControlEvent::Activity) => {
                        activity_deadline.as_mut().reset(Instant::now() + activity_timeout);
                    }
                    Some(ControlEvent::Idle) | None => break,
                    Some(ControlEvent::AuthRequired { message }) => return Err(ExecutorError::AuthRequired(message)),
                    Some(ControlEvent::SessionError { message }) => {
                        return Err(ExecutorError::Io(io::Error::other(message)));
                    }
                    Some(ControlEvent::Disconnected) if !cancel.is_cancelled() => {
                        return Err(ExecutorError::Io(io::Error::other(
                            "OpenCode event stream disconnected while waiting for session to go idle",
                        )));
                    }
                    Some(ControlEvent::Disconnected) => return Ok(()),
                }
            }
        }
    }

    Ok(())
}

async fn wait_for_control_error(
    control_rx: &mut mpsc::UnboundedReceiver<ControlEvent>,
    cancel: CancellationToken,
    timeout: Duration,
) -> Option<ExecutorError> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return None,
            _ = &mut deadline => return None,
            event = control_rx.recv() => match event {
                Some(ControlEvent::AuthRequired { message }) => {
                    return Some(ExecutorError::AuthRequired(message));
                }
                Some(ControlEvent::SessionError { message }) => {
                    return Some(ExecutorError::Io(io::Error::other(message)));
                }
                Some(ControlEvent::Disconnected) | None => return None,
                Some(ControlEvent::Activity | ControlEvent::Idle) => {}
            }
        }
    }
}

pub async fn wait_for_health(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<String, ExecutorError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut last_err: Option<String> = None;

    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(ExecutorError::Io(io::Error::other(format!(
                "Timed out waiting for OpenCode server health: {}",
                last_err.unwrap_or_else(|| "unknown error".to_string())
            ))));
        }

        let resp = client
            .get(format!("{base_url}/global/health"))
            .timeout(HEALTH_REQUEST_TIMEOUT)
            .send()
            .await;
        match resp {
            Ok(resp) => {
                if !resp.status().is_success() {
                    last_err = Some(format!("HTTP {}", resp.status()));
                } else if let Ok(body) = resp.json::<HealthResponse>().await {
                    if body.healthy {
                        return Ok(body.version);
                    }
                    last_err = Some(format!("unhealthy server (version {})", body.version));
                } else {
                    last_err = Some("failed to parse health response".to_string());
                }
            }
            Err(err) => {
                last_err = Some(err.to_string());
            }
        }

        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

pub fn ensure_expected_version(actual: &str, expected: &str) -> Result<(), ExecutorError> {
    if actual == expected {
        return Ok(());
    }

    Err(ExecutorError::Io(io::Error::other(format!(
        "OpenCode server version mismatch: expected opencode-ai {expected}, got {actual}. \
This usually means the executor launched a different OpenCode binary than the pinned version."
    ))))
}

pub async fn create_session(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<String, ExecutorError> {
    let resp = client
        .post(format!("{base_url}/session"))
        .query(&[("directory", directory)])
        .json(&serde_json::json!({}))
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.create failed: HTTP {}",
            resp.status()
        ))));
    }

    let session = resp
        .json::<SessionResponse>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;
    Ok(session.id)
}

pub async fn fork_session(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    session_id: &str,
) -> Result<String, ExecutorError> {
    let resp = client
        .post(format!("{base_url}/session/{session_id}/fork"))
        .query(&[("directory", directory)])
        .json(&serde_json::json!({}))
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.fork failed: HTTP {}",
            resp.status()
        ))));
    }

    let session = resp
        .json::<SessionResponse>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;
    Ok(session.id)
}

fn preview_http_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "<empty body>".to_string();
    }

    const MAX_CHARS: usize = 1200;
    let total_chars = trimmed.chars().count();
    if total_chars <= MAX_CHARS {
        return trimmed.to_string();
    }

    let mut preview = trimmed.chars().take(MAX_CHARS).collect::<String>();
    preview.push_str(" ...<truncated>");
    preview
}

#[allow(clippy::too_many_arguments)]
async fn prompt(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    session_id: &str,
    prompt: &str,
    model: Option<ModelSpec>,
    model_variant: Option<String>,
    agent: Option<String>,
) -> Result<(), ExecutorError> {
    tracing::debug!(
        base_url = %base_url,
        directory = %directory,
        session_id = %session_id,
        prompt_len = prompt.chars().count(),
        model = ?model.as_ref().map(|m| format!("{}/{}", m.provider_id, m.model_id)),
        model_variant = ?model_variant,
        agent = ?agent,
        "Sending OpenCode session.message request"
    );
    let req = PromptRequest {
        model,
        agent,
        variant: model_variant,
        parts: vec![TextPartInput {
            r#type: "text",
            text: prompt.to_string(),
        }],
    };

    let resp = client
        .post(format!("{base_url}/session/{session_id}/message"))
        .query(&[("directory", directory)])
        .json(&req)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    // The OpenCode server uses streaming responses and may set the HTTP status early; validate
    // success using the response body shape as well.
    if !status.is_success() {
        let body_preview = preview_http_error_body(&body);
        tracing::warn!(
            base_url = %base_url,
            directory = %directory,
            session_id = %session_id,
            status = %status,
            body_len = body.len(),
            body = %body_preview,
            "OpenCode session.message returned non-success status"
        );
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.prompt failed: HTTP {status} {body_preview}"
        ))));
    }

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(ExecutorError::Io(io::Error::other(
            "OpenCode session.prompt returned empty response body",
        )));
    }

    let parsed: Value =
        serde_json::from_str(trimmed).map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    // Success response: { info, parts }
    if parsed.get("info").is_some() && parsed.get("parts").is_some() {
        tracing::debug!(
            base_url = %base_url,
            directory = %directory,
            session_id = %session_id,
            response_len = body.len(),
            "OpenCode session.message succeeded"
        );
        return Ok(());
    }

    // Error response: { name, data }
    if let Some(name) = parsed.get("name").and_then(Value::as_str) {
        let message = parsed
            .pointer("/data/message")
            .and_then(Value::as_str)
            .unwrap_or(trimmed);
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.prompt failed: {name}: {message}"
        ))));
    }

    Err(ExecutorError::Io(io::Error::other(format!(
        "OpenCode session.prompt returned unexpected response: {trimmed}"
    ))))
}

#[derive(Debug, Serialize)]
struct SessionCommandRequest {
    command: String,
    arguments: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    variant: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub async fn session_command(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    session_id: &str,
    command: String,
    arguments: String,
    agent: Option<String>,
    model: Option<String>,
    model_variant: Option<String>,
) -> Result<(), ExecutorError> {
    let req = SessionCommandRequest {
        command,
        arguments,
        agent,
        model,
        variant: model_variant,
    };

    let resp = client
        .post(format!("{base_url}/session/{session_id}/command"))
        .query(&[("directory", directory)])
        .json(&req)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !status.is_success() {
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.command failed: HTTP {status} {body}"
        ))));
    }

    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err(ExecutorError::Io(io::Error::other(
            "OpenCode session.command returned empty response body",
        )));
    }

    let parsed: Value =
        serde_json::from_str(trimmed).map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if parsed.get("info").is_some() && parsed.get("parts").is_some() {
        return Ok(());
    }

    if let Some(name) = parsed.get("name").and_then(Value::as_str) {
        let message = parsed
            .pointer("/data/message")
            .and_then(Value::as_str)
            .unwrap_or(trimmed);
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode session.command failed: {name}: {message}"
        ))));
    }

    Err(ExecutorError::Io(io::Error::other(format!(
        "OpenCode session.command returned unexpected response: {trimmed}"
    ))))
}

#[derive(Debug, Serialize)]
struct SummarizeRequest {
    #[serde(rename = "providerID")]
    provider_id: String,
    #[serde(rename = "modelID")]
    model_id: String,
    auto: bool,
}

pub async fn session_summarize(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    session_id: &str,
    model: ModelSpec,
) -> Result<(), ExecutorError> {
    let req = SummarizeRequest {
        provider_id: model.provider_id,
        model_id: model.model_id,
        auto: false,
    };

    let resp = client
        .post(format!("{base_url}/session/{session_id}/summarize"))
        .query(&[("directory", directory)])
        .json(&req)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(build_response_error(resp, "session.summarize").await);
    }

    let _ = resp
        .json::<bool>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;
    Ok(())
}

pub async fn list_commands(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<Vec<CommandInfo>, ExecutorError> {
    let resp = client
        .get(format!("{base_url}/command"))
        .query(&[("directory", directory)])
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(build_response_error(resp, "command.list").await);
    }

    resp.json::<Vec<CommandInfo>>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))
}

pub async fn list_agents(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<Vec<AgentInfo>, ExecutorError> {
    let resp = client
        .get(format!("{base_url}/agent"))
        .query(&[("directory", directory)])
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(build_response_error(resp, "agent.list").await);
    }

    resp.json::<Vec<AgentInfo>>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))
}

pub async fn config_get(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<ConfigResponse, ExecutorError> {
    let resp = client
        .get(format!("{base_url}/config"))
        .query(&[("directory", directory)])
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(build_response_error(resp, "config.get").await);
    }

    resp.json::<ConfigResponse>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))
}

pub async fn list_config_providers(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<ConfigProvidersResponse, ExecutorError> {
    let resp = client
        .get(format!("{base_url}/config/providers"))
        .query(&[("directory", directory)])
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(build_response_error(resp, "config.providers").await);
    }

    resp.json::<ConfigProvidersResponse>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))
}

pub async fn list_providers(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<ProviderListResponse, ExecutorError> {
    let resp = client
        .get(format!("{base_url}/provider"))
        .query(&[("directory", directory)])
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(build_response_error(resp, "provider.list").await);
    }

    resp.json::<ProviderListResponse>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))
}

pub async fn mcp_status(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<HashMap<String, Value>, ExecutorError> {
    let resp = client
        .get(format!("{base_url}/mcp"))
        .query(&[("directory", directory)])
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(build_response_error(resp, "mcp.status").await);
    }

    resp.json::<HashMap<String, Value>>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))
}

pub async fn lsp_status(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<Vec<LspStatus>, ExecutorError> {
    let resp = client
        .get(format!("{base_url}/lsp"))
        .query(&[("directory", directory)])
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(build_response_error(resp, "lsp.status").await);
    }

    resp.json::<Vec<LspStatus>>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))
}

pub async fn formatter_status(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
) -> Result<Vec<FormatterStatus>, ExecutorError> {
    let resp = client
        .get(format!("{base_url}/formatter"))
        .query(&[("directory", directory)])
        .timeout(LOCAL_REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        return Err(build_response_error(resp, "formatter.status").await);
    }

    resp.json::<Vec<FormatterStatus>>()
        .await
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))
}

async fn build_response_error(resp: reqwest::Response, context: &str) -> ExecutorError {
    let status = resp.status();
    let body = resp
        .text()
        .await
        .unwrap_or_else(|_| "<failed to read response body>".to_string());
    ExecutorError::Io(io::Error::other(format!(
        "OpenCode {context} failed: HTTP {status} {body}"
    )))
}

pub async fn send_abort(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    session_id: &str,
) {
    let request = client
        .post(format!("{base_url}/session/{session_id}/abort"))
        .query(&[("directory", directory)]);

    let _ = tokio::time::timeout(Duration::from_millis(800), async move {
        let resp = request.send().await;
        if let Ok(resp) = resp {
            // Drain body
            let _ = resp.bytes().await;
        }
    })
    .await;
}

fn parse_model(model: &str) -> Option<ModelSpec> {
    parse_model_strict(model)
}

fn parse_model_strict(model: &str) -> Option<ModelSpec> {
    let (provider_id, model_id) = model.split_once('/')?;
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }
    Some(ModelSpec {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
    })
}

pub async fn resolve_compaction_model(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    configured_model: Option<&str>,
) -> Result<ModelSpec, ExecutorError> {
    if let Some(model) = configured_model.and_then(parse_model_strict) {
        return Ok(model);
    }

    let config = config_get(client, base_url, directory).await?;
    if let Some(model) = config.model.as_deref().and_then(parse_model_strict) {
        return Ok(model);
    }

    let providers = list_config_providers(client, base_url, directory).await?;
    let mut provider_ids: Vec<_> = providers.default.keys().cloned().collect();
    provider_ids.sort();

    if let Some(provider_id) = provider_ids.first()
        && let Some(model_id) = providers.default.get(provider_id)
    {
        return Ok(ModelSpec {
            provider_id: provider_id.clone(),
            model_id: model_id.clone(),
        });
    }

    if let Some(provider) = providers.providers.first()
        && let Some((model_id, _)) = provider.models.iter().next()
    {
        return Ok(ModelSpec {
            provider_id: provider.id.clone(),
            model_id: model_id.clone(),
        });
    }

    Err(ExecutorError::Io(io::Error::other(
        "OpenCode compaction requires a configured model",
    )))
}

pub async fn connect_event_stream(
    client: &reqwest::Client,
    base_url: &str,
    directory: &str,
    last_event_id: Option<&str>,
) -> Result<reqwest::Response, ExecutorError> {
    let mut req = client
        .get(format!("{base_url}/event"))
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .query(&[("directory", directory)]);

    if let Some(last_event_id) = last_event_id {
        req = req.header("Last-Event-ID", last_event_id);
    }

    let resp = tokio::time::timeout(LOCAL_REQUEST_TIMEOUT, req.send())
        .await
        .map_err(|_| {
            ExecutorError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "OpenCode event stream connection timed out",
            ))
        })?
        .map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read response body>".to_string());
        return Err(ExecutorError::Io(io::Error::other(format!(
            "OpenCode event stream failed: HTTP {status} {body}"
        ))));
    }

    Ok(resp)
}

pub struct EventListenerConfig {
    pub client: reqwest::Client,
    pub base_url: String,
    pub directory: String,
    pub session_id: String,
    pub log_writer: LogWriter,
    pub approvals: Option<Arc<dyn ExecutorApprovalService>>,
    pub auto_approve: bool,
    pub control_tx: mpsc::UnboundedSender<ControlEvent>,
    pub models_cache_key: String,
    pub cancel: CancellationToken,
}

pub async fn spawn_event_listener(config: EventListenerConfig, initial_resp: reqwest::Response) {
    let EventListenerConfig {
        client,
        base_url,
        directory,
        session_id,
        log_writer,
        approvals,
        auto_approve,
        control_tx,
        models_cache_key,
        cancel,
    } = config;

    let mut seen_permissions: HashSet<String> = HashSet::new();
    let mut sessions = ProjectedSessionGraph::new(session_id.clone());
    let mut last_event_id: Option<String> = None;
    let mut base_retry_delay = Duration::from_millis(3000);
    let mut attempt: u32 = 0;
    let max_attempts: u32 = 20;
    let mut resp: Option<reqwest::Response> = Some(initial_resp);

    loop {
        let current_resp = match resp.take() {
            Some(r) => {
                attempt = 0;
                r
            }
            None => {
                match connect_event_stream(&client, &base_url, &directory, last_event_id.as_deref())
                    .await
                {
                    Ok(r) => {
                        attempt = 0;
                        r
                    }
                    Err(err) => {
                        let _ = log_writer
                            .log_error(format!("OpenCode event stream reconnect failed: {err}"))
                            .await;
                        attempt += 1;
                        if attempt >= max_attempts {
                            let _ = control_tx.send(ControlEvent::Disconnected);
                            return;
                        }

                        tokio::time::sleep(exponential_backoff(base_retry_delay, attempt)).await;
                        continue;
                    }
                }
            }
        };

        let outcome = process_event_stream(
            EventStreamContext {
                seen_permissions: &mut seen_permissions,
                sessions: &mut sessions,
                client: &client,
                base_url: &base_url,
                directory: &directory,
                log_writer: &log_writer,
                approvals: approvals.clone(),
                auto_approve,
                control_tx: &control_tx,
                base_retry_delay: &mut base_retry_delay,
                last_event_id: &mut last_event_id,
                models_cache_key: &models_cache_key,
                cancel: cancel.clone(),
            },
            current_resp,
        )
        .await;

        match outcome {
            Ok(EventStreamOutcome::Idle) => {
                // Keep listening - there may be more prompts (e.g., commit reminder)
                // The task will be aborted by event_handle.abort() when done
                resp = None;
                continue;
            }
            Ok(EventStreamOutcome::Terminal) => return,
            Ok(EventStreamOutcome::Disconnected) | Err(_) => {
                attempt += 1;
                if attempt >= max_attempts {
                    let _ = control_tx.send(ControlEvent::Disconnected);
                    return;
                }
            }
        }

        tokio::time::sleep(exponential_backoff(base_retry_delay, attempt)).await;
        resp = None;
    }
}

fn exponential_backoff(base: Duration, attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(10);
    let mult = 1u32 << exp;
    base.checked_mul(mult)
        .unwrap_or(Duration::from_secs(30))
        .min(Duration::from_secs(30))
}

fn extract_retry_status(event: &Value) -> Option<(u64, String)> {
    let status_type = event
        .pointer("/properties/status/type")
        .and_then(Value::as_str)?;
    if status_type != "retry" {
        return None;
    }

    let attempt = event
        .pointer("/properties/status/attempt")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let message = event
        .pointer("/properties/status/message")
        .and_then(Value::as_str)
        .unwrap_or("OpenCode request is retrying")
        .to_string();

    Some((attempt, message))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventStreamOutcome {
    Idle,
    Terminal,
    Disconnected,
}

#[derive(Debug)]
struct ProjectedSessionGraph {
    root_session_id: String,
    sessions: HashMap<String, OpencodeSessionMetadata>,
}

impl ProjectedSessionGraph {
    fn new(root_session_id: String) -> Self {
        let root = OpencodeSessionMetadata {
            session_id: root_session_id.clone(),
            parent_session_id: None,
            title: None,
        };
        Self {
            root_session_id,
            sessions: HashMap::from([(root.session_id.clone(), root)]),
        }
    }

    fn observe_session_event(&mut self, event_type: &str, event: &Value) {
        if !matches!(event_type, "session.created" | "session.updated") {
            return;
        }

        let Some(session_id) = event
            .pointer("/properties/info/id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
        else {
            return;
        };
        let parent_session_id = event
            .pointer("/properties/info/parentID")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty());

        let belongs_to_run = session_id == self.root_session_id
            || self.sessions.contains_key(session_id)
            || parent_session_id.is_some_and(|parent| self.sessions.contains_key(parent));
        if !belongs_to_run {
            return;
        }

        let title = event
            .pointer("/properties/info/title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string);
        let existing = self.sessions.get(session_id);
        self.sessions.insert(
            session_id.to_string(),
            OpencodeSessionMetadata {
                session_id: session_id.to_string(),
                parent_session_id: parent_session_id
                    .map(str::to_string)
                    .or_else(|| existing.and_then(|session| session.parent_session_id.clone())),
                title: title.or_else(|| existing.and_then(|session| session.title.clone())),
            },
        );
    }

    fn metadata_for_event(
        &self,
        event_type: &str,
        event: &Value,
    ) -> Option<OpencodeSessionMetadata> {
        let session_id = event_session_id(event_type, event)?;
        self.sessions.get(session_id).cloned()
    }

    fn is_root_event(&self, event_type: &str, event: &Value) -> bool {
        event_session_id(event_type, event) == Some(self.root_session_id.as_str())
    }
}

pub(super) struct EventStreamContext<'a> {
    seen_permissions: &'a mut HashSet<String>,
    sessions: &'a mut ProjectedSessionGraph,
    pub client: &'a reqwest::Client,
    pub base_url: &'a str,
    pub directory: &'a str,
    pub log_writer: &'a LogWriter,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    auto_approve: bool,
    control_tx: &'a mpsc::UnboundedSender<ControlEvent>,
    base_retry_delay: &'a mut Duration,
    last_event_id: &'a mut Option<String>,
    /// Cache key for model context windows, derived from config that affects available models.
    pub models_cache_key: &'a str,
    cancel: CancellationToken,
}

async fn process_event_stream(
    ctx: EventStreamContext<'_>,
    resp: reqwest::Response,
) -> Result<EventStreamOutcome, ExecutorError> {
    let mut stream = resp.bytes_stream().eventsource();

    loop {
        let evt = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                return Ok(EventStreamOutcome::Terminal);
            }
            evt = stream.next() => {
                match evt {
                    Some(evt) => evt,
                    None => break,
                }
            }
        };
        let evt = evt.map_err(|err| ExecutorError::Io(io::Error::other(err)))?;

        if !evt.id.trim().is_empty() {
            *ctx.last_event_id = Some(evt.id.trim().to_string());
        }
        if let Some(retry) = evt.retry {
            *ctx.base_retry_delay = retry;
        }

        let trimmed = evt.data.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(data) = serde_json::from_str::<Value>(trimmed) else {
            let _ = ctx
                .log_writer
                .log_error(format!(
                    "OpenCode event stream delivered non-JSON event payload: {trimmed}"
                ))
                .await;
            continue;
        };

        let Some(event_type) = data.get("type").and_then(Value::as_str) else {
            continue;
        };

        ctx.sessions.observe_session_event(event_type, &data);
        if matches!(event_type, "session.created" | "session.updated") {
            continue;
        }

        let session = ctx.sessions.metadata_for_event(event_type, &data);
        let root_state_event = matches!(
            event_type,
            "session.idle" | "session.error" | "session.status"
        );
        let permission_event = matches!(event_type, "permission.asked" | "permission.replied");
        if root_state_event {
            if !ctx.sessions.is_root_event(event_type, &data) {
                continue;
            }
        } else if session.is_none() && !permission_event {
            continue;
        }

        // Unknown-session permission requests are still bridged because a child may request
        // approval before its creation event is observed. Do not project those unverified events
        // into the run log; all visible activity must belong to the tracked root session tree.
        if session.is_some() || root_state_event {
            let _ = ctx
                .log_writer
                .log_event(&OpencodeExecutorEvent::SdkEvent {
                    event: data.clone(),
                    session,
                })
                .await;
        }
        let _ = ctx.control_tx.send(ControlEvent::Activity);

        match event_type {
            "message.updated" => {
                maybe_emit_token_usage(&ctx, &data).await;
            }
            "session.status" => {
                if let Some((attempt, message)) = extract_retry_status(&data)
                    && attempt >= SESSION_RETRY_LIMIT_BEFORE_FAIL
                {
                    let message =
                        format!("OpenCode request exceeded retry limit ({attempt}): {message}");
                    let _ = ctx.control_tx.send(ControlEvent::SessionError { message });
                    return Ok(EventStreamOutcome::Terminal);
                }
            }
            "session.idle" => {
                let _ = ctx.control_tx.send(ControlEvent::Idle);
                return Ok(EventStreamOutcome::Idle);
            }
            "session.error" => {
                let error_type = data
                    .pointer("/properties/error/name")
                    .or_else(|| data.pointer("/properties/error/type"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let message = data
                    .pointer("/properties/error/data/message")
                    .or_else(|| data.pointer("/properties/error/message"))
                    .and_then(Value::as_str)
                    .unwrap_or("OpenCode session error")
                    .to_string();

                if error_type == "ProviderAuthError" {
                    let _ = ctx.control_tx.send(ControlEvent::AuthRequired { message });
                    return Ok(EventStreamOutcome::Terminal);
                }

                let _ = ctx.control_tx.send(ControlEvent::SessionError { message });
            }
            "permission.asked" => {
                let request_id = data
                    .pointer("/properties/id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();

                if request_id.is_empty() || !ctx.seen_permissions.insert(request_id.clone()) {
                    continue;
                }

                let tool_call_id = data
                    .pointer("/properties/tool/callID")
                    .and_then(Value::as_str)
                    .unwrap_or(&request_id)
                    .to_string();

                let permission = data
                    .pointer("/properties/permission")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();

                let tool_input = data
                    .get("properties")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));

                let approvals = ctx.approvals.clone();
                let client = ctx.client.clone();
                let base_url = ctx.base_url.to_string();
                let directory = ctx.directory.to_string();
                let log_writer = ctx.log_writer.clone();
                let auto_approve = ctx.auto_approve;
                let cancel = ctx.cancel.clone();
                tokio::spawn(async move {
                    let reply = match request_permission_approval(
                        auto_approve,
                        approvals,
                        &permission,
                        tool_input,
                        &tool_call_id,
                        cancel,
                    )
                    .await
                    {
                        Ok(reply) => reply,
                        Err(ExecutorApprovalError::Cancelled) => {
                            tracing::debug!(
                                "OpenCode approval cancelled for tool_call_id={}",
                                tool_call_id
                            );
                            return;
                        }
                        Err(err) => {
                            tracing::error!(
                                "OpenCode approval failed for tool_call_id={}: {err}",
                                tool_call_id
                            );
                            return;
                        }
                    };

                    let (status, message) = match reply {
                        OpencodePermissionReply::Once | OpencodePermissionReply::Always => {
                            (ApprovalStatus::Approved, None)
                        }
                        OpencodePermissionReply::Reject => {
                            let message = "User denied this tool use request".to_string();
                            (ApprovalStatus::Denied { reason: None }, Some(message))
                        }
                    };

                    let _ = log_writer
                        .log_event(&OpencodeExecutorEvent::ApprovalResponse {
                            tool_call_id: tool_call_id.clone(),
                            status: status.clone(),
                        })
                        .await;

                    // If we reject without a message, OpenCode treats it as a hard stop.
                    // Provide a message so the agent can continue with guidance.
                    let payload = if reply == OpencodePermissionReply::Reject {
                        serde_json::json!({ "reply": reply.as_str(), "message": message.unwrap_or_else(|| "User denied this tool use request".to_string()) })
                    } else {
                        serde_json::json!({ "reply": reply.as_str() })
                    };

                    let _ = client
                        .post(format!("{base_url}/permission/{request_id}/reply"))
                        .query(&[("directory", directory.as_str())])
                        .json(&payload)
                        .timeout(LOCAL_REQUEST_TIMEOUT)
                        .send()
                        .await;
                });
            }
            _ => {}
        }
    }

    Ok(EventStreamOutcome::Disconnected)
}

fn event_session_id<'a>(event_type: &str, event: &'a Value) -> Option<&'a str> {
    match event_type {
        "message.updated" => event
            .pointer("/properties/info/sessionID")
            .and_then(Value::as_str),
        "message.part.updated" => event
            .pointer("/properties/part/sessionID")
            .and_then(Value::as_str),
        "session.idle" | "session.error" => event
            .pointer("/properties/sessionID")
            .and_then(Value::as_str),
        _ => event
            .pointer("/properties/sessionID")
            .and_then(Value::as_str)
            .or_else(|| {
                event
                    .pointer("/properties/info/sessionID")
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                event
                    .pointer("/properties/part/sessionID")
                    .and_then(Value::as_str)
            }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpencodePermissionReply {
    Once,
    Always,
    Reject,
}

impl OpencodePermissionReply {
    fn from_option_id(option_id: &str) -> Result<Self, ExecutorApprovalError> {
        match option_id {
            "once" => Ok(Self::Once),
            "always" => Ok(Self::Always),
            "reject" => Ok(Self::Reject),
            _ => Err(ExecutorApprovalError::RequestFailed(format!(
                "OpenCode approval service selected unknown option `{option_id}`"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Reject => "reject",
        }
    }
}

async fn request_permission_approval(
    auto_approve: bool,
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
    tool_name: &str,
    tool_input: Value,
    tool_call_id: &str,
    cancel: CancellationToken,
) -> Result<OpencodePermissionReply, ExecutorApprovalError> {
    if auto_approve {
        return Ok(OpencodePermissionReply::Once);
    }

    let Some(approvals) = approvals else {
        return Ok(OpencodePermissionReply::Once);
    };

    // Reuse the option-preserving approval bridge so `always` is not collapsed into
    // the same binary approved status as `once`.
    match approvals
        .request_acp_tool_approval(
            ExecutorApprovalRequest {
                tool_name: tool_name.to_string(),
                tool_input,
                tool_call_id: tool_call_id.to_string(),
                options: vec![
                    ExecutorApprovalOption {
                        option_id: "once".to_string(),
                        kind: "allow_once".to_string(),
                        label: "Allow once".to_string(),
                    },
                    ExecutorApprovalOption {
                        option_id: "always".to_string(),
                        kind: "allow_always".to_string(),
                        label: "Always allow".to_string(),
                    },
                    ExecutorApprovalOption {
                        option_id: "reject".to_string(),
                        kind: "reject_once".to_string(),
                        label: "Reject".to_string(),
                    },
                ],
            },
            cancel,
        )
        .await
    {
        Ok(option_id) => OpencodePermissionReply::from_option_id(&option_id),
        Err(
            ExecutorApprovalError::ServiceUnavailable | ExecutorApprovalError::SessionNotRegistered,
        ) => Ok(OpencodePermissionReply::Once),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use std::{future, io, sync::Arc, time::Duration};

    use serde_json::json;
    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;

    use super::{
        ControlEvent, OpencodePermissionReply, ProjectedSessionGraph, extract_retry_status,
        request_permission_approval, run_request_with_control, run_request_with_control_timeout,
    };
    use crate::{
        approvals::{ExecutorApprovalError, ExecutorApprovalRequest, ExecutorApprovalService},
        executors::ExecutorError,
    };

    struct CapturingApprovalService {
        selected_option_id: &'static str,
        request: Mutex<Option<ExecutorApprovalRequest>>,
    }

    #[async_trait::async_trait]
    impl ExecutorApprovalService for CapturingApprovalService {
        async fn request_tool_approval(
            &self,
            _tool_name: &str,
            _tool_input: serde_json::Value,
            _tool_call_id: &str,
            _cancel: CancellationToken,
        ) -> Result<workspace_utils::approvals::ApprovalStatus, ExecutorApprovalError> {
            unreachable!("OpenCode preserves the native permission options")
        }

        async fn request_acp_tool_approval(
            &self,
            request: ExecutorApprovalRequest,
            _cancel: CancellationToken,
        ) -> Result<String, ExecutorApprovalError> {
            *self.request.lock().await = Some(request);
            Ok(self.selected_option_id.to_string())
        }
    }

    #[tokio::test]
    async fn opencode_permission_approval_preserves_always_option() {
        let approvals = Arc::new(CapturingApprovalService {
            selected_option_id: "always",
            request: Mutex::new(None),
        });

        let reply = request_permission_approval(
            false,
            Some(approvals.clone()),
            "bash",
            json!({ "patterns": ["cargo test"] }),
            "tool-call",
            CancellationToken::new(),
        )
        .await
        .expect("resolve OpenCode permission");

        assert_eq!(reply, OpencodePermissionReply::Always);
        assert_eq!(reply.as_str(), "always");
        let request = approvals
            .request
            .lock()
            .await
            .take()
            .expect("captured approval request");
        assert_eq!(request.tool_name, "bash");
        assert_eq!(request.tool_call_id, "tool-call");
        assert_eq!(
            request
                .options
                .iter()
                .map(|option| (option.option_id.as_str(), option.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("once", "allow_once"),
                ("always", "allow_always"),
                ("reject", "reject_once"),
            ]
        );
    }

    #[test]
    fn extract_retry_status_parses_retry_payload() {
        let payload = json!({
            "type": "session.status",
            "properties": {
                "status": {
                    "type": "retry",
                    "attempt": 7,
                    "message": "Too Many Requests"
                }
            }
        });

        let parsed = extract_retry_status(&payload).expect("retry status");
        assert_eq!(parsed.0, 7);
        assert_eq!(parsed.1, "Too Many Requests");
    }

    #[test]
    fn extract_retry_status_ignores_non_retry_payload() {
        let payload = json!({
            "type": "session.status",
            "properties": {
                "status": {
                    "type": "busy"
                }
            }
        });

        assert!(extract_retry_status(&payload).is_none());
    }

    #[test]
    fn child_terminal_events_do_not_match_the_root_session() {
        let sessions = ProjectedSessionGraph::new("main-session".to_string());
        for event_type in ["session.idle", "session.error", "session.status"] {
            let active = json!({
                "type": event_type,
                "properties": { "sessionID": "main-session" }
            });
            let child = json!({
                "type": event_type,
                "properties": { "sessionID": "child-session" }
            });

            assert!(sessions.is_root_event(event_type, &active));
            assert!(!sessions.is_root_event(event_type, &child));
        }
    }

    #[test]
    fn interleaved_child_activity_keeps_stable_session_metadata() {
        let mut sessions = ProjectedSessionGraph::new("main-session".to_string());
        for (id, title) in [("child-a", "Explore API"), ("child-b", "Inspect tests")] {
            sessions.observe_session_event(
                "session.created",
                &json!({
                    "type": "session.created",
                    "properties": {
                        "info": {
                            "id": id,
                            "parentID": "main-session",
                            "title": title
                        }
                    }
                }),
            );
        }

        let events = [
            json!({ "properties": { "part": { "sessionID": "child-a" } } }),
            json!({ "properties": { "part": { "sessionID": "child-b" } } }),
            json!({ "properties": { "part": { "sessionID": "child-a" } } }),
        ];
        let projected = events
            .iter()
            .map(|event| {
                sessions
                    .metadata_for_event("message.part.updated", event)
                    .expect("child activity should project")
            })
            .collect::<Vec<_>>();

        assert_eq!(projected[0].session_id, "child-a");
        assert_eq!(projected[0].title.as_deref(), Some("Explore API"));
        assert_eq!(projected[1].session_id, "child-b");
        assert_eq!(projected[1].title.as_deref(), Some("Inspect tests"));
        assert_eq!(projected[2], projected[0]);
        assert!(
            projected
                .iter()
                .all(|session| session.parent_session_id.as_deref() == Some("main-session"))
        );
    }

    #[test]
    fn foreign_run_activity_is_not_projected() {
        let mut sessions = ProjectedSessionGraph::new("main-session".to_string());
        sessions.observe_session_event(
            "session.created",
            &json!({
                "type": "session.created",
                "properties": {
                    "info": {
                        "id": "foreign-child",
                        "parentID": "foreign-root",
                        "title": "Other run"
                    }
                }
            }),
        );
        let event = json!({
            "properties": {
                "part": { "sessionID": "foreign-child" }
            }
        });

        assert!(
            sessions
                .metadata_for_event("message.part.updated", &event)
                .is_none()
        );
    }

    #[test]
    fn permission_events_from_descendant_sessions_are_projected() {
        let mut sessions = ProjectedSessionGraph::new("main-session".to_string());
        sessions.observe_session_event(
            "session.created",
            &json!({
                "type": "session.created",
                "properties": {
                    "info": {
                        "id": "child-session",
                        "parentID": "main-session",
                        "title": "Delegated task"
                    }
                }
            }),
        );
        let asked = json!({
            "properties": {
                "sessionID": "child-session"
            }
        });

        let metadata = sessions
            .metadata_for_event("permission.asked", &asked)
            .expect("child approval should keep projection metadata");
        assert_eq!(metadata.session_id, "child-session");
        assert_eq!(metadata.parent_session_id.as_deref(), Some("main-session"));
    }

    #[tokio::test]
    async fn run_request_with_control_fails_immediately_on_session_error() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(ControlEvent::SessionError {
            message: "retry limit reached".to_string(),
        })
        .expect("send control event");

        let result = run_request_with_control(
            future::pending::<Result<(), ExecutorError>>(),
            &mut rx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        let err = result.expect_err("session error should fail request");
        let ExecutorError::Io(err) = err else {
            panic!("expected io error");
        };
        assert!(err.to_string().contains("retry limit reached"));
    }

    #[tokio::test]
    async fn run_request_with_control_returns_on_idle_even_if_request_is_still_pending() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(ControlEvent::Idle).expect("send control event");

        let result = run_request_with_control(
            future::pending::<Result<(), ExecutorError>>(),
            &mut rx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(result.is_ok(), "idle should end the request wait");
    }

    #[tokio::test]
    async fn run_request_with_control_prefers_session_error_after_http_failure() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            tx.send(ControlEvent::SessionError {
                message:
                    "Model not found: volcengine-plan/ark-code-latest. Did you mean: kimi-k2.7-code?"
                        .to_string(),
            })
            .expect("send session error");
        });

        let result = run_request_with_control(
            future::ready(Err::<(), ExecutorError>(ExecutorError::Io(
                io::Error::other(
                    "OpenCode session.prompt failed: HTTP 500 Unexpected server error",
                ),
            ))),
            &mut rx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        let err = result.expect_err("session error should replace generic HTTP error");
        assert!(err.to_string().contains("Model not found"));
    }

    #[tokio::test]
    async fn run_request_with_control_times_out_when_activity_stops() {
        let (_tx, mut rx) = mpsc::unbounded_channel();

        let result = run_request_with_control_timeout(
            &mut future::pending::<Result<(), ExecutorError>>(),
            &mut rx,
            tokio_util::sync::CancellationToken::new(),
            Duration::from_millis(25),
        )
        .await;

        let err = result.expect_err("missing activity should fail request");
        assert!(err.to_string().contains("without session activity"));
    }

    #[tokio::test]
    async fn run_request_with_control_resets_timeout_on_activity() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            tx.send(ControlEvent::Activity).expect("send activity");
            tokio::time::sleep(Duration::from_millis(15)).await;
            tx.send(ControlEvent::Idle).expect("send idle");
        });

        let result = run_request_with_control_timeout(
            &mut future::pending::<Result<(), ExecutorError>>(),
            &mut rx,
            tokio_util::sync::CancellationToken::new(),
            Duration::from_millis(40),
        )
        .await;

        assert!(result.is_ok(), "activity should extend the wait until idle");
    }
}
