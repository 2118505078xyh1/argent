//! Provider templates, model catalog and client construction.
//!
//! ZCode separates three things that are easy to conflate:
//!
//! * a **provider template** — a vendor, a base URL and an auth source;
//! * a **protocol** — Anthropic Messages, OpenAI chat completions or OpenAI
//!   responses;
//! * a **model** — an id plus its capabilities.
//!
//! The catalog below is a clean-room summary of the endpoints and model ids the
//! user already has credentials for. It is data, not ZCode source: users can
//! override any template or add their own in `~/.argent/cli/config.json`
//! under `providers`.

use argent_ai::{
    AnthropicConfig, AnthropicProvider, ModelInfo, OpenAiConfig, OpenAiProvider, Provider,
    ResponsesConfig, ResponsesProvider,
};
use argent_core::error::{Error, Result};
use argent_core::json::Json;

pub mod models;

pub use models::ModelCatalog;

/// Wire protocol spoken by a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Anthropic `/v1/messages`, including compatible gateways.
    AnthropicMessages,
    /// OpenAI `/chat/completions`.
    OpenAiChat,
    /// OpenAI `/responses`.
    OpenAiResponses,
}

impl Protocol {
    pub fn parse(value: &str) -> Option<Protocol> {
        match value.to_ascii_lowercase().as_str() {
            "anthropic" | "anthropic-messages" | "anthropic_messages" => {
                Some(Protocol::AnthropicMessages)
            }
            "openai" | "openai-chat" | "openai-chat-completions" | "chat" => {
                Some(Protocol::OpenAiChat)
            }
            "openai-responses" | "responses" => Some(Protocol::OpenAiResponses),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Protocol::AnthropicMessages => "anthropic-messages",
            Protocol::OpenAiChat => "openai-chat-completions",
            Protocol::OpenAiResponses => "openai-responses",
        }
    }
}

/// One model in a provider's catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: String,
    pub name: String,
    pub context: usize,
    pub max_output: usize,
    pub tools: bool,
    pub reasoning: bool,
    /// 是否接受图片输入；模板没写时按 true 处理，与 argent-ai 的保守默认一致。
    pub vision: bool,
}

impl ModelSpec {
    pub fn from_json(value: &Json) -> Option<ModelSpec> {
        let id = value.str_or("id", "");
        if id.is_empty() {
            return None;
        }
        Some(ModelSpec {
            name: value.str_or("name", &id),
            context: value
                .get("context")
                .and_then(Json::as_i64)
                .unwrap_or(0)
                .max(0) as usize,
            max_output: value
                .get("maxOutput")
                .and_then(Json::as_i64)
                .unwrap_or(0)
                .max(0) as usize,
            tools: value.bool_or("tools", true),
            reasoning: value.bool_or("reasoning", false),
            vision: value.bool_or("vision", true),
            id,
        })
    }

    /// 序列化回 JSON，字段名与解析时一致，便于写入模型缓存。
    pub fn to_json(&self) -> Json {
        let mut value = Json::obj();
        value.set("id", Json::string(self.id.clone()));
        value.set("name", Json::string(self.name.clone()));
        value.set("context", Json::num(self.context as f64));
        value.set("maxOutput", Json::num(self.max_output as f64));
        value.set("tools", Json::bool(self.tools));
        value.set("reasoning", Json::bool(self.reasoning));
        value.set("vision", Json::bool(self.vision));
        value
    }

    /// An ad-hoc entry for a model id that is not in the catalog.
    pub fn unlisted(id: &str) -> ModelSpec {
        ModelSpec {
            id: id.to_string(),
            name: id.to_string(),
            context: 0,
            max_output: 0,
            tools: true,
            reasoning: true,
            vision: true,
        }
    }

    pub fn to_model_info(&self) -> ModelInfo {
        ModelInfo {
            id: self.id.clone(),
            display_name: self.name.clone(),
            context_length: self.context,
            max_output: self.max_output,
            supports_tools: self.tools,
            supports_reasoning: self.reasoning,
            supports_vision: self.vision,
            reasoning_levels: if self.reasoning {
                vec![
                    "disabled".to_string(),
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                ]
            } else {
                Vec::new()
            },
        }
    }
}

/// OAuth 2.0 authorization-code settings for a provider.
#[derive(Debug, Clone)]
pub struct OAuthSpec {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub redirect_port: u16,
    pub audience: Option<String>,
}

impl OAuthSpec {
    pub fn from_json(value: &Json) -> Option<OAuthSpec> {
        let authorize_url = value.str_or("authorizeUrl", "");
        let token_url = value.str_or("tokenUrl", "");
        let client_id = value.str_or("clientId", "");
        if authorize_url.is_empty() || token_url.is_empty() || client_id.is_empty() {
            return None;
        }
        Some(OAuthSpec {
            authorize_url,
            token_url,
            client_id,
            scopes: value
                .get("scopes")
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            redirect_port: value
                .get("redirectPort")
                .and_then(Json::as_i64)
                .unwrap_or(0)
                .clamp(0, 65535) as u16,
            audience: value
                .get("audience")
                .and_then(Json::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        })
    }
}

/// A vendor endpoint plus the models it exposes.
#[derive(Debug, Clone)]
pub struct ProviderSpec {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub default_model: String,
    pub thinking: bool,
    pub models: Vec<ModelSpec>,
    pub oauth: Option<OAuthSpec>,
}

impl ProviderSpec {
    pub fn from_json(value: &Json) -> Option<ProviderSpec> {
        let id = value.str_or("id", "");
        let base_url = value.str_or("baseUrl", "");
        if id.is_empty() || base_url.is_empty() {
            return None;
        }
        let protocol = value
            .get("protocol")
            .and_then(Json::as_str)
            .and_then(Protocol::parse)
            .unwrap_or(Protocol::OpenAiChat);
        let models: Vec<ModelSpec> = value
            .get("models")
            .and_then(Json::as_array)
            .map(|items| items.iter().filter_map(ModelSpec::from_json).collect())
            .unwrap_or_default();
        let default_model = value.str_or("defaultModel", "").trim().to_string();
        let default_model = if default_model.is_empty() {
            models
                .first()
                .map(|model| model.id.clone())
                .unwrap_or_default()
        } else {
            default_model
        };
        Some(ProviderSpec {
            name: value.str_or("name", &id),
            api_key_env: value
                .get("apiKeyEnv")
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            thinking: value.bool_or("thinking", true),
            models,
            default_model,
            protocol,
            base_url,
            id,
            oauth: value.get("oauth").and_then(OAuthSpec::from_json),
        })
    }

    pub fn model(&self, requested: Option<&str>) -> ModelSpec {
        let id = requested
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&self.default_model);
        self.models
            .iter()
            .find(|model| model.id == id)
            .cloned()
            .unwrap_or_else(|| ModelSpec::unlisted(id))
    }

    /// API key from the template's environment variable, if set.
    pub fn env_api_key(&self) -> Option<String> {
        let name = self.api_key_env.as_deref()?;
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }
}

/// A resolved provider plus the model selected for this session.
#[derive(Debug, Clone)]
pub struct ResolvedProvider {
    pub provider: ProviderSpec,
    pub model: ModelSpec,
}

/// The catalog, in lookup order.
#[derive(Debug, Clone, Default)]
pub struct ProviderCatalog {
    providers: Vec<ProviderSpec>,
}

impl ProviderCatalog {
    /// The templates shipped with argent.
    pub fn builtin() -> ProviderCatalog {
        let text = include_str!("../providers.json");
        let value = Json::parse(text).expect("embedded providers.json is valid");
        ProviderCatalog::from_json(&value).expect("embedded providers.json has providers")
    }

    pub fn from_json(value: &Json) -> Option<ProviderCatalog> {
        let items = value.get("providers").and_then(Json::as_array)?;
        let providers = items
            .iter()
            .filter_map(ProviderSpec::from_json)
            .collect::<Vec<_>>();
        if providers.is_empty() {
            return None;
        }
        Some(ProviderCatalog { providers })
    }

    /// Add or replace templates from a config document.
    ///
    /// A user template with the same id replaces the built-in one, which is how
    /// a private gateway is configured without forking the catalog.
    pub fn merge(&mut self, value: &Json) {
        let items = match value
            .get("providers")
            .or_else(|| value.get("providerTemplates"))
            .and_then(Json::as_array)
        {
            Some(items) => items,
            None => return,
        };
        for item in items {
            let Some(spec) = ProviderSpec::from_json(item) else {
                continue;
            };
            if let Some(existing) = self.providers.iter_mut().find(|p| p.id == spec.id) {
                *existing = spec;
            } else {
                self.providers.push(spec);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&ProviderSpec> {
        self.providers.iter().find(|provider| provider.id == id)
    }

    pub fn list(&self) -> &[ProviderSpec] {
        &self.providers
    }

    /// Pick the first template whose id matches, falling back to a built-in
    /// local OpenAI-compatible endpoint.
    pub fn resolve(&self, provider: Option<&str>, model: Option<&str>) -> Result<ResolvedProvider> {
        let id = provider
            .filter(|value| !value.is_empty())
            .unwrap_or("local");
        let spec = self.get(id).or_else(|| self.get("local")).ok_or_else(|| {
            Error::config(format!(
                "unknown provider '{}'; known providers: {}",
                id,
                self.providers
                    .iter()
                    .map(|p| p.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
        Ok(ResolvedProvider {
            provider: spec.clone(),
            model: spec.model(model),
        })
    }

    /// Build the protocol client for a resolved provider.
    pub fn build(
        &self,
        resolved: &ResolvedProvider,
        api_key: Option<String>,
        thinking: &str,
        max_tokens: usize,
    ) -> Box<dyn Provider> {
        match resolved.provider.protocol {
            Protocol::AnthropicMessages => {
                let mut config = AnthropicConfig::new(resolved.provider.base_url.clone())
                    .with_max_tokens(max_tokens)
                    .with_thinking(
                        resolved.provider.thinking
                            && !matches!(
                                thinking.to_ascii_lowercase().as_str(),
                                "" | "off" | "none" | "disabled"
                            ),
                    )
                    .with_models(
                        resolved
                            .provider
                            .models
                            .iter()
                            .map(ModelSpec::to_model_info)
                            .collect(),
                    );
                if let Some(key) = api_key {
                    config = config.with_api_key(key);
                }
                Box::new(AnthropicProvider::new(config))
            }
            Protocol::OpenAiResponses => {
                let mut config = ResponsesConfig::new(resolved.provider.base_url.clone())
                    .with_max_output_tokens(max_tokens)
                    .with_models(
                        resolved
                            .provider
                            .models
                            .iter()
                            .map(ModelSpec::to_model_info)
                            .collect(),
                    );
                if let Some(key) = api_key {
                    config = config.with_api_key(key);
                }
                Box::new(ResponsesProvider::new(config))
            }
            Protocol::OpenAiChat => {
                let mut config = OpenAiConfig::new(resolved.provider.base_url.clone()).with_models(
                    resolved
                        .provider
                        .models
                        .iter()
                        .map(ModelSpec::to_model_info)
                        .collect(),
                );
                if let Some(key) = api_key {
                    config = config.with_api_key(key);
                }
                Box::new(OpenAiProvider::new(config))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_has_the_primary_providers() {
        let catalog = ProviderCatalog::builtin();
        for id in [
            "local",
            "zai",
            "bigmodel",
            "anthropic",
            "deepseek",
            "openai",
        ] {
            assert!(catalog.get(id).is_some(), "missing provider {}", id);
        }
        let zai = catalog.get("zai").unwrap();
        assert_eq!(zai.protocol, Protocol::AnthropicMessages);
        assert_eq!(zai.default_model, "GLM-5.3");
    }

    #[test]
    fn an_unknown_model_is_still_usable() {
        let catalog = ProviderCatalog::builtin();
        let resolved = catalog.resolve(Some("zai"), Some("glm-private")).unwrap();
        assert_eq!(resolved.model.id, "glm-private");
        assert!(resolved.model.tools);
    }

    #[test]
    fn a_user_template_replaces_a_builtin_one() {
        let mut catalog = ProviderCatalog::builtin();
        catalog.merge(
            &Json::parse(
                r#"{"providers":[{"id":"zai","name":"Private","protocol":"anthropic-messages","baseUrl":"http://localhost:9","defaultModel":"m"}]}"#,
            )
            .unwrap(),
        );
        let zai = catalog.get("zai").unwrap();
        assert_eq!(zai.name, "Private");
        assert_eq!(zai.base_url, "http://localhost:9");
    }

    #[test]
    fn protocol_names_round_trip() {
        assert_eq!(
            Protocol::parse("anthropic-messages"),
            Some(Protocol::AnthropicMessages)
        );
        assert_eq!(Protocol::parse("openai-chat"), Some(Protocol::OpenAiChat));
        assert_eq!(Protocol::parse("nope"), None);
        assert_eq!(Protocol::OpenAiResponses.name(), "openai-responses");
    }
}
