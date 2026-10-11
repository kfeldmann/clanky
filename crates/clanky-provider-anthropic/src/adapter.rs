//! Request/response adapter: clanky protocol ⇄ Anthropic Messages.
//!
//! Shape of the mapping (planning, "Adapter mapping notes"):
//! - `system` is a top-level parameter, not a message; the identity block is
//!   prepended on the OAuth surface (spike-confirmed: required to identify
//!   as Claude Code, and the model answers *as* Claude Code with it).
//! - assistant `tool_calls` ⇄ `tool_use` content blocks; `role: "tool"`
//!   results are sent as a `user` message of `tool_result` blocks.
//! - `thinking` budget → `thinking: {type: "enabled", budget_tokens}`.
//!   Sampling values pass through verbatim even when thinking is on:
//!   Anthropic rejects `temperature` other than 1 in that combination
//!   (spike-confirmed 400), and per the project invariants that provider
//!   rejection is the actionable error we surface — the user's value is
//!   never silently dropped.

use clanky_protocol::{ChatMessage, FinishReason, Sampling, Thinking, Tool, Usage};
use serde_json::{Value, json};

/// System block every OAuth request must start with (spike-confirmed:
/// identifies the caller to the OAuth surface; the model then answers as
/// Claude Code — visible in the captured fixtures).
pub const IDENTITY_SYSTEM_TEXT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// `max_tokens` used when the request carries none.
pub const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// Serialize protocol chat messages into an Anthropic `/v1/messages` body.
///
/// `system` messages (clanky allows several) are concatenated — in wire
/// order — into the system parameter after the identity block.
pub fn build_request_body(
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
    sampling: Option<&Sampling>,
    thinking: Option<Thinking>,
    stream: bool,
) -> Result<String, clanky_protocol::Error> {
    let mut system_blocks = vec![json!({"type": "text", "text": IDENTITY_SYSTEM_TEXT})];
    let mut conversation: Vec<Value> = Vec::new();

    for message in messages {
        match message {
            ChatMessage::System { content } => {
                system_blocks.push(json!({"type": "text", "text": content}));
            }
            ChatMessage::User { content } => {
                conversation.push(json!({"role": "user", "content": content}));
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                // Blocks: text first (if any), then one tool_use per call.
                let mut blocks = Vec::new();
                if !content.is_empty() {
                    blocks.push(json!({"type": "text", "text": content}));
                }
                for call in tool_calls.iter().flatten() {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                if blocks.is_empty() {
                    // An assistant message with neither text nor calls is not
                    // representable on the wire; skip it.
                    continue;
                }
                conversation.push(json!({"role": "assistant", "content": blocks}));
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => {
                // Anthropic wants tool_result in a user message, immediately
                // after the tool_use. Consecutive tool results are grouped.
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": content,
                });
                match conversation.last_mut() {
                    Some(last) if last["role"] == "user" && last["content"].is_array() => {
                        last["content"].as_array_mut().expect("array").push(block);
                    }
                    _ => conversation.push(json!({"role": "user", "content": [block]})),
                }
            }
        }
    }

    // Sampling entries pass through verbatim (spec §5): `temperature`,
    // `top_p`, `top_k`, `stop_sequences`, … whatever the Messages API
    // accepts. The user's values are sent as-is even when thinking is on —
    // Anthropic rejects `temperature` other than 1 in that combination
    // (spike-confirmed 400), and per the project invariants that provider
    // rejection is the actionable error we surface, never a silent drop.
    let mut body = json!({});
    let obj = body.as_object_mut().expect("object literal");
    if let Some(sampling) = sampling {
        for (key, value) in sampling.iter() {
            obj.insert(key.clone(), value.clone());
        }
    }

    // `max_tokens` is required by the Messages API: the user's value is used
    // as-is; the default fills the gap only when none was supplied.
    obj.entry("max_tokens")
        .or_insert_with(|| json!(DEFAULT_MAX_TOKENS));

    // Streaming on by default (M11b): `stream: true` buys SSE deltas, which
    // map onto the protocol's chunks. As everywhere else, the user's own
    // `stream` entry in `sampling` wins — a non-streaming request is legal
    // (`post_message`'s one-shot shape answers it too).
    obj.entry("stream").or_insert(json!(stream));

    // The conversation shape belongs to the protocol and cannot be
    // overridden through `sampling`.
    obj.insert("model".into(), json!(model));
    obj.insert("messages".into(), conversation.into());
    obj.insert("system".into(), system_blocks.into());
    if let Some(tools) = tools.filter(|t| !t.is_empty()) {
        obj.insert(
            "tools".into(),
            Value::Array(tools.iter().map(anthropic_tool).collect()),
        );
    }
    // Derived thinking: only sent when a positive budget was provided, and
    // only to fill the gap — a `thinking` block the user set in `sampling`
    // wins over it.
    if !obj.contains_key("thinking")
        && let Some(budget) = thinking.and_then(|t| t.budget_tokens).filter(|&n| n > 0)
    {
        obj.insert(
            "thinking".into(),
            json!({
                "type": "enabled",
                "budget_tokens": budget,
            }),
        );
    }

    serde_json::to_string(&body)
        .map_err(|e| clanky_protocol::Error::Protocol(format!("cannot serialize request: {e}")))
}

fn anthropic_tool(tool: &Tool) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters.clone().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    })
}

/// Anthropic response content block (parsed leniently).
#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct AnthropicBlock {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub input: Option<Value>,
}

/// Anthropic Messages response (parsed leniently).
#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct AnthropicResponse {
    #[serde(default)]
    pub content: Vec<AnthropicBlock>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
    /// Error body shape (`{"type":"error","error":{...}}`).
    #[serde(default)]
    pub error: Option<AnthropicErrorBody>,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct AnthropicErrorBody {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Translate a parsed Anthropic response into the protocol terminal status
/// plus the streamed events a non-streaming backend emits up front. Every
/// content block is emitted as exactly one chunk (text/thinking), tool_use
/// blocks as a start plus a single args chunk carrying the whole JSON.
/// Non-streaming translation of a whole Messages response (M11a's chat
/// path). The streaming path ([`StreamState`]) is now the default; this
/// stays for requests that asked `stream: false` and as the canonical
/// reference mapping the stream events reproduce.
#[allow(dead_code)]
pub fn response_to_events(
    response: &AnthropicResponse,
    sink: &mut dyn FnMut(clanky_protocol::ChunkPayload),
) -> ChatDoneParts {
    use clanky_protocol::ChunkPayload;
    // Tool calls are numbered in block order so the client's assembler
    // keeps them apart (multiple tool_use blocks in one turn are legal).
    let mut tool_index: u32 = 0;
    for block in &response.content {
        match block.r#type.as_str() {
            "text" => {
                if let Some(text) = block.text.as_deref().filter(|t| !t.is_empty()) {
                    sink(ChunkPayload::Text {
                        text: text.to_string(),
                    });
                }
            }
            "thinking" => {
                if let Some(text) = block.thinking.as_deref().filter(|t| !t.is_empty()) {
                    sink(ChunkPayload::Thinking {
                        text: text.to_string(),
                    });
                }
            }
            "tool_use" => {
                let id = block.id.clone().unwrap_or_default();
                let name = block.name.clone().unwrap_or_default();
                sink(ChunkPayload::ToolCallStart {
                    index: tool_index,
                    id,
                    name,
                });
                let args = block.input.clone().unwrap_or_else(|| json!({}));
                sink(ChunkPayload::ToolCallArgs {
                    index: tool_index,
                    args_chunk: args.to_string(),
                });
                tool_index += 1;
            }
            _ => {}
        }
    }
    ChatDoneParts {
        finish_reason: map_stop_reason(response.stop_reason.as_deref()),
        usage: response.usage.as_ref().map(usage_from_anthropic),
    }
}

/// Terminal status parts of a chat turn.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatDoneParts {
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
}

fn map_stop_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("tool_use") => FinishReason::ToolCalls,
        Some("max_tokens") => FinishReason::Length,
        Some("refusal") | Some("sensitive") => FinishReason::ContentFilter,
        // `end_turn`, `stop_sequence`, `pause_turn` and anything unknown map
        // to a plain stop; only Clanky itself produces `cancelled`.
        _ => FinishReason::Stop,
    }
}

// ---------------------------------------------------------------------------
// Streaming (M11b)
//

/// One parsed Anthropic stream event (parsed leniently; `type` discriminates).
///
/// The wire shapes, from the pinned captures in `tests/fixtures/stream-*.txt`:
///
/// - `message_start`: `{"type":"message_start","message":{...,"usage":{...}}}`
/// - `content_block_start`: `{"type":"content_block_start","index":0,
///   "content_block":{"type":"text"|"thinking"|"tool_use","id":...,"name":...}}`
/// - `content_block_delta`: `{"type":"content_block_delta","index":0,
///   "delta":{"type":"text_delta","text":...}|{"type":"thinking_delta",...}|
///   {"type":"input_json_delta","partial_json":...}}`
/// - `message_delta`: `{"type":"message_delta","delta":{"stop_reason":...},
///   "usage":{...}}` (the usage here is cumulative output_tokens) — note the
///   stop reason rides in the same `delta` field a content_block_delta uses
/// - `message_stop`: `{"type":"message_stop"}` (terminal)
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct StreamEvent {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub index: u32,
    /// `content_block` of a `content_block_start`.
    #[serde(default)]
    pub content_block: Option<AnthropicBlock>,
    /// `delta` of a `content_block_delta` (text/thinking/partial_json) and
    /// of a `message_delta` (stop_reason) — the wire reuses the field.
    #[serde(default)]
    pub delta: Option<StreamDelta>,
    /// `usage` of a `message_delta` (cumulative, authoritative).
    #[serde(default)]
    pub usage: Option<AnthropicUsage>,
    /// `message` of a `message_start` (usage only).
    #[serde(default)]
    pub message: Option<AnthropicResponse>,
}

#[derive(Debug, Clone, PartialEq, Default, serde::Deserialize)]
pub struct StreamDelta {
    #[serde(default)]
    pub r#type: String,
    /// `message_delta` only: the terminal stop reason.
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub thinking: Option<String>,
    #[serde(default)]
    pub partial_json: Option<String>,
}

/// Stream state, mirroring the wire exactly: one block per `index`, tool
/// calls numbered by announcement order so the client's assembler (which
/// keys on a 0..n `toolCall` index) keeps them apart.
#[derive(Debug, Default)]
pub struct StreamState {
    /// Content block types by wire `index` (from `content_block_start`).
    blocks: std::collections::HashMap<u32, String>,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
    /// Protocol tool-call index of the next tool_use block.
    tool_index: u32,
}

impl StreamState {
    /// Ingest one stream event, translating it into protocol chunks.
    /// Returns `true` for the terminal `message_stop` event.
    pub fn ingest(
        &mut self,
        event: &StreamEvent,
        sink: &mut dyn FnMut(clanky_protocol::ChunkPayload),
    ) -> bool {
        use clanky_protocol::ChunkPayload;
        match event.r#type.as_str() {
            "message_start" => {
                if let Some(usage) = event.message.as_ref().and_then(|m| m.usage.as_ref()) {
                    self.usage = Some(usage_from_anthropic(usage));
                }
                false
            }
            "content_block_start" => {
                let block_type = event
                    .content_block
                    .as_ref()
                    .map(|b| b.r#type.clone())
                    .unwrap_or_default();
                self.blocks.insert(event.index, block_type.clone());
                if block_type == "tool_use" {
                    let block = event.content_block.as_ref();
                    sink(ChunkPayload::ToolCallStart {
                        index: self.tool_index,
                        id: block.and_then(|b| b.id.clone()).unwrap_or_default(),
                        name: block.and_then(|b| b.name.clone()).unwrap_or_default(),
                    });
                    self.tool_index += 1;
                }
                false
            }
            "content_block_delta" => {
                let Some(delta) = event.delta.as_ref() else {
                    return false;
                };
                // Deltas for an unannounced block are dropped: the wire
                // guarantees a `content_block_start` first, and without it
                // there is no type to map the delta onto.
                if !self.blocks.contains_key(&event.index) {
                    return false;
                }
                match delta.r#type.as_str() {
                    "text_delta" => {
                        if let Some(text) = delta.text.as_deref().filter(|t| !t.is_empty()) {
                            sink(ChunkPayload::Text {
                                text: text.to_string(),
                            });
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.thinking.as_deref().filter(|t| !t.is_empty()) {
                            sink(ChunkPayload::Thinking {
                                text: text.to_string(),
                            });
                        }
                    }
                    // `input_json_delta` fragments are forwarded verbatim —
                    // Anthropic guarantees each block's fragments concatenate
                    // to one valid JSON object; empty fragments (an artifact
                    // of the API's chunking) pass through harmlessly.
                    "input_json_delta" => {
                        sink(ChunkPayload::ToolCallArgs {
                            index: self.tool_index_for_wire_index(event.index),
                            args_chunk: delta.partial_json.clone().unwrap_or_default(),
                        });
                    }
                    _ => {}
                }
                false
            }
            "message_delta" => {
                if let Some(stop) = event.delta.as_ref().and_then(|d| d.stop_reason.as_deref()) {
                    self.finish_reason = Some(map_stop_reason(Some(stop)));
                }
                // The final usage is cumulative; message_start's is a
                // first-draft snapshot. Either may be absent — keep
                // whichever is more recent.
                if let Some(usage) = event.usage.as_ref() {
                    self.usage = Some(usage_from_anthropic(usage));
                }
                false
            }
            "message_stop" => true,
            // `ping`, `content_block_stop`, and anything else carry no
            // protocol content.
            _ => false,
        }
    }

    /// Map a wire `content_block` index to the protocol tool-call index of
    /// the tool_use block it belongs to. Tool_use blocks may interleave with
    /// text/thinking ones, so the nth tool_use block has protocol index n-1
    /// regardless of its wire index. A wire index with no announced block is
    /// unreachable (ingest drops deltas for unannounced blocks); 0 is the
    /// safe fallback.
    fn tool_index_for_wire_index(&self, wire_index: u32) -> u32 {
        let mut sorted: Vec<u32> = self.blocks.keys().copied().collect();
        sorted.sort_unstable();
        let wire_position = sorted
            .iter()
            .position(|index| *index == wire_index)
            .map(|pos| pos as u32)
            .unwrap_or(0);
        sorted
            .iter()
            .take(wire_position as usize)
            .filter(|index| self.blocks.get(*index).is_some_and(|t| t == "tool_use"))
            .count() as u32
    }

    /// The usage reported so far (for a cancelled turn's terminal status).
    pub fn clone_usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Terminal status when the stream ran to its `message_stop`.
    pub fn finish(self) -> ChatDoneParts {
        ChatDoneParts {
            finish_reason: self.finish_reason.unwrap_or(FinishReason::Stop),
            usage: self.usage,
        }
    }
}

/// Anthropic usage → protocol usage (shared by the streaming and
/// non-streaming paths).
fn usage_from_anthropic(u: &AnthropicUsage) -> Usage {
    Usage {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        // Anthropic's cache fields do not map onto the protocol's OpenAI-style
        // `cached_tokens` (that is a *read* count); carry the read count so
        // the cost display can use it.
        cached_tokens: u.cache_read_input_tokens,
    }
}

/// Extract the human-readable message from an Anthropic error body.
pub fn extract_error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && let Some(message) = value.pointer("/error/message").and_then(Value::as_str)
    {
        return message.to_string();
    }
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clanky_protocol::{ChunkPayload, Error, ErrorCode, ToolCall};
    use serde_json::json;

    /// The spike fixtures: pinned live captures (redacted only where noted).
    const REQ_BASIC: &str = include_str!("../tests/fixtures/req-basic.json");
    const RESP_BASIC: &str = include_str!("../tests/fixtures/resp-basic.json");
    const REQ_TOOLS: &str = include_str!("../tests/fixtures/req-tools.json");
    const RESP_TOOLS: &str = include_str!("../tests/fixtures/resp-tools.json");
    const RESP_TOOLRESULT: &str = include_str!("../tests/fixtures/resp-toolresult.json");
    const RESP_THINKING: &str = include_str!("../tests/fixtures/resp-thinking.json");

    #[test]
    fn request_body_matches_the_captured_shape() {
        // Build a request equivalent to the spike capture and compare field
        // by field against the pinned fixture (the captured max_tokens is
        // what the spike explicitly set).
        let messages = vec![
            ChatMessage::system("You are terse."),
            ChatMessage::user("say hi"),
        ];
        let body: Value = serde_json::from_str(
            &build_request_body(
                "claude-haiku-4-5-20251001",
                &messages,
                None,
                None,
                None,
                false,
            )
            .unwrap(),
        )
        .unwrap();
        let fixture: Value = serde_json::from_str(REQ_BASIC).unwrap();
        assert_eq!(body["model"], fixture["model"]);
        assert_eq!(body["system"], fixture["system"]);
        assert_eq!(body["messages"], fixture["messages"]);
        assert_eq!(body["max_tokens"], 64_000, "default max_tokens");
        assert!(body.get("tools").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn system_messages_append_after_the_identity_block() {
        let messages = vec![
            ChatMessage::system("First."),
            ChatMessage::user("hi"),
            ChatMessage::system("Second."),
        ];
        let body: Value = serde_json::from_str(
            &build_request_body("m", &messages, None, None, None, false).unwrap(),
        )
        .unwrap();
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["text"], IDENTITY_SYSTEM_TEXT);
        assert_eq!(system[1]["text"], "First.");
        assert_eq!(system[2]["text"], "Second.");
    }

    #[test]
    fn tool_use_round_trips_to_tool_result_messages() {
        // The spike's second request: an assistant tool_use followed by a
        // tool result must become assistant tool_use + user tool_result.
        let messages = vec![
            ChatMessage::user("count the files in /tmp using the bash tool"),
            ChatMessage::Assistant {
                content: String::new(),
                tool_calls: Some(vec![ToolCall {
                    id: "toolu_01BhVetyfKsqmX56rNskCQuh".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls -1 /tmp | wc -l"}),
                }]),
            },
            ChatMessage::tool("toolu_01BhVetyfKsqmX56rNskCQuh", "42"),
        ];
        let body: Value = serde_json::from_str(
            &build_request_body(
                "claude-haiku-4-5-20251001",
                &messages,
                Some(&[Tool {
                    name: "bash".into(),
                    description: Some("Run a shell command and return its output".into()),
                    parameters: Some(
                        json!({"type":"object","properties":{"command":{"type":"string","description":"The shell command to run"}},"required":["command"]}),
                    ),
                }]),
                None,
                None,
                false,
            )
            .unwrap(),
        )
        .unwrap();
        let fixture: Value = serde_json::from_str(REQ_TOOLS).unwrap();
        assert_eq!(body["tools"], fixture["tools"]);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["content"], "42");
    }

    #[test]
    fn consecutive_tool_results_group_into_one_user_message() {
        let tool = Tool {
            name: "bash".into(),
            description: None,
            parameters: None,
        };
        let messages = vec![
            ChatMessage::user("do two things"),
            ChatMessage::Assistant {
                content: String::new(),
                tool_calls: Some(vec![
                    ToolCall {
                        id: "a".into(),
                        name: "bash".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "bash".into(),
                        arguments: json!({}),
                    },
                ]),
            },
            ChatMessage::tool("a", "one"),
            ChatMessage::tool("b", "two"),
        ];
        let body: Value = serde_json::from_str(
            &build_request_body("m", &messages, Some(&[tool]), None, None, false).unwrap(),
        )
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "two tool results share one user message");
        assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn thinking_sets_budget_and_passes_temperature_through() {
        let messages = vec![ChatMessage::user("hi")];
        let sampling = Sampling::from_iter([
            ("temperature".into(), json!(0.2)),
            ("top_p".into(), json!(0.9)),
        ]);
        let body: Value = serde_json::from_str(
            &build_request_body(
                "m",
                &messages,
                None,
                Some(&sampling),
                Some(Thinking {
                    budget_tokens: Some(1024),
                }),
                false,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024})
        );
        // The user's temperature goes out verbatim even with thinking on.
        // Anthropic rejects anything but 1 in that combination (spike-
        // confirmed 400) and that rejection is the actionable error we
        // surface — the value must never be silently dropped.
        assert_eq!(body["temperature"], json!(0.2));
        assert_eq!(body["top_p"], json!(0.9));
    }

    #[test]
    fn thinking_zero_keeps_temperature() {
        // Budget 0 means thinking is off on the wire; the user's temperature
        // must still go out.
        let messages = vec![ChatMessage::user("hi")];
        let sampling = Sampling::from_iter([("temperature".into(), json!(0.2))]);
        let body: Value = serde_json::from_str(
            &build_request_body(
                "m",
                &messages,
                None,
                Some(&sampling),
                Some(Thinking {
                    budget_tokens: Some(0),
                }),
                false,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(body.get("thinking").is_none());
        assert_eq!(body["temperature"], json!(0.2));
    }

    #[test]
    fn arbitrary_sampling_parameters_pass_through_verbatim() {
        let messages = vec![ChatMessage::user("hi")];
        let sampling = Sampling::from_iter([
            ("top_k".into(), json!(40)),
            ("stop_sequences".into(), json!(["\n\n"])),
            ("max_tokens".into(), json!(123)),
            // A `thinking` block the user sets themselves wins over the
            // budget-derived one.
            ("thinking".into(), json!({"type": "disabled"})),
        ]);
        let body: Value = serde_json::from_str(
            &build_request_body(
                "m",
                &messages,
                None,
                Some(&sampling),
                Some(Thinking {
                    budget_tokens: Some(1024),
                }),
                false,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(body["top_k"], json!(40));
        assert_eq!(body["stop_sequences"], json!(["\n\n"]));
        assert_eq!(
            body["max_tokens"],
            json!(123),
            "the user's max_tokens as-is"
        );
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
    }

    #[test]
    fn thinking_zero_is_off() {
        let messages = vec![ChatMessage::user("hi")];
        let body: Value = serde_json::from_str(
            &build_request_body(
                "m",
                &messages,
                None,
                None,
                Some(Thinking {
                    budget_tokens: Some(0),
                }),
                false,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn response_chunks_carry_text_then_tool_calls() {
        let response: AnthropicResponse = serde_json::from_str(RESP_TOOLS).unwrap();
        let mut events = Vec::new();
        let parts = response_to_events(&response, &mut |e| events.push(e));
        assert_eq!(parts.finish_reason, FinishReason::ToolCalls);
        assert_eq!(
            events,
            vec![
                ChunkPayload::ToolCallStart {
                    index: 0,
                    id: "toolu_01BhVetyfKsqmX56rNskCQuh".into(),
                    name: "bash".into()
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: json!({"command": "ls -1 /tmp | wc -l"}).to_string()
                },
            ]
        );
        assert_eq!(parts.usage.unwrap().prompt_tokens, Some(1244));
    }

    #[test]
    fn plain_text_response_streams_one_chunk() {
        let response: AnthropicResponse = serde_json::from_str(RESP_BASIC).unwrap();
        let mut events = Vec::new();
        let parts = response_to_events(&response, &mut |e| events.push(e));
        assert_eq!(parts.finish_reason, FinishReason::Stop);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ChunkPayload::Text { text } => assert!(text.starts_with("Hi")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn thinking_blocks_surface_as_thinking_chunks() {
        let response: AnthropicResponse = serde_json::from_str(RESP_THINKING).unwrap();
        let mut events = Vec::new();
        let parts = response_to_events(&response, &mut |e| events.push(e));
        assert_eq!(parts.finish_reason, FinishReason::Stop);
        assert!(matches!(&events[0], ChunkPayload::Thinking { .. }));
        assert!(matches!(&events[1], ChunkPayload::Text { .. }));
    }

    #[test]
    fn tool_result_response_is_stop() {
        let response: AnthropicResponse = serde_json::from_str(RESP_TOOLRESULT).unwrap();
        let mut events = Vec::new();
        let parts = response_to_events(&response, &mut |e| events.push(e));
        assert_eq!(parts.finish_reason, FinishReason::Stop);
        assert!(matches!(&events[0], ChunkPayload::Text { .. }));
    }

    #[test]
    fn error_bodies_extract_their_message() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"OAuth access token is invalid."}}"#;
        assert_eq!(
            extract_error_message(body),
            "OAuth access token is invalid."
        );
        assert_eq!(extract_error_message("not json"), "not json");
    }

    #[test]
    fn stop_reasons_map_onto_the_protocol() {
        assert_eq!(map_stop_reason(Some("end_turn")), FinishReason::Stop);
        assert_eq!(map_stop_reason(Some("stop_sequence")), FinishReason::Stop);
        assert_eq!(map_stop_reason(Some("pause_turn")), FinishReason::Stop);
        assert_eq!(map_stop_reason(Some("tool_use")), FinishReason::ToolCalls);
        assert_eq!(map_stop_reason(Some("max_tokens")), FinishReason::Length);
        assert_eq!(
            map_stop_reason(Some("refusal")),
            FinishReason::ContentFilter
        );
        assert_eq!(map_stop_reason(None), FinishReason::Stop);
    }

    #[test]
    fn protocol_errors_are_request_scoped() {
        // Sanity: the Error shape the provider hands back for a 401.
        let err = Error::Provider {
            code: ErrorCode::Auth,
            message: "401: OAuth access token is invalid.".into(),
            retryable: false,
            retry_after_ms: None,
        };
        assert!(err.to_string().starts_with("auth: "));
    }

    // --- streaming (M11b): pinned live-capture fixtures -------------------

    const STREAM_BASIC: &str = include_str!("../tests/fixtures/stream-basic.txt");
    const STREAM_THINKING: &str = include_str!("../tests/fixtures/stream-thinking.txt");
    const STREAM_TOOLS: &str = include_str!("../tests/fixtures/stream-tools.txt");

    /// The captured SSE text → the parsed event stream the provider consumes
    /// (`event:` lines dropped, `data:` payloads one per event).
    fn stream_events(text: &str) -> Vec<StreamEvent> {
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.strip_prefix(' ').unwrap_or(line))
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<StreamEvent>(line).expect("valid event"))
            .collect()
    }

    fn run_stream(text: &str) -> (Vec<clanky_protocol::ChunkPayload>, ChatDoneParts) {
        let mut chunks = Vec::new();
        let mut state = StreamState::default();
        for event in stream_events(text) {
            if state.ingest(&event, &mut |c| chunks.push(c)) {
                break;
            }
        }
        (chunks, state.finish())
    }

    #[test]
    fn plain_stream_assembles_text_usage_and_finish_reason() {
        let (chunks, parts) = run_stream(STREAM_BASIC);
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "1,2,3,4,5");
        // Deltas stayed per-event; nothing was coalesced or reordered.
        let deltas = chunks
            .iter()
            .filter(|c| matches!(c, ChunkPayload::Text { .. }))
            .count();
        assert_eq!(deltas, 3);
        assert_eq!(parts.finish_reason, FinishReason::Stop);
        assert_eq!(
            parts.usage,
            Some(Usage {
                // `message_delta`'s cumulative usage wins over
                // `message_start`'s snapshot (13 vs 1 output tokens).
                prompt_tokens: Some(685),
                completion_tokens: Some(13),
                cached_tokens: Some(0),
            })
        );
    }

    #[test]
    fn thinking_stream_separates_thinking_from_text() {
        let (chunks, parts) = run_stream(STREAM_THINKING);
        let thinking: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Thinking { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            thinking,
            "This is a straightforward math question. Let me calculate 17 * 23.\n\n17 * 23 = 17 * 20 + 17 * 3 = 340 + 51 = 391\n\nThe user asks me to think briefly, then answer. They want a terse response according to my instructions."
        );
        // The visible answer is clean: the thinking never leaks into it.
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "17 × 23 = 391");
        assert_eq!(parts.finish_reason, FinishReason::Stop);
        assert_eq!(parts.usage.as_ref().unwrap().completion_tokens, Some(95));
    }

    #[test]
    fn tool_stream_maps_start_and_arg_fragments_verbatim() {
        let (chunks, parts) = run_stream(STREAM_TOOLS);

        let starts: Vec<&ChunkPayload> = chunks
            .iter()
            .filter(|c| matches!(c, ChunkPayload::ToolCallStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1, "one tool call, announced once");
        match starts[0] {
            ChunkPayload::ToolCallStart { index, id, name } => {
                assert_eq!(*index, 0);
                assert_eq!(id, "toolu_01Ko64L89gKDZTEabFP8ZXrQ");
                assert_eq!(name, "bash");
            }
            _ => unreachable!(),
        }

        let args: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::ToolCallArgs { args_chunk, .. } => Some(args_chunk.as_str()),
                _ => None,
            })
            .collect();
        // The `partial_json` fragments concatenate (verbatim, empty leading
        // fragment and all) into one valid JSON object.
        assert_eq!(
            serde_json::from_str::<Value>(&args).unwrap(),
            json!({"command": "ls -la /tmp"})
        );
        assert_eq!(parts.finish_reason, FinishReason::ToolCalls);
        assert_eq!(parts.usage.as_ref().unwrap().completion_tokens, Some(71));
    }

    #[test]
    fn interleaved_tool_use_keeps_protocol_and_wire_indices_apart() {
        // Wire indices count all blocks; protocol indices count tool calls.
        // A thinking block (wire 0) before the tool_use (wire 1) must not
        // shift the tool call's protocol index.
        let events = [
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"bash"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ];
        let mut state = StreamState::default();
        let mut chunks = Vec::new();
        for raw in events {
            let event: StreamEvent = serde_json::from_str(raw).unwrap();
            state.ingest(&event, &mut |c| chunks.push(c));
        }
        assert!(matches!(chunks[0], ChunkPayload::Thinking { .. }));
        match &chunks[1] {
            ChunkPayload::ToolCallStart { index, .. } => assert_eq!(*index, 0),
            other => panic!("unexpected: {other:?}"),
        }
        match &chunks[2] {
            ChunkPayload::ToolCallArgs { index, args_chunk } => {
                assert_eq!(*index, 0, "tool call's protocol index");
                assert_eq!(args_chunk, "{}", "the single fragment verbatim");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn message_stop_is_terminal_and_unknown_events_are_ignored() {
        let mut state = StreamState::default();
        let mut chunks = Vec::new();
        for raw in [
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"future_event","index":9,"delta":{"type":"whatever"}}"#,
            r#"{"type":"message_stop"}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"late"}}"#,
        ] {
            let event: StreamEvent = serde_json::from_str(raw).unwrap();
            if state.ingest(&event, &mut |c| chunks.push(c)) {
                break;
            }
        }
        assert!(chunks.is_empty());
        // A turn cut short before message_stop still reports a stop
        // (usage absent — nothing was reported).
        let parts = state.finish();
        assert_eq!(parts.finish_reason, FinishReason::Stop);
        assert_eq!(parts.usage, None);
    }

    #[test]
    fn request_body_sets_stream_by_default_and_sampling_wins() {
        let messages = vec![ChatMessage::user("hi")];
        let streaming = serde_json::from_str::<Value>(
            &build_request_body("m", &messages, None, None, None, true).unwrap(),
        )
        .unwrap();
        assert_eq!(streaming["stream"], true, "the provider's streaming path");
        let non_streaming = serde_json::from_str::<Value>(
            &build_request_body("m", &messages, None, None, None, false).unwrap(),
        )
        .unwrap();
        assert_eq!(non_streaming["stream"], false);
        // The user's own `stream` in sampling wins over the derived value.
        let sampling = Sampling::from_iter([("stream".into(), json!(false))]);
        let user_wins = serde_json::from_str::<Value>(
            &build_request_body("m", &messages, None, Some(&sampling), None, true).unwrap(),
        )
        .unwrap();
        assert_eq!(
            user_wins["stream"], false,
            "the user's value is authoritative"
        );
    }
}
