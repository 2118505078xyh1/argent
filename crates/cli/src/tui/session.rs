//! Session/transcript helpers behind the worker and the app.

use super::*;

pub(super) fn clip_hook_text(text: &str) -> String {
    let mut out: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > 240 {
        out = out.chars().take(240).collect();
        out.push_str("...");
    }
    out
}

/// Run a `!command` entered by the operator. The output is shown as a tool
/// row and never sent to the model, so the command does not cost tokens.
pub(super) fn run_shell(cwd: &Path, command: &str) -> (String, bool) {
    // Probe the shell, do not just look for a file named bash. On Windows a
    // bash.exe on PATH can be the WSL app-execution alias, which fails before
    // running anything when no distribution is installed (GitHub's runners
    // are exactly like this). `locate_posix_shell` verifies the candidate
    // actually executes, so the fallback to cmd is taken in that case.
    let mut spec = if let Some(program) = argent_tools::bash::locate_posix_shell() {
        let mut spec = argent_core::process::CommandSpec::new(program);
        spec.args.push("-lc".to_string());
        spec.args.push(command.to_string());
        spec
    } else if cfg!(windows) {
        let mut spec = argent_core::process::CommandSpec::new("cmd");
        spec.args.push("/C".to_string());
        spec.args.push(command.to_string());
        spec
    } else {
        let mut spec = argent_core::process::CommandSpec::new("sh");
        spec.args.push("-lc".to_string());
        spec.args.push(command.to_string());
        spec
    };
    spec.cwd = Some(cwd.display().to_string());
    spec.timeout = Duration::from_secs(120);
    match argent_core::process::run(&spec) {
        Ok(output) => {
            let mut text = output.stdout.trim_end().to_string();
            if !output.stderr.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(output.stderr.trim_end());
            }
            if text.is_empty() {
                text = "(no output)".to_string();
            }
            text.push_str(&format!(
                "\n[exit {} in {:.1}s]",
                output.status,
                output.duration.as_secs_f32()
            ));
            (text, output.success())
        }
        Err(error) => (format!("shell failed: {}", error), false),
    }
}

/// A small, factual AGENTS.md template. It describes the repository, not the
/// model's guesses; the operator can edit it afterwards.
pub(super) fn project_init_text(cwd: &Path) -> String {
    let mut out = String::new();
    out.push_str(
        "# Project instructions

",
    );
    out.push_str(&format!(
        "Working directory: `{}`

",
        cwd.display()
    ));
    out.push_str(
        "## Layout
",
    );
    let mut top: Vec<String> = std::fs::read_dir(cwd)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.')
                || matches!(name.as_str(), "target" | "node_modules" | "dist" | "build")
            {
                return None;
            }
            let suffix = if entry.path().is_dir() { "/" } else { "" };
            Some(format!("{}{}", name, suffix))
        })
        .collect();
    top.sort();
    top.truncate(24);
    for item in &top {
        out.push_str(&format!(
            "- `{}`
",
            item
        ));
    }
    out.push_str(
        "
## Languages
",
    );
    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for entry in argent_core::path::walk(cwd, 4).into_iter().take(3_000) {
        let extension = entry
            .path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let language = match extension.as_str() {
            "rs" => "Rust",
            "ts" | "tsx" => "TypeScript",
            "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
            "py" => "Python",
            "go" => "Go",
            "java" => "Java",
            "kt" | "kts" => "Kotlin",
            "cs" => "C#",
            "c" | "h" | "cc" | "cpp" | "hpp" => "C/C++",
            "md" | "markdown" => "Markdown",
            _ => continue,
        };
        *counts.entry(language).or_default() += 1;
    }
    let mut languages: Vec<_> = counts.into_iter().collect();
    languages.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (language, count) in languages.iter().take(6) {
        out.push_str(&format!(
            "- {}: {} files
",
            language, count
        ));
    }
    out.push_str(
        "
## Build and test
",
    );
    let mut commands = Vec::new();
    if cwd.join("Cargo.toml").is_file() {
        commands.push("cargo build");
        commands.push("cargo test");
        commands.push("cargo fmt --check");
        commands.push("cargo clippy --all-targets");
    }
    if cwd.join("package.json").is_file() {
        commands.push("npm install");
        commands.push("npm test");
        commands.push("npm run build");
    }
    if cwd.join("pyproject.toml").is_file() || cwd.join("requirements.txt").is_file() {
        commands.push("pytest");
    }
    if cwd.join("Makefile").is_file() {
        commands.push("make");
    }
    if commands.is_empty() {
        out.push_str(
            "- (add the real build and test commands here)
",
        );
    } else {
        for command in commands {
            out.push_str(&format!(
                "- `{}`
",
                command
            ));
        }
    }
    out.push_str(
        "
## Conventions
",
    );
    out.push_str(
        "- Read a file before editing it.
",
    );
    out.push_str(
        "- Run the build/tests before reporting success.
",
    );
    out.push_str(
        "- Keep changes focused; do not refactor unrelated code.
",
    );
    out
}

/// The title a new task takes from its first prompt.
pub(super) fn title_from_prompt(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "新任务".to_string()
    } else {
        line.chars().take(60).collect()
    }
}

/// Assemble the tree overlay rows for one session.
pub(super) fn tree_rows(session: &SessionContext) -> Vec<TreeRow> {
    let active: Vec<String> = session
        .tree
        .active_path()
        .iter()
        .map(|node| node.id.clone())
        .collect();
    let leaf = session.tree.active().unwrap_or("").to_string();
    let mut rows = Vec::new();
    for id in session.tree.ids() {
        let Some(node) = session.tree.get(id) else {
            continue;
        };
        let depth = session.tree.path_to(id).map(|p| p.len()).unwrap_or(1) - 1;
        let label = match &node.payload {
            argent_session::Entry::Message(message) => {
                let role = match message.role {
                    argent_ai::Role::User => "user",
                    argent_ai::Role::Assistant => "assistant",
                    argent_ai::Role::Tool => "tool",
                    argent_ai::Role::System => "system",
                };
                let text = message.text().replace('\n', " ");
                let text = clip_to_width(text.trim(), 52);
                format!("{}: {}", role, text)
            }
            argent_session::Entry::Summary(text) => {
                format!(
                    "summary: {}",
                    clip_to_width(text.replace('\n', " ").trim(), 44)
                )
            }
            argent_session::Entry::Header(data) => {
                format!("header: {}", data.str_or("title", "(untitled)"))
            }
            argent_session::Entry::Event(_) => "event".to_string(),
        };
        rows.push(TreeRow {
            id: id.clone(),
            depth,
            label,
            on_path: active.iter().any(|node| node == id),
            leaf: leaf == *id,
            branch: session.tree.children(id).len() > 1,
        });
    }
    rows
}

/// Copy an external session file into the sessions directory, keeping every
/// id, parent link and the original header (cwd included).
pub(super) fn import_session(
    index: &SessionIndex,
    source: &str,
) -> argent_core::error::Result<std::path::PathBuf> {
    let source_path = std::path::PathBuf::from(source);
    if !source_path.is_file() {
        return Err(argent_core::error::Error::io(format!(
            "no such file: {}",
            source
        )));
    }
    // Structural validation: every line must be JSON; the tree rebuild also
    // verifies parent links resolve.
    let lines = argent_session::SessionStore::read_raw(&source_path)?;
    argent_session::SessionTree::from_raw(&lines)?;
    let stamp = argent_session::timestamp_slug();
    let stem = source_path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "imported".to_string());
    let name = argent_session::sanitize(&format!("import-{}-{}", stamp, stem));
    let destination = index.dir().join(format!("{}.jsonl", name));
    std::fs::create_dir_all(index.dir()).map_err(|e| {
        argent_core::error::Error::io(format!("create {} failed: {}", index.dir().display(), e))
    })?;
    std::fs::copy(&source_path, &destination)
        .map_err(|e| argent_core::error::Error::io(format!("copy {} failed: {}", source, e)))?;
    Ok(destination)
}

/// /resume 切到另一个会话时，缓存亲和键也跟着换，避免和别的会话
/// 共用一份 KV 前缀（pi 也是按 session 文件做 cache key）。
pub(super) fn refresh_session_cache_key(path: &std::path::Path) {
    if std::env::var_os("ARGENT_CACHE_KEY_AUTO").is_none() {
        return;
    }
    let key = argent_core::hash::short_hash(path.to_string_lossy().as_bytes(), 24);
    std::env::set_var("ARGENT_CACHE_KEY", key);
}

/// Rows for the `/hooks` panel: trust state per document, then each command.
pub(super) fn hook_overview(cwd: &Path) -> Vec<(String, String)> {
    let manager = argent_plugins::PluginManager::for_workspace(cwd);
    let trust = argent_hooks::HookTrust::at(argent_config::Paths::new(cwd).hook_trust()).ok();
    let mut rows = Vec::new();
    for file in crate::workspace_hook_files(&manager, cwd) {
        if !file.is_file() {
            continue;
        }
        let trusted = match (&trust, argent_hooks::digest_file(&file)) {
            (Some(trust), Ok(digest)) => trust.is_trusted(&file, &digest),
            _ => false,
        };
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("hooks.json");
        rows.push((
            format!(
                "{}  {}",
                if trusted { "trusted  " } else { "untrusted" },
                name
            ),
            file.display().to_string(),
        ));
        match HookConfig::load(&file) {
            Ok(config) => {
                for (event, command) in config.entries() {
                    rows.push((
                        format!("  {}", event.name()),
                        format!(
                            "{} · {}ms · {}{}",
                            command.matcher.as_deref().unwrap_or("*"),
                            command.timeout_ms,
                            command.command,
                            if command.async_hook { "  [async]" } else { "" }
                        ),
                    ));
                }
            }
            Err(error) => rows.push(("  invalid".to_string(), error.to_string())),
        }
    }
    if rows.is_empty() {
        rows.push(("no hook documents found".to_string(), String::new()));
    }
    rows
}

pub(super) fn session_rows(index: &SessionIndex) -> Vec<SessionRow> {
    index
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|(path, info)| SessionRow {
            path,
            title: info.str_or("title", "(untitled)"),
            cwd: info.str_or("cwd", ""),
            updated: argent_session::local_stamp(info.num_or("modified", 0.0) as u64),
        })
        .collect()
}

#[cfg(test)]
mod tree_tests {
    use super::*;
    use argent_ai::Message;
    use argent_session::Entry;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "argent-tree-ui-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create");
        dir
    }

    fn seeded_session(dir: &std::path::Path) -> SessionContext {
        let path = dir.join("tree.jsonl");
        let mut ctx = SessionContext::open(path).expect("open");
        ctx.record(Message::user("first question")).expect("record");
        ctx.record(Message::assistant("first answer"))
            .expect("record");
        ctx
    }

    #[test]
    fn tree_rows_label_roles_and_mark_the_active_leaf() {
        let dir = temp_dir("rows");
        let ctx = seeded_session(&dir);
        let rows = tree_rows(&ctx);
        // header? none in this file; three nodes: user, assistant — plus none.
        assert!(rows
            .iter()
            .any(|row| row.label.starts_with("user:") && row.label.contains("first question")));
        assert!(rows
            .iter()
            .any(|row| row.label.starts_with("assistant:") && row.label.contains("first answer")));
        let leaf_rows: Vec<_> = rows.iter().filter(|row| row.leaf).collect();
        assert_eq!(leaf_rows.len(), 1, "exactly one leaf");
        assert!(leaf_rows[0].label.contains("first answer"));
        assert!(
            rows.iter().all(|row| row.on_path),
            "a linear file is all on-path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fork_shows_a_branch_marker_and_two_leaves_over_time() {
        let dir = temp_dir("branch");
        let mut ctx = seeded_session(&dir);
        let one = ctx
            .tree
            .ids()
            .iter()
            .find(|id| {
                matches!(&ctx.tree.get(id).expect("node").payload, Entry::Message(m)
                    if m.text() == "first question")
            })
            .cloned()
            .expect("node");
        ctx.fork_at(&one).expect("fork");
        ctx.record(Message::assistant("alternative"))
            .expect("record");

        let rows = tree_rows(&ctx);
        let branch_rows: Vec<_> = rows.iter().filter(|row| row.branch).collect();
        assert_eq!(branch_rows.len(), 1, "the fork point");
        assert!(branch_rows[0].label.contains("first question"));
        let on_path: Vec<_> = rows.iter().filter(|row| row.on_path).collect();
        assert_eq!(on_path.len(), 2, "fork point + the new leaf");
        assert!(on_path.iter().all(|row| {
            row.label.contains("first question") || row.label.contains("alternative")
        }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_copies_a_session_and_rejects_garbage() {
        let dir = temp_dir("import");
        let index = SessionIndex::new(dir.join("sessions"));
        // 合法会话文件
        let source = dir.join("outer.jsonl");
        let mut ctx = SessionContext::open(source.clone()).expect("open");
        ctx.record(Message::user("traveller")).expect("record");
        ctx.close();
        let destination = import_session(&index, source.to_str().expect("utf8")).expect("import");
        assert!(destination.is_file());
        assert_eq!(
            destination.parent().expect("parent"),
            index.dir(),
            "lands in the sessions directory"
        );
        let entries = argent_session::SessionStore::read_all(&destination).expect("read");
        assert!(entries.iter().any(|entry| matches!(
            entry,
            Entry::Message(m) if m.text() == "traveller"
        )));
        // 坏 JSON 被拒绝
        let bad = dir.join("bad.jsonl");
        std::fs::write(&bad, "not json at all").expect("write");
        assert!(import_session(&index, bad.to_str().expect("utf8")).is_err());
        // 不存在的文件被拒绝
        assert!(import_session(&index, dir.join("nope.jsonl").to_str().expect("utf8")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tree_overlay_only_offers_rows_that_exist() {
        let dir = temp_dir("overlay");
        let ctx = seeded_session(&dir);
        let rows = tree_rows(&ctx);
        for row in &rows {
            assert!(ctx.tree.contains(&row.id), "row id {} must exist", row.id);
            assert!(ctx.tree.path_to(&row.id).is_ok());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
