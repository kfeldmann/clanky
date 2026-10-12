---
name: parallel-subagents
description: Run multiple clanky subagents in parallel via the clanky-subagents binary and the bash tool. Use when a task decomposes into several independent agent-sized subtasks — e.g. investigate several files or areas at once, review and test in parallel, or gather answers from multiple independent questions.
---

# Parallel subagents

Run several independent Clanky subagents at once and get all their outputs in
one JSON result. Each job is a full `clanky -pq` run (its own agentic loop and
tool calls), so reserve it for agent-sized work, not for plain shell commands —
use the bash tool directly for those.

## When to use

- The task splits into 2+ **independent** subtasks, each of which would itself
  need an agent (multi-file investigation, open-ended exploration, review).
- A cheaper/different model or settings are wanted for some subtasks.
- Subtasks don't depend on each other's results. If they do, run them
  sequentially or in a second batch.

Cost warning: N jobs ≈ N full agent turns in tokens. Don't spawn jobs for
trivial lookups.

## How to run it

Single bash tool call with the JSON piped on stdin (heredoc keeps quoting
simple and avoids shell-quoting bugs):

```bash
clanky-subagents <<'JSON'
{
  "jobs": [
    { "id": "auth", "instruction": "Find all places in src/ where auth tokens are handled. Summarize each location with file:line and what it does." },
    { "id": "tests", "instruction": "Run the test suite and summarize failures. Do not fix anything." }
  ]
}
JSON
```

## Input fields

| Field | Meaning |
|---|---|
| `jobs` (required) | Array of `{ "id", "instruction", ...overrides }`; ids are your own labels, must be unique |
| `defaults` | Option overrides applied to every job that doesn't set them (e.g. `{ "model": "...", "thinking": "off" }`) |
| `max_parallel` | Concurrency cap; defaults to all jobs at once |
| `timeout_secs` | Per-job timeout (kills the job's process group); no default |

Optional per-job overrides (and in `defaults`), passed to the subagent's clanky
command line verbatim: `provider`, `model`, `thinking`, `sampling` (string,
`--sampling` format like `"temperature=0.7,top_p=0.9"`), `max_tool_rounds`,
`max_retries`.

Jobs run in your cwd, so subagents see the same files and settings you do;
sessions are never shared — the instruction is the subagent's entire prompt,
so write it self-contained.

## Reading the result

The tool result is one JSON object, jobs in **input order**:

```json
{ "jobs": [ { "id": "auth", "exit": 0, "stdout": "...", "stderr": "" } ] }
```

- The subagent's answer is in `stdout`. `stderr` holds activity/errors.
- `exit` is the subagent's own exit code; nonzero there means the job itself
  reported failure — read its `stdout`/`stderr` for why. The runner exits 0
  whenever it ran the batch, so a plain zero exit on your bash call does
  **not** mean all jobs succeeded: check each `exit`.
- A timed-out or unspawnable job has an `error` field instead of `exit`
  (with partial output kept for timeouts).
- Only unknown keys, duplicate ids, empty `jobs`, or malformed JSON make the
  runner exit nonzero.

## Writing instructions

Each subagent sees only its instruction. Include everything it needs: the
goal, relevant paths, what to return, and constraints ("do not modify files",
"summarize, don't fix"). A good instruction is one you could hand to a
colleague with no other context.

## Practical notes

- Pipe the JSON on stdin via heredoc (`<<'JSON'`) rather than shell-quoting it
  as an argument — much less error-prone for long instructions.
- Give jobs descriptive ids: they name the results.
- Prefer 2–5 focused jobs over many tiny ones; batch size is token cost.
- Sequential dependent steps = separate batches: run one batch, read the
  results, decide the next.
