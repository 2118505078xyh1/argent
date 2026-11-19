//! The native window's state.
//!
//! The UI thread owns pixels; the agent owns a worker thread. The only link
//! between them is the same [`argent_engine`] channel pair every GUI frontend
//! uses: commands go in, events and approval requests come out. A short timer
//! drains those channels, so streaming text appears without blocking the
//! renderer.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::widget::text_editor;
use iced::{Subscription, Task};

use argent_agent::{AbortHandle, AgentEvent};
use argent_automation::{Action, Automation, AutomationStore, Schedule};
use argent_core::config::Config;
use argent_engine::{Approval, Engine, SessionRow, UiEvent};
use argent_i18n::{t, Locale};
use argent_session::SessionIndex;
use argent_ui_core::{Command, Palette, Transcript};

pub const SCROLL_ID: &str = "argent-transcript";
pub const INPUT_ID: &str = "argent-composer";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Chat,
    Automations,
    Documents,
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Drain the worker's channels.
    Tick,
    Edit(text_editor::Action),
    /// Fill the composer from an empty-state suggestion.
    SetPrompt(String),
    Send,
    Abort,
    NewSession,
    Rewind,
    /// Open the session at this index in the sidebar.
    Open(usize),
    Model(String),
    Thinking(String),
    Allow(u64),
    Deny(u64),
    /// The transcript scrolled; `true` means it is at the bottom.
    Scrolled(bool),
    DismissError,
    FolderInput(String),
    ChooseFolder(String),
    CancelFolder,
    Profile(String),
    ShowChat,
    ShowAutomations,
    AutomationName(String),
    AutomationSchedule(String),
    AutomationPrompt(String),
    AutomationAdd,
    AutomationToggle(usize),
    AutomationRun(usize),
    AutomationDelete(usize),
    AutomationLog(usize),
    CloseAutomationLog,
    ShowDocuments,
    OpenDocument(usize),
}

pub struct OfficeDoc {
    pub name: String,
    pub path: PathBuf,
}

pub struct DocumentView {
    pub title: String,
    pub body: String,
}

pub struct AutomationEvent {
    pub text: String,
}

pub struct AutomationLogView {
    pub title: String,
    pub body: String,
}

/// The worker's end of the channels.
pub struct Worker {
    commands: Sender<Command>,
    events: Receiver<UiEvent>,
    approvals: Receiver<Approval>,
    abort: AbortHandle,
    steering: Arc<Mutex<VecDeque<String>>>,
}

/// A task chosen in the folder picker but not yet named by its first prompt.
pub struct PendingTask {
    pub folder: PathBuf,
    pub created: Instant,
}

pub struct FolderPicker {
    pub path: String,
    /// The profile this task will run under; `None` is the default.
    pub profile: Option<String>,
    pub error: Option<String>,
}

/// One approval on screen.
pub struct Pending {
    pub id: u64,
    pub name: String,
    pub detail: String,
}

pub struct App {
    pub cwd: PathBuf,
    pub config: Config,
    pub session_dir: PathBuf,
    pub locale: Locale,
    pub palette: Palette,
    pub profiles: Vec<argent_profiles::Profile>,
    /// The profile the current worker runs under, for display.
    pub profile: Option<String>,

    pub screen: Screen,
    pub automations: Vec<Automation>,
    pub automation_name: String,
    pub automation_schedule: String,
    pub automation_prompt: String,
    pub automation_error: Option<String>,
    pub automation_status: Option<String>,
    pub automation_log: Option<AutomationLogView>,
    automation_tx: Sender<AutomationEvent>,
    automation_rx: Receiver<AutomationEvent>,

    pub office_files: Vec<OfficeDoc>,
    pub document: Option<DocumentView>,

    pub worker: Option<Worker>,

    pub transcript: Transcript,
    pub input: text_editor::Content,

    pub model: String,
    pub thinking: String,
    pub models: Vec<String>,
    pub levels: Vec<String>,
    pub context_max: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
    pub requests: u64,
    pub cost: Option<f64>,

    pub working: bool,
    /// Follow new output only while the operator is at the bottom.
    pub follow: bool,

    pub sessions: Vec<SessionRow>,
    pub active: Option<PathBuf>,
    /// When the current empty session was opened: used to expire it after a
    /// minute without a prompt.
    pub active_since: Option<Instant>,
    pub pending_task: Option<PendingTask>,
    pub folder_picker: Option<FolderPicker>,
    last_sweep: Instant,
    pub pending: Vec<Pending>,
    replies: HashMap<u64, Sender<bool>>,
    pub error: Option<String>,
}

impl App {
    pub fn new() -> App {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let locale = Locale::from_env();
        let (automation_tx, automation_rx) = channel();
        match argent_engine::prepare_config(&cwd) {
            Ok(config) => {
                let sessions =
                    argent_engine::session_rows(&SessionIndex::new(config.session_dir.clone()));
                let palette = Palette::from_config(Some(config.theme.as_str()));
                let session_dir = config.session_dir.clone();
                let profiles = load_profiles(&cwd);
                let profile = applied_profile(&config);
                let automations = load_automations(&cwd);
                let worker = start_worker(&config, &cwd);
                App {
                    model: config.model.clone(),
                    thinking: config.thinking_level.clone(),
                    context_max: config.max_context_tokens,
                    config,
                    session_dir,
                    cwd,
                    locale,
                    palette,
                    profiles,
                    profile,
                    screen: Screen::Chat,
                    automations,
                    automation_name: String::new(),
                    automation_schedule: "every 30m".to_string(),
                    automation_prompt: String::new(),
                    automation_error: None,
                    automation_status: None,
                    automation_log: None,
                    automation_tx,
                    automation_rx,
                    office_files: Vec::new(),
                    document: None,
                    worker: Some(worker),
                    transcript: Transcript::new(),
                    input: text_editor::Content::new(),
                    models: Vec::new(),
                    levels: vec![
                        "auto".to_string(),
                        "low".to_string(),
                        "medium".to_string(),
                        "high".to_string(),
                        "max".to_string(),
                    ],
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_tokens: 0,
                    requests: 0,
                    cost: None,
                    working: false,
                    follow: true,
                    sessions,
                    active: None,
                    active_since: None,
                    pending_task: None,
                    folder_picker: None,
                    last_sweep: Instant::now(),
                    pending: Vec::new(),
                    replies: HashMap::new(),
                    error: None,
                }
            }
            Err(error) => {
                let mut app = App {
                    cwd,
                    config: Config::default(),
                    session_dir: PathBuf::new(),
                    locale,
                    palette: Palette::dark(),
                    profiles: Vec::new(),
                    profile: None,
                    screen: Screen::Chat,
                    automations: Vec::new(),
                    automation_name: String::new(),
                    automation_schedule: "every 30m".to_string(),
                    automation_prompt: String::new(),
                    automation_error: None,
                    automation_status: None,
                    automation_log: None,
                    automation_tx,
                    automation_rx,
                    office_files: Vec::new(),
                    document: None,
                    worker: None,
                    transcript: Transcript::new(),
                    input: text_editor::Content::new(),
                    model: String::new(),
                    thinking: String::new(),
                    models: Vec::new(),
                    levels: Vec::new(),
                    context_max: 0,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_tokens: 0,
                    requests: 0,
                    cost: None,
                    working: false,
                    follow: true,
                    sessions: Vec::new(),
                    active: None,
                    active_since: None,
                    pending_task: None,
                    folder_picker: None,
                    last_sweep: Instant::now(),
                    pending: Vec::new(),
                    replies: HashMap::new(),
                    error: None,
                };
                app.error = Some(format!("cannot start argent: {}", error));
                app
            }
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => {
                let mut changed = self.drain();
                while let Ok(event) = self.automation_rx.try_recv() {
                    self.automation_status = Some(event.text);
                    self.automations = load_automations(&self.cwd);
                    changed = true;
                }
                // A chosen task that never received its first prompt is not a
                // task: drop the virtual row after a minute.
                if self
                    .pending_task
                    .as_ref()
                    .is_some_and(|task| task.created.elapsed() >= Duration::from_secs(60))
                {
                    self.pending_task = None;
                    changed = true;
                }
                // The same rule for an empty session opened from the sidebar:
                // close its writer and let the engine delete the file.
                if !self.working
                    && self.active.is_some()
                    && self.transcript.is_empty()
                    && self
                        .active_since
                        .is_some_and(|since| since.elapsed() >= Duration::from_secs(60))
                {
                    if let Some(worker) = &self.worker {
                        let _ = worker.commands.send(Command::NewSession);
                    }
                    self.active = None;
                    self.active_since = None;
                    self.transcript.clear();
                    changed = true;
                }
                // Empty placeholders from older runs have no open writer;
                // remove them twice a minute.
                if self.last_sweep.elapsed() >= Duration::from_secs(5) {
                    self.last_sweep = Instant::now();
                    let index = SessionIndex::new(self.session_dir.clone());
                    let removed =
                        argent_engine::sweep_empty_sessions(&index, &[], Duration::from_secs(60));
                    if !removed.is_empty() {
                        self.sessions = argent_engine::session_rows(&index);
                        changed = true;
                    }
                }
                if changed && self.follow {
                    return iced::widget::operation::snap_to_end(SCROLL_ID);
                }
            }
            Message::Edit(action) => self.input.perform(action),
            Message::SetPrompt(value) => {
                self.input = text_editor::Content::with_text(&value);
                return iced::widget::operation::focus(INPUT_ID);
            }
            Message::Send => return self.send(),
            Message::Abort => {
                if let Some(worker) = &self.worker {
                    worker.abort.abort();
                }
            }
            Message::NewSession => {
                if self.working {
                    self.transcript.push_notice(
                        argent_ui_core::NoticeKind::Warning,
                        t(self.locale, "ui.busy"),
                    );
                } else {
                    // A task belongs to a folder: pick one before creating it.
                    self.folder_picker = Some(FolderPicker {
                        path: self.cwd.display().to_string(),
                        profile: self.profile.clone(),
                        error: None,
                    });
                }
            }
            Message::Rewind => {
                // 运行中回滚会和正在进行的写入抢同一批文件，等它结束。
                if self.working {
                    self.transcript.push_notice(
                        argent_ui_core::NoticeKind::Warning,
                        t(self.locale, "ui.busy"),
                    );
                } else if let Some(worker) = &self.worker {
                    let _ = worker.commands.send(Command::Rewind { steps: 1 });
                }
            }
            Message::Open(index) => {
                if self.working {
                    self.transcript.push_notice(
                        argent_ui_core::NoticeKind::Warning,
                        t(self.locale, "ui.busy"),
                    );
                } else if let Some(row) = self.sessions.get(index) {
                    if self.active.as_ref() == Some(&row.path) {
                        return Task::none();
                    }
                    if let Some(worker) = &self.worker {
                        let _ = worker.commands.send(Command::OpenSession(row.path.clone()));
                        self.screen = Screen::Chat;
                        self.active = Some(row.path.clone());
                        self.active_since = None;
                        self.pending_task = None;
                        self.folder_picker = None;
                        self.transcript.clear();
                        self.pending.clear();
                        self.follow = true;
                        self.error = None;
                    }
                }
            }
            Message::Model(id) => {
                if let Some(worker) = &self.worker {
                    let _ = worker.commands.send(Command::SetModel(id.clone()));
                }
                self.model = id;
            }
            Message::Thinking(level) => {
                if let Some(worker) = &self.worker {
                    let _ = worker.commands.send(Command::SetThinking(level.clone()));
                }
                self.thinking = level;
            }
            Message::Allow(id) => self.answer(id, true),
            Message::Deny(id) => self.answer(id, false),
            Message::Scrolled(at_end) => self.follow = at_end,
            Message::DismissError => self.error = None,
            Message::FolderInput(value) => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.path = value;
                    picker.error = None;
                }
            }
            Message::CancelFolder => self.folder_picker = None,
            Message::Profile(name) => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.profile = if name == t(self.locale, "ui.default_profile") {
                        None
                    } else {
                        Some(name)
                    };
                }
            }
            Message::ChooseFolder(path) => return self.choose_folder(&path),
            Message::ShowChat => self.screen = Screen::Chat,
            Message::ShowAutomations => {
                self.screen = Screen::Automations;
                self.automations = load_automations(&self.cwd);
            }
            Message::AutomationName(value) => self.automation_name = value,
            Message::AutomationSchedule(value) => self.automation_schedule = value,
            Message::AutomationPrompt(value) => self.automation_prompt = value,
            Message::AutomationAdd => self.add_automation(),
            Message::AutomationToggle(index) => self.toggle_automation(index),
            Message::AutomationRun(index) => return self.run_automation(index),
            Message::AutomationDelete(index) => self.delete_automation(index),
            Message::AutomationLog(index) => self.open_automation_log(index),
            Message::CloseAutomationLog => self.automation_log = None,
            Message::ShowDocuments => {
                self.screen = Screen::Documents;
                self.office_files = load_office_files(&self.cwd);
            }
            Message::OpenDocument(index) => self.open_document(index),
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        // Idle windows do not need 25 wake-ups a second; a running turn does.
        let period = if self.working || !self.pending.is_empty() {
            Duration::from_millis(40)
        } else {
            Duration::from_millis(400)
        };
        let tick = iced::time::every(period).map(|_| Message::Tick);
        Subscription::batch([tick])
    }

    fn send(&mut self) -> Task<Message> {
        let text = self.input.text().trim().to_string();
        if text.is_empty() {
            return Task::none();
        }
        // While a run is active the message is a correction, not a new
        // prompt: it enters the loop between turns without aborting tools.
        if self.working {
            if let Some(worker) = &self.worker {
                if let Ok(mut queue) = worker.steering.lock() {
                    queue.push_back(text.clone());
                }
            }
            self.input = text_editor::Content::new();
            self.transcript.push_user(text);
            self.follow = true;
            return Task::batch([
                iced::widget::operation::snap_to_end(SCROLL_ID),
                iced::widget::operation::focus(INPUT_ID),
            ]);
        }
        let Some(worker) = &self.worker else {
            self.error = Some("the agent worker is not running".to_string());
            return Task::none();
        };
        if worker.commands.send(Command::Prompt(text.clone())).is_err() {
            self.error = Some("the agent worker is gone".to_string());
            return Task::none();
        }
        self.input = text_editor::Content::new();
        self.pending_task = None;
        self.transcript.push_user(text);
        self.working = true;
        self.follow = true;
        self.error = None;
        Task::batch([
            iced::widget::operation::snap_to_end(SCROLL_ID),
            iced::widget::operation::focus(INPUT_ID),
        ])
    }

    fn choose_folder(&mut self, raw: &str) -> Task<Message> {
        let folder = PathBuf::from(raw.trim().trim_matches('"'));
        if !folder.is_dir() {
            if let Some(picker) = &mut self.folder_picker {
                picker.error = Some(t(self.locale, "ui.invalid_folder").to_string());
            }
            return Task::none();
        }
        if self.working {
            self.transcript.push_notice(
                argent_ui_core::NoticeKind::Warning,
                t(self.locale, "ui.busy"),
            );
            return Task::none();
        }
        let selected = self.folder_picker.as_ref().and_then(|p| p.profile.clone());
        let profiles = load_profiles(&folder);
        if let Some(name) = &selected {
            if !profiles.iter().any(|profile| &profile.name == name) {
                if let Some(picker) = &mut self.folder_picker {
                    picker.error = Some(format!("unknown profile '{}'", name));
                }
                return Task::none();
            }
        }
        let config = match argent_engine::prepare_config_with_profile(&folder, selected.as_deref())
        {
            Ok(config) => config,
            Err(error) => {
                if let Some(picker) = &mut self.folder_picker {
                    picker.error = Some(error);
                }
                return Task::none();
            }
        };
        // The worker's tools run in the task's folder, so a new folder means
        // a new worker. The previous session stays on disk.
        if let Some(worker) = self.worker.take() {
            let _ = worker.commands.send(Command::Shutdown);
        }
        self.model = config.model.clone();
        self.thinking = config.thinking_level.clone();
        self.context_max = config.max_context_tokens;
        self.palette = Palette::from_config(Some(config.theme.as_str()));
        self.profiles = profiles;
        self.profile = selected;
        self.worker = Some(start_worker(&config, &folder));
        self.config = config;
        self.cwd = folder.clone();
        self.screen = Screen::Chat;
        self.automations = load_automations(&folder);
        self.transcript.clear();
        self.active = None;
        self.active_since = None;
        self.pending.clear();
        self.replies.clear();
        self.folder_picker = None;
        self.pending_task = Some(PendingTask {
            folder,
            created: Instant::now(),
        });
        self.follow = true;
        self.error = None;
        iced::widget::operation::focus(INPUT_ID)
    }

    fn open_document(&mut self, index: usize) {
        let Some(document) = self.office_files.get(index) else {
            return;
        };
        let body = match argent_tools::office::extract_text(&document.path) {
            Ok(text) => clip(&text, 60_000),
            Err(error) => error.to_string(),
        };
        self.document = Some(DocumentView {
            title: document.name.clone(),
            body,
        });
    }

    fn add_automation(&mut self) {
        let name = self.automation_name.trim().to_string();
        let prompt = self.automation_prompt.trim().to_string();
        if name.is_empty() || prompt.is_empty() {
            self.automation_error = Some(t(self.locale, "ui.automation_empty").to_string());
            return;
        }
        let schedule = match Schedule::parse(self.automation_schedule.trim()) {
            Ok(schedule) => schedule,
            Err(error) => {
                self.automation_error = Some(error.to_string());
                return;
            }
        };
        let mut store = match AutomationStore::for_workspace(&self.cwd) {
            Ok(store) => store,
            Err(error) => {
                self.automation_error = Some(error.to_string());
                return;
            }
        };
        let mut task = Automation::new(&name, schedule, Action::Prompt(prompt));
        task.cwd = Some(self.cwd.clone());
        task.profile = self.profile.clone();
        store.upsert(task);
        if let Err(error) = store.save() {
            self.automation_error = Some(error.to_string());
            return;
        }
        self.automation_name.clear();
        self.automation_prompt.clear();
        self.automation_error = None;
        self.automation_status = Some(t(self.locale, "ui.saved").to_string());
        self.automations = load_automations(&self.cwd);
    }

    fn toggle_automation(&mut self, index: usize) {
        let Some(task) = self.automations.get(index) else {
            return;
        };
        let id = task.id.clone();
        if let Ok(mut store) = AutomationStore::for_workspace(&self.cwd) {
            if let Some(saved) = store.get_mut(&id) {
                saved.enabled = !saved.enabled;
            }
            let _ = store.save();
        }
        self.automations = load_automations(&self.cwd);
    }

    fn delete_automation(&mut self, index: usize) {
        let Some(task) = self.automations.get(index) else {
            return;
        };
        let id = task.id.clone();
        if let Ok(mut store) = AutomationStore::for_workspace(&self.cwd) {
            store.remove(&id);
            let _ = store.save();
        }
        self.automations = load_automations(&self.cwd);
    }

    fn run_automation(&mut self, index: usize) -> Task<Message> {
        let Some(task) = self.automations.get(index).cloned() else {
            return Task::none();
        };
        let cwd = self.cwd.clone();
        let tx = self.automation_tx.clone();
        self.automation_status = Some(t(self.locale, "ui.running").to_string());
        std::thread::spawn(move || {
            let result = AutomationStore::for_workspace(&cwd).and_then(|mut store| {
                argent_automation::run(&task, &mut store, argent_automation::now())
            });
            let text = match result {
                Ok(result) => format!(
                    "{} · {}s · {}",
                    result.status,
                    result.seconds,
                    result.log.display()
                ),
                Err(error) => format!("error: {}", error),
            };
            let _ = tx.send(AutomationEvent { text });
        });
        Task::none()
    }

    fn open_automation_log(&mut self, index: usize) {
        let Some(task) = self.automations.get(index) else {
            return;
        };
        let Some(path) = &task.last_log else {
            self.automation_status = Some(t(self.locale, "ui.no_log").to_string());
            return;
        };
        match std::fs::read_to_string(path) {
            Ok(body) => {
                self.automation_log = Some(AutomationLogView {
                    title: task.name.clone(),
                    body: clip(&body, 200_000),
                });
            }
            Err(error) => self.automation_status = Some(error.to_string()),
        }
    }

    fn answer(&mut self, id: u64, allow: bool) {
        if let Some(reply) = self.replies.remove(&id) {
            let _ = reply.send(allow);
        }
        self.pending.retain(|pending| pending.id != id);
    }

    /// Move everything the worker produced into the view model.
    fn drain(&mut self) -> bool {
        let mut events = Vec::new();
        let mut approvals = Vec::new();
        if let Some(worker) = &mut self.worker {
            while let Ok(event) = worker.events.try_recv() {
                events.push(event);
            }
            while let Ok(request) = worker.approvals.try_recv() {
                approvals.push(request);
            }
        }
        let changed = !events.is_empty() || !approvals.is_empty();
        for event in events {
            self.apply(event);
        }
        for request in approvals {
            let detail = approval_hint(&request.name, &request.arguments);
            self.replies.insert(request.id, request.reply);
            self.pending.push(Pending {
                id: request.id,
                name: request.name,
                detail,
            });
        }
        changed
    }

    fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::Ready {
                model,
                level,
                context_max,
                models,
                thinking,
                cwd,
                theme,
                ..
            } => {
                self.model = model;
                self.thinking = level;
                self.context_max = context_max;
                self.models = models.into_iter().map(|(id, _)| id).collect();
                self.levels = thinking;
                self.cwd = PathBuf::from(cwd);
                self.palette = Palette::from_config(Some(theme.as_str()));
            }
            UiEvent::Agent(event) => {
                match &event {
                    AgentEvent::TurnStart { .. } => self.working = true,
                    AgentEvent::Finished { .. } => self.working = false,
                    _ => {}
                }
                self.transcript.apply(&event);
            }
            UiEvent::TurnDone {
                usage,
                requests,
                cost,
                ..
            } => {
                self.working = false;
                self.prompt_tokens = usage.prompt_tokens;
                self.completion_tokens = usage.completion_tokens;
                self.cached_tokens = usage.cached_tokens;
                self.requests = requests;
                self.cost = cost;
            }
            UiEvent::Error(message) => {
                self.working = false;
                self.error = Some(message.clone());
                self.transcript
                    .push_notice(argent_ui_core::NoticeKind::Error, message);
            }
            UiEvent::Session { path } => {
                self.active = Some(path);
                self.active_since = Some(Instant::now());
            }
            UiEvent::SessionCleared => {
                self.active = None;
                self.active_since = None;
                self.transcript.clear();
                self.pending.clear();
                self.replies.clear();
            }
            UiEvent::Loaded(entries) => {
                self.transcript = Transcript::new();
                for entry in entries {
                    self.transcript.push_entry(entry);
                }
            }
            UiEvent::Sessions(rows) => {
                self.sessions = rows;
            }
        }
    }

    /// The heading: the active session's own title, or the new-task label.
    pub fn title(&self) -> String {
        if let Some(path) = &self.active {
            if let Some(row) = self.sessions.iter().find(|row| &row.path == path) {
                return row.title.clone();
            }
        }
        t(self.locale, "ui.new_task_title").to_string()
    }
}

fn load_profiles(cwd: &std::path::Path) -> Vec<argent_profiles::Profile> {
    let Ok(settings) = argent_config::Settings::load(cwd) else {
        return Vec::new();
    };
    argent_profiles::ProfileRegistry::load(cwd, &settings)
        .map(|registry| registry.list().into_iter().cloned().collect())
        .unwrap_or_default()
}

fn applied_profile(config: &Config) -> Option<String> {
    config
        .extra
        .get("profile")
        .and_then(argent_core::json::Json::as_str)
        .map(str::to_string)
}

fn load_office_files(cwd: &std::path::Path) -> Vec<OfficeDoc> {
    let mut out = Vec::new();
    collect_office_files(cwd, 0, &mut out);
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn collect_office_files(dir: &std::path::Path, depth: usize, out: &mut Vec<OfficeDoc>) {
    if depth > 3 || out.len() >= 100 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            !name.starts_with('.') && !matches!(name, "target" | "node_modules" | "dist" | "build")
        })
        .collect();
    children.sort();
    for path in children {
        if path.is_dir() {
            collect_office_files(&path, depth + 1, out);
            continue;
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(extension.as_str(), "docx" | "pptx" | "xlsx" | "pdf") {
            out.push(OfficeDoc {
                name: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("document")
                    .to_string(),
                path,
            });
        }
    }
}

fn load_automations(cwd: &std::path::Path) -> Vec<Automation> {
    AutomationStore::for_workspace(cwd)
        .map(|store| store.list().to_vec())
        .unwrap_or_default()
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

fn start_worker(config: &Config, cwd: &Path) -> Worker {
    let Engine {
        commands,
        events,
        approvals,
        abort,
        steering,
        paused: _,
    } = argent_engine::start_session(
        config.clone(),
        cwd.to_path_buf(),
        None,
        Arc::new(AtomicU64::new(1)),
    );
    Worker {
        commands,
        events,
        approvals,
        abort,
        steering,
    }
}

/// A one-line description of what the operator is being asked to allow.
fn approval_hint(name: &str, arguments: &str) -> String {
    let command = argent_core::json::Json::parse(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("command")
                .and_then(|command| command.as_str())
                .map(str::to_string)
        });
    match command {
        Some(command) => format!("{} will run:\n{}", name, command),
        None => format!("{} is waiting for approval", name),
    }
}
