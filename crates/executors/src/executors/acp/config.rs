use std::path::PathBuf;

use agent_client_protocol::schema::v1::{McpServer, SessionConfigOptionValue};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpConfigValue {
    ValueId { value: String },
    Boolean { value: bool },
}

impl AcpConfigValue {
    pub fn to_protocol(&self) -> SessionConfigOptionValue {
        match self {
            Self::ValueId { value } => SessionConfigOptionValue::value_id(value.clone()),
            Self::Boolean { value } => SessionConfigOptionValue::boolean(*value),
        }
    }

    pub fn from_protocol(value: &SessionConfigOptionValue) -> Self {
        match value {
            SessionConfigOptionValue::ValueId { value } => Self::ValueId {
                value: value.0.to_string(),
            },
            SessionConfigOptionValue::Boolean { value } => Self::Boolean { value: *value },
            _ => unreachable!("unsupported ACP config value variant"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
pub struct AcpConfigOverride {
    pub option_id: String,
    pub value: AcpConfigValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_snapshot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category_snapshot: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct AcpConfigChoice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpConfigOptionKind {
    Select {
        current_value: String,
        options: Vec<AcpConfigChoice>,
    },
    Boolean {
        current_value: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct AcpConfigOptionSnapshot {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    #[serde(flatten)]
    pub kind: AcpConfigOptionKind,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "snake_case")]
#[ts(use_ts_enum)]
pub enum AcpConfigSource {
    #[default]
    None,
    Stable,
    LegacyModel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct AcpAuthMethodInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
pub struct AcpCapabilityProbe {
    pub protocol_version: String,
    pub agent_name: Option<String>,
    pub agent_version: Option<String>,
    pub auth_methods: Vec<AcpAuthMethodInfo>,
    pub supports_session_list: bool,
    pub supports_session_resume: bool,
    pub supports_session_close: bool,
    pub supports_session_delete: bool,
    pub supports_additional_directories: bool,
    #[ts(type = "JsonValue")]
    pub agent_capabilities: serde_json::Value,
    pub config_source: AcpConfigSource,
    pub config_options: Vec<AcpConfigOptionSnapshot>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[ts(use_ts_enum)]
pub enum AcpAccessMode {
    WorkspaceOnly,
    #[default]
    FullAccess,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[ts(use_ts_enum)]
pub enum AcpApprovalMode {
    #[default]
    Ask,
    AutoAllow,
    AutoReject,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AcpAuthSelection {
    #[default]
    Auto,
    MethodId {
        method_id: String,
    },
}

/// Partial ACP settings. `None` means inherit from the lower-priority layer.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS, JsonSchema)]
pub struct AcpExecutionOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_mode: Option<AcpAccessMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<AcpApprovalMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AcpAuthSelection>,
    /// `Some` replaces lower-priority directories, including with an empty list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_directories: Option<Vec<String>>,
    /// Exact option IDs and typed values advertised by the ACP Agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_overrides: Option<Vec<AcpConfigOverride>>,
}

impl AcpExecutionOptions {
    pub fn overlay(&self, higher_priority: &Self) -> Self {
        Self {
            access_mode: higher_priority.access_mode.or(self.access_mode),
            approval_mode: higher_priority.approval_mode.or(self.approval_mode),
            auth: higher_priority.auth.clone().or_else(|| self.auth.clone()),
            additional_directories: higher_priority
                .additional_directories
                .clone()
                .or_else(|| self.additional_directories.clone()),
            config_overrides: higher_priority
                .config_overrides
                .clone()
                .or_else(|| self.config_overrides.clone()),
        }
    }

    pub async fn validated_directories(&self) -> Result<Vec<PathBuf>, std::io::Error> {
        let mut resolved = Vec::new();
        for raw in self.additional_directories.clone().unwrap_or_default() {
            let path = PathBuf::from(raw);
            if !path.is_absolute() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "ACP additional directory must be absolute: {}",
                        path.display()
                    ),
                ));
            }
            let canonical = tokio::fs::canonicalize(&path).await.map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!(
                        "invalid ACP additional directory `{}`: {error}",
                        path.display()
                    ),
                )
            })?;
            if !canonical.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "ACP additional directory is not a directory: {}",
                        canonical.display()
                    ),
                ));
            }
            if !resolved.contains(&canonical) {
                resolved.push(canonical);
            }
        }
        Ok(resolved)
    }
}

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
    /// Allow ACP Client file and terminal services to operate outside the
    /// configured workspace roots.
    pub full_access: bool,
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
            full_access: false,
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
    pub auth_method_id: Option<String>,
    pub session: AcpSessionPreferences,
    pub client_services: AcpClientServicePolicy,
    pub additional_directories: Vec<PathBuf>,
    pub mcp_servers: Vec<McpServer>,
}

impl Default for AcpRunConfig {
    fn default() -> Self {
        Self {
            approval_policy: AcpApprovalPolicy::Ask,
            auth_method_id: None,
            session: AcpSessionPreferences::default(),
            client_services: AcpClientServicePolicy::default(),
            additional_directories: Vec::new(),
            mcp_servers: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_overlay_replaces_directories_without_expanding_them() {
        let global = AcpExecutionOptions {
            access_mode: Some(AcpAccessMode::WorkspaceOnly),
            approval_mode: Some(AcpApprovalMode::Ask),
            auth: Some(AcpAuthSelection::Auto),
            additional_directories: Some(vec!["/global".to_string()]),
            config_overrides: Some(vec![AcpConfigOverride {
                option_id: "model".to_string(),
                value: AcpConfigValue::ValueId {
                    value: "global-model".to_string(),
                },
                label_snapshot: None,
                category_snapshot: Some("model".to_string()),
            }]),
        };
        let member = AcpExecutionOptions {
            approval_mode: Some(AcpApprovalMode::AutoReject),
            additional_directories: Some(Vec::new()),
            ..Default::default()
        };

        let effective = global.overlay(&member);
        assert_eq!(effective.access_mode, Some(AcpAccessMode::WorkspaceOnly));
        assert_eq!(effective.approval_mode, Some(AcpApprovalMode::AutoReject));
        assert_eq!(effective.additional_directories, Some(Vec::new()));
        assert_eq!(
            effective.config_overrides, global.config_overrides,
            "unset member config must inherit exact ACP selections"
        );
    }

    #[tokio::test]
    async fn additional_directories_reject_relative_paths() {
        let options = AcpExecutionOptions {
            additional_directories: Some(vec!["relative/path".to_string()]),
            ..Default::default()
        };

        let error = options
            .validated_directories()
            .await
            .expect_err("relative ACP directory must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("must be absolute"));
    }
}
