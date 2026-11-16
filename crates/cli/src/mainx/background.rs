//! Glue between the loaded config and [`argent_ai::BackgroundAssistant`]:
//! builds the provider from the main config and resolves the cheaper model
//! name. `None` when the feature is off.

use argent_ai::{BackgroundAssistant as Assistant, Provider};
use argent_core::config::Config;

pub struct BackgroundAssistant {
    inner: Assistant,
}

impl Clone for BackgroundAssistant {
    fn clone(&self) -> Self {
        BackgroundAssistant {
            inner: self.inner.clone(),
        }
    }
}

impl BackgroundAssistant {
    /// Build from the loaded config, or `None` when the feature is off.
    ///
    /// The model defaults to the main model; `backgroundModel` names a
    /// cheaper one. The provider is rebuilt rather than shared: the agent's
    /// instance is owned by the run loop and this call must not contend with
    /// it for stream state.
    pub fn from_config(config: &Config) -> Option<BackgroundAssistant> {
        if !config.background_intelligence {
            return None;
        }
        let model = config
            .background_model
            .clone()
            .unwrap_or_else(|| config.model.clone());
        let provider: std::sync::Arc<dyn Provider> =
            std::sync::Arc::from(crate::mainx::build::build_provider(config));
        Some(BackgroundAssistant {
            inner: Assistant::new(provider, model),
        })
    }

    pub fn title_for(&self, first_exchange: &str) -> Option<String> {
        self.inner.title_for(first_exchange)
    }

    pub fn summarize_for(&self, heuristic: &str) -> Option<String> {
        self.inner.summarize_for(heuristic)
    }

    pub fn branch_summary_for(&self, raw_branch_text: &str) -> Option<String> {
        self.inner.branch_summary_for(raw_branch_text)
    }
}
