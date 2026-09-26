//! Port of `server/lib/small-model/call.js`: wire formats and per-provider
//! auth. Credentials never leave this module's request paths.
//!
//! Injectable seams mirror the JS test mocks: the HTTP transport
//! (`globalThis.fetch`), the OpenCode config layers (`shared.js`), the auth
//! store (`auth.js`) and the runtime provider snapshot
//! (`runtime-providers.js`).

use std::sync::Arc;

use base64::Engine as _;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde_json::{Map, Value, json};

use crate::small_model::auth_store::AuthStore;
use crate::small_model::http::{
    Fetch, FetchRequest, FetchResponse, merge_headers_case_insensitive, now_ms, spread_headers,
    uri_encode_component, utf16_len, utf16_prefix,
};
use crate::small_model::opencode_config::{ConfigReader, is_plain_object};
use crate::small_model::resolve::get_auth_entry_for_provider;
use crate::small_model::runtime_providers::{RuntimeProvider, RuntimeProviders};

const REQUEST_TIMEOUT_MS: u64 = 60_000;
const COPILOT_MODELS_TIMEOUT_MS: u64 = 5_000;
/// Generous default: thinking models that can't be switched off (DeepSeek,
/// Qwen, …) spend part of this budget on reasoning before the actual answer.
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 4_000;

const USER_AGENT: &str = "opencode/1.0 ompchamber";

const CODEX_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

const STRUCTURED_OUTPUT_NAME: &str = "response";

/// Error shape carrying the fields JS attaches to thrown errors
/// (`statusCode`, `code`, `providerID`, `requiredChars`/`availableChars`);
/// route mapping and callers read them exactly like the JS.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct SmallModelError {
    pub status_code: u16,
    pub message: String,
    pub code: Option<String>,
    pub provider_id: Option<String>,
    pub required_chars: Option<usize>,
    pub available_chars: Option<usize>,
}

impl SmallModelError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status_code: 500,
            message: message.into(),
            code: None,
            provider_id: None,
            required_chars: None,
            available_chars: None,
        }
    }

    pub fn with_status(status_code: u16, message: impl Into<String>) -> Self {
        Self {
            status_code,
            ..Self::internal(message)
        }
    }

    pub fn with_code(status_code: u16, code: &str, message: impl Into<String>) -> Self {
        Self {
            status_code,
            code: Some(code.to_string()),
            ..Self::internal(message)
        }
    }
}

pub type SmallModelResult<T> = Result<T, SmallModelError>;

/// `httpError`: callers need the status to tell "this provider rejected the
/// request shape" (retryable with a different shape) from "this provider is
/// down". Reads as a 500 (JS attaches no `statusCode`).
async fn http_error(response: &FetchResponse, provider: &str) -> SmallModelError {
    let body = response.text();
    let snippet = if body.is_empty() {
        String::new()
    } else {
        format!(": {}", utf16_prefix(&body, 300))
    };
    SmallModelError::internal(format!(
        "{provider} request failed with {}{snippet}",
        response.status
    ))
}

/// `requestSignal`: per-call deadline (the JS caller-abort `signal` seam has
/// no Rust caller yet; the HTTP route passes none either).
fn request_timeout_ms(timeout_ms: Option<u64>) -> u64 {
    timeout_ms
        .filter(|value| *value > 0)
        .unwrap_or(REQUEST_TIMEOUT_MS)
}

/// Google's schema dialect is OpenAPI-flavored and rejects JSON Schema
/// keywords it does not know, so unsupported keys are dropped rather than
/// passed through.
const GOOGLE_UNSUPPORTED_SCHEMA_KEYS: [&str; 6] = [
    "$schema",
    "additionalProperties",
    "definitions",
    "$defs",
    "$ref",
    "strict",
];

pub fn to_google_schema(schema: &Value) -> Value {
    match schema {
        Value::Array(items) => Value::Array(items.iter().map(to_google_schema).collect()),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !GOOGLE_UNSUPPORTED_SCHEMA_KEYS.contains(&key.as_str()))
                .map(|(key, value)| (key.clone(), to_google_schema(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// OpenAI OAuth (ChatGPT plan / codex) token refresh — single-flight, with the
// refreshed token written back to auth.json exactly like OpenCode does.
// ---------------------------------------------------------------------------

type RefreshFuture = Shared<BoxFuture<'static, Result<Value, String>>>;

#[derive(Default)]
struct RefreshGate {
    inflight: Option<RefreshFuture>,
    generation: u64,
}

fn decode_jwt_claims(token: &str) -> Option<Map<String, Value>> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn extract_chatgpt_account_id(access_token: &str) -> Option<String> {
    let claims = decode_jwt_claims(access_token)?;
    let value = claims
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()?;
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

async fn refresh_openai_oauth(deps: &Arc<CallDeps>, entry: &Value) -> SmallModelResult<Value> {
    let request = FetchRequest {
        method: "POST".to_string(),
        url: CODEX_TOKEN_URL.to_string(),
        headers: vec![("Content-Type".to_string(), "application/json".to_string())],
        body: Some(
            json!({
                "grant_type": "refresh_token",
                "refresh_token": entry.get("refresh").and_then(Value::as_str).unwrap_or_default(),
                "client_id": CODEX_CLIENT_ID,
            })
            .to_string()
            .into_bytes(),
        ),
        timeout_ms: 30_000,
    };
    let response = (deps.fetch)(request)
        .await
        .map_err(SmallModelError::internal)?;
    if !(200..300).contains(&response.status) {
        return Err(http_error(&response, "OpenAI token refresh").await);
    }
    let payload: Value = serde_json::from_slice(&response.body)
        .map_err(|_| SmallModelError::internal("OpenAI token refresh returned invalid JSON"))?;
    let access = payload
        .get("access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if access.is_empty() {
        return Err(SmallModelError::internal(
            "OpenAI token refresh returned no access token",
        ));
    }
    let mut refreshed = entry.clone();
    let object = refreshed
        .as_object_mut()
        .ok_or_else(|| SmallModelError::internal("OpenAI OAuth entry is malformed"))?;
    object.insert("type".to_string(), json!("oauth"));
    object.insert("access".to_string(), json!(access));
    let next_refresh = payload
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    if let Some(next_refresh) = next_refresh {
        object.insert("refresh".to_string(), json!(next_refresh));
    }
    let expires_in = payload
        .get("expires_in")
        .and_then(Value::as_f64)
        .filter(|value| *value > 0.0)
        .unwrap_or(3600.0);
    object.insert(
        "expires".to_string(),
        json!(now_ms() as f64 + expires_in * 1000.0),
    );
    let mut auth = deps.auth_store.read().map_err(SmallModelError::internal)?;
    auth.as_object_mut()
        .ok_or_else(|| SmallModelError::internal("OpenCode auth store is malformed"))?
        .insert("openai".to_string(), refreshed.clone());
    deps.auth_store
        .write(&auth)
        .map_err(SmallModelError::internal)?;
    Ok(refreshed)
}

async fn ensure_fresh_openai_oauth(deps: &Arc<CallDeps>, entry: &Value) -> SmallModelResult<Value> {
    let access = entry.get("access").and_then(Value::as_str).unwrap_or("");
    let expires = entry
        .get("expires")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN);
    if !access.is_empty() && expires > now_ms() as f64 {
        return Ok(entry.clone());
    }
    let has_refresh = entry
        .get("refresh")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty());
    if !has_refresh {
        return Err(SmallModelError::internal(
            "OpenAI OAuth entry has no refresh token",
        ));
    }

    // Single-flight: concurrent callers share one refresh (JS module-global
    // `openaiRefreshPromise`); the flight clears itself when it settles.
    let shared = {
        let mut gate = deps.openai_refresh.lock().await;
        if let Some(existing) = &gate.inflight {
            existing.clone()
        } else {
            let deps = Arc::clone(deps);
            let entry = entry.clone();
            let shared: RefreshFuture = tokio::spawn(async move {
                refresh_openai_oauth(&deps, &entry)
                    .await
                    .map_err(|error| error.message)
            })
            .map(|joined| joined.unwrap_or_else(|error| Err(error.to_string())))
            .boxed()
            .shared();
            gate.inflight = Some(shared.clone());
            gate.generation += 1;
            shared
        }
    };
    let result = shared.await.map_err(SmallModelError::internal);
    let mut gate = deps.openai_refresh.lock().await;
    gate.inflight = None;
    result
}

// ---------------------------------------------------------------------------
// Wire formats
// ---------------------------------------------------------------------------

async fn read_response_json(response: &FetchResponse) -> SmallModelResult<Value> {
    serde_json::from_slice(&response.body)
        .map_err(|_| SmallModelError::internal("Provider returned invalid JSON"))
}

async fn call_openai_compatible(
    deps: &CallDeps,
    base_url: &str,
    headers: &[(String, String)],
    model_id: &str,
    prompt: &str,
    system: Option<&str>,
    max_output_tokens: u64,
    provider_label: &str,
    extra_body: Option<&Value>,
    response_schema: Option<&Value>,
    timeout_ms: Option<u64>,
) -> SmallModelResult<String> {
    let trimmed_base = base_url.trim_end_matches('/');
    let thinking_disabled = extra_body
        .and_then(|body| body.get("thinking"))
        .and_then(|thinking| thinking.get("type"))
        .and_then(|value| value.as_str())
        .map(|kind| kind == "disabled")
        .unwrap_or(false);
    tracing::info!(
        target: "small_model",
        provider = provider_label,
        model = model_id,
        max_output_tokens,
        thinking_disabled,
        prompt_chars = utf16_len(prompt),
        system_chars = system.map(utf16_len).unwrap_or(0),
        input_chars = utf16_len(prompt) + system.map(utf16_len).unwrap_or(0),
        "[small-model:diagnostic] request"
    );

    let mut messages = Vec::new();
    if let Some(system) = system.filter(|value| !value.is_empty()) {
        messages.push(json!({ "role": "system", "content": system }));
    }
    messages.push(json!({ "role": "user", "content": prompt }));
    let mut body = json!({
        "model": model_id,
        "messages": messages,
        "max_tokens": max_output_tokens,
        "stream": false,
    });
    if let Some(schema) = response_schema.filter(|schema| !schema.is_null()) {
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": { "name": STRUCTURED_OUTPUT_NAME, "strict": true, "schema": schema },
        });
    }
    if let Some(extra) = extra_body
        && let (Some(body_object), Some(extra_object)) = (body.as_object_mut(), extra.as_object())
    {
        for (key, value) in extra_object {
            body_object.insert(key.clone(), value.clone());
        }
    }

    let response = (deps.fetch)(FetchRequest {
        method: "POST".to_string(),
        url: format!("{trimmed_base}/chat/completions"),
        headers: merge_headers_case_insensitive(
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            Some(headers),
        ),
        body: Some(body.to_string().into_bytes()),
        timeout_ms: request_timeout_ms(timeout_ms),
    })
    .await
    .map_err(SmallModelError::internal)?;

    tracing::info!(
        target: "small_model",
        provider = provider_label,
        model = model_id,
        http_status = response.status,
        ok = (200..300).contains(&response.status),
        "[small-model:diagnostic] response"
    );
    if !(200..300).contains(&response.status) {
        return Err(http_error(&response, provider_label).await);
    }
    let payload = read_response_json(&response).await?;
    let choice = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    let message = choice
        .and_then(|choice| choice.get("message"))
        .cloned()
        .unwrap_or(Value::Null);
    let finish_reason = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let content_chars = message_content_length(&message);
    let reasoning_chars = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .map(utf16_len)
        .unwrap_or(0);
    tracing::info!(
        target: "small_model",
        provider = provider_label,
        model = model_id,
        finish_reason = finish_reason.as_deref(),
        content_chars,
        reasoning_chars,
        "[small-model:diagnostic] completion"
    );

    // Providers disagree on the content shape: plain string, an array of
    // typed parts, or (thinking models) an empty content with the budget
    // spent on reasoning_content.
    let text = message_content_text(&message);
    let reasoning_only = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty());
    if text.trim().is_empty() && (finish_reason.as_deref() == Some("length") || reasoning_only) {
        // The model produced only reasoning, or was cut off before answering.
        // This is a budget problem, not a transport problem.
        let suffix = finish_reason
            .as_deref()
            .map(|reason| format!(" (finish_reason: {reason})"))
            .unwrap_or_default();
        return Err(SmallModelError {
            status_code: 500,
            message: format!(
                "{provider_label} spent the output budget on reasoning and returned no answer{suffix}"
            ),
            code: Some("output-exhausted".to_string()),
            provider_id: None,
            required_chars: None,
            available_chars: None,
        });
    }
    if text.trim().is_empty() {
        return Err(SmallModelError::internal(format!(
            "{provider_label} returned no message content"
        )));
    }
    Ok(text)
}

fn message_content_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| part.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect(),
        _ => String::new(),
    }
}

fn message_content_length(message: &Value) -> usize {
    match message.get("content") {
        Some(Value::String(text)) => utf16_len(text),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| {
                part.get("text")
                    .and_then(Value::as_str)
                    .map(utf16_len)
                    .unwrap_or(0)
            })
            .sum(),
        _ => 0,
    }
}

async fn call_openai_responses(
    deps: &CallDeps,
    base_url: &str,
    headers: &[(String, String)],
    model_id: &str,
    prompt: &str,
    system: Option<&str>,
    max_output_tokens: u64,
    provider_label: &str,
    response_schema: Option<&Value>,
    timeout_ms: Option<u64>,
) -> SmallModelResult<String> {
    let trimmed_base = base_url.trim_end_matches('/');
    let mut body = json!({
        "model": model_id,
        "input": [{
            "role": "user",
            "content": [{ "type": "input_text", "text": prompt }],
        }],
        "max_output_tokens": max_output_tokens,
        "stream": false,
        "store": false,
    });
    if let Some(system) = system.filter(|value| !value.is_empty()) {
        body["instructions"] = json!(system);
    }
    if let Some(schema) = response_schema.filter(|schema| !schema.is_null()) {
        body["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": STRUCTURED_OUTPUT_NAME,
                "strict": true,
                "schema": schema,
            }
        });
    }
    let response = (deps.fetch)(FetchRequest {
        method: "POST".to_string(),
        url: format!("{trimmed_base}/responses"),
        headers: spread_headers(
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            headers,
        ),
        body: Some(body.to_string().into_bytes()),
        timeout_ms: request_timeout_ms(timeout_ms),
    })
    .await
    .map_err(SmallModelError::internal)?;
    if !(200..300).contains(&response.status) {
        return Err(http_error(&response, provider_label).await);
    }
    let payload = read_response_json(&response).await?;
    let text = match payload.get("output_text") {
        Some(Value::String(text)) => text.clone(),
        _ => payload
            .get("output")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .flat_map(|item| {
                        item.get("content")
                            .and_then(Value::as_array)
                            .map(Vec::as_slice)
                            .unwrap_or(&[])
                            .iter()
                    })
                    .map(|part| {
                        if part.get("type").and_then(Value::as_str) == Some("output_text") {
                            part.get("text").and_then(Value::as_str).unwrap_or("")
                        } else {
                            ""
                        }
                    })
                    .collect::<String>()
            })
            .unwrap_or_default(),
    };
    if text.trim().is_empty() {
        return Err(SmallModelError::internal(format!(
            "{provider_label} returned no text output"
        )));
    }
    Ok(text)
}

async fn call_messages(
    deps: &CallDeps,
    url: String,
    headers: Vec<(String, String)>,
    model_id: &str,
    prompt: &str,
    system: Option<&str>,
    max_output_tokens: u64,
    provider_label: &str,
    response_schema: Option<&Value>,
    timeout_ms: Option<u64>,
) -> SmallModelResult<String> {
    let mut body = json!({
        "model": model_id,
        "max_tokens": max_output_tokens,
        "messages": [{ "role": "user", "content": prompt }],
    });
    if let Some(system) = system.filter(|value| !value.is_empty()) {
        body["system"] = json!(system);
    }
    if let Some(schema) = response_schema.filter(|schema| !schema.is_null()) {
        // The messages API has no response_format; a forced single-tool call
        // is the supported way to get schema-shaped output.
        body["tools"] = json!([{
            "name": STRUCTURED_OUTPUT_NAME,
            "description": "Return the answer in the required structure.",
            "input_schema": schema,
        }]);
        body["tool_choice"] = json!({ "type": "tool", "name": STRUCTURED_OUTPUT_NAME });
    }
    let response = (deps.fetch)(FetchRequest {
        method: "POST".to_string(),
        url,
        headers: spread_headers(
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
            ],
            &headers,
        ),
        body: Some(body.to_string().into_bytes()),
        timeout_ms: request_timeout_ms(timeout_ms),
    })
    .await
    .map_err(SmallModelError::internal)?;
    if !(200..300).contains(&response.status) {
        return Err(http_error(&response, provider_label).await);
    }
    let payload = read_response_json(&response).await?;

    if let Some(_schema) = response_schema.filter(|schema| !schema.is_null()) {
        let tool_use = payload
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .find(|part| {
                part.get("type").and_then(Value::as_str) == Some("tool_use")
                    && part.get("name").and_then(Value::as_str) == Some(STRUCTURED_OUTPUT_NAME)
            });
        let input = tool_use
            .and_then(|tool| tool.get("input"))
            .filter(|input| input.as_object().is_some());
        let Some(input) = input else {
            return Err(SmallModelError::internal(format!(
                "{provider_label} returned no structured output"
            )));
        };
        return serde_json::to_string(input)
            .map_err(|_| SmallModelError::internal("structured output serialization failed"));
    }

    let text: String = payload
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .map(|part| part.get("text").and_then(Value::as_str).unwrap_or(""))
        .collect();
    if text.is_empty() {
        return Err(SmallModelError::internal(format!(
            "{provider_label} returned no text content"
        )));
    }
    Ok(text)
}

async fn call_anthropic(
    deps: &CallDeps,
    api_key: &str,
    base_url: Option<&str>,
    model_id: &str,
    prompt: &str,
    system: Option<&str>,
    max_output_tokens: u64,
    response_schema: Option<&Value>,
    timeout_ms: Option<u64>,
) -> SmallModelResult<String> {
    // Matches @ai-sdk/anthropic: baseURL is the full API prefix (commonly
    // already ending in /v1), so /messages is appended as-is.
    let base = base_url
        .filter(|value| !value.is_empty())
        .unwrap_or("https://api.anthropic.com/v1");
    call_messages(
        deps,
        format!("{}/messages", base.trim_end_matches('/')),
        vec![
            ("x-api-key".to_string(), api_key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ],
        model_id,
        prompt,
        system,
        max_output_tokens,
        "Anthropic",
        response_schema,
        timeout_ms,
    )
    .await
}

async fn get_copilot_endpoint(
    deps: &CallDeps,
    base_url: &str,
    headers: &[(String, String)],
    model_id: &str,
) -> SmallModelResult<&'static str> {
    let response = (deps.fetch)(FetchRequest {
        method: "GET".to_string(),
        url: format!("{}/models", base_url.trim_end_matches('/')),
        headers: spread_headers(
            vec![("Accept".to_string(), "application/json".to_string())],
            headers,
        ),
        body: None,
        timeout_ms: COPILOT_MODELS_TIMEOUT_MS,
    })
    .await
    .map_err(SmallModelError::internal)?;
    if !(200..300).contains(&response.status) {
        return Err(http_error(&response, "GitHub Copilot models").await);
    }
    let payload = read_response_json(&response)
        .await
        .map_err(|_| SmallModelError::internal("GitHub Copilot models returned invalid JSON"))?;
    let Some(data) = payload.get("data").and_then(Value::as_array) else {
        return Err(SmallModelError::internal(
            "GitHub Copilot models returned an invalid model list",
        ));
    };
    let Some(model) = data.iter().find(|item| {
        item.as_object()
            .is_some_and(|object| object.get("id").and_then(Value::as_str) == Some(model_id))
    }) else {
        return Err(SmallModelError::internal(format!(
            "GitHub Copilot model \"{model_id}\" was not returned by /models"
        )));
    };
    let endpoints = model.get("supported_endpoints");
    if endpoints.is_none() {
        return Ok("chat");
    }
    let Some(endpoints) = endpoints.and_then(Value::as_array) else {
        return Err(SmallModelError::internal(format!(
            "GitHub Copilot model \"{model_id}\" returned invalid endpoint metadata"
        )));
    };
    let contains = |name: &str| {
        endpoints
            .iter()
            .any(|endpoint| endpoint.as_str() == Some(name))
    };
    if contains("/v1/messages") {
        return Ok("messages");
    }
    if contains("/responses") {
        return Ok("responses");
    }
    if contains("/chat/completions") {
        return Ok("chat");
    }
    Err(SmallModelError::internal(format!(
        "GitHub Copilot model \"{model_id}\" has no supported text endpoint"
    )))
}

async fn call_google(
    deps: &CallDeps,
    api_key: &str,
    model_id: &str,
    prompt: &str,
    system: Option<&str>,
    max_output_tokens: u64,
    response_schema: Option<&Value>,
    timeout_ms: Option<u64>,
) -> SmallModelResult<String> {
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
        uri_encode_component(model_id)
    );
    let lower_model_id = model_id.to_lowercase();
    let thinking_config: Option<Value> = if lower_model_id.starts_with("gemini-3") {
        Some(json!({
            "thinkingLevel": if lower_model_id.contains("flash") { "minimal" } else { "low" }
        }))
    } else if lower_model_id.starts_with("gemini-2") {
        Some(json!({ "thinkingBudget": 0 }))
    } else {
        None
    };

    let mut generation_config = json!({ "maxOutputTokens": max_output_tokens });
    if let Some(thinking_config) = &thinking_config {
        generation_config["thinkingConfig"] = thinking_config.clone();
    }
    if let Some(schema) = response_schema.filter(|schema| !schema.is_null()) {
        generation_config["responseMimeType"] = json!("application/json");
        generation_config["responseSchema"] = to_google_schema(schema);
    }
    let mut body = json!({
        "contents": [{ "role": "user", "parts": [{ "text": prompt }] }],
        "generationConfig": generation_config,
    });
    if let Some(system) = system.filter(|value| !value.is_empty()) {
        body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
    }

    let response = (deps.fetch)(FetchRequest {
        method: "POST".to_string(),
        url,
        headers: vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            ("x-goog-api-key".to_string(), api_key.to_string()),
        ],
        body: Some(body.to_string().into_bytes()),
        timeout_ms: request_timeout_ms(timeout_ms),
    })
    .await
    .map_err(SmallModelError::internal)?;
    if !(200..300).contains(&response.status) {
        return Err(http_error(&response, "Google").await);
    }
    let payload = read_response_json(&response).await?;
    let text: String = payload
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first())
        .and_then(|candidate| candidate.get("content"))
        .and_then(|content| content.get("parts"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|part| part.get("text").and_then(Value::as_str).unwrap_or(""))
        .collect();
    if text.is_empty() {
        return Err(SmallModelError::internal("Google returned no text content"));
    }
    Ok(text)
}

/// ChatGPT-plan traffic goes to the codex backend, which only speaks the
/// streaming Responses API — collect the output_text deltas from the SSE body.
async fn call_codex_responses(
    deps: &CallDeps,
    access_token: &str,
    account_id: Option<&str>,
    model_id: &str,
    prompt: &str,
    system: Option<&str>,
    timeout_ms: Option<u64>,
) -> SmallModelResult<String> {
    let mut body = json!({
        "model": model_id,
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": prompt }],
        }],
        // The codex backend rejects max_output_tokens (OpenCode forces it to
        // undefined for this provider too).
        "stream": true,
        "store": false,
    });
    if let Some(system) = system.filter(|value| !value.is_empty()) {
        body["instructions"] = json!(system);
    }
    let mut headers = vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "text/event-stream".to_string()),
        (
            "Authorization".to_string(),
            format!("Bearer {access_token}"),
        ),
        ("originator".to_string(), "opencode".to_string()),
        ("User-Agent".to_string(), USER_AGENT.to_string()),
    ];
    if let Some(account_id) = account_id.filter(|value| !value.is_empty()) {
        headers.push(("ChatGPT-Account-Id".to_string(), account_id.to_string()));
    }
    let response = (deps.fetch)(FetchRequest {
        method: "POST".to_string(),
        url: CODEX_RESPONSES_URL.to_string(),
        headers,
        body: Some(body.to_string().into_bytes()),
        timeout_ms: request_timeout_ms(timeout_ms),
    })
    .await
    .map_err(SmallModelError::internal)?;
    if !(200..300).contains(&response.status) {
        return Err(http_error(&response, "OpenAI (ChatGPT plan)").await);
    }

    let raw = response.text();
    let mut text = String::new();
    let mut completed_text = String::new();
    for line in raw.split('\n') {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let event_type = event.get("type").and_then(Value::as_str);
        if event_type == Some("response.output_text.delta")
            && let Some(delta) = event.get("delta").and_then(Value::as_str)
        {
            text.push_str(delta);
        }
        if event_type == Some("response.output_text.done")
            && let Some(done) = event.get("text").and_then(Value::as_str)
        {
            completed_text = done.to_string();
        }
        if event_type == Some("response.failed") || event_type == Some("error") {
            let message = event
                .get("response")
                .and_then(|response| response.get("error"))
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .or_else(|| event.get("message").and_then(Value::as_str))
                .unwrap_or("response failed");
            return Err(SmallModelError::internal(format!(
                "OpenAI (ChatGPT plan) stream error: {message}"
            )));
        }
    }
    let result = if completed_text.is_empty() {
        text
    } else {
        completed_text
    };
    if result.is_empty() {
        return Err(SmallModelError::internal(
            "OpenAI (ChatGPT plan) returned no text output",
        ));
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Custom provider configuration support
// ---------------------------------------------------------------------------

/// `resolveConfigValue`: `{env:NAME}` and `{file:path}` substitutions. Only
/// the relative-file branch consults the config layers (which layer declared
/// the reference); everything else is direct.
fn resolve_config_value(
    config: &ConfigReader,
    value: &str,
    working_directory: Option<&str>,
    provider_id: &str,
    header_name: Option<&str>,
) -> Result<Option<String>, String> {
    // `{env:NAME}` (case-insensitive pattern, trimmed name/value).
    let trimmed = value.trim();
    if let Some(name) = extract_braced(trimmed, "env:") {
        return Ok(std::env::var(name.trim())
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()));
    }
    let Some(configured_path) = extract_braced(trimmed, "file:") else {
        return Ok(Some(trimmed.to_string()));
    };
    let configured_path = configured_path.trim();

    let resolved_path = if configured_path == "~" {
        crate::config::home_dir().unwrap_or_else(|| std::path::PathBuf::from(configured_path))
    } else if configured_path.starts_with("~/") || configured_path.starts_with("~\\") {
        crate::config::home_dir()
            .map(|home| home.join(&configured_path[2..]))
            .unwrap_or_else(|| std::path::PathBuf::from(configured_path))
    } else if std::path::Path::new(configured_path).is_absolute() {
        std::path::PathBuf::from(configured_path)
    } else {
        let layers = config(working_directory);
        let source = [
            (&layers.custom_config, layers.custom_path.as_deref()),
            (&layers.project_config, layers.project_path.as_deref()),
            (&layers.user_config, layers.user_path.as_deref()),
        ]
        .into_iter()
        .find(|(config_layer, _)| {
            let Some(options) = config_layer
                .get("provider")
                .and_then(|provider| provider.get(provider_id))
                .and_then(|provider| provider.get("options"))
            else {
                return false;
            };
            match header_name {
                Some(header_name) => {
                    options
                        .get("headers")
                        .and_then(|headers| headers.get(header_name))
                        .and_then(Value::as_str)
                        == Some(value)
                }
                None => options.get("apiKey").and_then(Value::as_str) == Some(value),
            }
        });
        let base = source
            .and_then(|(_, path)| path)
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| {
                working_directory
                    .filter(|value| !value.is_empty())
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                    })
            });
        base.join(configured_path)
    };

    match std::fs::read_to_string(&resolved_path) {
        Ok(content) if !content.trim().is_empty() => Ok(Some(content.trim().to_string())),
        _ => Err(format!(
            "Failed to resolve configured {} file for provider \"{provider_id}\"",
            header_name
                .map(|name| format!("header \"{name}\""))
                .unwrap_or_else(|| "apiKey".to_string())
        )),
    }
}

/// JS `/^\{env:([^}]+)\}$/i`-style single match: the value must be exactly
/// `{prefix:…}` for the (case-insensitive) prefix.
fn extract_braced<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let value = value.strip_prefix('{')?.strip_suffix('}')?;
    let (kind, rest) = value.split_once(':')?;
    if !kind.eq_ignore_ascii_case(prefix.trim_end_matches(':')) {
        return None;
    }
    Some(rest)
}

fn read_configured_headers(
    config: &ConfigReader,
    provider_config: &Value,
    working_directory: Option<&str>,
    provider_id: &str,
) -> Result<Option<Vec<(String, String)>>, String> {
    let Some(configured) = provider_config
        .get("options")
        .and_then(|options| options.get("headers"))
    else {
        return Ok(None);
    };
    if !is_plain_object(configured) {
        return Ok(None);
    }
    let Some(configured) = configured.as_object() else {
        return Ok(None);
    };
    let mut headers = Vec::new();
    for (name, value) in configured {
        // Config headers are strings; a malformed entry is skipped rather
        // than stringified into a header the gateway would reject.
        let Some(value) = value.as_str() else {
            continue;
        };
        // Resolution failures abort the whole header read (JS throws out of
        // readProviderConfig's try, dropping the provider config).
        if let Some(resolved) = resolve_config_value(
            config,
            value.trim(),
            working_directory,
            provider_id,
            Some(name),
        )? {
            headers.push((name.clone(), resolved));
        }
    }
    Ok((!headers.is_empty()).then_some(headers))
}

#[derive(Default)]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub headers: Option<Vec<(String, String)>>,
    /// Shape the config-supplied key as a regular api-key auth entry so it
    /// can win the precedence check and flow through the dispatch's
    /// `entry.type === 'api'` branch unchanged.
    pub auth: Option<Value>,
}

/// `readProviderConfig`. Provider config is non-essential — any failure
/// (a `{file:…}` key/header that cannot be resolved included) drops the
/// whole entry and continues with catalog-only resolution, exactly like the
/// JS `try { … } catch { return null; }`.
pub fn read_provider_config(
    config: &ConfigReader,
    working_directory: Option<&str>,
    provider_id: &str,
) -> ProviderConfig {
    read_provider_config_inner(config, working_directory, provider_id).unwrap_or_default()
}

fn read_provider_config_inner(
    config: &ConfigReader,
    working_directory: Option<&str>,
    provider_id: &str,
) -> Result<ProviderConfig, String> {
    let merged = config(working_directory).merged;
    let Some(provider_config) = merged
        .get("provider")
        .and_then(|provider| provider.get(provider_id))
        .filter(|provider| provider.as_object().is_some())
    else {
        return Ok(ProviderConfig::default());
    };
    let base_url = provider_config
        .get("options")
        .and_then(|options| options.get("baseURL"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let raw_api_key = provider_config
        .get("options")
        .and_then(|options| options.get("apiKey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let api_key = match raw_api_key {
        Some(raw) => resolve_config_value(config, raw, working_directory, provider_id, None)?,
        None => None,
    };
    let headers = read_configured_headers(config, provider_config, working_directory, provider_id)?;
    Ok(ProviderConfig {
        base_url,
        headers,
        auth: api_key.map(|key| json!({ "type": "api", "key": key })),
    })
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Providers reached through a dedicated wire format below: a token exchange,
/// an OAuth refresh, or a non-bearer header. OpenCode's runtime
/// `options.apiKey` is not the value those branches need — the ChatGPT-plan
/// `openai` login is the clearest case, where the runtime key is an OAuth
/// access token that api.openai.com answers with 401 — so the runtime
/// credential never stands in for them, and the runtime listing skips them
/// because the auth.json scan already covers them.
pub const DEDICATED_WIRE_FORMAT_PROVIDERS: [&str; 5] =
    ["github-copilot", "copilot", "openai", "anthropic", "google"];

pub fn is_dedicated_wire_format_provider(provider_id: &str) -> bool {
    DEDICATED_WIRE_FORMAT_PROVIDERS.contains(&provider_id)
}

/// `runtimeCredential`: the runtime credential shaped as an auth entry, or
/// `None` when the provider owns its credential handling or OpenCode reports
/// nothing usable.
pub fn runtime_credential(provider_id: &str, runtime: Option<&RuntimeProvider>) -> Option<Value> {
    if is_dedicated_wire_format_provider(provider_id) {
        return None;
    }
    let key = runtime?.api_key.as_deref().filter(|key| !key.is_empty())?;
    Some(json!({ "type": "api", "key": key }))
}

/// Injectable dependencies (the seams the JS suites mock module-by-module).
pub struct CallDeps {
    pub fetch: Fetch,
    pub config: ConfigReader,
    pub auth_store: Arc<dyn AuthStore>,
    pub runtime: Arc<RuntimeProviders>,
    openai_refresh: tokio::sync::Mutex<RefreshGate>,
}

impl CallDeps {
    pub fn new(
        fetch: Fetch,
        config: ConfigReader,
        auth_store: Arc<dyn AuthStore>,
        runtime: Arc<RuntimeProviders>,
    ) -> Arc<Self> {
        Arc::new(Self {
            fetch,
            config,
            auth_store,
            runtime,
            openai_refresh: tokio::sync::Mutex::new(RefreshGate::default()),
        })
    }
}

/// `resolveProviderLogin`: the same credential resolution the request path
/// uses — config `provider.<id>.options.apiKey` wins, then the runtime
/// credential OpenCode resolved for a plugin provider, then the auth.json
/// entry. Callers that need to refuse before spending a request must use
/// this rather than inventing a second rule.
pub async fn resolve_provider_login(
    deps: &CallDeps,
    auth: &Value,
    working_directory: Option<&str>,
    provider_id: &str,
) -> Option<Value> {
    let provider_config = read_provider_config(&deps.config, working_directory, provider_id);
    if provider_config.auth.is_some() {
        return provider_config.auth;
    }
    if let Some(runtime) = runtime_credential(
        provider_id,
        deps.runtime.provider(provider_id).await.as_ref(),
    ) {
        return Some(runtime);
    }
    get_auth_entry_for_provider(auth, provider_id).cloned()
}

pub struct CallParams<'a> {
    pub auth: &'a Value,
    pub catalog: &'a Value,
    pub working_directory: Option<&'a str>,
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub prompt: &'a str,
    pub system: Option<&'a str>,
    pub max_output_tokens: Option<u64>,
    pub response_schema: Option<&'a Value>,
    pub timeout_ms: Option<u64>,
}

/// `callSmallModel`.
pub async fn call_small_model(
    deps: &Arc<CallDeps>,
    params: CallParams<'_>,
) -> SmallModelResult<String> {
    let CallParams {
        auth,
        catalog,
        working_directory,
        provider_id,
        model_id,
        prompt,
        system,
        max_output_tokens,
        response_schema,
        timeout_ms,
    } = params;
    let tokens = max_output_tokens
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let provider_config = read_provider_config(&deps.config, working_directory, provider_id);
    let runtime_provider = deps.runtime.provider(provider_id).await;
    // Match OpenCode's resolveSDK precedence: config `provider.<id>.options`
    // wins, then what OpenCode itself resolved at runtime (the only place a
    // plugin's credential exists), and the auth.json entry last.
    let entry = provider_config
        .auth
        .clone()
        .or_else(|| runtime_credential(provider_id, runtime_provider.as_ref()))
        .or_else(|| get_auth_entry_for_provider(auth, provider_id).cloned());
    let Some(entry) = entry else {
        // Structured so the walkthrough (and any other caller) can show a
        // blocker instead of a raw 500 banner with this developer-facing
        // sentence.
        return Err(SmallModelError {
            status_code: 401,
            message: format!("No OpenCode login found for provider \"{provider_id}\""),
            code: Some("no-provider-login".to_string()),
            provider_id: Some(provider_id.to_string()),
            required_chars: None,
            available_chars: None,
        });
    };

    if provider_id == "github-copilot" {
        return call_copilot(
            deps,
            &entry,
            model_id,
            prompt,
            system,
            tokens,
            response_schema,
            timeout_ms,
        )
        .await;
    }

    if provider_id == "openai" && entry.get("type").and_then(Value::as_str) == Some("oauth") {
        // The codex backend speaks only the streaming Responses API and
        // rejects the structured-output fields, so a schema request fails
        // loudly here instead of silently returning free-form prose.
        if response_schema.is_some_and(|schema| !schema.is_null()) {
            return Err(SmallModelError::with_code(
                500,
                "structured-output-unsupported",
                "The ChatGPT-plan OpenAI login does not support structured output — choose another small model",
            ));
        }
        let fresh = ensure_fresh_openai_oauth(deps, &entry).await?;
        let access = fresh
            .get("access")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let account_id = fresh
            .get("accountId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|value| !value.is_empty())
            .or_else(|| extract_chatgpt_account_id(access));
        return call_codex_responses(
            deps,
            access,
            account_id.as_deref(),
            model_id,
            prompt,
            system,
            timeout_ms,
        )
        .await;
    }

    let kind = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let api_key = match kind {
        "api" => entry.get("key").and_then(Value::as_str),
        "wellknown" => entry.get("token").and_then(Value::as_str),
        _ => entry.get("access").and_then(Value::as_str),
    }
    .unwrap_or_default();
    if api_key.is_empty() {
        return Err(SmallModelError::internal(format!(
            "OpenCode login for \"{provider_id}\" has no usable credential"
        )));
    }

    if provider_id == "anthropic" {
        return call_anthropic(
            deps,
            api_key,
            provider_config.base_url.as_deref(),
            model_id,
            prompt,
            system,
            tokens,
            response_schema,
            timeout_ms,
        )
        .await;
    }
    if provider_id == "google" {
        return call_google(
            deps,
            api_key,
            model_id,
            prompt,
            system,
            tokens,
            response_schema,
            timeout_ms,
        )
        .await;
    }

    // Everything else: OpenAI-compatible chat completions against the
    // catalog's base URL for that provider (openai itself included). When a
    // custom provider is not in the catalog (e.g. a user-configured
    // OpenAI-compatible proxy), fall back to its baseURL from the OpenCode
    // provider config, then to the endpoint OpenCode resolved at runtime —
    // which for a plugin provider is the only place it exists. The openai
    // provider also respects provider.openai.options.baseURL.
    let provider = crate::small_model::catalog::get_catalog_provider(catalog, provider_id);
    let default_openai_url = "https://api.openai.com/v1";
    let base_url = provider_config
        .base_url
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            if provider_id == "openai" {
                Some(default_openai_url.to_string())
            } else {
                None
            }
        })
        .or_else(|| {
            runtime_provider
                .as_ref()
                .and_then(|runtime| runtime.base_url.clone())
        })
        .or_else(|| {
            provider
                .and_then(|provider| provider.get("api"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
    let Some(base_url) = base_url else {
        return Err(SmallModelError::internal(format!(
            "Provider \"{provider_id}\" has no known API base URL"
        )));
    };

    // Thinking models burn the output budget on reasoning and leave content
    // empty — disable thinking where a wire-format switch exists. There is
    // NO universal parameter: unknown body fields 400 on some providers, so
    // this stays an explicit allowlist. Models without a switch (DeepSeek,
    // Qwen, Kimi, …) just get the generous output budget.
    let lower_model = model_id.to_lowercase();
    let supports_thinking_toggle = provider_id.contains("zai")
        || provider_id.contains("zhipu")
        || lower_model.contains("glm")
        || lower_model.contains("minimax-m3");
    let extra_body =
        supports_thinking_toggle.then(|| json!({ "thinking": { "type": "disabled" } }));

    call_openai_compatible(
        deps,
        &base_url,
        // Configured headers last: a gateway that authenticates on its own
        // header must be able to override the bearer default rather than sit
        // beside it.
        &merge_headers_case_insensitive(
            vec![("Authorization".to_string(), format!("Bearer {api_key}"))],
            provider_config.headers.as_deref(),
        ),
        model_id,
        prompt,
        system,
        tokens,
        provider
            .and_then(|provider| provider.get("name"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(provider_id),
        extra_body.as_ref(),
        response_schema,
        timeout_ms,
    )
    .await
}

async fn call_copilot(
    deps: &CallDeps,
    entry: &Value,
    model_id: &str,
    prompt: &str,
    system: Option<&str>,
    tokens: u64,
    response_schema: Option<&Value>,
    timeout_ms: Option<u64>,
) -> SmallModelResult<String> {
    // OpenCode uses the stored device-OAuth token directly as the bearer —
    // access === refresh, no exchange, no expiry.
    let token = ["refresh", "access", "key"]
        .iter()
        .map(|field| {
            entry
                .get(*field)
                .and_then(Value::as_str)
                .unwrap_or_default()
        })
        .find(|value| !value.is_empty());
    let Some(token) = token else {
        return Err(SmallModelError::internal(
            "GitHub Copilot login has no token",
        ));
    };
    let enterprise = entry
        .get("enterpriseUrl")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|url| {
            let host = url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/');
            format!("https://copilot-api.{host}")
        });
    let base_url = enterprise.unwrap_or_else(|| "https://api.githubcopilot.com".to_string());
    let auth_headers = vec![
        ("Authorization".to_string(), format!("Bearer {token}")),
        ("User-Agent".to_string(), USER_AGENT.to_string()),
        ("X-GitHub-Api-Version".to_string(), "2026-06-01".to_string()),
    ];
    let mut headers = auth_headers.clone();
    headers.push((
        "Openai-Intent".to_string(),
        "conversation-edits".to_string(),
    ));
    headers.push(("x-initiator".to_string(), "agent".to_string()));

    let endpoint = get_copilot_endpoint(deps, &base_url, &auth_headers, model_id).await?;
    match endpoint {
        "messages" => {
            let mut message_headers = headers.clone();
            message_headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
            call_messages(
                deps,
                format!("{}/v1/messages", base_url.trim_end_matches('/')),
                message_headers,
                model_id,
                prompt,
                system,
                tokens,
                "GitHub Copilot",
                response_schema,
                timeout_ms,
            )
            .await
        }
        "responses" => {
            call_openai_responses(
                deps,
                &base_url,
                &headers,
                model_id,
                prompt,
                system,
                tokens,
                "GitHub Copilot",
                response_schema,
                timeout_ms,
            )
            .await
        }
        _ => {
            call_openai_compatible(
                deps,
                &base_url,
                &headers,
                model_id,
                prompt,
                system,
                tokens,
                "GitHub Copilot",
                None,
                response_schema,
                timeout_ms,
            )
            .await
        }
    }
}
