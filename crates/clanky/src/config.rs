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

use std::path::PathBuf;

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
    let mut scopes = Vec::new();
    if let Some(user) = user_dir() {
        scopes.push(user);
    }
    scopes.push(project_dir());
    scopes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_user_then_project() {
        let scopes = scopes();
        assert_eq!(scopes.len(), 2);
        assert_eq!(scopes[0], user_dir().unwrap());
        assert_eq!(scopes[1], PathBuf::from(".clanky"));
        assert_eq!(scopes[1], project_dir());
    }

    #[test]
    fn subpaths_follow_the_documented_layout() {
        let scopes = scopes();
        let scope = scopes.last().unwrap();
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
