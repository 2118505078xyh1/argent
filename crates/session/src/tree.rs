//! Session trees: entries form a tree, and a branch is a path through it.
//!
//! Every entry carries an id and the id of its parent. The conversation the
//! model sees is the path from the root to the active leaf, so a session can
//! fork at any point without disturbing the original line.

use std::collections::BTreeMap;

use argent_ai::Message;
use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::store::Entry;

/// A node in the session tree.
#[derive(Debug, Clone)]
pub struct Node {
    /// Unique id within the session.
    pub id: String,
    /// The id of the parent node, or `None` for a root.
    pub parent: Option<String>,
    /// The payload: a message, a summary, an event or the header.
    pub payload: Entry,
    /// Creation time in epoch milliseconds.
    pub created_at: u64,
}

/// A session tree.
#[derive(Debug, Default)]
pub struct SessionTree {
    nodes: BTreeMap<String, Node>,
    order: Vec<String>,
    /// The leaf the active branch ends at.
    active: Option<String>,
    /// Monotonic counter used to mint ids.
    counter: u64,
}

impl SessionTree {
    /// An empty tree.
    pub fn new() -> SessionTree {
        SessionTree::default()
    }

    /// Append a node as a child of the active leaf.
    pub fn append(&mut self, payload: Entry) -> Result<String> {
        let id = self.next_id();
        let parent = self.active.clone();
        let node = Node {
            id: id.clone(),
            parent,
            payload,
            created_at: now_millis(),
        };
        self.nodes.insert(id.clone(), node);
        self.order.push(id.clone());
        self.active = Some(id.clone());
        Ok(id)
    }

    /// Append a node as a child of a specific parent, creating a branch.
    pub fn append_to(&mut self, parent: &str, payload: Entry) -> Result<String> {
        if !self.nodes.contains_key(parent) {
            return Err(Error::config(format!(
                "no session node with id '{}'",
                parent
            )));
        }
        let id = self.next_id();
        let node = Node {
            id: id.clone(),
            parent: Some(parent.to_string()),
            payload,
            created_at: now_millis(),
        };
        self.nodes.insert(id.clone(), node);
        self.order.push(id.clone());
        self.active = Some(id.clone());
        Ok(id)
    }

    /// Mint a new id: the lowest number not in use.
    ///
    /// A file can hold lines written before the tree existed (no `id` field).
    /// Minting "highest + 1" would shift those ids relative to the ones the
    /// live session assigned, breaking the parent links the file does carry.
    fn next_id(&mut self) -> String {
        loop {
            self.counter += 1;
            let candidate = format!("n{:06}", self.counter);
            if !self.nodes.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    /// Whether a node with this id exists.
    pub fn contains(&self, id: &str) -> bool {
        self.nodes.contains_key(id)
    }

    /// Insert a known node (id and parent taken as-is), keeping its position
    /// in the file order. Used when rebuilding a tree from disk.
    pub fn insert_node(&mut self, id: &str, parent: Option<String>, payload: Entry) -> Result<()> {
        if self.nodes.contains_key(id) {
            return Err(Error::config(format!("duplicate node id '{}'", id)));
        }
        if let Some(parent) = &parent {
            if !self.nodes.contains_key(parent) {
                return Err(Error::config(format!(
                    "no session node with id '{}'",
                    parent
                )));
            }
        }
        let node = Node {
            id: id.to_string(),
            parent,
            payload,
            created_at: now_millis(),
        };
        self.nodes.insert(id.to_string(), node);
        self.order.push(id.to_string());
        self.active = Some(id.to_string());
        Ok(())
    }

    /// Rebuild a tree from the raw JSON lines of a session file.
    ///
    /// Lines carry `id` / `parent` when they were written by the tree path;
    /// older linear files do not, and their entries are minted fresh ids
    /// chained in file order. An `active` event line restores the branch the
    /// session was on; without one the last line wins.
    pub fn from_raw(lines: &[Json]) -> Result<SessionTree> {
        let mut tree = SessionTree::new();
        let mut pending_active: Option<String> = None;
        for line in lines {
            let entry = match Entry::from_json(line) {
                Some(entry) => entry,
                None => continue,
            };
            if let Entry::Event(event) = &entry {
                if event.str_or("type", "") == "active" {
                    pending_active = Some(event.str_or("id", ""));
                    continue;
                }
            }
            let id = line.str_or("id", "");
            if id.is_empty() || tree.contains(&id) {
                tree.append(entry)?;
            } else {
                let parent = match line.get("parent") {
                    Some(Json::Str(parent)) => Some(parent.clone()),
                    _ => None,
                };
                tree.insert_node(&id, parent, entry)?;
            }
        }
        // A persisted active marker only wins when it still points at a leaf:
        // fork/checkout writes the marker before new records, and those newer
        // lines must not be pulled off the active path by a stale anchor.
        if let Some(id) = pending_active {
            if tree.contains(&id) && tree.children(&id).is_empty() {
                tree.set_active(&id)?;
            }
        }
        Ok(tree)
    }

    /// The active leaf.
    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    /// Make a node the active leaf, switching branches.
    pub fn set_active(&mut self, id: &str) -> Result<()> {
        if !self.nodes.contains_key(id) {
            return Err(Error::config(format!("no session node with id '{}'", id)));
        }
        self.active = Some(id.to_string());
        Ok(())
    }

    /// The path from the root to the active leaf, oldest first.
    pub fn active_path(&self) -> Vec<&Node> {
        let mut path = Vec::new();
        let mut current = self.active.clone();
        while let Some(id) = current {
            let node = match self.nodes.get(&id) {
                Some(node) => node,
                None => break,
            };
            path.push(node);
            current = node.parent.clone();
        }
        path.reverse();
        path
    }

    /// The path to a specific node.
    pub fn path_to(&self, id: &str) -> Result<Vec<&Node>> {
        let mut path = Vec::new();
        let mut current = Some(id.to_string());
        while let Some(current_id) = current {
            let node = match self.nodes.get(&current_id) {
                Some(node) => node,
                None => return Err(Error::config(format!("unknown node '{}'", current_id))),
            };
            path.push(node);
            current = node.parent.clone();
        }
        path.reverse();
        Ok(path)
    }

    /// Messages on `target` that are not on `other` after their common
    /// ancestor. Used to summarize a branch when switching to it (Pi's
    /// `branchSummary`). Returns an empty list when the two paths share the
    /// whole target, i.e. there is nothing branch-specific to report.
    pub fn branch_summary_messages(&self, target: &str, other: &str) -> Vec<Message> {
        let target_path = match self.path_to(target) {
            Ok(path) => path,
            Err(_) => return Vec::new(),
        };
        let other_path = match self.path_to(other) {
            Ok(path) => path,
            Err(_) => return Vec::new(),
        };
        let common = target_path
            .iter()
            .zip(other_path.iter())
            .take_while(|(left, right)| left.id == right.id)
            .count();
        let mut out = Vec::new();
        for node in target_path.into_iter().skip(common) {
            match &node.payload {
                Entry::Message(message) => out.push(message.clone()),
                Entry::Summary(text) => out.push(Message::system(format!(
                    "Earlier branch summary: {}",
                    text
                ))),
                _ => {}
            }
        }
        out
    }

    /// The conversation on the active branch.
    pub fn messages(&self) -> Vec<Message> {
        let mut out = Vec::new();
        for node in self.active_path() {
            match &node.payload {
                Entry::Message(message) => out.push(message.clone()),
                Entry::Summary(text) => out.push(Message::system(format!(
                    "Summary of earlier conversation:\n{}",
                    text
                ))),
                _ => {}
            }
        }
        out
    }

    /// Every node id, in insertion order.
    pub fn ids(&self) -> &[String] {
        &self.order
    }

    /// A node by id.
    pub fn get(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// The children of a node.
    pub fn children(&self, id: &str) -> Vec<&Node> {
        self.order
            .iter()
            .filter_map(|node_id| self.nodes.get(node_id))
            .filter(|node| node.parent.as_deref() == Some(id))
            .collect()
    }

    /// Nodes that have more than one child: the branch points.
    pub fn branch_points(&self) -> Vec<&Node> {
        let mut out = Vec::new();
        for id in &self.order {
            if let Some(node) = self.nodes.get(id) {
                if self.children(id).len() > 1 {
                    out.push(node);
                }
            }
        }
        out
    }

    /// Fork the tree at a node, producing a new tree whose active leaf is that
    /// node. Changes in the new tree do not touch the original.
    pub fn fork_at(&self, id: &str) -> Result<SessionTree> {
        let path = self.path_to(id)?;
        let mut fork = SessionTree::new();
        for node in path {
            let new_id = fork.append(node.payload.clone())?;
            // Preserve the original ids so the fork still points at the same
            // history when it is written back.
            if let Some(stored) = fork.nodes.get_mut(&new_id) {
                stored.id = node.id.clone();
            }
            let renamed = node.id.clone();
            fork.nodes.remove(&new_id);
            fork.order.pop();
            fork.nodes.insert(
                renamed.clone(),
                Node {
                    id: renamed.clone(),
                    parent: node.parent.clone(),
                    payload: node.payload.clone(),
                    created_at: node.created_at,
                },
            );
            fork.order.push(renamed.clone());
            fork.active = Some(renamed);
        }
        fork.counter = self.counter;
        Ok(fork)
    }

    /// Serialize the whole tree.
    pub fn to_json(&self) -> Json {
        let mut nodes = Vec::new();
        for id in &self.order {
            if let Some(node) = self.nodes.get(id) {
                let mut entry = node.payload.to_json();
                entry.set("id", Json::string(node.id.clone()));
                match &node.parent {
                    Some(parent) => entry.set("parent", Json::string(parent.clone())),
                    None => entry.set("parent", Json::Null),
                }
                entry.set("created_at", Json::num(node.created_at as f64));
                nodes.push(entry);
            }
        }
        let mut out = Json::obj();
        out.set("kind", Json::string("tree"));
        out.set("nodes", Json::Arr(nodes));
        match &self.active {
            Some(active) => out.set("active", Json::string(active.clone())),
            None => out.set("active", Json::Null),
        }
        out
    }

    /// Rebuild a tree from its serialized form.
    pub fn from_json(value: &Json) -> Result<SessionTree> {
        let mut tree = SessionTree::new();
        let nodes = value
            .get("nodes")
            .and_then(Json::as_array)
            .map(|items| items.to_vec())
            .unwrap_or_default();
        for entry in nodes {
            let id = entry.str_or("id", "");
            if id.is_empty() {
                continue;
            }
            let parent = match entry.get("parent") {
                Some(Json::Str(p)) => Some(p.clone()),
                _ => None,
            };
            let payload = match Entry::from_json(&entry) {
                Some(payload) => payload,
                None => continue,
            };
            let created_at = entry.num_or("created_at", 0.0) as u64;
            tree.nodes.insert(
                id.clone(),
                Node {
                    id: id.clone(),
                    parent,
                    payload,
                    created_at,
                },
            );
            tree.order.push(id.clone());
        }
        tree.active = value
            .get("active")
            .and_then(Json::as_str)
            .map(str::to_string);
        tree.counter = tree.order.len() as u64;
        Ok(tree)
    }
}

/// Current time in epoch milliseconds.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Entry {
        Entry::Message(Message::user(text))
    }

    #[test]
    fn appends_a_linear_path() {
        let mut tree = SessionTree::new();
        tree.append(user("one")).expect("append");
        tree.append(user("two")).expect("append");
        let messages = tree.messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "one");
        assert_eq!(messages[1].text(), "two");
    }

    #[test]
    fn branches_keep_their_own_history() {
        let mut tree = SessionTree::new();
        let first = tree.append(user("one")).expect("append");
        tree.append(user("two")).expect("append");
        // Branch from the first node.
        tree.append_to(&first, user("alternative")).expect("branch");

        let active = tree.messages();
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].text(), "one");
        assert_eq!(active[1].text(), "alternative");

        // Switching back restores the original branch.
        tree.set_active("n000002").expect("switch");
        let original = tree.messages();
        assert_eq!(original[1].text(), "two");
    }

    #[test]
    fn branch_summary_returns_only_the_target_branchs_unique_messages() {
        let mut tree = SessionTree::new();
        let root = tree.append(user("one")).expect("append");
        let original_leaf = tree.append(user("original")).expect("append");
        let branch_leaf = tree.append_to(&root, user("alternative")).expect("branch");
        tree.append_to(&branch_leaf, user("follow-up")).expect("append");

        let summary = tree.branch_summary_messages(&branch_leaf, &original_leaf);
        let texts: Vec<String> = summary.iter().map(|m| m.text()).collect();
        assert_eq!(texts, ["alternative".to_string()], "shared root is not repeated");

        // Switching the other way reports the messages unique to that side.
        let reverse = tree.branch_summary_messages(&original_leaf, &branch_leaf);
        let reverse_texts: Vec<String> = reverse.iter().map(|m| m.text()).collect();
        assert_eq!(reverse_texts, ["original".to_string()]);
    }

    #[test]
    fn reports_branch_points() {
        let mut tree = SessionTree::new();
        let first = tree.append(user("one")).expect("append");
        tree.append(user("two")).expect("append");
        tree.append_to(&first, user("alt")).expect("branch");
        let points = tree.branch_points();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].id, first);
    }

    #[test]
    fn forks_without_touching_the_original() {
        let mut tree = SessionTree::new();
        tree.append(user("one")).expect("append");
        tree.append(user("two")).expect("append");
        let mut fork = tree.fork_at("n000001").expect("fork");
        fork.append(user("three")).expect("append");

        assert_eq!(tree.messages().len(), 2);
        assert_eq!(fork.messages().len(), 2);
        assert_eq!(fork.messages()[1].text(), "three");
    }

    #[test]
    fn round_trips_through_json() {
        let mut tree = SessionTree::new();
        tree.append(user("one")).expect("append");
        tree.append(user("two")).expect("append");
        let json = tree.to_json();
        let restored = SessionTree::from_json(&json).expect("parse");
        assert_eq!(restored.messages().len(), 2);
        assert_eq!(restored.active(), Some("n000002"));
    }

    #[test]
    fn rejects_an_unknown_parent() {
        let mut tree = SessionTree::new();
        assert!(tree.append_to("nope", user("x")).is_err());
    }
}
