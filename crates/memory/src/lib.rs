//! argent memory layer: a plain-text, cross-session fact store.

pub mod store;

pub use store::{MemoryEntry, MemoryStore, Scope};
