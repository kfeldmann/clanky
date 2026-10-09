//! Anthropic provider plugin for Clanky (OAuth-authenticated).
//!
//! Implements the Clanky provider protocol (`clanky-protocol`) on top of
//! Anthropic's Messages API, authenticated exclusively by OAuth (our
//! enterprise tenant issues no API keys). The library holds the provider
//! logic (testable in-process); the `clanky-provider-anthropic` binary
//! serves it as a plugin process over stdin/stdout.
//!
//! Auth stays here, never in core (protocol §1): credentials live on disk
//! under `~/.clanky/providers/anthropic/`, written by the headless `login`
//! subcommand (PKCE + paste, no browser on this machine), and are refreshed
//! automatically before expiry.

mod adapter;
mod auth;
mod backend;
mod oauth;
mod pkce;
mod provider;

pub use auth::{Credentials, credentials_path, load, store};
pub use backend::{Backend, BackendError, UreqBackend};
pub use oauth::{
    AUTHORIZE_URL, CLIENT_ID, REDIRECT_URI, SCOPES, TOKEN_URL, TokenResponse, authorize_url,
    exchange_authorization_code, parse_authorization_input, refresh,
};
pub use pkce::{Pkce, challenge_of, generate as generate_pkce};
pub use provider::{
    ANTHROPIC_VERSION, AnthropicProvider, CLAUDE_CLI_VERSION, DEFAULT_BASE_URL, OAUTH_BETA,
    PROVIDER_NAME,
};

/// The concrete provider used by Clanky: Anthropic over a blocking HTTP
/// backend.
pub type Anthropic = AnthropicProvider<UreqBackend>;

/// Run the headless login over stdio and store the resulting credentials.
/// Used by the `login` subcommand; kept in the library so it is testable.
pub fn login_and_store() -> Result<(), String> {
    let response = oauth::login_stdio()?;
    let creds = Credentials {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at: auth::expiry_from(response.expires_in),
        refresh_token_expires_at: response
            .refresh_token_expires_in
            .map(|s| auth::now_ms() + s * 1000),
    };
    auth::store(&creds).map_err(|e| format!("cannot store credentials: {e}"))
}
