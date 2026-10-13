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
  As built (revised with protocol v2): sampling is a pure pass-through —
  `[sampling]`/`--sampling` accept any backend parameter (`seed`,
  `repetition_penalty`, `reasoning_effort`, …) and the backend validates it,
  per the project invariants (no local whitelist, no local value checks).
  Protocol v2 replaced the closed `temperature`/`topP`/`maxTokens` struct
  with an open map keyed by backend parameter names; the handshake rejects
  v1 plugins loudly instead of silently dropping parameters. Settings
  values may be any TOML value; untyped strings decode as JSON when they
  parse as JSON (`temperature = "0.7"` = `temperature = 0.7`). Provider
  plugins merge `sampling` into the backend request verbatim, fill required
  or derived knobs (Anthropic `max_tokens`, `thinking` → `reasoning_effort`)
  only when the user did not set the key, and own the request's structural
  fields (`model`, `messages`, `tools`, `stream`, …). Across layers
  (user file, project file, `--sampling`) the map merges per key: later
  layers win key-by-key and keys set only in a lower layer are kept — no
  supplied value is silently dropped.
- Done when: `clanky -p "count files in /tmp"` runs bash and answers.

## ✔ M3 — TUI basics
- Ratatui + crossterm: chat view (scrollable transcript), input line with caret,
  streaming render, markdown wrap (fences, headings, lists, tables w/ alignment, bold/italic/code)
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
- DONE - Example provider plugin in the repo (hello-world, Python) — already added
  in M8 (`examples/hello-world-provider/`), so M10 only needs to link it
- DONE - **LiteLLM provider plugin** (`clanky-provider-litellm`, `crates/
  clanky-provider-litellm`): the proxy's OpenAI-compatible `/v1` surface,
  pinned to captures from a live gateway (`tests/fixtures/`). Reads
  `LITELLM_API_KEY` / `LITELLM_BASE_URL`; metadata
  (`/model/info`, called bare — the `/v1/` alias can be denied a virtual
  key) is optional and degrades to the plain `/v1/models` catalog
  when a key is denied it; the metadata curates the model list (the picker
  hides tool-less models via the `supportsTools` hint) while `tools` and
  `reasoning_effort` pass through to the proxy unconditionally. The built-in default model is
  `claude-haiku-4-5-20251001` (overridable with `model`/`--model`). Design notes: `litellm-provider-planning.md`.
  As built (revised after review): an earlier build filtered `reasoning_effort`
  and `tools` out of the request per model ("so a heterogeneous proxy does
  not 400"), which silently ignored user intent and pre-empted the proxy's
  own validation (project invariants 1 and 2). Now the request is never
  edited: thinking/tools are sent unconditionally and the proxy's error is
  surfaced unchanged (it tells the user to set thinking off / that tools are
  not supported); the `/model/info` hints instead carry `supportsTools` per
  model (new optional protocol hint, additive in v2) and the `/model` picker
  hides tool-less models. `--model` bypasses the picker, so such a model is
  still reachable and answers with the proxy's error.
- DONE - TUI quality-of-life: `↑`/`↓` prompt recall, `/system` (view the assembled
  system prompt); fix: fresh TUI sessions now seed the system context like
  pipe mode
- DONE - Provider retries: retryable errors (`rateLimit`, `backend` — network
  timeouts included) are retried up to `max_retries` (default 5) with
  backoff growing ×1.25 per retry (500 ms … 30 s cap), honoring `Retry-After`; a retry
  is surfaced in the transcript (TUI) or on stderr (pipe), and a round that
  already streamed content is never replayed
- DONE - TUI reworked to linear terminal output: the transcript is printed straight
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
- DONE - Interrupted turns lose nothing streamed: when a turn is cut short by a
  provider error mid-stream or by ctrl+c quitting mid-stream, the
  accumulated deltas are consolidated and written to the session (for
  thinking and text; partial tool calls/results are not kept), so
  `/resume` shows the same conversation the terminal did and "please
  continue" sends the full chat as context
  As built: the turn loop flushes the in-flight round as one `Round` event
  when a round dies with streamed content (also appending the partial text
  to `messages`), and the TUI accumulates deltas not yet consolidated into
  a round record; `record_done` and the quit path (after draining the
  events the worker already sent) flush that accumulator — a completed
  round supersedes it, so nothing is written twice.
- DONE - Timeouts removed — unbounded by design: the 600 s HTTP request timeout in
  all three provider backends and the 600 s default bash command timeout are
  gone. Requests/streams are never killed mid-answer and commands run until
  they finish; the 15 s connect timeout stays. A voluntary per-call
  `timeout_seconds` is the only command deadline — accepted range is any
  positive integer (schema `minimum: 1`, no `maximum`; 0 or non-integer →
  error). Trade-off accepted: a stuck upstream or hanging command holds the
  turn indefinitely (ctrl+c is the escape hatch).
- DONE - TUI rendering stability (status-line duplication on `$EDITOR`
  return and resize; invisible last column under GNU screen): the tail
  repositioning now uses absolute cursor addressing (CUP, clamped by every
  terminal) instead of "cursor next line with a huge count", which VTE
  (xfce4-terminal) treats as scrolling — that was the duplicate status line
  after ctrl+e and the status-line ladder while resizing. A fresh anchor
  (startup, resize, editor roundtrip) is placed with absolute CUP (clamped
  everywhere) so the block ends on the bottom row; steady-state redraws
  stay cursor-relative, because printing a growing transcript scrolls the
  screen and invalidates every absolute row address — the first attempt
  tracked an anchor row and used CUP for steady-state redraws, which
  pushed the tail into scrollback once per render. And the last
  terminal column is never painted: writing it sets the pending-wrap flag
  (the following CR/LF scrolls, and GNU screen defers displaying a
  character written there until the screen scrolls — the "invisible last
  column"), so the whole layout is computed one column narrower and the
  reserved column stays blank. Everything else (native scrolling, printed
  history, popups) is unchanged.
- Done when: a stranger can `cargo install` and use it.

## ✔ M11 — Anthropic provider plugin (OAuth)
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

### ✔ M11a — Confirm-and-capture spike, then OAuth + minimal non-streaming chat
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

### ✔ M11b — Streaming + agentic fidelity
- SSE → protocol chunks: `content_block_delta` text/thinking deltas →
  `kind: "text"` / `kind: "thinking"`; `input_json_delta` →
  `toolCallArgs`; finish-reason and usage mapping
- Pinned live-capture fixtures (the LiteLLM M10 pattern); golden transcripts
- Live-testable against the real tenant: bash tool loop end to end (do not build live tests into the code)
- Done when: `clanky -p "count files in /tmp" --provider anthropic --model claude-haiku-4-5-20251001` runs
  bash and answers, streamed.

### ✔ M11c — API-key fallback + polish
- `x-api-key` request mode beside bearer (the two auth modes differ only in
  headers) — covers any tenant/user that *does* have a key
- Actionable-error polish, README section,
  `provider-protocol.md` note recommending the login-subcommand pattern for
  OAuth-based plugins
- Done when: an `ANTHROPIC_API_KEY` user gets identical behavior through the
  same adapter.

  As built: auth became `AuthMode` (`Bearer(Credentials)` / `ApiKey(String)`)
  at the backend boundary; `apply_auth` sets the spike-captured OAuth headers
  vs. the documented plain key headers (`x-api-key` + `anthropic-version`
  only — no Claude Code identity on the key path). `$ANTHROPIC_API_KEY` is
  checked per request and wins over the store (explicit env var = the
  user's choice, per the project invariants); no refresh/expiry on that
  path. Error polish: "not logged in — run `clanky-provider-anthropic
  login`, or set ANTHROPIC_API_KEY" when nothing is usable (the
  "login expired" message stays re-login-only). Best-effort caveat: our
  tenant issues no keys, so the key path is header-tested only (structural
  tests + a mock-backend chat pinned to the same fixtures); first live
  key-user should sanity-check against the real API.

### ☐ M12 — Refinements, Bug Fixes
1. [x] Add support for ctl+u in the input field (clear from cursor to beginning of line)
2. [x] Print-mode (-p) output changes
   - [x] Thinking text seems to end with no newline (should trim and add two newlines so there is a blank after the thinking block)
   - [x] Tool results are not truncated. Should behave like the TUI but without the color
   - [x] Tool results have their newlines replaced with a symbol. Remove that functionality. Copy the TUI behavior but without color
   - [x] Add `-q/--quiet` which prints only the agent response messages and errors. No thinking. No tool calls or results. `-q` is only effective with `-p` (should be rejected in TUI mode)
     - As built: the TUI's result-preview function moved to `clanky::tools::preview_result`
       (line-capped with a `… (+N more lines)` note) and is shared by the TUI transcript and
       pipe-mode stderr, so the two can't drift; the old newline-collapsing `truncate_for_display`
       was deleted. Print-mode events go through `PrintSink` in `main.rs` (testable against
       captured streams). `-q` in TUI mode is a usage error (`Error::QuietRequiresPrint`); in
       quiet mode retry notices are suppressed too (they're activity, not errors).
   - [x] Every print-mode entry gets exactly one blank line after it. Assistant
     text blocks previously ended with whatever trailing whitespace the stream
     happened to deliver (often none at all), so `clanky -p` printed a response
     with no final newline. Text blocks now accumulate with trailing whitespace
     held back — the same mechanism thinking blocks already used — and close on
     a clean trim plus one blank line, on both stdout and stderr. A close is
     triggered by the next discrete event (tool call, tool result, retry notice,
     thinking, or a new round's text — each closed block reopens as a fresh
     paragraph) and once at end of turn, on both success and error. The blank
     line is emitted even in `-q` mode, so a quiet response is always a
     well-formed paragraph (`clanky -p -q` output ends with `\n\n`).
     Discrete stderr entries (tool calls, tool results, retry notices) already
     ended with a blank line; they were verified and are now pinned by tests.
     `flush_partial_round` already fed the streamed text back into the
     conversation on a mid-stream error, so the held-back whitespace being
     dropped had no effect on what the model sees. The old post-turn
     "finish the line" `writeln!` in `run_pipe` is gone — `close_blocks` owns
     all separators now, so stdout formatting lives in one place.
3. [x] Status line should de-content gracefully as terminal narrows
   - Drop one element at a time from the status line when the width is too narrow. Drop elements in the following order:
       - provider
       - thinking-budget
       - model
       - cost
       - session-name
       - queued indicator (can reduce to just the dot/circle for narrow terminals)
       - context-percentage
   - The "queued" indicator is transient. It normally uses a symbol and a word. In narrow terminals it could reduce to just the symbol
   - The hints (/ commands, ctl+c quit) replace the cost and context indicators if price data is not available
   - Another way to think of it: Add elements one at a time from the bottom of this list towards the top. Add as many as will fit.
   - Example status lines at different terminal widths, showing the desired ordering/placement of the elements as well as the addition of more elements as the width increases:
       session-name queued-dot (ctx xx% | hints)
       session-name queued-dot ($x.xxxx ctx xx% | hints)
       model session-name queued-dot ($x.xxxx ctx xx% | hints)
       model think xxxx session-name queued-full ($x.xxxx ctx xx% | hints)
       provider model think xxxx session-name queued-full ($x.xxxx ctx xx% | hints)  <-- current status line
   - As built: `status_line` in `src/tui/screen.rs` builds the line by adding elements one at a
     time from the bottom of the drop list upward, keeping each only if the whole line still
     fits. The cost and context percentage share one right-hand group (`$x.xxxx · xx% ctx`);
     when price data is unavailable that group is the hints instead. The queued indicator is
     two elements — the dot (kept with session-name as long as possible) and the word (added
     only when there is room). Below the hints' width, elements keep dropping until the
     provider alone fits, then nothing. Coverage in
     `status_line_decontents_gracefully_as_the_terminal_narrows`.
4. [x] Muli-line input (produced via $EDITOR) is displayed in the input field with each line starting directly below the end of the previous (stair-step)
   - As built: `input_rows` in `src/tui/screen.rs` renders one row per physical line of the
     input buffer — the first after the prompt (`● ❯ `), the rest indented to the same
     column — so every line starts at the same place instead of stair-stepping. Each row
     windows horizontally (caret stays visible), the caret parks on the row containing the
     cursor (the footer's caret became a `(row, col)`), and when the input has more lines
     than fit, the rows around the caret are kept. Coverage in
     `multi_line_input_starts_each_row_at_the_prompt_column` and friends.
5. In `--help`, it says "piped stdin is picked up in M1". Update that message now that piped stdin is a stable feature.
6. [x] Terminal corruption while thinking streams (status line duplicated upward ~20
   rows, then overwritten with the proper content; tool calls printed cleanly)
   - Root cause was caret parking, not content: the caret's row is a *footer*-relative
     offset (status + input), but `Screen::render` measured it from the whole tail,
     which while an entry streams also holds the streaming entry's rows above the
     footer. Only streamed entries (thinking blocks, assistant text) grow that gap —
     tool calls are sealed instantly, so they were unaffected. The cursor parked too
     high; the next render (a relative MoveUp from the caret) started its redraw that
     many rows early, leaving stale copies of the tail below — the duplicated status
     line the next render then overwrote. Fixed by skipping the streaming entry's
     tail rows in the parking math (`entry_tail_rows` in `src/tui/screen.rs`).
   - Hardened while in there: `print_line` never emits control characters (the wrap
     math budgets zero columns for them via `unicode-width`, but a real terminal moves
     the cursor for a tab — potentially onto/past the reserved last column, setting the
     pending-wrap flag; raw ESC could be taken as a control sequence), and
     `markdown::render` expands tabs to 8-column stops first so the wrap math and the
     terminal agree on code-snippet indentation in thinking blocks. Regression tests:
     `thinking_tabs_expand_to_stable_rows`, `print_line_never_emits_control_characters`,
     `streaming_thinking_overflow_keeps_one_status_line`.

### ✔ M13 — Parallel subagents
1. [x] `clanky-subagents` binary (`crates/clanky-subagents`, no dependency on
   clanky core): JSON run description via argv or stdin, one `clanky -pq`
   subprocess per job, bounded concurrency (`max_parallel`, default all),
   per-job timeout (`timeout_secs`, default none) killing the job's process
   group, per-stream 1 MiB tail capture with truncation marker, result JSON in
   input order. Per-job clanky option overrides (`provider`, `model`,
   `thinking`, `sampling` string, `max_tool_rounds`, `max_retries`) plus a
   run-level `defaults` object (job wins per field); options are an
   allow-list, so `-p`/`-q`/`--resume`/`--help`/`--version` can't appear.
   Runner exits 0 for any completed batch (per-job exits in JSON), nonzero
   only for bad input (unknown keys named via `deny_unknown_fields`).
   - As built: `wait_timeout` + a per-job process group (`process_group(0)`)
     so a timeout kill reaches clanky's own children, which otherwise inherit
     the pipes and wedge the capture threads open — the kill happens *before*
     joining the capture threads. Process-group kill shells out to `kill -TERM
     -pgid` (no libc dep). Tests stub `clanky` on `PATH`
     (`crates/clanky-subagents/tests/subagents.rs`); design in
     `parallel-subagents-plan.md`; user docs in README ("Parallel
     subagents"). Shipped with `contrib/skills/parallel-subagents/SKILL.md`,
     an agent-facing skill (when to fan out, how to call the runner, how to
     read the result) for users to copy into a skills scope.

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
