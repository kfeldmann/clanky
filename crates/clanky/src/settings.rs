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
//!
//! The layering is additive *unless a scope says otherwise*: the
//! `override` array in either file names the resources that scope takes
//! over from the user scope instead of adding to it (see
//! [`crate::config::Override`]). Loading therefore happens in two passes —
//! the `override` lists first, then the merged settings with those
//! overrides applied — because a project's `override` decides whether its
//! own user-scope file is read at all.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{self, Override};
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
    /// Resources this scope takes over from the user scope instead of
    /// adding to it: `["settings", "system", "skills"]`. Written as the
    /// plain strings in the file and validated into [`Override`]s here;
    /// an unknown name is an error, never a silently ignored entry.
    /// Override lists union across scopes — see [`Settings::overrides`].
    /// Deserialized through `deserialize_overrides` (which rejects
    /// unknown names); serialized as the plain names.
    #[serde(
        default,
        deserialize_with = "deserialize_overrides",
        serialize_with = "serialize_overrides"
    )]
    pub r#override: Option<Vec<Override>>,
}

/// Deserialize `override = ["system", "skills"]`, rejecting unknown names
/// with an error naming the accepted values. Kept out of the `serde`
/// derive because [`Override`]'s spelling in the file is not its variant
/// name and the failure has to be actionable.
fn deserialize_overrides<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<Override>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<Vec<String>>::deserialize(deserializer)?;
    raw.map(|names| {
        names
            .into_iter()
            .map(|name| {
                Override::parse(&name).ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "unknown override `{name}`; expected any of `settings`, `system`, `skills`"
                    ))
                })
            })
            .collect()
    })
    .transpose()
}

/// Serialize the override list back to its settings.toml spelling.
fn serialize_overrides<S>(
    overrides: &Option<Vec<Override>>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match overrides {
        Some(overrides) => serializer.serialize_some(
            &overrides
                .iter()
                .map(|over| over.as_str())
                .collect::<Vec<_>>(),
        ),
        None => serializer.serialize_none(),
    }
}

impl Settings {
    /// Load user- and project-scope settings and merge them, honoring the
    /// `override` lists found in either file.
    ///
    /// Two passes, because an `override` list decides which files count:
    /// the lists are collected first (from both files, always — a file
    /// that is itself overridden still gets a say in what is overridden),
    /// then the files are layered with the user scope left out for every
    /// overridden resource. `override = ["settings"]` in the project file
    /// therefore drops the user file, and the same line in the user file
    /// drops it for every project.
    pub fn load_layered() -> Result<Settings> {
        let user = config::user_dir().map(|dir| dir.join(config::SETTINGS_FILE));
        let project = config::project_dir().join(config::SETTINGS_FILE);
        Self::load_scopes(user.as_deref(), &project)
    }

    /// Pure form of [`Settings::load_layered`] for testing: layer the
    /// `user` settings file (if any) under the `project` one, applying the
    /// override lists found in either.
    pub fn load_scopes(user: Option<&Path>, project: &Path) -> Result<Settings> {
        let user = match user {
            Some(path) => Self::load_file(path)?,
            None => None,
        };
        let project = Self::load_file(project)?;

        // The override lists come from every file that exists — including
        // a file that is itself about to be dropped: it still gets a say
        // in what is overridden.
        let mut overrides = Vec::new();
        for settings in [&user, &project].into_iter().flatten() {
            overrides.extend(settings.overrides());
        }
        overrides.sort();
        overrides.dedup();

        // The values, from the files that still count. Only
        // `override = ["settings"]` matters here — the other kinds govern
        // other resources, not which settings files are read.
        let mut merged = Settings::default();
        if let Some(user) = user.filter(|_| !overrides.contains(&Override::Settings)) {
            merged.overlay(user);
        }
        if let Some(project) = project {
            merged.overlay(project);
        }
        // The result carries the union of every file's list (a dropped
        // file's list included), so the override in effect is reported
        // whole.
        merged.r#override = (!overrides.is_empty()).then_some(overrides);
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
        if let Some(over_overrides) = over.r#override {
            // Override lists union: a scope that overrides `skills` cannot
            // un-override a `system` its other scope took over — the
            // resources are independent, and dropping one silently is
            // exactly what this feature exists to avoid.
            match &mut self.r#override {
                Some(base) => {
                    for over in over_overrides {
                        if !base.contains(&over) {
                            base.push(over);
                        }
                    }
                    base.sort();
                }
                None => self.r#override = Some(over_overrides),
            }
        }
    }

    /// The override resources in effect, sorted and deduplicated. Empty
    /// when no scope overrides anything.
    pub fn overrides(&self) -> Vec<Override> {
        let mut overrides = self.r#override.clone().unwrap_or_default();
        overrides.sort();
        overrides.dedup();
        overrides
    }

    /// Whether conversations are recorded into session files. Sessions
    /// are on unless explicitly disabled (`session = false` in
    /// settings.toml, or the `--no-session` flag — the flag wins).
    pub fn record_sessions(&self) -> bool {
        self.session.unwrap_or(true)
    }
}

/// The effective settings of one session (M18): the merged values a
/// turn runs with — provider, model, and the settings fields that shape
/// it. Both front ends dump this at startup, and the session file
/// carries it as a `settings` record at session start and again
/// whenever settings change mid-session. Unset fields are elided in
/// the dump and in the records; each record is a full snapshot, so
/// folding a session's records in order (later records win per field)
/// reconstructs the settings state as of the end of that session.
///
/// Deliberately *not* the `Settings` struct: `prompt` (it becomes a
/// user record, not a configuration fact) and `session` (a recording
/// switch, not part of what a turn runs with) are excluded.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SessionSettings {
    /// Provider name (e.g. `deepinfra`).
    pub provider: Option<String>,
    /// Model identifier as known to the provider.
    pub model: Option<String>,
    /// Thinking budget in tokens or `off`.
    pub thinking: Option<String>,
    /// Sampling parameters (merged per key across layers).
    pub sampling: Option<SamplingParams>,
    /// Tool-loop round cap for one turn (`0` = unlimited).
    pub max_tool_rounds: Option<usize>,
    /// Retries for a retryable provider error (`0` = never retry).
    pub max_retries: Option<u32>,
    /// Project label shown at the left of the TUI input prompt.
    pub project: Option<String>,
    /// Default tool-call timeout in seconds (`None` = unbounded).
    pub timeout_seconds: Option<u64>,
}

impl SessionSettings {
    /// Overlay `over` on top of `self`, field by field (used when
    /// folding a session's settings records in order: later records win
    /// per field, exactly as they were written). `sampling` merges per
    /// key, like every settings layering.
    pub fn overlay(&mut self, over: &Self) {
        if over.provider.is_some() {
            self.provider = over.provider.clone();
        }
        if over.model.is_some() {
            self.model = over.model.clone();
        }
        if over.thinking.is_some() {
            self.thinking = over.thinking.clone();
        }
        if let Some(over_sampling) = &over.sampling {
            match &mut self.sampling {
                Some(base) => base.extend(over_sampling.clone()),
                None => self.sampling = Some(over_sampling.clone()),
            }
        }
        if over.max_tool_rounds.is_some() {
            self.max_tool_rounds = over.max_tool_rounds;
        }
        if over.max_retries.is_some() {
            self.max_retries = over.max_retries;
        }
        if over.project.is_some() {
            self.project = over.project.clone();
        }
        if over.timeout_seconds.is_some() {
            self.timeout_seconds = over.timeout_seconds;
        }
    }

    /// Whether the two states differ in at least one field. Equivalent
    /// to `a != b` (`sampling` merge aside); spelled out for intent at
    /// the recorder's change-detection call site.
    pub fn differs_from(&self, other: &Self) -> bool {
        self != other
    }

    /// One-line human description for the startup dump (M18): provider
    /// and model, then the merged settings; unset fields elided.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(provider) = &self.provider {
            parts.push(format!("provider {provider}"));
        }
        if let Some(model) = &self.model {
            parts.push(format!("model {model}"));
        }
        if let Some(thinking) = &self.thinking {
            parts.push(format!("thinking {thinking}"));
        }
        if let Some(sampling) = &self.sampling {
            parts.push(format!("sampling {}", describe_sampling(sampling)));
        }
        if let Some(rounds) = self.max_tool_rounds {
            parts.push(format!("max_tool_rounds {rounds}"));
        }
        if let Some(retries) = self.max_retries {
            parts.push(format!("max_retries {retries}"));
        }
        if let Some(project) = &self.project {
            parts.push(format!("project {project}"));
        }
        if let Some(seconds) = self.timeout_seconds {
            parts.push(format!("timeout {seconds}s"));
        }
        if parts.is_empty() {
            return "all defaults".into();
        }
        parts.join(" · ")
    }
}

impl From<&Settings> for SessionSettings {
    fn from(settings: &Settings) -> Self {
        Self {
            provider: settings.provider.clone(),
            model: settings.model.clone(),
            thinking: settings.thinking.clone(),
            sampling: settings.sampling.clone(),
            max_tool_rounds: settings.max_tool_rounds,
            max_retries: settings.max_retries,
            project: settings.project.clone(),
            timeout_seconds: settings.timeout_seconds,
        }
    }
}

/// Sampling parameters as `key=value` pairs for the status messages and
/// the startup dump. Values are shown as given: strings verbatim, other
/// JSON values in their JSON form.
pub fn describe_sampling(params: &SamplingParams) -> String {
    params
        .iter()
        .map(|(key, value)| match value {
            serde_json::Value::String(raw) => format!("{key}={raw}"),
            other => format!("{key}={other}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sampling(pairs: &[(&str, &str)]) -> SamplingParams {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect()
    }

    /// Layer two settings files (user then project), the way the binary
    /// does, from the path pair the older tests already build.
    fn layer(user: &Path, project: &Path) -> Settings {
        Settings::load_scopes(Some(user), project).unwrap()
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
        let merged = layer(
            Path::new("/nonexistent-user/settings.toml"),
            Path::new("/nonexistent-project/settings.toml"),
        );
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
        let merged = layer(&user, &project);

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
        let merged = layer(&user, &project);
        assert_eq!(merged.project.as_deref(), Some("project-proj"));

        // A layer that does not set it keeps the lower layer's value.
        let merged = layer(
            &user,
            Path::new("/nonexistent-clanky-project/settings.toml"),
        );
        assert_eq!(merged.project.as_deref(), Some("user-proj"));

        std::fs::remove_file(project).unwrap();
        assert_eq!(Settings::default().project, None, "unset means no label");
    }

    #[test]
    fn session_settings_exclude_prompt_and_session_switch() {
        // The session dump is the merged configuration a turn runs with:
        // `prompt` becomes a user record (not a configuration fact) and
        // `session` is a recording switch. Neither is part of it.
        let settings = Settings {
            provider: Some("deepinfra".into()),
            prompt: Some("hi".into()),
            session: Some(false),
            ..Settings::default()
        };
        let effective = SessionSettings::from(&settings);
        assert_eq!(effective.provider.as_deref(), Some("deepinfra"));
        assert_eq!(effective.project, None, "unset fields stay unset");
    }

    #[test]
    fn session_settings_overlay_folds_later_records() {
        // Folding is the `/resume` mechanism: later records win per
        // field, exactly as they were written.
        let mut state = SessionSettings {
            provider: Some("deepinfra".into()),
            model: Some("old/model".into()),
            sampling: Some(sampling(&[("temperature", "0.7"), ("seed", "42")])),
            timeout_seconds: Some(30),
            ..SessionSettings::default()
        };
        state.overlay(&SessionSettings {
            model: Some("new/model".into()),
            sampling: Some(sampling(&[("top_p", "0.9"), ("seed", "7")])),
            timeout_seconds: None, // a later unset field keeps the value
            ..SessionSettings::default()
        });
        assert_eq!(state.model.as_deref(), Some("new/model"));
        assert_eq!(state.timeout_seconds, Some(30), "unset does not clear");
        let sampling = state.sampling.unwrap();
        // `sampling` values are strings in these helpers (JSON-decoded
        // on the wire only), so compare as strings.
        assert_eq!(sampling["temperature"], serde_json::json!("0.7"));
        assert_eq!(sampling["top_p"], serde_json::json!("0.9"));
        assert_eq!(sampling["seed"], serde_json::json!("7"));
    }

    #[test]
    fn session_settings_describe_elides_unset_fields() {
        assert_eq!(SessionSettings::default().describe(), "all defaults");
        let effective = SessionSettings {
            provider: Some("deepinfra".into()),
            model: Some("m/x".into()),
            sampling: Some(sampling(&[("temperature", "0.7")])),
            timeout_seconds: Some(5),
            ..SessionSettings::default()
        };
        let described = effective.describe();
        assert!(described.contains("provider deepinfra"), "{described}");
        assert!(described.contains("model m/x"), "{described}");
        assert!(
            described.contains("sampling temperature=0.7"),
            "{described}"
        );
        assert!(described.contains("timeout 5s"), "{described}");
        assert!(!described.contains("thinking"), "unset elided: {described}");
        assert!(!described.contains("project"), "unset elided: {described}");
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let tmp = std::env::temp_dir().join("clanky-test-badkey");
        std::fs::create_dir_all(&tmp).unwrap();
        let path = write_temp_settings(&tmp, "provieder = \"oops\"\n");
        let err = Settings::load_scopes(None, &path).unwrap_err();
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
        let merged = layer(&user, &project);
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
        let merged = Settings::load_scopes(None, &path).unwrap();
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
    fn override_settings_is_positional_first_file_is_the_user_scope() {
        // The user settings file is the first of the pair, so the drop is
        // decided by position, exactly as `config::scopes` orders them.
        let tmp = std::env::temp_dir().join("clanky-test-override-positional");
        let user = write_temp_settings(
            &tmp.join("user"),
            "override = [\"settings\"]\nprovider = \"user-provider\"\n",
        );
        let project = write_temp_settings(&tmp.join("project"), "model = \"project-model\"\n");
        let merged = layer(&user, &project);
        assert_eq!(merged.provider, None, "the user file is the dropped one");
        assert_eq!(merged.model.as_deref(), Some("project-model"));

        // With no user scope configured at all (`$HOME` unknown), the
        // single file is never treated as a droppable user scope.
        let merged = Settings::load_scopes(None, &user).unwrap();
        assert_eq!(merged.provider.as_deref(), Some("user-provider"));

        std::fs::remove_dir_all(tmp).ok();
    }

    #[test]
    fn override_parses_known_names_and_merges_across_scopes() {
        let tmp = std::env::temp_dir().join("clanky-test-override-merge");
        let user = write_temp_settings(
            &tmp.join("user"),
            "override = [\"system\"]\nmodel = \"user-model\"\n",
        );
        let project = write_temp_settings(&tmp.join("project"), "override = [\"skills\"]\n");
        let merged = Settings::load_scopes(Some(&user), &project).unwrap();
        assert_eq!(
            merged.overrides(),
            vec![Override::System, Override::Skills],
            "lists union and sort; user settings still layered (not overridden)"
        );
        assert_eq!(merged.model.as_deref(), Some("user-model"));

        std::fs::remove_dir_all(tmp).ok();
    }

    #[test]
    fn an_unknown_override_is_an_error_naming_the_file() {
        let tmp = std::env::temp_dir().join("clanky-test-override-unknown");
        let project = write_temp_settings(&tmp, "override = [\"prompts\"]\n");
        let err = Settings::load_scopes(None, &project).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("settings.toml"), "{msg}");
        assert!(msg.contains("unknown override `prompts`"), "{msg}");
        assert!(
            msg.contains("`settings`, `system`, `skills`"),
            "the error lists the accepted values: {msg}"
        );

        std::fs::remove_dir_all(tmp).ok();
    }

    #[test]
    fn override_settings_drops_the_user_file() {
        let tmp = std::env::temp_dir().join("clanky-test-override-settings");
        let user = write_temp_settings(
            &tmp.join("user"),
            "provider = \"user-provider\"\nmodel = \"user-model\"\n\
             [sampling]\ntemperature = 0.7\n",
        );
        let project = write_temp_settings(
            &tmp.join("project"),
            "override = [\"settings\"]\nmodel = \"project-model\"\n",
        );
        let merged = Settings::load_scopes(Some(&user), &project).unwrap();
        assert_eq!(
            merged.provider, None,
            "the user file is not read at all: nothing is left to layer"
        );
        assert_eq!(merged.model.as_deref(), Some("project-model"));
        assert_eq!(merged.sampling, None, "the user [sampling] is gone too");
        assert_eq!(merged.overrides(), vec![Override::Settings]);

        std::fs::remove_dir_all(tmp).ok();
    }

    #[test]
    fn override_in_the_user_file_drops_it_for_every_project() {
        // Pointless but consistent: `override` is accepted in either file,
        // and in the user file it nullifies the user scope everywhere.
        let tmp = std::env::temp_dir().join("clanky-test-override-user-self");
        let user = write_temp_settings(
            &tmp.join("user"),
            "override = [\"settings\"]\nprovider = \"user-provider\"\n",
        );
        let project = write_temp_settings(&tmp.join("project"), "model = \"project-model\"\n");
        let merged = Settings::load_scopes(Some(&user), &project).unwrap();
        assert_eq!(merged.provider, None);
        assert_eq!(merged.model.as_deref(), Some("project-model"));

        std::fs::remove_dir_all(tmp).ok();
    }

    #[test]
    fn override_of_another_resource_keeps_the_user_settings() {
        // `override = ["system"]` governs the system prompt only; the user
        // settings file is still layered.
        let tmp = std::env::temp_dir().join("clanky-test-override-other");
        let user = write_temp_settings(&tmp.join("user"), "provider = \"user-provider\"\n");
        let project = write_temp_settings(&tmp.join("project"), "override = [\"system\"]\n");
        let merged = Settings::load_scopes(Some(&user), &project).unwrap();
        assert_eq!(merged.provider.as_deref(), Some("user-provider"));

        std::fs::remove_dir_all(tmp).ok();
    }

    #[test]
    fn override_serializes_to_its_settings_spelling() {
        // The write side is only used for diagnostics today, but it must
        // not invent variant names (`System`) in a settings file.
        let settings = Settings {
            r#override: Some(vec![Override::System, Override::Skills]),
            ..Settings::default()
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(value["override"], serde_json::json!(["system", "skills"]));
        assert_eq!(
            Settings::default().r#override,
            None,
            "an unset list serializes as null, never as an empty one"
        );
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
        let merged = Settings::load_scopes(None, &path).unwrap();
        let sampling = merged.sampling.as_ref().unwrap();
        assert_eq!(sampling["temperature"], serde_json::json!(0.7));
        assert_eq!(sampling["seed"], serde_json::json!(42));
        assert_eq!(sampling["stop"], serde_json::json!(["\n\n"]));
        assert_eq!(sampling["repetition_penalty"], serde_json::json!(1.1));

        std::fs::remove_file(path).unwrap();
    }
}
