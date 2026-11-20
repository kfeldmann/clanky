//! `clanky-provider-deepinfra` — the DeepInfra provider as a plugin process.
//!
//! Clanky discovers this binary on `$PATH` (name convention
//! `clanky-provider-*`), spawns it, and speaks the provider protocol over
//! stdin/stdout (see `design-docs/provider-protocol.md`). The whole plugin is glue:
//! wire [`DeepInfra`] to [`clanky_protocol::serve_unavailable`].
//!
//! Auth stays here, never in core: the API key is read from
//! `DEEPINFRA_API_KEY` (or `DEEPINFRA_TOKEN`), and the base URL from
//! `DEEPINFRA_URL`. When no key is set, the plugin still starts and
//! handshakes, so the failure surfaces as a normal `auth` error on the first
//! turn ("no API key: set DEEPINFRA_API_KEY") instead of a dead process.

use std::process::ExitCode;

use clanky_protocol::{Handler, serve_unavailable};

use clanky_provider_deepinfra::{DEFAULT_MODEL, DeepInfra, PROVIDER_NAME};

fn main() -> ExitCode {
    let provider: Result<Box<dyn Handler>, clanky_protocol::Error> =
        DeepInfra::from_env().map(|provider| Box::new(provider) as Box<dyn Handler>);
    match serve_unavailable(PROVIDER_NAME, Some(DEFAULT_MODEL), provider) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // stderr is never protocol; Clanky captures it into the plugin
            // log. A clean exit still means "stdin closed".
            eprintln!("clanky-provider-deepinfra: {err}");
            ExitCode::from(1)
        }
    }
}
