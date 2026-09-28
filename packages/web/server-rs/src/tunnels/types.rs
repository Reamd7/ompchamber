//! Port of `server/lib/tunnels/types.js`: tunnel constants, normalization, and
//! shared validation. Platform-parameterized path helpers mirror the JS
//! `path`/`path.win32` switching driven by the `platform` argument.
//! （中文说明）tunnel 公共类型与工具集：定义 provider/mode/intent 常量与
//! 白名单；提供归一化函数（provider/mode/intent/可选路径）、平台参数化
//! 的路径操作（join/resolve/目录包含判断/波浪线展开并限制在 home 内），
//! 以及启动请求的合并归一化与按 provider 能力表的校验。与 JS 版一致：
//! 非法 provider/mode 回退默认值，非法 intent 归为 None，configPath 越出
//! home 目录时返回 validation_error。

use serde_json::Value;

/// tunnel provider 标识：Cloudflare（默认 provider）。
pub const TUNNEL_PROVIDER_CLOUDFLARE: &str = "cloudflare";
/// tunnel provider 标识：ngrok。
pub const TUNNEL_PROVIDER_NGROK: &str = "ngrok";

/// tunnel 模式：快速临时隧道，无需账号即可获得随机公开 URL。
pub const TUNNEL_MODE_QUICK: &str = "quick";
/// tunnel 模式：远程受管隧道，由 Cloudflare 云端管理（需 token 与 hostname）。
pub const TUNNEL_MODE_MANAGED_REMOTE: &str = "managed-remote";
/// tunnel 模式：本地受管隧道，由本地 cloudflared 配置文件驱动。
pub const TUNNEL_MODE_MANAGED_LOCAL: &str = "managed-local";

/// tunnel 意图：临时公开访问（quick 模式绑定的意图）。
pub const TUNNEL_INTENT_EPHEMERAL_PUBLIC: &str = "ephemeral-public";
/// tunnel 意图：持久公开访问（managed 系列模式绑定的意图）。
pub const TUNNEL_INTENT_PERSISTENT_PUBLIC: &str = "persistent-public";
/// tunnel 意图：私有网络访问（当前仅作为合法意图值保留）。
pub const TUNNEL_INTENT_PRIVATE_NETWORK: &str = "private-network";

/// 合法 intent 白名单，归一化与启动校验共用。
const SUPPORTED_TUNNEL_INTENTS: [&str; 3] = [
    TUNNEL_INTENT_EPHEMERAL_PUBLIC,
    TUNNEL_INTENT_PERSISTENT_PUBLIC,
    TUNNEL_INTENT_PRIVATE_NETWORK,
];

/// 合法 mode 白名单，归一化与启动校验共用。
const SUPPORTED_TUNNEL_MODES: [&str; 3] = [
    TUNNEL_MODE_QUICK,
    TUNNEL_MODE_MANAGED_REMOTE,
    TUNNEL_MODE_MANAGED_LOCAL,
];

/// 合法 provider 白名单；未知值在归一化时回退为 cloudflare。
const SUPPORTED_TUNNEL_PROVIDERS: [&str; 2] = [TUNNEL_PROVIDER_CLOUDFLARE, TUNNEL_PROVIDER_NGROK];

/// `TunnelServiceError` (`code` + `message`; `details` is always null in the JS
/// source and never crosses the wire).
/// （中文）tunnel 服务错误：`code` 为稳定机器码（validation_error、
/// provider_unsupported、mode_unsupported 等），路由层据此映射响应；
/// `message` 直接面向用户展示；thiserror 让 Display 输出 message。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TunnelServiceError {
    /// 稳定错误码（snake_case），调用方按它分支处理。
    pub code: String,
    /// 人类可读的错误描述（英文文案与 JS 版保持一致）。
    pub message: String,
}

/// 错误构造的辅助方法。
impl TunnelServiceError {
    /// 用任意可转为 String 的入参构造错误，便于直接传字面量与格式化串。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// 快捷构造 `code = validation_error` 的请求校验错误。
    pub fn validation_error(message: impl Into<String>) -> Self {
        Self::new("validation_error", message)
    }
}

/// The platforms `types.js` switches on (`path.win32` vs `path`).
/// （中文）路径操作的目标平台。JS 版按入参在 `path.win32` 与 `path`
/// 之间切换；这里显式建模成枚举，使任意宿主机都能离线测试两种路径形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Windows：分隔符为反斜杠，路径可带盘符前缀，比较不区分大小写。
    Win32,
    /// macOS/Linux：分隔符为斜杠，绝对路径以根斜杠起始。
    Posix,
}

/// 平台相关的辅助方法。
impl Platform {
    /// 当前进程所在平台（编译期由 cfg!(windows) 决定，无运行时开销）。
    pub fn current() -> Self {
        if cfg!(windows) {
            Platform::Win32
        } else {
            Platform::Posix
        }
    }

    /// JS `process.platform` string for responses.
    /// （中文）返回 JS `process.platform` 风格名称（win32/darwin/linux），
    /// 仅用于构造响应与依赖安装提示文案。
    pub fn js_name(self) -> &'static str {
        match self {
            Platform::Win32 => "win32",
            Platform::Posix => {
                if cfg!(target_os = "macos") {
                    "darwin"
                } else {
                    "linux"
                }
            }
        }
    }

    /// 该平台的路径分隔符：Win32 为反斜杠，Posix 为斜杠。
    fn sep(self) -> char {
        match self {
            Platform::Win32 => '\\',
            Platform::Posix => '/',
        }
    }
}

/// 判断片段是否为 Windows 盘符前缀（单个 ASCII 字母后跟冒号）。
fn is_win_drive_prefix(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

/// 统一路径分隔符为目标平台形态：Win32 把正斜杠替换为反斜杠，Posix
/// 原样返回（对应 JS path 模块对两种分隔符的容忍）。
fn normalize_separators(value: &str, platform: Platform) -> String {
    match platform {
        Platform::Win32 => value.replace('/', "\\"),
        Platform::Posix => value.to_string(),
    }
}

/// `pathApi.join(base, rest)`: normalize separators, then join.
/// （中文）`pathApi.join(base, rest)` 的移植：先统一两侧分隔符，再去掉
/// base 尾部与 rest 头部的重复分隔符后拼接；任一侧为空则返回另一侧。
pub fn platform_join(base: &str, rest: &str, platform: Platform) -> String {
    let sep = platform.sep();
    let base = normalize_separators(base, platform);
    let rest = normalize_separators(rest, platform);
    let base = base.trim_end_matches(sep);
    let rest = rest.trim_start_matches(sep);
    if base.is_empty() {
        return rest.to_string();
    }
    if rest.is_empty() {
        return base.to_string();
    }
    format!("{base}{sep}{rest}")
}

/// `pathApi.resolve(value)` against `cwd`, implemented for the subset the
/// module's inputs use (absolute paths, drive letters, `.`/`..` segments).
/// （中文）以 `cwd` 为基准把 `value` 解析为绝对路径，仅覆盖本模块输入
/// 用到的子集：绝对路径、Windows 盘符与当前盘根相对路径、点段的折叠。
/// UNC 路径原样返回（tunnels 模块不会收到 UNC 输入）；组件折叠到空时
/// Posix 返回根目录、Win32 返回盘符加分隔符，与 Node 的 resolve 对齐。
pub fn platform_resolve(value: &str, cwd: &str, platform: Platform) -> String {
    let sep = platform.sep();
    let value = normalize_separators(value, platform);
    let cwd = normalize_separators(cwd, platform);
    let posix_absolute = platform == Platform::Posix && value.starts_with('/');

    let mut prefix = String::new();
    let mut components: Vec<String> = Vec::new();

    let rest: String = match platform {
        Platform::Posix => {
            if value.starts_with('/') {
                components.clear();
                value.clone()
            } else {
                for segment in cwd.split('/') {
                    push_segment(&mut components, segment);
                }
                value.clone()
            }
        }
        Platform::Win32 => {
            if value.starts_with("\\\\") {
                // UNC path: keep verbatim (the JS resolver normalizes `.`/`..`
                // only; the tunnels module never receives UNC inputs).
                return value;
            } else if is_win_drive_prefix(&value) {
                prefix = value[..2].to_string();
                value[2..].to_string()
            } else if value.starts_with('\\') {
                // Rooted on the current drive.
                let drive = if is_win_drive_prefix(&cwd) {
                    cwd[..2].to_string()
                } else {
                    "C:".to_string()
                };
                prefix = drive;
                value
            } else {
                if is_win_drive_prefix(&cwd) {
                    prefix = cwd[..2].to_string();
                    for segment in cwd[2..].split('\\') {
                        push_segment(&mut components, segment);
                    }
                } else {
                    for segment in cwd.split('\\') {
                        push_segment(&mut components, segment);
                    }
                }
                value
            }
        }
    };

    let segment_sep = match platform {
        Platform::Posix => '/',
        Platform::Win32 => '\\',
    };
    if posix_absolute {
        // Absolute posix path: resolve from the root.
        components.clear();
    }
    for segment in rest.split(segment_sep) {
        push_segment(&mut components, segment);
    }

    let joined = components.join(&sep.to_string());
    if joined.is_empty() {
        if platform == Platform::Posix {
            return "/".to_string();
        }
        return format!("{prefix}{sep}");
    }
    format!("{prefix}{sep}{joined}")
}

/// 路径组件折叠规则：空串与 `.` 跳过；`..` 弹出上一级（根处再弹为空
/// 操作）；其余压入组件栈。
fn push_segment(components: &mut Vec<String>, segment: &str) {
    match segment {
        "" | "." => {}
        ".." => {
            components.pop();
        }
        other => components.push(other.to_string()),
    }
}

/// `isPathWithinDirectory(candidatePath, directoryPath, platform)`.
/// （中文）判断 candidate 解析后是否落在 directory 内（与目录本身相等
/// 也算在内）。Win32 先转小写再比较以模拟大小写不敏感；前缀匹配要求
/// 目录以分隔符结尾，避免同级前缀目录（如 home 与 home2）误判。
pub fn is_path_within_directory(
    candidate: &str,
    directory: &str,
    cwd: &str,
    platform: Platform,
) -> bool {
    let resolved_candidate = platform_resolve(candidate, cwd, platform);
    let resolved_directory = platform_resolve(directory, cwd, platform);
    let comparable = |value: String| -> String {
        if platform == Platform::Win32 {
            value.to_lowercase()
        } else {
            value
        }
    };
    let comparable_candidate = comparable(resolved_candidate);
    let comparable_directory = comparable(resolved_directory);
    if comparable_candidate == comparable_directory {
        return true;
    }
    let sep = platform.sep();
    let directory_prefix = if comparable_directory.ends_with(sep) {
        comparable_directory
    } else {
        format!("{comparable_directory}{sep}")
    };
    comparable_candidate.starts_with(&directory_prefix)
}

/// `resolveTunnelConfigPath(value, home, platform)`: expands `~`, resolves,
/// and confines the result to `home`.
/// （中文）解析用户提供的 tunnel 配置路径：`~` 展开为 home（兼容正反
/// 斜杠两种写法），其余按 cwd 解析为绝对路径；结果必须位于 home 目录
/// 内，否则返回 validation_error（防止越权读写任意文件）。
pub fn resolve_tunnel_config_path(
    value: &str,
    home: &str,
    cwd: &str,
    platform: Platform,
) -> Result<String, TunnelServiceError> {
    let resolved = if value == "~" {
        home.to_string()
    } else if let Some(rest) = value.strip_prefix("~/") {
        platform_join(home, rest, platform)
    } else if let Some(rest) = value
        .strip_prefix("~\\")
        .map(|rest| normalize_separators(rest, platform))
    {
        platform_join(home, &rest, platform)
    } else {
        platform_resolve(value, cwd, platform)
    };

    if !is_path_within_directory(&resolved, home, cwd, platform) {
        return Err(TunnelServiceError::validation_error(format!(
            "Config path must be within the home directory ({home}). Got: {resolved}"
        )));
    }
    Ok(resolved)
}

/// `normalizeTunnelProvider(value)`.
/// （中文）归一化 provider：trim 加小写；缺失、空白或不支持的值一律
/// 回退 cloudflare（与 JS 默认值一致）。
pub fn normalize_tunnel_provider(value: Option<&str>) -> String {
    let Some(value) = value else {
        return TUNNEL_PROVIDER_CLOUDFLARE.to_string();
    };
    let provider = value.trim().to_lowercase();
    if provider.is_empty() || !SUPPORTED_TUNNEL_PROVIDERS.contains(&provider.as_str()) {
        return TUNNEL_PROVIDER_CLOUDFLARE.to_string();
    }
    provider
}

/// `normalizeTunnelMode` / `normalizeTunnelModeForRequest` (identical effects).
/// （中文）归一化 mode：trim 加小写；缺失或非法值回退 quick。
pub fn normalize_tunnel_mode(value: Option<&str>) -> String {
    let Some(value) = value else {
        return TUNNEL_MODE_QUICK.to_string();
    };
    let mode = value.trim().to_lowercase();
    if SUPPORTED_TUNNEL_MODES.contains(&mode.as_str()) {
        mode
    } else {
        TUNNEL_MODE_QUICK.to_string()
    }
}

/// `normalizeTunnelIntent(value)`: `None` for absent/unsupported.
/// （中文）归一化 intent：缺失、空白或不支持的值返回 None（调用方再按
/// mode 回退推导），仅白名单内的值原样返回。
pub fn normalize_tunnel_intent(value: Option<&str>) -> Option<String> {
    let value = value?;
    let intent = value.trim().to_lowercase();
    if intent.is_empty() || !SUPPORTED_TUNNEL_INTENTS.contains(&intent.as_str()) {
        return None;
    }
    Some(intent)
}

/// `modeIntentFallback(mode)`.
/// （中文）按 mode 推导默认 intent：quick 对应 ephemeral-public，两种
/// managed 模式对应 persistent-public，未知 mode 返回 None。
pub fn mode_intent_fallback(mode: &str) -> Option<&'static str> {
    match mode {
        TUNNEL_MODE_QUICK => Some(TUNNEL_INTENT_EPHEMERAL_PUBLIC),
        TUNNEL_MODE_MANAGED_REMOTE | TUNNEL_MODE_MANAGED_LOCAL => {
            Some(TUNNEL_INTENT_PERSISTENT_PUBLIC)
        }
        _ => None,
    }
}

/// mode 是否在全局支持列表内（仅校验，不做归一化回退）。
pub fn is_supported_tunnel_mode(mode: &str) -> bool {
    SUPPORTED_TUNNEL_MODES.contains(&mode)
}

/// intent 是否在全局支持列表内（仅校验，不做归一化回退）。
pub fn is_supported_tunnel_intent(intent: &str) -> bool {
    SUPPORTED_TUNNEL_INTENTS.contains(&intent)
}

/// `normalizeOptionalPath(value)`: `None` covers both JS `null` (empty input)
/// and `undefined` (non-string input); resolution errors propagate.
/// （中文）归一化可选路径：None（JS 的 undefined）与空白串（JS 的空串
/// 或 null）都返回 None，其余交给 resolve_tunnel_config_path 解析并
/// 限制在 home 内；解析错误原样上抛。
pub fn normalize_optional_path(
    value: Option<&str>,
    home: &str,
    cwd: &str,
    platform: Platform,
) -> Result<Option<String>, TunnelServiceError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    resolve_tunnel_config_path(trimmed, home, cwd, platform).map(Some)
}

/// Normalized tunnel start request (`normalizeTunnelStartRequest`).
/// （中文）归一化完成的 tunnel 启动请求：所有字段已 trim、小写、路径
/// 展开；intent 为 None 表示用户未显式指定，config_path 为 None 表示
/// 未提供配置文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelStartRequest {
    /// tunnel provider 标识（cloudflare/ngrok，已归一化）。
    pub provider: String,
    /// tunnel 模式（quick/managed-remote/managed-local，已归一化）。
    pub mode: String,
    /// 用户显式指定的意图；缺省时由 mode_intent_fallback 推导。
    pub intent: Option<String>,
    /// 可选的 cloudflared 配置文件绝对路径（已限制在 home 内）。
    pub config_path: Option<String>,
    /// managed-remote 所需的 tunnel token（已 trim，未提供为空串）。
    pub token: String,
    /// 期望的公开 hostname（已小写，未提供为空串）。
    pub hostname: String,
}

/// JS `input.key ?? defaults.key`: falls through on `null`/absent.
/// （中文）复刻 JS 的空值合并语义：input 上该键存在且非 null 才优先，
/// 否则回落 defaults 上同名且非 null 的值。
fn coalesce<'a>(input: &'a Value, defaults: &'a Value, key: &str) -> Option<&'a Value> {
    input
        .get(key)
        .filter(|value| !value.is_null())
        .or_else(|| defaults.get(key).filter(|value| !value.is_null()))
}

/// 归一化 tunnel 启动请求：逐字段按空值合并语义合并 input 与 defaults；
/// provider/mode/intent 走各自的归一化函数，intent 缺省按 mode 回退；
/// configPath 用 hasOwnProperty 语义，input 上显式存在（哪怕为 null）
/// 就不再读 defaults；token 与 hostname 做 trim（hostname 另转小写），
/// 缺省为空串。路径解析失败原样上抛。
pub fn normalize_tunnel_start_request(
    input: &Value,
    defaults: &Value,
    home: &str,
    cwd: &str,
    platform: Platform,
) -> Result<TunnelStartRequest, TunnelServiceError> {
    let provider =
        normalize_tunnel_provider(coalesce(input, defaults, "provider").and_then(Value::as_str));
    let mode = normalize_tunnel_mode(coalesce(input, defaults, "mode").and_then(Value::as_str));
    let explicit_intent =
        normalize_tunnel_intent(coalesce(input, defaults, "intent").and_then(Value::as_str));
    let intent = explicit_intent.or_else(|| mode_intent_fallback(&mode).map(str::to_string));

    // `hasOwnProperty.call(input, 'configPath') ? input.configPath : defaults.configPath`
    let config_path_value = if input.get("configPath").is_some() {
        input.get("configPath")
    } else {
        defaults.get("configPath")
    };
    let config_path = normalize_optional_path(
        config_path_value.and_then(Value::as_str),
        home,
        cwd,
        platform,
    )?;

    let token = coalesce(input, defaults, "token")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_string())
        .unwrap_or_default();

    let hostname = coalesce(input, defaults, "hostname")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_lowercase())
        .unwrap_or_default();

    Ok(TunnelStartRequest {
        provider,
        mode,
        intent,
        config_path,
        token,
        hostname,
    })
}

/// A provider mode descriptor (`capabilities.modes[]`).
/// （中文）provider 能力表中的单个 mode 描述：key 与 intent 参与启动
/// 校验，requires 列出该 mode 必填的请求字段，supports 与 stability
/// 仅作为给前端的展示元数据。
#[derive(Debug, Clone)]
pub struct ModeDescriptor {
    /// mode 标识（与 TUNNEL_MODE 系列常量取值一致）。
    pub key: &'static str,
    /// 面向用户的展示名称。
    pub label: &'static str,
    /// 该 mode 绑定的 intent（请求显式给出的 intent 必须与之一致）。
    pub intent: &'static str,
    /// 必填请求字段名列表（token/hostname/configPath 的子集）。
    pub requires: &'static [&'static str],
    /// 该 mode 支持的可选项列表（展示用）。
    pub supports: &'static [&'static str],
    /// 稳定性标签（ga/beta，展示用）。
    pub stability: &'static str,
}

/// 序列化为 capabilities 响应中的 mode 对象。
impl ModeDescriptor {
    /// 输出驼峰键名与 JS capabilities 响应格式一致的 JSON 对象。
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "key": self.key,
            "label": self.label,
            "intent": self.intent,
            "requires": self.requires,
            "supports": self.supports,
            "stability": self.stability,
        })
    }
}

/// `validateTunnelStartRequest(request, capabilities)`.
/// （中文）按 provider 能力表校验启动请求，失败路径依次为：provider 为
/// 空 → validation_error；mode 不在全局白名单 → mode_unsupported；与
/// 能力表 provider 不符 → provider_unsupported；mode 不在该 provider
/// 能力表 → mode_unsupported；显式 intent 非法或与 mode 绑定 intent
/// 不符 → validation_error；requires 中 token/hostname/configPath 缺失
/// → validation_error。全部通过返回 Ok。
pub fn validate_tunnel_start_request(
    request: &TunnelStartRequest,
    capabilities_provider: &str,
    modes: &[ModeDescriptor],
) -> Result<(), TunnelServiceError> {
    if request.provider.is_empty() {
        return Err(TunnelServiceError::validation_error(
            "Tunnel provider is required",
        ));
    }

    if !is_supported_tunnel_mode(&request.mode) {
        return Err(TunnelServiceError::new(
            "mode_unsupported",
            format!("Unsupported tunnel mode: {}", request.mode),
        ));
    }

    if capabilities_provider != request.provider {
        return Err(TunnelServiceError::new(
            "provider_unsupported",
            format!("Unsupported tunnel provider: {}", request.provider),
        ));
    }

    let mode_descriptor = modes
        .iter()
        .find(|entry| entry.key == request.mode)
        .ok_or_else(|| {
            TunnelServiceError::new(
                "mode_unsupported",
                format!(
                    "Provider '{}' does not support mode '{}'",
                    request.provider, request.mode
                ),
            )
        })?;

    if let Some(intent) = request.intent.as_deref() {
        if !is_supported_tunnel_intent(intent) {
            return Err(TunnelServiceError::validation_error(format!(
                "Unsupported tunnel intent: {intent}"
            )));
        }
        if mode_descriptor.intent != intent {
            return Err(TunnelServiceError::validation_error(format!(
                "Tunnel intent '{}' does not match mode '{}' (expected '{}')",
                intent, request.mode, mode_descriptor.intent
            )));
        }
    }

    for field in mode_descriptor.requires {
        match *field {
            "token" if request.token.is_empty() => {
                return Err(TunnelServiceError::validation_error(
                    "Managed remote tunnel token is required",
                ));
            }
            "hostname" if request.hostname.is_empty() => {
                return Err(TunnelServiceError::validation_error(
                    "Managed remote tunnel hostname is required",
                ));
            }
            "configPath" if request.config_path.is_none() => {
                return Err(TunnelServiceError::validation_error(format!(
                    "Mode '{}' requires a configPath",
                    request.mode
                )));
            }
            _ => {}
        }
    }

    Ok(())
}

/// 路径归一化、目录包含判断、请求归一化与按能力表校验的纯逻辑测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// Windows 用例的 home 目录。
    const WIN_HOME: &str = "C:\\Users\\Bohdan";
    /// Windows 用例的 cwd（home 下的项目目录）。
    const WIN_CWD: &str = "C:\\Users\\Bohdan\\project";

    /// Win32 下盘符大小写不同的 home 路径仍视为目录内。
    #[test]
    fn allows_windows_home_paths_with_different_drive_casing() {
        assert!(is_path_within_directory(
            "c:\\Users\\Bohdan\\.cloudflared\\config.yml",
            WIN_HOME,
            WIN_CWD,
            Platform::Win32,
        ));
    }

    /// Win32 下同级前缀目录（Bohdan2 对 Bohdan）不算在 home 内。
    #[test]
    fn does_not_allow_windows_sibling_home_directories() {
        assert!(!is_path_within_directory(
            "C:\\Users\\Bohdan2\\.cloudflared\\config.yml",
            WIN_HOME,
            WIN_CWD,
            Platform::Win32,
        ));
    }

    /// Win32 下波浪线加反斜杠的路径展开为 home 内的绝对路径。
    #[test]
    fn resolves_windows_tilde_paths_inside_home() {
        assert_eq!(
            resolve_tunnel_config_path(
                "~\\.cloudflared\\config.yml",
                WIN_HOME,
                WIN_CWD,
                Platform::Win32
            )
            .expect("valid path"),
            "C:\\Users\\Bohdan\\.cloudflared\\config.yml"
        );
    }

    /// 越出 home 的绝对路径被拒绝，错误码与文案均含原始路径。
    #[test]
    fn rejects_windows_paths_outside_home() {
        let error =
            resolve_tunnel_config_path("C:\\Temp\\config.yml", WIN_HOME, WIN_CWD, Platform::Win32)
                .expect_err("outside home");
        assert_eq!(error.code, "validation_error");
        assert_eq!(
            error.message,
            "Config path must be within the home directory (C:\\Users\\Bohdan). Got: C:\\Temp\\config.yml"
        );
    }

    /// Posix 相对路径按 cwd 解析、点段折叠，绝对路径原样保留。
    #[test]
    fn resolves_posix_relative_paths_against_cwd() {
        assert_eq!(
            platform_resolve("cfg/config.yml", "/home/ada/app", Platform::Posix),
            "/home/ada/app/cfg/config.yml"
        );
        assert_eq!(
            platform_resolve("../shared/config.yml", "/home/ada/app", Platform::Posix),
            "/home/ada/shared/config.yml"
        );
        assert_eq!(
            platform_resolve("/etc/config.yml", "/home/ada", Platform::Posix),
            "/etc/config.yml"
        );
    }

    /// Posix 目录包含按分隔符前缀判断：目录本身算在内，同级前缀不算。
    #[test]
    fn posix_home_containment_uses_slash_prefix() {
        assert!(is_path_within_directory(
            "/home/ada/.cloudflared/config.yml",
            "/home/ada",
            "/tmp",
            Platform::Posix
        ));
        assert!(!is_path_within_directory(
            "/home/ada2/.cloudflared/config.yml",
            "/home/ada",
            "/tmp",
            Platform::Posix
        ));
        // The home directory itself counts as within (JS: candidate === directory).
        assert!(is_path_within_directory(
            "/home/ada",
            "/home/ada",
            "/tmp",
            Platform::Posix
        ));
    }

    /// provider/mode 归一化：trim 加小写，非法或缺失回退 cloudflare/quick。
    #[test]
    fn normalizes_providers_and_modes_with_defaults() {
        assert_eq!(normalize_tunnel_provider(None), TUNNEL_PROVIDER_CLOUDFLARE);
        assert_eq!(
            normalize_tunnel_provider(Some("  NGROK ")),
            TUNNEL_PROVIDER_NGROK
        );
        assert_eq!(
            normalize_tunnel_provider(Some("unknown")),
            TUNNEL_PROVIDER_CLOUDFLARE
        );
        assert_eq!(normalize_tunnel_mode(None), TUNNEL_MODE_QUICK);
        assert_eq!(normalize_tunnel_mode(Some("")), TUNNEL_MODE_QUICK);
        assert_eq!(
            normalize_tunnel_mode(Some("Managed-Remote")),
            TUNNEL_MODE_MANAGED_REMOTE
        );
        assert_eq!(normalize_tunnel_mode(Some("bogus")), TUNNEL_MODE_QUICK);
    }

    /// intent 归一化丢弃非法值；mode 回退把 quick 映射到 ephemeral、
    /// managed 映射到 persistent。
    #[test]
    fn normalizes_intents_and_falls_back_by_mode() {
        assert_eq!(
            normalize_tunnel_intent(Some("ephemeral-public")).as_deref(),
            Some("ephemeral-public")
        );
        assert_eq!(normalize_tunnel_intent(Some("nope")), None);
        assert_eq!(normalize_tunnel_intent(None), None);
        assert_eq!(
            mode_intent_fallback(TUNNEL_MODE_QUICK),
            Some(TUNNEL_INTENT_EPHEMERAL_PUBLIC)
        );
        assert_eq!(
            mode_intent_fallback(TUNNEL_MODE_MANAGED_REMOTE),
            Some(TUNNEL_INTENT_PERSISTENT_PUBLIC)
        );
    }

    /// input 上显式的 null configPath 覆盖 defaults 值（hasOwnProperty 语义）。
    #[test]
    fn null_config_path_on_input_beats_default() {
        let input = serde_json::json!({ "configPath": null });
        let defaults = serde_json::json!({ "configPath": "/tmp/other.yml" });
        let request =
            normalize_tunnel_start_request(&input, &defaults, "/home/ada", "/tmp", Platform::Posix)
                .expect("normalizes");
        // JS hasOwnProperty(input) is true and the value is null → normalizeOptionalPath(null) → null.
        assert_eq!(request.config_path, None);
    }

    /// input 缺失 configPath 时回退 defaults 并展开波浪线。
    #[test]
    fn absent_config_path_falls_back_to_default() {
        let input = serde_json::json!({});
        let defaults = serde_json::json!({ "configPath": "~/.cloudflared/config.yml" });
        let request =
            normalize_tunnel_start_request(&input, &defaults, "/home/ada", "/tmp", Platform::Posix)
                .expect("normalizes");
        assert_eq!(
            request.config_path.as_deref(),
            Some("/home/ada/.cloudflared/config.yml")
        );
    }

    /// 启动请求各字段完成 trim 与小写归一化。
    #[test]
    fn normalizes_request_fields() {
        let input = serde_json::json!({
            "provider": " ngrok ",
            "mode": "QUICK",
            "intent": "  EPHEMERAL-PUBLIC ",
            "token": "  tok  ",
            "hostname": " Example.COM "
        });
        let request = normalize_tunnel_start_request(
            &input,
            &serde_json::json!({}),
            "/home/ada",
            "/tmp",
            Platform::Posix,
        )
        .expect("normalizes");
        assert_eq!(request.provider, "ngrok");
        assert_eq!(request.mode, "quick");
        assert_eq!(request.intent.as_deref(), Some("ephemeral-public"));
        assert_eq!(request.token, "tok");
        assert_eq!(request.hostname, "example.com");
    }

    /// 按 mode 的 requires 逐项拦截缺失字段，并校验 intent 与 mode 匹配。
    #[test]
    fn validates_required_fields_per_mode() {
        let capabilities_provider = "cloudflare";
        let modes = vec![
            ModeDescriptor {
                key: "quick",
                label: "Quick Tunnel",
                intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC,
                requires: &[],
                supports: &["sessionTTL"],
                stability: "ga",
            },
            ModeDescriptor {
                key: "managed-remote",
                label: "Managed Remote Tunnel",
                intent: TUNNEL_INTENT_PERSISTENT_PUBLIC,
                requires: &["token", "hostname"],
                supports: &["customDomain", "sessionTTL"],
                stability: "ga",
            },
        ];

        let mut request = TunnelStartRequest {
            provider: "cloudflare".to_string(),
            mode: "managed-remote".to_string(),
            intent: Some("persistent-public".to_string()),
            config_path: None,
            token: String::new(),
            hostname: String::new(),
        };
        let error = validate_tunnel_start_request(&request, capabilities_provider, &modes)
            .expect_err("token required");
        assert_eq!(error.code, "validation_error");
        assert_eq!(error.message, "Managed remote tunnel token is required");

        request.token = "tok".to_string();
        let error = validate_tunnel_start_request(&request, capabilities_provider, &modes)
            .expect_err("hostname required");
        assert_eq!(error.message, "Managed remote tunnel hostname is required");

        request.hostname = "example.com".to_string();
        validate_tunnel_start_request(&request, capabilities_provider, &modes)
            .expect("valid managed-remote request");

        // Intent mismatch: a persistent intent against the quick mode.
        request.intent = Some("persistent-public".to_string());
        request.mode = "quick".to_string();
        let error = validate_tunnel_start_request(&request, capabilities_provider, &modes)
            .expect_err("intent mismatch");
        assert_eq!(
            error.message,
            "Tunnel intent 'persistent-public' does not match mode 'quick' (expected 'ephemeral-public')"
        );

        // Unsupported intent value.
        request.intent = Some("nope".to_string());
        let error = validate_tunnel_start_request(&request, capabilities_provider, &modes)
            .expect_err("unsupported intent");
        assert_eq!(error.message, "Unsupported tunnel intent: nope");
    }

    /// provider 与能力表不符、mode 不在能力表时返回对应错误码。
    #[test]
    fn rejects_wrong_provider_and_mode() {
        let modes = vec![ModeDescriptor {
            key: "quick",
            label: "Quick",
            intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC,
            requires: &[],
            supports: &[],
            stability: "ga",
        }];
        let request = TunnelStartRequest {
            provider: "ngrok".to_string(),
            mode: "quick".to_string(),
            intent: None,
            config_path: None,
            token: String::new(),
            hostname: String::new(),
        };
        let error =
            validate_tunnel_start_request(&request, "cloudflare", &modes).expect_err("provider");
        assert_eq!(error.code, "provider_unsupported");
        assert_eq!(error.message, "Unsupported tunnel provider: ngrok");

        let request = TunnelStartRequest {
            provider: "cloudflare".to_string(),
            mode: "managed-local".to_string(),
            intent: None,
            config_path: None,
            token: String::new(),
            hostname: String::new(),
        };
        let error = validate_tunnel_start_request(&request, "cloudflare", &modes)
            .expect_err("mode unsupported by provider");
        assert_eq!(error.code, "mode_unsupported");
        assert_eq!(
            error.message,
            "Provider 'cloudflare' does not support mode 'managed-local'"
        );
    }
}
