//! DeepInfra provider plugin for Clanky.
//!
//! Implements the Clanky provider protocol (`clanky-protocol`) on top of
//! DeepInfra's OpenAI-compatible chat completions API. M1 runs it in-process
//! behind the loopback transport; in M8 the same handler ships behind a
//! spawned process (`clanky-provider-deepinfra` binary).
//!
//! Auth: the provider reads its own credentials from the environment —
//! `DEEPINFRA_API_KEY`, falling back to `DEEPINFRA_TOKEN`. Clanky core never
//! touches provider keys. The base URL defaults to DeepInfra's public
//! endpoint and can be overridden with `DEEPINFRA_URL` (useful for proxies
//! and tests).

mod backend;
mod provider;

pub use backend::{Backend, BackendError, UreqBackend};
pub use provider::{
    DEFAULT_BASE_URL, DEFAULT_MODEL, DeepInfraProvider, ENV_API_KEY, ENV_BASE_URL,
    ENV_TOKEN_FALLBACK, PROVIDER_NAME,
};

/// The concrete provider used by Clanky: DeepInfra over a blocking HTTP
/// backend.
pub type DeepInfra = DeepInfraProvider<UreqBackend>;
