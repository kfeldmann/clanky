//! Layout and drawing for the TUI (M3).
//!
//! The screen is three stacked regions: the scrollable transcript, a one-row
//! status line, and the input line. The transcript is pre-wrapped
//! (`App::transcript_lines`), so the scroll offset in rows is exact.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget as _};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use super::app::App;

/// Prompt symbol for the input line.
const INPUT_PROMPT: &str = "❯ ";

/// Per-frame display context that is not part of the mutable app state.
pub struct Status<'a> {
    pub provider: &'a str,
    pub model: Option<&'a str>,
}

/// Draw one frame. `transcript` must be the app's transcript pre-wrapped to
/// the terminal width; `scroll_from_top` is the clamped scroll offset.
pub fn draw(
    frame: &mut Frame,
    app: &App,
    transcript: Vec<Line<'static>>,
    scroll_from_top: usize,
    status: &Status<'_>,
) {
    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    let (chat_area, status_area, input_area) = (layout[0], layout[1], layout[2]);

    let scroll = scroll_from_top.min(u16::MAX as usize) as u16;
    Paragraph::new(transcript)
        .scroll((scroll, 0))
        .render(chat_area, frame.buffer_mut());

    draw_status(frame, status_area, app, status);
    draw_input(frame, input_area, app);
}

fn draw_status(frame: &mut Frame, area: ratatui::layout::Rect, app: &App, status: &Status<'_>) {
    let base = Style::new().fg(Color::DarkGray);
    let mut left = vec![Span::styled(
        status.provider.to_string(),
        base.add_modifier(Modifier::BOLD),
    )];
    if let Some(model) = status.model {
        left.push(Span::styled(" · ", base));
        left.push(Span::styled(model.to_string(), base));
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
                    transcript,
                    0,
                    &Status {
                        provider: "deepinfra",
                        model: Some("mock/model"),
                    },
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
                    Vec::new(),
                    0,
                    &Status {
                        provider: "an-very-long-provider-name",
                        model: Some("model"),
                    },
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
}
