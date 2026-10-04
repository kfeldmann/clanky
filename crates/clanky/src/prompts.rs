//! Prompt templates (M5): markdown files invoked as slash commands.
//!
//! A template is a `.md` file in a scope's `prompts/` directory; the file
//! stem is the command name (`prompts/review.md` → `/review`) and the
//! trimmed file body is the prompt text. Two scopes are read, user first,
//! then project; on a name clash the project scope shadows the user scope
//! (same precedence as settings: user < project). Project scope wins the
//! final map insertion.
//!
//! Scopes and directory names come from [`crate::config`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config;

/// One prompt template: `name` is the file stem (invoked as `/name`),
/// `content` the trimmed file body.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub name: String,
    pub content: String,
}

/// Discover templates from the real locations (cwd as project root).
pub fn templates() -> Vec<Template> {
    templates_in(&config::scopes())
}

/// Pure form of [`templates`] for testing: read `prompts/*.md` under each
/// given scope directory, project scope shadowing user scope. The result
/// is sorted by name so pickers are stable.
pub fn templates_in(scopes: &[PathBuf]) -> Vec<Template> {
    let mut by_name = BTreeMap::new();
    for dir in scopes {
        for template in read_scope(dir) {
            by_name.insert(template.name.clone(), template);
        }
    }
    by_name.into_values().collect()
}

/// Read all usable templates from one scope's `prompts/` directory.
/// Missing directories, non-UTF-8 files, and empty files are skipped
/// silently (templates are best-effort, like context files).
fn read_scope(scope: &Path) -> Vec<Template> {
    let prompts = scope.join(config::PROMPTS_DIR);
    let Ok(entries) = std::fs::read_dir(&prompts) else {
        return Vec::new();
    };
    let mut templates = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let content = text.trim().to_string();
        if content.is_empty() {
            continue;
        }
        templates.push(Template {
            name: stem.to_string(),
            content,
        });
    }
    templates
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clanky-prompts-{name}"));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake scope directory containing a `prompts/` subdir.
    fn scope_dir(base: &Path) -> PathBuf {
        let dir = base.join(config::project_dir());
        fs::create_dir_all(dir.join(config::PROMPTS_DIR)).unwrap();
        dir
    }

    #[test]
    fn missing_directories_yield_no_templates() {
        let project = temp_dir("empty");
        assert!(templates_in(std::slice::from_ref(&project)).is_empty());
        fs::remove_dir_all(project).ok();
    }

    #[test]
    fn md_files_become_templates() {
        let project = scope_dir(&temp_dir("files"));
        fs::write(
            project.join(config::PROMPTS_DIR).join("review.md"),
            "Review this:\n{code}\n",
        )
        .unwrap();
        fs::write(project.join(config::PROMPTS_DIR).join("empty.md"), "   \n").unwrap();
        fs::write(
            project.join(config::PROMPTS_DIR).join("notes.txt"),
            "ignored",
        )
        .unwrap();

        let templates = templates_in(std::slice::from_ref(&project));
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "review");
        assert_eq!(templates[0].content, "Review this:\n{code}");
        fs::remove_dir_all(project).ok();
    }

    #[test]
    fn project_shadows_user_and_results_are_sorted() {
        let user = scope_dir(&temp_dir("user"));
        let project = scope_dir(&temp_dir("project"));
        for (dir, body) in [(&user, "user review"), (&project, "project review")] {
            fs::write(dir.join(config::PROMPTS_DIR).join("review.md"), body).unwrap();
        }
        fs::write(
            user.join(config::PROMPTS_DIR).join("aaa.md"),
            "first alphabetically",
        )
        .unwrap();

        let templates = templates_in(&[user.clone(), project.clone()]);
        let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["aaa", "review"], "sorted by name");
        let review = templates.iter().find(|t| t.name == "review").unwrap();
        assert_eq!(review.content, "project review");
        fs::remove_dir_all(user).ok();
        fs::remove_dir_all(project).ok();
    }
}
