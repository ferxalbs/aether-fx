#![deny(unsafe_code)]

//! Stateful, provider-neutral terminal UI for AETHER Fx.

use std::{
    collections::{HashMap, VecDeque},
    env,
    io::{self, Stdout},
    time::Instant,
};

use aether_core::{
    AgentEvent, Appointment, AppointmentDraft, AppointmentId, BoundedText, PermissionDecision,
    PermissionRequest, SessionId, ToolCallId, TranscriptCell, TurnId, sanitize_transcript,
};
use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event as CrosstermEvent, EventStream, KeyCode,
        KeyEvent, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{
        Clear, ClearType, DisableLineWrap, EnableLineWrap, EnterAlternateScreen,
        LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    },
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear as ClearWidget, List, ListItem, ListState, Paragraph, Wrap},
};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_COMMAND_QUERY_BYTES: usize = 256;
const MAX_COMPOSER_BYTES: usize = 16 * 1024;
const MAX_HISTORY_ITEMS: usize = 64;
const MAX_QUEUED_PROMPTS: usize = 8;
const MAX_LIVE_TOOL_OUTPUT_BYTES: usize = 8 * 1024;
const MAX_NOTICE_BYTES: usize = 1024;
const INLINE_VIEWPORT_HEIGHT: u16 = 24;

/// Configuration displayed in the TUI header.
#[derive(Clone, Debug, Default)]
pub struct TuiConfig {
    /// AETHER release version.
    pub version: String,
    /// Canonical workspace path.
    pub workspace: String,
    /// Selected model identifier, if one is active.
    pub model: Option<String>,
    /// Active reasoning mode/capability label, if reported by the model catalog.
    pub reasoning: Option<String>,
    /// Active session identifier, if a session has started.
    pub session_id: Option<SessionId>,
    /// Whether tool calls skip interactive permission prompts.
    pub yolo: bool,
    /// Whether the state came from a resumed session.
    pub resumed: bool,
    /// Optional bounded status shown before the first interaction.
    pub initial_status: Option<String>,
}

/// A bounded item shown in a model or session picker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PickerItem {
    /// Opaque value returned when selected.
    pub value: String,
    /// Primary display label.
    pub label: String,
    /// Optional secondary display detail.
    pub detail: Option<String>,
    /// Whether the item is visible but cannot be selected.
    pub unavailable: bool,
}

impl PickerItem {
    /// Construct a selectable picker item.
    pub fn new(value: impl Into<String>, label: impl Into<String>, detail: Option<String>) -> Self {
        Self { value: value.into(), label: label.into(), detail, unavailable: false }
    }
}

/// Events consumed by the stateful UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiEvent {
    /// A keyboard event from crossterm.
    Key(KeyEvent),
    /// A bracketed paste payload.
    Paste(String),
    /// A terminal resize notification.
    Resize(u16, u16),
    /// The terminal regained focus.
    FocusGained,
    /// The terminal lost focus.
    FocusLost,
    /// A local redraw/timer tick.
    Timer,
}

impl TuiEvent {
    fn from_crossterm(event: CrosstermEvent) -> Option<Self> {
        match event {
            CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => Some(Self::Key(key)),
            CrosstermEvent::Paste(value) => Some(Self::Paste(value)),
            CrosstermEvent::Resize(width, height) => Some(Self::Resize(width, height)),
            CrosstermEvent::FocusGained => Some(Self::FocusGained),
            CrosstermEvent::FocusLost => Some(Self::FocusLost),
            CrosstermEvent::Key(_) | CrosstermEvent::Mouse(_) => None,
        }
    }
}

/// Effects emitted by the UI and handled by the application runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TuiAction {
    /// Submit one prompt to the existing agent runtime.
    SubmitPrompt(String),
    /// Execute an existing local slash command.
    Command(String),
    /// Resolve one existing permission request.
    Permission { call_id: ToolCallId, decision: PermissionDecision },
    /// Cancel the active agent turn.
    CancelTurn,
    /// Choose a model from the existing catalog.
    SelectModel(String),
    /// Resume a session from the existing session store.
    SelectSession(String),
    /// Create an appointment through the application-owned adapter.
    Schedule(AppointmentDraft),
    /// Finish the interactive session.
    Quit,
}

#[derive(Clone, Debug)]
enum Overlay {
    None,
    Commands { query: String, selected: usize },
    Models { items: Vec<PickerItem>, selected: usize },
    Sessions { items: Vec<PickerItem>, selected: usize },
    Help,
    Transcript { scroll: u16 },
    Approval { request: PermissionRequest, selected: usize },
    ScheduleForm(ScheduleForm),
    ScheduleReview { draft: AppointmentDraft, preview: Box<Appointment> },
    SchedulePending,
}

#[derive(Clone, Debug)]
struct ScheduleForm {
    fields: Vec<String>,
    selected: usize,
    error: Option<(usize, String)>,
}

impl ScheduleForm {
    fn new() -> Self {
        Self {
            fields: vec![
                String::new(),
                String::new(),
                String::new(),
                default_utc_offset(),
                "30".to_owned(),
                String::new(),
                String::new(),
                String::new(),
            ],
            selected: 0,
            error: None,
        }
    }

    fn labels() -> [&'static str; 8] {
        [
            "Title",
            "Date (YYYY-MM-DD)",
            "Time (HH:MM)",
            "UTC offset (+HH:MM)",
            "Duration (minutes)",
            "Location (optional)",
            "Notes (optional)",
            "Attendees (comma-separated, optional)",
        ]
    }

    fn draft(&self) -> AppointmentDraft {
        AppointmentDraft {
            title: self.fields[0].clone(),
            date: self.fields[1].clone(),
            time: self.fields[2].clone(),
            utc_offset: self.fields[3].clone(),
            duration_minutes: self.fields[4].clone(),
            location: (!self.fields[5].trim().is_empty()).then(|| self.fields[5].clone()),
            notes: (!self.fields[6].trim().is_empty()).then(|| self.fields[6].clone()),
            attendees: self.fields[7]
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
        }
    }

    fn preview(&self) -> Result<(AppointmentDraft, Appointment), String> {
        let draft = self.draft();
        let id = AppointmentId::new("appointment-preview").map_err(|error| error.to_string())?;
        let preview =
            draft.normalize(id, OffsetDateTime::now_utc()).map_err(|error| error.to_string())?;
        Ok((draft, preview))
    }
}

#[derive(Clone, Debug)]
enum LiveCell {
    User(String),
    Assistant(String),
    Tool { name: String, output: String, ok: Option<bool> },
    Warning(String),
    Error(String),
    TurnSummary { turn_id: TurnId, summary: String },
    Appointment(Appointment),
}

/// Stateful application model rendered by ratatui.
#[derive(Clone, Debug)]
pub struct TuiState {
    config: TuiConfig,
    cells: Vec<LiveCell>,
    overlay: Overlay,
    composer: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    queued: VecDeque<String>,
    tool_indices: HashMap<ToolCallId, usize>,
    current_assistant: Option<usize>,
    busy: bool,
    turn_started: Option<Instant>,
    working_detail: Option<String>,
    footer_note: Option<String>,
    focused: bool,
    scroll: u16,
    terminal_width: u16,
}

impl TuiState {
    /// Construct a fresh state model.
    pub fn new(config: TuiConfig) -> Self {
        let footer_note = config
            .initial_status
            .clone()
            .or_else(|| config.resumed.then(|| "Resuming session…".to_owned()));
        Self {
            config,
            cells: Vec::new(),
            overlay: Overlay::None,
            composer: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            queued: VecDeque::new(),
            tool_indices: HashMap::new(),
            current_assistant: None,
            busy: false,
            turn_started: None,
            working_detail: None,
            footer_note,
            focused: true,
            scroll: 0,
            terminal_width: 80,
        }
    }

    /// Return whether the agent currently owns an active turn.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Return whether a picker, modal, or transcript overlay is active.
    pub fn has_overlay(&self) -> bool {
        !matches!(self.overlay, Overlay::None)
    }

    /// Return the number of queued prompts.
    pub fn queued_prompt_count(&self) -> usize {
        self.queued.len()
    }

    /// Return the next queued prompt after a turn finishes.
    pub fn pop_queued_prompt(&mut self) -> Option<String> {
        self.queued.pop_front()
    }

    /// Return safe semantic cells suitable for session persistence.
    pub fn persistable_transcript(&self) -> Vec<TranscriptCell> {
        sanitize_transcript(self.cells.iter().filter_map(LiveCell::persistable))
    }

    /// Replace the live transcript with a restored safe transcript.
    pub fn restore_transcript(&mut self, cells: &[TranscriptCell]) {
        self.cells = sanitize_transcript(cells.iter().cloned())
            .iter()
            .filter_map(LiveCell::from_persisted)
            .collect();
        self.current_assistant = None;
        self.scroll = u16::MAX;
    }

    /// Update the header model label.
    pub fn set_model(&mut self, model: Option<String>) {
        self.config.model = model;
    }

    /// Update the header reasoning label.
    pub fn set_reasoning(&mut self, reasoning: Option<String>) {
        self.config.reasoning = reasoning;
    }

    /// Update the active session identity.
    pub fn set_session(&mut self, session_id: Option<SessionId>) {
        self.config.session_id = session_id;
    }

    /// Put an invocation prompt into the composer when model selection must happen first.
    pub fn set_composer(&mut self, value: impl Into<String>) {
        self.composer.clear();
        append_bounded(&mut self.composer, &value.into(), MAX_COMPOSER_BYTES);
        self.cursor = self.composer.len();
    }

    /// Reset the visible session transcript while preserving the selected model.
    pub fn reset_session(&mut self) {
        self.cells.clear();
        self.overlay = Overlay::None;
        self.composer.clear();
        self.cursor = 0;
        self.history_index = None;
        self.queued.clear();
        self.tool_indices.clear();
        self.current_assistant = None;
        self.busy = false;
        self.turn_started = None;
        self.working_detail = None;
        self.footer_note = Some("Started a fresh local session".to_owned());
        self.scroll = 0;
        self.config.session_id = None;
    }

    /// Add a bounded notice to the transcript.
    pub fn push_notice(&mut self, message: impl AsRef<str>) {
        let message = bound_notice(message.as_ref());
        self.cells.push(LiveCell::Warning(message));
        self.scroll = u16::MAX;
    }

    /// Mark a new submitted turn and add its user cell.
    pub fn begin_turn(&mut self, prompt: String) {
        let prompt = bounded_text(&prompt, aether_core::MAX_TRANSCRIPT_TEXT_BYTES);
        if prompt.trim().is_empty() {
            return;
        }
        self.history.push(prompt.clone());
        if self.history.len() > MAX_HISTORY_ITEMS {
            self.history.remove(0);
        }
        self.history_index = None;
        self.cells.push(LiveCell::User(prompt));
        self.current_assistant = None;
        self.tool_indices.clear();
        self.busy = true;
        self.turn_started = Some(Instant::now());
        self.working_detail = None;
        self.footer_note = None;
        self.scroll = u16::MAX;
    }

    /// Mark the active turn complete and add its compact summary.
    pub fn finish_turn(&mut self, turn_id: TurnId, summary: impl AsRef<str>) {
        self.cells.push(LiveCell::TurnSummary { turn_id, summary: bound_notice(summary.as_ref()) });
        self.busy = false;
        self.turn_started = None;
        self.current_assistant = None;
        self.working_detail = None;
        self.footer_note = None;
        self.scroll = u16::MAX;
    }

    /// Mark the active turn cancelled without destroying the transcript.
    pub fn cancel_turn(&mut self) {
        self.busy = false;
        self.turn_started = None;
        self.current_assistant = None;
        self.working_detail = None;
        self.footer_note = Some("Turn cancelled".to_owned());
        self.scroll = u16::MAX;
    }

    /// Display a successful appointment confirmation.
    pub fn appointment_created(&mut self, appointment: Appointment) {
        self.overlay = Overlay::None;
        self.cells.push(LiveCell::Appointment(appointment));
        self.footer_note = Some("Appointment saved locally".to_owned());
        self.scroll = u16::MAX;
    }

    /// Keep the draft visible while a local appointment write fails.
    pub fn appointment_failed(&mut self, draft: AppointmentDraft, message: impl AsRef<str>) {
        let preview_id = AppointmentId::new("appointment-preview")
            .expect("appointment preview id is a valid local identifier");
        if let Ok(preview) = draft.normalize(preview_id, OffsetDateTime::now_utc()) {
            self.overlay = Overlay::ScheduleReview { draft, preview: Box::new(preview) };
        } else {
            self.overlay = Overlay::ScheduleForm(ScheduleForm {
                fields: form_fields_from_draft(&draft),
                selected: 0,
                error: Some((0, "The appointment draft needs correction".to_owned())),
            });
        }
        self.push_notice(message);
        self.footer_note = Some("Appointment was not saved".to_owned());
    }

    /// Open the model picker with bounded catalog items.
    pub fn open_model_picker(&mut self, items: Vec<PickerItem>) {
        let mut selected = self
            .config
            .model
            .as_deref()
            .and_then(|model| items.iter().position(|item| item.value == model))
            .unwrap_or(0);
        normalize_selection(&mut selected, &items);
        self.overlay = Overlay::Models { items, selected };
    }

    /// Open the session picker with bounded session items.
    pub fn open_session_picker(&mut self, items: Vec<PickerItem>) {
        let mut selected = self
            .config
            .session_id
            .as_ref()
            .and_then(|session| items.iter().position(|item| item.value == session.as_str()))
            .unwrap_or(0);
        normalize_selection(&mut selected, &items);
        self.overlay = Overlay::Sessions { items, selected };
    }

    /// Open the appointment form.
    pub fn open_schedule(&mut self) {
        self.overlay = Overlay::ScheduleForm(ScheduleForm::new());
    }

    /// Open the local command palette.
    pub fn open_commands(&mut self) {
        self.overlay = Overlay::Commands { query: String::new(), selected: 0 };
    }

    /// Open the keyboard shortcut overlay.
    pub fn open_help(&mut self) {
        self.overlay = Overlay::Help;
    }

    /// Open the transcript overlay.
    pub fn open_transcript(&mut self) {
        self.overlay = Overlay::Transcript { scroll: self.scroll };
    }

    /// Apply one agent event to the live transcript and status area.
    pub fn apply_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TextDelta { text } => {
                let index = if let Some(index) = self.current_assistant {
                    index
                } else {
                    self.cells.push(LiveCell::Assistant(String::new()));
                    let index = self.cells.len().saturating_sub(1);
                    self.current_assistant = Some(index);
                    index
                };
                if let Some(LiveCell::Assistant(value)) = self.cells.get_mut(index) {
                    append_bounded(value, text.as_str(), aether_core::MAX_CONTEXT_RENDER_BYTES);
                }
            }
            AgentEvent::ToolStarted { call_id, name, operation, .. } => {
                self.cells.push(LiveCell::Tool {
                    name: sanitize_display(name),
                    output: sanitize_display(operation),
                    ok: None,
                });
                let index = self.cells.len().saturating_sub(1);
                self.tool_indices.insert(call_id.clone(), index);
                self.working_detail = Some(sanitize_display(operation));
                self.current_assistant = None;
            }
            AgentEvent::ToolOutput { call_id, output } => {
                if let Some(index) = self.tool_indices.get(call_id).copied()
                    && let Some(LiveCell::Tool { output: detail, .. }) = self.cells.get_mut(index)
                {
                    append_bounded(detail, "\n", MAX_LIVE_TOOL_OUTPUT_BYTES);
                    append_bounded(detail, output.as_str(), MAX_LIVE_TOOL_OUTPUT_BYTES);
                }
            }
            AgentEvent::ToolFinished { call_id, ok } => {
                if let Some(index) = self.tool_indices.get(call_id).copied()
                    && let Some(LiveCell::Tool { ok: status, .. }) = self.cells.get_mut(index)
                {
                    *status = Some(*ok);
                }
                self.working_detail = None;
            }
            AgentEvent::PermissionRequested { request } => {
                self.overlay = Overlay::Approval { request: request.clone(), selected: 0 };
            }
            AgentEvent::PermissionResolved { .. } => {
                if matches!(self.overlay, Overlay::Approval { .. }) {
                    self.overlay = Overlay::None;
                }
            }
            AgentEvent::Usage { .. } => {}
            AgentEvent::Warning { message } => {
                self.cells.push(LiveCell::Warning(sanitize_display(message.as_str())));
            }
            AgentEvent::Error { message } => {
                self.cells.push(LiveCell::Error(sanitize_display(message.as_str())));
            }
            AgentEvent::Done => {
                self.working_detail = None;
            }
        }
        self.scroll = u16::MAX;
    }

    /// Convert a UI event into an application action, if one is ready.
    pub fn handle_event(&mut self, event: TuiEvent) -> Option<TuiAction> {
        match event {
            TuiEvent::Key(key) => self.handle_key(key),
            TuiEvent::Paste(value) => {
                if let Overlay::ScheduleForm(form) = &mut self.overlay {
                    let selected = form.selected;
                    append_bounded(&mut form.fields[selected], &value, field_limit(selected));
                    form.error = None;
                } else {
                    self.insert_text(&value);
                }
                None
            }
            TuiEvent::Resize(width, _) => {
                self.terminal_width = width.max(1);
                None
            }
            TuiEvent::FocusGained => {
                self.focused = true;
                None
            }
            TuiEvent::FocusLost => {
                self.focused = false;
                None
            }
            TuiEvent::Timer => None,
        }
    }

    /// Render the current state into a ratatui frame.
    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let composer_height = self.composer_height(area.width);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Min(1),
                Constraint::Length(composer_height),
                Constraint::Length(1),
            ])
            .split(area);
        self.render_header(frame, chunks[0]);
        self.render_transcript(frame, chunks[1]);
        self.render_composer(frame, chunks[2]);
        self.render_footer(frame, chunks[3]);
        self.render_overlay(frame, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<TuiAction> {
        if !self.focused {
            return None;
        }
        if !matches!(self.overlay, Overlay::None) {
            return self.handle_overlay_key(key);
        }
        if self.busy {
            if key.code == KeyCode::Esc || is_ctrl(key, 'c') {
                return Some(TuiAction::CancelTurn);
            }
            if key.code == KeyCode::Enter {
                let prompt = self.composer.trim().to_owned();
                if !prompt.is_empty() {
                    if self.queued.len() < MAX_QUEUED_PROMPTS {
                        self.queued.push_back(prompt);
                        self.composer.clear();
                        self.cursor = 0;
                        self.footer_note = Some(format!("{} prompt(s) queued", self.queued.len()));
                    } else {
                        self.footer_note = Some("Prompt queue is full".to_owned());
                    }
                }
                return None;
            }
        }
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.composer.clear();
                self.cursor = 0;
            }
            KeyCode::Char('/') if self.composer.is_empty() => {
                self.open_commands();
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_text("\n");
            }
            KeyCode::Char('?') if self.composer.is_empty() => self.open_help(),
            KeyCode::Char(character) => self.insert_text(&character.to_string()),
            KeyCode::Enter => {
                let prompt = self.composer.trim().to_owned();
                if !prompt.is_empty() {
                    self.composer.clear();
                    self.cursor = 0;
                    return Some(if prompt.starts_with('/') {
                        TuiAction::Command(prompt)
                    } else {
                        TuiAction::SubmitPrompt(prompt)
                    });
                }
            }
            KeyCode::Backspace => self.delete_previous_grapheme(),
            KeyCode::Delete => self.delete_next_grapheme(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.composer.len(),
            KeyCode::Up => self.history_previous(),
            KeyCode::Down => self.history_next(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(8),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(8),
            KeyCode::F(1) => self.open_help(),
            KeyCode::F(2) => self.open_transcript(),
            KeyCode::Esc => {
                self.composer.clear();
                self.cursor = 0;
            }
            KeyCode::Tab => self.open_commands(),
            _ => {}
        }
        None
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Option<TuiAction> {
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::Commands { mut query, mut selected } => {
                let items = command_items(&query);
                let action = handle_picker_key(key, &mut selected, items.len())
                    .unwrap_or(PickerAction::Move);
                if action == PickerAction::Cancel {
                    return None;
                }
                if action == PickerAction::Confirm
                    && let Some(item) = items.get(selected)
                {
                    self.overlay = Overlay::None;
                    return Some(TuiAction::Command(item.0.to_owned()));
                }
                match key.code {
                    KeyCode::Backspace => {
                        if let Some((index, _)) = query.grapheme_indices(true).next_back() {
                            query.truncate(index);
                        }
                        selected = 0;
                    }
                    KeyCode::Char(character)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT)
                            && query.len() < MAX_COMMAND_QUERY_BYTES =>
                    {
                        query.push(character);
                        selected = 0;
                    }
                    _ => {}
                }
                self.overlay = Overlay::Commands { query, selected };
                None
            }
            Overlay::Models { items, mut selected } => {
                let action = handle_picker_items_key(key, &mut selected, &items)
                    .unwrap_or(PickerAction::Move);
                if action == PickerAction::Cancel {
                    return None;
                }
                if action == PickerAction::Confirm
                    && let Some(item) = items.get(selected)
                    && !item.unavailable
                {
                    self.overlay = Overlay::None;
                    return Some(TuiAction::SelectModel(item.value.clone()));
                }
                normalize_selection(&mut selected, &items);
                self.overlay = Overlay::Models { items, selected };
                None
            }
            Overlay::Sessions { items, mut selected } => {
                let action = handle_picker_items_key(key, &mut selected, &items)
                    .unwrap_or(PickerAction::Move);
                if action == PickerAction::Cancel {
                    return None;
                }
                if action == PickerAction::Confirm
                    && let Some(item) = items.get(selected)
                    && !item.unavailable
                {
                    self.overlay = Overlay::None;
                    return Some(TuiAction::SelectSession(item.value.clone()));
                }
                normalize_selection(&mut selected, &items);
                self.overlay = Overlay::Sessions { items, selected };
                None
            }
            Overlay::Help => {
                if key.code != KeyCode::Esc && key.code != KeyCode::Char('q') {
                    self.overlay = Overlay::Help;
                }
                None
            }
            Overlay::Transcript { mut scroll } => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => {}
                    KeyCode::Up | KeyCode::Char('k' | 'K') => scroll = scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j' | 'J') => scroll = scroll.saturating_add(1),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(8),
                    KeyCode::PageDown => scroll = scroll.saturating_add(8),
                    KeyCode::Home => scroll = 0,
                    KeyCode::End => scroll = u16::MAX,
                    _ => self.overlay = Overlay::Transcript { scroll },
                }
                if !matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.overlay = Overlay::Transcript { scroll };
                }
                None
            }
            Overlay::Approval { request, mut selected } => {
                let options = approval_options(&request);
                let decision = if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    Some(PermissionDecision::AllowOnce)
                } else if matches!(key.code, KeyCode::Char('s' | 'S'))
                    && request.class != aether_core::PermissionClass::BrowserAction
                {
                    Some(PermissionDecision::AllowSession)
                } else if matches!(key.code, KeyCode::Char('n' | 'N')) {
                    Some(PermissionDecision::Deny)
                } else {
                    None
                };
                if let Some(decision) = decision {
                    return Some(TuiAction::Permission { call_id: request.call_id, decision });
                }
                let action = handle_picker_key(key, &mut selected, options.len())
                    .unwrap_or(PickerAction::Move);
                if action == PickerAction::Cancel {
                    return Some(TuiAction::CancelTurn);
                }
                if action == PickerAction::Confirm {
                    let decision = match selected {
                        0 => PermissionDecision::AllowOnce,
                        1 if request.class != aether_core::PermissionClass::BrowserAction => {
                            PermissionDecision::AllowSession
                        }
                        _ => PermissionDecision::Deny,
                    };
                    return Some(TuiAction::Permission { call_id: request.call_id, decision });
                }
                self.overlay = Overlay::Approval { request, selected };
                None
            }
            Overlay::ScheduleForm(mut form) => {
                if key.code == KeyCode::Esc {
                    return None;
                }
                if key.code == KeyCode::BackTab
                    || (key.modifiers.contains(KeyModifiers::SHIFT) && key.code == KeyCode::Tab)
                    || key.code == KeyCode::Up
                {
                    form.selected = if form.selected == 0 {
                        form.fields.len().saturating_sub(1)
                    } else {
                        form.selected - 1
                    };
                    form.error = None;
                } else if key.code == KeyCode::Tab
                    || key.code == KeyCode::Down
                    || (key.code == KeyCode::Enter
                        && form.selected < form.fields.len().saturating_sub(1))
                {
                    form.selected = (form.selected + 1) % form.fields.len();
                    form.error = None;
                } else if key.code == KeyCode::Enter {
                    match form.preview() {
                        Ok((draft, preview)) => {
                            self.overlay =
                                Overlay::ScheduleReview { draft, preview: Box::new(preview) };
                        }
                        Err(error) => {
                            let field = validation_field(&error);
                            form.selected = field;
                            form.error = Some((field, bound_notice(&error)));
                            self.overlay = Overlay::ScheduleForm(form);
                        }
                    }
                    return None;
                } else {
                    Self::handle_form_edit(&mut form, key);
                }
                if matches!(self.overlay, Overlay::None) {
                    self.overlay = Overlay::ScheduleForm(form);
                }
                None
            }
            Overlay::ScheduleReview { draft, preview } => {
                if key.code == KeyCode::Esc {
                    let mut form = ScheduleForm::new();
                    form.fields = form_fields_from_draft(&draft);
                    self.overlay = Overlay::ScheduleForm(form);
                    return None;
                }
                if key.code == KeyCode::Enter {
                    self.overlay = Overlay::SchedulePending;
                    return Some(TuiAction::Schedule(draft));
                }
                self.overlay = Overlay::ScheduleReview { draft, preview };
                None
            }
            Overlay::SchedulePending => {
                self.overlay = Overlay::SchedulePending;
                None
            }
            Overlay::None => None,
        }
    }

    fn handle_form_edit(form: &mut ScheduleForm, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('j') {
            let selected = form.selected;
            if field_limit(selected) > form.fields[selected].len() {
                form.fields[selected].push('\n');
            }
            return;
        }
        match key.code {
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let selected = form.selected;
                if form.fields[selected].len().saturating_add(character.len_utf8())
                    <= field_limit(selected)
                {
                    form.fields[selected].push(character);
                    form.error = None;
                }
            }
            KeyCode::Backspace => {
                let selected = form.selected;
                if let Some((index, _)) = form.fields[selected].grapheme_indices(true).next_back() {
                    form.fields[selected].truncate(index);
                    form.error = None;
                }
            }
            KeyCode::Delete => {
                form.fields[form.selected].clear();
                form.error = None;
            }
            _ => {}
        }
    }

    fn insert_text(&mut self, value: &str) {
        if self.composer.len().saturating_add(value.len()) > MAX_COMPOSER_BYTES {
            self.footer_note = Some("Prompt exceeds the 16 KiB input limit".to_owned());
            return;
        }
        self.composer.insert_str(self.cursor, value);
        self.cursor = self.cursor.saturating_add(value.len());
    }

    fn delete_previous_grapheme(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if let Some((index, _)) = self.composer[..self.cursor].grapheme_indices(true).next_back() {
            self.composer.drain(index..self.cursor);
            self.cursor = index;
        }
    }

    fn delete_next_grapheme(&mut self) {
        if self.cursor >= self.composer.len() {
            return;
        }
        if let Some((_, value)) = self.composer[self.cursor..].grapheme_indices(true).next() {
            let end = self.cursor.saturating_add(value.len());
            self.composer.drain(self.cursor..end);
        }
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.composer[..self.cursor]
                .grapheme_indices(true)
                .next_back()
                .map_or(0, |(index, _)| index);
        }
    }

    fn move_right(&mut self) {
        if self.cursor < self.composer.len() {
            self.cursor = self.composer[self.cursor..]
                .grapheme_indices(true)
                .next()
                .map_or(self.composer.len(), |(_, value)| self.cursor + value.len());
        }
    }

    fn history_previous(&mut self) {
        if self.history.is_empty() || self.busy {
            return;
        }
        let index = self.history_index.unwrap_or(self.history.len());
        let next = index.saturating_sub(1);
        self.history_index = Some(next);
        self.composer = self.history[next].clone();
        self.cursor = self.composer.len();
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_index else { return };
        if index + 1 >= self.history.len() {
            self.history_index = None;
            self.composer.clear();
            self.cursor = 0;
        } else {
            self.history_index = Some(index + 1);
            self.composer = self.history[index + 1].clone();
            self.cursor = self.composer.len();
        }
    }

    fn composer_height(&self, width: u16) -> u16 {
        let inner_width = width.saturating_sub(4).max(1) as usize;
        let logical_lines = self.composer.split('\n').map(|line| {
            let cells = unicode_width::UnicodeWidthStr::width(line).max(1);
            cells.div_ceil(inner_width).max(1)
        });
        let lines = logical_lines.sum::<usize>().saturating_add(2);
        lines.clamp(3, 8) as u16
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let model = self.config.model.as_deref().unwrap_or("not selected");
        let reasoning = self.config.reasoning.as_deref().unwrap_or("default");
        let permission = if self.config.yolo { "yolo" } else { "prompt" };
        let session = self.config.session_id.as_ref().map_or("new", SessionId::as_str);
        let lines = vec![
            Line::styled(
                format!(">_ AETHER Fx (v{})", display_text(&self.config.version)),
                primary_style(),
            ),
            Line::from(vec![
                Span::styled("model:     ", secondary_style()),
                Span::styled(truncate(model, 32), accent_style()),
                Span::styled("  /model to change", secondary_style()),
            ]),
            Line::from(vec![
                Span::styled("reasoning: ", secondary_style()),
                Span::styled(truncate(reasoning, 24), accent_style()),
            ]),
            Line::from(vec![
                Span::styled("directory: ", secondary_style()),
                Span::styled(truncate(&self.config.workspace, 48), primary_style()),
                Span::styled(
                    format!("  session: {session}  permissions: {permission}"),
                    secondary_style(),
                ),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" AETHER ")),
            area,
        );
    }

    fn render_transcript(&self, frame: &mut Frame, area: Rect) {
        let lines = self.cells.iter().flat_map(LiveCell::render_lines).collect::<Vec<_>>();
        let scroll = bounded_scroll(self.scroll, lines.len(), area.height);
        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((scroll, 0));
        frame.render_widget(paragraph, area);
    }

    fn render_composer(&self, frame: &mut Frame, area: Rect) {
        let title = if self.busy { " Working " } else { " Ask AETHER to do anything " };
        let text = if self.composer.is_empty() && !self.busy {
            "› Ask AETHER to do anything".to_owned()
        } else {
            self.composer
                .split('\n')
                .enumerate()
                .map(
                    |(index, line)| {
                        if index == 0 { format!("› {line}") } else { format!("  {line}") }
                    },
                )
                .collect::<Vec<_>>()
                .join("\n")
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(if self.composer.is_empty() { secondary_style() } else { primary_style() })
                .block(Block::default().borders(Borders::TOP).title(title))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let value = if let Some(note) = &self.footer_note {
            note.clone()
        } else if self.busy {
            let seconds = self.turn_started.map_or(0, |started| started.elapsed().as_secs());
            let detail = self
                .working_detail
                .as_deref()
                .map_or_else(String::new, |detail| format!(" · {}", truncate(detail, 28)));
            format!("• Working ({seconds}s{detail} · esc to interrupt)")
        } else if self.queued.is_empty() {
            "? for shortcuts · ctrl+j newline · / for commands".to_owned()
        } else {
            format!("{} prompt(s) queued · ? for shortcuts", self.queued.len())
        };
        frame.render_widget(
            Paragraph::new(truncate(&value, area.width.saturating_sub(1) as usize))
                .style(secondary_style()),
            area,
        );
    }

    fn render_overlay(&self, frame: &mut Frame, area: Rect) {
        if matches!(self.overlay, Overlay::None) {
            return;
        }
        let overlay_area = centered_rect(area, 78, 70);
        frame.render_widget(ClearWidget, overlay_area);
        match &self.overlay {
            Overlay::Commands { query, selected } => {
                let items = command_items(query);
                let rows = if items.is_empty() {
                    vec![ListItem::new(Line::styled("No matching commands", secondary_style()))]
                } else {
                    items
                        .iter()
                        .map(|(command, description)| {
                            ListItem::new(Line::from(format!("{command}  {description}")))
                        })
                        .collect::<Vec<_>>()
                };
                let title = if query.is_empty() {
                    "/ Commands".to_owned()
                } else {
                    format!("/ Commands · {query}")
                };
                let mut list_state = ListState::default();
                list_state.select(if items.is_empty() {
                    None
                } else {
                    Some((*selected).min(items.len() - 1))
                });
                frame.render_stateful_widget(
                    List::new(rows)
                        .block(Block::default().borders(Borders::ALL).title(title))
                        .highlight_style(accent_style().add_modifier(Modifier::BOLD))
                        .highlight_symbol("› "),
                    overlay_area,
                    &mut list_state,
                );
            }
            Overlay::Models { items, selected } => {
                self.render_picker(frame, overlay_area, "Models", items, *selected);
            }
            Overlay::Sessions { items, selected } => {
                self.render_picker(
                    frame,
                    overlay_area,
                    "Resume a previous session",
                    items,
                    *selected,
                );
            }
            Overlay::Help => {
                let lines = vec![
                    Line::styled("AETHER shortcuts", primary_style()),
                    Line::from(""),
                    Line::from("enter       submit prompt / confirm"),
                    Line::from("ctrl+j      insert newline"),
                    Line::from("/           command palette"),
                    Line::from("f1          shortcuts"),
                    Line::from("f2          transcript"),
                    Line::from("esc         close / interrupt"),
                    Line::from("ctrl+c      cancel current input or turn"),
                    Line::from(""),
                    Line::styled("press esc or q to close", secondary_style()),
                ];
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(Block::default().borders(Borders::ALL).title(" Shortcuts ")),
                    overlay_area,
                );
            }
            Overlay::Transcript { scroll } => {
                let lines = self.cells.iter().flat_map(LiveCell::render_lines).collect::<Vec<_>>();
                let footer = format!("↑/↓ scroll · pgup/pgdn page · q/esc close · offset {scroll}");
                let area = inset_bottom(overlay_area, 1);
                let scroll = bounded_scroll(*scroll, lines.len(), area.height);
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .title(" / T R A N S C R I P T / "),
                        )
                        .wrap(Wrap { trim: false })
                        .scroll((scroll, 0)),
                    area,
                );
                frame.render_widget(
                    Paragraph::new(footer).style(secondary_style()),
                    bottom_line(overlay_area),
                );
            }
            Overlay::Approval { request, selected } => {
                let mut lines = vec![
                    Line::styled("AETHER needs your approval", primary_style()),
                    Line::from(""),
                    Line::from(format!("Operation: {}", sanitize_display(&request.operation))),
                    Line::from(format!("Permission: {}", request.class)),
                ];
                if let Some(target) = &request.target {
                    lines.push(Line::from(format!("Target: {}", sanitize_display(target))));
                }
                lines.push(Line::from(""));
                for (index, option) in approval_options(request).iter().enumerate() {
                    let prefix = if index == *selected { "› " } else { "  " };
                    lines.push(Line::styled(
                        format!("{prefix}{option}"),
                        if index == *selected { accent_style() } else { primary_style() },
                    ));
                }
                lines.push(Line::from(""));
                lines.push(Line::styled(
                    "enter confirm · y allow once · s allow session · n deny · esc cancel",
                    secondary_style(),
                ));
                frame.render_widget(
                    Paragraph::new(lines)
                        .block(Block::default().borders(Borders::ALL).title(" Approval ")),
                    overlay_area,
                );
            }
            Overlay::ScheduleForm(form) => self.render_schedule_form(frame, overlay_area, form),
            Overlay::ScheduleReview { draft, preview } => {
                self.render_schedule_review(frame, overlay_area, draft, preview)
            }
            Overlay::SchedulePending => {
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::styled("Saving appointment…", primary_style()),
                        Line::from(""),
                        Line::styled("Please wait", secondary_style()),
                    ])
                    .block(Block::default().borders(Borders::ALL).title(" Schedule ")),
                    overlay_area,
                );
            }
            Overlay::None => {}
        }
    }

    fn render_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        items: &[PickerItem],
        selected: usize,
    ) {
        let rows = items
            .iter()
            .map(|item| {
                let label = truncate(&display_text(&item.label), 64);
                let detail = item.detail.as_deref().map_or_else(String::new, |value| {
                    format!(" · {}", truncate(&display_text(value), 64))
                });
                let suffix = if item.unavailable { " · unavailable" } else { "" };
                ListItem::new(Line::from(format!("{label}{detail}{suffix}")))
            })
            .collect::<Vec<_>>();
        let mut list_state = ListState::default();
        list_state.select(if items.is_empty() {
            None
        } else {
            Some(selected.min(items.len() - 1))
        });
        frame.render_stateful_widget(
            List::new(rows)
                .block(Block::default().borders(Borders::ALL).title(format!(" {title} ")))
                .highlight_style(accent_style().add_modifier(Modifier::BOLD))
                .highlight_symbol("› "),
            area,
            &mut list_state,
        );
    }

    fn render_schedule_form(&self, frame: &mut Frame, area: Rect, form: &ScheduleForm) {
        let labels = ScheduleForm::labels();
        let mut lines =
            vec![Line::styled("Schedule an appointment", primary_style()), Line::from("")];
        for (index, label) in labels.iter().enumerate() {
            let prefix = if index == form.selected { "› " } else { "  " };
            let value = if form.fields[index].is_empty() { "(empty)" } else { &form.fields[index] };
            lines.push(Line::styled(
                format!("{prefix}{label}: {}", truncate(value, 48)),
                if index == form.selected { accent_style() } else { primary_style() },
            ));
            if let Some((error_field, error)) = &form.error
                && *error_field == index
            {
                lines.push(Line::styled(format!("    ↳ {}", truncate(error, 92)), error_style()));
            }
        }
        if let Some((field, error)) = &form.error
            && *field >= labels.len()
        {
            lines.push(Line::from(""));
            lines.push(Line::styled(truncate(error, 96), error_style()));
        }
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "tab/shift-tab move · enter next/review · esc cancel",
            secondary_style(),
        ));
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Schedule ")),
            area,
        );
    }

    fn render_schedule_review(
        &self,
        frame: &mut Frame,
        area: Rect,
        _draft: &AppointmentDraft,
        preview: &Appointment,
    ) {
        let local = local_display(preview);
        let mut lines = vec![
            Line::styled("Review appointment", primary_style()),
            Line::from(""),
            Line::from(format!("Title:    {}", preview.title.as_str())),
            Line::from(format!("Starts:   {local}")),
            Line::from(format!("Stored:   {}", preview.starts_at)),
            Line::from(format!("Duration: {} minutes", preview.duration_minutes)),
        ];
        if let Some(location) = &preview.location {
            lines.push(Line::from(format!("Location: {}", truncate(location.as_str(), 72))));
        }
        if let Some(notes) = &preview.notes {
            lines.push(Line::from(format!("Notes:    {}", truncate(notes.as_str(), 72))));
        }
        if !preview.attendees.is_empty() {
            lines.push(Line::from(format!(
                "Attendees: {}",
                preview.attendees.iter().map(BoundedText::as_str).collect::<Vec<_>>().join(", ")
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::styled("enter confirm and save locally · esc edit", secondary_style()));
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(" Confirm appointment ")),
            area,
        );
    }
}

impl LiveCell {
    fn persistable(&self) -> Option<TranscriptCell> {
        match self {
            Self::User(text) => Some(TranscriptCell::User {
                text: BoundedText::new(text, aether_core::MAX_TRANSCRIPT_TEXT_BYTES),
            }),
            Self::Assistant(text) => Some(TranscriptCell::Assistant {
                text: BoundedText::new(text, aether_core::MAX_TRANSCRIPT_TEXT_BYTES),
            }),
            Self::Tool { name, ok, .. } => Some(TranscriptCell::Tool {
                name: BoundedText::new(name, aether_core::MAX_TRANSCRIPT_TEXT_BYTES),
                ok: *ok,
            }),
            Self::Warning(message) => Some(TranscriptCell::Warning {
                message: BoundedText::new(message, aether_core::MAX_TRANSCRIPT_TEXT_BYTES),
            }),
            Self::Error(message) => Some(TranscriptCell::Error {
                message: BoundedText::new(message, aether_core::MAX_TRANSCRIPT_TEXT_BYTES),
            }),
            Self::TurnSummary { turn_id, summary } => Some(TranscriptCell::TurnSummary {
                turn_id: turn_id.clone(),
                summary: BoundedText::new(summary, aether_core::MAX_TRANSCRIPT_TEXT_BYTES),
            }),
            Self::Appointment(appointment) => Some(TranscriptCell::AppointmentConfirmed {
                id: appointment.id.clone(),
                title: appointment.title.clone(),
                starts_at: appointment.starts_at.clone(),
                utc_offset_minutes: appointment.utc_offset_minutes,
                duration_minutes: appointment.duration_minutes,
            }),
        }
    }

    fn from_persisted(cell: &TranscriptCell) -> Option<Self> {
        match cell {
            TranscriptCell::User { text } => Some(Self::User(text.as_str().to_owned())),
            TranscriptCell::Assistant { text } => Some(Self::Assistant(text.as_str().to_owned())),
            TranscriptCell::Tool { name, ok } => {
                Some(Self::Tool { name: name.as_str().to_owned(), output: String::new(), ok: *ok })
            }
            TranscriptCell::Warning { message } => Some(Self::Warning(message.as_str().to_owned())),
            TranscriptCell::Error { message } => Some(Self::Error(message.as_str().to_owned())),
            TranscriptCell::TurnSummary { turn_id, summary } => Some(Self::TurnSummary {
                turn_id: turn_id.clone(),
                summary: summary.as_str().to_owned(),
            }),
            TranscriptCell::AppointmentConfirmed {
                id,
                title,
                starts_at,
                utc_offset_minutes,
                duration_minutes,
            } => Some(Self::Appointment(Appointment {
                id: id.clone(),
                title: title.clone(),
                starts_at: starts_at.clone(),
                utc_offset_minutes: *utc_offset_minutes,
                duration_minutes: *duration_minutes,
                location: None,
                notes: None,
                attendees: Vec::new(),
                created_at: String::new(),
            })),
        }
    }

    fn render_lines(&self) -> Vec<Line<'static>> {
        match self {
            Self::User(text) => vec![Line::styled(format!("› {text}"), accent_style())],
            Self::Assistant(text) => vec![Line::styled(text.clone(), primary_style())],
            Self::Tool { name, output, ok } => {
                let status = ok.map_or("…", |value| if value { "done" } else { "failed" });
                vec![
                    Line::styled(format!("  ├ {name} · {status}"), tool_style()),
                    if output.is_empty() {
                        Line::from("")
                    } else {
                        Line::styled(format!("  │ {output}"), secondary_style())
                    },
                ]
            }
            Self::Warning(message) => vec![Line::styled(format!("! {message}"), warning_style())],
            Self::Error(message) => vec![Line::styled(format!("× {message}"), error_style())],
            Self::TurnSummary { summary, .. } => {
                vec![Line::styled(format!("  · {summary}"), secondary_style())]
            }
            Self::Appointment(appointment) => vec![
                Line::styled("  ✓ Appointment saved locally", success_style()),
                Line::from(format!(
                    "    {} · {}",
                    appointment.title.as_str(),
                    local_display(appointment)
                )),
            ],
        }
    }
}

/// RAII owner for raw mode, crossterm events, inline frames, and overlay screens.
pub struct TuiSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    events: EventStream,
    state: TuiState,
    overlay_screen: bool,
}

impl TuiSession {
    /// Enter the stateful TUI and preserve terminal restoration through `Drop`.
    pub fn enter(config: TuiConfig) -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnableBracketedPaste, DisableLineWrap, Hide) {
            let _ = execute!(stdout, DisableBracketedPaste, EnableLineWrap, Show);
            let _ = disable_raw_mode();
            return Err(error);
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::with_options(
            backend,
            TerminalOptions { viewport: Viewport::Inline(INLINE_VIEWPORT_HEIGHT) },
        ) {
            Ok(terminal) => terminal,
            Err(error) => {
                let mut cleanup_stdout = io::stdout();
                let _ = execute!(cleanup_stdout, DisableBracketedPaste, EnableLineWrap, Show);
                let _ = disable_raw_mode();
                return Err(error);
            }
        };
        Ok(Self {
            terminal,
            events: EventStream::new(),
            state: TuiState::new(config),
            overlay_screen: false,
        })
    }

    /// Borrow the UI state.
    pub fn state(&self) -> &TuiState {
        &self.state
    }

    /// Mutably borrow the UI state.
    pub fn state_mut(&mut self) -> &mut TuiState {
        &mut self.state
    }

    /// Read the next crossterm event without blocking the async runtime.
    pub async fn next_event(&mut self) -> io::Result<Option<TuiEvent>> {
        while let Some(event) = self.events.next().await {
            let event = event?;
            if let Some(event) = TuiEvent::from_crossterm(event) {
                return Ok(Some(event));
            }
        }
        Ok(None)
    }

    /// Temporarily return terminal ownership to a legacy command that may print or prompt.
    pub fn suspend(&mut self) -> io::Result<()> {
        if self.overlay_screen {
            execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
            self.overlay_screen = false;
        }
        execute!(self.terminal.backend_mut(), EnableLineWrap, DisableBracketedPaste, Show)?;
        disable_raw_mode()
    }

    /// Re-enter the TUI after a suspended local operation.
    pub fn resume(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        execute!(self.terminal.backend_mut(), EnableBracketedPaste, DisableLineWrap, Hide)?;
        self.terminal.clear().map(|_| ())
    }

    /// Draw the current frame and synchronize the overlay terminal screen.
    pub fn draw(&mut self) -> io::Result<()> {
        let should_use_overlay = self.state.has_overlay();
        if should_use_overlay != self.overlay_screen {
            if should_use_overlay {
                execute!(self.terminal.backend_mut(), EnterAlternateScreen, Clear(ClearType::All))?;
            } else {
                execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Clear(ClearType::All))?;
            }
            self.overlay_screen = should_use_overlay;
        }
        self.terminal.draw(|frame| self.state.render(frame)).map(|_| ())
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        if self.overlay_screen {
            let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        }
        let _ = execute!(self.terminal.backend_mut(), EnableLineWrap, DisableBracketedPaste, Show);
        let _ = disable_raw_mode();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerAction {
    Move,
    Confirm,
    Cancel,
}

fn handle_picker_key(key: KeyEvent, selected: &mut usize, count: usize) -> Option<PickerAction> {
    if key.code == KeyCode::Esc || is_ctrl(key, 'c') {
        return Some(PickerAction::Cancel);
    }
    if key.code == KeyCode::Enter {
        return Some(PickerAction::Confirm);
    }
    if count == 0 {
        return Some(PickerAction::Move);
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k' | 'K') => *selected = selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j' | 'J') => {
            *selected = (*selected + 1).min(count.saturating_sub(1));
        }
        KeyCode::PageUp => *selected = selected.saturating_sub(5),
        KeyCode::PageDown => *selected = (*selected + 5).min(count.saturating_sub(1)),
        KeyCode::Home => *selected = 0,
        KeyCode::End => *selected = count.saturating_sub(1),
        _ => return Some(PickerAction::Move),
    }
    Some(PickerAction::Move)
}

fn handle_picker_items_key(
    key: KeyEvent,
    selected: &mut usize,
    items: &[PickerItem],
) -> Option<PickerAction> {
    if key.code == KeyCode::Esc || is_ctrl(key, 'c') {
        return Some(PickerAction::Cancel);
    }
    if key.code == KeyCode::Enter {
        return Some(PickerAction::Confirm);
    }
    if items.is_empty() {
        return Some(PickerAction::Move);
    }
    match key.code {
        KeyCode::Up | KeyCode::Char('k' | 'K') => move_selection(selected, items, -1),
        KeyCode::Down | KeyCode::Char('j' | 'J') => move_selection(selected, items, 1),
        KeyCode::PageUp => move_selection(selected, items, -5),
        KeyCode::PageDown => move_selection(selected, items, 5),
        KeyCode::Home => {
            *selected = items.iter().position(|item| !item.unavailable).unwrap_or(0);
        }
        KeyCode::End => {
            *selected = items.iter().rposition(|item| !item.unavailable).unwrap_or(0);
        }
        _ => return Some(PickerAction::Move),
    }
    Some(PickerAction::Move)
}

fn move_selection(selected: &mut usize, items: &[PickerItem], delta: isize) {
    let last = items.len().saturating_sub(1) as isize;
    let direction = delta.signum();
    let mut index = (*selected as isize + delta).clamp(0, last);
    while items[index as usize].unavailable && index != 0 && index != last {
        index = (index + direction).clamp(0, last);
    }
    if items[index as usize].unavailable {
        let fallback = if direction < 0 {
            (0..=last).rev().collect::<Vec<_>>()
        } else {
            (0..=last).collect::<Vec<_>>()
        };
        if let Some(index) = fallback.into_iter().find(|index| !items[*index as usize].unavailable)
        {
            *selected = index as usize;
            return;
        }
    }
    *selected = index as usize;
}

fn normalize_selection(selected: &mut usize, items: &[PickerItem]) {
    if items.is_empty() {
        *selected = 0;
        return;
    }
    *selected = (*selected).min(items.len().saturating_sub(1));
    if items[*selected].unavailable
        && let Some(index) = items.iter().position(|item| !item.unavailable)
    {
        *selected = index;
    }
}

fn field_limit(index: usize) -> usize {
    match index {
        0 => aether_core::MAX_APPOINTMENT_TITLE_BYTES,
        1 => 10,
        2 => 5,
        3 => 6,
        4 => 4,
        5 => aether_core::MAX_APPOINTMENT_LOCATION_BYTES,
        6 => aether_core::MAX_APPOINTMENT_NOTES_BYTES,
        _ => aether_core::MAX_APPOINTMENT_ATTENDEE_BYTES * aether_core::MAX_APPOINTMENT_ATTENDEES,
    }
}

fn command_items(query: &str) -> Vec<(&'static str, &'static str)> {
    const COMMANDS: &[(&str, &str)] = &[
        ("/model", "Choose a live Rainy model"),
        ("/status", "Show session, model, workflow, and permission state"),
        ("/context", "Show bounded context and workflow evidence"),
        ("/config", "Show or update local preferences"),
        ("/auth", "Show credential status or enter a key"),
        ("/sessions", "Pick a local session"),
        ("/resume", "Restore a local session"),
        ("/doctor", "Run offline diagnostics"),
        ("/update", "Update AETHER Fx and leave the shell"),
        ("/browser", "Install/start Obscura or run read-only research"),
        ("/schedule", "Create a confirmed local appointment"),
        ("/transcript", "Browse the safe session transcript"),
        ("/help", "Show local shortcuts and commands"),
        ("/clear", "Start a fresh local session"),
        ("/exit", "Finish and leave the shell"),
    ];
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return COMMANDS.to_vec();
    }
    COMMANDS
        .iter()
        .copied()
        .filter(|(command, description)| {
            command.contains(&query) || description.to_ascii_lowercase().contains(&query)
        })
        .collect()
}

fn approval_options(request: &PermissionRequest) -> Vec<&'static str> {
    if request.class == aether_core::PermissionClass::BrowserAction {
        vec!["Yes, proceed (y)", "No, deny (n)"]
    } else {
        vec!["Yes, proceed (y)", "Yes, and allow this scope (s)", "No, deny (n)"]
    }
}

fn form_fields_from_draft(draft: &AppointmentDraft) -> Vec<String> {
    vec![
        draft.title.clone(),
        draft.date.clone(),
        draft.time.clone(),
        draft.utc_offset.clone(),
        draft.duration_minutes.clone(),
        draft.location.clone().unwrap_or_default(),
        draft.notes.clone().unwrap_or_default(),
        draft.attendees.join(", "),
    ]
}

fn validation_field(error: &str) -> usize {
    let error = error.to_ascii_lowercase();
    if error.contains("title") {
        0
    } else if error.contains("date") || error.contains("start") {
        1
    } else if error.contains("time") {
        2
    } else if error.contains("offset") {
        3
    } else if error.contains("duration") {
        4
    } else if error.contains("location") {
        5
    } else if error.contains("notes") {
        6
    } else if error.contains("attendee") {
        7
    } else {
        0
    }
}

fn local_display(appointment: &Appointment) -> String {
    let sign = if appointment.utc_offset_minutes < 0 { '-' } else { '+' };
    let minutes = appointment.utc_offset_minutes.unsigned_abs();
    let local = OffsetDateTime::parse(&appointment.starts_at, &Rfc3339).ok().and_then(|instant| {
        UtcOffset::from_whole_seconds(appointment.utc_offset_minutes.saturating_mul(60))
            .ok()
            .map(|offset| instant.to_offset(offset))
    });
    let local = local.map_or_else(
        || appointment.starts_at.clone(),
        |local| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}",
                local.year(),
                local.month() as u8,
                local.day(),
                local.hour(),
                local.minute()
            )
        },
    );
    format!("{local} ({}{:02}:{:02})", sign, minutes / 60, minutes % 60)
}

fn append_bounded(value: &mut String, addition: &str, limit: usize) {
    if value.len() >= limit {
        return;
    }
    let remaining = limit.saturating_sub(value.len());
    let mut end = addition.len().min(remaining);
    while end > 0 && !addition.is_char_boundary(end) {
        end -= 1;
    }
    value.push_str(&addition[..end]);
}

fn bound_notice(value: &str) -> String {
    bounded_text(value, MAX_NOTICE_BYTES)
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    let value = sanitize_display(value);
    let mut bounded = String::new();
    append_bounded(&mut bounded, &value, max_bytes);
    bounded
}

fn sanitize_display(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect()
}

fn display_text(value: &str) -> String {
    if value.is_empty() { "unknown".to_owned() } else { sanitize_display(value) }
}

fn default_utc_offset() -> String {
    let seconds =
        UtcOffset::current_local_offset().map(|offset| offset.whole_seconds()).unwrap_or(0);
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.unsigned_abs() / 60;
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

fn truncate(value: &str, max_cells: usize) -> String {
    let value = sanitize_display(value);
    if max_cells == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(value.as_str()) <= max_cells {
        return value;
    }

    let ellipsis = '…';
    let ellipsis_width = UnicodeWidthStr::width("…");
    let content_limit = max_cells.saturating_sub(ellipsis_width);
    let mut result = String::new();
    let mut width: usize = 0;
    for grapheme in value.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width.saturating_add(grapheme_width) > content_limit {
            break;
        }
        result.push_str(grapheme);
        width += grapheme_width;
    }
    result.push(ellipsis);
    result
}

fn bounded_scroll(requested: u16, line_count: usize, viewport_height: u16) -> u16 {
    let maximum = line_count.saturating_sub(viewport_height as usize);
    if requested == u16::MAX {
        maximum.min(u16::MAX as usize) as u16
    } else {
        (requested as usize).min(maximum).min(u16::MAX as usize) as u16
    }
}

fn is_ctrl(key: KeyEvent, character: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&character))
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn inset_bottom(area: Rect, rows: u16) -> Rect {
    Rect { height: area.height.saturating_sub(rows), ..area }
}

fn bottom_line(area: Rect) -> Rect {
    Rect { y: area.y.saturating_add(area.height.saturating_sub(1)), height: 1, ..area }
}

fn no_color() -> bool {
    env::var_os("NO_COLOR").is_some()
}

fn primary_style() -> Style {
    Style::default()
}

fn secondary_style() -> Style {
    if no_color() { primary_style() } else { Style::default().add_modifier(Modifier::DIM) }
}

fn accent_style() -> Style {
    if no_color() { primary_style() } else { Style::default().fg(Color::Cyan) }
}

fn tool_style() -> Style {
    if no_color() { primary_style() } else { Style::default().fg(Color::Magenta) }
}

fn warning_style() -> Style {
    if no_color() { primary_style() } else { Style::default().fg(Color::Yellow) }
}

fn error_style() -> Style {
    if no_color() { primary_style() } else { Style::default().fg(Color::Red) }
}

fn success_style() -> Style {
    if no_color() { primary_style() } else { Style::default().fg(Color::Green) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn state() -> TuiState {
        TuiState::new(TuiConfig {
            version: "0.1.0".to_owned(),
            workspace: "/tmp/project".to_owned(),
            model: Some("model-a".to_owned()),
            reasoning: Some("supported".to_owned()),
            session_id: None,
            yolo: false,
            resumed: false,
            initial_status: None,
        })
    }

    #[test]
    fn composer_handles_grapheme_safe_backspace() {
        let mut state = state();
        state.insert_text("a\u{301}");
        state.delete_previous_grapheme();
        assert!(state.composer.is_empty());
    }

    #[test]
    fn command_palette_selects_schedule() {
        let mut state = state();
        state.open_commands();
        while let Overlay::Commands { selected, .. } = state.overlay {
            if command_items("")[selected].0 == "/schedule" {
                break;
            }
            let _ =
                state.handle_event(TuiEvent::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        }
        let _ =
            state.handle_event(TuiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(matches!(state.overlay, Overlay::None));
    }

    #[test]
    fn schedule_form_reaches_review_after_valid_input() {
        let mut state = state();
        state.open_schedule();
        if let Overlay::ScheduleForm(form) = &mut state.overlay {
            form.fields = vec![
                "Design review".to_owned(),
                "2026-09-03".to_owned(),
                "14:30".to_owned(),
                "-05:00".to_owned(),
                "45".to_owned(),
                String::new(),
                String::new(),
                String::new(),
            ];
            form.selected = form.fields.len() - 1;
        }
        let _ =
            state.handle_event(TuiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(matches!(state.overlay, Overlay::ScheduleReview { .. }));
    }

    #[test]
    fn renders_a_fixed_terminal_without_panicking() {
        let mut terminal = Terminal::new(TestBackend::new(48, 18)).unwrap();
        let state = state();
        terminal.draw(|frame| state.render(frame)).unwrap();
    }

    fn rendered(state: &TuiState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| state.render(frame)).unwrap();
        format!("{}", terminal.backend())
    }

    #[test]
    fn snapshot_shell_at_fixed_widths() {
        let mut state = state();
        state.begin_turn("Inspect the appointment flow".to_owned());
        state.apply_agent_event(&AgentEvent::TextDelta {
            text: BoundedText::new("I will inspect the local flow.", 1024),
        });
        state.apply_agent_event(&AgentEvent::ToolStarted {
            call_id: ToolCallId::new("tool-1").unwrap(),
            name: "read_file".to_owned(),
            permission: "read".to_owned(),
            operation: "inspect source".to_owned(),
            step_id: None,
        });
        state.apply_agent_event(&AgentEvent::Warning {
            message: BoundedText::new("bounded warning", 1024),
        });
        let snapshot = [18, 24, 48, 80, 120]
            .into_iter()
            .map(|width| format!("--- {width} columns ---\n{}", rendered(&state, width, 18)))
            .collect::<Vec<_>>()
            .join("\n");
        insta::assert_snapshot!(snapshot);
    }

    #[test]
    fn snapshot_command_palette_and_schedule_review() {
        let mut state = state();
        state.open_commands();
        let palette = rendered(&state, 80, 24);

        state.open_schedule();
        if let Overlay::ScheduleForm(form) = &mut state.overlay {
            form.fields = vec![
                "Design review".to_owned(),
                "2026-09-03".to_owned(),
                "14:30".to_owned(),
                "-05:00".to_owned(),
                "45".to_owned(),
                "Studio".to_owned(),
                "Bring the latest flow".to_owned(),
                "Alex".to_owned(),
            ];
            form.selected = 7;
        }
        let _ =
            state.handle_event(TuiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        let review = rendered(&state, 80, 24);
        insta::assert_snapshot!(format!("palette\n{palette}\nreview\n{review}"));
    }

    #[test]
    fn queued_prompts_are_bounded_and_paste_preserves_newlines() {
        let mut state = state();
        state.begin_turn("active".to_owned());
        for index in 0..(MAX_QUEUED_PROMPTS + 2) {
            state.set_composer(format!("queued-{index}"));
            let _ = state
                .handle_event(TuiEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        }
        assert_eq!(state.queued_prompt_count(), MAX_QUEUED_PROMPTS);
        state.cancel_turn();
        state.open_schedule();
        let _ = state.handle_event(TuiEvent::Paste("Design\nreview".to_owned()));
        assert!(
            matches!(state.overlay, Overlay::ScheduleForm(ScheduleForm { fields, .. }) if fields[0] == "Design\nreview")
        );
    }
}
