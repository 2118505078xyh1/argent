//! `usage`, `audit`, `policy`, `profiles`, `telemetry`, `bug`, `themes`, `keybindings`.

use super::*;

/// `argent keybindings`：列出当前的手势绑定。
pub(super) fn keybindings(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let bindings = crate::keybindings::Bindings::load(&cwd)?;
    for (action, keys) in bindings.list() {
        let names: Vec<String> = keys.iter().map(crate::keybindings::key_name).collect();
        println!("{:<18} {}", action.name(), names.join(", "));
    }
    Ok(0)
}

/// `argent themes`：列出可用主题与当前选择。
pub(super) fn themes(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let positional: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    let action = positional.first().copied().unwrap_or("list");
    let paths = Paths::new(&cwd);
    let user_dir = paths.storage().join("themes");
    let project_dir = cwd.join(".argent").join("themes");
    let locate = |name: &str| -> Option<PathBuf> {
        [
            project_dir.join(format!("{name}.json")),
            user_dir.join(format!("{name}.json")),
        ]
        .into_iter()
        .find(|path| path.is_file())
    };

    match action {
        "list" | "ls" | "themes" => {
            let current = crate::theme::current();
            for name in crate::theme::names(&cwd) {
                let marker = if name == current.name { "*" } else { " " };
                println!("{} {}", marker, name);
            }
            Ok(0)
        }
        "validate" | "check" => {
            let Some(name) = positional.get(1) else {
                return Err(Error::config("usage: argent themes validate <name>"));
            };
            let Some(path) = locate(name) else {
                return Err(Error::config(format!("theme '{}' has no JSON file", name)));
            };
            let text = std::fs::read_to_string(&path)
                .map_err(|error| Error::io(format!("read {} failed: {}", path.display(), error)))?;
            let value = Json::parse(&text)
                .map_err(|error| Error::config(format!("{}: {}", path.display(), error)))?;
            let known = [
                "background",
                "accent",
                "secondary",
                "text",
                "dim",
                "success",
                "warning",
                "error",
                "user_fg",
                "user_bg",
                "code_fg",
                "code_bg",
            ];
            let mut warnings = 0;
            for (key, _) in value.as_object().unwrap_or(&[]) {
                if !known.contains(&key.as_str()) {
                    eprintln!("warning: unknown field '{}' (ignored)", key);
                    warnings += 1;
                }
            }
            let base = crate::theme::Theme::builtin("dark").expect("dark");
            base.overlaid(&value)?;
            println!(
                "ok: {} ({} known field(s), {} warning(s))",
                path.display(),
                known.len(),
                warnings
            );
            Ok(0)
        }
        "export" => {
            let Some(name) = positional.get(1) else {
                return Err(Error::config(
                    "usage: argent themes export <name> [--output FILE]",
                ));
            };
            let theme = crate::theme::load(name, &cwd);
            let mut data = Json::obj();
            for (key, color) in [
                ("background", theme.background),
                ("accent", theme.accent),
                ("secondary", theme.secondary),
                ("text", theme.text),
                ("dim", theme.dim),
                ("success", theme.success),
                ("warning", theme.warning),
                ("error", theme.error),
                ("user_fg", theme.user_fg),
                ("user_bg", theme.user_bg),
                ("code_fg", theme.code_fg),
                ("code_bg", theme.code_bg),
            ] {
                data.set(key, Json::string(color_text(color)));
            }
            let text = data.dump_pretty();
            match flag_value(args, "--output") {
                Some(path) => {
                    std::fs::write(
                        path,
                        format!(
                            "{}
",
                            text
                        ),
                    )
                    .map_err(|error| Error::io(format!("write {} failed: {}", path, error)))?;
                    println!("exported {} to {}", theme.name, path);
                }
                None => println!("{}", text),
            }
            Ok(0)
        }
        "import" => {
            let Some(source) = positional.get(1) else {
                return Err(Error::config("usage: argent themes import <file.json>"));
            };
            let path = Path::new(source);
            let text = std::fs::read_to_string(path)
                .map_err(|error| Error::io(format!("read {} failed: {}", path.display(), error)))?;
            let value = Json::parse(&text)
                .map_err(|error| Error::config(format!("{}: {}", path.display(), error)))?;
            crate::theme::Theme::builtin("dark")
                .expect("dark")
                .overlaid(&value)?;
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                return Err(Error::config("theme file needs a name"));
            };
            std::fs::create_dir_all(&user_dir).map_err(|error| {
                Error::io(format!("create {} failed: {}", user_dir.display(), error))
            })?;
            let destination = user_dir.join(format!("{}.json", stem));
            if destination.exists() && !args.iter().any(|arg| arg == "--force") {
                return Err(Error::config(format!(
                    "{} already exists; pass --force to overwrite",
                    destination.display()
                )));
            }
            std::fs::write(&destination, text).map_err(|error| {
                Error::io(format!("write {} failed: {}", destination.display(), error))
            })?;
            println!("imported {} as '{}'", source, stem);
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown themes command '{}'; expected list, validate, export or import",
            other
        ))),
    }
}

fn array_mut<'a>(value: &'a mut Json, key: &str) -> &'a mut Vec<Json> {
    let Some(slot) = value.get_mut(key) else {
        panic!("board is missing its {key} array");
    };
    if !matches!(slot, Json::Arr(_)) {
        *slot = Json::arr();
    }
    match slot {
        Json::Arr(items) => items,
        _ => unreachable!(),
    }
}

/// A file-backed task board shared by agents working in the same workspace.
///
/// This is intentionally small: a JSON file under `.argent/` is visible to
/// every agent (and to the operator), needs no service, and survives restarts.
/// It gives cooperating agents a shared todo list plus a message inbox.
pub(super) fn board(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let path = cwd.join(".argent/board.json");
    // Skip flag values as well as flags: `--workspace DIR` must not become
    // part of a task title or a message body.
    let mut positional: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if ["--workspace", "--cwd", "-C", "--as"].contains(&arg) {
            index += 2;
        } else if arg.starts_with('-') {
            index += 1;
        } else {
            positional.push(arg);
            index += 1;
        }
    }
    let action = positional.first().copied().unwrap_or("list");
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_secs())
            .unwrap_or(0)
    };
    let blank = || {
        let mut doc = Json::obj();
        doc.set("tasks", Json::arr());
        doc.set("messages", Json::arr());
        doc
    };
    let mut doc = if path.is_file() {
        let text = std::fs::read_to_string(&path)
            .map_err(|error| Error::io(format!("read {} failed: {}", path.display(), error)))?;
        Json::parse(&text).unwrap_or_else(|_| blank())
    } else {
        blank()
    };
    if action != "list" && action != "inbox" {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::io(format!("create {} failed: {}", parent.display(), error))
            })?;
        }
    }
    let save = |doc: &Json| -> Result<()> {
        std::fs::write(
            &path,
            format!(
                "{}
",
                doc.dump_pretty()
            ),
        )
        .map_err(|error| Error::io(format!("write {} failed: {}", path.display(), error)))
    };

    match action {
        "init" => {
            if !path.exists() {
                save(&doc)?;
                println!("created {}", path.display());
            } else {
                println!("board already exists at {}", path.display());
            }
            Ok(0)
        }
        "add" => {
            let title = positional[1..].join(" ").trim().to_string();
            if title.is_empty() {
                return Err(Error::config("usage: argent board add <title>"));
            }
            let tasks = array_mut(&mut doc, "tasks");
            let next = tasks
                .iter()
                .filter_map(|task| task.get("id").and_then(Json::as_i64))
                .max()
                .unwrap_or(0)
                + 1;
            let mut task = Json::obj();
            task.set("id", Json::num(next as f64));
            task.set("title", Json::string(title.clone()));
            task.set("owner", Json::string(String::new()));
            task.set("status", Json::string("todo"));
            task.set("created", Json::num(now() as f64));
            tasks.push(task);
            save(&doc)?;
            println!("#{} {}", next, title);
            Ok(0)
        }
        "list" => {
            let tasks = doc.get("tasks").and_then(Json::as_array).unwrap_or(&[]);
            if tasks.is_empty() {
                println!("no tasks; add one with `argent board add <title>`");
            }
            for task in tasks {
                println!(
                    "#{:<3} {:<9} {:<12} {}",
                    task.get("id").and_then(Json::as_i64).unwrap_or(0),
                    task.str_or("status", "todo"),
                    task.str_or("owner", "-"),
                    task.str_or("title", ""),
                );
            }
            Ok(0)
        }
        "claim" => {
            let Some(id) = positional
                .get(1)
                .and_then(|value| value.parse::<i64>().ok())
            else {
                return Err(Error::config("usage: argent board claim <id> [--as AGENT]"));
            };
            let agent = flag_value(args, "--as").unwrap_or("agent").to_string();
            let tasks = array_mut(&mut doc, "tasks");
            let task = tasks
                .iter_mut()
                .find(|task| task.get("id").and_then(Json::as_i64) == Some(id))
                .ok_or_else(|| Error::config(format!("task #{} not found", id)))?;
            task.set("owner", Json::string(agent.clone()));
            task.set("status", Json::string("doing"));
            save(&doc)?;
            println!("#{} claimed by {}", id, agent);
            Ok(0)
        }
        "done" => {
            let Some(id) = positional
                .get(1)
                .and_then(|value| value.parse::<i64>().ok())
            else {
                return Err(Error::config("usage: argent board done <id>"));
            };
            let tasks = array_mut(&mut doc, "tasks");
            let task = tasks
                .iter_mut()
                .find(|task| task.get("id").and_then(Json::as_i64) == Some(id))
                .ok_or_else(|| Error::config(format!("task #{} not found", id)))?;
            task.set("status", Json::string("done"));
            save(&doc)?;
            println!("#{} done", id);
            Ok(0)
        }
        "message" => {
            if positional.len() < 4 {
                return Err(Error::config(
                    "usage: argent board message <from> <to> <text...>",
                ));
            }
            let from = positional[1];
            let to = positional[2];
            let text = positional[3..].join(" ");
            let mut message = Json::obj();
            message.set("from", Json::string(from.to_string()));
            message.set("to", Json::string(to.to_string()));
            message.set("text", Json::string(text));
            message.set("epoch", Json::num(now() as f64));
            array_mut(&mut doc, "messages").push(message);
            save(&doc)?;
            println!("message queued for {}", to);
            Ok(0)
        }
        "inbox" => {
            let Some(agent) = positional.get(1) else {
                return Err(Error::config("usage: argent board inbox <agent>"));
            };
            let messages = doc.get("messages").and_then(Json::as_array).unwrap_or(&[]);
            for message in messages
                .iter()
                .filter(|message| message.str_or("to", "") == *agent)
            {
                println!(
                    "{} ← {}: {}",
                    argent_session::local_stamp(message.num_or("epoch", 0.0) as u64),
                    message.str_or("from", "?"),
                    message.str_or("text", "")
                );
            }
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown board command '{}'; expected init, add, list, claim, done, message or inbox",
            other
        ))),
    }
}

fn color_text(color: argent_core::screen::Color) -> String {
    use argent_core::screen::Color;
    match color {
        Color::Default => "default".to_string(),
        Color::Red => "red".to_string(),
        Color::Green => "green".to_string(),
        Color::Yellow => "yellow".to_string(),
        Color::Blue => "blue".to_string(),
        Color::Magenta => "magenta".to_string(),
        Color::Cyan => "cyan".to_string(),
        Color::Gray => "gray".to_string(),
        Color::White => "white".to_string(),
        Color::Fixed(index) => index.to_string(),
        Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),
    }
}

// ------------------------------------------------------------------ usage

/// Token and spend totals from the usage ledger.
pub(super) fn usage(args: &[String]) -> Result<i32> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let days = flag_value(args, "--days")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(7);
    let since = if args.iter().any(|arg| arg == "--all") {
        0
    } else {
        now.saturating_sub(days.saturating_mul(86_400))
    };
    let session_filter = flag_value(args, "--session").map(|value| value.to_ascii_lowercase());
    let model_filter = flag_value(args, "--model").map(|value| value.to_ascii_lowercase());
    let records: Vec<argent_session::usage::UsageRecord> = argent_session::usage::read()
        .into_iter()
        .filter(|record| record.epoch >= since)
        .filter(|record| {
            session_filter
                .as_deref()
                .map(|needle| record.session.to_ascii_lowercase().contains(needle))
                .unwrap_or(true)
        })
        .filter(|record| {
            model_filter
                .as_deref()
                .map(|needle| record.model.to_ascii_lowercase().contains(needle))
                .unwrap_or(true)
        })
        .collect();
    let summary = argent_session::usage::summary(&records, since);
    if args.iter().any(|arg| arg == "--json") {
        let mut data = Json::obj();
        data.set("turns", Json::num(summary.turns as f64));
        data.set("prompt", Json::num(summary.prompt_tokens as f64));
        data.set("completion", Json::num(summary.completion_tokens as f64));
        data.set("cached", Json::num(summary.cached_tokens as f64));
        data.set("missed", Json::num(summary.missed_tokens as f64));
        data.set(
            "cache_hit_percent",
            Json::num(summary.cache_hit_percent() as f64),
        );
        data.set("requests", Json::num(summary.requests as f64));
        if summary.warm_requests > 0 {
            data.set("warm_requests", Json::num(summary.warm_requests as f64));
        }
        data.set("ttft_samples", Json::num(summary.ttft_samples as f64));
        if let Some(ttft) = summary.avg_ttft_ms() {
            data.set("avg_ttft_ms", Json::num(ttft as f64));
        }
        data.set("cost", Json::num(summary.cost));
        let mut models = Json::obj();
        for (name, item) in &summary.by_model {
            let mut entry = Json::obj();
            entry.set("turns", Json::num(item.turns as f64));
            entry.set("prompt", Json::num(item.prompt_tokens as f64));
            entry.set("completion", Json::num(item.completion_tokens as f64));
            entry.set("cached", Json::num(item.cached_tokens as f64));
            entry.set("missed", Json::num(item.missed_tokens as f64));
            entry.set("cost", Json::num(item.cost));
            models.set(name, entry);
        }
        data.set("models", models);
        println!("{}", data.dump_pretty());
        return Ok(0);
    }
    if summary.turns == 0 {
        println!(
            "no usage records in {} in the last {} day(s)",
            argent_session::usage::usage_path().display(),
            days
        );
        return Ok(0);
    }
    let since_label = if since == 0 {
        "all time".to_string()
    } else {
        format!("since {}", argent_session::local_stamp(since))
    };
    let hit = summary.cache_hit_percent();
    println!(
        "usage: {} turn(s), {} request(s), {}",
        summary.turns, summary.requests, since_label
    );
    let ttft = summary
        .avg_ttft_ms()
        .map(|ms| format!(" · ttft {:.1}s avg", ms as f64 / 1000.0))
        .unwrap_or_default();
    let warm = if summary.warm_requests > 0 {
        format!(" · {} warm", summary.warm_requests)
    } else {
        String::new()
    };
    println!(
        "prompt {} · completion {} · cached {} ({}%) · missed {} · ${:.4}{}{}",
        short_tokens(summary.prompt_tokens),
        short_tokens(summary.completion_tokens),
        short_tokens(summary.cached_tokens),
        hit,
        short_tokens(summary.missed_tokens),
        summary.cost,
        warm,
        ttft
    );
    if !summary.by_model.is_empty() {
        println!("by model:");
        let mut models: Vec<_> = summary.by_model.iter().collect();
        models.sort_by(|a, b| {
            b.1.cost
                .partial_cmp(&a.1.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (name, item) in models.iter().take(10) {
            let model_hit = item.cache_hit_percent();
            println!(
                "  {:<36} {:>4} turns  {:>8} in  {:>8} out  cache {:>3}%  missed {:>8}  ${:.4}",
                clip(name, 36),
                item.turns,
                short_tokens(item.prompt_tokens),
                short_tokens(item.completion_tokens),
                model_hit,
                short_tokens(item.missed_tokens),
                item.cost
            );
        }
    }
    if !summary.by_session.is_empty() {
        println!("by session:");
        let mut sessions: Vec<_> = summary.by_session.iter().collect();
        sessions.sort_by(|a, b| {
            b.1.cost
                .partial_cmp(&a.1.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (path, item) in sessions.iter().take(10) {
            println!(
                "  {:<58} {:>4} turns  ${:.4}",
                clip(path, 58),
                item.turns,
                item.cost
            );
        }
    }
    Ok(0)
}

// ------------------------------------------------------------------ audit

/// Show the audit trail: every tool call, its decision and its arguments.
///
/// The arguments are already redacted on the way in, so this is safe to paste
/// into a bug report.
pub(super) fn audit(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let paths = Paths::new(&cwd);
    let log = argent_security::AuditLog::new(paths.storage());
    let entries = argent_security::AuditLog::read(log.path());
    if entries.is_empty() {
        println!("no audit entries in {}", log.path().display());
        return Ok(0);
    }
    let limit = args
        .iter()
        .position(|arg| arg == "--limit")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(50);
    let start = entries.len().saturating_sub(limit);
    for entry in &entries[start..] {
        let tool = entry.str_or("tool", "?");
        let decision = entry.str_or("decision", "?");
        let stamp = entry.str_or("timestamp", "?");
        let arguments = entry.str_or("arguments", "");
        let reason = entry.str_or("reason", "");
        if reason.is_empty() {
            println!("{}  {:<12} {:<8} {}", stamp, tool, decision, arguments);
        } else {
            println!(
                "{}  {:<12} {:<8} {}  ({})",
                stamp, tool, decision, arguments, reason
            );
        }
    }
    Ok(0)
}

// ----------------------------------------------------------------- policy

/// List the profiles visible from a workspace.
pub(super) fn profiles(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let settings = Settings::load(&cwd)?;
    let registry = argent_profiles::ProfileRegistry::load(&cwd, &settings)?;
    if registry.is_empty() {
        println!(
            "no profiles; add .argent/profiles/<name>.md or a 'profiles' block in argent.json"
        );
        return Ok(0);
    }
    for profile in registry.list() {
        let model = profile.model.as_deref().unwrap_or("-");
        let source = profile
            .source
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "settings".to_string());
        println!(
            "{:<16} model={:<28} {}\n  {}",
            profile.name, model, source, profile.description
        );
    }
    Ok(0)
}

/// Show the effective permission policy.
pub(super) fn policy(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let settings = Settings::load(&cwd)?;
    let policy = argent_security::Policy::from_settings(&settings);
    if policy.is_empty() {
        println!("no permission rules; every tool is allowed by policy");
        return Ok(0);
    }
    println!("deny paths:");
    for rule in &policy.deny_paths {
        println!("  {rule}");
    }
    if !policy.allow_paths.is_empty() {
        println!("allow paths (everything else is denied):");
        for rule in &policy.allow_paths {
            println!("  {rule}");
        }
    }
    println!("deny commands:");
    for rule in &policy.deny_commands {
        println!("  {rule}");
    }
    println!("deny command paths:");
    for rule in &policy.deny_command_paths {
        println!("  {rule}");
    }
    println!("deny tools:");
    for rule in &policy.deny_tools {
        println!("  {rule}");
    }
    if !policy.rules.is_empty() {
        println!("ordered rules:");
        for rule in &policy.rules {
            let decision = match rule.decision {
                argent_security::Decision::Allow => "allow",
                argent_security::Decision::Ask => "ask",
                argent_security::Decision::Deny => "deny",
            };
            let tool = rule.tool.as_deref().unwrap_or("*");
            let mut line = format!("  {decision} tool={tool}");
            if let Some(path) = &rule.path {
                line.push_str(&format!(" path={path}"));
            }
            if let Some(command) = &rule.command {
                line.push_str(&format!(" command={command}"));
            }
            println!("{line}");
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------- telemetry

pub(super) fn telemetry(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let settings = Settings::load(&cwd)?;
    let writer = argent_telemetry::Telemetry::from_settings(&settings, &cwd);
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("status");
    match action {
        "status" => {
            println!(
                "telemetry: {}",
                if writer.enabled() {
                    "enabled (local)"
                } else {
                    "disabled"
                }
            );
            println!("events: {}", writer.events_path().display());
            match writer.endpoint() {
                Some(endpoint) => println!("endpoint: {}", endpoint),
                None => println!("endpoint: (not configured)"),
            }
            println!(
                "remote upload: {}",
                if !writer.enabled() {
                    "off (telemetry is disabled)"
                } else if writer.remote() {
                    "enabled"
                } else if writer.endpoint().is_some() {
                    "off (needs consent: `argent telemetry enable --consent`)"
                } else {
                    "off (needs an endpoint and consent)"
                }
            );
            Ok(0)
        }
        "enable" | "on" => {
            let endpoint = flag_value(args, "--endpoint").map(str::to_string);
            let consent = args.iter().any(|arg| arg == "--consent");
            // 只开本地时不写对象，保持和老配置一样的形状。
            if endpoint.is_none() && !consent {
                settings.set_user("telemetry", Json::bool(true))?;
            } else {
                let mut value = Json::obj();
                value.set("enabled", Json::bool(true));
                if let Some(endpoint) = &endpoint {
                    value.set("endpoint", Json::string(endpoint.clone()));
                }
                value.set("consent", Json::bool(consent));
                settings.set_user("telemetry", value)?;
            }
            println!(
                "telemetry enabled; events stay in {}",
                writer.events_path().display()
            );
            if let Some(endpoint) = &endpoint {
                println!("events will also be posted to {}", endpoint);
            }
            if !consent {
                println!("remote upload stays off until `argent telemetry enable --consent`");
            }
            Ok(0)
        }
        "disable" | "off" => {
            // 保留已配置的端点与同意状态，重新打开时不用再配一遍。
            let mut value = Json::obj();
            value.set("enabled", Json::bool(false));
            if let Some(existing) = settings.get("telemetry") {
                if let Some(endpoint) = existing.get("endpoint").and_then(Json::as_str) {
                    value.set("endpoint", Json::string(endpoint.to_string()));
                }
                if let Some(consent) = existing.get("consent").and_then(Json::as_bool) {
                    value.set("consent", Json::bool(consent));
                }
            }
            settings.set_user("telemetry", value)?;
            println!("telemetry disabled");
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown telemetry command '{}'; expected status, enable, disable",
            other
        ))),
    }
}

// ---------------------------------------------------------------- helpers

/// `argent bug`: sanitized local report. No network.
pub(super) fn bug_report(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let mut config = argent_core::config::Config::load(&cwd)?;
    crate::mainx::settings::apply_settings(&mut config, &crate::args::Cli::default(), None)?;
    let path = crate::mainx::bug_report::write_report(&config, &cwd)?;
    println!("bug report written to {}", path.display());
    println!("attach it manually if you file an issue; it never leaves your machine");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_shares_tasks_and_messages_between_agents() {
        let dir = std::env::temp_dir().join(format!("argent-board-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let workspace = || ["--workspace".to_string(), dir.display().to_string()];
        let mut init = workspace().to_vec();
        init.insert(0, "init".to_string());
        assert_eq!(board(&init).expect("init"), 0);

        let mut add = workspace().to_vec();
        add.insert(0, "add".to_string());
        add.insert(1, "fix the flaky test".to_string());
        assert_eq!(board(&add).expect("add"), 0);

        let mut claim = workspace().to_vec();
        claim.insert(0, "claim".to_string());
        claim.insert(1, "1".to_string());
        claim.extend(["--as".to_string(), "alice".to_string()]);
        assert_eq!(board(&claim).expect("claim"), 0);

        let mut message = workspace().to_vec();
        message.insert(0, "message".to_string());
        message.extend([
            "alice".to_string(),
            "bob".to_string(),
            "need review".to_string(),
        ]);
        assert_eq!(board(&message).expect("message"), 0);

        let mut inbox = workspace().to_vec();
        inbox.insert(0, "inbox".to_string());
        inbox.insert(1, "bob".to_string());
        assert_eq!(board(&inbox).expect("inbox"), 0);

        let text = std::fs::read_to_string(dir.join(".argent/board.json")).expect("board file");
        assert!(text.contains("fix the flaky test"), "{text}");
        assert!(text.contains("alice"), "{text}");
        assert!(text.contains("need review"), "{text}");
        let doc = Json::parse(&text).expect("board json");
        let task = &doc.get("tasks").and_then(Json::as_array).unwrap()[0];
        assert_eq!(task.str_or("status", ""), "doing");
        assert_eq!(task.str_or("owner", ""), "alice");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn themes_validate_export_and_import_round_trip() {
        let dir = std::env::temp_dir().join(format!("argent-theme-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let themes_dir = dir.join(".argent/themes");
        std::fs::create_dir_all(&themes_dir).expect("themes dir");
        std::fs::write(
            themes_dir.join("custom.json"),
            r##"{"accent":"#ff0000","text":"white"}"##,
        )
        .expect("write theme");
        let args = |action: &str, extra: &[&str]| {
            let mut out = vec![
                action.to_string(),
                "custom".to_string(),
                "--workspace".to_string(),
                dir.display().to_string(),
            ];
            out.extend(extra.iter().map(|value| value.to_string()));
            out
        };
        assert_eq!(themes(&args("validate", &[])).expect("valid theme"), 0);

        let exported = dir.join("exported.json");
        assert_eq!(
            themes(&args("export", &["--output", exported.to_str().unwrap()])).expect("export"),
            0
        );
        let text = std::fs::read_to_string(&exported).expect("exported file");
        assert!(text.contains("\"accent\""), "{text}");

        let imported = dir.join("imported.json");
        std::fs::write(&imported, text).expect("write import");
        let code = themes(&[
            "import".to_string(),
            imported.display().to_string(),
            "--workspace".to_string(),
            dir.display().to_string(),
            "--force".to_string(),
        ])
        .expect("import");
        assert_eq!(code, 0);

        std::fs::write(
            themes_dir.join("custom.json"),
            r#"{"accent":"not-a-color"}"#,
        )
        .expect("write bad theme");
        assert!(themes(&args("validate", &[])).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
