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

use std::path::Path;

use futures::future::BoxFuture;
use tokio::sync::mpsc;

use super::session::{SessionEvent, StreamingTranscriptionSession};

/// Error surfaced by the unavailable engine. The JS equivalent (worker
/// fails to load the sherpa addon) propagates through
/// `WorkerBackedTranscriptionSession.connect()` and is mapped by the service
/// exactly like any other connect failure.
pub const UNAVAILABLE_MESSAGE: &str = "Local speech engine is unavailable in this server build (sherpa-onnx native binding not linked)";

/// A connected session plus its event stream.
pub type LocalSession = (
    Box<dyn StreamingTranscriptionSession>,
    mpsc::UnboundedReceiver<SessionEvent>,
);

/// `DictationWorkerClient.createSession` seam: resolve a connected streaming
/// session for one local model.
pub trait LocalRecognizer: Send + Sync {
    fn create_session(
        &self,
        models_dir: &Path,
        model_id: &str,
    ) -> BoxFuture<'static, Result<LocalSession, String>>;
}

/// `DictationWorkerClient.synthesizeSpeech` seam: synthesize speech with the
/// local TTS model, returning WAV bytes.
pub trait LocalSpeaker: Send + Sync {
    fn synthesize(
        &self,
        params: LocalSpeechParams,
    ) -> BoxFuture<'static, Result<LocalSpeechOutput, String>>;
}

#[derive(Debug, Clone)]
pub struct LocalSpeechParams {
    pub models_dir: std::path::PathBuf,
    pub model_id: String,
    pub text: String,
    pub speaker_id: Option<i64>,
    pub speed: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct LocalSpeechOutput {
    pub audio: Vec<u8>,
    pub format: String,
}

/// Default engine: every native operation is unavailable. Wiring point for a
/// future `sherpa-onnx` Rust binding.
#[derive(Debug, Clone, Default)]
pub struct UnavailableLocalEngine;

impl LocalRecognizer for UnavailableLocalEngine {
    fn create_session(
        &self,
        _models_dir: &Path,
        _model_id: &str,
    ) -> BoxFuture<'static, Result<LocalSession, String>> {
        Box::pin(async { Err(UNAVAILABLE_MESSAGE.to_string()) })
    }
}

impl LocalSpeaker for UnavailableLocalEngine {
    fn synthesize(
        &self,
        _params: LocalSpeechParams,
    ) -> BoxFuture<'static, Result<LocalSpeechOutput, String>> {
        Box::pin(async { Err(UNAVAILABLE_MESSAGE.to_string()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
