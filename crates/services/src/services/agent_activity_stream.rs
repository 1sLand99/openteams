use std::collections::HashMap;

use executors::logs::{
    ActionType, FileChange, NormalizedEntry, NormalizedEntryType, ToolResult, ToolStatus,
    utils::patch::extract_normalized_entry_from_patch,
};
use json_patch::Patch;

use super::chat_runner::{ChatRunActivityLineType, ChatStreamDeltaType};

#[derive(Debug, Clone)]
pub struct AgentActivityEntryLine {
    pub stream_type: ChatStreamDeltaType,
    pub line_type: ChatRunActivityLineType,
    pub content: String,
    pub immediate: bool,
    pub runtime_session_id: Option<String>,
    pub runtime_parent_session_id: Option<String>,
    pub runtime_session_title: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct PendingLineBuffer {
    content: String,
    index: Option<usize>,
    runtime_session_id: Option<String>,
    runtime_parent_session_id: Option<String>,
    runtime_session_title: Option<String>,
}

#[derive(Default)]
pub struct AgentActivityStreamState {
    last_content_by_index: HashMap<usize, String>,
    assistant: PendingLineBuffer,
    thinking: PendingLineBuffer,
    error: PendingLineBuffer,
}

impl AgentActivityStreamState {
    pub fn drain_patch_lines(
        &mut self,
        patch: &Patch,
        include_assistant: bool,
    ) -> Vec<AgentActivityEntryLine> {
        let Some((index, entry)) = extract_normalized_entry_from_patch(patch) else {
            return Vec::new();
        };

        let Some(line) = activity_line_for_entry(&entry, include_assistant) else {
            return Vec::new();
        };

        let previous = self
            .last_content_by_index
            .insert(index, line.content.clone())
            .unwrap_or_default();
        if previous == line.content {
            return Vec::new();
        }

        if line.immediate {
            return vec![line];
        }

        let mut emitted = self
            .mark_stream_entry_boundary(&line, index)
            .into_iter()
            .collect::<Vec<_>>();

        let chunk = if line.content.starts_with(&previous) {
            line.content[previous.len()..].to_string()
        } else {
            line.content
        };

        emitted.extend(self.drain_chunk_lines(line.stream_type, line.line_type, &chunk));
        emitted
    }

    fn buffer_mut(&mut self, stream_type: &ChatStreamDeltaType) -> &mut PendingLineBuffer {
        match stream_type {
            ChatStreamDeltaType::Assistant => &mut self.assistant,
            ChatStreamDeltaType::Thinking => &mut self.thinking,
            ChatStreamDeltaType::Error => &mut self.error,
        }
    }

    fn mark_stream_entry_boundary(
        &mut self,
        line: &AgentActivityEntryLine,
        index: usize,
    ) -> Option<AgentActivityEntryLine> {
        let buffer = self.buffer_mut(&line.stream_type);
        let emitted = if buffer.index.is_some_and(|current| current != index) {
            take_pending_line(buffer, line.stream_type.clone(), line.line_type.clone())
        } else {
            None
        };

        if buffer.index != Some(index) {
            buffer.index = Some(index);
            buffer.runtime_session_id = line.runtime_session_id.clone();
            buffer.runtime_parent_session_id = line.runtime_parent_session_id.clone();
            buffer.runtime_session_title = line.runtime_session_title.clone();
        }
        emitted
    }

    fn drain_chunk_lines(
        &mut self,
        stream_type: ChatStreamDeltaType,
        line_type: ChatRunActivityLineType,
        chunk: &str,
    ) -> Vec<AgentActivityEntryLine> {
        if chunk.is_empty() {
            return Vec::new();
        }

        let normalized = chunk.replace("\r\n", "\n").replace('\r', "\n");
        let buffer = self.buffer_mut(&stream_type);
        buffer.content.push_str(&normalized);

        let mut emitted = Vec::new();
        while let Some(newline_index) = buffer.content.find('\n') {
            let line = buffer.content[..newline_index].trim();
            if !line.is_empty() {
                emitted.push(AgentActivityEntryLine {
                    stream_type: stream_type.clone(),
                    line_type: line_type.clone(),
                    content: line.to_string(),
                    immediate: false,
                    runtime_session_id: buffer.runtime_session_id.clone(),
                    runtime_parent_session_id: buffer.runtime_parent_session_id.clone(),
                    runtime_session_title: buffer.runtime_session_title.clone(),
                });
            }
            buffer.content.drain(..=newline_index);
        }

        emitted
    }

    pub fn flush_pending_lines(&mut self) -> Vec<AgentActivityEntryLine> {
        let mut emitted = Vec::new();

        for (stream_type, line_type, buffer) in [
            (
                ChatStreamDeltaType::Assistant,
                ChatRunActivityLineType::Assistant,
                &mut self.assistant,
            ),
            (
                ChatStreamDeltaType::Thinking,
                ChatRunActivityLineType::Thinking,
                &mut self.thinking,
            ),
            (
                ChatStreamDeltaType::Error,
                ChatRunActivityLineType::Error,
                &mut self.error,
            ),
        ] {
            if let Some(line) = take_pending_line(buffer, stream_type, line_type) {
                emitted.push(line);
            }
        }

        emitted
    }
}

fn take_pending_line(
    buffer: &mut PendingLineBuffer,
    stream_type: ChatStreamDeltaType,
    line_type: ChatRunActivityLineType,
) -> Option<AgentActivityEntryLine> {
    let content = std::mem::take(&mut buffer.content).trim().to_string();
    let line = (!content.is_empty()).then(|| AgentActivityEntryLine {
        stream_type,
        line_type,
        content,
        immediate: false,
        runtime_session_id: buffer.runtime_session_id.clone(),
        runtime_parent_session_id: buffer.runtime_parent_session_id.clone(),
        runtime_session_title: buffer.runtime_session_title.clone(),
    });
    buffer.index = None;
    buffer.runtime_session_id = None;
    buffer.runtime_parent_session_id = None;
    buffer.runtime_session_title = None;
    line
}

#[derive(Default)]
struct ActivitySessionProjection {
    runtime_session_id: Option<String>,
    runtime_parent_session_id: Option<String>,
    runtime_session_title: Option<String>,
}

fn session_projection(entry: &NormalizedEntry) -> ActivitySessionProjection {
    let metadata = entry.metadata.as_ref();
    ActivitySessionProjection {
        runtime_session_id: metadata
            .and_then(|value| value.get("runtime_session_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        runtime_parent_session_id: metadata
            .and_then(|value| value.get("runtime_parent_session_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        runtime_session_title: metadata
            .and_then(|value| value.get("runtime_session_title"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    }
}

pub fn activity_line_for_entry(
    entry: &NormalizedEntry,
    include_assistant: bool,
) -> Option<AgentActivityEntryLine> {
    let projection = session_projection(entry);
    match &entry.entry_type {
        NormalizedEntryType::AssistantMessage if include_assistant => {
            Some(AgentActivityEntryLine {
                stream_type: ChatStreamDeltaType::Assistant,
                line_type: ChatRunActivityLineType::Assistant,
                content: entry.content.clone(),
                immediate: false,
                runtime_session_id: projection.runtime_session_id,
                runtime_parent_session_id: projection.runtime_parent_session_id,
                runtime_session_title: projection.runtime_session_title,
            })
        }
        NormalizedEntryType::Thinking => Some(AgentActivityEntryLine {
            stream_type: ChatStreamDeltaType::Thinking,
            line_type: ChatRunActivityLineType::Thinking,
            content: entry.content.clone(),
            immediate: false,
            runtime_session_id: projection.runtime_session_id,
            runtime_parent_session_id: projection.runtime_parent_session_id,
            runtime_session_title: projection.runtime_session_title,
        }),
        NormalizedEntryType::ToolUse {
            tool_name,
            action_type,
            status,
        } => tool_activity_content(tool_name, action_type, status, &entry.content).map(|content| {
            AgentActivityEntryLine {
                stream_type: ChatStreamDeltaType::Thinking,
                line_type: ChatRunActivityLineType::Tool,
                content,
                immediate: true,
                runtime_session_id: projection.runtime_session_id,
                runtime_parent_session_id: projection.runtime_parent_session_id,
                runtime_session_title: projection.runtime_session_title,
            }
        }),
        NormalizedEntryType::ErrorMessage { .. } => Some(AgentActivityEntryLine {
            stream_type: ChatStreamDeltaType::Error,
            line_type: ChatRunActivityLineType::Error,
            content: entry.content.clone(),
            immediate: true,
            runtime_session_id: projection.runtime_session_id,
            runtime_parent_session_id: projection.runtime_parent_session_id,
            runtime_session_title: projection.runtime_session_title,
        }),
        _ => None,
    }
}

pub(crate) fn tool_activity_content(
    tool_name: &str,
    action_type: &ActionType,
    status: &ToolStatus,
    fallback_content: &str,
) -> Option<String> {
    let status_label = tool_status_label(status);

    let content = match action_type {
        ActionType::FileEdit { path, changes } => {
            let change_summary = file_change_summary(changes);
            format!("{status_label} file edit: {path}{change_summary}")
        }
        ActionType::CommandRun { command, result } => {
            let mut line = format!(
                "{status_label} command: {}",
                truncate_activity_line(command)
            );
            if let Some(preview) = result
                .as_ref()
                .and_then(|result| result.output.as_deref())
                .and_then(activity_result_preview)
            {
                line.push_str(": ");
                line.push_str(&preview);
            }
            line
        }
        ActionType::Tool {
            tool_name: inner_tool_name,
            result,
            ..
        } => {
            let display_tool_name = if inner_tool_name.trim().is_empty() {
                tool_name
            } else {
                inner_tool_name
            };
            let prefix = if tool_name.starts_with("mcp:") || display_tool_name.starts_with("mcp:") {
                "MCP tool"
            } else {
                "Tool"
            };
            let mut line = format!("{status_label} {prefix}: {display_tool_name}");
            if let Some(preview) = tool_result_preview(result) {
                line.push_str(": ");
                line.push_str(&preview);
            }
            line
        }
        ActionType::TaskCreate {
            description,
            subagent_type,
            result,
        } => {
            let mut line = format!(
                "{status_label} task: {}",
                truncate_activity_line(description)
            );
            if let Some(subagent_type) = subagent_type
                && !subagent_type.trim().is_empty()
            {
                line.push_str(" (");
                line.push_str(subagent_type.trim());
                line.push(')');
            }
            if let Some(preview) = tool_result_preview(result) {
                line.push_str(": ");
                line.push_str(&preview);
            }
            line
        }
        ActionType::FileRead { path } => format!("{status_label} file read: {path}"),
        ActionType::Search { query } => {
            format!("{status_label} search: {}", truncate_activity_line(query))
        }
        ActionType::WebFetch { url } => format!("{status_label} web fetch: {url}"),
        ActionType::TodoManagement { todos, operation } => {
            format!("{status_label} plan {operation}: {} item(s)", todos.len())
        }
        ActionType::PlanPresentation { plan } => {
            format!("{status_label} plan: {}", truncate_activity_line(plan))
        }
        ActionType::Other { description } => {
            format!(
                "{status_label} activity: {}",
                truncate_activity_line(description)
            )
        }
    };

    let content = content.trim();
    if !content.is_empty() {
        return Some(content.to_string());
    }

    let fallback = fallback_content.trim();
    (!fallback.is_empty()).then(|| {
        format!(
            "{status_label} activity: {}",
            truncate_activity_line(fallback)
        )
    })
}

fn tool_status_label(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Created => "Started",
        ToolStatus::Success => "Completed",
        ToolStatus::Failed => "Failed",
        ToolStatus::Denied { .. } => "Denied",
        ToolStatus::PendingApproval { .. } => "Waiting approval for",
        ToolStatus::TimedOut => "Timed out",
    }
}

fn file_change_summary(changes: &[FileChange]) -> String {
    if changes.is_empty() {
        return String::new();
    }

    let mut write_count = 0;
    let mut edit_count = 0;
    let mut delete_count = 0;
    let mut rename_count = 0;

    for change in changes {
        match change {
            FileChange::Write { .. } => write_count += 1,
            FileChange::Edit { .. } => edit_count += 1,
            FileChange::Delete => delete_count += 1,
            FileChange::Rename { .. } => rename_count += 1,
        }
    }

    let mut parts = Vec::new();
    if write_count > 0 {
        parts.push(format!("{write_count} write"));
    }
    if edit_count > 0 {
        parts.push(format!("{edit_count} edit"));
    }
    if delete_count > 0 {
        parts.push(format!("{delete_count} delete"));
    }
    if rename_count > 0 {
        parts.push(format!("{rename_count} rename"));
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn tool_result_preview(result: &Option<ToolResult>) -> Option<String> {
    let result = result.as_ref()?;
    let preview = match &result.value {
        serde_json::Value::String(value) => value.clone(),
        value => value.to_string(),
    };
    activity_result_preview(&preview)
}

fn activity_result_preview(result: &str) -> Option<String> {
    let preview = result
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    Some(truncate_activity_line(preview))
}

pub(crate) fn truncate_activity_line(value: &str) -> String {
    const MAX_LEN: usize = 220;

    let trimmed = value.trim();
    let mut chars = trimmed.chars();
    let truncated = chars.by_ref().take(MAX_LEN).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use executors::logs::{NormalizedEntry, utils::patch::ConversationPatch};

    use super::*;

    #[test]
    fn chat_runner_line_buffer_emits_only_complete_lines_and_flushes_tail() {
        let mut state = AgentActivityStreamState::default();
        let first = ConversationPatch::add_normalized_entry(
            0,
            NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::Thinking,
                content: "first partial".to_string(),
                metadata: None,
            },
        );
        assert!(state.drain_patch_lines(&first, true).is_empty());

        let second = ConversationPatch::replace(
            0,
            NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::Thinking,
                content: "first partial\nsecond partial".to_string(),
                metadata: None,
            },
        );
        let lines = state.drain_patch_lines(&second, true);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].content, "first partial");
        assert_eq!(lines[0].line_type, ChatRunActivityLineType::Thinking);

        let flushed = state.flush_pending_lines();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].content, "second partial");
    }

    #[test]
    fn chat_runner_line_buffer_separates_reasoning_summary_entries_without_newlines() {
        let mut state = AgentActivityStreamState::default();
        let first = ConversationPatch::add_normalized_entry(
            0,
            NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::Thinking,
                content: "**Planning file inspection**".to_string(),
                metadata: None,
            },
        );
        assert!(state.drain_patch_lines(&first, true).is_empty());

        let second = ConversationPatch::add_normalized_entry(
            1,
            NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::Thinking,
                content: "**Reading existing package**".to_string(),
                metadata: None,
            },
        );
        let lines = state.drain_patch_lines(&second, true);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].content, "**Planning file inspection**");

        let flushed = state.flush_pending_lines();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].content, "**Reading existing package**");
    }

    #[test]
    fn chat_runner_interleaved_child_output_keeps_its_session_identity() {
        let mut state = AgentActivityStreamState::default();
        let child_a = ConversationPatch::add_normalized_entry(
            0,
            NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::Thinking,
                content: "A is inspecting".to_string(),
                metadata: Some(serde_json::json!({
                    "runtime_session_id": "child-a",
                    "runtime_parent_session_id": "root",
                    "runtime_session_title": "Inspect API"
                })),
            },
        );
        let child_b = ConversationPatch::add_normalized_entry(
            1,
            NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::Thinking,
                content: "B is testing".to_string(),
                metadata: Some(serde_json::json!({
                    "runtime_session_id": "child-b",
                    "runtime_parent_session_id": "root",
                    "runtime_session_title": "Inspect tests"
                })),
            },
        );

        assert!(state.drain_patch_lines(&child_a, true).is_empty());
        let first = state.drain_patch_lines(&child_b, true);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].content, "A is inspecting");
        assert_eq!(first[0].runtime_session_id.as_deref(), Some("child-a"));
        assert_eq!(
            first[0].runtime_session_title.as_deref(),
            Some("Inspect API")
        );

        let child_a_continues = ConversationPatch::replace(
            0,
            NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::Thinking,
                content: "A is inspecting\nA found routes".to_string(),
                metadata: Some(serde_json::json!({
                    "runtime_session_id": "child-a",
                    "runtime_parent_session_id": "root",
                    "runtime_session_title": "Inspect API"
                })),
            },
        );
        let second = state.drain_patch_lines(&child_a_continues, true);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].content, "B is testing");
        assert_eq!(second[0].runtime_session_id.as_deref(), Some("child-b"));

        let tail = state.flush_pending_lines();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].content, "A found routes");
        assert_eq!(tail[0].runtime_session_id.as_deref(), Some("child-a"));
        assert_eq!(tail[0].runtime_parent_session_id.as_deref(), Some("root"));
    }

    #[test]
    fn chat_runner_tool_line_is_emitted_as_summary_line() {
        let mut state = AgentActivityStreamState::default();
        let patch = ConversationPatch::add_normalized_entry(
            0,
            NormalizedEntry {
                timestamp: None,
                entry_type: NormalizedEntryType::ToolUse {
                    tool_name: "shell".to_string(),
                    action_type: ActionType::CommandRun {
                        command: "cargo test -p services chat_runner".to_string(),
                        result: None,
                    },
                    status: ToolStatus::Created,
                },
                content: String::new(),
                metadata: None,
            },
        );

        let lines = state.drain_patch_lines(&patch, true);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line_type, ChatRunActivityLineType::Tool);
        assert_eq!(
            lines[0].content,
            "Started command: cargo test -p services chat_runner"
        );
    }

    #[test]
    fn chat_runner_command_result_includes_a_collapsible_preview() {
        let entry = NormalizedEntry {
            timestamp: None,
            entry_type: NormalizedEntryType::ToolUse {
                tool_name: "shell".to_string(),
                action_type: ActionType::CommandRun {
                    command: "pnpm test".to_string(),
                    result: Some(executors::logs::CommandRunResult {
                        exit_status: None,
                        output: Some("\nAll tests passed\nmore output".to_string()),
                    }),
                },
                status: ToolStatus::Success,
            },
            content: String::new(),
            metadata: None,
        };

        let line = activity_line_for_entry(&entry, true).expect("command result line");

        assert_eq!(
            line.content,
            "Completed command: pnpm test: All tests passed"
        );
    }
}
