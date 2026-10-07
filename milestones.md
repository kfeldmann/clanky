# Clanky — Milestones

Companion to `plan.md`. Each milestone is shippable and testable on its own.
M1–M2 form the MVP; everything after is incremental.

Architectural decisions already made:
- **No sandbox/trust system in Clanky.** Isolation is the user's responsibility:
  they run Clanky inside a Docker container with only the project directory
  mounted and only the desired commands installed. Clanky just runs bash.
- **Plugins are subprocesses** speaking JSON over stdin/stdout (any language,
  crash-isolated).
- **All providers are plugins — homogeneous architecture.** DeepInfra ships as a
  provider plugin binary from the same workspace (`cargo install` installs
  `clanky` + `clanky-provider-deepinfra`, both into `~/.cargo/bin`); Clanky
  discovers `clanky-provider-*` on `$PATH`, so the default experience is
  zero-config. Core contains no
  provider-specific code. The provider protocol is defined in M1; the DeepInfra
  client is protocol-shaped from the start, so the M8 process-boundary work is
  mechanical.
- **MCP deferred** (see M9): when needed, it becomes a source of `Tool`s beside
  bash. M2's agentic loop consumes tools via a small internal `Tool` trait so
  this is a plug-in, not a rework.
- **`bash` stays a built-in tool, not an MCP plugin.** It is already a child
  process (crash-isolated), it is the one tool every session needs (default UX
  must not depend on plugin infra), and its streaming output / direct
  cancellation would degrade behind MCP's single-result request model. MCP adds
  tools beside it; it does not replace it.
- Settings: TOML. Sessions: JSONL.

## ️✔ M0 — Skeleton
- Cargo workspace, CI (fmt, clippy, test)
- CLI arg parsing (`clap`): `-p`, `--provider`, `--model`, `--thinking`, `--sampling`, prompt args
- Settings file loading (TOML via `serde`): `~/.clanky/settings.toml`, `./.clanky/settings.toml` with layering
- Exit codes + error type (`thiserror`/`anyhow`)
- Done when: `clanky --help` works, settings merge correctly.

## ✔ M1 — Non-interactive pipe (single turn)
- **Provider protocol defined** (handshake/capabilities, list-models,
  chat-stream request, stream chunk, error, cancel) — JSON over stdio shapes,
  exercised in-process with a direct-call transport stub
- DeepInfra client (OpenAI-compatible chat completions, non-streaming first)
  implementing the protocol shapes in-process
- Auth via per-provider env var (e.g. `DEEPINFRA_API_KEY`), read by the provider
  code itself — Clanky core never handles provider keys
- Prompt from argv or stdin; `-p` prints response and exits
- Context assembly stub: read `AGENTS.md` / `SYSTEM.md` if present
- Done when: `clanky -p "say hi"` prints a reply.

## ✔ M2 — Streaming + tool loop
- SSE streaming through the protocol-shaped provider interface
- Internal `Tool` trait (name, schema, execute) + `bash` implementation
- Agentic loop: tool calls parsed, executed via `Tool`, results fed back
- Mock provider for tests; golden-file session transcripts
- Thinking budget + sampling params plumbed through
- Done when: `clanky -p "count files in /tmp"` runs bash and answers.

## ✔ M3 — TUI basics
- Ratatui + crossterm: chat view (scrollable transcript), input line with caret,
  streaming render, markdown wrap (fences, headings, lists, bold/italic/code)
- Turns run on a worker thread; events stream to the render loop over a channel
- Fallback to command-line mode when stdin/stdout redirected (also `clanky` with
  a piped stdin and no `-p`)
- Keys: Enter submit (queued while streaming), Ctrl+C/Ctrl+D quit, Ctrl+L clear,
  PgUp/PgDn/↑/↓ + mouse wheel scroll, basic line editing (Home/End/Backspace/Delete)
- Status line: provider · model, streaming/queued state, last-turn token usage
- Done when: interactive chat works with streaming, `|` pipe still prints plain.

## ✔ M4 — Sessions
- Session file format (JSONL: one record per event, versioned header)
- `/name`, `/resume` (picker), autosave
- Done when: kill the TUI mid-session, resume, full context restored.

## ✔ M5 — Slash commands + pickers
- `/model`, `/provider`, `/thinking`, `/sampling`, prompt templates `/\<name\>`
- Unknown command → error, not sent to the model
- Picker component (reused for `/resume`)
- Done when: all pickers work; unknown `/foo` errors cleanly.

## ✔ M6 — Context & config completion
- Full `~/.clanky/` and `./.clanky/` layout: `AGENTS.md`, `SYSTEM.md`, `skills/`, `prompts/`, `plugins/` (the `plugins/` directory is dropped in M8 — never used; see M8)
- Layering rules user < project; doc the precedence
- Done when: skills/prompts load from both scopes.

## ✔ M7 — Input ergonomics
- Tab completion of file paths
- `$EDITOR` launch key (suspend TUI, edit, resume)
- Done when: both work in TUI without breaking rendering.

## ☐ M8 — Plugin system (providers)
- Process-boundary transport for the provider protocol: spawn, handshake,
  long-lived process (keep alive for the session)
- Migrate DeepInfra client into `clanky-provider-deepinfra` binary (mechanical —
  it is already protocol-shaped); core keeps only the protocol client
- Cancellation (protocol cancel message + kill fallback)
- Crash isolation: restart policy; plugin stderr → debug log, never the chat;
  errors pass through intact (a 401 surfaces as "401: bad API key")
- PATH-only auto-discovery of `clanky-provider-*` executables on `$PATH`;
  protocol version negotiated at handshake
- Default model advertised by the plugin at handshake (core drops its
  hardcoded per-provider table)
- Done when: `clanky -p "say hi"` works via the spawned DeepInfra plugin, and a
  hello-world provider plugin written in Python serves a chat turn.

  Decisions (see `provider-plugins-plan.md`): discovery is PATH-only — no
  sibling-of-binary scan, and no settings declaration in M8 (plugins inherit
  core's environment, so nothing needs configuring). `[plugins.<name>]`
  declarations arrive in M9 for MCP servers. `.clanky/plugins/` is removed,
  not reserved.

## ☐ M9 — MCP tools (deferred until needed)
- Speak to MCP servers over stdio; their tools join `bash` behind the `Tool`
  trait from M2
- Plugin declarations may name an MCP server instead of a provider
- Done when: a third-party MCP server's tools work in a session.

## ☐ M10 — Polish & release
- Error messages, logging, docs, packaging (crates.io / release binaries)
- Example provider plugin in the repo (hello-world, Python)
- TUI quality-of-life: `↑`/`↓` prompt recall, `/system` (view the assembled
  system prompt); fix: fresh TUI sessions now seed the system context like
  pipe mode
- Provider retries: retryable errors (`rateLimit`, `backend` — network
  timeouts included) are retried up to `max_retries` (default 5) with
  exponential backoff (500 ms … 30 s cap), honoring `Retry-After`; a retry
  is surfaced in the transcript (TUI) or on stderr (pipe), and a round that
  already streamed content is never replayed
- TUI reworked to linear terminal output: the transcript is printed straight
  to the normal buffer (no alternate screen, no mouse capture), so native
  scrolling and selection work and history persists after quitting; replaced
  the ratatui viewport, mouse drag selection, and OSC 52 copy entirely
- Done when: a stranger can `cargo install` and use it.

## Open Questions
1. ✔ **Provider plugin protocol details:** exact message shapes, streaming
   granularity, cancellation semantics, how `list-models` interacts with
   `/model` picker. Mostly settled during M1 design.
   Answer: See ./provider-protocol.md
2. ✔ **Plugin startup policy:** eager at launch (catches broken plugins early) vs
   lazy on first use (faster startup). Recommend lazy with error surfacing.
   Answer: lazy
3. ✔ **Discovery details:** how aggressively to auto-discover sibling binaries
   (`clanky-provider-*` naming convention?) vs requiring settings entries.
   Answer: naming convension (refined in M8 planning to PATH-only lookup, no
   sibling scan and no settings entries — see `provider-plugins-plan.md`)
4. ☐ **MCP scope when it arrives:** tools first; prompts/resources later if wanted.
