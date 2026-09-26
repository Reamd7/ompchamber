//! Tests for the linear module port, mirroring the JS vitest suites
//! (`oauth.test.js`, `auth.test.js`, `routes.test.js`, `issues.test.js`,
//! `mapping.test.js`, `teams.test.js`, `status.test.js`,
//! `status-runtime.test.js`) with a fake transport in place of the stubbed
//! global `fetch`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use serde_json::{Value, json};
use tower::ServiceExt;

use super::auth::SetLinearAuthInput;
use super::http::{FakeTransport, HttpBody, HttpRequest, HttpResponse, HttpTransport};
use super::issues::{
    ListIssuesParams, create_linear_issue_comment, get_linear_issue, list_linear_issue_states,
    list_linear_issues, parse_linear_issue_ref, update_linear_issue,
};
use super::mapping::{
    MappingSlice, TeamRef, merge_linear_mapping_view, resolve_mapped_project_path,
};
use super::oauth::{BrokerReceipt, CallbackQuery};
use super::status::{
    SessionStatusInput, build_linear_session_open_url, build_linear_session_status_comment,
    is_public_session_origin, post_linear_session_status, prune_session_status_records,
    read_session_origin,
};
use super::status_runtime::LinearSessionStatusRuntime;
use super::teams::list_linear_teams;
use super::{LinearError, LinearState};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ompchamber-linear-{tag}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn state_with(
    tag: &str,
    env_pairs: &[(&str, &str)],
    transport: Arc<dyn HttpTransport>,
) -> (Arc<LinearState>, PathBuf) {
    let dir = temp_dir(tag);
    let state = Arc::new(LinearState::new(
        dir.clone(),
        Some(env(env_pairs)),
        transport,
    ));
    (state, dir)
}

fn json_ok(payload: Value) -> Result<HttpResponse, String> {
    Ok(HttpResponse {
        status: 200,
        body: serde_json::to_vec(&payload).expect("serialize"),
    })
}

fn transport_always_error() -> Arc<FakeTransport> {
    FakeTransport::new(|_| Err("unexpected transport call".to_string()))
}

fn set_connected(state: &LinearState, token: &str) {
    state
        .set_auth(
            SetLinearAuthInput {
                access_token: token.to_string(),
                refresh_token: Some(Some("refresh-1".to_string())),
                token_type: Some("Bearer".to_string()),
                expires_at: Some(super::now_ms() + 86_400_000.0),
                scope: Some("read,write,comments:create".to_string()),
                ..SetLinearAuthInput::default()
            },
            true,
        )
        .expect("set auth");
}

fn body_json(request: &HttpRequest) -> Value {
    match &request.body {
        HttpBody::Json(body) | HttpBody::Form(body) => {
            serde_json::from_str(body).unwrap_or(Value::Null)
        }
        HttpBody::None => Value::Null,
    }
}

fn form_pairs(request: &HttpRequest) -> Vec<(String, String)> {
    match &request.body {
        HttpBody::Form(body) => url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect(),
        _ => Vec::new(),
    }
}

fn form_get<'a>(request: &'a HttpRequest, key: &str) -> Option<String> {
    form_pairs(request)
        .into_iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

fn url_query_param(url: &str, key: &str) -> String {
    url::Url::parse(url)
        .expect("url")
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

async fn body_string(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

async fn body_json_response(response: axum::response::Response) -> Value {
    let text = body_string(response).await;
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

fn issue_node() -> Value {
    json!({
        "id": "issue-uuid-1",
        "identifier": "ENG-12",
        "title": "Broken login",
        "url": "https://linear.app/openchamber/issue/ENG-12",
        "priority": 1,
        "state": { "id": "state-started", "name": "In Progress", "type": "started" },
        "assignee": { "name": "Ada", "displayName": "Ada Lovelace", "avatarUrl": "https://example.com/a.png" },
        "team": { "id": "team-eng", "key": "ENG", "name": "Engineering" },
        "labels": { "nodes": [{ "id": "label-bug", "name": "Bug", "color": "EB5757" }] },
    })
}

// ---------------------------------------------------------------------------
// parse
// ---------------------------------------------------------------------------

#[test]
fn parse_linear_issue_ref_reads_identifiers_urls_uuids() {
    assert_eq!(
        parse_linear_issue_ref("eng-12").map(|r| (r.kind, r.value)),
        Some((
            super::issues::IssueRefKind::Identifier,
            "ENG-12".to_string()
        ))
    );
    assert_eq!(
        parse_linear_issue_ref("https://linear.app/openchamber/issue/ENG-12/broken-login")
            .map(|r| (r.kind, r.value)),
        Some((
            super::issues::IssueRefKind::Identifier,
            "ENG-12".to_string()
        ))
    );
    assert_eq!(
        parse_linear_issue_ref("11111111-2222-3333-4444-555555555555").map(|r| (r.kind, r.value)),
        Some((
            super::issues::IssueRefKind::Id,
            "11111111-2222-3333-4444-555555555555".to_string()
        ))
    );
    assert_eq!(parse_linear_issue_ref("login redirect"), None);
    assert_eq!(parse_linear_issue_ref("linear.app/issue/"), None);
    // The JS regex is unanchored, so a foreign host embedding a Linear path
    // still resolves — mirror exactly.
    assert_eq!(
        parse_linear_issue_ref("https://evil.example.com/linear.app/issue/ENG-12/x")
            .map(|r| (r.kind, r.value)),
        Some((
            super::issues::IssueRefKind::Identifier,
            "ENG-12".to_string()
        ))
    );
}

// ---------------------------------------------------------------------------
// oauth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oauth_authorize_url_uses_s256_and_stores_pending() {
    let (state, _dir) = state_with(
        "authorize",
        &[(
            "OPENCHAMBER_LINEAR_REDIRECT_URI",
            "http://127.0.0.1:3001/linear/oauth/callback",
        )],
        transport_always_error(),
    );

    let started = state.start_authorization("desktop").await.expect("start");
    let url = started["authorizationUrl"].as_str().expect("url");
    let parsed = url::Url::parse(url).expect("parse");
    assert_eq!(
        format!("{}{}", parsed.origin().ascii_serialization(), parsed.path()),
        "https://linear.app/oauth/authorize"
    );
    assert_eq!(
        url_query_param(url, "client_id"),
        "91bbe26a69a2c8568d3683f1e01e776c"
    );
    assert_eq!(
        url_query_param(url, "redirect_uri"),
        "http://127.0.0.1:3001/linear/oauth/callback"
    );
    assert_eq!(url_query_param(url, "code_challenge_method"), "S256");
    let challenge = url_query_param(url, "code_challenge");
    assert_eq!(challenge.len(), 43);
    assert!(
        challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );
    assert_eq!(url_query_param(url, "actor"), "user");
    assert_eq!(url_query_param(url, "prompt"), "consent");
    assert_eq!(started["scope"], json!("read,write,comments:create"));
    assert_eq!(started["expiresIn"], json!(600));
}

#[tokio::test]
async fn oauth_rejects_unknown_state_without_transport_call() {
    let (state, _dir) = state_with(
        "unknown-state",
        &[(
            "OPENCHAMBER_LINEAR_REDIRECT_URI",
            "http://127.0.0.1:3001/linear/oauth/callback",
        )],
        transport_always_error(),
    );
    let error = state
        .consume_authorization_callback(&CallbackQuery {
            code: "attacker-code".into(),
            state: "forged".into(),
            ..CallbackQuery::default()
        })
        .await
        .expect_err("must reject");
    assert_eq!(error.code.as_deref(), Some("UNKNOWN_STATE"));
}

#[tokio::test]
async fn oauth_exchanges_code_with_pkce_verifier_once() {
    let transport = FakeTransport::new(|request| {
        assert_eq!(request.url, "https://api.linear.app/oauth/token");
        assert_eq!(
            request.header("Content-Type"),
            Some("application/x-www-form-urlencoded")
        );
        json_ok(json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "token_type": "Bearer",
            "expires_in": 86399,
            "scope": "read,write,comments:create",
        }))
    });
    let (state, _dir) = state_with(
        "exchange",
        &[(
            "OPENCHAMBER_LINEAR_REDIRECT_URI",
            "http://127.0.0.1:3001/linear/oauth/callback",
        )],
        transport.clone(),
    );

    let started = state.start_authorization("web").await.expect("start");
    let authorization_url = started["authorizationUrl"].as_str().expect("url");
    let oauth_state = url_query_param(authorization_url, "state");

    let result = state
        .consume_authorization_callback(&CallbackQuery {
            code: "auth-code".into(),
            state: oauth_state.clone(),
            ..CallbackQuery::default()
        })
        .await
        .expect("exchange");
    assert_eq!(result.tokens.access_token, "access-1");
    assert_eq!(result.tokens.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(result.origin, super::oauth::AuthOrigin::Web);

    assert_eq!(transport.call_count(), 1);
    let call = &transport.calls()[0];
    let verifier = form_get(call, "code_verifier").expect("verifier");
    assert_eq!(verifier.len(), 43);
    assert_eq!(
        form_get(call, "grant_type").as_deref(),
        Some("authorization_code")
    );
    assert_eq!(form_get(call, "code").as_deref(), Some("auth-code"));
    assert_eq!(form_get(call, "client_secret"), None);

    // The pending entry is consumed; a replay is rejected.
    let replay = state
        .consume_authorization_callback(&CallbackQuery {
            code: "auth-code".into(),
            state: oauth_state,
            ..CallbackQuery::default()
        })
        .await
        .expect_err("must reject");
    assert_eq!(replay.code.as_deref(), Some("UNKNOWN_STATE"));
    assert_eq!(transport.call_count(), 1);
}

#[tokio::test]
async fn oauth_refresh_rotates_token() {
    let transport = FakeTransport::new(|_request| {
        json_ok(json!({
            "access_token": "access-2",
            "refresh_token": "refresh-2",
            "token_type": "Bearer",
            "expires_in": 86399,
        }))
    });
    let (state, _dir) = state_with("refresh", &[], transport.clone());

    let tokens = state
        .refresh_access_token("refresh-1")
        .await
        .expect("refresh");
    assert_eq!(tokens.access_token, "access-2");
    assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-2"));
    let call = &transport.calls()[0];
    assert_eq!(
        form_get(call, "grant_type").as_deref(),
        Some("refresh_token")
    );
    assert_eq!(
        form_get(call, "refresh_token").as_deref(),
        Some("refresh-1")
    );

    let error = state.refresh_access_token("  ").await.expect_err("empty");
    assert_eq!(error.code.as_deref(), Some("MISSING_REFRESH_TOKEN"));
}

#[tokio::test]
async fn oauth_broker_flow_start_poll_complete() {
    let transport = FakeTransport::new(|request| match request.url.as_str() {
        url if url.ends_with("/start") => {
            let body = body_json(request);
            assert_eq!(body["state"].as_str().map(str::len), Some(43));
            assert_eq!(body["claimSecret"].as_str().map(str::len), Some(43));
            json_ok(json!({
                "redirectUri": "https://api.openchamber.dev/v1/oauth/linear/callback",
                "expiresIn": 600,
            }))
        }
        url if url.ends_with("/poll") => {
            json_ok(json!({ "status": "complete", "code": "broker-code" }))
        }
        url if url.ends_with("/complete") => json_ok(json!({ "ok": true })),
        "https://api.linear.app/oauth/token" => {
            let redirect = form_get(request, "redirect_uri").unwrap_or_default();
            assert_eq!(
                redirect,
                "https://api.openchamber.dev/v1/oauth/linear/callback"
            );
            assert_eq!(form_get(request, "code").as_deref(), Some("broker-code"));
            assert_eq!(
                form_get(request, "code_verifier").map(|v| v.len()),
                Some(43)
            );
            json_ok(json!({
                "access_token": "broker-access",
                "refresh_token": "broker-refresh",
                "expires_in": 86399,
            }))
        }
        other => panic!("unexpected fetch: {other}"),
    });
    let (state, _dir) = state_with("broker", &[], transport.clone());

    let started = state.start_authorization("desktop").await.expect("start");
    let authorization_url = started["authorizationUrl"].as_str().expect("url");
    assert_eq!(
        url_query_param(authorization_url, "redirect_uri"),
        "https://api.openchamber.dev/v1/oauth/linear/callback"
    );

    let result = state
        .poll_authorization_broker()
        .await
        .expect("poll")
        .expect("completed");
    assert_eq!(result.tokens.access_token, "broker-access");
    assert_eq!(result.origin, super::oauth::AuthOrigin::Desktop);
    let receipt = BrokerReceipt {
        state: url_query_param(authorization_url, "state"),
        url: "https://api.openchamber.dev/v1/oauth/linear".to_string(),
        claim_secret: "claim".to_string(),
    };
    assert!(state.complete_authorization_broker(&receipt).await.unwrap());
    assert_eq!(transport.call_count(), 4);
}

#[tokio::test]
async fn oauth_broker_poll_waits_on_202() {
    let transport = FakeTransport::new(|request| {
        if request.url.ends_with("/start") {
            return json_ok(json!({
                "redirectUri": "https://api.openchamber.dev/v1/oauth/linear/callback",
            }));
        }
        assert!(request.url.ends_with("/poll"));
        Ok(HttpResponse {
            status: 202,
            body: Vec::new(),
        })
    });
    let (state, _dir) = state_with("broker-202", &[], transport.clone());
    state.start_authorization("web").await.expect("start");
    let result = state.poll_authorization_broker().await.expect("poll");
    assert!(result.is_none());
    assert_eq!(transport.call_count(), 2); // /start + /poll
}

// ---------------------------------------------------------------------------
// auth store
// ---------------------------------------------------------------------------

#[test]
fn auth_store_shapes_and_public_status() {
    let (state, dir) = state_with("auth-shapes", &[], transport_always_error());
    assert!(state.get_auth().is_none());
    assert_eq!(
        state.to_public_status(None, None),
        json!({ "connected": false })
    );

    let stored = state
        .set_auth(
            SetLinearAuthInput {
                access_token: "lin_oauth_access".into(),
                refresh_token: Some(Some("lin_oauth_refresh".into())),
                expires_at: Some(super::now_ms() + 60_000.0),
                scope: Some("read,write".into()),
                user: Some(Some(super::auth::LinearUser {
                    id: "user-1".into(),
                    name: Some("Ada".into()),
                    display_name: Some("Ada Lovelace".into()),
                    email: Some("ada@example.com".into()),
                    avatar_url: Some("https://example.com/a.png".into()),
                })),
                organization: Some(Some(super::auth::LinearOrganization {
                    id: "org-1".into(),
                    name: "OpenChamber".into(),
                    url_key: Some("openchamber".into()),
                })),
                ..SetLinearAuthInput::default()
            },
            true,
        )
        .expect("set");

    assert_eq!(stored.access_token, "lin_oauth_access");
    assert_eq!(stored.workspace_id, "org-1");

    let public = state.to_public_status(state.get_auth().as_ref(), None);
    assert_eq!(public["connected"], json!(true));
    assert_eq!(public["scope"], json!("read,write"));
    assert_eq!(public["workspaces"][0]["id"], json!("org-1"));
    assert!(public["workspaces"][0]["authorizedAt"].is_number());
    let serialized = public.to_string();
    assert!(!serialized.contains("lin_oauth"));

    let raw: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("linear-auth.json")).expect("auth file"),
    )
    .expect("json");
    assert!(raw.get("accessToken").is_none());
    assert_eq!(raw["workspaces"].as_array().map(Vec::len), Some(1));
    assert_eq!(
        raw["workspaces"][0]["accessToken"],
        json!("lin_oauth_access")
    );
    assert!(raw["workspaces"][0]["expiresAt"].is_number());
}

#[test]
fn auth_refresh_token_semantics() {
    let (state, _dir) = state_with("auth-refresh", &[], transport_always_error());
    state
        .set_auth(
            SetLinearAuthInput {
                access_token: "access-1".into(),
                refresh_token: Some(Some("refresh-1".into())),
                expires_at: Some(1.0),
                ..SetLinearAuthInput::default()
            },
            true,
        )
        .unwrap();
    // Absent refreshToken keeps the stored one.
    state
        .set_auth(
            SetLinearAuthInput {
                access_token: "access-2".into(),
                expires_at: Some(2.0),
                ..SetLinearAuthInput::default()
            },
            true,
        )
        .unwrap();
    let auth = state.get_auth().unwrap();
    assert_eq!(auth.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(auth.access_token, "access-2");
    // Present (empty) refreshToken drops it.
    state
        .set_auth(
            SetLinearAuthInput {
                access_token: "access-3".into(),
                refresh_token: Some(None),
                ..SetLinearAuthInput::default()
            },
            true,
        )
        .unwrap();
    assert_eq!(state.get_auth().unwrap().refresh_token, None);

    let error = state
        .set_auth(
            SetLinearAuthInput {
                refresh_token: Some(Some("refresh-1".into())),
                ..SetLinearAuthInput::default()
            },
            true,
        )
        .expect_err("missing access token");
    assert_eq!(error.message, "accessToken is required");
}

#[test]
fn auth_stale_detection_and_client_config() {
    assert!(super::auth::is_access_token_stale(None, super::now_ms()));
    assert!(super::auth::is_access_token_stale(
        Some(super::now_ms() - 1.0),
        super::now_ms()
    ));
    assert!(!super::auth::is_access_token_stale(
        Some(super::now_ms() + 10.0 * 60_000.0),
        super::now_ms()
    ));

    let (state, _dir) = state_with("auth-config", &[], transport_always_error());
    assert_eq!(state.get_client_id(), super::auth::DEFAULT_LINEAR_CLIENT_ID);
    assert_eq!(
        state.get_redirect_uri(),
        "https://api.openchamber.dev/v1/oauth/linear/callback"
    );

    let (overridden, _dir2) = state_with(
        "auth-config-env",
        &[
            ("OPENCHAMBER_LINEAR_CLIENT_ID", "env-client"),
            (
                "OPENCHAMBER_LINEAR_REDIRECT_URI",
                "http://localhost:3000/linear/oauth/callback",
            ),
            (
                "OPENCHAMBER_LINEAR_BROKER_URL",
                "https://broker.example.com/v1/oauth/linear///",
            ),
        ],
        transport_always_error(),
    );
    assert_eq!(overridden.get_client_id(), "env-client");
    assert_eq!(
        overridden.get_redirect_uri(),
        "http://localhost:3000/linear/oauth/callback"
    );
    assert_eq!(
        overridden.get_broker_url(),
        "https://broker.example.com/v1/oauth/linear"
    );
}

#[test]
fn auth_clear_deletes_file_and_returns_disconnected() {
    let (state, dir) = state_with("auth-clear", &[], transport_always_error());
    state
        .set_auth(
            SetLinearAuthInput {
                access_token: "access-1".into(),
                refresh_token: Some(Some("refresh-1".into())),
                ..SetLinearAuthInput::default()
            },
            true,
        )
        .unwrap();
    assert!(dir.join("linear-auth.json").exists());
    assert!(state.clear_auth(None));
    assert!(!dir.join("linear-auth.json").exists());
    assert!(state.get_auth().is_none());
}

#[test]
fn auth_migrates_legacy_single_workspace_file() {
    let (state, dir) = state_with("auth-legacy", &[], transport_always_error());
    std::fs::write(
        dir.join("linear-auth.json"),
        serde_json::to_string(&json!({
            "accessToken": "legacy-access",
            "refreshToken": "legacy-refresh",
            "user": { "id": "user-1", "name": "Ada" },
            "organization": { "id": "org-1", "name": "OpenChamber", "urlKey": "openchamber" },
        }))
        .unwrap(),
    )
    .unwrap();

    let stored = state.get_auth().expect("migrated");
    assert_eq!(stored.access_token, "legacy-access");
    assert_eq!(stored.workspace_id, "org-1");
    assert!(stored.current);
    let raw: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("linear-auth.json")).unwrap())
            .unwrap();
    assert_eq!(raw["workspaces"].as_array().map(Vec::len), Some(1));
    assert!(raw.get("accessToken").is_none());
}

fn auth_with_org(state: &LinearState, token: &str, org: &str) {
    state
        .set_auth(
            SetLinearAuthInput {
                access_token: token.into(),
                user: Some(Some(super::auth::LinearUser {
                    id: format!("user-{org}"),
                    name: Some(org.into()),
                    display_name: None,
                    email: None,
                    avatar_url: None,
                })),
                organization: Some(Some(super::auth::LinearOrganization {
                    id: org.into(),
                    name: org.into(),
                    url_key: Some(org.into()),
                })),
                ..SetLinearAuthInput::default()
            },
            true,
        )
        .unwrap();
}

#[test]
fn auth_multiple_workspaces_activate_and_scoped_clear() {
    let (state, _dir) = state_with("auth-multi", &[], transport_always_error());
    auth_with_org(&state, "access-a", "org-a");
    auth_with_org(&state, "access-b", "org-b");

    let current = state.get_auth().unwrap();
    assert_eq!(current.workspace_id, "org-b");
    assert_eq!(
        state
            .auth_workspaces()
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<Vec<_>>(),
        vec!["org-a", "org-b"]
    );
    assert!(state.activate_auth("org-a"));
    assert_eq!(state.get_auth().unwrap().access_token, "access-a");
    assert!(!state.auth_workspaces()[1].current);

    // Unscoped clear drops only the current workspace.
    assert!(state.clear_auth(None));
    let remaining = state.get_auth().unwrap();
    assert_eq!(remaining.workspace_id, "org-b");
    assert_eq!(state.auth_workspaces().len(), 1);
    assert!(!state.activate_auth("missing"));
}

#[test]
fn auth_authorized_at_not_bumped_without_activate() {
    let (state, dir) = state_with("auth-authorized-at", &[], transport_always_error());
    auth_with_org(&state, "access-1", "org-1");
    let raw_path = dir.join("linear-auth.json");
    let mut raw: Value =
        serde_json::from_str(&std::fs::read_to_string(&raw_path).unwrap()).unwrap();
    raw["workspaces"][0]["authorizedAt"] = json!(111);
    std::fs::write(&raw_path, serde_json::to_string_pretty(&raw).unwrap()).unwrap();

    state
        .set_auth(
            SetLinearAuthInput {
                access_token: "access-1".into(),
                workspace_id: Some("org-1".into()),
                user: Some(Some(super::auth::LinearUser {
                    id: "user-1".into(),
                    name: Some("Ada".into()),
                    display_name: None,
                    email: None,
                    avatar_url: None,
                })),
                organization: Some(Some(super::auth::LinearOrganization {
                    id: "org-1".into(),
                    name: "OpenChamber".into(),
                    url_key: None,
                })),
                ..SetLinearAuthInput::default()
            },
            false,
        )
        .unwrap();
    let auth = state.get_auth().unwrap();
    assert_eq!(auth.authorized_at, Some(111.0));
    assert!(auth.current);
}

#[test]
fn auth_session_comments_preference_roundtrip() {
    let (state, _dir) = state_with("auth-comments", &[], transport_always_error());
    assert!(!state.session_comments_enabled());
    assert!(state.set_session_comments_enabled(true));
    assert!(state.session_comments_enabled());
}

// ---------------------------------------------------------------------------
// mapping
// ---------------------------------------------------------------------------

#[test]
fn mapping_missing_file_roundtrip_and_permissions() {
    let (state, dir) = state_with("mapping-roundtrip", &[], transport_always_error());
    assert!(!dir.join("linear-mapping.json").exists());
    assert_eq!(
        state.read_stored_mapping().unwrap(),
        MappingSlice::default()
    );

    let written = state
        .set_stored_mapping(&json!({
            "defaultProjectPath": "/Users/ada/openchamber",
            "teamProjectPaths": { "team-eng": "/Users/ada/eng", "team-empty": "   " },
        }))
        .unwrap();
    assert_eq!(
        written.default_project_path.as_deref(),
        Some("/Users/ada/openchamber")
    );
    assert_eq!(
        written.team_project_paths,
        vec![("team-eng".to_string(), "/Users/ada/eng".to_string())]
    );
    assert_eq!(state.read_stored_mapping().unwrap(), written);

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(dir.join("linear-mapping.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn mapping_replaces_previous_and_rejects_invalid_body() {
    let (state, _dir) = state_with("mapping-replace", &[], transport_always_error());
    state
        .set_stored_mapping(&json!({
            "defaultProjectPath": "/old",
            "teamProjectPaths": { "team-eng": "/eng" },
        }))
        .unwrap();
    let next = state
        .set_stored_mapping(&json!({ "defaultProjectPath": null, "teamProjectPaths": {} }))
        .unwrap();
    assert_eq!(next, MappingSlice::default());
    assert_eq!(state.read_stored_mapping().unwrap(), next);

    let error = state.set_stored_mapping(&Value::Null).unwrap_err();
    assert_eq!(error.code.as_deref(), Some("INVALID"));
    assert_eq!(error.message, "Mapping body must be an object");
    // The previous mapping survives a rejected write.
    assert_eq!(
        state.read_stored_mapping().unwrap(),
        MappingSlice::default()
    );
}

#[test]
fn mapping_malformed_file_is_an_error_not_empty() {
    let (state, dir) = state_with("mapping-malformed", &[], transport_always_error());
    std::fs::write(dir.join("linear-mapping.json"), "{not-json").unwrap();
    let error = state.read_stored_mapping().unwrap_err();
    assert_eq!(error.code.as_deref(), Some("MALFORMED"));
    assert_eq!(error.message, "Linear mapping file is malformed");
}

#[test]
fn mapping_merge_and_resolve() {
    let stored = MappingSlice {
        default_project_path: Some("/default".into()),
        team_project_paths: vec![("team-eng".into(), "/eng".into())],
    };
    let teams = vec![
        super::issues::read_team(&json!({ "id": "team-eng", "key": "ENG", "name": "Engineering" }))
            .unwrap(),
        super::issues::read_team(&json!({ "id": "team-des", "key": "DES", "name": "Design" }))
            .unwrap(),
    ];
    let view = merge_linear_mapping_view(Some(&stored), &teams);
    assert_eq!(view.default_project_path.as_deref(), Some("/default"));
    assert_eq!(view.teams[0].project_path.as_deref(), Some("/eng"));
    assert_eq!(view.teams[1].project_path, None);

    assert_eq!(
        resolve_mapped_project_path(
            &view,
            Some(&TeamRef {
                id: "team-eng".into(),
                key: "ENG".into()
            })
        ),
        Some("/eng".to_string())
    );
    assert_eq!(
        resolve_mapped_project_path(
            &view,
            Some(&TeamRef {
                id: "team-des".into(),
                key: "DES".into()
            })
        ),
        Some("/default".to_string())
    );
    assert_eq!(
        resolve_mapped_project_path(&view, None),
        Some("/default".to_string())
    );
}

#[test]
fn mapping_slices_stay_per_workspace() {
    let (state, _dir) = state_with("mapping-slices", &[], transport_always_error());
    auth_with_org(&state, "access-a", "org-a");
    state
        .set_stored_mapping(&json!({
            "defaultProjectPath": "/alpha",
            "teamProjectPaths": { "team-a": "/alpha-eng" },
        }))
        .unwrap();
    auth_with_org(&state, "access-b", "org-b");
    state
        .set_stored_mapping(&json!({ "defaultProjectPath": "/beta", "teamProjectPaths": {} }))
        .unwrap();
    assert_eq!(
        state.read_stored_mapping().unwrap().default_project_path,
        Some("/beta".to_string())
    );
    assert!(state.activate_auth("org-a"));
    let alpha = state.read_stored_mapping().unwrap();
    assert_eq!(alpha.default_project_path, Some("/alpha".to_string()));
    assert_eq!(
        alpha.team_project_paths,
        vec![("team-a".to_string(), "/alpha-eng".to_string())]
    );
}

// ---------------------------------------------------------------------------
// issues (transport level)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn issues_disconnected_without_transport_calls() {
    let (state, _dir) = state_with("issues-disconnected", &[], transport_always_error());
    assert_eq!(
        list_linear_issues(&state, &ListIssuesParams::default())
            .await
            .unwrap(),
        json!({ "connected": false })
    );
}

#[tokio::test]
async fn issues_list_shape_headers_and_pagination_fields() {
    let transport = FakeTransport::new(|request| {
        assert_eq!(request.url, "https://api.linear.app/graphql");
        assert_eq!(request.header("Authorization"), Some("Bearer access-1"));
        assert_eq!(request.header("public-file-urls-expire-in"), Some("3600"));
        let body = body_json(request);
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("query ListLinearIssues")
        );
        assert_eq!(
            body["variables"]["filter"]["state"]["type"]["nin"],
            json!(["completed", "canceled", "duplicate"])
        );
        assert_eq!(body["variables"]["first"], json!(50));
        json_ok(json!({
            "data": {
                "issues": {
                    "nodes": [issue_node()],
                    "pageInfo": { "hasNextPage": true, "endCursor": "cursor-2" },
                },
            },
        }))
    });
    let (state, _dir) = state_with("issues-list", &[], transport.clone());
    set_connected(&state, "access-1");

    let result = list_linear_issues(&state, &ListIssuesParams::default())
        .await
        .unwrap();
    assert_eq!(result["connected"], json!(true));
    assert_eq!(result["cursor"], json!("cursor-2"));
    assert_eq!(result["hasMore"], json!(true));
    let issue = &result["issues"][0];
    assert_eq!(issue["identifier"], json!("ENG-12"));
    assert_eq!(issue["priority"], json!(1));
    assert_eq!(
        issue["labels"][0],
        json!({
            "id": "label-bug", "name": "Bug", "color": "#eb5757"
        })
    );
    assert_eq!(
        issue["state"],
        json!({ "id": "state-started", "name": "In Progress", "type": "started" })
    );
    assert!(!result.to_string().contains("access-1"));
}

#[tokio::test]
async fn issues_drop_invalid_priority_and_label_colors() {
    let transport = FakeTransport::new(|_| {
        json_ok(json!({
            "data": {
                "issues": {
                    "nodes": [{
                        "id": "issue-uuid-1",
                        "identifier": "ENG-12",
                        "title": "Broken login",
                        "url": "https://linear.app/openchamber/issue/ENG-12",
                        "priority": 9,
                        "labels": { "nodes": [
                            { "id": "label-ok", "name": "Bug", "color": "#EB5757" },
                            { "id": "label-bad-color", "name": "Nope", "color": "red" },
                            { "id": "", "name": "Missing id" },
                        ]},
                    }],
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                },
            },
        }))
    });
    let (state, _dir) = state_with("issues-invalid", &[], transport);
    set_connected(&state, "access-1");
    let result = list_linear_issues(&state, &ListIssuesParams::default())
        .await
        .unwrap();
    assert_eq!(result["issues"][0]["priority"], Value::Null);
    assert_eq!(
        result["issues"][0]["labels"],
        json!([
            { "id": "label-ok", "name": "Bug", "color": "#eb5757" },
            { "id": "label-bad-color", "name": "Nope", "color": null },
        ])
    );
}

#[tokio::test]
async fn issues_search_and_identifier_direct_lookup() {
    let transport = FakeTransport::new(|request| {
        let body = body_json(request);
        if body["query"]
            .as_str()
            .unwrap()
            .contains("SearchLinearIssues")
        {
            assert_eq!(body["variables"]["term"], json!("login"));
            return json_ok(json!({
                "data": {
                    "searchIssues": {
                        "nodes": [issue_node()],
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                    },
                },
            }));
        }
        assert_eq!(body["variables"]["id"], json!("ENG-12"));
        json_ok(json!({
            "data": {
                "issue": {
                    "id": "issue-uuid-1",
                    "identifier": "ENG-12",
                    "title": "Broken login",
                    "url": "https://linear.app/openchamber/issue/ENG-12",
                    "state": { "name": "In Progress", "type": "started" },
                    "assignee": Value::Null,
                    "team": { "id": "team-eng", "key": "ENG", "name": "Engineering" },
                    "labels": { "nodes": [] },
                    "priority": Value::Null,
                    "description": "Users cannot sign in.",
                    "comments": { "nodes": [{
                        "id": "comment-1",
                        "body": "Still broken",
                        "createdAt": "2026-08-24T10:00:00.000Z",
                        "user": { "name": "Ada", "displayName": "Ada Lovelace" },
                    }]},
                },
            },
        }))
    });
    let (state, _dir) = state_with("issues-search", &[], transport);
    set_connected(&state, "access-1");

    let search = list_linear_issues(
        &state,
        &ListIssuesParams {
            query: "login".into(),
            ..ListIssuesParams::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(search["issues"].as_array().map(Vec::len), Some(1));
    assert_eq!(search["hasMore"], json!(false));

    let by_url = list_linear_issues(
        &state,
        &ListIssuesParams {
            query: "https://linear.app/openchamber/issue/ENG-12".into(),
            ..ListIssuesParams::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(by_url["issues"][0]["identifier"], json!("ENG-12"));
    assert_eq!(by_url["hasMore"], json!(false));
    assert_eq!(by_url["cursor"], Value::Null);
}

#[tokio::test]
async fn issues_status_filter_table() {
    let (state, _dir) = state_with("issues-filters", &[], transport_always_error());
    set_connected(&state, "access-1");
    let expected: Vec<(&str, Value)> = vec![
        (
            "open",
            json!({ "type": { "nin": ["completed", "canceled", "duplicate"] } }),
        ),
        ("backlog", json!({ "type": { "eq": "backlog" } })),
        ("todo", json!({ "type": { "eq": "unstarted" } })),
        (
            "started",
            json!({ "type": { "eq": "started" }, "name": { "neqIgnoreCase": "In Review" } }),
        ),
        (
            "inReview",
            json!({ "name": { "eqIgnoreCase": "In Review" } }),
        ),
        ("completed", json!({ "type": { "eq": "completed" } })),
        (
            "canceled",
            json!({ "type": { "eq": "canceled" }, "name": { "neqIgnoreCase": "Duplicate" } }),
        ),
        (
            "duplicate",
            json!({ "or": [
                { "type": { "eq": "duplicate" } },
                { "name": { "eqIgnoreCase": "Duplicate" } },
            ]}),
        ),
    ];
    for (status, state_filter) in expected {
        assert_eq!(
            super::issues::build_issue_list_filter(&ListIssuesParams {
                status: status.into(),
                ..ListIssuesParams::default()
            })
            .unwrap_or(Value::Null)["state"],
            state_filter,
            "status {status}"
        );
    }
    // 'all' and unknown statuses omit the state filter entirely.
    assert_eq!(
        super::issues::build_issue_list_filter(&ListIssuesParams {
            status: "all".into(),
            ..ListIssuesParams::default()
        }),
        None
    );
    assert_eq!(
        super::issues::build_issue_list_filter(&ListIssuesParams {
            status: "nonsense".into(),
            ..ListIssuesParams::default()
        })
        .unwrap()["state"],
        json!({ "type": { "nin": ["completed", "canceled", "duplicate"] } })
    );
    // Combined filters and priority mapping.
    assert_eq!(
        super::issues::build_issue_list_filter(&ListIssuesParams {
            status: "completed".into(),
            assignee: "me".into(),
            team_id: "team-eng".into(),
            priority: "urgent".into(),
            ..ListIssuesParams::default()
        })
        .unwrap(),
        json!({
            "state": { "type": { "eq": "completed" } },
            "assignee": { "isMe": { "eq": true } },
            "team": { "id": { "eq": "team-eng" } },
            "priority": { "eq": 1 },
        })
    );
    assert_eq!(
        super::issues::build_issue_list_filter(&ListIssuesParams {
            status: "all".into(),
            priority: "none".into(),
            ..ListIssuesParams::default()
        })
        .unwrap(),
        json!({ "priority": { "eq": 0 } })
    );
}

#[tokio::test]
async fn issues_identifier_lookup_ignores_list_filters() {
    let transport = FakeTransport::new(|request| {
        let body = body_json(request);
        assert!(body["query"].as_str().unwrap().contains("GetLinearIssue"));
        assert_eq!(body["variables"]["id"], json!("ENG-12"));
        assert!(body["variables"].get("filter").is_none());
        json_ok(json!({ "data": { "issue": issue_node() } }))
    });
    let (state, _dir) = state_with("issues-by-id", &[], transport);
    set_connected(&state, "access-1");
    let result = list_linear_issues(
        &state,
        &ListIssuesParams {
            query: "ENG-12".into(),
            status: "completed".into(),
            assignee: "me".into(),
            team_id: "team-eng".into(),
            priority: "urgent".into(),
            ..ListIssuesParams::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(result["issues"][0]["identifier"], json!("ENG-12"));
}

#[tokio::test]
async fn issues_get_includes_comments_and_description() {
    let transport = FakeTransport::new(|_| {
        json_ok(json!({
            "data": {
                "issue": {
                    "id": "issue-uuid-1",
                    "identifier": "ENG-12",
                    "title": "Broken login",
                    "url": "https://linear.app/openchamber/issue/ENG-12",
                    "state": { "name": "Todo", "type": "unstarted" },
                    "assignee": Value::Null,
                    "team": { "id": "team-eng", "key": "ENG", "name": "Engineering" },
                    "labels": { "nodes": [{ "id": "label-bug", "name": "Bug", "color": "EB5757" }] },
                    "priority": 1,
                    "description": "Users cannot sign in.",
                    "comments": { "nodes": [{
                        "id": "comment-1",
                        "body": "Still broken",
                        "createdAt": "2026-08-24T10:00:00.000Z",
                        "user": { "name": "Ada", "displayName": null, "avatarUrl": "https://linear.app/avatar/ada.png" },
                    }]},
                },
            },
        }))
    });
    let (state, _dir) = state_with("issues-get", &[], transport);
    set_connected(&state, "access-1");
    let result = get_linear_issue(&state, "ENG-12").await.unwrap();
    assert_eq!(result["connected"], json!(true));
    assert_eq!(
        result["issue"]["description"],
        json!("Users cannot sign in.")
    );
    assert_eq!(
        result["issue"]["comments"][0],
        json!({
            "id": "comment-1",
            "body": "Still broken",
            "createdAt": "2026-08-24T10:00:00.000Z",
            "user": { "name": "Ada", "displayName": null, "avatarUrl": "https://linear.app/avatar/ada.png" },
        })
    );
    // An empty id answers a null issue without touching the network.
    let empty = get_linear_issue(&state, "   ").await.unwrap();
    assert_eq!(empty, json!({ "connected": true, "issue": Value::Null }));
}

#[tokio::test]
async fn issues_workflow_states_ordered_like_linear() {
    let transport = FakeTransport::new(|_| {
        json_ok(json!({
            "data": {
                "team": {
                    "states": { "nodes": [
                        { "id": "state-done", "name": "Done", "type": "completed", "position": 0 },
                        { "id": "state-review", "name": "In Review", "type": "started", "position": 1 },
                        { "id": "state-todo", "name": "Todo", "type": "unstarted", "position": 0 },
                        { "id": "state-dup", "name": "Duplicate", "type": "canceled", "position": 1 },
                        { "id": "state-progress", "name": "In Progress", "type": "started", "position": 0 },
                        { "id": "state-backlog", "name": "Backlog", "type": "backlog", "position": 0 },
                        { "id": "state-canceled", "name": "Canceled", "type": "canceled", "position": 0 },
                    ]},
                },
            },
        }))
    });
    let (state, _dir) = state_with("issues-states", &[], transport);
    set_connected(&state, "access-1");
    let result = list_linear_issue_states(&state, "team-eng").await.unwrap();
    let names: Vec<&str> = result["states"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec![
            "Backlog",
            "Todo",
            "In Progress",
            "In Review",
            "Done",
            "Canceled",
            "Duplicate"
        ]
    );

    let error = list_linear_issue_states(&state, " ").await.unwrap_err();
    assert_eq!(error.code.as_deref(), Some("INVALID"));
    assert_eq!(error.message, "teamId is required");
}

#[tokio::test]
async fn issues_update_resolves_identifier_then_mutates() {
    let transport = FakeTransport::new(|request| {
        let body = body_json(request);
        if body["query"].as_str().unwrap().contains("GetLinearIssue") {
            assert_eq!(body["variables"]["id"], json!("ENG-12"));
            return json_ok(json!({ "data": { "issue": issue_node() } }));
        }
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("mutation IssueUpdate")
        );
        assert_eq!(
            body["variables"],
            json!({ "id": "issue-uuid-1", "input": { "stateId": "state-done" } })
        );
        json_ok(json!({
            "data": {
                "issueUpdate": {
                    "success": true,
                    "issue": {
                        "id": "issue-uuid-1",
                        "identifier": "ENG-12",
                        "title": "Broken login",
                        "url": "https://linear.app/openchamber/issue/ENG-12",
                        "state": { "id": "state-done", "name": "Done", "type": "completed" },
                        "assignee": Value::Null,
                        "team": { "id": "team-eng", "key": "ENG", "name": "Engineering" },
                        "labels": { "nodes": [] },
                        "priority": Value::Null,
                        "description": null,
                        "comments": { "nodes": [] },
                    },
                },
            },
        }))
    });
    let (state, _dir) = state_with("issues-update", &[], transport);
    set_connected(&state, "access-1");
    let result = update_linear_issue(&state, "ENG-12", "state-done")
        .await
        .unwrap();
    assert_eq!(result["connected"], json!(true));
    assert_eq!(result["issue"]["id"], json!("issue-uuid-1"));
    assert_eq!(
        result["issue"]["state"],
        json!({ "id": "state-done", "name": "Done", "type": "completed" })
    );

    let error = update_linear_issue(&state, "issue-uuid-1", " ")
        .await
        .unwrap_err();
    assert_eq!(error.message, "id and stateId are required");
}

#[tokio::test]
async fn issues_comment_create_resolves_uuid() {
    let transport = FakeTransport::new(|request| {
        let body = body_json(request);
        if body["query"].as_str().unwrap().contains("GetLinearIssue") {
            return json_ok(json!({ "data": { "issue": issue_node() } }));
        }
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("mutation CommentCreate")
        );
        assert_eq!(
            body["variables"]["input"],
            json!({ "issueId": "issue-uuid-1", "body": "OpenChamber session started." })
        );
        json_ok(json!({
            "data": { "commentCreate": { "success": true, "comment": { "id": "comment-9" } } },
        }))
    });
    let (state, _dir) = state_with("issues-comment", &[], transport);
    set_connected(&state, "access-1");
    let result = create_linear_issue_comment(
        &state,
        "ENG-12",
        &Value::String("OpenChamber session started.".into()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        result,
        json!({ "connected": true, "comment": { "id": "comment-9" } })
    );
}

#[tokio::test]
async fn issues_graphql_401_clears_workspace() {
    let transport = FakeTransport::new(|_| {
        Ok(HttpResponse {
            status: 401,
            body: br#"{"errors":[{"message":"Unauthorized"}]}"#.to_vec(),
        })
    });
    let (state, _dir) = state_with("issues-401", &[], transport.clone());
    set_connected(&state, "access-1");
    assert_eq!(
        list_linear_issues(&state, &ListIssuesParams::default())
            .await
            .unwrap(),
        json!({ "connected": false })
    );
    assert!(state.get_auth().is_none());
    // A second call stays disconnected without another transport round trip.
    assert_eq!(
        list_linear_issues(&state, &ListIssuesParams::default())
            .await
            .unwrap(),
        json!({ "connected": false })
    );
    assert_eq!(transport.call_count(), 1);
}

#[tokio::test]
async fn issues_validation_errors_surface_as_user_errors() {
    let transport = FakeTransport::new(|_| {
        json_ok(json!({
            "data": Value::Null,
            "errors": [{
                "message": "Argument Validation Error",
                "extensions": {
                    "code": "INVALID_INPUT",
                    "userError": true,
                    "userPresentableMessage": "stateId must be a UUID.",
                    "validationErrors": [{
                        "property": "stateId",
                        "constraints": { "isUuid": "stateId must be a UUID." },
                    }],
                },
            }],
        }))
    });
    let (state, _dir) = state_with("issues-validation", &[], transport);
    set_connected(&state, "access-1");
    let error = update_linear_issue(&state, "issue-uuid-1", "not-a-uuid")
        .await
        .unwrap_err();
    assert_eq!(error.message, "stateId must be a UUID.");
    assert!(error.user_error);
    assert_eq!(error.http_status(), Some(400));
}

// ---------------------------------------------------------------------------
// teams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn teams_list_across_pages() {
    let transport = FakeTransport::new(|request| {
        assert!(request.url.ends_with("/graphql"));
        let body = body_json(request);
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("query ListLinearTeams")
        );
        assert_eq!(request.header("Authorization"), Some("Bearer access-1"));
        if body["variables"]["after"].is_null() {
            return json_ok(json!({
                "data": {
                    "teams": {
                        "nodes": [{ "id": "team-eng", "key": "ENG", "name": "Engineering" }],
                        "pageInfo": { "hasNextPage": true, "endCursor": "cursor-2" },
                    },
                },
            }));
        }
        assert_eq!(body["variables"]["after"], json!("cursor-2"));
        json_ok(json!({
            "data": {
                "teams": {
                    "nodes": [{ "id": "team-des", "key": "DES", "name": "Design" }],
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                },
            },
        }))
    });
    let (state, _dir) = state_with("teams-pages", &[], transport.clone());
    set_connected(&state, "access-1");
    let result = list_linear_teams(&state).await.unwrap();
    assert_eq!(
        result,
        json!({
            "connected": true,
            "teams": [
                { "id": "team-eng", "key": "ENG", "name": "Engineering" },
                { "id": "team-des", "key": "DES", "name": "Design" },
            ],
        })
    );
    assert!(!result.to_string().contains("access-1"));
    assert_eq!(transport.call_count(), 2);
}

#[tokio::test]
async fn teams_graphql_401_clears_and_disconnects() {
    let transport = FakeTransport::new(|_| {
        Ok(HttpResponse {
            status: 401,
            body: b"{}".to_vec(),
        })
    });
    let (state, _dir) = state_with("teams-401", &[], transport);
    set_connected(&state, "access-1");
    assert_eq!(
        list_linear_teams(&state).await.unwrap(),
        json!({ "connected": false })
    );
    assert!(state.get_auth().is_none());
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[test]
fn status_origin_parsing() {
    assert_eq!(
        read_session_origin("https://app.example.com"),
        "https://app.example.com"
    );
    assert_eq!(
        read_session_origin("http://127.0.0.1:3001/"),
        "http://127.0.0.1:3001"
    );
    assert_eq!(read_session_origin("javascript:alert(1)"), "");
    assert_eq!(read_session_origin("https://app.example.com/secret"), "");
    assert_eq!(read_session_origin("https://app.example.com?x=1"), "");
    assert_eq!(read_session_origin("openchamber:"), "");
    assert_eq!(read_session_origin(""), "");
    assert_eq!(
        build_linear_session_open_url("ses_1", "https://app.example.com"),
        "https://app.example.com/?session=ses_1"
    );
    assert_eq!(build_linear_session_open_url("ses_1", ""), "");
}

#[test]
fn status_public_origin_classification() {
    assert!(is_public_session_origin("https://chamber.example.com"));
    assert!(is_public_session_origin("http://chamber.example.com:8080"));
    assert!(is_public_session_origin("https://203.0.113.10"));

    assert!(!is_public_session_origin("http://localhost:3001"));
    assert!(!is_public_session_origin("http://127.0.0.1:3001"));
    assert!(!is_public_session_origin("http://[::1]:3001"));
    assert!(!is_public_session_origin("http://192.168.1.20:3001"));
    assert!(!is_public_session_origin("http://10.0.0.5:3001"));
    assert!(!is_public_session_origin("http://172.20.1.4:3001"));
    assert!(!is_public_session_origin("http://169.254.10.1:3001"));
    assert!(!is_public_session_origin("http://100.101.102.103:3001"));
    assert!(!is_public_session_origin("http://macbook.local:3001"));
    assert!(!is_public_session_origin("http://macbook:3001"));
    assert!(!is_public_session_origin("http://[fd00::1]:3001"));
    assert!(!is_public_session_origin("openchamber:"));
    assert!(!is_public_session_origin(""));
}

#[test]
fn status_comment_body_is_one_link() {
    let url = "https://app.example.com/?session=ses_1";
    assert_eq!(
        build_linear_session_status_comment("started", url),
        "[OpenChamber session started](https://app.example.com/?session=ses_1)"
    );
    assert_eq!(
        build_linear_session_status_comment("completed", url),
        "[OpenChamber session completed](https://app.example.com/?session=ses_1)"
    );
    assert_eq!(
        build_linear_session_status_comment("failure", url),
        "[OpenChamber session failed](https://app.example.com/?session=ses_1)"
    );
    assert_eq!(
        build_linear_session_status_comment("started", ""),
        "OpenChamber session started"
    );
}

#[test]
fn status_prune_keeps_newest_by_insertion_order() {
    let mut records = Vec::new();
    // Deliberately non-alphabetical insertion order.
    for key in ["zeta", "alpha", "mike", "bravo", "kilo"] {
        records.push((
            key.to_string(),
            super::status::StatusRecord {
                issue_identifier: "ENG-12".into(),
                session_origin: None,
                organization_id: None,
                started: true,
                completed: false,
                failure: false,
            },
        ));
    }
    let pruned = prune_session_status_records(records, 3);
    let keys: Vec<&str> = pruned.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys, vec!["mike", "bravo", "kilo"]);
    // The written file preserves insertion order, so a reload prunes the same.
    let body = {
        let mut renderer = String::new();
        renderer.push_str("{\n");
        for (index, (key, _)) in pruned.iter().enumerate() {
            if index > 0 {
                renderer.push_str(",\n");
            }
            renderer.push_str(&format!("  \"{key}\": {{\n"));
            renderer.push_str("    \"issueIdentifier\": \"ENG-12\",\n");
            renderer.push_str("    \"started\": true\n");
            renderer.push_str("  }");
        }
        renderer.push_str("\n}");
        renderer
    };
    assert_eq!(
        super::top_level_key_order(&body),
        vec!["mike", "bravo", "kilo"]
    );
}

fn status_transport(comment_id: &str) -> Arc<FakeTransport> {
    let comment_id = comment_id.to_string();
    FakeTransport::new(move |request| {
        let body = body_json(request);
        if body["query"].as_str().unwrap().contains("GetLinearIssue") {
            return json_ok(json!({
                "data": {
                    "issue": {
                        "id": "issue-uuid-1",
                        "identifier": "ENG-12",
                        "title": "Broken login",
                        "url": "https://linear.app/openchamber/issue/ENG-12",
                        "state": { "name": "In Progress", "type": "started" },
                        "assignee": Value::Null,
                        "team": { "id": "team-eng", "key": "ENG", "name": "Engineering" },
                        "labels": { "nodes": [] },
                        "priority": Value::Null,
                        "description": null,
                        "comments": { "nodes": [] },
                    },
                },
            }));
        }
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("mutation CommentCreate")
        );
        assert_eq!(body["variables"]["input"]["issueId"], json!("issue-uuid-1"));
        json_ok(json!({
            "data": { "commentCreate": { "success": true, "comment": { "id": comment_id } } },
        }))
    })
}

async fn post_status(
    state: &Arc<LinearState>,
    kind: &str,
    session_id: &str,
    issue: &str,
    origin: &str,
) -> Result<Value, LinearError> {
    post_linear_session_status(
        state,
        &SessionStatusInput {
            kind: kind.into(),
            session_id: session_id.into(),
            issue_identifier: issue.into(),
            session_origin: origin.into(),
            ..SessionStatusInput::default()
        },
    )
    .await
}

#[tokio::test]
async fn status_skips_when_disabled_or_origin_not_public() {
    let (state, _dir) = state_with("status-skip", &[], transport_always_error());
    set_connected(&state, "access-1");
    // Turned off: no comment, still connected.
    state.set_session_comments_enabled(false);
    assert_eq!(
        post_status(
            &state,
            "started",
            "ses_1",
            "ENG-12",
            "https://app.example.com"
        )
        .await
        .unwrap(),
        json!({ "connected": true, "posted": false, "skipped": "disabled" })
    );
    state.set_session_comments_enabled(true);
    assert_eq!(
        post_status(
            &state,
            "started",
            "ses_1",
            "ENG-12",
            "http://127.0.0.1:3001"
        )
        .await
        .unwrap(),
        json!({ "connected": true, "posted": false, "skipped": "origin-not-public" })
    );
    assert_eq!(
        post_status(&state, "started", "ses_2", "ENG-12", "")
            .await
            .unwrap(),
        json!({ "connected": true, "posted": false, "skipped": "origin-not-public" })
    );
}

#[tokio::test]
async fn status_started_posts_once_and_dedupes() {
    let transport = status_transport("comment-1");
    let (state, _dir) = state_with("status-started", &[], transport.clone());
    set_connected(&state, "access-1");
    state.set_session_comments_enabled(true);

    let first = post_status(
        &state,
        "started",
        "ses_1",
        "ENG-12",
        "https://app.example.com",
    )
    .await
    .unwrap();
    assert_eq!(
        first,
        json!({ "connected": true, "posted": true, "commentId": "comment-1" })
    );
    let second = post_status(
        &state,
        "started",
        "ses_1",
        "ENG-12",
        "https://app.example.com",
    )
    .await
    .unwrap();
    assert_eq!(
        second,
        json!({ "connected": true, "posted": false, "skipped": "already-posted" })
    );

    let comment_calls: Vec<HttpRequest> = transport
        .calls()
        .into_iter()
        .filter(|call| {
            body_json(call)["query"]
                .as_str()
                .is_some_and(|q| q.contains("mutation CommentCreate"))
        })
        .collect();
    assert_eq!(comment_calls.len(), 1);
    let body = body_json(&comment_calls[0])["variables"]["input"]["body"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        body,
        "[OpenChamber session started](https://app.example.com/?session=ses_1)"
    );
    assert!(!first.to_string().contains("access-1"));
}
#[tokio::test]
async fn status_completed_reuses_stored_origin_and_requires_started() {
    let transport = status_transport("comment-done");
    let (state, _dir) = state_with("status-completed", &[], transport.clone());
    set_connected(&state, "access-1");
    state.set_session_comments_enabled(true);

    // A completed comment without a prior started record never posts.
    assert_eq!(
        post_status(&state, "completed", "ses_1", "", "")
            .await
            .unwrap(),
        json!({ "connected": true, "posted": false, "skipped": "not-started" })
    );

    post_status(
        &state,
        "started",
        "ses_1",
        "ENG-12",
        "https://app.example.com",
    )
    .await
    .unwrap();
    // `completed` reuses the stored issue and origin: no identifiers needed.
    let first = post_status(&state, "completed", "ses_1", "", "")
        .await
        .unwrap();
    assert_eq!(
        first,
        json!({ "connected": true, "posted": true, "commentId": "comment-done" })
    );
    let second = post_status(&state, "completed", "ses_1", "", "")
        .await
        .unwrap();
    assert_eq!(
        second,
        json!({ "connected": true, "posted": false, "skipped": "already-posted" })
    );

    let comment_calls: Vec<HttpRequest> = transport
        .calls()
        .into_iter()
        .filter(|call| {
            body_json(call)["query"]
                .as_str()
                .is_some_and(|q| q.contains("mutation CommentCreate"))
        })
        .collect();
    assert_eq!(comment_calls.len(), 2);
    let bodies: Vec<String> = comment_calls
        .iter()
        .map(|call| {
            body_json(call)["variables"]["input"]["body"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        bodies[1],
        "[OpenChamber session completed](https://app.example.com/?session=ses_1)"
    );
}

// ---------------------------------------------------------------------------
// status runtime
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runtime_posts_completed_once_on_idle() {
    let (state, _dir) = state_with("runtime-idle", &[], status_transport("started"));
    set_connected(&state, "access-1");
    state.set_session_comments_enabled(true);
    post_status(
        &state,
        "started",
        "ses_1",
        "ENG-12",
        "https://app.example.com",
    )
    .await
    .unwrap();

    let transport = status_transport("done");
    let (state, _dir) = state_with("runtime-idle-2", &[], transport.clone());
    set_connected(&state, "access-1");
    state.set_session_comments_enabled(true);
    // Seed the started record by posting through the same state.
    post_status(
        &state,
        "started",
        "ses_1",
        "ENG-12",
        "https://app.example.com",
    )
    .await
    .unwrap();

    let runtime = LinearSessionStatusRuntime::new(state.clone());
    let idle = json!({
        "type": "session.status",
        "properties": { "sessionID": "ses_1", "status": { "type": "idle" } },
    });
    runtime
        .process_payload(&idle)
        .expect("task")
        .await
        .expect("join");
    runtime
        .process_payload(&idle)
        .expect("task")
        .await
        .expect("join");

    let comment_calls: Vec<HttpRequest> = transport
        .calls()
        .into_iter()
        .filter(|call| {
            body_json(call)["query"]
                .as_str()
                .is_some_and(|q| q.contains("mutation CommentCreate"))
        })
        .collect();
    // One for `started`, one for the first idle; the second idle dedupes.
    assert_eq!(comment_calls.len(), 2);
    let bodies: Vec<String> = comment_calls
        .iter()
        .map(|call| {
            body_json(call)["variables"]["input"]["body"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(bodies.iter().any(|body| body.contains("session completed")));
    runtime.stop();
}

#[tokio::test]
async fn runtime_posts_failure_and_skips_user_abort() {
    let transport = status_transport("fail");
    let (state, _dir) = state_with("runtime-error", &[], transport.clone());
    set_connected(&state, "access-1");
    state.set_session_comments_enabled(true);
    post_status(
        &state,
        "started",
        "ses_1",
        "ENG-12",
        "https://app.example.com",
    )
    .await
    .unwrap();

    let runtime = LinearSessionStatusRuntime::new(state.clone());
    let abort = json!({
        "type": "session.error",
        "properties": { "sessionID": "ses_1", "error": { "name": "MessageAbortedError" } },
    });
    assert!(runtime.process_payload(&abort).is_none());

    let failure = json!({
        "type": "session.error",
        "properties": { "sessionID": "ses_1", "error": { "name": "ProviderError" } },
    });
    runtime
        .process_payload(&failure)
        .expect("task")
        .await
        .expect("join");
    let comment_calls: Vec<HttpRequest> = transport
        .calls()
        .into_iter()
        .filter(|call| {
            body_json(call)["query"]
                .as_str()
                .is_some_and(|q| q.contains("mutation CommentCreate"))
        })
        .collect();
    let bodies: Vec<String> = comment_calls
        .iter()
        .map(|call| {
            body_json(call)["variables"]["input"]["body"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("OpenChamber session failed"))
    );
    runtime.stop();
}

#[tokio::test]
async fn runtime_ignores_busy_and_stops() {
    let transport = status_transport("none");
    let (state, _dir) = state_with("runtime-busy", &[], transport.clone());
    set_connected(&state, "access-1");
    state.set_session_comments_enabled(true);
    let runtime = LinearSessionStatusRuntime::new(state.clone());
    let busy = json!({
        "type": "session.status",
        "properties": { "sessionID": "ses_1", "status": { "type": "busy" } },
    });
    assert!(runtime.process_payload(&busy).is_none());
    runtime.stop();
    let idle = json!({
        "type": "session.status",
        "properties": { "sessionID": "ses_1", "status": { "type": "idle" } },
    });
    assert!(runtime.process_payload(&idle).is_none());
    assert_eq!(transport.call_count(), 0);
}

// ---------------------------------------------------------------------------
// routes (oneshot)
// ---------------------------------------------------------------------------

fn route_state(tag: &str, transport: Arc<dyn HttpTransport>) -> (axum::Router, Arc<LinearState>) {
    let (state, _dir) = state_with(
        tag,
        &[(
            "OPENCHAMBER_LINEAR_REDIRECT_URI",
            "http://127.0.0.1:3001/linear/oauth/callback",
        )],
        transport,
    );
    (super::routes::router(state.clone()), state)
}

async fn get_json(app: axum::Router, uri: &str) -> (axum::http::StatusCode, Value) {
    let response = app
        .oneshot(
            axum::http::Request::get(uri)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    (status, body_json_response(response).await)
}

async fn send_json(
    app: axum::Router,
    method: &str,
    uri: &str,
    body: Value,
) -> (axum::http::StatusCode, Value) {
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    (status, body_json_response(response).await)
}

fn token_and_viewer_transport() -> Arc<FakeTransport> {
    FakeTransport::new(|request| match request.url.as_str() {
        "https://api.linear.app/oauth/token" => json_ok(json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "token_type": "Bearer",
            "expires_in": 86399,
            "scope": "read,write,comments:create",
        })),
        "https://api.linear.app/graphql" => json_ok(json!({
            "data": {
                "viewer": {
                    "id": "user-1",
                    "name": "Ada",
                    "displayName": "Ada Lovelace",
                    "email": "ada@example.com",
                    "avatarUrl": "https://example.com/a.png",
                },
                "organization": { "id": "org-1", "name": "OpenChamber", "urlKey": "openchamber" },
            },
        })),
        other => panic!("unexpected fetch: {other}"),
    })
}

async fn connect_through_callback(app: &axum::Router, origin: &str) -> String {
    let (status, payload) = send_json(
        app.clone(),
        "POST",
        "/api/linear/auth/start",
        json!({ "origin": origin }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let url = payload["authorizationUrl"].as_str().expect("url");
    url_query_param(url, "state")
}

#[tokio::test]
async fn route_full_oauth_flow_status_and_stable_authorized_at() {
    let (app, state) = route_state("route-flow", token_and_viewer_transport());
    let oauth_state = connect_through_callback(&app, "desktop").await;

    let callback = app
        .clone()
        .oneshot(
            axum::http::Request::get(format!(
                "/linear/oauth/callback?state={oauth_state}&code=auth-code"
            ))
            .body(Body::empty())
            .expect("request"),
        )
        .await
        .expect("callback");
    assert_eq!(callback.status(), axum::http::StatusCode::OK);
    let page = body_string(callback).await;
    assert!(page.contains("Authorization Complete"));
    assert!(page.contains("openchamber://focus/linear-auth"));

    let (status, body) = get_json(app.clone(), "/api/linear/auth/status").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["connected"], json!(true));
    assert_eq!(body["user"]["id"], json!("user-1"));
    assert_eq!(
        body["organization"],
        json!({ "id": "org-1", "name": "OpenChamber", "urlKey": "openchamber" })
    );
    assert_eq!(body["scope"], json!("read,write,comments:create"));
    let workspaces = body["workspaces"].as_array().expect("workspaces");
    assert_eq!(workspaces.len(), 1);
    assert_eq!(workspaces[0]["id"], json!("org-1"));
    assert_eq!(workspaces[0]["current"], json!(true));
    assert!(workspaces[0]["authorizedAt"].is_number());
    assert!(!body.to_string().contains("access-1"));
    assert!(!body.to_string().contains("refresh-1"));

    // Identity refresh does not bump authorizedAt (activate: false).
    let (_, again) = get_json(app.clone(), "/api/linear/auth/status").await;
    assert_eq!(
        again["workspaces"][0]["authorizedAt"],
        body["workspaces"][0]["authorizedAt"]
    );
    let _ = state;
}

#[tokio::test]
async fn route_unknown_state_is_400_page_without_deep_link() {
    let (app, _state) = route_state("route-unknown", transport_always_error());
    let response = app
        .oneshot(
            axum::http::Request::get("/linear/oauth/callback?state=forged&code=attacker-code")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("callback");
    assert_eq!(response.status(), axum::http::StatusCode::BAD_REQUEST);
    let page = body_string(response).await;
    assert!(page.contains("Authorization Failed"));
    assert!(!page.contains("openchamber://"));
}

#[tokio::test]
async fn route_web_origin_omits_desktop_deep_link() {
    let (app, _state) = route_state("route-web", token_and_viewer_transport());
    let oauth_state = connect_through_callback(&app, "web").await;
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::get(format!(
                "/linear/oauth/callback?state={oauth_state}&code=auth-code"
            ))
            .body(Body::empty())
            .expect("request"),
        )
        .await
        .expect("callback");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let page = body_string(response).await;
    assert!(page.contains("Authorization Complete"));
    assert!(!page.contains("openchamber://"));
}

#[tokio::test]
async fn route_disconnect_revokes_refresh_token() {
    let transport = FakeTransport::new(|request| match request.url.as_str() {
        "https://api.linear.app/oauth/token" => json_ok(json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "expires_in": 86399,
        })),
        url if url.contains("/graphql") => json_ok(json!({
            "data": { "viewer": { "id": "user-1", "name": "Ada" }, "organization": Value::Null },
        })),
        url if url.contains("/oauth/revoke") => Ok(HttpResponse {
            status: 200,
            body: Vec::new(),
        }),
        other => panic!("unexpected fetch: {other}"),
    });
    let (app, _state) = route_state("route-disconnect", transport.clone());
    let oauth_state = connect_through_callback(&app, "web").await;
    let callback = app
        .clone()
        .oneshot(
            axum::http::Request::get(format!(
                "/linear/oauth/callback?state={oauth_state}&code=auth-code"
            ))
            .body(Body::empty())
            .expect("request"),
        )
        .await
        .expect("callback");
    assert_eq!(callback.status(), axum::http::StatusCode::OK);

    let (status, body) = send_json(app.clone(), "DELETE", "/api/linear/auth", json!({})).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, json!({ "success": true, "removed": true }));

    let revoke = transport
        .calls()
        .into_iter()
        .find(|call| call.url.contains("/oauth/revoke"))
        .expect("revoke call");
    assert_eq!(form_get(&revoke, "token").as_deref(), Some("refresh-1"));
    assert_eq!(
        form_get(&revoke, "token_type_hint").as_deref(),
        Some("refresh_token")
    );

    let (status, body) = get_json(app.clone(), "/api/linear/auth/status").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body, json!({ "connected": false }));
}

#[tokio::test]
async fn route_two_workspaces_switch_and_partial_disconnect() {
    let transport = FakeTransport::new(|request| {
        if request.url == "https://api.linear.app/oauth/token" {
            let code = form_get(request, "code").unwrap_or_default();
            let (access, refresh) = if code == "code-a" {
                ("access-a", "refresh-a")
            } else {
                ("access-b", "refresh-b")
            };
            return json_ok(json!({
                "access_token": access,
                "refresh_token": refresh,
                "expires_in": 86399,
                "scope": "read,write,comments:create",
            }));
        }
        let token = request.header("Authorization").unwrap_or_default();
        let (user, org) = if token == "Bearer access-a" {
            ("user-a", "org-a")
        } else {
            ("user-b", "org-b")
        };
        json_ok(json!({
            "data": {
                "viewer": { "id": user, "name": user },
                "organization": { "id": org, "name": org, "urlKey": org },
            },
        }))
    });
    let (app, _state) = route_state("route-two", transport);

    for code in ["code-a", "code-b"] {
        let (_, start) = send_json(app.clone(), "POST", "/api/linear/auth/start", json!({})).await;
        let url = start["authorizationUrl"].as_str().expect("url");
        let oauth_state = url_query_param(url, "state");
        let callback = app
            .clone()
            .oneshot(
                axum::http::Request::get(format!(
                    "/linear/oauth/callback?state={oauth_state}&code={code}"
                ))
                .body(Body::empty())
                .expect("request"),
            )
            .await
            .expect("callback");
        assert_eq!(callback.status(), axum::http::StatusCode::OK);
    }

    let (status, both) = get_json(app.clone(), "/api/linear/auth/status").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(both["organization"]["id"], json!("org-b"));
    assert_eq!(both["workspaces"].as_array().map(Vec::len), Some(2));

    let (status, _) = send_json(app.clone(), "POST", "/api/linear/auth/activate", json!({})).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    let (status, body) = send_json(
        app.clone(),
        "POST",
        "/api/linear/auth/activate",
        json!({ "organizationId": "missing" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(body["error"], json!("Linear workspace not found"));

    let (status, activated) = send_json(
        app.clone(),
        "POST",
        "/api/linear/auth/activate",
        json!({ "organizationId": "org-a" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(activated["organization"]["id"], json!("org-a"));

    let (status, _) = send_json(app.clone(), "DELETE", "/api/linear/auth", json!({})).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let (status, remaining) = get_json(app, "/api/linear/auth/status").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(remaining["connected"], json!(true));
    assert_eq!(remaining["organization"]["id"], json!("org-b"));
    assert_eq!(remaining["workspaces"].as_array().map(Vec::len), Some(1));
}

#[tokio::test]
async fn route_issues_list_get_and_validation_errors() {
    let transport = FakeTransport::new(|request| {
        let body = body_json(request);
        if body["query"]
            .as_str()
            .unwrap()
            .contains("TeamWorkflowStates")
        {
            return json_ok(json!({
                "data": { "team": { "states": { "nodes": [
                    { "id": "state-todo", "name": "Todo", "type": "unstarted", "position": 1 },
                    { "id": "state-done", "name": "Done", "type": "completed", "position": 2 },
                ]}}},
            }));
        }
        if body["query"].as_str().unwrap().contains("GetLinearIssue") {
            return json_ok(json!({
                "data": { "issue": {
                    "id": "issue-1",
                    "identifier": "ENG-12",
                    "title": "Broken login",
                    "url": "https://linear.app/openchamber/issue/ENG-12",
                    "state": { "name": "Todo", "type": "unstarted" },
                    "assignee": Value::Null,
                    "team": { "id": "team-eng", "key": "ENG", "name": "Engineering" },
                    "labels": { "nodes": [] },
                    "priority": Value::Null,
                    "description": "Users cannot sign in.",
                    "comments": { "nodes": [] },
                }},
            }));
        }
        if body["query"]
            .as_str()
            .unwrap()
            .contains("mutation IssueUpdate")
        {
            return json_ok(json!({
                "data": { "issueUpdate": { "success": true, "issue": {
                    "id": "issue-uuid-1",
                    "identifier": "ENG-12",
                    "title": "Broken login",
                    "url": "https://linear.app/openchamber/issue/ENG-12",
                    "state": { "id": "state-done", "name": "Done", "type": "completed" },
                    "assignee": Value::Null,
                    "team": { "id": "team-eng", "key": "ENG", "name": "Engineering" },
                    "labels": { "nodes": [] },
                    "priority": Value::Null,
                    "description": null,
                    "comments": { "nodes": [] },
                }}},
            }));
        }
        json_ok(json!({
            "data": { "issues": {
                "nodes": [{
                    "id": "issue-1",
                    "identifier": "ENG-12",
                    "title": "Broken login",
                    "url": "https://linear.app/openchamber/issue/ENG-12",
                    "state": { "name": "Todo", "type": "unstarted" },
                    "assignee": Value::Null,
                    "team": { "id": "team-eng", "key": "ENG", "name": "Engineering" },
                    "labels": { "nodes": [] },
                    "priority": Value::Null,
                }],
                "pageInfo": { "hasNextPage": false, "endCursor": null },
            }},
        }))
    });
    let (app, state) = route_state("route-issues", transport);
    set_connected(&state, "access-1");

    let (status, list) = get_json(app.clone(), "/api/linear/issues/list").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(list["connected"], json!(true));
    assert_eq!(list["issues"].as_array().map(Vec::len), Some(1));

    let (status, missing) = get_json(app.clone(), "/api/linear/issues/get").await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(missing["error"], json!("id is required"));

    let (status, got) = get_json(app.clone(), "/api/linear/issues/get?id=ENG-12").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(got["issue"]["description"], json!("Users cannot sign in."));
    assert_eq!(
        got["issue"]["state"],
        json!({ "id": Value::Null, "name": "Todo", "type": "unstarted" })
    );

    let (status, missing_team) = get_json(app.clone(), "/api/linear/issues/states").await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(missing_team["error"], json!("teamId is required"));

    let (status, states) = get_json(app.clone(), "/api/linear/issues/states?teamId=team-eng").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        states["states"],
        json!([
            { "id": "state-todo", "name": "Todo", "type": "unstarted", "position": 1 },
            { "id": "state-done", "name": "Done", "type": "completed", "position": 2 },
        ])
    );

    let (status, missing_body) =
        send_json(app.clone(), "POST", "/api/linear/issues/update", json!({})).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(missing_body["error"], json!("id and stateId are required"));

    let (status, updated) = send_json(
        app.clone(),
        "POST",
        "/api/linear/issues/update",
        json!({ "id": "issue-uuid-1", "stateId": "state-done" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(updated["connected"], json!(true));
    assert_eq!(updated["issue"]["identifier"], json!("ENG-12"));
}

#[tokio::test]
async fn route_linear_validation_errors_are_400() {
    let transport = FakeTransport::new(|request| {
        let body = body_json(request);
        if body["query"]
            .as_str()
            .unwrap()
            .contains("TeamWorkflowStates")
        {
            return json_ok(json!({
                "data": Value::Null,
                "errors": [{
                    "message": "Entity not found: Team",
                    "extensions": {
                        "code": "INPUT_ERROR",
                        "userError": true,
                        "userPresentableMessage": "Could not find referenced Team.",
                    },
                }],
            }));
        }
        json_ok(json!({
            "data": Value::Null,
            "errors": [{
                "message": "Argument Validation Error",
                "extensions": {
                    "code": "INVALID_INPUT",
                    "userError": true,
                    "userPresentableMessage": "stateId must be a UUID.",
                },
            }],
        }))
    });
    let (app, state) = route_state("route-validation", transport);
    set_connected(&state, "access-1");

    let (status, states) =
        get_json(app.clone(), "/api/linear/issues/states?teamId=missing-team").await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(states["error"], json!("Could not find referenced Team."));

    let (status, updated) = send_json(
        app,
        "POST",
        "/api/linear/issues/update",
        json!({ "id": "issue-uuid-1", "stateId": "not-a-uuid" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(updated["error"], json!("stateId must be a UUID."));
}

#[tokio::test]
async fn route_disconnected_shapes() {
    let (app, _state) = route_state("route-disconnected", transport_always_error());
    let (status, list) = get_json(app.clone(), "/api/linear/issues/list").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(list, json!({ "connected": false }));

    let (status, got) = get_json(app.clone(), "/api/linear/issues/get?id=ENG-12").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(got, json!({ "connected": false }));

    let (status, states) = get_json(app.clone(), "/api/linear/issues/states?teamId=team-eng").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(states, json!({ "connected": false }));

    let (status, updated) = send_json(
        app.clone(),
        "POST",
        "/api/linear/issues/update",
        json!({ "id": "issue-1", "stateId": "state-done" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(updated, json!({ "connected": false }));

    let (status, mapping) = get_json(app.clone(), "/api/linear/mapping").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(mapping, json!({ "connected": false }));

    let (status, saved) = send_json(
        app.clone(),
        "PUT",
        "/api/linear/mapping",
        json!({ "defaultProjectPath": "/tmp/project", "teamProjectPaths": {} }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(saved, json!({ "connected": false }));

    let (status, session) = send_json(
        app,
        "POST",
        "/api/linear/session-status",
        json!({ "kind": "started", "sessionId": "ses_1", "issueIdentifier": "ENG-12" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(session, json!({ "connected": false }));
}

#[tokio::test]
async fn route_mapping_roundtrip() {
    let transport = FakeTransport::new(|_| {
        json_ok(json!({
            "data": { "teams": {
                "nodes": [
                    { "id": "team-eng", "key": "ENG", "name": "Engineering" },
                    { "id": "team-des", "key": "DES", "name": "Design" },
                ],
                "pageInfo": { "hasNextPage": false, "endCursor": null },
            }},
        }))
    });
    let (app, state) = route_state("route-mapping", transport);
    set_connected(&state, "access-1");

    let (status, empty) = get_json(app.clone(), "/api/linear/mapping").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        empty,
        json!({
            "connected": true,
            "defaultProjectPath": Value::Null,
            "teams": [
                { "id": "team-eng", "key": "ENG", "name": "Engineering", "projectPath": Value::Null },
                { "id": "team-des", "key": "DES", "name": "Design", "projectPath": Value::Null },
            ],
        })
    );

    let (status, saved) = send_json(
        app.clone(),
        "PUT",
        "/api/linear/mapping",
        json!({
            "defaultProjectPath": "/Users/ada/openchamber",
            "teamProjectPaths": { "team-eng": "/Users/ada/eng" },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        saved,
        json!({
            "connected": true,
            "defaultProjectPath": "/Users/ada/openchamber",
            "teams": [
                { "id": "team-eng", "key": "ENG", "name": "Engineering", "projectPath": "/Users/ada/eng" },
                { "id": "team-des", "key": "DES", "name": "Design", "projectPath": Value::Null },
            ],
        })
    );

    let (_, reread) = get_json(app, "/api/linear/mapping").await;
    assert_eq!(
        reread["defaultProjectPath"],
        json!("/Users/ada/openchamber")
    );
    assert_eq!(reread["teams"][0]["projectPath"], json!("/Users/ada/eng"));

    // `state` keeps ownership of the router's data dir for other tests.
    let _ = state;
}

#[tokio::test]
async fn route_session_status_post_shape() {
    let transport = status_transport("comment-1");
    let (app, state) = route_state("route-session", transport);
    set_connected(&state, "access-1");
    state.set_session_comments_enabled(true);

    let (status, missing) = send_json(
        app.clone(),
        "POST",
        "/api/linear/session-status",
        json!({ "kind": "started" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(missing["error"], json!("kind and sessionId are required"));

    let (status, posted) = send_json(
        app,
        "POST",
        "/api/linear/session-status",
        json!({
            "kind": "started",
            "sessionId": "ses_1",
            "issueIdentifier": "ENG-12",
            "sessionOrigin": "https://app.example.com",
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(
        posted,
        json!({ "connected": true, "posted": true, "commentId": "comment-1" })
    );
}

#[tokio::test]
async fn route_preferences_roundtrip_and_validation() {
    let (app, _state) = route_state("route-prefs", transport_always_error());
    let (status, initial) = get_json(app.clone(), "/api/linear/preferences").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(initial, json!({ "sessionComments": false }));

    let (status, invalid) = send_json(
        app.clone(),
        "PUT",
        "/api/linear/preferences",
        json!({ "sessionComments": "yes" }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(invalid["error"], json!("sessionComments must be a boolean"));

    let (status, enabled) = send_json(
        app.clone(),
        "PUT",
        "/api/linear/preferences",
        json!({ "sessionComments": true }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(enabled, json!({ "sessionComments": true }));
    let (_, reread) = get_json(app, "/api/linear/preferences").await;
    assert_eq!(reread, json!({ "sessionComments": true }));
}

#[tokio::test]
async fn route_mapping_invalid_body_is_400() {
    let transport = FakeTransport::new(|_| {
        json_ok(json!({
            "data": { "teams": { "nodes": [], "pageInfo": { "hasNextPage": false, "endCursor": null } } },
        }))
    });
    let (app, state) = route_state("route-mapping-invalid", transport);
    set_connected(&state, "access-1");
    let (status, body) = send_json(app, "PUT", "/api/linear/mapping", json!(null)).await;
    assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], json!("Mapping body must be an object"));
}
