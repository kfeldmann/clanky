//! Interactive TUI (M3): linear terminal output, input line, streaming render.
//!
//! The transcript is printed straight to the terminal (see `screen.rs`):
//! finalized lines flow into the normal buffer and scrollback, so the
//! native scrollbar, native text selection, and the history left behind
//! after quitting all work without being emulated. Only a small region at
//! the bottom of the screen — the streaming entry, an optional popup, the
//! status line, and the input line — is redrawn in place.
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
//! Slash commands (M5): `/model`, `/provider`, `/thinking`, `/sampling`
//! reconfigure the session for subsequent turns; `/system` prints the
//! assembled system prompt (context files and skills, exactly as sent);
//! `/<template>` inserts a prompt template from `.clanky/prompts/` into
//! the input. Configuration commands and `/<model id>` open pickers;
//! unknown commands error cleanly and are never sent to the model.
//!
//! Input ergonomics (M7): Tab completes the file path at the caret (longest
//! common prefix first, further Tabs cycle candidates, shown in a popup
//! above the input line); ctrl+e opens `$EDITOR` with the prompt buffer —
//! raw mode is dropped, the editor owns the terminal, and the TUI resumes
//! with the edited text. `↑`/`↓` recall previously submitted prompts
//! (the first `↑` saves the current input as a draft).
//!
//! Scrolling and copying are the terminal's own: the mouse is never
//! captured and no alternate screen is used, so the wheel, the scrollbar,
//! and native selection (including shift+drag) work as in any shell.
//!
//! Screen layout (`screen.rs`):
//! │ … printed history (scrollback, final) … │
//! │ ◆ streaming entry (rewritten as it grows) │
//! │ provider · model · think · session · cost      ↑↓ tok │
//! │ ●❯ input line │

mod app;
mod commands;
mod completion;
mod editor;
mod markdown;
mod picker;
mod screen;

use std::io::{IsTerminal as _, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use clanky_protocol::{ChatMessage, ModelInfo};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};

use crate::error::Result;
use crate::prompts::{self, Template};
use crate::session::{self, SessionInfo, SessionWriter};
use crate::settings::{SamplingParams, Settings};
use crate::turn::{TurnConfig, TurnEvent, TurnOutput};
use commands::{Command, SamplingEdit};

/// Presets offered by the bare `/thinking` picker.
const THINKING_OPTIONS: &[(&str, &str)] = &[
    ("off", "disable thinking"),
    ("1024", "small budget"),
    ("4096", "typical budget"),
    ("16384", "large budget"),
    ("32768", "very large budget"),
];

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
    /// Tool-loop round cap (`None` = default, `Some(0)` = unlimited).
    pub max_tool_rounds: Option<usize>,
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
            max_tool_rounds: settings.max_tool_rounds,
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

/// What the worker threads report back to the UI loop.
/// Terminal status of one turn: the outcome plus the conversation as the
/// turn left it. The history comes back even on error, so an aborted turn
/// (e.g. the tool-loop limit) keeps its partial context and the session
/// stays continuable.
struct TurnEnd {
    result: crate::error::Result<TurnOutput>,
    history: Vec<ChatMessage>,
}

enum WorkerEvent {
    Turn(TurnEvent),
    /// Terminal status of the turn.
    Done(TurnEnd),
    /// Model list for the [`Catalog`].
    Models(crate::error::Result<Vec<ModelInfo>>),
}

/// The provider's model catalog, fetched on a worker thread so the status
/// line can price the session and `/model` can open instantly.
struct Catalog {
    /// Provider the catalog was fetched for (stale after a switch).
    provider: String,
    models: Vec<ModelInfo>,
    /// A listing was requested for the `/model` picker: open it when the
    /// fetch completes.
    for_picker: bool,
}

impl Catalog {
    /// An empty catalog for `provider` (nothing fetched yet).
    fn new(provider: String) -> Self {
        Self {
            provider,
            models: Vec::new(),
            for_picker: false,
        }
    }

    /// Fetch the catalog for `provider` on a worker thread; `for_picker`
    /// opens the `/model` picker when the fetch completes.
    fn request(&mut self, provider: String, tx: &Sender<WorkerEvent>, for_picker: bool) {
        self.provider = provider;
        self.for_picker = for_picker;
        spawn_models(self.provider.clone(), tx.clone());
    }

    /// Context window of `model`, when the catalog matches the active
    /// provider and the model declares one.
    fn context_window_for(&self, provider: &str, model: Option<&str>) -> Option<u64> {
        if self.provider != provider {
            return None;
        }
        self.models
            .iter()
            .find(|m| Some(m.id.as_str()) == model)?
            .context_window
    }

    /// Pricing of `model` (dollars per million tokens: input, output)
    /// when the catalog matches the active provider and carries prices.
    fn pricing_for(&self, provider: &str, model: Option<&str>) -> Option<(f64, f64)> {
        if self.provider != provider {
            return None;
        }
        let info = self.models.iter().find(|m| Some(m.id.as_str()) == model)?;
        Some((info.input_price_per_mtok?, info.output_price_per_mtok?))
    }
}

/// Run the interactive session until the user quits.
pub fn run(mut launch: Launch) -> Result<()> {
    enable_raw_mode()?;
    let (width, height) = size()?;
    let mut screen = screen::Screen::new(std::io::stdout(), width, height);
    let result = event_loop(&mut launch, &mut screen);
    match restore(&mut screen) {
        Ok(()) => result,
        Err(restore_err) => {
            // Surface the original error if cleanup also fails.
            result.and(Err(restore_err))
        }
    }
}

/// Leave the terminal as the user's shell expects it: raw mode off and a
/// fresh line below the transcript, which stays in the terminal.
fn restore<W: std::io::Write>(screen: &mut screen::Screen<W>) -> Result<()> {
    // The newline goes out while raw mode is still on (ONLCR would turn
    // it into a stray extra carriage return otherwise).
    screen.finish()?;
    disable_raw_mode()?;
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
            let outcome = (|| {
                let handler = crate::provider::create(&launch.provider)?;
                let config = TurnConfig {
                    model: launch.model.clone(),
                    sampling: launch.sampling.clone(),
                    thinking: launch.thinking.clone(),
                    max_tool_rounds: launch.max_tool_rounds,
                };
                crate::turn::run_turn(
                    handler,
                    &crate::tools::default_tools(),
                    &mut messages,
                    &config,
                    &mut |event: TurnEvent| {
                        let _ = tx.send(WorkerEvent::Turn(event));
                    },
                )
            })();
            // Hand the conversation back in every case, not just on
            // success: an aborted turn still leaves valid context behind.
            let _ = tx.send(WorkerEvent::Done(TurnEnd {
                result: outcome,
                history: messages,
            }));
        })
        .expect("failed to spawn turn worker thread");
}

/// Fetch the provider's model list on a worker thread (network I/O must
/// not block the render loop); the result feeds [`Catalog`].
fn spawn_models(provider_name: String, tx: Sender<WorkerEvent>) {
    std::thread::Builder::new()
        .name("clanky-models".into())
        .spawn(move || {
            let outcome: crate::error::Result<Vec<ModelInfo>> = (|| {
                let handler = crate::provider::create(&provider_name)?;
                let mut client = clanky_protocol::ProviderClient::new(
                    clanky_protocol::LoopbackTransport::new(handler),
                );
                client.handshake()?;
                Ok(client.list_models()?)
            })();
            let _ = tx.send(WorkerEvent::Models(outcome));
        })
        .expect("failed to spawn model-listing thread");
}

/// What a key press asks the main loop to do.
enum Action {
    None,
    Quit,
    Clear,
    Submit(String),
    /// Open a picker; the kind carries the data it selects from.
    OpenPicker(PickerKind),
    /// Suspend the TUI and edit the prompt buffer in `$EDITOR` (M7).
    OpenEditor,
}

/// What an open picker selects (M5): the payload decides what Enter does.
enum PickerKind {
    /// Pick a saved session and load it.
    Resume(Vec<SessionInfo>),
    /// Pick a model for the current provider.
    Models(Vec<ModelInfo>),
    /// Pick a provider from the known list.
    Providers,
    /// Pick a thinking preset.
    Thinking,
    /// The command palette (opened by bare `/`): built-in commands and
    /// prompt templates.
    Palette(Vec<PaletteItem>),
}

/// One entry of the command palette.
enum PaletteItem {
    /// A built-in command; `text` is inserted into the input (trailing
    /// space included) so the user can add arguments.
    Builtin { text: String, hint: String },
    /// A prompt template; selecting inserts its body into the input.
    Template(Template),
}

/// Built-in commands offered by the palette: (input text, hint).
const PALETTE_BUILTINS: &[(&str, &str)] = &[
    ("/model", "pick a model"),
    ("/provider", "pick a provider"),
    ("/thinking", "thinking presets"),
    ("/sampling ", "edit sampling parameters"),
    ("/system", "show the assembled system prompt"),
    ("/name ", "rename the session"),
    ("/resume", "load a saved session"),
];

/// The palette picker kind: built-ins first, then templates by name.
fn palette_kind(templates: Vec<Template>) -> PickerKind {
    let mut items: Vec<PaletteItem> = PALETTE_BUILTINS
        .iter()
        .map(|(text, hint)| PaletteItem::Builtin {
            text: (*text).to_string(),
            hint: (*hint).to_string(),
        })
        .collect();
    items.extend(templates.into_iter().map(PaletteItem::Template));
    PickerKind::Palette(items)
}

/// Build the picker UI for a kind (the kind carries its own data). The
/// item order is the order [`apply_picker_selection`] indexes into.
fn picker_for(kind: &PickerKind) -> picker::Picker {
    let (title, items) = match kind {
        PickerKind::Resume(infos) => (
            "resume",
            infos
                .iter()
                .map(|info| picker::PickerItem {
                    label: info.name.clone(),
                    detail: format!(
                        "{} · {} turns",
                        session::format_datetime(info.modified),
                        info.turns
                    ),
                })
                .collect(),
        ),
        PickerKind::Models(models) => (
            "model",
            models
                .iter()
                .map(|model| picker::PickerItem {
                    label: model.id.clone(),
                    detail: model_detail(model),
                })
                .collect(),
        ),
        PickerKind::Providers => (
            "provider",
            crate::provider::available()
                .iter()
                .map(|name| picker::PickerItem {
                    label: (*name).to_string(),
                    detail: crate::provider::default_model(name)
                        .unwrap_or("")
                        .to_string(),
                })
                .collect(),
        ),
        PickerKind::Thinking => (
            "thinking",
            THINKING_OPTIONS
                .iter()
                .map(|(value, detail)| picker::PickerItem {
                    label: (*value).to_string(),
                    detail: (*detail).to_string(),
                })
                .collect(),
        ),
        PickerKind::Palette(items) => (
            "commands",
            items
                .iter()
                .map(|item| match item {
                    PaletteItem::Builtin { text, hint } => picker::PickerItem {
                        label: text.clone(),
                        detail: hint.clone(),
                    },
                    PaletteItem::Template(template) => picker::PickerItem {
                        label: format!("/{}", template.name),
                        detail: first_line(&template.content),
                    },
                })
                .collect(),
        ),
    };
    picker::Picker::new(title, items)
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

fn event_loop(launch: &mut Launch, screen: &mut screen::Screen<Stdout>) -> Result<()> {
    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    let mut app = app::App::new();
    // Fresh session: seed the system context so the first turn sends it
    // (resumed sessions rebuild it inside `load_session`).
    app.history = crate::context::system_messages();
    let mut state = SessionState::new(launch.clone());
    // Prefetch the provider's model catalog in the background: it prices
    // the session-cost display and makes `/model` instant.
    let mut catalog = Catalog::new(launch.provider.clone());
    catalog.request(launch.provider.clone(), &tx, false);
    // Open picker with the payload it selects from, when active.
    let mut picker: Option<(picker::Picker, PickerKind)> = None;
    // Something changed since the last render (transcript, input, popup).
    let mut dirty = true;

    // `--resume NAME` loads the session before the first render; plain
    // `--resume` opens the picker. A resumed transcript is committed to
    // the terminal on the first render.
    match launch.resume.as_deref() {
        Some("") => {
            let kind = PickerKind::Resume(session::list(&state.dir));
            picker = Some((picker_for(&kind), kind));
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
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(key) = pressed(key) {
                        let tab = key.code == KeyCode::Tab
                            && !key.modifiers.contains(KeyModifiers::CONTROL);
                        if picker.is_some() {
                            match picker_key(
                                &mut picker,
                                key,
                                &mut app,
                                &mut state,
                                launch,
                                &mut catalog,
                                &tx,
                            ) {
                                PickerOutcome::Quit => break,
                                PickerOutcome::Cancel => picker = None,
                                PickerOutcome::Selected => {
                                    // Handled inside picker_key; nothing here.
                                }
                                PickerOutcome::None => {}
                            }
                        } else {
                            match handle_key(&mut app, &mut state, launch, &mut catalog, &tx, key) {
                                Action::Quit => break,
                                Action::Clear => {
                                    app.clear();
                                    screen.clear_screen();
                                }
                                Action::OpenPicker(kind) => {
                                    app.completion = None;
                                    picker = Some((picker_for(&kind), kind));
                                }
                                Action::OpenEditor => {
                                    if let Err(err) = run_editor(&mut app, screen) {
                                        app.entries.push(app::Entry::Error(err.to_string()));
                                    }
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
                        // The completion popup lives on between Tab presses
                        // only; any other key dismisses it.
                        if !tab {
                            app.completion = None;
                        }
                        dirty = true;
                    }
                }
                // The buffer was reflowed; re-anchor the redrawable region.
                Event::Resize(width, height) => {
                    screen.resync(&mut app, width, height);
                    dirty = true;
                }
                // The mouse is never captured: wheel scrolling and native
                // text selection are the terminal's own.
                _ => {}
            }
        }

        // Drain everything the worker produced.
        let mut turn_finished = false;
        while let Ok(event) = rx.try_recv() {
            dirty = true;
            match event {
                WorkerEvent::Turn(turn_event) => {
                    app.on_turn_event(turn_event.clone());
                    record_turn_event(&mut state, &mut app, &turn_event);
                }
                WorkerEvent::Done(done) => {
                    record_done(&mut state, &mut app, &done);
                    app.on_turn_done(&done.result);
                    turn_finished = true;
                }
                WorkerEvent::Models(result) => match result {
                    Ok(fetched) => {
                        let empty = fetched.is_empty();
                        catalog.models = fetched;
                        if catalog.for_picker {
                            catalog.for_picker = false;
                            if empty {
                                app.entries.push(app::Entry::Info(format!(
                                    "`{}` lists no models",
                                    launch.provider
                                )));
                            } else if picker.is_none() {
                                let kind =
                                    PickerKind::Models(text_generation_models(&catalog.models));
                                picker = Some((picker_for(&kind), kind));
                            }
                        }
                    }
                    Err(err) => app.entries.push(app::Entry::Error(err.to_string())),
                },
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

        if dirty {
            let session_name = state.current_name();
            // Estimated session cost: catalog pricing (dollars per million
            // tokens) × cumulative token totals. Prompt tokens are re-billed
            // every turn, so summing turns is what the provider charges for.
            let cost = catalog
                .pricing_for(&launch.provider, launch.model.as_deref())
                .map(|(input, output)| {
                    (app.total_prompt_tokens as f64 * input
                        + app.total_completion_tokens as f64 * output)
                        / 1_000_000.0
                });
            // `off`/empty mean disabled and are not shown.
            let thinking = launch
                .thinking
                .as_deref()
                .filter(|value| !value.is_empty() && *value != "off");
            let status = screen::Status {
                provider: &launch.provider,
                model: launch.model.as_deref(),
                thinking,
                session: session_name.as_deref(),
                cost,
                context_window: catalog
                    .context_window_for(&launch.provider, launch.model.as_deref()),
            };
            let picker_ref = picker.as_ref().map(|(p, _)| p);
            let completion = app.completion.clone();
            screen.render(&mut app, &status, picker_ref, completion.as_ref())?;
            dirty = false;
        }
    }
    Ok(())
}

/// ctrl+e (M7): suspend the TUI, edit the prompt buffer in `$EDITOR`,
/// resume with the edited text. An empty buffer clears the input; a
/// failing editor leaves the input untouched and reports the error.
fn run_editor(app: &mut app::App, screen: &mut screen::Screen<Stdout>) -> Result<()> {
    let editor = editor::editor_command()?;
    let initial = app.input.clone();
    // Hand the terminal to the editor: raw mode off, printed history
    // stays in place.
    disable_raw_mode()?;
    let outcome = editor::edit_with(&initial, &editor);
    // Re-establish the TUI no matter how the editor behaved.
    enable_raw_mode()?;
    screen.resync(app, screen.size().0, screen.size().1);
    app.completion = None;
    match outcome {
        Ok(Some(text)) => {
            app.input = text;
            app.cursor = app.input.len();
        }
        Ok(None) => {
            app.input.clear();
            app.cursor = 0;
        }
        Err(err) => return Err(err),
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
/// the conversation history. The history is adopted even when the turn
/// failed, so the model sees the partial context (its prompt, rounds and
/// tool results) on the next turn.
fn record_done(state: &mut SessionState, app: &mut app::App, done: &TurnEnd) {
    match &done.result {
        Ok(output) => {
            if let Some(usage) = output.usage {
                state.record(app, session::usage_record(usage));
            }
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
    app.history = done.history.clone();
}

/// What a picker key press produced.
enum PickerOutcome {
    None,
    Quit,
    Cancel,
    Selected,
}

fn picker_key(
    picker: &mut Option<(picker::Picker, PickerKind)>,
    key: KeyEvent,
    app: &mut app::App,
    state: &mut SessionState,
    launch: &mut Launch,
    catalog: &mut Catalog,
    tx: &Sender<WorkerEvent>,
) -> PickerOutcome {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let Some((picker_state, _kind)) = picker.as_mut() else {
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
            apply_picker_selection(picker, app, state, launch, catalog, tx);
            PickerOutcome::Selected
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

/// Act on a confirmed picker selection; the picker closes first (the
/// action may surface errors or open follow-up pickers via commands).
/// An empty selection (no filtered items) simply closes the picker.
fn apply_picker_selection(
    picker: &mut Option<(picker::Picker, PickerKind)>,
    app: &mut app::App,
    state: &mut SessionState,
    launch: &mut Launch,
    catalog: &mut Catalog,
    tx: &Sender<WorkerEvent>,
) {
    let Some(index) = picker
        .as_mut()
        .and_then(|(picker_state, _)| picker_state.confirm())
    else {
        // Nothing selectable (empty list): close the picker.
        *picker = None;
        return;
    };
    let Some((_, kind)) = picker.take() else {
        unreachable!("picker was just read");
    };
    match kind {
        PickerKind::Resume(infos) => {
            if let Some(info) = infos.get(index)
                && let Err(err) = load_session(info, app, state)
            {
                app.entries.push(app::Entry::Error(err.to_string()));
            }
        }
        PickerKind::Models(models) => {
            if let Some(model) = models.get(index) {
                let id = model.id.clone();
                launch.model = Some(id.clone());
                app.entries
                    .push(app::Entry::Info(format!("model set to `{id}`")));
            }
        }
        PickerKind::Providers => {
            // Item order matches provider::available().
            if let Some(name) = crate::provider::available().get(index) {
                match set_provider(launch, name) {
                    Ok(message) => app.entries.push(app::Entry::Info(message)),
                    Err(message) => app.entries.push(app::Entry::Error(message)),
                }
            }
        }
        PickerKind::Thinking => {
            if let Some((value, _)) = THINKING_OPTIONS.get(index) {
                launch.thinking = crate::turn::parse_thinking(value)
                    .ok()
                    .flatten()
                    .map(|_| value.to_string());
                app.entries.push(app::Entry::Info(match launch.thinking {
                    Some(_) => format!("thinking set to `{value}`"),
                    None => "thinking disabled".into(),
                }));
            }
        }
        PickerKind::Palette(items) => match items.get(index) {
            Some(PaletteItem::Template(template)) => {
                *picker = None;
                apply_template(app, template, None);
            }
            Some(PaletteItem::Builtin { text, .. }) => {
                *picker = None;
                match text.as_str() {
                    "/model" => {
                        handle_command(app, state, launch, catalog, Command::Model(None), tx);
                    }
                    "/provider" => {
                        *picker = Some((picker_for(&PickerKind::Providers), PickerKind::Providers));
                    }
                    "/thinking" => {
                        *picker = Some((picker_for(&PickerKind::Thinking), PickerKind::Thinking));
                    }
                    "/resume" => {
                        // Re-run the command; apply the picker it asks for.
                        if let Action::OpenPicker(kind) =
                            handle_command(app, state, launch, catalog, Command::Resume, tx)
                        {
                            *picker = Some((picker_for(&kind), kind));
                        }
                    }
                    // Commands that need arguments (`/name `, `/sampling `)
                    // land in the input line for the user to complete.
                    arg_command => {
                        app.input = (*arg_command).to_string();
                        app.cursor = app.input.len();
                    }
                }
            }
            None => {}
        },
    }
}

/// Execute a slash command. Commands never produce model turns; they only
/// reconfigure the session, touch session files, or open pickers.
fn handle_command(
    app: &mut app::App,
    state: &mut SessionState,
    launch: &mut Launch,
    catalog: &mut Catalog,
    command: Command,
    tx: &Sender<WorkerEvent>,
) -> Action {
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
            Action::OpenPicker(PickerKind::Resume(infos))
        }
        Command::Model(Some(id)) => {
            launch.model = Some(id.clone());
            app.entries
                .push(app::Entry::Info(format!("model set to `{id}`")));
            Action::None
        }
        Command::Model(None) => {
            // An already-fetched catalog opens the picker instantly.
            if !catalog.models.is_empty() && catalog.provider == launch.provider {
                Action::OpenPicker(PickerKind::Models(text_generation_models(&catalog.models)))
            } else {
                app.entries.push(app::Entry::Info(format!(
                    "loading models from {}…",
                    launch.provider
                )));
                catalog.request(launch.provider.clone(), tx, true);
                Action::None
            }
        }
        Command::Provider(Some(name)) => {
            match set_provider(launch, &name) {
                Ok(message) => {
                    // Refresh the catalog so pricing follows the provider.
                    catalog.request(name.clone(), tx, false);
                    app.entries.push(app::Entry::Info(message));
                }
                Err(message) => app.entries.push(app::Entry::Error(message)),
            }
            Action::None
        }
        Command::Provider(None) => Action::OpenPicker(PickerKind::Providers),
        Command::Thinking(Some(raw)) => {
            match crate::turn::parse_thinking(&raw) {
                Ok(parsed) => {
                    launch.thinking = parsed.map(|_| raw.clone());
                    app.entries.push(app::Entry::Info(match parsed {
                        Some(_) => format!("thinking set to `{raw}`"),
                        None => "thinking disabled".into(),
                    }));
                }
                Err(err) => app.entries.push(app::Entry::Error(err.to_string())),
            }
            Action::None
        }
        Command::Thinking(None) => Action::OpenPicker(PickerKind::Thinking),
        Command::Sampling(edit) => {
            apply_sampling(launch, &edit, app);
            Action::None
        }
        Command::System => {
            push_system_report(app, &crate::context::system_parts());
            Action::None
        }
        Command::Palette => Action::OpenPicker(palette_kind(prompts::templates())),
        Command::Template { template, extra } => {
            apply_template(app, &template, extra.as_deref());
            Action::None
        }
    }
}

/// Push the `/system` report to the transcript: a summary `Info` line,
/// then one labelled `SystemPart` entry per part (or a single `Info`
/// explaining there is no context). Pure apart from `app`, so it can be
/// tested with synthetic parts.
fn push_system_report(app: &mut app::App, parts: &[(String, String)]) {
    let total: usize = parts
        .iter()
        .map(|(_, content)| content.chars().count())
        .sum();
    if parts.is_empty() {
        app.entries.push(app::Entry::Info(
            "no system context: no AGENTS.md, SYSTEM.md, or skills found".into(),
        ));
    } else {
        app.entries.push(app::Entry::Info(format!(
            "system prompt: {} part(s), {} chars — sent before the first user message",
            parts.len(),
            total
        )));
        for (label, content) in parts {
            app.entries.push(app::Entry::SystemPart {
                label: label.clone(),
                content: content.clone(),
            });
        }
    }
}

/// Switch provider; the model resets to the new provider's default (model
/// ids do not transfer between providers).
fn set_provider(launch: &mut Launch, name: &str) -> std::result::Result<String, String> {
    let Some(default_model) = crate::provider::default_model(name) else {
        return Err(format!(
            "unknown provider `{name}`; available: {}",
            crate::provider::available().join(", ")
        ));
    };
    launch.provider = name.to_string();
    launch.model = Some(default_model.to_string());
    Ok(format!(
        "provider set to `{name}` (model `{default_model}`)"
    ))
}

/// Apply a `/sampling` edit to the launch config, reporting as an entry.
/// Parameter values are validated the same way turns validate them, so
/// typos surface immediately instead of at the next turn.
fn apply_sampling(launch: &mut Launch, edit: &SamplingEdit, app: &mut app::App) {
    match edit {
        SamplingEdit::Show => {
            let description = launch
                .sampling
                .as_ref()
                .map(|p| format!("sampling: {}", describe_sampling(p)))
                .unwrap_or_else(|| "no sampling parameters set".into());
            app.entries.push(app::Entry::Info(description));
        }
        SamplingEdit::Clear => {
            launch.sampling = None;
            app.entries
                .push(app::Entry::Info("sampling parameters cleared".into()));
        }
        SamplingEdit::Set(pairs) => {
            let merged: SamplingParams = pairs.iter().cloned().collect();
            if let Err(err) = crate::turn::sampling_from(&Some(merged)) {
                app.entries.push(app::Entry::Error(err.to_string()));
                return;
            }
            let current = launch.sampling.get_or_insert_with(SamplingParams::new);
            for (key, value) in pairs {
                current.insert(key.clone(), value.clone());
            }
            app.entries.push(app::Entry::Info(format!(
                "sampling: {}",
                describe_sampling(current)
            )));
        }
        SamplingEdit::Unset(keys) => {
            if let Some(current) = launch.sampling.as_mut() {
                for key in keys {
                    current.remove(key.as_str());
                }
            }
            let description = launch
                .sampling
                .as_ref()
                .filter(|p| !p.is_empty())
                .map(|p| format!("sampling: {}", describe_sampling(p)))
                .unwrap_or_else(|| "no sampling parameters set".into());
            app.entries.push(app::Entry::Info(description));
        }
    }
}

/// `key=value` pairs joined for display.
fn describe_sampling(params: &SamplingParams) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Put a prompt template into the input line (plus any extra text typed
/// after the command) for editing; nothing is sent until Enter.
fn apply_template(app: &mut app::App, template: &Template, extra: Option<&str>) {
    let mut text = template.content.clone();
    if let Some(extra) = extra.map(str::trim).filter(|e| !e.is_empty()) {
        text.push_str("\n\n");
        text.push_str(extra);
    }
    app.input = text;
    app.cursor = app.input.len();
    app.completion = None;
    app.entries.push(app::Entry::Info(format!(
        "inserted template `/{}` — edit and press Enter",
        template.name
    )));
}

/// Models usable as a chat brain: text-generation capable, per the
/// provider's catalog. Unknown capability stays — can't confirm it can't.
fn text_generation_models(models: &[ModelInfo]) -> Vec<ModelInfo> {
    models
        .iter()
        .filter(|m| m.supports_text_generation != Some(false))
        .cloned()
        .collect()
}

/// Format a per-Mtok price for a picker detail line: exactly two decimal
/// places, but round numbers drop their trailing zeros (`2.0` → `2`).
fn format_price(price: f64) -> String {
    let s = format!("{price:.2}");
    match s.strip_suffix(".00") {
        Some(int) => int.to_string(),
        None => s,
    }
}

/// Detail line for a model in the `/model` picker.
fn model_detail(model: &ModelInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(name) = &model.display_name {
        parts.push(name.clone());
    }
    if let Some(window) = model.context_window {
        parts.push(format!("ctx {window}"));
    }
    if model.supports_thinking == Some(true) {
        parts.push("thinking".into());
    }
    if let (Some(input), Some(output)) = (model.input_price_per_mtok, model.output_price_per_mtok) {
        parts.push(format!(
            "${} · ${} /Mtok",
            format_price(input),
            format_price(output)
        ));
    }
    parts.join(" · ")
}

/// First non-empty line of a template body, truncated for the picker.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut out: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        out.push('…');
    }
    out
}

/// Only act on press events (release events come from Windows terminals).
fn pressed(key: KeyEvent) -> Option<KeyEvent> {
    if key.kind == KeyEventKind::Press {
        Some(key)
    } else {
        None
    }
}

fn handle_key(
    app: &mut app::App,
    state: &mut SessionState,
    launch: &mut Launch,
    catalog: &mut Catalog,
    tx: &Sender<WorkerEvent>,
    key: KeyEvent,
) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match (ctrl, key.code) {
        (true, KeyCode::Char('c')) | (true, KeyCode::Char('q')) => Action::Quit,
        (true, KeyCode::Char('d')) if app.input.is_empty() => Action::Quit,
        (true, KeyCode::Char('l')) => Action::Clear,
        (true, KeyCode::Char('e')) => Action::OpenEditor,

        (false, KeyCode::Tab) => {
            app.tab_complete();
            Action::None
        }

        (false, KeyCode::Enter) => match app.take_input() {
            Some(prompt) => {
                if let Some(parsed) = commands::parse_command(&prompt, &prompts::templates()) {
                    match parsed {
                        Ok(command) => handle_command(app, state, launch, catalog, command, tx),
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
        (false, KeyCode::Up) => {
            app.history_up();
            Action::None
        }
        (false, KeyCode::Down) => {
            app.history_down();
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
    use crate::settings::Settings;

    fn launch() -> Launch {
        Launch::from_settings("deepinfra", &Settings::default())
    }

    #[test]
    fn picker_for_labels_resume_sessions() {
        let kind = PickerKind::Resume(vec![
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
        ]);
        let picker = picker_for(&kind);
        assert_eq!(picker.title(), "resume");
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

    #[test]
    fn picker_builders_for_m5_commands() {
        let models = PickerKind::Models(vec![ModelInfo {
            id: "meta/Llama-3-70B".into(),
            display_name: Some("Llama 3".into()),
            context_window: Some(8192),
            supports_thinking: Some(true),
            supports_text_generation: Some(true),
            input_price_per_mtok: Some(0.4),
            output_price_per_mtok: Some(1.2),
        }]);
        let picker = picker_for(&models);
        assert_eq!(picker.title(), "model");
        let lines = picker.lines(10);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        let all = texts.join("\n");
        assert!(all.contains("meta/Llama-3-70B"), "{all}");
        assert!(all.contains("Llama 3"), "{all}");
        assert!(all.contains("ctx 8192"), "{all}");
        assert!(all.contains("thinking"), "{all}");
        assert!(all.contains("$0.40 · $1.20 /Mtok"), "{all}");

        assert_eq!(picker_for(&PickerKind::Providers).title(), "provider");
        let providers = picker_for(&PickerKind::Providers)
            .lines(10)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<Vec<String>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(providers.contains("deepinfra"), "{providers}");

        assert_eq!(picker_for(&PickerKind::Thinking).title(), "thinking");
        let thinking = picker_for(&PickerKind::Thinking)
            .lines(10)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<Vec<String>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(thinking.contains("off"), "{thinking}");
        assert!(thinking.contains("4096"), "{thinking}");

        let templates = palette_kind(vec![Template {
            name: "review".into(),
            content: "Review this code\n\nvery carefully".into(),
        }]);
        let picker = picker_for(&templates);
        assert_eq!(picker.title(), "commands");
        let lines = picker.lines(10);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        let all = texts.join("\n");
        assert!(all.contains("/model"), "{all}");
        assert!(all.contains("/resume"), "{all}");
        assert!(all.contains("/review"), "{all}");
        assert!(all.contains("Review this code"), "{all}");
    }

    #[test]
    fn model_detail_combines_fields() {
        assert_eq!(
            model_detail(&ModelInfo {
                id: "m".into(),
                display_name: None,
                context_window: None,
                supports_thinking: None,
                supports_text_generation: None,
                input_price_per_mtok: None,
                output_price_per_mtok: None,
            }),
            ""
        );
        assert_eq!(
            model_detail(&ModelInfo {
                id: "m".into(),
                display_name: Some("Model".into()),
                context_window: Some(4096),
                supports_thinking: Some(true),
                supports_text_generation: Some(true),
                input_price_per_mtok: Some(0.09),
                output_price_per_mtok: Some(0.18),
            }),
            "Model · ctx 4096 · thinking · $0.09 · $0.18 /Mtok"
        );
        // Exactly two decimals, except round numbers lose them entirely.
        assert_eq!(
            model_detail(&ModelInfo {
                id: "m".into(),
                display_name: None,
                context_window: None,
                supports_thinking: None,
                supports_text_generation: None,
                input_price_per_mtok: Some(2.0),
                output_price_per_mtok: Some(1.5),
            }),
            "$2 · $1.50 /Mtok"
        );
    }

    #[test]
    fn model_picker_filters_out_non_text_generation_models() {
        let model = |id: &str, text: Option<bool>| ModelInfo {
            id: id.into(),
            display_name: None,
            context_window: None,
            supports_thinking: None,
            supports_text_generation: text,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
        };
        let filtered = text_generation_models(&[
            model("chat/model", Some(true)),
            model("embed/model", Some(false)),
            model("unknown/model", None),
        ]);
        let ids: Vec<&str> = filtered.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["chat/model", "unknown/model"]);
    }

    #[test]
    fn set_provider_validates_and_resets_model() {
        let mut launch = launch();
        launch.model = Some("old/model".into());
        let message = set_provider(&mut launch, "deepinfra").unwrap();
        assert_eq!(launch.provider, "deepinfra");
        assert_eq!(
            launch.model.as_deref(),
            Some("deepseek-ai/DeepSeek-V4-Flash-0731")
        );
        assert!(message.contains("deepinfra"), "{message}");

        let err = set_provider(&mut launch, "nonexistent").unwrap_err();
        assert!(err.contains("unknown provider `nonexistent`"), "{err}");
        assert_eq!(launch.provider, "deepinfra", "failed switch is a no-op");
    }

    #[test]
    fn sampling_edits_apply_and_validate() {
        let mut cfg = launch();
        let mut app = app::App::new();

        apply_sampling(&mut cfg, &SamplingEdit::Show, &mut app);
        assert!(cfg.sampling.is_none());
        assert!(matches!(app.entries.last(), Some(app::Entry::Info(_))));

        apply_sampling(
            &mut cfg,
            &SamplingEdit::Set(vec![("temperature".into(), "0.7".into())]),
            &mut app,
        );
        assert_eq!(
            cfg.sampling.as_ref().unwrap().get("temperature"),
            Some(&"0.7".to_string())
        );
        apply_sampling(
            &mut cfg,
            &SamplingEdit::Set(vec![("top_p".into(), "0.9".into())]),
            &mut app,
        );
        assert_eq!(cfg.sampling.as_ref().unwrap().len(), 2, "set merges");

        apply_sampling(
            &mut cfg,
            &SamplingEdit::Set(vec![("temperature".into(), "hot".into())]),
            &mut app,
        );
        assert!(matches!(app.entries.last(), Some(app::Entry::Error(_))));
        assert_eq!(
            cfg.sampling.as_ref().unwrap().get("temperature"),
            Some(&"0.7".to_string()),
            "rejected edit leaves the old value"
        );

        apply_sampling(
            &mut cfg,
            &SamplingEdit::Unset(vec!["temperature".into(), "missing".into()]),
            &mut app,
        );
        assert_eq!(
            cfg.sampling.as_ref().unwrap().get("temperature"),
            None,
            "unset removes known keys and ignores unknown ones"
        );
        assert_eq!(
            cfg.sampling.as_ref().unwrap().get("top_p"),
            Some(&"0.9".to_string())
        );

        apply_sampling(&mut cfg, &SamplingEdit::Clear, &mut app);
        assert!(cfg.sampling.is_none());
    }

    #[test]
    fn thinking_commands_set_and_clear() {
        let mut cfg = launch();
        let mut state = SessionState::new(cfg.clone());
        let mut app = app::App::new();
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());

        let action = handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Thinking(Some("2048".into())),
            &tx,
        );
        assert!(matches!(action, Action::None));
        assert_eq!(cfg.thinking.as_deref(), Some("2048"));

        handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Thinking(Some("off".into())),
            &tx,
        );
        assert_eq!(cfg.thinking, None, "off clears thinking");

        handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Thinking(Some("lots".into())),
            &tx,
        );
        assert!(matches!(app.entries.last(), Some(app::Entry::Error(_))));
        assert_eq!(cfg.thinking, None, "rejected value is not applied");
    }

    #[test]
    fn model_and_provider_commands_reconfigure_the_launch() {
        let mut cfg = launch();
        let mut state = SessionState::new(cfg.clone());
        let mut app = app::App::new();
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());

        handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Model(Some("other/model".into())),
            &tx,
        );
        assert_eq!(cfg.model.as_deref(), Some("other/model"));
        assert!(matches!(app.entries.last(), Some(app::Entry::Info(_))));

        handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Provider(Some("nonexistent".into())),
            &tx,
        );
        assert!(matches!(app.entries.last(), Some(app::Entry::Error(_))));
        assert_eq!(cfg.provider, "deepinfra", "failed switch is a no-op");
    }

    #[test]
    fn template_commands_fill_the_input_line() {
        let mut cfg = launch();
        let mut state = SessionState::new(cfg.clone());
        let mut app = app::App::new();
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());
        let template = Template {
            name: "review".into(),
            content: "Review this code".into(),
        };

        handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Template {
                template: template.clone(),
                extra: None,
            },
            &tx,
        );
        assert_eq!(app.input, "Review this code");
        assert_eq!(app.cursor, app.input.len());

        handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Template {
                template,
                extra: Some("  the auth module ".into()),
            },
            &tx,
        );
        assert_eq!(app.input, "Review this code\n\nthe auth module");
    }

    #[test]
    fn first_line_truncates() {
        assert_eq!(first_line("\n\n  hi  \nnext"), "hi");
        let long = "x".repeat(80);
        let shown = first_line(&long);
        assert_eq!(shown.chars().count(), 61, "60 chars + ellipsis");
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn system_command_pushes_labelled_parts() {
        let mut app = app::App::new();
        let parts = vec![
            ("~/.clanky/SYSTEM.md".to_string(), "be terse".to_string()),
            ("AGENTS.md".to_string(), "be helpful".to_string()),
        ];
        push_system_report(&mut app, &parts);
        assert_eq!(app.entries.len(), 3, "summary + one entry per part");
        assert!(matches!(app.entries[0], app::Entry::Info(_)));
        let app::Entry::SystemPart { label, content } = &app.entries[1] else {
            panic!("expected a SystemPart, got {:?}", app.entries[1]);
        };
        assert_eq!(label, "~/.clanky/SYSTEM.md");
        assert_eq!(content, "be terse");
        let app::Entry::SystemPart { label, .. } = &app.entries[2] else {
            panic!("expected a SystemPart, got {:?}", app.entries[2]);
        };
        assert_eq!(label, "AGENTS.md");
    }

    #[test]
    fn system_command_without_context_pushes_a_note() {
        let mut app = app::App::new();
        push_system_report(&mut app, &[]);
        assert_eq!(app.entries.len(), 1);
        assert!(matches!(app.entries[0], app::Entry::Info(_)));
    }
}
