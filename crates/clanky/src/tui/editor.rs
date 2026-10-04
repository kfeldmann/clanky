//! `$EDITOR` integration (M7): edit the prompt buffer in an external
//! editor. The TUI suspends (leaves the alternate screen, disables raw
//! mode), the editor inherits the terminal, and on exit the TUI resumes
//! with the edited text. Any keybindings here are only reachable when no
//! picker is open.

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
    let mut path = std::env::temp_dir();
    path.push(format!(
        "clanky-edit-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::write(&path, initial)?;

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
    fn missing_editor_command_is_an_error() {
        assert!(split_editor("").is_err());
        assert!(split_editor("   ").is_err());
        let parts = split_editor("code -w").unwrap();
        assert_eq!(parts, vec!["code".to_string(), "-w".to_string()]);
    }
}
