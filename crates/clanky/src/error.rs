//! Error types and exit-code conventions.
//!
//! Exit codes:
//! - `0` — success
//! - `1` — runtime error (this module's variants)
//! - `2` — usage error (handled by `clap` itself)

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
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

    #[error("no prompt given: pass a prompt on the command line or pipe one via stdin")]
    NoPrompt,

    #[error("unknown provider `{0}`; available: deepinfra")]
    UnknownProvider(String),

    #[error("no model configured: set `model` in settings.toml or pass --model")]
    NoModel,

    #[error("invalid --sampling: {0}")]
    InvalidSampling(String),

    #[error("invalid --thinking value `{0}`; expected an integer token budget, or `off`")]
    InvalidThinking(String),

    #[error("io error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    #[error("provider error: {0}")]
    Provider(#[from] clanky_protocol::Error),

    #[error("tool loop exceeded {0} chat rounds; aborting to avoid an endless cycle")]
    ToolLoopLimit(usize),
}

pub type Result<T> = std::result::Result<T, Error>;
