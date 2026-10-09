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
├── logs/                   provider plugin stderr logs (project scope only)
└── sessions/               session files (project scope only)
```

Provider plugins are **not** configured here: they are discovered on `$PATH`
(see [Provider plugins](#provider-plugins) below).

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
| `max_tool_rounds` | integer  | Cap on chat rounds per turn (`0` = unlimited)       |
| `max_retries` | integer       | Retries for a retryable provider error (`0` = never retry; default `5`) |
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
- `max_retries` covers transient provider failures — rate limits (429
  `Model busy`), backend errors, and network timeouts. Retries use
  exponential backoff (500 ms, 1 s, 2 s, 4 s, 8 s), honoring a server
  `Retry-After` when it asks for longer, and are shown in the transcript
  (TUI) or on stderr (pipe mode). A round that already streamed content is
  never replayed, so a retry cannot duplicate output. Set it on the CLI
  with `--max-retries`.
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

## Provider plugins

Clanky core contains no provider-specific code. Every provider is a separate
executable named `clanky-provider-<name>` on `$PATH`, speaking a small JSONL
protocol over stdin/stdout (see `provider-protocol.md`). This means:

- **Zero config.** Install `clanky` and `clanky-provider-deepinfra` (both from
  the same workspace) and Clanky finds the provider on `$PATH`:

  ```sh
  cargo install clanky clanky-provider-deepinfra
  ```

  (Both land in `~/.cargo/bin`, which is on `$PATH`.)

- **Any language.** A plugin is a process; the contract is JSON over stdio.
  There is a complete ~90-line Python example in
  [`examples/hello-world-provider/`](examples/hello-world-provider/).
- **Provider name comes from the handshake, not the filename.** The binary
  `clanky-provider-deepinfra` reports `name: "deepinfra"` when it starts, and
  Clanky registers it under that name (`--provider deepinfra`, sessions, and
  settings all keep working).
- **No settings table.** A plugin reads its own credentials from the
  environment, which it inherits from Clanky (`DEEPINFRA_API_KEY` /
  `DEEPINFRA_TOKEN` / `DEEPINFRA_URL`); Clanky never handles provider keys.
- **Isolation.** Each plugin is a long-lived subprocess (one per session,
  spawned lazily on first use). Its stderr goes to
  `./.clanky/logs/plugin-<name>.log`, never to the chat; a crash mid-turn
  surfaces an error and the plugin is respawned on the next turn.

The provider's default model (used when neither settings nor `--model` name
one) is advertised by the plugin at handshake, so Clanky keeps no hardcoded
per-provider table.

### Built-in providers

| Provider | Binary | Credentials |
| --- | --- | --- |
| DeepInfra | `clanky-provider-deepinfra` | `DEEPINFRA_API_KEY` (or `DEEPINFRA_TOKEN`), optional `DEEPINFRA_URL` |
| LiteLLM proxy | `clanky-provider-litellm` | `LITELLM_API_KEY`, optional `LITELLM_BASE_URL`, optional `LITELLM_MODEL` |

Both ship from this workspace:

```sh
cargo install clanky clanky-provider-deepinfra clanky-provider-litellm
```

### LiteLLM

The LiteLLM plugin talks to a [LiteLLM proxy](https://docs.litellm.ai/)'s
OpenAI-compatible `/v1` surface, so one key reaches every model the proxy is
configured to serve.

```sh
export LITELLM_BASE_URL=https://your-gateway.example.com   # default: http://localhost:4000
export LITELLM_API_KEY=sk-...                              # required
export LITELLM_MODEL=claude-sonnet-4-6                     # optional default model
clanky --provider litellm -p "say hi"
```

Notes:

- **The model list is whatever your key can see.** `/v1/models` is
  key-scoped, and the richer metadata (context window, pricing, reasoning and
  tool support) comes from `/v1/model/info`. That endpoint is *not* one of
  the LLM API routes, so a virtual key may be denied it (403); the plugin
  then falls back to the plain catalog and simply omits the hints — a listing
  never fails because of it.
- **Thinking maps to `reasoning_effort`.** Clanky's `--thinking <budget>` is
  translated onto LiteLLM's effort ladder (`none`/`minimal`/`low`/`medium`/
  `high`/`xhigh`/`max`); `--thinking off` omits the field. Because the proxy
  is heterogeneous, the plugin drops `reasoning_effort` (and `tools`) for a
  model whose `/model/info` says it does not support them, instead of
  provoking a 400.
- **No built-in default model.** The proxy's catalog is arbitrary, so unless
  `LITELLM_MODEL` is set you must name a model (`model` in settings or
  `--model`). Clanky still defaults to the `deepinfra` provider, so a
  LiteLLM-only setup also sets `provider = "litellm"` in `settings.toml` (or
  passes `--provider litellm`).
- **Reasoning is shown as thinking.** The proxy returns reasoning in
  `delta.reasoning_content`, which the plugin surfaces as Clanky's thinking
  stream. (If the proxy is configured with
  `merge_reasoning_content_in_choices: true`, reasoning is instead merged into
  the visible content and cannot be separated — the plugin does not attempt
  to split it.)

### Writing a provider plugin

A plugin is a program that:

1. Reads one JSON message per line from stdin and writes one per line to
   stdout (stderr is ignored except for logging).
2. Answers `hello` with its `name`, `capabilities`, and `defaultModel`.
3. Answers `listModels` with its catalog.
4. Answers `chat` with `chunk` events and exactly one terminal `done`/`error`.
5. Exits when stdin closes.

See `provider-protocol.md` for the full spec and
[`examples/hello-world-provider/clanky-provider-hello-world`](examples/hello-world-provider/clanky-provider-hello-world)
for a minimal working example. To try it:

```sh
export PATH="$PWD/examples/hello-world-provider:$PATH"
clanky --provider hello-world -p "say hi"
```

## TUI keys

| Key | Action |
| --- | --- |
| `Enter` | submit (queued while a turn is streaming) |
| `↑`/`↓` | recall previously submitted prompts (the first `↑` saves the current input as a draft; `↓` past the newest entry restores it) |
| `Tab` | complete the file path at the caret: longest common prefix first, further `Tab` presses cycle candidates |
| `ctrl+e` | edit the prompt buffer in `$EDITOR` (the TUI suspends; an emptied buffer clears the input) |
| `ctrl+c` / `ctrl+d` | quit |
| `ctrl+l` | clear the visible screen |
| `ctrl+t` | toggle showing streamed thinking (display only; the session file always records it) |
| mouse wheel, scrollbar | scroll (native terminal scrolling) |
| text selection | copy/paste (native terminal selection) |
| `/` (empty input) | command palette |
| `/system` | show the assembled system prompt (context files and skills, as sent to the model) |
| `/md [flags] [file]` | export the session as a Markdown file (asks for a filename in a modal when none is given) |

### `ctrl+t` — show/hide thinking

`ctrl+t` toggles whether streamed thinking is displayed. The status line
always shows the current state (`think:on` / `think:off`). This is a
display-only switch: the session file records every thinking block, so
`/md --thinking` and `--all` still export the full text.

Because the transcript is printed straight to the terminal, it cannot be
rewritten to expand or collapse what is already on screen. The toggle
therefore affects text from that point on:

- While thinking is hidden, each new thinking block prints a single
  `Thinking...` line — interleaved with whatever comes around it (tool
  calls, results, assistant text) in the order things happen.
- The hidden text is deliberately discarded rather than stored, so
  turning the display back on cannot surface a block out of order.
- The toggle works mid-stream. Turning it off while thinking is
  streaming ends the block there and prints `Thinking...`; turning it on
  resumes printing from that delta — possibly mid-sentence, as a new
  block below what was already printed.

### `/md` — export the session as Markdown

`/md` writes the current session to a Markdown file. The default export
contains the user prompts and the assistant's visible answers; two flags
add the rest, and combine freely:

| Flag | Adds |
| --- | --- |
| `--thinking` | the assistant's thinking blocks |
| `--tools` | tool calls (with their arguments) and their results |
| `--all` | both of the above |

A filename may be given on the command line; without one, a modal asks
for it (`Esc` cancels, `Enter` confirms). Relative paths resolve against
the working directory, parent directories are created as needed, and an
existing file is overwritten.

Authored text — user prompts and assistant answers — is exported
verbatim, so its Markdown (code blocks, headings, lists) renders exactly
as it did in the conversation. Thinking and tool output is raw
machine-produced text and is wrapped in fenced code blocks instead. For
example:

```text
/md session.md
/md --tools --thinking out/session.md
/md --all
```

The TUI only starts when stdin *and* stdout are terminals; piped input
falls back to plain one-shot mode.
