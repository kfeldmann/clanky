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

use clap::{CommandFactory as _, Parser as _};

use clanky::cli;
use clanky::context;
use clanky::error::Result;
use clanky::provider;
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

    if cli.print || !tui::available() {
        if cli.resume.is_some() {
            return Err(clanky::error::Error::ResumeNonInteractive);
        }
        // No prompt on the command line and no piped stdin: nothing to do
        // in command-line mode, so show usage instead of a cryptic error.
        if cli.prompt.is_empty() && !cli.print && std::io::stdin().is_terminal() {
            eprint!("{}", cli::Cli::command().render_help());
            std::process::exit(2);
        }
        return run_pipe(&mut settings, cli.quiet);
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
fn run_pipe(settings: &mut Settings, quiet: bool) -> Result<()> {
    let prompt_text = clanky::prompt::resolve(settings.prompt.take())?;
    let provider_name = settings
        .provider
        .take()
        .unwrap_or_else(|| provider::DEFAULT_PROVIDER.into());

    // One long-lived plugin process for the turn (M8). It is shut down when
    // the session drops (stdin EOF per spec §1).
    let mut session = provider::create(&provider_name)?;
    let default_model = session.default_model().map(str::to_string);
    let config = turn::TurnConfig::from_settings(settings, default_model.as_deref());

    let mut messages = context::system_messages();
    messages.push(clanky_protocol::ChatMessage::user(&prompt_text));

    let mut sink = PrintSink::new(quiet);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let result = {
        let mut out = stdout.lock();
        let mut err = stderr.lock();
        let result = turn::run_turn_with(
            session.client_mut(),
            &tools::default_tools(),
            &mut messages,
            &config,
            &mut |event: TurnEvent| sink.event(event, &mut out, &mut err),
        );
        // A block that ran to the end of the turn (a thinking block, or
        // the answer text) still needs its closing separator: the trim
        // plus blank line were held back while it was open.
        sink.close_blocks(&mut out, &mut err);
        result
    };

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
}

impl PrintSink {
    fn new(quiet: bool) -> Self {
        Self {
            quiet,
            in_text: false,
            in_thinking: false,
            pending_ws: String::new(),
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
                self.hold_ws(out, delta);
            }
            TurnEvent::Thinking { delta } => {
                if self.quiet {
                    return;
                }
                // The thinking text ends when the answer text (or the
                // next event) starts; close that block first.
                self.close_text(out);
                self.in_thinking = true;
                self.hold_ws(err, delta);
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
                    "  ↻ {message} — retrying in {:.1}s ({attempt}/{max_retries})",
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
    /// block can close on a clean trim.
    fn hold_ws<W: Write>(&mut self, w: &mut W, delta: String) {
        let trimmed = delta.trim_end_matches(char::is_whitespace);
        let ws = &delta[trimmed.len()..];
        if !self.pending_ws.is_empty() && !trimmed.is_empty() {
            write!(w, "{}", self.pending_ws).ok();
        }
        if !trimmed.is_empty() {
            write!(w, "{trimmed}").ok();
        }
        self.pending_ws.push_str(ws);
        w.flush().ok();
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
        assert_eq!(err, "hmm\n\n");
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
        assert_eq!(err, "a\nb\n\n");
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
        assert_eq!(err, "hmm\n\n");
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
