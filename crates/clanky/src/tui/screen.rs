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
//! Two terminal-level rules keep the region stable:
//!
//! - A fresh anchor (startup, resize, returning from `$EDITOR`) positions
//!   the redrawable block with absolute cursor addressing (CUP), so it
//!   *ends* on the bottom row; CUP is clamped by every terminal and never
//!   scrolls. (The old "cursor next line with a huge count" printed at
//!   the bottom edge and scrolled, which some terminals — VTE — turn
//!   into a duplicated status line.) Steady-state redraws are
//!   cursor-relative instead: printing a growing transcript scrolls the
//!   screen, which invalidates every absolute row address, so only
//!   relative moves can track a live tail across scrolls.
//! - No row is ever painted into the terminal's last column. Writing it
//!   sets the pending-wrap flag, and a following CR/LF then scrolls the
//!   screen one row (duplicate status lines after `$EDITOR`, status lines
//!   streaming upward on resize); GNU screen also defers displaying a
//!   character written there until the screen scrolls, so the last column
//!   would look invisible. Reserving one blank column sidesteps the
//!   whole pending-wrap minefield.
//!
//! Finalized
//! transcript lines are printed once above the tail and never rewritten
//! (`App::printed_*` bookkeeping); a streaming entry that outgrows the
//! screen has its oldest rows committed the same way, keeping the tail
//! within one screen. A resize reflows the buffer and loses the anchor,
//! so the region is re-anchored at the bottom edge and the streaming
//! entry is printed once more in the new width.

use std::io::{self, Write};

use ratatui::crossterm::cursor::{MoveTo, MoveToColumn, MoveUp};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{Clear, ClearType};
use std::borrow::Cow;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use super::app::{App, CompletionState};
use super::modal::Modal;
use super::picker::Picker;

/// Prompt symbol for the input line.
const INPUT_PROMPT: &str = "❯ ";

/// Per-render display context that is not part of the mutable app state.
pub struct Status<'a> {
    pub provider: &'a str,
    pub model: Option<&'a str>,
    /// Active thinking setting, when enabled (token budget, e.g. `4096`;
    /// `off`/unset is not shown).
    pub thinking: Option<&'a str>,
    /// Display name of the current session file, when one exists.
    pub session: Option<&'a str>,
    /// Estimated session cost in dollars (catalog pricing × billed token
    /// totals, per-round usage reports summed); `None` when no pricing is
    /// known.
    pub cost: Option<f64>,
    /// Model context window (catalog hint); `None` when unknown, in which
    /// case no context percentage is shown.
    pub context_window: Option<u64>,
}

/// The terminal, viewed as a linear transcript with a redrawable tail.
pub struct Screen<W: Write> {
    out: W,
    width: u16,
    height: u16,
    /// Rows printed by the last render that the next render rewrites
    /// (streaming tail + popup + status + input).
    tail_height: usize,
    /// Rows between the cursor and the bottom edge when the last render
    /// finished: usually 0 (steady-state redraws end on the bottom row),
    /// but a caret parked on an upper input row leaves it positive — the
    /// next redraw must start from the cursor's *actual* position.
    cursor_top: usize,
}

impl<W: Write> Screen<W> {
    /// Take ownership of a writer and anchor the region at the bottom row.
    pub fn new(out: W, width: u16, height: u16) -> Self {
        Self {
            out,
            width,
            height,
            tail_height: 0,
            cursor_top: 0,
        }
    }

    /// Current terminal dimensions as last seen by the renderer.
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Re-anchor after a resize (the buffer was reflowed, the anchor is
    /// lost): the tail is redrawn from scratch and the streaming entry is
    /// reprinted in full at the new width.
    pub fn resync(&mut self, app: &mut App, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.tail_height = 0;
        self.cursor_top = 0;
        // The buffer was reflowed and the anchor is lost. Whatever the
        // streaming entry has printed so far is conserved across the
        // resync: the next render commits its unprinted portion (just
        // as sealed entries commit theirs), and the tail reprints only
        // from there — the transcript still only ever grows downward,
        // nothing is redrawn onto old rows. Losing this bookkeeping (an
        // earlier resync reset it) made the redraw stream already-printed
        // rows again — the transcript then *overwrote* instead of pushing
        // up after every resize.
        let printed = app.printed(app.entries.len().saturating_sub(1));
        if app.sealed_boundary() < app.entries.len() {
            app.set_printed(app.entries.len() - 1, printed);
        }
        // The archived session text that was reached by cursor_top rows...
    }

    /// Erase the visible screen (Ctrl+L). Scrollback is preserved; the
    /// caller clears the app's entries and print bookkeeping itself.
    pub fn clear_screen(&mut self) {
        let _ = execute!(self.out, Clear(ClearType::All), MoveTo(0, 0));
        self.tail_height = 0;
        self.cursor_top = 0;
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
        modal: Option<&Modal>,
        completion: Option<&CompletionState>,
    ) -> io::Result<()> {
        // One column is reserved: painting the terminal's last column
        // would set the pending-wrap flag (and a character written there
        // is not displayed by GNU screen until the screen scrolls), so
        // the whole layout — wrapping, status padding, boxes — happens
        // at one column less. The reserved column stays blank.
        let width = self.width.saturating_sub(1).max(1);
        let height = self.height;

        // The footer's height caps how many rows of the streaming entry
        // can stay in the redrawable tail.
        let (estimated_prompt, estimated_completion) = app.estimated_tokens();
        let footer = footer_lines(
            app,
            status,
            picker,
            modal,
            completion,
            width,
            height,
            estimated_prompt,
            estimated_completion,
        );

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
        // Rows of the streaming entry that stay in the tail, above the
        // footer. The caret's row is a *footer*-relative offset, so the
        // distance from the tail's bottom row must skip these — otherwise
        // the cursor parks too high while an entry streams, and the next
        // redraw (a relative MoveUp from the caret) starts that many rows
        // early, leaving stale copies of the tail below it.
        let entry_tail_rows = tail.len();
        tail.extend(footer.lines);

        // Positioning splits by anchor state, because the two states can
        // trust different things:
        //
        // - Fresh (startup, resize, `$EDITOR` roundtrip): the cursor's
        //   whereabouts are unknown (the buffer was reflowed, the editor
        //   scrolled), so positioning is absolute: CUP to the row where
        //   the block *ends* on the bottom row. CUP never scrolls (every
        //   terminal clamps it inside the screen) — the previous "cursor
        //   next line with a huge count" printed at the bottom edge and
        //   scrolled, which VTE turned into duplicated status lines.
        //   Stale content left in the region by the editor or the reflow
        //   is wiped first.
        // - Live (steady state): everything above the previous tail is
        //   transcript — printed once, immutable, kept by the terminal's
        //   scrollback like a program writing to stdout. The redraw may
        //   therefore only touch the previous tail's rows plus whatever
        //   has grown below them. So the redraw reaches the *previous*
        //   tail's top row from the caret parked there last frame
        //   (`cursor_top` rows above the tail's bottom), then prints the
        //   committed rows and the new tail DOWNWARD from there: growth
        //   extends downward and pushes the status line down, and when
        //   the block runs past the bottom edge the terminal scrolls
        //   naturally and history flows into the native scrollback.
        //   Anything above the previous tail is never touched. (The
        //   column is reset first: MoveUp/MoveDown preserve it, and
        //   printing from column > 0 would shift the tail right.)
        //
        //   A prior anchoring computed from the *new* block height
        //   (`new_rows - 1` rows up): when the block had grown that move
        //   started redraws rows *above* the previous tail, painting the
        //   streaming text on top of the transcript above the tail —
        //   text blocks visibly overwrote the prompt, the tool call and
        //   each other instead of pushing it up.
        let new_rows = commit.len() + tail.len();
        let shrink = new_rows < self.tail_height;
        if self.tail_height == 0 {
            let top = (self.height as usize)
                .saturating_sub(new_rows)
                .min(self.height as usize - 1);
            execute!(self.out, MoveTo(0, top as u16))?;
            execute!(self.out, Clear(ClearType::FromCursorDown))?;
            self.cursor_top = 0;
        } else {
            // From the parked caret, the previous tail's top row is
            // `tail_height - 1 - cursor_top` rows up (the caret may sit
            // on any row of the input region: `cursor_top` counts how
            // far above the tail's bottom row it was parked). When the
            // cursor already sits on the previous tail's top row
            // (`cursor_top == tail_height - 1`, the steady state after
            // any single-row-input frame), the redraw would otherwise
            // *start* there and overwrite it without knowing the tail
            // has grown: the CUP frees the tail's old rows below the
            // cursor. CUP is safe here — the redraw never scrolls
            // upward, only downward as text is printed.
            let up = (self.tail_height - 1).saturating_sub(self.cursor_top);
            execute!(self.out, MoveUp(up as u16))?;
            execute!(self.out, MoveToColumn(0))?;
        }
        let mut first = true;
        for line in commit.iter().chain(tail.iter()) {
            if !first {
                self.out.write_all(b"\r\n")?;
            }
            first = false;
            print_line(&mut self.out, line, width)?;
            // Erase stale characters when the new row is shorter than
            // what the previous render left on it (e.g. the input
            // placeholder replaced by shorter typed text). The full
            // width is cleared, including the reserved column.
            execute!(self.out, Clear(ClearType::UntilNewLine))?;
        }
        if shrink {
            execute!(self.out, Clear(ClearType::FromCursorDown))?;
        }
        self.tail_height = tail.len();
        // The draw rewrites `tail.len()` rows from the tail's top; ended
        // on the bottom edge (no caret repark yet).
        self.cursor_top = 0;
        if let Some((row, col)) = footer.caret {
            // Park the caret on its own row (the next redraw reads
            // `cursor_top` instead of assuming the bottom edge). Moving
            // up from the tail's bottom row reaches it without absolute
            // addressing (a live anchor cannot use CUP — see above).
            // `footer.caret`'s row is already an index into the tail's
            // lines (the input region appends its `input_start`), so the
            // caret sits `tail.len() - 1 - wanted` rows above the tail's
            // bottom row — on the bottom row itself when it is the last
            // line. (An earlier `+ 1` double-counted the entry-tail
            // skip: the caret parked one row low — for the common single
            // input row, below the bottom edge — so the next redraw
            // measured the previous frame's cursor one row off, redrew
            // one row high, and corrected on the row after: the input
            // line visibly jumped up and back on every keystroke.)
            let wanted = entry_tail_rows + row;
            let from_bottom = (tail.len() - 1).saturating_sub(wanted);
            if from_bottom > 0 {
                execute!(self.out, MoveUp(from_bottom as u16))?;
            }
            execute!(self.out, MoveToColumn(col))?;
            self.cursor_top = from_bottom;
        }
        if std::env::var_os("CLANKY_TRACE").is_some() {
            let focus = app
                .entries
                .iter()
                .rev()
                .find_map(|e| match e {
                    crate::tui::app::Entry::Thinking(t) => {
                        Some(format!("think[{}]", t.chars().count()))
                    }
                    crate::tui::app::Entry::Assistant(t) => {
                        Some(format!("text[{}]", t.chars().count()))
                    }
                    crate::tui::app::Entry::ToolCall { name, .. } => Some(format!("call[{name}]")),
                    crate::tui::app::Entry::ToolResult { name, .. } => {
                        Some(format!("result[{name}]"))
                    }
                    _ => None,
                })
                .unwrap_or_default();
            eprintln!(
                "TRACE commit={} tail={} cursor_top={} sealed={} nentries={} last={focus} printed={:?}",
                commit.len(),
                tail.len(),
                self.cursor_top,
                sealed,
                app.entries.len(),
                app.printed
            );
        }
        self.out.flush()
    }
}

/// Print one styled line as ANSI, truncated to `width` display columns
/// (truncation keeps the row-count math exact: a wrapped row would shift
/// everything below).
pub fn print_line<W: Write>(out: &mut W, line: &Line<'_>, width: u16) -> io::Result<()> {
    let width = width as usize;
    let mut col = 0usize;
    // A styled row (any span carrying a background color) is padded to
    // the full row width with background-only spaces, so the color forms
    // one continuous block across the row. The padded width is the
    // layout width (the last terminal column is reserved, never
    // painted — see `Screen::render`), and it must be emitted before the
    // caller's end-of-row erase: `Clear(UntilNewLine)` would otherwise
    // cut the background at the last visible character. Rows without a
    // background are not padded (they end at their last character, and
    // the erase clears the rest).
    let paints_bg = line
        .spans
        .iter()
        .any(|span| span.style.patch(line.style).bg.is_some());
    // The SGR sequence currently in effect. Every style change must emit
    // the full new state (reset plus the span's own codes): `sgr` only
    // produces codes for attributes a style *has*, so switching to a
    // plain span would otherwise leave the previous span's color and
    // modifiers active until the next styled span or the end of the line.
    let mut applied: Option<String> = None;
    for span in &line.spans {
        let want = sgr(span.style);
        if applied.as_deref() != Some(want.as_str()) {
            out.write_all(b"\x1b[0m")?;
            if !want.is_empty() {
                out.write_all(want.as_bytes())?;
            }
            applied = Some(want);
        }
        for c in span.content.chars() {
            // Control characters are never emitted. `width()` reports
            // `None` (counted as zero) for them, but a real terminal
            // does not: a tab jumps the cursor to the next tab stop —
            // possibly onto or past the reserved last column, setting
            // the pending-wrap flag so the next CR/LF scrolls the
            // screen (duplicated status lines); ESC/DEL could be taken
            // as control sequences; CR would move the cursor invisibly.
            // Skipping them keeps the emitted row exactly what the
            // wrapping math budgeted. The transcript text is not
            // rewritten: this is display-side only.
            if c.is_control() {
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                break;
            }
            let mut buf = [0u8; 4];
            out.write_all(c.encode_utf8(&mut buf).as_bytes())?;
            col += w;
        }
    }
    if paints_bg && col < width {
        // Full-width background: keep the active SGR (bg included) and
        // fill the rest of the row with spaces. Same edge cases as the
        // text pass above: control characters never appear (only spaces
        // are emitted) and the fill cannot overflow `width`.
        let fill = " ".repeat(width - col);
        out.write_all(fill.as_bytes())?;
    }
    out.write_all(b"\x1b[0m")
}

/// ANSI SGR sequence for a ratatui style (the colors and modifiers the
/// TUI actually uses; anything else falls back to the default).
fn sgr(style: Style) -> String {
    let mut codes: Vec<Cow<'static, str>> = Vec::new();
    let fg = match style.fg {
        Some(Color::Black) => Some(Cow::Borrowed("30")),
        Some(Color::Red) => Some(Cow::Borrowed("31")),
        Some(Color::Green) => Some(Cow::Borrowed("32")),
        Some(Color::Yellow) => Some(Cow::Borrowed("33")),
        Some(Color::Blue) => Some(Cow::Borrowed("34")),
        Some(Color::Magenta) => Some(Cow::Borrowed("35")),
        Some(Color::Cyan) => Some(Cow::Borrowed("36")),
        Some(Color::Gray) => Some(Cow::Borrowed("37")),
        Some(Color::DarkGray) => Some(Cow::Borrowed("90")),
        Some(Color::LightRed) => Some(Cow::Borrowed("91")),
        Some(Color::LightGreen) => Some(Cow::Borrowed("92")),
        Some(Color::LightYellow) => Some(Cow::Borrowed("93")),
        Some(Color::LightBlue) => Some(Cow::Borrowed("94")),
        Some(Color::LightMagenta) => Some(Cow::Borrowed("95")),
        Some(Color::LightCyan) => Some(Cow::Borrowed("96")),
        Some(Color::White) => Some(Cow::Borrowed("97")),
        // 256-color palette entries emit as indexed: `38;5;n`. The
        // code is dynamically formatted, so it goes out owned (the
        // static arms borrow theirs).
        Some(Color::Indexed(i)) => Some(Cow::Owned(format!("38;5;{i}"))),
        _ => None,
    };
    if let Some(code) = fg {
        codes.push(code);
    }
    // Background colors (40-47 standard, 100-107 bright). Painted only
    // where the row has characters: the caller pads rows that must look
    // full-width (see `print_line`).
    let bg = match style.bg {
        Some(Color::Black) => Some(Cow::Borrowed("40")),
        Some(Color::Red) => Some(Cow::Borrowed("41")),
        Some(Color::Green) => Some(Cow::Borrowed("42")),
        Some(Color::Yellow) => Some(Cow::Borrowed("43")),
        Some(Color::Blue) => Some(Cow::Borrowed("44")),
        Some(Color::Magenta) => Some(Cow::Borrowed("45")),
        Some(Color::Cyan) => Some(Cow::Borrowed("46")),
        Some(Color::Gray) => Some(Cow::Borrowed("47")),
        Some(Color::DarkGray) => Some(Cow::Borrowed("100")),
        Some(Color::LightRed) => Some(Cow::Borrowed("101")),
        Some(Color::LightGreen) => Some(Cow::Borrowed("102")),
        Some(Color::LightYellow) => Some(Cow::Borrowed("103")),
        Some(Color::LightBlue) => Some(Cow::Borrowed("104")),
        Some(Color::LightMagenta) => Some(Cow::Borrowed("105")),
        Some(Color::LightCyan) => Some(Cow::Borrowed("106")),
        Some(Color::White) => Some(Cow::Borrowed("107")),
        // As on the fg side: `48;5;n`, owned rather than borrowed.
        Some(Color::Indexed(i)) => Some(Cow::Owned(format!("48;5;{i}"))),
        _ => None,
    };
    if let Some(code) = bg {
        codes.push(code);
    }
    let m = style.add_modifier;
    if m.contains(Modifier::BOLD) {
        codes.push(Cow::Borrowed("1"));
    }
    if m.contains(Modifier::DIM) {
        codes.push(Cow::Borrowed("2"));
    }
    if m.contains(Modifier::ITALIC) {
        codes.push(Cow::Borrowed("3"));
    }
    if m.contains(Modifier::UNDERLINED) {
        codes.push(Cow::Borrowed("4"));
    }
    if m.contains(Modifier::REVERSED) {
        codes.push(Cow::Borrowed("7"));
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// The redrawable footer: an optional popup, the status line, and the
/// input line(s) — plus the caret's (row, column) within the footer.
/// The row is an offset into `lines` (the caret sits in the input
/// region, on the physical line containing the cursor).
struct Footer {
    lines: Vec<Line<'static>>,
    /// `None` when there is no safe place to park the caret (e.g. the
    /// input region did not fit); the cursor stays where the last row
    /// left it, one column past the row's text.
    caret: Option<(usize, u16)>,
}

#[allow(clippy::too_many_arguments)]
fn footer_lines(
    app: &App,
    status: &Status<'_>,
    picker: Option<&Picker>,
    modal: Option<&Modal>,
    completion: Option<&CompletionState>,
    width: u16,
    height: u16,
    estimated_prompt: u64,
    estimated_completion: u64,
) -> Footer {
    let mut lines = Vec::new();
    // An open modal owns the caret: it is drawn above the status line and
    // the main input row (which stays visible) keeps its own.
    let mut modal_caret = None;
    if let Some(modal) = modal {
        let (box_lines, caret) = modal_box(modal, width, height);
        if !box_lines.is_empty() {
            modal_caret = Some(caret);
        }
        lines.extend(box_lines);
    } else if let Some(picker) = picker {
        lines.extend(picker_box(picker, width, height));
    } else if let Some(completion) = completion {
        lines.extend(completion_box(completion, width, height));
    }
    lines.push(status_line(
        app,
        status,
        width,
        estimated_prompt,
        estimated_completion,
    ));
    // How many rows the input region may occupy. The footer always
    // includes the status line; the input rows get the rest of the
    // screen (the terminal needs at least one row left for the input —
    // one screen of transcript rows can be committed anyway).
    let input_budget = (height as usize).saturating_sub(lines.len()).max(1);
    let input = input_rows(app, width, input_budget);
    let input_start = lines.len();
    lines.extend(input.lines);
    Footer {
        lines,
        caret: modal_caret.or_else(|| input.caret.map(|(row, col)| (input_start + row, col))),
    }
}

/// Status line. As the terminal narrows, elements are dropped one at a
/// time — first provider, then thinking-budget, model, cost, session-name,
/// the "queued" word (reducing to just the dot), and finally the context
/// percentage — so the line de-contents gracefully instead of truncating.
/// The hints (`/ commands · ctrl+c quit`) replace the cost and context
/// elements whenever price data is unavailable; they are the last element
/// to go, after which the line collapses to just the queued dot.
#[allow(clippy::too_many_arguments)]
fn status_line(
    app: &App,
    status: &Status<'_>,
    width: u16,
    estimated_prompt: u64,
    estimated_completion: u64,
) -> Line<'static> {
    let width = width as usize;
    let base = Style::new().fg(Color::DarkGray);
    let sep = || Span::styled(" · ".to_string(), base);

    // Context percentage: how much of the window the next request would
    // occupy (billed token counts plus estimates for content not yet
    // covered by a provider report).
    let has_usage = app.last_usage.is_some() || estimated_prompt > 0 || estimated_completion > 0;
    let ctx = status
        .context_window
        .filter(|window| *window > 0 && has_usage)
        .map(|window| {
            let prompt = app
                .last_usage
                .and_then(|usage| usage.prompt_tokens)
                .unwrap_or(0)
                + estimated_prompt;
            let completion = app
                .last_usage
                .and_then(|usage| usage.completion_tokens)
                .unwrap_or(0)
                + estimated_completion;
            Span::styled(
                format!("{}% ctx", (prompt + completion) * 100 / window),
                base,
            )
        });
    let cost = status
        .cost
        .map(|c| Span::styled(format_cost(c), Style::new().fg(Color::Yellow)));
    // The right group: cost and context when price data exists, otherwise
    // the hints stand in for both.
    let has_price = cost.is_some() || ctx.is_some();
    let queued = !app.pending.is_empty();
    let hints = || Span::styled("/ commands · ctrl+c quit".to_string(), base);

    // Elements, in the order they are dropped as the terminal narrows
    // (so: the order they are added as it widens, most expendable last
    // to be added... first to be dropped). The context element comes
    // first because it is dropped last.
    #[derive(Clone, Copy, PartialEq)]
    enum Elem {
        Context, // or the hints, when there is no price data
        QueuedDot,
        Session,
        QueuedWord,
        Cost,
        Model,
        Think,
        Provider,
    }
    let mut candidates = vec![Elem::Context];
    if queued {
        candidates.push(Elem::QueuedDot);
    }
    if status.session.is_some() {
        candidates.push(Elem::Session);
    }
    if queued {
        candidates.push(Elem::QueuedWord);
    }
    if cost.is_some() {
        candidates.push(Elem::Cost);
    }
    if status.model.is_some() {
        candidates.push(Elem::Model);
    }
    if status.thinking.is_some() {
        candidates.push(Elem::Think);
    }
    candidates.push(Elem::Provider);

    // Build the line with `on` telling which elements survived.
    let build = |on: &[Elem]| -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let has = |e: Elem| on.contains(&e);
        let mut left: Vec<Span<'static>> = Vec::new();
        if has(Elem::Provider) {
            left.push(Span::styled(
                status.provider.to_string(),
                base.add_modifier(Modifier::BOLD),
            ));
        }
        if has(Elem::Model)
            && let Some(model) = status.model
        {
            if !left.is_empty() {
                left.push(sep());
            }
            left.push(Span::styled(model.to_string(), base));
        }
        if has(Elem::Think)
            && let Some(thinking) = status.thinking
        {
            if !left.is_empty() {
                left.push(sep());
            }
            left.push(Span::styled("think ".to_string(), base));
            left.push(Span::styled(
                thinking.to_string(),
                Style::new().fg(Color::Cyan),
            ));
        }
        if has(Elem::Session)
            && let Some(session) = status.session
        {
            if !left.is_empty() {
                left.push(sep());
            }
            left.push(Span::styled(session.to_string(), base));
        }
        if has(Elem::QueuedDot) {
            // The dot is glued to whatever precedes it with a single
            // space ("session ○"), standing alone when nothing does.
            if !left.is_empty() {
                left.push(Span::raw(" "));
            }
            left.push(Span::styled(
                "○".to_string(),
                Style::new().fg(Color::Yellow),
            ));
        }
        if has(Elem::QueuedWord) {
            left.push(Span::styled(
                " queued".to_string(),
                Style::new().fg(Color::Yellow),
            ));
        }
        let mut right: Vec<Span<'static>> = Vec::new();
        if has_price {
            if has(Elem::Cost)
                && let Some(cost) = &cost
            {
                right.push(cost.clone());
            }
            if has(Elem::Context)
                && let Some(ctx) = &ctx
            {
                if !right.is_empty() {
                    right.push(sep());
                }
                right.push(ctx.clone());
            }
        } else if has(Elem::Context) {
            right.push(hints());
        }
        (left, right)
    };

    // Add elements one at a time, keeping each only if the whole line
    // still fits: left + a gap of at least one space + right.
    let fits = |on: &[Elem]| -> bool {
        let (left, right) = build(on);
        let gap = usize::from(!left.is_empty() && !right.is_empty());
        spans_width(&left) + gap + spans_width(&right) <= width
    };
    let mut on: Vec<Elem> = Vec::new();
    for elem in candidates {
        on.push(elem);
        if !fits(&on) {
            on.pop();
        }
    }
    let (left, right) = build(&on);

    let left_width = spans_width(&left);
    let right_width = spans_width(&right);
    if left_width + right_width <= width {
        let mut all = left;
        if right_width > 0 {
            all.push(Span::raw(" ".repeat(width - left_width - right_width)));
            all.extend(right);
        }
        Line::from(all)
    } else {
        // Nothing fits (an absurdly narrow terminal): truncate what the
        // drop order left us with.
        let mut all = left;
        all.extend(right);
        Line::from(truncate_spans(all, width))
    }
}

/// Format a session cost for the status line: four decimals while small,
/// two once it grows past a few dollars.
fn format_cost(cost: f64) -> String {
    if cost >= 10.0 {
        format!("${cost:.2}")
    } else {
        format!("${cost:.4}")
    }
}

/// The input region: one row per physical line of the input buffer,
/// plus the caret's (row, column) within the region.
///
/// Multi-line input (paste, `$EDITOR`) used to render as one row whose
/// embedded newlines made each terminal emulator wrap after the
/// previous line's text — a stair-step. Now each physical line gets its
/// own row: the first starts after the prompt symbol (`● ❯ `) and the
/// rest are indented under it, so every line starts at the same column.
/// Each row is windowed horizontally so the caret stays on screen.
struct InputRows {
    lines: Vec<Line<'static>>,
    /// The caret's row (index into `lines`) and column, or `None` when
    /// the terminal is too narrow for an input row at all.
    caret: Option<(usize, u16)>,
}

/// The prompt prefix is always at least three columns: a streaming dot
/// ("●" while a turn runs, a blank otherwise) plus "❯ ", optionally
/// preceded by the project label (`project` setting, e.g. `clanky  ❯ `).
/// The fixed width keeps the caret column steady whether or not the dot
/// is lit. Continuation rows are indented to the same width.
const PROMPT_WIDTH: usize = 3;

/// Spans printed at the left of the input prompt: the project label
/// (`project` setting) followed by the prompt block itself. The first
/// span's width is the total prefix width the continuation rows and the
/// caret column must match; `None` is the bare three-column prompt.
fn prompt_prefix(app: &App) -> Vec<Span<'static>> {
    let prompt_style = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
    let dot = if app.busy {
        Span::styled("●".to_string(), prompt_style)
    } else {
        Span::raw(" ".to_string())
    };
    let mut spans = Vec::new();
    if let Some(project) = app.project.as_deref() {
        spans.push(Span::styled(
            project.to_string(),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" ".to_string()));
    }
    spans.push(dot);
    spans.push(Span::styled(INPUT_PROMPT.to_string(), prompt_style));
    spans
}

/// Display width of the prompt prefix: the project label plus a space
/// when set, then the fixed dot + prompt symbol.
fn prompt_prefix_width(app: &App) -> usize {
    PROMPT_WIDTH + app.project.as_deref().map(|p| p.width() + 1).unwrap_or(0)
}

/// `budget` caps how many rows the input region may occupy. When the
/// input has more lines than fit, the rows around the caret are kept
/// (the caret must stay on screen); the hidden ones just don't render —
/// the buffer itself is unaffected.
fn input_rows(app: &App, width: u16, budget: usize) -> InputRows {
    let prefix_width = prompt_prefix_width(app);
    let view_width = (width as usize).saturating_sub(prefix_width);
    if view_width == 0 {
        return InputRows {
            lines: Vec::new(),
            caret: None,
        };
    }
    let prefix = prompt_prefix(app);
    if app.input.is_empty() && !app.busy {
        let mut lines = prefix.clone();
        lines.push(Span::styled(
            "type a message…".to_string(),
            Style::new().fg(Color::DarkGray),
        ));
        return InputRows {
            lines: vec![Line::from(lines)],
            caret: Some((0, prefix_width as u16)),
        };
    }

    // Split into physical lines, tracking where each begins in the
    // buffer so the cursor maps to a (row, column) pair. A trailing
    // newline yields an empty final row (the caret's own row).
    let mut starts: Vec<usize> = vec![0];
    let mut rows: Vec<String> = Vec::new();
    let mut row_text = String::new();
    for (i, c) in app.input.char_indices() {
        if c == '\n' {
            rows.push(std::mem::take(&mut row_text));
            starts.push(i + 1);
        } else {
            row_text.push(c);
        }
    }
    rows.push(row_text);
    // Cursor position as (row, byte offset within that row). A cursor
    // exactly at a newline belongs to the end of the row above it (the
    // newline ends that row); a cursor at the very end of the buffer
    // (or an empty buffer) is not inside any [start, next-start) range
    // and lands on the final row.
    let cursor = app.cursor.min(app.input.len());
    let mut caret_row = 0usize;
    let mut caret_byte = cursor;
    let mut found = false;
    for (r, &start) in starts.iter().enumerate().take(rows.len()) {
        let end = starts.get(r + 1).copied().unwrap_or(app.input.len());
        if (start..end).contains(&cursor) {
            caret_row = r;
            caret_byte = cursor - start;
            found = true;
            break;
        }
    }
    if !found {
        caret_row = rows.len() - 1;
        caret_byte = rows[caret_row].len();
    }

    let indent = " ".repeat(prefix_width);
    let mut lines = Vec::new();
    let mut caret = None;
    for (r, row) in rows.iter().enumerate() {
        let cursor_here = if r == caret_row {
            Some(caret_byte)
        } else {
            None
        };
        let (window, caret_col) = input_window(row, cursor_here, view_width);
        let mut spans = Vec::with_capacity(3);
        if r == 0 {
            spans.extend(prefix.clone());
        } else {
            spans.push(Span::raw(indent.clone()));
        }
        spans.extend(window);
        if r == caret_row {
            caret = Some((r, (prefix_width + caret_col) as u16));
        }
        lines.push(Line::from(spans));
    }
    // Trim to the budget around the caret so the caret's row stays
    // visible. The hidden rows are dropped entirely (the renderer
    // erases stale characters below).
    if lines.len() > budget {
        let anchor = caret.map(|(r, _)| r).unwrap_or(0);
        let max_skip = lines.len() - budget;
        // Center the window on the caret, clamped to the buffer.
        let skip = anchor.saturating_sub(budget / 2).min(max_skip).max(
            max_skip
                .saturating_sub(lines.len() - 1 - anchor)
                .min(max_skip),
        );
        let caret = caret.map(|(r, col)| (r - skip, col));
        let lines = lines.into_iter().skip(skip).take(budget).collect();
        return InputRows { lines, caret };
    }
    InputRows { lines, caret }
}

/// The visible slice of one physical input line so the caret stays on
/// screen. Returns the styled spans and the caret column within the
/// window. `cursor` is the caret's byte offset within `line`, if the
/// caret is on this line.
fn input_window(
    line: &str,
    cursor: Option<usize>,
    view_width: usize,
) -> (Vec<Span<'static>>, usize) {
    // Display columns of every char and of the caret.
    let mut cols = Vec::new();
    let mut col = 0usize;
    let mut caret_col = None;
    for (i, c) in line.char_indices() {
        if cursor == Some(i) {
            caret_col = Some(col);
        }
        cols.push((col, c));
        col += c.width().unwrap_or(0);
    }
    let caret_col = match caret_col {
        Some(c) => c,
        // The caret is at the end of this line (or not on it): park it
        // past the text — no windowing needed for that line.
        None => col,
    };

    if col <= view_width {
        return (vec![Span::raw(line.to_string())], caret_col);
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

/// The text-entry modal: a prompt row and the field being typed, windowed
/// so the caret stays visible. Returns the box rows plus the caret's
/// (row, column) within them (the box may be skipped on a tiny terminal,
/// in which case the caller keeps the main input caret).
fn modal_box(modal: &Modal, width: u16, height: u16) -> (Vec<Line<'static>>, (usize, u16)) {
    if width < 24 || height < 8 {
        return (Vec::new(), (0, 0));
    }
    let title = format!(" {} ", modal.title());
    let prompt = Line::from(Span::styled(
        modal.prompt().to_string(),
        Style::new().fg(Color::DarkGray),
    ));
    // Size the box from the prompt (the field is windowed to the box, so
    // it can never widen it) and window the field to that inner width.
    let inner = box_width(title.width(), line_width(&prompt), width) - 4;
    let (field, caret_col) = input_window(&modal.input, Some(modal.cursor), inner);
    // Rows: top border, prompt, field, bottom border — the caret lives on
    // the field row, the second row inside the box.
    let rows = vec![prompt, Line::from(field)];
    let field_row = 2;
    (
        boxed(&title, rows, width),
        (field_row, 2 + caret_col as u16),
    )
}

/// The box width for a title and content of the given widths: wide enough
/// for both, never wider than the screen.
fn box_width(title_width: usize, content_width: usize, width: u16) -> usize {
    content_width
        .max(title_width)
        .saturating_add(4) // "│ " + " │"
        .min(width.saturating_sub(2) as usize)
        .max(title_width + 6)
        .max(8)
}

/// Wrap `rows` in a rounded box with `title` in the top border. The box
/// is as wide as the widest row (plus padding), never wider than the
/// screen; rows are truncated and padded to the box width.
fn boxed(title: &str, rows: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let border = Style::new().fg(Color::DarkGray);
    let title_style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);

    let content_width = rows.iter().map(|l| line_width(l)).max().unwrap_or(0);
    let box_width = box_width(title.width(), content_width, width);
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
    use crate::turn::{TurnEvent, TurnOutput};

    /// A committed user prompt (rendered once, sealed) is followed by a
    /// thinking block streaming in row by row: each redraw rewrites the
    /// tail in place — one status row, one input row, no duplicates, no
    /// drift — while the committed rows join the transcript irreversibly.
    /// Regression (commit-aware live anchor): the redraw used to move up
    /// `tail.len() - 1` rows but then also print `commit.len()` rows
    /// from there, so commit rows painted over the stable tail's rows
    /// while tail rows printed past the bottom edge — scrolling the
    /// screen once per committed row and repositioning every frame:
    /// older messages appeared nailed above a "viewport" of streaming
    /// text instead of the stream flowing into history.
    #[test]
    fn thinking_stream_commit_rewrites_the_tail_in_place() {
        let mut app = App::new(None);
        let mut s = screen(Buf::default(), 60, 15);
        s.render(&mut app, &status(), None, None, None).unwrap();
        s.take_out();
        app.push_user("first question");
        app.busy = true;
        s.render(&mut app, &status(), None, None, None).unwrap();
        // The very next render has nothing new: the tail stays put.
        s.render(&mut app, &status(), None, None, None).unwrap();
        s.take_out();

        for n in 1..=6 {
            app.on_turn_event(TurnEvent::Thinking {
                delta: format!("think line {n}\n"),
            });
            s.render(&mut app, &status(), None, None, None).unwrap();
        }
        let rows = s.take_out().emulated_screen(60, 15);
        // All six streamed lines on screen, each once.
        for n in 1..=6 {
            assert_eq!(
                rows.iter()
                    .filter(|r| r.contains(&format!("think line {n}")))
                    .count(),
                1,
                "think line {n} duplicated or missing: {rows:?}"
            );
        }
        // Exactly one status line and one input line, below the stream.
        assert_eq!(rows.iter().filter(|r| r.contains("deepinfra ·")).count(), 1);
        assert_eq!(rows.iter().filter(|r| r.contains("●❯")).count(), 1);
        // The committed prompt scrolled off the top: the streamed rows
        // plus their separators exceed the screen and the transcript
        // (never the tail) flows into history. Nothing was duplicated.
        assert!(!rows.iter().any(|r| r.contains("first question")));
        // Long enough that the streamed tail commits its oldest rows:
        // those commit rows join history and must not reorder or
        // duplicate what is already there.
        for n in 7..=12 {
            app.on_turn_event(TurnEvent::Thinking {
                delta: format!("think line {n}\n"),
            });
            s.render(&mut app, &status(), None, None, None).unwrap();
        }
        let rows = s.take_out().emulated_screen(60, 15);
        assert_eq!(rows.iter().filter(|r| r.contains("deepinfra ·")).count(), 1);
        assert_eq!(rows.iter().filter(|r| r.contains("●❯")).count(), 1);
        // Rows are consecutive in SortedRows: each visible streaming row
        // appears exactly once. The oldest lines scroll off the top as
        // they commit, still exactly once each.
        for n in 2..=12 {
            let shown = rows
                .iter()
                .filter(|r| r.contains(&format!("think line {n}")))
                .count();
            assert!(shown <= 1, "think line {n} duplicated: {rows:?}");
        }
        // The top visible row is not a tail row (status or input).
        if let Some(top) = rows.iter().find(|r| !r.trim().is_empty()) {
            assert!(
                !top.contains("deepinfra ·") && !top.contains("●❯"),
                "a tail row is the top of the screen: {top:?}"
            );
        }
    }

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

        /// Emulate the recorded stream on a `width`×`height` screen and
        /// return the final rows plus every row that scrolled off the
        /// top (scrollback), both space-padded to `width`. Handles the
        /// sequences this module emits (CUU `A`, CNL `E`, CHA `G`,
        /// EL `K`, ED `J`, SGR `m` ignored) plus CR, LF with scroll at
        /// the bottom edge, and autowrap — enough to catch redraws that
        /// start at the wrong row or column, leave stale characters, or
        /// wrongly push redrawn content (e.g. the status line) into
        /// scrollback.
        fn emulated_screen(&self, width: usize, height: usize) -> Vec<String> {
            self.emulated(width, height).0
        }

        /// Like [`Self::emulated_screen`], also returning scrollback.
        fn emulated(&self, width: usize, height: usize) -> (Vec<String>, Vec<String>) {
            let mut rows = vec![vec![' '; width]; height];
            let mut history: Vec<Vec<char>> = Vec::new();
            let (mut row, mut col) = (0usize, 0usize);
            let mut chars = std::str::from_utf8(&self.0).unwrap().chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '\x1b' => {
                        let mut body = String::new();
                        let mut final_byte = None;
                        for c in chars.by_ref() {
                            if c.is_ascii_alphabetic() {
                                final_byte = Some(c);
                                break;
                            }
                            body.push(c);
                        }
                        let params = body.trim_start_matches('[');
                        let nums: Vec<usize> = params
                            .split(';')
                            .map(|p| p.parse::<usize>().ok().unwrap_or(1))
                            .collect();
                        let n = nums.first().copied().unwrap_or(1);
                        match final_byte {
                            Some('A') => row = row.saturating_sub(n.max(1)),
                            Some('E') => {
                                row = (row + n.max(1)).min(height - 1);
                                col = 0;
                            }
                            // CUP (what the renderer positions with):
                            // clamped, never scrolls.
                            Some('H') | Some('f') => {
                                row = nums
                                    .first()
                                    .copied()
                                    .unwrap_or(1)
                                    .saturating_sub(1)
                                    .min(height - 1);
                                col = nums
                                    .get(1)
                                    .copied()
                                    .unwrap_or(1)
                                    .saturating_sub(1)
                                    .min(width - 1);
                            }
                            Some('G') => col = n.saturating_sub(1).min(width - 1),
                            Some('K') => {
                                for cell in &mut rows[row][col..] {
                                    *cell = ' ';
                                }
                            }
                            Some('J') => {
                                for cell in &mut rows[row][col..] {
                                    *cell = ' ';
                                }
                                for r in rows.iter_mut().skip(row + 1) {
                                    *r = vec![' '; width];
                                }
                            }
                            _ => {} // SGR and anything else: styling only
                        }
                    }
                    '\r' => col = 0,
                    '\n' => {
                        if row + 1 == height {
                            history.push(rows.remove(0));
                            rows.push(vec![' '; width]);
                        } else {
                            row += 1;
                        }
                    }
                    c => {
                        let w = c.width().unwrap_or(0);
                        if w == 0 {
                            continue;
                        }
                        if col + w > width {
                            col = 0;
                            if row + 1 == height {
                                history.push(rows.remove(0));
                                rows.push(vec![' '; width]);
                            } else {
                                row += 1;
                            }
                        }
                        rows[row][col] = c;
                        col += 1;
                    }
                }
            }
            let screen = rows.into_iter().map(|r| r.into_iter().collect()).collect();
            let history = history
                .into_iter()
                .map(|r| r.into_iter().collect())
                .collect();
            (screen, history)
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

    /// Analysis harness (temporary): replays the recorded event stream of
    /// a real session (`target/replay/events.json`) through the screen,
    /// dumping the raw ANSI stream so an external terminal emulator can
    /// check for stale remnants.
    #[test]
    fn replay_dump_for_terminal_analysis() {
        let Some(dir) = std::env::var_os("CLANKY_REPLAY_DUMP_DIR") else {
            return;
        };
        let script_path = std::env::var("CLANKY_REPLAY_EVENTS")
            .unwrap_or_else(|_| "target/replay/events.json".into());
        let script = std::fs::read_to_string(&script_path)
            .unwrap_or_else(|e| panic!("replay script {script_path}: {e}"));
        #[derive(serde::Deserialize)]
        struct Ev {
            kind: String,
            #[serde(default)]
            text: String,
            #[serde(default)]
            name: String,
        }
        let events: Vec<Ev> = serde_json::from_str(&script).unwrap();
        let width: u16 = std::env::var("CLANKY_REPLAY_W")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
        let height: u16 = std::env::var("CLANKY_REPLAY_H")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut app = App::new(None);
        let mut s = screen(Buf::default(), width, height);
        let mut st = status();
        st.model = Some("zai-org/GLM-5.3-Flash");
        st.thinking = Some("16384");
        st.session = Some("20261005-023938-8513");

        macro_rules! render {
            ($frame:expr) => {{
                s.render(&mut app, &st, None, None, None).unwrap();
                let buf = std::mem::take(&mut s.out);
                std::fs::write(dir.join(format!("frame-{:04}.bin", $frame)), &buf.0).unwrap();
                s.out = buf;
            }};
        }

        for (frame, ev) in events.iter().enumerate() {
            match ev.kind.as_str() {
                "user" => {
                    app.push_user(&ev.text);
                    app.busy = true;
                }
                "think" => app.on_turn_event(TurnEvent::Thinking {
                    delta: ev.text.clone(),
                }),
                "text" => app.on_turn_event(TurnEvent::Text {
                    delta: ev.text.clone(),
                }),
                "call" => app.on_turn_event(TurnEvent::ToolCall {
                    name: ev.name.clone(),
                    arguments: ev.text.clone(),
                }),
                "result" => app.on_turn_event(TurnEvent::ToolResult {
                    name: ev.name.clone(),
                    output: ev.text.clone(),
                    images: Vec::new(),
                }),
                "done" => {
                    app.on_turn_done(&Ok(TurnOutput {
                        text: String::new(),
                        finish_reason: clanky_protocol::FinishReason::Stop,
                        usage: Some(clanky_protocol::Usage {
                            prompt_tokens: Some(17255),
                            completion_tokens: Some(796),
                            cached_tokens: None,
                        }),
                    }));
                }
                _ => {}
            }
            render!(frame);
        }
    }

    #[test]
    fn full_turn_stream_replays_cleanly() {
        let mut app = App::new(None);
        app.push_user("question");
        app.busy = true;
        let mut s = Screen::new(Buf::default(), 80, 24);
        let mut stream = Buf::default();
        let frame = |app: &mut App, s: &mut Screen<Buf>, stream: &mut Buf| {
            s.render(app, &status(), None, None, None).unwrap();
            let f = s.take_out();
            stream.0.extend(f.0);
        };
        frame(&mut app, &mut s, &mut stream);
        for n in 1..=8 {
            app.on_turn_event(TurnEvent::Thinking {
                delta: format!("think line {n}\n"),
            });
            frame(&mut app, &mut s, &mut stream);
        }
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: "ls".into(),
        });
        frame(&mut app, &mut s, &mut stream);
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "file1\nfile2".into(),
            images: Vec::new(),
        });
        frame(&mut app, &mut s, &mut stream);
        for n in 1..=6 {
            app.on_turn_event(TurnEvent::Text {
                delta: format!("answer line {n}\n"),
            });
            frame(&mut app, &mut s, &mut stream);
        }
        let rows = stream.emulated_screen(80, 24);
        // The call line and its result line each show `bash` once: the
        // result no longer overwrites the call (entries push up).
        for needle in [
            "think line 1",
            "think line 8",
            "● bash",
            "└─ bash",
            "ls",
            "answer line 6",
            "question",
        ] {
            let n_count = rows.iter().filter(|r| r.contains(needle)).count();
            assert!(n_count <= 1, "{needle} appears {n_count} times: {rows:?}");
        }
    }

    /// Regression: a whole round arriving between renders (thinking, text,
    /// tool call, tool result in one burst) commits everything at once;
    /// the next visible frame must show all entries, none duplicated and
    /// none lost. The per-event replay tests never caught this because
    /// each of their frames rendered between events.
    #[test]
    fn whole_round_in_one_burst_commits_exactly_once() {
        let mut app = App::new(None);
        app.push_user("question");
        app.busy = true;
        let mut s = Screen::new(Buf::default(), 80, 24);
        let mut stream = Buf::default();
        let render = |app: &mut App, s: &mut Screen<Buf>, stream: &mut Buf| {
            s.render(app, &status(), None, None, None).unwrap();
            stream.0.extend(s.take_out().0);
        };
        render(&mut app, &mut s, &mut stream);
        // The whole first round lands in one drain: thinking, text, tool
        // call, tool result.
        app.on_turn_event(TurnEvent::Thinking {
            delta: "Considering the request... thinking line one\n".into(),
        });
        app.on_turn_event(TurnEvent::Text {
            delta: "Let me check that. ".into(),
        });
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: "{\"command\":\"echo hi\"}".into(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "hi".into(),
            images: Vec::new(),
        });
        app.on_turn_event(TurnEvent::Thinking {
            delta: "second thought\n".into(),
        });
        app.on_turn_event(TurnEvent::Text { delta: "Tool done. Final answer with a fairly long line to force wrapping of the display row.\nSecond paragraph.".into() });
        render(&mut app, &mut s, &mut stream);
        let rows = stream.emulated_screen(80, 24);
        let count = |needle: &str| rows.iter().filter(|r| r.contains(needle)).count();
        assert_eq!(count("Let me check"), 1, "text lost/dup: {rows:?}");
        assert_eq!(count("Considering"), 1, "thinking lost/dup: {rows:?}");
        assert_eq!(count("echo hi"), 1, "tool call lost/dup: {rows:?}");
        assert_eq!(count("hello-from-bash"), 0, "n/a");
        assert_eq!(count("Tool done"), 1, "final text lost/dup: {rows:?}");
        assert_eq!(
            count("second thought"),
            1,
            "second thinking lost/dup: {rows:?}"
        );
        // The result no longer overwrites the call: both lines visible.
        assert_eq!(count("● bash"), 1, "call line lost/dup: {rows:?}");
        assert_eq!(count("└─ bash"), 1, "result line lost/dup: {rows:?}");
        assert_eq!(
            rows.iter().filter(|r| r.contains("deepinfra ·")).count(),
            1,
            "status: {rows:?}"
        );
    }

    /// Same burst, but rendered per event (one render after each event):
    /// each entry must appear exactly once when the turn settles.
    #[test]
    fn per_event_renders_commit_exactly_once() {
        let mut app = App::new(None);
        app.push_user("question");
        app.busy = true;
        let mut s = Screen::new(Buf::default(), 80, 24);
        let mut stream = Buf::default();
        let render = |app: &mut App, s: &mut Screen<Buf>, stream: &mut Buf| {
            s.render(app, &status(), None, None, None).unwrap();
            stream.0.extend(s.take_out().0);
        };
        render(&mut app, &mut s, &mut stream);
        for ev in [
            TurnEvent::Thinking { delta: "Considering the request... thinking line one\n".into() },
            TurnEvent::Text { delta: "Let me check that. ".into() },
            TurnEvent::ToolCall { name: "bash".into(), arguments: "{\"command\":\"echo hi\"}".into() },
            TurnEvent::ToolResult { name: "bash".into(), output: "hi".into(), images: Vec::new() },
            TurnEvent::Thinking { delta: "second thought\n".into() },
            TurnEvent::Text { delta: "Tool done. Final answer with a fairly long line to force wrapping of the display row.\nSecond paragraph.".into() },
        ] {
            app.on_turn_event(ev);
            render(&mut app, &mut s, &mut stream);
            let (rows, hist) = stream.emulated(80, 24);
                        println!("=== frame ===");
            for (i, r) in hist.iter().enumerate() { println!("H{i:02}|{r}|"); }
            for (i, r) in rows.iter().enumerate() { println!("S{i:02}|{r}|"); }
        }
        let rows = stream.emulated_screen(80, 24);
        let count = |needle: &str| rows.iter().filter(|r| r.contains(needle)).count();
        assert_eq!(count("Let me check"), 1, "text lost/dup: {rows:?}");
        assert_eq!(count("Considering"), 1, "thinking lost/dup: {rows:?}");
        assert_eq!(count("echo hi"), 1, "tool call lost/dup: {rows:?}");
        assert_eq!(count("● bash"), 1, "call line lost/dup: {rows:?}");
        assert_eq!(count("└─ bash"), 1, "result line lost/dup: {rows:?}");
        assert_eq!(count("Tool done"), 1, "final text lost/dup: {rows:?}");
        assert_eq!(
            count("second thought"),
            1,
            "second thinking lost/dup: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains("deepinfra ·")).count(),
            1,
            "status: {rows:?}"
        );
    }

    /// Nothing that was printed may ever be rewritten: each finalized
    /// entry (prompt, thinking, call, result, answer) pushes down and
    /// remains visible above the newer one. Regression for the
    /// overwrite-instead-of-push-up bug where streaming text painted
    /// over the prompt and earlier blocks.
    #[test]
    fn nothing_is_ever_overwritten() {
        let mut app = App::new(None);
        app.push_user("question");
        app.busy = true;
        let mut s = Screen::new(Buf::default(), 80, 30);
        let mut stream = Buf::default();
        let render = |app: &mut App, s: &mut Screen<Buf>, stream: &mut Buf| {
            s.render(app, &status(), None, None, None).unwrap();
            stream.0.extend(s.take_out().0);
        };
        render(&mut app, &mut s, &mut stream);
        for ev in [
            TurnEvent::Thinking {
                delta: "reasoning about the task\n".into(),
            },
            TurnEvent::Text {
                delta: "I will now check. ".into(),
            },
            TurnEvent::ToolCall {
                name: "bash".into(),
                arguments: "\"command\":\"echo threedee\"".into(),
            },
            TurnEvent::ToolResult {
                name: "bash".into(),
                output: "threedee".into(),
                images: Vec::new(),
            },
            TurnEvent::Text {
                delta: "All finished with the requested task.".into(),
            },
        ] {
            app.on_turn_event(ev);
            render(&mut app, &mut s, &mut stream);
        }
        let rows = stream.emulated_screen(80, 30);
        let count = |needle: &str| rows.iter().filter(|r| r.contains(needle)).count();
        assert_eq!(count("❯ question"), 1, "prompt overwritten: {rows:?}");
        assert_eq!(
            count("reasoning about the task"),
            1,
            "thinking overwritten: {rows:?}"
        );
        assert_eq!(
            count("I will now check"),
            1,
            "assistant text overwritten: {rows:?}"
        );
        assert_eq!(count("echo threedee"), 1, "call overwritten: {rows:?}");
        assert_eq!(
            count("└─ bash: threedee"),
            1,
            "result overwritten: {rows:?}"
        );
        assert_eq!(
            count("All finished with the requested task"),
            1,
            "final text: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains("deepinfra ·")).count(),
            1,
            "status: {rows:?}"
        );
    }

    fn status() -> Status<'static> {
        Status {
            provider: "deepinfra",
            model: Some("mock/model"),
            thinking: None,
            session: None,
            cost: None,
            context_window: Some(10000),
        }
    }

    #[test]
    fn committed_rows_are_printed_once() {
        let buf = Buf::default();
        let mut app = App::new(None);
        app.push_user("hello");
        let mut s = screen(buf, 60, 10);
        s.render(&mut app, &status(), None, None, None).unwrap();
        assert_eq!(s.take_out().count("❯ hello"), 1);

        // A re-render with no new entries must not reprint the transcript.
        s.render(&mut app, &status(), None, None, None).unwrap();
        assert_eq!(s.take_out().count("❯ hello"), 0);
    }

    #[test]
    fn streaming_tail_is_rewritten_and_then_committed() {
        let buf = Buf::default();
        let mut app = App::new(None);
        let mut s = screen(buf, 60, 10);
        app.on_turn_event(TurnEvent::Text {
            delta: "alpha".into(),
        });
        s.render(&mut app, &status(), None, None, None).unwrap();
        assert_eq!(s.take_out().count("alpha"), 1);

        app.on_turn_event(TurnEvent::Text {
            delta: " beta".into(),
        });
        s.render(&mut app, &status(), None, None, None).unwrap();
        let text = s.take_out().text();
        assert_eq!(text.matches("alpha").count(), 1, "tail is rewritten");
        assert_eq!(text.matches("beta").count(), 1);

        // Once the entry is finalized it is committed and stays put.
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: "{}".into(),
        });
        s.render(&mut app, &status(), None, None, None).unwrap();
        let text = s.take_out().text();
        assert_eq!(text.matches("alpha").count(), 1, "final entry reprinted");
        assert!(text.contains("● bash"));
        s.render(&mut app, &status(), None, None, None).unwrap();
        let text = s.take_out().text();
        assert_eq!(text.matches("alpha").count(), 0, "then left alone");
        assert_eq!(text.matches("● bash").count(), 0);
    }

    #[test]
    fn a_long_streaming_entry_commits_its_oldest_rows() {
        let buf = Buf::default();
        let mut app = App::new(None);
        let mut s = screen(buf, 60, 5); // keep = 5 - 2 - 1 = 2 tail rows
        for n in 1..=6 {
            app.on_turn_event(TurnEvent::Text {
                delta: format!("line-{n}\n"),
            });
            s.render(&mut app, &status(), None, None, None).unwrap();
        }
        let out = s.take_out().text();
        // The assistant entry ends with a trailing blank line (the new
        // separator), so "line-1" appears twice in the raw byte stream:
        // once committed permanently, once still in the redrawable tail
        // from the previous render's last frame. The "0% ctx" markers
        // (the estimate crossed a 10-token step) also vanished: with a
        // blank separator line in the tail the tail grew by one row.
        assert!(out.matches("line-1").count() >= 1);
        assert_eq!(out.matches("line-6").count(), 1);
    }

    /// The tail must be redrawn in place: at column 0, on the same rows
    /// as the first render, with stale characters from longer previous
    /// rows erased. Regression test: the old render moved up the full
    /// tail height (off by one → drift upward) and kept the previous
    /// caret column (→ drift right) without clearing row remainders.
    #[test]
    fn redraw_keeps_the_tail_in_place() {
        let mut app = App::new(None);
        let mut s = Screen::new(Buf::default(), 80, 8);
        s.render(&mut app, &status(), None, None, None).unwrap(); // placeholder
        app.input = "Say hello".into();
        app.cursor = 9;
        s.render(&mut app, &status(), None, None, None).unwrap();

        let rows = s.take_out().emulated_screen(80, 8);
        assert!(
            rows[..6].iter().all(|r| r.trim().is_empty()),
            "history above the tail untouched: {rows:?}"
        );
        let status_row = &rows[6];
        assert!(
            status_row.starts_with("deepinfra · mock/model"),
            "{status_row}"
        );
        assert!(
            status_row.trim_end().ends_with("/ commands · ctrl+c quit"),
            "{status_row}"
        );
        // The last column is reserved (never painted) so the terminal's
        // pending-wrap state is never entered; the row spans the rest.
        assert_eq!(
            status_row.chars().count(),
            80,
            "one status row, last column left blank: {status_row:?}"
        );
        let input_row = &rows[7];
        assert_eq!(input_row.trim_end(), " ❯ Say hello", "{input_row}");
        assert_eq!(input_row.chars().count(), 80, "placeholder remnants erased");
    }

    /// Regression (editor roundtrip): returning from `$EDITOR` used to
    /// print the tail at the bottom edge via a relative "move down a lot"
    /// (CNL with a huge count), which some terminals (VTE: xfce4-terminal)
    /// treat as scrolling instead of clamping — duplicating the status
    /// line one row up on every editor return. The renderer now positions
    /// with absolute CUP and ends the block on the bottom row instead of
    /// printing past it, so nothing ever scrolls.
    #[test]
    fn editor_roundtrip_does_not_scroll_the_status_line() {
        let mut app = App::new(None);
        app.push_user("hello");
        let mut s = Screen::new(Buf::default(), 80, 8);
        s.render(&mut app, &status(), None, None, None).unwrap();
        // ctrl+e: the TUI resyncs at the same size, then renders.
        let (w, h) = s.size();
        s.resync(&mut app, w, h);
        s.render(&mut app, &status(), None, None, None).unwrap();

        let rows = s.take_out().emulated_screen(80, 8);
        let status_rows = rows
            .iter()
            .filter(|r| r.contains("deepinfra · mock/model"))
            .count();
        assert_eq!(status_rows, 1, "one status line, not scrolled: {rows:?}");
        assert!(
            rows[6].contains("deepinfra"),
            "status above the input: {rows:?}"
        );
        assert!(
            rows[7].contains("type a message…"),
            "input on the bottom row: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("❯ hello")),
            "the committed prompt stays in history: {rows:?}"
        );
    }

    /// Regression (resize): each resize used to print the tail at the
    /// bottom edge (scrolling one row per redraw), so resizing streamed a
    /// ladder of status lines upward. After a resync the block is drawn
    /// ending on the bottom row, and further renders rewrite it in place.
    #[test]
    fn resize_re_anchors_and_then_rewrites_in_place() {
        let mut app = App::new(None);
        app.push_user("hello");
        let mut s = Screen::new(Buf::default(), 80, 8);
        s.render(&mut app, &status(), None, None, None).unwrap();

        // Widen the terminal: resync, then two more renders.
        s.resync(&mut app, 100, 8);
        s.render(&mut app, &status(), None, None, None).unwrap();
        s.render(&mut app, &status(), None, None, None).unwrap();

        let rows = s.take_out().emulated_screen(100, 8);
        let status_rows = rows
            .iter()
            .filter(|r| r.contains("deepinfra · mock/model"))
            .count();
        assert_eq!(status_rows, 1, "no status-line ladder: {rows:?}");
        assert!(rows[6].contains("deepinfra"), "{rows:?}");
        assert!(rows[7].contains("type a message…"), "{rows:?}");
        // Everything above the re-anchored block is untouched.
        assert!(rows[..4].iter().all(|r| r.trim().is_empty()), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("❯ hello")),
            "the committed prompt stays put: {rows:?}"
        );
    }

    /// Typing one character at a time: every redraw rewrites the input
    /// row in place. Regression (caret flicker): the redraw used to mis-
    /// position when the caret's parked row fed back into the next
    /// frame's move, so each keystroke redrew the tail a row high and
    /// then corrected it — the input (and everything above) visibly
    /// jumped once per keystroke.
    #[test]
    fn typing_rewrites_the_input_row_in_place_every_keystroke() {
        let mut app = App::new(None);
        app.push_user("earlier message");
        let mut s = screen(Buf::default(), 80, 10);
        let mut stream = Buf::default();
        s.render(&mut app, &status(), None, None, None).unwrap();
        stream.0.extend(s.take_out().0);
        // Accumulate every frame into one stream and replay it in a real
        // (bottom-anchored) emulator: per-frame fresh replays are only
        // valid when a frame ends on the bottom row, which a parked
        // caret breaks.
        for text in [
            Some(""),
            Some("S"),
            Some("Sa"),
            Some("Say"),
            Some("Say "),
            Some("Say h"),
            Some("Say he"),
            Some("Say hel"),
        ] {
            if let Some(text) = text {
                app.input = text.into();
                app.cursor = text.len();
            }
            s.render(&mut app, &status(), None, None, None).unwrap();
            let frame = s.take_out();
            let rows = frame.emulated_screen(80, 10);
            stream.0.extend(frame.0);
            if let Some(text) = text {
                if text.is_empty() {
                    continue;
                }
                let status = rows.iter().filter(|r| r.contains("deepinfra ·")).count();
                let input = rows.iter().filter(|r| r.contains(text)).count();
                assert_eq!(status, 1, "({text}) status duplicated/lost: {rows:?}");
                assert_eq!(input, 1, "({text}) input row duplicated/lost: {rows:?}");
            }
        }
        let rows = stream.emulated_screen(80, 10);
        // Over the whole typing session the transcript above the tail
        // never gains duplicates...
        assert_eq!(
            rows.iter()
                .filter(|r| r.contains("❯ earlier message"))
                .count(),
            1,
            "prompt duplicated/lost: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains("deepinfra ·")).count(),
            1,
            "status duplicated: {rows:?}"
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.trim_end().ends_with("Say hel"))
                .count(),
            1,
            "final input duplicated: {rows:?}"
        );
    }

    /// The terminal's last column is never painted: writing it sets the
    /// pending-wrap flag (a following CR/LF then scrolls, and GNU screen
    /// defers displaying the character until the screen scrolls — the
    /// "invisible last column"). Every row the renderer emits stops one
    /// column short; the status line pads up to (but not including) it.
    #[test]
    fn rows_never_paint_the_last_column() {
        let mut app = App::new(None);
        app.busy = true; // full-width status right edge
        let mut s = Screen::new(Buf::default(), 40, 8);
        s.render(&mut app, &status(), None, None, None).unwrap();
        let text = s.take_out().text();
        // No printable row in the stream reaches 40 display columns
        // (escape codes are already stripped by `text`).
        for line in text.split('\n') {
            assert!(
                line.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() < 40,
                "row painted into the last column: {line:?}"
            );
        }
    }

    /// Regression (absolute addressing): steady-state redraws used to be
    /// positioned with CUP against a tracked screen row, but printing a
    /// growing transcript scrolls the screen, which invalidates that
    /// row — every subsequent redraw then landed too low and pushed the
    /// status and input lines into scrollback, once per render, so
    /// scrollback filled with copies of the tail. Steady-state redraws
    /// must be cursor-relative (scroll-proof); only a fresh anchor uses
    /// CUP, and its block ends on the bottom row without scrolling.
    #[test]
    fn growing_transcript_never_pushes_the_tail_into_scrollback() {
        let mut app = App::new(None);
        let mut s = screen(Buf::default(), 40, 6);
        // One screenful of scrolling history: many finalized entries,
        // each rendered right after the previous (steady-state path).
        for n in 0..12 {
            app.push_user(&format!("msg-{n}"));
            s.render(&mut app, &status(), None, None, None).unwrap();
        }
        let (rows, history) = s.take_out().emulated(40, 6);
        // Scrollback holds only transcript lines, never tail rows.
        for r in &history {
            assert!(
                !r.contains("0% ctx") && !r.contains("type a message"),
                "a redraw pushed the tail into scrollback: {r:?}"
            );
        }
        // Exactly one status line, on the screen, above the input.
        let status_rows = rows
            .iter()
            .filter(|r| r.contains("deepinfra · mock/model"))
            .count();
        assert_eq!(status_rows, 1, "one status line: {rows:?}");
        assert!(rows[5].contains("type a message…"), "{rows:?}");
    }

    /// Regression (resize): a fresh anchor after a resize used to place
    /// the block at an absolute top row computed by subtraction, which
    /// saturates at row 0 when the block is taller than the screen (the
    /// streaming entry is reprinted in full at the new width); the block
    /// then printed past the bottom and scrolled — streaming status-line
    /// copies upward, the exact symptom resync was meant to fix. A fresh
    /// anchor must draw the block *ending* on the bottom row: rows that
    /// do not fit scroll once (transcript), never the tail repeatedly.
    #[test]
    fn resize_with_a_tall_streaming_entry_does_not_scroll_the_tail() {
        let mut app = App::new(None);
        app.busy = true;
        let mut s = screen(Buf::default(), 20, 6);
        for n in 1..=8 {
            app.on_turn_event(TurnEvent::Text {
                delta: format!("line-{n}\n"),
            });
        }
        s.render(&mut app, &status(), None, None, None).unwrap();

        // Resize: the anchor is lost, the streaming entry is reprinted
        // in full at the new width (taller than one screen here).
        s.resync(&mut app, 20, 6);
        s.render(&mut app, &status(), None, None, None).unwrap();

        let (rows, history) = s.take_out().emulated(20, 6);
        for r in &history {
            assert!(
                !r.contains("0% ctx") && !r.contains("type a message"),
                "the resize pushed the tail into scrollback: {r:?}"
            );
        }
        let status_rows = rows.iter().filter(|r| r.contains("0% ctx")).count();
        assert_eq!(status_rows, 1, "one status line: {rows:?}");
        assert!(rows[5].contains("❯"), "input on the bottom row: {rows:?}");
    }

    /// Steady-state redraws keep working while the tail *grows* across
    /// the bottom row (the streaming entry gets longer than the budget
    /// and its oldest rows commit, scrolling the screen between renders):
    /// the region stays put and the tail is rewritten in place, not
    /// pushed into scrollback.
    #[test]
    fn tail_growth_scrolls_once_per_commit_not_per_redraw() {
        let mut app = App::new(None);
        app.busy = true;
        let mut s = screen(Buf::default(), 20, 6);
        for n in 1..=16 {
            app.on_turn_event(TurnEvent::Text {
                delta: format!("line-{n}\n"),
            });
            s.render(&mut app, &status(), None, None, None).unwrap();
        }
        let (rows, history) = s.take_out().emulated(20, 6);
        for r in &history {
            assert!(
                !r.contains("0% ctx") && !r.contains("type a message"),
                "a redraw pushed the tail into scrollback: {r:?}"
            );
        }
        assert!(
            rows.iter().filter(|r| r.contains("0% ctx")).count() == 1,
            "{rows:?}"
        );
    }

    /// Regression test: multi-line tool output used to reach the
    /// terminal as raw `\n` characters inside one row; LF moves down
    /// without returning to column 0, so every output line started right
    /// below the end of the previous one (a staircase).
    #[test]
    fn multi_line_tool_output_prints_at_the_left_edge() {
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: "{}".into(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "aaa\nbbb".into(),
            images: Vec::new(),
        });
        let mut s = Screen::new(Buf::default(), 40, 12);
        s.render(&mut app, &status(), None, None, None).unwrap();
        let rows = s.take_out().emulated_screen(40, 12);
        let aaa = rows
            .iter()
            .find(|r| r.contains("aaa"))
            .expect("first output line printed");
        assert!(aaa.trim_end().ends_with("└─ bash: aaa"), "{aaa:?}");
        let bbb = rows
            .iter()
            .find(|r| r.contains("bbb"))
            .expect("second output line printed");
        // Column 11 = "  └─ " + "bash" + ": ": aligned under the first
        // output character, not under the end of the previous line.
        assert_eq!(bbb.trim_end(), " ".repeat(11) + "bbb", "{bbb:?}");
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

    /// Regression: a plain span after a styled one used to emit no SGR at
    /// all, so the earlier span's color and modifiers stayed active until
    /// the next styled span or the end of the line — e.g. `**bold** the
    /// `/code`` leaked bold past "bold" and cyan over the plain text
    /// between code spans (see the 20261005-023938 session replay).
    #[test]
    fn style_change_resets_the_previous_span_style() {
        let mut buf = Buf::default();
        print_line(
            &mut buf,
            &Line::from(vec![
                Span::styled("a".to_string(), Style::new().fg(Color::Cyan)),
                Span::raw(" b ".to_string()),
                Span::styled("c".to_string(), Style::new().add_modifier(Modifier::BOLD)),
            ]),
            80,
        )
        .unwrap();
        assert_eq!(buf.0, b"\x1b[0m\x1b[36ma\x1b[0m b \x1b[0m\x1b[1mc\x1b[0m");
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
        let mut app = App::new(None);
        app.busy = true;
        app.last_usage = Some(clanky_protocol::Usage {
            prompt_tokens: Some(120),
            completion_tokens: Some(80),
            cached_tokens: None,
        });
        let line = status_line(&app, &status(), 40, 0, 0);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("deepinfra · mock/model"), "{text}");
        // Streaming keeps the context use on the right; the dot moved to
        // the input prompt instead.
        assert!(text.contains("2% ctx"), "{text}");
        assert!(!text.contains("streaming"), "{text}");
        assert_eq!(text.chars().count(), 40, "padded to the full width");

        app.busy = false;
        app.pending = vec!["x".into()];
        let line = status_line(&app, &status(), 40, 0, 0);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("○ queued"), "{text}");
    }

    #[test]
    fn status_line_decontents_gracefully_as_the_terminal_narrows() {
        let mut app = App::new(None);
        app.last_usage = Some(clanky_protocol::Usage {
            prompt_tokens: Some(2500),
            completion_tokens: Some(500),
            cached_tokens: None,
        });
        app.pending = vec!["x".into()];
        let mut st = status();
        st.provider = "deepinfra";
        st.model = Some("meta-llama/Llama-4-Maverick-17B-128E-Instruct");
        st.thinking = Some("12345");
        st.session = Some("20261005-0239");
        st.cost = Some(0.1234);
        st.context_window = Some(10000);

        let text = |w: u16| -> String {
            status_line(&app, &st, w, 0, 0)
                .spans
                .iter()
                .map(|s| s.content.clone())
                .collect()
        };

        // Wide enough for everything: provider · model · think ·
        // session ○ queued ... $cost · ctx%.
        let full = text(120);
        assert!(
            full.contains(
                "deepinfra · meta-llama/Llama-4-Maverick-17B-128E-Instruct · think 12345"
            ),
            "{full}"
        );
        assert!(full.contains("20261005-0239 ○ queued"), "{full}");
        assert!(full.ends_with("$0.1234 · 30% ctx"), "{full}");

        // Each narrowing drops exactly one element, in order:
        // provider, thinking, model, cost, session, the "queued" word.
        let t = text(103);
        assert!(!t.contains("deepinfra"), "{t}");
        assert!(
            t.starts_with("meta-llama/Llama-4-Maverick-17B-128E-Instruct · think 12345"),
            "{t}"
        );
        let t = text(89);
        assert!(!t.contains("think"), "{t}");
        assert!(
            t.starts_with("meta-llama/Llama-4-Maverick-17B-128E-Instruct · 20261005-0239"),
            "{t}"
        );
        let t = text(41);
        assert!(!t.contains("meta-llama"), "{t}");
        assert!(t.starts_with("20261005-0239 ○ queued"), "{t}");
        let t = text(22);
        assert!(!t.contains("20261005-0239"), "{t}");
        assert!(t.starts_with("○ queued"), "{t}");
        // The queued word reduces to the bare dot; the context
        // percentage is the last element standing.
        let t = text(10);
        assert_eq!(t, "○  30% ctx");

        // Without price data the hints replace cost and context, and are
        // the last element to go.
        st.cost = None;
        st.context_window = None;
        app.last_usage = None;
        app.pending.clear();
        let text = |w: u16| -> String {
            status_line(&app, &st, w, 0, 0)
                .spans
                .iter()
                .map(|s| s.content.clone())
                .collect()
        };
        let t = text(116);
        assert!(t.ends_with("/ commands · ctrl+c quit"), "{t}");
        let t = text(39);
        assert!(!t.contains("meta-llama"), "{t}");
        assert!(t.starts_with("20261005-0239"), "{t}");
        assert!(t.ends_with("/ commands · ctrl+c quit"), "{t}");
        let t = text(24);
        assert_eq!(t, "/ commands · ctrl+c quit");
        // Only the provider fits below that.
        let t = text(10);
        assert_eq!(t, "deepinfra");
    }

    #[test]
    fn hidden_thinking_renders_as_a_placeholder_line() {
        let mut app = App::new(None);
        app.toggle_thinking_display(); // off
        app.on_turn_event(TurnEvent::Thinking {
            delta: "secret reasoning".into(),
        });
        let text: String = app
            .transcript_lines(80)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<String>>()
            .join("\n");
        assert!(text.contains("Thinking..."), "{text}");
        assert!(!text.contains("secret reasoning"), "{text}");
    }

    #[test]
    fn status_line_shows_thinking_and_cost() {
        let app = App::new(None);
        let status = Status {
            provider: "deepinfra",
            model: Some("mock/model"),
            thinking: Some("4096"),
            session: Some("s"),
            cost: Some(0.0123),
            context_window: None,
        };
        let line = status_line(&app, &status, 80, 0, 0);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("mock/model · think 4096"), "{text}");
        // Cost sits on the right side.
        assert!(text.contains("s") && text.contains("$0.0123"), "{text}");

        // Large costs drop to two decimals.
        assert_eq!(format_cost(12.5), "$12.50");
        assert_eq!(format_cost(0.000_099), "$0.0001");
    }

    #[test]
    fn estimates_count_into_the_display_while_nothing_is_billed() {
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::Thinking {
            delta: "x".repeat(96),
        });
        // 96 counted chars -> 3 steps * 32 chars / 3 = 32 estimated
        // completion tokens; the display shows them with no billed
        // usage in hand, plus a prompt estimate passed in for the user
        // message (counted via `push_user` in the real flow).
        let (est_p, est_c) = app.estimated_tokens();
        let line = status_line(&app, &status(), 80, est_p + 13, est_c);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        // (13 + 32) / 10_000 with a known window.
        assert!(text.contains("0% ctx"), "{text}");

        // The estimate feeds the context percentage too.
        let line = status_line(&app, &status(), 80, 1_000, 0);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        // (1000 + 21) / 10_000 -> 10%.
        assert!(text.contains("10% ctx"), "{text}");
    }

    #[test]
    fn usage_shows_context_percent_when_the_window_is_known() {
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::Usage {
            usage: clanky_protocol::Usage {
                prompt_tokens: Some(17255),
                completion_tokens: Some(796),
                cached_tokens: None,
            },
        });

        // No known window: the hint fills the right side instead.
        let unknown = Status {
            context_window: None,
            ..status()
        };
        let line = status_line(&app, &unknown, 80, 0, 0);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("/ commands · ctrl+c quit"), "{text}");
        assert!(!text.contains("ctx"), "{text}");

        // Known window: the consumed percentage.
        let known = Status {
            context_window: Some(128_000),
            ..status()
        };
        let line = status_line(&app, &known, 80, 0, 0);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("14% ctx"), "{text}");

        // Integer division: 18_051 / 100_000 → 18% (floors, never 19).
        let known = Status {
            context_window: Some(100_000),
            ..status()
        };
        let line = status_line(&app, &known, 80, 0, 0);
        let text: String = line.spans.iter().map(|s| s.content.clone()).collect();
        assert!(text.contains("18% ctx"), "{text}");
    }

    fn input_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.clone()).collect()
    }

    #[test]
    fn input_rows_show_placeholder_and_window_long_input() {
        let mut app = App::new(None);
        let input = input_rows(&app, 40, 24);
        assert_eq!(input.lines.len(), 1);
        let text = input_text(&input.lines[0]);
        assert!(text.contains("type a message…"), "{text}");
        assert_eq!(
            input.caret,
            Some((0, PROMPT_WIDTH as u16)),
            "caret after the dot and prompt symbol"
        );

        app.input = "hi".into();
        app.cursor = 2;
        let input = input_rows(&app, 40, 24);
        assert_eq!(input.lines.len(), 1);
        let text = input_text(&input.lines[0]);
        assert!(text.contains("❯ hi"), "{text}");
        assert_eq!(input.caret, Some((0, 5)));

        // Long input: the caret stays visible at the right edge.
        app.input = "x".repeat(100);
        app.cursor = 100;
        let input = input_rows(&app, 40, 24);
        assert_eq!(input.lines.len(), 1);
        assert!(input.caret.map(|(_, c)| c) > Some(0));
    }

    /// RED: after leaving the second input row (↑) and returning to
    /// left/right edits on the first row, the tail redraw started below
    /// the tail's top, leaving stale copies of the rows below — the
    /// duplicated "second line".
    #[test]
    fn leaving_the_last_input_row_then_editing_does_not_duplicate_rows() {
        let mut app = App::new(None);
        app.input = "first line\nsecond line".into();
        app.cursor = app.input.len();
        let mut s = Screen::new(Buf::default(), 40, 8);

        // Fill the tail, then ↑ onto the first row and edit on it.
        s.render(&mut app, &status(), None, None, None).unwrap();
        app.cursor_up();
        s.render(&mut app, &status(), None, None, None).unwrap();
        app.cursor_left();
        app.cursor_right();
        s.render(&mut app, &status(), None, None, None).unwrap();

        // Replay all three renders on a fresh emulated screen: the last
        // render must have rewritten the tail in place, not printed a
        // second copy below it (that stale copy is the duplicated row).
        let all = s.take_out();
        let rows = all.emulated_screen(40, 8);
        assert_eq!(
            rows.iter().filter(|r| r.contains("second line")).count(),
            1,
            "the second input row must appear exactly once: {rows:?}"
        );
        assert_eq!(
            rows.iter().filter(|r| r.contains("first line")).count(),
            1,
            "the first input row must appear exactly once: {rows:?}"
        );
    }

    /// Regression (M12 #4): multi-line input (paste, `$EDITOR`) used to
    /// render as one row with embedded newlines, so each terminal
    /// wrapped line two columns after the previous one ended — a
    /// stair-step. Each physical line now gets its own row, indented
    /// under the prompt symbol so every line starts at the same column.
    #[test]
    fn multi_line_input_starts_each_row_at_the_prompt_column() {
        let mut app = App::new(None);
        app.input = "line one\nline two\nline three".into();
        app.cursor = app.input.len();
        let mut s = Screen::new(Buf::default(), 40, 8);
        s.render(&mut app, &status(), None, None, None).unwrap();
        let rows = s.take_out().emulated_screen(40, 8);
        // Rows: status, then one input row per physical line, ending on
        // the bottom row. Every input row starts at the same column.
        assert_eq!(input_text_row(&rows, "line one"), 5, "{rows:?}");
        assert_eq!(input_text_row(&rows, "line two"), 6, "{rows:?}");
        assert_eq!(input_text_row(&rows, "line three"), 7, "{rows:?}");
        for (needle, expect) in [("line one", 3), ("line two", 3), ("line three", 3)] {
            let row = &rows[input_text_row(&rows, needle)];
            assert_eq!(
                row.find(needle).map(|i| row[..i].chars().count()),
                Some(expect),
                "{needle} starts at column {expect}: {row}"
            );
        }
        // The caret sits past the end of the last line, on the bottom row.
        // (The caret move is a column-only CHA; the bottom row is where the
        // block ends.)
    }

    /// The `project` setting is printed at the left of the input prompt:
    /// the label, then the unchanged dot + prompt symbol, then the text.
    #[test]
    fn project_label_sits_at_the_left_of_the_input_prompt() {
        let mut app = App::new(Some("clanky".into()));
        app.input = "hi".into();
        app.cursor = 2;
        let input = input_rows(&app, 40, 24);
        assert_eq!(input.lines.len(), 1);
        let text = input_text(&input.lines[0]);
        assert!(text.contains("clanky  ❯ hi"), "{text}");
        // The caret column follows the wider prefix.
        assert_eq!(
            input.caret,
            Some((0, (PROMPT_WIDTH + "clanky".len() + 1 + 2) as u16))
        );
    }

    /// With a project label, the streaming dot still sits between the
    /// label and the prompt symbol, and continuation rows indent past
    /// the whole prefix.
    #[test]
    fn project_label_keeps_the_dot_and_alignment() {
        let mut app = App::new(Some("proj".into()));
        app.busy = true;
        let input = input_rows(&app, 40, 24);
        let text = input_text(&input.lines[0]);
        assert!(text.contains("proj ●❯"), "{text}");

        // Multi-line: every continuation row starts after the full
        // prefix ("proj " + dot + prompt symbol = 8 columns).
        app.busy = false;
        app.input = "one\ntwo".into();
        app.cursor = app.input.len();
        let mut s = Screen::new(Buf::default(), 40, 8);
        s.render(&mut app, &status(), None, None, None).unwrap();
        let rows = s.take_out().emulated_screen(40, 8);
        for needle in ["one", "two"] {
            let row = &rows[input_text_row(&rows, needle)];
            assert_eq!(
                row.find(needle).map(|i| row[..i].chars().count()),
                Some(8),
                "{needle} starts at column 8: {row}"
            );
        }
    }

    /// No `project` setting: the input prompt is exactly as before
    /// (three-column prefix, no label).
    #[test]
    fn no_project_label_leaves_the_prompt_unchanged() {
        let mut app = App::new(None);
        app.input = "hi".into();
        app.cursor = 2;
        let input = input_rows(&app, 40, 24);
        let text = input_text(&input.lines[0]);
        assert!(text.contains("❯ hi"), "{text}");
        assert!(!text.contains("clanky"), "{text}");
        assert_eq!(input.caret, Some((0, (PROMPT_WIDTH + 2) as u16)));
    }

    /// A trailing newline yields an empty final input row, and the caret
    /// (at the end of the buffer) sits on it.
    #[test]
    fn trailing_newline_gives_the_caret_an_empty_row() {
        let mut app = App::new(None);
        app.input = "hello\n".into();
        app.cursor = app.input.len();
        let mut s = Screen::new(Buf::default(), 40, 8);
        s.render(&mut app, &status(), None, None, None).unwrap();
        let buf = s.take_out();
        let rows = buf.emulated_screen(40, 8);
        let hello_row = rows.iter().position(|r| r.contains("❯ hello")).unwrap();
        let empty_row = hello_row + 1;
        assert!(rows[empty_row].trim().is_empty(), "{rows:?}");
        assert_eq!(
            empty_row, 7,
            "caret's empty row is the bottom row: {rows:?}"
        );
        // The caret parks on its own empty row, in the prompt column
        // (MoveToColumn(3) emits CHA 4), with no CUU.
        let out = String::from_utf8_lossy(&buf.0);
        assert!(
            out.contains("\x1b[4G"),
            "caret in the prompt column: {out:?}"
        );
        assert!(
            !out.contains('A'),
            "no CUU — the caret is on the bottom row: {out:?}"
        );
    }

    /// The caret in a multi-line buffer lands on the row containing the
    /// cursor — the renderer emits CUU (move up) past the later rows.
    #[test]
    fn caret_moves_to_the_row_with_the_cursor() {
        let mut app = App::new(None);
        app.input = "aaaa\nbbbb\ncccc".into();
        app.cursor = 5; // start of "bbbb" (after the first newline)
        let mut s = Screen::new(Buf::default(), 40, 10);
        s.render(&mut app, &status(), None, None, None).unwrap();
        let buf = s.take_out();
        let out = String::from_utf8_lossy(&buf.0);
        // The tail spans status + 3 input rows; the caret is on the
        // middle input row, one row above the tail's bottom.
        assert!(out.contains("\x1b[1A"), "move up one row: {out:?}");
        assert!(
            out.contains("\x1b[4G"),
            "caret in the prompt column: {out:?}"
        );
    }

    /// More input lines than the footer budget: the rows around the
    /// caret are kept so the caret stays on screen.
    #[test]
    fn input_rows_trim_to_the_budget_around_the_caret() {
        let mut app = App::new(None);
        app.input = (0..20)
            .map(|i| format!("row-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.cursor = app.input.find("row-10").unwrap();
        let input = input_rows(&app, 40, 6);
        assert_eq!(input.lines.len(), 6, "trimmed to the budget");
        let texts: Vec<String> = input.lines.iter().map(input_text).collect();
        // The caret row is kept; the window is centered on it.
        assert!(texts.iter().any(|t| t.contains("row-10")), "{texts:?}");
        // The window is centered on the caret: two rows above, three
        // below (budget 6).
        assert_eq!(input.caret.map(|(r, _)| r), Some(3), "{texts:?}");
        assert!(
            texts[0].contains("row-7") && texts[5].contains("row-12"),
            "{texts:?}"
        );
    }

    /// The prompt width constant must match the actual prompt pieces.
    #[test]
    fn prompt_width_matches_the_rendered_prompt() {
        let mut app = App::new(None);
        app.input = "x".into();
        app.cursor = 1;
        let input = input_rows(&app, 40, 24);
        let text = input_text(&input.lines[0]);
        // " ●" + "❯ " — the dot plus the prompt symbol.
        assert_eq!(
            text.find('x').map(|i| text[..i].chars().count()),
            Some(PROMPT_WIDTH) // x starts right after the prompt
        );
        assert_eq!(INPUT_PROMPT.width() + 1, PROMPT_WIDTH);
    }

    fn input_text_row(rows: &[String], needle: &str) -> usize {
        rows.iter().position(|r| r.contains(needle)).unwrap()
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
    fn modal_box_shows_the_prompt_and_the_field() {
        let modal = Modal::new("export markdown", "file name", "notes.md");
        let (rows, (caret_row, caret_col)) = modal_box(&modal, 60, 20);
        let texts: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert!(texts[0].contains("export markdown"), "{texts:?}");
        assert!(texts[0].starts_with('╭') && texts[0].ends_with('╮'));
        assert!(texts.iter().any(|t| t.contains("file name")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("notes.md")), "{texts:?}");
        assert!(texts[texts.len() - 1].starts_with('╰'));
        // Caret position: on the field row (top border, prompt, field), at
        // border + padding, then the field text.
        assert_eq!(caret_row, 2);
        assert_eq!(caret_col, 2 + "notes.md".len() as u16);

        // Every row is the same display width.
        let widths: Vec<usize> = rows.iter().map(line_width).collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
    }

    #[test]
    fn modal_box_skips_when_the_terminal_is_tiny() {
        let modal = Modal::new("export markdown", "file name", "");
        assert!(modal_box(&modal, 20, 20).0.is_empty());
        assert!(modal_box(&modal, 60, 6).0.is_empty());
    }

    #[test]
    fn modal_box_windows_a_long_value_and_keeps_the_caret_inside() {
        let long = "x".repeat(200);
        let modal = Modal::new("export markdown", "file name", &long);
        let (rows, (caret_row, caret)) = modal_box(&modal, 40, 20);
        assert!(!rows.is_empty());
        assert_eq!(caret_row, 2, "the caret sits on the field row");
        assert!((2..40).contains(&caret), "caret inside the box: {caret}");
        assert!(
            rows.iter().all(|l| line_width(l) <= 40),
            "no row exceeds the screen width"
        );
    }

    #[test]
    fn an_open_modal_owns_the_caret() {
        let mut app = App::new(None);
        app.input = "typing".into();
        app.cursor = 6;
        let modal = Modal::new("export markdown", "file name", "a.md");
        let mut s = Screen::new(Buf::default(), 60, 12);
        s.render(&mut app, &status(), None, Some(&modal), None)
            .unwrap();
        let rows = s.take_out().emulated_screen(60, 12);
        assert!(
            rows.iter().any(|r| r.contains("export markdown")),
            "{rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains("a.md")), "{rows:?}");
        // The main input row is still drawn (dimmed behind the dialog).
        assert!(rows.iter().any(|r| r.contains("❯ typing")), "{rows:?}");
    }

    #[test]
    fn completion_box_lists_candidates() {
        let mut app = App::new(None);
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

    /// Regression (control characters): `print_line` must never emit
    /// control characters. `unicode-width` reports no width for them, so
    /// the wrap math budgets zero columns — but a real terminal does
    /// move the cursor for a tab (to the next tab stop, possibly onto or
    /// past the reserved last column, setting the pending-wrap flag so
    /// the following CR/LF scrolls the screen: duplicated status lines
    /// that the next render overwrites). Raw ESC bytes could likewise be
    /// taken as control sequences. Symptom: streamed thinking blocks
    /// (raw, uncapped prose — often containing tabs) made the status line
    /// duplicate and shift upward while tool-call text printed cleanly.
    #[test]
    fn print_line_never_emits_control_characters() {
        let mut out = Buf::default();
        let line = Line::from(Span::raw("a\tb\rc\x1b[2Jd\x7f e"));
        // The ESC of the embedded "\x1b[2J" is dropped, but its printable
        // "[2J" letters survive (they are not control characters).
        print_line(&mut out, &line, 40).unwrap();
        let text = out.0;
        assert!(!text.contains(&b'\t'), "tab emitted: {text:?}");
        assert!(!text.contains(&b'\r'), "CR emitted: {text:?}");
        // Only the SGR reset/attribute bytes we emit ourselves may be ESC.
        let stripped: Vec<u8> = text
            .iter()
            .copied()
            .filter(|&b| !(0x20..0x7f).contains(&b) && b >= 0x80)
            .collect();
        assert!(stripped.is_empty(), "unexpected control bytes: {text:?}");
        assert!(std::str::from_utf8(&text).unwrap().contains("abc[2Jd e"));
    }

    /// Thinking text with tabs (code snippets inside reasoning blocks)
    /// is expanded to tab stops before wrapping, so the wrap math and
    /// the terminal agree on the row layout — no stale remnants and no
    /// rows pushed out of place.
    #[test]
    fn thinking_tabs_expand_to_stable_rows() {
        let mut app = App::new(None);
        app.on_turn_event(TurnEvent::Thinking {
            delta: "code:\n\tfoo();\n\t\tbar(); done".into(),
        });
        let mut s = screen(Buf::default(), 30, 10);
        s.render(&mut app, &status(), None, None, None).unwrap();
        s.render(&mut app, &status(), None, None, None).unwrap();
        let rows = s.take_out().emulated_screen(30, 10);
        let joined = rows.join("\n");
        assert!(joined.contains("        foo();"), "{rows:?}");
        assert!(joined.contains("                bar();"), "{rows:?}");
        assert!(
            rows.iter().filter(|r| r.contains("commands")).count() == 1,
            "one status line: {rows:?}"
        );
    }

    /// A thinking block streaming past one screenful (committing its
    /// oldest rows, rewriting the tail every delta) leaves exactly one
    /// status line on screen and none in scrollback.
    #[test]
    fn streaming_thinking_overflow_keeps_one_status_line() {
        let mut app = App::new(None);
        app.busy = true;
        let mut s = screen(Buf::default(), 40, 10);
        s.render(&mut app, &status(), None, None, None).unwrap();
        for n in 0..40 {
            app.on_turn_event(TurnEvent::Thinking {
                delta: format!("word-{n} "),
            });
            s.render(&mut app, &status(), None, None, None).unwrap();
        }
        let (rows, history) = s.take_out().emulated(40, 10);
        let status_rows = rows.iter().filter(|r| r.contains("deepinfra")).count();
        assert_eq!(status_rows, 1, "screen: {rows:?}");
        let hist_status = history.iter().filter(|r| r.contains("deepinfra")).count();
        assert_eq!(hist_status, 0, "scrollback: {history:?}");
    }
}

#[cfg(test)]
mod bg_tests {
    use super::*;

    #[test]
    fn sgr_emits_background_codes() {
        assert_eq!(sgr(Style::new().bg(Color::Blue)), "\x1b[44m");
        assert_eq!(sgr(Style::new().bg(Color::LightBlue)), "\x1b[104m");
        // Combined with fg and modifiers, in a stable order: bg after fg,
        // modifiers last (matching how `codes` is built). Note ratatui's
        // `White` is the bright variant (97/107); `Gray` is 37/47.
        assert_eq!(
            sgr(Style::new()
                .fg(Color::White)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD)),
            "\x1b[97;41;1m"
        );
    }

    #[test]
    fn sgr_emits_indexed_256_color_codes() {
        // Palette colors emit as indexed: fg `38;5;n`, bg `48;5;n`.
        assert_eq!(sgr(Style::new().fg(Color::Indexed(87))), "\x1b[38;5;87m");
        assert_eq!(sgr(Style::new().bg(Color::Indexed(87))), "\x1b[48;5;87m");
        // Both together, in the same order as the static arms.
        assert_eq!(
            sgr(Style::new().fg(Color::Indexed(15)).bg(Color::Indexed(87))),
            "\x1b[38;5;15;48;5;87m"
        );
    }

    #[test]
    fn print_line_pads_bg_rows_to_width() {
        let line = Line::from(Span::styled("hi", Style::new().bg(Color::Red)));
        let mut out = Vec::new();
        print_line(&mut out, &line, 10).unwrap();
        let s = String::from_utf8(out).unwrap();
        // One styled run: the pad shares the span's SGR (still active),
        // so the row is a single continuous background block. Width 10:
        // "hi" plus 8 pad spaces.
        assert_eq!(s, "\x1b[0m\x1b[41mhi        \x1b[0m");
    }
}
