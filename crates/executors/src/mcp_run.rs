use std::{
    fmt,
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    executors::{ExecutorError, ExecutorRunCleanup},
    mcp_config::MemberMcpConfig,
};

/// Immutable, adapter-neutral identity and filesystem scope for one executor run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpRunContext {
    current_dir: PathBuf,
    session_agent_id: Uuid,
    run_id: Uuid,
    authorized_skill_paths: Vec<PathBuf>,
}

impl McpRunContext {
    pub fn new(
        current_dir: impl Into<PathBuf>,
        session_agent_id: Uuid,
        run_id: Uuid,
    ) -> Result<Self, ExecutorError> {
        let current_dir = current_dir.into();
        if !current_dir.is_absolute() {
            return Err(ExecutorError::Configuration(
                "MCP run workspace path must be absolute".to_string(),
            ));
        }
        Ok(Self {
            current_dir,
            session_agent_id,
            run_id,
            authorized_skill_paths: Vec::new(),
        })
    }

    pub fn with_authorized_skill_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.authorized_skill_paths = paths;
        self
    }

    pub fn current_dir(&self) -> &Path {
        &self.current_dir
    }

    pub fn session_agent_id(&self) -> Uuid {
        self.session_agent_id
    }

    pub fn run_id(&self) -> Uuid {
        self.run_id
    }

    pub fn authorized_skill_paths(&self) -> &[PathBuf] {
        &self.authorized_skill_paths
    }
}

/// Secret-safe metadata and cleanup ownership produced before an executor is spawned.
pub struct PreparedMcpRun {
    config_hash: String,
    server_names: Vec<String>,
    cleanup: Option<ExecutorRunCleanup>,
}

/// Secret-safe failure metadata for the public run-scoped MCP preparation boundary.
///
/// Adapter errors are deliberately classified and dropped rather than retained as a
/// source: diagnostics may contain command arguments, headers, or environment values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "Run-scoped MCP preparation failed: {kind}; mcp_config_hash={config_hash}; mcp_server_count={server_count}; mcp_server_names={server_names:?}"
)]
pub struct McpRunPreparationError {
    kind: McpRunPreparationFailureKind,
    config_hash: String,
    server_count: usize,
    server_names: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum McpRunPreparationFailureKind {
    #[error("member MCP configuration is not initialized")]
    NotInitialized,
    #[error("member MCP configuration is invalid")]
    InvalidConfiguration,
    #[error("MCP is not supported by this executor")]
    NotSupported,
    #[error("run-scoped MCP isolation is not implemented by this executor")]
    IsolationNotImplemented,
    #[error("executor rejected run-scoped MCP preparation")]
    AdapterRejected,
    #[error("run-scoped MCP resource preparation failed")]
    ResourcePreparationFailed,
}

impl McpRunPreparationError {
    pub fn not_initialized() -> Self {
        Self {
            kind: McpRunPreparationFailureKind::NotInitialized,
            config_hash: "unavailable".to_string(),
            server_count: 0,
            server_names: Vec::new(),
        }
    }

    pub fn invalid_configuration(canonical: &MemberMcpConfig) -> Self {
        Self::from_kind(
            McpRunPreparationFailureKind::InvalidConfiguration,
            canonical,
        )
    }

    pub fn from_executor_error(canonical: &MemberMcpConfig, error: ExecutorError) -> Self {
        let kind = match error {
            ExecutorError::McpNotSupported => McpRunPreparationFailureKind::NotSupported,
            ExecutorError::McpIsolationNotImplemented => {
                McpRunPreparationFailureKind::IsolationNotImplemented
            }
            ExecutorError::Io(_)
            | ExecutorError::SpawnError(_)
            | ExecutorError::Json(_)
            | ExecutorError::TomlSerialize(_)
            | ExecutorError::TomlDeserialize(_)
            | ExecutorError::Yaml(_)
            | ExecutorError::CommandBuild(_)
            | ExecutorError::ExecutableNotFound { .. } => {
                McpRunPreparationFailureKind::ResourcePreparationFailed
            }
            ExecutorError::FollowUpNotSupported(_)
            | ExecutorError::UnknownExecutorType(_)
            | ExecutorError::ExecutorApprovalError(_)
            | ExecutorError::SetupHelperNotSupported
            | ExecutorError::AuthRequired(_)
            | ExecutorError::Configuration(_) => McpRunPreparationFailureKind::AdapterRejected,
        };
        Self::from_kind(kind, canonical)
    }

    pub fn kind(&self) -> McpRunPreparationFailureKind {
        self.kind
    }

    pub fn config_hash(&self) -> &str {
        &self.config_hash
    }

    pub fn server_count(&self) -> usize {
        self.server_count
    }

    pub fn server_names(&self) -> &[String] {
        &self.server_names
    }

    fn from_kind(kind: McpRunPreparationFailureKind, canonical: &MemberMcpConfig) -> Self {
        let server_names = canonical.mcp_servers.keys().cloned().collect::<Vec<_>>();
        Self {
            kind,
            config_hash: canonical_mcp_server_map_hash(canonical)
                .unwrap_or_else(|_| "unavailable".to_string()),
            server_count: server_names.len(),
            server_names,
        }
    }
}

impl PreparedMcpRun {
    pub fn new(canonical: &MemberMcpConfig) -> Result<Self, ExecutorError> {
        Ok(Self {
            config_hash: canonical_mcp_server_map_hash(canonical)?,
            server_names: canonical.mcp_servers.keys().cloned().collect(),
            cleanup: None,
        })
    }

    pub fn with_cleanup(mut self, cleanup: ExecutorRunCleanup) -> Self {
        self.cleanup = ExecutorRunCleanup::combine(self.cleanup.take(), Some(cleanup));
        self
    }

    pub fn config_hash(&self) -> &str {
        &self.config_hash
    }

    pub fn server_names(&self) -> &[String] {
        &self.server_names
    }

    pub fn server_count(&self) -> usize {
        self.server_names.len()
    }

    pub fn into_cleanup(mut self) -> Option<ExecutorRunCleanup> {
        self.cleanup.take()
    }
}

impl fmt::Debug for PreparedMcpRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedMcpRun")
            .field("config_hash", &self.config_hash)
            .field("server_count", &self.server_names.len())
            .field("server_names", &self.server_names)
            .field("has_cleanup", &self.cleanup.is_some())
            .finish()
    }
}

/// Hash only the canonical server map. Object keys are recursively sorted so
/// semantically identical JSON does not depend on editor insertion order.
pub fn canonical_mcp_server_map_hash(canonical: &MemberMcpConfig) -> Result<String, ExecutorError> {
    let mut servers = Map::new();
    for (name, definition) in &canonical.mcp_servers {
        servers.insert(name.clone(), canonicalize_json(definition));
    }
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&Value::Object(servers))?)
    ))
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut canonical = Map::new();
            for (key, value) in entries {
                canonical.insert(key.clone(), canonicalize_json(value));
            }
            Value::Object(canonical)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_json).collect()),
        _ => value.clone(),
    }
}

/// A UUID-tokenized, owner-private directory below `<workspace>/.openteams/tmp`.
/// Keeping the cleanup armed makes partially-created resources fail closed.
pub struct PrivateMcpRunDirectory {
    path: PathBuf,
    cleanup: Option<ExecutorRunCleanup>,
}

impl PrivateMcpRunDirectory {
    pub fn create(context: &McpRunContext, prefix: &str) -> Result<Self, ExecutorError> {
        validate_prefix(prefix)?;
        let runtime_root = context.current_dir().join(".openteams").join("tmp");
        fs::create_dir_all(&runtime_root).map_err(ExecutorError::Io)?;
        ensure_runtime_root_is_scoped(context.current_dir(), &runtime_root)?;

        let path = runtime_root.join(format!("{prefix}-{}-{}", context.run_id(), Uuid::new_v4()));
        create_private_directory(&path)?;
        let cleanup = ExecutorRunCleanup::private_directory(path.clone());
        Ok(Self {
            path,
            cleanup: Some(cleanup),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn create_directory(
        &self,
        relative_path: impl AsRef<Path>,
    ) -> Result<PathBuf, ExecutorError> {
        let relative_path = relative_path.as_ref();
        validate_relative_path(relative_path, false)?;
        self.ensure_relative_directories(relative_path)?;
        Ok(self.path.join(relative_path))
    }

    pub fn write_file(
        &self,
        relative_path: impl AsRef<Path>,
        contents: &[u8],
    ) -> Result<PathBuf, ExecutorError> {
        let relative_path = relative_path.as_ref();
        validate_relative_path(relative_path, false)?;
        if let Some(parent) = relative_path.parent()
            && !parent.as_os_str().is_empty()
        {
            self.ensure_relative_directories(parent)?;
        }
        let path = self.path.join(relative_path);
        write_private_file(&path, contents)?;
        Ok(path)
    }

    pub fn into_cleanup(mut self) -> ExecutorRunCleanup {
        self.cleanup
            .take()
            .expect("private MCP run directory cleanup is armed")
    }

    fn ensure_relative_directories(&self, relative_path: &Path) -> Result<(), ExecutorError> {
        validate_relative_path(relative_path, true)?;
        let mut current = self.path.clone();
        for component in relative_path.components() {
            let Component::Normal(component) = component else {
                unreachable!("relative path was validated")
            };
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                Ok(_) => {
                    return Err(ExecutorError::Configuration(
                        "MCP run private path is not a directory".to_string(),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    create_private_directory(&current)?;
                }
                Err(error) => return Err(ExecutorError::Io(error)),
            }
        }
        Ok(())
    }
}

impl fmt::Debug for PrivateMcpRunDirectory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateMcpRunDirectory")
            .field("path", &self.path)
            .field("armed", &self.cleanup.is_some())
            .finish()
    }
}

fn validate_prefix(prefix: &str) -> Result<(), ExecutorError> {
    if prefix.is_empty()
        || prefix.len() > 64
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ExecutorError::Configuration(
            "MCP run private resource prefix is invalid".to_string(),
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &Path, allow_empty: bool) -> Result<(), ExecutorError> {
    if !allow_empty && path.as_os_str().is_empty() {
        return Err(ExecutorError::Configuration(
            "MCP run private resource path is empty".to_string(),
        ));
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ExecutorError::Configuration(
            "MCP run private resource path must stay inside its run directory".to_string(),
        ));
    }
    Ok(())
}

fn ensure_runtime_root_is_scoped(
    workspace: &Path,
    runtime_root: &Path,
) -> Result<(), ExecutorError> {
    let workspace = fs::canonicalize(workspace).map_err(ExecutorError::Io)?;
    let runtime_root = fs::canonicalize(runtime_root).map_err(ExecutorError::Io)?;
    if !runtime_root.starts_with(&workspace) {
        return Err(ExecutorError::Configuration(
            "MCP run temporary directory escapes the workspace".to_string(),
        ));
    }
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), ExecutorError> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).map_err(ExecutorError::Io)
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), ExecutorError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(ExecutorError::Io)?;
    file.write_all(contents).map_err(ExecutorError::Io)?;
    file.sync_all().map_err(ExecutorError::Io)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{
        env::{ExecutionEnv, RepoContext},
        executors::{BaseCodingAgent, StandardCodingAgentExecutor},
        profile::{ExecutorConfigs, ExecutorProfileId},
    };

    fn context(workspace: &std::path::Path) -> McpRunContext {
        McpRunContext::new(workspace, Uuid::new_v4(), Uuid::new_v4()).expect("absolute run context")
    }

    #[test]
    fn config_hash_is_stable_and_only_covers_the_server_map() {
        let first = MemberMcpConfig {
            mcp_servers: [(
                "alpha".to_string(),
                json!({
                    "command": "/bin/echo",
                    "args": ["hello"],
                    "env": {"TOKEN": "fake-secret", "REGION": "test"}
                }),
            )]
            .into_iter()
            .collect(),
        };
        let reordered = MemberMcpConfig {
            mcp_servers: [(
                "alpha".to_string(),
                json!({
                    "env": {"REGION": "test", "TOKEN": "fake-secret"},
                    "args": ["hello"],
                    "command": "/bin/echo"
                }),
            )]
            .into_iter()
            .collect(),
        };

        let first_hash = canonical_mcp_server_map_hash(&first).expect("first hash");
        let reordered_hash = canonical_mcp_server_map_hash(&reordered).expect("reordered hash");
        assert_eq!(first_hash, reordered_hash);

        let prepared = PreparedMcpRun::new(&first).expect("prepared metadata");
        let debug = format!("{prepared:?}");
        assert!(debug.contains(&first_hash));
        assert!(debug.contains("alpha"));
        assert!(!debug.contains("fake-secret"));
        assert!(!debug.contains("/bin/echo"));
    }

    #[test]
    fn adapter_diagnostics_are_dropped_at_the_public_preparation_boundary() {
        let canonical_secret = "MCP_CANONICAL_SECRET_NEVER_EXPOSE";
        let adapter_secret = "MCP_ADAPTER_DIAGNOSTIC_SECRET_NEVER_EXPOSE";
        let canonical = MemberMcpConfig {
            mcp_servers: [(
                "safe-server-name".to_string(),
                json!({
                    "command": format!("/tmp/{canonical_secret}"),
                    "args": [canonical_secret],
                    "env": {"TOKEN": canonical_secret}
                }),
            )]
            .into_iter()
            .collect(),
        };
        let expected_hash =
            canonical_mcp_server_map_hash(&canonical).expect("canonical config hash");

        let error = McpRunPreparationError::from_executor_error(
            &canonical,
            ExecutorError::Io(std::io::Error::other(format!(
                "adapter stderr echoed {adapter_secret} and {canonical_secret}"
            ))),
        );
        let display = error.to_string();
        let debug = format!("{error:?}");

        for output in [&display, &debug] {
            assert!(!output.contains(canonical_secret));
            assert!(!output.contains(adapter_secret));
            assert!(output.contains(&expected_hash));
            assert!(output.contains("safe-server-name"));
        }
        assert_eq!(
            error.kind(),
            McpRunPreparationFailureKind::ResourcePreparationFailed
        );
        assert_eq!(error.server_count(), 1);
    }

    #[test]
    fn context_rejects_relative_workspaces() {
        let error = McpRunContext::new(
            PathBuf::from("relative-workspace"),
            Uuid::new_v4(),
            Uuid::new_v4(),
        )
        .expect_err("relative workspace must fail closed");

        assert!(error.to_string().contains("absolute"));
    }

    #[cfg(unix)]
    #[test]
    fn private_run_directories_use_unpredictable_names_and_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().expect("workspace");
        let context = context(workspace.path());
        let first = PrivateMcpRunDirectory::create(&context, "fixture").expect("first directory");
        let second = PrivateMcpRunDirectory::create(&context, "fixture").expect("second directory");
        assert_ne!(first.path(), second.path());
        assert_eq!(
            std::fs::metadata(first.path())
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let private_file = first
            .write_file("nested/config.json", br#"{"token":"fake-secret"}"#)
            .expect("private file");
        assert_eq!(
            std::fs::metadata(&private_file)
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(private_file.parent().expect("file parent"))
                .expect("parent metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn failed_private_resource_preparation_cleans_partial_directory() {
        let workspace = tempfile::tempdir().expect("workspace");
        let context = context(workspace.path());
        let directory =
            PrivateMcpRunDirectory::create(&context, "partial").expect("private directory");
        let private_root = directory.path().to_path_buf();
        directory
            .write_file("config.json", b"first")
            .expect("first file");
        directory
            .write_file("config.json", b"duplicate")
            .expect_err("create_new must reject duplicate files");
        drop(directory);

        assert!(!private_root.exists());
    }

    #[test]
    fn combined_cleanup_removes_only_registered_run_resources() {
        let workspace = tempfile::tempdir().expect("workspace");
        let context = context(workspace.path());
        let vendor_file = workspace.path().join("vendor-config.json");
        std::fs::write(&vendor_file, "preserve").expect("vendor fixture");

        let first = PrivateMcpRunDirectory::create(&context, "first").expect("first directory");
        let first_path = first.path().to_path_buf();
        let second = PrivateMcpRunDirectory::create(&context, "second").expect("second directory");
        let second_path = second.path().to_path_buf();
        let cleanup =
            ExecutorRunCleanup::combine(Some(first.into_cleanup()), Some(second.into_cleanup()))
                .expect("combined cleanup");

        assert!(first_path.exists());
        assert!(second_path.exists());
        drop(cleanup);

        assert!(!first_path.exists());
        assert!(!second_path.exists());
        assert_eq!(
            std::fs::read_to_string(vendor_file).expect("preserved vendor file"),
            "preserve"
        );
    }

    #[test]
    fn prepared_cleanup_is_released_when_spawn_fails_before_handoff() {
        fn fail_spawn(_prepared: PreparedMcpRun) -> Result<(), ExecutorError> {
            Err(ExecutorError::Io(std::io::Error::other(
                "fixture spawn failure",
            )))
        }

        let workspace = tempfile::tempdir().expect("workspace");
        let context = context(workspace.path());
        let directory =
            PrivateMcpRunDirectory::create(&context, "spawn-failure").expect("private directory");
        let cleanup_path = directory.path().to_path_buf();
        let prepared = PreparedMcpRun::new(&MemberMcpConfig::default())
            .expect("prepared metadata")
            .with_cleanup(directory.into_cleanup());

        fail_spawn(prepared).expect_err("spawn must fail");

        assert!(!cleanup_path.exists());
    }

    #[tokio::test]
    async fn default_executor_capability_fails_closed_without_isolation() {
        let workspace = tempfile::tempdir().expect("workspace");
        let context = context(workspace.path());
        let mut env = ExecutionEnv::new(
            RepoContext::new(workspace.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        );
        let mut executor = ExecutorConfigs::from_defaults()
            .get_coding_agent_or_default(&ExecutorProfileId::new(BaseCodingAgent::Codex));

        let error = executor
            .prepare_mcp_for_run(&MemberMcpConfig::default(), &context, &mut env)
            .await
            .expect_err("MCP adapter without isolation must fail even for an empty map");

        assert!(matches!(error, ExecutorError::McpIsolationNotImplemented));
    }

    #[tokio::test]
    async fn default_non_mcp_executor_accepts_only_an_empty_map() {
        let workspace = tempfile::tempdir().expect("workspace");
        let context = context(workspace.path());
        let mut env = ExecutionEnv::new(
            RepoContext::new(workspace.path().to_path_buf(), Vec::new()),
            false,
            String::new(),
        );
        let mut executor = ExecutorConfigs::from_defaults()
            .get_coding_agent_or_default(&ExecutorProfileId::new(BaseCodingAgent::DeepseekHarness));

        let prepared = executor
            .prepare_mcp_for_run(&MemberMcpConfig::default(), &context, &mut env)
            .await
            .expect("empty map is valid for a non-MCP executor");
        assert_eq!(prepared.server_count(), 0);

        let configured = MemberMcpConfig {
            mcp_servers: [("server".to_string(), json!({"command": "/bin/echo"}))]
                .into_iter()
                .collect(),
        };
        let error = executor
            .prepare_mcp_for_run(&configured, &context, &mut env)
            .await
            .expect_err("non-MCP executor must reject configured servers");
        assert!(matches!(error, ExecutorError::McpNotSupported));
    }
}
