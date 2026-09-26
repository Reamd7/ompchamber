//! Port of `bin/lib/commands-tunnel.js` (plus its pure-CLI helper modules
//! `cli-tunnel-profiles.js`, `cli-tunnel-utils.js`,
//! `cli-tunnel-capabilities.js`): the `tunnel` command group.
//!
//! Profile CRUD + legacy migration is local file state; providers/ready/
//! doctor/status/start/stop drive a running instance's HTTP tunnel API with
//! `cli-http.js` request semantics (UI-password session retry, desktop bearer
//! token). Interactive clack prompts do not exist in this port: `canPrompt`
//! is always false, so the CLI takes the same paths the JS takes when
//! stdin/stdout are not TTYs (flag-driven, hard errors for missing input).
//!
//! Known divergence: `--qr` cannot render a QR code (the `qrcode-terminal`
//! crate is unavailable); the URL is printed and a warning goes to stderr.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use super::args::{DEFAULT_PORT, Options, Parsed};
use super::process;
use super::{CliError, GENERAL_ERROR, OutputMode, print_json};

const TUNNEL_PROFILES_VERSION: u64 = 1;
const MAX_TOKEN_FILE_BYTES: u64 = 8 * 1024;

const TUNNEL_BOOTSTRAP_TTL_MIN_MS: f64 = 60_000.0;
const TUNNEL_BOOTSTRAP_TTL_MAX_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
const TUNNEL_SESSION_TTL_MIN_MS: f64 = 5.0 * 60.0 * 1000.0;
const TUNNEL_SESSION_TTL_MAX_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

pub fn help_text() -> &'static str {
    include_str!("help/tunnel.txt")
}

/// `cli.js` wires tunnel commands with a plain (non-async) call; HTTP work is
/// driven on the ambient tokio runtime.
fn block_on<F: Future>(future: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// `crypto.randomUUID()` (v4 shape) from the `rand` crate.
fn random_uuid_v4() -> String {
    let bytes: [u8; 16] = rand::random();
    let mut hex = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            hex.push('-');
        }
        let value = match i {
            6 => (byte & 0x0f) | 0x40,
            8 => (byte & 0x3f) | 0x80,
            _ => *byte,
        };
        hex.push_str(&format!("{value:02x}"));
    }
    hex
}

// ---------------------------------------------------------------------------
// cli-tunnel-capabilities.js
// ---------------------------------------------------------------------------

/// `DEFAULT_TUNNEL_PROVIDER_CAPABILITIES` (cloudflare + ngrok).
fn default_tunnel_provider_capabilities() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "provider": "cloudflare",
            "defaults": { "mode": "quick", "optionDefaults": {} },
            "modes": [
                {
                    "key": "quick",
                    "label": "Quick Tunnel",
                    "intent": "ephemeral-public",
                    "requires": [],
                    "supports": ["sessionTTL"],
                    "stability": "ga",
                },
                {
                    "key": "managed-remote",
                    "label": "Managed Remote Tunnel",
                    "intent": "persistent-public",
                    "requires": ["token", "hostname"],
                    "supports": ["customDomain", "sessionTTL"],
                    "stability": "ga",
                },
                {
                    "key": "managed-local",
                    "label": "Managed Local Tunnel",
                    "intent": "persistent-public",
                    "requires": [],
                    "supports": ["configFile", "customDomain", "sessionTTL"],
                    "stability": "ga",
                },
            ],
        }),
        serde_json::json!({
            "provider": "ngrok",
            "defaults": { "mode": "quick", "optionDefaults": {} },
            "modes": [
                {
                    "key": "quick",
                    "label": "Quick Tunnel",
                    "intent": "ephemeral-public",
                    "requires": [],
                    "supports": ["sessionTTL"],
                    "stability": "beta",
                },
            ],
        }),
    ]
}

// ---------------------------------------------------------------------------
// cli-tunnel-profiles.js
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelProfile {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub mode: String,
    pub hostname: String,
    pub token: String,
    pub created_at: u64,
    pub updated_at: u64,
}

fn normalize_profile_provider(value: Option<&str>) -> String {
    value.unwrap_or_default().trim().to_lowercase()
}

fn normalize_profile_mode(value: Option<&str>) -> String {
    value.unwrap_or_default().trim().to_lowercase()
}

fn normalize_profile_name(value: Option<&str>) -> String {
    value.unwrap_or_default().trim().to_string()
}

fn normalize_profile_hostname(value: Option<&str>) -> String {
    value.unwrap_or_default().trim().to_string()
}

fn normalize_profile_token(value: Option<&str>) -> String {
    value.unwrap_or_default().trim().to_string()
}

#[cfg_attr(not(test), allow(dead_code))]
fn suggest_profile_name_from_hostname(hostname: Option<&str>) -> String {
    let host = normalize_profile_hostname(hostname);
    if host.is_empty() {
        return "prod-main".to_string();
    }
    let first_label = host.split('.').next().unwrap_or(&host);
    let mut sanitized = String::new();
    let mut in_run = false;
    for ch in first_label.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            sanitized.push(ch);
            in_run = false;
        } else if !in_run {
            sanitized.push('-');
            in_run = true;
        }
    }
    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "prod-main".to_string()
    } else {
        trimmed.to_string()
    }
}

fn mask_token(token: &str) -> String {
    if token.is_empty() {
        return "***".to_string();
    }
    let len = token.chars().count();
    if len <= 4 {
        return "*".repeat(len);
    }
    let tail: String = token.chars().skip(len - 4).collect();
    format!("{}{}", "*".repeat(4.max(len - 4)), tail)
}

fn read_token_from_file_safely(token_file_path: &str) -> Result<String, CliError> {
    let absolute = absolute_path(token_file_path);
    let abs = absolute.display().to_string();
    let real = match std::fs::canonicalize(&absolute) {
        Ok(real) => real,
        Err(error) => match error.kind() {
            std::io::ErrorKind::NotFound => {
                return Err(CliError::new(
                    format!("Token file '{abs}' not found."),
                    GENERAL_ERROR,
                ));
            }
            std::io::ErrorKind::PermissionDenied => {
                return Err(CliError::new(
                    format!("Token file '{abs}' is not readable. Check file permissions."),
                    GENERAL_ERROR,
                ));
            }
            _ => return Err(CliError::new(format!("{abs}: {error}"), GENERAL_ERROR)),
        },
    };

    let metadata = match std::fs::metadata(&real) {
        Ok(metadata) => metadata,
        Err(error) => {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return Err(CliError::new(
                    format!("Token file '{abs}' is not readable. Check file permissions."),
                    GENERAL_ERROR,
                ));
            }
            return Err(CliError::new(format!("{abs}: {error}"), GENERAL_ERROR));
        }
    };
    if !metadata.is_file() {
        return Err(CliError::new(
            format!("Token file '{abs}' must be a regular file."),
            GENERAL_ERROR,
        ));
    }
    if metadata.len() == 0 {
        return Err(CliError::new(
            format!("Token file '{abs}' is empty."),
            GENERAL_ERROR,
        ));
    }
    if metadata.len() > MAX_TOKEN_FILE_BYTES {
        return Err(CliError::new(
            format!("Token file '{abs}' is too large (max {MAX_TOKEN_FILE_BYTES} bytes)."),
            GENERAL_ERROR,
        ));
    }

    let raw = std::fs::read_to_string(&real)
        .map_err(|error| CliError::new(format!("{abs}: {error}"), GENERAL_ERROR))?;
    if raw.contains('\0') {
        return Err(CliError::new(
            format!("Token file '{abs}' appears to be binary. Use a plain text token file."),
            GENERAL_ERROR,
        ));
    }
    let value = raw.trim();
    if value.is_empty() {
        return Err(CliError::new(
            format!("Token file '{abs}' is empty."),
            GENERAL_ERROR,
        ));
    }
    Ok(value.to_string())
}

fn absolute_path(value: &str) -> PathBuf {
    std::path::absolute(value).unwrap_or_else(|_| PathBuf::from(value))
}

/// `resolveToken(options)`: single source among --token-stdin/--token-file/
/// --token, resolved and trimmed. `Ok(None)` when no source given.
fn resolve_token(options: &Options) -> Result<Option<String>, CliError> {
    let mut sources: Vec<&str> = Vec::new();
    if options.token_stdin {
        sources.push("stdin");
    }
    if options.token_file.as_deref().is_some_and(|v| !v.is_empty()) {
        sources.push("file");
    }
    if options.token.as_deref().is_some_and(|v| !v.is_empty()) {
        sources.push("flag");
    }
    if sources.len() > 1 {
        return Err(CliError::new(
            format!(
                "Multiple token sources specified ({}). Use only one of --token, --token-file, or --token-stdin.",
                sources.join(", ")
            ),
            GENERAL_ERROR,
        ));
    }

    if options.token_stdin {
        let mut buffer = vec![0u8; 65536];
        let read = std::io::stdin()
            .lock()
            .read(&mut buffer)
            .map_err(|error| CliError::new(format!("{error}"), GENERAL_ERROR))?;
        let value = String::from_utf8_lossy(&buffer[..read]).trim().to_string();
        if value.is_empty() {
            return Err(CliError::new(
                "No token received from stdin.",
                GENERAL_ERROR,
            ));
        }
        return Ok(Some(value));
    }

    if let Some(token_file) = options.token_file.as_deref().filter(|v| !v.is_empty()) {
        return read_token_from_file_safely(token_file).map(Some);
    }

    Ok(options
        .token
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from))
}

fn redact_profile_for_output(profile: &TunnelProfile, show_secrets: bool) -> serde_json::Value {
    let token = if show_secrets {
        profile.token.clone()
    } else {
        mask_token(&profile.token)
    };
    serde_json::json!({
        "id": profile.id,
        "name": profile.name,
        "provider": profile.provider,
        "mode": profile.mode,
        "hostname": profile.hostname,
        "token": token,
        "createdAt": profile.created_at,
        "updatedAt": profile.updated_at,
    })
}

fn redact_profiles_for_output(profiles: &[TunnelProfile], show_secrets: bool) -> serde_json::Value {
    serde_json::Value::Array(
        profiles
            .iter()
            .map(|p| redact_profile_for_output(p, show_secrets))
            .collect(),
    )
}

fn format_profile_token_status(token: &str, show_secrets: bool) -> String {
    let token = token.trim();
    if token.is_empty() {
        return "token:missing".to_string();
    }
    if show_secrets {
        return format!("token:{token}");
    }
    "token:present".to_string()
}

fn finite_ms(value: Option<&serde_json::Value>) -> Option<u64> {
    value
        .and_then(serde_json::Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n as u64)
}

/// `sanitizeTunnelProfilesData`: drop incomplete entries, dedupe by
/// `provider::name(lower)`, default ids/timestamps.
fn sanitize_profiles(data: &serde_json::Value) -> Vec<TunnelProfile> {
    let Some(list) = data.get("profiles").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut profiles = Vec::new();
    for entry in list {
        if !entry.is_object() {
            continue;
        }
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .unwrap_or_else(random_uuid_v4);
        let provider =
            normalize_profile_provider(entry.get("provider").and_then(serde_json::Value::as_str));
        let mode = normalize_profile_mode(entry.get("mode").and_then(serde_json::Value::as_str));
        let name = normalize_profile_name(entry.get("name").and_then(serde_json::Value::as_str));
        let hostname =
            normalize_profile_hostname(entry.get("hostname").and_then(serde_json::Value::as_str));
        let token = normalize_profile_token(entry.get("token").and_then(serde_json::Value::as_str));
        if provider.is_empty()
            || mode.is_empty()
            || name.is_empty()
            || hostname.is_empty()
            || token.is_empty()
        {
            continue;
        }
        let key = format!("{provider}::{}", name.to_lowercase());
        if !seen.insert(key) {
            continue;
        }
        profiles.push(TunnelProfile {
            id,
            name,
            provider,
            mode,
            hostname,
            token,
            created_at: finite_ms(entry.get("createdAt")).unwrap_or_else(now_ms),
            updated_at: finite_ms(entry.get("updatedAt")).unwrap_or_else(now_ms),
        });
    }
    profiles
}

/// `warnIfUnsafeFilePermissions` (stderr notice, non-fatal).
fn warn_if_unsafe_file_permissions(file_path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(file_path) {
            let perms = metadata.permissions().mode() & 0o777;
            if perms & 0o077 != 0 {
                let octal = format!("{perms:>3o}");
                let display = file_path.display();
                eprintln!(
                    "Warning: Profile file '{display}' has permissions {octal} (should be 600). \
                     Other users may be able to read tunnel tokens. Fix with: chmod 600 '{display}'"
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file_path;
    }
}

fn read_tunnel_profiles_from_disk() -> Vec<TunnelProfile> {
    let file_path = super::paths::tunnel_profiles_file_path();
    warn_if_unsafe_file_permissions(&file_path);
    match std::fs::read_to_string(&file_path) {
        Ok(raw) => match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(value) => sanitize_profiles(&value),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

fn write_json_private(path: &Path, value: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(json) = serde_json::to_string_pretty(value) else {
        return;
    };
    if let Err(error) = std::fs::write(path, json) {
        eprintln!("Warning: Could not write '{}': {error}", path.display());
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

fn write_tunnel_profiles_to_disk(profiles: &[TunnelProfile]) -> Vec<TunnelProfile> {
    let sanitized = sanitize_profiles(&serde_json::json!({
        "version": TUNNEL_PROFILES_VERSION,
        "profiles": profiles,
    }));
    write_json_private(
        &super::paths::tunnel_profiles_file_path(),
        &serde_json::json!({
            "version": TUNNEL_PROFILES_VERSION,
            "profiles": sanitized,
        }),
    );
    sanitized
}

/// `writeManagedRemotePairsToDiskFromProfiles`: mirror cloudflare
/// managed-remote profiles into the legacy pairs file.
fn write_managed_remote_pairs_to_disk_from_profiles(profiles: &[TunnelProfile]) {
    let tunnels: Vec<serde_json::Value> = profiles
        .iter()
        .filter(|p| p.provider == "cloudflare" && p.mode == "managed-remote")
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "name": p.name,
                "hostname": p.hostname,
                "token": p.token,
                "updatedAt": p.updated_at,
            })
        })
        .collect();
    write_json_private(
        &super::paths::legacy_cloudflare_managed_remote_file_path(),
        &serde_json::json!({ "version": 1, "tunnels": tunnels }),
    );
}

fn read_legacy_managed_remote_entries() -> Vec<TunnelProfile> {
    let Ok(raw) =
        std::fs::read_to_string(super::paths::legacy_cloudflare_managed_remote_file_path())
    else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let Some(tunnels) = parsed.get("tunnels").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    tunnels
        .iter()
        .filter_map(|entry| {
            if !entry.is_object() {
                return None;
            }
            let id = entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
                .unwrap_or_else(random_uuid_v4);
            let name =
                normalize_profile_name(entry.get("name").and_then(serde_json::Value::as_str));
            let hostname = normalize_profile_hostname(
                entry.get("hostname").and_then(serde_json::Value::as_str),
            );
            let token =
                normalize_profile_token(entry.get("token").and_then(serde_json::Value::as_str));
            if name.is_empty() || hostname.is_empty() || token.is_empty() {
                return None;
            }
            let updated = finite_ms(entry.get("updatedAt")).unwrap_or_else(now_ms);
            Some(TunnelProfile {
                id,
                name,
                provider: "cloudflare".to_string(),
                mode: "managed-remote".to_string(),
                hostname,
                token,
                created_at: updated,
                updated_at: updated,
            })
        })
        .collect()
}

fn make_unique_profile_name(
    provider: &str,
    desired_name: &str,
    existing: &[TunnelProfile],
) -> String {
    let desired = normalize_profile_name(Some(desired_name));
    if desired.is_empty() {
        return String::new();
    }
    let existing_names: HashSet<String> = existing
        .iter()
        .filter(|p| p.provider == provider)
        .map(|p| p.name.to_lowercase())
        .collect();
    if !existing_names.contains(&desired.to_lowercase()) {
        return desired;
    }
    let mut index = 2;
    loop {
        let candidate = format!("{desired}-{index}");
        if !existing_names.contains(&candidate.to_lowercase()) {
            return candidate;
        }
        index += 1;
    }
}

/// `ensureTunnelProfilesMigrated`: read profiles, migrating the legacy
/// cloudflare pairs file when the new store is empty.
fn ensure_tunnel_profiles_migrated() -> Vec<TunnelProfile> {
    let current = read_tunnel_profiles_from_disk();
    if !current.is_empty() {
        return current;
    }
    let legacy = read_legacy_managed_remote_entries();
    if legacy.is_empty() {
        return current;
    }
    let mut migrated: Vec<TunnelProfile> = Vec::new();
    for entry in legacy {
        let name = make_unique_profile_name(&entry.provider, &entry.name, &migrated);
        migrated.push(TunnelProfile { name, ..entry });
    }
    let persisted = write_tunnel_profiles_to_disk(&migrated);
    write_managed_remote_pairs_to_disk_from_profiles(&persisted);
    persisted
}

fn resolve_profile_by_name<'a>(
    profiles: &'a [TunnelProfile],
    profile_name: &str,
    provider: Option<&str>,
) -> Result<&'a TunnelProfile, String> {
    let normalized_name = normalize_profile_name(Some(profile_name)).to_lowercase();
    let normalized_provider = normalize_profile_provider(provider);
    let matches: Vec<&TunnelProfile> = profiles
        .iter()
        .filter(|entry| {
            if entry.name.to_lowercase() != normalized_name {
                return false;
            }
            normalized_provider.is_empty() || entry.provider == normalized_provider
        })
        .collect();
    match matches.len() {
        0 => Err(format!(
            "No tunnel profile found for name '{profile_name}'. Run 'ompchamber tunnel profile list'."
        )),
        1 => Ok(matches[0]),
        _ => Err(format!(
            "Profile name '{profile_name}' exists for multiple providers. Use --provider <id>."
        )),
    }
}

// ---------------------------------------------------------------------------
// cli-tunnel-utils.js
// ---------------------------------------------------------------------------

/// `parseHumanDurationToMs`: `30m`, `2h`, `1d`, `1h30m`, plain ms.
fn parse_human_duration_to_ms(value: &str) -> Option<f64> {
    let trimmed = value.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().all(|c| c.is_ascii_digit()) {
        return trimmed.parse::<f64>().ok();
    }
    let normalized: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = normalized.as_bytes();
    let mut cursor = 0usize;
    let mut total = 0f64;
    while cursor < bytes.len() {
        let digits_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if digits_start == cursor {
            return None;
        }
        let amount: f64 = normalized[digits_start..cursor].parse().ok()?;
        let unit_start = cursor;
        while cursor < bytes.len() && !bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        let unit = &normalized[unit_start..cursor];
        let unit_ms = match unit {
            "ms" => 1.0,
            "s" => 1000.0,
            "m" => 60.0 * 1000.0,
            "h" => 60.0 * 60.0 * 1000.0,
            "d" => 24.0 * 60.0 * 60.0 * 1000.0,
            _ => return None,
        };
        total += amount * unit_ms;
    }
    Some(total)
}

fn parse_ttl_ms_or_throw(
    raw: &str,
    flag_name: &str,
    min_ms: f64,
    max_ms: f64,
) -> Result<f64, CliError> {
    let Some(parsed) = parse_human_duration_to_ms(raw) else {
        return Err(CliError::usage(format!(
            "Invalid value for {flag_name}. Use a positive duration like 30m, 24h, 1d, or milliseconds."
        )));
    };
    if !parsed.is_finite() || parsed <= 0.0 {
        return Err(CliError::usage(format!(
            "Invalid value for {flag_name}. Use a positive duration like 30m, 24h, 1d, or milliseconds."
        )));
    }
    if parsed < min_ms || parsed > max_ms {
        return Err(CliError::usage(format!(
            "{flag_name} must be between {min_ms}ms and {max_ms}ms."
        )));
    }
    Ok(parsed)
}

fn format_duration_for_cli(ms: f64) -> Option<String> {
    if !ms.is_finite() || ms <= 0.0 {
        return None;
    }
    let value = ms.round();
    let day = 24.0 * 60.0 * 60.0 * 1000.0;
    let hour = 60.0 * 60.0 * 1000.0;
    let minute = 60.0 * 1000.0;
    if value % day == 0.0 {
        return Some(format!("{}d", (value / day) as i64));
    }
    if value % hour == 0.0 {
        return Some(format!("{}h", (value / hour) as i64));
    }
    if value % minute == 0.0 {
        return Some(format!("{}m", (value / minute) as i64));
    }
    if value % 1000.0 == 0.0 {
        return Some(format!("{}s", (value / 1000.0) as i64));
    }
    Some(format!("{}ms", value as i64))
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '='))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

struct ReplayParams<'a> {
    port: u16,
    provider: &'a str,
    mode: &'a str,
    profile_name: Option<&'a str>,
    config_path: Option<&'a str>,
    hostname: Option<&'a str>,
    connect_ttl_ms: Option<f64>,
    session_ttl_ms: Option<f64>,
    qr: bool,
    no_qr: bool,
    include_token_placeholder: bool,
    token_via_stdin: bool,
    token_file_provided: bool,
}

fn build_tunnel_start_replay_command(params: &ReplayParams<'_>) -> String {
    let mut parts: Vec<String> = vec!["ompchamber".into(), "tunnel".into(), "start".into()];
    if params.port > 0 {
        parts.push("--port".into());
        parts.push(params.port.to_string());
    }
    if let Some(name) = params.profile_name.filter(|v| !v.is_empty()) {
        parts.push("--profile".into());
        parts.push(shell_quote(name));
    }
    if !params.provider.is_empty() {
        parts.push("--provider".into());
        parts.push(shell_quote(params.provider));
    }
    if !params.mode.is_empty() {
        parts.push("--mode".into());
        parts.push(shell_quote(params.mode));
    }
    if let Some(config) = params.config_path.map(str::trim).filter(|v| !v.is_empty()) {
        parts.push("--config".into());
        parts.push(shell_quote(config));
    }
    if let Some(hostname) = params.hostname.map(str::trim).filter(|v| !v.is_empty()) {
        parts.push("--hostname".into());
        parts.push(shell_quote(hostname));
    }
    if let Some(connect) = format_duration_for_cli(params.connect_ttl_ms.unwrap_or(0.0)) {
        parts.push("--connect-ttl".into());
        parts.push(connect);
    }
    if let Some(session) = format_duration_for_cli(params.session_ttl_ms.unwrap_or(0.0)) {
        parts.push("--session-ttl".into());
        parts.push(session);
    }
    if params.qr {
        parts.push("--qr".into());
    }
    if params.no_qr {
        parts.push("--no-qr".into());
    }
    if params.include_token_placeholder {
        if params.token_via_stdin {
            parts.push("--token-stdin".into());
        } else if params.token_file_provided {
            parts.push("--token-file".into());
            parts.push("<redacted>".into());
        } else {
            parts.push("--token".into());
            parts.push("<redacted>".into());
        }
    }
    parts.join(" ")
}

/// JS serializes whole TTLs as integers (`7200000`, not `7200000.0`).
fn ttl_json(ms: f64) -> serde_json::Value {
    if ms.fract() == 0.0 {
        serde_json::json!(ms as u64)
    } else {
        serde_json::json!(ms)
    }
}

fn build_tunnel_profile_add_command(provider: Option<&str>, hostname: Option<&str>) -> String {
    ["ompchamber", "tunnel", "profile", "add", "--provider"]
        .iter()
        .map(|s| s.to_string())
        .chain([
            shell_quote(provider.filter(|v| !v.is_empty()).unwrap_or("cloudflare")),
            "--mode".to_string(),
            "managed-remote".to_string(),
            "--name".to_string(),
            "<name>".to_string(),
            "--hostname".to_string(),
            shell_quote(hostname.filter(|v| !v.is_empty()).unwrap_or("<hostname>")),
            "--token".to_string(),
            "<token>".to_string(),
        ])
        .collect::<Vec<_>>()
        .join(" ")
}

/// `resolveTunnelTtlOverrides` without the interactive pickers: flags only.
fn resolve_tunnel_ttl_overrides(options: &Options) -> Result<(Option<f64>, Option<f64>), CliError> {
    let connect_ttl_ms = match options.connect_ttl.as_deref() {
        Some(raw) => Some(parse_ttl_ms_or_throw(
            raw,
            "--connect-ttl",
            TUNNEL_BOOTSTRAP_TTL_MIN_MS,
            TUNNEL_BOOTSTRAP_TTL_MAX_MS,
        )?),
        None => None,
    };
    let session_ttl_ms = match options.session_ttl.as_deref() {
        Some(raw) => Some(parse_ttl_ms_or_throw(
            raw,
            "--session-ttl",
            TUNNEL_SESSION_TTL_MIN_MS,
            TUNNEL_SESSION_TTL_MAX_MS,
        )?),
        None => None,
    };
    Ok((connect_ttl_ms, session_ttl_ms))
}

// ---------------------------------------------------------------------------
// cli-args.js helpers (tunnel-local copies)
// ---------------------------------------------------------------------------

fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut dp: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut previous = dp[0];
        dp[0] = i;
        for j in 1..=b.len() {
            let temp = dp[j];
            dp[j] = if a[i - 1] == b[j - 1] {
                previous
            } else {
                1 + dp[j].min(dp[j - 1]).min(previous)
            };
            previous = temp;
        }
    }
    dp[b.len()]
}

fn find_closest_match<'a>(
    input: &str,
    candidates: &[&'a str],
    max_distance: usize,
) -> Option<&'a str> {
    if input.is_empty() {
        return None;
    }
    let normalized = input.to_lowercase();
    let mut best: Option<&'a str> = None;
    let mut best_distance = max_distance + 1;
    for candidate in candidates {
        let distance = levenshtein_distance(&normalized, &candidate.to_lowercase());
        if distance < best_distance {
            best_distance = distance;
            best = Some(candidate);
        }
    }
    if best_distance <= max_distance {
        best
    } else {
        None
    }
}

/// Browser-unsafe ports (Fetch/Chromium restricted ports) — full JS set.
const UNSAFE_BROWSER_PORTS: [u16; 81] = [
    0, 1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101,
    102, 103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427,
    465, 512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990,
    993, 995, 1719, 1720, 1723, 2049, 3659, 4045, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668,
    6669, 6697, 10080,
];

fn is_unsafe_browser_port(port: u16) -> bool {
    UNSAFE_BROWSER_PORTS.contains(&port)
}

/// `assertSafeBrowserPort` (cli-network.js): hard usage error on unsafe ports.
fn assert_safe_browser_port_strict(port: u16, context: &str) -> Result<(), CliError> {
    if !is_unsafe_browser_port(port) {
        return Ok(());
    }
    let url = build_local_url(port, "/", None);
    Err(CliError::usage(format!(
        "{context} cannot use port {port}. Port {port} is browser-unsafe (ERR_UNSAFE_PORT) and is not supported for OMPChamber UI at {url}. Use a safe port such as 3000, 5173, 8080, or a high ephemeral port."
    )))
}

// ---------------------------------------------------------------------------
// cli-http.js / cli-network.js request layer
// ---------------------------------------------------------------------------

/// `resolveApiHost` + `formatHostForUrl` + `buildLocalUrl`.
fn resolve_api_host(host_override: Option<&str>) -> String {
    let configured = host_override
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
        .or_else(|| {
            std::env::var("OMPCHAMBER_HOST")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| "127.0.0.1".to_string());
    match configured.as_str() {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" | "[::]" => "::1".to_string(),
        _ if configured.starts_with('[') && configured.ends_with(']') => {
            configured[1..configured.len() - 1].to_string()
        }
        _ => configured,
    }
}

fn build_local_url(port: u16, endpoint: &str, host_override: Option<&str>) -> String {
    let host = resolve_api_host(host_override);
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host
    };
    let path = if endpoint.starts_with('/') {
        endpoint
    } else {
        &format!("/{endpoint}")
    };
    format!("http://{host}:{port}{path}")
}

#[derive(Debug, Clone)]
struct SystemInfo {
    runtime: String,
    pid: Option<i64>,
}

/// `fetchSystemInfoFromPort` (1.5s timeout, plain fetch).
async fn fetch_system_info_from_port(
    client: &reqwest::Client,
    port: u16,
    host_override: Option<&str>,
) -> Option<SystemInfo> {
    let url = build_local_url(port, "/api/system/info", host_override);
    let response = client
        .get(&url)
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_millis(1500))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let runtime = body
        .get("runtime")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let pid = body
        .get("pid")
        .and_then(serde_json::Value::as_f64)
        .filter(|v| v.is_finite())
        .map(|v| v as i64);
    Some(SystemInfo { runtime, pid })
}

/// `fetchTunnelProvidersFromPort`.
async fn fetch_tunnel_providers_from_port(
    client: &reqwest::Client,
    port: u16,
) -> Option<Vec<serde_json::Value>> {
    let url = build_local_url(port, "/api/ompchamber/tunnel/providers", None);
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    body.get("providers")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .map(|mut list| {
            list.retain(|v| v.is_object());
            list
        })
}

/// `isServerHealthReady`.
async fn is_server_health_ready(client: &reqwest::Client, port: u16, timeout_ms: u64) -> bool {
    let url = build_local_url(port, "/health", None);
    client
        .get(&url)
        .header("Accept", "text/plain")
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// `waitForServerHealth`.
async fn wait_for_server_health(
    client: &reqwest::Client,
    port: u16,
    timeout_ms: u64,
    interval_ms: u64,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if is_server_health_ready(client, port, 1000.min(interval_ms * 2)).await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    }
}

#[derive(Debug, Clone)]
struct JsonResponse {
    status: u16,
    body: serde_json::Value,
}

impl JsonResponse {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

fn transport_error_message(error: &reqwest::Error, endpoint: &str, timeout_ms: u64) -> String {
    if error.is_timeout() {
        format!("Request to {endpoint} timed out after {timeout_ms}ms.")
    } else if error.is_connect() {
        "fetch failed".to_string()
    } else {
        error.to_string()
    }
}

/// `resolveUiPasswordForPort`: explicit flag > instance file > flag value.
fn resolve_ui_password_for_port(port: u16, options: &Options) -> Option<String> {
    if options.explicit_ui_password {
        if let Some(password) = options
            .ui_password
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        {
            return Some(password.to_string());
        }
    }
    if let Some(instance) = process::read_instance_options(&super::paths::instance_file_path(port))
    {
        if let Some(password) = instance
            .ui_password
            .as_deref()
            .filter(|p| !p.trim().is_empty())
        {
            return Some(password.to_string());
        }
    }
    options
        .ui_password
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .map(String::from)
}

/// `getDesktopLocalAuthHeader`.
fn desktop_local_auth_header(port: u16) -> Option<String> {
    let desktop_port = super::paths::read_desktop_local_port_from_settings()?;
    if desktop_port != port {
        return None;
    }
    let token = super::paths::read_desktop_local_client_token_from_settings();
    (!token.is_empty()).then(|| format!("Bearer {token}"))
}

/// `createUiSessionCookie`: POST /auth/session, extract `oc_ui_session`.
async fn create_ui_session_cookie(
    client: &reqwest::Client,
    port: u16,
    password: Option<String>,
    timeout_ms: u64,
) -> Option<String> {
    let password = password?;
    if password.is_empty() {
        return None;
    }
    let url = build_local_url(port, "/auth/session", None);
    let response = client
        .post(&url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .body(serde_json::to_string(&serde_json::json!({ "password": password })).ok()?)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    for value in response.headers().get_all(reqwest::header::SET_COOKIE) {
        let Ok(value) = value.to_str() else { continue };
        if let Some(rest) = value.trim().strip_prefix("oc_ui_session=") {
            let cookie = rest.split(';').next().unwrap_or(rest);
            return Some(format!("oc_ui_session={cookie}"));
        }
    }
    None
}

/// `requestJson`: JSON request with default 4s timeout, desktop bearer auth,
/// and a UI-password session-cookie retry on `UI authentication required`.
async fn request_json(
    client: &reqwest::Client,
    port: u16,
    endpoint: &str,
    method: reqwest::Method,
    body: Option<serde_json::Value>,
    timeout_ms: u64,
    options: &Options,
) -> Result<JsonResponse, String> {
    let url = build_local_url(port, endpoint, None);
    let timeout = std::time::Duration::from_millis(timeout_ms);
    let body_text = body.map(|value| value.to_string());

    let mut request = client
        .request(method.clone(), &url)
        .timeout(timeout)
        .header("Accept", "application/json");
    if let Some(auth) = desktop_local_auth_header(port) {
        request = request.header("Authorization", auth);
    }
    if let Some(body_text) = body_text.as_deref() {
        request = request
            .header("Content-Type", "application/json")
            .body(body_text.to_string());
    }
    let response = request
        .send()
        .await
        .map_err(|e| transport_error_message(&e, endpoint, timeout_ms))?;
    let status = response.status().as_u16();
    let response_body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);

    if status == 401
        && response_body
            .get("error")
            .and_then(serde_json::Value::as_str)
            == Some("UI authentication required")
    {
        let password = resolve_ui_password_for_port(port, options);
        if let Some(cookie) = create_ui_session_cookie(client, port, password, timeout_ms).await {
            let mut retry = client
                .request(method, &url)
                .timeout(timeout)
                .header("Accept", "application/json")
                .header("Cookie", cookie);
            if let Some(auth) = desktop_local_auth_header(port) {
                retry = retry.header("Authorization", auth);
            }
            if let Some(body_text) = body_text.as_deref() {
                retry = retry
                    .header("Content-Type", "application/json")
                    .body(body_text.to_string());
            }
            let response = retry
                .send()
                .await
                .map_err(|e| transport_error_message(&e, endpoint, timeout_ms))?;
            let retry_status = response.status().as_u16();
            let retry_body: serde_json::Value =
                response.json().await.unwrap_or(serde_json::Value::Null);
            return Ok(JsonResponse {
                status: retry_status,
                body: retry_body,
            });
        }
    }

    Ok(JsonResponse {
        status,
        body: response_body,
    })
}

// ---------------------------------------------------------------------------
// cli-lifecycle.js discovery (tunnel view)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct RunningInstance {
    port: u16,
    mtime: u64,
    started_at: f64,
}

enum ProcessState {
    Matched,
    Mismatched,
    Dead,
}

fn get_ompchamber_process_state(pid: u32) -> ProcessState {
    if !process::is_process_running(pid) {
        return ProcessState::Dead;
    }
    match process::read_process_cmdline(pid) {
        Some(cmdline) if process::is_ompchamber_cmdline(&cmdline) => ProcessState::Matched,
        Some(_) => ProcessState::Mismatched,
        None => ProcessState::Mismatched,
    }
}

fn has_ompchamber_runtime_info(info: &Option<SystemInfo>) -> bool {
    info.as_ref()
        .map(|i| !i.runtime.is_empty())
        .unwrap_or(false)
}

/// `getSystemInfoProbeHosts` + `fetchSystemInfoFromPortCandidates`.
async fn fetch_system_info_confirmed(
    client: &reqwest::Client,
    port: u16,
    stored_host: Option<&str>,
    options_host: Option<&str>,
    expected_pid: u32,
) -> Option<SystemInfo> {
    let mut candidates: Vec<(Option<String>, bool)> = Vec::new();
    let push = |host: Option<String>,
                requires_pid_match: bool,
                candidates: &mut Vec<(Option<String>, bool)>| {
        let key = resolve_api_host(host.as_deref());
        if !candidates
            .iter()
            .any(|(h, _)| resolve_api_host(h.as_deref()) == key)
        {
            candidates.push((host.filter(|v| !v.trim().is_empty()), requires_pid_match));
        }
    };
    let has_concrete = stored_host.map(|h| !h.trim().is_empty()).unwrap_or(false)
        || options_host.map(|h| !h.trim().is_empty()).unwrap_or(false);
    if let Some(host) = stored_host.filter(|h| !h.trim().is_empty()) {
        push(Some(host.to_string()), false, &mut candidates);
    }
    if let Some(host) = options_host.filter(|h| !h.trim().is_empty()) {
        push(Some(host.to_string()), false, &mut candidates);
    }
    push(None, has_concrete, &mut candidates);
    push(Some("127.0.0.1".to_string()), has_concrete, &mut candidates);

    for (host, requires_pid_match) in candidates {
        if let Some(info) = fetch_system_info_from_port(client, port, host.as_deref()).await {
            if requires_pid_match && info.pid != Some(expected_pid as i64) {
                continue;
            }
            return Some(info);
        }
    }
    None
}

fn pid_file_mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// `discoverRunningInstances`: pid+instance registry entries confirmed by an
/// `/api/system/info` probe; stale/desktop entries are pruned.
async fn discover_running_instances(
    client: &reqwest::Client,
    options_host: Option<&str>,
) -> Vec<RunningInstance> {
    let mut instances = Vec::new();
    let Ok(entries) = std::fs::read_dir(super::paths::run_dir()) else {
        return instances;
    };
    let mut pid_files: Vec<(u16, PathBuf)> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(port) = name
            .strip_prefix("ompchamber-")
            .and_then(|rest| rest.strip_suffix(".pid"))
            .and_then(|port| port.parse::<u16>().ok())
        else {
            continue;
        };
        if port == 0 {
            continue;
        }
        pid_files.push((port, entry.path()));
    }
    pid_files.sort_by_key(|(port, _)| *port);

    for (port, pid_file) in pid_files {
        let Some(pid) = process::read_pid_file(&pid_file) else {
            process::remove_pid_file(&pid_file);
            process::remove_instance_file(&super::paths::instance_file_path(port));
            continue;
        };
        let instance_file = super::paths::instance_file_path(port);
        let stored = process::read_instance_options(&instance_file);
        let state = get_ompchamber_process_state(pid);
        if matches!(state, ProcessState::Dead) {
            process::remove_pid_file(&pid_file);
            process::remove_instance_file(&instance_file);
            continue;
        }
        let live = fetch_system_info_confirmed(
            client,
            port,
            stored.as_ref().and_then(|s| s.host.as_deref()),
            options_host,
            pid,
        )
        .await;
        if !has_ompchamber_runtime_info(&live) {
            if matches!(state, ProcessState::Mismatched) {
                process::remove_pid_file(&pid_file);
                process::remove_instance_file(&instance_file);
            }
            continue;
        }
        let Some(info) = live else { continue };
        if info.runtime == "desktop" {
            process::remove_pid_file(&pid_file);
            process::remove_instance_file(&instance_file);
            continue;
        }
        instances.push(RunningInstance {
            port,
            mtime: pid_file_mtime(&pid_file),
            started_at: stored.as_ref().map(|s| s.started_at).unwrap_or_default(),
        });
    }
    instances
}

/// `getLatestInstance`.
fn get_latest_instance(mut instances: Vec<RunningInstance>) -> Option<RunningInstance> {
    if instances.is_empty() {
        return None;
    }
    instances.sort_by(|a, b| {
        b.started_at
            .partial_cmp(&a.started_at)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.mtime.cmp(&a.mtime))
            .then(b.port.cmp(&a.port))
    });
    instances.into_iter().next()
}

/// `isDesktopRuntimeForPort`.
fn is_desktop_runtime_for_port(info: &SystemInfo, port: u16) -> bool {
    if info.runtime != "desktop" {
        return false;
    }
    match super::paths::read_desktop_local_port_from_settings() {
        Some(desktop_port) => desktop_port == port,
        None => true,
    }
}

struct Attachability {
    attachable: bool,
    reason: &'static str,
}

/// `inspectTunnelAttachability`.
async fn inspect_tunnel_attachability(client: &reqwest::Client, port: u16) -> Attachability {
    let Some(info) = fetch_system_info_from_port(client, port, None).await else {
        return Attachability {
            attachable: false,
            reason: "unreachable",
        };
    };
    if info.runtime.is_empty() {
        return Attachability {
            attachable: false,
            reason: "unreachable",
        };
    }
    if is_desktop_runtime_for_port(&info, port) {
        return Attachability {
            attachable: false,
            reason: "desktop",
        };
    }
    if !is_server_health_ready(client, port, 1200).await {
        return Attachability {
            attachable: false,
            reason: "unhealthy",
        };
    }
    Attachability {
        attachable: true,
        reason: "ok",
    }
}

/// `discoverDesktopInstance` (port only).
async fn discover_desktop_instance(client: &reqwest::Client) -> Option<u16> {
    let port = super::paths::read_desktop_local_port_from_settings()?;
    let info = fetch_system_info_from_port(client, port, None).await?;
    (info.runtime == "desktop").then_some(port)
}

struct PortStatus {
    port: Option<u16>,
    available: bool,
    line: String,
}

/// `resolveDoctorPortStatuses`.
async fn resolve_doctor_port_statuses(
    client: &reqwest::Client,
    options: &Options,
) -> (Vec<PortStatus>, Vec<RunningInstance>) {
    let running = discover_running_instances(client, options.host.as_deref()).await;
    let desktop_port = discover_desktop_instance(client).await;
    let mut statuses = Vec::new();

    if options.explicit_port {
        let requested = options.port.unwrap_or(DEFAULT_PORT);
        if running.iter().any(|e| e.port == requested) {
            statuses.push(PortStatus {
                port: Some(requested),
                available: true,
                line: format!("port {requested} available for tunneling"),
            });
            return (
                statuses,
                vec![
                    running
                        .iter()
                        .find(|e| e.port == requested)
                        .cloned()
                        .unwrap(),
                ],
            );
        }
        if desktop_port == Some(requested) {
            statuses.push(PortStatus {
                port: Some(requested),
                available: false,
                line: format!("port {requested} not available (desktop runtime)"),
            });
            return (statuses, Vec::new());
        }
        statuses.push(PortStatus {
            port: Some(requested),
            available: false,
            line: format!("port {requested} not available (no running instance)"),
        });
        return (statuses, Vec::new());
    }

    for entry in &running {
        statuses.push(PortStatus {
            port: Some(entry.port),
            available: true,
            line: format!("port {} available for tunneling", entry.port),
        });
    }
    if let Some(port) = desktop_port {
        if !running.iter().any(|e| e.port == port) {
            statuses.push(PortStatus {
                port: Some(port),
                available: false,
                line: format!("port {port} not available (desktop runtime)"),
            });
        }
    }
    if running.is_empty() {
        statuses.push(PortStatus {
            port: None,
            available: false,
            line: "no CLI ports available for tunneling".to_string(),
        });
    }
    (statuses, running)
}

/// `resolveTunnelProviders`: probe candidate ports, fall back to the static
/// capability list.
async fn resolve_tunnel_providers(
    client: &reqwest::Client,
    options: &Options,
    discovered_ports: &[u16],
) -> (Vec<serde_json::Value>, String) {
    let mut candidate_ports: Vec<u16> = Vec::new();
    candidate_ports.push(options.port.unwrap_or(DEFAULT_PORT));
    candidate_ports.extend(discovered_ports.iter().copied());
    if !candidate_ports.contains(&DEFAULT_PORT) {
        candidate_ports.push(DEFAULT_PORT);
    }

    for port in candidate_ports {
        if let Some(providers) = fetch_tunnel_providers_from_port(client, port).await {
            return (providers, format!("api:{port}"));
        }
    }
    (
        default_tunnel_provider_capabilities(),
        "fallback".to_string(),
    )
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

/// `printJson` (cli-output.js): injects `status: "ok"` when absent.
fn emit_json(mut value: serde_json::Value) {
    if let Some(object) = value.as_object_mut() {
        if !object.contains_key("status") {
            object.shift_insert(0, "status".to_string(), serde_json::json!("ok"));
        }
    }
    print_json(&value);
}

/// `logStatus` with clack frames (cli::ui).
fn log_status(message: &str, detail: Option<&str>) {
    super::ui::info(message);
    if let Some(detail) = detail.filter(|d| !d.is_empty()) {
        // detail rides inside the same info block (logStatus joins with \n)
        println!("{detail}");
    }
}

fn clack_intro(title: &str) {
    super::ui::intro(title);
}

fn clack_outro(text: &str) {
    super::ui::outro(text);
}

fn gray_bar() -> String {
    if super::ui::tty_enabled() {
        "\x1b[90m│\x1b[0m".to_string()
    } else {
        "│".to_string()
    }
}

fn step_glyph() -> String {
    if super::ui::tty_enabled() {
        "\x1b[90m◇\x1b[0m".to_string()
    } else {
        "◇".to_string()
    }
}

/// `formatProviderWithIcon`.
fn format_provider_with_icon(provider: Option<&str>) -> String {
    let provider = provider.unwrap_or_default().trim();
    if provider.is_empty() {
        return "unknown".to_string();
    }
    let normalized = provider.to_lowercase();
    if normalized == "cloudflare" {
        format!("☁ {normalized}")
    } else {
        normalized
    }
}

/// `formatModeRequirements`.
fn format_mode_requirements(mode: &serde_json::Value) -> String {
    if mode.get("key").and_then(serde_json::Value::as_str) == Some("managed-local") {
        return "config-path (or default cloudflared config)".to_string();
    }
    let requires: Vec<&str> = mode
        .get("requires")
        .and_then(serde_json::Value::as_array)
        .map(|list| list.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    if requires.is_empty() {
        return "none".to_string();
    }
    requires.join(", ")
}

/// `annotateTunnelProvidersForOutput`.
fn annotate_tunnel_providers_for_output(
    providers: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    providers
        .into_iter()
        .map(|provider| {
            let modes = provider
                .get("modes")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let modes: Vec<serde_json::Value> = modes
                .into_iter()
                .map(|mut mode| {
                    let display_requires = format_mode_requirements(&mode);
                    if let Some(object) = mode.as_object_mut() {
                        object.insert(
                            "displayRequires".to_string(),
                            serde_json::json!(display_requires),
                        );
                    }
                    mode
                })
                .collect();
            let mut annotated = provider;
            if let Some(object) = annotated.as_object_mut() {
                object.insert("modes".to_string(), serde_json::Value::Array(modes));
            }
            annotated
        })
        .collect()
}

fn truthy_env(name: &str) -> bool {
    match std::env::var(name) {
        Ok(value) => {
            let normalized = value.trim().to_lowercase();
            !normalized.is_empty()
                && normalized != "0"
                && normalized != "false"
                && normalized != "no"
        }
        Err(_) => false,
    }
}

/// `shouldDisplayTunnelQr`.
fn should_display_tunnel_qr(options: &Options) -> bool {
    if options.json || options.quiet {
        return false;
    }
    if options.explicit_qr {
        return options.qr == Some(true);
    }
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return false;
    }
    !truthy_env("CI")
}

/// `displayTunnelQrCode` with the qrcode-terminal divergence.
fn display_tunnel_qr_code(url: &str) {
    println!();
    println!("📱 Scan this QR code to access the tunnel:");
    println!();
    println!("{url}");
    println!();
    eprintln!(
        "Warning: Could not generate QR code: QR rendering requires qrcode-terminal (pending)"
    );
}

/// Per-instance probe outcome for ready/status/stop.
struct PortResult {
    port: u16,
    error: Option<String>,
    body: serde_json::Value,
}

fn instances_payload(results: &[PortResult], body_key: &str) -> serde_json::Value {
    serde_json::Value::Array(
        results
            .iter()
            .map(|result| {
                let mut entry = serde_json::Map::new();
                entry.insert("port".to_string(), serde_json::json!(result.port));
                match &result.error {
                    Some(error) => {
                        entry.insert("error".to_string(), serde_json::json!(error));
                    }
                    None => {
                        entry.insert(body_key.to_string(), result.body.clone());
                    }
                }
                serde_json::Value::Object(entry)
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

pub fn command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let subcommand = parsed
        .subcommand
        .clone()
        .or_else(|| parsed.positionals.get(1).cloned())
        .unwrap_or_else(|| "help".to_string());
    match subcommand.as_str() {
        "help" => {
            print!("{}", help_text());
            Ok(())
        }
        "profile" => handle_tunnel_profile_subcommand(&options, parsed.tunnel_action.as_deref()),
        "providers" => block_on(providers_command(&options)),
        "ready" => block_on(ready_command(&options)),
        "status" => block_on(status_command(&options)),
        "doctor" => block_on(doctor_command(&options)),
        "start" => block_on(start_command(options)),
        "stop" => block_on(stop_command(&options)),
        "completion" => completion_command(parsed.tunnel_action.as_deref()),
        other => {
            let known = [
                "help",
                "providers",
                "ready",
                "doctor",
                "status",
                "start",
                "stop",
                "profile",
                "completion",
            ];
            let hint = match find_closest_match(other, &known, 3) {
                Some(suggestion) => format!(" Did you mean '{suggestion}'?"),
                None => String::new(),
            };
            Err(CliError::usage(format!(
                "Unknown tunnel subcommand '{other}'.{hint} Use 'ompchamber tunnel help'."
            )))
        }
    }
}

// ---------------------------------------------------------------------------
// tunnel profile
// ---------------------------------------------------------------------------

fn handle_tunnel_profile_subcommand(
    options: &Options,
    action: Option<&str>,
) -> Result<(), CliError> {
    let sub = action.unwrap_or_default().trim().to_lowercase();
    let store = ensure_tunnel_profiles_migrated();
    let mode = OutputMode::from_options(options);

    if sub.is_empty() {
        match mode {
            OutputMode::Json => emit_json(serde_json::json!({
                "command": "tunnel profile",
                "subcommands": ["list", "show", "add", "remove"],
            })),
            OutputMode::Quiet => {}
            OutputMode::Human => {
                println!("Tunnel Profile");
                log_status("Available subcommands", Some("list, show, add, remove"));
                println!("List profiles: `ompchamber tunnel profile list`");
                println!("Show one profile: `ompchamber tunnel profile show --name <name>`");
                println!(
                    "Add profile: `ompchamber tunnel profile add --provider cloudflare --mode managed-remote --name <name> --hostname <host> --token <token>`"
                );
                println!("Remove profile: `ompchamber tunnel profile remove --name <name>`");
                println!("Choose a subcommand");
            }
        }
        return Ok(());
    }

    if sub == "list" {
        let provider_filter = normalize_profile_provider(options.provider.as_deref());
        let profiles: Vec<&TunnelProfile> = store
            .iter()
            .filter(|p| provider_filter.is_empty() || p.provider == provider_filter)
            .collect();
        match mode {
            OutputMode::Json => emit_json(serde_json::json!({
                "profiles": redact_profiles_for_output(&profiles.iter().map(|p| (*p).clone()).collect::<Vec<_>>(), options.show_secrets),
            })),
            OutputMode::Quiet => {
                for profile in &profiles {
                    println!(
                        "{} {}/{} {}",
                        profile.name, profile.provider, profile.mode, profile.hostname
                    );
                }
            }
            OutputMode::Human => {
                clack_intro("Tunnel Profiles");
                for profile in &profiles {
                    log_status(
                        &format!("{} ({}/{})", profile.name, profile.provider, profile.mode),
                        Some(&format!(
                            "{} {}",
                            profile.hostname,
                            format_profile_token_status(&profile.token, options.show_secrets)
                        )),
                    );
                }
                clack_outro(&format!("{} profile(s)", profiles.len()));
            }
        }
        return Ok(());
    }

    if sub == "show" {
        let name = normalize_profile_name(options.name.as_deref());
        if name.is_empty() {
            return Err(CliError::new(
                "`tunnel profile show` requires --name <name>.",
                GENERAL_ERROR,
            ));
        }
        let profile = resolve_profile_by_name(&store, &name, options.provider.as_deref())
            .map_err(|error| CliError::new(error, GENERAL_ERROR))?;
        match mode {
            OutputMode::Json => emit_json(serde_json::json!({
                "profile": redact_profile_for_output(profile, options.show_secrets),
            })),
            OutputMode::Quiet => println!(
                "{} {}/{} {} {}",
                profile.name,
                profile.provider,
                profile.mode,
                profile.hostname,
                format_profile_token_status(&profile.token, options.show_secrets)
            ),
            OutputMode::Human => {
                println!("Tunnel Profile");
                log_status(
                    &format!("{} ({}/{})", profile.name, profile.provider, profile.mode),
                    Some(&format!(
                        "{} {}",
                        profile.hostname,
                        format_profile_token_status(&profile.token, options.show_secrets)
                    )),
                );
                println!("show complete");
            }
        }
        return Ok(());
    }

    if sub == "add" {
        return profile_add(options, &store);
    }

    if sub == "remove" {
        let name = normalize_profile_name(options.name.as_deref());
        if name.is_empty() {
            return Err(CliError::new(
                "`tunnel profile remove` requires --name <name>.",
                GENERAL_ERROR,
            ));
        }
        let profile = resolve_profile_by_name(&store, &name, options.provider.as_deref())
            .map_err(|error| CliError::new(error, GENERAL_ERROR))?
            .clone();
        let next: Vec<TunnelProfile> = store
            .iter()
            .filter(|e| e.id != profile.id)
            .cloned()
            .collect();
        let persisted = write_tunnel_profiles_to_disk(&next);
        write_managed_remote_pairs_to_disk_from_profiles(&persisted);

        match mode {
            OutputMode::Json => emit_json(serde_json::json!({
                "ok": true,
                "removed": redact_profile_for_output(&profile, options.show_secrets),
            })),
            OutputMode::Quiet => println!(
                "removed {} {}/{} {}",
                profile.name, profile.provider, profile.mode, profile.hostname
            ),
            OutputMode::Human => {
                println!("Tunnel Profile Removed");
                log_status(
                    &format!("{} ({}/{})", profile.name, profile.provider, profile.mode),
                    Some(&profile.hostname),
                );
                println!("remove complete");
            }
        }
        return Ok(());
    }

    let known = ["list", "show", "add", "remove"];
    let hint = match find_closest_match(&sub, &known, 3) {
        Some(suggestion) => format!(" Did you mean '{suggestion}'?"),
        None => String::new(),
    };
    Err(CliError::usage(format!(
        "Unknown tunnel profile subcommand '{sub}'.{hint} Use 'ompchamber tunnel help'."
    )))
}

fn profile_add(options: &Options, store: &[TunnelProfile]) -> Result<(), CliError> {
    let mode = OutputMode::from_options(options);
    let provider = normalize_profile_provider(options.provider.as_deref());
    let profile_mode = normalize_profile_mode(options.mode.as_deref());
    let name = normalize_profile_name(options.name.as_deref());
    let hostname = normalize_profile_hostname(options.hostname.as_deref());
    let token = normalize_profile_token(resolve_token(options)?.as_deref());

    if provider.is_empty() || profile_mode.is_empty() || name.is_empty() || hostname.is_empty() {
        return Err(CliError::new(
            "`tunnel profile add` requires --provider, --mode managed-remote, --name, and --hostname.",
            GENERAL_ERROR,
        ));
    }
    if token.is_empty() {
        return Err(CliError::new(
            "`tunnel profile add` requires a token (--token, --token-file, or --token-stdin).",
            GENERAL_ERROR,
        ));
    }
    if profile_mode != "managed-remote" {
        return Err(CliError::new(
            "`tunnel profile add` currently supports only --mode managed-remote.",
            GENERAL_ERROR,
        ));
    }

    let existing_index = store
        .iter()
        .position(|e| e.provider == provider && e.name.to_lowercase() == name.to_lowercase());

    if existing_index.is_some() && !options.force && !options.dry_run {
        return Err(CliError::new(
            format!(
                "Profile '{name}' already exists for provider '{provider}'. Use --force to overwrite."
            ),
            GENERAL_ERROR,
        ));
    }

    if options.dry_run {
        let candidate = TunnelProfile {
            id: String::new(),
            name: name.clone(),
            provider: provider.clone(),
            mode: profile_mode.clone(),
            hostname: hostname.clone(),
            token: token.clone(),
            created_at: 0,
            updated_at: 0,
        };
        let dry_run_result = serde_json::json!({
            "ok": true,
            "dryRun": true,
            "action": if existing_index.is_some() { "overwrite" } else { "create" },
            "profile": redact_profile_for_output(&candidate, options.show_secrets),
        });
        match mode {
            OutputMode::Json => emit_json(dry_run_result),
            OutputMode::Quiet => {}
            OutputMode::Human => {
                println!("Tunnel Profile Add (dry-run)");
                log_status(
                    &format!(
                        "Would {}: {name} ({provider}/{profile_mode})",
                        if existing_index.is_some() {
                            "overwrite"
                        } else {
                            "create"
                        }
                    ),
                    Some(&format!(
                        "{hostname} {}",
                        format_profile_token_status(&token, options.show_secrets)
                    )),
                );
                println!("dry-run complete (no changes applied)");
            }
        }
        return Ok(());
    }

    let now = now_ms();
    let mut next: Vec<TunnelProfile> = store.to_vec();
    match existing_index {
        Some(index) => {
            let current = next[index].clone();
            next[index] = TunnelProfile {
                mode: profile_mode.clone(),
                hostname: hostname.clone(),
                token: token.clone(),
                updated_at: now,
                ..current
            };
        }
        None => next.push(TunnelProfile {
            id: random_uuid_v4(),
            name: name.clone(),
            provider: provider.clone(),
            mode: profile_mode.clone(),
            hostname: hostname.clone(),
            token: token.clone(),
            created_at: now,
            updated_at: now,
        }),
    }
    let persisted = write_tunnel_profiles_to_disk(&next);
    write_managed_remote_pairs_to_disk_from_profiles(&persisted);
    let added = persisted
        .iter()
        .find(|e| e.provider == provider && e.name.to_lowercase() == name.to_lowercase())
        .cloned()
        .ok_or_else(|| CliError::new("Failed to persist tunnel profile.", GENERAL_ERROR))?;

    match mode {
        OutputMode::Json => emit_json(serde_json::json!({
            "ok": true,
            "profile": redact_profile_for_output(&added, options.show_secrets),
        })),
        OutputMode::Quiet => println!(
            "saved {} {}/{} {}",
            added.name, added.provider, added.mode, added.hostname
        ),
        OutputMode::Human => {
            println!();
            println!("Tunnel Profile Saved");
            log_status(
                &format!("{} ({}/{})", added.name, added.provider, added.mode),
                Some(&format!(
                    "{} {}",
                    added.hostname,
                    format_profile_token_status(&added.token, options.show_secrets)
                )),
            );
            println!("save complete");
            log_status(
                "[START_PROFILE]",
                Some(&format!("ompchamber tunnel start --profile {}", added.name)),
            );
            println!();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tunnel providers
// ---------------------------------------------------------------------------

async fn providers_command(options: &Options) -> Result<(), CliError> {
    let client = reqwest::Client::new();
    let discovered = discover_running_instances(&client, options.host.as_deref()).await;
    let (providers, source) =
        resolve_tunnel_providers(&client, options, &discovered_ports(&discovered)).await;
    let mode = OutputMode::from_options(options);

    match mode {
        OutputMode::Json => emit_json(serde_json::json!({
            "providers": annotate_tunnel_providers_for_output(providers.clone()),
            "source": source,
        })),
        OutputMode::Quiet => {
            for provider in &providers {
                let provider_modes = provider
                    .get("modes")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let provider_id = provider
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("unknown");
                println!("provider {provider_id} modes {}", provider_modes.len());
                for tunnel_mode in &provider_modes {
                    let requires = format_mode_requirements(tunnel_mode).replace(", ", ",");
                    println!(
                        "mode {} requires {requires}",
                        tunnel_mode
                            .get("key")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown")
                    );
                }
            }
        }
        OutputMode::Human => {
            clack_intro("Tunnel Providers");
            for provider in &providers {
                let provider_modes = provider
                    .get("modes")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let provider_id = provider
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("unknown");
                // logStatus('info', provider header) + per-mode log.message
                // steps with `◇ key — label` + indented requirements.
                super::ui::success(&format!(
                    "{} — {} mode(s)",
                    format_provider_with_icon(Some(provider_id)),
                    provider_modes.len()
                ));
                for tunnel_mode in &provider_modes {
                    let key = tunnel_mode
                        .get("key")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown");
                    let label = tunnel_mode
                        .get("label")
                        .and_then(serde_json::Value::as_str)
                        .filter(|v| !v.is_empty())
                        .unwrap_or(key);
                    println!("{}\n{}  {key} — {label}", gray_bar(), step_glyph());
                    println!(
                        "{}    requires: {}",
                        gray_bar(),
                        format_mode_requirements(tunnel_mode)
                    );
                }
            }
            clack_outro(&format!("{} provider(s)", providers.len()));
        }
    }
    Ok(())
}

fn discovered_ports(instances: &[RunningInstance]) -> Vec<u16> {
    instances.iter().map(|e| e.port).collect()
}

// ---------------------------------------------------------------------------
// tunnel ready / status
// ---------------------------------------------------------------------------

/// `resolveTunnelReadEntries`.
async fn resolve_tunnel_read_entries(
    client: &reqwest::Client,
    options: &Options,
) -> Result<Vec<RunningInstance>, CliError> {
    let running = discover_running_instances(client, options.host.as_deref()).await;
    if options.explicit_port {
        let port = options.port.unwrap_or(DEFAULT_PORT);
        if let Some(found) = running.iter().find(|e| e.port == port) {
            return Ok(vec![found.clone()]);
        }
        return Err(CliError::new(
            format!("No running OMPChamber instance found on port {port}."),
            GENERAL_ERROR,
        ));
    }
    if running.is_empty() {
        return Err(CliError::new(
            "No running OMPChamber instance found. Start one with `ompchamber serve`.",
            GENERAL_ERROR,
        ));
    }
    Ok(running)
}

async fn probe_ports(
    client: &reqwest::Client,
    entries: &[RunningInstance],
    endpoint: &str,
    method: reqwest::Method,
    body: Option<serde_json::Value>,
    timeout_ms: u64,
    options: &Options,
    error_label: &str,
) -> Vec<PortResult> {
    let mut results = Vec::new();
    for entry in entries {
        match request_json(
            client,
            entry.port,
            endpoint,
            method.clone(),
            body.clone(),
            timeout_ms,
            options,
        )
        .await
        {
            Ok(response) => {
                if response.ok() {
                    results.push(PortResult {
                        port: entry.port,
                        error: None,
                        body: response.body,
                    });
                } else {
                    let error = response
                        .body
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from)
                        .unwrap_or_else(|| format!("{error_label} {}", response.status));
                    results.push(PortResult {
                        port: entry.port,
                        error: Some(error),
                        body: response.body,
                    });
                }
            }
            Err(message) => results.push(PortResult {
                port: entry.port,
                error: Some(message),
                body: serde_json::Value::Null,
            }),
        }
    }
    results
}

async fn ready_command(options: &Options) -> Result<(), CliError> {
    let client = reqwest::Client::new();
    let entries = resolve_tunnel_read_entries(&client, options).await?;
    let provider = match options
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(provider) => provider.to_lowercase(),
        None => "cloudflare".to_string(),
    };
    let endpoint = format!(
        "/api/ompchamber/tunnel/check?provider={}",
        encode_uri_component(&provider)
    );
    let results = probe_ports(
        &client,
        &entries,
        &endpoint,
        reqwest::Method::GET,
        None,
        4000,
        options,
        "check",
    )
    .await;
    let mode = OutputMode::from_options(options);

    match mode {
        OutputMode::Json => {
            emit_json(serde_json::json!({ "instances": instances_payload(&results, "result") }))
        }
        OutputMode::Quiet => {
            for result in &results {
                if let Some(error) = &result.error {
                    eprintln!("port {} failed: {error}", result.port);
                    continue;
                }
                let provider_id = result
                    .body
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or(&provider)
                    .to_string();
                let available = result
                    .body
                    .get("available")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if available {
                    let version = result
                        .body
                        .get("version")
                        .and_then(serde_json::Value::as_str)
                        .filter(|v| !v.is_empty())
                        .unwrap_or("unknown");
                    println!("port {} ready {provider_id} {version}", result.port);
                } else {
                    let message = result
                        .body
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .filter(|v| !v.is_empty())
                        .unwrap_or("not ready");
                    println!("port {} not-ready {provider_id} {message}", result.port);
                }
            }
        }
        OutputMode::Human => {
            println!("Tunnel Ready");
            for result in &results {
                if let Some(error) = &result.error {
                    log_status(&format!("port {} failed", result.port), Some(error));
                    continue;
                }
                let provider_id = result
                    .body
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or(&provider);
                let available = result
                    .body
                    .get("available")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let line = format!(
                    "port {} provider {}",
                    result.port,
                    format_provider_with_icon(Some(provider_id))
                );
                if available {
                    let version = result
                        .body
                        .get("version")
                        .and_then(serde_json::Value::as_str)
                        .filter(|v| !v.is_empty())
                        .unwrap_or("unknown version");
                    log_status(&line, Some(&format!("ready ({version})")));
                } else {
                    let message = result
                        .body
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .filter(|v| !v.is_empty())
                        .unwrap_or("not ready");
                    log_status(&line, Some(message));
                }
            }
            println!("{} instance(s)", results.len());
        }
    }
    Ok(())
}

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

async fn status_command(options: &Options) -> Result<(), CliError> {
    let client = reqwest::Client::new();
    let entries = resolve_tunnel_read_entries(&client, options).await?;
    let results = probe_ports(
        &client,
        &entries,
        "/api/ompchamber/tunnel/status",
        reqwest::Method::GET,
        None,
        4000,
        options,
        "status",
    )
    .await;
    let mode = OutputMode::from_options(options);

    match mode {
        OutputMode::Json => {
            emit_json(serde_json::json!({ "instances": instances_payload(&results, "status") }))
        }
        OutputMode::Quiet => {
            for result in &results {
                if let Some(error) = &result.error {
                    eprintln!("port {} failed: {error}", result.port);
                    continue;
                }
                let active = result
                    .body
                    .get("active")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let provider =
                    str_field(&result.body, "provider").unwrap_or_else(|| "unknown".to_string());
                let tunnel_mode =
                    str_field(&result.body, "mode").unwrap_or_else(|| "unknown".to_string());
                let url = str_field(&result.body, "url").unwrap_or_else(|| "n/a".to_string());
                println!(
                    "port {} {} {}/{} {url}",
                    result.port,
                    if active { "active" } else { "inactive" },
                    provider,
                    tunnel_mode
                );
            }
        }
        OutputMode::Human => {
            println!("Tunnel Status");
            for result in &results {
                if let Some(error) = &result.error {
                    log_status(&format!("port {} failed", result.port), Some(error));
                    continue;
                }
                let active = result
                    .body
                    .get("active")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let provider =
                    str_field(&result.body, "provider").unwrap_or_else(|| "unknown".to_string());
                let tunnel_mode =
                    str_field(&result.body, "mode").unwrap_or_else(|| "unknown".to_string());
                let url = str_field(&result.body, "url").unwrap_or_else(|| "n/a".to_string());
                let line = format!(
                    "port {} {} ({}/{})",
                    result.port,
                    if active { "active" } else { "inactive" },
                    format_provider_with_icon(Some(&provider)),
                    tunnel_mode
                );
                log_status(&line, Some(&url));
            }
            println!("{} instance(s)", results.len());
        }
    }
    Ok(())
}

fn str_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .map(String::from)
}

// ---------------------------------------------------------------------------
// tunnel doctor
// ---------------------------------------------------------------------------

fn is_valid_tunnel_doctor_response(body: &serde_json::Value) -> bool {
    if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return false;
    }
    if !body
        .get("providerChecks")
        .map(serde_json::Value::is_array)
        .unwrap_or(false)
    {
        return false;
    }
    let Some(modes) = body.get("modes").and_then(serde_json::Value::as_array) else {
        return false;
    };
    modes.iter().all(|entry| {
        if entry
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return false;
        }
        let ready = entry.get("ready").and_then(serde_json::Value::as_bool);
        if ready.is_some()
            && entry
                .get("blockers")
                .map(serde_json::Value::is_array)
                .unwrap_or(false)
        {
            return true;
        }
        entry
            .get("checks")
            .map(serde_json::Value::is_array)
            .unwrap_or(false)
            && entry
                .get("summary")
                .and_then(|s| s.get("ready"))
                .and_then(serde_json::Value::as_bool)
                .is_some()
    })
}

async fn doctor_command(options: &Options) -> Result<(), CliError> {
    let client = reqwest::Client::new();
    let (port_statuses, available_entries) = resolve_doctor_port_statuses(&client, options).await;

    let mut provider_option = options
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_lowercase())
        .unwrap_or_default();

    let mut doctor_profile: Option<TunnelProfile> = None;
    let mut doctor_hostname_override = options
        .hostname
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let explicit_hostname_provided = !doctor_hostname_override.is_empty();
    let explicit_token_provided = options.token_stdin
        || options
            .token
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        || options
            .token_file
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    let mut doctor_token_value = resolve_token(options)?;
    let mut has_saved_managed_remote_profile = false;
    let normalized_mode = options
        .mode
        .as_deref()
        .map(str::trim)
        .map(|v| v.to_lowercase())
        .unwrap_or_default();

    if options
        .profile
        .as_deref()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        let store = ensure_tunnel_profiles_migrated();
        let profile_name = options.profile.as_deref().unwrap_or_default().trim();
        let resolved = resolve_profile_by_name(
            &store,
            profile_name,
            Some(provider_option.as_str())
                .filter(|v| !v.is_empty())
                .or(options.provider.as_deref()),
        )
        .map_err(|error| CliError::new(error, GENERAL_ERROR))?;
        doctor_profile = Some(resolved.clone());
    } else if doctor_hostname_override.is_empty()
        && !explicit_token_provided
        && (normalized_mode.is_empty() || normalized_mode == "managed-remote")
    {
        let store = ensure_tunnel_profiles_migrated();
        let remote_profiles: Vec<&TunnelProfile> = store
            .iter()
            .filter(|entry| {
                entry.mode == "managed-remote"
                    && (provider_option.is_empty() || entry.provider == provider_option)
            })
            .collect();
        has_saved_managed_remote_profile = remote_profiles.iter().any(|entry| {
            let hostname = normalize_profile_hostname(Some(&entry.hostname));
            let token = normalize_profile_token(Some(&entry.token));
            !hostname.is_empty() && !token.is_empty()
        });
        if remote_profiles.len() == 1 {
            doctor_profile = Some(remote_profiles[0].clone());
        }
    }

    if let Some(profile) = &doctor_profile {
        if provider_option.is_empty() {
            provider_option = profile.provider.clone();
        }
        if doctor_hostname_override.is_empty() {
            doctor_hostname_override = profile.hostname.trim().to_string();
        }
        if doctor_token_value
            .as_deref()
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            doctor_token_value = Some(profile.token.trim().to_string());
        }
    }

    let mut doctor_result: Option<serde_json::Value> = None;
    let mut doctor_error: Option<String> = None;
    let mut diagnostics_entries = available_entries.clone();
    diagnostics_entries.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    if !diagnostics_entries.is_empty() {
        let mut query: Vec<(String, String)> = Vec::new();
        if !provider_option.is_empty() {
            query.push(("provider".into(), provider_option.clone()));
        }
        if let Some(mode) = options
            .mode
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            query.push(("mode".into(), mode.to_lowercase()));
        }
        if let Some(config_path) = options.config_path.as_deref() {
            query.push(("configPath".into(), config_path.to_string()));
        }
        if !doctor_hostname_override.is_empty() {
            query.push((
                "managedRemoteTunnelHostname".into(),
                doctor_hostname_override.clone(),
            ));
        }
        if has_saved_managed_remote_profile {
            query.push(("hasSavedManagedRemoteProfile".into(), "1".into()));
        }
        let query_string: String = query
            .iter()
            .map(|(key, value)| format!("{}={}", key, encode_uri_component(value)))
            .collect::<Vec<_>>()
            .join("&");

        let mut doctor_body = serde_json::json!({
            "managedRemoteTunnelTokenProvided": explicit_token_provided,
            "managedRemoteTunnelHostnameProvided": explicit_hostname_provided,
        });
        if let Some(token) = doctor_token_value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            doctor_body["managedRemoteTunnelToken"] = serde_json::json!(token);
        }

        let endpoint = format!("/api/ompchamber/tunnel/doctor?{query_string}");
        let mut failed_ports: Vec<String> = Vec::new();
        for entry in &diagnostics_entries {
            match request_json(
                &client,
                entry.port,
                &endpoint,
                reqwest::Method::POST,
                Some(doctor_body.clone()),
                10000,
                options,
            )
            .await
            {
                Ok(response) => {
                    if response.ok() && is_valid_tunnel_doctor_response(&response.body) {
                        doctor_result = Some(response.body);
                        doctor_error = None;
                        break;
                    }
                    let looks_incompatible = response.ok()
                        && !response
                            .body
                            .get("ok")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                    failed_ports.push(if looks_incompatible {
                        format!(
                            "port {}: doctor endpoint unavailable or incompatible (restart this CLI instance)",
                            entry.port
                        )
                    } else {
                        format!(
                            "port {}: {}",
                            entry.port,
                            response
                                .body
                                .get("error")
                                .and_then(serde_json::Value::as_str)
                                .map(String::from)
                                .unwrap_or_else(|| format!("doctor {}", response.status))
                        )
                    });
                }
                Err(message) => failed_ports.push(format!("port {}: {message}", entry.port)),
            }
        }
        if doctor_result.is_none() {
            doctor_error = Some(if !failed_ports.is_empty() {
                failed_ports[0].clone()
            } else {
                "No compatible CLI instance found for tunnel doctor.".to_string()
            });
        }
    }

    let mode = OutputMode::from_options(options);
    match mode {
        OutputMode::Json => {
            let cli_ports: Vec<serde_json::Value> = port_statuses
                .iter()
                .filter(|s| s.available)
                .map(|s| serde_json::json!({ "port": s.port, "type": "cli", "available": true }))
                .collect();
            let desktop_ports: Vec<serde_json::Value> = port_statuses
                .iter()
                .filter(|s| !s.available)
                .map(|s| serde_json::json!({ "port": s.port, "type": "desktop", "available": false }))
                .collect();
            let all_ports: Vec<serde_json::Value> = [cli_ports, desktop_ports].concat();
            let mut payload = serde_json::json!({
                "ports": all_ports,
                "provider": doctor_result.as_ref().map(|result| {
                    serde_json::json!({
                        "id": result.get("provider").cloned().unwrap_or(serde_json::Value::Null),
                        "checks": result.get("providerChecks").cloned().unwrap_or_else(|| serde_json::json!([])),
                    })
                }),
                "modes": doctor_result
                    .as_ref()
                    .and_then(|result| result.get("modes").cloned())
                    .unwrap_or_else(|| serde_json::json!([])),
            });
            if let Some(error) = &doctor_error {
                payload["error"] = serde_json::json!(error);
            }
            emit_json(payload);
        }
        OutputMode::Quiet => {
            let cli_ports: Vec<String> = port_statuses
                .iter()
                .filter(|s| s.available)
                .filter_map(|s| s.port.map(|p| p.to_string()))
                .collect();
            println!(
                "cli-ports {}",
                if cli_ports.is_empty() {
                    "none".to_string()
                } else {
                    cli_ports.join(",")
                }
            );
            if let Some(error) = &doctor_error {
                eprintln!("doctor-error {error}");
                return Ok(());
            }
            let Some(result) = &doctor_result else {
                println!("doctor unavailable");
                return Ok(());
            };
            let provider_label = result
                .get("provider")
                .and_then(serde_json::Value::as_str)
                .filter(|v| !v.is_empty())
                .map(String::from)
                .unwrap_or_else(|| provider_option.clone());
            let fallback_label = if provider_label.is_empty() {
                "unknown".to_string()
            } else {
                provider_label
            };
            println!("provider {fallback_label}");
            let modes = result
                .get("modes")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            for mode_entry in &modes {
                let mode_name = mode_entry
                    .get("mode")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let ready = mode_entry
                    .get("ready")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                    || mode_entry
                        .get("summary")
                        .and_then(|s| s.get("ready"))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                if ready {
                    println!("mode {mode_name} ready");
                    continue;
                }
                let blockers = doctor_mode_blockers(mode_entry);
                println!("mode {mode_name} not-ready {}", blockers.len());
                for blocker in &blockers {
                    println!("blocker {mode_name} {blocker}");
                }
            }
        }
        OutputMode::Human => {
            let cli_ports: Vec<&PortStatus> =
                port_statuses.iter().filter(|s| s.available).collect();
            let unavailable_ports: Vec<&PortStatus> =
                port_statuses.iter().filter(|s| !s.available).collect();

            println!("Ports");
            for entry in &cli_ports {
                log_status(
                    &format!(
                        "port {} — CLI (available)",
                        entry.port.unwrap_or(DEFAULT_PORT)
                    ),
                    None,
                );
            }
            let mut desktop_unavailable_ports: Vec<u16> = Vec::new();
            let port_label = |port: Option<u16>| {
                port.map(|p| p.to_string())
                    .unwrap_or_else(|| "null".to_string())
            };
            for entry in &unavailable_ports {
                if entry.line.contains("desktop runtime") {
                    if let Some(port) = entry.port {
                        desktop_unavailable_ports.push(port);
                    }
                    log_status(
                        &format!(
                            "port {} — Desktop (tunneling not supported)",
                            port_label(entry.port)
                        ),
                        None,
                    );
                    continue;
                }
                log_status(
                    &format!("port {} — No running instance", port_label(entry.port)),
                    None,
                );
            }
            if !desktop_unavailable_ports.is_empty() {
                println!("Only CLI instances (ompchamber serve) support tunneling.");
            }
            if cli_ports.is_empty() && unavailable_ports.is_empty() {
                log_status(
                    "No running instances found",
                    Some("Start one with `ompchamber serve`."),
                );
                println!("No ports available");
                return Ok(());
            }
            if cli_ports.is_empty() {
                log_status(
                    "No CLI instances available for tunneling",
                    Some("Start one with `ompchamber serve`."),
                );
                println!("No CLI ports available");
                return Ok(());
            }
            println!(
                "{} CLI {} available",
                cli_ports.len(),
                if cli_ports.len() == 1 {
                    "port"
                } else {
                    "ports"
                }
            );
            println!();

            if let Some(profile) = &doctor_profile {
                log_status(
                    "Using saved profile for managed-remote checks",
                    Some(&format!(
                        "{} ({}/{})",
                        profile.name, profile.provider, profile.mode
                    )),
                );
                println!();
            }

            if let Some(error) = &doctor_error {
                println!("Provider");
                log_status("Provider diagnostics failed", Some(error));
                println!("Failed");
                return Ok(());
            }
            let Some(result) = &doctor_result else {
                println!("Provider");
                log_status("Could not reach a running instance for diagnostics", None);
                println!("Unavailable");
                return Ok(());
            };

            let provider_label = format_provider_with_icon(
                result
                    .get("provider")
                    .and_then(serde_json::Value::as_str)
                    .filter(|v| !v.is_empty())
                    .or(Some("unknown")),
            );
            println!("Provider: {provider_label}");

            let provider_checks = result
                .get("providerChecks")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut provider_pass_count = 0usize;
            for check in &provider_checks {
                let passed =
                    check.get("status").and_then(serde_json::Value::as_str) == Some("pass");
                let label = check
                    .get("label")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let detail = check.get("detail").and_then(serde_json::Value::as_str);
                if passed {
                    provider_pass_count += 1;
                    log_status(
                        &match detail.filter(|d| !d.is_empty()) {
                            Some(detail) => format!("{label} — {detail}"),
                            None => label.to_string(),
                        },
                        None,
                    );
                } else {
                    log_status(label, detail.filter(|d| !d.is_empty()));
                }
            }
            let dep_check = provider_checks.iter().find(|c| {
                matches!(
                    c.get("id").and_then(serde_json::Value::as_str),
                    Some("dependency") | Some("provider_dependency")
                )
            });
            if let Some(dep_check) = dep_check {
                if dep_check.get("status").and_then(serde_json::Value::as_str) != Some("pass") {
                    println!("1 blocker — resolve before checking modes");
                    return Ok(());
                }
            }
            println!(
                "{} {} passed",
                provider_pass_count,
                if provider_pass_count == 1 {
                    "check"
                } else {
                    "checks"
                }
            );
            println!();

            let modes = result
                .get("modes")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            if modes.is_empty() {
                return Ok(());
            }

            println!("Modes");
            let mut total_blockers = 0usize;
            let mut troubleshooting_hints: Vec<(&str, &str, [&str; 3])> = Vec::new();
            for mode_entry in &modes {
                let mode_name = mode_entry
                    .get("mode")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let ready = mode_entry
                    .get("ready")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                    || mode_entry
                        .get("summary")
                        .and_then(|s| s.get("ready"))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                if ready {
                    let pass_detail = mode_entry
                        .get("checks")
                        .and_then(serde_json::Value::as_array)
                        .and_then(|checks| {
                            checks.iter().find_map(|c| {
                                (c.get("status").and_then(serde_json::Value::as_str)
                                    == Some("pass")
                                    && !matches!(
                                        c.get("id").and_then(serde_json::Value::as_str),
                                        Some("startup_readiness")
                                            | Some("quick_mode_prerequisites")
                                    ))
                                .then(|| c.get("detail").and_then(serde_json::Value::as_str))?
                            })
                        })
                        .filter(|d| !d.is_empty());
                    log_status(
                        &match pass_detail {
                            Some(detail) => format!("{mode_name} — Ready ({detail})"),
                            None => format!("{mode_name} — Ready"),
                        },
                        None,
                    );
                } else {
                    let blockers = doctor_mode_blockers(mode_entry);
                    total_blockers += blockers.len();
                    let count = blockers.len();
                    let word = if count == 1 { "blocker" } else { "blockers" };
                    log_status(
                        &format!(
                            "{mode_name} — Not ready{optionals}",
                            optionals = if count > 0 {
                                format!(" ({count} {word})")
                            } else {
                                String::new()
                            }
                        ),
                        None,
                    );
                    for blocker in &blockers {
                        println!("  {blocker}");
                    }

                    let normalized_blockers: Vec<String> =
                        blockers.iter().map(|b| b.to_lowercase()).collect();
                    let is_managed_remote = mode_name == "managed-remote";
                    let has_token_issue = normalized_blockers.iter().any(|line| {
                        line.contains("token")
                            || line.contains("unauthorized")
                            || line.contains("forbidden")
                            || line.contains("authentication")
                            || line.contains("auth")
                    });
                    let has_port_or_origin_issue = normalized_blockers.iter().any(|line| {
                        line.contains("port")
                            || line.contains("localhost")
                            || line.contains("127.0.0.1")
                            || line.contains("connection refused")
                            || line.contains("dial tcp")
                    });

                    if is_managed_remote && (has_port_or_origin_issue || has_token_issue) {
                        troubleshooting_hints.push((
                            "managed-remote-port",
                            "[PORT_MISMATCH]",
                            [
                                "Cloudflare target must match the active OMPChamber CLI port.",
                                "Example: `http://127.0.0.1:<port>`",
                                "If CLI picked a different port, update Cloudflare or run `ompchamber serve --port <port>`.",
                            ],
                        ));
                    }
                    if is_managed_remote && has_token_issue {
                        troubleshooting_hints.push((
                            "managed-remote-token",
                            "[QR_PREFETCH_TOKEN]",
                            [
                                "Some QR readers pre-fetch scanned URLs.",
                                "Pre-fetch can consume one-time bootstrap tokens.",
                                "If validation fails, generate a fresh token/QR and use it immediately in one browser/device.",
                            ],
                        ));
                    }
                }
            }
            println!(
                "{}",
                if total_blockers > 0 {
                    format!(
                        "Done ({total_blockers} {})",
                        if total_blockers == 1 {
                            "issue"
                        } else {
                            "issues"
                        }
                    )
                } else {
                    "All modes ready".to_string()
                }
            );

            let mut deduped_hints: Vec<(&str, &str, [&str; 3])> = Vec::new();
            let mut seen_hint_keys: HashSet<&str> = HashSet::new();
            for hint in troubleshooting_hints {
                if seen_hint_keys.insert(hint.0) {
                    deduped_hints.push(hint);
                }
            }
            if !deduped_hints.is_empty() {
                println!();
                println!("Suggestion notes");
                for (key, code, lines) in &deduped_hints {
                    let _ = key;
                    let detail = lines
                        .iter()
                        .map(|line| format!("  {line}"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    log_status(code, Some(&detail));
                }
                println!(
                    "{} {}",
                    deduped_hints.len(),
                    if deduped_hints.len() == 1 {
                        "suggestion"
                    } else {
                        "suggestions"
                    }
                );
            }
        }
    }
    Ok(())
}

/// Doctor mode blockers: `blockers[]` when present, else derived from failed
/// checks (excluding `startup_readiness`).
fn doctor_mode_blockers(mode_entry: &serde_json::Value) -> Vec<String> {
    if let Some(blockers) = mode_entry
        .get("blockers")
        .and_then(serde_json::Value::as_array)
    {
        return blockers
            .iter()
            .map(|b| match b {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
    }
    mode_entry
        .get("checks")
        .and_then(serde_json::Value::as_array)
        .map(|checks| {
            checks
                .iter()
                .filter(|c| {
                    c.get("status").and_then(serde_json::Value::as_str) == Some("fail")
                        && c.get("id").and_then(serde_json::Value::as_str)
                            != Some("startup_readiness")
                })
                .map(|c| {
                    c.get("detail")
                        .and_then(serde_json::Value::as_str)
                        .or(c.get("label").and_then(serde_json::Value::as_str))
                        .or(c.get("id").and_then(serde_json::Value::as_str))
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// tunnel start
// ---------------------------------------------------------------------------

async fn start_command(options: Options) -> Result<(), CliError> {
    let mode = OutputMode::from_options(&options);
    let client = reqwest::Client::new();

    let mut provider = options
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_lowercase())
        .unwrap_or_default();
    let mut tunnel_mode = options
        .mode
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_lowercase())
        .unwrap_or_default();
    let mut token = resolve_token(&options)?;
    let mut hostname = options.hostname.clone();
    let mut selected_profile: Option<TunnelProfile> = None;

    if options.explicit_port {
        assert_safe_browser_port_strict(options.port.unwrap_or(DEFAULT_PORT), "Tunnel start")?;
    }

    if options
        .profile
        .as_deref()
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        let store = ensure_tunnel_profiles_migrated();
        let profile_name = options.profile.as_deref().unwrap_or_default();
        let resolved = resolve_profile_by_name(&store, profile_name, options.provider.as_deref())
            .map_err(|error| CliError::new(error, GENERAL_ERROR))?
            .clone();
        if provider.is_empty() {
            provider = resolved.provider.clone();
        }
        if tunnel_mode.is_empty() {
            tunnel_mode = resolved.mode.clone();
        }
        if token
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .is_none()
        {
            token = Some(resolved.token.clone());
        }
        if options
            .hostname
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            hostname = options.hostname.clone();
        } else {
            hostname = Some(resolved.hostname.clone());
        }
        selected_profile = Some(resolved);
    }

    if provider.is_empty() {
        provider = "cloudflare".to_string();
    }
    if tunnel_mode.is_empty() {
        tunnel_mode = "quick".to_string();
    }

    if tunnel_mode == "managed-remote" {
        if !hostname.as_deref().is_some_and(|v| !v.trim().is_empty()) {
            return Err(CliError::new(
                "Managed-remote mode requires --hostname <hostname>.",
                GENERAL_ERROR,
            ));
        }
        if !token.as_deref().is_some_and(|v| !v.trim().is_empty()) {
            return Err(CliError::new(
                "Managed-remote mode requires a token (--token, --token-file, or --token-stdin).",
                GENERAL_ERROR,
            ));
        }
    }
    let (connect_ttl_ms, session_ttl_ms) = resolve_tunnel_ttl_overrides(&options)?;

    if options.dry_run {
        let dry_run_result = serde_json::json!({
            "ok": true,
            "dryRun": true,
            "provider": provider,
            "mode": tunnel_mode,
            "hostname": hostname.clone().map(|h| serde_json::json!(h)).unwrap_or(serde_json::Value::Null),
            "hasToken": token.as_deref().map(|v| !v.trim().is_empty()).unwrap_or(false),
            "profile": selected_profile.as_ref().map(|p| serde_json::json!(p.name)).unwrap_or(serde_json::Value::Null),
            "configPath": options.config_path.clone().map(|c| serde_json::json!(c)).unwrap_or(serde_json::Value::Null),
            "connectTtlMs": connect_ttl_ms.map(ttl_json).unwrap_or(serde_json::Value::Null),
            "sessionTtlMs": session_ttl_ms.map(ttl_json).unwrap_or(serde_json::Value::Null),
        });
        match mode {
            OutputMode::Json => emit_json(dry_run_result),
            OutputMode::Quiet => {}
            OutputMode::Human => {
                println!("Tunnel Start (dry-run)");
                let target = hostname
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| "(ephemeral URL)".to_string());
                log_status(
                    &format!(
                        "Would start {}/{}",
                        format_provider_with_icon(Some(&provider)),
                        tunnel_mode
                    ),
                    Some(&target),
                );
                println!("dry-run complete (no changes applied)");
            }
        }
        return Ok(());
    }

    let instance = resolve_target_instance(&client, &options, true, false, true)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
            CliError::new(
                "No attachable OMPChamber instance found. Start one with `ompchamber serve`.",
                GENERAL_ERROR,
            )
        })?;
    let auto_started = instance.auto_started;
    let port = instance.entry.port;

    if auto_started {
        if !options.quiet && !options.json {
            log_status(
                &format!("Using auto-started instance on port {port}"),
                Some(&format!("logs: ompchamber logs -p {port}")),
            );
        }
        if !wait_for_server_health(&client, port, 60_000, 250).await {
            return Err(CliError::new(
                format!(
                    "OMPChamber on port {port} is still starting after 60s. Startup time can vary by machine performance. \
                     Wait another minute, then check health with `curl -fsS {}`. \
                     If health is OK, retry tunnel start with `ompchamber tunnel start --port {port}`. \
                     For diagnostics run `ompchamber logs -p {port}`.",
                    build_local_url(port, "/health", None)
                ),
                GENERAL_ERROR,
            ));
        }
    }

    if let Some(profile) = &selected_profile {
        if tunnel_mode == "managed-remote" {
            let token_sync_payload = serde_json::json!({
                "presetId": profile.id,
                "presetName": profile.name,
                "managedRemoteTunnelHostname": hostname,
                "managedRemoteTunnelToken": token,
            });
            let response = request_json(
                &client,
                port,
                "/api/ompchamber/tunnel/managed-remote-token",
                reqwest::Method::PUT,
                Some(token_sync_payload),
                4000,
                &options,
            )
            .await
            .map_err(|message| CliError::new(message, GENERAL_ERROR))?;
            if !response.ok()
                || response.body.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
            {
                return Err(CliError::new(
                    response
                        .body
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(String::from)
                        .unwrap_or_else(|| {
                            format!("Failed to sync tunnel profile token ({})", response.status)
                        }),
                    GENERAL_ERROR,
                ));
            }
        }
    }

    let mut payload = serde_json::json!({ "provider": provider, "mode": tunnel_mode });
    if let Some(connect) = connect_ttl_ms {
        payload["connectTtlMs"] = ttl_json(connect);
    }
    if let Some(session) = session_ttl_ms {
        payload["sessionTtlMs"] = ttl_json(session);
    }
    if let Some(config_path) = options.config_path.as_deref() {
        payload["configPath"] = serde_json::json!(config_path);
    }
    if let Some(token) = token.as_deref() {
        payload["token"] = serde_json::json!(token);
    }
    if let Some(hostname) = hostname.as_deref() {
        payload["hostname"] = serde_json::json!(hostname);
    }
    if let Some(profile) = &selected_profile {
        payload["managedRemoteTunnelPresetId"] = serde_json::json!(profile.id);
        payload["managedRemoteTunnelPresetName"] = serde_json::json!(profile.name);
    }

    let response = match request_json(
        &client,
        port,
        "/api/ompchamber/tunnel/start",
        reqwest::Method::POST,
        Some(payload),
        60_000,
        &options,
    )
    .await
    {
        Ok(response) => response,
        Err(message) => {
            if message.contains("/api/ompchamber/tunnel/start") && message.contains("timed out") {
                return Err(CliError::new(
                    format!(
                        "Tunnel start timed out after 60s. cloudflared may still be starting; check with `ompchamber tunnel status --port {port}`. Run `ompchamber logs -p {port}` for details."
                    ),
                    GENERAL_ERROR,
                ));
            }
            return Err(CliError::new(
                format!("{message} Run `ompchamber logs -p {port}` for details."),
                GENERAL_ERROR,
            ));
        }
    };

    if !response.ok() || response.body.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
    {
        let base_error = response
            .body
            .get("error")
            .and_then(serde_json::Value::as_str)
            .map(String::from)
            .unwrap_or_else(|| format!("Tunnel start failed ({})", response.status));
        let is_cloudflare_timeout = [
            "context deadline exceeded",
            "Client.Timeout exceeded while awaiting headers",
            "failed to request quick Tunnel",
        ]
        .iter()
        .any(|needle| {
            base_error
                .to_lowercase()
                .contains(needle.to_lowercase().as_str())
        });
        let user_error = if is_cloudflare_timeout {
            format!("Cloudflare quick tunnel request timed out. {base_error}")
        } else {
            base_error
        };
        return Err(CliError::new(
            format!("{user_error} Run `ompchamber logs -p {port}` for details."),
            GENERAL_ERROR,
        ));
    }
    let body = response.body;

    let replay_command = build_tunnel_start_replay_command(&ReplayParams {
        port,
        provider: &provider,
        mode: &tunnel_mode,
        profile_name: selected_profile.as_ref().map(|p| p.name.as_str()),
        config_path: options.config_path.as_deref(),
        hostname: hostname.as_deref(),
        connect_ttl_ms,
        session_ttl_ms,
        qr: options.qr == Some(true),
        no_qr: options.qr == Some(false) && options.explicit_qr,
        include_token_placeholder: selected_profile.is_none()
            && tunnel_mode == "managed-remote"
            && token
                .as_deref()
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false),
        token_via_stdin: options.token_stdin,
        token_file_provided: options
            .token_file
            .as_deref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false),
    });

    match mode {
        OutputMode::Json => {
            let mut output = serde_json::json!({ "port": port, "replayCommand": replay_command });
            if let (Some(out), Some(in_body)) = (output.as_object_mut(), body.as_object()) {
                for (key, value) in in_body {
                    out.insert(key.clone(), value.clone());
                }
            }
            emit_json(output);
        }
        OutputMode::Quiet => {
            let quiet_url = str_field(&body, "connectUrl")
                .or_else(|| str_field(&body, "url"))
                .unwrap_or_else(|| "n/a".to_string());
            println!("port {port} {quiet_url}");
        }
        OutputMode::Human => {
            println!();
            println!("Tunnel Started");
            log_status(
                &format!(
                    "port {port} {}/{}",
                    format_provider_with_icon(
                        body.get("provider").and_then(serde_json::Value::as_str)
                    ),
                    str_field(&body, "mode").unwrap_or_else(|| "unknown".to_string())
                ),
                None,
            );
            let tunnel_url = str_field(&body, "connectUrl")
                .or_else(|| str_field(&body, "url"))
                .unwrap_or_else(|| "n/a".to_string());
            log_status(&tunnel_url, None);
            if body
                .get("replacedTunnel")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
            {
                let revoked = finite_or(&body, "revokedBootstrapCount", 0);
                let invalidated = finite_or(&body, "invalidatedSessionCount", 0);
                let previous_mode = body
                    .get("replaced")
                    .and_then(|r| r.get("mode"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|v| !v.is_empty())
                    .unwrap_or("unknown");
                log_status(
                    &format!("replaced previous {previous_mode} tunnel"),
                    Some(&format!("revoked {revoked}, invalidated {invalidated}")),
                );
            }
            println!();

            let mut optional_tips: Vec<(&str, String)> = vec![
                ("Check status", "ompchamber tunnel status".to_string()),
                ("Stop tunnel", "ompchamber tunnel stop".to_string()),
                (
                    "If needed, repeat with same settings",
                    replay_command.clone(),
                ),
            ];
            if selected_profile.is_none()
                && tunnel_mode == "managed-remote"
                && hostname
                    .as_deref()
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
            {
                let profile_save_command =
                    build_tunnel_profile_add_command(Some(&provider), hostname.as_deref());
                optional_tips.push((
                    "Optional: save reusable profile (stores hostname + token locally)",
                    profile_save_command,
                ));
                optional_tips.push((
                    "Start from saved profile",
                    "ompchamber tunnel start --profile <name>".to_string(),
                ));
            }

            println!();
            println!("Optional Tips");
            for (line, detail) in &optional_tips {
                log_status(line, Some(detail));
            }
            println!();
        }
    }

    if should_display_tunnel_qr(&options) {
        let url = str_field(&body, "connectUrl").or_else(|| str_field(&body, "url"));
        if let Some(url) = url.filter(|u| !u.is_empty()) {
            display_tunnel_qr_code(&url);
        }
    }
    Ok(())
}

fn finite_or(body: &serde_json::Value, key: &str, fallback: i64) -> i64 {
    body.get(key)
        .and_then(serde_json::Value::as_f64)
        .filter(|v| v.is_finite())
        .map(|v| v as i64)
        .unwrap_or(fallback)
}

struct TargetInstance {
    entry: RunningInstance,
    auto_started: bool,
}

/// `resolveTargetInstance`: pick the instance to drive. With
/// `require_all`/single semantics this returns one entry per selected
/// instance (start/stop use exactly one except `stop --all`).
async fn resolve_target_instance(
    client: &reqwest::Client,
    options: &Options,
    allow_auto_start: bool,
    require_all: bool,
    reject_desktop_runtime: bool,
) -> Result<Vec<TargetInstance>, CliError> {
    let mut running = discover_running_instances(client, options.host.as_deref()).await;

    if options.all && require_all {
        if running.is_empty() {
            return Err(CliError::new(
                "No running OMPChamber instance found. Start one with `ompchamber serve`.",
                GENERAL_ERROR,
            ));
        }
        return Ok(running
            .into_iter()
            .map(|entry| TargetInstance {
                entry,
                auto_started: false,
            })
            .collect());
    }

    if options.explicit_port {
        let port = options.port.unwrap_or(DEFAULT_PORT);
        if let Some(found) = running.iter().find(|e| e.port == port).cloned() {
            if reject_desktop_runtime {
                let attachability = inspect_tunnel_attachability(client, port).await;
                if !attachability.attachable {
                    if attachability.reason == "desktop" {
                        return Err(CliError::new(
                            format!(
                                "Port {port} is used by OMPChamber Desktop app. Tunnel attach requires a CLI instance from `ompchamber serve`."
                            ),
                            GENERAL_ERROR,
                        ));
                    }
                    return Err(CliError::new(
                        format!(
                            "Port {port} is not an attachable OMPChamber tunnel instance. Ensure it is healthy and running OMPChamber CLI runtime."
                        ),
                        GENERAL_ERROR,
                    ));
                }
            }
            return Ok(vec![TargetInstance {
                entry: found,
                auto_started: false,
            }]);
        }

        if reject_desktop_runtime {
            if let Some(info) =
                fetch_system_info_from_port(client, port, options.host.as_deref()).await
            {
                if is_desktop_runtime_for_port(&info, port) {
                    return Err(CliError::new(
                        format!(
                            "Port {port} is used by OMPChamber Desktop app. Tunnel attach requires a CLI instance from `ompchamber serve`."
                        ),
                        GENERAL_ERROR,
                    ));
                }
            }
        }

        if allow_auto_start {
            auto_start_server(options, Some(port)).await?;
            running = discover_running_instances(client, options.host.as_deref()).await;
            if let Some(started) = running.iter().find(|e| e.port == port).cloned() {
                return Ok(vec![TargetInstance {
                    entry: started,
                    auto_started: true,
                }]);
            }
        }
        return Err(CliError::new(
            format!("No running OMPChamber instance found on port {port}."),
            GENERAL_ERROR,
        ));
    }

    if reject_desktop_runtime {
        let mut attachable_entries: Vec<RunningInstance> = Vec::new();
        let mut saw_desktop = false;
        for entry in &running {
            let attachability = inspect_tunnel_attachability(client, entry.port).await;
            if attachability.reason == "desktop" {
                saw_desktop = true;
            }
            if attachability.attachable {
                attachable_entries.push(entry.clone());
            }
        }
        if attachable_entries.len() == 1 {
            return Ok(vec![TargetInstance {
                entry: attachable_entries.remove(0),
                auto_started: false,
            }]);
        }
        if attachable_entries.len() > 1 {
            let ports: Vec<String> = attachable_entries
                .iter()
                .map(|e| e.port.to_string())
                .collect();
            return Err(CliError::new(
                format!(
                    "Multiple attachable OMPChamber instances found: {}. Use --port <port> or --all.",
                    ports.join(", ")
                ),
                GENERAL_ERROR,
            ));
        }
        if allow_auto_start {
            let started = auto_start_server(options, None).await?;
            running = discover_running_instances(client, options.host.as_deref()).await;
            let found = match started {
                Some(port) => running.iter().find(|e| e.port == port).cloned(),
                None => None,
            };
            let picked = found.or_else(|| get_latest_instance(running.clone()));
            if let Some(entry) = picked {
                return Ok(vec![TargetInstance {
                    entry,
                    auto_started: true,
                }]);
            }
        }
        if saw_desktop {
            return Err(CliError::new(
                "Only OMPChamber Desktop instance(s) detected. Tunnel attach requires a CLI instance from `ompchamber serve`.",
                GENERAL_ERROR,
            ));
        }
        return Err(CliError::new(
            "No attachable OMPChamber instance found. Start one with `ompchamber serve`.",
            GENERAL_ERROR,
        ));
    }

    if running.len() == 1 {
        return Ok(vec![TargetInstance {
            entry: running.remove(0),
            auto_started: false,
        }]);
    }
    if running.is_empty() {
        if allow_auto_start {
            let started = auto_start_server(options, None).await?;
            running = discover_running_instances(client, options.host.as_deref()).await;
            let found = match started {
                Some(port) => running.iter().find(|e| e.port == port).cloned(),
                None => None,
            };
            let picked = found.or_else(|| get_latest_instance(running.clone()));
            if let Some(entry) = picked {
                return Ok(vec![TargetInstance {
                    entry,
                    auto_started: true,
                }]);
            }
        }
        return Err(CliError::new(
            "No running OMPChamber instance found. Start one with `ompchamber serve`.",
            GENERAL_ERROR,
        ));
    }
    let ports: Vec<String> = running.iter().map(|e| e.port.to_string()).collect();
    Err(CliError::new(
        format!(
            "Multiple OMPChamber instances found: {}. Use --port <port> or --all.",
            ports.join(", ")
        ),
        GENERAL_ERROR,
    ))
}

/// Auto-start a serve instance (`serveCommand` dep). Returns the started port
/// when discoverable; callers re-discover from the instance registry.
async fn auto_start_server(
    options: &Options,
    explicit_port: Option<u16>,
) -> Result<Option<u16>, CliError> {
    let serve_options = match explicit_port {
        Some(port) => Options {
            port: Some(port),
            explicit_port: true,
            host: options.host.clone(),
            ui_password: options.ui_password.clone(),
            api_only: options.api_only,
            suppress_unsafe_port_warning: true,
            suppress_ui_password_warning: true,
            suppress_startup_summary: true,
            ..Options::default()
        },
        None => Options {
            explicit_port: false,
            suppress_unsafe_port_warning: true,
            suppress_ui_password_warning: true,
            suppress_startup_summary: true,
            ..options.clone()
        },
    };
    let serve_parsed = Parsed {
        command: "serve".to_string(),
        subcommand: None,
        tunnel_action: None,
        startup_action: None,
        schedule_action: None,
        session_action: None,
        control_action: None,
        options: serve_options.clone(),
        removed_flag_errors: Vec::new(),
        help_requested: false,
        version_requested: false,
        positionals: vec!["serve".to_string()],
    };
    let started_port = serve_options.port;
    super::serve::command(&serve_parsed, serve_options).await?;
    Ok(started_port)
}

// ---------------------------------------------------------------------------
// tunnel stop
// ---------------------------------------------------------------------------

async fn stop_command(options: &Options) -> Result<(), CliError> {
    let client = reqwest::Client::new();
    let entries: Vec<RunningInstance> = if options.all {
        resolve_target_instance(&client, options, false, true, false)
            .await?
            .into_iter()
            .map(|t| t.entry)
            .collect()
    } else {
        resolve_target_instance(&client, options, false, false, false)
            .await?
            .into_iter()
            .map(|t| t.entry)
            .collect()
    };

    let results = probe_ports(
        &client,
        &entries,
        "/api/ompchamber/tunnel/stop",
        reqwest::Method::POST,
        None,
        4000,
        options,
        "stop",
    )
    .await;
    let mode = OutputMode::from_options(options);

    match mode {
        OutputMode::Json => {
            emit_json(serde_json::json!({ "instances": instances_payload(&results, "result") }))
        }
        OutputMode::Quiet => {
            for result in &results {
                if let Some(error) = &result.error {
                    eprintln!("port {} failed: {error}", result.port);
                    continue;
                }
                println!("port {} stopped", result.port);
            }
        }
        OutputMode::Human => {
            println!("Tunnel Stop");
            for result in &results {
                if let Some(error) = &result.error {
                    log_status(&format!("port {} failed", result.port), Some(error));
                    continue;
                }
                let revoked = finite_or(&result.body, "revokedBootstrapCount", 0);
                let invalidated = finite_or(&result.body, "invalidatedSessionCount", 0);
                log_status(
                    &format!("port {} stopped", result.port),
                    Some(&format!("revoked {revoked}, invalidated {invalidated}")),
                );
            }
            println!("{} instance(s)", results.len());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tunnel completion
// ---------------------------------------------------------------------------

fn completion_script(shell: &str) -> Option<&'static str> {
    match shell.trim().to_lowercase().as_str() {
        "bash" => Some(COMPLETION_BASH),
        "zsh" => Some(COMPLETION_ZSH),
        "fish" => Some(COMPLETION_FISH),
        _ => None,
    }
}

fn completion_command(action: Option<&str>) -> Result<(), CliError> {
    let shell = action.filter(|v| !v.is_empty()).unwrap_or("bash");
    let Some(script) = completion_script(shell) else {
        return Err(CliError::usage(format!(
            "Unsupported shell '{shell}'. Supported: bash, zsh, fish."
        )));
    };
    print!("{script}");
    Ok(())
}

const COMPLETION_BASH: &str = r#"# Bash completion for ompchamber tunnel
# Add to ~/.bashrc: eval "$(ompchamber tunnel completion bash)"
_ompchamber_tunnel() {
  local cur prev commands tunnel_commands profile_commands common_flags start_flags
  COMPREPLY=()
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"

    commands="serve stop restart status schedule session models projects tunnel logs update"
  tunnel_commands="help providers ready doctor status start stop profile completion"
  profile_commands="list show add remove"
  common_flags="--port --foreground --no-daemon --json --all --help --version --plain --quiet"
  start_flags="--provider --mode --profile --config --token --token-file --token-stdin --hostname --connect-ttl --session-ttl --qr --no-qr --dry-run --show-secrets"

  if [[ ${COMP_CWORD} -eq 1 ]]; then
    COMPREPLY=( $(compgen -W "${commands}" -- "${cur}") )
    return 0
  fi

  if [[ "${COMP_WORDS[1]}" == "tunnel" ]]; then
    if [[ ${COMP_CWORD} -eq 2 ]]; then
      COMPREPLY=( $(compgen -W "${tunnel_commands}" -- "${cur}") )
      return 0
    fi
    if [[ "${COMP_WORDS[2]}" == "profile" && ${COMP_CWORD} -eq 3 ]]; then
      COMPREPLY=( $(compgen -W "${profile_commands}" -- "${cur}") )
      return 0
    fi
    if [[ "${COMP_WORDS[2]}" == "completion" && ${COMP_CWORD} -eq 3 ]]; then
      COMPREPLY=( $(compgen -W "bash zsh fish" -- "${cur}") )
      return 0
    fi
    if [[ "${COMP_WORDS[2]}" == "start" ]]; then
      COMPREPLY=( $(compgen -W "${start_flags} ${common_flags}" -- "${cur}") )
      return 0
    fi
    COMPREPLY=( $(compgen -W "${common_flags}" -- "${cur}") )
    return 0
  fi

  COMPREPLY=( $(compgen -W "${common_flags}" -- "${cur}") )
  return 0
}
complete -F _ompchamber_tunnel ompchamber
"#;

const COMPLETION_ZSH: &str = r#"#compdef ompchamber
# Zsh completion for ompchamber tunnel
# Add to ~/.zshrc: eval "$(ompchamber tunnel completion zsh)"

_ompchamber() {
  local -a commands tunnel_commands profile_commands

  commands=(
    'serve:Start the web server'
    'stop:Stop running instance(s)'
    'restart:Stop and start the server'
    'status:Show server status'
    'schedule:Manage scheduled tasks'
    'session:Create sessions'
    'models:Show default and favorite models'
    'projects:Show configured projects and IDs'
    'tunnel:Tunnel lifecycle commands'
    'logs:Tail OMPChamber logs'
    'update:Check for and install updates'
  )

  tunnel_commands=(
    'help:Show tunnel help'
    'providers:Show available providers'
    'ready:Check tunnel readiness'
    'doctor:Run tunnel diagnostics'
    'status:Show tunnel status'
    'start:Start a tunnel'
    'stop:Stop active tunnel'
    'profile:Manage saved profiles'
    'completion:Generate shell completion'
  )

  profile_commands=(
    'list:List profiles'
    'show:Show profile details'
    'add:Add a profile'
    'remove:Remove a profile'
  )

  _arguments -C \
    '1:command:->command' \
    '*::arg:->args'

  case $state in
    command)
      _describe 'command' commands
      ;;
    args)
      case $words[1] in
        tunnel)
          if (( CURRENT == 2 )); then
            _describe 'tunnel command' tunnel_commands
          elif [[ $words[2] == "profile" ]] && (( CURRENT == 3 )); then
            _describe 'profile action' profile_commands
          elif [[ $words[2] == "completion" ]] && (( CURRENT == 3 )); then
            _values 'shell' bash zsh fish
          fi
          ;;
      esac
      ;;
  esac
}

compdef _ompchamber ompchamber
"#;

const COMPLETION_FISH: &str = r#"# Fish completion for ompchamber tunnel
# Save to ~/.config/fish/completions/ompchamber.fish

complete -c ompchamber -n '__fish_use_subcommand' -a 'serve' -d 'Start the web server'
complete -c ompchamber -n '__fish_seen_subcommand_from serve' -l foreground -d 'Run in foreground (for systemd/process managers)'
complete -c ompchamber -n '__fish_seen_subcommand_from serve' -l no-daemon -d 'Run in foreground (alias for --foreground)'
complete -c ompchamber -n '__fish_use_subcommand' -a 'stop' -d 'Stop running instance(s)'
complete -c ompchamber -n '__fish_use_subcommand' -a 'restart' -d 'Stop and start the server'
complete -c ompchamber -n '__fish_use_subcommand' -a 'status' -d 'Show server status'
complete -c ompchamber -n '__fish_use_subcommand' -a 'tunnel' -d 'Tunnel lifecycle commands'
complete -c ompchamber -n '__fish_use_subcommand' -a 'logs' -d 'Tail logs'
complete -c ompchamber -n '__fish_use_subcommand' -a 'update' -d 'Check for updates'

complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and not __fish_seen_subcommand_from help providers ready doctor status start stop profile completion' -a 'help' -d 'Show tunnel help'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and not __fish_seen_subcommand_from help providers ready doctor status start stop profile completion' -a 'providers' -d 'Show providers'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and not __fish_seen_subcommand_from help providers ready doctor status start stop profile completion' -a 'ready' -d 'Check readiness'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and not __fish_seen_subcommand_from help providers ready doctor status start stop profile completion' -a 'doctor' -d 'Run diagnostics'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and not __fish_seen_subcommand_from help providers ready doctor status start stop profile completion' -a 'status' -d 'Show tunnel status'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and not __fish_seen_subcommand_from help providers ready doctor status start stop profile completion' -a 'start' -d 'Start a tunnel'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and not __fish_seen_subcommand_from help providers ready doctor status start stop profile completion' -a 'stop' -d 'Stop tunnel'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and not __fish_seen_subcommand_from help providers ready doctor status start stop profile completion' -a 'profile' -d 'Manage profiles'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and not __fish_seen_subcommand_from help providers ready doctor status start stop profile completion' -a 'completion' -d 'Generate completions'

complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l provider -d 'Provider id'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l mode -d 'Tunnel mode'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l profile -d 'Profile name'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l config -d 'Config path'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l token -d 'Token'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l token-file -d 'Token file path'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l token-stdin -d 'Read token from stdin'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l hostname -d 'Hostname'
complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l dry-run -d 'Validate without applying'

complete -c ompchamber -n '__fish_seen_subcommand_from tunnel; and __fish_seen_subcommand_from start' -l qr -d 'Show QR code'
"#;

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::super::USAGE_ERROR;
    use super::*;
    use axum::routing::{get, post};
    use std::sync::Mutex;

    #[test]
    fn ttl_json_prints_whole_numbers_as_integers() {
        assert_eq!(ttl_json(7_200_000.0), serde_json::json!(7_200_000u64));
        assert_eq!(ttl_json(1_800_000.5), serde_json::json!(1_800_000.5));
    }

    /// OMPCHAMBER_DATA_DIR is process-global; serialize tests that touch it.
    fn data_dir_lock() -> &'static Mutex<()> {
        &crate::cli::TEST_ENV_MUTEX
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-tunnel-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn with_data_dir(tag: &str, f: impl FnOnce(&Path)) {
        let _guard = data_dir_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let dir = temp_dir(tag);
        // SAFETY: test-only environment mutation, serialized by the lock.
        unsafe { std::env::set_var("OMPCHAMBER_DATA_DIR", &dir) };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&dir)));
        unsafe { std::env::remove_var("OMPCHAMBER_DATA_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    fn parse(argv: &[&str]) -> Parsed {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        super::super::args::parse_args(&args).expect("parse args")
    }

    fn write_json(path: &Path, value: serde_json::Value) {
        std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }

    // -- profiles ------------------------------------------------------------

    #[test]
    fn profiles_roundtrip_and_legacy_migration() {
        with_data_dir("migration", |dir| {
            // Legacy pairs file with two entries sharing a name.
            let legacy = dir.join("cloudflare-managed-remote-tunnels.json");
            write_json(
                &legacy,
                serde_json::json!({
                    "version": 1,
                    "tunnels": [
                        { "id": "legacy-1", "name": "app", "hostname": "app.example.com", "token": "tok1", "updatedAt": 1000 },
                        { "id": "legacy-2", "name": "app", "hostname": "app2.example.com", "token": "tok2", "updatedAt": 2000 },
                        { "id": "", "name": "  ", "hostname": "", "token": "" },
                    ],
                }),
            );

            let migrated = ensure_tunnel_profiles_migrated();
            assert_eq!(migrated.len(), 2);
            assert_eq!(migrated[0].name, "app");
            assert_eq!(migrated[1].name, "app-2");
            assert_eq!(migrated[0].provider, "cloudflare");
            assert_eq!(migrated[0].mode, "managed-remote");
            assert_eq!(migrated[0].id, "legacy-1");
            assert_eq!(migrated[0].created_at, 1000);

            // New store written, legacy file mirrored from profiles.
            let store_path = dir.join("tunnel-profiles.json");
            let raw = std::fs::read_to_string(&store_path).unwrap();
            assert!(raw.contains("\"name\": \"app-2\""));
            let mirrored: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&legacy).unwrap()).unwrap();
            let tunnels = mirrored["tunnels"].as_array().unwrap();
            assert_eq!(tunnels.len(), 2);
            assert_eq!(tunnels[1]["name"], "app-2");

            // add: duplicate without --force fails; with --force overwrites in place.
            let options = Options {
                provider: Some("Cloudflare".into()),
                mode: Some("managed-remote".into()),
                name: Some("app".into()),
                hostname: Some("app.example.com".into()),
                token: Some("tok3".into()),
                ..Options::default()
            };
            let error = profile_add(&options, &migrated).unwrap_err();
            assert_eq!(
                error.message,
                "Profile 'app' already exists for provider 'cloudflare'. Use --force to overwrite."
            );
            assert_eq!(error.exit_code, GENERAL_ERROR);

            let force_options = Options {
                force: true,
                ..options.clone()
            };
            profile_add(&force_options, &migrated).unwrap();
            let after = ensure_tunnel_profiles_migrated();
            assert_eq!(after.len(), 2);
            let app = after.iter().find(|p| p.name == "app").unwrap();
            assert_eq!(app.token, "tok3");
            assert_eq!(app.hostname, "app.example.com");
            assert_eq!(app.id, "legacy-1");

            // remove: by id, leaves the other.
            let removed = resolve_profile_by_name(&after, "app-2", None).unwrap();
            let next: Vec<TunnelProfile> = after
                .iter()
                .filter(|e| e.id != removed.id)
                .cloned()
                .collect();
            let persisted = write_tunnel_profiles_to_disk(&next);
            write_managed_remote_pairs_to_disk_from_profiles(&persisted);
            assert_eq!(persisted.len(), 1);
            assert_eq!(persisted[0].name, "app");
            assert!(resolve_profile_by_name(&persisted, "app-2", None).is_err());
        });
    }

    #[test]
    fn profile_output_redaction_and_status() {
        let profile = TunnelProfile {
            id: "id-1".into(),
            name: "prod".into(),
            provider: "cloudflare".into(),
            mode: "managed-remote".into(),
            hostname: "app.example.com".into(),
            token: "abcdefghij".into(),
            created_at: 1,
            updated_at: 2,
        };
        let redacted = redact_profile_for_output(&profile, false);
        assert_eq!(redacted["token"], "******ghij");
        assert_eq!(redacted["name"], "prod");
        assert_eq!(redacted["createdAt"], 1);
        let shown = redact_profile_for_output(&profile, true);
        assert_eq!(shown["token"], "abcdefghij");
        assert_eq!(format_profile_token_status(" tok ", false), "token:present");
        assert_eq!(format_profile_token_status("tok", true), "token:tok");
        assert_eq!(format_profile_token_status("", false), "token:missing");
        assert_eq!(mask_token("abc"), "***");
        assert_eq!(mask_token("abcd"), "****");
        assert_eq!(mask_token(""), "***");
        assert_eq!(
            suggest_profile_name_from_hostname(Some("app.example.com")),
            "app"
        );
        assert_eq!(
            suggest_profile_name_from_hostname(Some("my app.io")),
            "my-app"
        );
        assert_eq!(suggest_profile_name_from_hostname(None), "prod-main");
    }

    #[test]
    fn sanitize_dedupes_and_drops_incomplete() {
        let data = serde_json::json!({
            "profiles": [
                { "id": "a", "name": "one", "provider": "cloudflare", "mode": "managed-remote", "hostname": "h", "token": "t" },
                { "id": "b", "name": "ONE", "provider": "cloudflare", "mode": "managed-remote", "hostname": "h", "token": "t" },
                { "id": "c", "name": "no-token", "provider": "cloudflare", "mode": "managed-remote", "hostname": "h", "token": "" },
                { "id": "", "name": "gen", "provider": "cloudflare", "mode": "managed-remote", "hostname": "h", "token": "t" },
            ]
        });
        let sanitized = sanitize_profiles(&data);
        assert_eq!(sanitized.len(), 2);
        assert_eq!(sanitized[0].id, "a");
        assert!(!sanitized[1].id.is_empty());
        assert_ne!(sanitized[1].id, "");
    }

    #[test]
    fn resolve_profile_by_name_errors() {
        let profiles = vec![
            TunnelProfile {
                id: "1".into(),
                name: "dual".into(),
                provider: "cloudflare".into(),
                mode: "managed-remote".into(),
                hostname: "h".into(),
                token: "t".into(),
                created_at: 0,
                updated_at: 0,
            },
            TunnelProfile {
                id: "2".into(),
                name: "dual".into(),
                provider: "ngrok".into(),
                mode: "managed-remote".into(),
                hostname: "h".into(),
                token: "t".into(),
                created_at: 0,
                updated_at: 0,
            },
        ];
        assert_eq!(
            resolve_profile_by_name(&profiles, "missing", None).unwrap_err(),
            "No tunnel profile found for name 'missing'. Run 'ompchamber tunnel profile list'."
        );
        assert_eq!(
            resolve_profile_by_name(&profiles, "dual", None).unwrap_err(),
            "Profile name 'dual' exists for multiple providers. Use --provider <id>."
        );
        assert_eq!(
            resolve_profile_by_name(&profiles, "DUAL", Some("ngrok"))
                .unwrap()
                .id,
            "2"
        );
    }

    // -- token resolution ----------------------------------------------------

    #[test]
    fn token_resolution_sources_and_files() {
        let multiple = Options {
            token_stdin: true,
            token_file: Some("token.txt".into()),
            token: Some("x".into()),
            ..Options::default()
        };
        assert_eq!(
            resolve_token(&multiple).unwrap_err().message,
            "Multiple token sources specified (stdin, file, flag). Use only one of --token, --token-file, or --token-stdin."
        );

        let flag_only = Options {
            token: Some("  abc  ".into()),
            ..Options::default()
        };
        assert_eq!(resolve_token(&flag_only).unwrap(), Some("abc".into()));
        assert_eq!(resolve_token(&Options::default()).unwrap(), None);

        with_data_dir("token-files", |dir| {
            let token_file = dir.join("token.txt");
            std::fs::write(&token_file, "  file-token \n").unwrap();
            let options = Options {
                token_file: Some(token_file.to_string_lossy().into_owned()),
                ..Options::default()
            };
            assert_eq!(resolve_token(&options).unwrap(), Some("file-token".into()));

            let missing = Options {
                token_file: Some(dir.join("nope").to_string_lossy().into_owned()),
                ..Options::default()
            };
            assert_eq!(
                resolve_token(&missing).unwrap_err().message,
                format!("Token file '{}' not found.", dir.join("nope").display())
            );

            let empty = dir.join("empty.txt");
            std::fs::write(&empty, "").unwrap();
            let options = Options {
                token_file: Some(empty.to_string_lossy().into_owned()),
                ..Options::default()
            };
            assert_eq!(
                resolve_token(&options).unwrap_err().message,
                format!("Token file '{}' is empty.", empty.display())
            );

            let big = dir.join("big.txt");
            std::fs::write(&big, "x".repeat(8193)).unwrap();
            let options = Options {
                token_file: Some(big.to_string_lossy().into_owned()),
                ..Options::default()
            };
            assert_eq!(
                resolve_token(&options).unwrap_err().message,
                format!(
                    "Token file '{}' is too large (max 8192 bytes).",
                    big.display()
                )
            );

            let binary = dir.join("bin.txt");
            std::fs::write(&binary, b"a\0b").unwrap();
            let options = Options {
                token_file: Some(binary.to_string_lossy().into_owned()),
                ..Options::default()
            };
            assert_eq!(
                resolve_token(&options).unwrap_err().message,
                format!(
                    "Token file '{}' appears to be binary. Use a plain text token file.",
                    binary.display()
                )
            );

            let dir_token = dir.join("adir");
            std::fs::create_dir_all(&dir_token).unwrap();
            let options = Options {
                token_file: Some(dir_token.to_string_lossy().into_owned()),
                ..Options::default()
            };
            assert_eq!(
                resolve_token(&options).unwrap_err().message,
                format!(
                    "Token file '{}' must be a regular file.",
                    dir_token.display()
                )
            );
        });
    }

    // -- ttl utils -----------------------------------------------------------

    #[test]
    fn ttl_parsing_and_errors() {
        assert_eq!(parse_human_duration_to_ms("30m"), Some(1_800_000.0));
        assert_eq!(parse_human_duration_to_ms("1h30m"), Some(5_400_000.0));
        assert_eq!(parse_human_duration_to_ms("1d"), Some(86_400_000.0));
        assert_eq!(parse_human_duration_to_ms("500"), Some(500.0));
        assert_eq!(parse_human_duration_to_ms("250ms"), Some(250.0));
        assert_eq!(parse_human_duration_to_ms("1x"), None);
        assert_eq!(parse_human_duration_to_ms("h"), None);
        assert_eq!(parse_human_duration_to_ms(""), None);

        assert_eq!(
            parse_ttl_ms_or_throw("nope", "--connect-ttl", 60_000.0, 86_400_000.0)
                .unwrap_err()
                .message,
            "Invalid value for --connect-ttl. Use a positive duration like 30m, 24h, 1d, or milliseconds."
        );
        assert_eq!(
            parse_ttl_ms_or_throw("30s", "--connect-ttl", 60_000.0, 86_400_000.0)
                .unwrap_err()
                .message,
            "--connect-ttl must be between 60000ms and 86400000ms."
        );
        assert_eq!(
            parse_ttl_ms_or_throw("1m", "--session-ttl", 300_000.0, 2_592_000_000.0)
                .unwrap_err()
                .message,
            "--session-ttl must be between 300000ms and 2592000000ms."
        );
        assert_eq!(
            parse_ttl_ms_or_throw("30m", "--connect-ttl", 60_000.0, 86_400_000.0).unwrap(),
            1_800_000.0
        );

        assert_eq!(format_duration_for_cli(1_800_000.0), Some("30m".into()));
        assert_eq!(format_duration_for_cli(86_400_000.0), Some("1d".into()));
        assert_eq!(format_duration_for_cli(0.0), None);
    }

    #[test]
    fn ttl_overrides_from_flags() {
        let options = Options {
            connect_ttl: Some("30m".into()),
            session_ttl: Some("8h".into()),
            ..Options::default()
        };
        let (connect, session) = resolve_tunnel_ttl_overrides(&options).unwrap();
        assert_eq!(connect, Some(1_800_000.0));
        assert_eq!(session, Some(28_800_000.0));
        let (connect, session) = resolve_tunnel_ttl_overrides(&Options::default()).unwrap();
        assert_eq!(connect, None);
        assert_eq!(session, None);
    }

    #[test]
    fn replay_and_profile_add_commands() {
        let command = build_tunnel_start_replay_command(&ReplayParams {
            port: 3000,
            provider: "cloudflare",
            mode: "managed-remote",
            profile_name: Some("prod main"),
            config_path: Some("/etc/cloudflared/config.yml"),
            hostname: Some("app.example.com"),
            connect_ttl_ms: Some(1_800_000.0),
            session_ttl_ms: Some(28_800_000.0),
            qr: false,
            no_qr: true,
            include_token_placeholder: true,
            token_via_stdin: false,
            token_file_provided: false,
        });
        assert_eq!(
            command,
            "ompchamber tunnel start --port 3000 --profile 'prod main' --provider cloudflare --mode managed-remote --config /etc/cloudflared/config.yml --hostname app.example.com --connect-ttl 30m --session-ttl 8h --no-qr --token <redacted>"
        );

        let stdin_variant = build_tunnel_start_replay_command(&ReplayParams {
            port: 0,
            provider: "cloudflare",
            mode: "managed-remote",
            profile_name: None,
            config_path: None,
            hostname: None,
            connect_ttl_ms: None,
            session_ttl_ms: None,
            qr: false,
            no_qr: false,
            include_token_placeholder: true,
            token_via_stdin: true,
            token_file_provided: false,
        });
        assert_eq!(
            stdin_variant,
            "ompchamber tunnel start --provider cloudflare --mode managed-remote --token-stdin"
        );

        let file_variant = build_tunnel_start_replay_command(&ReplayParams {
            port: 3000,
            provider: "cloudflare",
            mode: "managed-remote",
            profile_name: None,
            config_path: None,
            hostname: None,
            connect_ttl_ms: None,
            session_ttl_ms: None,
            qr: false,
            no_qr: false,
            include_token_placeholder: true,
            token_via_stdin: false,
            token_file_provided: true,
        });
        assert!(file_variant.ends_with("--token-file <redacted>"));

        assert_eq!(
            build_tunnel_profile_add_command(Some("cloudflare"), Some("app.example.com")),
            "ompchamber tunnel profile add --provider cloudflare --mode managed-remote --name <name> --hostname app.example.com --token <token>"
        );
        assert_eq!(
            build_tunnel_profile_add_command(None, None),
            "ompchamber tunnel profile add --provider cloudflare --mode managed-remote --name <name> --hostname '<hostname>' --token <token>"
        );
    }

    // -- dispatch matrix -----------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_matrix_errors() {
        // Unknown subcommand with a close-match suggestion.
        let parsed = parse(&["tunnel", "stat"]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(
            error.message,
            "Unknown tunnel subcommand 'stat'. Did you mean 'start'? Use 'ompchamber tunnel help'."
        );

        let parsed = parse(&["tunnel", "zzzzzzzz"]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(
            error.message,
            "Unknown tunnel subcommand 'zzzzzzzz'. Use 'ompchamber tunnel help'."
        );

        // Unknown profile subcommand.
        let parsed = parse(&["tunnel", "profile", "ad"]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(
            error.message,
            "Unknown tunnel profile subcommand 'ad'. Did you mean 'add'? Use 'ompchamber tunnel help'."
        );

        // profile show requires --name.
        let parsed = parse(&["tunnel", "profile", "show"]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(
            error.message,
            "`tunnel profile show` requires --name <name>."
        );
        assert_eq!(error.exit_code, GENERAL_ERROR);

        // profile add requires flags.
        let parsed = parse(&["tunnel", "profile", "add"]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(
            error.message,
            "`tunnel profile add` requires --provider, --mode managed-remote, --name, and --hostname."
        );

        // completion shell.
        let parsed = parse(&["tunnel", "completion", "powershell"]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(
            error.message,
            "Unsupported shell 'powershell'. Supported: bash, zsh, fish."
        );
        assert!(completion_script("  ZSH ").is_some());

        // start managed-remote requires hostname then token.
        let parsed = parse(&["tunnel", "start", "--mode", "managed-remote"]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(
            error.message,
            "Managed-remote mode requires --hostname <hostname>."
        );
        let parsed = parse(&[
            "tunnel",
            "start",
            "--mode",
            "managed-remote",
            "--hostname",
            "app.example.com",
        ]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(
            error.message,
            "Managed-remote mode requires a token (--token, --token-file, or --token-stdin)."
        );

        // ttl range failure happens before any network work.
        let parsed = parse(&["tunnel", "start", "--connect-ttl", "30s"]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(
            error.message,
            "--connect-ttl must be between 60000ms and 86400000ms."
        );

        // unsafe browser port (full JS set: 6000 is unsafe).
        let parsed = parse(&["tunnel", "start", "--port", "6000"]);
        let error = command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert!(
            error
                .message
                .starts_with("Tunnel start cannot use port 6000.")
        );
        assert!(error.message.contains("ERR_UNSAFE_PORT"));
        assert!(!is_unsafe_browser_port(3000));
    }

    #[test]
    fn dispatch_profile_requires_store_lookup() {
        with_data_dir("dispatch-profile", |dir| {
            let store = dir.join("tunnel-profiles.json");
            write_json(
                &store,
                serde_json::json!({
                    "version": 1,
                    "profiles": [
                        { "id": "p1", "name": "prod", "provider": "cloudflare", "mode": "managed-remote", "hostname": "app.example.com", "token": "tok", "createdAt": 1, "updatedAt": 1 }
                    ],
                }),
            );
            let parsed = parse(&["tunnel", "profile", "show", "--name", "prod"]);
            command(&parsed, parsed.options.clone()).unwrap();

            let parsed = parse(&["tunnel", "profile", "show", "--name", "ghost"]);
            let error = command(&parsed, parsed.options.clone()).unwrap_err();
            assert_eq!(
                error.message,
                "No tunnel profile found for name 'ghost'. Run 'ompchamber tunnel profile list'."
            );
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_dry_run_requires_no_network() {
        with_data_dir("start-dry-run", |_dir| {
            let parsed = parse(&[
                "tunnel",
                "start",
                "--dry-run",
                "--mode",
                "managed-remote",
                "--hostname",
                "app.example.com",
                "--token",
                "tok",
            ]);
            command(&parsed, parsed.options.clone()).unwrap();
        });
    }

    // -- fake HTTP output shapes ----------------------------------------------

    async fn fake_server(routes: axum::Router) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        (port, handle)
    }

    fn running(port: u16) -> RunningInstance {
        RunningInstance {
            port,
            mtime: 0,
            started_at: 0.0,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn status_and_ready_shapes_on_fake_http() {
        let routes = axum::Router::new()
            .route(
                "/api/ompchamber/tunnel/status",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "active": true,
                        "url": "https://x.trycloudflare.com",
                        "mode": "quick",
                        "provider": "cloudflare",
                    }))
                }),
            )
            .route(
                "/api/ompchamber/tunnel/check",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "available": true,
                        "provider": "cloudflare",
                        "version": "2024.1.1",
                        "message": null,
                    }))
                }),
            );
        let (port, _server) = fake_server(routes).await;
        let client = reqwest::Client::new();
        let options = Options::default();

        let status_results = probe_ports(
            &client,
            &[running(port)],
            "/api/ompchamber/tunnel/status",
            reqwest::Method::GET,
            None,
            4000,
            &options,
            "status",
        )
        .await;
        assert!(status_results[0].error.is_none());
        let payload = instances_payload(&status_results, "status");
        assert_eq!(payload[0]["port"], port);
        assert_eq!(payload[0]["status"]["url"], "https://x.trycloudflare.com");

        let ready_results = probe_ports(
            &client,
            &[running(port)],
            &format!(
                "/api/ompchamber/tunnel/check?provider={}",
                encode_uri_component("cloudflare")
            ),
            reqwest::Method::GET,
            None,
            4000,
            &options,
            "check",
        )
        .await;
        let payload = instances_payload(&ready_results, "result");
        assert_eq!(payload[0]["result"]["available"], true);
        assert_eq!(payload[0]["result"]["version"], "2024.1.1");

        // Error shape: HTTP failure maps body.error through.
        let routes = axum::Router::new().route(
            "/api/ompchamber/tunnel/status",
            get(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({ "error": "boom" })),
                )
            }),
        );
        let (bad_port, _server) = fake_server(routes).await;
        let results = probe_ports(
            &client,
            &[running(bad_port)],
            "/api/ompchamber/tunnel/status",
            reqwest::Method::GET,
            None,
            4000,
            &options,
            "status",
        )
        .await;
        assert_eq!(results[0].error.as_deref(), Some("boom"));
        let payload = instances_payload(&results, "status");
        assert_eq!(payload[0]["error"], "boom");
        assert!(payload[0].get("status").is_none());

        // Unreachable port maps to a fetch failure error entry.
        let results = probe_ports(
            &client,
            &[running(1)],
            "/api/ompchamber/tunnel/status",
            reqwest::Method::GET,
            None,
            4000,
            &options,
            "status",
        )
        .await;
        assert_eq!(results[0].error.as_deref(), Some("fetch failed"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_shape_on_fake_http() {
        let routes = axum::Router::new().route(
            "/api/ompchamber/tunnel/stop",
            post(|| async {
                axum::Json(serde_json::json!({
                    "ok": true,
                    "revokedBootstrapCount": 2,
                    "invalidatedSessionCount": 1,
                }))
            }),
        );
        let (port, _server) = fake_server(routes).await;
        let client = reqwest::Client::new();
        let options = Options::default();
        let results = probe_ports(
            &client,
            &[running(port)],
            "/api/ompchamber/tunnel/stop",
            reqwest::Method::POST,
            None,
            4000,
            &options,
            "stop",
        )
        .await;
        assert!(results[0].error.is_none());
        assert_eq!(finite_or(&results[0].body, "revokedBootstrapCount", 0), 2);
        let payload = instances_payload(&results, "result");
        assert_eq!(payload[0]["result"]["invalidatedSessionCount"], 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn providers_fallback_and_annotation() {
        let (providers, source) =
            resolve_tunnel_providers(&reqwest::Client::new(), &Options::default(), &[]).await;
        assert_eq!(source, "fallback");
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0]["provider"], "cloudflare");
        assert_eq!(providers[1]["modes"].as_array().unwrap().len(), 1);

        let annotated = annotate_tunnel_providers_for_output(providers);
        let modes = annotated[0]["modes"].as_array().unwrap();
        assert_eq!(modes[0]["displayRequires"], "none");
        assert_eq!(modes[1]["displayRequires"], "token, hostname");
        assert_eq!(
            modes[2]["displayRequires"],
            "config-path (or default cloudflared config)"
        );
        assert_eq!(
            format_mode_requirements(&modes[1]).replace(", ", ","),
            "token,hostname"
        );
    }

    #[test]
    fn doctor_response_validation() {
        let valid = serde_json::json!({
            "ok": true,
            "provider": "cloudflare",
            "providerChecks": [],
            "modes": [ { "mode": "quick", "ready": true, "blockers": [] } ],
        });
        assert!(is_valid_tunnel_doctor_response(&valid));
        let server_shape = serde_json::json!({
            "ok": true,
            "provider": "cloudflare",
            "providerChecks": [],
            "modes": [ { "mode": "quick", "checks": [], "summary": { "ready": false } } ],
        });
        assert!(is_valid_tunnel_doctor_response(&server_shape));
        assert!(!is_valid_tunnel_doctor_response(
            &serde_json::json!({ "ok": false })
        ));
        assert!(!is_valid_tunnel_doctor_response(
            &serde_json::json!({ "ok": true, "modes": [ { "mode": "x" } ] })
        ));

        let entry = serde_json::json!({
            "mode": "managed-remote",
            "checks": [
                { "id": "dependency", "status": "fail", "label": "cloudflared", "detail": "not installed" },
                { "id": "startup_readiness", "status": "fail", "label": "startup", "detail": "skipped" },
            ],
        });
        assert_eq!(
            doctor_mode_blockers(&entry),
            vec!["not installed".to_string()]
        );
        let with_blockers = serde_json::json!({ "mode": "quick", "blockers": ["a", 2] });
        assert_eq!(
            doctor_mode_blockers(&with_blockers),
            vec!["a".to_string(), "2".to_string()]
        );
    }

    #[test]
    fn levenshtein_matches_js_distances() {
        assert_eq!(levenshtein_distance("stat", "status"), 2);
        assert_eq!(levenshtein_distance("ad", "add"), 1);
        assert_eq!(
            find_closest_match("stat", &["status", "start"], 3),
            Some("start")
        );
        assert_eq!(find_closest_match("", &["status"], 3), None);
        assert_eq!(find_closest_match("zzzzzzzz", &["status"], 3), None);
    }

    #[test]
    fn url_building_matches_cli_network() {
        assert_eq!(
            build_local_url(3000, "/health", None),
            "http://127.0.0.1:3000/health"
        );
        assert_eq!(
            build_local_url(3000, "health", None),
            "http://127.0.0.1:3000/health"
        );
        assert_eq!(
            build_local_url(3000, "/", Some("0.0.0.0")),
            "http://127.0.0.1:3000/"
        );
        assert_eq!(build_local_url(3000, "/", Some("::")), "http://[::1]:3000/");
        assert_eq!(
            build_local_url(3000, "/", Some("[::1]")),
            "http://[::1]:3000/"
        );
    }

    #[test]
    fn qr_gate_follows_js_rules() {
        let mut options = Options {
            json: true,
            qr: Some(true),
            explicit_qr: true,
            ..Options::default()
        };
        assert!(!should_display_tunnel_qr(&options));
        options = Options {
            quiet: true,
            qr: Some(true),
            explicit_qr: true,
            ..Options::default()
        };
        assert!(!should_display_tunnel_qr(&options));
        options = Options {
            qr: Some(false),
            explicit_qr: true,
            ..Options::default()
        };
        assert!(!should_display_tunnel_qr(&options));
        // Non-TTY stdout (test harness) blocks the implicit path.
        options = Options::default();
        assert!(!should_display_tunnel_qr(&options));
    }

    #[test]
    fn token_sync_payload_reaches_managed_remote_token_route() {
        // Shape check for the PUT payload built in start_command.
        let profile = TunnelProfile {
            id: "p1".into(),
            name: "prod".into(),
            provider: "cloudflare".into(),
            mode: "managed-remote".into(),
            hostname: "app.example.com".into(),
            token: "tok".into(),
            created_at: 0,
            updated_at: 0,
        };
        let payload = serde_json::json!({
            "presetId": profile.id,
            "presetName": profile.name,
            "managedRemoteTunnelHostname": Some(profile.hostname.clone()),
            "managedRemoteTunnelToken": Some(profile.token.clone()),
        });
        assert_eq!(payload["presetId"], "p1");
        assert_eq!(payload["managedRemoteTunnelToken"], "tok");
    }
}
