//! HTTP transport to the Anthropic Messages API.
//!
//! Kept behind the [`Backend`] trait so provider logic is testable without
//! network access. Unlike the OpenAI-compatible plugins, the bearer path
//! shapes its own headers (spike-confirmed):
//!
//! - `Authorization: Bearer <token>` (never `x-api-key` on the OAuth path)
//! - `anthropic-beta: claude-code-20250219,oauth-2025-04-20`
//! - `anthropic-version: 2023-06-01` (required — the spike's first 400)
//! - `user-agent: claude-cli/2.1.280` + `x-app: cli`

use std::io::BufRead;
use std::time::Duration;

use crate::auth::Credentials;

/// A failure at the HTTP layer: an HTTP status (when the server answered)
/// or a connection/client error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub status: Option<u16>,
    pub message: String,
    /// Server-provided retry hint in milliseconds (from a `Retry-After`
    /// header). `None` when absent or unparseable.
    pub retry_after_ms: Option<u64>,
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
    fn get_model_list(&self, url: &str, creds: &Credentials) -> Result<String, BackendError>;

    /// `POST <url>` with OAuth bearer + beta headers, returning the body.
    /// Non-streaming in M11a (the whole response is read).
    fn post_message(
        &self,
        url: &str,
        creds: &Credentials,
        body: &str,
    ) -> Result<String, BackendError>;

    /// POST a request and stream the response as SSE payloads — one item
    /// per event, the joined `data:` lines with the prefix and a single
    /// leading space stripped. Errors after the stream opens (a dropped
    /// connection) surface on the item stream; errors before it (a non-2xx
    /// status) are the `Err` of this call.
    fn post_stream(
        &self,
        url: &str,
        creds: &Credentials,
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
                let mut err = BackendError::new(Some(status), text);
                if let Some(secs) = retry_after
                    .as_deref()
                    .and_then(|v| v.trim().parse::<i64>().ok())
                {
                    err.retry_after_ms = Some((secs.max(0) as u64) * 1000);
                }
                Err(err)
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
    fn get_model_list(&self, url: &str, creds: &Credentials) -> Result<String, BackendError> {
        self.call(bearer_headers(self.agent.get(url), creds), None)
    }

    fn post_message(
        &self,
        url: &str,
        creds: &Credentials,
        body: &str,
    ) -> Result<String, BackendError> {
        let req = bearer_headers(self.agent.post(url), creds)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json");
        self.call(req, Some(body))
    }

    fn post_stream(
        &self,
        url: &str,
        creds: &Credentials,
        body: &str,
    ) -> Result<Box<dyn Iterator<Item = Result<String, BackendError>>>, BackendError> {
        let req = bearer_headers(self.agent.post(url), creds)
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream");
        let resp = match req.send_string(body) {
            Ok(resp) => resp,
            Err(ureq::Error::Status(status, resp)) => {
                let retry_after = resp.header("retry-after").map(str::to_owned);
                let text = resp.into_string().unwrap_or_default();
                let mut err = BackendError::new(Some(status), text);
                if let Some(secs) = retry_after
                    .as_deref()
                    .and_then(|v| v.trim().parse::<i64>().ok())
                {
                    err.retry_after_ms = Some((secs.max(0) as u64) * 1000);
                }
                return Err(err);
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

/// Attach the OAuth bearer path's headers (the exact strings the spike
/// captured; `Authorization` over `x-api-key`, Claude Code identity).
fn bearer_headers(req: ureq::Request, creds: &Credentials) -> ureq::Request {
    req.set("Authorization", &format!("Bearer {}", creds.access_token))
        .set("anthropic-version", "2023-06-01")
        .set("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
        .set("User-Agent", "claude-cli/2.1.280")
        .set("x-app", "cli")
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
