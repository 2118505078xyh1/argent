//! Import selected Claude Code / Codex CLI sessions into argent.
//!
//! Both sources are JSONL transcripts. Only the human/assistant prose (and
//! assistant thinking blocks) are portable — tool-call schemas differ, so
//! tool activity is folded into a plain notice instead of being replayed.
//! The result is a normal argent session file created through
//! [`SessionIndex::create`], so restore, tree browsing and compaction work
//! as with any native session.

use std::path::{Path, PathBuf};

use argent_ai::{Message, Role};
use argent_core::error::{Error, Result};
use argent_core::json::Json;

use crate::{Entry, SessionIndex, SessionStore};

/// One utterance pulled out of a foreign transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedTurn {
    pub role: Role,
    pub text: String,
    pub thinking: Option<String>,
}

/// Detect the source format by the first lines and parse its turns.
pub fn parse(text: &str) -> Result<Vec<ImportedTurn>> {
    // 首行常是 meta（CC 的 `mode`、Codex 的 `session_meta`），扫描到第一条
    // 真实记录再判格式。
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(root) = Json::parse(line) else {
            continue;
        };
        if root.get("message").is_some()
            || root.get("parentUuid").is_some()
            || matches!(root.str_or("type", "").as_str(), "user" | "assistant")
        {
            return parse_claude(text);
        }
        if root.get("session_meta").is_some()
            || root.str_or("type", "") == "session_meta"
            || root.str_or("type", "") == "response_item"
        {
            return parse_codex(text);
        }
    }
    Err(Error::config(
        "unrecognized session format (expected Claude Code or Codex JSONL)",
    ))
}

fn serde_parse(line: &str) -> Result<Json> {
    Json::parse(line).map_err(|e| Error::config(format!("invalid JSONL line: {}", e)))
}

/// Claude Code: `{"type":"user"|"assistant","message":{"role",...}}`.
/// Content blocks are `{type:"text"|"thinking", ...}`.
fn parse_claude(text: &str) -> Result<Vec<ImportedTurn>> {
    let mut turns = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_parse(line)?;
        let Some(message) = value.get("message") else {
            continue;
        };
        let role = match message.str_or("role", "").as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        if value.bool_or("isSidechain", false) {
            continue;
        }
        let mut text_parts = Vec::new();
        let mut thinking = Vec::new();
        match message.get("content") {
            Some(Json::Arr(blocks)) => {
                for block in blocks {
                    match block.str_or("type", "").as_str() {
                        "text" | "input_text" => {
                            let part = block.str_or("text", "");
                            if !part.trim().is_empty() {
                                text_parts.push(part);
                            }
                        }
                        "thinking" => {
                            let part = block.str_or("thinking", "");
                            if !part.trim().is_empty() {
                                thinking.push(part);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some(Json::Str(s)) => text_parts.push(s.clone()),
            _ => {}
        }
        if text_parts.is_empty() && thinking.is_empty() {
            continue;
        }
        turns.push(ImportedTurn {
            role,
            text: text_parts.join("\n"),
            thinking: (!thinking.is_empty()).then(|| thinking.join("\n")),
        });
    }
    Ok(turns)
}

/// Codex: `response_item.payload.{type:"message",role,content}` and
/// `event_msg.payload.{type:"item_completed",item:{type:"UserMessage"|"AgentMessage"}}`.
fn parse_codex(text: &str) -> Result<Vec<ImportedTurn>> {
    let mut turns = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_parse(line)?;
        let payload = match value.get("payload") {
            Some(payload) => payload,
            None => continue,
        };
        let (role, content) = match value.str_or("type", "").as_str() {
            "response_item" if payload.str_or("type", "") == "message" => {
                let role = match payload.str_or("role", "").as_str() {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    _ => continue,
                };
                (role, payload.get("content").cloned())
            }
            "event_msg" if payload.str_or("type", "") == "item_completed" => {
                let item = match payload.get("item") {
                    Some(item) => item,
                    None => continue,
                };
                let role = match item.str_or("type", "").as_str() {
                    "UserMessage" => Role::User,
                    "AgentMessage" | "AgentMessageDelta" => Role::Assistant,
                    _ => continue,
                };
                (role, item.get("content").cloned())
            }
            _ => continue,
        };
        let mut text_parts = Vec::new();
        let mut thinking = Vec::new();
        if let Some(Json::Arr(blocks)) = &content {
            for block in blocks {
                // Codex uses both `text` and `input_text`/`output_text` keys.
                for key in ["text", "input_text", "output_text"] {
                    let part = block.str_or(key, "");
                    if !part.trim().is_empty() {
                        text_parts.push(part);
                        break;
                    }
                }
                let reason = block
                    .str_or("reasoning", "")
                    .pipe_if_empty(block.str_or("thinking", ""));
                if !reason.trim().is_empty() {
                    thinking.push(reason);
                }
            }
        }
        if text_parts.is_empty() && thinking.is_empty() {
            continue;
        }
        turns.push(ImportedTurn {
            role,
            text: text_parts.join("\n"),
            thinking: (!thinking.is_empty()).then(|| thinking.join("\n")),
        });
    }
    Ok(turns)
}

/// Fold the imported turns into a fresh argent session under `index`.
/// Returns the new session path.
pub fn import(
    index: &SessionIndex,
    cwd: &Path,
    title: &str,
    turns: Vec<ImportedTurn>,
) -> Result<PathBuf> {
    let path = index.create(cwd, title)?;
    let mut store = SessionStore::open(path.clone())?;
    for turn in turns {
        let mut content = Vec::new();
        if let Some(thinking) = &turn.thinking {
            content.push(argent_ai::Content::Thinking {
                text: thinking.clone(),
                signature: String::new(),
            });
        }
        if !turn.text.trim().is_empty() {
            content.push(argent_ai::Content::text(turn.text.trim_end()));
        }
        if content.is_empty() {
            continue;
        }
        let message = Message {
            role: turn.role,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        };
        store.append(&Entry::Message(message))?;
    }
    Ok(path)
}

// Small helper to keep the Codex block loop readable without a dependency.
trait PipeIfEmpty {
    fn pipe_if_empty(self, other: String) -> String;
}

impl PipeIfEmpty for String {
    fn pipe_if_empty(self, other: String) -> String {
        if self.is_empty() {
            other
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_text_and_thinking_blocks_are_imported() {
        let jsonl = r#"{"parentUuid":null,"type":"user","message":{"role":"user","content":"hello"},"uuid":"u1"}
{"type":"user","message":{"role":"user","content":[{"type":"text","text":"sidechain noise"}]},"isSidechain":true}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"think hard"},{"type":"text","text":"world"}]}}"#;
        let turns = parse(jsonl).expect("parse");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, Role::User);
        assert_eq!(turns[0].text, "hello");
        assert_eq!(turns[1].role, Role::Assistant);
        assert_eq!(turns[1].text, "world");
        assert_eq!(turns[1].thinking.as_deref(), Some("think hard"));
    }

    #[test]
    fn codex_response_items_and_completed_items_are_imported() {
        let jsonl = r#"{"type":"session_meta","payload":{"session_id":"s"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"do the thing"}]}}
{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"done"}]}}}
{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FunctionCall","name":"bash"}}}"#;
        let turns = parse(jsonl).expect("parse");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].role, Role::User);
        assert_eq!(turns[0].text, "do the thing");
        assert_eq!(turns[1].role, Role::Assistant);
        assert_eq!(turns[1].text, "done");
    }

    #[test]
    fn importing_creates_a_restorable_argent_session() {
        let dir = std::env::temp_dir().join(format!("argent-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let index = SessionIndex::new(dir.clone());
        let turns = vec![
            ImportedTurn {
                role: Role::User,
                text: "hello".to_string(),
                thinking: None,
            },
            ImportedTurn {
                role: Role::Assistant,
                text: "hi".to_string(),
                thinking: Some("be concise".to_string()),
            },
        ];
        let path = import(&index, Path::new("/work"), "Imported", turns).expect("import");
        assert!(path.is_file());
        let entries = SessionStore::read_all(&path).expect("read");
        assert_eq!(entries.len(), 3, "header + two messages");
        match &entries[1] {
            Entry::Message(m) => assert_eq!(m.role, Role::User),
            other => panic!("expected message, got {:?}", other),
        }
        // index.list 需看到它，才能被 /resume 恢复。
        let list = index.list().expect("list");
        assert!(list.iter().any(|(p, _)| p == &path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn many_claude_turns_preserve_order_and_skip_sidechains() {
        for turns in 1..=12 {
            let mut jsonl = String::new();
            for index in 0..turns {
                jsonl.push_str(&format!(
                    "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"u{index}\"}}}}\n"
                ));
                if index % 3 == 0 {
                    jsonl.push_str(
                        "{\"type\":\"user\",\"isSidechain\":true,\"message\":{\"role\":\"user\",\"content\":\"sidechain\"}}\n",
                    );
                }
                jsonl.push_str(&format!(
                    "{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"thinking\",\"thinking\":\"t{index}\"}},{{\"type\":\"text\",\"text\":\"a{index}\"}}]}}}}\n"
                ));
            }
            let parsed = parse(&jsonl).expect("parse");
            assert_eq!(parsed.len(), turns * 2);
            for index in 0..turns {
                assert_eq!(parsed[index * 2].role, Role::User);
                assert_eq!(parsed[index * 2].text, format!("u{index}"));
                assert_eq!(parsed[index * 2].thinking, None);
                assert_eq!(parsed[index * 2 + 1].role, Role::Assistant);
                assert_eq!(parsed[index * 2 + 1].text, format!("a{index}"));
                let thinking = format!("t{index}");
                assert_eq!(
                    parsed[index * 2 + 1].thinking.as_deref(),
                    Some(thinking.as_str())
                );
            }
        }
    }

    #[test]
    fn unrecognized_jsonl_is_rejected() {
        let err = parse("{\"hello\":\"world\"}").expect_err("reject");
        assert!(err.to_string().contains("unrecognized"));
    }
}
