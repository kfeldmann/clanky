# Clanky

An AI coding agent/harness for the terminal, written in Rust.

## Configuration & context

Clanky reads configuration and context from two scopes. The **user scope**
holds your personal, machine-wide preferences; the **project scope** holds
settings and instructions for one project. Both use the same layout, all
entries optional:

```text
~/.clanky/                  user scope
./.clanky/                  project scope (the cwd is the project root)
├── settings.toml           settings
├── SYSTEM.md               system prompt text
├── AGENTS.md               agent instructions
├── skills/                 skills, loaded into the context
│   ├── <name>.md           plain markdown skill
│   └── <name>/SKILL.md     pi-style directory skill
├── prompts/                prompt templates, invoked as /<name> slash commands
│   └── <name>.md
├── plugins/                plugin declarations (providers; arrives in M8)
└── sessions/               session files (project scope only)
```

### Precedence

The layering rule is **user < project** for everything, applied per
resource:

| Resource            | On a name clash                          |
| ------------------- | ---------------------------------------- |
| `settings.toml`     | project field wins (per-field overlay); CLI flags win over both |
| prompt templates    | project `prompts/<name>.md` shadows the user one |
| skills              | project `skills/<name>.md` shadows the user one |
| `SYSTEM.md`/`AGENTS.md` | additive: read user first, project second, so project text lands later in the context (most backends weight later system messages more) |

A project-root `AGENTS.md` (the ecosystem convention) is honored in
addition to `./.clanky/AGENTS.md`, and comes after both scopes.

### `settings.toml`

All fields are optional; unknown keys are rejected with a parse error.

| Field      | Type             | Meaning                                            |
| ---------- | ---------------- | -------------------------------------------------- |
| `provider` | string           | Provider name (e.g. `deepinfra`)                   |
| `model`    | string           | Model identifier as known to the provider          |
| `thinking` | string           | Thinking budget or level (`1024`, `low`, `off`)    |
| `sampling` | table of strings | Sampling parameters, e.g. `temperature = "0.7"`    |
| `prompt`   | string           | Prompt text (mainly useful on the CLI, not in a file) |

Example:

```toml
provider = "deepinfra"
model = "meta-llama/Llama-3.3-70B-Instruct"
thinking = "off"

[sampling]
temperature = "0.7"
top_p = "0.9"
```

Notes:

- Sampling values stay strings in the file; the provider validates and
  coerces them, so an invalid value fails at call time, not load time.
- Precedence is per field: CLI flag > project file > user file, and a
  scope only overrides fields it actually sets.

### Session precedence note

Sessions always live in the **project scope** (`./.clanky/sessions/`);
there is no user-scope session store.

The transcript is printed straight to the terminal — no alternate
screen, no mouse capture — so scrolling and copy/paste are your
terminal's own (native scrollbar or wheel, native text selection), and
the conversation stays in the terminal after clanky quits. `ctrl+l`
erases the visible screen (scrollback is kept).

## TUI keys

| Key | Action |
| --- | --- |
| `Enter` | submit (queued while a turn is streaming) |
| `↑`/`↓` | recall previously submitted prompts (the first `↑` saves the current input as a draft; `↓` past the newest entry restores it) |
| `Tab` | complete the file path at the caret: longest common prefix first, further `Tab` presses cycle candidates |
| `ctrl+e` | edit the prompt buffer in `$EDITOR` (the TUI suspends; an emptied buffer clears the input) |
| `ctrl+c` / `ctrl+d` | quit |
| `ctrl+l` | clear the visible screen |
| mouse wheel, scrollbar | scroll (native terminal scrolling) |
| text selection | copy/paste (native terminal selection) |
| `/` (empty input) | command palette |
| `/system` | show the assembled system prompt (context files and skills, as sent to the model) |

The TUI only starts when stdin *and* stdout are terminals; piped input
falls back to plain one-shot mode.
