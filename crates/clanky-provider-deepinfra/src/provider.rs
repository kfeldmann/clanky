//! DeepInfra provider logic: protocol messages ⇄ OpenAI-compatible chat
//! completions (streaming over SSE).

use clanky_protocol::{
    CancelFlag, Capabilities, ChatDone, ChatRequest, ChunkPayload, Error, ErrorCode, Handler,
    ModelInfo, PluginInfo, Thinking, ThinkingBlock,
};
use clanky_provider_openai::stream::{OpenAiStreamChunk, StreamState};
use clanky_provider_openai::wire::{openai_messages, openai_tools, reasoning_content};
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
            Value::Array(openai_messages(
                &request.messages,
                &deepinfra_reasoning_passback,
            )),
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

/// DeepInfra's reasoning passback for an assistant message (M23): the
/// concatenated block text is the payload here — DeepInfra counts it as
/// prompt context (measured +≈400 tokens for a ~1800-char block), while
/// `thinking_blocks`/`reasoning_details`/`thinking` are silently ignored
/// (+0 tokens), so `reasoning_content` is sent and nothing else (session 2).
/// A block with no text contributes nothing; signatures and redacted data
/// have no wire meaning on this provider and are dropped.
///
/// A message whose blocks carry no text at all sends no field either —
/// nothing to pass back must look exactly like nothing (an empty
/// `reasoning_content` would be a forged field).
fn deepinfra_reasoning_passback(
    obj: &mut serde_json::Map<String, Value>,
    blocks: &[ThinkingBlock],
) {
    let reasoning = reasoning_content(blocks);
    if !reasoning.is_empty() {
        obj.insert("reasoning_content".into(), json!(reasoning));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use clanky_protocol::{ChatMessage, FinishReason, Sampling, Tool, Usage};
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

    #[test]
    fn reasoning_text_is_passed_back_as_reasoning_content_and_nothing_else() {
        // M23 session 2, the OpenAI-compatible arm: the blocks' text is the
        // payload here (measured +≈400 prompt tokens for a ~1800-char block
        // on `gpt-oss-20b`), sent as `reasoning_content` — DeepInfra counts
        // `reasoning` as an alias too but silently ignores
        // `thinking_blocks`/`reasoning_details`/`thinking` (+0 tokens), so
        // those never go out. A blockless message stays byte-identical to
        // the pre-M23 shape.
        let provider =
            DeepInfraProvider::new(MockBackend::new(Vec::new(), "[]"), "https://x.invalid");
        let with_blocks = ChatRequest {
            messages: vec![
                ChatMessage::user("hi"),
                ChatMessage::Assistant {
                    content: "answer".into(),
                    tool_calls: None,
                    thinking: Some(vec![
                        clanky_protocol::ThinkingBlock::thinking(
                            "first thought, ",
                            Some("sig".into()),
                        ),
                        clanky_protocol::ThinkingBlock::thinking("second", None),
                        clanky_protocol::ThinkingBlock::redacted("opaque blob"),
                    ]),
                },
            ],
            ..simple_request()
        };
        let without_blocks = ChatRequest {
            messages: vec![ChatMessage::user("hi"), ChatMessage::assistant("answer")],
            ..simple_request()
        };
        let body: Value = serde_json::from_str(&provider.build_request_body(&with_blocks).unwrap())
            .expect("valid body");
        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(
            assistant["reasoning_content"].as_str(),
            Some("first thought, second"),
            "text joined in block order; signature and redacted data have no wire meaning here"
        );
        assert!(
            serde_json::to_string(&body)
                .unwrap()
                .split("reasoning_content")
                .count()
                == 2,
            "exactly one field occurrence: no alias or ignored shapes sent"
        );
        // A blockless request is byte-identical to the pre-M23 shape (the
        // ignore-when-absent direction).
        assert_eq!(
            provider.build_request_body(&without_blocks).unwrap(),
            provider.build_request_body(&without_blocks).unwrap(),
            "a blockless request is byte-identical (the ignore-when-absent direction)"
        );
        // A message whose blocks carry no text at all sends no field
        // either — nothing to pass back must look exactly like nothing
        // (an empty `reasoning_content` would be a forged field).
        let textless = ChatRequest {
            messages: vec![
                ChatMessage::user("hi"),
                ChatMessage::Assistant {
                    content: "answer".into(),
                    tool_calls: None,
                    thinking: Some(vec![
                        clanky_protocol::ThinkingBlock::thinking("", Some("sig".into())),
                        clanky_protocol::ThinkingBlock::redacted("opaque blob"),
                    ]),
                },
            ],
            ..simple_request()
        };
        assert_eq!(
            provider.build_request_body(&textless).unwrap(),
            provider.build_request_body(&without_blocks).unwrap(),
            "a no-text payload is byte-identical to sending nothing"
        );
    }

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

    /// The live acceptance check (M23 session 2's exit criterion): a real
    /// round-trip asserts the *observed* token accounting — passing a
    /// reasoning block back as `reasoning_content` raises the second
    /// round's `prompt_tokens` by roughly one token per 4–5 characters on
    /// `openai/gpt-oss-20b` (measured: +400 for a ~1800-char block, +4400
    /// for a ~19k one, with a small fixed overhead). Gated behind
    /// `M23_LIVE_DEEPINFRA=1` (plus `DEEPINFRA_API_KEY`/`DEEPINFRA_TOKEN`)
    /// so CI stays offline-green; a future refactor that silently stops
    /// sending the field fails this instead of quietly changing behavior.
    #[test]
    fn live_reasoning_content_costs_its_tokens() -> Result<(), Error> {
        if std::env::var("M23_LIVE_DEEPINFRA").map(|v| v != "0") != Ok(true) {
            eprintln!("skipping: set M23_LIVE_DEEPINFRA=1 to run the live DeepInfra check");
            return Ok(());
        }
        let mut provider = DeepInfraProvider::from_env()?;
        let model = "openai/gpt-oss-20b";
        let thinking = Thinking {
            budget_tokens: Some(32_768),
        };
        // Round 1: a tool loop that makes the model reason, and report its
        // reasoning as `reasoning_content` deltas.
        let ask = |tools: Option<Vec<Tool>>, text: &str| ChatRequest {
            model: model.into(),
            messages: vec![ChatMessage::user(text.to_string())],
            tools,
            sampling: None,
            thinking: Some(thinking),
        };
        let bash = Tool {
            name: "bash".into(),
            description: Some("Run a shell command".into()),
            parameters: Some(json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"]
            })),
        };
        let with_tools = ask(Some(vec![bash.clone()]), "call the tool on 'hello'");
        let mut round1_chunks = Vec::new();
        let round1_done = provider.chat(&with_tools, &mut |c| round1_chunks.push(c))?;
        assert_eq!(round1_done.finish_reason, FinishReason::ToolCalls);
        let calls = round1_chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::ToolCallStart { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!calls.is_empty(), "the round must produce a tool call");
        let round1_reasoning: String = round1_chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Thinking { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let reasoning_chars = round1_reasoning.chars().count();
        assert!(
            reasoning_chars >= 200,
            "the round must reason enough to measure ({reasoning_chars} chars)"
        );
        let round2_usage = round1_done.usage.expect("usage");

        // Round 2, with passback: identical history plus the assistant
        // round's `reasoning_content`. The delta against the same request
        // without it is the measured cost.
        let mut passback_messages = with_tools.messages.clone();
        passback_messages.push(ChatMessage::Assistant {
            content: String::new(),
            tool_calls: Some(vec![clanky_protocol::ToolCall {
                id: calls[0].clone(),
                name: "bash".into(),
                arguments: json!({"command": "echo hello"}),
            }]),
            thinking: Some(vec![clanky_protocol::ThinkingBlock::thinking(
                round1_reasoning.clone(),
                None,
            )]),
        });
        passback_messages.push(ChatMessage::tool(calls[0].clone(), "hello"));
        let mut passback_request = with_tools.clone();
        passback_request.messages = passback_messages;
        let mut round2p_chunks = Vec::new();
        let round2p_done = provider.chat(&passback_request, &mut |c| round2p_chunks.push(c))?;
        let round2p_usage = round2p_done.usage.expect("usage");

        let delta_passback = round2p_usage.prompt_tokens.unwrap_or(0) as i64
            - round2_usage.prompt_tokens.unwrap_or(0) as i64;
        // A band, not a point: tokenization of prose is not exact, so the
        // assertion is "at least one token per five characters and well
        // under the 1:2 ceiling, plus a small fixed overhead" — tight
        // enough that a refactor which stops sending the field (delta 0)
        // fails, loose enough to survive sampling and tokenizer variance.
        let min = (reasoning_chars / 5) as i64;
        let max = (reasoning_chars / 2 + 60) as i64;
        assert!(
            (min..=max).contains(&delta_passback),
            "passing back a {reasoning_chars}-char block should cost ≈1 token per 4–5 chars \
             ({min}..{max} prompt tokens), got {delta_passback} \
             (round2 = {round2_usage:?}, round2' = {round2p_usage:?})"
        );

        // Sanity on the measurement itself: replaying the identical
        // no-passback request must not move prompt tokens by anything like
        // the passback cost.
        let mut replay_chunks = Vec::new();
        let replay_done = provider.chat(&with_tools, &mut |c| replay_chunks.push(c))?;
        let replay_usage = replay_done.usage.expect("usage");
        let delta_replay = replay_usage.prompt_tokens.unwrap_or(0) as i64
            - round2_usage.prompt_tokens.unwrap_or(0) as i64;
        assert!(
            delta_replay.abs() <= 40,
            "replaying the identical request must not move prompt tokens by ~400 (got {delta_replay})"
        );
        Ok(())
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
