//! `/bug`: collect a sanitized, local-only bug report.
//!
//! The report is a single JSON file under the argent storage directory. It
//! never leaves the machine: no upload, no network call. Sanitization is
//! conservative — the operator's home directory is replaced with `~`, and
//! credentials are never read.

use argent_core::config::Config;
use argent_core::error::Error;
use argent_core::json::Json;
use argent_core::path::expand_home;
use std::path::{Path, PathBuf};

/// Build the report and write it to storage; returns the file path.
pub fn write_report(config: &Config, cwd: &Path) -> Result<PathBuf, Error> {
    let report = collect(config, cwd);
    let stamp = argent_session::timestamp_slug();
    let dir = expand_home(Path::new("~/.argent/bug-reports"));
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::io(format!("create {} failed: {}", dir.display(), e)))?;
    let path = dir.join(format!("argent-bug-{}.json", stamp));
    std::fs::write(&path, report.dump_pretty())
        .map_err(|e| Error::io(format!("write {} failed: {}", path.display(), e)))?;
    Ok(path)
}

/// Assemble the sanitized report.
fn collect(config: &Config, cwd: &Path) -> Json {
    let mut report = Json::obj();
    report.set("version", Json::string(argent_core::VERSION));
    report.set("os", Json::string(std::env::consts::OS.to_string()));
    report.set("arch", Json::string(std::env::consts::ARCH.to_string()));
    report.set("cwd", Json::string(sanitize_path(cwd)));

    let mut provider = Json::obj();
    provider.set("id", Json::string(config.provider.clone()));
    provider.set("protocol", Json::string(config.protocol.clone()));
    provider.set("model", Json::string(config.model.clone()));
    // The base URL can carry a private host (a LAN proxy); keep only the
    // scheme and host, drop any path or query.
    provider.set("baseUrl", Json::string(sanitize_url(&config.base_url)));
    provider.set(
        "apiKeySet",
        Json::bool(
            config
                .api_key
                .as_deref()
                .is_some_and(|k| !k.trim().is_empty()),
        ),
    );
    report.set("provider", provider);

    let mut settings = Json::obj();
    settings.set(
        "approvalMode",
        Json::string(config.approval_mode.name().to_string()),
    );
    settings.set("autoCompaction", Json::bool(config.auto_compaction));
    settings.set(
        "backgroundIntelligence",
        Json::bool(config.background_intelligence),
    );
    if let Some(model) = &config.background_model {
        settings.set("backgroundModel", Json::string(model.clone()));
    }
    // followUpMode / cacheWarming live in `extra` (they configure the agent
    // after the config parse), so report them from there when present.
    for key in ["followUpMode", "cacheWarming"] {
        if let Some(value) = config.extra.get(key) {
            settings.set(key, value.clone());
        }
    }
    settings.set(
        "maxContextTokens",
        Json::num(config.max_context_tokens as f64),
    );
    settings.set(
        "toolTimeoutSecs",
        Json::num(config.tool_timeout_secs as f64),
    );
    report.set("settings", settings);

    report
}

/// Replace the home directory prefix with `~` so the report does not carry
/// the operator's account name.
fn sanitize_path(path: &Path) -> String {
    let raw = path.to_string_lossy().into_owned();
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    {
        let home = home.to_string_lossy().into_owned();
        if !home.is_empty() && raw.starts_with(&home) {
            return format!("~{}", &raw[home.len()..]);
        }
    }
    raw
}

/// Keep `scheme://host[:port]`, drop everything after the authority.
fn sanitize_url(url: &str) -> String {
    if let Some(rest) = url.split_once("://") {
        let (scheme, authority_and_rest) = rest;
        let authority = authority_and_rest
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("");
        format!("{}://{}", scheme, authority)
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_url_keeps_only_the_authority() {
        assert_eq!(
            sanitize_url("https://api.example.com/v1/chat?key=secret"),
            "https://api.example.com"
        );
        assert_eq!(
            sanitize_url("http://127.0.0.1:3425/v1"),
            "http://127.0.0.1:3425"
        );
    }

    #[test]
    fn collect_never_carries_a_key() {
        let report = collect(
            &Config {
                api_key: Some("sk-real-secret".to_string()),
                ..Config::default()
            },
            Path::new("."),
        );
        let dumped = report.dump();
        assert!(!dumped.contains("sk-real-secret"));
        assert!(dumped.contains("apiKeySet"));
    }
}
