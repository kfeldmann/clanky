//! Anthropic provider logic: protocol messages ⇄ the Anthropic Messages API
//! (bearer-auth OAuth path; non-streaming in M11a).
//!
//! Auth is OAuth-only in M11a: credentials come from the on-disk store
//! (`~/.clanky/providers/anthropic/credentials.json`, written by the
//! `login` subcommand), and are refreshed automatically before expiry. A
//! failed refresh surfaces as an actionable `auth` error — the refresh
//! token is finite, so "login expired" will be the most common failure.

use std::cell::RefCell;

use clanky_protocol::{
    CancelFlag, Capabilities, ChatDone, ChatRequest, ChunkPayload, Error, ErrorCode, FinishReason,
    Handler, ModelInfo, PluginInfo, Usage,
};

use crate::adapter::{self, AnthropicResponse};
use crate::auth::{self, Credentials};
use crate::oauth::{self, OAuthError};

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
    /// Set by the plugin runtime (spec §7); checked around the blocking HTTP
    /// call (non-streaming in M11a) — a cancel during the single call is
    /// covered by the client's kill fallback.
    cancel: CancelFlag,
    /// Credentials loaded lazily (they may have been written by `login`
    /// after this process started) and refreshed on expiry. `None` until the
    /// first successful load; `Some(Err)` caches the failure so every
    /// request surfaces the same actionable error.
    credentials: RefCell<Option<Result<Credentials, Error>>>,
    /// Credential source (defaults to the on-disk store; injectable in
    /// tests). `None` models "no usable credentials".
    loader: CredentialLoader,
    /// Persists refreshed credentials (defaults to the on-disk store).
    saver: CredentialSaver,
}

/// Loads stored credentials; `None` means "not present / unusable".
type CredentialLoader = Box<dyn Fn() -> Option<Credentials>>;
/// Persists (rotated) credentials after a refresh.
type CredentialSaver = Box<dyn Fn(&Credentials)>;

impl AnthropicProvider<crate::backend::UreqBackend> {
    /// Build the standard provider from the environment. Unlike the other
    /// plugins there is no key env var: credentials live on disk, so the
    /// provider always constructs and only reports `auth` errors per request.
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

    /// Current credentials, refreshing past-expiry ones in place. The
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
        let loaded = (self.loader)().ok_or_else(login_expired_error)?;
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

fn login_expired_error() -> Error {
    Error::Provider {
        code: ErrorCode::Auth,
        message: "login expired — run `clanky-provider-anthropic login`".into(),
        retryable: false,
        retry_after_ms: None,
    }
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
        let creds = self.current_credentials()?;
        let body = self
            .backend
            .get_model_list(&self.url("v1/models"), &creds)
            .map_err(|e| map_backend_error(e, "listing models"))?;
        parse_model_list(&body)
    }

    fn chat(
        &mut self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<ChatDone, Error> {
        let creds = self.current_credentials()?;
        let body = adapter::build_request_body(
            &request.model,
            &request.messages,
            request.tools.as_deref(),
            request.sampling,
            request.thinking,
        )?;
        let raw = self
            .backend
            .post_message(&self.url("v1/messages"), &creds, &body)
            .map_err(|e| map_backend_error(e, "chat completion"))?;
        let response: AnthropicResponse = serde_json::from_str(&raw)
            .map_err(|e| Error::Protocol(format!("malformed messages response: {e}")))?;
        let parts = adapter::response_to_events(&response, sink);
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

/// Usage part carried between the adapter and the done mapping (kept local;
/// the protocol `Usage` is constructed in `adapter::response_to_events`).
#[allow(dead_code)]
fn usage_from(u: &adapter::AnthropicUsage) -> Option<Usage> {
    Some(Usage {
        prompt_tokens: u.input_tokens,
        completion_tokens: u.output_tokens,
        cached_tokens: u.cache_read_input_tokens,
    })
}

/// `FinishReason` is re-exported here for the adapter's mapping tests.
#[allow(unused_imports)]
use FinishReason as _;
