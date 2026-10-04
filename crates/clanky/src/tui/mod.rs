//! Interactive TUI (M3): chat view, input line, streaming render.
//!
//! The agentic turn runs on a worker thread so the UI stays responsive
//! while the model streams and tools execute; [`TurnEvent`]s cross to the
//! render loop over a channel. Ctrl+C quits; `clanky` falls back to
//! command-line mode when stdin or stdout is redirected (see `main.rs`).
//!
//! Layout (see `ui.rs`):
//! ┌────────────────────────────────┐
//! │ transcript (markdown, scroll)  │
//! │ provider · model      ● state  │
//! │ ❯ input line                   │
//! └────────────────────────────────┘

mod app;
mod markdown;
mod ui;

use std::io::{IsTerminal as _, Stdout};
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

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
fn spawn_turn(launch: &Launch, prompt: String, tx: Sender<WorkerEvent>) {
    let launch = launch.clone();
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
                    crate::context::system_messages(),
                    &prompt,
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
}

fn event_loop(launch: &Launch, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    let mut app = app::App::new();

    // A command-line prompt starts the first turn immediately.
    if let Some(prompt) = launch.initial_prompt.clone() {
        app.push_user(&prompt);
        app.busy = true;
        spawn_turn(launch, prompt, tx.clone());
    }

    loop {
        let size = terminal.size()?;
        let (width, height) = (size.width, size.height);
        let chat_height = height.saturating_sub(2) as usize;
        let transcript = app.transcript_lines(width);
        let max_scroll = transcript.len().saturating_sub(chat_height);
        let scroll_from_top = max_scroll.saturating_sub(app.scroll_from_bottom);

        let status = ui::Status {
            provider: &launch.provider,
            model: launch.model.as_deref(),
        };
        terminal.draw(|frame| {
            ui::draw(frame, &app, transcript, scroll_from_top, &status);
        })?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(key) = pressed(key) {
                        match handle_key(&mut app, key) {
                            Action::Quit => break,
                            Action::Clear => app.clear(),
                            Action::Submit(prompt) => {
                                app.push_user(&prompt);
                                if app.busy {
                                    app.pending = Some(prompt);
                                } else {
                                    app.busy = true;
                                    spawn_turn(launch, prompt, tx.clone());
                                }
                            }
                            Action::None => {}
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
                WorkerEvent::Turn(event) => app.on_turn_event(event),
                WorkerEvent::Done(result) => {
                    app.on_turn_done(&result);
                    turn_finished = true;
                }
            }
        }
        if let (true, Some(prompt)) = (turn_finished, app.pending.take()) {
            app.push_user(&prompt);
            app.busy = true;
            spawn_turn(launch, prompt, tx.clone());
        }
    }
    Ok(())
}

/// Only act on press events (release events come from Windows terminals).
fn pressed(key: KeyEvent) -> Option<KeyEvent> {
    if key.kind == KeyEventKind::Press {
        Some(key)
    } else {
        None
    }
}

fn handle_key(app: &mut app::App, key: KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (ctrl, key.code) {
        (true, KeyCode::Char('c')) | (true, KeyCode::Char('q')) => Action::Quit,
        (true, KeyCode::Char('d')) if app.input.is_empty() => Action::Quit,
        (true, KeyCode::Char('l')) => Action::Clear,

        (false, KeyCode::Enter) => match app.take_input() {
            Some(prompt) => Action::Submit(prompt),
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
