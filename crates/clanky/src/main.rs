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
use clanky::settings::Settings;
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

    // One shared rejection for both front ends: with recording off there
    // is no session to load into and none would be saved.
    if !settings.record_sessions() && cli.resume.is_some() {
        return Err(clanky::error::Error::ResumeWithNoSession);
    }

    if cli.print || !tui::available() {
        // No prompt on the command line and no piped stdin: nothing to do
        // in command-line mode, so show usage instead of a cryptic error.
        if cli.prompt.is_empty() && !cli.print && std::io::stdin().is_terminal() {
            eprint!("{}", cli::Cli::command().render_help());
            std::process::exit(2);
        }
        return run_pipe(&mut settings, cli.resume.as_deref(), cli.quiet);
    }

    // Interactive mode. A prompt given on the command line starts the
    // first chat turn.
    let provider_name = settings
        .provider
        .clone()
        .unwrap_or_else(|| provider::DEFAULT_PROVIDER.into());
    let mut launch = tui::Launch::from_settings(&provider_name, &settings);
    launch.resume = cli.resume.clone();
    tui::run(launch)
}

/// Non-interactive single turn: prompt from argv or stdin, response to
/// stdout, activity to stderr.
///
/// The turn is recorded into a session file exactly as a TUI run would
/// (M14): same recorder, same partial flush when the turn is cut short —
/// by an error or by ctrl+c.
fn run_pipe(settings: &mut Settings, resume: Option<&str>, quiet: bool) -> Result<()> {
    let prompt_text = clanky::prompt::resolve(settings.prompt.take())?;
    let provider_name = settings
        .provider
        .take()
        .unwrap_or_else(|| provider::DEFAULT_PROVIDER.into());

    // One long-lived plugin process for the turn (M8). It is shut down when
    // the session drops (stdin EOF per spec §1).
    let mut provider_session = provider::create(&provider_name)?;
    let default_model = provider_session.default_model().map(str::to_string);
    let config = turn::TurnConfig::from_settings(settings, default_model.as_deref());

    // Same session behavior as the TUI (M14): records by default, the
    // user prompt first, everything the turn produces appended as it
    // happens. `--no-session` (or `session = false`) disables the file:
    // every record call becomes a no-op and nothing is created.
    let mut recorder =
        clanky::recorder::Recorder::new(provider_session.name(), config.model.as_deref())
            .recording(settings.record_sessions());
    let mut errors = clanky::recorder::StderrErrors;

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
        recorder.reopen(path);
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
    let result = {
        let mut out = stdout.lock();
        let mut err = stderr.lock();
        let result = turn::run_turn_with(
            provider_session.client_mut(),
            &tools::default_tools(),
            &mut messages,
            &config,
            &mut |event: TurnEvent| {
                // Both consumers see the event in order: the recorder
                // turns it into session records, the sink turns it into
                // output.
                clanky::recorder::record_turn_event(&mut recorder, &mut errors, &event);
                sink.event(event, &mut out, &mut err);
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
}

impl PrintSink {
    fn new(quiet: bool) -> Self {
        Self {
            quiet,
            in_text: false,
            in_thinking: false,
            pending_ws: String::new(),
            need_quote: false,
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
                writeln!(
                    err,
                    "INFO: ↻ {message} — retrying in {:.1}s ({attempt}/{max_retries})",
                    delay.as_secs_f64()
                )
                .ok();
                writeln!(err).ok();
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
                writeln!(err, "● {name} {preview}").ok();
                writeln!(err).ok();
            }
            TurnEvent::ToolResult { name, output } => {
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
                writeln!(err, "{marker}{name}: {head}").ok();
                for l in lines {
                    writeln!(err, "{indent}{l}").ok();
                }
                writeln!(err).ok();
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
        w.flush().ok();
    }

    /// Write text, inserting the `> ` quote prefix after each newline
    /// (and before a leading paragraph character) when `quote` is set.
    fn write_quoted<W: Write>(&mut self, w: &mut W, text: &str, quote: bool) {
        if !quote {
            write!(w, "{text}").ok();
            return;
        }
        for c in text.chars() {
            if self.need_quote && c != '\n' {
                write!(w, "> ").ok();
                self.need_quote = false;
            }
            if c == '\n' {
                self.need_quote = true;
            }
            write!(w, "{c}").ok();
        }
    }

    /// Close an open text block: the held-back trailing whitespace is
    /// dropped (the block ends trimmed) and two newlines are added — one
    /// to end the block's last line, one blank line after it.
    fn close_text<O: Write>(&mut self, out: &mut O) {
        if self.in_text {
            self.in_text = false;
            self.pending_ws.clear();
            write!(out, "\n\n").ok();
            out.flush().ok();
        }
    }

    /// Close an open thinking block the same way (on stderr).
    fn close_thinking<E: Write>(&mut self, err: &mut E) {
        if self.in_thinking {
            self.in_thinking = false;
            self.pending_ws.clear();
            write!(err, "\n\n").ok();
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
                },
                TurnEvent::Text {
                    delta: "answer".into(),
                },
            ],
        );
        assert_eq!(out, "answer\n\n");
        assert_eq!(err, "");
    }
}
