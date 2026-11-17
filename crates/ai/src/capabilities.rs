//! 按模型能力裁剪请求的规则层。
//!
//! 模型目录（`providers.json` 模板或端点 `/models`）声明的能力在这里变成
//! 请求构造规则：非视觉模型不发图片，不支持 reasoning 的模型不发 thinking。
//! 三种协议共用同一套判定，避免 Anthropic 与 OpenAI 两条路径行为分叉。
//!
//! 目录里查不到的模型按“全部支持”处理：把支持的能力误裁掉比让端点报错
//! 更难发现，未知时宁可原样发送。

use crate::message::ModelInfo;

/// 一次请求需要遵守的能力集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub vision: bool,
    pub reasoning: bool,
}

impl Default for Capabilities {
    fn default() -> Capabilities {
        Capabilities {
            vision: true,
            reasoning: true,
        }
    }
}

impl Capabilities {
    /// 按模型 id 在目录里查能力；大小写不敏感，未知模型返回全支持。
    pub fn for_model(models: &[ModelInfo], id: &str) -> Capabilities {
        models
            .iter()
            .find(|model| model.id.eq_ignore_ascii_case(id))
            .map(|model| Capabilities {
                vision: model.supports_vision,
                reasoning: model.supports_reasoning,
            })
            .unwrap_or_default()
    }

    /// 图片被裁掉时替换成的文字说明。
    ///
    /// 留一句话而不是静默丢弃：模型至少知道原消息里有过一张图，可以据此
    /// 请用户换模型重发，而不是困惑于“图片在哪”。
    pub fn image_placeholder(model: &str) -> String {
        format!(
            "[image omitted: model {} does not accept image input; ask the user to \
             switch to a vision-capable model and resend]",
            model
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argent_core::Json;

    fn model(id: &str, vision: bool, reasoning: bool) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            display_name: id.to_string(),
            context_length: 0,
            max_output: 0,
            supports_tools: true,
            supports_reasoning: reasoning,
            supports_vision: vision,
            reasoning_levels: Vec::new(),
        }
    }

    #[test]
    fn catalog_capabilities_are_looked_up_case_insensitively() {
        let models = vec![model("text-only", false, false)];
        let caps = Capabilities::for_model(&models, "TEXT-ONLY");
        assert!(!caps.vision);
        assert!(!caps.reasoning);
    }

    #[test]
    fn unknown_models_keep_everything() {
        let caps = Capabilities::for_model(&[], "mystery");
        assert_eq!(caps, Capabilities::default());
        assert!(caps.vision && caps.reasoning);
    }

    #[test]
    fn catalog_json_carries_vision_and_output_limit() {
        let value = Json::parse(
            r#"{"id":"m","context_length":1000,"maxOutput":512,"vision":false,"reasoning":true}"#,
        )
        .unwrap();
        let info = ModelInfo::from_json(&value).expect("parse");
        assert!(!info.supports_vision);
        assert_eq!(info.max_output, 512);
        assert!(info.supports_reasoning);
    }
}
