//! Settings loading and layering.
//!
//! Precedence (later wins):
//! 1. user scope:    `$HOME/.clanky/settings.toml`
//! 2. project scope: `./.clanky/settings.toml`
//! 3. CLI flags
//!
//! Layering is per-field: a scope only overrides fields it actually sets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::{Error, Result};

/// Sampling parameters as `key -> raw value` strings
/// (e.g. `"temperature" -> "0.7"`). Values stay strings for now; the
/// provider validates and coerces them (M1/M2).
pub type SamplingParams = BTreeMap<String, String>;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Provider name (e.g. `deepinfra`).
    pub provider: Option<String>,
    /// Model identifier as known to the provider.
    pub model: Option<String>,
    /// Thinking budget or level (e.g. `1024`, `low`, `off`).
    pub thinking: Option<String>,
    /// Sampling parameters.
    pub sampling: Option<SamplingParams>,
    /// Prompt, when supplied on the command line.
    pub prompt: Option<String>,
}

impl Settings {
    /// Load user- and project-scope settings and merge them.
    pub fn load_layered() -> Result<Settings> {
        let paths: Vec<PathBuf> = config::scopes()
            .into_iter()
            .map(|scope| scope.join(config::SETTINGS_FILE))
            .collect();
        Self::load_from_paths(&paths)
    }

    /// Load settings from a sequence of paths, each later path overlaying
    /// the previous. Missing files are skipped silently.
    pub fn load_from_paths(paths: &[PathBuf]) -> Result<Settings> {
        let mut merged = Settings::default();
        for path in paths {
            if let Some(scope) = Self::load_file(path)? {
                merged.overlay(scope);
            }
        }
        Ok(merged)
    }

    /// Read and parse one settings file; `Ok(None)` if it does not exist.
    fn load_file(path: &Path) -> Result<Option<Settings>> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(Error::ReadSettings {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|source| Error::ParseSettings {
                path: path.to_path_buf(),
                source,
            })
    }

    /// Overlay `over` on top of `self`: fields set in `over` win.
    pub fn overlay(&mut self, over: Settings) {
        if over.provider.is_some() {
            self.provider = over.provider;
        }
        if over.model.is_some() {
            self.model = over.model;
        }
        if over.thinking.is_some() {
            self.thinking = over.thinking;
        }
        if over.sampling.is_some() {
            self.sampling = over.sampling;
        }
        if over.prompt.is_some() {
            self.prompt = over.prompt;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampling(pairs: &[(&str, &str)]) -> SamplingParams {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn write_temp_settings(dir: &Path, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn missing_files_yield_defaults() {
        let merged = Settings::load_from_paths(&[
            PathBuf::from("/nonexistent-user/settings.toml"),
            PathBuf::from("/nonexistent-project/settings.toml"),
        ])
        .unwrap();
        assert_eq!(merged, Settings::default());
    }

    #[test]
    fn project_overrides_user_per_field() {
        let tmp = std::env::temp_dir();
        let user = write_temp_settings(
            &tmp.join("clanky-test-user"),
            "provider = \"deepinfra\"\nmodel = \"user-model\"\n",
        );
        let project = write_temp_settings(
            &tmp.join("clanky-test-project"),
            "model = \"project-model\"\n[sampling]\ntemperature = \"0.7\"\n",
        );
        let merged = Settings::load_from_paths(&[user.clone(), project.clone()]).unwrap();

        assert_eq!(merged.provider.as_deref(), Some("deepinfra"));
        assert_eq!(merged.model.as_deref(), Some("project-model"));
        assert_eq!(
            merged.sampling.as_ref().unwrap().get("temperature"),
            Some(&"0.7".to_string())
        );

        std::fs::remove_file(user).unwrap();
        std::fs::remove_file(project).unwrap();
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let tmp = std::env::temp_dir().join("clanky-test-badkey");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = write_temp_settings(&tmp, "provieder = \"oops\"\n");
        let err = Settings::load_from_paths(std::slice::from_ref(&path)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("settings.toml"),
            "message should name the file: {msg}"
        );

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn overlay_cli_wins() {
        let mut settings = Settings {
            provider: Some("user-provider".into()),
            model: Some("user-model".into()),
            ..Settings::default()
        };
        settings.overlay(Settings {
            model: Some("cli-model".into()),
            sampling: Some(sampling(&[("temperature", "0.5")])),
            ..Settings::default()
        });
        assert_eq!(settings.provider.as_deref(), Some("user-provider"));
        assert_eq!(settings.model.as_deref(), Some("cli-model"));
        assert_eq!(
            settings.sampling.as_ref().unwrap().get("temperature"),
            Some(&"0.5".to_string())
        );
    }
}
