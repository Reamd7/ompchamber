//! Port of `bin/lib/cli-network.js`: serve host/password resolution and the
//! authenticated-exposure assertion.
//!
//! 中文说明：移植自 `bin/lib/cli-network.js`：serve 监听地址解析、
//! UI 密码（显式参数 / 环境变量 / 随机生成）解析，以及"绑定网络暴露
//! 地址必须带密码"的安全断言与浏览器不安全端口提醒。

use super::{AUTH_CONFIG_ERROR, CliError};

/// `resolveServeHost`: option > OMPCHAMBER_HOST env > 127.0.0.1.
/// 中文：解析优先级为显式参数 > OMPCHAMBER_HOST 环境变量 > 127.0.0.1；
/// 参数与环境变量的空白值都视为未设置。
pub fn resolve_serve_host(host: Option<&str>) -> String {
    if let Some(host) = host.map(str::trim).filter(|h| !h.is_empty()) {
        return host.to_string();
    }
    std::env::var("OMPCHAMBER_HOST")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// `isNetworkExposedBindHost` (server/lib/security/bind-host.js subset).
/// 中文：是否绑定到对外暴露的地址——除 127.0.0.1/localhost/::1/[::1]/
/// ::/[::] 之外都算网络暴露（server/lib/security/bind-host.js 的子集）。
pub fn is_network_exposed_bind_host(host: &str) -> bool {
    !(host == "127.0.0.1"
        || host == "localhost"
        || host == "::1"
        || host == "[::1]"
        || host == "::"
        || host == "[::]")
}

/// 生成 24 字节随机数的 base64url（无填充）UI 密码，固定 32 字符。
pub fn generate_ui_password() -> String {
    let bytes: [u8; 24] = rand::random();
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

/// resolveServeUiPassword 的结果：密码值 + 是否为随机生成。
pub struct ResolvedUiPassword {
    /// 解析出的 UI 密码；None 表示本次不启用密码。
    pub password: Option<String>,
    /// 密码是否由服务端随机生成（生成值需要回显给用户）。
    pub generated: bool,
}

/// `resolveServeUiPassword`: `--ui-password` (empty value ⇒ generate);
/// otherwise OMPCHAMBER_UI_PASSWORD / OPENCODE_UI_PASSWORD env; else none.
/// 中文：--ui-password 显式给出则用之（空串视为"帮我生成一个"）；
/// 否则依次查 OMPCHAMBER_UI_PASSWORD / OPENCODE_UI_PASSWORD；都无则 None。
pub fn resolve_serve_ui_password(options: &super::args::Options) -> ResolvedUiPassword {
    if options.explicit_ui_password {
        if let Some(password) = options
            .ui_password
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        {
            return ResolvedUiPassword {
                password: Some(password.to_string()),
                generated: false,
            };
        }
        return ResolvedUiPassword {
            password: Some(generate_ui_password()),
            generated: true,
        };
    }
    let from_env = std::env::var("OMPCHAMBER_UI_PASSWORD")
        .or_else(|_| std::env::var("OPENCODE_UI_PASSWORD"))
        .ok()
        .filter(|v| !v.is_empty());
    ResolvedUiPassword {
        password: from_env,
        generated: false,
    }
}

/// `assertAuthenticatedNetworkExposure`: a network-exposed bind without a UI
/// password is a configuration error.
/// 中文：网络暴露绑定却未设密码时拒绝启动（AUTH_CONFIG_ERROR），
/// 错误信息提示 --ui-password 或 OMPCHAMBER_UI_PASSWORD。
pub fn assert_authenticated_network_exposure(
    host: &str,
    ui_password: Option<&str>,
) -> Result<(), CliError> {
    if is_network_exposed_bind_host(host)
        && ui_password
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .is_none()
    {
        return Err(CliError::new(
            "Refusing to start: server is bound to a network-exposed address without a UI password. Pass --ui-password or set OMPCHAMBER_UI_PASSWORD.",
            AUTH_CONFIG_ERROR,
        ));
    }
    Ok(())
}

/// `assertSafeBrowserPort`: warn-level notice for unsafe ports (returned, the
/// caller prints per output mode).
/// 中文：端口命中浏览器"不安全端口"黑名单（含 macOS AirPlay 占用的
/// 5000-5010）时返回警告文案，由调用方按输出模式打印；安全端口返回 None。
pub fn assert_safe_browser_port(port: u16, context: &str) -> Option<String> {
    // 少量知名的浏览器封锁端口（ftp/discard 等）；69、137-139 与
    // 5000-5010 在下方条件里单独判断。
    const UNSAFE: [u16; 7] = [1, 7, 9, 11, 13, 15, 17];
    if UNSAFE.contains(&port)
        || (5000..=5010).contains(&port)
        || port == 69
        || port == 137
        || port == 138
        || port == 139
    {
        return Some(format!(
            "Warning: {context} is using port {port}, which may be reserved or proxied by the system."
        ));
    }
    None
}
