//! Port of `server/lib/dictation/runtime.js` — the streaming dictation
//! WebSocket endpoint and the HTTP status/model routes.
//!
//! WebSocket protocol (JSON text frames) on `/api/dictation/ws`:
//! - client → server: `start {dictationId, format, options?}`,
//!   `chunk {dictationId, seq, audio}` (base64 PCM16LE),
//!   `finish {dictationId, finalSeq}`, `cancel {dictationId}`, `ping`
//! - server → client: `ready`, `ack {ackSeq}`, `partial {text}`,
//!   `finish_accepted {timeoutMs}`, `final {text}`,
//!   `error {error, retryable, reasonCode?}`, `pong`
//!
//! The endpoint is auth-gated the same way as the terminal WS (the `/api`
//! gate layer covers the upgrade request; the origin check runs here only
//! while a UI password is configured — no origin shortcuts added or
//! skipped). Registration order in the JS (`startup-pipeline-runtime.js`,
//! after `registerAuthAndAccessRoutes`, before the proxy) puts the HTTP
//! routes behind the same gate.

pub mod audio;
pub mod local;
pub mod model_catalog;
pub mod model_downloader;
pub mod openai_session;
pub mod service;
pub mod session;
pub mod stream_manager;

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::context::RouterContext;
use service::{DictationService, SttSessionResolution, SynthesizeError, SynthesizeOptions};
use session::SttSessionOutcome;
use stream_manager::DictationStreamManager;

/// `DICTATION_WS_MAX_PAYLOAD_BYTES`.
const DICTATION_WS_MAX_PAYLOAD_BYTES: usize = 512 * 1024;
/// `DICTATION_WS_HEARTBEAT_INTERVAL_MS`.
const DICTATION_WS_HEARTBEAT_INTERVAL_MS: u64 = 30_000;
/// The route's own `express.json({ limit: '1mb' })` middleware.
const SPEAK_BODY_LIMIT_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
struct DictationState {
    service: Arc<DictationService>,
    /// JS `uiAuthController?.enabled` — governs the upgrade origin check.
    auth_enabled: bool,
}

pub fn router(ctx: RouterContext) -> axum::Router {
    // index.js: `dictationModelsDir = path.join(OMPCHAMBER_USER_CONFIG_ROOT,
    // 'speech-models')`.
    // index.js: `dictationModelsDir = path.join(OMPCHAMBER_USER_CONFIG_ROOT, 'speech-models')`
    // (`data_dir` is this port's stand-in for that root).
    let models_dir = ctx.config.data_dir.join("speech-models");
    let auth_enabled = ctx
        .config
        .ui_password
        .as_deref()
        .map(str::trim)
        .is_some_and(|password| !password.is_empty());
    let state = DictationState {
        service: DictationService::new(models_dir),
        auth_enabled,
    };
    routes()
        .layer(crate::ui_auth::middleware(ctx))
        .with_state(state)
}

fn routes() -> Router<DictationState> {
    Router::new()
        .route("/api/dictation/tts/speak", post(dictation_tts_speak))
        .route("/api/dictation/status", get(dictation_status))
        .route(
            "/api/dictation/models/{modelId}/download",
            post(dictation_model_download),
        )
        .route(
            "/api/dictation/models/{modelId}",
            delete(dictation_model_delete),
        )
        .route("/api/dictation/ws", get(dictation_ws))
}

#[cfg(test)]
pub(crate) fn test_router(state: DictationState) -> Router {
    routes().with_state(state)
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn read_json_body(request: Request) -> Result<Value, Response> {
    let bytes = to_bytes(request.into_body(), SPEAK_BODY_LIMIT_BYTES)
        .await
        .map_err(|_| json_error(StatusCode::PAYLOAD_TOO_LARGE, "Payload too large"))?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Invalid JSON body"))
}

/// `slice(0, 4000)` on the client-provided sample (UTF-16 units in the JS;
/// chars here — identical for BMP text).
fn sample_prefix(value: &str) -> String {
    value.chars().take(4000).collect()
}

// ---------------------------------------------------------------------------
// Local TTS
// ---------------------------------------------------------------------------

/// JS `Number.isInteger`: an integer-valued JSON number.
fn json_integer(value: Option<&Value>) -> Option<i64> {
    let number = value?.as_f64()?;
    if number.fract() != 0.0 || !number.is_finite() || number.abs() >= 9.007_199_254_740_992e15 {
        return None;
    }
    Some(number as i64)
}

async fn dictation_tts_speak(State(state): State<DictationState>, request: Request) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };

    let text = body
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if text.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Text is required");
    }

    let result = state
        .service
        .synthesize_speech(SynthesizeOptions {
            text: text.to_string(),
            model: body
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            speaker_id: json_integer(body.get("speakerId")),
            speed: body.get("speed").and_then(Value::as_f64),
            language: (body.get("language").and_then(Value::as_str) == Some("auto"))
                .then(|| "auto".to_string()),
            language_sample: body
                .get("languageSample")
                .and_then(Value::as_str)
                .map(sample_prefix),
        })
        .await;

    match result {
        Ok(speech) => {
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    if speech.format.is_empty() {
                        "audio/wav"
                    } else {
                        &speech.format
                    },
                )
                .header("X-Speech-Model", &speech.model_id);
            if let Some(language) = speech.language.as_deref() {
                builder = builder.header("X-Speech-Language", language);
            }
            if let Ok(length) = HeaderValue::from_str(&speech.audio.len().to_string()) {
                builder = builder.header(header::CONTENT_LENGTH, length);
            }
            builder
                .body(Body::from(speech.audio))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(SynthesizeError::Ready(error)) => {
            let mut payload = json!({
                "error": error.error,
                "retryable": error.retryable,
            });
            if let Some(reason_code) = error.reason_code {
                payload["reasonCode"] = Value::String(reason_code);
            }
            (StatusCode::SERVICE_UNAVAILABLE, Json(payload)).into_response()
        }
        // The JS route's `catch`: 500 with the thrown message.
        Err(SynthesizeError::Engine(message)) => {
            json_error(StatusCode::INTERNAL_SERVER_ERROR, &message)
        }
    }
}

// ---------------------------------------------------------------------------
// Status + model management
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct StatusQuery {
    provider: Option<String>,
    #[serde(rename = "localModel")]
    local_model: Option<String>,
}

async fn dictation_status(
    State(state): State<DictationState>,
    Query(query): Query<StatusQuery>,
) -> Response {
    let options = json!({
        "provider": query.provider,
        "localModel": query.local_model,
    });
    Json(state.service.get_status(&options).await).into_response()
}

async fn dictation_model_download(
    State(state): State<DictationState>,
    Path(model_id): Path<String>,
) -> Response {
    let result = state.service.request_model_download(&model_id).await;
    if result["ok"] != Value::Bool(true) {
        let error = result["error"].as_str().unwrap_or("Unknown error");
        return json_error(StatusCode::BAD_REQUEST, error);
    }
    Json(result).into_response()
}

async fn dictation_model_delete(
    State(state): State<DictationState>,
    Path(model_id): Path<String>,
) -> Response {
    let result = state.service.delete_model(&model_id).await;
    if result["ok"] != Value::Bool(true) {
        let error = result["error"].as_str().unwrap_or("Unknown error");
        return json_error(StatusCode::BAD_REQUEST, error);
    }
    Json(result).into_response()
}

// ---------------------------------------------------------------------------
// WebSocket transport
// ---------------------------------------------------------------------------

/// JS `upgradeHandler`: only while `uiAuthController.enabled` — the 401
/// session-token check already ran in the gate layer for the upgrade
/// request; the origin check is this module's own responsibility.
fn ws_origin_gate(auth_enabled: bool, parts: &Parts) -> Option<Response> {
    if auth_enabled && !crate::ui_auth::is_request_origin_allowed(parts) {
        return Some(crate::ui_auth::reject_websocket_upgrade(
            403,
            "Invalid origin",
        ));
    }
    None
}

async fn dictation_ws(
    State(state): State<DictationState>,
    ws: WebSocketUpgrade,
    parts: Parts,
) -> Response {
    if let Some(rejection) = ws_origin_gate(state.auth_enabled, &parts) {
        return rejection;
    }
    ws.max_message_size(DICTATION_WS_MAX_PAYLOAD_BYTES)
        .on_upgrade(move |socket| async move { run_dictation_socket(state.service, socket).await })
}

/// One connected client: the shared service, a per-connection stream
/// manager, a ready frame, a heartbeat, and the message pump.
async fn run_dictation_socket(service: Arc<DictationService>, mut socket: WebSocket) {
    let (emit_tx, mut emit_rx) = mpsc::unbounded_channel::<Value>();

    let create_stt_session: session::CreateSttSession = {
        let service = Arc::clone(&service);
        Arc::new(move |options| {
            let service = Arc::clone(&service);
            Box::pin(async move {
                match service.create_stt_session(&options).await {
                    SttSessionResolution::Session { session, events } => {
                        SttSessionOutcome::Session { session, events }
                    }
                    SttSessionResolution::NotReady(error) => SttSessionOutcome::NotReady {
                        error: error.error,
                        retryable: error.retryable,
                        reason_code: error.reason_code,
                    },
                }
            })
        })
    };
    let manager = DictationStreamManager::new(emit_tx.clone(), create_stt_session);

    // The JS sends `ready` immediately on connection.
    if socket
        .send(Message::Text(json!({ "type": "ready" }).to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut heartbeat =
        tokio::time::interval(Duration::from_millis(DICTATION_WS_HEARTBEAT_INTERVAL_MS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heartbeat_armed = false;

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if !heartbeat_armed {
                    // tokio intervals fire immediately once; the JS waits a
                    // full period first.
                    heartbeat_armed = true;
                    continue;
                }
                let _ = socket.send(Message::Ping(Vec::new().into())).await;
            }
            outgoing = emit_rx.recv() => {
                let Some(message) = outgoing else { break };
                if socket
                    .send(Message::Text(message.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            frame = socket.recv() => {
                let Some(Ok(message)) = frame else { break };
                match message {
                    // Binary frames are ignored (`if (isBinary) return`).
                    Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                    Message::Text(text) => {
                        handle_socket_text(&manager, &emit_tx, &text).await;
                    }
                }
            }
        }
    }

    // `close` follows (also after socket errors): perform cleanup.
    manager.cleanup_all();
}

/// One inbound text frame (`socket.on('message')`).
async fn handle_socket_text(
    manager: &Arc<DictationStreamManager>,
    emit_tx: &mpsc::UnboundedSender<Value>,
    text: &str,
) {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if !message.is_object() {
        return;
    }
    let str_field = |name: &str| message.get(name).and_then(Value::as_str);
    match message.get("type").and_then(Value::as_str) {
        Some("start") => {
            let (Some(dictation_id), Some(format)) =
                (str_field("dictationId"), str_field("format"))
            else {
                return;
            };
            let options = if message.get("options").is_some_and(Value::is_object) {
                message["options"].clone()
            } else {
                json!({})
            };
            // `void manager.handleStart(...)`: start runs concurrently with
            // subsequent chunks (which fail with "Dictation stream not
            // started" until it lands — same as the JS).
            let manager = Arc::clone(manager);
            let dictation_id = dictation_id.to_string();
            let format = format.to_string();
            tokio::spawn(async move {
                manager.handle_start(dictation_id, format, options).await;
            });
        }
        Some("chunk") => {
            let (Some(dictation_id), Some(audio)) = (str_field("dictationId"), str_field("audio"))
            else {
                return;
            };
            let Some(seq) = message.get("seq").and_then(Value::as_f64) else {
                return;
            };
            // `handleChunk`'s `Number.isInteger(seq) && seq >= 0`.
            if !seq.is_finite() || seq.fract() != 0.0 || seq < 0.0 {
                return;
            }
            manager.handle_chunk(dictation_id, seq as i64, audio);
        }
        Some("finish") => {
            let Some(dictation_id) = str_field("dictationId") else {
                return;
            };
            // `typeof message.finalSeq !== 'number'` — non-integers pass.
            let Some(final_seq) = message.get("finalSeq").and_then(Value::as_f64) else {
                return;
            };
            manager.handle_finish(dictation_id, final_seq);
        }
        Some("cancel") => {
            if let Some(dictation_id) = str_field("dictationId") {
                manager.handle_cancel(dictation_id);
            }
        }
        Some("ping") => {
            let _ = emit_tx.send(json!({ "type": "pong" }));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use futures::future::BoxFuture;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tower::ServiceExt;

    use crate::dictation_tts::tts::stt::{TranscribeFuture, TranscribeParams, Transcriber};
    use service::ReadyError;
    use session::StreamingTranscriptionSession;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-dictation-routes-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    struct NullTranscriber;
    impl Transcriber for NullTranscriber {
        fn transcribe(&self, _params: TranscribeParams) -> TranscribeFuture {
            Box::pin(async { Ok(String::new()) })
        }
    }

    struct RecordingSpeaker {
        calls: Mutex<Vec<local::LocalSpeechParams>>,
        audio: Vec<u8>,
    }

    impl local::LocalSpeaker for RecordingSpeaker {
        fn synthesize(
            &self,
            params: local::LocalSpeechParams,
        ) -> BoxFuture<'static, Result<local::LocalSpeechOutput, String>> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(params);
            let audio = self.audio.clone();
            Box::pin(async move {
                Ok(local::LocalSpeechOutput {
                    audio,
                    format: "audio/wav".to_string(),
                })
            })
        }
    }

    struct NullRecognizer;
    impl local::LocalRecognizer for NullRecognizer {
        fn create_session(
            &self,
            _models_dir: &std::path::Path,
            _model_id: &str,
        ) -> BoxFuture<
            'static,
            Result<
                (
                    Box<dyn StreamingTranscriptionSession>,
                    mpsc::UnboundedReceiver<session::SessionEvent>,
                ),
                String,
            >,
        > {
            Box::pin(async {
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
                let (tx, rx) = mpsc::unbounded_channel();
                Ok((
                    Box::new(NullSession) as Box<dyn StreamingTranscriptionSession>,
                    rx,
                ))
            })
        }
    }

    fn install_kokoro_english(models_dir: &std::path::Path) {
        let model_dir = models_dir.join("kokoro-en-v0_19");
        std::fs::create_dir_all(model_dir.join("espeak-ng-data")).unwrap();
        for file in ["model.onnx", "voices.bin", "tokens.txt"] {
            std::fs::write(model_dir.join(file), b"x").unwrap();
        }
    }

    fn fixture(models_dir: PathBuf, speaker_audio: Vec<u8>) -> (Router, Arc<RecordingSpeaker>) {
        let speaker = Arc::new(RecordingSpeaker {
            calls: Mutex::new(Vec::new()),
            audio: speaker_audio,
        });
        let state = DictationState {
            service: DictationService::with_engines(
                models_dir,
                Arc::new(NullRecognizer),
                Arc::clone(&speaker) as Arc<dyn local::LocalSpeaker>,
                Arc::new(NullTranscriber),
            ),
            auth_enabled: false,
        };
        (test_router(state), speaker)
    }

    async fn call(
        router: &Router,
        request: Request<Body>,
    ) -> (StatusCode, Value, Vec<u8>, axum::http::HeaderMap) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 16 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        let value = if headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"))
        {
            serde_json::from_slice(&body).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        (status, value, body, headers)
    }

    fn post_json(path: &str, body: Value) -> Request<Body> {
        HttpRequest::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn speak_requires_text() {
        let (router, _) = fixture(temp_dir("speak-400"), vec![]);
        let (status, body, _, _) = call(
            &router,
            post_json("/api/dictation/tts/speak", json!({ "text": "   " })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Text is required");
    }

    #[tokio::test]
    async fn speak_streams_wav_bytes_with_model_headers() {
        let dir = temp_dir("speak-200");
        install_kokoro_english(&dir);
        let (router, speaker) = fixture(dir.clone(), vec![9, 9]);
        let (status, body, raw, headers) = call(
            &router,
            post_json(
                "/api/dictation/tts/speak",
                json!({ "text": "hello world", "speakerId": 2, "speed": 1.2 }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(raw, vec![9, 9]);
        assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "audio/wav");
        assert_eq!(headers.get("X-Speech-Model").unwrap(), "kokoro-en-v0_19");
        assert!(headers.get("X-Speech-Language").is_none());
        let calls = speaker.calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].speaker_id, Some(2));
        assert_eq!(calls[0].speed, Some(1.2));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn speak_reports_language_when_auto_detected() {
        let dir = temp_dir("speak-lang");
        install_kokoro_english(&dir);
        // Also install the Ukrainian model so the switch lands.
        let uk = dir.join("vits-piper-uk_UA-lada-x_low");
        std::fs::create_dir_all(uk.join("espeak-ng-data")).unwrap();
        for file in ["uk_UA-lada-x_low.onnx", "tokens.txt"] {
            std::fs::write(uk.join(file), b"x").unwrap();
        }
        let (router, _) = fixture(dir.clone(), vec![1]);
        let (status, _, _, headers) = call(
            &router,
            post_json(
                "/api/dictation/tts/speak",
                json!({
                    "text": "Привіт, це тест",
                    "language": "auto",
                    "languageSample": "Привіт! Це відповідь українською мовою."
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("X-Speech-Model").unwrap(),
            "piper-uk_UA-lada-x_low"
        );
        assert_eq!(headers.get("X-Speech-Language").unwrap(), "uk");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn speak_answers_503_while_model_downloading() {
        let (router, _) = fixture(temp_dir("speak-503"), vec![]);
        let (status, body, _, _) = call(
            &router,
            post_json("/api/dictation/tts/speak", json!({ "text": "hi" })),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "TTS model is downloading");
        assert_eq!(body["retryable"], true);
        assert_eq!(body["reasonCode"], "model_download_in_progress");
    }

    #[tokio::test]
    async fn speak_answers_500_when_the_local_engine_throws() {
        // Installed model + Unavailable engine → the JS route's catch.
        // (with_engines keeps this test independent of the local-speech
        // feature, which swaps the default engines for the native ones.)
        let dir = temp_dir("speak-500");
        install_kokoro_english(&dir);
        let service = DictationService::with_engines(
            dir.clone(),
            std::sync::Arc::new(super::local::UnavailableLocalEngine),
            std::sync::Arc::new(super::local::UnavailableLocalEngine),
            std::sync::Arc::new(crate::dictation_tts::tts::stt::HttpTranscriber::default()),
        );
        let state = DictationState {
            service,
            auth_enabled: false,
        };
        let router = test_router(state);
        let (status, body, _, _) = call(
            &router,
            post_json("/api/dictation/tts/speak", json!({ "text": "hi" })),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body["error"],
            "Local speech engine is unavailable in this server build (sherpa-onnx native binding not linked)"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn status_route_reports_models_missing_shape() {
        let (router, _) = fixture(temp_dir("status"), vec![]);
        let request = HttpRequest::get("/api/dictation/status")
            .body(Body::empty())
            .unwrap();
        let (status, body, _, _) = call(&router, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["provider"], "local");
        assert_eq!(body["available"], false);
        assert_eq!(body["reasonCode"], "models_missing");
        assert_eq!(body["activeModel"], "parakeet-tdt-0.6b-v2-int8");
        assert_eq!(body["models"].as_array().unwrap().len(), 4);
        assert_eq!(body["ttsModels"].as_array().unwrap().len(), 14);
    }

    #[tokio::test]
    async fn status_route_forwards_provider_query() {
        let (router, _) = fixture(temp_dir("status-compat"), vec![]);
        let request = HttpRequest::get("/api/dictation/status?provider=openai-compatible")
            .body(Body::empty())
            .unwrap();
        let (_, body, _, _) = call(&router, request).await;
        assert_eq!(body["provider"], "openai-compatible");
        assert_eq!(body["available"], true);
    }

    #[tokio::test]
    async fn model_routes_validate_ids() {
        let dir = temp_dir("models");
        install_kokoro_english(&dir);
        let (router, _) = fixture(dir.clone(), vec![]);

        let request = HttpRequest::post("/api/dictation/models/bogus/download")
            .body(Body::empty())
            .unwrap();
        let (status, body, _, _) = call(&router, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Unknown model id");

        let request = HttpRequest::delete("/api/dictation/models/bogus")
            .body(Body::empty())
            .unwrap();
        let (status, body, _, _) = call(&router, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Unknown model id");

        // An installed model downloads as a no-op and deletes cleanly.
        let request = HttpRequest::post("/api/dictation/models/kokoro-en-v0_19/download")
            .body(Body::empty())
            .unwrap();
        let (status, body, _, _) = call(&router, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true, "installed": true }));

        let request = HttpRequest::delete("/api/dictation/models/kokoro-en-v0_19")
            .body(Body::empty())
            .unwrap();
        let (status, body, _, _) = call(&router, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "ok": true }));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn ws_endpoint_rejects_plain_gets() {
        // The JS registers no HTTP route on the WS path; axum's upgrade
        // extractor answers non-upgrade requests with an error status.
        let (router, _) = fixture(temp_dir("ws-plain"), vec![]);
        let request = HttpRequest::get("/api/dictation/ws")
            .body(Body::empty())
            .unwrap();
        let (status, _, _, _) = call(&router, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    fn origin_parts(origin: &str, host: &str) -> Parts {
        let request = HttpRequest::get("/api/dictation/ws")
            .header(header::ORIGIN, origin)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap();
        let (parts, _) = request.into_parts();
        parts
    }

    #[test]
    fn ws_origin_gate_rejects_foreign_origins_only_while_auth_enabled() {
        // While UI auth is configured, a foreign origin is rejected with the
        // JS upgrade-error frame.
        let rejection = ws_origin_gate(
            true,
            &origin_parts("https://evil.example.com", "localhost:4200"),
        );
        let response = rejection.expect("expected rejection");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(response.headers().get(header::CONNECTION).unwrap(), "close");

        // Same-origin and packaged-client origins pass the gate.
        assert!(
            ws_origin_gate(
                true,
                &origin_parts("http://localhost:4200", "localhost:4200")
            )
            .is_none()
        );
        assert!(
            ws_origin_gate(true, &origin_parts("ompchamber-ui://app", "localhost:4200")).is_none()
        );

        // Without UI auth the JS performs no origin check at all.
        assert!(
            ws_origin_gate(
                false,
                &origin_parts("https://evil.example.com", "localhost:4200")
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn ready_error_serializes_the_ready_shape() {
        // Compile-time shape check for the route-facing error type.
        let error = ReadyError {
            error: "Dictation model is downloading".to_string(),
            retryable: true,
            reason_code: Some("model_download_in_progress".to_string()),
        };
        let mut payload = json!({ "error": error.error, "retryable": error.retryable });
        payload["reasonCode"] = Value::String(error.reason_code.unwrap());
        assert_eq!(payload["reasonCode"], "model_download_in_progress");
    }
}
