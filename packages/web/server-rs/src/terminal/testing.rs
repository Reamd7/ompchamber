//! Terminal module tests: unit suites live next to their implementations
//! (`protocol.rs`, `history.rs`, `theme.rs`, `shell_integration.rs`,
//! `shells.rs`, `grid.rs`); this module covers the runtime — HTTP routes via
//! `Router::oneshot` with the fake PTY provider, and the WebSocket transport
//! over a real loopback listener through a minimal RFC6455 client (the crate
//! has no client WS dependency, and exercising the real upgrade path is the
//! point).
//!
//! 中文说明：终端模块的运行时集成测试。单元测试位于各自实现文件旁
//!（protocol/history/theme/shell_integration/shells/grid），本文件覆盖运行
//! 时行为：用 Router::oneshot 挂 fake PTY provider 驱动 HTTP 路由，并通过
//! 手写的最小 RFC6455 客户端连真实回环 listener 验证 WebSocket 传输
//!（crate 未引入客户端 WS 依赖，走真实升级路径正是测试目的）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

use crate::terminal::pty::PtyProvider;
use crate::terminal::pty::fake::{FakePtyProcess, FakePtyProvider};
use crate::terminal::runtime::{TerminalOptions, TerminalState};
use crate::terminal::shells::{Platform, ShellDeps};
use crate::terminal::{protocol, test_router};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// 测试用 shell 依赖：SHELL=/bin/sh，仅 /bin/sh 可执行，无 /etc/shells。
fn test_shell_deps() -> ShellDeps {
    ShellDeps {
        platform: Platform::Posix,
        env: Box::new(|key| match key {
            "SHELL" => Some("/bin/sh".to_string()),
            _ => None,
        }),
        build_augmented_path: Box::new(|| "/usr/bin:/bin".to_string()),
        search_path_for: Box::new(|name: &str, _| (name == "sh").then(|| "/bin/sh".to_string())),
        is_executable: Box::new(|candidate: &str| candidate == "/bin/sh"),
        read_etc_shells: Box::new(|| None),
    }
}

/// 构造挂 fake PTY 的 TerminalState；终止宽限期压到 10ms 便于观察信号序列。
fn test_state() -> (Arc<TerminalState>, Arc<FakePtyProvider>) {
    let provider = Arc::new(FakePtyProvider::new());
    let state = TerminalState::new(
        TerminalOptions {
            heartbeat_interval_ms: 30_000,
            termination_grace_ms: 10,
            shell_integration: false,
        },
        Arc::clone(&provider) as Arc<dyn PtyProvider>,
        test_shell_deps(),
    );
    (state, provider)
}

/// 建一个进程内唯一的临时目录（进程 id + UUID 防并行冲突），作 create 的 cwd。
fn unique_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oc-terminal-test-{label}-{}-{}",
        std::process::id(),
        crate::terminal::runtime::random_uuid()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// 用 oneshot 发一次 JSON 请求；返回 (状态码, 解析后的 JSON 体)，空体按 Null 处理。
async fn request_json(
    router: &mut Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = router
        .clone()
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Poll until `probe` holds or the deadline passes (the session pump is async).
/// 中文补充：最多 500 次 × 10ms；超时 panic，避免异步副作用导致的假失败。
async fn eventually<F>(probe: F)
where
    F: Fn() -> bool,
{
    for _ in 0..500 {
        if probe() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met within 5s");
}

/// 取第 index 个 spawn 出的假进程句柄（下标即 spawn 顺序）。
fn fake_at(provider: &FakePtyProvider, index: usize) -> Arc<FakePtyProcess> {
    Arc::clone(&provider.spawned.lock().unwrap_or_else(|e| e.into_inner())[index])
}

// ---------------------------------------------------------------------------
// HTTP route behavior
// ---------------------------------------------------------------------------

/// 创建带客户端 id 的会话：cwd/尺寸/主题色正确进入 spawn 环境，OSC 主题
/// 握手与外观变更按预期写回 PTY，合法 resize 生效、超界 resize 返回 400。
#[tokio::test]
async fn creates_client_identified_sessions_and_forwards_bounded_resizes() {
    let (state, provider) = test_state();
    let cwd = unique_dir("create");
    let mut router = test_router(Arc::clone(&state));

    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({
            "sessionId": "term-1", "cwd": cwd.to_string_lossy(), "cols": 120, "rows": 40,
            "themeMode": "light", "terminalBackground": "#faf8f0", "terminalForeground": "#1b1b1b",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "sessionId": "term-1", "cols": 120, "rows": 40, "status": "running" })
    );

    let process = fake_at(&provider, 0);
    let spawned = process
        .spawned_with
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    assert_eq!(spawned.cwd, cwd.to_string_lossy());
    assert_eq!(spawned.cols, 120);
    assert_eq!(spawned.rows, 40);
    assert_eq!(
        spawned.env.get("COLORFGBG").map(String::as_str),
        Some("0;15")
    );
    assert_eq!(
        spawned.env.get("NODE_CHANNEL_FD").map(String::as_str),
        Some("")
    );
    assert_eq!(
        spawned.env.get("TERM").map(String::as_str),
        Some("xterm-256color")
    );
    assert_eq!(
        spawned.env.get("COLORTERM").map(String::as_str),
        Some("truecolor")
    );

    // Theme handshake over emitted queries, answered by the PTY responder.
    process.emit_data("\u{1b}[?2031h\u{1b}]10;?\u{7}\u{1b}]11;?\u{7}\u{1b}[0c");
    let expected_writes = vec![
        "\u{1b}]10;rgb:1b1b/1b1b/1b1b\u{1b}\\".to_string(),
        "\u{1b}]11;rgb:fafa/f8f8/f0f0\u{1b}\\".to_string(),
        "\u{1b}[?1;2c".to_string(),
    ];
    eventually(|| process.writes() == expected_writes).await;

    // Appearance change reports the mode to a subscribed TUI.
    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/term-1/appearance",
        Some(json!({ "themeMode": "dark" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "success": true }));
    eventually(|| process.writes().last().map(String::as_str) == Some("\u{1b}[?997;1n")).await;

    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/term-1/resize",
        Some(json!({ "cols": 200, "rows": 60 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "success": true, "cols": 200, "rows": 60 }));
    assert_eq!(process.resizes(), vec![(200, 60)]);

    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/term-1/resize",
        Some(json!({ "cols": 1001, "rows": 60 })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
/// cwd 指向普通文件时创建被拒，错误文案为 Invalid working directory。
#[tokio::test]
async fn rejects_regular_files_as_working_directories() {
    let (state, _provider) = test_state();
    let mut router = test_router(Arc::clone(&state));
    let file = unique_dir("file-cwd").join("not-a-directory.txt");
    std::fs::write(&file, b"x").expect("write");

    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "cwd": file.to_string_lossy() })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "Invalid working directory" }));
}

/// 各类非法参数（尺寸越界/null/命令串/类型不符/未知 shell/超长 id）逐条
/// 命中与 JS 一致的错误文案，且全程不 spawn 任何进程；sh 不支持登录模式
/// 的错误会点名 shell。
#[tokio::test]
async fn rejects_invalid_dimensions_shells_and_login_modes_with_exact_messages() {
    let (state, provider) = test_state();
    let cwd = unique_dir("invalid").to_string_lossy().into_owned();
    let mut router = test_router(Arc::clone(&state));

    let cases = [
        (
            json!({ "cwd": cwd, "cols": 1001 }),
            "Invalid terminal dimensions",
        ),
        (
            json!({ "cwd": cwd, "cols": null }),
            "Invalid terminal dimensions",
        ),
        (
            json!({ "cwd": cwd, "rows": 0 }),
            "Invalid terminal dimensions",
        ),
        (
            json!({ "cwd": cwd, "loginShell": "true" }),
            "Invalid terminal login mode",
        ),
        (
            json!({ "cwd": cwd, "loginShell": null }),
            "Invalid terminal login mode",
        ),
        (
            json!({ "cwd": cwd, "shell": "zsh -c whoami" }),
            "Invalid terminal shell",
        ),
        (json!({ "cwd": cwd, "shell": 42 }), "Invalid terminal shell"),
        (
            json!({ "cwd": cwd, "shell": "fish" }),
            "Terminal shell \"fish\" is not available",
        ),
        (
            json!({ "cwd": cwd, "sessionId": "x".repeat(129) }),
            "Invalid terminal session id",
        ),
    ];
    for (body, message) in cases {
        let (status, response) = request_json(
            &mut router,
            Method::POST,
            "/api/terminal/create",
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert_eq!(response, json!({ "error": message }));
    }
    assert!(
        provider
            .spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    );

    // `sh` has no supported login mode — the error names the shell.
    let (status, response) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "cwd": cwd, "shell": "sh", "loginShell": true })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        response,
        json!({ "error": "Terminal shell \"sh\" does not support login mode" })
    );
}

/// sessions 列表暴露 id/cwd/状态/创建时间并支持按 cwd 过滤；touch 只刷新
/// 存在的字符串 id，其余静默跳过。
#[tokio::test]
async fn lists_sessions_scoped_to_a_working_directory_and_touches() {
    let (state, provider) = test_state();
    let repo = unique_dir("repo").to_string_lossy().into_owned();
    let other = unique_dir("other").to_string_lossy().into_owned();
    let mut router = test_router(Arc::clone(&state));

    for (id, cwd) in [("term-a", &repo), ("term-b", &other)] {
        let (status, _) = request_json(
            &mut router,
            Method::POST,
            "/api/terminal/create",
            Some(json!({ "sessionId": id, "cwd": cwd })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    assert_eq!(
        provider
            .spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        2
    );

    let (status, body) =
        request_json(&mut router, Method::GET, "/api/terminal/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
    let mut ids: Vec<&str> = body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["sessionId"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["term-a", "term-b"]);

    let (status, body) = request_json(
        &mut router,
        Method::GET,
        &format!("/api/terminal/sessions?cwd={}", urlencoding_lite(&repo)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(body["sessions"][0]["sessionId"], "term-a");
    assert_eq!(body["sessions"][0]["cwd"], repo);
    assert_eq!(body["sessions"][0]["status"], "running");
    assert!(body["sessions"][0]["createdAt"].is_u64());

    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/touch",
        Some(json!({ "sessionIds": ["term-a", "missing", 42] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "touched": 1 }));

    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/touch",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "touched": 0 }));
}

/// 重启原子替换：旧进程收到 TERM 后 KILL、新进程以新 cwd 启动；替代 shell
/// 不可用时重启失败且原进程原样保留。
#[tokio::test]
async fn restarts_atomically_and_preserves_the_process_on_bad_restarts() {
    let (state, provider) = test_state();
    let repo = unique_dir("restart").to_string_lossy().into_owned();
    let other = unique_dir("restart-other").to_string_lossy().into_owned();
    let mut router = test_router(Arc::clone(&state));

    let (status, _) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-1", "cwd": repo })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/term-1/restart",
        Some(json!({ "cwd": other, "cols": 90, "rows": 30 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "sessionId": "term-1", "cols": 90, "rows": 30, "status": "running" })
    );
    assert_eq!(
        provider
            .spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        2
    );
    eventually(|| fake_at(&provider, 0).kills() == vec!["SIGTERM", "SIGKILL"]).await;
    assert_eq!(
        fake_at(&provider, 1)
            .spawned_with
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cwd,
        other
    );

    // An unavailable replacement shell fails the restart without killing.
    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/term-1/restart",
        Some(json!({ "shell": "fish" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({ "error": "Terminal shell \"fish\" is not available" })
    );
    assert!(fake_at(&provider, 1).kills().is_empty());
    assert_eq!(
        provider
            .spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        2
    );
}

/// 并发重启被串行化：两次请求都成功，两个旧进程各收到终止信号，最终只剩
/// 一个存活的新进程（无孤儿替代进程）。
#[tokio::test]
async fn serializes_concurrent_restarts_without_orphaning_replacements() {
    let (state, provider) = test_state();
    let repo = unique_dir("serial").to_string_lossy().into_owned();
    let first = unique_dir("serial-first").to_string_lossy().into_owned();
    let second = unique_dir("serial-second").to_string_lossy().into_owned();
    let mut router = test_router(Arc::clone(&state));

    let (status, _) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-1", "cwd": repo })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let mut router_a = router.clone();
    let mut router_b = router.clone();
    let (a, b) = tokio::join!(
        request_json(
            &mut router_a,
            Method::POST,
            "/api/terminal/term-1/restart",
            Some(json!({ "cwd": first }))
        ),
        request_json(
            &mut router_b,
            Method::POST,
            "/api/terminal/term-1/restart",
            Some(json!({ "cwd": second }))
        ),
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(b.0, StatusCode::OK);
    assert_eq!(
        provider
            .spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        3
    );
    eventually(|| fake_at(&provider, 0).kills().first().is_some()).await;
    eventually(|| fake_at(&provider, 1).kills().first().is_some()).await;
    assert!(fake_at(&provider, 2).kills().is_empty());
    assert_eq!(
        fake_at(&provider, 2)
            .spawned_with
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cwd,
        second
    );
}

/// 并发同参创建被去重为一次 spawn；对在途创建以不同 cwd 或不同 shell 干扰
/// 会被拒绝为冲突（多线程 runtime：spawn 延迟阻塞工作线程，pending 窗口
/// 必须在另一线程上仍可观察）。
// Multi-thread runtime: the fake provider's spawn delay blocks its worker
// thread, and the pending-create window must stay observable on another.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deduplicates_concurrent_creates_and_rejects_conflicts() {
    let (state, provider) = test_state();
    let repo = unique_dir("dedupe").to_string_lossy().into_owned();
    let other = unique_dir("dedupe-other").to_string_lossy().into_owned();
    let router = test_router(Arc::clone(&state));

    let mut a = router.clone();
    let mut b = router.clone();
    let (a, b) = tokio::join!(
        request_json(
            &mut a,
            Method::POST,
            "/api/terminal/create",
            Some(json!({ "sessionId": "term-shared", "cwd": repo }))
        ),
        request_json(
            &mut b,
            Method::POST,
            "/api/terminal/create",
            Some(json!({ "sessionId": "term-shared", "cwd": repo }))
        ),
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(b.0, StatusCode::OK);
    assert_eq!(a.1["sessionId"], "term-shared");
    assert_eq!(b.1["sessionId"], "term-shared");
    assert_eq!(
        provider
            .spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        1
    );

    let mut router = router.clone();
    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-shared", "cwd": other })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({ "error": "Terminal session belongs to a different working directory" })
    );

    // Conflicting shell preference against the in-flight create: hold the
    // first create open with a fake spawn delay so its pending slot is live.
    provider
        .spawn_delay_ms
        .store(250, std::sync::atomic::Ordering::Release);
    let repo_clone = repo.clone();
    let mut c = router.clone();
    let mut d = router.clone();
    let blocker = tokio::spawn(async move {
        let (status, body) = request_json(
            &mut c,
            Method::POST,
            "/api/terminal/create",
            Some(json!({ "sessionId": "term-x", "cwd": repo_clone })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    });
    // Wait until the pending-create slot is registered, then conflict.
    tokio::time::sleep(Duration::from_millis(50)).await; // pending slot registered before spawn completes
    let (status, body) = request_json(
        &mut d,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-x", "cwd": repo, "shell": "zsh" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        json!({ "error": "Terminal session is already being created with a different shell" })
    );
    blocker.await.unwrap();
}

/// 达到 MAX_SESSIONS 上限后再创建返回 429，且不多 spawn 进程。
#[tokio::test]
async fn enforces_the_session_cap_with_429() {
    let (state, provider) = test_state();
    let cwd = unique_dir("cap").to_string_lossy().into_owned();
    let mut router = test_router(Arc::clone(&state));

    for index in 0..crate::terminal::runtime::MAX_SESSIONS {
        let (status, _) = request_json(
            &mut router,
            Method::POST,
            "/api/terminal/create",
            Some(json!({ "sessionId": format!("term-{index}"), "cwd": cwd })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "cwd": cwd })),
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body,
        json!({ "error": "Maximum terminal sessions reached" })
    );
    assert_eq!(
        provider
            .spawned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len(),
        crate::terminal::runtime::MAX_SESSIONS
    );
}

/// 退出后的会话保留在列表里（仍可 resize），直到显式删除；删除后进程已清空、
/// 不再补发信号，重复删除返回 404。
#[tokio::test]
async fn retains_exited_sessions_and_closes_on_delete() {
    let (state, provider) = test_state();
    let cwd = unique_dir("exited").to_string_lossy().into_owned();
    let mut router = test_router(Arc::clone(&state));

    let (status, _) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-1", "cwd": cwd })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let process = fake_at(&provider, 0);
    process.emit_data("last output");
    process.emit_exit(Some(7), None);
    eventually(|| {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .get("term-1")
            .map(|session| {
                session
                    .inner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .status
                    == crate::terminal::runtime::SessionStatus::Exited
            })
            .unwrap_or(false)
    })
    .await;

    // Exited sessions still resize (retained until explicit close).
    let (status, _) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/term-1/resize",
        Some(json!({ "cols": 80, "rows": 24 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) =
        request_json(&mut router, Method::DELETE, "/api/terminal/term-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "success": true, "released": true, "killed": true })
    );
    // The exit drain already cleared `session.process`, so termination is a
    // no-op exactly like JS `terminateProcess(null)` — no signals recorded.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(process.kills().is_empty(), "kills: {:?}", process.kills());

    let (status, body) =
        request_json(&mut router, Method::DELETE, "/api/terminal/term-1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "Terminal session not found" }));
}

/// 带 claimant 的删除只释放该窗口认领：其余认领存活则进程保留；把幸存认领
/// 改成过期时间戳后，再删除会真正击杀。
#[tokio::test]
async fn releases_only_the_closing_claimant_and_kills_when_the_last_claim_goes() {
    let (state, provider) = test_state();
    let cwd = unique_dir("claims").to_string_lossy().into_owned();
    let mut router = test_router(Arc::clone(&state));

    let (status, _) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-1", "cwd": cwd })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for claimant in ["client-a", "client-b"] {
        let (status, _) = request_json(
            &mut router,
            Method::POST,
            "/api/terminal/touch",
            Some(json!({ "sessionIds": ["term-1"], "claimant": claimant })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    // Window A closes; window B still claims the session, so the PTY lives.
    let (status, body) = request_json(
        &mut router,
        Method::DELETE,
        "/api/terminal/term-1?claimant=client-a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "success": true, "released": true, "killed": false })
    );
    assert!(fake_at(&provider, 0).kills().is_empty());

    // Expired claims must not keep sessions undead: age the survivor out.
    {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let session = sessions.get("term-1").unwrap();
        session
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .claims
            .insert("client-b".to_string(), 0);
    }
    let (status, body) = request_json(
        &mut router,
        Method::DELETE,
        "/api/terminal/term-1?claimant=client-a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "success": true, "released": true, "killed": true })
    );
    eventually(|| fake_at(&provider, 0).kills().first().is_some()).await;
}

/// 不带 claimant 的删除是无条件击杀：即使仍有存活认领也照杀不误。
#[tokio::test]
async fn closes_unconditionally_without_a_claimant_even_while_claims_are_live() {
    let (state, provider) = test_state();
    let cwd = unique_dir("uncond").to_string_lossy().into_owned();
    let mut router = test_router(Arc::clone(&state));

    let (status, _) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-1", "cwd": cwd })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/touch",
        Some(json!({ "sessionIds": ["term-1"], "claimant": "client-a" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) =
        request_json(&mut router, Method::DELETE, "/api/terminal/term-1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "success": true, "released": true, "killed": true })
    );
    eventually(|| fake_at(&provider, 0).kills().first().is_some()).await;
}

/// force-kill 按 cwd 精确命中目标会话（SIGKILL），不波及其它目录的会话。
#[tokio::test]
async fn force_kill_targets_a_session_or_working_directory() {
    let (state, provider) = test_state();
    let repo = unique_dir("fk-repo").to_string_lossy().into_owned();
    let other = unique_dir("fk-other").to_string_lossy().into_owned();
    let mut router = test_router(Arc::clone(&state));

    for (id, cwd) in [("term-a", &repo), ("term-b", &other)] {
        let (status, _) = request_json(
            &mut router,
            Method::POST,
            "/api/terminal/create",
            Some(json!({ "sessionId": id, "cwd": cwd })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let (status, body) = request_json(
        &mut router,
        Method::POST,
        "/api/terminal/force-kill",
        Some(json!({ "cwd": repo })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "success": true, "killedCount": 1, "killedSessionIds": ["term-a"] })
    );
    eventually(|| fake_at(&provider, 0).kills() == vec!["SIGKILL"]).await;
    assert!(fake_at(&provider, 1).kills().is_empty());
}

/// 子进程环境剥离宿主私有变量（ARGV0/ELECTRON_RUN_AS_NODE/BASH_ENV），
/// PATH 替换为增强版并注入 COLORFGBG/NODE_CHANNEL_FD，HOME 等正常保留。
#[tokio::test]
async fn child_env_strips_host_private_variables() {
    let mut parent = HashMap::new();
    parent.insert("PATH".to_string(), "/original".to_string());
    parent.insert("ARGV0".to_string(), "/app/AppImage".to_string());
    parent.insert("ELECTRON_RUN_AS_NODE".to_string(), "1".to_string());
    parent.insert("BASH_ENV".to_string(), "/evil".to_string());
    parent.insert("HOME".to_string(), "/home/dev".to_string());

    let env = crate::terminal::runtime::build_child_env(
        &parent,
        "/augmented",
        crate::terminal::runtime::ThemeMode::Dark,
    );
    assert_eq!(env.get("PATH").map(String::as_str), Some("/augmented"));
    assert_eq!(env.get("COLORFGBG").map(String::as_str), Some("15;0"));
    assert!(!env.contains_key("ARGV0"));
    assert!(!env.contains_key("ELECTRON_RUN_AS_NODE"));
    assert!(!env.contains_key("BASH_ENV"));
    assert_eq!(env.get("NODE_CHANNEL_FD").map(String::as_str), Some(""));
    assert_eq!(env.get("HOME").map(String::as_str), Some("/home/dev"));
}

// ---------------------------------------------------------------------------
// Minimal RFC6455 client + loopback server
// ---------------------------------------------------------------------------

/// 手写的最小 WebSocket 客户端：裸 TCP 上完成 RFC6455 升级握手并收发帧。
struct WsClient {
    /// 到回环服务器的 TCP 流。
    stream: tokio::net::TcpStream,
    /// 已接收未消费的字节，作为帧解析的缓冲区（握手残余也在这里）。
    buffer: Vec<u8>,
}

/// 帧收发与控制消息解码。
impl WsClient {
    /// 发送升级请求并断言 101；握手响应里多读的字节转入缓冲区。
    async fn connect(port: u16) -> Self {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("tcp connect");
        let key = base64::engine::general_purpose::STANDARD.encode([
            0x12u8, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88,
        ]);
        let request = format!(
            "GET /api/terminal/ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream
            .write_all(request.as_bytes())
            .await
            .expect("handshake send");
        let mut handshake = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).await.expect("handshake read");
            handshake.extend_from_slice(&chunk[..read]);
            if handshake.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&handshake);
        assert!(text.starts_with("HTTP/1.1 101"), "upgrade failed: {text}");
        let leftover = handshake
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| handshake[index + 4..].to_vec())
            .unwrap_or_default();
        Self {
            stream,
            buffer: leftover,
        }
    }

    /// 构造并发出一个带固定掩码的客户端帧（按载荷长度选择 7/16/64 位长度编码）。
    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) {
        let mut frame = vec![0x80 | opcode];
        let mask_key = [0x11u8, 0x22, 0x33, 0x44];
        match payload.len() {
            len if len < 126 => frame.push(0x80 | len as u8),
            len if len < 65536 => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask_key);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask_key[index % 4]),
        );
        self.stream.write_all(&frame).await.expect("frame send");
    }

    /// 把 JSON 控制消息打包成 binary 帧发送。
    async fn send_control(&mut self, message: &Value) {
        self.send_frame(0x2, &protocol::create_terminal_ws_control_frame(message))
            .await;
    }

    /// 发送 text 帧，用于验证服务端拒绝非 binary 帧。
    async fn send_text(&mut self, text: &str) {
        self.send_frame(0x1, text.as_bytes()).await;
    }

    /// 从流里读一块进缓冲区；EOF 或错误返回 false。
    async fn fill_buffer(&mut self) -> bool {
        let mut chunk = [0u8; 64 * 1024];
        match self.stream.read(&mut chunk).await {
            Ok(0) | Err(_) => false,
            Ok(read) => {
                self.buffer.extend_from_slice(&chunk[..read]);
                true
            }
        }
    }

    /// Next data frame payload (binary or text); `None` on close/EOF.
    /// 中文补充：服务器帧不带掩码，此处只解析长度与 opcode；ping/pong 被跳过。
    async fn recv_payload(&mut self) -> Option<Vec<u8>> {
        loop {
            if self.buffer.len() >= 2 {
                let length = self.buffer[1] & 0x7f;
                let header = match length {
                    126 => 4,
                    127 => 10,
                    _ => 2,
                };
                if self.buffer.len() >= header {
                    let payload_len = match length {
                        126 => u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as usize,
                        127 => {
                            usize::from_be_bytes(self.buffer[2..10].try_into().expect("u64 bytes"))
                        }
                        _ => length as usize,
                    };
                    if self.buffer.len() >= header + payload_len {
                        let opcode = self.buffer[0] & 0x0f;
                        let payload = self.buffer[header..header + payload_len].to_vec();
                        self.buffer.drain(..header + payload_len);
                        match opcode {
                            0x1 | 0x2 => return Some(payload),
                            0x8 => return None,
                            _ => continue, // ping/pong skipped
                        }
                    }
                }
            }
            if !self.fill_buffer().await {
                return None;
            }
        }
    }

    /// Next decoded control message of the given type (and optional session).
    /// 中文补充：不匹配的帧被丢弃，直到类型（及可选 session）符合为止。
    async fn next(&mut self, kind: &str, session: Option<&str>) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let payload = tokio::time::timeout_at(deadline, self.recv_payload())
                .await
                .expect("timed out waiting for frame")
                .expect("connection closed");
            if let Some(message) = protocol::read_terminal_ws_control_frame(&payload) {
                if message.get("t").and_then(Value::as_str) == Some(kind)
                    && session.is_none_or(|expected| {
                        message.get("s").and_then(Value::as_str) == Some(expected)
                    })
                {
                    return message;
                }
            }
        }
    }

    /// 收集在途消息直到流连续静默（连续 4 个 50ms 窗口无帧或连接关闭）。
    async fn drain_pending(&mut self) -> Vec<Value> {
        let mut messages = Vec::new();
        let mut quiet = 0;
        // Collect frames until the stream stays quiet for a few 50ms windows.
        while quiet < 4 {
            match tokio::time::timeout(Duration::from_millis(50), self.recv_payload()).await {
                Ok(Some(payload)) => {
                    quiet = 0;
                    if let Some(message) = protocol::read_terminal_ws_control_frame(&payload) {
                        messages.push(message);
                    }
                }
                Ok(None) => break,
                Err(_) => quiet += 1,
            }
        }
        messages
    }
}

/// 在随机端口起一个真实的后台 axum server，返回端口号。
async fn serve(router: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    port
}

/// Minimal percent-encoding for query strings in test URLs.
/// 中文补充：仅编码查询串用途，斜杠保持原样。
fn urlencoding_lite(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// WS 全链路 happy path：hello 后 attach 先发快照；scoped write 落到对应进程；
/// 输出带递增序号 q；detach 只停对应终端的投递；重连按有界历史对账；退出
/// 携带 exitCode；HTTP 删除触发 fatal 的 CLOSED error 广播。
#[tokio::test]
async fn ws_runs_snapshot_first_attach_scoped_io_replay_and_close() {
    let (state, provider) = test_state();
    let repo = unique_dir("ws-repo").to_string_lossy().into_owned();
    let other = unique_dir("ws-other").to_string_lossy().into_owned();
    let mut http = test_router(Arc::clone(&state));
    for (id, cwd) in [("term-live", &repo), ("term-second", &other)] {
        let (status, _) = request_json(
            &mut http,
            Method::POST,
            "/api/terminal/create",
            Some(json!({ "sessionId": id, "cwd": cwd, "cols": 80, "rows": 24 })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let port = serve(test_router(Arc::clone(&state))).await;
    let mut client = WsClient::connect(port).await;
    let hello = client.next("hello", None).await;
    let connection_id = hello["connectionId"]
        .as_str()
        .expect("connectionId")
        .to_string();
    assert_eq!(hello["v"], 3);
    assert!(!connection_id.is_empty());

    client
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-live" }))
        .await;
    client
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-second" }))
        .await;
    let snapshot = client.next("snapshot", Some("term-live")).await;
    assert_eq!(snapshot["q"], 0);
    assert_eq!(snapshot["history"], "");
    assert_eq!(snapshot["status"], "running");
    assert_eq!(snapshot["runtime"], "node");
    assert_eq!(snapshot["ptyBackend"], "fake-pty");
    client.next("snapshot", Some("term-second")).await;

    // Scoped input: writes always carry the terminal id.
    client
        .send_control(&json!({ "t": "write", "v": 3, "s": "term-live", "d": "echo ok\r" }))
        .await;
    client
        .send_control(&json!({ "t": "write", "v": 3, "s": "term-second", "d": "pwd\r" }))
        .await;
    client
        .send_control(&json!({ "t": "write", "v": 3, "s": "term-live", "d": "echo next\r" }))
        .await;
    eventually(|| fake_at(&provider, 0).writes() == vec!["echo ok\r", "echo next\r"]).await;
    eventually(|| fake_at(&provider, 1).writes() == vec!["pwd\r"]).await;

    fake_at(&provider, 1).emit_data("/other\r\n");
    let output = client.next("output", Some("term-second")).await;
    assert_eq!(output["q"], 1);
    assert_eq!(output["d"], "/other\r\n");

    // Detach stops delivery for that terminal only.
    client
        .send_control(&json!({ "t": "detach", "v": 3, "s": "term-second" }))
        .await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    fake_at(&provider, 1).emit_data("detached\r\n");
    tokio::time::sleep(Duration::from_millis(50)).await;
    for message in client.drain_pending().await {
        assert!(
            !(message["t"] == "output" && message["s"] == "term-second"),
            "detached terminal still delivered: {message}"
        );
    }

    fake_at(&provider, 0).emit_data("ok\r\n");
    let output = client.next("output", Some("term-live")).await;
    assert_eq!(output["q"], 1);
    assert_eq!(output["d"], "ok\r\n");

    // Sanitized replay bytes ride the `r` field for stripped query exchanges.
    fake_at(&provider, 0).emit_data("\u{1b}[6n");
    let stripped = client.next("output", Some("term-live")).await;
    assert_eq!(stripped["q"], 2);
    assert_eq!(stripped["d"], "\u{1b}[6n");
    assert_eq!(stripped["r"], "");

    // Reconnect reconciles from bounded history; exit carries the code.
    drop(client);
    let mut second = WsClient::connect(port).await;
    second.next("hello", None).await;
    second
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-live" }))
        .await;
    let snapshot = second.next("snapshot", Some("term-live")).await;
    assert_eq!(snapshot["q"], 2);
    assert_eq!(snapshot["history"], "ok\r\n");
    fake_at(&provider, 0).emit_exit(Some(7), None);
    let exit = second.next("exit", Some("term-live")).await;
    assert_eq!(exit["q"], 3);
    assert_eq!(exit["exitCode"], 7);
    assert_eq!(exit["signal"], Value::Null);

    let mut closer = test_router(Arc::clone(&state));
    let (status, _) =
        request_json(&mut closer, Method::DELETE, "/api/terminal/term-live", None).await;
    assert_eq!(status, StatusCode::OK);
    let error = second.next("error", Some("term-live")).await;
    assert_eq!(error["code"], "CLOSED");
    assert_eq!(error["fatal"], true);
    assert_eq!(error["message"], "Terminal closed");
}

/// 帧校验与错误路径：text 帧/坏 tag/旧版本帧返回 BAD_FRAME（非 fatal）；
/// 未知会话的 attach 返回 fatal 的 SESSION_NOT_FOUND；ping 应答 pong；
/// 空输入与死进程写入分别返回 BAD_INPUT/NOT_RUNNING 且不断连。
#[tokio::test]
async fn ws_rejects_text_frames_and_unknown_sessions_and_pongs_pings() {
    let (state, provider) = test_state();
    let cwd = unique_dir("ws-err").to_string_lossy().into_owned();
    let mut http = test_router(Arc::clone(&state));
    let (status, _) = request_json(
        &mut http,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-1", "cwd": cwd })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let port = serve(test_router(Arc::clone(&state))).await;
    let mut client = WsClient::connect(port).await;
    client.next("hello", None).await;

    client.send_text("not binary").await;
    let error = client.next("error", None).await;
    assert_eq!(error["code"], "BAD_FRAME");
    assert_eq!(error["message"], "Binary control frame required");
    assert_eq!(error["fatal"], false);

    client.send_frame(0x2, b"\x02not-a-tag").await;
    let error = client.next("error", None).await;
    assert_eq!(error["code"], "BAD_FRAME");
    assert_eq!(error["message"], "Invalid terminal frame");

    client
        .send_control(&json!({ "t": "write", "v": 2, "s": "term-1", "d": "x" }))
        .await;
    let error = client.next("error", None).await;
    assert_eq!(error["code"], "BAD_FRAME");

    client
        .send_control(&json!({ "t": "attach", "v": 3, "s": "missing" }))
        .await;
    let error = client.next("error", Some("missing")).await;
    assert_eq!(error["code"], "SESSION_NOT_FOUND");
    assert_eq!(error["message"], "Terminal session not found");
    assert_eq!(error["fatal"], true);

    client.send_control(&json!({ "t": "ping", "v": 3 })).await;
    let pong = client.next("pong", None).await;
    assert_eq!(pong["v"], 3);

    // Bad input and dead writes are reported without closing the socket.
    client
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-1" }))
        .await;
    client.next("snapshot", Some("term-1")).await;
    client
        .send_control(&json!({ "t": "write", "v": 3, "s": "term-1", "d": "" }))
        .await;
    let error = client.next("error", Some("term-1")).await;
    assert_eq!(error["code"], "BAD_INPUT");
    fake_at(&provider, 0).emit_exit(Some(0), None);
    eventually(|| {
        let sessions = state.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.get("term-1").is_none_or(|session| {
            session
                .inner
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .process
                .is_none()
        })
    })
    .await;
    client
        .send_control(&json!({ "t": "write", "v": 3, "s": "term-1", "d": "x" }))
        .await;
    let error = client.next("error", Some("term-1")).await;
    assert_eq!(error["code"], "NOT_RUNNING");
}

/// 洪泛抑制：从不 ack 时投递被压制在远小于发送量的字节内且 exit 不被抑制；
/// ack 之后以有界快照恢复直播，晚到的 attach 拿到同样有界的尾部历史。
#[tokio::test]
async fn ws_bounds_flood_output_with_ack_driven_suppression_and_snapshot_recovery() {
    let (state, provider) = test_state();
    let cwd = unique_dir("ws-flood").to_string_lossy().into_owned();
    let mut http = test_router(Arc::clone(&state));
    let (status, _) = request_json(
        &mut http,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-flood", "cwd": cwd, "cols": 80, "rows": 24 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let port = serve(test_router(Arc::clone(&state))).await;
    let mut client = WsClient::connect(port).await;
    client.next("hello", None).await;
    client
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-flood" }))
        .await;
    client.next("snapshot", Some("term-flood")).await;

    // Small output flows live without any acknowledgment.
    fake_at(&provider, 0).emit_data("hello\r\n");
    let output = client.next("output", Some("term-flood")).await;
    assert_eq!(output["d"], "hello\r\n");

    // Flood ~12.6 MiB in 64 KiB chunks while never acknowledging: suppression
    // must engage (no acks, >8 MiB fallback), bounding delivered bytes.
    let chunk = format!("{}\n", "x".repeat(64 * 1024 - 1));
    let chunks = 200;
    for _ in 0..chunks {
        fake_at(&provider, 0).emit_data(&chunk);
    }
    // Collect everything still in flight.
    let mut outputs = Vec::new();
    let mut last_q = 0u64;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let received = match tokio::time::timeout_at(deadline, client.recv_payload()).await {
            Ok(Some(payload)) => payload,
            Ok(None) | Err(_) => break,
        };
        if let Some(message) = protocol::read_terminal_ws_control_frame(&received) {
            if message["t"] == "output" {
                last_q = last_q.max(message["q"].as_u64().unwrap_or(0));
                outputs.push(message);
            }
        }
    }
    let received_bytes: usize = outputs
        .iter()
        .map(|message| message["d"].as_str().map(str::len).unwrap_or(0))
        .sum();
    assert!(outputs.len() < chunks, "outputs={}", outputs.len());
    assert!(
        received_bytes < 9 * 1024 * 1024,
        "received {received_bytes} bytes"
    );

    // Exit is never suppressed.
    fake_at(&provider, 0).emit_exit(Some(3), None);
    let exit = client.next("exit", Some("term-flood")).await;
    assert_eq!(exit["exitCode"], 3);

    // Acknowledging drains the lag: the attachment recovers with a bounded
    // snapshot reflecting the tail, and live output resumes.
    client
        .send_control(&json!({ "t": "ack", "v": 3, "s": "term-flood", "q": last_q + 100 }))
        .await;
    let recovery = client.next("snapshot", Some("term-flood")).await;
    let history = recovery["history"].as_str().unwrap_or_default();
    assert!(
        history.len() <= 512 * 1024 + 1024,
        "history len={}",
        history.len()
    );
    assert!(
        history.ends_with("x\n"),
        "history tail: {:?}",
        &history[history.len().saturating_sub(8)..]
    );

    // The session has exited (stale process ids are dropped, like the JS);
    // instead a fresh attachment gets the same bounded tail, like the JS test.
    let mut late = WsClient::connect(port).await;
    late.next("hello", None).await;
    late.send_control(&json!({ "t": "attach", "v": 3, "s": "term-flood" }))
        .await;
    let late_snapshot = late.next("snapshot", Some("term-flood")).await;
    let late_history = late_snapshot["history"].as_str().unwrap_or_default();
    assert!(late_history.len() <= 512 * 1024 + 1024);
    assert!(late_history.ends_with("x\n"));
}

/// 视口所有权协商：IDLE 下取最小尺寸、claimViewport 抢占驱动权、follower
/// 的 viewport 更新不再改 PTY 尺寸、release 回到最小尺寸、驱动断连自动
/// 释放并重协商、未 attach 的连接不能 claim（NOT_ATTACHED）。
// The debug-build future for this flow exceeds the default 2 MiB test-thread
// stack, so it runs on a dedicated big-stack thread with its own runtime.
#[test]
fn ws_negotiates_viewport_ownership_across_connections() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(ws_negotiates_viewport_body());
        })
        .expect("spawn test thread")
        .join()
        .expect("test thread panicked");
}

/// 上一测试的实际执行体：在独立的大栈线程 + 自建 runtime 中 block_on。
async fn ws_negotiates_viewport_body() {
    let (state, provider) = test_state();
    let cwd = unique_dir("ws-viewport").to_string_lossy().into_owned();
    let mut http = test_router(Arc::clone(&state));
    let (status, _) = request_json(
        &mut http,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-drv", "cwd": cwd, "cols": 80, "rows": 24 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let port = serve(test_router(Arc::clone(&state))).await;

    let mut big = WsClient::connect(port).await;
    big.next("hello", None).await;
    let mut small = WsClient::connect(port).await;
    small.next("hello", None).await;

    big.send_control(&json!({ "t": "attach", "v": 3, "s": "term-drv", "cols": 100, "rows": 30 }))
        .await;
    big.next("snapshot", Some("term-drv")).await;
    let resized = big.next("resized", Some("term-drv")).await;
    assert_eq!(resized["cols"], 100);
    assert_eq!(resized["rows"], 30);

    small
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-drv", "cols": 60, "rows": 20 }))
        .await;
    small.next("snapshot", Some("term-drv")).await;
    // IDLE min-size: the narrowest device implicitly owns the grid.
    let narrowed = big.next("resized", Some("term-drv")).await;
    assert_eq!(narrowed["cols"], 60);
    assert_eq!(narrowed["rows"], 20);
    assert_eq!(fake_at(&provider, 0).resizes().last(), Some(&(60, 20)));

    // Claim: the big screen takes control; the PTY follows its viewport.
    big.send_control(
        &json!({ "t": "claimViewport", "v": 3, "s": "term-drv", "cols": 100, "rows": 30 }),
    )
    .await;
    let small_sees = small.next("driverChanged", Some("term-drv")).await;
    assert_eq!(small_sees["cols"], 100);
    assert_eq!(small_sees["rows"], 30);
    assert_eq!(fake_at(&provider, 0).resizes().last(), Some(&(100, 30)));

    // A follower viewport update must NOT resize the PTY in DRIVEN mode.
    small
        .send_control(&json!({ "t": "viewport", "v": 3, "s": "term-drv", "cols": 50, "rows": 15 }))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fake_at(&provider, 0).resizes().last(), Some(&(100, 30)));

    // Release: back to IDLE min-size across remaining attachments.
    big.send_control(&json!({ "t": "releaseViewport", "v": 3, "s": "term-drv" }))
        .await;
    let released = small.next("driverChanged", Some("term-drv")).await;
    assert_eq!(released["driverId"], Value::Null);
    assert_eq!(fake_at(&provider, 0).resizes().last(), Some(&(50, 15)));
    // Both attachments receive every broadcast; drain big's backlog (the
    // claim copy, then the release) so later waits observe fresh events.
    for _ in 0..4 {
        let big_copy = big.next("driverChanged", Some("term-drv")).await;
        if big_copy["driverId"] == Value::Null {
            break;
        }
    }

    // Claiming requires attachment.
    let mut stranger = WsClient::connect(port).await;
    stranger.next("hello", None).await;
    stranger
        .send_control(
            &json!({ "t": "claimViewport", "v": 3, "s": "term-drv", "cols": 90, "rows": 30 }),
        )
        .await;
    let error = stranger.next("error", Some("term-drv")).await;
    assert_eq!(error["code"], "NOT_ATTACHED");

    // Dropping the driver connection releases the role and renegotiates.
    small
        .send_control(
            &json!({ "t": "claimViewport", "v": 3, "s": "term-drv", "cols": 120, "rows": 40 }),
        )
        .await;
    big.next("driverChanged", Some("term-drv")).await;
    assert_eq!(fake_at(&provider, 0).resizes().last(), Some(&(120, 40)));
    drop(small);
    let after_drop = big.next("driverChanged", Some("term-drv")).await;
    assert_eq!(after_drop["driverId"], Value::Null);
    assert_eq!(fake_at(&provider, 0).resizes().last(), Some(&(100, 30)));

    // A releaseViewport from a non-driver is ignored.
    big.send_control(&json!({ "t": "releaseViewport", "v": 3, "s": "term-drv" }))
        .await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fake_at(&provider, 0).resizes().last(), Some(&(100, 30)));
}

/// grid feed：attach feed=grid 后输出以解析过的 rows 差分帧投递（含颜色），
/// byte 附件照常收原始字节且序号恰好 +1；grid 重连从解析屏幕而非字节历史
/// 对账；resize 以新尺寸触发全量帧。
#[tokio::test]
async fn ws_grid_feed_delivers_parsed_frames_and_byte_feed_coexists() {
    let (state, provider) = test_state();
    let cwd = unique_dir("ws-grid").to_string_lossy().into_owned();
    let mut http = test_router(Arc::clone(&state));
    let (status, _) = request_json(
        &mut http,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-grid", "cwd": cwd, "cols": 80, "rows": 24 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let port = serve(test_router(Arc::clone(&state))).await;

    let mut grid_client = WsClient::connect(port).await;
    grid_client.next("hello", None).await;
    let mut byte_client = WsClient::connect(port).await;
    byte_client.next("hello", None).await;

    // While only a byte attachment watches, sequences advance by exactly one
    // per output — no drift from the grid capability.
    byte_client
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-grid" }))
        .await;
    let byte_snapshot = byte_client.next("snapshot", Some("term-grid")).await;
    fake_at(&provider, 0).emit_data("one\r\n");
    let output = byte_client.next("output", Some("term-grid")).await;
    assert_eq!(output["q"], byte_snapshot["q"].as_u64().unwrap() + 1);

    grid_client
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-grid", "feed": "grid" }))
        .await;
    let grid_snapshot = grid_client.next("snapshot", Some("term-grid")).await;
    assert_eq!(grid_snapshot["history"], "");
    assert_eq!(grid_snapshot["grid"]["t"], "full");

    fake_at(&provider, 0).emit_data("\u{1b}[32mhello\u{1b}[0m grid");
    // Cursor-only frames legitimately ride the feed (deferred drains); wait
    // for one carrying content.
    let mut frame = Value::Null;
    for _ in 0..5 {
        let grid_frame = grid_client.next("grid", Some("term-grid")).await;
        if grid_frame["g"]["t"] != "cursor" {
            frame = grid_frame["g"].clone();
            break;
        }
    }
    let frame = &frame;
    assert_eq!(frame["t"], "rows");
    // The write lands at the cursor (row 1 after the earlier `one\r\n`), so
    // read whichever row the diff carries.
    let rows_map = frame["rowsMap"].as_object().expect("rowsMap");
    let row = rows_map.values().next().expect("a changed row");
    let text: String = row
        .as_array()
        .unwrap()
        .iter()
        .map(|cell| {
            let cp = cell[0].as_u64().unwrap_or(32) as u32;
            if cp >= 32 {
                char::from_u32(cp).unwrap_or(' ')
            } else {
                ' '
            }
        })
        .collect();
    assert_eq!(text.trim_end(), "hello grid");
    // Green fg on the first content cell (ghostty-vt aligned palette).
    let fg = row[0][1].as_u64().unwrap();
    assert_eq!((fg >> 16) & 255, 181);

    let byte_output = byte_client.next("output", Some("term-grid")).await;
    assert_eq!(byte_output["d"], "\u{1b}[32mhello\u{1b}[0m grid");

    // A grid-fed reconnect reconciles from the parsed screen, not byte history.
    drop(grid_client);
    let mut regrid = WsClient::connect(port).await;
    regrid.next("hello", None).await;
    regrid
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-grid", "feed": "grid" }))
        .await;
    let snapshot = regrid.next("snapshot", Some("term-grid")).await;
    assert_eq!(snapshot["history"], "");
    assert_eq!(snapshot["grid"]["t"], "full");
    let cells = snapshot["grid"]["cells"].as_array().expect("grid cells");
    let all_rows: String = cells
        .iter()
        .map(|row| {
            row.as_array()
                .unwrap()
                .iter()
                .map(|cell| {
                    let cp = cell[0].as_u64().unwrap_or(32) as u32;
                    if cp >= 32 {
                        char::from_u32(cp).unwrap_or(' ')
                    } else {
                        ' '
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<String>>()
        .join("\n");
    assert!(all_rows.contains("hello grid"), "grid rows: {all_rows}");
    assert!(
        all_rows.contains("one"),
        "earlier output stays parsed: {all_rows}"
    );

    // Resizes propagate to the grid as a full frame at the new dimensions.
    let mut resizer = test_router(Arc::clone(&state));
    let (status, _) = request_json(
        &mut resizer,
        Method::POST,
        "/api/terminal/term-grid/resize",
        Some(json!({ "cols": 20, "rows": 4 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let frame = regrid.next("grid", Some("term-grid")).await;
    assert_eq!(frame["g"]["t"], "full");
    assert_eq!(frame["g"]["cols"], 20);
    assert_eq!(frame["g"]["rows"], 4);
}

/// resync 控制消息让服务端重发携带最新历史的快照。
#[tokio::test]
async fn ws_resync_sends_a_fresh_snapshot() {
    let (state, provider) = test_state();
    let cwd = unique_dir("ws-resync").to_string_lossy().into_owned();
    let mut http = test_router(Arc::clone(&state));
    let (status, _) = request_json(
        &mut http,
        Method::POST,
        "/api/terminal/create",
        Some(json!({ "sessionId": "term-sync", "cwd": cwd })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let port = serve(test_router(Arc::clone(&state))).await;
    let mut client = WsClient::connect(port).await;
    client.next("hello", None).await;
    client
        .send_control(&json!({ "t": "attach", "v": 3, "s": "term-sync" }))
        .await;
    client.next("snapshot", Some("term-sync")).await;
    fake_at(&provider, 0).emit_data("payload\r\n");
    client.next("output", Some("term-sync")).await;

    client
        .send_control(&json!({ "t": "resync", "v": 3, "s": "term-sync" }))
        .await;
    let snapshot = client.next("snapshot", Some("term-sync")).await;
    assert_eq!(snapshot["history"], "payload\r\n");
}
