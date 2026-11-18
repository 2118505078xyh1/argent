//! Tauri commands and the one worker that owns the Rust agent.

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use argent_engine::{prepare_config, start_session, Engine, UiEvent};
use argent_ui_core::Command;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

struct Shared {
    engine: Mutex<Engine>,
    approvals: Mutex<HashMap<u64, Sender<bool>>>,
    ready: Mutex<Option<Value>>,
}

/// Tauri-managed handle to the agent worker.
pub struct Backend {
    shared: Arc<Shared>,
}

impl Backend {
    pub fn start(app: AppHandle) -> Result<Self, String> {
        let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
        let config = prepare_config(&cwd)?;
        let engine = start_session(config, cwd, None, Arc::new(AtomicU64::new(1)));
        let shared = Arc::new(Shared {
            engine: Mutex::new(engine),
            approvals: Mutex::new(HashMap::new()),
            ready: Mutex::new(None),
        });
        let worker = Arc::clone(&shared);
        let app = app.clone();
        thread::spawn(move || loop {
            let mut events = Vec::new();
            {
                let engine = worker.engine.lock().expect("engine poisoned");
                while let Ok(event) = engine.events.try_recv() {
                    events.push(event);
                }
                while let Ok(approval) = engine.approvals.try_recv() {
                    let id = approval.id;
                    worker
                        .approvals
                        .lock()
                        .expect("approvals poisoned")
                        .insert(id, approval.reply);
                    let _ = app.emit(
                        "argent://event",
                        json!({
                            "kind": "approval",
                            "id": id,
                            "name": approval.name,
                            "arguments": approval.arguments,
                        }),
                    );
                }
            }
            for event in events {
                let payload = map_event(event);
                if payload["kind"] == "ready" {
                    *worker.ready.lock().expect("ready poisoned") = Some(payload.clone());
                }
                let _ = app.emit("argent://event", payload);
            }
            thread::sleep(Duration::from_millis(30));
        });
        Ok(Self { shared })
    }
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
            "models": models.into_iter().map(|(name, _)| name).collect::<Vec<_>>(),
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
            requests,
            cost,
            ..
        } => json!({
            "kind": "turn_done",
            "reason": reason.describe(),
            "requests": requests,
            "cost": cost,
        }),
        UiEvent::Error(message) => json!({ "kind": "error", "message": message }),
        UiEvent::Session { path } => json!({
            "kind": "session",
            "path": path.to_string_lossy(),
        }),
        UiEvent::SessionCleared => json!({ "kind": "session_cleared" }),
        UiEvent::Loaded(_) => json!({ "kind": "loaded" }),
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
    let ready = state.shared.ready.lock().expect("ready poisoned").clone();
    json!({ "ready": ready })
}

#[tauri::command]
pub fn send_prompt(state: State<'_, Backend>, text: String) -> Result<(), String> {
    state
        .shared
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::Prompt(text))
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn abort(state: State<'_, Backend>) -> Result<(), String> {
    state
        .shared
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::Abort)
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn new_session(state: State<'_, Backend>) -> Result<(), String> {
    state
        .shared
        .engine
        .lock()
        .map_err(|_| "engine lock poisoned".to_string())?
        .commands
        .send(Command::NewSession)
        .map_err(|_| "agent worker is gone".to_string())
}

#[tauri::command]
pub fn answer_approval(state: State<'_, Backend>, id: u64, allow: bool) -> Result<(), String> {
    let reply = state
        .shared
        .approvals
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
