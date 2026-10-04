//! Clanky — an AI coding agent for the terminal.
//!
//! M3: interactive TUI (chat view, streaming render, markdown wrap) with
//! the M2 agentic tool loop underneath. A prompt is run against the
//! DeepInfra provider via the protocol loopback transport. When stdin or
//! stdout is redirected (or `-p` is passed) the same turn runs in plain
//! command-line mode, streaming text to stdout and tool activity to
//! stderr. Spawned provider plugin processes arrive in M8.

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
    tui::run(tui::Launch::from_settings(&provider_name, &settings))
}

/// Non-interactive single turn: prompt from argv or stdin, response to
/// stdout, activity to stderr.
fn run_pipe(settings: &mut Settings) -> Result<()> {
    let prompt_text = clanky::prompt::resolve(settings.prompt.take())?;
    let provider_name = settings
        .provider
        .take()
        .unwrap_or_else(|| provider::DEFAULT_PROVIDER.into());

    let handler = provider::create(&provider_name)?;
    let config = turn::TurnConfig::from_settings(&provider_name, settings);

    let output = turn::run_turn(
        handler,
        &tools::default_tools(),
        context::system_messages(),
        &prompt_text,
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
        TurnEvent::ToolCall { name, arguments } => {
            eprintln!("\n● {name} {arguments}");
        }
        TurnEvent::ToolResult { name, output } => {
            let preview = clanky::tools::truncate_for_display(&output, 4_000);
            eprintln!("  └─ {name}: {preview}");
        }
    }
}
