//! Plugin-side runtime: read JSONL requests, dispatch to a [`Handler`], write
//! JSONL responses (spec §1, §3–§8).
//!
//! This is the mirror of [`crate::ProviderClient`]: the client drives a
//! transport, the plugin serves a handler. It is the one implementation of
//! the wire rules on the plugin side, shared by the real plugin binary
//! (`clanky-provider-deepinfra`) and, for the non-streaming requests, by
//! [`crate::transport::LoopbackTransport`] via [`dispatch_request`].
//!
//! Concurrency: a reader thread parses incoming messages and handles
//! `cancel` out of band (spec §7), so a handler blocked inside a chat can
//! still be cancelled. Every other message is queued to the dispatch loop,
//! which runs the handler synchronously — one chat in flight per connection
//! (spec §6).

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::handler::{CancelFlag, ChatDone, ChatRequest, Handler};
use crate::messages::{ChunkPayload, Error, ErrorCode, Message, PROTOCOL_VERSION};

/// Handle a non-streaming request (`hello`, `listModels`) on the plugin side,
/// returning the response message(s) to write. `Ok(None)` means the caller
/// must handle the message itself (`chat` streams via a sink).
///
/// This is the shared wire-rule implementation: both [`serve`] and
/// [`LoopbackTransport`](crate::transport::LoopbackTransport) use it, so the
/// handshake and model-list semantics cannot drift between the in-process and
/// process transports.
pub fn dispatch_request(handler: &mut dyn Handler, msg: Message) -> Result<Option<Message>, Error> {
    match msg {
        Message::Hello {
            protocol_version, ..
        } => {
            if protocol_version != PROTOCOL_VERSION {
                // Spec §3: the plugin replies with an `error` and exits.
                return Err(Error::VersionMismatch {
                    peer: protocol_version,
                });
            }
            let info = handler.info();
            Ok(Some(Message::Hello {
                protocol_version: PROTOCOL_VERSION,
                name: Some(info.name),
                capabilities: Some(info.capabilities),
                default_model: info.default_model,
            }))
        }
        Message::ListModels { id } => Ok(Some(list_models_response(handler, id))),
        _ => Ok(None),
    }
}

/// `listModels` → `models` (or a request-scoped `error`).
fn list_models_response(handler: &mut dyn Handler, id: u64) -> Message {
    match handler.list_models() {
        Ok(models) => Message::Models { id, models },
        Err(Error::Provider {
            code,
            message,
            retryable,
            retry_after_ms,
        }) => Message::Error {
            request_id: Some(id),
            code,
            message,
            retryable,
            retry_after_ms,
        },
        Err(other) => connection_fatal(other),
    }
}

/// The terminal message for a chat round: `done` on success, a
/// request-scoped `error` for provider failures, a connection-fatal `error`
/// otherwise (spec §8).
fn chat_response(id: u64, outcome: Result<ChatDone, Error>) -> Message {
    match outcome {
        Ok(ChatDone {
            finish_reason,
            usage,
        }) => Message::Done {
            request_id: id,
            finish_reason,
            usage,
        },
        Err(Error::Provider {
            code,
            message,
            retryable,
            retry_after_ms,
        }) => Message::Error {
            request_id: Some(id),
            code,
            message,
            retryable,
            retry_after_ms,
        },
        Err(other) => connection_fatal(other),
    }
}

/// A connection-fatal `error` (no `requestId`): the plugin cannot continue.
fn connection_fatal(err: Error) -> Message {
    Message::Error {
        request_id: None,
        code: ErrorCode::Protocol,
        message: err.to_string(),
        retryable: false,
        retry_after_ms: None,
    }
}

/// Serve one plugin connection: read requests from `reader` until EOF,
/// dispatch them to `handler`, write responses to `writer`.
///
/// The reader is moved onto a thread so `cancel` (spec §7) is observed even
/// while the handler is blocked in a chat. Returns `Ok(())` on clean EOF;
/// a version mismatch or a fatal handler error stops serving.
pub fn serve(
    handler: &mut dyn Handler,
    reader: impl BufRead + Send + 'static,
    writer: &mut dyn Write,
) -> Result<(), Error> {
    let cancel = CancelFlag::default();
    handler.set_cancel_flag(cancel.clone());

    let (tx, rx) = mpsc::channel::<Message>();
    let reader_cancel = cancel.clone();
    // `cancel` may be observed and reset out of order against the dispatch
    // loop: the reader can flip the flag for a request the dispatch loop has
    // not started yet, and `reset` (before each chat) would then wipe it.
    // Remember the cancelled ids so the flag can be restored for that chat.
    let cancelled_ids = Arc::new(Mutex::new(HashSet::new()));
    let reader_ids = Arc::clone(&cancelled_ids);
    let reader_thread = thread::Builder::new()
        .name("clanky-plugin-reader".into())
        .spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match crate::messages::parse_message(&line) {
                    // Cancel is advisory and out of band: flip the flag
                    // immediately, no queueing.
                    Ok(Some(Message::Cancel { request_id })) => {
                        reader_cancel.cancel();
                        reader_ids.lock().unwrap().insert(request_id);
                    }
                    Ok(Some(msg)) => {
                        if tx.send(msg).is_err() {
                            break;
                        }
                    }
                    // Unknown message types and malformed lines are ignored
                    // (spec §9: additive changes; a strict plugin must not
                    // die on a future message it does not know).
                    Ok(None) | Err(_) => {}
                }
            }
            // Dropping `tx` closes the channel, ending the dispatch loop.
        })?;

    let outcome = dispatch_loop(handler, &cancel, &cancelled_ids, &rx, writer);
    // The reader thread ends when stdin reaches EOF. If we stopped early
    // (fatal error) it may still be blocked reading; detach rather than
    // join so `serve` returns promptly. `Drop` for the process handles it.
    drop(reader_thread);
    outcome
}

fn dispatch_loop(
    handler: &mut dyn Handler,
    cancel: &CancelFlag,
    cancelled_ids: &Mutex<HashSet<u64>>,
    rx: &mpsc::Receiver<Message>,
    writer: &mut dyn Write,
) -> Result<(), Error> {
    for msg in rx {
        match msg {
            Message::Hello { .. } => {
                // dispatch_request validates the version; on mismatch the
                // plugin must send `error` and exit (spec §3).
                match dispatch_request(handler, msg) {
                    Ok(Some(response)) => write_message(writer, &response)?,
                    Ok(None) => {}
                    Err(Error::VersionMismatch { peer }) => {
                        write_message(
                            writer,
                            &Message::Error {
                                request_id: None,
                                code: ErrorCode::ProtocolVersion,
                                message: format!(
                                    "plugin requires protocol v{PROTOCOL_VERSION}, client speaks v{peer}"
                                ),
                                retryable: false,
                                retry_after_ms: None,
                            },
                        )?;
                        return Err(Error::VersionMismatch { peer });
                    }
                    Err(other) => return Err(other),
                }
            }
            Message::ListModels { .. } => {
                let response = match dispatch_request(handler, msg) {
                    Ok(Some(response)) => response,
                    Ok(None) => continue,
                    Err(other) => return Err(other),
                };
                write_message(writer, &response)?;
            }
            Message::Chat {
                id,
                model,
                messages,
                tools,
                sampling,
                thinking,
            } => {
                cancel.reset();
                // Restore a cancel that raced ahead of this chat's dispatch
                // (see `cancelled_ids` in `serve`).
                if cancelled_ids.lock().unwrap().remove(&id) {
                    cancel.cancel();
                }
                let request = ChatRequest {
                    model,
                    messages,
                    tools,
                    sampling,
                    thinking,
                };
                let mut sink = |payload: ChunkPayload| {
                    // A broken pipe here means the client is gone; the
                    // terminal write below will surface it.
                    let _ = write_message(
                        writer,
                        &Message::Chunk {
                            request_id: id,
                            payload,
                        },
                    );
                };
                let outcome = handler.chat(&request, &mut sink);
                let response = chat_response(id, outcome);
                write_message(writer, &response)?;
            }
            // Handled in the reader thread.
            Message::Cancel { .. } => {}
            // Responses/clients-only messages are not requests; ignore.
            _ => {}
        }
    }
    Ok(())
}

/// Serialize and write one protocol message as a single JSON line.
pub fn write_message(writer: &mut dyn Write, msg: &Message) -> Result<(), Error> {
    let line = serde_json::to_string(msg).map_err(|e| Error::Protocol(e.to_string()))?;
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// Serve a handler over the process's own stdin/stdout. The plugin binary's
/// entire `main` is a call to this.
pub fn serve_stdio(handler: &mut dyn Handler) -> Result<(), Error> {
    let reader = std::io::BufReader::new(std::io::stdin());
    let mut writer = std::io::stdout().lock();
    serve(handler, reader, &mut writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::PluginInfo;
    use crate::messages::{Capabilities, FinishReason, ModelInfo};
    use std::io::Cursor;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fake {
        chats: Mutex<Vec<ChatRequest>>,
        calls: AtomicUsize,
        cancel: CancelFlag,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                chats: Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
                cancel: CancelFlag::default(),
            }
        }
    }

    impl Handler for Fake {
        fn info(&self) -> PluginInfo {
            PluginInfo {
                name: "fake".into(),
                capabilities: Capabilities {
                    list_models: true,
                    thinking: false,
                    tools: true,
                },
                default_model: Some("fake/default".into()),
            }
        }

        fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
            Ok(vec![ModelInfo {
                id: "fake/m".into(),
                display_name: None,
                context_window: None,
                supports_thinking: None,
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
            self.chats.lock().unwrap().push(request.clone());
            self.calls.fetch_add(1, Ordering::SeqCst);
            sink(ChunkPayload::Text { text: "hi".into() });
            Ok(ChatDone {
                finish_reason: FinishReason::Stop,
                usage: None,
            })
        }

        fn set_cancel_flag(&mut self, flag: CancelFlag) {
            self.cancel = flag;
        }
    }

    /// Drive `serve` with a scripted stdin and collect the JSONL output.
    fn drive(input: &str) -> Vec<Message> {
        let mut handler = Fake::new();
        let mut out: Vec<u8> = Vec::new();
        serve(&mut handler, Cursor::new(input.to_string()), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| {
                crate::messages::parse_message(line)
                    .unwrap()
                    .expect("known")
            })
            .collect()
    }

    #[test]
    fn handshake_advertises_name_capabilities_and_default_model() {
        let out = drive(r#"{"type":"hello","protocolVersion":1}"#);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Message::Hello {
                protocol_version,
                name,
                capabilities,
                default_model,
            } => {
                assert_eq!(*protocol_version, 1);
                assert_eq!(name.as_deref(), Some("fake"));
                assert!(capabilities.unwrap().list_models);
                assert_eq!(default_model.as_deref(), Some("fake/default"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn version_mismatch_answers_with_a_fatal_error() {
        let mut handler = Fake::new();
        let mut out: Vec<u8> = Vec::new();
        let err = serve(
            &mut handler,
            Cursor::new(r#"{"type":"hello","protocolVersion":99}"#.to_string()),
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, Error::VersionMismatch { peer: 99 }));
        let lines: Vec<Message> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| crate::messages::parse_message(l).unwrap().unwrap())
            .collect();
        assert!(matches!(
            &lines[0],
            Message::Error {
                request_id: None,
                code: ErrorCode::ProtocolVersion,
                ..
            }
        ));
    }

    #[test]
    fn list_models_and_chat_round_trip() {
        let out = drive(
            r#"{"type":"hello","protocolVersion":1}
{"type":"listModels","id":7}
{"type":"chat","id":42,"model":"fake/m","messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert!(matches!(out[0], Message::Hello { .. }));
        assert!(matches!(&out[1], Message::Models { id: 7, models } if models.len() == 1));
        assert!(matches!(
            &out[2],
            Message::Chunk { request_id: 42, payload: ChunkPayload::Text { text } } if text == "hi"
        ));
        assert!(matches!(
            &out[3],
            Message::Done {
                request_id: 42,
                finish_reason: FinishReason::Stop,
                ..
            }
        ));
    }

    #[test]
    fn provider_errors_are_request_scoped_and_fatal_errors_are_not() {
        struct Failing;
        impl Handler for Failing {
            fn info(&self) -> PluginInfo {
                PluginInfo {
                    name: "fail".into(),
                    capabilities: Capabilities::default(),
                    default_model: None,
                }
            }
            fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
                Err(Error::provider(ErrorCode::Auth, "401: bad key", false))
            }
            fn chat(
                &mut self,
                _request: &ChatRequest,
                _sink: &mut dyn FnMut(ChunkPayload),
            ) -> Result<ChatDone, Error> {
                Err(Error::provider(ErrorCode::Backend, "boom", true))
            }
        }
        let mut out: Vec<u8> = Vec::new();
        let mut handler = Failing;
        serve(
            &mut handler,
            Cursor::new(
                r#"{"type":"listModels","id":1}
{"type":"chat","id":2,"model":"m","messages":[]}"#
                    .to_string(),
            ),
            &mut out,
        )
        .unwrap();
        println!("OUT={}", String::from_utf8_lossy(&out));
        let lines: Vec<Message> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| crate::messages::parse_message(l).unwrap().unwrap())
            .collect();
        assert!(matches!(
            &lines[0],
            Message::Error {
                request_id: Some(1),
                code: ErrorCode::Auth,
                retryable: false,
                ..
            }
        ));
        assert!(matches!(
            &lines[1],
            Message::Error {
                request_id: Some(2),
                code: ErrorCode::Backend,
                retryable: true,
                ..
            }
        ));
    }

    #[test]
    fn cancel_sets_the_flag_out_of_band() {
        // A handler that blocks in `chat` until the cancel flag is set.
        struct Blocking {
            cancel: CancelFlag,
        }
        impl Handler for Blocking {
            fn info(&self) -> PluginInfo {
                PluginInfo {
                    name: "block".into(),
                    capabilities: Capabilities::default(),
                    default_model: None,
                }
            }
            fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
                Ok(vec![])
            }
            fn chat(
                &mut self,
                _request: &ChatRequest,
                sink: &mut dyn FnMut(ChunkPayload),
            ) -> Result<ChatDone, Error> {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !self.cancel.is_cancelled() {
                    if std::time::Instant::now() > deadline {
                        return Err(Error::Protocol("cancel never arrived".into()));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                sink(ChunkPayload::Text {
                    text: "partial".into(),
                });
                Ok(ChatDone {
                    finish_reason: FinishReason::Cancelled,
                    usage: None,
                })
            }
            fn set_cancel_flag(&mut self, flag: CancelFlag) {
                self.cancel = flag;
            }
        }

        let mut handler = Blocking {
            cancel: CancelFlag::default(),
        };
        let mut out: Vec<u8> = Vec::new();
        serve(
            &mut handler,
            Cursor::new(
                r#"{"type":"chat","id":9,"model":"m","messages":[]}
{"type":"cancel","requestId":9}"#
                    .to_string(),
            ),
            &mut out,
        )
        .unwrap();
        let lines: Vec<Message> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| crate::messages::parse_message(l).unwrap().unwrap())
            .collect();
        assert!(matches!(
            &lines.last(),
            Some(Message::Done {
                request_id: 9,
                finish_reason: FinishReason::Cancelled,
                ..
            })
        ));
    }

    #[test]
    fn eof_without_a_terminal_message_just_stops() {
        // A chat that returns a provider error is terminal; EOF after that
        // simply ends serving.
        let out = drive(r#"{"type":"listModels","id":1}"#);
        assert_eq!(out.len(), 1);
    }
}
