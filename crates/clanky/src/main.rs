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
    let output = {
        let mut out = stdout.lock();
        let mut err = stderr.lock();
        let result = turn::run_turn_with(
            session.client_mut(),
            &tools::default_tools(),
            &mut messages,
            &config,
            &mut |event: TurnEvent| sink.event(event, &mut out, &mut err),
        );
        // A thinking block that ran to the end of the turn still needs
        // its closing separator.
        sink.close_thinking(&mut err);
        result
    }?;

    // The response text was streamed incrementally; finish the line.
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    if !output.text.ends_with('\n') {
        writeln!(stdout)?;
    }
    stdout.flush()?;
    Ok(())
}

/// Print-mode event sink: response text to stdout, activity to stderr so
/// `clanky -p ... | grep` style pipes stay clean. With `--quiet` (`-q`),
/// only the response text and errors are printed — no thinking, tool
/// calls, or tool results.
///
/// `event` writes into the callers' streams so tests can capture output;
/// the real run passes the process stdout/stderr.
struct PrintSink {
    quiet: bool,
    /// Whether a thinking block is currently streaming (to `err`).
    in_thinking: bool,
    /// Trailing whitespace held back from the thinking stream so the
    /// block can be closed with a clean trim (the milestone asks for the
    /// thinking text trimmed, then two newlines: one to end the block's
    /// last line and one blank line after it).
    pending_ws: String,
}

impl PrintSink {
    fn new(quiet: bool) -> Self {
        Self {
            quiet,
            in_thinking: false,
            pending_ws: String::new(),
        }
    }

    fn event<O: Write, E: Write>(&mut self, event: TurnEvent, out: &mut O, err: &mut E) {
        match event {
            TurnEvent::Text { delta } => {
                self.close_thinking(err);
                write!(out, "{delta}").ok();
                out.flush().ok();
            }
            TurnEvent::Thinking { delta } => {
                if self.quiet {
                    return;
                }
                self.in_thinking = true;
                // Hold trailing whitespace back: if the block ends here,
                // it is dropped so the close can trim cleanly. If more
                // text arrives, the held whitespace is emitted first.
                let trimmed = delta.trim_end_matches(char::is_whitespace);
                let ws = &delta[trimmed.len()..];
                if !self.pending_ws.is_empty() && !trimmed.is_empty() {
                    write!(err, "{}", self.pending_ws).ok();
                }
                if !trimmed.is_empty() {
                    write!(err, "{trimmed}").ok();
                }
                self.pending_ws.push_str(ws);
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
                self.close_thinking(err);
                if self.quiet {
                    return;
                }
                writeln!(
                    err,
                    "  ↻ {message} — retrying in {:.1}s ({attempt}/{max_retries})",
                    delay.as_secs_f64()
                )
                .ok();
            }
            // Activity entries end with a blank line so consecutive tool
            // rounds and the following answer text breathe, matching the
            // TUI transcript's one-blank-line-per-entry spacing.
            TurnEvent::ToolCall { name, arguments } => {
                self.close_thinking(err);
                if self.quiet {
                    return;
                }
                let preview = tools::truncate_call_arguments(&arguments, tools::CALL_PREVIEW_CHARS);
                writeln!(err, "● {name} {preview}\n").ok();
            }
            TurnEvent::ToolResult { name, output } => {
                self.close_thinking(err);
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

    /// Close an open thinking block: the held-back trailing whitespace is
    /// dropped (the block ends trimmed) and two newlines are added — one
    /// to end the block's last line, one blank line after it.
    fn close_thinking<E: Write>(&mut self, err: &mut E) {
        if self.in_thinking {
            self.in_thinking = false;
            self.pending_ws.clear();
            write!(err, "\n\n").ok();
        }
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
        sink.close_thinking(&mut err);
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
        assert_eq!(out, "answer");
        assert_eq!(err, "");
    }
}
