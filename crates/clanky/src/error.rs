//! Error types and exit-code conventions.
//!
//! Exit codes:
//! - `0` — success
//! - `1` — runtime error (this module's variants)
//! - `2` — usage error (handled by `clap` itself)

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
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

    #[error("unknown provider `{name}`; discovered plugins: {available}", available = available.join(", "))]
    UnknownProvider {
        name: String,
        /// Discovered `clanky-provider-*` names (possibly empty).
        available: Vec<String>,
    },

    #[error(
        "no plugin found for the default provider `{name}`: install `{binary}` (e.g. `cargo install {binary}`) so it is on $PATH"
    )]
    DefaultProviderMissing {
        name: String,
        binary: String,
        available: Vec<String>,
    },

    #[error("failed to start provider plugin `{name}` from {}: {source} (plugin log: {})", command.display(), log.display())]
    PluginSpawn {
        name: String,
        command: PathBuf,
        log: PathBuf,
        #[source]
        source: Box<clanky_protocol::Error>,
    },

    #[error("provider plugin `{name}` failed its handshake: {source} (plugin log: {})", log.display())]
    PluginHandshake {
        name: String,
        log: PathBuf,
        #[source]
        source: Box<clanky_protocol::Error>,
    },

    #[error(
        "provider plugin `{name}` requires protocol v{peer}, but clanky speaks v{supported}; upgrade clanky or the plugin"
    )]
    PluginProtocolVersion {
        name: String,
        peer: u32,
        supported: u32,
    },

    #[error(
        "provider plugin `{requested}` reported the name `{reported}` at handshake; the name must match the `clanky-provider-*` filename suffix"
    )]
    PluginNameMismatch { requested: String, reported: String },

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

    #[error("failed to write session {}: {source}", path.display())]
    WriteSession {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("invalid session file {}: {message}", path.display())]
    InvalidSession { path: PathBuf, message: String },

    #[error("unsupported session format version {found} in {} (this build reads version {supported})", path.display())]
    UnsupportedSessionVersion {
        path: PathBuf,
        found: u32,
        supported: u32,
    },

    #[error("session `{0}` already exists")]
    SessionExists(String),

    #[error(
        "invalid session name `{0}`: use letters, digits, `-`, `_` and `.` only (max 64 chars)"
    )]
    InvalidSessionName(String),

    #[error("no session named `{0}` in .clanky/sessions")]
    SessionNotFound(String),

    #[error("cannot use --resume in non-interactive mode")]
    ResumeNonInteractive,

    #[error("$EDITOR is not set; set it to edit the prompt buffer (ctrl+e)")]
    NoEditor,

    #[error("editor failed: {0}")]
    EditorFailed(String),
}

pub type Result<T> = std::result::Result<T, Error>;
