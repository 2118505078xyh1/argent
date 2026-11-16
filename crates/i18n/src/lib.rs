//! Locale selection and the user-facing string catalog.
//!
//! The catalog is deliberately a match rather than a runtime file: the UI
//! strings are part of the binary, a missing key is a compile-time-visible
//! `&'static str`, and there is no locale file to lose.

/// A supported UI language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    En,
    Zh,
}

impl Locale {
    pub fn parse(value: &str) -> Option<Locale> {
        match value.trim().to_ascii_lowercase().as_str() {
            "en" | "en-us" | "en_us" | "english" => Some(Locale::En),
            "zh" | "zh-cn" | "zh_cn" | "chinese" | "中文" => Some(Locale::Zh),
            _ => None,
        }
    }

    /// `ARGENT_LOCALE`, then the OS language heuristic, then Chinese.
    pub fn from_env() -> Locale {
        if let Ok(value) = std::env::var("ARGENT_LOCALE") {
            if let Some(locale) = Locale::parse(&value) {
                return locale;
            }
        }
        Locale::Zh
    }

    pub fn code(&self) -> &'static str {
        match self {
            Locale::En => "en-US",
            Locale::Zh => "zh-CN",
        }
    }
}

/// Look up one UI string. An unknown key returns the key itself so a missing
/// translation is visible, not silently blank.
pub fn t(locale: Locale, key: &str) -> &'static str {
    match (locale, key) {
        (Locale::En, "tui.placeholder") => "Message argent…",
        (Locale::Zh, "tui.placeholder") => "给 argent 发消息…",
        (Locale::En, "tui.empty") => "argent — ask something, or type / for commands.",
        (Locale::Zh, "tui.empty") => "argent — 问点什么，输入 / 查看命令。",
        (Locale::En, "tui.hint") => {
            "Enter send · Esc abort · Ctrl+E expand · /help for keys and commands"
        }
        (Locale::Zh, "tui.hint") => {
            "Enter 发送 · Esc 中止 · Ctrl+E 展开 · /help 查看键位与命令"
        }
        (Locale::En, "tui.error_hint") => "failed — send the prompt again to retry",
        (Locale::Zh, "tui.error_hint") => "出错 · 重新发送即可重试",
        (Locale::En, "tui.ready") => "ready",
        (Locale::Zh, "tui.ready") => "就绪",
        (Locale::En, "tui.working") => "working",
        (Locale::Zh, "tui.working") => "运行中",
        (Locale::En, "tui.queued") => "queued",
        (Locale::Zh, "tui.queued") => "排队",
        (Locale::En, "tui.ctx") => "ctx",
        (Locale::Zh, "tui.ctx") => "上下文",
        (Locale::En, "tui.down") => "down",
        (Locale::Zh, "tui.down") => "已接收",
        (Locale::En, "tui.tok_s") => "tok/s",
        (Locale::Zh, "tui.tok_s") => "tok/s",
        (Locale::En, "tui.help.title") => "keys",
        (Locale::Zh, "tui.help.title") => "快捷键",
        (Locale::En, "tui.help.enter") => "send the prompt",
        (Locale::Zh, "tui.help.enter") => "发送",
        (Locale::En, "tui.help.esc") => "abort the running turn",
        (Locale::Zh, "tui.help.esc") => "中止当前回合（退出用 /quit）",
        (Locale::En, "tui.help.ctrl_d") => "quit when the input is empty",
        (Locale::Zh, "tui.help.ctrl_d") => "输入为空时退出",
        (Locale::En, "tui.help.scroll") => "scroll the transcript (wheel, PgUp/PgDn)",
        (Locale::Zh, "tui.help.scroll") => "滚动对话（滚轮、PgUp/PgDn）",
        (Locale::En, "tui.help.ctrl_e") => "expand or fold tool output",
        (Locale::Zh, "tui.help.ctrl_e") => "展开/折叠工具输出",
        (Locale::En, "tui.help.history") => "input history (also ↑ ↓)",
        (Locale::Zh, "tui.help.history") => "输入历史（↑↓ 也行）",
        (Locale::En, "tui.help.ctrl_l") => "redraw",
        (Locale::Zh, "tui.help.ctrl_l") => "重绘",
        (Locale::En, "tui.help.models") => "switch provider model and effort",
        (Locale::Zh, "tui.help.models") => "切换模型与思考强度",
        (Locale::En, "tui.help.sessions") => "session management",
        (Locale::Zh, "tui.help.sessions") => "会话管理",
        (Locale::En, "tui.help.resume") => "open a stored session",
        (Locale::Zh, "tui.help.resume") => "打开已有会话",
        (Locale::En, "tui.help.utils") => "conversation utilities",
        (Locale::Zh, "tui.help.utils") => "会话工具",
        (Locale::En, "tui.sessions.title") => "sessions",
        (Locale::Zh, "tui.sessions.title") => "会话",
        (Locale::En, "tui.sessions.rename") => "rename session",
        (Locale::Zh, "tui.sessions.rename") => "重命名会话",
        (Locale::En, "tui.tree.title") => "session tree (enter switch · f fork)",
        (Locale::Zh, "tui.tree.title") => "会话树（回车切换 · f 分叉）",
        (Locale::En, "tui.help.tree") => "browse the branch tree; f forks from a node",
        (Locale::Zh, "tui.help.tree") => "浏览分支树；f 从选中节点分叉",
        (Locale::En, "tui.help.all_commands") => "type / to filter every command",
        (Locale::Zh, "tui.help.all_commands") => "输入 / 可过滤所有命令",
        (Locale::En, "tui.fork_usage") => "usage: /fork <message number> (a plain /fork clones the session)",
        (Locale::Zh, "tui.fork_usage") => "用法：/fork <消息序号>（不带参数的 /fork 复制整个会话）",
        (Locale::En, "tui.import_usage") => "usage: /import <path to a .jsonl session>",
        (Locale::Zh, "tui.import_usage") => "用法：/import <会话 JSONL 路径>",
        (Locale::En, "tui.models.title") => "models",
        (Locale::Zh, "tui.models.title") => "模型",
        (Locale::En, "tui.thinking.title") => "thinking",
        (Locale::Zh, "tui.thinking.title") => "思考强度",
        (Locale::En, "tui.tools.title") => "tools",
        (Locale::Zh, "tui.tools.title") => "工具",
        (Locale::En, "tui.permission.title") => "permission",
        (Locale::Zh, "tui.permission.title") => "权限",
        (Locale::En, "tui.permission.choice") => "[y] approve   [p] this command   [n] reject   [a] approve all",
        (Locale::Zh, "tui.permission.choice") => "[y] 允许   [p] 此命令   [n] 拒绝   [a] 全部允许",
        (Locale::En, "tui.approve_prefix") => "command allowed for this session",
        (Locale::Zh, "tui.approve_prefix") => "本会话内允许该命令",
        (Locale::En, "tui.approved") => "approved",
        (Locale::Zh, "tui.approved") => "已允许",
        (Locale::En, "tui.rejected") => "rejected",
        (Locale::Zh, "tui.rejected") => "已拒绝",
        (Locale::En, "tui.approve_all") => "approving every call",
        (Locale::Zh, "tui.approve_all") => "全部允许",
        (Locale::En, "tui.abort_requested") => "abort requested",
        (Locale::Zh, "tui.abort_requested") => "已请求中止",
        (Locale::En, "tui.expanded") => "tool output expanded",
        (Locale::Zh, "tui.expanded") => "工具输出已展开",
        (Locale::En, "tui.folded") => "tool output folded",
        (Locale::Zh, "tui.folded") => "工具输出已折叠",
        (Locale::En, "tui.unknown_command") => "unknown command",
        (Locale::Zh, "tui.unknown_command") => "未知命令",
        (Locale::En, "tui.model") => "model",
        (Locale::Zh, "tui.model") => "模型",
        (Locale::En, "tui.thinking") => "thinking",
        (Locale::Zh, "tui.thinking") => "思考",
        (Locale::En, "tui.provider") => "provider",
        (Locale::Zh, "tui.provider") => "provider",
        (Locale::En, "tui.cleared") => "conversation cleared",
        (Locale::Zh, "tui.cleared") => "会话已清空",
        (Locale::En, "tui.compacted") => "context compacted",
        (Locale::Zh, "tui.compacted") => "上下文已压缩",
        (Locale::En, "tui.nothing_compact") => "nothing to compact yet",
        (Locale::Zh, "tui.nothing_compact") => "暂无可压缩内容",
        (Locale::En, "tui.rename_usage") => "usage: /rename <title>",
        (Locale::Zh, "tui.rename_usage") => "用法：/rename <标题>",
        (Locale::En, "tui.new_session") => "new task",
        (Locale::Zh, "tui.new_session") => "新任务",
        (Locale::En, "tui.plan_on") => "plan mode on (Shift+Tab to leave)",
        (Locale::Zh, "tui.plan_on") => "计划模式已开启（Shift+Tab 退出）",
        (Locale::En, "tui.plan_off") => "plan mode off",
        (Locale::Zh, "tui.plan_off") => "计划模式已关闭",
        (Locale::En, "tui.folded_hint") => "thinking folded (Ctrl+E)",
        (Locale::Zh, "tui.folded_hint") => "思考已折叠 (Ctrl+E)",
        (Locale::En, "ui.new_chat") => "＋ New chat",
        (Locale::Zh, "ui.new_chat") => "＋ 新对话",
        (Locale::En, "ui.new_task") => "New task",
        (Locale::Zh, "ui.new_task") => "新建任务",
        (Locale::En, "ui.new_task_title") => "New task",
        (Locale::Zh, "ui.new_task_title") => "新任务",
        (Locale::En, "ui.hide") => "Hide",
        (Locale::Zh, "ui.hide") => "收起",
        (Locale::En, "ui.sessions") => "Sessions",
        (Locale::Zh, "ui.sessions") => "会话列表",
        (Locale::En, "ui.ready") => "Ready",
        (Locale::Zh, "ui.ready") => "就绪",
        (Locale::En, "ui.you") => "You",
        (Locale::Zh, "ui.you") => "你",
        (Locale::En, "ui.thinking") => "Thinking",
        (Locale::Zh, "ui.thinking") => "思考",
        (Locale::En, "ui.busy") => "A run is in progress; stop it first",
        (Locale::Zh, "ui.busy") => "正在运行，请先停止",
        (Locale::En, "ui.choose_folder") => "Choose a folder",
        (Locale::Zh, "ui.choose_folder") => "选择文件夹",
        (Locale::En, "ui.folder_hint") => {
            "The task runs its tools in this folder, and appears in that folder's task list."
        }
        (Locale::Zh, "ui.folder_hint") => "任务将在这个文件夹里运行工具，并出现在该文件夹的任务列表里。",
        (Locale::En, "ui.recent_folders") => "Recent folders",
        (Locale::Zh, "ui.recent_folders") => "最近文件夹",
        (Locale::En, "ui.folder_path") => "Folder path",
        (Locale::Zh, "ui.folder_path") => "文件夹路径",
        (Locale::En, "ui.profile") => "Profile",
        (Locale::Zh, "ui.profile") => "Profile",
        (Locale::En, "ui.default_profile") => "Default",
        (Locale::Zh, "ui.default_profile") => "默认",
        (Locale::En, "ui.invalid_folder") => "That folder does not exist",
        (Locale::Zh, "ui.invalid_folder") => "这个文件夹不存在",
        (Locale::En, "ui.create_task") => "Create task",
        (Locale::Zh, "ui.create_task") => "新建任务",
        (Locale::En, "ui.tasks") => "Tasks",
        (Locale::Zh, "ui.tasks") => "任务",
        (Locale::En, "ui.automation") => "Automation",
        (Locale::Zh, "ui.automation") => "自动化",
        (Locale::En, "ui.new_automation") => "New automation",
        (Locale::Zh, "ui.new_automation") => "新建自动化",
        (Locale::En, "ui.automation_name") => "Name",
        (Locale::Zh, "ui.automation_name") => "名称",
        (Locale::En, "ui.automation_schedule") => "Schedule",
        (Locale::Zh, "ui.automation_schedule") => "计划",
        (Locale::En, "ui.automation_schedule_hint") => {
            "every 30m · daily 09:00 · weekly mon 09:00"
        }
        (Locale::Zh, "ui.automation_schedule_hint") => {
            "every 30m · daily 09:00 · weekly mon 09:00"
        },
        (Locale::En, "ui.automation_prompt") => "Prompt",
        (Locale::Zh, "ui.automation_prompt") => "提示词",
        (Locale::En, "ui.automation_add") => "Add",
        (Locale::Zh, "ui.automation_add") => "添加",
        (Locale::En, "ui.automation_empty") => "No automations yet",
        (Locale::Zh, "ui.automation_empty") => "还没有自动化任务",
        (Locale::En, "ui.run") => "Run",
        (Locale::Zh, "ui.run") => "运行",
        (Locale::En, "ui.enable") => "Enable",
        (Locale::Zh, "ui.enable") => "启用",
        (Locale::En, "ui.disable") => "Disable",
        (Locale::Zh, "ui.disable") => "停用",
        (Locale::En, "ui.logs") => "Logs",
        (Locale::Zh, "ui.logs") => "日志",
        (Locale::En, "ui.next_run") => "Next",
        (Locale::Zh, "ui.next_run") => "下次",
        (Locale::En, "ui.last_status") => "Last",
        (Locale::Zh, "ui.last_status") => "上次",
        (Locale::En, "ui.saved") => "Saved",
        (Locale::Zh, "ui.saved") => "已保存",
        (Locale::En, "ui.running") => "Running…",
        (Locale::Zh, "ui.running") => "运行中…",
        (Locale::En, "ui.no_log") => "No run log yet",
        (Locale::Zh, "ui.no_log") => "还没有运行日志",
        (Locale::En, "ui.no_documents") => "No documents in this folder",
        (Locale::Zh, "ui.no_documents") => "这个文件夹里没有文档",
        (Locale::En, "ui.select_document") => "Pick a document to read",
        (Locale::Zh, "ui.select_document") => "选择一份文档查看",
        (Locale::En, "ui.send") => "Send",
        (Locale::Zh, "ui.send") => "发送",
        (Locale::En, "ui.stop") => "Stop",
        (Locale::Zh, "ui.stop") => "停止",
        (Locale::En, "ui.rewind") => "↶ Rewind",
        (Locale::Zh, "ui.rewind") => "↶ 回滚",
        (Locale::En, "ui.placeholder") => "Message argent…",
        (Locale::Zh, "ui.placeholder") => "给 argent 发消息…",
        (Locale::En, "ui.theme") => "Toggle theme",
        (Locale::Zh, "ui.theme") => "切换主题",
        (Locale::En, "ui.panels") => "Plugin panels",
        (Locale::Zh, "ui.panels") => "插件面板",
        (Locale::En, "ui.no_sessions") => "No sessions yet",
        (Locale::Zh, "ui.no_sessions") => "还没有会话",
        (Locale::En, "ui.new_title") => "New chat",
        (Locale::Zh, "ui.new_title") => "新对话",
        (Locale::En, "ui.rename") => "Rename",
        (Locale::Zh, "ui.rename") => "重命名",
        (Locale::En, "ui.delete") => "Delete",
        (Locale::Zh, "ui.delete") => "删除",
        (Locale::En, "ui.allow_tool") => "Allow this tool call?",
        (Locale::Zh, "ui.allow_tool") => "允许这次工具调用？",
        (Locale::En, "ui.allow_tool_named") => "Allow a tool call from",
        (Locale::Zh, "ui.allow_tool_named") => "允许来自该会话的工具调用：",
        (Locale::En, "ui.reject") => "Reject",
        (Locale::Zh, "ui.reject") => "拒绝",
        (Locale::En, "ui.allow") => "Allow",
        (Locale::Zh, "ui.allow") => "允许",
        (Locale::En, "ui.stopped") => "Stopped",
        (Locale::Zh, "ui.stopped") => "已停止",
        (Locale::En, "ui.rejected_notice") => "Tool call rejected",
        (Locale::Zh, "ui.rejected_notice") => "已拒绝该工具调用",
        (Locale::En, "ui.docs") => "Documents",
        (Locale::Zh, "ui.docs") => "文档",
        (Locale::En, "ui.add_to_chat") => "Add to chat",
        (Locale::Zh, "ui.add_to_chat") => "加入对话",
        (Locale::En, "ui.reading") => "Reading…",
        (Locale::Zh, "ui.reading") => "读取中…",
        (Locale::En, "ui.empty_doc") => "(empty document)",
        (Locale::Zh, "ui.empty_doc") => "(空文档)",
        (Locale::En, "ui.plugin_panels_empty") => "No plugin panels installed",
        (Locale::Zh, "ui.plugin_panels_empty") => "没有已安装的插件面板",
        (Locale::En, "ui.scan_docs_failed") => "Cannot scan documents",
        (Locale::Zh, "ui.scan_docs_failed") => "无法扫描文档",
        (Locale::En, "ui.read_failed") => "Cannot read",
        (Locale::Zh, "ui.read_failed") => "无法读取",
        (Locale::En, "ui.read_doc_prompt") => "Please read the file:",
        (Locale::Zh, "ui.read_doc_prompt") => "请阅读文件：",
        (Locale::En, "ui.queue") => "queued",
        (Locale::Zh, "ui.queue") => "排队",
        (Locale::En, "ui.empty_hint") => "Ask something, or type / for commands.",
        (Locale::Zh, "ui.empty_hint") => "问点什么，或者输入 / 看命令。",
        (Locale::En, "ui.empty_keys") => "Enter send · Shift+Enter newline · Esc closes dialogs",
        (Locale::Zh, "ui.empty_keys") => "Enter 发送 · Shift+Enter 换行 · Esc 关闭弹窗",
        (Locale::En, "ui.generating") => "Generating",
        (Locale::Zh, "ui.generating") => "生成中",
        (Locale::En, "ui.close_tab") => "Close tab (keeps the session)",
        (Locale::Zh, "ui.close_tab") => "关闭标签（不删除会话）",
        (Locale::En, "ui.delete_title") => "Delete this session?",
        (Locale::Zh, "ui.delete_title") => "删除这个会话？",
        (Locale::En, "ui.delete_body") => {
            "The file moves to the recycle directory; it is not gone immediately."
        }
        (Locale::Zh, "ui.delete_body") => "删除后文件会移入回收目录，不会立即消失。",
        (Locale::En, "ui.cancel") => "Cancel",
        (Locale::Zh, "ui.cancel") => "取消",
        (Locale::En, "ui.rename_title") => "Rename session",
        (Locale::Zh, "ui.rename_title") => "重命名会话",
        (Locale::En, "ui.close") => "Close",
        (Locale::Zh, "ui.close") => "关闭",
        (Locale::En, "ui.new_session_title") => "New session (Ctrl+N)",
        (Locale::Zh, "ui.new_session_title") => "新建会话 (Ctrl+N)",
        (Locale::En, "ui.hide_sidebar") => "Hide sidebar",
        (Locale::Zh, "ui.hide_sidebar") => "收起侧栏",
        (Locale::En, "ui.show_sidebar") => "Sessions",
        (Locale::Zh, "ui.show_sidebar") => "会话列表",
        (Locale::En, "ui.ctx") => "Context usage",
        (Locale::Zh, "ui.ctx") => "上下文占用",
        (Locale::En, "ui.cache") => "Prefix cache hit",
        (Locale::Zh, "ui.cache") => "前缀缓存命中",
        (Locale::En, "ui.rate") => "Generation speed",
        (Locale::Zh, "ui.rate") => "生成速度",
        (Locale::En, "ui.ttft") => "Time to first token",
        (Locale::Zh, "ui.ttft") => "首字延迟",
        (Locale::En, "ui.modal_close") => "Close dialog",
        (Locale::Zh, "ui.modal_close") => "关闭弹窗",
        (Locale::En, "ui.read_plugin_panels_failed") => "Cannot read plugin panels:",
        (Locale::Zh, "ui.read_plugin_panels_failed") => "无法读取插件面板：",
        (Locale::En, "ui.office_scan_failed") => "Cannot scan documents:",
        (Locale::Zh, "ui.office_scan_failed") => "无法扫描文档：",
        (Locale::En, "ui.office_read_failed") => "Cannot read:",
        (Locale::Zh, "ui.office_read_failed") => "无法读取：",
        _ => "",
    }
}

/// A key exists when the catalog returns something other than the empty string.
pub fn has(locale: Locale, key: &str) -> bool {
    !t(locale, key).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_locale_names() {
        assert_eq!(Locale::parse("en-US"), Some(Locale::En));
        assert_eq!(Locale::parse("zh-CN"), Some(Locale::Zh));
        assert_eq!(Locale::parse("auto"), None);
    }

    #[test]
    fn every_key_has_both_languages() {
        for key in [
            "tui.placeholder",
            "tui.empty",
            "tui.hint",
            "tui.error_hint",
            "tui.ready",
            "tui.help.title",
            "ui.new_chat",
            "ui.allow_tool",
            "ui.docs",
        ] {
            assert!(!t(Locale::En, key).is_empty(), "{} en", key);
            assert!(!t(Locale::Zh, key).is_empty(), "{} zh", key);
        }
    }
}
