use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::StreamReader;

use super::{AcpEvent, events::AcpRuntimeEvent};
use crate::{env::SensitiveValueRedactor, executors::ExecutorOutput};

const DEFAULT_OUTPUT_CAPACITY: usize = 256;

/// Ordered bridge from ACP callbacks to the executor stdout contract.
#[derive(Clone)]
pub struct AcpOutput {
    state: Arc<Mutex<OutputState>>,
    redactor: SensitiveValueRedactor,
}

struct OutputState {
    tx: mpsc::Sender<AcpRuntimeEvent>,
    connection_id: String,
    session_id: Option<String>,
    next_sequence: u64,
}

impl AcpOutput {
    pub fn start<W>(
        writer: W,
        redactor: SensitiveValueRedactor,
    ) -> (Self, JoinHandle<std::io::Result<()>>)
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<AcpRuntimeEvent>(DEFAULT_OUTPUT_CAPACITY);
        let writer = Arc::new(Mutex::new(writer));
        let task = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let mut line = serde_json::to_vec(&event)
                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                line.push(b'\n');
                let mut writer = writer.lock().await;
                writer.write_all(&line).await?;
            }
            writer.lock().await.flush().await
        });
        (
            Self {
                state: Arc::new(Mutex::new(OutputState {
                    tx,
                    connection_id: uuid::Uuid::new_v4().to_string(),
                    session_id: None,
                    next_sequence: 0,
                })),
                redactor,
            },
            task,
        )
    }

    /// Create a bounded synthetic stdout stream. Replay notifications are filtered before they
    /// reach this bridge, so the small handshake can complete before the caller attaches its log
    /// forwarder while current-turn output still receives normal backpressure.
    pub fn channel(redactor: SensitiveValueRedactor) -> (Self, ExecutorOutput) {
        let (tx, rx) = mpsc::channel::<AcpRuntimeEvent>(DEFAULT_OUTPUT_CAPACITY);
        let stream = ReceiverStream::new(rx).map(|event| {
            let mut line = serde_json::to_vec(&event)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            line.push(b'\n');
            Ok::<_, std::io::Error>(Bytes::from(line))
        });
        let reader = StreamReader::new(stream);
        (
            Self {
                state: Arc::new(Mutex::new(OutputState {
                    tx,
                    connection_id: uuid::Uuid::new_v4().to_string(),
                    session_id: None,
                    next_sequence: 0,
                })),
                redactor,
            },
            ExecutorOutput::new(reader),
        )
    }

    pub async fn send(&self, event: AcpEvent) -> Result<(), AcpEvent> {
        let event = redact_event(event, &self.redactor);
        let mut state = self.state.lock().await;
        let explicit_session_id = match &event {
            AcpEvent::SessionStart(session_id) => Some(session_id.clone()),
            AcpEvent::RequestPermission(request) => Some(request.session_id.0.to_string()),
            AcpEvent::Other(notification) => Some(notification.session_id.0.to_string()),
            _ => None,
        };
        if explicit_session_id.is_some() {
            state.session_id.clone_from(&explicit_session_id);
        }
        let tool_call_id = match &event {
            AcpEvent::ToolCall(tool_call) => Some(tool_call.tool_call_id.0.to_string()),
            AcpEvent::ToolUpdate(update) => Some(update.tool_call_id.0.to_string()),
            AcpEvent::RequestPermission(request) => {
                Some(request.tool_call.tool_call_id.0.to_string())
            }
            AcpEvent::ApprovalResponse(response) => Some(response.tool_call_id.clone()),
            _ => None,
        };
        let message_id = match &event {
            AcpEvent::UserBlock(chunk) | AcpEvent::Message(chunk) | AcpEvent::Thought(chunk) => {
                chunk
                    .message_id
                    .as_ref()
                    .map(|message_id| message_id.0.to_string())
            }
            _ => None,
        };
        let runtime_event = AcpRuntimeEvent {
            connection_id: state.connection_id.clone(),
            session_id: state.session_id.clone(),
            sequence: state.next_sequence,
            message_id,
            tool_call_id,
            payload: event,
        };
        state.next_sequence = state.next_sequence.saturating_add(1);
        state
            .tx
            .send(runtime_event)
            .await
            .map_err(|error| error.0.payload)
    }
}

fn redact_event(event: AcpEvent, redactor: &SensitiveValueRedactor) -> AcpEvent {
    if redactor.is_empty() {
        return event;
    }
    let Ok(mut value) = serde_json::to_value(event) else {
        return AcpEvent::Error("ACP output event redaction failed".to_string());
    };
    redactor.redact_json(&mut value);
    serde_json::from_value(value)
        .unwrap_or_else(|_| AcpEvent::Error("ACP output event redaction failed".to_string()))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};
    use tokio::io::AsyncReadExt;

    use super::*;

    #[tokio::test]
    async fn drain_flushes_tail_event_with_monotonic_sequence() {
        let (writer, mut reader) = tokio::io::duplex(4096);
        let (output, task) = AcpOutput::start(writer, SensitiveValueRedactor::default());
        output
            .send(AcpEvent::SessionStart("session".to_string()))
            .await
            .expect("session event");
        output
            .send(AcpEvent::Message(
                ContentChunk::new(ContentBlock::Text(TextContent::new("message")))
                    .message_id("message-id"),
            ))
            .await
            .expect("message event");
        output
            .send(AcpEvent::Done("\"end_turn\"".to_string()))
            .await
            .expect("done event");
        drop(output);
        task.await.expect("output task").expect("output flush");

        let mut body = String::new();
        reader.read_to_string(&mut body).await.expect("read output");
        let events = body
            .lines()
            .map(|line| serde_json::from_str::<AcpRuntimeEvent>(line).expect("runtime event"))
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].sequence, 0);
        assert_eq!(events[1].sequence, 1);
        assert_eq!(events[1].message_id.as_deref(), Some("message-id"));
        assert_eq!(events[2].sequence, 2);
        assert_eq!(events[2].session_id.as_deref(), Some("session"));
        assert!(matches!(events[2].payload, AcpEvent::Done(_)));
    }

    #[tokio::test]
    async fn short_explicit_secret_is_redacted_without_corrupting_event_shape() {
        let (writer, mut reader) = tokio::io::duplex(4096);
        let redactor = SensitiveValueRedactor::default().with_sensitive_values(["a"]);
        let (output, task) = AcpOutput::start(writer, redactor);
        output
            .send(AcpEvent::Message(ContentChunk::new(ContentBlock::Text(
                TextContent::new("data=a"),
            ))))
            .await
            .expect("message event");
        drop(output);
        task.await.expect("output task").expect("output flush");

        let mut body = String::new();
        reader.read_to_string(&mut body).await.expect("read output");
        let event = serde_json::from_str::<AcpRuntimeEvent>(body.trim()).expect("runtime event");

        assert!(matches!(event.payload, AcpEvent::Message(_)), "{body}");
        assert!(body.contains("[redacted]"), "{body}");
    }
}
