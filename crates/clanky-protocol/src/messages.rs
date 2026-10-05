//! Protocol v1 message and payload types.
//!
//! Serialized forms follow `provider-protocol.md`: every message is one JSON
//! object with a `"type"` field, camelCase field names, and unknown fields
//! ignored on deserialization.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version this crate implements (see §9 of the spec).
pub const PROTOCOL_VERSION: u32 = 1;

/// Every protocol message: one JSON object tagged with `"type"`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Message {
    /// Handshake, both directions. Client sends `protocolVersion` only; the
    /// plugin answers with its name and capability flags.
    Hello {
        protocol_version: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        capabilities: Option<Capabilities>,
    },
    /// Client → plugin: enumerate models.
    ListModels { id: u64 },
    /// Plugin → client: reply to [`Message::ListModels`].
    Models { id: u64, models: Vec<ModelInfo> },
    /// Client → plugin: one chat turn (streamed back as chunks).
    Chat {
        id: u64,
        model: String,
        messages: Vec<ChatMessage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tools: Option<Vec<Tool>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sampling: Option<Sampling>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<Thinking>,
    },
    /// Plugin → client: one stream event of an in-flight chat.
    Chunk {
        request_id: u64,
        payload: ChunkPayload,
    },
    /// Plugin → client: terminal success of a chat stream.
    Done {
        request_id: u64,
        finish_reason: FinishReason,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// Plugin → client: request-scoped error (carries `requestId`) or
    /// connection-fatal error (`requestId` absent).
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
        code: ErrorCode,
        message: String,
        #[serde(default)]
        retryable: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
    },
    /// Client → plugin: advisory cancellation of an in-flight chat.
    Cancel { request_id: u64 },
}

impl Message {
    /// The `id`/`requestId` this message responds to, when it has one.
    pub fn request_id(&self) -> Option<u64> {
        match self {
            Message::ListModels { id }
            | Message::Models { id, .. }
            | Message::Chat { id, .. }
            | Message::Chunk { request_id: id, .. }
            | Message::Done { request_id: id, .. }
            | Message::Cancel { request_id: id } => Some(*id),
            Message::Error { request_id, .. } => *request_id,
            Message::Hello { .. } => None,
        }
    }
}

/// Plugin capability flags. Absent flags are `false`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Capabilities {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub list_models: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub thinking: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub tools: bool,
}

/// A model offered by the provider. Only `id` is required; the rest are
/// optional hints for pickers and UI display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    /// Whether the model can generate text (chat/LLM). Providers that
    /// classify their models set this; `None` means unknown, and callers
    /// should treat unknown as usable rather than filter it out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_text_generation: Option<bool>,
    /// Catalog pricing in dollars per million tokens, when the provider
    /// exposes it (`metadata.pricing` on DeepInfra). Optional hints for a
    /// session-cost display; absent on most other providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_price_per_mtok: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_price_per_mtok: Option<f64>,
}

/// One conversation message. `content` is always a plain string in v1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "role",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(default)]
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: content.into(),
            tool_calls: None,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::Tool {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
        }
    }

    pub fn content(&self) -> &str {
        match self {
            Self::System { content }
            | Self::User { content }
            | Self::Assistant { content, .. }
            | Self::Tool { content, .. } => content,
        }
    }
}

/// A tool call as recorded in an assistant message. `arguments` is a parsed
/// object, not a JSON string (spec §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// A tool definition offered to the model; `parameters` is JSON Schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

/// Sampling parameters; every field optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Sampling {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// Thinking/reasoning controls; every field optional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Thinking {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
}

/// One stream event of a chat turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ChunkPayload {
    /// Delta for the assistant's visible text.
    Text { text: String },
    /// Delta for the assistant's thinking text.
    Thinking { text: String },
    /// Announces a tool call by `index`; arguments follow via
    /// [`ChunkPayload::ToolCallArgs`].
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    /// Appends a fragment of a tool call's JSON arguments string.
    ToolCallArgs { index: u32, args_chunk: String },
}

/// Why a chat turn finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Cancelled,
    ContentFilter,
}

/// Error codes from the provider (spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    Auth,
    RateLimit,
    InvalidRequest,
    Backend,
    Protocol,
    ProtocolVersion,
    Internal,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::RateLimit => "rateLimit",
            Self::InvalidRequest => "invalidRequest",
            Self::Backend => "backend",
            Self::Protocol => "protocol",
            Self::ProtocolVersion => "protocolVersion",
            Self::Internal => "internal",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Token usage as reported by the backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u64>,
}

/// Errors surfaced by the protocol layer (either side of the transport).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("protocol version mismatch: peer speaks {peer}, clanky speaks {PROTOCOL_VERSION}")]
    VersionMismatch { peer: u32 },
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("transport error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{code}: {message}")]
    Provider {
        code: ErrorCode,
        message: String,
        retryable: bool,
    },
}

/// Parse one wire-format message, ignoring unknown message types (spec §9:
/// v1 changes are additive; consumers must skip what they don't know).
pub fn parse_message(line: &str) -> Result<Option<Message>, Error> {
    let value: Value = serde_json::from_str(line).map_err(|e| Error::Protocol(e.to_string()))?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Protocol("message has no `type` field".into()))?
        .to_string();
    if !is_known_type(&kind) {
        return Ok(None);
    }
    serde_json::from_value(value)
        .map(Some)
        .map_err(|e| Error::Protocol(format!("malformed `{kind}` message: {e}")))
}

fn is_known_type(kind: &str) -> bool {
    matches!(
        kind,
        "hello" | "listModels" | "models" | "chat" | "chunk" | "done" | "error" | "cancel"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hello_serializes_like_the_spec() {
        let hello = Message::Hello {
            protocol_version: 1,
            name: Some("deepinfra".into()),
            capabilities: Some(Capabilities {
                list_models: true,
                thinking: true,
                tools: true,
            }),
        };
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(
            json,
            json!({
                "type": "hello",
                "protocolVersion": 1,
                "name": "deepinfra",
                "capabilities": {"listModels": true, "thinking": true, "tools": true}
            })
        );
    }

    #[test]
    fn capabilities_absent_flags_are_false() {
        let caps: Capabilities = serde_json::from_str("{}").unwrap();
        assert_eq!(caps, Capabilities::default());
        let only_list: Capabilities = serde_json::from_str(r#"{"listModels": true}"#).unwrap();
        assert!(only_list.list_models);
        assert!(!only_list.thinking);
        // `false` flags are not serialized (absent = false).
        let json = serde_json::to_value(Capabilities::default()).unwrap();
        assert_eq!(json, json!({}));
    }

    #[test]
    fn chat_request_roundtrips() {
        let chat = Message::Chat {
            id: 42,
            model: "deepseek-ai/DeepSeek-V3".into(),
            messages: vec![
                ChatMessage::system("You are a terminal coding agent."),
                ChatMessage::user("count files in /tmp"),
                ChatMessage::Assistant {
                    content: String::new(),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "ls /tmp | wc -l"}),
                    }]),
                },
                ChatMessage::tool("call_1", "42"),
            ],
            tools: Some(vec![Tool {
                name: "bash".into(),
                description: Some("Run a shell command".into()),
                parameters: Some(json!({"type": "object"})),
            }]),
            sampling: Some(Sampling {
                temperature: Some(0.7),
                top_p: Some(1.0),
                max_tokens: Some(4096),
            }),
            thinking: Some(Thinking {
                budget_tokens: Some(2048),
            }),
        };
        let line = serde_json::to_string(&chat).unwrap();
        let parsed: Message = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, chat);
        assert!(line.contains("\"toolCalls\""));
        assert!(line.contains("\"toolCallId\""));
        assert!(line.contains("\"topP\""));
        assert!(line.contains("\"budgetTokens\""));
    }

    #[test]
    fn spec_examples_deserialize() {
        // Straight out of provider-protocol.md.
        for (line, expected_type) in [
            (r#"{"type": "hello", "protocolVersion": 1}"#, "hello"),
            (
                r#"{"type": "hello", "protocolVersion": 1, "name": "deepinfra",
                    "capabilities": {"listModels": true, "thinking": true, "tools": true}}"#,
                "hello",
            ),
            (r#"{"type": "listModels", "id": 7}"#, "listModels"),
            (
                r#"{"type": "models", "id": 7,
                    "models": [{"id": "deepseek-ai/DeepSeek-V3",
                                "displayName": "DeepSeek V3",
                                "contextWindow": 128000,
                                "supportsThinking": true}]}"#,
                "models",
            ),
            (
                r#"{"type": "chat", "id": 42, "model": "deepseek-ai/DeepSeek-V3",
                    "messages": [{"role": "system", "content": "You are a terminal coding agent."}]}"#,
                "chat",
            ),
            (
                r#"{"type": "chunk", "requestId": 42, "payload": {"kind": "text", "text": "There "}}"#,
                "chunk",
            ),
            (
                r#"{"type": "chunk", "requestId": 42, "payload": {"kind": "toolCallStart", "index": 0, "id": "call_1", "name": "bash"}}"#,
                "chunk",
            ),
            (
                r#"{"type": "chunk", "requestId": 42, "payload": {"kind": "toolCallArgs", "index": 0, "argsChunk": "{\"command\":"}}"#,
                "chunk",
            ),
            (
                r#"{"type": "done", "requestId": 42, "finishReason": "toolCalls",
                    "usage": {"promptTokens": 1523, "completionTokens": 87}}"#,
                "done",
            ),
            (
                r#"{"type": "error", "requestId": 42, "code": "auth", "message": "401: invalid API key", "retryable": false}"#,
                "error",
            ),
            (r#"{"type": "cancel", "requestId": 42}"#, "cancel"),
        ] {
            let parsed = parse_message(line).unwrap().expect(line);
            let json = serde_json::to_value(&parsed).unwrap();
            assert_eq!(json.get("type").unwrap().as_str().unwrap(), expected_type);
        }
    }

    #[test]
    fn unknown_types_and_fields_are_ignored() {
        let parsed = parse_message(r#"{"type": "futureThing", "data": 1}"#).unwrap();
        assert_eq!(parsed, None);

        // Known type with an unknown extra field: accepted.
        let parsed = parse_message(r#"{"type": "listModels", "id": 7, "extra": true}"#)
            .unwrap()
            .expect("known type with extra field");
        assert_eq!(parsed.request_id(), Some(7));
    }

    #[test]
    fn missing_type_is_a_protocol_error() {
        let err = parse_message(r#"{"id": 7}"#).unwrap_err();
        assert!(err.to_string().contains("no `type` field"));
    }

    #[test]
    fn assistant_content_defaults_to_empty() {
        let msg: ChatMessage =
            serde_json::from_str(r#"{"role": "assistant", "toolCalls": []}"#).unwrap();
        assert_eq!(msg.content(), "");
    }
}
