//! `login` / `logout`: credential capture for a provider.

use super::*;

// ---------------------------------------------------------------- login

pub(super) fn login(args: &[String]) -> Result<i32> {
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

pub(super) fn key_page(provider: &str) -> &'static str {
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

pub(super) fn logout(args: &[String]) -> Result<i32> {
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
