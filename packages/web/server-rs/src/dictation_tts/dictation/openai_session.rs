//! Port of `server/lib/dictation/openai-compatible-session.js` — the
//! pseudo-streaming transcription session for OpenAI-compatible Whisper
//! endpoints (faster-whisper, whisper.cpp, OpenAI, ...).
//!
//! The Whisper HTTP API cannot stream, so audio is buffered per segment and
//! transcribed on `commit()` — everything shorter than a segment is one
//! request on stop, matching how the local session behaves.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;

use super::audio::pcm16_to_wav;
use super::session::{SessionEvent, StreamingTranscriptionSession, random_uuid};
use crate::dictation_tts::tts::stt::{TranscribeParams, Transcriber};

pub const OPENAI_COMPATIBLE_SAMPLE_RATE: u32 = 16000;

/// The session config from the `start` options (`openaiCompatible`).
#[derive(Debug, Clone, Default)]
pub struct OpenAiCompatibleConfig {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub language: Option<String>,
}

pub struct OpenAiCompatibleSession {
    config: OpenAiCompatibleConfig,
    transcriber: Arc<dyn Transcriber>,
    required_sample_rate: u32,
    connected: bool,
    segment_id: String,
    previous_segment_id: Option<String>,
    pcm16: Vec<u8>,
    events: mpsc::UnboundedSender<SessionEvent>,
}

impl OpenAiCompatibleSession {
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

    fn emit(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }
}

impl StreamingTranscriptionSession for OpenAiCompatibleSession {
    fn required_sample_rate(&self) -> u32 {
        self.required_sample_rate
    }

    fn append_pcm16(&mut self, chunk: Vec<u8>) {
        if !self.connected {
            self.emit(SessionEvent::Error {
                message: "STT session not connected".to_string(),
            });
            return;
        }
        self.pcm16.extend_from_slice(&chunk);
    }

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

    fn clear(&mut self) {
        self.pcm16 = Vec::new();
        self.segment_id = random_uuid();
    }

    fn close(&mut self) {
        self.connected = false;
        self.pcm16 = Vec::new();
    }
}

/// Build the session from raw `start.options.openaiCompatible` JSON (the JS
/// reads `config.baseUrl`, `config.model`, `config.apiKey` with plain field
/// access — non-string values become `None`).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dictation_tts::tts::stt::TranscribeFuture;
    use futures::future::BoxFuture;
    use std::sync::Mutex;

    struct FakeTranscriber {
        result: Result<String, String>,
        calls: Mutex<Vec<TranscribeParams>>,
    }

    impl Transcriber for FakeTranscriber {
        fn transcribe(&self, params: TranscribeParams) -> TranscribeFuture {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(params);
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

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

    fn config() -> OpenAiCompatibleConfig {
        OpenAiCompatibleConfig {
            base_url: Some("http://localhost:8880/v1".to_string()),
            model: Some("whisper-tiny".to_string()),
            api_key: None,
            language: Some("en".to_string()),
        }
    }

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
