//! LiteLLM proxy provider logic: protocol messages ⇄ the proxy's
//! OpenAI-compatible chat completions (streaming over SSE).
//!
//! LiteLLM sits in front of many backends and presents one OpenAI-shaped
//! `/v1` surface, so the delta/usage/tool-call translation is the same as the
//! DeepInfra plugin's. Two things are LiteLLM-specific:
//!
//! - **Metadata comes from `/model/info`.** That endpoint is *not* part of
//!   the LLM API routes, so a virtual key may be denied it (403). The catalog
//!   itself (`/v1/models`) is always available; the richer hints (context
//!   window, pricing, reasoning/tool support) are merged in when
//!   `/model/info` answers, and silently omitted when it does not.
//! - **Capabilities are per model, not per plugin.** The proxy is
//!   heterogeneous: one key may expose reasoning and non-reasoning, tool and
//!   non-tool models side by side. The handshake therefore advertises
//!   `thinking`/`tools` for the plugin as a whole, and the adapter filters the
//!   request per model using the metadata it cached from `/model/info` (drop
//!   `reasoning_effort` unless `supports_reasoning`, drop `tools` unless
//!   `supports_function_calling`). When metadata is unavailable the request is
//!   sent unchanged — LiteLLM's `drop_params`/`supported_openai_params`
//!   machinery is then the safety net.

use std::cell::RefCell;
use std::collections::HashMap;

use clanky_protocol::{
    CancelFlag, Capabilities, ChatDone, ChatMessage, ChatRequest, ChunkPayload, Error, ErrorCode,
    FinishReason, Handler, ModelInfo, PluginInfo, Thinking, Tool, Usage,
};
use serde_json::{Value, json};

use crate::backend::{Backend, BackendError, truncate_body};

/// Provider name reported at handshake.
pub const PROVIDER_NAME: &str = "litellm";
/// LiteLLM proxy base URL. Paths are relative to this, so a chat request goes
/// to `http://localhost:4000/v1/chat/completions`. Override with
/// `LITELLM_BASE_URL`; a value that already ends in `/v1` (or `/v1/`) is
/// accepted and not doubled.
pub const DEFAULT_BASE_URL: &str = "http://localhost:4000";
/// Auth env var read by this provider (never by Clanky core). A LiteLLM
/// proxy key starts with `sk-`.
pub const ENV_API_KEY: &str = "LITELLM_API_KEY";
/// Optional base URL override.
pub const ENV_BASE_URL: &str = "LITELLM_BASE_URL";
/// Optional default model advertised at handshake. The proxy's catalog is
/// arbitrary, so there is no built-in default; when this is unset the plugin
/// advertises none and Clanky requires an explicit `model`/`--model`.
pub const ENV_DEFAULT_MODEL: &str = "LITELLM_MODEL";

/// Dollars-per-token → dollars-per-million-tokens.
const PER_MTOK: f64 = 1_000_000.0;

/// LiteLLM provider, generic over the HTTP [`Backend`] so tests can inject
/// canned responses. The concrete blocking-HTTP instance is
/// [`crate::LiteLlm`].
pub struct LiteLlmProvider<B: Backend> {
    backend: B,
    base_url: String,
    /// Advertised default model (from `LITELLM_MODEL`), if any.
    default_model: Option<String>,
    /// Set by the plugin runtime (spec §7); checked between SSE events so a
    /// cancel stops generation at the next event boundary. A cancel during a
    /// single blocked read is covered by the client's kill fallback.
    cancel: CancelFlag,
    /// Per-model hints cached from `/model/info` on the first `listModels`,
    /// used to filter `reasoning_effort`/`tools` per model. `None` until a
    /// successful listing; `Some(empty)` when `/model/info` was unavailable
    /// (so the request is left unfiltered rather than refetched each turn).
    metadata: RefCell<Option<HashMap<String, ModelHints>>>,
}

impl LiteLlmProvider<crate::backend::UreqBackend> {
    /// Build the standard provider from the environment. Fails with an
    /// `auth` error when no API key is set (every LiteLLM proxy expects one).
    pub fn from_env() -> Result<Self, Error> {
        let api_key = std::env::var(ENV_API_KEY)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(missing_key_error)?;
        let base_url = std::env::var(ENV_BASE_URL)
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into());
        Ok(
            Self::new(crate::backend::UreqBackend::new(api_key), base_url)
                .with_default_model(default_model_from_env()),
        )
    }
}

/// The default model named by `LITELLM_MODEL`, if any. Read separately from
/// [`LiteLlmProvider::from_env`] so the plugin can still advertise it on the
/// handshake when the key is missing (the auth failure then surfaces on the
/// first request instead of a "no model" error).
pub fn default_model_from_env() -> Option<String> {
    std::env::var(ENV_DEFAULT_MODEL)
        .ok()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
}

fn missing_key_error() -> Error {
    Error::Provider {
        code: ErrorCode::Auth,
        message: format!("no API key: set {ENV_API_KEY}"),
        retryable: false,
        retry_after_ms: None,
    }
}

impl<B: Backend> LiteLlmProvider<B> {
    pub fn new(backend: B, base_url: impl Into<String>) -> Self {
        Self {
            backend,
            base_url: base_url.into(),
            default_model: None,
            cancel: CancelFlag::default(),
            metadata: RefCell::new(None),
        }
    }

    /// Advertise `model` as the handshake default (from `LITELLM_MODEL`).
    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }

    /// Join a path onto the base URL. A base that already ends in `/v1`
    /// (with or without a trailing slash) is not doubled, so both
    /// `http://host:4000` and `http://host:4000/v1` work.
    fn url(&self, path: &str) -> String {
        format!("{}/v1/{path}", self.base_root())
    }

    /// Join a path onto the base URL *without* the `/v1` prefix, for
    /// LiteLLM management routes. `/model/info` is served both bare and
    /// under `/v1`, but a virtual key's route allowlist names it bare
    /// (`llm_api_routes, '/model/info'`): the `/v1/` alias is registered on
    /// the same handler yet is not itself in the `llm_api`/info route
    /// groups, so a restricted key gets a 403 for exactly the path this
    /// plugin used to call. The bare form is the canonical one.
    fn root_url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_root())
    }

    /// The base URL, trimmed and with any `/v1` suffix removed.
    fn base_root(&self) -> &str {
        let base = self.base_url.trim_end_matches('/');
        base.strip_suffix("/v1").unwrap_or(base)
    }
}

impl<B: Backend> Handler for LiteLlmProvider<B> {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: PROVIDER_NAME.into(),
            capabilities: Capabilities {
                list_models: true,
                thinking: true,
                tools: true,
            },
            default_model: self.default_model.clone(),
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
        let mut models = parse_catalog(&body)?;
        let hints = self.fetch_hints();
        for model in &mut models {
            if let Some(hint) = hints.get(&model.id) {
                hint.apply(model);
            }
        }
        *self.metadata.borrow_mut() = Some(hints);
        Ok(models)
    }

    fn chat(
        &mut self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<ChatDone, Error> {
        // Per-model filtering needs `/model/info`, which `list_models`
        // normally loads. A client that never lists models (Clanky's pipe
        // mode) still gets it: fetch the hints once, lazily, before the first
        // request that can be filtered.
        if request.tools.is_some() || request.thinking.is_some() {
            self.ensure_metadata();
        }
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

impl<B: Backend> LiteLlmProvider<B> {
    /// Fetch `/model/info` hints, best effort. The endpoint is optional: it
    /// is not an LLM API route, so a virtual key may be denied it (403). A
    /// failure or malformed body yields an empty map — the catalog and chat
    /// keep working, only the metadata hints are omitted.
    fn fetch_hints(&self) -> HashMap<String, ModelHints> {
        match self.backend.get_status(&self.root_url("model/info")) {
            Ok((_, body)) => parse_model_info(&body),
            Err(err) => {
                eprintln!(
                    "clanky-provider-litellm: /model/info unavailable ({}); \
                     model metadata omitted",
                    err.status
                        .map_or_else(|| "no status".into(), |s| s.to_string())
                );
                HashMap::new()
            }
        }
    }

    /// Load the per-model hints once, on demand (see [`Self::chat`]).
    fn ensure_metadata(&mut self) {
        if self.metadata.borrow().is_none() {
            let hints = self.fetch_hints();
            *self.metadata.borrow_mut() = Some(hints);
        }
    }

    /// Build the OpenAI-compatible request body for a chat request.
    /// Streaming-first: SSE deltas come back as OpenAI-style chunks.
    ///
    /// `reasoning_effort` and `tools` are filtered per model using the
    /// `/model/info` hints, because the proxy mixes reasoning and
    /// non-reasoning, tool and non-tool models behind one key.
    fn build_request_body(&self, request: &ChatRequest) -> Result<String, Error> {
        let hints = self.metadata.borrow();
        let hint = hints.as_ref().and_then(|map| map.get(&request.model));

        let mut body = json!({
            "model": request.model,
            "messages": openai_messages(&request.messages)?,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let obj = body.as_object_mut().expect("object literal");

        let supports_tools = hint.and_then(|h| h.supports_tools).unwrap_or(true);
        if supports_tools
            && let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty())
        {
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

        let supports_reasoning = hint.and_then(|h| h.supports_reasoning).unwrap_or(true);
        if supports_reasoning && let Some(effort) = request.thinking.and_then(reasoning_effort) {
            obj.insert("reasoning_effort".into(), json!(effort));
        }

        serde_json::to_string(&body)
            .map_err(|e| Error::Protocol(format!("cannot serialize request: {e}")))
    }
}

/// Map a protocol thinking budget onto LiteLLM's `reasoning_effort` ladder.
/// `None` means "let the proxy decide" (or the model does not reason); an
/// explicit budget of 0 maps to `"none"`, which LiteLLM documents as
/// disabling reasoning — unlike omitting the field, which falls back to the
/// model default. Rungs: none/minimal/low/medium/high/xhigh/max.
fn reasoning_effort(thinking: Thinking) -> Option<&'static str> {
    match thinking.budget_tokens {
        None => None,
        Some(0) => Some("none"),
        Some(n) if n <= 512 => Some("minimal"),
        Some(n) if n <= 4096 => Some("low"),
        Some(n) if n <= 16384 => Some("medium"),
        Some(n) if n <= 32768 => Some("high"),
        Some(n) if n <= 65536 => Some("xhigh"),
        Some(_) => Some("max"),
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
                let obj = msg.as_object_mut().expect("object literal");
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

/// Map an HTTP-layer failure to a request-scoped protocol error. The message
/// is unwrapped from LiteLLM's error envelope (`{"error":{"message":…}}` or
/// `{"detail":…}`) when possible, so the user sees the proxy's own words
/// rather than raw JSON. A `Retry-After` hint is carried through as
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
        truncate_body(&extract_error_message(message.trim()), 400)
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

/// Pull the human-readable message out of a LiteLLM error body. The proxy
/// uses the OpenAI envelope for provider errors and a bare `detail` for its
/// own route/auth failures; anything else (or non-JSON) is passed through.
fn extract_error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
            return message.to_string();
        }
        if let Some(detail) = value.get("detail").and_then(Value::as_str) {
            return detail.to_string();
        }
    }
    body.to_string()
}

/// Parse the `GET /v1/models` catalog. Only `id` is required; the endpoint
/// carries no other useful metadata (that is what `/model/info` is for).
fn parse_catalog(body: &str) -> Result<Vec<ModelInfo>, Error> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| Error::Protocol(format!("malformed models response: {e}")))?;
    let data = root
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("models response has no `data` array".into()))?;
    Ok(data
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            Some(ModelInfo {
                display_name: Some(id.clone()),
                id,
                context_window: None,
                supports_thinking: None,
                supports_text_generation: None,
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_read_price_per_mtok: None,
            })
        })
        .collect())
}

/// Hints for one model, read from a `/model/info` row's `model_info` object.
/// All fields optional; `None` means "the proxy did not say".
#[derive(Debug, Clone, Default, PartialEq)]
struct ModelHints {
    display_name: Option<String>,
    context_window: Option<u64>,
    input_price_per_mtok: Option<f64>,
    output_price_per_mtok: Option<f64>,
    cache_read_price_per_mtok: Option<f64>,
    supports_reasoning: Option<bool>,
    supports_tools: Option<bool>,
    supports_text_generation: Option<bool>,
}

impl ModelHints {
    fn apply(&self, model: &mut ModelInfo) {
        if self.display_name.is_some() {
            model.display_name.clone_from(&self.display_name);
        }
        if self.context_window.is_some() {
            model.context_window = self.context_window;
        }
        if self.input_price_per_mtok.is_some() {
            model.input_price_per_mtok = self.input_price_per_mtok;
        }
        if self.output_price_per_mtok.is_some() {
            model.output_price_per_mtok = self.output_price_per_mtok;
        }
        if self.cache_read_price_per_mtok.is_some() {
            model.cache_read_price_per_mtok = self.cache_read_price_per_mtok;
        }
        if self.supports_reasoning.is_some() {
            model.supports_thinking = self.supports_reasoning;
        }
        if self.supports_text_generation.is_some() {
            model.supports_text_generation = self.supports_text_generation;
        }
    }
}

/// Parse `GET /model/info` into a `model_name → hints` map. Load-balanced
/// groups repeat a `model_name` (one row per deployment), so the first row
/// wins — mirroring the Pi extension's `if (name in map) continue`. A
/// malformed body yields an empty map (the caller degrades gracefully).
fn parse_model_info(body: &str) -> HashMap<String, ModelHints> {
    let mut map = HashMap::new();
    let Ok(root) = serde_json::from_str::<Value>(body) else {
        return map;
    };
    let Some(data) = root.get("data").and_then(Value::as_array) else {
        return map;
    };
    for entry in data {
        let Some(name) = entry.get("model_name").and_then(Value::as_str) else {
            continue;
        };
        if map.contains_key(name) {
            continue;
        }
        let info = entry.get("model_info");
        let get = |key: &str| info.and_then(|i| i.get(key));
        let u64_of = |key: &str| get(key).and_then(Value::as_u64);
        let f64_of = |key: &str| {
            get(key)
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0)
        };
        let mode = get("mode").and_then(Value::as_str);
        map.insert(
            name.to_string(),
            ModelHints {
                display_name: get("display_name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                // `max_input_tokens` is the prompt-context limit; some
                // providers only populate `max_tokens`.
                context_window: u64_of("max_input_tokens").or_else(|| u64_of("max_tokens")),
                input_price_per_mtok: f64_of("input_cost_per_token").map(|p| p * PER_MTOK),
                output_price_per_mtok: f64_of("output_cost_per_token").map(|p| p * PER_MTOK),
                cache_read_price_per_mtok: f64_of("cache_read_input_token_cost")
                    .map(|p| p * PER_MTOK),
                supports_reasoning: get("supports_reasoning").and_then(Value::as_bool),
                supports_tools: get("supports_function_calling").and_then(Value::as_bool),
                // `mode` is the LiteLLM model class: `chat`/`responses` can
                // generate text, `embedding`/`image_generation` cannot.
                supports_text_generation: mode.map(|m| m == "chat" || m == "responses"),
            },
        );
    }
    map
}

// --- OpenAI streaming chunk shapes (parsed leniently; LiteLLM is
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
        while self.calls.len() <= call.index as usize {
            self.calls.push(CallAccumulator::default());
        }
        let acc = &mut self.calls[call.index as usize];
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
    use clanky_protocol::{Sampling, ToolCall};

    const CATALOG: &str = include_str!("../tests/fixtures/v1_models.json");
    const MODEL_INFO: &str = include_str!("../tests/fixtures/model_info.json");
    const STREAM_PLAIN: &str = include_str!("../tests/fixtures/stream_plain.txt");
    const STREAM_REASONING: &str = include_str!("../tests/fixtures/stream_reasoning.txt");
    const STREAM_TOOLS: &str = include_str!("../tests/fixtures/stream_tools.txt");

    /// Records requests and replays canned responses. `/v1/models` and
    /// `/model/info` are configured independently so a test can make the
    /// optional metadata endpoint fail.
    #[derive(Default)]
    struct MockBackend {
        posts: RefCell<Vec<(String, String)>>,
        gets: RefCell<Vec<String>>,
        catalog: Option<String>,
        /// `Some` makes `/model/info` fail; `None` answers with `info_body`.
        info_error: Option<BackendError>,
        info_body: String,
        sse: Vec<String>,
        post_error: Option<BackendError>,
    }

    impl MockBackend {
        fn new(catalog: &str, info: &str, sse: Vec<String>) -> Self {
            Self {
                catalog: Some(catalog.to_string()),
                info_body: info.to_string(),
                sse,
                ..Self::default()
            }
        }

        fn last_body(&self) -> Value {
            let posts = self.posts.borrow();
            serde_json::from_str(&posts.last().expect("a chat was posted").1).unwrap()
        }
    }

    impl Backend for MockBackend {
        fn get(&self, url: &str) -> Result<String, BackendError> {
            self.get_status(url).map(|(_, body)| body)
        }

        fn get_status(&self, url: &str) -> Result<(u16, String), BackendError> {
            self.gets.borrow_mut().push(url.to_string());
            if url.ends_with("/model/info") {
                match &self.info_error {
                    Some(err) => Err(err.clone()),
                    None => Ok((200, self.info_body.clone())),
                }
            } else {
                self.catalog
                    .clone()
                    .map(|body| (200, body))
                    .ok_or_else(|| BackendError::new(None, "no catalog configured"))
            }
        }

        fn post_stream(
            &self,
            url: &str,
            body: &str,
        ) -> Result<Box<dyn Iterator<Item = Result<String, BackendError>>>, BackendError> {
            self.posts
                .borrow_mut()
                .push((url.to_string(), body.to_string()));
            if let Some(err) = &self.post_error {
                return Err(err.clone());
            }
            Ok(Box::new(self.sse.clone().into_iter().map(Ok)))
        }
    }

    /// Turn captured SSE text (`data: {...}` lines) into the payloads the
    /// backend yields: one per event, with the `data: ` prefix stripped.
    fn payloads(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.strip_prefix(' ').unwrap_or(line).to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }

    fn provider(catalog: &str, info: &str, sse: Vec<String>) -> LiteLlmProvider<MockBackend> {
        LiteLlmProvider::new(
            MockBackend::new(catalog, info, sse),
            "http://localhost:4000",
        )
    }

    fn request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![
                ChatMessage::system("You are terse."),
                ChatMessage::user("say hi"),
            ],
            tools: None,
            sampling: None,
            thinking: None,
        }
    }

    fn run_chat(
        provider: &mut LiteLlmProvider<MockBackend>,
        request: &ChatRequest,
    ) -> (Vec<ChunkPayload>, Result<ChatDone, Error>) {
        let mut chunks = Vec::new();
        let done = provider.chat(request, &mut |chunk| chunks.push(chunk));
        (chunks, done)
    }

    fn find<'a>(models: &'a [ModelInfo], id: &str) -> &'a ModelInfo {
        models
            .iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("no model {id}"))
    }

    // --- handshake -------------------------------------------------------

    #[test]
    fn handshake_reports_name_and_capabilities() {
        let provider = provider(CATALOG, MODEL_INFO, Vec::new());
        let info = provider.info();
        assert_eq!(info.name, "litellm");
        assert!(info.capabilities.list_models);
        assert!(info.capabilities.thinking);
        assert!(info.capabilities.tools);
        // The proxy's catalog is arbitrary: no built-in default.
        assert_eq!(info.default_model, None);
    }

    #[test]
    fn handshake_advertises_an_optional_default_model() {
        let provider = provider(CATALOG, MODEL_INFO, Vec::new())
            .with_default_model(Some("claude-sonnet-4-6".into()));
        assert_eq!(
            provider.info().default_model.as_deref(),
            Some("claude-sonnet-4-6")
        );
    }

    // --- catalog + metadata ----------------------------------------------

    #[test]
    fn catalog_and_model_info_merge_into_hints() {
        let mut provider = provider(CATALOG, MODEL_INFO, Vec::new());
        let models = provider.list_models().unwrap();
        assert_eq!(models.len(), 12);

        // Both endpoints, in order: catalog first, then the optional metadata.
        assert_eq!(
            provider.backend.gets.borrow().as_slice(),
            [
                "http://localhost:4000/v1/models",
                "http://localhost:4000/model/info"
            ]
        );

        let sonnet = find(&models, "claude-sonnet-4-6");
        assert_eq!(sonnet.context_window, Some(1_000_000));
        assert_eq!(sonnet.input_price_per_mtok, Some(3.0));
        assert_eq!(sonnet.output_price_per_mtok, Some(15.0));
        assert_eq!(sonnet.cache_read_price_per_mtok, Some(0.3));
        assert_eq!(sonnet.supports_thinking, Some(true));
        assert_eq!(sonnet.supports_text_generation, Some(true));

        // `mode: "responses"` still generates text; `max_input_tokens` is the
        // context window even when it exceeds `max_tokens`.
        let gpt = find(&models, "gpt-5.5");
        assert_eq!(gpt.context_window, Some(272_000));
        assert_eq!(gpt.input_price_per_mtok, Some(5.5));
        assert_eq!(gpt.output_price_per_mtok, Some(33.0));
        assert_eq!(gpt.supports_text_generation, Some(true));
    }

    #[test]
    fn model_info_failure_degrades_to_the_plain_catalog() {
        let mut provider = provider(CATALOG, "", Vec::new());
        provider.backend.info_error = Some(BackendError::new(
            Some(403),
            r#"{"detail":"Virtual key is not allowed to call this route."}"#,
        ));
        let models = provider.list_models().unwrap();
        assert_eq!(models.len(), 12);
        // The id survives; the metadata hints are simply absent.
        let sonnet = find(&models, "claude-sonnet-4-6");
        assert_eq!(sonnet.display_name.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(sonnet.context_window, None);
        assert_eq!(sonnet.supports_thinking, None);
    }

    #[test]
    fn model_info_uses_the_bare_path_a_restricted_key_is_allowed() {
        // Regression: the plugin used to call `/v1/model/info`. The handler
        // is aliased, but a virtual key whose `allowed_routes` name
        // `['llm_api_routes', '/model/info']` (LiteLLM's own error message
        // spells the allowlist out) is denied the `/v1/` alias with a 403 —
        // which silently dropped every model's cost/context metadata. The
        // bare `/model/info` path is in the allowlist, so it must be the one
        // the plugin calls.
        let mut provider = provider(CATALOG, MODEL_INFO, Vec::new());
        let models = provider.list_models().unwrap();
        assert_eq!(
            provider.backend.gets.borrow().as_slice(),
            [
                "http://localhost:4000/v1/models",
                "http://localhost:4000/model/info"
            ],
            "`/model/info` must be requested bare, not under `/v1`"
        );
        // And the metadata actually arrives (nothing degraded).
        assert_eq!(
            find(&models, "claude-sonnet-4-6").input_price_per_mtok,
            Some(3.0)
        );
    }

    #[test]
    fn a_duplicate_model_name_keeps_the_first_row() {
        let info = json!({
            "data": [
                {"model_name": "gpt-x", "model_info": {"max_input_tokens": 111}},
                {"model_name": "gpt-x", "model_info": {"max_input_tokens": 999}}
            ]
        })
        .to_string();
        let catalog = json!({"data": [{"id": "gpt-x"}]}).to_string();
        let mut provider = provider(&catalog, &info, Vec::new());
        let models = provider.list_models().unwrap();
        assert_eq!(models[0].context_window, Some(111));
    }

    // --- streaming -------------------------------------------------------

    #[test]
    fn plain_stream_becomes_text_chunks_with_usage() {
        let mut provider = provider(CATALOG, MODEL_INFO, payloads(STREAM_PLAIN));
        let (chunks, done) = run_chat(&mut provider, &request("claude-sonnet-4-6"));

        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text,
            "Hi there! 👋 How are you doing? Is there something I can help you with today?"
        );

        let done = done.unwrap();
        assert_eq!(done.finish_reason, FinishReason::Stop);
        assert_eq!(
            done.usage,
            Some(Usage {
                prompt_tokens: Some(9),
                completion_tokens: Some(25),
                cached_tokens: Some(0),
            })
        );
    }

    #[test]
    fn reasoning_stream_separates_thinking_from_text() {
        let mut provider = provider(CATALOG, MODEL_INFO, payloads(STREAM_REASONING));
        let (chunks, done) = run_chat(&mut provider, &request("claude-sonnet-4-6"));

        let thinking: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Thinking { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "4");

        // The visible answer is clean: the reasoning never leaks into it.
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.starts_with("**Thinking it through:**"), "{text}");
        assert!(text.ends_with("4**"), "{text}");
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);
    }

    #[test]
    fn tool_stream_becomes_start_and_args_chunks() {
        let mut provider = provider(CATALOG, MODEL_INFO, payloads(STREAM_TOOLS));
        let (chunks, done) = run_chat(&mut provider, &request("claude-sonnet-4-6"));

        let starts: Vec<&ChunkPayload> = chunks
            .iter()
            .filter(|c| matches!(c, ChunkPayload::ToolCallStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1, "one tool call, announced once");
        match starts[0] {
            ChunkPayload::ToolCallStart { index, id, name } => {
                assert_eq!(*index, 0);
                assert_eq!(id, "tooluse_rKQZEwx7icQfqiTvo6jjxo");
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
        assert_eq!(
            serde_json::from_str::<Value>(&args).unwrap(),
            json!({"command": "ls /tmp"})
        );
        assert_eq!(done.unwrap().finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn legacy_function_call_finish_reason_maps_to_tool_calls() {
        // LiteLLM still emits the legacy reason on some routes; clanky must
        // run the tool, not treat it as a plain stop.
        let sse = vec![
            json!({"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_1", "type": "function",
                "function": {"name": "bash", "arguments": "{}"}
            }]}, "finish_reason": null}]})
            .to_string(),
            json!({"choices": [{"delta": {}, "finish_reason": "function_call"}]}).to_string(),
            "[DONE]".to_string(),
        ];
        let mut provider = provider(CATALOG, MODEL_INFO, sse);
        let (_, done) = run_chat(&mut provider, &request("claude-sonnet-4-6"));
        assert_eq!(done.unwrap().finish_reason, FinishReason::ToolCalls);
    }

    // --- request building ------------------------------------------------

    #[test]
    fn request_body_is_openai_shaped_and_streaming() {
        let mut provider = provider(
            CATALOG,
            MODEL_INFO,
            vec![r#"{"choices":[{"finish_reason":"stop","delta":{}}]}"#.into()],
        );
        let mut req = request("claude-sonnet-4-6");
        req.sampling = Some(Sampling {
            temperature: Some(0.5),
            top_p: Some(0.9),
            max_tokens: Some(256),
        });
        req.tools = Some(vec![Tool {
            name: "bash".into(),
            description: Some("Run a shell command".into()),
            parameters: Some(json!({"type": "object"})),
        }]);
        run_chat(&mut provider, &req).1.unwrap();

        let body = provider.backend.last_body();
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "bash");
        // Endpoint is `{base}/v1/chat/completions`.
        assert_eq!(
            provider.backend.posts.borrow()[0].0,
            "http://localhost:4000/v1/chat/completions"
        );
    }

    #[test]
    fn assistant_tool_calls_and_tool_results_round_trip() {
        let mut provider = provider(
            CATALOG,
            MODEL_INFO,
            vec![r#"{"choices":[{"finish_reason":"stop","delta":{}}]}"#.into()],
        );
        let req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![
                ChatMessage::user("list /tmp"),
                ChatMessage::Assistant {
                    content: String::new(),
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "ls /tmp"}),
                    }]),
                },
                ChatMessage::tool("call_1", "a\nb"),
            ],
            tools: None,
            sampling: None,
            thinking: None,
        };
        run_chat(&mut provider, &req).1.unwrap();

        let body = provider.backend.last_body();
        // Arguments are re-encoded as a JSON *string* (the OpenAI convention).
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls /tmp\"}"
        );
        assert_eq!(body["messages"][2]["role"], "tool");
        assert_eq!(body["messages"][2]["tool_call_id"], "call_1");
    }

    #[test]
    fn base_url_variants_are_normalized() {
        for base in [
            "http://localhost:4000",
            "http://localhost:4000/",
            "http://localhost:4000/v1",
            "http://localhost:4000/v1/",
        ] {
            let mut provider = LiteLlmProvider::new(
                MockBackend::new(CATALOG, MODEL_INFO, vec![r#"{"choices":[]}"#.into()]),
                base,
            );
            provider.list_models().unwrap();
            assert_eq!(
                provider.backend.gets.borrow()[0],
                "http://localhost:4000/v1/models",
                "base `{base}`"
            );
        }
    }

    // --- per-model capability filtering ----------------------------------

    #[test]
    fn thinking_budget_maps_to_the_reasoning_effort_ladder() {
        let mut provider = provider(CATALOG, MODEL_INFO, vec![r#"{"choices":[]}"#.into()]);
        for (budget, expected) in [
            (0u32, "none"),
            (256, "minimal"),
            (1024, "low"),
            (8192, "medium"),
            (20000, "high"),
            (40000, "xhigh"),
            (100000, "max"),
        ] {
            let mut req = request("claude-sonnet-4-6");
            req.thinking = Some(Thinking {
                budget_tokens: Some(budget),
            });
            run_chat(&mut provider, &req).1.unwrap();
            assert_eq!(
                provider.backend.last_body()["reasoning_effort"],
                expected,
                "budget {budget}"
            );
        }
    }

    #[test]
    fn thinking_is_dropped_for_a_model_without_reasoning() {
        let info = json!({"data": [
            {"model_name": "plain", "model_info": {"mode": "chat", "supports_reasoning": false}}
        ]})
        .to_string();
        let catalog = json!({"data": [{"id": "plain"}]}).to_string();
        let mut provider = provider(&catalog, &info, vec![r#"{"choices":[]}"#.into()]);
        provider.list_models().unwrap();

        let mut req = request("plain");
        req.thinking = Some(Thinking {
            budget_tokens: Some(4096),
        });
        run_chat(&mut provider, &req).1.unwrap();
        assert!(
            provider
                .backend
                .last_body()
                .get("reasoning_effort")
                .is_none()
        );
    }

    #[test]
    fn tools_are_dropped_for_a_model_without_function_calling() {
        let info = json!({"data": [
            {"model_name": "plain", "model_info": {"mode": "chat", "supports_function_calling": false}}
        ]})
        .to_string();
        let catalog = json!({"data": [{"id": "plain"}]}).to_string();
        let mut provider = provider(&catalog, &info, vec![r#"{"choices":[]}"#.into()]);
        provider.list_models().unwrap();

        let mut req = request("plain");
        req.tools = Some(vec![Tool {
            name: "bash".into(),
            description: None,
            parameters: None,
        }]);
        run_chat(&mut provider, &req).1.unwrap();
        assert!(provider.backend.last_body().get("tools").is_none());
    }

    #[test]
    fn a_model_absent_from_the_metadata_is_left_unfiltered() {
        // The proxy did not describe this model, so there is nothing to
        // filter on: send tools/thinking as-is and let the proxy's own
        // `drop_params` machinery decide.
        let mut provider = provider(CATALOG, MODEL_INFO, vec![r#"{"choices":[]}"#.into()]);
        let mut req = request("not-in-model-info");
        req.thinking = Some(Thinking {
            budget_tokens: Some(1024),
        });
        req.tools = Some(vec![Tool {
            name: "bash".into(),
            description: None,
            parameters: None,
        }]);
        run_chat(&mut provider, &req).1.unwrap();
        let body = provider.backend.last_body();
        assert_eq!(body["reasoning_effort"], "low");
        assert!(body.get("tools").is_some());
    }

    #[test]
    fn chat_fetches_metadata_lazily_without_a_prior_listing() {
        // Clanky's pipe mode never calls `listModels`, so a chat carrying
        // tools must still load `/model/info` to filter per model.
        let info = json!({"data": [
            {"model_name": "plain", "model_info": {"mode": "chat", "supports_function_calling": false}}
        ]})
        .to_string();
        let catalog = json!({"data": [{"id": "plain"}]}).to_string();
        let mut provider = provider(&catalog, &info, vec![r#"{"choices":[]}"#.into()]);
        // No `list_models()` here.
        let mut req = request("plain");
        req.tools = Some(vec![Tool {
            name: "bash".into(),
            description: None,
            parameters: None,
        }]);
        run_chat(&mut provider, &req).1.unwrap();
        assert!(
            provider.backend.last_body().get("tools").is_none(),
            "the lazy fetch must have filtered `tools`"
        );
        assert_eq!(
            provider.backend.gets.borrow().as_slice(),
            ["http://localhost:4000/model/info"],
            "only `/model/info` is fetched lazily, not the catalog"
        );
    }

    // --- errors ----------------------------------------------------------

    #[test]
    fn error_envelope_is_unwrapped_into_a_readable_message() {
        // The exact body the proxy returned for an unknown model (a 400).
        let body = r#"{"error":{"message":"/chat/completions: Invalid model name passed in model=no-such-model. Call `/v1/models` to view available models for your key.","type":"None","param":"None","code":"400"}}"#;
        let mut provider = provider(CATALOG, MODEL_INFO, Vec::new());
        provider.backend.post_error = Some(BackendError::new(Some(400), body));
        let err = run_chat(&mut provider, &request("no-such-model"))
            .1
            .unwrap_err();
        match err {
            Error::Provider {
                code,
                message,
                retryable,
                ..
            } => {
                assert_eq!(code, ErrorCode::InvalidRequest);
                assert!(!retryable);
                assert!(
                    message.starts_with("400: /chat/completions: Invalid model name"),
                    "{message}"
                );
                // Not the raw JSON envelope.
                assert!(!message.contains("\"error\""), "{message}");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn route_denial_detail_is_unwrapped() {
        // Real 403 body from a proxy whose virtual key was allowed
        // `llm_api_routes` + `/model/info` but denied the `/v1/model/info`
        // alias — the reason `fetch_hints` calls the bare path.
        let body = r#"{"detail":"Virtual key is not allowed to call this route. Only allowed to call routes: ['llm_api_routes', '/model/info']. Tried to call route: /v1/model/info"}"#;
        let mut provider = provider(CATALOG, MODEL_INFO, Vec::new());
        provider.backend.post_error = Some(BackendError::new(Some(403), body));
        let err = run_chat(&mut provider, &request("x")).1.unwrap_err();
        let Error::Provider { code, message, .. } = err else {
            panic!("expected a provider error");
        };
        assert_eq!(code, ErrorCode::Auth);
        assert!(
            message.starts_with("403: Virtual key is not allowed"),
            "{message}"
        );
    }

    #[test]
    fn http_status_maps_to_error_codes() {
        let cases: [(u16, ErrorCode, bool); 6] = [
            (401, ErrorCode::Auth, false),
            (403, ErrorCode::Auth, false),
            (429, ErrorCode::RateLimit, true),
            (400, ErrorCode::InvalidRequest, false),
            (404, ErrorCode::InvalidRequest, false),
            (500, ErrorCode::Backend, true),
        ];
        for (status, expected, retryable) in cases {
            let mut provider = provider(CATALOG, MODEL_INFO, Vec::new());
            provider.backend.post_error = Some(BackendError::new(Some(status), "boom"));
            let err = run_chat(&mut provider, &request("x")).1.unwrap_err();
            match err {
                Error::Provider {
                    code,
                    message,
                    retryable: got,
                    ..
                } => {
                    assert_eq!(code, expected, "status {status}");
                    assert_eq!(got, retryable, "status {status}");
                    assert!(message.contains("boom"), "{message}");
                }
                other => panic!("unexpected: {other}"),
            }
        }
    }

    #[test]
    fn rate_limit_carries_a_retry_after_hint() {
        let mut provider = provider(CATALOG, MODEL_INFO, Vec::new());
        provider.backend.post_error = Some(
            BackendError::new(Some(429), r#"{"error":{"message":"rate limited"}}"#)
                .with_retry_after_header("7", 0),
        );
        let err = run_chat(&mut provider, &request("x")).1.unwrap_err();
        match err {
            Error::Provider {
                code,
                retryable,
                retry_after_ms,
                ..
            } => {
                assert_eq!(code, ErrorCode::RateLimit);
                assert!(retryable);
                assert_eq!(retry_after_ms, Some(7_000));
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn transport_timeouts_are_retryable_backend_errors() {
        let mut provider = provider(CATALOG, MODEL_INFO, Vec::new());
        provider.backend.post_error = Some(BackendError::new(None, "timed out reading response"));
        match run_chat(&mut provider, &request("x")).1.unwrap_err() {
            Error::Provider {
                code, retryable, ..
            } => {
                assert_eq!(code, ErrorCode::Backend);
                assert!(retryable, "a network timeout must be retryable");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn malformed_stream_chunk_is_a_protocol_error() {
        let mut provider = provider(CATALOG, MODEL_INFO, vec!["{not json".into()]);
        let err = run_chat(&mut provider, &request("x")).1.unwrap_err();
        assert!(err.to_string().contains("malformed stream chunk"), "{err}");
    }

    #[test]
    fn empty_stream_yields_no_chunks_but_a_done() {
        let mut provider = provider(
            CATALOG,
            MODEL_INFO,
            vec![r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#.into()],
        );
        let (chunks, done) = run_chat(&mut provider, &request("x"));
        assert!(chunks.is_empty());
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);
    }
}
