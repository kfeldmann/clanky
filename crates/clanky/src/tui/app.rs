//! TUI application state (M3): transcript entries, input line, scrolling.

use std::path::PathBuf;

use clanky_protocol::{ChatMessage, Usage};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::completion;
use super::markdown;
use super::selection::{self, Selection};
use crate::error::Result;
use crate::session::Record;
use crate::turn::{TurnEvent, TurnOutput};

/// How many lines of a tool result are shown in the transcript (the full
/// result is always what the model receives).
const TOOL_PREVIEW_LINES: usize = 8;

/// One block in the chat transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Thinking(String),
    ToolCall {
        name: String,
        arguments: String,
    },
    ToolResult {
        name: String,
        output: String,
    },
    Error(String),
    /// A UI-only note (command feedback, etc.); not sent to the model
    /// and not recorded in the session file.
    Info(String),
    /// One labelled part of the assembled system prompt, shown by
    /// `/system` (UI-only): not sent, not recorded.
    SystemPart {
        label: String,
        content: String,
    },
}

/// Active Tab-completion cycle state (M7): Tab first inserts the longest
/// common prefix of the candidates (shown in a popup above the input);
/// further Tabs cycle through the individual candidates.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionState {
    /// Input text before the completed token.
    prefix: String,
    /// Input text after the completed token.
    suffix: String,
    /// Full replacement texts for the token, sorted.
    candidates: Vec<String>,
    /// Index into `candidates` currently inserted; `None` while the
    /// longest common prefix is shown.
    index: Option<usize>,
}

impl CompletionState {
    /// Build from the input split around the completed token.
    pub(crate) fn new(prefix: String, suffix: String, candidates: Vec<String>) -> Self {
        Self {
            prefix,
            suffix,
            candidates,
            index: None,
        }
    }

    /// Candidate texts, in cycle order.
    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }

    /// Currently selected candidate, if cycling has started.
    pub fn selected(&self) -> Option<usize> {
        self.index
    }

    /// Advance to the next candidate, wrapping around.
    pub(crate) fn advance(&mut self) {
        self.index = match self.index {
            None => Some(0),
            Some(i) => Some((i + 1) % self.candidates.len()),
        };
    }
}

/// Which transcript entry streaming deltas currently append to.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OpenKind {
    Text,
    Thinking,
}

/// Everything the TUI needs to know between frames.
pub struct App {
    pub entries: Vec<Entry>,
    /// Text being typed, always valid UTF-8.
    pub input: String,
    /// Byte offset of the caret into `input` (always on a char boundary).
    pub cursor: usize,
    /// A turn is running in the worker thread.
    pub busy: bool,
    /// A prompt typed while busy, sent when the turn finishes.
    pub pending: Option<String>,
    /// Distance from the bottom of the transcript in lines; 0 = follow.
    pub scroll_from_bottom: usize,
    /// Usage of the most recently completed turn, for the status line.
    pub last_usage: Option<Usage>,
    /// The complete conversation (system context first): what is passed
    /// to the next turn. Updated after each turn, restored on resume (M4).
    pub history: Vec<ChatMessage>,
    /// Active Tab-completion cycle (M7); cleared on any non-Tab key.
    pub completion: Option<CompletionState>,
    /// Active mouse selection over the transcript, in transcript
    /// coordinates; cleared by any key press and consumed on release.
    pub selection: Option<Selection>,
    /// Previously submitted prompts (most recent last), for ↑/↓ recall.
    input_history: Vec<String>,
    /// Index into `input_history` while recalling; `None` = live input.
    input_index: Option<usize>,
    /// The live input saved when ↑ first recalled a prompt.
    draft: String,
    open: Option<OpenKind>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            busy: false,
            pending: None,
            scroll_from_bottom: 0,
            last_usage: None,
            history: Vec::new(),
            completion: None,
            selection: None,
            input_history: Vec::new(),
            input_index: None,
            draft: String::new(),
            open: None,
        }
    }

    // --- transcript ---------------------------------------------------------

    /// Push a user prompt as a transcript entry.
    pub fn push_user(&mut self, prompt: &str) {
        self.entries.push(Entry::User(prompt.to_string()));
    }

    /// Apply one event from the agentic loop.
    pub fn on_turn_event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Text { delta } => self.append_delta(&delta, OpenKind::Text),
            TurnEvent::Thinking { delta } => self.append_delta(&delta, OpenKind::Thinking),
            TurnEvent::ToolCall { name, arguments } => {
                self.entries.push(Entry::ToolCall { name, arguments });
                self.open = None;
            }
            TurnEvent::ToolResult { name, output } => {
                self.entries.push(Entry::ToolResult { name, output });
                self.open = None;
            }
            // Round summaries are for the session recorder only; the
            // transcript is built from the streaming deltas above.
            TurnEvent::Round { .. } => {}
        }
    }

    /// Finalize the turn: record usage or surface the error.
    pub fn on_turn_done(&mut self, result: &Result<TurnOutput>) {
        self.busy = false;
        self.open = None;
        match result {
            Ok(output) => self.last_usage = output.usage,
            Err(err) => self.entries.push(Entry::Error(err.to_string())),
        }
    }

    fn append_delta(&mut self, delta: &str, kind: OpenKind) {
        let matches_kind = |entry: &Entry| {
            matches!(
                (entry, kind),
                (Entry::Assistant(_), OpenKind::Text) | (Entry::Thinking(_), OpenKind::Thinking)
            )
        };
        if self.open != Some(kind) || !self.entries.last().is_some_and(matches_kind) {
            self.entries.push(match kind {
                OpenKind::Text => Entry::Assistant(String::new()),
                OpenKind::Thinking => Entry::Thinking(String::new()),
            });
        }
        let Some(Entry::Assistant(text) | Entry::Thinking(text)) = self.entries.last_mut() else {
            unreachable!("just pushed a streaming entry")
        };
        text.push_str(delta);
        self.open = Some(kind);
    }

    // --- session (M4) -------------------------------------------------------

    /// Replace the transcript with a loaded session's records and return
    /// the rebuilt conversation history (system context excluded; the
    /// caller prepends fresh system messages).
    pub fn restore(&mut self, records: &[Record]) -> Vec<ChatMessage> {
        self.entries.clear();
        self.scroll_from_bottom = 0;
        self.last_usage = None;
        self.open = None;
        for record in records {
            match record {
                Record::User { text } => {
                    self.entries.push(Entry::User(text.clone()));
                    self.input_history.push(text.clone());
                }
                Record::Thinking { text } => self.entries.push(Entry::Thinking(text.clone())),
                Record::Assistant { text, calls } => {
                    self.entries.push(Entry::Assistant(text.clone()));
                    for call in calls {
                        self.entries.push(Entry::ToolCall {
                            name: call.name.clone(),
                            arguments: call.arguments.to_string(),
                        });
                    }
                }
                Record::ToolResult { name, output } => self.entries.push(Entry::ToolResult {
                    name: name.clone(),
                    output: output.clone(),
                }),
                Record::Error { message } => self.entries.push(Entry::Error(message.clone())),
                Record::Usage {
                    prompt_tokens,
                    completion_tokens,
                } => {
                    self.last_usage = Some(Usage {
                        prompt_tokens: *prompt_tokens,
                        completion_tokens: *completion_tokens,
                    })
                }
            }
        }
        crate::session::history_from_records(records)
    }

    // --- input --------------------------------------------------------------

    /// Take the current input as a prompt (if non-blank), clearing the
    /// line and recording the prompt for ↑/↓ recall.
    pub fn take_input(&mut self) -> Option<String> {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return None;
        }
        self.input.clear();
        self.cursor = 0;
        self.completion = None;
        self.input_index = None;
        self.draft.clear();
        self.input_history.push(text.clone());
        Some(text)
    }

    /// ↑: step back through previously submitted prompts. The first
    /// press saves the current input as a draft to restore later.
    pub fn history_up(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        self.input_index = Some(match self.input_index {
            None => {
                self.draft = std::mem::take(&mut self.input);
                self.input_history.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        });
        self.recall();
    }

    /// ↓: step forward through the history; past the newest entry the
    /// saved draft comes back and live editing resumes.
    pub fn history_down(&mut self) {
        let Some(index) = self.input_index else {
            return;
        };
        if index + 1 < self.input_history.len() {
            self.input_index = Some(index + 1);
            self.recall();
        } else {
            // Past the newest entry: back to live editing with the draft.
            self.input_index = None;
            self.input = std::mem::take(&mut self.draft);
            self.cursor = self.input.len();
            self.completion = None;
        }
    }

    /// Put the currently recalled entry into the input, caret at the end.
    fn recall(&mut self) {
        let index = self.input_index.expect("recall only while navigating");
        self.input = self.input_history[index].clone();
        self.cursor = self.input.len();
        self.completion = None;
    }

    // --- mouse selection ----------------------------------------------------

    /// Start selecting at a transcript point (mouse press).
    pub fn selection_start(&mut self, line: usize, col: usize) {
        self.selection = Some(Selection::new(line, col));
    }

    /// Drag the free end of the selection.
    pub fn selection_extend(&mut self, line: usize, col: usize) {
        if let Some(sel) = &mut self.selection {
            sel.extend(line, col);
        }
    }

    /// Finish selecting (mouse release): extract the selected text from
    /// the transcript wrapped at `width` and clear the state. `None` when
    /// the selection is empty or the anchor fell past the transcript.
    pub fn selection_take(&mut self, width: u16) -> Option<String> {
        let sel = self.selection.take()?;
        let lines = self.transcript_lines(width);
        let ((start_line, start_col), (end_line, end_col)) = sel.range();
        if start_line >= lines.len() {
            return None;
        }
        let end_line = end_line.min(lines.len() - 1);
        let mut text = String::new();
        for (index, line) in lines
            .iter()
            .enumerate()
            .skip(start_line)
            .take(end_line - start_line + 1)
        {
            let (start, end) = match (index == start_line, index == end_line) {
                (true, true) => (start_col, end_col),
                (true, false) => (start_col, usize::MAX),
                (false, true) => (0, end_col),
                (false, false) => (0, usize::MAX),
            };
            if index > start_line {
                text.push('\n');
            }
            text.push_str(&selection::text_in_columns(line, start, end));
        }
        let text = text.trim_end().to_string();
        (!text.is_empty()).then_some(text)
    }

    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn backspace(&mut self) {
        if let Some(pos) = self.prev_boundary() {
            self.input.replace_range(pos..self.cursor, "");
            self.cursor = pos;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.input.len() {
            let next = self.cursor
                + self.input[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8);
            self.input.replace_range(self.cursor..next, "");
        }
    }

    pub fn cursor_left(&mut self) {
        if let Some(pos) = self.prev_boundary() {
            self.cursor = pos;
        }
    }

    pub fn cursor_right(&mut self) {
        self.cursor = self
            .input
            .char_indices()
            .skip_while(|(i, _)| *i <= self.cursor)
            .map(|(i, _)| i)
            .next()
            .unwrap_or(self.input.len());
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.input.len();
    }

    fn prev_boundary(&self) -> Option<usize> {
        self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    // --- tab completion (M7) ------------------------------------------------

    /// Complete the file path at the caret. First press inserts the longest
    /// common prefix and opens the candidate popup; further presses cycle
    /// through the candidates. Any other key clears the state.
    pub fn tab_complete(&mut self) {
        if self.completion.is_some() {
            self.cycle_completion();
            return;
        }
        let (start, end) = completion::token_range(&self.input, self.cursor);
        let token = self.input[start..end].to_string();
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let candidates = completion::candidates(&base, &token);
        match candidates.as_slice() {
            [] => {
                let note = Entry::Info("no completions".into());
                if self.entries.last() != Some(&note) {
                    self.entries.push(note);
                }
            }
            [only] => self.replace_token(start, end, only),
            many => {
                let prefix = self.input[..start].to_string();
                let suffix = self.input[end..].to_string();
                let lcp = completion::common_prefix(many);
                self.replace_token(start, end, &lcp);
                self.completion = Some(CompletionState::new(prefix, suffix, candidates));
            }
        }
    }

    /// Cycle to the next candidate of the active completion.
    fn cycle_completion(&mut self) {
        let Some(mut state) = self.completion.take() else {
            return;
        };
        state.advance();
        let text = state.candidates[state.index.expect("advance sets an index")].clone();
        self.input = format!("{}{}{}", state.prefix, text, state.suffix);
        self.cursor = state.prefix.len() + text.len();
        self.completion = Some(state);
    }

    /// Substitute the token at `start..end` with `text`; the caret moves to
    /// the end of the inserted text.
    fn replace_token(&mut self, start: usize, end: usize, text: &str) {
        let suffix = self.input[end..].to_string();
        self.input = format!("{}{}{}", &self.input[..start], text, suffix);
        self.cursor = start + text.len();
    }

    // --- scrolling ----------------------------------------------------------

    /// Clear the transcript (Ctrl+L); scroll position resets.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.scroll_from_bottom = 0;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_from_bottom += lines;
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(lines);
    }

    // --- rendering ----------------------------------------------------------

    /// Build the (pre-wrapped) transcript lines for a viewport `width`.
    pub fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for entry in &self.entries {
            match entry {
                Entry::User(text) => {
                    let prompt = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
                    lines.extend(markdown::wrap(
                        &[Line::from(vec![
                            Span::styled("❯ ", prompt),
                            Span::styled(text.clone(), Style::new()),
                        ])],
                        width,
                    ));
                }
                Entry::Assistant(text) => {
                    lines.extend(markdown::wrap(&markdown::render(text, Style::new()), width));
                    lines.push(Line::default());
                }
                Entry::Thinking(text) => {
                    let base = Style::new()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC);
                    lines.extend(markdown::wrap(&markdown::render(text, base), width));
                    lines.push(Line::default());
                }
                Entry::ToolCall { name, arguments } => {
                    lines.push(Line::from(vec![
                        Span::styled("● ", Style::new().fg(Color::Blue)),
                        Span::styled(name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" {arguments}"), Style::new().fg(Color::DarkGray)),
                    ]));
                }
                Entry::ToolResult { name, output } => {
                    lines.push(Line::from(vec![
                        Span::styled("  └─ ", Style::new().fg(Color::DarkGray)),
                        Span::styled(name.clone(), Style::new().fg(Color::DarkGray)),
                        Span::styled(": ", Style::new().fg(Color::DarkGray)),
                        Span::styled(preview(output), Style::new().fg(Color::Gray)),
                    ]));
                }
                Entry::Error(message) => {
                    lines.push(Line::from(Span::styled(
                        format!("✗ {message}"),
                        Style::new().fg(Color::Red),
                    )));
                }
                Entry::Info(message) => {
                    lines.push(Line::from(Span::styled(
                        format!("· {message}"),
                        Style::new().fg(Color::DarkGray),
                    )));
                }
                Entry::SystemPart { label, content } => {
                    lines.push(Line::from(vec![
                        Span::styled("◆ ", Style::new().fg(Color::Cyan)),
                        Span::styled(
                            label.clone(),
                            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    let dimmed = Style::new().fg(Color::Gray);
                    lines.extend(markdown::wrap(
                        &[Line::from(Span::styled(content.clone(), dimmed))],
                        width,
                    ));
                    lines.push(Line::default());
                }
            }
        }
        lines
    }
}

/// Shorten a tool result for display: a few lines plus an omission note.
fn preview(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= TOOL_PREVIEW_LINES {
        return output.to_string();
    }
    let shown = lines[..TOOL_PREVIEW_LINES].join("\n");
    format!(
        "{shown}\n… (+{} more lines)",
        lines.len() - TOOL_PREVIEW_LINES
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_deltas_form_one_assistant_entry() {
        let mut app = App::new();
        app.on_turn_event(TurnEvent::Text { delta: "he".into() });
        app.on_turn_event(TurnEvent::Text {
            delta: "llo".into(),
        });
        assert_eq!(app.entries, vec![Entry::Assistant("hello".into())]);
    }

    #[test]
    fn thinking_then_text_split_entries() {
        let mut app = App::new();
        app.on_turn_event(TurnEvent::Thinking {
            delta: "hmm".into(),
        });
        app.on_turn_event(TurnEvent::Text { delta: "hi".into() });
        assert_eq!(
            app.entries,
            vec![Entry::Thinking("hmm".into()), Entry::Assistant("hi".into())]
        );
    }

    #[test]
    fn tool_call_closes_the_streaming_entry() {
        // Text after a tool call must open a new assistant entry so it
        // renders *below* the tool activity, not above it.
        let mut app = App::new();
        app.on_turn_event(TurnEvent::Text {
            delta: "checking".into(),
        });
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "a\nb".into(),
        });
        app.on_turn_event(TurnEvent::Text {
            delta: "done".into(),
        });
        assert_eq!(
            app.entries,
            vec![
                Entry::Assistant("checking".into()),
                Entry::ToolCall {
                    name: "bash".into(),
                    arguments: r#"{"command":"ls"}"#.into()
                },
                Entry::ToolResult {
                    name: "bash".into(),
                    output: "a\nb".into()
                },
                Entry::Assistant("done".into()),
            ]
        );
    }

    #[test]
    fn done_records_usage_or_error() {
        let mut app = App::new();
        app.busy = true;
        let usage = Some(Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(3),
        });
        app.on_turn_done(&Ok(TurnOutput {
            text: "hi".into(),
            finish_reason: clanky_protocol::FinishReason::Stop,
            usage,
            history: Vec::new(),
        }));
        assert_eq!(app.last_usage, usage);
        assert!(!app.busy);

        app.busy = true;
        app.on_turn_done(&Err(crate::error::Error::NoModel));
        assert!(!app.busy);
        assert!(matches!(app.entries.last(), Some(Entry::Error(_))));
    }

    #[test]
    fn input_editing_inserts_and_removes_chars() {
        let mut app = App::new();
        for c in "ab".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.input, "ab");
        assert_eq!(app.cursor, 2);

        app.backspace();
        assert_eq!(app.input, "a");
        assert_eq!(app.cursor, 1);

        app.cursor_left();
        app.insert_char('c');
        assert_eq!(app.input, "ca");
        assert_eq!(app.cursor, 1);

        app.cursor_right();
        app.insert_char('d');
        assert_eq!(app.input, "cad");
        assert_eq!(app.cursor, 3);

        app.cursor_home();
        app.delete();
        assert_eq!(app.input, "ad");
        assert_eq!(app.cursor, 0);

        app.cursor_end();
        app.delete();
        assert_eq!(app.input, "ad", "delete at end is a no-op");
        app.cursor_home();
        app.backspace();
        assert_eq!(app.input, "ad", "backspace at start is a no-op");
    }

    #[test]
    fn multibyte_chars_edit_correctly() {
        let mut app = App::new();
        for c in "é中".chars() {
            app.insert_char(c);
        }
        assert_eq!(app.input, "é中");
        assert_eq!(app.cursor, "é中".len());

        app.backspace();
        assert_eq!(app.input, "é");
        app.backspace();
        assert_eq!(app.input, "");
        app.backspace(); // no-op at start
        assert_eq!(app.input, "");
    }

    #[test]
    fn take_input_clears_and_requires_text() {
        let mut app = App::new();
        assert_eq!(app.take_input(), None);
        app.input = "  hi  ".into();
        app.cursor = 6;
        assert_eq!(app.take_input().as_deref(), Some("hi"));
        assert_eq!(app.input, "");
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn tool_results_preview_to_few_lines() {
        let output = (1..=12)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let shown = preview(&output);
        assert_eq!(shown.lines().count(), TOOL_PREVIEW_LINES + 1);
        assert!(shown.contains("(+4 more lines)"));
        assert_eq!(preview("one line"), "one line");
    }

    #[test]
    fn transcript_renders_markdown_and_wraps() {
        let mut app = App::new();
        app.push_user("hello");
        app.on_turn_event(TurnEvent::Text {
            delta: "a **bold** reply".into(),
        });
        let lines = app.transcript_lines(40);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(texts.iter().any(|t| t.contains("❯ hello")));
        assert!(texts.iter().any(|t| t.contains("bold")));
    }

    #[test]
    fn tool_calls_render_with_marker_and_preview() {
        let mut app = App::new();
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: json!({"command": "ls"}).to_string(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "file.txt".into(),
        });
        let lines = app.transcript_lines(80);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("● bash {\"command\":\"ls\"}"))
        );
        assert!(texts.iter().any(|t| t.contains("└─ bash: file.txt")));
    }

    #[test]
    fn scroll_tracks_distance_from_bottom() {
        let mut app = App::new();
        app.scroll_up(3);
        assert_eq!(app.scroll_from_bottom, 3);
        app.scroll_down(1);
        assert_eq!(app.scroll_from_bottom, 2);
        app.scroll_down(5);
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn clear_empties_the_transcript() {
        let mut app = App::new();
        app.push_user("hi");
        app.scroll_up(5);
        app.clear();
        assert!(app.entries.is_empty());
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn restore_rebuilds_entries_usage_and_history() {
        use serde_json::json;
        let records = vec![
            Record::User { text: "hi".into() },
            Record::Assistant {
                text: "checking".into(),
                calls: vec![clanky_protocol::ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "ls"}),
                }],
            },
            Record::ToolResult {
                name: "bash".into(),
                output: "a\nb".into(),
            },
            Record::Assistant {
                text: "done".into(),
                calls: vec![],
            },
            Record::Usage {
                prompt_tokens: Some(9),
                completion_tokens: Some(2),
            },
        ];
        let mut app = App::new();
        app.push_user("old");
        let history = app.restore(&records);
        assert_eq!(
            app.entries.len(),
            5,
            "user, assistant, call, result, assistant"
        );
        assert_eq!(app.last_usage.and_then(|u| u.completion_tokens), Some(2));
        assert_eq!(history.len(), 4, "user, assistant+call, tool, assistant");

        let lines = app.transcript_lines(80);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(texts.iter().any(|t| t.contains("❯ hi")));
        assert!(texts.iter().any(|t| t.contains("● bash")));
        assert!(texts.iter().any(|t| t.contains("└─ bash: a")));
    }

    #[test]
    fn info_entries_render_dimmed() {
        let mut app = App::new();
        app.entries.push(Entry::Info("renamed".into()));
        let lines = app.transcript_lines(40);
        assert!(lines.iter().any(|l| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
                .contains("· renamed")
        }));
    }

    #[test]
    fn completion_cycles_through_candidates() {
        let mut app = App::new();
        app.completion = Some(CompletionState::new(
            "cat ".into(),
            String::new(),
            vec!["alpha.md ".into(), "alpha.txt ".into()],
        ));

        // First Tab applies the first candidate (the LCP was shown before).
        app.tab_complete();
        assert_eq!(app.input, "cat alpha.md ");
        assert_eq!(app.cursor, "cat alpha.md ".len());

        // Subsequent Tabs cycle, wrapping around at the end.
        app.tab_complete();
        assert_eq!(app.input, "cat alpha.txt ");
        app.tab_complete();
        assert_eq!(app.input, "cat alpha.md ");
    }

    #[test]
    fn completion_cycle_keeps_prefix_and_suffix() {
        let mut app = App::new();
        app.completion = Some(CompletionState::new(
            "read ".into(),
            " now".into(),
            vec!["beta/".into(), "src/".into()],
        ));
        app.tab_complete();
        assert_eq!(app.input, "read beta/ now");
        assert_eq!(app.cursor, "read beta/".len(), "caret after the token");
        app.tab_complete();
        assert_eq!(app.input, "read src/ now");
    }

    #[test]
    fn take_input_clears_completion_state() {
        let mut app = App::new();
        app.completion = Some(CompletionState::new(
            String::new(),
            String::new(),
            vec!["x ".into()],
        ));
        app.input = "hi".into();
        app.cursor = 2;
        app.take_input();
        assert!(app.completion.is_none());
    }

    #[test]
    fn submitted_prompts_are_recallable_with_up_down() {
        let mut app = App::new();
        app.input = "first".into();
        app.cursor = 5;
        app.take_input();
        app.input = "second".into();
        app.cursor = 6;
        app.take_input();

        // The first ↑ saves the live input as a draft, then recalls the
        // newest prompt; further ↑s walk back, clamped at the oldest.
        app.input = "half-typed".into();
        app.cursor = 4;
        app.history_up();
        assert_eq!(app.input, "second");
        assert_eq!(app.draft, "half-typed");
        app.history_up();
        assert_eq!(app.input, "first");
        app.history_up(); // clamped at the oldest
        assert_eq!(app.input, "first");

        // ↓ comes back, and past the newest restores the saved draft.
        app.history_down();
        assert_eq!(app.input, "second");
        app.history_down(); // past the end: draft returns
        assert_eq!(app.input, "half-typed");
        assert_eq!(app.cursor, app.input.len(), "caret at the end");
        app.history_down(); // live again: no-op
        assert_eq!(app.input, "half-typed");
    }

    #[test]
    fn history_up_with_no_history_is_a_no_op() {
        let mut app = App::new();
        app.input = "hi".into();
        app.history_up();
        assert_eq!(app.input, "hi");
        app.history_down();
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn restore_rebuilds_the_input_history() {
        let records = vec![
            Record::User { text: "one".into() },
            Record::Assistant {
                text: "hi".into(),
                calls: vec![],
            },
            Record::User { text: "two".into() },
        ];
        let mut app = App::new();
        app.restore(&records);
        app.history_up();
        assert_eq!(app.input, "two");
        app.history_up();
        assert_eq!(app.input, "one");
    }

    #[test]
    fn selection_take_extracts_the_selected_text() {
        let mut app = App::new();
        app.push_user("hello world");
        app.entries.push(Entry::Assistant("alpha beta".into()));

        // Transcript lines: 0 = "❯ hello world", 1 = "alpha beta",
        // 2 = blank (assistant entries end with a blank line).

        // Single line, middle columns: `world` spans columns 8..13.
        app.selection_start(0, 8);
        app.selection_extend(0, 13);
        assert_eq!(app.selection_take(80).as_deref(), Some("world"));

        // Backwards drag across two lines: end of line 0 down to `alpha`.
        app.selection_start(1, 5);
        app.selection_extend(0, 8);
        assert_eq!(app.selection_take(80).as_deref(), Some("world\nalpha"));

        // A click without a drag selects nothing.
        app.selection_start(0, 3);
        app.selection_extend(0, 3);
        assert_eq!(app.selection_take(80), None);

        // An anchor past the transcript selects nothing.
        app.selection_start(50, 0);
        app.selection_extend(50, 5);
        assert_eq!(app.selection_take(80), None);
    }

    #[test]
    fn system_parts_render_with_label_and_content() {
        let mut app = App::new();
        app.entries.push(Entry::Info("2 part(s)".into()));
        app.entries.push(Entry::SystemPart {
            label: "skill review".into(),
            content: "Check the diffs.".into(),
        });
        let lines = app.transcript_lines(80);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("◆ skill review")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("Check the diffs.")));
    }
}
