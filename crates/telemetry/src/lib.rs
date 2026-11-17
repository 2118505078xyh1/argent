//! Opt-in telemetry.
//!
//! 本地部分是默认行为：开启后把结构化事件追加到
//! `~/.argent/telemetry/events.jsonl`，崩溃记录写到 `~/.argent/logs/crash.log`。
//!
//! 远端上传需要两个条件同时满足——设置了 `telemetry.endpoint`，并且
//! `telemetry.consent` 明确为 `true`。没有同意就绝不联网，这是硬性约束。
//! 上传是尽力而为的：失败只影响上传本身，绝不影响正在跑的任务。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argent_config::{Paths, Settings};
use argent_core::error::{Error, Result};
use argent_core::json::Json;

/// 远端上传的超时时间。事件很小，等这么久已经足够。
const REMOTE_TIMEOUT: Duration = Duration::from_secs(5);

/// A telemetry writer.
#[derive(Debug, Clone)]
pub struct Telemetry {
    enabled: bool,
    events: PathBuf,
    crashes: PathBuf,
    /// 远端端点；为空表示只写本地。
    endpoint: Option<String>,
    /// 用户是否明确同意上传。
    consent: bool,
}

impl Telemetry {
    /// Read the opt-in flag from settings. Anything other than an explicit
    /// `true` leaves telemetry off.
    pub fn from_settings(settings: &Settings, cwd: &Path) -> Telemetry {
        let value = settings.get("telemetry");
        let enabled = settings
            .bool("telemetry")
            .or_else(|| {
                value
                    .and_then(|value| value.get("enabled"))
                    .and_then(Json::as_bool)
            })
            .unwrap_or(false);
        let endpoint = value
            .and_then(|value| value.get("endpoint"))
            .and_then(Json::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
            .or_else(|| {
                settings
                    .string("telemetryEndpoint")
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_string)
            });
        let consent = value
            .and_then(|value| value.get("consent"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let storage = Paths::new(cwd).storage().to_path_buf();
        Telemetry {
            enabled,
            events: storage.join("telemetry").join("events.jsonl"),
            crashes: storage.join("logs").join("crash.log"),
            endpoint,
            consent,
        }
    }

    /// A writer with an explicit destination, for tests and hosts.
    pub fn at(events: impl Into<PathBuf>, crashes: impl Into<PathBuf>, enabled: bool) -> Telemetry {
        Telemetry {
            enabled,
            events: events.into(),
            crashes: crashes.into(),
            endpoint: None,
            consent: false,
        }
    }

    /// 额外配置远端上传；`endpoint` 为空或没有同意时远端被关闭。
    pub fn with_remote(mut self, endpoint: Option<String>, consent: bool) -> Telemetry {
        self.endpoint = endpoint.filter(|text| !text.trim().is_empty());
        self.consent = consent;
        self
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn events_path(&self) -> &Path {
        &self.events
    }

    /// 远端端点（如果配置了）。
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// 远端上传是否真的会发出去：开启 + 有端点 + 明确同意。
    pub fn remote(&self) -> bool {
        self.enabled && self.endpoint.is_some() && self.consent
    }

    /// Append one event. A telemetry failure must never break a run.
    pub fn record(&self, kind: &str, data: Json) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut entry = Json::obj();
        entry.set("time", Json::num(now_secs() as f64));
        entry.set("kind", Json::string(kind.to_string()));
        entry.set("data", data.clone());
        append(&self.events, &format!("{}\n", entry.dump()))?;
        self.send_remote(kind, &data);
        Ok(())
    }

    /// Append a crash line and keep the process alive.
    pub fn crash(&self, message: &str) {
        if !self.enabled {
            return;
        }
        let _ = append(
            &self.crashes,
            &format!("{} {}\n", now_secs(), message.replace('\n', " ")),
        );
    }

    /// 远端请求体：一次事件一行 JSON 太多余，直接把事件本身发过去。
    fn remote_body(&self, kind: &str, data: &Json) -> Json {
        let mut body = Json::obj();
        body.set("version", Json::string(argent_core::VERSION));
        body.set("time", Json::num(now_secs() as f64));
        body.set("kind", Json::string(kind.to_string()));
        body.set("data", data.clone());
        body
    }

    /// 尽力而为地上传一条事件：后台线程发，失败不声张。
    fn send_remote(&self, kind: &str, data: &Json) {
        if !self.remote() {
            return;
        }
        let Some(endpoint) = self.endpoint.clone() else {
            return;
        };
        let request =
            argent_core::http::Request::post_json(&endpoint, &self.remote_body(kind, data).dump())
                .header("X-Argent-Telemetry", "1")
                .timeout(REMOTE_TIMEOUT);
        // 放在后台线程里发，绝不让网络拖慢任务。
        std::thread::spawn(move || {
            let _ = argent_core::http::send(&request);
        });
    }
}

fn append(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("create {} failed: {}", parent.display(), error)))?;
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| Error::io(format!("open {} failed: {}", path.display(), error)))?;
    file.write_all(text.as_bytes())
        .map_err(|error| Error::io(format!("write {} failed: {}", path.display(), error)))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn disabled_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("argent-telemetry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let telemetry = Telemetry::at(dir.join("events.jsonl"), dir.join("crash.log"), false);
        telemetry.record("test", Json::obj()).unwrap();
        telemetry.crash("ignored");
        assert!(!dir.join("events.jsonl").exists());
        assert!(!dir.join("crash.log").exists());
    }

    #[test]
    fn enabled_appends_events_and_crashes() {
        let dir = std::env::temp_dir().join(format!("argent-telemetry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let telemetry = Telemetry::at(dir.join("events.jsonl"), dir.join("crash.log"), true);
        telemetry.record("run", Json::obj()).unwrap();
        telemetry.record("turn", Json::obj()).unwrap();
        telemetry.crash("boom");
        let events = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert_eq!(events.lines().count(), 2);
        let crashes = std::fs::read_to_string(dir.join("crash.log")).unwrap();
        assert!(crashes.contains("boom"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_needs_both_an_endpoint_and_consent() {
        let dir =
            std::env::temp_dir().join(format!("argent-telemetry-remote-{}", std::process::id()));
        let mut telemetry = Telemetry::at(dir.join("events.jsonl"), dir.join("crash.log"), true);
        telemetry = telemetry.with_remote(Some("http://127.0.0.1:9/collect".to_string()), false);
        assert!(!telemetry.remote(), "no consent means no upload");
        telemetry = telemetry.with_remote(None, true);
        assert!(!telemetry.remote(), "no endpoint means no upload");
        telemetry = telemetry.with_remote(Some("http://127.0.0.1:9/collect".to_string()), true);
        assert!(telemetry.remote());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_enabled_writer_posts_to_the_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        listener.set_nonblocking(true).expect("nonblocking");
        let dir =
            std::env::temp_dir().join(format!("argent-telemetry-post-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let telemetry = Telemetry::at(dir.join("events.jsonl"), dir.join("crash.log"), true)
            .with_remote(Some(format!("http://127.0.0.1:{}/collect", port)), true);
        telemetry.record("run", Json::obj()).expect("record");

        // 后台线程发请求，这里等它一下。
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut body = String::new();
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 1024];
                    // 读到头加上 body；请求里带 Content-Length。
                    loop {
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(read) => {
                                buffer.extend_from_slice(&chunk[..read]);
                                let text = String::from_utf8_lossy(&buffer).into_owned();
                                if text.contains("run") && text.contains("\r\n\r\n") {
                                    body = text;
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    // 回一个 200，别让发送端报错。
                    let _ = stream.write_all(
                        b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        assert!(!body.is_empty(), "the endpoint should receive a POST");
        assert!(body.starts_with("POST"), "{}", body);
        assert!(body.contains("\"kind\":\"run\""), "{}", body);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
