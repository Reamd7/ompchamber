//! Route-level tests for the fs port — exercised through `Router::oneshot`
//! exactly like the JS tests drive the registered express handlers, using
//! temp fixtures and the `x-opencode-directory` header to pin the workspace.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::handlers::{derive_clone_directory_name, reveal_invocation};
use super::{FsState, router};
use crate::config::{EngineConfig, ServerConfig, TunnelOptions};
use crate::context::RouterContext;
use crate::engine::EngineState;
use crate::hub::EventHub;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("fsport-{label}-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp fixture");
    dir
}

fn test_ctx() -> RouterContext {
    let dir = unique_temp_dir("ctx");
    RouterContext {
        config: std::sync::Arc::new(ServerConfig {
            port: 0,
            host: None,
            lan: false,
            ui_password: None,
            api_only: true,
            data_dir: dir.clone(),
            dist_dir: dir.join("dist"),
            tunnel: TunnelOptions::default(),
            engine: EngineConfig::External {
                base_url: "http://127.0.0.1:1".to_string(),
            },
        }),
        engine: EngineState::external("http://127.0.0.1:1".to_string(), None),
        hub: EventHub::new(),
    }
}

fn app() -> Router {
    router(test_ctx())
}

struct Sent {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Sent {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "response body is not JSON ({}): {:?}",
                error,
                String::from_utf8_lossy(&self.body)
            )
        })
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

async fn send(router: Router, request: Request<Body>) -> Sent {
    let response = router
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("oneshot failed: {error}"));
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .expect("read response body");
    Sent {
        status,
        headers,
        body: bytes.to_vec(),
    }
}

fn get(uri: &str, extra_headers: &[(&str, String)]) -> Request<Body> {
    let mut builder = Request::builder().method(Method::GET).uri(uri);
    for (name, value) in extra_headers {
        builder = builder.header(*name, value.as_str());
    }
    builder.body(Body::empty()).unwrap()
}

fn post_json(uri: &str, body: Value, extra_headers: &[(&str, String)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in extra_headers {
        builder = builder.header(*name, value.as_str());
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn post_octet(uri: &str, body: &[u8], extra_headers: &[(&str, String)]) -> Request<Body> {
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/octet-stream");
    for (name, value) in extra_headers {
        builder = builder.header(*name, value.as_str());
    }
    builder.body(Body::from(body.to_vec())).unwrap()
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

// ---------------------------------------------------------------------------
// GET /api/fs/home
// ---------------------------------------------------------------------------

#[tokio::test]
async fn home_returns_the_home_directory() {
    let sent = send(app(), get("/api/fs/home", &[])).await;
    assert_eq!(sent.status, StatusCode::OK);
    let body = sent.json();
    assert_eq!(
        body.get("home").and_then(Value::as_str),
        {
            // os.homedir(): USERPROFILE on Windows, HOME elsewhere.
            if cfg!(windows) {
                std::env::var("USERPROFILE").ok()
            } else {
                std::env::var("HOME").ok()
            }
        }
        .as_deref(),
        "{body}"
    );
}

// ---------------------------------------------------------------------------
// GET /api/fs/list
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_lists_tree_and_keeps_requested_path_space() {
    let fixture = unique_temp_dir("list");
    write_file(&fixture.join("README.md"), "readme");
    std::fs::create_dir_all(fixture.join("src")).unwrap();
    write_file(&fixture.join("src/main.ts"), "main");

    let uri = format!("/api/fs/list?path={}", urlencoding_of(&fixture));
    let sent = send(app(), get(&uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());

    let body = sent.json();
    // The response path stays in the caller's requested path space even
    // though realpath was used to read the directory (issue 2627).
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some(fixture.to_string_lossy().as_ref())
    );

    let entries = body
        .get("entries")
        .and_then(Value::as_array)
        .expect("entries");
    let by_name: Vec<(&str, bool, bool, bool)> = entries
        .iter()
        .map(|entry| {
            (
                entry
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                entry
                    .get("isDirectory")
                    .and_then(Value::as_bool)
                    .unwrap_or_default(),
                entry
                    .get("isFile")
                    .and_then(Value::as_bool)
                    .unwrap_or_default(),
                entry
                    .get("isSymbolicLink")
                    .and_then(Value::as_bool)
                    .unwrap_or_default(),
            )
        })
        .collect();
    let readme = by_name
        .iter()
        .find(|(name, ..)| *name == "README.md")
        .expect("README.md");
    assert_eq!(readme.1, false, "file is not a directory");
    assert_eq!(readme.2, true, "file is a file");
    let src = by_name
        .iter()
        .find(|(name, ..)| *name == "src")
        .expect("src");
    assert_eq!(src.1, true, "src is a directory");

    let readme_entry = entries
        .iter()
        .find(|e| e.get("name").and_then(Value::as_str) == Some("README.md"))
        .unwrap();
    assert_eq!(
        readme_entry.get("path").and_then(Value::as_str),
        Some(fixture.join("README.md").to_string_lossy().as_ref())
    );

    std::fs::remove_dir_all(&fixture).ok();
}

#[cfg(unix)]
#[tokio::test]
async fn list_resolves_symlinked_directories_but_reports_requested_paths() {
    let fixture = unique_temp_dir("listlink");
    let real = fixture.join("real-pkg");
    std::fs::create_dir_all(&real).unwrap();
    write_file(&real.join("a.txt"), "a");
    std::os::unix::fs::symlink(&real, fixture.join("pkg")).unwrap();

    let requested = fixture.join("pkg");
    let uri = format!("/api/fs/list?path={}", urlencoding_of(&requested));
    let sent = send(app(), get(&uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());

    let body = sent.json();
    assert_eq!(
        body.get("path").and_then(Value::as_str),
        Some(requested.to_string_lossy().as_ref())
    );
    let entries = body.get("entries").and_then(Value::as_array).unwrap();
    let a = entries
        .iter()
        .find(|e| e.get("name").and_then(Value::as_str) == Some("a.txt"))
        .expect("a.txt");
    // Entry paths stay in the caller's (symlinked) path space, not realpath.
    assert_eq!(
        a.get("path").and_then(Value::as_str),
        Some(requested.join("a.txt").to_string_lossy().as_ref())
    );
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn list_missing_directory_is_404_with_reason() {
    let fixture = unique_temp_dir("listmissing");
    let uri = format!(
        "/api/fs/list?path={}",
        urlencoding_of(&fixture.join("nope"))
    );
    let sent = send(app(), get(&uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    assert_eq!(
        sent.json(),
        json!({ "error": "Directory not found", "reason": "not-found" })
    );
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn list_missing_plans_directory_answers_empty_200() {
    let fixture = unique_temp_dir("plans");
    let plans = fixture.join(".opencode").join("plans");
    let uri = format!("/api/fs/list?path={}", urlencoding_of(&plans));
    let sent = send(app(), get(&uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(
        sent.json(),
        json!({ "path": plans.to_string_lossy(), "entries": [] })
    );
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn list_file_target_is_rejected_as_not_directory() {
    let fixture = unique_temp_dir("listfile");
    write_file(&fixture.join("file.txt"), "x");
    let uri = format!(
        "/api/fs/list?path={}",
        urlencoding_of(&fixture.join("file.txt"))
    );
    let sent = send(app(), get(&uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Specified path is not a directory", "reason": "not-directory" })
    );
    std::fs::remove_dir_all(&fixture).ok();
}

// ---------------------------------------------------------------------------
// POST /api/fs/write + GET /api/fs/read + GET /api/fs/stat
// ---------------------------------------------------------------------------

#[tokio::test]
async fn write_read_stat_roundtrip() {
    let fixture = std::fs::canonicalize(unique_temp_dir("roundtrip")).unwrap();
    let target = fixture.join("hello.txt");

    let uri = format!("/api/fs/write");
    let sent = send(
        app(),
        post_json(
            &uri,
            json!({ "path": target.to_string_lossy(), "content": "hello fs port" }),
            &[directory_header_of(&fixture)],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(
        sent.json(),
        json!({ "success": true, "path": target.to_string_lossy() })
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello fs port");

    let read_uri = format!("/api/fs/read?path={}", urlencoding_of(&target));
    let sent = send(app(), get(&read_uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::OK);
    assert_eq!(
        sent.headers.get(header::CONTENT_TYPE).unwrap(),
        "text/plain; charset=utf-8"
    );
    assert_eq!(sent.text(), "hello fs port");

    let stat_uri = format!("/api/fs/stat?path={}", urlencoding_of(&target));
    let sent = send(app(), get(&stat_uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::OK);
    let body = sent.json();
    assert_eq!(body.get("isFile"), Some(&json!(true)));
    assert_eq!(body.get("size"), Some(&json!(13)));
    assert!(body.get("mtimeMs").and_then(Value::as_f64).unwrap_or(0.0) > 0.0);

    // Rewriting identical content is a no-op success.
    let sent = send(
        app(),
        post_json(
            "/api/fs/write",
            json!({ "path": target.to_string_lossy(), "content": "hello fs port" }),
            &[directory_header_of(&fixture)],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK);
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn write_creates_missing_parent_directories() {
    let fixture = std::fs::canonicalize(unique_temp_dir("writeparents")).unwrap();
    let target = fixture.join("deep/nested/dir/file.txt");
    let sent = send(
        app(),
        post_json(
            "/api/fs/write",
            json!({ "path": target.to_string_lossy(), "content": "nested" }),
            &[directory_header_of(&fixture)],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "nested");
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn write_requires_path_and_content() {
    let fixture = unique_temp_dir("writebad");
    let header = directory_header_of(&fixture);
    let sent = send(
        app(),
        post_json(
            "/api/fs/write",
            json!({ "content": "x" }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(sent.json(), json!({ "error": "Path is required" }));

    let sent = send(
        app(),
        post_json(
            "/api/fs/write",
            json!({ "path": fixture.join("x").to_string_lossy() }),
            &[header],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(sent.json(), json!({ "error": "Content is required" }));
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn traversal_escaping_the_workspace_is_rejected_with_js_error() {
    let fixture = unique_temp_dir("traversal");
    let outside = unique_temp_dir("outside");
    write_file(&outside.join("secret.txt"), "secret");

    let traversal = format!("{}/sub/../../secret.txt", outside.to_string_lossy());
    let sent = send(
        app(),
        post_json(
            "/api/fs/write",
            json!({ "path": traversal, "content": "evil" }),
            &[directory_header_of(&fixture)],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Path is outside of active workspace" })
    );
    // The outside file was untouched.
    assert_eq!(
        std::fs::read_to_string(outside.join("secret.txt")).unwrap(),
        "secret"
    );

    let read_uri = format!(
        "/api/fs/read?path={}",
        urlencoding_of(Path::new(&traversal))
    );
    let sent = send(app(), get(&read_uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Path is outside of active workspace" })
    );

    std::fs::remove_dir_all(&fixture).ok();
    std::fs::remove_dir_all(&outside).ok();
}

#[tokio::test]
async fn read_missing_file_maps_to_404_and_optional_empty() {
    let fixture = unique_temp_dir("readmissing");
    let missing = fixture.join("gone.txt");
    let header = directory_header_of(&fixture);

    let uri = format!("/api/fs/read?path={}", urlencoding_of(&missing));
    let sent = send(app(), get(&uri, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    assert_eq!(sent.json(), json!({ "error": "File not found" }));

    let uri = format!(
        "/api/fs/read?optional=true&path={}",
        urlencoding_of(&missing)
    );
    let sent = send(app(), get(&uri, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::OK);
    assert_eq!(sent.text(), "");

    let uri = format!("/api/fs/stat?path={}", urlencoding_of(&missing));
    let sent = send(app(), get(&uri, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    assert_eq!(sent.json(), json!({ "error": "File not found" }));

    let uri = format!(
        "/api/fs/stat?optional=true&path={}",
        urlencoding_of(&missing)
    );
    let sent = send(app(), get(&uri, &[header])).await;
    assert_eq!(sent.status, StatusCode::OK);
    assert_eq!(
        sent.json(),
        json!({ "path": missing.to_string_lossy(), "exists": false })
    );

    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn read_directory_target_is_rejected() {
    let fixture = unique_temp_dir("readdir");
    std::fs::create_dir_all(fixture.join("sub")).unwrap();
    let uri = format!("/api/fs/read?path={}", urlencoding_of(&fixture.join("sub")));
    let sent = send(app(), get(&uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Specified path is not a file" })
    );
    std::fs::remove_dir_all(&fixture).ok();
}

// ---------------------------------------------------------------------------
// POST /api/fs/mkdir + /api/fs/delete + /api/fs/rename
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mkdir_creates_directories_and_rejects_outside_grants() {
    let fixture = unique_temp_dir("mkdir");
    let header = directory_header_of(&fixture);

    let target = fixture.join("a/b/c");
    let sent = send(
        app(),
        post_json(
            "/api/fs/mkdir",
            json!({ "path": target.to_string_lossy() }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert!(target.is_dir());

    let sent = send(
        app(),
        post_json(
            "/api/fs/mkdir",
            json!({ "path": "/tmp/staging", "allowOutsideWorkspace": true }),
            &[header],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::FORBIDDEN);
    assert_eq!(
        sent.json(),
        json!({ "error": "Outside workspace directory creation requires a grant" })
    );
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn delete_missing_path_is_force_success_and_outside_is_400() {
    let fixture = unique_temp_dir("delete");
    let header = directory_header_of(&fixture);

    // rm(recursive: true, force: true) tolerates missing targets.
    let sent = send(
        app(),
        post_json(
            "/api/fs/delete",
            json!({ "path": fixture.join("missing").to_string_lossy() }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(
        sent.json(),
        json!({ "success": true, "path": fixture.join("missing").to_string_lossy() })
    );

    write_file(&fixture.join("doomed.txt"), "x");
    std::fs::create_dir_all(fixture.join("doomed-dir/nested")).unwrap();
    write_file(&fixture.join("doomed-dir/nested/inner.txt"), "x");

    let sent = send(
        app(),
        post_json(
            "/api/fs/delete",
            json!({ "path": fixture.join("doomed.txt").to_string_lossy() }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK);
    assert!(!fixture.join("doomed.txt").exists());

    let sent = send(
        app(),
        post_json(
            "/api/fs/delete",
            json!({ "path": fixture.join("doomed-dir").to_string_lossy() }),
            &[header],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK);
    assert!(!fixture.join("doomed-dir").exists());

    let sent = send(
        app(),
        post_json(
            "/api/fs/delete",
            json!({ "path": "/etc" }),
            &[directory_header_of(&fixture)],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Path is outside of active workspace" })
    );
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn rename_moves_files_and_requires_shared_root() {
    let fixture = unique_temp_dir("rename");
    let header = directory_header_of(&fixture);
    write_file(&fixture.join("old.txt"), "data");

    let sent = send(
        app(),
        post_json(
            "/api/fs/rename",
            json!({
                "oldPath": fixture.join("old.txt").to_string_lossy(),
                "newPath": fixture.join("new.txt").to_string_lossy(),
            }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(
        sent.json(),
        json!({ "success": true, "path": fixture.join("new.txt").to_string_lossy() })
    );
    assert!(!fixture.join("old.txt").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.join("new.txt")).unwrap(),
        "data"
    );

    // Cross-root rename: the config root is an always-admitted second base,
    // so a destination there resolves against a different base than the
    // fixture and must be rejected before touching the filesystem.
    let config_child = super::paths::user_config_root().join("definitely-not-a-real-destination");
    let sent = send(
        app(),
        post_json(
            "/api/fs/rename",
            json!({
                "oldPath": fixture.join("new.txt").to_string_lossy(),
                "newPath": config_child.to_string_lossy(),
            }),
            &[header],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Source and destination must share the same workspace root" })
    );
    assert!(fixture.join("new.txt").exists());
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn rename_missing_source_is_404() {
    let fixture = unique_temp_dir("renamemissing");
    let sent = send(
        app(),
        post_json(
            "/api/fs/rename",
            json!({
                "oldPath": fixture.join("nope.txt").to_string_lossy(),
                "newPath": fixture.join("also.txt").to_string_lossy(),
            }),
            &[directory_header_of(&fixture)],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    assert_eq!(sent.json(), json!({ "error": "Source path not found" }));
    std::fs::remove_dir_all(&fixture).ok();
}

// ---------------------------------------------------------------------------
// POST /api/fs/upload
// ---------------------------------------------------------------------------

/// Serializes the upload tests: one of them mutates
/// OMPCHAMBER_FS_UPLOAD_MAX_BYTES (process-global), so no other upload test
/// may stream concurrently.
static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn upload_streams_and_commits_atomically() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = unique_temp_dir("upload");
    let target = fixture.join("blob.bin");
    let header = directory_header_of(&fixture);

    let uri = format!("/api/fs/upload?path={}", urlencoding_of(&target));
    let payload: Vec<u8> = vec![0, 1, 2, 255, 254];
    let sent = send(app(), post_octet(&uri, &payload, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(
        sent.json(),
        json!({ "success": true, "path": target.to_string_lossy() })
    );
    assert_eq!(std::fs::read(&target).unwrap(), payload);

    // No temp leftovers.
    let leftovers: Vec<_> = std::fs::read_dir(&fixture)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".upload-"))
        .collect();
    assert!(leftovers.is_empty(), "temp files must be cleaned up");

    // Existing target without overwrite → 409 already-exists.
    let sent = send(app(), post_octet(&uri, &payload, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::CONFLICT);
    assert_eq!(
        sent.json(),
        json!({ "error": "File already exists", "reason": "already-exists" })
    );

    // Overwrite commits the new bytes.
    let uri = format!(
        "/api/fs/upload?overwrite=true&path={}",
        urlencoding_of(&target)
    );
    let replacement = b"replacement-bytes".to_vec();
    let sent = send(app(), post_octet(&uri, &replacement, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(std::fs::read(&target).unwrap(), replacement);

    // Wrong content type → 415.
    let uri = format!(
        "/api/fs/upload?path={}",
        urlencoding_of(&fixture.join("x.bin"))
    );
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(&uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}".to_string()))
        .unwrap();
    request
        .headers_mut()
        .insert("x-opencode-directory", header.1.parse().unwrap());
    let sent = send(app(), request).await;
    assert_eq!(sent.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        sent.json(),
        json!({ "error": "Content-Type must be application/octet-stream" })
    );

    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn upload_directory_target_is_rejected() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = unique_temp_dir("uploaddir");
    std::fs::create_dir_all(fixture.join("adir")).unwrap();
    let uri = format!(
        "/api/fs/upload?path={}",
        urlencoding_of(&fixture.join("adir"))
    );
    let sent = send(
        app(),
        post_octet(&uri, b"data", &[directory_header_of(&fixture)]),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Specified path is a directory" })
    );
    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn upload_rejects_declared_and_streamed_oversize_bodies() {
    let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        std::env::set_var("OMPCHAMBER_FS_UPLOAD_MAX_BYTES", "5");
    }

    let fixture = unique_temp_dir("uploadlimit");
    let header = directory_header_of(&fixture);

    // Declared Content-Length over the cap → 413 before reading the body.
    let target = fixture.join("big-declared.bin");
    let uri = format!("/api/fs/upload?path={}", urlencoding_of(&target));
    let request = Request::builder()
        .method(Method::POST)
        .uri(&uri)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, "6")
        .body(Body::from("123456".to_string()))
        .unwrap();
    let request = with_directory(request, &header.1);
    let sent = send(app(), request).await;
    assert_eq!(sent.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        sent.json(),
        json!({ "error": "File exceeds maximum size of 5 bytes" })
    );
    assert!(!target.exists());

    // Streamed body exceeding the cap (no Content-Length) → 413 + cleanup.
    let target = fixture.join("big-streamed.bin");
    let uri = format!("/api/fs/upload?path={}", urlencoding_of(&target));
    let request = with_directory(
        Request::builder()
            .method(Method::POST)
            .uri(&uri)
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from("123456".to_string()))
            .unwrap(),
        &header.1,
    );
    let sent = send(app(), request).await;
    assert_eq!(sent.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        sent.json(),
        json!({ "error": "File exceeds maximum size of 5 bytes" })
    );
    assert!(!target.exists());
    let leftovers: Vec<_> = std::fs::read_dir(&fixture)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".upload-"))
        .collect();
    assert!(leftovers.is_empty(), "oversize temp file must be removed");

    unsafe {
        std::env::remove_var("OMPCHAMBER_FS_UPLOAD_MAX_BYTES");
    }
    std::fs::remove_dir_all(&fixture).ok();
}

fn with_directory(request: Request<Body>, directory: &str) -> Request<Body> {
    let mut request = request;
    request
        .headers_mut()
        .insert("x-opencode-directory", directory.parse().unwrap());
    request
}

fn directory_header_of(fixture: &Path) -> (&'static str, String) {
    (
        "x-opencode-directory",
        fixture.to_string_lossy().into_owned(),
    )
}

fn urlencoding_of(path: &Path) -> String {
    super::paths::encode_uri_component(&path.to_string_lossy())
}

// ---------------------------------------------------------------------------
// GET /api/fs/raw + /api/fs/serve
// ---------------------------------------------------------------------------

#[tokio::test]
async fn raw_sets_mime_download_and_cache_headers() {
    let fixture = unique_temp_dir("raw");
    let header = directory_header_of(&fixture);

    std::fs::write(&fixture.join("image.png"), b"\x89PNG-not-really").unwrap();
    let uri = format!(
        "/api/fs/raw?download=true&path={}",
        urlencoding_of(&fixture.join("image.png"))
    );
    let sent = send(app(), get(&uri, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::OK);
    assert_eq!(sent.headers.get(header::CONTENT_TYPE).unwrap(), "image/png");
    assert_eq!(sent.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    let disposition = sent
        .headers
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        disposition.contains("filename=\"image.png\""),
        "{disposition}"
    );
    assert!(
        disposition.contains("filename*=UTF-8''image.png"),
        "{disposition}"
    );
    assert!(
        sent.headers.get(header::REFERRER_POLICY).is_none(),
        "no grant → no no-referrer"
    );

    // Non-ASCII file names: RFC 5987 filename*= with ASCII fallback.
    write_file(&fixture.join("\u{6587}\u{4ef6}.txt"), "content");
    let uri = format!(
        "/api/fs/raw?download=true&path={}",
        urlencoding_of(&fixture.join("\u{6587}\u{4ef6}.txt"))
    );
    let sent = send(app(), get(&uri, &[header])).await;
    assert_eq!(sent.status, StatusCode::OK);
    let disposition = sent
        .headers
        .get(header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(disposition.contains("filename=\".txt\""), "{disposition}");
    assert!(
        disposition.contains(&format!(
            "filename*=UTF-8''{}",
            super::paths::encode_uri_component("\u{6587}\u{4ef6}.txt")
        )),
        "{disposition}"
    );

    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn serve_sets_nosniff_and_rejects_outside_flag_and_oversize() {
    let fixture = unique_temp_dir("serve");
    let header = directory_header_of(&fixture);

    write_file(&fixture.join("app.js"), "console.log(1)");
    let uri = format!(
        "/api/fs/serve/{}",
        encode_segment(&fixture.join("app.js").to_string_lossy())
    );
    let sent = send(app(), get(&uri, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(
        sent.headers.get(header::CONTENT_TYPE).unwrap(),
        "application/javascript"
    );
    assert_eq!(
        sent.headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    assert_eq!(sent.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");

    let uri = format!(
        "/api/fs/serve/{}?allowOutsideWorkspace=true",
        encode_segment(&fixture.join("app.js").to_string_lossy())
    );
    let sent = send(app(), get(&uri, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::FORBIDDEN);
    assert_eq!(
        sent.json(),
        json!({ "error": "allowOutsideWorkspace is not permitted for this endpoint" })
    );

    // MAX_SERVE_BYTES: a sparse file past the cap is refused with 413.
    let big = fixture.join("big.bin");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(100 * 1024 * 1024 + 1).unwrap();
    drop(file);
    let uri = format!("/api/fs/serve/{}", encode_segment(&big.to_string_lossy()));
    let sent = send(app(), get(&uri, &[header])).await;
    assert_eq!(sent.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(sent.json(), json!({ "error": "File too large to serve" }));

    std::fs::remove_dir_all(&fixture).ok();
}

fn encode_segment(path: &str) -> String {
    // Path params arrive percent-encoded; encode '/' so the whole path rides
    // the {*path} wildcard.
    super::paths::encode_uri_component(path).replace("%2F", "%2F")
}

// ---------------------------------------------------------------------------
// Outside-file grants (allowOutsideWorkspace=true)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn outside_file_grants_gate_read_raw_stat() {
    let fixture = unique_temp_dir("grants");
    let outside = unique_temp_dir("grantsoutside");
    write_file(&outside.join("plan.txt"), "secret");
    let header = directory_header_of(&fixture);

    // Without a grant → 400 with the JS message.
    let uri = format!(
        "/api/fs/read?allowOutsideWorkspace=true&path={}",
        urlencoding_of(&outside.join("plan.txt"))
    );
    let sent = send(app(), get(&uri, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Outside workspace file access requires a grant" })
    );

    // Mint a grant against the module state (feature routes do this after an
    // explicit user pick).
    let state = FsState::new();
    let granted = state
        .grants()
        .mint(
            &outside.join("plan.txt").to_string_lossy(),
            &["stat", "read", "raw"],
        )
        .await
        .expect("mint grant");
    let token = granted
        .get("outsideFileGrant")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let uri = format!(
        "/api/fs/read?allowOutsideWorkspace=true&outsideFileGrant={token}&path={}",
        urlencoding_of(&outside.join("plan.txt"))
    );
    let sent = send(app(), get(&uri, &[header.clone()])).await;
    // A different router instance owns a different grant store — the grant
    // does not transfer, which mirrors the JS process-wide map semantics.
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Outside workspace file grant is invalid or expired" })
    );

    std::fs::remove_dir_all(&fixture).ok();
    std::fs::remove_dir_all(&outside).ok();
}

#[tokio::test]
async fn grant_mismatch_and_scope_errors_use_js_messages() {
    let outside = unique_temp_dir("grantmismatch");
    write_file(&outside.join("a.txt"), "a");
    write_file(&outside.join("b.txt"), "b");
    let state = FsState::new();

    let granted = state
        .grants()
        .mint(&outside.join("a.txt").to_string_lossy(), &["raw"])
        .await
        .expect("mint grant");
    let token = granted
        .get("outsideFileGrant")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Wrong canonical path.
    let error = state
        .grants()
        .resolve(Some(&token), &outside.join("b.txt"), "raw")
        .await
        .expect_err("mismatch must fail");
    assert!(
        matches!(error, super::workspace::GrantError::Denied(ref message) if message == "Outside workspace file grant does not match requested path")
    );

    // Wrong scope.
    let error = state
        .grants()
        .resolve(Some(&token), &outside.join("a.txt"), "read")
        .await
        .expect_err("scope must fail");
    assert!(
        matches!(error, super::workspace::GrantError::Denied(ref message) if message == "Outside workspace file grant does not allow this operation")
    );

    // Missing token.
    let error = state
        .grants()
        .resolve(None, &outside.join("a.txt"), "read")
        .await
        .expect_err("no token");
    assert!(
        matches!(error, super::workspace::GrantError::Denied(ref message) if message == "Outside workspace file access requires a grant")
    );

    // Directory targets cannot be granted.
    let error = state
        .grants()
        .mint(&outside.to_string_lossy(), &[])
        .await
        .expect_err("dirs rejected");
    assert_eq!(error, "Outside file grants require a file path");

    std::fs::remove_dir_all(&outside).ok();
}

// ---------------------------------------------------------------------------
// POST /api/fs/exec + GET /api/fs/exec/{jobId}
// ---------------------------------------------------------------------------

#[tokio::test]
async fn exec_runs_commands_and_exposes_the_job() {
    let fixture = unique_temp_dir("exec");
    let header = directory_header_of(&fixture);
    // One router instance == one server process with one exec job store.
    let shared = app();

    let sent = send(
        shared.clone(),
        post_json(
            "/api/fs/exec",
            json!({ "commands": ["echo fs-port", "exit 3", 42, "   "], "cwd": fixture.to_string_lossy() }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    let body = sent.json();
    assert_eq!(body.get("status"), Some(&json!("done")));
    assert_eq!(
        body.get("success"),
        Some(&json!(false)),
        "one failing command fails the job"
    );
    let results = body.get("results").and_then(Value::as_array).unwrap();

    let echo = &results[0];
    assert_eq!(echo.get("success"), Some(&json!(true)));
    assert_eq!(echo.get("stdout"), Some(&json!("fs-port")));
    assert_eq!(echo.get("exitCode"), Some(&json!(0)));

    let failing = &results[1];
    assert_eq!(failing.get("success"), Some(&json!(false)));
    assert_eq!(failing.get("exitCode"), Some(&json!(3)));

    // Invalid command entries carry the minimal JS shape.
    assert_eq!(
        results[2],
        json!({ "command": 42, "success": false, "error": "Invalid command" })
    );
    assert_eq!(
        results[3],
        json!({ "command": "   ", "success": false, "error": "Invalid command" })
    );

    // The job is retrievable and reports done through the same router.
    let job_id = body
        .get("jobId")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    let uri = format!("/api/fs/exec/{job_id}");
    let sent = send(shared.clone(), get(&uri, &[header.clone()])).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    let fetched = sent.json();
    assert_eq!(fetched.get("jobId"), Some(&json!(job_id.clone())));
    assert_eq!(fetched.get("status"), Some(&json!("done")));

    // Unknown job → 404.
    let sent = send(
        shared,
        get(
            "/api/fs/exec/00000000-0000-4000-8000-000000000000",
            &[header],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::NOT_FOUND);
    assert_eq!(sent.json(), json!({ "error": "Job not found" }));

    std::fs::remove_dir_all(&fixture).ok();
}

#[tokio::test]
async fn exec_validates_input_and_workspace() {
    let fixture = unique_temp_dir("execbad");
    let header = directory_header_of(&fixture);

    let sent = send(
        app(),
        post_json(
            "/api/fs/exec",
            json!({ "cwd": fixture.to_string_lossy() }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Commands array is required" })
    );

    let sent = send(
        app(),
        post_json(
            "/api/fs/exec",
            json!({ "commands": ["id"], "cwd": "" }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Working directory (cwd) is required" })
    );

    let sent = send(
        app(),
        post_json(
            "/api/fs/exec",
            json!({ "commands": ["id"], "cwd": fixture.to_string_lossy(), "background": true }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Background command execution is not allowed" })
    );

    // Outside workspace → 403.
    let sent = send(
        app(),
        post_json(
            "/api/fs/exec",
            json!({ "commands": ["id"], "cwd": "/etc" }),
            &[header.clone()],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::FORBIDDEN);
    assert_eq!(
        sent.json(),
        json!({ "error": "Path is outside of active workspace" })
    );

    // File cwd → 400.
    write_file(&fixture.join("file.txt"), "x");
    let sent = send(
        app(),
        post_json(
            "/api/fs/exec",
            json!({ "commands": ["id"], "cwd": fixture.join("file.txt").to_string_lossy() }),
            &[header],
        ),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        sent.json(),
        json!({ "error": "Specified cwd is not a directory" })
    );

    std::fs::remove_dir_all(&fixture).ok();
}

// ---------------------------------------------------------------------------
// GET /api/fs/git-dirs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn git_dirs_discovers_nested_repositories() {
    let fixture = unique_temp_dir("gitdirs");
    std::fs::create_dir_all(fixture.join("proj-a/.git")).unwrap();
    std::fs::create_dir_all(fixture.join("node_modules/dep/.git")).unwrap();
    std::fs::create_dir_all(fixture.join("proj-b")).unwrap();
    write_file(&fixture.join("proj-b/.git"), "gitdir: ../proj-a/.git\n"); // worktree file

    let uri = format!("/api/fs/git-dirs?path={}", urlencoding_of(&fixture));
    let sent = send(app(), get(&uri, &[directory_header_of(&fixture)])).await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    let body = sent.json();
    let repositories = body.get("repositories").and_then(Value::as_array).unwrap();
    let names: Vec<&str> = repositories
        .iter()
        .filter_map(|r| r.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(names, vec!["proj-a", "proj-b"]);

    let sent = send(
        app(),
        get("/api/fs/git-dirs", &[directory_header_of(&fixture)]),
    )
    .await;
    assert_eq!(sent.status, StatusCode::BAD_REQUEST);
    assert_eq!(sent.json(), json!({ "error": "Path is required" }));

    std::fs::remove_dir_all(&fixture).ok();
}

// ---------------------------------------------------------------------------
// Pure helper units
// ---------------------------------------------------------------------------

#[test]
fn reveal_invocation_matches_platform_shapes() {
    let file = Path::new("/repo/file.txt");
    let dir = Path::new("/repo");

    let (program, args, wait) = reveal_invocation("macos", file, false);
    assert_eq!(
        (program.as_str(), args, wait),
        (
            "open",
            vec!["-R".to_string(), "/repo/file.txt".to_string()],
            false
        )
    );

    let (program, args, wait) = reveal_invocation("macos", dir, true);
    assert_eq!(
        (program.as_str(), args, wait),
        ("open", vec!["/repo".to_string()], false)
    );

    let (program, args, wait) = reveal_invocation("linux", file, false);
    assert_eq!(
        (program.as_str(), args, wait),
        ("xdg-open", vec!["/repo".to_string()], false)
    );

    let (program, args, wait) = reveal_invocation("windows", file, false);
    assert_eq!(program, "powershell.exe");
    assert_eq!(args[..3], ["-NoProfile", "-NonInteractive", "-Command"][..]);
    assert_eq!(
        args[3],
        "Start-Process -FilePath explorer.exe -ArgumentList '/select,/repo/file.txt'"
    );
    assert!(wait);
}

#[test]
fn derive_clone_directory_name_matches_js() {
    assert_eq!(
        derive_clone_directory_name("https://github.com/org/repo.git"),
        "repo"
    );
    assert_eq!(
        derive_clone_directory_name("https://github.com/org/repo"),
        "repo"
    );
    assert_eq!(
        derive_clone_directory_name("git@github.com:org/repo.git?ts=1"),
        "repo"
    );
    assert_eq!(
        derive_clone_directory_name("https://host/org/repo.github#frag"),
        "repo.github"
    );
    assert_eq!(derive_clone_directory_name("  "), "");
    assert_eq!(derive_clone_directory_name("https://host/"), "");
}

#[test]
fn escape_clone_ssh_key_path_quotes_and_rejects_dangerous_chars() {
    let quoted = super::handlers::escape_clone_ssh_key_path("/home/u/id_ed25519");
    assert_eq!(quoted, Ok("'/home/u/id_ed25519'".to_string()));

    let error = super::handlers::escape_clone_ssh_key_path("/home/u/key'name");
    assert_eq!(
        error,
        Err("SSH key path contains invalid characters: /home/u/key'name".to_string())
    );
}
