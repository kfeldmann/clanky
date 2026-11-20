//! HTTP transport to the LiteLLM proxy's OpenAI-compatible API.
//!
//! The mechanism lives in the shared OpenAI-compatible crate
//! ([`clanky_provider_openai::backend`]): the blocking `ureq` agent,
//! `BackendError`, `Retry-After` parsing, the SSE framer and `truncate_body`
//! are the same for every OpenAI-compatible plugin. This module re-exports
//! them under the names the LiteLLM plugin uses.
//!
//! [`Backend::get_status`] is part of that shared trait — the LiteLLM
//! provider needs the HTTP status alongside the body so it can degrade
//! gracefully when `/model/info` is forbidden for a key (a virtual key may be
//! scoped to the LLM API routes only).

pub use clanky_provider_openai::backend::{Backend, BackendError, UreqBackend, truncate_body};
