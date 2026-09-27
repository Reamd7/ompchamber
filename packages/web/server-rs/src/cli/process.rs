//! Port of `bin/lib/cli-process.js`: pid/instance files, liveness, cmdline
//! identity, process-tree termination.

use std::path::Path;

use super::paths;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceOptions {
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default)]
    pub launch_mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_password: Option<String>,
    #[serde(default)]
    pub has_ui_password: bool,
    #[serde(default)]
    pub api_only: bool,
    #[serde(default)]
    pub started_at: f64,
}

pub fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse::<u32>().ok())
}

pub fn write_pid_file(path: &Path, pid: u32) {
    if let Err(error) = std::fs::write(path, pid.to_string()) {
        eprintln!("Warning: Could not write PID file: {error}");
    }
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

pub fn remove_pid_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub fn read_instance_options(path: &Path) -> Option<InstanceOptions> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

pub fn write_instance_options(path: &Path, options: &InstanceOptions) {
    if let Ok(json) = serde_json::to_string_pretty(options) {
        if let Err(error) = std::fs::write(path, json) {
            eprintln!("Warning: Could not write instance file: {error}");
        }
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

pub fn remove_instance_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Liveness only — "is *some* process alive with this PID".
pub fn is_process_running(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Best-effort command line for identity verification (`ps` on macOS,
/// /proc on Linux); None when undeterminable.
pub fn read_process_cmdline(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .ok()
            .map(|raw| raw.replace('\0', " ").trim().to_string())
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .ok()?;
        let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!out.is_empty()).then_some(out)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// `isOmpchamberCmdline`: the process is one of ours.
pub fn is_ompchamber_cmdline(cmdline: &str) -> bool {
    let lower = cmdline.to_ascii_lowercase();
    (lower.contains("ompchamber") || lower.contains("ompchamber-server"))
        && (lower.contains("server/index.js")
            || lower.contains("ompchamber-server")
            || lower.contains("serve"))
}

/// `terminateProcessTree`: TERM to the group, bounded wait, then KILL.
pub fn terminate_process_tree(pid: u32, graceful_timeout_ms: u64, force_timeout_ms: u64) {
    let send = |signal: &str| {
        let _ = std::process::Command::new("/bin/kill")
            .args([format!("-{signal}"), format!("-{pid}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::process::Command::new("/bin/kill")
            .args([format!("-{signal}"), pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    };
    send("TERM");
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(graceful_timeout_ms);
    while is_process_running(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if is_process_running(pid) {
        send("KILL");
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(force_timeout_ms);
        while is_process_running(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// Enumerate instance files in the run dir with live pids.
pub fn discover_instances() -> Vec<(u16, InstanceOptions, u32)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(paths::run_dir()) else {
        return found;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(port) = name
            .strip_prefix("ompchamber-")
            .and_then(|rest| rest.strip_suffix(".json"))
            .and_then(|port| port.parse::<u16>().ok())
        else {
            continue;
        };
        let Some(instance) = read_instance_options(&entry.path()) else {
            continue;
        };
        let pid_file = paths::pid_file_path(port);
        let Some(pid) = read_pid_file(&pid_file) else {
            continue;
        };
        if !is_process_running(pid) {
            continue;
        }
        found.push((port, instance, pid));
    }
    found.sort_by_key(|(port, _, _)| *port);
    found
}
