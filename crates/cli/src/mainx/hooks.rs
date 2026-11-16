//! Hook dispatch: permission, notification, pre-compaction, session end.


use crate::*;

/// Run `PermissionRequest` hooks and return a blocking reason, if any.
pub(crate) fn permission_hook_block(
    config: &HookConfig,
    cwd: &Path,
    tool: &str,
    args: &Json,
) -> Option<String> {
    let ctx = HookContext {
        cwd: cwd.to_path_buf(),
        tool_name: Some(tool.to_string()),
        tool_input: Some(args.dump()),
        ..HookContext::default()
    };
    let mut block = None;
    for hook in config.for_event(HookEvent::PermissionRequest, Some(tool)) {
        let outcome = argent_hooks::run_command(HookEvent::PermissionRequest, hook, &ctx);
        if !outcome.success && hook.blocks_on_failure() && block.is_none() {
            block = Some(clip_hook_output(&outcome.stdout, &outcome.stderr));
        }
    }
    block
}

/// 在需要用户裁决前触发 `Notification`。
///
/// Claude Code 用这个事件做“需要你操作”的提醒（桌面通知、声音等）。
/// 失败不影响审批流程，所以结果直接丢弃。
pub(crate) fn notification_hook(config: &HookConfig, cwd: &Path, tool: &str, args: &Json) {
    if config.for_event(HookEvent::Notification, Some(tool)).is_empty() {
        return;
    }
    let ctx = HookContext {
        cwd: cwd.to_path_buf(),
        tool_name: Some(tool.to_string()),
        tool_input: Some(args.dump()),
        message: Some(format!("approval needed for {}", tool)),
        ..HookContext::default()
    };
    let _ = argent_hooks::run(config, HookEvent::Notification, &ctx);
}

/// 压缩真正发生前触发 `PreCompact`。
pub(crate) fn pre_compact_hook(config: &HookConfig, cwd: &Path) {
    let ctx = HookContext {
        cwd: cwd.to_path_buf(),
        ..HookContext::default()
    };
    let _ = argent_hooks::run(config, HookEvent::PreCompact, &ctx);
}

/// 一次运行结束时触发 `SessionEnd`。
pub(crate) fn session_end_hook(config: &HookConfig, cwd: &Path) {
    let ctx = HookContext {
        cwd: cwd.to_path_buf(),
        ..HookContext::default()
    };
    let _ = argent_hooks::run(config, HookEvent::SessionEnd, &ctx);
}

/// Every hook document that belongs to this workspace, in load order.
///
/// Shared by the loader and by `argent hooks trust`, so a document that is reviewed
/// is exactly a document that can run.
pub(crate) fn workspace_hook_files(plugins: &PluginManager, cwd: &Path) -> Vec<PathBuf> {
    argent_plugins::workspace_hook_files(plugins, cwd, Paths::new(cwd).storage())
}

/// Load every trusted hook document for this workspace.
///
/// A hook document that has not been granted is skipped with a notice. The
/// trust check is the whole point: a plugin hook is arbitrary shell code.
pub(crate) fn load_hooks(plugins: &PluginManager, cwd: &Path) -> HookConfig {
    let paths = Paths::new(cwd);
    argent_plugins::load_trusted_hooks(plugins, cwd, paths.storage(), &paths.hook_trust(), &mut |line| {
        eprintln!("{}", line);
    })
}

pub(crate) fn clip_hook_output(stdout: &str, stderr: &str) -> String {
    let text = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    let mut out: String = text.chars().take(300).collect();
    if text.chars().count() > 300 {
        out.push('…');
    }
    out.replace(['\n', '\r'], " ")
}
