//! `argent auth`：给脚本读取已存凭据的三个子命令。
//!
//! - `print-api-key <provider>`：只打印 API key 本体；
//! - `print-bearer-token <provider>`：只打印可直接放进
//!   `Authorization: Bearer` 的 OAuth 令牌（过期时会先刷新）；
//! - `check [<provider>]`：打印不含秘密的凭据元信息报告，不带 provider 时
//!   列出所有已存凭据的 provider。
//!
//! 契约：不带 `--json` 时，秘密本体原样打到 **stdout**（脚本直接抓走），
//! 报告类提示一律走 **stderr**；`--json` 时 stdout 只有一个 JSON 对象或数组。
//! 找不到 / 无凭据时退出码 1，错误走 stderr，绝不 panic。
//!
//! 参数解析单独成函数，方便单测；真正的动作只有三个，都只读凭据。

use argent_auth::{AuthStore, CredentialReport};
use argent_config::Settings;
use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_provider::{ProviderCatalog, ProviderSpec};

/// 解析后的 `argent auth` 子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthCommand {
    /// 打印 provider 已存的 API key。
    PrintApiKey { provider: String, json: bool },
    /// 打印 provider 的 OAuth bearer 令牌。
    PrintBearerToken { provider: String, json: bool },
    /// 凭据状态报告；`provider` 为 `None` 时列出所有已存凭据的 provider。
    Check {
        provider: Option<String>,
        json: bool,
    },
}

/// `argent auth` 的用法文本。
const USAGE: &str = "\
usage: argent auth print-api-key <provider> [--json]
       argent auth print-bearer-token <provider> [--json]
       argent auth check [<provider>] [--json]

Without --json the secret goes to stdout (for scripts) and reports go to
stderr; with --json stdout carries exactly one JSON object or array. A missing
credential exits with code 1.";

/// 解析 `argent auth ...` 的参数。
pub(crate) fn parse(args: &[String]) -> Result<AuthCommand> {
    let mut json = false;
    let mut positional: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            // 全局的 -C/--cwd 由 cwd_from 处理，这里跳过后面的取值。
            "--cwd" | "-C" => index += 1,
            other if other.starts_with('-') => {
                return Err(Error::config(format!("unknown auth option '{}'", other)));
            }
            other => positional.push(other.to_string()),
        }
        index += 1;
    }

    let action = positional.first().map(String::as_str).unwrap_or("");
    match action {
        "print-api-key" => {
            if positional.len() > 2 {
                return Err(Error::config(
                    "usage: argent auth print-api-key <provider> [--json]",
                ));
            }
            let provider = positional.get(1).cloned().ok_or_else(|| {
                Error::config("usage: argent auth print-api-key <provider> [--json]")
            })?;
            Ok(AuthCommand::PrintApiKey { provider, json })
        }
        "print-bearer-token" => {
            if positional.len() > 2 {
                return Err(Error::config(
                    "usage: argent auth print-bearer-token <provider> [--json]",
                ));
            }
            let provider = positional.get(1).cloned().ok_or_else(|| {
                Error::config("usage: argent auth print-bearer-token <provider> [--json]")
            })?;
            Ok(AuthCommand::PrintBearerToken { provider, json })
        }
        "check" => {
            if positional.len() > 2 {
                return Err(Error::config(
                    "usage: argent auth check [<provider>] [--json]",
                ));
            }
            Ok(AuthCommand::Check {
                provider: positional.get(1).cloned(),
                json,
            })
        }
        "" => Err(Error::config(
            "usage: argent auth print-api-key|print-bearer-token|check ...",
        )),
        other => Err(Error::config(format!(
            "unknown auth command '{}'; expected print-api-key, print-bearer-token or check",
            other
        ))),
    }
}

/// 执行 `argent auth ...`。
pub(crate) fn run(args: &[String]) -> Result<i32> {
    if crate::commands::wants_help(args) {
        println!("{}", USAGE);
        return Ok(0);
    }
    let command = parse(args)?;
    let cwd = crate::commands::cwd_from(args);
    // 与构建会话目录时一致：内置模板叠加用户配置，私有网关也能被识别。
    let settings = Settings::load(&cwd)?;
    let mut catalog = ProviderCatalog::builtin();
    catalog.merge(settings.raw());
    // 后端随 `credentialBackend` 走：密钥环或 DPAPI 文件。
    let mut store = AuthStore::load()?;

    match command {
        AuthCommand::PrintApiKey { provider, json } => {
            print_secret(&mut store, &catalog, &provider, json, true)
        }
        AuthCommand::PrintBearerToken { provider, json } => {
            print_secret(&mut store, &catalog, &provider, json, false)
        }
        AuthCommand::Check { provider, json } => check(&mut store, &catalog, provider, json),
    }
}

/// 打印一条秘密：成功时 stdout 只有秘密本身（或一个 JSON 对象），失败时退出码 1。
fn print_secret(
    store: &mut AuthStore,
    catalog: &ProviderCatalog,
    provider: &str,
    json: bool,
    is_key: bool,
) -> Result<i32> {
    let spec: Option<ProviderSpec> = catalog.get(provider).cloned();
    let result = if is_key {
        store.print_api_key_with_spec(provider, spec.as_ref())
    } else {
        store.print_bearer_token_with_spec(provider, spec.as_ref())
    };
    match result {
        Ok(secret) => {
            if json {
                let mut value = Json::obj();
                value.set("provider", Json::string(provider.to_string()));
                value.set(
                    "kind",
                    Json::string(if is_key { "api-key" } else { "bearer-token" }),
                );
                value.set("value", Json::string(secret));
                println!("{}", value.dump_pretty());
            } else {
                println!("{}", secret);
            }
            Ok(0)
        }
        Err(error) => {
            // 找不到 / 无凭据 / 种类不符：可读错误走 stderr，退出码 1。
            eprintln!("{}", error);
            Ok(1)
        }
    }
}

/// `check`：单个 provider 未找到时退出码 1；不带 provider 时列出全部已存凭据。
fn check(
    store: &mut AuthStore,
    catalog: &ProviderCatalog,
    provider: Option<String>,
    json: bool,
) -> Result<i32> {
    match provider {
        Some(provider) => {
            let spec: Option<ProviderSpec> = catalog.get(&provider).cloned();
            let report = store.check_with_spec(&provider, spec.as_ref(), None);
            if json {
                println!("{}", report_to_json(&report).dump_pretty());
                if !report.found {
                    eprintln!("{}", not_found_message(&provider));
                }
            } else {
                print_report_human(&report);
            }
            Ok(if report.found { 0 } else { 1 })
        }
        None => {
            let ids = store.provider_ids();
            if ids.is_empty() {
                eprintln!("no stored credentials; run argent login <provider> to store one");
                return Ok(1);
            }
            let reports: Vec<CredentialReport> = ids
                .iter()
                .map(|id| {
                    let spec: Option<ProviderSpec> = catalog.get(id).cloned();
                    store.check_with_spec(id, spec.as_ref(), None)
                })
                .collect();
            if json {
                let array = Json::Arr(reports.iter().map(report_to_json).collect());
                println!("{}", array.dump_pretty());
            } else {
                for report in &reports {
                    print_report_human(report);
                }
            }
            Ok(0)
        }
    }
}

/// 报告转 JSON：只含元信息，绝无秘密字段。
fn report_to_json(report: &CredentialReport) -> Json {
    let mut value = Json::obj();
    value.set("provider", Json::string(report.provider.clone()));
    value.set("found", Json::bool(report.found));
    value.set(
        "kind",
        match report.kind {
            Some(kind) => Json::string(kind.as_str().to_string()),
            None => Json::Null,
        },
    );
    value.set(
        "source",
        match report.source {
            Some(source) => Json::string(source.as_str().to_string()),
            None => Json::Null,
        },
    );
    value.set("expired", Json::bool(report.expired));
    value.set("store", Json::string(report.store.clone()));
    value.set(
        "scopes",
        Json::Arr(
            report
                .scopes
                .iter()
                .map(|scope| Json::string(scope.clone()))
                .collect(),
        ),
    );
    value
}

/// 人类可读报告：走 stderr，避免和 stdout 上的秘密混在一起。
fn print_report_human(report: &CredentialReport) {
    if !report.found {
        eprintln!("{}", not_found_message(&report.provider));
        return;
    }
    eprintln!("{}:", report.provider);
    eprintln!(
        "  kind    {}",
        report.kind.map(|kind| kind.as_str()).unwrap_or("-")
    );
    eprintln!(
        "  source  {}",
        report.source.map(|source| source.as_str()).unwrap_or("-")
    );
    eprintln!("  expired {}", if report.expired { "yes" } else { "no" });
    eprintln!("  store   {}", report.store);
    if !report.scopes.is_empty() {
        eprintln!("  scopes  {}", report.scopes.join(", "));
    }
}

/// 单 provider 无凭据时的可读提示（英文，与其它 CLI 错误风格一致）。
fn not_found_message(provider: &str) -> String {
    format!(
        "no credential for provider '{}' (store: resolved); run argent login {} to store one",
        provider, provider
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use argent_auth::{CredentialKind, CredentialSource};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_print_commands() {
        assert_eq!(
            parse(&args(&["print-api-key", "zai"])).expect("parse"),
            AuthCommand::PrintApiKey {
                provider: "zai".to_string(),
                json: false,
            }
        );
        assert_eq!(
            parse(&args(&["print-bearer-token", "zai", "--json"])).expect("parse"),
            AuthCommand::PrintBearerToken {
                provider: "zai".to_string(),
                json: true,
            }
        );
    }

    #[test]
    fn parses_check_with_and_without_provider() {
        assert_eq!(
            parse(&args(&["check"])).expect("parse"),
            AuthCommand::Check {
                provider: None,
                json: false,
            }
        );
        assert_eq!(
            parse(&args(&["check", "anthropic", "--json"])).expect("parse"),
            AuthCommand::Check {
                provider: Some("anthropic".to_string()),
                json: true,
            }
        );
    }

    #[test]
    fn cwd_flags_do_not_become_providers() {
        assert_eq!(
            parse(&args(&["check", "-C", "some/dir", "zai"])).expect("parse"),
            AuthCommand::Check {
                provider: Some("zai".to_string()),
                json: false,
            }
        );
    }

    #[test]
    fn missing_or_unknown_arguments_are_readable() {
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["print-api-key"])).is_err());
        assert!(parse(&args(&["print-bearer-token"])).is_err());
        assert!(parse(&args(&["frobnicate"])).is_err());
        assert!(parse(&args(&["--wat"])).is_err());
        assert!(parse(&args(&["check", "zai", "extra"])).is_err());
    }

    #[test]
    fn the_json_report_carries_no_secret_field() {
        let report = CredentialReport {
            provider: "zai".to_string(),
            found: true,
            kind: Some(CredentialKind::ApiKey),
            source: Some(CredentialSource::File),
            expired: false,
            store: "C:/creds/auth.json".to_string(),
            scopes: vec!["read".to_string()],
        };
        let dumped = report_to_json(&report).dump();
        assert!(dumped.contains("\"kind\":\"api-key\""), "{}", dumped);
        assert!(dumped.contains("\"source\":\"file\""), "{}", dumped);
        assert!(dumped.contains("\"found\":true"), "{}", dumped);
        // 报告结构里根本没有秘密字段。
        assert!(!dumped.contains("secret"), "{}", dumped);
    }

    #[test]
    fn a_not_found_report_is_json_null_typed() {
        let report = CredentialReport {
            provider: "ghost".to_string(),
            found: false,
            kind: None,
            source: None,
            expired: false,
            store: "keyring".to_string(),
            scopes: Vec::new(),
        };
        let dumped = report_to_json(&report).dump();
        assert!(dumped.contains("\"kind\":null"), "{}", dumped);
        assert!(dumped.contains("\"found\":false"), "{}", dumped);
    }
}
