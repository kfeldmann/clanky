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
use clanky::turn::{TurnConfig, TurnEvent, run_turn_with};
use clanky_protocol::{ChatMessage, ProcessOptions, ProcessTransport, ProviderClient};

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
    };
    let mut streamed = String::new();
    let output = run_turn_with(
        &mut client,
        &tools::default_tools(),
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
