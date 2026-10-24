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

use std::io::BufRead;
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

/// A failure at the HTTP layer: an HTTP status (when the server answered)
/// or a connection/client error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub status: Option<u16>,
    pub message: String,
    /// Server-provided retry hint in milliseconds (from a `Retry-After`
    /// header, delay-seconds or HTTP-date form). `None` when absent or
    /// unparseable; only meaningful on rate-limit responses.
    pub retry_after_ms: Option<u64>,
}

/// Attach a retry hint parsed from a `Retry-After` header value
/// (delay-seconds or HTTP-date; the date form is measured against
/// `now_secs`, the current UNIX time).
pub fn backend_error_with_retry_after(
    mut err: BackendError,
    value: Option<&str>,
    now_secs: u64,
) -> BackendError {
    err.retry_after_ms = value
        .and_then(|v| parse_retry_after(v, now_secs))
        .map(|secs| secs.max(0) as u64 * 1000);
    err
}

/// Parse a `Retry-After` header into delay seconds. Accepts the
/// delay-seconds form (negative clamps to 0) and the HTTP-date form
/// (RFC 9110 IMF-fixdate, measured against `now_secs`); anything else is
/// `None`.
fn parse_retry_after(value: &str, now_secs: u64) -> Option<i64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let secs = value.parse::<i64>().ok();
    if let Some(secs) = secs {
        return Some(secs.max(0));
    }
    parse_http_date(value)?.checked_sub(i64::try_from(now_secs).unwrap_or(i64::MAX))
}

/// Parse an IMF-fixdate HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`) to
/// UNIX seconds via the days-from-civil algorithm (Howard Hinnant), which
/// is exact for the proleptic Gregorian calendar. The weekday name and the
/// `GMT` zone suffix are validated leniently (any 3-letter weekday, any
/// `GMT`/`UT`+offset spelling of zero). Returns `None` on anything else.
fn parse_http_date(value: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let (weekday, rest) = value.split_once(',')?;
    if weekday.len() != 3 || !weekday.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let mut parts = rest.trim_start().split_ascii_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month_str = parts.next()?;
    let month = MONTHS
        .iter()
        .position(|m| month_str.eq_ignore_ascii_case(m))? as i64;
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let zone = parts.next()?;
    if !zone.eq_ignore_ascii_case("gmt") && !zone.eq_ignore_ascii_case("ut") {
        return None;
    }
    let (hour, minute, second) = {
        let mut t = time.split(':');
        let h: i64 = t.next()?.parse().ok()?;
        let m: i64 = t.next()?.parse().ok()?;
        let s: i64 = t.next()?.parse().ok()?;
        (h, m, s)
    };
    // days_from_civil: counts days since 1970-01-01 for a civil date.
    // `month` is a 0-based index; Hinnant's formula wants the 1-based
    // month folded into March-based years (Jan/Feb roll into the
    // previous year as months 10/11).
    let (y, m) = (
        if month <= 1 { year - 1 } else { year },
        if month <= 1 { month + 10 } else { month - 2 },
    );
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let doy = (153 * m + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Cap error bodies so a chatty backend cannot flood the UI.
pub fn truncate_body(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

impl BackendError {
    pub fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after_ms: None,
        }
    }
}

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
        let prepared = req;
        let result = match body {
            Some(body) => prepared.send_string(body),
            None => prepared.call(),
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

/// Current UNIX time in seconds; used to resolve HTTP-date `Retry-After`
/// values. A missing clock falls back to 0, which turns date-form hints
/// into large delays rather than errors.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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

/// Frame a byte stream of Server-Sent-Events into payloads: one item per
/// event, the joined `data:` lines with the `data:` prefix and a single
/// leading space stripped. Lines with no `data:` field (comments, `event:`,
/// `id:`) are ignored — Anthropic's stream emits `event: content_block_delta`
/// beside each `data:` line. EOF flushes a trailing event that never saw a
/// blank line; a read error is surfaced (after any buffered event).
pub fn sse_payloads(
    reader: impl BufRead + 'static,
) -> impl Iterator<Item = Result<String, BackendError>> {
    SseLines {
        reader,
        data: Vec::new(),
        finished: false,
        pending_error: None,
    }
}

struct SseLines<R: BufRead> {
    reader: R,
    data: Vec<String>,
    finished: bool,
    pending_error: Option<String>,
}

impl<R: BufRead> Iterator for SseLines<R> {
    type Item = Result<String, BackendError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.finished {
                return None;
            }
            if let Some(message) = self.pending_error.take() {
                return Some(Err(BackendError::new(None, message)));
            }
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => {
                    // EOF: flush a trailing event that never saw a blank line.
                    self.finished = true;
                    if self.data.is_empty() {
                        return None;
                    }
                    let payload = std::mem::take(&mut self.data).join("\n");
                    return Some(Ok(payload));
                }
                Ok(_) => {
                    let line = line.trim_end_matches(['\n', '\r']);
                    if line.is_empty() {
                        // End of event: yield accumulated data, if any.
                        if !self.data.is_empty() {
                            let payload = std::mem::take(&mut self.data).join("\n");
                            return Some(Ok(payload));
                        }
                        continue;
                    }
                    if let Some(rest) = line.strip_prefix("data:") {
                        self.data
                            .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
                    }
                    // Other fields (`event:`, `id:`, comments) are ignored.
                }
                Err(source) => {
                    // Surface any buffered event first; the error follows on
                    // the next call (self.finished bounds it to one).
                    self.finished = true;
                    if self.data.is_empty() {
                        return Some(Err(BackendError::new(None, source.to_string())));
                    }
                    let payload = std::mem::take(&mut self.data).join("\n");
                    self.pending_error = Some(source.to_string());
                    return Some(Ok(payload));
                }
            }
        }
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

/// `parse_retry_after` covers both header forms (the same coverage the
/// DeepInfra/LiteLLM backends pin).
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

#[cfg(test)]
mod sse_tests {
    use super::*;
    use std::io::Cursor;

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
