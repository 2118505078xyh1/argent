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
    let names = crate::theme::names(&cwd);
    let current = crate::theme::current();
    for name in names {
        let marker = if name == current.name { "*" } else { " " };
        println!("{} {}", marker, name);
    }
    Ok(0)
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
    println!(
        "prompt {} · completion {} · cached {} ({}%) · missed {} · ${:.4}{}",
        short_tokens(summary.prompt_tokens),
        short_tokens(summary.completion_tokens),
        short_tokens(summary.cached_tokens),
        hit,
        short_tokens(summary.missed_tokens),
        summary.cost,
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
