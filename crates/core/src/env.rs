//! 环境变量兼容层。
//!
//! 项目从 `mu` 改名为 `argent` 时，环境变量前缀也从 `MU_` 换成了 `ARGENT_`。
//! 用户旧脚本、旧 shell profile 里可能还留着 `MU_*` 导出；这个模块把它们镜像
//! 到对应的 `ARGENT_*`，让已有配置继续生效。零依赖。

/// 旧版环境变量前缀。
pub const LEGACY_PREFIX: &str = "MU_";

/// 新版环境变量前缀。
pub const PREFIX: &str = "ARGENT_";

/// 把 `MU_<X>` 环境变量镜像成 `ARGENT_<X>`。
///
/// 只补不覆盖：某个 `ARGENT_<X>` 已经存在时保持原值，这样显式设置的新变量优先。
/// 在进程启动早期调用一次即可。edition 2021 下 `std::env::set_var` 是安全函数。
pub fn inherit_legacy() {
    // 先收集再写入：`vars()` 借用了进程环境，边遍历边 `set_var` 属于未定义行为。
    let mirrored: Vec<(String, String)> = std::env::vars()
        .filter_map(|(key, value)| {
            let suffix = key.strip_prefix(LEGACY_PREFIX)?;
            Some((format!("{}{}", PREFIX, suffix), value))
        })
        .collect();
    for (key, value) in mirrored {
        if std::env::var_os(&key).is_none() {
            std::env::set_var(&key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 环境变量是进程全局的，而 cargo test 默认多线程；这条锁把本模块里改动
    /// 环境的用例串起来，避免互相踩。
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn legacy_variables_are_mirrored() {
        let _guard = env_lock();
        // 唯一前缀，避免和真实环境或其它用例撞名。
        let legacy = "MU_SELFTEST_MIRROR_ALPHA_7f3a";
        let current = "ARGENT_SELFTEST_MIRROR_ALPHA_7f3a";
        std::env::set_var(legacy, "old-value");
        std::env::remove_var(current);

        inherit_legacy();

        assert_eq!(std::env::var(current).as_deref(), Ok("old-value"));
        std::env::remove_var(legacy);
        std::env::remove_var(current);
    }

    #[test]
    fn an_existing_current_variable_wins() {
        let _guard = env_lock();
        let legacy = "MU_SELFTEST_MIRROR_BETA_7f3a";
        let current = "ARGENT_SELFTEST_MIRROR_BETA_7f3a";
        std::env::set_var(legacy, "old-value");
        std::env::set_var(current, "new-value");

        inherit_legacy();

        assert_eq!(std::env::var(current).as_deref(), Ok("new-value"));
        std::env::remove_var(legacy);
        std::env::remove_var(current);
    }
}
