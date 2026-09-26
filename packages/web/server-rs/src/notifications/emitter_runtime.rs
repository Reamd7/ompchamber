//! Port of `server/lib/notifications/emitter-runtime.js`: unified
//! notification emission channels — the `/api/notifications/stream` SSE
//! client set, the server-wide hub broadcast (the JS WS-client channel),
//! and desktop notification delivery (injected native callback or the
//! one-line stdout protocol).

use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::hub::EventHub;

/// JS `DESKTOP_NOTIFY_PREFIX` (index.js).
pub const DESKTOP_NOTIFY_PREFIX: &str = "[OMPChamberDesktopNotify] ";

pub type DesktopNotificationCallback = Arc<dyn Fn(&Value) + Send + Sync>;

/// JS `ENV_DESKTOP_NOTIFY` (index.js): env flag, desktop runtime marker,
/// or an `ompchamber-server` entry in argv[0]/argv[1].
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

pub struct EmitterRuntime {
    desktop_notify_enabled: Arc<dyn Fn() -> bool + Send + Sync>,
    prefix: String,
    /// Serialized synthetic payloads for the connected
    /// `/api/notifications/stream` clients (the JS `uiNotificationClients`).
    notification_tx: broadcast::Sender<String>,
    /// The JS `broadcastGlobalUiEvent` WS-client channel: publishing to the
    /// server hub reaches every message-stream client.
    hub: Arc<EventHub>,
    on_desktop_notification: Mutex<Option<DesktopNotificationCallback>>,
}

impl EmitterRuntime {
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
    pub fn set_on_desktop_notification(&self, callback: Option<DesktopNotificationCallback>) {
        *self
            .on_desktop_notification
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = callback;
    }

    /// `writeSseEvent`: one SSE `data:` frame.
    pub fn write_sse_event(&self, payload: &Value) -> String {
        format!(
            "data: {}\n\n",
            serde_json::to_string(payload).unwrap_or_default()
        )
    }

    /// `emitDesktopNotification`: returns whether a native channel accepted
    /// the payload. An injected callback wins; otherwise the one-line
    /// stdout protocol (`${prefix}${json}\n`) serves legacy shells.
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
fn payload_object_cloned(payload: &Value) -> Value {
    match payload {
        Value::Object(map) => Value::Object(map.clone()),
        _ => Value::Object(serde_json::Map::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn respects_the_desktop_notify_toggle() {
        let runtime = runtime(false, Some(Arc::new(|_| {})));
        assert!(!runtime.emit_desktop_notification(&json!({ "title": "Ready" })));
    }

    #[test]
    fn reports_stdout_delivery_for_legacy_shells() {
        // Enabled, no injected callback → the stdout path answers true.
        let runtime = runtime(true, None);
        assert!(runtime.emit_desktop_notification(&json!({ "title": "Ready" })));
        // Non-object payloads are rejected.
        assert!(!runtime.emit_desktop_notification(&json!("plain")));
    }

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

    #[test]
    fn sse_frame_wraps_payload_as_data_line() {
        let runtime = runtime(true, None);
        assert_eq!(
            runtime.write_sse_event(&json!({ "type": "x" })),
            "data: {\"type\":\"x\"}\n\n"
        );
    }
}
