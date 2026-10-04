//! Error types and exit-code conventions.
//!
//! Exit codes:
//! - `0` — success
//! - `1` — runtime error (this module's variants)
//! - `2` — usage error (handled by `clap` itself)

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("interactive mode is not implemented yet: {0}")]
    NotImplementedYet(&'static str),

    #[error("cannot locate home directory; set $HOME")]
    NoHomeDir,

    #[error("failed to read settings file {}: {source}", path.display())]
    ReadSettings {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid settings in {}: {source}", path.display())]
    ParseSettings {
        path: PathBuf,
        source: toml::de::Error,
    },
}

pub type Result<T> = std::result::Result<T, Error>;
