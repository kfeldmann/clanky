//! The shared session recorder (M14).
//!
//! One mapping from [`TurnEvent`]s to [`session::Record`]s, used by both
//! front ends: the TUI (`tui/mod.rs`) and print mode (`main.rs`). The
//! recorder owns the session file (lazily created on the first record),
//! the accumulation of streamed deltas for the round in flight, and the
//! consolidation of a round cut short by an error or ctrl+c.
//!
//! The recorder knows nothing about the UI: write failures are reported
//! through [`ErrorSink`] (the TUI pushes them onto the transcript; print
//! mode prints them to stderr), and rename/display-name operations are
//! explicit calls the TUI wires its `/name` command to.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::session;
pub use crate::session::SessionWriter;
use crate::turn::TurnEvent;

/// Where write failures go. Errors must never abort the turn: a broken
/// session file is a logging problem, not a conversation problem.
pub trait ErrorSink {
    fn report(&mut self, message: String);
}

/// Streamed deltas of the chat round currently in flight. Deltas are
/// shown as they arrive but only recorded once the round completes (as
/// one consolidated record), so this accumulator is what an interrupted
/// turn — a provider error, or ctrl+c mid-stream — consolidates and
/// writes to make sure nothing shown to the user is lost (see
/// [`Recorder::flush_partial`]).
#[derive(Debug, Default, Clone, PartialEq)]
struct PartialRound {
    thinking: String,
    text: String,
}

/// Session recorder shared by the TUI and print mode: the open session
/// file (lazily created on the first record), the deltas accumulated for
/// the round in flight, and the pending `/name` rename.
///
/// With recording off (`--no-session` / `session = false`) the recorder
/// keeps its provider/model for the `/md` fresh header but writes
/// nothing: every record call is a no-op, no file is ever created, and
/// the name/resume operations report that sessions are off.
pub struct Recorder {
    dir: PathBuf,
    provider: String,
    model: Option<String>,
    /// Whether records are written at all (`--no-session` turns it off).
    recording: bool,
    writer: Option<SessionWriter>,
    /// Name requested via `/name` before any file exists yet.
    pending_name: Option<String>,
    /// Streamed deltas not yet consolidated into a round record.
    partial: PartialRound,
}

impl Recorder {
    /// A recorder for a fresh session (provider and model are stamped
    /// into the file header when the file is created).
    pub fn new(provider: &str, model: Option<&str>) -> Self {
        Self {
            dir: session::sessions_dir(),
            provider: provider.to_string(),
            model: model.map(str::to_string),
            recording: true,
            writer: None,
            pending_name: None,
            partial: PartialRound::default(),
        }
    }

    /// Builder: turn recording off (`--no-session`). Everything the
    /// front end feeds the recorder is then accepted but discarded —
    /// including any pending `/name`, which must not surface as a
    /// session name when no file will ever be created.
    pub fn recording(mut self, on: bool) -> Self {
        self.recording = on;
        if !on {
            self.pending_name = None;
        }
        self
    }

    /// Whether records are being written (`false` = `--no-session`).
    pub fn recording_enabled(&self) -> bool {
        self.recording
    }

    /// Record into a specific directory, under a known file name (tests;
    /// production always records into `sessions_dir()` with a generated
    /// name). Returns the file path records are appended to.
    pub fn with_dir(dir: PathBuf, name: &str, provider: &str, model: Option<&str>) -> Self {
        let mut state = Self::new(provider, model);
        state.dir = dir;
        state.pending_name = Some(name.to_string());
        state
    }

    /// The message `/name` and `/resume` report when recording is off:
    /// one wording for every session command.
    pub fn disabled_message() -> String {
        "sessions are off (--no-session)".into()
    }

    /// The directory saved sessions live in (the `/resume` picker lists
    /// it; tests point it at a scratch directory).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Mutable access for tests that point the recorder at a scratch
    /// directory.
    pub fn dir_mut(&mut self) -> &mut PathBuf {
        &mut self.dir
    }

    /// The current display name: the open file's stem, else the pending
    /// `/name` request.
    pub fn current_name(&self) -> Option<String> {
        self.writer
            .as_ref()
            .and_then(|w| w.path().file_stem().and_then(|s| s.to_str()))
            .map(str::to_string)
            .or_else(|| self.pending_name.clone())
    }

    /// The open file, when one exists (the `/md` export reads exactly
    /// that file's records).
    pub fn open_file(&self) -> Option<&Path> {
        self.writer.as_ref().map(|w| w.path())
    }

    /// A session header stamped now, for a session with no records yet
    /// (`/md` on a fresh session exports it).
    pub fn fresh_header(&self) -> session::Header {
        session::Header {
            version: session::FORMAT_VERSION,
            created: session::now_millis(),
            provider: Some(self.provider.clone()),
            model: self.model.clone(),
        }
    }

    /// Ensure a writer exists (using a pending `/name` if one was given)
    /// and append a record. Write failures surface through the sink.
    /// A no-op when recording is off: nothing is created, nothing is
    /// written, and no failure is reported (there is no file to fail).
    pub fn record(&mut self, errors: &mut dyn ErrorSink, record: session::Record) {
        if !self.recording {
            return;
        }
        if let Err(err) = self
            .ensure_writer()
            .and_then(|writer| writer.append(&record))
        {
            errors.report(format!("session not saved: {err}"));
        }
    }

    /// Accumulate one streamed delta into the in-flight round.
    fn accumulate(&mut self, delta: &str, thinking: bool) {
        let slot = if thinking {
            &mut self.partial.thinking
        } else {
            &mut self.partial.text
        };
        slot.push_str(delta);
    }

    /// The round completed and its records were written; they supersede
    /// the deltas accumulated for the same content.
    fn clear_partial(&mut self) {
        self.partial = PartialRound::default();
    }

    /// Consolidate the in-flight round's accumulated deltas into the same
    /// records a completed round writes (thinking first, then text), so a
    /// turn cut short by an error or ctrl+c loses nothing. Writes nothing
    /// when the last round completed normally. Partial tool calls are not
    /// kept: they never reached the screen and nothing executed them.
    pub fn flush_partial(&mut self, errors: &mut dyn ErrorSink) {
        let partial = std::mem::take(&mut self.partial);
        if !partial.thinking.is_empty() {
            self.record(
                errors,
                session::Record::Thinking {
                    text: partial.thinking,
                },
            );
        }
        if !partial.text.is_empty() {
            self.record(
                errors,
                session::Record::Assistant {
                    text: partial.text,
                    calls: Vec::new(),
                },
            );
        }
    }

    fn ensure_writer(&mut self) -> Result<&mut SessionWriter> {
        if self.writer.is_none() {
            let name = self
                .pending_name
                .take()
                .unwrap_or_else(session::default_name);
            self.writer = Some(SessionWriter::new(
                self.dir.join(format!("{name}.jsonl")),
                session::Header {
                    version: session::FORMAT_VERSION,
                    created: 0, // stamped when the file is created
                    provider: Some(self.provider.clone()),
                    model: self.model.clone(),
                },
            ));
        }
        Ok(self.writer.as_mut().expect("just set"))
    }

    /// `/name [name]`: rename the open file, or remember the name for the
    /// file that will be created with the next prompt. Returns what the
    /// TUI should show: `Ok(Some(…))` is an info line, `Err(…)` an error
    /// line (the strings are this recorder's own user-facing messages).
    pub fn set_name(&mut self, name: Option<String>) -> std::result::Result<String, String> {
        if !self.recording {
            return Err(Self::disabled_message());
        }
        let Some(name) = name else {
            return Err("usage: /name <new-name>".into());
        };
        match self.writer.as_mut() {
            Some(writer) => match writer.rename(&name) {
                Ok(_) => Ok(format!("session saved as `{name}`")),
                Err(err) => Err(err.to_string()),
            },
            None => match session::validate_name(&name) {
                Ok(()) => {
                    self.pending_name = Some(name.clone());
                    Ok(format!("session will be saved as `{name}`"))
                }
                Err(err) => Err(err.to_string()),
            },
        }
    }

    /// Continue an existing session file: subsequent records append to
    /// `--resume`d sessions (no new header), and the pending `/name` is
    /// cleared — the file already has its name. A no-op when recording
    /// is off: the caller rejects resuming into a disabled session, and
    /// even a stray reopen must not start writing again.
    pub fn reopen(&mut self, path: PathBuf) {
        if !self.recording {
            return;
        }
        self.writer = Some(SessionWriter::reopen(path));
        self.pending_name = None;
    }

    /// Record the turn's terminal state: anything the last round streamed
    /// without completing is consolidated and written first (content,
    /// then the error), as on screen.
    pub fn record_done(
        &mut self,
        errors: &mut dyn ErrorSink,
        result: &Result<crate::turn::TurnOutput>,
    ) {
        self.flush_partial(errors);
        if let Err(err) = result {
            self.record(
                errors,
                session::Record::Error {
                    message: err.to_string(),
                },
            );
        }
    }
}

/// Map one turn event onto session records (autosave). Write failures
/// surface through the caller's sink, one report per failed write, so a
/// persistently broken session file cannot fail silently.
pub fn record_turn_event(recorder: &mut Recorder, errors: &mut dyn ErrorSink, event: &TurnEvent) {
    match event {
        TurnEvent::Round {
            text,
            thinking,
            calls,
        } => {
            // The records written below supersede the deltas accumulated
            // for this round.
            recorder.clear_partial();
            if !thinking.is_empty() {
                recorder.record(
                    errors,
                    session::Record::Thinking {
                        text: thinking.clone(),
                    },
                );
            }
            if !text.is_empty() || !calls.is_empty() {
                recorder.record(
                    errors,
                    session::Record::Assistant {
                        text: text.clone(),
                        calls: calls.clone(),
                    },
                );
            }
        }
        TurnEvent::ToolResult { name, output } => {
            recorder.record(
                errors,
                session::Record::ToolResult {
                    name: name.clone(),
                    output: output.clone(),
                },
            );
        }
        TurnEvent::Usage { usage } => {
            recorder.record(errors, session::usage_record(*usage));
        }
        // Streaming deltas are accumulated and consolidated into the
        // round's records when the round completes (or is cut short; see
        // `Recorder::flush_partial`).
        TurnEvent::Text { delta } => recorder.accumulate(delta, false),
        TurnEvent::Thinking { delta } => recorder.accumulate(delta, true),
        // Retry notices and tool-call activity are UI-only: they are not
        // conversation content, so nothing is recorded in the session
        // file.
        TurnEvent::ToolCall { .. } | TurnEvent::Retrying { .. } => {}
    }
}

/// `recorder::ErrorSink` for print mode: write failures go to stderr,
/// keeping stdout a clean stream of the answer (a session file that
/// cannot be written is a logging problem, not part of the response).
/// Every failure prints, one per failed write — no damping: a broken
/// session file should be loudly obvious.
#[derive(Default)]
pub struct StderrErrors;

impl ErrorSink for StderrErrors {
    fn report(&mut self, message: String) {
        eprintln!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::TurnEvent;

    struct PanickySink;
    impl ErrorSink for PanickySink {
        fn report(&mut self, message: String) {
            panic!("unexpected sink report: {message}");
        }
    }

    /// A disabled recorder recording into a scratch directory (the
    /// directory would be where the file would appear, so its absence
    /// proves nothing was created).
    fn disabled(tag: &str) -> (Recorder, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "clanky-nosession-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Recorder::with_dir(dir.clone(), "session", "deepinfra", None).recording(false);
        (state, dir)
    }

    #[test]
    fn a_disabled_recorder_writes_nothing() {
        let (mut state, dir) = disabled("silent");
        let mut errors = PanickySink;
        state.record(&mut errors, session::Record::User { text: "hi".into() });
        recorder_turn_events(&mut state, &mut errors);
        state.record_done(&mut errors, &Err(crate::error::Error::NoPrompt));
        assert!(
            std::fs::read_dir(&dir).unwrap().next().is_none(),
            "no file may be created"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_disabled_recorder_has_no_name_and_declines_commands() {
        let (mut state, dir) = disabled("name");
        assert_eq!(state.current_name(), None);
        assert!(state.open_file().is_none());
        assert!(!state.recording_enabled());
        assert_eq!(
            state.set_name(Some("renamed".into())).unwrap_err(),
            Recorder::disabled_message()
        );
        state.reopen(dir.join("session.jsonl"));
        assert!(state.open_file().is_none(), "reopen must not start writing");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A little turn worth recording, for the tests above.
    fn recorder_turn_events(state: &mut Recorder, errors: &mut dyn ErrorSink) {
        for event in [
            TurnEvent::Text {
                delta: "partial".into(),
            },
            TurnEvent::Round {
                text: "done".into(),
                thinking: String::new(),
                calls: vec![],
            },
            TurnEvent::ToolResult {
                name: "bash".into(),
                output: "ok".into(),
            },
        ] {
            record_turn_event(state, errors, &event);
        }
    }
}
