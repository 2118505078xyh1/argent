//! Configuration loading and merging.
//!
//! Precedence (low to high): built-in defaults, global config file,
//! project config file, environment overrides, CLI flags.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::json::Json;

/// Default model provider endpoint.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:3425/v1";
pub const DEFAULT_MODEL: &str = "auto/deepseek-v4.1-flash";

/// Effective runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub api_key: Option<String>,
    /// Provider template id, e.g. `zai`, `anthropic`, `local`.
    pub provider: String,
    /// Wire protocol selected by the template.
    pub protocol: String,
    /// Environment variable the template reads for its key.
    pub api_key_env: Option<String>,
    pub model: String,
    pub max_output_tokens: usize,
    /// provider 模板声明的视觉能力；配置层不知道时默认 true。
    pub supports_vision: bool,
    pub thinking_level: String,
    pub max_context_tokens: usize,
    pub reserved_context_tokens: usize,
    pub keep_recent_tokens: usize,
    /// Status-bar segments in order; empty means the default set.
    pub statusline: Vec<String>,
    /// Compact automatically before a request would overflow the window.
    /// Turning it off leaves `/compact` (and the RPC `compact` method) as the
    /// only way to reclaim context.
    pub auto_compaction: bool,
    pub tool_timeout_secs: u64,
    pub bash_timeout_secs: u64,
    pub approval_mode: ApprovalMode,
    /// Models shown in the picker and allowed by model cycling. Empty means
    /// every model from the provider catalog (Pi's `enabledModels`).
    pub enabled_models: Vec<String>,
    /// Plugins to load at startup, by name; `all` loads every one.
    pub plugins: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub excluded_tools: Vec<String>,
    pub auto_approve_bash: bool,
    pub confirm_delete: bool,
    pub max_cost_usd: Option<f64>,
    /// Stop a session after this many billable tokens.
    pub max_total_tokens: Option<u64>,
    /// Stop a run after this many seconds of wall time.
    pub max_wall_seconds: Option<u64>,
    /// Prices used to turn token usage into a dollar estimate.
    pub price_input_per_mtok: Option<f64>,
    pub price_output_per_mtok: Option<f64>,
    pub project_trusted: bool,
    pub session_dir: PathBuf,
    /// Colour theme name for the interactive UI.
    pub theme: String,
    pub extra: BTreeMap<String, Json>,
}

/// How risky actions are gated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Every mutating tool call asks first.
    Always,
    /// Only destructive operations (delete, force push) ask.
    Sensible,
    /// Nothing asks.
    Never,
}

impl ApprovalMode {
    pub fn from_name(name: &str) -> Option<ApprovalMode> {
        match name.to_ascii_lowercase().as_str() {
            "always" => Some(ApprovalMode::Always),
            "sensible" | "smart" => Some(ApprovalMode::Sensible),
            "never" | "yolo" | "off" => Some(ApprovalMode::Never),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            ApprovalMode::Always => "always",
            ApprovalMode::Sensible => "sensible",
            ApprovalMode::Never => "never",
        }
    }
}

impl Default for Config {
    fn default() -> Config {
        Config {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: None,
            provider: "local".to_string(),
            protocol: "openai-chat-completions".to_string(),
            api_key_env: None,
            model: DEFAULT_MODEL.to_string(),
            max_output_tokens: 32_000,
            supports_vision: true,
            thinking_level: "auto".to_string(),
            max_context_tokens: 128_000,
            reserved_context_tokens: 16_000,
            keep_recent_tokens: 15_000,
            auto_compaction: true,
            statusline: Vec::new(),
            tool_timeout_secs: 300,
            bash_timeout_secs: 300,
            approval_mode: ApprovalMode::Sensible,
            enabled_models: Vec::new(),
            plugins: Vec::new(),
            allowed_tools: Vec::new(),
            excluded_tools: Vec::new(),
            auto_approve_bash: true,
            confirm_delete: true,
            max_cost_usd: None,
            max_total_tokens: None,
            max_wall_seconds: None,
            price_input_per_mtok: None,
            price_output_per_mtok: None,
            project_trusted: false,
            session_dir: default_session_dir(),
            theme: "dark".to_string(),
            extra: BTreeMap::new(),
        }
    }
}

fn default_session_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".argent").join("sessions")
}

impl Config {
    /// Load and merge configuration for a working directory.
    pub fn load(cwd: &Path) -> Result<Config> {
        let mut config = Config::default();
        let global_path = global_config_path();
        if global_path.is_file() {
            if let Some(text) = read_text(&global_path) {
                merge_into(&mut config, &text, &global_path)?;
            }
        }
        let project_path = cwd.join(".argent").join("settings.json");
        if project_path.is_file() {
            if let Some(text) = read_text(&project_path) {
                merge_into(&mut config, &text, &project_path)?;
            }
            // A project config file implies the project is trusted enough to
            // load its resources.
            config.project_trusted = true;
        }
        apply_env(&mut config);
        Ok(config)
    }

    /// Model endpoint URL for chat completions.
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Model endpoint URL for listing models.
    pub fn models_url(&self) -> String {
        format!("{}/models", self.base_url.trim_end_matches('/'))
    }

    pub fn auth_header(&self) -> Option<String> {
        self.api_key.as_ref().map(|key| format!("Bearer {}", key))
    }

    /// Whether a model id is in the configured scope. An empty scope admits
    /// all models. Patterns support `*` wildcards; matching is case-insensitive
    /// and also tries the bare id after `provider/`.
    pub fn model_in_scope(&self, id: &str) -> bool {
        if self.enabled_models.is_empty() {
            return true;
        }
        let bare = id.rsplit('/').next().unwrap_or(id);
        self.enabled_models.iter().any(|pattern| {
            let pattern = pattern.trim();
            if pattern.is_empty() {
                return false;
            }
            glob_matches(pattern, id) || glob_matches(pattern, bare)
        })
    }
}

/// Minimal case-insensitive glob matcher: only `*` is special.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let text = text.to_ascii_lowercase();
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut position = 0usize;
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            if !text.starts_with(part) {
                return false;
            }
            position = part.len();
        } else {
            match text[position..].find(part) {
                Some(found) => position += found + part.len(),
                None => return false,
            }
        }
    }
    pattern.ends_with('*') || position == text.len()
}

/// Path of the global config file.
/// Write one key into the global settings file, keeping the others.
///
/// A malformed existing file is replaced rather than merged: refusing to
/// remember a preference because the file is corrupt helps nobody.
pub fn set_global_setting(key: &str, value: Json) -> Result<()> {
    let path = global_config_path();
    let mut root = if path.is_file() {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| Json::parse(&text).ok())
            .filter(|value| matches!(value, Json::Obj(_)))
            .unwrap_or_else(Json::obj)
    } else {
        Json::obj()
    };
    root.set(key, value);
    crate::write_text_file(&path, &root.dump_pretty())
}

pub fn global_config_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".argent").join("settings.json")
}

fn read_text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// Merge one JSON config document into `config`.
pub fn merge_into(config: &mut Config, text: &str, source: &Path) -> Result<()> {
    let value = Json::parse(text)
        .map_err(|e| Error::config(format!("invalid JSON in {}: {}", source.display(), e)))?;
    let Json::Obj(entries) = value else {
        return Err(Error::config(format!(
            "{} must contain a JSON object",
            source.display()
        )));
    };

    for (key, val) in entries {
        match key.as_str() {
            "theme" => {
                if let Some(s) = val.as_str() {
                    config.theme = s.to_string();
                }
            }
            "baseUrl" | "base_url" => {
                if let Some(s) = val.as_str() {
                    config.base_url = s.to_string();
                }
            }
            "apiKey" | "api_key" => {
                config.api_key = val.as_str().map(str::to_string);
            }
            "provider" => {
                if let Some(s) = val.as_str() {
                    config.provider = s.to_string();
                }
            }
            "protocol" => {
                if let Some(s) = val.as_str() {
                    config.protocol = s.to_string();
                }
            }
            "apiKeyEnv" | "api_key_env" => {
                config.api_key_env = val.as_str().map(str::to_string);
            }
            "maxOutputTokens" | "max_output_tokens" => {
                if let Some(n) = val.as_i64() {
                    config.max_output_tokens = n.max(1) as usize;
                }
            }
            "model" => {
                if let Some(s) = val.as_str() {
                    config.model = s.to_string();
                }
            }
            "enabledModels" | "enabled_models" => {
                if let Some(items) = val.as_array() {
                    config.enabled_models = items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect();
                }
            }
            "thinkingLevel" | "thinking_level" => {
                if let Some(s) = val.as_str() {
                    config.thinking_level = s.to_string();
                }
            }
            "maxContextTokens" | "max_context_tokens" => {
                if let Some(n) = val.as_i64() {
                    config.max_context_tokens = n as usize;
                }
            }
            "reservedContextTokens" | "reserved_context_tokens" => {
                if let Some(n) = val.as_i64() {
                    config.reserved_context_tokens = n as usize;
                }
            }
            "keepRecentTokens" | "keep_recent_tokens" => {
                if let Some(n) = val.as_i64() {
                    config.keep_recent_tokens = n as usize;
                }
            }
            "autoCompaction" | "auto_compaction" => {
                config.auto_compaction = val.as_bool().unwrap_or(true);
            }
            "statusline" => {
                if let Some(items) = val.as_array() {
                    config.statusline = items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect();
                }
            }
            "toolTimeoutSecs" | "tool_timeout_secs" => {
                if let Some(n) = val.as_i64() {
                    config.tool_timeout_secs = n as u64;
                }
            }
            "bashTimeoutSecs" | "bash_timeout_secs" => {
                if let Some(n) = val.as_i64() {
                    config.bash_timeout_secs = n as u64;
                }
            }
            "approval" | "approvalMode" => {
                if let Some(s) = val.as_str() {
                    if let Some(mode) = ApprovalMode::from_name(s) {
                        config.approval_mode = mode;
                    }
                }
            }
            "tools" => {
                if let Json::Obj(tool_entries) = &val {
                    for (name, spec) in tool_entries {
                        match name.as_str() {
                            "include" => {
                                config.allowed_tools = string_list(spec);
                            }
                            "exclude" => {
                                config.excluded_tools = string_list(spec);
                            }
                            _ => {}
                        }
                    }
                }
            }
            "plugins" => {
                config.plugins = string_list(&val);
            }
            "autoApproveBash" | "auto_approve_bash" => {
                if let Some(b) = val.as_bool() {
                    config.auto_approve_bash = b;
                }
            }
            "confirmDelete" | "confirm_delete" => {
                if let Some(b) = val.as_bool() {
                    config.confirm_delete = b;
                }
            }
            "maxCostUsd" | "max_cost_usd" => {
                config.max_cost_usd = val.as_f64();
            }
            "maxTotalTokens" | "max_total_tokens" => {
                config.max_total_tokens = val
                    .as_i64()
                    .map(|n| n.max(0) as u64)
                    .or_else(|| val.as_f64().map(|n| n.max(0.0) as u64));
            }
            "maxWallSeconds" | "max_wall_seconds" => {
                config.max_wall_seconds = val
                    .as_i64()
                    .map(|n| n.max(0) as u64)
                    .or_else(|| val.as_f64().map(|n| n.max(0.0) as u64));
            }
            "priceInputPerMtok" | "price_input_per_mtok" => {
                config.price_input_per_mtok = val.as_f64();
            }
            "priceOutputPerMtok" | "price_output_per_mtok" => {
                config.price_output_per_mtok = val.as_f64();
            }
            "sessionDir" | "session_dir" => {
                if let Some(s) = val.as_str() {
                    config.session_dir = PathBuf::from(s);
                }
            }
            other => {
                config.extra.insert(other.to_string(), val);
            }
        }
    }
    Ok(())
}

fn string_list(value: &Json) -> Vec<String> {
    match value {
        Json::Arr(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Json::Str(s) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Apply environment overrides.
pub fn apply_env(config: &mut Config) {
    if let Ok(url) = std::env::var("ARGENT_BASE_URL") {
        config.base_url = url;
    }
    if let Ok(key) = std::env::var("ARGENT_API_KEY") {
        config.api_key = Some(key);
    }
    if let Ok(model) = std::env::var("ARGENT_MODEL") {
        config.model = model;
    }
    if let Ok(level) = std::env::var("ARGENT_THINKING") {
        config.thinking_level = level;
    }
    if let Ok(mode) = std::env::var("ARGENT_APPROVAL") {
        if let Some(parsed) = ApprovalMode::from_name(&mode) {
            config.approval_mode = parsed;
        }
    }
    if let Ok(dir) = std::env::var("ARGENT_SESSION_DIR") {
        config.session_dir = PathBuf::from(dir);
    }
    if let Ok(list) = std::env::var("ARGENT_PLUGINS") {
        config.plugins = list
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
    }
    if let Ok(value) = std::env::var("ARGENT_MAX_TOKENS") {
        if let Ok(tokens) = value.parse::<u64>() {
            config.max_total_tokens = Some(tokens);
        }
    }
    if let Ok(value) = std::env::var("ARGENT_MAX_COST") {
        if let Ok(cost) = value.parse::<f64>() {
            config.max_cost_usd = Some(cost);
        }
    }
    if let Ok(value) = std::env::var("ARGENT_MAX_SECONDS") {
        if let Ok(seconds) = value.parse::<u64>() {
            config.max_wall_seconds = Some(seconds);
        }
    }
    if let Ok(value) = std::env::var("ARGENT_PRICE_INPUT") {
        if let Ok(price) = value.parse::<f64>() {
            config.price_input_per_mtok = Some(price);
        }
    }
    if let Ok(value) = std::env::var("ARGENT_PRICE_OUTPUT") {
        if let Ok(price) = value.parse::<f64>() {
            config.price_output_per_mtok = Some(price);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn enabled_models_scope_filters_ids_and_glob_patterns() {
        let mut config = super::Config::default();
        assert!(config.model_in_scope("anything"), "empty scope admits all");
        config.enabled_models = vec!["gpt-*".into(), "anthropic/claude-sonnet-4".into()];
        assert!(config.model_in_scope("gpt-5"));
        assert!(config.model_in_scope("openai/GPT-4o"), "bare id matches, case-insensitive");
        assert!(config.model_in_scope("anthropic/claude-sonnet-4"));
        assert!(!config.model_in_scope("deepseek-v4"));
    }

    use super::*;

    #[test]
    fn merges_base_url_and_model() {
        let mut config = Config::default();
        merge_into(
            &mut config,
            r#"{"baseUrl":"http://x:1/v1","model":"m"}"#,
            Path::new("test.json"),
        )
        .unwrap();
        assert_eq!(config.base_url, "http://x:1/v1");
        assert_eq!(config.model, "m");
        assert_eq!(config.chat_url(), "http://x:1/v1/chat/completions");
    }

    #[test]
    fn rejects_non_object_config() {
        let mut config = Config::default();
        assert!(merge_into(&mut config, "[1,2]", Path::new("t.json")).is_err());
    }

    #[test]
    fn parses_approval_modes() {
        assert_eq!(ApprovalMode::from_name("yolo"), Some(ApprovalMode::Never));
        assert_eq!(ApprovalMode::from_name("nope"), None);
    }

    #[test]
    fn reads_the_plugin_list() {
        let mut config = Config::default();
        merge_into(
            &mut config,
            r#"{"plugins":["explore","all"]}"#,
            Path::new("t.json"),
        )
        .unwrap();
        assert_eq!(config.plugins, vec!["explore", "all"]);
        // A single name is accepted too, like the tool lists.
        merge_into(&mut config, r#"{"plugins":"plan"}"#, Path::new("t.json")).unwrap();
        assert_eq!(config.plugins, vec!["plan"]);
    }

    #[test]
    fn reads_tool_lists() {
        let mut config = Config::default();
        merge_into(
            &mut config,
            r#"{"tools":{"include":["read","bash"],"exclude":["write"]}}"#,
            Path::new("t.json"),
        )
        .unwrap();
        assert_eq!(config.allowed_tools, vec!["read", "bash"]);
        assert_eq!(config.excluded_tools, vec!["write"]);
    }
}
