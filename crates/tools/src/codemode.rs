//! Code mode: let the model write one script that calls several tools.
//!
//! A tool call costs a full round trip. When the work is "read these twenty
//! files and count what matches", twenty round trips is twenty times the
//! latency and twenty times the tokens. Code mode replaces them with one
//! script.
//!
//! No script engine is embedded. The script runs on the operator's own `node`
//! or `python`, and reaches the tools through `argent.call(...)`, which is a
//! blocking HTTP request back into this process. That keeps the tool set exactly
//! the one the model was given its permissions by, and it keeps argent free of a
//! JavaScript runtime.
//!
//! ```python
//! hits = [argent.call("grep", pattern="TODO", path="src") for _ in range(1)]
//! print(hits[0])
//! ```

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_core::process::CommandSpec;

use crate::registry::ToolRegistry;
use crate::tool::{arg_str, schema_object, Tool, ToolCtx, ToolResult};

/// Executes one tool call on behalf of a script.
pub type ToolRunner = Arc<dyn Fn(&str, &Json) -> Result<ToolResult> + Send + Sync>;

/// A late-bound runner.
///
/// The tool has to exist before the registry that will answer its calls, so the
/// runner is installed afterwards through this slot.
pub type RunnerSlot = Arc<Mutex<Option<ToolRunner>>>;

/// 宿主注入的工具门禁。
///
/// 脚本里的每次 `argent.call` 都和模型直接发起的调用走同一套硬策略、前置钩子、
/// 审批与审计。没有它，`code` 就是一条绕过 `Policy` 的通道：模型只要把
/// `read` 换成一段脚本，denyPaths 就失效了。
#[derive(Clone, Default)]
pub struct ToolGate {
    /// 硬策略。返回 `Some(reason)` 表示拒绝。
    pub policy: Option<Arc<dyn Fn(&str, &Json) -> Option<String> + Send + Sync>>,
    /// 工具前置钩子。
    pub pre_tool: Option<Arc<dyn Fn(&str, &Json) -> Option<String> + Send + Sync>>,
    /// 工具后置钩子；第三个参数表示这次调用是否以错误结束。
    pub post_tool: Option<Arc<dyn Fn(&str, &Json, bool) + Send + Sync>>,
    /// 审批回调。返回 false 表示操作者拒绝。
    pub approve: Option<Arc<dyn Fn(&str, &Json) -> bool + Send + Sync>>,
    /// true 时每个 mutating 工具都要审批（对应 `AllMutating`）；false 时
    /// 只问 `needs_approval` 为真的工具。
    pub approve_all_mutating: bool,
    /// 审计回调。
    pub audit: Option<Arc<dyn Fn(&str, &Json, &str, Option<String>) + Send + Sync>>,
}

impl ToolGate {
    /// 把一次脚本发起的调用送过全部门禁。
    ///
    /// 门禁拒绝时返回 `Ok(ToolResult::error(..))`，脚本看到的是 `ok:false`
    /// 而不是异常；`Err` 留给未知工具这类真正的执行错误。
    pub fn run(
        &self,
        registry: &ToolRegistry,
        ctx: &ToolCtx,
        name: &str,
        args: &Json,
    ) -> Result<ToolResult> {
        // 顺序与 agent 的 execute_tool 一致：策略先于钩子，钩子先于审批。
        if let Some(policy) = &self.policy {
            if let Some(reason) = policy(name, args) {
                self.record(name, args, "blocked", Some(reason.clone()));
                return Ok(ToolResult::error(format!(
                    "The operator's policy blocked this {} call: {}. Do not try to reach the \
                     same effect another way.",
                    name, reason
                )));
            }
        }
        if let Some(pre) = &self.pre_tool {
            if let Some(reason) = pre(name, args) {
                self.record(name, args, "blocked", Some(reason.clone()));
                return Ok(ToolResult::error(reason));
            }
        }
        if let Some(approve) = &self.approve {
            let needs = if self.approve_all_mutating {
                registry
                    .get(name)
                    .map(|tool| tool.is_mutating())
                    .unwrap_or(false)
            } else {
                registry.needs_approval(name, args)
            };
            if needs && !approve(name, args) {
                self.record(name, args, "declined", None);
                if let Some(post) = &self.post_tool {
                    post(name, args, true);
                }
                return Ok(ToolResult::error(format!(
                    "The operator declined this {} call. Do not attempt the same effect through \
                     a different command; stop and ask the operator how to proceed.",
                    name
                )));
            }
        }
        let result = registry.execute(ctx, name, args);
        let is_error = result.as_ref().map(|r| r.is_error).unwrap_or(true);
        self.record(
            name,
            args,
            if is_error { "failed" } else { "allowed" },
            None,
        );
        if let Some(post) = &self.post_tool {
            post(name, args, is_error);
        }
        result
    }

    fn record(&self, name: &str, args: &Json, decision: &str, reason: Option<String>) {
        if let Some(audit) = &self.audit {
            audit(name, args, decision, reason);
        }
    }
}

/// The `code` tool.
pub struct CodeTool {
    runner: RunnerSlot,
}

impl CodeTool {
    pub fn new(runner: RunnerSlot) -> CodeTool {
        CodeTool { runner }
    }

    /// The tool with no runner installed; every call reports that clearly.
    pub fn disconnected() -> CodeTool {
        CodeTool::new(Arc::new(Mutex::new(None)))
    }
}

impl Tool for CodeTool {
    fn name(&self) -> &str {
        "code"
    }

    fn description(&self) -> &str {
        "Run one script that calls tools through argent.call(name, args). Use it to \
         replace many tool round trips with one: read many files, filter results, \
         loop. python or javascript."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                ("language", "string", "Interpreter: python or javascript"),
                ("source", "string", "The script body"),
            ],
            vec!["language", "source"],
        )
    }

    fn is_mutating(&self) -> bool {
        // The script may call write or bash, so this is at least as dangerous as
        // either of them.
        true
    }

    fn needs_approval(&self, _args: &Json) -> bool {
        true
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let language = arg_str(args, "language")?;
        let source = arg_str(args, "source")?;
        let runner = {
            let slot = self.runner.lock().unwrap_or_else(|e| e.into_inner());
            match slot.as_ref() {
                Some(runner) => Arc::clone(runner),
                None => {
                    return Ok(ToolResult::error(
                        "code mode has no tool runner installed in this build",
                    ))
                }
            }
        };
        run_script(&language, &source, ctx, runner)
    }
}

/// Which interpreter to use and how to hand it the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Python,
    JavaScript,
}

impl Flavor {
    fn parse(name: &str) -> Result<Flavor> {
        match name.trim().to_ascii_lowercase().as_str() {
            "python" | "py" | "python3" => Ok(Flavor::Python),
            "javascript" | "js" | "node" => Ok(Flavor::JavaScript),
            other => Err(Error::tool(format!(
                "unknown language '{}'; use python or javascript",
                other
            ))),
        }
    }

    fn programs(&self) -> &'static [&'static str] {
        match self {
            Flavor::Python => &["python", "python3"],
            Flavor::JavaScript => &["node"],
        }
    }

    fn extension(&self) -> &'static str {
        match self {
            Flavor::Python => "py",
            Flavor::JavaScript => "js",
        }
    }

    /// Code inserted before the model's script.
    fn prelude(&self) -> &'static str {
        match self {
            Flavor::Python => PYTHON_PRELUDE,
            Flavor::JavaScript => JS_PRELUDE,
        }
    }
}

const PYTHON_PRELUDE: &str = r#"import json as _json, os as _os, urllib.request as _request

class _Mu:
    def call(self, name, **kwargs):
        body = _json.dumps({"name": name, "args": kwargs}).encode("utf-8")
        request = _request.Request(
            _os.environ["ARGENT_CODE_URL"],
            data=body,
            headers={
                "Content-Type": "application/json",
                "X-Argent-Token": _os.environ["ARGENT_CODE_TOKEN"],
            },
        )
        with _request.urlopen(request, timeout=1800) as response:
            payload = _json.loads(response.read().decode("utf-8"))
        if not payload.get("ok"):
            raise RuntimeError(payload.get("content") or "tool failed")
        return payload.get("content")

argent = _Mu()
"#;

const JS_PRELUDE: &str = r#"const { execFileSync } = require("child_process");

function _call(name, args) {
  const body = JSON.stringify({ name: name, args: args || {} });
  const out = execFileSync("curl", [
    "-sS", "-X", "POST",
    "-H", "Content-Type: application/json",
    "-H", "X-Argent-Token: " + process.env.ARGENT_CODE_TOKEN,
    "--data-binary", body,
    process.env.ARGENT_CODE_URL,
  ], { encoding: "utf8", maxBuffer: 256 * 1024 * 1024 });
  const payload = JSON.parse(out);
  if (!payload.ok) throw new Error(payload.content || "tool failed");
  return payload.content;
}

globalThis.argent = { call: _call };
"#;

/// Run one script with a callback server up.
fn run_script(
    language: &str,
    source: &str,
    ctx: &ToolCtx,
    runner: ToolRunner,
) -> Result<ToolResult> {
    let flavor = Flavor::parse(language)?;
    let program = find_program(flavor).ok_or_else(|| {
        Error::tool(format!(
            "code mode needs one of {} on PATH",
            flavor.programs().join(" / ")
        ))
    })?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| Error::io(format!("cannot open the code-mode callback port: {}", e)))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::io(e.to_string()))?
        .port();
    let url = format!("http://127.0.0.1:{}/call", port);
    // 回调端口开着，本机任何进程都能连上来；脚本凭这个令牌证明自己是
    // 这次调用启动的，别的进程拿不到它。
    let token = script_token();

    let script = format!("{}{}", flavor.prelude(), source);
    // A counter, not just the clock: two scripts started in the same
    // millisecond would otherwise share a path and overwrite each other.
    let path = std::env::temp_dir().join(format!(
        "argent-code-{}-{}-{}.{}",
        std::process::id(),
        timestamp(),
        next_script_id(),
        flavor.extension()
    ));
    argent_core::write_text_file(&path, &script)?;

    // The server runs on its own thread and stops when the child exits: the
    // listener is closed here, which makes `accept` fail and end the loop.
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let server_calls = Arc::clone(&calls);
    let server_runner = Arc::clone(&runner);
    let server_ctx = ctx.clone();
    let server_token = token.clone();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_stop = Arc::clone(&stop);
    let server = std::thread::spawn(move || {
        for stream in listener.incoming() {
            if server_stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let stream = match stream {
                Ok(stream) => stream,
                Err(_) => break,
            };
            handle_connection(
                stream,
                &server_runner,
                &server_ctx,
                &server_calls,
                &server_token,
            );
        }
    });

    let seconds = ctx.timeout_secs.clamp(1, 1800);
    let spec = CommandSpec::new(&program)
        .arg(path.to_string_lossy().to_string())
        .cwd(ctx.cwd.to_string_lossy().to_string())
        .env("ARGENT_CODE_URL", &url)
        .env("ARGENT_CODE_TOKEN", &token)
        // 让 Python 脚本始终按 UTF-8 输出，不受 Windows 控制台代码页影响。
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONUTF8", "1")
        .timeout(std::time::Duration::from_secs(seconds))
        // 脚本可能跑很久；Esc/Ctrl+C 或子代理超时要能立刻停下。
        .cancel(ctx.cancel.clone());

    let output = argent_core::process::run(&spec);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    // Unblock the accept loop so the thread can end.
    let _ = TcpStream::connect(("127.0.0.1", port));
    // 脚本被中止时，服务器线程可能正卡在一次审批里；此时不等它收尾，
    // 否则中止会被审批对话框拖住。线程处理完那一次请求后自行退出。
    let interrupted = output.as_ref().map(|o| o.interrupted).unwrap_or(false);
    if !interrupted {
        let _ = server.join();
    }
    let _ = std::fs::remove_file(&path);

    let output = output?;
    let call_count = calls.lock().map(|list| list.len()).unwrap_or(0);
    let stdout = output.stdout.trim_end();
    let stderr = output.stderr.trim_end();

    let mut text = String::new();
    text.push_str(&format!(
        "{} tool call(s) from the script, exit {}\n",
        call_count, output.status
    ));
    if !stdout.is_empty() {
        text.push_str("--- stdout ---\n");
        text.push_str(&clip(stdout, 40_000));
        text.push('\n');
    }
    if !stderr.is_empty() {
        text.push_str("--- stderr ---\n");
        text.push_str(&clip(stderr, 8_000));
        text.push('\n');
    }
    if stdout.is_empty() && stderr.is_empty() {
        text.push_str("(the script printed nothing)\n");
    }
    let result = if output.interrupted {
        ToolResult::error(format!(
            "the script was interrupted by the operator\n\n{}",
            text
        ))
    } else if output.success() {
        ToolResult::ok(text)
    } else {
        ToolResult::error(format!(
            "the script exited with {}\n\n{}",
            output.status, text
        ))
    };
    let mut metadata = Json::obj();
    metadata.set("tool_calls", Json::num(call_count as f64));
    metadata.set("exit", Json::num(output.status as f64));
    metadata.set("interrupted", Json::bool(output.interrupted));
    Ok(result.with_metadata(metadata))
}

/// Answer one callback request.
fn handle_connection(
    mut stream: TcpStream,
    runner: &ToolRunner,
    _ctx: &ToolCtx,
    calls: &Arc<Mutex<Vec<String>>>,
    token: &str,
) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut content_length = 0usize;
    // 请求头里的令牌；缺失或不对就拒绝，避免本机其他进程借用这次运行。
    let mut presented = String::new();
    loop {
        let mut header = String::new();
        match reader.read_line(&mut header) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => return,
        }
        if header.trim().is_empty() {
            break;
        }
        if let Some((key, value)) = header.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
            if key.eq_ignore_ascii_case("x-argent-token") {
                presented = value.trim().to_string();
            }
        }
    }
    let mut body = vec![0u8; content_length.min(64 * 1024 * 1024)];
    if content_length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }
    let text = String::from_utf8_lossy(&body).to_string();

    if presented != token {
        respond(
            &mut stream,
            "403 Forbidden",
            "{\"ok\":false,\"content\":\"the callback request did not carry this script's token\"}",
        );
        return;
    }

    let parsed = Json::parse(&text).unwrap_or_else(|_| Json::obj());
    let name = parsed.str_or("name", "");
    let args = parsed.get("args").cloned().unwrap_or_else(Json::obj);

    let (ok, content) = if name.is_empty() {
        (false, "the callback needs a 'name' field".to_string())
    } else {
        match runner(&name, &args) {
            Ok(result) => (!result.is_error, result.content),
            Err(e) => (false, e.to_string()),
        }
    };
    if ok {
        if let Ok(mut list) = calls.lock() {
            list.push(name);
        }
    }

    let mut payload = Json::obj();
    payload.set("ok", Json::bool(ok));
    payload.set("content", Json::string(content));
    let body = payload.dump();
    respond(&mut stream, "200 OK", &body);
}

/// 写一个最小的 HTTP 响应并关闭连接。
fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    )
    .ok();
    let _ = stream.flush();
}

fn find_program(flavor: Flavor) -> Option<String> {
    for name in flavor.programs() {
        if let Some(found) = argent_core::process::which(name) {
            return Some(found);
        }
    }
    None
}

fn next_script_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 每次脚本一个不可预测的令牌。
fn script_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 纳秒时钟 + 进程号 + 计数器，同一进程里两次调用也不会撞。
    format!(
        "{:x}-{:x}-{:x}",
        nanos,
        std::process::id(),
        next_script_id()
    )
}

fn timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut boundary = max;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!(
        "{}\n… ({} bytes truncated)",
        &text[..boundary],
        text.len() - boundary
    )
}

/// The runtime viewer: which interpreters are available for code mode.
pub fn available_languages() -> BTreeMap<&'static str, Option<String>> {
    let mut map = BTreeMap::new();
    map.insert("python", find_program(Flavor::Python));
    map.insert("javascript", find_program(Flavor::JavaScript));
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolCtx {
        ToolCtx::new(std::env::temp_dir())
    }

    fn runner() -> ToolRunner {
        Arc::new(|name: &str, args: &Json| Ok(ToolResult::ok(format!("{}:{}", name, args.dump()))))
    }

    #[test]
    fn unknown_languages_are_rejected() {
        assert!(Flavor::parse("ruby").is_err());
        assert_eq!(Flavor::parse("PY").unwrap(), Flavor::Python);
        assert_eq!(Flavor::parse("node").unwrap(), Flavor::JavaScript);
    }

    #[test]
    fn the_python_prelude_exposes_argent_call() {
        assert!(PYTHON_PRELUDE.contains("argent = _Mu()"));
        assert!(PYTHON_PRELUDE.contains("ARGENT_CODE_URL"));
        assert!(PYTHON_PRELUDE.contains("ARGENT_CODE_TOKEN"));
        assert!(PYTHON_PRELUDE.contains("X-Argent-Token"));
    }

    #[test]
    fn the_javascript_prelude_exposes_argent_call() {
        assert!(JS_PRELUDE.contains("globalThis.argent"));
        assert!(JS_PRELUDE.contains("ARGENT_CODE_URL"));
        assert!(JS_PRELUDE.contains("ARGENT_CODE_TOKEN"));
        assert!(JS_PRELUDE.contains("X-Argent-Token"));
    }

    #[test]
    fn the_script_token_is_not_repeated() {
        let first = script_token();
        let second = script_token();
        assert_ne!(first, second);
        assert!(first.split('-').count() >= 3);
    }

    #[test]
    fn a_disconnected_tool_explains_itself() {
        let tool = CodeTool::disconnected();
        let result = tool
            .execute(
                &ctx(),
                &Json::parse(r#"{"language":"python","source":"print(1)"}"#).unwrap(),
            )
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("no tool runner"));
    }

    #[test]
    fn a_script_reaches_a_tool_and_prints() {
        let Some(_) = find_program(Flavor::Python) else {
            // The interpreter is optional; skip rather than fail on a machine
            // that does not have one.
            return;
        };
        let tool = CodeTool::new(Arc::new(Mutex::new(Some(runner()))));
        let result = tool
            .execute(
                &ctx(),
                &Json::parse(
                    r#"{"language":"python","source":"print(argent.call('read', path='x'))"}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("read:"), "{}", result.content);
        assert!(result.content.contains("1 tool call"), "{}", result.content);
    }

    #[test]
    fn a_failing_script_is_reported_as_an_error() {
        let Some(_) = find_program(Flavor::Python) else {
            return;
        };
        let tool = CodeTool::new(Arc::new(Mutex::new(Some(runner()))));
        let result = tool
            .execute(
                &ctx(),
                &Json::parse(r#"{"language":"python","source":"raise SystemExit(3)"}"#).unwrap(),
            )
            .unwrap();
        assert!(result.is_error, "{}", result.content);
        assert!(result.content.contains("exit"));
    }

    #[test]
    fn clipping_keeps_a_character_boundary() {
        // 7 bytes would split the third character; the clip backs off to 6.
        let text = "中文中文中文";
        let clipped = clip(text, 7);
        assert!(
            clipped.starts_with(
                "中文
"
            ),
            "{:?}",
            clipped
        );
        assert!(clipped.contains("truncated"));
    }

    /// 一个记录自己被调用过几次的测试工具。
    struct ProbeTool {
        executed: Arc<Mutex<usize>>,
        needs: bool,
        mutating: bool,
    }

    impl Tool for ProbeTool {
        fn name(&self) -> &str {
            "probe"
        }

        fn description(&self) -> &str {
            "test probe"
        }

        fn parameters(&self) -> Json {
            schema_object(vec![], vec![])
        }

        fn is_mutating(&self) -> bool {
            self.mutating
        }

        fn needs_approval(&self, _args: &Json) -> bool {
            self.needs
        }

        fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
            *self.executed.lock().unwrap() += 1;
            Ok(ToolResult::ok(format!("probe:{}", args.dump())))
        }
    }

    fn registry_with_probe(needs: bool, mutating: bool) -> (ToolRegistry, Arc<Mutex<usize>>) {
        let executed = Arc::new(Mutex::new(0usize));
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ProbeTool {
            executed: Arc::clone(&executed),
            needs,
            mutating,
        }));
        (registry, executed)
    }

    fn audit_sink() -> (Arc<Mutex<Vec<String>>>, impl Fn(&str, &Json, &str, Option<String>) + Send + Sync + 'static)
    {
        let decisions = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = Arc::clone(&decisions);
        let sink = move |_name: &str, _args: &Json, decision: &str, _reason: Option<String>| {
            if let Ok(mut list) = log.lock() {
                list.push(decision.to_string());
            }
        };
        (decisions, sink)
    }

    #[test]
    fn the_gate_blocks_a_call_the_policy_refuses() {
        let (registry, executed) = registry_with_probe(true, true);
        let (decisions, sink) = audit_sink();
        let gate = ToolGate {
            policy: Some(Arc::new(|name: &str, _args: &Json| {
                (name == "probe").then(|| "the test policy says no".to_string())
            })),
            audit: Some(Arc::new(sink)),
            ..ToolGate::default()
        };
        let result = gate.run(&registry, &ctx(), "probe", &Json::obj()).unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("test policy says no"));
        assert_eq!(*executed.lock().unwrap(), 0);
        assert_eq!(*decisions.lock().unwrap(), vec!["blocked"]);
    }

    #[test]
    fn the_gate_records_a_declined_call_and_does_not_run_it() {
        let (registry, executed) = registry_with_probe(true, true);
        let (decisions, sink) = audit_sink();
        let posts = Arc::new(Mutex::new(Vec::<bool>::new()));
        let log = Arc::clone(&posts);
        let gate = ToolGate {
            approve: Some(Arc::new(|_: &str, _: &Json| false)),
            post_tool: Some(Arc::new(move |_name: &str, _args: &Json, is_error: bool| {
                if let Ok(mut list) = log.lock() {
                    list.push(is_error);
                }
            })),
            audit: Some(Arc::new(sink)),
            ..ToolGate::default()
        };
        let result = gate.run(&registry, &ctx(), "probe", &Json::obj()).unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("declined"));
        assert_eq!(*executed.lock().unwrap(), 0);
        assert_eq!(*decisions.lock().unwrap(), vec!["declined"]);
        assert_eq!(*posts.lock().unwrap(), vec![true]);
    }

    #[test]
    fn the_gate_executes_an_approved_call_once() {
        let (registry, executed) = registry_with_probe(true, true);
        let (decisions, sink) = audit_sink();
        let posts = Arc::new(Mutex::new(Vec::<bool>::new()));
        let log = Arc::clone(&posts);
        let gate = ToolGate {
            approve: Some(Arc::new(|_: &str, _: &Json| true)),
            post_tool: Some(Arc::new(move |_name: &str, _args: &Json, is_error: bool| {
                if let Ok(mut list) = log.lock() {
                    list.push(is_error);
                }
            })),
            audit: Some(Arc::new(sink)),
            ..ToolGate::default()
        };
        let result = gate.run(&registry, &ctx(), "probe", &Json::obj()).unwrap();
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("probe:"));
        assert_eq!(*executed.lock().unwrap(), 1);
        assert_eq!(*decisions.lock().unwrap(), vec!["allowed"]);
        assert_eq!(*posts.lock().unwrap(), vec![false]);
    }

    #[test]
    fn all_mutating_asks_about_mutating_tools_without_their_own_flag() {
        let (registry, executed) = registry_with_probe(false, true);
        let gate = ToolGate {
            approve: Some(Arc::new(|_: &str, _: &Json| false)),
            approve_all_mutating: true,
            ..ToolGate::default()
        };
        let result = gate.run(&registry, &ctx(), "probe", &Json::obj()).unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("declined"));
        assert_eq!(*executed.lock().unwrap(), 0);
    }
}
