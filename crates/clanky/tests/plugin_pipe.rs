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
      printf '%s\n' '{"type":"hello","protocolVersion":2,"name":"hello-world","capabilities":{"listModels":true},"defaultModel":"hello-world/tiny"}'
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

#[test]
fn no_session_creates_no_session_file_in_pipe_mode() {
    let dir = fake_path_dir();
    let output = Command::new(env!("CARGO_BIN_EXE_clanky"))
        .args(["--provider", "hello-world", "-p", "--no-session", "say hi"])
        .current_dir(&dir)
        .env("PATH", path_with(&dir))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run clanky");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "clanky failed: status {:?}\nstderr: {stderr}",
        output.status
    );
    let sessions = dir.join(".clanky/sessions");
    assert!(
        !sessions.exists() || std::fs::read_dir(&sessions).unwrap().next().is_none(),
        "--no-session must not leave a session file"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn print_mode_records_a_session_by_default() {
    let dir = fake_path_dir();
    let output = Command::new(env!("CARGO_BIN_EXE_clanky"))
        .args(["--provider", "hello-world", "-p", "say hi"])
        .current_dir(&dir)
        .env("PATH", path_with(&dir))
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run clanky");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "clanky failed: status {:?}\nstderr: {stderr}",
        output.status
    );
    let sessions = dir.join(".clanky/sessions");
    let files: Vec<_> = std::fs::read_dir(&sessions)
        .unwrap_or_else(|_| panic!("session dir must exist at {}", sessions.display()))
        .collect();
    assert_eq!(files.len(), 1, "exactly one session file");
    let text = std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap();
    assert!(text.contains("\"type\":\"header\""), "{text}");
    assert!(text.contains("\"type\":\"user\""), "{text}");
    assert!(text.contains("Hello from a plugin!"), "{text}");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn resume_with_no_session_is_rejected_with_one_shared_message() {
    let dir = fake_path_dir();
    for extra in [&[][..], &["-p"][..]] {
        let output = Command::new(env!("CARGO_BIN_EXE_clanky"))
            .args(["--resume", "old"])
            .args(extra)
            .args(["--no-session", "hi"])
            .current_dir(&dir)
            .env("PATH", path_with(&dir))
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run clanky");
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("cannot use --resume with session recording off"),
            "shared message expected; stderr: {stderr}"
        );
        assert!(
            stderr.contains("--no-session"),
            "the message must name the flag: {stderr}"
        );
    }
    std::fs::remove_dir_all(dir).ok();
}

/// A plugin that answers the first chat request of a process with a tool
/// call, every later one with the final answer. Which branch a request
/// takes is decided by its own ordinal, not by scanning the request body:
/// the tool loop re-sends the whole conversation each round, so any
/// content match (e.g. on the first prompt) would match every later
/// request too and loop forever.
///
/// Every chat request is also dumped to stderr with a `CHAT-REQUEST: `
/// prefix (never the protocol stream, spec §1), so tests can assert on
/// exactly what crossed the wire.
const TOOL_PLUGIN: &str = r#"#!/bin/sh
n=0
while IFS= read -r line; do
  case "$line" in
    *'"hello"'*)
      printf '%s\n' '{"type":"hello","protocolVersion":2,"name":"hello-world","capabilities":{"tools":true,"listModels":true},"defaultModel":"hello-world/tiny"}'
      ;;
    *'"listModels"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"type":"models","id":%s,"models":[{"id":"hello-world/tiny"}]}\n' "$id"
      ;;
    *'"chat"'*)
      n=$((n+1))
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf 'CHAT-REQUEST: %s\n' "$line" >&2
      if [ "$n" -eq 1 ]; then
        printf '{"type":"chunk","requestId":%s,"payload":{"kind":"thinking","text":"let me check"}}\n' "$id"
        printf '{"type":"chunk","requestId":%s,"payload":{"kind":"toolCallStart","index":0,"id":"call_1","name":"bash"}}\n' "$id"
        printf '{"type":"chunk","requestId":%s,"payload":{"kind":"toolCallArgs","index":0,"argsChunk":"{\\"command\\":\\"echo 42\\"}"}}\n' "$id"
        printf '{"type":"done","requestId":%s,"finishReason":"toolCalls"}\n' "$id"
      else
        printf '{"type":"chunk","requestId":%s,"payload":{"kind":"text","text":"The tool ran."}}\n' "$id"
        printf '{"type":"done","requestId":%s,"finishReason":"stop","usage":{"promptTokens":4,"completionTokens":2}}\n' "$id"
      fi
      ;;
  esac
done
"#;

/// A plugin that streams half a round (thinking + text) and then fails
/// with a request-scoped non-retryable error: the partial round must be
/// flushed to the session file (M14).
const HALF_STREAM_PLUGIN: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"hello"'*)
      printf '%s\n' '{"type":"hello","protocolVersion":2,"name":"hello-world","capabilities":{"tools":false,"listModels":false},"defaultModel":"hello-world/tiny"}'
      ;;
    *'"chat"'*)
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      printf '{"type":"chunk","requestId":%s,"payload":{"kind":"thinking","text":"let me thi"}}\n' "$id"
      printf '{"type":"chunk","requestId":%s,"payload":{"kind":"text","text":"The an"}}\n' "$id"
      printf '{"type":"error","requestId":%s,"code":"backend","message":"timed out reading response","retryable":false}\n' "$id"
      ;;
  esac
done
"#;

/// A fresh scratch directory containing an executable plugin script, so
/// tests are independent of each other's files.
fn plugin_dir(plugin: &str, script: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "clanky-pipe-sessions-{}-{}-{}",
        plugin,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("clanky-provider-{plugin}"));
    std::fs::write(&file, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// One print-mode run in `dir` with the given extra argv.
fn run_clanky(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_clanky"))
        .args(args)
        .current_dir(dir)
        .env("PATH", path_with(dir))
        .env("CLANKY_PLUGIN_DEBUG", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run clanky")
}

/// The one session file under `dir/.clanky/sessions` (there must be
/// exactly one), read into memory.
fn single_session(dir: &Path) -> (PathBuf, String) {
    let sessions = dir.join(".clanky/sessions");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&sessions)
        .unwrap_or_else(|_| panic!("session dir must exist at {}", sessions.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1, "exactly one session file");
    let path = entries.remove(0);
    let text = std::fs::read_to_string(&path).unwrap();
    (path, text)
}

/// The session file's records (header excluded), parsed as raw JSON.
fn session_records(dir: &Path) -> Vec<serde_json::Value> {
    let (_, text) = single_session(dir);
    text.lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// A one-line session (user prompt + fixed answer) written by hand, for
/// `--resume NAME` to continue.
fn write_session(dir: &Path, name: &str) {
    let sessions = dir.join(".clanky/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let line = concat!(
        r#"{"type":"header","version":1,"created":0,"provider":"hello-world","model":"hello-world/tiny"}"#,
        "\n",
        r#"{"type":"user","text":"run it"}"#,
        "\n",
        r#"{"type":"assistant","text":"It said: echoed.","calls":[]}"#,
        "\n"
    );
    let line = line.replace("\\n", "\n");
    std::fs::write(sessions.join(format!("{name}.jsonl")), line).unwrap();
}

/// The chat requests the plugin received during a run, as raw JSON
/// values, in wire order. `TOOL_PLUGIN` logs each request it reads to
/// stderr with a `CHAT-REQUEST: ` prefix (stderr is never protocol,
/// spec §1), so tests can assert on exactly what crossed the wire.
fn chat_requests(output: &std::process::Output) -> Vec<serde_json::Value> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix("CHAT-REQUEST: "))
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|err| panic!("chat request is valid JSON ({err}): {line}"))
        })
        .collect()
}

/// The model-side messages of a run's first chat request (the one the
/// resumed context, if any, was loaded into).
fn first_request_messages(output: &std::process::Output) -> Vec<serde_json::Value> {
    let requests = chat_requests(output);
    let first = requests
        .first()
        .unwrap_or_else(|| panic!("no chat request was sent; stderr: {:?}", output.stderr));
    first["messages"]
        .as_array()
        .expect("chat request has a messages array")
        .clone()
}

#[test]
fn print_mode_records_the_same_records_a_tui_run_would() {
    // The tool-call transcript (thinking, call, result, final answer,
    // usage): every record type the recorder maps from a turn.
    let dir = plugin_dir("hello-world", TOOL_PLUGIN);
    let output = run_clanky(&dir, &["--provider", "hello-world", "-p", "run it"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "clanky failed: {:?}\nstderr: {stderr}",
        output.status
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "The tool ran."
    );
    let records = session_records(&dir);
    let types: Vec<&str> = records
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "user",
            "thinking",
            "assistant",
            "toolResult",
            "assistant",
            "usage"
        ],
        "the tool-loop transcript is recorded completely: {records:?}"
    );
    assert_eq!(records[0]["text"], "run it");
    assert_eq!(records[1]["text"], "let me check");
    assert_eq!(
        records[2]["calls"][0]["name"], "bash",
        "the round's tool call is recorded: {records:?}"
    );
    assert_eq!(records[3]["output"], "42");
    assert_eq!(records[4]["text"], "The tool ran.");
    assert_eq!(records[5]["promptTokens"], 4);
    assert_eq!(records[5]["completionTokens"], 2);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_mid_stream_error_flushes_the_partial_round() {
    // The provider dies mid-round after streaming thinking and text: the
    // turn fails but nothing the user saw may be lost from the file —
    // the same consolidated partial flush the TUI writes, before the
    // error record.
    let dir = plugin_dir("hello-world", HALF_STREAM_PLUGIN);
    let output = run_clanky(&dir, &["--provider", "hello-world", "-p", "go"]);
    assert!(
        !output.status.success(),
        "the provider error must surface as a failure: {:?}",
        output
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("timed out reading response"),
        "the provider's error message reaches the user: {stderr}"
    );

    let records = session_records(&dir);
    let types: Vec<&str> = records
        .iter()
        .map(|r| r["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        ["user", "thinking", "assistant", "error"],
        "partial round flushed, then the error record: {records:?}"
    );
    assert_eq!(records[1]["text"], "let me thi");
    assert_eq!(records[2]["text"], "The an");
    assert_eq!(records[2]["calls"], serde_json::json!([]));
    let error = records[3]["message"].as_str().unwrap();
    assert!(
        error.contains("timed out reading response"),
        "the error record carries the failure: {error}"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn no_session_leaves_no_file_in_either_mode() {
    // Print mode.
    let dir = plugin_dir("hello-world", PLUGIN);
    let output = run_clanky(
        &dir,
        &["--provider", "hello-world", "-p", "--no-session", "hi"],
    );
    assert!(output.status.success(), "pipe run failed");
    assert!(
        !dir.join(".clanky/sessions").exists(),
        "--no-session must not create the sessions directory"
    );
    std::fs::remove_dir_all(dir).ok();

    // TUI mode: no session file may appear either. The TUI cannot run
    // without a terminal, so the file-creation logic is what is exercised
    // here through the same setting path main uses for both front ends.
    let dir = plugin_dir("hello-world", PLUGIN);
    let output = run_clanky(&dir, &["--provider", "hello-world", "--no-session", "hi"]);
    assert!(
        output.status.success(),
        "clanky failed: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !dir.join(".clanky/sessions").exists(),
        "--no-session must not create the sessions directory"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn resume_in_pipe_mode_continues_a_session_and_appends() {
    let dir = plugin_dir("hello-world", TOOL_PLUGIN);
    write_session(&dir, "saved");

    // Continue it: the note goes to stderr, the answer to stdout, and
    // the records append to the same file.
    let output = run_clanky(
        &dir,
        &[
            "--provider",
            "hello-world",
            "-p",
            "--resume",
            "saved",
            "and again",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "clanky failed: {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status
    );
    assert_eq!(
        stdout.trim(),
        "The tool ran.",
        "stdout stays a clean stream of the answer: {stdout:?}"
    );
    assert!(
        stderr.contains("resuming `saved`") && stderr.contains("2 records"),
        "stderr notes what is being continued: {stderr}"
    );

    // The resumed file holds the old records plus this turn's.
    let records = session_records(&dir);
    let texts: Vec<&str> = records
        .iter()
        .filter(|r| r["type"] == "user")
        .filter_map(|r| r["text"].as_str())
        .collect();
    assert_eq!(texts, ["run it", "and again"], "appended: {records:?}");
    assert!(
        records.iter().any(|r| r["text"] == "The tool ran."),
        "the new turn's answer is in the same file: {records:?}"
    );

    // The header was not rewritten (no duplicate): exactly one header line.
    let (_, text) = single_session(&dir);
    assert_eq!(
        text.matches(r#""type":"header""#).count(),
        1,
        "reopen appends, it does not re-head: {text}"
    );
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn resume_in_pipe_mode_sends_the_full_prior_context() {
    // The plugin dumps each chat request it receives, so the wire itself
    // can be checked: a resumed turn must carry the prior conversation.
    let dir = plugin_dir("hello-world", TOOL_PLUGIN);
    write_session(&dir, "saved");
    // Pin one system part in the test's own project scope, so the
    // assertion below holds on machines whose user scope has no
    // SYSTEM.md/AGENTS.md (the context is best-effort and would
    // otherwise be empty here).
    std::fs::create_dir_all(dir.join(".clanky")).unwrap();
    std::fs::write(dir.join(".clanky/SYSTEM.md"), "test system context.\n").unwrap();
    let output = run_clanky(
        &dir,
        &[
            "--provider",
            "hello-world",
            "-p",
            "--resume",
            "saved",
            "and again",
        ],
    );
    assert!(
        output.status.success(),
        "clanky failed: {output:?}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages = first_request_messages(&output);
    // The system context is rebuilt fresh, so it leads the request; how
    // many parts it has depends on the machine's user-scope files, so
    // only its placement is pinned — everything before the history must
    // be system messages.
    let history_start = messages
        .iter()
        .position(|m| m["role"] != "system")
        .unwrap_or(messages.len());
    assert!(
        history_start >= 1,
        "the fresh system context is sent first: {messages:?}"
    );
    let roles: Vec<&str> = messages[history_start..]
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user"],
        "records' history, then the new prompt: {messages:?}"
    );
    assert_eq!(messages[history_start]["content"], "run it");
    assert_eq!(messages[history_start + 1]["content"], "It said: echoed.");
    assert_eq!(messages[history_start + 2]["content"], "and again");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn resume_with_a_missing_session_fails_the_same_way_in_both_modes() {
    let dir = plugin_dir("hello-world", PLUGIN);
    for extra in [&[][..], &["-p"][..]] {
        let output = run_clanky(
            &dir,
            &["--provider", "hello-world", "--resume", "nosuch"]
                .iter()
                .chain(extra.iter())
                .copied()
                .chain(["hi"])
                .collect::<Vec<_>>(),
        );
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no session named `nosuch` in .clanky/sessions"),
            "one shared message expected; stderr: {stderr}"
        );
    }
    std::fs::remove_dir_all(dir).ok();
}
