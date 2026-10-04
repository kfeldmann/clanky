//! Interactive TUI (M3): chat view, input line, streaming render.
//!
//! The agentic turn runs on a worker thread so the UI stays responsive
//! while the model streams and tools execute; [`TurnEvent`]s cross to the
//! render loop over a channel. Ctrl+C quits; `clanky` falls back to
//! command-line mode when stdin or stdout is redirected (see `main.rs`).
//!
//! Sessions (M4): every submitted prompt and every turn event is appended
//! to a JSONL session file under `./.clanky/sessions/` (autosave), so a
//! killed TUI loses at most the event being written. `/name <name>` renames
//! the file, `/resume` reopens a saved session via a picker; resuming
//! restores the transcript and the full conversation history.
//!
//! Layout (see `ui.rs`):
//! ┌────────────────────────────────┐
//! │ transcript (markdown, scroll)  │
//! │ provider · model · session  ●  │
//! │ ❯ input line                   │
//! └────────────────────────────────┘

mod app;
mod markdown;
mod picker;
mod ui;

use std::io::{IsTerminal as _, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use clanky_protocol::ChatMessage;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::error::Result;
use crate::session::{self, SessionInfo, SessionWriter};
use crate::settings::{SamplingParams, Settings};
use crate::turn::{TurnConfig, TurnEvent, TurnOutput};

/// Half a screen per PageUp/PageDown press.
const SCROLL_PAGE: u16 = 16;

/// True when the terminal can run the TUI: both stdin and stdout must be
/// attached to a terminal (plan.md: no TUI when redirected).
pub fn available() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Configuration for one interactive session, resolved once in `main`.
#[derive(Debug, Clone)]
pub struct Launch {
    pub provider: String,
    pub model: Option<String>,
    pub sampling: Option<SamplingParams>,
    pub thinking: Option<String>,
    /// Prompt from the command line, sent as the first chat message.
    pub initial_prompt: Option<String>,
    /// `--resume NAME` loads that session; `--resume` (empty string) opens
    /// the picker at startup.
    pub resume: Option<String>,
}

impl Launch {
    /// Build from merged settings for the chosen provider. The model falls
    /// back to the provider default, mirroring pipe mode.
    pub fn from_settings(provider: &str, settings: &Settings) -> Self {
        Self {
            provider: provider.to_string(),
            model: settings
                .model
                .clone()
                .or_else(|| crate::provider::default_model(provider).map(str::to_string)),
            sampling: settings.sampling.clone(),
            thinking: settings.thinking.clone(),
            initial_prompt: settings
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string),
            resume: None,
        }
    }
}

/// What the worker thread reports back to the UI loop.
enum WorkerEvent {
    Turn(TurnEvent),
    /// Terminal status of the turn.
    Done(crate::error::Result<TurnOutput>),
}

/// Run the interactive session until the user quits.
pub fn run(launch: Launch) -> Result<()> {
    let mut terminal = setup()?;
    let result = event_loop(&launch, &mut terminal);
    match restore(&mut terminal) {
        Ok(()) => result,
        Err(restore_err) => {
            // Surface the original error if cleanup also fails.
            result.and(Err(restore_err))
        }
    }
}

fn setup() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    Ok(terminal)
}

fn restore(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Spawn one agentic turn on a worker thread. The provider handler is
/// created inside the thread, so `Handler` implementations need not be
/// `Send` (M8's process transport will be).
fn spawn_turn(history: Vec<ChatMessage>, launch: &Launch, prompt: String, tx: Sender<WorkerEvent>) {
    let launch = launch.clone();
    let mut messages = history;
    messages.push(ChatMessage::user(prompt));
    std::thread::Builder::new()
        .name("clanky-turn".into())
        .spawn(move || {
            let outcome: crate::error::Result<TurnOutput> = (|| {
                let handler = crate::provider::create(&launch.provider)?;
                let config = TurnConfig {
                    model: launch.model.clone(),
                    sampling: launch.sampling.clone(),
                    thinking: launch.thinking.clone(),
                };
                crate::turn::run_turn(
                    handler,
                    &crate::tools::default_tools(),
                    messages,
                    &config,
                    &mut |event: TurnEvent| {
                        let _ = tx.send(WorkerEvent::Turn(event));
                    },
                )
            })();
            let _ = tx.send(WorkerEvent::Done(outcome));
        })
        .expect("failed to spawn turn worker thread");
}

/// What a key press asks the main loop to do.
enum Action {
    None,
    Quit,
    Clear,
    Submit(String),
    /// Open the `/resume` picker over the current sessions.
    OpenPicker,
}

/// Slash commands (M4). M5 adds more and a picker component for pickers.
#[derive(Debug, PartialEq)]
enum Command {
    /// `/name [name]`: rename the session file.
    Name(Option<String>),
    /// `/resume`: pick and load a saved session.
    Resume,
}

/// Parse a slash command. `None` = not a command (plain prompt);
/// `Some(Err)` = unknown or malformed command (never sent to the model).
fn parse_command(input: &str) -> Option<std::result::Result<Command, String>> {
    let rest = input.strip_prefix('/')?;
    let (word, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    let arg = arg.trim();
    match word {
        "name" => Some(Ok(Command::Name(
            (!arg.is_empty()).then(|| arg.to_string()),
        ))),
        "resume" if arg.is_empty() => Some(Ok(Command::Resume)),
        other => Some(Err(format!(
            "unknown command `/{other}` (known: /name, /resume)"
        ))),
    }
}

/// Autosave state: the open session file, lazily created on the first
/// record. All file access happens on the UI thread; the worker only
/// streams events over the channel.
struct SessionState {
    dir: PathBuf,
    launch: Launch,
    writer: Option<SessionWriter>,
    /// Name requested via `/name` before any file exists yet.
    pending_name: Option<String>,
}

impl SessionState {
    fn new(launch: Launch) -> Self {
        Self {
            dir: session::sessions_dir(),
            launch,
            writer: None,
            pending_name: None,
        }
    }

    /// The current display name: the open file's stem, else the pending
    /// `/name` request.
    fn current_name(&self) -> Option<String> {
        self.writer
            .as_ref()
            .and_then(|w| w.path().file_stem().and_then(|s| s.to_str()))
            .map(str::to_string)
            .or_else(|| self.pending_name.clone())
    }

    /// Ensure a writer exists (using a pending `/name` if one was given)
    /// and append a record. Write failures surface as transcript errors.
    fn record(&mut self, app: &mut app::App, record: session::Record) {
        if let Err(err) = self
            .ensure_writer()
            .and_then(|writer| writer.append(&record))
        {
            app.entries
                .push(app::Entry::Error(format!("session not saved: {err}")));
        }
    }

    fn ensure_writer(&mut self) -> Result<&mut SessionWriter> {
        if self.writer.is_none() {
            let name = self
                .pending_name
                .take()
                .unwrap_or_else(session::default_name);
            self.writer = Some(SessionWriter::new(
                self.dir.join(format!("{name}.jsonl")),
                session::Header {
                    version: session::FORMAT_VERSION,
                    created: 0, // stamped when the file is created
                    provider: Some(self.launch.provider.clone()),
                    model: self.launch.model.clone(),
                },
            ));
        }
        Ok(self.writer.as_mut().expect("just set"))
    }

    /// `/name [name]`: rename the open file, or remember the name for the
    /// file that will be created with the next prompt.
    fn set_name(&mut self, app: &mut app::App, name: Option<String>) {
        let Some(name) = name else {
            app.entries
                .push(app::Entry::Info("usage: /name <new-name>".into()));
            return;
        };
        match self.writer.as_mut() {
            Some(writer) => match writer.rename(&name) {
                Ok(_) => app
                    .entries
                    .push(app::Entry::Info(format!("session saved as `{name}`"))),
                Err(err) => app.entries.push(app::Entry::Error(err.to_string())),
            },
            None => match session::validate_name(&name) {
                Ok(()) => {
                    self.pending_name = Some(name.clone());
                    app.entries.push(app::Entry::Info(format!(
                        "session will be saved as `{name}`"
                    )));
                }
                Err(err) => app.entries.push(app::Entry::Error(err.to_string())),
            },
        }
    }
}

/// Load a saved session into the app: transcript, usage and history;
/// subsequent records continue the same file.
fn load_session(info: &SessionInfo, app: &mut app::App, state: &mut SessionState) -> Result<()> {
    let data = session::load(&info.path)?;
    let conversation = app.restore(&data.records);
    app.history = crate::context::system_messages();
    app.history.extend(conversation);
    state.writer = Some(SessionWriter::reopen(info.path.clone()));
    state.pending_name = None;
    app.entries.push(app::Entry::Info(format!(
        "resumed `{}` ({} records)",
        info.name,
        data.records.len()
    )));
    Ok(())
}

/// Find a session by display name for `--resume NAME`.
fn session_by_name(name: &str) -> Result<SessionInfo> {
    session::validate_name(name)?;
    let path = session::sessions_dir().join(format!("{name}.jsonl"));
    if !path.exists() {
        return Err(crate::error::Error::SessionNotFound(name.into()));
    }
    Ok(SessionInfo {
        path,
        name: name.to_string(),
        modified: 0,
        turns: 0,
    })
}

fn event_loop(launch: &Launch, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    let mut app = app::App::new();
    let mut state = SessionState::new(launch.clone());
    // Open picker with the sessions it lists, when active.
    let mut picker: Option<(picker::Picker, Vec<SessionInfo>)> = None;

    // `--resume NAME` loads the session before the first frame; plain
    // `--resume` opens the picker.
    match launch.resume.as_deref() {
        Some("") => {
            let infos = session::list(&state.dir);
            picker = Some((resume_picker(&infos), infos));
        }
        Some(name) => {
            let info = session_by_name(name)?;
            load_session(&info, &mut app, &mut state)?;
        }
        None => {}
    }

    // A command-line prompt starts the first turn immediately.
    if let Some(prompt) = launch.initial_prompt.clone() {
        app.push_user(&prompt);
        state.record(
            &mut app,
            session::Record::User {
                text: prompt.clone(),
            },
        );
        app.busy = true;
        spawn_turn(app.history.clone(), launch, prompt, tx.clone());
    }

    loop {
        let size = terminal.size()?;
        let (width, height) = (size.width, size.height);
        let chat_height = height.saturating_sub(2) as usize;
        let transcript = app.transcript_lines(width);
        let max_scroll = transcript.len().saturating_sub(chat_height);
        let scroll_from_top = max_scroll.saturating_sub(app.scroll_from_bottom);

        let session_name = state.current_name();
        let status = ui::Status {
            provider: &launch.provider,
            model: launch.model.as_deref(),
            session: session_name.as_deref(),
        };
        let picker_ref = picker.as_ref().map(|(p, _)| p);
        terminal.draw(|frame| {
            ui::draw(
                frame,
                &app,
                transcript,
                scroll_from_top,
                &status,
                picker_ref,
            );
        })?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(key) = pressed(key) {
                        if picker.is_some() {
                            match picker_key(&mut picker, key, &mut app, &mut state) {
                                PickerOutcome::Quit => break,
                                PickerOutcome::Cancel => picker = None,
                                PickerOutcome::Selected => {
                                    // Handled inside picker_key; nothing here.
                                }
                                PickerOutcome::None => {}
                            }
                        } else {
                            match handle_key(&mut app, &mut state, key) {
                                Action::Quit => break,
                                Action::Clear => app.clear(),
                                Action::OpenPicker => {
                                    let infos = session::list(&state.dir);
                                    picker = Some((resume_picker(&infos), infos));
                                }
                                Action::Submit(prompt) => {
                                    app.push_user(&prompt);
                                    state.record(
                                        &mut app,
                                        session::Record::User {
                                            text: prompt.clone(),
                                        },
                                    );
                                    if app.busy {
                                        app.pending = Some(prompt);
                                    } else {
                                        app.busy = true;
                                        spawn_turn(app.history.clone(), launch, prompt, tx.clone());
                                    }
                                }
                                Action::None => {}
                            }
                        }
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll_up(3),
                    MouseEventKind::ScrollDown => app.scroll_down(3),
                    _ => {}
                },
                _ => {}
            }
        }

        // Drain everything the worker produced.
        let mut turn_finished = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                WorkerEvent::Turn(turn_event) => {
                    app.on_turn_event(turn_event.clone());
                    record_turn_event(&mut state, &mut app, &turn_event);
                }
                WorkerEvent::Done(result) => {
                    record_done(&mut state, &mut app, &result);
                    app.on_turn_done(&result);
                    turn_finished = true;
                }
            }
        }
        if let (true, Some(prompt)) = (turn_finished, app.pending.take()) {
            app.push_user(&prompt);
            state.record(
                &mut app,
                session::Record::User {
                    text: prompt.clone(),
                },
            );
            app.busy = true;
            spawn_turn(app.history.clone(), launch, prompt, tx.clone());
        }
    }
    Ok(())
}

/// Map one turn event onto session records (autosave).
fn record_turn_event(state: &mut SessionState, app: &mut app::App, event: &TurnEvent) {
    match event {
        TurnEvent::Round {
            text,
            thinking,
            calls,
        } => {
            if !thinking.is_empty() {
                state.record(
                    app,
                    session::Record::Thinking {
                        text: thinking.clone(),
                    },
                );
            }
            if !text.is_empty() || !calls.is_empty() {
                state.record(
                    app,
                    session::Record::Assistant {
                        text: text.clone(),
                        calls: calls.clone(),
                    },
                );
            }
        }
        TurnEvent::ToolResult { name, output } => {
            state.record(
                app,
                session::Record::ToolResult {
                    name: name.clone(),
                    output: output.clone(),
                },
            );
        }
        TurnEvent::Text { .. } | TurnEvent::Thinking { .. } | TurnEvent::ToolCall { .. } => {}
    }
}

/// Map a finished turn onto session records (usage or error) and update
/// the conversation history from the turn output.
fn record_done(
    state: &mut SessionState,
    app: &mut app::App,
    result: &crate::error::Result<TurnOutput>,
) {
    match result {
        Ok(output) => {
            if let Some(usage) = output.usage {
                state.record(app, session::usage_record(usage));
            }
            app.history = output.history.clone();
        }
        Err(err) => {
            state.record(
                app,
                session::Record::Error {
                    message: err.to_string(),
                },
            );
        }
    }
}

/// What a picker key press produced.
enum PickerOutcome {
    None,
    Quit,
    Cancel,
    Selected,
}

fn picker_key(
    picker: &mut Option<(picker::Picker, Vec<SessionInfo>)>,
    key: KeyEvent,
    app: &mut app::App,
    state: &mut SessionState,
) -> PickerOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some((picker_state, infos)) = picker.as_mut() else {
        return PickerOutcome::None;
    };
    match (ctrl, key.code) {
        (true, KeyCode::Char('c')) | (true, KeyCode::Char('q')) => PickerOutcome::Quit,
        (_, KeyCode::Up) => {
            picker_state.up();
            PickerOutcome::None
        }
        (_, KeyCode::Down) => {
            picker_state.down();
            PickerOutcome::None
        }
        (_, KeyCode::Esc) => PickerOutcome::Cancel,
        (_, KeyCode::Enter) => {
            if let Some(index) = picker_state.confirm() {
                let info = infos[index].clone();
                *picker = None;
                if let Err(err) = load_session(&info, app, state) {
                    app.entries.push(app::Entry::Error(err.to_string()));
                }
                PickerOutcome::Selected
            } else {
                // Nothing selectable (empty list): close the picker.
                *picker = None;
                PickerOutcome::Cancel
            }
        }
        (_, KeyCode::Backspace) => {
            picker_state.pop_char();
            PickerOutcome::None
        }
        (_, KeyCode::Char(c)) => {
            picker_state.push_char(c);
            PickerOutcome::None
        }
        _ => PickerOutcome::None,
    }
}

/// Build the `/resume` picker from a session listing.
fn resume_picker(infos: &[SessionInfo]) -> picker::Picker {
    let items = infos
        .iter()
        .map(|info| picker::PickerItem {
            label: info.name.clone(),
            detail: format!(
                "{} · {} turns",
                session::format_datetime(info.modified),
                info.turns
            ),
        })
        .collect();
    picker::Picker::new("resume", items)
}

/// Handle a slash command. Commands never produce model turns.
fn handle_command(app: &mut app::App, state: &mut SessionState, command: Command) -> Action {
    match command {
        Command::Name(name) => {
            state.set_name(app, name);
            Action::None
        }
        Command::Resume => {
            if app.busy {
                app.entries.push(app::Entry::Info(
                    "wait for the current turn to finish".into(),
                ));
                return Action::None;
            }
            let infos = session::list(&state.dir);
            if infos.is_empty() {
                app.entries
                    .push(app::Entry::Info("no saved sessions".into()));
                return Action::None;
            }
            Action::OpenPicker
        }
    }
}

/// Only act on press events (release events come from Windows terminals).
fn pressed(key: KeyEvent) -> Option<KeyEvent> {
    if key.kind == KeyEventKind::Press {
        Some(key)
    } else {
        None
    }
}

fn handle_key(app: &mut app::App, state: &mut SessionState, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (ctrl, key.code) {
        (true, KeyCode::Char('c')) | (true, KeyCode::Char('q')) => Action::Quit,
        (true, KeyCode::Char('d')) if app.input.is_empty() => Action::Quit,
        (true, KeyCode::Char('l')) => Action::Clear,

        (false, KeyCode::Enter) => match app.take_input() {
            Some(prompt) => {
                if let Some(parsed) = parse_command(&prompt) {
                    match parsed {
                        Ok(command) => handle_command(app, state, command),
                        Err(message) => {
                            app.entries.push(app::Entry::Error(message));
                            Action::None
                        }
                    }
                } else {
                    Action::Submit(prompt)
                }
            }
            None => Action::None,
        },
        (false, KeyCode::Esc) => Action::Quit,
        (false, KeyCode::Backspace) => {
            app.backspace();
            Action::None
        }
        (false, KeyCode::Delete) => {
            app.delete();
            Action::None
        }
        (false, KeyCode::Left) => {
            app.cursor_left();
            Action::None
        }
        (false, KeyCode::Right) => {
            app.cursor_right();
            Action::None
        }
        (false, KeyCode::Home) => {
            app.cursor_home();
            Action::None
        }
        (false, KeyCode::End) => {
            app.cursor_end();
            Action::None
        }
        (false, KeyCode::PageUp) => {
            app.scroll_up(SCROLL_PAGE as usize);
            Action::None
        }
        (false, KeyCode::PageDown) => {
            app.scroll_down(SCROLL_PAGE as usize);
            Action::None
        }
        (false, KeyCode::Up) => {
            app.scroll_up(1);
            Action::None
        }
        (false, KeyCode::Down) => {
            app.scroll_down(1);
            Action::None
        }
        (false, KeyCode::Char(c)) => {
            app.insert_char(c);
            Action::None
        }
        _ => Action::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(
            parse_command("hello"),
            None,
            "plain prompt is not a command"
        );
        assert_eq!(parse_command("/resume"), Some(Ok(Command::Resume)));
        assert_eq!(
            parse_command("/name my-session"),
            Some(Ok(Command::Name(Some("my-session".into()))))
        );
        assert_eq!(parse_command("/name"), Some(Ok(Command::Name(None))));
        assert_eq!(parse_command("/name   "), Some(Ok(Command::Name(None))));
        assert!(matches!(
            parse_command("/foo"),
            Some(Err(message)) if message.contains("unknown command `/foo`")
        ));
        assert!(matches!(
            parse_command("/resume later"),
            Some(Err(message)) if message.contains("unknown command")
        ));
    }

    #[test]
    fn resume_picker_labels_sessions() {
        let infos = vec![
            SessionInfo {
                path: PathBuf::from(".clanky/sessions/a.jsonl"),
                name: "a".into(),
                modified: 1_700_000_000,
                turns: 3,
            },
            SessionInfo {
                path: PathBuf::from(".clanky/sessions/b.jsonl"),
                name: "b".into(),
                modified: 0,
                turns: 0,
            },
        ];
        let picker = resume_picker(&infos);
        let lines = picker.lines(10);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("a") && t.contains("2023-11-14 22:13"))
        );
        assert!(texts.iter().any(|t| t.contains("3 turns")));
    }
}
