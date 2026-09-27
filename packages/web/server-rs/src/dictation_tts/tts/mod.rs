//! Port of `server/lib/tts/` HTTP surface — `routes.js` registrations:
//! `/api/voice/token`, `/api/tts/speak`, `/api/text/summarize`,
//! `/api/tts/status`, `/api/tts/say/status`, `/api/tts/say/speak`, and
//! `/api/stt/transcribe`. The module also hosts the domain pieces
//! (`base-url`, `language-detect`, `service`, `stt`, `capability-runtime`).
//!
//! Registration order in the JS (`bootstrap-runtime.js` after
//! `registerAuthAndAccessRoutes`) puts these routes behind the `/api` auth
//! gate — mirrored by layering `ui_auth::middleware` over this router.

//! 中文说明：本模块把旧 JS server `server/lib/tts/` 的 HTTP 面移植为 axum 路由，
//! 注册 `/api/voice/token`、`/api/tts/speak`、`/api/text/summarize`、
//! `/api/tts/status`、`/api/tts/say/status`、`/api/tts/say/speak`、
//! `/api/stt/transcribe` 七个端点，并托管 base_url（自定义 OpenAI 兼容 URL 的
//! 校验与规范化）、capability（macOS `say` 能力探测）、language_detect（语言
//! 检测与 voice 挑选）、service（语音合成）、stt（语音转写 proxy）五个子模块。
//! 整个 router 被 `ui_auth::middleware` 包裹，因此全部路由都位于 `/api` 鉴权门
//! 之后，与 JS 版 `bootstrap-runtime.js` 在 `registerAuthAndAccessRoutes` 之后
//! 注册的顺序保持一致。
/// 自定义 OpenAI 兼容 base URL 的校验与规范化（默认仅放行本地主机）。
pub mod base_url;
/// macOS `say` 命令能力探测：启动时执行一次 `say -v '?'` 并共享唯一结果。
pub mod capability;
/// 无第三方依赖的语言检测，以及按语言挑选 `say` voice 的逻辑。
pub mod language_detect;
/// OpenAI（或 OpenAI 兼容服务器）语音合成服务封装。
pub mod service;
/// 语音转写（STT）proxy：把音频以 multipart 转发到 OpenAI 兼容的
/// `/v1/audio/transcriptions` 端点。
pub mod stt;

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use crate::context::RouterContext;
use capability::SayCapabilityProbe;
use language_detect::{
    VoiceEntry, detect_text_language, language_of_locale, pick_voice_for_language,
};
use service::{GenerateOptions, TtsService};
use stt::{HttpTranscriber, TranscribeParams, Transcriber};

/// `registerCommonRequestMiddleware` gives `/api/voice`, `/api/tts`, and
/// `/api/text` bodies `express.json({ limit: '50mb' })`.
/// 即 50 MiB；超限的请求体按 413 拒绝。
const JSON_BODY_LIMIT_BYTES: usize = 50 * 1024 * 1024;
/// `express.raw({ limit: '20mb' })` on `/api/stt/transcribe`.
const STT_BODY_LIMIT_BYTES: usize = 20 * 1024 * 1024;
/// 即 20 MiB；超限的音频 body 按 413 拒绝。
/// 请求未携带 `x-model` 头时 STT 转写使用的默认 whisper 模型。
const DEFAULT_STT_MODEL: &str = "deepdml/faster-whisper-large-v3-turbo-ct2";

/// `process.platform` seam (tests drive the non-macOS branch deterministically).
/// 编译期定值：macOS 目标返回 `"darwin"`，其余返回 `"linux"`。
fn production_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

/// 各路由共享的状态：语音合成服务、`say` 能力探测、转写器、平台名与
/// 环境变量读取接缝。
#[derive(Clone)]
struct TtsState {
    /// OpenAI 语音合成服务（内含 provider 与 API key 来源）。
    service: Arc<TtsService>,
    /// `say` 能力探测的共享结果（所有请求等待同一次探测）。
    say: SayCapabilityProbe,
    /// STT 转写器（生产为 HTTP 实现，测试注入 fake）。
    transcriber: Arc<dyn Transcriber>,
    /// 当前平台名；`say` 相关端点仅在 `"darwin"` 上可用。
    platform: &'static str,
    /// Env read seam for the base-URL policy (production: `std::env`).
    /// 测试注入固定映射表即可驱动 base URL 策略的各分支。
    env_reader: EnvReader,
}

/// 按 key 名返回非空环境值的函数指针；抽象成 `Arc<dyn Fn>` 以便测试替换
/// `std::env`。
type EnvReader = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// 生产实现：读 `std::env::var` 并把空白值视为未设置（与 JS 侧仅非空值
/// 生效的语义一致）。
fn production_env_reader() -> EnvReader {
    Arc::new(|key: &str| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}
/// 构建生产 router：装配真实 [`TtsService`]、按平台启动的 `say` 探测、HTTP
/// 转写器与 `std::env` 读取器，并把 `ui_auth::middleware(ctx)` 叠加在全部
/// 路由之上（`/api` 鉴权门）。
pub fn router(ctx: RouterContext) -> axum::Router {
    let state = TtsState {
        service: TtsService::production(),
        say: SayCapabilityProbe::spawn(production_platform()),
        transcriber: Arc::new(HttpTranscriber::default()),
        platform: production_platform(),
        env_reader: production_env_reader(),
    };
    routes()
        .layer(crate::ui_auth::middleware(ctx))
        .with_state(state)
}

/// 注册全部七个 TTS/STT 路由（自身不挂鉴权层，由调用方叠加）；状态类型为
/// [`TtsState`]。
fn routes() -> Router<TtsState> {
    Router::new()
        .route("/api/voice/token", post(voice_token))
        .route("/api/tts/speak", post(tts_speak))
        .route("/api/text/summarize", post(text_summarize))
        .route("/api/tts/status", get(tts_status))
        .route("/api/tts/say/status", get(say_status))
        .route("/api/tts/say/speak", post(say_speak))
        .route("/api/stt/transcribe", post(stt_transcribe))
}

/// 测试入口：把给定状态直接绑定到 `routes()`，不经过鉴权中间件。
#[cfg(test)]
pub(crate) fn test_router(state: TtsState) -> Router {
    routes().with_state(state)
}

// ---------------------------------------------------------------------------
// Body + coercion helpers (JS destructure defaults apply to missing keys
// only — JSON `null` survives as `null`)
// ---------------------------------------------------------------------------

/// 读取并解析 JSON 请求体：空体按 `express.json()` 语义返回空对象 `{}`；
/// 超过 [`JSON_BODY_LIMIT_BYTES`] 返回 413，非法 JSON 返回 400（错误以
/// `Err(response)` 形式交还给调用方直接返回）。
async fn read_json_body(request: Request) -> Result<Value, Response> {
    let bytes = to_bytes(request.into_body(), JSON_BODY_LIMIT_BYTES)
        .await
        .map_err(|_| json_error(StatusCode::PAYLOAD_TOO_LARGE, "Payload too large"))?;
    if bytes.is_empty() {
        // express.json() yields `{}` for an empty body.
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| json_error(StatusCode::BAD_REQUEST, "Invalid JSON body"))
}

/// 构造 `{ "error": message }` 形式的 JSON 错误响应。
fn json_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// JS truthiness over a JSON value.
/// 数字 0 与 0.0 为假；数组与对象即使为空也为真。
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// JS `const { key = default } = body` — a missing key takes the default,
/// every present value (including `null`) passes through.
/// 因此字段"存在但为 `null`"与"缺失"在这里是两种不同结果。
fn field_or(body: &Value, key: &str, default: Value) -> Value {
    match body.get(key) {
        Some(value) => value.clone(),
        None => default,
    }
}

/// 读取 body 中某个键的字符串值；键缺失或不是字符串时返回 `None`。
fn str_field<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get(key).and_then(Value::as_str)
}

/// JS template-literal interpolation of a JSON value (`${rate}`).
/// 字符串原样返回，其余类型按 JS 规则转字符串（如 `null` → "null"）。
fn js_template_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => js_number_string(number),
        other => other.to_string(),
    }
}

/// 按 JS `String(number)` 的规则格式化：整数值不带小数点；绝对值小于 1e21
/// 且无小数部分的浮点数也压成整数形式，其余用 `Display` 输出。
fn js_number_string(number: &serde_json::Number) -> String {
    if let Some(integer) = number.as_i64() {
        return integer.to_string();
    }
    match number.as_f64() {
        Some(value) if value.fract() == 0.0 && value.abs() < 1e21 => format!("{}", value as i64),
        Some(value) => format!("{value}"),
        None => number.to_string(),
    }
}

// ---------------------------------------------------------------------------
// POST /api/voice/token
// ---------------------------------------------------------------------------

/// Pure body of the route: only `process.env.OPENAI_API_KEY` decides (the
/// auth file is never consulted here).
///
/// 有 key 返回 200 与 provider 信息；无 key（含空串）返回 503 与配置提示。
fn voice_token_response(openai_api_key: Option<&str>) -> Response {
    match openai_api_key.filter(|key| !key.is_empty()) {
        Some(_) => Json(json!({
            "allowed": true,
            "provider": "openai",
            "message": "OpenAI TTS is available"
        }))
        .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "allowed": false,
                "error": "OpenAI voice service not configured. Set OPENAI_API_KEY environment variable."
            })),
        )
            .into_response(),
    }
}

/// POST `/api/voice/token` 处理器：以进程环境中的 `OPENAI_API_KEY` 生成
/// 可用性响应。
async fn voice_token(_request: Request) -> Response {
    voice_token_response(std::env::var("OPENAI_API_KEY").ok().as_deref())
}

// ---------------------------------------------------------------------------
// POST /api/tts/speak
// ---------------------------------------------------------------------------

/// POST `/api/tts/speak` 处理器：校验文本与自定义 base URL 后调用 OpenAI
///（或自定义服务器）合成语音。可用性三选一：服务端配置的 key、请求体
/// `apiKey`、自定义 `baseURL`；三者皆无时返回 503。成功时以音频内容类型
/// 回传字节并附 `Cache-Control: no-cache` 与 Content-Length；provider 失败
/// 时返回 500，`detail` 回显原始 `model`/`voice` 字段与 `hasBaseURL` 布尔值
///（与 JS 的 JSON 序列化丢弃 `undefined` 的行为一致）。
async fn tts_speak(State(state): State<TtsState>, request: Request) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };

    let normalized_base_url = match base_url::normalize_custom_openai_base_url_with(
        str_field(&body, "baseURL"),
        (state.env_reader)("OMPCHAMBER_RUNTIME").as_deref(),
        (state.env_reader)("OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS").as_deref(),
    ) {
        Ok(value) => value,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, &error),
    };

    let Some(text) = str_field(&body, "text") else {
        return json_error(StatusCode::BAD_REQUEST, "Text is required");
    };
    if text.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Text is required");
    }

    // Availability: server-configured key, client-provided key, or a custom
    // server URL.
    let has_server_key = state.service.is_available();
    let client_api_key = str_field(&body, "apiKey")
        .filter(|key| !key.trim().is_empty())
        .map(str::trim);
    let has_custom_base_url = normalized_base_url
        .as_deref()
        .is_some_and(|url| !url.is_empty());

    if !has_server_key && client_api_key.is_none() && !has_custom_base_url {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "TTS service not available. Please configure OpenAI in OpenCode, provide an API key, or set a custom server URL in settings."
            })),
        )
            .into_response();
    }

    // Historical summarize request fields are intentionally ignored; the
    // model-backed summarization provider is retired.
    let result = state
        .service
        .generate_speech_stream(GenerateOptions {
            text: text.trim().to_string(),
            voice: Some(field_or(&body, "voice", json!("nova"))),
            model: Some(field_or(&body, "model", json!("gpt-4o-mini-tts"))),
            speed: Some(field_or(&body, "speed", json!(0.9))),
            instructions: body.get("instructions").cloned(),
            api_key: client_api_key.map(str::to_string),
            base_url: if has_custom_base_url {
                normalized_base_url.clone()
            } else {
                None
            },
        })
        .await;

    match result {
        Ok(output) => {
            let mut response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, output.content_type)
                .header(header::CACHE_CONTROL, "no-cache");
            if let Ok(length) = HeaderValue::from_str(&output.buffer.len().to_string()) {
                response = response.header(header::CONTENT_LENGTH, length);
            }
            response
                .body(Body::from(output.buffer))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(message) => {
            // JS detail: `{ model, voice, hasBaseURL }` from the RAW body
            // fields; `undefined` values are dropped by JSON serialization.
            let mut detail = Map::new();
            if let Some(model) = body.get("model") {
                detail.insert("model".to_string(), model.clone());
            }
            if let Some(voice) = body.get("voice") {
                detail.insert("voice".to_string(), voice.clone());
            }
            detail.insert(
                "hasBaseURL".to_string(),
                Value::Bool(body.get("baseURL").is_some_and(js_truthy)),
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": message, "detail": Value::Object(detail) })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// POST /api/text/summarize
// ---------------------------------------------------------------------------

/// POST `/api/text/summarize` 处理器：走本地摘要兜底（模型摘要 provider 已
/// 退役，历史请求字段被忽略）。`threshold` 缺省 200、`maxLength` 缺省 500、
/// `mode` 缺省 `"tts"`；核心语义在
/// `crate::small_model::summarization::summarize_text_simple`。
async fn text_summarize(request: Request) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };

    let Some(text) = str_field(&body, "text") else {
        return json_error(StatusCode::BAD_REQUEST, "Text is required");
    };
    if text.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Text is required");
    }

    // JS destructuring defaults apply to `undefined` only; the ported
    // `summarize_text_simple` owns the remaining coercion semantics
    // (`null`/non-number → the JS comparison outcomes; `len <= floor(t)` is
    // equivalent to the JS `len <= t` for the integer lengths involved).
    let threshold = match body.get("threshold") {
        None => 200u64,
        Some(Value::Number(number)) => number
            .as_f64()
            .map(|value| value.max(0.0) as u64)
            .unwrap_or(0),
        Some(_) => 0,
    };
    let max_length = match body.get("maxLength") {
        None => Some(500.0f64),
        Some(Value::Number(number)) => number.as_f64(),
        Some(_) => Some(f64::NAN),
    };
    let mode = str_field(&body, "mode").unwrap_or("tts");

    let result = crate::small_model::summarization::summarize_text_simple(
        Some(text),
        threshold,
        max_length,
        mode,
    )
    .await;
    Json(result).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/tts/status · GET /api/tts/say/status
// ---------------------------------------------------------------------------

/// GET `/api/tts/status` 处理器：返回服务端 OpenAI TTS 可用性与固定 voice
/// 列表（[`service::TTS_VOICES`]）。
async fn tts_status(State(state): State<TtsState>) -> Response {
    Json(json!({
        "available": state.service.is_available(),
        "voices": service::TTS_VOICES,
    }))
    .into_response()
}

/// GET `/api/tts/say/status` 处理器：等待启动时那次 `say` 探测的权威结果并
/// 原样返回。
async fn say_status(State(state): State<TtsState>) -> Response {
    // The startup probe runs concurrently with server bootstrap; an early
    // status request waits for that same authoritative result.
    Json(state.say.get().await).into_response()
}

// ---------------------------------------------------------------------------
// POST /api/tts/say/speak
// ---------------------------------------------------------------------------

/// 当前 Unix 时间戳（毫秒），用于生成 `say` 输出临时文件名；时钟早于纪元时
/// 返回 0。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// The `language: 'auto'` resolution: keep the chosen voice while it speaks
/// the text's language, otherwise switch to an installed voice that does.
///
/// 返回 (最终 voice, 检测到的语言)。非 `"auto"` 时原样返回且语言为 `None`；
/// `auto` 时对样本（`languageSample` 前 4000 字符，空白则回退正文）做语言
/// 检测，若所选 voice 的 locale 不说该语言且已安装 voice 中有会说的，就切换
/// 到那个 voice。
pub(crate) fn resolve_say_voice(
    voice: &str,
    language: Option<&str>,
    language_sample: Option<&str>,
    text: &str,
    voices: &[VoiceEntry],
) -> (String, Option<String>) {
    if language != Some("auto") {
        return (voice.to_string(), None);
    }
    // `languageSample.slice(0, 4000)` — JS slices UTF-16 code units; this
    // takes the first 4000 chars (identical for BMP text).
    let sample = language_sample
        .filter(|sample| !sample.trim().is_empty())
        .map(|sample| sample.chars().take(4000).collect::<String>())
        .unwrap_or_else(|| text.to_string());
    let detected = detect_text_language(&sample).language;
    let mut resolved = voice.to_string();
    let chosen = voices.iter().find(|entry| entry.name == voice);
    if language_of_locale(chosen.map(|entry| entry.locale.as_str())).as_deref()
        != Some(detected.as_str())
        && let Some(matched) = pick_voice_for_language(&detected, voices)
    {
        resolved = matched;
    }
    (resolved, Some(detected))
}

/// 从 `say` 能力 JSON 的 `voices` 数组提取 [`VoiceEntry`] 列表；`voices` 不是
/// 数组或条目缺少 `name`/`locale` 字符串字段时，对应条目被跳过（整体缺失
/// 返回空表）。
fn capability_voices(capability: &Value) -> Vec<VoiceEntry> {
    capability
        .get("voices")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    Some(VoiceEntry {
                        name: entry.get("name")?.as_str()?.to_string(),
                        locale: entry.get("locale")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// POST `/api/tts/say/speak` 处理器：仅 macOS 可用（其余平台 503）。voice
/// 缺省 "Samantha"；`language: 'auto'` 时先解析 voice 与语言，随后以 argv
/// 形式调用 `say -v <voice> -r <rate> -o <临时文件> --data-format=aac <text>`
/// 合成 AAC 音频，读回文件内容（尽力删除临时文件）后以 `audio/mp4` 返回，
/// 并附 `X-Speech-Voice` 与（auto 时）`X-Speech-Language` 响应头。
async fn say_speak(State(state): State<TtsState>, request: Request) -> Response {
    let body = match read_json_body(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };

    let Some(text) = str_field(&body, "text") else {
        return json_error(StatusCode::BAD_REQUEST, "Text is required");
    };
    if text.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Text is required");
    }

    let mut voice = match str_field(&body, "voice") {
        Some(voice) if !voice.trim().is_empty() => voice.trim().to_string(),
        _ => "Samantha".to_string(),
    };
    let rate = field_or(&body, "rate", json!(200));

    // Check if we're on macOS.
    if state.platform != "darwin" {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "macOS say command not available on this platform",
        );
    }

    let mut resolved_language = None;
    let language = str_field(&body, "language");
    if language == Some("auto") {
        let capability = state.say.get().await;
        let voices = capability_voices(&capability);
        let (resolved, detected) = resolve_say_voice(
            &voice,
            language,
            str_field(&body, "languageSample"),
            text,
            &voices,
        );
        voice = resolved;
        resolved_language = detected;
    }

    // `say -v <voice> -r <rate> -o <temp> --data-format=aac <text>` — the JS
    // shells out with quoting; the argv form synthesizes the same audio
    // without a shell.
    let temp_file = std::env::temp_dir().join(format!("say-{}.m4a", now_ms()));
    let status = match tokio::process::Command::new("say")
        .arg("-v")
        .arg(&voice)
        .arg("-r")
        .arg(js_template_string(&rate))
        .arg("-o")
        .arg(&temp_file)
        .arg("--data-format=aac")
        .arg(text.trim())
        .output()
        .await
    {
        Ok(output) if output.status.success() => output.status,
        Ok(output) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Command failed: say exited with status {}", output.status),
            );
        }
        Err(error) => {
            return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
        }
    };
    let _ = status;

    let audio = match tokio::fs::read(&temp_file).await {
        Ok(audio) => audio,
        Err(error) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
    };
    // Clean up temp file (best-effort, like the JS fire-and-forget unlink).
    let _ = tokio::fs::remove_file(&temp_file).await;

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/mp4")
        .header("X-Speech-Voice", &voice);
    if let Some(language) = resolved_language.as_deref() {
        builder = builder.header("X-Speech-Language", language);
    }
    if let Ok(length) = HeaderValue::from_str(&audio.len().to_string()) {
        builder = builder.header(header::CONTENT_LENGTH, length);
    }
    builder
        .body(Body::from(audio))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// POST /api/stt/transcribe
// ---------------------------------------------------------------------------

/// POST `/api/stt/transcribe` 处理器：要求 `Content-Type` 以 `audio/` 开头、
/// body 非空且携带 `x-base-url` 头（违反任一返回 400）；`x-model` 缺省
/// [`DEFAULT_STT_MODEL`]，bearer token 取自 `Authorization` 头。参数交给
/// [`Transcriber`] 转写，成功返回 `{ "transcript": ... }`，失败 500。
async fn stt_transcribe(State(state): State<TtsState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .unwrap_or("");
    // `express.raw({ type: contentType.startsWith('audio/') })`: a non-audio
    // content type leaves the body unparsed → "Audio data is required".
    if !content_type.starts_with("audio/") {
        return json_error(StatusCode::BAD_REQUEST, "Audio data is required");
    }

    let bytes = match to_bytes(body, STT_BODY_LIMIT_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "Payload too large"),
    };
    if bytes.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Audio data is required");
    }

    let mime_type = if content_type.is_empty() {
        "audio/webm"
    } else {
        content_type
    }
    .split(',')
    .next()
    .unwrap_or("")
    .trim()
    .to_string();

    let header_str = |name: &'static str| trimmed_header(&parts.headers, name);

    let base_url = header_str("x-base-url").unwrap_or_default();
    if base_url.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "X-Base-URL header is required");
    }
    let model = header_str("x-model").unwrap_or_else(|| DEFAULT_STT_MODEL.to_string());
    let language = header_str("x-language");
    let api_key = header_str("authorization").and_then(|auth| {
        auth.strip_prefix("Bearer ")
            .map(|token| token.trim())
            .filter(|token| !token.is_empty())
            .map(str::to_string)
    });

    match state
        .transcriber
        .transcribe(TranscribeParams {
            audio_buffer: bytes.to_vec(),
            mime_type,
            model,
            base_url: Some(base_url),
            api_key,
            language,
        })
        .await
    {
        Ok(transcript) => Json(json!({ "transcript": transcript })).into_response(),
        Err(message) => json_error(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

/// A request header as a trimmed, non-empty string (JS
/// `typeof req.headers[name] === 'string' ? value.trim() : ''`).
/// 头缺失、非 ASCII 或 trim 后为空都返回 `None`（模拟 JS 空串的 falsy 语义）。
fn trimmed_header(headers: &axum::http::HeaderMap, name: &'static str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 路由层测试：以注入的 fake provider/transcriber 验证各端点的状态码、响应
/// 形状与转发参数。
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use std::sync::Mutex;
    use tower::ServiceExt;

    use service::{SpeechOutput, SpeechProvider, SpeechRequest};

    /// 测试用语音合成 fake：记录收到的请求并返回预设结果。
    struct FakeSpeech {
        /// 预设结果（音频字节或错误消息）。
        result: Result<Vec<u8>, String>,
        /// 收到的 `SpeechRequest` 记录（供断言转发参数）。
        requests: Mutex<Vec<SpeechRequest>>,
    }

    /// fake 的 trait 实现：入队请求后返回预设结果。
    impl SpeechProvider for FakeSpeech {
        /// 记录请求并异步返回预设的合成结果。
        fn generate_speech(&self, request: SpeechRequest) -> service::SpeechFuture {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request);
            let result = self.result.clone();
            Box::pin(async move {
                result.map(|buffer| SpeechOutput {
                    buffer,
                    content_type: "audio/mpeg",
                })
            })
        }
    }

    /// 测试用转写 fake：记录调用参数并返回预设转写文本。
    struct FakeTranscriber {
        /// 预设结果（转写文本或错误消息）。
        result: Result<String, String>,
        /// 收到的 `TranscribeParams` 记录。
        calls: Mutex<Vec<TranscribeParams>>,
    }

    /// fake 的 trait 实现：入队参数后返回预设结果。
    impl Transcriber for FakeTranscriber {
        /// 记录参数并异步返回预设转写结果。
        fn transcribe(&self, params: TranscribeParams) -> stt::TranscribeFuture {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(params);
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }
    /// 一次路由测试的装配件：测试 router 与两个 fake 的共享句柄。
    struct Fixture {
        /// 已绑定状态的 router（未挂鉴权层）。
        router: Router,
        /// 语音合成 fake。
        speech: Arc<FakeSpeech>,
        /// 转写 fake。
        transcriber: Arc<FakeTranscriber>,
    }

    /// 用给定的合成结果、服务端 key、`say` 能力 JSON 与平台名组装测试装配件。
    fn fixture(
        speech_result: Result<Vec<u8>, String>,
        key: Option<&str>,
        capability: Value,
        platform: &'static str,
    ) -> Fixture {
        let speech = Arc::new(FakeSpeech {
            result: speech_result,
            requests: Mutex::new(Vec::new()),
        });
        let transcriber = Arc::new(FakeTranscriber {
            result: Ok("transcribed text".to_string()),
            calls: Mutex::new(Vec::new()),
        });
        let key = key.map(str::to_string);
        let state = TtsState {
            service: Arc::new(TtsService::new(
                Arc::clone(&speech) as Arc<dyn SpeechProvider>,
                Arc::new(move || key.clone()),
            )),
            say: SayCapabilityProbe::resolved(capability),
            transcriber: Arc::clone(&transcriber) as Arc<dyn Transcriber>,
            platform,
            env_reader: Arc::new(|_| None),
        };
        Fixture {
            router: test_router(state),
            speech,
            transcriber,
        }
    }

    /// 默认装配件：合成成功、配置了 server key、`say` 探测不可用、平台 linux。
    fn default_fixture() -> Fixture {
        fixture(
            Ok(vec![1, 2, 3]),
            Some("server-key"),
            json!({ "available": false, "voices": [], "reason": "Not checked" }),
            "linux",
        )
    }

    /// 以 one-shot 方式调用 router，解出 (状态码, JSON body 或 `Value::Null`,
    /// 原始字节)。
    async fn call(router: &Router, request: Request<Body>) -> (StatusCode, Value, Vec<u8>) {
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        let is_json = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("application/json"));
        let value = if is_json {
            serde_json::from_slice(&body).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        (status, value, body)
    }

    /// 构造带 `application/json` 头的 POST 请求。
    fn post_json(path: &str, body: Value) -> Request<Body> {
        HttpRequest::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    // -- voice/token ------------------------------------------------------

    /// 验证 voice/token：有环境 key 返回 200，无 key 返回 503。
    #[test]
    fn voice_token_maps_env_key_presence() {
        let unconfigured = voice_token_response(None);
        assert_eq!(unconfigured.status(), StatusCode::SERVICE_UNAVAILABLE);
        let configured = voice_token_response(Some("sk-key"));
        assert_eq!(configured.status(), StatusCode::OK);
    }

    /// 验证空白 `OPENAI_API_KEY` 等同未配置（503）。
    #[test]
    fn voice_token_treats_blank_env_as_unconfigured() {
        assert_eq!(
            voice_token_response(Some("")).status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // -- tts/speak --------------------------------------------------------

    /// 验证无任何 key 与自定义 URL 时 `/api/tts/speak` 返回 503 及固定错误文案。
    #[tokio::test]
    async fn speak_returns_503_without_any_key_or_custom_url() {
        let fixture = fixture(
            Ok(vec![]),
            None,
            json!({ "available": false, "voices": [], "reason": "Not checked" }),
            "linux",
        );
        let (status, body, _) = call(
            &fixture.router,
            post_json("/api/tts/speak", json!({ "text": "hello" })),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body["error"],
            "TTS service not available. Please configure OpenAI in OpenCode, provide an API key, or set a custom server URL in settings."
        );
    }

    /// 验证空白文本返回 400 "Text is required"，且远程自定义 URL 被策略拒绝。
    #[tokio::test]
    async fn speak_validates_text_and_base_url() {
        let fixture = default_fixture();
        let (status, body, _) = call(
            &fixture.router,
            post_json("/api/tts/speak", json!({ "text": "   " })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Text is required");

        let (status, body, _) = call(
            &fixture.router,
            post_json(
                "/api/tts/speak",
                json!({ "text": "hi", "baseURL": "https://remote.example.com/v1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .starts_with("Remote custom server URLs are disabled")
        );
    }

    /// 验证成功路径：返回音频字节与 Content-Type/Cache-Control/Content-Length
    /// 头，且默认 voice/model/speed 与服务端 key 被正确转发。
    #[tokio::test]
    async fn speak_streams_audio_with_headers() {
        let fixture = default_fixture();
        let response = fixture
            .router
            .clone()
            .oneshot(post_json(
                "/api/tts/speak",
                json!({ "text": "hello world" }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "audio/mpeg"
        );
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache"
        );
        assert_eq!(response.headers().get(header::CONTENT_LENGTH).unwrap(), "3");
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], &[1, 2, 3]);
        let request = fixture
            .speech
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap();
        assert_eq!(request.body["voice"], "nova");
        assert_eq!(request.body["model"], "gpt-4o-mini-tts");
        assert_eq!(request.body["speed"], 0.9);
        assert_eq!(request.body["input"], "hello world");
        assert_eq!(request.api_key, "server-key");
    }

    /// 验证 provider 失败映射为 500，`detail` 回显原始 `model`/`voice` 字段与
    /// `hasBaseURL`。
    #[tokio::test]
    async fn speak_maps_provider_errors_to_detail_shape() {
        let fixture = fixture(
            Err("Failed to generate speech: 500 Internal Server Error".to_string()),
            Some("k"),
            json!({ "available": false, "voices": [], "reason": "Not checked" }),
            "linux",
        );
        let (status, body, _) = call(
            &fixture.router,
            post_json(
                "/api/tts/speak",
                json!({ "text": "hi", "model": "m", "voice": "v", "baseURL": "http://localhost:9/v1" }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body["error"],
            "Failed to generate speech: 500 Internal Server Error"
        );
        assert_eq!(body["detail"]["model"], "m");
        assert_eq!(body["detail"]["voice"], "v");
        assert_eq!(body["detail"]["hasBaseURL"], true);
    }

    // -- text/summarize ---------------------------------------------------

    /// 验证 note 模式摘要兜底：取首句并标记 `summarized: false` 与原因。
    #[tokio::test]
    async fn summarize_returns_local_note_fallback() {
        let router = default_fixture().router;
        let (status, body, _) = call(
            &router,
            post_json(
                "/api/text/summarize",
                json!({
                    "text": "First sentence. Second sentence with the useful insight.",
                    "threshold": 0,
                    "maxLength": 100,
                    "mode": "note"
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["summary"], "First sentence.");
        assert_eq!(body["summarized"], false);
        assert_eq!(body["reason"], "Model summarization provider unavailable");
    }

    /// 验证 notification 模式兜底：原文原样返回且 `summarized: false`。
    #[tokio::test]
    async fn summarize_notification_fallback_returns_text() {
        let router = default_fixture().router;
        let (status, body, _) = call(
            &router,
            post_json(
                "/api/text/summarize",
                json!({
                    "text": "Notification text that should fall back cleanly.",
                    "threshold": 0,
                    "maxLength": 100,
                    "mode": "notification"
                }),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["summary"],
            "Notification text that should fall back cleanly."
        );
        assert_eq!(body["summarized"], false);
        assert_eq!(body["reason"], "Model summarization provider unavailable");
    }

    /// 验证缺失文本返回 400 "Text is required"。
    #[tokio::test]
    async fn summarize_requires_text() {
        let router = default_fixture().router;
        let (status, body, _) = call(
            &router,
            post_json("/api/text/summarize", json!({ "threshold": 5 })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Text is required");
    }

    /// 验证文本长度低于 threshold 时返回 "Text under threshold" 原因。
    #[tokio::test]
    async fn summarize_under_threshold_reason() {
        let router = default_fixture().router;
        let (_, body, _) = call(
            &router,
            post_json(
                "/api/text/summarize",
                json!({ "text": "Short text.", "threshold": 100 }),
            ),
        )
        .await;
        assert_eq!(body["summarized"], false);
        assert_eq!(body["reason"], "Text under threshold");
    }

    // -- status routes ----------------------------------------------------

    /// 验证 `/api/tts/status` 返回可用性与 13 个固定 voice。
    #[tokio::test]
    async fn tts_status_reports_availability_and_voices() {
        let router = default_fixture().router;
        let (status, body, _) = call(
            &router,
            HttpRequest::get("/api/tts/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["available"], true);
        assert_eq!(body["voices"].as_array().unwrap().len(), 13);
        assert_eq!(body["voices"][0], "alloy");
        assert_eq!(body["voices"][12], "cedar");
    }

    /// 验证 `/api/tts/say/status` 等待并原样返回探测结果 JSON。
    #[tokio::test]
    async fn say_status_waits_for_the_authoritative_capability() {
        let capability = json!({
            "available": true,
            "voices": [{ "name": "Samantha", "locale": "en_US" }]
        });
        let router = fixture(Ok(vec![]), None, capability.clone(), "linux").router;
        let (status, body, _) = call(
            &router,
            HttpRequest::get("/api/tts/say/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, capability);
    }

    // -- tts/say/speak ----------------------------------------------------

    /// 验证非 macOS 平台 `/api/tts/say/speak` 返回 503。
    #[tokio::test]
    async fn say_speak_refuses_off_macos() {
        let router = default_fixture().router;
        let (status, body, _) = call(
            &router,
            post_json("/api/tts/say/speak", json!({ "text": "hello" })),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body["error"],
            "macOS say command not available on this platform"
        );
    }

    /// 验证缺失文本返回 400 "Text is required"。
    #[tokio::test]
    async fn say_speak_requires_text() {
        let router = default_fixture().router;
        let (status, body, _) = call(&router, post_json("/api/tts/say/speak", json!({}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Text is required");
    }

    /// 验证 `auto` 语言解析：乌克兰语文本切换到会该语言的 Enhanced voice、英文
    /// 保持原 voice、非 auto 时原样返回且语言为 `None`。
    #[test]
    fn resolves_say_voice_for_the_texts_language() {
        let voices = vec![
            VoiceEntry {
                name: "Samantha".into(),
                locale: "en_US".into(),
            },
            VoiceEntry {
                name: "Lesya".into(),
                locale: "uk_UA".into(),
            },
            VoiceEntry {
                name: "Lesya (Enhanced)".into(),
                locale: "uk_UA".into(),
            },
        ];
        let (voice, language) = resolve_say_voice(
            "Samantha",
            Some("auto"),
            Some("Привіт! Це відповідь українською мовою, і вона досить довга."),
            "",
            &voices,
        );
        assert_eq!(voice, "Lesya (Enhanced)");
        assert_eq!(language.as_deref(), Some("uk"));

        // English text keeps the English voice.
        let (voice, language) =
            resolve_say_voice("Samantha", Some("auto"), None, "Hello there", &voices);
        assert_eq!(voice, "Samantha");
        assert_eq!(language.as_deref(), Some("en"));

        // Non-auto language keeps the chosen voice untouched.
        let (voice, language) = resolve_say_voice("Samantha", None, None, "Привіт", &voices);
        assert_eq!(voice, "Samantha");
        assert_eq!(language, None);
    }

    ///（仅 macOS）验证真实 `say` 合成返回非空 `audio/mp4` 与 voice/语言响应头。
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn say_speak_synthesizes_on_macos_with_resolved_voice() {
        let capability = json!({
            "available": true,
            "voices": [{ "name": "Samantha", "locale": "en_US" }]
        });
        let router = fixture(Ok(vec![]), None, capability, "darwin").router;
        let response = router
            .oneshot(post_json(
                "/api/tts/say/speak",
                json!({ "text": "Hello from the test.", "language": "auto" }),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "audio/mp4"
        );
        assert_eq!(
            response.headers().get("X-Speech-Voice").unwrap(),
            "Samantha"
        );
        assert_eq!(response.headers().get("X-Speech-Language").unwrap(), "en");
        let body = to_bytes(response.into_body(), 10 * 1024 * 1024)
            .await
            .unwrap();
        assert!(!body.is_empty());
    }

    // -- stt/transcribe ---------------------------------------------------

    /// 验证非 `audio/` Content-Type 返回 400 "Audio data is required"。
    #[tokio::test]
    async fn stt_requires_audio_content_type() {
        let fixture = default_fixture();
        let request = HttpRequest::post("/api/stt/transcribe")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("ignored"))
            .unwrap();
        let (status, body, _) = call(&fixture.router, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Audio data is required");
    }

    /// 验证空音频 body 返回 400 "Audio data is required"。
    #[tokio::test]
    async fn stt_requires_nonempty_audio() {
        let fixture = default_fixture();
        let request = HttpRequest::post("/api/stt/transcribe")
            .header(header::CONTENT_TYPE, "audio/webm")
            .body(Body::empty())
            .unwrap();
        let (status, body, _) = call(&fixture.router, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Audio data is required");
    }

    /// 验证缺失 `x-base-url` 头返回 400 "X-Base-URL header is required"。
    #[tokio::test]
    async fn stt_requires_base_url_header() {
        let fixture = default_fixture();
        let request = HttpRequest::post("/api/stt/transcribe")
            .header(header::CONTENT_TYPE, "audio/webm")
            .body(Body::from(vec![1, 2, 3]))
            .unwrap();
        let (status, body, _) = call(&fixture.router, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "X-Base-URL header is required");
    }

    /// 验证头参数（trim 后的 base URL、model、language、Bearer token）与音频
    /// 字节被原样转发，MIME 参数部分保留。
    #[tokio::test]
    async fn stt_proxies_to_the_transcriber_with_headers() {
        let fixture = default_fixture();
        let request = HttpRequest::post("/api/stt/transcribe")
            .header(header::CONTENT_TYPE, "audio/webm; codecs=opus")
            .header("x-base-url", " http://localhost:8880/v1 ")
            .header("x-model", "whisper-tiny")
            .header("x-language", "en")
            .header(header::AUTHORIZATION, "Bearer sk-test")
            .body(Body::from(vec![9, 9, 9]))
            .unwrap();
        let (status, body, _) = call(&fixture.router, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["transcript"], "transcribed text");
        let params = fixture
            .transcriber
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap();
        assert_eq!(params.audio_buffer, vec![9, 9, 9]);
        // JS splits on commas only — parameters survive (mimeTypeToExt
        // strips them later).
        assert_eq!(params.mime_type, "audio/webm; codecs=opus");
        assert_eq!(params.base_url.as_deref(), Some("http://localhost:8880/v1"));
        assert_eq!(params.model, "whisper-tiny");
        assert_eq!(params.language.as_deref(), Some("en"));
        assert_eq!(params.api_key.as_deref(), Some("sk-test"));
    }

    /// 验证转写器错误映射为 500 且 `error` 消息透传。
    #[tokio::test]
    async fn stt_maps_transcriber_errors_to_500() {
        let speech = Arc::new(FakeSpeech {
            result: Ok(vec![]),
            requests: Mutex::new(Vec::new()),
        });
        let transcriber = Arc::new(FakeTranscriber {
            result: Err("upstream broke".to_string()),
            calls: Mutex::new(Vec::new()),
        });
        let state = TtsState {
            service: Arc::new(TtsService::new(
                Arc::clone(&speech) as Arc<dyn SpeechProvider>,
                Arc::new(|| None),
            )),
            say: SayCapabilityProbe::resolved(json!({ "available": false, "voices": [] })),
            transcriber: Arc::clone(&transcriber) as Arc<dyn Transcriber>,
            platform: "linux",
            env_reader: Arc::new(|_| None),
        };
        let router = test_router(state);
        let request = HttpRequest::post("/api/stt/transcribe")
            .header(header::CONTENT_TYPE, "audio/wav")
            .header("x-base-url", "http://localhost:8880/v1")
            .body(Body::from(vec![1]))
            .unwrap();
        let (status, body, _) = call(&router, request).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "upstream broke");
    }

    /// 验证缺失 `x-model` 头时使用 `DEFAULT_STT_MODEL`，且 language/api key 为空。
    #[tokio::test]
    async fn stt_applies_default_model_when_header_missing() {
        let fixture = default_fixture();
        let request = HttpRequest::post("/api/stt/transcribe")
            .header(header::CONTENT_TYPE, "audio/wav")
            .header("x-base-url", "http://localhost:8880/v1")
            .body(Body::from(vec![1]))
            .unwrap();
        call(&fixture.router, request).await;
        let params = fixture
            .transcriber
            .calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop()
            .unwrap();
        assert_eq!(params.model, DEFAULT_STT_MODEL);
        assert_eq!(params.language, None);
        assert_eq!(params.api_key, None);
    }
}
