//! TUI application state (M3): transcript entries and the input line.
//!
//! The transcript is printed straight to the terminal (see `super::screen`):
//! entries become final once they stop changing, and the renderer commits
//! their lines permanently. These fields track what has been printed so a
//! re-render only touches the streaming tail.

use std::path::PathBuf;

use clanky_protocol::{ChatMessage, Usage};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr as _;

use super::completion;
use super::markdown;
use crate::error::Result;
use crate::session::Record;
use crate::turn::{TurnEvent, TurnOutput};

/// How many lines of a tool result are shown in the transcript (the full
/// result is always what the model receives).
const TOOL_PREVIEW_LINES: usize = 8;

/// Conservative tokens-per-character ratio for live usage estimates.
/// English prose runs ~4 chars/token (incl. the space); code, JSON and
/// identifier-heavy text run ~3. Assuming 3 overestimates prose by ~25%
/// but never under-reports a context that is actually filling up.
const ESTIMATE_CHARS_PER_TOKEN: f64 = 3.0;

/// Fraction of a streamed message already counted before the estimate
/// tops up. Every chunk fires `pending_tokens`, so with r character the
/// estimate advances r - floor((r-k)/k'·k')-style in steps of k' ~ k/2:
/// e.g. k = 32 gives +16 tokens per 32 characters (48 chars/token
/// average, a deliberately loose lower bound) with at most one top-up
/// lagging behind the newest text.
const ESTIMATE_STEP_CHARS: usize = 32;

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
    /// Usage of the most recently completed chat round (not turn), for
    /// the status line: its prompt tokens are what the next request
    /// re-sends, so they drive the ctx % display.
    pub last_usage: Option<Usage>,
    /// Session totals across all completed turns (prompt tokens are
    /// re-billed every turn, so the cumulative sum is what the cost display
    /// needs). Restored from a resumed session's usage records.
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    /// The complete conversation (system context first): what is passed
    /// to the next turn. Updated after each turn, restored on resume (M4).
    pub history: Vec<ChatMessage>,
    /// Estimate pools for content no provider report covers yet (see
    /// `estimate_add`): character counts and the token estimates derived
    /// from them, one pool for prompt-side content (the user prompt,
    /// tool results fed back) and one for completion-side content
    /// (streamed text/thinking). A usage report covers everything sent
    /// and received so far, so it clears both pools.
    estimate_prompt_chars: usize,
    estimate_prompt: u64,
    estimate_completion_chars: usize,
    estimate_completion: u64,
    /// Active Tab-completion cycle (M7); cleared on any non-Tab key.
    pub completion: Option<CompletionState>,
    /// How many leading entries are fully printed to the terminal; the
    /// screen renderer commits final entries permanently and only rewrites
    /// the streaming tail.
    pub(crate) printed_entries: usize,
    /// Committed-line count per entry index: the streaming entry may be
    /// partially committed when it outgrows the screen (see `super::screen`).
    pub(crate) printed: Vec<usize>,
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
            last_usage: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            completion: None,
            printed_entries: 0,
            printed: Vec::new(),
            input_index: None,
            draft: String::new(),
            open: None,
            history: Vec::new(),
            input_history: Vec::new(),
            estimate_prompt_chars: 0,
            estimate_prompt: 0,
            estimate_completion_chars: 0,
            estimate_completion: 0,
        }
    }

    // --- transcript ---------------------------------------------------------

    /// Push a user prompt as a transcript entry and count it into the
    /// prompt estimate: until a usage report covers it, its cost toward
    /// the context is estimated from its length.
    pub fn push_user(&mut self, prompt: &str) {
        self.entries.push(Entry::User(prompt.to_string()));
        self.estimate_add(prompt.chars().count(), true);
    }

    /// Apply one event from the agentic loop.
    pub fn on_turn_event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::Text { delta } => {
                self.estimate_add(delta.chars().count(), false);
                self.append_delta(&delta, OpenKind::Text);
            }
            TurnEvent::Thinking { delta } => {
                self.estimate_add(delta.chars().count(), false);
                self.append_delta(&delta, OpenKind::Thinking);
            }
            TurnEvent::ToolCall { name, arguments } => {
                self.entries.push(Entry::ToolCall { name, arguments });
                self.open = None;
            }
            TurnEvent::ToolResult { name, output } => {
                // The result is fed back to the model, so it counts as
                // prompt-side content until the next round's report.
                self.estimate_add(output.chars().count(), true);
                self.entries.push(Entry::ToolResult { name, output });
                self.open = None;
            }
            // Round summaries are for the session recorder only; the
            // transcript is built from the streaming deltas above.
            TurnEvent::Round { .. } => {}
            // Per-round usage arrives before the turn's `done`; the
            // turn-total event is gone, so this is the only place
            // counters are updated. `last_usage` becomes the latest
            // round's own usage — exactly what the next request re-sends
            // as prompt tokens, which is what the ctx % should show.
            TurnEvent::Usage { usage } => {
                self.last_usage = Some(usage);
                self.total_prompt_tokens += usage.prompt_tokens.unwrap_or(0);
                self.total_completion_tokens += usage.completion_tokens.unwrap_or(0);
                // The report covers everything sent and received so far:
                // live values switch to billed-only until new content lands.
                self.estimate_reset();
            }
        }
    }

    /// Finalize the turn: surface the error, if any. Usage was already
    /// applied round-by-round via [`TurnEvent::Usage`].
    pub fn on_turn_done(&mut self, result: &Result<TurnOutput>) {
        self.busy = false;
        self.open = None;
        // The turn is over; stale estimates for content that arrived
        // without a usage report would otherwise linger.
        self.estimate_reset();
        if let Err(err) = result {
            self.entries.push(Entry::Error(err.to_string()));
        }
    }

    // --- live usage estimates -----------------------------------------------

    /// Top up an estimate pool with `chars` new characters. The pools
    /// cover content whose real token counts are not known yet: the
    /// prompt side holds the user prompt and tool results (both go back
    /// to the model in the next request), the completion side holds
    /// streamed text and thinking. A step counter keeps the display from
    /// flickering on every chunk (see the constants above).
    fn estimate_add(&mut self, chars: usize, prompt: bool) {
        let (counted, tokens) = if prompt {
            (&mut self.estimate_prompt_chars, &mut self.estimate_prompt)
        } else {
            (
                &mut self.estimate_completion_chars,
                &mut self.estimate_completion,
            )
        };
        *counted += chars;
        let steps = *counted / ESTIMATE_STEP_CHARS;
        let chars = (steps * ESTIMATE_STEP_CHARS) as f64;
        *tokens = (chars / ESTIMATE_CHARS_PER_TOKEN) as u64;
    }

    /// A usage report (or the turn's end) covers all prior content:
    /// clear the pools so live values are billed-only.
    fn estimate_reset(&mut self) {
        self.estimate_prompt_chars = 0;
        self.estimate_prompt = 0;
        self.estimate_completion_chars = 0;
        self.estimate_completion = 0;
    }

    /// Estimated `(prompt, completion)` tokens for content not yet
    /// covered by a provider usage report. Live status values are the
    /// billed totals plus these; with a report in hand they are zero
    /// (exact display), but a report only lands after a round completes,
    /// so a huge reasoning block still counts while it streams.
    pub fn estimated_tokens(&self) -> (u64, u64) {
        (self.estimate_prompt, self.estimate_completion)
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
        self.printed_entries = 0;
        self.printed.clear();
        self.last_usage = None;
        self.total_prompt_tokens = 0;
        self.total_completion_tokens = 0;
        self.estimate_reset();
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
                    });
                    self.total_prompt_tokens += prompt_tokens.unwrap_or(0);
                    self.total_completion_tokens += completion_tokens.unwrap_or(0);
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

    // --- printed-line bookkeeping -------------------------------------------

    /// How many leading lines of entry `index` are already printed to the
    /// terminal permanently (0 when the entry is not tracked yet).
    pub(crate) fn printed(&self, index: usize) -> usize {
        self.printed.get(index).copied().unwrap_or(0)
    }

    /// Record that `count` leading lines of entry `index` are printed.
    pub(crate) fn set_printed(&mut self, index: usize, count: usize) {
        if self.printed.len() <= index {
            self.printed.resize(index + 1, 0);
        }
        self.printed[index] = count;
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

    // --- screen bookkeeping --------------------------------------------------

    /// Clear the transcript (Ctrl+L): the screen renderer erases the visible
    /// screen; the session file keeps every record.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.printed_entries = 0;
        self.printed = Vec::new();
    }

    // --- rendering ----------------------------------------------------------

    /// Index of the first entry whose display lines can still change (the
    /// one being streamed into); everything before it is final. Equal to
    /// `entries.len()` when nothing is streaming.
    pub fn sealed_boundary(&self) -> usize {
        let streaming_last = matches!(
            (self.open, self.entries.last()),
            (Some(OpenKind::Text), Some(Entry::Assistant(_)))
                | (Some(OpenKind::Thinking), Some(Entry::Thinking(_)))
        );
        if streaming_last {
            self.entries.len() - 1
        } else {
            self.entries.len()
        }
    }

    /// Wrapped display lines for one transcript entry.
    pub fn entry_lines(&self, index: usize, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        match &self.entries[index] {
            Entry::User(text) => {
                let prompt = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
                // Multi-line input must be split per source line; feeding
                // it to `Line::raw` as one string would lose the newlines.
                let mut src: Vec<Line<'static>> = Vec::new();
                for (i, l) in text.lines().enumerate() {
                    if i == 0 {
                        src.push(Line::from(vec![
                            Span::styled("❯ ", prompt),
                            Span::styled(l.to_string(), Style::new()),
                        ]));
                    } else {
                        src.push(Line::from(Span::styled(format!("  {l}"), Style::new())));
                    }
                }
                if src.is_empty() {
                    src.push(Line::from(Span::styled("❯ ", prompt)));
                }
                lines.extend(markdown::wrap(&src, width));
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
                // Arguments may contain newlines (pretty-printed JSON):
                // emit one source Line per argument line. Raw `\n`
                // characters must never reach the terminal: LF moves the
                // cursor down without returning to column 0, so every
                // line would start below the end of the previous one.
                let mut body = arguments.lines();
                let mut src = vec![Line::from(vec![
                    Span::styled("● ".to_string(), Style::new().fg(Color::Blue)),
                    Span::styled(name.clone(), Style::new().add_modifier(Modifier::BOLD)),
                ])];
                if let Some(first) = body.next() {
                    src[0].spans.push(Span::styled(
                        format!(" {first}"),
                        Style::new().fg(Color::DarkGray),
                    ));
                }
                for l in body {
                    src.push(Line::from(Span::styled(
                        format!("   {l}"),
                        Style::new().fg(Color::DarkGray),
                    )));
                }
                lines.extend(markdown::wrap(&src, width));
            }
            Entry::ToolResult { name, output } => {
                let failed = crate::tools::is_failed_result(output);
                let style = if failed {
                    Style::new().fg(Color::Red)
                } else {
                    Style::new().fg(Color::DarkGray)
                };
                let preview_style = if failed {
                    Style::new().fg(Color::Red)
                } else {
                    Style::new().fg(Color::Gray)
                };
                // The preview spans multiple lines: emit one source Line
                // per output line (continuations aligned under the first
                // output character) and wrap each to `width`. Raw `\n`
                // characters must never reach the terminal: LF moves the
                // cursor down without returning to column 0, so every
                // line would start below the end of the previous one
                // instead of at the left edge.
                let preview = preview(output);
                let mut body = preview.lines();
                let mut src = vec![Line::from(vec![
                    Span::styled("  └─ ".to_string(), style),
                    Span::styled(name.clone(), style),
                    Span::styled(": ".to_string(), style),
                    Span::styled(body.next().unwrap_or("").to_string(), preview_style),
                ])];
                let indent = " ".repeat("  └─ ".width() + name.width() + 2);
                for l in body {
                    src.push(Line::from(Span::styled(
                        format!("{indent}{l}"),
                        preview_style,
                    )));
                }
                lines.extend(markdown::wrap(&src, width));
            }
            // Error/info text may contain newlines; route it through
            // `wrap_text` so they split into separate rows (see the
            // `Entry::ToolResult` note on raw LF reaching the terminal).
            Entry::Error(message) => {
                lines.extend(markdown::wrap_text(
                    &format!("✗ {message}"),
                    Style::new().fg(Color::Red),
                    width,
                ));
            }
            Entry::Info(message) => {
                lines.extend(markdown::wrap_text(
                    &format!("· {message}"),
                    Style::new().fg(Color::DarkGray),
                    width,
                ));
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
                lines.extend(markdown::wrap_text(content, dimmed, width));
                lines.push(Line::default());
            }
        }
        lines
    }

    /// Wrapped display lines for entries `start..`.
    #[cfg(test)]
    pub fn lines_from(&self, start: usize, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for index in start..self.entries.len() {
            lines.extend(self.entry_lines(index, width));
        }
        lines
    }

    /// Build the (pre-wrapped) transcript lines for a viewport `width`.
    #[cfg(test)]
    pub fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.lines_from(0, width)
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
    fn usage_accumulates_per_round_and_done_surfaces_errors() {
        let mut app = App::new();
        app.busy = true;
        let round1 = Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(3),
        };
        let round2 = Usage {
            prompt_tokens: Some(12),
            completion_tokens: Some(4),
        };
        // A two-round turn: usage lands as each round completes.
        app.on_turn_event(TurnEvent::Usage { usage: round1 });
        app.on_turn_event(TurnEvent::Usage { usage: round2 });
        assert_eq!(app.last_usage, Some(round2), "last round's own usage");
        assert_eq!(app.total_prompt_tokens, 22, "rounds sum into the total");
        assert_eq!(app.total_completion_tokens, 7);

        // done only ends the turn; usage was already applied.
        let before = (app.total_prompt_tokens, app.total_completion_tokens);
        app.on_turn_done(&Ok(TurnOutput {
            text: "hi".into(),
            finish_reason: clanky_protocol::FinishReason::Stop,
            usage: None,
        }));
        assert!(!app.busy);
        assert_eq!(app.last_usage, Some(round2));
        assert_eq!(
            (app.total_prompt_tokens, app.total_completion_tokens),
            before,
            "done must not re-add usage"
        );

        app.busy = true;
        app.on_turn_done(&Err(crate::error::Error::NoModel));
        assert!(!app.busy);
        assert!(matches!(app.entries.last(), Some(Entry::Error(_))));
    }

    #[test]
    fn estimates_streamed_text_and_resets_on_usage() {
        let mut app = App::new();
        assert_eq!(app.estimated_tokens(), (0, 0));

        // 40 chars at a 32-char step and 3 chars/token: 32/3 -> 10.
        app.on_turn_event(TurnEvent::Thinking {
            delta: "x".repeat(40),
        });
        assert_eq!(app.estimated_tokens(), (0, 10));

        // More text tops the estimate up; text and thinking share the
        // completion-side pool.
        app.on_turn_event(TurnEvent::Text {
            delta: "y".repeat(40),
        });
        // 80 counted chars -> 64/3 = 21 (floor).
        assert_eq!(app.estimated_tokens(), (0, 21));

        // The provider's report covers everything streamed so far: the
        // estimate clears, the billed totals carry the values.
        app.on_turn_event(TurnEvent::Usage {
            usage: Usage {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
            },
        });
        assert_eq!(app.estimated_tokens(), (0, 0));
        assert_eq!(app.total_prompt_tokens, 100);
        assert_eq!(app.total_completion_tokens, 50);

        // Streaming again starts from a clean estimate.
        app.on_turn_event(TurnEvent::Text {
            delta: "z".repeat(32),
        });
        assert_eq!(app.estimated_tokens(), (0, 10));
    }

    #[test]
    fn user_prompt_and_tool_results_estimate_into_the_prompt_pool() {
        let mut app = App::new();
        // 96 chars -> 3 steps * 32 chars / 3 = 32.
        app.push_user(&"x".repeat(96));
        assert_eq!(app.estimated_tokens(), (32, 0));

        // A tool result is fed back to the model: prompt-side too.
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "y".repeat(96),
        });
        assert_eq!(app.estimated_tokens(), (64, 0));

        // A usage report covers everything sent and received so far.
        app.on_turn_event(TurnEvent::Usage {
            usage: Usage {
                prompt_tokens: Some(500),
                completion_tokens: Some(20),
            },
        });
        assert_eq!(app.estimated_tokens(), (0, 0));
        assert_eq!(app.total_prompt_tokens, 500);
    }

    #[test]
    fn estimate_clears_when_a_turn_ends_without_usage() {
        let mut app = App::new();
        app.busy = true;
        app.on_turn_event(TurnEvent::Thinking {
            delta: "x".repeat(96),
        });
        assert_eq!(app.estimated_tokens(), (0, 32));
        app.on_turn_done(&Ok(TurnOutput {
            text: String::new(),
            finish_reason: clanky_protocol::FinishReason::Stop,
            usage: None,
        }));
        assert_eq!(app.estimated_tokens(), (0, 0));
    }

    #[test]
    fn failed_tool_results_render_in_red() {
        let mut app = App::new();
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: r#"{"command":"false"}"#.into(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "out\n[exit status: 1]".into(),
        });
        let lines = app.entry_lines(1, 80);
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|s| s.style.fg == Some(Color::Red))
        );

        // Successful results keep their muted colors. The header spans
        // share one style and merge under wrapping, so the preview text
        // is the last span of the row.
        app.entries.push(Entry::ToolResult {
            name: "bash".into(),
            output: "ok".into(),
        });
        let lines = app.entry_lines(2, 80);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));
        let last = lines[0].spans.last().unwrap();
        assert_eq!(last.style.fg, Some(Color::Gray));
        assert_eq!(last.content, "ok");

        // Tool-call failures (ERROR: prefix) are failures too.
        app.entries.push(Entry::ToolResult {
            name: "nope".into(),
            output: "ERROR: unknown tool: nope".into(),
        });
        let lines = app.entry_lines(3, 80);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
    }

    /// Regression test: multi-line tool output used to be embedded as
    /// raw `\n` characters inside one `Line`; the terminal's LF moves
    /// down without returning to column 0, so every output line started
    /// right below the end of the previous one (a staircase) and the
    /// renderer's row-count math broke. Each output line must be its own
    /// wrapped row, aligned under the first output character.
    #[test]
    fn tool_result_output_lines_start_at_the_left_edge() {
        let mut app = App::new();
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "one\ntwo\nthree".into(),
        });
        let lines = app.entry_lines(0, 80);
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !span.content.contains('\n'),
                    "raw newline reached a display line: {:?}",
                    span.content
                );
            }
        }
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        // "  └─ " + "bash" + ": " = 11 columns before the first output
        // character; continuation rows align under it.
        assert!(texts[0].ends_with("└─ bash: one"), "{texts:?}");
        assert_eq!(texts[1], " ".repeat(11) + "two", "{texts:?}");
        assert_eq!(texts[2], " ".repeat(11) + "three", "{texts:?}");
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
        eprintln!("TEXTS: {texts:?}");
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("● bash {\"command\":\"ls\"}"))
        );
        assert!(texts.iter().any(|t| t.contains("└─ bash: file.txt")));
    }

    #[test]
    fn clear_empties_the_transcript_and_print_bookkeeping() {
        let mut app = App::new();
        app.push_user("hi");
        app.set_printed(0, 3);
        app.printed_entries = 1;
        app.clear();
        assert!(app.entries.is_empty());
        assert_eq!(app.printed_entries, 0);
        assert_eq!(app.printed(0), 0);
    }

    #[test]
    fn sealed_boundary_tracks_the_streaming_entry() {
        let mut app = App::new();
        assert_eq!(app.sealed_boundary(), 0, "empty transcript is all sealed");

        app.push_user("hi");
        assert_eq!(app.sealed_boundary(), 1, "final entries are sealed");

        app.on_turn_event(TurnEvent::Text {
            delta: "streaming".into(),
        });
        assert_eq!(app.sealed_boundary(), 1, "the open entry stays unsealed");

        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: "{}".into(),
        });
        assert_eq!(app.sealed_boundary(), 3, "closing the stream seals it");

        // An info note pushed mid-stream seals the streaming entry too.
        app.on_turn_event(TurnEvent::Text {
            delta: "more".into(),
        });
        assert_eq!(app.sealed_boundary(), 3);
        app.entries.push(Entry::Info("note".into()));
        assert_eq!(app.sealed_boundary(), 5, "everything is final again");
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
