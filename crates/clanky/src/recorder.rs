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
use crate::settings::SessionSettings;
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
    /// The session's effective settings (M18): the merged values the
    /// turns run with. Dumped as a `settings` record at session start
    /// and again whenever a change is observed; the live state is
    /// updated by the front ends through [`Recorder::set_settings`].
    settings: SessionSettings,
    /// Whether the settings record for the current state has been
    /// written (`false` until the first dump; the file is created
    /// lazily, so the first user record must be preceded by the dump —
    /// see [`Recorder::record`]).
    settings_written: bool,
    /// Whether records are written at all (`--no-session` turns it off).
    recording: bool,
    writer: Option<SessionWriter>,
    /// Name requested via `/name` before any file exists yet.
    pending_name: Option<String>,
    /// Streamed deltas not yet consolidated into a round record.
    partial: PartialRound,
}

impl Recorder {
    /// Name the session file before anything is recorded (`--name`).
    ///
    /// An invalid name is an error, never a silent fallback to a
    /// generated one (finding 19): `--name` is a user-provided value, so
    /// replacing it would break the user-values invariant. The callers
    /// validate it up front too (an invalid `--name` aborts the run
    /// before either front end starts, with the same message this
    /// returns); this is the second check that keeps a future caller from
    /// reintroducing the substitution. A no-op with recording off — there
    /// is no file to name.
    pub fn with_name(&mut self, name: &str) -> Result<()> {
        if self.recording {
            session::validate_name(name)?;
            self.pending_name = Some(name.to_string());
        }
        Ok(())
    }

    /// A recorder for a fresh session, starting from the merged
    /// settings state (M18). The first settings record is written with
    /// the first user record (both front ends record the prompt before
    /// the first turn), so the settings record always precedes the
    /// first user record in the file.
    pub fn new(settings: SessionSettings) -> Self {
        Self {
            dir: session::sessions_dir(),
            settings,
            settings_written: false,
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
    pub fn with_dir(dir: PathBuf, name: &str, settings: SessionSettings) -> Self {
        let mut state = Self::new(settings);
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
    /// (`/md` on a fresh session exports it). Write-once since M18:
    /// provider/model live in the settings records.
    pub fn fresh_header(&self) -> session::Header {
        session::Header {
            version: session::FORMAT_VERSION,
            created: session::now_millis(),
            provider: None,
            model: None,
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
        // The first record of the file must be the settings snapshot
        // (M18: it precedes the first user record). The front ends
        // record the user prompt before the first turn, so this fires
        // exactly once, at session start.
        if !self.settings_written {
            self.settings_written = true;
            self.write(session::settings_record(&self.settings), errors);
        }
        self.write(record, errors);
    }

    /// Append one record, reporting write failures through the sink.
    /// A no-op when recording is off, like [`Recorder::record`].
    fn write(&mut self, record: session::Record, errors: &mut dyn ErrorSink) {
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

    /// The current settings state, for a front end's change detection
    /// (`/model` &c. compare before they assign the new value).
    /// A settings change happened (`/model`, `/thinking`, `/sampling`,
    /// `/timeout`, `/provider`, or a model default adopted from the
    /// plugin handshake): update the live state and, once the session
    /// has started (the startup dump is out), write a settings record
    /// for the new state. Before the first record the dump has not been
    /// written yet, so the change is simply folded into it — a settings
    /// change before the first prompt never produces a record of its
    /// own (and never creates the file early).
    pub fn set_settings(&mut self, settings: SessionSettings, errors: &mut dyn ErrorSink) {
        self.settings = settings;
        if self.settings_written {
            self.write(session::settings_record(&self.settings), errors);
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
                // Write-once header (M18): provider/model live in the
                // settings records now.
                session::Header {
                    version: session::FORMAT_VERSION,
                    created: 0, // stamped when the file is created
                    provider: None,
                    model: None,
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
    /// cleared — the file already has its name. `folded` is the
    /// session's own settings records folded in order (M18) — the state
    /// as of the end of that session. The records never *set* values
    /// here: when the folded state equals the active settings, nothing
    /// is written (the file already describes its settings); when it
    /// differs, one change record — a snapshot of the active settings —
    /// is appended, so from the resumed session's perspective the
    /// settings changed mid-session, in order. Either way the resumed
    /// session counts as started: later settings changes write records.
    /// A no-op when recording is off: the caller rejects resuming into a
    /// disabled session, and even a stray reopen must not start writing.
    pub fn reopen(&mut self, path: PathBuf, folded: &SessionSettings, errors: &mut dyn ErrorSink) {
        if !self.recording {
            return;
        }
        self.writer = Some(SessionWriter::reopen(path));
        self.pending_name = None;
        self.settings_written = true;
        if folded.differs_from(&self.settings) {
            self.write(session::settings_record(&self.settings), errors);
        }
    }

    /// Record the turn's terminal state: anything the last round streamed
    /// without completing is consolidated and written first (content,
    /// then the error), as on screen. A turn that reached its terminal
    /// `done` also appends a `stopReason` record; a turn that failed gets
    /// an `error` record instead — the terminal record says which way the
    /// turn ended.
    pub fn record_done(
        &mut self,
        errors: &mut dyn ErrorSink,
        result: &Result<crate::turn::TurnOutput>,
    ) {
        self.flush_partial(errors);
        match result {
            Ok(output) => {
                self.record(errors, session::stop_reason_record(output.finish_reason));
            }
            Err(err) => {
                self.record(
                    errors,
                    session::Record::Error {
                        message: err.to_string(),
                    },
                );
            }
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
        TurnEvent::ToolResult {
            name,
            output,
            images,
        } => {
            recorder.record(
                errors,
                session::Record::ToolResult {
                    name: name.clone(),
                    output: output.clone(),
                    images: (!images.is_empty()).then(|| images.clone()),
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
        eprintln!("ERROR: {message}");
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
        let state =
            Recorder::with_dir(dir.clone(), "session", SessionSettings::default()).recording(false);
        (state, dir)
    }

    #[test]
    fn an_invalid_name_is_an_error_not_a_generated_name() {
        // Finding 19: the user's `--name` must never be silently replaced
        // by a generated one. `with_name` refuses instead, and the
        // recorder keeps no name at all.
        let dir = std::env::temp_dir().join(format!(
            "clanky-bad-name-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = Recorder::with_dir(dir.clone(), "session", SessionSettings::default());
        state.pending_name = None;
        let err = state.with_name("has space").unwrap_err();
        assert!(err.to_string().contains("invalid session name"), "{err}");
        assert_eq!(state.current_name(), None, "no name was substituted");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_valid_name_is_taken() {
        let dir = std::env::temp_dir().join(format!(
            "clanky-good-name-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = Recorder::with_dir(dir.clone(), "session", SessionSettings::default());
        state.pending_name = None;
        state.with_name("fix-123").unwrap();
        assert_eq!(state.current_name().as_deref(), Some("fix-123"));
        std::fs::remove_dir_all(dir).ok();
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
        let mut errors = PanickySink;
        state.reopen(
            dir.join("session.jsonl"),
            &SessionSettings::default(),
            &mut errors,
        );
        assert!(state.open_file().is_none(), "reopen must not start writing");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A fresh recorder with one settings change folded in before the
    /// first record (the startup-dump path: `set_settings` before the
    /// first `record` must not create the file early nor write a record).
    fn pre_start_change(tag: &str) -> (Recorder, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "clanky-settings-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = Recorder::with_dir(
            dir.clone(),
            "session",
            SessionSettings {
                provider: Some("deepinfra".into()),
                ..SessionSettings::default()
            },
        );
        let mut errors = PanickySink;
        state.set_settings(
            SessionSettings {
                provider: Some("deepinfra".into()),
                model: Some("m/new".into()),
                ..SessionSettings::default()
            },
            &mut errors,
        );
        assert!(
            state.open_file().is_none(),
            "a pre-start change must not create the file"
        );
        (state, dir)
    }

    /// The records on disk (settings record included).
    fn records_on_disk(dir: &Path) -> Vec<session::Record> {
        session::load(&dir.join("session.jsonl")).unwrap().records
    }

    #[test]
    fn the_first_record_is_the_settings_snapshot() {
        // The dump fires with the first user record, so the file's first
        // record is the settings record, before the first user record.
        let (mut state, dir) = pre_start_change("first-record");
        let mut errors = PanickySink;
        state.record(&mut errors, session::Record::User { text: "hi".into() });
        let records = records_on_disk(&dir);
        assert_eq!(
            records,
            vec![
                session::settings_record(&SessionSettings {
                    provider: Some("deepinfra".into()),
                    model: Some("m/new".into()),
                    ..SessionSettings::default()
                }),
                session::Record::User { text: "hi".into() },
            ]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_change_after_start_writes_a_settings_record() {
        // `/model` after the session started: the recorder observes the
        // new state and writes one record for it.
        let (mut state, dir) = pre_start_change("mid-change");
        let mut errors = PanickySink;
        state.record(&mut errors, session::Record::User { text: "hi".into() });
        state.set_settings(
            SessionSettings {
                provider: Some("deepinfra".into()),
                model: Some("m/other".into()),
                timeout_seconds: Some(10),
                ..SessionSettings::default()
            },
            &mut errors,
        );
        let records = records_on_disk(&dir);
        assert_eq!(records.len(), 3);
        assert_eq!(
            records[2],
            session::settings_record(&SessionSettings {
                provider: Some("deepinfra".into()),
                model: Some("m/other".into()),
                timeout_seconds: Some(10),
                ..SessionSettings::default()
            })
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn resume_with_equal_settings_writes_no_record_and_unequal_writes_one() {
        // M18's `/resume` semantics: the session's records folded in
        // order are compared against the active settings; equal → no
        // record (the file already describes its settings), different →
        // one change record, a snapshot of the active settings.
        let dir = std::env::temp_dir().join(format!(
            "clanky-resume-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // A pre-M18-shaped file: legacy header, no settings records.
        let path = dir.join("old.jsonl");
        let mut writer = SessionWriter::new(
            path.clone(),
            session::Header {
                version: session::FORMAT_VERSION,
                created: 1_700_000_000_000,
                provider: Some("deepinfra".into()),
                model: Some("m/old".into()),
            },
        );
        writer
            .append(&session::Record::User { text: "hi".into() })
            .unwrap();
        drop(writer);
        let data = session::load(&path).unwrap();
        let folded = session::fold_settings(&data);

        // Active settings equal to the folded state: no record. Each
        // recorder gets its own copy of the file, so the two appends
        // never share one (reopen writes to the file it is given).
        std::fs::copy(&path, dir.join("equal.jsonl")).unwrap();
        let mut state = Recorder::with_dir(
            dir.clone(),
            "equal-unused",
            SessionSettings {
                provider: Some("deepinfra".into()),
                model: Some("m/old".into()),
                ..SessionSettings::default()
            },
        );
        let mut errors = PanickySink;
        state.reopen(dir.join("equal.jsonl"), &folded, &mut errors);
        state.record(
            &mut errors,
            session::Record::User {
                text: "again".into(),
            },
        );
        // Reopen switched the writer to the resumed file.
        let records = session::load(&path).unwrap().records;
        assert!(
            records
                .iter()
                .all(|r| !matches!(r, session::Record::Settings { .. })),
            "equal states write no record: {records:?}"
        );

        // Active settings differing: one change record (the active
        // snapshot), before the new user record.
        std::fs::copy(&path, dir.join("unequal.jsonl")).unwrap();
        let mut state = Recorder::with_dir(
            dir.clone(),
            "unequal-unused",
            SessionSettings {
                provider: Some("deepinfra".into()),
                model: Some("m/new".into()),
                ..SessionSettings::default()
            },
        );
        state.reopen(dir.join("unequal.jsonl"), &folded, &mut errors);
        state.record(
            &mut errors,
            session::Record::User {
                text: "again".into(),
            },
        );
        let records = session::load(&dir.join("unequal.jsonl")).unwrap().records;
        // The file's own records, then the change record, then the new
        // user record.
        assert_eq!(
            records,
            vec![
                session::Record::User { text: "hi".into() },
                session::settings_record(&SessionSettings {
                    provider: Some("deepinfra".into()),
                    model: Some("m/new".into()),
                    ..SessionSettings::default()
                }),
                session::Record::User {
                    text: "again".into()
                },
            ]
        );
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
                images: Vec::new(),
            },
        ] {
            record_turn_event(state, errors, &event);
        }
    }
}
