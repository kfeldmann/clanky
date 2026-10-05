//! Linear screen output: the transcript is printed straight to the
//! terminal instead of living inside a redrawn viewport.
//!
//! Printed lines flow into the terminal's normal buffer and scrollback, so
//! the native scrollbar, native text selection, and the history left
//! behind after quitting all work without being emulated. No alternate
//! screen is entered and the mouse is never captured.
//!
//! Only a small region at the bottom of the screen is rewritten in place:
//! the entry currently streaming into (if any), an optional picker or
//! Tab-completion popup, the status line, and the input line. Everything
//! above that region is final: once printed it is never touched again.
//!
//! The redraw is cursor-relative. After every render the cursor sits on
//! the input row; moving up `tail_height` rows reaches the top of the
//! redrawable region, so no absolute positions are tracked. Finalized
//! transcript lines are printed once above the tail and never rewritten
//! (`App::printed_*` bookkeeping); a streaming entry that outgrows the
//! screen has its oldest rows committed the same way, keeping the tail
//! within one screen. A resize reflows the buffer and loses the anchor,
//! so the region is re-anchored at the bottom edge and the streaming
//! entry is printed once more in the new width.

use std::io::{self, Write};

use ratatui::crossterm::cursor::{MoveTo, MoveToColumn, MoveToNextLine, MoveUp};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{Clear, ClearType};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use super::app::{App, CompletionState};
use super::picker::Picker;

/// Prompt symbol for the input line.
const INPUT_PROMPT: &str = "❯ ";

/// Per-render display context that is not part of the mutable app state.
pub struct Status<'a> {
    pub provider: &'a str,
    pub model: Option<&'a str>,
    /// Display name of the current session file, when one exists.
    pub session: Option<&'a str>,
}

/// The terminal, viewed as a linear transcript with a redrawable tail.
pub struct Screen<W: Write> {
    out: W,
    width: u16,
    height: u16,
    /// Rows printed by the last render that the next render rewrites
    /// (streaming tail + popup + status + input).
    tail_height: usize,
}

impl<W: Write> Screen<W> {
    /// Take ownership of a writer and anchor the region at the bottom row.
    pub fn new(out: W, width: u16, height: u16) -> Self {
        let mut this = Self {
            out,
            width,
            height,
            tail_height: 0,
        };
        this.reanchor();
        this
    }

    /// Current terminal dimensions as last seen by the renderer.
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Move to the bottom row (clamped: cursor down never scrolls) and the
    /// first column. This is the anchor every region draw starts from.
    fn reanchor(&mut self) {
        let _ = execute!(self.out, MoveToNextLine(u16::MAX), MoveToColumn(0));
    }

    /// Re-anchor after a resize (the buffer was reflowed, the anchor is
    /// lost): the tail is redrawn from scratch and the streaming entry is
    /// reprinted in full at the new width.
    pub fn resync(&mut self, app: &mut App, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.tail_height = 0;
        let sealed = app.sealed_boundary();
        if sealed < app.entries.len() {
            app.set_printed(sealed, 0);
        }
        self.reanchor();
    }

    /// Erase the visible screen (Ctrl+L). Scrollback is preserved; the
    /// caller clears the app's entries and print bookkeeping itself.
    pub fn clear_screen(&mut self) {
        let _ = execute!(self.out, Clear(ClearType::All), MoveTo(0, 0));
        self.tail_height = 0;
        self.reanchor();
    }

    /// Finish: a fresh line below the transcript so the shell prompt does
    /// not print onto the input line. The printed history stays.
    pub fn finish(&mut self) -> io::Result<()> {
        self.out.write_all(b"\r\n")?;
        self.out.flush()
    }

    /// Print what changed since the last call: newly finalized transcript
    /// lines flow permanently into the terminal; the tail (streaming
    /// entry + popup + status + input) is rewritten in place.
    pub fn render(
        &mut self,
        app: &mut App,
        status: &Status<'_>,
        picker: Option<&Picker>,
        completion: Option<&CompletionState>,
    ) -> io::Result<()> {
        let width = self.width;
        let height = self.height;

        // The footer's height caps how many rows of the streaming entry
        // can stay in the redrawable tail.
        let footer = footer_lines(app, status, picker, completion, width, height);

        // Commit finalized entries that are not printed yet.
        let sealed = app.sealed_boundary();
        let mut commit: Vec<Line<'static>> = Vec::new();
        let mut index = app.printed_entries;
        while index < sealed {
            let lines = app.entry_lines(index, width);
            let start = app.printed(index).min(lines.len());
            app.set_printed(index, lines.len());
            commit.extend(lines.into_iter().skip(start));
            index += 1;
        }
        app.printed_entries = sealed;

        // The streaming entry: rows beyond the screen budget are committed
        // permanently; the newest rows stay in the tail and are rewritten
        // as more text arrives.
        let mut tail: Vec<Line<'static>> = Vec::new();
        if sealed < app.entries.len() {
            let lines = app.entry_lines(sealed, width);
            let keep = (height as usize)
                .saturating_sub(footer.lines.len())
                .saturating_sub(1)
                .max(1);
            let committed = app.printed(sealed);
            let overflow_end = lines.len().saturating_sub(keep);
            if committed < overflow_end {
                commit.extend(lines[committed..overflow_end].iter().cloned());
                app.set_printed(sealed, overflow_end);
            }
            let start = app.printed(sealed).min(lines.len());
            tail.extend(lines.into_iter().skip(start));
        }
        tail.extend(footer.lines);

        // Move to the top of the previous tail, print the new rows (the
        // final row without a trailing newline: the cursor stays on it),
        // and erase whatever is left below when the tail shrank.
        if self.tail_height > 0 {
            execute!(self.out, MoveUp(self.tail_height as u16))?;
        }
        let shrink = commit.len() + tail.len() < self.tail_height;
        let mut first = true;
        for line in commit.iter().chain(tail.iter()) {
            if !first {
                self.out.write_all(b"\r\n")?;
            }
            first = false;
            print_line(&mut self.out, line, width)?;
        }
        if shrink {
            execute!(self.out, Clear(ClearType::FromCursorDown))?;
        }
        self.tail_height = tail.len();
        execute!(self.out, MoveToColumn(footer.caret))?;
        self.out.flush()
    }
}

/// Print one styled line as ANSI, truncated to `width` display columns
/// (truncation keeps the row-count math exact: a wrapped row would shift
/// everything below).
fn print_line<W: Write>(out: &mut W, line: &Line<'_>, width: u16) -> io::Result<()> {
    let width = width as usize;
    out.write_all(b"\x1b[0m")?;
    let mut col = 0usize;
    for span in &line.spans {
        out.write_all(sgr(span.style).as_bytes())?;
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if col + w > width {
                break;
            }
            let mut buf = [0u8; 4];
            out.write_all(c.encode_utf8(&mut buf).as_bytes())?;
            col += w;
        }
    }
    out.write_all(b"\x1b[0m")
}

/// ANSI SGR sequence for a ratatui style (the colors and modifiers the
/// TUI actually uses; anything else falls back to the default).
fn sgr(style: Style) -> String {
    let mut codes: Vec<&str> = Vec::new();
    let fg = match style.fg {
        Some(Color::Black) => Some("30"),
        Some(Color::Red) => Some("31"),
        Some(Color::Green) => Some("32"),
        Some(Color::Yellow) => Some("33"),
        Some(Color::Blue) => Some("34"),
        Some(Color::Magenta) => Some("35"),
        Some(Color::Cyan) => Some("36"),
        Some(Color::Gray) => Some("37"),
        Some(Color::DarkGray) => Some("90"),
        Some(Color::LightRed) => Some("91"),
        Some(Color::LightGreen) => Some("92"),
        Some(Color::LightYellow) => Some("93"),
        Some(Color::LightBlue) => Some("94"),
        Some(Color::LightMagenta) => Some("95"),
        Some(Color::LightCyan) => Some("96"),
        Some(Color::White) => Some("97"),
        _ => None,
    };
    if let Some(code) = fg {
        codes.push(code);
    }
    let m = style.add_modifier;
    if m.contains(Modifier::BOLD) {
        codes.push("1");
    }
    if m.contains(Modifier::DIM) {
        codes.push("2");
    }
    if m.contains(Modifier::ITALIC) {
        codes.push("3");
    }
    if m.contains(Modifier::UNDERLINED) {
        codes.push("4");
    }
    if m.contains(Modifier::REVERSED) {
        codes.push("7");
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// The redrawable footer: an optional popup, the status line, and the
/// input line — plus the caret column within the last row.
struct Footer {
    lines: Vec<Line<'static>>,
    caret: u16,
}

fn footer_lines(
    app: &App,
    status: &Status<'_>,
    picker: Option<&Picker>,
    completion: Option<&CompletionState>,
    width: u16,
    height: u16,
) -> Footer {
    let mut lines = Vec::new();
    if let Some(picker) = picker {
        lines.extend(picker_box(picker, width, height));
    } else if let Some(completion) = completion {
        lines.extend(completion_box(completion, width, height));
    }
    lines.push(status_line(app, status, width));
    let (input, caret) = input_row(app, width);
    lines.push(input);
    Footer { lines, caret }
}

/// Status line: provider · model · session on the left; streaming state,
/// token usage, or a hint on the right.
fn status_line(app: &App, status: &Status<'_>, width: u16) -> Line<'static> {
    let base = Style::new().fg(Color::DarkGray);
    let mut left = vec![Span::styled(
        status.provider.to_string(),
        base.add_modifier(Modifier::BOLD),
    )];
    if let Some(model) = status.model {
        left.push(Span::styled(" · ".to_string(), base));
        left.push(Span::styled(model.to_string(), base));
    }
    if let Some(session) = status.session {
        left.push(Span::styled(" · ".to_string(), base));
        left.push(Span::styled(session.to_string(), base));
    }

    let mut right = Vec::new();
    if app.busy {
        right.push(Span::styled(
            "● streaming".to_string(),
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ));
    } else if app.pending.is_some() {
        right.push(Span::styled(
            "○ queued".to_string(),
            Style::new().fg(Color::Yellow),
        ));
    } else if let Some(usage) = app.last_usage {
        right.push(Span::styled(
            format!(
                "↑{} ↓{} tok",
                usage.prompt_tokens.unwrap_or(0),
                usage.completion_tokens.unwrap_or(0)
            ),
            base,
        ));
    } else {
        right.push(Span::styled("/ commands · ctrl+c quit".to_string(), base));
    }

    let width = width as usize;
    let left_width = spans_width(&left);
    let right_width = spans_width(&right);
    if right_width + left_width <= width {
        left.push(Span::raw(" ".repeat(width - left_width - right_width)));
        left.extend(right);
        Line::from(left)
    } else {
        // Too tight for both: show only the status, truncated.
        Line::from(truncate_spans(right, width))
    }
}

/// The input line and the caret column within it. The text is windowed
/// horizontally so the caret stays on screen.
fn input_row(app: &App, width: u16) -> (Line<'static>, u16) {
    let prompt_width = INPUT_PROMPT.width();
    let view_width = (width as usize).saturating_sub(prompt_width);
    if view_width == 0 {
        return (Line::default(), 0);
    }
    let prompt_style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
    if app.input.is_empty() && !app.busy {
        return (
            Line::from(vec![
                Span::styled(INPUT_PROMPT.to_string(), prompt_style),
                Span::styled(
                    "type a message…".to_string(),
                    Style::new().fg(Color::DarkGray),
                ),
            ]),
            prompt_width as u16,
        );
    }
    let (window, caret_col) = input_window(&app.input, app.cursor, view_width);
    let mut spans = vec![Span::styled(INPUT_PROMPT.to_string(), prompt_style)];
    spans.extend(window);
    (Line::from(spans), (prompt_width + caret_col) as u16)
}

/// The visible slice of the input line so the caret stays on screen.
/// Returns the styled spans and the caret column within the window.
fn input_window(input: &str, cursor: usize, view_width: usize) -> (Vec<Span<'static>>, usize) {
    // Display columns of every char and of the caret.
    let mut cols = Vec::new();
    let mut col = 0usize;
    let mut caret_col = 0usize;
    for (i, c) in input.char_indices() {
        if i == cursor {
            caret_col = col;
        }
        cols.push((col, c));
        col += c.width().unwrap_or(0);
    }
    if cursor >= input.len() {
        caret_col = col;
    }

    if col <= view_width {
        return (vec![Span::raw(input.to_string())], caret_col);
    }

    // Keep the caret visible with a little slack on the right.
    let start_col = caret_col.saturating_sub(view_width.saturating_sub(1));
    let mut window_text = String::new();
    for &(char_col, c) in &cols {
        if char_col < start_col {
            continue;
        }
        if char_col - start_col >= view_width {
            break;
        }
        window_text.push(c);
    }
    (vec![Span::raw(window_text)], caret_col - start_col)
}

/// The picker, drawn as a bordered box that sits above the status line in
/// the redrawable region (it overlays nothing: history above stays put).
fn picker_box(picker: &Picker, width: u16, height: u16) -> Vec<Line<'static>> {
    const MAX_VISIBLE: usize = 10;
    if width < 24 || height < 8 {
        return Vec::new(); // not enough room
    }
    let visible = MAX_VISIBLE.min((height as usize).saturating_sub(6)).max(1);
    let title = format!(" {} ", picker.title());
    boxed(&title, picker.lines(visible), width)
}

/// The Tab-completion candidates, drawn like a small picker box.
fn completion_box(completion: &CompletionState, width: u16, height: u16) -> Vec<Line<'static>> {
    const MAX_VISIBLE: usize = 8;
    let candidates = completion.candidates();
    if candidates.is_empty() || width < 20 || height < 6 {
        return Vec::new();
    }
    let visible = candidates
        .len()
        .min(MAX_VISIBLE)
        .min((height as usize).saturating_sub(4))
        .max(1);
    let selected = completion.selected();
    let start = selected.map_or(0, |i| i.saturating_sub(visible.saturating_sub(1)));
    let rows: Vec<Line<'static>> = candidates[start..start + visible]
        .iter()
        .enumerate()
        .map(|(row, text)| {
            let is_selected = selected == Some(start + row);
            let style = if is_selected {
                Style::new()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::new()
            };
            Line::from(vec![
                Span::styled(format!(" {} ", if is_selected { '▸' } else { ' ' }), style),
                Span::styled(text.clone(), style),
            ])
        })
        .collect();
    let title = match selected {
        Some(i) => format!(" complete {}/{} ", i + 1, candidates.len()),
        None => " complete ".to_string(),
    };
    boxed(&title, rows, width)
}

/// Wrap `rows` in a rounded box with `title` in the top border. The box
/// is as wide as the widest row (plus padding), never wider than the
/// screen; rows are truncated and padded to the box width.
fn boxed(title: &str, rows: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let border = Style::new().fg(Color::DarkGray);
    let title_style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);

    let content_width = rows.iter().map(|l| line_width(l)).max().unwrap_or(0);
    let box_width = content_width
        .max(title.width())
        .saturating_add(4) // "│ " + " │"
        .min(width.saturating_sub(2) as usize)
        .max(title.width() + 6)
        .max(8);
    let inner_width = box_width - 4;

    let mut lines = Vec::with_capacity(rows.len() + 2);
    // ╭─ title ─────╮
    let dashes = box_width.saturating_sub(title.width() + 5);
    lines.push(Line::from(vec![
        Span::styled("╭─ ".to_string(), border),
        Span::styled(title.to_string(), title_style),
        Span::styled(format!("─{}╮", "─".repeat(dashes)), border),
    ]));
    for row in rows {
        let inner = truncate_line(row, inner_width);
        let pad = inner_width.saturating_sub(line_width(&inner));
        let mut spans = vec![Span::styled("│ ".to_string(), border)];
        spans.extend(inner.spans);
        spans.push(Span::styled(format!("{} │", " ".repeat(pad)), border));
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(box_width - 2)),
        border,
    )));
    lines
}

/// Total display width of a line's spans.
fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|s| s.width()).sum()
}

/// Cut a line to `max` display columns, keeping earlier spans whole.
fn truncate_line(line: Line<'static>, max: usize) -> Line<'static> {
    if line_width(&line) <= max {
        return line;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut remaining = max;
    for span in line.spans {
        if remaining == 0 {
            break;
        }
        let mut text = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if w > remaining {
                break;
            }
            text.push(c);
            remaining -= w;
        }
        if !text.is_empty() {
            out.push(Span::styled(text, span.style));
        }
        if remaining == 0 {
            break;
        }
    }
    Line::from(out)
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.width()).sum()
}

/// Cut spans to `width` display columns, keeping earlier spans whole.
fn truncate_spans(spans: Vec<Span<'_>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut remaining = width;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let w = span.width();
        if w <= remaining {
            remaining -= w;
            out.push(Span::raw(span.content.to_string()));
        } else {
            let mut text = String::new();
            for c in span.content.chars() {
                let cw = c.width().unwrap_or(0);
                if cw > remaining {
                    break;
                }
                text.push(c);
                remaining -= cw;
            }
            out.push(Span::raw(text));
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::TurnEvent;

    /// A writer that records the plain text rows printed by a screen,
    /// keeping escape sequences out of the assertions.
    #[derive(Default)]
    struct Buf(Vec<u8>);

    impl Write for Buf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Buf {
        /// Plain text as printed, escape codes stripped, `\r\n` → `\n`.
        fn text(&self) -> String {
            let mut out = String::new();
            let mut chars = std::str::from_utf8(&self.0).unwrap().chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else if c == '\r' {
                    // drop
                } else {
                    out.push(c);
                }
            }
            out
        }

        fn count(&self, needle: &str) -> usize {
            self.text().matches(needle).count()
        }
    }

    fn screen(buf: Buf, width: u16, height: u16) -> Screen<Buf> {
        Screen::new(buf, width, height)
    }

    impl<W: Write + Default> Screen<W> {
        /// Take the output written since the last call (the writer is
        /// replaced with a fresh one).
        fn take_out(&mut self) -> W {
            std::mem::take(&mut self.out)
        }
    }

    fn status() -> Status<'static> {
        Status {
            provider: "deepinfra",
            model: Some("mock/model"),
            session: None,
        }
    }

    #[test]
    fn committed_rows_are_printed_once() {
        let buf = Buf::default();
        let mut app = App::new();
        app.push_user("hello");
        let mut s = screen(buf, 60, 10);
        s.render(&mut app, &status(), None, None).unwrap();
        assert_eq!(s.take_out().count("❯ hello"), 1);

        // A re-render with no new entries must not reprint the transcript.
        s.render(&mut app, &status(), None, None).unwrap();
        assert_eq!(s.take_out().count("❯ hello"), 0);
    }

    #[test]
    fn streaming_tail_is_rewritten_and_then_committed() {
        let buf = Buf::default();
        let mut app = App::new();
        let mut s = screen(buf, 60, 10);
        app.on_turn_event(TurnEvent::Text {
            delta: "alpha".into(),
        });
        s.render(&mut app, &status(), None, None).unwrap();
        assert_eq!(s.take_out().count("alpha"), 1);

        app.on_turn_event(TurnEvent::Text {
            delta: " beta".into(),
        });
        s.render(&mut app, &status(), None, None).unwrap();
        let text = s.take_out().text();
        assert_eq!(text.matches("alpha").count(), 1, "tail is rewritten");
        assert_eq!(text.matches("beta").count(), 1);

        // Once the entry is finalized it is committed and stays put.
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: "{}".into(),
        });
        s.render(&mut app, &status(), None, None).unwrap();
        let text = s.take_out().text();
        assert_eq!(text.matches("alpha").count(), 1, "final entry reprinted");
        assert!(text.contains("● bash"));
        s.render(&mut app, &status(), None, None).unwrap();
        let text = s.take_out().text();
        assert_eq!(text.matches("alpha").count(), 0, "then left alone");
        assert_eq!(text.matches("● bash").count(), 0);
    }

    #[test]
    fn a_long_streaming_entry_commits_its_oldest_rows() {
        let buf = Buf::default();
        let mut app = App::new();
        let mut s = screen(buf, 60, 5); // keep = 5 - 2 - 1 = 2 tail rows
        for n in 1..=6 {
            app.on_turn_event(TurnEvent::Text {
                delta: format!("line-{n}\n"),
            });
            s.render(&mut app, &status(), None, None).unwrap();
        }
        let out = s.take_out().text();
        // Early lines were committed permanently, newest stay in the tail.
        assert_eq!(out.matches("line-1").count(), 1);
        assert_eq!(out.matches("line-6").count(), 1);
    }

    #[test]
    fn styled_text_is_printed_with_ansi_and_reset() {
        let mut buf = Buf::default();
        print_line(
            &mut buf,
            &Line::from(Span::styled(
                "hi".to_string(),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            80,
        )
        .unwrap();
        assert_eq!(buf.0, b"\x1b[0m\x1b[31;1mhi\x1b[0m");
    }

    #[test]
    fn print_line_truncates_to_the_row_width() {
        let mut buf = Buf::default();
        print_line(
            &mut buf,
            &Line::from(Span::raw("0123456789".to_string())),
            5,
        )
        .unwrap();
        assert!(std::str::from_utf8(&buf.0).unwrap().contains("01234"));
        assert!(!std::str::from_utf8(&buf.0).unwrap().contains("56789"));
    }

    #[test]
    fn status_line_pads_and_shows_state() {
        let mut app = App::new();
        app.busy = true;
        let line = status_line(&app, &status(), 40);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("deepinfra · mock/model"), "{text}");
        assert!(text.contains("● streaming"), "{text}");
        assert_eq!(text.chars().count(), 40, "padded to the full width");

        app.busy = false;
        app.pending = Some("x".into());
        let line = status_line(&app, &status(), 40);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("○ queued"), "{text}");
    }

    #[test]
    fn input_row_shows_placeholder_and_windows_long_input() {
        let mut app = App::new();
        let (line, caret) = input_row(&app, 40);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("type a message…"), "{text}");
        assert_eq!(caret, 2, "caret after the prompt symbol");

        app.input = "hi".into();
        app.cursor = 2;
        let (line, caret) = input_row(&app, 40);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("❯ hi"), "{text}");
        assert_eq!(caret, 4);

        // Long input: the caret stays visible at the right edge.
        app.input = "x".repeat(100);
        app.cursor = 100;
        let (_, caret) = input_row(&app, 40);
        assert!(caret > 0);
    }

    #[test]
    fn picker_box_rounds_the_rows() {
        let picker = Picker::new(
            "resume",
            vec![super::super::picker::PickerItem {
                label: "session-a".into(),
                detail: "3 turns".into(),
            }],
        );
        let rows = picker_box(&picker, 60, 20);
        assert_eq!(rows.len(), 4, "border, search, item, border");
        let texts: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert!(texts[0].contains("resume"), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("session-a")));
        assert!(texts[0].starts_with('╭') && texts[0].ends_with('╮'));
        assert!(texts[3].starts_with('╰') && texts[3].ends_with('╯'));
        // Every row is the same display width.
        let widths: Vec<usize> = rows.iter().map(line_width).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
    }

    #[test]
    fn picker_box_skips_when_the_terminal_is_tiny() {
        let picker = Picker::new("resume", vec![]);
        assert!(picker_box(&picker, 20, 20).is_empty());
        assert!(picker_box(&picker, 60, 6).is_empty());
    }

    #[test]
    fn completion_box_lists_candidates() {
        let mut app = App::new();
        app.completion = Some(CompletionState::new(
            String::new(),
            String::new(),
            vec!["one ".into(), "two ".into()],
        ));
        let rows = completion_box(app.completion.as_ref().unwrap(), 60, 20);
        let texts: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert!(texts.iter().any(|t| t.contains("one")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("two")));
        assert!(texts.iter().any(|t| t.contains("complete")));
    }
}
