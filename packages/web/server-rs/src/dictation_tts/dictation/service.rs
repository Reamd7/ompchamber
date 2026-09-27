//! Port of `server/lib/dictation/service.js` — provider resolution and
//! readiness. Providers:
//! - `local` (default): sherpa-onnx in a worker process in the JS; here the
//!   [`LocalRecognizer`]/[`LocalSpeaker`] seams default to the documented
//!   Unavailable engine (models still download, install-check, and report).
//! - `openai-compatible`: buffered per-segment transcription against any
//!   OpenAI-compatible endpoint.

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
#[derive(Default)]
struct Downloads {
    states: HashMap<String, &'static str>,
    errors: HashMap<String, String>,
    progress: HashMap<String, Option<u64>>,
    running: HashMap<String, tokio::task::JoinHandle<()>>,
}

impl Downloads {
    fn is_downloading(&self, model_id: &str) -> bool {
        self.states.get(model_id).copied() == Some("downloading")
    }

    fn is_error(&self, model_id: &str) -> bool {
        self.states.get(model_id).copied() == Some("error")
    }
}

/// The readiness error shape shared by `createSttSession` and
/// `synthesizeSpeech`.
#[derive(Debug, Clone)]
pub struct ReadyError {
    pub error: String,
    pub retryable: bool,
    pub reason_code: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SynthesizeSpeech {
    pub audio: Vec<u8>,
    pub format: String,
    pub model_id: String,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SynthesizeOptions {
    pub text: String,
    pub model: Option<String>,
    pub speaker_id: Option<i64>,
    pub speed: Option<f64>,
    pub language: Option<String>,
    pub language_sample: Option<String>,
}

/// `synthesizeSpeech` failures: readiness errors answer 503 with a reason
/// code; an engine throw answers 500 with the raw message (the JS route's
/// `catch`).
#[derive(Debug, Clone)]
pub enum SynthesizeError {
    Ready(ReadyError),
    Engine(String),
}

pub enum SttSessionResolution {
    Session {
        session: Box<dyn StreamingTranscriptionSession>,
        events: mpsc::UnboundedReceiver<SessionEvent>,
    },
    NotReady(ReadyError),
}

fn resolve_local_model_id(requested: Option<&str>) -> String {
    match requested {
        Some(id) if is_local_stt_model_id(id) => id.to_string(),
        _ => DEFAULT_LOCAL_STT_MODEL.to_string(),
    }
}

pub struct DictationService {
    models_dir: PathBuf,
    recognizer: Arc<dyn LocalRecognizer>,
    speaker: Arc<dyn LocalSpeaker>,
    transcriber: Arc<dyn Transcriber>,
    downloads: Mutex<Downloads>,
}

impl DictationService {
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

#[cfg(test)]
impl DictationService {
    /// Force the `'downloading'` state without spawning a real download
    /// (test-only; the JS suite reaches this state with mocked fetches).
    pub(crate) fn mark_downloading_for_test(&self, model_id: &str) {
        self.downloads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .downloading_marker(model_id);
    }

    pub(crate) fn clear_download_state_for_test(&self, model_id: &str) {
        let mut downloads = self.downloads.lock().unwrap_or_else(|e| e.into_inner());
        downloads.states.remove(model_id);
        downloads.progress.remove(model_id);
    }
}

impl Downloads {
    fn downloading_marker(&mut self, model_id: &str) {
        self.states.insert(model_id.to_string(), "downloading");
        self.errors.remove(model_id);
        self.progress.insert(model_id.to_string(), Some(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation_tts::dictation::local::LocalSpeechOutput;
    use crate::dictation_tts::tts::stt::TranscribeFuture;
    use futures::future::BoxFuture;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-dictation-svc-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    struct NullTranscriber;
    impl Transcriber for NullTranscriber {
        fn transcribe(
            &self,
            _params: crate::dictation_tts::tts::stt::TranscribeParams,
        ) -> TranscribeFuture {
            Box::pin(async { Ok(String::new()) })
        }
    }

    struct RecordingRecognizer {
        calls: Mutex<Vec<String>>,
        fail_with: Option<String>,
    }

    impl LocalRecognizer for RecordingRecognizer {
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
                struct NullSession;
                impl StreamingTranscriptionSession for NullSession {
                    fn required_sample_rate(&self) -> u32 {
                        16000
                    }
                    fn append_pcm16(&mut self, _chunk: Vec<u8>) {}
                    fn commit(&mut self) {}
                    fn clear(&mut self) {}
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

    struct RecordingSpeaker {
        calls: Mutex<Vec<LocalSpeechParams>>,
        audio: Vec<u8>,
    }

    impl LocalSpeaker for RecordingSpeaker {
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
