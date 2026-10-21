//! Settings loading and layering.
//!
//! Precedence (later wins):
//! 1. user scope:    `$HOME/.clanky/settings.toml`
//! 2. project scope: `./.clanky/settings.toml`
//! 3. CLI flags
//!
//! Layering is per-field: a scope only overrides fields it actually sets.
//! The `sampling` map merges per key: a higher layer wins key-by-key and
//! keys it does not set are kept from lower layers.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::{Error, Result};

/// Sampling parameters as `key -> value` pairs, e.g. `"temperature" ->
/// 0.7`. Values may be any TOML value and are passed to the provider
/// verbatim (the backend validates them); untyped strings are decoded as
/// JSON when they parse as JSON (see `turn::sampling_from`), so
/// `temperature = "0.7"` and `temperature = 0.7` mean the same thing.
pub type SamplingParams = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Provider name (e.g. `deepinfra`).
    pub provider: Option<String>,
    /// Model identifier as known to the provider.
    pub model: Option<String>,
    /// Thinking budget in tokens (e.g. `1024`) or `off`.
    pub thinking: Option<String>,
    /// Sampling parameters. Merged per key across layers: keys set in a
    /// higher layer (project file, then CLI) win; other keys are kept.
    pub sampling: Option<SamplingParams>,
    /// Tool-loop round cap for one turn (`0` = unlimited). Unset means the
    /// built-in default.
    pub max_tool_rounds: Option<usize>,
    /// Retries for a retryable provider error (`rateLimit`, `backend`,
    /// network timeouts) before a turn gives up (`0` = never retry).
    /// Unset means the built-in default.
    pub max_retries: Option<u32>,
    /// Prompt, when supplied on the command line.
    pub prompt: Option<String>,
    /// Whether sessions are recorded. `session = false` (or the CLI
    /// `--no-session` flag) turns recording off in both front ends;
    /// unset means recorded.
    pub session: Option<bool>,
    /// Project label shown at the left of the TUI input prompt (e.g.
    /// `project = "clanky"` renders `clanky  ❯ `). Display-only: the
    /// print front end ignores it.
    pub project: Option<String>,
    /// Default tool-call timeout in seconds, applied to any tool call that
    /// does not pass its own `timeout_seconds`. Unset (default) means
    /// unbounded — unchanged behavior.
    pub timeout_seconds: Option<u64>,
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
    /// `sampling` merges per key rather than wholesale, so a key supplied
    /// in only one layer is never dropped.
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
        if let Some(over_sampling) = over.sampling {
            match &mut self.sampling {
                Some(base) => base.extend(over_sampling),
                None => self.sampling = Some(over_sampling),
            }
        }
        if over.max_tool_rounds.is_some() {
            self.max_tool_rounds = over.max_tool_rounds;
        }
        if over.max_retries.is_some() {
            self.max_retries = over.max_retries;
        }
        if over.prompt.is_some() {
            self.prompt = over.prompt;
        }
        if over.session.is_some() {
            self.session = over.session;
        }
        if over.project.is_some() {
            self.project = over.project;
        }
        if over.timeout_seconds.is_some() {
            self.timeout_seconds = over.timeout_seconds;
        }
    }

    /// Whether conversations are recorded into session files. Sessions
    /// are on unless explicitly disabled (`session = false` in
    /// settings.toml, or the `--no-session` flag — the flag wins).
    pub fn record_sessions(&self) -> bool {
        self.session.unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampling(pairs: &[(&str, &str)]) -> SamplingParams {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect()
    }

    fn write_temp_settings(dir: &Path, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("settings.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn timeout_seconds_parses_from_toml() {
        let tmp = std::env::temp_dir();
        let path = write_temp_settings(&tmp.join("clanky-test-timeout"), "timeout_seconds = 5\n");
        let settings = Settings::load_file(&path).unwrap().unwrap();
        assert_eq!(settings.timeout_seconds, Some(5));
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
    fn timeout_seconds_overlays_per_field() {
        let base = Settings {
            timeout_seconds: Some(30),
            ..Settings::default()
        };
        // A setting layer that does not set it keeps the lower value.
        let mut merged = base.clone();
        merged.overlay(Settings::default());
        assert_eq!(merged.timeout_seconds, Some(30));
        // A higher layer wins.
        let mut merged = base;
        merged.overlay(Settings {
            timeout_seconds: Some(5),
            ..Settings::default()
        });
        assert_eq!(merged.timeout_seconds, Some(5));
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
            Some(&serde_json::Value::String("0.7".into()))
        );

        std::fs::remove_file(user).unwrap();
        std::fs::remove_file(project).unwrap();
    }

    #[test]
    fn project_field_parses_and_overlays_per_layer() {
        let tmp = std::env::temp_dir().join("clanky-test-project-field");
        std::fs::create_dir_all(&tmp).unwrap();
        let user = write_temp_settings(
            &tmp.join("clanky-test-project-user"),
            "project = \"user-proj\"\n",
        );
        let project = write_temp_settings(
            &tmp.join("clanky-test-project-project"),
            "project = \"project-proj\"\n",
        );
        let merged = Settings::load_from_paths(&[user.clone(), project.clone()]).unwrap();
        assert_eq!(merged.project.as_deref(), Some("project-proj"));

        // A layer that does not set it keeps the lower layer's value.
        let merged = Settings::load_from_paths(&[
            user,
            PathBuf::from("/nonexistent-clanky-project/settings.toml"),
        ])
        .unwrap();
        assert_eq!(merged.project.as_deref(), Some("user-proj"));

        std::fs::remove_file(project).unwrap();
        assert_eq!(Settings::default().project, None, "unset means no label");
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
            Some(&serde_json::Value::String("0.5".into()))
        );
    }

    #[test]
    fn sampling_merges_per_key_across_scopes() {
        // Keys supplied in only one scope must survive the merge; only the
        // key set in both is taken from the later scope.
        let tmp = std::env::temp_dir();
        let user = write_temp_settings(
            &tmp.join("clanky-test-sampling-merge-user"),
            "[sampling]\ntemperature = \"0.7\"\nseed = 42\n",
        );
        let project = write_temp_settings(
            &tmp.join("clanky-test-sampling-merge-project"),
            "[sampling]\ntop_p = \"0.9\"\nseed = 7\n",
        );
        let merged = Settings::load_from_paths(&[user.clone(), project.clone()]).unwrap();
        let sampling = merged.sampling.as_ref().unwrap();

        assert_eq!(sampling["temperature"], serde_json::json!("0.7"));
        assert_eq!(sampling["top_p"], serde_json::json!("0.9"));
        assert_eq!(sampling["seed"], serde_json::json!(7));

        std::fs::remove_file(user).unwrap();
        std::fs::remove_file(project).unwrap();
    }

    #[test]
    fn sampling_merges_per_key_from_cli() {
        let mut settings = Settings {
            sampling: Some(sampling(&[("temperature", "0.7"), ("seed", "42")])),
            ..Settings::default()
        };
        settings.overlay(Settings {
            sampling: Some(sampling(&[("top_p", "0.9"), ("seed", "7")])),
            ..Settings::default()
        });
        let sampling = settings.sampling.as_ref().unwrap();

        assert_eq!(sampling["temperature"], serde_json::json!("0.7"));
        assert_eq!(sampling["top_p"], serde_json::json!("0.9"));
        assert_eq!(sampling["seed"], serde_json::json!("7"));
    }

    #[test]
    fn session_field_parses_and_defaults_to_recording() {
        let tmp = std::env::temp_dir().join("clanky-test-session-field");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = write_temp_settings(&tmp, "session = false\n");
        let merged = Settings::load_from_paths(std::slice::from_ref(&path)).unwrap();
        assert!(!merged.record_sessions(), "`session = false` must disable");

        std::fs::remove_file(path).unwrap();
        assert!(Settings::default().record_sessions(), "unset means on");
    }

    #[test]
    fn session_field_overlays_per_layer() {
        // Project `session = false`, then CLI leaving it unset keeps the
        // off state; the CLI flag (an overlay of `Some(false)`) wins.
        let mut settings = Settings {
            session: Some(false),
            ..Settings::default()
        };
        settings.overlay(Settings::default());
        assert!(!settings.record_sessions());
        settings.overlay(Settings {
            session: Some(false),
            ..Settings::default()
        });
        assert!(!settings.record_sessions());
    }

    #[test]
    fn sampling_values_of_any_toml_type_load() {
        // Numbers, booleans, arrays, tables: whatever the user writes in
        // `[sampling]` is accepted as-is — validation is the backend's job.
        let tmp = std::env::temp_dir().join("clanky-test-sampling-types");
        let path = write_temp_settings(
            &tmp,
            "[sampling]\n\
             temperature = 0.7\n\
             seed = 42\n\
             stop = [\"\\n\\n\"]\n\
             repetition_penalty = 1.1\n",
        );
        let merged = Settings::load_from_paths(std::slice::from_ref(&path)).unwrap();
        let sampling = merged.sampling.as_ref().unwrap();
        assert_eq!(sampling["temperature"], serde_json::json!(0.7));
        assert_eq!(sampling["seed"], serde_json::json!(42));
        assert_eq!(sampling["stop"], serde_json::json!(["\n\n"]));
        assert_eq!(sampling["repetition_penalty"], serde_json::json!(1.1));

        std::fs::remove_file(path).unwrap();
    }
}
