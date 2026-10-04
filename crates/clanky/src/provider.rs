//! Provider registry (M1 stub).
//!
//! In M1 providers run in-process; the registry maps a provider name to its
//! handler. M8 replaces the in-process instantiation with sibling-binary
//! auto-discovery and spawned plugin processes (`clanky-provider-*`); the
//! name-to-provider mapping stays here.

use clanky_protocol::Handler;

use crate::error::{Error, Result};

/// Default provider when neither settings nor CLI name one.
pub const DEFAULT_PROVIDER: &str = "deepinfra";

/// Instantiate the named provider in-process.
pub fn create(name: &str) -> Result<Box<dyn Handler>> {
    match name {
        "deepinfra" => Ok(Box::new(clanky_provider_deepinfra::DeepInfra::from_env()?)),
        other => Err(Error::UnknownProvider(other.into())),
    }
}

/// Provider-recommended default model, used when no model is configured.
/// `None` means "no built-in default; require explicit configuration".
pub fn default_model(name: &str) -> Option<&'static str> {
    match name {
        "deepinfra" => Some(clanky_provider_deepinfra::DEFAULT_MODEL),
        _ => None,
    }
}
