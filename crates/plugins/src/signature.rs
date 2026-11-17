//! 插件签名校验。
//!
//! 插件目录里与清单同级的 `plugin.sig` 保存清单文件**原始字节**的 Ed25519
//! 分离签名（十六进制）。这样清单改动一个字节签名就会失效，签名与清单的
//! 归属关系不依赖清单里的字段。
//!
//! 校验级别由设置 `pluginSignature` 决定：
//!
//! * `required` —— 缺少签名或签名无效时拒绝安装；
//! * `warn` —— 只提示，不阻止安装；
//! * `off` —— 完全跳过校验（默认）。
//!
//! 公钥来自设置 `pluginPublicKey`（也接受 `pluginSignature.publicKey`），
//! 没有配置时回落到环境变量 `ARGENT_PLUGIN_PUBLIC_KEY`。

use std::path::{Path, PathBuf};

use argent_config::Settings;
use argent_core::json::Json;

use crate::manifest::find_manifest;

/// 签名校验级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SignatureMode {
    /// 缺少或签名无效时拒绝安装。
    Required,
    /// 缺少或签名无效时只提示。
    Warn,
    /// 跳过校验。
    #[default]
    Off,
}

impl SignatureMode {
    pub fn parse(value: &str) -> SignatureMode {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" | "require" | "strict" => SignatureMode::Required,
            "warn" | "warning" => SignatureMode::Warn,
            _ => SignatureMode::Off,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            SignatureMode::Required => "required",
            SignatureMode::Warn => "warn",
            SignatureMode::Off => "off",
        }
    }
}

/// 签名策略：校验级别 + 公钥。
#[derive(Debug, Clone, Default)]
pub struct SignaturePolicy {
    pub mode: SignatureMode,
    /// 公钥的十六进制串，或公钥文件路径。
    pub public_key: Option<String>,
}

impl SignaturePolicy {
    pub fn new(mode: SignatureMode) -> SignaturePolicy {
        SignaturePolicy {
            mode,
            public_key: None,
        }
    }

    /// 从合并后的设置中读取策略。
    pub fn from_settings(settings: &Settings) -> SignaturePolicy {
        let mut mode = SignatureMode::default();
        let mut public_key = settings
            .string("pluginPublicKey")
            .map(str::to_string)
            .filter(|value| !value.trim().is_empty());
        if let Some(value) = settings.get("pluginSignature") {
            match value {
                Json::Str(text) => mode = SignatureMode::parse(text),
                Json::Obj(_) => {
                    if let Some(text) = value.get("mode").and_then(Json::as_str) {
                        mode = SignatureMode::parse(text);
                    }
                    if let Some(text) = value.get("publicKey").and_then(Json::as_str) {
                        if !text.trim().is_empty() {
                            public_key = Some(text.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        if public_key.is_none() {
            public_key = std::env::var("ARGENT_PLUGIN_PUBLIC_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty());
        }
        SignaturePolicy { mode, public_key }
    }
}

/// 一次校验的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureStatus {
    /// 签名有效。
    Verified,
    /// 校验被关闭。
    Skipped,
    /// 没有签名文件。
    Missing,
    /// 签名文件存在但无法校验。
    Invalid(String),
}

impl SignatureStatus {
    pub fn is_verified(&self) -> bool {
        matches!(self, SignatureStatus::Verified | SignatureStatus::Skipped)
    }

    /// 一句话说明，供提示与错误信息使用。
    pub fn describe(&self) -> String {
        match self {
            SignatureStatus::Verified => "签名校验通过".to_string(),
            SignatureStatus::Skipped => "已跳过签名校验".to_string(),
            SignatureStatus::Missing => "缺少签名文件".to_string(),
            SignatureStatus::Invalid(reason) => format!("签名无效：{}", reason),
        }
    }
}

/// 签名文件与清单文件同名，只换扩展名。
pub fn signature_path(manifest_path: &Path) -> PathBuf {
    manifest_path.with_extension("sig")
}

/// 校验插件目录的签名。
pub fn verify(root: &Path, policy: &SignaturePolicy) -> SignatureStatus {
    if policy.mode == SignatureMode::Off {
        return SignatureStatus::Skipped;
    }
    let Some(manifest_path) = find_manifest(root) else {
        return SignatureStatus::Invalid("找不到插件清单".to_string());
    };
    let signature_path = signature_path(&manifest_path);
    if !signature_path.is_file() {
        return SignatureStatus::Missing;
    }
    let Some(public_key) = policy.public_key.as_deref() else {
        return SignatureStatus::Invalid("没有配置公钥（pluginPublicKey）".to_string());
    };
    let bytes = match std::fs::read(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return SignatureStatus::Invalid(format!("读取清单失败：{}", error));
        }
    };
    let signature = match std::fs::read_to_string(&signature_path) {
        Ok(text) => text,
        Err(error) => {
            return SignatureStatus::Invalid(format!("读取签名失败：{}", error));
        }
    };
    match argent_update::verify_detached(&bytes, signature.trim(), public_key) {
        Ok(()) => SignatureStatus::Verified,
        Err(error) => SignatureStatus::Invalid(error.to_string()),
    }
}

/// 为插件目录的清单生成签名文件。
///
/// 返回 `(签名文件路径, 十六进制公钥)`。公钥用于写入 `pluginPublicKey`。
pub fn sign(root: &Path, key_path: &Path) -> Result<(PathBuf, String), String> {
    let manifest_path =
        find_manifest(root).ok_or_else(|| format!("{} 里找不到插件清单", root.display()))?;
    let bytes = std::fs::read(&manifest_path)
        .map_err(|error| format!("读取 {} 失败：{}", manifest_path.display(), error))?;
    let signature =
        argent_update::sign_detached(&bytes, key_path).map_err(|error| error.to_string())?;
    let public_key = argent_update::public_key_hex(key_path).map_err(|error| error.to_string())?;
    let destination = signature_path(&manifest_path);
    argent_core::write_text_file(&destination, &format!("{}\n", signature))
        .map_err(|error| format!("写入 {} 失败：{}", destination.display(), error))?;
    Ok((destination, public_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "argent-plugin-sign-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn write_plugin(root: &Path, name: &str, body: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join(".zcode-plugin")).unwrap();
        std::fs::write(
            dir.join(".zcode-plugin").join("plugin.json"),
            format!(r#"{{"name":"{}",{}}}"#, name, body),
        )
        .unwrap();
        dir
    }

    fn key_pair(dir: &Path) -> (PathBuf, String) {
        let key_path = argent_update::keygen(dir).expect("keygen");
        let public = argent_update::public_key_hex(&key_path).expect("public");
        (key_path, public)
    }

    #[test]
    fn an_unsigned_plugin_is_missing_under_the_required_policy() {
        let root = scratch("missing");
        let plugin = write_plugin(&root, "alpha", r#""description":"a""#);
        let policy = SignaturePolicy {
            mode: SignatureMode::Required,
            public_key: Some("00".repeat(32)),
        };
        assert_eq!(verify(&plugin, &policy), SignatureStatus::Missing);
    }

    #[test]
    fn a_signed_plugin_verifies_and_tampering_is_caught() {
        let root = scratch("signed");
        let plugin = write_plugin(&root, "alpha", r#""description":"a""#);
        let (key_path, public) = key_pair(&root);
        let (signature_file, _) = sign(&plugin, &key_path).expect("sign");
        assert!(signature_file.ends_with("plugin.sig"));

        let policy = SignaturePolicy {
            mode: SignatureMode::Required,
            public_key: Some(public.clone()),
        };
        assert_eq!(verify(&plugin, &policy), SignatureStatus::Verified);

        // 改动清单一个字节，签名立刻失效。
        std::fs::write(
            plugin.join(".zcode-plugin").join("plugin.json"),
            r#"{"name":"alpha","description":"tampered"}"#,
        )
        .unwrap();
        match verify(&plugin, &policy) {
            SignatureStatus::Invalid(_) => {}
            other => panic!("expected an invalid signature, got {:?}", other),
        }
    }

    #[test]
    fn the_off_policy_skips_the_check_entirely() {
        let root = scratch("off");
        let plugin = write_plugin(&root, "alpha", r#""description":"a""#);
        let policy = SignaturePolicy::new(SignatureMode::Off);
        assert_eq!(verify(&plugin, &policy), SignatureStatus::Skipped);
    }

    #[test]
    fn modes_parse_from_settings_text() {
        assert_eq!(SignatureMode::parse("required"), SignatureMode::Required);
        assert_eq!(SignatureMode::parse("WARN"), SignatureMode::Warn);
        assert_eq!(SignatureMode::parse("nonsense"), SignatureMode::Off);
        assert_eq!(SignatureMode::default(), SignatureMode::Off);
    }
}
