//! The shared frontend contract, exercised against the desktop engine.
//!
//! argent runs three worker loops over the same agent. A rule that only holds
//! in one of them is a bug waiting to happen: the terminal UI once left its
//! "working" flag on when a prompt failed, because the worker reported the
//! error but never closed the turn.
//!
//! The rule under test lives in `argent_ui_core::turn` so every frontend
//! checks against the same text. This file proves the desktop engine honors
//! it by driving a real worker against an endpoint that refuses the request.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::Duration;

use argent_core::config::Config;
use argent_ui_core::turn::{check_turn, failed_turn, TurnEvents};

use super::{start_session, Command, UiEvent};

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
    let dir = std::env::temp_dir().join(format!("argent-engine-contract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let config = Config {
        base_url: argent_ui_core::failing_base_url(),
        protocol: "openai-chat-completions".to_string(),
        model: "contract-model".to_string(),
        session_dir: dir.clone(),
        ..Config::default()
    };

    let engine = start_session(config, dir.clone(), None, Arc::new(AtomicU64::new(1)));
    // Drain the startup events before the turn begins.
    while engine.events.try_recv().is_ok() {}

    let _ = engine.commands.send(Command::Prompt("hello".to_string()));

    let events = collect_turn(&engine.events);
    assert_eq!(
        events.terminal.len(),
        1,
        "the turn must close with exactly one terminal event"
    );
    assert!(
        matches!(events.terminal[0], argent_agent::FinishReason::Error(_)),
        "a refused request must be reported as an error turn: {:?}",
        events.terminal[0]
    );
    let _ = engine.commands.send(Command::Shutdown);
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
