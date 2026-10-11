//! LiteLLM proxy provider plugin for Clanky.
//!
//! Implements the Clanky provider protocol (`clanky-protocol`) on top of a
//! LiteLLM proxy's OpenAI-compatible chat completions API. The library holds
//! the provider logic (testable in-process); the `clanky-provider-litellm`
//! binary serves it as a plugin process over stdin/stdout.
//!
//! Auth: the provider reads its own credentials from the environment —
//! `LITELLM_API_KEY`. Clanky core never touches provider keys. The base URL
//! comes from `LITELLM_BASE_URL` and defaults to `http://localhost:4000`.
//! The built-in default model advertised at handshake is
//! `claude-haiku-4-5`.

mod backend;
mod provider;

pub use backend::{Backend, BackendError, UreqBackend};
pub use provider::{
    DEFAULT_BASE_URL, DEFAULT_MODEL, ENV_API_KEY, ENV_BASE_URL, LiteLlmProvider, PROVIDER_NAME,
};

/// The concrete provider used by Clanky: LiteLLM over a blocking HTTP
/// backend.
pub type LiteLlm = LiteLlmProvider<UreqBackend>;
