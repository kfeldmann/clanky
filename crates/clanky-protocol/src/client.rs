//! Client-side driver: handshake, `listModels`, streaming chat.
//!
//! The client is generic over [`Transport`], so the same code drives the M1
//! loopback and the M8 process transport unchanged.

use crate::handler::{ChatDone, ChatRequest, PluginInfo};
use crate::messages::{ChunkPayload, Error, Message, ModelInfo, PROTOCOL_VERSION};
use crate::transport::Transport;

/// Drives one provider connection.
#[derive(Debug)]
pub struct ProviderClient<T: Transport> {
    transport: T,
    next_id: u64,
    peer: Option<PluginInfo>,
}

impl<T: Transport> ProviderClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next_id: 1,
            peer: None,
        }
    }

    pub fn peer(&self) -> Option<&PluginInfo> {
        self.peer.as_ref()
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Borrow the underlying transport (e.g. to reach a plugin process's
    /// [`CancelHandle`](crate::process::CancelHandle)).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Perform the handshake (or return the cached result of a prior one).
    /// Must succeed before any other call.
    pub fn handshake(&mut self) -> Result<&PluginInfo, Error> {
        if self.peer.is_none() {
            self.perform_handshake()?;
        }
        self.peer
            .as_ref()
            .ok_or_else(|| Error::Protocol("handshake did not record peer identity".into()))
    }

    fn perform_handshake(&mut self) -> Result<(), Error> {
        let hello = Message::Hello {
            protocol_version: PROTOCOL_VERSION,
            name: None,
            capabilities: None,
            default_model: None,
        };
        let mut no_chunks = |_: ChunkPayload| {};
        let mut responses = self.transport.send(hello, &mut no_chunks)?;
        match responses.next() {
            Some(Ok(Message::Hello {
                protocol_version,
                name,
                capabilities,
                default_model,
            })) => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(Error::VersionMismatch {
                        peer: protocol_version,
                    });
                }
                self.peer = Some(PluginInfo {
                    name: name.unwrap_or_default(),
                    capabilities: capabilities.unwrap_or_default(),
                    default_model,
                });
                Ok(())
            }
            Some(Ok(other)) => Err(Error::Protocol(format!("expected `hello`, got {other:?}"))),
            Some(Err(err)) => Err(err),
            None => Err(Error::Protocol("connection closed before handshake".into())),
        }
    }

    /// Enumerate the provider's models. Requires the `listModels` capability;
    /// otherwise models come only from settings.
    pub fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
        self.require_capability(|c| c.list_models, "listModels")?;
        let id = self.alloc_id();
        let mut no_chunks = |_: ChunkPayload| {};
        let mut responses = self
            .transport
            .send(Message::ListModels { id }, &mut no_chunks)?;
        match responses.next() {
            Some(Ok(Message::Models { id: rid, models })) if rid == id => Ok(models),
            Some(Ok(Message::Error {
                request_id: Some(rid),
                code,
                message,
                retryable,
                retry_after_ms,
            })) if rid == id => Err(Error::Provider {
                code,
                message,
                retryable,
                retry_after_ms,
            }),
            Some(Ok(other)) => Err(Error::Protocol(format!("expected `models`, got {other:?}"))),
            Some(Err(err)) => Err(err),
            None => Err(Error::Protocol("connection closed mid-request".into())),
        }
    }

    /// Run one chat turn, dispatching every stream chunk to `on_event` as it
    /// arrives. Returns the terminal status (`done` payload).
    pub fn chat(
        &mut self,
        request: ChatRequest,
        on_event: &mut dyn FnMut(&ChunkPayload),
    ) -> Result<ChatDone, Error> {
        let id = self.alloc_id();
        let msg = Message::Chat {
            id,
            model: request.model,
            messages: request.messages,
            tools: request.tools,
            sampling: request.sampling,
            thinking: request.thinking,
        };
        let mut sink = |payload: ChunkPayload| on_event(&payload);
        let responses = self.transport.send(msg, &mut sink)?;
        for resp in responses {
            match resp? {
                // A transport may deliver stream events either through the
                // `sink` (loopback, in-process) or as `chunk` messages in
                // the response iterator (process transport); both are
                // forwarded here, exactly once.
                Message::Chunk {
                    request_id,
                    payload,
                } if request_id == id => on_event(&payload),
                Message::Chunk { request_id, .. } => {
                    return Err(Error::Protocol(format!(
                        "chunk for request {request_id} while {id} is in flight"
                    )));
                }
                Message::Done {
                    request_id,
                    finish_reason,
                    usage,
                } if request_id == id => {
                    return Ok(ChatDone {
                        finish_reason,
                        usage,
                    });
                }
                Message::Done { request_id, .. } => {
                    return Err(Error::Protocol(format!(
                        "`done` for request {request_id} while {id} is in flight"
                    )));
                }
                Message::Error {
                    request_id: Some(rid),
                    code,
                    message,
                    retryable,
                    retry_after_ms,
                } if rid == id => {
                    return Err(Error::Provider {
                        code,
                        message,
                        retryable,
                        retry_after_ms,
                    });
                }
                Message::Error {
                    request_id: None,
                    code,
                    message,
                    ..
                } => {
                    return Err(Error::Protocol(format!(
                        "connection-fatal error ({code}): {message}"
                    )));
                }
                Message::Error {
                    request_id: Some(rid),
                    code,
                    message,
                    ..
                } => {
                    return Err(Error::Protocol(format!(
                        "error for request {rid} while {id} is in flight ({code}): {message}"
                    )));
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "unexpected message during chat: {other:?}"
                    )));
                }
            }
        }
        Err(Error::Protocol(
            "stream ended without a terminal message".into(),
        ))
    }

    /// Advise the provider to stop an in-flight chat (spec §7). Advisory
    /// only; the terminal `done` still arrives (or the transport dies).
    pub fn cancel(&mut self, request_id: u64) -> Result<(), Error> {
        self.transport.notify(Message::Cancel { request_id })
    }

    fn require_capability(
        &self,
        flag: impl Fn(&crate::messages::Capabilities) -> bool,
        name: &str,
    ) -> Result<(), Error> {
        let info = self
            .peer
            .as_ref()
            .ok_or_else(|| Error::Protocol("handshake not performed".into()))?;
        if flag(&info.capabilities) {
            Ok(())
        } else {
            Err(Error::Protocol(format!(
                "provider `{}` does not support {name}",
                info.name
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{ChatDone, Handler};
    use crate::messages::{
        Capabilities, ChatMessage, ChunkPayload, ErrorCode, FinishReason, Usage,
    };
    use std::cell::RefCell;

    /// A canned plugin-side handler for exercising the client.
    struct FakePlugin {
        name: &'static str,
        caps: Capabilities,
        fail: Option<ErrorCode>,
        chats: RefCell<Vec<ChatRequest>>,
    }

    impl FakePlugin {
        fn new() -> Self {
            Self {
                name: "fake",
                caps: Capabilities {
                    list_models: true,
                    thinking: true,
                    tools: false,
                },
                fail: None,
                chats: RefCell::new(Vec::new()),
            }
        }
    }

    impl Handler for FakePlugin {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: self.name.into(),
                capabilities: self.caps,
                default_model: None,
            }
        }

        fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
            if let Some(code) = self.fail {
                return Err(Error::provider(code, "nope", false));
            }
            Ok(vec![ModelInfo {
                id: "fake/m".into(),
                display_name: None,
                context_window: Some(128_000),
                supports_thinking: Some(true),
                supports_text_generation: None,
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_read_price_per_mtok: None,
            }])
        }

        fn chat(
            &mut self,
            request: &ChatRequest,
            sink: &mut dyn FnMut(ChunkPayload),
        ) -> Result<ChatDone, Error> {
            self.chats.borrow_mut().push(request.clone());
            if let Some(code) = self.fail {
                return Err(Error::provider(code, "denied", false));
            }
            for chunk in [
                ChunkPayload::Thinking {
                    text: "hmm ".into(),
                },
                ChunkPayload::Text { text: "Hi".into() },
                ChunkPayload::Text {
                    text: " there".into(),
                },
            ] {
                sink(chunk);
            }
            Ok(ChatDone {
                finish_reason: FinishReason::Stop,
                usage: Some(Usage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(3),
                    cached_tokens: None,
                }),
            })
        }
    }

    fn request(model: &str, prompt: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![ChatMessage::user(prompt)],
            tools: None,
            sampling: None,
            thinking: None,
        }
    }

    #[test]
    fn handshake_captures_identity_and_capabilities() {
        let mut client =
            ProviderClient::new(crate::transport::LoopbackTransport::new(FakePlugin::new()));
        let peer = client.handshake().unwrap();
        assert_eq!(peer.name, "fake");
        assert!(peer.capabilities.list_models);
        assert!(!peer.capabilities.tools);
    }

    #[test]
    fn list_models_requires_capability_and_returns_models() {
        let mut client =
            ProviderClient::new(crate::transport::LoopbackTransport::new(FakePlugin::new()));
        client.handshake().unwrap();
        let models = client.list_models().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "fake/m");

        let mut limited = FakePlugin::new();
        limited.caps = Capabilities::default();
        let mut client = ProviderClient::new(crate::transport::LoopbackTransport::new(limited));
        client.handshake().unwrap();
        let err = client.list_models().unwrap_err();
        assert!(err.to_string().contains("does not support listModels"));
    }

    #[test]
    fn chat_streams_chunks_and_returns_done() {
        let mut client =
            ProviderClient::new(crate::transport::LoopbackTransport::new(FakePlugin::new()));
        client.handshake().unwrap();
        let mut events: Vec<String> = Vec::new();
        let done = client
            .chat(request("fake/m", "hi"), &mut |payload| match payload {
                ChunkPayload::Text { text } => events.push(format!("text:{text}")),
                ChunkPayload::Thinking { text } => events.push(format!("think:{text}")),
                other => panic!("unexpected payload {other:?}"),
            })
            .unwrap();
        assert_eq!(done.finish_reason, FinishReason::Stop);
        assert_eq!(done.usage.unwrap().completion_tokens, Some(3));
        assert_eq!(events, ["think:hmm ", "text:Hi", "text: there"]);
    }

    #[test]
    fn provider_error_messages_map_to_errors() {
        let mut plugin = FakePlugin::new();
        plugin.fail = Some(ErrorCode::Auth);
        let mut client = ProviderClient::new(crate::transport::LoopbackTransport::new(plugin));
        client.handshake().unwrap();
        let err = client
            .chat(request("fake/m", "hi"), &mut |_| {})
            .unwrap_err();
        match err {
            Error::Provider {
                code,
                message,
                retryable,
                ..
            } => {
                assert_eq!(code, ErrorCode::Auth);
                assert_eq!(message, "denied");
                assert!(!retryable);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn version_mismatch_is_rejected() {
        // A peer that answers `hello` with a foreign protocol version.
        struct OldPeer;
        impl Transport for OldPeer {
            fn send(
                &mut self,
                _msg: Message,
                _sink: &mut dyn FnMut(ChunkPayload),
            ) -> Result<Box<dyn Iterator<Item = std::result::Result<Message, Error>> + '_>, Error>
            {
                Ok(Box::new(
                    vec![Message::Hello {
                        protocol_version: 99,
                        name: Some("old".into()),
                        capabilities: None,
                        default_model: None,
                    }]
                    .into_iter()
                    .map(Ok),
                ))
            }
        }
        let err = ProviderClient::new(OldPeer).handshake().unwrap_err();
        assert!(err.to_string().contains("protocol version mismatch"));
    }

    #[test]
    fn missing_handshake_is_rejected() {
        struct BareTransport;
        impl Transport for BareTransport {
            fn send(
                &mut self,
                _msg: Message,
                _sink: &mut dyn FnMut(ChunkPayload),
            ) -> Result<Box<dyn Iterator<Item = std::result::Result<Message, Error>> + '_>, Error>
            {
                Ok(Box::new(
                    vec![Message::Cancel { request_id: 0 }].into_iter().map(Ok),
                ))
            }
        }
        let err = ProviderClient::new(BareTransport)
            .list_models()
            .unwrap_err();
        assert!(err.to_string().contains("handshake not performed"));
    }
}
