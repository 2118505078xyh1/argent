//! 把启用插件的运行时声明合并进宿主配置。
//!
//! ZCode/Claude/Codex 插件用占位符引用自身根目录与数据目录；这里统一展开，
//! CLI 与 GUI 共用同一套规则，避免两边对同一个插件解释出不同的服务器地址。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use argent_mcp::{McpConfig, ServerConfig, TransportKind};

use crate::manifest::PluginManifest;
use crate::PluginManager;

/// 复制已启用插件的 MCP server 声明到运行时配置。
///
/// 同名服务器以插件声明为准：插件启停后不需要手工改 `mcp.json`。
pub fn add_mcp_servers(
    mcp: &mut McpConfig,
    plugins: &PluginManager,
    cwd: &Path,
    storage: &Path,
) {
    let mut additions: Vec<ServerConfig> = Vec::new();
    for manifest in plugins.enabled() {
        for server in &manifest.mcp_servers {
            let endpoint = server
                .command
                .clone()
                .or_else(|| server.url.clone())
                .unwrap_or_default();
            if endpoint.is_empty() {
                continue;
            }
            let name = format!(
                "plugin_{}_{}",
                component(&manifest.name),
                component(&server.name)
            );
            let kind = match server.kind.to_ascii_lowercase().as_str() {
                "stdio" => TransportKind::Stdio,
                "sse" => TransportKind::Sse,
                _ => TransportKind::Http,
            };
            let args = server
                .args
                .iter()
                .map(|value| expand_plugin_text(value, manifest, cwd, storage))
                .collect();
            let env = server
                .env
                .iter()
                .map(|(key, value)| {
                    (
                        expand_plugin_text(key, manifest, cwd, storage),
                        expand_plugin_text(value, manifest, cwd, storage),
                    )
                })
                .collect();
            additions.push(ServerConfig {
                name,
                endpoint: expand_plugin_text(&endpoint, manifest, cwd, storage),
                args,
                env,
                cwd: server
                    .cwd
                    .as_ref()
                    .map(|value| expand_plugin_text(value, manifest, cwd, storage)),
                headers: BTreeMap::new(),
                // 插件清单目前不声明 OAuth；需要时用 mcp.json 覆盖同名服务器。
                oauth: None,
                kind,
                disabled: false,
            });
        }
    }
    for server in additions {
        mcp.servers.retain(|existing| existing.name != server.name);
        mcp.servers.push(server);
    }
}

/// 展开插件文本里的 `${...}` 占位符。
///
/// `${user_config.*}` 回退到清单里的默认值；设置界面以后可以在启动前用
/// 实际值替换。
pub fn expand_plugin_text(
    value: &str,
    manifest: &PluginManifest,
    cwd: &Path,
    storage: &Path,
) -> String {
    let root = manifest.root_string();
    let plugin_data = storage.join("plugin-data").join(&manifest.name);
    let mut out = value
        .replace("${ZCODE_PLUGIN_ROOT}", &root)
        .replace("${CLAUDE_PLUGIN_ROOT}", &root)
        .replace("${ARGENT_PLUGIN_ROOT}", &root)
        .replace("${ZCODE_PROJECT_DIR}", &cwd.display().to_string())
        .replace("${ARGENT_PROJECT_DIR}", &cwd.display().to_string())
        .replace("${ZCODE_PLUGIN_DATA}", &plugin_data.display().to_string())
        .replace("${ARGENT_PLUGIN_DATA}", &plugin_data.display().to_string());
    for spec in &manifest.user_config {
        let default = spec.default.as_str().unwrap_or("");
        out = out.replace(&format!("${{user_config.{}}}", spec.key), default);
    }
    out
}

/// 把插件或服务器名转成可做工具名前缀的片段。
pub fn component(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() || ch == '_' { ch } else { '_' })
        .collect()
}

/// 本工作区所有 hook 文档，按加载顺序排列。
///
/// 插件文档在前，随后是项目级 `.argent/hooks.json` 与用户级存储目录里的
/// `hooks.json`。CLI 的 `argent hooks trust` 与 GUI 共用这份清单，所以被审阅过
/// 的文档恰好就是能运行的文档。
pub fn workspace_hook_files(plugins: &PluginManager, cwd: &Path, storage: &Path) -> Vec<PathBuf> {
    let mut files = plugins.hook_files();
    files.push(cwd.join(".argent").join("hooks.json"));
    files.push(storage.join("hooks.json"));
    files
}

/// 加载所有受信任的 hook 文档。
///
/// 未授权的文档跳过（GUI 里以 Notice 提示，CLI 里打到 stderr）；信任检查是
/// 全部意义所在：插件 hook 是一段以操作者权限执行的 shell 命令。
///
/// `trust_path` 是信任库文件（`Paths::hook_trust()`）；`notify` 在每次跳过或
/// 加载失败时收到一行人可读的说明，传空闭包即静默。
pub fn load_trusted_hooks(
    plugins: &PluginManager,
    cwd: &Path,
    storage: &Path,
    trust_path: &Path,
    notify: &mut dyn FnMut(&str),
) -> argent_hooks::HookConfig {
    let trust = match argent_hooks::HookTrust::at(trust_path) {
        Ok(trust) => trust,
        Err(error) => {
            notify(&format!("cannot read hook trust store: {}", error));
            return argent_hooks::HookConfig::default();
        }
    };
    let mut config = argent_hooks::HookConfig::default();
    let mut seen = std::collections::BTreeSet::new();
    for file in workspace_hook_files(plugins, cwd, storage) {
        if !file.is_file() || !seen.insert(file.clone()) {
            continue;
        }
        let digest = match argent_hooks::digest_file(&file) {
            Ok(digest) => digest,
            Err(error) => {
                notify(&format!("cannot hash {}: {}", file.display(), error));
                continue;
            }
        };
        if !trust.is_trusted(&file, &digest) {
            notify(&format!(
                "hook document {} is not trusted; run `argent hooks trust grant --all-current`",
                file.display()
            ));
            continue;
        }
        match argent_hooks::HookConfig::load(&file) {
            Ok(loaded) => config.merge(loaded),
            Err(error) => notify(&format!("cannot load {}: {}", file.display(), error)),
        }
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 在工作区里放一个插件，走真实的发现流程。
    fn workspace(name: &str) -> (PathBuf, PluginManager) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "argent-plugin-runtime-{}-{}-{}",
            name,
            std::process::id(),
            nanos
        ));
        let dir = root.join("demo");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.json"),
            r#"{
                "name": "demo",
                "version": "1.0.0",
                "description": "demo plugin",
                "userConfig": {"token": {"type": "string", "default": "abc"}},
                "mcpServers": {
                    "docs": {
                        "type": "stdio",
                        "command": "${CLAUDE_PLUGIN_ROOT}/bin/docs",
                        "args": ["--data", "${ARGENT_PLUGIN_DATA}"],
                        "env": {"TOKEN": "${user_config.token}"}
                    }
                }
            }"#,
        )
        .unwrap();
        let manager = PluginManager::discover(std::slice::from_ref(&root));
        (root, manager)
    }

    fn slashed(text: &str) -> String {
        text.replace('\\', "/")
    }

    #[test]
    fn placeholders_expand_to_paths_and_user_defaults() {
        let (root, manager) = workspace("expand");
        let manifest = manager.enabled()[0];
        let text = expand_plugin_text(
            "${CLAUDE_PLUGIN_ROOT}/bin/docs ${ARGENT_PROJECT_DIR} ${ARGENT_PLUGIN_DATA} ${user_config.token}",
            manifest,
            &root,
            Path::new("C:/store"),
        );
        let text = slashed(&text);
        assert!(text.contains("bin/docs"), "{}", text);
        assert!(text.contains(&slashed(&manifest.root_string())), "{}", text);
        assert!(text.contains(&slashed(&root.display().to_string())), "{}", text);
        assert!(text.contains("C:/store/plugin-data/demo"), "{}", text);
        assert!(text.contains("abc"), "{}", text);
    }

    #[test]
    fn plugin_servers_merge_into_the_runtime_config() {
        let (root, manager) = workspace("merge");
        let mut mcp = McpConfig::default();
        add_mcp_servers(&mut mcp, &manager, &root, Path::new("C:/store"));
        let server = mcp
            .servers
            .iter()
            .find(|server| server.name == "plugin_demo_docs")
            .expect("plugin server");
        assert_eq!(server.kind, TransportKind::Stdio);
        let endpoint = slashed(&server.endpoint);
        assert!(endpoint.ends_with("bin/docs"), "{}", endpoint);
        assert!(slashed(&server.args[1]).contains("plugin-data/demo"), "{:?}", server.args);
        assert_eq!(server.env.get("TOKEN").map(String::as_str), Some("abc"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_become_tool_prefix_fragments() {
        assert_eq!(component("my-plugin"), "my_plugin");
        assert_eq!(component("ok_1"), "ok_1");
    }

    /// 把 hook 文档登记进指定的信任库。
    fn grant_all(trust_path: &Path, files: &[PathBuf]) {
        let mut trust = argent_hooks::HookTrust::at(trust_path).unwrap();
        for file in files {
            let digest = argent_hooks::digest_file(file).unwrap();
            trust.grant(file, &digest).unwrap();
        }
    }

    #[test]
    fn untrusted_hook_documents_are_skipped_with_a_notice() {
        let (root, _manager) = workspace("hooks-untrusted");
        // 插件声明一个 hook 文档（内容任意，加载器只关心信任与 JSON 形状）。
        std::fs::create_dir_all(root.join("demo").join("hooks")).unwrap();
        std::fs::write(
            root.join("demo").join("hooks").join("hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"bash","hooks":[{"type":"command","command":"echo pre"}]}]}}"#,
        )
        .unwrap();
        // 重新发现：manifest.hooks 需要在清单里声明才会进入 hook_files。
        std::fs::write(
            root.join("demo").join("plugin.json"),
            r#"{
                "name": "demo",
                "version": "1.0.0",
                "description": "demo plugin",
                "hooks": "hooks/hooks.json",
                "mcpServers": {}
            }"#,
        )
        .unwrap();
        let manager = PluginManager::discover(std::slice::from_ref(&root));
        let storage = root.join("store");
        let trust_path = root.join("trust").join("trust.json");
        // 用加载器自己的清单解析路径，Windows 上斜杠方向才与真实流程一致。
        let hook_file = {
            let files = workspace_hook_files(&manager, &root, &storage);
            assert!(
                files.iter().any(|path| *path == root.join("demo").join("hooks").join("hooks.json")),
                "the plugin contributes its document: {:?}",
                files
            );
            root.join("demo").join("hooks").join("hooks.json")
        };

        let mut notices: Vec<String> = Vec::new();
        let config = load_trusted_hooks(
            &manager,
            &root,
            &storage,
            &trust_path,
            &mut |line| notices.push(line.to_string()),
        );
        assert!(config.is_empty(), "nothing is trusted yet");
        assert!(
            notices
                .iter()
                .any(|line| line.contains("is not trusted")),
            "the operator must be told why the hook did not load: {:?}",
            notices
        );

        // 授权后同一份文档即可加载。
        grant_all(&trust_path, std::slice::from_ref(&hook_file));
        let mut notices: Vec<String> = Vec::new();
        let config = load_trusted_hooks(
            &manager,
            &root,
            &storage,
            &trust_path,
            &mut |line| notices.push(line.to_string()),
        );
        assert!(
            notices.is_empty(),
            "a granted document loads silently: {:?}",
            notices
        );
        assert_eq!(config.len(), 1);
        assert!(!config
            .for_event(argent_hooks::HookEvent::PreToolUse, Some("bash"))
            .is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_hook_files_cover_plugins_project_and_storage() {
        let (root, manager) = workspace("hooks-files");
        let storage = root.join("store");
        let files = workspace_hook_files(&manager, &root, &storage);
        assert_eq!(files.len(), 2, "the demo plugin declares no hooks");
        assert_eq!(files[0], root.join(".argent").join("hooks.json"));
        assert_eq!(files[1], storage.join("hooks.json"));
        let _ = std::fs::remove_dir_all(&root);
    }
}