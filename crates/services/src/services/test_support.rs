use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use executors::mcp_config::MemberMcpConfig;
use tracing_subscriber::fmt::MakeWriter;

pub(crate) const CANONICAL_MCP_SECRET: &str = "MCP_CANONICAL_SECRET_NEVER_EXPOSE_5E71";
pub(crate) const ADAPTER_DIAGNOSTIC_SECRET: &str =
    "MCP_ADAPTER_DIAGNOSTIC_SECRET_NEVER_EXPOSE_9A42";

pub(crate) fn canonical_mcp_config_with_fake_secrets() -> MemberMcpConfig {
    MemberMcpConfig {
        mcp_servers: [
            (
                "local-safe-name".to_string(),
                serde_json::json!({
                    "command": format!("/tmp/{CANONICAL_MCP_SECRET}"),
                    "args": [CANONICAL_MCP_SECRET],
                    "env": {"TOKEN": CANONICAL_MCP_SECRET}
                }),
            ),
            (
                "remote-safe-name".to_string(),
                serde_json::json!({
                    "url": "https://example.test/mcp",
                    "headers": {"Authorization": CANONICAL_MCP_SECRET}
                }),
            ),
        ]
        .into_iter()
        .collect(),
    }
}

#[derive(Clone, Default)]
struct CaptureWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

struct CaptureGuardWriter {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl Write for CaptureGuardWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes
            .lock()
            .expect("tracing capture buffer lock")
            .write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> MakeWriter<'writer> for CaptureWriter {
    type Writer = CaptureGuardWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        CaptureGuardWriter {
            bytes: self.bytes.clone(),
        }
    }
}

pub(crate) struct TracingCapture {
    writer: CaptureWriter,
    _subscriber: tracing::subscriber::DefaultGuard,
}

impl TracingCapture {
    pub(crate) fn start() -> Self {
        let writer = CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(writer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        Self {
            writer,
            _subscriber: guard,
        }
    }

    pub(crate) fn finish(self) -> String {
        String::from_utf8(
            self.writer
                .bytes
                .lock()
                .expect("tracing capture buffer lock")
                .clone(),
        )
        .expect("tracing output is UTF-8")
    }
}
