# Plan — TUI markdown tables: box borders, fitted widths, wrapped cells

> **Status: planned.** Nothing implemented yet; this is the working plan.
> Source: `ptm-project-todo-manager` `src/ui/markdown.rs`
> (https://github.com/kfeldmann/ptm-project-todo-manager/blob/main/src/ui/markdown.rs),
> the author's other ratatui program, now MIT-licensed (same license as
> Clanky — see `LICENSE` in both repos). The column-sizing core of
> `render_table` is lifted nearly verbatim; everything touching Clanky's
> data model (styled cells, existing parser, border styles) is adapted.
> The MIT notice satisfies the attribution requirement.

## Motivation

Clanky's current table renderer (`crates/clanky/src/tui/markdown.rs`,
`render_table`) draws tables as plain `|`-and-`-` markdown source rows,
sized at *natural* width, then lets the generic word-wrapper reflow them.
In a narrow terminal a wide table wraps into an unreadable tangle of
broken rows: the wrapper splits a table row at a space, and the cell
content of one row ends up interleaved with border fragments.

The ptm renderer solves both problems: it draws tables with box-drawing
borders (`┌─┬┐`/`│`/`├─┼┤`/`└─┴┘`), computes column widths against the
available viewport (shrinking proportionally, with a per-column floor of
the longest word so words don't break), and word-wraps each cell to its
column. We want both, plus one deliberate departure: ptm's non-standard
feature where *repeating the delimiter row* between data rows opts into
inter-row separators is **not** ported — Clanky **always** draws the
inter-row borders.

## What we copy directly vs. adapt

**Copied essentially verbatim** (MIT, same project author):

- The column-sizing block from ptm's `render_table`: `max_word_w`
  computation (longest word per column), natural-width pass, the
  `total > width` overflow branch with its two regimes (proportional
  slack distribution when `sum_min < avail`; proportional floor-scaling
  when even the word floors overflow). The f64 arithmetic and the
  `saturating_sub` guards carry over; only the indent term drops out
  (ptm indents tables by 2; Clanky's transcript starts at column 0, so
  `total = 1 + Σ(wᵢ + 3)` and `avail = width − 1 − 3·cols`).
- The rule-builder closure: `make_rule(left, inner_sep, right)` —
  `┌─┬┐` / `├─┼┤` / `└─┴┘` per column width `w + 2`.
- The multi-line row emission: wrap each cell, `row_height` = max lines
  across cells, emit `│ text │ text │` per physical line with `line_idx`
  padding, `│` spans interleaved with cell spans.

**Adapted to Clanky** (different data model or deliberate choices):

- **Cells are styled spans, not plain strings.** ptm strips inline
  styling inside cells (`Event::Code`/bold/etc. are suppressed during
  cell accumulation); Clanky deliberately keeps inline styling
  (`table_inline_styling_and_escaped_pipes` — bold and inline code work
  in cells). So cell wrapping goes through the existing `wrap_chars`:
  flatten the cell's `inline()` spans to `(char, Style)` pairs — exactly
  what `wrap_impl` does today — wrap to the column width, then
  `char_vec_to_line` rebuilds spans. One wrapping code path for the
  whole module instead of a port of ptm's `word_wrap_cell`. Width math
  per chunk uses `unicode_width` as ptm does.
- **Style plumbing.** ptm's `Theme`-based styles are replaced with
  Clanky's `base`-derived ones: borders `dim(base)` (DarkGray), header
  bold (as today), separators same as borders (see below). Per-cell
  `cell_s` in the emit loop stays.
- **Parser.** No switch to `pulldown-cmark`. Clanky's hand-rolled block
  loop already detects tables, handles escaped pipes (`\|`), alignment
  (`:--`, `:--:`, `--:`), and inline styling in cells. Only the renderer
  changes.
- **Always-on inter-row rules** (the requested departure): ptm's
  opt-in machinery — `table_body_sep_after`, `is_separator_row`, and the
  repeated-delimiter-row feature with its lighter mid-gray body
  separator style (`Indexed(244)`) — is dropped entirely. Clanky draws
  `├─┼─┤` after every body row except the last, same style as the border
  (with separators always on, a second border shade would add noise,
  not hierarchy; the header/body divider keeps its distinctness from
  the bold header row above it).
- **Degenerate widths** (ptm never faces them; Clanky renders at any
  width ≥ 1): if `avail < cols`, even 1-char columns can't fit. Render
  anyway at `wᵢ = 1` and let the row exceed `width`: the generic
  `wrap()` pass splits it and `print_line` truncates at `width`
  regardless. Scroll math stays exact in every case because row counts
  come from the *wrapped* result. A test pins this so the degradation is
  at least stable.

## Non-goals

- No change to headings, lists, blockquotes, code fences, or the generic
  `wrap`/`wrap_capped`/`wrap_text` machinery — `entry_lines`' use of
  them is untouched except where noted.
- No new dependencies: `unicode-width` (already a dependency) covers the
  width math.

## Current state (the seam change)

`markdown::render(text, base)` is line-oriented and width-ignorant;
`markdown::wrap(lines, width)` reflows afterwards. A table block
(header + delimiter + body rows) is consumed whole by the block loop and
rendered by `render_table` at natural width: header row, a dashed rule,
body rows — no inter-row rules, no fitting, no cell wrapping.

The new renderer needs the target width (that's the whole point of
intelligent column sizing), so the plan touches the seam between the two
passes **for tables only**:

- `markdown::render(text, base)` → `markdown::render(text, base, width: u16)`.
  Both callers (`app.rs` `Entry::Assistant`, `Entry::Thinking`) already
  have `width` in scope. Tables come out of `render` *already fitted and
  wrapped*: every emitted row is ≤ `width`, so the later `wrap()` pass is
  a no-op on them (that's the invariant the scroll math rides on).
- Everything else `render` emits is unchanged and still word-wrapped by
  the generic `wrap()`.

Two consequences of width-aware rendering worth stating:

- **Streaming**: assistant text arrives incrementally and `entry_lines`
  re-renders the streaming entry each frame, so a table appears as plain
  text until the delimiter row lands, then re-draws as a growing box as
  rows arrive. Same as today's behavior, new look.
- **Resize**: sealed entries are printed to the terminal permanently
  (scrollback is never rewritten). A table fitted to the width at commit
  time stays at that width after a resize — identical to wrapped text
  today, nothing new to solve.

## Work breakdown

1. **`markdown::render` signature** — add `width: u16` (layout width,
   the caller's `entry_lines` width, not terminal width). Update both
   `app.rs` call sites and the module's own tests (which today call
   `render(md, Style::new())` — pass 80).
2. **`render_table` rewrite** — box borders and rule builder copied from
   ptm; column sizing copied (indent term dropped); styled cell wrapping
   via `wrap_chars` + alignment padding (right → pad left, center →
   split, left/none → pad right, as today); always-on inter-row rules.
   Cells keep flowing through the existing `inline()`/`unescape_pipes`
   path; `Align` stays.
3. **Tests** — update the four table tests in `markdown.rs`
   (`tables_align_columns`, `table_center_alignment`,
   `table_inline_styling_and_escaped_pipes`, `table_stops_at_blank_line`)
   to the new expected box output; add new ones (see Test plan).
4. **Docs** — README "Rendering notes" gains a sentence on tables
   (box-drawing borders, fitted to the viewport, cells wrapped);
   `milestones.md` gets an as-built note when this lands.

## Test plan

In `crates/clanky/src/tui/markdown.rs` (unit, width-driven via the new
`render` signature):

- Box structure: exact expected rows for a 2-column, 2-body-row table —
  `top`, header, `mid`, row, `mid`, row, `bottom` (the always-on
  inter-row rule is pinned here; also assert `mid` count = body rows − 1
  when some body row wraps to multiple physical lines — the rule still
  comes after the *logical* row).
- Fitting: a table whose natural width exceeds `width` shrinks to ≤
  `width`; every emitted row's display width is asserted ≤ `width`.
- Word floors: shrinking never breaks a word when `Σmin ≤ avail`
  (a long word in one column keeps its full width; the slack comes out
  of other columns).
- Cell wrapping: a cell longer than its column wraps to multiple
  physical rows, each carrying the full `│ … │` borders; short cells in
  the same row pad with blanks; alignment respected on every physical
  row.
- Styles survive wrapping: a `**bold**` cell split across rows keeps
  the bold span on each row (char-level styles, same as the existing
  `wrap_preserves_styles`).
- Escaped pipes + inline code still render inside the box.
- Wide characters: a CJK cell counts 2 columns per char in width math
  and padding.
- Degenerate width: 5 columns at width 12 — assert the render doesn't
  panic and rows degrade as described above.
- No-table regressions: `dashes_alone_are_not_a_table` and the rest of
  the suite keep passing (parser untouched).

In `crates/clanky/src/tui/app.rs` (integration through `entry_lines`):

- An assistant entry containing a table renders rows ≤ width at a
  narrow width (e.g. 40), exercising render+wrap seam (the later
  `wrap()` must not re-split any table row — assert that table row
  count equals the expected physical rows, i.e. no row of the box was
  further split).

Golden fixtures contain no tables (checked: no table delimiters in
`tests/golden/*.json`), so no golden updates are expected — verify by
running the golden suite anyway.

## Risks / open questions

- **Degenerate widths** are inherently lossy; the choice is between
  broken borders (split rows) and clipped borders (truncation). The
  plan picks split-then-clip because both fall out of existing machinery
  with zero new code paths. If it looks bad in practice, the fallback is
  to render such tables as today's plain `|`-rows — decide when
  implementing, with a test either way.
- **Session/export**: `screen_records` and `/md` export authored text
  verbatim; rendering is display-only, so no export changes.
