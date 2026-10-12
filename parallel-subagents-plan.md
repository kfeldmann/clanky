# Plan — Parallel subagents

> **Status: implemented.** All six decisions below were settled and shipped
> (see `milestones.md` M13 for as-built notes and
> `crates/clanky-subagents/tests/subagents.rs` for the test suite).

## Motivation

The model can already run subagents synchronously via the bash tool
(`clanky -pq '<instruction>'`). Running several *in parallel* has no good story:
`nohup clanky -pq ... >log` fans out but gives the caller no way to know when a
job finished, and ad-hoc `&`/`wait` shells interleave output with no reliable
attribution. GNU `parallel` gets close but is absent from the Alpine image, has
shell-quoting landmines, and its output contract isn't JSON.

The missing piece is not launching — it is **reliable attribution and
completion signaling**. A tiny purpose-built binary that fans out jobs, waits
for all of them, and returns one structured JSON result closes the gap.

## Non-goals (v1)

- No streaming/partial progress — the bash tool call blocks until all jobs
  finish (voluntary `timeout_seconds` still applies, as with any bash call).
- No shared session or context between parent and subagents; each subagent is
  a fresh `clanky -pq` process.
- No scheduling, priorities, or retries — each job runs exactly once.
- No new clanky-core machinery: no built-in tool, no protocol changes. The
  binary is invoked *through* the existing bash tool.

## Interface

### Invocation

```
subagents '<input-json>'      # JSON as argv[1]
subagents < jobs.json         # JSON on stdin
subagents - < jobs.json       # explicit stdin
```

Reading stdin as well as argv avoids the classic model failure of quoting a
long, instruction-heavy JSON string inside a bash `-c` command. The argv form
stays because it's the convenient one-liner.

### Input

```json
{
  "defaults": { "model": "meta-llama/Meta-Llama-3.1-70B-Instruct", "thinking": "off" },
  "jobs": [
    { "id": "search",  "instruction": "Find all uses of X and summarize." },
    { "id": "tests",   "instruction": "Run the test suite and summarize failures.",
      "model": "Qwen/Qwen2.5-72B-Instruct" }
  ],
  "max_parallel": 4,
  "timeout_secs": 600
}
```

- `jobs` (required, non-empty). Each job:
  - `id` (required, caller-chosen, unique). The model picks the ids, so the
    request/response mapping is self-documenting.
  - `instruction` (required). The subagent's prompt.
  - Optional clanky option overrides (see Decision 1):
    `provider`, `model`, `thinking`, `sampling`, `max_tool_rounds`,
    `max_retries`.
- `defaults` (optional). Any of the same option overrides, applied to every
  job that doesn't set that option itself. Job-level wins per field; nothing
  is merged or clamped — a field is either the job's own, the run default, or
  left to clanky's normal settings resolution.
- `max_parallel` (optional). Defaults to `jobs.len()` — parallel is the whole
    point — but a cap matters since each job is a full clanky process making
    its own API calls. A provided value is passed through as-is (invariant:
    never clamp user values).
- `timeout_secs` (optional). Per-job timeout, applied to each subprocess
    individually, not to the batch. Default: **no timeout** (see Decision 4).

### Output

One JSON object on stdout, jobs in **input order** regardless of completion
order:

```json
{
  "jobs": [
    { "id": "search", "exit": 0, "stdout": "...", "stderr": "" },
    { "id": "tests",  "exit": 1, "stdout": "...", "stderr": "3 tests failed..." }
  ]
}
```

Per-job failure fields (exactly one of these replaces stdout/stderr on
abnormal completion):

- spawn failure: `{ "id": ..., "error": "spawn: <message>" }`
- timeout: `{ "id": ..., "error": "timeout after <n>s", "stdout": "...", "stderr": "..." }`
  (partial output is still returned — it's the only evidence of what happened)

## Design decisions

### Decision 1 — Jobs launch clanky, with pass-through option overrides

Each job runs `clanky -pq '<instruction>'` as a subprocess — the runner is a
clanky launcher, never a general command runner (arbitrary `command` strings
would reinvent GNU parallel and invite quoting bugs).

A key subagent use case is running a *different model or settings* than the
parent session, so each job accepts optional overrides mirroring clanky's
relevant CLI flags, as optional JSON fields:

- `provider` (string), `model` (string), `thinking` (string, e.g. `"1024"`
  or `"off"`), `max_tool_rounds` (integer), `max_retries` (integer) — passed
  to the subprocess as `--provider`, `--model`, etc., verbatim.
- `sampling` (string in clanky's CLI format, e.g.
  `"temperature=0.7,top_p=0.9"`). We deliberately do **not** accept a JSON
  object here: the string form is exactly what `--sampling` takes, is parsed
  by clanky itself, and keeps the runner from re-implementing the format.
  Invariant: the runner passes values through untouched; the provider
  validates them.

Not accepted anywhere in the input (the runner controls these): `-p`, `-q`,
`--resume`, `--help`, `--version`, prompt sources. They are not `deny`
lists enforced at runtime — the runner simply never constructs argv from
anything outside the allow-list above.

An optional run-level `defaults` object carries any of the same fields,
applied per-field to jobs that omit them (job wins per field). This lets the
caller say "run everything on the cheap model, except this one job" without
repeating the override N times.

**Recommendation: as above (accepted).**

### Decision 2 — In-memory capture with per-stream caps

Each job's stdout/stderr is captured in memory, capped at **1 MiB per
stream**. On overflow, keep the *last* 1 MiB (tail — the end of a clanky run
is the answer) and prefix a `"[output truncated, kept last 1 MiB]"` marker.
No temp files, no cleanup, no path-collision concerns.

**Recommendation: as above; cap is a compile-time constant in v1.**

### Decision 3 — The binary always exits 0 if it ran the batch

Per-job exit codes are reported in the JSON. Rationale: clanky's bash tool
marks a result as failed if the command exits nonzero (`[exit status: N]`),
which would flag the whole batch when one job fails and risk burying the
successful jobs' outputs. "Runner succeeded" vs "jobs succeeded" are
different facts; the JSON is the honest carrier for the latter. The runner
itself exits nonzero only when it fails to run at all (bad JSON, no jobs) —
cases where there is no result JSON to consult.

**Recommendation: exit 0 on any completed batch; per-job exits in JSON.**

### Decision 4 — Per-job timeout optional, default unlimited

Consistent with the bash tool's own stance: commands run unbounded, and a
voluntary timeout is the only deadline. The parent bash call may already
carry `timeout_seconds`; adding a default inside the runner would silently
contradict the parent's choice. A hung job blocks its slot, but
`max_parallel` bounds concurrent hang cost, and the caller can pass
`timeout_secs`.

**Recommendation: no default timeout; optional `timeout_secs` per run.**

### Decision 5 — Context inheritance: cwd yes; settings via explicit overrides

Jobs run in the runner's cwd (i.e., the project directory the parent bash
tool already operates in), so each subagent picks up the project's
`.clanky/` and the user's `~/.clanky` exactly like any `clanky -pq` run.
Session *content* is never inherited — each subagent starts fresh, with the
instruction as its entire prompt. Model and settings come from normal
settings resolution plus the job's explicit overrides (Decision 1).

**Recommendation: cwd inherited; overrides per Decision 1 (accepted).**

### Decision 6 — Separate crate, not a clanky subcommand

`crates/clanky-subagents/` — a small binary crate, deps: `serde`,
`serde_json` only. It needs zero knowledge of clanky internals (just
`std::process`), so it stays out of core's dependency graph, like the
providers. A `clanky subagents ...` subcommand would be more discoverable
but couples the runner to core's CLI surface for little gain — and the
bash-tool invocation path doesn't need it. Ship the binary name
`clanky-subagents` (namespaced like the providers) rather than bare
`subagents` to avoid PATH collisions.

**Recommendation: `crates/clanky-subagents`, binary `clanky-subagents`.**

## Failure modes summary

| Case | Behavior |
|---|---|
| Malformed JSON / empty `jobs` | nonzero exit, error on stderr, no result JSON |
| Duplicate job ids | nonzero exit (the caller's contract is violated; silent merging would be worse) |
| Unknown key in input JSON | nonzero exit with the offending key named |
| Job fails to spawn | job entry with `"error": "spawn: ..."` |
| Job times out (if `timeout_secs` set) | job entry with `"error": "timeout ..."`, partial output kept |
| Stream exceeds cap | tail-kept with truncation marker |
| Job exits nonzero | reported as `"exit": N` with its output |

## Testing strategy

- **Unit tests** with a stub `clanky` earlier on `PATH`: a tiny script
  binary that echoes its argv to stdout, optionally exits N, optionally
  hangs — covers exit capture, ordering, caps, timeouts, and that option
  overrides/defaults land in the subprocess argv correctly.
- **E2E**: `crates/clanky/tests/` — run `clanky-subagents` with jobs whose
  stub outputs are deterministic; assert the JSON shape, input ordering,
  and per-job attribution.
- The existing golden-session machinery doesn't change (no core changes),
  but a golden transcript exercising a parallel call would document the
  intended model behavior — nice-to-have, not a gate.

## Documentation updates (ship with the change)

- README: a "Parallel subagents" section — invocation, JSON schemas,
  the token-cost warning (each job is a full clanky run; N jobs ≈ N× the
  parent's per-turn cost), and the PATH/`cargo install` note like the
  providers have.
- milestones.md: checkbox under M12 (or a new M13 if you prefer it to
  stand alone) plus an as-built note.

## Work breakdown

1. `crates/clanky-subagents` skeleton: arg/stdin input parsing, serde
   types (with `deny_unknown_fields`, so unknown keys are named errors),
   validation errors, argv construction from the allow-listed options.
2. Job runner: spawn `clanky -pq` per job, bounded-concurrency scheduler
   (a simple worker pool over a job queue; no async needed), capture with
   caps, per-job timeout via `wait_timeout` (already a workspace dep).
3. Result assembly: input-order JSON, failure fields, exit 0 semantics.
4. Unit tests with stub `clanky`; e2e test in `crates/clanky/tests/`.
5. README + milestones updates.

Estimate: small — the crate is ~300 lines plus tests; no core changes.
