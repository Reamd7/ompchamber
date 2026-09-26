//! Port of `bin/lib/commands-update.js` + `bin/lib/commands-connect-url.js`,
//! plus the `cli-lifecycle.js` instance-discovery subset and the
//! `cli-http.js` / `cli-network.js` helpers they consume.
//!
//! JS→Rust notes:
//! - `ompchamber update` dynamic-imports `server/lib/package-manager.js`;
//!   the Rust port calls `crate::package_manager` directly through the
//!   [`UpdateBackend`] seam (tests inject fakes exactly like the vitest
//!   module mocks).
//! - mod.rs dispatches these commands synchronously, so the pub entries
//!   bridge into the ambient tokio runtime via `block_in_place`.
//! - clack spinners do not exist here: `createSpinner(options)` is only
//!   non-null on an interactive TTY, and every `updateSpin?.x` call is a
//!   no-op in that case — the Rust port renders the `showOutput` branches
//!   (the `!updateSpin` complements) as plain lines.
//! - `--connect-ttl`/`--session-ttl` are tunnel-flow flags: the JS
//!   connect-url command never reads them (the pairing TTL is the store's
//!   10-minute default), and neither does this port.
//! - `--qr`: the qrcode crate is unavailable; the JS `displayTunnelQrCode`
//!   failure path is reproduced — an honest warning on stderr, stdout
//!   contract unchanged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::args::{DEFAULT_PORT, Options, Parsed};
use super::process;
use super::{CliError, GENERAL_ERROR, USAGE_ERROR};
use crate::package_manager::{
    CheckForUpdatesOptions, ExecuteUpdateOptions, UpdateExecution, UpdateInfo,
};

// ── output plumbing ───────────────────────────────────────────────────

/// Line destination: clack/quiet output goes to stdout, warnings and the
/// QR fallback note to stderr.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutKind {
    Out,
    Err,
}

pub(crate) type Emit<'a> = &'a mut dyn FnMut(OutKind, &str);

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `cli-output.js printJson` prepends the normalized `status` field.
fn with_status(payload: Value) -> Value {
    let mut out = json!({ "status": "ok" });
    merge_object(&mut out, &payload);
    out
}

fn merge_object(target: &mut Value, source: &Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn block_on_cli<F: Future>(future: F) -> F::Output {
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| handle.block_on(future))
}

fn probe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ── cli-network.js subset (local copies; network.rs owns the serve set) ──

/// `resolveApiHost`: a bind host mapped onto a connectable destination.
fn resolve_api_host_with(host_override: Option<&str>, env_host: Option<&str>) -> String {
    let configured = host_override
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .or_else(|| {
            env_host
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "127.0.0.1".to_string());
    if configured.is_empty() {
        return "127.0.0.1".to_string();
    }
    // Wildcard bind hosts are not valid destination hosts.
    if configured == "0.0.0.0" {
        return "127.0.0.1".to_string();
    }
    if configured == "::" || configured == "[::]" {
        return "::1".to_string();
    }
    // Strip brackets if user provided [::1].
    if configured.starts_with('[') && configured.ends_with(']') {
        return configured[1..configured.len() - 1].to_string();
    }
    configured
}

fn resolve_api_host(host_override: Option<&str>) -> String {
    resolve_api_host_with(
        host_override,
        std::env::var("OMPCHAMBER_HOST").ok().as_deref(),
    )
}

/// `formatHostForUrl`: bracket IPv6 for URL usage.
fn format_host_for_url(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn build_local_url_with(
    port: u16,
    endpoint: &str,
    host_override: Option<&str>,
    env_host: Option<&str>,
) -> String {
    let host = format_host_for_url(&resolve_api_host_with(host_override, env_host));
    let path_part = if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    };
    format!("http://{host}:{port}{path_part}")
}

// Browser-unsafe ports (Fetch/Chromium restricted ports).
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
fn assert_safe_browser_port_throwing(port: u16, context: &str) -> Result<(), CliError> {
    if !is_unsafe_browser_port(port) {
        return Ok(());
    }
    Err(CliError::new(
        format!(
            "{context} cannot use port {port}. Port {port} is browser-unsafe (ERR_UNSAFE_PORT) and is not supported for OMPChamber UI at {}. Use a safe port such as 3000, 5173, 8080, or a high ephemeral port.",
            build_local_url_with(port, "/", None, None)
        ),
        USAGE_ERROR,
    ))
}

/// `detectLanIPv4Address`: the UDP-connect routing trick, then interface
/// enumeration (ifconfig/ip/ipconfig — no if-addrs crate available).
fn detect_lan_ipv4_address() -> Option<String> {
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                let ip = addr.ip().to_string();
                if ip != "0.0.0.0" && !ip.starts_with("127.") {
                    return Some(ip);
                }
            }
        }
    }
    fallback_interface_scan()
}

fn fallback_interface_scan() -> Option<String> {
    let output = if cfg!(target_os = "macos") {
        std::process::Command::new("ifconfig").output().ok()?
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("ip")
            .args(["-4", "-o", "addr"])
            .output()
            .ok()?
    } else {
        std::process::Command::new("ipconfig").output().ok()?
    };
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        while let Some(part) = parts.next() {
            if part == "inet" {
                if let Some(address) = parts.next() {
                    let address = address.split('%').next().unwrap_or(address);
                    if address != "127.0.0.1" && !address.contains(':') && !address.is_empty() {
                        return Some(address.to_string());
                    }
                }
            }
        }
    }
    None
}

// ── URL shaping (commands-connect-url.js) ─────────────────────────────

/// `normalizeServerUrlForConnection`: http(s) URL, hash dropped, trailing
/// slashes stripped; None for anything else.
pub(crate) fn normalize_server_url_for_connection(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut parsed = url::Url::parse(trimmed).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    parsed.set_fragment(None);
    Some(parsed.to_string().trim_end_matches('/').to_string())
}

fn is_wildcard_bind_host(host: &str) -> bool {
    host == "0.0.0.0" || host == "::" || host == "[::]"
}

fn is_wildcard_probe_host(host: Option<&str>) -> bool {
    matches!(normalize_probe_host(host), Some(h) if h == "0.0.0.0" || h == "::" || h == "[::]")
}

fn is_loopback_probe_host(host: Option<&str>) -> bool {
    matches!(
        normalize_probe_host(host),
        Some(h) if h == "127.0.0.1" || h == "localhost" || h == "::1" || h == "[::1]"
    )
}

fn normalize_probe_host(host: Option<&str>) -> Option<String> {
    host.map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// `isLoopbackServerUrl`.
pub(crate) fn is_loopback_server_url(server_url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(server_url) else {
        return false;
    };
    let hostname = parsed
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    hostname == "127.0.0.1" || hostname == "localhost" || hostname == "::1"
}

/// Pure core of `resolveConnectUrlServerUrl` (env/LAN/stored-host injected
/// for testability).
fn resolve_server_url_core(
    port: u16,
    host_override: Option<&str>,
    env_host: Option<&str>,
    stored_host: Option<&str>,
    lan_address: Option<&str>,
) -> (String, &'static str) {
    // JS folds the stored instance host in only when neither the flag nor
    // OMPCHAMBER_HOST provides one.
    let host_override = if host_override.is_none() && env_host.is_none() {
        stored_host.map(str::trim).filter(|h| !h.is_empty())
    } else {
        host_override
    };
    let bind_host = host_override
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
        .or_else(|| {
            env_host
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "127.0.0.1".to_string());

    // A host that's already a full http(s) URL is a public/server URL, not
    // a bind address — use it directly.
    if let Some(server_url) = normalize_server_url_for_connection(&bind_host) {
        return (server_url, "configured-host");
    }
    if !is_wildcard_bind_host(&bind_host) {
        let url = build_local_url_with(port, "/", host_override, env_host);
        return (url.trim_end_matches('/').to_string(), "configured-host");
    }
    match lan_address {
        Some(lan) => (
            format!("http://{}:{port}", format_host_for_url(lan)),
            "lan-detected",
        ),
        None => {
            let url = build_local_url_with(port, "/", None, env_host);
            (url.trim_end_matches('/').to_string(), "loopback-fallback")
        }
    }
}

async fn resolve_connect_url_server_url(
    port: u16,
    host_override: Option<&str>,
    data_dir: &Path,
) -> (String, &'static str) {
    let env_host = std::env::var("OMPCHAMBER_HOST")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let mut stored_host = None;
    if host_override.is_none() && env_host.is_none() {
        let instance_file = data_dir.join("run").join(format!("ompchamber-{port}.json"));
        if let Some(stored) = process::read_instance_options(&instance_file) {
            stored_host = stored
                .host
                .map(|host| host.trim().to_string())
                .filter(|h| !h.is_empty());
        }
    }
    let lan = detect_lan_ipv4_address();
    resolve_server_url_core(
        port,
        host_override.map(str::trim).filter(|h| !h.is_empty()),
        env_host.as_deref(),
        stored_host.as_deref(),
        lan.as_deref(),
    )
}

// ── cli-http.js subset ────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub(crate) struct SystemInfo {
    pub runtime: String,
    pub pid: Option<u32>,
}

async fn fetch_system_info_from_port(
    client: &reqwest::Client,
    port: u16,
    host_override: Option<&str>,
) -> Option<SystemInfo> {
    if port == 0 {
        return None;
    }
    let url = build_local_url_with(
        port,
        "/api/system/info",
        host_override,
        std::env::var("OMPCHAMBER_HOST").ok().as_deref(),
    );
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    let runtime = body.get("runtime")?.as_str()?.to_string();
    let pid = body
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok());
    Some(SystemInfo { runtime, pid })
}

/// `requestServerShutdown`: best-effort POST; false on any failure.
async fn request_server_shutdown(
    client: &reqwest::Client,
    port: u16,
    host_override: Option<&str>,
) -> bool {
    if port == 0 {
        return false;
    }
    let url = build_local_url_with(
        port,
        "/api/system/shutdown",
        host_override,
        std::env::var("OMPCHAMBER_HOST").ok().as_deref(),
    );
    match client.post(url).send().await {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

// ── cli-lifecycle.js discovery subset ─────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessState {
    Dead,
    Matched,
    Mismatched,
    Unknown,
}

fn ompchamber_process_state(pid: u32) -> ProcessState {
    if pid == 0 || !process::is_process_running(pid) {
        return ProcessState::Dead;
    }
    // Where identity can't be determined, fall back to liveness so there
    // are no false negatives (issue #1721 semantics).
    match process::read_process_cmdline(pid) {
        None => ProcessState::Unknown,
        Some(cmdline) if process::is_ompchamber_cmdline(&cmdline) => ProcessState::Matched,
        Some(_) => ProcessState::Mismatched,
    }
}

/// `getSystemInfoProbeHosts`: ordered, deduped probe hosts; the fallback
/// entries must pid-match when a concrete authoritative host was supplied.
fn get_system_info_probe_hosts(input_hosts: &[Option<String>]) -> Vec<(Option<String>, bool)> {
    let is_concrete = |host: &Option<String>| {
        let normalized = normalize_probe_host(host.as_deref());
        normalized.is_some()
            && !is_wildcard_probe_host(host.as_deref())
            && !is_loopback_probe_host(host.as_deref())
    };
    let has_concrete_authoritative_host = input_hosts.iter().any(is_concrete);
    let mut out: Vec<(Option<String>, bool)> = Vec::new();
    let push =
        |host: Option<String>, requires_pid_match: bool, out: &mut Vec<(Option<String>, bool)>| {
            let key = resolve_api_host(host.as_deref());
            if !out
                .iter()
                .any(|(existing, _)| resolve_api_host(existing.as_deref()) == key)
            {
                out.push((host, requires_pid_match));
            }
        };
    for host in input_hosts {
        if normalize_probe_host(host.as_deref()).is_some() {
            push(host.clone(), false, &mut out);
        }
    }
    push(None, has_concrete_authoritative_host, &mut out);
    push(
        Some("127.0.0.1".to_string()),
        has_concrete_authoritative_host,
        &mut out,
    );
    out
}

async fn fetch_system_info_from_port_candidates(
    client: &reqwest::Client,
    port: u16,
    hosts: &[(Option<String>, bool)],
    expected_pid: Option<u32>,
) -> (Option<SystemInfo>, Option<String>) {
    for (host, requires_pid_match) in hosts {
        let info = fetch_system_info_from_port(client, port, host.as_deref()).await;
        if let Some(info) = info.filter(|info| !info.runtime.is_empty()) {
            if *requires_pid_match && info.pid != expected_pid {
                continue;
            }
            return (Some(info), host.clone());
        }
    }
    (None, None)
}

#[derive(Clone, Debug)]
pub(crate) struct DiscoveredInstance {
    pub port: u16,
    pub pid: Option<u32>,
    pub pid_file_path: PathBuf,
    pub stored: Option<process::InstanceOptions>,
    pub confirmed_host: Option<String>,
}

/// `discoverRunningInstances` (registry + probe, with stale-file cleanup).
pub(crate) async fn discover_running_instances(
    client: &reqwest::Client,
    data_dir: &Path,
) -> Vec<DiscoveredInstance> {
    let run_dir = data_dir.join("run");
    let Ok(entries) = std::fs::read_dir(&run_dir) else {
        return Vec::new();
    };
    let mut instances = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(port_str) = name
            .strip_prefix("ompchamber-")
            .and_then(|rest| rest.strip_suffix(".pid"))
        else {
            continue;
        };
        let Ok(port) = port_str.parse::<u16>() else {
            continue;
        };
        let pid_file_path = run_dir.join(&name);
        let instance_file_path = run_dir.join(format!("ompchamber-{port}.json"));
        let Some(pid) = process::read_pid_file(&pid_file_path) else {
            process::remove_pid_file(&pid_file_path);
            process::remove_instance_file(&instance_file_path);
            continue;
        };
        let stored = process::read_instance_options(&instance_file_path);
        let process_state = ompchamber_process_state(pid);
        if process_state == ProcessState::Dead {
            process::remove_pid_file(&pid_file_path);
            process::remove_instance_file(&instance_file_path);
            continue;
        }
        // A live PID-file is only the right instance if the recorded port
        // also confirms OMPChamber (a recycled PID from another OMPChamber
        // process on a different port would otherwise match).
        let stored_host = stored.as_ref().and_then(|options| options.host.clone());
        let (live_info, confirmed_host) = fetch_system_info_from_port_candidates(
            client,
            port,
            &get_system_info_probe_hosts(&[stored_host.clone()]),
            Some(pid),
        )
        .await;
        let Some(live_info) = live_info else {
            if process_state == ProcessState::Mismatched {
                process::remove_pid_file(&pid_file_path);
                process::remove_instance_file(&instance_file_path);
            }
            continue;
        };
        if live_info.runtime == "desktop" {
            process::remove_pid_file(&pid_file_path);
            process::remove_instance_file(&instance_file_path);
            continue;
        }
        instances.push(DiscoveredInstance {
            port,
            pid: live_info.pid.or(if process_state == ProcessState::Matched {
                Some(pid)
            } else {
                None
            }),
            pid_file_path,
            stored,
            confirmed_host: confirmed_host
                .filter(|host| !host.is_empty())
                .or_else(|| stored_host.clone().filter(|host| !host.is_empty())),
        });
    }
    instances.sort_by_key(|instance| instance.port);
    instances
}

// ── serve launcher seam (commands-update.js / connect-url call serve) ──

#[derive(Clone, Debug, Default)]
pub(crate) struct ServeRequest {
    pub port: u16,
    pub host: Option<String>,
    pub ui_password: Option<String>,
    pub api_only: bool,
    pub quiet: bool,
    pub suppress_quiet_output: bool,
    pub suppress_startup_summary: bool,
}

pub(crate) type BoxFut<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub(crate) type ServeLauncher =
    Arc<dyn Fn(ServeRequest) -> BoxFut<'static, Result<(), CliError>> + Send + Sync>;

fn serve_parsed() -> Parsed {
    Parsed {
        command: "serve".to_string(),
        subcommand: None,
        tunnel_action: None,
        startup_action: None,
        schedule_action: None,
        session_action: None,
        control_action: None,
        options: Options::default(),
        removed_flag_errors: Vec::new(),
        help_requested: false,
        version_requested: false,
        positionals: Vec::new(),
    }
}

fn production_serve_launcher() -> ServeLauncher {
    Arc::new(|request: ServeRequest| {
        Box::pin(async move {
            let options = Options {
                port: Some(request.port),
                explicit_port: true,
                host: request.host.clone(),
                ui_password: request.ui_password.clone(),
                // A plain value wins; no value never generates here (JS
                // connect-url/update pass uiPassword without the explicit
                // flag).
                explicit_ui_password: false,
                api_only: request.api_only,
                quiet: request.quiet,
                suppress_ui_password_warning: true,
                suppress_unsafe_port_warning: true,
                suppress_quiet_output: request.suppress_quiet_output,
                suppress_startup_summary: request.suppress_startup_summary,
                ..Options::default()
            };
            super::serve::command(&serve_parsed(), options).await
        })
    })
}

// ── update command (commands-update.js) ───────────────────────────────

/// The package-manager surface `createUpdateCommand` dynamic-imports.
pub(crate) trait UpdateBackend {
    fn current_version(&self) -> String;
    fn check_for_updates(&self) -> BoxFut<'_, UpdateInfo>;
    fn detect_package_manager(&self) -> BoxFut<'_, String>;
    fn execute_update<'a>(
        &'a self,
        pm: &'a str,
        version: Option<&'a str>,
        silent: bool,
    ) -> BoxFut<'a, UpdateExecution>;
}

impl UpdateBackend for crate::package_manager::PackageManagerRuntime {
    fn current_version(&self) -> String {
        crate::package_manager::PackageManagerRuntime::get_current_version(self)
    }
    fn check_for_updates(&self) -> BoxFut<'_, UpdateInfo> {
        Box::pin(
            crate::package_manager::PackageManagerRuntime::check_for_updates(
                self,
                CheckForUpdatesOptions::default(),
            ),
        )
    }
    fn detect_package_manager(&self) -> BoxFut<'_, String> {
        Box::pin(crate::package_manager::PackageManagerRuntime::detect_package_manager(self))
    }
    fn execute_update<'a>(
        &'a self,
        pm: &'a str,
        version: Option<&'a str>,
        silent: bool,
    ) -> BoxFut<'a, UpdateExecution> {
        Box::pin(
            crate::package_manager::PackageManagerRuntime::execute_update(
                self,
                Some(pm),
                ExecuteUpdateOptions {
                    version: version.map(str::to_string),
                    silent,
                },
            ),
        )
    }
}

fn update_up_to_date_json(current_version: &str, latest_version: &str) -> Value {
    with_status(json!({
        "currentVersion": current_version,
        "latestVersion": latest_version,
        "updated": false,
    }))
}

fn update_complete_json(
    current_version: &str,
    latest_version: &str,
    restarted_count: usize,
) -> Value {
    with_status(json!({
        "currentVersion": current_version,
        "latestVersion": latest_version,
        "updated": true,
        "restartedCount": restarted_count,
    }))
}

async fn wait_for_process_exit(pid: u32, timeout_ms: u64) -> bool {
    if pid == 0 {
        return true;
    }
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if !process::is_process_running(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// `stopInstanceProcess`: bounded graceful shutdown wait, then tree kill.
async fn stop_instance_process(pid: u32, shutdown_wait_ms: u64, graceful_ms: u64, force_ms: u64) {
    if pid == 0 {
        return;
    }
    if wait_for_process_exit(pid, shutdown_wait_ms).await {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        process::terminate_process_tree(pid, graceful_ms, force_ms)
    })
    .await;
}

pub(crate) async fn update_command_flow(
    options: &Options,
    backend: &dyn UpdateBackend,
    data_dir: &Path,
    serve: &ServeLauncher,
    client: &reqwest::Client,
    emit: Emit<'_>,
) -> Result<(), CliError> {
    let show_output = !options.json && !options.quiet;
    let running = discover_running_instances(client, data_dir).await;
    let current_version = backend.current_version();

    if show_output {
        emit(OutKind::Out, "OMPChamber Update");
        emit(OutKind::Out, &format!("current version: {current_version}"));
    }

    let update_info = backend.check_for_updates().await;
    if let Some(error) = update_info.error.clone().filter(|error| !error.is_empty()) {
        if show_output {
            emit(OutKind::Out, "update failed");
        }
        return Err(CliError::new(error, GENERAL_ERROR));
    }
    if !update_info.available {
        let latest = update_info
            .version
            .clone()
            .filter(|version| !version.is_empty())
            .unwrap_or_else(|| current_version.clone());
        if options.json {
            emit(
                OutKind::Out,
                &pretty(&update_up_to_date_json(&current_version, &latest)),
            );
            return Ok(());
        }
        if show_output {
            emit(OutKind::Out, "you are running the latest version");
            emit(OutKind::Out, "no update needed");
        } else if options.quiet {
            emit(OutKind::Out, &format!("up-to-date {current_version}"));
        }
        return Ok(());
    }

    let from = {
        let current = update_info.current_version.clone();
        if current.is_empty() {
            current_version.clone()
        } else {
            current
        }
    };
    let to = update_info
        .version
        .clone()
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "latest".to_string());
    if show_output {
        emit(OutKind::Out, &format!("updating {from} -> {to}"));
    }

    // Stop running instances; per-instance failures are swallowed (JS try/catch).
    for instance in &running {
        let requested =
            request_server_shutdown(client, instance.port, instance.confirmed_host.as_deref())
                .await;
        if let Some(pid) = instance.pid {
            stop_instance_process(pid, if requested { 5000 } else { 0 }, 2500, 3000).await;
        }
        process::remove_pid_file(&instance.pid_file_path);
    }

    let pm = backend.detect_package_manager().await;
    let silent = options.json || options.quiet;
    let result = backend
        .execute_update(&pm, update_info.version.as_deref(), silent)
        .await;
    if !result.success {
        if show_output {
            emit(OutKind::Out, "update failed");
        }
        let exit_code = result
            .exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "null".to_string());
        return Err(CliError::new(
            format!("Update failed with exit code {exit_code}"),
            GENERAL_ERROR,
        ));
    }

    // Restart the instances that were stopped, with their stored options.
    for instance in &running {
        let stored = instance.stored.clone().unwrap_or(process::InstanceOptions {
            port: instance.port,
            host: None,
            launch_mode: String::new(),
            ui_password: None,
            has_ui_password: false,
            api_only: false,
            started_at: 0.0,
        });
        let port = if stored.port != 0 {
            stored.port
        } else {
            instance.port
        };
        serve(ServeRequest {
            port,
            host: stored.host.clone(),
            ui_password: stored.ui_password.clone(),
            quiet: true,
            suppress_startup_summary: true,
            ..ServeRequest::default()
        })
        .await?;
    }

    if show_output {
        emit(OutKind::Out, &format!("updated to {to}"));
    }
    if options.json {
        emit(
            OutKind::Out,
            &pretty(&update_complete_json(&current_version, &to, running.len())),
        );
        return Ok(());
    }
    if show_output {
        emit(OutKind::Out, "update complete");
    } else if options.quiet {
        emit(OutKind::Out, &format!("updated {to}"));
    }
    Ok(())
}

/// `ompchamber update` — dispatch entry (mod.rs contract).
pub fn update_command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let _ = parsed;
    let data_dir = super::paths::data_dir();
    let serve = production_serve_launcher();
    let client = probe_client();
    block_on_cli(async {
        let mut emit = |kind: OutKind, line: &str| match kind {
            OutKind::Out => println!("{line}"),
            OutKind::Err => eprintln!("{line}"),
        };
        update_command_flow(
            &options,
            crate::package_manager::shared(),
            &data_dir,
            &serve,
            &client,
            &mut emit,
        )
        .await
    })
}

// ── connect-url command (commands-connect-url.js) ─────────────────────

struct RelayInfo {
    enabled: bool,
    relay_url: String,
    server_id: String,
    host_enc_pub_jwk: Value,
}

/// `resolveRelayUrl`: OMPCHAMBER_RELAY_URL env override, then the stored
/// setting, then the default — the same relay the host connects out to.
fn resolve_relay_url(env_url: Option<&str>, private_relay: Option<&Value>) -> String {
    if let Some(url) = env_url.filter(|url| crate::relay::service::is_valid_relay_url(url)) {
        return url.trim().to_string();
    }
    if let Some(stored) = private_relay
        .and_then(|value| value.get("relayUrl"))
        .and_then(Value::as_str)
        .filter(|url| crate::relay::service::is_valid_relay_url(url))
    {
        return stored.trim().to_string();
    }
    crate::relay::service::DEFAULT_RELAY_URL.to_string()
}

/// `buildRelayPairingCandidate` — the relay identity (generating it if the
/// relay was never enabled) as a pairing-v2 relay candidate.
async fn build_relay_pairing_candidate(data_dir: &Path) -> Result<RelayInfo, CliError> {
    let store = crate::settings::store_for_path(&data_dir.join("settings.json"));
    let settings = store.read_raw().await;
    let private_relay = settings.get("privateRelay");
    let relay_url = resolve_relay_url(
        std::env::var("OMPCHAMBER_RELAY_URL").ok().as_deref(),
        private_relay,
    );
    let identity = crate::relay::identity::RelayIdentityRuntime::new(
        store,
        crate::relay::identity::system_clock(),
    )
    .get_relay_identity()
    .await
    .map_err(|error| CliError::new(error.to_string(), GENERAL_ERROR))?;
    Ok(RelayInfo {
        enabled: private_relay.and_then(|value| value.get("enabled")) == Some(&Value::Bool(true)),
        relay_url,
        server_id: identity.server_id.clone(),
        host_enc_pub_jwk: identity.host_enc_pub_jwk.clone(),
    })
}

/// `buildPairingPayload` — the v2 link payload.
fn build_pairing_payload(
    pairing_id: &str,
    secret: &str,
    label: Option<&str>,
    fingerprint: &str,
    expires_at: &str,
    candidates: &[Value],
) -> Value {
    let mut payload = json!({
        "v": 2,
        "pairingId": pairing_id,
        "secret": secret,
    });
    if let Some(label) = label.filter(|label| !label.is_empty()) {
        payload["label"] = json!(label);
    }
    if !fingerprint.is_empty() {
        payload["fingerprint"] = json!(fingerprint);
    }
    if !expires_at.is_empty() {
        payload["expiresAt"] = json!(expires_at);
    }
    payload["candidates"] = json!(candidates);
    payload
}

/// `encodePairingConnectUrl`: v2 payload → base64url(JSON) in the query.
fn encode_pairing_connect_url(payload: &Value) -> String {
    let body = serde_json::to_string(payload).unwrap_or_default();
    format!(
        "ompchamber://connect?v=2&p={}",
        crate::relay::e2ee::bytes_to_base64_url(body.as_bytes())
    )
}

fn os_hostname() -> String {
    if let Some(value) = std::env::var("HOSTNAME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return value;
    }
    if let Ok(output) = std::process::Command::new("hostname").output() {
        let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    "unknown".to_string()
}

#[derive(Debug)]
pub(crate) struct ConnectUrlOutcome {
    pub port: u16,
    pub server_url: String,
    pub connect_url: String,
    pub pairing_id: String,
    pub fingerprint: String,
    pub expires_at: String,
    pub candidates: Vec<Value>,
    pub auto_started: bool,
    pub source: &'static str,
    pub relay_enabled: bool,
    pub relay_url: String,
}

/// The full connect-url flow: ensure a server, resolve the server URL,
/// build candidates (direct + relay), mint the one-time pairing session,
/// encode the link.
pub(crate) async fn build_connect_url(
    options: &Options,
    data_dir: &Path,
    serve: &ServeLauncher,
    client: &reqwest::Client,
) -> Result<ConnectUrlOutcome, CliError> {
    let port = options.port.unwrap_or(DEFAULT_PORT);
    assert_safe_browser_port_throwing(port, "OMPChamber connect-url")?;
    let explicit_server_url = options
        .server
        .as_deref()
        .and_then(normalize_server_url_for_connection);
    if options.server.is_some() && explicit_server_url.is_none() {
        return Err(CliError::usage(
            "Invalid --server URL. Use an http:// or https:// URL.",
        ));
    }

    let running = discover_running_instances(client, data_dir).await;
    let auto_started = !running.iter().any(|entry| entry.port == port);
    if auto_started {
        serve(ServeRequest {
            port,
            host: options.host.clone(),
            ui_password: options.ui_password.clone(),
            api_only: options.api_only,
            suppress_quiet_output: true,
            suppress_startup_summary: true,
            ..ServeRequest::default()
        })
        .await?;
    }

    let (server_url, source) = match explicit_server_url {
        Some(url) => (url, "explicit"),
        None => resolve_connect_url_server_url(port, options.host.as_deref(), data_dir).await,
    };
    let label = options
        .name
        .clone()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(os_hostname);

    // Direct candidate for the reachable server URL, plus the relay
    // transport as a fallback candidate — one link that works both on the
    // LAN and off-network. `--relay` opts in even when the host relay is
    // not up yet; otherwise the relay rides along only when enabled.
    let mut candidates = vec![json!({
        "type": if server_url.starts_with("https://") { "tunnel" } else { "lan" },
        "url": server_url,
        "priority": 10,
    })];
    let relay = build_relay_pairing_candidate(data_dir).await?;
    if options.relay || relay.enabled {
        candidates.push(json!({
            "type": "relay",
            "relayUrl": relay.relay_url,
            "serverId": relay.server_id,
            "hostEncPubJwk": relay.host_enc_pub_jwk,
            "priority": 30,
        }));
    }

    let pairing_state = crate::client_auth::state_for_data_dir(data_dir);
    // Mark relay-carrying sessions like the server route does, so the
    // host's demand-driven relay lifecycle keeps the relay up while the
    // link is pending.
    let uses_relay = candidates
        .iter()
        .any(|candidate| candidate.get("type").and_then(Value::as_str) == Some("relay"));
    let created = pairing_state
        .pairing
        .create_pairing_session(crate::client_auth::pairing::CreatePairingInput {
            label: Some(label.clone()),
            allowed_client_kinds: None,
            created_by_client_id: None,
            uses_relay,
        })
        .await
        .map_err(|error| CliError::new(error.to_string(), GENERAL_ERROR))?;
    let session = created.pairing.session;
    let payload = build_pairing_payload(
        &session.id,
        &created.pairing.secret,
        Some(&label),
        &session.fingerprint,
        &session.expires_at,
        &candidates,
    );
    let connect_url = encode_pairing_connect_url(&payload);

    Ok(ConnectUrlOutcome {
        port,
        server_url,
        connect_url,
        pairing_id: session.id,
        fingerprint: session.fingerprint,
        expires_at: session.expires_at,
        candidates,
        auto_started,
        source,
        relay_enabled: relay.enabled,
        relay_url: relay.relay_url,
    })
}

/// Output rendering for the connect-url result (JSON/quiet/human).
pub(crate) fn emit_connect_url_output(
    outcome: &ConnectUrlOutcome,
    options: &Options,
    emit: Emit<'_>,
) {
    if options.json {
        emit(
            OutKind::Out,
            &pretty(&with_status(json!({
                "serverUrl": outcome.server_url,
                "connectUrl": outcome.connect_url,
                "pairingId": outcome.pairing_id,
                "fingerprint": outcome.fingerprint,
                "expiresAt": outcome.expires_at,
                "candidates": outcome.candidates,
                "autoStarted": outcome.auto_started,
            }))),
        );
        return;
    }
    if options.quiet {
        emit(OutKind::Out, &outcome.connect_url);
        return;
    }
    emit(OutKind::Out, "OMPChamber pairing link");
    if outcome.auto_started {
        emit(
            OutKind::Out,
            &format!("started OMPChamber on port {}", outcome.port),
        );
    }
    emit(OutKind::Out, &outcome.connect_url);
    emit(OutKind::Out, &format!("Server URL: {}", outcome.server_url));
    if options.relay || outcome.relay_enabled {
        emit(
            OutKind::Out,
            &format!("Relay fallback: {}", outcome.relay_url),
        );
    }
    if options.relay && !outcome.relay_enabled {
        emit(OutKind::Out, "[RELAY_STARTING]");
        emit(
            OutKind::Out,
            "  Relay is not up yet. A running instance starts it within a minute; a stopped instance starts it on next launch.",
        );
    }
    if !outcome.fingerprint.is_empty() {
        emit(
            OutKind::Out,
            &format!("Fingerprint: {}", outcome.fingerprint),
        );
    }
    if outcome.source == "lan-detected" {
        emit(
            OutKind::Out,
            "Detected a LAN address because OMPChamber is bound to all interfaces. Use --server to override it.",
        );
    } else if outcome.source == "loopback-fallback" {
        emit(
            OutKind::Out,
            "OMPChamber is bound to all interfaces, but no LAN address was detected. Use --server to provide a reachable URL.",
        );
    } else if is_loopback_server_url(&outcome.server_url) {
        // The direct candidate points at this machine only — other devices
        // cannot use it. Say so instead of letting a "LAN" link silently
        // not work.
        emit(OutKind::Out, "[LAN_UNREACHABLE]");
        if options.relay {
            emit(
                OutKind::Out,
                "  OMPChamber only listens on this machine, so devices will always connect through the relay. Restart with --lan to allow direct home-network connections.",
            );
        } else {
            emit(
                OutKind::Out,
                "  OMPChamber only listens on this machine, so other devices cannot use this link. Restart with --lan, or use --server to provide a reachable URL.",
            );
        }
    }
    emit(
        OutKind::Out,
        "Scan or paste this link into another OMPChamber client. It is single-use and expires.",
    );
    if options.qr == Some(true) {
        // qrcode-terminal is not available in the Rust port: honest
        // failure-path warning on stderr, stdout contract unchanged.
        emit(
            OutKind::Err,
            "Warning: Could not generate QR code: QR rendering requires qrcode-terminal (pending)",
        );
    }
    emit(OutKind::Out, "pairing link generated");
}

/// `ompchamber connect-url` — dispatch entry (mod.rs contract).
pub fn connect_url_command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let _ = parsed;
    let data_dir = super::paths::data_dir();
    let serve = production_serve_launcher();
    let client = probe_client();
    let outcome =
        block_on_cli(async { build_connect_url(&options, &data_dir, &serve, &client).await })?;
    let mut emit = |kind: OutKind, line: &str| match kind {
        OutKind::Out => println!("{line}"),
        OutKind::Err => eprintln!("{line}"),
    };
    emit_connect_url_output(&outcome, &options, &mut emit);
    Ok(())
}

/// `showConnectUrlHelp` — for mod.rs's `connect-url --help` arm.
pub fn connect_url_help_text() -> &'static str {
    "\n OMPChamber Connect URL\n\nUSAGE:\n  ompchamber connect-url [OPTIONS]\n\nDESCRIPTION:\n  Generate an ompchamber:// connection link for adding this server to another\n  OMPChamber app. If no server is running on the selected port, it starts one.\n\nOPTIONS:\n  -p, --port <port>       Server port to use or start (default: 3000)\n  --host <address>        Bind address when starting the server\n  --hostname <address>    Alias for --host\n  --lan                   Bind to 0.0.0.0 for LAN access when starting\n  --server <url>          Public URL saved into the connection link\n  --server-url <url>      Alias for --server\n  --relay                 Also include the end-to-end-encrypted relay transport\n                          so the link works away from the local network. The\n                          device prefers the direct connection when reachable;\n                          the instance brings the relay up on its own. Set\n                          OMPCHAMBER_RELAY_URL to use a self-hosted relay.\n  --name <label>          Label saved with the remote client token\n  --ui-password <value>   Protect browser access when UI routes are enabled\n  --api-only              Start in headless/API-only mode when starting\n  --qr                    Print a QR code for the connection link\n  --json                  Output machine-readable JSON\n  -q, --quiet             Print only the connection link\n  -h, --help              Show this help\n\nEXAMPLES:\n  ompchamber connect-url --port 3000 --qr\n  ompchamber connect-url --port 3000 --api-only --lan --server http://workstation.local:3000 --qr\n  ompchamber connect-url --server https://ompchamber.example.com --name Workstation\n  ompchamber connect-url --relay --name \"My laptop\"\n\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-updc-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn make_info(available: bool, version: Option<&str>, current: &str) -> UpdateInfo {
        UpdateInfo {
            available,
            version: version.map(str::to_string),
            current_version: current.to_string(),
            body: None,
            release_url: None,
            download_url: None,
            package_manager: None,
            update_command: None,
            next_suggested_check_in_sec: None,
            error: None,
        }
    }

    struct FakeUpdateBackend {
        info: UpdateInfo,
        exec: UpdateExecution,
        detected: String,
        calls: Mutex<Vec<(String, Option<String>, bool)>>,
    }

    impl FakeUpdateBackend {
        fn new(info: UpdateInfo) -> Self {
            Self {
                info,
                exec: UpdateExecution {
                    success: true,
                    exit_code: Some(0),
                },
                detected: "npm".to_string(),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<(String, Option<String>, bool)> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    impl UpdateBackend for FakeUpdateBackend {
        fn current_version(&self) -> String {
            self.info.current_version.clone()
        }
        fn check_for_updates(&self) -> BoxFut<'_, UpdateInfo> {
            Box::pin(std::future::ready(self.info.clone()))
        }
        fn detect_package_manager(&self) -> BoxFut<'_, String> {
            Box::pin(std::future::ready(self.detected.clone()))
        }
        fn execute_update(
            &self,
            pm: &str,
            version: Option<&str>,
            silent: bool,
        ) -> BoxFut<'_, UpdateExecution> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((pm.to_string(), version.map(str::to_string), silent));
            Box::pin(std::future::ready(self.exec.clone()))
        }
    }

    fn recording_serve() -> (ServeLauncher, Arc<Mutex<Vec<ServeRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let launcher: ServeLauncher = Arc::new(move |request: ServeRequest| {
            captured
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(request.clone());
            Box::pin(std::future::ready(Ok(())))
        });
        (launcher, requests)
    }

    type Lines = std::rc::Rc<std::cell::RefCell<Vec<(OutKind, String)>>>;

    fn collect() -> (Lines, impl FnMut(OutKind, &str)) {
        let lines: Lines = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink_lines = lines.clone();
        let sink = move |kind: OutKind, line: &str| {
            sink_lines.borrow_mut().push((kind, line.to_string()));
        };
        (lines, sink)
    }

    fn out_of(lines: &Lines) -> Vec<String> {
        lines
            .borrow()
            .iter()
            .filter(|(kind, _)| *kind == OutKind::Out)
            .map(|(_, line)| line.clone())
            .collect()
    }

    fn err_of(lines: &Lines) -> Vec<String> {
        lines
            .borrow()
            .iter()
            .filter(|(kind, _)| *kind == OutKind::Err)
            .map(|(_, line)| line.clone())
            .collect()
    }

    // ── URL shaping ────────────────────────────────────────────────────

    #[test]
    fn normalize_server_url_for_connection_accepts_http_and_strips_trailing_slashes() {
        assert_eq!(
            normalize_server_url_for_connection("https://ompchamber.example.com"),
            Some("https://ompchamber.example.com".to_string())
        );
        assert_eq!(
            normalize_server_url_for_connection("http://homebridge:3002/"),
            Some("http://homebridge:3002".to_string())
        );
        assert_eq!(
            normalize_server_url_for_connection("ftp://example.com"),
            None
        );
        assert_eq!(normalize_server_url_for_connection("not a url"), None);
        assert_eq!(normalize_server_url_for_connection(""), None);
        assert_eq!(
            normalize_server_url_for_connection("https://x.example.com/payload#secret"),
            Some("https://x.example.com/payload".to_string())
        );
    }

    #[test]
    fn is_loopback_server_url_detection() {
        assert!(is_loopback_server_url("http://127.0.0.1:3000"));
        assert!(is_loopback_server_url("http://localhost:3000"));
        assert!(is_loopback_server_url("http://[::1]:3000"));
        assert!(!is_loopback_server_url("http://192.168.1.5:3000"));
        assert!(!is_loopback_server_url("ompchamber://connect"));
    }

    #[test]
    fn resolve_server_url_core_sources() {
        // Explicit non-wildcard host → direct local URL.
        let (url, source) = resolve_server_url_core(3000, Some("192.168.1.9"), None, None, None);
        assert_eq!(url, "http://192.168.1.9:3000");
        assert_eq!(source, "configured-host");

        // Host given as a full URL is used as-is.
        let (url, source) = resolve_server_url_core(
            3000,
            Some("https://devchamber.example.com"),
            None,
            None,
            None,
        );
        assert_eq!(url, "https://devchamber.example.com");
        assert_eq!(source, "configured-host");

        // Wildcard + LAN detected → LAN address.
        let (url, source) =
            resolve_server_url_core(3000, Some("0.0.0.0"), None, None, Some("192.168.1.5"));
        assert_eq!(url, "http://192.168.1.5:3000");
        assert_eq!(source, "lan-detected");

        // Wildcard + no LAN → loopback fallback (JS buildLocalUrl with no
        // host override always probes 127.0.0.1).
        let (url, source) = resolve_server_url_core(3000, Some("::"), None, None, None);
        assert_eq!(url, "http://127.0.0.1:3000");
        assert_eq!(source, "loopback-fallback");

        // No override at all → default bind host.
        let (url, _) = resolve_server_url_core(3000, None, None, None, None);
        assert_eq!(url, "http://127.0.0.1:3000");

        // Stored instance host is used when neither flag nor env supply one.
        let (url, _) = resolve_server_url_core(3000, None, None, Some("10.0.0.7"), None);
        assert_eq!(url, "http://10.0.0.7:3000");

        // …but the env var wins over the stored host.
        let (url, _) =
            resolve_server_url_core(3000, None, Some("10.0.0.8"), Some("10.0.0.7"), None);
        assert_eq!(url, "http://10.0.0.8:3000");

        // IPv6 host is bracketed.
        let (url, _) = resolve_server_url_core(3000, Some("fe80::1"), None, None, None);
        assert_eq!(url, "http://[fe80::1]:3000");
    }

    #[test]
    fn probe_hosts_dedupe_and_pid_matching() {
        let hosts = get_system_info_probe_hosts(&[Some("0.0.0.0".to_string())]);
        // 0.0.0.0 maps onto the same probe key as the default/loopback
        // fallback hosts, so everything dedupes into a single entry.
        assert_eq!(hosts, vec![(Some("0.0.0.0".to_string()), false)]);

        // A concrete host adds the default fallback (pid-matched) but the
        // explicit 127.0.0.1 still dedupes against it.
        let concrete = get_system_info_probe_hosts(&[Some("192.168.1.9".to_string())]);
        assert_eq!(
            concrete,
            vec![(Some("192.168.1.9".to_string()), false), (None, true),]
        );

        // 127.0.0.1 collapses with the default entry entirely.
        let loopback = get_system_info_probe_hosts(&[Some("127.0.0.1".to_string())]);
        assert_eq!(loopback, vec![(Some("127.0.0.1".to_string()), false)]);
    }

    #[test]
    fn unsafe_browser_ports_rejected_with_js_message() {
        let error =
            assert_safe_browser_port_throwing(22, "OMPChamber connect-url").expect_err("unsafe");
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(
            error.message,
            "OMPChamber connect-url cannot use port 22. Port 22 is browser-unsafe (ERR_UNSAFE_PORT) and is not supported for OMPChamber UI at http://127.0.0.1:22/. Use a safe port such as 3000, 5173, 8080, or a high ephemeral port."
        );
        assert!(assert_safe_browser_port_throwing(3000, "OMPChamber connect-url").is_ok());
        assert!(is_unsafe_browser_port(10080));
        assert!(!is_unsafe_browser_port(3000));
    }

    // ── pairing payload / link ─────────────────────────────────────────

    #[test]
    fn pairing_payload_and_connect_url_encoding() {
        let candidates =
            vec![json!({"type": "lan", "url": "http://127.0.0.1:3000", "priority": 10})];
        let payload = build_pairing_payload(
            "pair_abc123",
            "s3cret",
            Some("My laptop"),
            "ABCD-1234",
            "2026-01-01T00:00:00.000Z",
            &candidates,
        );
        assert_eq!(payload["v"], 2);
        assert_eq!(payload["pairingId"], "pair_abc123");
        assert_eq!(payload["secret"], "s3cret");
        assert_eq!(payload["label"], "My laptop");
        assert_eq!(payload["fingerprint"], "ABCD-1234");
        assert_eq!(payload["expiresAt"], "2026-01-01T00:00:00.000Z");
        assert_eq!(payload["candidates"].as_array().map(Vec::len), Some(1));

        let url = encode_pairing_connect_url(&payload);
        assert!(url.starts_with("ompchamber://connect?v=2&p="));
        let encoded = url.trim_start_matches("ompchamber://connect?v=2&p=");
        // base64url alphabet only.
        assert!(!encoded.contains('+') && !encoded.contains('/') && !encoded.contains('='));
        // Round-trips through the payload shape.
        let decoded = base64_decode_url(encoded);
        let parsed: Value = serde_json::from_str(&decoded).expect("payload json");
        assert_eq!(parsed["pairingId"], "pair_abc123");
        assert_eq!(parsed["secret"], "s3cret");

        // Empty optional fields are omitted.
        let lean = build_pairing_payload("pair_x", "s", None, "", "", &[]);
        assert!(lean.get("label").is_none());
        assert!(lean.get("fingerprint").is_none());
        assert!(lean.get("expiresAt").is_none());
        assert_eq!(lean["candidates"], json!([]));
    }

    fn base64_decode_url(value: &str) -> String {
        use base64::Engine;
        let padded = match value.len() % 4 {
            2 => format!("{value}=="),
            3 => format!("{value}="),
            _ => value.to_string(),
        };
        let bytes = base64::engine::general_purpose::URL_SAFE
            .decode(padded.as_bytes())
            .expect("base64url decode");
        String::from_utf8(bytes).expect("utf8")
    }

    #[test]
    fn relay_url_resolution_env_stored_default() {
        assert_eq!(
            resolve_relay_url(
                Some("wss://self.example/ws"),
                Some(&json!({"relayUrl": "wss://stored/ws"}))
            ),
            "wss://self.example/ws"
        );
        // Invalid env falls through to the stored setting.
        assert_eq!(
            resolve_relay_url(
                Some("http://not-a-relay"),
                Some(&json!({"relayUrl": "wss://stored/ws"}))
            ),
            "wss://stored/ws"
        );
        assert_eq!(
            resolve_relay_url(None, Some(&json!({"relayUrl": "wss://stored/ws"}))),
            "wss://stored/ws"
        );
        assert_eq!(
            resolve_relay_url(None, Some(&json!({"relayUrl": "garbage"}))),
            crate::relay::service::DEFAULT_RELAY_URL
        );
        assert_eq!(
            resolve_relay_url(None, None),
            crate::relay::service::DEFAULT_RELAY_URL
        );
    }

    // ── update flow (fake PM seam) ─────────────────────────────────────

    #[tokio::test]
    async fn update_up_to_date_shapes() {
        let data_dir = temp_dir("upd-up-to-date");
        let backend = FakeUpdateBackend::new(make_info(false, Some("1.0.0"), "1.0.0"));
        let (serve, requests) = recording_serve();
        let client = probe_client();

        // JSON mode
        let (lines, sink) = collect();
        let mut sink = sink;
        update_command_flow(
            &Options {
                json: true,
                ..Default::default()
            },
            &backend,
            &data_dir,
            &serve,
            &client,
            &mut sink,
        )
        .await
        .expect("update ok");
        let emitted = out_of(&lines);
        assert_eq!(emitted.len(), 1);
        let json: Value = serde_json::from_str(&emitted[0]).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["currentVersion"], "1.0.0");
        assert_eq!(json["latestVersion"], "1.0.0");
        assert_eq!(json["updated"], false);
        assert!(json.get("restartedCount").is_none());

        // Quiet mode
        let (lines, sink) = collect();
        let mut sink = sink;
        update_command_flow(
            &Options {
                quiet: true,
                ..Default::default()
            },
            &backend,
            &data_dir,
            &serve,
            &client,
            &mut sink,
        )
        .await
        .expect("update ok");
        assert_eq!(out_of(&lines), vec!["up-to-date 1.0.0".to_string()]);

        // Human mode
        let (lines, sink) = collect();
        let mut sink = sink;
        update_command_flow(
            &Options::default(),
            &backend,
            &data_dir,
            &serve,
            &client,
            &mut sink,
        )
        .await
        .expect("update ok");
        assert_eq!(
            out_of(&lines),
            vec![
                "OMPChamber Update".to_string(),
                "current version: 1.0.0".to_string(),
                "you are running the latest version".to_string(),
                "no update needed".to_string(),
            ]
        );
        assert!(
            requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn update_available_executes_and_reports() {
        let data_dir = temp_dir("upd-avail");
        let backend = FakeUpdateBackend::new(make_info(true, Some("2.0.0"), "1.0.0"));
        let (serve, _requests) = recording_serve();
        let client = probe_client();

        let (lines, sink) = collect();
        let mut sink = sink;
        update_command_flow(
            &Options {
                json: true,
                ..Default::default()
            },
            &backend,
            &data_dir,
            &serve,
            &client,
            &mut sink,
        )
        .await
        .expect("update ok");
        let emitted = out_of(&lines);
        let json: Value = serde_json::from_str(&emitted[0]).expect("json line");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["currentVersion"], "1.0.0");
        assert_eq!(json["latestVersion"], "2.0.0");
        assert_eq!(json["updated"], true);
        assert_eq!(json["restartedCount"], 0);
        // executeUpdate ran with silent=true in json mode and the version pinned.
        assert_eq!(
            backend.calls(),
            vec![("npm".to_string(), Some("2.0.0".to_string()), true)]
        );

        // Quiet mode prints the compact line and stays silent on stderr.
        let (lines_q, sink) = collect();
        let mut sink = sink;
        update_command_flow(
            &Options {
                quiet: true,
                ..Default::default()
            },
            &backend,
            &data_dir,
            &serve,
            &client,
            &mut sink,
        )
        .await
        .expect("update ok");
        assert_eq!(out_of(&lines_q), vec!["updated 2.0.0".to_string()]);

        // Human mode walks the full message flow.
        let (lines_h, sink) = collect();
        let mut sink = sink;
        update_command_flow(
            &Options::default(),
            &backend,
            &data_dir,
            &serve,
            &client,
            &mut sink,
        )
        .await
        .expect("update ok");
        assert_eq!(
            out_of(&lines_h),
            vec![
                "OMPChamber Update".to_string(),
                "current version: 1.0.0".to_string(),
                "updating 1.0.0 -> 2.0.0".to_string(),
                "updated to 2.0.0".to_string(),
                "update complete".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn update_check_error_propagates_as_general_error() {
        let data_dir = temp_dir("upd-err");
        let mut info = make_info(false, None, "1.0.0");
        info.error = Some("registry unreachable".to_string());
        let backend = FakeUpdateBackend::new(info);
        let (serve, _) = recording_serve();
        let (lines, sink) = collect();
        let mut sink = sink;
        let error = update_command_flow(
            &Options::default(),
            &backend,
            &data_dir,
            &serve,
            &probe_client(),
            &mut sink,
        )
        .await
        .expect_err("check error");
        assert_eq!(error.exit_code, GENERAL_ERROR);
        assert_eq!(error.message, "registry unreachable");
        assert_eq!(
            out_of(&lines),
            vec![
                "OMPChamber Update".to_string(),
                "current version: 1.0.0".to_string(),
                "update failed".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn update_execute_failure_message_includes_exit_code() {
        let data_dir = temp_dir("upd-exec-err");
        let mut backend = FakeUpdateBackend::new(make_info(true, Some("2.0.0"), "1.0.0"));
        backend.exec = UpdateExecution {
            success: false,
            exit_code: Some(3),
        };
        let (serve, _) = recording_serve();
        let (_lines, sink) = collect();
        let mut sink = sink;
        let error = update_command_flow(
            &Options {
                quiet: true,
                ..Default::default()
            },
            &backend,
            &data_dir,
            &serve,
            &probe_client(),
            &mut sink,
        )
        .await
        .expect_err("exec failure");
        assert_eq!(error.exit_code, GENERAL_ERROR);
        assert_eq!(error.message, "Update failed with exit code 3");

        backend.exec = UpdateExecution {
            success: false,
            exit_code: None,
        };
        let error = update_command_flow(
            &Options {
                quiet: true,
                ..Default::default()
            },
            &backend,
            &data_dir,
            &serve,
            &probe_client(),
            &mut sink,
        )
        .await
        .expect_err("exec failure null");
        assert_eq!(error.message, "Update failed with exit code null");
    }

    #[test]
    fn update_json_payloads_carry_status_ok() {
        let json = update_up_to_date_json("1.0.0", "1.0.0");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["latestVersion"], "1.0.0");
        let json = update_complete_json("1.0.0", "2.0.0", 2);
        assert_eq!(json["latestVersion"], "2.0.0");
        assert_eq!(json["restartedCount"], 2);
    }

    // ── connect-url flow ───────────────────────────────────────────────

    fn connect_options() -> Options {
        // Explicit host keeps the resolved server URL deterministic even
        // when the ambient environment exports OMPCHAMBER_HOST.
        Options {
            port: Some(3999),
            explicit_port: true,
            host: Some("127.0.0.1".to_string()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn connect_url_builds_link_and_starts_server() {
        let data_dir = temp_dir("conn-build");
        let (serve, requests) = recording_serve();
        let client = probe_client();
        let outcome = build_connect_url(&connect_options(), &data_dir, &serve, &client)
            .await
            .expect("connect url");

        assert!(
            outcome.auto_started,
            "no instance running → serve is invoked"
        );
        let requests = requests.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].port, 3999);
        assert!(requests[0].suppress_quiet_output && requests[0].suppress_startup_summary);

        assert_eq!(outcome.server_url, "http://127.0.0.1:3999");
        assert!(
            outcome
                .connect_url
                .starts_with("ompchamber://connect?v=2&p=")
        );
        assert!(outcome.pairing_id.starts_with("pair_"));
        assert!(!outcome.fingerprint.is_empty());
        assert!(!outcome.expires_at.is_empty());
        assert_eq!(outcome.candidates.len(), 1);
        assert_eq!(outcome.candidates[0]["type"], "lan");
        assert_eq!(outcome.candidates[0]["url"], "http://127.0.0.1:3999");
        assert_eq!(outcome.candidates[0]["priority"], 10);
        assert!(!outcome.relay_enabled);

        // The pairing session is redeemable: the shared store carries it.
        let store_path = data_dir.join("client-pairing-sessions.json");
        let stored: Value =
            serde_json::from_str(&std::fs::read_to_string(&store_path).expect("pairing store"))
                .expect("json");
        assert!(
            stored["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|session| session["id"] == outcome.pairing_id.as_str())
        );

        // The relay identity was generated into settings.json.
        assert!(data_dir.join("settings.json").exists());
    }

    #[tokio::test]
    async fn connect_url_relay_flag_adds_relay_candidate() {
        let data_dir = temp_dir("conn-relay");
        let (serve, _requests) = recording_serve();
        let client = probe_client();
        let options = Options {
            port: Some(3999),
            relay: true,
            ..Default::default()
        };
        let outcome = build_connect_url(&options, &data_dir, &serve, &client)
            .await
            .expect("connect url");
        assert_eq!(outcome.candidates.len(), 2);
        let relay = &outcome.candidates[1];
        assert_eq!(relay["type"], "relay");
        assert_eq!(relay["relayUrl"], crate::relay::service::DEFAULT_RELAY_URL);
        assert_eq!(relay["priority"], 30);
        assert!(!relay["serverId"].as_str().unwrap_or("").is_empty());
        assert!(relay["hostEncPubJwk"].get("kty").is_some());
    }

    #[tokio::test]
    async fn connect_url_invalid_server_url_is_usage_error() {
        let data_dir = temp_dir("conn-bad-server");
        let (serve, requests) = recording_serve();
        let options = Options {
            port: Some(3999),
            server: Some("ftp://nope".to_string()),
            ..Default::default()
        };
        let error = build_connect_url(&options, &data_dir, &serve, &probe_client())
            .await
            .expect_err("invalid server");
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(
            error.message,
            "Invalid --server URL. Use an http:// or https:// URL."
        );
        assert!(
            requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn connect_url_unsafe_port_rejected_before_anything() {
        let data_dir = temp_dir("conn-unsafe-port");
        let (serve, requests) = recording_serve();
        let options = Options {
            port: Some(6000),
            explicit_port: true,
            ..Default::default()
        };
        let error = build_connect_url(&options, &data_dir, &serve, &probe_client())
            .await
            .expect_err("unsafe port");
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert!(
            error
                .message
                .starts_with("OMPChamber connect-url cannot use port 6000.")
        );
        assert!(
            requests
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn connect_url_uses_explicit_server_url() {
        let data_dir = temp_dir("conn-explicit");
        let (serve, _) = recording_serve();
        let options = Options {
            port: Some(3999),
            server: Some("https://ompchamber.example.com".to_string()),
            ..Default::default()
        };
        let outcome = build_connect_url(&options, &data_dir, &serve, &probe_client())
            .await
            .expect("connect url");
        assert_eq!(outcome.server_url, "https://ompchamber.example.com");
        assert_eq!(outcome.source, "explicit");
        assert_eq!(outcome.candidates[0]["type"], "tunnel");
    }

    fn outcome_fixture() -> ConnectUrlOutcome {
        ConnectUrlOutcome {
            port: 3999,
            server_url: "http://127.0.0.1:3999".to_string(),
            connect_url: "ompchamber://connect?v=2&p=abc".to_string(),
            pairing_id: "pair_x".to_string(),
            fingerprint: "ABCD-1234".to_string(),
            expires_at: "2026-01-01T00:00:00.000Z".to_string(),
            candidates: vec![
                json!({"type": "lan", "url": "http://127.0.0.1:3999", "priority": 10}),
            ],
            auto_started: true,
            source: "configured-host",
            relay_enabled: false,
            relay_url: crate::relay::service::DEFAULT_RELAY_URL.to_string(),
        }
    }

    #[test]
    fn connect_url_output_json_shape() {
        let (lines, sink) = collect();
        let mut sink = sink;
        emit_connect_url_output(
            &outcome_fixture(),
            &Options {
                port: Some(3999),
                json: true,
                ..Default::default()
            },
            &mut sink,
        );
        let lines = out_of(&lines);
        assert_eq!(lines.len(), 1);
        let json: Value = serde_json::from_str(&lines[0]).expect("json");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["serverUrl"], "http://127.0.0.1:3999");
        assert_eq!(json["connectUrl"], "ompchamber://connect?v=2&p=abc");
        assert_eq!(json["pairingId"], "pair_x");
        assert_eq!(json["fingerprint"], "ABCD-1234");
        assert_eq!(json["expiresAt"], "2026-01-01T00:00:00.000Z");
        assert_eq!(json["autoStarted"], true);
        assert_eq!(json["candidates"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn connect_url_output_quiet_is_just_the_link() {
        let (lines, sink) = collect();
        let mut sink = sink;
        emit_connect_url_output(
            &outcome_fixture(),
            &Options {
                port: Some(3999),
                quiet: true,
                ..Default::default()
            },
            &mut sink,
        );
        assert_eq!(
            out_of(&lines),
            vec!["ompchamber://connect?v=2&p=abc".to_string()]
        );
    }

    #[test]
    fn connect_url_output_human_flow_and_qr_note() {
        let fixture = outcome_fixture();
        // Without --qr.
        let (lines, sink) = collect();
        let mut sink = sink;
        emit_connect_url_output(
            &fixture,
            &Options {
                port: Some(3999),
                ..Default::default()
            },
            &mut sink,
        );
        let all = lines.borrow().clone();
        assert!(all.iter().all(|(kind, _)| *kind == OutKind::Out));
        assert_eq!(
            out_of(&lines),
            vec![
                "OMPChamber pairing link".to_string(),
                "started OMPChamber on port 3999".to_string(),
                "ompchamber://connect?v=2&p=abc".to_string(),
                "Server URL: http://127.0.0.1:3999".to_string(),
                "Fingerprint: ABCD-1234".to_string(),
                // loopback server URL → LAN_UNREACHABLE notice
                "[LAN_UNREACHABLE]".to_string(),
                "  OMPChamber only listens on this machine, so other devices cannot use this link. Restart with --lan, or use --server to provide a reachable URL.".to_string(),
                "Scan or paste this link into another OMPChamber client. It is single-use and expires.".to_string(),
                "pairing link generated".to_string(),
            ]
        );

        // With --qr: identical stdout, honest stderr note.
        let (lines_qr, sink) = collect();
        let mut sink = sink;
        emit_connect_url_output(
            &fixture,
            &Options {
                port: Some(3999),
                qr: Some(true),
                ..Default::default()
            },
            &mut sink,
        );
        assert_eq!(out_of(&lines_qr), out_of(&lines));
        assert_eq!(
            err_of(&lines_qr),
            vec!["Warning: Could not generate QR code: QR rendering requires qrcode-terminal (pending)".to_string()]
        );
    }

    #[test]
    fn connect_url_output_relay_starting_notice() {
        let mut fixture = outcome_fixture();
        fixture.relay_enabled = false;
        let (lines, sink) = collect();
        let mut sink = sink;
        emit_connect_url_output(
            &fixture,
            &Options {
                port: Some(3999),
                relay: true,
                ..Default::default()
            },
            &mut sink,
        );
        let lines = out_of(&lines);
        assert!(lines.contains(&"[RELAY_STARTING]".to_string()));
        assert!(lines.contains(&"Relay fallback: wss://relay.ompchamber.dev/ws".to_string()));
        assert!(lines.contains(&"  OMPChamber only listens on this machine, so devices will always connect through the relay. Restart with --lan to allow direct home-network connections.".to_string()));
    }

    #[test]
    fn connect_url_help_text_matches_show_connect_url_help() {
        assert!(connect_url_help_text().starts_with("\n OMPChamber Connect URL\n\nUSAGE:\n"));
        assert!(
            connect_url_help_text().contains(
                "  -p, --port <port>       Server port to use or start (default: 3000)\n"
            )
        );
        assert!(
            connect_url_help_text()
                .ends_with("  ompchamber connect-url --relay --name \"My laptop\"\n\n")
        );
    }

    #[test]
    fn discovery_cleanups_stale_pid_files() {
        let data_dir = temp_dir("disc-stale");
        let run_dir = data_dir.join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        // A pid file pointing at a dead pid gets removed with its instance file.
        let dead_pid = 4_000_000;
        std::fs::write(run_dir.join("ompchamber-3998.pid"), dead_pid.to_string()).unwrap();
        std::fs::write(run_dir.join("ompchamber-3998.json"), "{\"port\":3998}").unwrap();
        // A non-parseable pid is treated as missing.
        std::fs::write(run_dir.join("ompchamber-3997.pid"), "not-a-pid").unwrap();
        std::fs::write(run_dir.join("ompchamber-3997.json"), "{\"port\":3997}").unwrap();

        let client = probe_client();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let instances = runtime.block_on(discover_running_instances(&client, &data_dir));
        assert!(instances.is_empty());
        assert!(!run_dir.join("ompchamber-3998.pid").exists());
        assert!(!run_dir.join("ompchamber-3998.json").exists());
        assert!(!run_dir.join("ompchamber-3997.pid").exists());
        assert!(!run_dir.join("ompchamber-3997.json").exists());
    }
}
