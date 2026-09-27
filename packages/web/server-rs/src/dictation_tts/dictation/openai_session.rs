//! Port of `server/lib/dictation/openai-compatible-session.js` — the
//! pseudo-streaming transcription session for OpenAI-compatible Whisper
//! endpoints (faster-whisper, whisper.cpp, OpenAI, ...).
//!
//! The Whisper HTTP API cannot stream, so audio is buffered per segment and
//! transcribed on `commit()` — everything shorter than a segment is one
//! request on stop, matching how the local session behaves.
//!
//! 中文说明：OpenAI 兼容 Whisper 端点（faster-whisper、whisper.cpp、
//! OpenAI 等）的伪流式转写会话，移植自 `openai-compatible-session.js`。
//! Whisper HTTP API 无法真流式，因此音频按段缓冲、在 `commit()` 时整段
//! 转写；不足一段的音频在 stop 时一次转写，行为与本地会话对齐。

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use super::audio::pcm16_to_wav;
use super::session::{SessionEvent, StreamingTranscriptionSession, random_uuid};
use crate::dictation_tts::tts::stt::{TranscribeParams, Transcriber};

/// 该 provider 要求的输入采样率（Whisper 端点固定 16 kHz）。
pub const OPENAI_COMPATIBLE_SAMPLE_RATE: u32 = 16000;

/// The session config from the `start` options (`openaiCompatible`).
///
/// 中文说明：字段为 `None` 时 `connect()` 会给出对应的配置错误。
#[derive(Debug, Clone, Default)]
pub struct OpenAiCompatibleConfig {
    /// 兼容端点的 base URL（如 `http://localhost:8880/v1`）。
    pub base_url: Option<String>,
    /// 端点上的模型名（如 `whisper-1`）。
    pub model: Option<String>,
    /// 可选 bearer token；自建端点通常不需要。
    pub api_key: Option<String>,
    /// 可选语言提示（由服务层从 start 选项的 language 合并进来）。
    pub language: Option<String>,
}

/// 按段缓冲 PCM16 并在 commit 时发起一次 HTTP 转写的会话实现。
pub struct OpenAiCompatibleSession {
    /// 端点配置（connect 时校验）。
    config: OpenAiCompatibleConfig,
    /// 实际执行 HTTP 转写的客户端。
    transcriber: Arc<dyn Transcriber>,
    /// 固定为 16 kHz（见 [`OPENAI_COMPATIBLE_SAMPLE_RATE`]）。
    required_sample_rate: u32,
    /// `connect()` 成功后为 true；`close()` 置回 false。
    connected: bool,
    /// 当前累计段的 UUID。
    segment_id: String,
    /// 上一个已提交段的 id（串联段间先后关系）。
    previous_segment_id: Option<String>,
    /// 当前段累计的 PCM16LE 字节。
    pcm16: Vec<u8>,
    /// 会话事件的发送端（Committed/Transcript/Error）。
    events: mpsc::UnboundedSender<SessionEvent>,
}

/// 会话的构造、连接校验与事件发送辅助。
impl OpenAiCompatibleSession {
    /// 以配置、转写器与事件通道构造会话；初始段 id 为随机 UUID。
    pub fn new(
        config: OpenAiCompatibleConfig,
        transcriber: Arc<dyn Transcriber>,
        events: mpsc::UnboundedSender<SessionEvent>,
    ) -> Self {
        Self {
            config,
            transcriber,
            required_sample_rate: OPENAI_COMPATIBLE_SAMPLE_RATE,
            connected: false,
            segment_id: random_uuid(),
            previous_segment_id: None,
            pcm16: Vec::new(),
            events,
        }
    }

    /// `connect()`: configuration must carry a base URL and a model.
    ///
    /// 中文说明：base URL 与 model 缺失或为空时返回对应错误文案，
    /// 成功则置 `connected = true`。
    pub fn connect(&mut self) -> Result<(), String> {
        let Some(base_url) = self.config.base_url.as_deref() else {
            return Err("Custom STT server URL is not configured".to_string());
        };
        if base_url.is_empty() {
            return Err("Custom STT server URL is not configured".to_string());
        }
        match self.config.model.as_deref() {
            Some(model) if !model.is_empty() => {}
            _ => return Err("STT model is not configured".to_string()),
        }
        self.connected = true;
        Ok(())
    }

    /// 向事件通道发送一个事件（通道已关闭时静默丢弃）。
    fn emit(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }
}

/// 流式会话契约实现：缓冲 PCM16、commit 时异步转写并发出事件。
impl StreamingTranscriptionSession for OpenAiCompatibleSession {
    /// 会话要求的输入采样率（16 kHz，调用方负责重采样）。
    fn required_sample_rate(&self) -> u32 {
        self.required_sample_rate
    }

    /// 追加一段 PCM16LE 音频到当前段缓冲；未连接时发出 Error 事件并丢弃。
    fn append_pcm16(&mut self, chunk: Vec<u8>) {
        if !self.connected {
            self.emit(SessionEvent::Error {
                message: "STT session not connected".to_string(),
            });
            return;
        }
        self.pcm16.extend_from_slice(&chunk);
    }

    /// 提交当前段：先发 `Committed`（带前后段 id），再异步把 WAV 封装的
    /// 音频送转写器，结果以 `Transcript`（is_final=true）或 `Error` 送达。
    fn commit(&mut self) {
        if !self.connected {
            self.emit(SessionEvent::Error {
                message: "STT session not connected".to_string(),
            });
            return;
        }

        let committed_id = self.segment_id.clone();
        let previous_segment_id = self.previous_segment_id.clone();
        self.previous_segment_id = Some(committed_id.clone());
        self.segment_id = random_uuid();
        let committed_pcm16 = std::mem::take(&mut self.pcm16);
        self.emit(SessionEvent::Committed {
            segment_id: committed_id.clone(),
            previous_segment_id,
        });

        let transcriber = Arc::clone(&self.transcriber);
        let config = self.config.clone();
        let events = self.events.clone();
        tokio::spawn(async move {
            let wav = pcm16_to_wav(&committed_pcm16, OPENAI_COMPATIBLE_SAMPLE_RATE);
            let result = transcriber
                .transcribe(TranscribeParams {
                    audio_buffer: wav,
                    mime_type: "audio/wav".to_string(),
                    model: config.model.clone().unwrap_or_default(),
                    base_url: config.base_url.clone(),
                    api_key: config.api_key.clone(),
                    language: config.language.clone(),
                })
                .await;
            match result {
                Ok(text) => {
                    let _ = events.send(SessionEvent::Transcript {
                        segment_id: committed_id,
                        transcript: text.trim().to_string(),
                        is_final: true,
                    });
                }
                Err(message) => {
                    let _ = events.send(SessionEvent::Error { message });
                }
            }
        });
    }

    /// 丢弃当前段缓冲并轮换段 id（不产生事件）。
    fn clear(&mut self) {
        self.pcm16 = Vec::new();
        self.segment_id = random_uuid();
    }

    /// 标记断开并清空缓冲；后续 append/commit 会报 not connected。
    fn close(&mut self) {
        self.connected = false;
        self.pcm16 = Vec::new();
    }
}

/// Build the session from raw `start.options.openaiCompatible` JSON (the JS
/// reads `config.baseUrl`, `config.model`, `config.apiKey` with plain field
/// access — non-string values become `None`).
///
/// 中文说明：仅读取 `baseUrl`/`model`/`apiKey` 三个字符串字段，
/// 空串与非字符串值一律视为 `None`；`language` 不从该对象读取。
pub fn config_from_value(value: Option<&Value>) -> OpenAiCompatibleConfig {
    let Some(Value::Object(config)) = value else {
        return OpenAiCompatibleConfig::default();
    };
    let field = |name: &str| -> Option<String> {
        config
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    OpenAiCompatibleConfig {
        base_url: field("baseUrl"),
        model: field("model"),
        api_key: field("apiKey"),
        language: None,
    }
}

/// 会话契约测试：连接校验、未连接错误、commit 事件序列与配置解析。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation_tts::tts::stt::TranscribeFuture;
    use futures::future::BoxFuture;
    use std::sync::Mutex;

    /// 返回预置结果并记录每次调用参数的转写器桩。
    struct FakeTranscriber {
        /// 每次调用返回的结果（Ok 文本或 Err 消息）。
        result: Result<String, String>,
        /// 收到的 TranscribeParams 记录。
        calls: Mutex<Vec<TranscribeParams>>,
    }

    /// 桩实现：记录参数并返回预置结果。
    impl Transcriber for FakeTranscriber {
        /// 记录参数后异步返回预置结果。
        fn transcribe(&self, params: TranscribeParams) -> TranscribeFuture {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(params);
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    /// 构造（会话, 转写器桩, 事件接收端）三元组。
    fn session(
        config: OpenAiCompatibleConfig,
        result: Result<String, String>,
    ) -> (
        OpenAiCompatibleSession,
        Arc<FakeTranscriber>,
        mpsc::UnboundedReceiver<SessionEvent>,
    ) {
        let transcriber = Arc::new(FakeTranscriber {
            result,
            calls: Mutex::new(Vec::new()),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        (
            OpenAiCompatibleSession::new(
                config,
                Arc::clone(&transcriber) as Arc<dyn Transcriber>,
                tx,
            ),
            transcriber,
            rx,
        )
    }

    /// 完整可用的测试配置（本地端点 + whisper-tiny + en）。
    fn config() -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig {
            base_url: Some("http://localhost:8880/v1".to_string()),
            model: Some("whisper-tiny".to_string()),
            api_key: None,
            language: Some("en".to_string()),
        }
    }

    /// 验证：缺 base URL 与缺 model 分别报对应错误，配置齐全时连接成功。
    #[test]
    fn connect_requires_base_url_and_model() {
        let (mut sess, _, _) = session(OpenAiCompatibleConfig::default(), Ok(String::new()));
        assert_eq!(
            sess.connect().unwrap_err(),
            "Custom STT server URL is not configured"
        );

        let (mut sess, _, _) = session(
            OpenAiCompatibleConfig {
                base_url: Some("http://localhost:1/v1".into()),
                ..Default::default()
            },
            Ok(String::new()),
        );
        assert_eq!(sess.connect().unwrap_err(), "STT model is not configured");

        let (mut sess, _, _) = session(config(), Ok(String::new()));
        assert!(sess.connect().is_ok());
    }

    /// 验证：未连接时 append 与 commit 各发出一次 "STT session not
    /// connected" 错误事件。
    #[tokio::test]
    async fn unconnected_sessions_error_on_append_and_commit() {
        let (mut sess, _, mut rx) = session(config(), Ok("x".into()));
        sess.append_pcm16(vec![1, 2]);
        sess.commit();
        match rx.recv().await.unwrap() {
            SessionEvent::Error { message } => assert_eq!(message, "STT session not connected"),
            other => panic!("unexpected {other:?}"),
        }
        match rx.recv().await.unwrap() {
            SessionEvent::Error { message } => assert_eq!(message, "STT session not connected"),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// 验证：commit 先发 Committed 再发最终 Transcript；转写参数带
    /// WAV/模型/语言，音频长度为 44 字节头 + PCM 数据。
    #[tokio::test]
    async fn commit_emits_committed_then_final_transcript() {
        let (mut sess, transcriber, mut rx) = session(config(), Ok("  hello world  ".to_string()));
        sess.connect().unwrap();
        sess.append_pcm16(vec![0u8; 320]);
        sess.commit();

        match rx.recv().await.unwrap() {
            SessionEvent::Committed {
                segment_id,
                previous_segment_id,
            } => {
                assert!(!segment_id.is_empty());
                assert_eq!(previous_segment_id, None);
            }
            other => panic!("unexpected {other:?}"),
        }
        match rx.recv().await.unwrap() {
            SessionEvent::Transcript {
                segment_id: _,
                transcript,
                is_final,
            } => {
                assert_eq!(transcript, "hello world");
                assert!(is_final);
            }
            other => panic!("unexpected {other:?}"),
        }
        let params = transcriber
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap();
        assert_eq!(params.mime_type, "audio/wav");
        assert_eq!(params.model, "whisper-tiny");
        assert_eq!(params.language.as_deref(), Some("en"));
        assert_eq!(params.audio_buffer.len(), 44 + 320);
    }

    /// 验证：转写失败以 Error 事件（原样消息）送达。
    #[tokio::test]
    async fn transcription_errors_surface_as_error_events() {
        let (mut sess, _, mut rx) = session(config(), Err("boom".to_string()));
        sess.connect().unwrap();
        sess.commit();
        assert!(matches!(
            rx.recv().await.unwrap(),
            SessionEvent::Committed { .. }
        ));
        match rx.recv().await.unwrap() {
            SessionEvent::Error { message } => assert_eq!(message, "boom"),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// 验证：clear 清空缓冲并轮换段 id，close 置 disconnected 并清缓冲。
    #[test]
    fn clear_resets_the_segment_identity() {
        let (mut sess, _, _) = session(config(), Ok(String::new()));
        sess.connect().unwrap();
        let before = sess.segment_id.clone();
        sess.append_pcm16(vec![1, 2, 3, 4]);
        sess.clear();
        assert_eq!(sess.pcm16.len(), 0);
        assert_ne!(sess.segment_id, before);
        sess.close();
        assert!(!sess.connected);
        assert_eq!(sess.pcm16.len(), 0);
    }

    /// 验证：仅字符串字段被读取，非字符串值（如数字 apiKey）被忽略。
    #[test]
    fn config_from_value_reads_string_fields_only() {
        let value = serde_json::json!({
            "baseUrl": "http://localhost:8880/v1",
            "model": "m",
            "apiKey": 42,
            "extra": true
        });
        let config = config_from_value(Some(&value));
        assert_eq!(config.base_url.as_deref(), Some("http://localhost:8880/v1"));
        assert_eq!(config.model.as_deref(), Some("m"));
        assert_eq!(config.api_key, None);
        assert!(config_from_value(None).base_url.is_none());
    }
}
