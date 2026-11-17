//! `replay` and `eval`: render a stored transcript, or run scripted scenarios.

use super::*;

pub(super) fn replay(args: &[String]) -> Result<i32> {
    let path = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .ok_or_else(|| {
            Error::config("usage: argent replay <session|events.jsonl> [--to-eval <name>]")
        })?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::io(format!("read {} failed: {}", path, e)))?;
    if let Some(name) = args
        .iter()
        .position(|arg| arg == "--to-eval")
        .and_then(|i| args.get(i + 1))
    {
        let scenario = session_to_eval(&text, name)?;
        let path = format!("{}.eval.json", name);
        std::fs::write(&path, scenario.dump_pretty())
            .map_err(|e| Error::io(format!("write {} failed: {}", path, e)))?;
        println!(
            "eval scenario written to {} (run it with argent eval)",
            path
        );
        return Ok(0);
    }
    for line in replay_lines(&text)? {
        println!("{}", line);
    }
    Ok(0)
}

/// Turn a recorded session into an eval scenario (P2.1: discovery ->
/// reproduction -> regression in one command).
///
/// The first user message becomes the scenario prompt; every assistant tool
/// call becomes a scripted turn, in order; the final answer seeds
/// `text_contains` and the tool sequence seeds `expect.tools`. Files the
/// session created via `write` become the scenario's initial files, so the
/// scripted run replays against the same starting point.
pub(super) fn session_to_eval(text: &str, name: &str) -> Result<Json> {
    let transcript = replay_transcript(text)?;
    let entries = transcript.entries();
    let mut prompt = String::new();
    let mut script: Vec<Json> = Vec::new();
    let mut tools: Vec<Json> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    let mut final_answer = String::new();
    for entry in entries {
        match entry {
            argent_ui_core::Entry::User(text) => {
                if prompt.is_empty() {
                    prompt = text.clone();
                }
            }
            argent_ui_core::Entry::Tool(run) => {
                tools.push(Json::string(run.name.clone()));
            }
            argent_ui_core::Entry::Assistant(text) => {
                final_answer = text.clone();
            }
            _ => {}
        }
    }
    // Rebuild the scripted turns from the tool runs: name + the arguments the
    // run recorded. Write calls double as initial files.
    for entry in entries {
        if let argent_ui_core::Entry::Tool(run) = entry {
            let args = Json::parse(&run.arguments).unwrap_or_else(|_| Json::obj());
            let mut turn = Json::obj();
            turn.set("tool", Json::string(run.name.clone()));
            turn.set("arguments", args.clone());
            script.push(turn);
            if run.name == "write" {
                if let Some(path_arg) = args.get("path").and_then(Json::as_str) {
                    if let Some(content) = args.get("content").and_then(Json::as_str) {
                        files.push((path_arg.to_string(), content.to_string()));
                    }
                }
            }
        }
    }
    let mut expect = Json::obj();
    expect.set("tools", Json::Arr(tools));
    let key = final_answer.lines().next().unwrap_or("").trim().to_string();
    if !key.is_empty() {
        expect.set("text_contains", Json::Arr(vec![Json::string(key)]));
    }
    let mut scenario = Json::obj();
    scenario.set("name", Json::string(name.to_string()));
    scenario.set("prompt", Json::string(prompt));
    scenario.set("script", Json::Arr(script));
    scenario.set("expect", expect);
    if !files.is_empty() {
        let mut files_obj = Json::obj();
        for (path, content) in files {
            files_obj.set(&path, Json::string(content));
        }
        scenario.set("files", files_obj);
    }
    Ok(scenario)
}

/// 重放的纯文本结果；输入无法识别时返回错误，而不是静默输出空内容。
pub(super) fn replay_transcript(text: &str) -> Result<argent_ui_core::Transcript> {
    let mut values = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value = Json::parse(line)
            .map_err(|e| Error::config(format!("line {} is not JSON: {}", index + 1, e)))?;
        values.push(value);
    }
    let first = values
        .first()
        .ok_or_else(|| Error::config("the input is empty"))?;
    let transcript = if first.get("kind").is_some() {
        let mut entries = Vec::new();
        for value in &values {
            let entry = argent_session::Entry::from_json(value).ok_or_else(|| {
                Error::config("a session line is missing its kind/message payload")
            })?;
            entries.push(entry);
        }
        let messages = argent_session::entries_to_messages(&entries);
        argent_ui_core::Transcript::from_messages(&messages)
    } else {
        let mut transcript = argent_ui_core::Transcript::new();
        for value in &values {
            match value.str_or("type", "").as_str() {
                // `--mode json` 的会话头与最终结果行不是对话内容。
                "session" | "result" => continue,
                "error" => {
                    transcript
                        .push_notice(argent_ui_core::NoticeKind::Error, value.str_or("error", ""));
                    continue;
                }
                "event" => {
                    let payload = value.get("data").cloned().unwrap_or_else(|| value.clone());
                    let event = argent_agent::event_from_json(&payload)
                        .ok_or_else(|| Error::config("an event line is not a known agent event"))?;
                    transcript.apply(&event);
                }
                // 裸事件 JSON（手写日志或别的前端）。
                _ => {
                    let event = argent_agent::event_from_json(value)
                        .ok_or_else(|| Error::config("an event line is not a known agent event"))?;
                    transcript.apply(&event);
                }
            }
        }
        transcript
    };
    Ok(transcript)
}

/// The plain-text rendering of a recorded session.
pub(super) fn replay_lines(text: &str) -> Result<Vec<String>> {
    let transcript = replay_transcript(text)?;
    Ok(crate::tui::transcript_plain(&transcript))
}

// ------------------------------------------------------------------- eval

/// Run an evaluation suite.
///
/// The model is scripted, so the run is deterministic and free. The tools are
/// real, so a broken tool fails the eval. The exit code is the number of
/// failures, so CI can use it directly.
pub(super) fn eval(args: &[String]) -> Result<i32> {
    let path = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .cloned()
        .ok_or_else(|| Error::config("usage: argent eval <suite.json>"))?;
    let text = argent_core::read_text_file(std::path::Path::new(&path))?;
    let value =
        Json::parse(&text).map_err(|e| Error::config(format!("invalid suite {}: {}", path, e)))?;
    // Accept both shapes: a suite object with a `scenarios` array, and the
    // single-scenario shape that `replay --to-eval` produces.
    let scenarios = if value.get("scenarios").and_then(Json::as_array).is_some() {
        argent_eval::Scenario::suite_from_json(&value)?
    } else {
        vec![argent_eval::Scenario::from_json(&value)?]
    };
    if scenarios.is_empty() {
        println!("the suite has no scenarios");
        return Ok(0);
    }
    let scratch = std::env::temp_dir().join(format!("argent-eval-{}", std::process::id()));
    let outcomes = argent_eval::run_suite(&scenarios, &scratch);
    let mut failures = 0;
    for outcome in &outcomes {
        println!("{}", outcome.line());
        if !outcome.passed {
            failures += 1;
        }
    }
    println!(
        "\n{} of {} scenarios passed",
        outcomes.len() - failures,
        outcomes.len()
    );
    Ok(failures.min(1) as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_renders_event_logs_and_session_files() {
        // 事件日志：与 `--mode json` 捕获的包装格式一致；会话头与结果行跳过。
        let events = concat!(
            r#"{"type":"session","data":{"session":"s.jsonl","model":"m","cwd":"/w"}}"#,
            "\n",
            r#"{"id":1,"type":"event","event":"text","data":{"type":"assistant_text","text":"hello from events"}}"#,
            "\n",
            r#"{"type":"result","data":{"reason":"complete"}}"#,
            "\n"
        );
        let lines = replay_lines(events).expect("event log");
        assert!(
            lines.join("\n").contains("hello from events"),
            "{:?}",
            lines
        );

        // 会话文件：kind 行，经过消息 → transcript。
        let dir = std::env::temp_dir().join(format!("argent-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let entries = [
            argent_session::Entry::Message(argent_ai::Message::user("hello from session")),
            argent_session::Entry::Message(argent_ai::Message::assistant("the answer")),
        ];
        let text: String = entries
            .iter()
            .map(|entry| entry.to_json().dump() + "\n")
            .collect();
        let lines = replay_lines(&text).expect("session");
        let joined = lines.join("\n");
        assert!(joined.contains("hello from session"), "{}", joined);
        assert!(joined.contains("the answer"), "{}", joined);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
