//! Port of `server/lib/magic-prompts/{runtime,routes}.js`.
//!
//! Persisted prompt overrides at `<data-dir>/magic-prompts.json`. Read
//! failures (missing or malformed) degrade to the empty default with a
//! warning — the JS module treats this cache as non-authoritative. Writes
//! are serialized (JS `writeLock` chain) and pretty-printed 2-space JSON.
//! 本模块是 `server/lib/magic-prompts/{runtime,routes}.js` 的 Rust 移植。
//!
//! prompt 覆盖持久化在 `<data-dir>/magic-prompts.json`。读取失败（文件
//! 缺失或损坏）降级为空默认值并告警——JS 模块视该缓存为非权威数据。
//! 写入按写锁串行化（对应 JS 的 writeLock 链），并以 2 空格缩进的
//! pretty JSON 落盘。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;

/// 持久化文件的版本号。
const FILE_VERSION: u64 = 1;
/// 单条 prompt 文本的最大长度（字节）。
const MAX_PROMPT_TEXT_LENGTH: usize = 200_000;
/// prompt id 的最大长度。
const PROMPT_ID_MAX: usize = 160;

/// 持久化的 prompt 覆盖状态：prompt id 到覆盖文本的有序映射。
#[derive(Debug, Clone, Default)]
pub struct PromptState {
    /// prompt id -> 覆盖文本（BTreeMap 保证确定性顺序）。
    pub overrides: BTreeMap<String, String>,
}

/// 状态的序列化。
impl PromptState {
    /// 序列化为带 version 字段的文件结构。
    fn to_json(&self) -> Value {
        json!({ "version": FILE_VERSION, "overrides": self.overrides })
    }
}

/// 对应 JS 的 PROMPT_ID_PATTERN（/^[a-z0-9._-]{1,160}$/）。
/// `PROMPT_ID_PATTERN`: `/^[a-z0-9._-]{1,160}$/`.
fn is_valid_prompt_id(id: &str) -> bool {
    let len = id.len();
    (1..=PROMPT_ID_MAX).contains(&len)
        && id
            .bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
}

/// 是否为 .visible 结尾的 prompt id（此类 prompt 的文本不允许为空白）。
fn is_visible_prompt_id(id: &str) -> bool {
    id.ends_with(".visible")
}

/// 对应 JS sanitizeOverrides：只保留符合 id 模式且值为字符串的条目。
/// sanitizeOverrides: keep only id-pattern keys with string values.
fn sanitize_overrides(value: &Value) -> BTreeMap<String, String> {
    let mut overrides = BTreeMap::new();
    if let Some(map) = value.get("overrides").and_then(Value::as_object) {
        for (id, entry) in map {
            if is_valid_prompt_id(id)
                && let Some(text) = entry.as_str()
            {
                overrides.insert(id.clone(), text.to_string());
            }
        }
    }
    overrides
}

/// 路由共享状态：持久化文件路径与串行化写入的锁。
#[derive(Clone)]
struct ModuleState {
    /// 持久化文件路径。
    file_path: PathBuf,
    /// 串行化写操作的互斥锁（对应 JS 的 writeLock 链）。
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

/// 状态的读取、写入与读-改-写复合操作。
impl ModuleState {
    /// 读取持久化状态：文件缺失、损坏或读取失败都降级为空默认（记 warn）。
    async fn read_prompt_state(&self) -> PromptState {
        match tokio::fs::read_to_string(&self.file_path).await {
            Ok(raw) => match serde_json::from_str::<Value>(&raw) {
                Ok(parsed) => PromptState {
                    overrides: sanitize_overrides(&parsed),
                },
                Err(error) => {
                    tracing::warn!("Failed to read magic prompts file: {error}");
                    PromptState::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PromptState::default(),
            Err(e) => {
                tracing::warn!("Failed to read magic prompts file: {e}");
                PromptState::default()
            }
        }
    }

    /// 把状态以 2 空格缩进 pretty JSON 写入文件；父目录缺失时自动创建。
    async fn write_prompt_state(&self, state: &PromptState) -> std::io::Result<()> {
        if let Some(parent) = self.file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(
            &self.file_path,
            serde_json::to_string_pretty(&state.to_json())?,
        )
        .await
    }

    /// 串行化的读-改-写：在写锁内读取当前状态、应用 mutator、落盘并
    /// 返回新状态；写失败以 Err(错误描述) 返回。
    async fn persist<F>(&self, mutator: F) -> Result<PromptState, String>
    where
        F: FnOnce(PromptState) -> PromptState,
    {
        let _guard = self.write_lock.lock().await;
        let current = self.read_prompt_state().await;
        let next = mutator(current);
        self.write_prompt_state(&next)
            .await
            .map_err(|e| e.to_string())?;
        Ok(next)
    }

    /// 设置单条覆盖：校验 id 模式、text 必须是字符串、.visible 的文本
    /// 不能为空白、长度不超过上限；通过后持久化并返回新状态。
    async fn set_override(&self, id: &str, text: Option<&str>) -> Result<PromptState, String> {
        let normalized = id.trim();
        if !is_valid_prompt_id(normalized) {
            return Err("Invalid prompt id".to_string());
        }
        let Some(text) = text else {
            return Err("Prompt text must be a string".to_string());
        };
        if is_visible_prompt_id(normalized) && text.trim().is_empty() {
            return Err("Visible prompt text cannot be empty".to_string());
        }
        if text.len() > MAX_PROMPT_TEXT_LENGTH {
            return Err("Prompt text is too long".to_string());
        }
        let key = normalized.to_string();
        let value = text.to_string();
        self.persist(|state| PromptState {
            overrides: {
                let mut overrides = state.overrides;
                overrides.insert(key, value);
                overrides
            },
        })
        .await
    }

    /// 删除单条覆盖：id 必须符合模式；条目不存在时同样成功（幂等）。
    async fn reset_override(&self, id: &str) -> Result<PromptState, String> {
        let normalized = id.trim();
        if !is_valid_prompt_id(normalized) {
            return Err("Invalid prompt id".to_string());
        }
        let key = normalized.to_string();
        self.persist(|state| PromptState {
            overrides: {
                let mut overrides = state.overrides;
                overrides.remove(&key);
                overrides
            },
        })
        .await
    }

    /// 清空全部覆盖（保留空的文件结构）。
    async fn reset_all_overrides(&self) -> Result<PromptState, String> {
        self.persist(|_| PromptState::default()).await
    }
}

/// 构造 {"error": message} 形态的 JSON 错误响应。
fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// 对应 JS 路由的状态映射：消息含 "Invalid prompt id"、"too long"、
/// "cannot be empty" 映射 400，其余 500。
/// JS routes map these message fragments to 400; anything else is 500.
fn status_for(message: &str) -> StatusCode {
    if message.contains("Invalid prompt id")
        || message.contains("too long")
        || message.contains("cannot be empty")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// GET /api/magic-prompts：返回当前持久化的 prompt 覆盖（读取失败降级
/// 为空默认）。
async fn get_prompts(State(state): State<ModuleState>) -> Response {
    Json(state.read_prompt_state().await.to_json()).into_response()
}

/// PUT /api/magic-prompts/{id}：设置单条覆盖；text 缺失或非字符串直接
/// 400，业务校验错误按 status_for 映射状态码。
async fn put_prompt(
    State(state): State<ModuleState>,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
    let text = body.get("text").and_then(Value::as_str);
    if body.get("text").is_some_and(|v| !v.is_string()) {
        return error_response(StatusCode::BAD_REQUEST, "text is required");
    }
    match state.set_override(&id, text).await {
        Ok(next) => Json(next.to_json()).into_response(),
        Err(message) => error_response(status_for(&message), &message),
    }
}

/// DELETE /api/magic-prompts/{id}：删除单条覆盖；"Invalid prompt id"
/// 映射 400，其余 500。
async fn delete_prompt(State(state): State<ModuleState>, Path(id): Path<String>) -> Response {
    match state.reset_override(&id).await {
        Ok(next) => Json(next.to_json()).into_response(),
        Err(message) => {
            let status = if message.contains("Invalid prompt id") {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            error_response(status, &message)
        }
    }
}

/// DELETE /api/magic-prompts：清空全部覆盖；失败一律 500。
async fn delete_all_prompts(State(state): State<ModuleState>) -> Response {
    match state.reset_all_overrides().await {
        Ok(next) => Json(next.to_json()).into_response(),
        Err(message) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &message),
    }
}

/// 构建 /api/magic-prompts 与 /api/magic-prompts/{id} 路由；状态指向
/// data dir 下的 magic-prompts.json。
pub fn router(ctx: RouterContext) -> Router {
    Router::new()
        .route(
            "/api/magic-prompts",
            get(get_prompts).delete(delete_all_prompts),
        )
        .route(
            "/api/magic-prompts/{id}",
            axum::routing::put(put_prompt).delete(delete_prompt),
        )
        .with_state(ModuleState {
            file_path: ctx.config.data_dir.join("magic-prompts.json"),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
}

/// magic-prompts 路由的行为测试。
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    /// 测试夹具：独立临时目录 + 指向该目录的路由应用。
    struct Fixture {
        /// 测试用临时目录（作为 data dir）。
        dir: std::path::PathBuf,
    }

    /// 夹具的构建与请求辅助。
    impl Fixture {
        /// 创建带唯一临时目录的夹具。
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "oc-magic-prompts-{tag}-{}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Fixture { dir }
        }

        /// 构建以夹具目录为 data dir 的路由应用。
        fn app(&self) -> Router {
            Router::new()
                .route(
                    "/api/magic-prompts",
                    get(get_prompts).delete(delete_all_prompts),
                )
                .route(
                    "/api/magic-prompts/{id}",
                    axum::routing::put(put_prompt).delete(delete_prompt),
                )
                .with_state(ModuleState {
                    file_path: self.dir.join("magic-prompts.json"),
                    write_lock: Arc::new(tokio::sync::Mutex::new(())),
                })
        }

        /// 对应用发起一次 oneshot 请求；有 body 时附带 JSON content-type。
        async fn request(
            &self,
            method: &str,
            uri: &str,
            body: Option<String>,
        ) -> axum::response::Response {
            let mut builder = axum::http::Request::builder().method(method).uri(uri);
            if body.is_some() {
                builder = builder.header("content-type", "application/json");
            }
            self.app()
                .oneshot(builder.body(Body::from(body.unwrap_or_default())).unwrap())
                .await
                .unwrap()
        }

        /// 读取响应体并解析为 JSON。
        async fn json(response: axum::response::Response) -> Value {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    /// 验证 PUT 写入、GET 读回、DELETE 重置的完整回路及落盘内容。
    #[tokio::test]
    async fn roundtrip_set_get_reset() {
        let fixture = Fixture::new("roundtrip");
        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/my.prompt-1",
                Some(r#"{"text":"hello"}"#.into()),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::json(response).await;
        assert_eq!(value["overrides"]["my.prompt-1"], "hello");

        let response = fixture.request("GET", "/api/magic-prompts", None).await;
        let value = Fixture::json(response).await;
        assert_eq!(value["version"], 1);
        assert_eq!(value["overrides"]["my.prompt-1"], "hello");

        let response = fixture
            .request("DELETE", "/api/magic-prompts/my.prompt-1", None)
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::json(response).await;
        assert!(value["overrides"].as_object().unwrap().is_empty());
    }

    /// 验证 id 模式、.visible 非空、长度上限与 text 类型四类校验
    /// 与 JS 行为一致（均返回 400 及对应错误消息）。
    #[tokio::test]
    async fn id_and_text_validation_mirror_js() {
        let fixture = Fixture::new("validation");

        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/BAD%20ID",
                Some(r#"{"text":"x"}"#.into()),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(Fixture::json(response).await["error"], "Invalid prompt id");

        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/a.visible",
                Some(r#"{"text":"  "}"#.into()),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            Fixture::json(response).await["error"],
            "Visible prompt text cannot be empty"
        );

        let long = "x".repeat(MAX_PROMPT_TEXT_LENGTH + 1);
        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/big",
                Some(format!(r#"{{"text":"{long}"}}"#)),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            Fixture::json(response).await["error"],
            "Prompt text is too long"
        );

        let response = fixture
            .request(
                "PUT",
                "/api/magic-prompts/ok",
                Some(r#"{"text":42}"#.into()),
            )
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(Fixture::json(response).await["error"], "text is required");
    }

    /// 验证存储文件损坏时读取降级为空默认值而不是报错。
    #[tokio::test]
    async fn malformed_file_degrades_to_empty_not_error() {
        let fixture = Fixture::new("malformed");
        std::fs::write(fixture.dir.join("magic-prompts.json"), "{nope").unwrap();
        let response = fixture.request("GET", "/api/magic-prompts", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::json(response).await;
        assert!(value["overrides"].as_object().unwrap().is_empty());
    }

    /// 验证 prompt id 校验与 JS 的 PROMPT_ID_PATTERN 语义一致。
    #[test]
    fn prompt_id_pattern_matches_js() {
        assert!(is_valid_prompt_id("a"));
        assert!(is_valid_prompt_id("my.prompt_1-x"));
        assert!(!is_valid_prompt_id(""));
        assert!(!is_valid_prompt_id("UPPER"));
        assert!(!is_valid_prompt_id("a b"));
        assert!(!is_valid_prompt_id(&"a".repeat(PROMPT_ID_MAX + 1)));
    }
}
