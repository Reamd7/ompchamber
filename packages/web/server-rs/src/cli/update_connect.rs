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
//!
//! 中文概要：本模块实现 CLI 的 `update` 与 `connect-url` 两条命令及其共享基础设施——
//! 输出管道（OutKind/Emit）、本地网络工具（主机解析、URL 拼装、LAN 地址探测、浏览器
//! 受限端口校验）、系统信息探测与运行实例发现、serve 启动接缝，以及 update 的包管理器
//! 接缝（UpdateBackend）与 connect-url 的 v2 配对链接编码。
//! update 流程：检查更新 → 优雅停止并清理运行实例 → 执行安装 → 按原参数重启实例。
//! connect-url 流程：确保服务器运行 → 解析可达的 server URL → 生成直连/relay 候选 →
//! 铸造一次性 pairing 会话并编码为 ompchamber:// 链接。

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
/// 输出行的目的地标记：正常输出（clack/quiet）走 stdout，警告与 QR 降级提示走 stderr，
/// 保证 stdout 的机器可读契约不被污染。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutKind {
    /// 普通输出：面向用户的正常信息行，写入 stdout。
    Out,
    /// 警告/错误提示行：写入 stderr。
    Err,
}

/// 输出回调类型：命令流程通过它发射 (目的地, 行文本)；测试注入收集器以断言输出契约，
/// 生产入口用 println/eprintln 直写终端。
pub(crate) type Emit<'a> = &'a mut dyn FnMut(OutKind, &str);

/// 把 JSON 值序列化为带缩进的美化字符串；序列化失败时退回空字符串，调用方直接整行输出。
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// `cli-output.js printJson` prepends the normalized `status` field.
/// 复刻 cli-output.js printJson 的前缀逻辑：在载荷最外层补上默认 status:"ok"，
/// 载荷自带的同名字段会覆盖默认值。
fn with_status(payload: Value) -> Value {
    let mut out = json!({ "status": "ok" });
    merge_object(&mut out, &payload);
    out
}

/// 浅合并：把 source 对象的键逐一覆盖进 target；任一侧不是 JSON 对象则静默不作为。
fn merge_object(target: &mut Value, source: &Value) {
    if let (Some(target), Some(source)) = (target.as_object_mut(), source.as_object()) {
        for (key, value) in source {
            target.insert(key.clone(), value.clone());
        }
    }
}

/// 在同步 CLI 入口里驱动异步 future：用 block_in_place 把当前 runtime 工作线程转为
/// 阻塞等待再 handle.block_on，避免在 runtime 线程上直接阻塞导致死锁；
/// 调用方必须已处于多线程 tokio runtime 上下文。
fn block_on_cli<F: Future>(future: F) -> F::Output {
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| handle.block_on(future))
}

/// 构造本地探测专用的 HTTP client：整体超时 1.5 秒，构建失败退回默认 client，
/// 确保对无响应端口的 /api/system/info 探测快速失败。
fn probe_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

// ── cli-network.js subset (local copies; network.rs owns the serve set) ──

/// `resolveApiHost`: a bind host mapped onto a connectable destination.
/// 把绑定主机解析为实际可连接的目标主机：优先 host_override，其次 env_host
/// （OMPCHAMBER_HOST），默认 127.0.0.1；通配地址 0.0.0.0 映射为 127.0.0.1、
/// ::/[::] 映射为 ::1，带方括号的输入（如 [::1]）会剥掉括号。
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

/// resolve_api_host_with 的便捷封装：env 主机固定读取 OMPCHAMBER_HOST 环境变量。
fn resolve_api_host(host_override: Option<&str>) -> String {
    resolve_api_host_with(
        host_override,
        std::env::var("OMPCHAMBER_HOST").ok().as_deref(),
    )
}

/// `formatHostForUrl`: bracket IPv6 for URL usage.
/// 为主机名拼进 URL 做准备：含冒号的 IPv6 字面量加上方括号，其余原样返回。
fn format_host_for_url(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// 组装本地 API 的完整 URL：经 resolve_api_host_with 解析目标主机（IPv6 已加括号），
/// 拼上端口与规范化为以 / 开头的 endpoint 路径。
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
/// Fetch/Chromium 视为受限（ERR_UNSAFE_PORT）的端口表：UI 绑定这些端口时浏览器会
/// 直接拒发请求，因此 CLI 在启动/生成链接前就主动拒绝它们。
const UNSAFE_BROWSER_PORTS: [u16; 81] = [
    0, 1, 7, 9, 11, 13, 15, 17, 19, 20, 21, 22, 23, 25, 37, 42, 43, 53, 69, 77, 79, 87, 95, 101,
    102, 103, 104, 109, 110, 111, 113, 115, 117, 119, 123, 135, 137, 139, 143, 161, 179, 389, 427,
    465, 512, 513, 514, 515, 526, 530, 531, 532, 540, 548, 554, 556, 563, 587, 601, 636, 989, 990,
    993, 995, 1719, 1720, 1723, 2049, 3659, 4045, 5060, 5061, 6000, 6566, 6665, 6666, 6667, 6668,
    6669, 6697, 10080,
];

/// 判断端口是否落在浏览器受限端口表内。
fn is_unsafe_browser_port(port: u16) -> bool {
    UNSAFE_BROWSER_PORTS.contains(&port)
}
/// 校验端口可安全用于浏览器访问：受限端口返回 USAGE_ERROR，错误文案与 JS 版逐字一致
///（含违规端口的示例 URL 与替代端口建议）；context 是拼进文案的命令名前缀。
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
/// 探测本机 LAN IPv4 地址：先做 UDP connect 8.8.8.8 的选路技巧（不真正发包，
/// 只借内核选出出口地址，排除 0.0.0.0 与 127.x），失败再退回网卡枚举。
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

/// 网卡枚举兜底：按平台调用 ifconfig（macOS）/ ip -4 -o addr（Linux）/ ipconfig
///（Windows），解析输出里第一个非回环、非 IPv6 的 inet 地址（已剥离 %zone 后缀）。
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
/// 规范化 --server 提供的 URL：仅接受 http/https scheme，剥掉 fragment 与尾部斜杠；
/// 空串、解析失败或其他 scheme 一律返回 None。
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

/// 判断是否为通配绑定主机（0.0.0.0 / :: / [::]）——不能直接当作连接目标使用。
fn is_wildcard_bind_host(host: &str) -> bool {
    host == "0.0.0.0" || host == "::" || host == "[::]"
}

/// 规范化后判断探测主机是否为通配地址。
fn is_wildcard_probe_host(host: Option<&str>) -> bool {
    matches!(normalize_probe_host(host), Some(h) if h == "0.0.0.0" || h == "::" || h == "[::]")
}

/// 规范化后判断探测主机是否为回环地址（127.0.0.1 / localhost / ::1 / [::1]）。
fn is_loopback_probe_host(host: Option<&str>) -> bool {
    matches!(
        normalize_probe_host(host),
        Some(h) if h == "127.0.0.1" || h == "localhost" || h == "::1" || h == "[::1]"
    )
}

/// 规范化探测主机：去除首尾空白，空串视为未提供（None）。
fn normalize_probe_host(host: Option<&str>) -> Option<String> {
    host.map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// `isLoopbackServerUrl`.
/// 判断 server URL 的主机名是否指向本机回环（127.0.0.1 / localhost / ::1；
/// IPv6 方括号已剥离）；URL 解析失败按非回环处理。
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
/// resolveConnectUrlServerUrl 的纯函数核心（env/LAN/存储主机均注入以便测试），
/// 返回 (server URL, 来源标签)。优先级：host_override > env_host > 存储的实例主机；
/// 主机本身是完整 http(s) URL 时直接采用；非通配主机拼本地 URL；通配绑定在探测到
/// LAN 地址时用 LAN（lan-detected），否则退回 127.0.0.1（loopback-fallback）。
/// 来源标签供人机输出给出相应提示。
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

/// resolve_server_url_core 的 I/O 包装：读取 OMPCHAMBER_HOST、run 目录下已登记实例
/// 的绑定主机（仅当 flag/env 都未提供时）以及当前 LAN 地址，再交由核心函数裁决。
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

/// /api/system/info 返回的最小实例信息：runtime 标识与进程 PID。
#[derive(Clone, Debug)]
pub(crate) struct SystemInfo {
    /// 实例运行时类型（"web" 或 "desktop"）；desktop 实例不归 CLI update 管理。
    pub runtime: String,
    /// 实例自报的进程 PID，用于与 pid 文件交叉验证；响应缺失时为 None。
    pub pid: Option<u32>,
}

/// 向指定端口的 /api/system/info 发 GET 探测（主机经解析，兼容 host 覆盖与
/// OMPCHAMBER_HOST）；端口为 0、请求失败、非 2xx 或响应缺 runtime 字段时返回 None。
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
/// 尽力而为地请求实例自我关闭：POST /api/system/shutdown；任何失败（端口 0、
/// 网络错误、非 2xx）都只返回 false，由调用方决定是否转入强杀流程。
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

/// PID 存活性与身份核对后的判定结果，驱动实例发现与陈旧登记文件的清理决策。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessState {
    /// 进程不存在或 PID 无效：登记文件视为陈旧，应清除。
    Dead,
    /// 进程存活且命令行确认属于 OMPChamber。
    Matched,
    /// 进程存活但命令行属于其他程序（PID 被复用）。
    Mismatched,
    /// 进程存活但读不到命令行（权限等）：按存活处理，避免误杀。
    Unknown,
}

/// 综合 PID 存活性与命令行判定进程状态；命令行不可读时退回“仅存活”判定
///（issue #1721 语义：宁可漏杀不可误杀）。
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
/// 构造有序且按 resolve_api_host 归一化键去重的探测主机列表：先列入全部显式主机，
/// 再补默认兜底（None 与 127.0.0.1，二者归一化后合一）；当且仅当存在具体的权威主机
///（非通配、非回环）时，兜底项标记 requires_pid_match，防止探到同机上别的 OMPChamber。
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

/// 依序探测候选主机，返回第一个成功响应的 (SystemInfo, 命中主机)；标记了
/// requires_pid_match 的候选在响应 PID 与 expected_pid 不符时跳过；全部失败返回 (None, None)。
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

/// 发现的一个运行中 OMPChamber 实例：来自 run 目录登记文件，并经 /api/system/info 在线确认。
#[derive(Clone, Debug)]
pub(crate) struct DiscoveredInstance {
    /// 实例监听端口。
    pub port: u16,
    /// 确认的实例 PID：优先取在线响应上报值，否则仅在进程身份 Matched 时沿用 pid 文件值。
    pub pid: Option<u32>,
    /// run 目录下该实例的 pid 文件路径，实例停止后由本模块删除。
    pub pid_file_path: PathBuf,
    /// 从实例 json 文件读回的启动参数，重启实例时按此还原。
    pub stored: Option<process::InstanceOptions>,
    /// 在线确认时实际连通的主机（或回退到存储的 host），作为后续 API 调用目标。
    pub confirmed_host: Option<String>,
}

/// `discoverRunningInstances` (registry + probe, with stale-file cleanup).
/// 扫描 data_dir/run 下的 ompchamber-<port>.pid 登记文件：读 PID 与实例选项，校验进程
/// 存活与身份，再经 /api/system/info 在线确认该端口确由该 PID 的 OMPChamber 提供。
/// 陈旧登记（PID 缺失/死亡/身份不符且无法在线确认/desktop 实例）连同 json 文件一并清除。
/// 返回按端口升序的存活实例列表；run 目录不存在时返回空表。
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

/// 传给 serve 启动器的参数集：update/connect-url 复用 `ompchamber serve` 拉起实例
/// 所需的最小选项。
#[derive(Clone, Debug, Default)]
pub(crate) struct ServeRequest {
    /// 服务监听端口。
    pub port: u16,
    /// 绑定主机；None 时使用 serve 的默认绑定。
    pub host: Option<String>,
    /// UI 访问密码（如有）。
    pub ui_password: Option<String>,
    /// 是否以 headless/仅 API 模式启动。
    pub api_only: bool,
    /// 是否静默启动（不打印 serve 自身输出）。
    pub quiet: bool,
    /// 抑制 quiet 模式下仍会打印的输出（如端口提示行）。
    pub suppress_quiet_output: bool,
    /// 抑制启动摘要，保持调用方输出干净。
    pub suppress_startup_summary: bool,
}

/// 装箱的 Send future 别名：在 trait 对象里按引用返回异步结果所需。
pub(crate) type BoxFut<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// serve 启动器接缝：接收 ServeRequest、异步拉起一个服务器实例；生产实现转发到
/// serve 命令，测试注入记录型假实现以断言启动参数。
pub(crate) type ServeLauncher =
    Arc<dyn Fn(ServeRequest) -> BoxFut<'static, Result<(), CliError>> + Send + Sync>;

/// 构造 command 为 "serve" 的空 Parsed：serve::command 实际只读 Options，
/// 这里仅为满足其签名。
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

/// 生产启动器：把 ServeRequest 映射为 serve 命令的 Options（显式端口，抑制 UI 密码
/// 与不安全端口警告，输出抑制由请求位决定），再调用 serve::command 拉起实例。
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
/// update 命令依赖的包管理器接缝（对应 JS 版 dynamic-import 的 package-manager.js）：
/// 生产由 PackageManagerRuntime 实现，测试注入 FakeUpdateBackend（等价于 vitest 模块 mock）。
pub(crate) trait UpdateBackend {
    /// 当前安装的版本号。
    fn current_version(&self) -> String;
    /// 向 registry 检查更新；失败信息携带在 UpdateInfo.error 中而非 Result 里。
    fn check_for_updates(&self) -> BoxFut<'_, UpdateInfo>;
    /// 探测应使用的包管理器（npm/bun/...）名称。
    fn detect_package_manager(&self) -> BoxFut<'_, String>;
    /// 执行实际更新：用检测到的包管理器安装指定版本（None 表示 latest），
    /// silent 控制安装命令自身的输出。
    fn execute_update<'a>(
        &'a self,
        pm: &'a str,
        version: Option<&'a str>,
        silent: bool,
    ) -> BoxFut<'a, UpdateExecution>;
}

/// 生产实现：把 trait 签名要求的 BoxFut 适配到 PackageManagerRuntime 的原生 async 方法上。
impl UpdateBackend for crate::package_manager::PackageManagerRuntime {
    /// 委托 PackageManagerRuntime::get_current_version。
    fn current_version(&self) -> String {
        crate::package_manager::PackageManagerRuntime::get_current_version(self)
    }
    /// 以默认 CheckForUpdatesOptions 委托 check_for_updates。
    fn check_for_updates(&self) -> BoxFut<'_, UpdateInfo> {
        Box::pin(
            crate::package_manager::PackageManagerRuntime::check_for_updates(
                self,
                CheckForUpdatesOptions::default(),
            ),
        )
    }
    /// 委托 detect_package_manager。
    fn detect_package_manager(&self) -> BoxFut<'_, String> {
        Box::pin(crate::package_manager::PackageManagerRuntime::detect_package_manager(self))
    }
    /// 组装 ExecuteUpdateOptions（版本与 silent）后委托 execute_update。
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

/// 构造“已是最新”的 JSON 输出载荷（status=ok、updated=false，无 restartedCount）。
fn update_up_to_date_json(current_version: &str, latest_version: &str) -> Value {
    with_status(json!({
        "currentVersion": current_version,
        "latestVersion": latest_version,
        "updated": false,
    }))
}

/// 构造“更新完成”的 JSON 输出载荷（status=ok、updated=true、restartedCount）。
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

/// 以 150ms 间隔轮询进程是否退出：timeout_ms 内退出返回 true，超时返回 false；
/// PID 为 0 视为无需等待、直接成功。
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
/// 停止实例进程：先给 shutdown_wait_ms 的优雅退出窗口，超时才在阻塞线程里调用
/// terminate_process_tree 逐级强杀（先 graceful_ms 后 force_ms）。
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

/// update 命令的完整流程（backend/serve/emit 均为注入点，便于测试）：发现运行实例 →
/// 检查更新（出错以 GENERAL_ERROR 冒泡）→ 已是最新则按 JSON/quiet/human 三种形态输出；
/// 有更新时逐实例请求自我关闭并兜底强杀、清理 pid 文件，经包管理器执行安装（失败报
/// GENERAL_ERROR），再按各实例存储的原始选项逐一重启，最后输出完成信息
///（JSON 模式附带 restartedCount）。
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
/// `ompchamber update` 的同步分发入口（mod.rs 契约）：装配数据目录、生产 serve 启动器、
/// 探测 client 与 println/eprintln 输出回调，经 block_on_cli 在 ambient runtime 上驱动流程。
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

/// relay 配对所需的实例身份信息（自 settings.json 读取，必要时生成）。
struct RelayInfo {
    /// 设置中 privateRelay.enabled 是否为 true。
    enabled: bool,
    /// 实际采用的 relay WebSocket 地址。
    relay_url: String,
    /// 本实例在 relay 侧的 serverId。
    server_id: String,
    /// host 端 E2EE 加密公钥（JWK），随 relay 候选下发供客户端加密使用。
    host_enc_pub_jwk: Value,
}

/// `resolveRelayUrl`: OMPCHAMBER_RELAY_URL env override, then the stored
/// setting, then the default — the same relay the host connects out to.
/// relay 地址三级回退：OMPCHAMBER_RELAY_URL 环境变量 → 存储的 privateRelay.relayUrl
/// 设置 → 内置默认 relay；前两级都须通过 is_valid_relay_url 校验，非法值被跳过。
/// 与 host 出站连接使用同一个 relay。
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
/// 读取 settings.json 解析 relay 配置并取得实例身份（身份不存在时由
/// RelayIdentityRuntime 生成）；读取/生成失败转成 GENERAL_ERROR 的 CliError。
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
/// 组装 v2 配对载荷：必含 v/pairingId/secret/candidates；label、fingerprint、
/// expiresAt 为空时整个字段省略，与 JS 版字段级行为一致。
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
/// 把 v2 载荷编码为 ompchamber://connect?v=2&p=<base64url(JSON)> 链接。
fn encode_pairing_connect_url(payload: &Value) -> String {
    let body = serde_json::to_string(payload).unwrap_or_default();
    format!(
        "ompchamber://connect?v=2&p={}",
        crate::relay::e2ee::bytes_to_base64_url(body.as_bytes())
    )
}

/// 取本机主机名作默认配对标签：HOSTNAME 环境变量优先，其次 hostname 命令输出，
/// 全部失败返回 "unknown"。
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

/// connect-url 命令的完整产出：最终链接及其构成（候选、会话、relay 状态），供输出层渲染。
#[derive(Debug)]
pub(crate) struct ConnectUrlOutcome {
    /// 目标（或已自动启动）实例的端口。
    pub port: u16,
    /// 解析出的直连 server URL（候选列表第一项）。
    pub server_url: String,
    /// 最终编码出的 ompchamber:// 链接。
    pub connect_url: String,
    /// 一次性配对会话 ID。
    pub pairing_id: String,
    /// 会话指纹，供用户在客户端核对身份。
    pub fingerprint: String,
    /// 会话过期时间。
    pub expires_at: String,
    /// 连接候选（直连 + 可选 relay），priority 数值越小越优先。
    pub candidates: Vec<Value>,
    /// 是否因目标端口无实例而自动启动了服务器。
    pub auto_started: bool,
    /// server URL 的来源标签：explicit/configured-host/lan-detected/loopback-fallback。
    pub source: &'static str,
    /// 设置中 relay 是否已启用（区别于 --relay 的显式请求）。
    pub relay_enabled: bool,
    /// 候选中使用的 relay 地址。
    pub relay_url: String,
}

/// The full connect-url flow: ensure a server, resolve the server URL,
/// build candidates (direct + relay), mint the one-time pairing session,
/// encode the link.
/// connect-url 全流程：校验端口浏览器安全与 --server 合法性（失败立即 USAGE_ERROR，
/// 不启动任何服务）→ 发现实例，端口无实例时经 serve 接缝自动启动 → 解析 server URL
///（显式 --server 优先）→ 构造直连候选，并在 --relay 或 relay 已启用时追加 relay 候选 →
/// 创建携带 uses_relay 标记的一次性配对会话 → 编码最终链接并返回产出。
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
/// 渲染 connect-url 结果：JSON 模式输出单行机器可读对象；quiet 只输出链接本身；
/// 人机模式依次输出标题/自动启动提示/链接/Server URL/relay 信息/fingerprint，并针对
/// LAN 探测、回环不可达、relay 未就绪给出醒目提示；--qr 无法渲染时仅在 stderr 输出诚实警告。
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
/// `ompchamber connect-url` 的同步分发入口（mod.rs 契约）：装配依赖、构建链接，
/// 再经 emit_connect_url_output 渲染输出。
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
/// `ompchamber connect-url --help` 的帮助文案（与 JS 版 showConnectUrlHelp 逐字一致）。
pub fn connect_url_help_text() -> &'static str {
    "\n OMPChamber Connect URL\n\nUSAGE:\n  ompchamber connect-url [OPTIONS]\n\nDESCRIPTION:\n  Generate an ompchamber:// connection link for adding this server to another\n  OMPChamber app. If no server is running on the selected port, it starts one.\n\nOPTIONS:\n  -p, --port <port>       Server port to use or start (default: 3000)\n  --host <address>        Bind address when starting the server\n  --hostname <address>    Alias for --host\n  --lan                   Bind to 0.0.0.0 for LAN access when starting\n  --server <url>          Public URL saved into the connection link\n  --server-url <url>      Alias for --server\n  --relay                 Also include the end-to-end-encrypted relay transport\n                          so the link works away from the local network. The\n                          device prefers the direct connection when reachable;\n                          the instance brings the relay up on its own. Set\n                          OMPCHAMBER_RELAY_URL to use a self-hosted relay.\n  --name <label>          Label saved with the remote client token\n  --ui-password <value>   Protect browser access when UI routes are enabled\n  --api-only              Start in headless/API-only mode when starting\n  --qr                    Print a QR code for the connection link\n  --json                  Output machine-readable JSON\n  -q, --quiet             Print only the connection link\n  -h, --help              Show this help\n\nEXAMPLES:\n  ompchamber connect-url --port 3000 --qr\n  ompchamber connect-url --port 3000 --api-only --lan --server http://workstation.local:3000 --qr\n  ompchamber connect-url --server https://ompchamber.example.com --name Workstation\n  ompchamber connect-url --relay --name \"My laptop\"\n\n"
}

/// update/connect-url 的单元测试：覆盖 URL 规范化、server URL 解析优先级、探测主机
/// 去重与 PID 匹配、浏览器受限端口、配对载荷编码、update 各输出模式与错误路径、
/// connect-url 全流程与输出渲染，以及陈旧 pid 文件清理。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 创建以 tag 命名的唯一临时目录（进程 ID + 原子计数器防撞），先删旧再建新。
    fn temp_dir(tag: &str) -> PathBuf {
        // 原子自增计数器：让同一进程内重复调用也拿到互不冲突的目录名。
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

    /// 构造指定可用性/目标版本/当前版本的 UpdateInfo 测试夹具，其余字段为 None。
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

    /// UpdateBackend 的假实现：返回预置的检查与执行结果，并记录 execute_update 调用参数。
    struct FakeUpdateBackend {
        /// 预置的更新检查结果。
        info: UpdateInfo,
        /// 预置的安装执行结果。
        exec: UpdateExecution,
        /// 预置的包管理器名。
        detected: String,
        /// execute_update 的调用记录 (pm, version, silent)。
        calls: Mutex<Vec<(String, Option<String>, bool)>>,
    }

    /// 假实现的构造与调用记录读取。
    impl FakeUpdateBackend {
        /// 以给定检查结果构造：执行默认成功（退出码 0）、包管理器 npm。
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

        /// 取调用记录快照；锁中毒时也恢复出内部数据。
        fn calls(&self) -> Vec<(String, Option<String>, bool)> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    /// 全部 future 立即完成的假实现，等价于 vitest 的模块 mock。
    impl UpdateBackend for FakeUpdateBackend {
        /// 返回夹具的当前版本。
        fn current_version(&self) -> String {
            self.info.current_version.clone()
        }
        /// 立即返回预置的 UpdateInfo。
        fn check_for_updates(&self) -> BoxFut<'_, UpdateInfo> {
            Box::pin(std::future::ready(self.info.clone()))
        }
        /// 立即返回预置的包管理器名。
        fn detect_package_manager(&self) -> BoxFut<'_, String> {
            Box::pin(std::future::ready(self.detected.clone()))
        }
        /// 记录 (pm, version, silent) 后立即返回预置的执行结果。
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

    /// 返回记录型 ServeLauncher 与捕获到的请求列表，用于断言传给 serve 的启动参数。
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

    /// 输出收集器的共享句柄类型：同时交给 sink 闭包与断言方。
    type Lines = std::rc::Rc<std::cell::RefCell<Vec<(OutKind, String)>>>;

    /// 创建输出收集器及其 sink 闭包，替代真实的 println/eprintln。
    fn collect() -> (Lines, impl FnMut(OutKind, &str)) {
        let lines: Lines = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink_lines = lines.clone();
        let sink = move |kind: OutKind, line: &str| {
            sink_lines.borrow_mut().push((kind, line.to_string()));
        };
        (lines, sink)
    }

    /// 过滤收集到的 stdout 行。
    fn out_of(lines: &Lines) -> Vec<String> {
        lines
            .borrow()
            .iter()
            .filter(|(kind, _)| *kind == OutKind::Out)
            .map(|(_, line)| line.clone())
            .collect()
    }

    /// 过滤收集到的 stderr 行。
    fn err_of(lines: &Lines) -> Vec<String> {
        lines
            .borrow()
            .iter()
            .filter(|(kind, _)| *kind == OutKind::Err)
            .map(|(_, line)| line.clone())
            .collect()
    }

    // ── URL shaping ────────────────────────────────────────────────────

    /// 验证 URL 规范化只接受 http/https、剥掉 fragment 与尾斜杠，其余输入返回 None。
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

    /// 验证回环 URL 判定覆盖 127.0.0.1/localhost/[::1]，并排除 LAN 地址与不可解析输入。
    #[test]
    fn is_loopback_server_url_detection() {
        assert!(is_loopback_server_url("http://127.0.0.1:3000"));
        assert!(is_loopback_server_url("http://localhost:3000"));
        assert!(is_loopback_server_url("http://[::1]:3000"));
        assert!(!is_loopback_server_url("http://192.168.1.5:3000"));
        assert!(!is_loopback_server_url("ompchamber://connect"));
    }

    /// 验证 server URL 解析优先级：显式主机、完整 URL 直用、通配+LAN、通配回退、存储主机兜底、env 覆盖存储、IPv6 加括号。
    #[test]
    fn resolve_server_url_core_sources() {
        // Host resolution reads the process env — serialize against EnvGuard
        // tests that rewrite OMPCHAMBER_HOST mid-run.
        let _env = crate::cli::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe { std::env::remove_var("OMPCHAMBER_HOST") };
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

    /// 验证探测主机列表按归一化键去重，且仅在存在具体权威主机时补入需 PID 匹配的兜底项。
    #[test]
    fn probe_hosts_dedupe_and_pid_matching() {
        // Host resolution reads the process env — serialize against EnvGuard
        // tests that rewrite OMPCHAMBER_HOST mid-run.
        let _env = crate::cli::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        unsafe { std::env::remove_var("OMPCHAMBER_HOST") };
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

    /// 验证受限端口以 USAGE_ERROR 与 JS 版逐字一致的文案被拒绝，安全端口放行。
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

    /// 验证 v2 配对载荷的字段与可选项省略行为，以及 base64url 链接编码可往返还原。
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

    /// 测试辅助：补齐 base64url 缺失的 padding 并解码回 UTF-8 字符串。
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

    /// 验证 relay 地址按 env > 存储设置 > 默认 三级回退，非法值被跳过。
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

    /// 验证“已是最新”时 JSON/quiet/human 三种输出形态，且不触发任何 serve 重启。
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

    /// 验证有更新时以 silent 模式、固定版本执行安装，并按三种模式输出结果（JSON 含 restartedCount）。
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

    /// 验证检查更新出错时以 GENERAL_ERROR 冒泡，且人机模式先打印 update failed。
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

    /// 验证安装失败信息携带退出码，退出码缺失时显示 null。
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

    /// 验证两种 JSON 载荷都带 status=ok 及各自关键字段。
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

    /// connect-url 测试的基准选项：显式端口 3999 + 显式 host 127.0.0.1，
    /// 让 server URL 解析不受环境变量 OMPCHAMBER_HOST 影响。
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

    /// 验证无实例时自动拉起 serve（抑制其输出）、生成单候选链接，且配对会话落盘、relay 身份已生成。
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

    /// 验证 --relay 在未启用时也追加 relay 候选（默认 relay 地址、serverId 与加密公钥齐全）。
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

    /// 验证非法 --server 在启动任何服务之前即以 USAGE_ERROR 拒绝。
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

    /// 验证受限端口的拒绝发生在一切动作之前（serve 未被调用）。
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

    /// 验证显式 --server 直接作为 server URL（来源 explicit），https 生成 tunnel 类型候选。
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

    /// 输出渲染测试用的标准 ConnectUrlOutcome 夹具。
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

    /// 验证 JSON 输出为单行对象且字段齐全。
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

    /// 验证 quiet 模式只输出链接本身。
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

    /// 验证人机输出的完整行序、回环地址触发 LAN_UNREACHABLE 提示，--qr 仅在 stderr 增加警告。
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

    /// 验证 --relay 且 relay 未启用时输出 RELAY_STARTING 提示与 relay 兜底行。
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

    /// 验证帮助文案的首尾关键内容与 JS 版一致。
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

    /// 验证发现流程会清除指向死亡 PID 与不可解析 PID 的登记文件及其实例 json。
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
