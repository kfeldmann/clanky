//! Skills (M6): reference material loaded into the conversation context.
//!
//! A skill is discovered in a scope's `skills/` directory, either as a
//! plain markdown file (`skills/<name>.md`) or as a directory with a
//! `SKILL.md` inside (`skills/<name>/SKILL.md`). The name is the file stem
//! or directory name; the trimmed body is the content. Skills are injected
//! as system messages (see [`crate::context`]) under a `# Skill: <name>`
//! heading.
//!
//! Two scopes are read, user first, then project; on a name clash the
//! project scope shadows the user scope (same precedence as settings:
//! user < project). Results are sorted by name so context assembly is
//! deterministic.

use std::collections::BTreeMap;
use std::path::Path;

use crate::config;

/// One skill: `name` is the stem of `skills/<name>.md` (or the directory
/// name of `skills/<name>/SKILL.md`), `content` the trimmed body.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub content: String,
}

/// Discover skills from the real locations (cwd as project root).
pub fn skills() -> Vec<Skill> {
    skills_in(&config::scopes())
}

/// Pure form of [`skills`] for testing: read the `skills/` directory under
/// each given scope, project scope shadowing user scope. The result is
/// sorted by name.
pub fn skills_in(scopes: &[std::path::PathBuf]) -> Vec<Skill> {
    let mut by_name = BTreeMap::new();
    for scope in scopes {
        for skill in read_scope(scope) {
            by_name.insert(skill.name.clone(), skill);
        }
    }
    by_name.into_values().collect()
}

/// Read all usable skills from one scope's `skills/` directory. Missing
/// directories, non-UTF-8 files, and empty files are skipped silently
/// (skills are best-effort, like the other context files).
fn read_scope(scope: &Path) -> Vec<Skill> {
    let dir = scope.join(config::SKILLS_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // skills/<name>/SKILL.md — pi-style directory skill.
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(skill) = read_file(&path.join("SKILL.md"), name) {
                skills.push(skill);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            // skills/<name>.md
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(skill) = read_file(&path, name) {
                skills.push(skill);
            }
        }
    }
    skills
}

/// Read one skill file and turn it into a [`Skill`] if usable; empty
/// bodies are skipped.
fn read_file(path: &Path, name: &str) -> Option<Skill> {
    let content = std::fs::read_to_string(path).ok()?.trim().to_string();
    if content.is_empty() {
        return None;
    }
    Some(Skill {
        name: name.to_string(),
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clanky-skills-{name}"));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_directories_yield_no_skills() {
        let project = temp_dir("empty");
        assert!(skills_in(std::slice::from_ref(&project)).is_empty());
        fs::remove_dir_all(project).ok();
    }

    #[test]
    fn md_files_and_skill_dirs_become_skills() {
        let project = temp_dir("files");
        let skills = project.join(config::SKILLS_DIR);
        fs::create_dir_all(skills.join("rust-style")).unwrap();
        fs::write(skills.join("rust-style/SKILL.md"), "Use rustfmt.\n").unwrap();
        fs::write(skills.join("review.md"), "Review carefully.\n").unwrap();
        fs::write(skills.join("empty.md"), "   \n").unwrap();
        fs::write(skills.join("notes.txt"), "ignored").unwrap();
        fs::create_dir_all(skills.join("no-skill-file")).unwrap();

        let mut found = skills_in(std::slice::from_ref(&project));
        assert_eq!(
            found.len(),
            2,
            "empty file, non-md file and SKILL-less dir skipped"
        );
        found.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(found[0].name, "review");
        assert_eq!(found[0].content, "Review carefully.");
        assert_eq!(found[1].name, "rust-style");
        assert_eq!(found[1].content, "Use rustfmt.");
        fs::remove_dir_all(project).ok();
    }

    #[test]
    fn project_shadows_user_and_results_are_sorted() {
        let user = temp_dir("user");
        let project = temp_dir("project");
        for (dir, body) in [(&user, "user style"), (&project, "project style")] {
            fs::create_dir_all(dir.join(config::SKILLS_DIR)).unwrap();
            fs::write(dir.join(config::SKILLS_DIR).join("style.md"), body).unwrap();
        }
        fs::create_dir_all(user.join(config::SKILLS_DIR)).unwrap();
        fs::write(
            user.join(config::SKILLS_DIR).join("aaa.md"),
            "first alphabetically",
        )
        .unwrap();

        let skills = skills_in(&[user.clone(), project.clone()]);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["aaa", "style"], "sorted by name");
        let style = skills.iter().find(|s| s.name == "style").unwrap();
        assert_eq!(style.content, "project style");
        fs::remove_dir_all(user).ok();
        fs::remove_dir_all(project).ok();
    }

    #[test]
    fn empty_body_is_skipped() {
        let project = temp_dir("blank");
        fs::create_dir_all(project.join(config::SKILLS_DIR)).unwrap();
        fs::write(
            project.join(config::SKILLS_DIR).join("blank.md"),
            "\n\n  \n",
        )
        .unwrap();
        assert!(skills_in(&[project]).is_empty());
    }

    #[test]
    fn read_file_returns_none_for_missing_file() {
        let dir = temp_dir("missing");
        assert!(read_file(&dir.join("nope.md"), "nope").is_none());
        fs::remove_dir_all(dir).ok();
    }
}
