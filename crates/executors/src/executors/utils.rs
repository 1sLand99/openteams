use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use lru::LruCache;

use super::SlashCommandDescription;
use crate::executors::BaseCodingAgent;

pub fn read_json_file(path: &Path) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_json_or_jsonc(&content)
}

fn parse_json_or_jsonc(content: &str) -> Option<serde_json::Value> {
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    serde_json::from_str(content).ok().or_else(|| {
        jsonc_parser::parse_to_serde_value(content, &jsonc_parser::ParseOptions::default())
            .ok()
            .flatten()
    })
}

pub fn json_has_nonempty_string(value: &serde_json::Value, pointers: &[&str]) -> bool {
    pointers.iter().any(|pointer| {
        value
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    })
}

pub fn dotenv_has_nonempty_value(path: &Path, keys: &[&str]) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.lines().any(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return false;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        keys.contains(&key.trim()) && !value.trim().trim_matches(['\'', '"']).is_empty()
    })
}

#[cfg(test)]
mod auth_tests {
    use super::parse_json_or_jsonc;

    #[test]
    fn json_reader_accepts_jsonc_auth_stores() {
        let value = parse_json_or_jsonc(
            r#"{
                // Managed by the CLI.
                "loggedInUsers": [{ "login": "octocat" }],
            }"#,
        )
        .expect("parse JSONC auth store");

        assert_eq!(value["loggedInUsers"][0]["login"], "octocat");
    }

    #[test]
    fn json_reader_rejects_empty_or_malformed_auth_stores() {
        assert!(parse_json_or_jsonc(" \n\t").is_none());
        assert!(parse_json_or_jsonc("{").is_none());
    }
}

/// Parsed slash command with name and arguments.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommandCall<'a> {
    /// The command name in lowercase (without the leading slash)
    pub name: String,
    /// The arguments after the command name
    pub arguments: &'a str,
}

/// Parse a slash command from a prompt string.
///
/// Returns `Some(T)` if the prompt starts with a slash command,
/// or `None` if it doesn't look like a slash command.
///
/// The return type `T` must implement `From<SlashCommandCall>`.
pub fn parse_slash_command<'a, T>(prompt: &'a str) -> Option<T>
where
    T: From<SlashCommandCall<'a>>,
{
    let trimmed = prompt.trim_start();
    let without_slash = trimmed.strip_prefix('/')?;
    let mut parts = without_slash.splitn(2, |ch: char| ch.is_whitespace());
    let name = parts.next()?.trim().to_lowercase();
    if name.is_empty() {
        return None;
    }
    let arguments = parts.next().map(|s| s.trim()).unwrap_or("");
    Some(T::from(SlashCommandCall { name, arguments }))
}

pub const SLASH_COMMANDS_CACHE_CAPACITY: usize = 32;
const TTL: Duration = Duration::from_secs(60 * 5);

/// Reorder slash commands to prioritize compact then review.
#[must_use]
pub fn reorder_slash_commands(
    commands: impl IntoIterator<Item = SlashCommandDescription>,
) -> Vec<SlashCommandDescription> {
    let mut compact_command = None;
    let mut review_commands = None;
    let mut remaining_commands = Vec::new();

    for command in commands {
        match command.name.as_str() {
            "compact" => compact_command = Some(command),
            "review" => review_commands = Some(command),
            _ => remaining_commands.push(command),
        }
    }

    compact_command
        .into_iter()
        .chain(review_commands)
        .chain(remaining_commands)
        .collect()
}

/// Executors can use this key to cache expensive slash command retrievals.
pub struct SlashCommandCache {
    cache: Mutex<LruCache<SlashCommandCacheKey, CachedEntry>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SlashCommandCacheKey {
    path: PathBuf,
    executor_id: String,
}

impl SlashCommandCacheKey {
    /// Create a new cache key for an executor.
    pub fn new(path: impl Into<PathBuf>, executor: &BaseCodingAgent) -> Self {
        Self {
            path: path.into(),
            executor_id: executor.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct CachedEntry {
    cached_at: Instant,
    commands: Arc<Vec<SlashCommandDescription>>,
}

impl SlashCommandCache {
    pub fn instance() -> &'static Self {
        static INSTANCE: OnceLock<SlashCommandCache> = OnceLock::new();
        INSTANCE.get_or_init(|| Self {
            cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(SLASH_COMMANDS_CACHE_CAPACITY).unwrap(),
            )),
        })
    }

    /// Get cached slash commands for the given key.
    #[must_use]
    pub fn get(&self, key: &SlashCommandCacheKey) -> Option<Arc<Vec<SlashCommandDescription>>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let entry = cache.get(key)?;
        if entry.cached_at.elapsed() > TTL {
            cache.pop(key);
            None
        } else {
            Some(entry.commands.clone())
        }
    }

    /// Store slash commands in the cache.
    pub fn put(&self, key: SlashCommandCacheKey, commands: Vec<SlashCommandDescription>) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.put(
            key,
            CachedEntry {
                cached_at: Instant::now(),
                commands: Arc::new(commands),
            },
        );
    }
}
