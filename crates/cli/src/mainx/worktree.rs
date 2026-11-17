//! `--worktree`: create or reuse a linked worktree and switch cwd to it.

use crate::*;
/// `--worktree`：创建或复用 worktree，并把本次运行的 cwd 切过去。
pub(crate) fn enter_worktree(cli: &mut Cli) -> Result<()> {
    let Some(request) = cli.worktree.clone() else {
        return Ok(());
    };
    let repo = git_output(&cli.cwd, &["rev-parse", "--show-toplevel"])?.ok_or_else(|| {
        argent_core::Error::config("--worktree needs to run inside a git repository")
    })?;
    let repo = PathBuf::from(repo.trim());
    let name = request.unwrap_or_else(|| {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0);
        format!("argent-{}", stamp)
    });
    if !valid_worktree_name(&name) {
        return Err(argent_core::Error::config(format!(
            "invalid worktree name '{}'; use letters, digits, '-', '_', '.' or '/'",
            name
        )));
    }
    let repo_name = repo
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    let target = argent_config::Paths::new(&repo)
        .storage()
        .join("worktrees")
        .join(repo_name)
        .join(&name);
    if !target.is_dir() {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                argent_core::Error::io(format!("create {} failed: {}", parent.display(), e))
            })?;
        }
        let branch = format!("argent/{}", name);
        // 新分支是常态；同名分支已存在时复用，而不是报错。
        let added = git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                &branch,
                &target.display().to_string(),
            ],
        )?;
        if !added
            && !git_ok(
                &repo,
                &["worktree", "add", &target.display().to_string(), &branch],
            )?
        {
            return Err(argent_core::Error::io(format!(
                "git worktree add failed for {} (branch {})",
                target.display(),
                branch
            )));
        }
    }
    cli.cwd = target;
    Ok(())
}

/// 只接受不会逃出存储目录、也不会被 git 当成选项的名字。
pub(crate) fn valid_worktree_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

pub(crate) fn git_output(cwd: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| argent_core::Error::io(format!("cannot run git: {}", e)))?;
    if output.status.success() {
        Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        ))
    } else {
        Ok(None)
    }
}

pub(crate) fn git_ok(cwd: &Path, args: &[&str]) -> Result<bool> {
    Ok(git_output(cwd, args)?.is_some())
}
