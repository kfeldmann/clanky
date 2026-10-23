//! Anthropic provider logic: protocol messages ⇄ the Anthropic Messages API
//! (SSE-streaming by default; a `stream = false` entry in `sampling` takes
//! the one-shot `post_message` path). Auth is [`crate::backend::AuthMode`]
//! (M11c):
//!
//! - **API key** (`ANTHROPIC_API_KEY`, checked per request): the documented
//!   third-party surface — `x-api-key` headers, no refresh, no expiry. An
//!   explicit env var wins over the credential store, the same way a CLI
//!   flag wins over settings (user's choice, not the program's).
//! - **OAuth** otherwise: credentials come from the on-disk store
//!   (`~/.clanky/providers/anthropic/credentials.json`, written by the
//!   `login` subcommand) and are refreshed automatically before expiry. A
//!   failed refresh surfaces as an actionable `auth` error — the refresh
//!   token is finite, so "login expired" will be the most common failure.

use std::cell::RefCell;

use clanky_protocol::{
    CancelFlag, Capabilities, ChatDone, ChatRequest, ChunkPayload, Error, ErrorCode, FinishReason,
    Handler, ModelInfo, PluginInfo, Sampling,
};
use serde_json::Value;

use crate::adapter;
use crate::auth::{self, Credentials};
use crate::backend::AuthMode;
use crate::oauth::{self, OAuthError};

/// Whether the request body ends up asking for a stream: the user's
/// `stream` entry in `sampling` wins over the provider's streaming default
/// (invariant: a user-supplied value is authoritative — this decides which
/// transport `chat` uses, so it must match what `build_request_body` puts
/// on the wire; `sampling` is an open JSON map and any non-`true` value
/// there means "do not stream").
fn wants_stream(sampling: Option<&Sampling>) -> bool {
    match sampling.and_then(|s| s.get("stream")) {
        Some(Value::Bool(false)) | Some(Value::Null) => false,
        // Absent, or any other value the backend will judge on its own
        // merits (a string, a number, …): the provider default is streaming.
        _ => true,
    }
}

/// Provider name reported at handshake.
pub const PROVIDER_NAME: &str = "anthropic";
/// Messages API base URL (paths are joined onto this).
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Beta features the OAuth surface requires (exact string spike-captured).
pub const OAUTH_BETA: &str = "claude-code-20250219,oauth-2025-04-20";
/// `anthropic-version` header value (spike-confirmed: required).
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// User agent presented to the OAuth surface (keep fresh; Pi pins 2.1.280).
pub const CLAUDE_CLI_VERSION: &str = "2.1.280";

/// Dollars-per-million-tokens hints for the cost display, from the live
/// catalog when present. Only the ones we can name confidently.
const MODEL_PRICING: &[(&str, f64, f64)] = &[
    // (id substring, input $/Mtok, output $/Mtok) — public list prices.
    ("claude-opus", 5.0, 25.0),
    ("claude-sonnet", 2.0, 10.0),
    ("claude-haiku", 0.1, 0.5),
    ("claude-fable", 10.0, 50.0),
];

/// Anthropic provider, generic over the HTTP backend so tests can inject
/// canned responses. The concrete blocking-HTTP instance is [`crate::Anthropic`].
pub struct AnthropicProvider<B: crate::backend::Backend> {
    backend: B,
    base_url: String,
    /// Advertised default model (handshake hint for pickers).
    default_model: Option<String>,
    /// Set by the plugin runtime (spec §7); checked between SSE events so a
    /// cancel stops generation at the next event boundary. A cancel during a
    /// single blocked read is covered by the client's kill fallback.
    cancel: CancelFlag,
    /// Credentials loaded lazily (they may have been written by `login`
    /// after this process started) and refreshed on expiry. `None` until the
    /// first successful load; `Some(Err)` caches the failure so every
    /// request surfaces the same actionable error.
    credentials: RefCell<Option<Result<Credentials, Error>>>,
    /// Credential source (defaults to the on-disk store; injectable in
    /// tests). `None` models "no usable credentials".
    loader: CredentialLoader,
    /// API-key source (defaults to `$ANTHROPIC_API_KEY`; injectable in
    /// tests). Checked first, per request. `None` means "no key".
    key_loader: KeyLoader,
    /// Persists refreshed credentials (defaults to the on-disk store).
    saver: CredentialSaver,
}

/// Loads stored credentials; `None` means "not present / unusable".
type CredentialLoader = Box<dyn Fn() -> Option<Credentials>>;
/// Persists (rotated) credentials after a refresh.
type CredentialSaver = Box<dyn Fn(&Credentials)>;
/// Reads the API key; `None` means "no key set".
type KeyLoader = Box<dyn Fn() -> Option<String>>;

impl AnthropicProvider<crate::backend::UreqBackend> {
    /// Build the standard provider from the environment. Two auth modes
    /// (M11c): `ANTHROPIC_API_KEY` if set (checked per request, no refresh),
    /// otherwise the on-disk OAuth store — so the provider always constructs
    /// and only reports `auth` errors per request.
    pub fn from_env() -> Self {
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.into());
        Self::new(crate::backend::UreqBackend::new(), base_url)
    }
}

impl<B: crate::backend::Backend> AnthropicProvider<B> {
    pub fn new(backend: B, base_url: impl Into<String>) -> Self {
        Self {
            backend,
            base_url: base_url.into(),
            default_model: None,
            cancel: CancelFlag::default(),
            loader: Box::new(auth::load),
            key_loader: Box::new(load_api_key),
            credentials: RefCell::new(None),
            saver: Box::new(|creds| {
                if let Err(err) = auth::store(creds) {
                    eprintln!(
                        "clanky-provider-anthropic: cannot persist refreshed credentials: {err}"
                    );
                }
            }),
        }
    }

    /// Inject the API-key loader (tests); defaults to `$ANTHROPIC_API_KEY`.
    pub fn with_api_key(mut self, key_loader: KeyLoader) -> Self {
        self.key_loader = key_loader;
        self
    }

    /// Inject the credential loader/saver (tests); both default to the
    /// on-disk store under `~/.clanky/providers/anthropic/`.
    pub fn with_credentials(
        mut self,
        loader: Box<dyn Fn() -> Option<Credentials>>,
        saver: Box<dyn Fn(&Credentials)>,
    ) -> Self {
        self.loader = loader;
        self.saver = saver;
        self
    }

    /// Advertise `model` as the handshake default.
    pub fn with_default_model(mut self, model: Option<String>) -> Self {
        self.default_model = model;
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url.trim_end_matches('/'))
    }

    /// Current request auth: the API key if set, else the OAuth store
    /// (refreshing past-expiry tokens in place). The `Some(Err)` cache means
    /// "login expired" is not re-attempted per turn.
    fn current_auth(&self) -> Result<AuthMode, Error> {
        // The key wins: an explicitly set env var is the user's choice.
        if let Some(key) = (self.key_loader)() {
            return Ok(AuthMode::ApiKey(key));
        }
        self.current_credentials().map(AuthMode::Bearer)
    }

    /// Current OAuth credentials, refreshing past-expiry ones in place. The
    /// `Some(Err)` cache means "login expired" is not re-attempted per turn.
    fn current_credentials(&self) -> Result<Credentials, Error> {
        // A cached failure ("login expired") short-circuits every request.
        let cached_err = match self.credentials.borrow().as_ref() {
            Some(Err(err)) => Some(reissue_error(err)),
            _ => None,
        };
        if let Some(err) = cached_err {
            return Err(err);
        }
        if let Some(Ok(creds)) = self
            .credentials
            .borrow()
            .as_ref()
            .filter(|c| creds_fresh(c))
        {
            return Ok(creds.clone());
        }
        // Otherwise (re)load: `login` may have written credentials after
        // this process started, or the cached token has since expired.
        match self.load_and_refresh() {
            Ok(creds) => {
                *self.credentials.borrow_mut() = Some(Ok(creds.clone()));
                Ok(creds)
            }
            Err(err) => {
                *self.credentials.borrow_mut() = Some(Err(reissue_error(&err)));
                Err(err)
            }
        }
    }

    /// Load credentials via the injected loader; if they exist but are
    /// expired, refresh them first (persisting the rotated refresh token).
    fn load_and_refresh(&self) -> Result<Credentials, Error> {
        let loaded = (self.loader)().ok_or_else(no_credentials_error)?;
        if !loaded.expired() {
            return Ok(loaded);
        }
        self.refresh(&loaded)
    }

    /// Refresh via the token endpoint and persist the rotated credentials.
    fn refresh(&self, stale: &Credentials) -> Result<Credentials, Error> {
        let response = oauth::refresh(&stale.refresh_token).map_err(map_oauth_error)?;
        let creds = Credentials {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at: auth::expiry_from(response.expires_in),
            refresh_token_expires_at: response
                .refresh_token_expires_in
                .map(|s| auth::now_ms() + s * 1000),
        };
        // Non-fatal persistence: the turn proceeds with the in-memory
        // token; the next process pays for one extra refresh.
        (self.saver)(&creds);
        *self.credentials.borrow_mut() = Some(Ok(creds.clone()));
        Ok(creds)
    }
}

/// Whether a cached credential entry is a live (unexpired) token.
fn creds_fresh(entry: &Result<Credentials, Error>) -> bool {
    matches!(entry, Ok(creds) if !creds.expired())
}

/// `clanky_protocol::Error` is not `Clone`; reissue a provider error.
fn reissue_error(err: &Error) -> Error {
    match err {
        Error::Provider {
            code,
            message,
            retryable,
            retry_after_ms,
        } => Error::Provider {
            code: *code,
            message: message.clone(),
            retryable: *retryable,
            retry_after_ms: *retry_after_ms,
        },
        other => Error::Provider {
            code: ErrorCode::Internal,
            message: other.to_string(),
            retryable: false,
            retry_after_ms: None,
        },
    }
}

/// The refresh token has expired or been rotated away: re-login (the key
/// alternative is noted in `no_credentials_error`, not here — a re-login is
/// the one-step fix for someone on the OAuth path).
fn login_expired_error() -> Error {
    Error::Provider {
        code: ErrorCode::Auth,
        message: "login expired — run `clanky-provider-anthropic login`".into(),
        retryable: false,
        retry_after_ms: None,
    }
}

/// No usable credentials at all: both ways to get them, one actionable step
/// each.
fn no_credentials_error() -> Error {
    Error::Provider {
        code: ErrorCode::Auth,
        message: "not logged in — run `clanky-provider-anthropic login`, or set ANTHROPIC_API_KEY"
            .into(),
        retryable: false,
        retry_after_ms: None,
    }
}

/// The API key from the environment, if set to a non-empty value.
fn load_api_key() -> Option<String> {
    std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
}

/// Map an OAuth-flow failure to a protocol error. `invalid_grant` means the
/// refresh token has expired or been rotated away: the actionable message.
fn map_oauth_error(err: OAuthError) -> Error {
    match err {
        OAuthError::Http { status, body } if status == 400 || status == 401 => {
            let body_text = oauth::extract_error(&body);
            if body_text.contains("invalid_grant") || body_text.contains("invalid") {
                login_expired_error()
            } else {
                Error::Provider {
                    code: ErrorCode::Auth,
                    message: format!("token refresh failed: {status}: {body_text}"),
                    retryable: false,
                    retry_after_ms: None,
                }
            }
        }
        OAuthError::Http { status, body } => Error::Provider {
            code: ErrorCode::Backend,
            message: format!(
                "token refresh failed: {status}: {}",
                oauth::extract_error(&body)
            ),
            retryable: true,
            retry_after_ms: None,
        },
        OAuthError::Network(message) => Error::Provider {
            code: ErrorCode::Backend,
            message: format!("token refresh failed: {message}"),
            retryable: true,
            retry_after_ms: None,
        },
        OAuthError::Malformed(message) => Error::Provider {
            code: ErrorCode::Protocol,
            message,
            retryable: false,
            retry_after_ms: None,
        },
    }
}

impl<B: crate::backend::Backend> Handler for AnthropicProvider<B> {
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
        let auth = self.current_auth()?;
        let body = self
            .backend
            .get_model_list(&self.url("v1/models"), &auth)
            .map_err(|e| map_backend_error(e, "listing models"))?;
        parse_model_list(&body)
    }

    fn chat(
        &mut self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<ChatDone, Error> {
        let auth = self.current_auth()?;
        let streaming = wants_stream(request.sampling.as_ref());
        let body = adapter::build_request_body(
            &request.model,
            &request.messages,
            request.tools.as_deref(),
            request.sampling.as_ref(),
            request.thinking,
            streaming,
        )?;
        // The same `stream` decision that went into the body picks the
        // transport: a `stream = false` response is one JSON object, not
        // SSE — feeding it through the stream parser would corrupt or
        // discard the user's explicitly requested shape.
        if !streaming {
            return self.chat_non_streaming(&auth, &body, sink);
        }
        let payloads = self
            .backend
            .post_stream(&self.url("v1/messages"), &auth, &body)
            .map_err(|e| map_backend_error(e, "chat completion"))?;
        let mut state = adapter::StreamState::default();
        for payload in payloads {
            // Spec §7: stop promptly once the client cancels. The check runs
            // between SSE events; a cancel during a blocked read is handled
            // by the client's kill fallback.
            if self.cancel.is_cancelled() {
                return Ok(ChatDone {
                    finish_reason: FinishReason::Cancelled,
                    usage: state.clone_usage(),
                });
            }
            let payload = payload.map_err(|e| map_backend_error(e, "chat stream"))?;
            let trimmed = payload.trim();
            if trimmed.is_empty() {
                continue;
            }
            let event: adapter::StreamEvent = serde_json::from_str(trimmed)
                .map_err(|e| Error::Protocol(format!("malformed stream event: {e}")))?;
            if state.ingest(&event, sink) {
                break; // message_stop
            }
        }
        let parts = state.finish();
        Ok(ChatDone {
            finish_reason: parts.finish_reason,
            usage: parts.usage,
        })
    }
}

impl<B: crate::backend::Backend> AnthropicProvider<B> {
    /// The non-streaming path (`stream = false` in `sampling`): POST and
    /// read the whole response, then translate it via
    /// [`adapter::response_to_events`], which emits the same chunks the
    /// streaming path would have — the protocol stays streaming-first
    /// regardless of what the backend wire request asked for.
    ///
    /// Cancellation granularity here is whole-response (no event
    /// boundaries to check between); that is the shape the user asked for.
    fn chat_non_streaming(
        &self,
        auth: &AuthMode,
        body: &str,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<ChatDone, Error> {
        let response = self
            .backend
            .post_message(&self.url("v1/messages"), auth, body)
            .map_err(|e| map_backend_error(e, "chat completion"))?;
        let trimmed = response.trim();
        if trimmed.is_empty() {
            return Err(Error::Protocol(
                "empty response body from a non-streaming request".into(),
            ));
        }
        let parsed: adapter::AnthropicResponse = serde_json::from_str(trimmed)
            .map_err(|e| Error::Protocol(format!("malformed messages response: {e}")))?;
        if let Some(err) = parsed.error.as_ref() {
            // The non-streaming error shape rides in the 200/body the
            // adapter parses (`{"type":"error","error":{...}}`).
            let message = err
                .message
                .clone()
                .unwrap_or_else(|| "unknown provider error".into());
            return Err(Error::Provider {
                code: ErrorCode::Backend,
                message,
                retryable: true,
                retry_after_ms: None,
            });
        }
        let parts = adapter::response_to_events(&parsed, sink);
        Ok(ChatDone {
            finish_reason: parts.finish_reason,
            usage: parts.usage,
        })
    }
}

fn parse_model_list(body: &str) -> Result<Vec<ModelInfo>, Error> {
    let root: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| Error::Protocol(format!("malformed models response: {e}")))?;
    let data = root
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| Error::Protocol("models response has no `data` array".into()))?;
    Ok(data
        .iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_string();
            let caps = item.get("capabilities");
            let context_window = item.get("max_input_tokens").and_then(|v| v.as_u64());
            let pricing = MODEL_PRICING
                .iter()
                .find(|(needle, _, _)| id.contains(needle));
            let supports_thinking = caps
                .and_then(|c| c.get("thinking"))
                .and_then(|t| t.get("supported"))
                .and_then(|v| v.as_bool());
            Some(ModelInfo {
                id,
                display_name: item
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                context_window,
                supports_thinking,
                // Every Messages API model supports tool use.
                supports_tools: Some(true),
                supports_text_generation: Some(true),
                input_price_per_mtok: pricing.map(|(_, i, _)| *i),
                output_price_per_mtok: pricing.map(|(_, _, o)| *o),
                cache_read_price_per_mtok: None,
            })
        })
        .collect())
}

/// Map a backend (HTTP-layer) failure to a request-scoped protocol error.
fn map_backend_error(err: crate::backend::BackendError, context: &str) -> Error {
    let crate::backend::BackendError {
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
        crate::backend::truncate_body(&adapter::extract_error_message(message.trim()), 400)
    };
    Error::Provider {
        code,
        message: format!("{status_note}{detail}"),
        retryable: matches!(code, ErrorCode::RateLimit | ErrorCode::Backend),
        retry_after_ms: if code == ErrorCode::RateLimit {
            retry_after_ms
        } else {
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::{StreamEvent, StreamState};
    use clanky_protocol::{ChatMessage, ChunkPayload, Tool, Usage};
    use serde_json::json;
    use std::cell::RefCell;

    /// The spike's streaming captures, pinned in `tests/fixtures/`
    /// (one per shape: plain text, thinking, tool_use).
    const STREAM_BASIC: &str = include_str!("../tests/fixtures/stream-basic.txt");
    const STREAM_TOOLS: &str = include_str!("../tests/fixtures/stream-tools.txt");
    const STREAM_THINKING: &str = include_str!("../tests/fixtures/stream-thinking.txt");

    /// A canned backend that never touches disk (credentials injected).
    fn test_creds() -> Credentials {
        Credentials {
            access_token: "sk-ant-oat01-test".into(),
            refresh_token: "sk-ant-ort01-test".into(),
            expires_at: auth::now_ms() + 3_600_000,
            refresh_token_expires_at: None,
        }
    }

    /// Records requests and replays a captured SSE stream.
    #[derive(Default)]
    struct MockBackend {
        posts: RefCell<Vec<(String, String)>>,
        /// API keys seen by `post_stream` (empty on the bearer path).
        api_keys: RefCell<Vec<String>>,
        /// Bodies recorded by `post_message` (the non-streaming path).
        non_stream_posts: RefCell<Vec<(String, String)>>,
        /// The canned non-streaming body `post_message` returns.
        response: RefCell<Option<Result<String, crate::backend::BackendError>>>,
        sse: Vec<String>,
        post_error: Option<crate::backend::BackendError>,
    }

    impl crate::backend::Backend for MockBackend {
        fn get_model_list(
            &self,
            _url: &str,
            _auth: &AuthMode,
        ) -> Result<String, crate::backend::BackendError> {
            Ok(r#"{"data": []}"#.into())
        }

        fn post_message(
            &self,
            url: &str,
            auth: &AuthMode,
            body: &str,
        ) -> Result<String, crate::backend::BackendError> {
            self.non_stream_posts
                .borrow_mut()
                .push((url.to_string(), body.to_string()));
            match auth {
                AuthMode::Bearer(c) => {
                    assert_eq!(
                        c.access_token, "sk-ant-oat01-test",
                        "unexpected auth: {auth:?}"
                    )
                }
                AuthMode::ApiKey(k) => {
                    assert_eq!(k, "sk-ant-api03-test", "unexpected auth: {auth:?}")
                }
            }
            self.response
                .borrow_mut()
                .take()
                .expect("a non-streaming chat ran without a canned response")
        }

        fn post_stream(
            &self,
            url: &str,
            auth: &AuthMode,
            body: &str,
        ) -> Result<
            Box<dyn Iterator<Item = Result<String, crate::backend::BackendError>>>,
            crate::backend::BackendError,
        > {
            assert!(
                matches!(auth, AuthMode::Bearer(c) if c.access_token == "sk-ant-oat01-test")
                    || matches!(auth, AuthMode::ApiKey(k) if k == "sk-ant-api03-test"),
                "unexpected auth mode: {auth:?}"
            );
            if let AuthMode::ApiKey(key) = auth {
                self.api_keys.borrow_mut().push(key.clone());
            }
            self.posts
                .borrow_mut()
                .push((url.to_string(), body.to_string()));
            if let Some(err) = &self.post_error {
                return Err(err.clone());
            }
            Ok(Box::new(self.sse.clone().into_iter().map(Ok)))
        }
    }

    /// The captured SSE text → the payload strings the backend yields
    /// (`data:` lines only, one per event — what `backend::sse_payloads`
    /// produces off the wire, with the `event:` lines ignored).
    fn payloads(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|line| line.strip_prefix(' ').unwrap_or(line).to_string())
            .filter(|line| !line.trim().is_empty())
            .collect()
    }

    fn provider(sse: Vec<String>) -> AnthropicProvider<MockBackend> {
        AnthropicProvider::new(
            MockBackend {
                sse,
                ..Default::default()
            },
            "https://api.anthropic.com",
        )
        .with_credentials(Box::new(|| Some(test_creds())), Box::new(|_| {}))
    }

    /// A provider whose backend serves one canned whole-body response (the
    /// non-streaming path) and no stream.
    fn non_streaming_provider(
        response: Result<String, crate::backend::BackendError>,
    ) -> AnthropicProvider<MockBackend> {
        AnthropicProvider::new(
            MockBackend {
                response: RefCell::new(Some(response)),
                ..Default::default()
            },
            "https://api.anthropic.com",
        )
        .with_credentials(Box::new(|| Some(test_creds())), Box::new(|_| {}))
    }

    /// A chat request asking `stream: false` in `sampling` (the user's
    /// explicit choice, honored end to end).
    fn non_streaming_request(model: &str, messages: Vec<ChatMessage>) -> ChatRequest {
        let mut req = request(model, messages);
        req.sampling = Some(clanky_protocol::Sampling::from_iter([(
            "stream".into(),
            serde_json::json!(false),
        )]));
        req
    }

    fn run_chat(
        provider: &mut AnthropicProvider<MockBackend>,
        request: &ChatRequest,
    ) -> (Vec<ChunkPayload>, Result<ChatDone, Error>) {
        let mut chunks = Vec::new();
        let done = provider.chat(request, &mut |c| chunks.push(c));
        (chunks, done)
    }

    fn request(model: &str, messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages,
            tools: None,
            sampling: None,
            thinking: None,
        }
    }

    // --- streaming chat ---------------------------------------------------

    #[test]
    fn plain_stream_answers_with_text_and_usage() {
        let mut provider = provider(payloads(STREAM_BASIC));
        let (chunks, done) = run_chat(
            &mut provider,
            &request(
                "claude-haiku-4-5-20251001",
                vec![ChatMessage::user("list 5 numbers")],
            ),
        );

        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "1,2,3,4,5");

        let done = done.unwrap();
        assert_eq!(done.finish_reason, FinishReason::Stop);
        assert_eq!(
            done.usage,
            Some(Usage {
                prompt_tokens: Some(685),
                completion_tokens: Some(13),
                cached_tokens: Some(0),
            })
        );
    }

    #[test]
    fn tool_stream_maps_to_tool_call_chunks_and_runs_the_loop_shape() {
        let mut provider = provider(payloads(STREAM_TOOLS));
        let mut req = request(
            "claude-haiku-4-5-20251001",
            vec![ChatMessage::user("list /tmp with the bash tool")],
        );
        req.tools = Some(vec![Tool {
            name: "bash".into(),
            description: Some("Run a shell command".into()),
            parameters: None,
        }]);
        let (chunks, done) = run_chat(&mut provider, &req);

        let starts: Vec<&ChunkPayload> = chunks
            .iter()
            .filter(|c| matches!(c, ChunkPayload::ToolCallStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1);
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
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&args).unwrap(),
            json!({"command": "ls -la /tmp"})
        );
        // The turn ends ToolCalls, so the agentic loop runs the tool.
        assert_eq!(done.unwrap().finish_reason, FinishReason::ToolCalls);

        // The request that went out asked for a stream.
        let body: serde_json::Value =
            serde_json::from_str(&provider.backend.posts.borrow()[0].1).unwrap();
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn thinking_stream_surfaces_thinking_chunks() {
        let mut provider = provider(payloads(STREAM_THINKING));
        let (chunks, done) = run_chat(
            &mut provider,
            &request(
                "claude-haiku-4-5-20251001",
                vec![ChatMessage::user("17*23?")],
            ),
        );

        let thinking: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Thinking { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(thinking.contains("17 * 23"), "{thinking}");
        // The visible answer is clean.
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "17 × 23 = 391");
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);
    }

    #[test]
    fn a_user_sampling_stream_false_wins_over_the_provider_default() {
        // Invariant: the user's `stream = false` in sampling is authoritative
        // — the provider never flips it back on, and the wire body carries
        // the user's value, not the streaming default.
        let mut provider =
            non_streaming_provider(Ok(include_str!("../tests/fixtures/resp-basic.json").into()));
        let mut req = request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]);
        req.sampling = Some(clanky_protocol::Sampling::from_iter([(
            "stream".into(),
            json!(false),
        )]));
        let _ = run_chat(&mut provider, &req).1;
        let body: serde_json::Value =
            serde_json::from_str(&provider.backend.non_stream_posts.borrow()[0].1).unwrap();
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn chat_posts_to_the_messages_endpoint_with_bearer_credentials() {
        let mut provider = provider(Vec::new());
        let _ = run_chat(
            &mut provider,
            &request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        )
        .1;
        assert_eq!(
            provider.backend.posts.borrow()[0].0,
            "https://api.anthropic.com/v1/messages"
        );
    }

    // --- non-streaming chat (`stream = false` in sampling) ----------------

    #[test]
    fn a_user_sampling_stream_false_takes_the_non_streaming_path() {
        // Invariant: the user's `stream = false` is accepted *and* used —
        // the response is one JSON body, never fed through the SSE parser.
        let mut provider =
            non_streaming_provider(Ok(include_str!("../tests/fixtures/resp-basic.json").into()));
        let (chunks, done) = run_chat(
            &mut provider,
            &non_streaming_request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        );
        let done = done.unwrap();
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hi! 👋");
        assert_eq!(done.finish_reason, FinishReason::Stop);
        let usage = done.usage.expect("usage from the response body");
        assert_eq!(usage.prompt_tokens, Some(675));
        assert_eq!(usage.completion_tokens, Some(8));

        // The request went out asking for exactly what the user set…
        let body: serde_json::Value =
            serde_json::from_str(&provider.backend.non_stream_posts.borrow()[0].1).unwrap();
        assert_eq!(body["stream"], false);
        // …over the one-shot transport, with no streaming call at all.
        assert!(
            provider.backend.posts.borrow().is_empty(),
            "a stream=false request must not hit the SSE transport"
        );
    }

    #[test]
    fn a_non_streaming_tool_call_matches_the_streamed_shape() {
        // The whole-body path emits the same chunks the SSE path does —
        // the client cannot tell which transport served the turn.
        let mut provider =
            non_streaming_provider(Ok(include_str!("../tests/fixtures/resp-tools.json").into()));
        let mut req = non_streaming_request(
            "claude-haiku-4-5-20251001",
            vec![ChatMessage::user("list /tmp with the bash tool")],
        );
        req.tools = Some(vec![Tool {
            name: "bash".into(),
            description: Some("Run a shell command".into()),
            parameters: None,
        }]);
        let (chunks, done) = run_chat(&mut provider, &req);

        let starts: Vec<&ChunkPayload> = chunks
            .iter()
            .filter(|c| matches!(c, ChunkPayload::ToolCallStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1);
        match starts[0] {
            ChunkPayload::ToolCallStart { index, id, name } => {
                assert_eq!(*index, 0);
                assert_eq!(id, "toolu_01BhVetyfKsqmX56rNskCQuh");
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
            serde_json::from_str::<serde_json::Value>(&args).unwrap(),
            json!({"command": "ls -1 /tmp | wc -l"})
        );
        assert_eq!(done.unwrap().finish_reason, FinishReason::ToolCalls);
    }

    #[test]
    fn a_non_streaming_thinking_response_surfaces_thinking_chunks() {
        let mut provider = non_streaming_provider(Ok(include_str!(
            "../tests/fixtures/resp-thinking.json"
        )
        .into()));
        let (chunks, done) = run_chat(
            &mut provider,
            &non_streaming_request(
                "claude-haiku-4-5-20251001",
                vec![ChatMessage::user("17*23?")],
            ),
        );
        let thinking: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Thinking { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(thinking.contains("17 * 23"), "{thinking}");
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "17 × 23 = **391**");
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);
    }

    #[test]
    fn a_malformed_non_streaming_body_is_a_protocol_error_not_sse_garbage() {
        // Before this fix, stream=false bodies went through the SSE parser
        // and surfaced as a confusing "malformed stream event" (or worse,
        // silent garbage). The one-shot path names the actual failure.
        let mut provider = non_streaming_provider(Ok("<html>gateway</html>".into()));
        let err = run_chat(
            &mut provider,
            &non_streaming_request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        )
        .1
        .unwrap_err();
        match err {
            Error::Protocol(message) => {
                assert!(message.contains("malformed messages response"), "{message}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn an_error_body_on_the_non_streaming_path_surfaces_as_a_provider_error() {
        let body = r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#;
        let mut provider = non_streaming_provider(Ok(body.into()));
        let err = run_chat(
            &mut provider,
            &non_streaming_request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        )
        .1
        .unwrap_err();
        match err {
            Error::Provider { code, message, .. } => {
                assert_eq!(code, ErrorCode::Backend);
                assert_eq!(message, "boom");
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn an_http_error_on_the_non_streaming_path_maps_like_the_streaming_one() {
        let mut provider = non_streaming_provider(Err(crate::backend::BackendError::new(
            Some(429),
            "rate limited",
        )));
        let err = run_chat(
            &mut provider,
            &non_streaming_request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        )
        .1
        .unwrap_err();
        match err {
            Error::Provider {
                code, retryable, ..
            } => {
                assert_eq!(code, ErrorCode::RateLimit);
                assert!(retryable);
            }
            other => panic!("unexpected: {other}"),
        }
    }

    // --- cancellation -----------------------------------------------------

    #[test]
    fn cancel_between_events_stops_generation_and_reports_cancelled() {
        // The basic capture, cut short after the first text delta (what the
        // stream looks like when the user hits ctrl+c between events).
        let events = payloads(STREAM_BASIC);
        let mut provider = provider(events[..4].to_vec());
        provider.cancel.cancel();
        let (chunks, done) = run_chat(
            &mut provider,
            &request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        );
        let done = done.unwrap();
        assert_eq!(
            done.finish_reason,
            FinishReason::Cancelled,
            "the client's cancel, not the stream's own stop"
        );
        // Whatever was emitted before the cancel stays.
        assert!(chunks.is_empty(), "nothing was streamed yet");
    }

    // --- API-key mode (M11c) ---------------------------------------------

    /// The M11c done-condition: an API-key user gets identical behavior
    /// through the same adapter — same endpoint, same request body, only
    /// the auth differs.
    #[test]
    fn an_api_key_user_gets_identical_behavior_through_the_same_adapter() {
        let mut provider = AnthropicProvider::new(
            MockBackend {
                sse: payloads(STREAM_BASIC),
                ..Default::default()
            },
            "https://api.anthropic.com",
        )
        .with_api_key(Box::new(|| Some("sk-ant-api03-test".into())))
        .with_credentials(Box::new(|| None), Box::new(|_| {}));

        let (chunks, done) = run_chat(
            &mut provider,
            &request(
                "claude-haiku-4-5-20251001",
                vec![ChatMessage::user("list 5 numbers")],
            ),
        );
        // Identical answer shape to the bearer-path fixture test.
        let text: String = chunks
            .iter()
            .filter_map(|c| match c {
                ChunkPayload::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "1,2,3,4,5");
        assert_eq!(done.unwrap().finish_reason, FinishReason::Stop);
        assert_eq!(
            provider.backend.api_keys.borrow().as_slice(),
            ["sk-ant-api03-test"],
            "the key path authenticates every request"
        );
        // Same endpoint and body as the bearer test pins.
        assert_eq!(
            provider.backend.posts.borrow()[0].0,
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn an_explicit_api_key_wins_over_stored_credentials() {
        // Invariant: the user's explicit env var is authoritative — it
        // selects the mode, not the program's preference.
        let mut provider = AnthropicProvider::new(
            MockBackend {
                sse: Vec::new(),
                ..Default::default()
            },
            "https://api.anthropic.com",
        )
        .with_api_key(Box::new(|| Some("sk-ant-api03-test".into())))
        .with_credentials(Box::new(|| Some(test_creds())), Box::new(|_| {}));
        let _ = run_chat(
            &mut provider,
            &request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        )
        .1;
        assert_eq!(
            provider.backend.api_keys.borrow().as_slice(),
            ["sk-ant-api03-test"]
        );
    }

    #[test]
    fn without_a_key_or_credentials_the_error_names_both_fixes() {
        let mut provider =
            AnthropicProvider::new(MockBackend::default(), "https://api.anthropic.com")
                .with_api_key(Box::new(|| None))
                .with_credentials(Box::new(|| None), Box::new(|_| {}));
        let err = run_chat(
            &mut provider,
            &request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        )
        .1
        .unwrap_err();
        match err {
            Error::Provider { code, message, .. } => {
                assert_eq!(code, ErrorCode::Auth);
                assert_eq!(
                    message,
                    "not logged in — run `clanky-provider-anthropic login`, or set ANTHROPIC_API_KEY"
                );
            }
            other => panic!("unexpected: {other}"),
        }
    }

    // --- error mapping ---------------------------------------------------

    #[test]
    fn stream_errors_surface_as_backend_errors() {
        let mut provider = provider(Vec::new());
        provider.backend.post_error =
            Some(crate::backend::BackendError::new(Some(429), "rate limited"));
        let err = run_chat(
            &mut provider,
            &request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        )
        .1
        .unwrap_err();
        match err {
            Error::Provider {
                code, retryable, ..
            } => {
                assert_eq!(code, ErrorCode::RateLimit);
                assert!(retryable);
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn a_malformed_event_is_a_protocol_error() {
        let mut provider = provider(vec!["{not json}".into()]);
        let err = run_chat(
            &mut provider,
            &request("claude-haiku-4-5-20251001", vec![ChatMessage::user("hi")]),
        )
        .1
        .unwrap_err();
        match err {
            Error::Protocol(message) => {
                assert!(message.contains("malformed stream event"), "{message}")
            }
            other => panic!("unexpected: {other}"),
        }
    }

    #[test]
    fn the_first_event_parses_and_message_stop_terminates() {
        // Adapter-level sanity for the state the provider drives.
        let mut state = StreamState::default();
        let mut terminal = false;
        for payload in payloads(STREAM_BASIC) {
            let event: StreamEvent = serde_json::from_str(payload.trim()).unwrap();
            if state.ingest(&event, &mut |_| {}) {
                terminal = true;
                break;
            }
        }
        assert!(terminal, "message_stop must be terminal");
    }
}
