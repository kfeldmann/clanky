# Plan — View-image (vision) tool (M20)

> **Status: implemented and live-verified** against a vision model —
> live testing amended the OpenAI-compatible wire shape (see the
> amendment note in §2): array `content` on a `tool` message is a 422,
> so images ride in a trailing `user` message. The protocol change is
> additive in v2.

A built-in `view-image` tool that puts an image file into the model's
context, for multimodal models. Scope: **image → model only.** The TUI
never displays the image (the tool-result transcript entry shows the size
note like any result); the model never generates images.

## 1. Protocol: images on tool results

`ChatMessage::Tool` gains one optional field:

```json
{"role": "tool", "toolCallId": "call_1", "content": "256 kb",
 "images": [{"dataBase64": "…", "mediaType": "image/jpeg"}]}
```

- `images` is a `Vec<ImagePart>`; **absent when there are none**, so every
  pre-existing message, session record, and golden stays byte-identical
  (additive in v2; no version bump — all three provider plugins are ours).
- `mediaType` is a closed enum (`png`, `jpeg`, `gif`, `webp`, `heic`,
  `heif`, `avif`; wire strings are the IANA `image/…` suffixes), not a
  free string: it composes into both backends' shapes (data URIs,
  Anthropic source blocks) with no validation gaps.
- Text `content` stays the primary field; turns continue to send text.
  A result with images keeps its size-note text and carries the data
  beside it.

## 2. Wire shapes per backend

OpenAI-compatible (DeepInfra, LiteLLM) — `image_url` data-URI parts after
the text part (`role: "tool"` accepts the same array `content` a user
message does):

```json
{"role": "tool", "toolCallId": "call_1",
 "content": [{"type": "text", "text": "256 kb"},
             {"type": "image_url",
              "image_url": {"url": "data:image/jpeg;base64,…"}}]}
```

> **Amended after live testing (the plan's one wrong guess):** the array
> form above is rejected by DeepInfra — its tool-message schema allows
> only a plain string (or a text-only array) for `content`, so an
> `image_url` part inside a `tool` message is a 422
> (`messages.5.tool.content.str: "Input should be a valid string"`),
> observed live against a vision model. The OpenAI-compatible shape as
> built keeps the `tool` message's `content` a plain string and carries
> the images in a **trailing `user` message** with bare `image_url`
> data-URI parts (no text — the result text stays in the `tool`
> message, unduplicated), emitted only when the result carries images
> so every image-free request is byte-identical, and always directly
> after the `tool` message it belongs to so replay reconstructs exactly
> what the model saw:
>
> ```json
> {"role": "tool", "toolCallId": "call_1", "content": "256 kb"}
> {"role": "user",
>  "content": [{"type": "image_url",
>               "image_url": {"url": "data:image/jpeg;base64,…"}}]}
> ```
>
> This supersedes the milestone's rejection of the follow-up-`user`
> shape: that argument was about a *bare* image turn with no tool
> context (replay divergence between providers); here the trailing user
> turn is deterministic and the protocol `ChatMessage` stream is
> unchanged — it is still one `tool` message carrying the images, and
> only the plugin's backend serialization differs. Anthropic keeps the
> in-`tool_result` shape (verified permitted by the Messages API).

Anthropic — **inside the `tool_result` block**, whose `content` may be an
array mixing text + image blocks (the Messages API permits this):

```json
{"role": "user",
 "content": [{"type": "tool_result", "tool_use_id": "call_1",
              "content": [{"type": "text", "text": "256 kb"},
                          {"type": "image",
                           "source": {"type": "base64",
                                      "media_type": "image/jpeg",
                                      "data": "…"}}]}]}
```

The alternative — a follow-up `user` message with a bare image block — was
rejected (as the milestone note anticipated): it injects a synthetic user
turn no other backend produces, so replay/history reconstruction would
diverge between providers. The tool_result-block shape needs no extra
headers per the Messages API docs; if a live probe shows images inside
`tool_result` are rejected for some model, that fallback is still
available and this file should record the decision.

A text-only model 400s when the image reaches it; the backend's error is
surfaced unchanged (invariant 2 — no local capability whitelist). A future
optional `supportsImages` hint (LiteLLM-style) could curate the picker,
like `supportsTools` in M10 — not required for this milestone.

## 3. Media-type resolution

Two-step, sniff first:

| Format | Sniff (magic bytes) | Extension |
|---|---|---|
| JPEG | `FF D8 FF` | `.jpg`/`.jpeg` |
| PNG | `89 50 4E 47` | `.png` |
| GIF | `GIF8` | `.gif` |
| WEBP | `RIFF…WEBP` | `.webp` |
| HEIC/HEIF/AVIF | (none — ISO-BMFF boxes are shared with video formats; a byte sniff cannot name the codec family reliably) | `.heic`/`.heif`/`.avif` |

`MediaType::sniff(bytes)` (protocol crate) covers the self-describing
formats; `MediaType::canonical_lookup(ext)` covers the rest (and any
misnamed file whose sniff fails). Case-insensitive; a file neither
sniffable nor extension-known is a tool failure ("unknown image media
type") the model can recover from. Sniffing wins over the extension, so a
JPEG named `.png` is still a JPEG.

## 4. Tool contract

`view-image {"path": …}` — description: "Read an image file from disk and
attach it so you can see it." (Name chosen over `read-image`/`vision`:
action-shaped verb matching how users phrase the intent — "show me", "take
a look", "see this screenshot" — while `view_` avoids the computer-science
connotation of "reading a file".)

- Reads a local file, base64-encodes it (standard alphabet, padded).
- Result text is a size note in SI units: `623 bytes` < 1 kB, `256 kb`
  < 1 MB, `1.2 mb` above (matches the milestone's `256 kb` example).
- The default tool set is now `bash`, `edit`, `view-image`; the timeout
  machinery never applies to it (plain buffered IO like `edit`).

## 5. Sessions and records

A tool result with images is written in full base64 (a screenshot is
~300–400 KB encoded — large JSONL lines, but the format rests on "replay
reconstructs exactly what the model saw"; truncating it violates that with
no principled cap). `toolResult` gains an optional `images` field with the
same additive/absent-when-none shape as the protocol; the record schema's
size expectation is this paragraph. `history_from_records` rebuilds the
image parts, and the resumed request re-attaches them (pinned by an
integration test). `/md` exports an `_attached image: image/jpeg (N bytes
decoded)_` note instead of the payload — the markdown file is for reading,
and a base64 blob would dwarf it.

## 6. In-memory plumbing

The `Tool` trait's `execute` now returns `ToolOutput { text, images }`
instead of a bare `String` (`From<String>` keeps plain tools one-liners).
The turn loop places non-empty image lists on the `tool` message
(`tool_with_images`); `TurnEvent::ToolResult` carries the images for the
recorder (session) and the transcript (size note only, images ignored for
display).

## 7. Tests

- Protocol: absent-images wire shape unchanged; images round-trip; legacy
  (field-less) results parse as `None`; sniff/extension tables.
- Providers: image-less tool results keep plain-string content; images
  become data-URI parts (DeepInfra, LiteLLM) / image blocks inside the
  tool_result (Anthropic).
- Tool: sniffed and extension-resolved media types, alias/case handling,
  sniff-wins-over-extension, unknown-type and missing-file failures,
  argument validation, size note.
- Golden transcript `tests/golden/view-image-tool.json`: pins the wire
  tool definition, the size note, and the inline images exactly as fed
  back over a view-image turn.
- Integration (`tests/sessions.rs`): an image-carrying turn survives
  save/reload and the resumed request re-attaches the same image.
