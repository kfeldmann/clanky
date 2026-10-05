//! Minimal markdown rendering for the chat view (M3).
//!
//! Supports what a coding assistant actually emits: fenced code blocks,
//! headings, bullet/ordered lists, blockquotes, inline code, bold, and
//! italic. Unrecognized syntax is rendered as plain text, so nothing is
//! ever lost.
//!
//! [`wrap`] pre-wraps styled lines to a viewport width so the scroll math
//! in the TUI stays exact (ratatui's own wrap would happen inside the
//! widget, after the scroll offset is already chosen).

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar as _;

/// Render assistant text as styled lines (not yet wrapped).
pub fn render(text: &str, base: Style) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            lines.push(Line::from(Span::styled(raw.to_string(), dim(base))));
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
            continue;
        }
        lines.extend(render_block_line(raw, base));
    }
    lines
}

/// Word-wrap styled lines to `width` display columns, preserving styles.
/// Blank lines are preserved. Embedded newlines split the text into
/// separate source lines before wrapping (raw `\n` characters must never
/// reach the terminal buffer: the cursor moves down without returning to
/// column 0, producing a staircase of increasingly indented lines).
pub fn wrap(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    let width = width.max(1) as usize;
    let mut out = Vec::new();
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
    out
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
        let lines = render("## Title", Style::new());
        let spans = &lines[0].spans;
        assert_eq!(span_text(spans), "## Title");
        assert_eq!(spans[1].style.add_modifier, Modifier::BOLD);
    }

    #[test]
    fn code_fences_toggle() {
        let lines = render("```rust\nlet x = 1;\n```", Style::new());
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));
        let code = lines[1].spans.last().unwrap();
        assert_eq!(code.content, "let x = 1;");
        assert_eq!(code.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn bullets_color_the_marker() {
        let lines = render("- one\n2. two", Style::new());
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
        let lines = render("aaa `bbb` ccc", Style::new());
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
