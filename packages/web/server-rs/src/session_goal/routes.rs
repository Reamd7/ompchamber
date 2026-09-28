//! Port of `server/lib/session-goal/routes.js`.
//!
//! OMPChamber-owned routes for file-backed goal objectives, keyed by session
//! id (one goal per session; a new goal overwrites the old file). The UI
//! writes the objective file before stamping the goal metadata (which only
//! carries an `objectiveFile: true` flag), reads it back for display, and
//! deletes it when the goal is removed.
//! （中文概览）路由按 session id 键控目标文件（一会话一目标，新目标覆盖
//! 旧文件）：UI 先写 objective 文件、再在 goal 元数据上打 objectiveFile
//! 标记，读取用于展示，删除随目标移除。

use std::path::PathBuf;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::session_goal::objectives::{delete_objective, read_objective, write_objective};

/// 构建目标文件路由：`/api/goals/objective/{sessionId}` 的 PUT/GET/DELETE，
/// 以 data_dir 作为共享 state。
pub fn routes(data_dir: PathBuf) -> Router {
    Router::new()
        .route(
            "/api/goals/objective/{sessionId}",
            put(put_objective)
                .get(get_objective)
                .delete(delete_objective_route),
        )
        .with_state(data_dir)
}

/// JS relies on express's `express.json()` body parser: only `*json` content
/// types populate `req.body`; anything else (or a malformed body) leaves it
/// undefined, which writeObjective answers with 400 "objective content is
/// required".
/// 复刻 express `express.json()` 的行为：仅当 Content-Type 含 "json" 时
/// 尝试解析请求体，否则（或解析失败时）返回 `Null`，交由写入方回答 400。
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

/// PUT 处理器：取请求体 `content` 字段写入目标文件；成功返回 `{"ok":true}`，
/// 失败按 ObjectiveError::status_code 的状态码回答，且仅在 5xx 时记录
/// error 日志。
async fn put_objective(
    State(data_dir): State<PathBuf>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let parsed = parse_json_body(&headers, &body);
    let content = parsed.get("content").cloned().unwrap_or(Value::Null);
    match write_objective(&data_dir, &session_id, &content).await {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(error) => {
            if error.status_code() >= 500 {
                tracing::error!("Failed to write goal objective: {error}");
            }
            (
                StatusCode::from_u16(error.status_code())
                    .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(json!({ "error": error.to_string() })),
            )
                .into_response()
        }
    }
}

/// GET 处理器：命中返回 `{"content":…}`；目标缺失或不可读时回答 404。
async fn get_objective(
    State(data_dir): State<PathBuf>,
    Path(session_id): Path<String>,
) -> Response {
    match read_objective(&data_dir, &session_id).await {
        Some(content) => Json(json!({ "content": content })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "objective not found" })),
        )
            .into_response(),
    }
}

/// DELETE 处理器：尽力删除目标文件，无论文件是否存在一律回答 `{"ok":true}`。
async fn delete_objective_route(
    State(data_dir): State<PathBuf>,
    Path(session_id): Path<String>,
) -> Response {
    delete_objective(&data_dir, &session_id).await;
    Json(json!({ "ok": true })).into_response()
}

/// routes 模块测试套件：用 tower 的 oneshot 直接驱动 axum Router，
/// 验证各方法的响应形态与状态码。
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    /// 为测试创建按标签隔离的临时目录（带进程 id 防并行冲突）。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-session-goal-routes-{tag}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// 把响应体完整读出为 UTF-8 字符串，便于断言 JSON 形态。
    async fn body_string(response: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    /// 契约：PUT/GET/DELETE 往返分别返回 `{"ok":true}` 与 `{"content":…}`，
    /// 删除后再 GET 变为 404。
    #[tokio::test]
    async fn put_get_delete_objective_roundtrip_shapes() {
        let dir = temp_dir("roundtrip");
        let app = routes(dir.clone());

        let put = app
            .clone()
            .oneshot(
                axum::http::Request::put("/api/goals/objective/ses_route1")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"Finish the task"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put.status(), StatusCode::OK);
        assert_eq!(body_string(put).await, r#"{"ok":true}"#);

        let get = app
            .clone()
            .oneshot(
                axum::http::Request::get("/api/goals/objective/ses_route1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get.status(), StatusCode::OK);
        assert_eq!(body_string(get).await, r#"{"content":"Finish the task"}"#);

        let delete = app
            .clone()
            .oneshot(
                axum::http::Request::delete("/api/goals/objective/ses_route1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete.status(), StatusCode::OK);
        assert_eq!(body_string(delete).await, r#"{"ok":true}"#);

        let get_after = app
            .oneshot(
                axum::http::Request::get("/api/goals/objective/ses_route1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_after.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_string(get_after).await,
            r#"{"error":"objective not found"}"#
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 契约：非法 session id 与缺失 content（含非 JSON Content-Type）回答
    /// 400 与对应错误文案；DELETE 对任意 id 恒为 200。
    #[tokio::test]
    async fn put_rejects_invalid_session_id_and_missing_content() {
        let dir = temp_dir("rejects");
        let app = routes(dir.clone());

        let invalid = app
            .clone()
            .oneshot(
                axum::http::Request::put("/api/goals/objective/..%2Fescape")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"content":"text"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(invalid).await,
            r#"{"error":"invalid session id"}"#
        );

        let empty = app
            .clone()
            .oneshot(
                axum::http::Request::put("/api/goals/objective/ses_route2")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(empty).await,
            r#"{"error":"objective content is required"}"#
        );

        // Non-JSON content types never populate the body (express behavior).
        let text = app
            .clone()
            .oneshot(
                axum::http::Request::put("/api/goals/objective/ses_route2")
                    .header("content-type", "text/plain")
                    .body(Body::from("{\"content\":\"text\"}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(text.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_string(text).await,
            r#"{"error":"objective content is required"}"#
        );

        // DELETE is always ok, even for ids that were never valid.
        let delete = app
            .oneshot(
                axum::http::Request::delete("/api/goals/objective/never-there")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(delete.status(), StatusCode::OK);

        std::fs::remove_dir_all(&dir).ok();
    }
}
