//! Port of `bin/lib/cli-paths.js` (+ the settings accessors it carries).
//!
//! 中文说明：移植自 `bin/lib/cli-paths.js`（及其附带的 settings 读取）：
//! 数据目录（OMPCHAMBER_DATA_DIR 可覆盖）、logs/run 子目录、各类状态
//! 文件路径，以及 settings.json 中桌面本地端口/客户端令牌的读取。

use std::path::PathBuf;

/// 数据目录：OMPCHAMBER_DATA_DIR 环境变量优先（空白忽略），否则
/// ~/.config/ompchamber，取不到 home 时退回当前目录下的 .ompchamber。
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

/// 日志目录：<data_dir>/logs。
pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// settings.json 路径：<data_dir>/settings.json。
pub fn settings_file_path() -> PathBuf {
    data_dir().join("settings.json")
}

/// run 目录：<data_dir>/run；确保存在，Unix 下设 0o700（内含 pid 与
/// 实例状态等运行时敏感文件）。
pub fn run_dir() -> PathBuf {
    let dir = data_dir().join("run");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir
}

/// 确保日志目录存在（失败静默，由后续写日志时的错误兜底）。
pub fn ensure_logs_dir() {
    let _ = std::fs::create_dir_all(logs_dir());
}

/// 指定端口对应的日志文件路径。
pub fn log_file_path(port: &str) -> PathBuf {
    logs_dir().join(format!("ompchamber-{port}.log"))
}

/// 指定端口对应的 pid 文件路径（run 目录内）。
pub fn pid_file_path(port: u16) -> PathBuf {
    run_dir().join(format!("ompchamber-{port}.pid"))
}

/// 指定端口对应的实例状态 JSON 路径（run 目录内）。
pub fn instance_file_path(port: u16) -> PathBuf {
    run_dir().join(format!("ompchamber-{port}.json"))
}

/// tunnel profile 配置文件路径。
pub fn tunnel_profiles_file_path() -> PathBuf {
    data_dir().join("tunnel-profiles.json")
}

/// 旧版 Cloudflare managed remote tunnel 配置路径（迁移读取用）。
pub fn legacy_cloudflare_managed_remote_file_path() -> PathBuf {
    data_dir().join("cloudflare-managed-remote-tunnels.json")
}

/// tunnel CLI 状态文件路径。
pub fn tunnel_cli_state_file_path() -> PathBuf {
    data_dir().join("tunnel-cli-state.json")
}

/// 读取并解析 settings.json；文件缺失或非法 JSON 一律返回 Null
/// （调用方按字段缺省处理）。
fn read_settings_json() -> serde_json::Value {
    std::fs::read_to_string(settings_file_path())
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// `readDesktopLocalPortFromSettings`.
/// 中文：读取 desktopLocalPort；仅接受 1..=65535 的数值，否则 None。
pub fn read_desktop_local_port_from_settings() -> Option<u16> {
    read_settings_json()
        .get("desktopLocalPort")
        .and_then(|v| v.as_f64())
        .filter(|v| *v > 0.0 && *v <= 65535.0)
        .and_then(|v| u16::try_from(v as i64).ok())
}

/// `readDesktopLocalClientTokenFromSettings`.
/// 中文：读取 desktopLocalClientToken（trim 后）；缺失或空白返回空串。
pub fn read_desktop_local_client_token_from_settings() -> String {
    read_settings_json()
        .get("desktopLocalClientToken")
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_default()
}
