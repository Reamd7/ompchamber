//! Port of `server/lib/session-folders/routes.js`.
//!
//! Persists the UI's session folder tree at `<data-dir>/sessions-directories.json`
//! with shape validation, last-writer-wins ordering (`updatedAt`), atomic
//! temp+rename writes, and serialized saves. A valid incoming snapshot
//! repairs malformed prior state; a stale snapshot is ignored, not rejected.
//!
//! 中文说明：本模块把 UI 的会话文件夹树持久化到
//! `<data-dir>/sessions-directories.json`。GET 返回存储快照（文件缺失时
//! 返回 `{version:1, exists:false}` 默认体；JSON 损坏或形状非法返回 500）；
//! POST 先做结构校验与体积上限检查，再按 `updatedAt` 实行最后写入者胜：
//! 磁盘上不旧于来稿的有效快照会让来稿被忽略（`ignored:true`，而非报错），
//! 损坏的旧状态则被有效来稿修复。写入经临时文件 + rename 原子落盘，
//! 并由 save_queue 串行化，避免并发写竞争。

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;

/// 请求体序列化后的最大允许字节数（4 MiB）：超过时 POST 返回 413，
/// 防止畸大的快照拖垮磁盘写入。
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// 判断 JSON 值是否为对象（map）；数组、字符串等其它类型返回 false。
/// 对应 JS 版对 body 顶层的 `typeof === "object"` 形状检查。
fn is_object_record(value: &Value) -> bool {
    value.as_object().is_some()
}

/// 判断数值是否可解析为有限的 f64（用于 `updatedAt` 校验）。
/// 注意：函数名虽含 positive，但实现只检查有限性——正值约束由调用方
/// （`n > 0.0`）另行判断。
fn finite_positive_number(value: &Value) -> bool {
    value.as_f64().is_some_and(f64::is_finite)
}

/// 校验单个文件夹记录的形状：`id`、`name` 必须是字符串，`sessionIds`
/// 必须是字符串数组，`createdAt` 必须是有限数值，`parentId` 缺省、
/// 为 null 或字符串均可（根文件夹无父级）。
fn has_valid_folder_shape(folder: &Value) -> bool {
    let Some(map) = folder.as_object() else {
        return false;
    };
    map.get("id").is_some_and(Value::is_string)
        && map.get("name").is_some_and(Value::is_string)
        && map
            .get("sessionIds")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().all(Value::is_string))
        && map
            .get("createdAt")
            .map(finite_positive_number_at_least_zero)
            .unwrap_or(false)
        && match map.get("parentId") {
            None | Some(Value::Null) => true,
            Some(value) => value.is_string(),
        }
}

/// `createdAt` 校验：数值有限即可（允许 0 与负值），与 JS 版语义一致；
/// 与 `finite_positive_number` 实现相同，按 JS 版命名单列。
fn finite_positive_number_at_least_zero(value: &Value) -> bool {
    value.as_f64().is_some_and(|n| n.is_finite())
}

/// 校验 `foldersMap` 的形状：必须是对象，且每个值（某个目录下的文件夹
/// 列表）都必须是数组、数组内每个元素都通过 `has_valid_folder_shape`。
fn has_valid_folders_map_shape(folders_map: &Value) -> bool {
    let Some(map) = folders_map.as_object() else {
        return false;
    };
    map.values().all(|folders| {
        folders
            .as_array()
            .is_some_and(|list| list.iter().all(has_valid_folder_shape))
    })
}

/// 校验整份快照的形状：顶层是对象、`version == 1`、`foldersMap` 形状
/// 合法、`collapsedFolderIds` 是字符串数组。
fn has_valid_folder_snapshot_shape(snapshot: &Value) -> bool {
    is_object_record(snapshot)
        && snapshot.get("version").and_then(Value::as_i64) == Some(1)
        && snapshot
            .get("foldersMap")
            .map(has_valid_folders_map_shape)
            .unwrap_or(false)
        && snapshot
            .get("collapsedFolderIds")
            .and_then(Value::as_array)
            .is_some_and(|ids| ids.iter().all(Value::is_string))
}

/// 路由共享状态：持久化目标文件路径 + 串行化保存用的异步锁
/// （对应 JS 版的 saveQueue，保证同一时刻只有一个写者）。
#[derive(Clone)]
struct ModuleState {
    /// 持久化目标：`<data-dir>/sessions-directories.json`。
    file_path: PathBuf,
    /// 保存互斥锁：串行化保存，避免并发 rename 相互覆盖。
    save_queue: Arc<tokio::sync::Mutex<()>>,
}

/// 构造 `{ "error": message }` 的 JSON 错误响应（状态码 + 消息，
/// 报错格式与 JS 版保持一致）。
fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

/// 读取文件原文：文件不存在返回 `Ok(None)`（视为"尚无快照"），
/// 其余 I/O 错误原样上抛。
async fn read_raw(path: &PathBuf) -> std::io::Result<Option<String>> {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// GET /api/session-folders：读取并返回存储的快照。
/// 文件缺失 → 200 + `{version:1, exists:false}`；JSON 损坏 → 500
/// "Stored session folders are malformed"；形状非法或 `updatedAt`
/// 非正有限数 → 500 "Stored session folders have an invalid shape"；
/// 正常路径在快照上附加 `exists:true` 后原样返回。
async fn get_folders(State(state): State<ModuleState>) -> Response {
    let raw = match read_raw(&state.file_path).await {
        Ok(raw) => raw,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let Some(raw) = raw else {
        return Json(json!({ "version": 1, "exists": false })).into_response();
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Stored session folders are malformed",
            );
        }
    };
    let updated_at_valid = parsed
        .get("updatedAt")
        .map(finite_positive_number)
        .unwrap_or(false)
        && parsed
            .get("updatedAt")
            .and_then(Value::as_f64)
            .is_some_and(|n| n > 0.0);
    if !has_valid_folder_snapshot_shape(&parsed) || !updated_at_valid {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Stored session folders have an invalid shape",
        );
    }
    let mut body = parsed;
    if let Some(map) = body.as_object_mut() {
        map.insert("exists".to_string(), Value::Bool(true));
    }
    Json(body).into_response()
}

/// POST /api/session-folders：校验并保存来稿快照。
/// 流程：body 必须是对象（否则 400 "Body must be an object"）→ 快照形状
/// 合法（400 "Invalid session folders payload"）→ 序列化后不超过
/// MAX_BODY_BYTES（413）→ `updatedAt` 必须是正有限数（400）。随后持锁做
/// 最后写入者胜：磁盘上有效且 `updatedAt` 不早于来稿的快照直接忽略来稿
/// （200 `{success:true, ignored:true}`）；否则创建父目录、写临时文件并
/// rename 原子落盘（200 `{success:true}`）。旧状态损坏时跳过新旧的比较，
/// 由本次有效写入修复；失败路径清理残留的临时文件。
async fn post_folders(State(state): State<ModuleState>, Json(body): Json<Value>) -> Response {
    if !is_object_record(&body) {
        return error_response(StatusCode::BAD_REQUEST, "Body must be an object");
    }
    if !has_valid_folder_snapshot_shape(&body) {
        return error_response(StatusCode::BAD_REQUEST, "Invalid session folders payload");
    }
    let serialized = match serde_json::to_string_pretty(&body) {
        Ok(serialized) => serialized,
        Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if serialized.len() > MAX_BODY_BYTES {
        return error_response(StatusCode::PAYLOAD_TOO_LARGE, "Payload too large");
    }
    let updated_at = body.get("updatedAt").and_then(Value::as_f64);
    match updated_at {
        Some(updated_at) if updated_at.is_finite() && updated_at > 0.0 => {}
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "updatedAt must be a positive finite number",
            );
        }
    }
    let updated_at = updated_at.expect("checked above");

    // Serialized saves (JS saveQueue): one writer at a time.
    let _guard = state.save_queue.lock().await;
    let tmp =
        state
            .file_path
            .with_extension(format!("json.tmp-{}-{}", std::process::id(), utc_millis(),));
    let mut saved = false;
    let result: Result<Response, std::io::Error> = async {
        if let Some(current_raw) = read_raw(&state.file_path).await? {
            // A valid current snapshot newer-or-equal wins; malformed prior
            // state falls through and is repaired by this save.
            if let Ok(current) = serde_json::from_str::<Value>(&current_raw) {
                let current_updated_at = if has_valid_folder_snapshot_shape(&current) {
                    current
                        .get("updatedAt")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                if current_updated_at >= updated_at {
                    return Ok(Json(json!({ "success": true, "ignored": true })).into_response());
                }
            }
        }
        if let Some(parent) = state.file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&tmp, &serialized).await?;
        match tokio::fs::rename(&tmp, &state.file_path).await {
            Ok(()) => {
                saved = true;
                Ok(Json(json!({ "success": true })).into_response())
            }
            Err(e) => Err(e),
        }
    }
    .await;

    match result {
        Ok(response) => response,
        Err(e) => {
            if !saved {
                let _ = tokio::fs::remove_file(&tmp).await;
            }
            error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

/// 当前 UTC 毫秒时间戳（UNIX_EPOCH 起算；时钟早于 epoch 时退化为 0），
/// 仅用于临时文件命名保证唯一性。
fn utc_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

/// 构建 /api/session-folders 路由（GET 读取、POST 保存），
/// 状态指向 data-dir 下的 sessions-directories.json 与共享保存锁。
pub fn router(ctx: RouterContext) -> Router {
    let file_path = ctx.config.data_dir.join("sessions-directories.json");
    Router::new()
        .route("/api/session-folders", get(get_folders).post(post_folders))
        .with_state(ModuleState {
            file_path,
            save_queue: Arc::new(tokio::sync::Mutex::new(())),
        })
}
/// 覆盖读取/保存两侧的契约：默认快照、损坏与非法形状的区分报错、
/// 写入往返、过期快照的忽略，以及坏状态的修复。
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    /// 测试夹具：独立临时数据目录 + 与生产 router 相同配置构建的
    /// axum 应用，供各用例发请求。
    struct Fixture {
        /// 每个测试独占的临时目录，夹具文件写在这里。
        dir: std::path::PathBuf,
    }

    /// 夹具的辅助方法：构建应用、发起请求、解析响应体。
    impl Fixture {
        /// 创建带唯一后缀（tag + 毫秒时间戳 + 进程号）的临时目录，
        /// 避免并发测试相互干扰。
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "oc-session-folders-{tag}-{}-{}",
                utc_millis(),
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Fixture { dir }
        }

        /// 构建与生产 router 等价的测试应用（同一路由与 ModuleState）。
        fn app(&self) -> Router {
            Router::new()
                .route("/api/session-folders", get(get_folders).post(post_folders))
                .with_state(ModuleState {
                    file_path: self.dir.join("sessions-directories.json"),
                    save_queue: Arc::new(tokio::sync::Mutex::new(())),
                })
        }

        /// 以 JSON body 发起 POST /api/session-folders 并返回响应。
        async fn post(&self, value: Value) -> axum::response::Response {
            self.app()
                .oneshot(
                    axum::http::Request::post("/api/session-folders")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_string(&value).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap()
        }

        /// 发起 GET /api/session-folders 并返回响应。
        async fn get(&self) -> axum::response::Response {
            self.app()
                .oneshot(
                    axum::http::Request::get("/api/session-folders")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        }

        /// 把响应体完整读出并解析为 JSON 值（测试断言用）。
        async fn body_json(response: axum::response::Response) -> Value {
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }
    }

    /// 构造一份形状合法的最小快照：单个目录 "/repo" 下一个文件夹，
    /// `updatedAt` 由参数指定（驱动新旧的比较分支）。
    fn sample_snapshot(updated_at: f64) -> Value {
        json!({
            "version": 1,
            "foldersMap": {
                "/repo": [
                    { "id": "f1", "name": "WIP", "sessionIds": ["s1"], "createdAt": 1.0, "parentId": null }
                ]
            },
            "collapsedFolderIds": [],
            "updatedAt": updated_at
        })
    }

    /// 行为契约：文件缺失时 GET 返回 200 与默认体 `{version:1, exists:false}`。
    #[tokio::test]
    async fn get_missing_answers_default_snapshot() {
        let fixture = Fixture::new("missing");
        let response = fixture.get().await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["version"], 1);
        assert_eq!(value["exists"], false);
    }

    /// 行为契约：JSON 损坏与形状/updatedAt 非法分别返回各自的 500 错误消息。
    #[tokio::test]
    async fn get_malformed_and_invalid_shape_fail_distinctly() {
        let fixture = Fixture::new("malformed");
        std::fs::write(fixture.dir.join("sessions-directories.json"), "{nope").unwrap();
        let response = fixture.get().await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["error"], "Stored session folders are malformed");

        std::fs::write(
            fixture.dir.join("sessions-directories.json"),
            serde_json::to_string(&json!({
                "version": 1, "foldersMap": {}, "collapsedFolderIds": []
            }))
            .unwrap(),
        )
        .unwrap();
        let response = fixture.get().await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let value = Fixture::body_json(response).await;
        assert_eq!(
            value["error"],
            "Stored session folders have an invalid shape"
        );
    }

    /// 行为契约：快照可写入并读回；`updatedAt` 更旧的来稿被忽略（ignored）而非报错。
    #[tokio::test]
    async fn post_roundtrip_and_stale_snapshot_ignored() {
        let fixture = Fixture::new("roundtrip");

        let response = fixture.post(sample_snapshot(10.0)).await;
        assert_eq!(response.status(), StatusCode::OK);

        // Stale write is ignored, not an error.
        let response = fixture.post(sample_snapshot(5.0)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["ignored"], true);

        let response = fixture.get().await;
        let value = Fixture::body_json(response).await;
        assert_eq!(value["exists"], true);
        assert_eq!(value["updatedAt"], 10.0);
    }

    /// 行为契约：非对象 body、非法形状与非法 `updatedAt` 分别被 400 拒绝，消息与 JS 版一致。
    #[tokio::test]
    async fn post_rejects_invalid_payloads_with_js_messages() {
        let fixture = Fixture::new("reject");

        let response = fixture.post(json!([1])).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["error"], "Body must be an object");

        let mut bad_shape = sample_snapshot(1.0);
        bad_shape["version"] = json!(2);
        let response = fixture.post(bad_shape).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let mut no_updated = sample_snapshot(0.0);
        no_updated["updatedAt"] = json!(0);
        let response = fixture.post(no_updated).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// 行为契约：磁盘旧状态损坏时，合法来稿直接覆盖修复并返回 success。
    #[tokio::test]
    async fn valid_snapshot_repairs_malformed_prior_state() {
        let fixture = Fixture::new("repair");
        std::fs::write(fixture.dir.join("sessions-directories.json"), "{broken").unwrap();
        let response = fixture.post(sample_snapshot(3.0)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let value = Fixture::body_json(response).await;
        assert_eq!(value["success"], true);
    }
}
