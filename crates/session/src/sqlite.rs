//! Live SQLite mirror for session transcripts.
//!
//! JSONL stays the portable source of truth; every append is also written to a
//! SQLite database when `ARGENT_SQLITE` or `sessionBackend: "sqlite"` is set. That
//! gives external tools a live, indexed view without making the agent depend on
//! SQLite for reading a transcript.

use std::path::{Path, PathBuf};

use argent_core::error::{Error, Result};
use rusqlite::{params, Connection};

use crate::store::{now_iso, Entry};
use crate::tree::{Node, SessionTree};

/// A live SQLite mirror for one session file.
pub struct SqliteMirror {
    conn: Connection,
    session_id: String,
    path: String,
}

impl SqliteMirror {
    /// Open or create the database and make sure this session has a row.
    pub fn open(db_path: &Path, session_path: &Path) -> Result<SqliteMirror> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Error::io(format!("create {} failed: {}", parent.display(), error))
            })?;
        }
        let conn = Connection::open(db_path)
            .map_err(|error| Error::io(format!("open {} failed: {}", db_path.display(), error)))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 path TEXT NOT NULL,
                 title TEXT NOT NULL DEFAULT '',
                 cwd TEXT NOT NULL DEFAULT '',
                 created TEXT NOT NULL DEFAULT '',
                 updated TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS entries (
                 session_id TEXT NOT NULL,
                 seq INTEGER NOT NULL,
                 id TEXT,
                 parent TEXT,
                 kind TEXT NOT NULL,
                 payload TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (session_id, seq)
             );
             CREATE INDEX IF NOT EXISTS entries_session ON entries(session_id, seq);",
        )
        .map_err(|error| Error::io(format!("initialize SQLite session DB failed: {}", error)))?;
        let session_id = session_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or("session")
            .to_string();
        let path = session_path.display().to_string();
        conn.execute(
            "INSERT INTO sessions(id, path, updated) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET path=excluded.path, updated=excluded.updated",
            params![session_id, path, now_iso()],
        )
        .map_err(|error| Error::io(format!("write SQLite session row failed: {}", error)))?;
        Ok(SqliteMirror {
            conn,
            session_id,
            path,
        })
    }

    /// Mirror one payload written through [`SessionStore::append`].
    pub fn append(&self, entry: &Entry) -> Result<()> {
        if let Entry::Header(data) = entry {
            let title = data.str_or("title", "");
            let cwd = data.str_or("cwd", "");
            let created = data.str_or("created", "");
            let _ = self.conn.execute(
                "UPDATE sessions SET title=?1, cwd=?2, created=CASE WHEN ?3='' THEN created ELSE ?3 END, updated=?4 WHERE id=?5",
                params![title, cwd, created, now_iso(), self.session_id],
            );
        }
        let payload = entry.to_json();
        let kind = payload.str_or("kind", "entry");
        self.insert(&kind, None, None, payload.dump(), 0)
    }

    /// Mirror one tree node, preserving its id and parent.
    pub fn append_node(&self, node: &Node, tree: &SessionTree) -> Result<()> {
        let mut payload = node.payload.to_json();
        payload.set("id", argent_core::Json::string(node.id.clone()));
        match &node.parent {
            Some(parent) => payload.set("parent", argent_core::Json::string(parent.clone())),
            None => payload.set("parent", argent_core::Json::Null),
        }
        payload.set("created_at", argent_core::Json::num(node.created_at as f64));
        let _ = tree;
        let kind = payload.str_or("kind", "entry");
        self.insert(
            &kind,
            Some(node.id.clone()),
            node.parent.clone(),
            payload.dump(),
            node.created_at,
        )
    }

    fn insert(
        &self,
        kind: &str,
        id: Option<String>,
        parent: Option<String>,
        payload: String,
        created_at: u64,
    ) -> Result<()> {
        let seq: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM entries WHERE session_id=?1",
                params![self.session_id],
                |row| row.get(0),
            )
            .map_err(|error| Error::io(format!("read SQLite sequence failed: {}", error)))?;
        self.conn
            .execute(
                "INSERT INTO entries(session_id, seq, id, parent, kind, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    self.session_id,
                    seq,
                    id,
                    parent,
                    kind,
                    payload,
                    created_at as i64
                ],
            )
            .map_err(|error| Error::io(format!("write SQLite entry failed: {}", error)))?;
        let _ = self.conn.execute(
            "UPDATE sessions SET updated=?1 WHERE id=?2",
            params![now_iso(), self.session_id],
        );
        let _ = self.path;
        Ok(())
    }
}

/// The database path selected by `ARGENT_SQLITE`.
///
/// `1` / `true` means `~/.argent/cli/db/db.sqlite`; any other non-empty value
/// is treated as the path itself.
pub fn configured_db_path() -> Option<PathBuf> {
    let value = std::env::var("ARGENT_SQLITE").ok()?;
    let value = value.trim();
    if value.is_empty() || matches!(value, "0" | "false" | "off") {
        return None;
    }
    if matches!(value, "1" | "true" | "on") {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        return Some(
            home.join(".argent")
                .join("cli")
                .join("db")
                .join("db.sqlite"),
        );
    }
    Some(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_entries_and_nodes() {
        let dir = std::env::temp_dir().join(format!("argent-sqlite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("s.jsonl");
        let db = dir.join("db.sqlite");
        let mirror = SqliteMirror::open(&db, &session).expect("mirror");
        let mut header = argent_core::Json::obj();
        header.set("title", argent_core::Json::string("demo"));
        header.set("cwd", argent_core::Json::string("/tmp"));
        mirror.append(&Entry::Header(header)).expect("header");
        mirror
            .append(&Entry::Message(argent_ai::Message::user("hello")))
            .expect("message");

        let conn = Connection::open(&db).expect("open");
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        let entries: i64 = conn
            .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sessions, 1);
        assert_eq!(entries, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
