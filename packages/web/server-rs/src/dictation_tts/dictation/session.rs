//! The streaming-transcription-session contract shared by the dictation
//! stream manager and its providers (`openai-compatible-session.js`,
//! `local/worker-client.js` in the JS).
//!
//! A session buffers appended PCM16 audio, transcribes each committed
//! segment exactly once, and reports outcomes through an unbounded event
//! channel (the JS `EventEmitter` `committed` / `transcript` / `error`
//! events). Delivery is asynchronous: events arrive after an async hop, so
//! the manager re-checks stream state on arrival.
//!
//! 中文说明：本模块是听写流管理器与各 provider 共享的流式转写会话契约
//! （JS 中对应 `openai-compatible-session.js` 与 `local/worker-client.js`）。
//! 会话缓冲追加的 PCM16 音频、每段恰好转写一次，并通过无界事件通道
//! 汇报结果。事件送达经过一次 async 跳跃，因此管理器在事件到达时会
//! 重新检查流状态。

use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::mpsc;

/// Events a session emits (the JS `session.emit(...)` payloads).
///
/// 中文说明：事件按发生顺序入队；调用方通过接收端按序消费。
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// `committed { segmentId, previousSegmentId }`.
    /// 中文：一段音频已被接受转写。
    Committed {
        /// 刚提交的段 id。
        segment_id: String,
        /// 前一段的 id（首段为 `None`）。
        previous_segment_id: Option<String>,
    },
    /// `transcript { segmentId, transcript, isFinal }`.
    /// 中文：一段的转写文本。
    Transcript {
        /// 对应段的 id。
        segment_id: String,
        /// 转写文本（provider 应已 trim）。
        transcript: String,
        /// 是否最终结果（本移植中恒为 true）。
        is_final: bool,
    },
    /// `error (Error)` — message only crosses the wire.
    /// 中文：仅错误消息字符串跨线传播。
    Error { message: String },
}

/// The session contract consumed by [`super::stream_manager`].
///
/// 中文说明：实现方保证 commit 恰好产生一次确认事件。
pub trait StreamingTranscriptionSession: Send {
    /// 会话要求的输入采样率（调用方负责把音频重采样到该值）。
    fn required_sample_rate(&self) -> u32;
    /// 追加一段 PCM16LE 音频字节到当前段缓冲。
    fn append_pcm16(&mut self, chunk: Vec<u8>);
    /// Requests a commit; the segment acknowledgment arrives as a
    /// [`SessionEvent::Committed`].
    /// 中文：提交是异步的，转写结果随后以事件形式到达。
    fn commit(&mut self);
    /// 丢弃当前段缓冲（不产生事件），用于取消后重新开始。
    fn clear(&mut self);
    /// 关闭会话、释放引擎资源；此后不得再调用其它方法。
    fn close(&mut self);
}

/// 生成随机 UUIDv4 字符串（无 uuid crate 依赖：16 个随机字节加上
/// 版本与变体位后格式化）。
pub fn random_uuid() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Outcome of `createSttSession`: a connected session plus its event stream,
/// or the readiness error shape the WS protocol reports.
///
/// 中文说明：与 `service::SttSessionResolution` 一一对应，是面向流管理器
/// 的解耦形态（不依赖 service 模块的具体类型）。
pub enum SttSessionOutcome {
    /// 会话已连接。
    Session {
        /// 会话对象。
        session: Box<dyn StreamingTranscriptionSession>,
        /// 事件流接收端。
        events: mpsc::UnboundedReceiver<SessionEvent>,
    },
    /// 未就绪：携带错误消息、可重试标记与原因码。
    NotReady {
        /// 面向用户的错误消息。
        error: String,
        /// 是否应稍后重试。
        retryable: bool,
        /// 机器可读原因码（如 `model_download_in_progress`）。
        reason_code: Option<String>,
    },
}

/// 创建会话的注入函数类型：流管理器借此与具体服务实现解耦。
pub type CreateSttSession =
    Arc<dyn Fn(serde_json::Value) -> BoxFuture<'static, SttSessionOutcome> + Send + Sync>;
