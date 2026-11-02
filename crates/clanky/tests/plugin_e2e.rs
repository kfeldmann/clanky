//! End-to-end plugin test (M8 "Done when"): a hello-world provider plugin
//! written in Python serves a chat turn through the process transport.
//!
//! The plugin is the in-repo example (`examples/hello-world-provider/`), run
//! exactly as Clanky runs a discovered plugin: spawned as a subprocess with
//! JSONL over stdio. No network, no API key.
//!
//! Skipped (with a note) when `python3` is unavailable, so the suite stays
//! green on minimal machines.

use std::path::PathBuf;

use clanky::tools;
use clanky::turn::{Interrupt, TurnConfig, TurnEvent, run_turn_with};
use clanky_protocol::{
    ChatMessage, FinishReason, ProcessOptions, ProcessTransport, ProviderClient,
};

fn example_plugin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("hello-world-provider")
        .join("clanky-provider-hello-world")
}

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Spawn the example plugin the way Clanky spawns a discovered one. The
/// script carries a `#!/usr/bin/env python3` shebang and is executable; if a
/// checkout dropped the mode bit, a `sh` wrapper runs it via `python3`.
fn spawn_example() -> ProcessTransport {
    let script = example_plugin();
    assert!(
        script.exists(),
        "example plugin missing: {}",
        script.display()
    );

    let direct = ProcessTransport::spawn(ProcessOptions::new(script.to_string_lossy()));
    if let Ok(transport) = direct {
        return transport;
    }

    let wrapper = std::env::temp_dir().join(format!(
        "clanky-provider-hello-world-{}",
        std::process::id()
    ));
    std::fs::write(
        &wrapper,
        format!("#!/bin/sh\nexec python3 '{}'\n", script.display()),
    )
    .expect("write wrapper");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&wrapper).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&wrapper, perms).unwrap();
    }
    ProcessTransport::spawn(ProcessOptions::new(wrapper.to_string_lossy())).expect("spawn plugin")
}

#[test]
fn hello_world_plugin_handshakes_and_answers() {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let transport = spawn_example();
    let mut client = ProviderClient::new(transport);
    let info = client.handshake().expect("handshake").clone();
    assert_eq!(info.name, "hello-world");
    assert_eq!(info.default_model.as_deref(), Some("hello-world/tiny"));
    assert!(!info.capabilities.tools);

    let models = client.list_models().expect("list models");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "hello-world/tiny");

    // Drive a full agentic turn through the process-backed client.
    let mut messages = vec![ChatMessage::user("say hi")];
    let config = TurnConfig {
        model: info.default_model.clone(),
        sampling: None,
        thinking: None,
        max_tool_rounds: None,
        max_retries: Some(0),
        retry_sleep: Some(|_| {}),
        interrupt: None,
    };
    let mut streamed = String::new();
    let output = run_turn_with(
        &mut client,
        &tools::default_tools(None),
        &mut messages,
        &config,
        &mut |event| {
            if let TurnEvent::Text { delta } = event {
                streamed.push_str(&delta);
            }
        },
    )
    .expect("turn");
    assert_eq!(output.text, "Hello from a plugin!");
    assert_eq!(
        streamed, "Hello from a plugin!",
        "text streamed incrementally"
    );
    // The conversation keeps the assistant's reply for the next turn.
    assert!(matches!(
        messages.last(),
        Some(ChatMessage::Assistant { content, .. }) if content == "Hello from a plugin!"
    ));
}

/// A plugin that streams slowly and honors `cancel` (spec §7): it stops at
/// the next chunk and reports `cancelled`.
const SLOW_PLUGIN: &str = r#"#!/usr/bin/env python3
import json, sys, threading

def emit(message):
    sys.stdout.write(json.dumps(message) + "\n")
    sys.stdout.flush()

cancelled = threading.Event()

def read_cancels():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("type") == "cancel":
            cancelled.set()
        elif message.get("type") == "chat":
            # Hand the chat to the writer thread so stdin keeps draining.
            threading.Thread(target=chat, args=(message.get("id"),), daemon=True).start()

def chat(request_id):
    for piece in ["first ", "second ", "third "]:
        if cancelled.is_set():
            emit({"type": "done", "requestId": request_id, "finishReason": "cancelled"})
            return
        emit({"type": "chunk", "requestId": request_id,
              "payload": {"kind": "text", "text": piece}})
        threading.Event().wait(0.2)
    emit({"type": "done", "requestId": request_id, "finishReason": "stop"})

emit({"type": "hello", "protocolVersion": 2, "name": "slow",
      "capabilities": {"listModels": False, "tools": False, "thinking": False},
      "defaultModel": "slow/model"})
threading.Thread(target=read_cancels, daemon=True).start()
threading.Event().wait(60)
"#;

/// Write the slow plugin to a scratch path and spawn it as a discovered
/// plugin would be spawned. `None` when `python3` is unavailable.
fn spawn_slow_plugin(tag: &str) -> Option<(ProcessTransport, PathBuf)> {
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return None;
    }
    let dir = std::env::temp_dir().join(format!(
        "clanky-slow-plugin-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("clanky-provider-slow");
    std::fs::write(&script, SLOW_PLUGIN).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    Some((
        ProcessTransport::spawn(ProcessOptions::new(script.to_string_lossy()))
            .expect("spawn slow plugin"),
        dir,
    ))
}

#[test]
fn esc_stops_a_real_plugin_and_keeps_what_streamed() {
    // The whole M22 path through a real subprocess: the turn publishes the
    // chat's id, `Esc` cancels it through the plugin's own `CancelHandle`,
    // the plugin answers `cancelled`, and the partial answer survives.
    let Some((transport, dir)) = spawn_slow_plugin("honors") else {
        return;
    };
    let handle = transport.cancel_handle();
    let mut client = ProviderClient::new(transport);
    client.handshake().expect("handshake");

    let interrupt = Interrupt::new();
    interrupt.attach(handle);
    let config = TurnConfig {
        model: Some("slow/model".into()),
        sampling: None,
        thinking: None,
        max_tool_rounds: None,
        max_retries: Some(0),
        retry_sleep: Some(|_| {}),
        interrupt: Some(interrupt.clone()),
    };
    let mut messages = vec![ChatMessage::user("talk")];
    let started = std::time::Instant::now();
    let mut streamed = String::new();
    let output = run_turn_with(
        &mut client,
        &tools::default_tools(None),
        &mut messages,
        &config,
        &mut |event| {
            if let TurnEvent::Text { delta } = &event {
                streamed.push_str(delta);
                // The first chunk is enough: stop right here, the way `Esc`
                // does from the UI thread.
                interrupt.request_stop();
            }
        },
    )
    .expect("turn");
    assert_eq!(output.finish_reason, FinishReason::Cancelled);
    assert_eq!(output.text, "first ");
    assert_eq!(streamed, "first ");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "stopped at the next chunk, took {:?}",
        started.elapsed()
    );
    assert!(matches!(
        messages.last(),
        Some(ChatMessage::Assistant { content, tool_calls: None }) if content == "first "
    ));
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn esc_stops_a_plugin_that_ignores_the_cancel() {
    // The fallback: a plugin that never reads `cancel` is killed by the
    // client's cancel deadline (spec §7), and the resulting death ends the
    // turn cancelled instead of surfacing an error.
    if !python3_available() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let dir = std::env::temp_dir().join(format!("clanky-deaf-plugin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("clanky-provider-deaf");
    // Reads one chat, emits one chunk, then sleeps and never looks at stdin.
    std::fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json, sys, time
print(json.dumps({"type": "hello", "protocolVersion": 2, "name": "deaf",
                  "capabilities": {"listModels": False, "tools": False, "thinking": False},
                  "defaultModel": "deaf/model"}), flush=True)
for line in sys.stdin:
    if json.loads(line).get("type") != "chat":
        continue
    print(json.dumps({"type": "chunk", "requestId": 1,
                      "payload": {"kind": "text", "text": "stuck"}}), flush=True)
    time.sleep(60)
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let transport = ProcessTransport::spawn(ProcessOptions::new(script.to_string_lossy()))
        .expect("spawn deaf plugin");
    let mut client = ProviderClient::new(transport);
    client.handshake().expect("handshake");

    let interrupt = Interrupt::new();
    interrupt.attach(client.transport().cancel_handle());
    let config = TurnConfig {
        model: Some("deaf/model".into()),
        sampling: None,
        thinking: None,
        max_tool_rounds: None,
        max_retries: Some(0),
        retry_sleep: Some(|_| {}),
        interrupt: Some(interrupt.clone()),
    };
    let mut messages = vec![ChatMessage::user("talk")];
    let started = std::time::Instant::now();
    let output = run_turn_with(
        &mut client,
        &tools::default_tools(None),
        &mut messages,
        &config,
        &mut |event| {
            if matches!(event, TurnEvent::Text { .. }) {
                interrupt.request_stop();
            }
        },
    )
    .expect("a kill after an ignored cancel ends the turn cancelled");
    assert_eq!(output.finish_reason, FinishReason::Cancelled);
    assert_eq!(output.text, "stuck");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the 2-second kill fallback ended it, took {:?}",
        started.elapsed()
    );
    std::fs::remove_dir_all(dir).ok();
}
