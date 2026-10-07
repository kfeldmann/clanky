# Plan — Provider plugins (M8)

Extract the DeepInfra provider out of the `clanky` binary into a standalone plugin process, and make Clanky core provider-agnostic. This is milestone M8 (`milestones.md`): "Plugin system (providers)".

## Goal

After this change:

- The default experience stays zero-config: Clanky finds the `clanky-provider-deepinfra` binary on `$PATH` and runs it as a subprocess. Both binaries ship from the same workspace; how `cargo install` delivers them is a packaging detail (see §8).
- `clanky` core contains **no** provider-specific code. It owns the protocol client, the plugin discovery machinery, and the agentic loop; it knows nothing about HTTP, DeepInfra URLs, or API keys.
- Adding a provider means dropping a `clanky-provider-<name>` binary on `$PATH` — no change to Clanky, no recompile, no settings.
- A provider plugin in any language works, because the contract is JSONL over stdio (already specified in `provider-protocol.md`).

The protocol itself does not change. `provider-protocol.md` already describes the process transport (§1, §7, §8) and says M8's work "is confined to transport + lifecycle". This plan is about making that true in the code.

## Where we are today

The protocol layer is already split out and shaped for this:

- `crates/clanky-protocol/` — `messages` (wire types), `Transport` trait, `LoopbackTransport`, `ProviderClient`, `StreamAssembler`, plugin-side `Handler` trait.
- `crates/clanky-provider-deepinfra/` — `DeepInfraProvider<B: Backend>` implementing `Handler`; `UreqBackend` does blocking HTTP + SSE framing.
- `crates/clanky/` — the app. `provider.rs` is the only provider-aware seam:

  ```rust
  pub const DEFAULT_PROVIDER: &str = "deepinfra";
  pub fn available() -> &'static [&'static str] { &["deepinfra"] }
  pub fn create(name: &str) -> Result<Box<dyn Handler>> {
      match name {
          "deepinfra" => Ok(Box::new(clanky_provider_deepinfra::DeepInfra::from_env()?)),
          other => Err(Error::UnknownProvider(other.into())),
      }
  }
  pub fn default_model(name: &str) -> Option<&'static str> { ... }
  ```

  Call sites: `main.rs` (pipe mode), `tui/mod.rs` (`spawn_turn`, `spawn_models`, the `/provider` picker, `set_provider`), `turn.rs` (`TurnConfig::from_settings` for the default model).

The process transport, discovery, and lifecycle do not exist yet. Everything else (streaming, tool loop, retries, session storage, TUI) is already written against `ProviderClient`, not against DeepInfra.

## Design decisions

1. **Core never links a provider.** Drop the `clanky-provider-deepinfra` dependency from `crates/clanky/Cargo.toml`. `crates/clanky` keeps `clanky-protocol` only.
2. **One plugin = one long-lived process per session.** Spawn lazily on first use (settled open question #2), keep it alive for the whole session so startup and model-list cost is paid once, kill it on exit (its stdin EOF does this by contract, §1). Switching providers spawns the new plugin and shuts the old one down.
3. **Discovery is PATH-only.** Scan `$PATH` for executables named `clanky-provider-*`. No sibling-of-`current_exe()` scan (redundant: `cargo install` puts both binaries in `~/.cargo/bin`, which is on `$PATH`, and the Docker image installs to `/usr/local/bin`), and no settings-based discovery in M8 (see §6). Plugin name = the name reported at handshake; the binary suffix is just the discovery hint. Dev workflow note: when hacking on a plugin locally, `cargo install --path crates/clanky-provider-deepinfra` (or symlink the built binary) — running `target/debug/clanky` no longer finds a sibling plugin.
4. **The provider's name comes from the handshake, not the filename.** The binary `clanky-provider-deepinfra` should answer `hello` with `name: "deepinfra"`; core registers it under that name. This keeps `--provider deepinfra` and existing sessions/settings valid.
5. **`default_model` moves into the protocol handshake.** Core currently hardcodes DeepInfra's default model. Instead the plugin advertises a default model in its `hello` message, so core needs no per-provider table.  (Protocol v1 rule: additive, optional change — see §9.)
6. **Loopback stays for tests.** The `Handler` trait and `LoopbackTransport` remain the way unit/integration tests inject scripted providers (`tests/golden.rs`, `tests/sessions.rs`, `turn.rs` tests). No test needs a real process.

## Work breakdown

### 1. Protocol: small additive v1 changes

`crates/clanky-protocol/src/messages.rs`

- Add an optional `defaultModel: Option<String>` field to the `hello` message itself (next to `name` / `capabilities`; `capabilities` is a flags struct, so the field does not belong inside it). Purely additive; old plugins omit it, old clients ignore it.
- No pricing work needed: `listModels` already carries `inputPricePerMtok` / `outputPricePerMtok`.
- No `env` concept in the protocol: the plugin's environment is inherited from core's environment (core never handles keys), so `DEEPINFRA_URL` etc. work straight from the shell.

`crates/clanky-protocol/src/handler.rs`

- Extend `PluginInfo` with `default_model: Option<String>` (the loopback reads it from the same field the process transport parses).

Tests: extend `spec_examples_deserialize` and the hello round-trip with the new optional field; assert absent = `None`.

### 2. Protocol: `ProcessTransport`

New file `crates/clanky-protocol/src/process.rs` (behind a `process` feature or always on — it only needs `std`, no new deps beyond `serde_json`).

Responsibilities:

- **Spawn**: `Command::new(cmd).stdin(piped).stdout(piped).stderr(piped)`; env inherited; cwd inherited (§1).
- **Reader thread**: owns stdout, reads lines, parses with `messages::parse_message`, forwards to an `mpsc::channel`. This is what makes crash/deadlock safety possible: a blocking read never blocks the UI or the turn logic, and partial lines are buffered.
- **Stderr thread**: drains plugin stderr into a debug log (§1: stderr is never protocol). Write to `.clanky/logs/plugin-<name>.log` (append), or to `stderr` when a debug env var is set. Never the chat.
- **Writer**: serializes outgoing `Message`s as one JSON line + `\n`, flushes.  All writes from one place; a broken pipe maps to `Error::Io` → crash path.
- **`Transport::send`**: write the request, then return an iterator that pulls messages from the channel and ends at the terminal `done`/`error` for that request id. Chunks go to the `sink` as they arrive (preserving the streaming contract of the trait). Handle the one-in-flight rule (§6) by construction.
- **`Transport::notify`**: write `cancel` without waiting.

Lifecycle/crash handling (new small type, e.g. `PluginProcess`):

- `child.wait()` polling / `try_wait` on the reader thread: on unexpected exit mid-request, synthesize `Error::Protocol` / a `backend` provider error for the in-flight turn and mark the process dead so the next request respawns (default policy, §8: restart on next use, **not** auto-retry the turn).
- `kill()` + `wait()` on shutdown; close stdin first so a well-behaved plugin exits on EOF.
- Cancel fallback: after `cancel`, if no terminal message arrives within ~2 s, kill the process (restart on next use). The `wait-timeout` crate is already a workspace dependency (used by the bash tool); add it to `clanky-protocol` for the same polling pattern.

Tests (`crates/clanky-protocol/tests/`): drive a tiny shell/`printf`-based fake plugin (no network) to assert
- handshake, `listModels`, and a streamed `chat` round-trip over real pipes;
- a plugin that prints junk on stderr mid-stream does not corrupt the stream;
- a plugin that exits mid-stream surfaces a turn error and respawns next use;
- a plugin that never answers a `cancel` gets killed.

### 3. Plugin binary: `clanky-provider-deepinfra`

Turn the existing library into a real plugin process.

- `crates/clanky-provider-deepinfra/src/main.rs`: a `main` that wires `DeepInfraProvider` to stdin/stdout via a plugin-side loop.
- Add a plugin-side runtime to `clanky-protocol`: the mirror of `ProviderClient` — read JSONL, dispatch `hello` / `listModels` / `chat` / `cancel`, call `Handler`, write responses. Today `LoopbackTransport` does this dispatch for tests; factor that dispatch into a reusable `serve(handler, reader, writer)` so both the loopback and the real plugin share one implementation of the wire rules. This is the key de-duplication: the DeepInfra binary is then ~30 lines of glue.
- Chat streaming: `Handler::chat` currently takes a `&mut dyn FnMut` sink; the plugin runtime buffers/forwards sink payloads as `chunk` lines as they are produced (the blocking `ureq` SSE iterator drives this naturally).
- Cancellation: run chat on a worker thread, keep a cancel flag; the HTTP read loop checks it and returns `FinishReason::Cancelled`. If clean abort is impractical inside blocking `ureq`, the kill fallback in §7 still satisfies the spec — document which path DeepInfra takes.
- Auth/env/base-URL stay exactly as they are (`DEEPINFRA_API_KEY` / `DEEPINFRA_TOKEN` / `DEEPINFRA_URL`), read by the plugin (§1: core never handles keys).
- Add `default_model = DEFAULT_MODEL` to the handshake response so core can drop its hardcoded table.

`Cargo.toml`: add `[[bin]] name = "clanky-provider-deepinfra"` with the same package (or a `src/bin/` file). Keep the library exports so existing unit tests keep compiling unchanged.

### 4. Core: replace `provider.rs` with a plugin registry

Rewrite `crates/clanky/src/provider.rs` around discovery + spawning. It becomes the only place core talks about plugins.

- `discover()` → list of plugin descriptors from a single source: scan `$PATH` for executables named `clanky-provider-*` and derive candidate names from the suffix (design decision 3). No settings entries in M8.
- `available()` returns names from `discover()` (used by the `/provider` picker). Note: this is a filesystem scan, so cache it once at startup and refresh on demand.
- `create(name)` spawns the plugin (`ProcessTransport`), performs the handshake, verifies the reported plugin name, and returns something that `turn::run_turn` can drive. Because a process is long-lived and the turn loop is sync, the natural shape is a `ProviderSession` wrapper owning the child process and the `ProviderClient<ProcessTransport>`.
- `default_model(name)` is removed from core; the default model comes from the handshake. Update `TurnConfig::from_settings` / `Launch::from_settings` to take it from the session's `PluginInfo`.
- `UnknownProvider` error message should list discovered names, not a hardcoded string. Also update the error text in `error.rs` (it currently says "available: deepinfra").
- **Default provider resolution.** `provider::DEFAULT_PROVIDER` ("deepinfra") is the last provider-specific string in core. Options: (a) keep it as a product default (harmless — it is just a name, and the M8 "Done when" wants zero-config DeepInfra); or (b) resolve dynamically: if the user named no provider, use the sole discovered plugin, and otherwise require `provider`/`--provider`. Recommend (a) for M8 — it preserves the documented default and existing sessions — with the error path (plugin missing) telling the user how to install it.

### 5. Core: thread the plugin session through the app

- `turn.rs`: `run_turn` currently constructs `LoopbackTransport::new(handler)`. Generalize it over the transport, or split it into
  - `run_turn_with<C: ...>(client: &mut ProviderClient<T>, ...)` for real use, and
  - keep a thin `run_turn(Box<dyn Handler>, ...)` wrapper for tests that builds a loopback client.
  This keeps every existing test source-compatible while letting the TUI/pipe paths pass a process-backed client.
- `main.rs` (pipe mode): build the plugin session from the resolved provider name, pass the client into the turn, and ensure the plugin is shut down on all exit paths (a small guard / `Drop` on `ProviderSession`).
- `tui/mod.rs`: the TUI spawns a turn on a worker thread and **creates the handler inside the thread** today. With a process plugin we want one long-lived process, not one per turn, and `ProviderClient<ProcessTransport>` should be `Send` so it can move to the worker. Options:
  - keep the session on the UI thread and move the client into each turn worker, returning it in `TurnEnd`; or
  - create a dedicated "provider thread" owning the process, with request channels.
  Recommend the first (move the `ProviderSession` into the turn worker and move it back in `WorkerEvent::Done`) — it needs no extra thread and preserves today's "worker owns the handler" simplicity. `spawn_models` likewise borrows the session (or spawns a short-lived one only when no session exists yet).
- `Catalog` fetching already goes through `ProviderClient::list_models`; it now talks to the process. `/provider` switch must tear down the old session and spawn the new one (and clear `Catalog`, which already tracks the provider name for staleness).
- `set_provider` / `Launch::from_settings`: default model comes from the new plugin's handshake instead of a hardcoded table. This makes the `/provider` picker's "detail" column require a handshake per listed provider — either handshake lazily on selection only, or show the binary name as the detail and leave the default model out of the picker. Recommend: picker lists discovered names with the resolved command path as detail; handshake happens on selection.

### 6. Settings: none for M8

No `[plugins.<name>]` table in M8. Discovery is the naming convention on `$PATH`; the plugin reads its own env (`DEEPINFRA_API_KEY` etc.), which the child process inherits from core's environment unchanged (core never handles keys). Nothing needs configuring.

This deliberately drops what `milestones.md` M8 called "Plugin declaration in settings". That mechanism earns its keep only when a plugin needs a custom command, args, or extra env — exactly the MCP-server case (M9: `npx some-server --root /path`). Re-introduce a `[plugins.<name>] { command, args, env }` settings table in M9 as the single declaration mechanism for MCP servers (it can also pin or override a provider plugin, if ever needed).

`config.rs`'s `PLUGINS_DIR` (`.clanky/plugins/`) is removed in M8 rather than reserved: a directory of bare binaries cannot express MCP servers (which need args), and a directory of TOML declarations would duplicate the settings table. If M9 wants a drop-in declarations directory, re-add it then. Update the `config.rs` doc comment and its test.

### 7. Errors and UX

- New error variants: `PluginSpawn { name, command, source }`, `PluginHandshake { name, message }`, `PluginCrashed { name }`, `PluginProtocolVersion { name, peer }`. Keep provider errors passing through intact — a 401 from DeepInfra must still read "401: invalid API key" (§8), which already works because the plugin emits a protocol `error`.
- `UnknownProvider` lists discovered providers.
- No plugin found for the default provider: a clear message that names the expected binary (`clanky-provider-deepinfra`) and how to install it.
- Plugin stderr → `.clanky/logs/plugin-<name>.log`; surface the log path in the error message when a plugin fails to start.
- Crash mid-turn: user-facing turn error; do not auto-retry (existing retry logic only retries protocol-`retryable` provider errors, which is correct).
- Restart policy: lazy, on next use (settled open question #2).

### 8. Packaging and docs

- Workspace: `clanky-provider-deepinfra` stays its own crate (core must not link it), so the two binaries install as two crates. Document the two-line install (`cargo install clanky clanky-provider-deepinfra` — both land in `~/.cargo/bin`, so PATH discovery finds the plugin with zero config); if a single command is wanted later, publish a small meta-crate depending on both. The meta-crate can be M10.
- `docker/runner.Dockerfile` and `build-runner`: copy **both** release binaries into the image and `cargo build --release` both in `build-linux-musl`. Update `build-runner`'s staging step (`docker/clanky` → also `docker/clanky-provider-deepinfra`) and the Dockerfile `COPY`/`chmod`.
- `run`: unchanged (env passthrough already covers provider keys), but add `DEEPINFRA_API_KEY` alongside the existing `DEEPINFRA_TOKEN` if desired.
- `README.md`: document the plugin model, PATH-based discovery, and how to write a plugin (link `provider-protocol.md`); add a "writing a provider plugin" section with the Python example from Appendix A.
- `milestones.md`: tick M8 and record the decisions (PATH-only discovery + handshake name; settings declaration deferred to M9; default model via handshake).

### 9. Tests

Keep all existing tests green (they use loopback mocks and must not change semantics):

- `crates/clanky/tests/golden.rs`, `tests/sessions.rs` — drive `run_turn(Box<dyn Handler>, ...)`; keep that entry point.
- `crates/clanky/src/turn.rs` unit tests — unchanged.
- `crates/clanky/src/tui/mod.rs` tests that assert `"deepinfra"` and the DeepInfra default model must be updated: they encode core's hardcoded provider table, which is exactly what we are removing. Replace with discovered-name fixtures / injected `PluginInfo`.

New tests:

- `clanky-protocol`: `ProcessTransport` tests with a scripted child process (a tiny Rust test binary or `sh -c` fake), covering handshake, streaming, stderr isolation, crash, cancel-timeout kill.
- `clanky`: discovery tests over a temp dir with fake `clanky-provider-*` files on a fake `$PATH` (assert name derivation, executability check, dedup).
- End-to-end: a hello-world Python plugin (M10 wants this in-repo; M8's "Done when" needs it) exercised through `clanky -p` in an integration test, or at minimum through `ProcessTransport` directly.

## Sequencing (each step lands independently)

1. Protocol additive fields (`defaultModel`) + `serve()` extraction; all tests still pass with loopback.
2. `ProcessTransport` + lifecycle, with fake-child tests.
3. `clanky-provider-deepinfra` binary wired to `serve()`.
4. Core registry/discovery (PATH-only); DeepInfra now runs as a spawned process; `main.rs` pipe mode switched over.
5. TUI session threading + `/provider` switch + default-model-from-handshake.
6. Packaging (docker/build scripts), docs, hello-world Python example.

## Risks / open points

- **`ProviderClient<ProcessTransport>` must be `Send`** to move into the TUI turn worker. `ProcessTransport` holds a `Child`, pipes, and an `mpsc::Receiver` — all `Send`; the design must avoid `Rc`/`RefCell` in it.
- **Blocking `ureq` + cancel**: clean cooperative cancellation inside a blocking read may not be possible; the spec's kill fallback covers it, but the DeepInfra plugin must return `finishReason: "cancelled"` promptly in the common case or document that it relies on the kill.
- **Model-listing on `/provider`**: handshaking every discovered plugin to populate picker details is slow; prefer listing names + paths and handshaking on selection.
- **Version negotiation**: the handshake already rejects a mismatched `protocolVersion`; make the user-facing message name the plugin and both versions.
- **`config::PLUGINS_DIR`**: resolved — remove it in M8 (see §6); fix the `config.rs` doc comment and test.
- **Windows**: `$PATH` lookup must respect `PATHEXT` (not just bare filenames). Not a target platform today, but avoid hardcoding extension-less lookups.
