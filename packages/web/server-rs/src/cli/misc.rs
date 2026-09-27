//! CLI command group: status, logs, schedule, session, models, projects,
//! control (`bin/lib/commands-status.js`, `commands-logs.js`,
//! `commands-schedule.js`, `commands-session.js`, `commands-models.js`,
//! `commands-projects.js`, `cli-control.js`, `cli-goal.js`,
//! `cli-api-target.js`) plus the lifecycle-discovery and HTTP-client pieces
//! they need from `cli-lifecycle.js` / `cli-http.js` / `cli-log-files.js`.
//! 中文说明：本模块是 CLI 杂项命令组的 Rust 移植，聚合了 status / logs / schedule /
//! session / models / projects / control 六组子命令，以及它们依赖的 async→sync 桥接、
//! 主机与 URL 解析、JSON 值辅助、clack 风格输出、系统信息探测、实例发现、
//! 日志跟随和带 UI 密码自动重试的 control HTTP 请求封装。

use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(test)]
use super::USAGE_ERROR;
use super::args::{self, Options, Parsed};
use super::paths;
use super::process;
use super::{CliError, GENERAL_ERROR, OutputMode, print_json};

// ── async bridge ───────────────────────────────────────────────────────
// The dispatch in `mod.rs` calls this group synchronously; the HTTP helpers
// below are async (plain reqwest, no blocking feature in the crate), so each
// sync entry point drives its futures through this bridge. Inside the
// multi-thread CLI runtime `block_in_place` keeps the reactor alive; outside
// one (unit tests) a fresh runtime is spun up.

/// 同步驱动一个 future 到完成：`mod.rs` 的分发层是同步调用，而本文件的 HTTP 辅助
/// 基于异步 reqwest。已处于多线程 tokio 运行时内时用 `block_in_place` 保住 reactor；
/// 否则（如单元测试环境）临时新建一个 runtime。同步入口点借此复用 async 实现。
fn block_on<F: Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => tokio::runtime::Runtime::new()
            .expect("failed to build tokio runtime for CLI request")
            .block_on(future),
    }
}

/// 进程级共享的 reqwest 客户端（LazyLock 惰性初始化），复用连接池；
/// 所有 CLI 的探测与 control 请求都走这一个实例。
fn http_client() -> &'static reqwest::Client {
    /// 惰性单例客户端；首次调用时构造，之后复用连接池。
    static CLIENT: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(reqwest::Client::new);
    &CLIENT
}

// ── host / URL helpers (`cli-network.js` subset) ───────────────────────

/// 规整探测主机：去除首尾空白，空白串视为未指定（返回 None）。
fn normalize_probe_host(host: Option<&str>) -> Option<String> {
    host.map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string)
}

/// 判断是否通配绑定地址（0.0.0.0 / :: / [::]）——不能作为具体连接目标。
fn is_wildcard_probe_host(host: Option<&str>) -> bool {
    matches!(
        normalize_probe_host(host).as_deref(),
        Some("0.0.0.0") | Some("::") | Some("[::]")
    )
}

/// 判断是否回环地址（127.0.0.1 / localhost / ::1 / [::1]）。
fn is_loopback_probe_host(host: Option<&str>) -> bool {
    matches!(
        normalize_probe_host(host).as_deref(),
        Some("127.0.0.1") | Some("localhost") | Some("::1") | Some("[::1]")
    )
}

/// 判断是否"具体的权威主机"：非空且既非通配也非回环（如局域网 IP）。
/// 该标记决定探测回退时是否强制 PID 匹配。
fn is_concrete_probe_host(host: Option<&str>) -> bool {
    let normalized = normalize_probe_host(host);
    normalized.is_some() && !is_wildcard_probe_host(host) && !is_loopback_probe_host(host)
}

/// `resolveApiHost`: option > OMPCHAMBER_HOST env > 127.0.0.1, with wildcard
/// and bracket normalization.
/// 解析实际连接用的 API 主机：显式参数 > OMPCHAMBER_HOST 环境变量 > 127.0.0.1；
/// 通配地址折叠为回环，多余的方括号（如 `[::1]`）会被剥掉。
fn resolve_api_host(host_override: Option<&str>) -> String {
    let configured = normalize_probe_host(host_override)
        .or_else(|| {
            std::env::var("OMPCHAMBER_HOST")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
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

/// 把主机格式化为 URL 片段：含冒号的 IPv6 地址加方括号，其余原样返回。
fn format_host_for_url(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// 构造 `http://<host>:<port><endpoint>` 形式的本地 URL；
/// endpoint 缺前导斜杠时自动补上，主机经 resolve_api_host 规整。
fn build_local_url(port: u16, endpoint: &str, host_override: Option<&str>) -> String {
    let host = format_host_for_url(&resolve_api_host(host_override));
    let path = if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    };
    format!("http://{host}:{port}{path}")
}

// ── JSON value helpers ─────────────────────────────────────────────────

/// 静态 JSON null，作为缺失字段的借用占位，让 `field` 无需分配即可返回默认值。
static JSON_NULL: serde_json::Value = serde_json::Value::Null;

/// `value?.key` with a JSON null stand-in for missing fields.
/// 安全取字段：等价 JS 的 `value?.key`——键缺失时返回静态 null 引用而非 panic。
fn field<'a>(value: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    value.get(key).unwrap_or(&JSON_NULL)
}

/// 读取字符串字段；字段缺失或不是字符串时返回 None。
fn value_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

/// trim 后非空才返回 Some，用于把"空白即未指定"的 CLI 选项规整掉。
fn as_non_empty_str(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// JS number interpolation (`${n}`): integral values print without a
/// fractional part.
/// 模拟 JS 数字插值：整数值（|n| < 1e21）不打印小数部分，与旧 JS 输出逐字一致。
fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// f64 转 JSON 数值；非有限值（NaN/Infinity）无法表示时落回 JSON null。
fn json_number(value: f64) -> serde_json::Value {
    serde_json::Number::from_f64(value)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

/// `Number(options.timeout)`: "" → 0, non-numeric → null (JS NaN).
/// 模拟 JS `Number(options.timeout)`：空串解析为 0，非数字解析为 null（对应 JS NaN）。
fn number_option(raw: Option<&str>) -> Option<serde_json::Value> {
    let raw = raw?;
    match raw.trim().parse::<f64>() {
        Ok(parsed) => Some(json_number(parsed)),
        Err(_) if raw.trim().is_empty() => Some(json_number(0.0)),
        Err(_) => Some(serde_json::Value::Null),
    }
}

/// `new Date(ms).toISOString()` (UTC, millisecond precision).
/// 毫秒级 Unix 时间戳转 ISO 8601（UTC、毫秒精度），等价 `new Date(ms).toISOString()`；
/// 非有限输入返回 None。纯整数运算，不依赖系统时区。
fn iso8601_from_epoch_ms(ms: f64) -> Option<String> {
    if !ms.is_finite() {
        return None;
    }
    let total_secs = (ms / 1000.0).floor() as i64;
    let millis = (ms - (total_secs as f64) * 1000.0) as i64;
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    ))
}

/// Howard Hinnant's `civil_from_days`.
/// Howard Hinnant 的"天数 → 公历日期"算法：以 1970-01-01 为纪元换算年月日，
/// 对负数（1970 年之前）同样成立。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ── clack output adapters (`cli-output.js` subset) ─────────────────────
// clack-compatible rendering (byte-matches @clack/prompts 1.7 captured
// output — see cli::ui for the glyph/ANSI contract).
/// JS printJson injects `status: "ok"` FIRST when absent (cli-output.js).
/// JS printJson 的兼容行为：对象缺少 `status` 键时把 `"ok"` 插入到第一位，
/// 保持与旧 CLI JSON 输出的键序一致。
fn status_first_json(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(map) = value.as_object_mut() {
        if !map.contains_key("status") {
            map.shift_insert(0, "status".to_string(), serde_json::json!("ok"));
        }
    }
    value
}

/// clack 风格输出首帧（intro 框线），转发到 cli::ui 渲染。
fn clack_intro(title: &str) {
    super::ui::intro(title);
}

/// clack 风格输出收尾帧（outro 框线）。
fn clack_outro(text: &str) {
    super::ui::outro(text);
}

/// 输出一条带状态图标（success/warning/info）的日志块，detail 并入框内。
fn log_status(status: &str, message: &str, detail: Option<&str>) {
    // ui::log_status joins detail into the block with the bar prefix.
    super::ui::log_status(status, message, detail);
}

// ── system info / health probes (`cli-http.js` subset) ─────────────────

/// `/api/system/info` 的最小化响应模型：仅保留实例识别所需的 runtime 标识与 PID。
#[derive(Debug, Clone)]
struct SystemInfo {
    /// runtime 标识（"cli" / "desktop" 等）；空串表示响应无效。
    runtime: String,
    /// 服务端上报的进程号；缺失或非有限数时为 None。
    pid: Option<u32>,
}

/// 判定探测结果是否为有效的 OMPChamber 信息：Some 且 runtime 非空。
fn has_ompchamber_runtime_info(info: &Option<SystemInfo>) -> bool {
    info.as_ref().is_some_and(|info| !info.runtime.is_empty())
}

/// 异步探测指定端口的 `/api/system/info`：1.5s 超时；非 2xx、JSON 解析失败或
/// runtime 为空均返回 None。port 为 0 直接跳过（视为无效端口）。
async fn fetch_system_info_from_port_async(
    port: u16,
    host_override: Option<&str>,
) -> Option<SystemInfo> {
    if port == 0 {
        return None;
    }
    let url = build_local_url(port, "/api/system/info", host_override);
    let response = http_client()
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(Duration::from_millis(1500))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    let runtime = body.get("runtime")?.as_str()?.to_string();
    if runtime.is_empty() {
        return None;
    }
    let pid = body
        .get("pid")
        .and_then(|v| v.as_f64())
        .filter(|pid| pid.is_finite())
        .map(|pid| pid as u32);
    Some(SystemInfo { runtime, pid })
}

/// `fetch_system_info_from_port_async` 的同步包装（经 block_on 桥接）。
fn fetch_system_info_from_port(port: u16, host_override: Option<&str>) -> Option<SystemInfo> {
    block_on(fetch_system_info_from_port_async(port, host_override))
}

/// 异步健康检查：GET /health，2xx 视为就绪。timeout_ms 为 0 时按 1000ms 计；
/// port 为 0 或任何网络错误都返回 false。
async fn is_server_health_ready_async(port: u16, timeout_ms: u64) -> bool {
    if port == 0 {
        return false;
    }
    let request_timeout = if timeout_ms > 0 { timeout_ms } else { 1000 };
    let url = build_local_url(port, "/health", None);
    let Ok(response) = http_client()
        .get(&url)
        .header(reqwest::header::ACCEPT, "text/plain")
        .timeout(Duration::from_millis(request_timeout))
        .send()
        .await
    else {
        return false;
    };
    response.status().is_success()
}

/// `is_server_health_ready_async` 的同步包装。
fn is_server_health_ready(port: u16, timeout_ms: u64) -> bool {
    block_on(is_server_health_ready_async(port, timeout_ms))
}

// ── process identity (`cli-process.js` `getOmpchamberProcessState`) ────

/// PID 归属判定结果：区分"pid 文件指向的进程"是否仍是本 CLI 启动的 OMPChamber。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessState {
    /// 进程已不存在——pid 文件过期，应当清理。
    Dead,
    /// 进程活着但读不到命令行（权限或平台限制），无法判定归属。
    Unknown,
    /// 命令行匹配 OMPChamber 特征，确认是自己的实例。
    Matched,
    /// 进程已被其它程序复用，pid 文件失效。
    Mismatched,
}

/// 判定 pid 的当前状态：先查存活，再读命令行比对 OMPChamber 特征。
fn get_ompchamber_process_state(pid: u32) -> ProcessState {
    if !process::is_process_running(pid) {
        return ProcessState::Dead;
    }
    match process::read_process_cmdline(pid) {
        None => ProcessState::Unknown,
        Some(cmdline) => {
            if process::is_ompchamber_cmdline(&cmdline) {
                ProcessState::Matched
            } else {
                ProcessState::Mismatched
            }
        }
    }
}

/// 文件 mtime 的毫秒 Unix 时间戳；任何失败（不存在、无权限）返回 0。
/// 用作实例"新旧"比较的兜底排序键。
fn file_mtime_ms(path: &Path) -> f64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or_default()
}

// ── instance discovery (`cli-lifecycle.js` subset) ─────────────────────

/// 一次发现得到的运行实例：来源可以是注册表 pid 文件 + 探测确认（registry+probe），
/// 也可以是仅凭端口探测（probe）。status / logs / 生命周期命令共用该模型。
#[derive(Debug, Clone)]
struct DiscoveredInstance {
    /// 实例监听端口（也是注册表文件名的一部分）。
    port: u16,
    /// 实例进程号；探测未取到时回退注册表 pid（仅当进程校验匹配）。
    pid: Option<u32>,
    /// 注册表实例元数据文件路径（instance_file_path(port)）。
    instance_file_path: PathBuf,
    /// pid 文件 mtime（毫秒）；纯探测来源时为 0。
    mtime_ms: f64,
    /// 实例元数据记录的启动时间戳；缺失或非法时为 0.0。
    started_at: f64,
    /// 启动模式："foreground" 或 "daemon"。
    launch_mode: String,
    /// 服务端上报的 runtime 标识（"cli" / "desktop" 等）。
    runtime: String,
    /// 发现来源标签："registry+probe" 或 "probe"（status 据此区分 cli / unmanaged）。
    source: &'static str,
}

/// `getSystemInfoProbeHosts`: candidate hosts in probe order, each flagging
/// whether a PID match is required to accept the answer.
/// 生成探测主机序列：显式主机优先且不要求 PID 匹配；存在具体权威主机时，
/// 兜底的默认 / 127.0.0.1 探测必须 PID 匹配才能采信。按 resolve_api_host 归一化去重。
fn get_system_info_probe_hosts(hosts: &[Option<String>]) -> Vec<(Option<String>, bool)> {
    /// 归一化并去重地压入一个候选主机（按 resolve_api_host 结果作键比较）。
    fn push_probe_host(
        out: &mut Vec<(Option<String>, bool)>,
        host: Option<String>,
        requires_pid_match: bool,
    ) {
        let normalized = normalize_probe_host(host.as_deref());
        let key = resolve_api_host(normalized.as_deref());
        if !out
            .iter()
            .any(|(host, _)| resolve_api_host(host.as_deref()) == key)
        {
            out.push((normalized, requires_pid_match));
        }
    }

    let mut out = Vec::new();
    let has_concrete_authoritative_host = hosts
        .iter()
        .any(|host| is_concrete_probe_host(host.as_deref()));
    for host in hosts {
        if normalize_probe_host(host.as_deref()).is_some() {
            push_probe_host(&mut out, host.clone(), false);
        }
    }
    push_probe_host(&mut out, None, has_concrete_authoritative_host);
    push_probe_host(
        &mut out,
        Some("127.0.0.1".to_string()),
        has_concrete_authoritative_host,
    );
    out
}

/// 依序尝试候选主机，返回第一个有效的系统信息；标记 requires_pid_match 的候选
/// 只有当响应 PID 与期望一致时才被采信，避免把端口上别的服务器误认成目标实例。
fn fetch_system_info_from_port_candidates(
    port: u16,
    hosts: &[(Option<String>, bool)],
    expected_pid: u32,
) -> Option<SystemInfo> {
    for (host, requires_pid_match) in hosts {
        let info = fetch_system_info_from_port(port, host.as_deref());
        if has_ompchamber_runtime_info(&info) {
            if *requires_pid_match && info.as_ref().and_then(|info| info.pid) != Some(expected_pid)
            {
                continue;
            }
            return info;
        }
    }
    None
}

/// `discoverRunningInstances`: registry pid files + live /api/system/info
/// confirmation, with stale-file cleanup.
/// 遍历 run 目录下的 `ompchamber-<port>.pid` 注册表文件：读 pid、校验进程归属、
/// 再经 /api/system/info 确认存活；确认失败的死文件 / 错配文件就地清理，
/// desktop runtime 不算 CLI 实例（同样清理），结果按端口升序返回。
fn discover_running_instances(options: &Options) -> Vec<DiscoveredInstance> {
    let mut instances = Vec::new();
    let run_dir = paths::run_dir();
    let Ok(entries) = std::fs::read_dir(&run_dir) else {
        return instances;
    };
    let mut pid_files: Vec<(u16, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with("ompchamber-") && name.ends_with(".pid"))
        .filter_map(|name| {
            let port = name
                .strip_prefix("ompchamber-")?
                .strip_suffix(".pid")?
                .parse::<u16>()
                .ok()?;
            Some((port, run_dir.join(&name)))
        })
        .collect();
    pid_files.sort();

    for (port, pid_file_path) in pid_files {
        let Some(pid) = process::read_pid_file(&pid_file_path) else {
            process::remove_pid_file(&pid_file_path);
            process::remove_instance_file(&paths::instance_file_path(port));
            continue;
        };
        let instance_file_path = paths::instance_file_path(port);
        let stored_options = process::read_instance_options(&instance_file_path);
        let process_state = get_ompchamber_process_state(pid);
        if process_state == ProcessState::Dead {
            process::remove_pid_file(&pid_file_path);
            process::remove_instance_file(&instance_file_path);
            continue;
        }

        let hosts = get_system_info_probe_hosts(&[
            stored_options
                .as_ref()
                .and_then(|options| options.host.clone()),
            options.host.clone(),
        ]);
        let info = fetch_system_info_from_port_candidates(port, &hosts, pid);
        if !has_ompchamber_runtime_info(&info) {
            if process_state == ProcessState::Mismatched {
                process::remove_pid_file(&pid_file_path);
                process::remove_instance_file(&instance_file_path);
            }
            continue;
        }
        let info = info.expect("runtime info checked above");

        if info.runtime == "desktop" {
            process::remove_pid_file(&pid_file_path);
            process::remove_instance_file(&instance_file_path);
            continue;
        }

        let live_pid = info.pid.filter(|pid| *pid > 0);
        let mtime_ms = file_mtime_ms(&pid_file_path);
        instances.push(DiscoveredInstance {
            port,
            pid: live_pid.or_else(|| (process_state == ProcessState::Matched).then_some(pid)),
            mtime_ms,
            started_at: stored_options
                .as_ref()
                .map(|options| options.started_at)
                .filter(|started_at| started_at.is_finite())
                .unwrap_or_default(),
            launch_mode: if stored_options
                .as_ref()
                .is_some_and(|options| options.launch_mode == "foreground")
            {
                "foreground".to_string()
            } else {
                "daemon".to_string()
            },
            runtime: info.runtime,
            source: "registry+probe",
            instance_file_path,
        });
    }
    instances.sort_by_key(|instance| instance.port);
    instances
}

/// 判定探测到的 desktop runtime 是否就是设置文件登记的那个端口；
/// 未登记 desktopLocalPort 时视为匹配（唯一的 desktop 即目标）。
fn is_desktop_runtime_for_port(info: &SystemInfo, port: u16) -> bool {
    if info.runtime != "desktop" {
        return false;
    }
    let desktop_port = paths::read_desktop_local_port_from_settings();
    desktop_port.is_none() || desktop_port == Some(port)
}

/// 用纯探测结果构造实例记录（source = "probe"）：无注册表元数据，
/// mtime / started_at 记 0，launch_mode 视为 daemon。
fn create_live_port_instance(port: u16, info: Option<SystemInfo>) -> Option<DiscoveredInstance> {
    if !has_ompchamber_runtime_info(&info) {
        return None;
    }
    let info = info.expect("runtime info checked above");
    Some(DiscoveredInstance {
        port,
        pid: info.pid,
        instance_file_path: paths::instance_file_path(port),
        mtime_ms: 0.0,
        started_at: 0.0,
        launch_mode: "daemon".to_string(),
        runtime: info.runtime,
        source: "probe",
    })
}

/// `discoverOMPChamberInstanceOnPort`.
/// 定位指定端口的实例：优先复用已发现的运行实例；否则直接探测该端口，
/// 若探测到 desktop runtime 但端口与设置不符则拒绝（返回 None）。
fn discover_instance_on_port(
    port: u16,
    options: &Options,
    running_instances: &[DiscoveredInstance],
) -> Option<DiscoveredInstance> {
    if port == 0 {
        return None;
    }
    if let Some(found) = running_instances.iter().find(|entry| entry.port == port) {
        return Some(found.clone());
    }
    let info = fetch_system_info_from_port(port, options.host.as_deref());
    if let Some(info) = &info {
        if info.runtime == "desktop" && !is_desktop_runtime_for_port(info, port) {
            return None;
        }
    }
    create_live_port_instance(port, info)
}

/// `discoverLifecycleInstances`.
/// 生命周期实例全集：默认返回全部注册表实例；用户显式指定端口时只返回该端口
/// （注册表命中优先，未命中则做单端口探测）。
fn discover_lifecycle_instances(options: &Options) -> Vec<DiscoveredInstance> {
    let running_instances = discover_running_instances(options);
    if !options.explicit_port {
        return running_instances;
    }
    if let Some(found) = running_instances
        .iter()
        .find(|entry| entry.port == options.port.unwrap_or_default())
    {
        return vec![found.clone()];
    }
    discover_instance_on_port(
        options.port.unwrap_or_default(),
        options,
        &running_instances,
    )
    .into_iter()
    .collect()
}

/// `discoverDesktopInstance`: (port, pid).
/// 发现 desktop 应用实例：读设置里的 desktopLocalPort 并探测确认 runtime 为
/// "desktop"；返回 (port, pid)。
fn discover_desktop_instance() -> Option<(u16, Option<u32>)> {
    let port = paths::read_desktop_local_port_from_settings()?;
    let info = fetch_system_info_from_port(port, None)?;
    if info.runtime != "desktop" {
        return None;
    }
    Some((port, info.pid))
}

/// `getLatestInstance`: newest startedAt, then pid-file mtime, then port.
/// 挑"最新"实例：先比 startedAt，其次 pid 文件 mtime，最后端口大小决胜负。
fn get_latest_instance(instances: &[DiscoveredInstance]) -> Option<&DiscoveredInstance> {
    instances.iter().max_by(|a, b| {
        let started = a
            .started_at
            .partial_cmp(&b.started_at)
            .unwrap_or(std::cmp::Ordering::Equal);
        if started != std::cmp::Ordering::Equal {
            return started;
        }
        let mtime = a
            .mtime_ms
            .partial_cmp(&b.mtime_ms)
            .unwrap_or(std::cmp::Ordering::Equal);
        if mtime != std::cmp::Ordering::Equal {
            return mtime;
        }
        a.port.cmp(&b.port)
    })
}

// ── status (`commands-status.js`) ──────────────────────────────────────

/// status 命令的展示条目：从发现结果与注册表元数据聚合而来。
#[derive(Debug, Clone, PartialEq)]
struct StatusEntry {
    /// 展示用 runtime 标签："cli" / "unmanaged" / "desktop"。
    runtime: String,
    /// 实例端口。
    port: u16,
    /// 进程号；探测未取到为 None。
    pid: Option<u32>,
    /// 启动模式；desktop 条目为 None。
    launch_mode: Option<String>,
    /// 是否受 UI 密码保护；unmanaged（无注册表元数据）实例为 None（unknown）。
    password_protected: Option<bool>,
}

/// 密码保护状态的人类可读标签：yes / no / unknown。
fn password_protection_label(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

/// 将条目序列化为 JSON 输出（驼峰键名与旧 JS CLI 对齐；缺失字段用 null）。
fn status_entry_json(entry: &StatusEntry) -> serde_json::Value {
    serde_json::json!({
        "runtime": entry.runtime,
        "port": entry.port,
        "pid": entry.pid.map(serde_json::Value::from).unwrap_or(serde_json::Value::Null),
        "launchMode": entry
            .launch_mode
            .clone()
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
        "passwordProtected": entry
            .password_protected
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null),
    })
}

/// The instance aggregation between discovery and presentation.
/// 发现与展示之间的一层聚合：注册表实例标注密码保护与 cli / unmanaged 来源；
/// 单独发现的 desktop 实例仅在无同端口 CLI 实例时追加（显式端口时只追加
/// 注册表里的 desktop 条目）。
fn collect_status_instances(options: &Options) -> Vec<StatusEntry> {
    let running_instances = discover_lifecycle_instances(options);
    let desktop_instance = if options.explicit_port {
        None
    } else {
        discover_desktop_instance()
    };

    let desktop_only = desktop_instance
        .filter(|(port, _)| !running_instances.iter().any(|entry| entry.port == *port))
        .map(|(port, pid)| StatusEntry {
            runtime: "desktop".to_string(),
            port,
            pid,
            launch_mode: None,
            password_protected: None,
        });

    let mut instances: Vec<StatusEntry> = running_instances
        .iter()
        .filter(|instance| instance.runtime != "desktop")
        .map(|instance| {
            let stored_options = process::read_instance_options(&instance.instance_file_path);
            let password_protected = stored_options.as_ref().is_some_and(|stored| {
                stored.has_ui_password
                    || stored
                        .ui_password
                        .as_deref()
                        .is_some_and(|password| !password.trim().is_empty())
            });
            StatusEntry {
                runtime: if instance.source == "probe" {
                    "unmanaged".to_string()
                } else {
                    "cli".to_string()
                },
                port: instance.port,
                pid: instance.pid,
                launch_mode: Some(
                    if instance.launch_mode.is_empty() {
                        "daemon"
                    } else {
                        instance.launch_mode.as_str()
                    }
                    .to_string(),
                ),
                password_protected: (instance.source != "probe").then_some(password_protected),
            }
        })
        .collect();

    if let Some(entry) = desktop_only {
        instances.push(entry);
    }

    if options.explicit_port {
        if let Some(explicit_desktop) = running_instances
            .iter()
            .find(|entry| entry.runtime == "desktop")
        {
            instances.push(StatusEntry {
                runtime: "desktop".to_string(),
                port: explicit_desktop.port,
                pid: explicit_desktop.pid,
                launch_mode: None,
                password_protected: None,
            });
        }
    }

    instances
}

/// `ompchamber status`：列出运行中的 OMPChamber runtime。
/// JSON 模式输出 state / runningCount / instances；Quiet 每实例打印
/// `port <p> mode:<m> pass:<x>` 单行；Human 模式用 clack 框线逐实例展示
/// 端口、PID、启动模式与密码保护状态。
pub fn status_command(_parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let instances = collect_status_instances(&options);
    let running_count = instances.len();

    match OutputMode::from_options(&options) {
        OutputMode::Json => {
            print_json(&status_first_json(serde_json::json!({
                "state": if running_count > 0 { "running" } else { "stopped" },
                "runningCount": running_count,
                "instances": instances
                    .iter()
                    .map(status_entry_json)
                    .collect::<Vec<_>>(),
            })));
        }
        OutputMode::Quiet if running_count == 0 => println!("stopped"),
        OutputMode::Quiet => {
            for instance in &instances {
                println!(
                    "port {} mode:{} pass:{}",
                    instance.port,
                    instance
                        .launch_mode
                        .clone()
                        .unwrap_or_else(|| "n/a".to_string()),
                    password_protection_label(instance.password_protected),
                );
            }
        }
        OutputMode::Human => {
            clack_intro("OMPChamber Status");
            if running_count == 0 {
                log_status("warning", "stopped", None);
                clack_outro("no running instances");
                return Ok(());
            }
            for instance in &instances {
                let pid_suffix = instance
                    .pid
                    .map(|pid| format!(" (PID: {pid})"))
                    .unwrap_or_default();
                let protection_detail = format!(
                    "password: {}",
                    password_protection_label(instance.password_protected)
                );
                let detail = match &instance.launch_mode {
                    Some(launch_mode) => format!("mode: {launch_mode}; {protection_detail}"),
                    None => protection_detail,
                };
                if instance.runtime == "desktop" {
                    log_status(
                        "info",
                        &format!("desktop app on port {}{}", instance.port, pid_suffix),
                        Some(&detail),
                    );
                } else {
                    log_status(
                        "success",
                        &format!("port {}{}", instance.port, pid_suffix),
                        Some(&detail),
                    );
                }
            }
            clack_outro(&format!("{running_count} running runtime(s)"));
        }
    }
    Ok(())
}

// ── log files (`cli-log-files.js` subset) ──────────────────────────────

/// `--lines` 未指定时的默认尾部行数。
const DEFAULT_TAIL_LINES: u32 = 200;

/// 读取文件最后 N 行（CRLF 归一、去掉末尾空行）；读取失败返回空表。
/// 一次性整读而非流式——CLI 场景的日志文件规模可控。
fn read_tail_lines(file_path: &Path, line_count: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(file_path) else {
        return Vec::new();
    };
    let raw = String::from_utf8_lossy(&raw);
    let mut lines: Vec<String> = raw
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    if lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let start = lines.len().saturating_sub(line_count as usize);
    lines.split_off(start)
}

/// Incremental state for `followFile`.
/// `followFile` 的增量跟随状态：记录已消费到的文件偏移与跨读取块的半行残渣。
struct FollowState {
    /// 被跟随的日志文件路径。
    path: PathBuf,
    /// 已读取到的字节偏移；文件被截断（变短）时重置为 0。
    position: u64,
    /// 上次读取留下的不含换行符的尾部片段，拼接到下次读取内容之前。
    remainder: String,
}

/// FollowState 的构造与初始偏移设定。
impl FollowState {
    /// 从文件当前大小起开始跟随（跳过既有内容），只输出后续追加的行。
    fn new(path: PathBuf) -> Self {
        let position = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        Self {
            path,
            position,
            remainder: String::new(),
        }
    }
}

/// One `followFile` interval tick: emits newly appended complete lines.
/// 跟随一跳：读出自上次偏移后追加的字节，仅产出已完整的行；文件变小则从头重读。
/// 返回空表表示无新内容（或文件暂时打不开）。
fn follow_poll(state: &mut FollowState) -> Vec<String> {
    let mut lines = Vec::new();
    let Ok(stats) = std::fs::metadata(&state.path) else {
        return lines;
    };
    let size = stats.len();
    if size < state.position {
        state.position = 0;
    }
    if size == state.position {
        return lines;
    }

    let Ok(mut file) = std::fs::File::open(&state.path) else {
        return lines;
    };
    use std::io::{Read, Seek, SeekFrom};
    if file.seek(SeekFrom::Start(state.position)).is_err() {
        return lines;
    }
    let length = (size - state.position) as usize;
    let mut buffer = vec![0u8; length];
    if file.read_exact(&mut buffer).is_err() {
        return lines;
    }
    state.position = size;

    let chunk = format!("{}{}", state.remainder, String::from_utf8_lossy(&buffer));
    let mut parts: Vec<String> = chunk
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect();
    state.remainder = parts.pop().unwrap_or_default();
    lines.append(&mut parts);
    lines
}

/// 多目标日志跟随主循环：每 400ms 轮询各目标日志的新增行（可带 `[port]` 前缀），
/// unix 上监听 SIGINT / SIGTERM 以干净退出。整个循环经 block_on 驱动。
fn follow_targets(targets: &[DiscoveredInstance], should_prefix_lines: bool) {
    block_on(async move {
        let mut states: Vec<(u16, FollowState)> = targets
            .iter()
            .map(|target| {
                (
                    target.port,
                    FollowState::new(paths::log_file_path(&target.port.to_string())),
                )
            })
            .collect();
        #[cfg(unix)]
        let mut terminate = Box::pin(async {
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                    .expect("failed to install SIGINT handler");
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {},
            }
        });
        loop {
            #[cfg(unix)]
            {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(400)) => {}
                    _ = &mut terminate => break,
                }
            }
            #[cfg(not(unix))]
            tokio::time::sleep(Duration::from_millis(400)).await;
            for (port, state) in &mut states {
                for line in follow_poll(state) {
                    if should_prefix_lines {
                        println!("[{port}] {line}");
                    } else {
                        println!("{line}");
                    }
                }
            }
        }
    });
}

// ── logs (`commands-logs.js`) ──────────────────────────────────────────

/// 解析 logs 命令的目标实例：--all 取全部；显式端口精确匹配（找不到报错）；
/// 默认取"最新"实例（human 模式下提示所选端口）；无实例时统一报错。
fn resolve_log_targets(options: &Options) -> Result<Vec<DiscoveredInstance>, CliError> {
    let running = discover_running_instances(options);
    if options.all {
        if running.is_empty() {
            return Err(CliError::new(
                "No running OMPChamber instance found.",
                GENERAL_ERROR,
            ));
        }
        return Ok(running);
    }
    if options.explicit_port {
        let port = options.port.unwrap_or_default();
        let found = running.iter().find(|entry| entry.port == port);
        if let Some(found) = found {
            return Ok(vec![found.clone()]);
        }
        return Err(CliError::new(
            format!("No running OMPChamber instance found on port {port}."),
            GENERAL_ERROR,
        ));
    }
    let latest = get_latest_instance(&running);
    let Some(latest) = latest else {
        return Err(CliError::new(
            "No running OMPChamber instance found.",
            GENERAL_ERROR,
        ));
    };
    if !options.json && !options.quiet {
        log_status(
            "info",
            &format!(
                "no port specified; using latest started instance on port {}",
                latest.port
            ),
            None,
        );
    }
    Ok(vec![latest.clone()])
}

/// logs 的 JSON 输出：每个目标一条 {port, logPath, lines}（不注入 status:first）。
fn log_entries_json(targets: &[DiscoveredInstance], line_count: u32) -> serde_json::Value {
    serde_json::json!({
        "entries": targets
            .iter()
            .map(|target| {
                let log_path = paths::log_file_path(&target.port.to_string());
                serde_json::json!({
                    "port": target.port,
                    "logPath": log_path.to_string_lossy(),
                    "lines": read_tail_lines(&log_path, line_count),
                })
            })
            .collect::<Vec<_>>(),
    })
}

/// `ompchamber logs`：打印目标实例日志尾部，默认跟随新增行。
/// --json 必须搭配 --no-follow（保证确定性输出）；human 模式带 clack 框线，
/// 多目标或非交互模式给每行加 `[port]` 前缀。
pub fn logs_command(_parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let show_frames = !options.json && !options.quiet;
    let should_prefix_lines = options.all || !show_frames;
    let targets = resolve_log_targets(&options)?;

    let follow = options.follow.unwrap_or(true);
    let line_count = options.lines.unwrap_or(DEFAULT_TAIL_LINES);

    if options.json {
        if follow {
            return Err(CliError::new(
                "`ompchamber logs --json` requires `--no-follow` for deterministic JSON output.",
                GENERAL_ERROR,
            ));
        }
        print_json(&log_entries_json(&targets, line_count)); // logs JSON: JS emits entries[] (no status injection)
        return Ok(());
    }

    if show_frames {
        clack_intro("OMPChamber Logs");
    }

    for target in &targets {
        let log_path = paths::log_file_path(&target.port.to_string());
        let lines = read_tail_lines(&log_path, line_count);
        if show_frames {
            log_status(
                "info",
                &format!("port {}", target.port),
                Some(&log_path.to_string_lossy()),
            );
        }
        for line in lines {
            if should_prefix_lines {
                println!("[{}] {}", target.port, line);
            } else {
                println!("{line}");
            }
        }
    }

    if show_frames {
        clack_outro(if follow {
            "following (Ctrl+C to stop)"
        } else {
            "tail complete"
        });
    }

    if !follow {
        return Ok(());
    }
    follow_targets(&targets, should_prefix_lines);
    Ok(())
}

// ── api target resolution (`cli-api-target.js`) ────────────────────────

/// 解析 control 请求的目标端口（cli-api-target.js）：显式 --port 直接生效；
/// 否则优先 desktop 实例，其次唯一存活实例；多实例时若默认端口健康则用默认端口，
/// 否则报 usage 错；无实例且默认端口不健康则提示启动 `ompchamber serve`。
fn resolve_target_port(options: &Options) -> Result<u16, CliError> {
    if options.explicit_port && options.port.is_some_and(|port| port > 0) {
        return Ok(options.port.unwrap_or_default());
    }

    let desktop_instance = discover_desktop_instance();
    let lifecycle_instances = discover_lifecycle_instances(options);

    if let Some((port, _)) = desktop_instance {
        return Ok(port);
    }

    if let Some(entry) = lifecycle_instances
        .iter()
        .find(|entry| entry.runtime == "desktop" && entry.port > 0)
    {
        return Ok(entry.port);
    }

    let mut seen = std::collections::HashSet::new();
    let ports: Vec<u16> = lifecycle_instances
        .iter()
        .map(|entry| entry.port)
        .filter(|port| *port > 0)
        .filter(|port| seen.insert(*port))
        .collect();

    if ports.len() == 1 {
        return Ok(ports[0]);
    }

    if ports.len() > 1 {
        if ports.contains(&args::DEFAULT_PORT) && is_server_health_ready(args::DEFAULT_PORT, 1200) {
            return Ok(args::DEFAULT_PORT);
        }
        let joined = ports
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(CliError::usage(format!(
            "Multiple OMPChamber instances are running (ports: {joined}). Choose one with --port <port>."
        )));
    }

    if is_server_health_ready(args::DEFAULT_PORT, 1200) {
        return Ok(args::DEFAULT_PORT);
    }

    Err(CliError::new(
        "No running OMPChamber server found. Start one with `ompchamber serve`, or pass --port <port>.",
        GENERAL_ERROR,
    ))
}

// ── authenticated control requests (`cli-control.js`, `cli-http.js`) ───

/// control 请求的原始响应封装：状态码、是否成功、解析后的 JSON 体。
struct JsonResponse {
    /// HTTP 状态码。
    status: u16,
    /// 是否 2xx 成功。
    ok: bool,
    /// 解析后的响应体；解析失败为 Null。
    body: serde_json::Value,
}

/// 从响应的 Set-Cookie 头里提取 `oc_ui_session=...` cookie（含值，不含属性）。
fn extract_ui_session_cookie(response: &reqwest::Response) -> Option<String> {
    for value in response.headers().get_all(reqwest::header::SET_COOKIE) {
        let set_cookie = value.to_str().ok()?;
        let start = set_cookie.find("oc_ui_session=")?;
        let tail = &set_cookie[start..];
        let cookie = tail.split(';').next()?;
        if !cookie.is_empty() {
            return Some(cookie.to_string());
        }
    }
    None
}

/// 解析 UI 密码：显式 --ui-password（非空）优先，其次该端口实例元数据里存的
/// 密码，最后回退 options 中的非空密码；都拿不到则为 None。
fn resolve_ui_password_for_port(port: u16, options: &Options) -> Option<String> {
    if options.explicit_ui_password {
        if let Some(password) = options
            .ui_password
            .as_deref()
            .filter(|password| !password.trim().is_empty())
        {
            return Some(password.to_string());
        }
    }
    if let Some(instance) = process::read_instance_options(&paths::instance_file_path(port)) {
        if let Some(password) = instance
            .ui_password
            .as_deref()
            .filter(|password| !password.trim().is_empty())
        {
            return Some(password.to_string());
        }
    }
    options
        .ui_password
        .as_deref()
        .filter(|password| !password.trim().is_empty())
        .map(str::to_string)
}

/// 异步登录换取会话 cookie：POST /auth/session（JSON 密码），成功后从 Set-Cookie
/// 提取 oc_ui_session。无密码、空密码、请求失败或非 2xx 均返回 None。
async fn create_ui_session_cookie_async(
    port: u16,
    password: Option<&str>,
    timeout_ms: u64,
) -> Option<String> {
    let password = password?;
    if password.is_empty() {
        return None;
    }
    let url = build_local_url(port, "/auth/session", None);
    let Ok(response) = http_client()
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({ "password": password }))
        .timeout(Duration::from_millis(timeout_ms))
        .send()
        .await
    else {
        return None;
    };
    if !response.status().is_success() {
        return None;
    }
    extract_ui_session_cookie(&response)
}

/// 若目标端口正是设置登记的 desktop 本地端口，读取 client token 并生成
/// `Bearer <token>` Authorization 头；否则返回 None。
fn get_desktop_local_auth_header(port: u16) -> Option<String> {
    if paths::read_desktop_local_port_from_settings() != Some(port) {
        return None;
    }
    let token = paths::read_desktop_local_client_token_from_settings();
    (!token.is_empty()).then(|| format!("Bearer {token}"))
}

/// 把 reqwest 错误映射为 CLI 错误：超时与一般失败给出不同文案（均 GENERAL_ERROR）。
fn map_request_error(error: &reqwest::Error, endpoint: &str, timeout_ms: u64) -> CliError {
    if error.is_timeout() {
        CliError::new(
            format!("Request to {endpoint} timed out after {timeout_ms}ms."),
            GENERAL_ERROR,
        )
    } else {
        CliError::new(
            format!("Request to {endpoint} failed: {error}"),
            GENERAL_ERROR,
        )
    }
}

/// 异步发起 control POST：超时缺省 4s，可带 desktop bearer 头；请求体可选。
/// 收到 401 "UI authentication required" 时自动用解析出的密码换会话 cookie 并
/// 重试一次。返回原始响应（业务错误映射交给上层 request_control_action）。
async fn request_json_async(
    port: u16,
    endpoint: &str,
    body: Option<String>,
    timeout_ms: Option<u64>,
    options: &Options,
) -> Result<JsonResponse, CliError> {
    let effective_timeout_ms = timeout_ms.filter(|timeout| *timeout > 0).unwrap_or(4000);
    let url = build_local_url(port, endpoint, None);

    let build_request = |cookie: Option<&str>| {
        let mut request = http_client()
            .post(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(Duration::from_millis(effective_timeout_ms));
        if let Some(body) = &body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone());
        }
        if let Some(auth) = get_desktop_local_auth_header(port) {
            request = request.header(reqwest::header::AUTHORIZATION, auth);
        }
        if let Some(cookie) = cookie {
            request = request.header(reqwest::header::COOKIE, cookie);
        }
        request
    };

    let response = match build_request(None).send().await {
        Ok(response) => response,
        Err(error) => return Err(map_request_error(&error, endpoint, effective_timeout_ms)),
    };
    let status = response.status().as_u16();
    let ok = response.status().is_success();
    let parsed: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);

    if status == 401
        && parsed.get("error").and_then(|v| v.as_str()) == Some("UI authentication required")
    {
        let ui_password = resolve_ui_password_for_port(port, options);
        if let Some(cookie) =
            create_ui_session_cookie_async(port, ui_password.as_deref(), effective_timeout_ms).await
        {
            let retry = match build_request(Some(&cookie)).send().await {
                Ok(response) => response,
                Err(error) => {
                    return Err(map_request_error(&error, endpoint, effective_timeout_ms));
                }
            };
            let retry_status = retry.status().as_u16();
            let retry_ok = retry.status().is_success();
            let retry_body: serde_json::Value =
                retry.json().await.unwrap_or(serde_json::Value::Null);
            return Ok(JsonResponse {
                status: retry_status,
                ok: retry_ok,
                body: retry_body,
            });
        }
    }

    Ok(JsonResponse {
        status,
        ok,
        body: parsed,
    })
}

/// `request_json_async` 的同步包装（经 block_on 桥接）。
fn request_json(
    port: u16,
    endpoint: &str,
    body: Option<String>,
    timeout_ms: Option<u64>,
    options: &Options,
) -> Result<JsonResponse, CliError> {
    block_on(request_json_async(
        port, endpoint, body, timeout_ms, options,
    ))
}

/// --wait 未显式给 timeout 时的默认等待秒数（600s）。
const DEFAULT_WAIT_TIMEOUT_SECONDS: f64 = 600.0;
/// 等待窗口之上追加的 HTTP 缓冲毫秒数，覆盖等待到期后响应传播的延迟。
const WAIT_HTTP_TIMEOUT_BUFFER_MS: u64 = 30_000;
/// worktree 预置（建会话前的 git 操作）的额外超时毫秒数。
const WORKTREE_PROVISION_TIMEOUT_MS: u64 = 120_000;

/// `resolveControlTimeoutMs`.
/// 计算 control 请求超时：显式覆盖值优先；无等待且无 worktree 则不设超时（None）；
/// 等待窗口 = timeout 秒 + 30s 缓冲；带 worktree 再加 120s 预置窗口。
fn resolve_control_timeout_ms(
    input: &serde_json::Value,
    timeout_ms_override: Option<u64>,
) -> Option<u64> {
    if let Some(timeout) = timeout_ms_override.filter(|timeout| *timeout > 0) {
        return Some(timeout);
    }
    let provisions_worktree = field(input, "worktree")
        .as_str()
        .is_some_and(|worktree| !worktree.trim().is_empty());
    if field(input, "wait") != &serde_json::Value::Bool(true) {
        return provisions_worktree.then_some(WORKTREE_PROVISION_TIMEOUT_MS);
    }
    let wait_seconds = field(input, "timeout")
        .as_f64()
        .filter(|timeout| *timeout > 0.0)
        .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECONDS);
    let wait_timeout_ms = (wait_seconds * 1000.0) as u64 + WAIT_HTTP_TIMEOUT_BUFFER_MS;
    Some(if provisions_worktree {
        wait_timeout_ms + WORKTREE_PROVISION_TIMEOUT_MS
    } else {
        wait_timeout_ms
    })
}

/// `requestControlAction`: one typed POST to the shared control endpoint.
/// 发送 {action, input} 信封到 /api/ompchamber/control。失败时读取响应里的 error
/// 文案（缺省 "Failed to execute <action>"），partial 结果附上残留会话提示；
/// 400 / 404 映射为 usage 错误，其余状态码映射为 GENERAL_ERROR。
fn request_control_action(
    port: u16,
    action: &str,
    input: &serde_json::Value,
    options: &Options,
) -> Result<serde_json::Value, CliError> {
    let timeout_ms = resolve_control_timeout_ms(input, None);
    let body = serde_json::json!({ "action": action, "input": input }).to_string();
    let response = request_json(
        port,
        "/api/ompchamber/control",
        Some(body),
        timeout_ms,
        options,
    )?;
    if response.ok {
        return Ok(response.body);
    }

    let partial = field(&response.body, "partial") == &serde_json::Value::Bool(true);
    let partial_session_id = partial
        .then(|| {
            value_str(&response.body, "sessionId")
                .map(str::trim)
                .filter(|session_id| !session_id.is_empty())
        })
        .flatten();
    let partial_directory = partial
        .then(|| {
            value_str(&response.body, "directory")
                .map(str::trim)
                .filter(|directory| !directory.is_empty())
        })
        .flatten();
    let partial_subject = if value_str(&response.body, "partialAction") == Some("goal-configured") {
        "Goal on session"
    } else {
        "Forked session"
    };
    let partial_suffix = match partial_session_id {
        Some(session_id) => format!(
            " {partial_subject} {session_id} remains available{}.",
            partial_directory
                .map(|directory| format!(" in {directory}"))
                .unwrap_or_default()
        ),
        None => String::new(),
    };
    let fallback = format!("Failed to execute {action}");
    let message = format!(
        "{}{partial_suffix}",
        value_str(&response.body, "error")
            .map(str::trim)
            .filter(|error| !error.is_empty())
            .unwrap_or(&fallback)
    );
    if response.status == 400 || response.status == 404 {
        Err(CliError::usage(message))
    } else {
        Err(CliError::new(message, GENERAL_ERROR))
    }
}

// ── goal mode (`cli-goal.js`) ──────────────────────────────────────────

/// 校验 --goal-token-budget：必须搭配 --goal，且是 1000..=100_000_000 的纯整数；
/// 选项未提供时返回 None。
fn parse_goal_token_budget(options: &Options) -> Result<Option<u64>, CliError> {
    let Some(raw) = options.goal_token_budget.as_deref() else {
        return Ok(None);
    };
    if !options.goal {
        return Err(CliError::usage("--goal-token-budget requires --goal."));
    }
    let invalid =
        || CliError::usage("--goal-token-budget must be an integer from 1000 to 100000000.");
    let trimmed = raw.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let Ok(budget) = trimmed.parse::<u64>() else {
        return Err(invalid());
    };
    if !(1000..=100_000_000).contains(&budget) {
        return Err(invalid());
    }
    Ok(Some(budget))
}

// ── schedule (`commands-schedule.js`) ──────────────────────────────────

/// `ompchamber schedule --help` 的帮助文案模板（与旧 JS CLI 逐字对齐）。
const SCHEDULE_HELP: &str = "OMPChamber Schedule Commands\n\nUSAGE:\n  ompchamber schedule status [OPTIONS]\n  ompchamber schedule list (--project <projectId> | --dir <path>) [OPTIONS]\n  ompchamber schedule create (--project <projectId> | --dir <path>) --name <name> --prompt <prompt> --model <provider/model> (--daily <HH:mm> | --weekly <0,1,2> --time <HH:mm> | --once <YYYY-MM-DD> --time <HH:mm> | --cron <expr>) [OPTIONS]\n  ompchamber schedule run (--project <projectId> | --dir <path>) --task <taskId> [OPTIONS]\n  ompchamber schedule delete (--project <projectId> | --dir <path>) --task <taskId> [OPTIONS]\n  ompchamber schedule enable (--project <projectId> | --dir <path>) --task <taskId> [OPTIONS]\n  ompchamber schedule disable (--project <projectId> | --dir <path>) --task <taskId> [OPTIONS]\n\nOPTIONS:\n  --project <projectId>   Project id from ompchamber projects\n  --dir <path>            Resolve project by directory\n  -p, --port <port>       OMPChamber server port\n  --timezone <zone>       IANA timezone for created tasks\n  --agent <id>            Agent to use when running task\n  --variant <id>          Model variant to use when running task\n  --goal                  Continue the scheduled session toward a goal\n  --goal-token-budget <n> Goal token budget (1000-100000000; requires --goal)\n  --disabled              Create task disabled\n  --json                  Output machine-readable JSON\n  -q, --quiet             Print concise output\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
/// 供 mod.rs 的共享 --help 分发使用。
pub fn schedule_help_text() -> &'static str {
    SCHEDULE_HELP
}

/// 断言必填选项非空，缺失时报 usage 错 "Missing required <flag>."。
fn assert_required(value: Option<&str>, flag_name: &str) -> Result<String, CliError> {
    as_non_empty_str(value).ok_or_else(|| CliError::usage(format!("Missing required {flag_name}.")))
}

/// 任务 execution 的 goal 摘要："goal:no" / "goal:yes" / "goal:yes budget:N"。
fn format_goal(execution: &serde_json::Value) -> String {
    if field(execution, "goalEnabled") != &serde_json::Value::Bool(true) {
        return "goal:no".to_string();
    }
    match field(execution, "goalTokenBudget")
        .as_f64()
        .filter(|budget| budget.is_finite())
    {
        Some(budget) => format!("goal:yes budget:{}", js_number(budget)),
        None => "goal:yes".to_string(),
    }
}

/// 把 JSON 数组拼成逗号串：字符串原样、数字按 JS 数字格式化、其余元素为空串。
fn join_string_array(value: &serde_json::Value) -> String {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| match item {
                    serde_json::Value::String(text) => text.clone(),
                    serde_json::Value::Number(number) => {
                        js_number(number.as_f64().unwrap_or_default())
                    }
                    _ => String::new(),
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

/// 任务 schedule 的人类可读摘要：daily / weekly / once / cron 各自格式化，
/// 未知 kind 原样输出，非对象输出 "unknown"。
fn format_schedule(schedule: &serde_json::Value) -> String {
    if !schedule.is_object() {
        return "unknown".to_string();
    }
    let kind = value_str(schedule, "kind").unwrap_or_default();
    match kind {
        "daily" => format!("daily {}", join_string_array(field(schedule, "times")))
            .trim()
            .to_string(),
        "weekly" => format!(
            "weekly days:{} time:{}",
            join_string_array(field(schedule, "weekdays")),
            join_string_array(field(schedule, "times"))
        ),
        "once" => format!(
            "once {} {}",
            value_str(schedule, "date").unwrap_or_default(),
            value_str(schedule, "time").unwrap_or_default()
        )
        .trim()
        .to_string(),
        "cron" => format!("cron {}", value_str(schedule, "cron").unwrap_or_default())
            .trim()
            .to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => "unknown".to_string(),
    }
}

/// 任务列表的三种输出模式：JSON（注入 status:first）、quiet（单行紧凑）、
/// human（clack 框线逐任务展示，禁用任务标 warning）。
fn output_tasks(options: &Options, tasks: &serde_json::Value) {
    let normalized_tasks = tasks.as_array().cloned().unwrap_or_default();
    if options.json {
        print_json(&status_first_json(
            serde_json::json!({ "tasks": normalized_tasks }),
        ));
        return;
    }
    if options.quiet {
        for task in &normalized_tasks {
            let disabled = field(task, "enabled") == &serde_json::Value::Bool(false);
            let status = field(field(task, "state"), "lastStatus")
                .as_str()
                .filter(|status| !status.is_empty())
                .unwrap_or("idle");
            println!(
                "{} enabled:{} {} status:{} {} {}",
                value_str(task, "id").unwrap_or_default(),
                if disabled { "no" } else { "yes" },
                format_goal(field(task, "execution")),
                status,
                format_schedule(field(task, "schedule")),
                value_str(task, "name").unwrap_or_default(),
            );
        }
        return;
    }

    clack_intro("Scheduled Tasks");
    if normalized_tasks.is_empty() {
        log_status("info", "No scheduled tasks found", None);
        clack_outro("0 tasks");
        return;
    }
    for task in &normalized_tasks {
        let status = if field(task, "enabled") == &serde_json::Value::Bool(false) {
            "warning"
        } else {
            "success"
        };
        let detail = format!(
            "id: {}; {}; status: {}; {}",
            value_str(task, "id").unwrap_or_default(),
            format_goal(field(task, "execution")),
            field(field(task, "state"), "lastStatus")
                .as_str()
                .filter(|status| !status.is_empty())
                .unwrap_or("idle"),
            format_schedule(field(task, "schedule")),
        );
        let label = value_str(task, "name")
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| value_str(task, "id").unwrap_or_default())
            .to_string();
        log_status(status, &label, Some(&detail));
    }
    clack_outro(&format!("{} task(s)", normalized_tasks.len()));
}

/// schedule target scope: `--project` / `--dir` (trimmed, when non-empty).
/// 构造目标范围输入：--project → projectId、--dir → directory（trim 后非空才带）。
fn schedule_target_input(options: &Options) -> serde_json::Map<String, serde_json::Value> {
    let mut target = serde_json::Map::new();
    if let Some(project) = as_non_empty_str(options.project.as_deref()) {
        target.insert("projectId".to_string(), serde_json::json!(project));
    }
    if let Some(directory) = as_non_empty_str(options.directory.as_deref()) {
        target.insert("directory".to_string(), serde_json::json!(directory));
    }
    target
}

/// `ompchamber schedule <action>`：status / list / create / run / delete / enable /
/// disable。先解析目标端口与范围，再按动作构造输入调用对应 control action，
/// 按 json / quiet / human 三种模式渲染结果；未知动作在端口解析之后报 usage 错。
pub fn schedule_command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let action = parsed
        .schedule_action
        .clone()
        .unwrap_or_else(|| "help".to_string());
    if action == "help" {
        print!("{SCHEDULE_HELP}");
        return Ok(());
    }

    let port = resolve_target_port(&options)?;
    let target = schedule_target_input(&options);

    if action == "status" {
        let body =
            request_control_action(port, "schedule.status", &serde_json::json!({}), &options)?;
        if options.json {
            print_json(&status_first_json(body.clone()));
            return Ok(());
        }
        let enabled_count = field(&body, "enabledScheduledTasksCount");
        let running_count = field(&body, "runningScheduledTasksCount");
        if options.quiet {
            println!(
                "enabled:{} running:{}",
                enabled_count
                    .as_f64()
                    .map(js_number)
                    .unwrap_or_else(|| "0".to_string()),
                running_count
                    .as_f64()
                    .map(js_number)
                    .unwrap_or_else(|| "0".to_string()),
            );
            return Ok(());
        }
        clack_intro("Scheduled Task Status");
        log_status(
            if field(&body, "hasEnabledScheduledTasks") == &serde_json::Value::Bool(true) {
                "success"
            } else {
                "info"
            },
            &format!(
                "enabled: {}",
                enabled_count
                    .as_f64()
                    .map(js_number)
                    .unwrap_or_else(|| "0".to_string())
            ),
            None,
        );
        log_status(
            if field(&body, "hasRunningScheduledTasks") == &serde_json::Value::Bool(true) {
                "success"
            } else {
                "info"
            },
            &format!(
                "running: {}",
                running_count
                    .as_f64()
                    .map(js_number)
                    .unwrap_or_else(|| "0".to_string())
            ),
            None,
        );
        clack_outro("status loaded");
        return Ok(());
    }

    if action == "list" {
        let body = request_control_action(
            port,
            "schedule.list",
            &serde_json::Value::Object(target),
            &options,
        )?;
        output_tasks(&options, field(&body, "tasks"));
        return Ok(());
    }

    if action == "create" {
        let goal_token_budget = parse_goal_token_budget(&options)?;
        let mut input = target;
        for key in [
            "name", "prompt", "model", "daily", "weekly", "once", "time", "cron", "timezone",
            "agent", "variant",
        ] {
            let value = match key {
                "name" => options.name.clone(),
                "prompt" => options.prompt.clone(),
                "model" => options.model.clone(),
                "daily" => options.daily.clone(),
                "weekly" => options.weekly.clone(),
                "once" => options.once.clone(),
                "time" => options.time.clone(),
                "cron" => options.cron.clone(),
                "timezone" => options.timezone.clone(),
                "agent" => options.agent.clone(),
                _ => options.variant.clone(),
            };
            if let Some(value) = value {
                input.insert(key.to_string(), serde_json::json!(value));
            }
        }
        input.insert("goal".to_string(), serde_json::json!(options.goal));
        if let Some(budget) = goal_token_budget {
            input.insert("goalTokenBudget".to_string(), serde_json::json!(budget));
        }
        input.insert("disabled".to_string(), serde_json::json!(options.disabled));

        let body = request_control_action(
            port,
            "schedule.create",
            &serde_json::Value::Object(input),
            &options,
        )?;
        let task = field(&body, "task");
        if options.json {
            print_json(&status_first_json(serde_json::json!({
                "task": task,
                "created": field(&body, "created") == &serde_json::Value::Bool(true),
            })));
            return Ok(());
        }
        if options.quiet {
            println!("{}", value_str(task, "id").unwrap_or_default());
            return Ok(());
        }
        clack_intro("Scheduled Task Created");
        let name = value_str(task, "name")
            .map(str::to_string)
            .or_else(|| options.name.clone())
            .unwrap_or_default();
        log_status(
            "success",
            &name,
            Some(&format!(
                "id: {}; {}; {}",
                value_str(task, "id").unwrap_or("unknown"),
                format_goal(field(task, "execution")),
                format_schedule(field(task, "schedule")),
            )),
        );
        clack_outro("created");
        return Ok(());
    }

    if action == "run" {
        let task_id = assert_required(options.task.as_deref(), "--task")?;
        let mut input = target;
        input.insert("taskId".to_string(), serde_json::json!(task_id));
        let body = request_control_action(
            port,
            "schedule.run",
            &serde_json::Value::Object(input),
            &options,
        )?;
        if options.json {
            let mut payload = serde_json::Map::new();
            payload.insert("task".to_string(), field(&body, "task").clone());
            if let Some(session_id) = body.get("sessionId").filter(|v| !v.is_null()) {
                payload.insert("sessionId".to_string(), session_id.clone());
            }
            print_json(&serde_json::Value::Object(payload));
            return Ok(());
        }
        if options.quiet {
            println!("{}", value_str(&body, "sessionId").unwrap_or_default());
            return Ok(());
        }
        let task = field(&body, "task");
        clack_intro("Scheduled Task Run");
        log_status(
            "success",
            value_str(task, "name")
                .filter(|name| !name.is_empty())
                .unwrap_or(&task_id),
            Some(&format!(
                "session: {}",
                value_str(&body, "sessionId")
                    .filter(|id| !id.is_empty())
                    .unwrap_or("unknown")
            )),
        );
        clack_outro("started");
        return Ok(());
    }

    if action == "delete" {
        let task_id = assert_required(options.task.as_deref(), "--task")?;
        let mut input = target;
        input.insert("taskId".to_string(), serde_json::json!(task_id));
        let body = request_control_action(
            port,
            "schedule.delete",
            &serde_json::Value::Object(input),
            &options,
        )?;
        if options.json {
            let tasks = body
                .get("tasks")
                .cloned()
                .filter(|tasks| tasks.is_array())
                .unwrap_or_else(|| serde_json::json!([]));
            print_json(&status_first_json(
                serde_json::json!({ "deleted": true, "tasks": tasks }),
            ));
            return Ok(());
        }
        if options.quiet {
            println!("deleted {task_id}");
            return Ok(());
        }
        clack_intro("Scheduled Task Deleted");
        log_status("success", &format!("deleted {task_id}"), None);
        clack_outro("deleted");
        return Ok(());
    }

    if action == "enable" || action == "disable" {
        let task_id = assert_required(options.task.as_deref(), "--task")?;
        let enabled = action == "enable";
        let mut input = target;
        input.insert("taskId".to_string(), serde_json::json!(task_id));
        input.insert("disabled".to_string(), serde_json::json!(!enabled));
        let body = request_control_action(
            port,
            "schedule.toggle",
            &serde_json::Value::Object(input),
            &options,
        )?;
        let task = field(&body, "task");
        if options.json {
            print_json(&status_first_json(
                serde_json::json!({ "task": task, "enabled": enabled }),
            ));
            return Ok(());
        }
        if options.quiet {
            println!("{task_id} enabled:{}", if enabled { "yes" } else { "no" });
            return Ok(());
        }
        clack_intro(if enabled {
            "Scheduled Task Enabled"
        } else {
            "Scheduled Task Disabled"
        });
        log_status(
            "success",
            value_str(task, "name")
                .filter(|name| !name.is_empty())
                .unwrap_or(&task_id),
            Some(&format!("enabled: {}", if enabled { "yes" } else { "no" })),
        );
        clack_outro(if enabled { "enabled" } else { "disabled" });
        return Ok(());
    }

    Err(CliError::usage(format!(
        "Unknown schedule command '{action}'."
    )))
}

// ── session (`commands-session.js`) ────────────────────────────────────

/// `ompchamber session --help` 的帮助文案模板（与旧 JS CLI 逐字对齐）。
const SESSION_HELP: &str = "OMPChamber Session Commands\n\nUSAGE:\n  ompchamber session list [--dir <path>] [--limit <count>] [--with-status] [OPTIONS]\n  ompchamber session create --dir <path> [--title <title>] [--wait] [OPTIONS]\n  ompchamber session create --project <projectId> [--title <title>] [--wait] [OPTIONS]\n  ompchamber session send --session <id> --dir <path> --prompt <text> [--wait] [OPTIONS]\n  ompchamber session fork --session <id> --dir <path> --prompt <text> [--message <id>] [--wait] [OPTIONS]\n  ompchamber session status --session <id> --dir <path> [OPTIONS]\n  ompchamber session messages --session <id> --dir <path> [--wait] [OPTIONS]\n\nLIST OPTIONS:\n  --dir <path>            Filter sessions by directory\n  --limit <count>         Maximum sessions to show (default: 10)\n  --all                   Include archived sessions\n  --with-status           Include authoritative idle/busy/retry status\n\nACTION OPTIONS:\n  --session <id>          Source or target session id\n  --dir <path>            Authoritative session directory\n  --prompt <text>         Prompt to send to the session\n  --message <id>          Fork from this message (fork only; default: latest)\n  --model <provider/model>  Model for the prompt (defaults to configured selection)\n  --agent <id>            Agent for the prompt (defaults to configured selection)\n  --variant <id>          Model variant for the prompt\n  --goal                  Run the prompt as a new goal\n  --goal-token-budget <n> Goal token budget (1000-100000000; requires --goal)\n  --wait                  Wait for the dispatched activity to become idle\n  --last-assistant        Include the last assistant text after waiting\n  --timeout <seconds>     Wait timeout in seconds (default: 600, max: 86400)\n\nCREATE OPTIONS:\n  --worktree <name>       Create a git worktree before creating the session\n  --branch <name>         Branch name for --worktree\n  --start-ref, --base <ref>  Start ref for --worktree\n  --upstream              Set upstream for the worktree branch\n  --no-upstream           Do not set upstream for the worktree branch\n  --name <title>          Alias for --title\n\nSTATUS/MESSAGES OPTIONS:\n  --last                  Return only the latest text-bearing message\n  --last-assistant        Shorthand for --last --role assistant\n  --limit <count>         Maximum text messages to return (default: 10)\n  --all                   Return all text-bearing messages\n  --role <role>           Filter messages: all, user, assistant\n\nOUTPUT OPTIONS:\n  -p, --port <port>       OMPChamber server port\n  --json                  Output machine-readable JSON\n  -q, --quiet             Print compact output\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
/// 供 mod.rs 的共享 --help 分发使用。
pub fn session_help_text() -> &'static str {
    SESSION_HELP
}

/// 校验 --model 必须是 `provider/model`（斜杠不能在首尾）；空 / 空白视为未指定。
fn validate_model(model: Option<&str>) -> Result<Option<String>, CliError> {
    let Some(normalized) = as_non_empty_str(model) else {
        return Ok(None);
    };
    if normalized
        .find('/')
        .is_none_or(|slash| slash == 0 || slash == normalized.len() - 1)
    {
        return Err(CliError::usage("--model must be in provider/model format."));
    }
    Ok(Some(normalized))
}

/// 规整 --limit：未提供用 fallback，提供了必须 ≥1，否则报 usage 错。
fn normalize_limit(value: Option<u32>, fallback: u32) -> Result<u32, CliError> {
    match value {
        None => Ok(fallback),
        Some(parsed) if parsed >= 1 => Ok(parsed),
        Some(_) => Err(CliError::usage(
            "Invalid limit value. Provide a positive integer.",
        )),
    }
}

/// 断言 --session 与 --dir 都非空，返回 (session_id, directory)。
fn assert_session_target(options: &Options) -> Result<(String, String), CliError> {
    let session_id = as_non_empty_str(options.session.as_deref())
        .ok_or_else(|| CliError::usage("Missing required --session."))?;
    let directory = as_non_empty_str(options.directory.as_deref())
        .ok_or_else(|| CliError::usage("Missing required --dir."))?;
    Ok((session_id, directory))
}

/// 规整消息角色过滤：缺省 "all"，只接受 all / user / assistant。
fn normalize_message_role(value: Option<&str>) -> Result<String, CliError> {
    let role = as_non_empty_str(value).unwrap_or_else(|| "all".to_string());
    if !matches!(role.as_str(), "all" | "user" | "assistant") {
        return Err(CliError::usage(
            "--role must be one of: all, user, assistant.",
        ));
    }
    Ok(role)
}

/// 单条文本消息的 Markdown 渲染：**User / Assistant** 标题，时间与模型作为斜体
/// 附注（createdAt 为 0 或无法格式化时省略），正文接在其后。
fn format_text_message(message: &serde_json::Value) -> String {
    let label = if value_str(message, "role") == Some("user") {
        "User"
    } else {
        "Assistant"
    };
    let timestamp = field(message, "createdAt")
        .as_f64()
        .filter(|created_at| *created_at != 0.0)
        .and_then(iso8601_from_epoch_ms)
        .unwrap_or_default();
    let model = value_str(message, "model").unwrap_or_default();
    let details = [timestamp, model.to_string()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    let details_suffix = if details.is_empty() {
        String::new()
    } else {
        format!("\n\n*{details}*")
    };
    format!(
        "**{label}**{details_suffix}\n\n{}",
        value_str(message, "text").unwrap_or_default()
    )
}

/// 从 session.model 提取 `provider/model` 引用：兼容 providerID / providerId 与
/// id / modelID / modelId 多种键名；任一缺失返回 None。
fn format_session_model(session: &serde_json::Value) -> Option<String> {
    let model = field(session, "model");
    let provider_id = value_str(model, "providerID")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            value_str(model, "providerId")
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })?;
    let model_id = ["id", "modelID", "modelId"].iter().find_map(|key| {
        value_str(model, key)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })?;
    Some(format!("{provider_id}/{model_id}"))
}

/// session list 的单行渲染：`标题` — `模型`, `agent`[, `variant`] — [status:xx] —
/// `目录`；标题回退 slug → id → untitled，缺省字段用 unknown-* 占位。
fn format_session_line(session: &serde_json::Value) -> String {
    let title = as_non_empty_str(value_str(session, "title"))
        .or_else(|| as_non_empty_str(value_str(session, "slug")))
        .or_else(|| as_non_empty_str(value_str(session, "id")))
        .unwrap_or_else(|| "untitled".to_string());
    let model = format_session_model(session).unwrap_or_else(|| "unknown-model".to_string());
    let agent = as_non_empty_str(value_str(session, "agent"))
        .unwrap_or_else(|| "unknown-agent".to_string());
    let variant =
        value_str(field(session, "model"), "variant").filter(|variant| !variant.is_empty());
    let directory = as_non_empty_str(value_str(session, "directory"))
        .unwrap_or_else(|| "unknown-directory".to_string());
    let mut selections = vec![format!("`{model}`"), format!("`{agent}`")];
    if let Some(variant) = variant {
        if variant != "default" {
            selections.push(format!("`{variant}`"));
        }
    }
    let status = as_non_empty_str(
        field(session, "status")
            .get("type")
            .and_then(|value| value.as_str()),
    );
    format!(
        "- `{title}` — {}{} — `{directory}`",
        selections.join(", "),
        status
            .map(|status| format!(" — status:{status}"))
            .unwrap_or_default(),
    )
}

/// `buildSessionCreatePayload`.
/// 构造 session.create 的输入：--dir 与 --project 二选一必填；--goal 必须带
/// prompt；worktree 相关选项收敛为嵌套对象 {name, branchName, startRef}。
fn build_session_create_payload(
    options: &Options,
) -> Result<serde_json::Map<String, serde_json::Value>, CliError> {
    let directory = as_non_empty_str(options.directory.as_deref());
    let project_id = as_non_empty_str(options.project.as_deref());
    if directory.is_none() && project_id.is_none() {
        return Err(CliError::usage("Missing required --dir or --project."));
    }
    if directory.is_some() && project_id.is_some() {
        return Err(CliError::usage("Provide only one of --dir or --project."));
    }

    let prompt = as_non_empty_str(options.prompt.as_deref());
    let model = validate_model(options.model.as_deref())?;
    let goal_enabled = options.goal;
    let goal_token_budget = parse_goal_token_budget(options)?;
    if goal_enabled && prompt.is_none() {
        return Err(CliError::usage("--goal requires --prompt."));
    }

    let mut payload = serde_json::Map::new();
    if let Some(directory) = directory {
        payload.insert("directory".to_string(), serde_json::json!(directory));
    }
    if let Some(project_id) = project_id {
        payload.insert("projectId".to_string(), serde_json::json!(project_id));
    }
    let title = as_non_empty_str(options.title.as_deref())
        .or_else(|| as_non_empty_str(options.name.as_deref()));
    if let Some(title) = title {
        payload.insert("title".to_string(), serde_json::json!(title));
    }
    if let Some(worktree) = as_non_empty_str(options.worktree.as_deref()) {
        let mut worktree_payload = serde_json::Map::new();
        worktree_payload.insert("name".to_string(), serde_json::json!(worktree));
        if let Some(branch) = as_non_empty_str(options.branch.as_deref()) {
            worktree_payload.insert("branchName".to_string(), serde_json::json!(branch));
        }
        if let Some(start_ref) = as_non_empty_str(options.start_ref.as_deref()) {
            worktree_payload.insert("startRef".to_string(), serde_json::json!(start_ref));
        }
        payload.insert(
            "worktree".to_string(),
            serde_json::Value::Object(worktree_payload),
        );
    }
    if let Some(prompt) = prompt {
        payload.insert("prompt".to_string(), serde_json::json!(prompt));
    }
    if let Some(model) = model {
        payload.insert("model".to_string(), serde_json::json!(model));
    }
    if let Some(agent) = as_non_empty_str(options.agent.as_deref()) {
        payload.insert("agent".to_string(), serde_json::json!(agent));
    }
    if let Some(variant) = as_non_empty_str(options.variant.as_deref()) {
        payload.insert("variant".to_string(), serde_json::json!(variant));
    }
    if goal_enabled {
        payload.insert("goal".to_string(), serde_json::json!(true));
    }
    if let Some(budget) = goal_token_budget {
        payload.insert("goalTokenBudget".to_string(), serde_json::json!(budget));
    }
    if let Some(set_upstream) = options.set_upstream {
        payload.insert("setUpstream".to_string(), serde_json::json!(set_upstream));
    }
    Ok(payload)
}

/// `buildSessionPromptPayload`.
/// 构造 session.send / fork 的输入：--session / --dir / --prompt 必填；
/// --message 仅 fork 可用；goal 与 token 预算按需附加。
fn build_session_prompt_payload(
    options: &Options,
    action: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, CliError> {
    let (_, directory) = assert_session_target(options)?;
    let prompt = as_non_empty_str(options.prompt.as_deref())
        .ok_or_else(|| CliError::usage("Missing required --prompt."))?;
    let model = validate_model(options.model.as_deref())?;
    let message_id = as_non_empty_str(options.message.as_deref());
    if message_id.is_some() && action != "fork" {
        return Err(CliError::usage("--message is only valid for session fork."));
    }
    let goal_enabled = options.goal;
    let goal_token_budget = parse_goal_token_budget(options)?;

    let mut payload = serde_json::Map::new();
    payload.insert("directory".to_string(), serde_json::json!(directory));
    payload.insert("prompt".to_string(), serde_json::json!(prompt));
    if let Some(message_id) = message_id {
        payload.insert("messageId".to_string(), serde_json::json!(message_id));
    }
    if let Some(model) = model {
        payload.insert("model".to_string(), serde_json::json!(model));
    }
    if let Some(agent) = as_non_empty_str(options.agent.as_deref()) {
        payload.insert("agent".to_string(), serde_json::json!(agent));
    }
    if let Some(variant) = as_non_empty_str(options.variant.as_deref()) {
        payload.insert("variant".to_string(), serde_json::json!(variant));
    }
    if goal_enabled {
        payload.insert("goal".to_string(), serde_json::json!(true));
    }
    if let Some(budget) = goal_token_budget {
        payload.insert("goalTokenBudget".to_string(), serde_json::json!(budget));
    }
    Ok(payload)
}

/// 校验等待相关选项组合：--timeout 与 --last-assistant 都要求搭配 --wait。
fn validate_action_wait_options(options: &Options, action: &str) -> Result<(), CliError> {
    if options.timeout.is_some() && !options.wait {
        return Err(CliError::usage("--timeout requires --wait."));
    }
    if options.last_assistant && !options.wait {
        return Err(CliError::usage(format!(
            "--last-assistant requires --wait for session {action}."
        )));
    }
    Ok(())
}

/// The shared "wait envelope" appended to dispatched control inputs.
/// 追加到派发输入末尾的"等待信封"：wait、lastAssistant，以及可选的 timeout
/// （按 JS Number 语义解析，经 number_option）。
fn wait_envelope(options: &Options) -> Vec<(String, serde_json::Value)> {
    let mut envelope = vec![
        ("wait".to_string(), serde_json::json!(options.wait)),
        (
            "lastAssistant".to_string(),
            serde_json::json!(options.last_assistant),
        ),
    ];
    if let Some(timeout) = number_option(options.timeout.as_deref()) {
        envelope.insert(1, ("timeout".to_string(), timeout));
    }
    envelope
}

/// Common human tail for send/fork/create results.
/// send / fork / create 结果的公共 human 尾部：worktree 信息（仅 create）、
/// prompt / command 派发提示、goal 模式与预算、会话最终状态。create 控制文案前缀。
fn print_session_result_details(result: &serde_json::Value, create: bool) {
    let worktree = field(result, "worktree");
    let worktree_path = value_str(worktree, "path").filter(|path| !path.is_empty());
    if create {
        if let Some(path) = worktree_path {
            let branch_or_name = value_str(worktree, "branch")
                .filter(|value| !value.is_empty())
                .or_else(|| value_str(worktree, "name").filter(|value| !value.is_empty()))
                .unwrap_or("created");
            log_status("info", &format!("worktree: {branch_or_name}"), Some(path));
        }
    }
    if field(result, "promptDispatched") == &serde_json::Value::Bool(true) {
        let as_command = field(result, "dispatchedAsCommand") == &serde_json::Value::Bool(true);
        log_status(
            "info",
            if as_command {
                if create {
                    "initial command dispatched"
                } else {
                    "command dispatched"
                }
            } else if create {
                "initial prompt dispatched"
            } else {
                "prompt dispatched"
            },
            None,
        );
    }
    if field(result, "goalEnabled") == &serde_json::Value::Bool(true) {
        let budget = field(result, "goalTokenBudget")
            .as_f64()
            // JS truthiness: 0/NaN/undefined suppress the budget detail.
            .filter(|budget| budget.is_finite() && *budget != 0.0)
            .map(|budget| format!("budget: {}", js_number(budget)));
        log_status("info", "goal mode active", budget.as_deref());
    }
    if !field(result, "sessionStatus").is_null() {
        log_status(
            "info",
            &format!(
                "session status: {}",
                value_str(field(result, "sessionStatus"), "type").unwrap_or_default()
            ),
            None,
        );
    }
}

/// `ompchamber session <action>`：list / status / messages / send / fork / create。
/// 各动作先做参数校验再解析端口，send / fork / create 追加等待信封后调用
/// session.* control action；输出分 json / quiet / human 三种模式，
/// 等待完成后可附最后一条 assistant 消息。
pub fn session_command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let action = parsed
        .session_action
        .clone()
        .unwrap_or_else(|| "help".to_string());
    if action == "help" {
        print!("{SESSION_HELP}");
        return Ok(());
    }

    if action == "list" {
        let limit = normalize_limit(options.limit, 10)?;
        let port = resolve_target_port(&options)?;
        let mut input = serde_json::Map::new();
        if let Some(directory) = as_non_empty_str(options.directory.as_deref()) {
            input.insert("directory".to_string(), serde_json::json!(directory));
        }
        input.insert("limit".to_string(), serde_json::json!(limit));
        input.insert("all".to_string(), serde_json::json!(options.all));
        input.insert(
            "withStatus".to_string(),
            serde_json::json!(options.with_status),
        );
        let body = request_control_action(
            port,
            "session.list",
            &serde_json::Value::Object(input),
            &options,
        )?;
        let sessions = field(&body, "sessions")
            .as_array()
            .cloned()
            .unwrap_or_default();
        if options.json {
            print_json(&status_first_json(body.clone()));
            return Ok(());
        }
        if sessions.is_empty() {
            println!("No sessions found.");
        } else {
            for session in &sessions {
                println!("{}", format_session_line(session));
            }
        }
        return Ok(());
    }

    if action == "status" {
        let (session_id, directory) = assert_session_target(&options)?;
        let port = resolve_target_port(&options)?;
        let result = request_control_action(
            port,
            "session.status",
            &serde_json::json!({ "sessionId": session_id, "directory": directory }),
            &options,
        )?;
        if options.json {
            print_json(&status_first_json(result.clone()));
            return Ok(());
        }
        let status_type = value_str(field(&result, "sessionStatus"), "type").unwrap_or_default();
        if options.quiet {
            println!("{status_type}");
            return Ok(());
        }
        println!("{session_id} status:{status_type} directory:{directory}");
        return Ok(());
    }

    if action == "messages" {
        let (session_id, directory) = assert_session_target(&options)?;
        if options.timeout.is_some() && !options.wait {
            return Err(CliError::usage("--timeout requires --wait."));
        }
        if options.last_assistant
            && options
                .role
                .as_deref()
                .is_some_and(|role| !role.trim().is_empty() && role != "assistant")
        {
            return Err(CliError::usage(
                "--last-assistant cannot be combined with a non-assistant --role.",
            ));
        }
        let role = if options.last_assistant {
            "assistant".to_string()
        } else {
            normalize_message_role(options.role.as_deref())?
        };
        let last = options.last || options.last_assistant;
        if options.all && (last || options.limit.is_some()) {
            return Err(CliError::usage(
                "--all cannot be combined with --last or --limit.",
            ));
        }
        if last && options.limit.is_some() {
            return Err(CliError::usage("--last cannot be combined with --limit."));
        }
        let limit = if options.all || last {
            None
        } else {
            Some(normalize_limit(options.limit, 10)?)
        };
        let port = resolve_target_port(&options)?;
        let mut input = serde_json::Map::new();
        input.insert("sessionId".to_string(), serde_json::json!(session_id));
        input.insert("directory".to_string(), serde_json::json!(directory));
        input.insert("role".to_string(), serde_json::json!(role));
        input.insert("all".to_string(), serde_json::json!(options.all));
        input.insert("last".to_string(), serde_json::json!(last));
        if let Some(limit) = limit {
            input.insert("limit".to_string(), serde_json::json!(limit));
        }
        input.insert("wait".to_string(), serde_json::json!(options.wait));
        if let Some(timeout) = number_option(options.timeout.as_deref()) {
            input.insert("timeout".to_string(), timeout);
        }
        input.insert(
            "lastAssistant".to_string(),
            serde_json::json!(options.last_assistant),
        );
        let result = request_control_action(
            port,
            "session.messages",
            &serde_json::Value::Object(input),
            &options,
        )?;
        let messages = field(&result, "messages")
            .as_array()
            .cloned()
            .unwrap_or_default();
        if options.json {
            print_json(&status_first_json(result.clone()));
            return Ok(());
        }
        if messages.is_empty() {
            println!("No text messages found.");
            return Ok(());
        }
        if options.quiet {
            println!(
                "{}",
                messages
                    .iter()
                    .map(|message| value_str(message, "text").unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            );
            return Ok(());
        }
        println!(
            "{}",
            messages
                .iter()
                .map(format_text_message)
                .collect::<Vec<_>>()
                .join("\n\n---\n\n")
        );
        return Ok(());
    }

    if action == "send" || action == "fork" {
        let (session_id, _) = assert_session_target(&options)?;
        let payload = build_session_prompt_payload(&options, &action)?;
        validate_action_wait_options(&options, &action)?;
        let port = resolve_target_port(&options)?;
        let mut input = payload;
        input.insert("sessionId".to_string(), serde_json::json!(session_id));
        for (key, value) in wait_envelope(&options) {
            input.insert(key, value);
        }
        let result = request_control_action(
            port,
            &format!("session.{action}"),
            &serde_json::Value::Object(input),
            &options,
        )?;
        let last_assistant_message = field(&result, "lastAssistantMessage");
        if options.json {
            print_json(&status_first_json(result.clone()));
            return Ok(());
        }
        if options.quiet {
            println!("{}", value_str(&result, "sessionId").unwrap_or_default());
            if let Some(text) =
                value_str(last_assistant_message, "text").filter(|text| !text.is_empty())
            {
                println!("{text}");
            }
            return Ok(());
        }
        clack_intro(if action == "fork" {
            "Session Forked"
        } else {
            "Session Prompt Sent"
        });
        let fallback_label = format!("{action} completed");
        log_status(
            "success",
            value_str(&result, "sessionId")
                .filter(|id| !id.is_empty())
                .unwrap_or(&fallback_label),
            Some(&format!(
                "directory: {}",
                value_str(&result, "directory")
                    .filter(|directory| !directory.is_empty())
                    .unwrap_or("unknown")
            )),
        );
        print_session_result_details(&result, false);
        clack_outro(if action == "fork" { "forked" } else { "sent" });
        if !last_assistant_message.is_null() {
            println!("\n{}\n", format_text_message(last_assistant_message));
        }
        return Ok(());
    }

    if action != "create" {
        return Err(CliError::usage(format!(
            "Unknown session command '{action}'."
        )));
    }

    let payload = build_session_create_payload(&options)?;
    validate_action_wait_options(&options, "create")?;
    let port = resolve_target_port(&options)?;
    let mut control_payload = payload;
    if let Some(worktree) = control_payload.get("worktree").cloned() {
        let name = value_str(&worktree, "name").unwrap_or_default().to_string();
        let branch = value_str(&worktree, "branchName").map(str::to_string);
        let start_ref = value_str(&worktree, "startRef").map(str::to_string);
        control_payload.remove("worktree");
        control_payload.insert("worktree".to_string(), serde_json::json!(name));
        if let Some(branch) = branch {
            control_payload.insert("branch".to_string(), serde_json::json!(branch));
        }
        if let Some(start_ref) = start_ref {
            control_payload.insert("startRef".to_string(), serde_json::json!(start_ref));
        }
    }
    for (key, value) in wait_envelope(&options) {
        control_payload.insert(key, value);
    }
    let result = request_control_action(
        port,
        "session.create",
        &serde_json::Value::Object(control_payload),
        &options,
    )?;
    let last_assistant_message = field(&result, "lastAssistantMessage");

    if options.json {
        print_json(&status_first_json(result.clone()));
        return Ok(());
    }
    if options.quiet {
        println!("{}", value_str(&result, "sessionId").unwrap_or_default());
        if let Some(text) =
            value_str(last_assistant_message, "text").filter(|text| !text.is_empty())
        {
            println!("{text}");
        }
        return Ok(());
    }

    clack_intro("Session Created");
    log_status(
        "success",
        value_str(&result, "sessionId")
            .filter(|id| !id.is_empty())
            .unwrap_or("session created"),
        Some(&format!(
            "directory: {}",
            value_str(&result, "directory")
                .filter(|directory| !directory.is_empty())
                .unwrap_or("unknown")
        )),
    );
    print_session_result_details(&result, true);
    clack_outro("created");
    if !last_assistant_message.is_null() {
        println!("\n{}\n", format_text_message(last_assistant_message));
    }
    Ok(())
}

// ── models (`commands-models.js`) ──────────────────────────────────────

/// `ompchamber models --help` 的帮助文案模板。
const MODELS_HELP: &str = "OMPChamber Models Commands\n\nUSAGE:\n  ompchamber models [OPTIONS]\n\nOUTPUT OPTIONS:\n  -p, --port <port>       OMPChamber server port\n  --json                  Output machine-readable JSON\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
/// 供 mod.rs 的共享 --help 分发使用。
pub fn models_help_text() -> &'static str {
    MODELS_HELP
}

/// 从模型条目提取 `provider/model` 引用：兼容 providerID / providerId 与
/// modelID / modelId / id 键名；任一缺失返回 None（列表中该条被过滤）。
fn format_model_ref(entry: &serde_json::Value) -> Option<String> {
    let provider_id = value_str(entry, "providerID")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            value_str(entry, "providerId")
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })?;
    let model_id = ["modelID", "modelId", "id"].iter().find_map(|key| {
        value_str(entry, key)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })?;
    Some(format!("{provider_id}/{model_id}"))
}

/// models 的 human 输出：默认模型（可含 variant）与默认 agent、收藏列表、
/// 最近使用列表；空列表打印 "- none"。
fn format_models_output(settings: &serde_json::Value) -> String {
    let favorites = field(settings, "favoriteModels")
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(format_model_ref)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let recent = field(settings, "recentModels")
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(format_model_ref)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let default_model =
        as_non_empty_str(value_str(settings, "defaultModel")).unwrap_or_else(|| "none".to_string());
    let default_agent =
        as_non_empty_str(value_str(settings, "defaultAgent")).unwrap_or_else(|| "none".to_string());
    let variant = value_str(settings, "defaultVariant").filter(|variant| !variant.is_empty());

    let mut lines = vec![format!(
        "Default: `{}{}` / `{}`",
        default_model,
        variant
            .map(|variant| format!(" ({variant})"))
            .unwrap_or_default(),
        default_agent
    )];
    lines.push(String::new());
    lines.push("Favorites:".to_string());
    if favorites.is_empty() {
        lines.push("- none".to_string());
    } else {
        lines.extend(favorites.iter().map(|model| format!("- `{model}`")));
    }
    lines.push(String::new());
    lines.push("Recent:".to_string());
    if recent.is_empty() {
        lines.push("- none".to_string());
    } else {
        lines.extend(recent.iter().map(|model| format!("- `{model}`")));
    }
    format!("{}\n", lines.join("\n"))
}

/// `ompchamber models [show]`：经 models.list control action 拉取设置，
/// JSON 原样输出（注入 status:first），human 用 format_models_output 渲染。
pub fn models_command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let action = parsed
        .positionals
        .get(1)
        .cloned()
        .unwrap_or_else(|| "show".to_string());
    if action == "help" {
        print!("{MODELS_HELP}");
        return Ok(());
    }
    if action != "show" {
        return Err(CliError::usage(format!(
            "Unknown models command '{action}'."
        )));
    }

    let port = resolve_target_port(&options)?;
    let result = request_control_action(port, "models.list", &serde_json::json!({}), &options)?;

    if options.json {
        print_json(&status_first_json(result.clone()));
        return Ok(());
    }
    print!("{}", format_models_output(&result));
    Ok(())
}

// ── projects (`commands-projects.js`) ──────────────────────────────────

/// `ompchamber projects --help` 的帮助文案模板。
const PROJECTS_HELP: &str = "OMPChamber Projects Commands\n\nUSAGE:\n  ompchamber projects [OPTIONS]\n\nOUTPUT OPTIONS:\n  -p, --port <port>       OMPChamber server port\n  --json                  Output machine-readable JSON\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
/// 供 mod.rs 的共享 --help 分发使用。
pub fn projects_help_text() -> &'static str {
    PROJECTS_HELP
}

/// 单个项目的一行渲染：`label` — `id` — `path`。
fn format_project_line(project: &serde_json::Value) -> String {
    format!(
        "- `{}` — `{}` — `{}`",
        value_str(project, "label").unwrap_or_default(),
        value_str(project, "id").unwrap_or_default(),
        value_str(project, "path").unwrap_or_default()
    )
}

/// `ompchamber projects [list]`：经 projects.list control action 拉取项目，
/// JSON 输出 {projects}，human 逐行打印；无项目提示 "No projects found."。
pub fn projects_command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let action = parsed
        .positionals
        .get(1)
        .cloned()
        .unwrap_or_else(|| "list".to_string());
    if action == "help" {
        print!("{PROJECTS_HELP}");
        return Ok(());
    }
    if action != "list" {
        return Err(CliError::usage(format!(
            "Unknown projects command '{action}'."
        )));
    }

    let port = resolve_target_port(&options)?;
    let body = request_control_action(port, "projects.list", &serde_json::json!({}), &options)?;
    let projects = field(&body, "projects")
        .as_array()
        .cloned()
        .unwrap_or_default();
    if options.json {
        print_json(&status_first_json(
            serde_json::json!({ "projects": projects }),
        ));
        return Ok(());
    }
    if projects.is_empty() {
        println!("No projects found.");
    } else {
        for project in &projects {
            println!("{}", format_project_line(project));
        }
    }
    Ok(())
}

// ── control (`cli.js` `showControlHelp`) ───────────────────────────────

/// `ompchamber control help` 的帮助文案模板（cli.js 的 showControlHelp）。
const CONTROL_HELP: &str = "\n OMPChamber Control Commands\n\nUSAGE:\n  ompchamber <COMMAND> [OPTIONS]\n\nCOMMANDS:\n  status                         Show running OMPChamber runtimes\n  session                        Create, inspect, and read sessions\n  models                         Show default and favorite models\n  projects                       Show configured projects and IDs\n  schedule                       Manage scheduled tasks\n  tunnel                         Inspect tunnel status/readiness\n  logs                           Tail logs for CLI-managed runtimes\n\nDETAILED HELP:\n  ompchamber session --help     Show session creation, status, and message options\n  ompchamber models --help      Show model defaults and favorites help\n  ompchamber projects --help    Show project list help\n  ompchamber schedule --help    Show scheduled task actions and schedule options\n  ompchamber tunnel help        Show tunnel lifecycle/status commands\n  ompchamber status --help      Show runtime status options\n\nCOMMON OPTIONS:\n  --json                         Output machine-readable JSON\n  -q, --quiet                    Print minimal output\n  -p, --port <port>              Target a specific OMPChamber runtime\n  --ui-password <password>       Authenticate to a password-protected runtime\n\nEXAMPLES:\n  ompchamber status\n  ompchamber models\n  ompchamber projects\n  ompchamber session --help\n  ompchamber schedule --help\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
/// 供 mod.rs 的共享 --help 分发使用。
pub fn control_help_text() -> &'static str {
    CONTROL_HELP
}

/// `ompchamber control`：只支持 help 子命令（打印帮助），其余动作报 usage 错。
pub fn control_command(parsed: &Parsed, _options: Options) -> Result<(), CliError> {
    let action = parsed
        .control_action
        .clone()
        .unwrap_or_else(|| "help".to_string());
    if action != "help" {
        return Err(CliError::usage(format!(
            "Unknown control command '{action}'."
        )));
    }
    // `console.log` appends one newline after the template's own trailing one.
    println!("{CONTROL_HELP}");
    Ok(())
}

/// 覆盖 misc.rs 的单元测试：日期 / 数字格式化、探测主机、实例发现与 status 聚合、
/// 目标端口解析、日志读取与跟随、control 传输与超时窗口、goal 校验、
/// schedule / session / models / projects 的 payload 构造与输出格式化。
/// 通过 EnvGuard 隔离环境变量与数据目录，spawn_fake_server 提供回环假服务端。
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    /// 全局环境变量互斥锁（crate::cli::TEST_ENV_MUTEX）：环境是进程级共享状态。
    fn env_mutex() -> &'static std::sync::Mutex<()> {
        &crate::cli::TEST_ENV_MUTEX
    }

    /// Poisoning-tolerant lock: one failing test must not cascade into every
    /// other env-guarded test.
    /// 容忍中毒的加锁：单个测试失败不得连锁拖垮其它持锁测试。
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        env_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 测试内安全设置环境变量（调用方须已持有 env_lock）。
    fn set_var(key: &str, value: &str) {
        // SAFETY: every test that touches process environment variables holds
        // ENV_LOCK for its whole body, serializing access across threads.
        unsafe { std::env::set_var(key, value) }
    }

    /// 测试内安全删除环境变量（调用方须已持有 env_lock）。
    fn remove_var(key: &str) {
        // SAFETY: see `set_var`.
        unsafe { std::env::remove_var(key) }
    }

    /// Points the CLI at a temp data dir; `isolate_host` reroutes env-derived
    /// probe hosts to 127.0.0.2 so DEFAULT_PORT health checks cannot reach a
    /// real server the developer may be running on 127.0.0.1.
    /// RAII 环境隔离：把 OMPCHAMBER_DATA_DIR / HOST / PORT 指向临时目录，
    /// Drop 时恢复原值；isolate_host 用 127.0.0.2 重定向 env 派生的探测主机，
    /// 防止 DEFAULT_PORT 健康检查误连开发者本机 127.0.0.1 上的真实服务。
    struct EnvGuard {
        /// Drop 时需要恢复的 (键, 原值) 列表；原值为 None 表示安装前不存在。
        saved: Vec<(&'static str, Option<String>)>,
    }

    /// EnvGuard 的安装入口。
    impl EnvGuard {
        /// Caller must already hold `ENV_LOCK` (tests lock it for their whole
        /// body) — std Mutex is not reentrant.
        /// 调用方必须已持有 env_lock（std::sync::Mutex 不可重入）。
        fn install(data_dir: &Path, isolate_host: bool) -> Self {
            let saved = ["OMPCHAMBER_DATA_DIR", "OMPCHAMBER_HOST", "OMPCHAMBER_PORT"]
                .iter()
                .map(|key| (*key, std::env::var(key).ok()))
                .collect::<Vec<_>>();
            set_var("OMPCHAMBER_DATA_DIR", &data_dir.to_string_lossy());
            remove_var("OMPCHAMBER_HOST");
            remove_var("OMPCHAMBER_PORT");
            if isolate_host {
                set_var("OMPCHAMBER_HOST", "127.0.0.2");
            }
            Self { saved }
        }
    }

    /// Drop 时恢复所有被改写的环境变量。
    impl Drop for EnvGuard {
        /// 逐项把环境变量恢复到安装前的状态。
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => set_var(key, &value),
                    None => remove_var(key),
                }
            }
        }
    }

    /// 创建唯一的临时数据目录（进程 id + 原子计数器保证并发唯一）。
    fn temp_data_dir(label: &str) -> PathBuf {
        /// 并发唯一的目录计数器。
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = format!(
            "ompchamber-misc-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        );
        let dir = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        dir
    }

    /// 假服务端的请求处理函数：path → (status, JSON body)。
    type Responder = Arc<dyn Fn(&str) -> (u16, String) + Send + Sync>;

    /// Minimal keep-alive HTTP/1.1 server; GETs and JSON POSTs both served.
    /// Recorded request: (method, path, body).
    /// 已记录请求的共享日志：(method, path, body) 列表。
    type RequestLog = Arc<std::sync::Mutex<Vec<(String, String, String)>>>;

    /// 在 127.0.0.1 随机端口起一个最小 keep-alive HTTP/1.1 假服务端，
    /// 返回 (port, 请求日志)；每连接一个线程，同时服务 GET 与 JSON POST。
    fn spawn_fake_server(responder: Responder) -> (u16, RequestLog) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake server");
        let port = listener.local_addr().unwrap().port();
        let log: RequestLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log_for_accept = Arc::clone(&log);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let responder = Arc::clone(&responder);
                let log = Arc::clone(&log_for_accept);
                std::thread::spawn(move || handle_connection(stream, responder, log));
            }
        });
        (port, log)
    }

    /// 单连接处理循环：按 Content-Length 读完整请求、记入日志、交给 responder
    /// 并写回固定格式响应；10s 读超时，超过 1MiB 的请求直接断开。
    fn handle_connection(mut stream: TcpStream, responder: Responder, log: RequestLog) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        loop {
            let mut raw = Vec::new();
            let mut buffer = [0u8; 8192];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(n) => {
                        raw.extend_from_slice(&buffer[..n]);
                        let header_end = raw.windows(4).position(|window| window == b"\r\n\r\n");
                        if let Some(header_end) = header_end {
                            let headers = String::from_utf8_lossy(&raw[..header_end]).to_string();
                            let content_length = headers
                                .lines()
                                .find_map(|line| {
                                    line.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|value| value.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if raw.len() >= header_end + 4 + content_length {
                                break;
                            }
                        }
                        if raw.len() > (1 << 20) {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
            let request = String::from_utf8_lossy(&raw).to_string();
            let mut request_parts = request.split("\r\n");
            let request_line = request_parts.next().unwrap_or_default().to_string();
            let mut words = request_line.split_whitespace();
            let method = words.next().unwrap_or_default().to_string();
            let path = words.next().unwrap_or("/").to_string();
            let body = request
                .split("\r\n\r\n")
                .nth(1)
                .unwrap_or_default()
                .to_string();
            log.lock().unwrap().push((method, path.clone(), body));
            let (status, body) = responder(&path);
            let reason = match status {
                400 => "Bad Request",
                401 => "Unauthorized",
                404 => "Not Found",
                _ => "OK",
            };
            let reply = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
                body.len()
            );
            if stream.write_all(reply.as_bytes()).is_err() {
                return;
            }
        }
    }

    /// 构造 /api/system/info + /health 的标准 responder（指定 runtime 与 pid）。
    fn system_info_responder(runtime: &str, pid: u32) -> Responder {
        let runtime = runtime.to_string();
        Arc::new(move |path| match path {
            "/api/system/info" => (200, format!("{{\"runtime\":\"{runtime}\",\"pid\":{pid}}}")),
            "/health" => (200, "ok".to_string()),
            _ => (404, "{}".to_string()),
        })
    }

    /// 写出注册表文件对：ompchamber-<port>.pid 与实例元数据 JSON
    /// （daemon 模式、可选 UI 密码、startedAt）。
    fn write_instance_files(
        data_dir: &Path,
        port: u16,
        pid: u32,
        started_at: f64,
        ui_password: Option<&str>,
    ) {
        let run_dir = data_dir.join("run");
        std::fs::create_dir_all(&run_dir).expect("create run dir");
        std::fs::write(
            run_dir.join(format!("ompchamber-{port}.pid")),
            pid.to_string(),
        )
        .expect("write pid file");
        let instance = serde_json::json!({
            "port": port,
            "launchMode": "daemon",
            "uiPassword": ui_password,
            "hasUiPassword": ui_password.is_some(),
            "apiOnly": false,
            "startedAt": started_at,
        });
        std::fs::write(
            run_dir.join(format!("ompchamber-{port}.json")),
            instance.to_string(),
        )
        .expect("write instance file");
    }

    /// 在 logs 目录写出 ompchamber-<port>.log。
    fn write_log_file(data_dir: &Path, port: u16, contents: &str) {
        let logs_dir = data_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).expect("create logs dir");
        std::fs::write(logs_dir.join(format!("ompchamber-{port}.log")), contents)
            .expect("write log file");
    }

    /// 解析 argv 为 Parsed（失败即 panic——测试输入必须合法）。
    fn parse(argv: &[&str]) -> Parsed {
        let owned = argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        args::parse_args(&owned).expect("parse args")
    }

    // ── log files ──────────────────────────────────────────────────

    /// 验证 read_tail_lines 只返回最后 N 行、CRLF 被归一、文件缺失返回空。
    #[test]
    fn read_tail_lines_returns_last_n_and_handles_crlf() {
        let dir = temp_data_dir("tail");
        let log = dir.join("sample.log");
        std::fs::write(&log, "one\r\ntwo\nthree\nfour\n").expect("write fixture");
        assert_eq!(
            read_tail_lines(&log, 2),
            vec!["three".to_string(), "four".to_string()]
        );
        assert_eq!(read_tail_lines(&log, 200).len(), 4);
        assert!(read_tail_lines(&dir.join("missing.log"), 5).is_empty());
    }

    /// 验证 follow_poll 只产出完整行（半行跨读取块拼接），文件截断重写后从头重读。
    #[test]
    fn follow_poll_emits_only_complete_lines_and_resets_on_truncate() {
        let dir = temp_data_dir("follow");
        let log = dir.join("app.log");
        std::fs::write(&log, "existing\n").expect("write fixture");
        let mut state = FollowState::new(log.clone());
        assert!(follow_poll(&mut state).is_empty());

        let append = |text: &str| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&log)
                .expect("open log for append");
            file.write_all(text.as_bytes()).expect("append");
        };
        append("partial");
        assert!(follow_poll(&mut state).is_empty());
        append(" line\nnext\n");
        assert_eq!(
            follow_poll(&mut state),
            vec!["partial line".to_string(), "next".to_string()]
        );

        std::fs::write(&log, "fresh\n").expect("rewrite log");
        assert_eq!(follow_poll(&mut state), vec!["fresh".to_string()]);
    }

    // ── date formatting ────────────────────────────────────────────

    /// 验证 iso8601_from_epoch_ms 与 JS toISOString 输出一致（含闰日）。
    #[test]
    fn iso8601_matches_new_date_toISOString() {
        assert_eq!(
            iso8601_from_epoch_ms(0.0).as_deref(),
            Some("1970-01-01T00:00:00.000Z")
        );
        assert_eq!(
            iso8601_from_epoch_ms(1_758_000_000_123.0).as_deref(),
            Some("2025-09-16T05:20:00.123Z")
        );
        // Leap day.
        assert_eq!(
            iso8601_from_epoch_ms(951_782_400_000.0).as_deref(),
            Some("2000-02-29T00:00:00.000Z")
        );
    }

    // ── probe hosts ────────────────────────────────────────────────

    /// 验证探测主机序列的去重规则，以及"具体权威主机 ⇒ 兜底探测需 PID 匹配"的标记。
    #[test]
    fn probe_hosts_dedup_loopback_and_flag_concrete_pid_matching() {
        let _env = env_lock();
        let _guard = EnvGuard::install(&temp_data_dir("probehosts"), false);
        let hosts = get_system_info_probe_hosts(&[None, None]);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0], (None, false));

        // With a concrete authoritative host the fallbacks require a PID
        // match; the literal 127.0.0.1 dedups against the None host (which
        // resolves to 127.0.0.1 with no OMPCHAMBER_HOST), matching the JS.
        let hosts = get_system_info_probe_hosts(&[Some("192.168.1.5".to_string())]);
        assert_eq!(
            hosts,
            vec![(Some("192.168.1.5".to_string()), false), (None, true),]
        );

        // Wildcard hosts are not authoritative, and resolveApiHost('0.0.0.0')
        // collapses onto the loopback key, so only the wildcard entry remains.
        let hosts = get_system_info_probe_hosts(&[Some("0.0.0.0".to_string())]);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0], (Some("0.0.0.0".to_string()), false));
    }

    // ── status + discovery ─────────────────────────────────────────

    /// 验证注册表实例经探测确认后的字段与 JSON 输出（含密码保护标记）。
    #[test]
    fn status_reports_registry_instance_with_password_protection() {
        let _env = env_lock();
        let data_dir = temp_data_dir("status-registry");
        let _guard = EnvGuard::install(&data_dir, false);

        let pid = std::process::id();
        let (port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, port, pid, 1_234.0, Some("sekret"));

        let options = parse(&["status"]).options;
        let instances = discover_running_instances(&options);
        assert_eq!(instances.len(), 1);
        let instance = &instances[0];
        assert_eq!(instance.port, port);
        assert_eq!(instance.pid, Some(pid));
        assert_eq!(instance.source, "registry+probe");
        assert_eq!(instance.launch_mode, "daemon");
        assert_eq!(instance.started_at, 1234.0);

        let entries = collect_status_instances(&options);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].runtime, "cli");
        assert_eq!(entries[0].password_protected, Some(true));
        let json = status_entry_json(&entries[0]);
        assert_eq!(json["port"], port);
        assert_eq!(json["launchMode"], "daemon");
        assert_eq!(json["passwordProtected"], true);
        assert_eq!(json["pid"], pid);
    }

    /// 验证仅凭端口探测发现的实例被标为 unmanaged，密码状态为 unknown。
    #[test]
    fn status_without_password_reports_unprotected_and_marks_probed_unmanaged() {
        let _env = env_lock();
        let data_dir = temp_data_dir("status-unmanaged");
        let _guard = EnvGuard::install(&data_dir, false);

        let pid = std::process::id();
        let (port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        // No registry files: only reachable through an explicit-port probe.
        let options = parse(&["status", "-p", &port.to_string()]).options;
        let entries = collect_status_instances(&options);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].runtime, "unmanaged");
        assert_eq!(entries[0].password_protected, None);
        assert_eq!(entries[0].launch_mode.as_deref(), Some("daemon"));

        let json = status_entry_json(&entries[0]);
        assert_eq!(json["runtime"], "unmanaged");
        assert_eq!(json["passwordProtected"], serde_json::Value::Null);
    }

    /// 验证 desktop 实例在无同端口 CLI 实例时作为独立条目追加。
    #[test]
    fn status_appends_desktop_entry_when_no_cli_instance_matches() {
        let _env = env_lock();
        let data_dir = temp_data_dir("status-desktop");
        let _guard = EnvGuard::install(&data_dir, false);

        let pid = std::process::id();
        let (cli_port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, cli_port, pid, 100.0, None);

        let (desktop_port, _log) = spawn_fake_server(system_info_responder("desktop", 999_999));
        std::fs::write(
            data_dir.join("settings.json"),
            serde_json::json!({ "desktopLocalPort": desktop_port }).to_string(),
        )
        .expect("write settings");

        let options = parse(&["status"]).options;
        let entries = collect_status_instances(&options);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].runtime, "cli");
        assert_eq!(entries[0].port, cli_port);
        assert_eq!(entries[1].runtime, "desktop");
        assert_eq!(entries[1].port, desktop_port);
        assert_eq!(entries[1].launch_mode, None);
        assert_eq!(entries[1].password_protected, None);
        assert_eq!(password_protection_label(None), "unknown");
    }

    // ── api target resolution ──────────────────────────────────────

    /// 验证显式 -p 直接短路返回，不做任何发现。
    #[test]
    fn resolve_target_port_explicit_short_circuits() {
        let _env = env_lock();
        let _guard = EnvGuard::install(&temp_data_dir("target-explicit"), false);
        let parsed = parse(&["schedule", "status", "-p", "4242"]);
        assert_eq!(resolve_target_port(&parsed.options).unwrap(), 4242);
    }

    /// 验证唯一存活实例时无需 --port 即返回该端口。
    #[test]
    fn resolve_target_port_returns_single_lifecycle_port() {
        let _env = env_lock();
        let data_dir = temp_data_dir("target-single");
        let _guard = EnvGuard::install(&data_dir, false);
        let pid = std::process::id();
        let (port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, port, pid, 100.0, None);

        let options = parse(&["models"]).options;
        assert_eq!(resolve_target_port(&options).unwrap(), port);
    }

    /// 验证 desktop 实例优先于 CLI 实例被选为目标端口。
    #[test]
    fn resolve_target_port_prefers_desktop_runtime() {
        let _env = env_lock();
        let data_dir = temp_data_dir("target-desktop");
        let _guard = EnvGuard::install(&data_dir, false);
        let pid = std::process::id();
        let (cli_port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, cli_port, pid, 100.0, None);

        let (desktop_port, _log) = spawn_fake_server(system_info_responder("desktop", 999_999));
        std::fs::write(
            data_dir.join("settings.json"),
            serde_json::json!({ "desktopLocalPort": desktop_port }).to_string(),
        )
        .expect("write settings");

        let options = parse(&["projects"]).options;
        assert_eq!(resolve_target_port(&options).unwrap(), desktop_port);
    }

    /// 验证多实例且无显式端口时报 usage 错并列出全部端口。
    #[test]
    fn resolve_target_port_errors_on_multiple_without_explicit() {
        let _env = env_lock();
        let data_dir = temp_data_dir("target-multiple");
        let _guard = EnvGuard::install(&data_dir, false);
        let pid = std::process::id();
        let (port_a, _log) = spawn_fake_server(system_info_responder("cli", pid));
        let (port_b, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, port_a, pid, 100.0, None);
        write_instance_files(&data_dir, port_b, pid, 200.0, None);

        let (low, high) = (port_a.min(port_b), port_a.max(port_b));
        let options = parse(&["models"]).options;
        let error = resolve_target_port(&options).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(
            error.message,
            format!(
                "Multiple OMPChamber instances are running (ports: {low}, {high}). Choose one with --port <port>."
            )
        );
    }

    /// 验证无实例且默认端口不健康时的报错文案与退出码。
    #[test]
    fn resolve_target_port_reports_missing_server() {
        let _env = env_lock();
        let _guard = EnvGuard::install(&temp_data_dir("target-missing"), true);
        let options = parse(&["models"]).options;
        let error = resolve_target_port(&options).unwrap_err();
        assert_eq!(error.exit_code, GENERAL_ERROR);
        assert_eq!(
            error.message,
            "No running OMPChamber server found. Start one with `ompchamber serve`, or pass --port <port>."
        );
    }

    // ── logs ───────────────────────────────────────────────────────

    /// 验证 logs 默认选最新实例，JSON 条目含端口、尾部行与日志路径。
    #[test]
    fn logs_resolves_latest_instance_and_builds_json_entries() {
        let _env = env_lock();
        let data_dir = temp_data_dir("logs-latest");
        let _guard = EnvGuard::install(&data_dir, false);
        let pid = std::process::id();
        let (older_port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        let (newer_port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, older_port, pid, 1000.0, None);
        write_instance_files(&data_dir, newer_port, pid, 2000.0, None);
        write_log_file(&data_dir, older_port, "older-1\nolder-2\n");
        write_log_file(&data_dir, newer_port, "newer-1\nnewer-2\n");

        let options = parse(&["logs", "--no-follow", "--lines", "1"]).options;
        let targets = resolve_log_targets(&options).expect("resolve log targets");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].port, newer_port);

        let json = log_entries_json(&targets, 1);
        let entry = &json["entries"][0];
        assert_eq!(entry["port"], newer_port);
        assert_eq!(entry["lines"], serde_json::json!(["newer-2"]));
        assert_eq!(
            entry["logPath"],
            paths::log_file_path(&newer_port.to_string())
                .to_string_lossy()
                .to_string()
        );
    }

    /// 验证无运行实例时 logs 报错。
    #[test]
    fn logs_errors_when_no_instance_running() {
        let _env = env_lock();
        let _guard = EnvGuard::install(&temp_data_dir("logs-none"), false);
        let options = parse(&["logs", "--no-follow"]).options;
        let error = resolve_log_targets(&options).unwrap_err();
        assert_eq!(error.exit_code, GENERAL_ERROR);
        assert_eq!(error.message, "No running OMPChamber instance found.");
    }

    /// 验证显式端口与发现结果不符时的报错文案。
    #[test]
    fn logs_explicit_port_missing_errors() {
        let _env = env_lock();
        let data_dir = temp_data_dir("logs-explicit");
        let _guard = EnvGuard::install(&data_dir, false);
        let pid = std::process::id();
        let (port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, port, pid, 100.0, None);

        let options = parse(&["logs", "--no-follow", "-p", "59999"]).options;
        let error = resolve_log_targets(&options).unwrap_err();
        assert_eq!(
            error.message,
            "No running OMPChamber instance found on port 59999."
        );
    }

    /// 验证 --json 必须搭配 --no-follow。
    #[test]
    fn logs_json_requires_no_follow() {
        let _env = env_lock();
        let data_dir = temp_data_dir("logs-json");
        let _guard = EnvGuard::install(&data_dir, false);
        let pid = std::process::id();
        let (port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, port, pid, 100.0, None);

        let parsed = parse(&["logs", "--json"]);
        let error = logs_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, GENERAL_ERROR);
        assert_eq!(
            error.message,
            "`ompchamber logs --json` requires `--no-follow` for deterministic JSON output."
        );
    }

    // ── control transport ──────────────────────────────────────────

    /// 验证 control action 成功回传、404 映射 usage 错、500 映射通用错误与兜底文案。
    #[test]
    fn request_control_action_round_trips_and_maps_errors() {
        let _env = env_lock();
        let _guard = EnvGuard::install(&temp_data_dir("control"), false);
        let options = parse(&["models"]).options;

        let responder: Responder = Arc::new(move |path| {
            if path == "/api/ompchamber/control" {
                (
                    200,
                    "{\"enabledScheduledTasksCount\":2,\"hasEnabledScheduledTasks\":true}"
                        .to_string(),
                )
            } else {
                (404, "{}".to_string())
            }
        });
        let (port, _log) = spawn_fake_server(responder);
        let body =
            request_control_action(port, "schedule.status", &serde_json::json!({}), &options)
                .expect("control action succeeds");
        assert_eq!(body["enabledScheduledTasksCount"], 2);
        assert_eq!(body["hasEnabledScheduledTasks"], true);

        // 404 responses map to usage errors with the server's message.
        let responder: Responder = Arc::new(move |path| {
            let _ = path;
            (404, "{\"error\":\"unknown action\"}".to_string())
        });
        let (port, _log) = spawn_fake_server(responder);
        let error =
            request_control_action(port, "schedule.bogus", &serde_json::json!({}), &options)
                .unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(error.message, "unknown action");

        // 500 responses fall back to the generic message and general error.
        let responder: Responder = Arc::new(move |path| {
            let _ = path;
            (500, "{}".to_string())
        });
        let (port, _log) = spawn_fake_server(responder);
        let error = request_control_action(port, "session.list", &serde_json::json!({}), &options)
            .unwrap_err();
        assert_eq!(error.exit_code, GENERAL_ERROR);
        assert_eq!(error.message, "Failed to execute session.list");
    }

    /// 验证请求体是 {action, input} 的 typed JSON 信封。
    #[test]
    fn request_control_action_sends_typed_envelope() {
        let _env = env_lock();
        let _guard = EnvGuard::install(&temp_data_dir("control-envelope"), false);
        let options = parse(&["models"]).options;

        let responder: Responder = Arc::new(|path| {
            if path == "/api/ompchamber/control" {
                (200, "{}".to_string())
            } else {
                (404, "{}".to_string())
            }
        });
        let (port, log) = spawn_fake_server(responder);
        let body = request_control_action(
            port,
            "session.status",
            &serde_json::json!({ "sessionId": "s1" }),
            &options,
        )
        .expect("control action succeeds");
        assert_eq!(body, serde_json::json!({}));

        let requests = log.lock().unwrap();
        let (method, path, request_body) = requests
            .iter()
            .find(|(_, path, _)| path == "/api/ompchamber/control")
            .expect("control request captured");
        assert_eq!(method, "POST");
        assert_eq!(path, "/api/ompchamber/control");
        let envelope: serde_json::Value =
            serde_json::from_str(request_body).expect("typed JSON envelope");
        assert_eq!(envelope["action"], "session.status");
        assert_eq!(envelope["input"]["sessionId"], "s1");
    }

    /// 验证超时窗口：无等待无 worktree 不设、worktree 加 120s、等待加缓冲、覆盖值优先。
    #[test]
    fn resolve_control_timeout_ms_follows_wait_and_worktree_windows() {
        assert_eq!(
            resolve_control_timeout_ms(&serde_json::json!({}), None),
            None
        );
        assert_eq!(
            resolve_control_timeout_ms(&serde_json::json!({ "wait": false, "timeout": 30 }), None),
            None
        );
        // Worktree provisioning gets the extended window even without wait.
        assert_eq!(
            resolve_control_timeout_ms(&serde_json::json!({ "worktree": "wt" }), None),
            Some(120_000)
        );
        // Wait windows cover the requested seconds plus the HTTP buffer.
        assert_eq!(
            resolve_control_timeout_ms(&serde_json::json!({ "wait": true, "timeout": 30 }), None),
            Some(60_000)
        );
        assert_eq!(
            resolve_control_timeout_ms(&serde_json::json!({ "wait": true }), None),
            Some(630_000)
        );
        assert_eq!(
            resolve_control_timeout_ms(
                &serde_json::json!({ "wait": true, "timeout": 30, "worktree": "wt" }),
                None
            ),
            Some(180_000)
        );
        assert_eq!(
            resolve_control_timeout_ms(&serde_json::json!({}), Some(1500)),
            Some(1500)
        );
    }

    /// 验证密码解析优先级：显式 --ui-password > 实例文件 > options 回退。
    #[test]
    fn resolve_ui_password_prefers_explicit_then_instance_file() {
        let _env = env_lock();
        let data_dir = temp_data_dir("uipassword");
        let _guard = EnvGuard::install(&data_dir, false);
        write_instance_files(&data_dir, 4567, 424242, 0.0, Some("stored"));

        let mut options = parse(&["models"]).options;
        options.ui_password = Some("fallback".to_string());
        assert_eq!(
            resolve_ui_password_for_port(4567, &options),
            Some("stored".to_string())
        );

        options.explicit_ui_password = true;
        options.ui_password = Some("explicit".to_string());
        assert_eq!(
            resolve_ui_password_for_port(4567, &options),
            Some("explicit".to_string())
        );

        options.explicit_ui_password = true;
        options.ui_password = None;
        assert_eq!(
            resolve_ui_password_for_port(4567, &options),
            Some("stored".to_string())
        );

        assert_eq!(resolve_ui_password_for_port(59999, &options), None);
    }

    // ── goal mode ──────────────────────────────────────────────────

    /// 验证 goal token budget 必须搭配 --goal 且是 1000..=100000000 的整数。
    #[test]
    fn goal_token_budget_validates_range_and_goal_flag() {
        let mut options = parse(&["schedule", "create"]).options;
        options.goal_token_budget = Some("5000".to_string());
        let error = parse_goal_token_budget(&options).unwrap_err();
        assert_eq!(error.message, "--goal-token-budget requires --goal.");
        assert_eq!(error.exit_code, USAGE_ERROR);

        options.goal = true;
        assert_eq!(parse_goal_token_budget(&options).unwrap(), Some(5000));

        for bad in ["", "abc", "12.5", "-3", "999", "100000001"] {
            options.goal_token_budget = Some(bad.to_string());
            let error = parse_goal_token_budget(&options).unwrap_err();
            assert_eq!(
                error.message,
                "--goal-token-budget must be an integer from 1000 to 100000000."
            );
        }

        options.goal_token_budget = Some("1000".to_string());
        assert_eq!(parse_goal_token_budget(&options).unwrap(), Some(1000));
        options.goal_token_budget = None;
        assert_eq!(parse_goal_token_budget(&options).unwrap(), None);
    }

    // ── schedule formatting ────────────────────────────────────────

    /// 验证 goal 摘要的三种形态（no / yes / yes+budget）。
    #[test]
    fn format_goal_covers_disabled_enabled_and_budget() {
        assert_eq!(format_goal(&serde_json::json!({})), "goal:no");
        assert_eq!(
            format_goal(&serde_json::json!({ "goalEnabled": false })),
            "goal:no"
        );
        assert_eq!(
            format_goal(&serde_json::json!({ "goalEnabled": true })),
            "goal:yes"
        );
        assert_eq!(
            format_goal(&serde_json::json!({ "goalEnabled": true, "goalTokenBudget": 32000 })),
            "goal:yes budget:32000"
        );
    }

    /// 验证 schedule 摘要覆盖 daily / weekly / once / cron / 未知类型。
    #[test]
    fn format_schedule_covers_all_kinds() {
        assert_eq!(format_schedule(&serde_json::Value::Null), "unknown");
        assert_eq!(format_schedule(&serde_json::json!({})), "unknown");
        assert_eq!(
            format_schedule(&serde_json::json!({ "kind": "daily", "times": ["09:00", "17:30"] })),
            "daily 09:00,17:30"
        );
        assert_eq!(
            format_schedule(&serde_json::json!({ "kind": "daily" })),
            "daily"
        );
        assert_eq!(
            format_schedule(
                &serde_json::json!({ "kind": "weekly", "weekdays": [1, 3], "times": ["08:00"] })
            ),
            "weekly days:1,3 time:08:00"
        );
        assert_eq!(
            format_schedule(
                &serde_json::json!({ "kind": "once", "date": "2026-10-01", "time": "09:15" })
            ),
            "once 2026-10-01 09:15"
        );
        assert_eq!(
            format_schedule(&serde_json::json!({ "kind": "cron", "cron": "0 9 * * *" })),
            "cron 0 9 * * *"
        );
        assert_eq!(
            format_schedule(&serde_json::json!({ "kind": "interval" })),
            "interval"
        );
    }

    /// 验证未知 schedule 动作在端口解析之后报 usage 错。
    #[test]
    fn schedule_unknown_action_errors_after_port_resolution() {
        let _env = env_lock();
        let data_dir = temp_data_dir("schedule-unknown");
        let _guard = EnvGuard::install(&data_dir, false);
        let pid = std::process::id();
        let (port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, port, pid, 100.0, None);

        let parsed = parse(&["schedule", "bogus"]);
        let error = schedule_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(error.message, "Unknown schedule command 'bogus'.");
    }

    /// 验证缺 --task 时报 usage 错。
    #[test]
    fn schedule_missing_task_flag_is_a_usage_error() {
        let _env = env_lock();
        let data_dir = temp_data_dir("schedule-missing");
        let _guard = EnvGuard::install(&data_dir, false);
        let pid = std::process::id();
        let (port, _log) = spawn_fake_server(system_info_responder("cli", pid));
        write_instance_files(&data_dir, port, pid, 100.0, None);

        let parsed = parse(&["schedule", "run"]);
        let error = schedule_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(error.message, "Missing required --task.");
    }

    // ── session payloads and formatting ────────────────────────────

    /// 验证 --model 的 provider/model 格式校验与 trim 归一。
    #[test]
    fn validate_model_requires_provider_slash_model() {
        assert_eq!(validate_model(None).unwrap(), None);
        assert_eq!(validate_model(Some("  ")).unwrap(), None);
        assert_eq!(
            validate_model(Some(" anthropic/claude "))
                .unwrap()
                .as_deref(),
            Some("anthropic/claude")
        );
        for bad in ["claude", "/claude", "anthropic/"] {
            let error = validate_model(Some(bad)).unwrap_err();
            assert_eq!(error.message, "--model must be in provider/model format.");
            assert_eq!(error.exit_code, USAGE_ERROR);
        }
    }

    /// 验证 create payload 的目标互斥 / 必填与 goal → prompt 依赖。
    #[test]
    fn build_session_create_payload_validates_target_and_goal() {
        let options = parse(&["session", "create", "--prompt", "hi", "--goal"]).options;
        let error = build_session_create_payload(&options).unwrap_err();
        assert_eq!(error.message, "Missing required --dir or --project.");

        let options = parse(&["session", "create", "--dir", "/tmp", "--project", "p1"]).options;
        let error = build_session_create_payload(&options).unwrap_err();
        assert_eq!(error.message, "Provide only one of --dir or --project.");

        let options = parse(&["session", "create", "--dir", "/tmp", "--goal"]).options;
        let error = build_session_create_payload(&options).unwrap_err();
        assert_eq!(error.message, "--goal requires --prompt.");
    }

    /// 验证 create payload 的 worktree 嵌套结构与各选项映射，及 control 传输前的展平。
    #[test]
    fn build_session_create_payload_shapes_worktree_and_flags() {
        let options = parse(&[
            "session",
            "create",
            "--dir",
            "/repo",
            "--title",
            "My Title",
            "--worktree",
            "wt",
            "--branch",
            "feature",
            "--start-ref",
            "main",
            "--upstream",
            "--prompt",
            "do it",
            "--model",
            "anthropic/claude",
            "--agent",
            "build",
            "--variant",
            "fast",
            "--goal",
            "--goal-token-budget",
            "4000",
        ])
        .options;
        let payload = build_session_create_payload(&options).unwrap();
        assert_eq!(payload["directory"], "/repo");
        assert_eq!(payload["title"], "My Title");
        assert_eq!(payload["worktree"]["name"], "wt");
        assert_eq!(payload["worktree"]["branchName"], "feature");
        assert_eq!(payload["worktree"]["startRef"], "main");
        assert_eq!(payload["prompt"], "do it");
        assert_eq!(payload["model"], "anthropic/claude");
        assert_eq!(payload["agent"], "build");
        assert_eq!(payload["variant"], "fast");
        assert_eq!(payload["goal"], true);
        assert_eq!(payload["goalTokenBudget"], 4000);
        assert_eq!(payload["setUpstream"], true);
        assert!(!payload.contains_key("projectId"));

        // Flattening for the control transport.
        let mut control_payload = payload;
        let worktree = control_payload.remove("worktree").unwrap();
        control_payload.insert("worktree".to_string(), worktree["name"].clone());
        control_payload.insert("branch".to_string(), worktree["branchName"].clone());
        control_payload.insert("startRef".to_string(), worktree["startRef"].clone());
        assert_eq!(control_payload["worktree"], "wt");
        assert_eq!(control_payload["branch"], "feature");
        assert_eq!(control_payload["startRef"], "main");
    }

    /// 验证 send / fork payload 的必填项，以及 --message 仅 fork 可用。
    #[test]
    fn build_session_prompt_payload_validates_inputs() {
        let options = parse(&["session", "send", "--session", "s1", "--dir", "/repo"]).options;
        let error = build_session_prompt_payload(&options, "send").unwrap_err();
        assert_eq!(error.message, "Missing required --prompt.");

        let options = parse(&[
            "session",
            "send",
            "--session",
            "s1",
            "--dir",
            "/repo",
            "--prompt",
            "hi",
        ])
        .options;
        let payload = build_session_prompt_payload(&options, "send").unwrap();
        assert_eq!(payload["directory"], "/repo");
        assert_eq!(payload["prompt"], "hi");
        assert!(!payload.contains_key("messageId"));

        // --message is fork-only.
        let options = parse(&[
            "session",
            "send",
            "--session",
            "s1",
            "--dir",
            "/repo",
            "--prompt",
            "hi",
            "--message",
            "m1",
        ])
        .options;
        let error = build_session_prompt_payload(&options, "send").unwrap_err();
        assert_eq!(error.message, "--message is only valid for session fork.");

        let options = parse(&[
            "session",
            "fork",
            "--session",
            "s1",
            "--dir",
            "/repo",
            "--prompt",
            "hi",
            "--message",
            "m1",
        ])
        .options;
        let payload = build_session_prompt_payload(&options, "fork").unwrap();
        assert_eq!(payload["messageId"], "m1");

        // Missing --session / --dir.
        let options = parse(&["session", "send", "--prompt", "hi"]).options;
        let error = build_session_prompt_payload(&options, "send").unwrap_err();
        assert_eq!(error.message, "Missing required --session.");
        let options = parse(&["session", "send", "--session", "s1", "--prompt", "hi"]).options;
        let error = build_session_prompt_payload(&options, "send").unwrap_err();
        assert_eq!(error.message, "Missing required --dir.");
    }

    /// 验证 --timeout / --last-assistant 均要求搭配 --wait。
    #[test]
    fn session_action_wait_options_validate() {
        let options = parse(&["session", "send", "--timeout", "30"]).options;
        let error = validate_action_wait_options(&options, "send").unwrap_err();
        assert_eq!(error.message, "--timeout requires --wait.");

        let options = parse(&["session", "send", "--last-assistant"]).options;
        let error = validate_action_wait_options(&options, "send").unwrap_err();
        assert_eq!(
            error.message,
            "--last-assistant requires --wait for session send."
        );

        let options = parse(&["session", "send", "--wait", "--last-assistant"]).options;
        assert!(validate_action_wait_options(&options, "send").is_ok());
    }

    /// 验证 messages 各选项冲突在端口解析之前即报 usage 错。
    #[test]
    fn session_messages_flag_conflicts_error_before_port_resolution() {
        let base = ["session", "messages", "--session", "s1", "--dir", "/repo"];
        let cases = [
            (
                "--all conflicts with --last",
                {
                    let mut argv = base.to_vec();
                    argv.extend(["--all", "--last"]);
                    argv
                },
                "--all cannot be combined with --last or --limit.",
            ),
            (
                "--all conflicts with --limit",
                {
                    let mut argv = base.to_vec();
                    argv.extend(["--all", "--limit", "5"]);
                    argv
                },
                "--all cannot be combined with --last or --limit.",
            ),
            (
                "--last conflicts with --limit",
                {
                    let mut argv = base.to_vec();
                    argv.extend(["--last", "--limit", "5"]);
                    argv
                },
                "--last cannot be combined with --limit.",
            ),
            (
                "--last-assistant rejects non-assistant roles",
                {
                    let mut argv = base.to_vec();
                    argv.extend(["--last-assistant", "--role", "user"]);
                    argv
                },
                "--last-assistant cannot be combined with a non-assistant --role.",
            ),
            (
                "invalid role",
                {
                    let mut argv = base.to_vec();
                    argv.extend(["--role", "system"]);
                    argv
                },
                "--role must be one of: all, user, assistant.",
            ),
            (
                "timeout requires wait",
                {
                    let mut argv = base.to_vec();
                    argv.extend(["--timeout", "30"]);
                    argv
                },
                "--timeout requires --wait.",
            ),
        ];
        for (label, argv, expected) in cases {
            let parsed = parse(&argv);
            let error = session_command(&parsed, parsed.options.clone()).unwrap_err();
            assert_eq!(error.message, expected, "case: {label}");
            assert_eq!(error.exit_code, USAGE_ERROR);
        }
    }

    /// 验证未知 session 动作直接报错，不触发端口发现。
    #[test]
    fn session_unknown_action_errors_without_port_resolution() {
        let parsed = parse(&["session", "bogus"]);
        let error = session_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(error.message, "Unknown session command 'bogus'.");
    }

    /// 验证 status 动作对 --session / --dir 的必填校验。
    #[test]
    fn session_status_requires_target_flags() {
        let parsed = parse(&["session", "status"]);
        let error = session_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.message, "Missing required --session.");
        let parsed = parse(&["session", "status", "--session", "s1"]);
        let error = session_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.message, "Missing required --dir.");
    }

    /// 验证 session 行渲染的键名兼容、variant / status 拼接与缺省回退。
    #[test]
    fn format_session_line_includes_model_agent_variant_status() {
        let session = serde_json::json!({
            "title": "Fix login",
            "model": { "providerID": "anthropic", "id": "claude", "variant": "fast" },
            "agent": "build",
            "directory": "/repo",
            "status": { "type": "idle" },
        });
        assert_eq!(
            format_session_line(&session),
            "- `Fix login` — `anthropic/claude`, `build`, `fast` — status:idle — `/repo`"
        );

        let session = serde_json::json!({
            "slug": "weekly-review",
            "model": { "providerId": "openai", "modelId": "gpt" },
            "directory": "",
        });
        assert_eq!(
            format_session_line(&session),
            "- `weekly-review` — `openai/gpt`, `unknown-agent` — `unknown-directory`"
        );

        assert_eq!(
            format_session_line(&serde_json::json!({})),
            "- `untitled` — `unknown-model`, `unknown-agent` — `unknown-directory`"
        );
    }

    /// 验证消息渲染的角色标签、时间 / 模型附注与正文。
    #[test]
    fn format_text_message_renders_label_details_and_body() {
        let message = serde_json::json!({
            "role": "user",
            "createdAt": 1_758_000_000_123.0,
            "text": "hello there",
        });
        assert_eq!(
            format_text_message(&message),
            "**User**\n\n*2025-09-16T05:20:00.123Z*\n\nhello there"
        );

        let message = serde_json::json!({
            "role": "assistant",
            "model": "claude",
            "text": "hi",
        });
        assert_eq!(
            format_text_message(&message),
            "**Assistant**\n\n*claude*\n\nhi"
        );
    }

    // ── models / projects formatting ───────────────────────────────

    /// 验证 models 输出的默认行、收藏 / 最近列表与空态。
    #[test]
    fn format_models_output_renders_defaults_favorites_and_recent() {
        let settings = serde_json::json!({
            "defaultModel": "anthropic/claude",
            "defaultAgent": "build",
            "defaultVariant": "fast",
            "favoriteModels": [
                { "providerID": "anthropic", "modelID": "claude" },
                { "providerId": "openai", "id": "gpt" },
                { "providerID": "" },
            ],
            "recentModels": [],
        });
        assert_eq!(
            format_models_output(&settings),
            "Default: `anthropic/claude (fast)` / `build`\n\nFavorites:\n- `anthropic/claude`\n- `openai/gpt`\n\nRecent:\n- none\n"
        );

        assert_eq!(
            format_models_output(&serde_json::json!({})),
            "Default: `none` / `none`\n\nFavorites:\n- none\n\nRecent:\n- none\n"
        );
    }

    /// 验证项目行的 label—id—path 渲染。
    #[test]
    fn format_project_line_uses_label_id_path() {
        assert_eq!(
            format_project_line(&serde_json::json!({
                "label": "Web app",
                "id": "proj_1",
                "path": "/repo/web"
            })),
            "- `Web app` — `proj_1` — `/repo/web`"
        );
    }

    /// 验证未知 models / projects 动作直接报 usage 错，不触发端口发现。
    #[test]
    fn models_and_projects_unknown_actions_error_without_ports() {
        let parsed = parse(&["models", "bogus"]);
        let error = models_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.message, "Unknown models command 'bogus'.");

        let parsed = parse(&["projects", "bogus"]);
        let error = projects_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.message, "Unknown projects command 'bogus'.");
    }

    // ── control help ───────────────────────────────────────────────

    /// 验证 control 帮助文案与 JS 模板逐字一致（含末尾换行语义）。
    #[test]
    fn control_help_matches_the_js_template() {
        let text = control_help_text();
        assert!(text.starts_with("\n OMPChamber Control Commands\n"));
        assert!(text.contains("ompchamber session --help"));
        assert!(text.ends_with("ompchamber schedule --help\n"));
        // `console.log` adds one newline after the template's trailing one.
        let rendered = format!("{text}\n");
        assert!(rendered.ends_with("ompchamber schedule --help\n\n"));
    }

    /// 验证 control 未知动作报 usage 错。
    #[test]
    fn control_unknown_action_is_a_usage_error() {
        let parsed = parse(&["control", "bogus"]);
        let error = control_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(error.message, "Unknown control command 'bogus'.");
    }

    // ── completion scripts ─────────────────────────────────────────

    /// 验证 bash / zsh / fish 补全脚本覆盖全部命令与 tunnel 子命令。
    #[test]
    fn completion_scripts_cover_all_commands_and_tunnel_subcommands() {
        let bash = include_str!("help/completion-bash.txt");
        assert!(bash.contains("# Bash completion for ompchamber tunnel"));
        assert!(
            bash.contains(
                "commands=\"serve stop restart status schedule session models projects tunnel logs update\""
            )
        );
        assert!(bash.contains(
            "tunnel_commands=\"help providers ready doctor status start stop profile completion\""
        ));
        assert!(bash.contains("profile_commands=\"list show add remove\""));
        assert!(
            bash.trim_end()
                .ends_with("complete -F _ompchamber_tunnel ompchamber")
        );

        let zsh = include_str!("help/completion-zsh.txt");
        for command in [
            "serve", "stop", "restart", "status", "schedule", "session", "models", "projects",
            "tunnel", "logs", "update",
        ] {
            assert!(
                zsh.contains(&format!("'{command}:")),
                "zsh missing {command}"
            );
        }
        for sub in [
            "help",
            "providers",
            "ready",
            "doctor",
            "start",
            "stop",
            "profile",
            "completion",
        ] {
            assert!(
                zsh.contains(&format!("'{sub}:")),
                "zsh missing tunnel {sub}"
            );
        }
        assert!(zsh.contains("compdef _ompchamber ompchamber"));

        let fish = include_str!("help/completion-fish.txt");
        for command in [
            "serve", "stop", "restart", "status", "tunnel", "logs", "update",
        ] {
            assert!(
                fish.contains(&format!("-a '{command}'")),
                "fish missing {command}"
            );
        }
        for sub in [
            "help",
            "providers",
            "ready",
            "doctor",
            "start",
            "stop",
            "profile",
            "completion",
        ] {
            assert!(
                fish.contains(&format!("-a '{sub}'")),
                "fish missing tunnel {sub}"
            );
        }
        assert!(fish.contains("-l token-stdin"));
    }

    // ── number formatting ──────────────────────────────────────────

    /// 验证 JS 数字格式化（整值无小数部）与 number_option 的 JS Number 语义。
    #[test]
    fn js_number_drops_integral_fraction_and_number_option_parses() {
        assert_eq!(js_number(32000.0), "32000");
        assert_eq!(js_number(2.5), "2.5");
        assert_eq!(number_option(None), None);
        assert_eq!(number_option(Some("30")), Some(serde_json::json!(30.0)));
        assert_eq!(number_option(Some("")), Some(serde_json::json!(0.0)));
        assert_eq!(number_option(Some("abc")), Some(serde_json::Value::Null));
    }
}
