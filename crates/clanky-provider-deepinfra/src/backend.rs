//! HTTP transport to the DeepInfra OpenAI-compatible API.
//!
//! The mechanism lives in the shared OpenAI-compatible crate
//! ([`clanky_provider_openai::backend`]) — the blocking `ureq` agent,
//! `BackendError`, `Retry-After` parsing in both forms, the SSE framer and
//! `truncate_body` are the same for every OpenAI-compatible plugin. This
//! module re-exports the pieces the DeepInfra plugin uses, so its own code
//! reads as it did before the extraction; nothing DeepInfra-specific is left
//! in the transport layer.

pub use clanky_provider_openai::backend::{Backend, BackendError, UreqBackend, truncate_body};
