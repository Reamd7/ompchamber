//! Port of `server/lib/event-stream/protocol.js`.
//!
//! Pure helpers: SSE envelope parsing, client frame serialization, and
//! directory-scope visibility. Kept side-effect free so every rule is unit
//! testable without a server.
//!
//! 中文概要：SSE 协议的纯函数集合——块解析（parse_sse_event_envelope）、
//! 客户端帧构造（event/ready/resync/error/heartbeat）与目录可见性判定
//! （event_visible_in_scope）。全部无副作用，规则可脱离服务器直接单测。

use serde_json::{Map, Value};

/// Browser-facing global message-stream path (JS WS path; served as SSE).
/// 全局（跨目录）订阅入口。
pub const MESSAGE_STREAM_GLOBAL_WS_PATH: &str = "/api/global/event/ws";
/// Browser-facing directory message-stream path (JS WS path; served as SSE).
/// 单目录订阅入口。
pub const MESSAGE_STREAM_DIRECTORY_WS_PATH: &str = "/api/event/ws";
/// 流式连接的心跳间隔：空闲时周期发送合成心跳，防止中间层断开连接。
pub const MESSAGE_STREAM_WS_HEARTBEAT_INTERVAL_MS: u64 = 15 * 1000;

/// Client connection parameters parsed from the stream query string
/// (`lastEventId`, `epoch`, `directory`) — mirrors the WS upgrade query in
/// runtime.js.
/// 查询值缺失或空白均归一为 None。
#[derive(Debug, Clone, Default)]
pub struct EventStreamParams {
    /// 客户端游标（上次收到的 eventId），断线重连的续传起点。
    pub last_event_id: Option<String>,
    /// 客户端记住的上游启动身份（跨重启判定）。
    pub epoch: Option<String>,
    /// 订阅的目录；None 表示全局范围。
    pub directory: Option<String>,
}

/// 连接参数的构造。
impl EventStreamParams {
    /// 从路由层解析好的查询映射构造参数。
    pub fn from_query(query: &std::collections::HashMap<String, String>) -> Self {
        /// 取键值并 trim，空串归一为 None。
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
/// 目录作用域判定中将其视作“广播给所有人”。
pub const GLOBAL_DIRECTORY: &str = "global";

/// Non-empty-string check mirroring the JS `stringOrNull` helper.
/// 用于把 SSE 行值与载荷字段归一为 Option<String>。
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
/// 目录推导只对业务载荷有意义，控制帧恒为 None。
#[derive(Debug, Clone, PartialEq)]
pub struct SseEventEnvelope {
    /// `id:` 行值；游标推进与重放定位的依据。
    pub event_id: Option<String>,
    /// `event:` 行值；resync/boot 等控制帧识别。
    pub event_name: Option<String>,
    /// 从载荷推导的目录（包裹形态优先，其次三层回退）。
    pub directory: Option<String>,
    /// data 解析结果；None 表示控制帧或 JSON null。
    pub payload: Option<Value>,
    /// data 非空但 JSON 解析失败；读取器必须拒绝推进游标。
    pub malformed: bool,
}

/// Parse one SSE block. Returns `None` for comment-only blocks.
/// 解析规则与 JS parseSseEventEnvelope 逐条对齐（单空格剥离、多行 data 拼接等）。
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

    /// 字符串字段的非空检查（内部复用 string_or_null）。
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

/// 值存在且非空才写入（序列化时省略空串字段）。
fn set_if_present(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.and_then(string_or_null) {
        map.insert(key.to_string(), Value::String(value));
    }
}

/// 构造业务事件帧；eventId/directory 为空或缺失时整体省略对应键。
pub fn event_frame(payload: &Value, event_id: Option<&str>, directory: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("event".to_string()));
    map.insert("payload".to_string(), payload.clone());
    set_if_present(&mut map, "eventId", event_id);
    set_if_present(&mut map, "directory", directory);
    Value::Object(map)
}

/// Ready control frame; `epoch` only when the upstream advertises one.
/// scope 为 "global" 或 "directory"。
pub fn ready_frame(scope: &str, epoch: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("ready".to_string()));
    map.insert("scope".to_string(), Value::String(scope.to_string()));
    set_if_present(&mut map, "epoch", epoch);
    Value::Object(map)
}

/// Transport-level resync control frame (docs/plan.md §5.2): cursor + epoch.
/// 上游游标失效时发给客户端的重新对齐指令。
pub fn resync_frame(event_id: Option<&str>, epoch: Option<&str>) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("resync".to_string()));
    set_if_present(&mut map, "eventId", event_id);
    set_if_present(&mut map, "epoch", epoch);
    Value::Object(map)
}

/// Error control frame sent before a stream is torn down.
/// 流拆除前发给客户端的最后一条错误帧。
pub fn error_frame(message: &str) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String("error".to_string()));
    map.insert("message".to_string(), Value::String(message.to_string()));
    Value::Object(map)
}

/// Synthetic heartbeat payload (JS sends it as a normal event frame scoped
/// `directory: 'global'`).
/// 仅用于保活，客户端应忽略其内容。
pub fn heartbeat_payload(timestamp_ms: u64) -> Value {
    serde_json::json!({ "type": "ompchamber:heartbeat", "timestamp": timestamp_ms })
}

/// Directory scoping per the JS protocol: events carry a directory, and a
/// client scoped to a directory receives only that directory's events plus
/// global ones (no directory / empty / `'global'`). A global-scope client
/// (`client_directory: None`) receives everything.
/// 返回 true 表示该事件应投递给该客户端。
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

/// 协议纯函数测试：解析分支、帧序列化与目录作用域规则。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 验证包裹形态解析出 id/event/目录与内层 payload。
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

    /// 验证裸载荷从 properties.directory 推导目录。
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

    /// 验证目录藏在 properties.info.directory 时也能推导。
    #[test]
    fn derives_directory_from_nested_properties_info() {
        let envelope = parse_sse_event_envelope(
            "data: {\"type\":\"x\",\"properties\":{\"info\":{\"directory\":\"/deep/dir\"}}}\n",
        )
        .expect("envelope");
        assert_eq!(envelope.directory.as_deref(), Some("/deep/dir"));
    }

    /// 验证无 data 块解析为控制帧，注释/空块返回 None。
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

    /// 验证不可解析 data 标记 malformed；无 id/event 的畸形块按注释丢弃。
    #[test]
    fn marks_unparseable_data_as_malformed_instead_of_control() {
        let envelope = parse_sse_event_envelope("id: evt-9\ndata: {not json}\n").expect("envelope");
        assert!(envelope.malformed);
        assert!(envelope.payload.is_none());
        // Malformed data with no id/event lines at all is a comment for JS.
        assert!(parse_sse_event_envelope("data: {not json}\n").is_none());
    }

    /// 验证 data: null 与控制帧同义（JS 的 payload ?? null 语义）。
    #[test]
    fn parses_null_data_as_a_control_frame_like_js() {
        let envelope = parse_sse_event_envelope("id: evt-0\ndata: null\n").expect("envelope");
        assert!(envelope.payload.is_none());
        assert!(!envelope.malformed);
    }

    /// 验证多行 data 先拼接再解析。
    #[test]
    fn joins_multi_line_data_before_parsing() {
        let envelope = parse_sse_event_envelope("id: multi\ndata: {\"type\":\ndata: \"joined\"}\n")
            .expect("envelope");
        assert_eq!(envelope.payload.as_ref().unwrap()["type"], "joined");
    }

    /// 验证包裹分支的 payload 数组也被接受。
    #[test]
    fn accepts_array_payloads_in_the_wrapped_branch() {
        let envelope = parse_sse_event_envelope("data: {\"directory\":\"/d\",\"payload\":[1,2]}\n")
            .expect("envelope");
        assert_eq!(envelope.payload, Some(json!([1, 2])));
        assert_eq!(envelope.directory.as_deref(), Some("/d"));
    }

    /// 验证事件帧携带路由元数据且空字符串字段被省略。
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

    /// 验证 ready/resync/error 控制帧的可选字段序列化。
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

    /// 验证目录客户端只收本目录与全局事件，全局客户端全收。
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
