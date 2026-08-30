use std::{collections::HashMap, path::PathBuf, sync::Arc};

use git::GitService;
use tokio::process::Command;

use crate::command::CmdOverrides;

#[derive(Clone, Default)]
pub struct SensitiveValueRedactor {
    values: Arc<Vec<String>>,
}

impl std::fmt::Debug for SensitiveValueRedactor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SensitiveValueRedactor")
            .field("value_count", &self.values.len())
            .finish()
    }
}

impl SensitiveValueRedactor {
    pub fn from_env(env: &HashMap<String, String>) -> Self {
        let mut values = env
            .iter()
            .filter_map(|(name, value)| {
                let value = value.trim();
                (is_sensitive_env_name(name)
                    && !value.is_empty()
                    && (value.len() >= 4 || name.eq_ignore_ascii_case("KIRO_API_KEY")))
                .then(|| value.to_string())
            })
            .collect::<Vec<_>>();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self {
            values: Arc::new(values),
        }
    }

    pub(crate) fn with_sensitive_values<I, S>(self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut merged = self.values.as_ref().clone();
        merged.extend(values.into_iter().filter_map(|value| {
            let value = value.as_ref().trim();
            (!value.is_empty()).then(|| value.to_string())
        }));
        merged.sort_by_key(|value| std::cmp::Reverse(value.len()));
        merged.dedup();
        Self {
            values: Arc::new(merged),
        }
    }

    pub fn redact(&self, value: &str) -> String {
        self.values.iter().fold(value.to_string(), |safe, secret| {
            safe.replace(secret, "[redacted]")
        })
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn redact_json(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => *text = self.redact(text),
            serde_json::Value::Array(values) => {
                for value in values {
                    self.redact_json(value);
                }
            }
            serde_json::Value::Object(fields) => {
                let original = std::mem::take(fields);
                for (key, mut value) in original {
                    self.redact_json(&mut value);
                    // Short explicit secrets must redact every JSON value, but replacing a
                    // one-character substring in serde's structural field names would make the
                    // typed ACP event impossible to deserialize. Preserve the established key
                    // redaction for normal-length secrets while keeping short values on the
                    // value-level path required by the protocol bridge.
                    let safe_key = self
                        .values
                        .iter()
                        .filter(|secret| secret.len() >= 4)
                        .fold(key, |safe, secret| safe.replace(secret, "[redacted]"));
                    fields.insert(safe_key, value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    pub(crate) fn stream(&self) -> SensitiveValueStreamRedactor {
        SensitiveValueStreamRedactor {
            values: self.values.clone(),
            pending: Vec::new(),
        }
    }
}

pub(crate) struct SensitiveValueStreamRedactor {
    values: Arc<Vec<String>>,
    pending: Vec<u8>,
}

impl SensitiveValueStreamRedactor {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        let mut output = Vec::new();

        loop {
            if let Some((start, length)) = self.earliest_secret() {
                output.extend(self.pending.drain(..start));
                output.extend_from_slice(b"[redacted]");
                self.pending.drain(..length);
                continue;
            }

            let retained = self.longest_secret_prefix_suffix();
            let emit_length = self.pending.len().saturating_sub(retained);
            output.extend(self.pending.drain(..emit_length));
            return output;
        }
    }

    pub(crate) fn finish(&mut self) -> Vec<u8> {
        let mut output = self.push(&[]);
        output.append(&mut self.pending);
        output
    }

    fn earliest_secret(&self) -> Option<(usize, usize)> {
        let mut earliest = None;
        for secret in self.values.iter() {
            let secret = secret.as_bytes();
            let Some(start) = find_bytes(&self.pending, secret) else {
                continue;
            };
            if earliest.is_none_or(|(best_start, best_length)| {
                start < best_start || (start == best_start && secret.len() > best_length)
            }) {
                earliest = Some((start, secret.len()));
            }
        }
        earliest
    }

    fn longest_secret_prefix_suffix(&self) -> usize {
        let mut retained = 0;
        for secret in self.values.iter() {
            let secret = secret.as_bytes();
            let max_length = self.pending.len().min(secret.len().saturating_sub(1));
            for length in (1..=max_length).rev() {
                if self.pending.ends_with(&secret[..length]) {
                    retained = retained.max(length);
                    break;
                }
            }
        }
        retained
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

pub fn is_sensitive_env_name(name: &str) -> bool {
    let normalized = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "PRIVATE_KEY",
        "CREDENTIAL",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|fragment| normalized.contains(fragment))
}

/// Repository context for executor operations
#[derive(Debug, Clone, Default)]
pub struct RepoContext {
    pub workspace_root: PathBuf,
    /// Names of repositories in the workspace (subdirectory names)
    pub repo_names: Vec<String>,
}

impl RepoContext {
    pub fn new(workspace_root: PathBuf, repo_names: Vec<String>) -> Self {
        Self {
            workspace_root,
            repo_names,
        }
    }

    pub fn repo_paths(&self) -> Vec<PathBuf> {
        self.repo_names
            .iter()
            .map(|name| self.workspace_root.join(name))
            .collect()
    }

    /// Check all repos for uncommitted changes.
    /// Returns a formatted string describing any uncommitted changes found,
    /// or an empty string if all repos are clean.
    pub async fn check_uncommitted_changes(&self) -> String {
        let repo_paths = self.repo_paths();
        if repo_paths.is_empty() {
            return String::new();
        }

        tokio::task::spawn_blocking(move || {
            let git = GitService::new();
            let mut all_status = String::new();

            for repo_path in &repo_paths {
                // Skip if not a git repository
                if !repo_path.join(".git").exists() {
                    continue;
                }

                match git.get_worktree_status(repo_path) {
                    Ok(status) if !status.entries.is_empty() => {
                        let mut status_output = String::new();
                        for entry in &status.entries {
                            status_output.push(entry.staged);
                            status_output.push(entry.unstaged);
                            status_output.push(' ');
                            status_output.push_str(&String::from_utf8_lossy(&entry.path));
                            status_output.push('\n');
                        }
                        all_status.push_str(&format!(
                            "\n{}:\n{}",
                            repo_path.display(),
                            status_output
                        ));
                    }
                    _ => {}
                }
            }

            all_status
        })
        .await
        .unwrap_or_default()
    }
}

/// Environment variables to inject into executor processes
#[derive(Debug, Clone)]
pub struct ExecutionEnv {
    pub vars: HashMap<String, String>,
    pub repo_context: RepoContext,
    pub commit_reminder: bool,
    pub commit_reminder_prompt: String,
}

impl ExecutionEnv {
    pub fn new(
        repo_context: RepoContext,
        commit_reminder: bool,
        commit_reminder_prompt: String,
    ) -> Self {
        Self {
            vars: HashMap::new(),
            repo_context,
            commit_reminder,
            commit_reminder_prompt,
        }
    }

    /// Insert an environment variable
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.vars.insert(key.into(), value.into());
    }

    /// Merge additional vars into this env. Incoming keys overwrite existing ones.
    pub fn merge(&mut self, other: &HashMap<String, String>) {
        self.vars
            .extend(other.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    /// Return a new env with overrides applied. Overrides take precedence.
    pub fn with_overrides(mut self, overrides: &HashMap<String, String>) -> Self {
        self.merge(overrides);
        self
    }

    /// Return a new env with profile env from CmdOverrides merged in.
    pub fn with_profile(self, cmd: &CmdOverrides) -> Self {
        if let Some(ref profile_env) = cmd.env {
            self.with_overrides(profile_env)
        } else {
            self
        }
    }

    /// Apply all environment variables to a Command
    pub fn apply_to_command(&self, command: &mut Command) {
        for (key, value) in &self.vars {
            command.env(key, value);
        }
    }

    pub fn sensitive_value_redactor(&self) -> SensitiveValueRedactor {
        let mut vars = std::env::vars().collect::<HashMap<_, _>>();
        vars.extend(
            self.vars
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        SensitiveValueRedactor::from_env(&vars)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.vars.contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.vars.get(key)
    }

    /// Remove an environment variable. No-op if the key is absent.
    pub fn remove(&mut self, key: &str) {
        self.vars.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_overrides_runtime_env() {
        let mut base = ExecutionEnv::new(RepoContext::default(), false, String::new());
        base.insert("VK_PROJECT_NAME", "runtime");
        base.insert("FOO", "runtime");

        let mut profile = HashMap::new();
        profile.insert("FOO".to_string(), "profile".to_string());
        profile.insert("BAR".to_string(), "profile".to_string());

        let merged = base.with_overrides(&profile);

        assert_eq!(merged.vars.get("VK_PROJECT_NAME").unwrap(), "runtime");
        assert_eq!(merged.vars.get("FOO").unwrap(), "profile"); // overrides
        assert_eq!(merged.vars.get("BAR").unwrap(), "profile");
    }

    #[test]
    fn sensitive_value_redactor_masks_api_keys_without_exposing_them_in_debug() {
        let mut env = ExecutionEnv::new(RepoContext::default(), false, String::new());
        env.insert("KIRO_API_KEY", "kiro-fixture-secret");
        env.insert("SHORT_TOKEN", "abc");
        env.insert("SAFE_MARKER", "visible");

        let redactor = env.sensitive_value_redactor();

        assert_eq!(
            redactor.redact("failed with kiro-fixture-secret; abc; visible"),
            "failed with [redacted]; abc; visible"
        );
        let debug = format!("{redactor:?}");
        assert!(!debug.contains("kiro-fixture-secret"));
    }

    #[test]
    fn sensitive_value_redactor_masks_every_nonempty_short_kiro_api_key() {
        for secret in ["a", "ab", "abc"] {
            let redactor = SensitiveValueRedactor::from_env(&HashMap::from([(
                "KIRO_API_KEY".to_string(),
                secret.to_string(),
            )]));

            assert_eq!(redactor.redact(secret), "[redacted]", "secret={secret:?}");
        }
    }

    #[test]
    fn sensitive_value_redactor_masks_every_nonempty_explicit_value() {
        for secret in ["a", "ab", "abc"] {
            let redactor = SensitiveValueRedactor::default().with_sensitive_values([secret]);

            assert_eq!(redactor.redact(secret), "[redacted]", "secret={secret:?}");
        }
    }

    #[test]
    fn streaming_redactor_masks_short_api_key_split_across_chunks() {
        let redactor = SensitiveValueRedactor::from_env(&HashMap::from([(
            "KIRO_API_KEY".to_string(),
            "abc".to_string(),
        )]));
        let mut stream = redactor.stream();
        let mut safe = Vec::new();

        safe.extend(stream.push(b"stderr: a"));
        safe.extend(stream.push(b"b"));
        safe.extend(stream.push(b"c; retry failed"));
        safe.extend(stream.finish());

        assert_eq!(
            String::from_utf8(safe).expect("redacted stderr is UTF-8"),
            "stderr: [redacted]; retry failed"
        );
    }
}
