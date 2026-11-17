//! Request/response adapter: clanky protocol ⇄ Anthropic Messages.
//!
//! Shape of the mapping (planning, "Adapter mapping notes"):
//! - `system` is a top-level parameter, not a message; the Claude Code
//!   identity block is prepended on the OAuth surface only (spike-confirmed:
//!   required to identify as Claude Code, and the model answers *as* Claude
//!   Code with it). The API-key surface is the documented plain one — no
//!   identity block, matching the plain headers `apply_auth` sends there.
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

/// Whether the request identifies as Claude Code. The identity system
/// block belongs to the OAuth surface only — its substitute for not
/// presenting real Clanky identity — while the API-key surface is the
/// documented plain third-party one. Matches the header split in
/// `backend::apply_auth`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// OAuth surface: prepend [`IDENTITY_SYSTEM_TEXT`] to `system`.
    ClaudeCode,
    /// API-key surface: send the user's system content only.
    Plain,
}

impl From<&crate::backend::AuthMode> for Identity {
    fn from(auth: &crate::backend::AuthMode) -> Self {
        match auth {
            crate::backend::AuthMode::Bearer(_) => Identity::ClaudeCode,
            crate::backend::AuthMode::ApiKey(_) => Identity::Plain,
        }
    }
}

/// `max_tokens` used when the request carries none.
pub const DEFAULT_MAX_TOKENS: u32 = 64_000;

/// Serialize protocol chat messages into an Anthropic `/v1/messages` body.
///
/// `system` messages (clanky allows several) are concatenated — in wire
/// order — into the system parameter, after the identity block on the
/// Claude Code (OAuth) surface.
pub fn build_request_body(
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
    sampling: Option<&Sampling>,
    thinking: Option<Thinking>,
    stream: bool,
    identity: Identity,
) -> Result<String, clanky_protocol::Error> {
    let mut system_blocks = Vec::new();
    if identity == Identity::ClaudeCode {
        system_blocks.push(json!({"type": "text", "text": IDENTITY_SYSTEM_TEXT}));
    }
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
                // The round's reasoning blocks (M23): passed back verbatim,
                // in wire order, *before* the text and tool_use blocks —
                // the spike measured that blocks placed after `tool_use`
                // silently disable thinking (finding 6). They are sent
                // exactly as received: no reordering, rewriting, or
                // dropping (an altered signature or data is a live 400).
                // Anthropic filters and bills only the blocks it keeps, so
                // over-sending is free; under-sending is what costs
                // quality (item 6).
                thinking,
            } => {
                let mut blocks: Vec<Value> =
                    thinking.iter().flatten().map(thinking_block_wire).collect();
                // Blocks: thinking first (if any), then text (if any), then
                // one tool_use per call.
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
                images,
            } => {
                // Anthropic wants tool_result in a user message, immediately
                // after the tool_use. Consecutive tool results are grouped.
                // The result's inline images ride inside the tool_result
                // block: its `content` may be an array mixing text + image
                // blocks (Messages API), so no synthetic user turn is
                // injected and replay stays identical across providers.
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_call_id,
                    "content": anthropic_tool_result_content(content, images.as_deref()),
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
    // `stream` entry in `sampling` wins — a non-streaming request is legal,
    // and the provider routes it to `post_message` (see `wants_stream`).
    // The `stream` value computed here and the transport choice in
    // `provider::chat` must agree; both read the same `sampling` entry.
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

/// An assistant message's reasoning block on the request wire, exactly as
/// it was received (M23 constraint 1: the only correct transformation is
/// identity — Anthropic answers any alteration of the `signature` or the
/// `data` with a 400). A `thinking` block always carries its `thinking`
/// text field (possibly empty — the `display: "omitted"` mainline); a
/// `redacted` block carries `data` instead and has no text.
fn thinking_block_wire(block: &clanky_protocol::ThinkingBlock) -> Value {
    match block.kind {
        clanky_protocol::ThinkingBlockKind::Thinking => {
            let mut wire = json!({
                "type": "thinking",
                "thinking": block.text.as_deref().unwrap_or(""),
            });
            if let Some(signature) = block.signature.as_deref() {
                wire["signature"] = json!(signature);
            }
            wire
        }
        clanky_protocol::ThinkingBlockKind::Redacted => {
            json!({
                "type": "redacted_thinking",
                "data": block.data.as_deref().unwrap_or(""),
            })
        }
    }
}

fn anthropic_tool(tool: &Tool) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.parameters.clone().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    })
}

/// The `content` of a `tool_result` block: a plain string when the result
/// carries no images (the common case), an array of text + image blocks
/// otherwise. Anthropic's image source blocks take base64 data and the
/// media type directly (no data-URI indirection).
fn anthropic_tool_result_content(
    text: &str,
    images: Option<&[clanky_protocol::ImagePart]>,
) -> Value {
    let Some(images) = images.filter(|imgs| !imgs.is_empty()) else {
        return json!(text);
    };
    let mut blocks = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    for image in images {
        blocks.push(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": image.media_type.as_str(),
                "data": image.data_base64,
            },
        }));
    }
    Value::Array(blocks)
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
    /// A `thinking` block's signature (M23): the payload a later request
    /// must pass back verbatim. Present on the `content_block_start` of a
    /// streaming thinking block as an empty string, then filled by one
    /// `signature_delta`.
    #[serde(default)]
    pub signature: Option<String>,
    /// A `redacted_thinking` block's opaque payload (M23). The whole block
    /// arrives in one `content_block_start`; there are no deltas for it.
    #[serde(default)]
    pub data: Option<String>,
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
/// Serves the provider's non-streaming path (a `stream = false` entry in
/// `sampling` — the user's value is honored end to end) and is the
/// canonical reference mapping the stream events reproduce.
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
                // The passback payload (M23): signature or nothing — an
                // unsigned thinking block is a 400 on Anthropic, while a
                // missing one is documented as allowed.
                if let Some(signature) = block.signature.as_deref().filter(|s| !s.is_empty()) {
                    sink(ChunkPayload::ThinkingBlock {
                        block: clanky_protocol::ThinkingBlock::thinking(
                            block.thinking.clone().unwrap_or_default(),
                            Some(signature.to_string()),
                        ),
                    });
                }
            }
            "redacted_thinking" => {
                // No display text exists for these; only the opaque payload,
                // which must reach the client to be passed back.
                if let Some(data) = block.data.as_deref().filter(|d| !d.is_empty()) {
                    sink(ChunkPayload::ThinkingBlock {
                        block: clanky_protocol::ThinkingBlock::redacted(data.to_string()),
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
        // `pause_turn`: Anthropic paused a long-running turn server-side.
        // Its documented contract is to send the partial response back and
        // request again — the protocol's Continuable, which Clanky core
        // resumes automatically (mapping it to a plain stop would silently
        // truncate the turn and drop that contract).
        Some("pause_turn") => FinishReason::Continuable,
        // `end_turn`, `stop_sequence` and anything unknown map to a plain
        // stop (a future stop reason should never lose the turn's output);
        // only Clanky itself produces `cancelled`.
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
    /// `signature_delta` only (M23): one fragment of a thinking block's
    /// signature. Anthropic sends exactly one per block today, but the
    /// accumulator joins them so a future multi-fragment shape still
    /// reassembles correctly.
    #[serde(default)]
    pub signature: Option<String>,
}

/// One content block as the stream built it (M23): the wire type plus the
/// passback payload accumulated for it. `signature` is joined from
/// `signature_delta`s; `data` is a `redacted_thinking` block's payload,
/// which arrives whole on `content_block_start`; `text` is the block's own
/// copy of its thinking text (the display stream is separate).
#[derive(Debug, Clone, Default)]
struct StreamBlock {
    kind: String,
    text: String,
    signature: String,
    data: Option<String>,
    /// Whether the block's passback payload has already been handed to the
    /// client (so the `message_stop` sweep does not emit it twice).
    closed: bool,
}

/// Is this wire block type one that carries a passback payload?
fn is_reasoning_block(kind: &str) -> bool {
    matches!(kind, "thinking" | "redacted_thinking")
}

/// Stream state, mirroring the wire exactly: one block per `index`, tool
/// calls numbered by announcement order so the client's assembler (which
/// keys on a 0..n `toolCall` index) keeps them apart.
#[derive(Debug, Default)]
pub struct StreamState {
    /// Per-block accumulation by wire `index` (from `content_block_start`):
    /// the wire type plus the passback payload (M23).
    blocks: std::collections::HashMap<u32, StreamBlock>,
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
                let started = event.content_block.clone().unwrap_or_default();
                self.blocks.insert(
                    event.index,
                    StreamBlock {
                        kind: started.r#type.clone(),
                        signature: started.signature.clone().unwrap_or_default(),
                        // A `redacted_thinking` block arrives whole on its
                        // start event — the wire sends no deltas for it.
                        data: started.data.clone(),
                        text: String::new(),
                        closed: false,
                    },
                );
                if started.r#type == "tool_use" {
                    sink(ChunkPayload::ToolCallStart {
                        index: self.tool_index,
                        id: started.id.clone().unwrap_or_default(),
                        name: started.name.clone().unwrap_or_default(),
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
                let Some(block) = self.blocks.get_mut(&event.index) else {
                    return false;
                };
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
                            // The block's own copy is what rides back to the
                            // provider; the chunk is the display stream.
                            block.text.push_str(text);
                            sink(ChunkPayload::Thinking {
                                text: text.to_string(),
                            });
                        }
                    }
                    // One `signature_delta` per thinking block today; the
                    // accumulator joins them so a multi-fragment shape would
                    // still reassemble.
                    "signature_delta" => {
                        if let Some(signature) = delta.signature.as_deref() {
                            block.signature.push_str(signature);
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
            // A block is complete: hand back its passback payload as one
            // terminal chunk (M23).
            "content_block_stop" => {
                self.close_block(event.index, sink);
                false
            }
            "message_stop" => {
                // A stream that ended without a `content_block_stop` for a
                // reasoning block still has its payload; close whatever is
                // left, in wire order, so nothing is silently dropped.
                let mut open: Vec<u32> = self
                    .blocks
                    .iter()
                    .filter(|(_, block)| !block.closed && is_reasoning_block(&block.kind))
                    .map(|(index, _)| *index)
                    .collect();
                open.sort_unstable();
                for index in open {
                    self.close_block(index, sink);
                }
                true
            }
            // `ping` and anything else carry no protocol content.
            _ => false,
        }
    }

    /// Emit the reasoning block at `index` as one terminal
    /// [`ChunkPayload::ThinkingBlock`] and mark it closed, so the
    /// `message_stop` sweep does not emit it twice.
    ///
    /// The payload rule (spec §5, M23 constraint 2 — "signature or
    /// nothing"): a `thinking` block without a signature is not emitted at
    /// all, because an unsigned block is a 400 on Anthropic and a missing
    /// block is documented as allowed. A `redacted_thinking` block without
    /// its data is likewise dropped. Text and `tool_use` blocks carry no
    /// passback payload.
    fn close_block(&mut self, index: u32, sink: &mut dyn FnMut(clanky_protocol::ChunkPayload)) {
        let Some(block) = self.blocks.get_mut(&index) else {
            return;
        };
        if block.closed || !is_reasoning_block(&block.kind) {
            return;
        }
        block.closed = true;
        let payload = match block.kind.as_str() {
            "thinking" if !block.signature.is_empty() => {
                // `thinking` is always present on the wire, even when empty
                // (`display: "omitted"` models): the block goes back exactly
                // as received, so the empty string is kept, not dropped.
                Some(clanky_protocol::ThinkingBlock::thinking(
                    std::mem::take(&mut block.text),
                    Some(std::mem::take(&mut block.signature)),
                ))
            }
            "redacted_thinking" => block
                .data
                .take()
                .filter(|data| !data.is_empty())
                .map(clanky_protocol::ThinkingBlock::redacted),
            _ => None,
        };
        if let Some(block) = payload {
            sink(clanky_protocol::ChunkPayload::ThinkingBlock { block });
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
            .filter(|index| {
                self.blocks
                    .get(*index)
                    .is_some_and(|block| block.kind == "tool_use")
            })
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
                Identity::ClaudeCode,
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
            &build_request_body(
                "m",
                &messages,
                None,
                None,
                None,
                false,
                Identity::ClaudeCode,
            )
            .unwrap(),
        )
        .unwrap();
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 3);
        assert_eq!(system[0]["text"], IDENTITY_SYSTEM_TEXT);
        assert_eq!(system[1]["text"], "First.");
        assert_eq!(system[2]["text"], "Second.");
    }

    #[test]
    fn the_api_key_path_sends_no_identity_block() {
        // The identity block is the OAuth surface's substitute for not
        // presenting real Clanky identity; the documented key surface gets
        // the user's system content only.
        let messages = vec![ChatMessage::system("You are terse.")];
        let body: Value = serde_json::from_str(
            &build_request_body("m", &messages, None, None, None, false, Identity::Plain).unwrap(),
        )
        .unwrap();
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["text"], "You are terse.");
        assert!(
            !serde_json::to_string(&body["system"])
                .unwrap()
                .contains("Claude Code"),
        );

        // And the same request on the OAuth surface keeps the block.
        let oauth: Value = serde_json::from_str(
            &build_request_body(
                "m",
                &messages,
                None,
                None,
                None,
                false,
                Identity::ClaudeCode,
            )
            .unwrap(),
        )
        .unwrap();
        let oauth_system = oauth["system"].as_array().unwrap();
        assert_eq!(oauth_system.len(), 2);
        assert_eq!(oauth_system[0]["text"], IDENTITY_SYSTEM_TEXT);
    }

    #[test]
    fn the_identity_follows_the_auth_mode() {
        let creds = crate::auth::Credentials {
            access_token: "t".into(),
            refresh_token: "r".into(),
            expires_at: 0,
            refresh_token_expires_at: None,
        };
        assert_eq!(
            Identity::from(&crate::backend::AuthMode::Bearer(creds)),
            Identity::ClaudeCode
        );
        assert_eq!(
            Identity::from(&crate::backend::AuthMode::ApiKey("k".into())),
            Identity::Plain
        );
    }

    #[test]
    fn tool_use_round_trips_to_tool_result_messages() {
        // The spike's second request: an assistant tool_use followed by a
        // tool result must become assistant tool_use + user tool_result.
        let messages = vec![
            ChatMessage::user("count the files in /tmp using the bash tool"),
            ChatMessage::Assistant {
                content: String::new(),
                thinking: None,
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
                Identity::ClaudeCode,
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
    fn tool_result_images_become_image_blocks_inside_the_tool_result() {
        // The decided shape (M20): the images ride *inside* the
        // tool_result block — its `content` may be an array mixing text
        // and image blocks — never as a synthetic follow-up user message.
        let messages = vec![
            ChatMessage::user("show me /work/sample.jpg"),
            ChatMessage::Assistant {
                content: String::new(),
                thinking: None,
                tool_calls: Some(vec![ToolCall {
                    id: "toolu_01".into(),
                    name: "view-image".into(),
                    arguments: json!({"path": "/work/sample.jpg"}),
                }]),
            },
            ChatMessage::tool_with_images(
                "toolu_01",
                "256 kb",
                vec![clanky_protocol::ImagePart {
                    data_base64: "aGVsbG8=".into(),
                    media_type: clanky_protocol::MediaType::Jpeg,
                }],
            ),
        ];
        let body: Value = serde_json::from_str(
            &build_request_body(
                "claude-haiku-4-5-20251001",
                &messages,
                None,
                None,
                None,
                false,
                Identity::ClaudeCode,
            )
            .unwrap(),
        )
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "no synthetic user turn is injected");
        let result = &msgs[2]["content"][0];
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_use_id"], "toolu_01");
        let content = result["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "256 kb");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(content[1]["source"]["data"], "aGVsbG8=");
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
                thinking: None,
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
            &build_request_body(
                "m",
                &messages,
                Some(&[tool]),
                None,
                None,
                false,
                Identity::ClaudeCode,
            )
            .unwrap(),
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
                Identity::ClaudeCode,
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
                Identity::ClaudeCode,
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
                Identity::ClaudeCode,
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
                Identity::ClaudeCode,
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
        // M23: the block's passback payload follows the display delta — the
        // text plus the signature that makes it re-sendable.
        match &events[1] {
            ChunkPayload::ThinkingBlock { block } => {
                assert_eq!(block.kind, clanky_protocol::ThinkingBlockKind::Thinking);
                assert!(block.signature.as_deref().is_some_and(|s| s.len() > 100));
                assert!(
                    block
                        .text
                        .as_deref()
                        .is_some_and(|t| t.starts_with("The user"))
                );
            }
            other => panic!("expected the block payload, got {other:?}"),
        }
        assert!(matches!(&events[2], ChunkPayload::Text { .. }));
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
        // A server-side pause is continuable, not a plain stop: Anthropic's
        // documented resume is to send the partial response back and
        // request again (a plain stop would truncate the turn and drop
        // that contract).
        assert_eq!(
            map_stop_reason(Some("pause_turn")),
            FinishReason::Continuable
        );
        assert_eq!(map_stop_reason(Some("tool_use")), FinishReason::ToolCalls);
        assert_eq!(map_stop_reason(Some("max_tokens")), FinishReason::Length);
        assert_eq!(
            map_stop_reason(Some("refusal")),
            FinishReason::ContentFilter
        );
        assert_eq!(map_stop_reason(None), FinishReason::Stop);
        // An unknown stop reason falls back to Stop, never Continuable.
        assert_eq!(
            map_stop_reason(Some("model_context_window_exceeded")),
            FinishReason::Stop
        );
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

        // M23: the block's passback payload rides its own terminal chunk —
        // the captured signature verbatim, plus the thinking text.
        let blocks: Vec<&clanky_protocol::ThinkingBlock> = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::ThinkingBlock { block } => Some(block),
                _ => None,
            })
            .collect();
        assert_eq!(blocks.len(), 1, "one block, emitted once");
        let block = blocks[0];
        assert_eq!(block.kind, clanky_protocol::ThinkingBlockKind::Thinking);
        assert_eq!(block.text.as_deref(), Some(thinking.as_str()));
        assert!(
            block
                .signature
                .as_deref()
                .is_some_and(|s| s.starts_with("EvUDCrgBCBIYAipA1VbPUptbMjuV22EODvAckD3Q")),
            "the captured signature verbatim: {:?}",
            block.signature
        );
    }

    /// A live-capture-shaped event stream for the M23 wire shapes (finding
    /// 7): `content_block_start` for a thinking block carries
    /// `"signature":""`; the signature arrives as one `signature_delta`; a
    /// `redacted_thinking` block arrives whole on its start event with no
    /// deltas at all.
    fn run_raw_stream(events: &[&str]) -> Vec<ChunkPayload> {
        let mut chunks = Vec::new();
        let mut state = StreamState::default();
        for raw in events {
            let event: StreamEvent = serde_json::from_str(raw).expect("valid event");
            if state.ingest(&event, &mut |c| chunks.push(c)) {
                break;
            }
        }
        chunks
    }

    fn blocks_of(chunks: &[ChunkPayload]) -> Vec<clanky_protocol::ThinkingBlock> {
        chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::ThinkingBlock { block } => Some(block.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_empty_text_signed_block_is_emitted() {
        // The `display: "omitted"` mainline (finding 3): the thinking text
        // is empty and the signature is the whole payload. It must be
        // emitted, with the empty text kept rather than dropped.
        let chunks = run_raw_stream(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-only"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"answer"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let blocks = blocks_of(&chunks);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, clanky_protocol::ThinkingBlockKind::Thinking);
        assert_eq!(blocks[0].text.as_deref(), Some(""), "empty text is kept");
        assert_eq!(blocks[0].signature.as_deref(), Some("sig-only"));
        assert!(
            chunks
                .iter()
                .all(|c| !matches!(c, ChunkPayload::Thinking { .. })),
            "an empty thinking delta emits no display chunk"
        );
    }

    #[test]
    fn a_redacted_thinking_block_arrives_whole_on_its_start_event() {
        let chunks = run_raw_stream(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque-blob"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let blocks = blocks_of(&chunks);
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            blocks[0],
            clanky_protocol::ThinkingBlock::redacted("opaque-blob")
        );
        assert_eq!(blocks[0].text, None, "a redacted block has no text");
    }

    #[test]
    fn several_blocks_keep_wire_order() {
        let chunks = run_raw_stream(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-a"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"blob-b"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"third"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"signature_delta","signature":"sig-c"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let blocks = blocks_of(&chunks);
        assert_eq!(
            blocks,
            [
                clanky_protocol::ThinkingBlock::thinking("", Some("sig-a".into())),
                clanky_protocol::ThinkingBlock::redacted("blob-b"),
                clanky_protocol::ThinkingBlock::thinking("third", Some("sig-c".into())),
            ],
            "consecutive thinking blocks keep the order the model generated"
        );
    }

    #[test]
    fn an_unsigned_block_is_not_emitted() {
        // Constraint 2 ("signature or nothing"): a block whose signature
        // never arrived — a round cut short mid-stream — is dropped rather
        // than sent back unsigned (a 400 on Anthropic).
        let chunks = run_raw_stream(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"half a thought"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        assert!(blocks_of(&chunks).is_empty(), "no forged block: {chunks:?}");
        // The display text still streamed.
        assert!(matches!(&chunks[0], ChunkPayload::Thinking { text } if text == "half a thought"));
    }

    #[test]
    fn a_block_without_a_content_block_stop_still_closes_at_message_stop() {
        let chunks = run_raw_stream(&[
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let blocks = blocks_of(&chunks);
        assert_eq!(blocks.len(), 1, "closed by the terminal sweep, once");
        assert_eq!(blocks[0].signature.as_deref(), Some("sig"));
    }

    #[test]
    fn non_streaming_responses_carry_signature_and_redacted_data() {
        // The non-streaming path (`stream = false`) puts `signature` and
        // `data` directly on the content block (finding 7).
        let body = r#"{"content":[
            {"type":"thinking","thinking":"","signature":"sig-ns"},
            {"type":"redacted_thinking","data":"blob-ns"},
            {"type":"text","text":"hi"}
        ],"stop_reason":"end_turn"}"#;
        let response: AnthropicResponse = serde_json::from_str(body).unwrap();
        let mut events = Vec::new();
        response_to_events(&response, &mut |e| events.push(e));
        assert_eq!(
            blocks_of(&events),
            [
                clanky_protocol::ThinkingBlock::thinking("", Some("sig-ns".into())),
                clanky_protocol::ThinkingBlock::redacted("blob-ns"),
            ]
        );
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
            &build_request_body("m", &messages, None, None, None, true, Identity::ClaudeCode)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(streaming["stream"], true, "the provider's streaming path");
        let non_streaming = serde_json::from_str::<Value>(
            &build_request_body(
                "m",
                &messages,
                None,
                None,
                None,
                false,
                Identity::ClaudeCode,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(non_streaming["stream"], false);
        // The user's own `stream` in sampling wins over the derived value.
        let sampling = Sampling::from_iter([("stream".into(), json!(false))]);
        let user_wins = serde_json::from_str::<Value>(
            &build_request_body(
                "m",
                &messages,
                None,
                Some(&sampling),
                None,
                true,
                Identity::ClaudeCode,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            user_wins["stream"], false,
            "the user's value is authoritative"
        );
    }

    /// A tool-loop round with reasoning blocks, shaped like the live spike
    /// capture: assistant (thinking + tool_use), tool result, and a new
    /// user prompt — the history whose missing thinking blocks silently
    /// disabled thinking on the `4-x`/`4-6` models (finding 5).
    fn passback_messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage::user("count the files in /tmp using the bash tool"),
            ChatMessage::Assistant {
                content: String::new(),
                thinking: Some(vec![
                    clanky_protocol::ThinkingBlock::thinking(
                        "",
                        Some("EuUDCrgBCBIYAipAKS6N…".into()),
                    ),
                    clanky_protocol::ThinkingBlock::redacted("EmwKAh…".to_string()),
                ]),
                tool_calls: Some(vec![ToolCall {
                    id: "toolu_01BhVetyfKsqmX56rNskCQuh".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls -1 /tmp | wc -l"}),
                }]),
            },
            ChatMessage::tool("toolu_01BhVetyfKsqmX56rNskCQuh", "42"),
        ]
    }

    #[test]
    fn reasoning_blocks_are_passed_back_verbatim_before_the_text() {
        // M23 session 2, the Anthropic arm: the round's blocks ride the
        // assistant message as content blocks — first in the block list
        // (the spike measured that blocks after `tool_use` silently
        // disable thinking — finding 6), in wire order, unmodified.
        let body: Value = serde_json::from_str(
            &build_request_body(
                "m",
                &passback_messages(),
                None,
                None,
                None,
                false,
                Identity::Plain,
            )
            .unwrap(),
        )
        .unwrap();
        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        let blocks = assistant["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        // Thinking blocks, in the order the model produced them, before the
        // tool_use block.
        assert_eq!(
            blocks[0],
            json!({"type": "thinking", "thinking": "", "signature": "EuUDCrgBCBIYAipAKS6N…"}),
            "empty text kept: the display-omitted mainline"
        );
        assert_eq!(
            blocks[1],
            json!({"type": "redacted_thinking", "data": "EmwKAh…"})
        );
        assert_eq!(blocks[2]["type"], "tool_use");
        assert_eq!(blocks[2]["id"], "toolu_01BhVetyfKsqmX56rNskCQuh");
    }

    #[test]
    fn a_readable_thinking_block_rides_back_with_its_text() {
        // On models that return real text (`haiku-4-5` et al.) the block
        // carries both halves; the text is sent unmodified (the API
        // verifies only the signature and ignores the text — spike finding
        // 4), but sending it costs nothing and nothing may be altered.
        let messages = vec![
            ChatMessage::user("17*23"),
            ChatMessage::Assistant {
                content: "391".into(),
                thinking: Some(vec![clanky_protocol::ThinkingBlock::thinking(
                    "17*23 = 391",
                    Some("sig-1".into()),
                )]),
                tool_calls: None,
            },
        ];
        let body: Value = serde_json::from_str(
            &build_request_body("m", &messages, None, None, None, false, Identity::Plain).unwrap(),
        )
        .unwrap();
        let blocks = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2, "thinking block first, then the text");
        assert_eq!(
            blocks[0],
            json!({"type": "thinking", "thinking": "17*23 = 391", "signature": "sig-1"})
        );
        assert_eq!(blocks[1], json!({"type": "text", "text": "391"}));
    }

    #[test]
    fn a_message_without_blocks_is_byte_identical_to_the_old_shape() {
        // The ignore-when-absent direction: every pre-M23 request (and
        // every provider without blocks) is unchanged.
        let without = vec![
            ChatMessage::user("hi"),
            ChatMessage::assistant("answer with a call"),
        ];
        let with = vec![
            ChatMessage::user("hi"),
            ChatMessage::Assistant {
                content: "answer with a call".into(),
                thinking: None,
                tool_calls: None,
            },
        ];
        let a: Value = serde_json::from_str(
            &build_request_body("m", &without, None, None, None, false, Identity::Plain).unwrap(),
        )
        .unwrap();
        let b: Value = serde_json::from_str(
            &build_request_body("m", &with, None, None, None, false, Identity::Plain).unwrap(),
        )
        .unwrap();
        assert_eq!(a, b);
    }
}
