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
    /// Server-provided retry hint in milliseconds (from a `Retry-After`
    /// header, HTTP-date or delay-seconds form). `None` when absent or
    /// unparseable; only meaningful on rate-limit responses.
    pub retry_after_ms: Option<u64>,
}

impl BackendError {
    pub fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            retry_after_ms: None,
        }
    }

    /// Attach a retry hint parsed from a `Retry-After` header value
    /// (delay-seconds or HTTP-date; the date form is measured against
    /// `now_secs`, the current UNIX time).
    pub fn with_retry_after_header(mut self, value: &str, now_secs: u64) -> Self {
        self.retry_after_ms = parse_retry_after(value, now_secs);
        self
    }
}

/// Parse a `Retry-After` header into milliseconds. Accepts the
/// delay-seconds form and the HTTP-date form (RFC 9110 IMF-fixdate);
/// anything else is `None`. Negative/overflowing delays clamp to 0.
fn parse_retry_after(value: &str, now_secs: u64) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let target = if let Ok(secs) = value.parse::<i64>() {
        now_secs.checked_add_signed(secs)?
    } else {
        parse_http_date(value)?
    };
    Some(target.saturating_sub(now_secs) * 1000)
}

/// Parse an IMF-fixdate HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`) to
/// UNIX seconds via the days-from-civil algorithm (Howard Hinnant), which
/// is exact for the proleptic Gregorian calendar. The weekday name and the
/// `GMT` zone suffix are validated leniently (any 3-letter weekday, any
/// `GMT`/`UT`+offset spelling of zero). Returns `None` on anything else.
fn parse_http_date(value: &str) -> Option<u64> {
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
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    u64::try_from(secs).ok()
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

/// Current UNIX time in seconds; used to resolve HTTP-date `Retry-After`
/// values. A missing clock falls back to 0, which turns date-form hints
/// into large delays rather than errors.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_response(response: Result<ureq::Response, ureq::Error>) -> Result<String, BackendError> {
    match response {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| BackendError::new(None, e.to_string())),
        Err(err) => Err(map_ureq_error(err)),
    }
}

fn map_ureq_error(err: ureq::Error) -> BackendError {
    match err {
        ureq::Error::Status(status, resp) => {
            let retry_after = resp.header("retry-after").map(str::to_owned);
            let text = resp.into_string().unwrap_or_default();
            BackendError::new(Some(status), text)
                .with_retry_after_header(retry_after.as_deref().unwrap_or(""), now_secs())
        }
        err => BackendError::new(None, err.to_string()),
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
    fn retry_after_delay_seconds_parses() {
        assert_eq!(parse_retry_after("3", 1_000_000), Some(3_000));
        assert_eq!(parse_retry_after(" 0 ", 1_000_000), Some(0));
        // Negative delay clamps to 0.
        assert_eq!(parse_retry_after("-5", 1_000_000), Some(0));
        assert_eq!(parse_retry_after("", 1_000_000), None);
        assert_eq!(parse_retry_after("soon", 1_000_000), None);
    }

    #[test]
    fn retry_after_http_date_parses() {
        // Classic RFC 9110 example: Sun, 06 Nov 1994 08:49:37 GMT.
        let date = "Sun, 06 Nov 1994 08:49:37 GMT";
        // 1994-11-06T08:49:37Z = 784111777.
        assert_eq!(parse_http_date(date), Some(784_111_777));
        // 30s in the future from that instant -> 30_000 ms.
        assert_eq!(parse_retry_after(date, 784_111_747), Some(30_000));
        // Past date clamps to 0.
        assert_eq!(parse_retry_after(date, 784_111_777), Some(0));
        // Case-insensitive weekday/month, no space after the comma.
        assert_eq!(
            parse_http_date("sun, 06 nov 1994 08:49:37 gmt"),
            Some(784_111_777)
        );
        // Junk is None.
        assert_eq!(parse_http_date("tomorrow"), None);
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 PST"), None);
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
