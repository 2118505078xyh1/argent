//! ZCode-style subcommands: login, plugins, skills, commands, hooks, doctor.
//!
//! These run before the agent is built. They only inspect or edit configuration
//! and extension directories, so they are cheap even on a cold binary.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use argent_auth::{AuthStore, Credential};
use argent_config::{Paths, Settings};
use argent_core::error::{Error, Result};
use argent_core::json::Json;
use argent_hooks::{digest_file, HookConfig, HookTrust};
use argent_plugins::{
    InstallPolicy, MarketplaceStore, PluginManager, PluginManifest, SignaturePolicy,
};
use argent_provider::ProviderCatalog;
use argent_tools::{Skill, SkillRegistry};

use crate::args::HELP;
use crate::export::{self, ExportMeta};

/// Run a subcommand when the first argument names one.
///
/// `None` means the argument list is an ordinary prompt or flag set and the
/// normal CLI path should handle it.
pub fn dispatch(raw: &[String]) -> Result<Option<i32>> {
    let Some(command) = raw.first() else {
        return Ok(None);
    };
    match command.as_str() {
        "login" => Ok(Some(login(&raw[1..])?)),
        "logout" => Ok(Some(logout(&raw[1..])?)),
        "auth" => Ok(Some(crate::auth_cmd::run(&raw[1..])?)),
        "models" => Ok(Some(crate::models_cmd::run(&raw[1..])?)),
        "mcp" => Ok(Some(crate::mcp::run(&raw[1..])?)),
        "plugins" | "plugin" => Ok(Some(plugins(&raw[1..])?)),
        "skills" => Ok(Some(skills(&raw[1..])?)),
        "commands" => Ok(Some(commands(&raw[1..])?)),
        "hooks" => Ok(Some(hooks(&raw[1..])?)),
        "workflow" => Ok(Some(workflow(&raw[1..])?)),
        "telemetry" => Ok(Some(telemetry(&raw[1..])?)),
        "sessions" => Ok(Some(sessions(&raw[1..])?)),
        "audit" => Ok(Some(audit(&raw[1..])?)),
        "usage" => Ok(Some(usage(&raw[1..])?)),
        "policy" => Ok(Some(policy(&raw[1..])?)),
        "profiles" | "profile" => Ok(Some(profiles(&raw[1..])?)),
        "themes" | "theme" => Ok(Some(themes(&raw[1..])?)),
        "keybindings" | "keys" => Ok(Some(keybindings(&raw[1..])?)),
        "automation" | "automations" | "schedule" => Ok(Some(automation(&raw[1..])?)),
        "eval" => Ok(Some(eval(&raw[1..])?)),
        "update" => Ok(Some(update(&raw[1..])?)),
        "completion" => Ok(Some(completion(&raw[1..])?)),
        "agents" | "background" => Ok(Some(agents(&raw[1..])?)),
        "worktree" | "worktrees" => Ok(Some(worktree(&raw[1..])?)),
        "replay" => Ok(Some(replay(&raw[1..])?)),
        "attach" => Ok(Some(attach_session(&raw[1..])?)),
        "logs" => Ok(Some(background_logs(&raw[1..], false)?)),
        "follow" => Ok(Some(background_logs(&raw[1..], true)?)),
        "stop" => Ok(Some(background_stop(&raw[1..])?)),
        "rm" | "remove" => Ok(Some(background_remove(&raw[1..])?)),
        "help" => {
            print!("{}", HELP);
            Ok(Some(0))
        }
        "version" => {
            println!("argent {}", argent_core::VERSION);
            Ok(Some(0))
        }
        "doctor" => Ok(Some(crate::doctor()?)),
        "bug" => Ok(Some(bug_report(&raw[1..])?)),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------- login

fn login(args: &[String]) -> Result<i32> {
    let mut provider = "zai".to_string();
    let mut api_key: Option<String> = None;
    let mut no_browser = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--api-key" => {
                index += 1;
                api_key = args.get(index).cloned();
                if api_key.is_none() {
                    return Err(Error::config("--api-key needs a value"));
                }
            }
            "--no-browser" => no_browser = true,
            other if other.starts_with('-') => {
                return Err(Error::config(format!("unknown login option '{}'", other)));
            }
            other => provider = other.to_string(),
        }
        index += 1;
    }

    let catalog = ProviderCatalog::builtin();
    let Some(spec) = catalog.get(&provider) else {
        let known = catalog
            .list()
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::config(format!(
            "unknown provider '{}'; known providers: {}",
            provider, known
        )));
    };

    if api_key.is_none() {
        if let Some(oauth) = &spec.oauth {
            let mut store = AuthStore::load()?;
            let mut flow = argent_auth::oauth::OAuthFlow::new(oauth, &provider, &mut store);
            match flow.login(!no_browser) {
                Ok(_) => {
                    println!(
                        "logged in to {} via browser OAuth; credential stored in {}",
                        provider,
                        store.location()
                    );
                    return Ok(0);
                }
                Err(error) => {
                    eprintln!("OAuth login failed: {}", error);
                    return Ok(1);
                }
            }
        }
        if no_browser {
            println!("Open the provider's API-key page and create a key:");
            println!("  {}", key_page(&provider));
            println!("Then run: argent login {} --api-key <key>", provider);
            return Ok(1);
        }
        if std::io::stdin().is_terminal() {
            print!(
                "Paste the {} API key (input is hidden by the terminal): ",
                provider
            );
            std::io::stdout().flush().ok();
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .map_err(|e| Error::io(format!("read key failed: {}", e)))?;
            let key = line.trim().to_string();
            if !key.is_empty() {
                api_key = Some(key);
            }
        }
    }

    let Some(key) = api_key else {
        println!("No key entered. Create one here: {}", key_page(&provider));
        println!("Then run: argent login {} --api-key <key>", provider);
        return Ok(1);
    };

    let mut store = AuthStore::load()?;
    store.set(
        &provider,
        Credential {
            api_key: Some(key),
            ..Credential::default()
        },
    )?;
    println!(
        "logged in to {}; credential stored in {}",
        provider,
        store.location()
    );
    if spec.api_key_env.is_some() {
        println!(
            "The provider's environment variable also works; the stored key takes precedence."
        );
    }
    Ok(0)
}

fn key_page(provider: &str) -> &'static str {
    match provider {
        "zai" => "https://z.ai/manage-apikey/apikey-list",
        "bigmodel" => "https://bigmodel.cn/usercenter/proj-mgmt/apikeys",
        "anthropic" => "https://console.anthropic.com/settings/keys",
        "openai" => "https://platform.openai.com/api-keys",
        "deepseek" => "https://platform.deepseek.com/api_keys",
        "moonshot" => "https://platform.moonshot.cn/console/api-keys",
        "openrouter" => "https://openrouter.ai/keys",
        _ => "the provider's dashboard",
    }
}

fn logout(args: &[String]) -> Result<i32> {
    let mut store = AuthStore::load()?;
    let provider = args.iter().find(|arg| !arg.starts_with('-')).cloned();
    match provider {
        Some(provider) => {
            if store.remove(&provider)? {
                println!("removed the stored credential for {}", provider);
            } else {
                println!("no stored credential for {}", provider);
            }
        }
        None => {
            let ids = store.provider_ids();
            if ids.is_empty() {
                println!("no stored credentials");
            } else {
                for id in ids {
                    store.remove(&id)?;
                }
                println!("removed every stored credential");
            }
        }
    }
    Ok(0)
}

// ---------------------------------------------------------------- plugins

fn plugins(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let settings = Settings::load(&cwd)?;
    let paths = Paths::new(&cwd);
    let mut manager = PluginManager::for_workspace(&cwd);
    manager.apply_state(&settings);

    let subcommand = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("list");
    let positional: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .cloned()
        .collect();
    let json = args.iter().any(|arg| arg == "--json");
    let force = args.iter().any(|arg| arg == "--force");
    let available = args.iter().any(|arg| arg == "--available");
    // 插件签名策略来自设置 `pluginSignature`，公钥来自 `pluginPublicKey`。
    let mut report = |line: &str| eprintln!("{}", line);
    let mut install_policy =
        InstallPolicy::with_notify(SignaturePolicy::from_settings(&settings), &mut report);

    match subcommand {
        "list" | "ls" => {
            if json {
                print_plugins_json(&manager);
            } else {
                for plugin in manager.all() {
                    println!(
                        "{}  {:<10}  {:<8}  {}",
                        if plugin.enabled { "on " } else { "off" },
                        plugin.manifest.name,
                        plugin.manifest.version,
                        plugin.manifest.description
                    );
                }
                if manager.all().is_empty() {
                    println!("no plugins found under:");
                    for root in manager.roots() {
                        println!("  {}", root.display());
                    }
                }
                if available {
                    let store = MarketplaceStore::load()?;
                    println!();
                    println!("available from marketplaces:");
                    for record in store.list() {
                        let catalog = match store.catalog(record) {
                            Ok(catalog) => catalog,
                            Err(error) => {
                                eprintln!("{}: {}", record.name, error);
                                continue;
                            }
                        };
                        for entry in &catalog.plugins {
                            if manager.get(&entry.name).is_none() {
                                println!(
                                    "  {}/{}  {}  {}",
                                    catalog.name,
                                    entry.name,
                                    entry.version,
                                    entry.description
                                );
                            }
                        }
                    }
                }
            }
            Ok(0)
        }
        "install" => {
            let Some(source) = positional.get(1) else {
                return Err(Error::config(
                    "usage: argent plugins install <path|name@marketplace> [--force]",
                ));
            };
            let (name, marketplace) = match source.split_once('@') {
                Some((name, marketplace)) => (name, Some(marketplace)),
                None => (source.as_str(), None),
            };
            let path = Path::new(name);
            let manifest = if path.exists() {
                PluginManager::install_local(
                    path,
                    &paths.plugins_root(),
                    force,
                    &mut install_policy,
                )?
            } else {
                let store = MarketplaceStore::load()?;
                let (catalog, entry) = store.find_plugin(name, marketplace)?;
                store.install(
                    &catalog,
                    &entry,
                    &paths.plugins_root(),
                    force,
                    &mut install_policy,
                )?
            };
            println!(
                "installed {} {} at {}",
                manifest.name,
                manifest.version,
                manifest.root.display()
            );
            Ok(0)
        }
        "update" => {
            let mut store = MarketplaceStore::load()?;
            match positional.get(1) {
                Some(name) => {
                    let (catalog, entry) = store.find_plugin(name, None)?;
                    let manifest = store.install(
                        &catalog,
                        &entry,
                        &paths.plugins_root(),
                        true,
                        &mut install_policy,
                    )?;
                    println!(
                        "updated {} {} at {}",
                        manifest.name,
                        manifest.version,
                        manifest.root.display()
                    );
                }
                None => {
                    let updated = store.update(None)?;
                    if updated.is_empty() {
                        println!("no marketplaces registered");
                    } else {
                        println!("updated {}", updated.join(", "));
                    }
                }
            }
            Ok(0)
        }
        "uninstall" | "remove" => {
            let Some(name) = positional.get(1) else {
                return Err(Error::config("usage: argent plugins uninstall <name>"));
            };
            if PluginManager::uninstall(name, &paths.plugin_dirs())? {
                println!("uninstalled {}", name);
                Ok(0)
            } else {
                eprintln!("plugin '{}' is not installed", name);
                Ok(1)
            }
        }
        "enable" | "disable" => {
            let Some(name) = positional.get(1) else {
                return Err(Error::config(format!(
                    "usage: argent plugins {} <name>",
                    subcommand
                )));
            };
            if manager.get(name).is_none() {
                eprintln!("plugin '{}' was not found", name);
                return Ok(1);
            }
            PluginManager::set_enabled(&settings, name, subcommand == "enable")?;
            println!(
                "{} {}",
                if subcommand == "enable" {
                    "enabled"
                } else {
                    "disabled"
                },
                name
            );
            Ok(0)
        }
        "validate" => {
            let Some(path) = positional.get(1) else {
                return Err(Error::config("usage: argent plugins validate <path>"));
            };
            let manifest = PluginManifest::load(Path::new(path))?;
            let status = argent_plugins::signature::verify(
                Path::new(path),
                &SignaturePolicy::from_settings(&settings),
            );
            println!(
                "{} {} is valid\n  root: {}\n  skills: {}\n  commands: {}\n  agents: {}\n  mcp servers: {}\n  signature: {}",
                manifest.name,
                manifest.version,
                manifest.root.display(),
                manifest.skills.len(),
                manifest.commands.len(),
                manifest.agents.len(),
                manifest.mcp_servers.len(),
                status.describe()
            );
            Ok(0)
        }
        "sign" => {
            let key = args
                .iter()
                .position(|arg| arg == "--key")
                .and_then(|index| args.get(index + 1))
                .cloned()
                .or_else(|| {
                    args.iter()
                        .find_map(|arg| arg.strip_prefix("--key=").map(str::to_string))
                })
                .ok_or_else(|| {
                    Error::config("usage: argent plugins sign --key <keyfile> <plugin-dir>")
                })?;
            // 位置参数里第一个是子命令本身；其余要排除掉已知开关的取值。
            let flag_values: Vec<&String> = ["--cwd", "-C", "--key", "--out"]
                .iter()
                .filter_map(|flag| {
                    args.iter()
                        .position(|arg| arg == flag)
                        .and_then(|index| args.get(index + 1))
                })
                .collect();
            let target = positional
                .iter()
                .skip(1)
                .find(|arg| !flag_values.contains(arg))
                .cloned()
                .ok_or_else(|| {
                    Error::config("usage: argent plugins sign --key <keyfile> <plugin-dir>")
                })?;
            let (signature_file, public_key) =
                argent_plugins::signature::sign(Path::new(&target), Path::new(&key))
                    .map_err(Error::config)?;
            println!(
                "signed {}\n  public key: {}",
                signature_file.display(),
                public_key
            );
            println!("在设置里写 pluginPublicKey = {} 以校验该插件", public_key);
            Ok(0)
        }
        "marketplace" => marketplace_command(&positional, args, &paths),
        other => Err(Error::config(format!(
            "unknown plugins command '{}'; expected list, install, update, uninstall, enable, disable, validate, sign, marketplace",
            other
        ))),
    }
}

/// `argent plugins marketplace add|list|remove|update`.
fn marketplace_command(positional: &[String], args: &[String], _paths: &Paths) -> Result<i32> {
    let action = positional.get(1).map(String::as_str).unwrap_or("list");
    let mut store = MarketplaceStore::load()?;
    match action {
        "add" => {
            let Some(source) = positional.get(2) else {
                return Err(Error::config(
                    "usage: argent plugins marketplace add <path|url> [--name <name>]",
                ));
            };
            let name = args
                .iter()
                .position(|arg| arg == "--name")
                .and_then(|index| args.get(index + 1))
                .map(String::as_str);
            let marketplace = store.add(source, name)?;
            println!(
                "added marketplace {} ({} plugins) at {}",
                marketplace.name,
                marketplace.plugins.len(),
                marketplace.root.display()
            );
            Ok(0)
        }
        "list" | "ls" => {
            if store.list().is_empty() {
                println!("no marketplaces registered");
                return Ok(0);
            }
            for record in store.list() {
                let count = store
                    .catalog(record)
                    .map(|catalog| catalog.plugins.len())
                    .unwrap_or(0);
                println!("{:<20} {:<6} {}", record.name, count, record.source);
            }
            Ok(0)
        }
        "remove" | "rm" => {
            let Some(name) = positional.get(2) else {
                return Err(Error::config(
                    "usage: argent plugins marketplace remove <name>",
                ));
            };
            if store.remove(name)? {
                println!("removed marketplace {}", name);
                Ok(0)
            } else {
                eprintln!("marketplace '{}' is not registered", name);
                Ok(1)
            }
        }
        "update" => {
            let name = positional.get(2).map(String::as_str);
            let updated = store.update(name)?;
            if updated.is_empty() {
                println!("no matching marketplaces");
            } else {
                println!("updated {}", updated.join(", "));
            }
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown marketplace command '{}'; expected add, list, remove, update",
            other
        ))),
    }
}

fn print_plugins_json(manager: &PluginManager) {
    let items: Vec<Json> = manager
        .all()
        .iter()
        .map(|plugin| {
            let mut entry = Json::obj();
            entry.set("name", Json::string(plugin.manifest.name.clone()));
            entry.set("version", Json::string(plugin.manifest.version.clone()));
            entry.set(
                "description",
                Json::string(plugin.manifest.description.clone()),
            );
            entry.set("enabled", Json::bool(plugin.enabled));
            entry.set(
                "path",
                Json::string(plugin.manifest.root.display().to_string()),
            );
            entry
        })
        .collect();
    println!("{}", Json::Arr(items).dump_pretty());
}

// ---------------------------------------------------------------- skills

fn skills(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let mut roots = SkillRegistry::default_roots(&cwd);
    let manager = PluginManager::for_workspace(&cwd);
    for dir in manager.skill_dirs() {
        roots.push(dir);
    }
    let registry = SkillRegistry::discover(&roots);
    let subcommand = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("list");

    match subcommand {
        "list" | "ls" => {
            if args.iter().any(|arg| arg == "--json") {
                let items: Vec<Json> = registry
                    .skills()
                    .iter()
                    .map(|skill| {
                        let mut entry = Json::obj();
                        entry.set("name", Json::string(skill.name.clone()));
                        entry.set("description", Json::string(skill.description.clone()));
                        entry.set("path", Json::string(skill.path.display().to_string()));
                        entry
                    })
                    .collect();
                println!("{}", Json::Arr(items).dump_pretty());
            } else if registry.is_empty() {
                println!("no skills found under:");
                for root in &roots {
                    println!("  {}", root.display());
                }
            } else {
                for skill in registry.skills() {
                    println!("{:<24} {}", skill.name, skill.description);
                }
            }
            Ok(0)
        }
        "inspect" | "show" => {
            let name = args
                .iter()
                .skip_while(|arg| arg.as_str() != "inspect" && arg.as_str() != "show")
                .nth(1);
            let Some(name) = name else {
                return Err(Error::config("usage: argent skills inspect <name>"));
            };
            let Some(skill) = registry.get(name) else {
                eprintln!("skill '{}' was not found", name);
                return Ok(1);
            };
            print_skill(skill);
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown skills command '{}'; expected list or inspect",
            other
        ))),
    }
}

fn print_skill(skill: &Skill) {
    println!("# {}", skill.name);
    if !skill.description.is_empty() {
        println!("{}", skill.description);
    }
    println!("({})", skill.path.display());
    println!();
    println!("{}", skill.body);
}

// ---------------------------------------------------------------- commands

fn commands(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let paths = Paths::new(&cwd);
    let manager = PluginManager::for_workspace(&cwd);
    let mut roots = paths.command_dirs();
    roots.extend(manager.command_dirs());
    let docs = argent_commands::discover(&roots);
    let subcommand = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("list");

    match subcommand {
        "list" | "ls" => {
            if docs.is_empty() {
                println!("no custom commands found under:");
                for root in &roots {
                    println!("  {}", root.display());
                }
            } else {
                for doc in &docs {
                    println!("{:<24} {}", doc.name, doc.description);
                }
            }
            Ok(0)
        }
        "inspect" | "show" => {
            let name = args
                .iter()
                .skip_while(|arg| arg.as_str() != "inspect" && arg.as_str() != "show")
                .nth(1);
            let Some(name) = name else {
                return Err(Error::config("usage: argent commands inspect <name>"));
            };
            let Some(doc) = docs.iter().find(|doc| &doc.name == name) else {
                eprintln!("command '{}' was not found", name);
                return Ok(1);
            };
            println!("# /{}", doc.name);
            if !doc.description.is_empty() {
                println!("{}", doc.description);
            }
            println!("({})", doc.path.display());
            println!();
            println!("{}", doc.body);
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown commands command '{}'; expected list or inspect",
            other
        ))),
    }
}

// ---------------------------------------------------------------- hooks

fn hooks(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let paths = Paths::new(&cwd);
    let trust_path = paths.hook_trust();
    let subcommand = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("status");

    match subcommand {
        "trust" => {
            let action = args
                .iter()
                .skip_while(|arg| arg.as_str() != "trust")
                .nth(1)
                .map(String::as_str)
                .unwrap_or("status");
            hooks_trust(action, args, &cwd, &trust_path)
        }
        "status" => {
            let trust = HookTrust::at(&trust_path)?;
            print_hook_trust(&trust);
            Ok(0)
        }
        "list" => {
            let manager = PluginManager::for_workspace(&cwd);
            print_hook_list(&manager, &cwd, &HookTrust::at(&trust_path)?)
        }
        "validate" => {
            let manager = PluginManager::for_workspace(&cwd);
            let explicit = args
                .iter()
                .skip_while(|arg| arg.as_str() != "validate")
                .skip(1)
                .find(|arg| !arg.starts_with('-'))
                .map(PathBuf::from);
            let files = match explicit {
                Some(path) => vec![path],
                None => crate::workspace_hook_files(&manager, &cwd),
            };
            hooks_validate(&files)
        }
        other => Err(Error::config(format!(
            "unknown hooks command '{}'; expected trust, list, validate",
            other
        ))),
    }
}

fn print_hook_list(manager: &PluginManager, cwd: &Path, trust: &HookTrust) -> Result<i32> {
    let files = crate::workspace_hook_files(manager, cwd);
    if files.is_empty() {
        println!("no hook documents found");
        return Ok(0);
    }
    let mut total = 0;
    let mut documents = 0;
    for file in files {
        if !file.is_file() {
            continue;
        }
        documents += 1;
        let digest = digest_file(&file)?;
        let trusted = trust.is_trusted(&file, &digest);
        let config = HookConfig::load(&file).unwrap_or_default();
        println!(
            "{}  {}  {} hook(s)  {}",
            if trusted { "trusted" } else { "untrusted" },
            &digest[..digest.len().min(12)],
            config.len(),
            file.display()
        );
        for (event, command) in config.entries() {
            total += 1;
            let matcher = command.matcher.as_deref().unwrap_or("*");
            let flags = if command.async_hook { " async" } else { "" };
            println!(
                "  {:<20} {:<16} {:<6} {}ms{}  {}",
                event.name(),
                matcher,
                command.on_error,
                command.timeout_ms,
                flags,
                clip(command.command.trim(), 80)
            );
        }
        if !trusted {
            println!("  (not trusted: review and grant before these run)");
        }
    }
    if documents == 0 {
        println!("no hook documents found");
        return Ok(0);
    }
    println!("{} hook(s) across {} document(s)", total, documents);
    Ok(0)
}

fn hooks_validate(files: &[PathBuf]) -> Result<i32> {
    if files.is_empty() {
        println!("no hook documents found");
        return Ok(0);
    }
    let mut failed = 0;
    for file in files {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) => {
                println!("{}: cannot read ({})", file.display(), error);
                failed += 1;
                continue;
            }
        };
        let value = match Json::parse(&text) {
            Ok(value) => value,
            Err(error) => {
                println!("{}: invalid JSON ({})", file.display(), error);
                failed += 1;
                continue;
            }
        };
        let warnings = argent_hooks::validate_json(&value);
        match HookConfig::from_json(&value) {
            Ok(config) => {
                println!("{}: ok, {} hook(s)", file.display(), config.len());
                for warning in warnings {
                    println!("  warning: {}", warning);
                }
            }
            Err(error) => {
                println!("{}: {}", file.display(), error);
                for warning in warnings {
                    println!("  warning: {}", warning);
                }
                failed += 1;
            }
        }
    }
    if failed > 0 {
        return Err(Error::config(format!(
            "{} hook document(s) failed validation",
            failed
        )));
    }
    Ok(0)
}

fn hooks_trust(action: &str, args: &[String], cwd: &Path, trust_path: &Path) -> Result<i32> {
    let mut trust = HookTrust::at(trust_path)?;
    match action {
        "status" => {
            print_hook_trust(&trust);
            Ok(0)
        }
        "review" => {
            let manager = PluginManager::for_workspace(cwd);
            let files = crate::workspace_hook_files(&manager, cwd);
            if files.is_empty() {
                println!("no hook documents found");
                return Ok(0);
            }
            for file in files {
                if !file.is_file() {
                    continue;
                }
                let digest = digest_file(&file)?;
                let hooks = HookConfig::load(&file)
                    .map(|config| config.len())
                    .unwrap_or(0);
                println!(
                    "{}  {}  {} hook(s)  {}",
                    if trust.is_trusted(&file, &digest) {
                        "trusted"
                    } else {
                        "pending"
                    },
                    &digest[..digest.len().min(12)],
                    hooks,
                    file.display()
                );
            }
            Ok(0)
        }
        "grant" => {
            let manager = PluginManager::for_workspace(cwd);
            if !args.iter().any(|arg| arg == "--all-current") {
                return Err(Error::config(
                    "usage: argent hooks trust grant --workspace <path> --all-current",
                ));
            }
            for file in crate::workspace_hook_files(&manager, cwd) {
                if !file.is_file() {
                    continue;
                }
                let digest = digest_file(&file)?;
                trust.grant(&file, &digest)?;
            }
            println!("granted {} hook bundle(s)", trust.entries().len());
            Ok(0)
        }
        "revoke" => {
            trust.revoke_all()?;
            println!("revoked every hook trust grant");
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown hooks trust command '{}'; expected status, review, grant, revoke",
            other
        ))),
    }
}

fn print_hook_trust(trust: &HookTrust) {
    println!("trust store: {}", trust.path().display());
    if trust.entries().is_empty() {
        println!("no hook grants");
    } else {
        for (hook, digest) in trust.entries() {
            println!("{}  {}", digest, hook);
        }
    }
}

// ---------------------------------------------------------------- update

fn update(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let paths = Paths::new(&cwd);
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("check");
    match action {
        "keygen" => {
            let dir = flag_value(args, "--out")
                .map(PathBuf::from)
                .unwrap_or_else(|| paths.storage().join("keys"));
            std::fs::create_dir_all(&dir).map_err(|error| {
                Error::io(format!("create {} failed: {}", dir.display(), error))
            })?;
            let key_path = argent_update::keygen(&dir)?;
            let public = argent_update::parse_public_key(&key_path.display().to_string())?;
            println!("wrote {}", key_path.display());
            println!("public key: {}", argent_core::hash::hex(&public));
            println!("Keep the private key offline; publish only the public key.");
            Ok(0)
        }
        "sign" => {
            let key = flag_value(args, "--key")
                .ok_or_else(|| Error::config("update sign needs --key <file>"))?;
            let manifest = flag_value(args, "--manifest")
                .ok_or_else(|| Error::config("update sign needs --manifest <file>"))?;
            let out = flag_value(args, "--out").map(PathBuf::from);
            let signed = argent_update::sign(Path::new(manifest), Path::new(key), out.as_deref())?;
            println!("signed {} {}", signed.version, manifest);
            Ok(0)
        }
        "check" => crate::update_cmd::run(args),
        "download" => {
            let location = positional_after(args, "download")
                .or_else(|| flag_value(args, "--manifest"))
                .ok_or_else(|| Error::config("update download needs a manifest path or URL"))?;
            let out = flag_value(args, "--out")
                .ok_or_else(|| Error::config("update download needs --out <file>"))?;
            let manifest = argent_update::load_manifest(location)?;
            let key = public_key(args)?.ok_or_else(|| {
                Error::config("update download requires --public-key (or ARGENT_UPDATE_PUBLIC_KEY)")
            })?;
            let path = argent_update::download(&manifest, &key, Path::new(out))?;
            println!("verified and wrote {}", path.display());
            Ok(0)
        }
        "apply" | "install" => {
            let location = positional_after(args, action)
                .or_else(|| flag_value(args, "--manifest"))
                .ok_or_else(|| Error::config("update apply needs a manifest path or URL"))?;
            let key = public_key(args)?.ok_or_else(|| {
                Error::config("update apply requires --public-key (or ARGENT_UPDATE_PUBLIC_KEY)")
            })?;
            let manifest = argent_update::load_manifest(location)?;
            manifest.verify(&key)?;

            // 先下载到临时目录，解压、找到可执行文件，再换掉旧的。
            let work = std::env::temp_dir().join(format!("argent-update-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&work);
            std::fs::create_dir_all(&work).map_err(|error| {
                Error::io(format!("create {} failed: {}", work.display(), error))
            })?;
            let archive = work.join("release");
            argent_update::download(&manifest, &key, &archive)?;
            let unpacked = work.join("unpacked");
            argent_update::install::extract(&archive, &unpacked)?;
            let source = argent_update::install::locate_binary(&unpacked)?;

            let target = match flag_value(args, "--bin") {
                Some(path) => PathBuf::from(path),
                None => std::env::current_exe().map_err(|error| {
                    Error::io(format!("locate the running argent failed: {}", error))
                })?,
            };
            let backup = argent_update::install::swap_binary(&source, &target)?;
            let _ = std::fs::remove_dir_all(&work);
            println!(
                "installed {} {} to {}",
                manifest.version,
                manifest.channel,
                target.display()
            );
            if let Some(backup) = backup {
                println!("the previous binary is at {}", backup.display());
                println!("run `argent update rollback` to put it back");
            }
            Ok(0)
        }
        "rollback" | "revert" => {
            let target = match flag_value(args, "--bin") {
                Some(path) => PathBuf::from(path),
                None => std::env::current_exe().map_err(|error| {
                    Error::io(format!("locate the running argent failed: {}", error))
                })?,
            };
            if argent_update::install::rollback(&target)? {
                println!("restored {}", target.display());
                Ok(0)
            } else {
                eprintln!("no backup for {}; nothing to roll back", target.display());
                Ok(1)
            }
        }
        "help" => {
            println!(
                "usage: argent update keygen [--out DIR]
       argent update sign --key <file> --manifest <file> [--out <file>]
       argent update check <manifest> [--public-key <hex|file>]
       argent update download <manifest> --out <file> --public-key <hex|file>
       argent update apply <manifest> --public-key <hex|file> [--bin <path>]
       argent update rollback [--bin <path>]"
            );
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown update command '{}'; expected keygen, sign, check, download, apply, rollback",
            other
        ))),
    }
}

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

fn positional_after<'a>(args: &'a [String], action: &str) -> Option<&'a str> {
    args.iter()
        .skip_while(|arg| arg.as_str() != action)
        .nth(1)
        .filter(|arg| !arg.starts_with('-'))
        .map(String::as_str)
}

fn public_key(args: &[String]) -> Result<Option<[u8; 32]>> {
    if let Some(value) = flag_value(args, "--public-key") {
        return Ok(Some(argent_update::parse_public_key(value)?));
    }
    if let Ok(value) = std::env::var("ARGENT_UPDATE_PUBLIC_KEY") {
        if !value.trim().is_empty() {
            return Ok(Some(argent_update::parse_public_key(&value)?));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------- sessions

/// 一行摘要：折叠空白并截断。
fn clip_line(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = compact.chars().collect();
    if chars.len() > 120 {
        let head: String = chars[..117].iter().collect();
        format!("{}...", head)
    } else {
        compact
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn temp_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "argent-search-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sessions")).expect("create");
        dir
    }

    #[test]
    fn a_query_matches_message_bodies() {
        let root = temp_root("hit");
        let sessions = root.join("sessions");
        let index = argent_session::SessionIndex::new(sessions.clone());
        let path = index.create(&root, "needle task").expect("create");
        let mut store = argent_session::SessionStore::open(path.clone()).expect("open");
        store
            .append(&argent_session::Entry::Message(argent_ai::Message::user(
                "the secret phrase is zebra crossing",
            )))
            .expect("append");
        store.close();
        let entries = argent_session::SessionStore::read_all(&path).expect("read");
        let texts: Vec<String> = entries
            .iter()
            .filter_map(|entry| match entry {
                argent_session::Entry::Message(m) => Some(m.text()),
                _ => None,
            })
            .collect();
        assert!(texts.iter().any(|t| t.contains("zebra")), "fixture sanity");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn clip_line_folds_and_truncates() {
        assert_eq!(
            clip_line(
                "a

  b   c"
            ),
            "a b c"
        );
        let long = "x".repeat(300);
        let clipped = clip_line(&long);
        assert!(clipped.ends_with("..."));
        assert!(clipped.chars().count() <= 120);
    }

    #[test]
    fn archive_hides_a_session_and_unarchive_restores_it() {
        let root = temp_root("archive");
        let sessions = root.join("sessions");
        let index = argent_session::SessionIndex::new(sessions);
        let path = index.create(&root, "archive me").expect("create");

        let archived = archive_session(&index, "archive-me").expect("archive");
        assert!(archived.is_file());
        assert!(!path.exists());
        assert!(
            index.list().expect("list").is_empty(),
            "archived fits no list row"
        );

        let restored = unarchive_session(&index, "archive-me").expect("unarchive");
        assert!(restored.is_file());
        assert_eq!(restored, path);
        assert_eq!(index.list().expect("list").len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_removes_the_session_and_its_checkpoints() {
        let root = temp_root("delete");
        let sessions = root.join("sessions");
        let index = argent_session::SessionIndex::new(sessions.clone());
        let path = index.create(&root, "delete me").expect("create");
        let stem = path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .to_string();
        let checkpoints = sessions.join("checkpoints").join(&stem);
        std::fs::create_dir_all(&checkpoints).expect("checkpoints");
        assert!(checkpoints.is_dir());

        let deleted = delete_session(&index, "delete-me").expect("delete");
        assert_eq!(deleted, path);
        assert!(!path.exists());
        assert!(!checkpoints.exists(), "checkpoints go with the session");
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// 把一个会话移进 `sessions/archive/`。
///
/// 列表只读目录顶层，归档即从日常视图消失，但文件仍在，随时可恢复。
fn archive_session(index: &argent_session::SessionIndex, reference: &str) -> Result<PathBuf> {
    let source = index.resolve(reference)?;
    let archive = index.dir().join("archive");
    std::fs::create_dir_all(&archive)
        .map_err(|e| Error::io(format!("create {} failed: {}", archive.display(), e)))?;
    let name = source
        .file_name()
        .ok_or_else(|| Error::config("session path has no file name"))?;
    let target = archive.join(name);
    std::fs::rename(&source, &target).map_err(|e| {
        Error::io(format!(
            "move {} to {} failed: {}",
            source.display(),
            target.display(),
            e
        ))
    })?;
    Ok(target)
}

/// 把归档里的会话移回 `sessions/`。
fn unarchive_session(index: &argent_session::SessionIndex, reference: &str) -> Result<PathBuf> {
    let archived = argent_session::SessionIndex::new(index.dir().join("archive"));
    let source = archived.resolve(reference)?;
    let name = source
        .file_name()
        .ok_or_else(|| Error::config("session path has no file name"))?;
    let target = index.dir().join(name);
    std::fs::rename(&source, &target).map_err(|e| {
        Error::io(format!(
            "move {} to {} failed: {}",
            source.display(),
            target.display(),
            e
        ))
    })?;
    Ok(target)
}

/// 永久删除一个会话及其检查点。
fn delete_session(index: &argent_session::SessionIndex, reference: &str) -> Result<PathBuf> {
    let target = index.resolve(reference)?;
    // 会话是唯一真相，检查点只是附带产物：先删会话，再清掉它的检查点
    // 目录，顺序反了也不会留下"会话已没、检查点还在"的半删状态。
    std::fs::remove_file(&target)
        .map_err(|e| Error::io(format!("delete {} failed: {}", target.display(), e)))?;
    argent_tools::checkpoint::remove_for_session(&target);
    Ok(target)
}

/// The commands the CLI accepts.
///
/// The completion script is generated from this list, so it cannot drift away
/// from the dispatcher above.
const COMMANDS: &[&str] = &[
    "login",
    "logout",
    "auth",
    "models",
    "mcp",
    "plugins",
    "skills",
    "commands",
    "hooks",
    "workflow",
    "telemetry",
    "sessions",
    "audit",
    "usage",
    "policy",
    "profiles",
    "themes",
    "keybindings",
    "automation",
    "eval",
    "update",
    "completion",
    "agents",
    "worktree",
    "replay",
    "attach",
    "logs",
    "follow",
    "stop",
    "rm",
    "doctor",
    "bug",
    "help",
    "version",
];

/// The second level of the commands that take one.
const SUBCOMMANDS: &[(&str, &[&str])] = &[
    (
        "sessions",
        &[
            "list",
            "search",
            "export",
            "archive",
            "unarchive",
            "delete",
            "prune",
        ],
    ),
    ("update", &["check", "apply", "rollback", "keygen", "sign"]),
    ("mcp", &["list", "add", "remove"]),
    ("worktree", &["list", "remove", "prune", "clean"]),
    ("agents", &["list", "stop", "rm"]),
    ("logs", &["follow"]),
    ("auth", &["login", "logout", "status"]),
];

/// Build the completion script for a shell.
pub fn completion_script(shell: &str) -> Result<String> {
    let commands = COMMANDS.join(" ");
    match shell {
        "bash" => {
            let mut cases = String::new();
            for (name, subs) in SUBCOMMANDS {
                cases.push_str(&format!(
                    "        {name}) COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") ) ;;\n",
                    subs.join(" ")
                ));
            }
            Ok(format!(
                r#"# argent bash completion
_argent() {{
    local cur="${{COMP_WORDS[COMP_CWORD]}}"
    local prev="${{COMP_WORDS[COMP_CWORD-1]}}"
    if [ "$COMP_CWORD" -eq 1 ]; then
        COMPREPLY=( $(compgen -W "{commands}" -- "$cur") )
        return
    fi
    case "$prev" in
{cases}        *) COMPREPLY=( $(compgen -f -- "$cur") ) ;;
    esac
}}
complete -F _argent argent"#
            ))
        }
        "zsh" => {
            let mut cases = String::new();
            for (name, subs) in SUBCOMMANDS {
                cases.push_str(&format!(
                    "        {name}) commands=({}) ;;\n",
                    subs.join(" ")
                ));
            }
            Ok(format!(
                r#"#compdef argent
_argent() {{
    local -a commands
    commands=({commands})
    if (( CURRENT == 2 )); then
        _describe 'command' commands
        return
    fi
    case "${{words[2]}}" in
{cases}        *) _files ;;
    esac
}}
_argent"#
            ))
        }
        "fish" => {
            let mut lines = Vec::new();
            for command in COMMANDS {
                lines.push(format!(
                    "complete -c argent -n '__fish_use_subcommand' -a {}",
                    command
                ));
            }
            for (name, subs) in SUBCOMMANDS {
                for sub in *subs {
                    lines.push(format!(
                        "complete -c argent -n '__fish_seen_subcommand_from {}' -a {}",
                        name, sub
                    ));
                }
            }
            Ok(lines.join("\n"))
        }
        "powershell" | "pwsh" => {
            let mut cases = String::new();
            for (name, subs) in SUBCOMMANDS {
                cases.push_str(&format!(
                    "            '{name}' {{ $subs = '{}' -split ' ' }}",
                    subs.join(" ")
                ));
            }
            Ok(format!(
                r#"Register-ArgumentCompleter -Native -CommandName argent -ScriptBlock {{
    param($wordToComplete, $commandAst, $cursorPosition)
    $subs = $null
    if ($commandAst.CommandElements.Count -gt 2) {{
        switch ($commandAst.CommandElements[1].Value) {{
{cases}
        }}
    }}
    if ($subs) {{
        $subs | Where-Object {{ $_ -like "$wordToComplete*" }} | ForEach-Object {{
            [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
        }}
        return
    }}
    $commands = '{commands}' -split ' '
    $commands | Where-Object {{ $_ -like "$wordToComplete*" }} | ForEach-Object {{
        [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_)
    }}
}}"#
            ))
        }
        other => Err(Error::config(format!(
            "unsupported shell '{}'; expected bash, zsh, fish or powershell",
            other
        ))),
    }
}

/// `argent completion <shell>`：输出补全脚本。
fn completion(args: &[String]) -> Result<i32> {
    let shell = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("bash");
    println!("{}", completion_script(shell)?);
    Ok(0)
}

/// `argent agents`：列出后台会话。
fn agents(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let json = args.iter().any(|arg| arg == "--json");
    let sessions = crate::background::list(&storage);
    if json {
        let items: Vec<Json> = sessions
            .iter()
            .map(|session| {
                let mut value = Json::obj();
                value.set("id", Json::string(session.id.clone()));
                value.set("pid", Json::num(session.pid as f64));
                value.set("prompt", Json::string(session.prompt.clone()));
                value.set("cwd", Json::string(session.cwd.clone()));
                value.set("model", Json::string(session.model.clone()));
                value.set("startedAt", Json::num(session.started_at as f64));
                value.set(
                    "running",
                    Json::bool(crate::background::process_alive(session.pid)),
                );
                value
            })
            .collect();
        println!("{}", Json::Arr(items).dump());
        return Ok(0);
    }
    if sessions.is_empty() {
        println!("no background sessions");
        return Ok(0);
    }
    for session in sessions {
        let state = if crate::background::process_alive(session.pid) {
            "running"
        } else {
            "exited"
        };
        let prompt: String = session.prompt.chars().take(60).collect();
        println!(
            "{:<24} {:<8} {:<8} {}",
            session.id, state, session.pid, prompt
        );
    }
    Ok(0)
}

/// `argent replay <file>`：离线把一份会话或事件日志渲染成 transcript。
///
/// 两种输入：
/// - 会话 JSONL（`{"kind":"message"|"header"|...}` 行）→ 消息 → transcript
/// - `--mode json` / RPC 捕获的事件流（`{"type":"event","data":{...}}` 行），
///   或裸事件 JSON（`{"type":"assistant_text",...}`）→ 逐个 apply
///
/// 输出与 TUI 退出时的回显一致（无颜色），既能当调试工具，也能在测试里
/// 断言重放结果。
fn replay(args: &[String]) -> Result<i32> {
    let path = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .ok_or_else(|| {
            Error::config("usage: argent replay <session|events.jsonl> [--to-eval <name>]")
        })?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::io(format!("read {} failed: {}", path, e)))?;
    if let Some(name) = args
        .iter()
        .position(|arg| arg == "--to-eval")
        .and_then(|i| args.get(i + 1))
    {
        let scenario = session_to_eval(&text, name)?;
        let path = format!("{}.eval.json", name);
        std::fs::write(&path, scenario.dump_pretty())
            .map_err(|e| Error::io(format!("write {} failed: {}", path, e)))?;
        println!(
            "eval scenario written to {} (run it with argent eval)",
            path
        );
        return Ok(0);
    }
    for line in replay_lines(&text)? {
        println!("{}", line);
    }
    Ok(0)
}

/// Turn a recorded session into an eval scenario (P2.1: discovery ->
/// reproduction -> regression in one command).
///
/// The first user message becomes the scenario prompt; every assistant tool
/// call becomes a scripted turn, in order; the final answer seeds
/// `text_contains` and the tool sequence seeds `expect.tools`. Files the
/// session created via `write` become the scenario's initial files, so the
/// scripted run replays against the same starting point.
fn session_to_eval(text: &str, name: &str) -> Result<Json> {
    let transcript = replay_transcript(text)?;
    let entries = transcript.entries();
    let mut prompt = String::new();
    let mut script: Vec<Json> = Vec::new();
    let mut tools: Vec<Json> = Vec::new();
    let mut files: Vec<(String, String)> = Vec::new();
    let mut final_answer = String::new();
    for entry in entries {
        match entry {
            argent_ui_core::Entry::User(text) => {
                if prompt.is_empty() {
                    prompt = text.clone();
                }
            }
            argent_ui_core::Entry::Tool(run) => {
                tools.push(Json::string(run.name.clone()));
            }
            argent_ui_core::Entry::Assistant(text) => {
                final_answer = text.clone();
            }
            _ => {}
        }
    }
    // Rebuild the scripted turns from the tool runs: name + the arguments the
    // run recorded. Write calls double as initial files.
    for entry in entries {
        if let argent_ui_core::Entry::Tool(run) = entry {
            let args = Json::parse(&run.arguments).unwrap_or_else(|_| Json::obj());
            let mut turn = Json::obj();
            turn.set("tool", Json::string(run.name.clone()));
            turn.set("arguments", args.clone());
            script.push(turn);
            if run.name == "write" {
                if let Some(path_arg) = args.get("path").and_then(Json::as_str) {
                    if let Some(content) = args.get("content").and_then(Json::as_str) {
                        files.push((path_arg.to_string(), content.to_string()));
                    }
                }
            }
        }
    }
    let mut expect = Json::obj();
    expect.set("tools", Json::Arr(tools));
    let key = final_answer.lines().next().unwrap_or("").trim().to_string();
    if !key.is_empty() {
        expect.set("text_contains", Json::Arr(vec![Json::string(key)]));
    }
    let mut scenario = Json::obj();
    scenario.set("name", Json::string(name.to_string()));
    scenario.set("prompt", Json::string(prompt));
    scenario.set("script", Json::Arr(script));
    scenario.set("expect", expect);
    if !files.is_empty() {
        let mut files_obj = Json::obj();
        for (path, content) in files {
            files_obj.set(&path, Json::string(content));
        }
        scenario.set("files", files_obj);
    }
    Ok(scenario)
}

/// 重放的纯文本结果；输入无法识别时返回错误，而不是静默输出空内容。
fn replay_transcript(text: &str) -> Result<argent_ui_core::Transcript> {
    let mut values = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value = Json::parse(line)
            .map_err(|e| Error::config(format!("line {} is not JSON: {}", index + 1, e)))?;
        values.push(value);
    }
    let first = values
        .first()
        .ok_or_else(|| Error::config("the input is empty"))?;
    let transcript = if first.get("kind").is_some() {
        let mut entries = Vec::new();
        for value in &values {
            let entry = argent_session::Entry::from_json(value).ok_or_else(|| {
                Error::config("a session line is missing its kind/message payload")
            })?;
            entries.push(entry);
        }
        let messages = argent_session::entries_to_messages(&entries);
        argent_ui_core::Transcript::from_messages(&messages)
    } else {
        let mut transcript = argent_ui_core::Transcript::new();
        for value in &values {
            match value.str_or("type", "").as_str() {
                // `--mode json` 的会话头与最终结果行不是对话内容。
                "session" | "result" => continue,
                "error" => {
                    transcript
                        .push_notice(argent_ui_core::NoticeKind::Error, value.str_or("error", ""));
                    continue;
                }
                "event" => {
                    let payload = value.get("data").cloned().unwrap_or_else(|| value.clone());
                    let event = argent_agent::event_from_json(&payload)
                        .ok_or_else(|| Error::config("an event line is not a known agent event"))?;
                    transcript.apply(&event);
                }
                // 裸事件 JSON（手写日志或别的前端）。
                _ => {
                    let event = argent_agent::event_from_json(value)
                        .ok_or_else(|| Error::config("an event line is not a known agent event"))?;
                    transcript.apply(&event);
                }
            }
        }
        transcript
    };
    Ok(transcript)
}

/// The plain-text rendering of a recorded session.
fn replay_lines(text: &str) -> Result<Vec<String>> {
    let transcript = replay_transcript(text)?;
    Ok(crate::tui::transcript_plain(&transcript))
}

/// `argent worktree`：`--worktree` 建出的工作树，列出来、删掉、清失效条目，
/// 以及批量清理已合并/空闲的工作树。
///
/// 路径约定与 `--worktree` 一致：`<storage>/worktrees/<repo>/<name>`，
/// 分支名 `argent/<name>`。
fn worktree(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let root = storage.join("worktrees");
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("list");
    let force = args.iter().any(|arg| arg == "--force" || arg == "-f");

    // 收集 (repo, name, path)。
    let mut entries: Vec<(String, String, std::path::PathBuf)> = Vec::new();
    if let Ok(repos) = std::fs::read_dir(&root) {
        for repo in repos.flatten() {
            let repo_name = repo.file_name().to_string_lossy().into_owned();
            if let Ok(items) = std::fs::read_dir(repo.path()) {
                for item in items.flatten() {
                    if item.path().is_dir() {
                        entries.push((
                            repo_name.clone(),
                            item.file_name().to_string_lossy().into_owned(),
                            item.path(),
                        ));
                    }
                }
            }
        }
    }
    entries.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));

    match action {
        "list" | "ls" => {
            if entries.is_empty() {
                println!("no worktrees under {}", root.display());
                return Ok(0);
            }
            for (repo, name, path) in &entries {
                let dirty = git_ok(path, &["status", "--porcelain"])
                    .map(|out| !out.trim().is_empty())
                    .unwrap_or(false);
                println!(
                    "{:<24} {:<18} {:<6} {}",
                    format!("{}/{}", repo, name),
                    format!("argent/{}", name),
                    if dirty { "dirty" } else { "clean" },
                    path.display()
                );
            }
            Ok(0)
        }
        "remove" | "rm" => {
            let target = args
                .iter()
                .filter(|arg| !arg.starts_with('-'))
                .nth(1)
                .ok_or_else(|| {
                    Error::config("usage: argent worktree remove <repo/name|name> [--force]")
                })?;
            let matched: Vec<_> = entries
                .iter()
                .filter(|(repo, name, _)| {
                    target == name.as_str()
                        || target == format!("{}/{}", repo, name).as_str()
                        || *name == target.trim_start_matches("argent/")
                })
                .collect();
            let (_repo, _name, path) = match matched.first() {
                Some(entry) => *entry,
                None => {
                    return Err(Error::config(format!(
                        "no worktree matches '{}'; try `argent worktree list`",
                        target
                    )))
                }
            };
            // 主仓库要从 worktree 的 .git 文件反推：目录管理器本身不是仓库。
            let main_repo = worktree_origin(path).ok_or_else(|| {
                Error::config(format!(
                    "{} does not look like a git worktree (no .git file)",
                    path.display()
                ))
            })?;
            let mut command = std::process::Command::new("git");
            command
                .arg("-C")
                .arg(&main_repo)
                .arg("worktree")
                .arg("remove");
            if force {
                command.arg("--force");
            }
            command.arg(path);
            let status = command
                .status()
                .map_err(|e| Error::io(format!("spawn git failed: {}", e)))?;
            if !status.success() {
                return Err(Error::io(format!(
                    "git worktree remove failed for {} (dirty? pass --force)",
                    path.display()
                )));
            }
            println!("removed {}", path.display());
            Ok(0)
        }
        "prune" => {
            let mut repos: Vec<std::path::PathBuf> = Vec::new();
            for (_, _, path) in &entries {
                if let Some(main) = worktree_origin(path) {
                    if !repos.contains(&main) {
                        repos.push(main);
                    }
                }
            }
            let mut pruned = 0usize;
            for repo in &repos {
                if git_ok(repo, &["worktree", "prune"]).is_some() {
                    pruned += 1;
                }
            }
            println!("pruned worktree metadata for {} repositories", pruned);
            Ok(0)
        }
        "clean" => {
            // 批量清理：分支已合并进默认分支，或空闲超过 N 天的工作树。
            // 默认只列候选（dry run），`--yes` 才真删；dirty 的要再给
            // `--force`，避免把没提交的改动一起删掉。
            let idle_days = args
                .iter()
                .find_map(|arg| arg.strip_prefix("--idle-days="))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(7);
            let confirm = args.iter().any(|arg| arg == "--yes" || arg == "-y");
            let force = args.iter().any(|arg| arg == "--force" || arg == "-f");
            if entries.is_empty() {
                println!("no worktrees under {}", root.display());
                return Ok(0);
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            let cutoff = now.saturating_sub(idle_days.saturating_mul(24 * 3600));

            struct Candidate {
                repo: String,
                name: String,
                path: std::path::PathBuf,
                main_repo: std::path::PathBuf,
                branch: String,
                dirty: bool,
                reason: &'static str,
            }

            let mut candidates: Vec<Candidate> = Vec::new();
            for (repo, name, path) in &entries {
                // 读不到状态就当作 dirty：宁可漏删，不可误删。
                let dirty = git_ok(path, &["status", "--porcelain"])
                    .map(|out| !out.trim().is_empty())
                    .unwrap_or(true);
                let Some(main_repo) = worktree_origin(path) else {
                    continue;
                };
                let branch = format!("argent/{}", name);
                let merged = default_branch(&main_repo)
                    .map(|main| branch_is_ancestor(&main_repo, &branch, &main))
                    .unwrap_or(false);
                let last = last_commit_time(path).unwrap_or(0);
                let idle = last > 0 && last < cutoff;
                if !merged && !idle {
                    continue;
                }
                candidates.push(Candidate {
                    repo: repo.clone(),
                    name: name.clone(),
                    path: path.clone(),
                    main_repo,
                    branch,
                    dirty,
                    reason: if merged { "merged" } else { "idle" },
                });
            }

            if candidates.is_empty() {
                println!(
                    "nothing to clean (no merged worktree, nothing idle for {} day(s))",
                    idle_days
                );
                return Ok(0);
            }

            let mut removed = 0usize;
            for candidate in &candidates {
                let label = format!("{}/{}", candidate.repo, candidate.name);
                if candidate.dirty && !force {
                    println!(
                        "skip     {} ({}, dirty — pass --force to remove)",
                        label, candidate.reason
                    );
                    continue;
                }
                if !confirm {
                    println!(
                        "would rm {} ({}{})",
                        label,
                        candidate.reason,
                        if candidate.dirty { ", dirty" } else { "" }
                    );
                    continue;
                }
                let mut command = std::process::Command::new("git");
                command
                    .arg("-C")
                    .arg(&candidate.main_repo)
                    .arg("worktree")
                    .arg("remove");
                if candidate.dirty {
                    command.arg("--force");
                }
                command.arg(&candidate.path);
                match command.status() {
                    Ok(status) if status.success() => {}
                    _ => {
                        println!("failed   {} (git worktree remove)", label);
                        continue;
                    }
                }
                // 只有已合并的分支才删；`-d` 对未合并分支会拒绝，正好当保险。
                if candidate.reason == "merged" {
                    let _ = git_ok(&candidate.main_repo, &["branch", "-d", &candidate.branch]);
                }
                println!("removed  {} ({})", label, candidate.reason);
                removed += 1;
            }

            if confirm {
                println!("removed {} worktree(s)", removed);
            } else {
                let blocked = candidates.iter().filter(|c| c.dirty && !force).count();
                println!(
                    "dry run: {} removable, {} need --force; pass --yes to remove",
                    candidates.len() - blocked,
                    blocked
                );
            }
            Ok(0)
        }
        "help" => {
            println!(
                "usage: argent worktree list
       argent worktree remove <repo/name|name> [--force]
       argent worktree prune
       argent worktree clean [--idle-days=N] [--yes] [--force]"
            );
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown worktree command '{}'; expected list, remove, prune or clean",
            other
        ))),
    }
}

/// 主仓库的默认分支名：优先 `origin/HEAD`，其次本地 `main` / `master`。
fn default_branch(repo: &std::path::Path) -> Option<String> {
    if let Some(out) = git_ok(
        repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    ) {
        let name = out.trim().trim_start_matches("origin/").to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    for name in ["main", "master"] {
        if git_ok(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{}", name),
            ],
        )
        .is_some()
        {
            return Some(name.to_string());
        }
    }
    None
}

/// `branch` 的提交是否已全部包含在 `main` 里（可以安全删分支）。
fn branch_is_ancestor(repo: &std::path::Path, branch: &str, main: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", branch, main])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 工作树里最后一次提交的 Unix 时间；还没有提交时返回 None。
fn last_commit_time(dir: &std::path::Path) -> Option<u64> {
    git_ok(dir, &["log", "-1", "--format=%ct"])?
        .trim()
        .parse()
        .ok()
}

/// 从 worktree 的 `.git` 文件反推主仓库根目录。
///
/// worktree 里的 `.git` 是文件，内容是 `gitdir: <repo>/.git/worktrees/<name>`。
fn worktree_origin(worktree: &std::path::Path) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let line = text.lines().next()?;
    let gitdir = line.strip_prefix("gitdir:")?.trim();
    let gitdir = std::path::Path::new(gitdir);
    // <repo>/.git/worktrees/<name> -> <repo>
    gitdir
        .parent()?
        .parent()?
        .parent()
        .map(|path| path.to_path_buf())
}

/// 在给定目录跑 git，返回 stdout；非零退出或起不来时返回 None。
fn git_ok(dir: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 后台子命令共用的位置参数解析：跳过 `-C <dir>` 的值。
fn background_target(args: &[String]) -> Option<String> {
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
            continue;
        }
        if arg == "-C" || arg == "--cwd" {
            skip = true;
            continue;
        }
        if !arg.starts_with('-') {
            return Some(arg.clone());
        }
    }
    None
}

fn background_logs(args: &[String], follow_default: bool) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let follow = follow_default || args.iter().any(|arg| arg == "--follow" || arg == "-f");
    let id = background_target(args)
        .ok_or_else(|| Error::config("usage: argent logs <id> [--follow]"))?;
    let session = crate::background::load(&storage, &id)?;
    crate::background::print_logs(&storage, &session, follow)?;
    Ok(0)
}

fn background_stop(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let id = background_target(args).ok_or_else(|| Error::config("usage: argent stop <id>"))?;
    let session = crate::background::load(&storage, &id)?;
    crate::background::stop(&session)?;
    println!("stopped {}", id);
    Ok(0)
}

fn background_remove(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let id = background_target(args).ok_or_else(|| Error::config("usage: argent rm <id>"))?;
    let session = crate::background::load(&storage, &id)?;
    crate::background::remove(&storage, &session)?;
    println!("removed {}", id);
    Ok(0)
}

/// `argent attach <id>`：接管后台会话（看事件、插话、中止）。
///
/// 默认进终端界面；`--plain` 只把事件流打印出来。
fn attach_session(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let storage = Paths::new(&cwd).storage().to_path_buf();
    let id = background_target(args)
        .ok_or_else(|| Error::config("usage: argent attach <id> [--plain]"))?;
    let session = crate::background::load(&storage, &id)?;
    let plain = args.iter().any(|arg| arg == "--plain");
    crate::attach_session::attach(&storage, &session, plain)
}

/// `argent keybindings`：列出当前的手势绑定。
fn keybindings(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let bindings = crate::keybindings::Bindings::load(&cwd)?;
    for (action, keys) in bindings.list() {
        let names: Vec<String> = keys.iter().map(crate::keybindings::key_name).collect();
        println!("{:<18} {}", action.name(), names.join(", "));
    }
    Ok(0)
}

/// `argent themes`：列出可用主题与当前选择。
fn themes(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let names = crate::theme::names(&cwd);
    let current = crate::theme::current();
    for name in names {
        let marker = if name == current.name { "*" } else { " " };
        println!("{} {}", marker, name);
    }
    Ok(0)
}

fn sessions(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let paths = Paths::new(&cwd);
    let index = argent_session::SessionIndex::new(paths.sessions());
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("list");
    match action {
        "list" | "ls" => {
            let _ = argent_session::sweep_empty_sessions(&index, &[], std::time::Duration::from_secs(60));
            println!("(usage: argent sessions list | search <query> | export --html <out> [session] | archive|unarchive|delete <session> | prune --dry-run)");
            let list = index.list()?;
            if list.is_empty() {
                println!("no sessions in {}", index.dir().display());
            }
            for (path, info) in list {
                println!(
                    "{}  {}  {}",
                    argent_session::local_stamp(info.num_or("modified", 0.0) as u64),
                    info.str_or("title", "(untitled)"),
                    path.display()
                );
            }
            Ok(0)
        }
        "search" | "grep" => {
            let query = match args.get(1) {
                Some(query) => query.to_lowercase(),
                None => {
                    return Err(Error::config("usage: argent sessions search <query>"));
                }
            };
            let limit = args
                .iter()
                .position(|arg| arg == "--limit")
                .and_then(|index| args.get(index + 1))
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(20);
            // 按内容搜：会话标题只能记住任务名，找"那天改过的那段话"要靠
            // 消息正文。JSONL 本来就是纯文本，不需要索引就能扫。
            let mut hits = 0usize;
            for (path, info) in index.list()? {
                let entries = argent_session::SessionStore::read_all(&path)?;
                let mut count = 0usize;
                let mut samples: Vec<String> = Vec::new();
                for entry in &entries {
                    if let argent_session::Entry::Message(message) = entry {
                        let text = message.text();
                        if text.is_empty() {
                            continue;
                        }
                        let lowered = text.to_lowercase();
                        if let Some(position) = lowered.find(&query) {
                            count += 1;
                            if samples.len() < 2 {
                                let start = position.saturating_sub(48);
                                let end = (position + query.len() + 48).min(text.len());
                                let snippet =
                                    text[start..end].replace('\n', " ").trim().to_string();
                                let role = match message.role {
                                    argent_ai::Role::User => "user",
                                    argent_ai::Role::Assistant => "assistant",
                                    _ => "other",
                                };
                                samples.push(format!("{}: {}", role, snippet));
                            }
                        }
                    }
                }
                if count > 0 {
                    hits += 1;
                    println!(
                        "{}  {}  ({} match{})",
                        argent_session::local_stamp(info.num_or("modified", 0.0) as u64),
                        info.str_or("title", "(untitled)"),
                        count,
                        if count == 1 { "" } else { "es" }
                    );
                    println!("    {}", path.display());
                    for sample in &samples {
                        println!("    {}",
                            clip_line(sample));
                    }
                    if hits >= limit {
                        println!("(stopped at {} sessions; narrow the query)", hits);
                        break;
                    }
                }
            }
            if hits == 0 {
                println!("no session mentions '{}'", query);
            }
            Ok(0)
        }
        "export" => {
            if let Some(position) = args.iter().position(|arg| arg == "--html") {
                let destination = args
                    .get(position + 1)
                    .map(PathBuf::from)
                    .ok_or_else(|| Error::config("sessions export --html needs a file"))?;
                let reference = args.get(position + 2).map(String::as_str);
                return export_html(&index, &destination, reference);
            }
            let sqlite = args
                .iter()
                .position(|arg| arg == "--sqlite")
                .and_then(|index| args.get(index + 1))
                .map(PathBuf::from)
                .unwrap_or_else(|| paths.storage().join("argent.sqlite3"));
            export_sqlite(&index, &sqlite)
        }
        "archive" => {
            let reference = args
                .get(1)
                .ok_or_else(|| Error::config("usage: argent sessions archive <session>"))?;
            let target = archive_session(&index, reference)?;
            println!("archived {}", target.display());
            Ok(0)
        }
        "unarchive" => {
            let reference = args
                .get(1)
                .ok_or_else(|| Error::config("usage: argent sessions unarchive <session>"))?;
            let target = unarchive_session(&index, reference)?;
            println!("restored {}", target.display());
            Ok(0)
        }
        "delete" | "rm" => {
            let reference = args
                .get(1)
                .ok_or_else(|| Error::config("usage: argent sessions delete <session>"))?;
            let target = delete_session(&index, reference)?;
            println!("deleted {}", target.display());
            Ok(0)
        }
        "prune" => {
            let dry_run = args.iter().any(|arg| arg == "--dry-run");
            let report = argent_tools::checkpoint::collect_garbage(&paths.sessions(), dry_run)?;
            if report.is_empty() {
                println!("no orphaned checkpoints under {}", paths.sessions().display());
                return Ok(0);
            }
            println!(
                "{} {} orphan checkpoint directories and {} unreferenced blobs ({} bytes)",
                if dry_run { "would remove" } else { "removed" },
                report.orphan_dirs.len(),
                report.orphan_blobs.len(),
                report.freed_bytes
            );
            for dir in &report.orphan_dirs {
                println!("  dir  {}", dir.display());
            }
            for blob in &report.orphan_blobs {
                println!("  blob {}", blob.display());
            }
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown sessions command '{}'; expected list, search, export, archive, unarchive, delete or prune",
            other
        ))),
    }
}

// ------------------------------------------------------------------ usage

/// Token and spend totals from the usage ledger.
fn usage(args: &[String]) -> Result<i32> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let days = flag_value(args, "--days")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(7);
    let since = if args.iter().any(|arg| arg == "--all") {
        0
    } else {
        now.saturating_sub(days.saturating_mul(86_400))
    };
    let session_filter = flag_value(args, "--session").map(|value| value.to_ascii_lowercase());
    let model_filter = flag_value(args, "--model").map(|value| value.to_ascii_lowercase());
    let records: Vec<argent_session::usage::UsageRecord> = argent_session::usage::read()
        .into_iter()
        .filter(|record| record.epoch >= since)
        .filter(|record| {
            session_filter
                .as_deref()
                .map(|needle| record.session.to_ascii_lowercase().contains(needle))
                .unwrap_or(true)
        })
        .filter(|record| {
            model_filter
                .as_deref()
                .map(|needle| record.model.to_ascii_lowercase().contains(needle))
                .unwrap_or(true)
        })
        .collect();
    let summary = argent_session::usage::summary(&records, since);
    if args.iter().any(|arg| arg == "--json") {
        let mut data = Json::obj();
        data.set("turns", Json::num(summary.turns as f64));
        data.set("prompt", Json::num(summary.prompt_tokens as f64));
        data.set("completion", Json::num(summary.completion_tokens as f64));
        data.set("cached", Json::num(summary.cached_tokens as f64));
        data.set("missed", Json::num(summary.missed_tokens as f64));
        data.set(
            "cache_hit_percent",
            Json::num(summary.cache_hit_percent() as f64),
        );
        data.set("requests", Json::num(summary.requests as f64));
        data.set("ttft_samples", Json::num(summary.ttft_samples as f64));
        if let Some(ttft) = summary.avg_ttft_ms() {
            data.set("avg_ttft_ms", Json::num(ttft as f64));
        }
        data.set("cost", Json::num(summary.cost));
        let mut models = Json::obj();
        for (name, item) in &summary.by_model {
            let mut entry = Json::obj();
            entry.set("turns", Json::num(item.turns as f64));
            entry.set("prompt", Json::num(item.prompt_tokens as f64));
            entry.set("completion", Json::num(item.completion_tokens as f64));
            entry.set("cached", Json::num(item.cached_tokens as f64));
            entry.set("missed", Json::num(item.missed_tokens as f64));
            entry.set("cost", Json::num(item.cost));
            models.set(name, entry);
        }
        data.set("models", models);
        println!("{}", data.dump_pretty());
        return Ok(0);
    }
    if summary.turns == 0 {
        println!(
            "no usage records in {} in the last {} day(s)",
            argent_session::usage::usage_path().display(),
            days
        );
        return Ok(0);
    }
    let since_label = if since == 0 {
        "all time".to_string()
    } else {
        format!("since {}", argent_session::local_stamp(since))
    };
    let hit = summary.cache_hit_percent();
    println!(
        "usage: {} turn(s), {} request(s), {}",
        summary.turns, summary.requests, since_label
    );
    let ttft = summary
        .avg_ttft_ms()
        .map(|ms| format!(" · ttft {:.1}s avg", ms as f64 / 1000.0))
        .unwrap_or_default();
    println!(
        "prompt {} · completion {} · cached {} ({}%) · missed {} · ${:.4}{}",
        short_tokens(summary.prompt_tokens),
        short_tokens(summary.completion_tokens),
        short_tokens(summary.cached_tokens),
        hit,
        short_tokens(summary.missed_tokens),
        summary.cost,
        ttft
    );
    if !summary.by_model.is_empty() {
        println!("by model:");
        let mut models: Vec<_> = summary.by_model.iter().collect();
        models.sort_by(|a, b| {
            b.1.cost
                .partial_cmp(&a.1.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (name, item) in models.iter().take(10) {
            let model_hit = item.cache_hit_percent();
            println!(
                "  {:<36} {:>4} turns  {:>8} in  {:>8} out  cache {:>3}%  missed {:>8}  ${:.4}",
                clip(name, 36),
                item.turns,
                short_tokens(item.prompt_tokens),
                short_tokens(item.completion_tokens),
                model_hit,
                short_tokens(item.missed_tokens),
                item.cost
            );
        }
    }
    if !summary.by_session.is_empty() {
        println!("by session:");
        let mut sessions: Vec<_> = summary.by_session.iter().collect();
        sessions.sort_by(|a, b| {
            b.1.cost
                .partial_cmp(&a.1.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (path, item) in sessions.iter().take(10) {
            println!(
                "  {:<58} {:>4} turns  ${:.4}",
                clip(path, 58),
                item.turns,
                item.cost
            );
        }
    }
    Ok(0)
}

fn short_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ------------------------------------------------------------------ audit

/// Show the audit trail: every tool call, its decision and its arguments.
///
/// The arguments are already redacted on the way in, so this is safe to paste
/// into a bug report.
fn audit(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let paths = Paths::new(&cwd);
    let log = argent_security::AuditLog::new(paths.storage());
    let entries = argent_security::AuditLog::read(log.path());
    if entries.is_empty() {
        println!("no audit entries in {}", log.path().display());
        return Ok(0);
    }
    let limit = args
        .iter()
        .position(|arg| arg == "--limit")
        .and_then(|index| args.get(index + 1))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(50);
    let start = entries.len().saturating_sub(limit);
    for entry in &entries[start..] {
        let tool = entry.str_or("tool", "?");
        let decision = entry.str_or("decision", "?");
        let stamp = entry.str_or("timestamp", "?");
        let arguments = entry.str_or("arguments", "");
        let reason = entry.str_or("reason", "");
        if reason.is_empty() {
            println!("{}  {:<12} {:<8} {}", stamp, tool, decision, arguments);
        } else {
            println!(
                "{}  {:<12} {:<8} {}  ({})",
                stamp, tool, decision, arguments, reason
            );
        }
    }
    Ok(0)
}

// --------------------------------------------------------------- automation

/// Scheduled tasks: list, add, remove, enable/disable, run, logs, daemon.
fn automation(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let mut store = argent_automation::AutomationStore::for_workspace(&cwd)?;
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("list");
    let now = argent_automation::now();
    match action {
        "list" | "ls" => {
            if store.list().is_empty() {
                println!(
                    "no automations in {}; add one with `argent automation add --name ... --every 1h --prompt ...`",
                    store.path().display()
                );
            }
            for task in store.list() {
                println!("{}", task.describe(now));
            }
            Ok(0)
        }
        "add" => {
            let name = flag_value(args, "--name")
                .ok_or_else(|| Error::config("automation add needs --name"))?;
            let schedule_text = flag_value(args, "--every")
                .map(|value| format!("every {}", value))
                .or_else(|| flag_value(args, "--daily").map(|value| format!("daily {}", value)))
                .or_else(|| flag_value(args, "--weekly").map(|value| format!("weekly {}", value)))
                .ok_or_else(|| {
                    Error::config("automation add needs --every, --daily or --weekly")
                })?;
            let schedule = argent_automation::Schedule::parse(&schedule_text)?;
            let action = match (flag_value(args, "--prompt"), flag_value(args, "--workflow")) {
                (Some(prompt), None) => argent_automation::Action::Prompt(prompt.to_string()),
                (None, Some(path)) => argent_automation::Action::Workflow(PathBuf::from(path)),
                _ => {
                    return Err(Error::config(
                        "automation add needs exactly one of --prompt or --workflow",
                    ))
                }
            };
            let mut task = argent_automation::Automation::new(name, schedule, action);
            task.profile = flag_value(args, "--profile").map(str::to_string);
            task.cwd = flag_value(args, "--cwd").map(PathBuf::from);
            if let Some(secs) = flag_value(args, "--max-runtime").and_then(|v| v.parse::<u64>().ok()) {
                task.max_runtime_secs = secs.max(1);
            }
            task.enabled = !args.iter().any(|arg| arg == "--disabled");
            let id = task.id.clone();
            store.upsert(task);
            store.save()?;
            println!(
                "{} saved; next run {}",
                id,
                argent_automation::format_iso(
                    store
                        .get(&id)
                        .map(|task| task.next_run(now))
                        .unwrap_or(now)
                )
            );
            Ok(0)
        }
        "remove" | "rm" => {
            let id = automation_id(args)?;
            if !store.remove(&id) {
                return Err(Error::config(format!("no automation '{}'", id)));
            }
            store.save()?;
            println!("{} removed", id);
            Ok(0)
        }
        "enable" | "disable" => {
            let id = automation_id(args)?;
            let enabled = action == "enable";
            let task = store
                .get_mut(&id)
                .ok_or_else(|| Error::config(format!("no automation '{}'", id)))?;
            task.enabled = enabled;
            let next = task.next_run(now);
            store.save()?;
            println!(
                "{} {}; next run {}",
                id,
                if enabled { "enabled" } else { "disabled" },
                argent_automation::format_iso(next)
            );
            Ok(0)
        }
        "run" => {
            let id = automation_id(args)?;
            let task = store
                .get(&id)
                .cloned()
                .ok_or_else(|| Error::config(format!("no automation '{}'", id)))?;
            let result = argent_automation::run(&task, &mut store, now)?;
            println!(
                "{} {} in {}s; log {}",
                result.id,
                result.status,
                result.seconds,
                result.log.display()
            );
            Ok(if result.success() { 0 } else { 1 })
        }
        "logs" => {
            let id = automation_id(args)?;
            let task = store
                .get(&id)
                .ok_or_else(|| Error::config(format!("no automation '{}'", id)))?;
            let Some(log) = &task.last_log else {
                println!("{} has not run yet", id);
                return Ok(0);
            };
            let text = argent_core::read_text_file(log)?;
            let lines: Vec<&str> = text.lines().collect();
            let limit: usize = flag_value(args, "--limit")
                .and_then(|value| value.parse().ok())
                .unwrap_or(80);
            let start = lines.len().saturating_sub(limit);
            for line in &lines[start..] {
                println!("{}", line);
            }
            Ok(0)
        }
        "daemon" => {
            let once = args.iter().any(|arg| arg == "--once");
            let interval: u64 = flag_value(args, "--interval")
                .and_then(|value| value.parse().ok())
                .unwrap_or(30)
                .max(1);
            loop {
                let now = argent_automation::now();
                let results = argent_automation::run_due(&mut store, now);
                for result in results {
                    println!(
                        "{} {} in {}s; log {}",
                        result.id,
                        result.status,
                        result.seconds,
                        result.log.display()
                    );
                }
                if once || argent_core::interrupt::is_interrupted() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(interval));
            }
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown automation command '{}'; expected list, add, remove, enable, disable, run, logs or daemon",
            other
        ))),
    }
}

/// The id argument of an automation subcommand.
fn automation_id(args: &[String]) -> Result<String> {
    args.iter()
        .skip(1)
        .find(|arg| !arg.starts_with('-'))
        .cloned()
        .ok_or_else(|| Error::config("this command needs an automation id"))
}

// ------------------------------------------------------------------- eval

/// Run an evaluation suite.
///
/// The model is scripted, so the run is deterministic and free. The tools are
/// real, so a broken tool fails the eval. The exit code is the number of
/// failures, so CI can use it directly.
fn eval(args: &[String]) -> Result<i32> {
    let path = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .cloned()
        .ok_or_else(|| Error::config("usage: argent eval <suite.json>"))?;
    let text = argent_core::read_text_file(std::path::Path::new(&path))?;
    let value =
        Json::parse(&text).map_err(|e| Error::config(format!("invalid suite {}: {}", path, e)))?;
    // Accept both shapes: a suite object with a `scenarios` array, and the
    // single-scenario shape that `replay --to-eval` produces.
    let scenarios = if value.get("scenarios").and_then(Json::as_array).is_some() {
        argent_eval::Scenario::suite_from_json(&value)?
    } else {
        vec![argent_eval::Scenario::from_json(&value)?]
    };
    if scenarios.is_empty() {
        println!("the suite has no scenarios");
        return Ok(0);
    }
    let scratch = std::env::temp_dir().join(format!("argent-eval-{}", std::process::id()));
    let outcomes = argent_eval::run_suite(&scenarios, &scratch);
    let mut failures = 0;
    for outcome in &outcomes {
        println!("{}", outcome.line());
        if !outcome.passed {
            failures += 1;
        }
    }
    println!(
        "\n{} of {} scenarios passed",
        outcomes.len() - failures,
        outcomes.len()
    );
    Ok(failures.min(1) as i32)
}

// ----------------------------------------------------------------- policy

/// List the profiles visible from a workspace.
fn profiles(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let settings = Settings::load(&cwd)?;
    let registry = argent_profiles::ProfileRegistry::load(&cwd, &settings)?;
    if registry.is_empty() {
        println!(
            "no profiles; add .argent/profiles/<name>.md or a 'profiles' block in argent.json"
        );
        return Ok(0);
    }
    for profile in registry.list() {
        let model = profile.model.as_deref().unwrap_or("-");
        let source = profile
            .source
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "settings".to_string());
        println!(
            "{:<16} model={:<28} {}\n  {}",
            profile.name, model, source, profile.description
        );
    }
    Ok(0)
}

/// Show the effective permission policy.
fn policy(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let settings = Settings::load(&cwd)?;
    let policy = argent_security::Policy::from_settings(&settings);
    if policy.is_empty() {
        println!("no permission rules; every tool is allowed by policy");
        return Ok(0);
    }
    println!("deny paths:");
    for rule in &policy.deny_paths {
        println!("  {rule}");
    }
    if !policy.allow_paths.is_empty() {
        println!("allow paths (everything else is denied):");
        for rule in &policy.allow_paths {
            println!("  {rule}");
        }
    }
    println!("deny commands:");
    for rule in &policy.deny_commands {
        println!("  {rule}");
    }
    println!("deny command paths:");
    for rule in &policy.deny_command_paths {
        println!("  {rule}");
    }
    println!("deny tools:");
    for rule in &policy.deny_tools {
        println!("  {rule}");
    }
    Ok(0)
}

/// Export one transcript as a standalone HTML file.
fn export_html(
    index: &argent_session::SessionIndex,
    destination: &Path,
    reference: Option<&str>,
) -> Result<i32> {
    let path = match reference {
        Some(reference) => index.resolve(reference)?,
        None => index
            .latest()?
            .ok_or_else(|| Error::config("no sessions to export"))?,
    };
    let entries = argent_session::SessionStore::read_all(&path)?;
    let messages = argent_session::entries_to_messages(&entries);
    let rows = index.list().unwrap_or_default();
    let (title, cwd) = rows
        .iter()
        .find(|(candidate, _)| candidate == &path)
        .map(|(_, info)| {
            (
                info.str_or("title", "argent transcript"),
                info.str_or("cwd", ""),
            )
        })
        .unwrap_or_else(|| ("argent transcript".to_string(), String::new()));
    let html = export::render(
        &ExportMeta {
            model: "",
            cwd: &cwd,
            title: &title,
        },
        &messages,
    );
    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::io(format!("create {} failed: {}", parent.display(), error))
            })?;
        }
    }
    std::fs::write(destination, html)
        .map_err(|error| Error::io(format!("write {} failed: {}", destination.display(), error)))?;
    println!(
        "exported {} message(s) to {}",
        messages.len(),
        destination.display()
    );
    Ok(0)
}

fn export_sqlite(index: &argent_session::SessionIndex, destination: &Path) -> Result<i32> {
    let mut script = String::new();
    script.push_str(
        "PRAGMA journal_mode=WAL;
",
    );
    script.push_str("CREATE TABLE IF NOT EXISTS sessions (path TEXT PRIMARY KEY, title TEXT, cwd TEXT, updated TEXT);
");
    script.push_str("CREATE TABLE IF NOT EXISTS entries (session TEXT, id TEXT, parent TEXT, kind TEXT, json TEXT);
");
    let mut sessions = 0usize;
    let mut entries = 0usize;
    for (path, info) in index.list()? {
        let session = sql_text(&path.display().to_string());
        let title = sql_text(&info.str_or("title", ""));
        let cwd = sql_text(&info.str_or("cwd", ""));
        let updated = sql_text(&info.str_or("modified_iso", ""));
        script.push_str(&format!(
            "DELETE FROM sessions WHERE path='{}';
DELETE FROM entries WHERE session='{}';
",
            session, session
        ));
        script.push_str(&format!(
            "INSERT INTO sessions VALUES ('{}','{}','{}','{}');
",
            session, title, cwd, updated
        ));
        let tree = argent_session::SessionStore::read_tree(&path)?;
        for id in tree.ids() {
            let Some(node) = tree.get(id) else {
                continue;
            };
            let payload = node.payload.to_json();
            let kind = payload.str_or("kind", "");
            script.push_str(&format!(
                "INSERT INTO entries VALUES ('{}','{}','{}','{}','{}');
",
                session,
                sql_text(id),
                sql_text(node.parent.as_deref().unwrap_or("")),
                sql_text(&kind),
                sql_text(&payload.dump())
            ));
            entries += 1;
        }
        sessions += 1;
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::io(format!("create {} failed: {}", parent.display(), error)))?;
    }
    let _ = std::fs::write(destination.with_extension("sql"), &script);
    let output = argent_core::process::run(
        &argent_core::process::CommandSpec::new("sqlite3")
            .arg(destination.to_string_lossy().into_owned())
            .stdin(&script)
            .timeout(std::time::Duration::from_secs(60)),
    )?;
    if !output.success() {
        return Err(Error::io(format!(
            "sqlite3 failed: {}{}",
            output.stderr.trim(),
            output.stdout.trim()
        )));
    }
    println!(
        "exported {} session(s), {} entr(ies) to {}",
        sessions,
        entries,
        destination.display()
    );
    Ok(0)
}

fn sql_text(value: &str) -> String {
    value.replace('\'', "''")
}

// ---------------------------------------------------------------- telemetry

fn telemetry(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let settings = Settings::load(&cwd)?;
    let writer = argent_telemetry::Telemetry::from_settings(&settings, &cwd);
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("status");
    match action {
        "status" => {
            println!(
                "telemetry: {}",
                if writer.enabled() {
                    "enabled (local)"
                } else {
                    "disabled"
                }
            );
            println!("events: {}", writer.events_path().display());
            match writer.endpoint() {
                Some(endpoint) => println!("endpoint: {}", endpoint),
                None => println!("endpoint: (not configured)"),
            }
            println!(
                "remote upload: {}",
                if !writer.enabled() {
                    "off (telemetry is disabled)"
                } else if writer.remote() {
                    "enabled"
                } else if writer.endpoint().is_some() {
                    "off (needs consent: `argent telemetry enable --consent`)"
                } else {
                    "off (needs an endpoint and consent)"
                }
            );
            Ok(0)
        }
        "enable" | "on" => {
            let endpoint = flag_value(args, "--endpoint").map(str::to_string);
            let consent = args.iter().any(|arg| arg == "--consent");
            // 只开本地时不写对象，保持和老配置一样的形状。
            if endpoint.is_none() && !consent {
                settings.set_user("telemetry", Json::bool(true))?;
            } else {
                let mut value = Json::obj();
                value.set("enabled", Json::bool(true));
                if let Some(endpoint) = &endpoint {
                    value.set("endpoint", Json::string(endpoint.clone()));
                }
                value.set("consent", Json::bool(consent));
                settings.set_user("telemetry", value)?;
            }
            println!(
                "telemetry enabled; events stay in {}",
                writer.events_path().display()
            );
            if let Some(endpoint) = &endpoint {
                println!("events will also be posted to {}", endpoint);
            }
            if !consent {
                println!("remote upload stays off until `argent telemetry enable --consent`");
            }
            Ok(0)
        }
        "disable" | "off" => {
            // 保留已配置的端点与同意状态，重新打开时不用再配一遍。
            let mut value = Json::obj();
            value.set("enabled", Json::bool(false));
            if let Some(existing) = settings.get("telemetry") {
                if let Some(endpoint) = existing.get("endpoint").and_then(Json::as_str) {
                    value.set("endpoint", Json::string(endpoint.to_string()));
                }
                if let Some(consent) = existing.get("consent").and_then(Json::as_bool) {
                    value.set("consent", Json::bool(consent));
                }
            }
            settings.set_user("telemetry", value)?;
            println!("telemetry disabled");
            Ok(0)
        }
        other => Err(Error::config(format!(
            "unknown telemetry command '{}'; expected status, enable, disable",
            other
        ))),
    }
}

// ---------------------------------------------------------------- workflow

fn workflow(args: &[String]) -> Result<i32> {
    let action = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("help");
    match action {
        "validate" => {
            let path = args
                .iter()
                .skip_while(|arg| arg.as_str() != "validate")
                .nth(1);
            let Some(path) = path else {
                return Err(Error::config("usage: argent workflow validate <file>"));
            };
            let workflow = argent_workflow::Workflow::load(Path::new(path))?;
            println!(
                "{} is valid: {} step(s), concurrency {}",
                workflow.name,
                workflow.steps.len(),
                workflow.max_concurrency
            );
            for step in &workflow.steps {
                let deps = if step.depends_on.is_empty() {
                    String::new()
                } else {
                    format!(" <- {}", step.depends_on.join(", "))
                };
                println!("  {:<16} {}{}", step.id, step.role, deps);
            }
            Ok(0)
        }
        "run" => {
            println!("workflows run through the agent: argent --workflow <file> [--resume]");
            println!("declarative .dwf.ts files are accepted too (no JavaScript is executed)");
            Ok(0)
        }
        _ => {
            println!(
                "usage: argent workflow validate <file>
       argent --workflow <file> [--workflow-journal <path>] [--resume]"
            );
            Ok(0)
        }
    }
}

// ---------------------------------------------------------------- helpers

/// `argent bug`: sanitized local report. No network.
fn bug_report(args: &[String]) -> Result<i32> {
    let cwd = cwd_from(args);
    let mut config = argent_core::config::Config::load(&cwd)?;
    crate::mainx::settings::apply_settings(&mut config, &crate::args::Cli::default(), None)?;
    let path = crate::mainx::bug_report::write_report(&config, &cwd)?;
    println!("bug report written to {}", path.display());
    println!("attach it manually if you file an issue; it never leaves your machine");
    Ok(0)
}

pub(crate) fn cwd_from(args: &[String]) -> PathBuf {
    let mut index = 0;
    while index < args.len() {
        if (args[index] == "--cwd" || args[index] == "-C") && index + 1 < args.len() {
            return PathBuf::from(&args[index + 1]);
        }
        index += 1;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 参数里是否请求了 `--help` / `-h`。子命令用它先打印用法再退出。
pub(crate) fn wants_help(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--help" || arg == "-h")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shell_gets_a_script() {
        for shell in ["bash", "zsh", "fish", "powershell", "pwsh"] {
            let script = completion_script(shell).expect("script");
            assert!(script.contains("argent"), "{}: {}", shell, script);
            assert!(script.len() > 50, "{}: too short", shell);
        }
    }

    #[test]
    fn the_top_level_commands_are_offered() {
        let script = completion_script("bash").expect("script");
        for command in ["sessions", "attach", "agents", "update", "worktree", "stop"] {
            assert!(script.contains(command), "{} missing", command);
        }
    }

    #[test]
    fn the_second_level_is_offered() {
        let bash = completion_script("bash").expect("script");
        assert!(bash.contains("search"), "sessions search missing");
        assert!(bash.contains("rollback"), "update rollback missing");
        let zsh = completion_script("zsh").expect("script");
        assert!(zsh.contains("prune"), "worktree prune missing");
        let fish = completion_script("fish").expect("script");
        assert!(
            fish.contains("__fish_seen_subcommand_from sessions"),
            "fish: {}",
            fish
        );
    }

    #[test]
    fn an_unknown_shell_is_refused() {
        let error = completion_script("csh").expect_err("should refuse");
        assert!(error.to_string().contains("unsupported shell"));
    }

    #[test]
    fn the_command_table_covers_the_dispatcher() {
        // Every command the dispatcher answers has to be offered here, or the
        // completion would hide a command that works.
        for command in [
            "login",
            "logout",
            "auth",
            "models",
            "mcp",
            "plugins",
            "skills",
            "commands",
            "hooks",
            "workflow",
            "telemetry",
            "sessions",
            "audit",
            "usage",
            "policy",
            "profiles",
            "themes",
            "keybindings",
            "automation",
            "eval",
            "update",
            "completion",
            "agents",
            "worktree",
            "attach",
            "logs",
            "follow",
            "stop",
            "rm",
            "doctor",
            "bug",
            "help",
            "version",
        ] {
            assert!(
                COMMANDS.contains(&command),
                "{} is not in the completion table",
                command
            );
        }
    }

    #[test]
    fn worktree_clean_recognises_merged_branches() {
        // 没有 git 就跳过：这个测试要真建仓库。
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "argent-worktree-clean-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create repo dir");

        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .expect("spawn git");
            assert!(status.success(), "git {:?} failed", args);
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "test@example.invalid"]);
        run(&["config", "user.name", "test"]);
        std::fs::write(dir.join("a.txt"), "a").expect("write a");
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        // 已合并分支：指向 main 的提交。
        run(&["branch", "argent/merged"]);
        // 未合并分支：多一个提交。
        run(&["checkout", "-b", "argent/unmerged"]);
        std::fs::write(dir.join("b.txt"), "b").expect("write b");
        run(&["add", "."]);
        run(&["commit", "-m", "work"]);
        run(&["checkout", "main"]);

        assert_eq!(default_branch(&dir).as_deref(), Some("main"));
        assert!(branch_is_ancestor(&dir, "argent/merged", "main"));
        assert!(!branch_is_ancestor(&dir, "argent/unmerged", "main"));
        assert!(last_commit_time(&dir).unwrap_or(0) > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_renders_event_logs_and_session_files() {
        // 事件日志：与 `--mode json` 捕获的包装格式一致；会话头与结果行跳过。
        let events = concat!(
            r#"{"type":"session","data":{"session":"s.jsonl","model":"m","cwd":"/w"}}"#,
            "\n",
            r#"{"id":1,"type":"event","event":"text","data":{"type":"assistant_text","text":"hello from events"}}"#,
            "\n",
            r#"{"type":"result","data":{"reason":"complete"}}"#,
            "\n"
        );
        let lines = replay_lines(events).expect("event log");
        assert!(
            lines.join("\n").contains("hello from events"),
            "{:?}",
            lines
        );

        // 会话文件：kind 行，经过消息 → transcript。
        let dir = std::env::temp_dir().join(format!("argent-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let entries = [
            argent_session::Entry::Message(argent_ai::Message::user("hello from session")),
            argent_session::Entry::Message(argent_ai::Message::assistant("the answer")),
        ];
        let text: String = entries
            .iter()
            .map(|entry| entry.to_json().dump() + "\n")
            .collect();
        let lines = replay_lines(&text).expect("session");
        let joined = lines.join("\n");
        assert!(joined.contains("hello from session"), "{}", joined);
        assert!(joined.contains("the answer"), "{}", joined);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
