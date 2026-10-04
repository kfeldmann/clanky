//! Clanky — an AI coding agent for the terminal.
//!
//! M0 skeleton: CLI parsing, TOML settings layering, error/exit-code
//! plumbing. The interactive TUI and the provider pipeline arrive in
//! later milestones (M1–M3).

mod cli;
mod error;
mod settings;

use std::process::ExitCode;

use clap::Parser as _;

use crate::error::{Error, Result};
use crate::settings::Settings;

/// Exit code for runtime errors (clap already exits with 2 on usage errors).
const EXIT_ERROR: u8 = 1;

fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            let _ = err; // chain (if any) already surfaced via source
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run(cli: &cli::Cli) -> Result<()> {
    // File settings: user scope, then project scope overlaid on top.
    let mut settings = Settings::load_layered()?;
    // CLI flags win over both scopes.
    settings.overlay(cli.overrides());

    if cli.print {
        // Placeholder for M1: wire this into the provider pipeline.
        println!("clanky (M0 skeleton): no provider wired up yet.");
        println!("Effective settings:\n{}", settings.render_toml());
        return Ok(());
    }

    // Placeholder for M3: interactive TUI.
    Err(Error::NotImplementedYet(
        "interactive mode arrives in M3; for now use `-p`",
    ))
}
