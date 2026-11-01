//! Context assembly (M6): system prompt and agent instructions from both
//! scopes, plus skills.
//!
//! Scope and order (later messages take precedence in most backends):
//! 1. user scope:    `$HOME/.clanky/SYSTEM.md`, `$HOME/.clanky/AGENTS.md`
//! 2. project scope: `./.clanky/SYSTEM.md`, `./.clanky/AGENTS.md`
//! 3. project root:  `./AGENTS.md` (the ecosystem convention, kept from M1)
//! 4. skills:        `skills/` under each scope, user first (see
//!    [`crate::skills`]), listed as one part: a short hint to read a
//!    matching skill's file, then one `- name: description (filepath)`
//!    line per skill. Bodies are never included; the agent reads the
//!    file when a task matches.
//!
//! Every file part is sent with a one-line provenance heading
//! (`# Context — <path>`) so the model can tell where each part ends and
//! which scope (user or project) authored it; project scope is the
//! override when instructions conflict. The skills part keeps its own
//! `## Skills` heading.
//!
//! Everything is best-effort: missing, empty, or unreadable files are
//! skipped silently.
//!
//! `override = ["system", "skills"]` in settings.toml (see
//! [`crate::config::Override`]) drops the user scope from the parts it
//! names: `system` removes the user `SYSTEM.md`, `skills` the user
//! `skills/` directory. The user `AGENTS.md` is not covered by either —
//! it is agent instructions, not the system prompt — so the user scope is
//! still read for it, and only the parts for overridden resources are
//! left out. With both overridden the context is the project scope's
//! alone (plus the project-root `AGENTS.md`).

use std::path::Path;

use clanky_protocol::ChatMessage;

use crate::config;
use crate::skills::{self, Skill};

/// Assemble system messages from the real locations (cwd as project root),
/// applying the `overrides` from the merged settings.
pub fn system_messages(overrides: &[config::Override]) -> Vec<ChatMessage> {
    system_messages_in(
        config::user_dir().as_deref(),
        &config::project_dir(),
        Path::new("."),
        overrides,
    )
}

/// Assemble the system prompt with one labelled part per source, for
/// `/system` display: `(source label, content)` pairs in send order.
pub fn system_parts(overrides: &[config::Override]) -> Vec<(String, String)> {
    system_parts_in(
        config::user_dir().as_deref(),
        &config::project_dir(),
        Path::new("."),
        overrides,
    )
}

/// Pure form of [`system_parts`] for testing: `user` and `project` are the
/// scope directories (user `None` when `$HOME` is unknown), `project_root`
/// carries the ecosystem `AGENTS.md`, and `overrides` are the merged
/// settings' overridden resources.
pub fn system_parts_in(
    user: Option<&Path>,
    project: &Path,
    project_root: &Path,
    overrides: &[config::Override],
) -> Vec<(String, String)> {
    let mut parts = Vec::new();
    // The user scope's `SYSTEM.md` is the system prompt, so the `system`
    // override removes it; its `AGENTS.md` is agent instructions and is
    // always layered.
    if let Some(user) = user {
        if !overrides.contains(&config::Override::System) {
            push_part(&mut parts, user, config::SYSTEM_FILE);
        }
        push_part(&mut parts, user, config::AGENTS_FILE);
    }
    push_part(&mut parts, project, config::SYSTEM_FILE);
    push_part(&mut parts, project, config::AGENTS_FILE);
    // Ecosystem convention from M1: a project-root AGENTS.md still counts.
    push_part(&mut parts, project_root, config::AGENTS_FILE);

    // Skills layer user-then-project unless the user directory is
    // overridden, in which case only the project scope is scanned.
    let mut scopes = Vec::new();
    if let Some(user) = user.filter(|_| !overrides.contains(&config::Override::Skills)) {
        scopes.push(user.to_path_buf());
    }
    scopes.push(project.to_path_buf());
    let found = skills::skills_in(&scopes);
    if !found.is_empty() {
        parts.push(("skills".to_string(), skill_list(&found)));
    }
    parts
}

/// Pure form of [`system_messages`] for testing; see [`system_parts_in`].
pub fn system_messages_in(
    user: Option<&Path>,
    project: &Path,
    project_root: &Path,
    overrides: &[config::Override],
) -> Vec<ChatMessage> {
    system_parts_in(user, project, project_root, overrides)
        .into_iter()
        .map(|(_, content)| ChatMessage::system(content))
        .collect()
}

/// Hint shown right before the skill list, so the agent knows when to
/// actually read a skill file.
const SKILL_HINT: &str = "If a skill matches the task you're working on (or if \
asked directly to use the skill), read that skill's file.";

/// One skill listing part: the hint, then `name`, `description`, and
/// `filepath` per skill — never the skill body, which the agent reads
/// on demand from `path`.
fn skill_list(skills: &[Skill]) -> String {
    let mut out = format!("## Skills\n\n{SKILL_HINT}\n\n");
    for skill in skills {
        let description = if skill.description.is_empty() {
            String::new()
        } else {
            format!(": {}", skill.description)
        };
        out.push_str(&format!(
            "- {}{} ({})\n",
            skill.name,
            description,
            skill.path.display()
        ));
    }
    out
}

/// Append a file as a labelled part; missing or empty files are skipped
/// silently (context is best-effort). The label is the file path, and the
/// content is prefixed with a provenance heading (`# Context — <path>`)
/// so the model can tell parts apart and which scope authored each.
fn push_part(parts: &mut Vec<(String, String)>, dir: &Path, name: &str) {
    let Ok(text) = std::fs::read_to_string(dir.join(name)) else {
        return;
    };
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        let path = dir.join(name);
        parts.push((
            path.display().to_string(),
            format!("# Context — {}\n\n{}", path.display(), trimmed),
        ));
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

    /// A fake scope directory (a `scope` inside `base`, which stands in
    /// for `$HOME` or the cwd).
    fn scope_dir(base: &Path, kind: &str) -> PathBuf {
        let dir = base.join(kind).join(config::project_dir());
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The usual pair: (user scope, project scope) directories.
    fn two_scopes(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let project = temp_dir(&format!("{name}-project"));
        let user = temp_dir(&format!("{name}-user"));
        let user_scope = scope_dir(&user, "u");
        let project_scope = scope_dir(&project, "p");
        (project, user, user_scope, project_scope)
    }

    #[test]
    fn system_parts_label_every_source() {
        let (project, user, user_scope, project_scope) = two_scopes("parts");
        fs::write(user_scope.join(config::SYSTEM_FILE), "user system").unwrap();
        fs::write(project_scope.join(config::AGENTS_FILE), "Be terse.\n").unwrap();
        let skills = project_scope.join(config::SKILLS_DIR);
        fs::create_dir_all(&skills).unwrap();
        fs::write(
            skills.join("style.md"),
            "---\ndescription: project style\n---\n\nignored body",
        )
        .unwrap();

        let parts = system_parts_in(Some(&user_scope), &project_scope, Path::new("."), &[]);
        let labels: Vec<&str> = parts.iter().map(|(label, _)| label.as_str()).collect();
        assert!(
            labels[0].contains(".clanky") && labels[0].ends_with("SYSTEM.md"),
            "{labels:?}"
        );
        assert!(labels[1].ends_with("AGENTS.md"), "{labels:?}");
        assert_eq!(labels[2], "skills");
        assert_eq!(
            parts[1].1,
            format!("# Context — {}\n\nBe terse.", labels[1])
        );
        assert_eq!(
            parts[2].1,
            format!(
                "## Skills\n\n{SKILL_HINT}\n\n- style: project style ({})\n",
                skills.join("style.md").display()
            )
        );

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn missing_files_yield_no_context() {
        let (project, user, user_scope, project_scope) = two_scopes("empty");
        assert!(
            system_messages_in(Some(&user_scope), &project_scope, Path::new("."), &[]).is_empty()
        );
        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn project_agents_md_becomes_system_message() {
        let (project, user, user_scope, project_scope) = two_scopes("with-agents");
        fs::write(project_scope.join(config::AGENTS_FILE), "Be terse.\n").unwrap();
        let msgs = system_messages_in(Some(&user_scope), &project_scope, Path::new("."), &[]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].content(),
            format!(
                "# Context — {}/AGENTS.md\n\nBe terse.",
                project_scope.display()
            )
        );

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn project_overrides_user_by_order() {
        let (project, user, user_scope, project_scope) = two_scopes("order");
        fs::write(user_scope.join(config::SYSTEM_FILE), "user system").unwrap();
        fs::write(user_scope.join(config::AGENTS_FILE), "user agents").unwrap();
        fs::write(project_scope.join(config::SYSTEM_FILE), "project system").unwrap();
        fs::write(project_scope.join(config::AGENTS_FILE), "project agents").unwrap();

        let msgs = system_messages_in(Some(&user_scope), &project_scope, Path::new("."), &[]);
        let contents: Vec<String> = msgs
            .iter()
            .zip(["SYSTEM.md", "AGENTS.md", "SYSTEM.md", "AGENTS.md"])
            .zip([&user_scope, &user_scope, &project_scope, &project_scope])
            .map(|((_, name), scope)| format!("# Context — {}/{}\n\n", scope.display(), name))
            .zip(msgs.iter().map(|m| m.content()))
            .map(|(head, content)| {
                assert!(content.starts_with(&head), "{head:?} vs {content:?}");
                content[head.len()..].trim().to_string()
            })
            .collect();
        // The heading carries the scope; the body is unchanged and the
        // layering order (user first, project last) is untouched.
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
        let project_scope = scope_dir(&project, "p");
        fs::write(project_scope.join(config::AGENTS_FILE), "   \n\t\n").unwrap();
        assert!(system_messages_in(None, &project_scope, Path::new("."), &[]).is_empty());
        fs::remove_dir_all(project).ok();
    }

    #[test]
    fn project_root_agents_md_still_counts() {
        let project = temp_dir("root-agents");
        let project_scope = scope_dir(&project, "p");
        fs::write(project.join(config::AGENTS_FILE), "root convention").unwrap();
        let msgs = system_messages_in(None, &project_scope, &project, &[]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].content(),
            format!(
                "# Context — {}/AGENTS.md\n\nroot convention",
                project.display()
            )
        );
        fs::remove_dir_all(project).ok();
    }

    #[test]
    fn skills_load_from_both_scopes_with_project_shadowing() {
        let (project, user, user_scope, project_scope) = two_scopes("skills");
        for scope in [&user_scope, &project_scope] {
            let skills = scope.join(config::SKILLS_DIR);
            fs::create_dir_all(&skills).unwrap();
            fs::write(
                skills.join("style.md"),
                "---\ndescription: project style\n---\n\nuser style",
            )
            .unwrap();
        }
        let user_skills = user_scope.join(config::SKILLS_DIR);
        fs::create_dir_all(&user_skills).unwrap();
        fs::write(user_skills.join("shared-user-only.md"), "only in user").unwrap();
        let project_skills = project_scope.join(config::SKILLS_DIR);
        fs::create_dir_all(project_skills.join("dirskill")).unwrap();
        fs::write(
            project_skills.join("dirskill/SKILL.md"),
            "---\ndescription: directory skill description\n---\n\ndirectory skill",
        )
        .unwrap();

        let msgs = system_messages_in(Some(&user_scope), &project_scope, Path::new("."), &[]);
        let contents: Vec<&str> = msgs.iter().map(|m| m.content()).collect();
        assert_eq!(
            contents,
            [format!(
                "## Skills\n\n{SKILL_HINT}\n\n- dirskill: {} ({})\n- shared-user-only ({})\n- style: project style ({})\n",
                "directory skill description",
                project_skills.join("dirskill/SKILL.md").display(),
                user_skills.join("shared-user-only.md").display(),
                project_skills.join("style.md").display(),
            )],
            "one listing part; project scope shadows user; sorted by name"
        );

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    /// A user scope with all three resources, and a project scope with
    /// only `AGENTS.md` — the setup every override test starts from.
    fn layered_scopes(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let (project, user, user_scope, project_scope) = two_scopes(name);
        fs::write(user_scope.join(config::SYSTEM_FILE), "user system").unwrap();
        fs::write(user_scope.join(config::AGENTS_FILE), "user agents").unwrap();
        let user_skills = user_scope.join(config::SKILLS_DIR);
        fs::create_dir_all(&user_skills).unwrap();
        fs::write(user_skills.join("user-skill.md"), "user skill").unwrap();
        fs::write(project_scope.join(config::AGENTS_FILE), "project agents").unwrap();
        (project, user, user_scope, project_scope)
    }

    /// The bodies of the parts, in send order.
    fn bodies(parts: &[(String, String)]) -> Vec<String> {
        parts.iter().map(|(_, body)| body.clone()).collect()
    }

    #[test]
    fn system_override_drops_only_the_user_system_md() {
        let (project, user, user_scope, project_scope) = layered_scopes("override-system");
        let parts = system_parts_in(
            Some(&user_scope),
            &project_scope,
            Path::new("."),
            &[config::Override::System],
        );
        let bodies = bodies(&parts);
        assert!(
            !bodies.iter().any(|body| body.contains("user system")),
            "the user SYSTEM.md is gone: {bodies:?}"
        );
        // Everything else the user scope contributes stays: `system` is
        // the system prompt, not the whole user scope.
        assert!(
            bodies.iter().any(|body| body.contains("user agents")),
            "the user AGENTS.md is still layered: {bodies:?}"
        );
        assert!(
            bodies.iter().any(|body| body.contains("user-skill")),
            "user skills are still layered: {bodies:?}"
        );
        assert!(
            bodies.iter().any(|body| body.contains("project agents")),
            "{bodies:?}"
        );

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn skills_override_drops_the_whole_user_skills_directory() {
        let (project, user, user_scope, project_scope) = layered_scopes("override-skills");
        let parts = system_parts_in(
            Some(&user_scope),
            &project_scope,
            Path::new("."),
            &[config::Override::Skills],
        );
        let bodies = bodies(&parts);
        assert!(
            !bodies.iter().any(|body| body.contains("user-skill")),
            "no user skill is listed: {bodies:?}"
        );
        assert!(
            !bodies.iter().any(|body| body.starts_with("## Skills")),
            "with no project skills either, there is no skills part at all: {bodies:?}"
        );
        assert!(
            bodies.iter().any(|body| body.contains("user system")),
            "`skills` does not touch SYSTEM.md: {bodies:?}"
        );

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }

    #[test]
    fn both_overrides_leave_only_the_project_context() {
        let (project, user, user_scope, project_scope) = layered_scopes("override-both");
        fs::write(project_scope.join(config::SYSTEM_FILE), "project system").unwrap();
        let skills = project_scope.join(config::SKILLS_DIR);
        fs::create_dir_all(&skills).unwrap();
        fs::write(skills.join("project-skill.md"), "project skill").unwrap();

        let parts = system_parts_in(
            Some(&user_scope),
            &project_scope,
            Path::new("."),
            &[config::Override::System, config::Override::Skills],
        );
        let labels: Vec<&str> = parts.iter().map(|(label, _)| label.as_str()).collect();
        // The one user-scope part left is `AGENTS.md`, which neither
        // override covers.
        let user_dir = user_scope.display().to_string();
        let from_user: Vec<&&str> = labels.iter().filter(|l| l.contains(&user_dir)).collect();
        assert_eq!(from_user.len(), 1, "only AGENTS.md: {labels:?}");
        assert!(from_user[0].ends_with("AGENTS.md"), "{labels:?}");
        let bodies = bodies(&parts);
        assert!(bodies.iter().any(|body| body.contains("project system")));
        assert!(bodies.iter().any(|body| body.contains("project agents")));
        assert!(bodies.iter().any(|body| body.contains("project-skill")));
        assert!(
            bodies.iter().any(|body| body.contains("user agents")),
            "AGENTS.md is not covered by an override and stays layered: {bodies:?}"
        );

        fs::remove_dir_all(project).ok();
        fs::remove_dir_all(user).ok();
    }
}
