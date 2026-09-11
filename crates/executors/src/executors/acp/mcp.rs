use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    path::PathBuf,
};

use agent_client_protocol::schema::v1::{
    AgentCapabilities, EnvVariable, HttpHeader, McpServer, McpServerHttp, McpServerSse,
    McpServerStdio,
};
use serde_json::Value;

use crate::{
    command::CmdOverrides,
    env::ExecutionEnv,
    executors::{ExecutorError, ExecutorRunCleanup},
    mcp_config::MemberMcpConfig,
    mcp_run::{
        McpRunContext, PreparedMcpRun, PrivateMcpRunDirectory, canonical_mcp_server_map_hash,
    },
};

pub const PREPARED_ACP_MCP_SNAPSHOT_ENV: &str = "OPENTEAMS_ACP_MCP_SNAPSHOT_PATH";
const PREPARED_ACP_MCP_HASH_ENV: &str = "OPENTEAMS_ACP_MCP_SNAPSHOT_HASH";

/// Runtime restrictions applied to canonical MCP definitions before they are
/// converted into ACP session parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcpMcpPolicy {
    /// `None` allows every configured server. `Some(empty)` allows none.
    pub allowed_server_names: Option<BTreeSet<String>>,
    pub disabled_server_names: BTreeSet<String>,
}

/// Secret-safe result of resolving canonical MCP definitions for one run.
#[derive(Clone)]
pub struct EffectiveAcpMcpConfig {
    pub servers: Vec<McpServer>,
    pub config_hash: String,
}

impl EffectiveAcpMcpConfig {
    pub fn server_names(&self) -> BTreeSet<String> {
        self.servers
            .iter()
            .filter_map(|server| match server {
                McpServer::Stdio(server) => Some(server.name.clone()),
                McpServer::Http(server) => Some(server.name.clone()),
                McpServer::Sse(server) => Some(server.name.clone()),
                _ => None,
            })
            .collect()
    }
}

impl fmt::Debug for EffectiveAcpMcpConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectiveAcpMcpConfig")
            .field("config_hash", &self.config_hash)
            .field("server_count", &self.servers.len())
            .field("server_names", &self.server_names())
            .finish()
    }
}

/// Filter canonical definitions for an isolated Pi adapter snapshot. Only the
/// server map is retained; ambient adapter settings and approval policies are
/// deliberately excluded because OpenTeams owns both isolation and approval.
pub fn resolve_isolated_mcp_snapshot(
    canonical_config: &Value,
    policy: &AcpMcpPolicy,
) -> Result<Value, ExecutorError> {
    let filtered = filter_canonical_servers(canonical_config, policy)?;
    parse_mcp_servers(&filtered)?;
    let mut servers = filtered
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for server in servers.values_mut() {
        if let Some(server) = server.as_object_mut() {
            server.remove("approveTools");
        }
    }
    Ok(serde_json::json!({
        "mcpServers": servers,
        "settings": {
            "hostConfigDiscovery": "off"
        }
    }))
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
            _ => return Err("ACP MCP server field `type` is unsupported".to_string()),
        };
        if name.trim().is_empty() {
            return Err("ACP MCP server name must not be empty".to_string());
        }
        if !names.insert(name) {
            return Err(format!("duplicate ACP MCP server name: {name}"));
        }
        if !transport_supported {
            return Err(format!(
                "ACP MCP server `{name}` field `type` is not supported by the Agent"
            ));
        }
    }
    Ok(())
}

pub(crate) fn mcp_server_output_secrets(servers: &[McpServer]) -> Vec<String> {
    let mut values = Vec::new();
    for server in servers {
        match server {
            McpServer::Stdio(server) => {
                values.extend(server.env.iter().map(|variable| variable.value.clone()));
            }
            McpServer::Http(server) => {
                values.extend(server.headers.iter().map(|header| header.value.clone()));
            }
            McpServer::Sse(server) => {
                values.extend(server.headers.iter().map(|header| header.value.clone()));
            }
            _ => {}
        }
    }
    values
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
    let effective_member = MemberMcpConfig {
        mcp_servers: serde_json::from_value(effective_servers)?,
    };
    let config_hash = canonical_mcp_server_map_hash(&effective_member)?;
    let servers = parse_mcp_servers(&effective_value)?;
    Ok(EffectiveAcpMcpConfig {
        servers,
        config_hash,
    })
}

/// Freeze one member canonical snapshot into a private run directory.
///
/// The snapshot path and hash are pinned in both environment layers so profile
/// overrides cannot redirect a prepared run back to ambient vendor state.
pub fn prepare_acp_mcp_for_run(
    canonical: &MemberMcpConfig,
    context: &McpRunContext,
    env: &mut ExecutionEnv,
    cmd: &mut CmdOverrides,
    prefix: &str,
) -> Result<PreparedMcpRun, ExecutorError> {
    let canonical_value = serde_json::to_value(canonical)?;
    resolve_effective_mcp_config(&canonical_value, &AcpMcpPolicy::default())?;
    let prepared = PreparedMcpRun::new(canonical)?;
    let directory = PrivateMcpRunDirectory::create(context, prefix)?;
    let snapshot_path = directory.write_file("mcp.json", &serde_json::to_vec(canonical)?)?;
    pin_mcp_run_environment(
        env,
        cmd,
        PREPARED_ACP_MCP_SNAPSHOT_ENV,
        snapshot_path.to_string_lossy().into_owned(),
    );
    pin_mcp_run_environment(
        env,
        cmd,
        PREPARED_ACP_MCP_HASH_ENV,
        prepared.config_hash().to_string(),
    );
    Ok(prepared.with_cleanup(directory.into_cleanup()))
}

/// Load only the private snapshot created by `prepare_acp_mcp_for_run`.
pub async fn load_prepared_acp_mcp_config(
    env: &ExecutionEnv,
) -> Result<EffectiveAcpMcpConfig, ExecutorError> {
    let snapshot_path = env
        .get(PREPARED_ACP_MCP_SNAPSHOT_ENV)
        .map(String::as_str)
        .filter(|path| !path.trim().is_empty())
        .ok_or(ExecutorError::McpIsolationNotImplemented)?;
    let expected_hash = env
        .get(PREPARED_ACP_MCP_HASH_ENV)
        .map(String::as_str)
        .filter(|hash| !hash.trim().is_empty())
        .ok_or(ExecutorError::McpIsolationNotImplemented)?;
    let canonical: MemberMcpConfig = serde_json::from_slice(
        &tokio::fs::read(snapshot_path)
            .await
            .map_err(ExecutorError::Io)?,
    )?;
    if canonical_mcp_server_map_hash(&canonical)? != expected_hash {
        return Err(ExecutorError::Configuration(
            "prepared ACP MCP snapshot hash does not match the frozen run".to_string(),
        ));
    }
    resolve_effective_mcp_config(&serde_json::to_value(canonical)?, &AcpMcpPolicy::default())
}

pub fn pin_mcp_run_environment(
    env: &mut ExecutionEnv,
    cmd: &mut CmdOverrides,
    key: &str,
    value: impl Into<String>,
) {
    let value = value.into();
    env.insert(key, value.clone());
    cmd.env
        .get_or_insert_with(Default::default)
        .insert(key.to_string(), value);
}

/// Create a private per-run system settings file that prevents the Agent from
/// also loading its vendor-global MCP list after OpenTeams injects the ACP list.
pub fn write_mcp_isolation_settings(
    context: &McpRunContext,
    prefix: &str,
    additional_settings: Value,
) -> Result<(PathBuf, ExecutorRunCleanup), ExecutorError> {
    let directory = PrivateMcpRunDirectory::create(context, prefix)?;
    let body = serde_json::to_vec_pretty(&build_mcp_isolation_settings(additional_settings)?)?;
    let path = directory.write_file("settings.json", &body)?;
    Ok((path, directory.into_cleanup()))
}

fn build_mcp_isolation_settings(mut additional_settings: Value) -> Result<Value, ExecutorError> {
    let Some(root) = additional_settings.as_object_mut() else {
        return Err(ExecutorError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ACP isolation settings must be a JSON object",
        )));
    };
    root.insert("mcpServers".to_string(), Value::Object(Default::default()));
    Ok(additional_settings)
}

fn parse_mcp_servers(value: &Value) -> Result<Vec<McpServer>, ExecutorError> {
    let Some(servers) = value.get("mcpServers").and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut result = Vec::with_capacity(servers.len());
    for (name, value) in servers {
        let Some(server) = value.as_object() else {
            return Err(invalid_mcp_config(name, "definition", "must be an object"));
        };
        if server.get("disabled").and_then(Value::as_bool) == Some(true)
            || server.get("enabled").and_then(Value::as_bool) == Some(false)
        {
            continue;
        }
        let transport = parse_transport(server.get("type"), name)?;
        let url = server
            .get("httpUrl")
            .or_else(|| server.get("url"))
            .and_then(Value::as_str);
        if let Some(url) = url {
            if matches!(transport, Some("stdio")) {
                return Err(invalid_mcp_config(
                    name,
                    "type",
                    "does not match the configured URL transport",
                ));
            }
            reqwest::Url::parse(url).map_err(|_| invalid_mcp_config(name, "url", "is invalid"))?;
            let headers = parse_headers(server.get("headers"), name)?;
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

        if matches!(transport, Some("http" | "sse")) {
            return Err(invalid_mcp_config(name, "url", "is missing"));
        }

        let Some(command) = server.get("command").and_then(Value::as_str) else {
            return Err(invalid_mcp_config(name, "command", "is missing"));
        };
        let command = which::which(command)
            .map_err(|_| invalid_mcp_config(name, "command", "was not found"))?;
        let args = parse_string_array(server.get("args"), name, "args")?;
        let env = parse_env(server.get("env"), name)?;
        result.push(McpServer::Stdio(
            McpServerStdio::new(name, command).args(args).env(env),
        ));
    }
    Ok(result)
}

fn parse_transport<'a>(
    value: Option<&'a Value>,
    name: &str,
) -> Result<Option<&'a str>, ExecutorError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(transport) = value.as_str() else {
        return Err(invalid_mcp_config(name, "type", "must be a string"));
    };
    match transport {
        "stdio" | "http" | "sse" => Ok(Some(transport)),
        _ => Err(invalid_mcp_config(name, "type", "is unsupported")),
    }
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
        return Err(invalid_mcp_config(name, "headers", "must be an object"));
    };
    headers
        .iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| HttpHeader::new(key, value))
                .ok_or_else(|| invalid_mcp_config(name, "headers", "values must be strings"))
        })
        .collect()
}

fn parse_env(value: Option<&Value>, name: &str) -> Result<Vec<EnvVariable>, ExecutorError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Some(env) = value.as_object() else {
        return Err(invalid_mcp_config(name, "env", "must be an object"));
    };
    env.iter()
        .map(|(key, value)| {
            value
                .as_str()
                .map(|value| EnvVariable::new(key, value))
                .ok_or_else(|| invalid_mcp_config(name, "env", "values must be strings"))
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
        return Err(invalid_mcp_config(name, field, "must be an array"));
    };
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| invalid_mcp_config(name, field, "values must be strings"))
        })
        .collect()
}

fn invalid_mcp_config(name: &str, field: &str, message: &str) -> ExecutorError {
    ExecutorError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("invalid MCP server `{name}` field `{field}`: {message}"),
    ))
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        AgentCapabilities, McpCapabilities, McpServerHttp, McpServerStdio,
    };
    use uuid::Uuid;

    use super::*;
    use crate::{
        command::CmdOverrides, env::ExecutionEnv, mcp_config::MemberMcpConfig,
        mcp_run::McpRunContext,
    };

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
        let env_secret = "e!";
        let header_secret = "h?";
        let value = serde_json::json!({
            "mcpServers": {
                "local": {
                    "command": "/bin/echo",
                    "args": ["hello"],
                    "env": {"TOKEN": env_secret}
                },
                "remote": {
                    "httpUrl": "https://example.test/mcp",
                    "headers": {"Authorization": header_secret}
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
        let redactor = crate::env::SensitiveValueRedactor::default()
            .with_sensitive_values(mcp_server_output_secrets(&servers));
        let output = redactor.redact(&format!("env={env_secret}; header={header_secret}"));
        assert_eq!(output, "env=[redacted]; header=[redacted]");
    }

    #[test]
    fn unsupported_transport_error_names_only_server_and_type_field() {
        let fake_secret = "openteams-unsupported-transport-fake-secret";
        let canonical = serde_json::json!({
            "mcpServers": {
                "member-tools": {
                    "type": "websocket",
                    "url": format!("https://example.test/{fake_secret}")
                }
            }
        });

        let error = resolve_effective_mcp_config(&canonical, &AcpMcpPolicy::default())
            .expect_err("unknown transport must be rejected")
            .to_string();

        assert!(error.contains("member-tools"));
        assert!(error.contains("type"));
        assert!(!error.contains("websocket"));
        assert!(!error.contains(fake_secret));
    }

    #[test]
    fn effective_acp_mcp_debug_redacts_fake_secret() {
        let fake_secret = "openteams-effective-acp-debug-fake-secret";
        let canonical = serde_json::json!({
            "mcpServers": {
                "member-tools": {
                    "command": "/bin/echo",
                    "env": {"TOKEN": fake_secret}
                }
            }
        });

        let effective = resolve_effective_mcp_config(&canonical, &AcpMcpPolicy::default())
            .expect("effective MCP config");
        let debug = format!("{effective:?}");

        assert!(debug.contains("server_count"));
        assert!(!debug.contains(fake_secret));
    }

    #[tokio::test]
    async fn explicit_empty_member_snapshot_is_prepared_and_cleaned() {
        let workspace = tempfile::tempdir().expect("workspace");
        let context = McpRunContext::new(workspace.path(), Uuid::new_v4(), Uuid::new_v4())
            .expect("run context");
        let canonical = MemberMcpConfig::default();
        let mut env = ExecutionEnv::new(Default::default(), false, String::new());
        let mut cmd = CmdOverrides::default();

        let prepared =
            prepare_acp_mcp_for_run(&canonical, &context, &mut env, &mut cmd, "acp-empty-test")
                .expect("prepare empty MCP snapshot");
        let snapshot_path = std::path::PathBuf::from(
            env.get(PREPARED_ACP_MCP_SNAPSHOT_ENV)
                .expect("prepared snapshot path"),
        );
        let effective = load_prepared_acp_mcp_config(&env)
            .await
            .expect("load prepared snapshot");

        assert_eq!(prepared.server_count(), 0);
        assert!(effective.servers.is_empty());
        assert!(snapshot_path.is_file());
        assert_eq!(
            cmd.env
                .as_ref()
                .and_then(|values| values.get(PREPARED_ACP_MCP_SNAPSHOT_ENV)),
            env.get(PREPARED_ACP_MCP_SNAPSHOT_ENV)
        );

        drop(prepared.into_cleanup());
        assert!(!snapshot_path.exists());
        assert!(!snapshot_path.parent().expect("snapshot parent").exists());
    }

    #[tokio::test]
    async fn missing_public_preparation_fails_closed() {
        let env = ExecutionEnv::new(Default::default(), false, String::new());

        assert!(matches!(
            load_prepared_acp_mcp_config(&env).await,
            Err(ExecutorError::McpIsolationNotImplemented)
        ));
    }

    #[test]
    fn isolation_settings_cleanup_removes_private_directory() {
        let workspace = tempfile::tempdir().expect("workspace");
        let context = McpRunContext::new(workspace.path(), Uuid::new_v4(), Uuid::new_v4())
            .expect("run context");

        let (path, cleanup) = write_mcp_isolation_settings(
            &context,
            "acp-settings-test",
            serde_json::json!({"vendorSetting": true}),
        )
        .expect("isolation settings");

        assert!(path.is_file());
        drop(cleanup);
        assert!(!path.exists());
        assert!(!path.parent().expect("settings parent").exists());
    }

    #[test]
    fn temporary_file_creation_failure_cleans_partial_run_and_redacts_fake_secret() {
        let workspace = tempfile::tempdir().expect("workspace");
        let context = McpRunContext::new(workspace.path(), Uuid::new_v4(), Uuid::new_v4())
            .expect("run context");
        let directory = PrivateMcpRunDirectory::create(&context, "acp-create-failure")
            .expect("private run directory");
        let private_root = directory.path().to_path_buf();
        let fake_secret = "acp-temporary-file-failure-fake-secret-never-leak";
        directory
            .write_file("mcp.json", fake_secret.as_bytes())
            .expect("first private file");

        let error = directory
            .write_file("mcp.json", b"duplicate")
            .expect_err("create_new must fail closed on a duplicate file");
        let error_display = error.to_string();
        let error_debug = format!("{error:?}");

        assert!(!error_display.contains(fake_secret));
        assert!(!error_debug.contains(fake_secret));
        drop(directory);
        assert!(
            !private_root.exists(),
            "temporary file failure must clean the partially-created run directory"
        );
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

    #[test]
    fn isolation_settings_preserve_vendor_overrides_and_clear_mcp_servers() {
        let settings = build_mcp_isolation_settings(serde_json::json!({
            "general": {
                "sessionRetention": {
                    "enabled": false
                }
            },
            "mcpServers": {
                "must-not-leak": {
                    "command": "/bin/echo"
                }
            }
        }))
        .expect("isolation settings");

        assert_eq!(
            settings["general"]["sessionRetention"]["enabled"],
            serde_json::json!(false)
        );
        assert!(
            settings["mcpServers"]
                .as_object()
                .expect("server map")
                .is_empty()
        );
    }

    #[test]
    fn different_pi_member_snapshots_contain_only_their_allowlisted_servers() {
        let canonical = serde_json::json!({
            "settings": {"hostConfigDiscovery": "on", "approveTools": true},
            "mcpServers": {
                "allowed": {"command": "/bin/echo", "approveTools": true},
                "denied": {"command": "/bin/echo"}
            }
        });
        let policy = AcpMcpPolicy {
            allowed_server_names: Some(["allowed".to_string()].into_iter().collect()),
            disabled_server_names: Default::default(),
        };

        let alpha_snapshot =
            resolve_isolated_mcp_snapshot(&canonical, &policy).expect("alpha snapshot");
        let beta_snapshot = resolve_isolated_mcp_snapshot(
            &canonical,
            &AcpMcpPolicy {
                allowed_server_names: Some(["denied".to_string()].into_iter().collect()),
                disabled_server_names: Default::default(),
            },
        )
        .expect("beta snapshot");

        assert_eq!(
            alpha_snapshot["mcpServers"]
                .as_object()
                .expect("servers")
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["allowed"]
        );
        assert_eq!(
            beta_snapshot["mcpServers"]
                .as_object()
                .expect("servers")
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            ["denied"]
        );
        assert!(alpha_snapshot["mcpServers"].get("denied").is_none());
        assert!(beta_snapshot["mcpServers"].get("allowed").is_none());
        assert_eq!(alpha_snapshot["settings"]["hostConfigDiscovery"], "off");
        assert!(alpha_snapshot["settings"].get("approveTools").is_none());
        assert!(
            alpha_snapshot["mcpServers"]["allowed"]
                .get("approveTools")
                .is_none()
        );
    }
}
