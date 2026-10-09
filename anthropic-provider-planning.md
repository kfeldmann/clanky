# Planning — Anthropic provider plugin (M11)

> **Status: flow confirmed, design in progress.** M11 is broken into a/b/c
> in `milestones.md`; this file records design decisions for the OAuth
> flow and the spike checklist. Update with as-built notes as work lands,
> the way M8/M10 did.

A provider plugin for the Anthropic Messages API, authed by OAuth 2.0
exclusively (our enterprise tenant issues no API keys). Crate:
`crates/clanky-provider-anthropic`, binary `clanky-provider-anthropic`
(discovered on `$PATH` like every plugin).

## The go/no-go question is mostly answered

The third-party OAuth client is not a published Anthropic API surface
(see the spike note below), which made M11a a real go/no-go gate. That
risk is now largely retired: **we already use [Pi](https://github.com/earendil-works/pi)
(MIT, TypeScript) against our enterprise tenant**, and it performs this
exact flow — same client_id, token endpoint, and bearer request shape —
on this account. Pi prints a per-token-pricing warning at session start,
matching how our account is billed (known and expected).

What remains for the M11a spike is therefore **confirm-and-capture, not
discovery**: run the flow ourselves, record fixtures, and verify the
tenant-specific details Pi cannot tell us (org/workspace scoping
enforcement, exact token lifetimes). The reference implementation lives
in Pi's `packages/ai/src/auth/oauth/anthropic.ts` (login/refresh) and
`packages/ai/src/api/anthropic-messages.ts` (bearer request shaping,
`isOAuthToken` branches). Cross-checks if ever needed:
`gotgenes/pi-anthropic-auth` and the `pi-claude-oauth-adapter` package
on pi.dev implement the same dance independently.

## Confirmed flow parameters (from Pi, re-verify in the spike)

- `client_id`: `9d1c250a-e61b-44d9-88ed-5944d1962f5e` (Anthropic's
  Claude Code client; not a published third-party client_id)
- Authorize endpoint: `https://claude.ai/oauth/authorize`
- Token endpoint: `https://platform.claude.com/v1/oauth/token`
- Scopes: `org:create_api_key user:profile user:inference
  user:sessions:claude_code user:mcp_servers user:file_upload`
- OAuth access tokens carry the `sk-ant-oat` prefix — Pi keys its whole
  bearer path off that prefix
- Beta headers required on the API surface: `claude-code-20250219`,
  `oauth-2025-04-20`
- The headless ("copy code") flow sends `redirect_uri=
  https://platform.claude.com/oauth/code/callback` **and a non-standard
  `code=true` authorize param**; the code-display-instead-of-redirect
  behavior is gated on these — don't drop them
- Pi uses the PKCE **verifier itself as `state`** — convenient for a
  paste flow; copy or consciously reject
- Token response gives `expires_in`; Pi stores `now + expires_in − 5min`
  as the effective expiry (adopt this skew for `expires_at`)

## Shape of the work

The M8 architecture makes this mostly additive. Free: `clanky-protocol`'s
`serve_stdio` / `Handler` / chunk assembly / cancellation / crash handling;
core never sees auth. Estimated size ~1.6k–2.2k lines plus fixtures/tests —
comparable to the LiteLLM plugin overall. The only genuinely novel part is
the OAuth client.

## The structural constraint: no interactive prompting mid-session

Clanky spawns the plugin with piped stdio and speaks the protocol on it
(provider-protocol.md §1). The plugin can never print a URL "during a
session" and read a pasted code, because its stdout is protocol JSONL.

Therefore the no-browser flow is an **offline subcommand on the plugin
binary**, not session behavior:

    clanky-provider-anthropic login      # print URL, read pasted code, store token
    clanky-provider-anthropic            # default: serve the protocol

This mirrors `ant auth login --no-browser` ("print the authorize URL and
paste the returned code back into the terminal") and Pi's "copy code
login (headless)" variant. It requires no protocol change: §1 already
assigns auth material to the plugin ("Clanky never handles third-party
provider keys").

Credentials live under `~/.clanky/providers/anthropic/` (fields:
`access_token`, `refresh_token`, `expires_at`). The serving process loads
them per session and refreshes automatically before expiry.

## The no-browser OAuth flow (M11a)

PKCE, authorization-code flow with the browser step replaced by
copy/paste (constants above):

1. Generate `code_verifier` + S256 `code_challenge`; use the verifier as
   `state` (Pi's approach).
2. Print the authorize URL — params: `code=true`, `response_type=code`,
   `redirect_uri=https://platform.claude.com/oauth/code/callback`,
   `scope=<scopes above>`, `code_challenge`, `code_challenge_method=S256`,
   `state`, `client_id` — for the user to open in any browser (their
   laptop, phone, jump host — headless is the point).
3. User signs in on the enterprise tenant; because of `code=true` +
   that redirect_uri, the auth page shows a code instead of redirecting
   (no local callback server exists).
4. User pastes the code (accept a bare code, a `code#state` pair, or the
   full redirect URL — Pi's `parseAuthorizationInput` handles all three);
   validate `state`.
5. Exchange code (+ verifier) at the token endpoint
   (`grant_type=authorization_code`, `client_id`, `code`, `state`,
   `redirect_uri`, `code_verifier`); store access/refresh/expiry (with
   the 5-minute skew).

## Bearer-path request shaping (M11a)

Two auth modes on the API side (M11a bearer, M11c adds `x-api-key`).
OAuth requests differ from API-key requests in **more than headers**:

- Headers: `Authorization: Bearer <token>` (never `x-api-key`),
  `anthropic-beta: claude-code-20250219,oauth-2025-04-20`,
  `user-agent: claude-cli/<version>` (Pi pins `2.1.280`; keep fresh),
  `x-app: cli`.
- Body: a first `system` block reading "You are Claude Code, Anthropic's
  official CLI for Claude." is **required** on the OAuth surface (Pi
  always injects it, then appends the real system prompt as a second
  block).
- Tool names: Pi case-normalizes tool names to Claude Code's canonical
  set (`Bash`, `Read`, `Write`, `Edit`, `Grep`, `Glob`, …) in both
  directions under OAuth — the subscription backend presumably expects
  them. Our `bash` tool may need the same mapping; verify in the spike.

Refresh happens automatically before expiry (`grant_type=refresh_token`
with `client_id`); refresh failure surfaces as a clear `auth` error
("login expired — run `clanky-provider-anthropic login`"), never a raw
401.

## Spike checklist (M11a — confirm-and-capture, before workspace code)

One live capture on our tenant, recorded as fixtures. The constants and
shaping above come from Pi, which demonstrably works on this account —
the spike confirms they hold for us and records what Pi cannot:

- [ ] Authorize URL builds and the tenant's login page accepts it; watch
      for `forceLoginOrgUUID` / workspace-scoping enforcement — if the
      tenant forces org selection, that changes the hardcoded URL params
      (Pi cannot tell us this for our tenant)
- [ ] Pasted code exchanges for access + refresh tokens; record token
      lifetime and refresh-token lifetime (finite for subscription
      logins; refresh will be the most common failure mode)
- [ ] Non-streaming `/v1/messages` with the bearer + beta headers +
      identity system block returns 200 with a completion; record the
      exact beta header string and endpoint used
- [ ] Verify our tool names (`bash`) pass through on the bearer path, or
      that the Claude Code case-normalized names are needed
- [ ] Capture the full request/response pair as the seed fixtures for
      the M11a/M11b test suites
- [ ] (If any step fails) document the failure mode; fallback directions:
      reuse an existing `ant` CLI profile's stored credentials from
      `~/.config/anthropic`, or get the supported client path confirmed
      with Anthropic

## Adapter mapping notes (M11a/M11b)

Anthropic's native format is closer to clanky's protocol than OpenAI's:
`system` is a top-level param (not a message), `tool_use` /
`tool_result` content blocks map to `toolCalls` / `role:"tool"`,
`content_block_delta` thinking deltas map to `kind:"thinking"`. M11a
implements the request/response mapping non-streaming (including basic
tool_use/tool_result, so M11b is delta-mapping only); M11b adds SSE
chunking: `content_block_start`/`content_block_delta`/`input_json_delta`
→ `chunk` payloads, `message_delta` → finish reason, `message_start` /
`message_delta` usage → `usage`. Pi's
`packages/ai/src/api/anthropic-messages.ts` is the reference for the
event mapping too.

Fixtures: pinned live captures in `tests/fixtures/` (the LiteLLM M10
pattern), seeded by the spike.
