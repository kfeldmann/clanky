//! OpenRouter provider logic: protocol messages ⇄ OpenRouter's
//! OpenAI-compatible chat completions (streaming over SSE).
//!
//! The delta/usage/tool-call translation is shared (`clanky-provider-openai`);
//! what stays here is OpenRouter's own policy:
//!
//! - **Reasoning has its own request shape.** Chat Completions takes a
//!   top-level `reasoning` object (`effort`, `max_tokens`, `enabled`, …) and
//!   passes reasoning back as `reasoning_details`, the structured union of
//!   `reasoning.text` / `reasoning.encrypted` / `reasoning.summary` items.
//!   `reasoning_content` — the field DeepInfra and LiteLLM document — is an
//!   undocumented alias in OpenRouter's chat-completions schema, so this
//!   plugin never sends it (M24 design constraint 1).
//! - **A 200 is not a success.** OpenRouter reports mid-generation failures
//!   *inside* the stream: a chunk with a top-level `error` object and/or a
//!   choice whose `finish_reason` is `"error"`. Both terminate the turn as a
//!   request-scoped error carrying the provider's own text. A stream that
//!   ends with no `finish_reason` at all is a truncated answer, not a `stop`.
//! - **`402` is two cases wearing one status.** Credits exhausted
//!   (`openrouter_credits`, `openrouter_key_limit`) is a hard stop; the
//!   in-flight budget (`openrouter_in_flight_budget`) is transient and is the
//!   only 402 OpenRouter sends a `Retry-After` with. The plugin maps it to
//!   `rateLimit` (retryable, with the hint) exactly when the header is
//!   present, and to `auth` otherwise.
//! - **The catalog is rich but full of sentinels.** `pricing` values are
//!   strings, `-1` (dynamic routers) and `0` (`:free` variants) mean "no
//!   fixed price", and `:batch` entries are not served by the chat endpoint
//!   at all.
//!
//! Per-model hints curate the *model list* only (the `/model` picker), never
//! the request: `tools` and the derived `reasoning` object are sent
//! unconditionally, and when the model cannot take them OpenRouter's own
//! error is the answer (project invariants: user values are authoritative,
//! the server validates).

use clanky_protocol::{
    CancelFlag, Capabilities, ChatDone, ChatRequest, ChunkPayload, Error, ErrorCode, Handler,
    ModelInfo, PluginInfo, Thinking, ThinkingBlock, ThinkingBlockKind,
};
use clanky_provider_openai::stream::{OpenAiStreamChunk, StreamState};
use clanky_provider_openai::wire::{openai_messages, openai_tools};
use serde_json::{Map, Value, json};

use crate::backend::{Backend, BackendError, truncate_body};

/// Provider name reported at handshake.
pub const PROVIDER_NAME: &str = "openrouter";
/// OpenRouter's API base URL. Paths are appended to this, so a chat request
/// goes to `https://openrouter.ai/api/v1/chat/completions`. Override with
/// `OPENROUTER_BASE_URL` (the value is used as given — append `/api/v1`
/// yourself if your base does not already end in it).
pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// Auth env var read by this provider (never by Clanky core). An OpenRouter
/// key starts with `sk-or-v1-`.
pub const ENV_API_KEY: &str = "OPENROUTER_API_KEY";
/// Optional base URL override.
pub const ENV_BASE_URL: &str = "OPENROUTER_BASE_URL";
/// Optional app-attribution header, sent verbatim when set.
pub const ENV_HTTP_REFERER: &str = "OPENROUTER_HTTP_REFERER";
/// Optional app-attribution header, sent verbatim when set.
pub const ENV_TITLE: &str = "OPENROUTER_TITLE";
/// The built-in default model advertised at handshake, used when neither
/// settings nor `--model` names one: cheap, 1M context, tools, and
/// non-mandatory reasoning (so `--thinking off` is legal on it).
pub const DEFAULT_MODEL: &str = "deepseek/deepseek-v4.1-flash";

/// The one endpoint a chat model must be served by to appear in the picker.
const CHAT_ENDPOINT: &str = "/api/v1/chat/completions";

/// Dollars-per-token → dollars-per-million-tokens.
const PER_MTOK: f64 = 1_000_000.0;

/// OpenRouter provider, generic over the HTTP [`Backend`] so tests can
/// inject canned responses. The concrete blocking-HTTP instance is
/// [`crate::OpenRouter`].
pub struct OpenRouterProvider<B: Backend> {
    backend: B,
    base_url: String,
    /// Set by the plugin runtime (spec §7); checked between SSE events so a
    /// cancel stops generation at the next event boundary. A cancel during a
    /// single blocked read is covered by the client's kill fallback.
    cancel: CancelFlag,
}

impl OpenRouterProvider<crate::backend::UreqBackend> {
    /// Build the standard provider from the environment. Fails with an
    /// `auth` error when no API key is set (every request needs one); the
    /// plugin still handshakes, so the failure surfaces on the first turn
    /// rather than as a dead process.
    ///
    /// The app-attribution headers are read verbatim and only sent when the
    /// user set them: no invented title, no invented referer (M24 decision
    /// 8).
    pub fn from_env() -> Result<Self, Error> {
        let api_key = std::env::var(ENV_API_KEY)
            .ok()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(missing_key_error)?;
        let base_url = std::env::var(ENV_BASE_URL)
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into());
        let mut headers = Vec::new();
        for (env, header) in [
            (ENV_HTTP_REFERER, "HTTP-Referer"),
            (ENV_TITLE, "X-OpenRouter-Title"),
        ] {
            if let Some(value) = std::env::var(env).ok().filter(|v| !v.trim().is_empty()) {
                headers.push((header.to_string(), value));
            }
        }
        Ok(Self::new(
            crate::backend::UreqBackend::with_headers(api_key, headers),
            base_url,
        ))
    }
}

fn missing_key_error() -> Error {
    Error::Provider {
        code: ErrorCode::Auth,
        message: format!("no API key: set {ENV_API_KEY}"),
        retryable: false,
        retry_after_ms: None,
    }
}

impl<B: Backend> OpenRouterProvider<B> {
    pub fn new(backend: B, base_url: impl Into<String>) -> Self {
        Self {
            backend,
            base_url: base_url.into(),
            cancel: CancelFlag::default(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url.trim_end_matches('/'))
    }
}

impl<B: Backend> Handler for OpenRouterProvider<B> {
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

    /// The key-scoped catalog (`/models/user`), not the public one: the
    /// public list also carries models this key cannot call. Both endpoints
    /// are optional-auth, so the key goes along either way.
    fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
        let body = self
            .backend
            .get(&self.url("models/user"))
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
        let mut reasoning = ReasoningAccumulator::default();
        // A `done` may only ever follow a real finish reason (constraint 2).
        let mut saw_finish_reason = false;
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
            let value: Value = serde_json::from_str(trimmed)
                .map_err(|e| Error::Protocol(format!("malformed stream chunk: {e}")))?;

            // A 200 with an `error` object is a provider failure, not output.
            let error = value.get("error").filter(|e| !e.is_null()).cloned();
            // `finish_reason: "error"` terminates the stream too. It is
            // stripped before the shared accumulator sees the chunk, whose
            // mapping would otherwise read it as an unknown reason — i.e. a
            // plain `stop`.
            let mut error_finish = false;
            let mut streamed = value.clone();
            if let Some(choices) = streamed.get_mut("choices").and_then(Value::as_array_mut) {
                for choice in choices {
                    if choice.get("finish_reason").and_then(Value::as_str) == Some("error") {
                        error_finish = true;
                        choice["finish_reason"] = Value::Null;
                    }
                }
            }
            // Deliver the delta before acting on the failure: content that
            // arrived alongside it is real output the user already has, and
            // events emitted before an `Err` are simply the stream so far
            // (spec §6).
            for choice in choices(&value) {
                if let Some(delta) = choice.get("delta") {
                    reasoning.ingest(delta, sink)?;
                }
                if choice
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .is_some_and(|reason| reason != "error")
                {
                    saw_finish_reason = true;
                }
            }
            let chunk: OpenAiStreamChunk = serde_json::from_value(streamed)
                .map_err(|e| Error::Protocol(format!("malformed stream chunk: {e}")))?;
            state.ingest(chunk, sink);

            if let Some(error) = error {
                return Err(mid_stream_error(&error));
            }
            if error_finish {
                // The documented mid-stream failure normally rides with the
                // top-level `error` handled above; without one the provider
                // still failed, and this must not become a successful empty
                // answer (design constraint 2).
                return Err(Error::Provider {
                    code: ErrorCode::Backend,
                    message: "the provider ended the stream with finish_reason \"error\" \
                              and no error message"
                        .into(),
                    retryable: true,
                    retry_after_ms: None,
                });
            }
        }
        if !saw_finish_reason {
            // A stream that stops without a finish reason was cut off mid
            // answer; reporting `stop` would turn a truncated answer into a
            // successful one.
            return Err(Error::Provider {
                code: ErrorCode::Backend,
                message: "stream ended without a finish reason (the answer was cut off)".into(),
                retryable: true,
                retry_after_ms: None,
            });
        }
        // The round's finished reasoning blocks, in generation order, for the
        // next request to pass back verbatim.
        reasoning.emit_blocks(sink);
        Ok(state.finish())
    }
}

impl<B: Backend> OpenRouterProvider<B> {
    /// Build the OpenAI-compatible request body for a chat request.
    /// Streaming-first: SSE deltas come back as OpenAI-style chunks.
    ///
    /// `sampling` entries pass through to OpenRouter verbatim (spec §5):
    /// arbitrary backend parameters work and the backend validates them.
    /// The request's own structural fields belong to the protocol and are set
    /// here afterwards, so they cannot be overridden through `sampling`.
    ///
    /// `stream_options.include_usage` is deliberately *not* sent: OpenRouter
    /// documents it as deprecated and without effect (full usage is always
    /// returned), so sending it would be noise (M24 decision 6). The final
    /// usage chunk is pinned by a test instead.
    ///
    /// `tools` and the derived `reasoning` object are sent unconditionally —
    /// even for a model whose catalog hints say it supports neither. Dropping
    /// them would silently ignore what the user asked for and pre-empt
    /// OpenRouter's own validation; the hints curate the model list (see
    /// [`parse_models`]), not the request.
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
                &openrouter_reasoning_passback,
            )),
        );
        obj.insert("stream".into(), json!(true));

        if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
            obj.insert("tools".into(), openai_tools(tools));
        }

        // The thinking → `reasoning.effort` mapping only fills the gap: a
        // `reasoning` (or `reasoning_effort`) the user set in `sampling` is
        // never overwritten — pass-through means the backend decides.
        if !obj.contains_key("reasoning")
            && !obj.contains_key("reasoning_effort")
            && let Some(effort) = request.thinking.and_then(reasoning_effort)
        {
            obj.insert("reasoning".into(), json!({ "effort": effort }));
        }

        serde_json::to_string(&body)
            .map_err(|e| Error::Protocol(format!("cannot serialize request: {e}")))
    }
}

/// Map a protocol thinking budget onto OpenRouter's `reasoning.effort` ladder.
///
/// The semantics differ from DeepInfra/LiteLLM: an OpenRouter effort is a
/// *fraction of the request's output token limit* (`max`/`xhigh` ≈95%,
/// `high` ≈80%, `medium` ≈50%, `low` ≈20%, `minimal` ≈10%), not an absolute
/// budget. The rungs are the same, so `--thinking <budget>` keeps its meaning
/// ("how hard to think") and README says the translation is lossy.
///
/// `None` means "let OpenRouter decide" (or the model does not reason); an
/// explicit budget of 0 maps to `"none"`, which disables reasoning — unlike
/// omitting the field, which falls back to the model default. A model whose
/// catalog says `mandatory: true` rejects `"none"`; the request is sent as
/// asked and the 400 is surfaced (constraint 5 — the server owns that rule).
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

/// OpenRouter's reasoning passback for an assistant message: the documented
/// `reasoning_details` union, built from the protocol's opaque blocks.
///
/// A block goes out as `reasoning.text` (its text, plus `signature` when the
/// provider signed it) or `reasoning.encrypted` (its opaque `data`). Only
/// `type` and the payload are sent: OpenRouter's schema also carries `id`,
/// `index` and `format`, and live round-trips confirmed the API accepts a
/// block without them. The protocol's `ThinkingBlock` therefore needs no extra
/// fields (M24 decision 2, option (a)).
///
/// How much OpenRouter *checks* depends on the backend, and it is not much:
/// an Anthropic-signed block is verified (an altered `signature` is a 400 from
/// Bedrock and Google), while an OpenAI/Grok `reasoning.encrypted` blob is
/// accepted unchanged, with `id`/`format` stripped, and even corrupted
/// (measured on `openai/gpt-6-luna`: truncating the blob's tail and refilling
/// it with junk still returned 200). The plugin sends the payload byte-exact
/// regardless — a payload we cannot validate is one we must not alter, and
/// the provider is free to tighten its checks at any time.
///
/// A message whose blocks carry no payload at all sends no field: nothing to
/// pass back must look exactly like nothing.
fn openrouter_reasoning_passback(obj: &mut Map<String, Value>, blocks: &[ThinkingBlock]) {
    let details: Vec<Value> = blocks.iter().filter_map(reasoning_detail_wire).collect();
    if !details.is_empty() {
        obj.insert("reasoning_details".into(), Value::Array(details));
    }
}

/// One block on the `reasoning_details` wire, or `None` when the block
/// carries no payload (an empty text with no signature is not a detail).
fn reasoning_detail_wire(block: &ThinkingBlock) -> Option<Value> {
    match block.kind {
        ThinkingBlockKind::Thinking => {
            let text = block.text.as_deref().unwrap_or("");
            let signature = block.signature.as_deref().filter(|s| !s.is_empty());
            if text.is_empty() && signature.is_none() {
                return None;
            }
            let mut detail = json!({ "type": "reasoning.text", "text": text });
            if let Some(signature) = signature {
                detail["signature"] = json!(signature);
            }
            Some(detail)
        }
        ThinkingBlockKind::Redacted => {
            let data = block.data.as_deref().unwrap_or("");
            (!data.is_empty()).then(|| json!({ "type": "reasoning.encrypted", "data": data }))
        }
    }
}

/// One reasoning detail of a streamed delta, as much as this plugin needs.
/// `reasoning.summary` items carry a `summary` string; they are a display
/// summary of the reasoning, not the reasoning payload, so they stream as
/// thinking text but are never passed back as if they were `reasoning.text`.
#[derive(Debug, Default, serde::Deserialize)]
struct ReasoningDetail {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

/// A delta's reasoning fields (`reasoning` is the display text; the
/// structured form is `reasoning_details`).
#[derive(Debug, Default, serde::Deserialize)]
struct DeltaReasoning {
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_details: Option<Vec<ReasoningDetail>>,
}

/// Accumulates a round's reasoning so the visible text streams as it arrives
/// and the structured blocks are emitted once, in order, for passback.
#[derive(Debug, Default)]
struct ReasoningAccumulator {
    blocks: Vec<ReasoningBlock>,
}

#[derive(Debug, PartialEq)]
enum ReasoningBlock {
    /// Readable reasoning: text plus the signature when the provider signed
    /// it (Anthropic signs the whole trace, so the signature usually arrives
    /// on a later delta than the text it belongs to).
    Text {
        text: String,
        signature: Option<String>,
    },
    /// Encrypted reasoning (`reasoning.encrypted`): an opaque payload, whole
    /// in one detail and never mergeable with anything.
    Encrypted(String),
    /// A display summary of the reasoning. Shown, never passed back.
    Summary(String),
}

impl ReasoningAccumulator {
    /// Consume one choice delta: stream its reasoning text to the sink and
    /// accumulate the blocks. `reasoning_details` is authoritative when
    /// present (it is the documented field); the plain `reasoning` string is
    /// used only when a provider sends no details, so the same text is never
    /// shown twice.
    fn ingest(&mut self, delta: &Value, sink: &mut dyn FnMut(ChunkPayload)) -> Result<(), Error> {
        let delta: DeltaReasoning = serde_json::from_value(delta.clone())
            .map_err(|e| Error::Protocol(format!("malformed stream delta: {e}")))?;
        let Some(details) = delta.reasoning_details.filter(|d| !d.is_empty()) else {
            if let Some(text) = delta.reasoning.filter(|t| !t.is_empty()) {
                sink(ChunkPayload::Thinking { text });
            }
            return Ok(());
        };
        for detail in details {
            match detail.kind.as_str() {
                // Consecutive text details are deltas of one logical block:
                // OpenRouter streams them in pieces, and the signature may
                // arrive on a later piece than the text it signs.
                "reasoning.text" => {
                    let text = detail.text.unwrap_or_default();
                    let signature = detail.signature.filter(|s| !s.is_empty());
                    if !text.is_empty() {
                        sink(ChunkPayload::Thinking { text: text.clone() });
                    }
                    match self.blocks.last_mut() {
                        Some(ReasoningBlock::Text {
                            text: merged,
                            signature: merged_signature,
                        }) => {
                            merged.push_str(&text);
                            if signature.is_some() {
                                *merged_signature = signature;
                            }
                        }
                        _ => self.blocks.push(ReasoningBlock::Text { text, signature }),
                    }
                }
                "reasoning.summary" => {
                    let summary = detail.summary.unwrap_or_default();
                    if summary.is_empty() {
                        continue;
                    }
                    sink(ChunkPayload::Thinking {
                        text: summary.clone(),
                    });
                    match self.blocks.last_mut() {
                        Some(ReasoningBlock::Summary(merged)) => merged.push_str(&summary),
                        _ => self.blocks.push(ReasoningBlock::Summary(summary)),
                    }
                }
                "reasoning.encrypted" => {
                    // Whole payload in one detail; a repeat is a re-send, not
                    // an extension (appending would corrupt it).
                    if let Some(data) = detail.data.filter(|d| !d.is_empty())
                        && self.blocks.last() != Some(&ReasoningBlock::Encrypted(data.clone()))
                    {
                        self.blocks.push(ReasoningBlock::Encrypted(data));
                    }
                }
                // `reasoning.server_tool_call` and anything newer: no
                // protocol shape, and inventing one would forge a payload.
                _ => {}
            }
        }
        Ok(())
    }

    /// Emit the round's finished blocks, in generation order.
    fn emit_blocks(&self, sink: &mut dyn FnMut(ChunkPayload)) {
        for block in &self.blocks {
            match block {
                ReasoningBlock::Text { text, signature } => {
                    if text.is_empty() && signature.is_none() {
                        continue;
                    }
                    sink(ChunkPayload::ThinkingBlock {
                        block: ThinkingBlock::thinking(text.clone(), signature.clone()),
                    });
                }
                ReasoningBlock::Encrypted(data) => sink(ChunkPayload::ThinkingBlock {
                    block: ThinkingBlock::redacted(data.clone()),
                }),
                // A summary is not the reasoning payload.
                ReasoningBlock::Summary(_) => {}
            }
        }
    }
}

/// The `choices` array of a stream chunk (empty when absent or malformed).
fn choices(value: &Value) -> &[Value] {
    value
        .get("choices")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// A mid-stream `error` object → a request-scoped protocol error carrying the
/// provider's own text. The HTTP status is already gone (the response was a
/// 200), so the code comes from the error's own `code` field.
fn mid_stream_error(error: &Value) -> Error {
    let status = error
        .get("code")
        .and_then(Value::as_u64)
        .and_then(|code| u16::try_from(code).ok());
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("provider error during generation");
    let message = with_upstream_raw(message, error);
    // Mid-stream there is no `Retry-After` header to honor, so a 402 here is
    // the hard-stop reading: credits, not the transient in-flight budget.
    provider_error(status, message, "chat stream", None)
}

/// Map an HTTP-layer failure to a request-scoped protocol error. The message
/// is unwrapped from OpenRouter's envelope (`{"error":{"message":…}}`) so the
/// user sees the provider's own words rather than raw JSON.
fn backend_to_protocol_error(err: BackendError, context: &str) -> Error {
    let BackendError {
        status,
        message,
        retry_after_ms,
    } = err;
    let message = extract_error_message(message.trim());
    provider_error(status, message, context, retry_after_ms)
}

/// The shared status → code mapping, plus OpenRouter's `402` rule: retryable
/// (`rateLimit`, with the hint) only when a `Retry-After` came with it, which
/// is exactly the in-flight-budget case; otherwise it is the credits/key-limit
/// hard stop and reads as `auth`.
fn provider_error(
    status: Option<u16>,
    message: String,
    context: &str,
    retry_after_ms: Option<u64>,
) -> Error {
    let code = match status {
        Some(401 | 403) => ErrorCode::Auth,
        Some(402) if retry_after_ms.is_some() => ErrorCode::RateLimit,
        Some(402) => ErrorCode::Auth,
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
        // Spec §8: `retryAfterMs` rides on `rateLimit` and `backend`
        // errors — a 503/504 that carries a `Retry-After` is telling the
        // client exactly when to come back.
        retry_after_ms: if matches!(code, ErrorCode::RateLimit | ErrorCode::Backend) {
            retry_after_ms
        } else {
            None
        },
    }
}

/// Pull the human-readable message out of an OpenRouter error body. The
/// envelope is `{"error":{"message":…,"code":…,"metadata":{…}}}`; anything
/// else (or non-JSON) is passed through unchanged.
fn extract_error_message(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return body.to_string();
    };
    let Some(error) = value.get("error").filter(|e| !e.is_null()) else {
        return body.to_string();
    };
    let message = error.get("message").and_then(Value::as_str).unwrap_or(body);
    with_upstream_raw(message, error)
}

/// OpenRouter's `error.message` is sometimes generic ("Provider returned
/// error") with the real cause in `error.metadata.raw`. Append the raw text
/// when it adds something, so the user sees the upstream's own words.
fn with_upstream_raw(message: &str, error: &Value) -> String {
    let Some(raw) = error.pointer("/metadata/raw").and_then(Value::as_str) else {
        return message.to_string();
    };
    let raw = raw.trim();
    if raw.is_empty() || message.contains(raw) {
        message.to_string()
    } else {
        format!("{message}: {raw}")
    }
}

/// Parse the key-scoped catalog (`GET /api/v1/models/user`) into the picker's
/// model list.
///
/// The catalog curates the *list*, never the request (constraint 4):
///
/// - Entries not served by `/api/v1/chat/completions` are dropped. That is
///   every `:batch` entry (92 of 478 in the public catalog) plus anything
///   else the key cannot chat with. `~` rolling aliases and `:free` variants
///   stay: they are legitimate choices.
/// - A model whose `supported_parameters` lacks `tools` is flagged
///   `supportsTools: false` so the picker hides it — Clanky is agentic, so a
///   tool-less model is a dead end. An absent list means unknown, not "no".
/// - `-1` (dynamic routers) and `0` (`:free`) prices are sentinels, not
///   prices: the field is omitted and the cost display falls back to its
///   no-pricing path. `pricing.overrides` tiers are not representable in
///   `ModelInfo` v1, so the displayed price is the base tier.
fn parse_models(body: &str) -> Result<Vec<ModelInfo>, Error> {
    let root: Value = serde_json::from_str(body)
        .map_err(|e| Error::Protocol(format!("malformed models response: {e}")))?;
    let data = root
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::Protocol("models response has no `data` array".into()))?;
    Ok(data.iter().filter_map(model_from_entry).collect())
}

fn model_from_entry(item: &Value) -> Option<ModelInfo> {
    let id = item.get("id")?.as_str()?.to_string();
    // Only models the chat endpoint serves belong in an agent's picker.
    // `:batch` entries are the bulk of them and are named in the id.
    if id.ends_with(":batch") {
        return None;
    }
    if let Some(endpoints) = item.get("endpoints").and_then(Value::as_array) {
        let serves_chat = endpoints
            .iter()
            .filter_map(Value::as_str)
            .any(|endpoint| endpoint == CHAT_ENDPOINT);
        if !serves_chat {
            return None;
        }
    }
    let parameters = item.get("supported_parameters").and_then(Value::as_array);
    let supports_tools = parameters.map(|params| {
        params
            .iter()
            .filter_map(Value::as_str)
            .any(|p| p == "tools")
    });
    let output_modalities = item
        .pointer("/architecture/output_modalities")
        .and_then(Value::as_array);
    let supports_text_generation = output_modalities.map(|modalities| {
        modalities
            .iter()
            .filter_map(Value::as_str)
            .any(|m| m == "text")
    });
    // Reasoning models carry a `reasoning` object; dynamic routers and
    // non-reasoning models omit it, so an absent object is "unknown", not
    // "cannot reason".
    let supports_thinking = item
        .get("reasoning")
        .is_some_and(|reasoning| !reasoning.is_null())
        .then_some(true);
    let pricing = item.get("pricing");
    Some(ModelInfo {
        display_name: item
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_string),
        id,
        // Per model, not `top_provider.context_length` (which is per
        // endpoint and can be smaller).
        context_window: item.get("context_length").and_then(Value::as_u64),
        supports_thinking,
        supports_tools,
        supports_text_generation,
        input_price_per_mtok: price(pricing, "prompt"),
        output_price_per_mtok: price(pricing, "completion"),
        cache_read_price_per_mtok: price(pricing, "input_cache_read"),
    })
}

/// One `pricing` value: a dollars-per-token *string* in the catalog, scaled to
/// dollars per million tokens. Non-positive values are the documented
/// sentinels (`-1` dynamic routers, `0` `:free` variants) and yield `None`.
fn price(pricing: Option<&Value>, key: &str) -> Option<f64> {
    let raw = pricing?.get(key)?;
    let per_token = match raw {
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        Value::Number(number) => number.as_f64()?,
        _ => return None,
    };
    (per_token.is_finite() && per_token > 0.0).then_some(per_token * PER_MTOK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use clanky_protocol::{ChatMessage, FinishReason, Sampling, Tool, ToolCall, Usage};
    use std::cell::RefCell;

    const CATALOG: &str = include_str!("../tests/fixtures/models_user.json");
    const STREAM_PLAIN: &str = include_str!("../tests/fixtures/stream_plain.txt");
    const STREAM_REASONING: &str = include_str!("../tests/fixtures/stream_reasoning.txt");
    const STREAM_TOOLS: &str = include_str!("../tests/fixtures/stream_tools.txt");
    const STREAM_REASONING_SIGNED: &str =
        include_str!("../tests/fixtures/stream_reasoning_signed.txt");
    const STREAM_REASONING_SUMMARY: &str =
        include_str!("../tests/fixtures/stream_reasoning_summary.txt");
    const ERR_400: &str = include_str!("../tests/fixtures/err_400.txt");
    const ERR_401: &str = include_str!("../tests/fixtures/err_401.txt");
    const ERR_429: &str = include_str!("../tests/fixtures/err_429.txt");
    const ERR_402: &str = include_str!("../tests/fixtures/err_402.txt");
    const STREAM_USAGE: &str = include_str!("../tests/fixtures/stream_usage.txt");
    const STREAM_ERROR: &str = include_str!("../tests/fixtures/stream_error.txt");

    /// Records requests and replays canned responses.
    #[derive(Default)]
    struct MockBackend {
        posts: RefCell<Vec<(String, String)>>,
        gets: RefCell<Vec<String>>,
        catalog: String,
        sse: Vec<String>,
        post_error: Option<BackendError>,
    }

    impl MockBackend {
        fn new(catalog: &str, sse: Vec<String>) -> Self {
            Self {
                catalog: catalog.to_string(),
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
            self.gets.borrow_mut().push(url.to_string());
            Ok(self.catalog.clone())
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

    fn provider(sse: Vec<String>) -> OpenRouterProvider<MockBackend> {
        OpenRouterProvider::new(
            MockBackend::new(CATALOG, sse),
            "https://openrouter.ai/api/v1",
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
        provider: &mut OpenRouterProvider<MockBackend>,
        request: &ChatRequest,
    ) -> (Vec<ChunkPayload>, Result<ChatDone, Error>) {
        let mut chunks = Vec::new();
        let done = provider.chat(request, &mut |chunk| chunks.push(chunk));
        (chunks, done)
    }

    fn text_of(chunks: &[ChunkPayload]) -> String {
        chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn thinking_of(chunks: &[ChunkPayload]) -> String {
        chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Thinking { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn blocks_of(chunks: &[ChunkPayload]) -> Vec<ThinkingBlock> {
        chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::ThinkingBlock { block } => Some(block.clone()),
                _ => None,
            })
            .collect()
    }

    fn find<'a>(models: &'a [ModelInfo], id: &str) -> &'a ModelInfo {
        models
            .iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("no model {id}"))
    }

    // --- handshake -------------------------------------------------------

    #[test]
    fn handshake_reports_name_capabilities_and_default_model() {
        let provider = provider(Vec::new());
        let info = provider.info();
        assert_eq!(info.name, "openrouter");
        assert!(info.capabilities.list_models);
        assert!(info.capabilities.thinking);
        assert!(info.capabilities.tools);
        assert_eq!(
            info.default_model.as_deref(),
            Some("deepseek/deepseek-v4.1-flash")
        );
    }

    #[test]
    fn a_missing_key_is_an_auth_error_naming_the_env_var() {
        let error = missing_key_error();
        match error {
            Error::Provider {
                code,
                message,
                retryable,
                ..
            } => {
                assert_eq!(code, ErrorCode::Auth);
                assert!(!retryable);
                assert!(message.contains("OPENROUTER_API_KEY"), "{message}");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    // --- catalog ---------------------------------------------------------

    #[test]
    fn the_catalog_comes_from_the_key_scoped_endpoint() {
        let mut provider = provider(Vec::new());
        provider.list_models().unwrap();
        assert_eq!(
            provider.backend.gets.borrow().as_slice(),
            ["https://openrouter.ai/api/v1/models/user"]
        );
    }

    #[test]
    fn catalog_metadata_maps_onto_model_info() {
        let mut provider = provider(Vec::new());
        let models = provider.list_models().unwrap();

        let flash = find(&models, "deepseek/deepseek-v4-flash-0731");
        assert_eq!(
            flash.display_name.as_deref(),
            Some("DeepSeek: DeepSeek V4 Flash 0731")
        );
        // `context_length` (per model), not `top_provider.context_length`.
        assert_eq!(flash.context_window, Some(1_048_576));
        assert_eq!(flash.supports_tools, Some(true));
        assert_eq!(flash.supports_text_generation, Some(true));
        assert_eq!(flash.supports_thinking, Some(true));
        // `pricing.*` are per-token *strings*; ModelInfo is per Mtok.
        assert_eq!(flash.input_price_per_mtok, Some(0.0443));
        assert_eq!(flash.output_price_per_mtok, Some(0.3));
        assert_eq!(flash.cache_read_price_per_mtok, Some(0.035));
    }

    #[test]
    fn sentinel_prices_are_omitted_not_shown() {
        // `-1` is a dynamic router and `0` a `:free` variant: both mean "no
        // fixed price", so the fields stay absent and the cost display falls
        // back to its no-pricing path instead of printing "$-1.00 /Mtok".
        let mut provider = provider(Vec::new());
        let models = provider.list_models().unwrap();

        let router = find(&models, "openrouter/auto");
        assert_eq!(router.input_price_per_mtok, None);
        assert_eq!(router.output_price_per_mtok, None);
        assert_eq!(router.cache_read_price_per_mtok, None);

        let free = find(&models, "inclusionai/ling-3.2-flash:free");
        assert_eq!(free.input_price_per_mtok, None);
        assert_eq!(free.output_price_per_mtok, None);

        // And nothing in the whole captured catalog shows a non-positive
        // price: the sentinels never leak into the picker.
        for model in &models {
            for price in [
                model.input_price_per_mtok,
                model.output_price_per_mtok,
                model.cache_read_price_per_mtok,
            ]
            .into_iter()
            .flatten()
            {
                assert!(price > 0.0, "{} shows a sentinel price {price}", model.id);
            }
        }
    }

    #[test]
    fn batch_entries_are_dropped_and_aliases_and_free_variants_are_kept() {
        // `:batch` entries are not served by `/api/v1/chat/completions` at
        // all, so they cannot drive an agent loop. `~` rolling aliases and
        // `:free` variants are legitimate choices and stay.
        let catalog = json!({
            "data": [
                {"id": "vendor/model", "endpoints": ["/api/v1/chat/completions"]},
                {"id": "vendor/model:batch", "endpoints": ["/api/v1/chat/completions"]},
                {"id": "~vendor/model-latest", "endpoints": ["/api/v1/chat/completions"]},
                {"id": "vendor/model:free", "endpoints": ["/api/v1/chat/completions"]},
                {"id": "vendor/embeddings", "endpoints": ["/api/v1/embeddings"]},
                {"id": "vendor/no-endpoints"}
            ]
        })
        .to_string();
        let mut provider = OpenRouterProvider::new(
            MockBackend::new(&catalog, Vec::new()),
            "https://openrouter.ai/api/v1",
        );
        let ids: Vec<String> = provider
            .list_models()
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(
            ids,
            [
                "vendor/model",
                "~vendor/model-latest",
                "vendor/model:free",
                // No `endpoints` array at all means "unknown", so the entry
                // is kept (the picker's job is curating, not guessing).
                "vendor/no-endpoints",
            ]
        );
    }

    #[test]
    fn tool_and_thinking_support_come_from_the_catalog_hints() {
        let catalog = json!({
            "data": [
                {
                    "id": "vendor/chatty",
                    "endpoints": ["/api/v1/chat/completions"],
                    "supported_parameters": ["tools", "reasoning"],
                    "architecture": {"output_modalities": ["text"]},
                    "reasoning": {"mandatory": true, "supported_efforts": ["high"]}
                },
                {
                    "id": "vendor/plain",
                    "endpoints": ["/api/v1/chat/completions"],
                    "supported_parameters": ["temperature"],
                    "architecture": {"output_modalities": ["text"]}
                },
                {
                    "id": "vendor/images",
                    "endpoints": ["/api/v1/chat/completions"],
                    "supported_parameters": ["tools"],
                    "architecture": {"output_modalities": ["image"]}
                }
            ]
        })
        .to_string();
        let mut provider = OpenRouterProvider::new(
            MockBackend::new(&catalog, Vec::new()),
            "https://openrouter.ai/api/v1",
        );
        let models = provider.list_models().unwrap();
        let chatty = find(&models, "vendor/chatty");
        assert_eq!(chatty.supports_tools, Some(true));
        assert_eq!(chatty.supports_thinking, Some(true));
        assert_eq!(chatty.supports_text_generation, Some(true));

        // No `tools` in `supported_parameters` is a definite "no": the
        // picker hides it. No `reasoning` object is "unknown", not "no".
        let plain = find(&models, "vendor/plain");
        assert_eq!(plain.supports_tools, Some(false));
        assert_eq!(plain.supports_thinking, None);

        // An image-only model cannot generate text for the loop.
        assert_eq!(
            find(&models, "vendor/images").supports_text_generation,
            Some(false)
        );
    }

    #[test]
    fn a_malformed_catalog_is_a_protocol_error() {
        let mut provider = OpenRouterProvider::new(
            MockBackend::new("not json", Vec::new()),
            "https://openrouter.ai/api/v1",
        );
        assert!(matches!(
            provider.list_models(),
            Err(Error::Protocol(message)) if message.contains("malformed models response")
        ));

        let mut provider = OpenRouterProvider::new(
            MockBackend::new("{}", Vec::new()),
            "https://openrouter.ai/api/v1",
        );
        assert!(matches!(
            provider.list_models(),
            Err(Error::Protocol(message)) if message.contains("no `data` array")
        ));
    }

    // --- streaming -------------------------------------------------------

    #[test]
    fn plain_stream_becomes_text_chunks_with_usage() {
        // Captured with `reasoning.effort: "none"`: no reasoning at all, and
        // the final chunk repeats `finish_reason: "stop"` alongside the
        // usage (OpenRouter's documented deviation from OpenAI's empty
        // choices). The accumulator must not mind the repeat.
        let mut provider = provider(payloads(STREAM_PLAIN));
        let (chunks, done) = run_chat(&mut provider, &request("deepseek/deepseek-v4-flash-0731"));

        assert_eq!(text_of(&chunks), "Hi there! How can I help you today?");
        assert_eq!(thinking_of(&chunks), "");
        let done = done.unwrap();
        assert_eq!(done.finish_reason, FinishReason::Stop);
        assert_eq!(
            done.usage,
            Some(Usage {
                prompt_tokens: Some(6),
                completion_tokens: Some(11),
                cached_tokens: Some(0),
            })
        );
    }

    #[test]
    fn the_usage_chunk_arrives_without_stream_options_in_the_request() {
        // M24 decision 6: `stream_options.include_usage` is deprecated and
        // ignored on OpenRouter, so it is not sent — and the usage still
        // arrives. Both halves are pinned here.
        let mut provider = provider(payloads(STREAM_PLAIN));
        run_chat(&mut provider, &request("deepseek/deepseek-v4-flash-0731"))
            .1
            .unwrap();
        let body = provider.backend.last_body();
        assert!(
            body.get("stream_options").is_none(),
            "stream_options must not be sent: {body}"
        );
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn reasoning_stream_separates_thinking_from_text_and_emits_blocks() {
        let mut provider = provider(payloads(STREAM_REASONING));
        let (chunks, done) = run_chat(&mut provider, &request("deepseek/deepseek-v4-flash-0731"));

        // Reasoning is its own stream: it never leaks into the visible text.
        let thinking = thinking_of(&chunks);
        assert!(thinking.starts_with("We need answer user"), "{thinking}");
        assert_eq!(text_of(&chunks), "2 + 2 = 4.");

        // The round's finished blocks are emitted once, after the stream, so
        // the next request can pass them back. OpenRouter streams the text in
        // many deltas; they are one logical block.
        let blocks = blocks_of(&chunks);
        assert_eq!(blocks.len(), 1, "one logical reasoning block: {blocks:?}");
        assert_eq!(blocks[0].kind, ThinkingBlockKind::Thinking);
        assert_eq!(blocks[0].text.as_deref(), Some(thinking.as_str()));
        assert_eq!(blocks[0].signature, None, "this model signs nothing");
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);
    }

    #[test]
    fn a_signed_reasoning_trace_keeps_its_signature_on_the_block() {
        // Anthropic-backed models sign the trace, and the signature arrives
        // on a later delta than the text it signs (captured live: five text
        // deltas, then one signature-only detail). The merged block must
        // carry both, and the passback must be byte-exact.
        let signature = "sig-abc123";
        let sse = vec![
            json!({"choices": [{"delta": {"reasoning": "think ", "reasoning_details": [
                {"type": "reasoning.text", "text": "think ", "format": "anthropic-claude-v1", "index": 0}
            ]}}]})
            .to_string(),
            json!({"choices": [{"delta": {"reasoning": "more", "reasoning_details": [
                {"type": "reasoning.text", "text": "more", "format": "anthropic-claude-v1", "index": 0}
            ]}}]})
            .to_string(),
            json!({"choices": [{"delta": {"reasoning_details": [
                {"type": "reasoning.text", "signature": signature, "format": "anthropic-claude-v1", "index": 0}
            ]}}]})
            .to_string(),
            json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]}).to_string(),
            "[DONE]".to_string(),
        ];
        let mut provider = provider(sse);
        let (chunks, _) = run_chat(&mut provider, &request("anthropic/claude-haiku-4.5"));

        let blocks = blocks_of(&chunks);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text.as_deref(), Some("think more"));
        assert_eq!(blocks[0].signature.as_deref(), Some(signature));

        // ... and back out on the next request as `reasoning_details`.
        let with_blocks = ChatRequest {
            messages: vec![
                ChatMessage::user("hi"),
                ChatMessage::Assistant {
                    content: "hello".into(),
                    tool_calls: None,
                    thinking: Some(blocks),
                },
            ],
            ..request("anthropic/claude-haiku-4.5")
        };
        let body: Value =
            serde_json::from_str(&provider.build_request_body(&with_blocks).unwrap()).unwrap();
        assert_eq!(
            body["messages"][1]["reasoning_details"],
            json!([{"type": "reasoning.text", "text": "think more", "signature": signature}]),
            "the block goes back byte-exact, in the documented shape"
        );
    }

    #[test]
    fn encrypted_reasoning_round_trips_as_its_own_opaque_block() {
        let sse = vec![
            json!({"choices": [{"delta": {"reasoning_details": [
                {"type": "reasoning.encrypted", "data": "opaque-blob", "index": 0}
            ]}}]})
            .to_string(),
            json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]}).to_string(),
            "[DONE]".to_string(),
        ];
        let mut provider = provider(sse);
        let (chunks, _) = run_chat(&mut provider, &request("vendor/model"));
        let blocks = blocks_of(&chunks);
        assert_eq!(
            blocks,
            vec![ThinkingBlock::redacted("opaque-blob")],
            "encrypted payloads are whole, never concatenated with anything"
        );

        let with_blocks = ChatRequest {
            messages: vec![
                ChatMessage::user("hi"),
                ChatMessage::Assistant {
                    content: "hello".into(),
                    tool_calls: None,
                    thinking: Some(blocks),
                },
            ],
            ..request("vendor/model")
        };
        let body: Value =
            serde_json::from_str(&provider.build_request_body(&with_blocks).unwrap()).unwrap();
        assert_eq!(
            body["messages"][1]["reasoning_details"],
            json!([{"type": "reasoning.encrypted", "data": "opaque-blob"}])
        );
    }

    #[test]
    fn reasoning_summaries_stream_but_are_not_passed_back() {
        // `reasoning.summary` is a display summary, not the reasoning
        // payload: showing it is right, sending it back as `reasoning.text`
        // would forge a payload the provider never produced.
        let sse = vec![
            json!({"choices": [{"delta": {"reasoning_details": [
                {"type": "reasoning.summary", "summary": "considered options"}
            ]}}]})
            .to_string(),
            json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]}).to_string(),
            "[DONE]".to_string(),
        ];
        let mut provider = provider(sse);
        let (chunks, _) = run_chat(&mut provider, &request("vendor/model"));
        assert_eq!(thinking_of(&chunks), "considered options");
        assert!(blocks_of(&chunks).is_empty(), "summaries are not passback");
    }

    #[test]
    fn plain_reasoning_string_streams_when_no_details_are_sent() {
        // Some routes send only the unstructured `reasoning` string. It is
        // display text; with no `reasoning_details` there is no passback
        // payload, and the text must not be shown twice.
        let sse = vec![
            json!({"choices": [{"delta": {"reasoning": "hmm"}}]}).to_string(),
            json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]}).to_string(),
            "[DONE]".to_string(),
        ];
        let mut provider = provider(sse);
        let (chunks, _) = run_chat(&mut provider, &request("vendor/model"));
        assert_eq!(thinking_of(&chunks), "hmm");
        assert!(blocks_of(&chunks).is_empty());
    }

    #[test]
    fn tool_stream_becomes_start_and_args_chunks() {
        let mut provider = provider(payloads(STREAM_TOOLS));
        let (chunks, done) = run_chat(&mut provider, &request("deepseek/deepseek-v4-flash-0731"));

        let starts: Vec<&ChunkPayload> = chunks
            .iter()
            .filter(|c| matches!(c, ChunkPayload::ToolCallStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1, "one tool call, announced once");
        match starts[0] {
            ChunkPayload::ToolCallStart { index, id, name } => {
                assert_eq!(*index, 0);
                assert_eq!(id, "chatcmpl-tool-bc243211a2457662");
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
            json!({"command": "ls -la /tmp"})
        );
        assert_eq!(done.unwrap().finish_reason, FinishReason::ToolCalls);

        // The reasoning that led to the call is still shown and passed back.
        assert!(!thinking_of(&chunks).is_empty());
        assert_eq!(blocks_of(&chunks).len(), 1);
    }

    // --- mid-stream errors (a 200 is not a success) ----------------------

    #[test]
    fn a_mid_stream_error_chunk_becomes_an_error_not_an_empty_answer() {
        // The documented shape: HTTP 200, a top-level `error` object, and a
        // choice with `finish_reason: "error"`. Turning this into a `done`
        // would report a provider failure as a successful empty answer.
        let sse = vec![
            json!({
                "id": "gen-1", "object": "chat.completion.chunk",
                "error": {"code": 502, "message": "Provider disconnected unexpectedly"},
                "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "error"}]
            })
            .to_string(),
            "[DONE]".to_string(),
        ];
        let mut provider = provider(sse);
        let (chunks, done) = run_chat(&mut provider, &request("vendor/model"));
        assert!(chunks.is_empty());
        match done {
            Err(Error::Provider {
                code,
                message,
                retryable,
                ..
            }) => {
                assert_eq!(code, ErrorCode::Backend);
                assert!(retryable, "a provider disconnect is transient");
                assert!(message.contains("502"), "{message}");
                assert!(message.contains("Provider disconnected"), "{message}");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn a_finish_reason_of_error_without_an_error_object_still_fails() {
        let sse = vec![
            json!({"choices": [{"delta": {"content": "partial"}, "finish_reason": "error"}]})
                .to_string(),
            "[DONE]".to_string(),
        ];
        let mut provider = provider(sse);
        let (chunks, done) = run_chat(&mut provider, &request("vendor/model"));
        // What streamed before the failure is still delivered — the turn's
        // partial output is real.
        assert_eq!(text_of(&chunks), "partial");
        assert!(
            matches!(
                done,
                Err(Error::Provider {
                    code: ErrorCode::Backend,
                    ..
                })
            ),
            "finish_reason \"error\" must never become a successful stop"
        );
    }

    #[test]
    fn an_error_before_any_token_fails_the_turn() {
        // The docs are explicit that the error can be the first and only
        // event, so a 200 carrying only an error is a failure, not an empty
        // success.
        let sse = vec![
            json!({
                "error": {"code": 429, "message": "Rate limit exceeded",
                          "metadata": {"error_type": "rate_limit_exceeded"}},
                "choices": []
            })
            .to_string(),
        ];
        let mut provider = provider(sse);
        let (_, done) = run_chat(&mut provider, &request("vendor/model"));
        match done {
            Err(Error::Provider { code, message, .. }) => {
                assert_eq!(code, ErrorCode::RateLimit);
                assert!(message.contains("Rate limit exceeded"), "{message}");
            }
            other => panic!("expected a rate-limit error, got {other:?}"),
        }
    }

    #[test]
    fn a_stream_that_stops_without_a_finish_reason_is_not_a_success() {
        let sse = vec![json!({"choices": [{"delta": {"content": "cut off"}}]}).to_string()];
        let mut provider = provider(sse);
        let (chunks, done) = run_chat(&mut provider, &request("vendor/model"));
        assert_eq!(text_of(&chunks), "cut off");
        match done {
            Err(Error::Provider {
                code,
                message,
                retryable,
                ..
            }) => {
                assert_eq!(code, ErrorCode::Backend);
                assert!(retryable);
                assert!(message.contains("cut off"), "{message}");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    // --- request building ------------------------------------------------

    #[test]
    fn request_body_is_openai_shaped_and_streaming() {
        let mut provider = provider(vec![
            r#"{"choices":[{"finish_reason":"stop","delta":{}}]}"#.into(),
        ]);
        let mut req = request("deepseek/deepseek-v4-flash-0731");
        req.sampling = Some(Sampling::from_iter([
            ("temperature".into(), json!(0.5)),
            ("top_p".into(), json!(0.9)),
            ("max_tokens".into(), json!(256)),
        ]));
        req.tools = Some(vec![Tool {
            name: "bash".into(),
            description: Some("Run a shell command".into()),
            parameters: Some(json!({"type": "object"})),
        }]);
        run_chat(&mut provider, &req).1.unwrap();

        let body = provider.backend.last_body();
        assert_eq!(body["model"], "deepseek/deepseek-v4-flash-0731");
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["top_p"], 0.9);
        assert_eq!(body["max_tokens"], 256);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "bash");
        assert_eq!(
            provider.backend.posts.borrow()[0].0,
            "https://openrouter.ai/api/v1/chat/completions"
        );
    }

    #[test]
    fn thinking_maps_onto_the_reasoning_effort_ladder() {
        // `--thinking <budget>` keeps its meaning, translated onto
        // OpenRouter's documented effort rungs (fractions of the output
        // limit; the translation is lossy and README says so).
        for (budget, effort) in [
            (0, "none"),
            (100, "minimal"),
            (1000, "low"),
            (4000, "medium"),
            (20000, "xhigh"),
            (70000, "max"),
        ] {
            let provider = provider(Vec::new());
            let req = ChatRequest {
                thinking: Some(Thinking {
                    budget_tokens: Some(budget),
                }),
                ..request("vendor/model")
            };
            let body: Value =
                serde_json::from_str(&provider.build_request_body(&req).unwrap()).unwrap();
            assert_eq!(
                body["reasoning"],
                json!({"effort": effort}),
                "budget {budget}"
            );
            // `reasoning_content` is undocumented here and is never sent.
            assert!(body.get("reasoning_content").is_none());
            assert!(body.get("reasoning_effort").is_none());
        }
    }

    #[test]
    fn no_thinking_means_no_reasoning_field() {
        let provider = provider(Vec::new());
        let body: Value = serde_json::from_str(
            &provider
                .build_request_body(&request("vendor/model"))
                .unwrap(),
        )
        .unwrap();
        assert!(
            body.get("reasoning").is_none(),
            "unset thinking lets OpenRouter apply the model default"
        );
    }

    #[test]
    fn a_sampling_reasoning_object_wins_over_the_derived_one() {
        // User values are authoritative: a `reasoning` (or the
        // `reasoning_effort` shorthand) set through `[sampling]` is never
        // overwritten by the derived ladder.
        let provider = provider(Vec::new());
        for sampling in [
            json!({"reasoning": {"effort": "low", "exclude": true}}),
            json!({"reasoning_effort": "low"}),
        ] {
            let req = ChatRequest {
                sampling: Some(Sampling::from_iter(sampling.as_object().unwrap().clone())),
                thinking: Some(Thinking {
                    budget_tokens: Some(64000),
                }),
                ..request("vendor/model")
            };
            let body: Value =
                serde_json::from_str(&provider.build_request_body(&req).unwrap()).unwrap();
            assert_eq!(
                body.get("reasoning").cloned().unwrap_or(Value::Null),
                sampling.get("reasoning").cloned().unwrap_or(Value::Null),
                "the user's reasoning object must pass through untouched"
            );
        }
    }

    #[test]
    fn assistant_tool_calls_and_tool_results_round_trip() {
        let mut provider = provider(vec![
            r#"{"choices":[{"finish_reason":"stop","delta":{}}]}"#.into(),
        ]);
        let req = ChatRequest {
            model: "vendor/model".into(),
            messages: vec![
                ChatMessage::user("list /tmp"),
                ChatMessage::Assistant {
                    content: String::new(),
                    thinking: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "call_1".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "ls /tmp"}),
                    }]),
                },
                ChatMessage::tool(
                    "call_1", "a
b",
                ),
            ],
            tools: None,
            sampling: None,
            thinking: None,
        };
        run_chat(&mut provider, &req).1.unwrap();

        let body = provider.backend.last_body();
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"command\":\"ls /tmp\"}"
        );
        assert_eq!(body["messages"][2]["role"], "tool");
        assert_eq!(body["messages"][2]["tool_call_id"], "call_1");
        assert_eq!(
            body["messages"][2]["content"],
            "a
b"
        );
    }

    #[test]
    fn a_blockless_assistant_message_sends_no_reasoning_field() {
        let provider = provider(Vec::new());
        let req = ChatRequest {
            messages: vec![ChatMessage::user("hi"), ChatMessage::assistant("answer")],
            ..request("vendor/model")
        };
        let body: Value =
            serde_json::from_str(&provider.build_request_body(&req).unwrap()).unwrap();
        assert!(body["messages"][1].get("reasoning_details").is_none());
        assert!(body["messages"][1].get("reasoning_content").is_none());
    }

    #[test]
    fn a_block_with_no_payload_is_not_sent() {
        // An empty text with no signature is not a detail; forging one would
        // put a payload on the wire the provider never produced.
        let provider = provider(Vec::new());
        let req = ChatRequest {
            messages: vec![
                ChatMessage::user("hi"),
                ChatMessage::Assistant {
                    content: "answer".into(),
                    tool_calls: None,
                    thinking: Some(vec![ThinkingBlock::thinking("", None)]),
                },
            ],
            ..request("vendor/model")
        };
        let body: Value =
            serde_json::from_str(&provider.build_request_body(&req).unwrap()).unwrap();
        assert!(body["messages"][1].get("reasoning_details").is_none());
    }

    // --- captured error envelopes ---------------------------------------

    /// Split a captured `curl -i` response into its status line's code and
    /// body. Headers arrive CRLF-terminated, so normalize line endings first.
    fn captured_error(fixture: &str) -> (u16, String) {
        let normalized = fixture.replace("\r\n", "\n");
        let (head, body) = normalized
            .split_once("\n\n")
            .expect("a captured response has headers and a body");
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        (status, body.trim().to_string())
    }

    /// Every `HTTP/…`-delimited response in a capture file, as
    /// `(status, retry-after in ms, body)`. The file interleaves comment
    /// blocks with responses, so records are found by their status line
    /// rather than by blank lines.
    fn captured_responses(fixture: &str) -> Vec<(u16, Option<u64>, String)> {
        let mut responses = Vec::new();
        let mut current: Option<(u16, Option<u64>, String)> = None;
        let mut in_body = false;
        for line in fixture.replace("\r\n", "\n").lines() {
            if line.starts_with("HTTP/") {
                if let Some(done) = current.take() {
                    responses.push(done);
                }
                let status = line
                    .split_whitespace()
                    .nth(1)
                    .and_then(|code| code.parse().ok())
                    .expect("a status code");
                current = Some((status, None, String::new()));
                in_body = false;
            } else if let Some((_, retry_after, body)) = current.as_mut() {
                if line.trim().is_empty() {
                    in_body = true;
                } else if in_body {
                    body.push_str(line);
                } else if let Some(value) = line.to_ascii_lowercase().strip_prefix("retry-after:") {
                    *retry_after = value.trim().parse::<u64>().ok().map(|s| s * 1000);
                }
            }
        }
        responses.extend(current);
        responses
    }

    #[test]
    fn captured_error_envelopes_map_onto_actionable_errors() {
        // err_400: an unknown model id. The provider's own sentence reaches
        // the user, and the raw envelope does not.
        let (status, body) = captured_error(ERR_400);
        assert_eq!(status, 400);
        match backend_to_protocol_error(BackendError::new(Some(status), body), "chat completion") {
            Error::Provider {
                code,
                message,
                retryable,
                ..
            } => {
                assert_eq!(code, ErrorCode::InvalidRequest);
                assert!(!retryable);
                assert!(message.contains("is not a valid model ID"), "{message}");
                assert!(
                    !message.contains("user_id"),
                    "raw envelope leaked: {message}"
                );
            }
            other => panic!("expected a provider error, got {other:?}"),
        }

        // err_401: a bad key. `auth`, never retried — retrying cannot help.
        let (status, body) = captured_error(ERR_401);
        assert_eq!(status, 401);
        match backend_to_protocol_error(BackendError::new(Some(status), body), "chat completion") {
            Error::Provider {
                code,
                message,
                retryable,
                ..
            } => {
                assert_eq!(code, ErrorCode::Auth);
                assert!(!retryable);
                assert!(message.contains("User not found"), "{message}");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }

        // err_429: an upstream rate limit whose `message` is generic
        // ("Provider returned error") and whose real cause is in
        // `error.metadata.raw`. Both must reach the user.
        let (status, body) = captured_error(ERR_429);
        assert_eq!(status, 429);
        match backend_to_protocol_error(BackendError::new(Some(status), body), "chat completion") {
            Error::Provider {
                code,
                message,
                retryable,
                ..
            } => {
                assert_eq!(code, ErrorCode::RateLimit);
                assert!(retryable);
                assert!(message.contains("Provider returned error"), "{message}");
                assert!(
                    message.contains("temporarily rate-limited upstream"),
                    "the actionable upstream text must survive: {message}"
                );
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn the_captured_signed_reasoning_stream_merges_into_one_signed_block() {
        // Anthropic via OpenRouter: five text deltas, then a signature-only
        // detail, then the visible answer. The block must carry the joined
        // text *and* the signature — that pair is what the provider verifies
        // on the way back.
        let mut provider = provider(payloads(STREAM_REASONING_SIGNED));
        let (chunks, done) = run_chat(&mut provider, &request("anthropic/claude-haiku-4.5"));

        assert_eq!(
            thinking_of(&chunks),
            "The user just said \"hi\" - this is a simple greeting. I should respond warmly and helpfully."
        );
        assert_eq!(text_of(&chunks), "Hello! How can I help you today?");
        let blocks = blocks_of(&chunks);
        assert_eq!(blocks.len(), 1, "one signed block: {blocks:?}");
        assert_eq!(blocks[0].kind, ThinkingBlockKind::Thinking);
        let signature = blocks[0].signature.as_deref().expect("signed trace");
        assert!(signature.starts_with("EtoCCpwB"), "{signature}");
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);
    }

    #[test]
    fn a_summary_and_encrypted_stream_shows_the_summary_and_passes_back_only_the_blob() {
        // Captured live from `openai/gpt-6-luna` (Azure): OpenAI's reasoning
        // models return a *summary* of their reasoning as `reasoning.summary`
        // deltas, and the actual reasoning as one opaque `reasoning.encrypted`
        // blob at the end. Same shape on `x-ai/grok-4.7`.
        //
        // Two things this pins: the summary is what the user sees (there is no
        // readable `reasoning.text` at all), and the passback carries the
        // blob — not the summary dressed up as reasoning.
        let mut provider = provider(payloads(STREAM_REASONING_SUMMARY));
        let (chunks, done) = run_chat(&mut provider, &request("openai/gpt-6-luna"));

        let shown = thinking_of(&chunks);
        assert!(!shown.is_empty(), "the summary is the visible reasoning");
        assert!(shown.contains("Demonstrating a simple answer"), "{shown}");
        assert_eq!(text_of(&chunks), "2 + 2 = 4.");

        let blocks = blocks_of(&chunks);
        assert_eq!(blocks.len(), 1, "one encrypted block, no forged text block");
        assert_eq!(blocks[0].kind, ThinkingBlockKind::Redacted);
        assert!(
            blocks[0].data.as_deref().is_some_and(|d| d.len() > 100),
            "the opaque blob is the payload"
        );
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);

        // On the wire it goes back as `reasoning.encrypted` and nothing else:
        // the summary never masquerades as `reasoning.text`.
        let with_blocks = ChatRequest {
            messages: vec![
                ChatMessage::user("hi"),
                ChatMessage::Assistant {
                    content: "hello".into(),
                    tool_calls: None,
                    thinking: Some(blocks.clone()),
                },
            ],
            ..request("openai/gpt-6-luna")
        };
        let body: Value =
            serde_json::from_str(&provider.build_request_body(&with_blocks).unwrap()).unwrap();
        let details = body["messages"][1]["reasoning_details"]
            .as_array()
            .expect("a reasoning_details array");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["type"], "reasoning.encrypted");
        assert_eq!(details[0]["data"], blocks[0].data.clone().unwrap());
    }

    #[test]
    fn the_captured_usage_chunk_repeats_the_finish_reason_and_still_counts() {
        // OpenRouter's terminal usage chunk deviates from OpenAI's: it carries
        // one choice whose content-free delta *repeats* `finish_reason`
        // instead of an empty `choices` array. A strict "empty choices means
        // usage frame" parser would drop it; the shared accumulator ignores
        // the content-free delta and keeps the usage.
        let mut provider = provider(payloads(STREAM_USAGE));
        let (chunks, done) = run_chat(&mut provider, &request("deepseek/deepseek-v4-flash-0731"));
        assert!(chunks.is_empty(), "no content in the tail: {chunks:?}");
        let done = done.unwrap();
        assert_eq!(done.finish_reason, FinishReason::Stop);
        assert_eq!(
            done.usage,
            Some(Usage {
                prompt_tokens: Some(6),
                completion_tokens: Some(11),
                cached_tokens: Some(0),
            }),
            "the usage in the duplicated-finish-reason chunk must not be lost"
        );
    }

    #[test]
    fn the_pinned_mid_stream_error_chunk_fails_the_turn() {
        // Same fixture the docs specify (see the file's own comment on why it
        // is not a live capture). Pinned so a refactor that starts mapping
        // `finish_reason: "error"` to `stop` fails a test instead of quietly
        // turning a provider failure into an empty successful answer.
        let mut provider = provider(payloads(STREAM_ERROR));
        let (_, done) = run_chat(&mut provider, &request("deepseek/deepseek-v4-flash-0731"));
        match done {
            Err(Error::Provider {
                code,
                message,
                retryable,
                ..
            }) => {
                assert_eq!(code, ErrorCode::Backend);
                assert!(retryable);
                assert!(message.contains("502"), "{message}");
                assert!(message.contains("Provider disconnected"), "{message}");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn the_two_documented_402s_map_differently() {
        // The fixture holds both documented bodies, each with its own headers.
        // The `Retry-After` header is the only signal that distinguishes
        // them, so it is what the mapping keys on.
        let responses = captured_responses(ERR_402);
        assert_eq!(responses.len(), 2, "both documented 402s are pinned");

        // Credits exhausted: no hint, a hard stop.
        let (_, retry_after, body) = &responses[0];
        assert_eq!(*retry_after, None);
        match backend_to_protocol_error(
            BackendError::new(Some(402), body.clone()),
            "chat completion",
        ) {
            Error::Provider {
                code,
                message,
                retryable,
                retry_after_ms,
            } => {
                assert_eq!(
                    code,
                    ErrorCode::Auth,
                    "credits exhausted reads as a hard stop"
                );
                assert!(!retryable);
                assert_eq!(retry_after_ms, None);
                assert!(message.contains("requires more credits"), "{message}");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }

        // In-flight budget: a hint, transient.
        let (_, retry_after, body) = &responses[1];
        assert_eq!(*retry_after, Some(12_000));
        let error = backend_to_protocol_error(
            BackendError::new(Some(402), body.clone()).with_retry_after_header("12", 0),
            "chat completion",
        );
        match error {
            Error::Provider {
                code,
                retryable,
                retry_after_ms,
                ..
            } => {
                assert_eq!(code, ErrorCode::RateLimit);
                assert!(retryable);
                assert_eq!(retry_after_ms, Some(12_000));
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    // --- error mapping ---------------------------------------------------

    fn error_for(status: Option<u16>, retry_after_ms: Option<u64>) -> Error {
        provider_error(
            status,
            "the provider's own words".into(),
            "chat completion",
            retry_after_ms,
        )
    }

    #[test]
    fn http_statuses_map_onto_error_codes() {
        for (status, code) in [
            (401, ErrorCode::Auth),
            (403, ErrorCode::Auth),
            (429, ErrorCode::RateLimit),
            (400, ErrorCode::InvalidRequest),
            (404, ErrorCode::InvalidRequest),
            (422, ErrorCode::InvalidRequest),
            (502, ErrorCode::Backend),
            (503, ErrorCode::Backend),
        ] {
            let error = error_for(Some(status), None);
            match error {
                Error::Provider {
                    code: got,
                    message,
                    retryable,
                    ..
                } => {
                    assert_eq!(got, code, "status {status}");
                    assert!(message.contains("the provider's own words"), "{message}");
                    assert_eq!(
                        retryable,
                        matches!(code, ErrorCode::RateLimit | ErrorCode::Backend),
                        "status {status}"
                    );
                }
                other => panic!("expected a provider error, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_402_without_retry_after_is_a_credits_problem_and_is_not_retried() {
        // Credits exhausted / key limit exhausted: no retry fixes it, and the
        // docs are explicit that no SDK retries a 402 on its own.
        match error_for(Some(402), None) {
            Error::Provider {
                code,
                retryable,
                retry_after_ms,
                ..
            } => {
                assert_eq!(code, ErrorCode::Auth);
                assert!(!retryable);
                assert_eq!(retry_after_ms, None);
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn a_402_with_retry_after_is_the_transient_in_flight_budget() {
        // The in-flight spending budget is the only 402 that carries
        // `Retry-After`, and it is worth waiting for.
        match error_for(Some(402), Some(12_000)) {
            Error::Provider {
                code,
                retryable,
                retry_after_ms,
                ..
            } => {
                assert_eq!(code, ErrorCode::RateLimit);
                assert!(retryable);
                assert_eq!(retry_after_ms, Some(12_000));
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn the_retry_after_hint_rides_on_rate_limit_and_backend_errors() {
        // Spec §8: `retryAfterMs` is meaningful on `rateLimit` and
        // `backend` — a 503 that carries `Retry-After` is telling the
        // client exactly when to come back.
        match error_for(Some(503), Some(5_000)) {
            Error::Provider {
                code,
                retryable,
                retry_after_ms,
                ..
            } => {
                assert_eq!(code, ErrorCode::Backend);
                assert!(retryable);
                assert_eq!(retry_after_ms, Some(5_000));
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
        // No hint on the codes that cannot be retried anyway.
        match error_for(Some(400), Some(5_000)) {
            Error::Provider { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, None);
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn the_error_envelope_is_unwrapped_to_the_providers_own_message() {
        // A captured 400: `{"error":{"message":…,"code":…},"user_id":…}`.
        let error = backend_to_protocol_error(
            BackendError::new(
                Some(400),
                r#"{"error":{"message":"no-such-model is not a valid model ID","code":400},"user_id":"user_1"}"#,
            ),
            "chat completion",
        );
        match error {
            Error::Provider { code, message, .. } => {
                assert_eq!(code, ErrorCode::InvalidRequest);
                assert!(message.contains("400: "), "{message}");
                assert!(
                    message.contains("no-such-model is not a valid model ID"),
                    "{message}"
                );
                assert!(
                    !message.contains("user_id"),
                    "raw envelope leaked: {message}"
                );
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn a_generic_message_carries_the_upstream_raw_text() {
        // A captured 429: the message is "Provider returned error" and the
        // actionable text lives in `error.metadata.raw`. Surfacing only the
        // generic string would tell the user nothing.
        let body = json!({
            "error": {
                "message": "Provider returned error",
                "code": 429,
                "metadata": {"raw": "model is temporarily rate-limited upstream"}
            }
        })
        .to_string();
        let error =
            backend_to_protocol_error(BackendError::new(Some(429), body), "chat completion");
        match error {
            Error::Provider { message, .. } => {
                assert!(message.contains("Provider returned error"), "{message}");
                assert!(
                    message.contains("temporarily rate-limited upstream"),
                    "{message}"
                );
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn a_non_json_error_body_passes_through() {
        let error = backend_to_protocol_error(
            BackendError::new(Some(502), "upstream said no"),
            "chat completion",
        );
        match error {
            Error::Provider { code, message, .. } => {
                assert_eq!(code, ErrorCode::Backend);
                assert!(message.contains("upstream said no"), "{message}");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[test]
    fn a_backend_failure_surfaces_as_a_request_error() {
        let mut provider = provider(Vec::new());
        provider.backend.post_error = Some(BackendError::new(
            Some(429),
            r#"{"error":{"message":"slow down","code":429}}"#,
        ));
        let (_, done) = run_chat(&mut provider, &request("vendor/model"));
        match done {
            Err(Error::Provider { code, message, .. }) => {
                assert_eq!(code, ErrorCode::RateLimit);
                assert!(message.contains("slow down"), "{message}");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }
}
