use std::{
    collections::{BTreeSet, HashSet},
    path::Path,
};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::executors::ExecutorError;

/// Runtime restrictions applied to canonical MCP definitions before they are
/// converted into ACP session parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcpMcpPolicy {
    /// `None` allows every configured server. `Some(empty)` allows none.
    pub allowed_server_names: Option<BTreeSet<String>>,
    pub disabled_server_names: BTreeSet<String>,
}

/// Secret-safe result of resolving canonical MCP definitions for one run.
#[derive(Debug, Clone)]
pub struct EffectiveAcpMcpConfig {
    pub servers: Vec<McpServer>,
    pub config_hash: String,
}

/// Validate a complete ACP MCP list against the negotiated Agent capabilities.
pub fn validate_mcp_servers(
    servers: &[McpServer],
    capabilities: &AgentCapabilities,
) -> Result<(), String> {
    let mut names = HashSet::new();
    for server in servers {
        let (name, transport_supported) = match server {
            McpServer::Stdio(server) => (server.name.as_str(), true),
            McpServer::Http(server) => (server.name.as_str(), capabilities.mcp_capabilities.http),
            McpServer::Sse(server) => (server.name.as_str(), capabilities.mcp_capabilities.sse),
            _ => return Err("unsupported ACP MCP transport".to_string()),
        };
        if name.trim().is_empty() {
            return Err("ACP MCP server name must not be empty".to_string());
        }
        if !names.insert(name) {
            return Err(format!("duplicate ACP MCP server name: {name}"));
        }
        if !transport_supported {
            return Err(format!(
                "ACP Agent does not support the configured MCP transport for {name}"
            ));
        }
    }
    Ok(())
}

/// Load the complete MCP list that OpenTeams will pass on ACP session requests.
pub fn resolve_effective_mcp_config(
    canonical_config: &Value,
    policy: &AcpMcpPolicy,
) -> Result<EffectiveAcpMcpConfig, ExecutorError> {
    let effective_value = filter_canonical_servers(canonical_config, policy)?;
    let effective_servers = effective_value
        .get("mcpServers")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let config_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&effective_servers)?)
    );
    let servers = parse_mcp_servers(&effective_value)?;
    Ok(EffectiveAcpMcpConfig {
        servers,
        config_hash,
    })
}

/// Create a per-run system settings file that prevents the Agent from also
/// loading its vendor-global MCP list after OpenTeams injects the ACP list.
pub async fn write_mcp_isolation_settings(
    current_dir: &Path,
    prefix: &str,
) -> Result<std::path::PathBuf, ExecutorError> {
    let directory = current_dir.join(".openteams").join("tmp");
    tokio::fs::create_dir_all(&directory)
        .await
        .map_err(ExecutorError::Io)?;
    let path = directory.join(format!("{prefix}-{}.json", uuid::Uuid::new_v4()));
    let body = serde_json::to_vec_pretty(&serde_json::json!({ "mcpServers": {} }))?;
    tokio::fs::write(&path, body)
        .await
        .map_err(ExecutorError::Io)?;
    Ok(path)
}

fn parse_mcp_servers(value: &Value) -> Result<Vec<McpServer>, ExecutorError> {
    let Some(servers) = value.get("mcpServers").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut result = Vec::with_capacity(servers.len());
    for (name, value) in servers {
        let Some(server) = value.as_object() else {
            return Err(invalid_mcp_config(
                name,
                "server definition must be an object",
            ));
        };
        if server.get("disabled").and_then(Value::as_bool) == Some(true)
            || server.get("enabled").and_then(Value::as_bool) == Some(false)
        {
            continue;
        }
        let headers = parse_headers(server.get("headers"), name)?;
        if let Some(url) = server
            .get("httpUrl")
            .or_else(|| server.get("url"))
            .and_then(Value::as_str)
        {
            reqwest::Url::parse(url)
                .map_err(|_| invalid_mcp_config(name, "server URL is invalid"))?;
            let transport = server.get("type").and_then(Value::as_str);
            if transport == Some("sse") {
                result.push(McpServer::Sse(
                    McpServerSse::new(name, url).headers(headers),
                ));
            } else {
                result.push(McpServer::Http(
                    McpServerHttp::new(name, url).headers(headers),
                ));
            }
            continue;
        }

        let Some(command) = server.get("command").and_then(Value::as_str) else {
            return Err(invalid_mcp_config(name, "stdio server command is missing"));
        };
        let command = which::which(command)
            .map_err(|_| invalid_mcp_config(name, "stdio server command was not found"))?;
        let args = parse_string_array(server.get("args"), name, "args")?;
        let env = parse_env(server.get("env"), name)?;
        result.push(McpServer::Stdio(
            McpServerStdio::new(name, command).args(args).env(env),
        ));
    }
    Ok(result)
}

fn filter_canonical_servers(value: &Value, policy: &AcpMcpPolicy) -> Result<Value, ExecutorError> {
    let Some(root) = value.as_object() else {
        return Err(ExecutorError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid MCP config: root must be an object",
        )));
    };
    let mut effective_root = root.clone();
    let Some(servers) = root.get("mcpServers").and_then(Value::as_object) else {
        effective_root.insert("mcpServers".to_string(), Value::Object(Default::default()));
        return Ok(Value::Object(effective_root));
    };
    let effective_servers = servers
        .iter()
        .filter(|(name, _)| {
            !policy.disabled_server_names.contains(*name)
                && policy
                    .allowed_server_names
                    .as_ref()
                    .is_none_or(|allowed| allowed.contains(*name))
        })
        .map(|(name, server)| (name.clone(), server.clone()))
        .collect();
    effective_root.insert("mcpServers".to_string(), Value::Object(effective_servers));
    Ok(Value::Object(effective_root))
}

fn parse_headers(value: Option<&Value>, name: &str) -> Result<Vec<HttpHeader>, ExecutorError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(headers) = value.as_object() else {
        return Err(invalid_mcp_config(name, "headers must be an object"));
    };
    headers
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| HttpHeader::new(key, value))
                .ok_or_else(|| invalid_mcp_config(name, "header values must be strings"))
        })
        .collect()
}

fn parse_env(value: Option<&Value>, name: &str) -> Result<Vec<EnvVariable>, ExecutorError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(env) = value.as_object() else {
        return Err(invalid_mcp_config(name, "env must be an object"));
    };
    env.iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| EnvVariable::new(key, value))
                .ok_or_else(|| invalid_mcp_config(name, "env values must be strings"))
        })
        .collect()
}

fn parse_string_array(
    value: Option<&Value>,
    name: &str,
    field: &str,
) -> Result<Vec<String>, ExecutorError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(invalid_mcp_config(
            name,
            &format!("{field} must be an array"),
        ));
    };
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid_mcp_config(name, &format!("{field} values must be strings")))
        })
        .collect()
}

fn invalid_mcp_config(name: &str, message: &str) -> ExecutorError {
    ExecutorError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("invalid MCP server `{name}`: {message}"),
    ))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        AgentCapabilities, McpCapabilities, McpServerHttp, McpServerStdio,
    };

    use super::*;

    #[test]
    fn rejects_duplicate_server_names_across_transports() {
        let servers = vec![
            McpServer::Stdio(McpServerStdio::new("tools", "/bin/true")),
            McpServer::Http(McpServerHttp::new("tools", "https://example.test/mcp")),
        ];
        let capabilities =
            AgentCapabilities::new().mcp_capabilities(McpCapabilities::new().http(true));
        assert!(validate_mcp_servers(&servers, &capabilities).is_err());
    }

    #[test]
    fn rejects_http_when_agent_did_not_advertise_it() {
        let servers = vec![McpServer::Http(McpServerHttp::new(
            "tools",
            "https://example.test/mcp",
        ))];
        assert!(validate_mcp_servers(&servers, &AgentCapabilities::new()).is_err());
    }

    #[test]
    fn stdio_is_always_supported() {
        let servers = vec![McpServer::Stdio(McpServerStdio::new("tools", "/bin/true"))];
        assert!(validate_mcp_servers(&servers, &AgentCapabilities::new()).is_ok());
    }

    #[test]
    fn parses_enabled_stdio_and_http_servers() {
        let value = serde_json::json!({
            "mcpServers": {
                "local": {
                    "command": "/bin/echo",
                    "args": ["hello"],
                    "env": {"VISIBLE": "value"}
                },
                "remote": {
                    "httpUrl": "https://example.test/mcp",
                    "headers": {"Authorization": "secret"}
                },
                "disabled": {
                    "command": "/bin/echo",
                    "disabled": true
                }
            }
        });
        let servers = parse_mcp_servers(&value).expect("MCP servers");
        assert_eq!(servers.len(), 2);
        assert!(matches!(servers[0], McpServer::Stdio(_)));
        assert!(matches!(servers[1], McpServer::Http(_)));
    }

    #[test]
    fn effective_policy_filters_servers_before_validation() {
        let value = serde_json::json!({
            "mcpServers": {
                "allowed": {"command": "/bin/echo"},
                "revoked": {"command": "definitely-not-installed"}
            }
        });
        let policy = AcpMcpPolicy {
            allowed_server_names: Some(["allowed".to_string()].into_iter().collect()),
            disabled_server_names: Default::default(),
        };
        let effective = filter_canonical_servers(&value, &policy).expect("effective config");
        let servers = parse_mcp_servers(&effective).expect("allowed server");
        assert_eq!(servers.len(), 1);
        assert!(matches!(&servers[0], McpServer::Stdio(server) if server.name == "allowed"));
    }

    #[test]
    fn explicit_empty_allowlist_revokes_all_servers() {
        let value = serde_json::json!({
            "mcpServers": {
                "server": {"command": "/bin/echo"}
            }
        });
        let policy = AcpMcpPolicy {
            allowed_server_names: Some(Default::default()),
            disabled_server_names: Default::default(),
        };
        let effective = filter_canonical_servers(&value, &policy).expect("effective config");
        assert!(
            effective["mcpServers"]
                .as_object()
                .expect("server map")
                .is_empty()
        );
    }
}
