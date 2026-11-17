### ✔ Session 1 — Carry the payload (items 1–3, constraint 5) — **done** (see M23's "As built (session 1)")
Capture and transport, **no behavior change on the wire** — nothing is sent back yet, so this session is fully offline-testable and safe to leave half-verified.

- `AnthropicBlock` + `StreamDelta` gain `signature`/`data`; `signature_delta` arm joins the match; per-block struct replaces `HashMap<u32, String>` (finding 7's exact wire shapes: `content_block_start` carries `"signature":""`, `redacted_thinking` arrives whole on the start event).
- `ChatMessage::Assistant.thinking: Option<Vec<ThinkingBlock>>`, additive, no version bump; `provider-protocol.md` §5 + §9 updated in the same change.
- One terminal `ThinkingBlock` chunk on close (the doc's preferred shape), `StreamAssembler` returns finished blocks, `TurnEvent::Round` carries them.
- **Exit:** protocol unit tests for both transports incl. redacted + empty-text-signed; existing goldens pass *unchanged*; one new golden with blocks pins the request field.

### ✔ Session 2 — Send it back (items 4, 6) — **done** (see M23's "As built (session 2)")
The only session that touches the wire, and the one where the milestone's headline win lands.

- Turn loop attaches blocks to the pushed assistant message (`turn.rs:531`) and to `Continuable` resumes (`:511`); interrupted/cancelled rounds send none.
- Anthropic arm: content blocks in wire order, before the text block, everything sent (server filters).
- DeepInfra/LiteLLM arm: `reasoning_content` only on DeepInfra; `+ thinking_blocks` on LiteLLM. Field-name asymmetry test.
- **Exit:** live Anthropic tool loop shows `output_tokens_details` present on round 2 (the degradation bug closed); live DeepInfra +≈400 prompt tokens with a ~1800-char block. Both gated behind `#[ignore]`/env so CI stays offline-green.

### ✔ Session 3 — Session fidelity (item 5, constraint 6) — **done** (see M23's "As built (session 3)")
Split out because the doc calls it "the delicate part" and it's independent of 2 given 1: `Record::Thinking.blocks` (defaulted), `history_from_records` folding a round's thinking onto the *preceding* assistant message instead of walking positionally, `/resume` reconstruction tests, and the README warning that a session file / `/md --thinking` export is trusted input.

- **Exit:** a resumed session produces the identical `Vec<ChatMessage>` a live session had — including a `redacted_thinking` block and a synthetic-result repair for an interrupted call.

### Session 4 — Knob + honest cost docs (items 7, 8)
- `thinking_passback = "off" | "previous-turn" | "all"` through the full settings machinery (CLI/`/`-command optional — the doc doesn't ask for one; dump + session records + README required), window policy enforced as **prefix-only** with the unbroken-run rule (finding 6 inverts the original item 8 text — worth restating in the as-built note).
- `session-cost.py`: prompt attribution for passed-back thinking, rewritten docstring/worked example, regression fixture; README's "never inflates a later prompt" paragraph replaced with the measured factors, **with Anthropic and DeepInfra cost models stated separately** (signature ~0.15 tok/char vs text at 4.49 chars/token).

**Ordering:** 1 → 2 → 3 → 4, with 2 and 3 swappable. 4 must come last because the knob only means something once providers send; 3 needs only 1.

## If sessions still feel long

The natural further cuts are: (a) session 2 splits into Anthropic arm / OpenAI-compatible arm — genuinely separable, different wire formats; (b) item 7 (`session-cost.py` + README) becomes a standalone docs session, since it depends on behavior that already exists by then and touches no Rust.

## Two things to carry into every session

- Each session should append an "As built" note to M23 and update README/`provider-protocol.md` in the same change — the milestone already has a rich spike note, and the four-session split only stays coherent if the decisions land in the doc as they're made.
- Watch the invariants where M23 is most likely to drift: don't add local pre-validation of `--thinking` (finding 2 explicitly says the provider's 400 is the right UX), don't translate `enabled`↔`adaptive`, and don't "repair" blocks read from a session file.
