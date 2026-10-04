//! Context assembly stub (M1): read `SYSTEM.md` / `AGENTS.md` when present.
//!
//! Scope and order (later messages take precedence in most backends):
//! 1. user scope:   `$HOME/.clanky/SYSTEM.md`, `$HOME/.clanky/AGENTS.md`
//! 2. project root: `./SYSTEM.md`, `./AGENTS.md`
//!
//! M6 completes the layout (skills/, prompts/, `./.clanky/` scope); this
//! stub exists so `-p` turns already pick up the common convention of a
//! project-root `AGENTS.md`.

use std::path::Path;

use clanky_protocol::ChatMessage;

/// Assemble system messages from the real locations (cwd as project root).
pub fn system_messages() -> Vec<ChatMessage> {
    let user_dir = dirs::home_dir().map(|home| home.join(".clanky"));
    system_messages_in(Path::new("."), user_dir.as_deref())
}

/// Pure form of [`system_messages`] for testing: scan explicit locations.
pub fn system_messages_in(project_root: &Path, user_dir: Option<&Path>) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    if let Some(user) = user_dir {
        push_file(&mut messages, user, "SYSTEM.md");
        push_file(&mut messages, user, "AGENTS.md");
    }
    push_file(&mut messages, project_root, "SYSTEM.md");
    push_file(&mut messages, project_root, "AGENTS.md");
    messages
}

/// Append a file as a system message; missing or empty files are skipped
/// silently (context is best-effort).
fn push_file(messages: &mut Vec<ChatMessage>, dir: &Path, name: &str) {
    let Ok(text) = std::fs::read_to_string(dir.join(name)) else {
        return;
    };
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        messages.push(ChatMessage::system(trimmed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clanky-context-{name}"));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_files_yield_no_context() {
        let project = temp_dir("empty-project");
        let user = temp_dir("empty-user");
        assert!(system_messages_in(&project, Some(&user)).is_empty());
        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn project_agents_md_becomes_system_message() {
        let project = temp_dir("with-agents");
        let user = temp_dir("with-agents-user");
        fs::write(project.join("AGENTS.md"), "Be terse.\n").unwrap();
        let msgs = system_messages_in(&project, Some(&user));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content(), "Be terse.");

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn project_overrides_user_by_order() {
        let project = temp_dir("order-project");
        let user = temp_dir("order-user");
        fs::write(user.join("SYSTEM.md"), "user system").unwrap();
        fs::write(user.join("AGENTS.md"), "user agents").unwrap();
        fs::write(project.join("SYSTEM.md"), "project system").unwrap();
        fs::write(project.join("AGENTS.md"), "project agents").unwrap();

        let msgs = system_messages_in(&project, Some(&user));
        let contents: Vec<&str> = msgs.iter().map(|m| m.content()).collect();
        assert_eq!(
            contents,
            [
                "user system",
                "user agents",
                "project system",
                "project agents"
            ]
        );

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn whitespace_only_files_are_skipped() {
        let project = temp_dir("blank-file");
        fs::write(project.join("AGENTS.md"), "   \n\t\n").unwrap();
        assert!(system_messages_in(&project, None).is_empty());
        fs::remove_dir_all(project).ok();
    }
}
