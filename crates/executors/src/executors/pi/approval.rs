use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use workspace_utils::approvals::ApprovalStatus;

use crate::approvals::{
    ExecutorApprovalError, ExecutorApprovalOption, ExecutorApprovalRequest, ExecutorApprovalService,
};

const SESSION_ALLOW_OPTION_ID: &str = "openteams-pi-allow-always";

struct PiToolApproval {
    tool_name: String,
    input: Value,
}

struct PiApprovalService {
    inner: Arc<dyn ExecutorApprovalService>,
    allowed_tools: Mutex<HashSet<String>>,
}

pub(super) fn wrap(
    approvals: Option<Arc<dyn ExecutorApprovalService>>,
) -> Option<Arc<dyn ExecutorApprovalService>> {
    approvals.map(|inner| {
        Arc::new(PiApprovalService {
            inner,
            allowed_tools: Mutex::new(HashSet::new()),
        }) as Arc<dyn ExecutorApprovalService>
    })
}

#[async_trait]
impl ExecutorApprovalService for PiApprovalService {
    async fn request_tool_approval(
        &self,
        tool_name: &str,
        tool_input: Value,
        tool_call_id: &str,
        cancel: CancellationToken,
    ) -> Result<ApprovalStatus, ExecutorApprovalError> {
        self.inner
            .request_tool_approval(tool_name, tool_input, tool_call_id, cancel)
            .await
    }

    async fn request_acp_tool_approval(
        &self,
        mut request: ExecutorApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<String, ExecutorApprovalError> {
        let Some(tool) = pi_tool_approval(&request.tool_input) else {
            return self.inner.request_acp_tool_approval(request, cancel).await;
        };
        let Some(allow_option_id) = request
            .options
            .iter()
            .find(|option| matches!(option.kind.as_str(), "allow_always" | "allow_once"))
            .map(|option| option.option_id.clone())
        else {
            return self.inner.request_acp_tool_approval(request, cancel).await;
        };
        let tool_key = tool.tool_name.to_ascii_lowercase();
        if self.allowed_tools.lock().await.contains(&tool_key) {
            return Ok(allow_option_id);
        }

        request.tool_name = tool.tool_name;
        if let Some(command) = tool
            .input
            .get("command")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|command| !command.is_empty())
            && let Some(fields) = request.tool_input.as_object_mut()
        {
            fields.insert("command".to_string(), Value::String(command.to_string()));
        }
        let inject_session_allow = !request
            .options
            .iter()
            .any(|option| option.kind == "allow_always")
            && !request
                .options
                .iter()
                .any(|option| option.option_id == SESSION_ALLOW_OPTION_ID);
        if inject_session_allow {
            request.options.push(ExecutorApprovalOption {
                option_id: SESSION_ALLOW_OPTION_ID.to_string(),
                kind: "allow_always".to_string(),
                label: "Always allow".to_string(),
            });
        }

        let selected = self
            .inner
            .request_acp_tool_approval(request, cancel)
            .await?;
        if !inject_session_allow || selected != SESSION_ALLOW_OPTION_ID {
            return Ok(selected);
        }
        self.allowed_tools.lock().await.insert(tool_key);
        Ok(allow_option_id)
    }
}

fn pi_tool_approval(tool_input: &Value) -> Option<PiToolApproval> {
    let raw_input = ["/tool_call/rawInput", "/tool_call/raw_input"]
        .into_iter()
        .find_map(|pointer| tool_input.pointer(pointer))?;
    if raw_input.get("method")?.as_str()? != "confirm" {
        return None;
    }
    let message = serde_json::from_str::<Value>(raw_input.get("message")?.as_str()?).ok()?;
    let tool_name = message.get("toolName")?.as_str()?.trim().to_string();
    if tool_name.is_empty() {
        return None;
    }
    Some(PiToolApproval {
        tool_name,
        input: message.get("input").cloned().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CapturingApprovalService {
        requests: Mutex<Vec<ExecutorApprovalRequest>>,
        selected_option_id: String,
    }

    #[async_trait]
    impl ExecutorApprovalService for CapturingApprovalService {
        async fn request_tool_approval(
            &self,
            _tool_name: &str,
            _tool_input: Value,
            _tool_call_id: &str,
            _cancel: CancellationToken,
        ) -> Result<ApprovalStatus, ExecutorApprovalError> {
            unreachable!("Pi ACP requests preserve the ACP option IDs")
        }

        async fn request_acp_tool_approval(
            &self,
            request: ExecutorApprovalRequest,
            _cancel: CancellationToken,
        ) -> Result<String, ExecutorApprovalError> {
            self.requests.lock().await.push(request);
            Ok(self.selected_option_id.clone())
        }
    }

    fn request(tool_call_id: &str, tool_name: &str, input: Value) -> ExecutorApprovalRequest {
        ExecutorApprovalRequest {
            tool_name: format!("Run Pi tool: {tool_name}"),
            tool_input: serde_json::json!({
                "tool_call": {
                    "toolCallId": tool_call_id,
                    "rawInput": {
                        "method": "confirm",
                        "title": format!("Run Pi tool: {tool_name}"),
                        "message": serde_json::json!({
                            "toolCallId": tool_call_id,
                            "toolName": tool_name,
                            "input": input,
                        }).to_string(),
                    }
                }
            }),
            tool_call_id: tool_call_id.to_string(),
            options: vec![
                ExecutorApprovalOption {
                    option_id: "yes".to_string(),
                    kind: "allow_once".to_string(),
                    label: "Allow".to_string(),
                },
                ExecutorApprovalOption {
                    option_id: "no".to_string(),
                    kind: "reject_once".to_string(),
                    label: "Reject".to_string(),
                },
            ],
        }
    }

    #[tokio::test]
    async fn full_command_and_session_allow_stay_inside_pi_adapter() {
        let inner = Arc::new(CapturingApprovalService {
            requests: Mutex::new(Vec::new()),
            selected_option_id: SESSION_ALLOW_OPTION_ID.to_string(),
        });
        let service = wrap(Some(inner.clone())).expect("wrapped approvals");
        let command = "cargo test -p executors --features qa-mode pi --no-fail-fast";

        let selected = service
            .request_acp_tool_approval(
                request("bash-1", "bash", serde_json::json!({"command": command})),
                CancellationToken::new(),
            )
            .await
            .expect("first approval");
        assert_eq!(selected, "yes");
        let requests = inner.requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tool_name, "bash");
        assert_eq!(requests[0].tool_input["command"], command);
        assert!(requests[0].options.iter().any(|option| {
            option.option_id == SESSION_ALLOW_OPTION_ID && option.kind == "allow_always"
        }));
        drop(requests);

        let selected = service
            .request_acp_tool_approval(
                request(
                    "bash-2",
                    "bash",
                    serde_json::json!({"command": "echo cached"}),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("cached approval");
        assert_eq!(selected, "yes");
        assert_eq!(inner.requests.lock().await.len(), 1);

        service
            .request_acp_tool_approval(
                request("mcp-1", "docs_lookup", serde_json::json!({"query": "ACP"})),
                CancellationToken::new(),
            )
            .await
            .expect("different tool approval");
        assert_eq!(inner.requests.lock().await.len(), 2);

        let next_service = wrap(Some(inner.clone())).expect("next run approvals");
        next_service
            .request_acp_tool_approval(
                request(
                    "bash-next",
                    "bash",
                    serde_json::json!({"command": "echo reset"}),
                ),
                CancellationToken::new(),
            )
            .await
            .expect("next run asks again");
        assert_eq!(inner.requests.lock().await.len(), 3);
    }

    #[tokio::test]
    async fn allow_once_does_not_cache_and_non_pi_shapes_pass_through() {
        let inner = Arc::new(CapturingApprovalService {
            requests: Mutex::new(Vec::new()),
            selected_option_id: "yes".to_string(),
        });
        let service = wrap(Some(inner.clone())).expect("wrapped approvals");
        for tool_call_id in ["bash-once-1", "bash-once-2"] {
            service
                .request_acp_tool_approval(
                    request(
                        tool_call_id,
                        "bash",
                        serde_json::json!({"command": "echo ask again"}),
                    ),
                    CancellationToken::new(),
                )
                .await
                .expect("allow once");
        }

        let passthrough = ExecutorApprovalRequest {
            tool_name: "Unrecognized confirmation".to_string(),
            tool_input: serde_json::json!({"tool_call": {"rawInput": {"method": "confirm", "message": "not-json"}}}),
            tool_call_id: "unknown".to_string(),
            options: vec![ExecutorApprovalOption {
                option_id: "yes".to_string(),
                kind: "allow_once".to_string(),
                label: "Allow".to_string(),
            }],
        };
        service
            .request_acp_tool_approval(passthrough.clone(), CancellationToken::new())
            .await
            .expect("unknown shape passes through");

        let requests = inner.requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2], passthrough);
    }
}
