//! Background sessions: `agents`, `logs`, `follow`, `attach`, `stop`, `rm`.

use super::*;

/// `argent agents`：列出后台会话。
pub(super) fn agents(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let json = args.iter().any(|arg| arg == "--json");
    let sessions = crate::background::list(&storage);
    if json {
        let items: Vec<Json> = sessions
            .iter()
            .map(|session| {
                let mut value = Json::obj();
                value.set("id", Json::string(session.id.clone()));
                value.set("pid", Json::num(session.pid as f64));
                value.set("prompt", Json::string(session.prompt.clone()));
                value.set("cwd", Json::string(session.cwd.clone()));
                value.set("model", Json::string(session.model.clone()));
                value.set("startedAt", Json::num(session.started_at as f64));
                value.set(
                    "running",
                    Json::bool(crate::background::process_alive(session.pid)),
                );
                value
            })
            .collect();
        println!("{}", Json::Arr(items).dump());
        return Ok(0);
    }
    if sessions.is_empty() {
        println!("no background sessions");
        return Ok(0);
    }
    for session in sessions {
        let state = if crate::background::process_alive(session.pid) {
            "running"
        } else {
            "exited"
        };
        let prompt: String = session.prompt.chars().take(60).collect();
        println!(
            "{:<24} {:<8} {:<8} {}",
            session.id, state, session.pid, prompt
        );
    }
    Ok(0)
}

/// `argent replay <file>`：离线把一份会话或事件日志渲染成 transcript。
///
/// 两种输入：
/// - 会话 JSONL（`{"kind":"message"|"header"|...}` 行）→ 消息 → transcript
/// - `--mode json` / RPC 捕获的事件流（`{"type":"event","data":{...}}` 行），
///   或裸事件 JSON（`{"type":"assistant_text",...}`）→ 逐个 apply
///
/// 输出与 TUI 退出时的回显一致（无颜色），既能当调试工具，也能在测试里
/// 断言重放结果。
/// 后台子命令共用的位置参数解析：跳过 `-C <dir>` 的值。
pub(super) fn background_target(args: &[String]) -> Option<String> {
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "-C" || arg == "--cwd" {
            skip = true;
            continue;
        }
        if !arg.starts_with('-') {
            return Some(arg.clone());
        }
    }
    None
}

pub(super) fn background_logs(args: &[String], follow_default: bool) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let follow = follow_default || args.iter().any(|arg| arg == "--follow" || arg == "-f");
    let id = background_target(args)
        .ok_or_else(|| Error::config("usage: argent logs <id> [--follow]"))?;
    let session = crate::background::load(&storage, &id)?;
    crate::background::print_logs(&storage, &session, follow)?;
    Ok(0)
}

pub(super) fn background_stop(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let id = background_target(args).ok_or_else(|| Error::config("usage: argent stop <id>"))?;
    let session = crate::background::load(&storage, &id)?;
    crate::background::stop(&session)?;
    println!("stopped {}", id);
    Ok(0)
}

pub(super) fn background_remove(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let id = background_target(args).ok_or_else(|| Error::config("usage: argent rm <id>"))?;
    let session = crate::background::load(&storage, &id)?;
    crate::background::remove(&storage, &session)?;
    println!("removed {}", id);
    Ok(0)
}

/// `argent attach <id>`：接管后台会话（看事件、插话、中止）。
///
/// 默认进终端界面；`--plain` 只把事件流打印出来。
pub(super) fn attach_session(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let id = background_target(args)
        .ok_or_else(|| Error::config("usage: argent attach <id> [--plain]"))?;
    let session = crate::background::load(&storage, &id)?;
    let plain = args.iter().any(|arg| arg == "--plain");
    crate::attach_session::attach(&storage, &session, plain)
}
