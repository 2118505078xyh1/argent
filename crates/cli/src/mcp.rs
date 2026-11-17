//! `argent mcp`：查看服务器、登录、登出。
//!
//! 参数解析单独成函数，方便单测；真正的动作只有三个：列出状态、走一次
//! OAuth 登录、删掉某个服务器已保存的令牌。

use std::path::Path;

use argent_core::error::{Error, Result};
use argent_mcp::{McpAuthStore, McpConfig, McpLogin, TransportKind};

/// 解析后的 `argent mcp` 子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpCommand {
    /// 列出服务器与状态。
    List,
    /// 登录一个服务器；`no_browser` 时只打印授权地址。
    Login { server: String, no_browser: bool },
    /// 忘掉一个服务器的凭据。
    Logout { server: String },
}

/// 解析 `argent mcp ...` 的参数。
pub(crate) fn parse(args: &[String]) -> Result<McpCommand> {
    let mut no_browser = false;
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--no-browser" => no_browser = true,
            // 全局的 -C/--cwd 由 cwd_from 处理，这里跳过后面的取值。
            "--cwd" | "-C" => index += 1,
            other if other.starts_with('-') => {
                return Err(Error::config(format!("unknown mcp option '{}'", other)));
            }
            other => positional.push(other.to_string()),
        }
        index += 1;
    }

    let action = positional.first().map(String::as_str).unwrap_or("list");
    match action {
        "list" | "ls" => {
            if positional.len() > 1 {
                return Err(Error::config("usage: argent mcp list"));
            }
            Ok(McpCommand::List)
        }
        "login" => {
            let server = positional
                .get(1)
                .cloned()
                .ok_or_else(|| Error::config("usage: argent mcp login <server> [--no-browser]"))?;
            Ok(McpCommand::Login { server, no_browser })
        }
        "logout" => {
            let server = positional
                .get(1)
                .cloned()
                .ok_or_else(|| Error::config("usage: argent mcp logout <server>"))?;
            Ok(McpCommand::Logout { server })
        }
        other => Err(Error::config(format!(
            "unknown mcp command '{}'; expected list, login or logout",
            other
        ))),
    }
}

/// `argent mcp` 的用法文本。
const USAGE: &str = "\
usage: argent mcp list
       argent mcp login <server> [--no-browser]
       argent mcp logout <server>

login opens a browser for OAuth; with --no-browser it only prints the
authorization URL. Tokens are stored in <storage>/mcp-auth.json.";

/// 执行 `argent mcp ...`。
pub(crate) fn run(args: &[String]) -> Result<i32> {
    if crate::commands::wants_help(args) {
        println!("{}", USAGE);
        return Ok(0);
    }
    let command = parse(args)?;
    let cwd = crate::commands::cwd_from(args);
    match command {
        McpCommand::List => list(&cwd),
        McpCommand::Login { server, no_browser } => login(&cwd, &server, no_browser),
        McpCommand::Logout { server } => logout(&cwd, &server),
    }
}

/// 列出每个服务器：传输方式、登录状态、工具数、最近一次连接错误。
fn list(cwd: &Path) -> Result<i32> {
    let config = McpConfig::load(cwd)?;
    if config.servers.is_empty() {
        println!(
            "no MCP servers configured; add one to {}",
            cwd.join(".argent").join("mcp.json").display()
        );
        return Ok(0);
    }

    println!(
        "{:<20} {:<8} {:<14} {:>5}  status",
        "name", "transport", "auth", "tools"
    );
    let mut failed = false;
    for status in config.probe() {
        let auth = if status.kind == TransportKind::Stdio {
            "n/a".to_string()
        } else {
            status.auth.describe().to_string()
        };
        let (tools, state) = match (&status.error, status.disabled) {
            (_, true) => ("-".to_string(), "disabled".to_string()),
            (None, _) => (status.tools.to_string(), "ok".to_string()),
            (Some(error), _) => {
                failed = true;
                ("-".to_string(), error.clone())
            }
        };
        println!(
            "{:<20} {:<8} {:<14} {:>5}  {}",
            status.name,
            status.kind.as_str(),
            auth,
            tools,
            state
        );
    }
    // 与 pi 一致：只要有一个启用的服务器连不上，退出码就是 1。
    Ok(if failed { 1 } else { 0 })
}

/// 走一次 OAuth 登录。
fn login(cwd: &Path, name: &str, no_browser: bool) -> Result<i32> {
    let config = McpConfig::load(cwd)?;
    let Some(server) = config.server(name) else {
        let known = config
            .servers
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::config(format!(
            "unknown MCP server '{}'; configured servers: {}",
            name,
            if known.is_empty() { "(none)" } else { &known }
        )));
    };
    if server.kind == TransportKind::Stdio {
        return Err(Error::config(format!(
            "MCP server '{}' runs over stdio; OAuth applies to http servers only",
            name
        )));
    }
    let store_path = config
        .auth_store_path()
        .ok_or_else(|| Error::config("no MCP auth store path resolved"))?
        .to_path_buf();
    let mut store = McpAuthStore::at(&store_path);
    let oauth = server.oauth.clone().unwrap_or_default();
    let mut flow = McpLogin {
        server_name: &server.name,
        server_url: &server.endpoint,
        oauth: &oauth,
        store: &mut store,
    };
    match flow.login(!no_browser) {
        Ok(_) => {
            println!(
                "signed in to MCP server '{}'; token stored in {}",
                server.name,
                store_path.display()
            );
            Ok(0)
        }
        Err(error) => {
            eprintln!(
                "OAuth login failed for MCP server '{}': {}",
                server.name, error
            );
            Ok(1)
        }
    }
}

/// 删除一个服务器已保存的凭据。
fn logout(cwd: &Path, name: &str) -> Result<i32> {
    let config = McpConfig::load(cwd)?;
    let store_path = match config.auth_store_path() {
        Some(path) => path.to_path_buf(),
        None => return Ok(0),
    };
    let mut store = McpAuthStore::at(&store_path);
    let removed = match config.server(name) {
        Some(server) => store.remove(&server.name, &server.endpoint)?,
        // 服务器已经从配置里删掉了：按名字清掉所有记录。
        None => store.remove_name(name)? > 0,
    };
    if removed {
        println!("signed out of MCP server '{}'", name);
    } else {
        println!("no stored credential for MCP server '{}'", name);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn help_is_handled_before_positional_arguments_are_parsed() {
        // --help 不是 mcp 的参数，必须在 parse() 之前被 run() 接住，否则会
        // 报 "unknown mcp option" 并以退出码 1 结束。
        assert_eq!(run(&args(&["--help"])).expect("help"), 0);
        assert_eq!(run(&args(&["-h"])).expect("help"), 0);
    }

    #[test]
    fn list_is_the_default_action() {
        assert_eq!(parse(&args(&[])).expect("parse"), McpCommand::List);
        assert_eq!(parse(&args(&["list"])).expect("parse"), McpCommand::List);
        assert_eq!(parse(&args(&["ls"])).expect("parse"), McpCommand::List);
    }

    #[test]
    fn parses_login_and_logout() {
        assert_eq!(
            parse(&args(&["login", "sentry"])).expect("parse"),
            McpCommand::Login {
                server: "sentry".to_string(),
                no_browser: false,
            }
        );
        assert_eq!(
            parse(&args(&["login", "sentry", "--no-browser"])).expect("parse"),
            McpCommand::Login {
                server: "sentry".to_string(),
                no_browser: true,
            }
        );
        assert_eq!(
            parse(&args(&["logout", "sentry"])).expect("parse"),
            McpCommand::Logout {
                server: "sentry".to_string(),
            }
        );
    }

    #[test]
    fn cwd_flags_do_not_become_servers() {
        assert_eq!(
            parse(&args(&["login", "-C", "some/dir", "sentry"])).expect("parse"),
            McpCommand::Login {
                server: "sentry".to_string(),
                no_browser: false,
            }
        );
    }

    #[test]
    fn missing_or_unknown_arguments_are_readable() {
        assert!(parse(&args(&["login"])).is_err());
        assert!(parse(&args(&["logout"])).is_err());
        assert!(parse(&args(&["frobnicate"])).is_err());
        assert!(parse(&args(&["--wat"])).is_err());
        assert!(parse(&args(&["list", "extra"])).is_err());
    }
}
