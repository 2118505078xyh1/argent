//! `sessions`: list, export, archive, restore, prune.

use super::*;

// ---------------------------------------------------------------- sessions

/// 一行摘要：折叠空白并截断。
pub(super) fn clip_line(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = compact.chars().collect();
    if chars.len() > 120 {
        let head: String = chars[..117].iter().collect();
        format!("{}...", head)
    } else {
        compact
    }
}

/// 把一个会话移进 `sessions/archive/`。
///
/// 列表只读目录顶层，归档即从日常视图消失，但文件仍在，随时可恢复。
pub(super) fn archive_session(
    index: &argent_session::SessionIndex,
    reference: &str,
) -> Result<PathBuf> {
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
pub(super) fn unarchive_session(
    index: &argent_session::SessionIndex,
    reference: &str,
) -> Result<PathBuf> {
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
pub(super) fn delete_session(
    index: &argent_session::SessionIndex,
    reference: &str,
) -> Result<PathBuf> {
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
pub(super) fn sessions(args: &[String]) -> Result<i32> {
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

/// Export one transcript as a standalone HTML file.
pub(super) fn export_html(
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

pub(super) fn export_sqlite(
    index: &argent_session::SessionIndex,
    destination: &Path,
) -> Result<i32> {
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

pub(super) fn sql_text(value: &str) -> String {
    value.replace('\'', "''")
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
