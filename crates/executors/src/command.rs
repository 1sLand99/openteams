use std::{collections::HashMap, path::PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;
use workspace_utils::shell::{resolve_executable_path, resolve_executable_path_blocking};

use crate::executors::ExecutorError;

#[derive(Debug, Error)]
pub enum CommandBuildError {
    #[error("base command cannot be parsed: {0}")]
    InvalidBase(String),
    #[error("base command is empty after parsing")]
    EmptyCommand,
    #[error("failed to quote command: {0}")]
    QuoteError(#[from] shlex::QuoteError),
    #[error("invalid shell parameters: {0}")]
    InvalidShellParams(String),
}

#[derive(Debug, Clone)]
pub struct CommandParts {
    program: String,
    args: Vec<String>,
}

impl CommandParts {
    pub fn new(program: String, args: Vec<String>) -> Self {
        Self { program, args }
    }

    #[cfg(test)]
    pub(crate) fn into_parts_for_test(self) -> (String, Vec<String>) {
        (self.program, self.args)
    }

    pub async fn into_resolved(self) -> Result<(PathBuf, Vec<String>), ExecutorError> {
        let CommandParts { program, args } = self;
        let executable = resolve_executable_path(&program)
            .await
            .ok_or(ExecutorError::ExecutableNotFound { program })?;
        Ok((executable, args))
    }

    /// Render the exact process invocation for diagnostics while masking
    /// credential-bearing flags. This never includes environment values.
    pub fn redacted_display(&self) -> String {
        redacted_command(&self.program, &self.args)
    }
}

/// Render a process invocation for user-facing diagnostics. Arguments are
/// quoted when needed and common credential flags are masked.
pub fn redacted_command(program: &str, args: &[String]) -> String {
    let mut parts = vec![quote_diagnostic_arg(program)];
    let mut redact_next = false;
    for arg in args {
        let rendered = if redact_next {
            redact_next = false;
            "<redacted>".to_string()
        } else if let Some((name, _)) = arg.split_once('=') {
            if is_sensitive_flag(name) {
                format!("{name}=<redacted>")
            } else {
                quote_diagnostic_arg(arg)
            }
        } else {
            if is_sensitive_flag(arg) {
                redact_next = true;
            }
            quote_diagnostic_arg(arg)
        };
        parts.push(rendered);
    }
    parts.join(" ")
}

fn is_sensitive_flag(value: &str) -> bool {
    let name = value
        .trim_start_matches(['-', '/'])
        .to_ascii_lowercase()
        .replace('_', "-");
    [
        "token",
        "api-key",
        "apikey",
        "password",
        "secret",
        "authorization",
        "credential",
    ]
    .iter()
    .any(|sensitive| name.contains(sensitive))
}

fn quote_diagnostic_arg(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '_' | '-' | '.' | '/' | ':' | '@' | '%' | '+' | '=' | ','
                )
        })
    {
        value.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| "<unprintable>".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema, Default)]
pub struct CmdOverrides {
    #[schemars(
        title = "Base Command Override",
        description = "Override the base command with a custom command"
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_command_override: Option<String>,
    #[schemars(
        title = "Additional Parameters",
        description = "Additional parameters to append to the base command"
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub additional_params: Option<Vec<String>>,
    #[schemars(
        title = "Environment Variables",
        description = "Environment variables to set when running the executor"
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
}

impl CmdOverrides {
    pub(crate) fn parsed_additional_params(&self) -> Result<Vec<String>, CommandBuildError> {
        let joined = self
            .additional_params
            .as_deref()
            .unwrap_or_default()
            .join(" ");

        if joined.trim().is_empty() {
            return Ok(Vec::new());
        }

        split_command_line(&joined)
            .map_err(|err| CommandBuildError::InvalidShellParams(format!("{joined}: {err}")))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS, JsonSchema)]
pub struct CommandBuilder {
    /// Base executable command (e.g., "npx -y @anthropic-ai/claude-code@2.1.161")
    pub base: String,
    /// Optional parameters to append to the base command
    pub params: Option<Vec<String>>,
}

impl CommandBuilder {
    pub fn new<S: Into<String>>(base: S) -> Self {
        Self {
            base: base.into(),
            params: None,
        }
    }

    pub fn params<I>(mut self, params: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        self.params = Some(params.into_iter().map(|p| p.into()).collect());
        self
    }

    pub fn override_base<S: Into<String>>(mut self, base: S) -> Self {
        self.base = base.into();
        self
    }

    pub fn extend_params<I>(mut self, more: I) -> Self
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let extra: Vec<String> = more.into_iter().map(|p| p.into()).collect();
        match &mut self.params {
            Some(p) => p.extend(extra),
            None => self.params = Some(extra),
        }
        self
    }

    pub fn build_initial(&self) -> Result<CommandParts, CommandBuildError> {
        self.build(&[])
    }

    pub fn build_follow_up(
        &self,
        additional_args: &[String],
    ) -> Result<CommandParts, CommandBuildError> {
        self.build(additional_args)
    }

    fn build(&self, additional_args: &[String]) -> Result<CommandParts, CommandBuildError> {
        let mut parts = vec![];
        let base_parts = split_command_line(&self.base)?;
        parts.extend(base_parts);
        if let Some(ref params) = self.params {
            parts.extend(params.clone());
        }
        parts.extend(additional_args.iter().cloned());

        if parts.is_empty() {
            return Err(CommandBuildError::EmptyCommand);
        }

        let program = parts.remove(0);
        Ok(CommandParts::new(program, parts))
    }
}

fn split_command_line(input: &str) -> Result<Vec<String>, CommandBuildError> {
    #[cfg(windows)]
    {
        let parts = winsplit::split(input);
        if parts.is_empty() {
            Err(CommandBuildError::EmptyCommand)
        } else {
            Ok(parts)
        }
    }

    #[cfg(not(windows))]
    {
        shlex::split(input).ok_or_else(|| CommandBuildError::InvalidBase(input.to_string()))
    }
}

/// Validate that a base command override does not contain shell metacharacter sequences
/// that could be used for command injection. This is a defense-in-depth measure: the
/// actual execution path uses shlex/winsplit tokenisation and direct process spawning
/// (no shell involved), so these sequences are already inert — but we reject them
/// proactively in case the execution model ever changes or a tampered profiles.json
/// is loaded.
fn validate_base_command_override(base: &str) -> Result<(), CommandBuildError> {
    const DANGEROUS_PATTERNS: &[&str] = &[";", "&&", "||", "`", "$(", "${", ">>", "<<"];
    for pattern in DANGEROUS_PATTERNS {
        if base.contains(pattern) {
            return Err(CommandBuildError::InvalidShellParams(format!(
                "base command override contains disallowed shell sequence: '{pattern}'"
            )));
        }
    }
    Ok(())
}

pub fn apply_overrides(
    builder: CommandBuilder,
    overrides: &CmdOverrides,
) -> Result<CommandBuilder, CommandBuildError> {
    let builder = if let Some(ref base) = overrides.base_command_override {
        validate_base_command_override(base)?;
        builder.override_base(base.clone())
    } else {
        builder
    };
    let additional_params = overrides.parsed_additional_params()?;
    if additional_params.is_empty() {
        Ok(builder)
    } else {
        Ok(builder.extend_params(additional_params))
    }
}

/// Return whether the executable used by an effective base command can be
/// resolved on this machine. Parsing follows the same platform-specific rules
/// as command execution, and a configured base-command override takes
/// precedence over the built-in command.
pub fn command_is_available(base_command: &str, overrides: &CmdOverrides) -> bool {
    let effective_command = overrides
        .base_command_override
        .as_deref()
        .unwrap_or(base_command);
    split_command_line(effective_command)
        .ok()
        .and_then(|parts| parts.into_iter().next())
        .and_then(|program| resolve_executable_path_blocking(&program))
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::{
        CmdOverrides, CommandBuilder, apply_overrides, command_is_available, redacted_command,
    };

    fn quoted_current_executable() -> String {
        format!(
            "\"{}\"",
            std::env::current_exe()
                .expect("resolve current test executable")
                .display()
        )
    }

    #[test]
    fn command_availability_resolves_effective_executable() {
        assert!(command_is_available(
            &quoted_current_executable(),
            &CmdOverrides::default()
        ));
        assert!(!command_is_available(
            "openteams-command-that-does-not-exist",
            &CmdOverrides::default()
        ));
    }

    #[test]
    fn command_availability_respects_base_command_override() {
        let overrides = CmdOverrides {
            base_command_override: Some(quoted_current_executable()),
            ..CmdOverrides::default()
        };
        assert!(command_is_available(
            "openteams-command-that-does-not-exist",
            &overrides
        ));
    }

    #[test]
    fn additional_params_parser_matches_built_command_tokens() {
        let overrides = CmdOverrides {
            additional_params: Some(vec![
                "\"--mcp-config\"".to_string(),
                "\"/tmp/member config.json\"".to_string(),
            ]),
            ..CmdOverrides::default()
        };
        let parsed = overrides
            .parsed_additional_params()
            .expect("parse quoted additional parameters");
        let (_, built_args) = apply_overrides(CommandBuilder::new("adapter"), &overrides)
            .expect("apply quoted additional parameters")
            .build_initial()
            .expect("build command")
            .into_parts_for_test();

        assert_eq!(parsed, ["--mcp-config", "/tmp/member config.json"]);
        assert_eq!(built_args, parsed);
    }

    #[test]
    fn diagnostic_command_quotes_arguments_and_redacts_credentials() {
        let rendered = redacted_command(
            "/path with spaces/copilot",
            &[
                "models".to_string(),
                "--api-key".to_string(),
                "top-secret".to_string(),
                "--token=another-secret".to_string(),
            ],
        );

        assert_eq!(
            rendered,
            "\"/path with spaces/copilot\" models --api-key <redacted> --token=<redacted>"
        );
        assert!(!rendered.contains("top-secret"));
        assert!(!rendered.contains("another-secret"));
    }
}
