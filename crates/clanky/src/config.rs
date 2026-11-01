//! The `.clanky` directory layout and scope layering (M6).
//!
//! Configuration and context live in two scopes:
//!
//! - user scope:    `$HOME/.clanky/`
//! - project scope: `./.clanky/` (the cwd is the project root)
//!
//! Precedence is **user < project**:
//!
//! - Resources that *clash* by name (settings fields, prompt templates,
//!   skills) are per-field/per-name overlays: a project-scope definition
//!   shadows the user-scope one with the same name.
//! - Resources that *add up* (context files) are read user first, project
//!   second, so project text lands later in the context — most backends
//!   weight later system messages more.
//!
//! Layering is *additive* by default: the project scope extends the user
//! scope rather than replacing it. A project that wants the user scope
//! gone for a resource says so explicitly, with `override` in
//! `settings.toml` (see [`Override`]): the user scope is then not read
//! for that resource at all, and only the project scope's copy counts.
//! [`scopes_for`] is where that decision is made — the user scope simply
//! leaves the list, so the readers stay unchanged.
//!
//! Layout inside each scope (all entries optional):
//!
//! ```text
//! .clanky/
//! ├── settings.toml   settings (per-field overlay onto the other scope)
//! ├── SYSTEM.md       system prompt text
//! ├── AGENTS.md       agent instructions
//! ├── skills/         skill markdown, loaded into context
//! ├── prompts/        prompt templates, invoked as /<name> slash commands
//! ├── logs/           plugin stderr logs (created on demand)
//! └── sessions/       session files (project scope only)
//! ```
//!
//! This module is the single source of truth for the layout; every other
//! module resolves its files through [`scopes`] and the constants here.

use std::path::{Path, PathBuf};

/// Settings file name inside a scope directory.
pub const SETTINGS_FILE: &str = "settings.toml";
/// System prompt file name inside a scope directory.
pub const SYSTEM_FILE: &str = "SYSTEM.md";
/// Agent instructions file name inside a scope directory.
pub const AGENTS_FILE: &str = "AGENTS.md";
/// Prompt templates directory inside a scope directory.
pub const PROMPTS_DIR: &str = "prompts";
/// Skills directory inside a scope directory.
pub const SKILLS_DIR: &str = "skills";
/// Logs directory inside the project scope directory; provider plugin
/// stderr is appended to `logs/plugin-<name>.log` (never the chat).
pub const LOGS_DIR: &str = "logs";
/// Sessions directory inside the project scope directory.
pub const SESSIONS_DIR: &str = "sessions";

/// One kind of user-scope configuration that a project can take over
/// wholesale, listed in the `override` array of `settings.toml`.
///
/// Without an override the two scopes are layered (user first, project
/// second); with one, the user scope is simply not read for that kind —
/// the project's copy is the only one, and an absent project copy means
/// "none" rather than "whatever the user had".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Override {
    /// `settings.toml`: the user file is not read, so every setting comes
    /// from the project file (or its built-in default).
    Settings,
    /// `SYSTEM.md`: the user file is not read. Only `SYSTEM.md` is meant —
    /// `AGENTS.md` is agent instructions, not the system prompt, and is
    /// still layered.
    System,
    /// `skills/`: the user directory is not read; only project skills
    /// exist (a project skill still shadows nothing, there being no user
    /// skills to shadow).
    Skills,
}

impl Override {
    /// The settings.toml spelling, as accepted in `override = [...]`.
    pub fn as_str(self) -> &'static str {
        match self {
            Override::Settings => "settings",
            Override::System => "system",
            Override::Skills => "skills",
        }
    }

    /// Parse one `override` entry; `None` for an unknown name.
    pub fn parse(name: &str) -> Option<Override> {
        match name {
            "settings" => Some(Override::Settings),
            "system" => Some(Override::System),
            "skills" => Some(Override::Skills),
            _ => None,
        }
    }
}

/// User scope directory: `$HOME/.clanky`. `None` when `$HOME` cannot be
/// determined (the user scope is then simply absent).
pub fn user_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".clanky"))
}

/// Project scope directory: `./.clanky`.
pub fn project_dir() -> PathBuf {
    PathBuf::from(".clanky")
}

/// Scope directories in layering order: user first, project last. Later
/// scopes shadow earlier ones for by-name resources and are read later for
/// additive ones.
pub fn scopes() -> Vec<PathBuf> {
    scope_list(user_dir().as_deref(), &project_dir(), &[], None)
}

/// Like [`scopes`], but for readers of one resource: when `resource` is
/// overridden, the user scope is not in the list at all, so the reader
/// sees the project scope only. This is where an [`Override`] turns into
/// behavior; the readers themselves stay unchanged.
pub fn scopes_for(overrides: &[Override], resource: Override) -> Vec<PathBuf> {
    scope_list(
        user_dir().as_deref(),
        &project_dir(),
        overrides,
        Some(resource),
    )
}

/// The scope list, with the user scope passed in rather than read from
/// `$HOME` — the pure form of [`scopes`] and [`scopes_for`], so the
/// layering rule can be tested without depending on the machine's real
/// configuration. `resource` is the resource the caller reads: `None`
/// means "every resource", so only a fully overridden user scope drops
/// out; `Some(r)` drops it when `r` is overridden.
fn scope_list(
    user: Option<&Path>,
    project: &Path,
    overrides: &[Override],
    resource: Option<Override>,
) -> Vec<PathBuf> {
    let user_dropped = match resource {
        Some(resource) => overrides.contains(&resource),
        None => {
            overrides.contains(&Override::Settings)
                && overrides.contains(&Override::System)
                && overrides.contains(&Override::Skills)
        }
    };
    let mut scopes = Vec::new();
    if !user_dropped && let Some(user) = user {
        scopes.push(user.to_path_buf());
    }
    scopes.push(project.to_path_buf());
    scopes
}

impl Override {
    /// The user-scope resource this override stops reading, as users know
    /// it in the layout (`settings.toml`, `SYSTEM.md`, `skills/`).
    pub fn user_resource(self) -> &'static str {
        match self {
            Override::Settings => "settings.toml",
            Override::System => "SYSTEM.md",
            Override::Skills => "skills/",
        }
    }
}

/// Human label list for an override set (`"system, skills"`), in the order
/// the resources are listed here rather than the order they were written.
/// Empty when nothing is overridden.
pub fn describe_overrides(overrides: &[Override]) -> String {
    overrides
        .iter()
        .map(|over| over.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One line naming what the user scope no longer contributes, for the
/// startup dump and `/system`: `None` when nothing is overridden, so the
/// callers can stay silent about an ordinary layered session.
///
/// It names the *files* rather than the override keywords, because the
/// question it answers is "why is my `~/.clanky/SYSTEM.md` not in here?".
pub fn override_note(overrides: &[Override]) -> Option<String> {
    if overrides.is_empty() {
        return None;
    }
    let resources: Vec<&str> = overrides.iter().map(|over| over.user_resource()).collect();
    Some(format!(
        "override: not reading the user scope's {} (project settings.toml)",
        resources.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake user scope, so these tests never depend on the real
    /// `$HOME` — which on any given machine may be empty, partly
    /// populated, or configured in ways that would change the outcome.
    const USER: &str = "/home/test/.clanky";
    const PROJECT: &str = ".clanky";

    #[test]
    fn scopes_are_user_then_project() {
        let scopes = scope_list(Some(Path::new(USER)), Path::new(PROJECT), &[], None);
        assert_eq!(scopes, vec![PathBuf::from(USER), PathBuf::from(PROJECT)]);
    }

    #[test]
    fn without_a_home_there_is_only_the_project_scope() {
        assert_eq!(
            scope_list(None, Path::new(PROJECT), &[], None),
            vec![PathBuf::from(PROJECT)]
        );
        assert_eq!(
            scope_list(
                None,
                Path::new(PROJECT),
                &[Override::Skills],
                Some(Override::Skills)
            ),
            vec![PathBuf::from(PROJECT)]
        );
    }

    #[test]
    fn an_overridden_resource_drops_the_user_scope() {
        let user = Some(Path::new(USER));
        let project = Path::new(PROJECT);
        let both = vec![PathBuf::from(USER), PathBuf::from(PROJECT)];
        let only_project = vec![PathBuf::from(PROJECT)];

        // No overrides: both scopes, user first.
        assert_eq!(scope_list(user, project, &[], Some(Override::Skills)), both);
        // One resource overridden: the project scope only, for that
        // resource's reader — the other readers are unaffected because
        // they ask about their own resource.
        for resource in [Override::Settings, Override::System, Override::Skills] {
            assert_eq!(
                scope_list(user, project, &[resource], Some(resource)),
                only_project
            );
        }
        // A different resource's override leaves the user scope in place.
        assert_eq!(
            scope_list(user, project, &[Override::System], Some(Override::Skills)),
            both
        );
        // Without a specific resource (`scopes()`), the user scope drops
        // only when everything it holds is overridden.
        assert_eq!(
            scope_list(user, project, &[Override::System, Override::Skills], None),
            both
        );
        assert_eq!(
            scope_list(
                user,
                project,
                &[Override::Settings, Override::System, Override::Skills],
                None
            ),
            only_project
        );
    }

    #[test]
    fn override_names_parse_and_print_identically() {
        for over in [Override::Settings, Override::System, Override::Skills] {
            assert_eq!(Override::parse(over.as_str()), Some(over));
        }
        assert_eq!(Override::parse("prompts"), None, "unknown names are errors");
        assert_eq!(Override::parse("System"), None, "names are lowercase");
        assert_eq!(
            describe_overrides(&[Override::System, Override::Skills]),
            "system, skills"
        );
        assert_eq!(describe_overrides(&[]), "");
        assert_eq!(override_note(&[]), None);
        assert_eq!(
            override_note(&[Override::System, Override::Skills]).as_deref(),
            Some(
                "override: not reading the user scope's SYSTEM.md, skills/ (project settings.toml)"
            )
        );
    }

    #[test]
    fn subpaths_follow_the_documented_layout() {
        let scope = Path::new(PROJECT);
        assert_eq!(
            scope.join(SETTINGS_FILE),
            PathBuf::from(".clanky/settings.toml")
        );
        assert_eq!(scope.join(SYSTEM_FILE), PathBuf::from(".clanky/SYSTEM.md"));
        assert_eq!(scope.join(AGENTS_FILE), PathBuf::from(".clanky/AGENTS.md"));
        assert_eq!(scope.join(PROMPTS_DIR), PathBuf::from(".clanky/prompts"));
        assert_eq!(scope.join(SKILLS_DIR), PathBuf::from(".clanky/skills"));
        assert_eq!(scope.join(LOGS_DIR), PathBuf::from(".clanky/logs"));
        assert_eq!(scope.join(SESSIONS_DIR), PathBuf::from(".clanky/sessions"));
    }
}
