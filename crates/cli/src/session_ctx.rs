//! The interactive session's mutable state.
//!
//! A `SessionContext` groups the three things that change together: the file a
//! session lives in, the open append handle, and the tree of entries. Switching
//! sessions means replacing the whole value, which is why it is a struct rather
//! than three loose locals.

use std::path::PathBuf;

use argent_ai::Message;
use argent_core::error::Result;
use argent_session::{Entry, SessionStore, SessionTree};

/// One open session.
pub struct SessionContext {
    /// The transcript file.
    pub path: PathBuf,
    /// The append handle.
    pub store: SessionStore,
    /// The entry tree.
    pub tree: SessionTree,
}

impl SessionContext {
    /// Open an existing session file, creating it when it is missing.
    pub fn open(path: PathBuf) -> Result<SessionContext> {
        let store = SessionStore::open(path.clone())?;
        let tree = SessionStore::read_tree(&path)?;
        Ok(SessionContext { path, store, tree })
    }

    /// The conversation the model will see.
    pub fn messages(&self) -> Vec<Message> {
        self.tree.messages()
    }

    /// Record one message.
    pub fn record(&mut self, message: Message) -> Result<()> {
        let id = self.tree.append(Entry::Message(message))?;
        self.store.append_tree_node(&self.tree, &id)
    }

    /// Record a compaction summary.
    pub fn record_summary(&mut self, summary: String) -> Result<()> {
        let id = self.tree.append(Entry::Summary(summary))?;
        self.store.append_tree_node(&self.tree, &id)
    }

    /// Record every message this run produced.
    ///
    /// `before` is the history length from before the run, so only new messages
    /// are written and the user's message is stored exactly once.
    pub fn record_new(&mut self, history: &[Message], before: usize) -> Result<()> {
        for message in history.iter().skip(before) {
            self.record(message.clone())?;
        }
        Ok(())
    }

    /// Move the active leaf to `id` and persist the choice.
    ///
    /// The returned messages are the conversation on the path to `id`; the
    /// caller replays them into the agent. New records append under `id`, so
    /// a node in the middle of an old branch starts a new one — this is the
    /// fork primitive.
    pub fn checkout(&mut self, id: &str) -> Result<Vec<Message>> {
        self.tree.set_active(id)?;
        self.persist_active(id)?;
        Ok(self.messages())
    }

    /// Fork the conversation at `id`: the active leaf moves to `id` and the
    /// next record starts a new branch. Same mechanics as [`checkout`]; the
    /// name documents the intent.
    pub fn fork_at(&mut self, id: &str) -> Result<Vec<Message>> {
        self.checkout(id)
    }

    /// Write the active-branch marker so reopening the session restores it.
    fn persist_active(&mut self, id: &str) -> Result<()> {
        let mut event = argent_core::Json::obj();
        event.set("type", argent_core::Json::string("active"));
        event.set("id", argent_core::Json::string(id));
        self.store.append(&Entry::Event(event))
    }

    /// Flush and detach before switching away.
    pub fn close(&mut self) {
        self.store.close();
    }

    /// The session's display title, from the newest header.
    pub fn title(&self) -> String {
        self.tree
            .ids()
            .iter()
            .rev()
            .filter_map(|id| self.tree.get(id))
            .find_map(|node| match &node.payload {
                Entry::Header(data) => Some(data.str_or("title", "")),
                _ => None,
            })
            .unwrap_or_else(|| "(untitled)".to_string())
    }

    /// The message count on the active branch.
    #[cfg(test)]
    pub fn message_count(&self) -> usize {
        self.tree
            .active_path()
            .iter()
            .filter(|node| matches!(node.payload, Entry::Message(_)))
            .count()
    }

    /// A short description for a banner.
    #[cfg(test)]
    pub fn describe(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!("{} ({} messages)", name, self.message_count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("argent-ctx-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create");
        dir
    }

    #[test]
    fn records_and_reads_back_messages() {
        let dir = temp_dir("record");
        let path = dir.join("s.jsonl");
        // Seed a header so the tree is not empty.
        let mut store = SessionStore::open(path.clone()).expect("open");
        store
            .append(&Entry::Header(
                argent_core::Json::parse(r#"{"title":"demo"}"#).unwrap(),
            ))
            .expect("header");
        store.close();

        let mut ctx = SessionContext::open(path).expect("open ctx");
        ctx.record(Message::user("hello")).expect("record");
        assert_eq!(ctx.message_count(), 1);
        assert_eq!(ctx.messages().len(), 1);
        assert_eq!(ctx.title(), "demo");
        assert!(ctx.describe().contains("1 messages"));
    }

    #[test]
    fn a_summary_is_part_of_the_conversation() {
        let dir = temp_dir("summary");
        let path = dir.join("s.jsonl");
        let mut ctx = SessionContext::open(path).expect("open");
        ctx.record(Message::user("one")).expect("record");
        ctx.record_summary("## Goal\n- do the thing".to_string())
            .expect("summary");
        let messages = ctx.messages();
        assert_eq!(messages.len(), 2);
        assert!(messages[1].text().contains("do the thing"));
    }

    #[test]
    fn record_new_only_writes_the_tail() {
        let dir = temp_dir("tail");
        let path = dir.join("s.jsonl");
        let mut ctx = SessionContext::open(path).expect("open");
        let history = vec![
            Message::user("one"),
            Message::assistant("two"),
            Message::user("three"),
        ];
        // Pretend the first two were already stored.
        ctx.record_new(&history, 2).expect("record");
        assert_eq!(ctx.message_count(), 1);
        assert_eq!(ctx.messages()[0].text(), "three");
    }

    #[test]
    fn a_forked_branch_survives_a_reopen() {
        let dir = temp_dir("fork-reopen");
        let path = dir.join("s.jsonl");
        let mut ctx = SessionContext::open(path.clone()).expect("open");
        ctx.record(Message::user("one")).expect("record");
        ctx.record(Message::assistant("two")).expect("record");
        let one_id = ctx
            .tree
            .ids()
            .iter()
            .rev()
            .find(|id| {
                matches!(&ctx.tree.get(id).expect("node").payload, Entry::Message(m)
                    if m.text() == "one")
            })
            .cloned()
            .expect("node for 'one'");

        // 从第一条 user 消息分叉：重放只有分叉点之前的路径。
        let replayed = ctx.fork_at(&one_id).expect("fork");
        assert_eq!(replayed.len(), 1);
        ctx.record(Message::assistant("alternative"))
            .expect("record");
        ctx.close();

        // 重开：分支结构保留，active 停在新分支上。
        let reopened = SessionContext::open(path).expect("reopen");
        let messages = reopened.messages();
        assert_eq!(messages.len(), 2, "one -> alternative");
        assert_eq!(messages[1].text(), "alternative");
        assert!(
            !reopened.tree.branch_points().is_empty(),
            "one is a branch point"
        );
        assert_eq!(reopened.tree.ids().len(), 3, "one + two + alternative");
    }

    #[test]
    fn a_linear_legacy_file_rebuilds_as_a_chain() {
        let dir = temp_dir("legacy");
        let path = dir.join("s.jsonl");
        // 旧格式：store.append 写不带 id/parent 的行。
        let mut store = SessionStore::open(path.clone()).expect("open");
        store
            .append(&Entry::Header(argent_core::Json::parse("{}").unwrap()))
            .expect("header");
        store
            .append(&Entry::Message(Message::user("old one")))
            .expect("m1");
        store
            .append(&Entry::Message(Message::assistant("old two")))
            .expect("m2");
        store.close();

        let mut ctx = SessionContext::open(path).expect("open");
        assert_eq!(ctx.messages().len(), 2);
        assert_eq!(ctx.messages()[1].text(), "old two");
        // 计数器越过文件已有的 id，后续 mint 不冲突。
        ctx.record(Message::user("new")).expect("record");
        assert_eq!(ctx.message_count(), 3);
    }
}
