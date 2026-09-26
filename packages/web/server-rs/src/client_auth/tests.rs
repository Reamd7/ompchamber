//! Tests for the client-auth port, mirroring `remote-clients.test.js`,
//! `pairing.test.js`, and the tunnel-auth behaviors exercised by
//! `core-routes.test.js`.

use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use futures::future::BoxFuture;

use super::pairing::{ClientIssuer, ClientPairing, CreatedClientRef, GENERIC_REDEEM_ERROR};
use super::remote_clients::{CreateClientInput, PublicClient, RemoteClientAuth, Transport};
use super::time::{Clock, system_clock};
use super::tunnel_auth::{RequestScope, TunnelAuth, TunnelRequestContext};
use crate::error::AppResult;

fn temp_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ompchamber-client-auth-{label}-{}-{:x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn label(value: &str) -> Option<String> {
    Some(value.to_string())
}

// -- remote clients ---------------------------------------------------------

#[tokio::test]
async fn creates_authenticates_lists_and_revokes_client_tokens() {
    let dir = temp_dir("lifecycle");
    let runtime = RemoteClientAuth::new(dir.join("remote-clients.json"), system_clock());

    let created = runtime
        .create_client(CreateClientInput {
            label: label("Laptop"),
            ..Default::default()
        })
        .await
        .expect("create");
    assert!(created.token.starts_with("oc_client_"));
    assert_eq!(created.client.label, "Laptop");

    let listed = runtime.list_clients().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.client.id);
    // The public shape never leaks the hash.
    let serialized = serde_json::to_value(&listed[0]).expect("serialize");
    assert!(serialized.get("tokenHash").is_none());

    let authenticated = runtime
        .authenticate_bearer_token(&created.token, Transport::Direct)
        .await
        .expect("authenticate")
        .expect("valid token");
    assert!(authenticated.ok);
    assert_eq!(authenticated.client_id, created.client.id);
    assert_eq!(authenticated.session_token, created.client.id);

    let after_use = runtime.list_clients().await.expect("list");
    assert!(after_use[0].last_used_at.is_some());

    let revoked = runtime
        .revoke_client(Some(&created.client.id))
        .await
        .expect("revoke");
    assert!(revoked.revoked);
    assert!(
        runtime
            .authenticate_bearer_token(&created.token, Transport::Direct)
            .await
            .expect("authenticate")
            .is_none()
    );

    let purged = runtime.purge_revoked_clients().await.expect("purge");
    assert_eq!(purged.purged, 1);
    assert_eq!(runtime.list_clients().await.expect("list").len(), 0);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn rejects_expired_client_tokens() {
    let dir = temp_dir("expired");
    let runtime = RemoteClientAuth::new(dir.join("remote-clients.json"), system_clock());

    let expired = runtime
        .create_client(CreateClientInput {
            label: label("Expired"),
            expires_at: label("2000-01-01T00:00:00.000Z"),
            ..Default::default()
        })
        .await
        .expect("create");
    assert_eq!(
        expired.client.expires_at.as_deref(),
        Some("2000-01-01T00:00:00.000Z")
    );
    assert!(
        runtime
            .authenticate_bearer_token(&expired.token, Transport::Direct)
            .await
            .expect("authenticate")
            .is_none()
    );

    let active = runtime
        .create_client(CreateClientInput {
            label: label("Active"),
            expires_at: label("2999-01-01T00:00:00.000Z"),
            ..Default::default()
        })
        .await
        .expect("create");
    let authenticated = runtime
        .authenticate_bearer_token(&active.token, Transport::Direct)
        .await
        .expect("authenticate")
        .expect("valid");
    assert_eq!(authenticated.client_id, active.client.id);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn keeps_one_client_per_dedupe_key() {
    let dir = temp_dir("dedupe");
    let runtime = RemoteClientAuth::new(dir.join("remote-clients.json"), system_clock());

    let first = runtime
        .create_client(CreateClientInput {
            label: label("Desktop"),
            client_kind: label("desktop-local"),
            dedupe_key: label("desktop-local"),
            ..Default::default()
        })
        .await
        .expect("create");
    let second = runtime
        .create_client(CreateClientInput {
            label: label("Desktop"),
            client_kind: label("desktop-local"),
            dedupe_key: label("desktop-local"),
            ..Default::default()
        })
        .await
        .expect("create");

    assert!(
        runtime
            .authenticate_bearer_token(&first.token, Transport::Direct)
            .await
            .expect("authenticate")
            .is_none()
    );
    assert!(
        runtime
            .authenticate_bearer_token(&second.token, Transport::Direct)
            .await
            .expect("authenticate")
            .is_some()
    );

    let listed = runtime.list_clients().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, second.client.id);
    assert_eq!(listed[0].client_kind.as_deref(), Some("desktop-local"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn keeps_the_replaced_record_label_on_a_dedupe_re_mint() {
    let dir = temp_dir("relabel");
    let runtime = RemoteClientAuth::new(dir.join("remote-clients.json"), system_clock());

    runtime
        .create_client(CreateClientInput {
            label: label("Iryna iPhone"),
            dedupe_key: label("mobile:device-1"),
            fallback_label: label("OMPChamber Mobile"),
            ..Default::default()
        })
        .await
        .expect("create");
    let remint = runtime
        .create_client(CreateClientInput {
            dedupe_key: label("mobile:device-1"),
            fallback_label: label("OMPChamber Mobile"),
            ..Default::default()
        })
        .await
        .expect("create");
    assert_eq!(remint.client.label, "Iryna iPhone");

    let renamed = runtime
        .create_client(CreateClientInput {
            label: label("Work phone"),
            dedupe_key: label("mobile:device-1"),
            fallback_label: label("OMPChamber Mobile"),
            ..Default::default()
        })
        .await
        .expect("create");
    assert_eq!(renamed.client.label, "Work phone");

    let fresh = runtime
        .create_client(CreateClientInput {
            dedupe_key: label("mobile:device-2"),
            fallback_label: label("OMPChamber Mobile"),
            ..Default::default()
        })
        .await
        .expect("create");
    assert_eq!(fresh.client.label, "OMPChamber Mobile");

    std::fs::remove_dir_all(&dir).ok();
}

#[cfg(unix)]
#[tokio::test]
async fn keeps_the_token_store_private_on_disk() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir("private");
    let runtime = RemoteClientAuth::new(dir.join("remote-clients.json"), system_clock());
    runtime
        .create_client(CreateClientInput {
            label: label("Laptop"),
            ..Default::default()
        })
        .await
        .expect("create");
    let mode = std::fs::metadata(dir.join("remote-clients.json"))
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn registry_roundtrips_across_runtime_instances() {
    let dir = temp_dir("roundtrip");
    let store = dir.join("remote-clients.json");
    let created = RemoteClientAuth::new(store.clone(), system_clock())
        .create_client(CreateClientInput {
            label: label("Laptop"),
            ..Default::default()
        })
        .await
        .expect("create");

    // A fresh runtime (another module, or a restart) reads the same disk
    // store, and the persisted shape round-trips.
    let reopened = RemoteClientAuth::new(store, system_clock());
    let listed = reopened.list_clients().await.expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, created.client.id);
    assert_eq!(listed[0].label, "Laptop");
    let raw = std::fs::read_to_string(dir.join("remote-clients.json")).expect("read");
    let payload: serde_json::Value = serde_json::from_str(&raw).expect("parse");
    assert_eq!(payload["version"], 1);
    assert_eq!(
        payload["clients"][0]["tokenHash"].as_str().map(str::len),
        Some(64)
    );
    assert_eq!(payload["clients"][0]["usesRelay"], false);

    // And the token still authenticates after reopen.
    assert!(
        reopened
            .authenticate_bearer_token(&created.token, Transport::Direct)
            .await
            .expect("authenticate")
            .is_some()
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn self_heals_uses_relay_when_a_request_arrives_through_the_relay_tunnel() {
    let dir = temp_dir("relay-heal");
    let runtime = RemoteClientAuth::new(dir.join("remote-clients.json"), system_clock());

    let created = runtime
        .create_client(CreateClientInput {
            label: label("Phone"),
            ..Default::default()
        })
        .await
        .expect("create");
    assert!(!created.client.uses_relay);
    assert!(!runtime.has_active_relay_clients().await.expect("relay"));

    let authenticated = runtime
        .authenticate_bearer_token(&created.token, Transport::Relay)
        .await
        .expect("authenticate")
        .expect("valid");
    assert!(authenticated.client.uses_relay);
    assert!(runtime.has_active_relay_clients().await.expect("relay"));

    // Sticky: a later direct request must not clear relay demand.
    runtime
        .authenticate_bearer_token(&created.token, Transport::Direct)
        .await
        .expect("authenticate");
    let listed = runtime.list_clients().await.expect("list");
    assert!(listed[0].uses_relay);
    assert_eq!(listed[0].last_transport.as_deref(), Some("direct"));
    assert!(runtime.has_active_relay_clients().await.expect("relay"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn counts_an_observed_relay_transport_as_relay_demand() {
    let dir = temp_dir("relay-demand");
    let store = dir.join("remote-clients.json");
    let runtime = RemoteClientAuth::new(store.clone(), system_clock());
    let created = runtime
        .create_client(CreateClientInput {
            label: label("Tablet"),
            ..Default::default()
        })
        .await
        .expect("create");

    // Simulate a store written by a build that tracked lastTransport but not
    // the healed usesRelay flag.
    let mut payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&store).expect("read")).expect("parse");
    payload["clients"][0]["lastTransport"] = serde_json::json!("relay");
    std::fs::write(&store, serde_json::to_string(&payload).expect("serialize")).expect("write");

    assert!(!created.client.uses_relay);
    assert!(runtime.has_active_relay_clients().await.expect("relay"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn does_not_resurrect_revoked_clients_after_concurrent_traffic() {
    let dir = temp_dir("concurrent");
    let runtime = Arc::new(RemoteClientAuth::new(
        dir.join("remote-clients.json"),
        system_clock(),
    ));
    let created = runtime
        .create_client(CreateClientInput {
            label: label("Laptop"),
            ..Default::default()
        })
        .await
        .expect("create");

    let mut tasks: Vec<tokio::task::JoinHandle<AppResult<()>>> = Vec::new();
    for _ in 0..20 {
        let runtime = runtime.clone();
        let token = created.token.clone();
        tasks.push(tokio::spawn(async move {
            runtime
                .authenticate_bearer_token(&token, Transport::Direct)
                .await
                .map(|_| ())
        }));
    }
    let runtime_for_revoke = runtime.clone();
    let id = created.client.id.clone();
    tasks.push(tokio::spawn(async move {
        runtime_for_revoke
            .revoke_client(Some(&id))
            .await
            .map(|_| ())
    }));
    for task in tasks {
        task.await.expect("join").expect("op");
    }

    assert!(
        runtime
            .authenticate_bearer_token(&created.token, Transport::Direct)
            .await
            .expect("authenticate")
            .is_none()
    );
    let clients = runtime.list_clients().await.expect("list");
    assert_eq!(clients.len(), 1);
    assert!(clients[0].revoked_at.is_some());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn is_valid_client_token_gates_on_prefix_hash_and_state() {
    let dir = temp_dir("validity");
    let runtime = RemoteClientAuth::new(dir.join("remote-clients.json"), system_clock());
    let created = runtime
        .create_client(CreateClientInput::default())
        .await
        .expect("create");

    assert!(runtime.is_valid_client_token(&created.token).await);
    // Wrong token, wrong secret material, and an empty probe fail closed.
    let mutated = format!("{}x", created.token);
    assert!(!runtime.is_valid_client_token(&mutated).await);
    assert!(!runtime.is_valid_client_token("oc_client_nope").await);
    assert!(!runtime.is_valid_client_token("").await);

    std::fs::remove_dir_all(&dir).ok();
}

// -- pairing ----------------------------------------------------------------

/// Fake issuer mirroring the `pairing.test.js` mock, with scriptable results
/// so a failing first issuance can leave the session unconsumed.
type ScriptedIssue = Vec<Result<(String, String), String>>;

struct FakeIssuer {
    next_id: Mutex<usize>,
    calls: Mutex<Vec<CreateClientInput>>,
    results: Mutex<ScriptedIssue>,
}

impl FakeIssuer {
    fn with(results: ScriptedIssue) -> Arc<Self> {
        Arc::new(Self {
            next_id: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(results),
        })
    }

    fn calls(&self) -> Vec<CreateClientInput> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn public_client_stub(id: &str, input: &CreateClientInput) -> PublicClient {
    PublicClient {
        id: id.to_string(),
        label: input
            .label
            .clone()
            .or_else(|| input.fallback_label.clone())
            .unwrap_or_else(|| "Remote client".to_string()),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        last_used_at: None,
        revoked_at: None,
        expires_at: None,
        client_kind: input.client_kind.clone(),
        auth_method: input.auth_method.clone(),
        pairing_id: input.pairing_id.clone(),
        device_name: input.device_name.clone(),
        device_platform: input.device_platform.clone(),
        device_model: input.device_model.clone(),
        app_version: input.app_version.clone(),
        uses_relay: input.uses_relay,
        last_transport: None,
    }
}

impl ClientIssuer for FakeIssuer {
    fn create_client<'a>(
        &'a self,
        input: CreateClientInput,
    ) -> BoxFuture<'a, AppResult<CreatedClientRef>> {
        let mut next_id = self.next_id.lock().unwrap_or_else(|e| e.into_inner());
        *next_id += 1;
        let index = *next_id;
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(input.clone());
        let result = self
            .results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index - 1)
            .cloned()
            .unwrap_or(Ok(("client-x".to_string(), "token-x".to_string())));
        Box::pin(async move {
            match result {
                Ok((client_id, token)) => Ok(CreatedClientRef {
                    client_id: Some(client_id.clone()),
                    client: public_client_stub(&client_id, &input),
                    token,
                }),
                Err(message) => Err(crate::error::AppError::internal(message)),
            }
        })
    }
}

fn pairing_runtime(
    dir: &std::path::Path,
    ttl_ms: i64,
    issuer: Arc<dyn ClientIssuer>,
) -> ClientPairing {
    ClientPairing::new(
        dir.join("client-pairing-sessions.json"),
        system_clock(),
        ttl_ms,
        issuer,
    )
}

fn redeem_error(result: &AppResult<super::pairing::RedeemedPairing>) -> String {
    match result {
        Ok(_) => "ok".to_string(),
        Err(err) => err.to_string(),
    }
}

#[tokio::test]
async fn redeems_a_pairing_session_once_and_propagates_client_metadata() {
    let dir = temp_dir("pairing-redeem");
    let issuer = FakeIssuer::with(vec![Ok(("client-1".to_string(), "token-1".to_string()))]);
    let runtime = pairing_runtime(&dir, super::pairing::DEFAULT_TTL_MS, issuer.clone());

    let created = runtime
        .create_pairing_session(super::pairing::CreatePairingInput {
            allowed_client_kinds: Some(vec!["mobile".to_string()]),
            ..Default::default()
        })
        .await
        .expect("create");
    assert!(created.pairing.session.id.starts_with("pair_"));
    assert_eq!(created.pairing.session.label, "Pair new device");
    assert_eq!(
        created.pairing.session.allowed_client_kinds,
        vec!["mobile".to_string()]
    );
    // 32 random bytes, base64url, unpadded.
    assert_eq!(created.pairing.secret.len(), 43);

    let result = runtime
        .redeem_pairing_session(super::pairing::RedeemPairingInput {
            pairing_id: Some(created.pairing.session.id.clone()),
            secret: Some(created.pairing.secret.clone()),
            client_label: Some("Iryna iPhone".to_string()),
            client_kind: Some("mobile".to_string()),
            device_name: Some("Iryna iPhone".to_string()),
            dedupe_key: Some("device-key".to_string()),
            ..Default::default()
        })
        .await
        .expect("redeem");

    assert_eq!(result.token, "token-1");
    assert_eq!(result.client.label, "Iryna iPhone");
    assert_eq!(result.client.client_kind.as_deref(), Some("mobile"));
    assert_eq!(result.client.auth_method.as_deref(), Some("pairing"));
    assert_eq!(
        result.client.pairing_id.as_deref(),
        Some(created.pairing.session.id.as_str())
    );
    assert_eq!(result.client.device_name.as_deref(), Some("Iryna iPhone"));
    assert_eq!(result.pairing.client_id.as_deref(), Some("client-1"));

    let calls = issuer.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.auth_method.as_deref(), Some("pairing"));
    assert_eq!(
        call.pairing_id.as_deref(),
        Some(created.pairing.session.id.as_str())
    );
    assert_eq!(call.client_kind.as_deref(), Some("mobile"));
    assert_eq!(call.dedupe_key.as_deref(), Some("device-key"));
    // No operator-typed pairing label: the app-reported name is only a
    // fallback so a re-pair keeps the existing device record's label.
    assert_eq!(call.label, None);
    assert_eq!(call.fallback_label.as_deref(), Some("Iryna iPhone"));

    // One-time semantics: a second redeem is a generic 400.
    let second = runtime
        .redeem_pairing_session(super::pairing::RedeemPairingInput {
            pairing_id: Some(created.pairing.session.id.clone()),
            secret: Some(created.pairing.secret.clone()),
            client_kind: Some("mobile".to_string()),
            ..Default::default()
        })
        .await;
    assert_eq!(redeem_error(&second), GENERIC_REDEEM_ERROR);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn rejects_expired_cancelled_wrong_secret_and_disallowed_kind_redemption() {
    // Expired: negative TTL.
    let dir = temp_dir("pairing-expired");
    let runtime = pairing_runtime(&dir, -1000, FakeIssuer::with(Vec::new()));
    let expired = runtime
        .create_pairing_session(Default::default())
        .await
        .expect("create");
    let result = runtime
        .redeem_pairing_session(super::pairing::RedeemPairingInput {
            pairing_id: Some(expired.pairing.session.id.clone()),
            secret: Some(expired.pairing.secret.clone()),
            client_kind: Some("mobile".to_string()),
            ..Default::default()
        })
        .await;
    assert_eq!(redeem_error(&result), GENERIC_REDEEM_ERROR);
    std::fs::remove_dir_all(&dir).ok();

    // Cancelled.
    let dir = temp_dir("pairing-cancelled");
    let runtime = pairing_runtime(
        &dir,
        super::pairing::DEFAULT_TTL_MS,
        FakeIssuer::with(Vec::new()),
    );
    let cancelled = runtime
        .create_pairing_session(Default::default())
        .await
        .expect("create");
    let cancel = runtime
        .cancel_pairing_session(Some(&cancelled.pairing.session.id))
        .await
        .expect("cancel");
    assert!(cancel.cancelled);
    let result = runtime
        .redeem_pairing_session(super::pairing::RedeemPairingInput {
            pairing_id: Some(cancelled.pairing.session.id.clone()),
            secret: Some(cancelled.pairing.secret.clone()),
            client_kind: Some("mobile".to_string()),
            ..Default::default()
        })
        .await;
    assert_eq!(redeem_error(&result), GENERIC_REDEEM_ERROR);
    assert!(
        runtime
            .list_pending_sessions()
            .await
            .expect("pending")
            .is_empty()
    );
    std::fs::remove_dir_all(&dir).ok();

    // Wrong secret.
    let dir = temp_dir("pairing-secret");
    let runtime = pairing_runtime(
        &dir,
        super::pairing::DEFAULT_TTL_MS,
        FakeIssuer::with(Vec::new()),
    );
    let wrong_secret = runtime
        .create_pairing_session(Default::default())
        .await
        .expect("create");
    let result = runtime
        .redeem_pairing_session(super::pairing::RedeemPairingInput {
            pairing_id: Some(wrong_secret.pairing.session.id.clone()),
            secret: Some("wrong".to_string()),
            client_kind: Some("mobile".to_string()),
            ..Default::default()
        })
        .await;
    assert_eq!(redeem_error(&result), GENERIC_REDEEM_ERROR);
    std::fs::remove_dir_all(&dir).ok();

    // Disallowed kind.
    let dir = temp_dir("pairing-kind");
    let runtime = pairing_runtime(
        &dir,
        super::pairing::DEFAULT_TTL_MS,
        FakeIssuer::with(Vec::new()),
    );
    let desktop_only = runtime
        .create_pairing_session(super::pairing::CreatePairingInput {
            allowed_client_kinds: Some(vec!["desktop".to_string()]),
            ..Default::default()
        })
        .await
        .expect("create");
    let result = runtime
        .redeem_pairing_session(super::pairing::RedeemPairingInput {
            pairing_id: Some(desktop_only.pairing.session.id.clone()),
            secret: Some(desktop_only.pairing.secret.clone()),
            client_kind: Some("mobile".to_string()),
            ..Default::default()
        })
        .await;
    assert_eq!(redeem_error(&result), GENERIC_REDEEM_ERROR);
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn does_not_consume_the_pairing_session_if_client_issuance_fails() {
    let dir = temp_dir("pairing-issuance");
    let issuer = FakeIssuer::with(vec![
        Err("disk failed".to_string()),
        Ok(("client-1".to_string(), "token-1".to_string())),
    ]);
    let runtime = pairing_runtime(&dir, super::pairing::DEFAULT_TTL_MS, issuer.clone());
    let created = runtime
        .create_pairing_session(Default::default())
        .await
        .expect("create");

    let first = runtime
        .redeem_pairing_session(super::pairing::RedeemPairingInput {
            pairing_id: Some(created.pairing.session.id.clone()),
            secret: Some(created.pairing.secret.clone()),
            client_kind: Some("mobile".to_string()),
            ..Default::default()
        })
        .await;
    assert_eq!(redeem_error(&first), "disk failed");

    let second = runtime
        .redeem_pairing_session(super::pairing::RedeemPairingInput {
            pairing_id: Some(created.pairing.session.id.clone()),
            secret: Some(created.pairing.secret.clone()),
            client_kind: Some("mobile".to_string()),
            ..Default::default()
        })
        .await
        .expect("redeem");
    assert_eq!(second.token, "token-1");
    let calls = issuer.calls();
    assert_eq!(
        calls.last().and_then(|call| call.dedupe_key.clone()),
        Some(format!("pairing:{}", created.pairing.session.id))
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn sweeps_expired_never_used_sessions_from_the_store_on_the_next_create() {
    let dir = temp_dir("pairing-sweep");
    let runtime = pairing_runtime(&dir, -1000, FakeIssuer::with(Vec::new()));
    // Immediately expired (negative TTL), never used or cancelled.
    let expired = runtime
        .create_pairing_session(super::pairing::CreatePairingInput {
            label: Some("stale".to_string()),
            ..Default::default()
        })
        .await
        .expect("create");

    // The next create sweeps the store; only the fresh session remains.
    runtime
        .create_pairing_session(super::pairing::CreatePairingInput {
            label: Some("fresh".to_string()),
            ..Default::default()
        })
        .await
        .expect("create");
    let store = std::fs::read_to_string(dir.join("client-pairing-sessions.json")).expect("read");
    let payload: serde_json::Value = serde_json::from_str(&store).expect("parse");
    let ids: Vec<&str> = payload["sessions"]
        .as_array()
        .expect("sessions")
        .iter()
        .map(|session| session["id"].as_str().expect("id"))
        .collect();
    assert!(!ids.contains(&expired.pairing.session.id.as_str()));
    assert_eq!(ids.len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn pairing_pending_list_and_get_hide_the_secret() {
    let dir = temp_dir("pairing-list");
    let runtime = pairing_runtime(
        &dir,
        super::pairing::DEFAULT_TTL_MS,
        FakeIssuer::with(Vec::new()),
    );

    let created = runtime
        .create_pairing_session(super::pairing::CreatePairingInput {
            label: Some("Scanner".to_string()),
            uses_relay: true,
            ..Default::default()
        })
        .await
        .expect("create");
    let pending = runtime.list_pending_sessions().await.expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].label, "Scanner");
    assert!(pending[0].uses_relay);
    let serialized = serde_json::to_value(&pending[0]).expect("serialize");
    assert!(serialized.get("secret").is_none());
    assert!(serialized.get("secretHash").is_none());

    let fetched = runtime
        .get_pairing_session(Some(&created.pairing.session.id))
        .await
        .expect("get")
        .expect("found");
    assert_eq!(fetched.id, created.pairing.session.id);
    assert!(
        runtime
            .get_pairing_session(None)
            .await
            .expect("get")
            .is_none()
    );
    assert!(runtime.has_active_relay_session().await.expect("relay"));

    std::fs::remove_dir_all(&dir).ok();
}

// -- tunnel auth ------------------------------------------------------------

/// A controllable wall clock: returns the shared current millis.
fn manual_clock(start_ms: i64) -> (Clock, Arc<Mutex<i64>>) {
    let current = Arc::new(Mutex::new(start_ms));
    let reader = current.clone();
    (
        Arc::new(move || *reader.lock().unwrap_or_else(|e| e.into_inner())),
        current,
    )
}

fn header_map(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            HeaderName::from_static(name),
            HeaderValue::from_str(value).expect("header value"),
        );
    }
    map
}

fn ctx_of(headers: &HeaderMap) -> TunnelRequestContext<'_> {
    TunnelRequestContext {
        headers,
        hostname: None,
        socket_remote_address: None,
        secure: false,
        ip: None,
    }
}

fn exchange_with(
    auth: &TunnelAuth,
    headers: &HeaderMap,
    token: Option<&str>,
    ttl: i64,
) -> super::tunnel_auth::ExchangeOutcome {
    let ctx = ctx_of(headers);
    auth.exchange_bootstrap_token(&ctx, token, ttl)
}

#[test]
fn classifies_request_scopes_like_the_js_controller() {
    let (clock, _) = manual_clock(1_700_000_000_000);
    let auth = TunnelAuth::new(clock);
    auth.set_active_tunnel(
        "tunnel-1",
        Some("https://tunnel.example.com"),
        Some("cloudflare"),
    );

    // Host matches the active tunnel's public URL host.
    let tunnel_headers = header_map(&[("host", "tunnel.example.com")]);
    assert_eq!(
        auth.classify_request_scope(&ctx_of(&tunnel_headers)),
        RequestScope::Tunnel
    );

    // A local Host header with a local socket peer is local.
    let local_headers = header_map(&[("host", "localhost:3000")]);
    let local = TunnelRequestContext {
        headers: &local_headers,
        hostname: None,
        socket_remote_address: Some("127.0.0.1"),
        secure: false,
        ip: None,
    };
    assert_eq!(auth.classify_request_scope(&local), RequestScope::Local);

    // A private Host header from a public socket peer is NOT trusted.
    let spoof_headers = header_map(&[("host", "192.168.1.5:57123")]);
    let spoof = TunnelRequestContext {
        headers: &spoof_headers,
        hostname: None,
        socket_remote_address: Some("203.0.113.10"),
        secure: false,
        ip: None,
    };
    assert_eq!(
        auth.classify_request_scope(&spoof),
        RequestScope::UnknownPublic
    );

    // Without an active tunnel every public host falls back to local.
    let (clock2, _) = manual_clock(1_700_000_000_000);
    let idle = TunnelAuth::new(clock2);
    let public_headers = header_map(&[("host", "example.com")]);
    assert_eq!(
        idle.classify_request_scope(&ctx_of(&public_headers)),
        RequestScope::Local
    );
}

#[test]
fn exchanges_bootstrap_tokens_one_time_and_sets_the_session_cookie() {
    let (clock, _) = manual_clock(1_700_000_000_000);
    let auth = TunnelAuth::new(clock);
    auth.set_active_tunnel("tunnel-1", Some("https://tunnel.example.com"), None);

    let issued = auth.issue_bootstrap_token(Some(60_000)).expect("issue");
    assert!(issued.expires_at.is_some());
    assert!(auth.bootstrap_status().has_bootstrap_token);

    let empty = header_map(&[]);
    let exchange = exchange_with(&auth, &empty, Some(&issued.token), 3_600_000);
    assert!(exchange.ok);
    assert_eq!(exchange.reason, None);
    assert!(exchange.session_expires_at.is_some());
    let cookie = exchange.set_cookie.expect("cookie");
    assert!(cookie.starts_with("oc_tunnel_session="));
    assert!(cookie.contains("; Path=/; HttpOnly; SameSite=Lax; Max-Age=3600; Expires="));
    assert!(!cookie.contains("Secure"));

    // Bootstrap tokens are one-time: a replay reports expiry.
    let replay = exchange_with(&auth, &empty, Some(&issued.token), 3_600_000);
    assert!(!replay.ok);
    assert_eq!(replay.reason, Some("expired"));

    // Wrong and missing tokens fail closed with the JS reasons.
    let (clock2, _) = manual_clock(1_700_000_000_000);
    let auth2 = TunnelAuth::new(clock2);
    auth2.set_active_tunnel("tunnel-2", Some("https://t.example.com"), None);
    let issued2 = auth2.issue_bootstrap_token(Some(60_000)).expect("issue");
    assert_eq!(
        exchange_with(&auth2, &empty, Some("wrong-token"), 3_600_000).reason,
        Some("invalid-token")
    );
    assert_eq!(
        exchange_with(&auth2, &empty, None, 3_600_000).reason,
        Some("missing-token")
    );

    // Without an active tunnel the exchange reports inactive.
    let (clock3, _) = manual_clock(1_700_000_000_000);
    let auth3 = TunnelAuth::new(clock3);
    assert_eq!(
        exchange_with(&auth3, &empty, Some(&issued2.token), 3_600_000).reason,
        Some("inactive")
    );
    // And issuing fails outright.
    assert!(auth3.issue_bootstrap_token(Some(60_000)).is_err());
}

#[test]
fn tunnel_sessions_authenticate_via_cookie_and_expire() {
    let (clock, now) = manual_clock(1_700_000_000_000);
    let auth = TunnelAuth::new(clock);
    auth.set_active_tunnel(
        "tunnel-1",
        Some("https://tunnel.example.com"),
        Some("cloudflare"),
    );
    let issued = auth.issue_bootstrap_token(Some(60_000)).expect("issue");

    let request_headers = header_map(&[("cookie", "unrelated=yes")]);
    let exchange = exchange_with(&auth, &request_headers, Some(&issued.token), 3_600_000);
    assert!(exchange.ok);
    let session_id = exchange
        .set_cookie
        .and_then(|cookie| {
            cookie
                .strip_prefix("oc_tunnel_session=")
                .map(str::to_string)
        })
        .and_then(|rest| rest.split(';').next().map(str::to_string))
        .expect("session id");

    let cookie_value = format!("unrelated=yes; oc_tunnel_session={session_id}; trailing=1");
    let real_headers = header_map(&[("cookie", cookie_value.as_str())]);
    let ctx = ctx_of(&real_headers);
    let session = auth.get_tunnel_session_from_request(&ctx).expect("session");
    assert_eq!(session.tunnel_id, "tunnel-1");
    assert!(auth.require_tunnel_session(&ctx).is_ok());

    // Sessions expire with the clock.
    *now.lock().unwrap_or_else(|e| e.into_inner()) += 3_600_001;
    assert!(auth.get_tunnel_session_from_request(&ctx).is_none());
    let rejection = auth.require_tunnel_session(&ctx).expect_err("reject");
    let response = rejection.into_response();
    assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    let set_cookie = response
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .expect("clear cookie");
    assert_eq!(
        set_cookie,
        "oc_tunnel_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
    );

    // Invalid cookies never authenticate.
    let bad_headers = header_map(&[("cookie", "oc_tunnel_session=not-a-session")]);
    assert!(
        auth.get_tunnel_session_from_request(&ctx_of(&bad_headers))
            .is_none()
    );
}

#[test]
fn revoking_tunnel_artifacts_invalidates_sessions_and_bootstrap() {
    let (clock, _) = manual_clock(1_700_000_000_000);
    let auth = TunnelAuth::new(clock);
    auth.set_active_tunnel("tunnel-1", Some("https://tunnel.example.com"), None);
    let issued = auth.issue_bootstrap_token(Some(60_000)).expect("issue");
    let empty = header_map(&[]);
    let exchange = exchange_with(&auth, &empty, Some(&issued.token), 3_600_000);
    assert!(exchange.ok);
    let session_id = exchange
        .set_cookie
        .and_then(|cookie| {
            cookie
                .strip_prefix("oc_tunnel_session=")
                .map(str::to_string)
        })
        .and_then(|rest| rest.split(';').next().map(str::to_string))
        .expect("session id");

    let revoked = auth.revoke_tunnel_artifacts("tunnel-1");
    assert_eq!(revoked.invalidated_session_count, 1);
    // JS revokeBootstrapToken stamps revokedAt even on a used record and
    // still counts it.
    assert_eq!(revoked.revoked_bootstrap_count, 1);

    let cookie_value = format!("oc_tunnel_session={session_id}");
    let real_headers = header_map(&[("cookie", cookie_value.as_str())]);
    assert!(
        auth.get_tunnel_session_from_request(&ctx_of(&real_headers))
            .is_none()
    );

    let listed = auth.list_tunnel_sessions();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].status, "inactive");
    assert_eq!(listed[0].inactive_reason.as_deref(), Some("tunnel-revoked"));

    // clearActiveTunnel wipes everything and future issues fail.
    auth.clear_active_tunnel();
    assert_eq!(auth.active_tunnel_id(), None);
    assert!(auth.issue_bootstrap_token(Some(60_000)).is_err());
}

#[test]
fn revoking_an_unused_bootstrap_token_counts_it() {
    let (clock, _) = manual_clock(1_700_000_000_000);
    let auth = TunnelAuth::new(clock);
    auth.set_active_tunnel("tunnel-1", Some("https://tunnel.example.com"), None);
    auth.issue_bootstrap_token(Some(60_000)).expect("issue");

    let revoked = auth.revoke_tunnel_artifacts("tunnel-1");
    assert_eq!(revoked.revoked_bootstrap_count, 1);
    assert!(!auth.bootstrap_status().has_bootstrap_token);
}

#[test]
fn connect_attempts_are_rate_limited_per_client_ip() {
    let (clock, now) = manual_clock(1_700_000_000_000);
    let auth = TunnelAuth::new(clock);
    auth.set_active_tunnel("tunnel-1", Some("https://t.example.com"), None);
    let issued = auth.issue_bootstrap_token(Some(60_000)).expect("issue");

    let xff_headers = header_map(&[("x-forwarded-for", "198.51.100.7, 10.0.0.1")]);
    // 20 failed attempts stay allowed (under the cap); the 21st locks.
    for _ in 0..20 {
        let outcome = exchange_with(&auth, &xff_headers, Some("wrong"), 3_600_000);
        assert_eq!(outcome.reason, Some("invalid-token"));
        *now.lock().unwrap_or_else(|e| e.into_inner()) += 1_000;
    }
    let locked = exchange_with(&auth, &xff_headers, Some("wrong"), 3_600_000);
    assert!(!locked.ok);
    assert_eq!(locked.reason, Some("rate-limited"));
    assert_eq!(locked.retry_after, Some(600));

    // The correct token is also refused while locked...
    let correct = exchange_with(&auth, &xff_headers, Some(&issued.token), 3_600_000);
    assert_eq!(correct.reason, Some("rate-limited"));

    // ...and a different client IP has its own bucket.
    let other_headers = header_map(&[("x-forwarded-for", "198.51.100.8")]);
    let succeeds = exchange_with(&auth, &other_headers, Some(&issued.token), 3_600_000);
    assert!(succeeds.ok);

    // The lock expires, but the bootstrap token was already consumed.
    *now.lock().unwrap_or_else(|e| e.into_inner()) += 10 * 60 * 1000 + 1;
    let after_lock = exchange_with(&auth, &xff_headers, Some(&issued.token), 3_600_000);
    assert_eq!(after_lock.reason, Some("expired"));
}

#[test]
fn no_ip_requests_get_the_stricter_rate_limit_bucket() {
    let (clock, now) = manual_clock(1_700_000_000_000);
    let auth = TunnelAuth::new(clock);
    auth.set_active_tunnel("tunnel-1", Some("https://t.example.com"), None);
    auth.issue_bootstrap_token(Some(60_000)).expect("issue");

    let empty = header_map(&[]);
    for _ in 0..5 {
        let outcome = exchange_with(&auth, &empty, Some("wrong"), 3_600_000);
        assert_eq!(outcome.reason, Some("invalid-token"));
        *now.lock().unwrap_or_else(|e| e.into_inner()) += 1_000;
    }
    let locked = exchange_with(&auth, &empty, Some("wrong"), 3_600_000);
    assert_eq!(locked.reason, Some("rate-limited"));
}

#[test]
fn secure_requests_get_secure_cookies() {
    let (clock, _) = manual_clock(1_700_000_000_000);
    let auth = TunnelAuth::new(clock);
    auth.set_active_tunnel("tunnel-1", Some("https://t.example.com"), None);
    let issued = auth.issue_bootstrap_token(Some(60_000)).expect("issue");

    let headers = header_map(&[("x-forwarded-proto", "https, http")]);
    let exchange = exchange_with(&auth, &headers, Some(&issued.token), 3_600_000);
    assert!(exchange.ok);
    assert!(exchange.set_cookie.expect("cookie").ends_with("; Secure"));
}

#[test]
fn active_tunnel_host_derives_from_the_public_url() {
    let (clock, _) = manual_clock(1_700_000_000_000);
    let auth = TunnelAuth::new(clock);
    auth.set_active_tunnel(
        "tunnel-1",
        Some("https://try.example.com:443"),
        Some("cloudflare"),
    );
    assert_eq!(
        auth.active_tunnel_host().as_deref(),
        Some("try.example.com")
    );
    assert_eq!(auth.active_tunnel_mode().as_deref(), Some("cloudflare"));
    assert_eq!(auth.active_tunnel_id().as_deref(), Some("tunnel-1"));

    // A URL with a port still classifies (normalizeHost strips the port).
    let headers = header_map(&[("host", "try.example.com:8443")]);
    assert_eq!(
        auth.classify_request_scope(&ctx_of(&headers)),
        RequestScope::Tunnel
    );

    // Unparseable URLs leave the host unset.
    auth.set_active_tunnel("tunnel-2", Some("not a url"), None);
    assert_eq!(auth.active_tunnel_host(), None);
}

// -- shared registry --------------------------------------------------------

#[tokio::test]
async fn state_registry_shares_runtimes_per_data_dir() {
    let dir = temp_dir("registry");
    let a = super::state_for_data_dir(&dir);
    let b = super::state_for_data_dir(&dir);
    assert!(Arc::ptr_eq(&a, &b));
    assert!(Arc::ptr_eq(&a.remote_clients, &b.remote_clients));

    // The pairing runtime redeems into the real remote-client store.
    let created = a
        .pairing
        .create_pairing_session(Default::default())
        .await
        .expect("create");
    let redeemed = a
        .pairing
        .redeem_pairing_session(super::pairing::RedeemPairingInput {
            pairing_id: Some(created.pairing.session.id.clone()),
            secret: Some(created.pairing.secret.clone()),
            client_kind: Some("desktop".to_string()),
            ..Default::default()
        })
        .await
        .expect("redeem");
    assert!(redeemed.token.starts_with("oc_client_"));
    assert!(
        a.remote_clients
            .is_valid_client_token(&redeemed.token)
            .await
    );
    assert_eq!(
        b.remote_clients.list_clients().await.expect("list").len(),
        1
    );
    assert!(dir.join("remote-clients.json").exists());
    assert!(dir.join("client-pairing-sessions.json").exists());

    std::fs::remove_dir_all(&dir).ok();
}

// -- iso helpers ------------------------------------------------------------

#[test]
fn iso_helpers_roundtrip_ms_precision() {
    let ms = 1_790_380_800_123i64;
    let iso = super::time::iso_utc_from_unix_millis(ms);
    assert_eq!(iso, "2026-09-26T00:00:00.123Z");
    assert_eq!(super::time::parse_iso_ms(&iso), Some(ms));
}
