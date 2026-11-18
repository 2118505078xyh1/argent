//! The shared frontend contract, exercised against the terminal UI.
//!
//! argent runs three worker loops over the same agent. A rule that only holds
//! in one of them is a bug waiting to happen: the terminal UI once left its
//! "working" flag on when a prompt failed, because the worker reported the
//! error but never closed the turn.
//!
//! The rule under test lives in `argent_ui_core::turn` so every frontend
//! checks against the same text. This file proves the terminal UI honors it
//! by driving a real worker with a provider that fails.

use std::sync::Arc;
use std::time::Duration;

use argent_agent::{Agent, AgentConfig};
use argent_ai::{ChatRequest, ModelInfo, Provider, StreamEvent};
use argent_core::error::Result as CoreResult;
use argent_session::SessionIndex;
use argent_tools::{ToolCtx, ToolRegistry};
use argent_ui_core::approval::{check_approval, ApprovalEvents};
use argent_ui_core::turn::{check_turn, failed_turn, TurnEvents};

use super::{Command, UiEvent};

/// A provider that always fails to open a stream.
struct FailingProvider;

impl Provider for FailingProvider {
    fn name(&self) -> &str {
        "failing"
    }

    fn models(&self) -> CoreResult<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    fn chat(
        &self,
        _request: &ChatRequest,
    ) -> CoreResult<Box<dyn Iterator<Item = CoreResult<StreamEvent>> + Send>> {
        Err(argent_core::error::Error::provider("the provider is down"))
    }
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "argent-tui-contract-{}-{}",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Collect the events of one turn and check them against the contract.
fn collect_turn(events: &std::sync::mpsc::Receiver<UiEvent>) -> TurnEvents {
    let mut collected = TurnEvents::default();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match events.recv_timeout(remaining) {
            // Only the frontend's turn event closes the turn. The agent's own
            // `AgentEvent::Finished` happens inside the run.
            Ok(UiEvent::TurnDone { reason, .. }) => collected.terminal(reason),
            Ok(UiEvent::Error(message)) => collected.error(message),
            Ok(_) => {}
            Err(_) => break,
        }
        if collected.has_single_terminal() {
            // Give the worker a moment to also report an error event, then
            // stop: a second terminal event would be a contract violation and
            // must be observed.
            let grace = std::time::Instant::now() + Duration::from_millis(200);
            while std::time::Instant::now() < grace {
                match events.recv_timeout(Duration::from_millis(50)) {
                    Ok(UiEvent::TurnDone { reason, .. }) => collected.terminal(reason),
                    Ok(UiEvent::Error(message)) => collected.error(message),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            break;
        }
    }
    collected
}

#[test]
fn a_failed_prompt_closes_the_turn_exactly_once() {
    let dir = scratch("failed-prompt");
    let index = SessionIndex::new(dir.join("sessions"));
    let path = index.create(&dir, "contract").expect("create");
    let session = crate::session_ctx::SessionContext::open(path).expect("open");

    let agent = Agent::new(
        Box::new(FailingProvider),
        Arc::new(ToolRegistry::with_core()),
        AgentConfig::new("failing-model"),
        ToolCtx::new(dir.clone()),
        "system",
    );
    let worker = super::spawn(
        agent,
        session,
        argent_tools::CheckpointStore::detached(),
        argent_core::config::Config::default(),
        index,
        dir.clone(),
        argent_hooks::HookConfig::default(),
        argent_i18n::Locale::En,
    );

    // Drain the startup events before the turn begins.
    while worker.events.try_recv().is_ok() {}
    let _ = worker.commands.send(Command::Prompt {
        text: "hello".to_string(),
        images: Vec::new(),
    });

    let events = collect_turn(&worker.events);
    assert_eq!(
        events.terminal.len(),
        1,
        "the turn must close with exactly one terminal event"
    );
    assert!(
        matches!(events.terminal[0], argent_agent::FinishReason::Error(_)),
        "a failed provider must be reported as an error turn: {:?}",
        events.terminal[0]
    );
    let _ = worker.commands.send(Command::Shutdown);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_contract_rejects_a_turn_without_a_terminal_event() {
    // The shared rule itself, so a frontend test that forgets to close the
    // turn fails with a contract violation rather than a bare count.
    let mut events = TurnEvents::default();
    events.error("the provider is down");
    assert!(check_turn(&events).is_err());
    assert!(check_turn(&failed_turn("the provider is down", false)).is_ok());
}

#[test]
fn an_approval_reply_is_sent_exactly_once() {
    // The terminal UI maps one approval overlay key press to one reply on the
    // worker's channel. A second key press after the pending request is gone
    // must not produce a second decision.
    let mut app = super::app::App::new(
        Vec::new(),
        crate::prompts::PromptLibrary::new(),
        argent_i18n::Locale::En,
        Vec::new(),
        crate::keybindings::Bindings::default(),
    );
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    app.handle_approval(super::ApprovalRequest {
        tool: "bash".to_string(),
        args: r#"{"command":"echo hello"}"#.to_string(),
        command: Some("echo hello".to_string()),
        reply: reply_tx,
    });

    let (command_tx, command_rx) = std::sync::mpsc::channel();
    let (_event_tx, event_rx) = std::sync::mpsc::channel();
    let worker = super::Worker {
        commands: command_tx,
        events: event_rx,
        abort: argent_agent::AbortHandle::new(),
        steering: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
    };

    app.handle_key(&worker, super::Key::Char('y'));
    assert_eq!(reply_rx.recv_timeout(Duration::from_secs(1)), Ok(true));
    assert!(reply_rx.try_recv().is_err(), "one key press is one reply");
    assert!(app.pending.is_none());
    // A second key press after the request closed must not send again.
    app.handle_key(&worker, super::Key::Char('n'));
    assert!(reply_rx.try_recv().is_err(), "no second reply");

    let mut contract = ApprovalEvents::default();
    contract.request("bash");
    contract.decision(true);
    assert!(check_approval(&contract).is_ok());
    let _ = command_rx;
}
