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
//! `LITELLM_MODEL` optionally names the default model advertised at
//! handshake; the proxy's catalog is arbitrary, so there is no built-in one.

mod backend;
mod provider;

pub use backend::{Backend, BackendError, UreqBackend};
pub use provider::{
    DEFAULT_BASE_URL, ENV_API_KEY, ENV_BASE_URL, ENV_DEFAULT_MODEL, LiteLlmProvider, PROVIDER_NAME,
    default_model_from_env,
};

/// The concrete provider used by Clanky: LiteLLM over a blocking HTTP
/// backend.
pub type LiteLlm = LiteLlmProvider<UreqBackend>;
