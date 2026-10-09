//! OAuth credentials: on-disk store under `~/.clanky/providers/anthropic/`
//! and token refresh (planning: "the no-browser flow is an offline
//! subcommand on the plugin binary").
//!
//! The store is a single JSON file, `credentials.json`, written with
//! user-only permissions:
//!
//! ```json
//! {"access_token": "sk-ant-oat01-…", "refresh_token": "sk-ant-ort01-…",
//!  "expires_at": 1791604609925}
//! ```
//!
//! `expires_at` is epoch milliseconds with a 5-minute skew subtracted (Pi's
//! convention), so "not expired" means " comfortably refreshable".

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Directory holding the credential store, under the user's `.clanky`.
pub fn credentials_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".clanky").join("providers").join("anthropic"))
}

/// Path of the credential file.
pub fn credentials_path() -> Option<PathBuf> {
    credentials_dir().map(|dir| dir.join(CREDENTIALS_FILE))
}

/// File name of the credential store.
pub const CREDENTIALS_FILE: &str = "credentials.json";

/// The 5-minute clock skew subtracted from real expiry (Pi's convention).
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;

/// Stored OAuth credentials, as serialized on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    /// Effective expiry, epoch milliseconds (real expiry minus the skew).
    pub expires_at: i64,
    /// Optional metadata recorded at refresh time (not sent anywhere).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token_expires_at: Option<i64>,
}

/// Read the credential file, if present and parseable.
pub fn load() -> Option<Credentials> {
    let path = credentials_path()?;
    read_credentials(&path)
}

/// Read credentials from an explicit path (testable form of [`load`]).
pub fn read_credentials(path: &Path) -> Option<Credentials> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write the credential file with user-only permissions (0600), creating the
/// directory if needed. `clanky-provider-anthropic login` and refresh both
/// land here.
pub fn store(credentials: &Credentials) -> Result<(), std::io::Error> {
    let Some(dir) = credentials_dir() else {
        return Err(std::io::Error::other("cannot determine $HOME"));
    };
    store_at(&dir.join(CREDENTIALS_FILE), credentials)
}

/// Write to an explicit path (testable form of [`store`]).
pub fn store_at(path: &Path, credentials: &Credentials) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(credentials)
        .map_err(|e| std::io::Error::other(format!("serialize: {e}")))?;
    std::fs::write(path, json)?;
    restrict_permissions(path)?;
    Ok(())
}

/// Restrict an existing file to user-only (best effort on non-unix).
fn restrict_permissions(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Current epoch time in milliseconds (0 when the clock is behind the epoch,
/// mirroring the litellm backend's `now_secs` fallback).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Compute the stored `expires_at` from a token response's `expires_in`
/// seconds, measured from now and minus the 5-minute skew.
pub fn expiry_from(expires_in_secs: i64) -> i64 {
    now_ms() + expires_in_secs * 1000 - EXPIRY_SKEW_MS
}

/// Whether the credentials exist and are past their (skewed) expiry.
impl Credentials {
    pub fn expired(&self) -> bool {
        self.expires_at <= now_ms()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("clanky-anthropic-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn credentials_roundtrip_through_disk() {
        let dir = temp_dir();
        let path = dir.join("credentials.json");
        let creds = Credentials {
            access_token: "sk-ant-oat01-x".into(),
            refresh_token: "sk-ant-ort01-y".into(),
            expires_at: 1_791_604_609_925,
            refresh_token_expires_at: None,
        };
        store_at(&path, &creds).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "credential file is user-only");
        }
        assert_eq!(read_credentials(&path).unwrap(), creds);
    }

    #[test]
    fn expiry_applies_the_five_minute_skew() {
        // expires_in 28800s -> now + 28800s - 300s (28800-300 = 28500).
        let expires = expiry_from(28_800);
        assert!((expires - (now_ms() + 28_500 * 1000)).abs() <= 1_500);
    }

    #[test]
    fn expired_checks_against_the_stored_skew() {
        let creds = Credentials {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: now_ms() - 1,
            refresh_token_expires_at: None,
        };
        assert!(creds.expired());
        let fresh = Credentials {
            expires_at: now_ms() + 60_000,
            ..creds.clone()
        };
        assert!(!fresh.expired());
    }
}
