//! Shared OpenAI-compatible machinery for Clanky provider plugins.
//!
//! Clanky's provider plugins for OpenAI-compatible chat-completions APIs
//! (DeepInfra, LiteLLM, and whatever comes next) share almost all of their
//! mechanism and differ only in policy. This crate holds the mechanism, so a
//! new plugin does not become a fourth copy of it:
//!
//! - [`backend`] — the blocking `ureq` HTTP [`backend::Backend`], its
//!   [`backend::BackendError`], `Retry-After` parsing (delay-seconds and
//!   HTTP-date), the SSE framer, and [`backend::truncate_body`].
//! - [`stream`] — the `OpenAi*` streaming chunk types, the
//!   [`stream::StreamState`] accumulator (tool-call start/args emission,
//!   usage, finish reasons), and [`stream::map_finish_reason`].
//! - [`wire`] — the request-side serializers: [`wire::openai_messages`]
//!   (with a callback for the provider's own reasoning passback),
//!   [`wire::openai_tools`], and their helpers.
//!
//! What stays in each plugin is what is genuinely its own: URL joining,
//! catalog parsing and model hints, error-envelope unwrapping, the
//! thinking→effort ladder, and the documented reasoning passback shape.
//!
//! The Anthropic plugin uses [`backend`]'s SSE framer and `Retry-After`
//! parsing too — the framing rules are the same and its date parser differed
//! only in signedness.

pub mod backend;
pub mod stream;
pub mod wire;
