//! Route surface — port of `server/lib/git/routes.js` (`registerGitRoutes`):
//! every `/api/git/*` path, verb, status code, JSON shape, and error envelope.
//! Handlers mirror the JS control flow exactly, including the soft
//! non-repository payloads on status/check/worktrees and the
//! `X-OMPChamber-Warning` header on worktree-list failures.
//!
//! 中文说明:/api/git/* 路由层。每个 handler 都对应旧 JS server
//! server/lib/git/routes.js(registerGitRoutes)中的一个路由:路径、HTTP
//! 方法、状态码、JSON 形状与错误包裹({"error": ...})保持一致,包括
//! status/check/worktrees 在非仓库目录上的 200 软响应,以及 worktree
//! 列表失败时的 X-OMPChamber-Warning 头部语义。

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post, put};
use serde_json::{Value, json};

use super::identity::{IdentityStorage, discover_git_credentials};
use super::service::GitService;
use super::worktrees as wt;

/// Module-local state (`Router::with_state` before returning).
/// 中文说明:路由模块本地状态,由上层通过 Router::with_state 注入后再返回 Router。
#[derive(Clone)]
pub struct GitState {
    /// git 服务句柄:所有 git 命令执行与 worktree/bootstrap 状态都挂在它上面。
    pub service: Arc<GitService>,
    /// 身份 profile 的持久化存储(identities 系列 CRUD 使用)。
    pub identity: Arc<IdentityStorage>,
    /// 服务端事件总线(SSE 广播等),随 state 透传给需要的模块。
    pub hub: Arc<crate::hub::EventHub>,
    /// server 数据目录路径。
    pub data_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Request helpers (express query/body semantics)
// ---------------------------------------------------------------------------

/// Express `req.query.<name>` / `resolveDirectoryQuery`: duplicate keys
/// produce an array in express and the JS takes the first element — take the
/// first value here for both shapes.
/// 中文说明:解析 query string 中第一个匹配 name 的参数值并做 percent 解码,
/// 对齐 express req.query.<name> / resolveDirectoryQuery 取首值的语义。
fn query_first(query: Option<&str>, name: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        if percent_decode(key) == name {
            return Some(percent_decode(value));
        }
    }
    None
}

/// application/x-www-form-urlencoded 解码:'+' 转空格,%XX 转回原始字节,
/// 非法的百分号序列原样保留;解码结果按 UTF-8 lossy 转为 String。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) =
                    u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
                {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// `resolveDirectoryQuery` — trimmed first value or `None`.
/// 中文说明:取 directory 参数的首个值并 trim,空串视为缺失返回 None。
fn resolve_directory_query(query: Option<&str>) -> Option<String> {
    query_first(query, "directory")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// express.json(): a JSON body parses to its value; missing/empty bodies are
/// `undefined` (callers apply `|| {}`); malformed JSON is a 400 like express.
/// 中文说明:对齐 express.json():合法 JSON 返回其值;空/纯空白 body 视为
/// undefined(Value::Null,调用方再按 || {} 归一);读取超限或解析失败
/// 返回 400 Response。
async fn read_json_body(body: Body) -> Result<Value, Response> {
    let bytes = axum::body::to_bytes(body, 1024 * 1024)
        .await
        .map_err(|_| error_response(StatusCode::BAD_REQUEST, "Invalid JSON body"))?;
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| error_response(StatusCode::BAD_REQUEST, "Invalid JSON body"))
}

/// 把任意 body 规整成对象视图:是 JSON object 就原样返回,否则返回共享的
/// Value::Null——后续字段访问都会得到 None,等价于 JS 中 body || {}
/// 之后再取字段的缺省行为。
fn body_object(body: &Value) -> &Value {
    // 共享的 Null 常量:非 object body 统一指向它,避免每次分配。
    static NULL_VALUE: std::sync::LazyLock<Value> = std::sync::LazyLock::new(|| Value::Null);
    if body.is_object() { body } else { &NULL_VALUE }
}

/// 取 body 中字段的字符串值;字段缺失或不是字符串返回 None。
fn str_field<'a>(body: &'a Value, field: &str) -> Option<&'a str> {
    body.get(field).and_then(Value::as_str)
}

/// Truthy string field (JS `if (!branch)` rejects empty strings too).
/// 中文说明:取真值字符串字段:字段缺失、非字符串或为空串都返回 None,
/// 对齐 JS if (!branch) 连空字符串一起拒绝的语义。
fn truthy_str(body: &Value, field: &str) -> Option<String> {
    str_field(body, field)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// 构造统一错误响应:指定状态码 + {"error": <message>} 的 JSON 包裹。
fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// 构造 500 响应;服务层错误消息为空时用 fallback 文本兜底。
fn internal_error(message: &str, fallback: &str) -> Response {
    let text = if message.is_empty() {
        fallback.to_string()
    } else {
        message.to_string()
    };
    error_response(StatusCode::INTERNAL_SERVER_ERROR, text)
}

/// 判断错误消息是否为 "not a git repository"(大小写不敏感),
/// 用于把这类失败降级为 200 的软非仓库负载而非 500。
fn is_non_repo_message(message: &str) -> bool {
    message.to_lowercase().contains("not a git repository")
}

/// 非仓库目录的 200 软负载:isGitRepository=false、空文件列表、
/// null 分支、ahead/behind 均为 0,与 JS 返回形状一致。
fn non_repo_status_payload() -> Value {
    json!({
        "isGitRepository": false,
        "files": [],
        "branch": null,
        "ahead": 0,
        "behind": 0,
    })
}

/// 包装 200 JSON 成功响应。
fn ok_json(value: Value) -> Response {
    Json(value).into_response()
}

/// 必需的 directory 查询参数(经 resolve_directory_query:取首值、trim、非空);
/// 缺失时返回 Err(400 "directory parameter is required")。
fn require_directory(query: Option<&str>) -> Result<String, Response> {
    let Some(directory) = resolve_directory_query(query) else {
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "directory parameter is required",
        ));
    };
    Ok(directory)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/git/identities:返回全部身份 profile 的 JSON 数组,恒 200。
async fn identities_list(State(state): State<GitState>, _req: Request) -> Response {
    let profiles = state.identity.get_profiles();
    ok_json(Value::Array(profiles))
}

/// POST /api/git/identities:以 JSON body 创建 profile;成功返回新 profile,
/// 校验/重名等失败按 400 {"error"} 返回。
async fn identities_create(State(state): State<GitState>, req: Request) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match state.identity.create_profile(&body) {
        Ok(profile) => ok_json(profile),
        Err(message) => error_response(StatusCode::BAD_REQUEST, message),
    }
}

/// PUT /api/git/identities/{id}:更新指定 profile;成功返回更新后的 profile,
/// 失败(id 不存在等)返回 400。
async fn identities_update(
    State(state): State<GitState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    req: Request,
) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match state.identity.update_profile(&id, &body) {
        Ok(profile) => ok_json(profile),
        Err(message) => error_response(StatusCode::BAD_REQUEST, message),
    }
}

/// DELETE /api/git/identities/{id}:删除指定 profile,成功返回
/// {"success":true},失败返回 400。
async fn identities_delete(
    State(state): State<GitState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    _req: Request,
) -> Response {
    match state.identity.delete_profile(&id) {
        Ok(_) => ok_json(json!({ "success": true })),
        Err(message) => error_response(StatusCode::BAD_REQUEST, message),
    }
}

/// GET /api/git/global-identity:读取 git 全局配置中的身份
/// (user.name/user.email 等),恒 200。
async fn global_identity(State(state): State<GitState>, _req: Request) -> Response {
    ok_json(state.service.get_global_identity().await)
}

/// GET /api/git/discover-credentials:枚举本机可发现的 git 凭据来源
/// (OS 钥匙串等),返回数组,恒 200。
async fn discover_credentials(State(_state): State<GitState>, _req: Request) -> Response {
    ok_json(Value::Array(discover_git_credentials(None)))
}

/// GET /api/git/check:探测目录是否为 git 仓库。对齐 JS:非仓库路径同样
/// 返回 200 { isGitRepository: false };只有 directory 参数缺失才是 400。
async fn git_check(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Ok(directory) = require_directory(query) else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    // JS: a non-repo path answers `{ isGitRepository: false }` (200); a hard
    // failure inside git is the 500 path.
    if state.service.is_git_repository(&directory).await {
        ok_json(json!({ "isGitRepository": true }))
    } else {
        ok_json(json!({ "isGitRepository": false }))
    }
}

/// GET /api/git/remote-url:取目录指定 remote(缺省 origin)的 URL,
/// 恒 200 { url }。
async fn remote_url(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let remote = query_first(query, "remote").unwrap_or_else(|| "origin".to_string());
    let url = state.service.get_remote_url(&directory, &remote).await;
    ok_json(json!({ "url": url }))
}

/// GET /api/git/current-identity:返回目录当前生效的 git 身份,恒 200。
async fn current_identity(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    ok_json(state.service.get_current_identity(&directory).await)
}

/// GET /api/git/has-local-identity:目录是否配置了本地(而非继承全局)身份,
/// 返回 { hasLocalIdentity },恒 200。
async fn has_local_identity(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let has_local = state.service.has_local_identity(&directory).await;
    ok_json(json!({ "hasLocalIdentity": has_local }))
}

/// POST /api/git/set-identity:把指定 profile 写入目录的本地 git 配置。
/// profileId="global" 时从全局身份合成虚拟 profile(未配置则 404);
/// 普通 id 查不到也 404;写入失败 500,成功返回 {"success":true,"profile"}。
async fn set_identity(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(profile_id) = truthy_str(&body, "profileId") else {
        return error_response(StatusCode::BAD_REQUEST, "profileId is required");
    };

    let profile: Value;
    if profile_id == "global" {
        let identity = state.service.get_global_identity().await;
        let user_name = identity
            .get("userName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let user_email = identity
            .get("userEmail")
            .and_then(Value::as_str)
            .unwrap_or("");
        if user_name.is_empty() || user_email.is_empty() {
            return error_response(StatusCode::NOT_FOUND, "Global identity is not configured");
        }
        let ssh_key = identity
            .get("sshCommand")
            .and_then(Value::as_str)
            .and_then(|command| command.strip_prefix("ssh -i "))
            .map(str::to_string);
        profile = json!({
            "id": "global",
            "name": "Global Identity",
            "userName": user_name,
            "userEmail": user_email,
            "sshKey": ssh_key,
        });
    } else {
        match state.identity.get_profile(&profile_id) {
            Some(found) => profile = found,
            None => return error_response(StatusCode::NOT_FOUND, "Profile not found"),
        }
    }

    match state.service.set_local_identity(&directory, &profile).await {
        Ok(_) => ok_json(json!({ "success": true, "profile": profile })),
        Err(message) => internal_error(&message, "Failed to set git identity"),
    }
}

/// GET /api/git/status:目录不是仓库(或报 not-a-repository 错误)时返回
/// 200 软负载 non_repo_status_payload;支持 mode=light 轻量模式;
/// 其余失败 500。
async fn status(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Ok(directory) = require_directory(query) else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };

    if !state.service.is_git_repository(&directory).await {
        return ok_json(non_repo_status_payload());
    }

    let mode = if query_first(query, "mode").as_deref() == Some("light") {
        Some("light")
    } else {
        None
    };
    match state.service.get_status(&directory, mode).await {
        Ok(value) => ok_json(value),
        Err(message) if is_non_repo_message(&message) => ok_json(non_repo_status_payload()),
        Err(message) => internal_error(&message, "Failed to get git status"),
    }
}

/// GET /api/git/primary-root:解析目录所属主 worktree 的根路径,恒 200。
async fn primary_root(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    ok_json(
        state
            .service
            .resolve_primary_worktree_root(&directory)
            .await,
    )
}

/// GET /api/git/toplevel:当前 worktree 的顶层目录,恒 200。
async fn toplevel(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    ok_json(state.service.resolve_worktree_top_level(&directory).await)
}

/// POST /api/git/commit-summaries:按 body.shas(提交 SHA 数组)批量取提交
/// 摘要;服务层失败按 400 返回(对齐 JS 的 res.status(400))。
async fn commit_summaries(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match state
        .service
        .get_commit_summaries(&directory, body.get("shas"))
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => error_response(StatusCode::BAD_REQUEST, message),
    }
}

/// POST /api/git/integrate/plan:计算把 worktree 提交集成回目标分支的计划
/// (经 integrate_action 统一包装请求解析与错误映射)。
async fn integrate_plan(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "plan", |state, body| {
        Box::pin(
            async move { super::integrate::compute_integrate_plan(&state.service, &body).await },
        )
    })
    .await
}

/// POST /api/git/integrate/conflict-details:读取临时 worktree 中集成冲突的
/// 详情(body.tempWorktreePath)。
async fn integrate_conflict_details(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "conflict-details", |state, body| {
        Box::pin(async move {
            super::integrate::get_integrate_conflict_details(
                &state.service,
                body.get("tempWorktreePath"),
            )
            .await
        })
    })
    .await
}

/// POST /api/git/integrate/cherry-pick-status:查询临时 worktree 是否有
/// cherry-pick 正在进行。
async fn integrate_cherry_pick_status(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "cherry-pick-status", |state, body| {
        Box::pin(async move {
            super::integrate::is_cherry_pick_in_progress(
                &state.service,
                body.get("tempWorktreePath"),
            )
            .await
        })
    })
    .await
}

/// POST /api/git/integrate/run:按 body.plan 执行集成(cherry-pick 应用提交)。
async fn integrate_run(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "run", |state, body| {
        Box::pin(async move {
            super::integrate::integrate_worktree_commits(
                &state.service,
                body.get("plan").unwrap_or(&Value::Null),
            )
            .await
        })
    })
    .await
}

/// POST /api/git/integrate/abort:中止集成并清理其临时 worktree(body.state)。
async fn integrate_abort(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "abort", |state, body| {
        Box::pin(async move {
            super::integrate::abort_integrate(
                &state.service,
                body.get("state").unwrap_or(&Value::Null),
            )
            .await
        })
    })
    .await
}

/// POST /api/git/integrate/continue:继续一次被冲突中断的集成(body.state)。
async fn integrate_continue(State(state): State<GitState>, req: Request) -> Response {
    integrate_action(req, state, "continue", |state, body| {
        Box::pin(async move {
            super::integrate::continue_integrate(
                &state.service,
                body.get("state").unwrap_or(&Value::Null),
            )
            .await
        })
    })
    .await
}

/// integrate 系列路由的公共骨架:读取 JSON body(null 归一为 {})后调用
/// handler;Ok 转 200 原样返回,Err 转 400,错误消息为空时用
/// "Failed to run git integrate <action>" 兜底。
async fn integrate_action(
    req: Request,
    state: GitState,
    action: &str,
    handler: impl FnOnce(GitState, Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
    + Send,
) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match handler(state, body).await {
        Ok(value) => ok_json(value),
        Err(message) => {
            let fallback = format!("Failed to run git integrate {}", action);
            let text = if message.is_empty() {
                fallback
            } else {
                message
            };
            error_response(StatusCode::BAD_REQUEST, text)
        }
    }
}

/// GET /api/git/diff:单文件 diff。查询参数 directory/path 必填,
/// staged=true 看暂存区,context 指定上下文行数(缺省 3);
/// 返回 {"diff"},失败 500。
async fn diff(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(path) = query_first(query, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let staged = query_first(query, "staged").as_deref() == Some("true");
    let context_lines = query_first(query, "context")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(3);

    match state
        .service
        .get_diff(&directory, Some(&path), staged, Some(context_lines))
        .await
    {
        Ok(diff) => ok_json(json!({ "diff": diff })),
        Err(message) => internal_error(&message, "Failed to get git diff"),
    }
}

/// GET /api/git/file-diff:取文件修改前后的完整内容(original/modified)、
/// 路径与 isBinary 标记;失败 500。
async fn file_diff(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(path) = query_first(query, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let staged = query_first(query, "staged").as_deref() == Some("true");

    match state.service.get_file_diff(&directory, &path, staged).await {
        Ok(result) => ok_json(json!({
            "original": result.get("original").cloned().unwrap_or(Value::Null),
            "modified": result.get("modified").cloned().unwrap_or(Value::Null),
            "path": result.get("path").cloned().unwrap_or(Value::Null),
            "isBinary": result.get("isBinary") == Some(&Value::Bool(true)),
        })),
        Err(message) => internal_error(&message, "Failed to get git file diff"),
    }
}

/// GET /api/git/range-diff:base..head 区间的 diff,可选 path 限定单文件、
/// context 指定上下文行数;base/head 缺失返回 400。
async fn range_diff(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let base = query_first(query, "base");
    let head = query_first(query, "head");
    if base.as_deref().map(str::is_empty).unwrap_or(true)
        || head.as_deref().map(str::is_empty).unwrap_or(true)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "base and head parameters are required",
        );
    }
    let path = query_first(query, "path").filter(|p| !p.is_empty());
    let context_lines = query_first(query, "context")
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(3);

    match state
        .service
        .get_range_diff(
            &directory,
            base.as_deref().unwrap(),
            head.as_deref().unwrap(),
            path.as_deref(),
            context_lines,
        )
        .await
    {
        Ok(diff) => ok_json(json!({ "diff": diff })),
        Err(message) => internal_error(&message, "Failed to get git range diff"),
    }
}

/// GET /api/git/branch-base:指定分支与基准的 merge-base 提交;
/// branch 参数缺失 400,其余失败 500。
async fn branch_base(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Ok(directory) = require_directory(query) else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(branch) = resolve_directory_query_named(query, "branch") else {
        return error_response(StatusCode::BAD_REQUEST, "branch parameter is required");
    };
    match state.service.get_branch_base(&directory, &branch).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get branch base"),
    }
}

/// resolve_directory_query 的任意参数名版本:取首值、trim、非空才返回 Some。
fn resolve_directory_query_named(query: Option<&str>, name: &str) -> Option<String> {
    query_first(query, name)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// GET /api/git/range-files:base..head 区间内变更的文件列表 {"files"};
/// base/head 缺失 400,其余失败 500。
async fn range_files(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Ok(directory) = require_directory(query) else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let base = resolve_directory_query_named(query, "base");
    let head = resolve_directory_query_named(query, "head");
    if base.is_none() || head.is_none() {
        return error_response(
            StatusCode::BAD_REQUEST,
            "base and head parameters are required",
        );
    }
    match state
        .service
        .get_range_files(
            &directory,
            base.as_deref().unwrap(),
            head.as_deref().unwrap(),
        )
        .await
    {
        Ok(files) => ok_json(json!({ "files": files })),
        Err(message) => internal_error(&message, "Failed to get git range files"),
    }
}

/// POST /api/git/revert:把文件内容恢复(body.path 必填,scope 可选,
/// 控制恢复到 HEAD 还是暂存版本);失败 500。
async fn revert(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(path) = truthy_str(body, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let scope = str_field(body, "scope");
    match state.service.revert_file(&directory, &path, scope).await {
        Ok(()) => ok_json(json!({ "success": true })),
        Err(message) => internal_error(&message, "Failed to revert git file"),
    }
}

/// POST /api/git/stage:暂存文件,转调 stage_unstage(staging=true)。
async fn stage(State(state): State<GitState>, req: Request) -> Response {
    stage_unstage(state, req, true).await
}

/// POST /api/git/unstage:取消暂存,转调 stage_unstage(staging=false)。
async fn unstage(State(state): State<GitState>, req: Request) -> Response {
    stage_unstage(state, req, false).await
}

/// stage/unstage 共用实现:接受 body.paths 数组或单个 body.path,至少一个
/// 非空字符串,否则 400;随后调用 stage_files/unstage_files,失败 500。
async fn stage_unstage(state: GitState, req: Request, staging: bool) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let file_paths: Vec<Value> = match body.get("paths").and_then(Value::as_array) {
        Some(paths) => paths.clone(),
        None => vec![body.get("path").cloned().unwrap_or(Value::Null)],
    };
    let has_valid = file_paths.iter().any(|value| {
        value
            .as_str()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    });
    if !has_valid {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    }

    let outcome = if staging {
        state.service.stage_files(&directory, &file_paths).await
    } else {
        state.service.unstage_files(&directory, &file_paths).await
    };
    match outcome {
        Ok(()) => ok_json(json!({ "success": true })),
        Err(message) => internal_error(
            &message,
            if staging {
                "Failed to stage git file"
            } else {
                "Failed to unstage git file"
            },
        ),
    }
}

/// POST /api/git/apply-hunk:对单文件应用单个 hunk patch;path/patch 必填,
/// action 必须是 stage/unstage/discard 之一(白名单校验,否则 400)。
async fn apply_hunk(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(file_path) = truthy_str(body, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let Some(patch) = str_field(body, "patch").filter(|p| !p.trim().is_empty()) else {
        return error_response(StatusCode::BAD_REQUEST, "patch is required");
    };
    let action = str_field(body, "action").unwrap_or("");
    if !matches!(action, "stage" | "unstage" | "discard") {
        return error_response(
            StatusCode::BAD_REQUEST,
            "action must be stage, unstage, or discard",
        );
    }

    match state
        .service
        .apply_hunk(&directory, &file_path, patch, action)
        .await
    {
        Ok(()) => ok_json(json!({ "success": true })),
        Err(message) => internal_error(&message, "Failed to apply git hunk"),
    }
}

/// POST /api/git/pull:从远端拉取并合并;body 透传给服务层(空 body 归一为
/// {}),失败 500。
async fn pull(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.pull(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to pull from remote"),
    }
}

/// POST /api/git/push:推送到远端;body 透传(remote/branch/force 等),
/// 失败 500。
async fn push(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.push(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to push to remote"),
    }
}

/// GET /api/git/stashes:列出 stash 栈 {"stashes"},失败 500。
async fn stashes_list(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.list_stashes(&directory).await {
        Ok(stashes) => ok_json(json!({ "stashes": stashes })),
        Err(message) => internal_error(&message, "Failed to list stashes"),
    }
}

/// POST /api/git/stashes/file-counts:按 body.refs(stash ref 数组)统计
/// 每个 stash 涉及的文件数量;失败 500。
async fn stashes_file_counts(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state
        .service
        .count_stash_files(&directory, body.get("refs"))
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to count stash files"),
    }
}

/// POST /api/git/stash:创建 stash(body 透传,可含 message/pathspec),
/// 失败 500。
async fn stash_push_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.stash_push(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to stash changes"),
    }
}

/// POST /api/git/stash/apply:应用 stash(不弹出),body 透传,失败 500。
async fn stash_apply_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.stash_apply(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to apply stash"),
    }
}

/// POST /api/git/stash/pop:应用并弹出 stash,body 透传,失败 500。
async fn stash_pop_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.stash_pop(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to pop stash"),
    }
}

/// POST /api/git/stash/drop:丢弃 stash,body 透传,失败 500。
async fn stash_drop_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.stash_drop(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to drop stash"),
    }
}

/// POST /api/git/fetch:从远端 fetch(body 透传,可指定 remote/refspec),
/// 失败 500。
async fn fetch_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.fetch(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to fetch from remote"),
    }
}

/// GET /api/git/remotes:列出全部 remote 及其 URL,失败 500。
async fn remotes_list(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.get_remotes(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get remotes"),
    }
}

/// DELETE /api/git/remotes:删除 remote;body.remote 必填(否则 400),
/// 失败 500。
async fn remotes_delete(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let remote = str_field(body, "remote").unwrap_or("").trim().to_string();
    if remote.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "remote is required");
    }
    match state.service.remove_remote(&directory, &remote).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to remove remote"),
    }
}

/// POST /api/git/rebase:发起 rebase(body 透传:目标分支等),失败 500。
async fn rebase_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.rebase(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to rebase"),
    }
}

/// POST /api/git/rebase/abort:中止进行中的 rebase,失败 500。
async fn rebase_abort(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.abort_rebase(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to abort rebase"),
    }
}

/// POST /api/git/merge:发起 merge(body 透传),失败 500。
async fn merge_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match state.service.merge(&directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to merge"),
    }
}

/// POST /api/git/merge/abort:中止进行中的 merge,失败 500。
async fn merge_abort(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.abort_merge(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to abort merge"),
    }
}

/// POST /api/git/rebase/continue:解决冲突后继续 rebase,失败 500。
async fn rebase_continue(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.continue_rebase(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to continue rebase"),
    }
}

/// POST /api/git/merge/continue:解决冲突后继续 merge,失败 500。
async fn merge_continue(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.continue_merge(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to continue merge"),
    }
}

/// GET /api/git/conflict-details:读取当前冲突文件及双方/基线版本内容,
/// 失败 500。
async fn conflict_details(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.get_conflict_details(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get conflict details"),
    }
}

/// POST /api/git/commit:创建提交;body.message 必填(空串同样 400),
/// 其余选项(amend 等)随 body 整体透传,失败 500。
async fn commit_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(message) = truthy_str(body, "message") else {
        return error_response(StatusCode::BAD_REQUEST, "message is required");
    };
    match state.service.commit(&directory, &message, body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to create commit"),
    }
}

/// GET /api/git/branches:列出本地分支及跟踪状态,失败 500。
async fn branches_list(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match state.service.get_branches(&directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get branches"),
    }
}

/// POST /api/git/branch-push-status:body.branches 必须是字符串数组(缺失或
/// 含非字符串均 400);返回各分支未推送的提交数,失败 500。
async fn branch_push_status(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(branches) = body.get("branches").and_then(Value::as_array) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "branches must be an array of branch names",
        );
    };
    if branches.iter().any(|branch| !branch.is_string()) {
        return error_response(
            StatusCode::BAD_REQUEST,
            "branches must be an array of branch names",
        );
    }
    match state
        .service
        .get_unpushed_branch_counts(&directory, branches)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get branch push status"),
    }
}

/// POST /api/git/branches:创建分支;name 必填,startPoint 可选(缺省从
/// 当前 HEAD),失败 500。
async fn branches_create(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(name) = truthy_str(body, "name") else {
        return error_response(StatusCode::BAD_REQUEST, "name is required");
    };
    let start_point = str_field(body, "startPoint").filter(|s| !s.is_empty());
    match state
        .service
        .create_branch(&directory, &name, start_point)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to create branch"),
    }
}

/// DELETE /api/git/branches:删除本地分支;branch 必填,force=true 走强制
/// 删除,失败 500。
async fn branches_delete(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(branch) = truthy_str(body, "branch") else {
        return error_response(StatusCode::BAD_REQUEST, "branch is required");
    };
    let force = body.get("force") == Some(&Value::Bool(true));
    match state
        .service
        .delete_branch(&directory, &branch, force)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to delete branch"),
    }
}

/// PUT /api/git/branches/rename:重命名分支;oldName/newName 均必填,失败 500。
async fn branches_rename(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(old_name) = truthy_str(body, "oldName") else {
        return error_response(StatusCode::BAD_REQUEST, "oldName is required");
    };
    let Some(new_name) = truthy_str(body, "newName") else {
        return error_response(StatusCode::BAD_REQUEST, "newName is required");
    };
    match state
        .service
        .rename_branch(&directory, &old_name, &new_name)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to rename branch"),
    }
}

/// DELETE /api/git/remote-branches:删除远端分支;body.branch 必填,
/// 其余参数随 body 透传给服务层,失败 500。
async fn remote_branches_delete(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    if truthy_str(body, "branch").is_none() {
        return error_response(StatusCode::BAD_REQUEST, "branch is required");
    }
    match state.service.delete_remote_branch(&directory, body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to delete remote branch"),
    }
}
/// GET /api/git/worktrees:列出仓库 worktree。get_worktrees 内部吞掉非仓库/
/// git 失败并返回 [],因此本路由恒 200;JS 版的 X-OMPChamber-Warning 头在该
/// 吞错实现下不会出现(见内联注释)。
async fn worktrees_list(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    // `getWorktrees` swallows non-repo/git failures and answers `[]`; the JS
    // route adds the warning header when even that fails, which cannot happen
    // with the swallowing implementation — kept for the rare error shape.
    let worktrees = wt::get_worktrees(&state.service, &directory).await;
    ok_json(Value::Array(worktrees))
}

/// POST /api/git/worktrees/validate:透传 validate_worktree_create 的结果
/// (恒 200,由 ok 字段表达校验是否通过与错误列表)。
async fn worktrees_validate(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    ok_json(wt::validate_worktree_create(&state.service, &directory, &body).await)
}

/// POST /api/git/checkout:切换本地分支;branch 必填(空串同样 400),
/// 失败 500。
async fn checkout_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(branch) = truthy_str(body, "branch") else {
        return error_response(StatusCode::BAD_REQUEST, "branch is required");
    };
    match state.service.checkout_branch(&directory, &branch).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to checkout branch"),
    }
}

/// hash 参数不是合法 commit SHA 时的统一 400 响应。
fn invalid_hash_response() -> Response {
    error_response(StatusCode::BAD_REQUEST, "Invalid commit hash")
}

/// POST /api/git/checkout-commit:分离头检出指定提交;hash 必须通过
/// is_valid_commit_hash 校验(否则 400),失败 500。
async fn checkout_commit(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(hash) = str_field(body, "hash").filter(|h| super::paths::is_valid_commit_hash(h))
    else {
        return invalid_hash_response();
    };
    match state.service.checkout_commit(&directory, hash).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to checkout commit"),
    }
}

/// POST /api/git/cherry-pick:拣选指定提交;hash 须为合法 SHA,失败 500。
async fn cherry_pick_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(hash) = str_field(body, "hash").filter(|h| super::paths::is_valid_commit_hash(h))
    else {
        return invalid_hash_response();
    };
    match state.service.cherry_pick(&directory, hash).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to cherry-pick"),
    }
}

/// POST /api/git/revert-commit:反向提交指定 commit;hash 须为合法 SHA,
/// 失败 500。
async fn revert_commit_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(hash) = str_field(body, "hash").filter(|h| super::paths::is_valid_commit_hash(h))
    else {
        return invalid_hash_response();
    };
    match state.service.revert_commit(&directory, hash).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to revert commit"),
    }
}

/// POST /api/git/reset-to-commit:重置到指定提交;hash 须为合法 SHA,
/// mode 必须是 soft/mixed/hard 之一(否则 400),force 透传;失败 500。
async fn reset_to_commit_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(hash) = str_field(body, "hash").filter(|h| super::paths::is_valid_commit_hash(h))
    else {
        return invalid_hash_response();
    };
    let mode = str_field(body, "mode").unwrap_or("");
    if !matches!(mode, "soft" | "mixed" | "hard") {
        return error_response(StatusCode::BAD_REQUEST, "mode must be soft, mixed, or hard");
    }
    let force = body.get("force") == Some(&Value::Bool(true));
    match state
        .service
        .reset_to_commit(&directory, hash, mode, force)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to reset"),
    }
}

/// POST /api/git/worktrees:创建 worktree(支持 returnAfterDirectoryCreated
/// 快速路径与后台 bootstrap);失败 500。
async fn worktrees_create(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match wt::create_worktree(&state.service, &directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to create worktree"),
    }
}

/// POST /api/git/worktrees/preview:预览将要创建的 worktree 名称/分支/路径,
/// 无副作用;失败 500。
async fn worktrees_preview(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = if body.is_null() { json!({}) } else { body };
    match wt::preview_worktree_create(&state.service, &directory, &body).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to preview worktree"),
    }
}

/// GET /api/git/worktrees/bootstrap-status:查询目录后台引导进度
/// (status/phase/error/updatedAt);失败 500。
async fn worktrees_bootstrap_status(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    match wt::get_worktree_bootstrap_status(&state.service, &directory).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get worktree bootstrap status"),
    }
}

/// DELETE /api/git/worktrees:删除 worktree。body.directory 指向待删的
/// worktree(必填),deleteLocalBranch 透传;成功返回 {"success":true},
/// 失败 500。
async fn worktrees_delete(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let worktree_directory = str_field(body, "directory").unwrap_or("");
    if worktree_directory.is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "worktree directory is required");
    }
    let input = json!({
        "directory": worktree_directory,
        "deleteLocalBranch": body.get("deleteLocalBranch") == Some(&Value::Bool(true)),
    });
    match wt::remove_worktree(&state.service, &directory, &input).await {
        Ok(result) => ok_json(json!({ "success": result })),
        Err(message) => internal_error(&message, "Failed to remove worktree"),
    }
}

/// GET /api/git/worktree-type:返回 { linked },表示目录是否为 linked worktree。
async fn worktree_type(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let linked = state.service.is_linked_worktree(&directory).await;
    ok_json(json!({ "linked": linked }))
}

/// POST /api/git/validate-directory:校验 directory 能否作为 worktree 使用
/// (相对 worktreeRoot);两个 body 字段均必填,恒 200 返回校验结果。
async fn validate_directory_route(State(state): State<GitState>, req: Request) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(directory) = truthy_str(body, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory is required");
    };
    let Some(worktree_root) = truthy_str(body, "worktreeRoot") else {
        return error_response(StatusCode::BAD_REQUEST, "worktreeRoot is required");
    };
    ok_json(
        state
            .service
            .validate_worktree_directory(&directory, &worktree_root)
            .await,
    )
}

/// POST /api/git/canonicalize-worktree-state:规范化目录的 worktree 状态
/// (分支/游离头等);directory 必填,恒 200。
async fn canonicalize_worktree_state_route(
    State(state): State<GitState>,
    req: Request,
) -> Response {
    let body = match read_json_body(req.into_body()).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let body = body_object(&body);
    let Some(directory) = truthy_str(body, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory is required");
    };
    ok_json(state.service.canonicalize_worktree_state(&directory).await)
}

/// GET /api/git/log:提交历史。查询参数映射为 LogOptions:maxCount/from/to/
/// file 与 all=true;失败 500。
async fn log_route(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let options = super::service_ops::LogOptions {
        max_count: query_first(query, "maxCount").and_then(|value| value.parse::<i64>().ok()),
        from: query_first(query, "from"),
        to: query_first(query, "to"),
        file: query_first(query, "file"),
        all: query_first(query, "all").as_deref() == Some("true"),
    };
    match state.service.get_log(&directory, &options).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get commit log"),
    }
}

/// GET /api/git/commit-files:指定提交涉及的文件列表;hash 必填,失败 500。
async fn commit_files(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(hash) = query_first(query, "hash") else {
        return error_response(StatusCode::BAD_REQUEST, "hash parameter is required");
    };
    match state.service.get_commit_files(&directory, &hash).await {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get commit files"),
    }
}

/// GET /api/git/commit-file-diff:指定提交中某文件的内容;hash 须为合法
/// SHA(否则 400),path 必填,binary=true 控制二进制输出模式;失败 500。
async fn commit_file_diff(State(state): State<GitState>, req: Request) -> Response {
    let query = req.uri().query();
    let Some(directory) = query_first(query, "directory") else {
        return error_response(StatusCode::BAD_REQUEST, "directory parameter is required");
    };
    let Some(hash) = query_first(query, "hash") else {
        return error_response(StatusCode::BAD_REQUEST, "hash parameter is required");
    };
    if !super::paths::is_valid_commit_hash(&hash) {
        return error_response(StatusCode::BAD_REQUEST, "hash must be a valid commit SHA");
    }
    let Some(path) = query_first(query, "path") else {
        return error_response(StatusCode::BAD_REQUEST, "path parameter is required");
    };
    let is_binary = query_first(query, "binary").as_deref() == Some("true");
    match state
        .service
        .get_commit_file_diff(&directory, &hash, &path, is_binary)
        .await
    {
        Ok(value) => ok_json(value),
        Err(message) => internal_error(&message, "Failed to get commit file diff"),
    }
}

// ---------------------------------------------------------------------------
// Router assembly
// ---------------------------------------------------------------------------

/// 组装并返回全部 /api/git/* 路由(挂到 Router<GitState>);调用方负责
/// with_state(GitState) 后并入主 Router。路由清单与 server/lib/git/routes.js
/// 的 registerGitRoutes 一一对应。
pub fn routes() -> Router<GitState> {
    Router::new()
        .route(
            "/api/git/identities",
            get(identities_list).post(identities_create),
        )
        .route(
            "/api/git/identities/{id}",
            put(identities_update).delete(identities_delete),
        )
        .route("/api/git/global-identity", get(global_identity))
        .route("/api/git/discover-credentials", get(discover_credentials))
        .route("/api/git/check", get(git_check))
        .route("/api/git/remote-url", get(remote_url))
        .route("/api/git/current-identity", get(current_identity))
        .route("/api/git/has-local-identity", get(has_local_identity))
        .route("/api/git/set-identity", post(set_identity))
        .route("/api/git/status", get(status))
        .route("/api/git/primary-root", get(primary_root))
        .route("/api/git/toplevel", get(toplevel))
        .route("/api/git/commit-summaries", post(commit_summaries))
        .route("/api/git/integrate/plan", post(integrate_plan))
        .route(
            "/api/git/integrate/conflict-details",
            post(integrate_conflict_details),
        )
        .route(
            "/api/git/integrate/cherry-pick-status",
            post(integrate_cherry_pick_status),
        )
        .route("/api/git/integrate/run", post(integrate_run))
        .route("/api/git/integrate/abort", post(integrate_abort))
        .route("/api/git/integrate/continue", post(integrate_continue))
        .route("/api/git/diff", get(diff))
        .route("/api/git/file-diff", get(file_diff))
        .route("/api/git/range-diff", get(range_diff))
        .route("/api/git/branch-base", get(branch_base))
        .route("/api/git/range-files", get(range_files))
        .route("/api/git/revert", post(revert))
        .route("/api/git/stage", post(stage))
        .route("/api/git/unstage", post(unstage))
        .route("/api/git/apply-hunk", post(apply_hunk))
        .route("/api/git/pull", post(pull))
        .route("/api/git/push", post(push))
        .route("/api/git/stashes", get(stashes_list))
        .route("/api/git/stashes/file-counts", post(stashes_file_counts))
        .route("/api/git/stash", post(stash_push_route))
        .route("/api/git/stash/apply", post(stash_apply_route))
        .route("/api/git/stash/pop", post(stash_pop_route))
        .route("/api/git/stash/drop", post(stash_drop_route))
        .route("/api/git/fetch", post(fetch_route))
        .route("/api/git/remotes", get(remotes_list).delete(remotes_delete))
        .route("/api/git/rebase", post(rebase_route))
        .route("/api/git/rebase/abort", post(rebase_abort))
        .route("/api/git/rebase/continue", post(rebase_continue))
        .route("/api/git/merge", post(merge_route))
        .route("/api/git/merge/abort", post(merge_abort))
        .route("/api/git/merge/continue", post(merge_continue))
        .route("/api/git/conflict-details", get(conflict_details))
        .route("/api/git/commit", post(commit_route))
        .route(
            "/api/git/branches",
            get(branches_list)
                .post(branches_create)
                .delete(branches_delete),
        )
        .route("/api/git/branches/rename", put(branches_rename))
        .route("/api/git/branch-push-status", post(branch_push_status))
        .route("/api/git/remote-branches", delete(remote_branches_delete))
        .route("/api/git/checkout", post(checkout_route))
        .route("/api/git/checkout-commit", post(checkout_commit))
        .route("/api/git/cherry-pick", post(cherry_pick_route))
        .route("/api/git/revert-commit", post(revert_commit_route))
        .route("/api/git/reset-to-commit", post(reset_to_commit_route))
        .route(
            "/api/git/worktrees",
            get(worktrees_list)
                .post(worktrees_create)
                .delete(worktrees_delete),
        )
        .route("/api/git/worktrees/validate", post(worktrees_validate))
        .route("/api/git/worktrees/preview", post(worktrees_preview))
        .route(
            "/api/git/worktrees/bootstrap-status",
            get(worktrees_bootstrap_status),
        )
        .route("/api/git/worktree-type", get(worktree_type))
        .route(
            "/api/git/validate-directory",
            post(validate_directory_route),
        )
        .route(
            "/api/git/canonicalize-worktree-state",
            post(canonicalize_worktree_state_route),
        )
        .route("/api/git/log", get(log_route))
        .route("/api/git/commit-files", get(commit_files))
        .route("/api/git/commit-file-diff", get(commit_file_diff))
}
