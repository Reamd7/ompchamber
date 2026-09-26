//! Port of `server/lib/tunnels/routes.js` (`createTunnelRoutesRuntime`) plus
//! the `/api/dev-tunnel` upgrade route from `dev-tunnel/runtime.js`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{FromRequestParts, RawQuery, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Map, Value, json};

use crate::context::RouterContext;
use crate::settings::SettingsStore;
use crate::settings::normalization::{
    normalize_managed_remote_tunnel_hostname, normalize_tunnel_bootstrap_ttl_ms,
    normalize_tunnel_session_ttl_ms,
};
use crate::ui_auth::reject_websocket_upgrade;

use super::cloudflare::CloudflareTunnelProvider;
use super::cloudflare::print_tunnel_warning;
use super::dev_tunnel::{
    AuthKind, DevTunnelState, DiscoverFn, DiscoverOutcome, ResolveAuthFn, dev_tunnel_preflight,
    pipe_dev_tunnel_socket,
};
use super::executable_search::{real_cwd, real_home};
use super::managed_config::ManagedTunnelConfigRuntime;
use super::ngrok::NgrokTunnelProvider;
use super::registry::{TunnelController, TunnelProviderRegistry};
use super::runner::RealRunner;
use super::service::TunnelService;
use super::types::{
    Platform, TUNNEL_MODE_MANAGED_LOCAL, TUNNEL_MODE_MANAGED_REMOTE, TUNNEL_MODE_QUICK,
    TUNNEL_PROVIDER_CLOUDFLARE, TunnelServiceError, normalize_optional_path, normalize_tunnel_mode,
    normalize_tunnel_provider,
};
use crate::client_auth::tunnel_auth::TunnelAuth;

#[derive(Clone)]
pub struct ModuleState {
    pub registry: Arc<TunnelProviderRegistry>,
    pub service: Arc<TunnelService>,
    pub auth: Arc<TunnelAuth>,
    pub managed_config: Arc<ManagedTunnelConfigRuntime>,
    pub controller: Arc<Mutex<Option<TunnelController>>>,
    pub runtime_hostname: Arc<Mutex<String>>,
    pub runtime_token: Arc<Mutex<String>>,
    pub active_port: Arc<AtomicU16>,
    pub settings: Arc<SettingsStore>,
    pub dev: DevTunnelState,
}

fn platform_now() -> Platform {
    Platform::current()
}

fn home_dir() -> String {
    real_home()
}

fn cwd_dir() -> String {
    real_cwd()
}

/// JS `value || null` for optional strings.
fn or_null(value: Option<String>) -> Value {
    match value {
        Some(value) if !value.is_empty() => Value::from(value),
        _ => Value::Null,
    }
}

fn query_map(raw: Option<&str>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(raw) = raw else {
        return map;
    };
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        map.entry(key.to_string())
            .or_insert_with(|| value.to_string());
    }
    map
}

fn query_str<'a>(params: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    params.get(key).map(String::as_str)
}

fn body_str<'a>(body: &'a Value, key: &str) -> &'a str {
    body.get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
}

fn normalized_hostname(value: Option<&str>) -> Option<String> {
    normalize_managed_remote_tunnel_hostname(&value.map(Value::from).unwrap_or(Value::Null))
}

fn hostname_value(value: Option<String>) -> Value {
    match value {
        Some(value) => Value::from(value),
        None => Value::Null,
    }
}

fn hostname_chain(candidates: &[Option<&str>]) -> Option<String> {
    for candidate in candidates {
        if let Some(hostname) = normalized_hostname(candidate.as_deref()) {
            return Some(hostname);
        }
    }
    None
}

/// `normalizeOptionalPath(a) ?? normalizeOptionalPath(b)` with JS throw
/// semantics for out-of-home paths.
fn optional_path_or(
    primary: Option<&str>,
    fallback: Option<&str>,
) -> Result<Option<String>, TunnelServiceError> {
    if let Some(resolved) =
        normalize_optional_path(primary, &home_dir(), &cwd_dir(), platform_now())?
    {
        return Ok(Some(resolved));
    }
    normalize_optional_path(fallback, &home_dir(), &cwd_dir(), platform_now())
}

async fn read_settings(st: &ModuleState) -> Map<String, Value> {
    st.settings.read_migrated().await.unwrap_or_default()
}

fn settings_str<'a>(settings: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    settings.get(key).and_then(|value| value.as_str())
}

/// `resolvePreferredTunnelProvider(reqBody)`.
async fn resolve_preferred_provider(st: &ModuleState, body: Option<&Value>) -> String {
    if let Some(body) = body
        && let Some(provider) = body.get("provider").and_then(Value::as_str)
        && !provider.trim().is_empty()
    {
        return normalize_tunnel_provider(Some(provider));
    }
    if let Some(active) = st.service.resolve_active_provider() {
        return normalize_tunnel_provider(Some(&active));
    }
    let settings = read_settings(st).await;
    normalize_tunnel_provider(settings_str(&settings, "tunnelProvider"))
}

/// The normalized start request `startTunnelWithNormalizedRequest` receives.
pub struct NormalizedStartRequest {
    pub provider: String,
    pub mode: String,
    pub intent: Option<String>,
    pub hostname: Option<String>,
    pub token: String,
    pub config_path: Option<String>,
    pub selected_preset_id: String,
    pub selected_preset_name: String,
}

/// `startTunnelWithNormalizedRequest`.
pub async fn start_tunnel_with_normalized_request(
    st: &ModuleState,
    request: NormalizedStartRequest,
) -> Result<(String, String, String, Value), TunnelServiceError> {
    if request.provider == TUNNEL_PROVIDER_CLOUDFLARE && request.mode == TUNNEL_MODE_MANAGED_REMOTE
    {
        *st.runtime_hostname
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = request.hostname.clone().unwrap_or_default();
        *st.runtime_token.lock().unwrap_or_else(|e| e.into_inner()) = request.token.clone();

        if !request.token.is_empty()
            && let Some(hostname) = &request.hostname
        {
            st.managed_config
                .upsert_managed_remote_tunnel_token(
                    selected_preset_id_if(&request.selected_preset_id, hostname),
                    selected_preset_name_if(&request.selected_preset_name, hostname),
                    hostname,
                    &request.token,
                )
                .await;
        }
    }

    let raw_request = json!({
        "provider": request.provider,
        "mode": request.mode,
        "intent": request.intent,
        "configPath": request.config_path,
        "token": request.token,
        "hostname": request.hostname,
    });
    let result = st.service.start(raw_request).await?;
    println!("Tunnel active ({}): {}", result.provider, result.public_url);
    Ok((
        result.public_url,
        result.active_mode,
        result.provider,
        result.provider_metadata,
    ))
}

fn selected_preset_id_if<'a>(selected_preset_id: &'a str, hostname: &'a str) -> &'a str {
    if selected_preset_id.is_empty() {
        hostname
    } else {
        selected_preset_id
    }
}

fn selected_preset_name_if<'a>(selected_preset_name: &'a str, hostname: &'a str) -> &'a str {
    if selected_preset_name.is_empty() {
        hostname
    } else {
        selected_preset_name
    }
}

fn error_status(code: &str) -> u16 {
    match code {
        "missing_dependency" => 400,
        "validation_error" | "provider_unsupported" | "mode_unsupported" => 422,
        _ => 500,
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn tunnel_check(State(st): State<ModuleState>, RawQuery(query): RawQuery) -> Response {
    let params = query_map(query.as_deref());
    let requested_provider = match query_str(&params, "provider")
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
    {
        Some(provider) => normalize_tunnel_provider(Some(provider)),
        None => resolve_preferred_provider(&st, None).await,
    };

    match st.service.check_availability(&requested_provider).await {
        Ok(result) => Json(json!({
            "available": result.available,
            "provider": requested_provider,
            "version": or_null(result.version),
            "dependency": or_null(Some(result.dependency)),
            "installCommand": or_null(Some(result.install_command)),
            "installUrl": or_null(Some(result.install_url)),
            "platform": if result.platform.is_empty() {
                Value::from(platform_now().js_name())
            } else {
                Value::from(result.platform)
            },
            "message": or_null(Some(result.message)),
        }))
        .into_response(),
        Err(error) => {
            tracing::warn!("Tunnel dependency check failed: {}", error.message);
            Json(json!({
                "available": false,
                "provider": null,
                "version": null,
                "dependency": null,
                "installCommand": null,
                "installUrl": null,
                "platform": platform_now().js_name(),
                "message": null,
            }))
            .into_response()
        }
    }
}

/// `runTunnelDoctor`.
async fn run_tunnel_doctor(
    st: &ModuleState,
    provider_id: &str,
    mode_filter: Option<&str>,
    doctor_request: super::registry::DiagnoseRequest,
) -> Result<Value, TunnelServiceError> {
    let Some(provider) = st.registry.get(provider_id) else {
        return Err(TunnelServiceError::new(
            "provider_unsupported",
            format!("Unsupported tunnel provider: {provider_id}"),
        ));
    };

    let mode_keys: Vec<&str> = provider
        .mode_descriptors()
        .iter()
        .map(|mode| mode.key)
        .collect();
    if let Some(filter) = mode_filter
        && !mode_keys.contains(&filter)
    {
        return Err(TunnelServiceError::new(
            "mode_unsupported",
            format!("Provider '{provider_id}' does not support mode '{filter}'"),
        ));
    }

    let mut request = doctor_request;
    if let Some(filter) = mode_filter {
        request.mode = Some(filter.to_string());
    }
    let diagnosed = provider.diagnose(request).await;
    let provider_checks = diagnosed
        .get("providerChecks")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let all_modes = diagnosed
        .get("modes")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    let modes = match mode_filter {
        Some(filter) => Value::Array(
            all_modes
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| entry["mode"].as_str() == Some(filter))
                .collect(),
        ),
        None => all_modes,
    };

    Ok(json!({
        "ok": true,
        "provider": provider_id,
        "providerChecks": provider_checks,
        "modes": modes,
    }))
}

async fn handle_tunnel_doctor(
    State(st): State<ModuleState>,
    RawQuery(query): RawQuery,
    body: Option<Json<Value>>,
) -> Response {
    let params = query_map(query.as_deref());
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));

    let result = async {
        let provider_id = match query_str(&params, "provider")
            .map(str::trim)
            .filter(|provider| !provider.is_empty())
        {
            Some(provider) => normalize_tunnel_provider(Some(provider)),
            None => resolve_preferred_provider(&st, None).await,
        };
        let mode_filter = query_str(&params, "mode")
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .map(|mode| mode.to_lowercase());

        let settings = read_settings(&st).await;
        let selected_preset_id = query_str(&params, "managedRemoteTunnelPresetId")
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        let request_config_path = optional_path_or(
            query_str(&params, "configPath"),
            settings_str(&settings, "managedLocalTunnelConfigPath"),
        )?;
        let hostname = hostname_chain(&[
            query_str(&params, "hostname"),
            query_str(&params, "tunnelHostname"),
            query_str(&params, "managedRemoteTunnelHostname"),
            settings_str(&settings, "managedRemoteTunnelHostname"),
        ]);

        let request_managed_remote_token = body_str(&body, "managedRemoteTunnelToken").trim();
        let request_tunnel_token = body_str(&body, "tunnelToken").trim();
        let request_token = body_str(&body, "token").trim();
        let request_token_provided = body.get("managedRemoteTunnelTokenProvided")
            == Some(&Value::Bool(true))
            || body.get("tunnelTokenProvided") == Some(&Value::Bool(true))
            || body.get("tokenProvided") == Some(&Value::Bool(true));
        let request_hostname_provided = body.get("managedRemoteTunnelHostnameProvided")
            == Some(&Value::Bool(true))
            || body.get("tunnelHostnameProvided") == Some(&Value::Bool(true))
            || body.get("hostnameProvided") == Some(&Value::Bool(true));
        let stored_managed_remote_token = settings_str(&settings, "managedRemoteTunnelToken")
            .unwrap_or_default()
            .trim()
            .to_string();
        let managed_remote_tunnel_config = st
            .managed_config
            .read_managed_remote_tunnel_config_from_disk()
            .await;
        let server_has_saved_profile = managed_remote_tunnel_config.tunnels.iter().any(|entry| {
            normalized_hostname(Some(&entry.hostname)).is_some() && !entry.token.trim().is_empty()
        });
        let cli_has_saved_profile = query_str(&params, "hasSavedManagedRemoteProfile") == Some("1");
        let has_saved_managed_remote_profile = server_has_saved_profile || cli_has_saved_profile;
        let config_managed_remote_token = if provider_id == TUNNEL_PROVIDER_CLOUDFLARE {
            st.managed_config
                .resolve_managed_remote_tunnel_token(
                    &selected_preset_id,
                    hostname.as_deref().unwrap_or_default(),
                )
                .await
        } else {
            String::new()
        };
        let runtime_hostname = st
            .runtime_hostname
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let runtime_token = st
            .runtime_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let runtime_matches = !runtime_hostname.is_empty()
            && hostname.is_some()
            && runtime_hostname == hostname.clone().unwrap_or_default();
        let token = first_non_empty(&[
            request_token,
            request_tunnel_token,
            request_managed_remote_token,
            if runtime_matches {
                runtime_token.as_str()
            } else {
                ""
            },
            config_managed_remote_token.as_str(),
            stored_managed_remote_token.as_str(),
        ])
        .to_string();

        let doctor_request = super::registry::DiagnoseRequest {
            mode: mode_filter.clone(),
            hostname: hostname.clone(),
            token: Some(token),
            token_provided: request_token_provided,
            hostname_provided: request_hostname_provided,
            config_path: request_config_path,
            has_saved_managed_remote_profile,
        };

        run_tunnel_doctor(&st, &provider_id, mode_filter.as_deref(), doctor_request).await
    }
    .await;

    match result {
        Ok(value) => Json(value).into_response(),
        // Only TunnelServiceError flows here (settings reads are lenient), so
        // every failure takes the JS 400 branch; the JS 500 catch-all covered
        // unexpected throws this port cannot produce.
        Err(error) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": error.message, "code": error.code })),
        )
            .into_response(),
    }
}

fn sessions_value(auth: &TunnelAuth) -> Value {
    serde_json::to_value(auth.list_tunnel_sessions()).unwrap_or(Value::Array(Vec::new()))
}

/// `tunnelAuthController.getActiveTunnelMode() || null`.
fn auth_mode_value(mode: Option<String>) -> Value {
    match mode {
        Some(mode) => Value::from(mode),
        None => Value::Null,
    }
}

/// `crypto.randomUUID()` (v4) — tunnel ids in routes.js.
fn random_uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    use rand::RngCore;
    rand::rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn first_non_empty<'a>(candidates: &[&'a str]) -> &'a str {
    candidates
        .iter()
        .find(|candidate| !candidate.is_empty())
        .copied()
        .unwrap_or_default()
}

async fn tunnel_providers(State(st): State<ModuleState>) -> Response {
    Json(json!({ "providers": st.registry.list_capabilities() })).into_response()
}

fn ttl_value(value: Option<&Value>) -> Value {
    match value {
        Some(value) => normalize_tunnel_bootstrap_ttl_ms(value),
        None => normalize_tunnel_bootstrap_ttl_ms(&Value::Bool(false)),
    }
}

fn ttl_option(value: &Value) -> Option<u64> {
    value.as_u64()
}

async fn tunnel_status(State(st): State<ModuleState>) -> Response {
    let settings = read_settings(&st).await;
    let normalized_mode = normalize_tunnel_mode(settings_str(&settings, "tunnelMode"));
    let managed_remote_hostname =
        normalized_hostname(settings_str(&settings, "managedRemoteTunnelHostname"));
    let managed_remote_tunnel_config = st
        .managed_config
        .read_managed_remote_tunnel_config_from_disk()
        .await;
    let managed_remote_tunnel_preset_summaries: Vec<Value> = managed_remote_tunnel_config
        .tunnels
        .iter()
        .map(|entry| json!({ "id": entry.id, "name": entry.name, "hostname": entry.hostname }))
        .collect();
    let managed_remote_tunnel_preset_ids: Vec<String> = managed_remote_tunnel_config
        .tunnels
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    let stored_managed_remote_token = settings_str(&settings, "managedRemoteTunnelToken")
        .unwrap_or_default()
        .trim();
    let has_stored_managed_remote_token = !stored_managed_remote_token.is_empty();
    let has_managed_remote_tunnel_token = !st
        .runtime_token
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty()
        || !managed_remote_tunnel_config.tunnels.is_empty()
        || has_stored_managed_remote_token;
    let bootstrap_ttl_value = ttl_value(settings.get("tunnelBootstrapTtlMs"));
    let session_ttl_ms = normalize_tunnel_session_ttl_ms(
        settings
            .get("tunnelSessionTtlMs")
            .unwrap_or(&Value::Bool(false)),
    )
    .as_u64();
    let active_sessions = sessions_value(&st.auth);
    let active_provider = st.service.resolve_active_provider();
    let provider = active_provider
        .clone()
        .unwrap_or_else(|| normalize_tunnel_provider(settings_str(&settings, "tunnelProvider")));

    let ttl_config = json!({
        "bootstrapTtlMs": if settings.get("tunnelBootstrapTtlMs").map(Value::is_null).unwrap_or(false) { Value::Null } else { Value::from(bootstrap_ttl_value.as_u64().unwrap_or_default()) },
        "sessionTtlMs": session_ttl_ms,
    });

    let local_port = st.active_port.load(Ordering::SeqCst);

    let public_url = st.service.get_public_url();
    let Some(public_url) = public_url else {
        return Json(json!({
            "active": false,
            "url": null,
            "mode": normalized_mode,
            "provider": provider,
            "providerMetadata": null,
            "hasManagedRemoteTunnelToken": has_managed_remote_tunnel_token,
            "managedRemoteTunnelHostname": hostname_value(managed_remote_hostname),
            "managedRemoteTunnelPresets": managed_remote_tunnel_preset_summaries,
            "managedRemoteTunnelTokenPresetIds": managed_remote_tunnel_preset_ids,
            "hasBootstrapToken": false,
            "bootstrapExpiresAt": null,
            "policy": "tunnel-gated",
            "activeTunnelMode": auth_mode_value(st.auth.active_tunnel_mode()),
            "activeSessions": active_sessions,
            "localPort": local_port,
            "ttlConfig": ttl_config,
        }))
        .into_response();
    };

    let active_normalized_mode = match st.service.resolve_active_mode().as_deref() {
        Some(TUNNEL_MODE_MANAGED_LOCAL) => TUNNEL_MODE_MANAGED_LOCAL.to_string(),
        Some(TUNNEL_MODE_MANAGED_REMOTE) => TUNNEL_MODE_MANAGED_REMOTE.to_string(),
        _ => TUNNEL_MODE_QUICK.to_string(),
    };
    let active_tunnel_id = st.auth.active_tunnel_id();
    let active_tunnel_host = st.auth.active_tunnel_host();
    // JS `resolveNormalizedTunnelHost`: `new URL(publicUrl).hostname` only.
    let resolved_tunnel_host = url::Url::parse(&public_url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_lowercase));
    let active_tunnel_mode = st.auth.active_tunnel_mode();
    let needs_active_tunnel_sync = active_tunnel_id.is_none()
        || active_tunnel_host.is_none()
        || resolved_tunnel_host.is_none()
        || active_tunnel_host != resolved_tunnel_host
        || active_tunnel_mode != Some(active_normalized_mode.clone());
    if needs_active_tunnel_sync {
        st.auth.set_active_tunnel(
            &active_tunnel_id.unwrap_or_else(random_uuid_v4),
            Some(public_url.as_str()),
            Some(active_normalized_mode.as_str()),
        );
    }

    let bootstrap_status = st.auth.bootstrap_status();
    let provider_metadata = st.service.get_provider_metadata();

    Json(json!({
        "active": true,
        "url": public_url,
        "mode": active_normalized_mode,
        "provider": provider,
        "providerMetadata": provider_metadata,
        "hasManagedRemoteTunnelToken": has_managed_remote_tunnel_token,
        "managedRemoteTunnelHostname": hostname_value(managed_remote_hostname),
        "managedRemoteTunnelPresets": managed_remote_tunnel_preset_summaries,
        "managedRemoteTunnelTokenPresetIds": managed_remote_tunnel_preset_ids,
        "hasBootstrapToken": bootstrap_status.has_bootstrap_token,
        "bootstrapExpiresAt": bootstrap_status.bootstrap_expires_at,
        "policy": "tunnel-gated",
        "activeTunnelMode": active_normalized_mode,
        "activeSessions": sessions_value(&st.auth),
        "localPort": local_port,
        "ttlConfig": ttl_config,
    }))
    .into_response()
}

async fn put_managed_remote_token(
    State(st): State<ModuleState>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
    let preset_id = body_str(&body, "presetId").trim();
    let preset_name = body_str(&body, "presetName").trim();
    let managed_remote_tunnel_hostname =
        normalized_hostname(Some(body_str(&body, "managedRemoteTunnelHostname")));
    let managed_remote_tunnel_token = body_str(&body, "managedRemoteTunnelToken").trim();

    if preset_id.is_empty()
        || preset_name.is_empty()
        || managed_remote_tunnel_hostname.is_none()
        || managed_remote_tunnel_token.is_empty()
    {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "presetId, presetName, managedRemoteTunnelHostname and managedRemoteTunnelToken are required"
            })),
        )
            .into_response();
    }

    let result = async {
        st.managed_config
            .upsert_managed_remote_tunnel_token(
                preset_id,
                preset_name,
                managed_remote_tunnel_hostname.as_deref().unwrap_or_default(),
                managed_remote_tunnel_token,
            )
            .await;
        let config = st
            .managed_config
            .read_managed_remote_tunnel_config_from_disk()
            .await;
        json!({
            "ok": true,
            "managedRemoteTunnelTokenPresetIds": config.tunnels.iter().map(|entry| entry.id.clone()).collect::<Vec<_>>(),
        })
    }
    .await;

    Json(result).into_response()
}

async fn post_tunnel_start(State(st): State<ModuleState>, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));

    enum StartRouteError {
        Status(axum::http::StatusCode, Value),
        Service(TunnelServiceError),
    }

    impl From<TunnelServiceError> for StartRouteError {
        fn from(error: TunnelServiceError) -> Self {
            StartRouteError::Service(error)
        }
    }

    let started: Result<Value, StartRouteError> = async {
        let settings = read_settings(&st).await;
        if let Some(provider) = body.get("provider").and_then(Value::as_str)
            && !provider.trim().is_empty()
        {
            let raw_provider = provider.trim().to_lowercase();
            if st.registry.get(&raw_provider).is_none() {
                return Err(StartRouteError::Status(
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    json!({
                        "ok": false,
                        "error": format!("Unsupported tunnel provider: {raw_provider}"),
                        "code": "provider_unsupported",
                    }),
                ));
            }
        }
        let provider = normalize_tunnel_provider(
            body.get("provider")
                .and_then(Value::as_str)
                .or_else(|| settings_str(&settings, "tunnelProvider")),
        );
        let mode_input = body
            .get("mode")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| settings_str(&settings, "tunnelMode").map(str::to_string));
        let intent = body
            .get("intent")
            .and_then(Value::as_str)
            .map(|value| value.trim().to_lowercase());
        let mode = match &mode_input {
            Some(mode) => {
                let lowered = mode.trim().to_lowercase();
                if body.get("mode").and_then(Value::as_str).map(str::trim).is_some_and(|m| !m.is_empty())
                    && !super::types::is_supported_tunnel_mode(&lowered)
                {
                    return Err(StartRouteError::Status(
                        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        json!({
                            "ok": false,
                            "error": format!("Unsupported tunnel mode: {lowered}"),
                            "code": "mode_unsupported",
                        }),
                    ));
                }
                lowered
            }
            None => normalize_tunnel_mode(None),
        };
        let selected_preset_id = body_str(&body, "managedRemoteTunnelPresetId").trim().to_string();
        let selected_preset_name =
            body_str(&body, "managedRemoteTunnelPresetName").trim().to_string();
        let request_config_path = optional_path_or(
            body.get("configPath").and_then(Value::as_str),
            settings_str(&settings, "managedLocalTunnelConfigPath"),
        )?;
        let hostname = hostname_chain(&[
            body.get("hostname").and_then(Value::as_str),
            body.get("tunnelHostname").and_then(Value::as_str),
            body.get("managedRemoteTunnelHostname").and_then(Value::as_str),
            settings_str(&settings, "managedRemoteTunnelHostname"),
        ]);
        let request_managed_remote_token = body_str(&body, "managedRemoteTunnelToken").trim();
        let request_tunnel_token = body_str(&body, "tunnelToken").trim();
        let request_token = body_str(&body, "token").trim();
        let stored_managed_remote_token = settings_str(&settings, "managedRemoteTunnelToken")
            .unwrap_or_default()
            .trim()
            .to_string();
        let config_managed_remote_token = if provider == TUNNEL_PROVIDER_CLOUDFLARE {
            st.managed_config
                .resolve_managed_remote_tunnel_token(
                    &selected_preset_id,
                    hostname.as_deref().unwrap_or_default(),
                )
                .await
        } else {
            String::new()
        };
        let runtime_hostname = st
            .runtime_hostname
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let runtime_token = st
            .runtime_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let runtime_matches = !runtime_hostname.is_empty()
            && hostname.is_some()
            && runtime_hostname == hostname.clone().unwrap_or_default();
        let token = first_non_empty(&[
            request_token,
            request_tunnel_token,
            request_managed_remote_token,
            if runtime_matches { runtime_token.as_str() } else { "" },
            config_managed_remote_token.as_str(),
            stored_managed_remote_token.as_str(),
        ])
        .to_string();
        let request_connect_ttl = body
            .get("connectTtlMs")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .map(|value| normalize_tunnel_bootstrap_ttl_ms(&Value::from(value as u64)));
        let request_session_ttl = body
            .get("sessionTtlMs")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .map(|value| normalize_tunnel_session_ttl_ms(&Value::from(value as u64)));
        let settings_bootstrap = match settings.get("tunnelBootstrapTtlMs") {
            Some(Value::Null) => Value::Null,
            Some(value) => normalize_tunnel_bootstrap_ttl_ms(value),
            None => normalize_tunnel_bootstrap_ttl_ms(&Value::Bool(false)),
        };
        let bootstrap_ttl_value = request_connect_ttl.unwrap_or(settings_bootstrap);
        let bootstrap_ttl_ms = ttl_option(&bootstrap_ttl_value);
        let session_ttl_ms = request_session_ttl
            .unwrap_or_else(|| {
                normalize_tunnel_session_ttl_ms(
                    settings
                        .get("tunnelSessionTtlMs")
                        .unwrap_or(&Value::Bool(false)),
                )
            })
            .as_u64();

        let previous_tunnel_id = st.auth.active_tunnel_id();
        let previous_mode = st.auth.active_tunnel_mode();
        let previous_provider = st.service.resolve_active_provider();
        let previous_url = st.service.get_public_url();

        let (public_url, _active_mode, active_provider, provider_metadata) =
            start_tunnel_with_normalized_request(
                &st,
                NormalizedStartRequest {
                    provider: provider.clone(),
                    mode: mode.clone(),
                    intent,
                    hostname: hostname.clone(),
                    token,
                    config_path: request_config_path,
                    selected_preset_id,
                    selected_preset_name,
                },
            )
            .await?;

        let replaced_tunnel = previous_tunnel_id.is_some()
            && (previous_mode != Some(mode.clone())
                || previous_provider != Some(active_provider.clone())
                || previous_url != Some(public_url.clone()));
        let mut revoked_bootstrap_count = 0usize;
        let mut invalidated_session_count = 0usize;
        if let Some(previous_tunnel_id) = &previous_tunnel_id
            && replaced_tunnel
        {
            let revoked = st.auth.revoke_tunnel_artifacts(previous_tunnel_id);
            revoked_bootstrap_count = revoked.revoked_bootstrap_count;
            invalidated_session_count = revoked.invalidated_session_count;
        }

        let tunnel_id = if replaced_tunnel || previous_tunnel_id.is_none() {
            random_uuid_v4()
        } else {
            previous_tunnel_id.clone().unwrap_or_default()
        };
        st.auth.set_active_tunnel(&tunnel_id, Some(public_url.as_str()), Some(mode.as_str()));

        let bootstrap_token = st
            .auth
            .issue_bootstrap_token(bootstrap_ttl_ms.map(|ttl| ttl as i64))
            .map_err(|_error| {
                // JS: a plain throw here lands in the catch-all branch.
                StartRouteError::Status(
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "ok": false, "error": "Failed to start tunnel", "code": "startup_failed" }),
                )
            })?;
        let trimmed_url = public_url
            .strip_suffix('/')
            .unwrap_or(public_url.as_str())
            .to_string();
        let connect_url = format!(
            "{}/connect?t={}",
            trimmed_url,
            encode_uri_component(&bootstrap_token.token)
        );
        let managed_remote_tunnel_config = st
            .managed_config
            .read_managed_remote_tunnel_config_from_disk()
            .await;
        let is_cloudflare_provider = active_provider == TUNNEL_PROVIDER_CLOUDFLARE;

        Ok(json!({
            "ok": true,
            "url": public_url,
            "mode": mode,
            "provider": active_provider,
            "providerMetadata": provider_metadata,
            "managedRemoteTunnelHostname": if is_cloudflare_provider { hostname_value(hostname.clone()) } else { Value::Null },
            "managedRemoteTunnelTokenPresetIds": if is_cloudflare_provider {
                managed_remote_tunnel_config.tunnels.iter().map(|entry| entry.id.clone()).collect::<Vec<_>>()
            } else {
                Vec::<String>::new()
            },
            "connectUrl": connect_url,
            "bootstrapExpiresAt": bootstrap_token.expires_at,
            "replacedTunnel": replaced_tunnel,
            "replaced": if replaced_tunnel {
                json!({ "mode": previous_mode, "provider": previous_provider, "url": previous_url })
            } else {
                Value::Null
            },
            "revokedBootstrapCount": revoked_bootstrap_count,
            "invalidatedSessionCount": invalidated_session_count,
            "policy": "tunnel-gated",
            "activeTunnelMode": mode,
            "activeSessions": sessions_value(&st.auth),
            "localPort": st.active_port.load(Ordering::SeqCst),
            "ttlConfig": {
                "bootstrapTtlMs": if bootstrap_ttl_value.is_null() { Value::Null } else { Value::from(bootstrap_ttl_ms.unwrap_or_default()) },
                "sessionTtlMs": session_ttl_ms,
            },
        }))
    }
    .await;

    // JS catch: clear the active tunnel, then map TunnelServiceError codes
    // (missing_dependency 400 / validation|provider|mode 422 / else 500).
    match started {
        Ok(value) => Json(value).into_response(),
        Err(StartRouteError::Status(status, body)) => {
            *st.controller.lock().unwrap_or_else(|e| e.into_inner()) = None;
            st.auth.clear_active_tunnel();
            (status, Json(body)).into_response()
        }
        Err(StartRouteError::Service(error)) => {
            *st.controller.lock().unwrap_or_else(|e| e.into_inner()) = None;
            st.auth.clear_active_tunnel();
            let status = axum::http::StatusCode::from_u16(error_status(&error.code))
                .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
            (
                status,
                Json(json!({ "ok": false, "error": error.message, "code": error.code })),
            )
                .into_response()
        }
    }
}

/// JS `encodeURIComponent` for the bootstrap token (base64url — identity for its
/// alphabet, but kept explicit).
fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => encoded.push(byte as char),
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

async fn post_tunnel_stop(State(st): State<ModuleState>) -> Response {
    let mut revoked_bootstrap_count = 0usize;
    let mut invalidated_session_count = 0usize;
    if let Some(active_tunnel_id) = st.auth.active_tunnel_id() {
        let revoked = st.auth.revoke_tunnel_artifacts(&active_tunnel_id);
        revoked_bootstrap_count = revoked.revoked_bootstrap_count;
        invalidated_session_count = revoked.invalidated_session_count;
    }

    let has_controller = st
        .controller
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    if has_controller {
        println!("Stopping active tunnel (user requested)...");
        st.service.stop();
    }

    st.auth.clear_active_tunnel();
    Json(json!({
        "ok": true,
        "revokedBootstrapCount": revoked_bootstrap_count,
        "invalidatedSessionCount": invalidated_session_count,
    }))
    .into_response()
}

async fn dev_tunnel_ws(State(st): State<ModuleState>, request: axum::extract::Request) -> Response {
    let (mut parts, _body) = request.into_parts();
    let port = match dev_tunnel_preflight(&st.dev, &parts).await {
        Ok(port) => port,
        Err(response) => return response,
    };
    match axum::extract::ws::WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => {
            upgrade.on_upgrade(move |socket| pipe_dev_tunnel_socket(st.dev.clone(), port, socket))
        }
        Err(_) => reject_websocket_upgrade(500, "Upgrade failed"),
    }
}

pub fn router_with(state: ModuleState) -> Router {
    Router::new()
        .route("/api/ompchamber/tunnel/check", get(tunnel_check))
        .route(
            "/api/ompchamber/tunnel/doctor",
            get(handle_tunnel_doctor).post(handle_tunnel_doctor),
        )
        .route("/api/ompchamber/tunnel/providers", get(tunnel_providers))
        .route("/api/ompchamber/tunnel/status", get(tunnel_status))
        .route(
            "/api/ompchamber/tunnel/managed-remote-token",
            put(put_managed_remote_token),
        )
        .route("/api/ompchamber/tunnel/start", post(post_tunnel_start))
        .route("/api/ompchamber/tunnel/stop", post(post_tunnel_stop))
        .route("/api/dev-tunnel", get(dev_tunnel_ws))
        .with_state(state)
}

/// Real wiring (tunnel-wiring-runtime.js `initialize`).
pub fn module_state(ctx: RouterContext) -> ModuleState {
    let http = reqwest::Client::new();
    let runner = Arc::new(RealRunner);
    let mut registry = TunnelProviderRegistry::new();
    registry
        .register(Arc::new(CloudflareTunnelProvider::new(
            runner.clone(),
            http.clone(),
        )))
        .expect("register cloudflare");
    registry
        .register(Arc::new(NgrokTunnelProvider::new(
            runner.clone(),
            http.clone(),
        )))
        .expect("register ngrok");
    let registry = Arc::new(registry);

    let controller: Arc<Mutex<Option<TunnelController>>> = Arc::new(Mutex::new(None));
    let active_port = Arc::new(AtomicU16::new(ctx.config.port));
    let port_for_service = active_port.clone();
    let get_active_port: Arc<dyn Fn() -> Option<u16> + Send + Sync> =
        Arc::new(move || Some(port_for_service.load(Ordering::SeqCst)));

    let service = Arc::new(TunnelService::new(
        registry.clone(),
        controller.clone(),
        get_active_port,
        Some(Arc::new(print_tunnel_warning)),
    ));

    let managed_config = Arc::new(ManagedTunnelConfigRuntime::new(
        ctx.config
            .data_dir
            .join("cloudflare-managed-remote-tunnels.json"),
        ctx.config.data_dir.join("cloudflare-named-tunnels.json"),
    ));

    let scanner = Arc::new(crate::dev_servers::DevServerScanner::new());
    let own_port = ctx.config.port;
    let discover: DiscoverFn = Arc::new(move || {
        let scanner = scanner.clone();
        Box::pin(async move {
            match scanner.discover(&[own_port]).await {
                crate::dev_servers::ScanOutcome::Ok { servers } => DiscoverOutcome::Available(
                    servers
                        .iter()
                        .filter_map(|server| server.get("port").and_then(Value::as_i64))
                        .map(|port| port as u16)
                        .collect(),
                ),
                crate::dev_servers::ScanOutcome::Unavailable { .. } => DiscoverOutcome::Unavailable,
            }
        })
    });

    let auth_ctx = ctx.clone();
    let resolve_auth: ResolveAuthFn = Arc::new(move |parts: &Parts| {
        // Client bearer credentials need the remote-client controller
        // (unported), so every successful resolution here is a session.
        crate::ui_auth::guard(&auth_ctx, parts)
            .ok()
            .map(|_| AuthKind::Session)
    });

    let dev = DevTunnelState {
        open_sockets: Arc::new(AtomicUsize::new(0)),
        discover,
        auth_enabled: ctx
            .config
            .ui_password
            .as_deref()
            .map(str::trim)
            .is_some_and(|password| !password.is_empty()),
        resolve_auth,
    };

    let auth = crate::client_auth::state(&ctx).tunnel_auth.clone();
    ModuleState {
        settings: crate::settings::store(&ctx),
        registry,
        service,
        auth,
        managed_config,
        controller,
        runtime_hostname: Arc::new(Mutex::new(String::new())),
        runtime_token: Arc::new(Mutex::new(String::new())),
        active_port,
        dev,
    }
}

#[cfg(test)]
#[path = "routes_tests.rs"]
mod routes_tests;
