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
//! `/md` exports the session as Markdown (`crate::export`): optional
//! `--all`/`--thinking`/`--tools` flags choose which record types are
//! written, and the target file is either named on the command line or
//! asked for in a modal (see `modal.rs`).
//!
//! Input ergonomics (M7): Tab completes the file path at the caret (longest
//! common prefix first, further Tabs cycle candidates, shown in a popup
//! above the input line); ctrl+e opens `$EDITOR` with the prompt buffer —
//! raw mode is dropped, the editor owns the terminal, and the TUI resumes
//! with the edited text.
//!
//! The input is multi-line: `ctrl+j` inserts a newline, a bracketed paste
//! is inserted whole (only its text, never as a submit), and `↑`/`↓`/ctrl+a/
//! ctrl+e move within the text being edited — `↑` recalls the previous
//! prompt only from the top row of a single-row buffer or a multi-line
//! buffer's first row, and `↓` steps forward through history only from the
//! bottom row.
//!
//! ctrl+t toggles whether streamed thinking is displayed (see
//! [`app::App::toggle_thinking_display`]). It is display-only — the session
//! file always records the full text — and, because the printed transcript
//! cannot be rewritten, it takes effect from the delta that arrives next:
//! hidden blocks collapse to a single `Thinking...` line, and the toggle
//! works mid-stream in both directions.
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
pub use app::App;
mod commands;
mod completion;
mod editor;
mod markdown;
mod modal;
mod picker;
mod screen;

use std::io::{IsTerminal as _, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use clanky_protocol::{ChatMessage, ModelInfo};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use serde_json::Value;

use crate::error::Result;
use crate::export::{self, ExportOptions};
use crate::prompts::{self, Template};
use crate::provider::ProviderSession;
use crate::recorder;
use crate::session::{self, SessionInfo};
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
/// attached to a terminal (design-docs/plan.md: no TUI when redirected).
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
    /// Retry budget for retryable provider errors (`None` = default,
    /// `Some(0)` = never retry).
    pub max_retries: Option<u32>,
    /// Prompt from the command line, sent as the first chat message.
    pub initial_prompt: Option<String>,
    /// `--resume NAME` loads that session; `--resume` (empty string) opens
    /// the picker at startup.
    pub resume: Option<String>,
    /// Whether the session is recorded (`--no-session` / `session =
    /// false` turns recording off): no file is created, `/name` and
    /// `/resume` report that sessions are off, and the status line shows
    /// `(no session)`.
    pub recording: bool,
    /// Project label printed at the left of the input prompt
    /// (`project` setting; `None` = no label).
    pub project: Option<String>,
}

impl Launch {
    /// Build from merged settings for the chosen provider. When the settings
    /// name no model, the plugin's advertised default (from the handshake)
    /// fills it in at the first turn / catalog fetch (M8) — core keeps no
    /// per-provider default table.
    pub fn from_settings(provider: &str, settings: &Settings) -> Self {
        Self {
            provider: provider.to_string(),
            model: settings.model.clone(),
            sampling: settings.sampling.clone(),
            thinking: settings.thinking.clone(),
            max_tool_rounds: settings.max_tool_rounds,
            max_retries: settings.max_retries,
            initial_prompt: settings
                .prompt
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string),
            resume: None,
            recording: settings.record_sessions(),
            project: settings.project.clone(),
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
    /// The long-lived plugin session, handed back to the UI thread so the
    /// next turn reuses the same process (M8 decision #2). `None` when the
    /// session could not be created (the next turn retries).
    session: Option<ProviderSession>,
}

enum WorkerEvent {
    Turn(TurnEvent),
    /// Terminal status of the turn.
    Done(TurnEnd),
    /// Model list for the [`Catalog`], plus the plugin's advertised default
    /// model (from the handshake).
    Models(crate::error::Result<CatalogFetch>),
}

/// The shared session recorder (crate::recorder), wired to surface its
/// write failures as transcript errors.
type SessionState = recorder::Recorder;

/// `recorder::ErrorSink` that pushes session write failures onto the
/// transcript, so a broken session file is visible but never aborts the
/// turn. The queue is drained into `app.entries` by the render pass.
#[derive(Default)]
struct TranscriptErrors {
    messages: Vec<String>,
}

impl TranscriptErrors {
    /// Move the pending messages onto the transcript.
    fn drain(&mut self, app: &mut app::App) {
        for message in self.messages.drain(..) {
            app.entries.push(app::Entry::Error(message));
        }
    }
}

impl recorder::ErrorSink for TranscriptErrors {
    fn report(&mut self, message: String) {
        self.messages.push(message);
    }
}

/// One catalog fetch: the provider it was for, its models, and its default
/// model. The provider name guards against a stale fetch (a switch races an
/// in-flight listing) adopting the wrong provider's default.
struct CatalogFetch {
    provider: String,
    models: Vec<ModelInfo>,
    default_model: Option<String>,
}

/// Effective per-million-token prices for one model, as used by the
/// session-cost display. `cache_read` is the billed rate for prompt
/// tokens the provider served from its cache; it defaults to the full
/// input rate when the catalog does not separate them.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pricing {
    input: f64,
    output: f64,
    cache_read: f64,
}

impl Pricing {
    /// Estimated session cost in dollars: billed token totals at their
    /// billed rates (uncached prompt at `input`, cached prompt at
    /// `cache_read`, completion at `output`), plus the live estimates for
    /// content no provider report covers yet — estimated prompt tokens at
    /// the input rate, estimated completion tokens at the output rate.
    /// The estimates keep the display moving between usage reports, the
    /// same way they feed the ctx % (see `App::estimated_tokens`).
    fn session_cost(
        &self,
        billed: &app::BilledUsage,
        estimated_prompt: u64,
        estimated_completion: u64,
    ) -> f64 {
        let uncached = billed.prompt_tokens - billed.cached_tokens;
        ((uncached + estimated_prompt) as f64 * self.input
            + billed.cached_tokens as f64 * self.cache_read
            + (billed.completion_tokens + estimated_completion) as f64 * self.output)
            / 1_000_000.0
    }
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

    /// Pricing of `model` (dollars per million tokens) when the catalog
    /// matches the active provider and carries input/output prices. The
    /// cache-read price falls back to the input price when the catalog
    /// does not distinguish them (most models do not).
    fn pricing_for(&self, provider: &str, model: Option<&str>) -> Option<Pricing> {
        if self.provider != provider {
            return None;
        }
        let info = self.models.iter().find(|m| Some(m.id.as_str()) == model)?;
        Some(Pricing {
            input: info.input_price_per_mtok?,
            output: info.output_price_per_mtok?,
            cache_read: info
                .cache_read_price_per_mtok
                .unwrap_or(info.input_price_per_mtok?),
        })
    }
}

/// Estimated session cost in dollars for the status line: catalog pricing
/// (dollars per million tokens) × token totals. Every chat round is its own
/// billed request (its report covers that request's full prompt), and each
/// turn re-sends the whole conversation, so reports simply sum across
/// rounds and turns. Cached prompt tokens bill at the cheaper cache-read
/// rate. Live estimates for content no report covers yet (the user prompt
/// and tool results between rounds, streamed text/thinking while they
/// stream) count in as well, so the cost ticks up during streaming the
/// same way the ctx % does.
fn session_cost(catalog: &Catalog, launch: &Launch, app: &App) -> Option<f64> {
    let pricing = catalog.pricing_for(&launch.provider, launch.model.as_deref())?;
    let (estimated_prompt, estimated_completion) = app.estimated_tokens();
    Some(pricing.session_cost(&app.billed_usage, estimated_prompt, estimated_completion))
}

/// Enables bracketed paste on construction and disables it on drop, so the
/// terminal keeps no stale paste mode when an error unwinds through `run`
/// (including past `restore`, whose early `?` returns skip explicit
/// cleanup).
struct PasteGuard(std::io::Stdout);

impl Drop for PasteGuard {
    fn drop(&mut self) {
        let _ = ratatui::crossterm::execute!(self.0, DisableBracketedPaste);
    }
}

/// Run the interactive session until the user quits.
pub fn run(mut launch: Launch) -> Result<()> {
    enable_raw_mode()?;
    // Bracketed paste: pasted text arrives as one `Event::Paste` (never as
    // key events), so a pasted newline cannot submit.
    let mut paste = PasteGuard(std::io::stdout());
    let _ = ratatui::crossterm::execute!(paste.0, EnableBracketedPaste);
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

/// Spawn one agentic turn on a worker thread, reusing the long-lived plugin
/// session when one exists (M8 decision #2). The session is moved in and
/// handed back in [`TurnEnd`], so the process survives across turns without
/// an extra thread. When no session exists yet, one is spawned lazily here
/// (settled open question #2).
fn spawn_turn(
    history: Vec<ChatMessage>,
    launch: &Launch,
    session: Option<ProviderSession>,
    prompt: String,
    tx: Sender<WorkerEvent>,
) {
    let launch = launch.clone();
    let mut messages = history;
    messages.push(ChatMessage::user(prompt));
    std::thread::Builder::new()
        .name("clanky-turn".into())
        .spawn(move || {
            // Reuse the session from the previous turn, or spawn the plugin
            // lazily on first use (settled open question #2). A session from
            // a different provider (after `/provider`) or a crashed process
            // is discarded and respawned here (restart on next use, spec §8).
            // A failed spawn keeps `session` as `None`, so the next turn
            // retries.
            let mut session = match session {
                Some(session) if session.is_alive() && session.name() == launch.provider => {
                    Some(session)
                }
                _ => None,
            };
            let outcome = (|| {
                if session.is_none() {
                    session = Some(crate::provider::create(&launch.provider)?);
                }
                let session = session.as_mut().expect("session was just ensured");
                // The plugin's advertised default fills in for an unset model.
                let model = launch
                    .model
                    .clone()
                    .or_else(|| session.default_model().map(str::to_string));
                let config = TurnConfig {
                    model,
                    sampling: launch.sampling.clone(),
                    thinking: launch.thinking.clone(),
                    max_tool_rounds: launch.max_tool_rounds,
                    max_retries: launch.max_retries,
                    retry_sleep: None,
                };
                crate::turn::run_turn_with(
                    session.client_mut(),
                    &crate::tools::default_tools(),
                    &mut messages,
                    &config,
                    &mut |event: TurnEvent| {
                        let _ = tx.send(WorkerEvent::Turn(event));
                    },
                )
            })();
            // Hand the conversation and the session back in every case, not
            // just on success: an aborted turn still leaves valid context
            // behind, and the plugin process stays alive for the next turn.
            let _ = tx.send(WorkerEvent::Done(TurnEnd {
                result: outcome,
                history: messages,
                session,
            }));
        })
        .expect("failed to spawn turn worker thread");
}

/// Fetch the provider's model list on a worker thread (network I/O must
/// not block the render loop); the result feeds [`Catalog`].
///
/// Model listing happens before the first turn (there is no long-lived
/// session yet) and after a `/provider` switch, so it uses a short-lived
/// plugin process of its own. The handshake also yields the provider's
/// default model, which the catalog fetch reports back.
fn spawn_models(provider_name: String, tx: Sender<WorkerEvent>) {
    std::thread::Builder::new()
        .name("clanky-models".into())
        .spawn(move || {
            let outcome: crate::error::Result<CatalogFetch> = (|| {
                let mut session = crate::provider::create(&provider_name)?;
                let models = session.client_mut().list_models()?;
                Ok(CatalogFetch {
                    provider: provider_name.clone(),
                    models,
                    default_model: session.default_model().map(str::to_string),
                })
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
    /// Toggle the display of streamed thinking (ctrl+t); display-only.
    ToggleThinking,
    Submit(String),
    /// Open a picker; the kind carries the data it selects from.
    OpenPicker(PickerKind),
    /// Open a modal asking for the `/md` export filename.
    OpenExportModal(ExportSource),
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
    ("/md ", "export the session as Markdown"),
    ("/name ", "rename the session"),
    ("/resume", "load a saved session"),
];

/// Status-line text shown in place of the session name while session
/// recording is off (`--no-session`): there is no session file.
const NO_SESSION_STATUS: &str = "(no session)";

/// The status line's session element: the open session file's name while
/// recording, `(no session)` when recording is off.
fn status_session_name(state: &SessionState) -> Option<String> {
    if state.recording_enabled() {
        state.current_name()
    } else {
        Some(NO_SESSION_STATUS.to_string())
    }
}

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
                    label: name.clone(),
                    // The resolved command is cheap and needs no handshake;
                    // the default model is only known after a handshake, so
                    // it is not shown here.
                    detail: crate::provider::command_for(name)
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
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

/// The complete input of one `/md` export, captured when the command runs
/// so a modal that is still open exports what the command saw rather than
/// whatever the session looks like when the filename is confirmed.
struct ExportSource {
    /// Session display name (the file stem), when there is one.
    name: Option<String>,
    header: session::Header,
    records: Vec<session::Record>,
    options: ExportOptions,
}

impl ExportSource {
    /// Capture the current session. A session with an open file exports
    /// exactly the records that file holds — the same stream `/resume`
    /// would restore — while a session with no file (nothing recorded
    /// yet, or recording off) exports what the screen showed: a fresh
    /// header plus records derived from the transcript entries.
    fn snapshot(state: &SessionState, app: &app::App, options: ExportOptions) -> Result<Self> {
        let name = state.current_name();
        let (header, records) = match state.open_file() {
            Some(path) if path.exists() => {
                let data = session::load(path)?;
                (data.header, data.records)
            }
            _ => (state.fresh_header(), app.screen_records()),
        };
        Ok(Self {
            name,
            header,
            records,
            options,
        })
    }

    /// Render the document and write it to `target`; the returned count
    /// is how many session records went into it.
    fn write(&self, target: &str) -> std::io::Result<usize> {
        let document = export::render(
            self.name.as_deref(),
            &self.header,
            &self.records,
            self.options,
        );
        write_export(target, &document)?;
        Ok(self.records.len())
    }
}

/// Write an export and report the outcome in the transcript.
fn apply_export(app: &mut app::App, target: &str, source: &ExportSource) {
    match source.write(target) {
        Ok(count) => app.entries.push(app::Entry::Info(format!(
            "exported {count} record(s) to `{target}`"
        ))),
        Err(err) => app
            .entries
            .push(app::Entry::Error(format!("cannot write `{target}`: {err}"))),
    }
}

/// Write an exported document, creating parent directories as needed.
fn write_export(target: &str, document: &str) -> std::io::Result<()> {
    let path = PathBuf::from(target);
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, document)
}

/// Load a saved session into the app: transcript, usage and history;
/// subsequent records continue the same file.
fn load_session(
    path: &std::path::Path,
    name: &str,
    app: &mut app::App,
    state: &mut SessionState,
) -> Result<()> {
    let data = session::load(path)?;
    let conversation = app.restore(&data.records);
    app.history = crate::context::system_messages();
    app.history.extend(conversation);
    state.reopen(path.to_path_buf());
    app.entries.push(app::Entry::Info(format!(
        "resumed `{name}` ({} records)",
        data.records.len()
    )));
    Ok(())
}

fn event_loop(launch: &mut Launch, screen: &mut screen::Screen<Stdout>) -> Result<()> {
    let (tx, rx) = mpsc::channel::<WorkerEvent>();
    let mut app = app::App::new(launch.project.clone());
    // Fresh session: seed the system context so the first turn sends it
    // (resumed sessions rebuild it inside `load_session`).
    app.history = crate::context::system_messages();
    let mut state =
        SessionState::new(&launch.provider, launch.model.as_deref()).recording(launch.recording);
    let mut transcript_errors = TranscriptErrors::default();
    // Prefetch the provider's model catalog in the background: it prices
    // the session-cost display and makes `/model` instant. The fetch also
    // reports the plugin's advertised default model.
    let mut catalog = Catalog::new(launch.provider.clone());
    catalog.request(launch.provider.clone(), &tx, false);
    // The long-lived plugin session (M8 decision #2): spawned lazily on the
    // first turn, moved into the turn worker and handed back on completion.
    let mut session_owner: Option<ProviderSession> = None;
    // Open picker with the payload it selects from, when active.
    let mut picker: Option<(picker::Picker, PickerKind)> = None;
    // Open modal, with the `/md` export it will write, when active.
    let mut modal: Option<(modal::Modal, ExportSource)> = None;
    // Something changed since the last render (transcript, input, popup).
    let mut dirty = true;

    // `--resume NAME` loads the session before the first render; plain
    // `--resume` opens the picker. A resumed transcript is committed to
    // the terminal on the first render.
    match launch.resume.as_deref() {
        Some("") => {
            let kind = PickerKind::Resume(session::list(state.dir()));
            picker = Some((picker_for(&kind), kind));
        }
        Some(name) => {
            let path = session::find_by_name(name)?;
            load_session(&path, name, &mut app, &mut state)?;
        }
        None => {}
    }

    // A command-line prompt starts the first turn immediately.
    if let Some(prompt) = launch.initial_prompt.clone() {
        app.push_user(&prompt);
        state.record(
            &mut transcript_errors,
            session::Record::User {
                text: prompt.clone(),
            },
        );
        app.busy = true;
        spawn_turn(
            app.history.clone(),
            launch,
            session_owner.take(),
            prompt,
            tx.clone(),
        );
    }

    loop {
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(key) = pressed(key) {
                        let tab = key.code == KeyCode::Tab
                            && !key.modifiers.contains(KeyModifiers::CONTROL);
                        if let Some((mut dialog, source)) = modal.take() {
                            // Ctrl+C/Ctrl+Q quit the whole TUI, exactly as
                            // they do while a picker is open; Esc closes the
                            // dialog alone.
                            if key.modifiers.contains(KeyModifiers::CONTROL)
                                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
                            {
                                break;
                            }
                            match dialog.key(key) {
                                // Still editing: put the dialog back.
                                modal::ModalOutcome::None => modal = Some((dialog, source)),
                                modal::ModalOutcome::Cancelled => {}
                                modal::ModalOutcome::Accepted => {
                                    // Enter only accepts a non-blank value.
                                    if let Some(value) = dialog.value() {
                                        apply_export(&mut app, &value, &source);
                                    }
                                }
                            }
                        } else if picker.is_some() {
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
                                Action::ToggleThinking => app.toggle_thinking_display(),
                                Action::OpenPicker(kind) => {
                                    app.completion = None;
                                    picker = Some((picker_for(&kind), kind));
                                }
                                Action::OpenExportModal(source) => {
                                    app.completion = None;
                                    modal = Some((export_modal(), source));
                                }
                                Action::OpenEditor => {
                                    if let Err(err) = run_editor(&mut app, screen) {
                                        app.entries.push(app::Entry::Error(err.to_string()));
                                    }
                                }
                                Action::Submit(prompt) => {
                                    if app.busy {
                                        // The message is NOT in the context
                                        // yet: show a queued placeholder and
                                        // wait for the turn to end. The real
                                        // user entry, session record and
                                        // estimate are applied at dispatch
                                        // (below), when the prompt is
                                        // actually sent.
                                        app.entries.push(app::Entry::Queued {
                                            preview: queued_preview(&prompt),
                                        });
                                        app.pending.push(prompt);
                                    } else {
                                        app.push_user(&prompt);
                                        state.record(
                                            &mut transcript_errors,
                                            session::Record::User {
                                                text: prompt.clone(),
                                            },
                                        );
                                        app.busy = true;
                                        spawn_turn(
                                            app.history.clone(),
                                            launch,
                                            session_owner.take(),
                                            prompt,
                                            tx.clone(),
                                        );
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
                // Bracketed paste: pasted text is one event, its
                // newlines included — insert it whole at the caret, never
                // as a submit. Only a pressed Enter submits.
                Event::Paste(text) => {
                    app.insert_str(&text);
                    dirty = true;
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
                    recorder::record_turn_event(&mut state, &mut transcript_errors, &turn_event);
                }
                WorkerEvent::Done(mut done) => {
                    // Reclaim the long-lived plugin process for the next turn.
                    if let Some(session) = done.session.take() {
                        session_owner = Some(session);
                    }
                    state.record_done(&mut transcript_errors, &done.result);
                    // Adopt the conversation as the turn left it, even on
                    // error: the partial context stays continuable.
                    app.history = done.history.clone();
                    app.on_turn_done(&done.result);
                    turn_finished = true;
                }
                WorkerEvent::Models(result) => match result {
                    Ok(fetched) => {
                        let empty = fetched.models.is_empty();
                        // Adopt the plugin's advertised default when the
                        // user/settings named no model. A late response for
                        // a provider that is no longer active is ignored.
                        if fetched.provider == launch.provider && launch.model.is_none() {
                            launch.model = fetched.default_model.clone();
                        }
                        catalog.models = fetched.models;
                        if catalog.for_picker {
                            catalog.for_picker = false;
                            if empty {
                                app.entries.push(app::Entry::Info(format!(
                                    "`{}` lists no models",
                                    launch.provider
                                )));
                            } else if picker.is_none() {
                                let kind = PickerKind::Models(chat_capable_models(&catalog.models));
                                picker = Some((picker_for(&kind), kind));
                            }
                        }
                    }
                    Err(err) => app.entries.push(app::Entry::Error(err.to_string())),
                },
            }
        }
        // Only consume the queued message when a turn actually finished;
        // taking it unconditionally would drop it whenever the turn was
        // still running.
        if let Some(prompt) = take_queued(turn_finished, &mut app.pending) {
            // The queued prompt now enters the context: print it in full
            // at its real place in the history (the placeholder above it
            // stays printed — terminal history cannot be rewritten),
            // record it, and count it into the estimate.
            app.push_user(&prompt);
            state.record(
                &mut transcript_errors,
                session::Record::User {
                    text: prompt.clone(),
                },
            );
            app.busy = true;
            spawn_turn(
                app.history.clone(),
                launch,
                session_owner.take(),
                prompt,
                tx.clone(),
            );
        }

        if dirty {
            transcript_errors.drain(&mut app);
            let session_name = status_session_name(&state);
            let cost = session_cost(&catalog, launch, &app);
            // `off`/empty mean disabled and are not shown.
            let thinking = launch
                .thinking
                .as_deref()
                .filter(|value| !value.is_empty() && *value != "off" && *value != "0");
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
            let modal_ref = modal.as_ref().map(|(m, _)| m);
            let completion = app.completion.clone();
            screen.render(
                &mut app,
                &status,
                picker_ref,
                modal_ref,
                completion.as_ref(),
            )?;
            dirty = false;
        }
    }
    // Ctrl+C/Ctrl+Q/Ctrl+D quits mid-turn without waiting for the worker:
    // record what it already sent, then consolidate the in-flight round's
    // deltas, so `/resume` restores exactly what the terminal showed.
    drain_turn_events(&rx, &mut state, &mut transcript_errors);
    Ok(())
}

/// Drain the turn events that arrived before the quit, recording them
/// exactly as the live loop does: a round or tool result that completed
/// just before ctrl+c must be kept too. The turn's `Done` is dropped —
/// the process is exiting, and an error that never reached the screen
/// must not reach the resumed transcript either.
fn drain_turn_events(
    rx: &mpsc::Receiver<WorkerEvent>,
    state: &mut SessionState,
    errors: &mut dyn recorder::ErrorSink,
) {
    while let Ok(event) = rx.try_recv() {
        if let WorkerEvent::Turn(turn_event) = event {
            recorder::record_turn_event(state, errors, &turn_event);
        }
    }
    state.flush_partial(errors);
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

/// The modal that asks for the `/md` export filename (the options the
/// command was invoked with are carried beside it in the event loop).
fn export_modal() -> modal::Modal {
    modal::Modal::new(
        "export markdown",
        "file name (relative paths resolve against the working directory)",
        "",
    )
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
                && let Err(err) = load_session(&info.path, &info.name, app, state)
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
            if let Some(name) = crate::provider::available().get(index).cloned() {
                switch_provider(launch, catalog, tx, &name, app);
            }
        }
        PickerKind::Thinking => {
            if let Some((value, _)) = THINKING_OPTIONS.get(index) {
                // `off` still occupies `launch.thinking` — the zero budget
                // it parses to must reach the provider, so the provider
                // sends its explicit "no reasoning" knob instead of
                // falling back to the backend default.
                let parsed = crate::turn::parse_thinking(value).ok();
                launch.thinking = parsed.map(|_| value.to_string());
                let disabled = parsed.is_some_and(|t| t.budget_tokens == Some(0));
                app.entries.push(app::Entry::Info(if disabled {
                    "thinking disabled".into()
                } else {
                    format!("thinking set to `{value}`")
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
                        crate::provider::refresh();
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
            if !state.recording_enabled() {
                app.entries
                    .push(app::Entry::Info(recorder::Recorder::disabled_message()));
                return Action::None;
            }
            match state.set_name(name) {
                Ok(info) => app.entries.push(app::Entry::Info(info)),
                Err(message) => app.entries.push(app::Entry::Error(message)),
            }
            Action::None
        }
        Command::Resume => {
            if !state.recording_enabled() {
                app.entries
                    .push(app::Entry::Info(recorder::Recorder::disabled_message()));
                return Action::None;
            }
            if app.busy {
                app.entries.push(app::Entry::Info(
                    "wait for the current turn to finish".into(),
                ));
                return Action::None;
            }
            let infos = session::list(state.dir());
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
                Action::OpenPicker(PickerKind::Models(chat_capable_models(&catalog.models)))
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
            switch_provider(launch, catalog, tx, &name, app);
            Action::None
        }
        Command::Provider(None) => {
            // Re-scan $PATH so a plugin installed since startup shows up.
            crate::provider::refresh();
            Action::OpenPicker(PickerKind::Providers)
        }
        Command::Thinking(Some(raw)) => {
            match crate::turn::parse_thinking(&raw).map_err(|e| e.to_string()) {
                Ok(parsed) => {
                    // `off` still occupies `launch.thinking` — the zero
                    // budget it parses to must reach the provider, so the
                    // provider sends its explicit "no reasoning" knob
                    // instead of falling back to the backend default.
                    launch.thinking = Some(raw.clone());
                    let disabled = parsed.budget_tokens == Some(0);
                    app.entries.push(app::Entry::Info(if disabled {
                        "thinking disabled".into()
                    } else {
                        format!("thinking set to `{raw}`")
                    }));
                }
                Err(err) => app.entries.push(app::Entry::Error(err)),
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
        Command::Export { options, path } => {
            let source = match ExportSource::snapshot(state, app, options) {
                Ok(source) => source,
                Err(err) => {
                    app.entries
                        .push(app::Entry::Error(format!("cannot export session: {err}")));
                    return Action::None;
                }
            };
            match path {
                Some(path) => {
                    apply_export(app, &path, &source);
                    Action::None
                }
                // No filename: ask for one.
                None => Action::OpenExportModal(source),
            }
        }
        Command::Palette => Action::OpenPicker(palette_kind(prompts::templates())),
        Command::Template { template, extra } => {
            apply_template(app, &template, extra.as_deref());
            Action::None
        }
    }
}

/// Push the `/system` report to the transcript: a summary `Info` line,
/// then one `SystemPart` entry per part with the content shown verbatim
/// (the parts already carry their own provenance headings), or a single
/// `Info` explaining there is no context. Pure apart from `app`, so it
/// can be tested with synthetic parts.
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
        for (_, content) in parts {
            app.entries.push(app::Entry::SystemPart {
                content: content.clone(),
            });
        }
    }
}

/// Validate a provider name against discovery; the model is cleared (model
/// ids do not transfer between providers) and filled from the new plugin's
/// handshake at the next turn.
fn set_provider(launch: &mut Launch, name: &str) -> std::result::Result<(), String> {
    if crate::provider::find(name).is_none() {
        return Err(format!(
            "unknown provider `{name}`; discovered plugins: {}",
            crate::provider::available().join(", ")
        ));
    }
    launch.provider = name.to_string();
    launch.model = None;
    Ok(())
}

/// Apply a provider switch: tear down the old session (the next turn spawns
/// the new plugin), reset the model, and refresh the catalog.
fn switch_provider(
    launch: &mut Launch,
    catalog: &mut Catalog,
    tx: &Sender<WorkerEvent>,
    name: &str,
    app: &mut app::App,
) {
    match set_provider(launch, name) {
        Ok(()) => {
            // The model is resolved from the new plugin's handshake on the
            // next turn; refresh the catalog so pricing follows the provider.
            catalog.request(name.to_string(), tx, false);
            app.entries
                .push(app::Entry::Info(format!("provider set to `{name}`")));
        }
        Err(message) => app.entries.push(app::Entry::Error(message)),
    }
}

/// Apply a `/sampling` edit to the launch config, reporting as an entry.
/// Parameters are pass-through: keys and values are not validated here (or
/// at turn time) — the provider backend validates them at call time.
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
            let current = launch.sampling.get_or_insert_with(SamplingParams::new);
            for (key, value) in pairs {
                current.insert(key.clone(), Value::String(value.clone()));
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

/// `key=value` pairs joined for display; string values are shown raw.
fn describe_sampling(params: &SamplingParams) -> String {
    params
        .iter()
        .map(|(key, value)| match value {
            Value::String(raw) => format!("{key}={raw}"),
            other => format!("{key}={other}"),
        })
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

/// Models the `/model` picker offers: text-generation and tool capable, per
/// the provider's catalog. Clanky is agentic — a model that cannot call
/// tools cannot drive the loop — so tool-less models are deal breakers and
/// the picker hides them. Unknown capability stays — can't confirm it
/// can't. Picker curation, not enforcement: `--model`/settings name a model
/// directly and the backend's own error for it is surfaced unchanged.
fn chat_capable_models(models: &[ModelInfo]) -> Vec<ModelInfo> {
    models
        .iter()
        .filter(|m| m.supports_text_generation != Some(false) && m.supports_tools != Some(false))
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
        if let Some(cache) = model.cache_read_price_per_mtok {
            parts.push(format!(
                "${} cached · ${} in · ${} out /Mtok",
                format_price(cache),
                format_price(input),
                format_price(output)
            ));
        } else {
            parts.push(format!(
                "${} · ${} /Mtok",
                format_price(input),
                format_price(output)
            ));
        }
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

/// Consume the queued message, but only when the running turn has ended:
/// a prompt typed mid-turn must survive every event-loop iteration until
/// the `Done` event frees the turn slot. (`Option::take` inside an `if let`
/// scrutinee would instead drop it the moment the turn was still busy.)
/// Truncated single-line preview of a queued prompt, for the
/// placeholder shown while the message waits for the turn to end.
fn queued_preview(prompt: &str) -> String {
    let single = prompt.lines().collect::<Vec<_>>().join("\u{21aa}");
    if let Some((cut, _)) = single.char_indices().nth(48) {
        return format!("{}\u{2026}", &single[..cut]);
    }
    single
}

fn take_queued(turn_finished: bool, pending: &mut Vec<String>) -> Option<String> {
    if turn_finished && !pending.is_empty() {
        Some(pending.remove(0))
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
        (true, KeyCode::Char('t')) => Action::ToggleThinking,
        (true, KeyCode::Char('a')) => {
            app.cursor_line_home();
            Action::None
        }
        (true, KeyCode::Char('e')) => {
            // End of the current line when the buffer spans several
            // lines; on the last row, open `$EDITOR` (readline semantics).
            if app.cursor_line_end() {
                Action::OpenEditor
            } else {
                Action::None
            }
        }
        (true, KeyCode::Char('j')) => {
            app.insert_char('\n');
            Action::None
        }
        (true, KeyCode::Char('u')) => {
            app.clear_to_start();
            Action::None
        }

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
            // On the top row the buffer is done — recall the previous
            // prompt; inside a multi-line buffer, move within the text.
            if app.cursor_up() == 0 {
                app.history_up();
            }
            Action::None
        }
        (false, KeyCode::Down) => {
            // On the bottom row, step forward through the history;
            // inside a multi-line buffer, move within the text.
            if app.cursor_down() == 0 {
                app.history_down();
            }
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
    use crate::provider::Plugin;
    use crate::settings::Settings;

    fn launch() -> Launch {
        Launch::from_settings("deepinfra", &Settings::default())
    }
    /// An `ErrorSink` for tests where write failures cannot happen.
    struct NoErrors;

    impl recorder::ErrorSink for NoErrors {
        fn report(&mut self, message: String) {
            panic!("unexpected session write failure: {message}");
        }
    }

    /// Install a fake discovered-plugin list for the duration of one test.
    /// The cache is process-wide, so the lock serializes these tests; tests
    /// that only read `launch()` never touch discovery.
    fn with_providers(plugins: &[(&str, &str)]) -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::provider::override_for_test(
            plugins
                .iter()
                .map(|(name, path)| Plugin {
                    name: (*name).to_string(),
                    path: PathBuf::from(path),
                })
                .collect(),
        );
        guard
    }

    #[test]
    fn ctrl_u_clears_from_cursor_to_start() {
        let mut cfg = launch();
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        let mut app = app::App::new(None);
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());
        app.input = "hello".into();
        app.cursor = 3;
        let key = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, key);
        assert_eq!(app.input, "lo");
        assert_eq!(app.cursor, 0);
        // Plain `u` still types a character.
        let plain = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE);
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, plain);
        assert_eq!(app.input, "ulo");
    }

    #[test]
    fn ctrl_t_toggles_thinking_display() {
        let mut cfg = launch();
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        let mut app = app::App::new(None);
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());

        let key = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, key),
            Action::ToggleThinking
        ));
        // Plain `t` still types a character.
        let plain = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE);
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, plain);
        assert_eq!(app.input, "t");
    }

    #[test]
    fn up_recalls_history_only_from_the_top_row() {
        let mut cfg = launch();
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        let mut app = app::App::new(None);
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());
        app.input = "earlier".into();
        app.cursor = 7;
        app.take_input();

        // Single-row buffer: ↑ recalls the previous prompt (top row).
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, up);
        assert_eq!(app.input, "earlier");

        // Multi-line buffer, caret on the top row: ↑ still recalls.
        app.history_down(); // back to live
        app.input = "top\nbottom".into();
        app.cursor = 0;
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, up);
        assert_eq!(app.input, "earlier");

        // ... but on a lower row it moves within the text instead.
        app.history_down();
        app.input = "top\nbottom".into();
        app.cursor = 6;
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, up);
        assert_eq!(app.input, "top\nbottom");
        assert_eq!(app.cursor, 2, "column 2 of the top row");
    }

    #[test]
    fn down_steps_history_only_from_the_bottom_row() {
        let mut cfg = launch();
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        let mut app = app::App::new(None);
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());
        app.input = "earlier".into();
        app.cursor = 7;
        app.take_input();

        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        // Recall with ↑ first, then build a multi-line buffer over it.
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, up);
        assert_eq!(app.input, "earlier");
        app.input = "row1\nrow2\nrow3".into();
        app.cursor = app.input.find("row2").unwrap();

        // ↓ on the middle row: within the text, no history step.
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, down);
        assert_eq!(app.input, "row1\nrow2\nrow3", "unchanged");
        assert_eq!(
            app.cursor,
            app.input.find("row3").unwrap(),
            "onto the bottom row"
        );

        // ↓ on the bottom row: a history step (past the newest entry, the
        // saved draft comes back — here the empty draft).
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, down);
        assert_eq!(app.input, "");
    }

    #[test]
    fn ctrl_j_inserts_a_newline_and_ctrl_a_e_move_within_lines() {
        let mut cfg = launch();
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        let mut app = app::App::new(None);
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());

        app.input = "one two".into();
        app.cursor = 3;
        let j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, j);
        assert_eq!(app.input, "one\n two");

        // ctrl+a / ctrl+e are line-scoped.
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        let e = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, a);
        assert_eq!(app.cursor, 4, "start of the second row");
        handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, e);
        assert_eq!(app.cursor, 8, "end of the second row");
        // On the last row at the buffer end, ctrl+e opens $EDITOR.
        assert!(matches!(
            handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, e),
            Action::OpenEditor
        ));
        // ... mid-buffer it does not.
        app.cursor = 1;
        assert!(matches!(
            handle_key(&mut app, &mut state, &mut cfg, &mut catalog, &tx, e),
            Action::None
        ));
        assert_eq!(app.cursor, 3, "end of the first row");
    }

    #[test]
    fn toggling_mid_stream_collapses_and_resumes() {
        let mut app = app::App::new(None);
        app.on_turn_event(TurnEvent::Thinking {
            delta: "one ".into(),
        });
        app.toggle_thinking_display(); // off mid-stream
        app.on_turn_event(TurnEvent::Thinking {
            delta: "two".into(),
        });
        assert_eq!(
            app.entries,
            vec![
                app::Entry::Thinking("one ".into()),
                app::Entry::ThinkingHidden
            ]
        );
        app.toggle_thinking_display(); // on mid-stream: new block below
        app.on_turn_event(TurnEvent::Thinking {
            delta: "three".into(),
        });
        assert_eq!(
            app.entries,
            vec![
                app::Entry::Thinking("one ".into()),
                app::Entry::ThinkingHidden,
                app::Entry::Thinking("three".into()),
            ]
        );
    }

    #[test]
    fn queued_prompts_survive_until_the_turn_finishes() {
        let mut pending = vec!["second".to_string()];
        // The turn is still running: the queue must not be touched.
        assert_eq!(take_queued(false, &mut pending), None);
        assert_eq!(pending, vec!["second"]);
        // The turn ended: one prompt leaves the queue per finished turn,
        // in the order they were typed.
        pending.push("third".into());
        assert_eq!(take_queued(true, &mut pending).as_deref(), Some("second"));
        assert_eq!(pending, vec!["third"]);
        assert_eq!(take_queued(true, &mut pending).as_deref(), Some("third"));
        assert!(pending.is_empty());
        // Nothing queued: a finished turn dispatches nothing.
        assert_eq!(take_queued(true, &mut pending), None);
    }

    #[test]
    fn queued_preview_is_one_line_and_truncated() {
        assert_eq!(queued_preview("short"), "short");
        let long = "x".repeat(200);
        let preview = queued_preview(&long);
        assert!(preview.ends_with('\u{2026}'), "{preview}");
        assert!(preview.chars().count() <= 49, "{}", preview.chars().count());
        // Newlines never reach the placeholder (LF would break the row).
        assert_eq!(queued_preview("a\nb\nc"), "a\u{21aa}b\u{21aa}c");
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
            supports_tools: Some(true),
            supports_text_generation: Some(true),
            input_price_per_mtok: Some(0.4),
            output_price_per_mtok: Some(1.2),
            cache_read_price_per_mtok: None,
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

        let _guard = with_providers(&[("deepinfra", "/usr/local/bin/clanky-provider-deepinfra")]);
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
        assert!(
            providers.contains("clanky-provider-deepinfra"),
            "{providers}"
        );

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

    /// Regression test: the status-line cost must tick up during
    /// streaming, not only when a round's usage report lands (end of
    /// streaming / before a tool call). Live estimates for content no
    /// report covers yet count in at the same rates the ctx % uses.
    #[test]
    fn cost_includes_live_estimates_between_usage_reports() {
        let mut catalog = Catalog::new("deepinfra".into());
        catalog.models = vec![ModelInfo {
            id: "mock/model".into(),
            display_name: None,
            context_window: None,
            supports_thinking: None,
            supports_tools: None,
            supports_text_generation: None,
            input_price_per_mtok: Some(1.0),
            output_price_per_mtok: Some(2.0),
            cache_read_price_per_mtok: None,
        }];
        let mut launch = launch();
        launch.model = Some("mock/model".into());
        let mut app = app::App::new(None);

        // Nothing billed and nothing estimated: no cost, no pricing-less
        // zero either.
        assert_eq!(session_cost(&catalog, &launch, &app), Some(0.0));

        // Stream some completion content: 300 chars -> 9 full steps of
        // 32 chars = 288 chars / 3 chars-per-token = 96 estimated
        // completion tokens.
        app.on_turn_event(TurnEvent::Text {
            delta: "x".repeat(300),
        });
        let (est_p, est_c) = app.estimated_tokens();
        assert_eq!((est_p, est_c), (0, 96));
        // 96 est. completion tokens at $2/Mtok.
        assert_eq!(
            session_cost(&catalog, &launch, &app),
            Some(96.0 * 2.0 / 1e6)
        );

        // A tool result feeds back to the model: estimated prompt side.
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "y".repeat(300),
        });
        let (est_p, est_c) = app.estimated_tokens();
        assert_eq!((est_p, est_c), (96, 96));
        // 96 est. prompt tokens at $1/Mtok + 96 est. completion at $2/Mtok.
        assert_eq!(
            session_cost(&catalog, &launch, &app),
            Some((96.0 * 1.0 + 96.0 * 2.0) / 1e6)
        );

        // The round's usage report covers everything so far: the
        // estimates clear and the cost snaps back to the billed totals.
        app.on_turn_event(TurnEvent::Usage {
            usage: clanky_protocol::Usage {
                prompt_tokens: Some(100),
                completion_tokens: Some(50),
                cached_tokens: Some(100),
            },
        });
        assert_eq!(app.estimated_tokens(), (0, 0));
        // 100 cached prompt tokens at the cache-read rate (falls back to
        // the input rate) + 50 completion at the output rate.
        assert_eq!(
            session_cost(&catalog, &launch, &app),
            Some((100.0 * 1.0 + 50.0 * 2.0) / 1e6)
        );

        // No catalog pricing for the model: no cost shown.
        let empty = Catalog::new("deepinfra".into());
        assert_eq!(session_cost(&empty, &launch, &app), None);
    }

    #[test]
    fn model_detail_combines_fields() {
        assert_eq!(
            model_detail(&ModelInfo {
                id: "m".into(),
                display_name: None,
                context_window: None,
                supports_thinking: None,
                supports_tools: None,
                supports_text_generation: None,
                input_price_per_mtok: None,
                output_price_per_mtok: None,
                cache_read_price_per_mtok: None,
            }),
            ""
        );
        assert_eq!(
            model_detail(&ModelInfo {
                id: "m".into(),
                display_name: Some("Model".into()),
                context_window: Some(4096),
                supports_thinking: Some(true),
                supports_tools: Some(true),
                supports_text_generation: Some(true),
                input_price_per_mtok: Some(0.09),
                output_price_per_mtok: Some(0.18),
                cache_read_price_per_mtok: None,
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
                supports_tools: None,
                supports_text_generation: None,
                input_price_per_mtok: Some(2.0),
                output_price_per_mtok: Some(1.5),
                cache_read_price_per_mtok: None,
            }),
            "$2 · $1.50 /Mtok"
        );
    }

    #[test]
    fn model_picker_filters_out_models_that_cannot_chat_or_call_tools() {
        let model = |id: &str, text: Option<bool>, tools: Option<bool>| ModelInfo {
            id: id.into(),
            display_name: None,
            context_window: None,
            supports_thinking: None,
            supports_tools: tools,
            supports_text_generation: text,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            cache_read_price_per_mtok: None,
        };
        let filtered = chat_capable_models(&[
            model("chat/model", Some(true), Some(true)),
            model("embed/model", Some(false), None),
            model("plain/model", Some(true), Some(false)),
            model("unknown/model", None, None),
        ]);
        let ids: Vec<&str> = filtered.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["chat/model", "unknown/model"]);
    }

    #[test]
    fn set_provider_validates_and_clears_the_model() {
        let _guard = with_providers(&[
            ("deepinfra", "/usr/local/bin/clanky-provider-deepinfra"),
            ("other", "/usr/local/bin/clanky-provider-other"),
        ]);
        let mut launch = launch();
        launch.model = Some("old/model".into());
        set_provider(&mut launch, "other").unwrap();
        assert_eq!(launch.provider, "other");
        // The model is resolved from the new plugin's handshake, so it is
        // cleared rather than carried over.
        assert_eq!(launch.model, None);

        let err = set_provider(&mut launch, "nonexistent").unwrap_err();
        assert!(err.contains("unknown provider `nonexistent`"), "{err}");
        assert_eq!(launch.provider, "other", "failed switch is a no-op");
    }

    #[test]
    fn sampling_edits_apply_and_merge() {
        let mut cfg = launch();
        let mut app = app::App::new(None);

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
            Some(&Value::String("0.7".into()))
        );
        apply_sampling(
            &mut cfg,
            &SamplingEdit::Set(vec![("top_p".into(), "0.9".into())]),
            &mut app,
        );
        assert_eq!(cfg.sampling.as_ref().unwrap().len(), 2, "set merges");

        // No local validation: any key with any value is accepted here —
        // the provider backend validates it at call time.
        apply_sampling(
            &mut cfg,
            &SamplingEdit::Set(vec![("reasoning_effort".into(), "high".into())]),
            &mut app,
        );
        assert!(matches!(app.entries.last(), Some(app::Entry::Info(_))));
        assert_eq!(
            cfg.sampling.as_ref().unwrap().get("reasoning_effort"),
            Some(&Value::String("high".into())),
            "arbitrary backend parameters apply like any other"
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
            Some(&Value::String("0.9".into()))
        );

        apply_sampling(&mut cfg, &SamplingEdit::Clear, &mut app);
        assert!(cfg.sampling.is_none());
    }

    #[test]
    fn thinking_commands_set_and_clear() {
        let mut cfg = launch();
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        let mut app = app::App::new(None);
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
        assert_eq!(
            cfg.thinking.as_deref(),
            Some("off"),
            "off is kept (as an explicit zero budget on the wire)"
        );

        handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Thinking(Some("lots".into())),
            &tx,
        );
        assert!(matches!(app.entries.last(), Some(app::Entry::Error(_))));
        assert_eq!(
            cfg.thinking.as_deref(),
            Some("off"),
            "rejected value is not applied; the previous setting is kept"
        );
    }

    #[test]
    fn model_and_provider_commands_reconfigure_the_launch() {
        let mut cfg = launch();
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        let mut app = app::App::new(None);
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
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        let mut app = app::App::new(None);
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

    /// A `SessionState` with one open session file in its own scratch
    /// directory (tests run in parallel, so nothing may be shared).
    fn session_with_file(tag: &str) -> (SessionState, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "clanky-md-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        let mut writer = recorder::SessionWriter::new(
            path.clone(),
            session::Header {
                version: session::FORMAT_VERSION,
                created: 1_700_000_000_000,
                provider: Some("deepinfra".into()),
                model: Some("mock/model".into()),
                stop_reason: None,
            },
        );
        for record in [
            session::Record::User {
                text: "count files".into(),
            },
            session::Record::Thinking {
                text: "I should run ls.".into(),
            },
            session::Record::Assistant {
                text: "Let me check.".into(),
                calls: vec![],
            },
        ] {
            writer.append(&record).unwrap();
        }
        let mut state = SessionState::with_dir(dir.clone(), "session", "deepinfra", None);
        state.reopen(path);
        (state, dir)
    }

    #[test]
    fn md_export_writes_the_default_selection() {
        let (state, dir) = session_with_file("default");
        let mut app = app::App::new(None);
        let source = ExportSource::snapshot(&state, &app, ExportOptions::default()).unwrap();
        let target = dir.join("out/session.md");
        apply_export(&mut app, target.to_str().unwrap(), &source);

        let doc = std::fs::read_to_string(&target).unwrap();
        assert!(doc.contains("# Clanky session: session"), "{doc}");
        assert!(doc.contains("## User\n\ncount files"), "{doc}");
        assert!(doc.contains("## Assistant\n\nLet me check."), "{doc}");
        assert!(!doc.contains("I should run ls."), "{doc}");
        assert!(matches!(app.entries.last(), Some(app::Entry::Info(_))));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn md_export_all_includes_thinking() {
        let (state, dir) = session_with_file("all");
        let mut app = app::App::new(None);
        let source = ExportSource::snapshot(&state, &app, ExportOptions::ALL).unwrap();
        let target = dir.join("all.md");
        apply_export(&mut app, target.to_str().unwrap(), &source);
        let doc = std::fs::read_to_string(&target).unwrap();
        assert!(doc.contains("## Thinking"), "{doc}");
        assert!(doc.contains("I should run ls."), "{doc}");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn md_export_without_a_session_writes_an_empty_document() {
        let mut state = SessionState::new("deepinfra", None);
        *(state.dir_mut()) = std::env::temp_dir();
        let mut app = app::App::new(None);
        let source = ExportSource::snapshot(&state, &app, ExportOptions::default()).unwrap();
        let target =
            std::env::temp_dir().join(format!("clanky-md-empty-{}.md", std::process::id()));
        apply_export(&mut app, target.to_str().unwrap(), &source);
        let doc = std::fs::read_to_string(&target).unwrap();
        assert!(doc.contains("_Nothing to export"), "{doc}");
        std::fs::remove_file(target).ok();
    }

    #[test]
    fn md_export_reports_write_failures() {
        let (state, dir) = session_with_file("unwritable");
        let mut app = app::App::new(None);
        let source = ExportSource::snapshot(&state, &app, ExportOptions::default()).unwrap();
        // A directory cannot be written as a file.
        apply_export(&mut app, dir.to_str().unwrap(), &source);
        assert!(matches!(app.entries.last(), Some(app::Entry::Error(_))));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn md_command_without_a_filename_opens_the_modal() {
        let mut cfg = launch();
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        let mut app = app::App::new(None);
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());

        let action = handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Export {
                options: ExportOptions::ALL,
                path: None,
            },
            &tx,
        );
        match action {
            Action::OpenExportModal(source) => {
                assert_eq!(source.options, ExportOptions::ALL);
                assert!(source.records.is_empty(), "no session recorded yet");
                assert_eq!(source.header.provider.as_deref(), Some("deepinfra"));
            }
            _ => panic!("expected an export modal"),
        }

        let action = handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Export {
                options: ExportOptions::default(),
                path: Some("out.md".into()),
            },
            &tx,
        );
        assert!(matches!(action, Action::None));
        assert!(matches!(app.entries.last(), Some(app::Entry::Info(_))));
        assert!(std::path::Path::new("out.md").exists(), "file written");
        std::fs::remove_file("out.md").ok();
    }

    #[test]
    fn system_command_pushes_labelled_parts() {
        let mut app = app::App::new(None);
        let parts = vec![
            ("~/.clanky/SYSTEM.md".to_string(), "be terse".to_string()),
            ("AGENTS.md".to_string(), "be helpful".to_string()),
        ];
        push_system_report(&mut app, &parts);
        assert_eq!(app.entries.len(), 3, "summary + one entry per part");
        assert!(matches!(app.entries[0], app::Entry::Info(_)));
        let app::Entry::SystemPart { content } = &app.entries[1] else {
            panic!("expected a SystemPart, got {:?}", app.entries[1]);
        };
        assert_eq!(content, "be terse");
        let app::Entry::SystemPart { content } = &app.entries[2] else {
            panic!("expected a SystemPart, got {:?}", app.entries[2]);
        };
        assert_eq!(content, "be helpful");
    }

    #[test]
    fn system_command_without_context_pushes_a_note() {
        let mut app = app::App::new(None);
        push_system_report(&mut app, &[]);
        assert_eq!(app.entries.len(), 1);
        assert!(matches!(app.entries[0], app::Entry::Info(_)));
    }

    /// A `SessionState` that records into its own scratch directory under
    /// a known file name (tests run in parallel, so nothing may be shared).
    fn recording_state(tag: &str) -> (SessionState, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "clanky-flush-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        (
            SessionState::with_dir(dir.clone(), "session", "deepinfra", None),
            dir,
        )
    }

    #[test]
    fn quitting_mid_stream_consolidates_and_writes_the_partial_round() {
        // Ctrl+C cut the stream before any `Round` arrived: the flush
        // consolidates the accumulated deltas into the same records a
        // completed round writes, thinking first, so nothing shown on
        // screen is lost and `/resume` restores the conversation.
        let (mut state, dir) = recording_state("quit");
        for event in [
            TurnEvent::Thinking {
                delta: "let me ".into(),
            },
            TurnEvent::Thinking {
                delta: "think".into(),
            },
            TurnEvent::Text {
                delta: "The answer".into(),
            },
        ] {
            recorder::record_turn_event(&mut state, &mut NoErrors, &event);
        }
        state.flush_partial(&mut NoErrors);
        let data = session::load(&dir.join("session.jsonl")).unwrap();
        assert_eq!(
            data.records,
            vec![
                session::Record::Thinking {
                    text: "let me think".into()
                },
                session::Record::Assistant {
                    text: "The answer".into(),
                    calls: vec![]
                },
            ]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_completed_round_is_not_written_twice() {
        // The round's records and the accumulated deltas describe the
        // same content; flushing after a `Round` event must be a no-op.
        let (mut state, dir) = recording_state("round");
        recorder::record_turn_event(
            &mut state,
            &mut NoErrors,
            &TurnEvent::Text {
                delta: "done".into(),
            },
        );
        recorder::record_turn_event(
            &mut state,
            &mut NoErrors,
            &TurnEvent::Round {
                text: "done".into(),
                thinking: String::new(),
                calls: vec![],
            },
        );
        state.flush_partial(&mut NoErrors);
        let data = session::load(&dir.join("session.jsonl")).unwrap();
        assert_eq!(
            data.records,
            vec![session::Record::Assistant {
                text: "done".into(),
                calls: vec![]
            }]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_turn_error_keeps_the_partial_round_before_the_error_record() {
        let (mut state, dir) = recording_state("error");
        recorder::record_turn_event(
            &mut state,
            &mut NoErrors,
            &TurnEvent::Text {
                delta: "partial".into(),
            },
        );
        state.record_done(
            &mut NoErrors,
            &Err(clanky_protocol::Error::provider(
                clanky_protocol::ErrorCode::Backend,
                "timed out reading response",
                true,
            )
            .into()),
        );
        let data = session::load(&dir.join("session.jsonl")).unwrap();
        assert_eq!(
            data.records,
            vec![
                session::Record::Assistant {
                    text: "partial".into(),
                    calls: vec![]
                },
                session::Record::Error {
                    message: "provider error: backend: timed out reading response".into()
                },
            ]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_completed_turn_appends_the_stop_reason_record() {
        // The terminal `done` of a successful turn becomes a `stopReason`
        // record — the turn's last record, after usage.
        let (mut state, dir) = recording_state("stop-reason");
        state.record_done(
            &mut NoErrors,
            &Ok(TurnOutput {
                text: "done".into(),
                finish_reason: clanky_protocol::FinishReason::Stop,
                usage: None,
            }),
        );
        let data = session::load(&dir.join("session.jsonl")).unwrap();
        assert_eq!(
            data.records,
            vec![session::Record::StopReason {
                reason: "stop".into()
            }]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn draining_before_the_quit_records_completed_rounds_too() {
        // What the worker already sent is recorded exactly as the live
        // loop would: a round that completed just before ctrl+c is kept,
        // deltas after it still flush, and the turn's `Done` is dropped.
        let (mut state, dir) = recording_state("drain");
        let (tx, rx) = mpsc::channel::<WorkerEvent>();
        tx.send(WorkerEvent::Turn(TurnEvent::Round {
            text: "first".into(),
            thinking: String::new(),
            calls: vec![],
        }))
        .unwrap();
        tx.send(WorkerEvent::Turn(TurnEvent::Text {
            delta: "second".into(),
        }))
        .unwrap();
        tx.send(WorkerEvent::Done(TurnEnd {
            result: Err(clanky_protocol::Error::provider(
                clanky_protocol::ErrorCode::Internal,
                "never displayed",
                false,
            )
            .into()),
            history: Vec::new(),
            session: None,
        }))
        .unwrap();
        drain_turn_events(&rx, &mut state, &mut NoErrors);
        state.flush_partial(&mut NoErrors);
        let data = session::load(&dir.join("session.jsonl")).unwrap();
        assert_eq!(
            data.records,
            vec![
                session::Record::Assistant {
                    text: "first".into(),
                    calls: vec![]
                },
                session::Record::Assistant {
                    text: "second".into(),
                    calls: vec![]
                },
            ],
            "the un-displayed error must not reach the resumed transcript"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn name_and_resume_report_politely_when_recording_is_off() {
        let mut cfg = launch();
        let mut state = SessionState::new(&cfg.provider, cfg.model.as_deref()).recording(false);
        let mut app = app::App::new(None);
        let (tx, _rx) = mpsc::channel::<WorkerEvent>();
        let mut catalog = Catalog::new("deepinfra".into());

        let action = handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Name(Some("renamed".into())),
            &tx,
        );
        assert!(matches!(action, Action::None));
        assert!(
            matches!(app.entries.last(), Some(app::Entry::Info(_))),
            "/name must report politely: {:?}",
            app.entries.last()
        );
        assert!(
            app.entries
                .last()
                .map(|e| match e {
                    app::Entry::Info(t) => t.clone(),
                    other => format!("{other:?}"),
                })
                .unwrap_or_default()
                .contains("--no-session"),
            "the note must say why: {:?}",
            app.entries.last()
        );

        let action = handle_command(
            &mut app,
            &mut state,
            &mut cfg,
            &mut catalog,
            Command::Resume,
            &tx,
        );
        assert!(matches!(action, Action::None), "no picker may open");
        assert!(matches!(app.entries.last(), Some(app::Entry::Info(_))));
    }

    #[test]
    fn status_session_name_shows_no_session_when_recording_is_off() {
        let cfg = launch();
        let state = SessionState::new(&cfg.provider, cfg.model.as_deref()).recording(false);
        assert_eq!(status_session_name(&state).as_deref(), Some("(no session)"));
        // Recording on keeps the existing display: the name once the file
        // exists, nothing before it.
        let state = SessionState::new(&cfg.provider, cfg.model.as_deref());
        assert_eq!(status_session_name(&state), None);
        let dir = std::env::temp_dir().join(format!("clanky-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = SessionState::with_dir(dir.clone(), "named", "deepinfra", None);
        state.record(&mut NoErrors, session::Record::User { text: "hi".into() });
        assert_eq!(status_session_name(&state).as_deref(), Some("named"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn md_export_without_a_file_uses_screen_records() {
        // With recording off the screen shows a conversation no file
        // ever held: `/md` must export what the screen showed.
        let cfg = launch();
        let state = SessionState::new(&cfg.provider, cfg.model.as_deref()).recording(false);
        let mut app = app::App::new(None);
        app.push_user("count files");
        app.on_turn_event(TurnEvent::ToolCall {
            name: "bash".into(),
            arguments: "{\"cmd\":\"ls\"}".into(),
        });
        app.on_turn_event(TurnEvent::ToolResult {
            name: "bash".into(),
            output: "3 files".into(),
        });
        app.on_turn_event(TurnEvent::Text {
            delta: "There are 3".into(),
        });
        let source = ExportSource::snapshot(&state, &app, ExportOptions::ALL).unwrap();
        assert!(source.name.is_none(), "no session file, no name");
        let doc = {
            let target =
                std::env::temp_dir().join(format!("clanky-md-screen-{}.md", std::process::id()));
            let count = source.write(target.to_str().unwrap()).unwrap();
            assert_eq!(count, 4, "user, assistant(calls), tool result, assistant");
            let doc = std::fs::read_to_string(&target).unwrap();
            std::fs::remove_file(target).unwrap();
            doc
        };
        assert!(doc.contains("## User\n\ncount files"), "{doc}");
        assert!(doc.contains("There are 3"), "{doc}");
        assert!(doc.contains("### Tool call: `bash`"), "{doc}");
        assert!(doc.contains("### Tool result: `bash`"), "{doc}");
    }
}
