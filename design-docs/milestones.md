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

## Open Questions
1. ✔ **Provider plugin protocol details:** exact message shapes, streaming
   granularity, cancellation semantics, how `list-models` interacts with
   `/model` picker. Mostly settled during M1 design.
   ANSWER: See ./provider-protocol.md
2. ✔ **Plugin startup policy:** eager at launch (catches broken plugins early) vs
   lazy on first use (faster startup). Recommend lazy with error surfacing.
   ANSWER: lazy
3. ✔ **Discovery details:** how aggressively to auto-discover sibling binaries
   (`clanky-provider-*` naming convention?) vs requiring settings entries.
   ANSWER: naming convension (refined in M8 planning to PATH-only lookup, no
   sibling scan and no settings entries — see `provider-plugins-plan.md`)
4. ☐ **MCP scope when it arrives:** tools first; prompts/resources later if wanted.

## ✔ M0 — Skeleton
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

## ✔ M3.1 — Session cost survives `/resume`

Reported symptom: the status line's cost was accurate during a session but
read much higher after quitting and resuming the same session.

Root cause: `Record::Usage` persisted only `promptTokens`/`completionTokens`,
and `App::restore` folded a restored round with `cached: 0`. The live session
priced the cache-served share of each prompt at the provider's cache-read
rate; after a resume that share was re-priced at the full input rate. The
factor is large — DeepInfra's current catalog has `cache_read_tokens` at
$0.006/Mtok against `input_tokens` at $0.20/Mtok (~33×) — and a long agentic
session serves most of every re-sent prompt from cache, so the resumed number
could be many times the honest one. Reproduced at $0.00314 → $0.0206 (6.5×)
for one heavily cached round.

Fix: the `usage` record carries `cachedTokens` (`#[serde(default,
skip_serializing_if = "Option::is_none")]`), `session::usage_record` copies it
from the protocol `Usage`, and `App::restore` folds it into `BilledUsage`
(and into the `last_usage` that drives the ctx %). Records written before the
field load unchanged with no cached share, and a record for a backend that
reports none serializes exactly as before — the existing goldens did not
move.

Coverage: `usage_records_round_trip_the_cached_prompt_share`,
`usage_records_without_cached_tokens_still_load`,
`a_usage_record_without_a_cached_share_omits_the_field` (session.rs),
`resume_restores_the_cached_share_so_the_cost_matches_the_live_session`
(app.rs), and `resumed_session_cost_matches_the_cost_before_quitting`
(tui/mod.rs) — the last one asserts the restored status-line cost equals the
pre-quit cost and that it stays below an all-at-input-rate total, so the
cheap rate is still being applied.

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

## ✔ M10 — Polish & release
- DONE - Error messages, logging, docs, packaging (crates.io / release binaries)
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
  As built (revised): `Retry-After` is honored on `backend` errors too
  (spec §8 now allows `retryAfterMs` on `rateLimit` and `backend`), and each
  retry attempt writes a `retry` record to the session file — the file
  previously could not distinguish a failure after five retries from a
  failure on the first.
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
  for 90+ minutes). The group is signalled through `rustix`
  (`kill(-pgid, sig)`; `unsafe` stays inside that crate, and there is no
  external `kill(1)` to depend on), probing with signal 0 first so a
  reaped group is never signalled at a recycled pid. The reader joins are
  also deadline-aware (2 s grace past the deadline, then TERM+KILL and
  return what was captured): a grandchild that daemonized out of the
  group (`setsid`) survives the group kill and would otherwise wedge the
  join forever. A command
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
- DONE - Done when: a stranger can `cargo install` and use it.

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

  Later fix (code review): `pause_turn` no longer collapses into a plain
  stop. The protocol gained a `FinishReason::Continuable` ("turn
  interrupted server-side; resume by sending the partial response back
  and requesting again" — Anthropic's documented `pause_turn` contract),
  the adapter maps it, and the turn loop resumes automatically: the
  partial round's text goes back as a normal assistant message, its
  usage is not billed (the next request re-reports it, cumulatively), and
  the same round cap that bounds tool-call loops bounds a runaway
  continuation. DeepInfra/LiteLLM have no such concept and never emit it.

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

  Later fix (code review): a `stream = false` entry in `sampling` is now
  honored end to end. Previously the request body carried the user's value
  but `chat()` always POSTed via the SSE transport and fed the response
  through the stream parser — a legal non-streaming JSON body parsed as SSE
  errored ("malformed stream event") or produced garbage, so the user's
  explicit value was accepted then functionally discarded. Now
  `provider::chat` derives the same `stream` decision the body does
  (`wants_stream`: any non-`true` value in the open sampling map means
  non-streaming) and routes `stream = false` to the one-shot
  `post_message` path, translating the whole body via
  `adapter::response_to_events` — the same chunks the SSE path emits, so
  the client cannot tell which transport served the turn. `post_message`
  and `response_to_events` are live code again; HTTP-status and in-body
  (`{"type":"error",...}`) failures map through the same error mapping as
  the streaming path.

  Later fixes (code review, together with the M11b `pause_turn` note):
  the Claude Code identity system block is now prepended on the OAuth
  path only — `build_request_body` takes an `adapter::Identity` derived
  from the request's `AuthMode`, matching the header split in
  `apply_auth`; the key path sends the user's system prompts as-is (it is
  the documented plain third-party surface, not a stand-in for real
  Clanky identity). The token-endpoint refresh error mapping matches
  `invalid_grant` exactly (the documented "refresh token expired/rotated"
  code) instead of the bare substring "invalid", so other 400/401 errors
  (e.g. `invalid_client`, which a re-login cannot fix) surface verbatim
  with their status. And the backend parses HTTP-date `Retry-After`
  values the same way the DeepInfra/LiteLLM backends do (delay-seconds or
  IMF-fixdate, negative/past dates clamping to 0).

  Later fix (user report): the OAuth token was never refreshed in a
  long-running session, and a re-`login` did not take effect either.
  `current_credentials` returned `session_credentials` unconditionally and
  *before* any other check, so the token loaded at process start was used
  for the life of the process. Access tokens live ~8 h, so a session open
  longer than that kept sending a token that had expired: the API answered
  `401: OAuth access token has expired`, and since nothing cleared the
  cache, every later turn failed the same way — and re-running `login`
  (which writes fresh credentials to disk) changed nothing, because the
  stale in-process token still short-circuited. The `README` promise
  ("re-running `login` works while Clanky is up") only ever held on the
  *cached failure* path, where a refresh had already failed; a token that
  merely expired hit the silent-reuse path instead.

  Fix, in two parts. (1) Resolution drops an **expired** in-process token
  instead of returning it, falling through to the store, which refreshes —
  so auto-refresh now works mid-session, not just at startup. (2) A 401
  from the API is taken as authoritative about the token: a local
  `expires_at` check cannot see a revoked token or one expired server-side
  early, so `with_auth_mode` reports the rejected token back to resolution,
  which refuses to hand it out again (even if the clock says it is live)
  and prefers a re-`login`'d store token or a refresh, retrying the request
  exactly once. A 401 is rejected before any generation, so the retry
  cannot double-bill or replay a partial stream. The API-key path never
  retries — a bad key is not fixed by re-resolution, and the user's key is
  authoritative (project invariant), so the server's message is surfaced
  as-is.

  Chat resolves auth once and passes the mode to both the body builder (the
  Claude Code identity is derived from it) and the request, so the body and
  the wire request cannot come from different credentials; a 401 retry can
  only stay on the bearer path, so a body built for the bearer identity is
  still correct. Coverage: `an_expired_in_process_token_is_refreshed_instead_of_reused`,
  `a_401_on_a_locally_live_token_triggers_a_refresh`,
  `a_401_is_recovered_by_the_credentials_a_live_login_wrote`,
  `a_rejected_api_key_is_reported_without_a_retry` — the first three fail
  against the old code. Test plumbing: the `oauth::cache_test_hooks` canned
  response became thread-local (it was one global slot, so parallel tests
  could arm/disarm each other's refresh — the pre-existing re-login test
  was already racy against any new cache test), and the mock backend can
  now reject named bearer tokens to model the server's 401.

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
       *(Revised: retry notices are diagnostics, not activity — they now reach stderr
       in `-q` as well, and every retry attempt is recorded in the session file as a
       `retry` record. A quiet turn that died after five silent retries was
       undiagnosable.)*
       Note: the TUI rejection was documented here but not actually implemented until the
       front-end choice was extracted into `select_front_end` (main.rs) — the flag was
       accepted and silently ignored, leaving `QuietRequiresPrint` as dead code. The
       redirected-stdin/stdout fallback counts as print mode, so `clanky -q < prompt`
       remains valid without `-p`.
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
     joining the capture threads. Process-group kill goes through `rustix`
     (`kill(-pgid, sig)`, `unsafe` confined to that crate; no external
     `kill(1)` to depend on), probing the group with signal 0 first so a
     reaped group is never signalled at a recycled pid. The capture-thread
     joins are bounded by a shared grace so a grandchild that escaped the
     group cannot run a job past `timeout_secs`. Tests stub `clanky` on `PATH`
     (`crates/clanky-subagents/tests/subagents.rs`); design in
     `parallel-subagents-plan.md`; user docs in README ("Parallel
     subagents"). Shipped with `contrib/skills/parallel-subagents/SKILL.md`,
     an agent-facing skill (when to fan out, how to call the runner, how to
     read the result) for users to copy into a skills scope.
   - Fix (signal deaths reported as no `exit`): `run_job` called
     `child.wait()` a second time *after* the wait that reaped the child, so
     the status it read back was an error and the code came out `None`. A job
     the user kills (or the OOM killer, or a crashed plugin) therefore
     reported no `exit` field at all — indistinguishable from "still
     running", and an `exit != 0` check passed on it. The wait's
     `ExitStatus` is now carried in `WaitOutcome::Done` and reported once.
     Since a signalled child has no exit code, `exit_fields` maps it to
     `128 + N` (shell convention: 137 for SIGKILL, 143 for SIGTERM) and
     appends `killed by signal N (Name)` to that job's `stderr` — the number
     reads as failure, the note says why.
   - Fix (a finished job could hang the batch): the capture joins were
     unbounded off the timeout path (`Duration::MAX`), on the assumption that
     a lingering writer was the job's own doing. But the direct child exiting
     does not close the pipes — a helper it left behind (`sleep 60 &`) holds
     them — so a job that had already finished, or been killed, could wedge
     the whole batch forever (reproduced with a killed job: the runner never
     printed results). `join_captures_bounded` now applies a shared grace on
     every path, then signals the job's group (TERM, then KILL after a short
     flush) so the readers hit EOF and their bytes are kept; only a writer
     that escaped the group (`setsid`) is detached, and its stream is then
     reported as lost rather than as empty. Mirrors the bash tool's
     `join_with_deadline`.

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

## ✔ M15 — TUI markdown tables: box borders, fitted widths, wrapped cells
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

## ✔ M16 — TUI input: multi-line caret movement, ctrl+a/e/j, paste-safe Enter

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
  <path> (lines 42, 57, 88); provide more surrounding context to make it
  unique, or pass `replace_all: true`"; success → "Replaced N occurrence(s)
  in <path> (lines 42-44)" (N counted the same way for `replace_all`, so the
  count is verifiable).
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

  **Revised after use** (observed sessions showed ~6,400 `bash` calls to 3
  `edit` calls; 43 of 51 `python3` heredocs were reimplementing the tool,
  22 of them single-file/single-replace — i.e. exactly `edit`'s shape, and
  only one `edit` failure ever occurred, so models were not bouncing off
  the tool but never reaching for it):

  1. *Description.* It advertised two failure modes and no benefit, which
     reads as "this thing fails a lot" next to `bash`'s unconstrained one
     line. It now names the preference ("prefer this over shell commands
     (`sed -i`, `python3` heredocs) for changing files"), says why (match
     count and line numbers, fails loudly), and says several calls in one
     round are fine (the loop has always supported that — 137 recorded
     rounds carried more than one call).
  2. *Success carries line numbers.* `Replaced N occurrence(s) in <path>
     (lines 42-44)`. This keeps the M19 "no contents, no diff" decision —
     a diff repeats strings the conversation already has — while adding
     the one fact it does not: where the change landed. Spans are computed
     in the *new* file (post-edit state, what the next read sees) by
     shifting each match offset by the accumulated length delta; several
     matches report distinct start lines, deduped, capped at 5 then
     `+N more`, and never collapsed into a range (two matches on adjacent
     lines are two locations, not one span). A deletion is zero-width and
     reports the single line it emptied. The end clamps to the file's last
     existing line (`last_line_number`): deleting the final line leaves the
     offset one past the last `\n`, and naming a line the file no longer has
     would contradict "the state after the edit", which is the point.
  3. *Zero-match diagnostic.* A whitespace-insensitive second pass (all
     ASCII whitespace runs → one space, both ends trimmed, with a parallel
     origin-offset map) locates a near miss and the error names the line
     *and quotes the file's own bytes there* — the model cannot rebuild the
     right `old_string` from a line number without re-reading the file, and
     re-reading with bash is the habit being fixed. The pass is
     **diagnostic only**: it never applies a match, so the byte-for-byte
     guarantee and M19's "literal, no patterns" decision both still hold
     (auto-applying a fuzzy match would edit text the model did not
     specify, and can land in the wrong place). A whitespace-only
     `old_string` normalizes to nothing and is skipped, so it cannot
     "match" at offset 0. Invalid UTF-8 is fine — it all works on bytes and
     renders with `from_utf8_lossy`.
  4. *Multiple-match error names the lines* (`(lines 42, 57, 88)`), which
     is what makes "add surrounding context" actionable.
  5. *Scan.* `count_subsequence`/`replace_subsequence` became
     `match_offsets`/`replace_at`: one scan yields both the count and the
     offsets the line reporting needs.
  6. Tests: the exact-string unit assertions moved to the new shapes, plus
     new cases for spans (single line, span, deletion), `replace_all`
     start-line lists and same-line dedupe, the multi-match line list, the
     whitespace diagnostic (tabs, trailing whitespace, CRLF) including
     that the file is untouched, the no-near-miss plain message, the
     whitespace-only guard, and the `line_of`/`line_span`/`describe_*`
     helpers. The `edit-tool` golden was regenerated (description + line
     number).

## ✔ M20 — View-image (vision) tool
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

  As built (live testing against a vision model still pending — the user
  runs it): the protocol change is additive in v2, no version bump —
  `ChatMessage::Tool` gains an optional `images`
  (`Vec<ImagePart { dataBase64, mediaType }>`), **absent when there are
  none**, so every existing message, session record, and golden stays
  byte-identical. `mediaType` is a closed enum (png/jpeg/gif/webp/heic/
  heif/avif, IANA `image/…` strings), not a free string. The Anthropic
  decision stands as the milestone drafted it: images ride **inside the
  `tool_result` block** (its `content` is an array mixing text + image
  blocks); OpenAI-compatible backends put `image_url` data-URI parts in a
  trailing `user` message directly after the `tool` message — **amended
  after live testing**: array `content` on a `tool` message is a 422
  (`messages.N.tool.content.str: Input should be a valid string`), so the
  `tool` message keeps its plain-string text and the images ride in the
  trailing user turn (deterministically ordered, emitted only when
  images are present, so image-free requests stay byte-identical). Media-type resolution is sniff-first (JPEG/PNG/GIF/WEBP magic
  bytes; the container formats HEIC/HEIF/AVIF share ISO-BMFF boxes with
  video formats so a byte sniff deliberately declines them), extension
  fallback (case-insensitive, `.jpeg` aliased), sniff wins over a wrong
  extension; unrecognizable + unknown-extension files fail with a data
  error ("unknown image media type"). The `Tool` trait's `execute` returns
  `ToolOutput { text, images }` (a `From<String>` keeps plain tools
  unchanged); the turn loop attaches non-empty image lists via
  `ChatMessage::tool_with_images`; `TurnEvent::ToolResult` carries them to
  the recorder (full base64 in the `toolResult` record — replayable) and
  the transcript (size note only, never displayed); `/md` exports an
  `_attached image_` note instead of the payload. Size note is SI:
  `623 bytes` / `256 kb` / `1.2 mb`. Design notes and wire shapes:
  `design-docs/view-image-plan.md`. Tests: protocol round-trip + legacy
  parse, per-provider wire shapes (image-less results keep plain-string
  content), tool unit tests, golden transcript
  `tests/golden/view-image-tool.json`, and an integration test that an
  image-carrying turn survives save/reload with the image re-attached
  (`tests/sessions.rs`).

Later fixes (code review, post-M20):
- **Bash tool: a backgrounded grandchild could wedge a call with *no* deadline**
  (`crates/clanky/src/tools.rs`). With the default (unset) deadline the reader
  joins took the plain-`join()` branch, so `sleep 30 & echo ok` — the direct
  child exits, a grandchild holds the stdout pipe — hung the tool call
  forever: the process-group kill machinery was only armed when a deadline
  existed. Now the reader-join loop always carries its own `JOIN_GRACE`
  grace window independent of the deadline, ending in a TERM/KILL of the
  group plus a short `KILL_FLUSH` pass that lets the readers observe EOF and
  hand over what they captured (so the output is not discarded — without
  that flush `sleep 30 & echo ok` returned "(no output)"). The
  `join_with_deadline` signature drops the deadline parameter; the wait
  loop's deadline logic is unchanged.
- **Process transport: a malformed line from a live plugin is skipped, not
  fatal** (`crates/clanky-protocol/src/process.rs`). The client-side reader
  used to surface a parse `Err` as a terminal item in the response stream,
  aborting the in-flight turn — asymmetric with `serve`, where the *plugin*
  side ignores malformed lines per spec §9. Now the client reader ignores
  them too (`Err(_) => {}`), matching the unknown-type handling. Framing was
  never actually at risk (the reader is line-buffered), but one glitchy
  line should not kill the session. Test:
  `a_malformed_line_from_a_live_plugin_is_skipped_not_fatal`.
- **`is_failed_result` no longer misreads a quoted marker** (`crates/clanky/
  src/tools.rs`). It matched `output.contains("[exit status: ")`, so any
  successful tool whose output merely *mentioned* the marker (a `cat` of a
  session transcript, a review quoting it) was classified as failed. It now
  checks only that the output's final line starts with `[exit status: ` and
  ends with `]` — the exact shape `BashTool::format` emits as the last line.
- **Print mode: a dead response channel abandons the turn** (`crates/clanky/src/main.rs`).
  Every `PrintSink` write used to be `writeln!(…).ok()`, so a closed stdout
  (`clanky -p … | head -1`) kept the turn alive: the model and tools ran on with
  output going nowhere, and the process exited 0. The sink now records the first
  write failure and `run_pipe` kills the plugin through the same `CancelHandle`
  ctrl+c uses (idempotent), so the in-flight request fails and the loop does not
  start another round — a tool call already dispatched in the current round still
  finishes (bounded work, nowhere to print). The sink's `finish()` closes the
  open block and flushes stdout, and the failure is taken from the sink *after*
  the turn — so a failure in the closing writes or in that final `flush()`
  counts too (a closed pipe can first show up at the flush, with the data still
  buffered). `run_pipe` then records the turn's final state under that real
  reason (so the session file says "response channel gone", not the plugin-death
  symptom) and returns `Error::PrintWriteFailed` — a non-zero exit naming the
  dead stream. Session-file writes stay non-fatal, as before.
  The sink keeps two failure slots: **response** (stdout) is fatal, **activity**
  (stderr) is not — `clanky -p … 2>&-` still delivers the answer, it just stops
  writing activity. Test: `a_closed_stdout_aborts_the_turn_with_a_nonzero_exit`
  (a plugin streaming past the 64 KB pipe buffer, one line read, then dropped).
- **`--name` is never silently replaced** (`main.rs`, `recorder.rs`, `tui/mod.rs`).
  An invalid `--name` used to be reported to stderr and the run continued under a
  generated name, contradicting both the recorder's doc comment and the
  user-values invariant. `main::run` now validates the name before either front
  end starts and aborts with the validation error; `Recorder::with_name` returns
  a `Result` instead of reporting-and-ignoring, so a future caller cannot
  reintroduce the substitution (the TUI reports it and records under no name,
  which cannot happen through the CLI). Test:
  `an_invalid_name_aborts_before_any_session_file_is_created` asserts a non-zero
  exit, the message, no answer, and no session file.
- **Session files, plugin logs and editor buffers are created owner-only**
  (`session.rs`, `clanky-protocol/src/process.rs`, `tui/editor.rs`). These files
  can carry the unredacted conversation (a model running `env`, plugin stderr),
  so a *new* file asks for `0600` at creation via `OpenOptions::mode` +
  `create_new`/`create` — not a `chmod` afterwards, which leaves the umask
  default in place for the instant between `open` and `chmod`. An existing
  file's mode is deliberately left alone (tightening it is the owner's call).
  Tests: `a_fresh_session_file_is_owner_only`,
  `a_reopened_session_file_keeps_its_own_mode`, `a_new_plugin_log_is_owner_only`,
  `an_existing_plugin_log_keeps_its_mode`, `the_buffer_file_is_owner_only`.
- **The editor buffer is created exclusively** (`tui/editor.rs`). The temp name
  is only as unique as pid + clock, so a plain `write` could silently truncate a
  file a concurrent session was editing. `write_buffer_file` now tries
  `create_new` (`O_EXCL`) and moves to the next candidate name on a collision
  (a stale file or a recycled pid is benign); it returns the path it actually
  owns, removes a half-seeded file if the initial write fails, and reports
  anything else. Tests: `the_buffer_file_is_created_exclusively`.
- **The `/model` picker shows a model once, not twice** (`tui/picker.rs`,
  `tui/mod.rs`). The row used the id as its label and the display name as the
  first detail field, so the two — which usually say the same thing
  (`meta-llama/Llama-3.3-70B-Instruct` / `Llama-3.3-70B-Instruct`) — printed
  side by side, and a 175-model DeepInfra catalog made for long rows. The
  label is now the display name (the id when the provider names none) and the
  detail carries only the attributes (context window, thinking, prices).
  The id is what a turn actually sends, so it is not lost: `PickerItem` gained
  a filter-only `search` field (`PickerItem::new(...).with_search(id)`), which
  `refilter` matches alongside the label while the row never prints it.
  Typing part of an id still narrows the list, and `confirm()` still returns
  the index into the original model list, so the selection path is unchanged.
  Test: `model_picker_shows_the_name_and_filters_by_the_id`.

## ✔ M21 — Project overrides: opt out of the user scope
Layering is additive everywhere: the project scope extends the user scope. A
project that must not inherit a machine's personal configuration — a shared
repo whose system prompt, skills, or settings have to be exactly the
project's — has no way to say so. Add one declarative escape hatch, in
`settings.toml`:

```toml
override = ["system", "skills"]
```

- `settings` — the user `settings.toml` is not read (no per-field overlay,
  no `[sampling]` merge, nothing); only the project file and its built-in
  defaults apply.
- `system` — the user `SYSTEM.md` is not read. `AGENTS.md` is *not* covered
  (agent instructions, not the system prompt) and stays layered; so do
  prompt templates (no override kind).
- `skills` — the user `skills/` directory is not read; only project skills
  exist.
- An unknown value is a parse error naming the accepted ones.
- Accepted in either file for consistency; in the user file it nullifies
  the user scope for every project. Lists union across scopes.
- When in play, say so once at startup (both front ends) and in `/system`:
  the effect is absence, which reads as a bug otherwise.

Design constraints:
1. **One place turns an override into behavior.** `config::Override` plus
   `config::scopes_for(overrides, resource)` — the user scope is dropped from
   the list, and the existing readers (settings files, context parts, skills)
   are unchanged. No per-reader override logic to drift.
2. **Two-pass settings load.** The override lists are read from both files
   first (a file that is itself dropped still gets a say in what is dropped),
   then the values are layered with the user file left out when `settings` is
   overridden. `Settings::load_scopes(user, project)` is the pure form.
3. **The override is not a setting.** No CLI flag, not in the effective
   settings dump or session `settings` records (those are values a turn runs
   with; the override is a file-reading declaration), and it unions rather
   than overlays. Serialization still round-trips through the file spelling.
4. **Absent project copy means none.** With `override = ["system"]` and no
   project `SYSTEM.md`, there is no system prompt part at all — that is the
   point of the feature, not a fallback case.

Done when: a project with `override = ["system", "skills"]` sends only its
own context (the user scope's files are not read), an unknown value aborts
with an actionable error, `override = ["settings"]` leaves the user
settings file unread, and the README documents it.

As built: `config::Override { Settings, System, Skills }` (parse/print, plus
`user_resource()` for messages) and `config::scopes_for(overrides, resource)`
are the whole mechanism; `config::scopes()` is unchanged for readers with no
override kind (prompt templates, session/log directories).
`Settings` gained a `r#override` field with a hand-written deserializer
(unknown names → a parse error naming `settings`, `system`, `skills`) and
serializer (the file spelling, never the variant names), and
`Settings::load_scopes` does the two-pass load; `load_from_paths` stays the
raw per-file overlay it always was. `SessionSettings` deliberately did **not**
gain the field — the dump describes what a turn runs with, and an override
is a declaration about which files were read — so no session record or
golden changed. `context::system_parts_in` now takes explicit scope
directories (`user: Option<&Path>`, `project: &Path`) instead of a scope
list, which makes the per-part decision testable without touching `$HOME`:
the user `SYSTEM.md` is skipped under `system`, the user `skills/` directory
is not scanned under `skills`, and the user `AGENTS.md` is untouched by both.
`skills::skills()` became `skills::skills(overrides)`. Both front ends print
`config::override_note` (`INFO: override: not reading the user scope's
SYSTEM.md, skills/ (project settings.toml)`) at startup and `/system`
repeats it. Tests: unit coverage in `config`, `settings`, `skills` and
`context` (each override kind, the union, the two-pass drop, the unknown-name
error, serialization), a `Launch` test, and five end-to-end runs in
`tests/plugin_pipe.rs` against a real user scope and project scope
(`override` absent, `system`, `system`+`skills`, `settings`, and an unknown
value).

## ✔ M22 — Esc stops the turn (cancel/stop)
Today the only way out of a running turn is ctrl+c, which quits the whole
session: a wrong direction, a runaway `bash`, or a model that will not stop
thinking costs you the terminal. Make the turn itself interruptible, in the
TUI only, without throwing away what it already produced.

`Esc`, pressed while a turn is running, stops the current activity — the
streaming response/thinking, a running tool, or the pause between retries —
and ends the turn. The partial output survives: streamed text and thinking
stay in the transcript and in the session file, a stopped `bash` reports
what it printed before it was killed, and the conversation stays continuable
(a follow-up turn sends everything the user saw).

Most of the wire machinery already exists and is untouched: protocol `cancel`
(spec §7), `CancelFlag` in every plugin, `CancelHandle`'s 2-second kill
fallback, and `FinishReason::Cancelled` — which the turn loop already handles
as a normal end (partial round flushed, text kept, `stopReason: "cancelled"`
recorded, `/md` renders it). What is missing is a way to *reach* it from a key
press, and cancellation of the parts the plugin cannot cancel for us.

1. **An interrupt token, TUI → turn loop.** `turn::Interrupt` (an atomic
   "stop requested" flag plus the id and `CancelHandle` of the in-flight
   chat), carried in `TurnConfig` as an `Option` field beside the existing
   doc-hidden `retry_sleep` hook (a turn run without one — every test, print
   mode — is simply not interruptible; the five full `TurnConfig` literals
   gain the field). `spawn_turn` attaches the session's `CancelHandle` once
   the plugin exists; the TUI keeps a clone.
2. **Publish the in-flight request id.** `ProviderClient::chat` allocates its
   id internally, so nothing outside can name the chat to cancel. Add an
   additive `chat_with(request, on_start, on_event)`; `chat` delegates with a
   no-op hook. The turn loop publishes `(id, handle)` into the token before
   streaming and clears it when the round's terminal message arrives.
3. **Esc → stop.** `handle_key` gains `(false, KeyCode::Esc)`, live only while
   `app.busy` (a modal or picker consumes Esc before this point). It sets the
   flag and, when a chat is in flight, sends `cancel(id)` immediately — no
   watcher thread; `CancelHandle` is built to be called from another thread.
   Esc while idle keeps doing nothing.
4. **Checks in the turn loop**, so an ignored or unavailable cancel still
   ends the turn promptly: before starting a round, before each retry sleep
   (never sleep out a backoff for a turn the user stopped), after each round,
   and after each tool call. A stop observed between rounds or during a tool
   ends the turn the same way a provider-reported `cancelled` does.
5. **Cancelled tool calls report partial output.** `Tool` gains a defaulted
   `execute_interruptible(&self, args, &Interrupt)` that delegates to
   `execute`, so `edit`/`view-image` (plain buffered IO) need no code.
   `BashTool` overrides it: its wait loop already polls every 100 ms, so a
   flag check joins the deadline check and reuses the same TERM/KILL of the
   command's process group. The kill closes the pipes, so the existing reader
   joins hand back everything printed before the kill — partial output costs
   no new plumbing. The result is the usual formatted output plus a final
   `[cancelled by user]` line (the shape `[exit status: N]` already has), and
   `is_failed_result` learns the marker so the transcript paints it red. It
   is returned as a *result*, not an error: the model sees the partial output
   and the reason, and can react.
6. **No dangling tool calls.** A round can assemble tool calls and be
   cancelled before any of them ran. That must never leave an assistant
   message with `tool_calls` and no results: `history_from_records` papers
   over it with a synthetic `ERROR: turn was interrupted` today, but the
   provider would reject or mis-associate the request. A round cut short by a
   cancel is flushed the way an errored round is (`calls: Vec::new()`), and
   calls already dispatched but not completed each get one synthetic
   cancelled tool result, so the message list stays valid.
7. **Queued prompts are cleared, exactly like cancelled tool calls.** A
   prompt typed while the turn ran (and shown as the `○ queued:` placeholder)
   was accepted only because that turn was going to end on its own. Once the
   user stops the turn, sending it is the opposite of what they asked for, so
   a cancel empties `app.pending` and each dropped prompt gets one `Info` line
   naming its preview (`cancelled queued prompt: …`) — a user-provided value
   is never dropped silently. The placeholder stays printed (terminal history
   cannot be rewritten), exactly as it does when the prompt does run.
8. **Say it happened.** `on_turn_done` seeing `finish_reason: Cancelled` adds
   a `⏹ turn cancelled` info entry; a cancelled turn otherwise ends silently
   and looks like a truncation. README: the Esc row in the TUI key table, a
   note beside the queued-message behavior, and the `bash` section's
   cancellation paragraph.
9. **Print/pipe mode is out of scope.** `-p` is a one-turn mode, so "end the
   turn" is already "end the session": its ctrl+c path (SIGINT → kill the
   plugin → flush the partial round) stays exactly as it is. `main::run` and
   the plugin plugins are untouched.

Design constraints:
1. **Cancellation is cooperative first, forceful second.** Ask the plugin
   (`cancel`), let the tool kill its own child, and fall back to the existing
   2-second process kill. A `ProcessDied` arriving while a stop is pending is
   the *expected* outcome of an ignored cancel, not an error to show — the
   turn ends cancelled.
2. **Nothing half-formed enters the conversation.** Whatever a cancel cuts
   short, `messages` must remain a valid chat: partial text is kept as an
   assistant message, every tool call has a result, and nothing is
   retried/replayed after a stop.
3. **Partial output is the point.** A stopped `bash` returns what it printed;
   streamed text stays on screen and in the session file. The only thing a
   stop discards is the queued prompts (item 7), and each is named.
4. **No new protocol surface.** Spec §7 already specifies all of this; no
   message, field, or capability is added, so no plugin changes and no
   protocol version bump.

Done when: in the TUI, Esc during streaming ends the turn at the next chunk
with the partial answer intact and `stopReason: "cancelled"` in the session;
Esc during `sleep 60` returns in about a second with the command's output so
far plus `[cancelled by user]`, and the process group is gone; Esc during a
retry backoff ends the turn instead of sleeping it out; queued prompts are
dropped with one visible note each and no turn starts; `/resume` restores the
cancelled turn exactly as the terminal showed it; and `-p` behavior is
unchanged.

Test surfaces: `turn.rs` unit tests with a mock handler that streams and
then reports `cancelled` (partial text kept, no further round, no dangling
`tool_calls`), and with a stop raised mid-round (one synthetic cancelled
result per dispatched call); `tools.rs` tests that flag a running `sleep 30`
from another thread and assert the call returns well under the join grace
with its partial output, the marker, and the group gone (the existing
`timeout_kills_grandchildren_holding_the_pipes` pattern); a fake `Transport`
pinning the id published by `chat_with`; `tui` tests for Esc-while-busy,
Esc-while-idle, and the queued-prompt drain with its notes; a golden
transcript for a cancelled round.

Open questions to settle while building:
- Should Esc also stop the model-catalog fetch or the editor round-trip?
  (Neither is a turn; the recommendation is no — they are bounded work.)
  **Answer: No.**
- Does a cancel mid-tool still let the remaining calls of that round run?
  (Recommendation: no — cancel them with the same synthetic result, since the
  user asked for the activity to stop, not for the round to finish.)
  **Answer: Cancel them all.**

As built:
- `turn::Interrupt` carries the stop flag, the plugin's `CancelHandle`
  (attached by the worker once the lazily-spawned plugin exists, so `Esc`
  can name the chat to cancel without the turn owning a watcher thread), and
  the in-flight request id. `TurnConfig::interrupt: Option<Interrupt>` sits
  beside `retry_sleep`; `None` (print mode, every test) is uninterruptible.
  `publish` re-checks the flag, so a stop that lands between the loop's last
  check and the publish still cancels the chat immediately.
- `ProviderClient::chat_with(request, on_start, on_event)`; `chat`
  delegates with a no-op hook. The id hook fires before anything is sent.
- Checks live before each round (also covering a stop while the plugin
  starts), in the retry wait (`Interrupt::sleep_or_stop`, polled every
  25 ms — a server `Retry-After` can be minutes), after each round, and
  after each tool call. A stop ends the turn through the same path a
  provider-reported `cancelled` does; a `ProcessDied` seen while a stop is
  pending is that expected outcome, not an error.
- A stopped round is flushed like an errored one (`calls: Vec::new()`), so
  no assistant message ever carries `tool_calls` without results; calls of
  the round that never ran each get `tools::cancelled_result()` (an `ERROR:`
  text ending in the marker) as a real `ToolResult` event + `tool` message.
- `Tool::execute_interruptible` defaults to `execute`; `BashTool` overrides
  it. Its wait loop checks the flag alongside the deadline and reuses the
  same TERM/KILL of the process group; the kill closes the pipes, so the
  existing reader joins hand back what printed. The result is the normal
  formatting plus a final `[cancelled by user]` line (`tools::CANCELLED_MARKER`,
  also added as a final line of the synthetic results), and `is_failed_result`
  matches it as an exact last line — output that merely mentions it is not a
  failure. The exit status is deliberately not reported on that path.
- TUI: `Action::Stop` from `(false, KeyCode::Esc)` guarded on `app.busy`
  (a modal or picker consumes Esc earlier); `handle_key` calls
  `interrupt.request_stop()`, and the event loop drains `app.pending` into
  one `cancelled queued prompt: <preview>` `Info` line each. `on_turn_done`
  pushes `⏹ turn cancelled` for `finish_reason: Cancelled`. The token is
  recreated per turn, so a stale flag cannot leak into the next one.
- No protocol change: §7 already specified all of it.
- Tests: turn.rs (provider-reported cancel keeps partial text with no
  further round; a stop mid-round gives each dispatched call a result and the
  undispatched ones a synthetic cancelled result; a stop before the first
  round sends nothing; a stop during backoff returns immediately with the
  backoff hook never called; a dying plugin after a stop ends cancelled; a
  round that ignored the stop drops its calls), tools.rs (a stopped
  `sleep 30` returns quickly with its partial output + marker; the process
  group goes with it; no marker on a normally-completed command; the marker's
  exact-last-line detection), client.rs (the id `chat_with` publishes),
  tui/mod.rs (Esc busy, Esc idle, the queued drain, the `⏹` note), a
  `cancelled-round` golden pinning the partial assistant record, the
  `stopReason: "cancelled"` record and the follow-up history, and two
  `plugin_e2e` runs against real Python subprocesses: one that honors
  `cancel` (stops at the next chunk, partial text kept) and one that ignores
  it (the 2-second kill fallback ends the turn cancelled instead of
  surfacing an error).

## ✔ M23 — Thinking passback: send reasoning back to the model

**All four sessions done: the payload is captured, carried, sent back —
live on all three providers (session 2) — survives a quit, so a resumed
session replays the identical message list a live one held (session 3),
and is now a knob (`thinking_passback`) with cost docs that state the
measured magnitudes per provider (session 4; see the "As built" notes
below and `design-docs/m23-sessions.md`).**

Clanky never sends an assistant round's thinking back to the provider. The
protocol's assistant message is `{content, tool_calls}` and nothing else
(`crates/clanky-protocol/src/messages.rs:255`), every `ChatMessage::Assistant`
the turn loop pushes carries text plus calls only (`turn.rs:492, 511, 519,
531, 601`), `StreamAssembler` keeps thinking "for display/session storage;
they are not part of the assistant message sent back"
(`crates/clanky-protocol/src/stream.rs:34`), and `history_from_records` drops
`Record::Thinking` outright (`session.rs:621`). The reasoning is streamed,
shown, and stored — and then thrown away.

For two of the three providers that is merely a missed optimization. For
Anthropic it is a correctness problem in the tool loop, and for LiteLLM
fronting Anthropic it is a documented 400. This milestone closes it.

### Why this is worth doing (and what the docs actually say)

M23 exists because an earlier claim — that OpenAI-compatible APIs are not
*supposed* to receive reasoning back — did not survive checking. The real
picture, per provider:

- **DeepInfra (OpenAI-compatible):** permissive. Its request schema has
  `reasoning_content` on the *input* `ChatCompletionAssistantMessage`
  (`docs.deepinfra.com/api-reference/chat-completions/openai-chat-completions.md`,
  the `ChatCompletionAssistantMessage` schema), and it is tokenized as
  context when sent. Measured against the live API with a `tools` +
  `reasoning_effort: "high"` tool loop, second round with the tool result:

  | model | without `reasoning_content` | with `reasoning_content` |
  |---|---|---|
  | `openai/gpt-oss-20b` | 200, prompt=154 | 200, prompt=**196** |
  | `deepseek-ai/DeepSeek-V4-Flash-0731` | 200, prompt=426 | 200, prompt=**438** |

  No 400 either way — DeepInfra is more lenient than DeepSeek's native API.
  Omitting is *safe* but forfeits continuity.
- **DeepSeek native (normative, conditional):** with `tools` present,
  "`reasoning_content` must be fully passed back to the API in all
  subsequent requests — even for turns where the model did not perform a
  tool call. If your code does not correctly pass back `reasoning_content`,
  the API will return a 400 error." Without `tools`, it "does not need to be
  passed back; even if passed to the API, it will be ignored."
- **Anthropic (normative):** "**Required:** within a tool-use turn, pass
  thinking blocks back." The block must be returned *complete and
  unmodified*, signature included; modified blocks are rejected with a 400
  `thinking blocks cannot be modified`. Older blocks may be omitted
  ("**Allowed:** outside tool use, omit prior turns' thinking"), and the API
  filters what it keeps per model.
- **LiteLLM fronting Anthropic:** documents this exact client shape as
  broken — "you must include `thinking_blocks` from the previous assistant
  response... Failure to do so will result in a 400 Bad Request error"
  (`docs.litellm.ai/docs/reasoning_content`), with `modify_params=True`
  silently dropping thinking as the workaround.

### Re-measured against the live API (2026-11): the effect is ~1:1, and the text really is used

A second round of measurement, run against DeepInfra with real credentials,
corrects two things the table above understates and settles the question
"is the reasoning *used*, or merely accepted?".

**It is used — the text is genuine model context, not decoration.** Placing
a self-instruction in the previous assistant message's `reasoning_content`
changes the answer, which is the only direct proof available that the field
is not inert:

| model | plain | with injected `reasoning_content` |
|---|---|---|
| `openai/gpt-oss-20b` | "The current time in Paris is 12:00 PM" | "Il est 12:00 à Tokyo." (French + wrong city) |
| `deepseek-ai/DeepSeek-V4-Flash-0731` (tools, effort) | "2+2 equals 4." | "Quatre, et Tokyo n'est concernée par ce calcul." |

No 400 in any shape: DeepInfra accepts `reasoning_content`, `reasoning`,
`thinking_blocks`, `reasoning_details`, `thinking` and unknown fields alike.
`reasoning_content` and `reasoning` are aliases that both work and both cost
tokens; **`thinking_blocks`, `reasoning_details` and `thinking` are silently
ignored** (measured: +0 prompt tokens). So the LiteLLM bullet above stays
right — `thinking_blocks` is what an Anthropic-backed model through the proxy
needs — but sending it to DeepInfra is a no-op, not a fidelity win.

**The token cost is ~1:1, not the ~40 tokens of the earlier table.** That
table used a short reasoning string; a ~1800-character block costs **+401
prompt tokens**, i.e. ≈4.5 chars/token, on every later round that carries it.
Whether a given model *counts* the block is per-model and per-shape, which
is exactly the "tool calling is part of the chain-of-thought" pattern:

| model | counted when |
|---|---|
| `openai/gpt-oss-20b` | the assistant message carries `tool_calls` (always otherwise ignored) |
| `deepseek-ai/DeepSeek-V4-Flash-0731`, `-V4.1-Flash-1017` | the *request* carries `tools` (always otherwise ignored) |
| `Qwen/Qwen3-32B`, `zai-org/GLM-5.3-Flash` | always |

**What that costs in clanky's own sessions.** Replaying the 11 saved
sessions in `.clanky/sessions/` through the passback arithmetic at the
measured 4.49 chars/token, over a prompt total of 73.3M tokens:

| policy | added prompt tokens | vs. today |
|---|---|---|
| immediately previous round only | +0.98M | **+1%** |
| current turn only (the tool-loop rounds) | +40.6M | **+56%** |
| previous turn only | +10.4M | **+14%** |
| all turns | +53.4M | **+73%** |

The intra-turn share is **76% of the all-turns cost**, which is the single
most important number here: the expensive part is not cross-turn retention,
it is the tool loop, where it is also *mandatory* on Anthropic and DeepSeek.
The reasoning in those sessions is 4.37M characters against 81k characters
of visible answers — 54× — so this is a real multiplier, not a rounding
error.

Two facts keep it tolerable:

- **Prompt caching still applies to passed-back reasoning.** The same request
  repeated, with and without `reasoning_content`: `cached_tokens` rose
  4352 → 7424 → 7680 with passback, and caching works normally on the
  rc-bearing prefix. The marginal cost is cache-read priced.
- **Anthropic filters and bills only what it keeps.** Sending everything is
  documented as safe there; DeepInfra/DeepSeek have no server-side filter,
  so what you send is what you pay for. That asymmetry is why the knob below
  is per-provider rather than one global switch.

**Corroboration that passback is worth its cost:** arXiv 2606.00135 (ICML
2026) §3.3 measures retaining thinking history on multi-turn BFCL at
**~2–5% better** on Qwen3 models and adopted it for all later experiments;
OpenAI's harmony docs (the actual gpt-oss chat format) say "the model is able
to call tools as part of its chain-of-thought and because of that, we should
pass the previous chain-of-thought back in as input for subsequent sampling";
OpenAI's function-calling guide makes it mandatory for reasoning items
returned with tool calls; and OpenAI's reasoning guide notes that a prefix
change makes the API *drop* reasoning, after which the model "may have to
redo some reasoning, which can increase token usage, cost, and latency."

**Server-side storage is not a substitute.** Every provider clanky speaks is
stateless for reasoning: Anthropic's Messages API is stateless by design
(only the `message-threads` beta and Managed Agents keep state server-side),
and Chat Completions has no reasoning input field at all. OpenAI's
`previous_response_id`/`encrypted_content` model does not apply to anything
clanky sends. Nothing re-includes the reasoning unless the client does.

### The finding that shapes the design: the signature, not the text

**The block that must be passed back is `{type, thinking, signature}`, and
clanky captures neither the signature nor redacted blocks today.**

- `AnthropicBlock` (`adapter.rs:224`) has `type`/`text`/`thinking`/`id`/
  `name`/`input` — **no `signature`**, and no `data` for `redacted_thinking`.
- `StreamDelta` (`adapter.rs:393`) has **no `signature` field**, so the
  `signature_delta` event falls into the `_ => {}` arm (`adapter.rs:489`).
- Non-streaming responses drop `redacted_thinking` entirely.

Consequence: a milestone that simply re-sent the accumulated *text* would be
**worse than doing nothing** — Anthropic verifies signatures and rejects
altered blocks with a 400. The signature is the payload; the text is a
display convenience. On models whose `display` defaults to `"omitted"`
(including Claude Haiku 5.5, Claude Opus 5.5, Claude Sonnet 5.5 and the
other current models), thinking blocks arrive with an **empty `thinking`
field and a populated signature** — so text-only passback would send
nothing at all, and text-plus-fabricated-signature would 400.

This is why the protocol field must carry an opaque, per-block payload rather
than a `String` of thinking text.

### Why it works today without erroring (verify before implementing)

Anthropic documents graceful degradation: "If you toggle thinking mid-turn
(the API doesn't error). Instead, it silently disables thinking for that
request... or disable thinking when the conversation history is incompatible
with thinking being enabled." Sending a tool-loop history with no thinking
blocks at all is exactly "incompatible history", which is the likely reason
the Anthropic provider runs without a 400 — **but that is an inference, not
an observed fact**, and it predicts a real cost: thinking silently disabled
for the follow-up rounds of every tool loop, plus no cache reuse of the
thinking prefix. Item 0 confirms it against the live API before any code is
written, because it decides whether this milestone is a bug fix or an
optimization.

**Field report (the reason this milestone is hedged, not asserted):** the
maintainer has been running the Anthropic provider with `thinking = "32000"`
and has seen **no errors at all** — including, presumably, through tool
loops. That is in direct tension with the per-model table below, which lists
`{type: "enabled", budget_tokens: N}` as a 400 for Claude Haiku 5.5. Either
the configured model resolves to something the table describes differently,
the deployment's API version predates that rejection, or the thinking block
is being dropped before it reaches the wire. Item 0 exists to find out which
— do not "fix" the thinking mode on the strength of the table alone, since a
working configuration that the docs call invalid is exactly the kind of
user-supplied value the project invariants say to pass through, not correct.

### Implementation

0. **[x] Spike first, live.** With real Anthropic credentials and
   `thinking` enabled, drive one tool loop and capture the follow-up request
   clanky sends. Confirm (a) no 400 today, (b) what the API does with the
   thinking-less history — `message_delta` stop reason and the round's
   `output_tokens_details.thinking_tokens` show whether thinking was
   disabled. Also record which thinking *mode* the configured model accepts:
   the per-model table makes `{type: "enabled", budget_tokens: N}` a 400 on
   several current models (Claude Haiku 5.5 among them, which prefers
   `adaptive`), so `settings.toml`'s `thinking = "32000"` may already be
   failing or being degraded on the model actually configured. Findings go
   in this section's "As built" note and settle items 1–2's shape.

   **Amendments from the 2026-11 re-measurement** (see the section above):

   - **The configured pair to check is `claude-haiku-5-5` +
     `thinking = "32000"`.** `~/.clanky/settings-anthropic.toml` sets
     exactly that, and `adapter.rs:170-178` turns a positive budget into
     `{"type":"enabled","budget_tokens":32000}`. The current per-model table
     lists `enabled` + `budget_tokens` as a **400 on Haiku 5.5** (adaptive
     only). This is now the top item to resolve, not a side note: if the
     request is being rejected, everything else in this milestone is being
     validated against a configuration that is not running.
   - **Record `display` too.** Haiku 5.5 defaults to `display: "omitted"`,
     so a working request still returns `thinking: ""` with a populated
     `signature`. Capture a real response body: if the text is empty, the
     signature-only case is the *normal* path for this deployment, and
     constraint 3 below is the mainline rather than the edge case. It also
     means the "the thinking text would often be useful" premise holds on
     DeepInfra/DeepSeek but **not** on this Anthropic config — there the
     encrypted signature is the payload and the text is a display
     convenience. Do not let the Anthropic arm of this milestone assume
     readable text exists.
   - **Do not add the `thinking-binding-controls-2026-08-01` beta header.**
     It would buy `input_transformations` diagnostics and
     `prefix_mismatch_behavior: "drop_block"`, both useful, but it is a
     second behavioral change riding on a correctness fix. Note it as a
     follow-up; keep this milestone to passback.

   **As built (spike, 2026-11-16, live against `api.anthropic.com` with the
   stored OAuth credentials).** Method: a logging reverse proxy on
   `127.0.0.1` in front of `api.anthropic.com` (`ANTHROPIC_BASE_URL`), so
   the follow-up request is captured byte-for-byte from an unmodified
   `clanky -p`; plus ~200 direct `curl`/Python probes against the API for
   the questions a single clanky run cannot answer. The proxy and captures
   are throwaway (`target/tmp/m23/`); no repository code was changed.

   **Headline: item 0's premise is half right, and the half that is wrong
   changes the milestone's shape.** There is no 400 today, and — on the
   configured model — there is also no silent degradation. But on the
   whole `4-5`/`4-6` family the degradation is real, reproducible, and
   total (thinking is silently off for every follow-up round of every tool
   loop). So the milestone is **a correctness fix for users on the `4-x`
   models and a fidelity/cost optimization on the configured
   `claude-haiku-5-5`** — not one or the other. Both are worth doing;
   which one a given user gets depends on their model.

   1. **`claude-haiku-5-5` + `thinking = "32000"` is accepted (200) and
      thinking really runs.** The per-model table's "400 on Haiku 5.5"
      is wrong as stated: `{type:"enabled",budget_tokens:32000}` returns
      200, produces thinking blocks, and reports
      `output_tokens_details.thinking_tokens > 0`. The `/v1/models`
      catalog does advertise `enabled.supported: false` /
      `adaptive.supported: true` for this model — the catalog is the
      unreliable one, the Messages endpoint is the truth. Per the project
      invariants nothing should "fix" `settings.toml`: **the configured
      pair works and must be left alone.**
      *Caveat found while probing:* `budget_tokens` is a much weaker
      control on `5-5` than on `4-x`. Same prompt, 5 reps per budget,
      median `thinking_tokens`: on `haiku-4-5` **244 at budget 1024 vs
      4,539 at 32000** (~19×, a clear effect); on `haiku-5-5` **2,286 vs
      3,112** (~1.4×, barely above the spread of the runs themselves —
      single observations ranged 2,102–3,689 and 2,396–3,689). Note also
      that even on `haiku-4-5` the budget is not a hard cap: budget 1024
      produced 3,849 and 7,618 thinking tokens in individual runs. So on
      the configured model the user's `thinking = "32000"` is a soft hint
      at best, not a cap. That is provider-side behavior on a value we
      pass through verbatim — under the invariants we pass it through and
      surface what happens; it is **not** this milestone's job to
      translate `enabled` into `adaptive` or vice versa. Recorded as a
      follow-up below.

   2. **`enabled` mode has a hard 1024-token floor and a `max_tokens`
      ceiling**, both live 400s with actionable messages:
      `thinking.enabled.budget_tokens: Input should be greater than or
      equal to 1024` (512 and 1023 reject; 1024 accepts) and
      ``` `max_tokens` must be greater than `thinking.budget_tokens` ```
      (verified on `haiku-4-5`: `max_tokens:1024` + budget 2048 400s;
      `haiku-5-5` accepts that pair, another sign its budget is not
      enforced). `clanky`'s `parse_thinking` accepts any non-negative
      integer, so `--thinking 512` reaches the wire and the user sees the
      provider's error — exactly what the invariants ask for. **No local
      pre-validation to add.**

   3. **`display` is a per-model default, and on this deployment the text
      is simply not there.** `haiku-5-5` returns `thinking: ""` with a
      populated `signature` under `enabled` with no `display`, with
      `display:"omitted"`, and — notably — with
      `display:"summarized"` too (5/5 reps). Only `adaptive` +
      `display:"summarized"` produces text on `haiku-5-5` (923–1,288
      chars, 3/3). `haiku-4-5` and `sonnet-4-5` return real text by
      default. **So constraint 3 is confirmed as the mainline for the
      configured model: the signature is the payload, the text is a
      display convenience that `enabled` mode does not even offer.**
      `clanky` currently captures neither — it streams the (empty) text
      and drops the signature on the floor.

   4. **The signature is genuinely the payload, and the thinking *text* is
      display-only.** All on `haiku-4-5`, the one current model that
      returns readable text, so both halves are testable:
      - *Tampering, signature:* altering any byte of `signature` (last 4
        chars, the whole thing, emptying it, deleting the field) is an
        immediate `` 400 invalid_request_error: Invalid `signature` in
        `thinking` block ``. *Tampering, text:* truncating, appending to,
        or emptying the thinking text is accepted with **200**. The API
        verifies the signature and ignores the text.
      - *The text is not sent as context.* `input_tokens` is
        **byte-identical** for the same history with the text left
        original, emptied to `""`, and padded with **+8,000 characters**
        (1,332 in all three cases in one run, 1,379 in another, on both a
        tool-loop and a cross-turn history). Sixteen kilobytes of text
        costing zero tokens is conclusive: the text never reaches the
        prompt. (A behavioural injection probe through the text was also
        run and was **inconclusive/noisy** — a single non-reproducing
        answer change across 6 trials, most likely ordinary sampling
        variance. The token measurement is the reliable evidence and it
        is unambiguous, so **do not cite the injection probe as proof of
        anything**; the milestone's earlier DeepInfra injection
        experiment does not transfer to Anthropic.)
      - *Cost tracks the signature.* Added prompt tokens on the tool-loop
        follow-up were +75/+104/+78/+220/+76/+169 for signatures of
        588/688/592/1100/584/1136 chars — **0.13–0.20 tokens per
        signature char (mean 0.15)**, with **+0** for any change to the
        text. Signatures ran **2–4× the text length** (mean 3.1×).
      **This confirms the milestone's own framing outright: on Anthropic
      the encrypted signature *is* the reasoning and the text is a
      display convenience.** It also means the `display: "omitted"`
      models lose nothing by passing back an empty-text block — the
      payload is intact.
      **Consequence for item 7's cost model:** the "+56% intra-turn /
      +73% all-turns" arithmetic in the milestone above was computed from
      *thinking text* at 4.49 chars/token. On Anthropic that is the wrong
      base: the text is free and the signature is billed at ~0.15
      tok/char over a longer string, so the per-block cost lands in a
      similar order of magnitude for an entirely different reason. The
      DeepInfra numbers (where `reasoning_content` *is* the payload)
      stand as measured. **Item 7 must state the two providers' cost
      models separately, not reuse one ratio.** A practical corollary:
      passback cost on Anthropic is roughly `0.15 × signature_chars`
      per block per later round, and signature length grows with thinking
      effort (588 chars at 44 thinking tokens → 1,136 chars at 137), so the
      expensive rounds are the ones that reasoned hardest.

   5. **A thinking-less tool-loop history silently disables thinking —
      on the models where `enabled` is the working mode.** This is the
      degradation item 0 predicted, and it is clean and reproducible.
      The signal is `output_tokens_details` in `message_delta`:
      **absent** when thinking was disabled for the request, **present**
      when it ran. Tool-loop trials per model (`haiku-4-5` and the `5-5`
      pair got 5 reps; the others 2, because each was unanimous on the
      first):

      | model | `enabled`/`adaptive` | R1 thinking | R2 history without blocks | R2 history with blocks |
      |---|---|---|---|---|
      | `claude-haiku-4-5-20251001` | enabled | 44–278 tok | `output_tokens_details` **absent** ×5 | **present** ×5 |
      | `claude-sonnet-4-5-20250929` | enabled | 323 tok | **absent** ×2 | **present** ×2 |
      | `claude-opus-4-5-20251101` | enabled | 40 tok | **absent** ×2 | **present** ×2 |
      | `claude-sonnet-4-6` | enabled | 45 tok | **absent** ×2 | **present** ×2 |
      | `claude-opus-4-6` | enabled | 26 tok | **absent** ×2 | **present** ×2 |
      | `claude-haiku-5-5` | enabled | 184–629 tok | **present** ×5 | **present** ×5 |
      | `claude-sonnet-5-5` | adaptive | 155–187 tok | **present** ×5 | **present** ×5 |
      | `claude-opus-5-5` | adaptive | (no block emitted) | **present** ×2 | **present** ×2 |

      **So the configured deployment is in the lucky group**: `haiku-5-5`
      keeps thinking on across a blockless tool loop, which is why the
      maintainer has never seen a problem. The `4-x`/`4-6` family is in
      the unlucky group, where today's clanky runs every follow-up round
      with thinking silently off. That is a real, measurable quality bug
      for any user on those models, and it is the strongest argument for
      item 4.
      **Also confirmed:** the degradation is triggered by a **tool-loop**
      history specifically. A cross-turn (no `tool_use`) history missing
      its thinking block does *not* degrade on `haiku-4-5` (thinking
      stays `present` in 3/3 trials). The API is matching the documented
      rule — "thinking blocks are required within a tool-use turn,
      optional outside it" — rather than the cruder "any blockless
      assistant message disables thinking". **Item 8's split (intra-turn
      mandatory, cross-turn optional) is exactly right, and is now
      measured rather than inferred.**

   6. **Block placement and run integrity are enforced, and the rules are
      stricter than the milestone assumed.** On `haiku-4-5`, with 57–70
      thinking blocks in one round (the magic redaction string produces
      dozens):
      - **Blocks must come first**, before the text and `tool_use`
        blocks. Blocks *after* `tool_use` → thinking silently disabled
        (no error, `output_tokens_details` absent). So the milestone's
        "before the text block, exactly as received" is load-bearing.
      - **Only a trailing truncation is legal.** Keeping a *prefix* of
        the run (dropping the newest blocks) → 200. Keeping a *suffix*
        (dropping the oldest) → `400: thinking or redacted_thinking
        blocks in the latest assistant message cannot be modified. These
        blocks must remain as they were in the original response.`
        Dropping from the middle (a hole) → same 400. **This inverts
        item 8's "Anthropic allows dropping the oldest, the newest, or
        all of them, but not one in between": in the *latest* assistant
        message only a prefix survives.** Dropping *all* of them is fine
        (200, thinking just goes off), and older messages can have their
        blocks dropped freely — the rule is scoped to the latest
        assistant message, exactly as the error text says.
      - **`redacted_thinking` data is verified the same way**: any
        alteration (last 8 chars, emptied, reversed) →
        `400: Invalid `data` in `redacted_thinking` block`.
      - **Signatures are not bound to a conversation on this deployment.**
        Transplanting conversation B's thinking block into conversation
        A's tool loop returned **200** in 3/3 reps — the documented
        `Invalid signature in thinking block. The block is bound to a
        different conversation` never fired. Constraint 1's "never
        modify" rule still holds absolutely (any byte change 400s), but
        the "bound to a conversation" clause is not observable here, so
        don't design a cross-conversation validity check around it.

   7. **Streaming shape for `redacted_thinking`** (for item 1):
      `content_block_start` carries the **whole** `{"type":"redacted_thinking","data":"…"}`
      block in one event — there are **no deltas** for it. A normal
      `thinking` block streams as `content_block_start {type:"thinking",
      thinking:""}` + N `thinking_delta`s + **one `signature_delta`** +
      `content_block_stop`. So the accumulator needs a per-block slot
      for the signature (joined `signature_delta`s) and must take
      `redacted_thinking`'s `data` off the start event — the milestone's
      item 1 describes exactly this, and the wire confirms it. Note
      `content_block_start` for `thinking` carries `"signature":""`, not
      an absent field, so "empty" and "absent" both need handling.
      The non-streaming path (`stream=false`) puts `signature` and
      `data` directly on the content block, as expected.

   8. **Side observation, not a milestone item: the OAuth identity block is
      load-bearing for the newer models.** On the OAuth surface, a
      Messages request **without** the Claude Code identity system block
      returns `429 rate_limit_error` — not a 401 or 403 — on
      `haiku-5-5`, `sonnet-5-5`, `sonnet-4-5` and the other `4-5`/`4-6`
      models; the same request *with* it returns 200. `haiku-4-5` does not
      care. Interleaved, 3 cycles each way, so it is not real rate
      limiting. clanky already sends the identity block on the OAuth path
      (M11c), so nothing needs to change — but it explains any future
      "why does a bare request 429?" confusion, and it is worth knowing
      that a 429 from this deployment can mean "you did not identify as
      Claude Code", not "you are going too fast".

   9. **Error strings to reproduce in tests** (all
      `400 invalid_request_error`, all surfaced verbatim by clanky):
      - `thinking.enabled.budget_tokens: Input should be greater than or equal to 1024`
      - ``` `max_tokens` must be greater than `thinking.budget_tokens`. ```
      - `adaptive thinking is not supported on this model`
      - ``` `temperature` may only be set to 1 when thinking is enabled. ```
      - `thinking: Input tag 'bogus' found using 'type' does not match any of the expected tags: 'adaptive', 'between_tools', 'disabled', 'enabled'`
      - `` Invalid `signature` in `thinking` block ``
      - `` Invalid `data` in `redacted_thinking` block ``
      - `` `thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified. These blocks must remain as they were in the original response. ``

   **Two findings that change the plan, beyond item 0's questions:**

   - **Item 8's window policy cannot drop "the oldest" on the latest
     assistant message.** Only a trailing truncation is accepted there
     (finding 6). Since intra-turn passback is mandatory and must not be
     switchable, the only window that matters is *cross-turn*, where
     older messages may lose their blocks freely — so `previous-turn`
     (keep the current turn's blocks in full, drop older turns') is not
     just the recommended default, it is the only shape the API accepts
     without truncating the newest run. **The policy description in item
     8 should be rewritten to say "drop whole older turns, never a
     partial run in the latest message".**
   - **`redacted_thinking` blocks are part of the same run, so the prefix
     rule covers them too.** Dropping *all* of them is legal (200,
     thinking silently off — 3/3 reps at 21–94 redacted blocks), and so
     is keeping any *prefix*; keeping a *suffix* or punching a hole 400s
     exactly as it does for plain thinking blocks. So a partially
     truncated redacted run is a hard failure, and a window policy that
     keeps the newest blocks instead of the oldest would turn a redacted
     tool loop into a 400. Item 1's "must reach the client too" is what
     keeps the run intact.
     *(Correction: an earlier probe in this spike reported that dropping
     all redacted blocks 400'd. That was a bug in the probe — it
     duplicated the `tool_use` block, and the API's real complaint was
     `` `tool_use` ids must be unique ``. Re-run correctly, dropping all
     of them is fine. The rule is the prefix rule, not "redacted blocks
     are mandatory".)*

   **What this settles, item by item:** items 1–5 are confirmed as
   written (capture the signature and `data` on both transports, carry an
   ordered block vector through the protocol, emit a terminal per-block
   chunk when the block closes, attach blocks to the pushed assistant
   message, and replay them from session records) — findings 7, 4 and 3
   pin the exact wire shapes they must handle. **Item 6's Anthropic arm
   is confirmed and its "send everything" instruction is safe** (the API
   filters, and cross-turn blocks are optional, so over-sending costs
   nothing it does not choose to keep). **Item 8 needs the window-policy
   rewrite above.** **Item 7 needs the two-provider cost split above.**
   Nothing here argues for changing `settings.toml`, `parse_thinking`, or
   the thinking-mode mapping: the configured pair works.

   **Not tested / deferred:** the `thinking-binding-controls-2026-08-01`
   beta header (per the amendment, deliberately not added); the
   `between_tools` thinking type (`sonnet-5-5` advertises it — a third
   thinking mode this milestone does not touch); and the `enabled`-mode
   budget being a soft hint on `5-5` models — worth a follow-up README
   note (not a code change, per the invariants), since
   `thinking = "32000"` in a settings file reads as a cap and is not one
   on the configured model.

   **Follow-up done (2026-11): the `5-5` family shows no thinking text
   under `enabled`, and that is the API, not Clanky.** Reported after M23
   as "M23 stopped Anthropic from showing thinking". It did not: the
   session records were empty-text *before* M23 too (the earlier sessions
   had no thinking blocks at all because no signature was captured then),
   and M23 only added the signature — which is why blocks appear now where
   nothing did before. Live re-measurement, driven through the real
   `clanky` binary and a logging `ANTHROPIC_BASE_URL` proxy:

   | model | request | readable text |
   |---|---|---|
   | `sonnet-4-5`, `haiku-4-5` | `enabled` | yes (default) |
   | `sonnet-4-5`, `haiku-4-5` | `enabled` + `display:"omitted"` | no |
   | `sonnet-5-5`, `haiku-5-5`, `opus-5-5` | `enabled` (+ any `display`) | **no** |
   | `sonnet-5-5`, `haiku-5-5` | `adaptive` + `display:"summarized"` | yes |
   | `sonnet-5-5` | `adaptive` (no `display`) | no |

   On `5-5` under `enabled`, rounds reported
   `output_tokens_details.thinking_tokens` ≈1100–1300 with an empty
   thinking field — reasoning ran, the signature came back, no text. This
   also corrects the spike note above ("only `adaptive` +
   `display:"summarized"` produces text on `haiku-5-5`"): that was right,
   but the spike did not record that `display` is *ignored* under
   `enabled`, which is the part that made the behavior look like a
   regression. `effort` is the dial in `adaptive` mode (`low`/`medium`
   produced no text on `sonnet-5-5`; `high`/`xhigh`/`max` did), and
   `adaptive` + `budget_tokens` is a 400 (`Extra inputs are not
   permitted`), so the budget setting is inert once a `[sampling]
   thinking` entry is set.

   **No code change, per the invariants** — translating `enabled` into
   `adaptive` would be the program overriding the user's mode, and the
   budget→`enabled` mapping is what the milestone explicitly declined to
   touch. The fix is documentation: README's Anthropic section now has a
   "Thinking: what you actually see" table with the `[sampling]`
   `adaptive` recipe, `settings.toml`'s `thinking` note points at it, and
   the `ctrl+t` section says there is nothing to toggle when the model
   returns no text. Making `adaptive` reachable from the `thinking`
   setting itself is a real design decision (it needs a protocol field for
   the mode), left open.

   **As built (session 1, 2026-11).** The payload is captured and carried
   end to end; nothing is sent back yet, so the wire is byte-identical to
   before and every existing golden passes unchanged.

   - **Anthropic capture** (`crates/clanky-provider-anthropic/src/adapter.rs`).
     `AnthropicBlock` gained `signature`/`data`; `StreamDelta` gained
     `signature` and a `signature_delta` arm; `StreamState.blocks` grew from
     `HashMap<u32, String>` into a per-block `StreamBlock { kind, text,
     signature, data, closed }`. `redacted_thinking` takes its `data` off
     the `content_block_start` (the wire sends no deltas for it). One
     terminal `ChunkPayload::ThinkingBlock` is emitted per reasoning block,
     in generation order, on `content_block_stop`; a stream that reaches
     `message_stop` without that event still closes its open blocks (a
     sweep in wire order) so nothing is dropped. The non-streaming path
     (`stream = false`) emits the same chunk from the block's own fields.
     **Payload rule enforced at the source:** a `thinking` block whose
     signature never arrived is not emitted (constraint 2, "signature or
     nothing"), and a `redacted_thinking` block without `data` likewise.
     An empty-text block is emitted with `text: ""` kept — that is the
     `display: "omitted"` mainline, not an edge case.
   - **Protocol** (`crates/clanky-protocol/src/messages.rs`).
     `ChatMessage::Assistant.thinking: Option<Vec<ThinkingBlock>>`
     (`#[serde(default, skip_serializing_if = "Option::is_none")]`) with
     `ThinkingBlock { kind, text?, signature?, data? }` and
     `ThinkingBlockKind::{Thinking, Redacted}`; a `ChatMessage::thinking()`
     accessor mirrors `images()`. `ChunkPayload::ThinkingBlock { block }`
     carries the finished block. `StreamAssembler` collects them
     (`thinking_blocks()`), keeping the `thinking` display string exactly as
     before. All additive and optional: an absent field stays absent, so
     every pre-M23 request is byte-identical (the untouched goldens prove
     it). No version bump — spec §9 records it as a v2 additive change.
   - **Turn loop** (`crates/clanky/src/turn.rs`). `TurnEvent::Round` gained
     `thinking_blocks: Vec<ThinkingBlock>`, and `run_turn_with` attaches
     `blocks_or_none(..)` to the assistant messages it pushes for a
     tool-call round, a `Continuable` resume, and a final plain-text round.
     A cancelled round and an error-flushed partial round attach none (a
     cut-short signature is unusable, the same rule their tool calls
     follow), and a round with no blocks passes `None`, never an empty list.
     This is the attachment session 2's provider arms read; the plugins
     themselves still ignore the field (`thinking: _`), so no wire changed.
   - **Tests.** Protocol: `messages.rs` (the field is absent when unused,
     present blocks round-trip, `thinking()` on every role), `stream.rs`
     (blocks collect in order beside the display text; a blockless round
     reports none), `client.rs` (loopback transport), `serve.rs` (both
     directions over JSONL: request blocks arrive intact, chunk blocks go
     out with only the fields each carries). Anthropic: the captured
     streaming fixture now asserts the signature verbatim; raw-event tests
     for the empty-text-signed case, a whole `redacted_thinking`, three
     blocks keeping wire order, an unsigned block being dropped, a block
     closed by the `message_stop` sweep, and the non-streaming path.
     DeepInfra/LiteLLM: a test each that an assistant message's blocks leave
     their request body byte-identical (the ignore-when-absent direction).
     Turn loop: blocks ride the right assistant messages (tool round,
     continuation, plain answer) and a cancelled round sends none. Golden:
     one new `thinking-passback.json` pins the request field (the second
     round's assistant message carries the blocks, the first carries
     none); the eight existing goldens are untouched.
   - **Docs.** `provider-protocol.md` §5 (the assistant `thinking` field and
     the opaque-payload/unmodified rule), §6 (the `thinkingBlock` chunk),
     §9 (the additive v2 entry); README's plugin-authoring bullet and the
     Anthropic notes.

   **As built (session 2, 2026-11).** The passback is live on all three
   providers; the wire changed and the existing goldens still pass
   unchanged (their requests carry no blocks, and the ignore-when-absent
   direction is byte-identical everywhere).

   - **Anthropic arm** (`adapter.rs`). `build_request_body` now emits an
     assistant message's `thinking` blocks as content blocks, in wire
     order, **before** the text and `tool_use` blocks — the spike's
     finding 6 (blocks after `tool_use` silently disable thinking) is
     load-bearing here. The mapping is identity: a `thinking` block goes
     out as `{"type":"thinking","thinking":<text>,"signature":<sig>}` with
     the empty text kept (the `display: "omitted"` mainline) and a
     `redacted` block as `{"type":"redacted_thinking","data":<data>}`.
     Nothing is filtered, reordered, or rewritten: Anthropic bills and
     keeps only what it chooses, so over-sending is free (item 6).
   - **DeepInfra arm** (`provider.rs`). `openai_messages` emits the
     blocks' concatenated text as `reasoning_content` on the assistant
     message, and nothing else — the re-measurement showed
     `thinking_blocks`/`reasoning_details`/`thinking` are silently
     ignored (+0 prompt tokens) there, and `reasoning` is an exact alias,
     so one field carries everything. A message with no text-bearing
     blocks (signature-only, redacted-only) emits no field at all: an
     empty `reasoning_content` would be a forged field, and nothing to
     send must be byte-identical to nothing.
   - **LiteLLM arm** (`provider.rs`). Same `reasoning_content` text, plus
     `thinking_blocks` — the finished blocks in LiteLLM's documented
     request shape (`{"type":"thinking","thinking":…,"signature":…}` /
     `{"type":"redacted_thinking","data":…}`) — because an
     Anthropic-backed model through the proxy is the case LiteLLM
     documents as a 400 without it. A proxy fronting anything else
     ignores unknown fields, so sending both is safe.
   - **Live acceptance checks**, both gated behind an env var so CI stays
     offline-green (`M23_LIVE_DEEPINFRA=1`, `M23_LIVE_ANTHROPIC=1`):
     DeepInfra asserts the observed accounting (passing a block back
     raises the follow-up's `prompt_tokens` by ≈1 token per 4–5
     characters — measured +221 for a 781-char block, +4400 for a ~19k
     one — with a same-request replay proving the delta is the passback
     and not noise); Anthropic drives a real tool loop and asserts the
     spike's degradation signal inverts: round 2's `message_delta`
     carries `output_tokens_details` (present) when the history had its
     blocks, and the field is absent when the same history is sent
     blockless — the silent-degradation bug closed on the wire.
   - **Docs.** `provider-protocol.md` §5's passback paragraph now records
     the per-plugin wire behavior as built; README's DeepInfra, LiteLLM
     and Anthropic notes describe what each provider now sends back, and
     the cost-report paragraph that promised "thinking never inflates a
     later prompt" is replaced by a pointer to the measured cost models
     (the full item-7 rewrite is session 4's job).

   **As built (session 3, 2026-11).** The passback payload now survives a
   quit: a resumed session rebuilds the identical message list a live
   session held, blocks included. No wire change — the field existed
   already; what changed is that it is no longer empty after `/resume`.

   - **`Record::Thinking` gained `blocks: Option<Vec<ThinkingBlock>>`**
     (`crates/clanky/src/session.rs`), `#[serde(default,
     skip_serializing_if = "Option::is_none")]`. A blockless round writes
     `{"type":"thinking","text":"…"}` exactly as before, so existing files
     load unchanged and new blockless files are byte-identical to the old
     shape (pinned by a test). The blocks are stored **verbatim** — the
     record is a replay source, not a repair step (constraint 1), and a
     `thinking` record whose round was cut short carries none (constraint
     2's "signature or nothing", matching the turn loop's rule for a
     cancelled or errored round).
   - **`history_from_records` folds, it does not walk positionally.** A
     round is recorded thinking-first then assistant, so the `thinking`
     record's blocks ride along in a `pending_blocks` slot and are attached
     to the next `Assistant` record — the message of their own round —
     instead of becoming a message of their own. Any other record
     (`user`, `usage`, `error`, `stopReason`, `settings`, `toolResult`)
     clears the slot, so blocks can never leak onto a later round even in a
     hand-edited file. A `thinking` record that precedes no assistant
     message (a round cut before any text) attaches to nothing and invents
     no message. The synthetic-result repair for an interrupted call is
     untouched and keeps working with blocks present.
   - **Writers.** `recorder::record_turn_event` writes the round's
     `thinking_blocks` onto the record; `Recorder::flush_partial` (the
     error/ctrl+c path) writes `blocks: None`, like the turn loop. The TUI's
     `screen_records` (an export from the screen) likewise writes `None`:
     the screen keeps no payload, and a screen export is a rendering, not a
     resume source.
   - **TUI `restore`.** A `thinking` record with empty text (the
     `display: "omitted"` mainline, where the signature is the whole
     payload) adds no transcript entry — nothing was printed for it live
     either — while its blocks still reach the rebuilt history. This keeps
     a resumed screen identical to the live one instead of printing an
     empty thinking block.
   - **Tests.** Session unit: blocks round-trip through the file and fold
     onto their own round, a blockless round sends none, a cut round
     attaches to nothing, a separating record breaks the fold, and the
     field is omitted when absent (the pre-M23 shape still parses).
     Recorder: a round's blocks land on its `thinking` record, a blockless
     round writes the old shape, a flushed partial round writes none. TUI:
     `restore` renders text-bearing thinking and adds no entry for a
     signature-only record. Integration (`tests/sessions.rs`): the full
     exit criterion — a live turn whose rounds carry a signature-only and a
     `redacted_thinking` block, killed and reloaded, rebuilds a history
     `==` the live one, and the next request carries the same blocks
     verbatim; plus a synthetic-result repair on a block-carrying
     interrupted call.
   - **Docs.** README's `/resume` section states that the restored history
     is exactly what the model saw (blocks replayed verbatim), and both it
     and `/md --thinking` now carry the plain warning that **a session file
     is trusted input** — model-authored reasoning is authoritative
     context, so a hand-edited file or export becomes an instruction
     channel (constraint 6). `provider-protocol.md` §5 records persistence
     as part of the field's contract.

   **As built (session 4, 2026-11).** The knob and the honest cost docs.
   The passback window is one decision made in one place — the turn loop,
   before each request is built — so no plugin filters and the wire is a
   faithful picture of it.

   - **`thinking_passback = "off" | "previous-turn" | "all"`**
     (`settings.rs`), defaulting to `previous-turn` via
     `DEFAULT_THINKING_PASSBACK`. It goes through the full settings
     machinery: layered per field like every other key, validated when a
     settings file is read (a fixed vocabulary, so a typo is a load error
     naming the accepted spellings rather than a silently ignored value),
     carried in `SessionSettings` and therefore in the startup dump
     (`thinking_passback previous-turn`) and the session `settings`
     records, and exposed on the CLI as `--thinking-passback` plus the
     TUI's `/thinking-passback` (no argument reports the value; the doc
     asked for no picker, and a four-value vocabulary does not need one).
   - **The window is a prefix drop of whole turns, enforced in
     `turn::passback_window`.** `turn_start` is captured once before the
     loop, so "this turn" is known however many rounds the turn takes.
     `previous-turn` keeps the current turn's rounds in full and drops
     earlier turns'; `all` sends everything; `off` clears every message's
     blocks, which makes the request byte-identical to a blockless one
     (no empty `thinking` field — pinned by a test that greps the
     serialized messages). Trimming happens on a copy: `messages` keeps
     the full payload, so the records and a later `/resume` still see it.
     **This inverts the original item 8 text, as the spike's finding 6
     required**: dropping "the oldest blocks" of the *latest* assistant
     message is a 400, so there is no such thing as a per-message window —
     only whole turns may go, and the intra-turn run is never touched.
     That is also why intra-turn passback is not switchable: it is the
     correctness half. **Corollary worth stating plainly (the docs do):**
     of item 8's four measured rows, only two are reachable — "previous
     turn only" would drop the tool loop's own rounds and "immediately
     previous round only" would trim the newest run, so the knob offers
     `previous-turn` (the default, which *is* the measured "current turn
     only" policy, +56%) and `all` (+73%), with `off` at zero. The +14%
     row is a policy the API will not accept, not a cheaper default.
   - **`off` on Anthropic is honored, not overridden.** It means thinking
     degrades after the first tool round of a turn; the invariants say a
     user's setting wins over the program's opinion, and the README says
     what the trade is. A settings-file typo is likewise a load error
     naming the accepted spellings, and an unknown `--thinking-passback`
     value is a clap usage error — verified by hand end to end (the dump
     reads `… · thinking_passback previous-turn` for a settings file and
     `all` for the flag).
   - **Tests.** `turn.rs`: the default keeps a turn's rounds and drops an
     earlier turn's; `all` keeps both; `off` sends none while the history
     keeps all three rounds; `off` leaves the request otherwise identical
     (no `thinking` key anywhere); a multi-round turn emits an unbroken
     run. `settings.rs`: parse + default, a bad value is a load error,
     per-field layering, and the value reaching the dump and the record
     view. `cli.rs`: the flag maps to settings and rejects an unknown
     value. TUI: `/thinking-passback` sets, reports and rejects, and the
     palette lists it. Golden: a new `thinking-passback-off.json` pins the
     whole path — the dump, the session `settings` record with
     `thinkingPassback: "off"`, no `thinking` key on any request, and the
     history keeping the blocks. `tests/sessions.rs`'s resume test now
     asserts both sides: the default drops a resumed earlier turn's blocks
     from the next request, `all` replays them verbatim.
   - **`session-cost.py` (item 7).** The "never inflates a later prompt"
     rule is gone. The reporter now attributes passed-back thinking to the
     prompt, with **the two providers' cost models stated separately**:
     a block carrying a `signature` (or a redacted block's `data`) is the
     Anthropic shape — text free (measured identical `input_tokens` with
     it emptied or padded), signature at ~0.15 tok/char, expressed as
     0.15 × 4.49 ≈ 0.67 character-equivalents; a text-only block is the
     DeepInfra/DeepSeek shape at the measured 4.49 chars/token. Which
     rounds' payload reaches a given prompt follows the session's own
     recorded policy (with `--thinking-passback` as a what-if override),
     and a round's own blocks never inflate its own prompt — they join the
     carried set when its `usage` record lands. The docstring, worked
     example and README paragraph were rewritten with the measured
     magnitudes, the two unreachable rows marked as such, and cache-read
     pricing plus Anthropic's server-side filter named as the two things
     that keep the cost affordable. The selftest gained a regression
     fixture: `selftest_passback` hand-checks all three policies round by
     round and fails if passback stops adding prompt tokens, so the old
     claim cannot creep back.
   - **Docs.** README: the `settings.toml` table row and a note on the
     knob, a `/thinking-passback` section, and the cost-report section
     rewritten around the measured table. `provider-protocol.md` §5
     records that the client owns the window and that a run inside one
     message is never trimmed.

1. **[x] Capture the signature (both transports).** `AnthropicBlock` gains
   `signature: Option<String>` and `data: Option<String>` (the
   `redacted_thinking` payload); `StreamDelta` gains `signature:
   Option<String>` and the `signature_delta` arm joins the match
   (`adapter.rs:489`). The streaming state accumulates per-block signature
   by wire index — the existing `blocks: HashMap<u32, String>` holds only a
   type, so it grows into a small per-block struct carrying the type, the
   accumulated signature, and (for redacted blocks) the data. Non-streaming
   `response_to_events` reads them off the block. `redacted_thinking` must
   reach the client too, not be dropped.

2. **[x] Carry it through the protocol.** `ChatMessage::Assistant` gains
   `#[serde(default, skip_serializing_if = "Option::is_none")] thinking:
   Option<Vec<ThinkingBlock>>`, with `ThinkingBlock { kind: thinking |
   redacted, text: Option<String>, signature: Option<String>, data:
   Option<String> }`. Ordering within the vector is the wire order — the
   spec requires the sequence of consecutive thinking blocks in the latest
   assistant message to match what the model generated. **Additive and
   optional, so no protocol version bump** (spec §9); it joins the
   `images`/`continuable` precedents in the version history. Providers that
   do not use it ignore it.

3. **[x] Stream it into the assembler.** `ChunkPayload` gains
   `ThinkingBlockStart { index, kind }` / `ThinkingBlockEnd { index,
   signature?, data? }`, or — simpler and preferred — one terminal
   `ThinkingBlock { kind, text, signature, data }` chunk emitted when the
   block closes. Pick the shape that keeps `StreamAssembler`'s "assemble
   deltas, hand back a finished round" model intact; `Thinking` deltas stay
   exactly as they are so display, `TurnEvent::Thinking`, and the session
   record do not change. `TurnEvent::Round` gains the assembled blocks
   beside `thinking` (the 20 `TurnEvent::Round` literal sites and 30
   `ChatMessage::Assistant` sites are the blast radius).

4. **Send it back.** The turn loop attaches the round's blocks to the
   assistant message it pushes for the next round (`turn.rs:531`) and for a
   `Continuable` resume (`turn.rs:511`). A round cut short (error or cancel)
   sends no blocks — partial signatures are unusable, and the existing
   partial-round flush already drops tool calls for the same reason.

5. **[x] Session fidelity.** `Record::Thinking` gains optional
   `blocks: Option<Vec<ThinkingBlock>>` (defaulted, so existing files load),
   and `history_from_records` stops skipping thinking: it must fold a
   round's `Thinking` record back onto the preceding assistant message
   rather than emitting a message of its own. This is the delicate part —
   records are written thinking-first then assistant (`recorder.rs:375`),
   and `history_from_records` currently walks them positionally. `/resume`
   must reconstruct exactly what the model saw, the same standard the
   `ToolResult.images` field is held to.

6. **[x] Provider behavior, per plugin.**
   - **Anthropic — send everything.** Emit the blocks as content blocks in
     wire order, before the text block, exactly as received. Never
     reconstruct, reorder, or rewrite them. Send *all* prior turns' blocks,
     not a window: the API filters and bills only what it keeps ("the API
     automatically filters them, keeps the blocks needed to preserve the
     model's reasoning, and bills input tokens only for the blocks actually
     shown to Claude"), so over-sending on Anthropic is free and
     under-sending is what costs quality. This is the one provider where the
     passback window in item 8 does not apply.
   - **DeepInfra / LiteLLM — `reasoning_content` (concatenated text)** on the
     assistant message when present. The measured token accounting above is
     the expected effect, not a bug. Note the field findings from the
     re-measurement: DeepInfra treats `reasoning` as an exact alias of
     `reasoning_content` (both counted) but **silently ignores
     `thinking_blocks`, `reasoning_details` and `thinking`** (measured +0
     prompt tokens), so send `reasoning_content` and nothing else there.
     LiteLLM additionally accepts `thinking_blocks`; send it too so an
     Anthropic-backed model through the proxy gets the same fidelity — that
     is the case LiteLLM documents as a 400 today.
   - **DeepSeek native** requires the same `reasoning_content` passback
     whenever `tools` is present (400 otherwise). The OpenAI-compatible path
     already sends it, so this comes for free; the only requirement is that
     the field is not gated on `tools` being absent.
   - Providers that ignore unknown fields lose nothing.

7. **[x] Budget accounting must stay honest.** `session-cost.py` and the README
   section it documents currently state, as a fact, that "Thinking is
   completion-side only... it is never sent back, so it costs once and never
   inflates a later prompt" (`README.md:403-407`, `session-cost.py:33-38`).
   After this milestone that is false: passed-back thinking is input on
   every later round. The reporter's `context` model (`session-cost.py:145`)
   and its "drop thinking from prompt shares" rule (`:210-212`) both need to
   attribute passed-back thinking to the prompt, and its module docstring,
   worked example, and the README paragraph all need rewriting.

   **Write the replacement with the measured magnitudes, not a vague
   warning.** The honest sentence is that passback multiplies the prompt by
   roughly `1 + thinking_chars / (4.49 × prompt_tokens)`, that in clanky's
   own saved sessions this came to **+56% for the current turn alone and
   +73% for all turns**, that the intra-turn share is **76% of that** and so
   is unavoidable wherever the provider requires it, and that the cached
   input rate is what keeps it affordable. A passback that inflates cost
   while the docs promise it does not is exactly the kind of silent surprise
   the project invariants exist to prevent; a doc that says "it may cost
   more" without the factor is only marginally better.

8. **[x] Make it controllable, because it costs money — and split the knob.**
   The re-measurement shows one boolean would conflate two different
   questions:

   - **Intra-turn passback (the tool loop)** is *correctness*. Anthropic and
     DeepSeek require it; omitting it is the silent-degradation bug this
     milestone exists to fix. Keep the same functionality regardless of provider
     or model. No magic override of the provided settings. If a user selects
     "off" for Anthropic, honor it.
   - **Cross-turn passback** is a *cost* decision, and the measured numbers
     say it is the cheap half: the immediately-previous-round policy costs
     **+1%** and the previous-turn policy **+14%**, against the +56% already
     spent intra-turn. Recommendation: default to **previous turn** (captures
     the measured continuity benefit and the whole tool-loop story for +14%
     rather than +73%).

   **Model it as a window, not an on/off.** Anthropic documents that you may
   "leave out the oldest blocks, the newest, or all of them, but none in
   between", and that "once a block is out, keep it out" — so the only legal
   trimming is a prefix drop. Use
   `thinking_passback = "off" | "previous-turn" | "all"` (default
   `previous-turn`) and, whatever is chosen, keep the emitted sequence an
   unbroken run: never drop a block from the middle. The setting must appear in the
   effective-settings dump, the session `settings` records, and README, like
   every other knob — and `off` must be a real off (send no blocks at all,
   not an empty field).

### Design constraints

1. **Never modify a thinking block.** No trimming, no re-wrapping, no
   re-encoding, no reordering, no deduplication. Anthropic rejects altered
   blocks with a 400 and binds signatures to a conversation
   (`Invalid signature in thinking block. The block is bound to a different
   conversation`), so the only correct transformation is identity. This
   extends the project invariant from user-supplied values to
   provider-supplied ones: both are authoritative.
2. **Signature or nothing.** If a round's signature was not captured, send
   no block for that round rather than an unsigned one. A missing block is
   documented as allowed outside tool use and degrades to today's behavior;
   a forged one is a 400.
3. **Text is optional, signature is not.** Handle empty-text blocks as the
   normal case, not an edge case — they are the default on current models,
   and on the configured `claude-haiku-5-5` (display defaults to
   `"omitted"`) the empty-text case is the *only* case. The measured
   DeepInfra behavior is the mirror image: there the text is the payload and
   there is no signature at all, so the protocol field must carry both
   without either being required.
4. **The conversation stays valid.** Item 5 must not produce an assistant
   message whose blocks disagree with its text/calls, and the existing
   synthetic-result repair for interrupted tool calls must keep working.
5. **Additive protocol change.** Optional field, no version bump, both
   directions ignore it when absent; update `provider-protocol.md` §5 field
   rules and §9 version history in the same change, per the spec's own
   policy.
6. **Resumed history is only as trustworthy as the file.** Passed-back
   reasoning is model-authored text that the model treats as authoritative
   context — the injection experiment above is a working proof (a French,
   Tokyo-flavored answer to a Paris question). clanky writes thinking to
   session files and exports it to Markdown (`/md --thinking`), so a
   hand-edited export resumed with `/resume` becomes an instruction channel
   the model will follow. This is not a reason to withhold passback, but the
   README section for `/resume` and `/md --thinking` should say plainly that
   a session file is trusted input, and item 5 must not invent or repair
   blocks to make a file load.

### Done when

A tool loop against Anthropic sends each round's thinking blocks back
verbatim (signature intact), the follow-up request shows thinking still
enabled rather than silently degraded, a `redacted_thinking` block survives
a round trip, `/resume` replays a session into the same message list a live
session had, DeepInfra and LiteLLM receive `reasoning_content` /
`thinking_blocks` and their token accounting reflects it, the cost reporter
attributes passed-back thinking to the prompt, and README plus
`provider-protocol.md` describe the new behavior. Because the new field is
`skip_serializing_if = "Option::is_none"`, the existing goldens
(`tests/golden/`) do not change unless a scripted turn emits blocks — add
one such turn (the two scripts that already emit `ChunkPayload::Thinking`,
`golden.rs:409` and `:731`, are the natural places) so the field is pinned
in a request, and confirm the untouched goldens still pass unchanged. The
protocol and plugin tests cover the ignore-when-absent direction for every
provider.

**Measured acceptance checks, added by the 2026-11 re-measurement:**

- A live DeepInfra round-trip asserts the *observed* accounting rather than
  just the field's presence: with a ~1800-character reasoning block, a
  request whose assistant message carries `tool_calls` shows prompt tokens
  up by **≈400** on `openai/gpt-oss-20b`, and one carrying `tools` shows the
  same on `deepseek-ai/DeepSeek-V4-Flash-0731`. Both are the expected
  effect; assert them so a future refactor that silently stops sending the
  field fails a test instead of quietly changing behavior.
- A test pins the field-name asymmetry: `reasoning_content` and `reasoning`
  are counted, `thinking_blocks` / `reasoning_details` / `thinking` are
  ignored by DeepInfra. This is the fact that makes the LiteLLM
  `thinking_blocks` bullet provider-specific rather than universal.
- `thinking_passback = "off"` is exercised end to end on both an
  OpenAI-compatible provider and Anthropic: no blocks on the wire, the
  request otherwise byte-identical, and the setting visible in the
  effective-settings dump and the session `settings` record.
- The window policy is exercised on a three-round tool loop: the default
  (`previous-turn`) drops the oldest round's blocks but keeps the current
  turn's, and the emitted run stays unbroken.
- `session-cost.py` gains a regression fixture whose expected prompt
  attribution includes passed-back thinking, so the "never inflates a later
  prompt" claim cannot creep back into the docs while the code does
  otherwise.

## ✔ M24 — OpenRouter provider plugin

New provider plugin `crates/clanky-provider-openrouter` (binary
`clanky-provider-openrouter`), talking to OpenRouter's OpenAI-compatible
`POST /api/v1/chat/completions`. Read env `OPENROUTER_API_KEY` (required;
`sk-or-v1-…`), optional `OPENROUTER_BASE_URL` (default
`https://openrouter.ai/api/v1`), optional `OPENROUTER_HTTP_REFERER` /
`OPENROUTER_TITLE` for the app-attribution headers. Provider name
`openrouter`; default model advertised at handshake
`deepseek/deepseek-v4.1-flash` (see "Default model" below). Same shape as
M10's LiteLLM plugin: `clanky_protocol::serve_stdio` does the wire, the
adapter is the work, and every behavior is pinned to captured fixtures.

Docs reviewed: `api_reference/overview.md`, `api_reference/models.md`,
`api_reference/streaming.md`, `api_reference/errors-and-debugging.md`,
`api_reference/authentication.md`, `api_reference/limits.md`,
`guides/best-practices/reasoning-tokens.md`, the chat-completions page in
the OpenAPI spec (`openapi.yaml`), and a live `GET /api/v1/models` dump
(478 models, 2026-11).

### What the docs say that changes the design

1. **Reasoning has its own request shape, and `reasoning_content` is not
   documented for requests.** Chat Completions exposes a top-level
   `reasoning` object (`effort`, `max_tokens`, `enabled`, `exclude`,
   `summary`, `mode`, `context`) plus a `reasoning_effort` shorthand, and
   `reasoning_details` as the structured passback. The prose says
   "`reasoning_content` … functions identically to `reasoning`", but the
   OpenAPI schema for `ChatAssistantMessage` lists only `reasoning` and
   `reasoning_details` — `reasoning_content` appears in **zero** of the
   chat-completions schema sections. So the DeepInfra/LiteLLM passback
   field is an undocumented alias here; the documented one is
   `reasoning_details`.
2. **`stream_options.include_usage` is deprecated and has no effect** — full
   usage is always returned. Sending it is harmless; the plan is to not send
   it and pin the always-on behavior with a fixture instead (see decision 6).
3. **The terminal usage chunk deliberately deviates from OpenAI**: it
   carries one choice with a content-free delta that *repeats*
   `finish_reason`. A strict "empty choices means usage frame" parser would
   be wrong here; the existing accumulator already ignores content-free
   deltas and a repeated finish reason, so this is a pin-with-a-test item,
   not a code change.
4. **Mid-stream errors arrive as a 200 with a top-level `error` object** and
   a choice whose `finish_reason` is `"error"`. `finish_reason` values are
   normalized to `tool_calls` / `stop` / `length` / `content_filter` /
   `error`. A plugin that maps unknown finish reasons to `stop` would
   silently turn a provider failure into a successful empty answer.
5. **`402` exists and is not one thing.** Credits exhausted
   (`openrouter_credits`, `openrouter_key_limit`) is a hard stop; the
   in-flight budget variant (`openrouter_in_flight_budget`) carries a
   `Retry-After` and *is* transient. `503` (no provider meets the routing
   requirements) and `429` also carry `Retry-After`. The docs note no SDK
   retries a `402` on its own, so the `Retry-After` header is the only
   signal that distinguishes the two cases.
6. **The catalog is rich but full of sentinels.** `pricing` values are
   **strings** (`"0.000003"`), some entries carry an `overrides` array
   (87 of 478 models: prompt-length tiers, UTC time-of-day tiers,
   `input_cache_write_1h`), and two sentinels mean "not a normal price":
   `-1` for dynamic routers (`openrouter/auto`, `openrouter/fusion`, 8
   models) and `0` for `:free` variants. 92 catalog entries are `:batch`
   and are **not** on `/api/v1/chat/completions` at all; 19 are `~`
   rolling aliases; 60 chat models have no `tools` in
   `supported_parameters`. Context window is `context_length` (per model),
   not `top_provider.context_length` (per endpoint).
7. **Per-model reasoning metadata exists** (`reasoning.supported_efforts`,
   `default_effort`, `default_enabled`, `mandatory`, `supports_max_tokens`)
   on 356 of 478 models — 151 of them `mandatory: true`, meaning the model
   **rejects** `effort: "none"`.
8. **`/api/v1/models` is public and unfiltered; `/api/v1/models/user` is
   the key-scoped view.** The first returns 478 models including ones this
   key cannot call (guardrails, provider preferences, privacy settings).
9. **`HTTP-Referer` / `X-OpenRouter-Title` are optional app-attribution
   headers**; they change nothing functionally and are the one place a
   plugin may identify itself.

### Decisions to settle before implementation

1. **Shared OpenAI-compatible core — yes, and as its own step (M24a).**
   The duplication is measurable, not stylistic. `backend.rs` is **85%
   identical** between the DeepInfra and LiteLLM plugins, ignoring comments
   (`BackendError`, `parse_retry_after`, `parse_http_date`, `UreqBackend`,
   `map_ureq_error`, the whole SSE framer, `truncate_body` and all of its
   tests) — and the Anthropic plugin carries a **third** copy of the SSE
   framer plus a third copy of the HTTP-date `Retry-After` parser
   (`backend.rs:82`, differing from the other two only in `i64` vs `u64`).
   In `provider.rs` the picture is different and worth stating honestly:
   only **16% of the non-comment lines** are verbatim-identical, but the
   *same blocks* are present in all three plugins — the seven
   `OpenAi*` chunk structs, `StreamState` + `CallAccumulator` (with its
   hard-won index/args-before-id/oversized-index handling), `args_string`,
   `MAX_TOOL_CALL_INDEX`, `map_finish_reason`, `openai_tools`,
   `role_name`, `reasoning_content`, `image_parts_user_message`, and
   `backend_to_protocol_error`'s status→`ErrorCode` mapping. They differ
   by doc comments and by a handful of provider-specific lines
   (LiteLLM's extra `thinking_blocks`, LiteLLM's error-envelope
   unwrapping), which is precisely why the shared crate should own the
   *mechanism* and leave the *policy* in each plugin. A fourth copy is not
   defensible. Extract a
   `crates/clanky-provider-openai` library crate holding: the `ureq`
   backend + SSE framer + `Retry-After` parsing + `truncate_body`; the
   OpenAI stream-chunk types and `StreamState`; and the message/tool
   serializers. Each plugin then keeps only what is genuinely its own
   (URL joining, catalog parsing, error-envelope unwrapping, thinking
   mapping). Do this **as a separate, mechanically verified step** (see
   M24a) so the refactor's only acceptance criterion is "all existing
   tests and goldens pass unchanged" — mixing it into new-provider work
   makes a regression impossible to attribute.
2. **`reasoning_details` is the passback field, not `reasoning_content`.**
   The protocol's `ThinkingBlock` maps cleanly onto OpenRouter's
   `reasoning_details` union: `Thinking` → `{"type":"reasoning.text",
   "text":…,"signature":…}`, `Redacted` → `{"type":"reasoning.encrypted",
   "data":…}`. OpenRouter also carries `id`, `index` and `format` on each
   detail; those are not in the protocol's `ThinkingBlock`. Two options:
   (a) send `reasoning_details` with only `type` + payload, letting
   OpenRouter fill the rest; (b) extend `ThinkingBlock` with optional
   `id`/`index`/`format` (additive, no version bump) so blocks round-trip
   verbatim as the docs require ("the entire sequence … must match the
   outputs generated by the model … you cannot rearrange or modify").
   Recommendation: **start with (a), and capture a live round-trip to see
   whether OpenRouter's verifier accepts a block missing `id`/`format`
   before deciding (b) is needed.** Do not send `reasoning_content` as
   well "to be safe": unlike DeepInfra, an extra field here is an
   undocumented one, and the project rule is to send what the provider
   documents.
3. **Thinking maps to `reasoning.effort`, with the budget as the source.**
   Reuse the budget→effort ladder (`0`→`none`, then the rungs), but note
   the semantics differ from DeepInfra: OpenRouter's rungs are *fractions
   of the output token limit* (`max`/`xhigh`≈95%, `high`≈80%,
   `medium`≈50%, `low`≈20%, `minimal`≈10%), not absolute budgets. That is
   a lossy but documented translation, and it is what every other client
   does. **`mandatory: true` models reject `effort: "none"`** — per
   invariant 2 the plugin still sends what the user asked for and surfaces
   the 400; the catalog hint exists so the `/model` picker can show it,
   not so the request can be rewritten. `--thinking <budget>` therefore
   keeps its meaning ("how hard to think"), and README must say the
   mapping is a fraction of the output limit here. Note also that the
   ladder is not universal: 240 of 478 catalog entries expose no
   `supported_efforts` list at all, `minimal` is absent from 196 models'
   lists, and `none` from 122. Sending an effort the model does not list
   is a 400; that is the documented, actionable answer, so nothing is
   clamped — but it does mean `--thinking off` is only reliable on models
   whose `supported_efforts` includes `none` (or which list no efforts and
   are non-`mandatory`), and README should say so.
4. **Default model: `deepseek/deepseek-v4.1-flash`.** Cheap
   ($0.30/$1.20 per Mtok, cache read $0.006), 1M context, tools,
   non-mandatory reasoning (so `--thinking off` is legal), and priced in
   the same band as the DeepInfra plugin's default
   (`zai-org/GLM-5.3-Flash`), which keeps a cross-provider comparison
   meaningful. Note that "match DeepInfra's default model" is *not* an
   option here: the same GLM 5.3 Flash is `mandatory: true` on OpenRouter,
   so it would reject `--thinking off`. The alternatives are
   `z-ai/glm-5.3-flash` (cheaper, mandatory — a bad default for a plugin
   whose README documents the off switch) and
   `anthropic/claude-haiku-4.5` (10× the input price, 200k context, no
   effort ladder exposed). Whatever is chosen goes in the handshake (core
   keeps no provider table) and in README.
5. **Catalog: `/api/v1/models/user`, not `/api/v1/models`.** The public
   catalog is 478 models including ones this key cannot call; the
   key-scoped one is what a picker should show. Both are optional-auth, so
   the plugin sends its key either way. Filter the picker by what the
   model can actually do: drop `:batch` entries (no chat endpoint),
   drop entries without `/api/v1/chat/completions` in `endpoints`, and
   flag `supportsTools: false` when `tools` is missing from
   `supported_parameters` (the LiteLLM precedent: hints curate the
   *list*, never the request). Keep `~` aliases and `:free` variants —
   they are legitimate user choices.
6. **`include_usage` is not sent; usage is asserted instead.** Since the
   docs say the field is deprecated and ignored, sending it is noise. Pin
   with a fixture that the final usage chunk arrives with no
   `stream_options` in the request, and that its content-free choice does
   not confuse the accumulator.
7. **Errors: `402` is the interesting one.** `401`/`403` → `auth`, `429` →
   `rateLimit` (with `Retry-After`), `400`/`404`/`422` → `invalidRequest`,
   `502`/`503` → `backend` (retryable; `503` carries `Retry-After`).
   `402` is two cases wearing one status: credits exhausted
   (`openrouter_credits`, `openrouter_key_limit`) is a hard stop that no
   retry fixes, while the in-flight budget
   (`openrouter_in_flight_budget`) carries `Retry-After` and is transient.
   The docs are explicit that "none of the SDKs retries a `402` on its own",
   so the rule is: **retryable only when a `Retry-After` is present**, and
   map it to `rateLimit` in that case, `auth` otherwise — with the
   provider's own message either way. The protocol's `retryable` hint
   already expresses exactly this and core already honors `retryAfterMs`,
   so no protocol change is needed. And **mid-stream errors must not be
   swallowed**: a chunk with a top-level `error` object, or a
   `finish_reason` of `"error"`, terminates the stream with a
   request-scoped error carrying the provider's `message`, not a `done`
   with `stop`.
8. **App attribution headers are opt-in and pass through exactly.** Read
   `OPENROUTER_HTTP_REFERER` and `OPENROUTER_TITLE` verbatim; send nothing
   when unset. No invented default (no "clanky" title): that would be the
   plugin deciding something the user did not.

### Steps

#### ✔ M24a — Extract `clanky-provider-openai` (refactor only, no new behavior)
- New workspace member `crates/clanky-provider-openai`, a library (no
  binary): `backend` (the `ureq` agent, `Backend`/`BackendError`, SSE
  framer, `Retry-After` parsing in both forms, `truncate_body`) and
  `stream` (the `OpenAi*` chunk types, `StreamState`, `CallAccumulator`,
  `map_finish_reason`, `MAX_TOOL_CALL_INDEX`) and `wire` (`openai_tools`,
  `role_name`, `reasoning_content`, `image_parts_user_message`, the
  message serializer).
- Point DeepInfra and LiteLLM at it; **their public behavior must not
  change**. Their existing tests stay where they are and must pass
  unmodified — the refactor's acceptance criterion is exactly that, plus
  `cargo fmt`/`clippy`/`test` green. Keep each plugin's own doc comments
  (the measured DeepInfra notes, the LiteLLM `thinking_blocks` note) by
  keeping the provider-specific *policy* in the plugin and only the
  mechanism in the shared crate.
- Move the Anthropic plugin's SSE framer and `Retry-After` parser onto
  the shared ones too (its parser differs only in signedness; unify on
  one and keep its behavior — past dates clamp to 0).
- Also fold `main.rs`'s `Unavailable`/`clone_error` glue (DeepInfra and
  LiteLLM have near-identical copies) into a shared
  `clanky_protocol::serve_unavailable(...)` helper or a small
  `clanky-provider-openai` entry point.
- Done when: three plugins build against the shared crate, all workspace
  tests and `tests/golden/` pass with **no fixture or golden edits**, and
  the diff of the moved code is byte-identical modulo imports.

  **As built (M24a).** `crates/clanky-provider-openai` exists as a library
  with exactly the three planned modules: `backend` (`Backend`,
  `BackendError`, `UreqBackend`, `sse_payloads`, `parse_retry_after` /
  `parse_http_date`, `now_secs`, `truncate_body`), `stream` (`OpenAi*` chunk
  types, `StreamState`, `CallAccumulator`, `args_string`,
  `MAX_TOOL_CALL_INDEX`, `map_finish_reason`) and `wire`
  (`openai_messages`, `openai_tools`, `role_name`, `reasoning_content`,
  `image_parts_user_message`). The moved bodies are unchanged; only
  visibility (`pub`) and the doc comments that named a single provider were
  rewritten. The moved mechanism's own tests moved into the crate
  (`backend::tests` verbatim, plus `stream`/`wire` tests covering the
  accumulator rules and the passback callback); the plugins' own tests —
  DeepInfra 26, LiteLLM 30 — are the provider-specific ones and were not
  edited. The new crate starts at 19 tests; no fixture or golden changed.

  Two small decisions the plan left open, both settled toward "shared":
  - **`Backend::get_status` is on the shared trait**, with a default that
    assumes a `200` from `get`. LiteLLM's `/model/info` degradation needs
    it; DeepInfra's mock backend doesn't implement it and still compiles.
    The shared `UreqBackend` implements it once, so `get` is defined in
    terms of it rather than the reverse (same requests, same errors).
  - **The reasoning passback is a callback** (`openai_messages` takes a
    `&dyn Fn(&mut Map, &[ThinkingBlock])`). That is what lets DeepInfra
    keep "send `reasoning_content` and nothing else" and LiteLLM keep
    "`reasoning_content` *and* `thinking_blocks`" without either plugin
    reimplementing the message loop — the shared crate owns the mechanism
    and each plugin owns its policy, which was decision 1's whole point.

  The Anthropic plugin re-exports the shared `sse_payloads`,
  `parse_retry_after`, `now_secs` and `truncate_body`; its `Retry-After`
  wrapper now delegates to the unsigned shared parser. Behavior is
  preserved: negative and already-past dates still clamp to 0 (the shared
  parser saturates before the ×1000, the old one clamped after — same
  values), and its own three `retry_after_tests` and four `sse_tests`
  still pass unmodified. Its 81 tests are untouched.

  `main.rs`'s `Unavailable`/`clone_error` glue is now
  `clanky_protocol::serve_unavailable(name, default_model, provider)` (plus
  the public `clanky_protocol::Unavailable` for a plugin that needs to serve
  one directly). Both OpenAI-compatible plugin binaries are three lines of
  glue over it; the protocol crate gained three tests for it (handshake
  identity, per-request error, non-provider error → `internal`).

#### ✔ M24b — Capture, then the adapter
- Capture first (the LiteLLM M10 pattern), into
  `crates/clanky-provider-openrouter/tests/fixtures/`: `models_user.json`
  (key-scoped catalog), `stream_plain.txt`, `stream_reasoning.txt`
  (`reasoning_details` deltas), `stream_tools.txt`, `stream_usage.txt`
  (the duplicated-finish-reason usage chunk), `stream_error.txt` (the
  mid-stream `error` chunk), `err_401.txt`, `err_402.txt`, `err_404.txt`.
  A `dumps.sh` beside them, like `litellm-reference/dumps.sh`.
- Adapter: `from_env` (key required; missing key is an `auth` error at
  first request, plugin still handshakes — the DeepInfra pattern),
  `list_models` from `/models/user` with the picker filters above,
  `chat` streaming with the `reasoning` object built from
  `thinking.budget_tokens`, `reasoning_details` passback, tools, and the
  error mapping from decision 7.
- Model metadata: `display_name` from `name`, `context_window` from
  `context_length`, `supports_tools` from `supported_parameters`,
  `supports_thinking` from the presence of `reasoning` (+ `mandatory`),
  prices from `pricing.prompt`/`completion`/`input_cache_read` × 1e6
  (strings → f64; `-1` and `0` are sentinels, and the `overrides` tiers
  are *not* representable in `ModelInfo` v1 — document that the displayed
  price is the base tier).
- Done when: `clanky --provider openrouter -p "count files in /tmp"`
  runs a bash tool loop against the live API, streamed, and every fixture
  above is pinned by a test.

  **As built (M24b).** `crates/clanky-provider-openrouter` exists with the
  planned shape: `backend` (re-exports the shared `Backend`/`BackendError`/
  `UreqBackend`/`truncate_body`), `provider` (the adapter), a three-line
  `main.rs` over `clanky_protocol::serve_unavailable`, and 40 tests.
  Fixtures in `tests/fixtures/` with `dumps.sh` beside them, all captured
  live against `deepseek/deepseek-v4-flash-0731` unless noted:

  - `models_user.json` — the key-scoped catalog (248 models), trimmed to the
    fields the plugin reads so it stays reviewable. Every model is kept, so
    the picker filters run against the real `:batch` / `~` / `:free` /
    sentinel-price entries.
  - `stream_plain.txt` (`reasoning.effort: "none"`), `stream_reasoning.txt`
    (`reasoning_details` deltas), `stream_tools.txt` (reasoning → tool call),
    `stream_reasoning_signed.txt` (Anthropic via OpenRouter: text deltas then
    a signature-only detail). `stream_usage.txt` is the tail of
    `stream_plain.txt` (the repeated-finish-reason chunk plus the usage
    chunk) split out so the usage-frame rule is pinned on its own.
  - `err_400.txt`, `err_401.txt`, `err_429.txt` — real `curl -i` captures.
    The plan said `err_404`; an unknown model id is a **400** on this API
    ("… is not a valid model ID"), so that is what is captured. The 429 is
    the interesting one: `error.message` is the generic "Provider returned
    error" and the actionable text is in `error.metadata.raw`, so the plugin
    appends the raw text when it adds something.
  - `stream_error.txt` and `err_402.txt` are **not** live captures — see
    below.

  Three decisions the plan left open, and how the captures settled them:

  - **Decision 2 resolved to option (a): no new `ThinkingBlock` fields.** A
    signed Anthropic block sent as `{"type":"reasoning.text","text":…,
    "signature":…}` — no `id`, no `format`, no `index` — was accepted by both
    Bedrock and Google on a live round-trip, and the same block with an
    altered signature is a 400. So `type` + payload is enough, the block goes
    back byte-exact, and the protocol needs no `id`/`index`/`format`. The same
    holds for `reasoning.encrypted` on OpenAI/Grok.
  - **How much the API verifies varies by backend, and it is less than
    assumed.** Measured on `openai/gpt-6-luna`: an encrypted blob sent with
    `id`/`format` stripped, and even one whose tail was replaced with junk,
    both returned 200. Only the Anthropic signature is actually checked. The
    plugin sends payloads byte-exact anyway — we cannot validate them, so we
    must not alter them, and the provider may tighten its checks later.
  - **Mid-stream errors could not be captured live.** They need a provider to
    fail after the 200 is committed; every attempt (free-tier models, huge
    `max_tokens`, provider pinning, concurrent load) either succeeded or
    failed *before* streaming. `stream_error.txt` is therefore the documented
    shape from `api_reference/errors-and-debugging.md`, and the file says so.
    The same applies to `err_402.txt` (needs an account without credits); both
    documented 402 bodies are pinned, and the `Retry-After` header — the only
    thing that distinguishes the transient case — is what the mapping keys on.
  - **`reasoning.summary` items stream but are never passed back.** They are a
    display summary, not the reasoning payload; sending one back as
    `reasoning.text` would forge a payload the provider never produced. This
    was not in the plan and is the one place the adapter is more careful than
    the plan required. It turned out to be the *common* case on OpenAI and
    Grok, not an exotic one: `openai/gpt-6-luna` streamed 78 `reasoning.summary`
    deltas and one `reasoning.encrypted` blob, with no `reasoning.text` at all
    (`x-ai/grok-4.7` the same shape). So on those models the visible thinking
    is a summary and the passback is the blob — pinned by
    `stream_reasoning_summary.txt`, a live capture.
  - **The `mandatory` hint is read but not surfaced.** `supports_thinking`
    comes from the presence of `reasoning` (so `mandatory` models show as
    thinking-capable, which they are), but `ModelInfo` v1 has no field to
    carry "`effort: "none"` will be rejected", and inventing one is a
    protocol change M24 did not scope. README says so instead of claiming the
    picker warns.

  Live verification (all through the real plugin process): a plain turn, a
  one-round and a two-round bash tool loop, `--thinking off` (→ `reasoning.
  effort: "none"`, no `stream_options` in the body), `reasoning_details`
  present on round 2's assistant message, a mid-stream `error` chunk
  surfacing as `error: provider error: backend: 502: …` with the partial
  output kept, and both 402 cases (credits → `auth`, not retried; in-flight
  → `rateLimit` with backoff). `list_models` through the plugin returned 249
  models, no `:batch` entries, 44 tool-less models flagged, and no sentinel
  prices.

  One shared-crate change: `UreqBackend::with_headers` (and a private
  `authorize` helper) so a plugin can send extra headers verbatim.
  `UreqBackend::new` is unchanged, so DeepInfra/LiteLLM/Anthropic behavior
  and their tests are untouched — goldens included.

#### ✔ M24c — Docs, packaging, and the invariants pass
- README: a `### OpenRouter` section beside DeepInfra/LiteLLM/Anthropic
  (env vars, default model, the effort-as-fraction caveat, the
  `reasoning_details` passback, the `402` meaning, the picker filters,
  the `:batch`/`:free`/`~` distinctions), plus the built-in providers
  table and the `cargo install` line.
- `build-linux-musl`, `build-runner`, `docker/`, `AGENTS.md`'s workspace
  layout, and `Cargo.toml` members.
- `provider-protocol.md` gets one note: an OpenAI-compatible plugin sends
  its backend's documented passback shape, which may be
  `reasoning_details` rather than `reasoning_content` — the protocol
  field is the same `ThinkingBlock`, the wire shape is per provider.
- Re-read `design-docs/code-reviews/invariants-and-defaults-review.md`
  against the new plugin: no local validation, no silent drops, no
  rewritten user values (prices, budgets, model ids and app headers all
  pass through).

### Design constraints

1. **Send what the provider documents, not what the last plugin did.**
   `reasoning_content` is an undocumented alias on OpenRouter's Chat
   Completions schema; `reasoning_details` is the documented passback.
   Copying the DeepInfra line because it is one line shorter is exactly
   the kind of silent divergence this milestone's doc review exists to
   catch.
2. **A 200 is not a success.** OpenRouter reports mid-generation failures
   inside the stream. The plugin's `done` must only ever follow a real
   `finish_reason` that is not `error`; an `error` chunk (or a top-level
   `error` object on any chunk) becomes an `error` message with the
   provider's text.
3. **Sentinels are not prices.** `-1` (dynamic routers) and `0`
   (`:free`) must not be shown as "$-1.00 /Mtok" or "$0.00" in the status
   line and the picker; both mean "no fixed price", so the field is
   omitted and the cost display falls back to its no-pricing path.
4. **The picker filters, the request does not.** `supportsTools: false`
   hides a model in `/model`; `--model` can still name it, and the
   server's error answers. Same rule as LiteLLM, restated here because
   OpenRouter's catalog makes it tempting to filter the request too.
5. **`mandatory` reasoning is a server-side rule.** `--thinking off` on a
   `mandatory: true` model sends `effort: "none"` and surfaces the 400.
   The plugin may *hint* in the picker (a `mandatory` marker), never
   silently upgrade `none` to `low`.
6. **Prices are strings with tiers.** Parse `pricing.*` as `f64` from
   strings; ignore `overrides` for v1 and say so in README. Do not
   invent a "representative" tier.
7. **No invented identity headers.** `HTTP-Referer`/`X-OpenRouter-Title`
   are sent only when the user set them.

### Done when

`clanky --provider openrouter -p "say hi"` and a bash tool loop both work
against the live API with `OPENROUTER_API_KEY` alone; the shared
OpenAI-compatible crate exists and the DeepInfra/LiteLLM/Anthropic plugins
build on it with unchanged behavior (goldens untouched); the `/model`
picker lists the key-scoped catalog with context windows and prices, hides
`:batch` and tool-less models, and never shows a sentinel price; a
mid-stream provider error surfaces as an error rather than an empty
successful answer; a `402` with no `Retry-After` reads as a credits problem
and is not retried, while the in-flight-budget `402` (which carries one)
backs off and retries;
`thinking`/`--thinking off` map onto `reasoning.effort` (and the 400 on a
`mandatory` model comes through verbatim); reasoning round-trips through
`reasoning_details` in a tool loop; and README, `provider-protocol.md`,
`AGENTS.md`, the workspace layout and the release scripts all mention the
new plugin.

**As built (M24c).** README gained an `### OpenRouter` section beside
LiteLLM/Anthropic (env vars, the lossy effort-as-fraction caveat, the
`--thinking off` / `mandatory` caveat, the `reasoning_details` passback, the
`200`-is-not-a-success rule, the two `402`s, the picker filters, the
`:batch`/`:free`/`~` distinctions and the base-tier price caveat), plus the
provider table row, the `cargo install` line, and the shared-crate paragraph
now naming OpenRouter. `provider-protocol.md` §5 gained the per-backend
passback note (same `ThinkingBlock`, wire shape is the provider's) and
records OpenRouter's choice of `reasoning_details`. `AGENTS.md` lists the
crate and its env vars. `build-linux-musl`, `build-runner` and
`docker/runner.Dockerfile` carry the new binary. The invariants review
(`design-docs/code-reviews/invariants-and-defaults-review.md`) gained a
Part 3 re-read of the new plugin: no local validation, no silent drops, no
rewritten user values — prices, budgets, model ids and app headers all pass
through.
