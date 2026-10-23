# Clanky Provider Plugin Protocol

Specification for the protocol between Clanky and provider plugins.
Target protocol version: **2** (v1 is superseded — see §9). Implemented
in-process from M1 (loopback transport); process transport arrives in M8.

Design goals, in order:

1. **Trivially implementable in any language.** A provider plugin is ~200 lines
   of Python. No schemas, no codegen, no binary framing required.
2. **Streaming-first.** Every chat is a stream; non-streaming is just the client
   buffering all chunks.
3. **Crash tolerance.** The plugin may die at any moment; the client must never
   deadlock or corrupt state because of it.
4. **Provider-agnostic.** Plugin authors adapt their backend's format
   (OpenAI-style deltas, Anthropic events, whatever) to one normalized shape.

## 1. Transport

- Clanky spawns the plugin as a subprocess and holds its stdin/stdout.
- **stdin/stdout carry newline-delimited JSON** (JSONL): one message per line,
  UTF-8, no embedded newlines inside a message. Partial lines are buffered;
  a message is complete only when its newline arrives.
- **stderr is never protocol.** It is captured and written to a debug log. This
  is a safety property: a plugin that prints a warning mid-JSON-response cannot
  corrupt the stream.
- The plugin exits when its stdin closes (EOF). Clanky treats unexpected exit
  as a crash: see §8.
- Clanky passes the environment through untouched. The plugin reads its own
  auth material (e.g. `DEEPINFRA_API_KEY`); Clanky never handles third-party
  provider keys. A plugin's settings entry may add extra env vars.
- **Interactive auth belongs in an offline subcommand, not the session.**
  The plugin's stdin/stdout *are* the protocol channel, so a plugin that
  needs an interactive flow (OAuth's browser + paste, device prompts)
  cannot ask the user mid-session. Give the plugin binary a subcommand —
  `clanky-provider-<name> login` — that runs the flow over its own
  terminal/stdin and stores the result on disk (e.g.
  `~/.clanky/providers/<name>/`); the serving process then loads it
  lazily, per request, so a login written after startup is picked up.
  This needs no protocol change: auth material is the plugin's business
  (this section), and an expired login surfaces as a normal `auth` error
  whose message names the subcommand to re-run. See
  `clanky-provider-anthropic login` for a working headless example
  (PKCE + pasted code, no browser).
- The plugin's working directory is Clanky's working directory.

## 2. Message envelope

Every message is one JSON object with a `"type"` field. Requests that expect a
response carry an integer `"id"`; responses and stream events reference it.
IDs are chosen by the client and unique per connection.

## 3. Handshake

Client → plugin, immediately on connect:

```json
{"type": "hello", "protocolVersion": 2}
```

Plugin → client:

```json
{"type": "hello", "protocolVersion": 2,
 "name": "deepinfra",
 "capabilities": {"listModels": true, "thinking": true, "tools": true},
 "defaultModel": "deepseek-ai/DeepSeek-V3"}
```

Rules:
- If the plugin does not speak the requested version, it replies with an
  `error` (`code: "protocolVersion"`) and exits. Clanky reports a clear error
  to the user ("plugin X requires protocol v2, Clanky speaks v1").
- `capabilities` flags are optional; absent = false. `listModels: false` means
  models come only from settings; `tools: false` means the plugin rejects
  chat requests containing `tools`.
- `defaultModel` is an optional, additive hint (v1): the model Clanky uses
  when neither settings nor `--model` name one. It lets the client keep no
  hardcoded per-provider table. Absent means "no built-in default; require
  explicit configuration".
- `name` is the provider's real name, and is what Clanky registers the plugin
  under (the `clanky-provider-<suffix>` filename is only the discovery hint).

## 4. List models

Client → plugin:

```json
{"type": "listModels", "id": 7}
```

Plugin → client:

```json
{"type": "models", "id": 7,
 "models": [{"id": "deepseek-ai/DeepSeek-V3",
             "displayName": "DeepSeek V3",
             "contextWindow": 128000,
             "supportsThinking": true,
             "supportsTools": true,
             "inputPricePerMtok": 0.27,
             "outputPricePerMtok": 1.1}]}
```

All model fields except `id` are optional hints for pickers and UI display.
The `supports*` flags (`supportsThinking`, `supportsTools`,
`supportsTextGeneration`) are tri-state: `false` marks a model that lacks the
capability — clients may hide it in pickers — while absent means unknown and
must be treated as usable. `supportsTools` is additive in v2.
Only sent when `capabilities.listModels` is true. `inputPricePerMtok` /
`outputPricePerMtok` are catalog prices in dollars per million tokens (the
`metadata.pricing.{input,output}_tokens` fields on DeepInfra); clients use
them for a session-cost estimate — prompt tokens are re-billed every turn,
so cost accumulates over the sum of per-turn usage.

## 5. Chat request

Client → plugin:

```json
{"type": "chat", "id": 42,
 "model": "deepseek-ai/DeepSeek-V3",
 "messages": [
   {"role": "system", "content": "You are a terminal coding agent."},
   {"role": "user", "content": "count files in /tmp"},
   {"role": "assistant", "content": "",
    "toolCalls": [{"id": "call_1", "name": "bash",
                   "arguments": {"command": "ls /tmp | wc -l"}}]},
   {"role": "tool", "toolCallId": "call_1", "content": "42"}
 ],
 "tools": [{"name": "bash",
            "description": "Run a shell command",
            "parameters": {"type": "object",
                           "properties": {"command": {"type": "string"}},
                           "required": ["command"]}}],
 "sampling": {"temperature": 0.7, "top_p": 1.0, "max_tokens": 4096,
              "seed": 42},
 "thinking": {"budgetTokens": 2048}
}
```

Field rules:
- `messages[].role` is `system` | `user` | `assistant` | `tool`.
- `content` is a plain string (the primary field; the multimodal extension
  point for later versions is the additive `images` list below).
- Assistant messages may carry `toolCalls` with **parsed** `arguments` (an
  object, not a JSON string — avoids double-encoding bugs in plugin authors'
  adapters).
- Tool results are `role: "tool"` messages with `toolCallId` + `content`.
  A tool result may additionally carry `images` — an optional, additive
  list of `{"dataBase64": …, "mediaType": "jpeg|png|gif|webp|heic|heif|avif"}`
  parts the tool put into the model's context (`view-image`'s attachment,
  M20); absent when there are none, so a plugin that ignores the field
  still serves text-only sessions correctly. The plugin is responsible for
  building its backend's multimodal shape: OpenAI-compatible endpoints get
  `image_url` data-URI parts (`data:<media>;base64,<data>`) appended after
  the text part; Anthropic gets `image` source blocks inside the
  `tool_result` block (its `content` may be an array mixing text + image
  blocks).
- `tools` uses JSON Schema for `parameters` (OpenAI-compatible; maps directly
  to DeepInfra/function calling).
- `sampling` is an **open map, passed through verbatim**: its keys are the
  backend's own parameter names (`temperature`, `top_p`, `max_tokens`,
  `seed`, `reasoning_effort`, `repetition_penalty`, whatever the backend
  accepts) and the plugin merges them unchanged into its backend request.
  Neither Clanky nor the plugin renames, validates, or drops them — the
  backend does, and its own error message is what the user sees. The one
  caveat: the request's structural fields (`model`, `messages`, `system`,
  `tools`, `stream`, …) belong to the protocol and are set by the plugin
  afterwards; they cannot be overridden through `sampling`. Filling a
  required knob the user did not set (e.g. Anthropic's `max_tokens`) is
  fine; overwriting a key the user did set is not.
- `sampling` and `thinking` are optional; `thinking` is only sent if
  `capabilities.thinking`.
- `thinking.budgetTokens: 0` means the user explicitly disabled thinking
  (`--thinking off`); an absent `thinking` means "unset" — the backend's
  default applies (which on reasoning models *enables* reasoning). A plugin
  that has an explicit "off" knob should send it for a zero budget instead
  of omitting its backend parameter (e.g. DeepInfra/LiteLLM
  `reasoning_effort: "none"`; Anthropic omits its `thinking` block either
  way, since an absent block is that API's documented "off").
- Any other top-level `chat` field is reserved for future versions; ignore
  unknown fields.

## 6. Stream chunks and completion

While a chat is in flight, the plugin sends `chunk` events, then exactly one
terminal message (`done` or `error`). Payload variants:

```json
{"type": "chunk", "requestId": 42, "payload": {"kind": "text", "text": "There "}}
{"type": "chunk", "requestId": 42, "payload": {"kind": "thinking", "text": "Need to ls..."}}
{"type": "chunk", "requestId": 42,
 "payload": {"kind": "toolCallStart", "index": 0, "id": "call_1", "name": "bash"}}
{"type": "chunk", "requestId": 42,
 "payload": {"kind": "toolCallArgs", "index": 0, "argsChunk": "{\"command\":"}}
```

Plugin → client, terminal:

```json
{"type": "done", "requestId": 42, "finishReason": "toolCalls",
 "usage": {"promptTokens": 1523, "completionTokens": 87}}
```

Rules:
- `kind: "text"` deltas append to the assistant text; `kind: "thinking"`
  deltas append to the thinking text (shown in the TUI, stored in the session).
- `toolCallStart` announces a tool call by `index`; `toolCallArgs` appends a
  fragment of that call's JSON arguments string, which the client assembles and
  parses on `done`. `index` is a 0-based counter per response (maps directly to
  OpenAI-style `tool_calls[i].index` deltas).
- `finishReason`: `stop` | `toolCalls` | `length` | `cancelled` | `contentFilter`.
- `usage` is optional; present it when the backend reports it.
- **Ordering:** chunks for a given `requestId` arrive in order. **Only one chat
  may be in flight per plugin connection.** This removes all interleaving
  complexity; parallel requests would need multiplexing we don't want in
  this protocol.

## 7. Cancellation

Client → plugin:

```json
{"type": "cancel", "requestId": 42}
```

- The plugin stops generating for that request and promptly emits
  `done` with `finishReason: "cancelled"` (or `error` if the backend errors
  out during abort).
- Cancel is advisory. If no terminal message arrives within ~2 seconds, the
  client kills the process (restart per §8). This is always safe because every
  event carries `requestId`.
- After `done`/`error`, the connection is free for the next `chat`.

## 8. Errors and crash handling

Plugin → client:

```json
{"type": "error", "requestId": 42,
 "code": "auth", "message": "401: invalid API key", "retryable": false}
```

- `code` is one of: `auth` | `rateLimit` | `invalidRequest` | `backend` |
  `protocol` | `protocolVersion` | `internal`. `retryable` is a hint;
  `retryAfterMs` (optional, on `rateLimit`) may refine it. The client acts
  on the hint: a `retryable` request-scoped error is retried (default 5
  attempts, backoff growing ×1.25 per retry, honoring `retryAfterMs`), so providers
  should set it for transient conditions (`rateLimit`, `backend` — network
  timeouts included) and leave it `false` for `auth`/`invalidRequest`. A
  retry replays the whole request, so it is only attempted while the round
  has streamed nothing yet (otherwise a partial answer would be
  duplicated).
- `error` responds to a request (carries its `id`/`requestId`) or aborts an
  in-flight stream (carries `requestId`, terminal — no `done` follows).
- An `error` with **no** `requestId` is connection-fatal: the plugin is telling
  the client it cannot continue (e.g. protocol violation). Client logs it and
  restarts the plugin before the next request.
- Unexpected process exit mid-stream = crash. Clanky surfaces a user-facing
  error for the in-flight turn; restart policy is per settings
  (default: restart on next use, not auto-retry the turn — silent retries can
  double-run tool calls).

## 9. Versioning policy

- `protocolVersion` is a single integer. **A version allows only additive,
  optional changes**; consumers must ignore unknown message types and unknown
  fields.
- Anything breaking (new required field, changed semantics, new in-flight
  rules) bumps the version, and the handshake decides compatibility.
- Version history:
  - **v1** — initial protocol. `sampling` was a closed struct
    (`temperature`, `topP`, `maxTokens`): every other sampling parameter was
    rejected or dropped, and two keys were renamed.
  - **v2** — `sampling` is an open, verbatim pass-through map whose keys are
    the backend's parameter names (e.g. `top_p`, `max_tokens`). Breaking for
    v1 peers (renamed keys, changed semantics), so the handshake rejects
    them loudly instead of silently dropping parameters.
  - **v2, additive** — `ChatMessage` tool results may carry `images`
    (M20: inline image parts on a `tool` message, absent when there are
    none; see §5 field rules). No version bump: additive and optional, and
    the three provider plugins in the repo were updated in the same change.

## 10. M1 loopback transport

In M1 the DeepInfra client implements exactly these message shapes behind a
`Transport` trait with two implementations:

- `LoopbackTransport` (M1): `send(msg) -> impl Stream<msg>` calling the
  in-process DeepInfra adapter directly. No process, no stdio.
- `ProcessTransport` (M8): spawns the plugin, speaks JSONL over stdio, adds
  lifecycle (§1, §8) — the message handling above is unchanged.

Consequence: M8's work is confined to transport + lifecycle; the M1 client code
does not change.

## Appendix A: minimal Python provider plugin (~40 lines of the interesting part)

```python
#!/usr/bin/env python3
import json, os, sys, urllib.request

def emit(msg): print(json.dumps(msg), flush=True)

def main():
    for line in sys.stdin:
        msg = json.loads(line)
        if msg["type"] == "hello":
            emit({"type": "hello", "protocolVersion": 2, "name": "hello-world",
                  "capabilities": {}, "defaultModel": "hello-world/tiny"})
        elif msg["type"] == "chat":
            # ... call backend, translate each delta to a chunk event ...
            emit({"type": "chunk", "requestId": msg["id"],
                  "payload": {"kind": "text", "text": "Hello from a plugin!"}})
            emit({"type": "done", "requestId": msg["id"], "finishReason": "stop"})

if __name__ == "__main__":
    main()
```

## Appendix B: example transcript (tool call)

```
> {"type":"hello","protocolVersion":2}
< {"type":"hello","protocolVersion":2,"name":"deepinfra",
   "capabilities":{"listModels":true,"thinking":true,"tools":true},
   "defaultModel":"deepseek-ai/DeepSeek-V3"}
> {"type":"listModels","id":7}
< {"type":"models","id":7,"models":[{"id":"deepseek-ai/DeepSeek-V3"}]}
> {"type":"chat","id":42,"model":"deepseek-ai/DeepSeek-V3",
   "messages":[...],"tools":[...]}
< {"type":"chunk","requestId":42,"payload":{"kind":"thinking","text":"ls /tmp..."}}
< {"type":"chunk","requestId":42,"payload":{"kind":"toolCallStart","index":0,
   "id":"call_1","name":"bash"}}
< {"type":"chunk","requestId":42,"payload":{"kind":"toolCallArgs","index":0,
   "argsChunk":"{\"command\": \"ls /tmp | wc -l\"}"}}
< {"type":"done","requestId":42,"finishReason":"toolCalls",
   "usage":{"promptTokens":1523,"completionTokens":87}}
  ... clanky runs bash, appends role:"tool" message, sends new chat id:43 ...
```
