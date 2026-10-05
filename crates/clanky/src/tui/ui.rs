//! Layout and drawing for the TUI (M3).
//!
//! The screen is three stacked regions: the scrollable transcript, a one-row
//! status line, and the input line. The transcript is pre-wrapped
//! (`App::transcript_lines`), so the scroll offset in rows is exact.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget as _};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use super::app::{App, CompletionState};
use super::picker::Picker;
use super::selection::{self, Selection};

/// Prompt symbol for the input line.
const INPUT_PROMPT: &str = "❯ ";

/// Per-frame display context that is not part of the mutable app state.
pub struct Status<'a> {
    pub provider: &'a str,
    pub model: Option<&'a str>,
    /// Display name of the current session file, when one exists.
    pub session: Option<&'a str>,
}

/// Per-frame transcript context: pre-wrapped lines, the clamped scroll
/// offset, and the active mouse selection.
pub struct Transcript<'a> {
    pub lines: Vec<Line<'static>>,
    pub scroll_from_top: usize,
    pub selection: Option<&'a Selection>,
}

/// Draw one frame. `view.lines` must be the app's transcript pre-wrapped to
/// the terminal width; `view.scroll_from_top` is the clamped scroll offset.
/// When `picker` is open it is drawn as a centered overlay on top; when Tab
/// completion is active (M7) its candidate popup is drawn above the input
/// line; an active mouse selection is highlighted in the transcript.
pub fn draw(
    frame: &mut Frame,
    app: &App,
    view: Transcript<'_>,
    status: &Status<'_>,
    completion: Option<&CompletionState>,
    picker: Option<&Picker>,
) {
    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    let (chat_area, status_area, input_area) = (layout[0], layout[1], layout[2]);

    let mut transcript = view.lines;
    if let Some(sel) = view.selection {
        apply_selection(&mut transcript, sel);
    }

    let scroll = view.scroll_from_top.min(u16::MAX as usize) as u16;
    Paragraph::new(transcript)
        .scroll((scroll, 0))
        .render(chat_area, frame.buffer_mut());

    draw_status(frame, status_area, app, status);
    draw_input(frame, input_area, app);
    if let Some(completion) = completion {
        draw_completion(frame, input_area, completion);
    }
    if let Some(picker) = picker {
        draw_picker(frame, area, picker);
    }
}

/// Highlight the transcript lines covered by the selection. Coordinates
/// are indices into the pre-wrapped transcript; the Paragraph only renders
/// the viewport, so out-of-view lines can be styled without harm.
fn apply_selection(transcript: &mut [Line<'static>], sel: &Selection) {
    let ((start_line, start_col), (end_line, end_col)) = sel.range();
    if start_line >= transcript.len() {
        return;
    }
    let end_line = end_line.min(transcript.len() - 1);
    for (index, line) in transcript
        .iter_mut()
        .enumerate()
        .take(end_line + 1)
        .skip(start_line)
    {
        let (start, end) = match (index == start_line, index == end_line) {
            (true, true) => (start_col, end_col),
            (true, false) => (start_col, usize::MAX),
            (false, true) => (0, end_col),
            (false, false) => (0, usize::MAX),
        };
        *line = selection::highlight(std::mem::take(line), start, end);
    }
}

/// Draw the picker as a centered bordered overlay.
fn draw_picker(frame: &mut Frame, area: Rect, picker: &Picker) {
    if area.width < 10 || area.height < 6 {
        return;
    }
    let width = area.width.clamp(20, 72);
    // Enough rows for query + a bounded list, never more than fits.
    let max_visible = area.height.saturating_sub(4).max(3) as usize;
    let lines = picker.lines(max_visible);
    let height = ((lines.len() + 2) as u16).min(area.height);
    let box_area = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    };
    Clear.render(box_area, frame.buffer_mut());
    let block = Block::bordered().title(Span::styled(
        format!(" {} ", picker.title()),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(box_area);
    block.render(box_area, frame.buffer_mut());
    Paragraph::new(lines).render(inner, frame.buffer_mut());
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App, status: &Status<'_>) {
    let base = Style::new().fg(Color::DarkGray);
    let mut left = vec![Span::styled(
        status.provider.to_string(),
        base.add_modifier(Modifier::BOLD),
    )];
    if let Some(model) = status.model {
        left.push(Span::styled(" · ", base));
        left.push(Span::styled(model.to_string(), base));
    }
    if let Some(session) = status.session {
        left.push(Span::styled(" · ", base));
        left.push(Span::styled(session.to_string(), base));
    }

    let mut right = Vec::new();
    if app.scroll_from_bottom > 0 {
        right.push(Span::styled("▲ ", Style::new().fg(Color::Yellow)));
    }
    if app.busy {
        right.push(Span::styled(
            "● streaming",
            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        ));
    } else if app.pending.is_some() {
        right.push(Span::styled("○ queued", Style::new().fg(Color::Yellow)));
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
        right.push(Span::styled("ctrl+c quit · pgup/pgdn scroll", base));
    }

    let width = area.width as usize;
    let left_width = spans_width(&left);
    let right_width = spans_width(&right);
    let mut spans = left;
    if right_width + left_width <= width {
        spans.push(Span::raw(" ".repeat(width - left_width - right_width)));
        spans.extend(right);
    } else {
        // Too tight for both: show only the status, truncated.
        spans.clear();
        spans.extend(truncate_spans(right, width));
    }
    frame.render_widget(ratatui::widgets::Paragraph::new(Line::from(spans)), area);
}

/// Completion candidate popup (M7): a small bordered box anchored to the
/// left edge of the input line, overlaying the transcript. The selected
/// candidate (while cycling) is highlighted.
fn draw_completion(frame: &mut Frame, input_area: Rect, completion: &CompletionState) {
    const MAX_VISIBLE: usize = 8;
    let candidates = completion.candidates();
    if candidates.is_empty() {
        return;
    }
    // Border + rows must fit between the top of the screen and the input.
    let visible = candidates
        .len()
        .min(MAX_VISIBLE)
        .min(input_area.y.saturating_sub(2) as usize);
    if visible == 0 {
        return;
    }
    let title = match completion.selected() {
        Some(i) => format!(" complete {}/{} ", i + 1, candidates.len()),
        None => " complete ".to_string(),
    };
    let width = candidates
        .iter()
        .map(|c| c.width())
        .max()
        .unwrap_or(0)
        .saturating_add(4)
        .max(title.width() + 2)
        .clamp(12, input_area.width as usize) as u16;
    let height = visible as u16 + 2;
    let box_area = Rect {
        x: input_area.x,
        y: input_area.y - height,
        width,
        height,
    };
    Clear.render(box_area, frame.buffer_mut());
    let block = Block::bordered().title(Span::styled(
        title,
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ));
    let inner = block.inner(box_area);
    block.render(box_area, frame.buffer_mut());

    // Sliding window around the selected candidate.
    let selected = completion.selected();
    let start = selected.map_or(0, |i| i.saturating_sub(visible.saturating_sub(1)));
    let lines: Vec<Line<'static>> = candidates[start..start + visible]
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
    Paragraph::new(lines).render(inner, frame.buffer_mut());
}

fn draw_input(frame: &mut Frame, area: ratatui::layout::Rect, app: &App) {
    let prompt_width = INPUT_PROMPT.width();
    let width = area.width as usize;
    if width <= prompt_width {
        return;
    }
    let view_width = width - prompt_width;

    let prompt_style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
    if app.input.is_empty() && !app.busy {
        let spans = vec![
            Span::styled(INPUT_PROMPT, prompt_style),
            Span::styled("type a message…", Style::new().fg(Color::DarkGray)),
        ];
        frame.render_widget(ratatui::widgets::Paragraph::new(Line::from(spans)), area);
        frame.set_cursor_position(Position {
            x: area.x + prompt_width as u16,
            y: area.y,
        });
        return;
    }

    let (window, caret_col) = input_window(&app.input, app.cursor, view_width);
    let mut spans = vec![Span::styled(INPUT_PROMPT, prompt_style)];
    spans.extend(window);
    frame.render_widget(ratatui::widgets::Paragraph::new(Line::from(spans)), area);
    frame.set_cursor_position(Position {
        x: area.x + prompt_width as u16 + caret_col as u16,
        y: area.y,
    });
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
    let mut spans: Vec<Span<'static>> = Vec::new();
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
    spans.push(Span::raw(window_text));
    (spans, caret_col - start_col)
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
    use super::CompletionState;
    use super::*;
    use crate::tui::app::Entry;
    use clanky_protocol::Usage;
    use ratatui::{Terminal, backend::TestBackend};

    fn draw_app(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let transcript = app.transcript_lines(width);
        terminal
            .draw(|f| {
                draw(
                    f,
                    app,
                    Transcript {
                        lines: transcript,
                        scroll_from_top: 0,
                        selection: app.selection.as_ref(),
                    },
                    &Status {
                        provider: "deepinfra",
                        model: Some("mock/model"),
                        session: None,
                    },
                    app.completion.as_ref(),
                    None,
                )
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn screen_text(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> String {
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn draws_chat_status_and_input() {
        let mut app = App::new();
        app.entries.push(Entry::User("hello".into()));
        app.entries.push(Entry::Assistant("world".into()));
        app.input = "next up".into();
        app.cursor = 7;

        let buffer = draw_app(&app, 60, 6);
        let screen = screen_text(&buffer, 60, 6);
        assert!(screen.contains("hello"), "{screen}");
        assert!(screen.contains("world"), "{screen}");
        assert!(screen.contains("❯ next up"), "{screen}");
        assert!(screen.contains("deepinfra · mock/model"), "{screen}");
        assert!(screen.contains("ctrl+c quit"), "{screen}");
    }

    #[test]
    fn busy_state_is_shown_and_scrolled_flag() {
        let mut app = App::new();
        app.busy = true;
        app.scroll_from_bottom = 2;
        let buffer = draw_app(&app, 60, 4);
        let screen = screen_text(&buffer, 60, 4);
        assert!(screen.contains("streaming"), "{screen}");
        assert!(screen.contains("▲"), "{screen}");
    }

    #[test]
    fn queued_state_is_shown() {
        let mut app = App::new();
        app.pending = Some("queued prompt".into());
        let buffer = draw_app(&app, 60, 4);
        assert!(screen_text(&buffer, 60, 4).contains("queued"));
    }

    #[test]
    fn error_entries_render_in_the_chat_area() {
        let mut app = App::new();
        app.entries.push(Entry::Error("no model configured".into()));
        let buffer = draw_app(&app, 60, 4);
        assert!(screen_text(&buffer, 60, 4).contains("✗ no model configured"));
    }

    #[test]
    fn long_input_keeps_the_caret_visible() {
        let mut app = App::new();
        app.input = "x".repeat(100);
        app.cursor = 100;
        let (spans, caret) = input_window(&app.input, app.cursor, 50);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text.chars().count(), 49, "caret sits at the right edge");
        assert_eq!(caret, 49);
    }

    #[test]
    fn short_input_shows_everything() {
        let (spans, caret) = input_window("hi", 2, 50);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "hi");
        assert_eq!(caret, 2);
    }

    #[test]
    fn status_truncates_when_too_narrow() {
        let app = App::new();
        let mut terminal = Terminal::new(TestBackend::new(10, 4)).unwrap();
        terminal
            .draw(|f| {
                draw(
                    f,
                    &app,
                    Transcript {
                        lines: Vec::new(),
                        scroll_from_top: 0,
                        selection: None,
                    },
                    &Status {
                        provider: "an-very-long-provider-name",
                        model: Some("model"),
                        session: None,
                    },
                    None,
                    None,
                )
            })
            .unwrap();
        // Must not panic; content is truncated.
    }

    #[test]
    fn usage_appears_when_available() {
        let mut app = App::new();
        app.last_usage = Some(Usage {
            prompt_tokens: Some(1523),
            completion_tokens: Some(87),
        });
        let buffer = draw_app(&app, 60, 4);
        assert!(screen_text(&buffer, 60, 4).contains("↑1523 ↓87 tok"));
    }

    #[test]
    fn session_name_shown_in_status() {
        let app = App::new();
        let buffer = draw_app(&app, 60, 4);
        assert!(!screen_text(&buffer, 60, 4).contains("my-session"));
        // With a session name it appears next to provider and model.
        let mut terminal = Terminal::new(TestBackend::new(90, 4)).unwrap();
        terminal
            .draw(|f| {
                draw(
                    f,
                    &app,
                    Transcript {
                        lines: Vec::new(),
                        scroll_from_top: 0,
                        selection: None,
                    },
                    &Status {
                        provider: "deepinfra",
                        model: Some("mock/model"),
                        session: Some("my-session"),
                    },
                    None,
                    None,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert!(screen_text(&buffer, 90, 4).contains("my-session"));
    }

    #[test]
    fn picker_overlays_the_chat() {
        let mut app = App::new();
        app.entries.push(Entry::User("underneath".into()));
        let picker = Picker::new(
            "resume",
            vec![super::super::picker::PickerItem {
                label: "session-a".into(),
                detail: "detail".into(),
            }],
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        terminal
            .draw(|f| {
                draw(
                    f,
                    &app,
                    Transcript {
                        lines: app.transcript_lines(60),
                        scroll_from_top: 0,
                        selection: None,
                    },
                    &Status {
                        provider: "deepinfra",
                        model: None,
                        session: None,
                    },
                    None,
                    Some(&picker),
                )
            })
            .unwrap();
        let screen = screen_text(terminal.backend().buffer(), 60, 10);
        assert!(screen.contains("session-a"), "{screen}");
        assert!(screen.contains("search:"), "{screen}");
    }

    #[test]
    fn completion_popup_renders_above_the_input() {
        let mut app = App::new();
        app.input = "cat alpha".into();
        app.cursor = app.input.len();
        app.completion = Some(CompletionState::new(
            "cat ".into(),
            String::new(),
            vec!["alpha.md ".into(), "alpha.txt ".into()],
        ));

        let buffer = draw_app(&app, 60, 10);
        let screen = screen_text(&buffer, 60, 10);
        assert!(screen.contains("complete"), "{screen}");
        assert!(screen.contains("alpha.md"), "{screen}");
        assert!(screen.contains("alpha.txt"), "{screen}");
        assert!(screen.contains("❯ cat alpha"), "{screen}");
    }

    #[test]
    fn completion_popup_marks_the_selected_candidate() {
        let mut app = App::new();
        app.completion = Some(CompletionState::new(
            String::new(),
            String::new(),
            vec!["one ".into(), "two ".into(), "three ".into()],
        ));
        // Simulate cycling to the second candidate.
        {
            let state = app.completion.as_mut().unwrap();
            state.advance();
            state.advance();
        }
        let buffer = draw_app(&app, 60, 10);
        let screen = screen_text(&buffer, 60, 10);
        assert!(screen.contains("complete 2/3"), "{screen}");
        assert!(screen.contains("▸ two"), "{screen}");
        assert!(!screen.contains("▸ one"), "{screen}");
    }

    #[test]
    fn completion_popup_skips_when_the_terminal_is_tiny() {
        let mut app = App::new();
        app.completion = Some(CompletionState::new(
            String::new(),
            String::new(),
            vec!["a ".into(), "b ".into()],
        ));
        // 3 rows: chat 1, status 1, input 1 — no room above the input.
        let buffer = draw_app(&app, 40, 3);
        let screen = screen_text(&buffer, 40, 3);
        assert!(!screen.contains("complete"), "{screen}");
    }

    #[test]
    fn selection_is_highlighted_in_the_transcript() {
        let mut app = App::new();
        app.push_user("hello world");
        app.selection_start(0, 8);
        app.selection_extend(0, 13);

        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
        terminal
            .draw(|f| {
                draw(
                    f,
                    &app,
                    Transcript {
                        lines: app.transcript_lines(40),
                        scroll_from_top: 0,
                        selection: app.selection.as_ref(),
                    },
                    &Status {
                        provider: "deepinfra",
                        model: None,
                        session: None,
                    },
                    None,
                    None,
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        // `world` sits at row 0, columns 8..13: reversed, `hello` is not.
        assert!(
            buffer[(10, 0)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            !buffer[(3, 0)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }
}
