//! Port of `server/lib/event-stream/protocol.js`.
//!
//! Pure helpers: SSE envelope parsing, client frame serialization, and
//! directory-scope visibility. Kept side-effect free so every rule is unit
//! testable without a server.

use serde_json::{Map, Value};

/// Browser-facing global message-stream path (JS WS path; served as SSE).
pub const MESSAGE_STREAM_GLOBAL_WS_PATH: &str = "/api/global/event/ws";
/// Browser-facing directory message-stream path (JS WS path; served as SSE).
pub const MESSAGE_STREAM_DIRECTORY_WS_PATH: &str = "/api/event/ws";
pub const MESSAGE_STREAM_WS_HEARTBEAT_INTERVAL_MS: u64 = 15 * 1000;

/// Client connection parameters parsed from the stream query string
/// (`lastEventId`, `epoch`, `directory`) — mirrors the WS upgrade query in
/// runtime.js.
#[derive(Debug, Clone, Default)]
pub struct EventStreamParams {
    pub last_event_id: Option<String>,
    pub epoch: Option<String>,
    pub directory: Option<String>,
}

impl EventStreamParams {
    pub fn from_query(query: &std::collections::HashMap<String, String>) -> Self {
        fn get(query: &std::collections::HashMap<String, String>, key: &str) -> Option<String> {
            query
                .get(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        }
        Self {
            last_event_id: get(query, "lastEventId"),
            epoch: get(query, "epoch"),
            directory: get(query, "directory"),
        }
    }
}

/// Marker directory JS assigns to events that carry no directory.
pub const GLOBAL_DIRECTORY: &str = "global";

/// Non-empty-string check mirroring the JS `stringOrNull` helper.
pub(crate) fn string_or_null(value: &str) -> Option<String> {
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Parsed SSE block: `parseSseEventEnvelope(block)` in protocol.js.
///
/// - `payload: None` marks a data-less control frame (`event:`/`id:` only) —
///   those must never be dropped because resync/restart controls ride them.
/// - `malformed: true` marks a block whose `data:` failed JSON parsing: the
///   reader must refuse to advance its cursor (a lost business event).
/// - Parsing to JSON `null` is indistinguishable from a control frame, exactly
///   as in JS (`payload ?? null`).
#[derive(Debug, Clone, PartialEq)]
pub struct SseEventEnvelope {
    pub event_id: Option<String>,
    pub event_name: Option<String>,
    pub directory: Option<String>,
    pub payload: Option<Value>,
    pub malformed: bool,
}

/// Parse one SSE block. Returns `None` for comment-only blocks.
pub fn parse_sse_event_envelope(block: &str) -> Option<SseEventEnvelope> {
    let mut event_id: Option<String> = None;
    let mut event_name: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();

    for line in block.split('\n') {
        if let Some(rest) = line.strip_prefix("id:") {
            event_id = string_or_null(rest.trim());
        } else if let Some(rest) = line.strip_prefix("event:") {
            event_name = string_or_null(rest.trim());
        } else if let Some(rest) = line.strip_prefix("data:") {
            // JS strips exactly ONE leading whitespace char after `data:`.
            let stripped = rest.strip_prefix(char::is_whitespace).unwrap_or(rest);
            data_lines.push(stripped.to_string());
        }
    }

    let payload_text = data_lines.join("\n");
    let payload_text = payload_text.trim();

    let parsed: Option<Value> = if payload_text.is_empty() {
        None
    } else {
        serde_json::from_str(payload_text).ok()
    };

    let Some(parsed) = parsed else {
        if event_id.is_none() && event_name.is_none() {
            // Comment-only block (or malformed data with no id/event lines).
            return None;
        }
        return Some(SseEventEnvelope {
            event_id,
            event_name,
            directory: None,
            payload: None,
            // Non-empty but unparseable data marks a malformed business
            // block, not a control frame; absent data stays a clean control.
            malformed: !payload_text.is_empty(),
        });
    };

    // `data: null` parses to JSON null — same wire meaning as a control
    // frame in the JS protocol (`payload ?? null`).
    if parsed.is_null() {
        return Some(SseEventEnvelope {
            event_id,
            event_name,
            directory: None,
            payload: None,
            malformed: false,
        });
    }

    fn non_empty_str(value: &Value) -> Option<String> {
        value.as_str().and_then(string_or_null)
    }

    // Wrapped branch: `{ directory, payload: {…} }` (typeof payload === 'object'
    // covers arrays too in JS).
    if parsed.is_object()
        && let Some(payload) = parsed.get("payload")
        && (payload.is_object() || payload.is_array())
    {
        return Some(SseEventEnvelope {
            event_id,
            event_name,
            directory: parsed.get("directory").and_then(non_empty_str),
            payload: Some(payload.clone()),
            malformed: false,
        });
    }

    // Unwrapped payload: directory may ride at three depths.
    let directory = parsed
        .get("directory")
        .and_then(non_empty_str)
        .or_else(|| {
            parsed
                .get("properties")
                .and_then(|props| props.get("directory"))
                .and_then(non_empty_str)
        })
        .or_else(|| {
            parsed
                .get("properties")
                .and_then(|props| props.get("info"))
                .and_then(|info| info.get("directory"))
                .and_then(non_empty_str)
        });

    Some(SseEventEnvelope {
        event_id,
        event_name,
        directory,
        payload: Some(parsed),
        malformed: false,
    })
}

fn set_if_present(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.and_then(string_or_null) {
        map.insert(key.to_string(), Value::String(value));
    }
}

pub fn event_frame(payload: &Value, event_id: Option<&str>, directory: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("event".to_string()));
    map.insert("payload".to_string(), payload.clone());
    set_if_present(&mut map, "eventId", event_id);
    set_if_present(&mut map, "directory", directory);
    Value::Object(map)
}

/// Ready control frame; `epoch` only when the upstream advertises one.
pub fn ready_frame(scope: &str, epoch: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("ready".to_string()));
    map.insert("scope".to_string(), Value::String(scope.to_string()));
    set_if_present(&mut map, "epoch", epoch);
    Value::Object(map)
}

/// Transport-level resync control frame (docs/plan.md §5.2): cursor + epoch.
pub fn resync_frame(event_id: Option<&str>, epoch: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("resync".to_string()));
    set_if_present(&mut map, "eventId", event_id);
    set_if_present(&mut map, "epoch", epoch);
    Value::Object(map)
}

/// Error control frame sent before a stream is torn down.
pub fn error_frame(message: &str) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("error".to_string()));
    map.insert("message".to_string(), Value::String(message.to_string()));
    Value::Object(map)
}

/// Synthetic heartbeat payload (JS sends it as a normal event frame scoped
/// `directory: 'global'`).
pub fn heartbeat_payload(timestamp_ms: u64) -> Value {
    serde_json::json!({ "type": "ompchamber:heartbeat", "timestamp": timestamp_ms })
}

/// Directory scoping per the JS protocol: events carry a directory, and a
/// client scoped to a directory receives only that directory's events plus
/// global ones (no directory / empty / `'global'`). A global-scope client
/// (`client_directory: None`) receives everything.
pub fn event_visible_in_scope(
    event_directory: Option<&str>,
    client_directory: Option<&str>,
) -> bool {
    let Some(client) = client_directory.filter(|c| !c.is_empty()) else {
        return true;
    };
    match event_directory {
        None => true,
        Some(dir) => dir.is_empty() || dir == GLOBAL_DIRECTORY || dir == client,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_wrapped_sse_payload_with_event_id_and_directory() {
        let envelope = parse_sse_event_envelope(
            "id: evt-1\nevent: message\ndata: {\"directory\":\"/tmp/project\",\"payload\":{\"type\":\"session.updated\"}}\n",
        )
        .expect("envelope");
        assert_eq!(
            envelope,
            SseEventEnvelope {
                event_id: Some("evt-1".to_string()),
                event_name: Some("message".to_string()),
                directory: Some("/tmp/project".to_string()),
                payload: Some(json!({ "type": "session.updated" })),
                malformed: false,
            }
        );
    }

    #[test]
    fn derives_directory_from_payload_properties_when_not_wrapped() {
        let envelope = parse_sse_event_envelope(
            "id: evt-2\ndata: {\"type\":\"server.connected\",\"properties\":{\"directory\":\"/tmp/project\"}}\n",
        )
        .expect("envelope");
        assert_eq!(envelope.directory.as_deref(), Some("/tmp/project"));
        assert_eq!(
            envelope.payload.as_ref().unwrap()["type"],
            "server.connected"
        );
    }

    #[test]
    fn derives_directory_from_nested_properties_info() {
        let envelope = parse_sse_event_envelope(
            "data: {\"type\":\"x\",\"properties\":{\"info\":{\"directory\":\"/deep/dir\"}}}\n",
        )
        .expect("envelope");
        assert_eq!(envelope.directory.as_deref(), Some("/deep/dir"));
    }

    #[test]
    fn returns_control_frames_for_data_less_blocks_and_null_for_comments() {
        let control =
            parse_sse_event_envelope("event: omp.stream.resync\nid: r9\n").expect("control");
        assert_eq!(control.event_name.as_deref(), Some("omp.stream.resync"));
        assert_eq!(control.event_id.as_deref(), Some("r9"));
        assert!(control.payload.is_none());
        assert!(!control.malformed);

        assert!(parse_sse_event_envelope(": keep-alive comment\n").is_none());
        assert!(parse_sse_event_envelope("").is_none());
    }

    #[test]
    fn marks_unparseable_data_as_malformed_instead_of_control() {
        let envelope = parse_sse_event_envelope("id: evt-9\ndata: {not json}\n").expect("envelope");
        assert!(envelope.malformed);
        assert!(envelope.payload.is_none());
        // Malformed data with no id/event lines at all is a comment for JS.
        assert!(parse_sse_event_envelope("data: {not json}\n").is_none());
    }

    #[test]
    fn parses_null_data_as_a_control_frame_like_js() {
        let envelope = parse_sse_event_envelope("id: evt-0\ndata: null\n").expect("envelope");
        assert!(envelope.payload.is_none());
        assert!(!envelope.malformed);
    }

    #[test]
    fn joins_multi_line_data_before_parsing() {
        let envelope = parse_sse_event_envelope("id: multi\ndata: {\"type\":\ndata: \"joined\"}\n")
            .expect("envelope");
        assert_eq!(envelope.payload.as_ref().unwrap()["type"], "joined");
    }

    #[test]
    fn accepts_array_payloads_in_the_wrapped_branch() {
        let envelope = parse_sse_event_envelope("data: {\"directory\":\"/d\",\"payload\":[1,2]}\n")
            .expect("envelope");
        assert_eq!(envelope.payload, Some(json!([1, 2])));
        assert_eq!(envelope.directory.as_deref(), Some("/d"));
    }

    #[test]
    fn serializes_event_frames_with_routing_metadata() {
        let frame = event_frame(&json!({ "type": "x" }), Some("evt-1"), Some("/tmp/p"));
        assert_eq!(
            frame,
            json!({ "type": "event", "payload": { "type": "x" }, "eventId": "evt-1", "directory": "/tmp/p" })
        );
        // Empty metadata is omitted, never empty strings.
        let frame = event_frame(&json!({ "type": "x" }), Some(""), None);
        assert_eq!(
            frame,
            json!({ "type": "event", "payload": { "type": "x" } })
        );
    }

    #[test]
    fn serializes_control_frames_with_optional_fields() {
        assert_eq!(
            ready_frame("global", Some("boot-7")),
            json!({ "type": "ready", "scope": "global", "epoch": "boot-7" })
        );
        assert_eq!(
            ready_frame("directory", None),
            json!({ "type": "ready", "scope": "directory" })
        );
        assert_eq!(
            resync_frame(Some("tail-3"), Some("boot-7")),
            json!({ "type": "resync", "eventId": "tail-3", "epoch": "boot-7" })
        );
        assert_eq!(resync_frame(None, None), json!({ "type": "resync" }));
        assert_eq!(
            error_frame("boom"),
            json!({ "type": "error", "message": "boom" })
        );
    }

    #[test]
    fn directory_scoping_delivers_own_and_global_events_only() {
        let scoped = Some("/tmp/project");
        assert!(event_visible_in_scope(Some("/tmp/project"), scoped));
        assert!(event_visible_in_scope(Some("global"), scoped));
        assert!(event_visible_in_scope(None, scoped));
        assert!(!event_visible_in_scope(Some("/tmp/other"), scoped));
        // Global-scope clients see everything.
        assert!(event_visible_in_scope(Some("/tmp/other"), None));
        assert!(event_visible_in_scope(None, None));
    }
}
