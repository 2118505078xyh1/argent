//! `argent update check`：带节流与策略门控的版本检查。
//!
//! 与 `argent update` 的其它子命令（keygen / sign / download / apply /
//! rollback）不同，这里读的是设置里的发布清单地址与公钥，并遵守
//! `update.autoUpdate` 策略和 24 小时节流：
//!
//! - `--manifest <url|path>` 覆盖设置里的 `update.manifestUrl`（兼容扁平 `manifestUrl`）；
//! - `--public-key <hex|path>` 覆盖设置里的 `update.publicKey`（兼容扁平 `publicKey`）；
//! - `--force` 跳过 24 小时节流，并在 `autoUpdate == off` 时也照常执行；
//! - `--json` 时 stdout 只有一个 JSON 对象。
//!
//! 检查完把结果 `record_check` 到 `<storage>/update-state.json`。没到点且没有
//! `--force` 时打印提示并以退出码 0 返回；没配 URL / 没配公钥 / 没网络等失败
//! 一律可读报错 + 非 0 退出码，绝不 panic。

use argent_config::{Paths, Settings};
use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_update::{
    check_remote, last_check, now_epoch, record_check, should_check, AutoUpdate, UpdateCheck,
    DEFAULT_CHECK_INTERVAL_SECS,
};

/// 解析后的 `argent update check` 参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckArgs {
    /// 发布清单地址或本地路径；缺省取设置。
    pub manifest: Option<String>,
    /// 公钥（十六进制串或公钥文件路径）；缺省取设置。
    pub public_key: Option<String>,
    /// 跳过节流与 `autoUpdate == off` 的门控。
    pub force: bool,
    /// 只输出一个 JSON 对象。
    pub json: bool,
}

/// `argent update check` 的用法文本。
const USAGE: &str = "\
usage: argent update check [manifest|--manifest <url|path>] [--public-key <hex|path>] [--force] [--json]

Defaults come from the update.manifestUrl and update.publicKey settings (flat
keys manifestUrl / publicKey also work); CLI flags override them. A check within
the last 24 hours is skipped unless --force; update.autoUpdate == off also
requires --force. The result is recorded in <storage>/update-state.json.";

/// 解析 `argent update check ...` 的参数（`args` 含开头的 `check`）。
pub(crate) fn parse(args: &[String]) -> Result<CheckArgs> {
    let mut manifest: Option<String> = None;
    let mut public_key: Option<String> = None;
    let mut force = false;
    let mut json = false;
    // 除首个 `check` 之外的普通参数一律不接受，避免拼错被静默吞掉。
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--force" => force = true,
            "--json" => json = true,
            "--manifest" => {
                index += 1;
                manifest = Some(
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| Error::config("--manifest needs a value"))?,
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
                return Err(Error::config(format!("unknown update option '{}'", other)));
            }
            other => positional.push(other.to_string()),
        }
        index += 1;
    }

    // 首个普通参数必须是 `check`（其余 `update` 子命令走老的 commands::update）。
    match positional.first().map(String::as_str) {
        Some("check") | None => {}
        Some(other) => {
            return Err(Error::config(format!(
                "unknown update command '{}'; expected keygen, sign, check, download, apply, rollback",
                other
            )));
        }
    }
    // `check` 之后的位置参数是 manifest 路径：与 `update apply <manifest>`
    // 的用法保持一致，免得两个子命令一个要标志、一个要位置。
    if positional.len() > 1 {
        if manifest.is_some() {
            return Err(Error::config(
                "usage: argent update check [manifest|--manifest <url|path>] [--public-key <hex|path>] [--force] [--json]",
            ));
        }
        manifest = Some(positional[1].clone());
    }
    if positional.len() > 2 {
        return Err(Error::config(
            "usage: argent update check [manifest|--manifest <url|path>] [--public-key <hex|path>] [--force] [--json]",
        ));
    }

    Ok(CheckArgs {
        manifest,
        public_key,
        force,
        json,
    })
}

/// 执行 `argent update check ...`。
pub(crate) fn run(args: &[String]) -> Result<i32> {
    if crate::commands::wants_help(args) {
        println!("{}", USAGE);
        return Ok(0);
    }
    let parsed = parse(args)?;
    let cwd = crate::commands::cwd_from(args);
    let settings = Settings::load(&cwd)?;
    let paths = Paths::new(&cwd);
    let state_path = paths.storage().join("update-state.json");

    // 策略门控：autoUpdate == off 时除非 --force，否则拒绝执行并说明原因。
    let auto_update = AutoUpdate::from_settings(&settings);
    if auto_update == AutoUpdate::Off && !parsed.force {
        return Err(Error::config(
            "update.autoUpdate is off; refusing to check for updates \
(set update.autoUpdate to notify or apply, or pass --force)",
        ));
    }

    // 节流：距上次检查未满默认间隔（24 小时）时，除非 --force，直接成功返回。
    let last = last_check(&state_path);
    if !parsed.force && !should_check(last.as_ref(), now_epoch(), DEFAULT_CHECK_INTERVAL_SECS) {
        let hours = DEFAULT_CHECK_INTERVAL_SECS / 3600;
        println!(
            "距离上次检查未满 {} 小时（用 --force 跳过）",
            hours
        );
        return Ok(0);
    }

    let location = resolve_manifest(&settings, parsed.manifest).ok_or_else(|| {
        Error::config(
            "no update manifest configured; set \"update\": {\"manifestUrl\": \"https://…\"} \
(或扁平键 manifestUrl) in argent.json, or pass --manifest <url|path>",
        )
    })?;
    let public_key = resolve_public_key(&settings, parsed.public_key).ok_or_else(|| {
        Error::config(
            "no update public key configured; set \"update\": {\"publicKey\": \"<hex|path>\"} \
(或扁平键 publicKey) in argent.json, or pass --public-key <hex|path>",
        )
    })?;

    // 当前版本与 `--version` 同源。
    let check = check_remote(argent_core::VERSION, &location, &public_key)?;
    record_check(&state_path, &check)?;
    report(&check, parsed.json);
    Ok(0)
}

/// 打印检查结果：`--json` 时只有一个 JSON 对象。
fn report(check: &UpdateCheck, json: bool) {
    if json {
        println!("{}", check.to_json().dump_pretty());
        return;
    }
    println!("update check");
    println!(
        "  available {}",
        if check.available { "yes" } else { "no" }
    );
    println!("  current   {}", check.current);
    println!("  latest    {}", check.latest);
    if let Some(published) = &check.published_at {
        println!("  published {}", published);
    }
    if let Some(url) = &check.url {
        println!("  url       {}", url);
    }
    if let Some(notes) = &check.notes {
        println!("  notes     {}", notes);
    }
}

/// 清单地址来源：`--manifest` > `update.manifestUrl` > 扁平键。
fn resolve_manifest(settings: &Settings, cli: Option<String>) -> Option<String> {
    cli.filter(|value| !value.trim().is_empty())
        .or_else(|| nested_string(settings, "update", "manifestUrl"))
        .or_else(|| nested_string(settings, "update", "manifest_url"))
        .or_else(|| flat_string(settings, &["manifestUrl", "manifest_url"]))
}

/// 公钥来源：`--public-key` > `update.publicKey` > 扁平键 > `ARGENT_UPDATE_PUBLIC_KEY`。
fn resolve_public_key(settings: &Settings, cli: Option<String>) -> Option<String> {
    cli.filter(|value| !value.trim().is_empty())
        .or_else(|| nested_string(settings, "update", "publicKey"))
        .or_else(|| nested_string(settings, "update", "public_key"))
        .or_else(|| flat_string(settings, &["publicKey", "public_key"]))
        .or_else(|| {
            std::env::var("ARGENT_UPDATE_PUBLIC_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
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
    fn parses_flags() {
        assert_eq!(
            parse(&args(&["check"])).expect("parse"),
            CheckArgs {
                manifest: None,
                public_key: None,
                force: false,
                json: false,
            }
        );
        assert_eq!(
            parse(&args(&[
                "check",
                "--manifest",
                "https://example.invalid/manifest.json",
                "--public-key",
                "abc",
                "--force",
                "--json",
            ]))
            .expect("parse"),
            CheckArgs {
                manifest: Some("https://example.invalid/manifest.json".to_string()),
                public_key: Some("abc".to_string()),
                force: true,
                json: true,
            }
        );
    }

    #[test]
    fn cwd_flags_do_not_become_positional() {
        assert_eq!(
            parse(&args(&["check", "-C", "some/dir"])).expect("parse"),
            CheckArgs {
                manifest: None,
                public_key: None,
                force: false,
                json: false,
            }
        );
    }

    #[test]
    fn missing_values_and_unknown_flags_are_readable() {
        assert!(parse(&args(&["check", "--manifest"])).is_err());
        assert!(parse(&args(&["check", "--public-key"])).is_err());
        assert!(parse(&args(&["check", "--wat"])).is_err());
        // 别的 update 子命令不该落到这里。
        assert!(parse(&args(&["keygen"])).is_err());
        // 位置参数现在就是 manifest；与 --manifest 同时给才是错误。
        assert!(parse(&args(&["check", "extra", "--manifest", "m.json"])).is_err());
        assert!(parse(&args(&["check", "a", "b"])).is_err());
        assert_eq!(
            parse(&args(&["check", "m.json"]))
                .expect("positional manifest")
                .manifest
                .as_deref(),
            Some("m.json")
        );
    }

    fn settings_from(dir: &std::path::Path, body: &str) -> Settings {
        std::fs::create_dir_all(dir).expect("dir");
        std::fs::write(dir.join("argent.json"), body).expect("write");
        Settings::load(dir).expect("load")
    }

    #[test]
    fn manifest_and_key_resolution_prefers_cli_then_settings() {
        let dir = std::env::temp_dir().join(format!(
            "argent-update-cmd-{}-{}",
            std::process::id(),
            now_epoch()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // 嵌套键。
        let settings =
            settings_from(&dir, r#"{"update":{"manifestUrl":"https://n/x.json","publicKey":"nestedkey"}}"#);
        assert_eq!(
            resolve_manifest(&settings, None).as_deref(),
            Some("https://n/x.json")
        );
        assert_eq!(resolve_public_key(&settings, None).as_deref(), Some("nestedkey"));
        // CLI 覆盖设置。
        assert_eq!(
            resolve_manifest(&settings, Some("cli-url".to_string())).as_deref(),
            Some("cli-url")
        );

        // 扁平键。
        let settings = settings_from(&dir, r#"{"manifestUrl":"https://f/x.json","publicKey":"flatkey"}"#);
        assert_eq!(
            resolve_manifest(&settings, None).as_deref(),
            Some("https://f/x.json")
        );
        assert_eq!(resolve_public_key(&settings, None).as_deref(), Some("flatkey"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
