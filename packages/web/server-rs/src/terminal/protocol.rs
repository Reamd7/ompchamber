//! Port of `server/lib/terminal/terminal-ws-protocol.js`.
//!
//! The terminal data transport is one WebSocket (`/api/terminal/ws`) carrying
//! **v3 binary JSON control frames**: a single `0x01` tag byte followed by a
//! UTF-8 JSON document. This is a shared contract with the UI
//! (`packages/ui/src/lib/terminalApi.ts` uses the same `TAG = 1`).
//!
//! JS-only surface intentionally not ported: `normalizeTerminalWsMessageToBuffer`
//! / `...ToText` (the `ws` library delivers Buffer/ArrayBuffer/Buffer[] shapes;
//! axum hands us one binary `Bytes` per message, so normalization is the
//! identity) and `parseRequestPathname` (Node's raw `upgrade` event needs manual
//! URL parsing; axum routes by path directly).
//!
//! 中文说明：终端数据传输走单条 WebSocket（`/api/terminal/ws`），控制帧
//! 为 v3 二进制 JSON——一个 `0x01` tag 字节后跟 UTF-8 JSON 文档，UI 侧
//! `terminalApi.ts` 使用同一 `TAG = 1` 契约。本模块还携带 rebind 限流
//! 的窗口/阈值常量与判定函数，供运行时在断线重绑时复用 JS host 的
//! 限流语义。

/// The only terminal data-transport path (must stay in `ui_auth`'s
/// `isUrlAuthWebSocketPath` and relay `ALLOWED_WS_PATHS`).
pub const TERMINAL_WS_PATH: &str = "/api/terminal/ws";

/// Binary control-frame tag identifying a JSON payload.
pub const TERMINAL_WS_CONTROL_TAG_JSON: u8 = 0x01;

/// Server-side receive cap (`ws` `maxPayload` option).
pub const TERMINAL_WS_MAX_PAYLOAD_BYTES: usize = 64 * 1024;

// Wiring constants from `server/index.js` (handed to `createTerminalRuntime`
// via `startup-pipeline-runtime.js`).
/// 输入 WebSocket 心跳间隔（`server/index.js` 接线值，15 秒）。
pub const TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS: u64 = 15_000;
/// rebind 限流滑动窗口长度（60 秒）。
pub const TERMINAL_INPUT_WS_REBIND_WINDOW_MS: u64 = 60_000;
/// 单个窗口内允许的最大 rebind 次数（达到即限流）。
pub const TERMINAL_INPUT_WS_MAX_REBINDS_PER_WINDOW: usize = 128;

/// `isTerminalWsPathname`.
pub fn is_terminal_ws_pathname(pathname: &str) -> bool {
    pathname == TERMINAL_WS_PATH
}

/// `readTerminalWsControlFrame`: `None` for empty input, missing tag, frames
/// shorter than tag+1 byte, malformed JSON, or a non-object document.
///
/// JS checks `typeof parsed !== 'object'` — in JS both objects **and arrays**
/// are `'object'`, so arrays pass this layer and are rejected later by the
/// `t`/`v` field checks; mirrored here.
pub fn read_terminal_ws_control_frame(raw: &[u8]) -> Option<serde_json::Value> {
    if raw.len() < 2 || raw[0] != TERMINAL_WS_CONTROL_TAG_JSON {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_slice(&raw[1..]).ok()?;
    match &parsed {
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => Some(parsed),
        _ => None,
    }
}

/// `createTerminalWsControlFrame`: tag byte + compact JSON.
pub fn create_terminal_ws_control_frame(payload: &serde_json::Value) -> Vec<u8> {
    let json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    let mut frame = Vec::with_capacity(json.len() + 1);
    frame.push(TERMINAL_WS_CONTROL_TAG_JSON);
    frame.extend_from_slice(json.as_bytes());
    frame
}

/// `pruneRebindTimestamps`: drop entries older than the window. Future
/// timestamps survive (negative age is `< windowMs`, matching the JS filter).
pub fn prune_rebind_timestamps(timestamps: &[u64], now: u64, window_ms: u64) -> Vec<u64> {
    timestamps
        .iter()
        .copied()
        .filter(|timestamp| (now as i64 - *timestamp as i64) < window_ms as i64)
        .collect()
}

/// `isRebindRateLimited`: limited once the window holds `maxPerWindow` events.
pub fn is_rebind_rate_limited(timestamps: &[u64], max_per_window: usize) -> bool {
    timestamps.len() >= max_per_window
}

/// WS 控制帧编解码与 rebind 限流的单元测试。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 验证：路径判定只接受固定的 `/api/terminal/ws`，前后缀差异都拒绝。
    #[test]
    fn uses_fixed_websocket_path() {
        assert_eq!(TERMINAL_WS_PATH, "/api/terminal/ws");
        assert!(is_terminal_ws_pathname("/api/terminal/ws"));
        assert!(!is_terminal_ws_pathname("/api/terminal/ws/"));
        assert!(!is_terminal_ws_pathname("/api/terminal"));
        assert!(!is_terminal_ws_pathname(""));
    }

    /// 验证：控制帧编码为 tag 字节 + 紧凑 JSON，键序与 JS stringify 一致。
    #[test]
    fn encodes_control_frames_with_control_tag_prefix() {
        let frame = create_terminal_ws_control_frame(&json!({ "t": "write", "d": "hi" }));
        assert_eq!(frame[0], TERMINAL_WS_CONTROL_TAG_JSON);
        // preserve_order keeps insertion order (matches the JS JSON.stringify).
        assert_eq!(&frame[1..], br#"{"t":"write","d":"hi"}"#.as_slice());
    }

    /// 验证：编码再解码可无损还原负载对象。
    #[test]
    fn roundtrips_control_frame_payload() {
        let payload = json!({ "t": "attach", "v": 3, "s": "term-1", "feed": "grid", "cols": 120 });
        let frame = create_terminal_ws_control_frame(&payload);
        assert_eq!(read_terminal_ws_control_frame(&frame), Some(payload));
    }

    /// 验证：缺少协议 tag 字节的裸 JSON 被拒绝。
    #[test]
    fn rejects_control_frame_without_protocol_tag() {
        assert_eq!(read_terminal_ws_control_frame(br#"{"t":"ping"}"#), None);
    }

    /// 验证：tag 后跟非法 JSON 时返回 `None`。
    #[test]
    fn rejects_malformed_control_json() {
        let frame = [TERMINAL_WS_CONTROL_TAG_JSON, b'{', b'"', b'a'];
        assert_eq!(read_terminal_ws_control_frame(&frame), None);
    }

    /// 验证：空输入与仅有 tag 字节的帧都被拒绝（最短帧为 tag+1 字节）。
    #[test]
    fn rejects_empty_control_payloads() {
        assert_eq!(read_terminal_ws_control_frame(&[]), None);
        assert_eq!(
            read_terminal_ws_control_frame(&[TERMINAL_WS_CONTROL_TAG_JSON]),
            None
        );
    }

    /// 验证：标量 JSON（数字/字符串/布尔/null）在此层被拒绝。
    #[test]
    fn rejects_control_json_that_is_not_object() {
        for raw in [br#"123"#.as_slice(), br#""text""#, br#"true"#, br#"null"#] {
            let mut frame = vec![TERMINAL_WS_CONTROL_TAG_JSON];
            frame.extend_from_slice(raw);
            assert_eq!(read_terminal_ws_control_frame(&frame), None, "{raw:?}");
        }
    }

    /// 验证：数组与 JS 的 `typeof === 'object'` 一样通过本层，
    /// 由后续 `t`/`v` 字段校验兜底。
    #[test]
    fn arrays_pass_frame_layer_like_js_typeof_object() {
        let frame = create_terminal_ws_control_frame(&json!([1, 2]));
        assert!(read_terminal_ws_control_frame(&frame).is_some());
    }

    /// 验证：窗口外的历史时间戳被剪除，窗口内的保留。
    #[test]
    fn prunes_stale_rebind_timestamps() {
        let now = 1_000;
        let pruned = prune_rebind_timestamps(&[100, 200, 950, 999], now, 100);
        assert_eq!(pruned, vec![950, 999]);
    }

    /// 验证：窗口内的近期时间戳与未来时间戳（负年龄）都保留。
    #[test]
    fn keeps_future_rebind_timestamps_within_active_window() {
        let now = 1_000;
        // 920 is 80ms old — inside a 100ms window — and future stamps survive.
        let pruned = prune_rebind_timestamps(&[920, 950, 999, 1_050], now, 100);
        assert_eq!(pruned, vec![920, 950, 999, 1_050]);
    }

    /// 验证：rebind 常量与 `server/index.js` 的接线值逐项一致。
    #[test]
    fn rebind_window_matches_index_js_wiring() {
        assert_eq!(TERMINAL_INPUT_WS_MAX_REBINDS_PER_WINDOW, 128);
        assert_eq!(TERMINAL_INPUT_WS_REBIND_WINDOW_MS, 60_000);
        assert_eq!(TERMINAL_INPUT_WS_HEARTBEAT_INTERVAL_MS, 15_000);
    }

    /// 验证：窗口内事件数低于阈值时不限流。
    #[test]
    fn does_not_rate_limit_below_threshold() {
        assert!(!is_rebind_rate_limited(&[1, 2, 3], 4));
        assert!(!is_rebind_rate_limited(&[], 1));
    }

    /// 验证：达到阈值即限流，剪除过期时间戳后限流解除。
    #[test]
    fn rate_limits_at_threshold() {
        assert!(is_rebind_rate_limited(&[1, 2, 3, 4], 4));
        // Pruning then re-checking lifts the limit, like the runtime would:
        // at t=60_003 only the 4ms-old stamp is inside the 60s window.
        let live = prune_rebind_timestamps(&[1, 2, 3, 4], 60_003, 60_000);
        assert_eq!(live, vec![4]);
        assert!(!is_rebind_rate_limited(&live, 4));
    }
}
