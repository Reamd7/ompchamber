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

/// Liveness only — "is *some* process alive with this PID". Use this when the
/// PID is known to be ours (a child we just spawned, or a process we are
/// stopping). Do NOT use it to validate a PID read from a pid file: after an
/// ungraceful shutdown the pid file is stale and the kernel may have recycled
/// that PID to an unrelated process — see `is_ompchamber_cmdline`.
#[cfg(unix)]
pub fn is_process_running(pid: u32) -> bool {
    std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Windows has no /bin/kill; probe liveness with `tasklist /FI "PID eq n"`.
/// Same probe engine.rs uses for its async exit watcher (`signal_process`
/// with signal "0"), kept in sync deliberately.
#[cfg(windows)]
pub fn is_process_running(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            // A match lists one process row containing the bare pid; the
            // no-match case prints an "INFO: No tasks" header instead.
            text.split_whitespace()
                .any(|token| token == pid.to_string())
        }
        _ => false,
    }
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

/// `terminateProcessTree` unix: TERM to the group then the pid, bounded wait,
/// then KILL.
#[cfg(unix)]
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

/// `terminateProcessTree` win32 branch: terminate the single pid first
/// (the JS `process.kill(pid)` TerminateProcess), then `taskkill /T` (tree),
/// then `taskkill /T /F` — each followed by a bounded liveness wait.
#[cfg(windows)]
pub fn terminate_process_tree(pid: u32, graceful_timeout_ms: u64, force_timeout_ms: u64) {
    fn taskkill(pid: u32, extra: &[&str]) {
        let _ = std::process::Command::new("taskkill")
            .arg("/PID")
            .arg(pid.to_string())
            .args(extra)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    fn wait_gone(pid: u32, timeout_ms: u64) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        while is_process_running(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        !is_process_running(pid)
    }

    taskkill(pid, &["/F"]);
    if wait_gone(pid, 800) {
        return;
    }
    taskkill(pid, &["/T"]);
    if wait_gone(pid, graceful_timeout_ms) {
        return;
    }
    taskkill(pid, &["/T", "/F"]);
    wait_gone(pid, force_timeout_ms);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn spawn_sleeper() -> std::process::Child {
        std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap()
    }

    #[cfg(windows)]
    fn spawn_sleeper() -> std::process::Child {
        // `ping -n` is the reliable Windows sleeper: `timeout` needs an
        // interactive console and stalls under redirected test output.
        std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn liveness_detects_self_and_missing_pid() {
        assert!(is_process_running(std::process::id()));
        // u32::MAX sits outside every supported pid space, so the probe
        // must answer "no such process".
        assert!(!is_process_running(u32::MAX));
    }

    #[test]
    fn terminate_process_tree_stops_a_live_child() {
        let mut child = spawn_sleeper();
        let pid = child.id();
        assert!(is_process_running(pid));
        terminate_process_tree(pid, 2500, 3000);
        assert!(!is_process_running(pid));
        let _ = child.wait();
    }
}
