//! Tab completion of file paths (M7).
//!
//! The token at the caret (whitespace-delimited) is treated as a path:
//! relative paths resolve against the working directory, `~` expands to
//! the home directory, absolute paths are used as-is. Directories
//! complete with a trailing `/`, files with a trailing space. Multiple
//! candidates shorten to their common prefix first; `App` then cycles
//! through them on repeated Tab presses (see `app::CompletionState`).

use std::path::{Path, PathBuf};

/// Byte range `(start, end)` of the whitespace-delimited token ending at
/// `cursor`. The range is empty when the cursor sits at (or directly
/// after) whitespace, so completion then starts from the directory.
pub fn token_range(input: &str, cursor: usize) -> (usize, usize) {
    let start = input[..cursor]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    (start, cursor)
}

/// Path completions for `token`, as replacement text for the token.
/// Sorted; hidden entries (leading `.`) are only offered when the token
/// itself starts with a dot.
pub fn candidates(base: &Path, token: &str) -> Vec<String> {
    let (display_dir, fs_dir, prefix) = split_token(base, token);
    let Ok(entries) = std::fs::read_dir(&fs_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with(&prefix) {
            continue;
        }
        if name.starts_with('.') && !prefix.starts_with('.') {
            continue;
        }
        let terminator = if entry.path().is_dir() { "/" } else { " " };
        out.push(format!("{display_dir}{name}{terminator}"));
    }
    out.sort();
    out
}

/// Longest common prefix over the candidates, on char boundaries.
pub fn common_prefix(candidates: &[String]) -> String {
    let Some(first) = candidates.first() else {
        return String::new();
    };
    let mut prefix = String::new();
    for (i, c) in first.char_indices() {
        if candidates
            .iter()
            .all(|candidate| candidate[i..].starts_with(c))
        {
            prefix.push(c);
        } else {
            break;
        }
    }
    prefix
}

/// Resolve a token into `(display prefix, directory to list, filename
/// prefix)`. The display prefix is re-prepended to each entry name so the
/// replacement text stays as the user typed it (e.g. `~`, `src/`).
fn split_token(base: &Path, token: &str) -> (String, PathBuf, String) {
    // `~` alone lists the home directory; `~/…` expands to it.
    if token == "~"
        && let Some(home) = dirs::home_dir()
    {
        return ("~/".to_string(), home, String::new());
    }
    if let Some(rest) = token.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        let (dir_part, prefix) = split_path(rest);
        let fs_dir = if dir_part.is_empty() {
            home
        } else {
            home.join(&dir_part)
        };
        return (format!("~/{dir_part}"), fs_dir, prefix);
    }
    if token.starts_with('/') {
        let (dir_part, prefix) = split_path(token);
        let fs_dir = if dir_part.is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(&dir_part)
        };
        return (dir_part, fs_dir, prefix);
    }
    let (dir_part, prefix) = split_path(token);
    let fs_dir = if dir_part.is_empty() {
        base.to_path_buf()
    } else {
        base.join(&dir_part)
    };
    (dir_part, fs_dir, prefix)
}

/// Split `a/b/c` into (`a/b/`, `c`); `a/b/` into (`a/b/`, "").
fn split_path(path: &str) -> (String, String) {
    match path.rfind('/') {
        Some(i) => (path[..=i].to_string(), path[i + 1..].to_string()),
        None => (String::new(), path.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory with a fixed layout; each test gets its own so
    /// parallel tests never share state.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "clanky-completion-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join("beta")).unwrap();
        std::fs::write(dir.join("alpha.md"), "").unwrap();
        std::fs::write(dir.join("alpha.txt"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("src/deep/file.txt"), "").unwrap();
        dir
    }

    #[test]
    fn token_range_follows_whitespace_and_cursor() {
        assert_eq!(token_range("cat src/ma", 10), (4, 10));
        assert_eq!(token_range("", 0), (0, 0));
        // Cursor right after a space starts an empty token.
        assert_eq!(token_range("cat ", 4), (4, 4));
        // Cursor mid-token completes the token up to the cursor.
        assert_eq!(token_range("cat src/ma", 8), (4, 8));
    }

    #[test]
    fn completes_relative_files_and_dirs() {
        let dir = scratch("relative");
        let files = candidates(&dir, "");
        assert!(
            files == vec![".hidden ", "alpha.md ", "alpha.txt ", "beta/", "src/"]
                || files == vec!["alpha.md ", "alpha.txt ", "beta/", "src/"],
            "got {files:?}"
        );

        let alpha = candidates(&dir, "alp");
        assert_eq!(alpha, vec!["alpha.md ", "alpha.txt "]);

        let src = candidates(&dir, "src");
        assert_eq!(src, vec!["src/"]);

        let deep = candidates(&dir, "src/deep/f");
        assert_eq!(deep, vec!["src/deep/file.txt "]);
    }

    #[test]
    fn trailing_slash_lists_directory_contents() {
        let dir = scratch("slash");
        let entries = candidates(&dir, "src/");
        assert_eq!(entries, vec!["src/deep/", "src/main.rs "]);
    }

    #[test]
    fn dotfiles_only_complete_when_requested() {
        let dir = scratch("hidden");
        assert!(!candidates(&dir, "").iter().any(|c| c.contains(".hidden")));
        let dot = candidates(&dir, ".");
        assert!(dot.iter().any(|c| c.contains(".hidden")), "{dot:?}");
    }

    #[test]
    fn unknown_paths_yield_no_candidates() {
        let dir = scratch("unknown");
        assert!(candidates(&dir, "nope/zzz").is_empty());
        assert!(candidates(&dir, "zzz").is_empty());
    }

    #[test]
    fn absolute_paths_are_used_as_is() {
        let dir = scratch("absolute");
        let token = format!("{}/src/ma", dir.display());
        assert_eq!(
            candidates(&dir, &token),
            vec![format!("{}/src/main.rs ", dir.display())]
        );
    }

    #[test]
    fn tilde_expands_to_the_home_directory() {
        let Some(home) = dirs::home_dir() else {
            return; // no HOME in this environment; skip
        };
        let names: Vec<String> = std::fs::read_dir(&home)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let name = e.file_name().into_string().ok()?;
                        if !name.starts_with(".clanky") {
                            return None;
                        }
                        let terminator = if e.path().is_dir() { "/" } else { " " };
                        Some(format!("~/{name}{terminator}"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if names.is_empty() {
            return; // nothing stable to assert against
        }
        let got = candidates(Path::new("/"), "~/.clan");
        assert_eq!(got, names, "tilde should resolve against $HOME");
    }

    #[test]
    fn common_prefix_is_char_safe() {
        assert_eq!(
            common_prefix(&["alpha.md ".into(), "alpha.txt ".into()]),
            "alpha."
        );
        assert_eq!(common_prefix(&["beta/".into(), "src/".into()]), "");
        assert_eq!(common_prefix(&["src/".into()]), "src/");
        assert_eq!(common_prefix(&[]), "");
        // Common prefix must not split a multi-byte char.
        let é = "é".to_string();
        assert_eq!(common_prefix(&[format!("{é}x "), format!("{é}y ")]), "é");
    }

    #[test]
    fn split_path_splits_at_the_last_slash() {
        assert_eq!(split_path("a/b/c"), ("a/b/".into(), "c".into()));
        assert_eq!(split_path("src/"), ("src/".into(), "".into()));
        assert_eq!(split_path("name"), ("".into(), "name".into()));
        assert_eq!(split_path(""), ("".into(), "".into()));
    }
}
