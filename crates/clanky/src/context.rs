//! Context assembly (M6): system prompt and agent instructions from both
//! scopes, plus skills.
//!
//! Scope and order (later messages take precedence in most backends):
//! 1. user scope:    `$HOME/.clanky/SYSTEM.md`, `$HOME/.clanky/AGENTS.md`
//! 2. project scope: `./.clanky/SYSTEM.md`, `./.clanky/AGENTS.md`
//! 3. project root:  `./AGENTS.md` (the ecosystem convention, kept from M1)
//! 4. skills:        `skills/` under each scope, user first (see
//!    [`crate::skills`]), injected under a `# Skill: <name>` heading
//!
//! Everything is best-effort: missing, empty, or unreadable files are
//! skipped silently.

use std::path::{Path, PathBuf};

use clanky_protocol::ChatMessage;

use crate::config;
use crate::skills::{self, Skill};

/// Assemble system messages from the real locations (cwd as project root).
pub fn system_messages() -> Vec<ChatMessage> {
    system_messages_in(&config::scopes(), Path::new("."))
}

/// Assemble the system prompt with one labelled part per source, for
/// `/system` display: `(source label, content)` pairs in send order.
pub fn system_parts() -> Vec<(String, String)> {
    system_parts_in(&config::scopes(), Path::new("."))
}

/// Pure form of [`system_parts`] for testing.
pub fn system_parts_in(scopes: &[PathBuf], project_root: &Path) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    for scope in scopes {
        push_part(&mut parts, scope, config::SYSTEM_FILE);
        push_part(&mut parts, scope, config::AGENTS_FILE);
    }
    // Ecosystem convention from M1: a project-root AGENTS.md still counts.
    push_part(&mut parts, project_root, config::AGENTS_FILE);

    for skill in skills::skills_in(scopes) {
        parts.push((format!("skill {}", skill.name), wrap_skill(&skill)));
    }
    parts
}

/// Pure form of [`system_messages`] for testing: scan explicit scope
/// directories (layering order: user first, project last) and a project
/// root for the `AGENTS.md` convention file.
pub fn system_messages_in(scopes: &[PathBuf], project_root: &Path) -> Vec<ChatMessage> {
    system_parts_in(scopes, project_root)
        .into_iter()
        .map(|(_, content)| ChatMessage::system(content))
        .collect()
}

/// Wrap a skill body in a system message with a heading.
fn wrap_skill(skill: &Skill) -> String {
    format!("# Skill: {}\n\n{}", skill.name, skill.content)
}

/// Append a file as a labelled part; missing or empty files are skipped
/// silently (context is best-effort). The label is the file path.
fn push_part(parts: &mut Vec<(String, String)>, dir: &Path, name: &str) {
    let Ok(text) = std::fs::read_to_string(dir.join(name)) else {
        return;
    };
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        parts.push((dir.join(name).display().to_string(), trimmed.to_string()));
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

    fn scope_dir(base: &Path, kind: &str) -> PathBuf {
        let dir = base.join(kind).join(config::project_dir());
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn system_parts_label_every_source() {
        let project = temp_dir("parts-project");
        let user = temp_dir("parts-user");
        let scopes = vec![scope_dir(&user, "u"), scope_dir(&project, "p")];
        fs::write(scopes[0].join(config::SYSTEM_FILE), "user system").unwrap();
        fs::write(scopes[1].join(config::AGENTS_FILE), "Be terse.\n").unwrap();
        let skills = scopes[1].join(config::SKILLS_DIR);
        fs::create_dir_all(&skills).unwrap();
        fs::write(skills.join("style.md"), "project style").unwrap();

        let parts = system_parts_in(&scopes, Path::new("."));
        let labels: Vec<&str> = parts.iter().map(|(label, _)| label.as_str()).collect();
        assert!(
            labels[0].contains(".clanky") && labels[0].ends_with("SYSTEM.md"),
            "{labels:?}"
        );
        assert!(labels[1].ends_with("AGENTS.md"), "{labels:?}");
        assert_eq!(labels[2], "skill style");
        assert_eq!(parts[1].1, "Be terse.");
        assert_eq!(parts[2].1, "# Skill: style\n\nproject style");

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn missing_files_yield_no_context() {
        let project = temp_dir("empty-project");
        let user = temp_dir("empty-user");
        let scopes = vec![scope_dir(&user, "u"), scope_dir(&project, "p")];
        assert!(system_messages_in(&scopes, Path::new(".")).is_empty());
        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn project_agents_md_becomes_system_message() {
        let project = temp_dir("with-agents");
        let user = temp_dir("with-agents-user");
        let scopes = vec![scope_dir(&user, "u"), scope_dir(&project, "p")];
        fs::write(scopes[1].join(config::AGENTS_FILE), "Be terse.\n").unwrap();
        let msgs = system_messages_in(&scopes, Path::new("."));
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content(), "Be terse.");

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn project_overrides_user_by_order() {
        let project = temp_dir("order-project");
        let user = temp_dir("order-user");
        let scopes = vec![scope_dir(&user, "u"), scope_dir(&project, "p")];
        fs::write(scopes[0].join(config::SYSTEM_FILE), "user system").unwrap();
        fs::write(scopes[0].join(config::AGENTS_FILE), "user agents").unwrap();
        fs::write(scopes[1].join(config::SYSTEM_FILE), "project system").unwrap();
        fs::write(scopes[1].join(config::AGENTS_FILE), "project agents").unwrap();

        let msgs = system_messages_in(&scopes, Path::new("."));
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
        let scopes = vec![scope_dir(&project, "p")];
        fs::write(scopes[0].join(config::AGENTS_FILE), "   \n\t\n").unwrap();
        assert!(system_messages_in(&scopes, Path::new(".")).is_empty());
        fs::remove_dir_all(project).ok();
    }

    #[test]
    fn project_root_agents_md_still_counts() {
        let project = temp_dir("root-agents");
        let scopes = vec![scope_dir(&project, "p")];
        fs::write(project.join(config::AGENTS_FILE), "root convention").unwrap();
        let msgs = system_messages_in(&scopes, &project);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content(), "root convention");
        fs::remove_dir_all(project).ok();
    }

    #[test]
    fn skills_load_from_both_scopes_with_project_shadowing() {
        let project = temp_dir("skills-project");
        let user = temp_dir("skills-user");
        let scopes = vec![scope_dir(&user, "u"), scope_dir(&project, "p")];
        for scope in &scopes {
            let skills = scope.join(config::SKILLS_DIR);
            fs::create_dir_all(&skills).unwrap();
            fs::write(skills.join("style.md"), "user style").unwrap();
        }
        // Project scope wins the shadowing: overwrite the project copy.
        fs::write(
            scopes[1].join(config::SKILLS_DIR).join("style.md"),
            "project style",
        )
        .unwrap();
        let user_skills = scopes[0].join(config::SKILLS_DIR);
        fs::create_dir_all(&user_skills).unwrap();
        fs::write(user_skills.join("shared-user-only.md"), "only in user").unwrap();
        let project_skills = scopes[1].join(config::SKILLS_DIR);
        fs::create_dir_all(project_skills.join("dirskill")).unwrap();
        fs::write(project_skills.join("dirskill/SKILL.md"), "directory skill").unwrap();

        let msgs = system_messages_in(&scopes, Path::new("."));
        let contents: Vec<&str> = msgs.iter().map(|m| m.content()).collect();
        assert_eq!(
            contents,
            [
                "# Skill: dirskill\n\ndirectory skill",
                "# Skill: shared-user-only\n\nonly in user",
                "# Skill: style\n\nproject style",
            ],
            "project scope shadows user; sorted by name"
        );

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }
}
