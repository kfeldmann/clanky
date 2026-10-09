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
