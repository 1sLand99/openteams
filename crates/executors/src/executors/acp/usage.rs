use agent_client_protocol::schema::v1::{Meta, PromptResponse, SessionUpdate};
use serde_json::Value;

use crate::logs::TokenUsageInfo;

const INPUT_TOKEN_KEYS: &[&str] = &[
    "inputTokens",
    "input_tokens",
    "promptTokenCount",
    "prompt_token_count",
];
const OUTPUT_TOKEN_KEYS: &[&str] = &[
    "outputTokens",
    "output_tokens",
    "candidatesTokenCount",
    "candidates_token_count",
];
const REASONING_TOKEN_KEYS: &[&str] = &[
    "thoughtTokens",
    "thought_tokens",
    "reasoningTokens",
    "reasoning_tokens",
    "reasoningOutputTokens",
    "reasoning_output_tokens",
    "thoughtsTokenCount",
    "thoughts_token_count",
];
const CACHE_READ_TOKEN_KEYS: &[&str] = &[
    "cachedReadTokens",
    "cached_read_tokens",
    "cacheReadTokens",
    "cache_read_tokens",
    "cachedInputTokens",
    "cached_input_tokens",
    "cachedContentTokenCount",
    "cached_content_token_count",
];
const MODEL_KEYS: &[&str] = &["model", "modelId", "model_id", "modelName", "model_name"];
const CONTEXT_WINDOW_KEYS: &[&str] = &[
    "modelContextWindow",
    "model_context_window",
    "contextWindow",
    "context_window",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct UsageAmounts {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    model_context_window: Option<u64>,
    runtime_model_id: Option<String>,
}

impl UsageAmounts {
    fn add_assign(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_output_tokens =
            add_optional(self.reasoning_output_tokens, other.reasoning_output_tokens);
        self.cache_read_tokens = add_optional(self.cache_read_tokens, other.cache_read_tokens);
        self.model_context_window = other.model_context_window.or(self.model_context_window);
        if self.runtime_model_id.is_none() {
            self.runtime_model_id.clone_from(&other.runtime_model_id);
        } else if other.runtime_model_id.is_some()
            && self.runtime_model_id != other.runtime_model_id
        {
            self.runtime_model_id = None;
        }
    }

    fn billable_total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Provider-neutral reducer for token usage carried by ACP.
///
/// Standard end-turn usage is authoritative. When an Agent only exposes usage
/// through extensible metadata, a final response aggregate overrides streamed
/// deltas; otherwise streamed deltas are summed for the active prompt turn.
#[derive(Debug, Default)]
pub(super) struct AcpTokenUsageAccumulator {
    streamed_usage: Option<UsageAmounts>,
    turn_active: bool,
    runtime_agent: Option<String>,
    runtime_model_id: Option<String>,
    runtime_thread_id: Option<String>,
}

impl AcpTokenUsageAccumulator {
    pub(super) fn set_runtime_identity(
        &mut self,
        runtime_agent: Option<String>,
        runtime_model_id: Option<String>,
        runtime_thread_id: String,
    ) {
        self.runtime_agent = runtime_agent;
        self.runtime_model_id = runtime_model_id;
        self.runtime_thread_id = Some(runtime_thread_id);
    }

    pub(super) fn observe_meta(&mut self, meta: Option<&Meta>) {
        if !self.turn_active {
            return;
        }
        let Some(usage) = meta.and_then(extract_usage_from_meta) else {
            return;
        };
        if let Some(accumulated) = self.streamed_usage.as_mut() {
            accumulated.add_assign(&usage);
        } else {
            self.streamed_usage = Some(usage);
        }
    }

    pub(super) fn observe_session_update(&mut self, update: &SessionUpdate) {
        let meta = serde_json::to_value(update)
            .ok()
            .and_then(|value| value.get("_meta").and_then(Value::as_object).cloned());
        self.observe_meta(meta.as_ref());
    }

    pub(super) fn begin_turn(&mut self) {
        // Resume/load may replay historical notifications before the prompt.
        self.streamed_usage = None;
        self.turn_active = true;
    }

    pub(super) fn finish_turn(&mut self, response: &PromptResponse) -> Option<TokenUsageInfo> {
        self.turn_active = false;
        let streamed_usage = self.streamed_usage.take();
        if let Some(usage) = response.usage.as_ref() {
            let amounts = UsageAmounts {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                reasoning_output_tokens: usage.thought_tokens,
                cache_read_tokens: usage.cached_read_tokens,
                model_context_window: None,
                runtime_model_id: usage.meta.as_ref().and_then(extract_unique_model_id),
            };
            return Some(self.to_token_usage(amounts, "thread_total_snapshot"));
        }

        if let Some(amounts) = response.meta.as_ref().and_then(extract_usage_from_meta) {
            return Some(self.to_token_usage(amounts, "turn_delta"));
        }

        streamed_usage.map(|amounts| self.to_token_usage(amounts, "turn_delta"))
    }

    fn to_token_usage(&self, amounts: UsageAmounts, usage_scope: &str) -> TokenUsageInfo {
        let total_tokens = clamp_u32(amounts.billable_total());
        let input_tokens = clamp_u32(amounts.input_tokens);
        let output_tokens = clamp_u32(amounts.output_tokens);
        let reasoning_output_tokens = amounts.reasoning_output_tokens.map(clamp_u32);
        let cache_read_tokens = amounts.cache_read_tokens.map(clamp_u32);
        let is_snapshot = usage_scope == "thread_total_snapshot";
        TokenUsageInfo {
            total_tokens,
            model_context_window: clamp_u32(amounts.model_context_window.unwrap_or_default()),
            input_tokens: Some(input_tokens),
            output_tokens: Some(output_tokens),
            reasoning_output_tokens,
            cache_read_tokens,
            runtime_agent: self.runtime_agent.clone(),
            runtime_model_id: amounts
                .runtime_model_id
                .or_else(|| self.runtime_model_id.clone()),
            provider_id: None,
            runtime_thread_id: self.runtime_thread_id.clone(),
            usage_scope: Some(usage_scope.to_string()),
            snapshot_total_tokens: is_snapshot.then_some(total_tokens),
            snapshot_input_tokens: is_snapshot.then_some(input_tokens),
            snapshot_output_tokens: is_snapshot.then_some(output_tokens),
            snapshot_reasoning_output_tokens: is_snapshot
                .then_some(reasoning_output_tokens)
                .flatten(),
            snapshot_cache_read_tokens: is_snapshot.then_some(cache_read_tokens).flatten(),
            is_estimated: false,
        }
    }
}

fn extract_usage_from_meta(meta: &Meta) -> Option<UsageAmounts> {
    // Require a complete input/output pair so context occupancy and arbitrary
    // numeric metadata cannot be mistaken for billable token usage.
    let root = Value::Object(meta.clone());
    let mut candidates = Vec::new();
    collect_usage_candidates(&root, &mut candidates);
    let mut usage = candidates
        .into_iter()
        .max_by_key(UsageAmounts::billable_total)?;
    usage.runtime_model_id = usage
        .runtime_model_id
        .or_else(|| extract_unique_model_id(meta));
    Some(usage)
}

fn collect_usage_candidates(value: &Value, candidates: &mut Vec<UsageAmounts>) {
    match value {
        Value::Object(object) => {
            if let (Some(input_tokens), Some(output_tokens)) = (
                first_u64(object, INPUT_TOKEN_KEYS),
                first_u64(object, OUTPUT_TOKEN_KEYS),
            ) {
                candidates.push(UsageAmounts {
                    input_tokens,
                    output_tokens,
                    reasoning_output_tokens: first_u64(object, REASONING_TOKEN_KEYS),
                    cache_read_tokens: first_u64(object, CACHE_READ_TOKEN_KEYS),
                    model_context_window: first_u64(object, CONTEXT_WINDOW_KEYS),
                    runtime_model_id: first_string(object, MODEL_KEYS),
                });
            }
            for child in object.values() {
                collect_usage_candidates(child, candidates);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_usage_candidates(child, candidates);
            }
        }
        _ => {}
    }
}

fn extract_unique_model_id(meta: &Meta) -> Option<String> {
    let mut models = Vec::new();
    collect_model_ids(&Value::Object(meta.clone()), &mut models);
    models.sort();
    models.dedup();
    (models.len() == 1).then(|| models.remove(0))
}

fn collect_model_ids(value: &Value, models: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if let Some(model) = first_string(object, MODEL_KEYS) {
                models.push(model);
            }
            for child in object.values() {
                collect_model_ids(child, models);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_model_ids(child, models);
            }
        }
        _ => {}
    }
}

fn first_u64(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(nonnegative_u64))
}

fn first_string(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        object
            .get(*key)
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string)
    })
}

fn nonnegative_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn clamp_u32(value: u64) -> u32 {
    value.min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        ContentBlock, ContentChunk, PromptResponse, SessionUpdate, StopReason, TextContent, Usage,
    };
    use serde_json::json;

    use super::*;

    fn meta(value: Value) -> Meta {
        value.as_object().expect("metadata object").clone()
    }

    fn accumulator() -> AcpTokenUsageAccumulator {
        let mut accumulator = AcpTokenUsageAccumulator::default();
        accumulator.set_runtime_identity(
            Some("acp-agent".to_string()),
            Some("configured-model".to_string()),
            "session-1".to_string(),
        );
        accumulator.begin_turn();
        accumulator
    }

    fn message_update(meta: Meta) -> SessionUpdate {
        SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(""))).meta(meta),
        )
    }

    #[test]
    fn accumulates_streamed_usage_metadata_without_agent_branches() {
        let mut accumulator = accumulator();
        accumulator.observe_session_update(&message_update(meta(json!({
            "usage": {
                "inputTokens": 100,
                "outputTokens": 20,
                "totalTokens": 120,
                "thoughtTokens": 5,
                "cachedReadTokens": 40
            }
        }))));
        accumulator.observe_session_update(&message_update(meta(json!({
            "usage": {
                "inputTokens": 80,
                "outputTokens": 10,
                "totalTokens": 90,
                "thoughtTokens": 2,
                "cachedReadTokens": 70
            }
        }))));

        let usage = accumulator
            .finish_turn(&PromptResponse::new(StopReason::EndTurn))
            .expect("usage");
        assert_eq!(usage.input_tokens, Some(180));
        assert_eq!(usage.output_tokens, Some(30));
        assert_eq!(usage.total_tokens, 210);
        assert_eq!(usage.reasoning_output_tokens, Some(7));
        assert_eq!(usage.cache_read_tokens, Some(110));
        assert_eq!(usage.usage_scope.as_deref(), Some("turn_delta"));
        assert_eq!(usage.runtime_agent.as_deref(), Some("acp-agent"));
        assert_eq!(usage.runtime_thread_id.as_deref(), Some("session-1"));
    }

    #[test]
    fn final_response_metadata_overrides_stream_fragments() {
        let mut accumulator = accumulator();
        accumulator.observe_meta(Some(&meta(json!({
            "usage": {"inputTokens": 100, "outputTokens": 20}
        }))));
        let response = PromptResponse::new(StopReason::EndTurn).meta(meta(json!({
            "quota": {
                "token_count": {"input_tokens": 300, "output_tokens": 40},
                "model_usage": [{
                    "model": "runtime-model",
                    "token_count": {"input_tokens": 300, "output_tokens": 40}
                }]
            }
        })));

        let usage = accumulator.finish_turn(&response).expect("usage");
        assert_eq!(usage.input_tokens, Some(300));
        assert_eq!(usage.output_tokens, Some(40));
        assert_eq!(usage.total_tokens, 340);
        assert_eq!(usage.runtime_model_id.as_deref(), Some("runtime-model"));
    }

    #[test]
    fn standard_acp_usage_is_a_session_snapshot() {
        let mut accumulator = accumulator();
        let response =
            PromptResponse::new(StopReason::EndTurn).usage(Usage::new(1_250, 1_000, 250));

        let usage = accumulator.finish_turn(&response).expect("usage");
        assert_eq!(usage.usage_scope.as_deref(), Some("thread_total_snapshot"));
        assert_eq!(usage.snapshot_input_tokens, Some(1_000));
        assert_eq!(usage.snapshot_output_tokens, Some(250));
        assert_eq!(usage.snapshot_total_tokens, Some(1_250));
    }

    #[test]
    fn ignores_metadata_without_input_and_output_breakdown() {
        let mut accumulator = accumulator();
        accumulator.observe_meta(Some(&meta(json!({
            "usage": {"totalTokens": 120}
        }))));

        assert!(
            accumulator
                .finish_turn(&PromptResponse::new(StopReason::EndTurn))
                .is_none()
        );
    }

    #[test]
    fn ignores_session_replay_usage_before_turn_begins() {
        let mut accumulator = AcpTokenUsageAccumulator::default();
        accumulator.observe_session_update(&message_update(meta(json!({
            "usage": {"inputTokens": 500, "outputTokens": 100}
        }))));
        accumulator.begin_turn();
        accumulator.observe_session_update(&message_update(meta(json!({
            "usage": {"inputTokens": 50, "outputTokens": 10}
        }))));

        let usage = accumulator
            .finish_turn(&PromptResponse::new(StopReason::EndTurn))
            .expect("usage");
        assert_eq!(usage.input_tokens, Some(50));
        assert_eq!(usage.output_tokens, Some(10));
    }
}
