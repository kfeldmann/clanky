//! OpenRouter provider plugin for Clanky.
//!
//! Implements the Clanky provider protocol (`clanky-protocol`) on top of
//! OpenRouter's OpenAI-compatible chat completions API. The library holds the
//! provider logic (testable in-process); the `clanky-provider-openrouter`
//! binary serves it as a plugin process over stdin/stdout.
//!
//! Auth: the provider reads its own credentials from the environment —
//! `OPENROUTER_API_KEY`. Clanky core never touches provider keys. The base URL
//! comes from `OPENROUTER_BASE_URL` and defaults to
//! `https://openrouter.ai/api/v1`; the optional app-attribution headers from
//! `OPENROUTER_HTTP_REFERER` / `OPENROUTER_TITLE`, sent verbatim when set and
//! not at all when unset. The built-in default model advertised at handshake
//! is `deepseek/deepseek-v4.1-flash`.

mod backend;
mod provider;

pub use backend::{Backend, BackendError, UreqBackend};
pub use provider::{
    DEFAULT_BASE_URL, DEFAULT_MODEL, ENV_API_KEY, ENV_BASE_URL, ENV_HTTP_REFERER, ENV_TITLE,
    OpenRouterProvider, PROVIDER_NAME,
};

/// The concrete provider used by Clanky: OpenRouter over a blocking HTTP
/// backend.
pub type OpenRouter = OpenRouterProvider<UreqBackend>;
