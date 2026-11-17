#!/usr/bin/env python3
"""Report token usage and cost for Clanky session files.

    ./session-cost.py [--pricing PRICING] [FILE_OR_DIR ...]

With no FILE arguments the sessions in ./.clanky/sessions are read (the
project scope's sessions, i.e. relative to the current directory). A FILE
argument may be a session .jsonl file or a directory of them.

Without --pricing the report is token counts. With --pricing it is a cost
estimate, using prices pasted from DeepInfra's model page verbatim:

    ./session-cost.py --pricing "$0.03 cached, $0.15 in, $0.50 out" tmp/*

The labels are `in`, `out` and `cached`; prices are per million tokens, as
DeepInfra lists them. `cached` may be omitted (cached tokens are then
priced at the input rate, exactly as Clanky's own status-line estimate
does), and an optional "/ 1M" suffix is ignored.

`--thinking-passback POLICY` re-attributes the prompt as if clanky had run
with a different reasoning-passback window (`off`, `previous-turn` or
`all`); by default the policy recorded in the session's own settings is
used. `--selftest` checks the attribution math on built-in sessions.

How tokens are attributed
-------------------------
Every completed chat round writes one `usage` record: `promptTokens`
restates the whole prompt that round sent, `completionTokens` is what the
model wrote, and `cachedTokens` is the part of the prompt the provider
served from its prompt cache. The file does not split those numbers by
content type, so this script splits them by content size:

  * completion tokens go to the categories of the round's own output:
    thinking text, assistant text, and tool-call arguments;
  * prompt tokens go to the categories of everything recorded *before*
    the round: user prompts, earlier answers, tool-call arguments, tool
    results — and passed-back thinking, below — so context is billed
    where it was made.

Passed-back thinking (M23)
--------------------------
Thinking is billed as output when the model produces it, and since M23 it
is also *sent back*: a round's reasoning rides the prompts of later
rounds. A `thinking` record's `blocks` field holds the payload that is
replayed, so the script attributes part of every later prompt to Thinking.

The cost model is per provider, because what is on the wire differs — and
the block itself says which case it is:

  * a block with a `signature` (or a `redacted` block's `data`) is the
    Anthropic shape. There the **signature is the payload and the text is
    a display convenience** — Anthropic bills no input tokens for the text
    at all (measured: byte-identical `input_tokens` with the text emptied
    or padded by 8,000 characters), and ~**0.15 tokens per signature
    char** for the signature;
  * a block with `text` and no signature is the OpenAI-compatible
    (DeepInfra/DeepSeek) shape, where the text *is* the payload, billed
    like any other text at the measured **4.49 characters/token**.

Both are expressed in the character-equivalent units the other rows use,
so the shares stay comparable: an Anthropic signature character counts
`0.15 x 4.49 = 0.67` characters, a DeepInfra thinking character counts
one.

How much of that reaches a prompt follows clanky's `thinking_passback`
window, read from the session's `settings` records (`off`,
`previous-turn` or `all`; `--thinking-passback` overrides it for a "what
if"). The default, `previous-turn`, keeps the current turn's rounds and
drops earlier turns': the tool loop's intra-turn passback is mandatory on
Anthropic and DeepSeek, and only a prefix of whole turns may be dropped,
so "current turn only" is the cheapest legal window. Measured on clanky's
own saved sessions, it adds ~56% to the prompt total and `all` ~73%, and
76% of the all-turns cost is the intra-turn share — the tool loop, not
cross-turn retention, is what a long reasoning session pays for. Prompt
caching still applies to the passed-back prefix (the marginal cost is
cache-read priced), which is what keeps it affordable.

Records written before M23 carry no `blocks`, so they attribute nothing
to Thinking on the prompt side — exactly what those sessions sent.

The four categories are Thinking, Tools (tool calls and their results),
User (prompts) and Agent (visible assistant text). Shares are by
character-equivalent size, not tokens, so a row is an estimate — but the
rows always add up to the billed usage exactly, so the total is the real
total. The system prompt (AGENTS.md, skills, ...) is not in the session
file at all; it is spread over the categories in the same proportion.
Images a tool result carried are left out of the character counts (their
base64 length says nothing about what the model was billed for them).

A worked example: two rounds, one turn. Round 1's prompt is the user
prompt; its output is 100 completion tokens split by its own output
characters. Round 2's prompt is that context *plus* round 1's reasoning
payload (if the window kept it), and its output is the answer. With
`off`, round 2's prompt goes to User/Tools/Agent alone.
"""

import argparse
import json
import re
import sys
from pathlib import Path

CATEGORIES = ("thinking", "tools", "user", "agent")
LABELS = {
    "thinking": "Thinking:",
    "tools": "Tools:",
    "user": "User:",
    "agent": "Agent:",
}
DEFAULT_DIR = Path(".clanky/sessions")

# Label spellings accepted in --pricing, mapped to their price.
PRICE_KEYS = {
    "in": "input",
    "input": "input",
    "out": "output",
    "output": "output",
    "cached": "cached",
    "cache": "cached",
}
PRICE_RE = re.compile(r"\$\s*([0-9]+(?:\.[0-9]+)?|\.[0-9]+)\s*([A-Za-z]+)?")


class PricingError(Exception):
    pass


class Pricing:
    """Dollars per million tokens for the billed rates.

    `cached` is `None` when the pricing gave no cache-read rate; cached
    prompt tokens then bill at the input rate, exactly as Clanky's own
    status-line estimate does when the catalog does not separate them.
    """

    def __init__(self, input, output, cached):
        self.input = input
        self.output = output
        self.cached = cached

    def cache_rate(self):
        return self.input if self.cached is None else self.cached


def parse_pricing(text):
    """Parse `$0.03 cached, $0.15 in, $0.50 out` (DeepInfra's listing)."""
    prices = {}
    for match in PRICE_RE.finditer(text):
        amount = float(match.group(1))
        word = (match.group(2) or "").lower()
        key = PRICE_KEYS.get(word)
        if key is None:
            raise PricingError(
                f"price ${amount:g} has no label (expected one of "
                f"in, out, cached): {match.group(0)!r}"
            )
        prices[key] = amount
    if not prices:
        raise PricingError(f"no prices found in {text!r}")
    missing = [k for k in ("input", "output") if k not in prices]
    if missing:
        raise PricingError(
            "pricing needs an 'in' and an 'out' price "
            f"(missing: {', '.join(missing)})"
        )
    return Pricing(
        input=prices["input"],
        output=prices["output"],
        cached=prices.get("cached"),
    )


# Measured cost of a passed-back reasoning block, in tokens.
#
# OpenAI-compatible providers (DeepInfra/DeepSeek) send the thinking *text*
# as `reasoning_content` and bill it like any other text: 4.49 chars/token
# (measured against the live API, ~1800 characters = +401 prompt tokens).
DEEPINFRA_CHARS_PER_TOKEN = 4.49
# Anthropic sends the opaque *signature*: the text is never billed as input
# (measured: identical `input_tokens` with it emptied or padded by 8,000
# chars) and the signature costs ~0.15 tokens/char (measured 0.13-0.20 over
# six blocks). Expressed in character-equivalents so the shares below stay
# comparable with the other rows.
ANTHROPIC_TOKENS_PER_SIGNATURE_CHAR = 0.15
ANTHROPIC_SIGNATURE_CHAR_EQUIVALENT = (
    ANTHROPIC_TOKENS_PER_SIGNATURE_CHAR * DEEPINFRA_CHARS_PER_TOKEN
)

PASSBACK_POLICIES = ("off", "previous-turn", "all")
DEFAULT_PASSBACK = "previous-turn"


def char_len(value):
    return len(value) if isinstance(value, str) else 0


def block_weight(block):
    """One passed-back block's prompt cost, in character-equivalents.

    A block carrying a `signature` (or a `redacted` block's `data`) is the
    Anthropic shape — the signature/data is the payload and the text is
    free. Otherwise it is the OpenAI-compatible shape and the text is the
    payload. Both are converted to the units the other categories use, so
    the reported shares stay comparable.
    """
    if not isinstance(block, dict):
        return 0
    payload = block.get("signature") or block.get("data")
    if isinstance(payload, str) and payload:
        return len(payload) * ANTHROPIC_SIGNATURE_CHAR_EQUIVALENT
    return char_len(block.get("text"))


def split_turns(records):
    """Group a session's records into turns.

    A turn starts at a `user` record, so the round records of one turn are
    the ones after its user record — the unit clanky's passback window
    (`previous-turn`) drops.
    """
    turns = [[]]
    for record in records:
        if isinstance(record, dict) and record.get("type") == "user":
            turns.append([])
        turns[-1].append(record)
    return turns


def shares(weights):
    """Normalize category weights to shares that sum to 1."""
    total = sum(weights.values())
    if total <= 0:
        even = 1.0 / len(CATEGORIES)
        return {c: even for c in CATEGORIES}
    return {c: weights[c] / total for c in CATEGORIES}


class Session:
    """One session file, folded into attributed token and cost totals.

    The whole file is read before anything is folded, because the prompt
    side needs to know which round each record belongs to *and* which turn:
    the passback window is a per-turn decision (see `_passback_context`).
    """

    def __init__(self, pricing=None, passback=None):
        self.price = pricing
        # The policy in effect. `None` means "read it from the session's
        # settings records" (the default: report what the session actually
        # sent); an explicit value is a "what if".
        self.passback_override = passback
        self.passback = DEFAULT_PASSBACK
        # Character counts of the content each category contributed.
        # `context` is what the next request will re-send (user prompts,
        # earlier rounds, tool results); `pending` is the round in flight
        # (its output becomes context once its usage record lands).
        self.context = dict.fromkeys(CATEGORIES, 0)
        self.pending = dict.fromkeys(CATEGORIES, 0)
        self.tokens = dict.fromkeys(CATEGORIES, 0.0)
        self.cost = dict.fromkeys(CATEGORIES, 0.0)
        self.prompt_tokens = 0
        self.completion_tokens = 0
        self.cached_tokens = 0
        self.rounds = 0
        self.models = []
        self.bad_lines = 0
        # The passback payload of each `thinking` record, built once by
        # `prepare`; the display text of a thinking record costs output but
        # never prompt, so this is the only thinking the prompt side sees.
        self._blocks = []

    def prepare(self, records):
        """Index the records' passback payloads and resolve the policy."""
        self._blocks = []
        configured = None
        for record in records:
            blocks = None
            if isinstance(record, dict) and record.get("type") == "thinking":
                blocks = record.get("blocks") or ()
            self._blocks.append(blocks)
            if isinstance(record, dict) and record.get("type") == "settings":
                value = (record.get("settings") or {}).get("thinkingPassback")
                if value in PASSBACK_POLICIES:
                    configured = value
        if self.passback_override is not None:
            self.passback = self.passback_override
        elif configured is not None:
            self.passback = configured

    def _passback_weight(self, billed, turn):
        """The passed-back thinking a round in `turn` carries.

        `billed` is the payload of the rounds that already finished, as
        `(turn, weight)` pairs: a round's prompt carries the payload of
        every *earlier* round the window keeps — all of them with `all`,
        only the same turn's with `previous-turn` (the default, i.e. the
        tool loop), none with `off`. A round's own blocks are not in its
        own prompt, which is why only finished rounds count.
        """
        if self.passback == "off":
            return 0.0
        return sum(
            weight
            for earlier_turn, weight in billed
            if self.passback == "all" or earlier_turn == turn
        )

    def feed(self, record, passback=0.0):
        """Fold one record in.

        `context` holds the prompt of the round about to bill: user prompts
        and tool results join it as they arrive (they are the round's
        input), while a round's own output waits in `pending` until its
        `usage` record commits it. `passback` is the passed-back thinking
        weight the window adds to that prompt.
        """
        kind = record.get("type")
        if kind == "user":
            self.context["user"] += char_len(record.get("text"))
        elif kind == "thinking":
            self.pending["thinking"] += char_len(record.get("text"))
        elif kind == "assistant":
            self.pending["agent"] += char_len(record.get("text"))
            calls = record.get("calls")
            if calls:
                self.pending["tools"] += char_len(json.dumps(calls))
        elif kind == "toolResult":
            # `images` is deliberately not counted: base64 length has no
            # relation to the image tokens the model was billed for.
            self.context["tools"] += char_len(record.get("output"))
        elif kind == "usage":
            self.fold_usage(record, passback)
        elif kind == "settings":
            model = (record.get("settings") or {}).get("model")
            if model and model not in self.models:
                self.models.append(model)

    def fold_usage(self, record, passback=0.0):
        """Attribute one round's billed tokens and fold the round in.

        `passback` is the passed-back thinking weight this round's prompt
        carried (`fold_records` computes it from the window).
        """
        prompt = int(record.get("promptTokens") or 0)
        completion = int(record.get("completionTokens") or 0)
        cached = min(int(record.get("cachedTokens") or 0), prompt)
        self.prompt_tokens += prompt
        self.completion_tokens += completion
        self.cached_tokens += cached
        self.rounds += 1

        context = dict(self.context)
        context["thinking"] += passback
        prompt_shares = shares(context)
        # A round that recorded no output of its own (nothing streamed)
        # still has completion tokens; spread those like the prompt.
        if sum(self.pending.values()) > 0:
            completion_shares = shares(self.pending)
        else:
            completion_shares = prompt_shares

        for cat in CATEGORIES:
            self.tokens[cat] += prompt * prompt_shares[cat]
            self.tokens[cat] += completion * completion_shares[cat]
            if self.price is not None:
                self.cost[cat] += (
                    (prompt - cached) * prompt_shares[cat] * self.price.input
                    + cached * prompt_shares[cat] * self.price.cache_rate()
                    + completion * completion_shares[cat] * self.price.output
                ) / 1_000_000

        # The round just billed is context for the next one. Thinking is
        # *not* added here: what a later prompt carries of it is the
        # passback window's decision, applied per round in `load_lines`
        # (the display text itself is never re-sent — only the payload).
        for cat in CATEGORIES:
            if cat != "thinking":
                self.context[cat] += self.pending[cat]
        self.pending = dict.fromkeys(CATEGORIES, 0)

    def total_cost(self):
        return sum(self.cost.values())

    def total_tokens(self):
        return sum(self.tokens.values())


def load_lines(lines, pricing=None, passback=None):
    """Fold session records (JSONL lines) into one `Session`."""
    session = Session(pricing, passback)
    records = []
    for line in lines:
        line = line.strip()
        if not line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            session.bad_lines += 1
            continue
        if isinstance(record, dict):
            records.append(record)
        else:
            session.bad_lines += 1
    return fold_records(records, session)


def fold_records(records, session):
    """Feed prepared records into `session`, with per-round prompt context.

    The prompt-side context of the round ending at record `i` is the sum of
    every earlier record's contribution — computed as a running total, so
    one pass over the file is enough — plus the passed-back thinking the
    window keeps for that round.
    """
    session.prepare(records)
    # Which turn each record belongs to: a turn starts at its `user` record.
    turn_of = []
    turn = 0
    for record in records:
        if record.get("type") == "user":
            turn += 1
        turn_of.append(turn)

    # The payload of the rounds that have already billed, as (turn, weight):
    # a round's own blocks join it only when its `usage` record arrives, so
    # they can never inflate its own prompt.
    billed = []
    inflight = []
    for index, record in enumerate(records):
        kind = record.get("type")
        if kind == "usage":
            session.feed(record, session._passback_weight(billed, turn_of[index]))
            billed.extend((turn_of[index], weight) for weight in inflight)
            inflight = []
            continue
        blocks = session._blocks[index]
        if blocks:
            inflight.append(sum(block_weight(block) for block in blocks))
        session.feed(record)
    return session


def load_session(path, pricing=None, passback=None):
    """Read one session file. Unreadable bytes are replaced, not fatal."""
    with open(path, encoding="utf-8", errors="replace") as handle:
        return load_lines(handle, pricing, passback)


def format_money(value):
    """A dollar amount, with enough decimals to stay readable when small."""
    if value < 0.01:
        return f"${value:.6f}"
    if value < 10:
        return f"${value:.4f}"
    return f"${value:,.2f}"


def format_tokens(value):
    return f"{int(round(value)):,}"


def render(name, session, pricing):
    """One session's report, as a list of lines."""
    def value(amount, tokens):
        if pricing:
            return format_money(amount)
        return format_tokens(tokens)

    rows = [(LABELS[cat], value(session.cost[cat], session.tokens[cat]))
            for cat in CATEGORIES]
    rows.append(("Total:", value(session.total_cost(), session.total_tokens())))
    width = max(len(text) for _, text in rows)
    lines = [name, "=" * len(name)]
    for label, text in rows[:-1]:
        lines.append(f"  {label:<10} {text:>{width}}")
    lines.append("  " + "-" * (10 + width))
    lines.append(f"  {rows[-1][0]:<10} {rows[-1][1]:>{width}}")
    footer = [
        f"in {session.prompt_tokens:,}",
        f"out {session.completion_tokens:,}",
        f"cached {session.cached_tokens:,}",
        f"{session.rounds} round{'' if session.rounds == 1 else 's'}",
    ]
    if session.models:
        footer.insert(0, ", ".join(session.models))
    lines.append("")
    lines.append("  " + " · ".join(footer))
    return lines


def expand_args(args):
    """Turn FILE_OR_DIR arguments into a list of session files."""
    paths = []
    for arg in args:
        path = Path(arg)
        if path.is_dir():
            paths.extend(sorted(path.glob("*.jsonl")))
        else:
            paths.append(path)
    return paths


SELFTEST_SESSION = """\
{"created":0,"type":"header","version":1}
{"type":"settings","settings":{"provider":"p","model":"m"}}
{"type":"user","text":"u"}
{"type":"thinking","text":"t"}
{"type":"assistant","text":"a","calls":[{"id":"1","name":"bash","arguments":{"command":"c"}}]}
{"type":"usage","promptTokens":1000,"completionTokens":100,"cachedTokens":400}
{"type":"toolResult","name":"bash","output":"r"}
{"type":"thinking","text":"t"}
{"type":"assistant","text":"a","calls":[]}
{"type":"usage","promptTokens":2000,"completionTokens":100,"cachedTokens":0}
"""


def selftest():
    """Check the attribution on hand-checkable sessions.

    First, a blockless session (the pre-M23 shape). Two rounds. Round 1's
    prompt is the one-character user prompt, so all 1000 prompt tokens go
    to User; its output is thinking "t", text "a" and one tool call, so the
    100 completion tokens split by those character counts. Round 2's prompt
    is that context again — user, answer, tool call and tool result — with
    no thinking in it, because the round recorded no passback blocks.

    Then the M23 half, which is the regression the milestone's item 7
    exists for: a session whose `thinking` records carry blocks must
    attribute part of the later prompt to Thinking — the old "never
    inflates a later prompt" rule would fail it.
    """
    failures = selftest_blockless() + selftest_passback()

    for failure in failures:
        print(f"FAIL {failure}", file=sys.stderr)
    if failures:
        return 1
    print("selftest ok")
    return 0


def selftest_blockless():
    """The pre-M23 shape: no blocks, so thinking never reaches a prompt."""
    pricing = parse_pricing("$0.30 cached, $1.50 in, $5.00 out")
    session = load_lines(SELFTEST_SESSION.splitlines(), pricing)

    call_chars = len(json.dumps([{"id": "1", "name": "bash",
                                  "arguments": {"command": "c"}}]))
    # Round 1 prompt: entirely the user prompt.
    tokens = {"thinking": 0.0, "tools": 0.0, "user": 1000.0, "agent": 0.0}
    # Round 1 completion, split by output characters (1 + 1 + call_chars).
    out1 = 1 + 1 + call_chars
    tokens["thinking"] += 100 * 1 / out1
    tokens["agent"] += 100 * 1 / out1
    tokens["tools"] += 100 * call_chars / out1
    # Round 2 prompt: user + answer + tool call + tool result (thinking out).
    ctx = {"user": 1, "agent": 1, "tools": call_chars + 1}
    ctx_total = sum(ctx.values())
    for cat, chars in ctx.items():
        tokens[cat] += 2000 * chars / ctx_total
    # Round 2 completion: one thinking block and one answer, equally sized.
    tokens["thinking"] += 50
    tokens["agent"] += 50

    failures = []
    for cat in CATEGORIES:
        if abs(session.tokens[cat] - tokens[cat]) > 1e-6:
            failures.append(f"{cat}: {session.tokens[cat]:.4f} != {tokens[cat]:.4f}")

    # The rows must always add up to the billed usage exactly.
    billed = session.prompt_tokens + session.completion_tokens
    if abs(session.total_tokens() - billed) > 1e-6:
        failures.append(f"total {session.total_tokens():.4f} != billed {billed}")
    expected_cost = (
        (600 * 1.50 + 400 * 0.30 + 100 * 5.00)
        + (2000 * 1.50 + 100 * 5.00)
    ) / 1_000_000
    if abs(session.total_cost() - expected_cost) > 1e-12:
        failures.append(
            f"cost {session.total_cost():.9f} != {expected_cost:.9f}"
        )

    return failures


# A session with passed-back reasoning (M23). Two turns, each one tool
# round then an answer, and every round records a block: an Anthropic-shaped
# one (signature payload) on the first, a DeepInfra-shaped one (text payload)
# on the second.
PASSBACK_SESSION = """\
{"created":0,"type":"header","version":1}
{"type":"settings","settings":{"provider":"p","model":"m","thinkingPassback":"previous-turn"}}
{"type":"user","text":"u"}
{"type":"thinking","text":"tt","blocks":[{"kind":"thinking","text":"","signature":"0123456789"}]}
{"type":"assistant","text":"a","calls":[{"id":"1","name":"bash","arguments":{"command":"c"}}]}
{"type":"usage","promptTokens":1000,"completionTokens":100}
{"type":"toolResult","name":"bash","output":"r"}
{"type":"thinking","text":"tt","blocks":[{"kind":"thinking","text":"wwwwwwwwww"}]}
{"type":"assistant","text":"a","calls":[]}
{"type":"usage","promptTokens":2000,"completionTokens":100}
{"type":"user","text":"u"}
{"type":"thinking","text":"tt","blocks":[{"kind":"thinking","text":"","signature":"0123456789"}]}
{"type":"assistant","text":"a","calls":[]}
{"type":"usage","promptTokens":3000,"completionTokens":100}
"""


def selftest_passback():
    """Passed-back thinking is attributed to the prompt (M23 item 7).

    Three rounds over two turns. Round 1 and round 2 are one turn (a tool
    call, then the answer), round 3 is the next turn. Round 2's prompt
    carries round 1's signature block; round 3's carries nothing, because
    the default `previous-turn` window drops the earlier turn's blocks —
    the "drop whole older turns, never a partial run" rule.
    """
    call_chars = len(json.dumps([{"id": "1", "name": "bash",
                                  "arguments": {"command": "c"}}]))
    # One Anthropic-shaped block: 10 signature chars at 0.15 tok/char,
    # expressed in the character-equivalents the shares use.
    sig = block_weight({"kind": "thinking", "signature": "0123456789"})
    text = block_weight({"kind": "thinking", "text": "wwwwwwwwww"})

    def expected(passback):
        """The attribution, computed round by round by hand."""
        tokens = dict.fromkeys(CATEGORIES, 0.0)
        rounds = [
            # (prompt tokens, prompt context, completion tokens, output chars)
            # Round 1: just the user prompt; its own output not yet in it.
            (1000, {"user": 1, "thinking": 0.0}, 100,
             {"thinking": 2, "agent": 1, "tools": call_chars}),
            # Round 2: round 1's context plus the tool result, plus round
            # 1's block when the window keeps it (same turn).
            (2000,
             {"user": 1, "agent": 1, "tools": call_chars + 1,
              "thinking": sig if passback != "off" else 0.0},
             100, {"thinking": 2, "agent": 1, "tools": 0}),
            # Round 3: the next turn. Only `all` carries turn 1's blocks.
            (3000,
             {"user": 2, "agent": 2, "tools": call_chars + 1,
              "thinking": (sig + text) if passback == "all" else 0.0},
             100, {"thinking": 2, "agent": 1, "tools": 0}),
        ]
        for prompt, ctx, completion, out in rounds:
            ctx = {cat: ctx.get(cat, 0.0) for cat in CATEGORIES}
            ctx_total = sum(ctx.values())
            out = {cat: out.get(cat, 0.0) for cat in CATEGORIES}
            out_total = sum(out.values())
            for cat in CATEGORIES:
                tokens[cat] += prompt * ctx[cat] / ctx_total
                tokens[cat] += completion * out[cat] / out_total
        return tokens

    failures = []
    for passback in PASSBACK_POLICIES:
        session = load_lines(PASSBACK_SESSION.splitlines(), passback=passback)
        want = expected(passback)
        for cat in CATEGORIES:
            if abs(session.tokens[cat] - want[cat]) > 1e-6:
                failures.append(
                    f"passback {passback} {cat}: "
                    f"{session.tokens[cat]:.4f} != {want[cat]:.4f}")
        billed = session.prompt_tokens + session.completion_tokens
        if abs(session.total_tokens() - billed) > 1e-6:
            failures.append(
                f"passback {passback}: total "
                f"{session.total_tokens():.4f} != billed {billed}")

    # The regression the milestone's item 7 exists for: with passback on, a
    # later prompt is partly Thinking. The old "thinking never inflates a
    # later prompt" rule would report the same number for all three.
    thinking = {
        passback: load_lines(
            PASSBACK_SESSION.splitlines(), passback=passback
        ).tokens["thinking"]
        for passback in PASSBACK_POLICIES
    }
    if not thinking["previous-turn"] > thinking["off"]:
        failures.append(
            f"passed-back thinking must add prompt tokens: {thinking}")
    if not thinking["all"] > thinking["previous-turn"]:
        failures.append(
            f"`all` keeps the earlier turn's blocks: {thinking}")

    # The session's own settings record decides the default; an explicit
    # --thinking-passback overrides it.
    recorded = load_lines(PASSBACK_SESSION.splitlines())
    if recorded.passback != "previous-turn":
        failures.append(
            f"the settings record's policy must be used: {recorded.passback}")
    forced = load_lines(PASSBACK_SESSION.splitlines(), passback="all")
    if forced.passback != "all":
        failures.append(f"an explicit policy must win: {forced.passback}")

    return failures


def main(argv):
    parser = argparse.ArgumentParser(
        prog="session-cost.py",
        description="Report token usage and cost for Clanky session files.",
        epilog=(
            "Without FILE arguments the sessions in ./.clanky/sessions are read. "
            "Pricing is per million tokens, pasted from DeepInfra verbatim, e.g. "
            '"$0.03 cached, $0.15 in, $0.50 out".'
        ),
    )
    parser.add_argument(
        "--pricing",
        metavar="PRICING",
        help='prices per million tokens, e.g. "$0.03 cached, $0.15 in, $0.50 out"',
    )
    parser.add_argument(
        "--thinking-passback",
        choices=PASSBACK_POLICIES,
        metavar="POLICY",
        help=(
            "attribute passed-back reasoning as if this window were in "
            "effect (off | previous-turn | all); by default the policy "
            "recorded in the session's settings is used"
        ),
    )
    parser.add_argument(
        "--selftest",
        action="store_true",
        help="check the attribution math on a built-in session and exit",
    )
    parser.add_argument(
        "files",
        nargs="*",
        metavar="FILE",
        help="session .jsonl files or directories of them",
    )
    args = parser.parse_args(argv)

    if args.selftest:
        return selftest()

    pricing = None
    if args.pricing is not None:
        try:
            pricing = parse_pricing(args.pricing)
        except PricingError as err:
            parser.error(f"--pricing: {err}")

    paths = expand_args(args.files) if args.files else expand_args([str(DEFAULT_DIR)])
    if not paths:
        where = " ".join(args.files) if args.files else str(DEFAULT_DIR)
        print(f"session-cost.py: no session files found in {where}", file=sys.stderr)
        return 1

    sessions = []
    for path in paths:
        try:
            session = load_session(path, pricing, args.thinking_passback)
        except OSError as err:
            print(f"session-cost.py: {path}: {err.strerror}", file=sys.stderr)
            continue
        if session.rounds == 0:
            print(
                f"session-cost.py: {path}: no usage records, skipping",
                file=sys.stderr,
            )
            continue
        if session.bad_lines:
            print(
                f"session-cost.py: {path}: skipped {session.bad_lines} "
                f"unparseable line{'s' if session.bad_lines != 1 else ''}",
                file=sys.stderr,
            )
        sessions.append((path.stem, session))

    if not sessions:
        print("session-cost.py: nothing to report", file=sys.stderr)
        return 1

    if pricing is not None and pricing.cached is None:
        print(
            "note: no cached price given; cached prompt tokens are priced "
            "at the input rate",
            file=sys.stderr,
        )

    first = True
    for name, session in sessions:
        if not first:
            print()
        first = False
        print("\n".join(render(name, session, pricing)))
        if pricing is not None and len(session.models) > 1:
            print(
                f"session-cost.py: {name}: session used {len(session.models)} "
                f"models ({', '.join(session.models)}); every round is priced "
                f"at the one --pricing given",
                file=sys.stderr,
            )

    if len(sessions) > 1:
        print()
        tokens = sum(s.total_tokens() for _, s in sessions)
        if pricing:
            total = sum(s.total_cost() for _, s in sessions)
            print(
                f"All {len(sessions)} sessions: {format_money(total)}"
                f" ({format_tokens(tokens)} tok)"
            )
        else:
            print(f"All {len(sessions)} sessions: {format_tokens(tokens)} tok")

    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
