//! Carrying protocol messages between client and provider.
//!
//! [`Transport`] is the client-side seam. In M1 [`LoopbackTransport`] calls a
//! plugin-side [`Handler`] in-process; in M8 a `ProcessTransport` will speak
//! JSONL over a spawned plugin's stdin/stdout. Everything above this trait
//! (the [`ProviderClient`](crate::ProviderClient) and callers) is transport-
//! agnostic.

use crate::handler::{ChatDone, ChatRequest, Handler};
use crate::messages::{ChunkPayload, Error, Message};
use crate::serve::dispatch_request;

/// Client-side transport: send one request, receive the response messages up
/// to and including the request's terminal message.
///
/// Stream events of an in-flight chat are forwarded to `sink` as they arrive
/// (lazily, for streaming backends); the returned iterator yields the
/// remaining response messages, ending with the terminal `done`/`error`.
/// Only one chat may be in flight per connection (spec §6).
pub trait Transport {
    fn send(
        &mut self,
        msg: Message,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<Box<dyn Iterator<Item = std::result::Result<Message, Error>> + '_>, Error>;

    /// One-way notification (e.g. [`Message::Cancel`]). No response expected.
    fn notify(&mut self, _msg: Message) -> Result<(), Error> {
        Ok(())
    }
}

/// In-process transport that dispatches directly to a [`Handler`].
///
/// This exercises the exact message shapes a process transport will carry:
/// handshake semantics, `id`/`requestId` echo, and request-scoped errors
/// delivered as protocol `error` messages.
pub struct LoopbackTransport<H: Handler> {
    handler: H,
}

impl<H: Handler> LoopbackTransport<H> {
    pub fn new(handler: H) -> Self {
        Self { handler }
    }

    pub fn into_inner(self) -> H {
        self.handler
    }

    fn dispatch(&mut self, msg: Message) -> Result<Vec<Message>, Error> {
        // NOTE: `Message::Chat` never reaches this method; see `Transport::send`.
        // The non-streaming wire rules are shared with the process plugin
        // runtime via `serve::dispatch_request`, so the loopback and a real
        // plugin cannot drift apart.
        match dispatch_request(&mut self.handler, msg)? {
            Some(response) => Ok(vec![response]),
            None => Err(Error::Protocol(
                "unexpected message from client: expected hello/listModels".into(),
            )),
        }
    }
}

impl<H: Handler> Transport for LoopbackTransport<H> {
    fn send(
        &mut self,
        msg: Message,
        sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<Box<dyn Iterator<Item = std::result::Result<Message, Error>> + '_>, Error> {
        // Chat streams straight through the sink; the iterator then yields
        // only the terminal message.
        if let Message::Chat {
            id,
            model,
            messages,
            tools,
            sampling,
            thinking,
        } = msg
        {
            let request = ChatRequest {
                model,
                messages,
                tools,
                sampling,
                thinking,
            };
            return match self.handler.chat(&request, sink) {
                Ok(ChatDone {
                    finish_reason,
                    usage,
                }) => Ok(Box::new(
                    vec![Message::Done {
                        request_id: id,
                        finish_reason,
                        usage,
                    }]
                    .into_iter()
                    .map(Ok),
                )),
                Err(Error::Provider {
                    code,
                    message,
                    retryable,
                    retry_after_ms,
                }) => Ok(Box::new(
                    vec![Message::Error {
                        request_id: Some(id),
                        code,
                        message,
                        retryable,
                        retry_after_ms,
                    }]
                    .into_iter()
                    .map(Ok),
                )),
                Err(other) => Err(other),
            };
        }
        let responses = self.dispatch(msg)?;
        Ok(Box::new(responses.into_iter().map(Ok)))
    }
}
