//! Port of `server/lib/agent-memory/routes.js`.
//!
//! The scope is a query parameter rather than part of the path, because
//! global and project memory are the same resource with two homes. Getting
//! the scope wrong must fail loudly, never silently write the user's global
//! memory from a project-scoped call. Memory is created by the agent through
//! the `ompchamber_memory` tool, so there is no create route here; the panel
//! reads, corrects, and deletes.
//!
//! 中文说明：agent memory 面板路由（对应 JS 端 routes.js）。scope 放在
//! 查询参数而非路径：全局与项目记忆是同一资源的两个归属地，scope 解析
//! 失败必须响亮报错，绝不静默把项目作用域的调用写进用户的全局记忆。
//! 记忆由 agent 经 `ompchamber_memory` 工具创建，故此处没有创建路由；
//! 面板只负责读取、更正与删除。

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use serde::Serialize;
use serde_json::{Value, json};

use super::runtime::{AgentMemoryRuntime, MemoryEnabledGate, MemoryError, Target, UpdatePatch};

/// `express.json({ limit: '1mb' })` on the one route that carries a body.
/// 唯一带请求体的路由（PATCH）的 body 上限，等价 JS 端
/// `express.json({ limit: '1mb' })` 的 1 MiB。
const JSON_BODY_LIMIT_BYTES: usize = 1024 * 1024;

/// 路由共享状态：存储运行时与可选的启用闸门，经 `with_state` 注入各
/// handler。
pub struct RoutesState {
    /// 共享的存储运行时，直接读写两个 scope 的记忆文件。
    pub runtime: Arc<AgentMemoryRuntime>,
    /// JS passes `isAgentMemoryEnabled` optionally; omitting it runs the
    /// surface ungated (how the JS route tests mount it).
    /// 可选启用闸门：JS 侧可省略 `isAgentMemoryEnabled`，省略即不加闸
    /// 运行（JS 路由测试即如此挂载）。
    pub is_enabled: Option<MemoryEnabledGate>,
}

/// 构造 agent memory 的 axum 路由：单 scope 读取（GET）、双 scope 合并
/// 读取（GET /all）、按记忆 id 的 PATCH 更正与 DELETE 删除；共享状态
/// 打包进 [`RoutesState`] 后随路由返回。
pub fn routes(runtime: Arc<AgentMemoryRuntime>, is_enabled: Option<MemoryEnabledGate>) -> Router {
    Router::new()
        .route("/api/agent-memory", get(get_memory))
        .route("/api/agent-memory/all", get(get_all_memory))
        .route(
            "/api/agent-memory/{memoryId}",
            patch(patch_memory).delete(delete_memory),
        )
        .with_state(Arc::new(RoutesState {
            runtime,
            is_enabled,
        }))
}

/// One gate for the whole surface. The settings toggle disables the feature,
/// not just its UI: with memory off, these routes must not read or write the
/// store at all, or a stale client would keep editing memory the user
/// believes is turned off.
/// 整个路由面的统一闸门：开关关闭时这些路由绝不能读写存储，否则过期
/// 客户端会继续编辑用户以为已关闭的记忆。关闭返回带 `disabled: true`
/// 标记的 404（与“条目不存在”的 404 区分），设置读取失败返回 503——
/// 宁可关闭也不暴露可能已被用户关掉的表面；闸门为 `None` 时直接放行。
async fn require_enabled(state: &RoutesState) -> Result<(), Response> {
    let Some(gate) = &state.is_enabled else {
        return Ok(());
    };
    match gate().await {
        // Flagged, not merely 404: a missing entry answers 404 too, and a
        // client that could not tell them apart would report a deleted
        // memory as the whole feature being switched off.
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Agent memory is disabled", "disabled": true })),
        )
            .into_response()),
        // An unreadable settings file must not silently expose a surface the
        // user may have turned off.
        Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Agent memory availability is unknown" })),
        )
            .into_response()),
        Ok(true) => Ok(()),
    }
}

/// The JS handlers classify caught errors by message substring.
/// 按错误消息子串判断是否为客户端校验错误（JS 处理器即如此分类）：
/// 命中这些子串的错误映射为 400，其余按 500 处理。
fn is_validation_error(message: &str) -> bool {
    message.contains("is required")
        || message.contains("unsupported characters")
        || message.contains("holds at most")
}

/// 把存储层错误转换成 HTTP 响应：先取出错误消息（IO 错误取其字符串
/// 形式），校验类消息返回 400；其余返回 500，消息为空时用
/// `fallback_message` 兜底，保证响应体总有可读的 `error` 字段。
fn respond_with_error(error: &MemoryError, fallback_message: &str) -> Response {
    let message = match error {
        MemoryError::Message(message) => message.clone(),
        MemoryError::Io(err) => err.to_string(),
    };
    if is_validation_error(&message) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response();
    }
    let message = if message.is_empty() {
        fallback_message.to_string()
    } else {
        message
    };
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": message })),
    )
        .into_response()
}

/// Parsed query pairs. [`query_string`] mirrors JS `typeof query.key ===
/// 'string'`: absent or repeated keys (`?k=a&k=b` parses as an array in
/// express) both read as "not a string".
/// 把原始查询串解析为 (key, value) 对列表；无查询串时返回空表。
/// 重复键的全部出现都保留，由 [`query_string`] 决定取舍。
fn parse_query(raw: Option<&str>) -> Vec<(String, String)> {
    raw.map(|query| {
        url::form_urlencoded::parse(query.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    })
    .unwrap_or_default()
}

/// 取“恰好出现一次”的 `key` 的值：键缺失或重复出现（express 会解析成
/// 数组，`typeof !== 'string'`）都返回 `None`，绝不退化为取最后一个值。
fn query_string(pairs: &[(String, String)], key: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for (candidate, value) in pairs {
        if candidate == key {
            if found.is_some() {
                return None;
            }
            found = Some(value.clone());
        }
    }
    found
}

/// Resolves the target scope, or the reason it could not be resolved. A
/// project request without an id is rejected here rather than quietly
/// falling back to global, which would write project facts into every other
/// project.
/// 从查询参数解析目标 scope：`scope=global`，或 `scope=project` 且附带
/// 非空 `projectId`。project 缺 id、scope 缺失或拼写不对时返回错误消息
/// （调用方据此回 400）；绝不静默回退到 global——那会把项目事实写进
/// 其它所有项目的记忆。
fn resolve_scope(pairs: &[(String, String)]) -> Result<Target, &'static str> {
    match query_string(pairs, "scope").as_deref() {
        Some("global") => Ok(Target::Global),
        Some("project") => {
            match query_string(pairs, "projectId").filter(|id| !id.trim().is_empty()) {
                Some(project_id) => Ok(Target::Project { project_id }),
                None => Err("projectId is required for project scope"),
            }
        }
        _ => Err("scope must be global or project"),
    }
}

/// JS relies on express's `express.json()`: only `*json` content types
/// populate `req.body`; anything else leaves it undefined, which this route
/// rejects as a malformed body.
/// 复刻 express 的 body 语义：仅当 Content-Type 含 "json" 才解析 body，
/// 其余情况（以及解析失败时）返回 `Value::Null`，由调用方按非法 body
/// 拒绝。
fn parse_json_body(headers: &HeaderMap, body: &[u8]) -> Value {
    let json_content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"));
    if !json_content_type {
        return Value::Null;
    }
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

/// PATCH 成功的响应体：更新后的条目与该 scope 的全部剩余条目，
/// 供面板立即重渲染。
#[derive(Serialize)]
struct UpdateResponse<'a> {
    /// 更新后的记忆条目。
    entry: &'a super::runtime::MemoryEntry,
    /// 该 scope 当前的全部条目。
    entries: &'a [super::runtime::MemoryEntry],
}

/// GET /api/agent-memory：读取单个 scope 的记忆并原样返回存储文件
/// JSON。闸门关闭→404（带 disabled 标记）、scope 非法→400、
/// 存储损坏→500。
async fn get_memory(State(state): State<Arc<RoutesState>>, RawQuery(query): RawQuery) -> Response {
    if let Err(denied) = require_enabled(&state).await {
        return denied;
    }
    let pairs = parse_query(query.as_deref());
    let target = match resolve_scope(&pairs) {
        Ok(target) => target,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
        }
    };
    match state.runtime.read(&target).await {
        Ok(file) => Json(file).into_response(),
        Err(error) => respond_with_error(&error, "Failed to read agent memory"),
    }
}

/// Both scopes in one response: the panel always shows them together, and
/// two separate requests would let one scope render while the other is still
/// loading, which reads as memory that has gone missing.
/// GET /api/agent-memory/all：一次返回 global 与 project 两个 scope——
/// 面板总是同屏展示两者，拆成两次请求会让一个 scope 先渲染而另一个
/// 还在加载，看起来像记忆丢失。projectId 可选；任一侧读取失败由
/// `read_all` 的 `*Failed` 标记表达，而非让整请求失败。
async fn get_all_memory(
    State(state): State<Arc<RoutesState>>,
    RawQuery(query): RawQuery,
) -> Response {
    if let Err(denied) = require_enabled(&state).await {
        return denied;
    }
    let pairs = parse_query(query.as_deref());
    let project_id = query_string(&pairs, "projectId").filter(|id| !id.trim().is_empty());
    let all = state.runtime.read_all(project_id.as_deref()).await;
    Json(all).into_response()
}

/// PATCH /api/agent-memory/{memoryId}：更正一条记忆。依次校验：启用
/// 闸门、scope（400）、body 大小上限（413）、Content-Type 与 JSON 对象
/// 形状（非对象拒绝）、title/body 必须为字符串、type 必须是
/// [`super::runtime::MEMORY_TYPES`] 之一；随后组装 [`UpdatePatch`] 下推
/// 存储层。命中返回 200（条目+列表），id 不存在返回 404。
async fn patch_memory(
    State(state): State<Arc<RoutesState>>,
    Path(memory_id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Err(denied) = require_enabled(&state).await {
        return denied;
    }
    let pairs = parse_query(query.as_deref());
    let target = match resolve_scope(&pairs) {
        Ok(target) => target,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
        }
    };

    // express.json's limit rejects larger payloads before the handler runs.
    if body.len() > JSON_BODY_LIMIT_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": "request entity too large" })),
        )
            .into_response();
    }

    let parsed = parse_json_body(&headers, &body);
    let Some(record) = parsed.as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Body must be an object" })),
        )
            .into_response();
    };
    if record.contains_key("title") && !record["title"].is_string() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "title must be a string" })),
        )
            .into_response();
    }
    if record.contains_key("body") && !record["body"].is_string() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "body must be a string" })),
        )
            .into_response();
    }
    if record.contains_key("type")
        && !record
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|entry_type| super::runtime::MEMORY_TYPES.contains(&entry_type))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "type must be fact, preference, or reference" })),
        )
            .into_response();
    }

    let patch = UpdatePatch {
        title: record
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
        body: record
            .get("body")
            .and_then(Value::as_str)
            .map(str::to_string),
        entry_type: record
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    match state.runtime.update(&target, &memory_id, &patch).await {
        Ok(Some(result)) => Json(UpdateResponse {
            entry: &result.entry,
            entries: &result.entries,
        })
        .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Memory not found" })),
        )
            .into_response(),
        Err(error) => respond_with_error(&error, "Failed to save memory"),
    }
}

/// DELETE /api/agent-memory/{memoryId}：删除指定 scope 的一条记忆。
/// 成功返回 `deleted: true` 与剩余条目；id 不存在返回 404；存储错误走
/// [`respond_with_error`]。
async fn delete_memory(
    State(state): State<Arc<RoutesState>>,
    Path(memory_id): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    if let Err(denied) = require_enabled(&state).await {
        return denied;
    }
    let pairs = parse_query(query.as_deref());
    let target = match resolve_scope(&pairs) {
        Ok(target) => target,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
        }
    };

    match state.runtime.remove(&target, &memory_id).await {
        Ok(result) if result.deleted => {
            // 删除成功的响应体：deleted 标记与该 scope 剩余条目。
            #[derive(Serialize)]
            struct DeleteResponse<'a> {
                deleted: bool,
                entries: &'a [super::runtime::MemoryEntry],
            }
            Json(DeleteResponse {
                deleted: true,
                entries: &result.entries,
            })
            .into_response()
        }
        Ok(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Memory not found" })),
        )
            .into_response(),
        Err(error) => respond_with_error(&error, "Failed to delete memory"),
    }
}

/// 路由层行为测试：scope 解析与拒绝、双 scope 读取、PATCH 校验、
/// DELETE、启用闸门的关闭语义，以及查询参数的 express 兼容语义。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_memory::runtime::{CreateInput, MemoryError as StoreError};
    use axum::body::Body;
    use tower::ServiceExt;

    /// 测试夹具：独立临时目录模拟用户配置根，drop 时整体清理。
    struct Fixture {
        /// 临时根目录（模拟 `~/.config/ompchamber` 一类配置根）。
        root: std::path::PathBuf,
    }

    /// 夹具的构造与派生 helper。
    impl Fixture {
        /// 以 tag + 进程 id + 自增序号命名临时目录，避免测试间冲突；
        /// 先清掉可能的残留再重建。
        fn new(tag: &str) -> Self {
            // 每次构造自增，保证同进程内多个夹具的目录名唯一。
            static SEQUENCE: std::sync::atomic::AtomicUsize =
                std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "oc-agent-memory-routes-{tag}-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            std::fs::remove_dir_all(&root).ok();
            std::fs::create_dir_all(&root).expect("fixture root");
            Fixture { root }
        }

        /// 基于夹具根构造存储运行时：配置根为 `<root>/config`，
        /// 项目记忆存放于其下 `projects/<projectId>/memory.json`。
        fn runtime(&self) -> Arc<AgentMemoryRuntime> {
            Arc::new(AgentMemoryRuntime::new(
                self.root.join("config"),
                self.root.join("config").join("projects"),
                None,
            ))
        }

        /// 全局记忆文件的期望路径，用于断言“开关关闭时从未触碰存储”。
        fn global_path(&self) -> std::path::PathBuf {
            self.root.join("config").join("memory.json")
        }
    }

    /// 夹具析构：递归删除临时根。
    impl Drop for Fixture {
        /// 删除临时目录；失败忽略（不影响其它测试的唯一目录名）。
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    /// 读出响应 body 并按 JSON 解析，供断言使用。
    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    /// 构造对 agent memory 路由的请求；带 body 时自动附 `application/json`。
    fn request(method: &str, uri: &str, body: Option<&str>) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        builder
            .body(Body::from(body.unwrap_or_default().to_string()))
            .expect("request")
    }

    /// 构造返回固定结果的启用闸门，模拟开关打开/关闭/设置读取失败三种状态。
    fn gate(returning: Result<bool, &'static str>) -> MemoryEnabledGate {
        Arc::new(move || {
            let outcome = returning;
            Box::pin(async move { outcome.map_err(StoreError::message) })
        })
    }

    /// 验证 GET global scope 返回带版本号的存储 JSON，且条目默认类型为 fact。
    #[tokio::test]
    async fn reads_global_scope() {
        let fixture = Fixture::new("get-global");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("Uses bun".into()),
                    body: Some("Tests run with bun test.".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime, None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["version"], json!(1));
        assert_eq!(body["entries"][0]["title"], json!("Uses bun"));
        assert_eq!(body["entries"][0]["type"], json!("fact"));
    }

    /// 验证带 projectId 的 project 读取落到该项目专属的 memory.json。
    #[tokio::test]
    async fn reads_project_scope_with_its_id() {
        let fixture = Fixture::new("get-project");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Project {
                    project_id: "path_abc".into(),
                },
                &CreateInput {
                    title: Some("P".into()),
                    body: Some("b".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime, None);

        let response = app
            .oneshot(request(
                "GET",
                "/api/agent-memory?scope=project&projectId=path_abc",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["entries"][0]["title"], json!("P"));
        assert!(
            fixture
                .root
                .join("config")
                .join("projects")
                .join("path_abc")
                .join("memory.json")
                .exists()
        );
    }

    /// 验证 project scope 缺 projectId 时返回 400，且绝不回退写全局存储。
    #[tokio::test]
    async fn refuses_a_project_scope_with_no_id_rather_than_falling_back_to_global() {
        let fixture = Fixture::new("no-project-id");
        let runtime = fixture.runtime();
        let app = routes(runtime, None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=project", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("projectId is required")
        );
        assert!(!fixture.global_path().exists(), "never touched the store");
    }

    /// 验证缺失 scope 参数返回 400，错误消息说明合法取值。
    #[tokio::test]
    async fn refuses_a_missing_scope() {
        let fixture = Fixture::new("no-scope");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert!(
            body["error"]
                .as_str()
                .expect("error")
                .contains("scope must be")
        );
    }

    /// 验证 DELETE 缺 scope 时在触碰存储之前就被 400 拒绝。
    #[tokio::test]
    async fn refuses_a_delete_with_no_scope_before_touching_the_store() {
        let fixture = Fixture::new("delete-no-scope");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("T".into()),
                    body: Some("b".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime.clone(), None);

        let response = app
            .oneshot(request("DELETE", "/api/agent-memory/mem-x", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            runtime
                .read(&Target::Global)
                .await
                .expect("read")
                .entries
                .len(),
            1,
            "store untouched"
        );
    }

    /// 验证 /all 一次返回两个 scope 的条目，且两个 failed 标记均为 false。
    #[tokio::test]
    async fn returns_global_and_project_together() {
        let fixture = Fixture::new("all");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("G".into()),
                    body: Some("x".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("global");
        runtime
            .create(
                &Target::Project {
                    project_id: "path_abc".into(),
                },
                &CreateInput {
                    title: Some("P".into()),
                    body: Some("y".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("project");
        let app = routes(runtime, None);

        let response = app
            .oneshot(request(
                "GET",
                "/api/agent-memory/all?projectId=path_abc",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["global"].as_array().expect("global").len(), 1);
        assert_eq!(body["project"].as_array().expect("project").len(), 1);
        assert_eq!(body["globalFailed"], json!(false));
        assert_eq!(body["projectFailed"], json!(false));
    }

    /// 验证未打开项目时 /all 的 project 侧为空数组而非报错。
    #[tokio::test]
    async fn reads_global_alone_when_no_project_is_open() {
        let fixture = Fixture::new("all-no-project");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory/all", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert!(body["project"].as_array().expect("project").is_empty());
    }

    /// 验证项目存储损坏时 projectFailed 置真，而不是把记忆渲染成空列表。
    #[tokio::test]
    async fn a_broken_project_scope_is_reported_not_rendered_as_empty() {
        let fixture = Fixture::new("all-broken");
        let runtime = fixture.runtime();
        let project_path = fixture
            .root
            .join("config")
            .join("projects")
            .join("path_abc")
            .join("memory.json");
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(&project_path, "{ broken").unwrap();
        let app = routes(runtime, None);

        let response = app
            .oneshot(request(
                "GET",
                "/api/agent-memory/all?projectId=path_abc",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["projectFailed"], json!(true));
        assert!(body["project"].as_array().expect("project").is_empty());
    }

    /// 验证全局存储文件损坏时返回 500 并报告 malformed。
    #[tokio::test]
    async fn reports_malformed_storage_as_a_server_error() {
        let fixture = Fixture::new("malformed");
        let runtime = fixture.runtime();
        std::fs::create_dir_all(fixture.global_path().parent().unwrap()).unwrap();
        std::fs::write(fixture.global_path(), "{ not json").unwrap();
        let app = routes(runtime, None);

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = body_json(response).await;
        assert_eq!(body["error"], json!("Stored agent memory is malformed"));
    }

    /// 验证含路径分隔符的 projectId 被判为不支持字符并返回 400。
    #[tokio::test]
    async fn reports_a_bad_project_id_as_a_client_error() {
        let fixture = Fixture::new("bad-id");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "GET",
                // A bare ".." passes the id pattern (dots are allowed), same
                // as JS; a traversal id with a separator is what fails.
                "/api/agent-memory?scope=project&projectId=..%2Fescape",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(
            body["error"],
            json!("projectId contains unsupported characters")
        );
    }

    /// 验证 PATCH 用 JSON body 更新 title/body，响应含更新条目与全列表。
    #[tokio::test]
    async fn patches_a_memory_from_a_json_body() {
        let fixture = Fixture::new("patch");
        let runtime = fixture.runtime();
        let created = runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("Unclear".into()),
                    body: Some("Original.".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime, None);

        let response = app
            .oneshot(request(
                "PATCH",
                &format!("/api/agent-memory/{}?scope=global", created.entry.id),
                Some(r#"{"title": "Clearer", "body": "Reworded."}"#),
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["entry"]["title"], json!("Clearer"));
        assert_eq!(body["entry"]["body"], json!("Reworded."));
        assert_eq!(body["entries"].as_array().expect("entries").len(), 1);
    }

    /// 验证 title 非字符串时返回 400。
    #[tokio::test]
    async fn rejects_a_non_string_title() {
        let fixture = Fixture::new("patch-bad-title");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "PATCH",
                "/api/agent-memory/mem-1?scope=global",
                Some(r#"{"title": 42}"#),
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(body["error"], json!("title must be a string"));
    }

    /// 验证数组 body 与缺失 JSON Content-Type 都按非法 body 返回 400
    /// （express 只在 JSON Content-Type 下填充 req.body）。
    #[tokio::test]
    async fn rejects_a_non_object_body_and_a_missing_content_type() {
        let fixture = Fixture::new("patch-bad-body");
        let app = routes(fixture.runtime(), None);

        let array_body = app
            .clone()
            .oneshot(request(
                "PATCH",
                "/api/agent-memory/mem-1?scope=global",
                Some(r#"[1, 2]"#),
            ))
            .await
            .expect("oneshot");
        assert_eq!(array_body.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(array_body).await["error"],
            json!("Body must be an object")
        );

        // No JSON content type: express leaves req.body undefined.
        let no_content_type = axum::http::Request::patch("/api/agent-memory/mem-1?scope=global")
            .body(Body::from(r#"{"title": "x"}"#))
            .unwrap();
        let response = app.oneshot(no_content_type).await.expect("oneshot");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// 验证空补丁 `{}` 被拒绝：title、body 或 type 至少提供一个。
    #[tokio::test]
    async fn an_empty_patch_is_a_client_error() {
        let fixture = Fixture::new("patch-empty");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "PATCH",
                "/api/agent-memory/mem-1?scope=global",
                Some("{}"),
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await["error"],
            json!("title, body or type is required")
        );
    }

    /// 验证 PATCH 命中不存在的 id 返回 404。
    #[tokio::test]
    async fn reports_a_missing_memory_as_404() {
        let fixture = Fixture::new("patch-miss");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "PATCH",
                "/api/agent-memory/nope?scope=global",
                Some(r#"{"body": "x"}"#),
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await["error"],
            json!("Memory not found")
        );
    }

    /// 验证 DELETE 删除指定 scope 的指定条目，响应含空剩余列表。
    #[tokio::test]
    async fn deletes_the_named_memory_in_the_named_scope() {
        let fixture = Fixture::new("delete");
        let runtime = fixture.runtime();
        let created = runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("T".into()),
                    body: Some("b".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime.clone(), None);

        let response = app
            .oneshot(request(
                "DELETE",
                &format!("/api/agent-memory/{}?scope=global", created.entry.id),
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["deleted"], json!(true));
        assert!(body["entries"].as_array().expect("entries").is_empty());
        assert!(
            runtime
                .read(&Target::Global)
                .await
                .expect("read")
                .entries
                .is_empty()
        );
    }

    /// 验证 DELETE 未命中返回 404 且不带 disabled 标记（与开关关闭区分）。
    #[tokio::test]
    async fn reports_a_missing_memory_as_404_on_delete() {
        let fixture = Fixture::new("delete-miss");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "DELETE",
                "/api/agent-memory/nope?scope=global",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["error"], json!("Memory not found"));
        assert!(body.get("disabled").is_none());
    }

    /// 验证开关关闭时响应为带 `disabled: true` 的 404，且全程不读存储文件。
    #[tokio::test]
    async fn the_disabled_answer_is_flagged_so_a_deleted_entry_cannot_be_mistaken_for_it() {
        let fixture = Fixture::new("gate-off");
        let runtime = fixture.runtime();
        let off = routes(runtime.clone(), Some(gate(Ok(false))));

        let disabled = off
            .clone()
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");
        assert_eq!(disabled.status(), StatusCode::NOT_FOUND);
        let body = body_json(disabled).await;
        assert_eq!(body["disabled"], json!(true));
        assert_eq!(body["error"], json!("Agent memory is disabled"));
        assert!(
            !fixture.global_path().exists(),
            "the store is never read while memory is off"
        );
    }

    /// 验证开关关闭时过期客户端的 DELETE 不产生任何删除。
    #[tokio::test]
    async fn refuses_deletes_from_a_stale_client_while_memory_is_off() {
        let fixture = Fixture::new("gate-off-delete");
        let runtime = fixture.runtime();
        runtime
            .create(
                &Target::Global,
                &CreateInput {
                    title: Some("T".into()),
                    body: Some("b".into()),
                    ..CreateInput::default()
                },
            )
            .await
            .expect("seed");
        let app = routes(runtime.clone(), Some(gate(Ok(false))));

        let response = app
            .oneshot(request(
                "DELETE",
                "/api/agent-memory/mem-1?scope=global",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            runtime
                .read(&Target::Global)
                .await
                .expect("read")
                .entries
                .len(),
            1,
            "nothing deleted while memory is off"
        );
    }

    /// 验证开关打开时路由正常服务。
    #[tokio::test]
    async fn serves_normally_while_memory_is_on() {
        let fixture = Fixture::new("gate-on");
        let app = routes(fixture.runtime(), Some(gate(Ok(true))));

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// 验证设置读取失败时以 503 关闭表面，而非冒险放行。
    #[tokio::test]
    async fn closes_the_surface_when_the_setting_cannot_be_read() {
        let fixture = Fixture::new("gate-error");
        let app = routes(fixture.runtime(), Some(gate(Err("settings unreadable"))));

        let response = app
            .oneshot(request("GET", "/api/agent-memory?scope=global", None))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            body_json(response).await["error"],
            json!("Agent memory availability is unknown")
        );
    }

    /// 验证重复查询键按“非字符串”处理返回 400，绝不取最后一个值。
    #[tokio::test]
    async fn a_repeated_query_key_reads_as_an_array_never_as_its_last_value() {
        let fixture = Fixture::new("repeated-keys");
        let app = routes(fixture.runtime(), None);

        let response = app
            .oneshot(request(
                "GET",
                "/api/agent-memory?scope=project&projectId=a&projectId=b",
                None,
            ))
            .await
            .expect("oneshot");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(
            body_json(response).await["error"]
                .as_str()
                .expect("error")
                .contains("projectId is required")
        );
    }
}
