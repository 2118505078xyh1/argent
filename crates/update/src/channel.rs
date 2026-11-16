//! 自更新通道：何时检查、发现了什么、检查状态记在哪。
//!
//! 这一层不碰下载与替换二进制——那是 [`crate::install`] 的活。它只回答三个问题：
//!
//! 1. **要不要检查**：由 [`AutoUpdate`] 策略决定（关、只提示、自动应用），
//!    加上 [`should_check`] 的时间节流（默认 24 小时一次）。
//! 2. **有没有新版本**：[`check`] 校验 release manifest 的 Ed25519 签名，
//!    再做语义化版本比较。签名不过就直接返回 `Err`，绝不基于未签名的数据
//!    告诉用户“有新版本”。
//! 3. **上次检查怎么样**：[`record_check`]/[`last_check`] 把结果落到
//!    `<home>/.argent/update-state.json`（路径由调用方给出，库里不猜 home）。
//!
//! 版本比较的规则见 [`compare_versions`]。

use std::cmp::Ordering;
use std::path::Path;

use argent_config::Settings;
use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// 默认的检查间隔：24 小时。仅作为调用方没给间隔时的建议值。
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// 自动更新策略。
///
/// 名称与配置文件里的 `update.autoUpdate` 取值一致，便于原样读写。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoUpdate {
    /// 完全不检查更新。
    Off,
    /// 只检查并提示，不自动下载或替换（默认）。
    Notify,
    /// 检查到新版本后自动下载并替换运行中的二进制。
    Apply,
}

impl AutoUpdate {
    /// 解析一个取值。
    ///
    /// 大小写不敏感、会去掉首尾空白。识别不了的值返回带说明的 `Err`，
    /// 让需要严格校验的调用方（例如 CLI 参数、配置体检）能报错。
    pub fn parse(value: &str) -> Result<AutoUpdate> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" | "disabled" => Ok(AutoUpdate::Off),
            "notify" | "prompt" | "check" => Ok(AutoUpdate::Notify),
            "apply" | "auto" | "install" | "true" | "enabled" => Ok(AutoUpdate::Apply),
            other => Err(Error::config(format!(
                "unrecognized update.autoUpdate value '{}': expected off, notify or apply",
                other
            ))),
        }
    }

    /// 规范名称（写回配置时用的键值）。
    pub fn name(&self) -> &'static str {
        match self {
            AutoUpdate::Off => "off",
            AutoUpdate::Notify => "notify",
            AutoUpdate::Apply => "apply",
        }
    }

    /// 从设置里读策略，读 `update.autoUpdate`，兼容扁平的 `autoUpdate`。
    ///
    /// 默认 **[`AutoUpdate::Notify`]**：只提示、不自动替换，是破坏性最小的
    /// 行为。识别不了的值回落到 `Notify` 而不是报错，理由是：读取发生在
    /// 启动路径上，且 `from_settings` 不返回 `Result`——一个配置里的拼写
    /// 错误不该让程序起不来。需要严格报错时请直接调用 [`AutoUpdate::parse`]。
    pub fn from_settings(settings: &Settings) -> AutoUpdate {
        let raw = settings
            .get("update")
            .and_then(|update| {
                update
                    .get("autoUpdate")
                    .or_else(|| update.get("auto_update"))
            })
            .or_else(|| settings.get("autoUpdate"))
            .or_else(|| settings.get("auto_update"));
        match raw {
            Some(value) if value.as_bool() == Some(true) => AutoUpdate::Apply,
            Some(value) if value.as_bool() == Some(false) => AutoUpdate::Off,
            Some(value) => value
                .as_str()
                .map(|text| AutoUpdate::parse(text).unwrap_or(AutoUpdate::Notify))
                .unwrap_or(AutoUpdate::Notify),
            None => AutoUpdate::Notify,
        }
    }
}

/// 一次版本检查的结果，也是写进状态文件的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    /// 是否存在一个比当前更新的版本。
    pub available: bool,
    /// 检查时的当前版本。
    pub current: String,
    /// manifest 里声明的最新版本。
    pub latest: String,
    /// 发布包下载地址（manifest 里必有，取不到时为 `None`）。
    pub url: Option<String>,
    /// 发布说明，空则 `None`。
    pub notes: Option<String>,
    /// 发布时间，取自 manifest 的 `publishedAt`/`published_at`，空则 `None`。
    pub published_at: Option<String>,
    /// 这次检查发生的时刻（Unix 秒）。
    ///
    /// [`crate::check`] 会打上当前时间；[`should_check`] 靠它做节流。
    /// 0 表示未知。
    pub checked_at: u64,
}

impl UpdateCheck {
    /// 读一块 JSON（状态文件的根对象）。
    ///
    /// 缺少关键的 `current`/`latest` 会报错，好让半个文件被当作损坏处理。
    pub fn from_json(value: &Json) -> Result<UpdateCheck> {
        let current = value
            .get("current")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::config("update state is missing the 'current' version"))?
            .to_string();
        let latest = value
            .get("latest")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::config("update state is missing the 'latest' version"))?
            .to_string();
        let available = value
            .get("available")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let checked_at = value
            .get("checked_at")
            .and_then(Json::as_i64)
            .or_else(|| value.get("checkedAt").and_then(Json::as_i64))
            .map(|n| n.max(0) as u64)
            .unwrap_or(0);
        Ok(UpdateCheck {
            available,
            current,
            latest,
            url: opt_str(value.get("url")),
            notes: opt_str(value.get("notes")),
            published_at: opt_str(value.get("published_at"))
                .or_else(|| opt_str(value.get("publishedAt"))),
            checked_at,
        })
    }

    /// 序列化为状态文件的根对象。
    pub fn to_json(&self) -> Json {
        Json::from_pairs(vec![
            ("available", Json::bool(self.available)),
            ("current", Json::string(self.current.clone())),
            ("latest", Json::string(self.latest.clone())),
            ("url", opt_json(&self.url)),
            ("notes", opt_json(&self.notes)),
            ("published_at", opt_json(&self.published_at)),
            ("checked_at", Json::num(self.checked_at as f64)),
        ])
    }
}

/// 校验 manifest 签名并判断是否有新版本。
///
/// `manifest_bytes` 是 release manifest 的原始字节；`public_key_hex` 可以是
/// 32 字节公钥的十六进制串，也可以是公钥文件路径（复用
/// [`crate::parse_public_key`]）。签名校验失败或被篡改都返回 `Err`。
///
/// ``current_version`` 与 manifest 里的版本都必须是合法版本串，否则返回 `Err`。
pub fn check(current_version: &str, manifest_bytes: &[u8], public_key_hex: &str) -> Result<UpdateCheck> {
    let text = String::from_utf8_lossy(manifest_bytes);
    let value = Json::parse(&text)
        .map_err(|error| Error::config(format!("invalid update manifest: {}", error)))?;
    let manifest = crate::UpdateManifest::from_json(&value)?;
    let key = crate::parse_public_key(public_key_hex)?;
    // 先验签，再比较版本：绝不基于未签名的数据下结论。
    manifest.verify(&key)?;

    let available = compare_versions(&manifest.version, current_version)? == Ordering::Greater;
    let published_at = opt_str(value.get("publishedAt"))
        .or_else(|| opt_str(value.get("published_at")));
    Ok(UpdateCheck {
        available,
        current: current_version.to_string(),
        latest: manifest.version,
        url: non_empty(manifest.url),
        notes: non_empty(manifest.notes),
        published_at,
        checked_at: now_epoch(),
    })
}

/// 从 URL 或本地文件下载 manifest 字节再走 [`check`]。
///
/// 覆盖“下载 manifest 字节”这一段；下载本身复用 crate 内已有的 `fetch`
/// （本地路径直接读文件，http(s) 走 [`argent_core::http`]）。
pub fn check_remote(
    current_version: &str,
    location: &str,
    public_key_hex: &str,
) -> Result<UpdateCheck> {
    let bytes = crate::fetch(location)?;
    check(current_version, &bytes, public_key_hex)
}

/// 把一次检查结果写进状态文件。
///
/// 会创建父目录。写入失败返回 `Err`。
pub fn record_check(path: &Path, check: &UpdateCheck) -> Result<()> {
    argent_core::write_text_file(path, &check.to_json().dump_pretty())
}

/// 读回上次的检查结果。
///
/// 文件不存在、为空、损坏、或不是合法的状态对象，一律返回 `None`——状态是
/// 尽力而为的缓存，坏了就当没有，绝不 panic。
pub fn last_check(path: &Path) -> Option<UpdateCheck> {
    let text = std::fs::read_to_string(path).ok()?;
    let value = Json::parse(&text).ok()?;
    UpdateCheck::from_json(&value).ok()
}

/// 距上次检查是否已经过了 `interval_secs`。
///
/// - 没有历史记录 → `true`（还没检查过）。
/// - 刚好到点（`now == checked_at + interval`）→ `true`。
/// - 还没到点 → `false`。
///
/// `interval_secs` 传 [`DEFAULT_CHECK_INTERVAL_SECS`] 即 24 小时。
pub fn should_check(last: Option<&UpdateCheck>, now_epoch: u64, interval_secs: u64) -> bool {
    match last {
        None => true,
        Some(check) => now_epoch >= check.checked_at.saturating_add(interval_secs),
    }
}

/// 比较两个版本串。
///
/// 规则（自己实现，不引依赖）：
///
/// - 主版本由 `.` 分成 1~3 段数字（`major`/`minor`/`patch`），缺的段按 0
///   （即 `1.2` 等于 `1.2.0`）；段必须是纯数字，超过 3 段或空段都报错。
/// - `+` 之后是 build metadata，比较时**忽略**。
/// - `-` 之后是预发布段。**预发布小于同号正式版**（`1.2.3-rc1 < 1.2.3`）。
/// - 两个预发布比较：逐段比较，纯数字段按数值比、数字段小于字母段、
///   字母段按 ASCII 字典序；前缀全等时，段数少的更小
///   （`1.0.0-alpha < 1.0.0-alpha.1`）。
pub fn compare_versions(left: &str, right: &str) -> Result<Ordering> {
    Ok(parse_semver(left)?.cmp(&parse_semver(right)?))
}

/// 当前 Unix 时间戳（秒）。取不到（时钟早于 1970）时返回 0。
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 一个解析后的语义化版本。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    main: [u64; 3],
    /// 空表示正式版。
    pre: Vec<PreId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PreId {
    Num(u64),
    Text(String),
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.main
            .cmp(&other.main)
            .then_with(|| compare_pre(&self.pre, &other.pre))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_pre(left: &[PreId], right: &[PreId]) -> Ordering {
    match (left.is_empty(), right.is_empty()) {
        (true, true) => Ordering::Equal,
        // 正式版比任何预发布都大。
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            for (a, b) in left.iter().zip(right.iter()) {
                let ord = match (a, b) {
                    (PreId::Num(a), PreId::Num(b)) => a.cmp(b),
                    (PreId::Num(_), PreId::Text(_)) => Ordering::Less,
                    (PreId::Text(_), PreId::Num(_)) => Ordering::Greater,
                    (PreId::Text(a), PreId::Text(b)) => a.cmp(b),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            // 前缀相同：段数少的更小。
            left.len().cmp(&right.len())
        }
    }
}

fn parse_semver(input: &str) -> Result<Version> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::config("version string is empty"));
    }
    // build metadata（+ 之后）不参与比较，直接丢掉。
    let without_build = input.split('+').next().unwrap_or(input);
    let (core, pre_text) = match without_build.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (without_build, None),
    };

    let parts: Vec<&str> = core.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(Error::config(format!(
            "version '{}' must have one to three numeric components",
            input
        )));
    }
    let mut main = [0u64; 3];
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::config(format!(
                "version '{}' has an invalid component '{}'",
                input, part
            )));
        }
        main[index] = part.parse::<u64>().map_err(|_| {
            Error::config(format!("version '{}' component '{}' is out of range", input, part))
        })?;
    }

    let pre = match pre_text {
        None => Vec::new(),
        Some(text) => {
            if text.is_empty() {
                return Err(Error::config(format!(
                    "version '{}' has an empty pre-release",
                    input
                )));
            }
            let mut ids = Vec::new();
            for id in text.split('.') {
                if id.is_empty() {
                    return Err(Error::config(format!(
                        "version '{}' has an empty pre-release identifier",
                        input
                    )));
                }
                if id.bytes().all(|b| b.is_ascii_digit()) {
                    let number = id.parse::<u64>().map_err(|_| {
                        Error::config(format!(
                            "version '{}' pre-release '{}' is out of range",
                            input, id
                        ))
                    })?;
                    ids.push(PreId::Num(number));
                } else {
                    ids.push(PreId::Text(id.to_string()));
                }
            }
            ids
        }
    };

    Ok(Version { main, pre })
}

fn opt_str(value: Option<&Json>) -> Option<String> {
    value
        .and_then(Json::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn opt_json(value: &Option<String>) -> Json {
    match value {
        Some(text) => Json::string(text.clone()),
        None => Json::Null,
    }
}

fn non_empty(value: String) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::path::PathBuf;
    use std::net::TcpListener;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "argent-update-channel-{}-{}-{}",
            name,
            std::process::id(),
            now_epoch()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// 造一个已签名的 manifest 字节串，返回 (bytes, public_key_hex)。
    fn signed_manifest(dir: &Path, version: &str) -> (Vec<u8>, String) {
        let key_path = crate::keygen(dir).expect("keygen");
        let public = crate::public_key_hex(&key_path).expect("public key");
        let unsigned = dir.join("unsigned.json");
        let mut value = Json::obj();
        value.set("version", Json::string(version));
        value.set("channel", Json::string("stable"));
        value.set("url", Json::string("http://127.0.0.1:1/argent.zip"));
        value.set("sha256", Json::string("deadbeef"));
        value.set("notes", Json::string("release notes"));
        value.set("signature", Json::string(""));
        argent_core::write_text_file(&unsigned, &value.dump()).expect("write unsigned");
        let signed = crate::sign(&unsigned, &key_path, None).expect("sign");
        (signed.to_json(true).dump().into_bytes(), public)
    }

    // ---- 版本比较 ----

    #[test]
    fn version_comparison_orders_numerically() {
        assert_eq!(compare_versions("1.2.3", "1.2.10").unwrap(), Ordering::Less);
        assert_eq!(compare_versions("1.0.0", "1.0.1").unwrap(), Ordering::Less);
        assert_eq!(compare_versions("2.0.0", "1.9.9").unwrap(), Ordering::Greater);
        // 缺段按 0 补齐：1.2 等于 1.2.0。
        assert_eq!(compare_versions("1.2", "1.2.0").unwrap(), Ordering::Equal);
    }

    #[test]
    fn version_comparison_treats_prerelease_as_smaller() {
        assert_eq!(compare_versions("1.2.3-rc1", "1.2.3").unwrap(), Ordering::Less);
        assert_eq!(
            compare_versions("1.2.3", "1.2.3-rc1").unwrap(),
            Ordering::Greater
        );
        // 预发布之间按段比较。
        assert_eq!(
            compare_versions("1.0.0-alpha", "1.0.0-alpha.1").unwrap(),
            Ordering::Less
        );
        assert_eq!(
            compare_versions("1.0.0-alpha.1", "1.0.0-alpha.beta").unwrap(),
            Ordering::Less
        );
        // build metadata 被忽略。
        assert_eq!(
            compare_versions("1.0.0+build.1", "1.0.0+build.2").unwrap(),
            Ordering::Equal
        );
    }

    #[test]
    fn version_comparison_equality() {
        assert_eq!(compare_versions("1.2.3", "1.2.3").unwrap(), Ordering::Equal);
        assert_eq!(compare_versions("1.2.3", "1.2.3").unwrap(), Ordering::Equal);
    }

    #[test]
    fn version_comparison_rejects_invalid_strings() {
        for bad in ["", "abc", "1.a.3", "1.2.3.4", "1..3", "1.2.-1", "1.2.3-", "1.2.3-rc."] {
            assert!(
                compare_versions(bad, "1.0.0").is_err(),
                "'{}' should be rejected",
                bad
            );
            assert!(
                compare_versions("1.0.0", bad).is_err(),
                "'{}' should be rejected on the right",
                bad
            );
        }
    }

    // ---- check ----

    #[test]
    fn check_reports_a_newer_release() {
        let dir = scratch("newer");
        let (bytes, public) = signed_manifest(&dir, "0.2.0");
        let result = check("0.1.0", &bytes, &public).expect("check");
        assert!(result.available);
        assert_eq!(result.current, "0.1.0");
        assert_eq!(result.latest, "0.2.0");
        assert_eq!(result.url.as_deref(), Some("http://127.0.0.1:1/argent.zip"));
        assert_eq!(result.notes.as_deref(), Some("release notes"));
        assert!(result.checked_at > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_reports_up_to_date() {
        let dir = scratch("uptodate");
        let (bytes, public) = signed_manifest(&dir, "0.1.0");
        let result = check("0.1.0", &bytes, &public).expect("check");
        assert!(!result.available);
        let older = check("0.3.0", &bytes, &public).expect("check");
        assert!(!older.available, "an older manifest is not an update");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_rejects_a_tampered_manifest() {
        let dir = scratch("tamper");
        let (bytes, public) = signed_manifest(&dir, "0.2.0");
        let mut value = Json::parse(&String::from_utf8_lossy(&bytes)).expect("parse");
        value.set("version", Json::string("9.9.9"));
        let tampered = value.dump().into_bytes();
        assert!(check("0.1.0", &tampered, &public).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_rejects_a_wrong_public_key() {
        let dir = scratch("wrongkey");
        let (bytes, _public) = signed_manifest(&dir, "0.2.0");
        let other = crate::keygen(&scratch("wrongkey-other")).expect("keygen");
        let other_public = crate::public_key_hex(&other).expect("public");
        assert!(check("0.1.0", &bytes, &other_public).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 状态文件 ----

    #[test]
    fn state_file_round_trips() {
        let dir = scratch("roundtrip");
        let path = dir.join("update-state.json");
        let original = UpdateCheck {
            available: true,
            current: "0.1.0".to_string(),
            latest: "0.2.0".to_string(),
            url: Some("https://example.invalid/argent.zip".to_string()),
            notes: Some("notes".to_string()),
            published_at: Some("2026-01-01".to_string()),
            checked_at: 1234,
        };
        record_check(&path, &original).expect("record");
        let read = last_check(&path).expect("read back");
        assert_eq!(read, original);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_broken_state_reads_as_none() {
        let dir = scratch("broken");
        let path = dir.join("update-state.json");

        // 不存在。
        assert_eq!(last_check(&path), None);

        // 空文件。
        std::fs::write(&path, "").unwrap();
        assert_eq!(last_check(&path), None);

        // 损坏的 JSON。
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(last_check(&path), None);

        // 半个文件：只有 current，缺 latest。
        std::fs::write(&path, r#"{"current":"0.1.0"}"#).unwrap();
        assert_eq!(last_check(&path), None);

        // 不是对象。
        std::fs::write(&path, "[1,2,3]").unwrap();
        assert_eq!(last_check(&path), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 节流 ----

    #[test]
    fn should_check_throttles_by_interval() {
        // 没有历史记录：该检查。
        assert!(should_check(None, 10_000, 3_600));

        let last = UpdateCheck {
            available: false,
            current: "0.1.0".to_string(),
            latest: "0.1.0".to_string(),
            url: None,
            notes: None,
            published_at: None,
            checked_at: 1_000,
        };

        // 还没到点。
        assert!(!should_check(Some(&last), 1_000 + 3_599, 3_600));
        // 刚好到点。
        assert!(should_check(Some(&last), 1_000 + 3_600, 3_600));
        // 过了点。
        assert!(should_check(Some(&last), 1_000 + 10_000, 3_600));
        // 间隔为 0：立刻可以再检查。
        assert!(should_check(Some(&last), 1_000, 0));
    }

    // ---- 策略 ----

    #[test]
    fn auto_update_parses_known_names() {
        assert_eq!(AutoUpdate::parse("off").unwrap(), AutoUpdate::Off);
        assert_eq!(AutoUpdate::parse("OFF").unwrap(), AutoUpdate::Off);
        assert_eq!(AutoUpdate::parse(" notify ").unwrap(), AutoUpdate::Notify);
        assert_eq!(AutoUpdate::parse("apply").unwrap(), AutoUpdate::Apply);
        assert!(AutoUpdate::parse("whatever").is_err());

        assert_eq!(AutoUpdate::Off.name(), "off");
        assert_eq!(AutoUpdate::Notify.name(), "notify");
        assert_eq!(AutoUpdate::Apply.name(), "apply");
    }

    #[test]
    fn auto_update_reads_settings_and_falls_back_to_notify() {
        let storage = scratch("settings-storage");
        let cwd = scratch("settings-cwd");
        let old = std::env::var_os(argent_config::STORAGE_ENV);
        std::env::set_var(argent_config::STORAGE_ENV, &storage);

        // 嵌套 update.autoUpdate。
        std::fs::write(cwd.join("argent.json"), r#"{"update":{"autoUpdate":"apply"}}"#).unwrap();
        assert_eq!(
            AutoUpdate::from_settings(&Settings::load(&cwd).unwrap()),
            AutoUpdate::Apply
        );

        // 扁平 autoUpdate。
        std::fs::write(cwd.join("argent.json"), r#"{"autoUpdate":"off"}"#).unwrap();
        assert_eq!(
            AutoUpdate::from_settings(&Settings::load(&cwd).unwrap()),
            AutoUpdate::Off
        );

        // 无法识别的值回落到 Notify。
        std::fs::write(cwd.join("argent.json"), r#"{"update":{"autoUpdate":"nonsense"}}"#).unwrap();
        assert_eq!(
            AutoUpdate::from_settings(&Settings::load(&cwd).unwrap()),
            AutoUpdate::Notify
        );

        // 没配置时默认 Notify。
        std::fs::write(cwd.join("argent.json"), r#"{}"#).unwrap();
        assert_eq!(
            AutoUpdate::from_settings(&Settings::load(&cwd).unwrap()),
            AutoUpdate::Notify
        );

        match old {
            Some(value) => std::env::set_var(argent_config::STORAGE_ENV, value),
            None => std::env::remove_var(argent_config::STORAGE_ENV),
        }
        let _ = std::fs::remove_dir_all(&storage);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // ---- 下载段（本地假服务端）----

    #[test]
    fn check_remote_fetches_manifest_over_http() {
        let dir = scratch("remote");
        let (bytes, public) = signed_manifest(&dir, "0.2.0");
        let body = String::from_utf8(bytes).expect("utf8 manifest");

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            // 读掉请求头。
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).expect("read") == 0 {
                    break;
                }
                if line == "\r\n" {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).expect("write");
            std::thread::sleep(std::time::Duration::from_millis(50));
        });

        let url = format!("http://{}/manifest.json", addr);
        let result = check_remote("0.1.0", &url, &public).expect("check_remote");
        assert!(result.available);
        assert_eq!(result.latest, "0.2.0");
        assert_eq!(result.notes.as_deref(), Some("release notes"));

        let _ = server.join();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
