//! Request/response adapter: clanky protocol ⇄ Anthropic Messages.
//!
//! Shape of the mapping (planning, "Adapter mapping notes"):
//! - `system` is a top-level parameter, not a message; the identity block is
//!   prepended on the OAuth surface (spike-confirmed: required to identify
//!   as Claude Code, and the model answers *as* Claude Code with it).
//! - assistant `tool_calls` ⇄ `tool_use` content blocks; `role: "tool"`
//!   results are sent as a `user` message of `tool_result` blocks.
//! - `thinking` budget → `thinking: {type: "enabled", budget_tokens}`;
//!   `temperature` is dropped whenever thinking is on (spike-confirmed 400).
//! - `max_tokens` defaults to a model-agnostic cap (the backend clamps to
//!   the model's own limit and says so in `usage.output_tokens.64k…` — the
//!   spike-confirmed 400 message names the model's maximum, so clients can
//!   react; we just default smaller).

use clanky_protocol::{ChatMessage, FinishReason, Sampling, Thinking, Tool, Usage};
use serde_json::{Value, json};

/// System block every OAuth request must start with (spike-confirmed:
/// identifies the caller to the OAuth surface; the model then answers as
/// Claude Code — visible in the captured fixtures).
pub const IDENTITY_SYSTEM_TEXT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// `max_tokens` used when the request carries none. Generous but well under
/// the smallest model cap in the live catalog (64 000).
pub const DEFAULT_MAX_TOKENS: u32 = 8_192;

/// Serialize protocol chat messages into an Anthropic `/v1/messages` body.
///
/// `system` messages (clanky allows several) are concatenated — in wire
/// order — into the system parameter after the identity block.
pub fn build_request_body(
    model: &str,
    messages: &[ChatMessage],
    tools: Option<&[Tool]>,
    sampling: Option<Sampling>,
    thinking: Option<Thinking>,
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

    let mut body = json!({
        "model": model,
        "max_tokens": sampling.and_then(|s| s.max_tokens).unwrap_or(DEFAULT_MAX_TOKENS),
        "messages": conversation,
        "system": system_blocks,
    });

    if let Some(tools) = tools.filter(|t| !t.is_empty()) {
        body["tools"] = Value::Array(tools.iter().map(anthropic_tool).collect());
    }
    if let Some(t) = thinking.filter(|t| t.budget_tokens.is_some_and(|n| n > 0)) {
        body["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": t.budget_tokens.expect("checked"),
        });
        // Spike-confirmed: temperature may only be 1 when thinking is on.
        // Drop the parameter entirely instead of clamping.
    } else if let Some(temperature) = sampling.and_then(|s| s.temperature) {
        body["temperature"] = json!(temperature);
    }
    if let Some(top_p) = sampling.and_then(|s| s.top_p) {
        body["top_p"] = json!(top_p);
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
        usage: response.usage.as_ref().map(|u| Usage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            // Anthropic's cache fields do not map onto the protocol's
            // OpenAI-style `cached_tokens` (that is a *read* count); carry
            // the read count so the cost display can use it.
            cached_tokens: u.cache_read_input_tokens,
        }),
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
            &build_request_body("claude-haiku-4-5", &messages, None, None, None).unwrap(),
        )
        .unwrap();
        let fixture: Value = serde_json::from_str(REQ_BASIC).unwrap();
        assert_eq!(body["model"], fixture["model"]);
        assert_eq!(body["system"], fixture["system"]);
        assert_eq!(body["messages"], fixture["messages"]);
        assert_eq!(body["max_tokens"], 8192, "default max_tokens");
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
        let body: Value =
            serde_json::from_str(&build_request_body("m", &messages, None, None, None).unwrap())
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
                "claude-haiku-4-5",
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
            &build_request_body("m", &messages, Some(&[tool]), None, None).unwrap(),
        )
        .unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "two tool results share one user message");
        assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn thinking_sets_budget_and_drops_temperature() {
        let messages = vec![ChatMessage::user("hi")];
        let sampling = Sampling {
            temperature: Some(0.2),
            top_p: Some(0.9),
            max_tokens: None,
        };
        let body: Value = serde_json::from_str(
            &build_request_body(
                "m",
                &messages,
                None,
                Some(sampling),
                Some(Thinking {
                    budget_tokens: Some(1024),
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024})
        );
        assert!(
            body.get("temperature").is_none(),
            "spike-confirmed 400 with temperature while thinking"
        );
        assert_eq!(body["top_p"], json!(0.9));
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
}
