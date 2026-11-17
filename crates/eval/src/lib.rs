//! An evaluation harness for the agent loop.
//!
//! A unit test proves that a function returns what it should. It does not
//! prove that the agent, its tools and its policy still agree with each
//! other after a change. That is what an eval is for: a set of scenarios, each
//! with a scripted model, run against the real tool registry, checked against
//! what the agent actually did.
//!
//! The model is scripted, so an eval is deterministic and costs nothing. The
//! tools are real, so a broken tool fails the eval. The point is to catch
//! regressions in the loop, the tools and the policy together.
//!
//! ```json
//! {
//!   "name": "reads a file before answering",
//!   "prompt": "what is in notes.txt?",
//!   "script": [
//!     {"tool": "read", "arguments": {"path": "notes.txt"}}
//!   ],
//!   "expect": {
//!     "tools": ["read"],
//!     "text_contains": ["notes"],
//!     "reason": "complete"
//!   }
//! }
//! ```
//!
//! A scenario that must be stopped by a limit sets `"repeat": true`: the
//! script then plays in a loop, so the model never stops on its own and the
//! turn limit is what ends the run.

use std::path::Path;
use std::sync::{Arc, Mutex};

use argent_agent::{Agent, AgentConfig, AgentEvent, EventSink, FinishReason};
use argent_ai::{ChatRequest, ModelInfo, Provider, StreamEvent, ToolCall};
use argent_core::error::Result;
use argent_core::json::Json;
use argent_tools::{CheckpointStore, ToolCtx, ToolRegistry};

/// One scripted model turn.
#[derive(Debug, Clone)]
pub struct Turn {
    /// The text the model says, if any.
    pub text: Option<String>,
    /// The tool the model calls, if any.
    pub tool: Option<(String, Json)>,
}

impl Turn {
    /// A turn that only speaks.
    pub fn say(text: &str) -> Turn {
        Turn {
            text: Some(text.to_string()),
            tool: None,
        }
    }

    /// A turn that calls a tool.
    pub fn call(name: &str, arguments: &str) -> Turn {
        Turn {
            text: None,
            tool: Some((
                name.to_string(),
                Json::parse(arguments).unwrap_or_else(|_| Json::obj()),
            )),
        }
    }
}

/// What an eval expects to have happened.
#[derive(Debug, Clone, Default)]
pub struct Expect {
    /// The tools that ran, in order.
    pub tools: Vec<String>,
    /// Text the final answer must contain.
    pub text_contains: Vec<String>,
    /// Text a tool result must contain.
    pub output_contains: Vec<String>,
    /// The reason the run must end with.
    pub reason: Option<String>,
    /// A path the run must not have touched.
    pub deny_paths: Vec<String>,
    /// 回滚最近一次运行后，工作区必须与场景初始文件完全一致。
    ///
    /// 用于验证检查点链路：运行开始 → write/edit 记录 → rewind 恢复。
    pub rewind_restores: bool,
}

/// One scenario.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    pub prompt: String,
    pub script: Vec<Turn>,
    pub expect: Expect,
    /// The files the scenario starts with, as `name` → content.
    pub files: Vec<(String, String)>,
    /// Whether the script loops.
    ///
    /// A run that must be stopped by a limit needs a model that does not stop
    /// on its own. Playing the script forever gives it one: every turn calls a
    /// tool, so the loop only ends when the turn budget runs out.
    pub repeat: bool,
    /// The turn budget for this scenario.
    pub max_turns: Option<usize>,
    /// The token budget for this scenario.
    pub max_total_tokens: Option<u64>,
    /// The spend ceiling for this scenario, when prices are configured.
    pub max_cost_usd: Option<f64>,
    /// Prices used to turn the scripted token counts into a cost.
    pub price_input_per_mtok: Option<f64>,
    pub price_output_per_mtok: Option<f64>,
    /// Paths the policy denies.
    pub deny_paths: Vec<String>,
}

impl Scenario {
    /// Parse a scenario from JSON.
    pub fn from_json(value: &Json) -> Result<Scenario> {
        let name = value.str_or("name", "scenario").to_string();
        let prompt = value.str_or("prompt", "").to_string();
        let mut script = Vec::new();
        if let Some(turns) = value.get("script").and_then(Json::as_array) {
            for turn in turns {
                let text = turn.get("text").and_then(Json::as_str).map(str::to_string);
                let tool = match turn.get("tool").and_then(Json::as_str) {
                    Some(name) => {
                        let arguments = turn.get("arguments").cloned().unwrap_or_else(Json::obj);
                        Some((name.to_string(), arguments))
                    }
                    None => None,
                };
                script.push(Turn { text, tool });
            }
        }
        let mut expect = Expect::default();
        if let Some(block) = value.get("expect") {
            if let Some(tools) = block.get("tools").and_then(Json::as_array) {
                expect.tools = tools
                    .iter()
                    .filter_map(|tool| tool.as_str().map(str::to_string))
                    .collect();
            }
            if let Some(text) = block.get("textContains").and_then(Json::as_array) {
                expect.text_contains = text
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect();
            }
            if let Some(text) = block.get("outputContains").and_then(Json::as_array) {
                expect.output_contains = text
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect();
            }
            if let Some(reason) = block.get("reason").and_then(Json::as_str) {
                expect.reason = Some(reason.to_string());
            }
            if let Some(paths) = block.get("denyPaths").and_then(Json::as_array) {
                expect.deny_paths = paths
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect();
            }
            expect.rewind_restores = block
                .get("rewindRestores")
                .and_then(Json::as_bool)
                .unwrap_or(false);
        }
        let mut files = Vec::new();
        if let Some(entries) = value.get("files").and_then(Json::as_array) {
            for entry in entries {
                let path = entry.str_or("path", "");
                let content = entry.str_or("content", "");
                if !path.is_empty() {
                    files.push((path.to_string(), content.to_string()));
                }
            }
        }
        Ok(Scenario {
            name,
            prompt,
            script,
            expect,
            files,
            repeat: value.get("repeat").and_then(Json::as_bool).unwrap_or(false),
            max_turns: value
                .get("maxTurns")
                .and_then(Json::as_i64)
                .map(|n| n.max(1) as usize),
            max_total_tokens: value
                .get("maxTotalTokens")
                .and_then(Json::as_i64)
                .map(|n| n.max(0) as u64),
            max_cost_usd: value.get("maxCostUsd").and_then(Json::as_f64),
            price_input_per_mtok: value.get("priceInputPerMtok").and_then(Json::as_f64),
            price_output_per_mtok: value.get("priceOutputPerMtok").and_then(Json::as_f64),
            deny_paths: value
                .get("denyPaths")
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Parse a document that holds an array of scenarios.
    pub fn suite_from_json(value: &Json) -> Result<Vec<Scenario>> {
        let items: Vec<Json> = match value.get("scenarios").and_then(Json::as_array) {
            Some(items) => items.to_vec(),
            None => match value {
                Json::Arr(items) => items.clone(),
                _ => Vec::new(),
            },
        };
        let mut out = Vec::new();
        for item in items {
            out.push(Scenario::from_json(&item)?);
        }
        Ok(out)
    }
}

/// The outcome of one run.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub name: String,
    pub passed: bool,
    /// The tools that ran, in order.
    pub tools: Vec<String>,
    /// The final answer.
    pub text: String,
    /// Why the run ended.
    pub reason: String,
    /// Why the eval failed, when it did.
    pub failure: Option<String>,
}

impl Outcome {
    /// One line for a report.
    pub fn line(&self) -> String {
        let mark = if self.passed { "pass" } else { "FAIL" };
        match &self.failure {
            Some(reason) => format!("{} {}: {}", mark, self.name, reason),
            None => format!("{} {}", mark, self.name),
        }
    }
}

/// A provider that replays a script.
struct ScriptedProvider {
    script: Mutex<Vec<Turn>>,
    calls: Mutex<usize>,
    /// Play the script in a loop instead of stopping when it runs out.
    repeat: bool,
}

impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn models(&self) -> Result<Vec<ModelInfo>> {
        Ok(vec![ModelInfo {
            id: "scripted".to_string(),
            display_name: "Scripted".to_string(),
            context_length: 32_000,
            max_output: 0,
            supports_tools: true,
            supports_reasoning: false,
            supports_vision: true,
            reasoning_levels: Vec::new(),
        }])
    }

    fn chat(
        &self,
        _request: &ChatRequest,
    ) -> Result<Box<dyn Iterator<Item = Result<StreamEvent>> + Send>> {
        let script = self.script.lock().expect("lock").clone();
        let index = {
            let mut calls = self.calls.lock().expect("lock");
            let current = *calls;
            *calls += 1;
            current
        };
        let turn = if self.repeat && !script.is_empty() {
            script.get(index % script.len()).cloned()
        } else {
            script.get(index).cloned()
        };
        let mut events: Vec<Result<StreamEvent>> = Vec::new();
        match turn {
            Some(turn) => {
                if let Some(text) = turn.text {
                    events.push(Ok(StreamEvent::Text(text)));
                }
                if let Some((name, arguments)) = turn.tool {
                    // The id must be unique per request: an identical id on two
                    // calls in the same history confuses tool-result pairing.
                    let id = format!("call-{}", index);
                    events.push(Ok(StreamEvent::ToolCall(ToolCall::new(
                        &id,
                        &name,
                        arguments.dump(),
                    ))));
                }
            }
            None => {
                events.push(Ok(StreamEvent::Text("the script ended".to_string())));
            }
        }
        // Usage is reported on every turn, exactly as a real gateway does.
        // Without it a token budget could never fire in an eval, which would
        // make the budget scenarios silently useless.
        events.push(Ok(StreamEvent::Usage(argent_ai::Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: 0,
        })));
        events.push(Ok(StreamEvent::Done));
        Ok(Box::new(events.into_iter()))
    }
}

/// Collects what the agent did.
#[derive(Default)]
struct Recorder {
    tools: Vec<String>,
    text: String,
    /// Every tool result, so an expectation can check what the tool said.
    output: String,
    reason: Option<String>,
}

impl EventSink for Recorder {
    fn emit(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::ToolFinished { name, content, .. } => {
                self.tools.push(name);
                self.output.push_str(&content);
                self.output.push('\n');
            }
            AgentEvent::AssistantText(text) => self.text.push_str(&text),
            AgentEvent::Finished { reason } => self.reason = Some(reason.describe()),
            _ => {}
        }
    }
}

/// 回滚后的工作区必须和场景开始时一模一样：原有内容恢复，新建文件消失。
fn workspace_matches(
    scratch: &Path,
    files: &[(String, String)],
) -> std::result::Result<(), String> {
    for (path, content) in files {
        match std::fs::read_to_string(scratch.join(path)) {
            Ok(actual) if &actual == content => {}
            Ok(actual) => {
                return Err(format!(
                    "rewind left '{}' as '{}', expected '{}'",
                    path, actual, content
                ))
            }
            Err(_) => return Err(format!("rewind did not restore '{}'", path)),
        }
    }
    let mut expected: Vec<String> = files.iter().map(|(path, _)| path.clone()).collect();
    expected.sort();
    let mut actual: Vec<String> = argent_core::path::walk(scratch, 16)
        .into_iter()
        .filter(|entry| entry.path.is_file())
        .filter_map(|entry| {
            entry
                .path
                .strip_prefix(scratch)
                .ok()
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    actual.sort();
    if actual != expected {
        return Err(format!(
            "after rewind the workspace holds {:?}, expected {:?}",
            actual, expected
        ));
    }
    Ok(())
}

/// Run one scenario in a scratch directory.
///
/// The directory is removed afterwards: an eval leaves no trace on the
/// machine it ran on.
pub fn run(scenario: &Scenario, scratch: &Path) -> Outcome {
    let _ = std::fs::remove_dir_all(scratch);
    if std::fs::create_dir_all(scratch).is_err() {
        return Outcome {
            name: scenario.name.clone(),
            passed: false,
            tools: Vec::new(),
            text: String::new(),
            reason: String::new(),
            failure: Some("cannot create the scratch directory".to_string()),
        };
    }
    for (path, content) in &scenario.files {
        let target = scratch.join(path);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&target, content);
    }

    let registry = Arc::new(ToolRegistry::with_builtins());
    let provider = Box::new(ScriptedProvider {
        script: Mutex::new(scenario.script.clone()),
        calls: Mutex::new(0),
        repeat: scenario.repeat,
    });
    // 检查点目录放在工作区旁边，回滚比较时不会被当成 agent 新建的文件。
    let checkpoint_dir = scratch.with_extension("checkpoints");
    let _ = std::fs::remove_dir_all(&checkpoint_dir);
    let checkpoints = CheckpointStore::open(checkpoint_dir.clone());
    let mut ctx = ToolCtx::new(scratch.to_path_buf());
    ctx.vars
        .insert("cwd".to_string(), scratch.display().to_string());
    ctx.checkpoints = Some(checkpoints.clone());
    let mut config = AgentConfig::new("scripted");
    config.on_run_start = Some(Arc::new({
        let store = checkpoints.clone();
        move || {
            store.begin_run();
        }
    }));
    if let Some(limit) = scenario.max_turns {
        config.max_turns = limit;
    }
    if scenario.max_total_tokens.is_some()
        || scenario.max_cost_usd.is_some()
        || scenario.price_input_per_mtok.is_some()
    {
        config.budget = Some(argent_agent::Budget {
            max_total_tokens: scenario.max_total_tokens,
            max_cost_usd: scenario.max_cost_usd,
            price_input_per_mtok: scenario.price_input_per_mtok.unwrap_or(0.0),
            price_output_per_mtok: scenario.price_output_per_mtok.unwrap_or(0.0),
            ..argent_agent::Budget::default()
        });
    }
    if let Some(denied) = scenario.deny_paths.first() {
        // The scenario denies one path; the gate is the same shape the
        // operator's settings file produces.
        let denied = denied.clone();
        let cwd = scratch.to_path_buf();
        config.policy = Some(Arc::new(move |name, args| {
            let path = args.get("path").and_then(Json::as_str).unwrap_or("");
            if path.is_empty() {
                return None;
            }
            let target = cwd.join(path);
            if target.to_string_lossy().contains(&denied) {
                Some(format!("the path '{}' is denied by policy", path))
            } else {
                let _ = name;
                None
            }
        }));
    }
    let mut agent = Agent::new(provider, registry, config, ctx, "system");
    let mut recorder = Recorder::default();
    let reason = agent.run(&scenario.prompt, &mut recorder).ok();

    let mut rewind_failure = None;
    if scenario.expect.rewind_restores {
        let (_, errors) = checkpoints.rewind(1);
        rewind_failure = if errors.is_empty() {
            workspace_matches(scratch, &scenario.files).err()
        } else {
            Some(format!("rewind failed: {}", errors.join("; ")))
        };
    }
    let _ = std::fs::remove_dir_all(scratch);
    let _ = std::fs::remove_dir_all(&checkpoint_dir);

    let reason_name = reason
        .map(|reason| match reason {
            FinishReason::Complete => "complete".to_string(),
            FinishReason::TurnLimit => "turn limit reached".to_string(),
            FinishReason::Budget(message) => format!("budget limit: {}", message),
            FinishReason::PolicyBlocked(message) => format!("policy blocked: {}", message),
            FinishReason::Aborted => "aborted".to_string(),
            FinishReason::Error(message) => format!("error: {}", message),
        })
        .unwrap_or_else(|| "unknown".to_string());

    let mut failure = rewind_failure;
    if !scenario.expect.tools.is_empty() {
        let expected = &scenario.expect.tools;
        let actual = &recorder.tools;
        // The scenario names the tools it expects; extra tools mean the agent
        // did something the scenario did not ask for, which is worth failing.
        if expected != actual {
            failure = Some(format!("expected tools {:?}, ran {:?}", expected, actual));
        }
    }
    if failure.is_none() {
        for needle in &scenario.expect.text_contains {
            if !recorder.text.contains(needle.as_str()) {
                failure = Some(format!(
                    "the answer does not contain '{}' (got '{}')",
                    needle, recorder.text
                ));
                break;
            }
        }
    }
    if failure.is_none() {
        for needle in &scenario.expect.output_contains {
            if !recorder.output.contains(needle.as_str()) {
                failure = Some(format!(
                    "no tool result contains '{}' (got '{}')",
                    needle, recorder.output
                ));
                break;
            }
        }
    }
    if failure.is_none() {
        if let Some(expected) = &scenario.expect.reason {
            // A prefix match: `budget limit` matches `budget limit: 15 tokens
            // used (limit 1)`. The scenario names the class of stop, not the
            // exact diagnostic string, which would make the eval brittle.
            if !reason_name.starts_with(expected.as_str()) {
                failure = Some(format!(
                    "expected the run to end with '{}', got '{}'",
                    expected, reason_name
                ));
            }
        }
    }
    if failure.is_none() && !reason_name.starts_with("budget") {
        if let Some(limit) = scenario.max_cost_usd {
            if let Some(cost) = agent.usage_cost() {
                if cost > limit + 1e-9 {
                    failure = Some(format!(
                        "spend ${:.4} exceeded the ${:.4} ceiling",
                        cost, limit
                    ));
                }
            }
        }
    }
    Outcome {
        name: scenario.name.clone(),
        passed: failure.is_none(),
        tools: recorder.tools,
        text: recorder.text,
        reason: reason_name,
        failure,
    }
}

/// Run a whole suite, returning the outcomes.
pub fn run_suite(scenarios: &[Scenario], scratch: &Path) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    for (index, scenario) in scenarios.iter().enumerate() {
        let mut directory = scratch.to_path_buf();
        directory.push(format!("case-{}", index));
        outcomes.push(run(scenario, &directory));
    }
    outcomes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario(text: &str) -> Scenario {
        Scenario {
            name: "test".to_string(),
            prompt: "hello".to_string(),
            script: vec![Turn::say(text)],
            expect: Expect::default(),
            files: Vec::new(),
            repeat: false,
            max_turns: None,
            max_total_tokens: None,
            max_cost_usd: None,
            price_input_per_mtok: None,
            price_output_per_mtok: None,
            deny_paths: Vec::new(),
        }
    }

    #[test]
    fn a_repeating_script_ends_at_the_turn_limit() {
        // A scenario that must be stopped by a limit needs a model that will
        // not stop on its own; `repeat` is how it gets one.
        let scratch =
            std::env::temp_dir().join(format!("argent-eval-repeat-{}", std::process::id()));
        let mut scenario = scenario("");
        scenario.script = vec![Turn::call("current_time", "{}")];
        scenario.repeat = true;
        scenario.expect.reason = Some("turn limit reached".to_string());
        let outcome = run(&scenario, &scratch);
        assert!(outcome.passed, "{:?}", outcome.failure);
        assert_eq!(outcome.reason, "turn limit reached");
    }

    #[test]
    fn a_repeating_scenario_parses_from_json() {
        let value = Json::parse(
            r#"{"name":"loop","repeat":true,"script":[{"tool":"current_time","arguments":{}}]}"#,
        )
        .unwrap();
        let scenario = Scenario::from_json(&value).unwrap();
        assert!(scenario.repeat);
        let value = Json::parse(r#"{"name":"once","script":[{"text":"hi"}]}"#).unwrap();
        assert!(!Scenario::from_json(&value).unwrap().repeat);
    }

    #[test]
    fn a_scripted_run_reports_the_text() {
        let scratch = std::env::temp_dir().join(format!("argent-eval-{}", std::process::id()));
        let outcome = run(&scenario("the answer"), &scratch);
        assert!(outcome.passed, "{:?}", outcome.failure);
        assert!(outcome.text.contains("the answer"));
        assert_eq!(outcome.reason, "complete");
    }

    #[test]
    fn an_expectation_that_does_not_hold_fails_the_run() {
        let scratch = std::env::temp_dir().join(format!("argent-eval-{}", std::process::id()));
        let mut scenario = scenario("the answer");
        scenario.expect.text_contains = vec!["something else".to_string()];
        let outcome = run(&scenario, &scratch);
        assert!(!outcome.passed);
        assert!(outcome.failure.unwrap().contains("does not contain"));
    }

    #[test]
    fn a_scenario_parses_from_json() {
        let value = Json::parse(
            r#"{"name":"s","prompt":"p","script":[{"tool":"read","arguments":{"path":"a.txt"}}],
                "expect":{"tools":["read"],"reason":"complete"},
                "files":[{"path":"a.txt","content":"hello"}]}"#,
        )
        .unwrap();
        let scenario = Scenario::from_json(&value).unwrap();
        assert_eq!(scenario.name, "s");
        assert_eq!(scenario.script.len(), 1);
        assert_eq!(scenario.expect.tools, vec!["read"]);
        assert_eq!(scenario.files.len(), 1);
    }

    #[test]
    fn a_suite_parses_from_json() {
        let value = Json::parse(r#"{"scenarios":[{"name":"a"},{"name":"b"}]}"#).unwrap();
        let scenarios = Scenario::suite_from_json(&value).unwrap();
        assert_eq!(scenarios.len(), 2);
    }
}
