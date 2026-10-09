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
}

/// Blocking HTTP backend built on `ureq`.
pub struct UreqBackend {
    agent: ureq::Agent,
}

impl UreqBackend {
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout(Duration::from_secs(600))
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
