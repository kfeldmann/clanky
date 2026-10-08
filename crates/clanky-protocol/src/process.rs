//! Process transport: run a provider plugin as a long-lived subprocess and
//! speak JSONL over its stdin/stdout (spec §1, §6–§8).
//!
//! Responsibilities:
//!
//! - **Spawn** the plugin with piped stdin/stdout/stderr; environment and
//!   working directory are inherited (core never handles provider keys).
//! - **Reader thread** owns stdout, parses one JSON message per line, and
//!   forwards it over a channel. A blocking read therefore never blocks the
//!   turn logic or the UI, and a partial line is buffered until its newline.
//! - **Stderr thread** drains plugin stderr into a log file (or our stderr in
//!   debug mode). stderr is never protocol (spec §1): a warning printed
//!   mid-response cannot corrupt the stream.
//! - **Writer** serializes outgoing messages as one JSON line each.
//! - **Crash handling**: an unexpected exit mid-request surfaces a
//!   [`Error::ProcessDied`] for the in-flight request and marks the transport
//!   dead, so the caller can respawn on next use (never auto-retry a turn).
//! - **Cancel fallback** (spec §7): after a `cancel`, if no terminal message
//!   arrives within [`CANCEL_TIMEOUT`], the process is killed.
//!
//! Cancellation can arrive while a chat is being read, so the process state
//! lives behind an `Arc`: [`ProcessTransport::cancel_handle`] returns a
//! cloneable, `Send + Sync` [`CancelHandle`] that writes `cancel` and can
//! kill the process from another thread without borrowing the transport.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wait_timeout::ChildExt as _;

use crate::messages::{ChunkPayload, Error, Message};
use crate::transport::Transport;

/// How long to wait for a terminal message after sending `cancel` before
/// killing the process (spec §7: "~2 seconds").
pub const CANCEL_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait for a plugin to exit on its own after stdin closes
/// before killing it.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Environment variable that routes plugin stderr to Clanky's own stderr
/// instead of the plugin log. Useful when developing a plugin.
pub const DEBUG_ENV: &str = "CLANKY_PLUGIN_DEBUG";

/// How to spawn a plugin process.
#[derive(Debug, Clone)]
pub struct ProcessOptions {
    /// Executable to run (a bare name is resolved on `$PATH`).
    pub command: String,
    /// Where to append the plugin's stderr. `None` forwards it to Clanky's
    /// own stderr (debug mode); `Some` writes to that file (created, appended).
    pub log_path: Option<PathBuf>,
}

impl ProcessOptions {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            log_path: None,
        }
    }

    pub fn with_log(mut self, path: impl Into<PathBuf>) -> Self {
        self.log_path = Some(path.into());
        self
    }
}

/// State shared between the transport and any [`CancelHandle`]s.
struct Shared {
    command: String,
    child: Mutex<Child>,
    /// `None` once stdin has been closed (shutdown).
    stdin: Mutex<Option<ChildStdin>>,
    /// Set by a `cancel`: while set, the response iterator uses a timeout and
    /// kills the process when it expires (spec §7).
    cancel_deadline: Mutex<Option<Instant>>,
    /// Set when the process is known dead (crash or kill): later writes fail
    /// fast instead of hanging on a broken pipe.
    dead: AtomicBool,
    log_path: Option<PathBuf>,
}

impl Shared {
    fn write(&self, msg: &Message) -> Result<(), Error> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(self.died_error("process is not running"));
        }
        let line = serde_json::to_string(msg).map_err(|e| Error::Protocol(e.to_string()))?;
        let mut guard = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        let Some(stdin) = guard.as_mut() else {
            return Err(self.died_error("stdin is closed"));
        };
        if let Err(source) = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.flush())
        {
            self.dead.store(true, Ordering::SeqCst);
            return Err(self.died_error(&source.to_string()));
        }
        Ok(())
    }

    fn request_cancel(&self, request_id: u64) -> Result<(), Error> {
        *self
            .cancel_deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + CANCEL_TIMEOUT);
        self.write(&Message::Cancel { request_id })
    }

    fn cancel_deadline(&self) -> Option<Instant> {
        *self
            .cancel_deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn clear_cancel(&self) {
        *self
            .cancel_deadline
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn is_alive(&self) -> bool {
        if self.dead.load(Ordering::SeqCst) {
            return false;
        }
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        matches!(child.try_wait(), Ok(None))
    }

    /// Kill the process and reap it. Idempotent.
    fn kill(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Close stdin (so a well-behaved plugin exits on EOF), give the process
    /// a moment to exit, then kill it if it is still around.
    fn shutdown(&self) {
        self.stdin.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        match child.wait_timeout(SHUTDOWN_TIMEOUT) {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        self.dead.store(true, Ordering::SeqCst);
    }

    fn died_error(&self, detail: &str) -> Error {
        let status = self
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_wait()
            .ok()
            .flatten()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "no exit status".into());
        let log = self
            .log_path
            .as_ref()
            .map(|p| format!(" (plugin log: {})", p.display()))
            .unwrap_or_default();
        Error::ProcessDied {
            message: format!("`{}` {detail}; exit {status}{log}", self.command),
        }
    }
}

/// A cloneable handle that can cancel or kill a running plugin from another
/// thread (spec §7), without borrowing the transport.
#[derive(Clone)]
pub struct CancelHandle {
    shared: Arc<Shared>,
}

impl CancelHandle {
    /// Ask the plugin to stop the in-flight chat and start the kill-fallback
    /// clock (spec §7).
    pub fn cancel(&self, request_id: u64) -> Result<(), Error> {
        self.shared.request_cancel(request_id)
    }

    /// Kill the process immediately (the unconditional fallback).
    pub fn kill(&self) {
        self.shared.kill();
    }

    /// The executable this handle's plugin was spawned from.
    pub fn command(&self) -> &str {
        &self.shared.command
    }
}

/// A running plugin process plus its stdout reader channel and stderr drain.
pub struct ProcessTransport {
    shared: Arc<Shared>,
    rx: Receiver<Result<Message, Error>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ProcessTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessTransport")
            .field("command", &self.shared.command)
            .field("alive", &self.shared.is_alive())
            .finish()
    }
}

impl ProcessTransport {
    /// Spawn the plugin process and start its reader/stderr threads.
    pub fn spawn(options: ProcessOptions) -> Result<Self, Error> {
        let mut child = Command::new(&options.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Protocol("plugin stdin was not captured".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Protocol("plugin stdout was not captured".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Protocol("plugin stderr was not captured".into()))?;

        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("clanky-plugin-stdout".into())
            .spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.trim().is_empty() {
                        continue;
                    }
                    match crate::messages::parse_message(&line) {
                        // Unknown message types are ignored (spec §9).
                        Ok(None) => {}
                        Ok(Some(msg)) => {
                            if tx.send(Ok(msg)).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            if tx.send(Err(err)).is_err() {
                                break;
                            }
                        }
                    }
                }
                // Dropping `tx` here signals EOF / exit to the iterator.
            })
            .map_err(Error::Io)?;

        let log_path = options.log_path.clone();
        let stderr_thread = thread::Builder::new()
            .name("clanky-plugin-stderr".into())
            .spawn(move || drain_stderr(stderr, log_path))
            .map_err(Error::Io)?;

        Ok(Self {
            shared: Arc::new(Shared {
                command: options.command,
                child: Mutex::new(child),
                stdin: Mutex::new(Some(stdin)),
                cancel_deadline: Mutex::new(None),
                dead: AtomicBool::new(false),
                log_path: options.log_path,
            }),
            rx,
            stderr_thread: Some(stderr_thread),
        })
    }

    /// A handle that can cancel this plugin from another thread.
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The executable this transport was spawned from.
    pub fn command(&self) -> &str {
        &self.shared.command
    }

    /// Where plugin stderr is being written, when it is a file.
    pub fn log_path(&self) -> Option<&Path> {
        self.shared.log_path.as_deref()
    }

    /// Whether the process is still believed to be running.
    pub fn is_alive(&self) -> bool {
        self.shared.is_alive()
    }

    /// Kill the process and reap it.
    pub fn kill(&self) {
        self.shared.kill();
    }

    /// Close stdin, let the plugin exit, then kill it if needed.
    pub fn shutdown(&self) {
        self.shared.shutdown();
    }

    /// Wait for the next message, honoring the cancel fallback.
    fn next_message(&mut self) -> Option<Result<Message, Error>> {
        if self.shared.dead.load(Ordering::SeqCst) {
            return Some(Err(self.shared.died_error("process is not running")));
        }
        let timeout = self
            .shared
            .cancel_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let received = match timeout {
            Some(timeout) => self.rx.recv_timeout(timeout),
            None => match self.rx.recv() {
                Ok(msg) => Ok(msg),
                Err(_) => Err(RecvTimeoutError::Disconnected),
            },
        };
        match received {
            Ok(msg) => Some(msg),
            Err(RecvTimeoutError::Timeout) => {
                // Spec §7: cancel is advisory; kill when it is ignored.
                self.shared.kill();
                Some(Err(self.shared.died_error("did not answer cancel")))
            }
            Err(RecvTimeoutError::Disconnected) => {
                // stdout hit EOF: the process exited.
                self.shared.dead.store(true, Ordering::SeqCst);
                Some(Err(self.shared.died_error("exited unexpectedly")))
            }
        }
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        self.shared.shutdown();
        // The stderr thread is detached rather than joined: a plugin that
        // leaked children holding the stderr pipe could otherwise block
        // teardown indefinitely. Lines are flushed as they are read, so
        // detaching loses nothing but a final partial line.
        let _ = self.stderr_thread.take();
    }
}

impl Transport for ProcessTransport {
    fn send(
        &mut self,
        msg: Message,
        _sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<Box<dyn Iterator<Item = Result<Message, Error>> + '_>, Error> {
        // Chunks are yielded as `Message::Chunk` items rather than pushed to
        // `sink`: the iterator is the only place that sees them over time
        // (the trait's `sink` borrow ends with this call). `ProviderClient`
        // handles both delivery paths identically.
        self.shared.clear_cancel();
        self.shared.write(&msg)?;
        Ok(Box::new(ProcessResponses {
            transport: self,
            done: false,
        }))
    }

    fn notify(&mut self, msg: Message) -> Result<(), Error> {
        match msg {
            Message::Cancel { request_id } => self.shared.request_cancel(request_id),
            other => self.shared.write(&other),
        }
    }
}

/// Iterator over one request's responses: yields `chunk` events, then exactly
/// one terminal message (`hello`, `models`, `done`, or `error`).
struct ProcessResponses<'a> {
    transport: &'a mut ProcessTransport,
    done: bool,
}

impl Iterator for ProcessResponses<'_> {
    type Item = Result<Message, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let msg = self.transport.next_message()?;
        let terminal = match &msg {
            Ok(Message::Chunk { .. }) => false,
            Ok(_) | Err(_) => true,
        };
        if terminal {
            self.done = true;
            self.transport.shared.clear_cancel();
        }
        Some(msg)
    }
}

/// Read plugin stderr to EOF and write it to the log file (or our stderr in
/// debug mode). stderr is never protocol, so this can never affect the chat.
fn drain_stderr(stderr: std::process::ChildStderr, log_path: Option<PathBuf>) {
    let reader = BufReader::new(stderr);
    let mut sink: Box<dyn Write + Send> = match log_path {
        Some(path) => match open_log(&path) {
            Ok(file) => Box::new(file),
            Err(err) => {
                eprintln!("clanky: cannot open plugin log {}: {err}", path.display());
                Box::new(std::io::stderr())
            }
        },
        None => Box::new(std::io::stderr()),
    };
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let _ = writeln!(sink, "{line}");
    }
    let _ = sink.flush();
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}
