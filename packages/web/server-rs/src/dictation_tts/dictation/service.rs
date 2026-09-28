//! Port of `server/lib/dictation/service.js` — provider resolution and
//! readiness. Providers:
//! - `local` (default): sherpa-onnx in a worker process in the JS; here the
//!   [`LocalRecognizer`]/[`LocalSpeaker`] seams default to the documented
//!   Unavailable engine (models still download, install-check, and report).
//! - `openai-compatible`: buffered per-segment transcription against any
//!   OpenAI-compatible endpoint.
//!
//! 中文说明：JS 版 `server/lib/dictation/service.js` 的移植，负责听写
//! provider 的解析与就绪状态管理。本地模型的下载、安装检查、状态上报、
//! 删除与 TTS 合成（含按文本语言自动切换模型）均在此模块实现。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::local::{LocalRecognizer, LocalSpeaker, LocalSpeechParams, UnavailableLocalEngine};
use super::model_catalog::{
    DEFAULT_LOCAL_STT_MODEL, DEFAULT_LOCAL_TTS_MODEL, get_local_model_dir,
    get_local_tts_default_speaker, is_local_model_id, is_local_stt_model_id, is_local_tts_model_id,
    local_model_spec, local_stt_model_ids, local_tts_model_ids,
    resolve_local_tts_model_for_language,
};
use super::model_downloader::{ProgressFn, ensure_local_model, is_local_model_installed};
use super::openai_session::OpenAiCompatibleSession;
use super::session::SessionEvent;
use super::session::StreamingTranscriptionSession;
use crate::dictation_tts::tts::language_detect::detect_text_language;
use crate::dictation_tts::tts::stt::{HttpTranscriber, Transcriber};

/// Download bookkeeping: the JS `downloadStates` (`'downloading'` |
/// `'error'`), `downloadErrors`, `downloadProgress`, and the in-flight
/// `downloadPromises` dedupe.
///
/// 中文说明：以 model id 为键记录状态标记、错误消息、进度百分比与在途
/// 任务句柄；`running` 承担 JS `downloadPromises` 的去重职责。
#[derive(Default)]
struct Downloads {
    /// model id → 状态标记（`"downloading"` 或 `"error"`）；无条目表示空闲。
    states: HashMap<String, &'static str>,
    /// model id → 最近一次下载失败的错误消息（仅 `"error"` 状态时有意义）。
    errors: HashMap<String, String>,
    /// model id → 下载进度百分比（0-100）；内层 `None` 表示总大小未知。
    progress: HashMap<String, Option<u64>>,
    /// model id → 在途下载任务句柄，避免同一模型重复启动下载。
    running: HashMap<String, tokio::task::JoinHandle<()>>,
}

/// 下载状态表的只读判断辅助。
impl Downloads {
    /// 该模型当前是否处于 `"downloading"` 状态。
    fn is_downloading(&self, model_id: &str) -> bool {
        self.states.get(model_id).copied() == Some("downloading")
    }

    /// 该模型最近一次下载是否以 `"error"` 结束。
    fn is_error(&self, model_id: &str) -> bool {
        self.states.get(model_id).copied() == Some("error")
    }
}

/// The readiness error shape shared by `createSttSession` and
/// `synthesizeSpeech`.
///
/// 中文说明：JS 中以 `{ error, retryable, reasonCode }` 形状跨 WS/HTTP
/// 上报；`retryable` 决定客户端是否应稍后重试。
#[derive(Debug, Clone)]
pub struct ReadyError {
    /// 面向用户的错误消息。
    pub error: String,
    /// 客户端是否应稍后重试（如模型正在下载）。
    pub retryable: bool,
    /// 机器可读的原因码（如 `model_download_in_progress`）。
    pub reason_code: Option<String>,
}

/// `synthesizeSpeech` 的成功结果。
#[derive(Debug, Clone)]
pub struct SynthesizeSpeech {
    /// 合成的音频字节（容器格式由 `format` 描述）。
    pub audio: Vec<u8>,
    /// 音频 MIME 类型（如 `audio/wav`）；空串时路由层回退为 `audio/wav`。
    pub format: String,
    /// 实际使用的 TTS 模型 id（语言自动切换后可能与请求值不同）。
    pub model_id: String,
    /// `language: "auto"` 时检测出的语言代码；未启用检测时为 `None`。
    pub language: Option<String>,
}

/// `synthesizeSpeech` 的入参选项（来自 speak 路由的请求体）。
#[derive(Debug, Clone, Default)]
pub struct SynthesizeOptions {
    /// 要朗读的文本（调用方已 trim 且确认非空）。
    pub text: String,
    /// 请求的本地 TTS 模型 id；非法或缺失时回退到默认模型。
    pub model: Option<String>,
    /// 请求的 speaker id；语言切换后可能被目标模型的默认 speaker 覆盖。
    pub speaker_id: Option<i64>,
    /// 语速倍率（1.0 为正常速度）。
    pub speed: Option<f64>,
    /// 仅当值为 `"auto"` 时启用语言自动检测（路由层已归一化）。
    pub language: Option<String>,
    /// 用于语言检测的整条消息样本（短 chunk 不足以判断语言时使用）。
    pub language_sample: Option<String>,
}

/// `synthesizeSpeech` failures: readiness errors answer 503 with a reason
/// code; an engine throw answers 500 with the raw message (the JS route's
/// `catch`).
///
/// 中文说明：`Ready` 变体走 503（带 retryable/reasonCode 的 JSON），
/// `Engine` 变体走 500（原始错误消息）。
#[derive(Debug, Clone)]
pub enum SynthesizeError {
    /// 模型未就绪（下载中/下载失败/引擎不可用）——503 路径。
    Ready(ReadyError),
    /// 本地引擎合成时抛出的错误——500 路径（JS 路由的 catch）。
    Engine(String),
}

/// `createSttSession` 的返回值：已连接的会话及其事件流，或就绪错误。
pub enum SttSessionResolution {
    /// 会话已连接。
    Session {
        /// 已连接的流式转写会话对象。
        session: Box<dyn StreamingTranscriptionSession>,
        /// 会话事件流的接收端（Committed/Transcript/Error）。
        events: mpsc::UnboundedReceiver<SessionEvent>,
    },
    /// provider 未就绪：携带 WS 协议上报的错误形状。
    NotReady(ReadyError),
}

/// 解析本地 STT 模型 id：请求值是合法目录项时原样返回，否则回退默认模型。
fn resolve_local_model_id(requested: Option<&str>) -> String {
    match requested {
        Some(id) if is_local_stt_model_id(id) => id.to_string(),
        _ => DEFAULT_LOCAL_STT_MODEL.to_string(),
    }
}

/// 听写核心服务：两个 provider 的 STT 会话创建、本地 TTS 合成、就绪状态
/// 查询与模型下载/删除管理；以 `Arc` 形式被路由层共享。
pub struct DictationService {
    /// 本地模型根目录（用户配置目录下的 `speech-models`）。
    models_dir: PathBuf,
    /// 本地 STT 引擎接缝（默认 Unavailable；`local-speech` 特性下为原生绑定）。
    recognizer: Arc<dyn LocalRecognizer>,
    /// 本地 TTS 引擎接缝。
    speaker: Arc<dyn LocalSpeaker>,
    /// openai-compatible provider 使用的 HTTP 转写客户端。
    transcriber: Arc<dyn Transcriber>,
    /// 下载簿记表（互斥锁保护；跨 await 前先做快照再释放）。
    downloads: Mutex<Downloads>,
}

/// 服务主体：provider 解析、下载生命周期、STT 会话与 TTS 合成。
impl DictationService {
    /// 生产构造函数：按 `local-speech` 特性选择本地引擎（原生 sherpa 绑定
    /// 或 Unavailable 引擎），HTTP 转写器固定为默认 [`HttpTranscriber`]。
    pub fn new(models_dir: PathBuf) -> Arc<Self> {
        #[cfg(feature = "local-speech")]
        let (recognizer, speaker) = crate::dictation_tts::native_sherpa::engine_pair();
        #[cfg(not(feature = "local-speech"))]
        let (recognizer, speaker) = (
            Arc::new(UnavailableLocalEngine),
            Arc::new(UnavailableLocalEngine),
        );
        Self::with_engines(
            models_dir,
            recognizer,
            speaker,
            Arc::new(HttpTranscriber::default()),
        )
    }

    /// 显式注入引擎的构造函数：测试与未来原生绑定的接线点。
    pub fn with_engines(
        models_dir: PathBuf,
        recognizer: Arc<dyn LocalRecognizer>,
        speaker: Arc<dyn LocalSpeaker>,
        transcriber: Arc<dyn Transcriber>,
    ) -> Arc<Self> {
        Arc::new(Self {
            models_dir,
            recognizer,
            speaker,
            transcriber,
            downloads: Mutex::new(Downloads::default()),
        })
    }

    /// `startModelDownload` — deduped and fire-and-forget (`void` in the
    /// JS); the state maps are the observable surface.
    ///
    /// 中文说明：先按在途句柄去重再置状态；任务结束时成功则清除状态，
    /// 失败则写入 `"error"` 与错误消息。进度回调把字节数换算为百分比。
    fn start_model_download(self: &Arc<Self>, model_id: &str) {
        {
            let mut downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
            if downloads
                .running
                .get(model_id)
                .is_some_and(|handle| !handle.is_finished())
            {
                return;
            }
            downloads.downloading_marker(model_id);
        }

        let service = Arc::clone(self);
        let model_id = model_id.to_string();
        let model_id_key = model_id.clone();
        let handle = tokio::spawn(async move {
            let progress: ProgressFn = {
                let service = Arc::clone(&service);
                let model_id = model_id.clone();
                Arc::new(move |downloaded: u64, total: Option<u64>| {
                    let percent = total
                        .filter(|total| *total > 0)
                        .map(|total| ((downloaded as f64 / total as f64) * 100.0).round() as u64)
                        .map(|percent| percent.min(100));
                    let mut downloads = service.downloads.lock().unwrap_or_else(|e| e.into_inner());
                    downloads.progress.insert(model_id.clone(), percent);
                })
            };
            let result = ensure_local_model(&service.models_dir, &model_id, Some(progress)).await;
            let mut downloads = service.downloads.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(_) => {
                    downloads.states.remove(&model_id);
                    downloads.progress.remove(&model_id);
                }
                Err(message) => {
                    downloads.states.insert(model_id.clone(), "error");
                    downloads.errors.insert(model_id.clone(), message);
                    downloads.progress.remove(&model_id);
                }
            }
        });
        self.downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .running
            .insert(model_id_key, handle);
    }

    /// `createSttSession`: a connected session for one dictation, or the
    /// readiness error when the provider is not ready.
    ///
    /// 中文说明：openai-compatible 路径校验配置后建会话（配置缺失为
    /// 不可重试的 stt_not_configured）；local 路径在模型缺失时按状态表
    /// 返回下载中/下载失败，并识别损坏模型触发删除重下。
    pub async fn create_stt_session(self: &Arc<Self>, options: &Value) -> SttSessionResolution {
        let provider = options.get("provider").and_then(Value::as_str);
        if provider == Some("openai-compatible") {
            let mut config =
                super::openai_session::config_from_value(options.get("openaiCompatible"));
            config.language = options
                .get("language")
                .and_then(Value::as_str)
                .filter(|language| !language.is_empty())
                .map(str::to_string)
                .or(config.language);
            let (tx, events) = mpsc::unbounded_channel();
            let mut session =
                OpenAiCompatibleSession::new(config, Arc::clone(&self.transcriber), tx);
            if let Err(error) = session.connect() {
                return SttSessionResolution::NotReady(ReadyError {
                    error,
                    retryable: false,
                    reason_code: Some("stt_not_configured".to_string()),
                });
            }
            return SttSessionResolution::Session {
                session: Box::new(session),
                events,
            };
        }

        let model_id = resolve_local_model_id(options.get("localModel").and_then(Value::as_str));
        if !is_local_model_installed(&self.models_dir, &model_id).await {
            let state = {
                let downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                if downloads.is_error(&model_id) {
                    "error"
                } else if downloads.is_downloading(&model_id) {
                    "downloading"
                } else {
                    ""
                }
            };
            if state == "error" {
                let message = {
                    let mut downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                    // Allow a retry on the next attempt.
                    downloads.states.remove(&model_id);
                    downloads.errors.remove(&model_id)
                }
                .unwrap_or_else(|| "Model download failed".to_string());
                return SttSessionResolution::NotReady(ReadyError {
                    error: format!("Failed to download dictation model: {message}"),
                    retryable: true,
                    reason_code: Some("model_download_failed".to_string()),
                });
            }
            self.start_model_download(&model_id);
            return SttSessionResolution::NotReady(ReadyError {
                error: "Dictation model is downloading".to_string(),
                retryable: true,
                reason_code: Some("model_download_in_progress".to_string()),
            });
        }

        match self
            .recognizer
            .create_session(&self.models_dir, &model_id)
            .await
        {
            Ok((session, events)) => SttSessionResolution::Session { session, events },
            Err(message) => {
                // A model that passes the file-presence check but fails to
                // load is corrupt on disk (e.g. truncated by an interrupted
                // extraction). Remove it so the next attempt re-downloads
                // instead of crashing forever.
                if message.contains("Load model") || message.contains("Protobuf parsing failed") {
                    let dir = get_local_model_dir(&self.models_dir, &model_id);
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                    return SttSessionResolution::NotReady(ReadyError {
                        error: "Dictation model files were corrupt and have been removed; retry to re-download".to_string(),
                        retryable: true,
                        reason_code: Some("model_corrupt".to_string()),
                    });
                }
                SttSessionResolution::NotReady(ReadyError {
                    error: message,
                    retryable: true,
                    reason_code: Some("stt_unavailable".to_string()),
                })
            }
        }
    }

    /// `getStatus` — the readiness snapshot for the status route and UI
    /// gating.
    ///
    /// 中文说明：按 provider 与 active model 的安装/下载状态拼出
    /// available、reasonCode 与模型清单（models/ttsModels）。
    pub async fn get_status(&self, options: &Value) -> Value {
        let provider =
            if options.get("provider").and_then(Value::as_str) == Some("openai-compatible") {
                "openai-compatible"
            } else {
                "local"
            };
        let model_id = resolve_local_model_id(options.get("localModel").and_then(Value::as_str));

        let mut models = Vec::new();
        for id in local_stt_model_ids() {
            models.push(self.describe_model(id).await);
        }
        let mut tts_models = Vec::new();
        for id in local_tts_model_ids() {
            tts_models.push(self.describe_model(id).await);
        }

        if provider == "openai-compatible" {
            return json!({ "provider": provider, "available": true, "models": models, "ttsModels": tts_models });
        }

        let model = models
            .iter()
            .find(|entry| entry["id"].as_str() == Some(model_id.as_str()));
        match model {
            Some(model) if model["installed"].as_bool() == Some(true) => json!({
                "provider": provider,
                "available": true,
                "activeModel": model_id,
                "models": models,
                "ttsModels": tts_models,
            }),
            Some(model) if model["downloading"].as_bool() == Some(true) => json!({
                "provider": provider,
                "available": false,
                "reasonCode": "model_download_in_progress",
                "activeModel": model_id,
                "models": models,
                "ttsModels": tts_models,
            }),
            Some(model) if !model["downloadError"].is_null() => json!({
                "provider": provider,
                "available": false,
                "reasonCode": "model_download_failed",
                "error": model["downloadError"].clone(),
                "activeModel": model_id,
                "models": models,
                "ttsModels": tts_models,
            }),
            _ => json!({
                "provider": provider,
                "available": false,
                "reasonCode": "models_missing",
                "activeModel": model_id,
                "models": models,
                "ttsModels": tts_models,
            }),
        }
    }

    /// `describeModel`: id, description, install state, download state.
    ///
    /// 中文说明：先对状态表取快照再做异步安装检查，避免检查期间持锁
    /// 阻塞下载任务的写入。
    async fn describe_model(&self, id: &str) -> Value {
        // Snapshot the maps first — the install check awaits and a download
        // task may want the lock.
        let (downloading, download_progress, download_error) = {
            let downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
            (
                downloads.is_downloading(id),
                downloads.progress.get(id).cloned().flatten(),
                downloads.errors.get(id).cloned(),
            )
        };
        json!({
            "id": id,
            "description": local_model_spec(id).map(|spec| spec.description).unwrap_or(""),
            "installed": is_local_model_installed(&self.models_dir, id).await,
            "downloading": downloading,
            "downloadProgress": download_progress,
            "downloadError": download_error,
        })
    }

    /// `synthesizeSpeech` — local TTS with language-aware model selection.
    ///
    /// With `language: 'auto'` the text's language decides the model: the
    /// caller's model when it speaks that language, otherwise the catalog
    /// model for it (downloaded on first use, reported as in-progress until
    /// it lands). A language no catalog model covers keeps the caller's
    /// model, so text is never silently dropped.
    ///
    /// 中文说明：模型缺失时启动后台下载并以 503 就绪错误应答，引擎失败
    /// 走 500；语言检测基于整条消息样本而非短 chunk。
    pub async fn synthesize_speech(
        self: &Arc<Self>,
        options: SynthesizeOptions,
    ) -> Result<SynthesizeSpeech, SynthesizeError> {
        let requested_model_id = match options.model.as_deref() {
            Some(model) if is_local_tts_model_id(model) => model.to_string(),
            _ => DEFAULT_LOCAL_TTS_MODEL.to_string(),
        };
        let mut model_id = requested_model_id.clone();
        let mut speaker_id = options.speaker_id;
        let mut resolved_language = None;
        if options.language.as_deref() == Some("auto") {
            // `languageSample` is the whole message the chunk belongs to:
            // the language is judged on that, never on a short chunk alone.
            let sample = options
                .language_sample
                .clone()
                .filter(|sample| !sample.is_empty())
                .unwrap_or_else(|| options.text.clone());
            let detected = detect_text_language(&sample).language;
            let for_language =
                resolve_local_tts_model_for_language(&detected, Some(&requested_model_id));
            if let Some(for_language) = for_language
                && for_language != requested_model_id
            {
                model_id = for_language;
                speaker_id = get_local_tts_default_speaker(&model_id, &detected);
            }
            resolved_language = Some(detected);
        }
        if !is_local_model_installed(&self.models_dir, &model_id).await {
            let is_error = {
                let downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                downloads.is_error(&model_id)
            };
            if is_error {
                let message = {
                    let mut downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
                    downloads.states.remove(&model_id);
                    downloads.errors.remove(&model_id)
                }
                .unwrap_or_else(|| "Model download failed".to_string());
                return Err(SynthesizeError::Ready(ReadyError {
                    error: format!("Failed to download TTS model: {message}"),
                    retryable: true,
                    reason_code: Some("model_download_failed".to_string()),
                }));
            }
            self.start_model_download(&model_id);
            return Err(SynthesizeError::Ready(ReadyError {
                error: "TTS model is downloading".to_string(),
                retryable: true,
                reason_code: Some("model_download_in_progress".to_string()),
            }));
        }

        let result = self
            .speaker
            .synthesize(LocalSpeechParams {
                models_dir: self.models_dir.clone(),
                model_id: model_id.clone(),
                text: options.text,
                speaker_id,
                speed: options.speed,
            })
            .await;
        match result {
            Ok(output) => Ok(SynthesizeSpeech {
                audio: output.audio,
                format: output.format,
                model_id,
                language: resolved_language,
            }),
            // The JS worker-client throw surfaces through the route's
            // 500 catch.
            Err(message) => Err(SynthesizeError::Engine(message)),
        }
    }

    /// `requestModelDownload` — background download kick for Settings.
    ///
    /// 中文说明：未知 id 报错；已安装视为 no-op；否则启动去重的后台下载。
    pub async fn request_model_download(self: &Arc<Self>, model_id: &str) -> Value {
        if !is_local_model_id(model_id) {
            return json!({ "ok": false, "error": "Unknown model id" });
        }
        if is_local_model_installed(&self.models_dir, model_id).await {
            return json!({ "ok": true, "installed": true });
        }
        self.start_model_download(model_id);
        json!({ "ok": true, "installed": false })
    }

    /// `deleteModel` — remove an installed model from disk. A model that is
    /// mid-download cannot be deleted; an engine already loaded keeps its
    /// in-memory copy until idle shutdown — the files are simply
    /// re-downloaded on next use.
    ///
    /// 中文说明：删除模型目录并清掉残留下载错误；不影响已加载引擎的
    /// 内存副本（下次使用时会重新下载文件）。
    pub async fn delete_model(&self, model_id: &str) -> Value {
        if !is_local_model_id(model_id) {
            return json!({ "ok": false, "error": "Unknown model id" });
        }
        let downloading = {
            let downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
            downloads.is_downloading(model_id)
        };
        if downloading {
            return json!({ "ok": false, "error": "Model is downloading" });
        }
        let dir = get_local_model_dir(&self.models_dir, model_id);
        let _ = tokio::fs::remove_dir_all(&dir).await;
        self.downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .errors
            .remove(model_id);
        json!({ "ok": true })
    }
}

/// 测试专用入口：直接操纵下载状态表。
#[cfg(test)]
impl DictationService {
    /// Force the `'downloading'` state without spawning a real download
    /// (test-only; the JS suite reaches this state with mocked fetches).
    ///
    /// 中文说明：JS 套件借助 mock fetch 达到同一状态。
    pub(crate) fn mark_downloading_for_test(&self, model_id: &str) {
        self.downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .downloading_marker(model_id);
    }

    /// 清除指定模型的状态与进度标记（测试间隔离用）。
    pub(crate) fn clear_download_state_for_test(&self, model_id: &str) {
        let mut downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
        downloads.states.remove(model_id);
        downloads.progress.remove(model_id);
    }
}

/// 下载状态表的写入辅助。
impl Downloads {
    /// 标记进入下载中：写入 `"downloading"`、清除旧错误、进度置 0。
    fn downloading_marker(&mut self, model_id: &str) {
        self.states.insert(model_id.to_string(), "downloading");
        self.errors.remove(model_id);
        self.progress.insert(model_id.to_string(), Some(0));
    }
}

/// 服务行为契约测试：provider 解析、下载状态机、语言感知 TTS 与模型管理。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation_tts::dictation::local::LocalSpeechOutput;
    use crate::dictation_tts::tts::stt::TranscribeFuture;
    use futures::future::BoxFuture;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 临时目录名的去重计数器。
    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// 创建唯一的临时目录（label + 进程 id + 序号）。
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-dictation-svc-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// 恒返回空文本的转写器桩。
    struct NullTranscriber;
    /// 桩实现：任何音频都转写为空字符串。
    impl Transcriber for NullTranscriber {
        /// 立即成功返回空文本。
        fn transcribe(
            &self,
            _params: crate::dictation_tts::tts::stt::TranscribeParams,
        ) -> TranscribeFuture {
            Box::pin(async { Ok(String::new()) })
        }
    }

    /// 记录 createSession 调用参数的识别器桩，可注入失败消息。
    struct RecordingRecognizer {
        /// 收到的 model id 调用记录。
        calls: Mutex<Vec<String>>,
        /// 注入的失败消息（`Some` 时 create_session 返回 Err）。
        fail_with: Option<String>,
    }

    /// 桩实现：记录 model id；成功时返回最小静音会话。
    impl LocalRecognizer for RecordingRecognizer {
        /// 记录调用并按 `fail_with` 决定成败。
        fn create_session(
            &self,
            _models_dir: &std::path::Path,
            model_id: &str,
        ) -> BoxFuture<
            'static,
            Result<
                (
                    Box<dyn StreamingTranscriptionSession>,
                    mpsc::UnboundedReceiver<SessionEvent>,
                ),
                String,
            >,
        > {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(model_id.to_string());
            let error = self.fail_with.clone();
            Box::pin(async move {
                // A minimal silent session.
                // 最小静音会话桩（中文补充）。
                struct NullSession;
                // 会话契约的空实现。
                impl StreamingTranscriptionSession for NullSession {
                    // 固定 16 kHz。
                    fn required_sample_rate(&self) -> u32 {
                        16000
                    }
                    // 忽略音频。
                    fn append_pcm16(&mut self, _chunk: Vec<u8>) {}
                    // 空提交。
                    fn commit(&mut self) {}
                    // 空清除。
                    fn clear(&mut self) {}
                    // 空关闭。
                    fn close(&mut self) {}
                }
                match error {
                    Some(error) => Err(error),
                    None => {
                        let (tx, rx) = mpsc::unbounded_channel();
                        Ok((
                            Box::new(NullSession) as Box<dyn StreamingTranscriptionSession>,
                            rx,
                        ))
                    }
                }
            })
        }
    }

    /// 记录合成参数并返回固定音频的 speaker 桩。
    struct RecordingSpeaker {
        /// 收到的合成参数记录。
        calls: Mutex<Vec<LocalSpeechParams>>,
        /// 每次合成返回的固定音频字节。
        audio: Vec<u8>,
    }

    /// 桩实现：记录参数并以 audio/wav 返回预置字节。
    impl LocalSpeaker for RecordingSpeaker {
        /// 记录参数，异步返回预置音频。
        fn synthesize(
            &self,
            params: LocalSpeechParams,
        ) -> BoxFuture<'static, Result<LocalSpeechOutput, String>> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(params.clone());
            let audio = self.audio.clone();
            Box::pin(async move {
                Ok(LocalSpeechOutput {
                    audio,
                    format: "audio/wav".to_string(),
                })
            })
        }
    }

    /// 在模型目录中伪造 whisper-tiny 的三个必需文件（内容任意但非空）。
    fn install_whisper_tiny(models_dir: &std::path::Path) {
        let model_dir = models_dir.join("sherpa-onnx-whisper-tiny");
        std::fs::create_dir_all(&model_dir).unwrap();
        for file in [
            "tiny-encoder.int8.onnx",
            "tiny-decoder.int8.onnx",
            "tiny-tokens.txt",
        ] {
            std::fs::write(model_dir.join(file), b"x").unwrap();
        }
    }

    /// 验证：本地 provider 且模型缺失时返回可重试的
    /// model_download_in_progress 并触发后台下载。
    #[tokio::test]
    async fn local_provider_with_missing_models_reports_download_in_progress() {
        let dir = temp_dir("missing");
        let service = DictationService::with_engines(
            dir.clone(),
            Arc::new(RecordingRecognizer {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }),
            Arc::new(RecordingSpeaker {
                calls: Mutex::new(Vec::new()),
                audio: vec![],
            }),
            Arc::new(NullTranscriber),
        );
        let SttSessionResolution::NotReady(error) = service.create_stt_session(&json!({})).await
        else {
            panic!("expected NotReady");
        };
        assert_eq!(error.error, "Dictation model is downloading");
        assert!(error.retryable);
        assert_eq!(
            error.reason_code.as_deref(),
            Some("model_download_in_progress")
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：非法 localModel id 回退到默认 parakeet 模型（仍缺失时同样
    /// 报告下载中）。
    #[tokio::test]
    async fn unknown_local_models_fall_back_to_the_default() {
        let dir = temp_dir("default");
        install_whisper_tiny(&dir);
        // whisper-tiny is installed, but the requested (bogus) id resolves
        // to the default parakeet model — still missing.
        let service = DictationService::with_engines(
            dir.clone(),
            Arc::new(RecordingRecognizer {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }),
            Arc::new(RecordingSpeaker {
                calls: Mutex::new(Vec::new()),
                audio: vec![],
            }),
            Arc::new(NullTranscriber),
        );
        let SttSessionResolution::NotReady(error) = service
            .create_stt_session(&json!({ "localModel": "not-a-model" }))
            .await
        else {
            panic!("expected NotReady");
        };
        assert_eq!(
            error.reason_code.as_deref(),
            Some("model_download_in_progress")
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：模型已安装但引擎不可用时映射为可重试的 stt_unavailable，
    /// 错误消息与 JS 完全一致。
    #[tokio::test]
    async fn installed_model_with_unavailable_engine_maps_to_stt_unavailable() {
        let dir = temp_dir("installed");
        install_whisper_tiny(&dir);
        let recognizer = Arc::new(RecordingRecognizer {
            calls: Mutex::new(Vec::new()),
            fail_with: Some(
                crate::dictation_tts::dictation::local::UNAVAILABLE_MESSAGE.to_string(),
            ),
        });
        let service = DictationService::with_engines(
            dir.clone(),
            recognizer,
            Arc::new(RecordingSpeaker {
                calls: Mutex::new(Vec::new()),
                audio: vec![],
            }),
            Arc::new(NullTranscriber),
        );
        let SttSessionResolution::NotReady(error) = service
            .create_stt_session(&json!({ "localModel": "whisper-tiny-int8" }))
            .await
        else {
            panic!("expected NotReady");
        };
        // The JS's exact error shape for an engine that cannot load.
        assert_eq!(
            error.error,
            "Local speech engine is unavailable in this server build (sherpa-onnx native binding not linked)"
        );
        assert!(error.retryable);
        assert_eq!(error.reason_code.as_deref(), Some("stt_unavailable"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：加载失败消息含 "Load model"/"Protobuf parsing failed" 时
    /// 判定模型损坏——删除模型目录并返回 model_corrupt 让下次重下。
    #[tokio::test]
    async fn corrupt_engine_failure_removes_the_model_directory() {
        let dir = temp_dir("corrupt");
        install_whisper_tiny(&dir);
        let recognizer = Arc::new(RecordingRecognizer {
            calls: Mutex::new(Vec::new()),
            fail_with: Some("Load model failed".to_string()),
        });
        let service = DictationService::with_engines(
            dir.clone(),
            recognizer,
            Arc::new(RecordingSpeaker {
                calls: Mutex::new(Vec::new()),
                audio: vec![],
            }),
            Arc::new(NullTranscriber),
        );
        let SttSessionResolution::NotReady(error) = service
            .create_stt_session(&json!({ "localModel": "whisper-tiny-int8" }))
            .await
        else {
            panic!("expected NotReady");
        };
        assert_eq!(
            error.error,
            "Dictation model files were corrupt and have been removed; retry to re-download"
        );
        assert_eq!(error.reason_code.as_deref(), Some("model_corrupt"));
        assert!(!dir.join("sherpa-onnx-whisper-tiny").exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：openai-compatible 缺 base URL 时返回不可重试的
    /// stt_not_configured；配置齐全时成功建立会话。
    #[tokio::test]
    async fn openai_compatible_provider_validates_config() {
        let dir = temp_dir("compat");
        let service = DictationService::with_engines(
            dir.clone(),
            Arc::new(RecordingRecognizer {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }),
            Arc::new(RecordingSpeaker {
                calls: Mutex::new(Vec::new()),
                audio: vec![],
            }),
            Arc::new(NullTranscriber),
        );
        let SttSessionResolution::NotReady(error) = service
            .create_stt_session(&json!({ "provider": "openai-compatible", "openaiCompatible": {} }))
            .await
        else {
            panic!("expected NotReady");
        };
        assert_eq!(error.error, "Custom STT server URL is not configured");
        assert!(!error.retryable);
        assert_eq!(error.reason_code.as_deref(), Some("stt_not_configured"));

        let SttSessionResolution::Session { .. } = service
            .create_stt_session(&json!({
                "provider": "openai-compatible",
                "openaiCompatible": { "baseUrl": "http://localhost:8880/v1", "model": "whisper" }
            }))
            .await
        else {
            panic!("expected a session");
        };
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：get_status 在模型缺失时报 models_missing 并列出全部 STT/TTS
    /// 目录项；openai-compatible provider 恒为 available。
    #[tokio::test]
    async fn status_reports_missing_models_with_the_unavailable_local_engine() {
        let dir = temp_dir("status");
        let service = DictationService::with_engines(
            dir.clone(),
            Arc::new(RecordingRecognizer {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }),
            Arc::new(RecordingSpeaker {
                calls: Mutex::new(Vec::new()),
                audio: vec![],
            }),
            Arc::new(NullTranscriber),
        );
        let status = service.get_status(&json!({})).await;
        assert_eq!(status["provider"], "local");
        assert_eq!(status["available"], false);
        assert_eq!(status["reasonCode"], "models_missing");
        assert_eq!(status["activeModel"], "parakeet-tdt-0.6b-v2-int8");
        let models = status["models"].as_array().unwrap();
        assert_eq!(models.len(), 4);
        assert_eq!(models[0]["id"], "parakeet-tdt-0.6b-v2-int8");
        assert_eq!(models[0]["installed"], false);
        assert_eq!(models[0]["downloading"], false);
        assert_eq!(models[0]["downloadProgress"], serde_json::Value::Null);
        assert_eq!(models[0]["downloadError"], serde_json::Value::Null);
        assert_eq!(models[0]["description"], "NVIDIA Parakeet TDT v2 (English)");
        assert_eq!(status["ttsModels"].as_array().unwrap().len(), 14);

        // The openai-compatible provider is always available.
        let status = service
            .get_status(&json!({ "provider": "openai-compatible" }))
            .await;
        assert_eq!(status["provider"], "openai-compatible");
        assert_eq!(status["available"], true);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：请求的本地模型已安装时状态为 available 并回显 activeModel。
    #[tokio::test]
    async fn status_reports_installed_local_model() {
        let dir = temp_dir("status-installed");
        install_whisper_tiny(&dir);
        let service = DictationService::with_engines(
            dir.clone(),
            Arc::new(RecordingRecognizer {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }),
            Arc::new(RecordingSpeaker {
                calls: Mutex::new(Vec::new()),
                audio: vec![],
            }),
            Arc::new(NullTranscriber),
        );
        let status = service
            .get_status(&json!({ "localModel": "whisper-tiny-int8" }))
            .await;
        assert_eq!(status["available"], true);
        assert_eq!(status["activeModel"], "whisper-tiny-int8");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：语言自动检测把英文模型上的乌克兰语文本切到 piper 模型，
    /// 替换模型无默认 speaker 时丢弃调用方 speaker；英文保持原模型。
    #[tokio::test]
    async fn synthesize_switches_model_for_the_texts_language() {
        let dir = temp_dir("tts-lang");
        // Install both the default English and the Ukrainian models.
        for (dir_name, files) in [
            (
                "kokoro-en-v0_19",
                vec!["model.onnx", "voices.bin", "tokens.txt"],
            ),
            (
                "vits-piper-uk_UA-lada-x_low",
                vec!["uk_UA-lada-x_low.onnx", "tokens.txt"],
            ),
        ] {
            let model_dir = dir.join(dir_name);
            std::fs::create_dir_all(model_dir.join("espeak-ng-data")).unwrap();
            for file in files {
                std::fs::write(model_dir.join(file), b"x").unwrap();
            }
        }
        let speaker = Arc::new(RecordingSpeaker {
            calls: Mutex::new(Vec::new()),
            audio: vec![7, 7, 7],
        });
        let service = DictationService::with_engines(
            dir.clone(),
            Arc::new(RecordingRecognizer {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }),
            Arc::clone(&speaker) as Arc<dyn LocalSpeaker>,
            Arc::new(NullTranscriber),
        );

        let result = service
            .synthesize_speech(SynthesizeOptions {
                text: "Привіт, це перевірка українською мовою для синтезу".to_string(),
                model: Some("kokoro-en-v0_19".to_string()),
                speaker_id: Some(5),
                speed: Some(1.1),
                language: Some("auto".to_string()),
                language_sample: None,
            })
            .await
            .unwrap();
        assert_eq!(result.model_id, "piper-uk_UA-lada-x_low");
        assert_eq!(result.language.as_deref(), Some("uk"));
        assert_eq!(result.audio, vec![7, 7, 7]);
        let calls = speaker.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].model_id, "piper-uk_UA-lada-x_low");
        // The substitute model has no default speaker → the caller's id is
        // dropped (JS assigns `undefined`), letting the engine pick its own.
        assert_eq!(calls[0].speaker_id, None);
        drop(calls);

        // English text keeps the English model.
        let result = service
            .synthesize_speech(SynthesizeOptions {
                text: "Hello there my friend".to_string(),
                model: Some("kokoro-en-v0_19".to_string()),
                speaker_id: Some(5),
                speed: None,
                language: Some("auto".to_string()),
                language_sample: None,
            })
            .await
            .unwrap();
        assert_eq!(result.model_id, "kokoro-en-v0_19");
        assert_eq!(result.language.as_deref(), Some("en"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：中文文本切到 kokoro-multi-lang 模型并以 zh 的默认 speaker 3
    /// 覆盖调用方 speaker。
    #[tokio::test]
    async fn synthesize_switches_to_kokoro_multi_lang_for_chinese() {
        let dir = temp_dir("tts-zh");
        for (dir_name, files) in [
            (
                "kokoro-en-v0_19",
                vec!["model.onnx", "voices.bin", "tokens.txt"],
            ),
            (
                "kokoro-multi-lang-v1_1",
                vec![
                    "model.onnx",
                    "voices.bin",
                    "tokens.txt",
                    "lexicon-us-en.txt",
                    "lexicon-zh.txt",
                ],
            ),
        ] {
            let model_dir = dir.join(dir_name);
            std::fs::create_dir_all(model_dir.join("espeak-ng-data")).unwrap();
            for file in files {
                std::fs::write(model_dir.join(file), b"x").unwrap();
            }
        }
        let speaker = Arc::new(RecordingSpeaker {
            calls: Mutex::new(Vec::new()),
            audio: vec![1],
        });
        let service = DictationService::with_engines(
            dir.clone(),
            Arc::new(RecordingRecognizer {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }),
            Arc::clone(&speaker) as Arc<dyn LocalSpeaker>,
            Arc::new(NullTranscriber),
        );
        let result = service
            .synthesize_speech(SynthesizeOptions {
                text: "修改已经完成，所有测试都通过了。".to_string(),
                model: Some("kokoro-en-v0_19".to_string()),
                speaker_id: Some(0),
                speed: None,
                language: Some("auto".to_string()),
                language_sample: Some("修改已经完成，所有测试都通过了。".to_string()),
            })
            .await
            .unwrap();
        assert_eq!(result.model_id, "kokoro-multi-lang-v1_1");
        assert_eq!(result.language.as_deref(), Some("zh"));
        let calls = speaker.calls.lock().unwrap_or_else(|e| e.into_inner());
        // defaultSpeakerByLanguage zh = 3 replaces the caller's speaker.
        assert_eq!(calls[0].speaker_id, Some(3));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：默认 TTS 模型缺失时合成返回 model_download_in_progress。
    #[tokio::test]
    async fn synthesize_missing_model_reports_download_in_progress() {
        let dir = temp_dir("tts-missing");
        let service = DictationService::with_engines(
            dir.clone(),
            Arc::new(RecordingRecognizer {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }),
            Arc::new(RecordingSpeaker {
                calls: Mutex::new(Vec::new()),
                audio: vec![],
            }),
            Arc::new(NullTranscriber),
        );
        let SynthesizeError::Ready(error) = service
            .synthesize_speech(SynthesizeOptions {
                text: "hello".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err()
        else {
            panic!("expected a readiness error");
        };
        assert_eq!(error.error, "TTS model is downloading");
        assert_eq!(
            error.reason_code.as_deref(),
            Some("model_download_in_progress")
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// 验证：下载/删除管理的 id 校验、下载中禁删、已安装模型下载为
    /// no-op、删除后目录消失。
    #[tokio::test]
    async fn request_and_delete_model_download_management() {
        let dir = temp_dir("manage");
        let service = DictationService::with_engines(
            dir.clone(),
            Arc::new(RecordingRecognizer {
                calls: Mutex::new(Vec::new()),
                fail_with: None,
            }),
            Arc::new(RecordingSpeaker {
                calls: Mutex::new(Vec::new()),
                audio: vec![],
            }),
            Arc::new(NullTranscriber),
        );

        let unknown = service.request_model_download("bogus").await;
        assert_eq!(unknown, json!({ "ok": false, "error": "Unknown model id" }));

        // Deleting while downloading is refused (state forced — a real
        // download would race the assertion).
        service.mark_downloading_for_test("whisper-tiny-int8");
        let downloading = service.delete_model("whisper-tiny-int8").await;
        assert_eq!(
            downloading,
            json!({ "ok": false, "error": "Model is downloading" })
        );

        let unknown_delete = service.delete_model("bogus").await;
        assert_eq!(
            unknown_delete,
            json!({ "ok": false, "error": "Unknown model id" })
        );
        service.clear_download_state_for_test("whisper-tiny-int8");

        install_whisper_tiny(&dir);
        let installed = service.request_model_download("whisper-tiny-int8").await;
        assert_eq!(installed, json!({ "ok": true, "installed": true }));
        let deleted = service.delete_model("whisper-tiny-int8").await;
        assert_eq!(deleted, json!({ "ok": true }));
        assert!(!dir.join("sherpa-onnx-whisper-tiny").exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
