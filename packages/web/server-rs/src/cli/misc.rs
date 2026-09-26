//! CLI command group: status, logs, schedule, session, models, projects,
//! control (`bin/lib/commands-status.js`, `commands-logs.js`,
//! `commands-schedule.js`, `commands-session.js`, `commands-models.js`,
//! `commands-projects.js`, `cli-control.js`, `cli-goal.js`,
//! `cli-api-target.js`) plus the lifecycle-discovery and HTTP-client pieces
//! they need from `cli-lifecycle.js` / `cli-http.js` / `cli-log-files.js`.

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

fn block_on<F: Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => tokio::runtime::Runtime::new()
            .expect("failed to build tokio runtime for CLI request")
            .block_on(future),
    }
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::LazyLock<reqwest::Client> =
        std::sync::LazyLock::new(reqwest::Client::new);
    &CLIENT
}

// ── host / URL helpers (`cli-network.js` subset) ───────────────────────

fn normalize_probe_host(host: Option<&str>) -> Option<String> {
    host.map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string)
}

fn is_wildcard_probe_host(host: Option<&str>) -> bool {
    matches!(
        normalize_probe_host(host).as_deref(),
        Some("0.0.0.0") | Some("::") | Some("[::]")
    )
}

fn is_loopback_probe_host(host: Option<&str>) -> bool {
    matches!(
        normalize_probe_host(host).as_deref(),
        Some("127.0.0.1") | Some("localhost") | Some("::1") | Some("[::1]")
    )
}

fn is_concrete_probe_host(host: Option<&str>) -> bool {
    let normalized = normalize_probe_host(host);
    normalized.is_some() && !is_wildcard_probe_host(host) && !is_loopback_probe_host(host)
}

/// `resolveApiHost`: option > OMPCHAMBER_HOST env > 127.0.0.1, with wildcard
/// and bracket normalization.
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

fn format_host_for_url(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

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

static JSON_NULL: serde_json::Value = serde_json::Value::Null;

/// `value?.key` with a JSON null stand-in for missing fields.
fn field<'a>(value: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    value.get(key).unwrap_or(&JSON_NULL)
}

fn value_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(|v| v.as_str())
}

fn as_non_empty_str(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// JS number interpolation (`${n}`): integral values print without a
/// fractional part.
fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn json_number(value: f64) -> serde_json::Value {
    serde_json::Number::from_f64(value)
        .map(serde_json::Value::Number)
        .unwrap_or(serde_json::Value::Null)
}

/// `Number(options.timeout)`: "" → 0, non-numeric → null (JS NaN).
fn number_option(raw: Option<&str>) -> Option<serde_json::Value> {
    let raw = raw?;
    match raw.trim().parse::<f64>() {
        Ok(parsed) => Some(json_number(parsed)),
        Err(_) if raw.trim().is_empty() => Some(json_number(0.0)),
        Err(_) => Some(serde_json::Value::Null),
    }
}

/// `new Date(ms).toISOString()` (UTC, millisecond precision).
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
fn status_first_json(mut value: serde_json::Value) -> serde_json::Value {
    if let Some(map) = value.as_object_mut() {
        if !map.contains_key("status") {
            map.shift_insert(0, "status".to_string(), serde_json::json!("ok"));
        }
    }
    value
}

fn clack_intro(title: &str) {
    super::ui::intro(title);
}

fn clack_outro(text: &str) {
    super::ui::outro(text);
}

fn log_status(status: &str, message: &str, detail: Option<&str>) {
    // ui::log_status joins detail into the block with the bar prefix.
    super::ui::log_status(status, message, detail);
}

// ── system info / health probes (`cli-http.js` subset) ─────────────────

#[derive(Debug, Clone)]
struct SystemInfo {
    runtime: String,
    pid: Option<u32>,
}

fn has_ompchamber_runtime_info(info: &Option<SystemInfo>) -> bool {
    info.as_ref().is_some_and(|info| !info.runtime.is_empty())
}

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

fn fetch_system_info_from_port(port: u16, host_override: Option<&str>) -> Option<SystemInfo> {
    block_on(fetch_system_info_from_port_async(port, host_override))
}

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

fn is_server_health_ready(port: u16, timeout_ms: u64) -> bool {
    block_on(is_server_health_ready_async(port, timeout_ms))
}

// ── process identity (`cli-process.js` `getOmpchamberProcessState`) ────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessState {
    Dead,
    Unknown,
    Matched,
    Mismatched,
}

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

fn file_mtime_ms(path: &Path) -> f64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or_default()
}

// ── instance discovery (`cli-lifecycle.js` subset) ─────────────────────

#[derive(Debug, Clone)]
struct DiscoveredInstance {
    port: u16,
    pid: Option<u32>,
    instance_file_path: PathBuf,
    mtime_ms: f64,
    started_at: f64,
    launch_mode: String,
    runtime: String,
    source: &'static str,
}

/// `getSystemInfoProbeHosts`: candidate hosts in probe order, each flagging
/// whether a PID match is required to accept the answer.
fn get_system_info_probe_hosts(hosts: &[Option<String>]) -> Vec<(Option<String>, bool)> {
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

fn is_desktop_runtime_for_port(info: &SystemInfo, port: u16) -> bool {
    if info.runtime != "desktop" {
        return false;
    }
    let desktop_port = paths::read_desktop_local_port_from_settings();
    desktop_port.is_none() || desktop_port == Some(port)
}

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
fn discover_desktop_instance() -> Option<(u16, Option<u32>)> {
    let port = paths::read_desktop_local_port_from_settings()?;
    let info = fetch_system_info_from_port(port, None)?;
    if info.runtime != "desktop" {
        return None;
    }
    Some((port, info.pid))
}

/// `getLatestInstance`: newest startedAt, then pid-file mtime, then port.
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

#[derive(Debug, Clone, PartialEq)]
struct StatusEntry {
    runtime: String,
    port: u16,
    pid: Option<u32>,
    launch_mode: Option<String>,
    password_protected: Option<bool>,
}

fn password_protection_label(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

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

const DEFAULT_TAIL_LINES: u32 = 200;

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
struct FollowState {
    path: PathBuf,
    position: u64,
    remainder: String,
}

impl FollowState {
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

struct JsonResponse {
    status: u16,
    ok: bool,
    body: serde_json::Value,
}

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

fn get_desktop_local_auth_header(port: u16) -> Option<String> {
    if paths::read_desktop_local_port_from_settings() != Some(port) {
        return None;
    }
    let token = paths::read_desktop_local_client_token_from_settings();
    (!token.is_empty()).then(|| format!("Bearer {token}"))
}

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

const DEFAULT_WAIT_TIMEOUT_SECONDS: f64 = 600.0;
const WAIT_HTTP_TIMEOUT_BUFFER_MS: u64 = 30_000;
const WORKTREE_PROVISION_TIMEOUT_MS: u64 = 120_000;

/// `resolveControlTimeoutMs`.
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

const SCHEDULE_HELP: &str = "OMPChamber Schedule Commands\n\nUSAGE:\n  ompchamber schedule status [OPTIONS]\n  ompchamber schedule list (--project <projectId> | --dir <path>) [OPTIONS]\n  ompchamber schedule create (--project <projectId> | --dir <path>) --name <name> --prompt <prompt> --model <provider/model> (--daily <HH:mm> | --weekly <0,1,2> --time <HH:mm> | --once <YYYY-MM-DD> --time <HH:mm> | --cron <expr>) [OPTIONS]\n  ompchamber schedule run (--project <projectId> | --dir <path>) --task <taskId> [OPTIONS]\n  ompchamber schedule delete (--project <projectId> | --dir <path>) --task <taskId> [OPTIONS]\n  ompchamber schedule enable (--project <projectId> | --dir <path>) --task <taskId> [OPTIONS]\n  ompchamber schedule disable (--project <projectId> | --dir <path>) --task <taskId> [OPTIONS]\n\nOPTIONS:\n  --project <projectId>   Project id from ompchamber projects\n  --dir <path>            Resolve project by directory\n  -p, --port <port>       OMPChamber server port\n  --timezone <zone>       IANA timezone for created tasks\n  --agent <id>            Agent to use when running task\n  --variant <id>          Model variant to use when running task\n  --goal                  Continue the scheduled session toward a goal\n  --goal-token-budget <n> Goal token budget (1000-100000000; requires --goal)\n  --disabled              Create task disabled\n  --json                  Output machine-readable JSON\n  -q, --quiet             Print concise output\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
pub fn schedule_help_text() -> &'static str {
    SCHEDULE_HELP
}

fn assert_required(value: Option<&str>, flag_name: &str) -> Result<String, CliError> {
    as_non_empty_str(value).ok_or_else(|| CliError::usage(format!("Missing required {flag_name}.")))
}

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

const SESSION_HELP: &str = "OMPChamber Session Commands\n\nUSAGE:\n  ompchamber session list [--dir <path>] [--limit <count>] [--with-status] [OPTIONS]\n  ompchamber session create --dir <path> [--title <title>] [--wait] [OPTIONS]\n  ompchamber session create --project <projectId> [--title <title>] [--wait] [OPTIONS]\n  ompchamber session send --session <id> --dir <path> --prompt <text> [--wait] [OPTIONS]\n  ompchamber session fork --session <id> --dir <path> --prompt <text> [--message <id>] [--wait] [OPTIONS]\n  ompchamber session status --session <id> --dir <path> [OPTIONS]\n  ompchamber session messages --session <id> --dir <path> [--wait] [OPTIONS]\n\nLIST OPTIONS:\n  --dir <path>            Filter sessions by directory\n  --limit <count>         Maximum sessions to show (default: 10)\n  --all                   Include archived sessions\n  --with-status           Include authoritative idle/busy/retry status\n\nACTION OPTIONS:\n  --session <id>          Source or target session id\n  --dir <path>            Authoritative session directory\n  --prompt <text>         Prompt to send to the session\n  --message <id>          Fork from this message (fork only; default: latest)\n  --model <provider/model>  Model for the prompt (defaults to configured selection)\n  --agent <id>            Agent for the prompt (defaults to configured selection)\n  --variant <id>          Model variant for the prompt\n  --goal                  Run the prompt as a new goal\n  --goal-token-budget <n> Goal token budget (1000-100000000; requires --goal)\n  --wait                  Wait for the dispatched activity to become idle\n  --last-assistant        Include the last assistant text after waiting\n  --timeout <seconds>     Wait timeout in seconds (default: 600, max: 86400)\n\nCREATE OPTIONS:\n  --worktree <name>       Create a git worktree before creating the session\n  --branch <name>         Branch name for --worktree\n  --start-ref, --base <ref>  Start ref for --worktree\n  --upstream              Set upstream for the worktree branch\n  --no-upstream           Do not set upstream for the worktree branch\n  --name <title>          Alias for --title\n\nSTATUS/MESSAGES OPTIONS:\n  --last                  Return only the latest text-bearing message\n  --last-assistant        Shorthand for --last --role assistant\n  --limit <count>         Maximum text messages to return (default: 10)\n  --all                   Return all text-bearing messages\n  --role <role>           Filter messages: all, user, assistant\n\nOUTPUT OPTIONS:\n  -p, --port <port>       OMPChamber server port\n  --json                  Output machine-readable JSON\n  -q, --quiet             Print compact output\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
pub fn session_help_text() -> &'static str {
    SESSION_HELP
}

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

fn normalize_limit(value: Option<u32>, fallback: u32) -> Result<u32, CliError> {
    match value {
        None => Ok(fallback),
        Some(parsed) if parsed >= 1 => Ok(parsed),
        Some(_) => Err(CliError::usage(
            "Invalid limit value. Provide a positive integer.",
        )),
    }
}

fn assert_session_target(options: &Options) -> Result<(String, String), CliError> {
    let session_id = as_non_empty_str(options.session.as_deref())
        .ok_or_else(|| CliError::usage("Missing required --session."))?;
    let directory = as_non_empty_str(options.directory.as_deref())
        .ok_or_else(|| CliError::usage("Missing required --dir."))?;
    Ok((session_id, directory))
}

fn normalize_message_role(value: Option<&str>) -> Result<String, CliError> {
    let role = as_non_empty_str(value).unwrap_or_else(|| "all".to_string());
    if !matches!(role.as_str(), "all" | "user" | "assistant") {
        return Err(CliError::usage(
            "--role must be one of: all, user, assistant.",
        ));
    }
    Ok(role)
}

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

const MODELS_HELP: &str = "OMPChamber Models Commands\n\nUSAGE:\n  ompchamber models [OPTIONS]\n\nOUTPUT OPTIONS:\n  -p, --port <port>       OMPChamber server port\n  --json                  Output machine-readable JSON\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
pub fn models_help_text() -> &'static str {
    MODELS_HELP
}

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

const PROJECTS_HELP: &str = "OMPChamber Projects Commands\n\nUSAGE:\n  ompchamber projects [OPTIONS]\n\nOUTPUT OPTIONS:\n  -p, --port <port>       OMPChamber server port\n  --json                  Output machine-readable JSON\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
pub fn projects_help_text() -> &'static str {
    PROJECTS_HELP
}

fn format_project_line(project: &serde_json::Value) -> String {
    format!(
        "- `{}` — `{}` — `{}`",
        value_str(project, "label").unwrap_or_default(),
        value_str(project, "id").unwrap_or_default(),
        value_str(project, "path").unwrap_or_default()
    )
}

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

const CONTROL_HELP: &str = "\n OMPChamber Control Commands\n\nUSAGE:\n  ompchamber <COMMAND> [OPTIONS]\n\nCOMMANDS:\n  status                         Show running OMPChamber runtimes\n  session                        Create, inspect, and read sessions\n  models                         Show default and favorite models\n  projects                       Show configured projects and IDs\n  schedule                       Manage scheduled tasks\n  tunnel                         Inspect tunnel status/readiness\n  logs                           Tail logs for CLI-managed runtimes\n\nDETAILED HELP:\n  ompchamber session --help     Show session creation, status, and message options\n  ompchamber models --help      Show model defaults and favorites help\n  ompchamber projects --help    Show project list help\n  ompchamber schedule --help    Show scheduled task actions and schedule options\n  ompchamber tunnel help        Show tunnel lifecycle/status commands\n  ompchamber status --help      Show runtime status options\n\nCOMMON OPTIONS:\n  --json                         Output machine-readable JSON\n  -q, --quiet                    Print minimal output\n  -p, --port <port>              Target a specific OMPChamber runtime\n  --ui-password <password>       Authenticate to a password-protected runtime\n\nEXAMPLES:\n  ompchamber status\n  ompchamber models\n  ompchamber projects\n  ompchamber session --help\n  ompchamber schedule --help\n";

/// Exposed for the shared `--help` dispatch (`mod.rs`).
pub fn control_help_text() -> &'static str {
    CONTROL_HELP
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn env_mutex() -> &'static std::sync::Mutex<()> {
        &crate::cli::TEST_ENV_MUTEX
    }

    /// Poisoning-tolerant lock: one failing test must not cascade into every
    /// other env-guarded test.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        env_mutex()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_var(key: &str, value: &str) {
        // SAFETY: every test that touches process environment variables holds
        // ENV_LOCK for its whole body, serializing access across threads.
        unsafe { std::env::set_var(key, value) }
    }

    fn remove_var(key: &str) {
        // SAFETY: see `set_var`.
        unsafe { std::env::remove_var(key) }
    }

    /// Points the CLI at a temp data dir; `isolate_host` reroutes env-derived
    /// probe hosts to 127.0.0.2 so DEFAULT_PORT health checks cannot reach a
    /// real server the developer may be running on 127.0.0.1.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        /// Caller must already hold `ENV_LOCK` (tests lock it for their whole
        /// body) — std Mutex is not reentrant.
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

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => set_var(key, &value),
                    None => remove_var(key),
                }
            }
        }
    }

    fn temp_data_dir(label: &str) -> PathBuf {
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

    type Responder = Arc<dyn Fn(&str) -> (u16, String) + Send + Sync>;

    /// Minimal keep-alive HTTP/1.1 server; GETs and JSON POSTs both served.
    /// Recorded request: (method, path, body).
    type RequestLog = Arc<std::sync::Mutex<Vec<(String, String, String)>>>;

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

    fn system_info_responder(runtime: &str, pid: u32) -> Responder {
        let runtime = runtime.to_string();
        Arc::new(move |path| match path {
            "/api/system/info" => (200, format!("{{\"runtime\":\"{runtime}\",\"pid\":{pid}}}")),
            "/health" => (200, "ok".to_string()),
            _ => (404, "{}".to_string()),
        })
    }

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

    fn write_log_file(data_dir: &Path, port: u16, contents: &str) {
        let logs_dir = data_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).expect("create logs dir");
        std::fs::write(logs_dir.join(format!("ompchamber-{port}.log")), contents)
            .expect("write log file");
    }

    fn parse(argv: &[&str]) -> Parsed {
        let owned = argv.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        args::parse_args(&owned).expect("parse args")
    }

    // ── log files ──────────────────────────────────────────────────

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

    #[test]
    fn resolve_target_port_explicit_short_circuits() {
        let _env = env_lock();
        let _guard = EnvGuard::install(&temp_data_dir("target-explicit"), false);
        let parsed = parse(&["schedule", "status", "-p", "4242"]);
        assert_eq!(resolve_target_port(&parsed.options).unwrap(), 4242);
    }

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

    #[test]
    fn logs_errors_when_no_instance_running() {
        let _env = env_lock();
        let _guard = EnvGuard::install(&temp_data_dir("logs-none"), false);
        let options = parse(&["logs", "--no-follow"]).options;
        let error = resolve_log_targets(&options).unwrap_err();
        assert_eq!(error.exit_code, GENERAL_ERROR);
        assert_eq!(error.message, "No running OMPChamber instance found.");
    }

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

    #[test]
    fn session_unknown_action_errors_without_port_resolution() {
        let parsed = parse(&["session", "bogus"]);
        let error = session_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(error.message, "Unknown session command 'bogus'.");
    }

    #[test]
    fn session_status_requires_target_flags() {
        let parsed = parse(&["session", "status"]);
        let error = session_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.message, "Missing required --session.");
        let parsed = parse(&["session", "status", "--session", "s1"]);
        let error = session_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.message, "Missing required --dir.");
    }

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

    #[test]
    fn control_unknown_action_is_a_usage_error() {
        let parsed = parse(&["control", "bogus"]);
        let error = control_command(&parsed, parsed.options.clone()).unwrap_err();
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(error.message, "Unknown control command 'bogus'.");
    }

    // ── completion scripts ─────────────────────────────────────────

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
