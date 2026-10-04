//! Prompt resolution: argv first, then piped stdin.

use std::io::{IsTerminal, Read};

use crate::error::{Error, Result};

/// Resolve the turn prompt from the command line or piped stdin.
///
/// - A non-empty argv prompt wins and stdin is never read (an interactive
///   shell would otherwise block).
/// - Otherwise, when stdin is piped/redirected, the prompt is read from it.
/// - If neither source provides text, [`Error::NoPrompt`].
pub fn resolve(from_argv: Option<String>) -> Result<String> {
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        std::io::stdin()
            .lock()
            .read_to_string(&mut buf)
            .map_err(|source| {
                // Wrapped as settings-free IO error via provider-free variant:
                // reuse the transport error shape for its Display.
                Error::Provider(clanky_protocol::Error::Io(source))
            })?;
        if let Some(text) = trimmed(&buf) {
            return Ok(text);
        }
        // Empty stdin: fall through to argv or the no-prompt error.
    }
    if let Some(text) = from_argv.and_then(|p| trimmed(&p)) {
        return Ok(text);
    }
    Err(Error::NoPrompt)
}

fn trimmed(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}
