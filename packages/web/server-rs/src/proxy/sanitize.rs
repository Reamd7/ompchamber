//! Port of the session-list sanitizer in `server/lib/opencode/proxy.js`
//! (`sanitizeSessionListItem` / `sanitizeSessionListPayload`): the session
//! LIST endpoints strip fields the UI must not rely on (`permission`,
//! oversized `revert` markers, and summary `diffs`) while session DETAIL
//! responses pass through untouched.

use serde_json::{Map, Value};

/// `SESSION_LIST_ALLOWED_FIELDS` in proxy.js.
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

pub(crate) fn sanitize_session_list_payload(payload: &Value) -> Value {
    let Some(items) = payload.as_array() else {
        // JS: non-array payloads pass through verbatim.
        return payload.clone();
    };
    Value::Array(items.iter().map(sanitize_session_list_item).collect())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn passes_non_array_payloads_through_verbatim() {
        let payload = json!({"id": "ses_1"});
        assert_eq!(sanitize_session_list_payload(&payload), payload);
    }

    #[test]
    fn passes_non_object_items_through_verbatim() {
        let payload = json!([null, 7, "raw", ["array"]]);
        assert_eq!(sanitize_session_list_payload(&payload), payload);
    }

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

    #[test]
    fn keeps_allowed_fields_even_when_null() {
        // JS: `if (key in session)` — presence, not truthiness.
        let payload = json!([{ "id": null, "time": { "created": 1 } }]);
        assert_eq!(
            sanitize_session_list_payload(&payload),
            json!([{ "id": null, "time": { "created": 1 } }])
        );
    }

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
