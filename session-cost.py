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

How tokens are attributed
-------------------------
Every completed chat round writes one `usage` record: `promptTokens`
restates the whole prompt that round sent, `completionTokens` is what the
model wrote, and `cachedTokens` is the part of the prompt the provider
served from its prompt cache. The file does not split those numbers by
content type, so this script splits them by character count:

  * completion tokens go to the categories of the round's own output:
    thinking text, assistant text, and tool-call arguments;
  * prompt tokens go to the categories of everything recorded *before*
    the round: user prompts, earlier answers, tool-call arguments and
    tool results — so context is billed where it was made.

Thinking is completion-side only. It is billed as output when the model
produces it, but it is never sent back: the rebuilt history carries user
prompts, assistant text, tool calls and tool results, and drops thinking
(`session::history_from_records`). So a long reasoning block costs once,
at the output rate, and never inflates later prompts.

The four categories are Thinking, Tools (tool calls and their results),
User (prompts) and Agent (visible assistant text). Shares are by
characters, not tokens, so a row is an estimate — but the rows always add
up to the billed usage exactly, so the total is the real total. The
system prompt (AGENTS.md, skills, ...) is not in the session file at all;
it is spread over the categories in the same proportion. Images a tool
result carried are left out of the character counts (their base64 length
says nothing about what the model was billed for them).
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


def char_len(value):
    return len(value) if isinstance(value, str) else 0


def shares(weights):
    """Normalize category weights to shares that sum to 1."""
    total = sum(weights.values())
    if total <= 0:
        even = 1.0 / len(CATEGORIES)
        return {c: even for c in CATEGORIES}
    return {c: weights[c] / total for c in CATEGORIES}


class Session:
    """One session file, folded into attributed token and cost totals."""

    def __init__(self, pricing=None):
        self.price = pricing
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

    def feed(self, record):
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
            self.fold_usage(record)
        elif kind == "settings":
            model = (record.get("settings") or {}).get("model")
            if model and model not in self.models:
                self.models.append(model)

    def fold_usage(self, record):
        """Attribute one round's billed tokens and fold the round in."""
        prompt = int(record.get("promptTokens") or 0)
        completion = int(record.get("completionTokens") or 0)
        cached = min(int(record.get("cachedTokens") or 0), prompt)
        self.prompt_tokens += prompt
        self.completion_tokens += completion
        self.cached_tokens += cached
        self.rounds += 1

        prompt_shares = shares(self.context)
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

        # The round just billed is context for the next one — except
        # thinking, which no provider sends back (see the module docstring).
        for cat in CATEGORIES:
            if cat != "thinking":
                self.context[cat] += self.pending[cat]
        self.pending = dict.fromkeys(CATEGORIES, 0)

    def total_cost(self):
        return sum(self.cost.values())

    def total_tokens(self):
        return sum(self.tokens.values())


def load_lines(lines, pricing=None):
    """Fold session records (JSONL lines) into one `Session`."""
    session = Session(pricing)
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
            session.feed(record)
        else:
            session.bad_lines += 1
    return session


def load_session(path, pricing=None):
    """Read one session file. Unreadable bytes are replaced, not fatal."""
    with open(path, encoding="utf-8", errors="replace") as handle:
        return load_lines(handle, pricing)


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
    """Check the attribution on a hand-checkable session.

    Two rounds. Round 1's prompt is the one-character user prompt, so all
    1000 prompt tokens go to User; its output is thinking "t", text "a"
    and one tool call, so the 100 completion tokens split by those
    character counts. Round 2's prompt is that context again — user,
    answer, tool call and tool result — and thinking is *not* in it,
    because no provider sends thinking back.
    """
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

    for failure in failures:
        print(f"FAIL {failure}", file=sys.stderr)
    if failures:
        return 1
    print("selftest ok")
    return 0


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
            session = load_session(path, pricing)
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
