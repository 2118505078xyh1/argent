//! `argent models`：查看合并后的模型目录，或从远端刷新本地缓存。
//!
//! - `list [--json]`：展示三层合并（内置 < 缓存 < 配置覆盖）后的目录，
//!   字段为 id / context / 是否支持 tools / reasoning / vision，顺序稳定；
//! - `update [--url <u>] [--public-key <hex|path>] [--force]`：取远端目录，
//!   可选做 Ed25519 分离签名校验，写进 `<home>/.argent/models.json`，并报告
//!   相对当前目录新增/覆盖了多少条。
//!
//! URL 来源优先级：`--url` > 设置 `models.url` > 扁平键 `modelsUrl`。都没有时
//! 明确报错并说明怎么配，绝不静默成功；`--force` 表示目录没变也照样重写缓存。

use argent_config::Settings;
use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_provider::models::{self, ModelCatalog};

/// 解析后的 `argent models` 子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelsCommand {
    /// 列出合并后的目录。
    List { json: bool },
    /// 从远端刷新缓存。
    Update {
        url: Option<String>,
        public_key: Option<String>,
        force: bool,
    },
}

/// `argent models` 的用法文本。
const USAGE: &str = "\
usage: argent models list [--json]
       argent models update [--url <u>] [--public-key <hex|path>] [--force]

update reads the remote catalog URL from --url, or the models.url setting
(flat key modelsUrl); with --public-key it verifies an Ed25519 signature over
the document without its 'signature' field, then writes <home>/.argent/models.json.";

/// 解析 `argent models ...` 的参数。
pub(crate) fn parse(args: &[String]) -> Result<ModelsCommand> {
    let mut json = false;
    let mut force = false;
    let mut url: Option<String> = None;
    let mut public_key: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--force" => force = true,
            "--url" => {
                index += 1;
                url = Some(
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| Error::config("--url needs a value"))?,
                );
            }
            "--public-key" => {
                index += 1;
                public_key = Some(
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| Error::config("--public-key needs a value"))?,
                );
            }
            // 全局的 -C/--cwd 由 cwd_from 处理，这里跳过后面的取值。
            "--cwd" | "-C" => index += 1,
            other if other.starts_with('-') => {
                return Err(Error::config(format!("unknown models option '{}'", other)));
            }
            other => positional.push(other.to_string()),
        }
        index += 1;
    }

    let action = positional.first().map(String::as_str).unwrap_or("list");
    match action {
        "list" | "ls" => {
            if positional.len() > 1 {
                return Err(Error::config("usage: argent models list [--json]"));
            }
            Ok(ModelsCommand::List { json })
        }
        "update" => {
            if positional.len() > 1 {
                return Err(Error::config(
                    "usage: argent models update [--url <u>] [--public-key <hex|path>] [--force]",
                ));
            }
            Ok(ModelsCommand::Update {
                url,
                public_key,
                force,
            })
        }
        other => Err(Error::config(format!(
            "unknown models command '{}'; expected list or update",
            other
        ))),
    }
}

/// 执行 `argent models ...`。
pub(crate) fn run(args: &[String]) -> Result<i32> {
    if crate::commands::wants_help(args) {
        println!("{}", USAGE);
        return Ok(0);
    }
    let command = parse(args)?;
    let cwd = crate::commands::cwd_from(args);
    let settings = Settings::load(&cwd)?;
    match command {
        ModelsCommand::List { json } => list(&settings, json),
        ModelsCommand::Update {
            url,
            public_key,
            force,
        } => update(&settings, url, public_key, force),
    }
}

/// 列出合并后的目录。顺序稳定：内置目录打底，缓存与配置覆盖的新 id 追加在末尾。
fn list(settings: &Settings, json: bool) -> Result<i32> {
    let catalog = models::load(Some(settings.raw()));
    if json {
        // 直接用目录自身的 JSON 形态：字段齐全、顺序稳定、可往返。
        println!("{}", catalog.to_json().dump_pretty());
        return Ok(0);
    }
    if catalog.is_empty() {
        println!("no models in the catalog");
        return Ok(0);
    }
    println!(
        "{:<32} {:>10} {:<8} {:<10} vision",
        "id", "context", "tools", "reasoning"
    );
    for model in catalog.list() {
        println!(
            "{:<32} {:>10} {:<8} {:<10} {}",
            model.id, model.context, model.tools, model.reasoning, model.vision
        );
    }
    Ok(0)
}

/// 从远端刷新缓存。
fn update(
    settings: &Settings,
    cli_url: Option<String>,
    cli_public_key: Option<String>,
    force: bool,
) -> Result<i32> {
    let url = resolve_url(settings, cli_url).ok_or_else(|| {
        Error::config(
            "no models catalog URL configured; set \"models\": {\"url\": \"https://…\"} \
(或扁平键 modelsUrl) in argent.json, or pass --url <u>",
        )
    })?;
    let public_key = resolve_public_key(settings, cli_public_key);

    // 带公钥时校验远端文档的 Ed25519 分离签名（签名覆盖去掉 signature 字段后
    // 的 JSON 文本）；不带公钥时按“可选签名校验”跳过。
    let verify = public_key.as_ref().map(|key| {
        let key = key.clone();
        move |bytes: &[u8]| verify_catalog(bytes, &key)
    });
    let bytes = models::refresh(
        &url,
        verify.as_ref().map(|f| f as &dyn Fn(&[u8]) -> Result<()>),
    )?;

    let fetched = ModelCatalog::parse(&String::from_utf8_lossy(&bytes));
    if fetched.is_empty() {
        eprintln!(
            "warning: the fetched catalog at {} has no models; nothing was written",
            url
        );
        return Ok(1);
    }

    // “新增/覆盖”相对当前目录（内置 + 既有缓存）来算：用户看到的就是这个基线。
    let previous = models::load_cache();
    let baseline = models::resolve(previous.as_ref(), None);
    let mut added = 0usize;
    let mut updated = 0usize;
    for model in fetched.list() {
        if baseline.get(&model.id).is_some() {
            updated += 1;
        } else {
            added += 1;
        }
    }

    if !force && previous.as_ref() == Some(&fetched) {
        println!(
            "models catalog is already up to date ({} entries); pass --force to rewrite",
            fetched.len()
        );
        return Ok(0);
    }

    models::save_cache(&fetched)?;
    println!(
        "models catalog updated: {} added, {} updated, {} total (cache: {})",
        added,
        updated,
        fetched.len(),
        models::cache_path().display()
    );
    Ok(0)
}

/// URL 来源：`--url` > `models.url` > 扁平 `modelsUrl` / `models_url`。
fn resolve_url(settings: &Settings, cli_url: Option<String>) -> Option<String> {
    cli_url
        .filter(|value| !value.trim().is_empty())
        .or_else(|| nested_string(settings, "models", "url"))
        .or_else(|| flat_string(settings, &["modelsUrl", "models_url"]))
}

/// 公钥来源：`--public-key` > `models.publicKey` > 扁平 `modelsPublicKey`。
fn resolve_public_key(settings: &Settings, cli_key: Option<String>) -> Option<String> {
    cli_key
        .filter(|value| !value.trim().is_empty())
        .or_else(|| nested_string(settings, "models", "publicKey"))
        .or_else(|| nested_string(settings, "models", "public_key"))
        .or_else(|| flat_string(settings, &["modelsPublicKey", "models_public_key"]))
}

/// 校验远端目录字节：签名覆盖去掉 `signature` 字段后的 JSON 文本。
fn verify_catalog(bytes: &[u8], public_key: &str) -> Result<()> {
    let text = String::from_utf8_lossy(bytes);
    let value = Json::parse(&text)
        .map_err(|error| Error::config(format!("invalid models catalog: {}", error)))?;
    let signature = value
        .get("signature")
        .and_then(Json::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| Error::config("remote models catalog has no 'signature' field to verify"))?;
    // 重建去掉 signature 的对象，dump 出确定性文本作为被签名的载荷。
    let mut unsigned = Json::obj();
    if let Some(entries) = value.as_object() {
        for (key, item) in entries {
            if key == "signature" {
                continue;
            }
            unsigned.set(key, item.clone());
        }
    }
    argent_update::verify_detached(unsigned.dump().as_bytes(), signature, public_key)
}

/// 取设置里嵌套的一组键，去空白，空串视作没配。
fn nested_string(settings: &Settings, group: &str, key: &str) -> Option<String> {
    settings
        .get(group)
        .and_then(|value| value.get(key))
        .and_then(Json::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
}

/// 依次尝试若干扁平键，取第一个非空值。
fn flat_string(settings: &Settings, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| settings.string(key))
        .find(|text| !text.trim().is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn list_is_the_default_action() {
        assert_eq!(
            parse(&args(&[])).expect("parse"),
            ModelsCommand::List { json: false }
        );
        assert_eq!(
            parse(&args(&["list"])).expect("parse"),
            ModelsCommand::List { json: false }
        );
        assert_eq!(
            parse(&args(&["list", "--json"])).expect("parse"),
            ModelsCommand::List { json: true }
        );
    }

    #[test]
    fn parses_update_flags() {
        assert_eq!(
            parse(&args(&["update"])).expect("parse"),
            ModelsCommand::Update {
                url: None,
                public_key: None,
                force: false,
            }
        );
        assert_eq!(
            parse(&args(&[
                "update",
                "--url",
                "https://example.invalid/models.json",
                "--public-key",
                "abcd",
                "--force",
            ]))
            .expect("parse"),
            ModelsCommand::Update {
                url: Some("https://example.invalid/models.json".to_string()),
                public_key: Some("abcd".to_string()),
                force: true,
            }
        );
    }

    #[test]
    fn cwd_flags_do_not_become_actions() {
        assert_eq!(
            parse(&args(&["-C", "some/dir", "list"])).expect("parse"),
            ModelsCommand::List { json: false }
        );
    }

    #[test]
    fn missing_or_unknown_arguments_are_readable() {
        assert!(parse(&args(&["frobnicate"])).is_err());
        assert!(parse(&args(&["--wat"])).is_err());
        assert!(parse(&args(&["update", "--url"])).is_err());
        assert!(parse(&args(&["update", "--public-key"])).is_err());
        assert!(parse(&args(&["list", "extra"])).is_err());
    }

    #[test]
    fn verify_rejects_a_document_without_a_signature() {
        let error =
            verify_catalog(br#"{"models":[{"id":"a"}]}"#, &"00".repeat(32)).expect_err("must fail");
        assert!(error.to_string().contains("signature"), "{}", error);
    }
}
