//! Port of `server/lib/tunnels/routes.js` (`createTunnelRoutesRuntime`) plus
//! the `/api/dev-tunnel` upgrade route from `dev-tunnel/runtime.js`.
//!
//! 中文说明：本模块以 axum 实现隧道相关路由，覆盖
//! `/api/ompchamber/tunnel/*`（check、doctor、providers、status、start、
//! stop、managed-remote-token）与 `/api/dev-tunnel` WebSocket 升级入口。
//! `module_state` 负责真实装配（provider 注册、TunnelService、tunnel 鉴权、
//! 设置存储、dev-tunnel 状态），`router_with` 把处理器挂到 Router 上；
//! 各 handler 通过 `State<ModuleState>` 取得共享句柄。

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

/// 路由层共享状态：聚合 provider 注册表、隧道服务、tunnel 鉴权控制器、
/// 托管配置运行时、活跃进程句柄与 dev-tunnel 状态等进程内共享组件。
/// 由 `module_state` 真实装配（测试用构造器拼装），内部全为 Arc，克隆开销低。
#[derive(Clone)]
pub struct ModuleState {
    /// 已注册隧道 provider 的注册表，支持按 id 获取与能力列表查询。
    pub registry: Arc<TunnelProviderRegistry>,
    /// 隧道生命周期服务：start/stop、可用性检查与活跃状态查询。
    pub service: Arc<TunnelService>,
    /// TunnelAuth 控制器：bootstrap token 签发/兑换与活跃 tunnel 会话管理。
    pub auth: Arc<TunnelAuth>,
    /// 托管隧道配置运行时：managed remote token 等凭据的磁盘持久化与解析。
    pub managed_config: Arc<ManagedTunnelConfigRuntime>,
    /// 当前活跃隧道进程句柄；None 表示没有运行中的隧道进程。
    pub controller: Arc<Mutex<Option<TunnelController>>>,
    /// 最近一次 managed remote 启动写入的 hostname（内存态，供后续请求复用）。
    pub runtime_hostname: Arc<Mutex<String>>,
    /// 最近一次 managed remote 启动写入的 token（内存态，hostname 匹配时复用）。
    pub runtime_token: Arc<Mutex<String>>,
    /// 被隧道暴露的本地端口（即 server 自身监听端口），报告进 status 响应。
    pub active_port: Arc<AtomicU16>,
    /// 设置存储（迁移后视图），为 provider/mode/hostname/token 提供默认值回退。
    pub settings: Arc<SettingsStore>,
    /// dev-tunnel WebSocket 管道状态：端口发现、鉴权开关与并发计数。
    pub dev: DevTunnelState,
}

/// 当前平台枚举（macOS/Linux/Windows），供路径归一化判断使用。
fn platform_now() -> Platform {
    Platform::current()
}

/// 用户 home 目录路径字符串。
fn home_dir() -> String {
    real_home()
}

/// 进程当前工作目录字符串。
fn cwd_dir() -> String {
    real_cwd()
}

/// JS `value || null` for optional strings.
/// 中文补充：Some 且非空串时输出 JSON 字符串，否则输出 null（与 JS 真值判断一致）。
fn or_null(value: Option<String>) -> Value {
    match value {
        Some(value) if !value.is_empty() => Value::from(value),
        _ => Value::Null,
    }
}

/// 把原始 query string 解析为 HashMap；同一 key 重复出现时保留首个值
/// （对应 JS 端只取第一个参数的语义）。
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

/// 按键从已解析的 query map 中取值。
fn query_str<'a>(params: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    params.get(key).map(String::as_str)
}

/// 读取 JSON body 顶层字符串字段；字段缺失或非字符串时返回空串
/// （对应 JS 的空串兜底语义）。
fn body_str<'a>(body: &'a Value, key: &str) -> &'a str {
    body.get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
}

/// 对可选 hostname 做 managed remote 归一化；输入为空或非法时返回 None。
fn normalized_hostname(value: Option<&str>) -> Option<String> {
    normalize_managed_remote_tunnel_hostname(&value.map(Value::from).unwrap_or(Value::Null))
}

/// hostname 的 JSON 输出形态：Some 原样输出字符串，None 输出 null。
fn hostname_value(value: Option<String>) -> Value {
    match value {
        Some(value) => Value::from(value),
        None => Value::Null,
    }
}

/// 依次尝试候选 hostname（请求字段优先于设置项），返回第一个归一化成功的值；
/// 全部失败时返回 None。
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
/// 中文补充：两级回退都经过 normalize_optional_path，越界路径沿 JS 抛错
/// 语义转换为 TunnelServiceError。
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

/// 读取（已迁移的）设置 map；读取失败时退回空 map，路由不因设置损坏而失败。
async fn read_settings(st: &ModuleState) -> Map<String, Value> {
    st.settings.read_migrated().await.unwrap_or_default()
}

/// 从设置 map 中取字符串字段值。
fn settings_str<'a>(settings: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    settings.get(key).and_then(|value| value.as_str())
}

/// `resolvePreferredTunnelProvider(reqBody)`.
/// 中文补充：解析顺序为 body 显式指定 > 当前活跃 provider > 设置项
/// `tunnelProvider`；返回值总是经过 normalize_tunnel_provider 归一化。
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
/// 中文补充：该结构已跳过路由层的校验与回退逻辑，字段取最终生效值。
pub struct NormalizedStartRequest {
    /// provider 标识，如 cloudflare / ngrok。
    pub provider: String,
    /// 隧道模式：quick / managed-local / managed-remote。
    pub mode: String,
    /// 可选意图标记，透传给 provider。
    pub intent: Option<String>,
    /// managed remote 模式的目标 hostname（已归一化），可为 None。
    pub hostname: Option<String>,
    /// provider 凭据 token；空串表示未提供。
    pub token: String,
    /// managed local 模式的配置文件路径（已归一化；home 外路径会报错）。
    pub config_path: Option<String>,
    /// 选中的 managed remote 预设 id；空串表示以 hostname 兜底。
    pub selected_preset_id: String,
    /// 选中的 managed remote 预设名称；空串表示以 hostname 兜底。
    pub selected_preset_name: String,
}

/// `startTunnelWithNormalizedRequest`.
/// 中文补充：cloudflare + managed-remote 时先把 hostname/token 写入运行时
/// 状态（token 与 hostname 均非空则持久化到托管配置），再调用
/// TunnelService::start；成功返回 (public_url, active_mode, provider,
/// provider_metadata) 四元组并打印活跃日志。
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

/// 预设 id 为空时以 hostname 充当隐式 id（与 JS 端兜底一致）。
fn selected_preset_id_if<'a>(selected_preset_id: &'a str, hostname: &'a str) -> &'a str {
    if selected_preset_id.is_empty() {
        hostname
    } else {
        selected_preset_id
    }
}

/// 预设名称为空时以 hostname 充当名称。
fn selected_preset_name_if<'a>(selected_preset_name: &'a str, hostname: &'a str) -> &'a str {
    if selected_preset_name.is_empty() {
        hostname
    } else {
        selected_preset_name
    }
}

/// 把 TunnelServiceError 的 code 映射为 HTTP 状态码：missing_dependency → 400；
/// validation_error / provider_unsupported / mode_unsupported → 422；其余 → 500。
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

/// GET /tunnel/check：查询指定 provider CLI 的可用性（版本、安装命令等）。
/// 未带 provider 参数时按首选 provider 解析；provider 检查抛错时返回
/// available=false 且其余字段全 null 的 200 响应（与 JS 端 catch 行为一致）。
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
/// 中文补充：provider 不存在报 provider_unsupported；mode 过滤值不在
/// provider 模式列表内报 mode_unsupported；诊断结果的 modes 按 filter 裁剪。
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

/// GET/POST /tunnel/doctor：从 query、body、设置与磁盘托管配置聚合出
/// DiagnoseRequest——hostname 按请求字段到设置的链式回退解析，token 按
/// 请求 > 运行时（hostname 匹配时）> 托管配置 > 设置 的优先级选取，
/// 并计算 saved-profile 标记；任何 TunnelServiceError 统一映射为
/// 400 + {ok:false, error, code}。
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

/// 把当前活跃 tunnel 会话列表序列化为 JSON；序列化失败时退回空数组。
fn sessions_value(auth: &TunnelAuth) -> Value {
    serde_json::to_value(auth.list_tunnel_sessions()).unwrap_or(Value::Array(Vec::new()))
}

/// `tunnelAuthController.getActiveTunnelMode() || null`.
/// 中文补充：Some(mode) 输出字符串，None 输出 null。
fn auth_mode_value(mode: Option<String>) -> Value {
    match mode {
        Some(mode) => Value::from(mode),
        None => Value::Null,
    }
}

/// `crypto.randomUUID()` (v4) — tunnel ids in routes.js.
/// 中文补充：随机 16 字节并按 RFC 4122 设置 version/variant 位，格式化为
/// 8-4-4-4-12 的十六进制小写串。
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

/// 返回第一个非空候选串；全部为空时返回空串（token 优先链的公共实现）。
fn first_non_empty<'a>(candidates: &[&'a str]) -> &'a str {
    candidates
        .iter()
        .find(|candidate| !candidate.is_empty())
        .copied()
        .unwrap_or_default()
}

/// GET /tunnel/providers：列出全部已注册 provider 的能力（id 与模式描述）。
async fn tunnel_providers(State(st): State<ModuleState>) -> Response {
    Json(json!({ "providers": st.registry.list_capabilities() })).into_response()
}

/// 归一化设置中的 bootstrap TTL 值；键缺失时按 false（即默认 TTL）归一化。
fn ttl_value(value: Option<&Value>) -> Value {
    match value {
        Some(value) => normalize_tunnel_bootstrap_ttl_ms(value),
        None => normalize_tunnel_bootstrap_ttl_ms(&Value::Bool(false)),
    }
}

/// 从归一化后的 TTL JSON 值中取毫秒数；null 或非数字返回 None（表示默认）。
fn ttl_option(value: &Value) -> Option<u64> {
    value.as_u64()
}

/// GET /tunnel/status：隧道状态快照。无活跃隧道时返回 active=false 的完整
/// 形状（设置默认 mode/provider、托管预设摘要、本地端口与 TTL 配置）；
/// 有活跃隧道时按服务端实际 mode 重算归一化模式，并在 TunnelAuth 中的
/// tunnel id / host / mode 与实际不一致时即时同步（缺 id 则生成新 UUID），
/// 最后附带 bootstrap token 状态、活跃会话与 provider 元数据。
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

/// PUT /tunnel/managed-remote-token：保存 managed remote 隧道凭据。
/// presetId、presetName、hostname、token 任一缺失返回 400；成功则写入
/// 托管配置磁盘文件并回传更新后的 preset id 列表。
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

/// POST /tunnel/start：启动或替换隧道的主流程。先做 provider/mode 校验
/// （未知值 422），解析 hostname 与 token 优先链，归一化 bootstrap/session
/// TTL（请求值优先于设置）；随后走归一化启动路径。替换旧隧道（mode /
/// provider / url 任一变化）时吊销其 bootstrap token 与会话并按替换报告；
/// 成功后为（新分配或复用的）tunnel id 签发 bootstrap token，拼出带
/// t= 参数的 connectUrl。失败时清空 controller 与活跃 tunnel 状态，
/// 再按错误 code 映射 400/422/500。
async fn post_tunnel_start(State(st): State<ModuleState>, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));

    // 启动路由内部错误：要么携带显式 (状态码, body)，要么包装 TunnelServiceError。
    enum StartRouteError {
        // 已确定 HTTP 状态码与响应 body 的失败（校验类错误）。
        Status(axum::http::StatusCode, Value),
        // 底层服务错误，状态码由 error_status 按 code 决定。
        Service(TunnelServiceError),
    }

    // 让 ? 运算符把 TunnelServiceError 自动转换为 Service 变体。
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
/// 中文补充：按 JS encodeURIComponent 的 unreserved 字符集合逐字节做
/// 百分号编码；对 base64url 字母表是恒等映射。
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

/// POST /tunnel/stop：吊销活跃 tunnel 的全部 bootstrap token 与会话；
/// 存在 provider 进程时停止它，最后清除活跃 tunnel 状态。返回吊销计数，
/// 操作幂等（无活跃隧道时计数为 0）。
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

/// GET /api/dev-tunnel：dev-tunnel WebSocket 升级入口。先执行
/// dev_tunnel_preflight（UI 鉴权 + 端口合法性/并发/发现校验），拒绝时直接
/// 返回对应状态码的响应；通过后把升级得到的 socket 交给
/// pipe_dev_tunnel_socket 与 loopback dev server 双向透传。
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

/// 构建并返回隧道路由 Router：注册 /api/ompchamber/tunnel/* 各端点与
/// /api/dev-tunnel 升级路由，并绑定给定的共享状态。
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
/// Desktop control quit-risk probe: the shared service handle is registered
/// here once per process (composition builds a fresh state per router).
/// 中文补充：OnceLock 保证同进程只登记一次。
static SHARED_TUNNEL_SERVICE: std::sync::OnceLock<Arc<TunnelService>> = std::sync::OnceLock::new();

/// Current public tunnel URL, if any (desktop control `quitRisk`).
/// 中文补充：未启动隧道或服务尚未装配时返回 None。
pub fn tunnel_public_url() -> Option<String> {
    SHARED_TUNNEL_SERVICE.get().and_then(|service| service.get_public_url())
}

/// 真实装配 ModuleState（对应 JS tunnel-wiring-runtime 的 initialize）：
/// 注册 cloudflared 与 ngrok provider，构造 TunnelService 与托管配置运行时，
/// 为 dev-tunnel 生成基于 DevServerScanner 的端口发现闭包和基于 ui_auth
/// 的同步会话鉴权闭包（bearer 凭据因需异步文件校验而不参与 dev-tunnel
/// 握手），并把服务句柄登记进 SHARED_TUNNEL_SERVICE 供桌面端
/// quit-risk 探测读取。
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
        // Sync closure: the session cookie / URL token subset only. Client
        // bearer credentials need async file-backed auth; the dev-tunnel
        // handshake never carries one.
        crate::ui_auth::guard_sync(&auth_ctx, parts)
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
    let _ = SHARED_TUNNEL_SERVICE.set(Arc::clone(&service));
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

/// 路由层单元测试（测试体位于 routes_tests.rs）。
#[cfg(test)]
#[path = "routes_tests.rs"]
mod routes_tests;
