//! The local sherpa-onnx engine seam.
//!
//! DECISION (port manifest): the JS runs the native `sherpa-onnx-node`
//! addon inside a forked worker process (`local/worker-process.js`,
//! `local/sherpa-recognizer.js`, `local/sherpa-tts.js`,
//! `local/sherpa-loader.js`, `local/worker-client.js`). That addon cannot be
//! linked from this crate with the allowed dependencies, so the local
//! recognizer/speaker default to [`UnavailableLocalEngine`]: every download,
//! install-check, status, delete, and readiness behavior still runs, and the
//! connect/synthesize calls fail with the JS's exact error *shape* mapped by
//! the service (`{ error, retryable, reasonCode }` / the 500 TTS path).
//!
//! A future native binding implements these traits and is injected through
//! [`super::service::DictationService`]'s constructor — no other wiring
//! changes.
//!
//! 中文说明：本地 sherpa-onnx 引擎接缝。下载、安装检查、状态、删除与
//! 就绪行为全部保留；connect/synthesize 以与 JS 完全一致的错误形状失败
//! （由服务层映射为 `{ error, retryable, reasonCode }` / TTS 的 500 路径）。
//! 未来的原生绑定实现这两个 trait 并通过
//! [`super::service::DictationService`] 的构造函数注入即可，无需改动其它
//! 接线。

use std::path::Path;

use futures::future::BoxFuture;
use tokio::sync::mpsc;

use super::session::{SessionEvent, StreamingTranscriptionSession};

/// Error surfaced by the unavailable engine. The JS equivalent (worker
/// fails to load the sherpa addon) propagates through
/// `WorkerBackedTranscriptionSession.connect()` and is mapped by the service
/// exactly like any other connect failure.
///
/// 中文说明：文案与 JS 中 worker 加载 sherpa addon 失败时的错误一致，
/// 测试据此断言错误形状。
pub const UNAVAILABLE_MESSAGE: &str = "Local speech engine is unavailable in this server build (sherpa-onnx native binding not linked)";

/// A connected session plus its event stream.
///
/// 中文说明：会话对象负责缓冲与提交，事件流由流管理器消费。
pub type LocalSession = (
    Box<dyn StreamingTranscriptionSession>,
    mpsc::UnboundedReceiver<SessionEvent>,
);

/// `DictationWorkerClient.createSession` seam: resolve a connected streaming
/// session for one local model.
///
/// 中文说明：实现方负责加载模型文件并建立识别会话。
pub trait LocalRecognizer: Send + Sync {
    /// 加载模型并建立会话；失败返回错误消息（由服务层映射为就绪错误）。
    fn create_session(
        &self,
        models_dir: &Path,
        model_id: &str,
    ) -> BoxFuture<'static, Result<LocalSession, String>>;
}

/// `DictationWorkerClient.synthesizeSpeech` seam: synthesize speech with the
/// local TTS model, returning WAV bytes.
///
/// 中文说明：实现方负责模型加载、文本正规化与音频编码。
pub trait LocalSpeaker: Send + Sync {
    /// 合成一次语音；失败返回错误消息（映射为 500）。
    fn synthesize(
        &self,
        params: LocalSpeechParams,
    ) -> BoxFuture<'static, Result<LocalSpeechOutput, String>>;
}

/// 本地 TTS 合成入参。
#[derive(Debug, Clone)]
pub struct LocalSpeechParams {
    /// 模型根目录（引擎自行拼接模型子目录）。
    pub models_dir: std::path::PathBuf,
    /// 要使用的本地 TTS 模型 id。
    pub model_id: String,
    /// 要朗读的文本。
    pub text: String,
    /// speaker id；`None` 时由引擎自选。
    pub speaker_id: Option<i64>,
    /// 语速倍率；`None` 用引擎默认。
    pub speed: Option<f64>,
}

/// 本地 TTS 合成结果。
#[derive(Debug, Clone)]
pub struct LocalSpeechOutput {
    /// 合成的音频字节。
    pub audio: Vec<u8>,
    /// 音频 MIME 类型（如 `audio/wav`）。
    pub format: String,
}

/// Default engine: every native operation is unavailable. Wiring point for a
/// future `sherpa-onnx` Rust binding.
///
/// 中文说明：除返回 [`UNAVAILABLE_MESSAGE`] 外不做任何事，也不触碰磁盘。
#[derive(Debug, Clone, Default)]
pub struct UnavailableLocalEngine;

/// 识别接缝的不可用实现：恒返回 [`UNAVAILABLE_MESSAGE`]。
impl LocalRecognizer for UnavailableLocalEngine {
    /// 直接以失败 future 返回，不触碰模型目录。
    fn create_session(
        &self,
        _models_dir: &Path,
        _model_id: &str,
    ) -> BoxFuture<'static, Result<LocalSession, String>> {
        Box::pin(async { Err(UNAVAILABLE_MESSAGE.to_string()) })
    }
}

/// 合成接缝的不可用实现：恒返回 [`UNAVAILABLE_MESSAGE`]。
impl LocalSpeaker for UnavailableLocalEngine {
    /// 直接以失败 future 返回，不执行合成。
    fn synthesize(
        &self,
        _params: LocalSpeechParams,
    ) -> BoxFuture<'static, Result<LocalSpeechOutput, String>> {
        Box::pin(async { Err(UNAVAILABLE_MESSAGE.to_string()) })
    }
}

/// 引擎接缝契约测试。
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// 验证：不可用引擎的识别与合成接缝都失败，并返回文档化错误文案。
    #[tokio::test]
    async fn unavailable_engine_fails_with_documented_message() {
        let engine = UnavailableLocalEngine;
        let error = match engine
            .create_session(Path::new("/models"), "whisper-tiny-int8")
            .await
        {
            Ok(_) => panic!("expected the unavailable engine to fail"),
            Err(error) => error,
        };
        assert_eq!(error, UNAVAILABLE_MESSAGE);
        assert!(error.contains("sherpa-onnx"));

        let error = engine
            .synthesize(LocalSpeechParams {
                models_dir: PathBuf::from("/models"),
                model_id: "kokoro-en-v0_19".to_string(),
                text: "hi".to_string(),
                speaker_id: None,
                speed: None,
            })
            .await
            .unwrap_err();
        assert_eq!(error, UNAVAILABLE_MESSAGE);
    }
}
