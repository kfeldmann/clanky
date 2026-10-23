![Clanky](clanky.jpg)
# Clanky

An AI coding agent/harness for the terminal, written in Rust.

## Features

- Small, simple, efficient. Low resource consumption
- Terminal interface (TUI) or pure command line (perfect for scripting)
- Open source with no spyware
- Isolation/safety comes from containeraization, not from built-in rules
- Three provider plugins included. You can write additional provider plugins easily in any language
- Parallel subagents
- Multimodal input (send images and text as input - output is always text)
- Full control over sampling parameters
- Export sessions as Markdown
- Resume previous sessions; can name session files for easy identification

To get started, read the [QUICKSTART.md](QUICKSTART.md).

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
| `settings.toml`     | project field wins (per-field overlay; `[sampling]` merges per key); CLI flags win over both |
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
| `thinking` | string           | Thinking budget in tokens (`1024`) or `off`        |
| `sampling` | table           | Sampling parameters passed to the provider verbatim, e.g. `temperature = 0.7` |
| `max_tool_rounds` | integer  | Cap on chat rounds per turn (`0` = unlimited)       |
| `max_retries` | integer       | Retries for a retryable provider error (`0` = never retry; default `5`) |
| `prompt`   | string           | Prompt text (mainly useful on the CLI, not in a file) |
| `session`  | boolean          | `false` disables session recording in every mode (default: on) |
| `project`  | string           | Label shown at the left of the TUI input prompt (TUI only) |
| `timeout_seconds` | integer  | Default tool-call timeout in seconds (`bash` calls without their own `timeout_seconds` run unbounded when unset; `edit` ignores it) |

Example:

```toml
provider = "deepinfra"
model = "meta-llama/Llama-3.3-70B-Instruct"
thinking = "off"

[sampling]
temperature = 0.7
top_p = 0.9
repetition_penalty = 1.1
```

Notes:

- `thinking = "off"` really turns thinking off: it is sent to the provider
  as an explicit zero budget, which each provider maps onto its backend's
  "disable reasoning" knob (DeepInfra and LiteLLM send
  `reasoning_effort: "none"`; Anthropic omits the `thinking` block, which
  that API treats as off). Leaving `thinking` unset is *not* the same thing:
  the backend's default then applies, which on reasoning models (e.g.
  DeepSeek, GLM) means a reasoning trace **is** produced.
- Sampling keys and values are passed to the provider backend **verbatim**:
  any parameter the backend accepts works here (`seed`,
  `repetition_penalty`, `reasoning_effort`, `top_k`, …) and the backend
  validates them, so an invalid value fails at call time with the server's
  own error — Clanky never rejects or rewrites a sampling value itself.
  Values may be any TOML value; a string that is JSON (e.g. `"0.7"`) is sent
  as that JSON value, so `temperature = "0.7"` and `temperature = 0.7` mean
  the same thing (a string like `"high"` stays a string). The request's
  structural fields (`model`, `messages`, `tools`, …) are set by
  the provider and cannot be overridden through `sampling`. The one
  exception is `stream` on the Anthropic provider: setting `stream =
  false` there is honored end to end (see above).
- `max_retries` covers transient provider failures — rate limits (429
  `Model busy`), backend errors, and network timeouts. Retries use
  backoff growing ×1.25 per retry (500 ms, 625 ms, 781 ms, 976 ms, 1.2 s,
  …, capped at 30 s), honoring a server
  `Retry-After` when it asks for longer, and are shown in the transcript
  (TUI) or on stderr (pipe mode). A round that already streamed content is
  never replayed, so a retry cannot duplicate output. Set it on the CLI
  with `--max-retries`.
- `timeout_seconds` sets the **default tool timeout**, applied to any tool
  call that does not pass its own `timeout_seconds` (today that is only
  `bash` — the `edit` tool ignores it; see below). Unset — the default —
  means unbounded, exactly as before; a per-call `timeout_seconds` on the
  call always wins over the configured default, in both directions. Set it
  on the CLI with `--timeout-seconds`, or live in the TUI with
  `/timeout <seconds|off>` (`off` clears back to unbounded). An expired
  killable call (e.g. `bash`) is real cancellation: the whole command tree
  is TERM/KILLed and the model sees the timeout error.
- Precedence is per field: CLI flag > project file > user file, and a
  scope only overrides fields it actually sets. For `[sampling]` the merge
  is per key: keys set in a higher layer win, and keys set only in a lower
  layer are kept — `--sampling "top_p=0.9"` never drops a `temperature`
  from `settings.toml`.

### The `bash` tool

Commands run in the project directory with **no timeout**: they run until
they finish. If the model wants a deadline it passes `timeout_seconds`
(any positive number of seconds) on the call — and a configured default
(`timeout_seconds` setting, `--timeout-seconds`, `/timeout`) applies when
the call passes none. The per-call value always wins over the configured
default; without either, a command runs unbounded. Any deadline is
honored by killing the command's **whole process tree** (TERM, then KILL)
when it expires, so grandchildren cannot outlive the deadline and wedge
the call. Without any deadline the group is never killed and the command
runs until it exits on its own (terminal ctrl+c propagates through
Clanky's own turn cancellation as before). Output sent to the model is
capped at 20 000 characters (then `… (output truncated)`).

### The `edit` tool

The `edit` tool replaces a literal substring in a file. The model passes
`path`, `old_string`, `new_string`, and optionally `replace_all: true`.
Matching is byte-for-byte literal — no patterns, no wildcards — so the text
in the tool call is exactly the text in the file (there is no read-before-edit
requirement; the model reads files through `bash`).

- Zero matches fails with `not found` (naming the file); the model re-reads
  the file and retries.
- Multiple matches fail without `replace_all`, naming the match count and
  asking for more surrounding context — with `replace_all: true` every
  occurrence is replaced.
- On success the result is a short confirmation, `Replaced N occurrence(s)
  in <path>` — no file contents, no diff: the tool call in the conversation
  already carries both strings.

Failures are data: they are fed back to the model as normal tool results
(rendered red in the TUI, like a non-zero bash exit) so it can recover, not
turn errors. There is no permission boundary — `bash` already writes
anywhere; the tool's value is intent and actionable failure messages.

### The `view-image` tool

The `view-image` tool puts an image file into the model's context, for
multimodal models. The model passes `path`; the tool reads the file,
base64-encodes it, and returns a result whose text is a size note
(`623 bytes`, `256 kb`, `1.2 mb`) and whose image list carries the data
beside the text. The TUI never displays the image — the transcript shows
the size note like any tool result; the image is image → model only (the
model never generates images).

- The media type is resolved from the file's magic bytes when the format
  carries one (JPEG, PNG, GIF, WEBP) and from the filename extension
  otherwise (HEIC/HEIF/AVIF and any misnamed file); an unrecognizable
  file with no known image extension fails with a data error the model
  can recover from.
- A model that cannot accept images 400s, and the backend's own error is
  surfaced unchanged — there is no local capability whitelist.
- On the wire, a tool result's images ride with the result: OpenAI-
  compatible providers (DeepInfra, LiteLLM) get `image_url` data-URI
  parts in a `user` message that directly follows the `tool` message
  (their `tool`-message schema accepts only plain-string content),
  Anthropic gets `image` source blocks inside the `tool_result` block.
- Session files record the image in full base64 so a replay reconstructs
  exactly what the model saw (a screenshot is ~300–400 KB encoded —
  expect large JSONL lines). `/md` exports an `_attached image_` note
  instead of the payload.

### Session precedence note

Sessions always live in the **project scope** (`./.clanky/sessions/`);
there is no user-scope session store.

Sessions are recorded in **both** modes — the TUI and print mode (`-p`)
autosave alike. `--no-session` (or `session = false` in settings.toml;
the flag wins) turns recording off in every mode: no session file is
created, `--resume` is rejected (nothing to load into, nothing would be
saved), the TUI's `/name` and `/resume` commands report that sessions are
off, and the status line shows `(no session)`. `/md` still works: it
exports what the screen showed.

`--resume NAME` resumes a saved session in both modes (the TUI also opens
a picker of saved sessions when `NAME` is omitted): the transcript and the
full conversation history are restored, and the turn's records append to
the same file.

### Settings records

Every session file starts with a `settings` record: provider, model, and
the merged settings a turn runs with (thinking, `[sampling]`,
`max_tool_rounds`, `max_retries`, `project`, `timeout_seconds`; unset
fields are elided). The session header carries only `version` and
`created` — provider/model moved into the settings records (old session
files with a provider/model header still resume fine). Whenever settings
change mid-session — `/model`, `/provider`, `/thinking`, `/sampling`,
`/timeout`, or a model default adopted from the plugin handshake — one
settings record is written, in the order the changes happen. Each record
is a full snapshot of the effective settings, so folding a session's
records in order (later records win per field, `[sampling]` per key)
reconstructs the settings state as of the end of that session.

On `--resume` the records never *set* values: the folded state is
compared against the settings you are resuming with and, when they
differ, one change record (a snapshot of the new active settings) is
appended — from the resumed session's perspective the settings changed
mid-session, in order.

`--name NAME` starts a session under a fixed name instead of the generated
timestamped one (same validation as `/name`, in both modes);
`--name` together with `--resume` is a usage error — the resumed file
already has its name (`/name` can still rename it mid-session).

Sessions autosave as the conversation happens. When a turn is cut short —
a provider error mid-stream, or `ctrl+c` quitting while the answer is
still streaming — the partially streamed round (thinking and text) is
consolidated and written anyway, so no streamed content is lost:
`/resume` restores exactly what the terminal showed, and continuing the
conversation (e.g. "please continue") sends the full chat as context.
Partial tool calls and tool results that never completed are not kept.

The transcript is printed straight to the terminal — no alternate
screen, no mouse capture — so scrolling and copy/paste are your
terminal's own (native scrollbar or wheel, native text selection), and
the conversation stays in the terminal after clanky quits. `ctrl+l`
erases the visible screen (scrollback is kept).

## Skills

Skills are reference material for the agent: each is a markdown file listed
(name, description, path) in the system prompt, and read into the
conversation only when a task matches its description. Drop them in
`~/.clanky/skills/<name>.md` (user scope) or `.clanky/skills/<name>.md`
(project scope; shadows the user one on a name clash). An optional YAML
front matter block may carry a `description:`.

Optional, ready-made skills live in this repository's `contrib/skills/` —
copy the ones you want into either scope. Current entries:

- `parallel-subagents` — when and how to fan work out to parallel Clanky
  subagents via `clanky-subagents` (see also
  [Parallel subagents](#parallel-subagents-clanky-subagents)).

## Provider plugins

Clanky core contains no provider-specific code. Every provider is a separate
executable named `clanky-provider-<name>` on `$PATH`, speaking a small JSONL
protocol over stdin/stdout (see `design-docs/provider-protocol.md`). This means:

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

### DeepInfra

Reasoning models on DeepInfra (DeepSeek, GLM, Kimi, Ling, …) produce a
reasoning trace by default, which streams as Clanky's thinking. To disable
it, set `thinking = "off"` (or pass `--thinking off`): the provider sends
DeepInfra's documented `reasoning_effort: "none"`. Leaving `thinking` unset
does **not** disable it — DeepInfra's default is to reason.

A thinking budget is quantized onto DeepInfra's `reasoning_effort` ladder
(`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`, per the per-model
docs in DeepInfra's model browser — the chat-completions API reference
still documents only a subset): `0`→`none`, `<500`→`minimal`, `<1682`→`low`,
`<5661`→`medium`, `<19051`→`high`, `<64107`→`xhigh`, else `max`. A
`reasoning_effort` set in `[sampling]` wins over the derived value.

### Built-in providers

| Provider | Binary | Credentials |
| --- | --- | --- |
| DeepInfra | `clanky-provider-deepinfra` | `DEEPINFRA_API_KEY` (or `DEEPINFRA_TOKEN`), optional `DEEPINFRA_URL` |
| LiteLLM proxy | `clanky-provider-litellm` | `LITELLM_API_KEY`, optional `LITELLM_BASE_URL` |
| Anthropic | `clanky-provider-anthropic` | `clanky-provider-anthropic login` (OAuth) — or `ANTHROPIC_API_KEY`; optional `ANTHROPIC_BASE_URL` |

All three ship from this workspace:

```sh
cargo install \
  clanky \
  clanky-provider-deepinfra \
  clanky-provider-litellm \
  clanky-provider-anthropic
```

### LiteLLM

The LiteLLM plugin talks to a [LiteLLM proxy](https://docs.litellm.ai/)'s
OpenAI-compatible `/v1` surface, so one key reaches every model the proxy is
configured to serve.

```sh
export LITELLM_BASE_URL=https://your-gateway.example.com   # default: http://localhost:4000
export LITELLM_API_KEY=sk-...                              # required
clanky --provider litellm -p "say hi"
```

Notes:

- **The model list is whatever your key can see.** `/v1/models` is
  key-scoped, and the richer metadata (context window, pricing, reasoning and
  tool support) comes from `/model/info`. That endpoint is *not* one of the
  LLM API routes, so a virtual key may be denied it (403); the plugin then
  falls back to the plain catalog and simply omits the hints — a listing
  never fails because of it. (The plugin calls the bare `/model/info` path,
  not the `/v1/model/info` alias: LiteLLM's virtual-key route allowlist names
  the bare form, so a restricted key can be denied the alias with a 403.)
- **Thinking maps to `reasoning_effort`.** Clanky's `--thinking <budget>` is
  translated onto LiteLLM's effort ladder (`none`/`minimal`/`low`/`medium`/
  `high`/`xhigh`/`max`); `--thinking off` maps to `none` (the proxy's
  documented "disable reasoning" value). Values pass
  through unconditionally and the proxy validates them: asking a model for
  thinking or tools it cannot handle surfaces the proxy's own error (which
  tells you to turn thinking off, or that tools are not supported) — nothing
  is silently dropped. The heterogeneous catalog is handled at the picker
  instead: the `/model` picker hides models whose `/model/info` says they
  cannot call tools (a deal breaker for the agentic loop), though `--model`
  can still name one and gets the server's answer.
- **Built-in default model.** The default model is `claude-haiku-4-5-20251001`,
  overridable with `model` in settings or `--model`. Clanky still defaults to
  the `deepinfra` provider, so a LiteLLM-only setup also sets
  `provider = "litellm"` in `settings.toml` (or passes `--provider litellm`).
- **Reasoning is shown as thinking.** The proxy returns reasoning in
  `delta.reasoning_content`, which the plugin surfaces as Clanky's thinking
  stream. (If the proxy is configured with
  `merge_reasoning_content_in_choices: true`, reasoning is instead merged into
  the visible content and cannot be separated — the plugin does not attempt
  to split it.)

### Anthropic

The Anthropic plugin talks to the Messages API and authenticates either way
your account allows:

**OAuth (subscription / enterprise accounts with no API key):**

```sh
clanky-provider-anthropic login   # once per machine; headless (SSH/container)
clanky --provider anthropic -p "say hi"
```

The `login` subcommand runs a PKCE flow with no browser: it prints an
authorize URL, you open it anywhere, and paste the code back. Credentials
live in `~/.clanky/providers/anthropic/credentials.json` (0600) and are
refreshed automatically before expiry. The refresh token is finite and
rotates on every refresh — when it expires (weeks later), requests start
failing with an actionable `auth` error and you re-run `login`.

**API key (if your account has one):**

```sh
export ANTHROPIC_API_KEY=sk-ant-...
clanky --provider anthropic -p "say hi"
```

An explicitly set key wins over stored credentials (your choice, not the
program's). The key path sends the documented `x-api-key` headers; the
OAuth path sends bearer headers and identifies as Claude Code. Everything
else — streaming, tools, thinking — is identical through both.

Streaming is the default. Setting `stream = false` in your `sampling`
config takes the non-streaming path end to end: the request goes out with
`stream: false`, the whole response body is read at once, and it is
translated into the same streamed chunks every other provider emits —
so nothing downstream changes, only the wire shape to the API.

Notes:

- The default model is `claude-haiku-4-5-20251001` (from the live catalog;
  overridable with `model` in settings or `--model`).
- The model list comes from `GET /v1/models` on the authenticated account.
- `ANTHROPIC_BASE_URL` overrides `https://api.anthropic.com`.

### Writing a provider plugin

A plugin is a program that:

1. Reads one JSON message per line from stdin and writes one per line to
   stdout (stderr is ignored except for logging).
2. Answers `hello` with its `name`, `capabilities`, and `defaultModel`.
3. Answers `listModels` with its catalog.
4. Answers `chat` with `chunk` events and exactly one terminal `done`/`error`.
5. Exits when stdin closes.

See `design-docs/provider-protocol.md` for the full spec and
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
| `↑`/`↓` | move within the lines of a multi-line buffer; from the top row `↑` recalls previously submitted prompts (the first `↑` saves the current input as a draft; `↓` past the newest entry restores it) and from the bottom row `↓` steps forward through history |
| `Tab` | complete the file path at the caret: longest common prefix first, further `Tab` presses cycle candidates |
| `ctrl+a` / `ctrl+e` | move to the start / end of the current input line (`ctrl+e` at the end of the buffer opens `$EDITOR`) |
| `ctrl+j` | insert a newline in the input |
| `ctrl+e` | edit the prompt buffer in `$EDITOR` (the TUI suspends; an emptied buffer clears the input) |
| `ctrl+u` | clear the input from the cursor to the beginning of the line |
| `ctrl+c` / `ctrl+d` | quit |
| `ctrl+l` | clear the visible screen |
| `ctrl+t` | toggle showing streamed thinking (display only; the session file always records it) |
| mouse wheel, scrollbar | scroll (native terminal scrolling) |
| text selection | copy/paste (native terminal selection) |
| `/` (empty input) | command palette |
| `/system` | show the assembled system prompt (context files and skills, as sent to the model) |
| `/md [flags] [file]` | export the session as a Markdown file (asks for a filename in a modal when none is given) |
| *(at startup)* | the effective settings are shown once as a transcript note (`settings: …`) — the same merged values `/model` &c. edit and the session file records |

### Rendering notes

The transcript prints straight to the terminal's normal buffer (no
alternate screen, no mouse capture), so native scrolling, selection, and
post-quit history all work. Two details of that model are visible:

- The rightmost column of the terminal is never painted. Writing the
  last column puts the terminal into a pending-wrap state whose
  follow-up behaviors differ across terminals (and GNU `screen` defers
  displaying a character written there until the screen scrolls), so the
  renderer reserves it and the last column stays blank.
- Markdown tables render as boxes with box-drawing borders
  (`┌─┬┐`/`│`/`├─┼┤`/`└┴┘`) instead of plain pipe rows, with inter-row rules
  always drawn. Column widths are fitted to the transcript width (each
  column keeps at least its longest word; extra space is distributed
  proportionally), and cells word-wrap inside their column, so a wide
  table in a narrow terminal stays readable instead of tangling with
  the generic word-wrapper. A table is fitted to the width when it is
  printed; resizing does not rewrite scrollback.
- Resizing, returning from `$EDITOR`, and clearing redraw the bottom
  region in place; nothing is pushed into scrollback by a redraw.
- A `project` label (the `project` setting or `--project`) prints at the
  left of the
  input prompt: `project = "clanky"` renders the label before the
  prompt symbol (idle: `clanky` + two spaces + symbol; streaming: the
  dot is lit between them); unset, the prompt is unchanged.
  Display-only (print mode ignores it).
- Multi-line input (`ctrl+j`, paste, `$EDITOR`) renders as one row per
  line, indented under the prompt symbol, so every line starts at the
  same column; each row scrolls horizontally to keep the caret visible,
  and when the input outgrows the screen the rows around the caret are
  the ones shown. Bracketed paste is on: a paste is inserted whole at
  the caret (its newlines become input lines) and never submits — only
  a pressed `Enter` submits.
- The status line de-contents gracefully as the terminal narrows:
  elements are dropped one at a time — provider, thinking budget,
  model, cost, session name, the word "queued" (leaving just the dot) —
  and finally the context percentage, so only the widest terminals show
  everything. When cost/context data is unavailable the hints
  (`/ commands · ctrl+c quit`) take their place on the right and are
  the last element to go.

### `ctrl+t` — show/hide thinking

`ctrl+t` toggles whether streamed thinking is displayed. This is a
display-only switch: the session file records every thinking block, so
`/md --thinking` and `--all` still export the full text, and /resume
loads the full session including all thinking text.

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

### `/timeout` — the default tool timeout

`/timeout` shows the current default tool timeout (as a transcript info
line), `/timeout N` sets it to `N` seconds, and `/timeout off` clears it
back to unbounded. The default applies to any tool call that passes no
`timeout_seconds` of its own (`bash`; the `edit` tool ignores it); a
per-call value always wins. Same effect as the `timeout_seconds` setting
or `--timeout-seconds`; the TUI edit changes the live session (and is not
written back to any settings file).

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

## Print mode (`-p`)

When the TUI can't start (or `-p` is given), Clanky runs a single turn:
the agent's response goes to **stdout**, and activity — thinking, tool
calls, tool results, retry notices — to **stderr**, so
`clanky -p ... | grep` style pipes stay clean.

Every printed entry ends with exactly one blank line, so consecutive
entries read as units:

- The answer text streams to stdout as it arrives. When the block ends
  (a tool round starts, or the turn finishes) it is trimmed and closed
  with a blank line. Tool-round texts are each closed this way, so a
  multi-round turn prints one paragraph per round.
- Thinking blocks print the same way, on stderr, with each paragraph
  prefixed with `> ` so they read as quoted asides.
- Tool calls print as `● name {arguments…}` (arguments preview-capped).
- Tool results render like the TUI transcript, without the color: a
  `└─ name: …` header, continuation lines aligned under the first
  output character, newlines kept, and the preview capped at a few lines
  with a `… (+N more lines)` note.
- Retry notices print as `INFO: ↻ message — retrying in Ns
  (attempt/max)`.
- A startup dump prints as `INFO: settings: …` — provider, model, and
  the merged settings (unset fields elided) — so a `-p` run's effective
  configuration is visible in a log with the session file's settings
  record next to it.

`-q` / `--quiet` (print mode only; rejected in TUI mode) prints just the
agent's response and errors — no thinking, tool calls, or tool results:

```sh
clanky -p -q "summarize this repo" > summary.txt
```

Like the TUI, a print-mode run records a session file under
`.clanky/sessions/` — the same records a TUI run would leave, including
the partial flush when the turn is cut short by an error or ctrl+c, so
`clanky -p "count files in /tmp"` leaves a resumable session and
subagent jobs stay inspectable afterwards. Pass `--no-session` (or set
`session = false` in settings.toml) to run without recording anything.

`--resume NAME` works in print mode too: the named session is loaded, the
turn continues it with the full prior conversation as context, and its
records append to the same file. A one-line `INFO: resuming …` note goes to
stderr, keeping stdout a clean stream of the answer. There is no picker in
a pipe, so print mode requires a NAME (the TUI's bare `--resume` opens the
picker). `--name NAME` names the new session file in print mode too;
`--name` with `--resume` is rejected in both modes with the same usage
error.

## Parallel subagents (`clanky-subagents`)

The `clanky-subagents` binary (built from the same workspace; install it on
`$PATH` alongside `clanky`, e.g. `cargo install --path crates/clanky-subagents`)
runs multiple Clanky subagents **in parallel** and returns their outputs as one
JSON object. It is designed to be invoked through the `bash` tool, but works
from any shell.

It takes a JSON run description as its single argument, or on stdin (`-`
or piped):

```sh
clanky-subagents '{"jobs":[{"id":"a","instruction":"Find all uses of X"}]}'
clanky-subagents - < jobs.json
```

Input:

```json
{
  "defaults": { "model": "meta-llama/Meta-Llama-3.1-70B-Instruct", "thinking": "off" },
  "jobs": [
    { "id": "a", "instruction": "Find all uses of X and summarize." },
    { "id": "b", "instruction": "Run the test suite and summarize failures.",
      "model": "Qwen/Qwen2.5-72B-Instruct" }
  ],
  "max_parallel": 4,
  "timeout_secs": 600
}
```

- `jobs` (required): each needs a unique caller-chosen `id` and an
  `instruction`; everything else clanky resolves from settings as usual.
- Optional per-job overrides, passed to the subagent's `clanky -pq` command
  line verbatim: `provider`, `model`, `thinking`, `sampling` (string in the
  `--sampling` CLI format), `max_tool_rounds`, `max_retries`, and
  `no_session` (boolean → `--no-session`). The provider validates them,
  never the runner.
- `defaults` (optional): any of the same fields, applied to jobs that don't
  set them (job-level wins per field).
- `max_parallel` (optional): defaults to all jobs at once.
- `timeout_secs` (optional): per-job timeout; a timed-out job's whole
  process group is killed and its partial output kept. No default.

Output: one JSON object on stdout, jobs in **input order**:

```json
{
  "jobs": [
    { "id": "a", "exit": 0, "stdout": "…the subagent's response…", "stderr": "" },
    { "id": "b", "exit": 1, "stdout": "…", "stderr": "…" }
  ]
}
```

Each stream is kept up to 1 MiB (the tail, with a truncation marker). The
runner itself exits 0 whenever it ran the batch — per-job failures are in
the JSON — and nonzero only for bad input (malformed JSON, empty `jobs`,
duplicate ids, unknown keys; the offending key is named). If `clanky` is
missing from `PATH`, each job reports `"error": "spawn: …"`.

Jobs run in the caller's working directory, so subagents pick up the same
project `.clanky/` and `~/.clanky` as any `clanky -pq` run; sessions are
never shared — but each job now records its own session file (as any
`-pq` run does), so pass `"no_session": true` per job (or in `defaults`)
when the batch should leave no files behind. Each job is a full Clanky
turn with its own tool loop — a run of N jobs costs roughly N times a
single-turn run in tokens, so use it where parallelism earns that.

Design details: `design-docs/parallel-subagents-plan.md`.
