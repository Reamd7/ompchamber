//! Tests for `ngrok.rs` (JS precedent: extract/summarize helpers from
//! ngrok-tunnel.js and the provider diagnose shape from providers/ngrok.js).
//! （中文说明）ngrok provider 的单元测试：公网 URL 归一化、stdout
//! 提取、错误摘要、本地 agent API 轮询、quick tunnel 的 argv/就绪/
//! 超时行为，以及 diagnose 与 capabilities 的 JSON 形状对齐。

use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::tunnels::runner::testutil::FakeRunner;

/// 行为契约：仅接受 ngrok 域名的 https URL 并去掉一个尾斜杠；其余输入一律 None。
#[test]
fn normalizes_ngrok_public_urls() {
    assert_eq!(
        normalize_ngrok_public_url(Some("https://demo.ngrok-free.app")).as_deref(),
        Some("https://demo.ngrok-free.app")
    );
    // WHATWG renders `https://x.ngrok.io` as `https://x.ngrok.io/`; the JS
    // strips exactly one trailing slash.
    assert_eq!(
        normalize_ngrok_public_url(Some("https://x.ngrok.io/path/")).as_deref(),
        Some("https://x.ngrok.io/path")
    );
    assert_eq!(normalize_ngrok_public_url(Some("http://x.ngrok.io")), None);
    assert_eq!(
        normalize_ngrok_public_url(Some("https://example.com")),
        None
    );
    assert_eq!(normalize_ngrok_public_url(Some("not a url")), None);
    assert_eq!(normalize_ngrok_public_url(Some("")), None);
    assert_eq!(normalize_ngrok_public_url(None), None);
}

/// 行为契约：JSON 行的 url/public_url 字段与纯文本中的 ngrok URL 均可提取；非 ngrok 域名被拒。
#[test]
fn extracts_urls_from_json_lines_and_plain_text() {
    let json_line = json!({"url": "https://abc-1.ngrok-free.app", "lvl": "info"}).to_string();
    assert_eq!(
        extract_ngrok_public_url_from_text(&json_line).as_deref(),
        Some("https://abc-1.ngrok-free.app")
    );
    let json_public = json!({"public_url": "https://abc-2.ngrok-free.app"}).to_string();
    assert_eq!(
        extract_ngrok_public_url_from_text(&json_public).as_deref(),
        Some("https://abc-2.ngrok-free.app")
    );
    assert_eq!(
        extract_ngrok_public_url_from_text("open https://abc-3.ngrok.io in your browser\r\nnext")
            .as_deref(),
        Some("https://abc-3.ngrok.io")
    );
    // Non-ngrok https URLs are rejected.
    assert_eq!(
        extract_ngrok_public_url_from_text("see https://example.com/x"),
        None
    );
    assert_eq!(extract_ngrok_public_url_from_text(""), None);
    assert_eq!(extract_ngrok_public_url_from_text("   "), None);
}

/// 行为契约：错误摘要优先取 error 级别 JSON 行的 err 字段。
#[test]
fn summarize_prefers_error_level_lines() {
    let lines = vec![
        json!({"lvl": "info", "msg": "starting web service"}).to_string(),
        json!({"lvl": "eror", "err": "authentication failed", "msg": "Establishing proxy"})
            .to_string(),
    ];
    assert_eq!(summarize_ngrok_output(&lines), "authentication failed");
}

/// 行为契约：无 error 级别行时回退到 failed 前缀的 msg，再回退到 info 行的 err 字段。
#[test]
fn summarize_falls_back_to_failed_msgs_and_err_fields() {
    let lines = vec![
        json!({"lvl": "info", "err": "context canceled", "msg": "Session closed"}).to_string(),
        json!({"lvl": "info", "msg": "failed to reconnect session"}).to_string(),
    ];
    // 'context canceled' is skipped; the failed msg wins.
    assert_eq!(
        summarize_ngrok_output(&lines),
        "failed to reconnect session"
    );

    let lines = vec![json!({"lvl": "info", "err": "dial tcp refused"}).to_string()];
    assert_eq!(summarize_ngrok_output(&lines), "dial tcp refused");
}

/// 行为契约：ERROR: 开头的纯文本行也参与摘要，最多拼接四条。
#[test]
fn summarize_uses_error_prefixed_plain_lines() {
    let lines = vec![
        "ERROR: authentication failed".to_string(),
        "ERROR: session limit reached".to_string(),
        "ERROR: third".to_string(),
        "ERROR: fourth".to_string(),
        "ERROR: fifth".to_string(),
        "plain tail".to_string(),
    ];
    assert_eq!(
        summarize_ngrok_output(&lines),
        "authentication failed session limit reached third fourth"
    );
}

/// 行为契约：仍无可用信息时取最后一行；空输入返回空串。
#[test]
fn summarize_falls_back_to_last_line() {
    let lines = vec![
        "starting ngrok...".to_string(),
        "tunnel established".to_string(),
    ];
    assert_eq!(summarize_ngrok_output(&lines), "tunnel established");

    let lines = vec![json!({"lvl": "info", "msg": "final message"}).to_string()];
    assert_eq!(summarize_ngrok_output(&lines), "final message");

    assert_eq!(summarize_ngrok_output(&[]), "");
}

/// 行为契约：append 摘要用冒号连接到基消息后；无摘要时保留原消息。
#[test]
fn append_summary_joins_with_colon() {
    let lines = vec![json!({"lvl": "eror", "err": "auth failed"}).to_string()];
    assert_eq!(
        append_ngrok_output_summary("Ngrok tunnel URL not received within 30 seconds", &lines),
        "Ngrok tunnel URL not received within 30 seconds: auth failed"
    );
    assert_eq!(
        append_ngrok_output_summary("base message", &[]),
        "base message"
    );
}

/// 行为契约：从本地 agent API 的 tunnels 列表取首个 https 公网 URL；
/// API 不可达返回 None（外层轮询继续等待）。
#[tokio::test]
async fn fetches_public_url_from_local_agent_api() {
    // Minimal HTTP server speaking the ngrok agent API shape.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = json!({
        "tunnels": [
            { "proto": "http", "public_url": "http://http-only.ngrok.io" },
            { "proto": "https", "public_url": "https://from-api.ngrok-free.app" },
        ]
    })
    .to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });

    let http = reqwest::Client::new();
    let api_url = format!("http://127.0.0.1:{port}/api/tunnels");
    assert_eq!(
        fetch_ngrok_public_url(&http, &api_url).await.as_deref(),
        Some("https://from-api.ngrok-free.app")
    );
    // Unreachable API returns None (poll loop keeps waiting).
    assert_eq!(
        fetch_ngrok_public_url(&http, "http://127.0.0.1:1/api/tunnels").await,
        None
    );
}

/// 行为契约：quick tunnel 以 `http 127.0.0.1:PORT` + JSON stdout 日志拉起，
/// 公网 URL 从 stdout 的 url 字段读出。
#[tokio::test]
async fn quick_tunnel_argv_and_url_from_stdout_log() {
    let runner = FakeRunner::new()
        .with_probe_stdout("ngrok version 3.6.0")
        .with_config_check(super::super::runner::ProbeOutcome {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        })
        .with_stdout_chunk(
            json!({"lvl": "info", "msg": "started tunnel", "url": "https://abc-def.ngrok-free.app"})
                .to_string() + "\n",
        );
    let http = reqwest::Client::new();
    let controller = start_ngrok_quick_tunnel(
        Some(3000),
        &runner,
        &http,
        &NgrokTunnelOpts {
            startup_timeout_ms: 2_000,
            poll_interval_ms: 50,
            api_url: "http://127.0.0.1:1/api/tunnels".to_string(),
        },
    )
    .await
    .expect("quick tunnel starts");

    assert_eq!(controller.mode, "quick");
    assert_eq!(
        controller.public_url.as_deref(),
        Some("https://abc-def.ngrok-free.app")
    );

    let spawns = runner.recorded_spawns();
    let (command, args, _env) = &spawns[0];
    assert_eq!(command, "ngrok");
    assert_eq!(
        args,
        &vec![
            "http",
            "--log=stdout",
            "--log-format=json",
            "127.0.0.1:3000"
        ]
    );
}

/// 行为契约：二进制缺失与 authtoken 未配置分别返回对应的启动错误。
#[tokio::test]
async fn quick_tunnel_requires_dependency_and_authtoken() {
    let http = reqwest::Client::new();
    let opts = NgrokTunnelOpts::default();

    let runner = FakeRunner::unavailable();
    let error = start_ngrok_quick_tunnel(Some(3000), &runner, &http, &opts)
        .await
        .expect_err("missing dependency");
    assert!(error.starts_with("ngrok is not installed."), "got: {error}");

    // Binary present, authtoken not configured (config check exits non-zero).
    let runner = FakeRunner::new()
        .with_probe_stdout("ngrok version 3.6.0")
        .with_config_check(super::super::runner::ProbeOutcome {
            status: Some(1),
            stdout: String::new(),
            stderr: "authtoken not found".to_string(),
        });
    let error = start_ngrok_quick_tunnel(Some(3000), &runner, &http, &opts)
        .await
        .expect_err("authtoken missing");
    assert_eq!(
        error,
        "ngrok authtoken is not configured. authtoken not found"
    );
}

/// 行为契约：没有本地端口时启动被拒绝。
#[tokio::test]
async fn quick_tunnel_requires_a_port() {
    let runner = FakeRunner::new()
        .with_probe_stdout("ngrok version 3.6.0")
        .with_config_check(super::super::runner::ProbeOutcome {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        });
    let http = reqwest::Client::new();
    let error = start_ngrok_quick_tunnel(None, &runner, &http, &NgrokTunnelOpts::default())
        .await
        .expect_err("port required");
    assert_eq!(error, "A local port is required to start an ngrok tunnel");
}

/// 行为契约：就绪超时返回固定的 30 秒错误消息（无输出时不追加摘要）并终止子进程。
#[tokio::test]
async fn quick_tunnel_timeout_includes_output_summary() {
    let runner = FakeRunner::new()
        .with_probe_stdout("ngrok version 3.6.0")
        .with_config_check(super::super::runner::ProbeOutcome {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
        });
    let http = reqwest::Client::new();
    let error = start_ngrok_quick_tunnel(
        Some(3000),
        &runner,
        &http,
        &NgrokTunnelOpts {
            startup_timeout_ms: 120,
            poll_interval_ms: 40,
            api_url: "http://127.0.0.1:1/api/tunnels".to_string(),
        },
    )
    .await
    .expect_err("times out");
    assert_eq!(error, "Ngrok tunnel URL not received within 30 seconds");
    assert!(runner.kill_called());
}

/// 行为契约：非 quick 模式返回 mode_unsupported 的服务错误。
#[tokio::test]
async fn provider_start_rejects_non_quick_modes() {
    let provider = NgrokTunnelProvider::new(Arc::new(FakeRunner::new()), reqwest::Client::new());
    let request = TunnelStartRequest {
        provider: "ngrok".to_string(),
        mode: "managed-remote".to_string(),
        intent: None,
        config_path: None,
        token: String::new(),
        hostname: String::new(),
    };
    let failure = provider
        .start(request, StartContext::default())
        .await
        .expect_err("mode unsupported");
    match failure {
        StartFailure::Service(error) => {
            assert_eq!(error.code, "mode_unsupported");
            assert_eq!(error.message, "Ngrok only supports 'quick' mode right now");
        }
        other => panic!("expected service error, got {other:?}"),
    }
}

/// 行为契约：diagnose 输出 dependency/authtoken/network 三项检查及
/// quick 模式的就绪状态与空 blockers。
#[tokio::test]
async fn provider_diagnose_reports_dependency_authtoken_network() {
    let provider = NgrokTunnelProvider::new(
        Arc::new(
            FakeRunner::new()
                .with_probe_stdout("ngrok version 3.6.0")
                .with_config_check(super::super::runner::ProbeOutcome {
                    status: Some(0),
                    stdout: "Valid configuration found".to_string(),
                    stderr: String::new(),
                }),
        ),
        reqwest::Client::new(),
    )
    .with_api_probe(Arc::new(|| {
        Box::pin(async {
            Reachability {
                reachable: true,
                status: Some(200),
                error: None,
            }
        })
    }));

    let result = provider.diagnose(DiagnoseRequest::default()).await;
    let checks = result["providerChecks"].as_array().unwrap();
    assert_eq!(checks.len(), 3);
    assert_eq!(checks[0]["id"], "dependency");
    assert_eq!(checks[0]["status"], "pass");
    assert_eq!(checks[0]["detail"], "ngrok version 3.6.0");
    assert_eq!(checks[1]["id"], "authtoken");
    assert_eq!(checks[1]["status"], "pass");
    assert_eq!(checks[1]["detail"], "Valid configuration found");
    assert_eq!(checks[2]["id"], "network");
    assert_eq!(checks[2]["detail"], "HTTP 200");

    let modes = result["modes"].as_array().unwrap();
    assert_eq!(modes.len(), 1);
    assert_eq!(modes[0]["mode"], "quick");
    assert_eq!(modes[0]["ready"], true);
    assert_eq!(modes[0]["blockers"], json!([]));
}

/// 行为契约：依赖缺失时 authtoken 检查失败、detail 为安装指引，模式
/// 未就绪且带 blockers 提示。
#[tokio::test]
async fn provider_diagnose_failure_blockers() {
    let provider =
        NgrokTunnelProvider::new(Arc::new(FakeRunner::unavailable()), reqwest::Client::new())
            .with_api_probe(Arc::new(|| Box::pin(async { Reachability::default() })));

    let result = provider.diagnose(DiagnoseRequest::default()).await;
    let modes = result["modes"].as_array().unwrap();
    assert_eq!(modes[0]["ready"], false);
    assert_eq!(
        modes[0]["blockers"],
        json!(["Resolve provider checks before starting tunnels."])
    );
    let checks = result["providerChecks"].as_array().unwrap();
    assert_eq!(checks[1]["status"], "fail");
    assert_eq!(checks[1]["detail"], ngrok_install_message());
}

/// 行为契约：capabilities JSON 的模式、intent、supports 与稳定性字段
/// 与 JS 版逐一对齐。
#[test]
fn capabilities_json_shape_matches_js() {
    let capabilities = ngrok_capabilities_json();
    assert_eq!(capabilities["provider"], "ngrok");
    assert_eq!(capabilities["defaults"]["mode"], "quick");
    let modes = capabilities["modes"].as_array().unwrap();
    assert_eq!(modes.len(), 1);
    assert_eq!(
        modes[0],
        json!({
            "key": "quick", "label": "Quick Tunnel", "intent": "ephemeral-public",
            "requires": [], "supports": ["sessionTTL"], "stability": "beta"
        })
    );
}
