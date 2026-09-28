//! Port of `server/lib/quota/providers/xai.js` — xAI Grok credits via the
//! `GetGrokCreditsConfig` gRPC-web endpoint, with OAuth refresh persisted
//! back into the OpenCode `auth.json` (`xai` entry, type `oauth`).
//!
//! The billing response is hand-parsed protobuf: gRPC-web framing (5-byte
//! prefixes, optional trailer frames with `grpc-status`), then a recursive
//! field scan collecting fixed32 floats and varints with their field paths.
//! Usage percent comes from fixed32 fields at path `[1]`/`[1,1]`; the reset
//! timestamp from epoch-second varints, preferring path `[1,5,1]`.
//!
//! 中文说明：本模块是 `server/lib/quota/providers/xai.js` 的移植：通过
//! `GetGrokCreditsConfig` gRPC-web 端点获取 xAI Grokcredits 用量，OAuth
//! 刷新后的 token 会写回 OpenCode `auth.json` 的 `xai` 条目（type 为
//! `oauth`）。计费响应为手写解析的 protobuf：先剥 gRPC-web 帧（5 字节
//! 前缀、可带 `grpc-status` 的 trailer 帧），再递归扫描收集 fixed32 浮点
//! 与 varint 及其字段路径；用量百分比取路径 `[1]`/`[1,1]` 的 fixed32，
//! 重置时间取 epoch 秒 varint（优先路径 `[1,5,1]`）。

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

/// provider 注册 ID（注册表与 API 路径中使用）。
pub const PROVIDER_ID: &str = "xai";
/// provider 展示名。
pub const PROVIDER_NAME: &str = "xAI";

/// Grok 计费配置的 gRPC-web 端点（用量百分比与重置时间来源）。
const USAGE_URL: &str = "https://grok.com/grok_api_v2.GrokBuildBilling/GetGrokCreditsConfig";
/// xAI OAuth token 刷新端点。
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";
/// OAuth 刷新使用的公开 client_id（与 grok.com 前端一致）。
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
/// 判定 token 是否需要刷新的提前量（毫秒）：过期时间落在当前时间 + 该
/// 偏移内即触发刷新。
const REFRESH_SKEW_MS: f64 = 120_000.0;
/// 出站请求（用量与 token 刷新）的超时毫秒数。
const REQUEST_TIMEOUT_MS: u64 = 15_000;
/// gRPC-web 空 RPC 体的固定 5 字节帧（flag 0 + 长度 0）。
const EMPTY_GRPC_WEB_BODY: [u8; 5] = [0, 0, 0, 0, 0];

// ============== protobuf scanning ==============

/// protobuf 扫描的可变游标：当前字节下标与 fixed32 字段的发现顺序。
struct ScanState {
    /// 下一个待读字节的下标。
    index: usize,
    /// fixed32 字段的发现序号（用于稳定排序，保证 JS 语义下的确定性）。
    order: usize,
}

/// 扫描到的一个 varint 字段：字段路径与解码值。
#[derive(Debug, Clone)]
struct VarintField {
    /// 字段路径（从消息根部到该字段的 field number 序列）。
    path: Vec<u32>,
    /// varint 解码值。
    value: u64,
}

/// 扫描到的一个 fixed32 字段：路径、浮点值与发现顺序。
#[derive(Debug, Clone)]
struct Fixed32Field {
    /// 字段路径（从消息根部到该字段的 field number 序列）。
    path: Vec<u32>,
    /// 小端 fixed32 解释出的 f32 值。
    value: f32,
    /// 发现顺序（同路径多值时的稳定排序依据）。
    order: usize,
}

/// 一次 protobuf 扫描的汇总结果：全部 fixed32 与 varint 字段。
#[derive(Default, Debug)]
struct ScanResult {
    /// 收集到的 fixed32 字段（含嵌套消息内的）。
    fixed32_fields: Vec<Fixed32Field>,
    /// 收集到的 varint 字段（含嵌套消息内的）。
    varint_fields: Vec<VarintField>,
}

/// `readVarint` — None on truncation or an over-long final byte.
///
/// 中文说明：读取一个 varint；截断、超过 64 位或最高字节的保留位非零
/// 都返回 `None`（对齐 JS `readVarint` 的失败语义）。
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

/// 路径相等比较（语义即切片相等，独立成函数以对应 JS 的 `samePath`）。
fn same_path(left: &[u32], right: &[u32]) -> bool {
    left == right
}

/// `USAGE_PERCENT_PATHS` — the flat billing message and the response envelope.
///
/// 中文说明：用量百分比所在的两个字段路径：扁平计费消息 `[1]` 与响应
/// 信封 `[1,1]`。
const USAGE_PERCENT_PATHS: [&[u32]; 2] = [&[1], &[1, 1]];

/// 判断候选路径是否命中给定路径集合中的任一路径。
fn has_path(paths: &[&[u32]], candidate: &[u32]) -> bool {
    paths.iter().any(|path| same_path(path, candidate))
}

/// `scanProtobuf` — Err mirrors the JS `false` return.
///
/// 中文说明：递归扫描 protobuf 字节（对应 JS `scanProtobuf`）：按
/// wire type 分派——varint 记入 `varint_fields`，fixed32 记入
/// `fixed32_fields`（携带顺序号），64-bit 定长字段跳过，length-delimited
/// 字段在深度 <4 时递归扫描（≥4 且非空则视为非法）。key 为 0、字段号
/// 越界、截断等任何非法输入都返回 `Err`（对应 JS 返回 `false`）。
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

/// gRPC-web 帧解析结果。
enum Framed {
    /// Not gRPC-web framed at all (fallback to raw protobuf).
    NotFramed,
    /// 帧结构存在但格式非法（对应 JS 的 malformed 分支）。
    Malformed,
    /// 合法帧：解出的数据消息与 trailer 中的 grpc-status 列表。
    Messages {
        /// 数据帧的 payload（protobuf 消息体）。
        messages: Vec<Vec<u8>>,
        /// trailer 帧里解析出的 `grpc-status` 值。
        trailer_statuses: Vec<i64>,
    },
}

/// `parseFrames`.
///
/// 中文说明：解析 gRPC-web 帧序列（对应 JS `parseFrames`）：每帧为
/// 1 字节 flag + 4 字节大端长度 + payload；首字节高位标记 trailer，
/// trailer 之后不允许再出现数据帧。总长不足 5 字节或首字节异常视为
/// 未帧化（回退按裸 protobuf 解析），其余任何违规都判 Malformed。
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
///
/// 中文说明：解析 trailer 帧中的 HTTP 风格头，提取 `grpc-status: N`
/// （仅接受纯数字，出现两次或非法格式都算失败返回 `None`）。
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
///
/// 中文说明：裸字节是否"像" protobuf（对应 JS `looksLikeProtobuf`）：
/// 首字节需解出非零字段号且 wire type 属于 0/1/2/5，作为未帧化响应
/// 的兜底判定。
fn looks_like_protobuf(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let field_number = bytes[0] >> 3;
    let wire_type = bytes[0] & 0x07;
    field_number > 0 && matches!(wire_type, 0 | 1 | 2 | 5)
}

/// 解析出的 xAI 用量：当前计费周期已用百分比与重置时间。
#[derive(Debug)]
pub struct XaiUsage {
    /// 已用百分比（0–100）；来自路径 `[1]`/`[1,1]` 的 fixed32 字段。
    pub used_percent: Option<f64>,
    /// 重置时间的 epoch 毫秒；来自 epoch 秒 varint（优先路径 `[1,5,1]`）。
    pub reset_at: Option<i64>,
}

/// `parseUsage`.
///
/// 中文说明：解析计费响应（对应 JS `parseUsage`）：先帧化并校验
/// grpc-status（非 0 报 "xAI billing RPC failed with status N"），未帧化
/// 但像 protobuf 的按裸消息处理；随后扫描各 payload，按路径与顺序选
/// 出用量百分比，varint 里落在 2024–2033 年区间且晚于 `now_ms` 的值
/// 作为重置时间候选（优先 `[1,5,1]` 路径，取最早）。无百分比但存在
/// 用量周期字段与重置时间时按 0% 处理；完全无可用数据则报错。
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

/// 读取 auth.json 中的 xAI OAuth 条目：必须是对象、type 为 `oauth` 且
/// `access`/`refresh` 至少一个非空；不满足时返回 `Ok(None)`（视为未
/// 配置），仅 auth 读取失败透传为 `Err`。
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

/// 解码 JWT 的 payload 段（base64url）为 claims 对象；分割、解码或 JSON
/// 解析任一失败都返回 `None`。
fn decode_jwt_claims(token: &str) -> Option<Map<String, Value>> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims.as_object().cloned()
}

/// 判断 token 是否需要刷新：无 access 直接需要；存储的 `expires` 落在
/// 当前时间 + [`REFRESH_SKEW_MS`] 之内需要；否则解码 JWT `exp` 再判一次
///（同样带提前量）。
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
///
/// 中文说明：刷新 OAuth（对应 JS `refreshXaiOauth`）：经 runtime 持有的
/// [`SharedSlot`] 合并并发刷新，向 [`TOKEN_URL`] 发 form 请求换取新
/// access token（`expires_in` 缺省 3600，非数字视为错误），把新
/// access/refresh/expires 写回 entry 并整体持久化进 `auth.json` 的
/// `xai` 条目，返回刷新后的条目。
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

/// 确保拿到未过期的 access：token 仍新鲜直接返回原条目；需要刷新但
/// 无可用 refresh token 时报错；否则执行（合并的）刷新。
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
///
/// 中文说明：拉取用量（对应 JS `fetchUsage`）：向 [`USAGE_URL`] 发带
/// bearer token 与 grok.com Origin/Referer 头的 gRPC-web POST（空 RPC
/// 体、15 秒超时）；优先校验响应头里的 `grpc-status`（非 0 报错），
/// 再校验 HTTP 状态，最后把 body 交给 [`parse_usage`]。
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

/// provider 是否已配置：auth.json 中存在可用的 xAI OAuth 条目（读取
/// 失败按未配置处理）。
pub fn is_configured(deps: &QuotaDeps) -> bool {
    read_xai_auth(deps).unwrap_or(None).is_some()
}

/// 配额抓取主入口（注册表 `fetch` 指向此处）：未配置返回 "Not
/// configured" 信封；auth 读取失败返回固定错误串；必要时先刷新 OAuth，
/// 再拉取用量并组装 `billing_cycle` 用量窗口（百分比 + 重置时间），
/// 成功产出 `ok/configured=true` 的结果信封，失败产出错误信封。
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

/// 刷新 future 的共享类型：并发刷新合并为同一个在途 future。
pub type XaiRefreshFuture = futures::future::Shared<BoxFuture<'static, Result<Value, String>>>;

/// The refresh coalescing slot (held by the runtime).
///
/// 中文说明：OAuth 刷新的合并槽位（由 [`QuotaRuntime`] 持有，跨请求共享）。
pub struct XaiRefreshState {
    /// 在途刷新的共享槽位：并发调用共享同一结果。
    pub pending: Arc<SharedSlot<Result<Value, String>>>,
}

/// [`XaiRefreshState`] 的构造。
impl XaiRefreshState {
    /// 创建带全新空槽位的刷新状态。
    pub fn new() -> Self {
        Self {
            pending: Arc::new(SharedSlot::new()),
        }
    }
}

/// [`XaiRefreshState`] 的 `Default` 委托给 [`XaiRefreshState::new`]。
impl Default for XaiRefreshState {
    /// 等价于 [`XaiRefreshState::new`]。
    fn default() -> Self {
        Self::new()
    }
}
