//! walkthrough 专用模型设置：直接容错地读取 `<data_dir>/settings.json`
//! 的 `walkthroughModelOverride`。文件缺失、不可读或 JSON 非法都等价于
//! “无覆盖，走 small model 默认解析”。
//! Port of `server/lib/walkthrough/model-settings.js`.
//!
//! The walkthrough may run on a different model than the rest of the
//! small-model callers. Those callers want cheap and fast; this one needs
//! structured output and enough context for a whole diff, and forcing one
//! setting to serve both means having to degrade one feature to fix the other.
//!
//! Like the JS module, this reads `<data_dir>/settings.json` directly and
//! tolerantly — no settings file, unreadable, or malformed all mean the same
//! thing: no override, use the small model.

use std::path::Path;

use serde_json::Value;

/// 读取用户显式选择的 walkthrough 模型；`None` 表示回落 small model。
/// The explicit walkthrough model, or `None` to fall back to normal
/// small-model resolution.
///
/// Having chosen a model *is* the opt-out; a separate toggle would let the
/// two disagree, and then clearing the picker would leave a setting that says
/// "do not use the small model" with nothing to use instead.
pub fn read_walkthrough_model_override(data_dir: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(data_dir.join("settings.json")).ok()?;
    let settings: Value = serde_json::from_str(&raw).ok()?;
    if !settings.is_object() {
        return None;
    }
    let override_value = match settings.get("walkthroughModelOverride") {
        Some(Value::String(value)) => value.trim().to_string(),
        _ => String::new(),
    };
    (!override_value.is_empty()).then_some(override_value)
}

/// 设置读取的容错性测试。
#[cfg(test)]
mod tests {
    use super::*;

/// 为每个测试分配互不冲突的临时 data 目录。
    fn temp_data_dir() -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "walkthrough-model-settings-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

/// 向临时目录写入 settings.json 内容。
    fn write(dir: &std::path::PathBuf, value: &str) {
        std::fs::write(dir.join("settings.json"), value).unwrap();
    }

/// 显式写入的模型原样读出。
    #[test]
    fn returns_the_chosen_model() {
        let dir = temp_data_dir();
        write(
            &dir,
            r#"{"walkthroughModelOverride": "anthropic/claude-haiku-4-5"}"#,
        );
        assert_eq!(
            read_walkthrough_model_override(&dir),
            Some("anthropic/claude-haiku-4-5".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

/// 空对象与空白字符串都读作“未选择”，回落 small model。
    #[test]
    fn defers_to_the_small_model_when_nothing_is_chosen() {
        let dir = temp_data_dir();
        write(&dir, "{}");
        assert_eq!(read_walkthrough_model_override(&dir), None);

        // Clearing the picker writes an empty string; that must read as "use
        // the small model", not as an override of ''.
        write(&dir, r#"{"walkthroughModelOverride": ""}"#);
        assert_eq!(read_walkthrough_model_override(&dir), None);

        write(&dir, r#"{"walkthroughModelOverride": "   "}"#);
        assert_eq!(read_walkthrough_model_override(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

/// 文件缺失或内容非法时不报错，一律返回 None。
    #[test]
    fn never_fails_on_a_missing_or_corrupt_settings_file() {
        let dir = temp_data_dir();
        assert_eq!(read_walkthrough_model_override(&dir), None);

        write(&dir, "{ not json");
        assert_eq!(read_walkthrough_model_override(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

/// 与 smallModelOverride 等其它设置键互不干扰。
    #[test]
    fn is_independent_of_the_small_model_override() {
        let dir = temp_data_dir();
        write(
            &dir,
            r#"{
                "smallModelUseDefault": false,
                "smallModelOverride": "google/gemini-2.5-flash",
                "walkthroughModelOverride": "anthropic/claude-haiku-4-5"
            }"#,
        );
        assert_eq!(
            read_walkthrough_model_override(&dir),
            Some("anthropic/claude-haiku-4-5".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
