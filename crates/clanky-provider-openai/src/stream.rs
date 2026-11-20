//! OpenAI streaming chunk shapes and the accumulator that translates them
//! into protocol chunks.
//!
//! This is the mechanism every OpenAI-compatible plugin shares: the same
//! `delta`/`usage`/`tool_calls` shapes and the same hard-won accumulation
//! rules (a tool call's id may trail its args; an oversized index is a
//! malformed chunk, not an allocation request). A provider that documents a
//! different field for one of these shapes (DeepInfra's `reasoning_content`,
//! say) still feeds the same [`OpenAiStreamChunk`] into [`StreamState`] and
//! keeps only the *policy* differences locally.

use clanky_protocol::{ChatDone, ChunkPayload, FinishReason, Usage};
use serde_json::Value;

// --- OpenAI streaming chunk shapes (parsed leniently; the backends are
// --- OpenAI-compatible) ---

#[derive(Debug, Default, serde::Deserialize)]
pub struct OpenAiStreamChunk {
    #[serde(default)]
    pub choices: Vec<OpenAiStreamChoice>,
    #[serde(default)]
    pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct OpenAiStreamChoice {
    #[serde(default)]
    pub delta: OpenAiDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct OpenAiDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<OpenAiStreamToolCall>>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct OpenAiStreamToolCall {
    #[serde(default)]
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    /// Lenient: some OpenAI-compatible backends emit index-only placeholder
    /// deltas with no `function` object at all; treat that as an empty one.
    #[serde(default)]
    pub function: OpenAiFunction,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct OpenAiFunction {
    #[serde(default)]
    pub name: Option<String>,
    /// OpenAI carries arguments as a JSON string; some backends send an
    /// object. Both are accepted.
    #[serde(default)]
    pub arguments: Option<Value>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct OpenAiUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    pub prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
}

/// OpenAI-style cached-token breakdown inside `usage.prompt_tokens_details`.
#[derive(Debug, Default, serde::Deserialize)]
pub struct OpenAiPromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// Accumulates one streamed completion and translates deltas into protocol
/// chunks. A tool call's `id`/`name` arrive with the first delta for its
/// index (OpenAI convention) and announce a `toolCallStart`; later deltas
/// append argument fragments.
#[derive(Debug, Default)]
pub struct StreamState {
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
    calls: Vec<CallAccumulator>,
}

#[derive(Debug, Default)]
struct CallAccumulator {
    id: String,
    name: String,
    args: String,
    started: bool,
}

impl StreamState {
    pub fn ingest(&mut self, chunk: OpenAiStreamChunk, sink: &mut dyn FnMut(ChunkPayload)) {
        if let Some(usage) = chunk.usage {
            self.usage = Some(Usage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                cached_tokens: usage.prompt_tokens_details.and_then(|d| d.cached_tokens),
            });
        }
        for choice in chunk.choices {
            if let Some(reason) = choice.finish_reason {
                self.finish_reason = Some(map_finish_reason(Some(&reason)));
            }
            let delta = choice.delta;
            if let Some(text) = delta.reasoning_content.filter(|s| !s.is_empty()) {
                sink(ChunkPayload::Thinking { text });
            }
            if let Some(text) = delta.content.filter(|s| !s.is_empty()) {
                sink(ChunkPayload::Text { text });
            }
            for call in delta.tool_calls.unwrap_or_default() {
                self.ingest_tool_call(call, sink);
            }
        }
    }

    fn ingest_tool_call(&mut self, call: OpenAiStreamToolCall, sink: &mut dyn FnMut(ChunkPayload)) {
        // A malformed index (e.g. `"index": 4294967295`) must not allocate a
        // huge accumulator buffer (~4 bytes per missing slot ⇒ OOM on a
        // single junk chunk). Real tool-call indices are tiny; anything
        // beyond this bound is a broken stream and the delta is dropped.
        if call.index > MAX_TOOL_CALL_INDEX {
            return;
        }
        while self.calls.len() <= call.index as usize {
            self.calls.push(CallAccumulator::default());
        }
        let acc = &mut self.calls[call.index as usize];
        // Args may arrive before the id/name: some backends lead with an
        // index-only placeholder and put the first args fragment on the
        // same delta as (or even before) the id. Buffer the fragment into
        // the accumulator regardless of `started` so it is never lost.
        let args_fragment = call
            .function
            .arguments
            .and_then(args_string)
            .filter(|s| !s.is_empty());
        if let Some(fragment) = &args_fragment {
            acc.args.push_str(fragment);
        }
        if !acc.started {
            // The id/name may still be on later deltas (some backends lead
            // with an index-only placeholder); wait until we have something
            // to announce before emitting `toolCallStart`.
            if let Some(id) = call.id.clone().filter(|id| !id.is_empty()) {
                acc.id = id;
            }
            if let Some(name) = call.function.name.clone().filter(|n| !n.is_empty()) {
                acc.name = name;
            }
            if acc.id.is_empty() && acc.name.is_empty() {
                // Nothing to announce yet; buffered args stay in `acc.args`
                // and are emitted once the call starts.
                return;
            }
            if acc.id.is_empty() {
                acc.id = format!("call_{}", call.index);
            }
            acc.started = true;
            sink(ChunkPayload::ToolCallStart {
                index: call.index,
                id: acc.id.clone(),
                name: acc.name.clone(),
            });
        } else if call
            .id
            .as_deref()
            .is_some_and(|id| !id.is_empty() && id != acc.id)
        {
            // Defensive: a repeated id after the start is ignored — the id
            // must stay stable for the client's `toolCallId` round trip.
            // (Name-only updates after the start are also ignored.)
        }
        if let Some(fragment) = args_fragment {
            sink(ChunkPayload::ToolCallArgs {
                index: call.index,
                args_chunk: fragment,
            });
        }
    }

    pub fn finish(self) -> ChatDone {
        ChatDone {
            finish_reason: self.finish_reason.unwrap_or(FinishReason::Stop),
            usage: self.usage,
        }
    }

    /// Terminal status for a cancelled turn: whatever usage was reported
    /// before the cancel, and `cancelled` as the finish reason (spec §7).
    pub fn finish_cancelled(self) -> ChatDone {
        ChatDone {
            finish_reason: FinishReason::Cancelled,
            usage: self.usage,
        }
    }
}

fn args_string(args: Value) -> Option<String> {
    match args {
        Value::String(s) => Some(s),
        other => Some(other.to_string()),
    }
}

/// Largest tool-call index we accept in a stream delta. Real backends emit
/// single-digit indices; anything larger is a malformed chunk, and honoring
/// it would allocate an accumulator per skipped slot (one `"index":
/// 4294967295` chunk would otherwise allocate ~4 GB of accumulators).
pub const MAX_TOOL_CALL_INDEX: u32 = 1024;

pub fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("tool_calls" | "function_call") => FinishReason::ToolCalls,
        Some("length" | "max_tokens") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        // Unknown reasons are treated as a plain stop; `cancelled` is only
        // ever produced by Clanky-side cancellation, never the backend.
        _ => FinishReason::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn chunks(stream: &[Value]) -> (Vec<ChunkPayload>, ChatDone) {
        let mut state = StreamState::default();
        let mut out = Vec::new();
        for chunk in stream {
            let parsed: OpenAiStreamChunk = serde_json::from_value(chunk.clone()).unwrap();
            state.ingest(parsed, &mut |payload| out.push(payload));
        }
        (out, state.finish())
    }

    #[test]
    fn text_reasoning_and_usage_map_onto_protocol_chunks() {
        let (chunks, done) = chunks(&[
            json!({"choices": [{"delta": {"reasoning_content": "think"}}]}),
            json!({"choices": [{"delta": {"content": "hi"}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}],
                   "usage": {"prompt_tokens": 3, "completion_tokens": 1,
                             "prompt_tokens_details": {"cached_tokens": 2}}}),
        ]);
        assert_eq!(
            chunks,
            vec![
                ChunkPayload::Thinking {
                    text: "think".into()
                },
                ChunkPayload::Text { text: "hi".into() },
            ]
        );
        assert_eq!(done.finish_reason, FinishReason::Stop);
        assert_eq!(
            done.usage,
            Some(Usage {
                prompt_tokens: Some(3),
                completion_tokens: Some(1),
                cached_tokens: Some(2),
            })
        );
    }

    #[test]
    fn an_index_only_placeholder_defers_the_start_to_the_real_delta() {
        // Some OpenAI-compatible backends emit a bare index placeholder
        // before the first real delta for that index. It announces nothing;
        // the real id/name then announce the call, and its own args are
        // emitted as usual. (This pins the pre-refactor behavior exactly,
        // including the fact that a fragment buffered on the placeholder
        // delta is not replayed — M24a is a pure move, not a fix.)
        let (chunks, done) = chunks(&[
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0}]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [
                {"index": 0, "id": "call_1", "function": {"name": "bash", "arguments": "{}"}}
            ]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        ]);
        assert_eq!(
            chunks,
            vec![
                ChunkPayload::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "bash".into()
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: "{}".into()
                },
            ]
        );
        assert_eq!(done.finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn an_oversized_tool_call_index_is_dropped_not_allocated() {
        // Regression: a malformed `"index": 4294967295` used to grow the
        // accumulator vector by billions of entries (OOM).
        let (chunks, _) = chunks(&[json!({"choices": [{"delta": {"tool_calls": [
            {"index": 4294967295u32, "id": "call_x", "function": {"name": "bash"}}
        ]}}]})]);
        assert!(chunks.is_empty());
    }

    #[test]
    fn finish_reasons_map_and_unknown_is_stop() {
        assert_eq!(
            map_finish_reason(Some("tool_calls")),
            FinishReason::ToolCalls
        );
        assert_eq!(
            map_finish_reason(Some("function_call")),
            FinishReason::ToolCalls
        );
        assert_eq!(map_finish_reason(Some("length")), FinishReason::Length);
        assert_eq!(
            map_finish_reason(Some("content_filter")),
            FinishReason::ContentFilter
        );
        assert_eq!(map_finish_reason(Some("something_new")), FinishReason::Stop);
        assert_eq!(map_finish_reason(None), FinishReason::Stop);
    }

    #[test]
    fn cancel_reports_cancelled_with_the_usage_so_far() {
        let mut state = StreamState::default();
        let chunk: OpenAiStreamChunk =
            serde_json::from_value(json!({"usage": {"prompt_tokens": 7, "completion_tokens": 1}}))
                .unwrap();
        state.ingest(chunk, &mut |_| {});
        let done = state.finish_cancelled();
        assert_eq!(done.finish_reason, FinishReason::Cancelled);
        assert_eq!(done.usage.unwrap().prompt_tokens, Some(7));
    }
}
