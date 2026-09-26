//! Route tests for `routes.rs` (JS precedent: routes.js handler shapes and
//! the dev-tunnel runtime contract from dev-tunnel/tunnel.test.js).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::ws::Message;
use axum::http::{Request, StatusCode, header};
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::super::dev_tunnel::{
    DevTunnelClient, DevTunnelState, DiscoverOutcome, is_dev_tunnel_path,
};
use super::super::registry::{
    AvailabilityInfo, DiagnoseRequest, StartContext, StartFailure, TunnelController, TunnelProvider,
};
use super::super::service::TunnelService;
use super::super::types::{
    ModeDescriptor, TUNNEL_INTENT_EPHEMERAL_PUBLIC, TUNNEL_INTENT_PERSISTENT_PUBLIC,
    TUNNEL_MODE_MANAGED_LOCAL, TUNNEL_MODE_MANAGED_REMOTE, TUNNEL_MODE_QUICK,
    TUNNEL_PROVIDER_CLOUDFLARE, TUNNEL_PROVIDER_NGROK, TunnelStartRequest,
};
use super::*;

// ---------------------------------------------------------------------------
// Fake provider
// ---------------------------------------------------------------------------

struct FakeProvider {
    id: &'static str,
    available: bool,
    modes: &'static [ModeDescriptor],
    url: Option<String>,
    start_error: Option<String>,
    started: AtomicUsize,
    stopped: AtomicUsize,
    metadata: Value,
}

impl FakeProvider {
    fn cloudflare() -> Arc<Self> {
        static MODES: [ModeDescriptor; 3] = [
            ModeDescriptor {
                key: TUNNEL_MODE_QUICK,
                label: "Quick Tunnel",
                intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC,
                requires: &[],
                supports: &["sessionTTL"],
                stability: "ga",
            },
            ModeDescriptor {
                key: TUNNEL_MODE_MANAGED_REMOTE,
                label: "Managed Remote Tunnel",
                intent: TUNNEL_INTENT_PERSISTENT_PUBLIC,
                requires: &["token", "hostname"],
                supports: &["customDomain", "sessionTTL"],
                stability: "ga",
            },
            ModeDescriptor {
                key: TUNNEL_MODE_MANAGED_LOCAL,
                label: "Managed Local Tunnel",
                intent: TUNNEL_INTENT_PERSISTENT_PUBLIC,
                requires: &[],
                supports: &["configFile", "customDomain", "sessionTTL"],
                stability: "ga",
            },
        ];
        Arc::new(Self {
            id: TUNNEL_PROVIDER_CLOUDFLARE,
            available: true,
            modes: &MODES,
            url: Some("https://fake.trycloudflare.com".to_string()),
            start_error: None,
            started: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
            metadata: json!({ "configPath": null, "resolvedHostname": "fake.example.com" }),
        })
    }

    fn ngrok() -> Arc<Self> {
        static MODES: [ModeDescriptor; 1] = [ModeDescriptor {
            key: TUNNEL_MODE_QUICK,
            label: "Quick Tunnel",
            intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC,
            requires: &[],
            supports: &["sessionTTL"],
            stability: "beta",
        }];
        Arc::new(Self {
            id: TUNNEL_PROVIDER_NGROK,
            available: true,
            modes: &MODES,
            url: Some("https://fake.ngrok-free.app".to_string()),
            start_error: None,
            started: AtomicUsize::new(0),
            stopped: AtomicUsize::new(0),
            metadata: Value::Null,
        })
    }
}

impl TunnelProvider for FakeProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    fn capabilities_json(&self) -> Value {
        json!({ "provider": self.id, "modes": self.modes.iter().map(|mode| mode.to_json()).collect::<Vec<_>>() })
    }

    fn mode_descriptors(&self) -> &'static [ModeDescriptor] {
        self.modes
    }

    fn check_availability(&self) -> BoxFuture<'static, AvailabilityInfo> {
        let available = self.available;
        let id = self.id;
        Box::pin(async move {
            AvailabilityInfo {
                available,
                version: if available {
                    Some(format!("{id} version 1.0"))
                } else {
                    None
                },
                dependency: id.to_string(),
                install_command: format!("brew install {id}"),
                install_url: format!("https://{id}.example/download"),
                platform: "darwin".to_string(),
                message: format!("{id} is not installed. Install it with: brew install {id}"),
            }
        })
    }

    fn diagnose(&self, _request: DiagnoseRequest) -> BoxFuture<'static, Value> {
        Box::pin(async {
            json!({
                "providerChecks": [{
                    "id": "dependency",
                    "label": "installed",
                    "status": "pass",
                    "detail": "ok"
                }],
                "modes": [
                    { "mode": "quick", "checks": [], "summary": {"ready": true, "failures": 0, "warnings": 0}, "ready": true, "blockers": [] },
                    { "mode": "managed-remote", "checks": [], "summary": {"ready": true, "failures": 0, "warnings": 0}, "ready": true, "blockers": [] },
                    { "mode": "managed-local", "checks": [], "summary": {"ready": true, "failures": 0, "warnings": 0}, "ready": true, "blockers": [] },
                ],
            })
        })
    }

    fn start(
        &self,
        request: TunnelStartRequest,
        _context: StartContext,
    ) -> BoxFuture<'static, Result<TunnelController, StartFailure>> {
        self.started.fetch_add(1, Ordering::SeqCst);
        if let Some(message) = &self.start_error {
            let message = message.clone();
            return Box::pin(async move { Err(StartFailure::Raw(message)) });
        }
        let url = self.url.clone().unwrap_or_default();
        let effective_config_path = request.config_path.clone();
        Box::pin(async move {
            Ok(TunnelController {
                provider: None,
                mode: request.mode,
                public_url: Some(url),
                stop: None,
                effective_config_path,
                resolved_hostname: Some("fake.example.com".to_string()),
            })
        })
    }

    fn stop(&self, _controller: &TunnelController) {
        self.stopped.fetch_add(1, Ordering::SeqCst);
    }

    fn get_metadata(&self, _controller: Option<&TunnelController>) -> Value {
        self.metadata.clone()
    }
}

// ---------------------------------------------------------------------------
// State builder
// ---------------------------------------------------------------------------

fn test_context() -> RouterContext {
    let data_dir = std::env::temp_dir().join(format!(
        "tunnels-routes-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let config = crate::config::ServerConfig {
        port: 0,
        host: None,
        lan: false,
        ui_password: None,
        api_only: false,
        data_dir: data_dir.clone(),
        dist_dir: data_dir.join("dist"),
        tunnel: Default::default(),
        engine: crate::config::EngineConfig::Managed {
            hostname: "127.0.0.1".to_string(),
        },
    };
    RouterContext {
        config: Arc::new(config),
        engine: crate::engine::EngineState::external("http://127.0.0.1:1".to_string(), None),
        hub: crate::hub::EventHub::new(),
    }
}

fn build_state(providers: Vec<Arc<dyn TunnelProvider>>, allowed_ports: Vec<u16>) -> ModuleState {
    let discover = discover_for(allowed_ports);
    build_state_full(providers, Vec::new(), false, discover)
}

fn build_state_full(
    providers: Vec<Arc<dyn TunnelProvider>>,
    allowed_ports: Vec<u16>,
    auth_enabled: bool,
    discover: DiscoverFn,
) -> ModuleState {
    let ctx = test_context();
    let mut registry = super::super::registry::TunnelProviderRegistry::new();
    for provider in providers {
        registry.register(provider).expect("register provider");
    }
    let registry = Arc::new(registry);
    let controller: Arc<Mutex<Option<TunnelController>>> = Arc::new(Mutex::new(None));
    let service = Arc::new(TunnelService::new(
        registry.clone(),
        controller.clone(),
        Arc::new(|| Some(3000)),
        None,
    ));
    let dev = DevTunnelState {
        open_sockets: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        discover,
        auth_enabled,
        resolve_auth: Arc::new(|_parts| {
            // The default resolver authenticates sessions only (see
            // module_state); tests override per-case.
            Some(super::super::dev_tunnel::AuthKind::Session)
        }),
    };
    let _ = allowed_ports;
    let settings = crate::settings::store_for_path(&ctx.config.data_dir.join("settings.json"));
    let auth = crate::client_auth::state_for_data_dir(&ctx.config.data_dir)
        .tunnel_auth
        .clone();
    ModuleState {
        settings,
        registry,
        service,
        auth,
        managed_config: Arc::new(
            super::super::managed_config::ManagedTunnelConfigRuntime::new(
                std::env::temp_dir().join(format!(
                    "tunnels-routes-managed-{}-{}.json",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )),
                std::env::temp_dir().join("tunnels-routes-legacy-missing.json"),
            ),
        ),
        controller,
        runtime_hostname: Arc::new(Mutex::new(String::new())),
        runtime_token: Arc::new(Mutex::new(String::new())),
        active_port: Arc::new(std::sync::atomic::AtomicU16::new(3000)),
        dev,
    }
}

fn discover_for(ports: Vec<u16>) -> DiscoverFn {
    Arc::new(move || {
        let ports = ports.clone();
        Box::pin(async move { DiscoverOutcome::Available(ports.clone()) })
    })
}

fn request_json(method: &str, uri: &str, body: Option<&Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(body) = body {
        let serialized = serde_json::to_string(body).unwrap();
        builder = builder
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, serialized.len());
        builder.body(Body::from(serialized)).unwrap()
    } else {
        builder.body(Body::empty()).unwrap()
    }
}

async fn send(router: &axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn providers_lists_capabilities() {
    let state = build_state(
        vec![FakeProvider::cloudflare(), FakeProvider::ngrok()],
        vec![],
    );
    let router = router_with(state);
    let (status, body) = send(
        &router,
        request_json("GET", "/api/ompchamber/tunnel/providers", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let providers = body["providers"].as_array().unwrap();
    assert_eq!(providers.len(), 2);
    assert_eq!(providers[0]["provider"], "cloudflare");
    assert_eq!(providers[1]["provider"], "ngrok");
}

#[tokio::test]
async fn check_reports_availability_and_install_info() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state);
    let (status, body) = send(
        &router,
        request_json(
            "GET",
            "/api/ompchamber/tunnel/check?provider=cloudflare",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], true);
    assert_eq!(body["provider"], "cloudflare");
    assert_eq!(body["version"], "cloudflare version 1.0");
    assert_eq!(body["dependency"], "cloudflare");
    assert_eq!(body["installCommand"], "brew install cloudflare");
    assert_eq!(body["platform"], "darwin");
    assert!(body["installUrl"].is_string());
}

#[tokio::test]
async fn check_unknown_provider_returns_null_shape() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state);
    // No providers registered as 'ngrok': check_availability throws
    // provider_unsupported → JS catch → all-null JSON with 200.
    let (status, body) = send(
        &router,
        request_json("GET", "/api/ompchamber/tunnel/check?provider=ngrok", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], false);
    assert_eq!(body["provider"], Value::Null);
    assert_eq!(body["version"], Value::Null);
    assert_eq!(body["message"], Value::Null);
    assert!(body["platform"].is_string());
}

#[tokio::test]
async fn doctor_returns_provider_checks_and_modes() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state);
    let (status, body) = send(
        &router,
        request_json(
            "GET",
            "/api/ompchamber/tunnel/doctor?provider=cloudflare",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["provider"], "cloudflare");
    assert_eq!(body["providerChecks"][0]["id"], "dependency");
    assert_eq!(body["modes"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn doctor_filters_modes_and_rejects_unknown_modes() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state);

    let (status, body) = send(
        &router,
        request_json(
            "GET",
            "/api/ompchamber/tunnel/doctor?provider=cloudflare&mode=quick",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["modes"].as_array().unwrap().len(), 1);
    assert_eq!(body["modes"][0]["mode"], "quick");

    let (status, body) = send(
        &router,
        request_json(
            "GET",
            "/api/ompchamber/tunnel/doctor?provider=cloudflare&mode=bogus",
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["ok"], false);
    assert_eq!(body["code"], "mode_unsupported");
    assert_eq!(
        body["error"],
        "Provider 'cloudflare' does not support mode 'bogus'"
    );
}

#[tokio::test]
async fn doctor_post_uses_body_token_flags() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state);
    let (status, _body) = send(
        &router,
        request_json(
            "POST",
            "/api/ompchamber/tunnel/doctor?provider=cloudflare",
            Some(&json!({ "tokenProvided": true })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn status_inactive_shape() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state);
    let (status, body) = send(
        &router,
        request_json("GET", "/api/ompchamber/tunnel/status", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], false);
    assert_eq!(body["url"], Value::Null);
    assert_eq!(body["mode"], "quick");
    assert_eq!(body["provider"], "cloudflare");
    assert_eq!(body["policy"], "tunnel-gated");
    assert_eq!(body["hasBootstrapToken"], false);
    assert_eq!(body["bootstrapExpiresAt"], Value::Null);
    assert_eq!(body["managedRemoteTunnelPresets"], json!([]));
    assert_eq!(body["managedRemoteTunnelTokenPresetIds"], json!([]));
    assert_eq!(body["localPort"], 3000);
    assert_eq!(body["ttlConfig"]["bootstrapTtlMs"], 1_800_000);
    assert_eq!(body["ttlConfig"]["sessionTtlMs"], 28_800_000);
    assert_eq!(body["activeSessions"], json!([]));
}

#[tokio::test]
async fn status_active_syncs_tunnel_auth_state() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    // Pre-activate a tunnel through the service so status sees it.
    state
        .service
        .start(json!({ "provider": "cloudflare", "mode": "quick" }))
        .await
        .expect("start");

    let router = router_with(state.clone());
    let (status, body) = send(
        &router,
        request_json("GET", "/api/ompchamber/tunnel/status", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["active"], true);
    assert_eq!(body["url"], "https://fake.trycloudflare.com");
    assert_eq!(body["mode"], "quick");
    assert_eq!(body["activeTunnelMode"], "quick");
    // The status route syncs the auth controller on the fly.
    let host = state.auth.active_tunnel_host();
    assert_eq!(host.as_deref(), Some("fake.trycloudflare.com"));
    assert!(state.auth.active_tunnel_id().is_some());
}

#[tokio::test]
async fn start_returns_full_response_with_connect_url() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state.clone());

    let (status, body) = send(
        &router,
        request_json(
            "POST",
            "/api/ompchamber/tunnel/start",
            Some(&json!({ "provider": "cloudflare", "mode": "quick" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["url"], "https://fake.trycloudflare.com");
    assert_eq!(body["mode"], "quick");
    assert_eq!(body["provider"], "cloudflare");
    assert_eq!(body["replacedTunnel"], false);
    assert_eq!(body["replaced"], Value::Null);
    assert_eq!(body["revokedBootstrapCount"], 0);
    assert_eq!(body["invalidatedSessionCount"], 0);
    assert_eq!(body["policy"], "tunnel-gated");
    assert_eq!(body["localPort"], 3000);
    assert_eq!(body["managedRemoteTunnelHostname"], Value::Null);
    assert_eq!(body["managedRemoteTunnelTokenPresetIds"], json!([]));

    let connect_url = body["connectUrl"].as_str().unwrap();
    assert!(
        connect_url.starts_with("https://fake.trycloudflare.com/connect?t="),
        "{connect_url}"
    );
    let token = connect_url.rsplit("t=").next().unwrap();
    assert!(!token.is_empty());

    // A bootstrap token is live afterwards.
    assert!(state.auth.bootstrap_status().has_bootstrap_token);
}

#[tokio::test]
async fn start_replaces_tunnel_and_revokes_connect_links() {
    let state = build_state(
        vec![FakeProvider::cloudflare(), FakeProvider::ngrok()],
        vec![],
    );
    let router = router_with(state.clone());

    let (status, first) = send(
        &router,
        request_json(
            "POST",
            "/api/ompchamber/tunnel/start",
            Some(&json!({ "provider": "cloudflare", "mode": "quick" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let _first_tunnel_id = state.auth.active_tunnel_id().unwrap();

    // A tunnel session from the first tunnel must be invalidated on replace —
    // seeded through the real bootstrap exchange flow.
    let issued = state
        .auth
        .issue_bootstrap_token(Some(60_000))
        .expect("issue bootstrap");
    let headers = axum::http::HeaderMap::new();
    let connect_ctx = crate::client_auth::tunnel_auth::TunnelRequestContext::from_headers(&headers);
    let exchanged = state
        .auth
        .exchange_bootstrap_token(&connect_ctx, Some(&issued.token), 60_000);
    assert!(exchanged.ok, "exchange failed: {:?}", exchanged.reason);

    let (status, second) = send(
        &router,
        request_json(
            "POST",
            "/api/ompchamber/tunnel/start",
            Some(&json!({ "provider": "ngrok", "mode": "quick" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(second["replacedTunnel"], true);
    assert_eq!(
        second["replaced"],
        json!({
            "mode": "quick",
            "provider": "cloudflare",
            "url": "https://fake.trycloudflare.com",
        })
    );
    assert_eq!(second["revokedBootstrapCount"], 1);
    assert_eq!(second["invalidatedSessionCount"], 1);
    assert_ne!(second["url"], first["url"]);

    let sessions = state.auth.list_tunnel_sessions();
    assert_eq!(sessions.len(), 1);
    let session = &sessions[0];
    assert_eq!(session.status, "inactive");
    assert_eq!(session.inactive_reason.as_deref(), Some("tunnel-revoked"));
}

#[tokio::test]
async fn start_rejects_unknown_provider_and_mode() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state);

    let (status, body) = send(
        &router,
        request_json(
            "POST",
            "/api/ompchamber/tunnel/start",
            Some(&json!({ "provider": "ngrok" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["ok"], false);
    assert_eq!(body["code"], "provider_unsupported");
    assert_eq!(body["error"], "Unsupported tunnel provider: ngrok");

    let (status, body) = send(
        &router,
        request_json(
            "POST",
            "/api/ompchamber/tunnel/start",
            Some(&json!({ "provider": "cloudflare", "mode": "wat" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "mode_unsupported");
    assert_eq!(body["error"], "Unsupported tunnel mode: wat");
}

#[tokio::test]
async fn start_requires_managed_remote_fields() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state);
    let (status, body) = send(
        &router,
        request_json(
            "POST",
            "/api/ompchamber/tunnel/start",
            Some(&json!({ "provider": "cloudflare", "mode": "managed-remote" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "validation_error");
    assert_eq!(body["error"], "Managed remote tunnel token is required");
}

#[tokio::test]
async fn start_managed_remote_persists_token_and_reports_hostname() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state.clone());
    let (status, body) = send(
        &router,
        request_json(
            "POST",
            "/api/ompchamber/tunnel/start",
            Some(&json!({
                "provider": "cloudflare",
                "mode": "managed-remote",
                "hostname": "tunnel.example.com",
                "token": "tok-1",
                "managedRemoteTunnelPresetId": "preset-1",
                "managedRemoteTunnelPresetName": "My Tunnel",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["managedRemoteTunnelHostname"], "tunnel.example.com");
    assert_eq!(
        body["managedRemoteTunnelTokenPresetIds"],
        json!(["preset-1"])
    );

    // The token resolves by preset id and hostname afterwards.
    assert_eq!(
        state
            .managed_config
            .resolve_managed_remote_tunnel_token("preset-1", "")
            .await,
        "tok-1"
    );
    assert_eq!(
        state
            .managed_config
            .resolve_managed_remote_tunnel_token("", "tunnel.example.com")
            .await,
        "tok-1"
    );
}

#[tokio::test]
async fn stop_reports_revocations_and_clears_state() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state.clone());
    send(
        &router,
        request_json(
            "POST",
            "/api/ompchamber/tunnel/start",
            Some(&json!({ "provider": "cloudflare", "mode": "quick" })),
        ),
    )
    .await;

    let (status, body) = send(
        &router,
        request_json("POST", "/api/ompchamber/tunnel/stop", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["revokedBootstrapCount"], 1);
    assert_eq!(body["invalidatedSessionCount"], 0);

    assert_eq!(state.auth.active_tunnel_id(), None);
    assert_eq!(state.service.get_public_url(), None);

    // Second stop: nothing to revoke.
    let (status, body) = send(
        &router,
        request_json("POST", "/api/ompchamber/tunnel/stop", None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["revokedBootstrapCount"], 0);
}

#[tokio::test]
async fn managed_remote_token_put_validates_and_persists() {
    let state = build_state(vec![FakeProvider::cloudflare()], vec![]);
    let router = router_with(state.clone());

    let (status, body) = send(
        &router,
        request_json(
            "PUT",
            "/api/ompchamber/tunnel/managed-remote-token",
            Some(&json!({ "presetId": "only-id" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["ok"], false);
    assert_eq!(
        body["error"],
        "presetId, presetName, managedRemoteTunnelHostname and managedRemoteTunnelToken are required"
    );

    let (status, body) = send(
        &router,
        request_json(
            "PUT",
            "/api/ompchamber/tunnel/managed-remote-token",
            Some(&json!({
                "presetId": "preset-9",
                "presetName": "Nine",
                "managedRemoteTunnelHostname": "nine.example.com",
                "managedRemoteTunnelToken": "tok-9",
            })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(
        body["managedRemoteTunnelTokenPresetIds"],
        json!(["preset-9"])
    );
}

// ---------------------------------------------------------------------------
// Dev tunnel
// ---------------------------------------------------------------------------

fn parts_for(uri: &str, headers: &[(&str, &str)]) -> axum::http::request::Parts {
    let mut builder = Request::builder().uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(()).unwrap();
    request.into_parts().0
}

#[tokio::test]
async fn dev_tunnel_path_matching() {
    assert!(is_dev_tunnel_path("/api/dev-tunnel?port=5173"));
    assert!(!is_dev_tunnel_path("/api/terminal/ws"));
    assert!(!is_dev_tunnel_path(""));
}

#[tokio::test]
async fn dev_tunnel_preflight_rejects_invalid_and_disallowed_ports() {
    let state = build_state(vec![], vec![5173]);
    let dev = &state.dev;

    let result = dev_tunnel_preflight(dev, &parts_for("/api/dev-tunnel?port=0", &[])).await;
    assert!(result.is_err());
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let result = dev_tunnel_preflight(dev, &parts_for("/api/dev-tunnel?port=9999", &[])).await;
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // A port discovery reports is allowed.
    let result = dev_tunnel_preflight(dev, &parts_for("/api/dev-tunnel?port=5173", &[])).await;
    assert_eq!(result.ok(), Some(5173));

    // Missing port param.
    let result = dev_tunnel_preflight(dev, &parts_for("/api/dev-tunnel", &[])).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn dev_tunnel_preflight_enforces_concurrency_cap() {
    let state = build_state(vec![], vec![5173]);
    state.dev.open_sockets.store(64, Ordering::SeqCst);
    let result =
        dev_tunnel_preflight(&state.dev, &parts_for("/api/dev-tunnel?port=5173", &[])).await;
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn dev_tunnel_preflight_requires_client_auth_without_origin() {
    let discover: DiscoverFn =
        Arc::new(|| Box::pin(async { DiscoverOutcome::Available(vec![5173]) }));
    let state = build_state_full(vec![], vec![], true, discover);

    // Session auth with no origin: rejected (ambient credentials are what the
    // origin check protects).
    let result =
        dev_tunnel_preflight(&state.dev, &parts_for("/api/dev-tunnel?port=5173", &[])).await;
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    // Unauthenticated: 401.
    let mut unauthenticated = state.dev.clone();
    unauthenticated.resolve_auth = Arc::new(|_parts| None);
    let result = dev_tunnel_preflight(
        &unauthenticated,
        &parts_for("/api/dev-tunnel?port=5173", &[]),
    )
    .await;
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn dev_tunnel_preflight_checks_origin_for_browsers() {
    let discover: DiscoverFn =
        Arc::new(|| Box::pin(async { DiscoverOutcome::Available(vec![5173]) }));
    let state = build_state_full(vec![], vec![], true, discover);

    // An origin header present + session auth: origin allowlist applies.
    // 'http://evil.example' is not an allowed origin for a localhost host.
    let result = dev_tunnel_preflight(
        &state.dev,
        &parts_for(
            "/api/dev-tunnel?port=5173",
            &[("origin", "http://evil.example")],
        ),
    )
    .await;
    let response = result.unwrap_err();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn dev_tunnel_end_to_end_pipes_bytes_unmodified() {
    // JS tunnel.test.js 'serves the dev server through a local port,
    // unmodified': a TCP dev server, the host runtime (axum), and the client.
    let dev_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let dev_port = dev_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = dev_listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0u8; 4096];
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            // Echo with a prefix so we can see the bytes flow.
                            let mut reply = b"echo:".to_vec();
                            reply.extend_from_slice(&buffer[..n]);
                            if socket.write_all(&reply).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    let discover: DiscoverFn =
        Arc::new(move || Box::pin(async move { DiscoverOutcome::Available(vec![dev_port]) }));
    let state = build_state_full(vec![], vec![], false, discover);
    let router = router_with(state);

    let host_listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let host_port = host_listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(host_listener, router).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client = DevTunnelClient::new();
    let opened = client
        .open(&format!("http://127.0.0.1:{host_port}"), dev_port, vec![])
        .await
        .expect("open tunnel");
    assert!(!opened.reused);

    // Repeat open reuses the listener (JS 'reuses one listener for repeat
    // opens of the same target').
    let second = client
        .open(&format!("http://127.0.0.1:{host_port}"), dev_port, vec![])
        .await
        .expect("reuse tunnel");
    assert_eq!(second.local_port, opened.local_port);
    assert!(second.reused);

    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", opened.local_port))
        .await
        .unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    socket.write_all(b"hello").await.unwrap();
    let mut buffer = [0u8; 64];
    let read = socket.read(&mut buffer).await.unwrap();
    assert_eq!(&buffer[..read], b"echo:hello");

    // Close frees the tunnel entry.
    assert!(client.close(&format!("http://127.0.0.1:{host_port}"), dev_port));
    assert!(client.list().is_empty());
}

#[tokio::test]
async fn dev_tunnel_client_rejects_bad_input() {
    let client = DevTunnelClient::new();
    // JS: 'rejects an invalid remote port before binding anything'.
    let error = client
        .open("http://127.0.0.1:1", 0, vec![])
        .await
        .expect_err("port");
    assert_eq!(error, "A valid remote port is required");
    let error = client.open("", 5173, vec![]).await.expect_err("base url");
    assert_eq!(error, "A remote base URL is required");
    // JS: 'rejects a non-http(s) base URL instead of crashing'.
    let error = client
        .open("ompchamber-ui://index", 5173, vec![])
        .await
        .expect_err("scheme");
    assert_eq!(
        error,
        "The remote base URL must be http(s); got \"ompchamber-ui\""
    );
    assert!(client.list().is_empty());
}

#[tokio::test]
async fn dev_tunnel_client_rejects_wss_with_explicit_message() {
    let client = DevTunnelClient::new();
    let error = client
        .open("https://remote.example.com", 5173, vec![])
        .await
        .expect_err("wss unsupported");
    assert_eq!(error, super::super::dev_tunnel::WSS_UNSUPPORTED_MESSAGE);
    assert!(client.list().is_empty());
}

#[tokio::test]
async fn dev_tunnel_client_protocol_frames_against_fake_ws_server() {
    // A hand-rolled WS server: completes the handshake (valid accept key) and
    // echoes binary frames, verifying the client codec both ways.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let server_port = listener.local_addr().unwrap().port();

    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 2048];
        let mut head = Vec::new();
        // Read the handshake request head.
        loop {
            let n = socket.read(&mut buffer).await.unwrap();
            head.extend_from_slice(&buffer[..n]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let mut key = String::new();
        for line in head.lines() {
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("sec-websocket-key") {
                    key = value.trim().to_string();
                }
            }
        }
        let accept = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(sha1_public(
                format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
            ))
        };
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        );
        socket.write_all(response.as_bytes()).await.unwrap();

        // Echo binary frames back unmasked.
        loop {
            let Some(frame) = read_frame_raw(&mut socket).await else {
                return;
            };
            if frame.0 != 0x2 {
                return;
            }
            let mut echo = vec![0x82];
            let len = frame.1.len();
            if len < 126 {
                echo.push(len as u8);
            } else {
                echo.push(126);
                echo.extend_from_slice(&(len as u16).to_be_bytes());
            }
            echo.extend_from_slice(&frame.1);
            socket.write_all(&echo).await.unwrap();
        }
    });

    let client = DevTunnelClient::new();
    let opened = client
        .open(&format!("http://127.0.0.1:{server_port}"), 4321, vec![])
        .await
        .expect("open");
    let mut local = tokio::net::TcpStream::connect(("127.0.0.1", opened.local_port))
        .await
        .unwrap();
    local.write_all(b"ping-through-ws").await.unwrap();
    let mut buffer = [0u8; 64];
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), local.read(&mut buffer))
        .await
        .expect("echo within timeout")
        .unwrap();
    assert_eq!(&buffer[..read], b"ping-through-ws");
    client.close_all();
    // The protocol assertions passed; tear the fake server down directly
    // instead of racing its echo loop against socket teardown.
    drop(local);
    server.abort();
}

fn sha1_public(data: &[u8]) -> [u8; 20] {
    super::super::dev_tunnel::test_exports::sha1_for_tests(data)
}

async fn read_frame_raw(socket: &mut tokio::net::TcpStream) -> Option<(u8, Vec<u8>)> {
    use tokio::io::AsyncReadExt;
    let mut header = [0u8; 2];
    socket.read_exact(&mut header).await.ok()?;
    let opcode = header[0] & 0x0f;
    let masked = header[1] & 0x80 != 0;
    let len = (header[1] & 0x7f) as usize;
    let len = match len {
        126 => {
            let mut extended = [0u8; 2];
            socket.read_exact(&mut extended).await.ok()?;
            u16::from_be_bytes(extended) as usize
        }
        127 => {
            let mut extended = [0u8; 8];
            socket.read_exact(&mut extended).await.ok()?;
            u64::from_be_bytes(extended) as usize
        }
        other => other,
    };
    let mask = if masked {
        let mut mask = [0u8; 4];
        socket.read_exact(&mut mask).await.ok()?;
        Some(mask)
    } else {
        None
    };
    let mut payload = vec![0u8; len];
    if len > 0 {
        socket.read_exact(&mut payload).await.ok()?;
    }
    if let Some(mask) = mask {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Some((opcode, payload))
}

// Unused imports kept for the e2e test when ws features settle.
#[allow(dead_code)]
fn touch_ws_types(message: Message) -> Message {
    message
}

#[allow(dead_code)]
async fn touch_sink(socket: axum::extract::ws::WebSocket) {
    let (mut sender, _receiver) = socket.split();
    let _ = sender.send(Message::Ping(Vec::new().into())).await;
}
