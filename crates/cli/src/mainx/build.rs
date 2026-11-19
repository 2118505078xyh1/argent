//! Provider construction and the resolved-model summary.

use crate::*;
/// Build the provider selected by the resolved protocol.
pub(crate) fn build_provider(config: &Config) -> Box<dyn Provider> {
    let api_key = config.api_key.clone().or_else(|| {
        config
            .api_key_env
            .as_ref()
            .and_then(|name| std::env::var(name).ok())
            .filter(|value| !value.is_empty())
    });
    match config.protocol.as_str() {
        "anthropic-messages" => {
            let mut provider = AnthropicConfig::new(config.base_url.clone())
                .with_max_tokens(config.max_output_tokens)
                .with_models(vec![model_info(config)])
                .with_thinking(!matches!(
                    config.thinking_level.to_ascii_lowercase().as_str(),
                    "off" | "none" | "disabled"
                ));
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            Box::new(AnthropicProvider::new(provider))
        }
        "openai-responses" => {
            let mut provider = ResponsesConfig::new(config.base_url.clone())
                .with_max_output_tokens(config.max_output_tokens)
                .with_models(vec![model_info(config)]);
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            Box::new(ResponsesProvider::new(provider))
        }
        _ => {
            let mut provider = OpenAiConfig::new(config.base_url.clone());
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            Box::new(OpenAiProvider::new(provider))
        }
    }
}

/// The selected model's capabilities, as the catalog resolved them.
pub(crate) fn model_info(config: &Config) -> argent_ai::ModelInfo {
    argent_ai::ModelInfo {
        id: config.model.clone(),
        display_name: config.model.clone(),
        context_length: config.max_context_tokens,
        max_output: config.max_output_tokens,
        supports_tools: true,
        supports_reasoning: !matches!(
            config.thinking_level.to_ascii_lowercase().as_str(),
            "off" | "none" | "disabled"
        ),
        // provider 模板声明的视觉能力；配置层不知道时为 true。
        supports_vision: config.supports_vision,
        reasoning_levels: vec![
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
            "max".to_string(),
        ],
    }
}

/// Build the tool execution context.
pub(crate) fn build_ctx(config: &Config, cwd: &Path) -> ToolCtx {
    let mut ctx = ToolCtx::new(cwd.to_path_buf());
    ctx.timeout_secs = config.tool_timeout_secs;
    ctx.confirm_delete = config.confirm_delete;
    ctx.auto_approve_bash = config.auto_approve_bash;
    ctx.vars.insert(
        "approval_mode".to_string(),
        match config.approval_mode {
            argent_core::config::ApprovalMode::Never => "never".to_string(),
            _ => "ask".to_string(),
        },
    );
    // workspace-write：write/edit 只能落在 cwd 与 --add-dir 里。read-only 由
    // 工具注册表直接摘掉写工具，这里不必再设根。danger-full-access 保持空。
    //
    // 默认收紧到 workspace-write：写工具弹审批只是 UX，bash 不会问，所以
    // 文件边界必须在工具层兜底。要完全放开的操作者显式传
    // `--sandbox-mode danger-full-access`（或配 profile）。
    let mode = config
        .extra
        .get("sandboxMode")
        .and_then(Json::as_str)
        .unwrap_or("workspace-write");
    if mode == "workspace-write" {
        let mut roots = vec![cwd.to_path_buf()];
        if let Some(Json::Arr(dirs)) = config.extra.get("addDirs") {
            roots.extend(dirs.iter().filter_map(Json::as_str).map(PathBuf::from));
        }
        ctx.write_roots = roots;
    }
    let lsp_root = cwd.to_path_buf();
    ctx.post_edit = Some(argent_tools::PostEditHook::new(move |path| {
        argent_lsp::diagnostics_if_running(&lsp_root, path, std::time::Duration::from_millis(1000))
    }));
    ctx
}

/// Derive a session title from the first prompt.
/// The name a session is filed under.
///
/// The operator's own first message is the title, verbatim: a slugified
/// "fix-the-bug-in-main.rs" tells them less than what they actually typed. A
/// session that has not been used yet says so.
pub(crate) fn session_title(prompt: Option<&str>) -> String {
    match prompt {
        Some(text) => {
            let line = text.lines().next().unwrap_or("").trim();
            if line.is_empty() {
                "新任务".to_string()
            } else {
                line.chars().take(60).collect()
            }
        }
        None => "新任务".to_string(),
    }
}
