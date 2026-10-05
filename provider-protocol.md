# Clanky Provider Plugin Protocol

Specification for the protocol between Clanky and provider plugins.
Target protocol version: **1**. Implemented in-process from M1 (loopback
transport); process transport arrives in M8.

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
- The plugin's working directory is Clanky's working directory.

## 2. Message envelope

Every message is one JSON object with a `"type"` field. Requests that expect a
response carry an integer `"id"`; responses and stream events reference it.
IDs are chosen by the client and unique per connection.

## 3. Handshake

Client → plugin, immediately on connect:

```json
{"type": "hello", "protocolVersion": 1}
```

Plugin → client:

```json
{"type": "hello", "protocolVersion": 1,
 "name": "deepinfra",
 "capabilities": {"listModels": true, "thinking": true, "tools": true}}
```

Rules:
- If the plugin does not speak the requested version, it replies with an
  `error` (`code: "protocolVersion"`) and exits. Clanky reports a clear error
  to the user ("plugin X requires protocol v2, Clanky speaks v1").
- `capabilities` flags are optional; absent = false. `listModels: false` means
  models come only from settings; `tools: false` means the plugin rejects
  chat requests containing `tools`.

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
             "inputPricePerMtok": 0.27,
             "outputPricePerMtok": 1.1}]}
```

All model fields except `id` are optional hints for pickers and UI display.
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
 "sampling": {"temperature": 0.7, "topP": 1.0, "maxTokens": 4096},
 "thinking": {"budgetTokens": 2048}
}
```

Field rules:
- `messages[].role` is `system` | `user` | `assistant` | `tool`.
- `content` is a plain string (no multimodal parts in v1; extension point for
  later versions).
- Assistant messages may carry `toolCalls` with **parsed** `arguments` (an
  object, not a JSON string — avoids double-encoding bugs in plugin authors'
  adapters).
- Tool results are `role: "tool"` messages with `toolCallId` + `content`.
- `tools` uses JSON Schema for `parameters` (OpenAI-compatible; maps directly
  to DeepInfra/function calling).
- `sampling` and `thinking` are optional; any field inside them may be omitted.
  `thinking` only sent if `capabilities.thinking`.

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
  complexity; parallel requests would need multiplexing we don't want in v1.

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
  `retryAfterMs` (optional, on `rateLimit`) may refine it.
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

- `protocolVersion` is a single integer. **v1 allows only additive, optional
  changes**; consumers must ignore unknown message types and unknown fields.
- Anything breaking (new required field, changed semantics, new in-flight
  rules) bumps to 2, and the handshake decides compatibility.

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
            emit({"type": "hello", "protocolVersion": 1, "name": "hello-world",
                  "capabilities": {}})
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
> {"type":"hello","protocolVersion":1}
< {"type":"hello","protocolVersion":1,"name":"deepinfra",
   "capabilities":{"listModels":true,"thinking":true,"tools":true}}
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
