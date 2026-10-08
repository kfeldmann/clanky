//! `clanky -p` through a discovered provider plugin (M8 acceptance).
//!
//! Sets a fake `$PATH` containing an executable named `clanky-provider-*`
//! (the discovery convention) and runs the real `clanky` binary against it,
//! asserting the plugin served the turn over the process boundary. This is
//! the "`clanky -p` works via a spawned plugin" half of M8's Done-when.
//!
//! The plugin is a small POSIX-shell script, so the test needs no network,
//! API key, or Python.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A minimal provider plugin: handshake, then a streamed fixed answer.
const PLUGIN: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"hello"'*)
      printf '%s\n' '{"type":"hello","protocolVersion":1,"name":"hello-world","capabilities":{"listModels":true},"defaultModel":"hello-world/tiny"}'
      ;;
    *'"listModels"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"type":"models","id":%s,"models":[{"id":"hello-world/tiny"}]}\n' "$id"
      ;;
    *'"chat"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"type":"chunk","requestId":%s,"payload":{"kind":"text","text":"Hello from a plugin!"}}\n' "$id"
      printf '{"type":"done","requestId":%s,"finishReason":"stop"}\n' "$id"
      ;;
  esac
done
"#;

fn fake_path_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "clanky-pipe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let plugin = dir.join("clanky-provider-hello-world");
    let mut file = std::fs::File::create(&plugin).unwrap();
    write!(file, "{PLUGIN}").unwrap();
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&plugin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&plugin, perms).unwrap();
    }
    dir
}

fn path_with(dir: &Path) -> std::ffi::OsString {
    let mut paths = vec![dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).unwrap()
}

#[test]
fn pipe_mode_runs_a_discovered_plugin() {
    let dir = fake_path_dir();
    let output = Command::new(env!("CARGO_BIN_EXE_clanky"))
        .args(["--provider", "hello-world", "-p", "say hi"])
        // Run in a scratch cwd so the repo's own AGENTS.md/context is not
        // read and no `.clanky` state is created in the project.
        .current_dir(&dir)
        .env("PATH", path_with(&dir))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run clanky");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "clanky failed: status {:?}\nstderr: {stderr}",
        output.status
    );
    assert!(
        stdout.contains("Hello from a plugin!"),
        "expected the plugin's answer on stdout; got: {stdout:?} (stderr: {stderr:?})"
    );

    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn unknown_provider_lists_discovered_plugins() {
    let dir = fake_path_dir();
    let output = Command::new(env!("CARGO_BIN_EXE_clanky"))
        .args(["--provider", "nonexistent", "-p", "hi"])
        .current_dir(&dir)
        .env("PATH", path_with(&dir))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run clanky");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown provider `nonexistent`"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("hello-world"), "stderr: {stderr}");

    std::fs::remove_dir_all(dir).ok();
}
