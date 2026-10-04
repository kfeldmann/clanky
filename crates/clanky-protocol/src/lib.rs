//! Clanky provider plugin protocol, version 1.
//!
//! This crate defines the message shapes exchanged between Clanky and
//! provider plugins (see `provider-protocol.md`), a [`Transport`] trait that
//! carries them, and a [`ProviderClient`] that drives a transport through
//! handshake, `listModels`, and streaming chat turns.
//!
//! In M1 everything runs in-process: [`transport::LoopbackTransport`] calls a
//! plugin-side [`Handler`] directly. The M8 process transport speaks the same
//! message shapes as JSONL over stdin/stdout; client code does not change.
//! Wire conventions:
//! - newline-delimited JSON, one message per line
//! - every message has a `"type"` field; message and field names are camelCase
//! - consumers must ignore unknown message types and unknown fields
//!   (`parse_message` implements this for the wire path)

pub mod client;
pub mod handler;
pub mod messages;
pub mod stream;
pub mod transport;

pub use client::ProviderClient;
pub use handler::{ChatDone, ChatRequest, Handler, PluginInfo};
pub use messages::{
    Capabilities, ChatMessage, ChunkPayload, Error, ErrorCode, FinishReason, Message, ModelInfo,
    PROTOCOL_VERSION, Sampling, Thinking, Tool, ToolCall, Usage, parse_message,
};
pub use stream::StreamAssembler;
pub use transport::{LoopbackTransport, Transport};
