//! OAuth 2.0 authorization-code flow with PKCE.
//!
//! The provider template decides the endpoints; this module only knows the
//! standard. Login binds a loopback listener, opens the provider's authorization
//! page, waits for the callback, exchanges the code, and stores the access and
//! refresh tokens in `AuthStore`.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argent_core::error::{Error, Result};
use argent_core::http;
use argent_core::json::Json;
use argent_provider::OAuthSpec;

use crate::{AuthStore, Credential};

/// Run the browser login for one provider.
pub struct OAuthFlow<'a> {
    pub spec: &'a OAuthSpec,
    pub provider: &'a str,
    pub store: &'a mut AuthStore,
}

impl<'a> OAuthFlow<'a> {
    pub fn new(spec: &'a OAuthSpec, provider: &'a str, store: &'a mut AuthStore) -> OAuthFlow<'a> {
        OAuthFlow {
            spec,
            provider,
            store,
        }
    }

    /// Complete a login. `open_browser` false prints the URL for a remote or
    /// headless operator but still waits for the loopback callback.
    pub fn login(&mut self, open_browser: bool) -> Result<Credential> {
        let verifier = random_token(48);
        let challenge = base64url(&argent_core::hash::sha256(verifier.as_bytes()));
        let state = random_token(18);

        let listener = TcpListener::bind(("127.0.0.1", self.spec.redirect_port))
            .map_err(|e| Error::io(format!("cannot bind OAuth callback: {}", e)))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| Error::io(format!("cannot configure OAuth callback: {}", e)))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::io(format!("cannot read callback port: {}", e)))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{}/callback", port);

        let mut url = format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
            self.spec.authorize_url,
            form_encode(&self.spec.client_id),
            form_encode(&redirect_uri),
            form_encode(&state),
            form_encode(&challenge)
        );
        if !self.spec.scopes.is_empty() {
            url.push_str(&format!(
                "&scope={}",
                form_encode(&self.spec.scopes.join(" "))
            ));
        }
        if let Some(audience) = &self.spec.audience {
            url.push_str(&format!("&audience={}", form_encode(audience)));
        }

        println!("Open this URL to authorize {}:", self.provider);
        println!("  {}", url);
        if open_browser {
            let _ = open_url(&url);
        }

        let code = wait_for_callback(&listener, &state, Duration::from_secs(180))?;
        let credential = exchange_code(
            &self.spec.token_url,
            &self.spec.client_id,
            &redirect_uri,
            &code,
            &verifier,
        )?;
        self.store.set(self.provider, credential.clone())?;
        Ok(credential)
    }
}

/// Refresh an expired access token before a request.
pub fn refresh_if_needed(store: &mut AuthStore, provider: &str, spec: &OAuthSpec) -> Result<bool> {
    let Some(credential) = store.get(provider).cloned() else {
        return Ok(false);
    };
    let now = now_secs();
    let fresh = credential
        .access_token
        .as_ref()
        .is_some_and(|token| !token.is_empty())
        && credential
            .expires_at
            .map(|expires| expires > now + 60)
            .unwrap_or(true);
    if fresh {
        return Ok(false);
    }
    let Some(refresh_token) = credential.refresh_token.clone() else {
        return Ok(false);
    };
    let form = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        form_encode(&refresh_token),
        form_encode(&spec.client_id)
    );
    let response = post_token(&spec.token_url, &form)?;
    let replacement = credential_from_json(&response, Some(refresh_token))?;
    store.set(provider, replacement)?;
    Ok(true)
}

fn exchange_code(
    token_url: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<Credential> {
    let form = format!(
        "grant_type=authorization_code&code={}&client_id={}&redirect_uri={}&code_verifier={}",
        form_encode(code),
        form_encode(client_id),
        form_encode(redirect_uri),
        form_encode(verifier)
    );
    let response = post_token(token_url, &form)?;
    credential_from_json(&response, None)
}

fn post_token(token_url: &str, form: &str) -> Result<Json> {
    let request = http::Request::post_form(token_url, form).header("Accept", "application/json");
    let response = http::send(&request)?;
    if !response.is_success() {
        return Err(Error::provider(format!(
            "token endpoint returned {}: {}",
            response.status,
            response.body_text()
        )));
    }
    Json::parse(&response.body_text())
        .map_err(|e| Error::provider(format!("invalid token response: {}", e)))
}

fn credential_from_json(value: &Json, fallback_refresh: Option<String>) -> Result<Credential> {
    let access = value.str_or("access_token", "");
    if access.is_empty() {
        return Err(Error::provider("token response has no access_token"));
    }
    let refresh = value.str_or("refresh_token", "").trim().to_string();
    let refresh = if refresh.is_empty() {
        fallback_refresh
    } else {
        Some(refresh)
    };
    let expires_at = value
        .get("expires_in")
        .and_then(Json::as_i64)
        .filter(|seconds| *seconds > 0)
        .map(|seconds| now_secs() + (seconds as u64).saturating_sub(60));
    Ok(Credential {
        api_key: None,
        access_token: Some(access),
        refresh_token: refresh,
        expires_at,
    })
}

fn wait_for_callback(
    listener: &TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String> {
    let deadline = SystemTime::now() + timeout;
    loop {
        if SystemTime::now() > deadline {
            return Err(Error::provider("OAuth callback timed out"));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut buffer = [0u8; 4096];
                let read = match stream.read(&mut buffer) {
                    Ok(read) => read,
                    Err(_) => continue,
                };
                let request = String::from_utf8_lossy(&buffer[..read]);
                let first_line = request.lines().next().unwrap_or("");
                let path = first_line.split_whitespace().nth(1).unwrap_or("");
                let Some((_, query)) = path.split_once('?') else {
                    respond(&mut stream, 404, "Not found");
                    continue;
                };
                let params = parse_query(query);
                let state = params.get("state").map(String::as_str).unwrap_or("");
                if state != expected_state {
                    respond(&mut stream, 400, "OAuth state mismatch");
                    return Err(Error::provider("OAuth state mismatch"));
                }
                let Some(code) = params.get("code") else {
                    let error = params
                        .get("error_description")
                        .or_else(|| params.get("error"))
                        .cloned()
                        .unwrap_or_else(|| "authorization was not granted".to_string());
                    respond(&mut stream, 400, "Authorization failed");
                    return Err(Error::provider(error));
                };
                respond(
                    &mut stream,
                    200,
                    "argent: authorization complete; you can close this window.",
                );
                return Ok(code.clone());
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn respond(stream: &mut std::net::TcpStream, status: u16, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>argent</title><body style=\"font-family:system-ui;padding:3rem\"><h1>{}</h1></body>",
        message
    );
    let response = format!(
        "HTTP/1.1 {} OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        params.insert(form_decode(key), form_decode(value));
    }
    params
}

fn form_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn form_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64url(data: &[u8]) -> String {
    argent_core::base64::encode(data)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

fn random_token(bytes: usize) -> String {
    let mut seed = Vec::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    seed.extend_from_slice(&nanos.to_le_bytes());
    seed.extend_from_slice(&std::process::id().to_le_bytes());
    let mut out = Vec::new();
    while out.len() < bytes {
        let digest = argent_core::hash::sha256(&seed);
        seed = digest.to_vec();
        out.extend_from_slice(&digest);
    }
    base64url(&out).chars().take(bytes).collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_decodes_form_values() {
        let value = "a b&c=d/你";
        let encoded = form_encode(value);
        assert!(encoded.contains("%20"));
        assert!(encoded.contains("%26"));
        assert_eq!(form_decode(&encoded), value);
    }

    #[test]
    fn pkce_verifier_is_url_safe() {
        let token = random_token(32);
        assert!(!token.contains('='));
        assert!(!token.contains('+'));
        assert!(!token.contains('/'));
        assert_eq!(token.len(), 32);
    }

    #[test]
    fn refreshes_an_expired_token_over_http() {
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            // Read the whole request before answering. The client writes the
            // head and the body separately; a server that answers after the
            // first read and then drops the socket makes Windows reset the
            // connection while the client is still writing (WSAECONNRESET).
            let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let mut expected: Option<usize> = None;
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        request.extend_from_slice(&chunk[..read]);
                        if expected.is_none() {
                            if let Some(split) = request
                                .windows(4)
                                .position(|window| window == b"\r\n\r\n")
                            {
                                let head = String::from_utf8_lossy(&request[..split]);
                                let length = head
                                    .lines()
                                    .find_map(|line| {
                                        let (name, value) = line.split_once(':')?;
                                        if name.eq_ignore_ascii_case("content-length") {
                                            value.trim().parse::<usize>().ok()
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap_or(0);
                                expected = Some(split + 4 + length);
                            }
                        }
                        if expected.is_some_and(|total| request.len() >= total) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let body = r#"{"access_token":"new","refresh_token":"next","expires_in":3600}"#;
            let crlf: String = [13u8, 10].iter().map(|byte| *byte as char).collect();
            let response = format!(
                "HTTP/1.1 200 OK{}Content-Type: application/json{}Content-Length: {}{}Connection: close{}{}{}",
                crlf, crlf, body.len(), crlf, crlf, crlf, body
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            // A half-close says "no more data from me" before the socket is
            // dropped, which is friendlier than an abrupt close on Windows.
            let _ = stream.shutdown(std::net::Shutdown::Write);
        });

        let dir = std::env::temp_dir().join(format!("argent-oauth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        let mut store = AuthStore::at(&path).expect("store");
        store
            .set(
                "demo",
                Credential {
                    api_key: None,
                    access_token: Some("old".to_string()),
                    refresh_token: Some("refresh".to_string()),
                    expires_at: Some(0),
                },
            )
            .expect("seed");
        let spec = OAuthSpec {
            authorize_url: "http://127.0.0.1/authorize".to_string(),
            token_url: format!("http://127.0.0.1:{}/token", port),
            client_id: "client".to_string(),
            scopes: Vec::new(),
            redirect_port: 0,
            audience: None,
        };
        assert!(refresh_if_needed(&mut store, "demo", &spec).expect("refresh"));
        let refreshed = store.get("demo").expect("credential");
        assert_eq!(refreshed.access_token.as_deref(), Some("new"));
        assert_eq!(refreshed.refresh_token.as_deref(), Some("next"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_a_callback_query() {
        let params = parse_query("code=abc&state=xyz");
        assert_eq!(params.get("code").map(String::as_str), Some("abc"));
        assert_eq!(params.get("state").map(String::as_str), Some("xyz"));
    }
}
