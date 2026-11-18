//! Tauri commands and the one worker that owns the Rust agent.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use argent_automation::{Action, AutomationStore, Schedule};
use argent_engine::{prepare_config, start_session, Engine, UiEvent};
use argent_ui_core::Command;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

/// Tracks pending approvals and routes the frontend's answer back to the
/// worker that is blocked waiting for it.
///
/// Kept separate from the Tauri `AppHandle` so the whole bridge can be tested
/// without a WebView: a click on "允许"/"拒绝" is just `answer(id, bool)`.
struct ApprovalBridge {
    pending: Mutex<HashMap<u64, Sender<bool>>>,
    /// Tools the operator chose to allow for the rest of this tab.
    allowed: Mutex<HashSet<String>>,
}

impl ApprovalBridge {
    fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            allowed: Mutex::new(HashSet::new()),
        }
    }

    fn allow_tool(&self, name: &str) {
        if let Ok(mut allowed) = self.allowed.lock() {
            allowed.insert(name.to_string());
        }
    }

    fn is_allowed(&self, name: &str) -> bool {
        self.allowed
            .lock()
            .map(|allowed| allowed.contains(name))
            .unwrap_or(false)
    }

    fn insert(&self, id: u64, reply: Sender<bool>) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, reply);
        }
    }

    fn answer(&self, id: u64, allow: bool) -> Result<(), String> {
        let reply = self
            .pending
            .lock()
            .map_err(|_| "approvals lock poisoned".to_string())?
            .remove(&id);
        match reply {
            Some(reply) => reply
                .send(allow)
                .map_err(|_| "approval receiver is gone".to_string()),
            None => Err("approval is no longer pending".to_string()),
        }
    }
}

/// One tab: its own agent worker, approval bridge and transcript state.
struct SessionHandle {
    engine: Mutex<Engine>,
    approvals: ApprovalBridge,
}

struct Shared {
    session_dir: std::path::PathBuf,
    sessions: Mutex<HashMap<String, Arc<SessionHandle>>>,
    active: Mutex<String>,
    ready: Mutex<HashMap<String, Value>>,
    /// Shared across tabs so approval ids stay unique.
    approval_ids: Arc<AtomicU64>,
    next_tab: AtomicU64,
}

/// Tauri-managed handle to all agent workers.
#[derive(Clone)]
pub struct Backend {
    shared: Arc<Shared>,
}

/// Poll one session's channels and tag every event with its tab id so the UI
/// can route background turns to the right tab.
fn spawn_session(app: AppHandle, shared: Arc<Shared>, sid: String, handle: Arc<SessionHandle>) {
    thread::spawn(move || loop {
        let mut events = Vec::new();
        {
            let Ok(engine) = handle.engine.lock() else {
                return;
            };
            match engine.events.recv_timeout(Duration::from_millis(25)) {
                Ok(first) => {
                    events.push(first);
                    while let Ok(event) = engine.events.try_recv() {
                        events.push(event);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            while let Ok(approval) = engine.approvals.try_recv() {
                let id = approval.id;
                if handle.approvals.is_allowed(&approval.name) {
                    let _ = approval.reply.send(true);
                    continue;
                }
                handle.approvals.insert(id, approval.reply);
                let _ = app.emit(
                    "argent://event",
                    json!({
                        "kind": "approval",
                        "sid": sid,
                        "id": id,
                        "name": approval.name,
                        "arguments": approval.arguments,
                    }),
                );
            }
        }
        for event in events {
            let mut payload = map_event(event);
            if payload["kind"] == "ready" {
                if let Ok(mut ready) = shared.ready.lock() {
                    ready.insert(sid.clone(), payload.clone());
                }
            }
            payload["sid"] = json!(sid);
            let _ = app.emit("argent://event", payload);
        }
    });
}

impl Backend {
    pub fn start(app: AppHandle) -> Result<Self, String> {
        let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        let config = prepare_config(&cwd)?;
        let shared = Arc::new(Shared {
            session_dir: config.session_dir.clone(),
            sessions: Mutex::new(HashMap::new()),
            active: Mutex::new("1".to_string()),
            ready: Mutex::new(HashMap::new()),
            approval_ids: Arc::new(AtomicU64::new(1)),
            next_tab: AtomicU64::new(2),
        });
        let engine = start_session(config, cwd, None, Arc::clone(&shared.approval_ids));
        let handle = Arc::new(SessionHandle {
            engine: Mutex::new(engine),
            approvals: ApprovalBridge::new(),
        });
        shared
            .sessions
            .lock()
            .expect("sessions poisoned")
            .insert("1".to_string(), Arc::clone(&handle));
        spawn_session(app, Arc::clone(&shared), "1".to_string(), handle);
        Ok(Self { shared })
    }

    /// Called when the window closes so every worker can flush its session.
    pub fn shutdown_all(&self) {
        let Ok(sessions) = self.shared.sessions.lock() else {
            return;
        };
        for handle in sessions.values() {
            if let Ok(engine) = handle.engine.lock() {
                let _ = engine.commands.send(Command::Shutdown);
            }
        }
    }
}

fn handle_for(state: &State<'_, Backend>, sid: &str) -> Result<Arc<SessionHandle>, String> {
    let id = if sid.is_empty() {
        state
            .shared
            .active
            .lock()
            .map_err(|_| "active lock poisoned".to_string())?
            .clone()
    } else {
        sid.to_string()
    };
    state
        .shared
        .sessions
        .lock()
        .map_err(|_| "sessions lock poisoned".to_string())?
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("session {} not found", id))
}

fn map_event(event: UiEvent) -> Value {
    match event {
        UiEvent::Ready {
            model,
            level,
            context_max,
            models,
            thinking,
            cwd,
            provider,
            protocol,
            theme,
        } => json!({
            "kind": "ready",
            "model": model,
            "level": level,
            "contextMax": context_max,
            "models": models
                .into_iter()
                .map(|(id, window)| json!({ "id": id, "contextLength": window }))
                .collect::<Vec<_>>(),
            "thinking": thinking,
            "cwd": cwd,
            "provider": provider,
            "protocol": protocol,
            "theme": theme,
        }),
        UiEvent::Agent(event) => json!({
            "kind": "agent",
            "event": map_agent_event(event),
        }),
        UiEvent::TurnDone {
            reason,
            usage,
            requests,
            cost,
        } => json!({
            "kind": "turn_done",
            "reason": reason.describe(),
            "requests": requests,
            "cost": cost,
            "priced": cost.is_some(),
            "promptTokens": usage.prompt_tokens,
            "completionTokens": usage.completion_tokens,
            "totalTokens": usage.total_tokens,
            "cachedTokens": usage.cached_tokens,
        }),
        UiEvent::Error(message) => json!({ "kind": "error", "message": message }),
        UiEvent::Session { path } => json!({
            "kind": "session",
            "path": path.to_string_lossy(),
        }),
        UiEvent::SessionCleared => json!({ "kind": "session_cleared" }),
        UiEvent::Loaded(entries) => json!({
            "kind": "loaded",
            "entries": entries.into_iter().map(map_entry).collect::<Vec<_>>(),
        }),
        UiEvent::Sessions(rows) => json!({
            "kind": "sessions",
            "sessions": rows.into_iter().map(|row| json!({
                "path": row.path.to_string_lossy(),
                "title": row.title,
                "updated": row.updated_local,
                "cwd": row.cwd,
            })).collect::<Vec<_>>(),
        }),
    }
}

/// One replayed transcript row. The frontend has no access to Rust enums, so
/// every variant is flattened to a `kind` tag plus plain fields.
fn map_entry(entry: argent_ui_core::Entry) -> Value {
    use argent_ui_core::{Entry, NoticeKind, ToolState};

    match entry {
        Entry::User(text) => json!({ "kind": "user", "text": text }),
        Entry::Assistant(text) => json!({ "kind": "assistant", "text": text }),
        Entry::Thinking(text) => json!({ "kind": "thinking", "text": text }),
        Entry::Notice(level, text) => json!({
            "kind": "notice",
            "level": match level {
                NoticeKind::Info => "info",
                NoticeKind::Warning => "warning",
                NoticeKind::Error => "error",
            },
            "text": text,
        }),
        Entry::Tool(run) => {
            let (state, duration_ms) = match run.state {
                ToolState::Running => ("running", None),
                ToolState::Ok { ms } => ("ok", Some(ms)),
                ToolState::Failed { ms } => ("error", Some(ms)),
            };
            json!({
                "kind": "tool",
                "callId": run.call_id,
                "name": run.name,
                "arguments": run.arguments,
                "output": run.output,
                "state": state,
                "durationMs": duration_ms,
            })
        }
    }
}

fn map_agent_event(event: argent_agent::AgentEvent) -> Value {
    use argent_agent::AgentEvent;
    match event {
        AgentEvent::TurnStart { index } => json!({ "type": "turn_start", "index": index }),
        AgentEvent::AssistantText(text) => json!({ "type": "assistant_text", "text": text }),
        AgentEvent::Thinking(text) => json!({ "type": "thinking", "text": text }),
        AgentEvent::ToolRequested(call) => json!({
            "type": "tool_requested",
            "id": call.id,
            "name": call.name,
            "arguments": call.arguments,
        }),
        AgentEvent::ToolFinished {
            call_id,
            name,
            content,
            is_error,
            duration_ms,
        } => json!({
            "type": "tool_finished",
            "id": call_id,
            "name": name,
            "output": content,
            "isError": is_error,
            "durationMs": duration_ms,
        }),
        AgentEvent::TurnEnd { index } => json!({ "type": "turn_end", "index": index }),
        AgentEvent::Finished { reason } => json!({
            "type": "finished",
            "reason": reason.describe(),
        }),
        AgentEvent::Notice(text) => json!({ "type": "notice", "text": text }),
        AgentEvent::Aborted => json!({ "type": "aborted" }),
    }
}

#[tauri::command]
pub fn get_state(state: State<'_, Backend>) -> Value {
    let sid = state.shared.active.lock().expect("active poisoned").clone();
    let ready = state
        .shared
        .ready
        .lock()
        .expect("ready poisoned")
        .get(&sid)
        .cloned();
    json!({ "sid": sid, "ready": ready })
}

#[tauri::command]
pub fn new_tab(state: State<'_, Backend>, app: AppHandle) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let config = prepare_config(&cwd)?;
    let sid = state.shared.next_tab.fetch_add(1, Ordering::Relaxed).to_string();
    let engine = start_session(config, cwd, None, Arc::clone(&state.shared.approval_ids));
    let handle = Arc::new(SessionHandle {
        engine: Mutex::new(engine),
        approvals: ApprovalBridge::new(),
    });
    state
        .shared
        .sessions
        .lock()
        .map_err(|_| "sessions lock poisoned".to_string())?
        .insert(sid.clone(), Arc::clone(&handle));
    *state
        .shared
        .active
        .lock()
        .map_err(|_| "active lock poisoned".to_string())? = sid.clone();
    spawn_session(app, Arc::clone(&state.shared), sid.clone(), handle);
    Ok(sid)
}

#[tauri::command]
pub fn activate_tab(state: State<'_, Backend>, sid: String) -> Result<(), String> {
    let exists = state
        .shared
        .sessions
        .lock()
        .map_err(|_| "sessions lock poisoned".to_string())?
        .contains_key(&sid);
    if !exists {
        return Err(format!("session {} not found", sid));
    }
    *state
        .shared
        .active
        .lock()
        .map_err(|_| "active lock poisoned".to_string())? = sid;
    Ok(())
}

#[tauri::command]
pub fn close_tab(state: State<'_, Backend>, sid: String) -> Result<(), String> {
    let handle = {
        let mut sessions = state
            .shared
            .sessions
            .lock()
            .map_err(|_| "sessions lock poisoned".to_string())?;
        if sessions.len() == 1 {
            return Err("cannot close the last tab".to_string());
        }
        sessions
            .remove(&sid)
            .ok_or_else(|| format!("session {} not found", sid))?
    };
    let _ = handle
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::Shutdown);
    let mut active = state
        .shared
        .active
        .lock()
        .map_err(|_| "active lock poisoned".to_string())?;
    if *active == sid {
        *active = state
            .shared
            .sessions
            .lock()
            .map_err(|_| "sessions lock poisoned".to_string())?
            .keys()
            .next()
            .cloned()
            .unwrap_or_default();
    }
    drop(active);
    state
        .shared
        .ready
        .lock()
        .map_err(|_| "ready lock poisoned".to_string())?
        .remove(&sid);
    Ok(())
}

#[tauri::command]
pub fn send_prompt(state: State<'_, Backend>, sid: String, text: String) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::Prompt(text))
        .map_err(|_| "agent worker is gone".to_string())
}

/// Queue a mid-turn correction. The agent drains this queue at the next tool
/// boundary, the same way the TUI and native window do.
fn enqueue_steering(queue: &Mutex<VecDeque<String>>, text: String) -> Result<(), String> {
    queue
        .lock()
        .map_err(|_| "steering queue poisoned".to_string())?
        .push_back(text);
    Ok(())
}

#[tauri::command]
pub fn steer(state: State<'_, Backend>, sid: String, text: String) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("steering text is empty".to_string());
    }
    let handle = handle_for(&state, &sid)?;
    let engine = handle
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?;
    enqueue_steering(&engine.steering, text)
}

#[tauri::command]
pub fn abort(state: State<'_, Backend>, sid: String) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::Abort)
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn new_session(state: State<'_, Backend>, sid: String) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::NewSession)
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn answer_approval(
    state: State<'_, Backend>,
    sid: String,
    id: u64,
    allow: bool,
) -> Result<(), String> {
    handle_for(&state, &sid)?.approvals.answer(id, allow)
}

#[tauri::command]
pub fn allow_tool(state: State<'_, Backend>, sid: String, name: String) -> Result<(), String> {
    handle_for(&state, &sid)?.approvals.allow_tool(&name);
    Ok(())
}

#[tauri::command]
pub fn open_session(state: State<'_, Backend>, sid: String, path: String) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::OpenSession(std::path::PathBuf::from(path)))
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn rename_session(
    state: State<'_, Backend>,
    sid: String,
    path: String,
    title: String,
) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::RenameSession {
            path: std::path::PathBuf::from(path),
            title,
        })
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn compact(state: State<'_, Backend>, sid: String) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::Compact)
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn rewind(state: State<'_, Backend>, sid: String, steps: u64) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::Rewind { steps })
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn set_model(state: State<'_, Backend>, sid: String, model: String) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::SetModel(model))
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn set_thinking(state: State<'_, Backend>, sid: String, level: String) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::SetThinking(level))
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn delete_session(state: State<'_, Backend>, sid: String, path: String) -> Result<(), String> {
    handle_for(&state, &sid)?
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::DeleteSession(std::path::PathBuf::from(path)))
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn usage_summary() -> Result<Value, String> {
    let records = argent_session::usage::read();
    let summary = argent_session::usage::summary(&records, 0);
    Ok(json!({
        "turns": summary.turns,
        "requests": summary.requests,
        "warmRequests": summary.warm_requests,
        "promptTokens": summary.prompt_tokens,
        "completionTokens": summary.completion_tokens,
        "cachedTokens": summary.cached_tokens,
        "missedTokens": summary.missed_tokens,
        "cacheHitPercent": summary.cache_hit_percent(),
        "cost": summary.cost,
        "models": summary.by_model.iter().map(|(name, usage)| json!({
            "name": name,
            "turns": usage.turns,
            "promptTokens": usage.prompt_tokens,
            "cost": usage.cost,
        })).collect::<Vec<_>>(),
    }))
}

fn config_root() -> std::path::PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".argent")
}

fn list_markdown_names(dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|value| value.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("md"))
                .unwrap_or(false)
        })
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

#[tauri::command]
pub fn list_extensions() -> Result<Value, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let global = config_root();
    let mut skills = list_markdown_names(&global.join("skills"));
    skills.extend(list_markdown_names(&cwd.join(".argent").join("skills")));
    let mut commands = list_markdown_names(&global.join("commands"));
    commands.extend(list_markdown_names(&cwd.join(".argent").join("commands")));

    let mut hooks = Vec::new();
    for path in [global.join("hooks.json"), cwd.join(".argent").join("hooks.json")] {
        let Ok(body) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<Value>(&body) {
            if let Some(map) = value.pointer("/hooks").and_then(Value::as_object) {
                for key in map.keys() {
                    if !hooks.contains(key) {
                        hooks.push(key.clone());
                    }
                }
            }
        }
    }
    hooks.sort();
    Ok(json!({ "skills": skills, "commands": commands, "hooks": hooks }))
}

fn memory_store() -> Result<argent_memory::MemoryStore, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let slug = cwd
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string());
    Ok(argent_memory::MemoryStore::new(
        argent_memory::MemoryStore::default_root(),
        slug,
    ))
}

#[tauri::command]
pub fn list_memory() -> Result<Value, String> {
    let store = memory_store()?;
    let entries = store.all().map_err(|error| error.to_string())?;
    Ok(json!(entries
        .iter()
        .map(|entry| json!({
            "scope": entry.scope.as_str(),
            "key": entry.key,
            "value": entry.value,
            "source": entry.source,
        }))
        .collect::<Vec<_>>()))
}

#[tauri::command]
pub fn add_memory(scope: String, key: String, value: String) -> Result<(), String> {
    let scope = argent_memory::Scope::parse(&scope)
        .ok_or_else(|| format!("unknown memory scope {}", scope))?;
    let store = memory_store()?;
    store
        .remember(argent_memory::MemoryEntry {
            scope,
            key,
            value,
            source: None,
        })
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn forget_memory(scope: String, key: String) -> Result<(), String> {
    let scope = argent_memory::Scope::parse(&scope)
        .ok_or_else(|| format!("unknown memory scope {}", scope))?;
    let store = memory_store()?;
    store
        .forget(scope, &key)
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn import_session_text(
    state: State<'_, Backend>,
    text: String,
    title: String,
) -> Result<String, String> {
    let turns = argent_session::import::parse(&text).map_err(|error| error.to_string())?;
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    // The import API needs the index; reuse the same directory the workers use.
    let index = argent_session::SessionIndex::new(state.shared.session_dir.clone());
    let path = argent_session::import::import(&index, &cwd, &title, turns)
        .map_err(|error| error.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command]
pub fn search_sessions(state: State<'_, Backend>, query: String, limit: usize) -> Result<Value, String> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return Ok(json!([]));
    }
    let dir = state.shared.session_dir.clone();
    let mut hits: Vec<(std::time::SystemTime, Value)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(json!([]));
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        for (index, line) in body.lines().enumerate() {
            if !line.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            let snippet: String = line.chars().take(180).collect();
            hits.push((
                modified,
                json!({
                    "path": path.to_string_lossy(),
                    "title": path.file_stem().map(|name| name.to_string_lossy()).unwrap_or_default(),
                    "line": index,
                    "snippet": snippet,
                }),
            ));
            break; // one hit per session is enough for navigation
        }
    }
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.0));
    Ok(json!(hits
        .into_iter()
        .take(limit.min(100))
        .map(|(_, value)| value)
        .collect::<Vec<_>>()))
}

fn collect_documents(dir: &std::path::Path, depth: usize, out: &mut Vec<Value>) {
    if depth > 3 || out.len() >= 100 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<std::path::PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    children.sort();
    for path in children {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || matches!(name, "target" | "node_modules" | "dist" | "build") {
            continue;
        }
        if path.is_dir() {
            collect_documents(&path, depth + 1, out);
            continue;
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "docx" | "pptx" | "xlsx" | "pdf") {
            continue;
        }
        out.push(json!({
            "name": name,
            "path": path.to_string_lossy(),
        }));
    }
}

fn collect_files(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    query: &str,
    out: &mut Vec<String>,
) {
    if depth > 3 || out.len() >= 200 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<std::path::PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    children.sort();
    for path in children {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || matches!(name, "target" | "node_modules" | "dist" | "build") {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, depth + 1, query, out);
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if query.is_empty() || relative.to_ascii_lowercase().contains(&query.to_ascii_lowercase()) {
            out.push(relative);
        }
    }
}

#[tauri::command]
pub fn list_files(query: String) -> Result<Vec<String>, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut found = Vec::new();
    collect_files(&cwd, &cwd, 0, &query, &mut found);
    Ok(found)
}

#[tauri::command]
pub fn list_documents() -> Result<Value, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut documents = Vec::new();
    collect_documents(&cwd, 0, &mut documents);
    Ok(json!(documents))
}

#[tauri::command]
pub fn read_document(path: String) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let root = std::fs::canonicalize(cwd).map_err(|error| error.to_string())?;
    let target = std::fs::canonicalize(&path).map_err(|error| error.to_string())?;
    if !target.starts_with(&root) {
        return Err("document is outside the workspace".to_string());
    }
    let text = argent_tools::office::extract_text(&target).map_err(|error| error.to_string())?;
    const LIMIT: usize = 60_000;
    if text.chars().count() <= LIMIT {
        return Ok(text);
    }
    let mut clipped: String = text.chars().take(LIMIT).collect();
    clipped.push('…');
    Ok(clipped)
}

fn automation_json(task: &argent_automation::Automation) -> Value {
    let now = argent_automation::now();
    let (kind, action) = match &task.action {
        Action::Prompt(text) => ("prompt", text.clone()),
        Action::Workflow(path) => ("workflow", path.to_string_lossy().into_owned()),
    };
    json!({
        "id": task.id,
        "name": task.name,
        "schedule": task.schedule.display(),
        "kind": kind,
        "action": action,
        "enabled": task.enabled,
        "nextRun": argent_automation::format_iso(task.next_run(now)),
        "lastStatus": task.last_status,
        "lastLog": task.last_log.as_ref().map(|path| path.to_string_lossy().into_owned()),
    })
}

#[tauri::command]
pub fn list_automations() -> Result<Value, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let store = AutomationStore::for_workspace(&cwd).map_err(|error| error.to_string())?;
    Ok(json!(store
        .list()
        .iter()
        .map(automation_json)
        .collect::<Vec<_>>()))
}

#[tauri::command]
pub fn create_automation(
    name: String,
    schedule: String,
    prompt: String,
) -> Result<(), String> {
    let name = name.trim();
    let prompt = prompt.trim();
    if name.is_empty() || prompt.is_empty() {
        return Err("name and prompt are required".to_string());
    }
    let parsed = Schedule::parse(schedule.trim()).map_err(|error| error.to_string())?;
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut store = AutomationStore::for_workspace(&cwd).map_err(|error| error.to_string())?;
    let mut task = argent_automation::Automation::new(name, parsed, Action::Prompt(prompt.into()));
    task.cwd = Some(cwd);
    store.upsert(task);
    store.save().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn toggle_automation(id: String) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut store = AutomationStore::for_workspace(&cwd).map_err(|error| error.to_string())?;
    match store.get_mut(&id) {
        Some(task) => task.enabled = !task.enabled,
        None => return Err(format!("automation {} not found", id)),
    }
    store.save().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn delete_automation(id: String) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut store = AutomationStore::for_workspace(&cwd).map_err(|error| error.to_string())?;
    store.remove(&id);
    store.save().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn run_automation(app: AppHandle, id: String) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let store = AutomationStore::for_workspace(&cwd).map_err(|error| error.to_string())?;
    let task = store
        .get(&id)
        .cloned()
        .ok_or_else(|| format!("automation {} not found", id))?;
    // A run spawns the CLI and waits; keep it off the command thread.
    thread::spawn(move || {
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
        let _ = app.emit(
            "argent://event",
            json!({ "kind": "automation_result", "text": text }),
        );
    });
    Ok(())
}

#[tauri::command]
pub fn automation_log(id: String) -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    let store = AutomationStore::for_workspace(&cwd).map_err(|error| error.to_string())?;
    let path = store
        .get(&id)
        .and_then(|task| task.last_log.clone())
        .ok_or_else(|| "no log for this automation".to_string())?;
    let body = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    const LIMIT: usize = 200_000;
    if body.chars().count() <= LIMIT {
        return Ok(body);
    }
    let mut clipped: String = body.chars().take(LIMIT).collect();
    clipped.push('…');
    Ok(clipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn approval_answer_reaches_the_blocked_worker() {
        let bridge = ApprovalBridge::new();
        let (reply_tx, reply_rx) = mpsc::channel();
        bridge.insert(7, reply_tx);
        // The frontend's "允许" button maps to answer(id, true).
        bridge.answer(7, true).expect("answer delivered");
        assert!(reply_rx.recv().unwrap());
    }

    #[test]
    fn a_denied_approval_is_delivered_and_cannot_be_answered_twice() {
        let bridge = ApprovalBridge::new();
        let (reply_tx, reply_rx) = mpsc::channel();
        bridge.insert(9, reply_tx);
        bridge.answer(9, false).expect("deny delivered");
        assert!(!reply_rx.recv().unwrap());
        // A second click on the same dialog must not reach the worker again.
        assert!(bridge.answer(9, true).is_err());
    }

    #[test]
    fn answering_an_unknown_approval_is_an_error() {
        let bridge = ApprovalBridge::new();
        assert!(bridge.answer(404, true).is_err());
    }

    #[test]
    fn document_scan_only_returns_supported_extensions() {
        let dir = std::env::temp_dir().join(format!("argent-docs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).expect("subdir");
        std::fs::create_dir_all(dir.join("node_modules")).expect("ignored dir");
        std::fs::write(dir.join("report.docx"), b"x").expect("docx");
        std::fs::write(dir.join("notes.txt"), b"x").expect("txt");
        std::fs::write(dir.join("sub").join("slide.pdf"), b"x").expect("pdf");
        std::fs::write(dir.join("node_modules").join("ignored.docx"), b"x").expect("ignored");
        let mut found = Vec::new();
        collect_documents(&dir, 0, &mut found);
        assert_eq!(found.len(), 2);
        let names: Vec<String> = found
            .iter()
            .map(|value| value["name"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(names.iter().any(|name| name == "report.docx"));
        assert!(names.iter().any(|name| name == "slide.pdf"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn automation_json_flattens_schedule_and_action() {
        let task = argent_automation::Automation::new(
            "Daily review",
            Schedule::parse("daily 09:00").expect("schedule"),
            Action::Prompt("review the diff".into()),
        );
        let payload = automation_json(&task);
        assert_eq!(payload["name"], "Daily review");
        assert_eq!(payload["schedule"], "daily 09:00");
        assert_eq!(payload["kind"], "prompt");
        assert_eq!(payload["action"], "review the diff");
        assert_eq!(payload["enabled"], true);
    }

    #[test]
    fn an_allowed_tool_is_remembered_for_the_tab() {
        let bridge = ApprovalBridge::new();
        bridge.allow_tool("read");
        assert!(bridge.is_allowed("read"));
        assert!(!bridge.is_allowed("bash"));
        let (reply_tx, reply_rx) = mpsc::channel();
        bridge.insert(1, reply_tx);
        bridge.answer(1, true).expect("answer delivered");
        assert!(reply_rx.recv().unwrap());
    }

    #[test]
    fn steering_text_is_queued_in_order() {
        let queue = Mutex::new(VecDeque::new());
        enqueue_steering(&queue, "first".into()).expect("queued");
        enqueue_steering(&queue, "second".into()).expect("queued");
        let mut guard = queue.lock().expect("queue lock");
        assert_eq!(guard.pop_front().as_deref(), Some("first"));
        assert_eq!(guard.pop_front().as_deref(), Some("second"));
    }

    #[test]
    fn ready_and_turn_done_payloads_carry_picker_and_usage_data() {
        use argent_agent::FinishReason;
        use argent_ai::Usage;

        let ready = map_event(UiEvent::Ready {
            model: "m".into(),
            level: "low".into(),
            context_max: 128_000,
            models: vec![("m".into(), 960_000)],
            thinking: vec!["low".into(), "high".into()],
            cwd: "/work".into(),
            provider: "local".into(),
            protocol: "openai-chat-completions".into(),
            theme: "dark".into(),
        });
        assert_eq!(ready["models"][0]["id"], "m");
        assert_eq!(ready["models"][0]["contextLength"], 960_000);
        assert_eq!(ready["thinking"][1], "high");

        let done = map_event(UiEvent::TurnDone {
            reason: FinishReason::Complete,
            usage: Usage {
                prompt_tokens: 12,
                completion_tokens: 3,
                total_tokens: 15,
                cached_tokens: 4,
            },
            requests: 2,
            cost: Some(0.5),
        });
        assert_eq!(done["promptTokens"], 12);
        assert_eq!(done["completionTokens"], 3);
        assert_eq!(done["totalTokens"], 15);
        assert_eq!(done["cachedTokens"], 4);
        assert_eq!(done["requests"], 2);
        assert_eq!(done["cost"], 0.5);
        assert_eq!(done["priced"], true);
    }

    #[test]
    fn loaded_event_preserves_every_transcript_row() {
        use argent_ui_core::{Entry, NoticeKind, ToolRun, ToolState};

        let payload = map_event(UiEvent::Loaded(vec![
            Entry::User("hello".into()),
            Entry::Assistant("hi".into()),
            Entry::Thinking("hmm".into()),
            Entry::Tool(ToolRun {
                call_id: "call-1".into(),
                name: "read".into(),
                arguments: "{\"path\":\"a.rs\"}".into(),
                output: "contents".into(),
                state: ToolState::Ok { ms: 12 },
            }),
            Entry::Notice(NoticeKind::Warning, "careful".into()),
        ]));

        assert_eq!(payload["kind"], "loaded");
        let entries = payload["entries"].as_array().expect("entries array");
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[0]["kind"], "user");
        assert_eq!(entries[0]["text"], "hello");
        assert_eq!(entries[1]["kind"], "assistant");
        assert_eq!(entries[2]["kind"], "thinking");
        assert_eq!(entries[3]["kind"], "tool");
        assert_eq!(entries[3]["callId"], "call-1");
        assert_eq!(entries[3]["state"], "ok");
        assert_eq!(entries[3]["durationMs"], 12);
        assert_eq!(entries[4]["kind"], "notice");
        assert_eq!(entries[4]["level"], "warning");
    }
}
