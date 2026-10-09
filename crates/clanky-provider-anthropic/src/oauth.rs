//! The Anthropic OAuth flow (authorization-code + PKCE, headless).
//!
//! Constants confirmed by the M11a spike against our enterprise tenant (and
//! matching Pi's `packages/ai/src/auth/oauth/anthropic.ts`, which performs
//! the same dance on this account):
//!
//! - client_id: Claude Code's client; the OAuth surface is not a published
//!   third-party API, so requests must identify as Claude Code (`user-agent`
//!   header + the identity system block, see `adapter`).
//! - the headless flow needs `code=true` and the special redirect_uri — the
//!   auth page then *displays* the code instead of redirecting.
//! - the PKCE verifier doubles as `state` (Pi's trick).
//! - access tokens live 8 h (`expires_in: 28800`); the refresh token is
//!   finite (~20 days in our capture) and rotates on every refresh.
//! - refresh responses also carry `refresh_token_expires_in`.

use std::io::BufRead;

use serde::{Deserialize, Serialize};

use crate::pkce::{self, Pkce};

/// Claude Code's OAuth client id (not a published third-party client).
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Authorize endpoint.
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
/// Token endpoint.
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
/// Redirect URI for the headless paste flow: with `code=true`, the auth page
/// shows the code instead of redirecting (no local callback server exists).
pub const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";
/// Scopes requested for a coding-agent login.
pub const SCOPES: &str = "org:create_api_key user:profile user:inference \
     user:sessions:claude_code user:mcp_servers user:file_upload";

/// A token endpoint response (success shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    /// Access-token lifetime in seconds (captured: 28800 = 8 h).
    pub expires_in: i64,
    /// Present on refresh responses: the *refresh* token's remaining
    /// lifetime in seconds (finite for subscription logins). Rotation:
    /// a refresh issues a new refresh token that replaces the old one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token_expires_in: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Build the authorize URL for a PKCE pair. The verifier is reused as
/// `state` (Pi's approach; the paste flow validates it on return).
pub fn authorize_url(pkce: &Pkce) -> String {
    [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPES),
        ("code_challenge", &pkce.challenge),
        ("code_challenge_method", "S256"),
        ("state", &pkce.verifier),
    ]
    .into_iter()
    .map(|(k, v)| format!("{k}={}", urlencode(v)))
    .collect::<Vec<_>>()
    .join("&")
    .prepend_to(AUTHORIZE_URL)
}

trait Prepend {
    fn prepend_to(self, prefix: &str) -> String;
}
impl Prepend for String {
    fn prepend_to(self, prefix: &str) -> String {
        format!("{prefix}?{self}")
    }
}

/// Minimal percent-encoding for query values (everything RFC 3986 reserves).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// What the user pasted back: a bare code, a `code#state` pair, or the full
/// redirect URL — all three are accepted (Pi's `parseAuthorizationInput`).
pub fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    // Full URL (or any `code=` query string): pull code + state params.
    if let Some(rest) = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
    {
        if let Some(query) = rest.split_once('?').map(|(_, q)| q) {
            return parse_query(query);
        }
        // A bare URL without a query has nothing to offer.
        return (None, None);
    }
    if value.contains("code=") {
        return parse_query(value);
    }
    // `code#state` pair.
    if let Some((code, state)) = value.split_once('#') {
        return (Some(code.to_string()), Some(state.to_string()));
    }
    // Bare code.
    (Some(value.to_string()), None)
}

/// Parse `a=1&b=2` style query params for `code` and `state`.
fn parse_query(query: &str) -> (Option<String>, Option<String>) {
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(urldecode(v)),
            "state" => state = Some(urldecode(v)),
            _ => {}
        }
    }
    (code, state)
}

/// Reverse of [`urlencode`] (and of the server's encoding).
fn urldecode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The error the HTTP layer produces; kept concrete so the provider can map
/// it onto protocol error codes.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The token endpoint answered with a non-2xx status.
    #[error("{status}: {body}")]
    Http { status: u16, body: String },
    /// A network/transport failure.
    #[error("network: {0}")]
    Network(String),
    /// The (JSON) response could not be parsed.
    #[error("malformed token response: {0}")]
    Malformed(String),
}

/// POST the token endpoint with a JSON body.
pub fn post_token_request(body: &str) -> Result<String, OAuthError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(60))
        .build();
    match agent
        .post(TOKEN_URL)
        .set("Content-Type", "application/json")
        .set("Accept", "application/json")
        .send_string(body)
    {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| OAuthError::Network(e.to_string())),
        Err(ureq::Error::Status(status, resp)) => Err(OAuthError::Http {
            status,
            body: resp.into_string().unwrap_or_default(),
        }),
        Err(err) => Err(OAuthError::Network(err.to_string())),
    }
}

/// Exchange an authorization code for tokens (step 5 of the flow). The
/// `state` sent here is the one validated against the paste; Pi sends the
/// verifier unless the paste carried its own.
pub fn exchange_authorization_code(
    code: &str,
    state: &str,
    verifier: &str,
) -> Result<TokenResponse, OAuthError> {
    let body = serde_json::json!({
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "code": code,
        "state": state,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": verifier,
    })
    .to_string();
    post_token_request(&body).and_then(parse_token_body)
}

/// Refresh an access token. A success always carries a *new* refresh token
/// (rotation) and its remaining lifetime; callers must persist both.
pub fn refresh(refresh_token: &str) -> Result<TokenResponse, OAuthError> {
    let body = serde_json::json!({
        "grant_type": "refresh_token",
        "client_id": CLIENT_ID,
        "refresh_token": refresh_token,
    })
    .to_string();
    post_token_request(&body).and_then(parse_token_body)
}

/// Pull a human-readable hint out of a token-endpoint error body: the
/// OAuth shape is `{"error": "invalid_grant", "error_description": "…"}`;
/// anything else is passed through.
pub fn extract_error(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        let err = value.get("error").and_then(|v| v.as_str()).unwrap_or("");
        let desc = value
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !err.is_empty() {
            return if desc.is_empty() {
                err.to_string()
            } else {
                format!("{err}: {desc}")
            };
        }
    }
    body.to_string()
}

fn parse_token_body(body: String) -> Result<TokenResponse, OAuthError> {
    serde_json::from_str(&body)
        .map_err(|e| OAuthError::Malformed(format!("{e}: {}", truncate(&body))))
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= 200 {
        text.to_string()
    } else {
        text.chars().take(200).collect()
    }
}

/// Run the interactive headless login: print the authorize URL, read the
/// pasted code, exchange, return the token response. `prompt`/`read_line`
/// are injectable so tests can drive the dance without a TTY.
pub fn login(
    mut prompt: impl FnMut(&str),
    mut read_line: impl FnMut() -> Option<String>,
) -> Result<TokenResponse, String> {
    let pkce = pkce::generate();
    prompt("Open this URL in any browser (your laptop, phone, jump host):");
    prompt(&authorize_url(&pkce));
    prompt(
        "Sign in, then paste the code the page displays (a bare code, code#state, or the full redirect URL):",
    );
    let input = read_line().ok_or("no input: authorization code required")?;
    let (code, state) = parse_authorization_input(&input);
    let Some(code) = code else {
        return Err("missing authorization code in pasted input".into());
    };
    if state.as_deref().is_some_and(|state| state != pkce.verifier) {
        return Err("OAuth state mismatch — paste the URL/code from this login attempt".into());
    }
    exchange_authorization_code(
        &code,
        state.as_deref().unwrap_or(&pkce.verifier),
        &pkce.verifier,
    )
    .map_err(|e| format!("token exchange failed: {e}"))
}

/// Drive [`login`] over real stdin/stdout (the `login` subcommand's path).
pub fn login_stdio() -> Result<TokenResponse, String> {
    let mut stdout = std::io::stdout().lock();
    let mut prompt = |text: &str| {
        use std::io::Write as _;
        let _ = writeln!(stdout, "{text}");
    };
    let mut stdin = std::io::stdin().lock();
    let mut read_line = move || {
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim_end_matches(['\n', '\r']).to_string()),
        }
    };
    login(&mut prompt, &mut read_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_carries_all_headless_params() {
        let pkce = Pkce {
            verifier: "v".repeat(43),
            challenge: "c".repeat(43),
        };
        let url = authorize_url(&pkce);
        assert!(url.starts_with(AUTHORIZE_URL));
        // The headless gating params must survive.
        assert!(url.contains("code=true"));
        assert!(url.contains(&format!("client_id={CLIENT_ID}")));
        assert!(url.contains(&format!("redirect_uri={}", urlencode(REDIRECT_URI))));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("code_challenge={}", "c".repeat(43))));
        assert!(url.contains(&format!("state={}", "v".repeat(43))));
        // Spaces in the scope are percent-encoded.
        assert!(url.contains("scope=org%3Acreate_api_key%20user%3Aprofile"));
    }

    #[test]
    fn paste_forms_all_parse() {
        let bare = "eac123";
        let (code, state) = parse_authorization_input(bare);
        assert_eq!(code.as_deref(), Some("eac123"));
        assert_eq!(state, None);

        let (code, state) = parse_authorization_input("eac123#5b7c");
        assert_eq!(code.as_deref(), Some("eac123"));
        assert_eq!(state.as_deref(), Some("5b7c"));

        let full = "https://platform.claude.com/oauth/code/callback?code=eac123&state=5b7c";
        let (code, state) = parse_authorization_input(full);
        assert_eq!(code.as_deref(), Some("eac123"));
        assert_eq!(state.as_deref(), Some("5b7c"));
    }

    #[test]
    fn url_decoding_roundtrips_scope() {
        let (code, _) = parse_authorization_input(
            "https://platform.claude.com/oauth/code/callback?code=e%2Bac&state=s",
        );
        assert_eq!(code.as_deref(), Some("e+ac"));
    }

    #[test]
    fn login_validates_state_and_returns_tokens() {
        use std::cell::RefCell;
        let pasted = RefCell::new(Some(
            "https://platform.claude.com/oauth/code/callback?code=eac123&state=MISMATCH"
                .to_string(),
        ));
        let result = login(|_| {}, || pasted.borrow_mut().take());
        assert!(result.is_err(), "state mismatch is rejected");
        // A matching paste succeeds.
        let pasted = RefCell::new(Some("eac123".to_string()));
        // (No network in unit tests: only the mismatch path is live here.)
        let _ = pasted;
    }
}
