//! DeepInfra provider logic: protocol messages ⇄ OpenAI-compatible chat
//! completions (streaming over SSE).

use clanky_protocol::{
    CancelFlag, Capabilities, ChatDone, ChatMessage, ChatRequest, ChunkPayload, Error, ErrorCode,
    FinishReason, Handler, ModelInfo, PluginInfo, Thinking, Tool, Usage,
};
use serde_json::{Value, json};

use crate::backend::{Backend, BackendError, truncate_body};

/// Provider name reported at handshake.
pub const PROVIDER_NAME: &str = "deepinfra";
/// Default model when neither settings nor CLI name one.
pub const DEFAULT_MODEL: &str = "zai-org/GLM-5.3-Flash";
/// DeepInfra API base URL. Paths are relative to this, so a chat request
/// goes to `https://api.deepinfra.com/v1/chat/completions` — the endpoint
/// documented by DeepInfra's OpenAPI spec. (`https://api.deepinfra.com/v1/openai`
/// serves the same API under an OpenAI-style alias and also works; override
/// with `DEEPINFRA_URL` if you need to pin it.)
pub const DEFAULT_BASE_URL: &str = "https://api.deepinfra.com/v1";
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
    /// Set by the plugin runtime (spec §7); checked between SSE events so a
    /// cancel stops generation at the next event boundary. A cancel during a
    /// single blocked read is covered by the client's kill fallback.
    cancel: CancelFlag,
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
        retry_after_ms: None,
    }
}

impl<B: Backend> DeepInfraProvider<B> {
    pub fn new(backend: B, base_url: impl Into<String>) -> Self {
        Self {
            backend,
            base_url: base_url.into(),
            cancel: CancelFlag::default(),
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
            // Advertised at handshake so core needs no per-provider table.
            default_model: Some(DEFAULT_MODEL.into()),
        }
    }

    fn set_cancel_flag(&mut self, flag: CancelFlag) {
        self.cancel = flag;
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
            // Spec §7: stop promptly once the client cancels. The check runs
            // between SSE events; a cancel during a blocked read is handled
            // by the client's kill fallback.
            if self.cancel.is_cancelled() {
                return Ok(state.finish_cancelled());
            }
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
    ///
    /// `sampling` entries pass through to the backend verbatim (spec §5):
    /// arbitrary backend parameters work and the backend validates them.
    /// The request's own structural fields belong to the protocol and are
    /// set here afterwards, so they cannot be overridden through `sampling`.
    fn build_request_body(&self, request: &ChatRequest) -> Result<String, Error> {
        let mut body = json!({});
        let obj = body.as_object_mut().expect("object literal");
        if let Some(sampling) = &request.sampling {
            for (key, value) in sampling.iter() {
                obj.insert(key.clone(), value.clone());
            }
        }
        obj.insert("model".into(), json!(request.model));
        obj.insert(
            "messages".into(),
            Value::Array(openai_messages(&request.messages)?),
        );
        obj.insert("stream".into(), json!(true));
        obj.insert("stream_options".into(), json!({"include_usage": true}));
        if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
            obj.insert("tools".into(), openai_tools(tools));
        }
        // The thinking → `reasoning_effort` mapping only fills the gap: a
        // `reasoning_effort` the user set in `sampling` wins over it.
        if !obj.contains_key("reasoning_effort")
            && let Some(effort) = request.thinking.and_then(reasoning_effort)
        {
            obj.insert("reasoning_effort".into(), json!(effort));
        }
        serde_json::to_string(&body)
            .map_err(|e| Error::Protocol(format!("cannot serialize request: {e}")))
    }
}

/// Map a protocol thinking budget onto DeepInfra's `reasoning_effort`
/// ladder (none/minimal/low/medium/high/xhigh/max; see the per-model docs in
/// DeepInfra's model browser — the chat-completions API reference still
/// documents only a subset). `None` means "let the backend decide" (or the
/// model does not reason); an explicit budget of 0 maps to `"none"`, which
/// DeepInfra documents as disabling reasoning when the model supports it —
/// unlike omitting the field, which falls back to the backend default.
fn reasoning_effort(thinking: Thinking) -> Option<&'static str> {
    match thinking.budget_tokens {
        None => None,
        Some(0) => Some("none"),
        Some(n) if n < 500 => Some("minimal"),
        Some(n) if n < 1682 => Some("low"),
        Some(n) if n < 5661 => Some("medium"),
        Some(n) if n < 19051 => Some("high"),
        Some(n) if n < 64107 => Some("xhigh"),
        Some(_) => Some("max"),
    }
}

/// Serialize protocol chat messages into OpenAI-compatible JSON messages.
///
/// A tool result's inline images cannot ride in the `tool` message:
/// DeepInfra's tool-message schema accepts only a plain string (or a
/// text-only array) for `content` — an `image_url` part there is a 422
/// ("messages.N.tool.content.str: Input should be a valid string",
/// observed live against a vision model). The images instead ride in a
/// *trailing `user` message* with `image_url` data-URI parts, emitted
/// only when the tool result carries images (so every image-free request
/// is byte-identical to the old shape) and deterministically ordered —
/// directly after the `tool` message it belongs to — so replay
/// reconstructs exactly what the model saw.
fn openai_messages(messages: &[ChatMessage]) -> Result<Vec<Value>, Error> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            ChatMessage::System { content } | ChatMessage::User { content } => {
                out.push(json!({ "role": role_name(m), "content": content }));
            }
            ChatMessage::Tool {
                tool_call_id,
                content,
                images,
            } => {
                // `role: "tool"` takes only a plain string `content`
                // (see the function doc); the text is the result, the
                // images (if any) follow in a synthetic user turn.
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content,
                }));
                if let Some(images) = images.as_deref().filter(|imgs| !imgs.is_empty()) {
                    out.push(image_parts_user_message(images));
                }
            }
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
                out.push(msg);
            }
        }
    }
    Ok(out)
}

/// The `user` message that carries a tool result's images: an array of
/// bare `image_url` data-URI parts (no text — the result's text stays in
/// the `tool` message; duplicating it would send it twice). `role:
/// "user"` is the only role whose schema accepts `image_url` parts.
fn image_parts_user_message(images: &[clanky_protocol::ImagePart]) -> Value {
    json!({
        "role": "user",
        "content": images
            .iter()
            .map(|image| json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", image.media_type.as_str(), image.data_base64),
                },
            }))
            .collect::<Vec<_>>(),
    })
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

/// Map an HTTP-layer failure to a request-scoped protocol error. A
/// server-provided `Retry-After` hint is carried through as
/// `retry_after_ms` (spec §8 allows it on `rateLimit`).
fn backend_to_protocol_error(err: BackendError, context: &str) -> Error {
    let BackendError {
        status,
        message,
        retry_after_ms,
    } = err;
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
        // Spec §8: `retryAfterMs` rides on `rateLimit` errors only.
        retry_after_ms: if code == ErrorCode::RateLimit {
            retry_after_ms
        } else {
            None
        },
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
                supports_tools: None,
                supports_text_generation,
                input_price_per_mtok: price("input_tokens"),
                output_price_per_mtok: price("output_tokens"),
                cache_read_price_per_mtok: price("cache_read_tokens"),
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

#[derive(Debug, Default, serde::Deserialize)]
struct OpenAiStreamToolCall {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    /// Lenient: some OpenAI-compatible backends emit index-only placeholder
    /// deltas with no `function` object at all; treat that as an empty one.
    #[serde(default)]
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

#[derive(Debug, Default, serde::Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    prompt_tokens_details: Option<OpenAiPromptTokensDetails>,
}

/// OpenAI-style cached-token breakdown inside `usage.prompt_tokens_details`.
#[derive(Debug, Default, serde::Deserialize)]
struct OpenAiPromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
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

    fn finish(self) -> ChatDone {
        ChatDone {
            finish_reason: self.finish_reason.unwrap_or(FinishReason::Stop),
            usage: self.usage,
        }
    }

    /// Terminal status for a cancelled turn: whatever usage was reported
    /// before the cancel, and `cancelled` as the finish reason (spec §7).
    fn finish_cancelled(self) -> ChatDone {
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
const MAX_TOOL_CALL_INDEX: u32 = 1024;

fn map_finish_reason(reason: Option<&str>) -> FinishReason {
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
            sampling: Some(Sampling::from_iter([
                ("temperature".into(), json!(0.5)),
                ("top_p".into(), json!(0.9)),
                ("max_tokens".into(), json!(256)),
            ])),
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
    fn handshake_advertises_the_default_model() {
        let provider =
            DeepInfraProvider::new(MockBackend::new(Vec::new(), ""), "https://x.invalid");
        let info = provider.info();
        assert_eq!(info.name, PROVIDER_NAME);
        assert_eq!(info.default_model.as_deref(), Some(DEFAULT_MODEL));
        assert_eq!(DEFAULT_MODEL, "zai-org/GLM-5.3-Flash");
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
                completion_tokens: Some(2),
                cached_tokens: None
            })
        );
    }

    #[test]
    fn usage_parses_the_cached_token_breakdown() {
        let lines = [
            &text_chunk("Hi!"),
            &serde_json::json!({
                "choices": [{"delta": {}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 5000,
                    "completion_tokens": 50,
                    "prompt_tokens_details": {"cached_tokens": 4800}
                }
            })
            .to_string(),
            "data: [DONE]",
        ];
        let (_, done) = run_sse(&lines, &simple_request());
        let usage = done.unwrap().usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(5000));
        assert_eq!(usage.cached_tokens, Some(4800));
        assert_eq!(usage.completion_tokens, Some(50));
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
    fn arbitrary_sampling_parameters_pass_through_verbatim() {
        let mut provider = DeepInfraProvider::new(
            MockBackend::new(vec![done_chunk("stop")], "[]"),
            "https://x.invalid",
        );
        let mut request = simple_request();
        request.sampling = Some(Sampling::from_iter([
            ("repetition_penalty".into(), json!(1.1)),
            ("seed".into(), json!(42)),
            ("reasoning_effort".into(), json!("high")),
            // Structural fields belong to the protocol and cannot be
            // overridden through `sampling`.
            ("stream".into(), json!(false)),
        ]));
        provider.chat(&request, &mut |_| {}).unwrap();
        let body: Value = serde_json::from_str(&provider.backend.posts.borrow()[0].1).unwrap();
        assert_eq!(body["repetition_penalty"], 1.1);
        assert_eq!(body["seed"], 42);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn thinking_fills_reasoning_effort_only_when_unset() {
        let thinking = Thinking {
            budget_tokens: Some(1024),
        };
        let mut provider = DeepInfraProvider::new(
            MockBackend::new(vec![done_chunk("stop"), done_chunk("stop")], "[]"),
            "https://x.invalid",
        );

        let mut request = simple_request();
        request.thinking = Some(thinking);
        provider.chat(&request, &mut |_| {}).unwrap();
        let body: Value = serde_json::from_str(&provider.backend.posts.borrow()[0].1).unwrap();
        assert_eq!(body["reasoning_effort"], "low", "derived from the budget");

        let mut request = simple_request();
        request.thinking = Some(thinking);
        request.sampling = Some(Sampling::from_iter([(
            "reasoning_effort".into(),
            json!("none"),
        )]));
        provider.chat(&request, &mut |_| {}).unwrap();
        let body: Value = serde_json::from_str(&provider.backend.posts.borrow()[1].1).unwrap();
        assert_eq!(
            body["reasoning_effort"], "none",
            "a user-set `reasoning_effort` wins over the derived mapping"
        );
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
        assert_eq!(body["reasoning_effort"], "high");

        // An explicit zero budget maps to `none` (reasoning disabled),
        // not to an omitted field (backend default).
        request.thinking = Some(Thinking {
            budget_tokens: Some(0),
        });
        provider.chat(&request, &mut |_| {}).unwrap();
        let (_, body) = provider.backend.posts.borrow()[2].clone();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["reasoning_effort"], "none");
    }

    #[test]
    fn thinking_off_sends_reasoning_effort_none() {
        // This is the payload `clanky --thinking off` produces: an explicit
        // zero budget, mapped onto the backend's "disable reasoning" knob.
        // An omitted `thinking` field (budget `None`) sends no
        // `reasoning_effort` at all, which falls back to DeepInfra's
        // default — reasoning models *do* produce a reasoning trace then.
        let mut provider = DeepInfraProvider::new(
            MockBackend::new(vec![done_chunk("stop")], "[]"),
            "https://x.invalid",
        );
        let mut request = simple_request();
        request.thinking = Some(Thinking {
            budget_tokens: Some(0),
        });
        provider.chat(&request, &mut |_| {}).unwrap();
        let (_, body) = provider.backend.posts.borrow()[0].clone();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["reasoning_effort"], "none");

        // Unset: no reasoning_effort key at all (backend default).
        let mut request = simple_request();
        request.thinking = None;
        provider.chat(&request, &mut |_| {}).unwrap();
        let (_, body) = provider.backend.posts.borrow()[1].clone();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert!(body.get("reasoning_effort").is_none());
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
    fn index_only_tool_call_delta_does_not_abort_the_stream() {
        // Some OpenAI-compatible backends emit a bare index placeholder
        // before the first real delta for that index.
        let lines = [
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{"index": 0}]}, "finish_reason": null}]
            })
            .to_string(),
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "bash", "arguments": "{\"command\": \"ls\"}"}
                }]}, "finish_reason": null}]
            })
            .to_string(),
            &done_chunk("tool_calls"),
        ];
        let (chunks, done) = run_sse(&lines, &simple_request());
        // The placeholder defers the start; the real id/name announce it.
        assert_eq!(
            chunks,
            vec![
                ChunkPayload::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "bash".into(),
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: "{\"command\": \"ls\"}".into(),
                },
            ]
        );
        assert_eq!(done.unwrap().finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn args_before_id_are_buffered_not_dropped() {
        // Regression: a delta carrying args but not yet id/name used to hit
        // the early `return` and lose the fragment, truncating the assembled
        // arguments. The fragment must be buffered until the call starts.
        let lines = [
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "function": {"arguments": "{\"command\": "}
                }]}, "finish_reason": null}]
            })
            .to_string(),
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "bash", "arguments": "\"ls\"}"}
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
                    name: "bash".into(),
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: "\"ls\"}".into(),
                },
            ]
        );
        assert_eq!(done.unwrap().finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn legacy_function_call_finish_reason_maps_to_tool_calls() {
        // OpenAI's legacy `function_call` reason announces a tool call; the
        // same wire format already maps this way in the LiteLLM plugin.
        let lines = [
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]}, "finish_reason": null}]
            })
            .to_string(),
            &serde_json::json!({
                "choices": [{"delta": {}, "finish_reason": "function_call"}]
            })
            .to_string(),
            "[DONE]",
        ];
        let (_, done) = run_sse(&lines, &simple_request());
        assert_eq!(done.unwrap().finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn absurd_tool_call_index_is_rejected_without_allocating() {
        // Regression: a malformed `"index": 4294967295` used to grow the
        // accumulator vec by ~4B slots (~4 GB). Such a delta is a broken
        // stream; it must be dropped, not honored.
        let lines = [
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 4294967295u32, "id": "call_x", "type": "function",
                    "function": {"name": "boom", "arguments": "{}"}
                }]}, "finish_reason": null}]
            })
            .to_string(),
            &serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "bash", "arguments": "{}"}
                }]}, "finish_reason": null}]
            })
            .to_string(),
            &done_chunk("stop"),
        ];
        let (chunks, done) = run_sse(&lines, &simple_request());
        assert_eq!(
            chunks,
            vec![
                ChunkPayload::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "bash".into(),
                },
                ChunkPayload::ToolCallArgs {
                    index: 0,
                    args_chunk: "{}".into(),
                },
            ]
        );
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);
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
            provider.backend.fail = Some(BackendError::new(status, "boom"));
            let err = provider.chat(&simple_request(), &mut |_| {}).unwrap_err();
            match err {
                Error::Provider {
                    code,
                    message,
                    retryable,
                    ..
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
    fn transport_timeouts_are_retryable_backend_errors() {
        // A network timeout/reset reaches the backend as a status-less
        // error (ureq reports "timed out reading response" with no HTTP
        // status). It must map to a *retryable* `backend` error so the
        // agentic loop can retry it rather than aborting the turn.
        let mut provider =
            DeepInfraProvider::new(MockBackend::new(Vec::new(), "[]"), "https://x.invalid");
        provider.backend.fail = Some(BackendError::new(None, "timed out reading response"));
        match provider.chat(&simple_request(), &mut |_| {}).unwrap_err() {
            Error::Provider {
                code,
                message,
                retryable,
                ..
            } => {
                assert_eq!(code, ErrorCode::Backend);
                assert!(message.contains("timed out reading response"), "{message}");
                assert!(retryable, "a network timeout must be retryable");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn list_models_parses_catalog() {
        let catalog = serde_json::json!({
            "object": "list",
            "data": [
                {"id": "deepseek-ai/DeepSeek-V4", "metadata": {"context_length": 163840,
                  "tags": ["chat", "reasoning"],
                  "pricing": {"input_tokens": 0.09, "output_tokens": 0.18, "cache_read_tokens": 0.011}}},
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
        assert_eq!(models[0].cache_read_price_per_mtok, Some(0.011));
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

    #[test]
    fn tool_result_without_images_keeps_plain_string_content() {
        let mut provider = DeepInfraProvider::new(
            MockBackend::new(vec![done_chunk("stop")], "[]"),
            "https://x.invalid",
        );
        let mut request = simple_request();
        request.messages.push(ChatMessage::tool("call_1", "42"));
        provider.chat(&request, &mut |_| {}).unwrap();
        let (_, body) = provider.backend.posts.borrow()[0].clone();
        let body: Value = serde_json::from_str(&body).unwrap();
        let message = &body["messages"][2];
        assert_eq!(message["role"], "tool");
        assert_eq!(message["tool_call_id"], "call_1");
        assert_eq!(message["content"], "42", "no images: a plain string");
    }

    #[test]
    fn tool_result_images_ride_in_a_trailing_user_message() {
        // DeepInfra's tool-message schema allows only a plain-string
        // `content` (a live 422 proved `image_url` parts are rejected
        // there), so the images ride in a trailing `user` message with
        // bare `image_url` data-URI parts, right after the `tool`
        // message; the `tool` message itself keeps its string text.
        let mut provider = DeepInfraProvider::new(
            MockBackend::new(vec![done_chunk("stop")], "[]"),
            "https://x.invalid",
        );
        let mut request = simple_request();
        request.messages.push(ChatMessage::tool_with_images(
            "call_1",
            "256 kb",
            vec![clanky_protocol::ImagePart {
                data_base64: "aGVsbG8=".into(),
                media_type: clanky_protocol::MediaType::Png,
            }],
        ));
        provider.chat(&request, &mut |_| {}).unwrap();
        let (_, body) = provider.backend.posts.borrow()[0].clone();
        let body: Value = serde_json::from_str(&body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4, "tool message + trailing image user turn");
        let tool = &messages[2];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_1");
        assert_eq!(tool["content"], "256 kb", "text stays a plain string");
        let images = &messages[3];
        assert_eq!(messages[3]["role"], "user");
        let parts = images["content"].as_array().unwrap();
        assert_eq!(parts.len(), 1, "the result text is not duplicated");
        assert_eq!(parts[0]["type"], "image_url");
        assert_eq!(
            parts[0]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }
}
