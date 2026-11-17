//! ZCode-style subcommands: `login`, `plugins`, `skills`, `sessions`, `hooks`,
//! `worktree`, `update`, `doctor`, and friends.
//!
//! These run before the agent is built. They only inspect or edit configuration
//! and extension directories, so they are cheap even on a cold binary.
//!
//! `dispatch` is the only entry point: it maps the first argument to one of the
//! submodules below, each of which owns a single command family and keeps its
//! own argument parsing, output and tests. The handful of helpers several
//! families share (`flag_value`, `clip`, `cwd_from`, …) stay in this module.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use argent_auth::{AuthStore, Credential};
use argent_config::{Paths, Settings};
use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_hooks::{digest_file, HookConfig, HookEvent, HookTrust};
use argent_plugins::{
    InstallPolicy, MarketplaceStore, PluginManager, PluginManifest, SignaturePolicy,
};
use argent_provider::ProviderCatalog;
use argent_tools::{Skill, SkillRegistry};

use crate::args::HELP;
use crate::export::{self, ExportMeta};

mod agents;
mod auth;
mod automation;
mod completion;
mod extensions;
mod ops;
mod replay;
mod sessions;
mod update;
mod worktree;

/// Run a subcommand when the first argument names one.
///
/// `None` means the argument list is an ordinary prompt or flag set and the
/// normal CLI path should handle it.
pub fn dispatch(raw: &[String]) -> Result<Option<i32>> {
    let Some(command) = raw.first() else {
        return Ok(None);
    };
    match command.as_str() {
        "login" => Ok(Some(auth::login(&raw[1..])?)),
        "logout" => Ok(Some(auth::logout(&raw[1..])?)),
        "auth" => Ok(Some(crate::auth_cmd::run(&raw[1..])?)),
        "models" => Ok(Some(crate::models_cmd::run(&raw[1..])?)),
        "mcp" => Ok(Some(crate::mcp::run(&raw[1..])?)),
        "plugins" | "plugin" => Ok(Some(extensions::plugins(&raw[1..])?)),
        "skills" => Ok(Some(extensions::skills(&raw[1..])?)),
        "commands" => Ok(Some(extensions::commands(&raw[1..])?)),
        "hooks" => Ok(Some(extensions::hooks(&raw[1..])?)),
        "workflow" => Ok(Some(automation::workflow(&raw[1..])?)),
        "telemetry" => Ok(Some(ops::telemetry(&raw[1..])?)),
        "sessions" => Ok(Some(sessions::sessions(&raw[1..])?)),
        "audit" => Ok(Some(ops::audit(&raw[1..])?)),
        "usage" => Ok(Some(ops::usage(&raw[1..])?)),
        "policy" => Ok(Some(ops::policy(&raw[1..])?)),
        "profiles" | "profile" => Ok(Some(ops::profiles(&raw[1..])?)),
        "themes" | "theme" => Ok(Some(ops::themes(&raw[1..])?)),
        "keybindings" | "keys" => Ok(Some(ops::keybindings(&raw[1..])?)),
        "automation" | "automations" | "schedule" => Ok(Some(automation::automation(&raw[1..])?)),
        "eval" => Ok(Some(replay::eval(&raw[1..])?)),
        "update" => Ok(Some(update::update(&raw[1..])?)),
        "completion" => Ok(Some(completion::completion(&raw[1..])?)),
        "agents" | "background" => Ok(Some(agents::agents(&raw[1..])?)),
        "worktree" | "worktrees" => Ok(Some(worktree::worktree(&raw[1..])?)),
        "replay" => Ok(Some(replay::replay(&raw[1..])?)),
        "attach" => Ok(Some(agents::attach_session(&raw[1..])?)),
        "logs" => Ok(Some(agents::background_logs(&raw[1..], false)?)),
        "follow" => Ok(Some(agents::background_logs(&raw[1..], true)?)),
        "stop" => Ok(Some(agents::background_stop(&raw[1..])?)),
        "rm" | "remove" => Ok(Some(agents::background_remove(&raw[1..])?)),
        "help" => {
            print!("{}", HELP);
            Ok(Some(0))
        }
        "version" => {
            println!("argent {}", argent_core::VERSION);
            Ok(Some(0))
        }
        "doctor" => Ok(Some(crate::doctor()?)),
        "bug" => Ok(Some(ops::bug_report(&raw[1..])?)),
        _ => Ok(None),
    }
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn positional_after<'a>(args: &'a [String], action: &str) -> Option<&'a str> {
    args.iter()
        .skip_while(|arg| arg.as_str() != action)
        .nth(1)
        .filter(|arg| !arg.starts_with('-'))
        .map(String::as_str)
}

pub(super) fn short_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

pub(super) fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn public_key(args: &[String]) -> Result<Option<[u8; 32]>> {
    if let Some(value) = flag_value(args, "--public-key") {
        return Ok(Some(argent_update::parse_public_key(value)?));
    }
    if let Ok(value) = std::env::var("ARGENT_UPDATE_PUBLIC_KEY") {
        if !value.trim().is_empty() {
            return Ok(Some(argent_update::parse_public_key(&value)?));
        }
    }
    Ok(None)
}

pub(crate) fn cwd_from(args: &[String]) -> PathBuf {
    let mut index = 0;
    while index < args.len() {
        if (args[index] == "--cwd" || args[index] == "-C" || args[index] == "--workspace")
            && index + 1 < args.len()
        {
            return PathBuf::from(&args[index + 1]);
        }
        index += 1;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 参数里是否请求了 `--help` / `-h`。子命令用它先打印用法再退出。
pub(crate) fn wants_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--help" || arg == "-h")
}
