//! Tests for `cloudflare.rs` (JS precedent: cloudflare-tunnel usage patterns
//! and the provider diagnose shapes from providers/cloudflare.js).

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::tunnels::runner::testutil::FakeRunner;

fn fast_opts() -> CloudflareTunnelOpts {
    CloudflareTunnelOpts {
        quick_timeout_ms: 2_000,
        managed_startup_timeout_ms: 2_000,
        liveness_fallback_ms: 300,
    }
}

#[test]
fn extracts_try_cloudflare_urls() {
    assert_eq!(
        extract_try_cloudflare_url("2024 INF +--+\nhttps://abc-def.trycloudflare.com\nnext"),
        Some("https://abc-def.trycloudflare.com".to_string())
    );
    assert_eq!(
        extract_try_cloudflare_url("see https://xyz.example.com/trycloudflare.com path"),
        None
    );
    assert_eq!(
        extract_try_cloudflare_url("HTTPS://AbC-123.TRYCLOUDFLARE.COM/extra"),
        // JS regex match[0] preserves the matched substring's casing.
        Some("HTTPS://AbC-123.TRYCLOUDFLARE.COM".to_string())
    );
    assert_eq!(extract_try_cloudflare_url("nothing here"), None);
}

#[test]
fn ready_and_fatal_log_patterns() {
    assert!(is_cloudflared_ready_log_line(
        "INF Registered tunnel connection connIndex=0"
    ));
    assert!(is_cloudflared_ready_log_line(
        "INF Connection ... registered"
    ));
    assert!(is_cloudflared_ready_log_line("INF Starting metrics server"));
    assert!(is_cloudflared_ready_log_line("INF Connected to edge"));
    assert!(!is_cloudflared_ready_log_line(""));

    assert!(is_cloudflared_fatal_log_line(
        "ERR Error parsing config from /x: yaml: bad"
    ));
    assert!(is_cloudflared_fatal_log_line("ERR failed to load config"));
    assert!(is_cloudflared_fatal_log_line("ERR Invalid token"));
    assert!(is_cloudflared_fatal_log_line("ERR Unauthorized"));
    assert!(is_cloudflared_fatal_log_line(
        "ERR credentials file /x.json not found"
    ));
    assert!(is_cloudflared_fatal_log_line(
        "ERR Provided tunnel credentials are invalid"
    ));
    assert!(!is_cloudflared_fatal_log_line("INF all good"));
}

#[test]
fn normalizes_cloudflare_hostnames() {
    assert_eq!(
        normalize_cloudflare_tunnel_hostname(Some("Example.COM ")).as_deref(),
        Some("example.com")
    );
    assert_eq!(
        normalize_cloudflare_tunnel_hostname(Some("https://Example.com/path")).as_deref(),
        Some("example.com")
    );
    assert_eq!(normalize_cloudflare_tunnel_hostname(Some("  ")), None);
    assert_eq!(normalize_cloudflare_tunnel_hostname(None), None);
    assert_eq!(
        normalize_cloudflare_tunnel_hostname(Some("not a url")),
        None
    );
}

#[test]
fn yaml_ingress_subset_extracts_hostnames_in_order() {
    let raw = "\
# cloudflared config
tunnel: abc-def
credentials-file: /home/ada/.cloudflared/abc.json
ingress:
  - hostname: first.example.com
    service: http://localhost:3000
  - hostname: https://second.example.com
    service: http://localhost:4000
  - service: http_status:404
warp-routing:
  enabled: false
";
    assert_eq!(
        yaml_ingress_hostnames(raw),
        vec![
            "first.example.com".to_string(),
            "https://second.example.com".to_string()
        ]
    );
}

#[test]
fn json_config_extracts_ingress_hostname() {
    let dir = super::super::managed_config::make_temp_dir("cf-json-cfg").unwrap();
    let path = dir.join("config.json");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "ingress": [
                { "hostname": "json.example.com", "service": "http://localhost:3000" },
                { "service": "http_status:404" }
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let (hostname, error) = extract_hostname_from_cloudflared_config_detailed(&path);
    assert_eq!(error, None);
    assert_eq!(hostname.as_deref(), Some("json.example.com"));
}

#[test]
fn invalid_json_config_reports_parse_error() {
    let dir = super::super::managed_config::make_temp_dir("cf-json-bad").unwrap();
    let path = dir.join("config.json");
    std::fs::write(&path, "{ not json").unwrap();
    let (hostname, error) = extract_hostname_from_cloudflared_config_detailed(&path);
    assert_eq!(hostname, None);
    assert_eq!(
        error.as_deref(),
        Some(
            "Managed local tunnel config is invalid. Use a valid cloudflared YAML/JSON config file."
        )
    );
}

#[test]
fn inspect_managed_local_requires_readable_config() {
    let inspection = inspect_managed_local_cloudflare_config(Some("/nonexistent/config.yml"), None);
    assert!(!inspection.ok);
    assert_eq!(
        inspection.error.as_deref(),
        Some(
            "Managed local tunnel config file was not found. Select a valid cloudflared config file."
        )
    );
}

#[test]
fn inspect_managed_local_reports_missing_hostname() {
    let dir = super::super::managed_config::make_temp_dir("cf-inspect-nohost").unwrap();
    let path = dir.join("config.yml");
    std::fs::write(
        &path,
        "tunnel: abc\ningress:\n  - service: http_status:404\n",
    )
    .unwrap();
    let inspection = inspect_managed_local_cloudflare_config(Some(path.to_str().unwrap()), None);
    assert!(!inspection.ok);
    assert_eq!(
        inspection.error.as_deref(),
        Some(
            "Managed local tunnel hostname is required (set --hostname or include ingress hostname in config)."
        )
    );
}

fn write_config(name: &str, body: &str, extension: &str) -> PathBuf {
    let dir = super::super::managed_config::make_temp_dir(name).unwrap();
    let path = dir.join(format!("config.{extension}"));
    std::fs::write(&path, body).unwrap();
    path
}

#[tokio::test]
async fn quick_tunnel_argv_and_url_extraction() {
    let runner = FakeRunner::new()
        .with_probe_stdout("cloudflared version 2024.1.0")
        .with_stdout_chunk("2024 INF +--+\n")
        .with_stdout_chunk("https://quick-abc.trycloudflare.com\n");
    let controller = start_cloudflare_quick_tunnel(
        "http://127.0.0.1:3000",
        &runner,
        &CloudflareTunnelOpts::default(),
    )
    .await
    .expect("quick tunnel starts");

    assert_eq!(controller.mode, "quick");
    assert_eq!(
        controller.public_url.as_deref(),
        Some("https://quick-abc.trycloudflare.com")
    );

    let spawns = runner.recorded_spawns();
    assert_eq!(spawns.len(), 1);
    let (command, args, env) = &spawns[0];
    assert_eq!(command, "cloudflared");
    assert_eq!(args, &vec!["tunnel", "--url", "http://127.0.0.1:3000"]);
    assert_eq!(
        env.get("CF_TELEMETRY_DISABLE").map(String::as_str),
        Some("1")
    );
    let home = env.get("HOME").expect("HOME override");
    assert!(home.contains("ompchamber-cf-"), "temp HOME dir: {home}");
}

#[tokio::test]
async fn quick_tunnel_without_url_times_out() {
    let runner = FakeRunner::new().with_probe_stdout("cloudflared version 1");
    let error = start_cloudflare_quick_tunnel(
        "http://127.0.0.1:3000",
        &runner,
        &CloudflareTunnelOpts {
            quick_timeout_ms: 150,
            ..CloudflareTunnelOpts::default()
        },
    )
    .await
    .expect_err("no url in output");
    assert_eq!(error, "Tunnel URL not received within 30 seconds");
    assert!(runner.kill_called(), "child killed on timeout");
}

#[tokio::test]
async fn quick_tunnel_requires_cloudflared() {
    let runner = FakeRunner::unavailable();
    let error = start_cloudflare_quick_tunnel(
        "http://127.0.0.1:3000",
        &runner,
        &CloudflareTunnelOpts::default(),
    )
    .await
    .expect_err("unavailable");
    assert_eq!(error, "cloudflared is not installed");
}

#[tokio::test]
async fn managed_remote_argv_token_file_and_url() {
    let runner = FakeRunner::new()
        .with_probe_stdout("cloudflared version 2024.1.0")
        .with_stdout_chunk("2024 INF Registered tunnel connection\n");
    let controller = start_cloudflare_managed_remote_tunnel(
        Some("  secret-token  "),
        Some("Tunnel.Example.COM"),
        None,
        &runner,
        &fast_opts(),
    )
    .await
    .expect("managed remote starts");

    assert_eq!(controller.mode, "managed-remote");
    assert_eq!(
        controller.public_url.as_deref(),
        Some("https://tunnel.example.com")
    );

    let spawns = runner.recorded_spawns();
    let (command, args, env) = &spawns[0];
    assert_eq!(command, "cloudflared");
    assert_eq!(args.len(), 4);
    assert_eq!(&args[..3], &["tunnel", "run", "--token-file"]);
    let token_file = PathBuf::from(&args[3]);
    assert!(
        token_file
            .to_string_lossy()
            .contains("ompchamber-cf-token-")
    );
    assert_eq!(
        std::fs::read_to_string(&token_file).unwrap(),
        "secret-token"
    );
    // JS inherits HOME from process.env; only the quick tunnel overrides it.
    assert_eq!(
        env.get("CF_TELEMETRY_DISABLE").map(String::as_str),
        Some("1")
    );
}

#[tokio::test]
async fn managed_remote_requires_token_and_hostname() {
    let runner = FakeRunner::new().with_probe_stdout("v");
    let error = start_cloudflare_managed_remote_tunnel(
        None,
        Some("h.example.com"),
        None,
        &runner,
        &fast_opts(),
    )
    .await
    .expect_err("token required");
    assert_eq!(error, "Managed remote tunnel token is required");

    let error =
        start_cloudflare_managed_remote_tunnel(Some("tok"), None, None, &runner, &fast_opts())
            .await
            .expect_err("hostname required");
    assert_eq!(error, "Managed remote tunnel hostname is required");
}

#[tokio::test]
async fn managed_tunnel_fatal_log_fails_start() {
    let runner = FakeRunner::new()
        .with_probe_stdout("cloudflared version 2024.1.0")
        .with_stderr_chunk("2024 ERR Invalid token provided\n");
    let error = start_cloudflare_managed_remote_tunnel(
        Some("tok"),
        Some("h.example.com"),
        None,
        &runner,
        &fast_opts(),
    )
    .await
    .expect_err("fatal log");
    assert_eq!(
        error,
        "Cloudflared failed to start managed-remote tunnel: 2024 ERR Invalid token provided"
    );
}

#[tokio::test]
async fn managed_tunnel_hard_timeout_without_output() {
    let runner = FakeRunner::new().with_probe_stdout("cloudflared version 2024.1.0");
    let error = start_cloudflare_managed_remote_tunnel(
        Some("tok"),
        Some("h.example.com"),
        None,
        &runner,
        &CloudflareTunnelOpts {
            quick_timeout_ms: 100,
            managed_startup_timeout_ms: 120,
            liveness_fallback_ms: 60,
        },
    )
    .await
    .expect_err("times out");
    assert_eq!(
        error,
        "Timed out waiting for cloudflared to initialize managed-remote tunnel. Check your tunnel config and credentials."
    );
}

#[tokio::test]
async fn managed_tunnel_liveness_fallback_accepts_output() {
    // Output that never matches a ready pattern: the liveness fallback
    // still resolves success once the process has produced output.
    let runner = FakeRunner::new()
        .with_probe_stdout("cloudflared version 2024.1.0")
        .with_stdout_chunk("2024 INF starting up\n");
    let controller = start_cloudflare_managed_remote_tunnel(
        Some("tok"),
        Some("h.example.com"),
        None,
        &runner,
        &CloudflareTunnelOpts {
            quick_timeout_ms: 100,
            managed_startup_timeout_ms: 60_000,
            liveness_fallback_ms: 80,
        },
    )
    .await
    .expect("liveness fallback");
    assert_eq!(controller.mode, "managed-remote");
}

#[tokio::test]
async fn managed_local_argv_with_config_and_hostname_from_config() {
    let config = write_config(
        "cf-local-yml",
        "tunnel: abc\ningress:\n  - hostname: from-config.example.com\n    service: http://localhost:3000\n",
        "yml",
    );
    let runner = FakeRunner::new()
        .with_probe_stdout("cloudflared version 2024.1.0")
        .with_stdout_chunk("2024 INF Registered tunnel connection\n");

    let controller = start_cloudflare_managed_local_tunnel(
        Some(config.to_str().unwrap()),
        None,
        &runner,
        &fast_opts(),
    )
    .await
    .expect("managed local starts");

    assert_eq!(controller.mode, "managed-local");
    assert_eq!(
        controller.public_url.as_deref(),
        Some("https://from-config.example.com")
    );
    assert_eq!(
        controller.effective_config_path.as_deref(),
        Some(config.to_str().unwrap())
    );
    assert_eq!(
        controller.resolved_hostname.as_deref(),
        Some("from-config.example.com")
    );

    let spawns = runner.recorded_spawns();
    let (_command, args, _env) = &spawns[0];
    assert_eq!(
        args,
        &vec![
            "tunnel".to_string(),
            "--config".to_string(),
            config.to_str().unwrap().to_string(),
            "run".to_string()
        ]
    );
}

#[tokio::test]
async fn managed_local_hostname_request_beats_config() {
    let config = write_config(
        "cf-local-json",
        &serde_json::to_string(&json!({
            "ingress": [{ "hostname": "config.example.com", "service": "http://localhost:1" }]
        }))
        .unwrap(),
        "json",
    );
    let runner = FakeRunner::new()
        .with_probe_stdout("cloudflared version 2024.1.0")
        .with_stdout_chunk("2024 INF Registered tunnel connection\n");

    let controller = start_cloudflare_managed_local_tunnel(
        Some(config.to_str().unwrap()),
        Some("override.example.com"),
        &runner,
        &fast_opts(),
    )
    .await
    .expect("managed local starts");
    assert_eq!(
        controller.public_url.as_deref(),
        Some("https://override.example.com")
    );
}

#[tokio::test]
async fn managed_local_rejects_unreadable_config() {
    let runner = FakeRunner::new().with_probe_stdout("cloudflared version 1");
    let error = start_cloudflare_managed_local_tunnel(
        Some("/nonexistent/config.yml"),
        None,
        &runner,
        &fast_opts(),
    )
    .await
    .expect_err("unreadable");
    assert_eq!(
        error,
        "Managed local tunnel config file was not found. Select a valid cloudflared config file."
    );
}

#[tokio::test]
async fn provider_start_requires_origin_url_for_quick_mode() {
    let provider = CloudflareTunnelProvider::new(
        Arc::new(FakeRunner::new().with_probe_stdout("v")),
        reqwest::Client::new(),
    );
    let request = TunnelStartRequest {
        provider: "cloudflare".to_string(),
        mode: "quick".to_string(),
        intent: None,
        config_path: None,
        token: String::new(),
        hostname: String::new(),
    };
    let failure = provider
        .start(request, StartContext::default())
        .await
        .expect_err("origin url required");
    match failure {
        StartFailure::Service(error) => {
            assert_eq!(error.code, "validation_error");
            assert_eq!(error.message, "originUrl is required for quick tunnel mode");
        }
        other => panic!("expected service error, got {other:?}"),
    }
}

#[tokio::test]
async fn provider_diagnose_reports_checks_and_modes() {
    let provider = CloudflareTunnelProvider::new(
        Arc::new(FakeRunner::new().with_probe_stdout("cloudflared 1.2.3")),
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

    let result = provider
        .diagnose(DiagnoseRequest {
            mode: None,
            hostname: Some("tunnel.example.com".to_string()),
            token: Some("valid-token".to_string()),
            token_provided: false,
            hostname_provided: false,
            config_path: None,
            has_saved_managed_remote_profile: false,
        })
        .await;

    let checks = result["providerChecks"].as_array().unwrap();
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0]["id"], "dependency");
    assert_eq!(checks[0]["status"], "pass");
    assert_eq!(checks[0]["detail"], "cloudflared 1.2.3");
    assert_eq!(checks[1]["id"], "network");
    assert_eq!(checks[1]["detail"], "HTTP 200");

    let modes = result["modes"].as_array().unwrap();
    assert_eq!(modes.len(), 3);
    assert_eq!(modes[0]["mode"], "quick");
    assert_eq!(modes[1]["mode"], "managed-remote");
    assert_eq!(modes[2]["mode"], "managed-local");
    assert_eq!(modes[1]["checks"][1]["status"], "pass");
    assert_eq!(modes[1]["checks"][1]["detail"], "tunnel.example.com");
    assert_eq!(modes[1]["checks"][2]["status"], "pass");
}

#[tokio::test]
async fn provider_diagnose_saved_profile_fallbacks() {
    let provider = CloudflareTunnelProvider::new(
        Arc::new(FakeRunner::new().with_probe_stdout("cloudflared 1.2.3")),
        reqwest::Client::new(),
    )
    .with_api_probe(Arc::new(|| Box::pin(async { Reachability::default() })));

    let result = provider
        .diagnose(DiagnoseRequest {
            mode: Some("managed-remote".to_string()),
            hostname: None,
            token: None,
            token_provided: false,
            hostname_provided: false,
            config_path: None,
            has_saved_managed_remote_profile: true,
        })
        .await;

    let modes = result["modes"].as_array().unwrap();
    assert_eq!(modes.len(), 1);
    let checks = modes[0]["checks"].as_array().unwrap();
    assert_eq!(checks[1]["status"], "pass");
    assert_eq!(checks[1]["detail"], "at least one saved profile present");
    assert_eq!(checks[2]["status"], "pass");
}

#[test]
fn capabilities_json_shape_matches_js() {
    let capabilities = cloudflare_capabilities_json();
    assert_eq!(capabilities["provider"], "cloudflare");
    assert_eq!(capabilities["defaults"]["mode"], "quick");
    assert_eq!(capabilities["defaults"]["optionDefaults"], json!({}));
    let modes = capabilities["modes"].as_array().unwrap();
    assert_eq!(modes.len(), 3);
    assert_eq!(
        modes[0],
        json!({
            "key": "quick", "label": "Quick Tunnel", "intent": "ephemeral-public",
            "requires": [], "supports": ["sessionTTL"], "stability": "ga"
        })
    );
    assert_eq!(modes[1]["requires"], json!(["token", "hostname"]));
    assert_eq!(
        modes[2]["supports"],
        json!(["configFile", "customDomain", "sessionTTL"])
    );
}
