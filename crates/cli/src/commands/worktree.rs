//! `worktree`: create, list, land and remove git worktrees.

use super::*;

/// `argent worktree`：`--worktree` 建出的工作树，列出来、删掉、清失效条目，
/// 以及批量清理已合并/空闲的工作树。
///
/// 路径约定与 `--worktree` 一致：`<storage>/worktrees/<repo>/<name>`，
/// 分支名 `argent/<name>`。
pub(super) fn worktree(args: &[String]) -> Result<i32> {
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
pub(super) fn default_branch(repo: &std::path::Path) -> Option<String> {
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
pub(super) fn branch_is_ancestor(repo: &std::path::Path, branch: &str, main: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", branch, main])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 工作树里最后一次提交的 Unix 时间；还没有提交时返回 None。
pub(super) fn last_commit_time(dir: &std::path::Path) -> Option<u64> {
    git_ok(dir, &["log", "-1", "--format=%ct"])?
        .trim()
        .parse()
        .ok()
}

/// 从 worktree 的 `.git` 文件反推主仓库根目录。
///
/// worktree 里的 `.git` 是文件，内容是 `gitdir: <repo>/.git/worktrees/<name>`。
pub(super) fn worktree_origin(worktree: &std::path::Path) -> Option<std::path::PathBuf> {
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
pub(super) fn git_ok(dir: &std::path::Path, args: &[&str]) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
