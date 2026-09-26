//! Port of `server/lib/tts/service.js` — server-side speech generation
//! against OpenAI (or any OpenAI-compatible server the caller supplies).
//!
//! The JS uses the OpenAI Node SDK with a cached client keyed on the active
//! API key; here the key resolution (env → OpenCode auth file) is preserved
//! behind an injectable source and the HTTP call goes through a provider
//! seam so route tests can fake the network.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use super::base_url::normalize_custom_openai_base_url;

/// `TTS_VOICES`.
pub const TTS_VOICES: [&str; 13] = [
    "alloy", "ash", "ballad", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer", "verse",
    "marin", "cedar",
];

/// `getOpenAIApiKey` seam: the production source reads `OPENAI_API_KEY`
/// then the OpenCode auth file (`auth.openai`/`auth.codex`/`auth.chatgpt`,
/// string form or `{access, token}` object form).
pub type ApiKeySource = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// One `generateSpeechStream` request. `voice`/`model`/`speed`/
/// `instructions` keep the raw JSON value because JS destructuring defaults
/// apply to `undefined` only and the values are forwarded verbatim.
#[derive(Debug, Clone, Default)]
pub struct GenerateOptions {
    pub text: String,
    pub voice: Option<Value>,
    pub model: Option<Value>,
    pub speed: Option<Value>,
    pub instructions: Option<Value>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpeechOutput {
    pub buffer: Vec<u8>,
    pub content_type: &'static str,
}

pub type SpeechFuture = BoxFuture<'static, Result<SpeechOutput, String>>;

/// Injectable `client.audio.speech.create`.
pub trait SpeechProvider: Send + Sync {
    fn generate_speech(&self, request: SpeechRequest) -> SpeechFuture;
}

/// The POST `{baseURL}/audio/speech` request the OpenAI SDK issues.
#[derive(Debug, Clone)]
pub struct SpeechRequest {
    /// `https://api.openai.com/v1` unless a normalized custom URL applies.
    pub endpoint: String,
    /// `apiKey`, or the literal `not-required` placeholder the JS sends for
    /// custom servers used without a key.
    pub api_key: String,
    pub body: Value,
}

/// Production provider over reqwest.
pub struct HttpSpeechProvider {
    pub http: reqwest::Client,
}

impl Default for HttpSpeechProvider {
    fn default() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl SpeechProvider for HttpSpeechProvider {
    fn generate_speech(&self, request: SpeechRequest) -> SpeechFuture {
        let http = self.http.clone();
        Box::pin(async move {
            let response = http
                .post(request.endpoint)
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", request.api_key),
                )
                .json(&request.body)
                .send()
                .await
                .map_err(|error| format!("Failed to generate speech: {error}"))?;
            let status = response.status();
            if !status.is_success() {
                // Approximates the SDK's `APIError.message`
                // (`${status} ${statusText}`, plus the body's message when
                // present).
                let body = response.bytes().await.unwrap_or_default();
                let detail = serde_json::from_slice::<Value>(&body)
                    .ok()
                    .and_then(|parsed| {
                        parsed
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    });
                return Err(match detail {
                    Some(detail) => format!("Failed to generate speech: {status} {detail}"),
                    None => format!("Failed to generate speech: {status}"),
                });
            }
            let buffer = response
                .bytes()
                .await
                .map_err(|error| format!("Failed to generate speech: {error}"))?
                .to_vec();
            Ok(SpeechOutput {
                buffer,
                content_type: "audio/mpeg",
            })
        })
    }
}

/// `getOpenAIApiKey` (env first, then the OpenCode auth file).
pub fn resolve_configured_api_key() -> Option<String> {
    if let Ok(env_key) = std::env::var("OPENAI_API_KEY")
        && !env_key.is_empty()
    {
        return Some(env_key);
    }
    read_api_key_from_auth_file()
}

fn read_api_key_from_auth_file() -> Option<String> {
    use crate::small_model::auth_store::{AuthStore, FsAuthStore};
    let store = FsAuthStore::default();
    let auth = match store.read() {
        Ok(auth) => auth,
        Err(message) => {
            tracing::warn!("[TTSService] Failed to read auth file: {message}");
            return None;
        }
    };
    // auth.openai || auth.codex || auth.chatgpt (JS truthiness: absent,
    // null, false, 0, and '' all fall through).
    for provider in ["openai", "codex", "chatgpt"] {
        let entry = auth.get(provider);
        let truthy = match &entry {
            None | Some(Value::Null) => false,
            Some(Value::String(value)) => !value.is_empty(),
            Some(Value::Bool(value)) => *value,
            Some(Value::Number(value)) => value.as_f64() != Some(0.0),
            Some(_) => true,
        };
        if !truthy {
            continue;
        }
        match entry {
            // Handle both string format (just the token) and object format.
            Some(Value::String(token)) => return Some(token.clone()),
            Some(Value::Object(object)) => {
                // Try access token first (OAuth), then regular token.
                if let Some(Value::String(access)) = object.get("access") {
                    return Some(access.clone());
                }
                if let Some(Value::String(token)) = object.get("token") {
                    return Some(token.clone());
                }
            }
            _ => {}
        }
    }
    None
}

/// `TTSService`.
pub struct TtsService {
    provider: Arc<dyn SpeechProvider>,
    key_source: ApiKeySource,
}

impl TtsService {
    pub fn new(provider: Arc<dyn SpeechProvider>, key_source: ApiKeySource) -> Self {
        Self {
            provider,
            key_source,
        }
    }

    /// Production wiring: real HTTP provider, env + auth-file key source.
    pub fn production() -> Arc<Self> {
        Arc::new(Self::new(
            Arc::new(HttpSpeechProvider::default()),
            Arc::new(resolve_configured_api_key),
        ))
    }

    /// `isAvailable`: an OpenAI key is configured on the server.
    pub fn is_available(&self) -> bool {
        (self.key_source)().is_some()
    }

    /// `generateSpeechStream`.
    pub async fn generate_speech_stream(
        &self,
        options: GenerateOptions,
    ) -> Result<SpeechOutput, String> {
        let normalized_base_url =
            match normalize_custom_openai_base_url(options.base_url.as_deref()) {
                Ok(value) => value,
                Err(error) => return Err(error),
            };

        // Use provided API key / baseURL or fall back to configured key.
        let (api_key, endpoint) = if normalized_base_url.is_some() || options.api_key.is_some() {
            // JS truthiness: an empty (or absent) key falls to the
            // `not-required` placeholder on the custom-client path.
            let key = match options.api_key.as_deref() {
                Some(key) if !key.is_empty() => key.to_string(),
                _ => "not-required".to_string(),
            };
            let endpoint = normalized_base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            (key, endpoint)
        } else {
            let Some(configured) = (self.key_source)() else {
                return Err("TTS service not available. Configure OpenAI in OpenCode, provide an API key, or set a custom server URL in settings.".to_string());
            };
            (configured, "https://api.openai.com/v1".to_string())
        };

        if options.text.trim().is_empty() {
            return Err("Text is required for TTS".to_string());
        }

        let voice = options
            .voice
            .clone()
            .unwrap_or_else(|| Value::String("coral".to_string()));
        let model = options
            .model
            .clone()
            .unwrap_or_else(|| Value::String("gpt-4o-mini-tts".to_string()));
        let speed = options
            .speed
            .clone()
            .unwrap_or_else(|| Value::Number(serde_json::Number::from_f64(1.0).unwrap()));

        let mut body = Map::new();
        body.insert("model".to_string(), model);
        body.insert("voice".to_string(), voice);
        body.insert("input".to_string(), Value::String(options.text.clone()));
        body.insert("speed".to_string(), speed);
        if normalized_base_url.is_none() {
            // OpenAI-compatible servers (custom baseURL) may not support
            // `instructions` or `response_format`, but do support `speed`.
            // Only the OpenAI endpoint gets the full parameter set.
            if let Some(instructions) = options.instructions.clone() {
                let truthy = match &instructions {
                    Value::String(value) => !value.is_empty(),
                    Value::Null | Value::Bool(false) => false,
                    _ => true,
                };
                if truthy {
                    body.insert("instructions".to_string(), instructions);
                }
            }
            body.insert(
                "response_format".to_string(),
                Value::String("mp3".to_string()),
            );
        }

        self.provider
            .generate_speech(SpeechRequest {
                endpoint: format!("{endpoint}/audio/speech"),
                api_key,
                body: Value::Object(body),
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeProvider {
        requests: Mutex<Vec<SpeechRequest>>,
        result: Result<Vec<u8>, String>,
    }

    impl SpeechProvider for FakeProvider {
        fn generate_speech(&self, request: SpeechRequest) -> SpeechFuture {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request.clone());
            let result = self.result.clone();
            Box::pin(async move {
                result.map(|buffer| SpeechOutput {
                    buffer,
                    content_type: "audio/mpeg",
                })
            })
        }
    }

    struct Fixture {
        service: TtsService,
        provider: Arc<FakeProvider>,
    }

    fn fixture(result: Result<Vec<u8>, String>, key: Option<&str>) -> Fixture {
        let provider = Arc::new(FakeProvider {
            requests: Mutex::new(Vec::new()),
            result,
        });
        let key = key.map(str::to_string);
        Fixture {
            service: TtsService::new(
                Arc::clone(&provider) as Arc<dyn SpeechProvider>,
                Arc::new(move || key.clone()),
            ),
            provider,
        }
    }

    fn default_options() -> GenerateOptions {
        GenerateOptions {
            text: "hello".to_string(),
            ..Default::default()
        }
    }

    fn captured(fixture: &Fixture) -> SpeechRequest {
        fixture
            .provider
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap()
    }

    #[test]
    fn availability_follows_the_key_source() {
        assert!(fixture(Ok(vec![]), Some("k")).service.is_available());
        assert!(!fixture(Ok(vec![]), None).service.is_available());
    }

    #[tokio::test]
    async fn errors_without_any_key_or_custom_server() {
        let error = fixture(Ok(vec![]), None)
            .service
            .generate_speech_stream(default_options())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "TTS service not available. Configure OpenAI in OpenCode, provide an API key, or set a custom server URL in settings."
        );
    }

    #[tokio::test]
    async fn rejects_blank_text() {
        let error = fixture(Ok(vec![]), Some("k"))
            .service
            .generate_speech_stream(GenerateOptions {
                text: "   ".to_string(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(error, "Text is required for TTS");
    }

    #[tokio::test]
    async fn configured_key_targets_openai_with_full_params() {
        let fixture = fixture(Ok(vec![1, 2, 3]), Some("server-key"));
        let output = fixture
            .service
            .generate_speech_stream(GenerateOptions {
                instructions: Some(Value::String("cheerful".to_string())),
                ..default_options()
            })
            .await
            .unwrap();
        assert_eq!(output.buffer, vec![1, 2, 3]);
        assert_eq!(output.content_type, "audio/mpeg");
        let request = captured(&fixture);
        assert_eq!(request.endpoint, "https://api.openai.com/v1/audio/speech");
        assert_eq!(request.api_key, "server-key");
        assert_eq!(request.body["voice"], "coral");
        assert_eq!(request.body["model"], "gpt-4o-mini-tts");
        assert_eq!(request.body["speed"], 1.0);
        assert_eq!(request.body["input"], "hello");
        assert_eq!(request.body["instructions"], "cheerful");
        assert_eq!(request.body["response_format"], "mp3");
    }

    #[tokio::test]
    async fn custom_base_url_sends_the_safe_subset() {
        let fixture = fixture(Ok(vec![]), None);
        fixture
            .service
            .generate_speech_stream(GenerateOptions {
                base_url: Some("http://localhost:8880/v1".to_string()),
                instructions: Some(Value::String("cheerful".to_string())),
                ..default_options()
            })
            .await
            .unwrap();
        let request = captured(&fixture);
        assert_eq!(request.endpoint, "http://localhost:8880/v1/audio/speech");
        assert_eq!(request.api_key, "not-required");
        assert!(request.body.get("instructions").is_none());
        assert!(request.body.get("response_format").is_none());
        assert!(request.body.get("speed").is_some());
    }

    #[tokio::test]
    async fn provider_errors_pass_through() {
        let error = fixture(Err("boom".to_string()), Some("k"))
            .service
            .generate_speech_stream(default_options())
            .await
            .unwrap_err();
        assert_eq!(error, "boom");
    }
}
