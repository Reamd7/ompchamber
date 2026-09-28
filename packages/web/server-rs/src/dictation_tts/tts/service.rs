//! Port of `server/lib/tts/service.js` — server-side speech generation
//! against OpenAI (or any OpenAI-compatible server the caller supplies).
//!
//! The JS uses the OpenAI Node SDK with a cached client keyed on the active
//! API key; here the key resolution (env → OpenCode auth file) is preserved
//! behind an injectable source and the HTTP call goes through a provider
//! seam so route tests can fake the network.
//!
//! 中文说明：移植自 `server/lib/tts/service.js`——服务端对接 OpenAI（或调用方
//! 指定的任意 OpenAI 兼容服务器）的语音合成。JS 版用 OpenAI Node SDK 并按
//! API key 缓存 client；此处保留 key 解析顺序（env → OpenCode auth 文件）并
//! 把它抽象为可注入来源，HTTP 调用走 provider 接缝，路由测试即可伪造网络。

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::{Map, Value};

use super::base_url::normalize_custom_openai_base_url;

/// `TTS_VOICES`.
/// 顺序与 JS `TTS_VOICES` 一致，status 路由原样返回。
pub const TTS_VOICES: [&str; 13] = [
    "alloy", "ash", "ballad", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer", "verse",
    "marin", "cedar",
];

/// `getOpenAIApiKey` seam: the production source reads `OPENAI_API_KEY`
/// then the OpenCode auth file (`auth.openai`/`auth.codex`/`auth.chatgpt`,
/// string form or `{access, token}` object form).
/// 每次调用现取，便于测试注入固定返回值。
pub type ApiKeySource = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// One `generateSpeechStream` request. `voice`/`model`/`speed`/
/// `instructions` keep the raw JSON value because JS destructuring defaults
/// apply to `undefined` only and the values are forwarded verbatim.
///
/// `text` 是唯一必填项（服务层校验非空）；其余字段保持原始 JSON 形态转发。
#[derive(Debug, Clone, Default)]
pub struct GenerateOptions {
    /// 要合成的文本（服务层要求 trim 后非空）。
    pub text: String,
    /// voice 名；`None` 时服务层用默认值 "coral"。
    pub voice: Option<Value>,
    /// 模型名；`None` 时服务层用默认值 "gpt-4o-mini-tts"。
    pub model: Option<Value>,
    /// 语速；`None` 时服务层用默认值 1.0。
    pub speed: Option<Value>,
    /// 风格指令；仅官方 OpenAI 端点转发，且需通过 JS 真值判定。
    pub instructions: Option<Value>,
    /// 客户端提供的 API key；空串按未提供处理（走 "not-required" 占位）。
    pub api_key: Option<String>,
    /// 自定义 OpenAI 兼容 base URL；先经 `normalize_custom_openai_base_url`
    /// 校验（远程主机受安全策略限制）。
    pub base_url: Option<String>,
}

/// provider 成功时的合成结果。
#[derive(Debug, Clone)]
pub struct SpeechOutput {
    /// 完整音频字节（OpenAI 端点为 mp3 流）。
    pub buffer: Vec<u8>,
    /// 随字节一起返回的 Content-Type。
    pub content_type: &'static str,
}

/// 错误以字符串消息形式返回（与 JS 抛错的 message 对应）。
pub type SpeechFuture = BoxFuture<'static, Result<SpeechOutput, String>>;

/// Injectable `client.audio.speech.create`.
/// trait 化使路由测试可以注入 fake，不必真实联网。
pub trait SpeechProvider: Send + Sync {
    /// 发起一次合成请求；future 由实现方 box 好。
    fn generate_speech(&self, request: SpeechRequest) -> SpeechFuture;
}

/// The POST `{baseURL}/audio/speech` request the OpenAI SDK issues.
/// 三个字段与 OpenAI SDK 实际发出的请求一一对应。
#[derive(Debug, Clone)]
pub struct SpeechRequest {
    /// `https://api.openai.com/v1` unless a normalized custom URL applies.
    /// 形如 `https://api.openai.com/v1/audio/speech` 或自定义服务器等价路径。
    pub endpoint: String,
    /// `apiKey`, or the literal `not-required` placeholder the JS sends for
    /// custom servers used without a key.
    /// 自定义服务器无 key 时为占位符，仅作 Bearer 头的形式值。
    pub api_key: String,
    /// 发给 `/audio/speech` 的 JSON 请求体。
    pub body: Value,
}

/// Production provider over reqwest.
/// 唯一字段 `http` 可注入定制过的 client（如带代理）。
pub struct HttpSpeechProvider {
    /// 实际发请求的 reqwest client（clone 开销很小）。
    pub http: reqwest::Client,
}

/// 默认构造一个全新 reqwest client。
impl Default for HttpSpeechProvider {
    /// Default trait 实现：新建默认 reqwest client。
    fn default() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

/// 用 Bearer key POST 到 `endpoint`；非 2xx 时尽量从响应体取 `error.message`
/// 拼进错误文案，模拟 SDK `APIError.message` 的格式。
impl SpeechProvider for HttpSpeechProvider {
    /// 执行 HTTP 请求并把整个响应体读成音频字节。
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
/// env 中的 key 优先且必须非空，auth 文件只是兜底。
pub fn resolve_configured_api_key() -> Option<String> {
    if let Ok(env_key) = std::env::var("OPENAI_API_KEY")
        && !env_key.is_empty()
    {
        return Some(env_key);
    }
    read_api_key_from_auth_file()
}

/// 依序尝试 `auth.openai`/`auth.codex`/`auth.chatgpt`：先按 JS 真值筛掉空
/// 条目，字符串形式直接作为 key，对象形式先取 `access`（OAuth）再取
/// `token`；auth 文件读取失败只记 warn 并返回 `None`（不影响启动）。
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
/// 生产经 [`TtsService::production`] 构造为 `Arc` 共享。
pub struct TtsService {
    /// 实际发起合成调用的 provider。
    provider: Arc<dyn SpeechProvider>,
    /// 服务端 API key 的解析来源（env + auth 文件）。
    key_source: ApiKeySource,
}

/// 可用性判定与一次合成请求的完整组装。
impl TtsService {
    /// 注入 provider 与 key 来源构造服务。
    pub fn new(provider: Arc<dyn SpeechProvider>, key_source: ApiKeySource) -> Self {
        Self {
            provider,
            key_source,
        }
    }

    /// Production wiring: real HTTP provider, env + auth-file key source.
    /// 真实 HTTP provider 加 env/auth 文件 key 解析。
    pub fn production() -> Arc<Self> {
        Arc::new(Self::new(
            Arc::new(HttpSpeechProvider::default()),
            Arc::new(resolve_configured_api_key),
        ))
    }

    /// `isAvailable`: an OpenAI key is configured on the server.
    /// 只探测 key 来源是否返回 `Some`，不发网络请求。
    pub fn is_available(&self) -> bool {
        (self.key_source)().is_some()
    }

    /// `generateSpeechStream`.
    ///
    /// key/endpoint 决策：有自定义 baseURL 或客户端 key 时走自定义路径（空
    /// key 用 "not-required" 占位），否则要求服务端配置 key；都没有则返回固定
    /// 错误文案。请求体组装：voice/model/speed 带默认值；`instructions` 与
    /// `response_format=mp3` 仅在未使用自定义 baseURL 时发送（兼容服务器可能
    /// 不支持这两个参数，但都支持 `speed`）。
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

/// 服务层行为测试：key/endpoint 决策与请求体组装契约。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 记录请求并返回预设结果的 provider fake。
    struct FakeProvider {
        /// 收到的请求记录（供断言）。
        requests: Mutex<Vec<SpeechRequest>>,
        /// 预设结果（音频字节或错误消息）。
        result: Result<Vec<u8>, String>,
    }

    /// fake 的 trait 实现。
    impl SpeechProvider for FakeProvider {
        /// 入队请求后异步返回预设结果。
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

    /// 服务 + fake 的装配件。
    struct Fixture {
        /// 被测服务。
        service: TtsService,
        /// 记录请求的 fake。
        provider: Arc<FakeProvider>,
    }

    /// 用给定结果与 key 构造装配件。
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

    /// 仅含文本 "hello" 的默认选项。
    fn default_options() -> GenerateOptions {
        GenerateOptions {
            text: "hello".to_string(),
            ..Default::default()
        }
    }

    /// 取出 fake 记录的最后一条请求。
    fn captured(fixture: &Fixture) -> SpeechRequest {
        fixture
            .provider
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap()
    }

    /// 验证 `is_available` 完全跟随注入的 key 来源。
    #[test]
    fn availability_follows_the_key_source() {
        assert!(fixture(Ok(vec![]), Some("k")).service.is_available());
        assert!(!fixture(Ok(vec![]), None).service.is_available());
    }

    /// 验证无 key 且无自定义服务器时的固定错误文案。
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

    /// 验证空白文本被拒绝（"Text is required for TTS"）。
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

    /// 验证服务端 key 走官方端点并带全量参数（含 instructions 与
    /// response_format）。
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

    /// 验证自定义 baseURL 走安全参数子集（不带 instructions/response_format，
    /// key 用 "not-required" 占位）。
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

    /// 验证 provider 错误消息原样透传。
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
