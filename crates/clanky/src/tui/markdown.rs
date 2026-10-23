//! Minimal markdown rendering for the chat view (M3).
//!
//! Supports what a coding assistant actually emits: fenced code blocks,
//! headings, bullet/ordered lists, blockquotes, tables (with `:--`,
//! `:--:`, `--:` column alignment), inline code, bold, and italic.
//! Unrecognized syntax is rendered as plain text, so nothing is ever
//! lost.
//!
//! [`wrap`] pre-wraps styled lines to a viewport width so the scroll math
//! in the TUI stays exact (ratatui's own wrap would happen inside the
//! widget, after the scroll offset is already chosen).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// Render assistant text as styled lines (not yet wrapped). `width` is
/// the layout width the caller wraps to: tables are fitted and wrapped to
/// it here, so the later generic [`wrap`] pass passes them through
/// unchanged (that invariant is what the scroll math rides on).
pub fn render(text: &str, base: Style, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code = false;
    // Tabs are expanded to 8-column tab stops first: `unicode-width`
    // gives a tab no width, so the wrap math would budget zero columns
    // for it while a real terminal advances the cursor to the next
    // stop — off the end of the row and into the pending-wrap hazard
    // (see `screen::print_line`). Indentation inside code snippets
    // (common in thinking blocks) survives as spaces the math can see.
    let expanded = expand_tabs(text);
    let text: &str = &expanded;
    let source: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < source.len() {
        let raw = source[i];
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            lines.push(Line::from(Span::styled(raw.to_string(), dim(base))));
            i += 1;
            continue;
        }
        if in_code {
            let indent = raw.len() - trimmed.len();
            let mut spans = Vec::new();
            if indent > 0 {
                spans.push(Span::styled(" ".repeat(indent), Style::new()));
            }
            spans.push(Span::styled(trimmed.to_string(), code_style(base)));
            lines.push(Line::from(spans));
            i += 1;
            continue;
        }
        // A table needs a header row, a delimiter row (`--` cells, with
        // optional `:` for column alignment), and at least the columns to
        // line up. Consume the whole block at once; a non-matching row
        // ends the table and is rendered on its own.
        if let Some(alignments) = source
            .get(i + 1)
            .and_then(|next| table_delimiter(next.trim()))
            .filter(|a| split_table_row(trimmed).len() == a.len())
        {
            let mut body: Vec<Vec<&str>> = Vec::new();
            let mut j = i + 2;
            while let Some(row) = source.get(j) {
                let row = row.trim();
                if row.is_empty() || !row.contains('|') {
                    break;
                }
                body.push(split_table_row(row));
                j += 1;
            }
            let header = split_table_row(trimmed);
            lines.extend(render_table(&header, &body, &alignments, base, width));
            i = j;
            continue;
        }
        lines.extend(render_block_line(raw, base));
        i += 1;
    }
    lines
}

/// Word-wrap styled lines to `width` display columns, preserving styles.
/// Blank lines are preserved. Embedded newlines split the text into
/// separate source lines before wrapping (raw `\n` characters must never
/// reach the terminal buffer: the cursor moves down without returning to
/// column 0, producing a staircase of increasingly indented lines).
///
/// Unbounded: a very long source line wraps into many rows. Preview
/// entries (tool calls and results) use [`wrap_capped`] instead, so the
/// transcript height stays predictable no matter what a tool printed.
pub fn wrap(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    wrap_impl(lines, width, None, Style::new())
}

/// Word-wrap styled lines and cap the output at `max_lines` rows, adding
/// a note with the hidden count. Wrapping happens first, truncation
/// second: the result is exactly `min(wrapped, max_lines)` rows, which
/// the screen renderer's scroll math depends on. The note is appended
/// (dimmed) to the last visible row, replacing that row's trailing
/// spaces; hidden content that ends in a blank row loses it, which
/// matters only for whitespace.
pub fn wrap_capped(
    lines: &[Line<'static>],
    width: u16,
    max_lines: usize,
    note_style: Style,
) -> Vec<Line<'static>> {
    wrap_impl(lines, width, Some(max_lines), note_style)
}

/// Expand tabs to spaces at 8-column tab stops (per source line).
/// Display-side only: the transcript's stored text is untouched.
fn expand_tabs(text: &str) -> String {
    const TAB: usize = 8;
    if !text.contains('\t') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.contains('\t') {
            out.push_str(line);
            continue;
        }
        let mut col = 0usize;
        for c in line.chars() {
            if c == '\t' {
                let spaces = TAB - (col % TAB);
                out.push_str(&" ".repeat(spaces));
                col += spaces;
            } else {
                // Zero-width characters do not advance the column the
                // tab stop is measured against.
                col += c.width().unwrap_or(0);
                out.push(c);
            }
        }
    }
    out
}

fn wrap_impl(
    lines: &[Line<'static>],
    width: u16,
    max_lines: Option<usize>,
    note_style: Style,
) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in lines {
        let chars: Vec<(char, Style)> = line
            .spans
            .iter()
            .flat_map(|span| {
                let style = span.style.patch(line.style);
                span.content.chars().map(move |c| (c, style))
            })
            .collect();
        for segment in split_newlines(&chars) {
            if segment.is_empty() {
                out.push(Line::default());
                continue;
            }
            for piece in wrap_chars(&segment, width) {
                out.push(char_vec_to_line(piece));
            }
        }
    }
    match max_lines {
        None => out,
        Some(max_lines) => cap_lines(out, max_lines, width, note_style),
    }
}

/// Truncate already-wrapped rows to `max_lines`, appending a
/// `… (+N more lines)` note to the last visible row. The caller wraps
/// first, so the result is exactly `min(wrapped, max_lines)` rows. The
/// note replaces the end of the last row: its content is cut (from the
/// left; leading alignment spaces survive) to however many columns fit
/// before the note, so the note is always visible.
fn cap_lines(
    wrapped: Vec<Line<'static>>,
    max_lines: usize,
    width: usize,
    note_style: Style,
) -> Vec<Line<'static>> {
    if wrapped.len() <= max_lines {
        return wrapped;
    }
    let hidden = wrapped.len() - max_lines;
    let mut shown: Vec<Line<'static>> = wrapped.into_iter().take(max_lines).collect();
    let note = format!("… (+{hidden} more lines)");
    let note_width = note.width();
    // Too narrow for content plus note: the renderer hard-truncates
    // rows to `width` anyway, so keep the row as it is.
    if note_width >= width {
        return shown;
    }
    let budget = width - note_width;
    let tail = &mut shown[max_lines - 1];
    // Keep the surviving content at its own style (the row may mix
    // styles); only the note itself gets `note_style`. Rebuilding the
    // row as a single `note_style` span would recolor content the user
    // already saw in another color.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut piece_width = 0usize;
    for (c, style) in tail.spans.iter().flat_map(|s| {
        let span_style = s.style.patch(tail.style);
        s.content.chars().map(move |c| (c, span_style))
    }) {
        let cw = c.width().unwrap_or(0);
        if piece_width + cw > budget {
            break;
        }
        piece_width += cw;
        match spans.last_mut() {
            Some(last) if last.style == style => {
                last.content.to_mut().push(c);
            }
            _ => spans.push(Span::styled(c.to_string(), style)),
        }
    }
    spans.push(Span::styled(note, note_style));
    *tail = Line::from(spans);
    shown
}

/// Split styled chars at newlines into segments. `"a\n\nb"` becomes
/// `["a", "", "b"]` (the blank line is kept); a trailing newline does not
/// add an empty final segment.
fn split_newlines(chars: &[(char, Style)]) -> Vec<Vec<(char, Style)>> {
    let mut segments: Vec<Vec<(char, Style)>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    for &(c, style) in chars {
        if c == '\n' {
            segments.push(std::mem::take(&mut current));
        } else {
            current.push((c, style));
        }
    }
    if !current.is_empty() || segments.is_empty() {
        segments.push(current);
    }
    segments
}

/// Style each source line of raw `text` and word-wrap to `width`. Use
/// this for strings that may contain newlines: `Line::raw` strips embedded
/// newline characters, so the text must be split into one `Line` per
/// source line first (see the `wrap_splits_embedded_newlines` test).
/// Internal blank lines are preserved; a trailing newline adds nothing.
pub fn wrap_text(text: &str, style: Style, width: u16) -> Vec<Line<'static>> {
    let lines: Vec<Line<'static>> = text
        .lines()
        .map(|l| Line::from(Span::styled(l.to_string(), style)))
        .collect();
    if lines.is_empty() {
        return vec![Line::default()];
    }
    wrap(&lines, width)
}

/// Parse a table delimiter row like `| :-- | --: | :--: |` into one
/// alignment per column. A leading/trailing `|` is required so ordinary
/// dashes (`--` used as a horizontal rule, or `a - b` prose) don't turn
/// into a table.
fn table_delimiter(line: &str) -> Option<Vec<Align>> {
    let t = line.trim();
    if !t.starts_with('|') || !t.ends_with('|') {
        return None;
    }
    let cells: Vec<&str> = split_table_row(t);
    if cells.is_empty() {
        return None;
    }
    let mut aligns = Vec::with_capacity(cells.len());
    for cell in cells {
        let c = cell.trim();
        let left = c.starts_with(':');
        let right = c.ends_with(':');
        // A bare `:` (len 1, both ends colons) would make start == end; not
        // dashes, so bail out rather than slice.
        let start = usize::from(left);
        let end = c.len().saturating_sub(usize::from(right));
        if start >= end {
            return None;
        }
        let dashes = &c[start..end];
        if !dashes.chars().all(|ch| ch == '-') || dashes.is_empty() {
            return None;
        }
        aligns.push(match (left, right) {
            (true, true) => Align::Center,
            (true, false) => Align::Left,
            (false, true) => Align::Right,
            (false, false) => Align::None,
        });
    }
    Some(aligns)
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    None,
    Left,
    Center,
    Right,
}

/// Split a table row on `|`, dropping the required leading and trailing
/// pipes. Escaped pipes (`\|`) are not split markers.
fn split_table_row(line: &str) -> Vec<&str> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells = Vec::new();
    let mut start = 0;
    let bytes = t.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            i += 2;
            continue;
        }
        if bytes[i] == b'|' {
            cells.push(&t[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    cells.push(&t[start..]);
    cells.iter().map(|c| c.trim()).collect()
}

/// Render a table block as a box: box-drawing borders, column widths
/// fitted to `width`, and each cell word-wrapped to its column. The
/// column-sizing core is lifted nearly verbatim from ptm-project-todo-manager
/// (MIT); see `design-docs/tui-table-rendering-plan.md`.
///
/// Unlike ptm, inter-row rules are always drawn, and cells keep their
/// inline styling: wrapping goes through the same [`wrap_chars`] the
/// generic wrapper uses (width math per chunk via `unicode_width`), so
/// bold/code spans survive a cell splitting across physical rows.
fn render_table(
    header: &[&str],
    body: &[Vec<&str>],
    aligns: &[Align],
    base: Style,
    width: u16,
) -> Vec<Line<'static>> {
    let col_count = aligns.len();
    if col_count == 0 {
        return Vec::new();
    }

    // Render cells first (styles), then measure. A missing cell is empty,
    // so short rows still line up with the delimiter's column count.
    // Header cells render with a bold base so their text (and any inline
    // styling inside it) is bold; body cells use the plain base. The
    // padding spans below use `cell_s` for the same reason.
    let render_row = |cells: &[&str], style: Style| -> Vec<Vec<(char, Style)>> {
        (0..col_count)
            .map(|c| {
                let raw = cells.get(c).map(|s| unescape_pipes(s)).unwrap_or_default();
                inline(&raw, style)
                    .into_iter()
                    .flat_map(|span| {
                        span.content
                            .chars()
                            .map(move |c| (c, span.style))
                            .collect::<Vec<_>>()
                    })
                    .collect()
            })
            .collect()
    };
    let header_style = base.add_modifier(Modifier::BOLD);
    let header_cells = render_row(header, header_style);
    let body_cells: Vec<Vec<Vec<(char, Style)>>> =
        body.iter().map(|r| render_row(r, base)).collect();
    let all_rows: Vec<&Vec<Vec<(char, Style)>>> = std::iter::once(&header_cells)
        .chain(body_cells.iter())
        .collect();

    // Per-column minimum width = longest whitespace-delimited token in any
    // cell. A column scaled below this value will char-wrap at least one
    // word; we use it as a floor during proportional shrinking.
    let max_word_w: Vec<usize> = (0..col_count)
        .map(|ci| {
            all_rows
                .iter()
                .filter_map(|row| row.get(ci))
                .map(|cell| {
                    let mut words = vec![Vec::new()];
                    for &(ch, _) in cell.iter() {
                        if ch == ' ' {
                            words.push(Vec::new());
                        } else {
                            words.last_mut().unwrap().push(ch);
                        }
                    }
                    words
                        .iter()
                        .map(|w| w.iter().map(|c| c.width().unwrap_or(0)).sum::<usize>())
                        .max()
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(1)
                .max(1)
        })
        .collect();

    // Natural column widths (at least 1 char each).
    let mut col_w = vec![1usize; col_count];
    for row in &all_rows {
        for (i, cell) in row.iter().enumerate() {
            if i < col_count {
                col_w[i] = col_w[i].max(display_width(cell));
            }
        }
    }

    // Total width: "│" + for each col: " " + content + " " + "│".
    // (ptm adds a 2-space indent for tables; Clanky's transcript starts
    // at column 0, so the indent term drops out.)
    let total = 1 + col_w.iter().map(|w| w + 3).sum::<usize>();
    if total > width.max(1) as usize {
        let avail = (width as usize)
            .saturating_sub(1 + col_count * 3)
            .max(col_count);
        let sum_min: usize = max_word_w.iter().sum();

        if sum_min >= avail {
            // Even at per-word floors the table overflows; scale
            // proportionally from the floors, accepting char-wrapping.
            for (i, w) in col_w.iter_mut().enumerate() {
                let mw = max_word_w[i];
                *w = if sum_min == 0 {
                    1
                } else {
                    (((mw as f64) * (avail as f64) / (sum_min as f64)).floor() as usize).max(1)
                };
            }
        } else {
            // Give every column its per-word floor first, then distribute
            // the remaining budget proportionally to each column's natural
            // slack (natural_width − floor). This prevents char-wrapping
            // while still filling as much of the available width as possible.
            let sum_slack: usize = col_w
                .iter()
                .enumerate()
                .map(|(i, &nw)| nw.saturating_sub(max_word_w[i]))
                .sum();
            let remainder = avail - sum_min; // guaranteed ≥ 0
            for (i, w) in col_w.iter_mut().enumerate() {
                let min_w = max_word_w[i];
                let slack = (*w).saturating_sub(min_w);
                let extra = if sum_slack == 0 {
                    0
                } else {
                    ((slack as f64) * (remainder as f64) / (sum_slack as f64)).floor() as usize
                };
                *w = min_w + extra;
            }
        }
    }

    // Build a horizontal rule segment.
    let make_rule = |left: char, inner_sep: char, right: char| -> String {
        let mut s = String::new();
        s.push(left);
        for (i, &w) in col_w.iter().enumerate() {
            s.push_str(&"\u{2500}".repeat(w + 2));
            s.push(if i + 1 < col_count { inner_sep } else { right });
        }
        s
    };

    let top = make_rule('\u{250c}', '\u{252c}', '\u{2510}');
    let mid = make_rule('\u{251c}', '\u{253c}', '\u{2524}');
    let bot = make_rule('\u{2514}', '\u{2534}', '\u{2518}');

    let border_s = dim(base);
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(top, border_s)));

    for (row_idx, cells) in all_rows.iter().enumerate() {
        let cell_s = if row_idx == 0 {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        };

        // Word-wrap each cell to its column width (styles preserved).
        let wrapped: Vec<Vec<Vec<(char, Style)>>> = col_w
            .iter()
            .enumerate()
            .map(|(ci, &w)| wrap_chars(&cells[ci], w.max(1)))
            .collect();

        let row_height = wrapped.iter().map(|v| v.len()).max().unwrap_or(1);

        for line_idx in 0..row_height {
            let mut spans: Vec<Span<'static>> =
                vec![Span::styled("\u{2502}".to_string(), border_s)];
            for (ci, &w) in col_w.iter().enumerate() {
                let cell = wrapped[ci]
                    .get(line_idx)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let tlen = display_width(cell);
                let pad = w.saturating_sub(tlen);
                let pad_span = |n: usize| Span::styled(" ".repeat(n), cell_s);
                // The pad slots of ptm's `" {}{} "` become explicit spans
                // so per-char styles stay intact: a styled cell must not
                // repaint its padding or vice versa.
                let mut parts: Vec<Span<'static>> = vec![pad_span(1)];
                match aligns.get(ci).copied().unwrap_or(Align::None) {
                    Align::Right => {
                        parts.push(pad_span(pad));
                        parts.extend(cell.iter().copied().map(char_to_span));
                    }
                    Align::Center => {
                        let lpad = pad / 2;
                        parts.push(pad_span(lpad));
                        parts.extend(cell.iter().copied().map(char_to_span));
                        parts.push(pad_span(pad - lpad));
                    }
                    // Unspecified (`--`) and left (`:--`) both align left.
                    _ => {
                        parts.extend(cell.iter().copied().map(char_to_span));
                        parts.push(pad_span(pad));
                    }
                }
                parts.push(pad_span(1));
                spans.extend(parts);
                spans.push(Span::styled("\u{2502}".to_string(), border_s));
            }
            lines.push(Line::from(spans));
        }

        // Always-on separators: after the header, and after every body
        // row except the last (ptm's repeated-delimiter-row opt-in is
        // deliberately not ported). If a row wrapped, the rule still
        // comes after the logical row — one `mid` per gap, never inside.
        if row_idx + 1 < all_rows.len() {
            lines.push(Line::from(Span::styled(mid.clone(), border_s)));
        }
    }

    lines.push(Line::from(Span::styled(bot, border_s)));
    lines
}

/// One styled char as its own span (adjacent same-style chars are merged
/// later by `char_vec_to_line` inside `wrap_impl`, and kept as-is here).
fn char_to_span((c, style): (char, Style)) -> Span<'static> {
    Span::styled(c.to_string(), style)
}

/// Turn `\|` back into `|` inside table cells.
fn unescape_pipes(cell: &str) -> String {
    if cell.contains("\\|") {
        cell.replace("\\|", "|")
    } else {
        cell.to_string()
    }
}

fn render_block_line(raw: &str, base: Style) -> Vec<Line<'static>> {
    let trimmed = raw.trim_start();
    let indent = raw.len() - trimmed.len();

    // Headings: `#`..`######` followed by a space.
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes > 0 && hashes <= 6 && trimmed[hashes..].starts_with(' ') {
        let content = trimmed[hashes..].trim_start();
        let mut spans = Vec::new();
        if indent > 0 {
            spans.push(Span::styled(" ".repeat(indent), Style::new()));
        }
        spans.push(Span::styled(format!("{} ", &trimmed[..hashes]), dim(base)));
        spans.push(Span::styled(content.to_string(), heading_style(base)));
        return vec![Line::from(spans)];
    }

    // Blockquotes.
    if trimmed.starts_with("> ") {
        return vec![Line::from(Span::styled(raw.to_string(), quote_style(base)))];
    }

    // Bullets and ordered lists: color the marker, render the rest inline.
    if let Some(marker_len) = list_marker(trimmed) {
        let mut spans = Vec::new();
        if indent > 0 {
            spans.push(Span::styled(" ".repeat(indent), Style::new()));
        }
        spans.push(Span::styled(
            format!("{} ", &trimmed[..marker_len]),
            list_style(base),
        ));
        spans.extend(inline(trimmed[marker_len..].trim_start(), base));
        return vec![Line::from(spans)];
    }

    vec![Line::from(inline(raw, base))]
}

fn heading_style(base: Style) -> Style {
    base.add_modifier(Modifier::BOLD)
}

fn code_style(base: Style) -> Style {
    base.fg(Color::Cyan)
}

fn quote_style(base: Style) -> Style {
    base.fg(Color::Gray).add_modifier(Modifier::ITALIC)
}

fn list_style(base: Style) -> Style {
    base.fg(Color::Green)
}

fn dim(base: Style) -> Style {
    base.fg(Color::DarkGray)
}

/// Length of a list marker (`- `, `* `, `+ `, `1. `), if the line starts
/// with one.
fn list_marker(line: &str) -> Option<usize> {
    for marker in ["-", "*", "+"] {
        if line.starts_with(marker) && line[marker.len()..].starts_with(' ') {
            return Some(marker.len());
        }
    }
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && line[digits..].starts_with(". ") {
        return Some(digits + 1);
    }
    None
}

/// Parse inline markdown into spans. Markers may not nest; unmatched
/// markers are rendered literally.
pub fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            if let Some(end) = find_from(&chars, i + 1, &['`']) {
                flush(&mut spans, &mut plain, base);
                spans.push(Span::styled(
                    chars[i + 1..end].iter().collect::<String>(),
                    code_style(base),
                ));
                i = end + 1;
                continue;
            }
        } else if chars[i] == '*' {
            if i + 1 < chars.len() && chars[i + 1] == '*' {
                if let Some(end) = find_from(&chars, i + 2, &['*', '*']) {
                    let content = &chars[i + 2..end];
                    if flanking_ok(content) {
                        flush(&mut spans, &mut plain, base);
                        spans.push(Span::styled(
                            content.iter().collect::<String>(),
                            base.add_modifier(Modifier::BOLD),
                        ));
                        i = end + 2;
                        continue;
                    }
                }
            } else if let Some(end) = find_from(&chars, i + 1, &['*']) {
                let content = &chars[i + 1..end];
                if flanking_ok(content) {
                    flush(&mut spans, &mut plain, base);
                    spans.push(Span::styled(
                        content.iter().collect::<String>(),
                        base.add_modifier(Modifier::ITALIC),
                    ));
                    i = end + 1;
                    continue;
                }
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    flush(&mut spans, &mut plain, base);
    spans
}

fn find_from(chars: &[char], start: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() || start > chars.len() {
        return None;
    }
    (start..chars.len().saturating_sub(needle.len() - 1))
        .find(|&i| &chars[i..i + needle.len()] == needle)
}

fn flush(spans: &mut Vec<Span<'static>>, plain: &mut String, base: Style) {
    if !plain.is_empty() {
        spans.push(Span::styled(std::mem::take(plain), base));
    }
}

/// Emphasis content must not be empty or space-padded (`2 * 3` stays
/// literal, like CommonMark's flanking rules).
fn flanking_ok(content: &[char]) -> bool {
    !content.is_empty()
        && !content.first().is_some_and(|c| c.is_whitespace())
        && !content.last().is_some_and(|c| c.is_whitespace())
}

/// One run of the wrapper: a word (no spaces) or a run of spaces.
enum Token {
    Word(Vec<(char, Style)>),
    Space(Vec<(char, Style)>),
}

fn tokenize(chars: &[(char, Style)]) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut run: Vec<(char, Style)> = Vec::new();
    let mut run_is_space = false;
    for &(c, style) in chars {
        let is_space = c == ' ';
        if !run.is_empty() && is_space != run_is_space {
            tokens.push(finish_token(std::mem::take(&mut run), run_is_space));
        }
        run_is_space = is_space;
        run.push((c, style));
    }
    if !run.is_empty() {
        tokens.push(finish_token(run, run_is_space));
    }
    tokens
}

fn finish_token(run: Vec<(char, Style)>, is_space: bool) -> Token {
    if is_space {
        Token::Space(run)
    } else {
        Token::Word(run)
    }
}

fn display_width(chars: &[(char, Style)]) -> usize {
    chars.iter().map(|&(c, _)| c.width().unwrap_or(0)).sum()
}

fn wrap_chars(chars: &[(char, Style)], width: usize) -> Vec<Vec<(char, Style)>> {
    let mut out: Vec<Vec<(char, Style)>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut current_width = 0usize;

    for token in tokenize(chars) {
        match token {
            Token::Space(spaces) => {
                // Leading spaces are preserved too: the indentation of code
                // blocks, tool-output continuation lines, and multi-line
                // user input must survive wrapping.
                current_width += display_width(&spaces);
                current.extend(spaces);
            }
            Token::Word(word) => {
                let word_width = display_width(&word);
                if !current.is_empty() && current_width + word_width > width {
                    // Drop trailing spaces and break before the word.
                    while current.last().is_some_and(|&(c, _)| c == ' ') {
                        current.pop();
                    }
                    // With leading spaces preserved, the remainder can be
                    // empty (only spaces preceded an oversized word); an
                    // empty row would shift everything below.
                    if !current.is_empty() {
                        out.push(std::mem::take(&mut current));
                    }
                    current_width = 0;
                }
                if word_width > width && current.is_empty() {
                    // A single word longer than the line: hard-break it.
                    let mut piece: Vec<(char, Style)> = Vec::new();
                    let mut piece_width = 0usize;
                    for &(c, style) in &word {
                        let char_width = c.width().unwrap_or(0);
                        if piece_width + char_width > width && !piece.is_empty() {
                            out.push(std::mem::take(&mut piece));
                            piece_width = 0;
                        }
                        piece.push((c, style));
                        piece_width += char_width;
                    }
                    out.push(piece);
                    continue;
                }
                current_width += word_width;
                current.extend(word);
            }
        }
    }

    if out.is_empty() && current.is_empty() {
        // Preserve lines that were blank.
        out.push(Vec::new());
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn char_vec_to_line(chars: Vec<(char, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut current_style: Option<Style> = None;
    for (c, style) in chars {
        match current_style {
            Some(s) if s == style => buf.push(c),
            Some(s) => {
                spans.push(Span::styled(std::mem::take(&mut buf), s));
                current_style = Some(style);
                buf.push(c);
            }
            None => {
                current_style = Some(style);
                buf.push(c);
            }
        }
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, current_style.unwrap_or_default()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Vec<Span<'static>> {
        inline(text, Style::new())
    }

    fn span_text(spans: &[Span<'static>]) -> String {
        spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn bare_colon_delimiter_cell_does_not_panic() {
        // A delimiter cell of `:` (no dashes) is not a valid table: the
        // old `c[1..c.len()-1]` slice computed `c[1..0]` and panicked. It
        // must instead fail to form a table and render as plain text.
        let md = "| a | b |\n| : | : |";
        let lines = render(md, Style::new(), 80);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        assert!(texts.iter().any(|t| t.contains('|')));
    }

    #[test]
    fn tables_align_columns() {
        let md = "| name | qty |\n| :-- | --: |\n| cat | 1 |\n| hippopotamus | 42 |";
        let lines = render(md, Style::new(), 80);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(
            texts,
            [
                "┌──────────────┬─────┐",
                "│ name         │ qty │",
                "├──────────────┼─────┤",
                "│ cat          │   1 │",
                "├──────────────┼─────┤",
                "│ hippopotamus │  42 │",
                "└──────────────┴─────┘",
            ]
        );
        // The header row is bold; the borders are dim.
        assert!(
            lines[1]
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|s| s.style.fg == Some(Color::DarkGray))
        );
    }

    #[test]
    fn table_center_alignment() {
        let md = "| a | b |\n| :-: | -- |\n| x | yy |";
        let lines = render(md, Style::new(), 80);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(
            texts,
            [
                "┌───┬────┐",
                "│ a │ b  │",
                "├───┼────┤",
                "│ x │ yy │",
                "└───┴────┘",
            ]
        );
    }

    #[test]
    fn table_inline_styling_and_escaped_pipes() {
        let md = "| code | note |\n| -- | -- |\n| `x\\|y` | **b** |";
        let lines = render(md, Style::new(), 80);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        // The escaped pipe stays in the cell; the cell is one column.
        assert_eq!(texts[3], "│ x|y  │ b    │");
        assert!(
            lines[3]
                .spans
                .iter()
                .any(|s| s.style.fg == Some(Color::Cyan))
        );
    }

    #[test]
    fn table_stops_at_blank_line() {
        let md = "| a | b |\n| -- | -- |\n| 1 | 2 |\n\n| 3 | 4 |";
        let lines = render(md, Style::new(), 80);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        // The row after the blank line has no header, so it renders inline.
        assert_eq!(texts[0], "┌───┬───┐");
        assert_eq!(texts[2], "├───┼───┤");
        assert_eq!(texts[3], "│ 1 │ 2 │");
        assert!(texts[6].contains("3 | 4"));
    }

    #[test]
    fn table_box_structure_with_wrapped_rows() {
        // One body cell wraps to two physical lines; the inter-row rule
        // still comes after the *logical* row (one `mid` per gap), and
        // short cells in the same row pad with blanks. Column sizing at
        // width 12: avail = 12 − 1 − 2·3 = 5; the word floors are 5
        // ("word"/"other") and 1 ("bb"), sum 6 ≥ 5, so floors scale
        // proportionally: col1 = 4, col2 = 0→1 — both cells char-wrap.
        let md = "| a | bb |\n| -- | -- |\n| word other | x |\n| c | d |";
        let lines = render(md, Style::new(), 12);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(texts[0], "┌─────┬───┐");
        assert_eq!(texts[1], "│ a   │ b │");
        assert_eq!(texts[2], "│     │ b │");
        assert_eq!(texts[3], "├─────┼───┤");
        assert_eq!(texts[4], "│ wor │ x │");
        assert_eq!(texts[5], "│ d   │   │");
        assert_eq!(texts[6], "│ oth │   │");
        assert_eq!(texts[7], "│ er  │   │");
        assert_eq!(texts[8], "├─────┼───┤");
        assert_eq!(texts[9], "│ c   │ d │");
        assert_eq!(texts[10], "└─────┴───┘");
        // One rule per logical gap, never inside a wrapped row.
        assert_eq!(texts.iter().filter(|t| t.starts_with("├")).count(), 2);
    }

    #[test]
    fn table_fits_the_viewport() {
        let md = "| aaa | bbb | ccc |\n| -- | -- | -- |\n| xx | yy | zz |";
        let lines = render(md, Style::new(), 20);
        for line in &lines {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 20, "row of width {w}: {line:?}");
        }
    }

    #[test]
    fn table_word_floors_never_break_a_word() {
        // A long word in one column keeps its full width; the slack
        // comes out of the other column (sum of floors ≤ avail).
        let md = "| aa | bbbbbbbbbbbb |\n| -- | -- |\n| x | yy |";
        let lines = render(md, Style::new(), 30);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        // "bbbbbbbbbbbb" (12) must appear unsplit on its own line.
        assert!(
            texts.iter().any(|t| t.contains(" bbbbbbbbbbbb ")),
            "{texts:?}"
        );
        // And every row still fits.
        for line in &lines {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert!(w <= 30, "row of width {w}: {line:?}");
        }
    }

    #[test]
    fn table_styles_survive_wrapping() {
        // A bold cell split across rows keeps the bold span on each row.
        let md = "| t | u |\n| -- | -- |\n| **aaa bbb** | x |";
        let lines = render(md, Style::new(), 12);
        let bolds: Vec<bool> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
            })
            .collect();
        // Rows: top, header, mid, body line 1, body line 2, bottom.
        assert!(bolds[3], "first body line bold: {bolds:?}");
        assert!(bolds[4], "second body line bold: {bolds:?}");
        assert!(!bolds[0] && !bolds[2], "borders stay dim: {bolds:?}");
    }

    #[test]
    fn table_header_text_is_bold() {
        let md = "| a | b |\n| -- | -- |\n| x | y |";
        let lines = render(md, Style::new(), 40);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        // Row 1 is the header; its text spans carry bold, its padding and
        // the borders do not.
        let header_bolds: Vec<bool> = lines[1]
            .spans
            .iter()
            .map(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .collect();
        assert!(
            header_bolds.iter().any(|&b| b) && !header_bolds.iter().all(|&b| b),
            "header text bold, padding not: {header_bolds:?}"
        );
        assert_eq!(texts[1], "│ a │ b │");
        // Body rows stay unbold, including their padding.
        assert!(
            !lines[3]
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn table_counts_wide_characters_as_two_columns() {
        // A CJK cell counts 2 columns per char in width math and padding.
        let md = "| 中 | b |\n| -- | -- |\n| x | y |";
        let lines = render(md, Style::new(), 40);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        // The CJK column is 2 columns wide (1 char × 2); all rows line up.
        assert_eq!(texts[0], "┌────┬───┐");
        assert_eq!(texts[1], "│ 中 │ b │");
        assert_eq!(texts[3], "│ x  │ y │");
        for line in &lines {
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert_eq!(w, 10, "all rows line up: {line:?}");
        }
    }

    #[test]
    fn table_degenerate_width_does_not_panic() {
        // Five columns at width 12: even 1-char columns cannot fit.
        // Render anyway at w=1 and let the generic wrap split the rows
        // (then print_line truncates); the degradation must be stable.
        let md = "| a | b | c | d | e |\n| -- | -- | -- | -- | -- |\n| 1 | 2 | 3 | 4 | 5 |";
        let lines = render(md, Style::new(), 12);
        assert!(!lines.is_empty());
        for line in &lines {
            let _ = span_text(&line.spans);
        }
    }

    #[test]
    fn dashes_alone_are_not_a_table() {
        let lines = render("--\n:--\n--:", Style::new(), 80);
        let texts: Vec<String> = lines.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(texts, ["--", ":--", "--:"]);
    }

    #[test]
    fn plain_text_is_one_span() {
        let spans = plain("hello world");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "hello world");
        assert_eq!(spans[0].style, Style::new());
    }

    #[test]
    fn inline_code_is_styled() {
        let spans = plain("use `cargo test` now");
        assert_eq!(span_text(&spans), "use cargo test now");
        assert_eq!(spans[1].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn bold_and_italic_are_styled() {
        let spans = plain("**bold** and *slanted*");
        assert_eq!(span_text(&spans), "bold and slanted");
        assert_eq!(spans[0].style.add_modifier, Modifier::BOLD);
        assert_eq!(spans[2].style.add_modifier, Modifier::ITALIC);
    }

    #[test]
    fn unmatched_markers_are_literal() {
        let spans = plain("2 * 3 * 4");
        assert_eq!(span_text(&spans), "2 * 3 * 4");
    }

    #[test]
    fn headings_render_bold_without_hashes() {
        let lines = render("## Title", Style::new(), 80);
        let spans = &lines[0].spans;
        assert_eq!(span_text(spans), "## Title");
        assert_eq!(spans[1].style.add_modifier, Modifier::BOLD);
    }

    #[test]
    fn code_fences_toggle() {
        let lines = render("```rust\nlet x = 1;\n```", Style::new(), 80);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));
        let code = lines[1].spans.last().unwrap();
        assert_eq!(code.content, "let x = 1;");
        assert_eq!(code.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn bullets_color_the_marker() {
        let lines = render("- one\n2. two", Style::new(), 80);
        assert_eq!(lines[0].spans[0].content, "- ");
        assert_eq!(lines[0].spans[1].content, "one");
        assert_eq!(lines[1].spans[0].content, "2. ");
        assert_eq!(lines[1].spans[1].content, "two");
    }

    #[test]
    fn wrap_breaks_at_spaces() {
        let lines = vec![Line::raw("aaa bbb ccc")];
        let wrapped = wrap(&lines, 7);
        let texts: Vec<String> = wrapped.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(texts, ["aaa bbb", "ccc"]);
    }

    #[test]
    fn wrap_preserves_styles() {
        let lines = render("aaa `bbb` ccc", Style::new(), 80);
        let wrapped = wrap(&lines, 9);
        assert_eq!(wrapped.len(), 2);
        assert!(
            wrapped[0]
                .spans
                .iter()
                .any(|s| s.style.fg == Some(Color::Cyan))
        );
        assert_eq!(span_text(&wrapped[0].spans), "aaa bbb");
        assert_eq!(span_text(&wrapped[1].spans), "ccc");
    }

    #[test]
    fn wrap_hard_breaks_oversized_words() {
        let lines = vec![Line::raw("abcdefghij")];
        let wrapped = wrap(&lines, 4);
        let texts: Vec<String> = wrapped.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(texts, ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_counts_wide_characters_as_two_columns() {
        let lines = vec![Line::raw("中文中文中文")];
        let wrapped = wrap(&lines, 5);
        // Each CJK char is 2 columns: 2 per line, remainder on the last.
        assert_eq!(wrapped.len(), 3);
        assert_eq!(span_text(&wrapped[0].spans), "中文");
    }

    #[test]
    fn wrap_preserves_blank_lines() {
        let lines = vec![Line::raw("a"), Line::raw(""), Line::raw("b")];
        assert_eq!(wrap(&lines, 10).len(), 3);
    }

    /// Regression test: leading spaces used to be dropped, which would
    /// eat the indentation of code blocks and tool-output continuation
    /// lines once they flow through `wrap`.
    #[test]
    fn wrap_preserves_leading_spaces() {
        let lines = vec![Line::raw("  indent"), Line::raw("    deeper")];
        let wrapped = wrap(&lines, 20);
        let texts: Vec<String> = wrapped.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(texts, ["  indent", "    deeper"]);
    }

    #[test]
    fn wrap_splits_embedded_newlines() {
        // app.rs feeds multi-line strings through `lines()` (one `Line` per
        // source line, since `Line::raw` strips embedded newlines); `wrap`
        // must keep internal blank lines and not re-join anything.
        let lines: Vec<Line<'static>> = "aaa bbb\nccc\n\nddd".lines().map(Line::raw).collect();
        let wrapped = wrap(&lines, 7);
        let texts: Vec<String> = wrapped.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(texts, ["aaa bbb", "ccc", "", "ddd"]);
    }

    #[test]
    fn wrap_text_handles_multiline_and_styles_each_line() {
        let style = Style::new().fg(Color::Gray);
        let wrapped = wrap_text("aaa bbbbbbbbbbbb\n\nccc", style, 7);
        let texts: Vec<String> = wrapped.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(texts, ["aaa", "bbbbbbb", "bbbbb", "", "ccc"]);
        for line in &wrapped {
            for span in &line.spans {
                assert_eq!(span.style, style);
            }
        }
    }

    #[test]
    fn wrap_text_empty_is_one_blank_line() {
        assert_eq!(wrap_text("", Style::new(), 10).len(), 1);
    }

    #[test]
    fn wrap_trailing_newline_adds_no_blank() {
        let lines = vec![Line::raw("aaa\n")];
        let wrapped = wrap(&lines, 10);
        let texts: Vec<String> = wrapped.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(texts, ["aaa"]);
    }

    #[test]
    fn wrap_capped_truncates_after_wrapping() {
        let lines = vec![Line::raw("word ".repeat(200) + "end")];
        let uncapped = wrap(&lines, 40);
        let capped = wrap_capped(&lines, 40, 3, Style::new());
        assert_eq!(capped.len(), 3);
        assert!(uncapped.len() > 3, "{}", uncapped.len());
        let texts: Vec<String> = capped.iter().map(|l| span_text(&l.spans)).collect();
        assert!(texts[2].contains("… ("), "{texts:?}");
        assert!(texts[2].contains("more lines)"), "{texts:?}");
        // The visible rows above the note are untouched by the cap.
        let uncapped_texts: Vec<String> = uncapped.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(texts[0], uncapped_texts[0]);
        assert_eq!(texts[1], uncapped_texts[1]);
    }

    #[test]
    fn wrap_capped_note_line_keeps_content_styles() {
        // The surviving content on the note row keeps its own style;
        // only the `… (+N more lines)` note uses `note_style`. A row
        // colored cyan (a tool result preview) must not flip colors
        // mid-row when the cap kicks in.
        let content = Span::styled("word ".repeat(40), Style::new().fg(Color::Cyan));
        let lines = vec![Line::from(vec![content])];
        let capped = wrap_capped(&lines, 40, 1, Style::new().fg(Color::DarkGray));
        assert_eq!(capped.len(), 1);
        let text = span_text(&capped[0].spans);
        assert!(text.contains("… (+"), "{text}");
        assert_eq!(capped[0].spans[0].style.fg, Some(Color::Cyan));
        let note = capped[0].spans.last().unwrap();
        assert!(note.content.contains("more lines)"));
        assert_eq!(note.style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn wrap_capped_keeps_everything_under_the_cap() {
        let lines = vec![Line::raw("aaa bbb"), Line::raw("ccc")];
        let capped = wrap_capped(&lines, 20, 3, Style::new());
        assert_eq!(capped.len(), 2);
        assert!(!span_text(&capped[1].spans).contains("more lines"));
    }

    /// The note lands on the last visible row, never on an extra row:
    /// the renderer's row-count math depends on the exact count. The
    /// last row's content is cut from the left to make room.
    #[test]
    fn wrap_capped_note_shares_the_last_row() {
        // One oversized word hard-breaks into 40-column rows.
        let lines = vec![Line::raw("x".repeat(100))];
        let capped = wrap_capped(&lines, 40, 2, Style::new());
        let texts: Vec<String> = capped.iter().map(|l| span_text(&l.spans)).collect();
        assert_eq!(capped.len(), 2, "{texts:?}");
        assert_eq!(texts[0], "x".repeat(40));
        // 23 columns of content + the 17-column note = 40.
        assert_eq!(texts[1], format!("{}… (+1 more lines)", "x".repeat(23)));
    }

    #[test]
    fn wrap_zero_width_clamps_to_one_column() {
        let lines = vec![Line::raw("abc")];
        let wrapped = wrap(&lines, 0);
        let joined: String = wrapped
            .iter()
            .map(|l| span_text(&l.spans))
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(joined, "abc");
    }
}
