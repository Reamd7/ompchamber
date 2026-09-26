//! Port of `server/lib/quota/providers/xai.js` — xAI Grok credits via the
//! `GetGrokCreditsConfig` gRPC-web endpoint, with OAuth refresh persisted
//! back into the OpenCode `auth.json` (`xai` entry, type `oauth`).
//!
//! The billing response is hand-parsed protobuf: gRPC-web framing (5-byte
//! prefixes, optional trailer frames with `grpc-status`), then a recursive
//! field scan collecting fixed32 floats and varints with their field paths.
//! Usage percent comes from fixed32 fields at path `[1]`/`[1,1]`; the reset
//! timestamp from epoch-second varints, preferring path `[1,5,1]`.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::BoxFuture;
use serde_json::{Map, Value, json};

use crate::quota::deps::QuotaDeps;
use crate::quota::http::{HttpError, HttpRequest};
use crate::quota::runtime::{QuotaRuntime, SharedSlot};
use crate::quota::utils::{
    build_result, field, non_empty_string_value, to_number, to_usage_window, usage_payload,
};

pub const PROVIDER_ID: &str = "xai";
pub const PROVIDER_NAME: &str = "xAI";

const USAGE_URL: &str = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const REFRESH_SKEW_MS: f64 = 120_000.0;
const REQUEST_TIMEOUT_MS: u64 = 15_000;
const EMPTY_GRPC_WEB_BODY: [u8; 5] = [0, 0, 0, 0, 0];

// ============== protobuf scanning ==============

struct ScanState {
    index: usize,
    order: usize,
}

#[derive(Debug, Clone)]
struct VarintField {
    path: Vec<u32>,
    value: u64,
}

#[derive(Debug, Clone)]
struct Fixed32Field {
    path: Vec<u32>,
    value: f32,
    order: usize,
}

#[derive(Default, Debug)]
struct ScanResult {
    fixed32_fields: Vec<Fixed32Field>,
    varint_fields: Vec<VarintField>,
}

/// `readVarint` — None on truncation or an over-long final byte.
fn read_varint(bytes: &[u8], state: &mut ScanState) -> Option<u64> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    while state.index < bytes.len() && shift < 64 {
        let byte = bytes[state.index];
        state.index += 1;
        if shift == 63 && (byte & 0x7e) != 0 {
            return None;
        }
        value |= ((byte & 0x7f) as u64) << shift;
        if (byte & 0x80) == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}

fn same_path(left: &[u32], right: &[u32]) -> bool {
    left == right
}

/// `USAGE_PERCENT_PATHS` — the flat billing message and the response envelope.
const USAGE_PERCENT_PATHS: [&[u32]; 2] = [&[1], &[1, 1]];

fn has_path(paths: &[&[u32]], candidate: &[u32]) -> bool {
    paths.iter().any(|path| same_path(path, candidate))
}

/// `scanProtobuf` — Err mirrors the JS `false` return.
fn scan_protobuf(
    bytes: &[u8],
    path: &[u32],
    depth: usize,
    state: &mut ScanState,
) -> Result<ScanResult, ()> {
    let mut result = ScanResult::default();

    while state.index < bytes.len() {
        let key = read_varint(bytes, state).ok_or(())?;
        if key == 0 {
            return Err(());
        }
        let field_number = (key >> 3) as u32;
        let wire_type = (key & 0x07) as u8;
        if field_number == 0 || field_number > 0x1fff_ffff {
            return Err(());
        }
        let mut field_path = path.to_vec();
        field_path.push(field_number);

        match wire_type {
            0 => {
                let value = read_varint(bytes, state).ok_or(())?;
                result.varint_fields.push(VarintField {
                    path: field_path,
                    value,
                });
            }
            1 => {
                if state.index + 8 > bytes.len() {
                    return Err(());
                }
                state.index += 8;
            }
            2 => {
                let length = read_varint(bytes, state).ok_or(())?;
                if length > (bytes.len() - state.index) as u64 {
                    return Err(());
                }
                let end = state.index + length as usize;
                if depth >= 4 && length != 0 {
                    return Err(());
                }
                if depth < 4 {
                    let mut nested_state = ScanState {
                        index: 0,
                        order: state.order,
                    };
                    let nested = scan_protobuf(
                        &bytes[state.index..end],
                        &field_path,
                        depth + 1,
                        &mut nested_state,
                    )?;
                    result.fixed32_fields.extend(nested.fixed32_fields);
                    result.varint_fields.extend(nested.varint_fields);
                    state.order = nested_state.order;
                }
                state.index = end;
            }
            5 => {
                if state.index + 4 > bytes.len() {
                    return Err(());
                }
                let value = f32::from_le_bytes([
                    bytes[state.index],
                    bytes[state.index + 1],
                    bytes[state.index + 2],
                    bytes[state.index + 3],
                ]);
                result.fixed32_fields.push(Fixed32Field {
                    path: field_path,
                    value,
                    order: state.order,
                });
                state.order += 1;
                state.index += 4;
            }
            _ => return Err(()),
        }
    }

    Ok(result)
}

enum Framed {
    /// Not gRPC-web framed at all (fallback to raw protobuf).
    NotFramed,
    Malformed,
    Messages {
        messages: Vec<Vec<u8>>,
        trailer_statuses: Vec<i64>,
    },
}

/// `parseFrames`.
fn parse_frames(bytes: &[u8]) -> Framed {
    if bytes.len() < 5 || (bytes[0] & 0x7f) != 0 {
        return Framed::NotFramed;
    }
    let mut messages = Vec::new();
    let mut trailer_statuses = Vec::new();
    let mut trailer_started = false;
    let mut index = 0;

    while index < bytes.len() {
        if index + 5 > bytes.len() {
            return Framed::Malformed;
        }
        let flags = bytes[index];
        index += 1;
        if (flags & 0x7f) != 0 {
            return Framed::Malformed;
        }
        let is_trailer = (flags & 0x80) != 0;
        if trailer_started && !is_trailer {
            return Framed::Malformed;
        }
        let length = (bytes[index] as u64) * 0x100_0000
            + (bytes[index + 1] as u64) * 0x1_0000
            + (bytes[index + 2] as u64) * 0x100
            + (bytes[index + 3] as u64);
        index += 4;
        let end = index + length as usize;
        if end > bytes.len() {
            return Framed::Malformed;
        }
        let payload = &bytes[index..end];
        if is_trailer {
            trailer_started = true;
            match parse_grpc_trailer_status(payload) {
                Some(status) => trailer_statuses.push(status),
                None => return Framed::Malformed,
            }
        } else {
            messages.push(payload.to_vec());
        }
        index = end;
    }

    Framed::Messages {
        messages,
        trailer_statuses,
    }
}

/// `parseGrpcTrailerStatus` — `grpc-status: N` headers, digits only.
fn parse_grpc_trailer_status(bytes: &[u8]) -> Option<i64> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut status: Option<i64> = None;
    for line in text.split("\r\n").flat_map(|line| line.split('\n')) {
        if line.is_empty() {
            continue;
        }
        let separator = line.find(':')?;
        if separator == 0 {
            return None;
        }
        let key = line[..separator].trim().to_ascii_lowercase();
        if key.is_empty() {
            return None;
        }
        if key != "grpc-status" {
            continue;
        }
        if status.is_some() {
            return None;
        }
        let raw_status = line[separator + 1..].trim();
        if raw_status.is_empty() || !raw_status.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let parsed: i64 = raw_status.parse().ok()?;
        if parsed > i32::MAX as i64 {
            return None;
        }
        status = Some(parsed);
    }
    status
}

/// `looksLikeProtobuf` — plausible first key byte.
fn looks_like_protobuf(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let field_number = bytes[0] >> 3;
    let wire_type = bytes[0] & 0x07;
    field_number > 0 && matches!(wire_type, 0 | 1 | 2 | 5)
}

#[derive(Debug)]
pub struct XaiUsage {
    pub used_percent: Option<f64>,
    pub reset_at: Option<i64>,
}

/// `parseUsage`.
pub fn parse_usage(bytes: &[u8], now_ms: u64) -> Result<XaiUsage, String> {
    let payloads: Vec<Vec<u8>> = match parse_frames(bytes) {
        Framed::Malformed => {
            return Err("xAI billing returned malformed gRPC-web framing".to_string());
        }
        Framed::NotFramed => {
            if looks_like_protobuf(bytes) {
                vec![bytes.to_vec()]
            } else {
                Vec::new()
            }
        }
        Framed::Messages {
            messages,
            trailer_statuses,
        } => {
            for status in trailer_statuses {
                if status != 0 {
                    return Err(format!("xAI billing RPC failed with status {status}"));
                }
            }
            messages
        }
    };
    if payloads.is_empty() {
        return Err("xAI billing returned an empty protobuf response".to_string());
    }

    let mut scan = ScanResult::default();
    for payload in &payloads {
        let mut state = ScanState { index: 0, order: 0 };
        match scan_protobuf(payload, &[], 0, &mut state) {
            Ok(result) => {
                scan.fixed32_fields.extend(result.fixed32_fields);
                scan.varint_fields.extend(result.varint_fields);
            }
            Err(_) => return Err("xAI billing returned malformed protobuf".to_string()),
        }
    }

    let mut percentages: Vec<&Fixed32Field> = scan
        .fixed32_fields
        .iter()
        .filter(|field| {
            has_path(&USAGE_PERCENT_PATHS, &field.path)
                && field.value.is_finite()
                && field.value >= 0.0
                && field.value <= 100.0
        })
        .collect();
    percentages.sort_by(|left, right| {
        left.path
            .len()
            .cmp(&right.path.len())
            .then(left.order.cmp(&right.order))
    });
    let used_percent = percentages.first().map(|field| field.value as f64);

    let reset_candidates: Vec<(&VarintField, i64)> = scan
        .varint_fields
        .iter()
        .filter_map(|field| {
            let value = field.value;
            if (1_700_000_000..=2_100_000_000).contains(&value) {
                let reset_at = (value as i64) * 1000;
                (reset_at > now_ms as i64).then_some((field, reset_at))
            } else {
                None
            }
        })
        .collect();
    let preferred: Vec<&(&VarintField, i64)> = reset_candidates
        .iter()
        .filter(|(field, _)| same_path(&field.path, &[1, 5, 1]))
        .collect();
    let pool: Vec<&(&VarintField, i64)> = if preferred.is_empty() {
        reset_candidates.iter().collect()
    } else {
        preferred
    };
    let reset_at = pool.iter().map(|(_, reset_at)| *reset_at).min();

    let has_usage_period = scan.varint_fields.iter().any(|field| {
        (field.path.len() >= 2 && field.path[0] == 1 && field.path[1] == 6)
            || (same_path(&field.path, &[1, 8, 1]) && (field.value == 1 || field.value == 2))
    });

    if used_percent.is_none()
        && scan.fixed32_fields.is_empty()
        && reset_at.is_some()
        && has_usage_period
    {
        return Ok(XaiUsage {
            used_percent: Some(0.0),
            reset_at,
        });
    }
    if used_percent.is_none() {
        return Err("xAI billing response had no usable current-period usage".to_string());
    }
    Ok(XaiUsage {
        used_percent,
        reset_at,
    })
}

// ============== OAuth ==============

fn read_xai_auth(deps: &QuotaDeps) -> Result<Option<Value>, String> {
    let auth = deps.read_auth_value()?;
    let Some(entry) = field(&auth, "xai") else {
        return Ok(None);
    };
    if !entry.is_object() || field(entry, "type").and_then(Value::as_str) != Some("oauth") {
        return Ok(None);
    }
    let has_access = field(entry, "access")
        .and_then(non_empty_string_value)
        .is_some();
    let has_refresh = field(entry, "refresh")
        .and_then(non_empty_string_value)
        .is_some();
    if !has_access && !has_refresh {
        return Ok(None);
    }
    Ok(Some(entry.clone()))
}

fn decode_jwt_claims(token: &str) -> Option<Map<String, Value>> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims.as_object().cloned()
}

fn token_needs_refresh(deps: &QuotaDeps, entry: &Value) -> bool {
    let Some(access) = field(entry, "access").and_then(non_empty_string_value) else {
        return true;
    };
    let now = deps.now_ms() as f64;
    let refresh_deadline = now + REFRESH_SKEW_MS;

    if let Some(stored_expiry) = to_number(field(entry, "expires"))
        && stored_expiry <= refresh_deadline
    {
        return true;
    }

    let jwt_expiry = decode_jwt_claims(&access)
        .and_then(|claims| claims.get("exp").and_then(Value::as_f64))
        .map(|exp| exp * 1000.0);
    matches!(jwt_expiry, Some(expiry) if expiry.is_finite() && expiry <= refresh_deadline)
}

/// `refreshXaiOauth` — coalesced, persisted back into `auth.json`.
fn refresh_xai_oauth(
    rt: &Arc<QuotaRuntime>,
    entry: Value,
) -> BoxFuture<'static, Result<Value, String>> {
    let pending = rt.xai_refresh.pending.clone();
    let rt = rt.clone();
    let shared = pending.subscribe(move || {
        let rt = rt.clone();
        let entry = entry.clone();
        Box::pin(async move {
            let deps = rt.deps.clone();
            let Some(refresh_token) = field(&entry, "refresh").and_then(non_empty_string_value)
            else {
                return Err("xAI OAuth entry has no usable refresh token".to_string());
            };

            let form = format!(
                "client_id={}&refresh_token={}&grant_type=refresh_token",
                CLIENT_ID, refresh_token
            );
            let request = HttpRequest::post(TOKEN_URL)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .form_body(form)
                .timeout(REQUEST_TIMEOUT_MS);

            let response = match (deps.http)(request).await {
                Ok(response) => response,
                Err(HttpError::Timeout) => return Err("The operation timed out".to_string()),
                Err(HttpError::Other(message)) => return Err(message),
            };
            if !response.ok() {
                return Err(format!(
                    "xAI OAuth refresh failed with HTTP {}",
                    response.status
                ));
            }
            let payload = match response.json() {
                Ok(payload) => payload,
                Err(_) => return Err("xAI OAuth refresh returned invalid JSON".to_string()),
            };

            let Some(access) = field(&payload, "access_token").and_then(non_empty_string_value)
            else {
                return Err("xAI OAuth refresh returned no access token".to_string());
            };
            // JS: `payload?.expires_in ?? 3600`, then a typeof-number check —
            // a non-numeric expiry is an error, not a default.
            let expires_in = match field(&payload, "expires_in") {
                None | Some(Value::Null) => 3600.0,
                Some(value) => value.as_f64().unwrap_or(f64::NAN),
            };
            if !expires_in.is_finite() {
                return Err("xAI OAuth refresh returned an invalid expiry".to_string());
            }
            let expires = deps.now_ms() as i64 + (expires_in * 1000.0) as i64;

            let mut refreshed = entry.as_object().cloned().unwrap_or_default();
            refreshed.insert("type".into(), json!("oauth"));
            refreshed.insert("access".into(), json!(access));
            let next_refresh = field(&payload, "refresh_token")
                .and_then(non_empty_string_value)
                .unwrap_or_else(|| refresh_token.clone());
            refreshed.insert("refresh".into(), json!(next_refresh));
            refreshed.insert("expires".into(), json!(expires));
            let refreshed = Value::Object(refreshed);

            let auth = deps.read_auth_value()?;
            let mut auth_map = auth.as_object().cloned().unwrap_or_default();
            auth_map.insert("xai".into(), refreshed.clone());
            (deps.write_auth)(&Value::Object(auth_map))?;
            Ok(refreshed)
        })
    });
    Box::pin(shared)
}

async fn ensure_fresh_access(rt: &Arc<QuotaRuntime>, entry: Value) -> Result<Value, String> {
    if !token_needs_refresh(&rt.deps, &entry) {
        return Ok(entry);
    }
    if field(&entry, "refresh")
        .and_then(non_empty_string_value)
        .is_none()
    {
        return Err(
            "xAI OAuth access token is expired and has no usable refresh token".to_string(),
        );
    }
    refresh_xai_oauth(rt, entry).await
}

/// `fetchUsage` — gRPC-web POST with the header-carried grpc-status check.
async fn fetch_usage(deps: &QuotaDeps, access_token: &str) -> Result<XaiUsage, String> {
    let request = HttpRequest::post(USAGE_URL)
        .bearer(access_token)
        .header("Origin", "https://grok.com")
        .header("Referer", "https://grok.com/?_s=usage")
        .header("Accept", "*/*")
        .raw_body("application/grpc-web+proto", EMPTY_GRPC_WEB_BODY.to_vec())
        .header("x-grpc-web", "1")
        .header("x-user-agent", "connect-es/2.1.1")
        .header("User-Agent", "OMPChamber")
        .timeout(REQUEST_TIMEOUT_MS);

    let response = (deps.http)(request).await.map_err(|error| match error {
        HttpError::Timeout => "The operation timed out".to_string(),
        HttpError::Other(message) => message,
    })?;

    if let Some(header_status) = response.header("grpc-status") {
        let trimmed = header_status.trim();
        if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return Err("xAI billing returned malformed gRPC status".to_string());
        }
        let status: i64 = trimmed
            .parse()
            .map_err(|_| "xAI billing returned malformed gRPC status".to_string())?;
        if status > i32::MAX as i64 {
            return Err("xAI billing returned malformed gRPC status".to_string());
        }
        if status != 0 {
            return Err(format!("xAI billing RPC failed with status {status}"));
        }
    }
    if !response.ok() {
        return Err(format!(
            "xAI billing request failed with HTTP {}",
            response.status
        ));
    }
    parse_usage(&response.body, deps.now_ms())
}

pub fn is_configured(deps: &QuotaDeps) -> bool {
    read_xai_auth(deps).unwrap_or(None).is_some()
}

pub fn fetch_quota(rt: Arc<QuotaRuntime>) -> BoxFuture<'static, Value> {
    Box::pin(async move {
        let deps = rt.deps.clone();
        let now = deps.now_ms();

        let entry = match read_xai_auth(&deps) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                return build_result(
                    PROVIDER_ID,
                    PROVIDER_NAME,
                    false,
                    false,
                    None,
                    Some("Not configured"),
                    None,
                    now,
                );
            }
            Err(_) => {
                // readXaiAuth reports a fixed error string, not the io message.
                return build_result(
                    PROVIDER_ID,
                    PROVIDER_NAME,
                    false,
                    true,
                    None,
                    Some("Failed to read xAI OAuth credentials"),
                    None,
                    now,
                );
            }
        };

        let failure = |message: String, now: u64| {
            build_result(
                PROVIDER_ID,
                PROVIDER_NAME,
                false,
                true,
                None,
                Some(&message),
                None,
                now,
            )
        };

        let fresh_entry = match ensure_fresh_access(&rt, entry).await {
            Ok(entry) => entry,
            Err(message) => return failure(message, deps.now_ms()),
        };
        let Some(access_token) = field(&fresh_entry, "access").and_then(non_empty_string_value)
        else {
            return failure(
                "xAI OAuth entry has no usable access token".to_string(),
                deps.now_ms(),
            );
        };

        let usage = match fetch_usage(&deps, &access_token).await {
            Ok(usage) => usage,
            Err(message) => return failure(message, deps.now_ms()),
        };

        let now = deps.now_ms();
        let mut windows = Map::new();
        let reset_at = usage.reset_at.map(|ms| json!(ms));
        windows.insert(
            "billing_cycle".into(),
            to_usage_window(now, usage.used_percent, None, reset_at.as_ref(), None),
        );

        build_result(
            PROVIDER_ID,
            PROVIDER_NAME,
            true,
            true,
            Some(usage_payload(windows, None)),
            None,
            None,
            deps.now_ms(),
        )
    })
}

pub type XaiRefreshFuture = futures::future::Shared<BoxFuture<'static, Result<Value, String>>>;

/// The refresh coalescing slot (held by the runtime).
pub struct XaiRefreshState {
    pub pending: Arc<SharedSlot<Result<Value, String>>>,
}

impl XaiRefreshState {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(SharedSlot::new()),
        }
    }
}

impl Default for XaiRefreshState {
    fn default() -> Self {
        Self::new()
    }
}
