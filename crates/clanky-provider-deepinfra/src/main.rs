//! `clanky-provider-deepinfra` — the DeepInfra provider as a plugin process.
//!
//! Clanky discovers this binary on `$PATH` (name convention
//! `clanky-provider-*`), spawns it, and speaks the provider protocol over
//! stdin/stdout (see `provider-protocol.md`). The whole plugin is glue: wire
//! [`DeepInfra`] to [`clanky_protocol::serve_stdio`].
//!
//! Auth stays here, never in core: the API key is read from
//! `DEEPINFRA_API_KEY` (or `DEEPINFRA_TOKEN`), and the base URL from
//! `DEEPINFRA_URL`. When no key is set, the plugin still starts and
//! handshakes, so the failure surfaces as a normal `auth` error on the first
//! turn ("no API key: set DEEPINFRA_API_KEY") instead of a dead process.

use std::process::ExitCode;

use clanky_protocol::{
    Capabilities, ChatDone, ChatRequest, ChunkPayload, Error, ErrorCode, Handler, ModelInfo,
    PluginInfo, serve_stdio,
};

use clanky_provider_deepinfra::{DEFAULT_MODEL, DeepInfra, PROVIDER_NAME};

/// A provider that could not be built (usually a missing API key): it
/// handshakes like DeepInfra and reports the stored error for every request.
/// This keeps the missing-key failure readable and request-scoped.
struct Unavailable {
    error: Error,
}

impl Handler for Unavailable {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: PROVIDER_NAME.into(),
            capabilities: Capabilities {
                list_models: true,
                thinking: true,
                tools: true,
            },
            default_model: Some(DEFAULT_MODEL.into()),
        }
    }

    fn list_models(&mut self) -> Result<Vec<ModelInfo>, Error> {
        Err(clone_error(&self.error))
    }

    fn chat(
        &mut self,
        _request: &ChatRequest,
        _sink: &mut dyn FnMut(ChunkPayload),
    ) -> Result<ChatDone, Error> {
        Err(clone_error(&self.error))
    }
}

/// `clanky_protocol::Error` is not `Clone`; reissue a provider error.
fn clone_error(err: &Error) -> Error {
    match err {
        Error::Provider {
            code,
            message,
            retryable,
            retry_after_ms,
        } => Error::Provider {
            code: *code,
            message: message.clone(),
            retryable: *retryable,
            retry_after_ms: *retry_after_ms,
        },
        other => Error::Provider {
            code: ErrorCode::Internal,
            message: other.to_string(),
            retryable: false,
            retry_after_ms: None,
        },
    }
}

fn main() -> ExitCode {
    let mut handler: Box<dyn Handler> = match DeepInfra::from_env() {
        Ok(provider) => Box::new(provider),
        Err(error) => Box::new(Unavailable { error }),
    };
    match serve_stdio(&mut *handler) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // stderr is never protocol; Clanky captures it into the plugin
            // log. A clean exit still means "stdin closed".
            eprintln!("clanky-provider-deepinfra: {err}");
            ExitCode::from(1)
        }
    }
}
