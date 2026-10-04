//! Plugin-side interface: what a provider must implement.
//!
//! A provider exposes its identity ([`PluginInfo`]), model listing, and chat
//! generation. `chat` is streaming-first (spec design goal 2): the provider
//! emits [`ChunkPayload`] events to a sink as they are produced; backends
//! that cannot stream emit all events right before returning. The transport
//! layer adapts this to protocol messages; in M8 the same trait sits behind a
//! spawned plugin process.

use crate::messages::ChatMessage;
use crate::messages::{
    Capabilities, ChunkPayload, Error, FinishReason, ModelInfo, Sampling, Thinking, Tool, Usage,
};

/// Provider identity and capability flags, reported at handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub name: String,
    pub capabilities: Capabilities,
}

/// A chat request, exactly the payload of a protocol `chat` message
/// (the client adds the `id`).
#[derive(Debug, Clone, PartialEq)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<Tool>>,
    pub sampling: Option<Sampling>,
    pub thinking: Option<Thinking>,
}

/// Terminal status of a chat turn: the payload of a protocol `done` message.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatDone {
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
}

/// Implemented by providers. Errors of kind [`Error::Provider`] are
/// request-scoped (delivered to the client as a protocol `error` message);
/// any other error aborts the connection.
pub trait Handler {
    fn info(&self) -> PluginInfo;

    fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error>;

    /// Run one chat turn, emitting stream events to `sink` as they are
    /// produced. The return value is the terminal status; events delivered
    /// via `sink` before an `Err` return are simply the stream so far.
    fn chat(
        &mut self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<ChatDone, Error>;
}

impl Handler for Box<dyn Handler> {
    fn info(&self) -> PluginInfo {
        (**self).info()
    }

    fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
        (**self).list_models()
    }

    fn chat(
        &mut self,
        request: &ChatRequest,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<ChatDone, Error> {
        (**self).chat(request, sink)
    }
}
