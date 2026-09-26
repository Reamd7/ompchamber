//! Tests ported from `server/lib/walkthrough/routes.test.js` (over oneshot
//! requests instead of a live socket — axum handlers complete regardless of
//! client disconnects, which is the behavior those tests existed to protect).

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

fn source_value() -> Value {
    json!({ "kind": "working-tree", "scope": "all" })
}

const PATCH: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,1 +1,2 @@\n+const added = true;\n";

const RESPONSE: &str = r#"{"title":"Change","focus":"why","chapters":[{"title":"Data","icon":"doc","blurb":"","stops":[{"title":"Adds a flag","hunks":["h1"],"importance":"normal","prose":"It adds a flag."}]}]}"#;

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

fn source_query() -> String {
    format!(
        "directory=/repo&source={}",
        urlencoding_of(&source_value().to_string())
    )
}

/// The JS client sends `encodeURIComponent(JSON.stringify(source))`.
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

fn generate_request(body: Value) -> Request<Body> {
    Request::post("/api/walkthrough/generate")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

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

#[tokio::test]
async fn non_string_directory_is_rejected() {
    let (router, _service, data_dir) = test_app("bad-directory");

    let (status, body) = send(&router, generate_request(json!({ "directory": 5 }))).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "directory is required" }));
    let _ = std::fs::remove_dir_all(&data_dir);
}
