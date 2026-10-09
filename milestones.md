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

## ✔ M8 — Plugin system (providers)
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

  As built: the plugin's name and default model come from the `hello`
  handshake (core has no provider table); one long-lived process per session,
  spawned lazily, moved into the TUI turn worker and handed back on
  completion; plugin stderr → `./.clanky/logs/plugin-<name>.log`; a crashed
  process is discarded and respawned on next use (no auto-retry of the turn).
  The plugin-side wire rules live once in `clanky_protocol::serve`, shared by
  the real binary and the loopback transport. The hello-world Python plugin
  lives in `examples/hello-world-provider/`.

## ☐ M9 — MCP tools (deferred until needed)
- Speak to MCP servers over stdio; their tools join `bash` behind the `Tool`
  trait from M2
- Plugin declarations may name an MCP server instead of a provider
- Done when: a third-party MCP server's tools work in a session.

## ☐ M10 — Polish & release
- Error messages, logging, docs, packaging (crates.io / release binaries)
- Example provider plugin in the repo (hello-world, Python) — already added
  in M8 (`examples/hello-world-provider/`), so M10 only needs to link it
- **LiteLLM provider plugin** (`clanky-provider-litellm`, `crates/
  clanky-provider-litellm`): the proxy's OpenAI-compatible `/v1` surface,
  pinned to captures from a live gateway (`tests/fixtures/`). Reads
  `LITELLM_API_KEY` / `LITELLM_BASE_URL` / `LITELLM_MODEL`; metadata
  (`/model/info`, called bare — the `/v1/` alias can be denied a virtual
  key) is optional and degrades to the plain `/v1/models` catalog
  when a key is denied it; `reasoning_effort` and `tools` are filtered per
  model from that metadata so a heterogeneous proxy does not 400. No built-in
  default model (the catalog is arbitrary), so `LITELLM_MODEL` or an explicit
  `model`/`--model` is required. Design notes: `litellm-provider-planning.md`.
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
  As built: `/md` exports authored text (user prompts, assistant answers)
  verbatim — it is Markdown, and a fenced code block inside it is balanced
  by its own closing fence, so no wrapping is needed. Only machine output
  (thinking, tool results/args, errors) is fenced. (Earlier builds wrapped
  any assistant message containing triple backticks in a longer fence;
  that degraded the common case — an answer with a code block — to literal
  text and was removed.)
- Done when: a stranger can `cargo install` and use it.

## ☐ M11 — Anthropic provider plugin (OAuth)
New provider plugin `crates/clanky-provider-anthropic` (Anthropic Messages
API), driven exclusively by OAuth — our enterprise tenant has no API keys.
Split into a/b/c, each shippable and testable on its own. Design notes,
confirmed flow parameters, and the spike checklist:
`anthropic-provider-planning.md`. The OAuth client is not a published
Anthropic surface, but the flow is already proven on our tenant: the Pi
coding agent (github.com/earendil-works/pi, MIT) performs it against this
account, so its source is our reference implementation — see the
planning file for the extracted constants (client_id, endpoints, beta
headers, identity system block, tool-name mapping).

Architectural constraint: the plugin's stdin/stdout *are* the protocol
channel, so the no-browser flow cannot prompt the user mid-session. It lives
in an offline `login` subcommand on the plugin binary (PKCE, print authorize
URL, read pasted code from stdin, token exchange, store credentials). Same
shape as `ant auth login --no-browser`; needs no protocol change (protocol
§1 already assigns auth material to the plugin, never to core).

Ordering note: OAuth is scheduled **before** streaming on purpose — it is
independent of the wire adapter (the login subcommand and credential store
don't care how the chat path streams; only the bearer-vs-`x-api-key` header
mode couples them) — so M11b's streaming work is live-testable against the
real tenant.

### ☐ M11a — Confirm-and-capture spike, then OAuth + minimal non-streaming chat
- **Spike first, before any workspace code:** one live capture of the full
  dance — PKCE authorize URL → pasted code → token exchange → one
  non-streaming `/v1/messages` call with `Authorization: Bearer`. Downgraded
  from go/no-go to confirm-and-capture: the flow is already proven on this
  tenant by the Pi coding agent, whose source is the reference (see
  `anthropic-provider-planning.md` for the extracted parameters). The spike
  still gates — it confirms the constants hold for us and records what Pi
  cannot: org/workspace scoping enforcement, token + refresh-token
  lifetimes, the exact beta header string, and whether our own tool names
  pass through or need Claude Code case-normalization. Capture fixtures
  seed the later test suites. Fallbacks (only if something breaks): reuse an
  `ant` CLI profile's stored credentials (`~/.config/anthropic`), or get the
  supported client path confirmed with Anthropic.
- `login` subcommand (PKCE S256, paste flow) + credential store
  (`access_token` / `refresh_token` / `expires_at`) under
  `~/.clanky/providers/anthropic/`
- Automatic refresh before expiry; on failure a clear `auth` error: "login
  expired — run `clanky-provider-anthropic login`" (refresh-token lifetime is
  finite; this will be the most common failure mode, keep it actionable)
- Serving process: handshake + non-streaming chat over the bearer path;
  basic `tool_use`/`tool_result` mapping included (non-streaming works for
  tools) so M11b is a delta-mapping problem only, not a structure problem
  discovered mid-stream
- Done when: `clanky -p "say hi" --provider anthropic` works on the
  enterprise account, headless (SSH/container), with no key.

### ☐ M11b — Streaming + agentic fidelity
- SSE → protocol chunks: `content_block_delta` text/thinking deltas →
  `kind: "text"` / `kind: "thinking"`; `input_json_delta` →
  `toolCallArgs`; finish-reason and usage mapping
- Pinned live-capture fixtures (the LiteLLM M10 pattern); golden transcripts
- Live-testable against the real tenant: bash tool loop end to end
- Done when: `clanky -p "count files in /tmp" --provider anthropic` runs
  bash and answers, streamed.

### ☐ M11c — API-key fallback + polish
- `x-api-key` request mode beside bearer (the two auth modes differ only in
  headers) — covers any tenant/user that *does* have a key
- Actionable-error polish, README section,
  `provider-protocol.md` note recommending the login-subcommand pattern for
  OAuth-based plugins
- Done when: an `ANTHROPIC_API_KEY` user gets identical behavior through the
  same adapter.

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
