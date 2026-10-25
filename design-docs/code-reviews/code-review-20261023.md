# Clanky session: 20261023-210913-9d09

| | |
|---|---|
| Provider | deepinfra |
| Model | zai-org/GLM-5.3-Flash |
| Created | 2026-10-23 21:09 |
| Contents | user and assistant messages |

## User

Please do a code review on this project.

## User

It looks like some of the agents took longer than 1800 seconds to complete their review. They are all done now. Please read the reviews from the session files here:

/work/.clanky/sessions/20261023-210955-e6a4.jsonl
/work/.clanky/sessions/20261023-210955-c92c.jsonl
/work/.clanky/sessions/20261023-210955-75c7.jsonl
/work/.clanky/sessions/20261023-210955-d2d0.jsonl

## Assistant

All four reviews are in. Before reporting, let me spot-check the highest-severity claims against the actual code, since review agents can occasionally overstate a finding.

## Assistant

I've verified the highest-severity claims against the actual source — all check out. Here's the consolidated review.

# Code Review — clanky

Four parallel review agents covered the whole workspace: core loop/tools/sessions, TUI, protocol/subagents/recorder, and the three provider plugins. I independently spot-checked the high and medium findings below; all are confirmed real.

## Overall assessment

The codebase is in good shape. Error handling in the turn loop is solid (failures become data, not panics), the OpenAI-compatible providers are lenient-but-safe against malformed server data, there are no command-injection or path-traversal issues beyond documented design decisions, and the sampling pass-through logic correctly respects the "user values are authoritative" invariant everywhere I checked. The Anthropic plugin concentrates most of the serious problems, largely because it hand-rolls more of the stack (auth, PKCE, streaming-only chat path, Retry-After parsing) instead of sharing code.

## High

1. DONE **`crates/clanky-protocol/src/messages.rs:14`** — `MediaType` derives `#[serde(rename_all = "snake_case")]`, so `WebP` serializes as `"web_p"` — but the protocol spec (`design-docs/provider-protocol.md:151`) says `"webp"`, and the enum's own `as_str`/`from_str` (`"webp"`) and tests use that spelling. The serde path disagrees with everything else in both directions: a plugin emitting spec-valid `"webp"` fails serde deserialization, and a Rust-side serde serialize emits a string no spec-compliant peer accepts. Fix: `#[serde(rename = "webp")]` on the variant (or rename the enum variant).

2. DONE **`crates/clanky-provider-anthropic/src/provider.rs:351` + `backend.rs:83`** — If the user puts `stream = false` in `sampling`, `adapter::build_request_body` correctly honors it, but `chat()` unconditionally calls `post_stream` and feeds the response through the SSE `StreamState` parser. A non-streamed JSON response body parsed as SSE either errors or produces garbage; the documented legal non-streaming path (`post_message`, kept as "the canonical reference mapping") is dead code from the provider's perspective. The user's explicit value is accepted then functionally discarded — an invariant violation with a silent failure mode.

## Medium

3. DONE **`crates/clanky/src/tui/markdown.rs:285`** — In `table_delimiter`, `&c[1..]`-style slicing on a delimiter cell like `":"` (left=true, right=true, len=1) computes `c[1..0]` — a byte-range panic. Markdown tables come from *model output*, so any assistant message containing `| : | : |` remotely panics the whole interactive session. Two-line bounds check to fix. (Confirmed: `c.len() - 1 == 0` with `left` true.)

4. DONE **`crates/clanky-provider-anthropic/src/oauth.rs:256`** — `parse_token_body` embeds `truncate(&body)` (up to 200 chars) in the error when the token response isn't valid JSON. A token endpoint that returns 200 with an HTML/login page or a partially-formed body containing the real token leaks the access/refresh token into the surfaced error message, which then lands in logs and session files.

5. DONE **`crates/clanky-provider-anthropic/src/pkce.rs:38`** — The PKCE verifier is generated from `RandomState` + clock + counter, not a CSPRNG. The module comment argues `RandomState`'s per-process OS seed makes it OK, but successive draws within a process are counter-derived and `RandomState` is not a cryptographic primitive; RFC 7636 §4.1 and OAuth BCP require a CSPRNG. Adding `getrandom` is the honest fix.

6. DONE **`crates/clanky-provider-anthropic/src/provider.rs:157-166`** — The `Some(Err)` credential cache short-circuits *all* subsequent requests for the life of the process, but the README documents a live-relogin path (`clanky-provider-anthropic login` while Clanky is running). After one expired-login, re-login on disk is never re-read until restart — documented workflow silently doesn't work.

7. DONE **`crates/clanky/src/tui/app.rs:891-898`** — `insert_str` (the `Event::Paste` path, `mod.rs:913`) does not clear `self.completion`. If the completion popup is open during a paste, the state now describes a token range in a stale `input`; the next Tab runs `cycle_completion`, which overwrites the input with `prefix + candidate + suffix` — silently discarding the pasted text. Clear `completion` in `insert_str` like the other input-mutating paths do.

8. DONE **`crates/clanky/src/tui/screen.rs:561-563`** — The modal (e.g. `/model` picker prompt-for-input) caret row is hardcoded to the tail's row 0 (the title border row) instead of the field row, so the caret renders on the wrong line in modals.

9. DONE **`crates/clanky/src/tools.rs:229, 568-582`** — With no deadline configured (the default: `UNSET_DEADLINE` overflows `Instant`, so `deadline` is `None`), `join_with_deadline` takes the plain-`join()` branch. A bash command like `sleep 5 & echo ok` leaves a grandchild holding the stdout pipe after the direct child exits: the reader never sees EOF and the tool call hangs forever. The process-group kill machinery exists but is only armed when a deadline is set. (I actually reproduced this class of hang earlier today with the built-in bash tool.)

10. DONE **`crates/clanky/src/main.rs:63-76`** — `clanky` with `-q` but no `-p` silently ignores `quiet` and launches the TUI (the `cli.print || !tui::available()` branch just passes `quiet` into pipe mode). The documented rejection exists (`error.rs:28 QuietRequiresPrint: "--quiet is only available in print mode"`) but nothing ever constructs it — dead code, and the CLI help's "Rejected in TUI mode" doesn't happen. Fixed: the front-end choice is now `select_front_end` in main.rs, which returns `Error::QuietRequiresPrint` when the TUI would launch with `-q`; the redirected-stdin/stdout fallback still counts as print mode. Covered by four unit tests in main.rs.

11. **`crates/clanky-provider-anthropic/src/adapter.rs:305-313`** — `pause_turn` maps to `FinishReason::Stop`, and `map_stop_reason`'s comment says "anything unknown maps to a plain stop". For Anthropic specifically, `pause_turn` means "turn interrupted server-side, resume with the same request" — mapping it to Stop silently truncates long turns and drops the continuation contract. (DeepInfra/LiteLLM don't have this concept; this is Anthropic-specific semantics lost in the mapping.)

12. DONE **`crates/clanky-protocol/src/process.rs:269`** — On the client side, a malformed line from a live plugin becomes a terminal `Err` in the response stream, aborting the in-flight turn and desyncing line framing for later turns. Contrast with `serve.rs:159` where the *plugin* side ignores malformed lines per spec §9. The asymmetric strictness means a single glitchy stderr-bleed line kills the session instead of being skipped like unknown types are.

## Low

13. DONE **`tools.rs:669-671`** — `is_failed_result` matches `output.contains("[exit status: ")`, so any *successful* tool whose output merely echoes that marker (e.g. the tool printing its own review, a `cat` of a session file) is misclassified as failed. Use `starts_with` on the exact formatted tail, or check the actual `ExitStatus` upstream.
14. **`turn.rs:384`** — A server `Retry-After` > 30 s is clamped to `MAX_RETRY_DELAY`, so we retry sooner than the server allows (then get rejected again, burning attempts). The doc comment says the clamp "never knocks sooner than backoff" — but the server's hint wins only up to the cap.
15. **`turn.rs:229` / `tools.rs:87`** — A huge per-call `timeout_seconds` (e.g. `u64::MAX`, which the unit test at `tools.rs:906` explicitly blesses) overflows `Instant` and silently becomes *no timeout* — a user value rewritten into its opposite. Needs either a clamp-with-note or an honest "too large" error.
16. **`tools.rs:625-638`** — `view-image` has no size cap: a model-directed path to a multi-GB file (or `/dev/zero`, or a FIFO) is read wholly, base64'd (~1.33×), and written into the session file. A `metadata().len()` check would turn this into a clean error.
17. **`tools.rs:240-253, 589-594`** — The process-group KILL is issued *after* `child.wait()` reaps the direct child, using the cached pid as pgid up to ~2 s later; on pid recycle the signal can hit an unrelated group. Narrow race, unguarded `kill`.
18. DONE **`main.rs:354-364`** — All `PrintSink` writes did `writeln!(...).ok()`, so a closed stdout (`clanky -p … | head -1`) didn't stop the turn: it kept calling the model and tools with output going nowhere and exited 0. Deliberate for the session recorder, but wrongly applied to the response channel too. Fixed: the sink records the first failure and `run_pipe` kills the plugin through the ctrl+c `CancelHandle`, so the in-flight request fails and no further round starts. The failure is read from the sink after the turn — including the closing writes and the final stdout `flush()` — the session file records that as the turn's terminal reason, and the process exits non-zero with `Error::PrintWriteFailed`, naming the dead stream. Response (stdout) failures are fatal; activity (stderr) failures are not, so `2>&-` still delivers the answer. Test: `a_closed_stdout_aborts_the_turn_with_a_nonzero_exit` (streams past the 64 KB pipe buffer, reads one line, drops the pipe, and checks the exit code, the message, and the recorded reason).
19. DONE **`main.rs:133-135` + `recorder.rs:74-83`** — An invalid `--name` was reported to stderr but the session proceeded under an auto-generated name; the doc comment claimed the recorder was dropped, and it wasn't at either call site. Touches the user-values invariant: the explicitly requested name was silently replaced. Fixed: `main::run` validates `--name` before either front end starts and aborts with the validation error; `Recorder::with_name` now returns `Result` instead of reporting-and-ignoring (so the substitution cannot come back), and the TUI reports a failure instead of falling back to a generated name. Test: `an_invalid_name_aborts_before_any_session_file_is_created`.
20. **`provider.rs` (deepinfra 533 / litellm 701)** — When a tool-args fragment arrives on the first delta *before* id/name (the code's own comment says some backends lead with index-only deltas), the early `return` skips argument ingestion and the fragment is lost, producing truncated/garbage assembled args. Buffer args instead of dropping them.
21. **`provider.rs` (deepinfra 591)** — Legacy `finish_reason: "function_call"` maps to `Stop`, discarding announced tool calls. LiteLLM's copy of the same OpenAI-compat mapping handles it (with a regression test); DeepInfra's doesn't. Same wire format, divergent behavior.
22. **`provider.rs` (deepinfra 517 / litellm 687)** — `while self.calls.len() <= call.index` with a bare `u32` index: one malformed `"index": 4294967295` chunk allocates ~4 B accumulators (OOM). Clamp or reject absurd indices as a protocol error.
23. **`litellm/provider.rs:559`** — Context-window hint falls back to `max_tokens`, which in LiteLLM's `model_info` is the *output*-token limit — badly understates the window for models that only populate it.
24. **`anthropic/provider.rs:280`** — `body_text.contains("invalid")` maps any 400/401 OAuth error containing the substring (e.g. "invalid_client", but also unrelated text) to "run `login`"; a genuinely non-recoverable error gets the wrong actionable message.
25. **`anthropic/backend.rs:117`** — HTTP-date `Retry-After` is unparsed (only integer seconds); the DeepInfra/LiteLLM backends handle the date form. Inconsistent retry-hint coverage.
26. **`anthropic/adapter.rs:39`** — The Claude-Code identity system block is injected on the API-key path too, contradicting the documented design (identity block is the OAuth path's substitute for not having real Clanky identity).
27. **`subagents/main.rs:174-183`** — The instruction is pushed as a trailing positional arg; an instruction with a leading `-` is flag-parsed by clap and the job fails with a clap error. Push `--` before it.
28. **`subagents/main.rs:236-250`** — If the process-group kill fails, pipe-holding grandchildren block the *un-timeouted* `join()`s, so a job can run past `timeout_secs`.
29. **`subagents/main.rs:154-165`** — `killpg` shells out to the external `kill -PID`; failure is silently discarded (`let _`).
30. **`process.rs:112-118`** — The cancel deadline is armed *before* the cancel write; if the write fails, a stale deadline survives and can kill a later innocent request via the `CancelHandle` path (the `send` path clears it, but a cancel-while-idle doesn't).
31. **`serve.rs:225-230`** — A cancel for an already-finished request flips the shared flag with no matching restore; masked only by the next chat's `reset()`. Latent inconsistency; worth a test.
32. DONE **`editor.rs:32-36`** — Temp file name was `pid + subsec_nanos` with no `O_EXCL`; a collision silently overwrote the buffer being edited. Fixed: `write_buffer_file` creates with `create_new` (`O_EXCL`) at `0600` and moves to the next candidate name on a collision (a stale file or a recycled pid is benign), removing a half-seeded file if the initial write fails. Test: `the_buffer_file_is_created_exclusively`.
33. DONE **`screen.rs:976`** — `input_window` could return a row one column wider than `view_width` with wide (CJK) chars, parking the caret one cell off briefly. Fixed: the window budgets each character's whole display width, starts on a character boundary (the actual off-by-one — the ideal start could land mid-character, so the row began with a truncated character and the caret column was computed from it), and clamps the caret to the painted row. Test: `input_window_never_exceeds_its_width_with_wide_characters`.
34. DONE **`screen.rs:259-262`** — The reported `min(height-1)` is a no-op (`saturating_sub` already yields 0) and is gone, with the `height == 0` underflow; the real defect was a tail *taller than the screen*: `keep`'s `.max(1)` let a footer that fills the screen (popup + multi-line input on a short terminal) keep one entry row on top of it, so `tail_height` was `height + 1` and every idle redraw's clamped `MoveUp(tail_height - 1)` started one row high and scrolled by one row — the persistent off-by-one. Test: `a_tail_taller_than_the_screen_does_not_crawl`.
35. TABLED **`app.rs:602-613`** — `screen_records` (`/md` export path) attributing tool calls to the wrong turn when prompts are queued mid-turn. A queued prompt's placeholder is UI-only and its real `User` entry is appended at dispatch, after the previous turn's entries, so a call lands in its own round; left as-is pending a concrete repro.
36. DONE **`markdown.rs:431-441`** — The table `sum_min >= avail` branch floored per column and gave every column at least one, so with narrow columns the total landed above `avail`; the overshoot is now handed back one column at a time, widest first, never below one. The `cap_lines` half needed no change (content is cut to `width - note_width`, so its rows are at most `width`; a 40-width × 5-sample sweep found no overflow). Test: `table_over_floor_scaling_still_fits_the_viewport`.
37. **`process.rs:364-372`** — Detached stderr thread loses the final partial log line and outlives the transport; matters only for crash forensics.
38. DONE **`session.rs:153-158` / `provider.rs:162-166`** — Security-adjacent: tool output (e.g. a model running `env`) and plugin stderr landed unredacted in world-readable (0o644) files. Fixed: new session files, plugin logs, and editor buffers are created owner-only (`0600`) via `OpenOptions::mode` + `create_new`/`create` — at creation, not by a later `chmod`, so there is no readable window. An existing file's mode is left alone. Tests: `a_fresh_session_file_is_owner_only`, `a_reopened_session_file_keeps_its_own_mode`, `a_new_plugin_log_is_owner_only`, `an_existing_plugin_log_keeps_its_mode`, `the_buffer_file_is_owner_only`.
39. **`examples/hello-world-provider`** — Uncaught `KeyError` on a known message missing `id` kills the plugin, contradicting the file's own "never die on junk" policy. One `.get("id")` fixes it.

## Verified non-issues (checked and cleared by the reviewers)

- Retry logic across core and all three plugins: no duplicate output, no retrying non-retryable errors, `retry_after_ms` correctly restricted to rate limits, streaming correctly disables retries.
- Secrets in the OpenAI-compat paths stay in request headers only; error bodies never echo keys.
- SSE framing (CRLF, multi-line `data:`, comments, EOF flush) correct in all three backends.
- All TUI cursor/backspace/completion helpers are char-boundary-safe; picker scroll math clamped; no unbounded transcript growth.
- `truncate_call_arguments`, `validate_name`, rename: no traversal/injection vectors.
- No user-triggerable panics/unwraps in core turn/session/tools outside the markdown table bug (#3).

## Suggested fix order

1. **#3** (markdown table panic) — remotely triggerable, two-line fix.
2. **#1** (webp serde) — spec-breaking both directions, one-line fix.
3. **#2** (Anthropic `stream:false`) — invariant violation, silent data loss.
4. **#4, #5** (token leak, PKCE) — security correctness in the OAuth path.
5. **#9, #12, #13** — the hangs/false-failure class that bites everyday use.

Want me to start fixing any of these? I'd suggest beginning with #3 and #1 since both are tiny, testable changes.


---

_session total: 1836844 prompt tokens · 11491 completion tokens_

---

_Last turn ended with stop reason `stop`._
