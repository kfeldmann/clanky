//! DeepInfra provider logic: protocol messages ⇄ OpenAI-compatible chat
//! completions (streaming over SSE).

use clanky_protocol::{
    Capabilities, ChatDone, ChatMessage, ChatRequest, ChunkPayload, Error, ErrorCode, FinishReason,
    Handler, ModelInfo, PluginInfo, Thinking, Tool, Usage,
};
use serde_json::{Value, json};

use crate::backend::{Backend, BackendError, truncate_body};

/// Provider name reported at handshake.
pub const PROVIDER_NAME: &str = "deepinfra";
/// Default model when neither settings nor CLI name one.
pub const DEFAULT_MODEL: &str = "deepseek-ai/DeepSeek-V4-Flash-0731";
/// DeepInfra's OpenAI-compatible base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.deepinfra.com/v1/openai";
/// Primary auth env var read by this provider (never by Clanky core).
pub const ENV_API_KEY: &str = "DEEPINFRA_API_KEY";
/// Fallback auth env var (the CI/test environment uses this name).
pub const ENV_TOKEN_FALLBACK: &str = "DEEPINFRA_TOKEN";
/// Optional base URL override.
pub const ENV_BASE_URL: &str = "DEEPINFRA_URL";

/// DeepInfra provider, generic over the HTTP [`Backend`] so tests can inject
/// canned responses. The concrete blocking-HTTP instance is [`crate::DeepInfra`].
pub struct DeepInfraProvider<B: Backend> {
    backend: B,
    base_url: String,
}

impl DeepInfraProvider<crate::backend::UreqBackend> {
    /// Build the standard provider from the environment. Fails with an
    /// `auth` error when no API key is set.
    pub fn from_env() -> Result<Self, Error> {
        let api_key = std::env::var(ENV_API_KEY)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| {
                std::env::var(ENV_TOKEN_FALLBACK)
                    .ok()
                    .filter(|k| !k.trim().is_empty())
            })
            .ok_or_else(missing_key_error)?;
        let base_url = std::env::var(ENV_BASE_URL)
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into());
        Ok(Self::new(
            crate::backend::UreqBackend::new(api_key),
            base_url,
        ))
    }
}

fn missing_key_error() -> Error {
    Error::Provider {
        code: ErrorCode::Auth,
        message: format!("no API key: set {ENV_API_KEY} (or {ENV_TOKEN_FALLBACK})"),
        retryable: false,
    }
}

impl<B: Backend> DeepInfraProvider<B> {
    pub fn new(backend: B, base_url: impl Into<String>) -> Self {
        Self {
            backend,
            base_url: base_url.into(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), path)
    }
}

impl<B: Backend> Handler for DeepInfraProvider<B> {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: PROVIDER_NAME.into(),
            capabilities: Capabilities {
                list_models: true,
                thinking: true,
                tools: true,
            },
        }
    }

    fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
        let body = self
            .backend
            .get(&self.url("models"))
            .map_err(|e| backend_to_protocol_error(e, "listing models"))?;
        parse_models(&body)
    }

    fn chat(
        &mut self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<ChatDone, Error> {
        let body = self.build_request_body(request)?;
        let payloads = self
            .backend
            .post_stream(&self.url("chat/completions"), &body)
            .map_err(|e| backend_to_protocol_error(e, "chat completion"))?;
        let mut state = StreamState::default();
        for payload in payloads {
            let payload = payload.map_err(|e| backend_to_protocol_error(e, "chat stream"))?;
            let trimmed = payload.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "[DONE]" {
                break;
            }
            let delta: OpenAiStreamChunk = serde_json::from_str(trimmed)
                .map_err(|e| Error::Protocol(format!("malformed stream chunk: {e}")))?;
            state.ingest(delta, sink);
        }
        Ok(state.finish())
    }
}

impl<B: Backend> DeepInfraProvider<B> {
    /// Build the OpenAI-compatible request body for a chat request.
    /// Streaming-first: SSE deltas come back as OpenAI-style chunks.
    fn build_request_body(&self, request: &ChatRequest) -> Result<String, Error> {
        let mut body = json!({
            "model": request.model,
            "messages": openai_messages(&request.messages)?,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let obj = body.as_object_mut().unwrap();
        if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
            obj.insert("tools".into(), openai_tools(tools));
        }
        if let Some(sampling) = request.sampling {
            if let Some(t) = sampling.temperature {
                obj.insert("temperature".into(), json!(t));
            }
            if let Some(p) = sampling.top_p {
                obj.insert("top_p".into(), json!(p));
            }
            if let Some(m) = sampling.max_tokens {
                obj.insert("max_tokens".into(), json!(m));
            }
        }
        if let Some(effort) = request.thinking.and_then(reasoning_effort) {
            obj.insert("reasoning_effort".into(), json!(effort));
        }
        serde_json::to_string(&body)
            .map_err(|e| Error::Protocol(format!("cannot serialize request: {e}")))
    }
}

/// Map a protocol thinking budget onto DeepInfra's `reasoning_effort`
/// (low/medium/high). `None` means "let the backend decide" (or the model
/// does not reason).
fn reasoning_effort(thinking: Thinking) -> Option<&'static str> {
    match thinking.budget_tokens {
        None => None,
        Some(0) => None,
        Some(n) if n <= 2048 => Some("low"),
        Some(n) if n <= 8192 => Some("medium"),
        Some(_) => Some("high"),
    }
}

/// Serialize protocol chat messages into OpenAI-compatible JSON messages.
fn openai_messages(messages: &[ChatMessage]) -> Result<Vec<Value>, Error> {
    messages
        .iter()
        .map(|m| match m {
            ChatMessage::System { content } | ChatMessage::User { content } => {
                Ok(json!({ "role": role_name(m), "content": content }))
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
            } => Ok(json!({ "role": "tool", "tool_call_id": tool_call_id, "content": content })),
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut msg = json!({ "role": "assistant" });
                let obj = msg.as_object_mut().unwrap();
                if !content.is_empty() {
                    obj.insert("content".into(), json!(content));
                }
                if let Some(calls) = tool_calls {
                    obj.insert(
                        "tool_calls".into(),
                        Value::Array(
                            calls
                                .iter()
                                .map(|c| {
                                    json!({
                                        "id": c.id,
                                        "type": "function",
                                        "function": {
                                            "name": c.name,
                                            // OpenAI carries arguments as a JSON *string*.
                                            "arguments": c.arguments.to_string(),
                                        }
                                    })
                                })
                                .collect(),
                        ),
                    );
                }
                Ok(msg)
            }
        })
        .collect()
}

fn role_name(message: &ChatMessage) -> &'static str {
    match message {
        ChatMessage::System { .. } => "system",
        ChatMessage::User { .. } => "user",
        ChatMessage::Assistant { .. } => "assistant",
        ChatMessage::Tool { .. } => "tool",
    }
}

fn openai_tools(tools: &[Tool]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect(),
    )
}

/// Map an HTTP-layer failure to a request-scoped protocol error.
fn backend_to_protocol_error(err: BackendError, context: &str) -> Error {
    let BackendError { status, message } = err;
    let code = match status {
        Some(401 | 403) => ErrorCode::Auth,
        Some(429) => ErrorCode::RateLimit,
        Some(400 | 404 | 422) => ErrorCode::InvalidRequest,
        _ => ErrorCode::Backend,
    };
    let status_note = status.map_or_else(String::new, |s| format!("{s}: "));
    let detail = if message.trim().is_empty() {
        format!("request failed ({context})")
    } else {
        truncate_body(message.trim(), 400)
    };
    Error::Provider {
        code,
        message: format!("{status_note}{detail}"),
        retryable: matches!(code, ErrorCode::RateLimit | ErrorCode::Backend),
    }
}

/// Parse the `GET /models` response. Only `id` is required; hints are taken
/// opportunistically.
fn parse_models(body: &str) -> Result<Vec<ModelInfo>, Error> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| Error::Protocol(format!("malformed models response: {e}")))?;
    let data = root
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("models response has no `data` array".into()))?;
    let models = data
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            let display_name = id.split('/').next_back().map(str::to_string);
            let context_window = item
                .pointer("/metadata/context_length")
                .and_then(Value::as_u64);
            // DeepInfra tags models in `metadata.tags`; the `chat` tag
            // marks the ones that can generate text. Absent tags stay
            // `None` (unknown, not "no").
            let supports_text_generation = item
                .pointer("/metadata/tags")
                .and_then(Value::as_array)
                .map(|tags| tags.iter().filter_map(Value::as_str).any(|t| t == "chat"));
            let pricing = item.pointer("/metadata/pricing");
            let price = |key: &str| {
                pricing
                    .and_then(|p| p.get(key))
                    .and_then(Value::as_f64)
                    .filter(|v| v.is_finite() && *v >= 0.0)
            };
            Some(ModelInfo {
                id,
                display_name,
                context_window,
                supports_thinking: None,
                supports_text_generation,
                input_price_per_mtok: price("input_tokens"),
                output_price_per_mtok: price("output_tokens"),
            })
        })
        .collect();
    Ok(models)
}

// --- OpenAI streaming chunk shapes (parsed leniently; DeepInfra is
// --- OpenAI-compatible) ---

#[derive(Debug, Default, serde::Deserialize)]
struct OpenAiStreamChunk {
    #[serde(default)]
    choices: Vec<OpenAiStreamChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct OpenAiStreamChoice {
    #[serde(default)]
    delta: OpenAiDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct OpenAiDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAiStreamToolCall>>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiStreamToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    function: OpenAiFunction,
}

#[derive(Debug, Default, serde::Deserialize)]
struct OpenAiFunction {
    #[serde(default)]
    name: Option<String>,
    /// OpenAI carries arguments as a JSON string; some backends send an
    /// object. Both are accepted.
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Debug, serde::Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
}

/// Accumulates one streamed completion and translates deltas into protocol
/// chunks. A tool call's `id`/`name` arrive with the first delta for its
/// index (OpenAI convention) and announce a `toolCallStart`; later deltas
/// append argument fragments.
#[derive(Debug, Default)]
struct StreamState {
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
    fn ingest(&mut self, chunk: OpenAiStreamChunk, sink: &mut dyn FnMut(ChunkPayload)) {
        if let Some(usage) = chunk.usage {
            self.usage = Some(Usage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
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
        while self.calls.len() <= call.index as usize {
            self.calls.push(CallAccumulator::default());
        }
        let acc = &mut self.calls[call.index as usize];
        if !acc.started {
            acc.id = call
                .id
                .clone()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("call_{}", call.index));
            acc.name = call.function.name.clone().unwrap_or_default();
            acc.started = true;
            sink(ChunkPayload::ToolCallStart {
                index: call.index,
                id: acc.id.clone(),
                name: acc.name.clone(),
            });
        }
        if let Some(fragment) = call
            .function
            .arguments
            .and_then(args_string)
            .filter(|s| !s.is_empty())
        {
            acc.args.push_str(&fragment);
            sink(ChunkPayload::ToolCallArgs {
                index: call.index,
                args_chunk: fragment,
            });
        }
    }

    fn finish(self) -> ChatDone {
        ChatDone {
            finish_reason: self.finish_reason.unwrap_or(FinishReason::Stop),
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

fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("tool_calls") => FinishReason::ToolCalls,
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
    use crate::backend::Backend;
    use clanky_protocol::Sampling;
    use std::cell::RefCell;

    /// Records URLs/bodies and streams canned SSE payloads.
    struct MockBackend {
        posts: RefCell<Vec<(String, String)>>,
        gets: RefCell<Vec<String>>,
        sse_lines: Vec<String>,
        get_response: String,
        fail: Option<BackendError>,
    }

    impl MockBackend {
        fn new(sse_lines: Vec<String>, get_response: &str) -> Self {
            Self {
                posts: RefCell::new(Vec::new()),
                gets: RefCell::new(Vec::new()),
                sse_lines,
                get_response: get_response.into(),
                fail: None,
            }
        }
    }

    impl Backend for MockBackend {
        fn post_stream(
            &self,
            url: &str,
            body: &str,
        ) -> Result<Box<dyn Iterator<Item = Result<String, BackendError>>>, BackendError> {
            self.posts.borrow_mut().push((url.into(), body.into()));
            if let Some(err) = &self.fail {
                return Err(err.clone());
            }
            Ok(Box::new(self.sse_lines.clone().into_iter().map(|line| {
                Ok(line.trim_start_matches("data: ").to_string())
            })))
        }

        fn get(&self, url: &str) -> Result<String, BackendError> {
            self.gets.borrow_mut().push(url.into());
            Ok(self.get_response.clone())
        }
    }

    fn simple_request() -> ChatRequest {
        ChatRequest {
            model: "deepseek-ai/DeepSeek-V4".into(),
            messages: vec![
                ChatMessage::system("You are terse."),
                ChatMessage::user("say hi"),
            ],
            tools: None,
            sampling: Some(Sampling {
                temperature: Some(0.5),
                top_p: Some(0.9),
                max_tokens: Some(256),
            }),
            thinking: None,
        }
    }

    /// Run a canned SSE conversation and collect the chunks.
    fn run_sse<L: AsRef<str>>(
        lines: &[L],
        request: &ChatRequest,
    ) -> (Vec<ChunkPayload>, Result<ChatDone, Error>) {
        let sse_lines: Vec<String> = lines.iter().map(|l| l.as_ref().to_string()).collect();
        let mut provider =
            DeepInfraProvider::new(MockBackend::new(sse_lines, "[]"), "https://x.invalid");
        let mut chunks = Vec::new();
        let done = provider.chat(request, &mut |chunk| chunks.push(chunk));
        (chunks, done)
    }

    fn text_chunk(text: &str) -> String {
        serde_json::json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]})
            .to_string()
    }

    fn done_chunk(reason: &str) -> String {
        serde_json::json!({"choices": [{"delta": {}, "finish_reason": reason}]}).to_string()
    }

    #[test]
    fn chat_streams_text_and_maps_usage() {
        let lines = [
            &text_chunk("Hi!"),
            &serde_json::json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 11, "completion_tokens": 2, "total_tokens": 13}
            })
            .to_string(),
            "data: [DONE]",
        ];
        let (chunks, done) = run_sse(&lines, &simple_request());
        assert_eq!(chunks, vec![ChunkPayload::Text { text: "Hi!".into() }]);
        let done = done.unwrap();
        assert_eq!(done.finish_reason, FinishReason::Stop);
        assert_eq!(
            done.usage,
            Some(Usage {
                prompt_tokens: Some(11),
                completion_tokens: Some(2)
            })
        );
    }

    #[test]
    fn request_body_is_openai_shaped_and_streaming() {
        let mut provider = DeepInfraProvider::new(
            MockBackend::new(vec![done_chunk("stop")], "[]"),
            "https://example.invalid/v1/openai/",
        );
        provider.chat(&simple_request(), &mut |_| {}).unwrap();
        let (url, body) = provider.backend.posts.borrow()[0].clone();

        // Trailing slash in the base URL must not double up.
        assert_eq!(url, "https://example.invalid/v1/openai/chat/completions");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["model"], "deepseek-ai/DeepSeek-V4");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "say hi");
    }

    #[test]
    fn reasoning_content_becomes_thinking_chunk() {
        let lines = [
            &serde_json::json!({
                "choices": [{"delta": {"reasoning_content": "Need to greet."}, "finish_reason": null}]
            })
            .to_string(),
            &text_chunk("Hello."),
            &done_chunk("stop"),
        ];
        let (chunks, done) = run_sse(&lines, &simple_request());
        assert_eq!(
            chunks,
            vec![
                ChunkPayload::Thinking {
                    text: "Need to greet.".into()
                },
                ChunkPayload::Text {
                    text: "Hello.".into()
                },
            ]
        );
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);
    }

    #[test]
    fn tool_calls_stream_as_start_and_args_chunks() {
        let lines = [
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "bash", "arguments": ""}
                }]}, "finish_reason": null}]
            })
            .to_string(),
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "function": {"arguments": "{\"command\": \"ls\"}"}
                }]}, "finish_reason": null}]
            })
            .to_string(),
            &done_chunk("tool_calls"),
        ];
        let (chunks, done) = run_sse(&lines, &simple_request());
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
                    args_chunk: "{\"command\": \"ls\"}".into()
                },
            ]
        );
        assert_eq!(done.unwrap().finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn multiple_tool_calls_keep_their_indices() {
        let lines = [
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "call_a", "type": "function", "function": {"name": "a", "arguments": ""}},
                    {"index": 1, "id": "call_b", "type": "function", "function": {"name": "b", "arguments": "{\"x\":1}"}}
                ]}, "finish_reason": null}]
            })
            .to_string(),
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": "{\"y\":2}"}}
                ]}, "finish_reason": "tool_calls"}]
            })
            .to_string(),
        ];
        let (chunks, _) = run_sse(&lines, &simple_request());
        assert_eq!(
            chunks,
            vec![
                ChunkPayload::ToolCallStart {
                    index: 0,
                    id: "call_a".into(),
                    name: "a".into()
                },
                ChunkPayload::ToolCallStart {
                    index: 1,
                    id: "call_b".into(),
                    name: "b".into()
                },
                ChunkPayload::ToolCallArgs {
                    index: 1,
                    args_chunk: "{\"x\":1}".into()
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: "{\"y\":2}".into()
                },
            ]
        );
    }

    #[test]
    fn thinking_budget_maps_to_reasoning_effort() {
        let mut provider = DeepInfraProvider::new(
            MockBackend::new(vec![done_chunk("stop")], "[]"),
            "https://x.invalid",
        );
        let mut request = simple_request();
        request.thinking = Some(Thinking {
            budget_tokens: Some(1024),
        });
        provider.chat(&request, &mut |_| {}).unwrap();
        let (_, body) = provider.backend.posts.borrow()[0].clone();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["reasoning_effort"], "low");

        request.thinking = Some(Thinking {
            budget_tokens: Some(8192),
        });
        provider.chat(&request, &mut |_| {}).unwrap();
        let (_, body) = provider.backend.posts.borrow()[1].clone();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["reasoning_effort"], "medium");

        // `off` (zero budget) is not sent.
        request.thinking = Some(Thinking {
            budget_tokens: Some(0),
        });
        provider.chat(&request, &mut |_| {}).unwrap();
        let (_, body) = provider.backend.posts.borrow()[2].clone();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert!(parsed.get("reasoning_effort").is_none());
    }

    #[test]
    fn tools_map_to_openai_function_format() {
        let mut provider = DeepInfraProvider::new(
            MockBackend::new(vec![done_chunk("stop")], "[]"),
            "https://x.invalid",
        );
        let mut request = simple_request();
        request.tools = Some(vec![Tool {
            name: "bash".into(),
            description: Some("Run a shell command".into()),
            parameters: Some(serde_json::json!({"type": "object"})),
        }]);
        provider.chat(&request, &mut |_| {}).unwrap();
        let (_, body) = provider.backend.posts.borrow()[0].clone();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "bash");
    }

    #[test]
    fn object_arguments_are_stringified_into_args_chunks() {
        let lines = [&serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": "bash", "arguments": {"command": "ls"}}
            }]}, "finish_reason": null}]
        })
        .to_string()];
        let (chunks, _) = run_sse(&lines, &simple_request());
        assert!(matches!(
            chunks.as_slice(),
            [
                ChunkPayload::ToolCallStart { .. },
                ChunkPayload::ToolCallArgs { args_chunk, .. }
            ] if serde_json::from_str::<Value>(args_chunk).unwrap() == serde_json::json!({"command": "ls"})
        ));
    }

    #[test]
    fn malformed_stream_chunk_is_a_protocol_error() {
        let lines = ["data: {not json"];
        let (_, done) = run_sse(&lines, &simple_request());
        let err = done.unwrap_err();
        assert!(err.to_string().contains("malformed stream chunk"), "{err}");
    }

    #[test]
    fn http_status_maps_to_error_codes() {
        let cases: [(Option<u16>, ErrorCode); 5] = [
            (Some(401), ErrorCode::Auth),
            (Some(403), ErrorCode::Auth),
            (Some(429), ErrorCode::RateLimit),
            (Some(400), ErrorCode::InvalidRequest),
            (Some(500), ErrorCode::Backend),
        ];
        for (status, expected) in cases {
            let mut provider =
                DeepInfraProvider::new(MockBackend::new(Vec::new(), "[]"), "https://x.invalid");
            provider.backend.fail = Some(BackendError {
                status,
                message: "boom".into(),
            });
            let err = provider.chat(&simple_request(), &mut |_| {}).unwrap_err();
            match err {
                Error::Provider {
                    code,
                    message,
                    retryable,
                } => {
                    assert_eq!(code, expected);
                    assert!(message.contains("boom"));
                    assert_eq!(
                        retryable,
                        matches!(expected, ErrorCode::RateLimit | ErrorCode::Backend)
                    );
                }
                other => panic!("unexpected: {other}"),
            }
        }
    }

    #[test]
    fn list_models_parses_catalog() {
        let catalog = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "deepseek-ai/DeepSeek-V4", "metadata": {"context_length": 163840,
                  "tags": ["chat", "reasoning"],
                  "pricing": {"input_tokens": 0.09, "output_tokens": 0.18}}},
                {"id": "black-forest-labs/FLUX-1-schnell", "metadata":
                  {"tags": ["image-gen"]}},
                {"id": "no-pricing-model"},
                {"id": "no-metadata-model"},
                {"broken": true}
            ]
        })
        .to_string();
        let mut provider =
            DeepInfraProvider::new(MockBackend::new(Vec::new(), &catalog), "https://x.invalid");
        let models = provider.list_models().unwrap();
        assert_eq!(models.len(), 4);
        assert_eq!(models[0].id, "deepseek-ai/DeepSeek-V4");
        assert_eq!(models[0].context_window, Some(163_840));
        assert_eq!(models[0].display_name.as_deref(), Some("DeepSeek-V4"));
        assert_eq!(models[0].input_price_per_mtok, Some(0.09));
        assert_eq!(models[0].output_price_per_mtok, Some(0.18));
        assert_eq!(models[0].supports_text_generation, Some(true));
        assert_eq!(models[1].id, "black-forest-labs/FLUX-1-schnell");
        assert_eq!(models[1].supports_text_generation, Some(false));
        assert_eq!(models[2].id, "no-pricing-model");
        assert_eq!(models[2].input_price_per_mtok, None);
        assert_eq!(models[3].id, "no-metadata-model");
        assert_eq!(models[3].context_window, None);
        assert_eq!(models[3].supports_text_generation, None);

        // Endpoint: {base}/models
        assert_eq!(
            provider.backend.gets.borrow()[0],
            "https://x.invalid/models"
        );
    }

    #[test]
    fn empty_stream_yields_no_chunks_but_a_done() {
        let lines = [&done_chunk("stop")];
        let (chunks, done) = run_sse(&lines, &simple_request());
        assert!(chunks.is_empty());
        let done = done.unwrap();
        assert_eq!(done.finish_reason, FinishReason::Stop);
        assert_eq!(done.usage, None);
    }
}
