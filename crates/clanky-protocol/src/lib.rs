//! Clanky provider plugin protocol, version 1.
//!
//! This crate defines the message shapes exchanged between Clanky and
//! provider plugins (see `provider-protocol.md`), a [`Transport`] trait that
//! carries them, and a [`ProviderClient`] that drives a transport through
//! handshake, `listModels`, and streaming chat turns.
//!
//! Two transports implement the trait:
//! - [`transport::LoopbackTransport`] calls a plugin-side [`Handler`]
//!   in-process (tests, and the M1-shaped path).
//! - [`process::ProcessTransport`] (M8) spawns a `clanky-provider-*` plugin
//!   as a subprocess and speaks JSONL over stdio, adding lifecycle (spawn,
//!   crash isolation, cancel fallback).
//!
//! The plugin side has a matching runtime: [`serve::serve`] reads JSONL
//! requests, dispatches them to a [`Handler`], and writes JSONL responses —
//! the mirror of [`ProviderClient`]. Wire conventions:
//! - newline-delimited JSON, one message per line
//! - every message has a `"type"` field; message and field names are camelCase
//! - consumers must ignore unknown message types and unknown fields
//!   (`parse_message` implements this for the wire path)

pub mod client;
pub mod handler;
pub mod messages;
pub mod process;
pub mod serve;
pub mod stream;
pub mod transport;

pub use client::ProviderClient;
pub use handler::{CancelFlag, ChatDone, ChatRequest, Handler, PluginInfo};
pub use messages::{
    Capabilities, ChatMessage, ChunkPayload, Error, ErrorCode, FinishReason, Message, ModelInfo,
    PROTOCOL_VERSION, Sampling, Thinking, Tool, ToolCall, Usage, parse_message,
};
pub use process::{CANCEL_TIMEOUT, CancelHandle, ProcessOptions, ProcessTransport};
pub use serve::{serve, serve_stdio};
pub use stream::StreamAssembler;
pub use transport::{LoopbackTransport, Transport};
