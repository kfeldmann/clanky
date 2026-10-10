# Clanky - an AI coding agent/harness for the terminal, written in Rust

The clanky project is in `/work`.

You do not have `git` available. You are in an Alpine Linux container with the following packages installed: 

- bash
- python3
- py3-pip
- lynx
- curl
- jq
- sed  # gnu
- gawk # gnu
- grep # gnu
- rust
- cargo
- rust-clippy
- rustfmt

You have network access and can search the web using:
```
lynx -dump https://lite.duckduckgo.com/lite?q=<url-encoded-query>
```

## Project state

M0-M8 are complete and shipped. Remaining: M9 (MCP tools, deferred until
needed) and parts of M10 (polish & release). See milestones.md — checkboxes
mark progress, and its "As built" notes record decisions made during
implementation. Check them before redesigning something.

## Project invariants

- **User-provided values are authoritative.** Never silently rewrite, clamp,
  coerce, or drop a value the user supplied (settings file, CLI flag, or other
  option). Pass provided values through exactly as given, and compute defaults
  only for values the user did not provide.
  (Precedence layering — CLI over project over user — is intended, not a
  violation: that's the user choosing, not the program deciding.)
- **Don't pre-validate what the server will validate.** The rule above is about
  not *changing* the user's value — not about re-implementing validation
  locally. If Clanky can faithfully pass a value to the provider, pass it; if
  the provider rejects it, the provider's error is the actionable error we
  promised, and surfacing it unchanged satisfies the rule. Err on the side of
  expecting the server's message to be useful. Add a local check with our own
  error message only when users hit a case where the server's message is weak
  or confusing — never speculatively. Keep Clanky simple and honest: one
  source of truth for what's valid is the provider itself.

## Key docs

- README.md — user-facing ground truth: config layout, settings.toml, env
  vars, CLI usage, plugin authoring. Update it in the same change as any
  behavior change.
- milestones.md — the plan. Keep status current when you finish work.
- provider-protocol.md — the wire protocol between Clanky and provider
  plugins (target version 1). Update the spec and the implementation
  together.
- litellm-provider-planning.md, provider-plugins-plan.md — design notes for
  specific features.

## Workspace layout

- `crates/clanky` — core: CLI, settings, context assembly, agentic turn loop
  (src/turn.rs), bash tool, sessions, TUI (src/tui/)
- `crates/clanky-protocol` — provider plugin protocol: message types, client,
  in-process loopback transport, and `serve` (plugin-side wire rules live
  here once, shared by real binaries and tests)
- `crates/clanky-provider-deepinfra` — DeepInfra provider plugin
  (OpenAI-compatible)
- `crates/clanky-provider-litellm` — LiteLLM proxy provider plugin
- `examples/hello-world-provider/` — dependency-free Python provider plugin;
  the smallest template for writing a new one
- `crates/clanky/tests/` — integration tests: golden session transcripts
  (tests/golden/), plugin e2e and pipe tests
- `target/replay`, `target/tmp` — scratch space; never commit

## Build, test, run

- CI gates (keep green):
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
- `unsafe` is forbidden workspace-wide; clippy warnings fail CI.
- Run: `cargo run -p clanky -- [-p "prompt"]` (TUI without `-p`).
- `./build-linux-musl` + `./build-runner` build the Dockerized release
  binary; `./run` executes it (requires `DEEPINFRA_TOKEN` or
  `LITELLM_API_KEY`).

## Environment variables

- DeepInfra: `DEEPINFRA_API_KEY` (fallback `DEEPINFRA_TOKEN`), optional
  `DEEPINFRA_URL` override
- LiteLLM: `LITELLM_API_KEY`, `LITELLM_BASE_URL`, `LITELLM_MODEL`
- Provider keys are read by the provider plugin, never by Clanky core.

## DeepInfra (first supported provider)

- OpenAI Chat Completions API:
  https://docs.deepinfra.com/api-reference/chat-completions/openai-chat-completions.md
- DeepInfra documentation index: https://docs.deepinfra.com/llms.txt
