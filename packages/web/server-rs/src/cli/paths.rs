//! Port of `bin/lib/cli-paths.js` (+ the settings accessors it carries).

use std::path::PathBuf;

pub fn data_dir() -> PathBuf {
    if let Ok(value) = std::env::var("OMPCHAMBER_DATA_DIR") {
        let value = value.trim();
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    crate::config::home_dir()
        .map(|home| home.join(".config").join("ompchamber"))
        .unwrap_or_else(|| PathBuf::from(".ompchamber"))
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn settings_file_path() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn run_dir() -> PathBuf {
    let dir = data_dir().join("run");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

pub fn ensure_logs_dir() {
    let _ = std::fs::create_dir_all(logs_dir());
}

pub fn log_file_path(port: &str) -> PathBuf {
    logs_dir().join(format!("ompchamber-{port}.log"))
}

pub fn pid_file_path(port: u16) -> PathBuf {
    run_dir().join(format!("ompchamber-{port}.pid"))
}

pub fn instance_file_path(port: u16) -> PathBuf {
    run_dir().join(format!("ompchamber-{port}.json"))
}

pub fn tunnel_profiles_file_path() -> PathBuf {
    data_dir().join("tunnel-profiles.json")
}

pub fn legacy_cloudflare_managed_remote_file_path() -> PathBuf {
    data_dir().join("cloudflare-managed-remote-tunnels.json")
}

pub fn tunnel_cli_state_file_path() -> PathBuf {
    data_dir().join("tunnel-cli-state.json")
}

fn read_settings_json() -> serde_json::Value {
    std::fs::read_to_string(settings_file_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// `readDesktopLocalPortFromSettings`.
pub fn read_desktop_local_port_from_settings() -> Option<u16> {
    read_settings_json()
        .get("desktopLocalPort")
        .and_then(|v| v.as_f64())
        .filter(|v| *v > 0.0 && *v <= 65535.0)
        .and_then(|v| u16::try_from(v as i64).ok())
}

/// `readDesktopLocalClientTokenFromSettings`.
pub fn read_desktop_local_client_token_from_settings() -> String {
    read_settings_json()
        .get("desktopLocalClientToken")
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_default()
}
