//! TUI application state (M3): transcript entries and the input line.
//!
//! The transcript is printed straight to the terminal (see `super::screen`):
//! entries become final once they stop changing, and the renderer commits
//! their lines permanently. These fields track what has been printed so a
//! re-render only touches the streaming tail.

use std::path::PathBuf;

use clanky_protocol::{ChatMessage, ToolCall, Usage};
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

/// Background colors for block-styled transcript entries (see
/// `App::with_block_bg`). Text colors live at the rendering sites (they
/// pair with the foreground choice); these are the block fills.
const USER_BG: Color = Color::Indexed(159); // 256-color light cyan
const ERROR_BG: Color = Color::Red;
const INFO_BG: Color = Color::Gray;

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
    /// Streamed thinking, displayed while the display is on (ctrl+t).
    Thinking(String),
    /// A thinking block that streamed while the display was off: it shows
    /// as a single `Thinking...` placeholder line, one per block. The text
    /// is deliberately discarded rather than kept — the terminal history
    /// cannot be rewritten, so surfacing it later would land out of order.
    ThinkingHidden,
    ToolCall {
        name: String,
        arguments: String,
    },
    ToolResult {
        name: String,
        output: String,
        /// Inline images the result put into the model's context
        /// (`view-image`'s attachment), kept so `screen_records` and the
        /// live history stay replayable even with recording off. Never
        /// displayed: the transcript shows the text (a size note) only.
        images: Vec<clanky_protocol::ImagePart>,
    },
    Error(String),
    /// A UI-only note (command feedback, etc.); not sent to the model
    /// and not recorded in the session file.
    Info(String),
    /// A prompt typed while a turn was running: shown queued (with a
    /// truncated preview) until the turn ends and the full prompt is
    /// printed as [`Entry::User`] at its real place in the history.
    /// UI-only: not sent, not recorded, and not counted toward the
    /// context estimate — the message is not in the context yet.
    Queued {
        preview: String,
    },
    /// One part of the assembled system prompt, shown verbatim by
    /// `/system` (UI-only): not sent, not recorded.
    SystemPart {
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

/// Token counts a provider has actually billed for during the session,
/// fed by per-round usage reports (see [`App::on_turn_event`]).
///
/// Every chat round is its own billed request: a round's prompt report is
/// the full prompt that request charged for, not a cumulative session
/// figure, so summing reports across rounds and turns is what the
/// provider bills (each turn re-sends and re-bills the whole context).
/// `cached_tokens` is the part served from the provider's prompt cache
/// and billed at the cheaper cache-read rate instead of the input rate;
/// it is a subset of the prompt count.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BilledUsage {
    /// Prompt tokens billed, all rounds and turns (each round's request
    /// is charged its full restated prompt).
    pub prompt_tokens: u64,
    /// Completion tokens billed, all rounds and turns (always fresh).
    pub completion_tokens: u64,
    /// Portion of `prompt_tokens` served from the provider's prompt
    /// cache, billed at the cache-read rate instead of the input rate.
    pub cached_tokens: u64,
}

impl BilledUsage {
    /// Fold one round's usage report.
    fn add_round(&mut self, prompt: u64, cached: u64, completion: u64) {
        self.prompt_tokens += prompt;
        self.completion_tokens += completion;
        self.cached_tokens += cached.min(prompt);
    }
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
    /// Prompts typed while busy, sent one per turn as turns finish.
    pub pending: Vec<String>,
    /// Usage of the most recently completed chat round (not turn), for
    /// the status line: its prompt tokens are what the next request
    /// re-sends, so they drive the ctx % display.
    pub last_usage: Option<Usage>,
    /// Token counts the provider has actually billed for, across all
    /// completed turns and the current one (see [`BilledUsage`]). Restored
    /// from a resumed session's usage records.
    pub billed_usage: BilledUsage,
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
    /// Whether streamed thinking is displayed (ctrl+t). Display-only: the
    /// session file always records the full thinking text. Turning it on
    /// affects text that arrives from that point on; blocks that streamed
    /// while it was off stay collapsed as [`Entry::Thinking`] placeholders.
    show_thinking: bool,
    open: Option<OpenKind>,
    /// Project label shown at the left of the input prompt (`project`
    /// setting; `None` = no label). Display-only.
    pub project: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        Self::new(None)
    }
}

impl App {
    /// `project` is the label rendered at the left of the input prompt
    /// (the `project` setting); `None` renders the prompt unchanged.
    pub fn new(project: Option<String>) -> Self {
        Self {
            entries: Vec::new(),
            input: String::new(),
            cursor: 0,
            busy: false,
            pending: Vec::new(),
            last_usage: None,
            billed_usage: BilledUsage::default(),
            completion: None,
            printed_entries: 0,
            printed: Vec::new(),
            input_index: None,
            draft: String::new(),
            show_thinking: true,
            open: None,
            history: Vec::new(),
            input_history: Vec::new(),
            estimate_prompt_chars: 0,
            estimate_prompt: 0,
            estimate_completion_chars: 0,
            estimate_completion: 0,
            project,
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
            TurnEvent::ToolResult {
                name,
                output,
                images,
            } => {
                // The result is fed back to the model, so it counts as
                // prompt-side content until the next round's report.
                self.estimate_add(output.chars().count(), true);
                self.entries.push(Entry::ToolResult {
                    name,
                    output,
                    images,
                });
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
                self.billed_usage.add_round(
                    usage.prompt_tokens.unwrap_or(0),
                    usage.cached_tokens.unwrap_or(0),
                    usage.completion_tokens.unwrap_or(0),
                );
                // The report covers everything sent and received so far:
                // live values switch to billed-only until new content lands.
                self.estimate_reset();
            }
            // The retry notice is informational: it closes the open block
            // so the replayed round does not append to a half-written
            // answer, and shows the pause in the transcript.
            TurnEvent::Retrying {
                attempt,
                max_retries,
                delay,
                message,
            } => {
                self.open = None;
                self.entries.push(Entry::Info(format!(
                    "↻ {message} — retrying in {:.1}s ({attempt}/{max_retries})",
                    delay.as_secs_f64()
                )));
            }
        }
    }

    /// Finalize the turn: surface the error, if any. Usage was already
    /// applied round-by-round via [`TurnEvent::Usage`]; this closes the
    /// turn's billing (one full charge for the final round's prompt).
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

    /// ctrl+t: toggle whether streamed thinking is displayed. Display-only:
    /// the session file still records every thinking block. The change takes
    /// effect from here on, because the printed terminal history cannot be
    /// rewritten — turning the display off ends the current block with a
    /// placeholder line, and turning it on resumes printing at the next
    /// delta (possibly mid-sentence, as a new entry below what was printed).
    pub fn toggle_thinking_display(&mut self) {
        self.show_thinking = !self.show_thinking;
        if self.open != Some(OpenKind::Thinking) {
            return;
        }
        if self.show_thinking {
            // Resume as a fresh displayed entry at the next delta.
            self.open = None;
        } else {
            // Collapse the rest of the block right away: the displayed
            // text ends and one placeholder marks the block. `open` stays
            // `Thinking`, so further deltas append nothing.
            self.entries.push(Entry::ThinkingHidden);
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
        if kind == OpenKind::Thinking && !self.show_thinking {
            // Display off: collapse the block into one placeholder and
            // discard the text (it must not resurface out of order later).
            if self.open != Some(OpenKind::Thinking) {
                self.entries.push(Entry::ThinkingHidden);
            }
            self.open = Some(OpenKind::Thinking);
            return;
        }
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

    /// Derive session records from the transcript entries — what the
    /// screen showed. Used by `/md` when there is no session file
    /// (recording off, or nothing recorded yet): the export then reflects
    /// the visible conversation, not an empty file that was never made.
    ///
    /// UI-only entries (`Info`, `Queued`, `SystemPart`) and
    /// `ThinkingHidden` placeholders (their text was discarded) are
    /// skipped. Consecutive assistant text and tool-call entries merge
    /// into one [`Record::Assistant`] with the calls attached, mirroring
    /// how a live round is recorded.
    pub fn screen_records(&self) -> Vec<Record> {
        let mut records: Vec<Record> = Vec::new();
        let mut round: Option<(String, Vec<ToolCall>)> = None;
        let flush_round = |records: &mut Vec<Record>,
                           round: &mut Option<(String, Vec<ToolCall>)>| {
            if let Some((text, calls)) = round.take() {
                records.push(Record::Assistant { text, calls });
            }
        };
        for entry in &self.entries {
            match entry {
                Entry::User(text) => {
                    flush_round(&mut records, &mut round);
                    records.push(Record::User { text: text.clone() });
                }
                Entry::Thinking(text) => {
                    flush_round(&mut records, &mut round);
                    records.push(Record::Thinking { text: text.clone() });
                }
                Entry::Assistant(text) => {
                    let r = round.get_or_insert_with(|| (String::new(), Vec::new()));
                    if !r.0.is_empty() {
                        r.0.push('\n');
                    }
                    r.0.push_str(text);
                }
                Entry::ToolCall { name, arguments } => {
                    let r = round.get_or_insert_with(|| (String::new(), Vec::new()));
                    let id = format!("screen-{}", r.1.len());
                    let arguments = serde_json::from_str(arguments)
                        .unwrap_or_else(|_| serde_json::Value::String(arguments.clone()));
                    r.1.push(ToolCall {
                        id,
                        name: name.clone(),
                        arguments,
                    });
                }
                Entry::ToolResult {
                    name,
                    output,
                    images,
                } => {
                    flush_round(&mut records, &mut round);
                    records.push(Record::ToolResult {
                        name: name.clone(),
                        output: output.clone(),
                        images: (!images.is_empty()).then(|| images.clone()),
                    });
                }
                Entry::Error(message) => {
                    flush_round(&mut records, &mut round);
                    records.push(Record::Error {
                        message: message.clone(),
                    });
                }
                Entry::Info(_)
                | Entry::Queued { .. }
                | Entry::SystemPart { .. }
                | Entry::ThinkingHidden => {}
            }
        }
        flush_round(&mut records, &mut round);
        records
    }

    /// Replace the transcript with a loaded session's records and return
    /// the rebuilt conversation history (system context excluded; the
    /// caller prepends fresh system messages).
    pub fn restore(&mut self, records: &[Record]) -> Vec<ChatMessage> {
        self.entries.clear();
        self.printed_entries = 0;
        self.printed.clear();
        self.last_usage = None;
        self.billed_usage = BilledUsage::default();
        self.estimate_reset();
        self.open = None;
        for record in records {
            match record {
                Record::User { text } => {
                    self.entries.push(Entry::User(text.clone()));
                    self.input_history.push(text.clone());
                }
                Record::Thinking { text } => {
                    // Past thinking is only shown while the display is on;
                    // a resumed session with the display off keeps the
                    // collapsed placeholders (the session file still has
                    // the text, e.g. for `/md --thinking`).
                    if self.show_thinking {
                        self.entries.push(Entry::Thinking(text.clone()));
                    } else {
                        self.entries.push(Entry::ThinkingHidden);
                    }
                }
                Record::Assistant { text, calls } => {
                    self.entries.push(Entry::Assistant(text.clone()));
                    for call in calls {
                        self.entries.push(Entry::ToolCall {
                            name: call.name.clone(),
                            arguments: call.arguments.to_string(),
                        });
                    }
                }
                Record::ToolResult {
                    name,
                    output,
                    images,
                } => self.entries.push(Entry::ToolResult {
                    name: name.clone(),
                    output: output.clone(),
                    images: images.clone().unwrap_or_default(),
                }),
                Record::Error { message } => self.entries.push(Entry::Error(message.clone())),
                Record::Usage {
                    prompt_tokens,
                    completion_tokens,
                } => {
                    self.last_usage = Some(Usage {
                        prompt_tokens: *prompt_tokens,
                        completion_tokens: *completion_tokens,
                        cached_tokens: None,
                    });
                    self.billed_usage.add_round(
                        prompt_tokens.unwrap_or(0),
                        0,
                        completion_tokens.unwrap_or(0),
                    );
                }
                // Metadata, not conversation content: usage is applied
                // round-by-round through [`TurnEvent::Usage`], the stop
                // reason has no transcript entry of its own, and a
                // settings snapshot is a settings fact (already visible
                // in the status line), not a transcript entry.
                Record::StopReason { .. } | Record::Settings { .. } => {}
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

    /// Insert `text` at the caret (a bracketed paste, ctrl+j's newline);
    /// the caret lands after it. Carriage returns are dropped: terminals
    /// deliver pastes with `\r\n` line endings (and a stray `\r` would
    /// only move the cursor invisibly), while `\n` becomes a real input
    /// line — so a multi-line paste is inserted whole and only a really
    /// pressed Enter submits.
    pub fn insert_str(&mut self, text: &str) {
        let cleaned: String = text.chars().filter(|&c| c != '\r').collect();
        self.input.insert_str(self.cursor, &cleaned);
        self.cursor += cleaned.len();
        // The paste may have landed inside (or shifted) the completed
        // token: the popup's prefix/suffix no longer describe `input`, so
        // cycling on the next Tab would overwrite the pasted text.
        self.completion = None;
    }

    pub fn backspace(&mut self) {
        if let Some(pos) = self.prev_boundary() {
            self.input.replace_range(pos..self.cursor, "");
            self.cursor = pos;
        }
    }

    /// ctrl+u: clear from the cursor to the beginning of the line.
    pub fn clear_to_start(&mut self) {
        self.input.replace_range(..self.cursor, "");
        self.cursor = 0;
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

    // --- multi-line caret movement ------------------------------------------

    /// How many physical rows the input renders as: the segments between
    /// newlines, plus the empty final row a trailing newline yields
    /// (the same split the input renderer draws, see `super::screen`).
    fn line_count(&self) -> usize {
        self.input.split('\n').count()
    }

    /// The row (0-based) the caret renders on. A caret sitting on a
    /// newline belongs to the end of the row above (the newline ends
    /// that row), so the newlines *strictly before* the caret count.
    fn caret_row(&self) -> usize {
        self.input[..self.cursor]
            .bytes()
            .filter(|&b| b == b'\n')
            .count()
    }

    /// Byte offset where physical row `row` starts: one past the newline
    /// ending its predecessor; row 0 starts at 0.
    fn row_start(&self, row: usize) -> usize {
        if row == 0 {
            return 0;
        }
        let mut seen = 0;
        for (i, b) in self.input.bytes().enumerate() {
            if b == b'\n' {
                seen += 1;
                if seen == row {
                    return i + 1;
                }
            }
        }
        self.input.len()
    }

    /// Byte length of physical row `row` (its newline excluded).
    fn row_len(&self, row: usize) -> usize {
        let start = self.row_start(row);
        self.input[start..]
            .find('\n')
            .unwrap_or(self.input.len() - start)
    }

    /// Byte offset where the caret's physical line starts.
    fn line_start(&self) -> usize {
        self.input[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    /// Byte offset where the caret's physical line ends (its newline
    /// excluded; the buffer's end when it is the last line).
    fn line_end(&self) -> usize {
        self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |i| self.cursor + i)
    }

    /// Move the caret onto physical row `row`, keeping its byte offset
    /// within the row when that row is long enough, else parking at the
    /// row's end (never inside a multi-byte char).
    fn move_to_row(&mut self, row: usize) {
        let start = self.row_start(row);
        let column = self.cursor - self.line_start();
        let mut target = start + column.min(self.row_len(row));
        while target > start && !self.input.is_char_boundary(target) {
            target -= 1;
        }
        self.cursor = target;
    }

    /// ↑ inside the buffer: onto the row above, keeping the offset within
    /// the row. Returns the rows moved; `0` = already on the top row (the
    /// caller then recalls the previous prompt). A single-row buffer is
    /// always at its top row: plain single-line ↑ behavior.
    pub fn cursor_up(&mut self) -> usize {
        let row = self.caret_row();
        if row == 0 {
            return 0;
        }
        self.move_to_row(row - 1);
        1
    }

    /// ↓ inside the buffer: onto the row below. Returns the rows moved;
    /// `0` = already on the bottom row (the caller then steps forward
    /// through the prompt history).
    pub fn cursor_down(&mut self) -> usize {
        let row = self.caret_row();
        if row + 1 >= self.line_count() {
            return 0;
        }
        self.move_to_row(row + 1);
        1
    }

    /// ctrl+a: to the beginning of the current line.
    pub fn cursor_line_home(&mut self) {
        self.cursor = self.line_start();
    }

    /// ctrl+e: to the end of the current line (before its newline).
    /// Returns whether the caret was already at the very end of the
    /// buffer — readline semantics let the caller then open `$EDITOR`.
    pub fn cursor_line_end(&mut self) -> bool {
        let at_buffer_end = self.cursor == self.input.len();
        self.cursor = self.line_end();
        at_buffer_end
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

    /// Apply a full-width background to already-wrapped display lines:
    /// every span is patched to add `bg`, and each row in the block is
    /// padded to `width` with bg-only spaces (see `screen::print_line`
    /// for why the padding lives in the spans rather than in the
    /// renderer). The block runs from the first row through the last
    /// populated row: paragraph breaks inside the entry are filled too,
    /// so the whole message reads as one seamless background. Only the
    /// entry's trailing blank rows (the separator appended after the
    /// block) stay uncolored, so a block never touches the entry below
    /// it.
    ///
    /// The caller must pass the same width the lines were wrapped at:
    /// padding wider would overflow the renderer's truncation (the row
    /// count math depends on rows never wrapping).
    fn with_block_bg(lines: Vec<Line<'static>>, bg: Color, width: u16) -> Vec<Line<'static>> {
        let width = width.max(1) as usize;
        let bg_style = Style::new().bg(bg);
        // The block ends at the last populated row; blanks after it are
        // the entry's trailing separator(s) and stay uncolored.
        let last_text = lines
            .iter()
            .rposition(|line| line.spans.iter().any(|s| !s.content.trim().is_empty()));
        lines
            .into_iter()
            .enumerate()
            .map(|(i, mut line)| {
                let in_block = last_text.is_some_and(|last| i <= last);
                let mut text_width = 0usize;
                for span in &mut line.spans {
                    span.style = span.style.patch(bg_style);
                    text_width += span.content.width();
                }
                let has_text = line.spans.iter().any(|s| !s.content.trim().is_empty());
                if in_block {
                    if has_text {
                        let text_width = text_width.min(width);
                        if text_width < width {
                            let pad = " ".repeat(width - text_width);
                            line.spans.push(Span::styled(pad, bg_style));
                        }
                    } else {
                        // A paragraph break inside the entry: one
                        // full-width row of background (no text), so
                        // the block stays seamless across paragraphs.
                        line.spans = vec![Span::styled(" ".repeat(width), bg_style)];
                    }
                } else {
                    // Trailing blanks (the separator line at the end of
                    // every entry) carry no background: a block never
                    // touches the entry below it.
                    for span in &mut line.spans {
                        span.style.bg = None;
                    }
                }
                line
            })
            .collect()
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
                // The bg goes on the source lines: wrapping preserves
                // per-character styles, so wrapped continuation rows
                // inherit it.
                for line in &mut src {
                    for span in &mut line.spans {
                        span.style = span.style.patch(Style::new().bg(USER_BG));
                    }
                }
                let wrapped = markdown::wrap(&src, width);
                lines.extend(Self::with_block_bg(wrapped, USER_BG, width));
                lines.push(Line::default());
            }
            // Assistant text: internal blank lines (paragraph breaks) are
            // kept, but trailing newlines in the raw text would stack with
            // the separator blank line below, so trim them first: the
            // entry always ends with exactly one blank line.
            Entry::Assistant(text) => {
                let trimmed = text.trim_end_matches(['\n', '\r']);
                lines.extend(markdown::wrap(
                    &markdown::render(trimmed, Style::new(), width),
                    width,
                ));
                lines.push(Line::default());
            }
            Entry::Thinking(text) => {
                let base = Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                lines.extend(markdown::wrap(&markdown::render(text, base, width), width));
                lines.push(Line::default());
            }
            // A collapsed thinking block: one placeholder line, so the
            // transcript shows that reasoning happened at this point
            // without ever printing the text.
            Entry::ThinkingHidden => {
                let base = Style::new()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                lines.push(Line::from(Span::styled("Thinking...", base)));
                lines.push(Line::default());
            }
            Entry::ToolCall { name, arguments } => {
                // Display a preview: very long arguments are cut (the
                // `timeout_seconds` override survives the cut); the
                // entry itself keeps the full text for resume. Arguments
                // may contain newlines (pretty-printed JSON): emit one
                // source Line per argument line. Raw `\n` characters
                // must never reach the terminal: LF moves the cursor
                // down without returning to column 0, so every line
                // would start below the end of the previous one.
                let shown = crate::tools::truncate_call_arguments(
                    arguments,
                    crate::tools::CALL_PREVIEW_CHARS,
                );
                let mut body = shown.lines();
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
                // Capped: the arguments preview keeps the transcript
                // height bounded when the cut text still wraps widely
                // (prettified JSON with many short lines).
                lines.extend(markdown::wrap_capped(
                    &src,
                    width,
                    TOOL_PREVIEW_LINES,
                    Style::new().fg(Color::DarkGray),
                ));
                lines.push(Line::default());
            }
            Entry::ToolResult {
                name,
                output,
                images: _,
            } => {
                let failed = crate::tools::is_failed_result(output);
                let style = if failed {
                    Style::new().fg(Color::Red)
                } else {
                    Style::new().fg(Color::DarkGray)
                };
                let preview_style = if failed {
                    Style::new().fg(Color::Red)
                } else {
                    // Matches inline code rendering (see markdown.rs).
                    Style::new().fg(Color::Cyan)
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
                // Capped: the line count of the preview is bounded even
                // when a single output line wraps into many rows, so the
                // entry's height stays predictable for the scroll math.
                lines.extend(markdown::wrap_capped(
                    &src,
                    width,
                    TOOL_PREVIEW_LINES,
                    style,
                ));
                lines.push(Line::default());
            }
            // Error/info text may contain newlines; route it through
            // `wrap_text` so they split into separate rows (see the
            // `Entry::ToolResult` note on raw LF reaching the terminal).
            Entry::Error(message) => {
                let mut text = Self::with_block_bg(
                    markdown::wrap_text(
                        &format!("✗ {message}"),
                        Style::new().fg(Color::White).bg(ERROR_BG),
                        width,
                    ),
                    ERROR_BG,
                    width,
                );
                lines.append(&mut text);
                lines.push(Line::default());
            }
            Entry::Info(message) => {
                let mut text = Self::with_block_bg(
                    markdown::wrap_text(
                        &format!("· {message}"),
                        Style::new().fg(Color::Black).bg(INFO_BG),
                        width,
                    ),
                    INFO_BG,
                    width,
                );
                lines.append(&mut text);
                lines.push(Line::default());
            }
            Entry::Queued { preview } => {
                let style = Style::new().fg(Color::Yellow);
                lines.extend(markdown::wrap_text(
                    &format!("○ queued: {preview}"),
                    style,
                    width,
                ));
                lines.push(Line::default());
            }
            // Show the part exactly as it is sent to the model: the
            // content already carries its own provenance heading.
            Entry::SystemPart { content } => {
                // The user's terminal PS1 blue (ANSI 0;34): keep the
                // system prompt visually kin to the prompt they know.
                let dimmed = Style::new().fg(Color::Blue);
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
/// Shared with pipe mode so stderr previews match the transcript.
fn preview(output: &str) -> String {
    crate::tools::preview_result(output, TOOL_PREVIEW_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_deltas_form_one_assistant_entry() {
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::Text { delta: "he".into() });
        app.on_turn_event(TurnEvent::Text {
            delta: "llo".into(),
        });
        assert_eq!(app.entries, vec![Entry::Assistant("hello".into())]);
    }

    /// Every entry ends with exactly one blank separator line; trailing
    /// newlines in the assistant text itself are trimmed first, so they
    /// never stack with the separator into 2-3 blank rows.
    #[test]
    fn each_entry_ends_with_one_blank_line() {
        let mut app = App::new(None);
        app.push_user("hi");
        app.on_turn_event(TurnEvent::Text {
            delta: "answer\n\n\n".into(),
        });
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: "{}".into(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "out".into(),
            images: Vec::new(),
        });
        app.entries.push(Entry::Error("boom".into()));
        app.entries.push(Entry::Info("note".into()));
        app.entries.push(Entry::SystemPart {
            content: "part".into(),
        });

        for (index, label) in [
            (0, "user"),
            (1, "assistant"),
            (2, "tool call"),
            (3, "tool result"),
            (4, "error"),
            (5, "info"),
            (6, "system part"),
        ] {
            let lines = app.entry_lines(index, 80);
            let last = lines.last().unwrap_or_else(|| panic!("{label}: no lines"));
            assert!(
                last.spans.is_empty() && last.style == Style::new(),
                "{label}: last line is not blank: {last:?}"
            );
        }

        // The assistant entry specifically: no blank rows between the
        // last content row and the separator (trailing newlines trimmed).
        let lines = app.entry_lines(1, 80);
        assert_eq!(lines.len(), 2, "content row + one blank: {lines:?}");
    }

    #[test]
    fn thinking_then_text_split_entries() {
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::Thinking {
            delta: "hmm".into(),
        });
        app.on_turn_event(TurnEvent::Text { delta: "hi".into() });
        assert_eq!(
            app.entries,
            vec![Entry::Thinking("hmm".into()), Entry::Assistant("hi".into())]
        );

        // With the display off, the same stream collapses to placeholders.
        let mut app = App::new(None);
        app.toggle_thinking_display();
        app.on_turn_event(TurnEvent::Thinking {
            delta: "hmm".into(),
        });
        app.on_turn_event(TurnEvent::Thinking {
            delta: " more".into(),
        });
        app.on_turn_event(TurnEvent::Text { delta: "hi".into() });
        assert_eq!(
            app.entries,
            vec![Entry::ThinkingHidden, Entry::Assistant("hi".into())]
        );
    }

    #[test]
    fn hidden_thinking_interleaves_with_tools_in_order() {
        // The user-visible shape with the display off:
        //   Thinking...
        //   ● bash {...}
        //   └─ bash: ...
        //   Thinking...
        let mut app = App::new(None);
        app.toggle_thinking_display();
        app.on_turn_event(TurnEvent::Thinking {
            delta: "reason 1".into(),
        });
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "a".into(),
            images: Vec::new(),
        });
        app.on_turn_event(TurnEvent::Thinking {
            delta: "reason 2".into(),
        });
        assert_eq!(
            app.entries,
            vec![
                Entry::ThinkingHidden,
                Entry::ToolCall {
                    name: "bash".into(),
                    arguments: r#"{"command":"ls"}"#.into()
                },
                Entry::ToolResult {
                    name: "bash".into(),
                    output: "a".into(),
                    images: Vec::new(),
                },
                Entry::ThinkingHidden,
            ],
            "each block is one placeholder, in event order"
        );
    }

    #[test]
    fn tool_call_closes_the_streaming_entry() {
        // Text after a tool call must open a new assistant entry so it
        // renders *below* the tool activity, not above it.
        let mut app = App::new(None);
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
            images: Vec::new(),
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
                    output: "a\nb".into(),
                    images: Vec::new(),
                },
                Entry::Assistant("done".into()),
            ]
        );
    }

    #[test]
    fn retry_notice_closes_the_block_and_is_informational() {
        use std::time::Duration;
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::Text {
            delta: "par".into(),
        });
        // A retry notice must not append to the half-written block, and it
        // shows as an info line rather than an error.
        app.on_turn_event(TurnEvent::Retrying {
            attempt: 2,
            max_retries: 5,
            delay: Duration::from_secs(2),
            message: "rateLimit: 429: Model busy".into(),
        });
        match app.entries.last() {
            Some(Entry::Info(text)) => {
                assert!(text.contains("429: Model busy"), "{text}");
                assert!(text.contains("2/5"), "{text}");
                assert!(text.contains("2.0s"), "{text}");
            }
            other => panic!("expected an info entry, got {other:?}"),
        }
        assert!(app.open.is_none());
    }

    #[test]
    fn billed_usage_accumulates_per_round_and_done_surfaces_errors() {
        let mut app = App::new(None);
        app.busy = true;
        let round1 = Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(3),
            cached_tokens: None,
        };
        // Round 2's report restates the whole prompt: only the 2 new
        // prompt tokens are an added charge.
        let round2 = Usage {
            prompt_tokens: Some(12),
            completion_tokens: Some(4),
            cached_tokens: None,
        };
        // A two-round turn: usage lands as each round completes. Each
        // round is its own billed request, so reports sum directly.
        app.on_turn_event(TurnEvent::Usage { usage: round1 });
        app.on_turn_event(TurnEvent::Usage { usage: round2 });
        assert_eq!(app.last_usage, Some(round2), "last round's own usage");
        assert_eq!(
            app.billed_usage.prompt_tokens, 22,
            "each round's request is billed its full prompt"
        );
        assert_eq!(app.billed_usage.completion_tokens, 7);

        // done only ends the turn; usage was already applied.
        let before = app.billed_usage;
        app.on_turn_done(&Ok(TurnOutput {
            text: "hi".into(),
            finish_reason: clanky_protocol::FinishReason::Stop,
            usage: None,
        }));
        assert!(!app.busy);
        assert_eq!(app.last_usage, Some(round2));
        assert_eq!(app.billed_usage, before, "done must not re-add usage");

        app.busy = true;
        app.on_turn_done(&Err(crate::error::Error::NoModel));
        assert!(!app.busy);
        assert!(matches!(app.entries.last(), Some(Entry::Error(_))));
    }

    #[test]
    fn cached_tokens_track_the_cache_served_prompt_portion() {
        let mut app = App::new(None);
        // 1000 prompt tokens, 800 from cache, 50 completion.
        app.on_turn_event(TurnEvent::Usage {
            usage: Usage {
                prompt_tokens: Some(1000),
                completion_tokens: Some(50),
                cached_tokens: Some(800),
            },
        });
        assert_eq!(
            app.billed_usage,
            BilledUsage {
                prompt_tokens: 1000,
                completion_tokens: 50,
                cached_tokens: 800,
            }
        );
        // A cached count larger than the prompt is clamped (defensive:
        // providers should not report it, but a bad number must not
        // price negative input tokens).
        app.on_turn_event(TurnEvent::Usage {
            usage: Usage {
                prompt_tokens: Some(10),
                completion_tokens: Some(5),
                cached_tokens: Some(999),
            },
        });
        assert_eq!(app.billed_usage.prompt_tokens, 1010);
        assert_eq!(app.billed_usage.cached_tokens, 810, "clamped to the prompt");
    }

    #[test]
    fn estimates_streamed_text_and_resets_on_usage() {
        let mut app = App::new(None);
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
                cached_tokens: None,
            },
        });
        assert_eq!(app.estimated_tokens(), (0, 0));
        assert_eq!(app.billed_usage.prompt_tokens, 100);
        assert_eq!(app.billed_usage.completion_tokens, 50);

        // Streaming again starts from a clean estimate.
        app.on_turn_event(TurnEvent::Text {
            delta: "z".repeat(32),
        });
        assert_eq!(app.estimated_tokens(), (0, 10));
    }

    #[test]
    fn user_prompt_and_tool_results_estimate_into_the_prompt_pool() {
        let mut app = App::new(None);
        // 96 chars -> 3 steps * 32 chars / 3 = 32.
        app.push_user(&"x".repeat(96));
        assert_eq!(app.estimated_tokens(), (32, 0));

        // A tool result is fed back to the model: prompt-side too.
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "y".repeat(96),
            images: Vec::new(),
        });
        assert_eq!(app.estimated_tokens(), (64, 0));

        // A usage report covers everything sent and received so far.
        app.on_turn_event(TurnEvent::Usage {
            usage: Usage {
                prompt_tokens: Some(500),
                completion_tokens: Some(20),
                cached_tokens: None,
            },
        });
        assert_eq!(app.estimated_tokens(), (0, 0));
        assert_eq!(app.billed_usage.prompt_tokens, 500);
    }

    #[test]
    fn estimate_clears_when_a_turn_ends_without_usage() {
        let mut app = App::new(None);
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
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: r#"{"command":"false"}"#.into(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "out\n[exit status: 1]".into(),
            images: Vec::new(),
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
            images: Vec::new(),
        });
        let lines = app.entry_lines(2, 80);
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::DarkGray));
        let last = lines[0].spans.last().unwrap();
        assert_eq!(last.style.fg, Some(Color::Cyan));
        assert_eq!(last.content, "ok");

        // Tool-call failures (ERROR: prefix) are failures too.
        app.entries.push(Entry::ToolResult {
            name: "nope".into(),
            output: "ERROR: unknown tool: nope".into(),
            images: Vec::new(),
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
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "one\ntwo\nthree".into(),
            images: Vec::new(),
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
        let mut app = App::new(None);
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
        let mut app = App::new(None);
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
    fn up_down_move_within_the_rows_of_the_buffer() {
        let mut app = App::new(None);
        app.input = "aaaa\nbbbb\ncccc".into();
        app.cursor = 5; // start of "bbbb"

        // Up from the middle row keeps the byte offset within the row
        // (the caret sits at column 0 of "bbbb", so it stays at column 0).
        assert_eq!(app.cursor_up(), 1);
        assert_eq!(app.cursor, 0);
        // Clamped when the row above is shorter.
        app.input = "a\nlonger row".into();
        app.cursor = app.input.find("row").unwrap();
        assert_eq!(app.cursor_up(), 1);
        assert_eq!(app.cursor, 1, "end of the short row above");
        // On the top row ↑ reports no movement: the caller recalls.
        assert_eq!(app.cursor_up(), 0);

        app.cursor = 0;
        assert_eq!(app.cursor_down(), 1);
        assert_eq!(app.cursor, 2, "start of the row below");
        // On the bottom row ↓ reports no movement: the caller walks on.
        assert_eq!(app.cursor_down(), 0);
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn caret_row_tracks_newlines_strictly_before_the_caret() {
        let mut app = App::new(None);
        // A caret on a newline belongs to the end of the row above.
        app.input = "one\ntwo".into();
        app.cursor = 3;
        assert_eq!(app.cursor_up(), 0);
        assert_eq!(app.cursor, 3, "row above the newline is the top row");
        app.cursor_down();
        assert_eq!(app.cursor, 7, "onto the second row");
    }

    #[test]
    fn a_trailing_newline_makes_the_last_row_editable() {
        let mut app = App::new(None);
        app.input = "hello\n".into();
        app.cursor = 6; // end of the buffer, on the empty final row
        // The renderer maps this caret to (row 1, column 0); ↑ keeps the
        // column, landing at the start of "hello".
        assert_eq!(app.cursor_up(), 1);
        assert_eq!(app.cursor, 0, "column 0 of the row above");
        app.cursor_down();
        assert_eq!(app.cursor, 6, "back onto the empty final row");
        assert_eq!(app.cursor_down(), 0, "bottom row: caller handles ↓");
    }

    #[test]
    fn ctrl_a_and_ctrl_e_move_within_the_current_line() {
        let mut app = App::new(None);
        app.input = "first\nsecond".into();
        app.cursor = 9; // inside "second"
        app.cursor_line_home();
        assert_eq!(app.cursor, 6);
        app.cursor_line_end();
        assert_eq!(app.cursor, 12, "before nothing — line end");
        // On the last row, ctrl+e at the buffer end opens $EDITOR.
        assert!(app.cursor_line_end());
        // Mid-buffer, it never reports buffer end.
        app.cursor = 2;
        assert!(!app.cursor_line_end());
        assert_eq!(app.cursor, 5, "before the newline");
    }

    #[test]
    fn a_paste_is_inserted_whole_without_carriage_returns() {
        let mut app = App::new(None);
        app.input = "keep ".into();
        app.cursor = 5;
        app.insert_str("one\r\ntwo\n");
        assert_eq!(app.input, "keep one\ntwo\n");
        assert_eq!(app.cursor, app.input.len());
        // UTF-8 safety: the caret lands on a char boundary of the insert.
        app.cursor = 0;
        app.insert_str("é");
        assert_eq!(app.input, "ékeep one\ntwo\n");
        assert_eq!(app.cursor, 2);
    }

    #[test]
    fn take_input_clears_and_requires_text() {
        let mut app = App::new(None);
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
        let mut app = App::new(None);
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
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: json!({"command": "ls"}).to_string(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "file.txt".into(),
            images: Vec::new(),
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
    fn long_tool_call_arguments_are_cut_for_display_only() {
        let long = "z".repeat(2_000);
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: json!({"command": long, "timeout_seconds": 90}).to_string(),
        });

        // Wide enough that the cut call fits under the cap: the cut
        // note and the re-attached timeout are both visible.
        let lines = app.entry_lines(0, 1_000);
        let shown: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(shown.contains("… (truncated)"), "{shown}");
        assert!(shown.contains("timeout_seconds\":90"), "{shown}");

        // At transcript width the call wraps past the preview cap, so
        // the entry's row count is bounded (8 rows + the separator).
        let lines = app.transcript_lines(80);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(texts.len(), TOOL_PREVIEW_LINES + 1, "{texts:?}");
        let shown: String = texts.concat();
        assert!(shown.len() < 1_000, "display is short: {}", shown.len());

        // The entry keeps the full text so resume restores it verbatim.
        let full = match &app.entries[0] {
            Entry::ToolCall { arguments, .. } => arguments.clone(),
            other => panic!("expected a tool call entry, got {other:?}"),
        };
        assert!(full.contains(&long));
    }

    #[test]
    fn assistant_table_fits_width_without_rewrapping() {
        use unicode_width::UnicodeWidthStr as _;
        let mut app = App::new(None);
        app.entries.push(Entry::Assistant(
            "| aaa | bbb | ccc |\n| -- | -- | -- |\n| longwordhere | x | y |".into(),
        ));
        let lines = app.entry_lines(0, 40);
        // Every table row keeps the full box borders: the later generic
        // `wrap()` must not re-split any row the table renderer emitted
        // (it would break the box into border fragments).
        let table_rows: Vec<&Line<'static>> = lines
            .iter()
            .filter(|l| {
                let t: String = l.spans.iter().map(|s| s.content.to_string()).collect();
                t.contains('┌') || t.contains('└') || t.contains('│') || t.contains('├')
            })
            .collect();
        assert_eq!(table_rows.len(), 5, "a bordered box: {lines:?}");
        for line in &table_rows {
            let t: String = line.spans.iter().map(|s| s.content.to_string()).collect();
            let w: usize = t.width();
            assert!(w <= 40, "row of width {w}: {t:?}");
            // A border fragment from re-wrapping would start mid-box.
            assert!(
                t.starts_with('┌')
                    || t.starts_with('└')
                    || t.starts_with('│')
                    || t.starts_with('├'),
                "row starts with a border char: {t:?}"
            );
        }
        assert_eq!(lines.last().unwrap().spans.len(), 0, "trailing separator");
    }

    #[test]
    fn clear_empties_the_transcript_and_print_bookkeeping() {
        let mut app = App::new(None);
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
        let mut app = App::new(None);
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
                images: None,
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
        let mut app = App::new(None);
        app.push_user("old");
        let history = app.restore(&records);
        assert_eq!(
            app.entries.len(),
            5,
            "user, assistant, call, result, assistant"
        );
        assert_eq!(app.last_usage.and_then(|u| u.completion_tokens), Some(2));
        assert_eq!(
            app.billed_usage,
            BilledUsage {
                prompt_tokens: 9,
                completion_tokens: 2,
                cached_tokens: 0,
            },
            "resume rebuilds the billed totals from the usage records"
        );
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
    fn user_error_info_rows_carry_block_bg() {
        let mut app = App::new(None);
        app.push_user("hello");
        app.entries.push(Entry::Error("boom".into()));
        app.entries.push(Entry::Info("renamed".into()));

        // User: bg on every populated row, none on the separator.
        let lines = app.entry_lines(0, 40);
        assert!(lines.iter().any(|l| {
            l.spans.iter().any(|s| s.style.bg == Some(USER_BG)) && !l.spans.is_empty()
        }));
        let last = lines.last().unwrap();
        assert!(last.spans.is_empty(), "separator stays unstyled: {last:?}");

        // Error: white on red, full-width fill on populated rows.
        let lines = app.entry_lines(1, 40);
        for line in lines.iter().filter(|l| !l.spans.is_empty()) {
            assert!(
                line.spans.iter().all(|s| s.style.bg == Some(ERROR_BG)),
                "error row bg: {line:?}"
            );
            assert!(
                line.spans.iter().any(|s| s.style.fg == Some(Color::White)),
                "error row fg: {line:?}"
            );
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert_eq!(w, 40, "error row padded to width: {line:?}");
        }

        // Info: black on gray.
        let lines = app.entry_lines(2, 40);
        for line in lines.iter().filter(|l| !l.spans.is_empty()) {
            assert!(
                line.spans.iter().all(|s| s.style.bg == Some(INFO_BG)),
                "info row bg: {line:?}"
            );
            assert!(
                line.spans.iter().any(|s| s.style.fg == Some(Color::Black)),
                "info row fg: {line:?}"
            );
        }
    }

    #[test]
    fn user_bg_survives_wrapping() {
        let mut app = App::new(None);
        // One long source line: wraps into several rows at width 10.
        app.push_user("aaa bbb ccc ddd eee");
        let lines = app.entry_lines(0, 10);
        let populated: Vec<_> = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| !s.content.trim().is_empty()))
            .collect();
        assert!(
            populated.len() >= 3,
            "expected several wrapped rows: {lines:?}"
        );
        for line in populated {
            assert!(
                line.spans.iter().all(|s| s.style.bg == Some(USER_BG)),
                "every wrapped row keeps the bg: {line:?}"
            );
            let w: usize = line.spans.iter().map(|s| s.content.width()).sum();
            assert_eq!(w, 10, "wrapped row padded to width: {line:?}");
        }
        // The wrap separator (last line) stays unstyled.
        assert!(lines.last().unwrap().spans.is_empty());
    }

    #[test]
    fn internal_blank_rows_are_filled_so_the_block_is_seamless() {
        let mut app = App::new(None);
        app.push_user("para one\n\npara two");
        let lines = app.entry_lines(0, 40);
        // Rows: "❯ para one" + pad, a full-width filled paragraph break,
        // "  para two" + pad, and the uncolored separator.
        assert_eq!(lines.len(), 4, "rows: {lines:?}");
        let middle = &lines[1];
        assert!(
            middle.spans.iter().all(|s| s.style.bg == Some(USER_BG)),
            "paragraph break is filled: {middle:?}"
        );
        assert!(
            middle.spans.iter().all(|s| s.content.trim().is_empty()),
            "the break row has no text: {middle:?}"
        );
        let w: usize = middle.spans.iter().map(|s| s.content.width()).sum();
        assert_eq!(w, 40, "break row fills the width: {middle:?}");
        // The trailing separator stays uncolored.
        assert!(lines[3].spans.is_empty(), "separator: {:?}", lines[3]);
    }

    #[test]
    fn info_entries_render_dimmed() {
        let mut app = App::new(None);
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
        let mut app = App::new(None);
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
        let mut app = App::new(None);
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
        let mut app = App::new(None);
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
        let mut app = App::new(None);
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
        let mut app = App::new(None);
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
        let mut app = App::new(None);
        app.restore(&records);
        app.history_up();
        assert_eq!(app.input, "two");
        app.history_up();
        assert_eq!(app.input, "one");
    }

    #[test]
    fn system_parts_render_with_label_and_content() {
        let mut app = App::new(None);
        app.entries.push(Entry::Info("2 part(s)".into()));
        app.entries.push(Entry::SystemPart {
            content: "# Context — skill review\n\nCheck the diffs.".into(),
        });
        let lines = app.transcript_lines(80);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            texts.iter().any(|t| t.contains("# Context — skill review")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("Check the diffs.")));
    }
}
