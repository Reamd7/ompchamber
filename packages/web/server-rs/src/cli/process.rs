//! Port of `bin/lib/cli-process.js`: pid/instance files, liveness, cmdline
//! identity, process-tree termination.
//!
//! 中文说明：本模块移植自 `bin/lib/cli-process.js`，提供 CLI 侧的进程管理
//! 原语：pid 文件与 instance 元数据 JSON 的读写删除、进程存活探测、
//! 尽力而为的进程命令行读取与 OMPChamber 身份识别、按"先 TERM 后 KILL"
//! 的进程树终止，以及扫描 run 目录汇总所有存活实例。供 lifecycle、
//! serve、misc 等命令组复用。

use std::path::Path;

use super::paths;

/// 单个运行实例的元数据，以 JSON 持久化在 run 目录（文件名含端口）。
/// serve 守护进程启动成功后写入，stop/restart/status/logs 读取它来
/// 判断实例归属与连接参数。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceOptions {
    /// 实例监听的端口号，同时是 pid/instance 文件名的一部分。
    pub port: u16,
    /// 绑定地址；缺省时消费者按 localhost 处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// 启动方式："daemon"（后台）或 "foreground"（前台）。
    #[serde(default)]
    pub launch_mode: String,
    /// UI 密码原文；未设置或仅经环境变量注入时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_password: Option<String>,
    /// 是否存在 UI 密码（密码可能不落盘，用布尔单独保留这一事实）。
    #[serde(default)]
    pub has_ui_password: bool,
    /// 是否以 --api-only 启动（不服务浏览器 UI 静态资源）。
    #[serde(default)]
    pub api_only: bool,
    /// 启动时刻的 Unix 毫秒时间戳（与 JS Date.now() 对齐）。
    #[serde(default)]
    pub started_at: f64,
}

/// 读取 pid 文件并解析为进程号；文件缺失、读取失败或内容不是合法 u32
/// 时返回 None（调用方据此视为"无运行实例"）。
pub fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.trim().parse::<u32>().ok())
}

/// 将 pid 以十进制文本写入文件。写入失败仅告警不报错；Unix 上将文件
/// 权限收紧为 0600，防止其他用户读取。
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

/// 删除 pid 文件；失败静默忽略（进程可能已退出，文件可能已被清理）。
pub fn remove_pid_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// 读取并反序列化 instance JSON；任何 IO 或解析失败都返回 None。
pub fn read_instance_options(path: &Path) -> Option<InstanceOptions> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

/// 将实例元数据以 pretty JSON 写盘；失败仅告警。Unix 上权限设为 0600
/// （内容含 UI 密码原文）。
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

/// 删除 instance 文件；失败静默忽略。
pub fn remove_instance_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Liveness only — "is *some* process alive with this PID".
/// 仅判断存活，不校验进程身份——身份校验需配合 read_process_cmdline
/// 与 is_ompchamber_cmdline 使用。
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
/// 读取失败或输出为空即返回 None，调用方按"身份未知"处理。
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
/// 大小写不敏感匹配，用于避免误杀恰好复用了该 pid 的无关进程。
pub fn is_ompchamber_cmdline(cmdline: &str) -> bool {
    let lower = cmdline.to_ascii_lowercase();
    (lower.contains("ompchamber") || lower.contains("ompchamber-server"))
        && (lower.contains("server/index.js")
            || lower.contains("ompchamber-server")
            || lower.contains("serve"))
}

/// `terminateProcessTree`: TERM to the group, bounded wait, then KILL.
/// 同步阻塞执行，仅供 CLI 命令路径使用，不得在 async 上下文调用。
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
/// 目录不存在或没有匹配文件时返回空表。
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
