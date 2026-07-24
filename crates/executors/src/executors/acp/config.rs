use std::path::PathBuf;

use agent_client_protocol::schema::v1::{McpServer, SessionConfigOptionValue};

/// How OpenTeams resolves ACP permission requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AcpApprovalPolicy {
    /// Ask through the configured OpenTeams approval service.
    #[default]
    Ask,
    /// Select an explicit ACP allow option without prompting.
    AutoAllow,
    /// Select an explicit ACP reject option without prompting.
    AutoReject,
}

/// A requested ACP session configuration value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpConfigSelection {
    pub option_id: String,
    pub value: SessionConfigOptionValue,
}

/// Preferences applied through stable `session/set_config_option`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcpSessionPreferences {
    pub model: Option<String>,
    pub thought_level: Option<String>,
    pub mode: Option<String>,
    pub options: Vec<AcpConfigSelection>,
}

/// Client services that an ACP Agent may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpClientServicePolicy {
    pub read_text_file: bool,
    pub write_text_file: bool,
    pub terminal: bool,
    pub max_file_bytes: usize,
    pub max_terminals: usize,
    pub max_terminal_output_bytes: usize,
}

impl Default for AcpClientServicePolicy {
    fn default() -> Self {
        Self {
            read_text_file: false,
            write_text_file: false,
            terminal: false,
            max_file_bytes: 1024 * 1024,
            max_terminals: 4,
            max_terminal_output_bytes: 1024 * 1024,
        }
    }
}

/// Per-run inputs for the generic ACP runtime.
#[derive(Debug, Clone)]
pub struct AcpRunConfig {
    pub approval_policy: AcpApprovalPolicy,
    pub session: AcpSessionPreferences,
    pub client_services: AcpClientServicePolicy,
    pub additional_directories: Vec<PathBuf>,
    pub mcp_servers: Vec<McpServer>,
}

impl Default for AcpRunConfig {
    fn default() -> Self {
        Self {
            approval_policy: AcpApprovalPolicy::Ask,
            session: AcpSessionPreferences::default(),
            client_services: AcpClientServicePolicy::default(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
        }
    }
}
