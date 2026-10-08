//! Clanky — an AI coding agent for the terminal.
//!
//! M3: interactive TUI (chat view, streaming render, markdown wrap) with
//! the M2 agentic tool loop underneath. A prompt is run against the
//! provider plugin (discovered on `$PATH`, spawned as a subprocess, M8).
//! When stdin or stdout is redirected (or `-p` is passed) the same turn runs
//! in plain command-line mode, streaming text to stdout and tool activity to
//! stderr.

use std::io::{IsTerminal as _, Write as _};
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
        return run_pipe(&mut settings);
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
fn run_pipe(settings: &mut Settings) -> Result<()> {
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

    let output = turn::run_turn_with(
        session.client_mut(),
        &tools::default_tools(),
        &mut messages,
        &config,
        &mut print_event,
    )?;

    // The response text was streamed incrementally; finish the line.
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    if !output.text.ends_with('\n') {
        writeln!(stdout)?;
    }
    stdout.flush()?;
    Ok(())
}

/// Print stream events: response text to stdout, activity to stderr so
/// `clanky -p ... | grep` style pipes stay clean.
fn print_event(event: TurnEvent) {
    match event {
        TurnEvent::Text { delta } => {
            print!("{delta}");
            let _ = std::io::stdout().flush();
        }
        TurnEvent::Thinking { delta } => {
            eprint!("{delta}");
        }
        // Round summaries are for the session recorder (M4); pipe mode
        // has already streamed the text deltas.
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
            eprintln!(
                "  ↻ {message} — retrying in {:.1}s ({attempt}/{max_retries})",
                delay.as_secs_f64()
            );
        }
        TurnEvent::ToolCall { name, arguments } => {
            let preview = clanky::tools::truncate_call_arguments(
                &arguments,
                clanky::tools::CALL_PREVIEW_CHARS,
            );
            eprintln!("\n● {name} {preview}");
        }
        TurnEvent::ToolResult { name, output } => {
            let preview = clanky::tools::truncate_for_display(&output, 4_000);
            if clanky::tools::is_failed_result(&output) {
                eprintln!("  ✗ {name}: {preview}");
            } else {
                eprintln!("  └─ {name}: {preview}");
            }
        }
    }
}
