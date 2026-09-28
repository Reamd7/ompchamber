//! Port of the session-list sanitizer in `server/lib/opencode/proxy.js`
//! (`sanitizeSessionListItem` / `sanitizeSessionListPayload`): the session
//! LIST endpoints strip fields the UI must not rely on (`permission`,
//! oversized `revert` markers, and summary `diffs`) while session DETAIL
//! responses pass through untouched.
//!
//! 中文说明：移植 proxy.js 里的会话列表脱敏器——LIST 端点剥离 UI 不应
//! 依赖的字段（permission、超大的 revert 标记、summary 里的 diffs），
//! DETAIL 响应则原样放行。

use serde_json::{Map, Value};

/// `SESSION_LIST_ALLOWED_FIELDS` in proxy.js.
/// 中文：会话列表项允许保留的字段白名单。
const SESSION_LIST_ALLOWED_FIELDS: &[&str] = &[
    "id",
    "slug",
    "projectID",
    "workspaceID",
    "directory",
    "path",
    "parentID",
    "title",
    "agent",
    "model",
    "version",
    "time",
    "cost",
    "tokens",
    "share",
    "metadata",
    "project",
];

/// 列表级入口：数组则逐项脱敏；非数组 payload 原样返回（与 JS 一致）。
pub(crate) fn sanitize_session_list_payload(payload: &Value) -> Value {
    let Some(items) = payload.as_array() else {
        // JS: non-array payloads pass through verbatim.
        return payload.clone();
    };
    Value::Array(items.iter().map(sanitize_session_list_item).collect())
}

/// 单项脱敏：仅保留白名单字段；summary 去掉 diffs 后保留；revert 缩减为
/// 字符串 messageID / partID（一个都没有则整个字段不输出）；非对象项
/// 原样返回。
fn sanitize_session_list_item(session: &Value) -> Value {
    let Some(object) = session.as_object() else {
        // JS: non-object (or array/null) items pass through.
        return session.clone();
    };

    let mut sanitized = Map::new();
    for key in SESSION_LIST_ALLOWED_FIELDS {
        if let Some(value) = object.get(*key) {
            sanitized.insert((*key).to_string(), value.clone());
        }
    }

    // summary minus diffs, only for plain objects.
    if let Some(summary) = object.get("summary")
        && summary.is_object()
    {
        let mut summary_without_diffs = summary.as_object().cloned().unwrap_or_default();
        summary_without_diffs.remove("diffs");
        sanitized.insert("summary".to_string(), Value::Object(summary_without_diffs));
    }

    // revert reduced to string messageID/partID; only emitted when at least
    // one marker survives.
    if let Some(revert) = object.get("revert")
        && revert.is_object()
    {
        let mut revert_marker = Map::new();
        if revert.get("messageID").and_then(Value::as_str).is_some() {
            revert_marker.insert("messageID".to_string(), revert["messageID"].clone());
        }
        if revert.get("partID").and_then(Value::as_str).is_some() {
            revert_marker.insert("partID".to_string(), revert["partID"].clone());
        }
        if !revert_marker.is_empty() {
            sanitized.insert("revert".to_string(), Value::Object(revert_marker));
        }
    }

    Value::Object(sanitized)
}

/// 脱敏规则的行为测试。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 验证：非数组 payload 原样透传。
    #[test]
    fn passes_non_array_payloads_through_verbatim() {
        let payload = json!({"id": "ses_1"});
        assert_eq!(sanitize_session_list_payload(&payload), payload);
    }

    /// 验证：非对象项（null、数字、字符串、数组）原样透传。
    #[test]
    fn passes_non_object_items_through_verbatim() {
        let payload = json!([null, 7, "raw", ["array"]]);
        assert_eq!(sanitize_session_list_payload(&payload), payload);
    }

    /// 验证：白名单字段保留，其余字段（permission、未知字段）丢弃。
    #[test]
    fn keeps_allowed_fields_and_drops_the_rest() {
        let payload = json!([{
            "id": "ses_1",
            "title": "Alpha",
            "permission": [{"permission": "todowrite", "action": "deny"}],
            "unknownField": true,
        }]);
        assert_eq!(
            sanitize_session_list_payload(&payload),
            json!([{ "id": "ses_1", "title": "Alpha" }])
        );
    }

    /// 验证：白名单字段即使值为 null 也保留（按存在性而非真值判断）。
    #[test]
    fn keeps_allowed_fields_even_when_null() {
        // JS: `if (key in session)` — presence, not truthiness.
        let payload = json!([{ "id": null, "time": { "created": 1 } }]);
        assert_eq!(
            sanitize_session_list_payload(&payload),
            json!([{ "id": null, "time": { "created": 1 } }])
        );
    }

    /// 验证：summary 保留统计、剥掉 diffs。
    #[test]
    fn strips_summary_diffs_but_keeps_summary_stats() {
        let payload = json!([{
            "id": "ses_1",
            "summary": { "additions": 5, "deletions": 3, "files": 2, "diffs": [{"patch": "@@ -1 +1 @@"}] },
        }]);
        assert_eq!(
            sanitize_session_list_payload(&payload),
            json!([{ "id": "ses_1", "summary": { "additions": 5, "deletions": 3, "files": 2 } }])
        );
    }

    /// 验证：非对象 summary 整个不保留。
    #[test]
    fn drops_summary_when_it_is_not_an_object() {
        // JS: `summary && typeof summary === 'object'` — a textual (or null)
        // summary is simply not carried over.
        let payload = json!([{ "id": "ses_1", "summary": "textual" }]);
        assert_eq!(
            sanitize_session_list_payload(&payload),
            json!([{ "id": "ses_1" }])
        );
    }

    /// 验证：revert 缩减为字符串标记，无字符串标记时整个丢弃。
    #[test]
    fn reduces_revert_to_string_markers_and_drops_it_when_empty() {
        let with_markers = json!([{
            "id": "ses_1",
            "revert": { "messageID": "msg_1", "partID": "part_1", "snapshot": "abc", "diff": "…" },
        }]);
        assert_eq!(
            sanitize_session_list_payload(&with_markers),
            json!([{ "id": "ses_1", "revert": { "messageID": "msg_1", "partID": "part_1" } }])
        );

        let without_string_markers = json!([{
            "id": "ses_2",
            "revert": { "snapshot": "abc", "messageID": 42 },
        }]);
        assert_eq!(
            sanitize_session_list_payload(&without_string_markers),
            json!([{ "id": "ses_2" }])
        );
    }

    /// 验证：非对象 revert（含 null）整个不保留。
    #[test]
    fn drops_revert_when_it_is_not_an_object() {
        // JS: `revert && typeof revert === 'object'` — null is falsy, so the
        // field is simply not carried over.
        let payload = json!([{ "id": "ses_1", "revert": null }]);
        assert_eq!(
            sanitize_session_list_payload(&payload),
            json!([{ "id": "ses_1" }])
        );
    }
}
