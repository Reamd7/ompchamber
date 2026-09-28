//! Port of `server/lib/session-goal/objectives.js`.
//!
//! File-backed goal objectives. Session metadata must stay light (it rides
//! every session.updated event), so the objective TEXT lives in a file under
//! the OMPChamber data dir, keyed by the SESSION ID: sessions are globally
//! unique and carry at most one goal at a time, so the mapping is fully
//! （中文概览）会话元数据必须保持轻量（它随每条 session.updated 事件
//! 传输），因此 objective 正文落在 OMPChamber data 目录下以 session id
//! 命名的文件里。
use std::path::{Path, PathBuf};

use serde_json::Value;

/// JS: `GOAL_OBJECTIVE_CHAR_LIMIT` (5000 chars).
/// 目标正文的字符上限；超长内容在写入前被截断。
pub const GOAL_OBJECTIVE_CHAR_LIMIT: usize = 5_000;

/// OpenCode session ids are URL-safe tokens; anything else is rejected before
/// touching the filesystem. JS: `/^[A-Za-z0-9_-]{4,128}$/`.
/// 校验 session id 是否为 4–128 位的 URL 安全 token（字母/数字/`-`/`_`），
/// 不合法的 key 在触碰文件系统之前即被拒绝。
pub fn is_valid_objective_key(session_id: &str) -> bool {
    let len = session_id.len();
    (4..=128).contains(&len)
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// JS: `String(value ?? '').trim().slice(0, limit)` — accepts any JSON value.
/// 按 JS `String()` 规则把任意 JSON 值强转为字符串：null 得空串、数组按
/// 元素强转后以 `,` 连接、对象得 "[object Object]"。
pub fn js_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        // JS String([a, b]) joins element coercions with ','.
        Value::Array(items) => items.iter().map(js_to_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

/// 按 Unicode 字符（而非字节）截取前 `limit` 个字符，与 JS `slice` 语义对齐。
pub fn chars_take(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// JS resolves `OMPCHAMBER_DATA_DIR || ~/.config/ompchamber` per call; the Rust
/// server resolves the same env once into `ServerConfig::data_dir` and hands it
/// down — equivalent for a single server process.
/// 目标文件的根目录 `<data_dir>/goals`（不存在时由写入方负责创建）。
pub fn goals_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("goals")
}

/// 单个目标文件路径 `<goals>/<session_id>.md`；调用前需先通过
/// is_valid_objective_key 校验，避免路径穿越。
fn objective_file_path(data_dir: &Path, session_id: &str) -> PathBuf {
    goals_dir(data_dir).join(format!("{session_id}.md"))
}

/// Mirrors the `statusCode`-tagged errors `objectives.js` throws.
/// 目标读写错误；每个变体经 status_code 方法映射为 JS 版对应的 HTTP 状态码。
#[derive(Debug, thiserror::Error)]
pub enum ObjectiveError {
    /// session id 未通过格式校验（映射 400）。
    #[error("invalid session id")]
    InvalidSessionId,
    /// 强转并去空白后正文为空（映射 400）。
    #[error("objective content is required")]
    EmptyContent,
    /// 底层文件 IO 错误（映射 500）。
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// HTTP 状态码映射，等价于 JS 抛错时附带的 `statusCode` 字段。
impl ObjectiveError {
    /// 返回该错误对应的 HTTP 状态码：参数类错误 400，IO 错误 500。
    pub fn status_code(&self) -> u16 {
        match self {
            ObjectiveError::InvalidSessionId | ObjectiveError::EmptyContent => 400,
            ObjectiveError::Io(_) => 500,
        }
    }
}

/// 写入前的正文规范化：JS String 强转 → trim → 截断到字符上限。
fn clamp_content(content: &Value) -> String {
    chars_take(js_to_string(content).trim(), GOAL_OBJECTIVE_CHAR_LIMIT)
}

/// Write (or overwrite — a new goal replaces the old one) the session's
/// objective. JS: `writeObjective`.
/// 写入（或覆盖——一个会话同一时刻只有一个 goal）指定会话的 objective
/// 文件并返回实际落盘文本；id 非法或正文为空返回对应 400 错误。
pub async fn write_objective(
    data_dir: &Path,
    session_id: &str,
    content: &Value,
) -> Result<String, ObjectiveError> {
    if !is_valid_objective_key(session_id) {
        return Err(ObjectiveError::InvalidSessionId);
    }
    let text = clamp_content(content);
    if text.is_empty() {
        return Err(ObjectiveError::EmptyContent);
    }
    tokio::fs::create_dir_all(goals_dir(data_dir)).await?;
    tokio::fs::write(objective_file_path(data_dir, session_id), &text).await?;
    Ok(text)
}

/// Returns the objective text, or `None` when missing/invalid. JS:
/// `readObjective` (any read error maps to null).
/// 读取 objective 文本；文件缺失、读取失败或 id 非法一律返回 `None`，
/// 结果同样 trim 并截断到上限。
pub async fn read_objective(data_dir: &Path, session_id: &str) -> Option<String> {
    if !is_valid_objective_key(session_id) {
        return None;
    }
    match tokio::fs::read_to_string(objective_file_path(data_dir, session_id)).await {
        Ok(raw) => Some(chars_take(raw.trim(), GOAL_OBJECTIVE_CHAR_LIMIT)),
        Err(_) => None,
    }
}

/// Best-effort delete; missing files are fine. JS: `deleteObjective`.
/// 尽力删除 objective 文件；文件本就不存在或 id 非法时不视为错误。
pub async fn delete_objective(data_dir: &Path, session_id: &str) {
    if !is_valid_objective_key(session_id) {
        return;
    }
    let _ = tokio::fs::remove_file(objective_file_path(data_dir, session_id)).await;
}

/// objectives 模块测试套件：覆盖 key 校验、读写删往返、截断与错误映射。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 为测试创建按标签隔离的临时目录（带进程 id 防并行冲突）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-session-goal-objectives-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// 契约：key 校验与 JS 正则 `/^[A-Za-z0-9_-]{4,128}$/` 判定一致，
    /// 拒绝过短、过长及含路径分隔符/空格的 id。
    #[test]
    fn objective_key_validation_matches_js_pattern() {
        assert!(is_valid_objective_key("ses_1234"));
        assert!(is_valid_objective_key("A-b_C"));
        assert!(!is_valid_objective_key("abc")); // too short
        assert!(!is_valid_objective_key("")); // too short
        assert!(!is_valid_objective_key(&"x".repeat(129)));
        assert!(!is_valid_objective_key("ses/../../etc"));
        assert!(!is_valid_objective_key("ses 1234"));
    }

    /// 契约：写入的正文可原样读回，删除后读取变为 `None`。
    #[tokio::test]
    async fn write_read_delete_roundtrip() {
        let dir = temp_dir("roundtrip");
        let written = write_objective(&dir, "ses_round", &json!("Finish the task"))
            .await
            .expect("write");
        assert_eq!(written, "Finish the task");
        assert_eq!(
            read_objective(&dir, "ses_round").await,
            Some("Finish the task".to_string())
        );

        delete_objective(&dir, "ses_round").await;
        assert_eq!(read_objective(&dir, "ses_round").await, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 契约：写入前 trim 首尾空白并把正文截断到 5000 字符上限。
    #[tokio::test]
    async fn write_clamps_and_trims_content() {
        let dir = temp_dir("clamp");
        let long = "x".repeat(6_000);
        let written = write_objective(&dir, "ses_clamp", &json!(format!("  {long}  ")))
            .await
            .expect("write");
        assert_eq!(written.chars().count(), GOAL_OBJECTIVE_CHAR_LIMIT);
        let stored = read_objective(&dir, "ses_clamp").await.expect("read back");
        assert_eq!(stored.chars().count(), GOAL_OBJECTIVE_CHAR_LIMIT);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 契约：非法 id 与空白/null 正文均以 400 拒绝，且错误文案与 JS 版一致。
    #[tokio::test]
    async fn write_rejects_invalid_key_and_empty_content() {
        let dir = temp_dir("rejects");
        let error = write_objective(&dir, "../escape", &json!("text"))
            .await
            .unwrap_err();
        assert_eq!(error.status_code(), 400);
        assert_eq!(error.to_string(), "invalid session id");

        let error = write_objective(&dir, "ses_ok123", &json!("   "))
            .await
            .unwrap_err();
        assert_eq!(error.status_code(), 400);
        assert_eq!(error.to_string(), "objective content is required");

        let error = write_objective(&dir, "ses_ok123", &Value::Null)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "objective content is required");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 契约：非法 id 或文件不存在时读取返回 `None` 而非报错。
    #[tokio::test]
    async fn read_returns_none_for_invalid_key_and_missing_file() {
        let dir = temp_dir("reads");
        assert_eq!(read_objective(&dir, "bad/id").await, None);
        assert_eq!(read_objective(&dir, "ses_missing").await, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 契约：js_to_string 对字符串/null/数字/布尔/对象的强转结果与 JS 一致。
    #[test]
    fn js_to_string_coerces_scalars() {
        assert_eq!(js_to_string(&json!("a")), "a");
        assert_eq!(js_to_string(&Value::Null), "");
        assert_eq!(js_to_string(&json!(12)), "12");
        assert_eq!(js_to_string(&json!(true)), "true");
        assert_eq!(js_to_string(&json!({ "a": 1 })), "[object Object]");
    }
}
