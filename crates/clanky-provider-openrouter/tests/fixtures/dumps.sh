#!/bin/sh
# Re-capture the OpenRouter fixtures this plugin's tests pin.
#
# The LiteLLM plugin's `litellm-reference/dumps.sh` is the pattern; the
# difference here is that OpenRouter needs a real key for the key-scoped
# catalog, so this script writes into the fixture directory directly.
#
# Captured 2026-11 against `deepseek/deepseek-v4-flash-0731` (cheap, tools,
# non-mandatory reasoning) and `anthropic/claude-haiku-4.5` (a *signed*
# reasoning trace, the one shape DeepSeek does not produce).
#
#   OPENROUTER_API_KEY=sk-or-v1-... sh dumps.sh
#
# Fixtures that cannot be captured on demand are marked below: a mid-stream
# `error` needs a provider to fail after the 200 is committed, and a `402`
# needs an account without credits. Both are pinned from the documented
# shapes in `provider.rs`'s tests instead.

set -e
: "${OPENROUTER_API_KEY:?set OPENROUTER_API_KEY}"
BASE="${OPENROUTER_BASE_URL:-https://openrouter.ai/api/v1}"
MODEL="${MODEL:-deepseek/deepseek-v4-flash-0731}"
HERE=$(dirname "$0")

chat() {
  curl -sS -N "$BASE/chat/completions" \
    -H "Authorization: Bearer $OPENROUTER_API_KEY" \
    -H 'Content-Type: application/json' -d "$1"
}

# The key-scoped catalog (not the public one: that lists models this key
# cannot call). Trimmed to the fields the plugin reads, so the fixture stays
# reviewable; every model is kept so the picker filters are exercised against
# the real `:batch` / `~` / `:free` / sentinel-price entries.
curl -sS "$BASE/models/user" -H "Authorization: Bearer $OPENROUTER_API_KEY" |
  jq '{data: [.data[] | {
        id, canonical_slug, name, context_length, pricing, supported_parameters,
        endpoints, reasoning,
        architecture: {
          modality: .architecture.modality,
          input_modalities: .architecture.input_modalities,
          output_modalities: .architecture.output_modalities
        }
      }]}' > "$HERE/models_user.json"

# Plain: reasoning off, so the stream is content only. The terminal usage
# chunk repeats `finish_reason` on a content-free choice — OpenRouter's
# documented deviation from OpenAI.
chat '{"model":"'"$MODEL"'","stream":true,"reasoning":{"effort":"none"},
       "messages":[{"role":"user","content":"say hi"}]}' > "$HERE/stream_plain.txt"

# Reasoning: `reasoning_details` deltas, the passback shape.
chat '{"model":"'"$MODEL"'","stream":true,"reasoning":{"effort":"low"},
       "messages":[{"role":"user","content":"2+2? think first"}]}' > "$HERE/stream_reasoning.txt"

# Tools: the model reasons, then announces a call whose id leads its args.
chat '{"model":"'"$MODEL"'","stream":true,
       "tools":[{"type":"function","function":{"name":"bash",
         "description":"Run a shell command",
         "parameters":{"type":"object","properties":{"command":{"type":"string"}},
                       "required":["command"]}}}],
       "messages":[{"role":"user","content":"list /tmp using the bash tool"}]}' \
  > "$HERE/stream_tools.txt"

# Signed reasoning (Anthropic via OpenRouter): text deltas, then a
# signature-only detail. `stream_reasoning.txt` has no signature because
# DeepSeek does not sign; this one pins the merged block.
curl -sS -N "$BASE/chat/completions" \
  -H "Authorization: Bearer $OPENROUTER_API_KEY" \
  -H 'Content-Type: application/json' -d '{
    "model":"anthropic/claude-haiku-4.5","stream":true,"reasoning":{"effort":"low"},
    "messages":[{"role":"user","content":"say hi"}]}' > "$HERE/stream_reasoning_signed.txt"

# Summary-shaped reasoning (OpenAI o-series/GPT reasoning, Grok): the model
# streams a *summary* of its reasoning as `reasoning.summary` deltas plus one
# opaque `reasoning.encrypted` blob, with no readable `reasoning.text` at all.
# This is the shape the passback rule has to get right.
curl -sS -N "$BASE/chat/completions" \
  -H "Authorization: Bearer $OPENROUTER_API_KEY" \
  -H 'Content-Type: application/json' -d '{
    "model":"openai/gpt-6-luna","stream":true,"reasoning":{"effort":"medium"},
    "messages":[{"role":"user","content":"2+2? think first"}]}' \
  > "$HERE/stream_reasoning_summary.txt"

# Error envelopes. `err_400.txt` is an unknown model id; `err_401.txt` a bad
# key; `err_429.txt` an upstream rate limit (its `error.message` is generic
# and the actionable text lives in `error.metadata.raw`).
curl -sS "$BASE/chat/completions" -H "Authorization: Bearer $OPENROUTER_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"no-such-model","messages":[{"role":"user","content":"hi"}]}' \
  > "$HERE/err_400.txt"

curl -sS "$BASE/chat/completions" -H "Authorization: Bearer sk-or-v1-invalid" \
  -H 'Content-Type: application/json' \
  -d '{"model":"'"$MODEL"'","messages":[{"role":"user","content":"hi"}]}' \
  > "$HERE/err_401.txt"

# Rate limiting is not reproducible on demand; this is the capture that
# happened to hit it, and the test only relies on the envelope shape.
chat '{"model":"inclusionai/ling-3.2-flash:free","stream":true,
       "messages":[{"role":"user","content":"hi"}]}' > "$HERE/stream_free.txt" || true
