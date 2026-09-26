//! Port of `server/lib/tunnels/types.js`: tunnel constants, normalization, and
//! shared validation. Platform-parameterized path helpers mirror the JS
//! `path`/`path.win32` switching driven by the `platform` argument.

use serde_json::Value;

pub const TUNNEL_PROVIDER_CLOUDFLARE: &str = "cloudflare";
pub const TUNNEL_PROVIDER_NGROK: &str = "ngrok";

pub const TUNNEL_MODE_QUICK: &str = "quick";
pub const TUNNEL_MODE_MANAGED_REMOTE: &str = "managed-remote";
pub const TUNNEL_MODE_MANAGED_LOCAL: &str = "managed-local";

pub const TUNNEL_INTENT_EPHEMERAL_PUBLIC: &str = "ephemeral-public";
pub const TUNNEL_INTENT_PERSISTENT_PUBLIC: &str = "persistent-public";
pub const TUNNEL_INTENT_PRIVATE_NETWORK: &str = "private-network";

const SUPPORTED_TUNNEL_INTENTS: [&str; 3] = [
    TUNNEL_INTENT_EPHEMERAL_PUBLIC,
    TUNNEL_INTENT_PERSISTENT_PUBLIC,
    TUNNEL_INTENT_PRIVATE_NETWORK,
];

const SUPPORTED_TUNNEL_MODES: [&str; 3] = [
    TUNNEL_MODE_QUICK,
    TUNNEL_MODE_MANAGED_REMOTE,
    TUNNEL_MODE_MANAGED_LOCAL,
];

const SUPPORTED_TUNNEL_PROVIDERS: [&str; 2] = [TUNNEL_PROVIDER_CLOUDFLARE, TUNNEL_PROVIDER_NGROK];

/// `TunnelServiceError` (`code` + `message`; `details` is always null in the JS
/// source and never crosses the wire).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct TunnelServiceError {
    pub code: String,
    pub message: String,
}

impl TunnelServiceError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn validation_error(message: impl Into<String>) -> Self {
        Self::new("validation_error", message)
    }
}

/// The platforms `types.js` switches on (`path.win32` vs `path`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Win32,
    Posix,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(windows) {
            Platform::Win32
        } else {
            Platform::Posix
        }
    }

    /// JS `process.platform` string for responses.
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

    fn sep(self) -> char {
        match self {
            Platform::Win32 => '\\',
            Platform::Posix => '/',
        }
    }
}

fn is_win_drive_prefix(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

fn normalize_separators(value: &str, platform: Platform) -> String {
    match platform {
        Platform::Win32 => value.replace('/', "\\"),
        Platform::Posix => value.to_string(),
    }
}

/// `pathApi.join(base, rest)`: normalize separators, then join.
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
pub fn normalize_tunnel_intent(value: Option<&str>) -> Option<String> {
    let value = value?;
    let intent = value.trim().to_lowercase();
    if intent.is_empty() || !SUPPORTED_TUNNEL_INTENTS.contains(&intent.as_str()) {
        return None;
    }
    Some(intent)
}

/// `modeIntentFallback(mode)`.
pub fn mode_intent_fallback(mode: &str) -> Option<&'static str> {
    match mode {
        TUNNEL_MODE_QUICK => Some(TUNNEL_INTENT_EPHEMERAL_PUBLIC),
        TUNNEL_MODE_MANAGED_REMOTE | TUNNEL_MODE_MANAGED_LOCAL => {
            Some(TUNNEL_INTENT_PERSISTENT_PUBLIC)
        }
        _ => None,
    }
}

pub fn is_supported_tunnel_mode(mode: &str) -> bool {
    SUPPORTED_TUNNEL_MODES.contains(&mode)
}

pub fn is_supported_tunnel_intent(intent: &str) -> bool {
    SUPPORTED_TUNNEL_INTENTS.contains(&intent)
}

/// `normalizeOptionalPath(value)`: `None` covers both JS `null` (empty input)
/// and `undefined` (non-string input); resolution errors propagate.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelStartRequest {
    pub provider: String,
    pub mode: String,
    pub intent: Option<String>,
    pub config_path: Option<String>,
    pub token: String,
    pub hostname: String,
}

/// JS `input.key ?? defaults.key`: falls through on `null`/absent.
fn coalesce<'a>(input: &'a Value, defaults: &'a Value, key: &str) -> Option<&'a Value> {
    input
        .get(key)
        .filter(|value| !value.is_null())
        .or_else(|| defaults.get(key).filter(|value| !value.is_null()))
}

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
#[derive(Debug, Clone)]
pub struct ModeDescriptor {
    pub key: &'static str,
    pub label: &'static str,
    pub intent: &'static str,
    pub requires: &'static [&'static str],
    pub supports: &'static [&'static str],
    pub stability: &'static str,
}

impl ModeDescriptor {
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

#[cfg(test)]
mod tests {
    use super::*;

    const WIN_HOME: &str = "C:\\Users\\Bohdan";
    const WIN_CWD: &str = "C:\\Users\\Bohdan\\project";

    #[test]
    fn allows_windows_home_paths_with_different_drive_casing() {
        assert!(is_path_within_directory(
            "c:\\Users\\Bohdan\\.cloudflared\\config.yml",
            WIN_HOME,
            WIN_CWD,
            Platform::Win32,
        ));
    }

    #[test]
    fn does_not_allow_windows_sibling_home_directories() {
        assert!(!is_path_within_directory(
            "C:\\Users\\Bohdan2\\.cloudflared\\config.yml",
            WIN_HOME,
            WIN_CWD,
            Platform::Win32,
        ));
    }

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
