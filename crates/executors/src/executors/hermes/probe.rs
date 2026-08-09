use std::{path::Path, process::Stdio, time::Duration};

use command_group::AsyncCommandGroup;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{ChildStdin, ChildStdout, Command},
    time::timeout,
};

use super::super::acp::{
    AcpAuthMethodInfo, AcpCapabilityProbe, AcpConfigChoice, AcpConfigOptionKind,
    AcpConfigOptionSnapshot, AcpConfigSource,
};
use crate::{
    command::{CmdOverrides, CommandParts},
    env::ExecutionEnv,
    executors::ExecutorError,
};

pub(crate) const HERMES_SETUP_AUTH_METHOD_ID: &str = "hermes-setup";
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(20);
const SESSION_METADATA_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn provider_needs_setup(probe: &AcpCapabilityProbe) -> bool {
    probe
        .auth_methods
        .iter()
        .any(|method| method.id == HERMES_SETUP_AUTH_METHOD_ID)
        && !probe
            .auth_methods
            .iter()
            .any(|method| method.id != HERMES_SETUP_AUTH_METHOD_ID)
}

pub(crate) async fn probe_hermes_acp_command(
    command_parts: CommandParts,
    current_dir: &Path,
    env: &ExecutionEnv,
    cmd_overrides: &CmdOverrides,
    auth_method_id: Option<String>,
) -> Result<AcpCapabilityProbe, ExecutorError> {
    if auth_method_id.as_deref() == Some(HERMES_SETUP_AUTH_METHOD_ID) {
        return Err(ExecutorError::AuthRequired(
            "Hermes provider setup is required; run `hermes acp --setup` in a terminal".to_string(),
        ));
    }

    let command_display = command_parts.redacted_display();
    let (program_path, args) = command_parts.into_resolved().await.map_err(|error| {
        probe_error(
            &command_display,
            "resolve Hermes ACP probe executable",
            error,
        )
    })?;
    let mut command = Command::new(program_path);
    command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .current_dir(current_dir)
        .args(args);
    env.clone()
        .with_profile(cmd_overrides)
        .apply_to_command(&mut command);
    let mut child = command
        .group_spawn()
        .map_err(|error| probe_error(&command_display, "start Hermes ACP probe process", error))?;
    let stdout = child.inner().stdout.take().ok_or_else(|| {
        probe_error(
            &command_display,
            "open Hermes ACP probe stdout",
            "stdout pipe unavailable",
        )
    })?;
    let mut stdin = child.inner().stdin.take().ok_or_else(|| {
        probe_error(
            &command_display,
            "open Hermes ACP probe stdin",
            "stdin pipe unavailable",
        )
    })?;
    let mut lines = BufReader::new(stdout).lines();

    let result = async {
        let initialize = request(
            &mut stdin,
            &mut lines,
            1,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false,
                    "session": { "configOptions": { "boolean": {} } }
                },
                "clientInfo": {
                    "name": "openteams-hermes-probe",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            INITIALIZE_TIMEOUT,
            &command_display,
            "initialize Hermes ACP connection",
        )
        .await?;
        let mut probe = parse_initialize(&initialize, &command_display)?;

        if provider_needs_setup(&probe) {
            return Ok(probe);
        }

        if let Some(method_id) = auth_method_id {
            if !probe
                .auth_methods
                .iter()
                .any(|method| method.id == method_id)
            {
                return Err(ExecutorError::AuthRequired(format!(
                    "Hermes ACP authentication method `{method_id}` was not advertised"
                )));
            }
            request(
                &mut stdin,
                &mut lines,
                2,
                "authenticate",
                json!({ "methodId": method_id }),
                INITIALIZE_TIMEOUT,
                &command_display,
                "authenticate Hermes ACP connection",
            )
            .await?;
        }

        match request(
            &mut stdin,
            &mut lines,
            3,
            "session/new",
            json!({
                "cwd": current_dir,
                "mcpServers": [],
                "additionalDirectories": []
            }),
            SESSION_METADATA_TIMEOUT,
            &command_display,
            "discover Hermes ACP session metadata",
        )
        .await
        {
            Ok(session) => apply_session_metadata(&mut probe, &session),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Hermes ACP initialized but session metadata discovery failed"
                );
            }
        }
        Ok(probe)
    }
    .await;

    let _ = child.kill().await;
    result
}

#[allow(clippy::too_many_arguments)]
async fn request(
    stdin: &mut ChildStdin,
    lines: &mut Lines<BufReader<ChildStdout>>,
    id: u64,
    method: &str,
    params: Value,
    duration: Duration,
    command_display: &str,
    operation: &str,
) -> Result<Value, ExecutorError> {
    timeout(duration, async {
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        stdin
            .write_all(payload.to_string().as_bytes())
            .await
            .map_err(|error| probe_error(command_display, operation, error))?;
        stdin
            .write_all(if cfg!(windows) { b"\r\n" } else { b"\n" })
            .await
            .map_err(|error| probe_error(command_display, operation, error))?;
        stdin
            .flush()
            .await
            .map_err(|error| probe_error(command_display, operation, error))?;

        loop {
            let line = lines
                .next_line()
                .await
                .map_err(|error| probe_error(command_display, operation, error))?
                .ok_or_else(|| {
                    probe_error(
                        command_display,
                        operation,
                        "probe process exited without a response",
                    )
                })?;
            let message: Value = serde_json::from_str(&line)
                .map_err(|error| probe_error(command_display, operation, error))?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let detail = error
                    .get("data")
                    .or_else(|| error.get("message"))
                    .unwrap_or(error);
                return Err(probe_error(command_display, operation, detail));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| probe_error(command_display, operation, "response omitted result"));
        }
    })
    .await
    .map_err(|_| {
        probe_error(
            command_display,
            operation,
            format!("timed out after {} seconds", duration.as_secs()),
        )
    })?
}

fn parse_initialize(
    initialize: &Value,
    command_display: &str,
) -> Result<AcpCapabilityProbe, ExecutorError> {
    let protocol_version = initialize
        .get("protocolVersion")
        .map(value_string)
        .unwrap_or_default();
    if protocol_version != "1" {
        return Err(probe_error(
            command_display,
            "initialize Hermes ACP connection",
            format!("unsupported ACP protocol version `{protocol_version}`"),
        ));
    }
    let capabilities = initialize
        .get("agentCapabilities")
        .cloned()
        .unwrap_or(Value::Null);
    let session = capabilities
        .get("sessionCapabilities")
        .unwrap_or(&Value::Null);
    let agent_info = initialize.get("agentInfo").unwrap_or(&Value::Null);
    let auth_methods = initialize
        .get("authMethods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|method| {
            let id = method.get("id")?.as_str()?.to_string();
            Some(AcpAuthMethodInfo {
                name: method
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_string(),
                description: method
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                id,
            })
        })
        .collect();

    Ok(AcpCapabilityProbe {
        protocol_version,
        agent_name: agent_info
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string),
        agent_version: agent_info
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
        auth_methods,
        supports_session_list: capability_declared(session, "list"),
        supports_session_resume: capability_declared(session, "resume"),
        supports_session_load: capabilities
            .get("loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        supports_session_close: capability_declared(session, "close"),
        supports_session_delete: capability_declared(session, "delete"),
        supports_additional_directories: capability_declared(session, "additionalDirectories"),
        agent_capabilities: capabilities,
        config_source: AcpConfigSource::None,
        config_options: Vec::new(),
    })
}

fn apply_session_metadata(probe: &mut AcpCapabilityProbe, session: &Value) {
    let Some(models) = session.get("models") else {
        return;
    };
    let Some(current_model_id) = models.get("currentModelId").and_then(Value::as_str) else {
        return;
    };
    let mut options = models
        .get("availableModels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let value = model.get("modelId")?.as_str()?.trim();
            if value.is_empty() {
                return None;
            }
            Some(AcpConfigChoice {
                value: value.to_string(),
                name: model
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(value)
                    .to_string(),
                description: model
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect::<Vec<_>>();
    if !options
        .iter()
        .any(|option| option.value == current_model_id)
    {
        options.push(AcpConfigChoice {
            value: current_model_id.to_string(),
            name: current_model_id.to_string(),
            description: None,
        });
    }
    if options.is_empty() {
        return;
    }
    probe.config_source = AcpConfigSource::LegacyModel;
    probe.config_options = vec![AcpConfigOptionSnapshot {
        id: "model".to_string(),
        name: "Model".to_string(),
        description: Some("Legacy Hermes ACP session model selector".to_string()),
        category: Some("model".to_string()),
        kind: AcpConfigOptionKind::Select {
            current_value: current_model_id.to_string(),
            options,
        },
    }];
}

fn capability_declared(value: &Value, key: &str) -> bool {
    match value.get(key) {
        Some(Value::Bool(value)) => *value,
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

fn value_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn probe_error(command: &str, operation: &str, result: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Io(std::io::Error::other(format!(
        "command=`{command}`; operation={operation}; result={}",
        result.to_string().trim()
    )))
}
