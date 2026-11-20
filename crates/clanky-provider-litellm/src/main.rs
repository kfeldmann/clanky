//! `clanky-provider-litellm` — a LiteLLM proxy provider as a plugin process.
//!
//! Clanky discovers this binary on `$PATH` (name convention
//! `clanky-provider-*`), spawns it, and speaks the provider protocol over
//! stdin/stdout (see `design-docs/provider-protocol.md`). The whole plugin is glue:
//! wire [`LiteLlm`] to [`clanky_protocol::serve_unavailable`].
//!
//! Auth stays here, never in core: the API key is read from
//! `LITELLM_API_KEY`, the base URL from `LITELLM_BASE_URL` (default
//! `http://localhost:4000`); the built-in default model is
//! `claude-haiku-4-5`. When no key is set, the plugin still starts and
//! handshakes, so the failure surfaces as a normal `auth` error on the first
//! turn ("no API key: set LITELLM_API_KEY") instead of a dead process.

use std::process::ExitCode;

use clanky_protocol::{Handler, serve_unavailable};

use clanky_provider_litellm::{DEFAULT_MODEL, LiteLlm, PROVIDER_NAME};

fn main() -> ExitCode {
    let provider: Result<Box<dyn Handler>, clanky_protocol::Error> =
        LiteLlm::from_env().map(|provider| Box::new(provider) as Box<dyn Handler>);
    match serve_unavailable(PROVIDER_NAME, Some(DEFAULT_MODEL), provider) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // stderr is never protocol; Clanky captures it into the plugin
            // log. A clean exit still means "stdin closed".
            eprintln!("clanky-provider-litellm: {err}");
            ExitCode::from(1)
        }
    }
}
