use argent_agent::{builtin_roles, ApprovalScope, ChildFactory, RoleRegistry};
use argent_tools::{DelegateBatch, DelegateRequest, ToolRegistry};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn main() {
    let mut roles = RoleRegistry::new();
    for role in builtin_roles() {
        roles.register(role);
    }

    let factory = ChildFactory {
        base_url: "http://127.0.0.1:3425/v1".to_string(),
        api_key: None,
        protocol: "openai-chat-completions".to_string(),
        model: "auto/deepseek-v4.1-flash".to_string(),
        thinking: "low".to_string(),
        models: Vec::new(),
        roles: Arc::new(Mutex::new(roles)),
        parent_registry: ToolRegistry::with_builtins(),
        cwd: std::env::temp_dir(),
        policy: None,
        audit: None,
        pre_tool: None,
        post_tool: None,
        approve: None,
        approval_scope: ApprovalScope::DestructiveOnly,
        progress: None,
        checkpoints: None,
        promotions: None,
    };

    let batch = DelegateBatch {
        tasks: vec![
            DelegateRequest {
                role: "scout".into(),
                task: "Reply with the single word: ALPHA".into(),
                model: None,
                thinking: None,
            },
            DelegateRequest {
                role: "oracle".into(),
                task: "Reply with the single word: BETA".into(),
                model: None,
                thinking: None,
            },
            DelegateRequest {
                role: "reviewer".into(),
                task: "Reply with the single word: GAMMA".into(),
                model: None,
                thinking: None,
            },
        ],
        timeout: Duration::from_secs(120),
    };

    let started = Instant::now();
    let outcomes = factory.run_batch(batch);
    println!("batch took {:?}", started.elapsed());
    for (i, o) in outcomes.iter().enumerate() {
        println!(
            "[{}] {} ok={} timed_out={} {:.1}s -> {}",
            i,
            o.role,
            !o.is_error,
            o.timed_out,
            o.elapsed.as_secs_f64(),
            o.output.chars().take(60).collect::<String>()
        );
    }
}
