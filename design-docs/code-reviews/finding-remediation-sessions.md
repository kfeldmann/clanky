# Clanky session: 20261024-041546-af1f

| | |
|---|---|
| Provider | deepinfra |
| Model | zai-org/GLM-5.3-Flash |
| Created | 2026-10-24 04:15 |
| Contents | user and assistant messages |

## User

For the findings listed in /work/code-review-20261023.md which are not marked 'DONE', please identify groupings of findings that can and should be addressed in the same session. Produce a list of sessions with the findings to remediate in each session.

## Assistant

I'll first re-read the review (done above) and quickly sanity-check that the non-DONE findings map to the files claimed, so the groupings reflect real code adjacency.

## Assistant

All 29 findings not marked DONE are accounted for. Here's the grouping, based on which findings share files, share a root cause or helper function, or share test scaffolding — i.e., would naturally be touched (and regression-tested) in one sitting.

## Session 1 — Core turn/tools: retry & deadline correctness (turn.rs, tools.rs)

**DONE**

| # | Finding |
|---|---|
| 13 | ALREADY DONE - `is_failed_result` substring match misclassifies successful tool output as failed |
| 14 | Server `Retry-After` > 30 s clamped to `MAX_RETRY_DELAY` — retries sooner than the server allows |
| 15 | Huge per-call `timeout_seconds` overflows `Instant` and silently becomes *no timeout* (also blesses the `u64::MAX` unit test) |
| 16 | `view-image` has no size cap (multi-GB / FIFO read base64'd into the session) |

- 14. **`turn.rs:384`** — A server `Retry-After` > 30 s is clamped to `MAX_RETRY_DELAY`, so we retry sooner than the server allows (then get rejected again, burning attempts). The doc comment says the clamp "never knocks sooner than backoff" — but the server's hint wins only up to the cap.
- 15. **`turn.rs:229` / `tools.rs:87`** — A huge per-call `timeout_seconds` (e.g. `u64::MAX`, which the unit test at `tools.rs:906` explicitly blesses) overflows `Instant` and silently becomes *no timeout* — a user value rewritten into its opposite. Needs either a clamp-with-note or an honest "too large" error.
- 16. **`tools.rs:625-638`** — `view-image` has no size cap: a model-directed path to a multi-GB file (or `/dev/zero`, or a FIFO) is read wholly, base64'd (~1.33×), and written into the session file. A `metadata().len()` check would turn this into a clean error.

**Why together:** 14 and 15 are both deadline/timeout arithmetic in `turn.rs`/`tools.rs` with the same "user value silently transformed" flavor; 13 and 16 are small `tools.rs` result/output-handling fixes that share the tool-output test fixtures.

## Session 2 — Process-group kill & cancel races (cross-crate: tools.rs, subagents, protocol)

**DONE**

| # | Finding |
|---|---|
| 17 | tools.rs: group KILL issued after `wait()` reaps the child — pid-recycle can signal an unrelated group |
| 27 | subagents: instruction pushed as trailing positional without `--` — leading `-` breaks clap |
| 28 | subagents: failed group kill → pipe-holding grandchildren block un-timeouted `join()`s |
| 29 | subagents: `killpg` failure silently discarded |
| 30 | protocol: cancel deadline armed before the cancel write; failed write leaves a stale deadline |
| 31 | protocol: cancel for an already-finished request flips the flag with no restore |
| 37 | protocol: detached stderr thread loses the final partial line, outlives the transport |

- 17. **`tools.rs:240-253, 589-594`** — The process-group KILL is issued *after* `child.wait()` reaps the direct child, using the cached pid as pgid up to ~2 s later; on pid recycle the signal can hit an unrelated group. Narrow race, unguarded `kill`.
- 27. **`subagents/main.rs:174-183`** — The instruction is pushed as a trailing positional arg; an instruction with a leading `-` is flag-parsed by clap and the job fails with a clap error. Push `--` before it.
- 28. **`subagents/main.rs:236-250`** — If the process-group kill fails, pipe-holding grandchildren block the *un-timeouted* `join()`s, so a job can run past `timeout_secs`.
- 29. **`subagents/main.rs:154-165`** — `killpg` shells out to the external `kill -PID`; failure is silently discarded (`let _`).
- 30. **`process.rs:112-118`** — The cancel deadline is armed *before* the cancel write; if the write fails, a stale deadline survives and can kill a later innocent request via the `CancelHandle` path (the `send` path clears it, but a cancel-while-idle doesn't).
- 31. **`serve.rs:225-230`** — A cancel for an already-finished request flips the shared flag with no matching restore; masked only by the next chat's `reset()`. Latent inconsistency; worth a test.
- 37. **`process.rs:364-372`** — Detached stderr thread loses the final partial log line and outlives the transport; matters only for crash forensics.

**Why together:** 27–29 are all in `clanky-subagents/src/main.rs`; 17 is the *same* `kill -pgid` shell-out pattern duplicated in `tools.rs` (`kill_process_group`) — hardening one without the other leaves the divergent copies. 30/31 are the cancel-flag lifecycle in `process.rs`/`serve.rs` and belong with the same kill/cancel test scaffolding.

## Session 3 — CLI/pipe mode & session-file hygiene (main.rs, recorder.rs, session.rs, editor.rs)

**DONE**

| # | Finding |
|---|---|
| 10 | `-q` without `-p` silently ignores `quiet` and launches the TUI (`QuietRequiresPrint` is dead code) |
| 18 | `PrintSink` swallows write errors (`writeln!(...).ok()`) — closed stdout doesn't stop the turn |
| 19 | Invalid `--name` reported but session proceeds under an auto-generated name (user-values invariant; doc contract broken) |
| 32 | editor.rs temp file: no `O_EXCL` — collision silently overwrites the buffer being edited |
| 38 | Session/tool-output/plugin-stderr files written world-readable (0o644) with unredacted content |

- 10. **`crates/clanky/src/main.rs:63-76`** — `clanky` with `-q` but no `-p` silently ignores `quiet` and launches the TUI (the `cli.print || !tui::available()` branch just passes `quiet` into pipe mode). The documented rejection exists (`error.rs:28 QuietRequiresPrint: "--quiet is only available in print mode"`) but nothing ever constructs it — dead code, and the CLI help's "Rejected in TUI mode" doesn't happen.
- 18. **`main.rs:354-364`** — All `PrintSink` writes do `writeln!(...).ok()`, so a closed stdout (`clanky -p … | head -1`) doesn't stop the turn: it keeps calling the model and tools with output going nowhere and exits 0. Deliberate for the session recorder, but wrongly applied to the response channel too.
- 19. **`main.rs:133-135` + `recorder.rs:74-83`** — An invalid `--name` is reported to stderr but the session proceeds under an auto-generated name; the doc comment claims the recorder is dropped, and it isn't at either call site. Touches the user-values invariant: the explicitly requested name is silently replaced. Honor the contract (disable recording / refuse) or fix the doc.
- 32. **`editor.rs:32-36`** — Temp file name is `pid + subsec_nanos` with no `O_EXCL`; a collision silently overwrites the buffer being edited. Use create-exclusive semantics.
- 38. **`session.rs:153-158` / `provider.rs:162-166`** — Security-adjacent: tool output (e.g. a model running `env`) and plugin stderr land unredacted in world-readable (0o644) files; no permission tightening or warning anywhere. A code change (0o600 on session/log files) would be cheap insurance.

**Why together:** all touch the print/record/session durability pipeline in `main.rs`, and 19 + 38 are both file-creation policy decisions (name honoring, permissions) that should be settled consistently in one pass.

## Session 4 — TUI rendering correctness (screen.rs, markdown.rs, app.rs)

**DONE** (35 tabled, see below)

| # | Finding |
|---|---|
| 33 | DONE - `input_window` can park the caret one cell off with wide (CJK) chars |
| 34 | DONE - tail could be taller than the screen, so idle redraws scrolled one row per frame |
| 35 | TABLED - `screen_records` tool-call/turn attribution with queued prompts (see note) |
| 36 | DONE - table `sum_min >= avail` branch emitted rows wider than the viewport (`cap_lines` was already correct) |

- 33. **`screen.rs:976`** — `input_window` can return a row one column wider than `view_width` with wide (CJK) chars, parking the caret one cell off briefly.
- 34. **`screen.rs:259-262`** — On a fresh anchor with `new_rows >= height`, `saturating_sub` + `min(height-1)` clamps `top` to the bottom row; the tail is written one row too low and the off-by-one persists on subsequent redraws (verified by the reviewer with an emulator of the code path).
- 35. **`app.rs:602-613`** — `screen_records` (the `/md` export path) can attribute tool calls to the wrong turn when prompts are queued mid-turn. UI-only.
- 36. **`markdown.rs:196-204, 431-441`** — `cap_lines` note row and the table `sum_min >= avail` branch can each emit rows one column too wide; cosmetic truncation.

**As built.** 33: `input_window` now budgets a character's whole display
width (a two-column char starting in the window's last column did not fit)
and snaps the window start down to a character boundary — the latter was the
actual off-by-one: the ideal start could land mid-character, so the window
began with that character's tail, came out one column wide, and had its
caret column computed from the character the renderer then truncated. The
caret is also clamped to the painted row (a wide character in a viewport
narrower than itself). Test:
`input_window_never_exceeds_its_width_with_wide_characters`.

34: the reviewer's `min(height - 1)` is a no-op (the `saturating_sub` already
yields 0); it is removed anyway, together with the `height == 0` underflow.
The real defect is that the tail could be *taller than the screen*: `keep`
had a `.max(1)`, so when the footer filled the screen on its own (a popup
plus a multi-line input on a short terminal) the entry kept one row on top
of it and `tail_height` became `height + 1`. The next redraw's relative
`MoveUp(tail_height - 1)` is clamped at the top row, so it started one row
above the tail's own top and scrolled the screen by one row *per frame* —
exactly the persistent off-by-one reported. Fixed by dropping the `.max(1)`
(the tail now always fits, and the footer gets the whole screen when it
needs it). Test: `a_tail_taller_than_the_screen_does_not_crawl` (idle
redraws must not grow the scrollback).

36: `cap_lines` needed no change — content is cut to `width - note_width`, so
its rows are at most `width` wide (a sweep over 40 widths and 5 samples found
no overflow). The table branch floored per column and gave every column at
least one, so with narrow columns the total landed above `avail`; the
overshoot is now handed back one column at a time, widest first, never below
one. Test: `table_over_floor_scaling_still_fits_the_viewport`. Below
`1 + 3*cols + cols` columns no layout can fit at all — that degradation stays
(`table_degenerate_width_does_not_panic`).

35: tabled. `screen_records` derives records from the transcript entries, and
a queued prompt's placeholder is UI-only while its real `User` entry is
appended when it is dispatched (after the previous turn's entries). A tool
call therefore lands in the round it actually belongs to; the export is
UI-only and was judged not worth changing without a concrete repro.

**Why together:** all are cosmetic/visual rendering bugs best verified with the same emulator-style test harness (the reviewer already built one for #34); fixing them separately would rebuild that harness each time.

## Session 5 — OpenAI-compatible provider plugins (deepinfra, litellm, examples)

**DONE**

| # | Finding |
|---|---|
| 20 | Tool-args fragment before id/name is dropped → truncated/garbage assembled args (both plugins) |
| 21 | DeepInfra legacy `finish_reason: "function_call"` maps to `Stop`, dropping tool calls (litellm already handles it) |
| 22 | Bare `u32` index with `<=` growth — malformed `"index": 4294967295` allocates ~4 B (both plugins) |
| 23 | litellm context-window hint falls back to *output* `max_tokens`, badly understating the window |
| 39 | hello-world example: `KeyError` on missing `id` kills the plugin, contradicting its own junk-tolerance policy |

- 20. **`provider.rs` (deepinfra 533 / litellm 701)** — When a tool-args fragment arrives on the first delta *before* id/name (the code's own comment says some backends lead with index-only deltas), the early `return` skips argument ingestion and the fragment is lost, producing truncated/garbage assembled args. Buffer args instead of dropping them.
- 21. **`provider.rs` (deepinfra 591)** — Legacy `finish_reason: "function_call"` maps to `Stop`, discarding announced tool calls. LiteLLM's copy of the same OpenAI-compat mapping handles it (with a regression test); DeepInfra's doesn't. Same wire format, divergent behavior.
- 22. **`provider.rs` (deepinfra 517 / litellm 687)** — `while self.calls.len() <= call.index` with a bare `u32` index: one malformed `"index": 4294967295` chunk allocates ~4 B accumulators (OOM). Clamp or reject absurd indices as a protocol error.
- 23. **`litellm/provider.rs:559`** — Context-window hint falls back to `max_tokens`, which in LiteLLM's `model_info` is the *output*-token limit — badly understates the window for models that only populate it.
- 39. **`examples/hello-world-provider`** — Uncaught `KeyError` on a known message missing `id` kills the plugin, contradicting the file's own "never die on junk" policy. One `.get("id")` fixes it.

**Why together:** 20/21/22 are the same streaming-delta handling code duplicated across the two plugins — they should be fixed in lockstep (and the litellm regression test for #21 ported). #39 is the same leniency theme in the example template.

## Session 6 — Anthropic plugin semantics (adapter.rs, provider.rs, backend.rs)

**DONE**

| # | Finding |
|---|---|
| 11 | `pause_turn` mapped to `Stop` — silently truncates long turns, drops the resume contract |
| 24 | `body_text.contains("invalid")` maps unrelated OAuth errors to "run `login`" |
| 25 | HTTP-date `Retry-After` unparsed (integer-seconds only), unlike the other backends |
| 26 | Claude-Code identity block injected on the API-key path, contradicting the documented design |

- 11. **`crates/clanky-provider-anthropic/src/adapter.rs:305-313`** — `pause_turn` maps to `FinishReason::Stop`, and `map_stop_reason`'s comment says "anything unknown maps to a plain stop". For Anthropic specifically, `pause_turn` means "turn interrupted server-side, resume with the same request" — mapping it to Stop silently truncates long turns and drops the continuation contract. (DeepInfra/LiteLLM don't have this concept; this is Anthropic-specific semantics lost in the mapping.)
- 24. **`anthropic/provider.rs:280`** — `body_text.contains("invalid")` maps any 400/401 OAuth error containing the substring (e.g. "invalid_client", but also unrelated text) to "run `login`"; a genuinely non-recoverable error gets the wrong actionable message.
- 25. **`anthropic/backend.rs:117`** — HTTP-date `Retry-After` is unparsed (only integer seconds); the DeepInfra/LiteLLM backends handle the date form. Inconsistent retry-hint coverage.
- 26. **`anthropic/adapter.rs:39`** — The Claude-Code identity system block is injected on the API-key path too, contradicting the documented design (identity block is the OAuth path's substitute for not having real Clanky identity).

**Why together:** all four are Anthropic-specific request/response semantics in one plugin; the other Anthropic findings (#2, #4–#6, #8) are already DONE, so this finishes the crate. Note #11 may also touch `clanky-protocol` if `FinishReason` needs a new variant — worth doing before/with Session 2 if the protocol type changes.

## Suggested order

1. **Session 1** — everyday-use correctness (retry/timeout misbehavior), self-contained.
2. **Session 5** — data-loss class (dropped tool args / calls), two plugins in lockstep.
3. **Session 6** — Anthropic turn truncation (#11) is the last remaining semantics loss; do before Session 2 if it changes protocol types.
4. **Session 2** — kill/cancel race hardening; consider deduplicating the `killpg` helpers while there.
5. **Session 3** — CLI contract + file hygiene.
6. **Session 4** — TUI rendering polish; lowest risk, purely visual.


---

_session total: 16076 prompt tokens · 6940 completion tokens_

---

_Last turn ended with stop reason `stop`._
