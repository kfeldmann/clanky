//! HTTP transport to the Anthropic Messages API.
//!
//! Kept behind the [`Backend`] trait so provider logic is testable without
//! network access. Auth is [`AuthMode`]: the two modes differ only in
//! headers, and every request goes through the same Messages adapter.
//!
//! - `AuthMode::Bearer` (OAuth; spike-confirmed):
//!   - `Authorization: Bearer <token>` (never `x-api-key` on this path)
//!   - `anthropic-beta: claude-code-20250219,oauth-2025-04-20`
//!   - `anthropic-version: 2023-06-01` (required — the spike's first 400)
//!   - `user-agent: claude-cli/2.1.280` + `x-app: cli`
//! - `AuthMode::ApiKey` (`ANTHROPIC_API_KEY`; M11c, best-effort — no key on
//!   this tenant to test against): the documented plain headers
//!   - `x-api-key: <key>`
//!   - `anthropic-version: 2023-06-01`
//!
//!   No Claude Code identity headers on the key path: this is the standard
//!   third-party surface, not the OAuth one.
//!
//! Only the Anthropic-specific parts live here: the `ureq` agent and its
//! auth headers, the one-shot Messages calls, and the streaming request. The
//! SSE framer, `Retry-After` parsing and `truncate_body` are re-exported from
//! the shared OpenAI-compatible crate — the framing rules are the same and
//! the old local date parser differed only in signedness.

use std::time::Duration;

use crate::auth::Credentials;

/// How a request authenticates. The two modes differ only in headers
/// ([`apply_auth`]); the body adapter is shared.
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// OAuth bearer token (the `login` subcommand / refresh path).
    Bearer(Credentials),
    /// API key (`ANTHROPIC_API_KEY`) sent as `x-api-key`.
    ApiKey(String),
}

impl AuthMode {
    /// The OAuth access token, or `None` on the API-key path. Used to tell
    /// *which* token a 401 rejected (and whether it can be refreshed at
    /// all: a key cannot).
    pub fn bearer_token(&self) -> Option<&str> {
        match self {
            Self::Bearer(creds) => Some(&creds.access_token),
            Self::ApiKey(_) => None,
        }
    }
}

pub use clanky_provider_openai::backend::{BackendError, now_secs, sse_payloads, truncate_body};

/// Attach a retry hint parsed from a `Retry-After` header value
/// (delay-seconds or HTTP-date; the date form is measured against
/// `now_secs`, the current UNIX time). The parser is the shared
/// OpenAI-compatible one: it returns milliseconds, clamps negative and
/// already-past values to 0, and treats anything unparseable as "no hint".
pub fn backend_error_with_retry_after(
    mut err: BackendError,
    value: Option<&str>,
    now_secs: u64,
) -> BackendError {
    err.retry_after_ms = value.and_then(|v| parse_retry_after(v, now_secs));
    err
}

/// Parse a `Retry-After` header into milliseconds. Re-exported from the
/// shared crate so the Anthropic plugin and the OpenAI-compatible ones
/// cannot drift apart (the old local copy differed only in signedness).
pub use clanky_provider_openai::backend::parse_retry_after;

/// Talks to the Anthropic API. Auth headers are applied here because the
/// token never leaves the plugin; the provider passes the credential
/// *handle*, not the header strings.
pub trait Backend {
    /// `GET <url>` with OAuth bearer + beta headers (model listing).
    fn get_model_list(&self, url: &str, auth: &AuthMode) -> Result<String, BackendError>;

    /// `POST <url>` with auth headers, returning the whole response body —
    /// the one-shot Messages call (`stream: false`).
    fn post_message(&self, url: &str, auth: &AuthMode, body: &str) -> Result<String, BackendError>;

    /// POST a request and stream the response as SSE payloads — one item
    /// per event, the joined `data:` lines with the prefix and a single
    /// leading space stripped. Errors after the stream opens (a dropped
    /// connection) surface on the item stream; errors before it (a non-2xx
    /// status) are the `Err` of this call.
    fn post_stream(
        &self,
        url: &str,
        auth: &AuthMode,
        body: &str,
    ) -> Result<Box<dyn Iterator<Item = Result<String, BackendError>>>, BackendError>;
}

/// Blocking HTTP backend built on `ureq`.
pub struct UreqBackend {
    agent: ureq::Agent,
}

impl UreqBackend {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            // Connect is bounded, but the request itself has no timeout:
            // a long response must never be killed mid-answer.
            .timeout_connect(Duration::from_secs(15))
            .build();
        Self { agent }
    }

    fn call(&self, req: ureq::Request, body: Option<&str>) -> Result<String, BackendError> {
        let result = match body {
            Some(body) => req.send_string(body),
            None => req.call(),
        };
        match result {
            Ok(resp) => resp
                .into_string()
                .map_err(|e| BackendError::new(None, e.to_string())),
            Err(ureq::Error::Status(status, resp)) => {
                let retry_after = resp.header("retry-after").map(str::to_owned);
                let text = resp.into_string().unwrap_or_default();
                Err(backend_error_with_retry_after(
                    BackendError::new(Some(status), text),
                    retry_after.as_deref(),
                    now_secs(),
                ))
            }
            Err(err) => Err(BackendError::new(None, err.to_string())),
        }
    }
}

impl Default for UreqBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for UreqBackend {
    fn get_model_list(&self, url: &str, auth: &AuthMode) -> Result<String, BackendError> {
        self.call(apply_auth(self.agent.get(url), auth), None)
    }

    fn post_message(&self, url: &str, auth: &AuthMode, body: &str) -> Result<String, BackendError> {
        let req = apply_auth(self.agent.post(url), auth)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json");
        self.call(req, Some(body))
    }

    fn post_stream(
        &self,
        url: &str,
        auth: &AuthMode,
        body: &str,
    ) -> Result<Box<dyn Iterator<Item = Result<String, BackendError>>>, BackendError> {
        let req = apply_auth(self.agent.post(url), auth)
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream");
        let resp = match req.send_string(body) {
            Ok(resp) => resp,
            Err(ureq::Error::Status(status, resp)) => {
                let retry_after = resp.header("retry-after").map(str::to_owned);
                let text = resp.into_string().unwrap_or_default();
                return Err(backend_error_with_retry_after(
                    BackendError::new(Some(status), text),
                    retry_after.as_deref(),
                    now_secs(),
                ));
            }
            Err(err) => return Err(BackendError::new(None, err.to_string())),
        };
        Ok(Box::new(sse_payloads(std::io::BufReader::new(
            resp.into_reader(),
        ))))
    }
}

/// Attach the mode's auth headers. The bearer path uses the exact strings
/// the spike captured (Claude Code identity required by the OAuth surface);
/// the key path uses the documented plain headers — no Claude Code
/// identity, since it is the standard third-party surface, not the OAuth
/// one.
fn apply_auth(req: ureq::Request, auth: &AuthMode) -> ureq::Request {
    match auth {
        AuthMode::Bearer(creds) => req
            .set("Authorization", &format!("Bearer {}", creds.access_token))
            .set("anthropic-version", "2023-06-01")
            .set("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
            .set("User-Agent", "claude-cli/2.1.280")
            .set("x-app", "cli"),
        AuthMode::ApiKey(key) => req
            .set("x-api-key", key)
            .set("anthropic-version", "2023-06-01"),
    }
}

/// The shared `parse_retry_after` covers both header forms (delay-seconds
/// and HTTP-date). These pin the Anthropic plugin's behavior through its own
/// `backend_error_with_retry_after` wrapper: negative and already-past
/// values clamp to 0, and an absent or unparseable header leaves no hint.
#[cfg(test)]
mod retry_after_tests {
    use super::*;

    #[test]
    fn delay_seconds_parses_and_clamps() {
        let now = 1_000_000;
        assert_eq!(
            backend_error_with_retry_after(BackendError::new(None, "x"), Some("3"), now)
                .retry_after_ms,
            Some(3_000)
        );
        assert_eq!(
            backend_error_with_retry_after(BackendError::new(None, "x"), Some(" 0 "), now)
                .retry_after_ms,
            Some(0)
        );
        assert_eq!(
            backend_error_with_retry_after(BackendError::new(None, "x"), Some("-5"), now)
                .retry_after_ms,
            Some(0)
        );
    }

    #[test]
    fn http_date_parses_relative_to_now() {
        let now = 784_111_747; // 1994-11-06T08:49:07Z
        let date = "Sun, 06 Nov 1994 08:49:37 GMT";
        let err = backend_error_with_retry_after(BackendError::new(None, "x"), Some(date), now);
        assert_eq!(err.retry_after_ms, Some(30_000));
        // In the past: clamps to 0, never a negative delay.
        let past =
            backend_error_with_retry_after(BackendError::new(None, "x"), Some(date), now + 31);
        assert_eq!(past.retry_after_ms, Some(0));
    }

    #[test]
    fn absent_or_unparseable_values_leave_no_hint() {
        let now = 1_000_000;
        for value in [None, Some(""), Some("soon"), Some("Mon, 06 Nov 1994")] {
            let err = backend_error_with_retry_after(BackendError::new(None, "x"), value, now);
            assert_eq!(err.retry_after_ms, None, "value: {value:?}");
        }
    }
}

/// `apply_auth` sets the mode's headers (checked structurally here; the
/// wire strings are the spike-captured / documented ones).
#[cfg(test)]
mod auth_headers_tests {
    use super::*;

    fn request() -> ureq::Request {
        UreqBackend::new()
            .agent
            .get("https://api.anthropic.com/v1/messages")
    }

    #[test]
    fn bearer_mode_sends_the_oauth_surface_headers() {
        let req = apply_auth(
            request(),
            &AuthMode::Bearer(Credentials {
                access_token: "sk-ant-oat01-x".into(),
                refresh_token: "r".into(),
                expires_at: 0,
                refresh_token_expires_at: None,
            }),
        );
        assert_eq!(req.header("authorization"), Some("Bearer sk-ant-oat01-x"));
        assert_eq!(
            req.header("anthropic-beta"),
            Some("claude-code-20250219,oauth-2025-04-20")
        );
        assert_eq!(req.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(req.header("user-agent"), Some("claude-cli/2.1.280"));
        assert_eq!(req.header("x-app"), Some("cli"));
        assert_eq!(req.header("x-api-key"), None);
    }

    #[test]
    fn api_key_mode_sends_the_documented_plain_headers() {
        let req = apply_auth(request(), &AuthMode::ApiKey("sk-ant-api03-x".into()));
        assert_eq!(req.header("x-api-key"), Some("sk-ant-api03-x"));
        assert_eq!(req.header("anthropic-version"), Some("2023-06-01"));
        // No OAuth-surface identity on the key path.
        assert_eq!(req.header("authorization"), None);
        assert_eq!(req.header("anthropic-beta"), None);
        assert_eq!(req.header("x-app"), None);
    }
}

/// The SSE framer is the shared one; these pin the shapes Anthropic's
/// stream actually uses (`event:` lines interleaved with `data:` ones).
#[cfg(test)]
mod sse_tests {
    use super::*;
    use std::io::{BufRead, Cursor};

    fn payloads(text: &str) -> Vec<Result<String, BackendError>> {
        sse_payloads(Cursor::new(text.to_string())).collect()
    }

    #[test]
    fn frames_single_data_events() {
        let items = payloads("data: {\"a\":1}\n\ndata: {\"b\":2}\n\n");
        let got: Vec<String> = items.into_iter().map(Result::unwrap).collect();
        assert_eq!(got, ["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn ignores_event_fields_and_comments() {
        // The captured streams interleave `event:` lines with `data:` ones.
        let items = payloads("event: content_block_delta\ndata: {\"x\":true}\n\n: keep-alive\n\n");
        let got: Vec<String> = items.into_iter().map(Result::unwrap).collect();
        assert_eq!(got, ["{\"x\":true}"]);
    }

    #[test]
    fn strips_one_leading_space_and_handles_crlf_and_missing_blank_line() {
        let items = payloads("data:  two\r\n\r\ndata:x\r\n");
        let got: Vec<String> = items.into_iter().map(Result::unwrap).collect();
        assert_eq!(got, [" two", "x"]);
    }

    #[test]
    fn read_errors_are_surfaced() {
        struct FailingReader;
        impl std::io::Read for FailingReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken pipe"))
            }
        }
        impl BufRead for FailingReader {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                Err(std::io::Error::other("broken pipe"))
            }
            fn consume(&mut self, _amt: usize) {}
        }
        let items: Vec<Result<String, BackendError>> = sse_payloads(FailingReader).collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());
    }
}
