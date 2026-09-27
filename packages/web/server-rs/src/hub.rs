//! Server-emitted SSE event hub.
//!
//! Minimal turn-1 primitive backing the port of `event-stream/global-hub.js` +
//! `opencode/watcher.js`: any server component can publish an SSE frame
//! (event name + serialized JSON payload) and every subscribed client stream
//! receives it. Directory scoping and resumable cursors live with the
//! `event_stream` module port, which owns those contracts.
//!
//! 中文说明：本模块是服务端 SSE 事件的进程内广播中枢。任意服务端组件调用
//! `publish` 发布一帧事件（事件名 + 已序列化的 JSON 负载），所有已订阅的
//! 客户端 SSE 流都会收到同一份数据。底层是 tokio broadcast channel（容量
//! 1024）。目录级作用域与可恢复游标等更复杂的契约由 `event_stream` 模块的
//! 移植负责，本模块只提供最小发布/订阅原语。

use std::sync::Arc;

use tokio::sync::broadcast;

/// 一帧待广播的 SSE 事件：`event:` 事件名 + `data:` 行的 JSON 文本。
#[derive(Debug, Clone)]
pub struct HubEvent {
    /// SSE `event:` name (e.g. `ompchamber:session-status`).
    /// SSE `event:` 字段的事件名，例如 `ompchamber:session-status`。
    pub event: String,
    /// Serialized JSON for the SSE `data:` line.
    /// SSE `data:` 行承载的已序列化 JSON 字符串。
    pub data: String,
}

/// 进程内 SSE 事件中枢：包装一个 broadcast channel，供各模块发布/订阅事件帧。
pub struct EventHub {
    /// broadcast 发送端；接收端由 `subscribe` 派生。容量 1024，慢消费者会收到 Lagged 错误而非阻塞发布方。
    tx: broadcast::Sender<HubEvent>,
}

/// `EventHub` 的构造与发布/订阅接口实现。
impl EventHub {
    /// 创建新的空事件中枢，返回 `Arc` 便于跨任务/跨模块共享。
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(1024);
        Arc::new(Self { tx })
    }

    /// Publish a frame; returns the number of clients it reached (0 when no
    /// subscriber is connected — a fan-out count, never an error).
    /// 中文说明：返回值是成功送达的订阅者数量；没有任何订阅者时返回 0
    /// （broadcast 语义下这不是错误）。
    pub fn publish(&self, event: impl Into<String>, data: impl Into<String>) -> usize {
        let frame = HubEvent {
            event: event.into(),
            data: data.into(),
        };
        self.tx.send(frame).unwrap_or(0)
    }

    /// 发布事件并携带任意 JSON 值作为负载：先序列化为字符串再委托给 `publish`；
    /// 序列化失败时降级为 `{}`，保证事件名仍能送达。
    pub fn publish_json(&self, event: impl Into<String>, data: &serde_json::Value) -> usize {
        self.publish(
            event,
            serde_json::to_string(data).unwrap_or_else(|_| "{}".to_string()),
        )
    }

    /// 订阅事件流：返回一个新的 broadcast 接收端。注意无订阅者期间发布的帧
    /// 无法恢复（broadcast 只投递给现存接收端）。
    pub fn subscribe(&self) -> broadcast::Receiver<HubEvent> {
        self.tx.subscribe()
    }
}

/// `Default` trait 实现：与 `new` 等价（容量 1024 的 broadcast channel）。
impl Default for EventHub {
    /// 构造默认的空事件中枢。
    fn default() -> Self {
        Self {
            tx: broadcast::channel(1024).0,
        }
    }
}

/// 事件中枢的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证：发布一帧可同时送达所有订阅者，且返回的触达数等于订阅者数量。
    #[tokio::test]
    async fn delivers_to_subscribers_and_counts_reach() {
        let hub = EventHub::new();
        let mut rx1 = hub.subscribe();
        let mut rx2 = hub.subscribe();
        assert_eq!(hub.publish("ompchamber:test", "{\"a\":1}"), 2);
        assert_eq!(rx1.recv().await.expect("frame 1").event, "ompchamber:test");
        assert_eq!(rx2.recv().await.expect("frame 2").data, "{\"a\":1}");
    }

    /// 验证：无订阅者时 `publish` 返回 0 而不是报错（fan-out 计数语义）。
    #[tokio::test]
    async fn publish_without_subscribers_is_zero_not_error() {
        let hub = EventHub::new();
        assert_eq!(hub.publish("ompchamber:none", "{}"), 0);
    }
}
