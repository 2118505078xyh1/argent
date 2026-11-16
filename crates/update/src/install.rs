//! 把发布包装到目标位置。
//!
//! 发布包是一个 zip（`scripts/build-release.ps1` 生成的），里面放着可执行
//! 文件和文档。安装分三步：解压、找到 argent、换掉旧文件。
//!
//! 换文件时先把旧的改名成 `.bak`——Windows 上正在运行的 exe 不能被覆盖，
//! 但可以改名，这样安装失败时还能改回来。

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use argent_core::process::{self, CommandSpec};

/// 旧二进制备份的扩展名。
pub const BACKUP_SUFFIX: &str = "bak";

/// 旧二进制备份的位置：`argent.exe` -> `argent.bak`。
pub fn backup_path(target: &Path) -> PathBuf {
    target.with_extension(BACKUP_SUFFIX)
}

/// 当前平台上的可执行文件名。
pub fn binary_name() -> &'static str {
    if cfg!(windows) {
        "argent.exe"
    } else {
        "argent"
    }
}

/// 在解压目录里找到 argent 可执行文件。
///
/// 发布包里通常还散着文档和校验文件，所以按名字找最稳；找不到就报错，
/// 免得把别的东西装成 argent。
pub fn locate_binary(dir: &Path) -> Result<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    collect_binaries(dir, 0, &mut found);
    found.sort();
    if found.is_empty() {
        return Err(Error::config(format!(
            "no '{}' binary found under {}",
            binary_name(),
            dir.display()
        )));
    }
    // 同名时优先当前平台的扩展名。
    let name = binary_name();
    Ok(found
        .iter()
        .find(|path| path.file_name().and_then(|n| n.to_str()) == Some(name))
        .cloned()
        .unwrap_or_else(|| found[0].clone()))
}

fn collect_binaries(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_binaries(&path, depth + 1, out);
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "argent" || name == "argent.exe" {
            out.push(path);
        }
    }
}

/// 解开发布包到 `destination`。
///
/// 不是 zip 的包被当成裸的可执行文件，直接放进去。
pub fn extract(archive: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)
        .map_err(|error| Error::io(format!("create {} failed: {}", destination.display(), error)))?;
    let bytes = std::fs::read(archive)
        .map_err(|error| Error::io(format!("read {} failed: {}", archive.display(), error)))?;
    if bytes.starts_with(b"PK") {
        extract_zip(archive, destination)
    } else {
        let target = destination.join(binary_name());
        std::fs::write(&target, &bytes)
            .map_err(|error| Error::io(format!("write {} failed: {}", target.display(), error)))?;
        Ok(())
    }
}

/// 解开 zip：依次尝试 tar、unzip，Windows 上再试 PowerShell 的 Expand-Archive。
fn extract_zip(archive: &Path, destination: &Path) -> Result<()> {
    let archive_text = archive.to_string_lossy().into_owned();
    let destination_text = destination.to_string_lossy().into_owned();
    let mut tried: Vec<&str> = Vec::new();

    let tar = CommandSpec::new("tar")
        .arg("-xf")
        .arg(archive_text.clone())
        .arg("-C")
        .arg(destination_text.clone());
    tried.push("tar");
    if run_ok(&tar) {
        return Ok(());
    }

    let unzip = CommandSpec::new("unzip")
        .arg("-o")
        .arg(archive_text.clone())
        .arg("-d")
        .arg(destination_text.clone());
    tried.push("unzip");
    if run_ok(&unzip) {
        return Ok(());
    }

    #[cfg(windows)]
    {
        let script = format!(
            "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
            archive_text, destination_text
        );
        let expand = CommandSpec::new("powershell")
            .arg("-NoProfile")
            .arg("-Command")
            .arg(script);
        tried.push("Expand-Archive");
        if run_ok(&expand) {
            return Ok(());
        }
    }

    Err(Error::io(format!(
        "cannot unpack {}: tried {}",
        archive.display(),
        tried.join(", ")
    )))
}

fn run_ok(spec: &CommandSpec) -> bool {
    match process::run(spec) {
        Ok(output) => output.success(),
        Err(_) => false,
    }
}

/// 用 `source` 替换 `target`，旧文件备份成 `.bak`。
///
/// 返回备份路径；原来没有目标文件时返回 `None`。安装失败会自动把旧文件
/// 改回去。
pub fn swap_binary(source: &Path, target: &Path) -> Result<Option<PathBuf>> {
    if !source.is_file() {
        return Err(Error::io(format!("{} is not a file", source.display())));
    }
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::io(format!("create {} failed: {}", parent.display(), error))
            })?;
        }
    }
    let backup = backup_path(target);
    let had_target = target.exists();
    if had_target {
        if backup.exists() {
            let _ = std::fs::remove_file(&backup);
        }
        // 先改名再覆盖：Windows 上正在运行的 exe 不能被写，但可以改名。
        std::fs::rename(target, &backup).map_err(|error| {
            Error::io(format!("backup {} failed: {}", target.display(), error))
        })?;
    }
    match std::fs::copy(source, target) {
        Ok(_) => Ok(if had_target { Some(backup) } else { None }),
        Err(error) => {
            if had_target {
                let _ = std::fs::rename(&backup, target);
            }
            Err(Error::io(format!(
                "install {} failed: {}",
                target.display(),
                error
            )))
        }
    }
}

/// 撤销一次替换：把 `.bak` 改回原名。
///
/// 返回是否真的撤销了。
pub fn rollback(target: &Path) -> Result<bool> {
    let backup = backup_path(target);
    if !backup.is_file() {
        return Ok(false);
    }
    if target.exists() {
        std::fs::remove_file(target)
            .map_err(|error| Error::io(format!("remove {} failed: {}", target.display(), error)))?;
    }
    std::fs::rename(&backup, target)
        .map_err(|error| Error::io(format!("restore {} failed: {}", target.display(), error)))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "argent-update-install-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    #[test]
    fn a_bare_binary_is_placed_as_the_executable() {
        let root = scratch("bare");
        let archive = root.join("argent.bin");
        std::fs::write(&archive, b"not a zip").unwrap();
        let out = root.join("out");
        extract(&archive, &out).expect("extract");
        let binary = locate_binary(&out).expect("locate");
        assert_eq!(
            std::fs::read_to_string(&binary).unwrap(),
            "not a zip",
            "the payload must survive the round trip"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn swapping_keeps_the_old_binary_as_a_backup() {
        let root = scratch("swap");
        let source = root.join("new-binary");
        std::fs::write(&source, "new").unwrap();
        let target = root.join("argent.exe");
        std::fs::write(&target, "old").unwrap();

        let backup = swap_binary(&source, &target).expect("swap").expect("backup");
        assert_eq!(backup, root.join("argent.bak"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "old");

        // 回滚把旧文件换回来。
        assert!(rollback(&target).expect("rollback"));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(!root.join("argent.bak").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn swapping_without_an_existing_binary_creates_no_backup() {
        let root = scratch("fresh");
        let source = root.join("new-binary");
        std::fs::write(&source, "new").unwrap();
        let target = root.join("argent.exe");
        assert!(swap_binary(&source, &target).expect("swap").is_none());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(!rollback(&target).expect("rollback"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_source_is_refused() {
        let root = scratch("missing");
        let error = swap_binary(&root.join("nope"), &root.join("argent.exe")).expect_err("refuse");
        assert!(error.to_string().contains("not a file"), "{}", error);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_binary_is_found_inside_a_nested_folder() {
        let root = scratch("nested");
        let nested = root.join("argent-0.2.0");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(binary_name()), "payload").unwrap();
        let found = locate_binary(&root).expect("locate");
        assert_eq!(found, nested.join(binary_name()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_package_without_a_binary_is_rejected() {
        let root = scratch("empty");
        std::fs::write(root.join("README.md"), "nothing here").unwrap();
        assert!(locate_binary(&root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
