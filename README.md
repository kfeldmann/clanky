![Clanky](clanky.jpg)
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

- Sampling keys and values are passed to the provider backend **verbatim**:
  any parameter the backend accepts works here (`seed`,
  `repetition_penalty`, `reasoning_effort`, `top_k`, …) and the backend
  validates them, so an invalid value fails at call time with the server's
  own error — Clanky never rejects or rewrites a sampling value itself.
  Values may be any TOML value; a string that is JSON (e.g. `"0.7"`) is sent
  as that JSON value, so `temperature = "0.7"` and `temperature = 0.7` mean
  the same thing (a string like `"high"` stays a string). The request's
  structural fields (`model`, `messages`, `tools`, `stream`, …) are set by
  the provider and cannot be overridden through `sampling`.
- `max_retries` covers transient provider failures — rate limits (429
  `Model busy`), backend errors, and network timeouts. Retries use
  backoff growing ×1.25 per retry (500 ms, 625 ms, 781 ms, 976 ms, 1.2 s,
  …, capped at 30 s), honoring a server
  `Retry-After` when it asks for longer, and are shown in the transcript
  (TUI) or on stderr (pipe mode). A round that already streamed content is
  never replayed, so a retry cannot duplicate output. Set it on the CLI
  with `--max-retries`.
- Precedence is per field: CLI flag > project file > user file, and a
  scope only overrides fields it actually sets. For `[sampling]` the merge
  is per key: keys set in a higher layer win, and keys set only in a lower
  layer are kept — `--sampling "top_p=0.9"` never drops a `temperature`
  from `settings.toml`.

### The `bash` tool

Commands run in the project directory with **no timeout**: they run until
they finish. If the model wants a deadline it passes `timeout_seconds`
(any positive number of seconds) on the call — that voluntary timeout is
honored, and the command is killed when it expires. Output sent to the
model is capped at 20 000 characters (then `… (output truncated)`).

### Session precedence note

Sessions always live in the **project scope** (`./.clanky/sessions/`);
there is no user-scope session store.

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
| LiteLLM proxy | `clanky-provider-litellm` | `LITELLM_API_KEY`, optional `LITELLM_BASE_URL` |
| Anthropic | `clanky-provider-anthropic` | `clanky-provider-anthropic login` (OAuth) — or `ANTHROPIC_API_KEY`; optional `ANTHROPIC_BASE_URL` |

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
  `high`/`xhigh`/`max`); `--thinking off` omits the field. Values pass
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
| `ctrl+u` | clear the input from the cursor to the beginning of the line |
| `ctrl+c` / `ctrl+d` | quit |
| `ctrl+l` | clear the visible screen |
| `ctrl+t` | toggle showing streamed thinking (display only; the session file always records it) |
| mouse wheel, scrollbar | scroll (native terminal scrolling) |
| text selection | copy/paste (native terminal selection) |
| `/` (empty input) | command palette |
| `/system` | show the assembled system prompt (context files and skills, as sent to the model) |
| `/md [flags] [file]` | export the session as a Markdown file (asks for a filename in a modal when none is given) |

### Rendering notes

The transcript prints straight to the terminal's normal buffer (no
alternate screen, no mouse capture), so native scrolling, selection, and
post-quit history all work. Two details of that model are visible:

- The rightmost column of the terminal is never painted. Writing the
  last column puts the terminal into a pending-wrap state whose
  follow-up behaviors differ across terminals (and GNU `screen` defers
  displaying a character written there until the screen scrolls), so the
  renderer reserves it and the last column stays blank.
- Resizing, returning from `$EDITOR`, and clearing redraw the bottom
  region in place; nothing is pushed into scrollback by a redraw.
- Multi-line input (paste, `$EDITOR`) renders as one row per line,
  indented under the prompt symbol, so every line starts at the same
  column; each row scrolls horizontally to keep the caret visible, and
  when the input outgrows the screen the rows around the caret are the
  ones shown.
- The status line de-contents gracefully as the terminal narrows:
  elements are dropped one at a time — provider, thinking budget,
  model, cost, session name, the word "queued" (leaving just the dot) —
  and finally the context percentage, so only the widest terminals show
  everything. When cost/context data is unavailable the hints
  (`/ commands · ctrl+c quit`) take their place on the right and are
  the last element to go.

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

## Print mode (`-p`)

When the TUI can't start (or `-p` is given), Clanky runs a single turn:
the agent's response goes to **stdout**, and activity — thinking, tool
calls, tool results, retry notices — to **stderr**, so
`clanky -p ... | grep` style pipes stay clean.

- Thinking blocks are printed as they stream and closed with a trimmed
  line plus a blank line, so each block reads as a unit.
- Tool results render like the TUI transcript, without the color: a
  `└─ name: …` header, continuation lines aligned under the first output
  character, newlines kept (never collapsed into a symbol), and the
  preview capped at a few lines with a `… (+N more lines)` note.

`-q` / `--quiet` (print mode only; rejected in TUI mode) prints just the
agent's response and errors — no thinking, tool calls, or tool results:

```sh
clanky -p -q "summarize this repo" > summary.txt
```

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
  `--sampling` CLI format), `max_tool_rounds`, `max_retries`. The provider
  validates them, never the runner.
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
never shared. Each job is a full Clanky turn with its own tool loop — a
run of N jobs costs roughly N times a single-turn run in tokens, so use
it where parallelism earns that.

Design details: `parallel-subagents-plan.md`.
