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
  As built (timeout kill fix): a command run with `timeout_seconds` is
  spawned in its own process group; on expiry the whole group gets TERM
  (then KILL after a short grace). Killing only the direct `bash -c`
  child left grandchildren alive — they inherit the output pipes, so the
  tool's reader joins blocked forever (a `cargo test … | tail` run wedged
  for 90+ minutes). The reader joins are also deadline-aware (2 s grace
  past the deadline, then TERM+KILL and return what was captured): a
  grandchild that daemonized out of the group (`setsid`) survives the
  group kill and would otherwise wedge the join forever. A command
  without a timeout stays in Clanky's process group, so terminal ctrl+c
  keeps reaching it (the group technique is the same as the subagents
  launcher, M12).
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

### ✔ M12 — Refinements, Bug Fixes
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
7. [x] `--thinking off` did not disable thinking on reasoning models
   - Bug (hit via DeepInfra): `parse_thinking` mapped `off` to `Ok(None)`, so the
     `chat` wire message omitted `thinking` entirely — indistinguishable from "unset".
     An omitted field falls back to the backend default, and DeepInfra's documented
     default for reasoning models *enables* the reasoning trace, so `--thinking off`
     still showed thinking. (`--thinking 0` — an explicit zero budget — worked, because
     every adapter already mapped `budgetTokens: 0` onto its backend's "off" knob.)
   - Fix: `off` (and an empty value) now parse to `Thinking { budgetTokens: 0 }`, which
     reaches the provider as an explicit zero budget. All three adapters already handle
     it (DeepInfra/LiteLLM → `reasoning_effort: "none"`; Anthropic omits the `thinking`
     block, which is that API's "off"), so the change lives in core only. TUI status
     line still hides the thinking chip for `off`/`0`; `/thinking off` keeps `off` in
     `launch.thinking` (a rejected value no longer clears the previous setting, which is
     the correct behavior anyway). Protocol spec §5 now documents that
     `budgetTokens: 0` means "explicitly disabled" and an absent `thinking` means
     "unset, backend default applies"; README documents the semantics in the
     `settings.toml` notes and a new DeepInfra section, and corrects the LiteLLM section
     (which claimed `off` omitted the field). Tests:
     `thinking_off_reaches_the_wire_as_an_explicit_zero_budget` (core),
     `thinking_off_sends_reasoning_effort_none` (DeepInfra).
8. [x] DeepInfra thinking → `reasoning_effort` ladder extended
   - The chat-completions API reference documents only none/low/medium/high, but
     the per-model docs in DeepInfra's model browser (validated against the live
     API) expose six effort levels plus `none`:
     `none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`.
   - As built: the ladder in `reasoning_effort` (`deepinfra/provider.rs`) is now
     0→`none`, <500→`minimal`, <1682→`low`, <5661→`medium`, <19051→`high`,
     <64107→`xhigh`, else `max` (boundaries as observed). A `reasoning_effort`
     set in `[sampling]` still wins over the derived value. README's DeepInfra
     section documents the ladder. Tests: `thinking_budget_maps_to_reasoning_effort`
     and `thinking_fills_reasoning_effort_only_when_unset` updated to the new rungs.

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

## ✔ M14 — Unified sessions: print mode records, `--no-session`, `--resume` everywhere
Session recording has so far been a TUI feature only (it was specified in M4
when the pipe path from M1 was already frozen), so `clanky -p` runs left no
transcript and could not resume. The turn layer was always shared — both front
ends drive the same `run_turn_with` and consume the same `TurnEvent` stream —
so this milestone moves the event→record mapping out of the TUI and gives both
front ends the same session behavior. A run of `-p` then leaves exactly the
JSONL a TUI run would, which in particular makes subagent (`clanky -pq`) work
inspectable afterward.

Principles:
- **One recorder, two front ends.** The `TurnEvent`→`session::Record` mapping
  (round consolidation, delta accumulation, partial flush on cut-short turns,
  tool results, usage) lives once, next to the session code (`src/session.rs`
  or a sibling); the TUI and pipe mode both feed it their turn events. The TUI
  keeps only its presentation concerns (error surfacing as transcript entries,
  `/name` display, `/md` snapshot).
- **Same features, same flags, both modes.** Sessions are recorded by default
  in print mode too. `--no-session` (CLI flag + `session = false` in
  `settings.toml`, precedence as always: CLI > project > user) disables
  recording in **both** modes — the TUI is not forced to save, print mode is
  not forced to lose. `--resume NAME` works in both modes: the TUI keeps its
  picker (`--resume` with no NAME), print mode requires a NAME (no picker in a
  pipe). With `--no-session`, `--resume` is rejected in both modes (nothing to
  load into, nothing would be saved) — one shared error message, not two.
  A resumed print-mode turn appends to the resumed file (reopen semantics),
  exactly like a resumed TUI turn.
- **User values pass through untouched.** The resume NAME is validated by the
  same `session::validate_name` used everywhere; no new local validation beyond
  "a session file with that name must exist" (which `--resume` already errors
  on in the TUI).
- **`-q` stays print-mode-only** (it is an output filter, not a session
  feature); sessions are orthogonal to it. `--no-session` is the orthogonal
  on/off, usable everywhere.
- **No behavior change when nothing is set.** Default remains "record in both
  modes"; existing TUI sessions, `/name`, `/resume`, `/md` behave exactly as
  before.

Tasks:
1. [x] Extract the session recorder: move `record_turn_event` /
   `flush_partial` / partial-accumulator logic out of `tui/mod.rs` into a
   shared module, decoupled from `&mut app::App` (error surfacing becomes a
   callback or return value). The TUI delegates to it unchanged.
2. [x] Print mode records by default: `run_pipe` creates a `SessionWriter`
   (default name from `session::default_name`, provider/model stamped in the
   header like the TUI) and feeds its turn events through the shared recorder.
   A print-mode turn cut short by an error or ctrl+c gets the same
   consolidated partial flush the TUI has. Pipe mode today has no SIGINT handling
   — the default disposition just kills the process. To flush on ctrl+c you need
   a signal handler (signal-hook is a new dependency; no unsafe needed in our
   code, so the workspace ban holds).
3. [x] `--no-session` flag + `session` settings field, overlaying through the
   existing precedence machinery. In the TUI, recording off means no session
   file, no `/name` target (the command reports it politely), and `/md`
   exports what the screen showed (fresh header, screen-derived records — the
   existing no-file path). In pipe mode it means no file is created. The
   status line replaces the session name with "(no session)" when there is no session file.
4. [x] `--resume NAME` in print mode: load the named session
   (`session::load`), seed `messages` via `history_from_records`, print what
   is being continued (a one-line note on stderr, keep stdout clean), run the
   turn, and append records to the same file (reopen). Remove the
   `ResumeNonInteractive` error. A TUI `--resume NAME` with no such session
   and a pipe-mode one must fail with the same message.
5. [x] README: settings table gets `session`; print-mode section documents
   that sessions are saved by default, `--no-session`, and `--resume NAME`;
   session-precedence note updated (both modes record). Subagents section
   notes that `clanky -pq` jobs now leave session files unless the run
   description passes a `--no-session` override (and that the runner's
   allow-list needs it added, so subagents can opt out per job).
6. [x] Tests: golden/pipe test that a `-p` run writes a session file with the
   same records the TUI would write for the same transcript; partial flush on
   mid-stream error in pipe mode; `--no-session` writes nothing in both
   modes; `--resume NAME` in pipe mode continues a session (records appended,
   context restored); shared-error test for `--resume` + `--no-session`.

Done when: `clanky -p "count files in /tmp"` leaves a resumable session file;
`clanky -pq --no-session` leaves none; a TUI session resumed with `clanky -p
--resume NAME` continues with full context and appends to the same file — and
the TUI's session behavior is unchanged.

As built notes:
- The shared recorder is `crates/clanky/src/recorder.rs`. Its `ErrorSink`
  trait is the "error surfacing becomes a callback" mechanism: the TUI
  pushes failures onto the transcript (`TranscriptErrors`), print mode
  prints them to stderr (`StderrErrors`), keeping stdout clean.
- Every session write reports its own failure through the front end's
  sink — no internal silent sink. A persistently broken session file
  therefore floods one error per failed write (deliberate: if writing to
  disk is failing, the user should see it loudly).
- Ctrl+c in print mode (task 2): two `AtomicBool` flags registered with
  signal-hook. The first SIGINT sets `interrupted`, which wakes a listener
  thread that kills the (possibly SIGINT-ignoring) plugin — the turn then
  fails and `record_done` runs the same partial flush an error gets,
  after which the thread arms the shutdown flag. A second SIGINT hits
  `register_conditional_shutdown` keyed on that armed flag and exits
  immediately, so a hung plugin can never hang the exit. The armed flag
  must be separate from `interrupted`: both flags run as actions in the
  signal handler in registration order, so keying the conditional
  shutdown on `interrupted` itself would make the *first* ctrl+c exit
  instantly with no flush (this was found and fixed in review).

- Task 3 (`--no-session`): the `session = false` setting and the CLI flag
  land in one `Settings::session: Option<bool>` field (`--no-session`
  overlays `Some(false)`; unset means on, resolved via
  `Settings::record_sessions`). The shared recorder gained a
  `.recording(bool)` builder: with it off, every record call is a no-op
  (no file is ever created, `reopen` is refused so a stray resume cannot
  restart writing), `/name` reports "sessions are off (--no-session)", and
  the TUI status line shows "(no session)" — while recording stays on,
  the display is unchanged (a fresh session shows nothing until its file
  exists). `/resume` in the TUI reports the same message when recording
  is off. `--resume` together with recording off (flag or setting) is
  rejected up front in `main::run` with one `ResumeWithNoSession` error
  for both front ends. `/md` with no session file now derives its records
  from the transcript entries (`app.screen_records`): UI-only entries are
  skipped and consecutive assistant/tool-call entries merge into one
  `Assistant` record, so recording a conversation off still exports what
  the screen showed. The subagent runner's option allow-list gained
  `no_session` (boolean, defaults-aware) passing `--no-session`.
- Task 5 (README): the README already carried all four required updates
  from the tasks 3-4 work; task 5 verified every claim against the built
  binaries (settings `session` row, session-precedence note covering both
  modes, print-mode section documenting default recording / `--no-session`
  / `--resume NAME` with the stderr note and no picker, and the subagents
  section documenting `no_session` per job and in `defaults`). No edits
  were needed.
- Task 4 (`--resume` everywhere): the "find a session by name" logic moved
  out of the TUI into `session::find_by_name` (same `validate_name`, same
  `SessionNotFound` error for both front ends — a pipe just has no picker,
  so it requires a NAME). Print mode loads the session before building the
  turn context: fresh system messages, then the records' rebuilt history,
  then the new user prompt; the stderr note (`resuming <name> (N records)`)
  keeps stdout clean, and `recorder.reopen(path)` makes the turn's records
  append to the resumed file exactly as the TUI does. `ResumeNonInteractive`
  is gone. The TUI's `load_session` took the path/name instead of a
  `SessionInfo` (that type stays for the `/resume` picker).

## ☑ M15 — TUI markdown tables: box borders, fitted widths, wrapped cells
The M3 table renderer draws tables as plain `|`-and-`-` markdown source rows
at natural width, so the generic word-wrapper reflows them afterwards — in a
narrow terminal a wide table becomes an unreadable tangle of border fragments
interleaved with cell content. Rework `render_table` to draw real tables:
box-drawing borders (`┌─┬┐`/`│`/`├─┼┤`/`└─┴┘`), column widths fitted to the
viewport (per-column word floors, proportional slack distribution when the
natural table overflows), and cells word-wrapped to their column. Inter-row
rules are always drawn (no opt-in). The width-fitting core is lifted nearly
verbatim from the author's other ratatui program
(`ptm-project-todo-manager`, MIT); the seam change is that
`markdown::render` gains a `width` parameter and emits tables already
fitted and wrapped, so the later generic `wrap()` pass stays a no-op on
table rows (the invariant the scroll math rides on). Plan, copied-vs-adapted
breakdown, and test plan: `tui-table-rendering-plan.md`.
- Done when: a wide table renders as a bordered box that fits the
  transcript width, with words intact inside their cells.


As built notes:
- `render_table` is a near-verbatim lift of ptm-project-todo-manager's
  column-sizing core (word floors, natural pass, proportional slack
  distribution / proportional floor-scaling on overflow), with the indent
  term dropped. Cells are Clanky's styled spans: rendering, wrapping
  (`wrap_chars`), alignment padding, and style plumbing all stay on the
  existing machinery — no port of ptm's `word_wrap_cell`, and one wrapping
  code path for the module.
- ptm's opt-in inter-row separator feature (repeated delimiter row +
  lighter body-separator style) is not ported; Clanky draws `├─┼┤` after
  every row except the last, in the border color. A rule lands after the
  *logical* row: a row whose cells wrap to several physical lines gets
  exactly one rule after its last line.
- ptm's one-span-per-cell emit (`" {cell} "` as one string) became
  explicit pad/content spans so per-char styles survive: a bold or
  code-styled cell never repaints the column padding around it, and a
  cell split across physical rows keeps its style on every line.
- Degenerate widths behave per plan: when even 1-char columns can't fit
  (`avail` clamped to `col_count`), the box still renders at w=1 and the
  generic `wrap()` splits the over-long rows; `print_line` truncates.
  The table tests pin both the degradation path and the no-panic
  invariant; nothing in the code special-cases it beyond the clamp.
- `markdown::render(text, base, width)` gained the width parameter; the
  two `app.rs` call sites (`Entry::Assistant`, `Entry::Thinking`) pass
  their `entry_lines` width. Tables therefore come out of `render`
  already fitted and wrapped (every row ≤ width), and the generic
  `wrap()` pass after is a no-op on them — pinned by
  `assistant_table_fits_width_without_rewrapping` in `app.rs` (each box
  row starts with a border char and fits, so no re-split happened).
- Streaming/resize behave as planned: a table appears as plain text
  until the delimiter row arrives, then re-draws as a growing box each
  frame; a committed table keeps its fit-time width after a resize
  (scrollback is never rewritten).
- README "Rendering notes" gained a bullet on the box borders, fitting,
  and wrapped cells; the plan doc `tui-table-rendering-plan.md` is left
  as the record of the port. Golden fixtures contain no tables; the
  golden suite passes unchanged.

## ☑ M16 — TUI input: multi-line caret movement, ctrl+a/e/j, paste-safe Enter

The TUI input rendered multi-line text (paste, `$EDITOR`, templates) but the
caret could not travel between lines: ↑/↓ always recalled history, so editing
a pasted block row-by-row was impossible, and a pasted newline submitted the
prompt mid-text (raw Enter became the pasted newline through the terminal's
input stream). The input is now a real multi-line editor.

Tasks:
1. [x] `↑`/`↓` move between the physical rows of the buffer, keeping the
   caret's column (clamped to the row's width). Only from the top row does
   `↑` recall the previous prompt (draft semantics unchanged), and only
   from the bottom row does `↓` step forward — on a single-row buffer both
   behave exactly as before.
2. [x] ctrl+a / ctrl+e move to the start/end of the current input line.
   ctrl+e keeps its readline double duty: at the end of the buffer it opens
   `$EDITOR` (the `ctrl+e` documented since M7), mid-buffer it is
   line-end-first.
3. [x] A pasted newline cannot submit: bracketed paste is enabled for the
   TUI's lifetime (a drop guard re-disables it even when an error unwinds
   through `run`), and `Event::Paste` inserts its text whole at the caret.
   Carriage returns are stripped (`\r\n` paste endings; a stray `\r` would
   only move the cursor invisibly). Only a physically pressed Enter submits.
4. [x] ctrl+j inserts a newline at the caret.
5. [x] README keys table and rendering notes updated.

Done when: typing a multi-line buffer with ctrl+j, ↑/↓ walk its rows; `↑` at
the top still recalls history; pasting a multi-line block inserts it whole
without submitting.

As built notes:
- The physical-row model mirrors the renderer's (`screen.rs` `input_rows`):
  rows are the segments between newlines, a caret sitting on a newline
  belongs to the end of the row above, and a caret at the buffer's end
  lands on the (possibly empty) final row. `App::caret_row` counts
  newlines strictly before the caret, which reproduces both rules.
- The ↑/↓ dispatch lives in `handle_key` via the `usize` return of
  `App::cursor_up`/`cursor_down` (rows moved): `0` means the caret cannot
  move further and the caller may touch the history.
- Bracketed paste is a crossterm feature of the re-exported
  `ratatui::crossterm` (no new dependency); the guard also covers the
  `$EDITOR` suspension, where raw mode is dropped but the paste mode is
  left on — the guard's drop fires on every exit path through `run`.
- Paste is inserted with `App::insert_str` (chars, `\r` filtered), not
  per-key events, so a large paste is one edit and one redraw.

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

## ✔ M17 — Config knobs & CLI additions
Four small, independent additions that all ride the existing settings-precedence
machinery (user file < project file < CLI) or the `clap` surface. Grouped because
each alone would be a one-line milestone; they should land **before** M18 so the
settings dump (M18) covers them from day one.

1. Tool-call timeout, settings + CLI + `/timeout`:
   - The per-tool default deadline moves from `bash`-specific to generic: the
     `Tool` layer gets a configurable default timeout applied to any tool call
     that does not specify `timeout_seconds`. Default remains unset =
     unbounded (unchanged behavior); unset is invisible to the tool, exactly
     as today.
   - The timeout is enforced at the `Tool` layer, and what expiry *means*
     depends on whether the tool can be cancelled:
     - **Killable tools** delegate their work to a child process (today only
       `bash`). Expiry is real cancellation: the child's process group gets
       TERM/KILL, the call ends because the work ended. This is today's
       behavior, unchanged.
     - **In-process tools** (planned `edit`, any future non-spawning built-in)
       have no child to kill; expiry can only mean abandonment: run the call,
       on deadline either surface the timeout error and possibly leave the
       work finishing on a detached strand, or (better) don't enforce at all
       when the tool can genuinely block on no recoverable path. The
       distinction is a per-tool property: in-process tools declare whether
       the default deadline applies to them. `edit`-class tools — plain
       buffered file IO that finishes in milliseconds or fails — do not; the
       default deadline is for killable tools plus any future tool where a
       hang is plausible and abandonment is safe (e.g. a network-fetch tool).
       Rationale: an abandonment timeout on a non-idempotent in-process tool
       lies to the model — the tool-timeout error invites a retry while the
       abandoned work may still complete, so `edit` could double-apply.
   - `timeout_seconds` setting (`[settings] timeout_seconds`), `--timeout-seconds
     N` CLI flag, and `/timeout <seconds|off>` slash command (off = clear back to
     unbounded, i.e. unset — the per-call field still wins over the configured
     default). Precedence: CLI > project > user, `/timeout` changes the live
     value.
   - An expired killable call surfaces as today's tool-timeout error (group
     TERM/KILL). Tools that opt out of the default deadline are unaffected by
     it entirely.
2. `--project` CLI option: overlays the existing `project` display label
   (same per-field overlay as all settings; label shown at the input prompt and
   (from M18) in the settings dump).
3. `--name NAME` CLI option: starts the session named (`default_name()` skipped
   in both TUI and print mode). Validated by the same `validate_name`; `--name`
   together with `--resume` is a usage error (one shared message for both modes).
   `/name` still works mid-session.
4. `--help` text updated as items land.

Done when: `--timeout-seconds 5 --name fix --project clanky clanky -p …` runs, a
long command is killed at 5 s (in TUI too, via `/timeout`), precedence CLI >
project > user verified, `--name` + `--resume` shared error, README updated.

As built notes:
- The generic deadline lives in `tools::default_tools(timeout_seconds)`: the
  `Tool` layer builds the set from the merged setting each turn (print mode in
  `run_pipe`, TUI in `spawn_turn`), so a `/timeout` edit takes effect from the
  next call without rebuilding anything. Unset goes through as the
  `UNSET_DEADLINE` sentinel (≈`u64::MAX/2` seconds): the deadline machinery
  runs unconditionally (a command always spawns in its own process group,
  never only when a deadline exists), but a deadline past `Instant`'s range
  never fires, so "unbounded" behaves exactly as before. Per-call `timeout_seconds` wins over the configured default in
  both directions; a per-call value is still validated the same way (0
  rejected, non-integer rejected).
- The M17 design's killable-vs-in-process distinction is not a code path yet:
  only `bash` exists today and it is killable, so the enforcement is plain
  TERM/KILL cancellation, unchanged from M2. `edit`-class tools (M19) will
  opt out by simply never consulting the default deadline (plain buffered IO
  has no recoverable hang to abandon). (M19 did exactly this — see its
  as-built notes; `edit` never reads a timeout.)
- `/timeout <seconds|off>` parses `off`/positive-integers/`show` (no arg) and
  rejects 0 and non-numeric values; the TUI palette gained a `/timeout ` entry.
  The live edit happens on `Launch.timeout_seconds` — a struct field like its
  siblings, never written back to a settings file (M18's settings *dump* will
  still observe `/timeout` changes via records; that work is M18). Note
  the process-group spawn became unconditional: with the sentinel deadline it
  changes nothing observable — an unbounded command is never killed and
  ctrl+c still reaches the child because the terminal keeps delivering to
  the foreground process group.
- `--name`: a new `Recorder::with_name(name, errors)` builder sets the pending
  name before any record (validation errors go through the front end's usual
  `ErrorSink`), so print mode and TUI share one code path; `--name` +
  `--resume` resolves to the shared `Error::NameWithResume` in `run` before
  either front end starts.
- `--timeout-seconds` is `u64` (clap's own parse error on non-numeric input —
  exit 2), `--project` maps onto the existing `project` setting's overlay
  field (`cli.overrides()`), `--name` stays out of `Settings` because it is a
  session concern, not a setting. The one subtlety with the trailing-var-arg
  prompt surface: flags must precede the positional PROMPT arguments (a flag
  after the first free word is swallowed by the prompt) — noted in the CLI
  test.

## ✔ M18 — Settings observability: startup dump + settings records in the log
The merged configuration is currently observable only by reading the status line
(provider · model) and guessing at the rest. Two coupled pieces:

1. **Startup INFO dump** (both TUI and print mode): model, provider, and the
   merged settings, printed at startup. Print mode writes it to stderr (stdout
   stays the response/quiet channel). Plain merged values — no per-field
   source-layer attribution. Unset fields elided.
2. **A dedicated `Settings` record type** in the session file, carrying model +
   provider (migrated out of the header) plus the merged settings, written:
   - at session start (before the first user record), and
   - in the order the changes happen, whenever settings change mid-session
     (`/model`, `/thinking`, `/sampling`, `/timeout` — M17 adds the last). The
     record type is versioned like every record; format-version notes updated.
   - **On `/resume` (M14 semantics, both modes): the records never *set* values.**
     The session's settings records are read and folded in order (later
     records win per field, exactly as they were written) to reconstruct the
     settings state as of the end of the resumed session; that folded state is
     compared against the currently active settings and a change record is
     written for the differences. From the resumed session's perspective the
     settings therefore appear to have changed mid-session, in order.
3. Header migration: provider/model move to settings records; loading an
   old file keeps them working (header still parsed and used if no settings
   record exists — the existing forward-compatible-skip rule does not apply
   to the header, which is versioned explicitly). With the never-rewritten
   fields gone, the header becomes write-once (`version`, `created` only):
   `Header::stop_reason` and `SessionWriter::set_stop_reason` are removed —
   dead code, never called outside `session.rs` tests (the per-turn
   `StopReason` record appended by the recorder carries the data; the
   `/resume` picker never read the header field). `Mode::Reopen` stops
   reading the header line back (it existed only to preserve `stop_reason`
   when rewriting). Old files load fine: its absence in real files is why
   it was spotted here.

Done when: `clanky -p "hi"` shows the INFO dump on stderr; the session file's
first record is a settings record with model/provider (header no longer carries
them, old files still resume); a `/model` change writes a settings record; a
`/resume` of a session whose settings differed writes a change record.
As built notes:
- The session dump type is `settings::SessionSettings` (provider, model,
  thinking, sampling, max_tool_rounds, max_retries, project,
  timeout_seconds): every `Settings` field except `prompt` (it becomes a
  user record, not a configuration fact) and `session` (a recording
  switch). `describe()` renders one line, unset fields elided
  (`all defaults` when nothing is set); `describe_sampling` moved from
  the TUI into `settings.rs`, shared by the dump and the `/sampling`
  messages.
- The record wire shape is `session::SessionSettingsRecord`
  (`camelCase`, unset fields elided) wrapping `SessionSettings`;
  `session::settings_record(&SessionSettings)` builds it and
  `session::fold_settings(&SessionData)` folds a file's records in order
  (later records win per field, `[sampling]` per key), falling back to
  the legacy header provider/model when the file has no settings record.
- The recorder owns the state. `Recorder::new(SessionSettings)` replaces
  the provider/model pair; the dump is written lazily with the *first*
  `record` call, so the settings record always precedes the first user
  record without creating the file before the first prompt.
  `set_settings(settings)` updates the live state and writes a record
  once the dump is out (pre-start changes fold into it silently â a
  change before the first prompt never creates the file early). The
  front ends snapshot the effective settings at every change
  (`observe_settings` in the TUI derives them from the live `Launch`
  fields, so record trail and live fields cannot drift; print mode
  builds one snapshot up front). Covered changes: `/model`,
  `/provider` (also clears the model until the new handshake resolves
  it), `/thinking`, `/sampling`, `/timeout`, the TUI's adoption of the
  plugin's advertised default model when none was set, and `/resume`
  differences.
- `/resume`: `recorder::reopen(path, folded, errors)` takes the folded
  state and appends one change record (a snapshot of the active
  settings) when it differs from the active settings; equal states write
  nothing. Resuming counts as session start: later changes write
  records. The records never set values (M18 semantics kept).
- Header migration: `Header` is write-once (`version`, `created`);
  `provider`/`model` remain as parse-only legacy fields (old files load,
  and `export::effective_settings` uses them only when no settings
  record exists), `stop_reason` and `SessionWriter::set_stop_reason` are
  removed, and `Mode::Reopen` no longer reads the header line back.
- Startup dump: print mode prints `INFO: settings: â¦` to stderr before
  the resume note; the TUI pushes a `settings: â¦` `Info` entry at
  startup (stderr is the terminal there). The print-mode snapshot fills
  the model from the resolved turn config (the plugin handshake's
  advertised default included), so the dump shows the model a turn will
  actually use.
- Test note: `plugin_pipe.rs` pins `HOME` to its scratch dir, so the
  merged settings a run records are deterministic regardless of the
  machine's `~/.clanky/settings.toml`; the golden `session-records`
  fixture gained the leading settings record (regenerated).


## ✔ M19 — Edit tool
A built-in `edit` tool beside `bash` behind the `Tool` trait; unrestricted paths
(no trust system — bash already writes anywhere; the value of the tool is intent
and failure messages for the model, not a permission boundary).

1. Schema: `path`, `old_string`, `new_string`, optional `replace_all: bool`.
   Semantics: literal byte-for-byte match, no patterns/wildcards:
   - zero matches → error "not found"
   - multiple matches, no `replace_all` → error naming the match count and
     asking for more surrounding context (failures are data: fed back so the
     model can recover)
   - `replace_all` → every occurrence replaced
2. No requirement to read-before-edit. We don't have a read tool. We can think
   about adding such a requirement in the future.
3. Model result: a short confirmation, not file contents and not a diff —
   the tool call in context already carries `old_string`/`new_string`:
   - success: `Replaced N occurrence(s) in <path>` (the count is how the
     model confirms it did what it intended). No separate line-capped
     preview; the display preview is simply this result text, capped by the
     usual `preview_result` like any tool result.
   - failure: the error message as a normal `ToolResult` (recovery
     possible), not a turn error. No separate error path: like a non-zero
     bash exit, failures rise from `is_failed_result`/`ERROR:` prefixing
     as usual and the TUI renders the result text red; standard result
     color on success.
4. Session records unchanged: an Edit is a normal `ToolResult` record.
5. README tool table updated.

Done when: `clanky -p "change the title in README.md to X"` performs a Read +
Edit + write cycle and succeeds; a dissertation on uniqueness/non-match read
back as tool results, golden transcripts updated.

  As built: `EditTool` is a unit struct in `crates/clanky/src/tools.rs`,
  added to `default_tools` beside `bash`. Matching counts and replaces run
  on raw bytes (a small non-overlapping subsequence scan, no regex crate):
  byte-for-byte literal as specified, and a file with invalid UTF-8 beside
  the edited region is editable — the edit is not blocked by encoding
  issues elsewhere in the file (a `sed`/rewriting approach would have been;
  the file is rewritten only with the replaced bytes, everything else
  untouched). Empty `old_string` is rejected with an argument error (an
  empty needle would match "everywhere"); the schema carries no
  `minimum`-style guard for it, the check is in `execute` so the message is
  the tool's own actionable one. Zero matches → "not found: `old_string`
  occurs 0 times in <path> (check the exact text, including whitespace)";
  multiple matches without `replace_all` → "`old_string` occurs N times in
  <path>; provide more surrounding context to make it unique, or pass
  `replace_all: true`"; success → "Replaced N occurrence(s) in <path>" (N
  counted the same way for `replace_all`, so the count is verifiable).
  All failures are `ToolError::Failed` — they rise through the existing
  `ERROR:` prefixing / `is_failed_result` red-rendering path unchanged, no
  new display code. The default deadline does not apply (the M17 design's
  opt-out): the tool never consults a timeout — plain buffered IO that
  finishes in milliseconds or fails has no recoverable hang to abandon, and
  an abandonment timeout on a non-idempotent in-process tool would lie
  (invite a retry while the abandoned work may complete → double-apply).
  No read-before-edit requirement (per the milestone). Tests: unit tests
  for all argument-validation and match-count paths (unique replace,
  zero/multiple matches, `replace_all`, literal-not-regex matching,
  invalid-UTF-8 preservation, missing file, multiline and adjacent
  occurrences) in `tools.rs`, plus a new golden transcript
  (`tests/golden/edit-tool.json`, test `edit_tool_transcript_matches_golden`)
  that pins the wire tool definition and the exact fed-back confirmation
  over an edit + bash-read-back turn; the existing tool goldens were
  regenerated because the default tool set now includes `edit` (its
  definition appears in every transcript). README: new "The `edit` tool"
  section and the `timeout_seconds` bullet now notes the default applies
  only to `bash` ('killable' tools).

## ☐ M20 — View-image (vision) tool
A built-in `view-image` tool that puts an image file into the model's context,
for multimodal models. Scope: **image → model only.** The TUI never displays the
image (tool-result entry shows size like any result); the model never generates
images. Cards on the table:

1. **Protocol:** `ChatMessage` gains an optional,
   additive image list — `Vec<ImagePart> { data_base64, media_type }`, new in a
   minor protocol version (additive in v2; adapters without the field are no
   longer needed since all three are ours). Text content stays the primary
   field; turns continue to send text. A tool result may carry images in
   addition to its text — the turn loop / adapter layer is responsible for
   building the backend request shapes:
   - OpenAI-compatible (DeepInfra, LiteLLM): `image_url` data-URI parts
     (`data:<media>;base64,<data>`)
   - Anthropic: `image` source blocks (decided: inside the `tool_result`
     block, whose `content` may be an array mixing text + image blocks —
     the Messages API permits this). A follow-up `user` message with a
     bare image block was considered and rejected: it injects a synthetic
     user turn no other backend produces, so replay/history reconstruction
     would diverge between providers. If a live probe or doc check shows
     tool_result images need extra headers or are rejected, fall back to
     the follow-up message and record the decision here.

A model that can't accept images 400s; the backend's error is surfaced
unchanged (invariant 2) — no local capability whitelist. A future optional
`supportsImages` hint (LiteLLM-style) could curate the picker, like
`supportsTools` in M10 — not required for this milestone.

2. Tool: `view-image {"path": …}`, description: "Read an image file from disk
   and attach it so you can see it." (Name chosen over `read-image`/`vision`:
   action-shaped verb matching how users phrase the intent — "show me", "take a
   look", "see this screenshot" — while `view_` avoids the computer-science
   connotation of "reading a file".) Reads a local file, base64-encodes it,
   returns a tool result whose text is a size note ("256 kb") and whose images
   list carries the data. Unknown/undetectable media types resolve from the
   filename extension (err for unknowns).
3. **Session records: keep it replayable.** A tool result with images is
   written in full base64 (a screenshot is ~300–400 KB encoded — large lines in
   the JSONL, but the whole session file format rests on "replay reconstructs
   exactly what the model saw"; truncating it violates that with no principled
   cap). Record schema documents the size expectation. `history_from_records`
   rebuilds the image parts.
4. Print mode `-p`: an image result prints only its size note.

Design notes (wire shapes for all three backends, media-type table, session
size caveat) go in a short `design-docs/read-image-plan.md`, matching the
pattern of M10/M11 planning docs.

Done when: `clanky -p "what's in /work/sample.jpg"` against a vision model
returns a description; the session replays and the model answers about the
image again; a text-only model surfaces the backend error unchanged. The user
will handle the live testing.
