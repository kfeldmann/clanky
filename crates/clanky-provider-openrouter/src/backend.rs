//! HTTP transport to OpenRouter's OpenAI-compatible API.
//!
//! The mechanism lives in the shared OpenAI-compatible crate
//! ([`clanky_provider_openai::backend`]): the blocking `ureq` agent,
//! `BackendError`, `Retry-After` parsing, the SSE framer and `truncate_body`
//! are the same as every other OpenAI-compatible plugin.
//!
//! What is OpenRouter's own is the *header set*: the optional
//! app-attribution headers (`HTTP-Referer`, `X-OpenRouter-Title`) that this
//! plugin passes through verbatim when the user set them (see
//! [`crate::OpenRouterProvider::from_env`]).

pub use clanky_provider_openai::backend::{Backend, BackendError, UreqBackend, truncate_body};
