//! `$EDITOR` integration (M7): edit the prompt buffer in an external
//! editor. The TUI suspends (leaves the alternate screen, disables raw
//! mode), the editor inherits the terminal, and on exit the TUI resumes
//! with the edited text. Any keybindings here are only reachable when no
//! picker is open.

use std::io::Write as _;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// Parse the `$EDITOR` spec into program + arguments
/// (whitespace-separated, e.g. `EDITOR="code -w"`).
pub fn editor_command() -> Result<Vec<String>> {
    split_editor(&std::env::var("EDITOR").unwrap_or_default())
}

fn split_editor(spec: &str) -> Result<Vec<String>> {
    let parts: Vec<String> = spec.split_whitespace().map(str::to_string).collect();
    if parts.is_empty() {
        return Err(Error::NoEditor);
    }
    Ok(parts)
}

/// Edit `initial` with an explicit editor command (program + args),
pub fn edit_with(initial: &str, editor: &[String]) -> Result<Option<String>> {
    let path = write_buffer_file(initial)?;

    let outcome = (|| -> Result<std::process::ExitStatus> {
        let (program, args) = editor.split_first().expect("editor command is non-empty");
        // Inherits stdin/stdout/stderr: the editor owns the terminal while
        // the TUI is suspended.
        let status = Command::new(program).args(args).arg(&path).status()?;
        Ok(status)
    })();

    // Always clean up the temp file, then surface editor failures.
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    if !outcome?.success() {
        return Err(Error::EditorFailed("non-zero exit".into()));
    }

    let text = strip_trailing_newline(&content);
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text.to_string()))
}

/// Editors append a trailing newline; remove one so re-editing is stable.
fn strip_trailing_newline(text: &str) -> &str {
    let stripped = text.strip_suffix('\n').unwrap_or(text);
    stripped.strip_suffix('\r').unwrap_or(stripped)
}

/// How many distinct names to try before giving up on a free buffer file.
/// A collision is benign (a stale file, a recycled pid); only a
/// persistent one is an error worth surfacing.
const BUFFER_NAME_ATTEMPTS: u32 = 8;

/// Create a fresh, exclusively-owned buffer file seeded with `initial`
/// and return its path (finding 32).
///
/// The name is only as unique as pid + clock, so it is never assumed
/// free: `create_new` (`O_EXCL`) makes a collision a *detected* event
/// instead of a silent truncation of a file another session is editing.
/// A collision just moves on to the next candidate name; anything else
/// is an error. The file is created owner-only (0600) — a draft prompt
/// can hold anything the user was about to send.
fn write_buffer_file(initial: &str) -> Result<std::path::PathBuf> {
    for attempt in 0..BUFFER_NAME_ATTEMPTS {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "clanky-edit-{}-{}-{attempt}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        match create_exclusive(&path) {
            Ok(mut file) => {
                // A failed seed must not leave a half-written buffer
                // behind: remove it and report the write error.
                if let Err(err) = file.write_all(initial.as_bytes()) {
                    drop(file);
                    let _ = std::fs::remove_file(&path);
                    return Err(Error::EditorTempFile(err));
                }
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(Error::EditorTempFile(err)),
        }
    }
    Err(Error::EditorTempFile(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "every candidate buffer name was taken",
    )))
}

/// `O_CREAT | O_EXCL` at 0600: no truncation of an existing file, and no
/// window in which the buffer is readable by anyone else.
fn create_exclusive(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake editor script that rewrites the buffer file, so tests can
    /// exercise the full suspend/edit/resume round trip without a TUI.
    fn fake_editor(tag: &str, body: &str) -> Vec<String> {
        let dir = std::env::temp_dir().join(format!(
            "clanky-editor-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("fake-editor.sh");
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        vec![script.to_string_lossy().into_owned()]
    }

    #[test]
    fn editor_rewrites_the_buffer() {
        let editor = fake_editor("rewrite", "#!/bin/sh\nprintf 'new text\\n' > \"$1\"\n");
        assert_eq!(edit_with("old", &editor).unwrap(), Some("new text".into()));
    }

    #[test]
    fn trailing_newline_is_stripped_once() {
        let editor = fake_editor("newline", "#!/bin/sh\nprintf 'a\\n\\n\\n' > \"$1\"\n");
        assert_eq!(edit_with("", &editor).unwrap(), Some("a\n\n".into()));
    }

    #[test]
    fn empty_buffer_clears_the_input() {
        let editor = fake_editor("empty", "#!/bin/sh\nprintf '  \\n' > \"$1\"\n");
        assert_eq!(edit_with("was here", &editor).unwrap(), None);
    }

    #[test]
    fn editor_keeps_untouched_buffer() {
        let editor = fake_editor("untouched", "#!/bin/sh\nexit 0\n");
        assert_eq!(edit_with("kept", &editor).unwrap(), Some("kept".into()));
    }

    #[test]
    fn failing_editor_is_an_error() {
        let editor = fake_editor("fail", "#!/bin/sh\nexit 3\n");
        let err = edit_with("x", &editor).unwrap_err();
        assert!(err.to_string().contains("editor failed"), "{err}");
    }

    #[test]
    fn the_buffer_file_is_created_exclusively() {
        // Finding 32: the buffer name is only as unique as pid + clock, so
        // a plain `write` could silently truncate a file a concurrent
        // session already owns — clobbering the very buffer being edited.
        // The exclusive create turns a collision into a fresh name, and a
        // pre-existing file's contents are never touched.
        let occupied = std::env::temp_dir().join(format!(
            "clanky-edit-occupied-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::write(&occupied, "do not truncate me").unwrap();
        let err = create_exclusive(&occupied).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&occupied).unwrap(),
            "do not truncate me",
            "the existing file survives the failed create"
        );
        std::fs::remove_file(&occupied).ok();

        // The real path helper seeds the contents and reports its name.
        let path = write_buffer_file("hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("clanky-edit-"),
            "{path:?}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    #[cfg(unix)]
    fn the_buffer_file_is_owner_only() {
        // A draft prompt can hold anything the user was about to send, so
        // the buffer is not world-readable while the editor has it open.
        use std::os::unix::fs::PermissionsExt as _;
        let path = write_buffer_file("secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "group/other must have no bits: {mode:o}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_editor_command_is_an_error() {
        assert!(split_editor("").is_err());
        assert!(split_editor("   ").is_err());
        let parts = split_editor("code -w").unwrap();
        assert_eq!(parts, vec!["code".to_string(), "-w".to_string()]);
    }
}
