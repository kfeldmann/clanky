# Clanky session: 20261018-172203-cfe6

| | |
|---|---|
| Provider | deepinfra |
| Model | zai-org/GLM-5.3-Flash |
| Created | 2026-10-18 17:22 |
| Contents | user and assistant messages |

## User

How hard would it be to have all queued messages sent at the earlist opportunity, rather than waiting for the LLM to end its turn? For example, if a tool call is made, we will be sending the result (along with the whole context) and that is an opportunity to send all queued messages.

## Assistant

Moderate — mostly plumbing, no protocol changes. I looked at the current code; here's the lay of the land and what it would take.

## Current mechanics

- Queued prompts live in `app.pending` (main thread / render loop). They're only consumed in `take_queued` **after** `WorkerEvent::Done`, i.e. when the turn's worker thread has fully exited.
- The tool loop is in `crates/clanky/src/turn.rs` `run_turn_with`, on the worker thread. Between rounds it appends tool results to `messages` and immediately requests the next round — that's exactly the "earliest opportunity" you describe.

## What a clean implementation looks like

1. **Bridge the queue across threads.** Replace (or supplement) `app.pending` with a shared inbox the turn worker can read — `Arc<Mutex<VecDeque<String>>>` or an `mpsc` the worker polls. The TUI keeps pushing to `app.pending` for display, and drains it into the shared inbox (or both share it).

2. **Inject in the tool loop.** In `run_turn_with`, at the top of each round (right after appending tool results, before building the request), drain the inbox and `messages.push(ChatMessage::user(text))` for each. Nothing about the wire protocol changes — they're just user messages in the context.

3. **Record + display in place.** For every injected prompt you need a new `TurnEvent` (e.g. `QueuedUser { text }`) so (a) the session recorder stores `Record::User` at the right point in history and (b) the TUI transcript prints the full text at its real position (mirroring the logic currently at the `take_queued` call site in `mod.rs` ~954). The golden transcripts in `tests/golden/` would then also cover it.

4. **Turn-end semantics.** When the model produces final text with no tool calls, the turn ends and remaining queued messages fall through to the existing `take_queued(turn_finished, …)` path — keep that as the fallback. So the rule becomes: *queued messages are consumed at any round boundary while the turn is still going; leftovers start the next turn.*

## Gotchas that add a bit of spice

- **The queue is mutable during a round.** The inbox must be checked only between rounds; don't try to splice messages into a mid-stream request (they'd be invisible to the in-flight call anyway and could arrive after the assistant already replied).
- **`max_tool_rounds` and finish_reason:** an injected user message mid-turn keeps the same turn; make sure loop-limit accounting doesn't get confused, and decide whether injected messages reset anything (they shouldn't).
- **Ctrl+C / `drain_turn_events`:** the quit path records events after the fact; the new `QueuedUser` events must flow through that path too so `/resume` reproduces the transcript exactly.
- **Error rounds:** a round that fails mid-stream is flushed with partial text; injection should happen before the *next* attempt, not into a failed state.
- **Alternative, harder design:** actually *interrupt* a streaming round when a message arrives (abort, keep the partial text as an assistant record, then rebuild and continue). That's substantially more work — turn cancellation plumbing plus tricky history/truncation semantics — and I'd call it a separate milestone. The between-rounds approach gets most of the benefit for a fraction of it.

## Effort estimate

Roughly one focused session: new inbox type + `TurnEvent` variant + injection point in the loop + recorder/TUI handling + tests (~a few hundred lines including tests). All confined to `clanky` core; `clanky-protocol` is untouched since it's just more user messages. The main risk is transcript/session bookkeeping, which the existing round/error-flush tests give you good scaffolding for.

If you want, I can implement the between-rounds version.


---

_session total: 22465 prompt tokens · 1522 completion tokens_

---

_Last turn ended with stop reason `stop`._
