# Part 1: Evaluation against the project invariants

## Invariant 1 — "User-provided values are authoritative"

### ✅ What's done right

- **Layering is clean** — `crates/clanky/src/settings.rs`: per-field overlay, unset fields never clobber; CLI > project > user precedence (sanctioned by the invariant itself). `max_tool_rounds = 0` / `max_retries = 0` pass through as distinct from "unset" (`unwrap_or(DEFAULT…)` only fires on `None`).
- **Anthropic honors `max_tokens` as-is** — `crates/clanky-provider-anthropic/src/adapter.rs:90`: explicitly commented *"Use the user-provided max_tokens as-is; fall back to the default if none was supplied."* The 64 000 default applies only when unset. Exactly the right pattern.
- **Sampling passes through verbatim** — DeepInfra/LiteLLM/Anthropic merge the open `sampling` map raw into the request body; no clamping to [0,1], no renaming, no local value checks.
- **Provider errors pass through intact** — status → `ErrorCode` mapping only; the server's message reaches the user unchanged (e.g. the LiteLLM tests assert `400: /chat/completions: Invalid model name passed in model=…` surfaces verbatim).
- **Bash tool errors are loud, not silent** — `timeout_seconds` that is not a positive integer is rejected with a message, not clamped (`crates/clanky/src/tools.rs:121`).

### ⚠️ Violations (ordered by severity)

**1. ✅ FIXED — Anthropic adapter silently drops `temperature` when thinking is on** — `crates/clanky-provider-anthropic/src/adapter.rs:114`, locked in by the test `thinking_sets_budget_and_drops_temperature`. The comment even says *"Drop the parameter entirely instead of clamping."* This is a literal instance of "never silently drop a value the user supplied." Per invariant 2, the server's 400 ("temperature may only be 1 when thinking is on") *is* the actionable error you promised to surface. Sub-bug: with `--thinking 0` (budget 0 → no thinking block sent) the `else if` still skips `temperature`, so it's dropped even though thinking is off on the wire. **Recommend: pass `temperature` through and let the server reject it (or at minimum print a warning).** *Resolution: `temperature` now passes through verbatim in every case (thinking on, thinking off, `--thinking 0`); the provider's 400 surfaces unchanged. The test became `thinking_sets_budget_and_passes_temperature_through`, plus `thinking_zero_keeps_temperature` for the sub-bug. Decision recorded in `anthropic-provider-planning.md` ("As built: `temperature` passes through with thinking on").*

**2. ✅ FIXED — Documented `thinking` values are rejected** — README and `settings.rs`/`cli.rs` all advertise *"Thinking budget or level (`1024`, `low`, `off`)"*, but `turn::parse_thinking` (`crates/clanky/src/turn.rs:459`) accepts only `""`/`off`/u32. `--thinking low` → `InvalidThinking`. The adjacent comment ("Named levels map onto budgets in the provider adapter") is aspirational — core rejects the level before any adapter can see it. Either implement pass-through of named levels (invariant 2: the provider owns that vocabulary) or fix the three doc sites. *Resolution: docs route — README's `settings.toml` table, `settings.rs`, and `cli.rs` now document the accepted values (a token budget or `off`), and the aspirational comment on `thinking_from` was corrected to say named levels are rejected (adapters map budgets onto their own vocabulary). Pass-through of named levels would need a protocol change — `Thinking` carries only `budgetTokens` — and remains a possible future enhancement.*

**3. ✅ FIXED — Sampling parameter whitelist** — `turn::sampling_from` hard-rejected anything but `temperature`, `top_p`, `max_tokens`. It was loud (an error, not a silent drop), but it was exactly "re-implementing validation locally": users could not set `repetition_penalty`, `seed`, `reasoning_effort`, etc., even when the provider supported them. The root cause was the closed `Sampling` struct in `clanky-protocol` — true pass-through needed an open map, i.e. a protocol question. *Resolution: sampling is now a true pass-through. `Sampling` in `clanky-protocol` is an open map keyed by backend parameter names (protocol **v2** — the closed camelCase struct renamed and dropped keys, which is breaking per the spec's own §9 policy, so the handshake rejects v1 plugins loudly instead of letting them silently drop parameters). `sampling_from` accepts every key and never rejects a value: untyped strings decode as JSON when they parse as JSON (`"0.7"` → `0.7`, `"high"` → `"high"`), any TOML value passes through as-is, and the provider backend validates. Provider plugins merge `sampling` into the backend request verbatim; only the request's structural fields (`model`, `messages`, `tools`, `stream`, …) are provider-owned, and derived knobs (Anthropic `max_tokens` default, `thinking` → `reasoning_effort`) fill only keys the user did not set. Locked in by pass-through tests in all three providers; recorded in `provider-protocol.md` §5/§9 and README.*

**4. ✅ FIXED - LiteLLM silently filters `tools` and `reasoning_effort`** — `crates/clanky-provider-litellm/src/provider.rs` (`build_request_body`): when `/model/info` hints say the model lacks support, the user's tools/thinking are silently omitted from the request "so a heterogeneous proxy does not 400." That pre-empts the server's own validation (invariant 2) and silently ignores user intent (invariant 1). Note this is a *documented* "As built" decision in milestones.md (M10) — a deliberate tension with the invariant you may want to revisit (options: pass through and surface the 400, or warn loudly when filtering). *Partially mitigated by #3: the filter now applies only to the derived `thinking` → `reasoning_effort` mapping and the protocol tool list — a `reasoning_effort`/`tools` the user sets in `[sampling]` passes through unfiltered.*

### ⚪ Gray areas / minor (noted, arguably fine)

- **Thinking budget → `reasoning_effort` quantization** (DeepInfra: ≤2048→`low`, ≤8192→`medium`, else `high`; LiteLLM: ≤512 `minimal`, ≤4096 `low`, ≤16384 `medium`, ≤32768 `high`, ≤65536 `xhigh`, else `max`, 0→`none`). Unavoidable API translation, documented in README; but budget 2049 and 8192 both become `medium` — the exact user number is not honored, and the rung boundaries are arbitrary.
- **✅ FIXED - `[sampling]` map merged per key across scopes/CLI** — `Settings::overlay` used to treat the whole map as one field: user scope `temperature = "0.7"` + project scope `top_p = "0.9"` → `temperature` silently dropped (same for `--sampling`). *Resolution: per-key merge — later layers win key-by-key, keys set only in a lower layer are kept, so every supplied value reaches the provider. Locked in by `sampling_merges_per_key_across_scopes` and `sampling_merges_per_key_from_cli`; documented in README (precedence) and milestones.md (M2).*
- **Prompt text is trimmed** (`prompt.rs:36`); whitespace-only prompts become "no prompt".
- **Context files are trimmed and re-wrapped** with a `# Context — <path>` heading (`context.rs` `push_part`) — user text altered (mildly) and annotated.
- **Session names validated** (`session.rs:240`: ≤64 chars, `[A-Za-z0-9._-]`, no leading `.`) — rejects user input, but it's path safety for clanky's own filesystem, not server validation, and it's loud. Acceptable.
- **URL/env normalization** — `LITELLM_BASE_URL` trailing-`/` and `/v1`-suffix trimming (behavior-preserving URL join), key/model trimming. Fine.
- **`u32` typing** of `budget_tokens` — values > 4 294 967 295 fail local parse. Theoretical. (`max_tokens` is no longer typed in core: it passes through like any other sampling value.)

## Invariant 2 — "Don't pre-validate what the server will validate"

**Mostly good.** No model-name checking (the server's "Invalid model name" surfaces unchanged), no parameter range checks, no speculative validation added anywhere. The exceptions: the **LiteLLM capability filter** (#4) and the **thinking-level parse** (#2). The **sampling whitelist** (#3) is fixed — sampling is now an open, verbatim pass-through, so the backend's own message is the one that reaches the user. Everything else errs correctly on the "expect the server's message to be useful" side.

---

# Part 2: Defaults and limits inventory

## User-facing defaults (settings/CLI)

| What | Default | Where | Configurable? |
|---|---|---|---|
| Provider | `deepinfra` | `clanky/src/provider.rs:28` | `provider` / `--provider` |
| DeepInfra model | `zai-org/GLM-5.3-Flash` | `clanky-provider-deepinfra/src/provider.rs:15` | `model` / `--model` |
| LiteLLM model | `claude-haiku-4-5` | `clanky-provider-litellm/src/provider.rs:49` | yes |
| Anthropic model | **none** (same) | `clanky-provider-anthropic` | yes |
| Max tool rounds per turn | `0` = **unlimited** (deliberate: user judges runaway turns) | `clanky/src/turn.rs:28` | `max_tool_rounds` / `--max-tool-rounds` |
| Max retries | `5` (`0` = never) | `clanky/src/turn.rs:36` | `max_retries` / `--max-retries` |
| Sampling params | none set (server defaults) | `settings.rs` | `sampling` / `--sampling` (any backend parameter, passed through verbatim) |
| Thinking | off | `turn.rs:459` | `thinking` / `--thinking` (`off` or integer budget only) |

## Retry / timing

| What | Value | Where |
|---|---|---|
| Retry backoff base | 500 ms, doubling per attempt | `turn.rs:40` |
| Retry backoff cap | 30 s (also caps `Retry-After` hints) | `turn.rs:43` |
| Backoff shift cap | `2^16` multiplier ceiling | `turn.rs:357` |
| HTTP connect timeout | 15 s (all 3 provider backends) | `*/backend.rs` |
| HTTP request timeout | none — unbounded (all 3 provider backends) | `*/backend.rs` |
| OAuth HTTP timeouts | connect 15 s, total 60 s | `anthropic/src/oauth.rs:183` |
| OAuth token expiry skew | 5 min (refresh early) | `anthropic/src/auth.rs:35` |
| Plugin cancel timeout | 2 s before kill fallback | `clanky-protocol/src/process.rs:42` |
| Plugin shutdown timeout | 2 s | `process.rs:46` |

## Bash tool (all hardcoded — **not** in settings.toml today)

| What | Value | Where |
|---|---|---|
| Default command timeout | none — unbounded | `tools.rs:68` |
| Per-call `timeout_seconds` range | any positive integer (1 … u64::MAX); 0 or non-integer → error | `tools.rs:130`, schema `minimum: 1`, no `maximum` (`tools.rs:105`) |
| Tool output cap fed to model | 20 000 chars, then `… (output truncated)` | `tools.rs:69` |
| Kill-check granularity | 100 ms poll | `tools.rs:172` |

## Provider plugin constants

| What | Value | Where |
|---|---|---|
| DeepInfra base URL | `https://api.deepinfra.com/v1` (`DEEPINFRA_URL`) | `deepinfra/provider.rs:21` |
| DeepInfra keys | `DEEPINFRA_API_KEY`, fallback `DEEPINFRA_TOKEN` | `deepinfra/provider.rs:23-25` |
| LiteLLM base URL | `http://localhost:4000` (`LITELLM_BASE_URL`) | `litellm/provider.rs:40` |
| Anthropic base URL | `https://api.anthropic.com` (`ANTHROPIC_BASE_URL`) | `anthropic/provider.rs:24` |
| Anthropic default `max_tokens` | 64 000 (only when user unset) | `anthropic/adapter.rs:21` |
| Anthropic API version / beta / UA | `2023-06-01` / `claude-code-20250219,oauth-2025-04-20` / `claude-cli/2.1.280` | `anthropic/provider.rs:26-30` |
| Anthropic pricing table ($/Mtok) | opus 5/25, sonnet 2/10, "fable" 10/50 | `anthropic/provider.rs:36-39` |
| Thinking→effort ladder (DeepInfra) | 0→`none`, ≤2048→`low`, ≤8192→`medium`, else `high` | `deepinfra/provider.rs:186` |
| Thinking→effort ladder (LiteLLM) | 0→`none`, ≤512 `minimal`, ≤4096 `low`, ≤16384 `medium`, ≤32768 `high`, ≤65536 `xhigh`, else `max` | `litellm/provider.rs:310` |
| Error message truncation | 400 chars (`truncate_body`); OAuth bodies 200 chars | `*/provider.rs`, `anthropic/oauth.rs:261` |

## Sessions / export / display

| What | Value | Where |
|---|---|---|
| Session name rules | ≤64 chars, `[A-Za-z0-9._-]`, no leading `.` | `session.rs:240` |
| Default session name | `YYYYMMDD-HHMMSS-xxxx` (20 chars) | `session.rs:259` |
| Export default | user/assistant text only; `--all` adds thinking + tools | `export.rs:22` |
| Tool-arg preview (TUI/pipe) | 600 chars (`CALL_PREVIEW_CHARS`) | `tools.rs:268` |
| Tool-result preview (pipe mode) | 4 000 chars | `main.rs:145` |
| TUI token estimate | 32 chars ≈ 1 token | `tui/app.rs:37` |
| TUI picker/completion rows | max 10 / 8 visible | `tui/screen.rs:539,550` |
| TUI event poll | 50 ms | `tui/mod.rs:818` |
| TUI thinking presets | off, 1024, 4096, 16384, 32768 | `tui/mod.rs:86` |
| Cost display | 4 decimals below $10, 2 above | `tui/screen.rs:453` |
| TUI queue preview | 1 line, ≤49 chars | `tui/mod.rs:1660` |

## Candidates you may want to adjust

1. **Bash output cap (20 000 chars) is hardcoded** — the command timeout is gone (unbounded default; a voluntary `timeout_seconds` is the model's own deadline), but the output cap still has no settings knob. Consider `bash_max_output` in settings.toml (the `BashTool::new()` constructor already exists).
2. **`DEFAULT_MAX_TOOL_ROUNDS = 0` (unlimited)** — deliberate and documented, but a footgun for unattended `-p` use; maybe a non-zero default for pipe mode only.
3. **`DEFAULT_MAX_RETRIES = 5`** with 30 s cap — worst case ~2 min of backoff per round; fine interactively, maybe lower for `-p`.
4. **Anthropic `DEFAULT_MAX_TOKENS = 64 000`** — generous; billed per output token if the model runs away.
5. **Error truncation at 400 chars** — provider errors can lose detail (e.g. multi-line LiteLLM messages); 800–1000 may be safer.
6. **✅ REMOVED — HTTP request timeout 600 s** — dropped from all three provider backends: requests and streams are now unbounded (connect stays at 15 s), so a long answer is never killed mid-stream. Trade-off: a stuck upstream now holds a turn indefinitely instead of 10 min.

If you'd like, I can turn any of the invariant findings (1–4) into concrete patches — #1 (temperature pass-through + `--thinking 0` edge) and #2 (docs vs. named levels) are small; #3/#4 are design decisions worth recording in `milestones.md` either way.
