//! Port of `server/lib/agent-memory/feature-flag.js`.
//!
//! Whether agent memory exists at all in this build. The feature is complete
//! but not released: it ships dark so it can be tested against real work
//! without appearing to users who have not asked for it. With the flag unset
//! there is no tool, no routes, no session index and no settings row — not a
//! switch left in the off position, which would invite someone to turn on
//! something unannounced.
//!
//! Read per call rather than captured at startup, so a process started with
//! the variable set is the only thing that decides.
//!
//! 中文说明：agent memory 的 feature flag（对应 JS 端 feature-flag.js）。
//! 本功能已完成但未发布，以暗发（dark launch）模式随构建测试；flag
//! 关闭时工具、路由、会话索引与设置行全部不存在——不是留在关闭位的
//! 开关，否则会诱人打开未公告的功能。开关每次调用时读取环境变量而非
//! 启动时固化。

/// `isAgentMemoryFeatureAvailable()`.
/// 判断 agent memory 功能在本构建中是否可用：读取环境变量
/// `OMPCHAMBER_MEMORY_ENABLE`，trim 并忽略大小写后仅接受
/// `1`/`true`/`yes`/`on` 四种真值拼法；变量缺失或其它取值一律视为关闭。
pub fn is_agent_memory_feature_available() -> bool {
    let Ok(raw) = std::env::var("OMPCHAMBER_MEMORY_ENABLE") else {
        return false;
    };
    matches!(
        raw.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// flag 取值解析测试：真值拼法、非真值拼法、变量缺失与按调用读取。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    /// Environment mutation is process-global; serialize the flag tests.
    /// 环境变量是进程全局状态，用互斥锁把各 flag 测试串行化，避免
    /// 并发用例相互覆盖对方的变量值。
    fn env_lock() -> &'static Mutex<()> {
        // 进程内唯一的串行化锁，首次调用时惰性创建。
        static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
        &LOCK
    }

    /// 临时设置（`Some`）或删除（`None`）环境变量后运行 `run`，
    /// 无论成败都恢复原值；返回 `run` 的布尔结果。
    /// set_var/remove_var 修改进程全局状态，在 Rust 2024 中为 unsafe。
    fn with_var(value: Option<&str>, run: impl FnOnce() -> bool) -> bool {
        let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let original = std::env::var("OMPCHAMBER_MEMORY_ENABLE").ok();
        match value {
            Some(v) => unsafe { std::env::set_var("OMPCHAMBER_MEMORY_ENABLE", v) },
            None => unsafe { std::env::remove_var("OMPCHAMBER_MEMORY_ENABLE") },
        }
        let outcome = run();
        match original {
            Some(v) => unsafe { std::env::set_var("OMPCHAMBER_MEMORY_ENABLE", v) },
            None => unsafe { std::env::remove_var("OMPCHAMBER_MEMORY_ENABLE") },
        }
        outcome
    }

    /// 验证变量未设置时功能保持关闭。
    #[test]
    fn closed_when_the_variable_is_unset() {
        assert!(!with_var(None, is_agent_memory_feature_available));
    }

    /// 验证常见真值拼法（含大写与首尾空白）都能打开开关。
    #[test]
    fn opens_for_the_usual_truthy_spellings() {
        for value in ["1", "true", "TRUE", "yes", "on", " true "] {
            assert!(
                with_var(Some(value), is_agent_memory_feature_available),
                "should open for {value:?}"
            );
        }
    }

    /// 验证空串、0、false、no、off 等其余取值一律保持关闭。
    #[test]
    fn stays_closed_for_anything_else_including_false() {
        for value in ["", "0", "false", "no", "off", "maybe"] {
            assert!(
                !with_var(Some(value), is_agent_memory_feature_available),
                "should stay closed for {value:?}"
            );
        }
    }

    /// 验证开关按调用读取：同一进程内变量从无到有，结果随之变化。
    #[test]
    fn is_read_per_call() {
        assert!(!with_var(None, is_agent_memory_feature_available));
        assert!(with_var(Some("1"), is_agent_memory_feature_available));
    }
}
