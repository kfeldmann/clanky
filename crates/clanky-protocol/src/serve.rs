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

/// Cancel bookkeeping shared by the reader thread and the dispatch loop
/// (spec §7: `cancel` is handled out of band).
///
/// The two threads race by design, and a cancel can be read at any of three
/// moments relative to the chat it names:
///
/// 1. **Queued, not started** — the reader has the `chat` line but the
///    dispatch loop has not reached it (it may still be running an earlier
///    chat). The cancel must be remembered and applied when that chat
///    starts, and it must *not* touch the flag now: the flag is shared, so
///    flipping it here would abort whichever chat is currently running.
/// 2. **Running** — the flag must be flipped immediately, since the handler
///    is polling it right now. It is also remembered, because the dispatch
///    loop may be between its `reset` and the handler's first poll.
/// 3. **Finished** — the cancel is stale. It must be ignored outright, not
///    recorded: a flag flipped with no chat left to reset it would leak into
///    the next chat (finding 31).
///
/// Hence: `seen` (received, not yet answered) decides whether the cancel is
/// live at all, `current` decides whether it is case 1 or 2, and `pending`
/// carries case-1 cancels to the dispatch loop. All three are updated under
/// one mutex so the check and the record cannot interleave with the loop
/// moving to the next chat.
#[derive(Default)]
struct CancelState {
    /// Ids received from the client whose terminal message is not yet out.
    seen: HashSet<u64>,
    /// The chat the dispatch loop is running, if any.
    current: Option<u64>,
    /// Cancels recorded for chats that have not started yet.
    pending: HashSet<u64>,
}

impl CancelState {
    /// A `chat` line was read: from now on a cancel for `id` is live.
    fn register(&mut self, id: u64) {
        self.seen.insert(id);
    }

    /// A `cancel` arrived for `id`. Returns whether the caller must flip the
    /// flag *now* (case 2); a `false` return either means the cancel is stale
    /// and was dropped, or that it was recorded in `pending` for a chat that
    /// has not started yet.
    fn cancel(&mut self, id: u64) -> bool {
        if !self.seen.contains(&id) {
            // Never seen (a cancel for a request we were not sent) or
            // already answered (finding 31): drop it.
            return false;
        }
        self.pending.insert(id);
        self.current == Some(id)
    }

    /// The dispatch loop is starting the chat for `id`: publish it as the
    /// current one and return whether a cancel is already pending for it.
    ///
    /// The caller must have reset the flag first; this returns `true` when
    /// the flag has to be set again for this chat.
    fn start(&mut self, id: u64) -> bool {
        self.current = Some(id);
        self.pending.remove(&id)
    }

    /// The chat for `id` is over: a cancel from now on is stale.
    fn finish(&mut self, id: u64) {
        self.current = None;
        self.seen.remove(&id);
        self.pending.remove(&id);
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
    let state: Arc<Mutex<CancelState>> = Arc::new(Mutex::new(CancelState::default()));
    let reader_state = Arc::clone(&state);
    let reader_thread = thread::Builder::new()
        .name("clanky-plugin-reader".into())
        .spawn(move || {
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match crate::messages::parse_message(&line) {
                    // Cancel is advisory and out of band: it never goes
                    // through the queue, it is applied to the flag directly.
                    Ok(Some(Message::Cancel { request_id })) => {
                        // Flip the flag only if that chat is the one the
                        // handler is polling right now; otherwise record the
                        // cancel for when it starts (see `CancelState`).
                        let flip_now = reader_state.lock().unwrap().cancel(request_id);
                        if flip_now {
                            reader_cancel.cancel();
                        }
                    }
                    Ok(Some(msg)) => {
                        // Register the id before queueing: from the client's
                        // point of view the chat is live as soon as it has
                        // been sent, so a cancel racing the dispatch loop
                        // must be honoured rather than dropped as unknown.
                        if let Message::Chat { id, .. } = &msg {
                            reader_state.lock().unwrap().register(*id);
                        }
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

    let outcome = dispatch_loop(handler, &cancel, &state, &rx, writer);
    // The reader thread ends when stdin reaches EOF. If we stopped early
    // (fatal error) it may still be blocked reading; detach rather than
    // join so `serve` returns promptly. `Drop` for the process handles it.
    drop(reader_thread);
    outcome
}

fn dispatch_loop(
    handler: &mut dyn Handler,
    cancel: &CancelFlag,
    state: &Mutex<CancelState>,
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
                // Publish the in-flight id and reset the flag under one
                // lock: a cancel read from here on either sees this id as
                // current (and flips the flag) or is recorded as pending and
                // restored below.
                cancel.reset();
                if state.lock().unwrap().start(id) {
                    // A cancel raced ahead of this chat's dispatch.
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
                // The chat is finished: a cancel that arrives from now on is
                // for a completed request and must not flip the flag.
                state.lock().unwrap().finish(id);
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
                supports_tools: None,
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
        let out = drive(r#"{"type":"hello","protocolVersion":2}"#);
        assert_eq!(out.len(), 1);
        match &out[0] {
            Message::Hello {
                protocol_version,
                name,
                capabilities,
                default_model,
            } => {
                assert_eq!(*protocol_version, PROTOCOL_VERSION);
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
            r#"{"type":"hello","protocolVersion":2}
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
    fn a_cancel_for_a_queued_chat_cancels_that_chat_and_not_the_running_one() {
        // Regression: the reader used to act only on a cancel whose id was
        // the *currently running* chat, so a cancel for a chat still sitting
        // in the dispatch queue was dropped and that chat ran to completion
        // anyway. The other half of the same bug is worse: recording the id
        // and flipping the shared flag immediately would abort the chat that
        // happens to be running. A cancel for a queued chat must be held
        // until that chat starts.
        struct TwoChats {
            cancel: CancelFlag,
            /// Signalled once chat 1 is inside `chat`, so the test can send a
            /// cancel for the *queued* chat 2 at that moment.
            running: Arc<(Mutex<bool>, std::sync::Condvar)>,
            /// `is_cancelled` as seen by chat 1 (must be false) and by chat 2
            /// (must be true).
            chat1_cancelled: Arc<Mutex<Option<bool>>>,
            chat2_cancelled: Arc<Mutex<Option<bool>>>,
        }
        impl Handler for TwoChats {
            fn info(&self) -> PluginInfo {
                PluginInfo {
                    name: "two-chats".into(),
                    capabilities: Capabilities::default(),
                    default_model: None,
                }
            }
            fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
                Ok(vec![])
            }
            fn chat(
                &mut self,
                request: &ChatRequest,
                _sink: &mut dyn FnMut(ChunkPayload),
            ) -> Result<ChatDone, Error> {
                match request.model.as_str() {
                    "first" => {
                        *self.running.0.lock().unwrap() = true;
                        self.running.1.notify_all();
                        // Long enough for the cancel for chat 2 to be read.
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        *self.chat1_cancelled.lock().unwrap() = Some(self.cancel.is_cancelled());
                    }
                    "second" => {
                        // Give a wrongly-recorded flag time to be wiped by the
                        // reset if the implementation restores it too early.
                        std::thread::sleep(std::time::Duration::from_millis(50));
                        *self.chat2_cancelled.lock().unwrap() = Some(self.cancel.is_cancelled());
                    }
                    _ => {}
                }
                Ok(ChatDone {
                    finish_reason: FinishReason::Stop,
                    usage: None,
                })
            }
            fn set_cancel_flag(&mut self, flag: CancelFlag) {
                self.cancel = flag;
            }
        }

        let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
        let running = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let chat1_cancelled = Arc::new(Mutex::new(None));
        let chat2_cancelled = Arc::new(Mutex::new(None));
        let mut handler = TwoChats {
            cancel: CancelFlag::default(),
            running: Arc::clone(&running),
            chat1_cancelled: Arc::clone(&chat1_cancelled),
            chat2_cancelled: Arc::clone(&chat2_cancelled),
        };

        let serve_running = Arc::clone(&running);
        let serving = std::thread::spawn(move || {
            let mut out: Vec<u8> = Vec::new();
            serve(&mut handler, std::io::BufReader::new(reader), &mut out).unwrap();
            out
        });

        writer
            .write_all(b"{\"type\":\"chat\",\"id\":1,\"model\":\"first\",\"messages\":[]}\n")
            .unwrap();
        writer
            .write_all(b"{\"type\":\"chat\",\"id\":2,\"model\":\"second\",\"messages\":[]}\n")
            .unwrap();
        // Chat 1 is running; chat 2 is queued behind it. Cancel chat 2.
        {
            let (lock, cvar) = &*serve_running;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = cvar.wait(ready).unwrap();
            }
        }
        writer
            .write_all(b"{\"type\":\"cancel\",\"requestId\":2}\n")
            .unwrap();
        drop(writer);
        serving.join().unwrap();

        assert_eq!(
            *chat1_cancelled.lock().unwrap(),
            Some(false),
            "a cancel for a queued chat must not abort the running chat"
        );
        assert_eq!(
            *chat2_cancelled.lock().unwrap(),
            Some(true),
            "a cancel for a queued chat must apply when that chat starts"
        );
    }

    #[test]
    fn a_cancel_for_a_finished_request_does_not_cancel_the_next_chat() {
        // Finding 31: a cancel that arrives after its own request finished
        // used to flip the shared flag with no matching restore. The next
        // chat's `reset()` masked it — but only if the stale cancel landed
        // *between* chats. If it landed while the next chat was already in
        // flight, that innocent chat was cancelled. The stale cancel must
        // be ignored outright.
        struct LateStale {
            cancel: CancelFlag,
            /// Set (and signalled) once chat 2 is running, so the test can
            /// deliver the stale cancel for chat 1 at exactly that moment.
            started: Arc<(Mutex<bool>, std::sync::Condvar)>,
            /// What chat 2 observed: did the stale cancel reach it?
            chat2_saw_cancel: Arc<Mutex<Option<bool>>>,
        }
        impl Handler for LateStale {
            fn info(&self) -> PluginInfo {
                PluginInfo {
                    name: "late-stale".into(),
                    capabilities: Capabilities::default(),
                    default_model: None,
                }
            }
            fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
                Ok(vec![])
            }
            fn chat(
                &mut self,
                request: &ChatRequest,
                _sink: &mut dyn FnMut(ChunkPayload),
            ) -> Result<ChatDone, Error> {
                if request.model == "second" {
                    // Announce that chat 2 is in flight, then wait long
                    // enough for the stale cancel to be read.
                    *self.started.0.lock().unwrap() = true;
                    self.started.1.notify_all();
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    *self.chat2_saw_cancel.lock().unwrap() = Some(self.cancel.is_cancelled());
                }
                Ok(ChatDone {
                    finish_reason: FinishReason::Stop,
                    usage: None,
                })
            }
            fn set_cancel_flag(&mut self, flag: CancelFlag) {
                self.cancel = flag;
            }
        }

        // A live stdin, so the stale cancel can be written *while* chat 2
        // runs (a `Cursor` would be consumed up front by the reader).
        let (mut writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
        let started = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let chat2_saw_cancel = Arc::new(Mutex::new(None));
        let mut handler = LateStale {
            cancel: CancelFlag::default(),
            started: Arc::clone(&started),
            chat2_saw_cancel: Arc::clone(&chat2_saw_cancel),
        };

        let serve_started = Arc::clone(&started);
        let serving = std::thread::spawn(move || {
            let mut out: Vec<u8> = Vec::new();
            serve(&mut handler, std::io::BufReader::new(reader), &mut out).unwrap();
            out
        });

        writer
            .write_all(b"{\"type\":\"chat\",\"id\":1,\"model\":\"first\",\"messages\":[]}\n")
            .unwrap();
        writer
            .write_all(b"{\"type\":\"chat\",\"id\":2,\"model\":\"second\",\"messages\":[]}\n")
            .unwrap();
        // Wait until chat 2 is running, then deliver the *stale* cancel for
        // the already-finished chat 1.
        {
            let (lock, cvar) = &*serve_started;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = cvar.wait(ready).unwrap();
            }
        }
        writer
            .write_all(b"{\"type\":\"cancel\",\"requestId\":1}\n")
            .unwrap();
        drop(writer); // EOF: `serve` returns after the queued chats
        serving.join().unwrap();

        assert_eq!(
            *chat2_saw_cancel.lock().unwrap(),
            Some(false),
            "a cancel for a finished request must not cancel the next chat"
        );
    }

    #[test]
    fn eof_without_a_terminal_message_just_stops() {
        // A chat that returns a provider error is terminal; EOF after that
        // simply ends serving.
        let out = drive(r#"{"type":"listModels","id":1}"#);
        assert_eq!(out.len(), 1);
    }
}
