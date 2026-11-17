//! 模型目录热更新：解析、缓存、合并与远端刷新。
//!
//! 模型能力过去写死在 [`crate::ProviderCatalog`] 的 `providers.json` 里，更新
//! 一次就得重装。这里把模型目录拆成三层，按模型 id 合并：
//!
//! 1. 内置目录——从 `providers.json` 展平出的所有模型条目；
//! 2. 本地缓存——`<home>/.argent/models.json`；
//! 3. 配置里的显式覆盖——配置文档 `models` 键下的条目（若存在）。
//!
//! 优先级依次升高：缓存里同 id 的条目覆盖内置，配置里的再覆盖缓存，新 id 一律
//! 追加。合并结果顺序稳定（id 首次出现的位置保持不变），方便断言。
//!
//! 解析、合并、刷新都写成纯函数；只有读写缓存与发请求会碰到外部世界。

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::{ModelSpec, ProviderCatalog};

/// 目录文档中承载模型条目数组的键名。
pub const MODELS_KEY: &str = "models";

/// 一份模型目录：按 id 去重、顺序稳定的模型条目集合。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCatalog {
    entries: Vec<ModelSpec>,
}

impl ModelCatalog {
    /// 空目录。
    pub fn new() -> ModelCatalog {
        ModelCatalog {
            entries: Vec::new(),
        }
    }

    /// 内置目录：把 `providers.json` 展平成一维模型列表。
    ///
    /// 展平顺序即 `providers.json` 中 provider 与模型的出现顺序；同 id 的后续
    /// 条目覆盖前面的（例如多个 provider 都暴露 `GLM-5.3`），位置保持在首次
    /// 出现处，因此结果确定。
    pub fn builtin() -> ModelCatalog {
        ModelCatalog::from_providers(&ProviderCatalog::builtin())
    }

    /// 从一份已解析的 provider 目录展平出模型目录。
    pub fn from_providers(catalog: &ProviderCatalog) -> ModelCatalog {
        let mut out = ModelCatalog::new();
        for provider in catalog.list() {
            for model in &provider.models {
                out.insert(model.clone());
            }
        }
        out
    }

    /// 解析一份目录文本。
    ///
    /// 期望顶层是对象、`models` 键下是模型条目数组。文本无法解析、顶层不是
    /// 对象、或 `models` 不是数组时返回空目录（视作“没有这份目录”），绝不
    /// panic。单条条目里未知字段被忽略，缺字段用保守默认。
    pub fn parse(text: &str) -> ModelCatalog {
        match Json::parse(text) {
            Ok(value) => ModelCatalog::from_json(&value),
            Err(_) => ModelCatalog::new(),
        }
    }

    /// 从 JSON 值构建目录；顶层不是对象或 `models` 缺失/非数组时返回空目录。
    pub fn from_json(value: &Json) -> ModelCatalog {
        let mut out = ModelCatalog::new();
        let items = match value.get(MODELS_KEY).and_then(Json::as_array) {
            Some(items) => items,
            None => return out,
        };
        for item in items {
            if let Some(model) = ModelSpec::from_json(item) {
                out.insert(model);
            }
        }
        out
    }

    /// 按 id 取一条模型。
    pub fn get(&self, id: &str) -> Option<&ModelSpec> {
        self.entries.iter().find(|model| model.id == id)
    }

    /// 全部条目，按稳定顺序。
    pub fn list(&self) -> &[ModelSpec] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 插入或替换一条模型。同 id 已存在则原位覆盖（保留首次出现的位置），
    /// 否则追加到末尾。
    pub fn insert(&mut self, model: ModelSpec) {
        if let Some(slot) = self.entries.iter_mut().find(|m| m.id == model.id) {
            *slot = model;
        } else {
            self.entries.push(model);
        }
    }

    /// 合并另一份目录：`overlay` 中同 id 的条目覆盖当前条目，新 id 追加。
    /// 顺序稳定，可反复合并。
    pub fn merge(&mut self, overlay: &ModelCatalog) {
        for model in &overlay.entries {
            self.insert(model.clone());
        }
    }

    /// 序列化成 JSON 对象，供写入缓存。字段与解析时一致，便于往返。
    pub fn to_json(&self) -> Json {
        let mut value = Json::obj();
        value.set("schemaVersion", Json::num(1.0));
        value.set(
            MODELS_KEY,
            Json::Arr(self.entries.iter().map(ModelSpec::to_json).collect()),
        );
        value
    }
}

/// 模型缓存的默认路径：`<home>/.argent/models.json`。
///
/// 与仓库其它 crate 的写法一致：优先 `HOME`，退回 `USERPROFILE`，都取不到时
/// 用当前目录。
pub fn cache_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".argent").join("models.json")
}

/// 从默认路径读取本地缓存；读不到可用目录时返回 `None`。
pub fn load_cache() -> Option<ModelCatalog> {
    load_cache_at(&cache_path())
}

/// 从指定路径读取本地缓存。
///
/// 文件不存在、读失败、内容不是合法 JSON、顶层不是对象、或 `models` 不是
/// 数组——一律返回 `None`，视作“没有缓存”。不 panic，也不用 `unwrap`。
pub fn load_cache_at(path: &Path) -> Option<ModelCatalog> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let value = Json::parse(&text).ok()?;
    // 顶层必须是对象且带模型数组，否则这份文件不像目录。
    value.get(MODELS_KEY).and_then(Json::as_array)?;
    Some(ModelCatalog::from_json(&value))
}

/// 把目录写入默认缓存路径。
pub fn save_cache(catalog: &ModelCatalog) -> Result<()> {
    save_cache_at(&cache_path(), catalog)
}

/// 把目录写入指定路径，自动创建父目录。
pub fn save_cache_at(path: &Path, catalog: &ModelCatalog) -> Result<()> {
    argent_core::write_text_file(path, &catalog.to_json().dump_pretty())
}

/// 三层合并：内置目录 < 本地缓存 < 配置覆盖。
///
/// `cache` 为 `None` 表示没有可用缓存；`overrides` 为 `None` 表示配置里没有
/// 显式覆盖。返回顺序稳定的目录，纯函数。
pub fn resolve(cache: Option<&ModelCatalog>, overrides: Option<&ModelCatalog>) -> ModelCatalog {
    let mut catalog = ModelCatalog::builtin();
    if let Some(cache) = cache {
        catalog.merge(cache);
    }
    if let Some(overrides) = overrides {
        catalog.merge(overrides);
    }
    catalog
}

/// 便捷入口：读取默认缓存，并按需合并配置文档里的显式覆盖。
///
/// `overrides` 是一份配置 JSON（取其 `models` 键）；传 `None` 表示没有覆盖。
pub fn load(overrides: Option<&Json>) -> ModelCatalog {
    let cache = load_cache();
    let override_catalog = overrides.map(ModelCatalog::from_json);
    resolve(cache.as_ref(), override_catalog.as_ref())
}

/// 从 `url` 拉取最新的模型目录**原始字节**。
///
/// `verify` 是可选的签名/完整性校验闭包：传入时，只有校验通过才返回字节，
/// 校验失败则整体拒绝（错误原样向上传播）。刻意把它做成参数而不是固定依赖，
/// 以免 provider 依赖 update/plugins，保持依赖图无环。HTTP 非 2xx（例如 404）
/// 返回 `Err` 而不是 panic。
pub fn refresh(url: &str, verify: Option<&dyn Fn(&[u8]) -> Result<()>>) -> Result<Vec<u8>> {
    let response = argent_core::http::send(&argent_core::http::Request::get(url))?;
    if !response.is_success() {
        return Err(Error::io(format!(
            "refresh {} returned HTTP {}",
            url, response.status
        )));
    }
    let body = response.body;
    if let Some(verify) = verify {
        verify(&body)?;
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    /// 一个临时文件路径，每项测试用不同名字避免并发冲突。
    fn scratch_path(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("argent-models-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir.join("models.json")
    }

    /// 起一个只应答一次的假 HTTP 服务，返回 (url, 线程句柄)。
    fn serve_once(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // 读掉请求头，直到空行。
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) => {
                            if line == "\r\n" || line == "\n" {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    reason,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (format!("http://127.0.0.1:{}/models.json", port), handle)
    }

    #[test]
    fn parses_a_normal_document() {
        let catalog = ModelCatalog::parse(
            r#"{"schemaVersion":1,"models":[
                {"id":"a","name":"Alpha","context":1000,"maxOutput":100,"tools":true,"reasoning":true,"vision":false},
                {"id":"b","name":"Beta","context":2000,"maxOutput":200,"tools":false,"reasoning":false}
            ]}"#,
        );
        assert_eq!(catalog.len(), 2);
        let a = catalog.get("a").expect("a");
        assert_eq!(a.name, "Alpha");
        assert_eq!(a.context, 1000);
        assert_eq!(a.max_output, 100);
        assert!(a.tools);
        assert!(a.reasoning);
        assert!(!a.vision);
        let b = catalog.get("b").expect("b");
        assert!(!b.tools);
        assert!(!b.reasoning);
        // vision 缺字段时取保守默认（与 argent-ai 一致，按 true 处理）。
        assert!(b.vision);
    }

    #[test]
    fn ignores_unknown_fields() {
        let catalog = ModelCatalog::parse(
            r#"{"models":[{"id":"a","context":10,"futureField":"ignored","nested":{"x":1}}]}"#,
        );
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.get("a").unwrap().context, 10);
    }

    #[test]
    fn missing_fields_get_conservative_defaults() {
        let catalog = ModelCatalog::parse(r#"{"models":[{"id":"bare"}]}"#);
        let model = catalog.get("bare").expect("bare");
        assert_eq!(model.name, "bare");
        assert_eq!(model.context, 0);
        assert_eq!(model.max_output, 0);
        assert!(model.tools);
        assert!(!model.reasoning);
        assert!(model.vision);
    }

    #[test]
    fn wrong_typed_fields_fall_back_without_panicking() {
        let catalog = ModelCatalog::parse(
            r#"{"models":[
                {"id":"a","name":42,"context":"big","maxOutput":null,"tools":"yes","reasoning":1,"vision":[]},
                {"id":7,"name":"no string id"},
                {"name":"id missing entirely"}
            ]}"#,
        );
        // 只有带字符串 id 的条目被收下。
        assert_eq!(catalog.len(), 1);
        let a = catalog.get("a").expect("a");
        assert_eq!(a.name, "a"); // name 类型不对，退回 id
        assert_eq!(a.context, 0); // 类型不对按缺字段的保守默认
        assert_eq!(a.max_output, 0);
        assert!(a.tools); // 类型不对，退回该字段的默认
        assert!(!a.reasoning);
        assert!(a.vision);
    }

    #[test]
    fn merge_overrides_same_id_and_appends_new() {
        let mut base = ModelCatalog::parse(r#"{"models":[{"id":"a","context":1},{"id":"b"}]}"#);
        let overlay = ModelCatalog::parse(r#"{"models":[{"id":"a","context":9},{"id":"c"}]}"#);
        base.merge(&overlay);
        assert_eq!(base.len(), 3);
        assert_eq!(base.get("a").unwrap().context, 9); // 同 id 覆盖
        assert_eq!(
            base.list()
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "c"] // 覆盖不挪位，新 id 追加
        );
    }

    #[test]
    fn merge_order_is_deterministic() {
        let cache = ModelCatalog::parse(r#"{"models":[{"id":"a","context":2},{"id":"z"}]}"#);
        let overrides = ModelCatalog::parse(r#"{"models":[{"id":"z","context":3}]}"#);
        let first = resolve(Some(&cache), Some(&overrides));
        let second = resolve(Some(&cache), Some(&overrides));
        assert_eq!(first, second);
        // 合并结果以内置目录开头，缓存/覆盖的新 id 追加在末尾，顺序稳定。
        let ids: Vec<&str> = first.list().iter().map(|m| m.id.as_str()).collect();
        assert!(ids.ends_with(&["a", "z"]));
        assert_eq!(first.get("z").unwrap().context, 3); // 配置覆盖高于缓存
    }

    #[test]
    fn builtin_flatten_is_stable_and_dedupes() {
        let catalog = ModelCatalog::builtin();
        // 内置目录里 GLM-5.3 出现在 zai 与 bigmodel 两家，展平后只保留一条。
        let glm: Vec<_> = catalog
            .list()
            .iter()
            .filter(|m| m.id == "GLM-5.3")
            .collect();
        assert_eq!(glm.len(), 1);
        // 展平结果可重复。
        assert_eq!(catalog, ModelCatalog::builtin());
    }

    #[test]
    fn an_empty_file_has_no_cache() {
        let path = scratch_path("empty");
        std::fs::write(&path, b"").unwrap();
        assert!(load_cache_at(&path).is_none());
    }

    #[test]
    fn truncated_json_has_no_cache() {
        let path = scratch_path("truncated");
        std::fs::write(&path, r#"{"models":[{"id":"a""#).unwrap();
        assert!(load_cache_at(&path).is_none());
    }

    #[test]
    fn a_top_level_array_has_no_cache() {
        let path = scratch_path("array");
        std::fs::write(&path, r#"[{"id":"a"}]"#).unwrap();
        assert!(load_cache_at(&path).is_none());
    }

    #[test]
    fn a_missing_file_has_no_cache() {
        let path = scratch_path("missing");
        assert!(load_cache_at(&path).is_none());
    }

    #[test]
    fn a_wrong_typed_models_field_has_no_cache() {
        let path = scratch_path("wrongtype");
        std::fs::write(&path, r#"{"models":"not an array"}"#).unwrap();
        assert!(load_cache_at(&path).is_none());
    }

    #[test]
    fn a_valid_cache_round_trips() {
        let path = scratch_path("valid");
        let catalog = ModelCatalog::parse(r#"{"models":[{"id":"x","context":123,"tools":false}]}"#);
        save_cache_at(&path, &catalog).expect("save");
        let loaded = load_cache_at(&path).expect("load");
        assert_eq!(loaded, catalog);
        assert_eq!(loaded.get("x").unwrap().context, 123);
        assert!(!loaded.get("x").unwrap().tools);
    }

    #[test]
    fn refresh_fetches_bytes_from_a_local_server() {
        let body = r#"{"models":[{"id":"remote","context":4096}]}"#;
        let (url, handle) = serve_once(200, body);
        let bytes = refresh(&url, None).expect("refresh");
        handle.join().unwrap();
        assert_eq!(&bytes, body.as_bytes());
        // 拉到的字节可以正常解析成目录。
        let catalog = ModelCatalog::parse(&String::from_utf8_lossy(&bytes));
        assert_eq!(catalog.get("remote").unwrap().context, 4096);
    }

    #[test]
    fn refresh_returns_err_on_http_404() {
        let (url, handle) = serve_once(404, "not found");
        let result = refresh(&url, None);
        handle.join().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn refresh_rejects_when_verify_fails() {
        let (url, handle) = serve_once(200, r#"{"models":[]}"#);
        let verify = |_bytes: &[u8]| Err(Error::config("signature does not verify"));
        let result = refresh(&url, Some(&verify));
        handle.join().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn refresh_accepts_when_verify_passes() {
        let (url, handle) = serve_once(200, r#"{"models":[]}"#);
        let verify = |_bytes: &[u8]| Ok(());
        let result = refresh(&url, Some(&verify)).expect("refresh");
        handle.join().unwrap();
        assert_eq!(result, b"{\"models\":[]}");
    }
}
