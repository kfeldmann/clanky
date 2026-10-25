//! Clanky — an AI coding agent for the terminal.
//!
//! M3: interactive TUI (chat view, streaming render, markdown wrap) with
//! the M2 agentic tool loop underneath. A prompt is run against the
//! provider plugin (discovered on `$PATH`, spawned as a subprocess, M8).
//! When stdin or stdout is redirected (or `-p` is passed) the same turn runs
//! in plain command-line mode, streaming text to stdout and tool activity to
//! stderr.

use std::io::{IsTerminal as _, Write};
use std::process::ExitCode;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use clap::{CommandFactory as _, Parser as _};

use clanky::cli;
use clanky::context;
use clanky::error::Result;
use clanky::provider;
use clanky::session;
use clanky::settings::{SessionSettings, Settings};
use clanky::tools;
use clanky::tui;
use clanky::turn::{self, TurnEvent};

/// Exit code for runtime errors (clap already exits with 2 on usage errors).
const EXIT_ERROR: u8 = 1;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Keep stdout clean-ish: errors go to stderr.
            eprintln!("error: {err}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run(cli: &cli::Cli) -> Result<()> {
    // File settings: user scope, then project scope overlaid on top.
    let mut settings = Settings::load_layered()?;
    // CLI flags win over both scopes.
    settings.overlay(cli.overrides());

    // A configured timeout past `Instant`'s range cannot be honored as a
    // deadline (it would overflow into "no timeout", the opposite of what
    // was asked), so it is rejected up front instead of being rewritten.
    if let Some(seconds) = settings.timeout_seconds
        && seconds > tools::MAX_DEADLINE_SECS
    {
        return Err(clanky::error::Error::InvalidConfig(format!(
            "timeout_seconds is too large ({seconds}s); the largest supported value is {}",
            tools::MAX_DEADLINE_SECS
        )));
    }

    // One shared rejection for both front ends: with recording off there
    // is no session to load into and none would be saved.
    if !settings.record_sessions() && cli.resume.is_some() {
        return Err(clanky::error::Error::ResumeWithNoSession);
    }

    // One shared rejection for both front ends: the resumed file already
    // has its name.
    if cli.resume.is_some() && cli.name.is_some() {
        return Err(clanky::error::Error::NameWithResume);
    }

    // `--name` is a user-provided value: an invalid one is a usage error,
    // not a silent fallback to a generated name (finding 19). Validating
    // here — before any front end starts — also honors the recorder's
    // documented contract that the recorder is dropped when the name is
    // invalid: there is no longer a run that continues under a different
    // name.
    if let Some(name) = cli.name.as_deref() {
        clanky::session::validate_name(name)?;
    }

    let front_end = select_front_end(cli.print, tui::available(), cli.quiet)?;

    if front_end == FrontEnd::Pipe {
        // No prompt on the command line and no piped stdin: nothing to do
        // in command-line mode, so show usage instead of a cryptic error.
        if cli.prompt.is_empty() && !cli.print && std::io::stdin().is_terminal() {
            eprint!("{}", cli::Cli::command().render_help());
            std::process::exit(2);
        }
        return run_pipe(
            &mut settings,
            cli.resume.as_deref(),
            cli.quiet,
            cli.name.as_deref(),
        );
    }

    // Interactive mode. A prompt given on the command line starts the
    // first chat turn.
    let provider_name = settings
        .provider
        .clone()
        .unwrap_or_else(|| provider::DEFAULT_PROVIDER.into());
    let mut launch = tui::Launch::from_settings(&provider_name, &settings);
    launch.resume = cli.resume.clone();
    launch.session_name = cli.name.clone();
    tui::run(launch)
}

/// The front end a run uses, decided once up front so the choice can be
/// tested without a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontEnd {
    /// One non-interactive turn: prompt from argv or stdin.
    Pipe,
    /// The full-screen interactive UI.
    Tui,
}

/// Pick the front end, rejecting flags that have no meaning in it.
///
/// `-p` forces pipe mode, and so does an unavailable TUI (stdin or stdout
/// redirected) — that fallback *is* print mode, so `clanky -q < prompt`
/// stays valid. `-q` is print-mode-only (M12): in the TUI there is nothing
/// to suppress, so it is a usage error rather than a silently ignored flag.
fn select_front_end(print: bool, tui_available: bool, quiet: bool) -> Result<FrontEnd> {
    if print || !tui_available {
        return Ok(FrontEnd::Pipe);
    }
    if quiet {
        return Err(clanky::error::Error::QuietRequiresPrint);
    }
    Ok(FrontEnd::Tui)
}

/// Non-interactive single turn: prompt from argv or stdin, response to
/// stdout, activity to stderr.
///
/// The turn is recorded into a session file exactly as a TUI run would
/// (M14): same recorder, same partial flush when the turn is cut short —
/// by an error or by ctrl+c.
fn run_pipe(
    settings: &mut Settings,
    resume: Option<&str>,
    quiet: bool,
    name: Option<&str>,
) -> Result<()> {
    let prompt_text = clanky::prompt::resolve(settings.prompt.take())?;
    let provider_name = settings
        .provider
        .clone()
        .unwrap_or_else(|| provider::DEFAULT_PROVIDER.into());

    // One long-lived plugin process for the turn (M8). It is shut down when
    // the session drops (stdin EOF per spec §1).
    let mut provider_session = provider::create(&provider_name)?;
    let default_model = provider_session.default_model().map(str::to_string);
    let config = turn::TurnConfig::from_settings(settings, default_model.as_deref());

    // The session's effective settings (M18): the merged values this
    // turn runs with — the plugin's advertised default fills in for an
    // unset model, exactly as the turn config does.
    let effective = SessionSettings {
        model: config.model.clone(),
        ..SessionSettings::from(&*settings)
    };
    // Startup INFO dump (M18): model, provider, and the merged settings,
    // on stderr (stdout stays the response/quiet channel). Unset fields
    // are elided.
    eprintln!("INFO: settings: {}", effective.describe());

    // Same session behavior as the TUI (M14): records by default, the
    // user prompt first, everything the turn produces appended as it
    // happens. `--no-session` (or `session = false`) disables the file:
    // every record call becomes a no-op and nothing is created.
    let mut recorder =
        clanky::recorder::Recorder::new(effective.clone()).recording(settings.record_sessions());
    let mut errors = clanky::recorder::StderrErrors;
    if let Some(name) = name {
        recorder = recorder.with_name(name, &mut errors);
    }

    // `--resume NAME`: load the session, rebuild the conversation the next
    // turn continues (system context fresh, records' history appended), and
    // reopen the same file so the turn's records append to it. The note
    // goes to stderr, keeping stdout a clean stream of the answer.
    let mut messages = context::system_messages();
    if let Some(name) = resume {
        let path = session::find_by_name(name)?;
        let data = session::load(&path)?;
        messages.extend(session::history_from_records(&data.records));
        eprintln!("INFO: resuming `{name}` ({} records)", data.records.len());
        // Fold the session's settings records in order (M18) to
        // reconstruct the settings state as of the end of that session;
        // the records never set values here. A state differing from the
        // active settings writes one change record (the active
        // snapshot), so from the resumed session's perspective the
        // settings changed mid-session, in order.
        let folded = session::fold_settings(&data);
        if folded.differs_from(&effective) {
            recorder.set_settings(effective.clone(), &mut errors);
        }
        recorder.reopen(path, &folded, &mut errors);
    }
    messages.push(clanky_protocol::ChatMessage::user(&prompt_text));
    recorder.record(
        &mut errors,
        session::Record::User {
            text: prompt_text.clone(),
        },
    );

    // Ctrl+C must flush the partial round instead of killing the process
    // mid-write (M14). The terminal delivers SIGINT to the whole process
    // group, so the plugin usually exits on its own; the turn then fails
    // and `record_done` below writes the normal partial flush. The
    // listener thread covers the gap — a provider plugin that ignores
    // SIGINT: it kills the plugin (idempotent), arms the immediate
    // shutdown, and the turn fails so the same flush runs.
    //
    // Both flags are registered as SIGINT actions, and signal-hook runs
    // its actions in the handler in registration order. The first ctrl+c
    // only sets `interrupted` — the listener thread's cue — because
    // `shutdown_armed` is still false. The second ctrl+c sets it again
    // and then, finding `shutdown_armed` set, exits the process
    // immediately, so an unkillable or hung plugin can never hang the
    // exit: by then the graceful flush already had its chance.
    let interrupted = Arc::new(AtomicBool::new(false));
    let shutdown_armed = Arc::new(AtomicBool::new(false));
    let sigint = Arc::clone(&interrupted);
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&interrupted))?;
    signal_hook::flag::register_conditional_shutdown(
        signal_hook::consts::SIGINT,
        EXIT_ERROR.into(),
        Arc::clone(&shutdown_armed),
    )?;
    let killer = provider_session.cancel_handle();
    let armed = Arc::clone(&shutdown_armed);
    std::thread::Builder::new()
        .name("clanky-interrupt".into())
        .spawn(move || {
            // The flag is also what makes `chat` fail fast: the turn
            // loop's request goes to a dead plugin and errors out.
            while !sigint.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Idempotent, so a stray extra wakeup (or a SIGINT that
            // raced the first one here) cannot double-kill.
            killer.kill();
            // The graceful flush had its chance; a further ctrl+c may
            // kill the process immediately.
            armed.store(true, Ordering::Relaxed);
        })?;

    let mut sink = PrintSink::new(quiet);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    // The write-failure abort (finding 18): a closed response channel
    // (`clanky -p … | head -1`) kills the plugin so the in-flight request
    // fails and the turn unwinds, instead of streaming into the void and
    // exiting 0. The same mechanism ctrl+c uses (kill is idempotent).
    let sink_killer = provider_session.cancel_handle();
    let write_failure = std::cell::RefCell::new(None);
    let result = {
        let mut out = stdout.lock();
        let mut err = stderr.lock();
        let result = turn::run_turn_with(
            provider_session.client_mut(),
            &tools::default_tools(settings.timeout_seconds),
            &mut messages,
            &config,
            &mut |event: TurnEvent| {
                // Both consumers see the event in order: the recorder
                // turns it into session records, the sink turns it into
                // output. The recorder still sees every event that
                // arrived before the failure, so the session file holds
                // the prefix of the turn, and the partial flush below
                // completes it.
                clanky::recorder::record_turn_event(&mut recorder, &mut errors, &event);
                sink.event(event, &mut out, &mut err);
                if let Some(fail) = sink.write_failed() {
                    let failure = clanky::error::Error::PrintWriteFailed {
                        stream: fail.kind().to_string(),
                        source: std::io::Error::new(fail.kind(), fail.to_string()),
                    };
                    *write_failure.borrow_mut() = Some(failure);
                    sink_killer.kill();
                }
            },
        );
        // A block that ran to the end of the turn (a thinking block, or
        // the answer text) still needs its closing separator: the trim
        // plus blank line were held back while it was open.
        sink.close_blocks(&mut out, &mut err);
        result
    };

    // The turn ended (success, error, or the ctrl+c-induced plugin
    // death): record its final state — partial flush plus the error
    // record — before the process exits.
    recorder.record_done(&mut errors, &result);

    // The response text was streamed incrementally; its closing blank
    // line was written by `close_blocks` above (on both success and
    // error), so stdout just needs its final flush.
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.flush()?;
    // The write failure is the actionable error: the turn's own result is
    // the plugin-death symptom of the kill above.
    if let Some(failure) = write_failure.into_inner() {
        return Err(failure);
    }
    result.map(|_| ())
}

/// Print-mode event sink: response text to stdout, activity to stderr so
/// `clanky -p ... | grep` style pipes stay clean. With `--quiet` (`-q`),
/// only the response text and errors are printed — no thinking, tool
/// calls, or tool results.
///
/// Every printed entry gets one blank line after it. Assistant text and
/// thinking arrive as arbitrary streaming chunks, so their trailing
/// whitespace is held back while the block is open and dropped when it
/// closes — the block then ends on a clean trim and the close adds two
/// newlines: one to end the block's last line, one blank line after it.
/// The discrete entries (tool calls, tool results, retry notices) are
/// written with `writeln!`, so a single trailing `writeln!` produces the
/// blank line.
///
/// `event` writes into the callers' streams so tests can capture output;
/// the real run passes the process stdout/stderr.
struct PrintSink {
    quiet: bool,
    /// Whether an assistant text block is currently streaming (to
    /// `out`). `false` between blocks, so a text event that arrives
    /// after another entry opens a fresh block.
    in_text: bool,
    /// Whether a thinking block is currently streaming (to `err`).
    in_thinking: bool,
    /// Trailing whitespace held back from the stream currently being
    /// written (`out` while `in_text`, `err` while `in_thinking`): if the
    /// block ends here it is dropped so the close can trim cleanly; if
    /// more text arrives, the held whitespace is emitted first.
    pending_ws: String,
    /// Whether the next thinking character written needs the `> ` quote
    /// prefix (a paragraph just started: the block opened, or a newline
    /// was written or is pending in `pending_ws`).
    need_quote: bool,
    /// The first write failure on the response channel (finding 18): a
    /// closed stdout (`clanky -p … | head -1`) must stop the turn instead
    /// of letting it keep calling the model with output going nowhere and
    /// exiting 0. Session-file writes stay non-fatal (their failures are
    /// the error sink's business — a logging problem, not a response
    /// problem); only `out`/`err` failures are fatal.
    write_error: Option<std::io::Error>,
}

impl PrintSink {
    fn new(quiet: bool) -> Self {
        Self {
            quiet,
            in_text: false,
            in_thinking: false,
            pending_ws: String::new(),
            need_quote: false,
            write_error: None,
        }
    }

    /// Whether a write to the response channel has failed. Once set, the
    /// turn should be abandoned: the consumer of the output is gone.
    pub fn write_failed(&self) -> Option<&std::io::Error> {
        self.write_error.as_ref()
    }

    /// Write to a response-channel stream, recording the first failure.
    /// After a failure further writes are skipped (the error that gets
    /// surfaced is the first one; later attempts would just add noise).
    fn out_write<W: Write>(w: &mut W, text: &str, error: &mut Option<std::io::Error>) {
        if error.is_some() {
            return;
        }
        if let Err(err) = write!(w, "{text}") {
            *error = Some(err);
        }
    }

    fn event<O: Write, E: Write>(&mut self, event: TurnEvent, out: &mut O, err: &mut E) {
        match event {
            TurnEvent::Text { delta } => {
                self.close_thinking(err);
                // Hold trailing whitespace back: if the block ends here,
                // it is dropped so the close can trim cleanly. If more
                // text arrives, the held whitespace is emitted first.
                self.in_text = true;
                self.hold_ws(out, delta, false);
            }
            TurnEvent::Thinking { delta } => {
                if self.quiet {
                    return;
                }
                // The thinking text ends when the answer text (or the
                // next event) starts; close that block first.
                self.close_text(out);
                if !self.in_thinking {
                    // Every thinking block opens with the prefix, so the
                    // stderr stream visibly marks where thinking starts.
                    self.need_quote = true;
                }
                self.in_thinking = true;
                self.hold_ws(err, delta, true);
            }
            // Round summaries are for the session recorder (M4); pipe
            // mode has already streamed the text deltas.
            TurnEvent::Round { .. } => {}
            // Pipe mode prints no token counters.
            TurnEvent::Usage { .. } => {}
            // Retries go to stderr, keeping stdout a clean stream of the
            // answer; without this the turn would look hung during backoff.
            TurnEvent::Retrying {
                attempt,
                max_retries,
                delay,
                message,
            } => {
                self.close_blocks(out, err);
                if self.quiet {
                    return;
                }
                Self::out_write(
                    err,
                    &format!(
                        "INFO: ↻ {message} — retrying in {:.1}s ({attempt}/{max_retries})\n",
                        delay.as_secs_f64()
                    ),
                    &mut self.write_error,
                );
                Self::out_write(err, "\n", &mut self.write_error);
            }
            // Activity entries end with a blank line so consecutive tool
            // rounds and the following answer text breathe, matching the
            // TUI transcript's one-blank-line-per-entry spacing.
            TurnEvent::ToolCall { name, arguments } => {
                self.close_blocks(out, err);
                if self.quiet {
                    return;
                }
                let preview = tools::truncate_call_arguments(&arguments, tools::CALL_PREVIEW_CHARS);
                Self::out_write(err, &format!("● {name} {preview}\n"), &mut self.write_error);
                Self::out_write(err, "\n", &mut self.write_error);
            }
            TurnEvent::ToolResult { name, output, .. } => {
                self.close_blocks(out, err);
                if self.quiet {
                    return;
                }
                // Same shape as the TUI transcript entry, without the
                // color: continuation lines aligned under the first
                // output character, newlines kept (never collapsed into
                // a symbol), preview capped at a few lines.
                let preview = tools::preview_result(&output, tools::RESULT_PREVIEW_LINES);
                let marker = "  └─ ";
                let mut lines = preview.lines();
                let head = lines.next().unwrap_or("");
                let indent = " ".repeat(marker.chars().count() + name.chars().count() + 2);
                Self::out_write(
                    err,
                    &format!("{marker}{name}: {head}\n"),
                    &mut self.write_error,
                );
                for l in lines {
                    Self::out_write(err, &format!("{indent}{l}\n"), &mut self.write_error);
                }
                Self::out_write(err, "\n", &mut self.write_error);
            }
        }
    }

    /// Stream a delta to `w`, holding trailing whitespace back so an open
    /// block can close on a clean trim. With `quote`, the `> ` prefix is
    /// written at the start of every paragraph: after the block opens and
    /// after each newline (including one held in `pending_ws`), before the
    /// next character that is not itself a newline.
    fn hold_ws<W: Write>(&mut self, w: &mut W, delta: String, quote: bool) {
        let trimmed = delta.trim_end_matches(char::is_whitespace);
        let ws = &delta[trimmed.len()..];
        if !self.pending_ws.is_empty() && !trimmed.is_empty() {
            self.write_quoted(w, &self.pending_ws.clone(), quote);
            // The held whitespace has now been emitted; drop it so it is
            // not re-emitted before every later delta.
            self.pending_ws.clear();
        }
        self.write_quoted(w, trimmed, quote);
        self.pending_ws.push_str(ws);
        if self.write_error.is_none()
            && let Err(err) = w.flush()
        {
            self.write_error = Some(err);
        }
    }

    /// Write text, inserting the `> ` quote prefix after each newline
    /// (and before a leading paragraph character) when `quote` is set.
    fn write_quoted<W: Write>(&mut self, w: &mut W, text: &str, quote: bool) {
        if !quote {
            Self::out_write(w, text, &mut self.write_error);
            return;
        }
        for c in text.chars() {
            if self.need_quote && c != '\n' {
                Self::out_write(w, "> ", &mut self.write_error);
                self.need_quote = false;
            }
            if c == '\n' {
                self.need_quote = true;
            }
            Self::out_write(w, &c.to_string(), &mut self.write_error);
        }
    }

    /// Close an open text block: the held-back trailing whitespace is
    /// dropped (the block ends trimmed) and two newlines are added — one
    /// to end the block's last line, one blank line after it.
    fn close_text<O: Write>(&mut self, out: &mut O) {
        if self.in_text {
            self.in_text = false;
            self.pending_ws.clear();
            Self::out_write(out, "\n\n", &mut self.write_error);
            if self.write_error.is_none()
                && let Err(err) = out.flush()
            {
                self.write_error = Some(err);
            }
        }
    }

    /// Close an open thinking block the same way (on stderr).
    fn close_thinking<E: Write>(&mut self, err: &mut E) {
        if self.in_thinking {
            self.in_thinking = false;
            self.pending_ws.clear();
            Self::out_write(err, "\n\n", &mut self.write_error);
        }
    }

    /// Close whichever block is open. The discrete entries call this so
    /// an open block ends with its blank line before the new entry
    /// prints, and `run_pipe` calls it once more at end of turn to close
    /// a block that streamed to the last chunk.
    fn close_blocks<O: Write, E: Write>(&mut self, out: &mut O, err: &mut E) {
        self.close_text(out);
        self.close_thinking(err);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_events(quiet: bool, events: &[TurnEvent]) -> (String, String) {
        let mut sink = PrintSink::new(quiet);
        let mut out = Vec::new();
        let mut err = Vec::new();
        for event in events {
            sink.event(event.clone(), &mut out, &mut err);
        }
        // Mirror the post-turn behavior of `run_pipe`.
        sink.close_blocks(&mut out, &mut err);
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn thinking_block_is_trimmed_and_gets_a_blank_line_after() {
        let (_, err) = run_events(
            false,
            &[
                TurnEvent::Thinking {
                    delta: "hmm\n  ".into(),
                },
                TurnEvent::Thinking {
                    delta: "  \n".into(),
                },
                TurnEvent::Text {
                    delta: "answer".into(),
                },
            ],
        );
        assert_eq!(err, "> hmm\n\n");
    }

    #[test]
    fn thinking_quotes_every_paragraph() {
        // Each paragraph gets its own `> ` prefix, including paragraphs
        // whose leading newline arrived in a held-whitespace chunk.
        let (_, err) = run_events(
            false,
            &[
                TurnEvent::Thinking {
                    delta: "one\n\n".into(),
                },
                TurnEvent::Thinking {
                    delta: "two\n".into(),
                },
                TurnEvent::Thinking {
                    delta: "three".into(),
                },
            ],
        );
        assert_eq!(err, "> one\n\n> two\n> three\n\n");
    }

    #[test]
    fn thinking_held_whitespace_is_emitted_when_more_text_follows() {
        let (_, err) = run_events(
            false,
            &[
                TurnEvent::Thinking {
                    delta: "a\n".into(),
                },
                TurnEvent::Thinking { delta: "b".into() },
                TurnEvent::Text { delta: "x".into() },
            ],
        );
        assert_eq!(err, "> a\n> b\n\n");
    }

    #[test]
    fn text_block_is_trimmed_and_gets_a_blank_line_after() {
        let (out, _) = run_events(
            false,
            &[
                TurnEvent::Text {
                    delta: "the answer\n  ".into(),
                },
                TurnEvent::Text {
                    delta: "  \n".into(),
                },
            ],
        );
        assert_eq!(out, "the answer\n\n");
    }

    #[test]
    fn text_held_whitespace_is_emitted_when_more_text_follows() {
        let (out, _) = run_events(
            false,
            &[
                TurnEvent::Text {
                    delta: "a\n".into(),
                },
                TurnEvent::Text { delta: "b".into() },
            ],
        );
        assert_eq!(out, "a\nb\n\n");
    }

    #[test]
    fn held_whitespace_is_not_re_emitted_before_every_later_delta() {
        // Regression: reasoning models often stream paragraph breaks at
        // delta granularity — a chunk ends in `\n\n` and the next chunk
        // carries the following words. The held whitespace must come out
        // once, not be re-emitted before every later delta.
        let (out, _) = run_events(
            false,
            &[
                TurnEvent::Text {
                    delta: "para one\n\n".into(),
                },
                TurnEvent::Text {
                    delta: "more".into(),
                },
                TurnEvent::Text {
                    delta: " words".into(),
                },
                TurnEvent::Text {
                    delta: " here".into(),
                },
            ],
        );
        assert_eq!(out, "para one\n\nmore words here\n\n");
    }

    #[test]
    fn tool_activity_after_answer_text_separates_the_blocks() {
        // A tool round's activity closes the open answer block with its
        // blank line; the next round's answer starts fresh on stdout.
        let (out, err) = run_events(
            false,
            &[
                TurnEvent::Text {
                    delta: "let me check\n\n".into(),
                },
                TurnEvent::ToolCall {
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
                TurnEvent::ToolResult {
                    name: "bash".into(),
                    output: "ok".into(),
                    images: Vec::new(),
                },
                TurnEvent::Text {
                    delta: "done".into(),
                },
            ],
        );
        assert_eq!(out, "let me check\n\ndone\n\n");
        assert_eq!(err, "● bash {}\n\n  └─ bash: ok\n\n");
    }

    #[test]
    fn thinking_between_rounds_closes_the_answer_block() {
        // Thinking never follows answer text mid-turn, but if a provider
        // interleaves them the answer block must still close cleanly
        // rather than swallowing the stderr thinking text.
        let (out, err) = run_events(
            false,
            &[
                TurnEvent::Text {
                    delta: "first\n".into(),
                },
                TurnEvent::Thinking {
                    delta: "hmm".into(),
                },
                TurnEvent::Text {
                    delta: "second".into(),
                },
            ],
        );
        assert_eq!(out, "first\n\nsecond\n\n");
        assert_eq!(err, "> hmm\n\n");
    }

    #[test]
    fn retry_notice_gets_a_blank_line_after() {
        use std::time::Duration;
        let (_, err) = run_events(
            false,
            &[TurnEvent::Retrying {
                attempt: 1,
                max_retries: 3,
                delay: Duration::from_secs(2),
                message: "Model busy".into(),
            }],
        );
        assert!(err.ends_with("\n\n"), "{err:?}");
    }

    #[test]
    fn tool_call_gets_a_blank_line_after() {
        let (_, err) = run_events(
            false,
            &[TurnEvent::ToolCall {
                name: "bash".into(),
                arguments: "{}".into(),
            }],
        );
        assert_eq!(err, "● bash {}\n\n");
    }

    #[test]
    fn tool_result_keeps_newlines_and_aligns_continuations() {
        let (_, err) = run_events(
            false,
            &[TurnEvent::ToolResult {
                name: "bash".into(),
                output: "aaa\nbbb".into(),
                images: Vec::new(),
            }],
        );
        assert_eq!(err, "  └─ bash: aaa\n           bbb\n\n");
    }

    #[test]
    fn tool_result_is_truncated_like_the_tui() {
        let output = (1..=20)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let (_, err) = run_events(
            false,
            &[TurnEvent::ToolResult {
                name: "bash".into(),
                output,
                images: Vec::new(),
            }],
        );
        assert!(err.contains("(+12 more lines)"), "{err:?}");
        assert!(!err.contains('⏎'), "{err:?}");
    }

    #[test]
    fn quiet_prints_only_the_response() {
        let (out, err) = run_events(
            true,
            &[
                TurnEvent::Thinking {
                    delta: "secret".into(),
                },
                TurnEvent::ToolCall {
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
                TurnEvent::ToolResult {
                    name: "bash".into(),
                    output: "noise".into(),
                    images: Vec::new(),
                },
                TurnEvent::Text {
                    delta: "answer".into(),
                },
            ],
        );
        assert_eq!(out, "answer\n\n");
        assert_eq!(err, "");
    }

    #[test]
    fn quiet_is_rejected_in_tui_mode() {
        // M12: `-q` is print-mode-only. With a usable TUI and no `-p`, the
        // flag would otherwise be accepted and silently ignored.
        let err = select_front_end(false, true, true).unwrap_err();
        assert!(matches!(err, clanky::error::Error::QuietRequiresPrint));
    }

    #[test]
    fn quiet_is_accepted_in_print_mode() {
        // `-p -q` is the documented invocation.
        assert_eq!(select_front_end(true, true, true).unwrap(), FrontEnd::Pipe);
    }

    #[test]
    fn quiet_is_accepted_when_the_tui_is_unavailable() {
        // Redirected stdin/stdout falls back to print mode, so
        // `clanky -q < prompt` must keep working without `-p`.
        assert_eq!(
            select_front_end(false, false, true).unwrap(),
            FrontEnd::Pipe
        );
    }

    #[test]
    fn plain_invocation_launches_the_tui() {
        assert_eq!(select_front_end(false, true, false).unwrap(), FrontEnd::Tui);
    }

    #[test]
    fn an_invalid_session_name_is_rejected_up_front() {
        // Finding 19: `--name` was validated by the recorder, which only
        // reported to stderr and let the run continue under a generated
        // name — the user's value silently replaced. Now the run aborts
        // before any front end starts.
        assert!(clanky::session::validate_name("-bad").is_err());
        assert!(clanky::session::validate_name("has space").is_err());
        assert!(clanky::session::validate_name("ok-name_1").is_ok());
    }

    /// A writer that fails after the first `capacity` bytes: the stand-in
    /// for a consumer that went away (`clanky -p … | head -1`).
    struct DyingWriter {
        capacity: usize,
        written: usize,
        buf: Vec<u8>,
    }

    impl DyingWriter {
        fn new(capacity: usize) -> Self {
            Self {
                capacity,
                written: 0,
                buf: Vec::new(),
            }
        }
    }

    impl std::io::Write for DyingWriter {
        fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
            if self.written + chunk.len() > self.capacity {
                return Err(std::io::Error::other("writer went away"));
            }
            self.written += chunk.len();
            self.buf.extend_from_slice(chunk);
            Ok(chunk.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_write_failure_is_recorded_not_swallowed() {
        // Finding 18: `writeln!(…).ok()` turned a closed stdout into
        // silence; the sink must remember the first failure instead.
        let mut sink = PrintSink::new(false);
        let mut out = DyingWriter::new(0);
        let mut err = Vec::new();
        sink.event(
            TurnEvent::Text {
                delta: "answer".into(),
            },
            &mut out,
            &mut err,
        );
        assert!(sink.write_failed().is_some(), "the failure is recorded");
    }

    #[test]
    fn a_write_failure_on_stderr_is_recorded_too() {
        // stderr carries activity, not the response, but it is still the
        // response channel: a run whose stderr is gone cannot report the
        // turn either.
        let mut sink = PrintSink::new(false);
        let mut out = Vec::new();
        let mut err = DyingWriter::new(0);
        sink.event(
            TurnEvent::ToolCall {
                name: "bash".into(),
                arguments: "{}".into(),
            },
            &mut out,
            &mut err,
        );
        assert!(sink.write_failed().is_some(), "the failure is recorded");
    }

    #[test]
    fn writes_stop_after_the_first_failure() {
        // After the first failure later events write nothing more (and
        // the first error stays the recorded one).
        let mut sink = PrintSink::new(false);
        let mut out = DyingWriter::new(4); // fits "answ", fails on "er"
        let mut err = Vec::new();
        sink.event(
            TurnEvent::Text {
                delta: "answer".into(),
            },
            &mut out,
            &mut err,
        );
        assert!(sink.write_failed().is_some(), "the failure is recorded");
        let before = out.buf.clone();
        sink.event(
            TurnEvent::Text {
                delta: "more".into(),
            },
            &mut out,
            &mut err,
        );
        assert_eq!(out.buf, before, "no further writes after the failure");
    }
}
