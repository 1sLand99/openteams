use std::{
    collections::BTreeSet,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

use crate::{
    executors::ExecutorError, mcp_config::MemberMcpConfig, mcp_run::PrivateMcpRunDirectory,
};

pub(super) struct CursorMcpOverlay {
    pub config_path: PathBuf,
    pub approval_path: PathBuf,
    pub disabled_path: PathBuf,
    pub project_slug: String,
}

/// Materialize Cursor MCP state entirely below the private run home.
///
/// Approvals are derived only from the frozen member snapshot. The user's
/// global approval file is deliberately neither read nor written.
pub(super) async fn write_run_mcp_overlay(
    directory: &PrivateMcpRunDirectory,
    canonical: &MemberMcpConfig,
    current_dir: &Path,
) -> Result<CursorMcpOverlay, ExecutorError> {
    let absolute_path =
        std::fs::canonicalize(current_dir).unwrap_or_else(|_| current_dir.to_path_buf());
    let project_slug = cursor_project_slug(&absolute_path).ok_or_else(|| {
        ExecutorError::Configuration("Cursor MCP workspace path is empty".to_string())
    })?;
    let worktree_path = absolute_path.to_string_lossy();
    let approvals = canonical
        .mcp_servers
        .iter()
        .filter(|(name, definition)| name.as_str() != "meta" && definition.is_object())
        .filter_map(|(name, definition)| {
            compute_cursor_approval_id(name, definition, &worktree_path)
        })
        .collect::<Vec<_>>();
    let disabled = project_mcp_servers_to_disable(current_dir, canonical).await?;
    let project_dir = Path::new(".cursor").join("projects").join(&project_slug);
    let config_path = directory.write_file(
        Path::new(".cursor").join("mcp.json"),
        &serde_json::to_vec_pretty(canonical)?,
    )?;
    let approval_path = directory.write_file(
        project_dir.join("mcp-approvals.json"),
        &serde_json::to_vec_pretty(&approvals)?,
    )?;
    let disabled_path = directory.write_file(
        project_dir.join("mcp-disabled.json"),
        &serde_json::to_vec_pretty(&disabled)?,
    )?;
    Ok(CursorMcpOverlay {
        config_path,
        approval_path,
        disabled_path,
        project_slug,
    })
}

async fn project_mcp_servers_to_disable(
    current_dir: &Path,
    canonical: &MemberMcpConfig,
) -> Result<Vec<String>, ExecutorError> {
    let project_path = current_dir.join(".cursor").join("mcp.json");
    let contents = match tokio::fs::read(&project_path).await {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(ExecutorError::Io(error)),
    };
    let project: serde_json::Value = serde_json::from_slice(&contents)?;
    let Some(servers) = project
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
    else {
        return Err(ExecutorError::Configuration(
            "Cursor project MCP server definitions must be an object".to_string(),
        ));
    };
    let disabled = servers
        .iter()
        .filter(|(name, definition)| canonical.mcp_servers.get(*name) != Some(*definition))
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    Ok(disabled.into_iter().collect())
}

pub(super) fn cursor_project_slug(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy();
    if raw.is_empty() {
        return None;
    }

    let slug = regex::Regex::new(r"[^A-Za-z0-9]+")
        .unwrap()
        .replace_all(&raw, "-")
        .trim_matches('-')
        .to_string();

    if slug.is_empty() { None } else { Some(slug) }
}

fn compute_cursor_approval_id(
    server_name: &str,
    definition: &serde_json::Value,
    worktree_path: &str,
) -> Option<String> {
    let payload = serde_json::json!({
        "path": worktree_path,
        "server": definition,
    });

    let serialized = serde_json::to_string(&payload).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(serialized.as_bytes());
    let digest = hasher.finalize();
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Some(format!("{server_name}-{}", &hex[..16]))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;
    use crate::mcp_run::McpRunContext;

    #[tokio::test]
    async fn overlay_trust_uses_only_snapshot_and_never_global_approval() {
        let workspace = TempDir::new().expect("create workspace");
        fs::create_dir(workspace.path().join(".cursor")).expect("create project cursor dir");
        fs::write(
            workspace.path().join(".cursor/mcp.json"),
            br#"{"mcpServers":{"ambient":{"command":"must-not-run"},"member":{"command":"different"}}}"#,
        )
        .expect("write project MCP config");
        let global_approval = workspace.path().join("global-mcp-approvals.json");
        let global_bytes = br#"["global-approval"]"#;
        fs::write(&global_approval, global_bytes).expect("write global approval fixture");
        let context = McpRunContext::new(workspace.path(), Uuid::new_v4(), Uuid::new_v4())
            .expect("create run context");
        let directory = PrivateMcpRunDirectory::create(&context, "cursor-overlay-test")
            .expect("create private directory");
        let canonical: MemberMcpConfig = serde_json::from_value(serde_json::json!({
            "mcpServers": {
                "member": {"command": "/bin/echo", "env": {"TOKEN": "cursor-secret"}}
            }
        }))
        .expect("deserialize canonical MCP config");

        let overlay = write_run_mcp_overlay(&directory, &canonical, workspace.path())
            .await
            .expect("write Cursor overlay");
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(&overlay.config_path).unwrap()).unwrap();
        let approvals: Vec<String> =
            serde_json::from_slice(&fs::read(&overlay.approval_path).unwrap()).unwrap();
        let disabled: Vec<String> =
            serde_json::from_slice(&fs::read(&overlay.disabled_path).unwrap()).unwrap();
        assert_eq!(config["mcpServers"].as_object().unwrap().len(), 1);
        assert!(
            approvals
                .iter()
                .any(|approval| approval.starts_with("member-"))
        );
        assert_eq!(disabled, ["ambient", "member"]);
        assert_eq!(fs::read(global_approval).unwrap(), global_bytes);
    }
}
