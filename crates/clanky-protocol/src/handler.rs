//! Plugin-side interface: what a provider must implement.
//!
//! A provider exposes its identity ([`PluginInfo`]), model listing, and chat
//! generation. `chat` is streaming-first (spec design goal 2): the provider
//! emits [`ChunkPayload`] events to a sink as they are produced; backends
//! that cannot stream emit all events right before returning. The transport
//! layer adapts this to protocol messages; in M8 the same trait sits behind a
//! spawned plugin process (see [`crate::serve`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::messages::ChatMessage;
use crate::messages::{
    Capabilities, ChunkPayload, Error, FinishReason, ModelInfo, Sampling, Thinking, Tool, Usage,
};

/// Provider identity and capability flags, reported at handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub name: String,
    pub capabilities: Capabilities,
    /// Provider-recommended default model, advertised in the `hello`
    /// message (spec §3, additive v1 field). `None` means "no built-in
    /// default; require explicit configuration". Core uses this instead of
    /// a hardcoded per-provider table.
    pub default_model: Option<String>,
}

/// A shared cancellation flag for an in-flight `chat` (spec §7).
///
/// The plugin runtime owns one per connection and hands it to the handler
/// ([`Handler::set_cancel_flag`]); the handler's blocking read loop checks
/// [`CancelFlag::is_cancelled`] and returns [`FinishReason::Cancelled`]
/// promptly. A handler that cannot check it (e.g. blocked in a single
/// uninterruptible read) is covered by the client's kill fallback (§7).
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    /// Ask the in-flight chat to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether a cancel was requested since the last [`CancelFlag::reset`].
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Clear the flag before a new chat starts.
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
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

    /// Receive the connection's cancellation flag (spec §7). Called once by
    /// the plugin runtime before serving; a handler that can check it should
    /// store it and return [`FinishReason::Cancelled`] promptly when it is
    /// set. The default is a no-op, so in-process handlers that never cancel
    /// need not care.
    fn set_cancel_flag(&mut self, flag: CancelFlag) {
        let _ = flag;
    }
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

    fn set_cancel_flag(&mut self, flag: CancelFlag) {
        (**self).set_cancel_flag(flag);
    }
}
