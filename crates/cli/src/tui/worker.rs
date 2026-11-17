//! The session worker: prompt handling, streaming, tools and approvals.

use super::*;

/// The UI's handle to the worker.
/// A compaction coroutine running on the worker thread, interleaved with
/// the main conversation at the main run's stream-yield points.
#[derive(Default)]
pub(super) struct CompactionState {
    /// `None` while nothing is being compacted.
    pub(super) coroutine: Option<corosensei::Coroutine<(), (), String>>,
    /// History revision the draft was computed on.
    pub(super) revision: u64,
    /// A finished upgrade, awaiting the end of the main run.
    pub(super) finished: Option<String>,
}

impl CompactionState {
    /// Advance the coroutine one scheduling step; store the result.
    pub(super) fn advance(&mut self) {
        let Some(coroutine) = self.coroutine.as_mut() else {
            return;
        };
        match coroutine.resume(()) {
            corosensei::CoroutineResult::Yield(()) => {}
            corosensei::CoroutineResult::Return(better) => {
                self.finished = Some(better);
                self.coroutine = None;
            }
        }
    }

    /// Write a finished summary back, but only if the conversation has not
    /// moved on since the draft was computed.
    pub(super) fn apply_if_ready(
        &mut self,
        agent: &mut argent_agent::Agent,
        event_tx: &std::sync::mpsc::Sender<UiEvent>,
    ) {
        if let Some(better) = self.finished.take() {
            if agent.history_revision_value() == self.revision {
                agent.replace_last_summary(&better);
                let _ = event_tx.send(UiEvent::Notice(
                    "compaction summary upgraded by the background model".to_string(),
                ));
            }
        }
    }
}

pub struct Worker {
    pub commands: Sender<Command>,
    pub events: Receiver<UiEvent>,
    pub abort: AbortHandle,
    pub steering: Arc<Mutex<VecDeque<String>>>,
}

/// Start the agent worker. The agent and session are moved into it.
pub fn spawn(
    mut agent: Agent,
    mut session: SessionContext,
    checkpoints: argent_tools::CheckpointStore,
    config: argent_core::config::Config,
    index: SessionIndex,
    cwd: PathBuf,
    hooks: HookConfig,
    locale: argent_i18n::Locale,
) -> Worker {
    let (command_tx, command_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let abort = agent.abort_handle();
    let steering = agent.steering_handle();
    let title = session.title();
    let path = session.path.display().to_string();

    let ready = UiEvent::Ready {
        model: agent.model().to_string(),
        thinking: agent.thinking().unwrap_or("default").to_string(),
        provider: config.provider.clone(),
        protocol: config.protocol.clone(),
        cwd: cwd.display().to_string(),
        session: title.clone(),
        session_path: path.clone(),
    };
    let initial = Transcript::from_messages(agent.history())
        .entries()
        .to_vec();
    let _ = argent_session::sweep_empty_sessions(
        &index,
        &[session.path.clone()],
        Duration::from_secs(60),
    );
    let rows = session_rows(&index);

    let worker_command_tx = command_tx.clone();
    std::thread::spawn(move || {
        let command_tx = worker_command_tx;
        let _ = event_tx.send(ready);
        if !initial.is_empty() {
            let _ = event_tx.send(UiEvent::Loaded(initial));
        }
        let _ = event_tx.send(UiEvent::Sessions(rows));

        let background = crate::mainx::background::BackgroundAssistant::from_config(&config);
        // Compaction runs as a coroutine on this same thread: the main run's
        // model-stream yield point advances it, so compaction never blocks
        // the conversation. `slot` holds a finished summary awaiting a safe
        // moment to write back (after the main run ends).
        // Same thread, coroutine closures included: Rc/RefCell is enough and
        // keeps the !Send coroutine contained.
        let compaction: std::rc::Rc<std::cell::RefCell<CompactionState>> =
            std::rc::Rc::new(std::cell::RefCell::new(CompactionState::default()));
        // The heuristic title (first prompt line) is replaced once: after the
        // first run finishes, a background model writes a real one.
        let mut title_pending: Option<String> = None;
        let session_ctx = HookContext {
            cwd: cwd.clone(),
            ..HookContext::default()
        };
        let (_messages, blocked) = run_hook_event(&hooks, HookEvent::SessionStart, &session_ctx);
        if let Some(reason) = blocked {
            let _ = event_tx.send(UiEvent::Notice(format!("SessionStart hook: {}", reason)));
        }

        while let Ok(command) = command_rx.recv() {
            match command {
                Command::Shell(command) => {
                    let call_id = format!(
                        "shell-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|elapsed| elapsed.as_millis())
                            .unwrap_or(0)
                    );
                    let mut args = argent_core::Json::obj();
                    args.set("command", argent_core::Json::string(command.clone()));
                    let call = argent_ai::ToolCall::new(&call_id, "bash", args.dump());
                    let _ = event_tx.send(UiEvent::Agent(AgentEvent::ToolRequested(call)));
                    let started = std::time::Instant::now();
                    let (content, ok) = run_shell(&cwd, &command);
                    let _ = event_tx.send(UiEvent::Agent(AgentEvent::ToolFinished {
                        call_id,
                        name: "bash".to_string(),
                        content,
                        is_error: !ok,
                        duration_ms: started.elapsed().as_millis() as u64,
                    }));
                }
                Command::Prompt { text, images } => {
                    let submit_ctx = HookContext {
                        cwd: cwd.clone(),
                        prompt: Some(text.clone()),
                        ..HookContext::default()
                    };
                    let (messages, blocked) =
                        run_hook_event(&hooks, HookEvent::UserPromptSubmit, &submit_ctx);
                    for message in messages {
                        let _ = event_tx.send(UiEvent::Notice(message));
                    }
                    if let Some(reason) = blocked {
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "UserPromptSubmit hook blocked the prompt: {}",
                            reason
                        )));
                        let _ = event_tx.send(UiEvent::TurnDone {
                            reason: FinishReason::Aborted,
                            usage: agent.usage(),
                            requests: agent.requests(),
                            context_used: argent_agent::estimate_tokens(agent.history())
                                .max(agent.last_prompt_tokens() as usize),
                            cost: None,
                        });
                        continue;
                    }

                    // The first message names the task. Rename before the run so the
                    // title is right while the model is still thinking.
                    if agent.history().is_empty() {
                        title_pending = Some(text.clone());
                        let title = title_from_prompt(&text);
                        let path = session.path.clone();
                        if index.rename(&path, &title).is_ok() {
                            session.close();
                            match SessionContext::open(path.clone()) {
                                Ok(opened) => {
                                    session = opened;
                                    let _ = event_tx.send(UiEvent::SessionRetitled {
                                        title: session.title(),
                                        path: path.display().to_string(),
                                    });
                                    let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                                }
                                Err(error) => {
                                    let _ = event_tx.send(UiEvent::Error(error.to_string()));
                                }
                            }
                        }
                    }

                    let before = agent.history().len();
                    // While the main run waits on the model, advance the
                    // compaction coroutine; while compaction waits on its
                    // own model call, control returns here.
                    let compaction_for_hook = std::rc::Rc::clone(&compaction);
                    let history_guard = unsafe {
                        argent_ai::cooperative::set_yielder(std::rc::Rc::new(move || {
                            compaction_for_hook.borrow_mut().advance();
                        }))
                    };
                    let mut sink = ChannelSink::new(event_tx.clone());
                    let result = if images.is_empty() {
                        agent.run(&text, &mut sink)
                    } else {
                        agent.run_message(
                            argent_ai::Message::user_with_images(text.clone(), images.clone()),
                            &mut sink,
                        )
                    };
                    match result {
                        Ok(reason) => {
                            let _ = session.record_new(agent.history(), before);
                            let stop_ctx = HookContext {
                                cwd: cwd.clone(),
                                prompt: Some(text.clone()),
                                ..HookContext::default()
                            };
                            let (messages, _) = run_hook_event(&hooks, HookEvent::Stop, &stop_ctx);
                            for message in messages {
                                let _ = event_tx.send(UiEvent::Notice(message));
                            }
                            let usage = agent.usage();
                            let requests = agent.requests();
                            let cost = agent.usage_cost();
                            let _ = argent_session::usage::record_with_ttft(
                                &session.path,
                                agent.model(),
                                &usage,
                                requests,
                                cost,
                                sink.clock.ms(),
                            );
                            let turn_ok = matches!(reason, FinishReason::Complete);
                            let _ = event_tx.send(UiEvent::TurnDone {
                                reason,
                                usage,
                                requests,
                                context_used: argent_agent::estimate_tokens(agent.history())
                                    .max(agent.last_prompt_tokens() as usize),
                                cost,
                            });
                            // The main run ended: a finished compaction summary
                            // can now be written back (revision-guarded).
                            compaction
                                .borrow_mut()
                                .apply_if_ready(&mut agent, &event_tx);
                            // Threshold reached and no compaction in flight:
                            // spawn the compaction coroutine. Its model call
                            // interleaves with the next run's stream reads.
                            let in_flight = {
                                let state = compaction.borrow();
                                state.coroutine.is_some() || state.finished.is_some()
                            };
                            if turn_ok && !in_flight {
                                let window = argent_session::controller::model_context_window(
                                    &agent,
                                    config.max_context_tokens,
                                );
                                let reserved = argent_session::effective_reserved(
                                    window,
                                    config.reserved_context_tokens,
                                );
                                if argent_session::should_compact_real(
                                    argent_agent::estimate_tokens(agent.history()),
                                    agent.last_prompt_tokens(),
                                    window,
                                    reserved,
                                ) {
                                    let history = agent.history().to_vec();
                                    let revision = agent.history_revision_value();
                                    if let Some(assistant) = &background {
                                        let heuristic = argent_session::summarize(&history);
                                        let assistant = assistant.clone();
                                        let compaction_state = std::rc::Rc::clone(&compaction);
                                        let coroutine: corosensei::Coroutine<(), (), String> =
                                            corosensei::Coroutine::new(move |yielder, _input| {
                                                // Register this coroutine's own
                                                // yielder so its model-stream
                                                // reads suspend back here instead
                                                // of stalling the main run. The
                                                // 'static erasure is safe: the
                                                // yielder's stack lives as long as
                                                // the suspended coroutine.
                                                let yielder: &'static corosensei::Yielder<(), ()> =
                                                    unsafe { &*(yielder as *const _) };
                                                let _guard = unsafe {
                                                    argent_ai::cooperative::set_yielder(
                                                        std::rc::Rc::new(move || {
                                                            yielder.suspend(())
                                                        }),
                                                    )
                                                };
                                                assistant
                                                    .summarize_for(&heuristic)
                                                    .unwrap_or(heuristic)
                                            });
                                        {
                                            let mut state = compaction_state.borrow_mut();
                                            state.coroutine = Some(coroutine);
                                            state.revision = revision;
                                        };
                                    }
                                }
                            };
                            drop(history_guard);
                            // First run of the session: hand the exchange to
                            // the background model, which answers with a real
                            // title. Delivered as Command::Rename so the
                            // worker thread (the only one allowed to reopen
                            // the session file) applies it.
                            if let Some(first) = title_pending.take() {
                                if let Some(assistant) = &background {
                                    if turn_ok {
                                        let answer = agent
                                            .history()
                                            .iter()
                                            .rev()
                                            .find(|m| m.role == argent_ai::Role::Assistant)
                                            .map(|m| m.text())
                                            .unwrap_or_default();
                                        let exchange = format!(
                                            "user: {}
assistant: {}",
                                            first, answer
                                        );
                                        let tx = command_tx.clone();
                                        let assistant = assistant.clone();
                                        std::thread::spawn(move || {
                                            if let Some(title) = assistant.title_for(&exchange) {
                                                let _ = tx.send(Command::Rename(title));
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            // 必须发 TurnDone：只发 Error 的话 UI 的 working
                            // 永远为 true，下一条 prompt 会进 steering 队列而
                            // 不是新回合，表现为「发第二条消息没有回复」。
                            let _ = event_tx.send(UiEvent::TurnDone {
                                reason: FinishReason::Error(error.to_string()),
                                usage: agent.usage(),
                                requests: agent.requests(),
                                context_used: argent_agent::estimate_tokens(agent.history())
                                    .max(agent.last_prompt_tokens() as usize),
                                cost: None,
                            });
                        }
                    }
                }
                Command::SetModel(model) => {
                    agent.set_model(model.clone());
                    let _ = event_tx.send(UiEvent::Notice(format!(
                        "{}: {}",
                        argent_i18n::t(locale, "tui.model"),
                        model
                    )));
                }
                Command::SetThinking(level) => {
                    agent.set_thinking(level.clone());
                    let _ = event_tx.send(UiEvent::Notice(format!(
                        "{}: {}",
                        argent_i18n::t(locale, "tui.thinking"),
                        level
                    )));
                }
                Command::Fork => {
                    let title = format!("{} (fork)", session.title());
                    match index.create(&cwd, &title) {
                        Ok(path) => match SessionContext::open(path.clone()) {
                            Ok(mut forked) => {
                                for message in agent.history() {
                                    let _ = forked.record(message.clone());
                                }
                                session.close();
                                session = forked;
                                // 新会话有自己的检查点目录。
                                checkpoints.retarget(&path);
                                let entries = Transcript::from_messages(agent.history())
                                    .entries()
                                    .to_vec();
                                let _ = event_tx.send(UiEvent::SessionOpened {
                                    title: session.title(),
                                    path: path.display().to_string(),
                                    entries,
                                });
                                let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        },
                        Err(error) => {
                            let _ = event_tx.send(UiEvent::Error(error.to_string()));
                        }
                    }
                }
                Command::Rename(title) => match index.rename(&session.path, &title) {
                    Ok(()) => {
                        session.close();
                        match SessionContext::open(session.path.clone()) {
                            Ok(opened) => {
                                session = opened;
                                let entries = Transcript::from_messages(agent.history())
                                    .entries()
                                    .to_vec();
                                let _ = event_tx.send(UiEvent::SessionOpened {
                                    title: session.title(),
                                    path: session.path.display().to_string(),
                                    entries,
                                });
                                let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        }
                    }
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                    }
                },
                Command::DeleteSession(path) => {
                    if path == session.path {
                        let _ = event_tx.send(UiEvent::Error(
                            "cannot delete the session that is open; switch away first".to_string(),
                        ));
                    } else {
                        match index.delete(&path) {
                            Ok(()) => {
                                let _ = event_tx.send(UiEvent::Notice(format!(
                                    "deleted {}",
                                    path.file_name()
                                        .and_then(|name| name.to_str())
                                        .unwrap_or("session")
                                )));
                                let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        }
                    }
                }
                Command::RenameSession(path, name) => {
                    let current = path == session.path;
                    match index.rename(&path, &name) {
                        Ok(()) => {
                            if current {
                                session.close();
                                match SessionContext::open(session.path.clone()) {
                                    Ok(opened) => {
                                        session = opened;
                                        let entries = Transcript::from_messages(agent.history())
                                            .entries()
                                            .to_vec();
                                        let _ = event_tx.send(UiEvent::SessionOpened {
                                            title: session.title(),
                                            path: session.path.display().to_string(),
                                            entries,
                                        });
                                    }
                                    Err(error) => {
                                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                                    }
                                }
                            }
                            let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                        }
                        Err(error) => {
                            let _ = event_tx.send(UiEvent::Error(error.to_string()));
                        }
                    }
                }
                Command::NewSession => {
                    let previous = session.path.clone();
                    let previous_empty = argent_session::is_empty_transcript(&previous);
                    match index.create(&cwd, argent_i18n::t(locale, "tui.new_session")) {
                        Ok(path) => match SessionContext::open(path.clone()) {
                            Ok(opened) => {
                                session.close();
                                session = opened;
                                checkpoints.retarget(&path);
                                refresh_session_cache_key(&path);
                                argent_session::controller::clear(&mut agent);
                                if previous_empty {
                                    let _ = std::fs::remove_file(&previous);
                                }
                                let _ = argent_session::sweep_empty_sessions(
                                    &index,
                                    std::slice::from_ref(&path),
                                    Duration::from_secs(60),
                                );
                                let _ = event_tx.send(UiEvent::SessionOpened {
                                    title: session.title(),
                                    path: path.display().to_string(),
                                    entries: Vec::new(),
                                });
                                let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        },
                        Err(error) => {
                            let _ = event_tx.send(UiEvent::Error(error.to_string()));
                        }
                    }
                }
                Command::OpenSession(path) => match SessionContext::open(path.clone()) {
                    Ok(opened) => {
                        session.close();
                        session = opened;
                        checkpoints.retarget(&path);
                        refresh_session_cache_key(&path);
                        let messages = session.messages();
                        argent_session::controller::open(&mut agent, messages.clone());
                        let entries = Transcript::from_messages(&messages).entries().to_vec();
                        let _ = event_tx.send(UiEvent::SessionOpened {
                            title: session.title(),
                            path: path.display().to_string(),
                            entries,
                        });
                    }
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                    }
                },
                Command::Tree => {
                    let _ = event_tx.send(UiEvent::TreeRows(tree_rows(&session)));
                }
                Command::CheckoutNode(id) => {
                    let previous_leaf = session.tree.active().map(str::to_string);
                    match session.checkout(&id) {
                        Ok(mut messages) => {
                            // Pi-style branch summary: make the messages unique to
                            // the branch we left visible in the new context.
                            if let Some(previous) = previous_leaf {
                                let branch = session.tree.branch_summary_messages(&id, &previous);
                                if !branch.is_empty() {
                                    let mut text = String::from(
                                        "The conversation switched to a different branch. \
                                     The other branch contained these messages:\n",
                                    );
                                    for message in branch.iter().take(40) {
                                        let role = message.role.as_str();
                                        let body = message.text();
                                        let body = if body.len() > 600 {
                                            &body[..600]
                                        } else {
                                            &body
                                        };
                                        text.push_str(&format!("\n[{}] {}", role, body));
                                    }
                                    if let Some(assistant) = &background {
                                        if let Some(condensed) = assistant.branch_summary_for(&text)
                                        {
                                            text = format!(
                                                "The conversation switched to a different \
                                             branch. That branch did:\n{}",
                                                condensed
                                            );
                                        }
                                    }
                                    messages.push(argent_ai::Message::system(text));
                                }
                            }
                            argent_session::controller::open(&mut agent, messages.clone());
                            let entries = Transcript::from_messages(&messages).entries().to_vec();
                            let _ = event_tx.send(UiEvent::SessionOpened {
                                title: session.title(),
                                path: session.path.display().to_string(),
                                entries,
                            });
                            let _ = event_tx
                                .send(UiEvent::Notice(format!("branch switched to {}", id)));
                        }
                        Err(error) => {
                            let _ = event_tx.send(UiEvent::Error(error.to_string()));
                        }
                    }
                }
                Command::ForkNode(id) => match session.fork_at(&id) {
                    Ok(messages) => {
                        argent_session::controller::open(&mut agent, messages.clone());
                        let entries = Transcript::from_messages(&messages).entries().to_vec();
                        let _ = event_tx.send(UiEvent::SessionOpened {
                            title: session.title(),
                            path: session.path.display().to_string(),
                            entries,
                        });
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "forked from {}; the next reply starts a new branch",
                            id
                        )));
                    }
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                    }
                },
                Command::ForkAtMessage(n) => {
                    // 1-based count along the active path, messages only.
                    let mut seen = 0usize;
                    let mut target: Option<String> = None;
                    for node in session.tree.active_path() {
                        if let argent_session::Entry::Message(_) = &node.payload {
                            seen += 1;
                            if seen == n {
                                target = Some(node.id.clone());
                                break;
                            }
                        }
                    }
                    match target {
                        Some(id) => match session.fork_at(&id) {
                            Ok(messages) => {
                                argent_session::controller::open(&mut agent, messages.clone());
                                let entries =
                                    Transcript::from_messages(&messages).entries().to_vec();
                                let _ = event_tx.send(UiEvent::SessionOpened {
                                    title: session.title(),
                                    path: session.path.display().to_string(),
                                    entries,
                                });
                                let _ = event_tx.send(UiEvent::Notice(format!(
                                    "forked at message {}; the next reply starts a new branch",
                                    n
                                )));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        },
                        None => {
                            let _ = event_tx.send(UiEvent::Error(format!(
                                "no message #{} on the active path ({} messages)",
                                n, seen
                            )));
                        }
                    }
                }
                Command::Import(source) => match import_session(&index, &source) {
                    Ok(path) => match SessionContext::open(path.clone()) {
                        Ok(opened) => {
                            session.close();
                            session = opened;
                            checkpoints.retarget(&path);
                            let messages = session.messages();
                            argent_session::controller::open(&mut agent, messages.clone());
                            let entries = Transcript::from_messages(&messages).entries().to_vec();
                            let _ = event_tx.send(UiEvent::SessionOpened {
                                title: session.title(),
                                path: path.display().to_string(),
                                entries,
                            });
                            let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                        }
                        Err(error) => {
                            let _ = event_tx.send(UiEvent::Error(error.to_string()));
                        }
                    },
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Error(error.to_string()));
                    }
                },
                Command::ListSessions => {
                    let _ = argent_session::sweep_empty_sessions(
                        &index,
                        &[session.path.clone()],
                        Duration::from_secs(60),
                    );
                    let _ = event_tx.send(UiEvent::Sessions(session_rows(&index)));
                }
                Command::ListModels => match agent.models() {
                    Ok(models) => {
                        let _ = event_tx.send(UiEvent::Models(
                            models
                                .into_iter()
                                .map(|model| model.id)
                                .filter(|id| config.model_in_scope(id))
                                .collect(),
                        ));
                    }
                    Err(error) => {
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "model catalog unavailable: {}",
                            error
                        )));
                    }
                },
                Command::ListTools => {
                    let _ = event_tx.send(UiEvent::Tools(agent.tool_index()));
                }
                Command::Compact => {
                    let window = argent_session::controller::model_context_window(
                        &agent,
                        config.max_context_tokens,
                    );
                    let reserved =
                        argent_session::effective_reserved(window, config.reserved_context_tokens);
                    if argent_session::should_compact_real(
                        argent_agent::estimate_tokens(agent.history()),
                        agent.last_prompt_tokens(),
                        window,
                        reserved,
                    ) {
                        let ctx = HookContext {
                            cwd: cwd.clone(),
                            ..HookContext::default()
                        };
                        let _ = run_hook_event(&hooks, HookEvent::PreCompact, &ctx);
                    }
                    let before_tokens = argent_agent::estimate_tokens(agent.history());
                    if let Some(summary) = argent_session::controller::compact(
                        &mut agent,
                        window,
                        config.reserved_context_tokens,
                        config.keep_recent_tokens,
                    ) {
                        let after_tokens = argent_agent::estimate_tokens(agent.history());
                        let _ = session.record_summary(summary.clone());
                        let entries = Transcript::from_messages(agent.history())
                            .entries()
                            .to_vec();
                        let _ = event_tx.send(UiEvent::Loaded(entries));
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "context compacted: {} -> {} tokens",
                            before_tokens, after_tokens
                        )));
                    } else {
                        let _ = event_tx.send(UiEvent::Notice(
                            argent_i18n::t(locale, "tui.nothing_compact").to_string(),
                        ));
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
                        let _ = event_tx.send(UiEvent::Notice("nothing to rewind".to_string()));
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
                        let _ = event_tx.send(UiEvent::Notice(format!(
                            "rewound {} run(s), restored {} file(s):\n{}",
                            steps,
                            restored.len(),
                            listing
                        )));
                    }
                }
                Command::Cost => {
                    let _ = event_tx.send(UiEvent::Notice(crate::cost_report(
                        &agent,
                        config.max_context_tokens,
                    )));
                }
                Command::CopyLast => {
                    let text = agent
                        .history()
                        .iter()
                        .rev()
                        .find(|m| m.role == argent_ai::Role::Assistant)
                        .map(|m| m.text())
                        .filter(|t| !t.is_empty());
                    match text {
                        Some(text) => match argent_core::clipboard::copy_text(&text) {
                            Ok(()) => {
                                let _ = event_tx.send(UiEvent::Notice(format!(
                                    "copied {} characters",
                                    text.chars().count()
                                )));
                            }
                            Err(error) => {
                                let _ = event_tx.send(UiEvent::Error(error.to_string()));
                            }
                        },
                        None => {
                            let _ = event_tx.send(UiEvent::Error(
                                "nothing to copy: no assistant reply yet".to_string(),
                            ));
                        }
                    }
                }
                Command::ClearHistory => {
                    argent_session::controller::clear(&mut agent);
                    let _ = event_tx.send(UiEvent::Notice("history cleared".to_string()));
                }
                Command::Shutdown => break,
            }
        }
        session.close();
    });

    Worker {
        commands: command_tx,
        events: event_rx,
        abort,
        steering,
    }
}

/// Run one hook event and return its notices plus a blocking reason.
pub(crate) fn run_hook_event(
    config: &HookConfig,
    event: HookEvent,
    ctx: &HookContext,
) -> (Vec<String>, Option<String>) {
    let mut notices = Vec::new();
    let mut block = None;
    for hook in config.for_event(event, ctx.tool_name.as_deref()) {
        let outcome = argent_hooks::run_command(event, hook, ctx);
        let text = if outcome.stderr.trim().is_empty() {
            outcome.stdout.trim()
        } else {
            outcome.stderr.trim()
        };
        if !outcome.success {
            if !text.is_empty() {
                notices.push(format!("{} hook: {}", event.name(), clip_hook_text(text)));
            }
            if hook.blocks_on_failure() && block.is_none() {
                block = Some(if text.is_empty() {
                    format!("{} hook failed", event.name())
                } else {
                    clip_hook_text(text)
                });
            }
        } else if !text.is_empty() {
            notices.push(clip_hook_text(text));
        }
    }
    (notices, block)
}

pub(super) struct ChannelSink {
    pub(super) tx: Sender<UiEvent>,
    pub(super) clock: argent_agent::TtftClock,
}

impl ChannelSink {
    pub(super) fn new(tx: Sender<UiEvent>) -> ChannelSink {
        ChannelSink {
            tx,
            clock: argent_agent::TtftClock::new(),
        }
    }
}

impl EventSink for ChannelSink {
    fn emit(&mut self, event: AgentEvent) {
        self.clock.observe(&event);
        let _ = self.tx.send(UiEvent::Agent(event));
    }
}
