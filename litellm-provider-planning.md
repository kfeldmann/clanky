# Clanky session: litellm-provider-planning

| | |
|---|---|
| Provider | deepinfra |
| Model | deepseek-ai/DeepSeek-V4.1-Flash |
| Created | 2026-10-08 21:39 |
| Contents | user and assistant messages |

## User

I'd like to create a provider plugin for LiteLLM. I have a LiteLLM provider extension that I made for Pi in case that's useful as a reference: ./reference/litellm.ts. Let me know what other documentation would be useful to ensure the provider plugin is correct.

## Assistant

I'll start by exploring the project to understand the context.

## Assistant

I've read `provider-protocol.md`, the M8 plan, the existing DeepInfra plugin (`crates/clanky-provider-deepinfra`), the Python hello-world example, and `./reference/litellm.ts`. I also pulled LiteLLM's live docs (`llms.txt` / `llms-full.txt`) to check what's actually available.

Short version: a LiteLLM plugin is very feasible and mostly *less* work than DeepInfra — LiteLLM's `/v1` surface is OpenAI-compatible, so the SSE framing, delta accumulation, and tool-call assembly in the DeepInfra plugin are reusable almost verbatim. The parts I can't verify without your proxy are the **metadata shapes** (`/model/info`) and the **exact delta/error shapes** your deployment emits.

## 1. The thing docs can't give me: captures from your actual proxy

This is the highest-value input. A plugin that is correct against LiteLLM's docs can still be wrong against *your* LiteLLM (version, `drop_params`, `always_include_stream_usage`, router/fallback config all change observable behavior). If you can run these and drop the outputs in `./reference/litellm/`, I can pin the adapter exactly:

```sh
export LITELLM_BASE_URL=http://localhost:4000        # your proxy
export LITELLM_API_KEY=sk-...                        # the key clanky will use
export MODEL=$(curl -sS "$LITELLM_BASE_URL/v1/models" -H "Authorization: Bearer $LITELLM_API_KEY" | jq -r '.data[0].id')

# catalog + metadata
curl -sS "$LITELLM_BASE_URL/v1/models"     -H "Authorization: Bearer $LITELLM_API_KEY" | jq . > models.json
curl -sS "$LITELLM_BASE_URL/v1/model/info" -H "Authorization: Bearer $LITELLM_API_KEY" | jq . > model-info.json

# streaming shapes: plain / reasoning / tool call
chat() { curl -sS "$LITELLM_BASE_URL/v1/chat/completions" -H "Authorization: Bearer $LITELLM_API_KEY" \
  -H 'Content-Type: application/json' -d "$1"; }
chat '{"model":"'"$MODEL"'","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":"say hi"}]}' > stream-plain.txt
chat '{"model":"'"$MODEL"'","stream":true,"stream_options":{"include_usage":true},"reasoning_effort":"low","messages":[{"role":"user","content":"2+2? think first"}]}' > stream-reasoning.txt
chat '{"model":"'"$MODEL"'","stream":true,"tools":[{"type":"function","function":{"name":"bash","description":"Run a shell command","parameters":{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}}],"messages":[{"role":"user","content":"list /tmp using the bash tool"}]}' > stream-tools.txt

# error envelope + headers (bad key, unknown model, and a 429 if you can trigger one)
curl -sS -i "$LITELLM_BASE_URL/v1/chat/completions" -H "Authorization: Bearer bad-key" -H 'Content-Type: application/json' -d '{"model":"'"$MODEL"'","messages":[{"role":"user","content":"hi"}]}' > err-401.txt
curl -sS -i "$LITELLM_BASE_URL/v1/chat/completions" -H "Authorization: Bearer $LITELLM_API_KEY" -H 'Content-Type: application/json' -d '{"model":"no-such-model","messages":[{"role":"user","content":"hi"}]}' > err-404.txt
```

Also useful, if cheap to share:

- your `config.yaml` with secrets redacted (`model_list`, `litellm_settings`, `router_settings`) — I need to know if `drop_params`, `always_include_stream_usage`, fallbacks, or router cooldowns are in play;
- `litellm --version` (or `GET /health/liveliness`) — behavior differs across releases;
- whether clanky will use a **master key** or a **virtual key**, and whether that key is allowed to call `/v1/model/info` (it's model-access filtered, and not all keys are permitted).

## 2. LiteLLM documentation worth pulling in

Everything below is fetchable as markdown by appending `.md` (and `https://docs.litellm.ai/llms-full.txt` is the whole corpus, 8.4 MB — I already have it cached, so you don't need to send these):

| Page | Why it matters for the plugin |
| --- | --- |
| [`/docs/proxy/model_management`](https://docs.litellm.ai/docs/proxy/model_management.md) | `/model/info` semantics: per-deployment rows, `model_info` passthrough, key masking |
| [`/docs/proxy/model_access`](https://docs.litellm.ai/docs/proxy/model_access.md) | Model lists are **key-scoped**; `discoverable: false` and `models` allowlists filter `/v1/models` *and* `/model/info` |
| [`/docs/proxy/model_discovery`](https://docs.litellm.ai/docs/proxy/model_discovery.md) | OpenAI-shaped `/v1/models` response, wildcard/`check_provider_endpoint` behavior |
| [`/docs/completion/output`](https://docs.litellm.ai/docs/completion/output.md) | finish_reason mapping + `provider_specific_fields.native_finish_reason`; streaming usage rules |
| [`/docs/completion/stream`](https://docs.litellm.ai/docs/completion/stream.md) | `stream_options.include_usage` → the extra usage chunk before `[DONE]` |
| [`/docs/reasoning_content`](https://docs.litellm.ai/docs/reasoning_content.md) | `reasoning_effort` rungs (`none/minimal/low/medium/high/xhigh/max/default`), Anthropic-style `thinking`, `reasoning_content` deltas, `merge_reasoning_content_in_choices` |
| [`/docs/completion/drop_params`](https://docs.litellm.ai/docs/completion/drop_params.md) | The mechanism to avoid 400s when a model rejects `reasoning_effort`/`tools` |
| [`/docs/proxy/error_diagnosis`](https://docs.litellm.ai/docs/proxy/error_diagnosis.md) + the "Error Reference" page | OpenAI-style `{"error":{message,type,param,code}}` envelope, gateway-vs-provider 429 disambiguation via `retry-after` |
| [`/docs/proxy/request_headers`](https://docs.litellm.ai/docs/proxy/request_headers.md) / [`response_headers`](https://docs.litellm.ai/docs/proxy/response_headers.md) | `x-litellm-timeout`, `x-litellm-stream-timeout`, `x-litellm-num-retries`, rate-limit headers |
| [`/docs/proxy/health`](https://docs.litellm.ai/docs/proxy/health.md) | Startup/validation story (also handy for a plugin "test connection" path) |
| ["Add Model Pricing & Context Window"](https://docs.litellm.ai/docs/proxy/model_management.md) (model cost map spec) | The exact `model_info` keys I'd read: `max_input_tokens`, `max_output_tokens`, `max_tokens`, `mode`, `input_cost_per_token`, `output_cost_per_token`, `cache_read_input_token_cost`, `supports_reasoning` |
| DeepInfra's chat-completions reference (already in your `AGENTS.md`) | Still the canonical OpenAI-shape reference for the adapter |

The `litellm.ts` reference covers the metadata mapping well; the two things it has that clanky's protocol doesn't are `cacheWrite` pricing and `input: ["text"]` — clanky's `ModelInfo` has `cacheReadPricePerMtok` only, so cache-write cost is simply not representable in v1. Not a blocker, just noting it.

## 3. Design decisions I need from you

1. **Rust crate vs. Python plugin.** My recommendation: a Rust crate `crates/clanky-provider-litellm` mirroring `clanky-provider-deepinfra`. `clanky_protocol::serve` already implements every wire rule (handshake, cancel out-of-band, one-chat-in-flight, stderr isolation, EOF exit), so the binary is ~30 lines of glue plus an adapter — and the DeepInfra SSE/tool-call code is directly reusable. Python would work but would re-implement framing that's already solved and tested here.
2. **Provider name / default model.** `name: "litellm"`; I'd advertise **no** `defaultModel` (the proxy's catalog is arbitrary), unless you'd rather have the plugin read an optional `LITELLM_MODEL` env var. Note core still defaults to `deepinfra`, so selecting litellm means `--provider litellm` or `provider = "litellm"` in settings.
3. **Thinking mapping.** clanky's `thinking.budgetTokens` is a token budget; LiteLLM wants `reasoning_effort` (a ladder) or Anthropic-style `thinking: {type: enabled, budget_tokens}`. Options: (a) map budget → `reasoning_effort` ladder (works for every reasoning model, lossy), (b) pass `thinking` through verbatim (exact budgets, Anthropic-backed models only), or (c) map to `reasoning_effort` and fall back to `thinking` on a 400. I lean (a) with the full LiteLLM ladder, plus (c)'s fallback only if your captures show it's needed.
4. **`/model/info` failure policy.** Should the plugin degrade gracefully (catalog from `/v1/models` alone, metadata omitted) when `/model/info` is forbidden for the key, or fail loudly? I'd degrade gracefully.
5. **Do you want a repo-level doc** (`docs/` or a README section) for the LiteLLM plugin, or just the crate?

## 4. Mapping sketch (for you to sanity-check)

- **Endpoints:** `GET {base}/v1/models`, `GET {base}/v1/model/info`, `POST {base}/v1/chat/completions` with `stream: true` + `stream_options.include_usage: true`.
- **Env:** `LITELLM_BASE_URL`, `LITELLM_API_KEY` (inherited, read by the plugin only — core never sees it), optional `LITELLM_MODEL`. Base URL normalized so both `http://host:4000` and `http://host:4000/v1` work.
- **ModelInfo:** `id` = `model_name`; dedupe by `model_name` (load-balanced groups repeat it — the reference's `if (name in map) continue` is right); `displayName` = `model_info.display_name`; `contextWindow` = `max_input_tokens ?? max_tokens`; `supportsThinking` = `supports_reasoning`; `supportsTextGeneration` = `mode == "chat"`; prices × 1e6 (`input_cost_per_token`, `output_cost_per_token`, `cache_read_input_token_cost`).
- **Errors:** 401/403 → `auth`; 429 → `rateLimit` retryable (+ `retry-after` header); 400/404/422 → `invalidRequest`; 5xx/timeout → `backend` retryable.
- **finish_reason:** `stop|length|tool_calls|function_call|content_filter`; unknown → `stop`.

## 5. Pitfalls I can already see

- **Capabilities are plugin-wide, but LiteLLM is heterogeneous.** `capabilities.thinking`/`tools` describe the whole plugin, so a proxy mixing reasoning and non-reasoning (or tool-capable and not) models will get 400s unless the adapter filters **per model** using the metadata it already cached from `/model/info` (i.e. drop `reasoning_effort` for `supports_reasoning != true`). The alternative — `drop_params: true` in the chat body — needs verifying against your proxy.
- **A 429 has two meanings.** Gateway rate limits/budgets/cooldowns set `retry-after`; provider 429s are forwarded without it. The retryable hint should probably be true either way, but the user-facing message should say which one it is.
- **`function_call` vs `tool_calls`.** LiteLLM still emits the legacy `function_call` finish reason on some routes; with the clanky protocol it should map to `ToolCalls`, not `Stop`.
- **Reasoning can be hidden.** If `merge_reasoning_content_in_choices` is on, thinking arrives inside `content` in ` thinking` tags instead of `delta.reasoning_content`. Cheap to handle both; needs the captures to confirm which your proxy does.
- **Virtual-key model scoping** means the catalog is per-key — nothing to do, but it makes the `/model/info` graceful-degradation decision matter.

## Next step

If you drop the captures from §1 into `./reference/litellm/` and answer the five decisions in §3, I'll scaffold `crates/clanky-provider-litellm` (adapter + `main.rs` + unit tests with a mock backend, mirroring the DeepInfra crate) and wire it into the workspace. If you'd rather I start now, I can build the OpenAI-compatible half against the DeepInfra shapes and treat the metadata/error mapping as the only thing left to pin down when the captures land — say the word.


---

_session total: 1854770 prompt tokens · 30538 completion tokens_

---

## Implementation notes (as built)

The plugin landed as `crates/clanky-provider-litellm` (binary
`clanky-provider-litellm`), mirroring `clanky-provider-deepinfra`. The five
decisions above were resolved as follows:

1. **Rust crate**, not a Python plugin — it reuses the protocol crate's
   `serve_stdio`, the blocking-`ureq` + SSE backend, and the DeepInfra
   adapter's delta/tool-call assembly almost verbatim.
2. **Name `litellm`; no built-in default model.** An optional `LITELLM_MODEL`
   is advertised at handshake when set; otherwise Clanky requires an explicit
   `model`/`--model`.
3. **Thinking → `reasoning_effort`**, mapped onto the full LiteLLM ladder
   (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`), with `off`
   omitting the field. No `thinking` passthrough or 400-fallback: the captures
   showed the effort ladder is accepted, and the per-model filtering (below)
   removes the main source of 400s.
4. **`/model/info` degrades gracefully.** A 403 (route not allowed for the
   virtual key) or a malformed body yields the plain `/v1/models` catalog with
   the hints omitted; a listing never fails because of it.
5. **README + crate docs** (no separate `docs/` tree).

Captures from `./reference/` are checked in as test fixtures
(`crates/clanky-provider-litellm/tests/fixtures/`): the catalog,
`/model/info`, and the plain/reasoning/tool SSE streams. The suite (31 tests)
pins the adapter to the observed bytes.

Pitfalls handled:

- **Heterogeneous capabilities** — `reasoning_effort` is dropped unless
  `supports_reasoning`, and `tools` unless `supports_function_calling`, using
  the `/model/info` hints cached at listing time. When metadata is
  unavailable the request is sent unchanged (the proxy's own `drop_params` is
  the safety net).
- **Legacy `function_call`** finish reason maps to `ToolCalls`.
- **Error envelopes** — `{"error":{"message":…}}` and `{"detail":…}` are
  unwrapped into the user-facing message rather than passed through as raw
  JSON; a 403 route denial maps to `auth`.
- **`merge_reasoning_content_in_choices: false`** in this deployment, so
  reasoning arrives in `delta.reasoning_content`; the merged-in-content case
  is documented as unsupported (not split).
