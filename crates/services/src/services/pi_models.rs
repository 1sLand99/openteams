use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;
use ts_rs::TS;
use uuid::Uuid;

use super::cli_config::{
    CliConfig, CustomModelConfig, CustomProviderEntry, normalize_custom_provider_entries,
};

const MANAGED_PROVIDER_PREFIX: &str = "openteams-";
const MODELS_FILE_NAME: &str = "models.json";
const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;
const DEFAULT_MAX_TOKENS: u64 = 16_384;
pub const PI_MODELS_SYNC_RETRY_PATH: &str = "/api/config/cli/pi-models/sync";

static PI_MODELS_SYNC_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct PiModelsSkippedProvider {
    pub provider_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct PiModelsSyncResult {
    pub updated: bool,
    pub managed_provider_count: usize,
    pub removed_provider_count: usize,
    pub skipped: Vec<PiModelsSkippedProvider>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export)]
pub struct PiModelsSyncDiagnostic {
    pub synchronized: bool,
    pub result: Option<PiModelsSyncResult>,
    pub error: Option<String>,
    pub retry_available: bool,
    pub retry_path: String,
}

impl PiModelsSyncDiagnostic {
    fn success(result: PiModelsSyncResult) -> Self {
        Self {
            synchronized: true,
            result: Some(result),
            error: None,
            retry_available: true,
            retry_path: PI_MODELS_SYNC_RETRY_PATH.to_string(),
        }
    }

    fn failure(error: PiModelsCoordinationError) -> Self {
        Self {
            synchronized: false,
            result: None,
            error: Some(error.to_string()),
            retry_available: true,
            retry_path: PI_MODELS_SYNC_RETRY_PATH.to_string(),
        }
    }
}

#[derive(Debug, Error)]
pub enum PiModelsCoordinationError {
    #[error("cannot determine the OpenTeams provider settings path")]
    SettingsPathUnavailable,
    #[error("OpenTeams provider settings could not be read or parsed; Pi models were not changed")]
    SettingsUnreadable,
    #[error(transparent)]
    Synchronize(#[from] PiModelsError),
    #[error("Pi model coordination task failed")]
    TaskFailed,
}

#[derive(Debug, Error)]
pub enum PiModelsError {
    #[error("cannot determine the Pi agent directory")]
    HomeDirectoryUnavailable,
    #[error("failed to read Pi models configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Pi models configuration at {path} is invalid JSON (line {line}, column {column})")]
    InvalidJson {
        path: PathBuf,
        line: usize,
        column: usize,
    },
    #[error("Pi models configuration at {path} has an invalid structure: {reason}")]
    InvalidStructure { path: PathBuf, reason: &'static str },
    #[error("failed to create the Pi agent directory at {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create a protected temporary Pi models file: {0}")]
    CreateTemporary(std::io::Error),
    #[error("failed to write the protected temporary Pi models file: {0}")]
    WriteTemporary(std::io::Error),
    #[error("failed to validate the temporary Pi models file")]
    ValidateTemporary,
    #[error("failed to atomically replace the Pi models file: {0}")]
    AtomicReplace(std::io::Error),
    #[error("failed to secure the Pi models file: {0}")]
    Permissions(std::io::Error),
}

#[derive(Debug, Serialize)]
struct PiProviderConfig {
    #[serde(rename = "baseUrl", skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    #[serde(rename = "apiKey", skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    api: &'static str,
    models: Vec<PiModelConfig>,
}

#[derive(Debug, Serialize)]
struct PiModelConfig {
    id: String,
    name: String,
    reasoning: bool,
    input: Vec<String>,
    #[serde(rename = "contextWindow")]
    context_window: u64,
    #[serde(rename = "maxTokens")]
    max_tokens: u64,
}

struct DesiredProviders {
    providers: BTreeMap<String, Value>,
    skipped: Vec<PiModelsSkippedProvider>,
}

pub fn pi_agent_dir() -> Result<PathBuf, PiModelsError> {
    dirs::home_dir()
        .map(|home| home.join(".pi").join("agent"))
        .ok_or(PiModelsError::HomeDirectoryUnavailable)
}

pub async fn synchronize_pi_models(
    config: &CliConfig,
) -> Result<PiModelsSyncResult, PiModelsError> {
    let config = config.clone();
    let agent_dir = pi_agent_dir()?;
    tokio::task::spawn_blocking(move || synchronize_pi_models_in_agent_dir(&config, &agent_dir))
        .await
        .map_err(|error| PiModelsError::WriteTemporary(std::io::Error::other(error)))?
}

pub async fn coordinate_pi_models_from_saved_config()
-> Result<PiModelsSyncResult, PiModelsCoordinationError> {
    let config_path =
        CliConfig::config_path().ok_or(PiModelsCoordinationError::SettingsPathUnavailable)?;
    let agent_dir = pi_agent_dir()?;
    coordinate_pi_models_from_paths(&config_path, &agent_dir).await
}

pub async fn coordinate_pi_models_with_diagnostic() -> PiModelsSyncDiagnostic {
    match coordinate_pi_models_from_saved_config().await {
        Ok(result) => PiModelsSyncDiagnostic::success(result),
        Err(error) => PiModelsSyncDiagnostic::failure(error),
    }
}

pub async fn coordinate_pi_models_from_paths(
    config_path: &Path,
    agent_dir: &Path,
) -> Result<PiModelsSyncResult, PiModelsCoordinationError> {
    let mut config = match tokio::fs::read_to_string(config_path).await {
        Ok(content) => toml::from_str::<CliConfig>(&content)
            .map_err(|_| PiModelsCoordinationError::SettingsUnreadable)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CliConfig::default_config(),
        Err(_) => return Err(PiModelsCoordinationError::SettingsUnreadable),
    };
    normalize_custom_provider_entries(&mut config);
    let agent_dir = agent_dir.to_path_buf();
    tokio::task::spawn_blocking(move || synchronize_pi_models_in_agent_dir(&config, &agent_dir))
        .await
        .map_err(|_| PiModelsCoordinationError::TaskFailed)?
        .map_err(PiModelsCoordinationError::from)
}

pub fn synchronize_pi_models_in_agent_dir(
    config: &CliConfig,
    agent_dir: &Path,
) -> Result<PiModelsSyncResult, PiModelsError> {
    let _guard = PI_MODELS_SYNC_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    synchronize_pi_models_in_agent_dir_unlocked(config, agent_dir)
}

fn synchronize_pi_models_in_agent_dir_unlocked(
    config: &CliConfig,
    agent_dir: &Path,
) -> Result<PiModelsSyncResult, PiModelsError> {
    let path = agent_dir.join(MODELS_FILE_NAME);
    let desired = build_desired_providers(config);
    let original = read_models_root(&path)?;
    let mut root = original.clone();
    let providers = root
        .as_object_mut()
        .expect("read_models_root validates the root object")
        .entry("providers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .expect("read_models_root validates the providers object");

    let existing_managed: BTreeSet<String> = providers
        .keys()
        .filter(|id| id.starts_with(MANAGED_PROVIDER_PREFIX))
        .cloned()
        .collect();
    let desired_ids: BTreeSet<String> = desired.providers.keys().cloned().collect();
    let removed_provider_count = existing_managed.difference(&desired_ids).count();

    providers.retain(|id, _| !id.starts_with(MANAGED_PROVIDER_PREFIX));
    for (id, provider) in &desired.providers {
        providers.insert(id.clone(), provider.clone());
    }

    let updated = original != root;
    if updated {
        write_models_file_atomically(&path, &root)?;
    } else if path.exists() {
        set_file_permissions(&path).map_err(PiModelsError::Permissions)?;
    }

    Ok(PiModelsSyncResult {
        updated,
        managed_provider_count: desired.providers.len(),
        removed_provider_count,
        skipped: desired.skipped,
    })
}

fn read_models_root(path: &Path) -> Result<Value, PiModelsError> {
    if !path.exists() {
        return Ok(serde_json::json!({ "providers": {} }));
    }

    let content = fs::read(path).map_err(|source| PiModelsError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value =
        serde_json::from_slice(&content).map_err(|error| PiModelsError::InvalidJson {
            path: path.to_path_buf(),
            line: error.line(),
            column: error.column(),
        })?;
    let root = value
        .as_object()
        .ok_or_else(|| PiModelsError::InvalidStructure {
            path: path.to_path_buf(),
            reason: "the root value must be an object",
        })?;
    if root
        .get("providers")
        .is_some_and(|providers| !providers.is_object())
    {
        return Err(PiModelsError::InvalidStructure {
            path: path.to_path_buf(),
            reason: "providers must be an object",
        });
    }
    Ok(value)
}

fn build_desired_providers(config: &CliConfig) -> DesiredProviders {
    let mut desired = DesiredProviders {
        providers: BTreeMap::new(),
        skipped: Vec::new(),
    };
    let Some(providers) = &config.provider.custom_providers else {
        return desired;
    };

    let mut provider_ids = providers.keys().collect::<Vec<_>>();
    provider_ids.sort();
    for provider_id in provider_ids {
        let provider = &providers[provider_id];
        let Some(api) = pi_api_for_sdk(provider.npm.as_deref()) else {
            desired.skipped.push(PiModelsSkippedProvider {
                provider_id: provider_id.clone(),
                reason: unsupported_sdk_reason(provider.npm.as_deref()),
            });
            continue;
        };

        let pi_provider = build_pi_provider(provider, api);
        let value = serde_json::to_value(pi_provider)
            .expect("Pi provider configuration contains only serializable fields");
        desired
            .providers
            .insert(format!("{MANAGED_PROVIDER_PREFIX}{provider_id}"), value);
    }
    desired
}

fn pi_api_for_sdk(npm: Option<&str>) -> Option<&'static str> {
    let npm = npm?.trim().to_ascii_lowercase();
    match npm.as_str() {
        "@ai-sdk/anthropic" => Some("anthropic-messages"),
        "@ai-sdk/google" => Some("google-generative-ai"),
        "@ai-sdk/openai"
        | "@ai-sdk/openai-compatible"
        | "@ai-sdk/deepinfra"
        | "@ai-sdk/groq"
        | "@ai-sdk/perplexity"
        | "@ai-sdk/togetherai"
        | "@ai-sdk/xai"
        | "@openrouter/ai-sdk-provider" => Some("openai-completions"),
        _ if npm.contains("openrouter") => Some("openai-completions"),
        _ => None,
    }
}

fn unsupported_sdk_reason(npm: Option<&str>) -> String {
    match npm.map(str::trim).filter(|npm| !npm.is_empty()) {
        Some(npm) => format!("unsupported SDK package: {npm}"),
        None => "missing SDK package".to_string(),
    }
}

fn build_pi_provider(provider: &CustomProviderEntry, api: &'static str) -> PiProviderConfig {
    let mut models = provider
        .models
        .as_ref()
        .map(|models| models.iter().collect::<Vec<_>>())
        .unwrap_or_default();
    models.sort_by(|(left, _), (right, _)| left.cmp(right));

    PiProviderConfig {
        base_url: non_empty(provider.options.base_url.as_deref()),
        api_key: non_empty(provider.options.api_key.as_deref())
            .map(|api_key| encode_pi_config_literal(&api_key)),
        api,
        models: models
            .into_iter()
            .map(|(id, model)| build_pi_model(id, model))
            .collect(),
    }
}

/// Encode a secret so Pi's config resolver returns it as a literal value.
fn encode_pi_config_literal(value: &str) -> String {
    let escaped_dollars = value.replace('$', "$$");
    match escaped_dollars.strip_prefix('!') {
        Some(rest) => format!("$!{rest}"),
        None => escaped_dollars,
    }
}

fn build_pi_model(id: &str, model: &CustomModelConfig) -> PiModelConfig {
    let input = model
        .modalities
        .as_ref()
        .and_then(|modalities| modalities.input.as_ref())
        .map(|input| {
            input
                .iter()
                .filter(|modality| matches!(modality.as_str(), "text" | "image"))
                .cloned()
                .collect::<Vec<_>>()
        })
        .filter(|input| !input.is_empty())
        .unwrap_or_else(|| vec!["text".to_string()]);

    PiModelConfig {
        id: id.to_string(),
        name: non_empty(model.name.as_deref()).unwrap_or_else(|| id.to_string()),
        reasoning: model_supports_reasoning(model),
        input,
        context_window: model
            .limit
            .as_ref()
            .and_then(|limit| limit.context)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW),
        max_tokens: model
            .limit
            .as_ref()
            .and_then(|limit| limit.output)
            .unwrap_or(DEFAULT_MAX_TOKENS),
    }
}

fn model_supports_reasoning(model: &CustomModelConfig) -> bool {
    let Some(options) = model.options.as_ref().and_then(Value::as_object) else {
        return false;
    };
    options
        .get("reasoning")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || options
            .get("thinking")
            .and_then(Value::as_object)
            .and_then(|thinking| thinking.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "enabled")
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn write_models_file_atomically(path: &Path, value: &Value) -> Result<(), PiModelsError> {
    write_models_file_with(path, value, replace_file_atomically)
}

#[cfg(windows)]
fn replace_file_atomically(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::{iter, os::windows::ffi::OsStrExt};

    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from = from
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomically(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

fn write_models_file_with<F>(path: &Path, value: &Value, replace: F) -> Result<(), PiModelsError>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    write_models_file_with_hooks(path, value, |_| Ok(()), replace)
}

fn write_models_file_with_hooks<B, R>(
    path: &Path,
    value: &Value,
    before_write: B,
    replace: R,
) -> Result<(), PiModelsError>
where
    B: FnOnce(&Path) -> std::io::Result<()>,
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let parent = path
        .parent()
        .ok_or_else(|| PiModelsError::CreateDirectory {
            path: path.to_path_buf(),
            source: std::io::Error::other("models path has no parent directory"),
        })?;
    fs::create_dir_all(parent).map_err(|source| PiModelsError::CreateDirectory {
        path: parent.to_path_buf(),
        source,
    })?;

    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| MODELS_FILE_NAME.to_string());
    let temp_path = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let mut content = serde_json::to_vec_pretty(value)
        .expect("Pi models value was validated before serialization");
    content.push(b'\n');

    let write_result = (|| -> Result<(), PiModelsError> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(PiModelsError::CreateTemporary)?;
        before_write(&temp_path).map_err(PiModelsError::WriteTemporary)?;
        file.write_all(&content)
            .map_err(PiModelsError::WriteTemporary)?;
        file.sync_all().map_err(PiModelsError::WriteTemporary)?;
        drop(file);
        set_file_permissions(&temp_path).map_err(PiModelsError::Permissions)?;

        let persisted = fs::read(&temp_path).map_err(PiModelsError::WriteTemporary)?;
        let verified: Value =
            serde_json::from_slice(&persisted).map_err(|_| PiModelsError::ValidateTemporary)?;
        if verified != *value {
            return Err(PiModelsError::ValidateTemporary);
        }

        replace(&temp_path, path).map_err(PiModelsError::AtomicReplace)?;
        set_file_permissions(path).map_err(PiModelsError::Permissions)?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(PiModelsError::WriteTemporary)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn set_file_permissions(_path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, process::Command};

    use serde_json::json;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::*;
    use crate::services::cli_config::{
        CustomProviderOptions, ModelLimits, ModelModalities, ProviderConfig,
    };

    const TEST_SECRET: &str = "pi-test-secret-never-leak";

    fn model(name: &str, reasoning: bool) -> CustomModelConfig {
        CustomModelConfig {
            name: Some(name.to_string()),
            modalities: Some(ModelModalities {
                input: Some(vec!["text".into(), "image".into()]),
                output: Some(vec!["text".into()]),
            }),
            options: reasoning.then(|| json!({ "thinking": { "type": "enabled" } })),
            limit: Some(ModelLimits {
                context: Some(200_000),
                output: Some(8_192),
            }),
        }
    }

    fn provider(id: &str, npm: &str, model_id: &str) -> CustomProviderEntry {
        CustomProviderEntry {
            id: id.to_string(),
            name: Some(format!("{id} display")),
            npm: Some(npm.to_string()),
            options: CustomProviderOptions {
                base_url: Some(format!("https://{id}.example.test/v1")),
                api_key: Some(TEST_SECRET.to_string()),
                timeout: None,
            },
            models: Some(HashMap::from([(
                model_id.to_string(),
                model(model_id, true),
            )])),
        }
    }

    fn config(providers: HashMap<String, CustomProviderEntry>) -> CliConfig {
        let mut config = CliConfig::default_config();
        config.provider = ProviderConfig {
            custom_providers: Some(providers),
            ..config.provider
        };
        config
    }

    fn read_json(agent_dir: &Path) -> Value {
        serde_json::from_slice(&fs::read(agent_dir.join(MODELS_FILE_NAME)).unwrap()).unwrap()
    }

    fn resolve_with_local_pi_083_fixture(
        encoded: &[String],
        temp: &TempDir,
    ) -> (Vec<String>, Vec<String>) {
        let input_path = temp.path().join("pi-literal-fixture-input.json");
        let output_path = temp.path().join("pi-literal-fixture-output.json");
        let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/services/pi_models/pi_0_83_resolve_config_value_fixture.mjs");
        fs::write(&input_path, serde_json::to_vec(encoded).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&input_path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let output = Command::new("node")
            .arg(&fixture_path)
            .arg(&input_path)
            .arg(&output_path)
            .output()
            .expect("run repository-local Pi 0.83 config resolver fixture");
        assert!(
            output.status.success(),
            "repository-local Pi 0.83 fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let fixture_output: Value =
            serde_json::from_slice(&fs::read(&output_path).unwrap()).unwrap();
        let resolved = fixture_output["resolved"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        let commands = fixture_output["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_string())
            .collect();
        (resolved, commands)
    }

    fn resolve_with_fixed_pi_083(encoded: &[String], temp: &TempDir) -> Vec<String> {
        let input_path = temp.path().join("pi-literal-input.json");
        let output_path = temp.path().join("pi-literal-output.json");
        let script_path = temp.path().join("resolve-pi-literals.mjs");
        fs::write(&input_path, serde_json::to_vec(encoded).unwrap()).unwrap();
        fs::write(
            &script_path,
            r#"import { readFileSync, realpathSync, writeFileSync } from "node:fs";
import { delimiter, dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

const [inputPath, outputPath] = process.argv.slice(2);
const piName = process.platform === "win32" ? "pi.cmd" : "pi";
const binDir = (process.env.PATH ?? "").split(delimiter)
  .find((entry) => {
    try { realpathSync(join(entry, piName)); return true; } catch { return false; }
  });
if (!binDir) throw new Error("fixed Pi executable is unavailable");
const cliPath = realpathSync(join(binDir, piName));
const resolverPath = join(dirname(cliPath), "core", "resolve-config-value.js");
const { resolveConfigValueUncached } = await import(pathToFileURL(resolverPath));
const values = JSON.parse(readFileSync(inputPath, "utf8"));
const resolved = values.map((value) => resolveConfigValueUncached(value, { ENV: "expanded-by-pi" }));
writeFileSync(outputPath, JSON.stringify(resolved), { mode: 0o600 });
"#,
        )
        .unwrap();
        #[cfg(unix)]
        for path in [&input_path, &script_path] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let package = format!(
            "{}@{}",
            executors::executors::pi::PI_CODING_AGENT_PACKAGE,
            executors::executors::pi::PI_CODING_AGENT_VERSION
        );
        let output = Command::new("npx")
            .args(["--offline", "--yes", "--package", &package, "--", "node"])
            .arg(&script_path)
            .arg(&input_path)
            .arg(&output_path)
            .output()
            .expect("run fixed Pi config resolver");
        assert!(
            output.status.success(),
            "fixed Pi resolver failed without exposing input values: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap()
    }

    #[test]
    fn pi_api_keys_are_encoded_as_literals_for_fixed_pi_083() {
        let temp = TempDir::new().unwrap();
        let command_sentinel = temp.path().join("must-not-exist");
        let fixture_controls = vec![
            "!literal".to_string(),
            "$ENV".to_string(),
            "${ENV}".to_string(),
            "prefix-$ENV-suffix".to_string(),
        ];
        let (control_values, control_commands) =
            resolve_with_local_pi_083_fixture(&fixture_controls, &temp);
        assert_eq!(
            control_values,
            [
                "unexpected-command-result",
                "expanded-by-pi",
                "expanded-by-pi",
                "prefix-expanded-by-pi-suffix",
            ]
        );
        assert_eq!(control_commands, ["literal"]);

        let raw = vec![
            "!literal".to_string(),
            "$ENV".to_string(),
            "${ENV}".to_string(),
            "prefix-$ENV-suffix".to_string(),
            "!both$ENV!${ENV}$".to_string(),
            format!("!touch {}", command_sentinel.display()),
        ];
        let encoded = raw
            .iter()
            .map(|value| encode_pi_config_literal(value))
            .collect::<Vec<_>>();

        assert_eq!(
            &encoded[..5],
            [
                "$!literal",
                "$$ENV",
                "$${ENV}",
                "prefix-$$ENV-suffix",
                "$!both$$ENV!$${ENV}$$",
            ]
        );
        assert!(encoded.iter().all(|value| !value.starts_with('!')));

        let agent_dir = temp.path().join("isolated-home/.pi/agent");
        let providers = raw
            .iter()
            .enumerate()
            .map(|(index, api_key)| {
                let id = format!("literal-{index}");
                let mut entry = provider(&id, "@ai-sdk/openai", "test-model");
                entry.options.api_key = Some(api_key.clone());
                (id, entry)
            })
            .collect::<HashMap<_, _>>();
        let literal_config = config(providers);
        let result = synchronize_pi_models_in_agent_dir(&literal_config, &agent_dir).unwrap();
        let stored = (0..raw.len())
            .map(|index| {
                read_json(&agent_dir)["providers"][format!("openteams-literal-{index}")]["apiKey"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(stored, encoded);
        let (resolved, commands) = resolve_with_local_pi_083_fixture(&stored, &temp);
        assert_eq!(resolved, raw);
        assert!(commands.is_empty());
        assert!(!command_sentinel.exists());
        let repeated = synchronize_pi_models_in_agent_dir(&literal_config, &agent_dir).unwrap();
        assert!(!repeated.updated);
        let repeated_json = read_json(&agent_dir);
        let repeated_stored = (0..raw.len())
            .map(|index| {
                repeated_json["providers"][format!("openteams-literal-{index}")]["apiKey"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        assert_eq!(stored, repeated_stored);
        let response = serde_json::to_string(&result).unwrap();
        assert!(raw.iter().all(|value| !response.contains(value)));
        assert!(encoded.iter().all(|value| !response.contains(value)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(agent_dir.join(MODELS_FILE_NAME))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        fs::write(agent_dir.join(MODELS_FILE_NAME), b"invalid json").unwrap();
        let error = synchronize_pi_models_in_agent_dir(&literal_config, &agent_dir).unwrap_err();
        let diagnostic = PiModelsSyncDiagnostic::failure(error.into());
        let diagnostic_json = serde_json::to_string(&diagnostic).unwrap();
        assert!(raw.iter().all(|value| !diagnostic_json.contains(value)));
        assert!(encoded.iter().all(|value| !diagnostic_json.contains(value)));
    }

    #[test]
    #[ignore = "requires @earendil-works/pi-coding-agent@0.83.0 in the npm cache or network access"]
    fn pi_api_keys_round_trip_with_real_fixed_pi_083_resolver() {
        let temp = TempDir::new().unwrap();
        let raw = vec![
            "!literal".to_string(),
            "$ENV".to_string(),
            "${ENV}".to_string(),
            "prefix-$ENV-suffix".to_string(),
            "!both$ENV!${ENV}$".to_string(),
        ];
        let encoded = raw
            .iter()
            .map(|value| encode_pi_config_literal(value))
            .collect::<Vec<_>>();

        assert_eq!(resolve_with_fixed_pi_083(&encoded, &temp), raw);
    }

    #[test]
    fn pi_models_conversion_maps_supported_sdks_and_model_fields() {
        let config = config(HashMap::from([
            (
                "anthropic-proxy".into(),
                provider("anthropic-proxy", "@ai-sdk/anthropic", "claude-test"),
            ),
            (
                "google-proxy".into(),
                provider("google-proxy", "@ai-sdk/google", "gemini-test"),
            ),
            (
                "openai-proxy".into(),
                provider("openai-proxy", "@ai-sdk/openai-compatible", "gpt-test"),
            ),
            (
                "unsupported".into(),
                provider("unsupported", "@ai-sdk/azure", "azure-test"),
            ),
        ]));

        let desired = build_desired_providers(&config);

        assert_eq!(desired.providers.len(), 3);
        assert_eq!(
            desired.providers["openteams-anthropic-proxy"]["api"],
            "anthropic-messages"
        );
        assert_eq!(
            desired.providers["openteams-google-proxy"]["api"],
            "google-generative-ai"
        );
        assert_eq!(
            desired.providers["openteams-openai-proxy"]["api"],
            "openai-completions"
        );
        let mapped = &desired.providers["openteams-openai-proxy"];
        assert_eq!(mapped["baseUrl"], "https://openai-proxy.example.test/v1");
        assert_eq!(mapped["apiKey"], TEST_SECRET);
        assert_eq!(mapped["models"][0]["input"], json!(["text", "image"]));
        assert_eq!(mapped["models"][0]["contextWindow"], 200_000);
        assert_eq!(mapped["models"][0]["maxTokens"], 8_192);
        assert_eq!(mapped["models"][0]["reasoning"], true);
        assert_eq!(desired.skipped.len(), 1);
        assert_eq!(desired.skipped[0].provider_id, "unsupported");
        assert!(desired.skipped[0].reason.contains("@ai-sdk/azure"));
        assert!(!format!("{:?}", desired.skipped).contains(TEST_SECRET));
    }

    #[test]
    fn pi_models_sync_creates_updates_deletes_and_is_idempotent_while_preserving_others() {
        let temp = TempDir::new().unwrap();
        let agent_dir = temp.path().join("home").join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::write(
            agent_dir.join(MODELS_FILE_NAME),
            serde_json::to_vec_pretty(&json!({
                "schemaVersion": 7,
                "providers": {
                    "same-id": { "keep": "unprefixed" },
                    "anthropic": { "keep": "pi builtin override" },
                    "third-party": { "keep": true },
                    "openteams-stale": { "remove": true },
                    "openteams-same-id": { "old": true }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let first_config = config(HashMap::from([(
            "same-id".into(),
            provider("same-id", "@ai-sdk/openai", "model-v1"),
        )]));
        let first = synchronize_pi_models_in_agent_dir(&first_config, &agent_dir).unwrap();
        assert!(first.updated);
        assert_eq!(first.removed_provider_count, 1);
        let created = read_json(&agent_dir);
        assert_eq!(created["schemaVersion"], 7);
        assert_eq!(created["providers"]["same-id"]["keep"], "unprefixed");
        assert_eq!(
            created["providers"]["anthropic"]["keep"],
            "pi builtin override"
        );
        assert_eq!(created["providers"]["third-party"]["keep"], true);
        assert!(created["providers"].get("openteams-stale").is_none());
        assert_eq!(
            created["providers"]["openteams-same-id"]["models"][0]["id"],
            "model-v1"
        );

        let repeated = synchronize_pi_models_in_agent_dir(&first_config, &agent_dir).unwrap();
        assert!(!repeated.updated);
        assert_eq!(created, read_json(&agent_dir));

        let updated_config = config(HashMap::from([(
            "same-id".into(),
            provider("same-id", "@ai-sdk/openai", "model-v2"),
        )]));
        let updated = synchronize_pi_models_in_agent_dir(&updated_config, &agent_dir).unwrap();
        assert!(updated.updated);
        assert_eq!(
            read_json(&agent_dir)["providers"]["openteams-same-id"]["models"][0]["id"],
            "model-v2"
        );

        let deleted =
            synchronize_pi_models_in_agent_dir(&config(HashMap::new()), &agent_dir).unwrap();
        assert!(deleted.updated);
        assert_eq!(deleted.removed_provider_count, 1);
        let final_json = read_json(&agent_dir);
        assert!(final_json["providers"].get("openteams-same-id").is_none());
        assert_eq!(final_json["providers"]["same-id"]["keep"], "unprefixed");
        assert_eq!(final_json["providers"]["third-party"]["keep"], true);
    }

    #[test]
    fn pi_models_invalid_json_preserves_original_bytes_and_hash() {
        let temp = TempDir::new().unwrap();
        let agent_dir = temp.path().join("isolated-home").join(".pi").join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let path = agent_dir.join(MODELS_FILE_NAME);
        let invalid = format!("{{\"providers\":{{\"secret\":\"{TEST_SECRET}\"}}");
        fs::write(&path, invalid.as_bytes()).unwrap();
        let before_hash = Sha256::digest(fs::read(&path).unwrap());

        let error = synchronize_pi_models_in_agent_dir(&config(HashMap::new()), &agent_dir)
            .expect_err("invalid JSON must fail");

        let after = fs::read(&path).unwrap();
        let after_hash = Sha256::digest(&after);
        assert_eq!(before_hash, after_hash);
        assert_eq!(after, invalid.as_bytes());
        assert!(!error.to_string().contains(TEST_SECRET));
    }

    #[test]
    fn pi_models_rename_failure_preserves_old_file_and_removes_temporary_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("isolated-home/.pi/agent/models.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = br#"{"providers":{"third-party":{"keep":true}}}"#;
        fs::write(&path, original).unwrap();
        let replacement = json!({ "providers": { "openteams-new": {} } });

        let error = write_models_file_with(&path, &replacement, |_from, _to| {
            Err(std::io::Error::other("simulated rename failure"))
        })
        .expect_err("rename failure must be reported");

        assert!(matches!(error, PiModelsError::AtomicReplace(_)));
        assert_eq!(fs::read(&path).unwrap(), original);
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn pi_models_write_failure_preserves_old_file_and_removes_temporary_file() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("isolated-home/.pi/agent/models.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = br#"{"providers":{"third-party":{"keep":true}}}"#;
        fs::write(&path, original).unwrap();
        let replacement = json!({ "providers": { "openteams-new": {} } });

        let error = write_models_file_with_hooks(
            &path,
            &replacement,
            |_temp_path| Err(std::io::Error::other("simulated write failure")),
            |from, to| fs::rename(from, to),
        )
        .expect_err("write failure must be reported");

        assert!(matches!(error, PiModelsError::WriteTemporary(_)));
        assert_eq!(fs::read(&path).unwrap(), original);
        let leftovers = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[cfg(unix)]
    #[test]
    fn pi_models_temporary_and_target_files_use_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let path = temp.path().join("isolated-home/.pi/agent/models.json");
        let value = json!({ "providers": { "openteams-test": {} } });
        let mut temporary_mode = None;

        write_models_file_with(&path, &value, |from, to| {
            temporary_mode = Some(fs::metadata(from)?.permissions().mode() & 0o777);
            fs::rename(from, to)
        })
        .unwrap();

        assert_eq!(temporary_mode, Some(0o600));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn pi_models_sync_result_and_errors_never_expose_api_keys() {
        let temp = TempDir::new().unwrap();
        let agent_dir = temp.path().join("isolated-home/.pi/agent");
        let config = config(HashMap::from([(
            "safe".into(),
            provider("safe", "@ai-sdk/openai", "safe-model"),
        )]));

        let result = synchronize_pi_models_in_agent_dir(&config, &agent_dir).unwrap();
        let api_output = serde_json::to_string(&result).unwrap();
        assert!(!api_output.contains(TEST_SECRET));
        assert!(!format!("{result:?}").contains(TEST_SECRET));

        fs::write(agent_dir.join(MODELS_FILE_NAME), b"invalid json").unwrap();
        let error = synchronize_pi_models_in_agent_dir(&config, &agent_dir).unwrap_err();
        assert!(!error.to_string().contains(TEST_SECRET));
        assert!(
            fs::read_to_string(agent_dir.join(MODELS_FILE_NAME))
                .unwrap()
                .contains("invalid json")
        );
    }

    #[tokio::test]
    async fn saved_config_coordination_is_service_owned_and_normalizes_legacy_providers() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("isolated-home/.openteams/config.toml");
        let agent_dir = temp.path().join("isolated-home/.pi/agent");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let saved = config(HashMap::from([(
            "litellm".into(),
            provider("litellm", "@ai-sdk/anthropic", "proxy-model"),
        )]));
        fs::write(&config_path, toml::to_string_pretty(&saved).unwrap()).unwrap();

        let result = coordinate_pi_models_from_paths(&config_path, &agent_dir)
            .await
            .expect("coordinate saved config");

        assert!(result.updated);
        assert_eq!(
            read_json(&agent_dir)["providers"]["openteams-litellm"]["api"],
            "openai-completions"
        );
    }

    #[tokio::test]
    async fn saved_config_diagnostic_is_structured_retryable_and_secret_safe() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("isolated-home/.openteams/config.toml");
        let agent_dir = temp.path().join("isolated-home/.pi/agent");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(
            &config_path,
            format!("[provider\napi_key = \"{TEST_SECRET}\""),
        )
        .unwrap();

        let error = coordinate_pi_models_from_paths(&config_path, &agent_dir)
            .await
            .expect_err("invalid saved config must fail");
        let diagnostic = PiModelsSyncDiagnostic::failure(error);
        let serialized = serde_json::to_string(&diagnostic).unwrap();

        assert!(!diagnostic.synchronized);
        assert!(diagnostic.retry_available);
        assert_eq!(diagnostic.retry_path, PI_MODELS_SYNC_RETRY_PATH);
        assert!(diagnostic.error.is_some());
        assert!(!serialized.contains(TEST_SECRET));
        assert!(!agent_dir.join(MODELS_FILE_NAME).exists());
    }
}
