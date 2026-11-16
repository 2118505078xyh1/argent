//! Task list, structured clarification, and web search tools.

use std::sync::{Mutex, OnceLock};

use argent_core::error::Result;
use argent_core::json::Json;

use crate::tool::{arg_bool_opt, arg_str, arg_str_opt, schema_object, Tool, ToolCtx, ToolResult};

// ---------------------------------------------------------------------------
// todo
// ---------------------------------------------------------------------------

/// One tracked task.
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub id: usize,
    pub content: String,
    pub status: String,
}

/// Shared, process-wide task list.
#[derive(Debug, Default)]
pub struct TodoStore {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoStore {
    pub fn new() -> TodoStore {
        TodoStore::default()
    }

    pub fn snapshot(&self) -> Vec<TodoItem> {
        self.items
            .lock()
            .map(|items| items.clone())
            .unwrap_or_default()
    }

    pub fn set_items(&self, items: Vec<TodoItem>) {
        if let Ok(mut guard) = self.items.lock() {
            *guard = items;
        }
    }
}

fn store() -> &'static TodoStore {
    static STORE: OnceLock<TodoStore> = OnceLock::new();
    STORE.get_or_init(TodoStore::new)
}

/// Maintain the session task list.
pub struct TodoTool;

impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Maintain the task list. Send the full list; status is pending, in_progress or completed."
    }

    fn parameters(&self) -> Json {
        let mut content = Json::obj();
        content.set("type", Json::string("string"));
        content.set("description", Json::string("Task"));
        let mut status = Json::obj();
        status.set("type", Json::string("string"));
        status.set(
            "enum",
            Json::Arr(vec![
                Json::string("pending"),
                Json::string("in_progress"),
                Json::string("completed"),
            ]),
        );
        let mut item = Json::obj();
        item.set("type", Json::string("object"));
        let mut item_props = Json::obj();
        item_props.set("content", content);
        item_props.set("status", status);
        item.set("properties", item_props);
        item.set("required", Json::Arr(vec![Json::string("content")]));

        let mut props = Json::obj();
        props.set("todos", {
            let mut arr = Json::obj();
            arr.set("type", Json::string("array"));
            arr.set("description", Json::string("The list"));
            arr.set("items", item);
            arr
        });
        let mut schema = Json::obj();
        schema.set("type", Json::string("object"));
        schema.set("properties", props);
        schema.set("required", Json::Arr(vec![Json::string("todos")]));
        schema
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let raw = match args.get("todos").and_then(Json::as_array) {
            Some(items) => items.to_vec(),
            None => {
                let snapshot = store().snapshot();
                return Ok(ToolResult::ok(render_todos(&snapshot)));
            }
        };
        let mut items = Vec::new();
        for (index, entry) in raw.iter().enumerate() {
            let content = entry.str_or("content", "");
            if content.is_empty() {
                continue;
            }
            let status = match entry.str_or("status", "pending").as_str() {
                "in_progress" => "in_progress".to_string(),
                "completed" => "completed".to_string(),
                _ => "pending".to_string(),
            };
            items.push(TodoItem {
                id: index + 1,
                content,
                status,
            });
        }
        store().set_items(items.clone());
        Ok(ToolResult::ok(render_todos(&items)))
    }
}

fn render_todos(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "task list is empty".to_string();
    }
    items
        .iter()
        .map(|item| {
            let marker = match item.status.as_str() {
                "completed" => "x",
                "in_progress" => ">",
                _ => " ",
            };
            format!("[{}] {} {}", marker, item.id, item.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// ask
// ---------------------------------------------------------------------------

/// One proposed answer to a clarification question.
#[derive(Debug, Clone)]
pub struct AskOption {
    pub label: String,
    pub description: String,
}

/// A question presented to the operator.
#[derive(Debug, Clone)]
pub struct AskQuestion {
    pub question: String,
    pub options: Vec<AskOption>,
    pub multi_select: bool,
}

/// A recorded answer.
#[derive(Debug, Clone)]
pub struct AskAnswer {
    pub question: String,
    pub selected: Vec<String>,
    pub note: Option<String>,
}

/// Where the `ask` tool delivers questions and collects answers.
pub trait AskChannel: Send + Sync {
    /// Present the questions and return the answers.
    fn ask(&self, questions: &[AskQuestion]) -> Vec<AskAnswer>;
}

/// A channel that records questions and returns the first option, used in
/// non-interactive sessions.
pub struct AutoAnswerChannel {
    answers: Mutex<Vec<AskAnswer>>,
}

impl Default for AutoAnswerChannel {
    fn default() -> Self {
        AutoAnswerChannel::new()
    }
}

impl AutoAnswerChannel {
    pub fn new() -> AutoAnswerChannel {
        AutoAnswerChannel {
            answers: Mutex::new(Vec::new()),
        }
    }

    pub fn recorded(&self) -> Vec<AskAnswer> {
        self.answers.lock().map(|a| a.clone()).unwrap_or_default()
    }
}

impl AskChannel for AutoAnswerChannel {
    fn ask(&self, questions: &[AskQuestion]) -> Vec<AskAnswer> {
        let mut out = Vec::new();
        for question in questions {
            let selected = question
                .options
                .first()
                .map(|o| vec![o.label.clone()])
                .unwrap_or_default();
            let answer = AskAnswer {
                question: question.question.clone(),
                selected,
                note: None,
            };
            if let Ok(mut guard) = self.answers.lock() {
                guard.push(answer.clone());
            }
            out.push(answer);
        }
        out
    }
}

/// A channel that asks the operator on the terminal.
///
/// Each question prints its options and reads one line from stdin. A numeric
/// answer selects that option; free text becomes the answer and the note; an
/// empty line or closed stdin falls back to the first option so a piped,
/// non-interactive run can never hang.
pub struct TerminalAnswerChannel {
    answers: Mutex<Vec<AskAnswer>>,
}

impl Default for TerminalAnswerChannel {
    fn default() -> Self {
        TerminalAnswerChannel::new()
    }
}

impl TerminalAnswerChannel {
    pub fn new() -> TerminalAnswerChannel {
        TerminalAnswerChannel {
            answers: Mutex::new(Vec::new()),
        }
    }

    pub fn recorded(&self) -> Vec<AskAnswer> {
        self.answers.lock().map(|a| a.clone()).unwrap_or_default()
    }
}

impl AskChannel for TerminalAnswerChannel {
    fn ask(&self, questions: &[AskQuestion]) -> Vec<AskAnswer> {
        use std::io::Write;
        let mut out = Vec::new();
        for question in questions {
            let mut stdout = std::io::stdout();
            let _ = write!(stdout, "\n\n\x1b[36m?\x1b[0m {}\n", question.question);
            for (index, option) in question.options.iter().enumerate() {
                let suffix = if option.description.is_empty() {
                    String::new()
                } else {
                    format!(" - {}", option.description)
                };
                let _ = write!(stdout, "  {}) {}{}\n", index + 1, option.label, suffix);
            }
            let prompt = if question.multi_select {
                "  (comma-separated numbers, or text) > "
            } else {
                "  > "
            };
            let _ = write!(stdout, "{}", prompt);
            let _ = stdout.flush();

            let mut line = String::new();
            let read = std::io::stdin().read_line(&mut line).unwrap_or(0);
            let answer_text = line.trim().to_string();

            let mut selected = Vec::new();
            let mut note = None;

            if read == 0 || answer_text.is_empty() {
                if let Some(first) = question.options.first() {
                    selected.push(first.label.clone());
                }
            } else if answer_text
                .chars()
                .all(|c| c.is_ascii_digit() || c == ',' || c.is_whitespace())
            {
                for token in answer_text.split(',') {
                    if let Ok(index) = token.trim().parse::<usize>() {
                        if let Some(option) = question.options.get(index.saturating_sub(1)) {
                            selected.push(option.label.clone());
                        }
                    }
                }
                if selected.is_empty() {
                    if let Some(first) = question.options.first() {
                        selected.push(first.label.clone());
                    }
                }
            } else {
                selected.push(answer_text.clone());
                note = Some(answer_text);
            }

            let answer = AskAnswer {
                question: question.question.clone(),
                selected,
                note,
            };
            if let Ok(mut guard) = self.answers.lock() {
                guard.push(answer.clone());
            }
            out.push(answer);
        }
        out
    }
}

/// Ask the operator a structured question instead of guessing.
pub struct AskTool;

impl Tool for AskTool {
    fn name(&self) -> &str {
        "ask"
    }

    fn description(&self) -> &str {
        "Ask the operator a structured question with options instead of guessing. Use when a decision materially changes the result."
    }

    fn parameters(&self) -> Json {
        let mut label = Json::obj();
        label.set("type", Json::string("string"));
        label.set("description", Json::string("Short label"));
        let mut description = Json::obj();
        description.set("type", Json::string("string"));
        description.set("description", Json::string("Option detail"));
        let mut option = Json::obj();
        option.set("type", Json::string("object"));
        let mut option_props = Json::obj();
        option_props.set("label", label);
        option_props.set("description", description);
        option.set("properties", option_props);
        option.set("required", Json::Arr(vec![Json::string("label")]));

        let mut question_text = Json::obj();
        question_text.set("type", Json::string("string"));
        question_text.set("description", Json::string("The question"));
        let mut multi = Json::obj();
        multi.set("type", Json::string("boolean"));
        multi.set("description", Json::string("Allow multiple selections"));

        let mut question = Json::obj();
        question.set("type", Json::string("object"));
        let mut question_props = Json::obj();
        question_props.set("question", question_text);
        question_props.set("options", {
            let mut arr = Json::obj();
            arr.set("type", Json::string("array"));
            arr.set("items", option);
            arr
        });
        question_props.set("multiSelect", multi);
        question.set("properties", question_props);
        question.set("required", Json::Arr(vec![Json::string("question")]));

        let mut props = Json::obj();
        props.set("questions", {
            let mut arr = Json::obj();
            arr.set("type", Json::string("array"));
            arr.set("description", Json::string("Questions to ask the operator"));
            arr.set("items", question);
            arr
        });
        let mut schema = Json::obj();
        schema.set("type", Json::string("object"));
        schema.set("properties", props);
        schema.set("required", Json::Arr(vec![Json::string("questions")]));
        schema
    }

    fn execute(&self, ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let raw = args
            .get("questions")
            .and_then(Json::as_array)
            .map(|items| items.to_vec())
            .unwrap_or_default();

        let mut questions = Vec::new();
        for entry in &raw {
            let text = entry.str_or("question", "");
            if text.is_empty() {
                continue;
            }
            let options = entry
                .get("options")
                .and_then(Json::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|opt| AskOption {
                            label: opt.str_or("label", ""),
                            description: opt.str_or("description", ""),
                        })
                        .filter(|o| !o.label.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let multi_select = entry.bool_or("multiSelect", false);
            questions.push(AskQuestion {
                question: text,
                options,
                multi_select,
            });
        }

        if questions.is_empty() {
            return Ok(ToolResult::error("no valid question supplied"));
        }

        // The channel is supplied through the tool context.
        let channel = ctx.var("ask_channel").map(|name| name.to_string());
        let answers = match channel.as_deref() {
            Some("terminal") => store_terminal_channel().ask(&questions),
            _ => store_auto_channel().ask(&questions),
        };

        let mut lines = Vec::new();
        for answer in &answers {
            lines.push(format!(
                "Q: {}\nA: {}{}",
                answer.question,
                answer.selected.join(", "),
                match &answer.note {
                    Some(note) => format!(" ({})", note),
                    None => String::new(),
                }
            ));
        }
        Ok(ToolResult::ok(lines.join("\n")))
    }
}

fn store_auto_channel() -> &'static AutoAnswerChannel {
    static CHANNEL: OnceLock<AutoAnswerChannel> = OnceLock::new();
    CHANNEL.get_or_init(AutoAnswerChannel::new)
}

/// The process-wide interactive channel, selected with
/// `ask_channel = "terminal"`.
pub fn store_terminal_channel() -> &'static TerminalAnswerChannel {
    static CHANNEL: OnceLock<TerminalAnswerChannel> = OnceLock::new();
    CHANNEL.get_or_init(TerminalAnswerChannel::new)
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

/// Submit a completed plan for approval.
pub struct SubmitPlanTool;

impl Tool for SubmitPlanTool {
    fn name(&self) -> &str {
        "submit_plan"
    }

    fn description(&self) -> &str {
        "Submit a completed plan for approval before executing it."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![("plan", "string", "The plan, as a numbered list of steps")],
            vec!["plan"],
        )
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let plan = arg_str(args, "plan")?;
        Ok(ToolResult::ok(format!("plan submitted:\n{}", plan)))
    }
}

/// Ask the operator whether a plan was approved.
pub fn plan_approved(approved: bool) -> String {
    if approved {
        "plan approved; proceed with execution".to_string()
    } else {
        "plan rejected; revise before executing".to_string()
    }
}

/// Read an optional string, exported for reuse.
pub fn optional_string(args: &Json, key: &str) -> Option<String> {
    arg_str_opt(args, key)
}

/// Read an optional boolean, exported for reuse.
pub fn optional_bool(args: &Json, key: &str) -> Option<bool> {
    arg_bool_opt(args, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_renders_status_markers() {
        let store = TodoStore::new();
        store.set_items(vec![TodoItem {
            id: 1,
            content: "build".to_string(),
            status: "in_progress".to_string(),
        }]);
        let rendered = render_todos(&store.snapshot());
        assert!(rendered.contains("[>] 1 build"));
    }

    #[test]
    fn todo_tool_updates_the_store() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let todo = TodoTool;
        let out = todo
            .execute(
                &ctx,
                &Json::parse(r#"{"todos":[{"content":"a","status":"completed"}]}"#).unwrap(),
            )
            .unwrap();
        assert!(out.content.contains("[x] 1 a"));
        assert_eq!(store().snapshot().len(), 1);
    }

    #[test]
    fn ask_returns_the_first_option() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let ask = AskTool;
        let out = ask
            .execute(
                &ctx,
                &Json::parse(
                    r#"{"questions":[{"question":"q","options":[{"label":"A"},{"label":"B"}]}]}"#,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(out.content.contains("A"));
    }

    #[test]
    fn terminal_channel_records_an_answer_without_hanging() {
        // stdin is not a TTY under the test harness, so the channel must fall
        // back to the first option rather than block forever.
        let channel = TerminalAnswerChannel::new();
        let questions = vec![AskQuestion {
            question: "pick one".to_string(),
            options: vec![
                AskOption {
                    label: "optA".to_string(),
                    description: "first".to_string(),
                },
                AskOption {
                    label: "optB".to_string(),
                    description: "second".to_string(),
                },
            ],
            multi_select: false,
        }];
        let answers = channel.ask(&questions);
        assert_eq!(answers.len(), 1);
        assert!(!answers[0].selected.is_empty());
        assert_eq!(channel.recorded().len(), 1);
    }

    #[test]
    fn ask_tool_uses_the_terminal_channel_when_selected() {
        let mut ctx = ToolCtx::new(std::env::temp_dir());
        ctx.vars
            .insert("ask_channel".to_string(), "terminal".to_string());
        let ask = AskTool;
        let out = ask
            .execute(
                &ctx,
                &Json::parse(r#"{"questions":[{"question":"q","options":[{"label":"A"}]}]}"#)
                    .unwrap(),
            )
            .unwrap();
        assert!(!out.is_error);
        assert!(!store_terminal_channel().recorded().is_empty());
    }

    #[test]
    fn submit_plan_returns_the_plan() {
        let ctx = ToolCtx::new(std::env::temp_dir());
        let tool = SubmitPlanTool;
        let out = tool
            .execute(&ctx, &Json::parse(r#"{"plan":"1. do it"}"#).unwrap())
            .unwrap();
        assert!(out.content.contains("1. do it"));
    }
}
