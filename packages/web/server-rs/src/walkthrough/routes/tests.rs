//! Tests ported from `server/lib/walkthrough/routes.test.js` (over oneshot
//! requests instead of a live socket — axum handlers complete regardless of
//! client disconnects, which is the behavior those tests existed to protect).
//!
//! 中文说明：walkthrough HTTP 路由层的集成测试。通过 tower 的 oneshot
//! 请求直接驱动 axum Router，不建立真实 socket——axum handler 即便客户端
//! 断开也会执行完毕，这正是原 JS socket 测试要守护的行为。git 与模型均为
//! fake：diff 固定为单文件单 hunk，模型即时返回预置 walkthrough，各用例
//! 据此断言状态码、JSON 响应体以及 service 侧副作用。

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::walkthrough::routes::routes;
use crate::walkthrough::service::WalkthroughService;
use crate::walkthrough::small_model::{
    GenerateTextOutput, GenerateTextRequest, ModelDescription, SmallModelError, SmallModelSeam,
};
use crate::walkthrough::sources::GitDeps;

/// 构造默认的 source 参数值（working-tree、scope=all），
/// 与前端发送的 JSON 结构一致，供 read 与 generate 请求复用。
fn source_value() -> Value {
    json!({ "kind": "working-tree", "scope": "all" })
}

/// fake git 返回的工作区 diff：src/a.ts 新增一行，
/// 解析后恰好得到 1 个 hunk（对应响应中 hunkCount=1）。
const PATCH: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,1 +1,2 @@\n+const added = true;\n";

/// fake 模型即时返回的 walkthrough JSON 文本：单章节单 stop，
/// 引用 hunk 别名 h1，与 PATCH 中的 hunk 对应，供用例断言响应字段。
const RESPONSE: &str = r#"{"title":"Change","focus":"why","chapters":[{"title":"Data","icon":"doc","blurb":"","stops":[{"title":"Adds a flag","hunks":["h1"],"importance":"normal","prose":"It adds a flag."}]}]}"#;

/// 创建当前用例专属的临时目录作为 data_dir（进程 id 加原子计数器保证
/// 并发唯一），调用方负责在用例末尾删除。
fn temp_data_dir(label: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "walkthrough-routes-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 构造一个已配置且已登录的 Anthropic haiku 模型描述，
/// 作为 SmallModelSeam 中 describe 回调的固定返回值。
fn model() -> ModelDescription {
    ModelDescription {
        provider_id: "anthropic".to_string(),
        model_id: "claude-haiku-4-5".to_string(),
        source: "config".to_string(),
        has_login: Some(true),
        input_char_budget: 1_000_000,
        context_tokens: Some(200_000),
        context_known: Some(true),
        output_tokens: None,
        structured_output: Some(true),
        output_token_limit: None,
    }
}

/// 构造一套 fake GitDeps：repository_root 固定解析为 /repo；
/// unstaged diff 返回 PATCH、staged diff 为空；
/// range diff 与 untracked 均返回空，避免测试触碰真实 git。
fn fake_git() -> GitDeps {
    GitDeps {
        repository_root: Arc::new(|_d| Box::pin(async { Ok("/repo".to_string()) })),
        diff: Arc::new(|_d, staged| {
            Box::pin(async move {
                Ok(if staged {
                    String::new()
                } else {
                    PATCH.to_string()
                })
            })
        }),
        range_diff: Arc::new(|_d, _b, _h| Box::pin(async { Ok(String::new()) })),
        untracked_paths: Arc::new(|_d| Box::pin(async { Ok(Vec::new()) })),
        untracked_diffs: Arc::new(|_d, _p| Box::pin(async { Ok(Vec::new()) })),
    }
}

/// Routes over a full fake service: fake git (one modified file), a model
/// that answers instantly. Returns (router, service) for extra assertions.
/// 中文补充：组装完整被测栈——即时应答的 SmallModelSeam、fake git 与独立
/// 临时 data_dir。返回的 service 句柄供用例做额外断言（如 is_generating），
/// data_dir 供用例清理。
fn test_app(label: &str) -> (axum::Router, Arc<WalkthroughService>, std::path::PathBuf) {
    let seam = SmallModelSeam {
        describe: Arc::new(|_request| Box::pin(async { Ok(Some(model())) })),
        generate: Arc::new(|_request: GenerateTextRequest| {
            Box::pin(async {
                Ok(GenerateTextOutput {
                    text: RESPONSE.to_string(),
                })
            })
        }),
    };
    let data_dir = temp_data_dir(label);
    let service = WalkthroughService::new(data_dir.clone(), fake_git(), seam, None);
    (routes(Arc::clone(&service)), service, data_dir)
}

/// 以 oneshot 方式把请求直接送进 router，返回状态码与解析后的 JSON body；
/// 空 body 或非 JSON body 一律折叠为 Value::Null，便于调用方统一断言。
async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

/// 拼 read 端点的查询串：directory=/repo 加上 percent-encoding
/// 之后的 source JSON。
fn source_query() -> String {
    format!(
        "directory=/repo&source={}",
        urlencoding_of(&source_value().to_string())
    )
}

/// The JS client sends `encodeURIComponent(JSON.stringify(source))`.
/// 中文补充：按 RFC 3986 unreserved 集合（字母数字与 - _ . ~）原样保留，
/// 其余字节转为大写 %XX；对本测试用到的 JSON 字符，输出与 JS 端
/// encodeURIComponent 一致，保证测试构造 URL 的方式与真实客户端相同。
fn urlencoding_of(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 构造 POST /api/walkthrough/generate 请求：content-type 为
/// application/json，body 由传入的 Value 序列化而来。
fn generate_request(body: Value) -> Request<Body> {
    Request::post("/api/walkthrough/generate")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// 契约：未被取消的 generate 请求返回 200 与 walkthrough 本体，
/// fromCache=false、language 回显 en、hunkCount 与 fake diff 的 1 一致。
#[tokio::test]
async fn answers_a_generation_request_that_nobody_interrupted() {
    let (router, _service, data_dir) = test_app("happy");

    let (status, body) = send(
        &router,
        generate_request(json!({ "directory": "/repo", "source": source_value() })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["walkthrough"]["title"], json!("Change"));
    assert_eq!(body["fromCache"], json!(false));
    assert_eq!(body["language"], json!("en"));
    assert_eq!(body["hunkCount"], json!(1));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：页面刷新后重连的客户端先通过 read 拿到已生成的 walkthrough
/// （generating=false），随后再次 generate 命中缓存（fromCache=true）——
/// 首个请求即使原客户端已消失，也已把结果写入缓存。
#[tokio::test]
async fn a_reconnected_client_sees_the_cached_result() {
    let (router, _service, data_dir) = test_app("reconnect");

    // First request completes (the job writes its cache entry even if the
    // original client vanished — the JS socket check never dropped work).
    let (status, _) = send(
        &router,
        generate_request(json!({ "directory": "/repo", "source": source_value() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The reloaded page reads, then re-attaches with a generate.
    let (status, read) = send(
        &router,
        Request::get(&format!("/api/walkthrough?{}", source_query()))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["generating"], json!(false));
    assert_eq!(read["walkthrough"]["title"], json!("Change"));

    let (status, reattached) = send(
        &router,
        generate_request(json!({ "directory": "/repo", "source": source_value() })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reattached["fromCache"], json!(true));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：generate 缺少 directory 时返回 400 与固定错误文案，
/// 且在触碰 service 之前即被拒绝，不会把任何 source 置为 generating。
#[tokio::test]
async fn rejects_a_generate_request_without_a_directory_before_touching_the_service() {
    let (router, service, data_dir) = test_app("no-directory");

    let (status, body) = send(
        &router,
        generate_request(json!({ "source": source_value() })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "directory is required" }));
    assert!(!service.is_generating("/repo", "working-tree:all"));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：GET /api/walkthrough 缺少 directory 参数时返回 400，
/// 错误文案为 directory parameter is required。
#[tokio::test]
async fn rejects_a_read_without_a_directory() {
    let (router, _service, data_dir) = test_app("get-no-directory");

    let (status, body) = send(
        &router,
        Request::get("/api/walkthrough?source=%7B%7D")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "directory parameter is required" }));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：GET /api/walkthrough/progress 同样要求 directory 参数，
/// 缺失时返回 400 而非空 progress。
#[tokio::test]
async fn rejects_a_progress_read_without_a_directory() {
    let (router, _service, data_dir) = test_app("progress-no-directory");

    let (status, body) = send(
        &router,
        Request::get("/api/walkthrough/progress")
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "directory parameter is required" }));
    let _ = std::fs::remove_dir_all(&data_dir);
}

// The language belongs to the request, not to a setting, so both the read
// and the generation have to carry it.
/// 契约：read 端点把 language=uk 传入 readiness 计算，
/// 按该语言的 prompt 返回 ready=true。
#[tokio::test]
async fn carries_the_requested_language_into_the_read() {
    let (router, _service, data_dir) = test_app("lang-get");

    let (status, read) = send(
        &router,
        Request::get(&format!(
            "/api/walkthrough?directory=/repo&language=uk&source={}",
            urlencoding_of(&source_value().to_string())
        ))
        .body(Body::empty())
        .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // Readiness is computed against the prompt for that language.
    assert_eq!(read["readiness"]["ready"], json!(true));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：generate 请求体携带 language=ja 时，
/// 响应中的 language 字段原样回显 ja。
#[tokio::test]
async fn carries_the_requested_language_into_generation() {
    let (router, _service, data_dir) = test_app("lang-post");

    let (status, body) = send(
        &router,
        generate_request(
            json!({ "directory": "/repo", "source": source_value(), "language": "ja" }),
        ),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["language"], json!("ja"));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：数组形式的 language 查询参数解析为独立键，language 视为缺失
/// （对应 JS 端的字符串类型守卫），read 仍返回 200 且
/// readiness.model 正常携带 providerID。
#[tokio::test]
async fn ignores_a_language_that_is_not_a_string() {
    let (router, _service, data_dir) = test_app("lang-non-string");

    // `language[]=uk` parses as a distinct key, so `language` is absent — the
    // JS `typeof req.query.language === 'string'` guard.
    let (status, body) = send(
        &router,
        Request::get(&format!(
            "/api/walkthrough?directory=/repo&language%5B%5D=uk&source={}",
            urlencoding_of(&source_value().to_string())
        ))
        .body(Body::empty())
        .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["readiness"]["model"]["providerID"], json!("anthropic"));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：POST /api/walkthrough/cancel 对没有在跑任务的目录返回 200 与
/// cancelled=false，形状与 JS 版针对 releaseJob fixture 的断言一致。
#[tokio::test]
async fn cancels_through_its_own_endpoint() {
    let (router, _service, data_dir) = test_app("cancel");

    let (status, body) = send(
        &router,
        Request::post("/api/walkthrough/cancel")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "directory": "/repo", "source": source_value() }).to_string(),
            ))
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // Nothing is running against the instant fake, so this reports false —
    // the JS suite asserted the same shape against its releaseJob fixture.
    assert_eq!(body, json!({ "cancelled": false }));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：空闲状态下 GET progress 返回 200，stage 为 null。
#[tokio::test]
async fn progress_reports_null_when_nothing_is_running() {
    let (router, _service, data_dir) = test_app("progress-idle");

    let (status, body) = send(
        &router,
        Request::get(&format!("/api/walkthrough/progress?{}", source_query()))
            .body(Body::empty())
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "stage": Value::Null }));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：source 参数非法时沿用 JS 版错误文案——无法解析返回
/// source is required，未知 kind 返回 Unknown source kind 错误。
#[tokio::test]
async fn surfaces_source_errors_with_their_js_messages() {
    let (router, _service, data_dir) = test_app("source-error");

    let (status, body) = send(
        &router,
        Request::get("/api/walkthrough?directory=/repo&source=not-json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "source is required" }));

    let (status, body) = send(
        &router,
        generate_request(json!({ "directory": "/repo", "source": { "kind": "nope" } })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "Unknown source kind \"nope\"" }));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：无可用模型时 generate 返回 404，body 带 code=no-model 与
/// 引导登录的错误文案，而不是透传底层模型的 503。
#[tokio::test]
async fn answers_generation_without_a_model_with_404_and_code() {
    let seam = SmallModelSeam {
        describe: Arc::new(|_request| Box::pin(async { Ok(None) })),
        generate: Arc::new(|_request: GenerateTextRequest| {
            Box::pin(async {
                Err(SmallModelError {
                    status_code: Some(503),
                    message: "unavailable".to_string(),
                    ..Default::default()
                })
            })
        }),
    };
    let data_dir = temp_data_dir("no-model");
    let service = WalkthroughService::new(data_dir.clone(), fake_git(), seam, None);
    let router = routes(service);

    let (status, body) = send(
        &router,
        generate_request(json!({ "directory": "/repo", "source": source_value() })),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], json!("no-model"));
    assert_eq!(
        body["error"],
        json!("No model is available — sign in to a provider first")
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// Issue 2607's route-level assertion: 401 with the structured code and model.
/// 中文补充：模型存在但 provider 未登录（has_login=false）时，generate
/// 返回 401，body 带 code=no-provider-login 并附上模型的 providerID 与
/// modelID，供前端引导用户登录。
#[tokio::test]
async fn answers_an_unauthenticated_provider_with_401_and_the_model() {
    let mut unauthenticated = model();
    unauthenticated.provider_id = "deepseek".to_string();
    unauthenticated.model_id = "deepseek-v4-flash".to_string();
    unauthenticated.has_login = Some(false);
    let seam = SmallModelSeam {
        describe: Arc::new(move |_request| {
            let model = unauthenticated.clone();
            Box::pin(async move { Ok(Some(model)) })
        }),
        generate: Arc::new(|_request: GenerateTextRequest| {
            Box::pin(async {
                Err(SmallModelError::no_provider_login(
                    "No OpenCode login found for provider \"deepseek\"".to_string(),
                ))
            })
        }),
    };
    let data_dir = temp_data_dir("2607");
    let service = WalkthroughService::new(data_dir.clone(), fake_git(), seam, None);
    let router = routes(service);

    let (status, body) = send(
        &router,
        generate_request(json!({ "directory": "/repo", "source": source_value() })),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], json!("no-provider-login"));
    assert_eq!(body["model"]["providerID"], json!("deepseek"));
    assert_eq!(body["model"]["modelID"], json!("deepseek-v4-flash"));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：content-type 非 JSON 的请求体按缺失处理，
/// 走与缺少字段相同的 400 错误路径。
#[tokio::test]
async fn treats_a_non_json_body_as_missing() {
    let (router, _service, data_dir) = test_app("non-json");

    let (status, body) = send(
        &router,
        Request::post("/api/walkthrough/generate")
            .header("content-type", "text/plain")
            .body(Body::from("directory=/repo"))
            .unwrap(),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "directory is required" }));
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：directory 不是字符串（如数字 5）时返回 400，
/// 错误文案为 directory is required。
#[tokio::test]
async fn non_string_directory_is_rejected() {
    let (router, _service, data_dir) = test_app("bad-directory");

    let (status, body) = send(&router, generate_request(json!({ "directory": 5 }))).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "directory is required" }));
    let _ = std::fs::remove_dir_all(&data_dir);
}
