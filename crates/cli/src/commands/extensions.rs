//! Extension surface: plugins, marketplace, skills, slash commands, hooks.

use super::*;

// ---------------------------------------------------------------- plugins

pub(super) fn plugins(args: &[String]) -> Result<i32> {
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
pub(super) fn marketplace_command(
    positional: &[String],
    args: &[String],
    _paths: &Paths,
) -> Result<i32> {
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

pub(super) fn print_plugins_json(manager: &PluginManager) {
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

pub(super) fn skills(args: &[String]) -> Result<i32> {
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

pub(super) fn print_skill(skill: &Skill) {
    println!("# {}", skill.name);
    if !skill.description.is_empty() {
        println!("{}", skill.description);
    }
    println!("({})", skill.path.display());
    println!();
    println!("{}", skill.body);
}

// ---------------------------------------------------------------- commands

pub(super) fn commands(args: &[String]) -> Result<i32> {
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

pub(super) fn hooks(args: &[String]) -> Result<i32> {
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
        "run" => {
            let name = args
                .iter()
                .skip_while(|arg| arg.as_str() != "run")
                .nth(1)
                .ok_or_else(|| {
                    Error::config(
                        "usage: argent hooks run <Event> [--tool NAME] [--workspace PATH]",
                    )
                })?;
            let event = HookEvent::parse(name).ok_or_else(|| {
                Error::config(format!(
                    "unknown hook event '{}'; expected one of: {}",
                    name,
                    HookEvent::ALL
                        .iter()
                        .map(|event| event.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;
            let tool = flag_value(args, "--tool");
            let manager = PluginManager::for_workspace(&cwd);
            let config = crate::load_hooks(&manager, &cwd);
            let ctx = argent_hooks::HookContext {
                cwd: cwd.clone(),
                tool_name: tool.map(str::to_string),
                tool_input: flag_value(args, "--input").map(str::to_string),
                prompt: flag_value(args, "--prompt").map(str::to_string),
                message: flag_value(args, "--message").map(str::to_string),
                ..argent_hooks::HookContext::default()
            };
            let hooks = config.for_event(event, tool);
            if hooks.is_empty() {
                println!(
                    "no trusted {} hook matches{}",
                    event.name(),
                    tool.map(|tool| format!(" tool '{}'", tool))
                        .unwrap_or_default()
                );
                return Ok(0);
            }
            let mut blocked = false;
            for hook in hooks {
                let outcome = argent_hooks::run_command(event, hook, &ctx);
                let status = if outcome.timed_out {
                    "timeout"
                } else if outcome.success {
                    "ok"
                } else {
                    "failed"
                };
                println!(
                    "{} · {}ms · {}",
                    status, outcome.elapsed_ms, outcome.command
                );
                for line in outcome.stdout.lines() {
                    println!("  out: {}", line);
                }
                for line in outcome.stderr.lines() {
                    println!("  err: {}", line);
                }
                blocked |= outcome.blocks(hook);
            }
            Ok(if blocked { 1 } else { 0 })
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
                // 默认发现会无条件列出两个约定路径（见 workspace_hook_files）；
                // 不存在的文件对 list 是"没找到"，对 validate 也该如此，
                // 而不是报 cannot read 退出 1。显式指定的路径仍如实报错。
                None => crate::workspace_hook_files(&manager, &cwd)
                    .into_iter()
                    .filter(|path| path.is_file())
                    .collect(),
            };
            hooks_validate(&files)
        }
        other => Err(Error::config(format!(
            "unknown hooks command '{}'; expected trust, list, validate",
            other
        ))),
    }
}

pub(super) fn print_hook_list(
    manager: &PluginManager,
    cwd: &Path,
    trust: &HookTrust,
) -> Result<i32> {
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

pub(super) fn hooks_validate(files: &[PathBuf]) -> Result<i32> {
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

pub(super) fn hooks_trust(
    action: &str,
    args: &[String],
    cwd: &Path,
    trust_path: &Path,
) -> Result<i32> {
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

pub(super) fn print_hook_trust(trust: &HookTrust) {
    println!("trust store: {}", trust.path().display());
    if trust.entries().is_empty() {
        println!("no hook grants");
    } else {
        for (hook, digest) in trust.entries() {
            println!("{}  {}", digest, hook);
        }
    }
}
