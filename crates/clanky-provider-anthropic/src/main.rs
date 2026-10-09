//! `clanky-provider-anthropic` — the Anthropic provider as a plugin process.
//!
//! Clanky discovers this binary on `$PATH` (name convention
//! `clanky-provider-*`), spawns it, and speaks the provider protocol over
//! stdin/stdout (see `provider-protocol.md`). Two modes:
//!
//! - `clanky-provider-anthropic login` — headless OAuth login (PKCE +
//!   paste; run once per machine, or whenever the refresh token expires).
//! - default — serve the protocol over stdin/stdout.
//!
//! The login flow cannot live in the session: the plugin's stdin/stdout are
//! the protocol channel, so user interaction is impossible there (planning:
//! "the no-browser flow is an offline subcommand on the plugin binary").
//! Same shape as `ant auth login --no-browser`.

use std::process::ExitCode;

use clanky_protocol::{Handler, serve_stdio};

use clanky_provider_anthropic::Anthropic;

fn main() -> ExitCode {
    // `login` is an offline subcommand on the plugin binary (see module docs).
    if std::env::args().nth(1).as_deref() == Some("login") {
        return match clanky_provider_anthropic::login_and_store() {
            Ok(()) => {
                println!("login stored.");
                ExitCode::SUCCESS
            }
            Err(message) => {
                eprintln!("clanky-provider-anthropic: {message}");
                ExitCode::from(1)
            }
        };
    }

    // The serving process: credentials are loaded lazily (after `login` may
    // have written them), so construction always succeeds.
    let mut handler: Box<dyn Handler> = Box::new(Anthropic::from_env());
    match serve_stdio(&mut *handler) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // stderr is never protocol; Clanky captures it into the plugin
            // log. A clean exit still means "stdin closed".
            eprintln!("clanky-provider-anthropic: {err}");
            ExitCode::from(1)
        }
    }
}
