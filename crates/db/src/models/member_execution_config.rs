use executors::{
    executors::{BaseCodingAgent, acp::AcpExecutionOptions},
    mcp_config::MemberMcpConfig,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct MemberExecutionConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_type: Option<BaseCodingAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_variant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpExecutionOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<MemberMcpConfig>,
}

impl MemberExecutionConfig {
    pub fn has_overrides(&self) -> bool {
        self.runner_type.is_some()
            || is_present(self.model_name.as_deref())
            || is_present(self.thinking_effort.as_deref())
            || is_present(self.model_variant.as_deref())
            || self.acp.is_some()
    }

    pub fn normalized(mut self) -> Self {
        self.model_name = normalize_optional_string(self.model_name);
        self.thinking_effort = normalize_optional_string(self.thinking_effort);
        self.model_variant = normalize_optional_string(self.model_variant);
        self
    }
}

fn is_present(value: Option<&str>) -> bool {
    value.map(str::trim).is_some_and(|value| !value.is_empty())
}

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use executors::mcp_config::MemberMcpConfig;

    use super::MemberExecutionConfig;

    #[test]
    fn mcp_state_does_not_count_as_runner_or_profile_override() {
        let config = MemberExecutionConfig {
            mcp: Some(MemberMcpConfig::default()),
            ..Default::default()
        };

        assert!(!config.has_overrides());
    }

    #[test]
    fn legacy_none_and_explicit_empty_mcp_serialize_distinctly() {
        let legacy = serde_json::to_value(MemberExecutionConfig::default())
            .expect("serialize legacy member config");
        let explicit_empty = serde_json::to_value(MemberExecutionConfig {
            mcp: Some(MemberMcpConfig::default()),
            ..Default::default()
        })
        .expect("serialize explicit empty MCP config");

        assert!(legacy.get("mcp").is_none());
        assert_eq!(explicit_empty["mcp"], serde_json::json!({"mcpServers": {}}));
    }
}
