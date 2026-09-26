//! Server-emitted SSE event hub.
//!
//! Minimal turn-1 primitive backing the port of `event-stream/global-hub.js` +
//! `opencode/watcher.js`: any server component can publish an SSE frame
//! (event name + serialized JSON payload) and every subscribed client stream
//! receives it. Directory scoping and resumable cursors live with the
//! `event_stream` module port, which owns those contracts.

use std::sync::Arc;

use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct HubEvent {
    /// SSE `event:` name (e.g. `ompchamber:session-status`).
    pub event: String,
    /// Serialized JSON for the SSE `data:` line.
    pub data: String,
}

pub struct EventHub {
    tx: broadcast::Sender<HubEvent>,
}

impl EventHub {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(1024);
        Arc::new(Self { tx })
    }

    /// Publish a frame; returns the number of clients it reached (0 when no
    /// subscriber is connected — a fan-out count, never an error).
    pub fn publish(&self, event: impl Into<String>, data: impl Into<String>) -> usize {
        let frame = HubEvent {
            event: event.into(),
            data: data.into(),
        };
        self.tx.send(frame).unwrap_or(0)
    }

    pub fn publish_json(&self, event: impl Into<String>, data: &serde_json::Value) -> usize {
        self.publish(
            event,
            serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string()),
        )
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.tx.subscribe()
    }
}

impl Default for EventHub {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(1024).0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delivers_to_subscribers_and_counts_reach() {
        let hub = EventHub::new();
        let mut rx1 = hub.subscribe();
        let mut rx2 = hub.subscribe();
        assert_eq!(hub.publish("ompchamber:test", "{\"a\":1}"), 2);
        assert_eq!(rx1.recv().await.expect("frame 1").event, "ompchamber:test");
        assert_eq!(rx2.recv().await.expect("frame 2").data, "{\"a\":1}");
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_zero_not_error() {
        let hub = EventHub::new();
        assert_eq!(hub.publish("ompchamber:none", "{}"), 0);
    }
}
