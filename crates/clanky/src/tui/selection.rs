//! Mouse text selection: drag over the transcript to select text; on
//! release the selected text is copied with an OSC 52 escape sequence,
//! which works in most terminals (including over ssh) without external
//! tools. While the drag is in progress the selection is highlighted.
//!
//! Coordinates are transcript coordinates: (line index into the
//! pre-wrapped transcript lines, display column within that line). The
//! chat area starts at screen row 0 and is left-aligned at column 0, so
//! the mapping is `line = scroll_from_top + screen row`,
//! `column = screen column`.

use ratatui::style::Modifier;
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar as _;

/// A selection between two anchored points, in any order.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    anchor: (usize, usize),
    end: (usize, usize),
}

impl Selection {
    pub fn new(line: usize, col: usize) -> Self {
        Self {
            anchor: (line, col),
            end: (line, col),
        }
    }

    /// Move the free end of the selection.
    pub fn extend(&mut self, line: usize, col: usize) {
        self.end = (line, col);
    }

    /// The ordered range: the lexicographically smaller point first.
    pub fn range(&self) -> ((usize, usize), (usize, usize)) {
        let (a, b) = (self.anchor, self.end);
        if a <= b { (a, b) } else { (b, a) }
    }
}

/// Re-style one line with the characters in the column range highlighted.
/// Characters partially covered by the boundary (double-width glyphs)
/// count as selected when they overlap the range.
pub fn highlight(line: Line<'static>, start: usize, end: usize) -> Line<'static> {
    let mut out: Vec<ratatui::text::Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in line.spans {
        let mut plain = String::new();
        let mut marked = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if col + w > start && col < end {
                marked.push(c);
            } else {
                plain.push(c);
            }
            col += w;
        }
        if !plain.is_empty() {
            out.push(ratatui::text::Span::styled(plain, span.style));
        }
        if !marked.is_empty() {
            out.push(ratatui::text::Span::styled(
                marked,
                span.style.add_modifier(Modifier::REVERSED),
            ));
        }
    }
    Line::from(out)
}

/// The text of one line within a column range. Zero columns (`start ==
/// end`) yield nothing; `end` past the line end takes the rest.
pub fn text_in_columns(line: &Line<'static>, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for span in &line.spans {
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if col + w > start && col < end {
                out.push(c);
            }
            col += w;
        }
    }
    out
}

/// Minimal base64 (RFC 4648, with padding) for OSC 52 payloads.
pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// The OSC 52 escape sequence that puts `text` on the clipboard.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    fn plain_line(text: &str) -> Line<'static> {
        Line::from(vec![Span::raw(text.to_string())])
    }

    #[test]
    fn range_orders_the_endpoints() {
        let mut sel = Selection::new(5, 3);
        sel.extend(2, 1);
        assert_eq!(sel.range(), ((2, 1), (5, 3)));
        sel.extend(5, 9);
        assert_eq!(sel.range(), ((5, 3), (5, 9)));
    }

    #[test]
    fn highlight_reverses_the_selected_columns() {
        let line = highlight(plain_line("hello world"), 6, 11);
        let texts: Vec<String> = line.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(texts.join(""), "hello world");
        assert_eq!(texts.first().unwrap(), "hello ");
        assert_eq!(texts.get(1).unwrap(), "world");
        assert!(
            line.spans
                .get(1)
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn empty_range_highlights_nothing() {
        let line = highlight(plain_line("abc"), 2, 2);
        assert_eq!(line.spans.len(), 1);
        assert!(
            !line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn text_extraction_follows_display_columns() {
        let line = plain_line("hello world");
        assert_eq!(text_in_columns(&line, 0, 5), "hello");
        assert_eq!(text_in_columns(&line, 6, 11), "world");
        assert_eq!(text_in_columns(&line, 0, 100), "hello world");
        assert_eq!(text_in_columns(&line, 3, 3), "");
    }

    #[test]
    fn double_width_chars_are_taken_whole() {
        let line = plain_line("a中b");
        // 中 occupies columns 1..3; selecting columns 2..4 still yields it.
        assert_eq!(text_in_columns(&line, 2, 4), "中b");
        assert_eq!(text_in_columns(&line, 0, 2), "a中");
    }

    #[test]
    fn base64_encodes_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn osc52_wraps_base64_in_the_escape_sequence() {
        assert_eq!(osc52("foo"), "\x1b]52;c;Zm9v\x07");
    }

    #[test]
    fn osc52_handles_multibyte_text() {
        // The payload must round-trip: decode it back through base64.
        let seq = osc52("hé");
        let payload = seq
            .strip_prefix("\x1b]52;c;")
            .and_then(|s| s.strip_suffix('\x07'))
            .unwrap();
        assert_eq!(base64("hé".as_bytes()), payload);
    }
}
