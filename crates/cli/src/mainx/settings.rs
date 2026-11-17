//! Configuration layering: CLI flags, settings file, built-in defaults.

use crate::*;
/// Apply CLI overrides on top of the loaded configuration.
pub(crate) fn apply_cli(config: &mut Config, cli: &Cli) {
    if let Some(model) = &cli.model {
        config.model = model.clone();
    }
    if let Some(url) = &cli.base_url {
        config.base_url = url.clone();
    }
    if let Some(key) = &cli.api_key {
        config.api_key = Some(key.clone());
    }
    if let Some(approval) = &cli.approval {
        if let Some(mode) = ApprovalMode::from_name(approval) {
            config.approval_mode = mode;
        }
    }
    if cli.no_auto_compact {
        config.auto_compaction = false;
    }
    if let Some(theme) = &cli.theme {
        config.theme = theme.clone();
    }
    if let Some(thinking) = &cli.thinking {
        config.thinking_level = thinking.clone();
    }
    if cli.yolo {
        config.approval_mode = ApprovalMode::Never;
    }
    if !cli.add_dirs.is_empty() {
        // 额外工作目录写进配置的扩展区，工具层和提示词都能读到同一份清单。
        let dirs = cli
            .add_dirs
            .iter()
            .map(|dir| Json::string(dir.display().to_string()))
            .collect();
        config.extra.insert("addDirs".to_string(), Json::Arr(dirs));
    }
    if let Some(mode) = &cli.sandbox_mode {
        config
            .extra
            .insert("sandboxMode".to_string(), Json::string(mode.clone()));
    }
}

/// 应用 `-c key=value` / `--config key=value` 覆盖。
///
/// 值先按 JSON 解析（`true`、`64000`、`["read"]`），解析失败按字符串处理；
/// 点路径构造嵌套对象，`tools.include` 等价于 `{"tools":{"include":...}}`。
/// 这一层位于配置文件与 `ARGENT_*` 环境变量之间，`--model` 等显式 flag
/// 仍然优先。
pub(crate) fn apply_config_overrides(
    config: &mut Config,
    overrides: &[(String, String)],
) -> Result<()> {
    if overrides.is_empty() {
        return Ok(());
    }
    let mut root = Json::obj();
    for (key, raw) in overrides {
        let value = Json::parse(raw).unwrap_or_else(|_| Json::string(raw.clone()));
        insert_dotted(&mut root, key, value);
    }
    argent_core::config::merge_into(config, &root.dump(), Path::new("<command line>"))
}

pub(crate) fn insert_dotted(root: &mut Json, key: &str, value: Json) {
    match key.split_once('.') {
        None => root.set(key, value),
        Some((head, rest)) => {
            if root.get(head).is_none() {
                root.set(head, Json::obj());
            }
            if let Some(child) = root.get_mut(head) {
                insert_dotted(child, rest, value);
            }
        }
    }
}

/// `argent review` 的提示词：只读评审，默认看未提交的改动。
pub(crate) fn review_prompt(target: Option<&str>) -> String {
    let target = target.unwrap_or(
        "the uncommitted changes (run `git diff` and `git diff --staged`; if the tree \
         is clean, review the last commit)",
    );
    format!(
        "Review {target} as a meticulous senior engineer. Read the changed files and \
         enough surrounding code to judge them. Do not modify anything. Report only \
         real defects: correctness bugs, security problems, data loss, races, gaps in \
         error handling, and regressions this change introduces. For each finding give \
         the file and line, the concrete failure scenario, and a fix. Order findings by \
         severity. If nothing material is wrong, say so plainly instead of padding the \
         list."
    )
}

/// Apply the layered argent settings and resolve the provider template.
///
/// CLI flags win over settings; settings win over the built-in catalog
/// defaults. The API key is resolved from the flag, then the credential store,
/// then the template's environment variable.
pub(crate) fn apply_settings(
    config: &mut Config,
    cli: &Cli,
    profile: Option<&argent_profiles::Profile>,
) -> Result<()> {
    let settings = Settings::load(&cli.cwd)?;
    if cli.locale.is_none() {
        if let Some(locale) = settings.string("locale") {
            if argent_i18n::Locale::parse(locale).is_some() {
                std::env::set_var("ARGENT_LOCALE", locale);
            }
        }
    }
    let mut catalog = ProviderCatalog::builtin();
    catalog.merge(settings.raw());

    // Precedence: CLI flag, then the named profile, then settings.
    let provider = cli
        .provider
        .as_deref()
        .or_else(|| profile.and_then(|p| p.provider.as_deref()))
        .or_else(|| settings.provider());
    let model = cli
        .model
        .as_deref()
        .or_else(|| profile.and_then(|p| p.model.as_deref()))
        .or_else(|| settings.model());
    let resolved = catalog.resolve(provider, model)?;

    if cli.base_url.is_none() {
        config.base_url = resolved.provider.base_url.clone();
    }
    if cli.model.is_none() {
        config.model = resolved.model.id.clone();
    }
    // A `prices` block turns token counts into dollars without a flag:
    // {"prices":{"glm-5.3":{"input":0.6,"output":2.2}}}
    if let Some(price) = settings
        .get("prices")
        .and_then(|prices| prices.get(&config.model))
    {
        if config.price_input_per_mtok.is_none() {
            config.price_input_per_mtok = price
                .get("input")
                .and_then(Json::as_f64)
                .or_else(|| price.get("inputPerMtok").and_then(Json::as_f64));
        }
        if config.price_output_per_mtok.is_none() {
            config.price_output_per_mtok = price
                .get("output")
                .and_then(Json::as_f64)
                .or_else(|| price.get("outputPerMtok").and_then(Json::as_f64));
        }
    }
    if cli.thinking.is_none() {
        let thinking = profile
            .and_then(|p| p.thinking.clone())
            .or_else(|| settings.thinking().map(str::to_string));
        if let Some(thinking) = thinking {
            config.thinking_level = thinking;
        }
    }
    if settings.get("sessionDir").is_none() && std::env::var_os("ARGENT_SESSION_DIR").is_none() {
        config.session_dir = Paths::new(&cli.cwd).sessions();
    }
    if settings.string("sessionBackend") == Some("sqlite")
        && std::env::var_os("ARGENT_SQLITE").is_none()
    {
        std::env::set_var(
            "ARGENT_SQLITE",
            Paths::new(&cli.cwd)
                .storage()
                .join("cli")
                .join("db")
                .join("db.sqlite"),
        );
    }
    if resolved.model.context > 0 && settings.get("maxContextTokens").is_none() {
        config.max_context_tokens = resolved.model.context;
    }
    if resolved.model.max_output > 0 && settings.get("maxOutputTokens").is_none() {
        config.max_output_tokens = resolved.model.max_output;
    }
    config.supports_vision = resolved.model.vision;
    if cli.approval.is_none() {
        let mode = profile
            .and_then(|p| p.approval.as_deref())
            .or_else(|| settings.approval_mode());
        if let Some(mode) = mode {
            if let Some(parsed) = ApprovalMode::from_name(mode) {
                config.approval_mode = parsed;
            }
        }
    }
    if let Some(mode) = &cli.mode {
        apply_mode(config, mode);
    }
    config.provider = resolved.provider.id.clone();
    config.protocol = resolved.provider.protocol.name().to_string();
    config.api_key_env = resolved.provider.api_key_env.clone();

    if cli.api_key.is_none() {
        let mut store = AuthStore::load()?;
        if let Some(oauth) = &resolved.provider.oauth {
            if let Err(error) =
                argent_auth::oauth::refresh_if_needed(&mut store, &resolved.provider.id, oauth)
            {
                eprintln!("token refresh failed: {}", error);
            }
        }
        config.api_key = store.resolve_key(&resolved.provider, None);
    }
    Ok(())
}

/// Map ZCode's interaction modes onto the approval policy.
///
/// A full plan mode also filters mutating tools; this project starts with the
/// gate, which is what protects the workspace in every mode.
pub(crate) fn apply_mode(config: &mut Config, mode: &str) {
    let mode = mode.to_ascii_lowercase();
    config.approval_mode = match mode.as_str() {
        "yolo" => ApprovalMode::Never,
        "edit" | "guarded" => ApprovalMode::Sensible,
        "build" | "plan" => ApprovalMode::Always,
        _ => config.approval_mode,
    };
    config.extra.insert("mode".to_string(), Json::string(mode));
}
