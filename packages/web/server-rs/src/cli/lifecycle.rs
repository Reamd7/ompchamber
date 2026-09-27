//! Port of `bin/lib/commands-lifecycle.js`: the `stop` and `restart`
//! commands, plus the discovery/probe surface they consume from
//! `bin/lib/cli-lifecycle.js` (`discoverRunningInstances`,
//! `discoverOMPChamberInstanceOnPort`, `discoverLifecycleInstances`,
//! `discoverUnconfirmedRegistryInstanceOnPort`) and the shutdown/system-info
//! helpers from `bin/lib/cli-http.js` (`requestServerShutdown`,
//! `fetchSystemInfoFromPort`, the multi-host probe-candidate walk) and
//! `bin/lib/cli-ports.js` (`isPortAvailable`).
//!
//! `bin/lib/cli-log-files.js` (rotateLogFile/readTailLines/followFile) is
//! imported by the serve and logs commands, not by stop/restart, so it is not
//! ported here.
//!
//! Human output follows the same plain-text convention as the ported serve
//! command: clack intro/outro titles print as plain lines, `logStatus` prints
//! message (+ detail) with no clack glyphs. JSON payloads and quiet-mode lines
//! match the JS byte-for-byte.
//!
//! 中文说明：本模块是旧 JS CLI 生命周期命令的 Rust 移植，实现
//! `ompchamber stop` 与 `ompchamber restart`：运行实例发现（run 目录
//! pid 文件遍历 + `/api/system/info` HTTP 探测）、端口可用性检测、
//! 优雅停机（先 POST `/api/system/shutdown`，超时升级为终止进程树），
//! 以及 desktop 运行时的豁免逻辑。输出与 JS 版对齐：human 模式纯
//! 文本、JSON 载荷逐字节一致、quiet 模式单行结果。restart 内部以
//! 完全静默的参数复用 stop，再按存储的实例选项调用 serve 拉起新实例。

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::args::{DEFAULT_PORT, Options, Parsed};
use super::paths;
use super::process;
use super::serve;
use super::{CliError, GENERAL_ERROR, print_json};

/// Entry point wired into `cli::run` dispatch. The dispatcher calls this
/// synchronously from inside the process's tokio runtime, so the (blocking)
/// pipeline runs on a dedicated thread with its own single-threaded runtime —
/// never `block_on` inside the caller's reactor, and plain `#[test]` callers
/// need no ambient runtime either.
/// 中文说明：按 `parsed.command` 分发到 restart 或 stop。因为调用方
/// 已持有进程的 tokio runtime，这里在 `std::thread::scope` 的专用
/// 线程上新建单线程 runtime 执行阻塞管线；worker 线程 panic 时统一
/// 转成 "lifecycle command panicked" 的 `CliError`。
pub fn command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let is_restart = parsed.command == "restart";
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| {
                    CliError::new(
                        format!("Could not create CLI runtime: {error}"),
                        GENERAL_ERROR,
                    )
                })?;
            runtime.block_on(async move {
                let mut sink = StdioSink;
                if is_restart {
                    restart_command_with_sink(&options, &mut sink).await
                } else {
                    stop_command_with_sink(&StopOptions::from_options(&options), &mut sink).await
                }
            })
        });
        worker
            .join()
            .unwrap_or_else(|_| Err(CliError::new("lifecycle command panicked", GENERAL_ERROR)))
    })
}

/// The slice of `Options` `stopCommand` reads (`cli-args.js` output). Restart
/// re-invokes stop with a synthetic quiet instance of this shape.
/// 中文说明：`stopCommand` 实际读取的 `Options` 子集（对应 JS
/// `cli-args.js` 的输出形状）。restart 内部复用 stop 时也以一份合成
/// 的 quiet 实例传入。
#[derive(Debug, Clone)]
struct StopOptions {
    /// 是否显式指定了端口（`--port`）；为 true 时按端口精确匹配而非全量停止。
    explicit_port: bool,
    /// 目标端口；未显式给出时回退到 [`DEFAULT_PORT`]。
    port: u16,
    /// 可选的 `--host` 覆盖值；影响探测与 shutdown 请求的目标地址。
    host: Option<String>,
    /// JSON 输出模式（`--json`）。
    json: bool,
    /// quiet 模式（`--quiet`）：每个实例输出一行精简结果。
    quiet: bool,
    /// 抑制 quiet 输出；restart 内部调用 stop 时置位以屏蔽中间结果。
    suppress_quiet_output: bool,
}

/// 中文说明：从全局 CLI 选项构造 `StopOptions` 的转换实现。
impl StopOptions {
    /// 截取完整 `Options` 中 stop 所需字段；端口缺省回退 [`DEFAULT_PORT`]。
    fn from_options(options: &Options) -> Self {
        Self {
            explicit_port: options.explicit_port,
            port: options.port.unwrap_or(DEFAULT_PORT),
            host: options.host.clone(),
            json: options.json,
            quiet: options.quiet,
            suppress_quiet_output: options.suppress_quiet_output,
        }
    }
}

/// Output sink so unit tests can assert the exact human/JSON/quiet shapes.
/// 中文说明：输出抽象——生产走 `StdioSink`，单元测试注入捕获型或空
/// sink 以精确断言 human/JSON/quiet 三种输出形态。
trait OutputSink {
    /// 输出一行 JSON 载荷（仅 json 模式调用）。
    fn print_json(&mut self, value: &serde_json::Value);
    /// 输出一行 stdout 文本。
    fn stdout_line(&mut self, line: &str);
    /// 输出一行 stderr 文本（quiet 模式的失败行）。
    fn stderr_line(&mut self, line: &str);
}

/// 生产环境的标准输出 sink：JSON 走全局 `print_json`，文本按行打印
/// 到 stdout/stderr。
struct StdioSink;

/// 中文说明：真实终端输出实现；`print_json` 复刻 JS `printJson`——
/// 载荷缺少 `status` 字段时在首位自动注入 `"ok"`。
impl OutputSink for StdioSink {
    /// 克隆载荷并在缺失时于对象首位注入 `status:"ok"` 后输出。
    fn print_json(&mut self, value: &serde_json::Value) {
        // JS printJson injects status:"ok" first when absent.
        let mut owned = value.clone();
        if let Some(map) = owned.as_object_mut() {
            if !map.contains_key("status") {
                map.shift_insert(0, "status".to_string(), serde_json::json!("ok"));
            }
        }
        print_json(&owned);
    }
    /// 按行写 stdout。
    fn stdout_line(&mut self, line: &str) {
        println!("{line}");
    }
    /// 按行写 stderr。
    fn stderr_line(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

/// Restart's inner stop/serve calls run fully suppressed (`quiet: true,
/// suppressQuietOutput: true` in the JS).
/// 中文说明：丢弃一切输出的空 sink，仅由 restart 内部复用 stop 时
/// 使用（对应 JS 的 `quiet: true, suppressQuietOutput: true`）。
struct NullSink;

/// 中文说明：全丢弃实现——所有方法均为空操作。
impl OutputSink for NullSink {
    /// 丢弃 JSON 输出。
    fn print_json(&mut self, _value: &serde_json::Value) {}
    /// 丢弃 stdout 行。
    fn stdout_line(&mut self, _line: &str) {}
    /// 丢弃 stderr 行。
    fn stderr_line(&mut self, _line: &str) {}
}

/// `logStatus` — plain message (+ optional detail) on stdout; the clack level
/// only drives glyphs/colors in the JS.
/// 中文说明：对应 JS `logStatus`——向 stdout 输出消息行与可选的
/// detail 行；`_level` 在 JS 侧仅决定 clack 图标/颜色，此处不参与
/// 格式化。
fn log_status(sink: &mut dyn OutputSink, _level: &str, message: &str, detail: Option<&str>) {
    sink.stdout_line(message);
    if let Some(detail) = detail {
        sink.stdout_line(detail);
    }
}

/// `outro`/`finish` — a plain closing line in human mode only.
/// 中文说明：对应 JS `outro`/`finish`——仅在 human 模式
///（show_output 为 true）下输出一行收尾文本。
fn finish(sink: &mut dyn OutputSink, show_output: bool, text: &str) {
    if show_output {
        sink.stdout_line(text);
    }
}

/// `printQuietStopResults`.
/// 中文说明：对应 JS `printQuietStopResults`——quiet 模式逐行输出
/// stop 结果：成功走 stdout 的 "stopped <port>"，失败走 stderr 的
/// "failed <port> <reason>"（reason 缺省 "failed"），空结果输出
/// "none"；suppress、非 quiet 或 json 模式下不输出任何内容。
fn print_quiet_stop_results(
    opts: &StopOptions,
    json_results: &[serde_json::Value],
    sink: &mut dyn OutputSink,
) {
    if opts.suppress_quiet_output {
        return;
    }
    if !opts.quiet || opts.json {
        return;
    }
    if json_results.is_empty() {
        sink.stdout_line("none");
        return;
    }
    for result in json_results {
        let port = result
            .get("port")
            .and_then(|value| value.as_u64())
            .unwrap_or_default();
        if result.get("stopped").and_then(|value| value.as_bool()) == Some(true) {
            sink.stdout_line(&format!("stopped {port}"));
        } else {
            let reason = result
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("failed");
            sink.stderr_line(&format!("failed {port} {reason}"));
        }
    }
}

// ── cli-process.js identity state ──────────────────────────────────────────

/// `getOmpchamberProcessState`.
/// 中文说明：对应 JS `getOmpchamberProcessState` 的四态身份分类，
/// 决定注册表条目是可信、待确认还是应清理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessState {
    /// 进程不存在（pid 为 0 或已退出）。
    Dead,
    /// 进程存活但 cmdline 不可读（如权限不足），身份无法判定。
    Unknown,
    /// cmdline 匹配 OMPChamber 进程特征。
    Matched,
    /// 进程存活但 cmdline 不属于 OMPChamber（pid 已被复用）。
    Mismatched,
}

/// 判定 pid 的存活与 cmdline 身份：pid 为 0 或已退出 →
/// [`ProcessState::Dead`]；存活但 cmdline 不可读 → `Unknown`；可读
/// 则按是否匹配 OMPChamber 命令行返回 `Matched`/`Mismatched`。
fn get_ompchamber_process_state(pid: u32) -> ProcessState {
    if pid == 0 || !process::is_process_running(pid) {
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

/// `waitForProcessExit`: poll liveness every 150ms until it exits or the
/// timeout elapses (a zero timeout still performs one liveness check).
/// 中文说明：对应 JS `waitForProcessExit`——每 150ms 轮询一次存活
/// 状态，进程退出返回 true、超时返回 false；timeout 为 0 仍执行一次
/// 存活检查。pid 为 0 直接视为已退出。
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

/// `stopInstanceProcess`: give the server `shutdownWaitMs` to exit on its own
/// after a shutdown request, then fall back to `terminateProcessTree`.
/// 中文说明：对应 JS `stopInstanceProcess`——shutdown 请求发出后先
/// 给 `shutdown_wait_ms` 毫秒自行退出的宽限，到时未退则调用
/// `terminate_process_tree`（graceful_timeout_ms → force_timeout_ms
/// 两级升级）兜底；返回最终是否确认退出。pid 为 0 直接成功。
async fn stop_instance_process(
    pid: u32,
    shutdown_wait_ms: u64,
    graceful_timeout_ms: u64,
    force_timeout_ms: u64,
) -> bool {
    if pid == 0 {
        return true;
    }
    if wait_for_process_exit(pid, shutdown_wait_ms).await {
        return true;
    }
    process::terminate_process_tree(pid, graceful_timeout_ms, force_timeout_ms);
    !process::is_process_running(pid)
}

// ── cli-network.js host/URL resolution (probe subset) ──────────────────────

/// `resolveConfiguredBindHost`.
/// 中文说明：对应 JS `resolveConfiguredBindHost`——解析生效的绑定
/// host：显式覆盖值 > 环境变量 `OMPCHAMBER_HOST` > 默认 "127.0.0.1"；
/// 空白值一律视为未提供。
fn resolve_configured_bind_host(host_override: Option<&str>) -> String {
    if let Some(host) = host_override.map(str::trim).filter(|host| !host.is_empty()) {
        return host.to_string();
    }
    std::env::var("OMPCHAMBER_HOST")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// `resolveApiHost`: wildcard bind hosts are not valid destinations.
/// 中文说明：对应 JS `resolveApiHost`——把绑定 host 归一为可请求的
/// 目的地址：通配地址（"0.0.0.0"→"127.0.0.1"，"::"/"[::]"→"::1"）
/// 不是合法目的地；方括号包裹形式去括号，其余原样返回。
fn resolve_api_host(host_override: Option<&str>) -> String {
    let configured = resolve_configured_bind_host(host_override);
    match configured.as_str() {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" | "[::]" => "::1".to_string(),
        s if s.starts_with('[') && s.ends_with(']') => s[1..s.len() - 1].to_string(),
        s => s.to_string(),
    }
}

/// `formatHostForUrl`: bracket IPv6 for URL usage.
/// 中文说明：对应 JS `formatHostForUrl`——含冒号的 host（IPv6）加
/// 方括号后返回，便于拼进 `http://host:port/path`。
fn format_host_for_url(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// `buildLocalUrl`.
/// 中文说明：对应 JS `buildLocalUrl`——拼出本地 API 地址
/// `http://{host}:{port}{path}`：host 经 `resolve_api_host` 归一并
/// 加 IPv6 方括号，endpoint 缺少前导斜杠时补上。
fn build_local_url(port: u16, endpoint: &str, host_override: Option<&str>) -> String {
    let host = format_host_for_url(&resolve_api_host(host_override));
    let path = if endpoint.starts_with('/') {
        endpoint.to_string()
    } else {
        format!("/{endpoint}")
    };
    format!("http://{host}:{port}{path}")
}

// ── cli-ports.js ────────────────────────────────────────────────────────────

/// Bind classification for the availability probe: `Some(ok)` when the bind
/// succeeded or definitively lost the race, `None` when the address family is
/// unavailable on this host.
/// 中文说明：对应 JS 侧端口探测的 bind 分类——一次真实的
/// `TcpListener::bind` 即释放，不做监听。
fn bind_result(host: &str, port: u16) -> Option<bool> {
    match TcpListener::bind((host, port)) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Some(false),
        Err(_) => None,
    }
}

/// `isPortAvailable`: bind the port (host if given). std sets `SO_REUSEADDR`,
/// which on BSD lets a wildcard bind succeed over an existing specific-address
/// listener — so the no-host case probes the specific loopback addresses the
/// CLI itself would talk to (Node's `listen({ port })` dual-stack semantics,
/// adapted): the port is available exactly where a fresh instance could bind.
/// 中文说明：对应 JS `isPortAvailable`——判断端口是否可供新实例绑定。
/// 给定 host 时直接试绑该地址；未给 host 时因 std 会设 `SO_REUSEADDR`
///（BSD 上通配绑定可能压过既有的具体地址监听），改为探测 CLI 自身
/// 会访问的具体回环地址（127.0.0.1 与 ::1，等价适配 Node
/// `listen({ port })` 的双栈语义）：任一明确被占即不可用，任一可绑
/// 即视为可用。端口 0 恒为不可用。
fn is_port_available(port: u16, host: Option<&str>) -> bool {
    if port == 0 {
        return false;
    }
    match host.map(str::trim).filter(|host| !host.is_empty()) {
        Some(host) => {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            TcpListener::bind((host, port)).is_ok()
        }
        None => {
            let ipv4 = bind_result("127.0.0.1", port);
            let ipv6 = bind_result("::1", port);
            if ipv4 == Some(false) || ipv6 == Some(false) {
                return false;
            }
            ipv4 == Some(true) || ipv6 == Some(true)
        }
    }
}

// ── cli-http.js probe subset ────────────────────────────────────────────────

/// The `/api/system/info` fields discovery consumes.
/// 中文说明：发现流程消费的 `/api/system/info` 字段子集，对应 JS
/// `fetchSystemInfoFromPort` 的返回值形状。
#[derive(Debug, Clone)]
struct SystemInfo {
    /// 运行时标识（如 "web"、"desktop"）；空串视为无效响应。
    runtime: String,
    /// 服务端上报的 pid（正整数）；缺失或非法时为 `None`。
    pid: Option<u32>,
}

/// `fetchSystemInfoFromPort` (1.5s timeout, `runtime` must be a string).
/// 中文说明：对应 JS `fetchSystemInfoFromPort`——GET 目标端口的
/// `/api/system/info`（1.5s 超时），要求响应为 JSON 且 `runtime` 是
/// 非空字符串；网络错误、非 2xx、字段缺失或类型不符均返回 `None`，
/// pid 字段宽松解析为可选正整数。端口 0 直接返回 `None`。
async fn fetch_system_info_from_port(port: u16, host_override: Option<&str>) -> Option<SystemInfo> {
    if port == 0 {
        return None;
    }
    let url = build_local_url(port, "/api/system/info", host_override);
    let response = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_millis(1500))
        .header("Accept", "application/json")
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
        .and_then(|value| value.as_f64())
        .filter(|pid| pid.is_finite() && *pid > 0.0)
        .and_then(|pid| u32::try_from(pid as i64).ok());
    Some(SystemInfo { runtime, pid })
}

/// `requestServerShutdown` (POST, 1.5s timeout, success iff 2xx).
/// 中文说明：对应 JS `requestServerShutdown`——POST 目标端口的
/// `/api/system/shutdown`（1.5s 超时），当且仅当响应为 2xx 时返回
/// true；端口 0 直接失败。
async fn request_server_shutdown(port: u16, host_override: Option<&str>) -> bool {
    if port == 0 {
        return false;
    }
    let url = build_local_url(port, "/api/system/shutdown", host_override);
    matches!(
        reqwest::Client::new()
            .post(&url)
            .timeout(Duration::from_millis(1500))
            .send()
            .await,
        Ok(response) if response.status().is_success()
    )
}

/// `normalizeProbeHost`.
/// 中文说明：对应 JS `normalizeProbeHost`——去除首尾空白并过滤空值；
/// `None` 或纯空白输入返回 `None`。
fn normalize_probe_host(host: Option<&str>) -> Option<String> {
    host.map(str::trim)
        .filter(|host| !host.is_empty())
        .map(|host| host.to_string())
}

/// `isWildcardProbeHost`.
/// 中文说明：对应 JS `isWildcardProbeHost`——是否为通配绑定地址
///（"0.0.0.0"、"::"、"[::]"）。
fn is_wildcard_probe_host(host: &str) -> bool {
    matches!(host.trim(), "0.0.0.0" | "::" | "[::]")
}

/// `isLoopbackProbeHost`.
/// 中文说明：对应 JS `isLoopbackProbeHost`——是否为回环地址
///（"127.0.0.1"、"localhost"、"::1"、"[::1]"）。
fn is_loopback_probe_host(host: &str) -> bool {
    matches!(host.trim(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// `isConcreteProbeHost`.
/// 中文说明：对应 JS `isConcreteProbeHost`——非空且既非通配也非回环
/// 的具体地址；此类地址被视为权威目标，会使回环兜底候选退化为需
/// pid 匹配。
fn is_concrete_probe_host(host: &str) -> bool {
    normalize_probe_host(Some(host)).is_some_and(|normalized| {
        !is_wildcard_probe_host(&normalized) && !is_loopback_probe_host(&normalized)
    })
}

/// 单个探测候选：目标 host（`None` 表示默认解析）以及命中时是否必须
/// 与期望 pid 匹配。
#[derive(Debug, Clone)]
struct ProbeHost {
    /// 探测目标 host；`None` 表示不覆盖、走默认 host 解析。
    host: Option<String>,
    /// 为 true 时，info 响应中的 pid 必须等于期望 pid 才算命中。
    requires_pid_match: bool,
}

/// 去重后追加一个探测候选：以 `resolve_api_host` 归一化后的地址为键，
/// 已存在同键候选则跳过，保持既有候选的优先级顺序不变。
fn push_probe_host(out: &mut Vec<ProbeHost>, host: Option<String>, requires_pid_match: bool) {
    let key = resolve_api_host(host.as_deref());
    if out
        .iter()
        .any(|entry| resolve_api_host(entry.host.as_deref()) == key)
    {
        return;
    }
    out.push(ProbeHost {
        host,
        requires_pid_match,
    });
}
/// `getSystemInfoProbeHosts`: explicit hosts first (authoritative), then the
/// loopback fallbacks — which must pid-match whenever a concrete host was
/// supplied, so a recycled pid on a wildcard-bound registry entry cannot be
/// confirmed by an unrelated local instance.
/// 中文说明：对应 JS `getSystemInfoProbeHosts`——构造探测候选序列：
/// 显式给出的 host 排最前（权威、无需 pid 匹配），随后是回环兜底
/// 候选；只要存在任一具体（非通配非回环）host，兜底候选就必须 pid
/// 匹配——防止通配绑定的注册表条目被一个不相关但复用了同一 pid 的
/// 本机实例误确认。
fn get_system_info_probe_hosts(hosts: &[Option<String>]) -> Vec<ProbeHost> {
    let mut out = Vec::new();
    let has_concrete_authoritative_host = hosts
        .iter()
        .any(|host| host.as_deref().is_some_and(is_concrete_probe_host));
    for host in hosts {
        if let Some(normalized) = normalize_probe_host(host.as_deref()) {
            push_probe_host(&mut out, Some(normalized), false);
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

/// `fetchSystemInfoFromPortCandidates`.
/// 中文说明：对应 JS `fetchSystemInfoFromPortCandidates`——按优先级
/// 顺序逐个尝试候选 host：返回第一个成功给出 system info 且（若要求
/// pid 匹配）pid 与 `expected_pid` 一致的候选；返回 (info, 命中
/// host)，全部失败返回 (None, None)。
async fn fetch_system_info_from_port_candidates(
    port: u16,
    hosts: &[ProbeHost],
    expected_pid: u32,
) -> (Option<SystemInfo>, Option<String>) {
    for probe in hosts {
        if let Some(info) = fetch_system_info_from_port(port, probe.host.as_deref()).await {
            if probe.requires_pid_match && info.pid != Some(expected_pid) {
                continue;
            }
            return (Some(info), probe.host.clone());
        }
    }
    (None, None)
}

// ── cli-lifecycle.js discovery ──────────────────────────────────────────────

/// 实例的发现来源，决定 stop 走注册表停机、无注册停机还是按 pid
/// 强停分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstanceSource {
    /// Confirmed pid file + live `/api/system/info`.
    /// 中文：注册表 pid 文件与存活的 `/api/system/info` 双重确认。
    RegistryProbe,
    /// No registry files; a live port answered `/api/system/info`.
    /// 中文：无注册表文件，仅端口上的 `/api/system/info` 存活应答。
    Probe,
    /// Pid file with a cmdline-matched pid whose port has no reachable
    /// OMPChamber HTTP surface.
    /// 中文：pid 文件存在且 cmdline 匹配，但端口上没有可达的
    /// OMPChamber HTTP 面。
    RegistryUnconfirmed,
}

/// 一个已发现的运行实例：端口、pid、注册表文件路径、来源分类及发现
/// 时采集的元数据；stop/restart 据此选择停机方式并清理注册文件。
#[derive(Debug, Clone)]
struct DiscoveredInstance {
    /// 实例监听端口。
    port: u16,
    /// 实例 pid；无法确定时为 `None`。
    pid: Option<u32>,
    /// run 目录中的 pid 注册文件路径。
    pid_file_path: PathBuf,
    /// run 目录中的实例选项 JSON 文件路径。
    instance_file_path: PathBuf,
    /// Discovery-record parity with the JS shape; consumed by the logs
    /// command's `getLatestInstance` tie-breaking (ported with the logs
    /// command), not by stop/restart.
    /// 中文：pid 文件的 mtime（Unix 纪元毫秒）。
    #[allow(dead_code)]
    mtime_ms: f64,
    /// 实例启动时间戳（毫秒），与 JS 发现记录形状对齐；stop/restart
    /// 不消费该字段。
    #[allow(dead_code)]
    started_at: f64,
    /// 启动模式："daemon" 或 "foreground"。
    launch_mode: String,
    /// 运行时标识："web"、"desktop"、"cli"（未确认）等。
    runtime: String,
    /// 发现来源分类，见 [`InstanceSource`]。
    source: InstanceSource,
    /// 确认实例时命中的探测 host（未命中具体地址时为 `None`）。
    host: Option<String>,
}

/// `hasOmpchamberRuntimeInfo`.
/// 中文说明：对应 JS `hasOmpchamberRuntimeInfo`——runtime 非空即视为
/// 有效的 OMPChamber 标识。
fn has_ompchamber_runtime_info(info: &SystemInfo) -> bool {
    !info.runtime.is_empty()
}

/// `createLivePortInstance`.
/// 中文说明：对应 JS `createLivePortInstance`——把端口探测到的
/// system info 包装成来源为 `Probe` 的实例；runtime 为空（非
/// OMPChamber 响应）返回 `None`。注册表字段指向该端口的标准路径，
/// 时间戳置 0、launch_mode 固定 "daemon"。
fn create_live_port_instance(
    port: u16,
    info: &SystemInfo,
    host: Option<String>,
) -> Option<DiscoveredInstance> {
    if !has_ompchamber_runtime_info(info) {
        return None;
    }
    Some(DiscoveredInstance {
        port,
        pid: info.pid,
        pid_file_path: paths::pid_file_path(port),
        instance_file_path: paths::instance_file_path(port),
        mtime_ms: 0.0,
        started_at: 0.0,
        launch_mode: "daemon".to_string(),
        runtime: info.runtime.clone(),
        source: InstanceSource::Probe,
        host: normalize_probe_host(host.as_deref()),
    })
}

/// `isDesktopRuntimeForPort`.
/// 中文说明：对应 JS `isDesktopRuntimeForPort`——仅当 runtime 为
/// "desktop" 且（settings 未配置 desktopLocalPort 或配置恰为本端口）
/// 时为 true；用于区分本应用自己的 desktop 实例与别人的 desktop
/// 实例。
fn is_desktop_runtime_for_port(info: &SystemInfo, port: u16) -> bool {
    if info.runtime != "desktop" {
        return false;
    }
    match paths::read_desktop_local_port_from_settings() {
        None => true,
        Some(desktop_port) => desktop_port == port,
    }
}

/// 读取文件 mtime 并换算为 Unix 纪元起的毫秒数；任何失败（文件不
/// 存在、元数据不可读等）都回退为 0.0。
fn file_mtime_ms(path: &std::path::Path) -> f64 {
    std::fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or(0.0)
}

/// `discoverRunningInstances`: walk the run-dir pid files, prune dead/mismatched
/// registrations, confirm each live one over HTTP, and drop desktop entries.
/// 中文说明：对应 JS `discoverRunningInstances`——遍历 run 目录的
/// pid 文件：清理死进程与身份不符的注册项，对每个存活注册经 HTTP
/// 确认后收入结果，并同步删除 desktop 运行时条目的注册文件。结果按
/// 端口升序返回。
async fn discover_running_instances(host_option: Option<&str>) -> Vec<DiscoveredInstance> {
    let mut instances = Vec::new();
    let run_dir = paths::run_dir();
    let Ok(entries) = std::fs::read_dir(&run_dir) else {
        return instances;
    };
    let mut pid_files: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with("ompchamber-") && name.ends_with(".pid"))
        .collect();
    pid_files.sort();

    for file in pid_files {
        let Some(port) = file
            .strip_prefix("ompchamber-")
            .and_then(|rest| rest.strip_suffix(".pid"))
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port > 0)
        else {
            continue;
        };
        let pid_file_path = run_dir.join(&file);
        let instance_file_path = run_dir.join(format!("ompchamber-{port}.json"));

        let Some(pid) = process::read_pid_file(&pid_file_path) else {
            process::remove_pid_file(&pid_file_path);
            process::remove_instance_file(&instance_file_path);
            continue;
        };

        let stored = process::read_instance_options(&instance_file_path);
        let state = get_ompchamber_process_state(pid);
        if state == ProcessState::Dead {
            process::remove_pid_file(&pid_file_path);
            process::remove_instance_file(&instance_file_path);
            continue;
        }

        // A live PID-file is only the right instance if the recorded port also
        // confirms OMPChamber. Cmdline identity alone can match a recycled PID
        // from another OMPChamber process on a different port. Try all plausible
        // hosts first; if matched/unknown identity still can't be confirmed,
        // keep the registry files but don't claim the instance is running.
        let hosts = get_system_info_probe_hosts(&[
            stored.as_ref().and_then(|options| options.host.clone()),
            host_option.map(str::to_string),
        ]);
        let (live_info, confirmed_host) =
            fetch_system_info_from_port_candidates(port, &hosts, pid).await;
        let Some(info) = live_info else {
            if state == ProcessState::Mismatched {
                process::remove_pid_file(&pid_file_path);
                process::remove_instance_file(&instance_file_path);
            }
            continue;
        };

        if info.runtime == "desktop" {
            process::remove_pid_file(&pid_file_path);
            process::remove_instance_file(&instance_file_path);
            continue;
        }

        let launch_mode = if stored
            .as_ref()
            .is_some_and(|options| options.launch_mode == "foreground")
        {
            "foreground"
        } else {
            "daemon"
        };
        instances.push(DiscoveredInstance {
            port,
            pid: info.pid.or(if state == ProcessState::Matched {
                Some(pid)
            } else {
                None
            }),
            pid_file_path: pid_file_path.clone(),
            instance_file_path,
            mtime_ms: file_mtime_ms(&pid_file_path),
            started_at: stored
                .as_ref()
                .map(|options| options.started_at)
                .filter(|started_at| started_at.is_finite())
                .unwrap_or(0.0),
            launch_mode: launch_mode.to_string(),
            runtime: info.runtime,
            source: InstanceSource::RegistryProbe,
            host: confirmed_host.filter(|host| !host.is_empty()).or_else(|| {
                stored
                    .as_ref()
                    .and_then(|options| options.host.clone())
                    .filter(|host| !host.is_empty())
            }),
        });
    }

    instances.sort_by_key(|instance| instance.port);
    instances
}

/// `discoverOMPChamberInstanceOnPort`.
/// 中文说明：对应 JS `discoverOMPChamberInstanceOnPort`——定位指定
/// 端口的实例：优先复用注册表发现结果中同端口的条目；否则直接探测
/// 该端口的 system info。desktop 运行时需通过
/// `is_desktop_runtime_for_port` 校验（他人的 desktop 端口返回
/// `None`）。端口 0 直接未命中。
async fn discover_ompchamber_instance_on_port(
    port: u16,
    host: Option<String>,
    running_instances: &[DiscoveredInstance],
) -> Option<DiscoveredInstance> {
    if port == 0 {
        return None;
    }
    if let Some(registry_match) = running_instances
        .iter()
        .find(|instance| instance.port == port)
    {
        return Some(registry_match.clone());
    }
    let info = fetch_system_info_from_port(port, host.as_deref()).await?;
    if info.runtime == "desktop" && !is_desktop_runtime_for_port(&info, port) {
        return None;
    }
    create_live_port_instance(port, &info, host)
}

/// `discoverLifecycleInstances`.
/// 中文说明：对应 JS `discoverLifecycleInstances`——未显式指定端口时
/// 返回全部运行实例；显式指定端口时先查注册表命中，未命中再退化为
/// 单端口探测，均未命中返回空表（是否走未确认兜底由调用方决定）。
async fn discover_lifecycle_instances(opts: &StopOptions) -> Vec<DiscoveredInstance> {
    let running_instances = discover_running_instances(opts.host.as_deref()).await;
    if !opts.explicit_port {
        return running_instances;
    }
    if let Some(found) = running_instances
        .iter()
        .find(|instance| instance.port == opts.port)
    {
        return vec![found.clone()];
    }
    match discover_ompchamber_instance_on_port(opts.port, opts.host.clone(), &running_instances)
        .await
    {
        Some(live_instance) => vec![live_instance],
        None => Vec::new(),
    }
}

/// `discoverUnconfirmedRegistryInstanceOnPort`: a cmdline-matched pid whose
/// port is occupied but has no reachable OMPChamber HTTP surface.
/// 中文说明：对应 JS `discoverUnconfirmedRegistryInstanceOnPort`——
/// 兜底发现：pid 文件存在、pid 的 cmdline 匹配 OMPChamber、端口被占
/// 但没有可达的 HTTP 面时，返回一个 `RegistryUnconfirmed` 实例
///（runtime 记为 "cli"）；pid 已死或端口实际空闲则清理注册文件并
/// 返回 `None`。
async fn discover_unconfirmed_registry_instance_on_port(
    port: u16,
    host_option: Option<&str>,
) -> Option<DiscoveredInstance> {
    if port == 0 {
        return None;
    }
    let pid_file_path = paths::pid_file_path(port);
    let pid = process::read_pid_file(&pid_file_path)?;
    let instance_file_path = paths::instance_file_path(port);
    let stored = process::read_instance_options(&instance_file_path);

    let state = get_ompchamber_process_state(pid);
    if state == ProcessState::Dead {
        process::remove_pid_file(&pid_file_path);
        process::remove_instance_file(&instance_file_path);
        return None;
    }
    if state != ProcessState::Matched {
        return None;
    }

    let host = stored
        .as_ref()
        .and_then(|options| options.host.clone())
        .filter(|host| !host.is_empty())
        .or_else(|| normalize_probe_host(host_option));
    if is_port_available(port, host.as_deref()) {
        process::remove_pid_file(&pid_file_path);
        process::remove_instance_file(&instance_file_path);
        return None;
    }

    Some(DiscoveredInstance {
        port,
        pid: Some(pid),
        pid_file_path,
        instance_file_path,
        mtime_ms: 0.0,
        started_at: stored
            .as_ref()
            .map(|options| options.started_at)
            .filter(|started_at| started_at.is_finite())
            .unwrap_or(0.0),
        launch_mode: if stored
            .as_ref()
            .is_some_and(|options| options.launch_mode == "foreground")
        {
            "foreground".to_string()
        } else {
            "daemon".to_string()
        },
        runtime: "cli".to_string(),
        source: InstanceSource::RegistryUnconfirmed,
        host: host.filter(|host| !host.is_empty()),
    })
}

// ── stop ────────────────────────────────────────────────────────────────────

/// Stop one registry-confirmed instance. `Err(message)` mirrors the JS thrown
/// `Error` (recorded as the JSON `reason`).
/// 中文说明：停掉一个注册表确认的实例——host 优先取实例自带值、
/// 否则用 fallback：先 POST shutdown（请求成功给 5s 自退宽限，失败
/// 则 0 等待立即兜底），由 `stop_instance_process` 终止进程树，最后
/// 清理 pid/instance 注册文件；进程仍存活时返回
/// Err("Timed out stopping pid ...")。
async fn stop_registered_instance(
    instance: &DiscoveredInstance,
    fallback_host: Option<&str>,
) -> Result<(), String> {
    let host = instance
        .host
        .clone()
        .filter(|host| !host.is_empty())
        .or_else(|| fallback_host.map(str::to_string));
    let requested = request_server_shutdown(instance.port, host.as_deref()).await;
    let pid = instance.pid.unwrap_or(0);
    let stopped = stop_instance_process(pid, if requested { 5000 } else { 0 }, 2500, 3000).await;
    if !stopped && pid != 0 && process::is_process_running(pid) {
        return Err(format!("Timed out stopping pid {pid}"));
    }
    process::remove_pid_file(&instance.pid_file_path);
    process::remove_instance_file(&instance.instance_file_path);
    Ok(())
}

/// `stopCommand`.
/// 中文说明：对应 JS `stopCommand`——stop 主流程：发现实例后按来源
/// 分支处理（显式端口：not-found / desktop 豁免 / 无注册实例 / 未确认
/// 实例；全量模式：逐个停掉注册表实例），并按 json/quiet/human 三种
/// 形态输出。所有分支都返回 `Ok(())`，失败信息记录在 JSON results
/// 的 reason 字段中。
async fn stop_command_with_sink(
    opts: &StopOptions,
    sink: &mut dyn OutputSink,
) -> Result<(), CliError> {
    let show_output = !opts.json && !opts.quiet;
    let mut json_results: Vec<serde_json::Value> = Vec::new();

    if show_output {
        sink.stdout_line("OMPChamber Stop");
    }

    let mut running_instances = discover_lifecycle_instances(opts).await;
    if opts.explicit_port {
        if running_instances.is_empty() {
            if let Some(unconfirmed) =
                discover_unconfirmed_registry_instance_on_port(opts.port, opts.host.as_deref())
                    .await
            {
                running_instances.push(unconfirmed);
            }
        }

        if running_instances.is_empty() {
            json_results.push(
                serde_json::json!({ "port": opts.port, "stopped": false, "reason": "not-found" }),
            );
            if opts.json {
                sink.print_json(&serde_json::json!({
                    "stoppedCount": 0,
                    "results": json_results.clone(),
                }));
            }
            if show_output {
                log_status(
                    sink,
                    "info",
                    &format!("no OMPChamber instance found on port {}", opts.port),
                    None,
                );
                finish(sink, show_output, "nothing to stop");
            }
            print_quiet_stop_results(opts, &json_results, sink);
            return Ok(());
        }

        let explicit_instance = &running_instances[0];
        if explicit_instance.runtime == "desktop" {
            json_results.push(serde_json::json!({
                "port": opts.port,
                "runtime": "desktop",
                "stopped": false,
                "reason": "desktop-managed",
            }));
            if opts.json {
                sink.print_json(&serde_json::json!({
                    "stoppedCount": 0,
                    "results": json_results.clone(),
                    "messages": [{
                        "level": "warning",
                        "code": "DESKTOP_MANAGED_PORT",
                        "message": format!("Port {} is managed by OMPChamber Desktop and cannot be stopped with this command.", opts.port),
                    }],
                }));
            }
            if show_output {
                log_status(
                    sink,
                    "warning",
                    &format!("port {} is managed by OMPChamber Desktop", opts.port),
                    Some("cannot be stopped with this command"),
                );
                finish(sink, show_output, "no changes applied");
            }
            print_quiet_stop_results(opts, &json_results, sink);
            return Ok(());
        }

        if explicit_instance.source == InstanceSource::Probe {
            if show_output {
                log_status(
                    sink,
                    "info",
                    &format!("found unmanaged OMPChamber instance on port {}", opts.port),
                    Some("attempting shutdown"),
                );
            }
            let requested = request_server_shutdown(opts.port, opts.host.as_deref()).await;

            if let Some(pid) = explicit_instance.pid.filter(|pid| *pid != 0) {
                if process::is_process_running(pid) {
                    let _ =
                        stop_instance_process(pid, if requested { 5000 } else { 0 }, 2500, 3000)
                            .await;
                }
            }

            if is_port_available(opts.port, opts.host.as_deref()) {
                json_results.push(serde_json::json!({
                    "port": opts.port,
                    "runtime": "unmanaged",
                    "stopped": true,
                }));
                if opts.json {
                    sink.print_json(
                        &serde_json::json!({ "stoppedCount": 1, "results": json_results.clone() }),
                    );
                }
                if show_output {
                    log_status(
                        sink,
                        "success",
                        &format!("stopped OMPChamber on port {}", opts.port),
                        None,
                    );
                    finish(sink, show_output, "stop complete");
                }
            } else if requested {
                json_results.push(serde_json::json!({
                    "port": opts.port,
                    "runtime": "unmanaged",
                    "stopped": false,
                    "reason": "shutdown-requested-port-busy",
                }));
                if opts.json {
                    sink.print_json(&serde_json::json!({
                        "status": "warning",
                        "stoppedCount": 0,
                        "results": json_results.clone(),
                        "messages": [{
                            "level": "warning",
                            "code": "SHUTDOWN_PARTIAL",
                            "message": format!("Shutdown was requested for port {}, but the port is still occupied.", opts.port),
                        }],
                    }));
                }
                if show_output {
                    log_status(
                        sink,
                        "warning",
                        &format!("shutdown requested on port {}", opts.port),
                        Some("port is still occupied"),
                    );
                    finish(sink, show_output, "partial stop");
                }
            } else {
                json_results.push(serde_json::json!({
                    "port": opts.port,
                    "runtime": "unmanaged",
                    "stopped": false,
                    "reason": "stop-failed",
                }));
                if opts.json {
                    sink.print_json(&serde_json::json!({
                        "status": "error",
                        "stoppedCount": 0,
                        "results": json_results.clone(),
                        "messages": [{
                            "level": "error",
                            "code": "STOP_FAILED",
                            "message": format!("Could not stop OMPChamber on port {}.", opts.port),
                        }],
                    }));
                }
                if show_output {
                    log_status(
                        sink,
                        "error",
                        &format!("could not stop OMPChamber on port {}", opts.port),
                        None,
                    );
                    finish(sink, show_output, "failed");
                }
            }
            print_quiet_stop_results(opts, &json_results, sink);
            return Ok(());
        }

        if explicit_instance.source == InstanceSource::RegistryUnconfirmed {
            let pid = explicit_instance.pid.unwrap_or(0);
            if show_output {
                log_status(
                    sink,
                    "info",
                    &format!(
                        "found unconfirmed OMPChamber pid {pid} on port {}",
                        opts.port
                    ),
                    Some("HTTP shutdown endpoint is unreachable; stopping by PID"),
                );
            }
            let stopped = stop_instance_process(pid, 0, 2500, 3000).await;

            if stopped || (pid != 0 && !process::is_process_running(pid)) {
                process::remove_pid_file(&explicit_instance.pid_file_path);
                process::remove_instance_file(&explicit_instance.instance_file_path);
                json_results.push(serde_json::json!({
                    "port": opts.port,
                    "pid": explicit_instance.pid,
                    "runtime": "unconfirmed",
                    "stopped": true,
                }));
                if opts.json {
                    sink.print_json(
                        &serde_json::json!({ "stoppedCount": 1, "results": json_results.clone() }),
                    );
                }
                if show_output {
                    log_status(sink, "success", &format!("stopped pid {pid}"), None);
                    finish(sink, show_output, "stop complete");
                }
            } else {
                json_results.push(serde_json::json!({
                    "port": opts.port,
                    "pid": explicit_instance.pid,
                    "runtime": "unconfirmed",
                    "stopped": false,
                    "reason": "stop-failed",
                }));
                if opts.json {
                    sink.print_json(&serde_json::json!({
                        "status": "error",
                        "stoppedCount": 0,
                        "results": json_results.clone(),
                        "messages": [{
                            "level": "error",
                            "code": "STOP_FAILED",
                            "message": format!("Could not stop OMPChamber PID {pid}."),
                        }],
                    }));
                }
                if show_output {
                    log_status(sink, "error", &format!("could not stop pid {pid}"), None);
                    finish(sink, show_output, "failed");
                }
            }
            print_quiet_stop_results(opts, &json_results, sink);
            return Ok(());
        }
    } else if running_instances.is_empty() {
        if opts.json {
            sink.print_json(&serde_json::json!({ "stoppedCount": 0, "results": [] }));
        }
        if show_output {
            log_status(sink, "info", "No running OMPChamber instances found", None);
            finish(sink, show_output, "nothing to stop");
        }
        print_quiet_stop_results(opts, &json_results, sink);
        return Ok(());
    }

    for instance in &running_instances {
        if show_output {
            let pid_text = instance
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "null".to_string());
            log_status(
                sink,
                "info",
                &format!("stopping port {} (PID: {})", instance.port, pid_text),
                None,
            );
        }
        match stop_registered_instance(instance, opts.host.as_deref()).await {
            Ok(()) => {
                json_results.push(serde_json::json!({
                    "port": instance.port,
                    "pid": instance.pid,
                    "stopped": true,
                }));
                if show_output {
                    log_status(
                        sink,
                        "success",
                        &format!("stopped port {}", instance.port),
                        None,
                    );
                }
            }
            Err(message) => {
                json_results.push(serde_json::json!({
                    "port": instance.port,
                    "pid": instance.pid,
                    "stopped": false,
                    "reason": message,
                }));
                if show_output {
                    log_status(
                        sink,
                        "error",
                        &format!("error stopping port {}", instance.port),
                        Some(&message),
                    );
                } else if !opts.json && !opts.quiet {
                    sink.stderr_line(&format!(
                        "Error stopping port {}: {}",
                        instance.port, message
                    ));
                }
            }
        }
    }

    if opts.json {
        let stopped_count = json_results
            .iter()
            .filter(|entry| entry.get("stopped").and_then(|value| value.as_bool()) == Some(true))
            .count();
        let has_failure = json_results
            .iter()
            .any(|entry| entry.get("stopped").and_then(|value| value.as_bool()) != Some(true));
        sink.print_json(&serde_json::json!({
            "status": if has_failure { "warning" } else { "ok" },
            "stoppedCount": stopped_count,
            "results": json_results,
        }));
        return Ok(());
    }

    finish(
        sink,
        show_output,
        &format!("{} instance(s)", running_instances.len()),
    );
    print_quiet_stop_results(opts, &json_results, sink);
    Ok(())
}

// ── restart ─────────────────────────────────────────────────────────────────

/// `readInstanceOptions(...) || { port }`.
/// 中文说明：对应 JS `readInstanceOptions(...) || { port }`——实例
/// 选项文件缺失或不可读时的兜底值：仅保留端口，其余字段取中性默认。
fn fallback_instance_options(port: u16) -> process::InstanceOptions {
    process::InstanceOptions {
        port,
        host: None,
        launch_mode: String::new(),
        ui_password: None,
        has_ui_password: false,
        api_only: false,
        started_at: 0.0,
    }
}

/// Serve's `parsed` argument is unused by the ported command; hand it a
/// synthetic `serve` invocation.
/// 中文说明：restart 调用 serve 时其 `parsed` 参数未被移植版命令
/// 使用，这里合成一个最小的 "serve" 调用以满足函数签名。
fn synthetic_serve_parsed() -> Parsed {
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

/// `restartCommand`.
/// 中文说明：对应 JS `restartCommand`——restart 主流程：发现运行
/// 实例后逐个处理；desktop 条目直接拒绝并返回 DESKTOP_MANAGED_PORT。
/// foreground 模式只停不启（重启交给 systemd 等进程管理器），daemon
/// 模式停后等 500ms 再按存储的实例选项重建 serve（端口沿用显式
/// `--port` 或原端口；ui_password 取显式 CLI 值或存储值）。按
/// json/quiet/human 形态输出重启结果。
async fn restart_command_with_sink(
    options: &Options,
    sink: &mut dyn OutputSink,
) -> Result<(), CliError> {
    let json_mode = options.json;
    let quiet_mode = options.quiet;
    let show_output = !json_mode && !quiet_mode;
    let mut restarted: Vec<serde_json::Value> = Vec::new();

    if show_output {
        sink.stdout_line("OMPChamber Restart");
    }

    let running_instances = discover_lifecycle_instances(&StopOptions::from_options(options)).await;
    let total = running_instances.len();
    if running_instances.is_empty() {
        if json_mode {
            sink.print_json(&serde_json::json!({ "restartedCount": 0, "results": restarted }));
        } else if show_output {
            log_status(
                sink,
                "info",
                "No running OMPChamber instances to restart",
                None,
            );
            finish(sink, show_output, "nothing to restart");
        } else if quiet_mode {
            sink.stdout_line("restarted 0");
        }
        return Ok(());
    }

    for instance in running_instances {
        if instance.runtime == "desktop" {
            let message = format!(
                "Port {} is managed by OMPChamber Desktop and cannot be restarted with this command.",
                instance.port
            );
            if json_mode {
                sink.print_json(&serde_json::json!({
                    "status": "warning",
                    "restartedCount": 0,
                    "results": [{
                        "fromPort": instance.port,
                        "runtime": "desktop",
                        "ok": false,
                        "reason": "desktop-managed",
                    }],
                    "messages": [{
                        "level": "warning",
                        "code": "DESKTOP_MANAGED_PORT",
                        "message": message,
                    }],
                }));
                return Ok(());
            }
            if show_output {
                log_status(
                    sink,
                    "warning",
                    &format!("port {} is managed by OMPChamber Desktop", instance.port),
                    Some("cannot be restarted with this command"),
                );
                finish(sink, show_output, "no changes applied");
            } else if quiet_mode {
                sink.stdout_line("restarted 0");
            }
            return Ok(());
        }

        let stored = process::read_instance_options(&instance.instance_file_path)
            .unwrap_or_else(|| fallback_instance_options(instance.port));
        let instance_host = stored
            .host
            .clone()
            .filter(|host| !host.is_empty())
            .or_else(|| instance.host.clone())
            .or_else(|| options.host.clone());
        let launch_mode = if instance.launch_mode.is_empty() {
            "daemon".to_string()
        } else {
            instance.launch_mode.clone()
        };
        let is_foreground = launch_mode == "foreground";
        let restart_port = if options.explicit_port {
            options.port.unwrap_or(DEFAULT_PORT)
        } else {
            instance.port
        };

        if show_output {
            log_status(
                sink,
                "info",
                &format!("restarting port {}", instance.port),
                Some(&format!("mode: {launch_mode}")),
            );
        }

        // Stop half — quiet with fully suppressed output (JS passes
        // `quiet: true, suppressQuietOutput: true`).
        let mut stop_sink = NullSink;
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port: instance.port,
                host: instance_host.clone(),
                json: false,
                quiet: true,
                suppress_quiet_output: true,
            },
            &mut stop_sink,
        )
        .await?;

        // Foreground instances are managed by a process manager (systemd,
        // Docker, etc.) that will restart them automatically after stop.
        // Do not call serve() here — just record the stop as a successful
        // restart and let the process manager handle the actual restart.
        if is_foreground {
            restarted.push(serde_json::json!({
                "fromPort": instance.port,
                "toPort": restart_port,
                "launchMode": launch_mode,
                "ok": true,
            }));
            if show_output {
                log_status(
                    sink,
                    "success",
                    &format!("port {} stopped", instance.port),
                    Some("process manager will restart"),
                );
            }
            continue;
        }

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Serve half. JS forwards `uiPassword` WITHOUT `explicitUiPassword`, so
        // a non-empty stored/CLI password wins (hasUiPasswordConfigured) and a
        // bare `--ui-password` does not generate a new one on restart. Passing
        // `explicit_ui_password: true` with the resolved value makes the Rust
        // serve use it verbatim; absent → env-or-none, matching the daemon's
        // own env handling.
        let effective_ui_password = if options.explicit_ui_password {
            options
                .ui_password
                .clone()
                .filter(|password| !password.trim().is_empty())
        } else {
            stored
                .ui_password
                .clone()
                .filter(|password| !password.is_empty())
                .or_else(|| options.ui_password.clone())
        };
        let mut serve_options = Options::default();
        serve_options.port = Some(restart_port);
        serve_options.explicit_port = true;
        serve_options.host = instance_host.clone();
        serve_options.api_only = stored.api_only || options.api_only;
        serve_options.quiet = true;
        serve_options.suppress_startup_summary = true;
        serve_options.suppress_ui_password_warning = true;
        serve_options.suppress_quiet_output = true;
        if let Some(password) = effective_ui_password.filter(|password| !password.trim().is_empty())
        {
            serve_options.ui_password = Some(password);
            serve_options.explicit_ui_password = true;
        }

        if let Err(error) = serve::command(&synthetic_serve_parsed(), serve_options).await {
            if show_output {
                log_status(
                    sink,
                    "error",
                    &format!("failed to restart port {}", instance.port),
                    Some(&error.message),
                );
            }
            return Err(error);
        }
        // With `explicitPort: true` serve binds exactly `restart_port` (or
        // fails), so the JS `restartedPort` equals it.
        restarted.push(serde_json::json!({
            "fromPort": instance.port,
            "toPort": restart_port,
            "launchMode": launch_mode,
            "ok": true,
        }));
        if show_output {
            log_status(
                sink,
                "success",
                &format!("port {restart_port} restarted"),
                Some(&format!("mode: {launch_mode}")),
            );
        }
    }

    if json_mode {
        sink.print_json(
            &serde_json::json!({ "restartedCount": restarted.len(), "results": restarted }),
        );
        return Ok(());
    }
    if show_output {
        finish(sink, show_output, &format!("{total} instance(s) restarted"));
    } else if quiet_mode {
        sink.stdout_line(&format!("restarted {}", restarted.len()));
    }
    Ok(())
}

/// lifecycle 模块测试集：覆盖 host/URL 归一化、探测候选构造与去重、
/// quiet 输出形态、stop 各发现分支（not-found、死 pid、身份不符、
/// 注册表停机、desktop 豁免、无注册实例、未确认注册）、restart 分支，
/// 以及进程终止的宽限/强杀/超时语义。
#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate process env (`OMPCHAMBER_DATA_DIR`,
    /// `OMPCHAMBER_HOST`).
    /// 中文说明：复用 `cli::TEST_ENV_MUTEX`，串行化所有改动进程环境变量
    /// （`OMPCHAMBER_DATA_DIR`、`OMPCHAMBER_HOST`）的测试，避免并发互踩。
    fn env_mutex() -> &'static std::sync::Mutex<()> {
        &crate::cli::TEST_ENV_MUTEX
    }

    /// 测试环境守卫：记录并接管 `OMPCHAMBER_DATA_DIR`/`OMPCHAMBER_HOST`，
    /// Drop 时恢复原值；存活期间持有全局环境锁。
    struct EnvGuard {
        /// 接管前 `OMPCHAMBER_DATA_DIR` 的原值（原本不存在则为 `None`）。
        previous_data_dir: Option<String>,
        /// 接管前 `OMPCHAMBER_HOST` 的原值（原本不存在则为 `None`）。
        previous_host: Option<String>,
        /// 全局测试环境锁的守卫，保证环境变量修改期间其它测试不会介入。
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    /// 中文说明：EnvGuard 的构造入口实现。
    impl EnvGuard {
        /// Point `OMPCHAMBER_DATA_DIR` at a temp dir and scrub
        /// `OMPCHAMBER_HOST` so probe-host resolution is deterministic.
        /// 中文说明：先取得全局环境锁，再把 `OMPCHAMBER_DATA_DIR` 指向
        /// 给定临时目录并清空 `OMPCHAMBER_HOST`，让后续 probe-host
        /// 解析结果确定可复现。
        fn data_dir(dir: &std::path::Path) -> Self {
            let lock = env_mutex()
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let previous_data_dir = std::env::var("OMPCHAMBER_DATA_DIR").ok();
            let previous_host = std::env::var("OMPCHAMBER_HOST").ok();
            unsafe {
                std::env::set_var("OMPCHAMBER_DATA_DIR", dir);
                std::env::remove_var("OMPCHAMBER_HOST");
            }
            Self {
                previous_data_dir,
                previous_host,
                _lock: lock,
            }
        }
    }

    /// 中文说明：离开作用域时恢复被测试改写的环境变量。
    impl Drop for EnvGuard {
        /// 恢复 `OMPCHAMBER_DATA_DIR` 与 `OMPCHAMBER_HOST` 的原始值；
        /// 原本不存在的变量会被移除。环境锁随字段析构自然释放。
        fn drop(&mut self) {
            unsafe {
                match self.previous_data_dir.take() {
                    Some(value) => std::env::set_var("OMPCHAMBER_DATA_DIR", value),
                    None => std::env::remove_var("OMPCHAMBER_DATA_DIR"),
                }
                match self.previous_host.take() {
                    Some(value) => std::env::set_var("OMPCHAMBER_HOST", value),
                    None => std::env::remove_var("OMPCHAMBER_HOST"),
                }
            }
        }
    }

    /// 以随机字母数字后缀创建唯一的临时数据目录（先尝试清理同名残留
    /// 再新建），让每个测试的 run 目录注册文件互相隔离。
    fn temp_data_dir(label: &str) -> PathBuf {
        use rand::Rng;
        let suffix: String = rand::rng()
            .sample_iter(rand::distr::Alphanumeric)
            .take(12)
            .map(|byte| byte as char)
            .collect();
        let dir = std::env::temp_dir().join(format!("cli-lifecycle-{label}-{suffix}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 捕获型 sink：分别收集 JSON 载荷、stdout 行与 stderr 行，供测试
    /// 逐行断言三种输出形态的精确内容。
    #[derive(Default)]
    struct CaptureSink {
        /// 捕获到的 JSON 载荷列表（按输出顺序）。
        json: Vec<serde_json::Value>,
        /// 捕获到的 stdout 行列表。
        out: Vec<String>,
        /// 捕获到的 stderr 行列表。
        err: Vec<String>,
    }

    /// 中文说明：把三种输出原样追加进对应向量的收集实现。
    impl OutputSink for CaptureSink {
        /// 记录一次 JSON 载荷输出。
        fn print_json(&mut self, value: &serde_json::Value) {
            self.json.push(value.clone());
        }
        /// 记录一行 stdout 输出。
        fn stdout_line(&mut self, line: &str) {
            self.out.push(line.to_string());
        }
        /// 记录一行 stderr 输出。
        fn stderr_line(&mut self, line: &str) {
            self.err.push(line.to_string());
        }
    }

    /// JSON 输出模式的基准 `StopOptions`（未显式指定端口）。
    fn json_opts() -> StopOptions {
        StopOptions {
            explicit_port: false,
            port: 0,
            host: None,
            json: true,
            quiet: false,
            suppress_quiet_output: false,
        }
    }

    /// quiet 模式的基准 `StopOptions`（未显式指定端口）。
    fn quiet_opts() -> StopOptions {
        StopOptions {
            explicit_port: false,
            port: 0,
            host: None,
            json: false,
            quiet: true,
            suppress_quiet_output: false,
        }
    }

    /// human 模式的基准 `StopOptions`（未显式指定端口）。
    fn human_opts() -> StopOptions {
        StopOptions {
            explicit_port: false,
            port: 0,
            host: None,
            json: false,
            quiet: false,
            suppress_quiet_output: false,
        }
    }

    /// 绑定 127.0.0.1 的 0 端口获取一个当前空闲端口（随即关闭监听，
    /// 存在被后续测试抢占的微小竞态窗口）。
    fn grab_free_port() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    /// 构造写入注册表的测试用实例选项；started_at 取固定时间戳。
    fn test_instance_options(port: u16, launch_mode: &str) -> process::InstanceOptions {
        process::InstanceOptions {
            port,
            host: None,
            launch_mode: launch_mode.to_string(),
            ui_password: None,
            has_ui_password: false,
            api_only: false,
            started_at: 1_700_000_000_000.0,
        }
    }

    /// Spawn `/bin/sleep` and reap it on a side thread so liveness checks
    /// (`kill -0`) see its real exit instead of a zombie.
    /// 中文说明：返回 (pid, reap 线程句柄)；测试结束 join 句柄以等待
    /// 真实退出并回收线程。
    fn spawn_reaped_sleep(
        seconds: u64,
    ) -> (u32, std::thread::JoinHandle<std::process::ExitStatus>) {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg(seconds.to_string())
            .spawn()
            .unwrap();
        let pid = child.id();
        let handle = std::thread::spawn(move || child.wait().unwrap());
        (pid, handle)
    }

    /// Spawn a process whose `ps` command line looks like an ompchamber server
    /// (`exec -a` rewrites argv[0]) for cmdline-identity checks.
    /// 中文说明：依赖 bash 的 `exec -a` 把 argv[0] 改写为
    /// "ompchamber-server-serve-fake"；系统无 `/bin/bash` 时返回
    /// `None`，调用方测试据此跳过。
    fn spawn_fake_ompchamber_cmdline(
        seconds: u64,
    ) -> Option<(u32, std::thread::JoinHandle<std::process::ExitStatus>)> {
        if !std::path::Path::new("/bin/bash").exists() {
            return None;
        }
        let mut child = std::process::Command::new("/bin/bash")
            .arg("-c")
            .arg(format!(
                "exec -a ompchamber-server-serve-fake /bin/sleep {seconds}"
            ))
            .spawn()
            .ok()?;
        let pid = child.id();
        let handle = std::thread::spawn(move || child.wait().unwrap());
        Some((pid, handle))
    }

    /// How the fake API answers `POST /api/system/shutdown`.
    /// 中文说明：控制假 API 服务对 shutdown 请求的应答行为，覆盖
    /// 优雅释放、假确认（端口保持占用）与明确拒绝三条路径。
    #[derive(Clone)]
    enum FakeShutdown {
        /// 200 and gracefully release the port.
        /// 中文：应答 200 并触发优雅停机、释放端口。
        AckAndRelease,
        /// 200 but keep listening (port stays occupied).
        /// 中文：应答 200 但继续监听，端口保持占用（半停机场景）。
        AckKeepAlive,
        /// Non-2xx; keep listening.
        /// 中文：应答携带的 non-2xx 状态码并继续监听。
        Fail(u16),
    }

    /// A minimal fake `/api/system/info` + `/api/system/shutdown` server on
    /// 127.0.0.1; returns the bound port.
    /// 中文说明：在独立线程的单线程 tokio runtime 上运行 axum 服务：
    /// info 路由返回构造时给定的 JSON，shutdown 路由按 `FakeShutdown`
    /// 策略应答（AckAndRelease 经 mpsc 通道触发 graceful shutdown）；
    /// 返回绑定端口供测试注册与访问。
    fn spawn_fake_api(info: serde_json::Value, shutdown_mode: FakeShutdown) -> u16 {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);
                let app = axum::Router::new()
                    .route(
                        "/api/system/info",
                        axum::routing::get(move || {
                            let info = info.clone();
                            async move { ([("content-type", "application/json")], info.to_string()) }
                        }),
                    )
                    .route(
                        "/api/system/shutdown",
                        axum::routing::post(move || {
                            let shutdown_tx = shutdown_tx.clone();
                            let mode = shutdown_mode.clone();
                            async move {
                                match mode {
                                    FakeShutdown::AckAndRelease => {
                                        let _ = shutdown_tx.send(()).await;
                                        axum::http::StatusCode::OK
                                    }
                                    FakeShutdown::AckKeepAlive => axum::http::StatusCode::OK,
                                    FakeShutdown::Fail(status) => {
                                        axum::http::StatusCode::from_u16(status).unwrap()
                                    }
                                }
                            }
                        }),
                    );
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                    let _ = shutdown_rx.recv().await;
                });
                if let Err(error) = server.await {
                    panic!("fake api server failed: {error}");
                }
            });
        });
        port
    }

    // ── pure helper matrices ────────────────────────────────────────────────

    /// 验证 host/URL 归一化契约：通配绑定映射回环、方括号形式去括号、
    /// 空白输入回落默认，`OMPCHAMBER_HOST` 作为最后一级默认来源，
    /// buildLocalUrl 正确拼接 endpoint 与 IPv6 方括号。
    #[test]
    fn resolve_api_host_normalizes_wildcards_and_brackets() {
        let _env = EnvGuard::data_dir(&temp_data_dir("api-host"));
        assert_eq!(resolve_api_host(None), "127.0.0.1");
        assert_eq!(resolve_api_host(Some("  ")), "127.0.0.1");
        assert_eq!(resolve_api_host(Some("0.0.0.0")), "127.0.0.1");
        assert_eq!(resolve_api_host(Some("::")), "::1");
        assert_eq!(resolve_api_host(Some("[::]")), "::1");
        assert_eq!(resolve_api_host(Some("[::1]")), "::1");
        assert_eq!(resolve_api_host(Some("localhost")), "localhost");
        assert_eq!(resolve_api_host(Some(" 10.0.0.9 ")), "10.0.0.9");
        assert_eq!(
            build_local_url(3000, "/health", None),
            "http://127.0.0.1:3000/health"
        );
        assert_eq!(
            build_local_url(3000, "health", Some("::1")),
            "http://[::1]:3000/health"
        );
        // OMPCHAMBER_HOST feeds the default host last.
        unsafe { std::env::set_var("OMPCHAMBER_HOST", "10.1.2.3") };
        assert_eq!(resolve_api_host(None), "10.1.2.3");
        assert_eq!(resolve_api_host(Some("192.0.2.1")), "192.0.2.1");
        assert_eq!(
            build_local_url(3000, "/health", None),
            "http://10.1.2.3:3000/health"
        );
    }

    /// 验证探测候选构造规则：归一化 host 作为去重键、具体权威地址使
    /// 回环兜底候选强制 pid 匹配、通配与空白地址不贡献额外候选。
    #[test]
    fn probe_host_candidates_dedup_and_pid_match_rules() {
        let _env = EnvGuard::data_dir(&temp_data_dir("probe-hosts"));

        let hosts = get_system_info_probe_hosts(&[None, None]);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host, None);
        assert!(!hosts[0].requires_pid_match);

        // A concrete authoritative host makes the loopback fallbacks require a
        // pid match; the trailing 127.0.0.1 dedups against the None entry.
        let hosts = get_system_info_probe_hosts(&[Some("192.168.1.24".to_string()), None]);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].host.as_deref(), Some("192.168.1.24"));
        assert!(!hosts[0].requires_pid_match);
        assert_eq!(hosts[1].host, None);
        assert!(hosts[1].requires_pid_match);

        // A wildcard host is not concrete and collapses onto the loopback key.
        let hosts = get_system_info_probe_hosts(&[Some("0.0.0.0".to_string())]);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host.as_deref(), Some("0.0.0.0"));
        assert!(!hosts[0].requires_pid_match);

        // Empty/whitespace hosts contribute nothing.
        let hosts = get_system_info_probe_hosts(&[Some("   ".to_string())]);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].host, None);
    }

    /// 验证 quiet 模式结果输出契约：空结果输出 "none"、成功行走
    /// stdout（"stopped <port>"）、失败行走 stderr（"failed <port>
    /// <reason>"，reason 缺省 "failed"）；抑制输出及 json/human 模式
    /// 下不产生任何行。
    #[test]
    fn quiet_stop_results_print_none_stopped_and_failed_lines() {
        let results = vec![
            serde_json::json!({ "port": 3000, "stopped": true }),
            serde_json::json!({ "port": 3001, "stopped": false, "reason": "stop-failed" }),
            serde_json::json!({ "port": 3002, "stopped": false }),
        ];

        let mut sink = CaptureSink::default();
        print_quiet_stop_results(&quiet_opts(), &[], &mut sink);
        assert_eq!(sink.out, vec!["none".to_string()]);
        assert!(sink.err.is_empty());

        let mut sink = CaptureSink::default();
        print_quiet_stop_results(&quiet_opts(), &results, &mut sink);
        assert_eq!(sink.out, vec!["stopped 3000".to_string()]);
        assert_eq!(
            sink.err,
            vec![
                "failed 3001 stop-failed".to_string(),
                "failed 3002 failed".to_string()
            ]
        );

        // Suppressed, and never in json mode.
        let mut sink = CaptureSink::default();
        print_quiet_stop_results(
            &StopOptions {
                suppress_quiet_output: true,
                ..quiet_opts()
            },
            &results,
            &mut sink,
        );
        print_quiet_stop_results(&json_opts(), &results, &mut sink);
        print_quiet_stop_results(&human_opts(), &results, &mut sink);
        assert!(sink.out.is_empty() && sink.err.is_empty());
    }

    // ── stop: selection matrix on temp run dirs ─────────────────────────────

    /// 验证显式端口无实例时的 not-found 契约：json 报
    /// reason:"not-found"、quiet 输出对应失败行、human 打印标题与
    /// "nothing to stop" 收尾，且不产生多余输出。
    #[tokio::test]
    async fn stop_reports_not_found_when_no_instance_on_port() {
        let dir = temp_data_dir("not-found");
        let _env = EnvGuard::data_dir(&dir);
        let port = grab_free_port();

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..json_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "stoppedCount": 0,
                "results": [{ "port": port, "stopped": false, "reason": "not-found" }],
            })]
        );
        assert!(sink.out.is_empty() && sink.err.is_empty());

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..quiet_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert!(sink.json.is_empty() && sink.out.is_empty());
        assert_eq!(sink.err, vec![format!("failed {port} not-found")]);

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..human_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.out,
            vec![
                "OMPChamber Stop".to_string(),
                format!("no OMPChamber instance found on port {port}"),
                "nothing to stop".to_string(),
            ]
        );
    }

    /// 验证死 pid 注册条目的清理契约：全量 stop 先清掉注册文件并返回
    /// 空结果，随后显式端口报 not-found，空注册表的 quiet 模式输出
    /// "none"。
    #[tokio::test]
    async fn stop_cleans_dead_pid_registry_entries_and_reports_none() {
        let dir = temp_data_dir("dead-pid");
        let _env = EnvGuard::data_dir(&dir);

        // A pid file whose process is gone (spawned + reaped) must be pruned.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let pid = child.id();
        let port = 45991;
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "daemon"),
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(&json_opts(), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({ "stoppedCount": 0, "results": [] })]
        );
        assert!(!paths::pid_file_path(port).exists());
        assert!(!paths::instance_file_path(port).exists());

        // Explicit port against the now-pruned registry → not-found.
        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..quiet_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(sink.err, vec![format!("failed {port} not-found")]);

        // Empty registry in quiet mode prints "none".
        let mut sink = CaptureSink::default();
        stop_command_with_sink(&quiet_opts(), &mut sink)
            .await
            .unwrap();
        assert_eq!(sink.out, vec!["none".to_string()]);
    }

    /// 验证 pid 复用防护：注册 pid 存活但 cmdline 不匹配 OMPChamber
    /// 且端口无 HTTP 确认时，注册文件被移除、实例不计入结果。
    #[tokio::test]
    async fn stop_prunes_mismatched_pid_registry_entries() {
        let dir = temp_data_dir("mismatched-pid");
        let _env = EnvGuard::data_dir(&dir);

        // A live but non-ompchamber pid with no HTTP confirmation: the pid
        // recycled to a stranger, so the registry files are removed and the
        // instance is not reported.
        let (pid, _handle) = spawn_reaped_sleep(30);
        let port = grab_free_port();
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "daemon"),
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(&json_opts(), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({ "stoppedCount": 0, "results": [] })]
        );
        assert!(!paths::pid_file_path(port).exists());
        assert!(!paths::instance_file_path(port).exists());
    }

    /// 验证注册表确认实例的停机契约：shutdown 被拒（500）后立即按
    /// pid 终止，json/quiet/human 三种形态输出正确，且每次停机都
    /// 清理 pid 与 instance 注册文件。
    #[tokio::test]
    async fn stop_registered_instance_reports_success_and_cleans_files() {
        let dir = temp_data_dir("registry-stop");
        let _env = EnvGuard::data_dir(&dir);

        // Registry pid file + live HTTP confirmation; shutdown request fails
        // (500) so the stop terminates the pid immediately (no 5s shutdown
        // wait — keeps the test fast).
        let (pid, handle) = spawn_reaped_sleep(30);
        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "web", "pid": pid }),
            FakeShutdown::Fail(500),
        );
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "daemon"),
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(&json_opts(), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "status": "ok",
                "stoppedCount": 1,
                "results": [{ "port": port, "pid": pid, "stopped": true }],
            })]
        );
        assert!(!paths::pid_file_path(port).exists());
        assert!(!paths::instance_file_path(port).exists());
        handle.join().unwrap();

        // Fresh registration per output mode (each stop consumes it).
        let (pid, handle) = spawn_reaped_sleep(30);
        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "web", "pid": pid }),
            FakeShutdown::Fail(500),
        );
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "daemon"),
        );
        let mut sink = CaptureSink::default();
        stop_command_with_sink(&quiet_opts(), &mut sink)
            .await
            .unwrap();
        assert_eq!(sink.out, vec![format!("stopped {port}")]);
        assert!(!paths::pid_file_path(port).exists());
        handle.join().unwrap();

        let (pid, handle) = spawn_reaped_sleep(30);
        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "web", "pid": pid }),
            FakeShutdown::Fail(500),
        );
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "daemon"),
        );
        let mut sink = CaptureSink::default();
        stop_command_with_sink(&human_opts(), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            sink.out,
            vec![
                "OMPChamber Stop".to_string(),
                format!("stopping port {port} (PID: {pid})"),
                format!("stopped port {port}"),
                "1 instance(s)".to_string(),
            ]
        );
        handle.join().unwrap();
    }

    /// 验证本机 desktop 运行时端口受保护：三种输出形态均拒绝停机，
    /// json 附带 DESKTOP_MANAGED_PORT 消息、quiet 报
    /// "failed <port> desktop-managed"。
    #[tokio::test]
    async fn stop_desktop_managed_port_is_refused() {
        let dir = temp_data_dir("desktop-port");
        let _env = EnvGuard::data_dir(&dir);

        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "desktop", "pid": 4242 }),
            FakeShutdown::AckKeepAlive,
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..json_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "stoppedCount": 0,
                "results": [{
                    "port": port,
                    "runtime": "desktop",
                    "stopped": false,
                    "reason": "desktop-managed",
                }],
                "messages": [{
                    "level": "warning",
                    "code": "DESKTOP_MANAGED_PORT",
                    "message": format!("Port {port} is managed by OMPChamber Desktop and cannot be stopped with this command."),
                }],
            })]
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..quiet_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(sink.err, vec![format!("failed {port} desktop-managed")]);

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..human_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.out,
            vec![
                "OMPChamber Stop".to_string(),
                format!("port {port} is managed by OMPChamber Desktop"),
                "cannot be stopped with this command".to_string(),
                "no changes applied".to_string(),
            ]
        );
    }

    /// 验证外部 desktop 实例识别：settings.json 的 desktopLocalPort
    /// 指向其它端口时，desktop 运行时应答的端口不属于本应用 →
    /// 按 not-found 处理。
    #[tokio::test]
    async fn stop_ignores_foreign_desktop_runtime_port() {
        let dir = temp_data_dir("foreign-desktop");
        let _env = EnvGuard::data_dir(&dir);

        // settings.json pins the desktop app to another port, so a desktop
        // runtime answering this port is not ours → not-found.
        std::fs::write(
            paths::settings_file_path(),
            serde_json::json!({ "desktopLocalPort": 49999 }).to_string(),
        )
        .unwrap();

        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "desktop", "pid": 4242 }),
            FakeShutdown::AckKeepAlive,
        );
        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..json_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "stoppedCount": 0,
                "results": [{ "port": port, "stopped": false, "reason": "not-found" }],
            })]
        );
    }

    /// 验证无注册实例的半停机路径：shutdown 请求被 200 应答但端口仍
    /// 被占用时，json 报 SHUTDOWN_PARTIAL 警告、reason 为
    /// "shutdown-requested-port-busy"，human 以 "partial stop" 收尾。
    #[tokio::test]
    async fn stop_unmanaged_instance_partial_when_port_stays_busy() {
        let dir = temp_data_dir("unmanaged-partial");
        let _env = EnvGuard::data_dir(&dir);

        // No registry files; the port answers /api/system/info. The shutdown
        // request is acknowledged but the fake keeps listening.
        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "web" }),
            FakeShutdown::AckKeepAlive,
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..json_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "status": "warning",
                "stoppedCount": 0,
                "results": [{
                    "port": port,
                    "runtime": "unmanaged",
                    "stopped": false,
                    "reason": "shutdown-requested-port-busy",
                }],
                "messages": [{
                    "level": "warning",
                    "code": "SHUTDOWN_PARTIAL",
                    "message": format!("Shutdown was requested for port {port}, but the port is still occupied."),
                }],
            })]
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..quiet_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.err,
            vec![format!("failed {port} shutdown-requested-port-busy")]
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..human_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.out,
            vec![
                "OMPChamber Stop".to_string(),
                format!("found unmanaged OMPChamber instance on port {port}"),
                "attempting shutdown".to_string(),
                format!("shutdown requested on port {port}"),
                "port is still occupied".to_string(),
                "partial stop".to_string(),
            ]
        );
    }

    /// 验证无注册实例停机失败路径：shutdown 请求被 500 拒绝且端口未
    /// 释放 → json 报 STOP_FAILED 错误、reason "stop-failed"。
    #[tokio::test]
    async fn stop_unmanaged_instance_fails_when_shutdown_refused() {
        let dir = temp_data_dir("unmanaged-fail");
        let _env = EnvGuard::data_dir(&dir);

        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "web" }),
            FakeShutdown::Fail(500),
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..json_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "status": "error",
                "stoppedCount": 0,
                "results": [{
                    "port": port,
                    "runtime": "unmanaged",
                    "stopped": false,
                    "reason": "stop-failed",
                }],
                "messages": [{
                    "level": "error",
                    "code": "STOP_FAILED",
                    "message": format!("Could not stop OMPChamber on port {port}."),
                }],
            })]
        );
    }

    /// 验证无注册实例的成功路径：shutdown 应答 200 并优雅释放端口、
    /// 伴随 pid 自行退出 → stoppedCount 1、runtime "unmanaged"。
    #[tokio::test]
    async fn stop_unmanaged_instance_succeeds_when_port_released() {
        let dir = temp_data_dir("unmanaged-ok");
        let _env = EnvGuard::data_dir(&dir);

        // A short-lived pid claimed by the info endpoint gives the port a
        // moment to be released before the availability check.
        let (pid, handle) = spawn_reaped_sleep(1);
        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "web", "pid": pid }),
            FakeShutdown::AckAndRelease,
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..json_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "stoppedCount": 1,
                "results": [{ "port": port, "runtime": "unmanaged", "stopped": true }],
            })]
        );
        handle.join().unwrap();
    }

    /// 验证未确认注册实例按 pid 强停的契约：端口被无 HTTP 面的监听
    /// 占用、pid 的 cmdline 匹配时，终止 pid、清理注册文件，json 与
    /// human 输出 runtime "unconfirmed" 的完整结果。
    #[tokio::test]
    async fn stop_unconfirmed_registry_instance_by_pid() {
        let dir = temp_data_dir("unconfirmed");
        let _env = EnvGuard::data_dir(&dir);

        let Some((pid, handle)) = spawn_fake_ompchamber_cmdline(30) else {
            eprintln!("skipping: /bin/bash unavailable for argv[0] spoofing");
            return;
        };

        // The port is occupied by a non-OMPChamber listener (no HTTP surface),
        // the pid file points at a cmdline-matched process.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "daemon"),
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..json_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "stoppedCount": 1,
                "results": [{
                    "port": port,
                    "pid": pid,
                    "runtime": "unconfirmed",
                    "stopped": true,
                }],
            })]
        );
        assert!(!paths::pid_file_path(port).exists());
        assert!(!paths::instance_file_path(port).exists());
        assert!(!process::is_process_running(pid));
        handle.join().unwrap();

        // Fresh registration for the human-mode shape.
        let Some((pid, handle)) = spawn_fake_ompchamber_cmdline(30) else {
            eprintln!("skipping: /bin/bash unavailable for argv[0] spoofing");
            return;
        };
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "daemon"),
        );
        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..human_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.out,
            vec![
                "OMPChamber Stop".to_string(),
                format!("found unconfirmed OMPChamber pid {pid} on port {port}"),
                "HTTP shutdown endpoint is unreachable; stopping by PID".to_string(),
                format!("stopped pid {pid}"),
                "stop complete".to_string(),
            ]
        );
        assert!(!process::is_process_running(pid));
        handle.join().unwrap();
        drop(listener);
    }

    /// 验证未确认分支遇到死 pid 的回落：注册文件被清理、按
    /// not-found 报告（而不是误报 unconfirmed）。
    #[tokio::test]
    async fn stop_unconfirmed_dead_pid_falls_back_to_not_found() {
        let dir = temp_data_dir("unconfirmed-dead");
        let _env = EnvGuard::data_dir(&dir);

        // A dead registry pid on an occupied port: unconfirmed discovery prunes
        // the files and returns none → not-found.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        process::write_pid_file(&paths::pid_file_path(port), child.id());
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "daemon"),
        );

        let mut sink = CaptureSink::default();
        stop_command_with_sink(
            &StopOptions {
                explicit_port: true,
                port,
                ..json_opts()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "stoppedCount": 0,
                "results": [{ "port": port, "stopped": false, "reason": "not-found" }],
            })]
        );
        assert!(!paths::pid_file_path(port).exists());
        assert!(!paths::instance_file_path(port).exists());
    }

    // ── restart ─────────────────────────────────────────────────────────────

    /// 验证空注册表时 restart 的三种输出形态：json 输出
    /// restartedCount 0 空结果、quiet 输出 "restarted 0"、human 打印
    /// 标题与 "nothing to restart"。
    #[tokio::test]
    async fn restart_reports_no_instances() {
        let dir = temp_data_dir("restart-empty");
        let _env = EnvGuard::data_dir(&dir);

        let mut sink = CaptureSink::default();
        restart_command_with_sink(
            &Options {
                json: true,
                ..Options::default()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({ "restartedCount": 0, "results": [] })]
        );

        let mut sink = CaptureSink::default();
        restart_command_with_sink(
            &Options {
                quiet: true,
                ..Options::default()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(sink.out, vec!["restarted 0".to_string()]);

        let mut sink = CaptureSink::default();
        restart_command_with_sink(&Options::default(), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            sink.out,
            vec![
                "OMPChamber Restart".to_string(),
                "No running OMPChamber instances to restart".to_string(),
                "nothing to restart".to_string(),
            ]
        );
    }

    /// 验证 desktop 管理端口拒绝 restart：json 报 desktop-managed
    /// 警告（restartedCount 0），quiet 输出 "restarted 0"。
    #[tokio::test]
    async fn restart_desktop_managed_port_is_refused() {
        let dir = temp_data_dir("restart-desktop");
        let _env = EnvGuard::data_dir(&dir);

        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "desktop", "pid": 4242 }),
            FakeShutdown::AckKeepAlive,
        );

        let mut sink = CaptureSink::default();
        restart_command_with_sink(
            &Options {
                json: true,
                explicit_port: true,
                port: Some(port),
                ..Options::default()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "status": "warning",
                "restartedCount": 0,
                "results": [{
                    "fromPort": port,
                    "runtime": "desktop",
                    "ok": false,
                    "reason": "desktop-managed",
                }],
                "messages": [{
                    "level": "warning",
                    "code": "DESKTOP_MANAGED_PORT",
                    "message": format!("Port {port} is managed by OMPChamber Desktop and cannot be restarted with this command."),
                }],
            })]
        );

        let mut sink = CaptureSink::default();
        restart_command_with_sink(
            &Options {
                quiet: true,
                explicit_port: true,
                port: Some(port),
                ..Options::default()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(sink.out, vec!["restarted 0".to_string()]);
    }

    /// 验证 foreground 实例的 restart 契约：只调用静默 stop、不再调用
    /// serve（重启交给 systemd 等进程管理器），记录 ok:true 且 human
    /// 输出 "process manager will restart"。
    #[tokio::test]
    async fn restart_foreground_instance_stops_and_defers_to_process_manager() {
        let dir = temp_data_dir("restart-fg");
        let _env = EnvGuard::data_dir(&dir);

        // Foreground launch mode: stop, record ok:true, and do NOT call serve.
        let (pid, handle) = spawn_reaped_sleep(30);
        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "web", "pid": pid }),
            FakeShutdown::Fail(500),
        );
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "foreground"),
        );

        let mut sink = CaptureSink::default();
        restart_command_with_sink(
            &Options {
                json: true,
                ..Options::default()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.json,
            vec![serde_json::json!({
                "restartedCount": 1,
                "results": [{ "fromPort": port, "toPort": port, "launchMode": "foreground", "ok": true }],
            })]
        );
        assert!(!paths::pid_file_path(port).exists());
        assert!(!paths::instance_file_path(port).exists());
        handle.join().unwrap();

        // Fresh foreground registration for the quiet shape.
        let (pid, handle) = spawn_reaped_sleep(30);
        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "web", "pid": pid }),
            FakeShutdown::Fail(500),
        );
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "foreground"),
        );
        let mut sink = CaptureSink::default();
        restart_command_with_sink(
            &Options {
                quiet: true,
                ..Options::default()
            },
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(sink.out, vec!["restarted 1".to_string()]);
        handle.join().unwrap();

        // Fresh foreground registration for the human shape.
        let (pid, handle) = spawn_reaped_sleep(30);
        let port = spawn_fake_api(
            serde_json::json!({ "runtime": "web", "pid": pid }),
            FakeShutdown::Fail(500),
        );
        process::write_pid_file(&paths::pid_file_path(port), pid);
        process::write_instance_options(
            &paths::instance_file_path(port),
            &test_instance_options(port, "foreground"),
        );
        let mut sink = CaptureSink::default();
        restart_command_with_sink(&Options::default(), &mut sink)
            .await
            .unwrap();
        assert_eq!(
            sink.out,
            vec![
                "OMPChamber Restart".to_string(),
                format!("restarting port {port}"),
                "mode: foreground".to_string(),
                format!("port {port} stopped"),
                "process manager will restart".to_string(),
                "1 instance(s) restarted".to_string(),
            ]
        );
        handle.join().unwrap();
    }

    // ── process termination ─────────────────────────────────────────────────

    /// 验证 stop_instance_process 的宽限→终止语义：短 shutdown 宽限
    /// 内进程未自退时被终止，最终确认不再存活。
    #[tokio::test]
    async fn stop_instance_process_waits_then_terminates() {
        let (pid, handle) = spawn_reaped_sleep(30);
        assert!(stop_instance_process(pid, 50, 150, 400).await);
        handle.join().unwrap();
        assert!(!process::is_process_running(pid));
    }

    /// 验证 TERM 被忽略进程的强杀升级：`trap '' TERM` 的 disposition
    /// 经 exec 存续，graceful 超时后 force（KILL）兜底生效。
    #[tokio::test]
    async fn stop_instance_process_force_kills_term_ignoring_process() {
        // `trap '' TERM` + exec: the ignored disposition survives exec, so only
        // the KILL escalation reaps it.
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("trap '' TERM; exec /bin/sleep 30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let handle = std::thread::spawn(move || child.wait().unwrap());
        assert!(stop_instance_process(pid, 40, 120, 400).await);
        handle.join().unwrap();
        assert!(!process::is_process_running(pid));
    }

    /// 验证 wait_for_process_exit 的边界：pid 0 视为已退出、零超时仍
    /// 执行一次存活检查、有限等待超时返回 false 且不影响被测进程。
    #[tokio::test]
    async fn wait_for_process_exit_bounds_and_zero_timeout() {
        assert!(wait_for_process_exit(0, 0).await);
        let (pid, handle) = spawn_reaped_sleep(30);
        // A zero timeout still performs one liveness check.
        assert!(!wait_for_process_exit(pid, 0).await);
        // A bounded wait elapses while the process lives.
        assert!(!wait_for_process_exit(pid, 50).await);
        assert!(process::is_process_running(pid));
        handle.join().unwrap();
    }
}
