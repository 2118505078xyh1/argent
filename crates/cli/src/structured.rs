//! Structured output: `--output-schema` gates the final answer.
//!
//! The gate is an output filter, not a retry storm: a rejected answer gets
//! exactly one correction attempt, and a second rejection fails the run
//! instead of handing a non-conforming value to the caller.

use argent_agent::{Agent, FinishReason};
use argent_core::error::Result;
use argent_core::json::Json;

use crate::render::Renderer;
use crate::JsonLinesSink;

/// Read and parse `--output-schema`: inline JSON, or a path to a file.
pub fn load_output_schema(raw: &str) -> Result<Json> {
    let text = if raw.trim_start().starts_with('{') {
        raw.to_string()
    } else {
        std::fs::read_to_string(raw).map_err(|e| {
            argent_core::Error::config(format!("--output-schema: cannot read {}: {}", raw, e))
        })?
    };
    Json::parse(&text)
        .map_err(|e| argent_core::Error::config(format!("--output-schema: invalid JSON: {}", e)))
}

/// Check the final answer against the schema; give the model one chance to
/// correct itself when it does not match.
///
/// The correction goes through a normal `run_message`, so it lands in the
/// history and in the event stream. A caller in `--mode json` sees the extra
/// request: it really happened.
pub fn enforce_output_schema(
    agent: &mut Agent,
    renderer: &mut Renderer,
    json_sink: Option<&JsonLinesSink>,
    schema: &Json,
) -> std::result::Result<Json, String> {
    let mut last_error = String::new();
    for attempt in 0..2 {
        let Some(text) = last_assistant_text(agent) else {
            return Err("the model produced no final text".to_string());
        };
        match Json::parse(text.trim()) {
            Ok(value) => match crate::schema::validate(schema, &value) {
                Ok(()) => return Ok(value),
                Err(error) => last_error = error,
            },
            Err(error) => last_error = format!("the final answer is not JSON: {}", error),
        }
        if attempt == 0 {
            crate::emit_notice(
                json_sink,
                renderer,
                &format!(
                    "structured output rejected ({}); asking for one correction",
                    last_error
                ),
            );
            let correction = format!(
                "Your previous answer was rejected: {}. Reply again with only a single \
                 JSON value that validates against the schema. No markdown fence, no \
                 explanation.",
                last_error
            );
            let result = if let Some(sink) = json_sink {
                let mut sink = sink.clone_handle();
                agent.run_message(argent_ai::Message::user(correction), &mut sink)
            } else {
                let mut sink = crate::RendererSink::new(&mut *renderer);
                agent.run_message(argent_ai::Message::user(correction), &mut sink)
            };
            match result {
                Ok(FinishReason::Complete) => {}
                Ok(reason) => {
                    return Err(format!(
                        "{} (correction run ended: {})",
                        last_error,
                        reason.describe()
                    ))
                }
                Err(error) => {
                    return Err(format!("{} (correction run failed: {})", last_error, error))
                }
            }
        }
    }
    Err(format!(
        "structured output did not match the schema: {}",
        last_error
    ))
}

/// The last assistant message with text, newest first.
pub fn last_assistant_text(agent: &Agent) -> Option<String> {
    agent
        .history()
        .iter()
        .rev()
        .find(|message| message.role == argent_ai::Role::Assistant)
        .map(|message| message.text())
        .filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A provider that answers with the next scripted reply, in order.
    struct ScriptedProvider {
        replies: Mutex<Vec<String>>,
    }

    impl ScriptedProvider {
        fn new(replies: &[&str]) -> ScriptedProvider {
            ScriptedProvider {
                replies: Mutex::new(replies.iter().map(|r| r.to_string()).collect()),
            }
        }
    }

    impl argent_ai::Provider for ScriptedProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn models(&self) -> Result<Vec<argent_ai::ModelInfo>> {
            Ok(Vec::new())
        }

        fn chat(
            &self,
            _request: &argent_ai::ChatRequest,
        ) -> Result<Box<dyn Iterator<Item = Result<argent_ai::StreamEvent>> + Send>> {
            let mut replies = self.replies.lock().expect("replies");
            let text = if replies.is_empty() {
                String::new()
            } else {
                replies.remove(0)
            };
            Ok(Box::new(std::iter::once(Ok(argent_ai::StreamEvent::Text(
                text,
            )))))
        }
    }

    fn agent_with(replies: &[&str]) -> Agent {
        Agent::new(
            Box::new(ScriptedProvider::new(replies)),
            Arc::new(argent_tools::ToolRegistry::with_builtins()),
            argent_agent::AgentConfig::new("test"),
            argent_tools::ToolCtx::new(std::env::temp_dir()),
            "system",
        )
    }

    fn city_schema() -> Json {
        Json::parse(
            r#"{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],
                "additionalProperties":false}"#,
        )
        .expect("schema")
    }

    /// Run one turn so the agent has an answer to be judged.
    fn answered(replies: &[&str]) -> Agent {
        let mut agent = agent_with(replies);
        let mut renderer = Renderer::new(false);
        let outcome = agent.run(
            "give me the answer",
            &mut crate::RendererSink::new(&mut renderer),
        );
        assert!(
            matches!(outcome, Ok(FinishReason::Complete)),
            "{:?}",
            outcome
        );
        agent
    }

    #[test]
    fn a_conforming_answer_passes() {
        let mut agent = answered(&[r#"{"city":"Beijing"}"#]);
        let mut renderer = Renderer::new(false);
        let value = enforce_output_schema(&mut agent, &mut renderer, None, &city_schema())
            .expect("should pass");
        assert_eq!(value.get("city").and_then(|v| v.as_str()), Some("Beijing"));
    }

    #[test]
    fn a_rejected_answer_gets_one_correction() {
        let mut agent = answered(&[r#"{"town":"Beijing"}"#, r#"{"city":"Beijing"}"#]);
        let mut renderer = Renderer::new(false);
        let value = enforce_output_schema(&mut agent, &mut renderer, None, &city_schema())
            .expect("should pass after a correction");
        assert_eq!(value.get("city").and_then(|v| v.as_str()), Some("Beijing"));
        // The correction is a real turn: it is in the history.
        assert!(agent.history().len() >= 3);
    }

    #[test]
    fn a_second_rejection_fails_the_run() {
        let mut agent = answered(&[r#"{"town":"Beijing"}"#, r#"{"town":"Beijing"}"#]);
        let mut renderer = Renderer::new(false);
        let error = enforce_output_schema(&mut agent, &mut renderer, None, &city_schema())
            .expect_err("should fail");
        assert!(error.contains("did not match the schema"), "{}", error);
    }

    #[test]
    fn prose_is_rejected_and_corrected() {
        let mut agent = answered(&["Beijing is nice.", r#"{"city":"Beijing"}"#]);
        let mut renderer = Renderer::new(false);
        let value = enforce_output_schema(&mut agent, &mut renderer, None, &city_schema())
            .expect("should pass after a correction");
        assert_eq!(value.get("city").and_then(|v| v.as_str()), Some("Beijing"));
    }

    #[test]
    fn a_fenced_answer_is_rejected() {
        let mut agent = answered(&[
            "```json
{\"city\":\"Beijing\"}
```",
            "```json
{\"city\":\"Beijing\"}
```",
        ]);
        let mut renderer = Renderer::new(false);
        let error = enforce_output_schema(&mut agent, &mut renderer, None, &city_schema())
            .expect_err("a fence is not JSON");
        assert!(error.contains("not JSON"), "{}", error);
    }

    #[test]
    fn an_inline_schema_is_parsed() {
        let schema = load_output_schema(r#"{"type":"object"}"#).expect("inline");
        assert!(matches!(schema, Json::Obj(_)));
    }

    #[test]
    fn a_schema_file_is_read() {
        let path = std::env::temp_dir().join(format!(
            "argent-schema-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&path, r#"{"type":"object"}"#).expect("write");
        let schema = load_output_schema(path.to_str().expect("path")).expect("file");
        assert!(matches!(schema, Json::Obj(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_broken_schema_fails_before_the_request() {
        assert!(load_output_schema("{not json}").is_err());
        assert!(load_output_schema("/no/such/schema/file.json").is_err());
    }
}
