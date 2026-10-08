//! Provider plugin registry and discovery (M8).
//!
//! Clanky core contains **no** provider-specific code. Every provider is a
//! separate executable named `clanky-provider-<name>` on `$PATH`, speaking
//! the JSONL protocol in `provider-protocol.md` over stdin/stdout. This
//! module is the only place core talks about plugins: it discovers them,
//! spawns them as long-lived processes, and hands back a [`ProviderSession`]
//! the turn loop can drive.
//!
//! Discovery is **PATH-only** (no sibling-of-binary scan, no settings
//! declaration in M8): `cargo install` puts both binaries in `~/.cargo/bin`,
//! which is on `$PATH`, so the default experience is zero-config. The plugin
//! *name* comes from the handshake, not the filename; the filename suffix is
//! only the discovery hint.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use clanky_protocol::{PluginInfo, ProcessOptions, ProcessTransport, ProviderClient};

use crate::config;
use crate::error::{Error, Result};

/// Default provider when neither settings nor CLI name one. This is a
/// product default (the documented zero-config experience), not provider
/// knowledge: it is just a name, resolved against discovered plugins.
pub const DEFAULT_PROVIDER: &str = "deepinfra";

/// Prefix that marks an executable as a Clanky provider plugin.
pub const PLUGIN_PREFIX: &str = "clanky-provider-";

/// One discovered plugin executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    /// Name derived from the filename suffix (`clanky-provider-deepinfra` →
    /// `deepinfra`). This is a *candidate* name; the plugin's real name comes
    /// from its handshake.
    pub name: String,
    /// Full path to the executable.
    pub path: PathBuf,
}

/// Process-lifetime cache of the `$PATH` scan. A filesystem scan should not
/// run on every key press (the `/provider` picker asks repeatedly).
static CACHE: OnceLock<Mutex<Vec<Plugin>>> = OnceLock::new();

fn cache() -> &'static Mutex<Vec<Plugin>> {
    CACHE.get_or_init(|| {
        let path = std::env::var_os("PATH").unwrap_or_default();
        Mutex::new(discover_in_path(&path))
    })
}

/// Discover provider plugins on `$PATH` (cached for the process lifetime).
pub fn discover() -> Vec<Plugin> {
    cache().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Re-scan `$PATH` and replace the cache.
pub fn refresh() {
    let path = std::env::var_os("PATH").unwrap_or_default();
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = discover_in_path(&path);
}

/// Replace the discovered plugin list (unit tests only; the real path is a
/// `$PATH` scan). Integration tests run in their own process and can use a
/// fake `$PATH` with [`refresh`] instead.
#[cfg(test)]
pub(crate) fn override_for_test(plugins: Vec<Plugin>) {
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = plugins;
}

/// Discover provider plugins in a specific `PATH` value. Split out so tests
/// can drive a temp directory without touching the process environment.
pub fn discover_in_path(path: &std::ffi::OsStr) -> Vec<Plugin> {
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in std::env::split_paths(path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Some(suffix) = plugin_suffix(name) else {
                continue;
            };
            if suffix.is_empty() || !is_executable(&entry.path()) {
                continue;
            }
            // First hit on $PATH wins (the shell's own resolution order).
            found.entry(suffix.to_string()).or_insert(entry.path());
        }
    }
    found
        .into_iter()
        .map(|(name, path)| Plugin { name, path })
        .collect()
}

/// The plugin-name suffix of an executable file name, or `None` if it is not
/// a plugin. Handles Windows `PATHEXT`-style extensions (`clanky-provider-x.exe`).
fn plugin_suffix(file_name: &str) -> Option<&str> {
    let rest = file_name.strip_prefix(PLUGIN_PREFIX)?;
    let stem = match rest.rsplit_once('.') {
        Some((stem, ext)) if is_executable_extension(ext) => stem,
        _ => rest,
    };
    Some(stem)
}

/// Windows executable extensions (ignored elsewhere; harmless).
fn is_executable_extension(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "exe" | "cmd" | "bat" | "com"
    )
}

/// Whether `path` is a file the current user may execute.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Names of discovered providers, for the `/provider` picker and error
/// messages.
pub fn available() -> Vec<String> {
    discover().iter().map(|p| p.name.clone()).collect()
}

/// Resolve a provider name to a discovered plugin.
pub fn find(name: &str) -> Option<Plugin> {
    discover().into_iter().find(|plugin| plugin.name == name)
}

/// Whether a discovered plugin provides `name`.
pub fn is_known(name: &str) -> bool {
    find(name).is_some()
}

/// The resolved command for a provider name (for display in pickers).
pub fn command_for(name: &str) -> Option<PathBuf> {
    find(name).map(|plugin| plugin.path)
}

/// Where a plugin's stderr is written: `.clanky/logs/plugin-<name>.log`.
pub fn plugin_log_path(name: &str) -> PathBuf {
    config::project_dir()
        .join(config::LOGS_DIR)
        .join(format!("plugin-{name}.log"))
}

/// Spawn and handshake a provider plugin.
///
/// The returned session owns a long-lived process (one per session, spec
/// decision #2): startup and model-list cost are paid once, and the plugin is
/// shut down on drop (stdin EOF per spec §1). The `PluginInfo` comes from the
/// handshake, so the caller learns the provider's real name, capabilities, and
/// default model without any hardcoded table.
pub fn create(name: &str) -> Result<ProviderSession> {
    let plugin = find(name).ok_or_else(|| unknown_provider(name))?;
    let mut options = ProcessOptions::new(plugin.path.to_string_lossy().into_owned());
    if std::env::var_os(clanky_protocol::process::DEBUG_ENV).is_none() {
        options = options.with_log(plugin_log_path(name));
    }
    let transport = ProcessTransport::spawn(options).map_err(|source| Error::PluginSpawn {
        name: name.to_string(),
        command: plugin.path.clone(),
        log: plugin_log_path(name),
        source: Box::new(source),
    })?;

    let mut client = ProviderClient::new(transport);
    client.handshake().map_err(|source| match source {
        // Name the plugin and both versions (plan §7): the generic
        // protocol error does not know which plugin was speaking.
        clanky_protocol::Error::VersionMismatch { peer } => Error::PluginProtocolVersion {
            name: name.to_string(),
            peer,
            supported: clanky_protocol::PROTOCOL_VERSION,
        },
        source => Error::PluginHandshake {
            name: name.to_string(),
            log: plugin_log_path(name),
            source: Box::new(source),
        },
    })?;

    // Design decision 4: the plugin's real name comes from the handshake.
    // The `clanky-provider-*` filename is only the discovery hint, so the two
    // must agree (otherwise `--provider <name>` and discovery would disagree
    // about what the plugin is called).
    let reported = client
        .peer()
        .map(|info| info.name.clone())
        .unwrap_or_default();
    if reported != name {
        return Err(Error::PluginNameMismatch {
            requested: name.to_string(),
            reported,
        });
    }
    Ok(ProviderSession::new(client))
}

/// A long-lived plugin process plus its protocol client.
///
/// `Send` so the TUI can move it into a turn worker and back.
#[derive(Debug)]
pub struct ProviderSession {
    client: ProviderClient<ProcessTransport>,
}

impl ProviderSession {
    fn new(client: ProviderClient<ProcessTransport>) -> Self {
        Self { client }
    }

    /// The plugin's handshake identity: real name, capabilities, default
    /// model. The name is verified against the discovered name in [`create`].
    pub fn info(&self) -> &PluginInfo {
        self.client
            .peer()
            .expect("handshake performed when the session was created")
    }

    /// The provider name (from the handshake). The TUI compares this against
    /// the active provider so a `/provider` switch discards the old process
    /// rather than reusing it for the new provider.
    pub fn name(&self) -> &str {
        &self.info().name
    }

    /// The provider's advertised default model, when it has one.
    pub fn default_model(&self) -> Option<&str> {
        self.info().default_model.as_deref()
    }

    /// Mutable access to the protocol client (chat, list models, cancel).
    pub fn client_mut(&mut self) -> &mut ProviderClient<ProcessTransport> {
        &mut self.client
    }

    /// A handle for cancelling the plugin from another thread (spec §7).
    pub fn cancel_handle(&self) -> clanky_protocol::CancelHandle {
        self.client.transport().cancel_handle()
    }

    /// Whether the plugin process is still running. A crashed session is
    /// discarded and respawned on next use (spec §8 restart policy).
    pub fn is_alive(&self) -> bool {
        self.client.transport().is_alive()
    }

    /// Where this plugin's stderr is logged.
    pub fn log_path(&self) -> Option<&Path> {
        self.client.transport().log_path()
    }
}

/// The error for a name that no discovered plugin provides, listing what is
/// available and, for the built-in default, how to install it.
fn unknown_provider(name: &str) -> Error {
    let available = available();
    if name == DEFAULT_PROVIDER {
        return Error::DefaultProviderMissing {
            name: name.to_string(),
            binary: format!("{PLUGIN_PREFIX}{name}"),
            available,
        };
    }
    Error::UnknownProvider {
        name: name.to_string(),
        available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_executable(dir: &Path, name: &str, mode: u32) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(mode);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
        path
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clanky-discovery-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn path_of(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs.iter().map(|d| d.to_path_buf())).unwrap()
    }

    /// The discovery cache is process-wide, so tests that replace it must not
    /// interleave; this lock serializes them.
    fn lock_discovery() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn discovers_executables_and_derives_names() {
        let dir = temp_dir("basic");
        write_executable(&dir, "clanky-provider-deepinfra", 0o755);
        write_executable(&dir, "clanky-provider-hello", 0o755);
        // Not executable: skipped.
        write_executable(&dir, "clanky-provider-nope", 0o644);
        // Wrong prefix: ignored.
        write_executable(&dir, "other-tool", 0o755);

        let found = discover_in_path(&path_of(&[&dir]));
        let names: Vec<&str> = found.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["deepinfra", "hello"]);
        assert!(found[0].path.ends_with("clanky-provider-deepinfra"));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn earlier_path_entries_win_and_duplicates_are_deduped() {
        let first = temp_dir("first");
        let second = temp_dir("second");
        write_executable(&first, "clanky-provider-x", 0o755);
        write_executable(&second, "clanky-provider-x", 0o755);

        let found = discover_in_path(&path_of(&[&first, &second]));
        assert_eq!(found.len(), 1, "same name is not listed twice");
        assert!(found[0].path.starts_with(&first), "first on PATH wins");

        std::fs::remove_dir_all(first).ok();
        std::fs::remove_dir_all(second).ok();
    }

    #[test]
    fn bare_prefix_is_not_a_plugin() {
        let dir = temp_dir("bare");
        write_executable(&dir, "clanky-provider-", 0o755);
        let found = discover_in_path(&path_of(&[&dir]));
        assert!(found.is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    /// A fake plugin script that handshakes and answers a chat.
    fn write_fake_plugin(dir: &Path, suffix: &str) -> PathBuf {
        let path = write_executable(dir, &format!("{PLUGIN_PREFIX}{suffix}"), 0o755);
        std::fs::write(
            &path,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"hello"'*) printf '%s\n' '{"type":"hello","protocolVersion":1,"name":"'$0'","capabilities":{}}' ;;
  esac
done
"#
            .replace("'$0'", "fake"),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[test]
    fn create_spawns_and_handshakes_a_discovered_plugin() {
        let _guard = lock_discovery();
        let dir = temp_dir("create");
        let plugin = write_fake_plugin(&dir, "fake");
        crate::provider::override_for_test(vec![Plugin {
            name: "fake".into(),
            path: plugin.clone(),
        }]);

        let session = create("fake").expect("spawn + handshake");
        // The real name comes from the handshake, not the filename.
        assert_eq!(session.info().name, "fake");
        assert_eq!(session.default_model(), None);
        assert!(session.is_alive());
        assert_eq!(session.name(), "fake");
        drop(session);

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn create_names_the_plugin_and_versions_on_a_protocol_mismatch() {
        let _guard = lock_discovery();
        let dir = temp_dir("version");
        let path = write_executable(&dir, "clanky-provider-future", 0o755);
        std::fs::write(
            &path,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"hello"'*)
      printf '%s\n' '{"type":"hello","protocolVersion":2,"name":"future","capabilities":{}}'
      ;;
  esac
done
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        crate::provider::override_for_test(vec![Plugin {
            name: "future".into(),
            path,
        }]);

        let err = create("future").unwrap_err();
        let message = err.to_string();
        // A valid `hello` with a foreign version maps to a dedicated error
        // that names the plugin and both versions (plan §7).
        assert!(
            message.contains("provider plugin `future` requires protocol v2"),
            "{message}"
        );
        assert!(message.contains("clanky speaks v1"), "{message}");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn create_rejects_a_plugin_whose_handshake_name_differs() {
        let _guard = lock_discovery();
        let dir = temp_dir("mismatch");
        let path = write_executable(&dir, "clanky-provider-imposter", 0o755);
        std::fs::write(
            &path,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"hello"'*) printf '%s\n' '{"type":"hello","protocolVersion":1,"name":"someone-else","capabilities":{}}' ;;
  esac
done
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        crate::provider::override_for_test(vec![Plugin {
            name: "imposter".into(),
            path,
        }]);

        let err = create("imposter").unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("reported the name `someone-else`"),
            "{message}"
        );
        assert!(message.contains("`imposter`"), "{message}");

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn create_reports_the_default_provider_install_hint() {
        let _guard = lock_discovery();
        crate::provider::override_for_test(Vec::new());
        let err = create(DEFAULT_PROVIDER).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("no plugin found for the default provider"),
            "{message}"
        );
        assert!(message.contains("clanky-provider-deepinfra"), "{message}");
    }

    #[test]
    fn create_lists_discovered_plugins_for_unknown_names() {
        let _guard = lock_discovery();
        crate::provider::override_for_test(vec![Plugin {
            name: "other".into(),
            path: PathBuf::from("/bin/false"),
        }]);
        let err = create("nope").unwrap_err();
        let message = err.to_string();
        assert!(message.contains("unknown provider `nope`"), "{message}");
        assert!(message.contains("other"), "{message}");
    }

    #[test]
    fn plugin_suffix_handles_windows_extensions() {
        assert_eq!(plugin_suffix("clanky-provider-x.exe"), Some("x"));
        assert_eq!(plugin_suffix("clanky-provider-x"), Some("x"));
        assert_eq!(plugin_suffix("clanky-provider-x.tar"), Some("x.tar"));
        assert_eq!(plugin_suffix("clanky-other-x"), None);
    }
}
