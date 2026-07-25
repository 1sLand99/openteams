#[tokio::main]
async fn main() -> agent_client_protocol::Result<()> {
    executors::executors::acp::qa_agent::run_stdio_agent().await
}
