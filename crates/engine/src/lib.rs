//! The engine behind the window: builds one agent and drives it from commands.
//!
//! The agent lives on a worker thread so the UI thread never blocks. Events go
//! one way and commands the other; only the UI thread touches pixels.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use argent_agent::{
    build_system_prompt, builtin_roles, AbortHandle, Agent, AgentConfig, AgentEvent, ApprovalScope,
    ChildFactory, EventSink, FinishReason, RoleRegistry,
};
use argent_ai::{
    AnthropicConfig, AnthropicProvider, Message, OpenAiConfig, OpenAiProvider, Provider,
    ResponsesConfig, ResponsesProvider, Usage,
};
use argent_auth::AuthStore;
use argent_config::Settings;
use argent_core::config::{ApprovalMode, Config};
use argent_core::json::Json;
use argent_provider::ProviderCatalog;
use argent_session::{entries_to_messages, Entry, SessionIndex, SessionStore};
use argent_tools::{
    CatalogEntry, CheckpointStore, DelegateBatch, DelegateHandle, DelegateTool,
    SkillRegistry, SkillTool, ToolCtx, ToolRegistry, ToolSearchTool,
};
use argent_ui_core::Command;

/// One row in the session sidebar.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub path: PathBuf,
    pub title: String,
    /// The stored UTC ISO timestamp.
    pub updated: String,
    /// The same instant in the operator's local time, `MM-DD HH:MM`.
    pub updated_local: String,
    /// Epoch seconds, for ordering and for the empty-task sweeper.
    pub modified: u64,
    pub cwd: String,
}

/// Something the UI has to react to.
pub enum UiEvent {
    /// The agent is built and ready.
    Ready {
        model: String,
        /// The reasoning level in effect.
        level: String,
        context_max: usize,
        models: Vec<(String, usize)>,
        thinking: Vec<String>,
        cwd: String,
        /// Provider template id and protocol, shown in the top bar.
        provider: String,
        protocol: String,
        /// Palette name from the config, so the window opens in the configured
        /// theme instead of always in the default one.
        theme: String,
    },
    /// Agent progress.
    Agent(AgentEvent),
    /// One request finished.
    TurnDone {
        reason: FinishReason,
        usage: Usage,
        requests: u64,
        /// Estimated spend for the session, when prices are configured.
        cost: Option<f64>,
    },
    /// The run failed before producing an event.
    Error(String),
    /// The transcript file messages are appended to.
    Session { path: PathBuf },
    /// There is no transcript file: nothing has been asked yet.
    SessionCleared,
    /// A stored conversation, replayed for display.
    Loaded(Vec<argent_ui_core::Entry>),
    /// The sidebar contents, newest first.
    Sessions(Vec<SessionRow>),
}

/// A tool call waiting for the operator's decision.
pub struct Approval {
    /// Monotonic id, so the frontend can answer a specific dialog.
    pub id: u64,
    pub name: String,
    pub arguments: String,
    /// Answer `true` to allow, `false` to deny.
    pub reply: Sender<bool>,
}

/// Handles to the worker.
pub struct Engine {
    pub commands: Sender<Command>,
    pub events: Receiver<UiEvent>,
    pub approvals: Receiver<Approval>,
    pub abort: AbortHandle,
    /// Corrections pushed by the UI while a run is in progress.
    pub steering: Arc<Mutex<VecDeque<String>>>,
}

/// Start one worker for a session. `approval_ids` is shared across sessions so
/// two background runs cannot hand the page the same approval id.
pub fn start_session(
    config: Config,
    cwd: PathBuf,
    session: Option<PathBuf>,
    approval_ids: Arc<std::sync::atomic::AtomicU64>,
) -> Engine {
    let _ = argent_agent::prune_tool_spills(std::time::Duration::from_secs(7 * 24 * 3600));
    let (command_tx, command_rx) = channel::<Command>();
    let (event_tx, event_rx) = channel::<UiEvent>();
    let (approval_tx, approval_rx) = channel::<Approval>();

    let abort = AbortHandle::new();
    let worker_abort = abort.clone();
    let steering = Arc::new(Mutex::new(VecDeque::new()));
    let worker_steering = Arc::clone(&steering);
    let session_label = session
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();

    std::thread::spawn(move || {
        // GUI 的会话文件要到第一条 prompt 才存在，先拿一个未绑定的仓库；
        // 会话出现后再 retarget，之前的记录请求都是空操作。
        let checkpoints = CheckpointStore::detached();
        let background = if config.background_intelligence {
            let model = config
                .background_model
                .clone()
                .unwrap_or_else(|| config.model.clone());
            let provider: std::sync::Arc<dyn argent_ai::Provider> =
                std::sync::Arc::from(build_provider(&config));
            Some(argent_ai::BackgroundAssistant::new(provider, model))
        } else {
            None
        };
        let mut agent = match build_agent(
            &config,
            &cwd,
            &session_label,
            approval_tx.clone(),
            worker_abort.clone(),
            approval_ids,
            checkpoints.clone(),
            event_tx.clone(),
        ) {
            Ok(agent) => agent,
            Err(message) => {
                let _ = event_tx.send(UiEvent::Error(message));
                return;
            }
        };
        agent.set_steering(worker_steering);

        // No transcript file until there is something to put in it: an empty
        // session on disk is a placeholder, and the sidebar should not list it.
        let index = SessionIndex::new(config.session_dir.clone());
        let mut current = session.unwrap_or_default();
        let mut store: Option<SessionStore> = if current.is_file() {
            match SessionStore::open(current.clone()) {
                Ok(store) => Some(store),
                Err(e) => {
                    let _ = event_tx.send(UiEvent::Error(e.to_string()));
                    return;
                }
            }
        } else {
            None
        };
        if store.is_some() {
            checkpoints.retarget(&current);
        }
        let mut titled = false;
        // A resumed session starts with its conversation on screen, not with an
        // empty pane over a history the model can already see.
        if store.is_some() {
            match SessionStore::read_all(&current).map(|entries| entries_to_messages(&entries)) {
                Ok(messages) if !messages.is_empty() => {
                    let loaded = argent_ui_core::Transcript::from_messages(&messages);
                    let _ = event_tx.send(UiEvent::Loaded(loaded.entries().to_vec()));
                    argent_session::controller::open(&mut agent, messages);
                    titled = true;
                }
                _ => {}
            }
        }

        // The catalog knows the model's real window; the configured number is
        // only a fallback.
        let (context_max, models) = catalog(&config);
        let _ = event_tx.send(UiEvent::Ready {
            model: agent.model().to_string(),
            level: config.thinking_level.clone(),
            context_max,
            models,
            thinking: vec![
                "auto".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
                "max".to_string(),
            ],
            cwd: cwd.to_string_lossy().into_owned(),
            provider: config.provider.clone(),
            protocol: config.protocol.clone(),
            theme: config.theme.clone(),
        });
        // Empty sessions are left alone: with several workers, one session's
        // startup must never delete another tab's file before its first prompt.
        let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
        if !current.as_os_str().is_empty() {
            let _ = event_tx.send(UiEvent::Session {
                path: current.clone(),
            });
        }

        while let Ok(command) = command_rx.recv() {
            match command {
                Command::Prompt(text) => {
                    // The first prompt is what makes the session exist.
                    if store.is_none() {
                        let title = title_from(&text);
                        match index.create(&cwd, &title) {
                            Ok(path) => match SessionStore::open(path.clone()) {
                                Ok(created) => {
                                    store = Some(created);
                                    current = path.clone();
                                    titled = true;
                                    checkpoints.retarget(&current);
                                    let _ = event_tx.send(UiEvent::Session { path });
                                }
                                Err(e) => {
                                    let _ = event_tx.send(UiEvent::Error(e.to_string()));
                                    continue;
                                }
                            },
                            Err(e) => {
                                let _ = event_tx.send(UiEvent::Error(e.to_string()));
                                continue;
                            }
                        }
                    }
                    let Some(store_ref) = store.as_mut() else {
                        continue;
                    };
                    // The first message names the task. Rename before the run,
                    // not after: the sidebar should show the task's real name
                    // while the model is still thinking.
                    if !titled {
                        let title = title_from(&text);
                        if index.rename(&current, &title).is_ok() {
                            titled = true;
                            let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                        }
                    }
                    // Compact before the turn, not after: the model is the one
                    // paying for the history, and `before` below must describe
                    // the history that is about to be appended to.
                    if let Some(summary) = argent_session::controller::compact(
                        &mut agent,
                        context_max,
                        config.reserved_context_tokens,
                        config.keep_recent_tokens,
                    ) {
                        // Background intelligence rewrites the heuristic draft
                        // into a denser narrative; the agent keeps the tail
                        // byte-identical, so only the first message re-serializes.
                        let upgraded = background.as_ref().and_then(|b| b.summarize_for(&summary));
                        if let Some(better) = upgraded {
                            agent.replace_last_summary(&better);
                        }
                        let _ = event_tx.send(UiEvent::Agent(AgentEvent::Notice(
                            "context compacted".to_string(),
                        )));
                    }
                    let before = agent.history().len();
                    let mut sink = ChannelSink {
                        tx: event_tx.clone(),
                    };
                    let outcome = agent.run(&text, &mut sink);
                    // Persist before reporting: a crash after this point cannot
                    // lose what the operator already watched arrive.
                    persist(store_ref, agent.history(), before);
                    match outcome {
                        Ok(reason) => {
                            let usage = agent.usage();
                            let requests = agent.requests();
                            let cost = agent.usage_cost();
                            let _ = argent_session::usage::record(
                                &current,
                                agent.model(),
                                &usage,
                                requests,
                                cost,
                            );
                            let _ = event_tx.send(UiEvent::TurnDone {
                                reason,
                                usage,
                                requests,
                                cost,
                            });
                        }
                        Err(e) => {
                            let _ = event_tx.send(UiEvent::Error(e.to_string()));
                        }
                    }
                    let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                }
                Command::Abort => worker_abort.abort(),
                Command::Clear => agent.set_history(Vec::new()),
                Command::OpenSession(path) => match open_session(&path) {
                    Ok((messages, opened)) => {
                        let loaded = argent_ui_core::Transcript::from_messages(&messages);
                        // The window renders what it is given; without this it
                        // would show an empty pane over a loaded conversation.
                        let _ = event_tx.send(UiEvent::Loaded(loaded.entries().to_vec()));
                        // An empty session is a placeholder: its first prompt
                        // should name it. A session with history keeps its name.
                        titled = !messages.is_empty();
                        argent_session::controller::open(&mut agent, messages);
                        store = Some(opened);
                        // 之前漏了这一步：切到别的会话后，current 仍指向旧
                        // 文件，重命名、用量记账和删除都会作用在旧会话上。
                        current = path.clone();
                        checkpoints.retarget(&current);
                        let _ = event_tx.send(UiEvent::Session { path });
                        // Reading a file is not a model run, so the counters
                        // reset rather than carrying the previous session's.
                        let _ = event_tx.send(UiEvent::TurnDone {
                            reason: FinishReason::Complete,
                            usage: Usage::default(),
                            requests: 0,
                            cost: None,
                        });
                    }
                    Err(message) => {
                        let _ = event_tx.send(UiEvent::Error(message));
                    }
                },
                Command::NewSession => {
                    // A session file that never received a message is a
                    // placeholder from a run that failed before persisting;
                    // drop it. A session with messages stays on disk and in
                    // the sidebar: starting a new conversation must not delete
                    // the one the operator was reading.
                    if let Some(old) = store.take() {
                        drop(old);
                        if current.is_file() && argent_session::is_empty_transcript(&current) {
                            let _ = std::fs::remove_file(&current);
                        }
                    }
                    argent_session::controller::clear(&mut agent);
                    current = PathBuf::new();
                    titled = false;
                    checkpoints.detach();
                    let _ = event_tx.send(UiEvent::SessionCleared);
                }
                Command::RenameSession { path, title } => {
                    if let Err(e) = index.rename(&path, &title) {
                        let _ = event_tx.send(UiEvent::Error(e.to_string()));
                    }
                }
                Command::DeleteSession(path) => {
                    let deleting_current = path == current;
                    match std::fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            let _ = event_tx.send(UiEvent::Error(format!(
                                "cannot delete {}: {}",
                                path.display(),
                                e
                            )));
                        }
                    }
                    // 会话没了，它的检查点也没有再指回的入口。
                    argent_tools::checkpoint::remove_for_session(&path);
                    if deleting_current {
                        // No replacement file: the next prompt creates one.
                        argent_session::controller::clear(&mut agent);
                        store = None;
                        current = PathBuf::new();
                        titled = false;
                        checkpoints.detach();
                        let _ = event_tx.send(UiEvent::SessionCleared);
                    }
                }
                Command::Rewind { steps } => {
                    let (restored, errors) = checkpoints.rewind(steps);
                    if !errors.is_empty() {
                        let _ = event_tx.send(UiEvent::Error(format!(
                            "rewind failed: {}",
                            errors.join("; ")
                        )));
                    } else if restored.is_empty() {
                        let _ = event_tx.send(UiEvent::Agent(AgentEvent::Notice(
                            "nothing to rewind".to_string(),
                        )));
                    } else {
                        let mut listing = restored
                            .iter()
                            .take(20)
                            .map(|path| format!("- {}", path.display()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if restored.len() > 20 {
                            listing.push_str(&format!("\n- … ({} more)", restored.len() - 20));
                        }
                        let _ = event_tx.send(UiEvent::Agent(AgentEvent::Notice(format!(
                            "rewound {} run(s), restored {} file(s):\n{}",
                            steps,
                            restored.len(),
                            listing
                        ))));
                    }
                }
                Command::SetModel(model) => agent.set_model(model),
                Command::SetThinking(level) => agent.set_thinking(&level),
                Command::Shutdown => break,
            }
            let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
        }
    });

    Engine {
        commands: command_tx,
        events: event_rx,
        approvals: approval_rx,
        abort,
        steering,
    }
}

/// Load the configuration for a working directory and resolve the provider,
/// model, credential and profile that go with it.
///
/// Every frontend starts the same way; sharing this means a profile or a
/// settings file behaves identically in the native window and the terminal.
pub fn prepare_config(cwd: &Path) -> Result<Config, String> {
    prepare_config_with_profile(cwd, None)
}

/// The same, with a profile chosen by the caller rather than by settings.
///
/// The folder picker lets the operator pick a profile per task; that profile
/// takes precedence over `ARGENT_PROFILE` and the settings file.
pub fn prepare_config_with_profile(
    cwd: &Path,
    profile_override: Option<&str>,
) -> Result<Config, String> {
    let mut config = Config::load(cwd).map_err(|e| e.to_string())?;
    let settings = Settings::load(cwd).map_err(|e| e.to_string())?;
    let profiles = argent_profiles::ProfileRegistry::load(cwd, &settings).map_err(|e| e.to_string())?;
    let profile_name = profile_override
        .map(str::to_string)
        .or_else(|| {
            config
                .extra
                .get("profile")
                .and_then(Json::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            std::env::var("ARGENT_PROFILE")
                .ok()
                .filter(|name| !name.trim().is_empty())
        })
        .or_else(|| settings.string("profile").map(str::to_string));
    let profile = profile_name
        .as_deref()
        .and_then(|name| profiles.get(name))
        .cloned();
    if let Some(name) = &profile_name {
        config
            .extra
            .insert("profile".to_string(), Json::string(name.clone()));
    }
    let mut catalog = ProviderCatalog::builtin();
    catalog.merge(settings.raw());
    let provider = profile
        .as_ref()
        .and_then(|p| p.provider.as_deref())
        .or_else(|| settings.provider());
    let model = profile
        .as_ref()
        .and_then(|p| p.model.as_deref())
        .or_else(|| settings.model());
    let resolved = catalog
        .resolve(provider, model)
        .map_err(|e| e.to_string())?;
    if std::env::var_os("ARGENT_LOCALE").is_none() {
        if let Some(locale) = settings.string("locale") {
            std::env::set_var("ARGENT_LOCALE", locale);
        }
    }
    if std::env::var_os("ARGENT_BASE_URL").is_none() {
        config.base_url = resolved.provider.base_url.clone();
    }
    if std::env::var_os("ARGENT_MODEL").is_none() {
        config.model = resolved.model.id.clone();
    }
    // A `prices` block turns token counts into dollars without a flag.
    if let Some(price) = settings
        .get("prices")
        .and_then(|prices| prices.get(&config.model))
    {
        if config.price_input_per_mtok.is_none() {
            config.price_input_per_mtok = price
                .get("input")
                .and_then(Json::as_f64)
                .or_else(|| price.get("inputPerMtok").and_then(Json::as_f64));
        }
        if config.price_output_per_mtok.is_none() {
            config.price_output_per_mtok = price
                .get("output")
                .and_then(Json::as_f64)
                .or_else(|| price.get("outputPerMtok").and_then(Json::as_f64));
        }
    }
    if std::env::var_os("ARGENT_THINKING").is_none() {
        let thinking = profile
            .as_ref()
            .and_then(|p| p.thinking.clone())
            .or_else(|| settings.thinking().map(str::to_string));
        if let Some(thinking) = thinking {
            config.thinking_level = thinking;
        }
    }
    if settings.get("sessionDir").is_none() && std::env::var_os("ARGENT_SESSION_DIR").is_none() {
        config.session_dir = argent_config::Paths::new(cwd).sessions();
    }
    if settings.string("sessionBackend") == Some("sqlite")
        && std::env::var_os("ARGENT_SQLITE").is_none()
    {
        std::env::set_var(
            "ARGENT_SQLITE",
            argent_config::Paths::new(cwd)
                .storage()
                .join("cli")
                .join("db")
                .join("db.sqlite"),
        );
    }
    if resolved.model.context > 0 && settings.get("maxContextTokens").is_none() {
        config.max_context_tokens = resolved.model.context;
    }
    if resolved.model.max_output > 0 && settings.get("maxOutputTokens").is_none() {
        config.max_output_tokens = resolved.model.max_output;
    }
    config.supports_vision = resolved.model.vision;
    if std::env::var_os("ARGENT_APPROVAL").is_none() {
        let mode = profile
            .as_ref()
            .and_then(|p| p.approval.as_deref())
            .or_else(|| settings.approval_mode());
        if let Some(mode) = mode {
            if let Some(parsed) = argent_core::config::ApprovalMode::from_name(mode) {
                config.approval_mode = parsed;
            }
        }
    }
    config.provider = resolved.provider.id.clone();
    config.protocol = resolved.provider.protocol.name().to_string();
    config.api_key_env = resolved.provider.api_key_env.clone();
    if let Some(profile) = &profile {
        profile.apply_policy(&mut config);
    }
    if std::env::var_os("ARGENT_API_KEY").is_none() {
        let mut store = AuthStore::load().map_err(|e| e.to_string())?;
        if let Some(oauth) = &resolved.provider.oauth {
            let _ = argent_auth::oauth::refresh_if_needed(&mut store, &resolved.provider.id, oauth);
        }
        config.api_key = store.resolve_key(&resolved.provider, config.api_key.as_deref());
    }
    Ok(config)
}

/// The title a new session is filed under, from the request that created it.
fn title_from(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "新任务".to_string()
    } else {
        line.chars().take(60).collect()
    }
}

/// The rows the sidebar shows, newest first.
pub fn session_rows(index: &SessionIndex) -> Vec<SessionRow> {
    index
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|(path, info)| SessionRow {
            path,
            title: {
                let title = info.str_or("title", "");
                if title.is_empty() {
                    "新对话".to_string()
                } else {
                    title
                }
            },
            updated: info.str_or("modified_iso", ""),
            updated_local: argent_session::local_stamp(info.num_or("modified", 0.0) as u64),
            modified: info.num_or("modified", 0.0) as u64,
            cwd: info.str_or("cwd", ""),
        })
        .collect()
}

/// Delete empty session files older than `max_age`.
///
/// The session layer owns the rule; the engine keeps this alias so frontends
/// do not need to know which crate implements the sweep.
pub fn sweep_empty_sessions(
    index: &SessionIndex,
    keep: &[PathBuf],
    max_age: std::time::Duration,
) -> Vec<PathBuf> {
    argent_session::sweep_empty_sessions(index, keep, max_age)
}

/// Append the messages a turn added, one entry per message.
fn persist(store: &mut SessionStore, history: &[Message], from: usize) {
    for message in history.iter().skip(from) {
        if let Err(e) = store.append(&Entry::Message(message.clone())) {
            eprintln!("argent: cannot append to the transcript: {}", e);
            return;
        }
    }
}

/// Read a session file back into a conversation, and reopen it for appending.
fn open_session(path: &std::path::Path) -> Result<(Vec<Message>, SessionStore), String> {
    let entries = SessionStore::read_all(path).map_err(|e| e.to_string())?;
    let messages = entries_to_messages(&entries);
    let store = SessionStore::open(path.to_path_buf()).map_err(|e| e.to_string())?;
    Ok((messages, store))
}

struct ChannelSink {
    tx: Sender<UiEvent>,
}

impl EventSink for ChannelSink {
    fn emit(&mut self, event: AgentEvent) {
        let _ = self.tx.send(UiEvent::Agent(event));
    }
}

/// The selected model's capabilities, for the picker and the context bar.
fn model_info(config: &Config) -> argent_ai::ModelInfo {
    argent_ai::ModelInfo {
        id: config.model.clone(),
        display_name: config.model.clone(),
        context_length: config.max_context_tokens,
        max_output: config.max_output_tokens,
        supports_tools: true,
        supports_reasoning: true,
        // provider 模板声明的视觉能力；配置层不知道时为 true。
        supports_vision: config.supports_vision,
        reasoning_levels: vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
            "max".to_string(),
        ],
    }
}

/// Build the provider selected by the resolved protocol.
fn build_provider(config: &Config) -> Box<dyn Provider> {
    let api_key = config.api_key.clone().or_else(|| {
        config
            .api_key_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
    });
    match config.protocol.as_str() {
        "anthropic-messages" => {
            let mut provider = AnthropicConfig::new(config.base_url.clone())
                .with_max_tokens(config.max_output_tokens)
                .with_models(vec![model_info(config)]);
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            Box::new(AnthropicProvider::new(provider))
        }
        "openai-responses" => {
            let mut provider = ResponsesConfig::new(config.base_url.clone())
                .with_max_output_tokens(config.max_output_tokens)
                .with_models(vec![model_info(config)]);
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            Box::new(ResponsesProvider::new(provider))
        }
        _ => {
            let mut provider = OpenAiConfig::new(config.base_url.clone());
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            Box::new(OpenAiProvider::new(provider))
        }
    }
}

fn build_agent(
    config: &Config,
    cwd: &PathBuf,
    session_label: &str,
    approval_tx: Sender<Approval>,
    abort: AbortHandle,
    approval_ids: Arc<std::sync::atomic::AtomicU64>,
    checkpoints: CheckpointStore,
    event_tx: Sender<UiEvent>,
) -> Result<Agent, String> {
    let provider = build_provider(config);
    // 设置文件读一次：profile、插件启停、插件数据目录都从这里取。
    let settings = Settings::load(cwd).ok();
    let profile = settings.as_ref().and_then(|settings| {
        let name = config
            .extra
            .get("profile")
            .and_then(Json::as_str)
            .map(str::to_string)
            .or_else(|| settings.string("profile").map(str::to_string))?;
        argent_profiles::ProfileRegistry::load(cwd, settings)
            .ok()
            .and_then(|profiles| profiles.get(&name).cloned())
    });
    // pi's four tools; everything else is a plugin the settings file decides
    // to load.
    let mut registry = ToolRegistry::with_core();

    // 与 CLI 同一套装配规则：发现工作区插件包，按设置启用，再注册原生
    // out-of-tree 工具库。GUI 会话由此获得与终端一致的插件能力。
    let mut plugin_manager = argent_plugins::PluginManager::for_workspace(cwd);
    if let Some(settings) = &settings {
        plugin_manager.apply_state(settings);
    }
    registry.add_builtin_plugins();

    // Pure-Rust script plugins (same loader as the CLI) contribute tools
    // before the tool-search catalog and system prompt are built.
    #[cfg(feature = "script-plugins")]
    {
        let storage = settings
            .as_ref()
            .map(|s| s.paths().storage().to_path_buf())
            .unwrap_or_else(|| argent_config::Paths::new(cwd).storage().to_path_buf());
        let script_ctx = argent_tools::ToolCtx::new(cwd.clone());
        argent_plugins::load_script_plugins(
            &plugin_manager,
            &mut registry,
            &script_ctx,
            cwd,
            &storage,
            &mut |line| eprintln!("{line}"),
        );
    }

    for tool in argent_plugin::loader::load_all(cwd) {
        registry.register(tool);
    }
    registry.register_plugin_tool(argent_tools::plugin::CODEINTEL, Box::new(argent_lsp::CodeIntelTool));
    registry.register_plugin_tool(argent_tools::plugin::JOBS, Box::new(argent_jobs::JobsTool));
    // 先占位：装配完门禁后换成带工厂的真实现（见下方 replace_delegate）。
    registry.register_plugin_tool(
        argent_tools::plugin::DELEGATE,
        Box::new(DelegateTool::disabled()),
    );

    // 插件自带技能目录追加到默认搜索路径之后，项目技能仍然优先。
    let mut skill_roots = SkillRegistry::default_roots(cwd);
    skill_roots.extend(plugin_manager.skill_dirs());
    let skills = std::sync::Arc::new(SkillRegistry::discover(&skill_roots));
    registry.register_plugin_tool(
        argent_tools::plugin::SKILLS,
        Box::new(SkillTool::new(std::sync::Arc::clone(&skills))),
    );
    for tool in argent_tools::skill_script_tools(&skills) {
        registry.register_plugin_tool(argent_tools::plugin::SKILLS, tool);
    }

    // MCP servers contribute tools under `mcp__<server>__<tool>`; they stay
    // unlisted and are reachable through tool_search. 插件声明的服务器与
    // mcp.json 合并后一次连接，CLI 与 GUI 共用 argent_plugins::runtime 的展开规则。
    let mut mcp_config = argent_mcp::McpConfig::load(cwd).map_err(|e| e.to_string())?;
    let storage = settings
        .as_ref()
        .map(|settings| settings.paths().storage().to_path_buf())
        .unwrap_or_else(|| argent_config::Paths::new(cwd).storage().to_path_buf());
    argent_plugins::add_mcp_servers(&mut mcp_config, &plugin_manager, cwd, &storage);
    let (_clients, mcp_tools) = mcp_config.connect_all();
    for tool in mcp_tools {
        registry.register(tool);
    }

    let mut requested = config.plugins.clone();
    if requested.is_empty() {
        requested.extend(
            argent_tools::plugin::DEFAULT_PLUGINS
                .iter()
                .map(|name| name.to_string()),
        );
    }
    registry.enable_plugins(&requested);

    let read_only = profile.as_ref().map(|p| p.read_only).unwrap_or(false);
    if !config.allowed_tools.is_empty() || !config.excluded_tools.is_empty() || read_only {
        registry = registry.filter(&config.allowed_tools, &config.excluded_tools, read_only);
    }

    let promotions: argent_tools::Promotions =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::new()));
    let catalog = std::sync::Arc::new(
        registry
            .names()
            .iter()
            .filter_map(|name| {
                registry.get(name).map(|tool| {
                    CatalogEntry::from_schema(tool.schema()).in_plugin(registry.tool_plugin(name))
                })
            })
            .collect::<Vec<_>>(),
    );
    let visible = std::sync::Arc::new(registry.enabled_names());
    let plugins = std::sync::Arc::new(registry.plugin_infos());
    registry.register_extension(Box::new(ToolSearchTool::new(
        catalog,
        visible,
        plugins,
        std::sync::Arc::clone(&promotions),
    )));

    let mut tool_ctx = ToolCtx::new(cwd.clone());
    tool_ctx.timeout_secs = config.tool_timeout_secs;
    tool_ctx.confirm_delete = config.confirm_delete;
    tool_ctx.auto_approve_bash = config.auto_approve_bash;
    tool_ctx.checkpoints = Some(checkpoints.clone());
    tool_ctx
        .vars
        .insert("ask_channel".to_string(), "terminal".to_string());

    let mut agent_config = AgentConfig::new(config.model.clone());
    agent_config.reasoning_effort = Some(config.thinking_level.clone());
    agent_config.promoted = Some(std::sync::Arc::clone(&promotions));
    agent_config.approval_scope = ApprovalScope::DestructiveOnly;
    if let Some(settings) = &settings {
        if let Some(mode) = settings.string("followUpMode") {
            agent_config.follow_up_mode = argent_agent::FollowUpMode::parse(mode);
        }
        if let Some(policy) = settings.string("cacheWarming") {
            agent_config.cache_warming = argent_agent::CacheWarming::parse(policy);
        }
    }
    // 每次用户 prompt 开始一轮运行；检查点按这个粒度回滚。
    {
        let checkpoints = checkpoints.clone();
        agent_config.on_run_start = Some(std::sync::Arc::new(move || {
            checkpoints.begin_run();
        }));
    }

    // 生命周期钩子与 CLI 共用同一套信任检查：未授权的文档跳过并以
    // Notice 告知操作者，绝不静默执行。
    let hook_config = argent_plugins::load_trusted_hooks(
        &plugin_manager,
        cwd,
        &storage,
        &argent_config::Paths::new(cwd).hook_trust(),
        &mut |line| {
            let _ = event_tx.send(UiEvent::Agent(AgentEvent::Notice(line.to_string())));
        },
    );
    if !hook_config.is_empty() {
        let config_for_pre = std::sync::Arc::new(hook_config.clone());
        let cwd_for_pre = cwd.clone();
        let event_for_pre = event_tx.clone();
        agent_config.pre_tool = Some(std::sync::Arc::new(move |name, args| {
            let ctx = argent_hooks::HookContext {
                cwd: cwd_for_pre.clone(),
                tool_name: Some(name.to_string()),
                tool_input: Some(args.dump()),
                ..argent_hooks::HookContext::default()
            };
            spawn_async_hooks(
                &config_for_pre,
                argent_hooks::HookEvent::PreToolUse,
                &ctx,
                &event_for_pre,
            );
            let mut block = None;
            for hook in config_for_pre
                .for_event(argent_hooks::HookEvent::PreToolUse, Some(name))
                .into_iter()
                .filter(|hook| !hook.async_hook)
            {
                let outcome = argent_hooks::run_command(argent_hooks::HookEvent::PreToolUse, hook, &ctx);
                if !outcome.success {
                    let _ = event_for_pre.send(UiEvent::Agent(AgentEvent::Notice(format!(
                        "PreToolUse hook failed for {}: {}",
                        name,
                        clip_hook_output(&outcome.stdout, &outcome.stderr)
                    ))));
                    if hook.blocks_on_failure() && block.is_none() {
                        block = Some(format!(
                            "A PreToolUse hook blocked this {} call: {}",
                            name,
                            clip_hook_output(&outcome.stdout, &outcome.stderr)
                        ));
                    }
                }
            }
            block
        }));

        let config_for_post = std::sync::Arc::new(hook_config.clone());
        let cwd_for_post = cwd.clone();
        let event_for_post = event_tx.clone();
        agent_config.post_tool = Some(std::sync::Arc::new(move |name, args, is_error| {
            let event = if is_error {
                argent_hooks::HookEvent::PostToolUseFailure
            } else {
                argent_hooks::HookEvent::PostToolUse
            };
            let ctx = argent_hooks::HookContext {
                cwd: cwd_for_post.clone(),
                tool_name: Some(name.to_string()),
                tool_input: Some(args.dump()),
                ..argent_hooks::HookContext::default()
            };
            spawn_async_hooks(&config_for_post, event, &ctx, &event_for_post);
            for hook in config_for_post
                .for_event(event, Some(name))
                .into_iter()
                .filter(|hook| !hook.async_hook)
            {
                let outcome = argent_hooks::run_command(event, hook, &ctx);
                if !outcome.success {
                    let _ = event_for_post.send(UiEvent::Agent(AgentEvent::Notice(format!(
                        "{} hook failed for {}: {}",
                        event.name(),
                        name,
                        clip_hook_output(&outcome.stdout, &outcome.stderr)
                    ))));
                }
            }
        }));
    }
    if let Some(profile) = &profile {
        if let Some(turns) = profile.max_turns {
            agent_config.max_turns = turns;
        }
    }
    if config.max_total_tokens.is_some()
        || config.max_cost_usd.is_some()
        || config.max_wall_seconds.is_some()
    {
        agent_config.budget = Some(argent_agent::Budget {
            max_total_tokens: config.max_total_tokens,
            max_cost_usd: config.max_cost_usd,
            price_input_per_mtok: config.price_input_per_mtok.unwrap_or(0.0),
            price_output_per_mtok: config.price_output_per_mtok.unwrap_or(0.0),
            max_wall_seconds: config.max_wall_seconds,
        });
    }
    if config.approval_mode != ApprovalMode::Never {
        // The dialog runs on the UI thread; this closure blocks the worker
        // until the operator answers, which is exactly what approval means.
        let approval_tx = approval_tx.clone();
        agent_config.approve = Some(std::sync::Arc::new(move |name: &str, args: &Json| {
            let (reply_tx, reply_rx) = channel::<bool>();
            let request = Approval {
                id: approval_ids.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                name: name.to_string(),
                // 桌面端用 <pre> 展示，直接给 diff/命令，而不是转义 JSON。
                arguments: argent_tools::approval_preview(name, args),
                reply: reply_tx,
            };
            if approval_tx.send(request).is_err() {
                return false;
            }
            reply_rx
                .recv_timeout(std::time::Duration::from_secs(600))
                .unwrap_or(false)
        }));
    }

    // The policy gate is a hard gate: it answers on its own, before the
    // approval dialog, so a rule the operator wrote into the settings file is
    // not something a mis-click waves through.
    if let Some(settings) = &settings {
        let policy = argent_security::Policy::from_settings(settings);
        if !policy.is_empty() {
            let cwd = cwd.clone();
            agent_config.policy = Some(std::sync::Arc::new(move |name, args| {
                policy.check_tool(name, args, &cwd)
            }));
        }
        // The audit trail answers "what did this agent do to my machine".
        let audit = std::sync::Arc::new(argent_security::AuditLog::new(settings.paths().storage()));
        let session_label = session_label.to_string();
        agent_config.audit = Some(std::sync::Arc::new(move |name, args, decision, reason| {
            let entry = argent_security::AuditEntry {
                timestamp: argent_session::now_iso(),
                session: session_label.clone(),
                tool: name.to_string(),
                decision: decision.to_string(),
                reason,
                arguments: Some(argent_security::redact_json(args).dump()),
            };
            audit.record(&entry);
        }));
    }

    // 子代理：内置角色 + 工作区/插件角色目录；工厂继承父代理的全部门禁，
    // delegate 不能成为绕过策略、钩子或审批的通道。进度以 Notice 转发，
    // 操作者在时间线里能看到每个孩子在调什么工具、成功与否。
    let mut roles = RoleRegistry::new();
    for role in builtin_roles() {
        roles.register(role);
    }
    {
        let mut role_dirs = vec![cwd.join(".argent").join("roles")];
        if let Some(home) = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
        {
            role_dirs.push(home.join(".argent").join("roles"));
        }
        role_dirs.extend(plugin_manager.agent_dirs());
        for dir in role_dirs {
            if let Err(e) = roles.load_dir(&dir) {
                let _ = event_tx.send(UiEvent::Agent(AgentEvent::Notice(format!(
                    "could not read {}: {}",
                    dir.display(),
                    e
                ))));
            }
        }
    }
    let roles = std::sync::Arc::new(std::sync::Mutex::new(roles));
    let factory = ChildFactory {
        base_url: config.base_url.clone(),
        api_key: config.api_key.clone(),
        protocol: config.protocol.clone(),
        model: config.model.clone(),
        thinking: config.thinking_level.clone(),
        models: vec![model_info(config)],
        roles: std::sync::Arc::clone(&roles),
        parent_registry: registry.subset(&registry.names()),
        cwd: cwd.clone(),
        policy: agent_config.policy.clone(),
        audit: agent_config.audit.clone(),
        pre_tool: agent_config.pre_tool.clone(),
        post_tool: agent_config.post_tool.clone(),
        approve: agent_config.approve.clone(),
        approval_scope: agent_config.approval_scope,
        progress: Some(std::sync::Arc::new(move |event: argent_tools::SubagentEvent| {
            let _ = event_tx.send(UiEvent::Agent(AgentEvent::Notice(event.describe())));
        })),
        // 子代理的写入记在父运行名下，回滚一次撤掉整轮。
        checkpoints: Some(checkpoints.clone()),
        promotions: Some(std::sync::Arc::clone(&promotions)),
    };
    let delegate_handle = DelegateHandle::new(std::sync::Arc::new(move |batch: DelegateBatch| {
        factory.run_batch(batch)
    }));
    {
        // 换上带工厂的 delegate；插件的启停状态不变——接线不能偷偷启用
        // 操作者关掉的插件。
        let enabled = registry.is_plugin_enabled(argent_tools::plugin::DELEGATE);
        registry.register_plugin_tool(
            argent_tools::plugin::DELEGATE,
            Box::new(DelegateTool::with_handle(delegate_handle)),
        );
        if enabled {
            registry.enable_plugin(argent_tools::plugin::DELEGATE);
        }
    }

    let registry = std::sync::Arc::new(registry);

    let mut notes = String::new();
    // 角色目录放在系统提示尾部块的最前面，与 CLI 的顺序一致。
    if let Ok(guard) = roles.lock() {
        if !guard.roles().is_empty() {
            notes.push_str("Subagents you can delegate to with the delegate tool:\n");
            for role in guard.roles() {
                let read_only = if role.read_only { " (read-only)" } else { "" };
                notes.push_str(&format!(
                    "- {}{}: {}\n",
                    role.name, read_only, role.description
                ));
            }
            notes.push('\n');
        }
    }
    if registry.is_plugin_enabled(argent_tools::plugin::SKILLS) {
        let skill_index = skills.index_text();
        if !skill_index.is_empty() {
            notes.push_str(&skill_index);
        }
    }
    let plugin_note = registry.plugin_note();
    if !plugin_note.is_empty() {
        if !notes.is_empty() {
            notes.push('\n');
        }
        notes.push_str(&plugin_note);
    }
    let context = argent_agent::session_context(cwd);
    if !context.is_empty() {
        if !notes.is_empty() {
            notes.push('\n');
        }
        notes.push_str(&context);
    }
    let hidden = registry.hidden_names();
    if !hidden.is_empty() {
        if !notes.is_empty() {
            notes.push('\n');
        }
        notes.push_str(&format!(
            "Tools registered but not listed (call tool_search with a query, then \
             tool_search with {{\"enable\": \"<name>\"}} to use one): {}\n",
            hidden.join(", ")
        ));
    }
    if let Some(profile) = &profile {
        let profile_text = profile.prompt_note(cwd);
        if !profile_text.is_empty() {
            if !notes.is_empty() {
                notes.push('\n');
            }
            notes.push_str(&profile_text);
        }
    }
    let system_prompt = build_system_prompt(cwd, &registry.prompt_index(), &notes);

    let mut agent = Agent::new(provider, registry, agent_config, tool_ctx, system_prompt);
    agent.set_abort(abort);
    Ok(agent)
}

/// 截取 hook 输出用于显示：stderr 优先，去换行，限 300 字符。
///
/// 与 CLI 的同名逻辑保持一致，桌面端通知面板空间有限。
/// Schedule async hooks and report their output on `event_tx` once they
/// finish. Never blocks the tool call that triggered them.
fn spawn_async_hooks(
    config: &argent_hooks::HookConfig,
    event: argent_hooks::HookEvent,
    ctx: &argent_hooks::HookContext,
    event_tx: &std::sync::mpsc::Sender<UiEvent>,
) {
    let run = argent_hooks::run_async(config, event, ctx);
    if run.is_empty() {
        return;
    }
    let tx = event_tx.clone();
    std::thread::spawn(move || {
        for outcome in run.collect() {
            let _ = tx.send(UiEvent::Agent(AgentEvent::Notice(outcome.as_notice())));
        }
    });
}

fn clip_hook_output(stdout: &str, stderr: &str) -> String {
    let text = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    let mut out: String = text.chars().take(300).collect();
    if text.chars().count() > 300 {
        out.push('…');
    }
    out.replace(['\n', '\r'], " ")
}

/// The provider's model list, plus the window of the configured model.
///
/// One catalog call answers both questions: the picker needs the ids, and the
/// context bar needs the real window rather than the configured default.
pub fn catalog(config: &Config) -> (usize, Vec<(String, usize)>) {
    let provider = build_provider(config);
    let entries: Vec<(String, usize)> = match provider.models() {
        Ok(models) => models
            .into_iter()
            .map(|info| (info.id, info.context_length))
            .collect(),
        Err(_) => Vec::new(),
    };
    let model_window = entries
        .iter()
        .find(|(id, _)| id == &config.model)
        .map(|(_, length)| *length);
    let context = argent_session::controller::context_window(
        config.max_context_tokens,
        model_window,
    );
    (context, entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_message_free_transcript_counts_as_empty() {
        let dir = std::env::temp_dir().join(format!("argent-ui-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let index = SessionIndex::new(dir.clone());
        let path = index.create(&dir, "test").expect("create");
        // A file that exists but holds no message: droppable.
        assert!(argent_session::is_empty_transcript(&path));

        // One message makes it a conversation: never droppable.
        let mut store = SessionStore::open(path.clone()).expect("open");
        store
            .append(&Entry::Message(Message::assistant("hello")))
            .expect("append");
        assert!(!argent_session::is_empty_transcript(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod session_worker_tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    #[test]
    fn two_sessions_get_independent_workers() {
        let dir = std::env::temp_dir().join(format!("argent-engine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            session_dir: dir.clone(),
            ..Config::default()
        };
        let index = SessionIndex::new(dir.clone());
        let first = index.create(&dir, "first").expect("first");
        let second = index.create(&dir, "second").expect("second");
        let ids = Arc::new(AtomicU64::new(1));

        let one = start_session(config.clone(), dir.clone(), Some(first), Arc::clone(&ids));
        let two = start_session(config.clone(), dir.clone(), Some(second), Arc::clone(&ids));

        // Each worker builds its own agent and announces itself.
        for _ in 0..2 {
            let event = one
                .events
                .recv_timeout(Duration::from_secs(10))
                .expect("first ready");
            assert!(matches!(
                event,
                UiEvent::Ready { .. } | UiEvent::Sessions(_) | UiEvent::Session { .. }
            ));
            let event = two
                .events
                .recv_timeout(Duration::from_secs(10))
                .expect("second ready");
            assert!(matches!(
                event,
                UiEvent::Ready { .. } | UiEvent::Sessions(_) | UiEvent::Session { .. }
            ));
        }

        let _ = one.commands.send(Command::Shutdown);
        let _ = two.commands.send(Command::Shutdown);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewind_without_recorded_runs_is_reported_as_a_notice() {
        let dir = std::env::temp_dir().join(format!("argent-engine-rewind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config = Config {
            session_dir: dir.clone(),
            ..Config::default()
        };
        let engine = start_session(
            config,
            dir.clone(),
            None,
            Arc::new(AtomicU64::new(1)),
        );
        let _ = engine.commands.send(Command::Rewind { steps: 1 });

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut saw = false;
        while std::time::Instant::now() < deadline {
            match engine.events.recv_timeout(Duration::from_millis(500)) {
                Ok(UiEvent::Agent(AgentEvent::Notice(text)))
                    if text.contains("nothing to rewind") =>
                {
                    saw = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        assert!(saw, "the worker must report an empty rewind");
        let _ = engine.commands.send(Command::Shutdown);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
