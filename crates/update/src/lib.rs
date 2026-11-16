//! Signed release updates.
//!
//! A manifest is JSON with a version, a download URL, a SHA-256 and an Ed25519
//! signature over the canonical manifest without the signature field:
//!
//! ```json
//! {"version":"0.2.0","channel":"stable","url":"https://…/argent.zip",
//!  "sha256":"…","notes":"…","signature":"…"}
//! ```
//!
//! `keygen` creates a private/public pair, `sign` signs a manifest, and
//! `check`/`download` verify both the signature and the archive hash before a
//! byte reaches the installation path. Replacing a running binary is left to
//! the installer; this crate supplies the trust decisions.

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use argent_core::error::{Error, Result};
use argent_core::json::Json;
use rand_core::OsRng;

pub mod channel;
pub mod install;

pub use channel::{
    check, check_remote, compare_versions, last_check, now_epoch, record_check, should_check,
    AutoUpdate, UpdateCheck, DEFAULT_CHECK_INTERVAL_SECS,
};

/// One signed release manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateManifest {
    pub version: String,
    pub channel: String,
    pub url: String,
    pub sha256: String,
    pub notes: String,
    pub signature: String,
}

impl UpdateManifest {
    pub fn from_json(value: &Json) -> Result<UpdateManifest> {
        let version = value.str_or("version", "");
        let url = value.str_or("url", "");
        let sha256 = value.str_or("sha256", "");
        let signature = value.str_or("signature", "");
        if version.is_empty() || url.is_empty() || sha256.is_empty() || signature.is_empty() {
            return Err(Error::config(
                "update manifest needs version, url, sha256 and signature",
            ));
        }
        Ok(UpdateManifest {
            version,
            channel: value.str_or("channel", "stable"),
            url,
            sha256,
            notes: value.str_or("notes", ""),
            signature,
        })
    }

    pub fn to_json(&self, include_signature: bool) -> Json {
        let mut value = Json::obj();
        value.set("version", Json::string(self.version.clone()));
        value.set("channel", Json::string(self.channel.clone()));
        value.set("url", Json::string(self.url.clone()));
        value.set("sha256", Json::string(self.sha256.clone()));
        value.set("notes", Json::string(self.notes.clone()));
        if include_signature {
            value.set("signature", Json::string(self.signature.clone()));
        }
        value
    }

    /// The exact bytes a signer signs. Fixed field order, no signature.
    fn signed_text(&self) -> String {
        self.to_json(false).dump()
    }

    /// Verify the Ed25519 signature with a 32-byte public key.
    pub fn verify(&self, public_key: &[u8; 32]) -> Result<()> {
        let key = VerifyingKey::from_bytes(public_key)
            .map_err(|error| Error::config(format!("invalid update public key: {}", error)))?;
        let signature = decode_signature(&self.signature)?;
        key.verify(self.signed_text().as_bytes(), &signature)
            .map_err(|_| Error::config("update manifest signature does not verify"))
    }
}

/// Load a manifest from a local file or an http(s) URL.
pub fn load_manifest(location: &str) -> Result<UpdateManifest> {
    let value = Json::parse(&String::from_utf8_lossy(&fetch(location)?))
        .map_err(|error| Error::config(format!("invalid update manifest: {}", error)))?;
    UpdateManifest::from_json(&value)
}

/// Generate a new Ed25519 key pair and write `update-key.json`.
pub fn keygen(dir: &Path) -> Result<PathBuf> {
    let key = SigningKey::generate(&mut OsRng);
    let public = key.verifying_key().to_bytes();
    let mut value = Json::obj();
    value.set("privateKey", Json::string(hex(&key.to_bytes())));
    value.set("publicKey", Json::string(hex(&public)));
    let path = dir.join("update-key.json");
    argent_core::write_text_file(&path, &value.dump_pretty())?;
    restrict(&path);
    Ok(path)
}

/// Sign a manifest in place (or write `out`).
pub fn sign(manifest_path: &Path, key_path: &Path, out: Option<&Path>) -> Result<UpdateManifest> {
    let text = argent_core::read_text_file(manifest_path)?;
    let value = Json::parse(&text)
        .map_err(|error| Error::config(format!("invalid manifest: {}", error)))?;
    let mut manifest = UpdateManifest {
        version: value.str_or("version", ""),
        channel: value.str_or("channel", "stable"),
        url: value.str_or("url", ""),
        sha256: value.str_or("sha256", ""),
        notes: value.str_or("notes", ""),
        signature: value.str_or("signature", ""),
    };
    if manifest.version.is_empty() || manifest.url.is_empty() || manifest.sha256.is_empty() {
        return Err(Error::config(
            "unsigned manifest needs version, url and sha256",
        ));
    }
    let key = load_signing_key(key_path)?;
    let signature: Signature = key.sign(manifest.signed_text().as_bytes());
    manifest.signature = hex(&signature.to_bytes());
    let destination = out.unwrap_or(manifest_path);
    argent_core::write_text_file(destination, &manifest.to_json(true).dump_pretty())?;
    Ok(manifest)
}

/// 校验任意字节的分离签名。
///
/// `signature_hex` 是 64 字节 Ed25519 签名的十六进制串，`public_key_hex` 可以
/// 是 32 字节公钥的十六进制串，也可以是公钥文件路径。
pub fn verify_detached(bytes: &[u8], signature_hex: &str, public_key_hex: &str) -> Result<()> {
    let public = parse_public_key(public_key_hex)?;
    let signature = decode_signature(signature_hex)?;
    let key = VerifyingKey::from_bytes(&public)
        .map_err(|error| Error::config(format!("invalid public key: {}", error)))?;
    key.verify(bytes, &signature)
        .map_err(|_| Error::config("signature does not verify"))
}

/// 用私钥文件给任意字节签名，返回十六进制签名。
pub fn sign_detached(bytes: &[u8], key_path: &Path) -> Result<String> {
    let key = load_signing_key(key_path)?;
    let signature: Signature = key.sign(bytes);
    Ok(hex(&signature.to_bytes()))
}

/// 读取私钥文件对应的公钥，返回十六进制。
pub fn public_key_hex(key_path: &Path) -> Result<String> {
    let key = load_signing_key(key_path)?;
    Ok(hex(&key.verifying_key().to_bytes()))
}

/// Compare a manifest against the running version.
pub fn is_newer(manifest: &UpdateManifest, current: &str) -> bool {
    parse_version(&manifest.version) > parse_version(current)
}

/// Download a verified update to `destination`.
pub fn download(
    manifest: &UpdateManifest,
    public_key: &[u8; 32],
    destination: &Path,
) -> Result<PathBuf> {
    manifest.verify(public_key)?;
    let bytes = fetch(&manifest.url)?;
    let digest = argent_core::hash::hex(&argent_core::hash::sha256(&bytes));
    if !digest.eq_ignore_ascii_case(&manifest.sha256) {
        return Err(Error::io(format!(
            "update hash mismatch: expected {}, got {}",
            manifest.sha256, digest
        )));
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("create {} failed: {}", parent.display(), error)))?;
    }
    std::fs::write(destination, &bytes)
        .map_err(|error| Error::io(format!("write {} failed: {}", destination.display(), error)))?;
    Ok(destination.to_path_buf())
}

/// Read a public key from a hex string or a key file.
pub fn parse_public_key(value: &str) -> Result<[u8; 32]> {
    let text = if Path::new(value).is_file() {
        argent_core::read_text_file(Path::new(value))?
    } else {
        value.to_string()
    };
    let trimmed = text.trim();
    let hex_value = if trimmed.starts_with('{') {
        let value = Json::parse(trimmed)
            .map_err(|error| Error::config(format!("invalid key file: {}", error)))?;
        value.str_or("publicKey", "")
    } else {
        trimmed.to_string()
    };
    let bytes = decode_hex(&hex_value)?;
    if bytes.len() != 32 {
        return Err(Error::config(format!(
            "an Ed25519 public key is 32 bytes, got {}",
            bytes.len()
        )));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let text = argent_core::read_text_file(path)?;
    let value = Json::parse(&text)
        .map_err(|error| Error::config(format!("invalid key file: {}", error)))?;
    let private = value.str_or("privateKey", "");
    let bytes = decode_hex(&private)?;
    if bytes.len() != 32 {
        return Err(Error::config("an Ed25519 private key is 32 bytes"));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&key))
}

fn decode_signature(value: &str) -> Result<Signature> {
    let bytes = decode_hex(value)?;
    if bytes.len() != 64 {
        return Err(Error::config("an Ed25519 signature is 64 bytes"));
    }
    let mut signature = [0u8; 64];
    signature.copy_from_slice(&bytes);
    Ok(Signature::from_bytes(&signature))
}

fn fetch(location: &str) -> Result<Vec<u8>> {
    if Path::new(location).is_file() {
        return std::fs::read(location)
            .map_err(|error| Error::io(format!("read {} failed: {}", location, error)));
    }
    let response = argent_core::http::send(&argent_core::http::Request::get(location))?;
    if !response.is_success() {
        return Err(Error::io(format!(
            "fetch {} returned HTTP {}",
            location, response.status
        )));
    }
    Ok(response.body)
}

fn parse_version(version: &str) -> (u64, u64, u64) {
    let main = version.split(['-', '+']).next().unwrap_or(version);
    let mut parts = main.split('.').map(|part| part.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

fn hex(bytes: &[u8]) -> String {
    argent_core::hash::hex(bytes)
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    if value.len() % 2 != 0 {
        return Err(Error::config("hex value has an odd length"));
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let chars: Vec<char> = value.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let pair: String = chars[index..index + 2].iter().collect();
        let byte = u8::from_str_radix(&pair, 16)
            .map_err(|_| Error::config(format!("invalid hex pair '{}'", pair)))?;
        bytes.push(byte);
        index += 2;
    }
    Ok(bytes)
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(metadata) = std::fs::metadata(path) {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn restrict(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_manifest(dir: &Path) -> (UpdateManifest, [u8; 32]) {
        let key = SigningKey::generate(&mut OsRng);
        let public = key.verifying_key().to_bytes();
        let mut manifest = UpdateManifest {
            version: "0.2.0".to_string(),
            channel: "stable".to_string(),
            url: "https://example.invalid/argent.zip".to_string(),
            sha256: "abc".to_string(),
            notes: "notes".to_string(),
            signature: String::new(),
        };
        manifest.signature =
            argent_core::hash::hex(&key.sign(manifest.signed_text().as_bytes()).to_bytes());
        let _ = dir;
        (manifest, public)
    }

    #[test]
    fn verifies_and_rejects_tampering() {
        let dir = std::env::temp_dir().join(format!("argent-update-{}", std::process::id()));
        let (manifest, public) = signed_manifest(&dir);
        manifest.verify(&public).expect("verify");
        let mut tampered = manifest.clone();
        tampered.version = "9.9.9".to_string();
        assert!(tampered.verify(&public).is_err());
    }

    #[test]
    fn keygen_and_sign_round_trip() {
        let dir = std::env::temp_dir().join(format!("argent-update-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = keygen(&dir).expect("keygen");
        let manifest_path = dir.join("manifest.json");
        let mut value = Json::obj();
        value.set("version", Json::string("0.2.0"));
        value.set("url", Json::string("https://example.invalid/x.zip"));
        value.set("sha256", Json::string("00"));
        value.set("signature", Json::string(""));
        argent_core::write_text_file(&manifest_path, &value.dump()).unwrap();
        // `sign` accepts an unsigned manifest when the signature is empty.
        let manifest = sign(&manifest_path, &key_path, None).expect("sign");
        let public = parse_public_key(&key_path.display().to_string()).expect("public");
        manifest.verify(&public).expect("verify");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer(
            &UpdateManifest {
                version: "0.2.0".to_string(),
                channel: "stable".to_string(),
                url: String::new(),
                sha256: String::new(),
                notes: String::new(),
                signature: String::new(),
            },
            "0.1.9"
        ));
        assert!(!is_newer(
            &UpdateManifest {
                version: "0.1.0".to_string(),
                channel: "stable".to_string(),
                url: String::new(),
                sha256: String::new(),
                notes: String::new(),
                signature: String::new(),
            },
            "0.1.0"
        ));
    }

    #[test]
    fn detached_signatures_round_trip_and_reject_tampering() {
        let dir = std::env::temp_dir().join(format!("argent-update-detached-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = keygen(&dir).expect("keygen");
        let public = public_key_hex(&key_path).expect("public key");
        let payload = b"plugin manifest bytes";
        let signature = sign_detached(payload, &key_path).expect("sign");
        verify_detached(payload, &signature, &public).expect("verify");
        assert!(verify_detached(b"other bytes", &signature, &public).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
