//! Port of `bin/lib/cli-network.js`: serve host/password resolution and the
//! authenticated-exposure assertion.

use super::{AUTH_CONFIG_ERROR, CliError};

/// `resolveServeHost`: option > OMPCHAMBER_HOST env > 127.0.0.1.
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
pub fn is_network_exposed_bind_host(host: &str) -> bool {
    !(host == "127.0.0.1"
        || host == "localhost"
        || host == "::1"
        || host == "[::1]"
        || host == "::"
        || host == "[::]")
}

pub fn generate_ui_password() -> String {
    let bytes: [u8; 24] = rand::random();
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, bytes)
}

pub struct ResolvedUiPassword {
    pub password: Option<String>,
    pub generated: bool,
}

/// `resolveServeUiPassword`: `--ui-password` (empty value ⇒ generate);
/// otherwise OMPCHAMBER_UI_PASSWORD / OPENCODE_UI_PASSWORD env; else none.
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
pub fn assert_safe_browser_port(port: u16, context: &str) -> Option<String> {
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
