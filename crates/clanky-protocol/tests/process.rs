//! Process-transport tests (M8).
//!
//! A tiny POSIX-shell fake plugin speaks the wire protocol over real pipes —
//! no network, no Rust test binary. This exercises spawn, handshake,
//! streaming, stderr isolation, crash, and the cancel-kill fallback.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use clanky_protocol::{
    ChunkPayload, Error, FinishReason, Message, ProcessOptions, ProcessTransport, Transport,
};

/// Write an executable fake plugin and return its path.
fn fake_plugin(tag: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "clanky-plugin-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("clanky-provider-fake");
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(file, "#!/bin/sh").unwrap();
    write!(file, "{body}").unwrap();
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

/// The JSON-request dispatch loop shared by every fake plugin: `hello`,
/// `listModels`, and `chat` (calling the named shell function for chunks).
const LOOP: &str = r#"
while IFS= read -r line; do
  case "$line" in
    *'"hello"'*)
      printf '%s\n' '{"type":"hello","protocolVersion":1,"name":"fake","capabilities":{"listModels":true,"thinking":true,"tools":true},"defaultModel":"fake/default"}'
      ;;
    *'"listModels"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"type":"models","id":%s,"models":[{"id":"fake/m","displayName":"Fake M"}]}\n' "$id"
      ;;
    *'"chat"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      on_chat "$id"
      ;;
    *'"cancel"'*)
      on_cancel
      ;;
  esac
done
"#;

fn spawn(tag: &str, body: &str) -> ProcessTransport {
    let path = fake_plugin(tag, body);
    // Executing a freshly written file can race the kernel's overlay
    // bookkeeping in containers (`Text file busy`); a short retry
    // settles it. Any other error fails immediately.
    let mut attempt = 0;
    loop {
        match ProcessTransport::spawn(ProcessOptions::new(path.to_str().unwrap())) {
            Ok(transport) => return transport,
            Err(Error::Io(e))
                if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 20 =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => panic!("spawn plugin: {e}"),
        }
    }
}

fn hello() -> Message {
    Message::Hello {
        protocol_version: 1,
        name: None,
        capabilities: None,
        default_model: None,
    }
}

fn chat(id: u64) -> Message {
    Message::Chat {
        id,
        model: "fake/m".into(),
        messages: vec![clanky_protocol::ChatMessage::user("hi")],
        tools: None,
        sampling: None,
        thinking: None,
    }
}

#[test]
fn handshake_list_models_and_streamed_chat_over_real_pipes() {
    let mut transport = spawn(
        "happy",
        &format!(
            r#"
on_chat() {{
  printf '{{"type":"chunk","requestId":%s,"payload":{{"kind":"thinking","text":"hmm "}}}}\n' "$1"
  printf '{{"type":"chunk","requestId":%s,"payload":{{"kind":"text","text":"Hello "}}}}\n' "$1"
  printf '{{"type":"chunk","requestId":%s,"payload":{{"kind":"text","text":"from a plugin"}}}}\n' "$1"
  printf '{{"type":"done","requestId":%s,"finishReason":"stop","usage":{{"promptTokens":3,"completionTokens":2}}}}\n' "$1"
}}
on_cancel() {{ :; }}
{LOOP}
"#
        ),
    );

    let mut sink = |_: ChunkPayload| {};
    let hello_msg = transport
        .send(hello(), &mut sink)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    match hello_msg {
        Message::Hello {
            name,
            capabilities,
            default_model,
            ..
        } => {
            assert_eq!(name.as_deref(), Some("fake"));
            assert!(capabilities.unwrap().list_models);
            assert_eq!(default_model.as_deref(), Some("fake/default"));
        }
        other => panic!("expected hello, got {other:?}"),
    }

    let models = transport
        .send(Message::ListModels { id: 7 }, &mut sink)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert!(matches!(&models, Message::Models { id: 7, models } if models.len() == 1));

    let mut events = Vec::new();
    let responses: Vec<Message> = transport
        .send(chat(42), &mut sink)
        .unwrap()
        .map(|m| m.unwrap())
        .collect();
    for msg in &responses {
        if let Message::Chunk { payload, .. } = msg {
            events.push(payload.clone());
        }
    }
    assert_eq!(
        events,
        vec![
            ChunkPayload::Thinking {
                text: "hmm ".into()
            },
            ChunkPayload::Text {
                text: "Hello ".into()
            },
            ChunkPayload::Text {
                text: "from a plugin".into()
            },
        ]
    );
    assert!(matches!(
        responses.last(),
        Some(Message::Done {
            finish_reason: FinishReason::Stop,
            ..
        })
    ));
}

#[test]
fn stderr_noise_mid_stream_does_not_corrupt_the_stream() {
    let mut transport = spawn(
        "stderr",
        &format!(
            r#"
on_chat() {{
  printf 'chunk 1\n' >&2
  printf '{{"type":"chunk","requestId":%s,"payload":{{"kind":"text","text":"A"}}}}\n' "$1"
  printf 'a warning that looks like JSON: {{"type":"bogus"}}\n' >&2
  printf '{{"type":"chunk","requestId":%s,"payload":{{"kind":"text","text":"B"}}}}\n' "$1"
  printf '{{"type":"done","requestId":%s,"finishReason":"stop"}}\n' "$1"
}}
on_cancel() {{ :; }}
{LOOP}
"#
        ),
    );

    let mut sink = |_: ChunkPayload| {};
    let responses: Vec<Message> = transport
        .send(chat(1), &mut sink)
        .unwrap()
        .map(|m| m.unwrap())
        .collect();
    let text: String = responses
        .iter()
        .filter_map(|m| match m {
            Message::Chunk {
                payload: ChunkPayload::Text { text },
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "AB");
    assert!(matches!(responses.last(), Some(Message::Done { .. })));
}

#[test]
fn crash_mid_stream_surfaces_an_error_and_respawn_works() {
    let body = format!(
        r#"
on_chat() {{
  printf '{{"type":"chunk","requestId":%s,"payload":{{"kind":"text","text":"partial"}}}}\n' "$1"
  exit 7
}}
on_cancel() {{ :; }}
{LOOP}
"#
    );

    let mut transport = spawn("crash", &body);
    let mut sink = |_: ChunkPayload| {};
    let mut responses = transport.send(chat(1), &mut sink).unwrap();
    let first = responses.next().unwrap().unwrap();
    assert!(matches!(first, Message::Chunk { .. }));
    let err = responses.next().unwrap().unwrap_err();
    match err {
        Error::ProcessDied { message } => {
            assert!(message.contains("exited"), "{message}");
        }
        other => panic!("expected ProcessDied, got {other:?}"),
    }
    drop(responses);
    // The dead transport fails fast rather than hanging.
    assert!(transport.send(hello(), &mut sink).is_err());

    // A fresh spawn serves again (restart-on-next-use).
    let mut restarted = spawn(
        "crash",
        &format!(
            r#"
on_chat() {{
  printf '{{"type":"done","requestId":%s,"finishReason":"stop"}}\n' "$1"
}}
on_cancel() {{ :; }}
{LOOP}
"#
        ),
    );
    let responses: Vec<Message> = restarted
        .send(chat(1), &mut sink)
        .unwrap()
        .map(|m| m.unwrap())
        .collect();
    assert!(matches!(responses.last(), Some(Message::Done { .. })));
}

#[test]
fn ignored_cancel_kills_the_process() {
    let mut transport = spawn(
        "cancel",
        &format!(
            r#"
on_chat() {{
  # Ignore cancel entirely: sleep far past the client's cancel timeout.
  # `exec` so killing the process kills the sleep (and closes our pipes).
  exec sleep 30
  printf '{{"type":"done","requestId":%s,"finishReason":"stop"}}\n' "$1"
}}
on_cancel() {{ :; }}
{LOOP}
"#
        ),
    );

    let mut sink = |_: ChunkPayload| {};
    let started = Instant::now();
    // Cancel from another thread while the chat is being read (the iterator
    // borrows the transport, so cancellation goes through the handle).
    // `send` must come first: it clears a stale cancel deadline, so a
    // cancel that lands before it would be silently dropped.
    let cancel = transport.cancel_handle();
    let mut responses = transport.send(chat(1), &mut sink).unwrap();
    let cancel_thread = std::thread::spawn(move || cancel.cancel(1).unwrap());
    cancel_thread.join().unwrap();
    // The iterator's next call blocks up to CANCEL_TIMEOUT, then kills the
    // process and reports the death.
    let err = responses.next().unwrap().unwrap_err();
    assert!(matches!(err, Error::ProcessDied { .. }), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancel fallback should not wait for the plugin's own exit"
    );
}

#[test]
fn a_clean_shutdown_closes_stdin_and_reaps_the_child() {
    // The plugin exits on stdin EOF by itself (spec §1); shutdown must not
    // need to kill it.
    let path = fake_plugin(
        "shutdown",
        r#"
# Exit as soon as stdin closes.
cat >/dev/null
"#,
    );
    let transport = ProcessTransport::spawn(ProcessOptions::new(path.to_str().unwrap())).unwrap();
    assert!(transport.is_alive());
    transport.shutdown();
    assert!(!transport.is_alive());
}

#[test]
fn a_missing_command_is_an_io_error() {
    let err = ProcessTransport::spawn(ProcessOptions::new("clanky-provider-does-not-exist-xyz"))
        .unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err:?}");
}
