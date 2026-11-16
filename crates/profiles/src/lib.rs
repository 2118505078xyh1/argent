//! Named profiles: a reusable bundle of prompt, model, tool policy and budget.
//!
//! A profile is either a markdown file with a small frontmatter block:
//!
//! ```text
//! ---
//! name: reviewer
//! description: Read a diff and report defects
//! model: GLM-5.3
//! thinking: high
//! tools: read, grep, find, ls
//! readOnly: true
//! maxCostUsd: 0.5
//! ---
//! You are a careful code reviewer.
//! ```
//!
//! or an entry under `profiles` in `argent.json`. Files under
//! `.argent/profiles/*.md` and `~/.argent/profiles/*.md` are discovered automatically;
//! inline settings win over files, project files win over user files.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use argent_config::Settings;
use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// One named profile.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    pub name: String,
    pub description: String,
    /// Extra system-prompt text appended after the project context.
    pub prompt: String,
    pub model: Option<String>,
    pub thinking: Option<String>,
    /// Provider template id. When set, the model is resolved on that provider.
    pub provider: Option<String>,
    /// `always` / `sensible` / `never`.
    pub approval: Option<String>,
    /// When set, only these tools survive; `tool_search` can still reach them.
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    /// Replace the configured plugin list when set.
    pub plugins: Option<Vec<String>>,
    /// Drop every mutating tool.
    pub read_only: bool,
    pub max_turns: Option<usize>,
    pub max_total_tokens: Option<u64>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_seconds: Option<u64>,
    pub price_input_per_mtok: Option<f64>,
    pub price_output_per_mtok: Option<f64>,
    /// Markdown files appended to the prompt, resolved next to the profile.
    pub prompt_files: Vec<String>,
    /// Where the profile was defined, for `argent profiles`.
    pub source: Option<PathBuf>,
}

impl Profile {
    /// Parse a markdown profile. The body is the prompt.
    pub fn parse_markdown(
        text: &str,
        fallback_name: &str,
        source: Option<PathBuf>,
    ) -> Result<Profile> {
        let text = text.trim_start_matches('\u{feff}');
        let (frontmatter, body) = split_frontmatter(text)
            .ok_or_else(|| Error::config("a profile must start with a '---' frontmatter block"))?;
        let fields = parse_fields(frontmatter);
        let mut profile = Profile {
            name: fallback_name.to_string(),
            prompt: body.trim().to_string(),
            source,
            ..Profile::default()
        };
        for (key, value) in fields {
            profile.apply_field(&key, &value)?;
        }
        if profile.name.is_empty() {
            profile.name = fallback_name.to_string();
        }
        Ok(profile)
    }

    /// Parse an inline JSON profile.
    pub fn from_json(value: &Json, fallback_name: &str) -> Result<Profile> {
        let mut profile = Profile {
            name: value.str_or("name", fallback_name),
            description: value.str_or("description", ""),
            prompt: value.str_or("prompt", ""),
            source: None,
            ..Profile::default()
        };
        if let Some(model) = value.get("model").and_then(Json::as_str) {
            profile.model = Some(model.to_string());
        }
        if let Some(provider) = value.get("provider").and_then(Json::as_str) {
            profile.provider = Some(provider.to_string());
        }
        if let Some(thinking) = value.get("thinking").and_then(Json::as_str) {
            profile.thinking = Some(thinking.to_string());
        }
        if let Some(approval) = value.get("approval").and_then(Json::as_str) {
            profile.approval = Some(approval.to_string());
        }
        if let Some(tools) = value.get("tools") {
            profile.tools = Some(string_list(tools));
        }
        if let Some(tools) = value.get("excludeTools") {
            profile.exclude_tools = string_list(tools);
        }
        if let Some(plugins) = value.get("plugins") {
            profile.plugins = Some(string_list(plugins));
        }
        profile.read_only = value.bool_or("readOnly", false) || value.bool_or("read_only", false);
        profile.max_turns = value
            .get("maxTurns")
            .or_else(|| value.get("max_turns"))
            .and_then(Json::as_i64)
            .map(|n| n.max(1) as usize);
        profile.max_total_tokens = value
            .get("maxTotalTokens")
            .or_else(|| value.get("max_total_tokens"))
            .and_then(Json::as_i64)
            .map(|n| n.max(0) as u64);
        profile.max_cost_usd = value
            .get("maxCostUsd")
            .or_else(|| value.get("max_cost_usd"))
            .and_then(Json::as_f64);
        profile.max_wall_seconds = value
            .get("maxWallSeconds")
            .or_else(|| value.get("max_wall_seconds"))
            .and_then(Json::as_i64)
            .map(|n| n.max(0) as u64);
        profile.price_input_per_mtok = value
            .get("priceInputPerMtok")
            .or_else(|| value.get("price_input_per_mtok"))
            .and_then(Json::as_f64);
        profile.price_output_per_mtok = value
            .get("priceOutputPerMtok")
            .or_else(|| value.get("price_output_per_mtok"))
            .and_then(Json::as_f64);
        if let Some(files) = value
            .get("promptFiles")
            .or_else(|| value.get("prompt_files"))
        {
            profile.prompt_files = string_list(files);
        }
        Ok(profile)
    }

    fn apply_field(&mut self, key: &str, value: &str) -> Result<()> {
        let normalized = key.trim().to_ascii_lowercase().replace(['-', '_'], "");
        let value = value.trim();
        match normalized.as_str() {
            "name" => self.name = value.to_string(),
            "description" => self.description = value.to_string(),
            "model" => self.model = Some(value.to_string()),
            "provider" => self.provider = Some(value.to_string()),
            "thinking" => self.thinking = Some(value.to_string()),
            "approval" => self.approval = Some(value.to_string()),
            "tools" => self.tools = Some(split_list(value)),
            "excludetools" => self.exclude_tools = split_list(value),
            "plugins" => self.plugins = Some(split_list(value)),
            "readonly" => self.read_only = truthy(value),
            "maxturns" => self.max_turns = value.parse().ok().map(|n: usize| n.max(1)),
            "maxtotaltokens" => self.max_total_tokens = value.parse().ok(),
            "maxcostusd" => self.max_cost_usd = value.parse().ok(),
            "maxwallseconds" => self.max_wall_seconds = value.parse().ok(),
            "priceinputpermtok" => self.price_input_per_mtok = value.parse().ok(),
            "priceoutputpermtok" => self.price_output_per_mtok = value.parse().ok(),
            "promptfiles" => self.prompt_files = split_list(value),
            _ => {
                return Err(Error::config(format!(
                    "unknown profile field '{}' (in {})",
                    key, self.name
                )))
            }
        }
        Ok(())
    }

    /// Append the profile prompt and its prompt files to a system-prompt note.
    pub fn prompt_note(&self, cwd: &Path) -> String {
        let mut out = String::new();
        if !self.prompt.trim().is_empty() {
            out.push_str(&format!("## Active profile: {}\n", self.name));
            if !self.description.is_empty() {
                out.push_str(&self.description);
                out.push('\n');
            }
            out.push_str(self.prompt.trim());
            out.push('\n');
        }
        for file in &self.prompt_files {
            let path = resolve_file(cwd, self.source.as_deref(), file);
            match argent_core::read_text_file(&path) {
                Ok(text) => {
                    let text = argent_core::text::truncate(&text, 20_000);
                    out.push_str(&format!(
                        "\n### Profile file: {}\n{}\n",
                        path.display(),
                        text.trim()
                    ));
                }
                Err(error) => out.push_str(&format!(
                    "\n### Profile file missing: {} ({})\n",
                    path.display(),
                    error
                )),
            }
        }
        out
    }

    /// Apply the profile's config-level overrides. CLI flags still win because
    /// this runs before the agent is built, not after the operator replies.
    pub fn apply_config(&self, config: &mut argent_core::config::Config) {
        if let Some(model) = &self.model {
            config.model = model.clone();
        }
        if let Some(thinking) = &self.thinking {
            config.thinking_level = thinking.clone();
        }
        if let Some(approval) = &self.approval {
            if let Some(mode) = argent_core::config::ApprovalMode::from_name(approval) {
                config.approval_mode = mode;
            }
        }
        self.apply_policy(config);
    }

    /// Apply only the parts that do not change provider resolution: plugins,
    /// tool filters, budget and the profile marker.
    pub fn apply_policy(&self, config: &mut argent_core::config::Config) {
        if let Some(plugins) = &self.plugins {
            config.plugins = plugins.clone();
        }
        config.allowed_tools = intersect_includes(&config.allowed_tools, self.tools.as_deref());
        for name in &self.exclude_tools {
            if !config.excluded_tools.contains(name) {
                config.excluded_tools.push(name.clone());
            }
        }
        if let Some(cost) = self.max_cost_usd {
            config.max_cost_usd = Some(cost);
        }
        if let Some(tokens) = self.max_total_tokens {
            config.max_total_tokens = Some(tokens);
        }
        if let Some(seconds) = self.max_wall_seconds {
            config.max_wall_seconds = Some(seconds);
        }
        if let Some(price) = self.price_input_per_mtok {
            config.price_input_per_mtok = Some(price);
        }
        if let Some(price) = self.price_output_per_mtok {
            config.price_output_per_mtok = Some(price);
        }
        config
            .extra
            .insert("profile".to_string(), Json::string(self.name.clone()));
    }
}

/// All profiles visible from one workspace.
#[derive(Debug, Clone, Default)]
pub struct ProfileRegistry {
    profiles: BTreeMap<String, Profile>,
}

impl ProfileRegistry {
    /// Load user files, then project files, then inline `profiles` settings.
    pub fn load(cwd: &Path, settings: &Settings) -> Result<ProfileRegistry> {
        let paths = settings.paths();
        let mut registry = ProfileRegistry::default();
        registry.load_dir(&paths.storage().join("profiles"))?;
        registry.load_dir(&cwd.join(argent_config::APP_DIR).join("profiles"))?;
        registry.load_inline(settings)?;
        Ok(registry)
    }

    fn load_dir(&mut self, dir: &Path) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| Error::io(format!("cannot read {}: {}", dir.display(), e)))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect();
        files.sort();
        for path in files {
            let extension = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let fallback = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("profile")
                .to_string();
            let profile = match extension.as_str() {
                "md" | "markdown" => {
                    let text = argent_core::read_text_file(&path)?;
                    Profile::parse_markdown(&text, &fallback, Some(path.clone()))?
                }
                "json" => {
                    let text = argent_core::read_text_file(&path)?;
                    let value = Json::parse(&text).map_err(|e| {
                        Error::config(format!("invalid profile {}: {}", path.display(), e))
                    })?;
                    Profile::from_json(&value, &fallback)?
                }
                _ => continue,
            };
            self.profiles.insert(profile.name.clone(), profile);
        }
        Ok(())
    }

    fn load_inline(&mut self, settings: &Settings) -> Result<()> {
        let Some(value) = settings.get("profiles") else {
            return Ok(());
        };
        match value {
            Json::Obj(map) => {
                for (name, entry) in map {
                    let profile = Profile::from_json(entry, name)?;
                    self.profiles.insert(profile.name.clone(), profile);
                }
            }
            Json::Arr(items) => {
                for (index, entry) in items.iter().enumerate() {
                    let fallback = format!("profile-{}", index + 1);
                    let profile = Profile::from_json(entry, &fallback)?;
                    self.profiles.insert(profile.name.clone(), profile);
                }
            }
            _ => {
                return Err(Error::config(
                    "'profiles' must be an object or an array of objects",
                ))
            }
        }
        Ok(())
    }

    pub fn names(&self) -> Vec<&str> {
        self.profiles.keys().map(String::as_str).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name).or_else(|| {
            self.profiles
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, profile)| profile)
        })
    }

    pub fn list(&self) -> Vec<&Profile> {
        self.profiles.values().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\r\n")
        .or_else(|| text.strip_prefix("---\n"))?;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.trim() == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

fn parse_fields(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        out.push((key.trim().to_string(), value.to_string()));
    }
    out
}

fn split_list(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|item| item.trim().trim_matches('"').trim_matches('\''))
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn string_list(value: &Json) -> Vec<String> {
    match value {
        Json::Arr(items) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        Json::Str(text) => split_list(text),
        _ => Vec::new(),
    }
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn resolve_file(cwd: &Path, source: Option<&Path>, file: &str) -> PathBuf {
    let path = PathBuf::from(file);
    if path.is_absolute() {
        return path;
    }
    if let Some(source) = source {
        if let Some(parent) = source.parent() {
            let candidate = parent.join(&path);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    cwd.join(path)
}

/// Combine two include lists. An empty list means "no restriction".
pub fn intersect_includes(base: &[String], extra: Option<&[String]>) -> Vec<String> {
    match extra {
        None => base.to_vec(),
        Some(extra) if base.is_empty() => extra.to_vec(),
        Some(extra) => base
            .iter()
            .filter(|name| extra.iter().any(|other| other == *name))
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_markdown_profile() {
        let text = "---\nname: reviewer\ndescription: Review code\nmodel: GLM-5.3\ntools: read, grep\nreadOnly: true\nmaxCostUsd: 0.5\n---\nBe careful.\n";
        let profile = Profile::parse_markdown(text, "fallback", None).unwrap();
        assert_eq!(profile.name, "reviewer");
        assert_eq!(profile.model.as_deref(), Some("GLM-5.3"));
        assert_eq!(profile.tools.unwrap(), vec!["read", "grep"]);
        assert!(profile.read_only);
        assert_eq!(profile.max_cost_usd, Some(0.5));
        assert_eq!(profile.prompt, "Be careful.");
    }

    #[test]
    fn inline_json_profile_parses() {
        let value = Json::parse(
            r#"{"description":"d","tools":["read"],"readOnly":true,"maxTurns":7,"plugins":"explore"}"#,
        )
        .unwrap();
        let profile = Profile::from_json(&value, "review").unwrap();
        assert_eq!(profile.name, "review");
        assert_eq!(profile.max_turns, Some(7));
        assert!(profile.read_only);
        assert_eq!(profile.plugins.unwrap(), vec!["explore"]);
    }

    #[test]
    fn include_lists_intersect() {
        let base = vec!["read".to_string(), "bash".to_string()];
        let extra = vec!["read".to_string(), "grep".to_string()];
        assert_eq!(intersect_includes(&base, Some(&extra)), vec!["read"]);
        assert_eq!(intersect_includes(&base, None), base);
        assert_eq!(intersect_includes(&[], Some(&extra)), extra);
    }
}
