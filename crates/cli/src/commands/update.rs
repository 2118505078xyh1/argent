//! `update`: signature keys, manifest checks, install and rollback.

use super::*;

// ---------------------------------------------------------------- update

pub(super) fn update(args: &[String]) -> Result<i32> {
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
