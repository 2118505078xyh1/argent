
use super::*;
use argent_ai::{ChatRequest, ModelInfo, StreamEvent};
use argent_tools::{Tool, ToolRegistry, ToolResult};
use std::sync::Mutex;

/// A provider that replays a fixed script of assistant messages.
struct ScriptedProvider {
    script: Mutex<Vec<Message>>,
    calls: Mutex<usize>,
}

impl ScriptedProvider {
    fn new(script: Vec<Message>) -> ScriptedProvider {
        ScriptedProvider {
            script: Mutex::new(script),
            calls: Mutex::new(0),
        }
    }
}

impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    fn chat(
        &self,
        _request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        let script = self.script.lock().expect("lock");
        let index = {
            let mut calls = self.calls.lock().expect("lock");
            let current = *calls;
            *calls += 1;
            current
        };
        let message = script
            .get(index)
            .cloned()
            .unwrap_or_else(|| Message::assistant("done"));
        let mut events: Vec<Result<StreamEvent>> = Vec::new();
        let text = message.text();
        if !text.is_empty() {
            events.push(Ok(StreamEvent::Text(text)));
        }
        for call in message.tool_calls {
            events.push(Ok(StreamEvent::ToolCall(call)));
        }
        events.push(Ok(StreamEvent::Usage(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: 0,
        })));
        events.push(Ok(StreamEvent::Done));
        Ok(Box::new(events.into_iter()))
    }
}

fn agent_with(script: Vec<Message>) -> Agent {
    let registry = Arc::new(ToolRegistry::with_builtins());
    let provider = Box::new(ScriptedProvider::new(script));
    let config = AgentConfig::new("test-model");
    Agent::new(
        provider,
        registry,
        config,
        ToolCtx::new(std::env::temp_dir()),
        "system",
    )
}

/// The interrupt flag is process-wide, so every test that runs an agent
/// takes this lock. Otherwise the Ctrl+C test aborts its neighbours.
fn run_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 历史里的每个 tool_use 都必须有配对的 tool_result。
///
/// Anthropic 会因为缺一条而拒绝整个会话，恢复出来的历史也无法重放。
fn assert_every_call_is_answered(agent: &Agent) {
    let history = agent.history();
    for (index, message) in history.iter().enumerate() {
        for call in &message.tool_calls {
            let answered = history[index + 1..]
                .iter()
                .any(|later| later.tool_call_id.as_deref() == Some(call.id.as_str()));
            assert!(
                answered,
                "tool call '{}' ({}) has no result",
                call.id, call.name
            );
        }
    }
}

/// 执行时触发全局中断的工具，用来模拟工具批处理中途按下 Ctrl+C。
struct InterruptingTool {
    name: &'static str,
    mutating: bool,
}

impl Tool for InterruptingTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "interrupting tool"
    }

    fn parameters(&self) -> Json {
        argent_tools::schema_object(vec![], vec![])
    }

    fn is_mutating(&self) -> bool {
        self.mutating
    }

    fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
        argent_core::interrupt::request();
        Ok(ToolResult::ok("body"))
    }
}

/// 模拟一个长时间运行、但会观察取消信号的工具。
struct WaitingTool {
    saw_cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl Tool for WaitingTool {
    fn name(&self) -> &str {
        "wait"
    }

    fn description(&self) -> &str {
        "waits until the run is cancelled"
    }

    fn parameters(&self) -> Json {
        argent_tools::schema_object(vec![], vec![])
    }

    fn execute(&self, ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while Instant::now() < deadline {
            if ctx.cancelled() {
                self.saw_cancel
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return Ok(ToolResult::error("interrupted"));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(ToolResult::error("waited too long"))
    }
}

/// 只发一个带签名的思考块和一段文本的 provider。
struct ThinkingProvider;

impl Provider for ThinkingProvider {
    fn name(&self) -> &str {
        "thinking-test"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    fn chat(
        &self,
        _request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::Reasoning("let me ".to_string())),
            Ok(StreamEvent::ReasoningBlock {
                text: "let me look".to_string(),
                signature: Some("sig-abc".to_string()),
            }),
            Ok(StreamEvent::Text("answer".to_string())),
            Ok(StreamEvent::Done),
        ];
        Ok(Box::new(events.into_iter()))
    }
}

#[test]
fn completes_without_tools() {
    let _guard = run_lock();
    let mut agent = agent_with(vec![Message::assistant("all done")]);
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("hello", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);
    assert_eq!(agent.history().len(), 2);
}

#[test]
fn a_warm_request_bills_into_the_session_usage() {
    let _guard = run_lock();
    let mut agent = agent_with(vec![Message::assistant("all done")]);
    agent.config.cache_warming = CacheWarming::Streaming;
    let mut sink = crate::events::CollectingSink::default();
    agent.run("hello", &mut sink).expect("run");
    assert_eq!(agent.warm_requests(), 1);
    // 主请求 10/5（ScriptedProvider 固定上报），暖请求同样 10/5。
    let usage = agent.usage();
    assert_eq!(usage.prompt_tokens, 20);
    assert_eq!(usage.completion_tokens, 10);
}

#[test]
fn warming_off_stays_out_of_the_usage_totals() {
    let _guard = run_lock();
    let mut agent = agent_with(vec![Message::assistant("all done")]);
    let mut sink = crate::events::CollectingSink::default();
    agent.run("hello", &mut sink).expect("run");
    assert_eq!(agent.warm_requests(), 0);
    let usage = agent.usage();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
}

#[test]
fn on_run_start_fires_once_per_turn() {
    let _guard = run_lock();
    let mut config = AgentConfig::new("test-model");
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&fired);
    config.on_run_start = Some(Arc::new(move || {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let provider = Box::new(ScriptedProvider::new(vec![
        Message::assistant("one"),
        Message::assistant("two"),
    ]));
    let mut agent = Agent::new(
        provider,
        Arc::new(ToolRegistry::with_builtins()),
        config,
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    agent.run("first", &mut sink).expect("run");
    agent.run("second", &mut sink).expect("run");
    // 检查点按“每次用户 prompt”编号，而不是按模型请求编号。
    assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn a_steering_message_joins_the_conversation_before_the_request() {
    let _guard = run_lock();
    let mut agent = agent_with(vec![Message::assistant("done")]);
    agent.steer("also check the tests");
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("hello", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);
    assert_eq!(agent.history().len(), 3);
    assert_eq!(agent.history()[1].text(), "also check the tests");
    assert!(sink
        .events
        .iter()
        .any(|event| matches!(event, AgentEvent::Notice(text) if text.contains("steering"))));
}

/// A provider whose every open fails, counting the calls it sees.
struct FailingProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl Provider for FailingProvider {
    fn name(&self) -> &str {
        "failing"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    fn chat(
        &self,
        _request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Err(Error::provider("provider is down"))
    }
}

#[test]
fn an_open_failure_is_not_retried_by_the_agent() {
    // The provider owns open retries (status-aware, honors Retry-After).
    // Retrying here as well multiplied the two layers.
    let _guard = run_lock();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut agent = Agent::new(
        Box::new(FailingProvider {
            calls: Arc::clone(&calls),
        }),
        Arc::new(ToolRegistry::with_builtins()),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("go", &mut sink).expect("run");
    assert!(matches!(reason, FinishReason::Error(_)));
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the agent must leave open retries to the provider"
    );
}

#[test]
fn executes_a_tool_then_finishes() {
    let _guard = run_lock();
    let mut assistant = Message::assistant("reading");
    assistant.tool_calls = vec![ToolCall::new(
        "c1",
        "write",
        r#"{"path":"argent-agent-test.txt","content":"x"}"#,
    )];
    let mut agent = agent_with(vec![assistant, Message::assistant("done")]);
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("write a file", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);

    let tool_events: Vec<_> = sink
        .events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolFinished { .. }))
        .collect();
    assert_eq!(tool_events.len(), 1);
    let _ = std::fs::remove_file("argent-agent-test.txt");
}

/// A read-only tool that records how many copies ran at once.
struct TrackingRead {
    active: Arc<std::sync::atomic::AtomicUsize>,
    peak: Arc<std::sync::atomic::AtomicUsize>,
}

impl Tool for TrackingRead {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "fake read"
    }

    fn parameters(&self) -> Json {
        argent_tools::schema_object(vec![], vec![])
    }

    fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
        let now = self
            .active
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        self.peak
            .fetch_max(now, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(120));
        self.active
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolResult::ok("body"))
    }
}

#[test]
fn read_only_calls_run_in_parallel_and_keep_their_order() {
    let _guard = run_lock();
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::with_core();
    registry.register(Box::new(TrackingRead {
        active: Arc::clone(&active),
        peak: Arc::clone(&peak),
    }));

    let mut assistant = Message::assistant("reading two files");
    assistant.tool_calls = vec![
        ToolCall::new("a", "read", r#"{"path":"a.txt"}"#),
        ToolCall::new("b", "read", r#"{"path":"b.txt"}"#),
    ];
    let provider = Box::new(ScriptedProvider::new(vec![
        assistant,
        Message::assistant("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    let started = Instant::now();
    let reason = agent.run("read both", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);

    // Both calls were inside `execute` at the same time.
    assert_eq!(
        peak.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the two reads should overlap"
    );
    // Results reach the transcript in call order, not completion order.
    let finished: Vec<String> = sink
        .events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolFinished { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(finished, vec!["a".to_string(), "b".to_string()]);
    // Two 120 ms reads that overlap are one wait, not two.
    assert!(
        started.elapsed() < std::time::Duration::from_millis(400),
        "the batch took {:?}",
        started.elapsed()
    );
}

/// A read-only tool that counts how many times it actually ran.
struct CountingRead {
    executions: Arc<std::sync::atomic::AtomicUsize>,
}

impl Tool for CountingRead {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "fake read"
    }

    fn parameters(&self) -> Json {
        argent_tools::schema_object(vec![], vec![])
    }

    fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
        self.executions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolResult::ok("body"))
    }
}

/// A bash stand-in that records any call that slips past the policy.
struct CountingBash {
    executions: Arc<std::sync::atomic::AtomicUsize>,
}

impl Tool for CountingBash {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "fake bash"
    }

    fn parameters(&self) -> Json {
        argent_tools::schema_object(vec![], vec![])
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
        self.executions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolResult::ok("ran"))
    }
}

/// A mutating tool that stands in for `write`.
struct FakeWrite;

impl Tool for FakeWrite {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "fake write"
    }

    fn parameters(&self) -> Json {
        argent_tools::schema_object(vec![], vec![])
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn execute(&self, _ctx: &ToolCtx, _args: &Json) -> Result<ToolResult> {
        Ok(ToolResult::ok("updated"))
    }
}

fn tool_turn(id: &str, name: &str, args: &str) -> Message {
    let mut message = Message::assistant("using a tool");
    message.tool_calls = vec![ToolCall::new(id, name, args)];
    message
}

#[test]
fn repeated_calls_match_regardless_of_argument_order() {
    let first = ToolCall::new("a", "read", r#"{"path":"a.txt","offset":1}"#);
    let second = ToolCall::new("b", "read", r#"{"offset":1,"path":"a.txt"}"#);
    assert_eq!(repeat_key(&first), repeat_key(&second));
    // A tool outside the guard keeps its own identity.
    assert!(repeat_key(&ToolCall::new("c", "bash", "{}")).is_none());
}

#[test]
fn an_identical_read_is_not_executed_twice() {
    let _guard = run_lock();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::with_core();
    registry.register(Box::new(CountingRead {
        executions: Arc::clone(&executions),
    }));

    let provider = Box::new(ScriptedProvider::new(vec![
        tool_turn("a", "read", r#"{"path":"a.txt"}"#),
        tool_turn("b", "read", r#"{"path":"a.txt"}"#),
        Message::assistant("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("read the file twice", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);

    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second identical read must be answered from the first result"
    );
    let warnings: Vec<_> = agent
        .history()
        .iter()
        .filter(|message| message.role == argent_ai::Role::Tool)
        .filter(|message| message.text().contains("duplicate call skipped"))
        .collect();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].text().contains("nothing has changed"));
}

#[test]
fn a_policy_block_stops_the_run_when_the_model_asks_again() {
    let _guard = run_lock();
    // The model keeps asking for the same denied path. The first answer
    // explains the refusal; the second one ends the run, because a
    // refusal the model can repeat forever is not a refusal.
    let call = tool_turn("p1", "read", r#"{"path":"secret.env"}"#);
    let mut agent = agent_with(vec![call.clone(), call]);
    agent.config.policy = Some(Arc::new(|name, _args| {
        if name == "read" {
            Some("the path is denied by policy".to_string())
        } else {
            None
        }
    }));
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("read the secret", &mut sink).expect("run");
    match reason {
        FinishReason::PolicyBlocked(message) => assert!(message.contains("denied by policy")),
        other => panic!("expected a policy stop, got {:?}", other),
    }
    // 触发停止的那次调用也必须有结果，否则历史无法重放。
    assert_every_call_is_answered(&agent);
}

#[test]
fn whitespace_only_retries_of_a_denied_bash_stop_the_run() {
    let _guard = run_lock();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::with_core();
    registry.register(Box::new(CountingBash {
        executions: Arc::clone(&executions),
    }));

    // The same denied command is asked for three times with only
    // whitespace changed. The second refusal must end the run: the third
    // call never reaches the policy gate or the tool registry.
    let mut second_turn = Message::assistant("trying again");
    second_turn.tool_calls = vec![
        ToolCall::new("b2", "bash", r#"{"command":"echo  denied"}"#),
        ToolCall::new("b3", "bash", r#"{"command":"  echo denied  "}"#),
    ];
    let mut agent = Agent::new(
        Box::new(ScriptedProvider::new(vec![
            tool_turn("b1", "bash", r#"{"command":"echo denied"}"#),
            second_turn,
            Message::assistant("should never be requested"),
        ])),
        Arc::new(registry),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    agent.config.max_turns = 8;
    agent.config.policy = Some(Arc::new(|name, _args| {
        (name == "bash").then(|| "the command is denied by policy".to_string())
    }));
    let decisions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&decisions);
    agent.config.audit = Some(Arc::new(move |_, _, decision, _| {
        recorder.lock().unwrap().push(decision.to_string());
    }));

    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("run the command", &mut sink).expect("run");
    match reason {
        FinishReason::PolicyBlocked(message) => {
            assert!(message.contains("denied by policy"));
        }
        other => panic!("expected a policy stop, got {:?}", other),
    }
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no denied bash call may reach the tool"
    );
    assert_eq!(
        *decisions.lock().unwrap(),
        vec!["blocked".to_string(), "blocked-again".to_string()]
    );
    assert_every_call_is_answered(&agent);
    assert!(agent
        .history()
        .iter()
        .any(|message| message.text() == UNRUN_POLICY));
    let assistant_turns = agent
        .history()
        .iter()
        .filter(|message| message.role == argent_ai::Role::Assistant)
        .count();
    assert_eq!(
        assistant_turns, 2,
        "the run must stop at the second refusal"
    );
    assert!(
        assistant_turns < agent.config.max_turns,
        "the run must not burn its whole turn budget"
    );
}

#[test]
fn a_policy_block_is_audited() {
    let _guard = run_lock();
    let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = Arc::clone(&seen);
    let call = tool_turn("a1", "read", r#"{"path":"secret.env"}"#);
    let mut agent = agent_with(vec![call]);
    agent.config.policy = Some(Arc::new(|_, _| Some("denied".to_string())));
    agent.config.audit = Some(Arc::new(move |name, _args, decision, _reason| {
        recorder
            .lock()
            .unwrap()
            .push((name.to_string(), decision.to_string()));
    }));
    let mut sink = crate::events::CollectingSink::default();
    let _ = agent.run("read the secret", &mut sink).expect("run");
    let entries = seen.lock().unwrap().clone();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "read");
    assert_eq!(entries[0].1, "blocked");
}

#[test]
fn a_read_after_an_edit_runs_again() {
    // 全局中断标志是所有测试共享的；不拿这把锁就会被并发的中断测试打断。
    let _guard = run_lock();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::with_core();
    registry.register(Box::new(CountingRead {
        executions: Arc::clone(&executions),
    }));
    registry.register(Box::new(FakeWrite));

    let provider = Box::new(ScriptedProvider::new(vec![
        tool_turn("a", "read", r#"{"path":"a.txt"}"#),
        tool_turn("b", "write", r#"{"path":"a.txt","content":"changed"}"#),
        tool_turn("c", "read", r#"{"path":"a.txt"}"#),
        Message::assistant("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("edit then re-read", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);

    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "a mutation must invalidate the earlier read"
    );
    assert!(!agent
        .history()
        .iter()
        .any(|message| message.text().contains("duplicate call skipped")));
}

#[test]
fn a_pre_tool_hook_can_block_a_call() {
    let _guard = run_lock();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::with_core();
    registry.register(Box::new(CountingRead {
        executions: Arc::clone(&executions),
    }));

    let provider = Box::new(ScriptedProvider::new(vec![
        tool_turn("a", "read", r#"{"path":"a.txt"}"#),
        Message::assistant("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    agent.config.pre_tool = Some(Arc::new(|name, _args| {
        if name == "read" {
            Some("blocked by hook".to_string())
        } else {
            None
        }
    }));
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("read", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the hook must prevent the tool from running"
    );
    assert!(agent
        .history()
        .iter()
        .any(|message| message.text().contains("blocked by hook")));
}

#[test]
fn honours_the_turn_limit() {
    let _guard = run_lock();
    let mut assistant = Message::assistant("loop");
    assistant.tool_calls = vec![ToolCall::new("c1", "current_time", "{}")];
    let mut agent = agent_with(vec![assistant.clone(), assistant.clone(), assistant]);
    agent.config.max_turns = 2;
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("loop forever", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::TurnLimit);
}

#[test]
fn a_token_budget_stops_before_the_next_tool_batch() {
    let _guard = run_lock();
    let assistant = tool_turn("m1", "current_time", "{}");
    let mut agent = agent_with(vec![assistant, Message::assistant("never reached")]);
    agent.config.budget = Some(Budget {
        max_total_tokens: Some(1),
        ..Budget::default()
    });
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("spend the budget", &mut sink).expect("run");
    match reason {
        FinishReason::Budget(message) => assert!(message.contains("tokens used")),
        other => panic!("expected a budget stop, got {:?}", other),
    }
    assert!(sink.events.iter().any(|event| matches!(
        event,
        AgentEvent::Notice(text) if text.contains("budget stopped")
    )));
}

#[test]
fn abort_stops_the_run() {
    let _guard = run_lock();
    let mut agent = agent_with(vec![Message::assistant("x")]);
    agent.abort_handle().abort();
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("go", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Aborted);
}

#[test]
fn an_abort_reaches_a_running_tool() {
    let _guard = run_lock();
    let saw_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut registry = ToolRegistry::with_core();
    registry.register(Box::new(WaitingTool {
        saw_cancel: Arc::clone(&saw_cancel),
    }));
    let provider = Box::new(ScriptedProvider::new(vec![
        tool_turn("w", "wait", "{}"),
        Message::assistant("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let abort = agent.abort_handle();
    let trigger = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        abort.abort();
    });
    let started = Instant::now();
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("wait until aborted", &mut sink).expect("run");
    trigger.join().unwrap();
    // 工具本身必须看到取消，而不是等它自己跑满 10 秒。
    assert!(
        saw_cancel.load(std::sync::atomic::Ordering::SeqCst),
        "the running tool never saw the abort"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "abort took {:?}",
        started.elapsed()
    );
    assert_eq!(reason, FinishReason::Aborted);
}

#[test]
fn estimates_tokens() {
    let messages = vec![Message::user("hello world")];
    assert!(estimate_tokens(&messages) >= 2);
}

#[test]
fn chinese_is_not_undercounted() {
    // Four Chinese characters are ~6-8 real tokens. A byte-based
    // `len() / 4` would report 3, which under-counts by more than 2x.
    let messages = vec![Message::user("你好世界")];
    let estimate = estimate_tokens(&messages);
    assert!(
        estimate >= 6,
        "estimated {} tokens for 4 CJK chars",
        estimate
    );
}

#[test]
fn ascii_stays_near_four_chars_per_token() {
    let messages = vec![Message::user("abcdefghijklmnop")];
    assert!(estimate_tokens(&messages) <= 10);
}

#[test]
fn ctrl_c_flag_aborts_the_run() {
    let _guard = run_lock();
    let mut agent = agent_with(vec![Message::assistant("x")]);
    argent_core::interrupt::request();
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("go", &mut sink).expect("run");
    argent_core::interrupt::clear();
    assert_eq!(reason, FinishReason::Aborted);
}

#[test]
fn an_abort_does_not_poison_the_next_run() {
    let _guard = run_lock();
    let mut agent = agent_with(vec![Message::assistant("x"), Message::assistant("y")]);
    agent.abort_handle().abort();
    let mut sink = crate::events::CollectingSink::default();
    assert_eq!(
        agent.run("go", &mut sink).expect("run"),
        FinishReason::Aborted
    );
    // 中止只作用于这一次运行：运行退出时句柄和全局 Ctrl+C 标志都复位。
    assert_eq!(
        agent.run("again", &mut sink).expect("run"),
        FinishReason::Complete
    );
}

#[test]
fn an_interrupt_mid_batch_answers_the_calls_that_never_ran() {
    let _guard = run_lock();
    let mut registry = ToolRegistry::with_core();
    registry.register(Box::new(InterruptingTool {
        name: "trip",
        mutating: false,
    }));
    let mut assistant = Message::assistant("two calls");
    assistant.tool_calls = vec![
        ToolCall::new("a", "trip", "{}"),
        ToolCall::new("b", "read", r#"{"path":"never.txt"}"#),
    ];
    let provider = Box::new(ScriptedProvider::new(vec![
        assistant,
        Message::assistant("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    assert_eq!(
        agent.run("go", &mut sink).expect("run"),
        FinishReason::Aborted
    );
    assert_every_call_is_answered(&agent);
    let unrun = agent
        .history()
        .iter()
        .find(|message| message.tool_call_id.as_deref() == Some("b"))
        .expect("the unrun call is answered");
    assert!(unrun.text().contains("interrupted"));
}

#[test]
fn a_parallel_abort_answers_the_calls_in_later_chunks() {
    let _guard = run_lock();
    let mut registry = ToolRegistry::with_core();
    registry.register(Box::new(InterruptingTool {
        name: "read",
        mutating: false,
    }));
    let mut assistant = Message::assistant("many calls");
    assistant.tool_calls = (0..MAX_PARALLEL_TOOLS + 2)
        .map(|index| {
            let args = format!(r#"{{"path":"f{}.txt"}}"#, index);
            ToolCall::new(format!("r{}", index), "read", args)
        })
        .collect();
    let provider = Box::new(ScriptedProvider::new(vec![
        assistant,
        Message::assistant("done"),
    ]));
    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    assert_eq!(
        agent.run("go", &mut sink).expect("run"),
        FinishReason::Aborted
    );
    assert_every_call_is_answered(&agent);
    // 整批在开始时就宣告过了，所以每个调用都应该收到 ToolFinished：
    // 第一批是真实结果，第二批是补出来的“未执行”。
    let finished = sink
        .events
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolFinished { .. }))
        .count();
    assert_eq!(finished, MAX_PARALLEL_TOOLS + 2);
}

#[test]
fn a_budget_stop_answers_the_tool_calls_it_skipped() {
    let _guard = run_lock();
    let assistant = tool_turn("m1", "current_time", "{}");
    let mut agent = agent_with(vec![assistant, Message::assistant("never reached")]);
    agent.config.budget = Some(Budget {
        max_total_tokens: Some(1),
        ..Budget::default()
    });
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("spend the budget", &mut sink).expect("run");
    assert!(matches!(reason, FinishReason::Budget(_)));
    assert_every_call_is_answered(&agent);
}

#[test]
fn a_signed_thinking_block_stays_on_the_assistant_message() {
    let _guard = run_lock();
    let mut agent = Agent::new(
        Box::new(ThinkingProvider),
        Arc::new(ToolRegistry::with_builtins()),
        AgentConfig::new("test-model"),
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("hi", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);
    let assistant = agent
        .history()
        .iter()
        .find(|message| message.role == argent_ai::Role::Assistant)
        .expect("assistant message");
    assert!(matches!(
        assistant.content.first(),
        Some(Content::Thinking { text, signature })
            if text == "let me look" && signature == "sig-abc"
    ));
    assert_eq!(assistant.text(), "answer");
}

#[test]
fn a_system_prompt_names_the_working_directory() {
    let index = vec![("read".to_string(), "Read a file".to_string())];
    let prompt = build_system_prompt(std::path::Path::new("/tmp"), &index, "extra");
    assert!(prompt.contains("/tmp"));
    assert!(prompt.contains("extra"));
}

#[test]
fn the_prompt_pins_language_missing_context_and_no_guessing() {
    let prompt = build_system_prompt(std::path::Path::new("/tmp"), &[], "");
    assert!(prompt.contains("think in Chinese"));
    assert!(prompt.contains("bare number"));
    assert!(prompt.contains("ask once and wait"));
    assert!(prompt.contains("Don't make things up"));
}

#[test]
fn the_tool_index_is_not_repeated_in_the_prompt() {
    // The schemas carry the descriptions, so a second copy would be paid on
    // every request. This guards against reintroducing it.
    let index = vec![("read".to_string(), "Read a file".to_string())];
    let prompt = build_system_prompt(std::path::Path::new("/tmp"), &index, "");
    assert!(!prompt.contains("read: Read a file"));
}

#[test]
fn the_prompt_stays_small() {
    // A baseline of a few thousand tokens is what made a one-word message
    // expensive; this is the tripwire.
    let index: Vec<(String, String)> = Vec::new();
    let prompt = build_system_prompt(std::path::Path::new("/tmp"), &index, "");
    assert!(
        prompt.len() < 1200,
        "system prompt grew to {} chars",
        prompt.len()
    );
}

#[test]
fn small_tool_results_pass_through_untouched() {
    let content = "ok\n";
    assert_eq!(
        prune_tool_result(content, TOOL_RESULT_TOKEN_BUDGET),
        content
    );
}

#[test]
fn a_small_history_is_not_pruned() {
    let mut agent = agent_with(Vec::new());
    agent.set_history(vec![
        Message::user("hi"),
        Message::tool_result("c1", "bash", "small output"),
    ]);
    assert!(agent.prune_old_tool_results().is_none());
}

#[test]
fn old_tool_results_are_pruned_and_recent_ones_survive() {
    let mut agent = agent_with(Vec::new());
    let mut history = vec![Message::user("start")];
    // Instructions must survive even when they are the oldest row.
    history.push(Message::tool_result(
        "s1",
        "skill",
        "keep these instructions",
    ));
    for index in 0..100 {
        history.push(Message::tool_result(
            format!("c{}", index),
            "bash",
            format!("result {}\n{}", index, "x".repeat(8000)),
        ));
    }
    agent.set_history(history);

    let (count, freed) = agent.prune_old_tool_results().expect("pruned");
    assert!(count >= 10, "pruned only {}", count);
    assert!(freed > 0);

    let skill = agent
        .history()
        .iter()
        .find(|message| message.name.as_deref() == Some("skill"))
        .expect("skill");
    assert_eq!(skill.text(), "keep these instructions");

    let last = agent.history().last().expect("last");
    assert!(last.text().contains("result 99"));

    let oldest_bash = agent
        .history()
        .iter()
        .find(|message| {
            message.role == argent_ai::Role::Tool && message.name.as_deref() == Some("bash")
        })
        .expect("bash");
    assert_eq!(oldest_bash.text(), PRUNED_TOOL_OUTPUT);

    // Running it again is a no-op once nothing else is outside the window.
    assert!(agent.prune_old_tool_results().is_none());
}

#[test]
fn large_tool_results_keep_head_and_tail() {
    let mut content = String::from("FIRST LINE\n");
    for i in 0..20_000 {
        content.push_str(&format!("line {}\n", i));
    }
    content.push_str("LAST LINE");
    let pruned = prune_tool_result(&content, 400);
    assert!(pruned.starts_with("FIRST LINE"));
    assert!(pruned.ends_with("LAST LINE"));
    assert!(pruned.contains("middle pruned"));
    // The whole point: the model must not receive the full text.
    assert!(
        pruned.len() < content.len() / 2,
        "pruned to {}",
        pruned.len()
    );
    // And it must stay well inside the token budget.
    assert!(estimate_text_tokens(&pruned) < 600);
}

#[test]
fn pruning_does_not_split_a_cjk_character() {
    let content = "开头".repeat(20_000);
    let pruned = prune_tool_result(&content, 400);
    assert!(pruned.starts_with("开头"));
    assert!(pruned.ends_with("开头"));
    assert!(pruned.contains("middle pruned"));
    // A split scalar value would have produced a replacement character.
    assert!(!pruned.contains('\u{FFFD}'));
}

#[test]
fn a_zero_budget_disables_pruning() {
    let content = "x".repeat(100_000);
    assert_eq!(prune_tool_result(&content, 0), content);
}

#[test]
fn an_explicit_off_effort_never_reaches_the_request() {
    let mut agent = agent_with(Vec::new());
    agent.set_thinking("off");
    assert_eq!(agent.effective_effort(), None);
    agent.set_thinking("disabled");
    assert_eq!(agent.effective_effort(), None);
    agent.set_thinking("high");
    assert_eq!(agent.effective_effort(), Some("high".to_string()));
}

#[test]
fn auto_effort_scales_with_the_request() {
    let mut agent = agent_with(Vec::new());
    agent.set_history(vec![Message::user("1+1?")]);
    assert_eq!(agent.auto_effort(), "low");
    agent.set_history(vec![Message::user(
        "请重构 crates/agent 的循环，并分析为什么这么慢，顺便排查性能问题",
    )]);
    assert_eq!(agent.auto_effort(), "high");
}

#[test]
fn the_turn_budget_reduces_later_results_to_pointers() {
    let big = "line of tool output\n".repeat(5_000);
    let mut remaining = 300usize;
    let first = bound_with_budgets("read", &big, false, &mut remaining, 1_000, 300);
    assert!(!first.is_empty());
    assert!(
        estimate_text_tokens(&first) <= 400,
        "{}",
        estimate_text_tokens(&first)
    );
    assert!(remaining < 100, "remaining {}", remaining);

    let second = bound_with_budgets("read", &big, false, &mut remaining, 1_000, 300);
    assert!(second.contains("omitted"), "{}", second);
    assert!(second.contains("full output"), "{}", second);
}

#[test]
fn a_failing_build_is_reduced_to_diagnostics() {
    let mut log = String::new();
    for index in 0..2_000 {
        log.push_str(&format!("noise line {}\n", index));
    }
    log.push_str("error[E0425]: cannot find value `x` in this scope\n");
    log.push_str(" --> src/main.rs:12:5\n");
    log.push_str("test result: FAILED. 2 passed; 1 failed\n");
    let mut remaining = 20_000usize;
    let bounded = bound_with_budgets("bash", &log, false, &mut remaining, 6_000, 20_000);
    assert!(bounded.contains("E0425"), "{}", bounded);
    assert!(bounded.contains("src/main.rs:12:5"), "{}", bounded);
    assert!(bounded.contains("FAILED"), "{}", bounded);
    assert!(bounded.contains("[full output:"), "{}", bounded);
    assert!(!bounded.contains("noise line 1999"), "{}", bounded);
    assert!(estimate_text_tokens(&bounded) < 2_000);
}

#[test]
fn an_oversized_result_spills_the_full_text_and_points_at_it() {
    let content = "line\n".repeat(20_000);
    let path = write_tool_spill("read", &content).expect("spill");
    assert!(path.is_file());
    assert_eq!(std::fs::read_to_string(&path).expect("read spill"), content);

    let bounded = prune_tool_result_with_spill(&content, 200, Some(&path));
    assert!(bounded.contains("middle pruned"));
    assert!(
        bounded.contains(&path.display().to_string()),
        "the model needs the path: {}",
        bounded
    );
    assert!(estimate_text_tokens(&bounded) < 400);
    let _ = std::fs::remove_file(&path);
}
// ---- P0 回归：顺序工具路径不丢图片结果 ----

#[test]
fn a_tool_result_message_keeps_images_for_vision() {
    let call = ToolCall::new("call-img", "browser", r#"{"url":"https://x"}"#);
    let outcome = ToolOutcome {
        content: "screenshot saved".to_string(),
        is_error: false,
        images: vec!["file:///tmp/shot.png".to_string()],
    };
    let message = tool_result_message(&call, &outcome, outcome.content.clone());
    assert_eq!(message.tool_call_id.as_deref(), Some("call-img"));
    assert!(
        message
            .content
            .iter()
            .any(|part| matches!(part, Content::Image { .. })),
        "the sequential path must not drop images"
    );

    // 没有图片时退化为纯文本结果。
    let plain = ToolOutcome {
        images: Vec::new(),
        ..outcome
    };
    let message = tool_result_message(&call, &plain, plain.content.clone());
    assert!(!message
        .content
        .iter()
        .any(|part| matches!(part, Content::Image { .. })));
}

// ---- P0 回归：set_history 重置 run 级状态，compaction 保留 usage ----

#[test]
fn set_history_resets_run_state_but_preserving_keeps_usage() {
    let _guard = run_lock();
    let mut agent = agent_with(vec![Message::assistant("done")]);
    let mut sink = crate::events::CollectingSink::default();
    agent.run("hello", &mut sink).expect("run");
    assert_eq!(agent.usage().total_tokens, 15);
    assert_eq!(agent.requests(), 1);

    // set_history：全新对话，run 级状态一并清零。
    agent.set_history(vec![Message::user("next topic")]);
    assert_eq!(agent.usage().total_tokens, 0);
    assert_eq!(agent.requests(), 0);

    // set_history_preserving_usage：压缩属于进行中的 run，usage 保留。
    agent.run("again", &mut sink).expect("run");
    let usage = agent.usage().total_tokens;
    let requests = agent.requests();
    assert!(usage > 0);
    agent.set_history_preserving_usage(vec![Message::user("compacted")]);
    assert_eq!(agent.usage().total_tokens, usage);
    assert_eq!(agent.requests(), requests);
}

// ---- P0 回归：Empty 流重试只按被采纳的 attempt 计费 ----

/// 第一次流只送了 usage 就以错误收场：零内容，可安全重试。
struct EmptyFirstProvider {
    calls: Mutex<usize>,
}

impl Provider for EmptyFirstProvider {
    fn name(&self) -> &str {
        "empty-first"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    fn chat(
        &self,
        _request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        let n = {
            let mut calls = self.calls.lock().expect("lock");
            let current = *calls;
            *calls += 1;
            current
        };
        if n == 0 {
            return Ok(Box::new(
                vec![
                    Ok(StreamEvent::Usage(Usage {
                        prompt_tokens: 10,
                        completion_tokens: 5,
                        total_tokens: 15,
                        cached_tokens: 0,
                    })),
                    Err(Error::provider("silent stream")),
                ]
                .into_iter(),
            ));
        }
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::Text("done".to_string())),
            Ok(StreamEvent::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
            })),
            Ok(StreamEvent::Done),
        ];
        Ok(Box::new(events.into_iter()))
    }
}

#[test]
fn an_empty_stream_retry_bills_usage_once() {
    let _guard = run_lock();
    // 第一条流没有文本也没有工具调用，只有 usage：这是空流，应当重试。
    let registry = Arc::new(ToolRegistry::with_builtins());
    let provider = Box::new(EmptyFirstProvider {
        calls: Mutex::new(0),
    });
    let config = AgentConfig::new("test-model");
    let mut agent = Agent::new(
        provider,
        registry,
        config,
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("hello", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);
    assert_eq!(agent.requests(), 2, "an empty stream must be retried");
    assert_eq!(
        agent.usage().total_tokens,
        15,
        "usage must be billed once, from the adopted attempt"
    );
    assert_eq!(
        agent.history().last().map(|message| message.text()),
        Some("done".to_string())
    );
}

// ---- Fallback 模型链：主模型空流耗尽重试后切备用模型 ----

/// 主模型（`primary`）每次都返回空流；备用模型（其他名字）正常完成。
/// 按请求里的 model 名分流，验证 agent 真的换了模型。
struct FallbackProvider {
    primary: String,
    calls: Mutex<Vec<String>>,
}

impl Provider for FallbackProvider {
    fn name(&self) -> &str {
        "fallback-test"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    fn chat(
        &self,
        request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        self.calls.lock().expect("lock").push(request.model.clone());
        if request.model == self.primary {
            return Ok(Box::new(
                vec![Err(Error::provider("gateway 504 timed out"))].into_iter(),
            ));
        }
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::Text(format!("saved by {}", request.model))),
            Ok(StreamEvent::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
            })),
            Ok(StreamEvent::Done),
        ];
        Ok(Box::new(events.into_iter()))
    }
}

#[test]
fn an_exhausted_primary_falls_back_to_the_backup_model() {
    let _guard = run_lock();
    let registry = Arc::new(ToolRegistry::with_builtins());
    let provider = Box::new(FallbackProvider {
        primary: "primary-model".to_string(),
        calls: Mutex::new(Vec::new()),
    });
    let mut config = AgentConfig::new("primary-model");
    config.fallback_model = Some("backup-model".to_string());
    config.stream_retries = 2;
    let mut agent = Agent::new(
        provider,
        registry,
        config,
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("hello", &mut sink).expect("run");
    assert_eq!(reason, FinishReason::Complete);
    // 2 次主模型重试 + 1 次备用模型。
    assert_eq!(agent.requests(), 3, "primary budget then one fallback shot");
    let last = agent
        .history()
        .last()
        .map(|message| message.text())
        .expect("answer");
    assert_eq!(last, "saved by backup-model");
    let notices: Vec<String> = sink
        .events
        .iter()
        .filter_map(|event| match event {
            crate::events::AgentEvent::Notice(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        notices
            .iter()
            .any(|text| text.contains("falling back to backup-model")),
        "operator must be told the run switched models: {notices:?}"
    );
}

#[test]
fn without_a_fallback_the_exhausted_primary_fails_the_run() {
    let _guard = run_lock();
    let registry = Arc::new(ToolRegistry::with_builtins());
    let provider = Box::new(FallbackProvider {
        primary: "primary-model".to_string(),
        calls: Mutex::new(Vec::new()),
    });
    let mut config = AgentConfig::new("primary-model");
    config.stream_retries = 2;
    let mut agent = Agent::new(
        provider,
        registry,
        config,
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("hello", &mut sink).expect("run");
    assert!(
        matches!(reason, FinishReason::Error(_)),
        "no fallback configured: the run must fail with the stream error"
    );
    assert_eq!(agent.requests(), 2, "only the primary budget runs");
}

#[test]
fn a_permanent_error_never_switches_to_the_fallback_model() {
    // A 400 or a rejected key comes back from the second model too:
    // the switch must not even be attempted.
    let _guard = run_lock();
    let registry = Arc::new(ToolRegistry::with_builtins());
    let provider = Box::new(FallbackProvider {
        primary: "primary-model".to_string(),
        calls: Mutex::new(Vec::new()),
    });
    let mut config = AgentConfig::new("primary-model");
    config.fallback_model = Some("backup-model".to_string());
    config.stream_retries = 2;
    let mut agent = Agent::new(
        provider,
        registry,
        config,
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    // Swap the provider's primary error for a permanent one.
    agent.provider = Box::new(PermanentErrorProvider {
        calls: Mutex::new(0),
    });
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("hello", &mut sink).expect("run");
    assert!(matches!(reason, FinishReason::Error(message) if message.contains("401")));
    assert_eq!(
        agent.requests(),
        2,
        "only the primary budget runs; the backup is never tried"
    );
}

/// 与 FallbackProvider 同形，但错误是永久性的（401）。
struct PermanentErrorProvider {
    calls: Mutex<usize>,
}

impl Provider for PermanentErrorProvider {
    fn name(&self) -> &str {
        "permanent-test"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    fn chat(
        &self,
        _request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        let mut calls = self.calls.lock().expect("lock");
        *calls += 1;
        Ok(Box::new(
            vec![Err(Error::provider("401 unauthorized"))].into_iter(),
        ))
    }
}

#[test]
fn transient_error_classification_matches_the_cli_contract() {
    for message in [
        "429 too many requests",
        "500 internal server error",
        "502 bad gateway",
        "503 overloaded",
        "504 gateway timeout",
        "request timed out",
        "connection reset by peer",
        "connection refused",
    ] {
        assert!(
            is_transient_model_error(message),
            "{} should be transient",
            message
        );
    }
    for message in [
        "400 bad request: unknown model",
        "model not found",
        "401 unauthorized",
        "403 forbidden",
        "invalid api key",
    ] {
        assert!(
            !is_transient_model_error(message),
            "{} should not be transient",
            message
        );
    }
}

// ---- P0 回归：Partial 流保留完整工具调用并配对结果 ----

/// 第一次流在发出完整工具调用后被切断；之后恢复为正常脚本。
struct FlakyStreamProvider {
    calls: Mutex<usize>,
}

impl Provider for FlakyStreamProvider {
    fn name(&self) -> &str {
        "flaky-stream"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    fn chat(
        &self,
        _request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        let n = {
            let mut calls = self.calls.lock().expect("lock");
            let current = *calls;
            *calls += 1;
            current
        };
        if n == 0 {
            let call = ToolCall::new("call-cut", "read", r#"{"path":"a.txt"}"#);
            return Ok(Box::new(
                vec![
                    Ok(StreamEvent::ToolCall(call)),
                    Err(Error::provider("stream cut")),
                ]
                .into_iter(),
            ));
        }
        let events: Vec<Result<StreamEvent>> = vec![
            Ok(StreamEvent::Text("recovered".to_string())),
            Ok(StreamEvent::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
            })),
            Ok(StreamEvent::Done),
        ];
        Ok(Box::new(events.into_iter()))
    }
}

#[test]
fn a_partial_stream_keeps_the_tool_call_and_pairs_it() {
    let _guard = run_lock();
    let registry = Arc::new(ToolRegistry::with_builtins());
    let provider = Box::new(FlakyStreamProvider {
        calls: Mutex::new(0),
    });
    let config = AgentConfig::new("test-model");
    let mut agent = Agent::new(
        provider,
        registry,
        config,
        ToolCtx::new(std::env::temp_dir()),
        "system",
    );
    let mut sink = crate::events::CollectingSink::default();
    let reason = agent.run("hello", &mut sink).expect("run");
    // 截断的回答要留下并报错，不能静默重放操作者已读过的内容。
    assert!(matches!(reason, FinishReason::Error(_)));
    // 每个 tool_use 都有配对的 tool_result，包括流断掉的那个。
    assert_every_call_is_answered(&agent);
    let paired = agent.history().iter().any(|message| {
        message.tool_call_id.as_deref() == Some("call-cut")
            && message.text().contains("model stream ended")
    });
    assert!(paired, "the cut call must get an error result");

    // 恢复后继续跑：usage 只来自被采纳的 attempt。
    let reason = agent.continue_run(&mut sink).expect("continue");
    assert_eq!(reason, FinishReason::Complete);
    assert_eq!(agent.requests(), 2);
    assert_eq!(agent.usage().total_tokens, 15);
    assert_eq!(
        agent.history().last().map(|message| message.text()),
        Some("recovered".to_string())
    );
}
