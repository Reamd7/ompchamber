//! Port of `server/lib/session-goal/objectives.js`.
//!
//! File-backed goal objectives. Session metadata must stay light (it rides
//! every session.updated event), so the objective TEXT lives in a file under
//! the OMPChamber data dir, keyed by the SESSION ID: sessions are globally
//! unique and carry at most one goal at a time, so the mapping is fully
use std::path::{Path, PathBuf};

use serde_json::Value;

/// JS: `GOAL_OBJECTIVE_CHAR_LIMIT` (5000 chars).
pub const GOAL_OBJECTIVE_CHAR_LIMIT: usize = 5_000;

/// OpenCode session ids are URL-safe tokens; anything else is rejected before
/// touching the filesystem. JS: `/^[A-Za-z0-9_-]{4,128}$/`.
pub fn is_valid_objective_key(session_id: &str) -> bool {
    let len = session_id.len();
    (4..=128).contains(&len)
        && session_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// JS: `String(value ?? '').trim().slice(0, limit)` — accepts any JSON value.
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

pub fn chars_take(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// JS resolves `OMPCHAMBER_DATA_DIR || ~/.config/ompchamber` per call; the Rust
/// server resolves the same env once into `ServerConfig::data_dir` and hands it
/// down — equivalent for a single server process.
pub fn goals_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("goals")
}

fn objective_file_path(data_dir: &Path, session_id: &str) -> PathBuf {
    goals_dir(data_dir).join(format!("{session_id}.md"))
}

/// Mirrors the `statusCode`-tagged errors `objectives.js` throws.
#[derive(Debug, thiserror::Error)]
pub enum ObjectiveError {
    #[error("invalid session id")]
    InvalidSessionId,
    #[error("objective content is required")]
    EmptyContent,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl ObjectiveError {
    pub fn status_code(&self) -> u16 {
        match self {
            ObjectiveError::InvalidSessionId | ObjectiveError::EmptyContent => 400,
            ObjectiveError::Io(_) => 500,
        }
    }
}

fn clamp_content(content: &Value) -> String {
    chars_take(js_to_string(content).trim(), GOAL_OBJECTIVE_CHAR_LIMIT)
}

/// Write (or overwrite — a new goal replaces the old one) the session's
/// objective. JS: `writeObjective`.
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
pub async fn delete_objective(data_dir: &Path, session_id: &str) {
    if !is_valid_objective_key(session_id) {
        return;
    }
    let _ = tokio::fs::remove_file(objective_file_path(data_dir, session_id)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-session-goal-objectives-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

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

    #[tokio::test]
    async fn read_returns_none_for_invalid_key_and_missing_file() {
        let dir = temp_dir("reads");
        assert_eq!(read_objective(&dir, "bad/id").await, None);
        assert_eq!(read_objective(&dir, "ses_missing").await, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn js_to_string_coerces_scalars() {
        assert_eq!(js_to_string(&json!("a")), "a");
        assert_eq!(js_to_string(&Value::Null), "");
        assert_eq!(js_to_string(&json!(12)), "12");
        assert_eq!(js_to_string(&json!(true)), "true");
        assert_eq!(js_to_string(&json!({ "a": 1 })), "[object Object]");
    }
}
