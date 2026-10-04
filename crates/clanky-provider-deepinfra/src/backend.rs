//! HTTP transport to the DeepInfra OpenAI-compatible API.
//!
//! Kept behind the [`Backend`] trait so provider logic is testable without
//! network access.

use std::io::BufRead;
use std::time::Duration;

/// A failure at the HTTP layer: an HTTP status (when the server answered)
/// or a connection/client error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub status: Option<u16>,
    pub message: String,
}

/// Talks to a chat-completions-style HTTP API. Full URLs are passed in by
/// the provider, so a backend can serve any compatible endpoint.
///
/// Chat is streaming-first: requests go through [`Backend::post_stream`],
/// which yields Server-Sent-Events payloads as they arrive; model listing
/// uses plain [`Backend::get`].
pub trait Backend {
    fn get(&self, url: &str) -> Result<String, BackendError>;

    /// POST a request and stream the response as SSE `data:` payloads —
    /// one item per event, including the terminal `[DONE]` sentinel.
    fn post_stream(
        &self,
        url: &str,
        body: &str,
    ) -> Result<Box<dyn Iterator<Item = Result<String, BackendError>>>, BackendError>;
}

/// Blocking HTTP backend built on `ureq`.
pub struct UreqBackend {
    agent: ureq::Agent,
    api_key: String,
}

impl UreqBackend {
    pub fn new(api_key: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
            .build();
        Self {
            agent,
            api_key: api_key.into(),
        }
    }
}

impl Backend for UreqBackend {
    fn get(&self, url: &str) -> Result<String, BackendError> {
        let call = self
            .agent
            .get(url)
            .set("Authorization", &format!("Bearer {}", self.api_key));
        read_response(call.call())
    }

    fn post_stream(
        &self,
        url: &str,
        body: &str,
    ) -> Result<Box<dyn Iterator<Item = Result<String, BackendError>>>, BackendError> {
        let call = self
            .agent
            .post(url)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .set("Accept", "text/event-stream");
        match call.send_string(body) {
            Ok(resp) => Ok(Box::new(sse_payloads(std::io::BufReader::new(
                resp.into_reader(),
            )))),
            Err(err) => Err(map_ureq_error(err)),
        }
    }
}

fn read_response(response: Result<ureq::Response, ureq::Error>) -> Result<String, BackendError> {
    match response {
        Ok(resp) => resp.into_string().map_err(|e| BackendError {
            status: None,
            message: e.to_string(),
        }),
        Err(err) => Err(map_ureq_error(err)),
    }
}

fn map_ureq_error(err: ureq::Error) -> BackendError {
    match err {
        ureq::Error::Status(status, resp) => {
            let text = resp.into_string().unwrap_or_default();
            BackendError {
                status: Some(status),
                message: text,
            }
        }
        err => BackendError {
            status: None,
            message: err.to_string(),
        },
    }
}

/// Frame a byte stream of Server-Sent-Events into payloads: one item per
/// event, containing the joined `data:` lines with the `data:` prefix and a
/// single leading space stripped. Lines before the first blank line with no
/// `data:` field (comments, `event:`/`id:` fields) are ignored. The final
/// `[DONE]` sentinel is passed through to the caller.
///
/// This is the only place SSE framing is handled: providers receive plain
/// payload strings.
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
                return Some(Err(BackendError {
                    status: None,
                    message,
                }));
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
                        return Some(Err(BackendError {
                            status: None,
                            message: source.to_string(),
                        }));
                    }
                    let payload = std::mem::take(&mut self.data).join("\n");
                    self.pending_error = Some(source.to_string());
                    return Some(Ok(payload));
                }
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn payloads(text: &str) -> Vec<Result<String, BackendError>> {
        sse_payloads(Cursor::new(text.to_string())).collect()
    }

    #[test]
    fn frames_single_data_events() {
        let items = payloads(
            "data: {\"a\":1}\n\n\
             data: [DONE]\n\n",
        );
        let payloads: Vec<String> = items.into_iter().map(Result::unwrap).collect();
        assert_eq!(payloads, ["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn strips_single_leading_space_but_not_more() {
        let items = payloads("data:  two\n\ndata:x\n\n");
        let payloads: Vec<String> = items.into_iter().map(Result::unwrap).collect();
        assert_eq!(payloads, [" two", "x"]);
    }

    #[test]
    fn ignores_comments_and_other_fields() {
        let items = payloads(
            ": keep-alive comment\n\
             event: delta\n\
             data: {\"x\":true}\n\n",
        );
        let payloads: Vec<String> = items.into_iter().map(Result::unwrap).collect();
        assert_eq!(payloads, ["{\"x\":true}"]);
    }

    #[test]
    fn joins_multiline_data_with_newlines() {
        let items = payloads("data: line1\ndata: line2\n\n");
        let payloads: Vec<String> = items.into_iter().map(Result::unwrap).collect();
        assert_eq!(payloads, ["line1\nline2"]);
    }

    #[test]
    fn handles_crlf_and_missing_trailing_blank_line() {
        let items = payloads("data: a\r\n\r\ndata: b\r\n");
        let payloads: Vec<String> = items.into_iter().map(Result::unwrap).collect();
        assert_eq!(payloads, ["a", "b"]);
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
