//! Port of `server/lib/notifications/emitter-runtime.js`: unified
//! notification emission channels — the `/api/notifications/stream` SSE
//! client set, the server-wide hub broadcast (the JS WS-client channel),
//! and desktop notification delivery (injected native callback or the
//! one-line stdout protocol).
//!
//! 中文概述：JS emitter-runtime 的移植。一条通知按固定优先级分发：
//! 先尝试原生桌面回调（shell 注入的槽位），未注入时退回 stdout 单行
//! 协议；同时把合成事件 `ompchamber:notification` 广播到全局 hub
//! （WS 客户端）与 SSE 订阅者 —— 三条通道互不阻塞、失败互不影响。

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::hub::EventHub;

/// JS `DESKTOP_NOTIFY_PREFIX` (index.js).
/// 中文：旧版 shell 按「前缀 + JSON + 换行」识别 stdout 上的通知行；
/// 新 shell 通过注入回调取而代之，该前缀仅作兼容兜底。
pub const DESKTOP_NOTIFY_PREFIX: &str = "[OMPChamberDesktopNotify] ";

/// 原生桌面通知回调类型：入参为完整 payload JSON；由桌面 shell 在
/// 启动时注入，回调内抛出的 panic 会被发射器捕获。
pub type DesktopNotificationCallback = Arc<dyn Fn(&Value) + Send + Sync>;

/// JS `ENV_DESKTOP_NOTIFY` (index.js): env flag, desktop runtime marker,
/// or an `ompchamber-server` entry in argv[0]/argv[1].
/// 中文：三个判定任一命中即启用桌面通知：显式 env 开关、runtime
/// 标记为 desktop、argv[0]/argv[1] 含 ompchamber-server（打包产物路径）。
pub fn env_desktop_notify() -> bool {
    if std::env::var("OMPCHAMBER_DESKTOP_NOTIFY").as_deref() == Ok("true") {
        return true;
    }
    if std::env::var("OMPCHAMBER_RUNTIME").as_deref() == Ok("desktop") {
        return true;
    }
    let mut args = std::env::args();
    let argv0 = args.next().unwrap_or_default();
    let argv1 = args.next().unwrap_or_default();
    let matches = |value: &str| value.to_ascii_lowercase().contains("ompchamber-server");
    matches(&argv0) || matches(&argv1)
}

/// 通知发射器：聚合原生桌面回调、stdout 协议、hub 广播与 SSE 四路
/// 出口（JS EmitterRuntime 的对等物）；构造后各字段不可变，回调槽位除外。
pub struct EmitterRuntime {
/// 桌面通知总开关（惰性闭包，运行期可随环境变化）。
    desktop_notify_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
/// stdout 协议行前缀（见 [`DESKTOP_NOTIFY_PREFIX`]）。
    prefix: String,
    /// Serialized synthetic payloads for the connected
    /// `/api/notifications/stream` clients (the JS `uiNotificationClients`).
/// 中文：通道里放的是「已序列化的合成事件字符串」，SSE 处理器原样
/// 写出；无订阅者时发送被静默丢弃。
    notification_tx: broadcast::Sender<String>,
    /// The JS `broadcastGlobalUiEvent` WS-client channel: publishing to the
    /// server hub reaches every message-stream client.
/// 中文：事件名固定为 `ompchamber:notification`，所有消息流（WS）
/// 客户端都会收到同一份 JSON。
    hub: Arc<EventHub>,
/// 当前注入的原生回调；None 表示未注入，发射时走 stdout 兜底路径。
    on_desktop_notification: Mutex<Option<DesktopNotificationCallback>>,
}

/// 构造入口与三条通道的写入方法。
impl EmitterRuntime {
/// 组装运行时并装进 Arc；开关闭包与前缀由组合根（notifications/mod.rs）
/// 决定，hub/SSE 通道来自 RouterContext 与本模块调用方。
    pub fn new(
        desktop_notify_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
        prefix: impl Into<String>,
        notification_tx: broadcast::Sender<String>,
        hub: Arc<EventHub>,
    ) -> Arc<Self> {
        Arc::new(Self {
            desktop_notify_enabled,
            prefix: prefix.into(),
            notification_tx,
            hub,
            on_desktop_notification: Mutex::new(None),
        })
    }

    /// `setOnDesktopNotification`: late-bindable in-process shell hook.
/// 中文：传 None 即卸载回调，后续发射回落到 stdout 协议。
    pub fn set_on_desktop_notification(&self, callback: Option<DesktopNotificationCallback>) {
        *self
            .on_desktop_notification
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = callback;
    }

    /// `writeSseEvent`: one SSE `data:` frame.
/// 中文：标准 SSE 帧 `data: <json>\n\n`；序列化失败产出空 JSON 帧
/// （与 JS `JSON.stringify` 失败路径的兜底一致）。
    pub fn write_sse_event(&self, payload: &Value) -> String {
        format!(
            "data: {}\n\n",
            serde_json::to_string(payload).unwrap_or_default()
        )
    }

    /// `emitDesktopNotification`: returns whether a native channel accepted
    /// the payload. An injected callback wins; otherwise the one-line
    /// stdout protocol (`${prefix}${json}\n`) serves legacy shells.
/// 中文：总开关关闭或 payload 非对象直接返回 false；回调路径用
/// catch_unwind 捕获宿主 panic（对齐 JS 的 try/catch），stdout 路径
/// 以整行写入成功与否为返回值。
    pub fn emit_desktop_notification(&self, payload: &Value) -> bool {
        if !(self.desktop_notify_enabled)() {
            return false;
        }
        if !payload.is_object() {
            return false;
        }
        let callback = self
            .on_desktop_notification
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(callback) = callback {
            // JS wraps the host callback in try/catch — mirror by catching
            // a panicking host hook instead of poisoning the trigger path.
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| callback(payload)));
            return result.is_ok();
        }
        let line = format!(
            "{}{}\n",
            self.prefix,
            serde_json::to_string(payload).unwrap_or_default()
        );
        use std::io::Write;
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        handle.write_all(line.as_bytes()).is_ok()
    }

    /// `broadcastUiNotification`: fan the notification out to the UI
    /// surfaces, marking whether a native channel already delivered it.
/// 中文：在 payload 自身字段之上叠加 `desktopNotificationDelivered` /
/// `desktopStdoutActive` 两个标记后同时发往 hub 与 SSE；任一通道
/// 无接收者的发送失败都被静默忽略。
    pub fn broadcast_ui_notification(&self, payload: &Value, desktop_notification_delivered: bool) {
        if !payload.is_object() {
            return;
        }
        let desktop_notify_enabled = (self.desktop_notify_enabled)();
        let mut properties = payload_object_cloned(payload);
        if let Value::Object(map) = &mut properties {
            map.insert(
                "desktopNotificationDelivered".to_string(),
                Value::Bool(desktop_notification_delivered),
            );
            map.insert(
                "desktopStdoutActive".to_string(),
                Value::Bool(desktop_notify_enabled),
            );
        }
        let synthetic = json!({
            "type": "ompchamber:notification",
            "properties": properties,
        });
        // JS broadcastGlobalUiEvent: message-stream (WS) clients via the
        // global broadcaster, notification-stream (SSE) clients via
        // writeSseEvent — both channels receive the same payload.
        self.hub.publish_json("ompchamber:notification", &synthetic);
        let _ = self
            .notification_tx
            .send(serde_json::to_string(&synthetic).unwrap_or_default());
    }
}

/// Spread the payload's own properties into the synthetic envelope
/// (JS `{ ...payload, desktopNotificationDelivered, … }`).
/// 中文：等价 JS 展开运算符 —— 只克隆对象自身键值，非对象输入得到
/// 空对象，保证下游 insert 不丢键。
fn payload_object_cloned(payload: &Value) -> Value {
    match payload {
        Value::Object(map) => Value::Object(map.clone()),
        _ => Value::Object(serde_json::Map::new()),
    }
}

/// 发射器单元测试：回调/stdout 分发优先级、开关门控与两路广播帧形状。
#[cfg(test)]
mod tests {
    use super::*;

/// 测试辅助：按给定开关与可选回调构建一个隔离的运行时实例。
    fn runtime(
        enabled: bool,
        on_notification: Option<DesktopNotificationCallback>,
    ) -> Arc<EmitterRuntime> {
        let runtime = EmitterRuntime::new(
            Arc::new(move || enabled),
            "[desktop-notify]",
            broadcast::channel(16).0,
            EventHub::new(),
        );
        runtime.set_on_desktop_notification(on_notification);
        runtime
    }

/// 验证注入的回调收到完整 payload 且返回 true（原生通道接单）。
    #[test]
    fn reports_desktop_delivery_through_the_injected_callback() {
        let delivered: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&delivered);
        let runtime = runtime(
            true,
            Some(Arc::new(move |payload| {
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(payload.clone());
            })),
        );
        let payload = json!({ "title": "Ready", "body": "Done" });
        assert!(runtime.emit_desktop_notification(&payload));
        let delivered = delivered.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(*delivered, vec![payload]);
    }

/// 验证开关关闭时即使有回调也不发桌面通知（返回 false）。
    #[test]
    fn respects_the_desktop_notify_toggle() {
        let runtime = runtime(false, Some(Arc::new(|_| {})));
        assert!(!runtime.emit_desktop_notification(&json!({ "title": "Ready" })));
    }

/// 验证无回调时 stdout 兜底路径返回 true，非对象 payload 被拒绝。
    #[test]
    fn reports_stdout_delivery_for_legacy_shells() {
        // Enabled, no injected callback → the stdout path answers true.
        let runtime = runtime(true, None);
        assert!(runtime.emit_desktop_notification(&json!({ "title": "Ready" })));
        // Non-object payloads are rejected.
        assert!(!runtime.emit_desktop_notification(&json!("plain")));
    }

/// 验证 UI 广播帧在 SSE 通道与 hub 通道各收到一份，且带上
/// 「已原生送达」「stdout 活跃」两个标记。
    #[tokio::test]
    async fn marks_ui_broadcasts_that_were_already_delivered_natively() {
        let (tx, mut rx) = broadcast::channel(16);
        let hub = EventHub::new();
        let mut hub_rx = hub.subscribe();
        let runtime = EmitterRuntime::new(Arc::new(|| true), "[desktop-notify]", tx, hub);

        runtime.broadcast_ui_notification(&json!({ "title": "Ready" }), true);

        let frame = rx.recv().await.expect("sse frame");
        let parsed: Value = serde_json::from_str(&frame).expect("json frame");
        assert_eq!(parsed["type"], "ompchamber:notification");
        assert_eq!(parsed["properties"]["title"], "Ready");
        assert_eq!(parsed["properties"]["desktopNotificationDelivered"], true);
        assert_eq!(parsed["properties"]["desktopStdoutActive"], true);

        let hub_event = hub_rx.recv().await.expect("hub frame");
        assert_eq!(hub_event.event, "ompchamber:notification");
    }

/// 验证 SSE 帧格式恰为 `data: <json>\n\n`。
    #[test]
    fn sse_frame_wraps_payload_as_data_line() {
        let runtime = runtime(true, None);
        assert_eq!(
            runtime.write_sse_event(&json!({ "type": "x" })),
            "data: {\"type\":\"x\"}\n\n"
        );
    }
}
