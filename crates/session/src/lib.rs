//! argent session layer: JSONL transcripts, session trees and compaction.

pub mod controller;
pub mod sqlite;
pub mod store;
pub mod tree;
pub mod usage;

pub use store::{
    compact, effective_reserved, entries_to_messages, is_empty_transcript, local_stamp, now_iso,
    sanitize, should_compact, should_compact_real, summarize, sweep_empty_sessions, tail_start,
    timestamp_slug, trim_to, trim_to_tokens, Entry, SessionIndex, SessionStore,
};
pub use tree::{Node, SessionTree};
