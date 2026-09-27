//! Port of `server/lib/cloudflare-tunnel.js` plus
//! `server/lib/tunnels/providers/cloudflare.js` (the provider adapter).
//!
//! YAML note: cloudflared configs are parsed with a minimal YAML subset
//! (top-level keys, `ingress:` lists of flat maps, quoted/unquoted scalars,
//! `#` comments). `.json` configs parse with serde_json. Full YAML is not
//! available in the allowed crate set; the subset covers the ingress
//! hostname extraction this module needs.
//!
//! 中文说明：Cloudflare 隧道适配层。前半部分直接驱动 `cloudflared`
//! 子进程：三种启动模式（quick 临时隧道 / managed-remote token 隧道 /
//! managed-local 配置文件隧道），各自等待启动日志就绪并产出可停止的
//! `TunnelController`。后半部分（provider adapter）把这些能力包装成
//! `TunnelProvider` trait 实现，向注册表暴露能力描述、可用性检查与
//! 按模式的诊断 JSON。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::executable_search::{EnvMap, create_executable_search_env, real_env, real_home};
use super::install_help::get_tunnel_dependency_install_info;
use super::registry::{
    AvailabilityInfo, DiagnoseRequest, StartContext, StartFailure, TunnelController, TunnelProvider,
};
use super::runner::{ChildChunk, ChildStream, CommandRunner, Spawned, spawn_output_readers};
use super::types::{
    ModeDescriptor, Platform, TUNNEL_INTENT_EPHEMERAL_PUBLIC, TUNNEL_INTENT_PERSISTENT_PUBLIC,
    TUNNEL_MODE_MANAGED_LOCAL, TUNNEL_MODE_MANAGED_REMOTE, TUNNEL_MODE_QUICK,
    TUNNEL_PROVIDER_CLOUDFLARE, TunnelServiceError, TunnelStartRequest,
};

/// quick 隧道等待 trycloudflare URL 出现的默认超时（30 秒）。
const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 30_000;
/// managed 隧道等待就绪日志的硬超时（20 秒）。
const MANAGED_TUNNEL_STARTUP_TIMEOUT_MS: u64 = 20_000;
/// managed 隧道的活性兜底窗口（6 秒）：窗口内只要子进程有输出即视为就绪。
const MANAGED_TUNNEL_LIVENESS_FALLBACK_MS: u64 = 6_000;

/// managed-local 配置文件的大小上限（256 KiB），避免读取超大文件。
const MANAGED_LOCAL_CONFIG_MAX_BYTES: u64 = 256 * 1024;
/// managed-local 配置允许的扩展名白名单（YAML 子集 + JSON）。
const MANAGED_LOCAL_CONFIG_ALLOWED_EXTENSIONS: [&str; 3] = [".yml", ".yaml", ".json"];

/// 三种启动模式的超时参数集合（毫秒），测试可注入更小的值。
#[derive(Debug, Clone)]
pub struct CloudflareTunnelOpts {
    /// quick 隧道等待 trycloudflare URL 的超时。
    pub quick_timeout_ms: u64,
    /// managed 隧道启动的硬超时，超时即 kill 子进程。
    pub managed_startup_timeout_ms: u64,
    /// managed 隧道的活性兜底窗口：有输出但未见到就绪日志时按此时限放行。
    pub liveness_fallback_ms: u64,
}

/// 默认值取上面的毫秒常量（对齐 JS host 的 30s/20s/6s）。
impl Default for CloudflareTunnelOpts {
    /// 以模块级默认超时构造。
    fn default() -> Self {
        Self {
            quick_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            managed_startup_timeout_ms: MANAGED_TUNNEL_STARTUP_TIMEOUT_MS,
            liveness_fallback_ms: MANAGED_TUNNEL_LIVENESS_FALLBACK_MS,
        }
    }
}

/// Cloudflare API 探测结果：可达性与失败详情。
#[derive(Debug, Clone, Default)]
pub struct Reachability {
    /// 请求是否成功发出并收到任意 HTTP 应答。
    pub reachable: bool,
    /// 收到的 HTTP 状态码（请求未成功时为 `None`）。
    pub status: Option<u16>,
    /// 发送/传输层错误描述（成功时为 `None`）。
    pub error: Option<String>,
}

/// API 可达性探测注入点：返回 future 的闭包，供测试离线替换，
/// 生产路径使用真实 HTTP 探测。
pub type ApiProbe = Arc<dyn Fn() -> BoxFuture<'static, Reachability> + Send + Sync>;

/// 小写化后的子串包含判定。
fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(needle)
}

/// `a[^\n]*b` (case-insensitive): `a` appears before `b` on the line.
fn followed_by_ci(line: &str, a: &str, b: &str) -> bool {
    let lower = line.to_lowercase();
    match lower.find(a) {
        Some(pos) => lower[pos + a.len()..].contains(b),
        None => false,
    }
}

/// 就绪日志判定：连接注册完成、metrics server 启动或已连上 edge，
/// 任一命中即视为隧道就绪。
pub fn is_cloudflared_ready_log_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    contains_ci(line, "registered tunnel connection")
        || followed_by_ci(line, "connection", "registered")
        || contains_ci(line, "starting metrics server")
        || contains_ci(line, "connected to edge")
}

/// 致命日志判定：配置解析/加载失败、token 无效或未授权、凭证文件
/// 缺失或凭证无效，命中即终止启动等待。
pub fn is_cloudflared_fatal_log_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    followed_by_ci(line, "error parsing", "config")
        || followed_by_ci(line, "failed to", "config")
        || contains_ci(line, "invalid token")
        || contains_ci(line, "unauthorized")
        || followed_by_ci(line, "credentials file", "not found")
        || contains_ci(line, "provided tunnel credentials are invalid")
}

/// `/https:\/\/[a-z0-9-]+\.trycloudflare\.com/i` — the URL is matched per
/// output chunk (the JS regex is tested per chunk, never across chunks).
pub fn extract_try_cloudflare_url(text: &str) -> Option<String> {
    let prefix = "https://";
    let mut search_from = 0usize;
    while let Some(pos) = lower_find(text, prefix, search_from) {
        let after = &text[pos + prefix.len()..];
        let subdomain_len = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .map(|c| c.len_utf8())
            .sum::<usize>();
        if subdomain_len == 0 {
            search_from = pos + prefix.len();
            continue;
        }
        let rest = &after[subdomain_len..];
        if let Some(after_dot) = rest.strip_prefix('.')
            && starts_with_ci(after_dot, "trycloudflare.com")
        {
            let end = pos + prefix.len() + subdomain_len + 1 + "trycloudflare.com".len();
            return Some(text[pos..end].to_string());
        }
        search_from = pos + prefix.len() + subdomain_len;
    }
    None
}

/// 自 `from` 起做 ASCII 大小写不敏感的子串查找，返回字节偏移；
/// needle 必须是纯 ASCII（小写形式）。
fn lower_find(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    let mut pos = from;
    while pos + needle.len() <= hay.len() {
        if hay[pos..pos + needle.len()]
            .iter()
            .zip(needle)
            .all(|(h, n)| h.to_ascii_lowercase() == *n)
        {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

/// ASCII 大小写不敏感的前缀判定。
fn starts_with_ci(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len()
        && haystack[..prefix.len()]
            .bytes()
            .zip(prefix.bytes())
            .all(|(h, p)| h.eq_ignore_ascii_case(&p))
}

/// `normalizeHostname(value)`.
pub fn normalize_cloudflare_tunnel_hostname(value: Option<&str>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = if trimmed.contains("://") {
        url::Url::parse(trimmed)
    } else {
        url::Url::parse(&format!("https://{trimmed}"))
    };
    let hostname = parsed
        .ok()
        .and_then(|url| url.host_str().map(|host| host.trim().to_lowercase()))?;
    if hostname.is_empty() || hostname.contains('*') {
        return None;
    }
    Some(hostname)
}

/// 打印 cloudflared 安装指引横幅（按平台列出安装命令）。
fn print_cloudflare_tunnel_install_help() {
    println!(
        "
╔══════════════════════════════════════════════════════════════════╗
║  Cloudflare tunnel requires 'cloudflared' to be installed        ║
╚══════════════════════════════════════════════════════════════════╝

Install instructions for your platform:

  macOS:    brew install cloudflared
  Windows:  winget install --id Cloudflare.cloudflared
  Linux:    Download from https://github.com/cloudflare/cloudflared/releases

Or visit: https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/
"
    );
}

/// `printTunnelWarning()` (quick-tunnel limitations banner).
pub fn print_tunnel_warning() {
    println!(
        "
⚠️  Quick Tunnel Limitations:

   • Provider limits may apply
   • URLs are temporary and will expire when the tunnel stops
   • Password protection is required for tunnel access

   For production use, set up a persistent provider tunnel or static domain.
"
    );
}

/// 组装 cloudflared 子进程环境：可执行搜索环境 + 关闭 CF 遥测
/// （`CF_TELEMETRY_DISABLE=1`）+ 调用方覆盖项。
fn cloudflared_env(env_overrides: &[(&str, String)]) -> EnvMap {
    let mut env = create_executable_search_env(&real_env(), Platform::current(), &real_home());
    env.insert("CF_TELEMETRY_DISABLE".to_string(), "1".to_string());
    for (key, value) in env_overrides {
        env.insert(key.to_string(), value.clone());
    }
    env
}

/// cloudflared 二进制的原始可用性探测结果。
#[derive(Debug, Clone, Default)]
pub struct RawAvailability {
    /// 解析到可执行文件且 `--version` 探测退出码为 0。
    pub available: bool,
    /// 解析到的可执行文件路径（后续 spawn 直接使用）。
    pub path: Option<String>,
    /// `--version` 的 stdout 原文（已 trim）。
    pub version: Option<String>,
}

/// `checkCloudflaredAvailable()`.
pub fn check_cloudflared_available_raw(runner: &dyn CommandRunner) -> RawAvailability {
    let target = runner.resolve("cloudflared");
    if let Some(target) = target
        && let Some(result) = runner.probe(&target.command, &["--version"], &target.env)
        && result.status == Some(0)
    {
        return RawAvailability {
            available: true,
            path: Some(target.command),
            version: Some(result.stdout.trim().to_string()),
        };
    }
    RawAvailability {
        available: false,
        path: None,
        version: None,
    }
}

/// `checkCloudflareApiReachability()`.
pub async fn check_cloudflare_api_reachability(
    http: &reqwest::Client,
    timeout_ms: u64,
) -> Reachability {
    let request = http
        .get("https://api.trycloudflare.com/")
        .timeout(Duration::from_millis(timeout_ms));
    match request.send().await {
        Ok(response) => Reachability {
            reachable: true,
            status: Some(response.status().as_u16()),
            error: None,
        },
        Err(error) => Reachability {
            reachable: false,
            status: None,
            error: Some(error.to_string()),
        },
    }
}

/// 校验 managed-local 配置文件可读且合规：存在、是文件、扩展名在
/// 白名单内、非空且不超过大小上限；错误文案面向用户。
fn assert_readable_file(file_path: &Path, context_label: &str) -> Result<(), String> {
    let stats = std::fs::metadata(file_path).map_err(|_| {
        format!("{context_label} file was not found. Select a valid cloudflared config file.")
    })?;

    if !stats.is_file() {
        return Err(format!(
            "{context_label} path is not a file. Select a cloudflared config file."
        ));
    }

    let extension = file_path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    if !MANAGED_LOCAL_CONFIG_ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
        return Err(format!(
            "{context_label} must be a .yml, .yaml, or .json file."
        ));
    }

    let size = stats.len();
    if size == 0 {
        return Err(format!("{context_label} file is empty."));
    }
    if size > MANAGED_LOCAL_CONFIG_MAX_BYTES {
        return Err(format!(
            "{context_label} file is too large (max {MANAGED_LOCAL_CONFIG_MAX_BYTES} bytes)."
        ));
    }

    if std::fs::File::open(file_path).is_err() {
        return Err(format!(
            "{context_label} file is not readable. Check file permissions and try again."
        ));
    }
    Ok(())
}

/// 去掉行内 ` #` 注释；带引号的标量原样保留。
fn strip_yaml_comment(value: &str) -> &str {
    if value.starts_with('"') || value.starts_with('\'') {
        return value;
    }
    match value.find(" #") {
        Some(pos) => value[..pos].trim(),
        None => value.trim(),
    }
}

/// 解析 YAML 标量：trim 后剥掉成对的单/双引号。
fn yaml_scalar(value: &str) -> String {
    let value = strip_yaml_comment(value.trim());
    value
        .trim_start_matches('"')
        .trim_end_matches('"')
        .trim_start_matches('\'')
        .trim_end_matches('\'')
        .to_string()
}

/// 从 `key: value` 形态的行提取非空标量值。
fn yaml_key_value(text: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    let rest = text.strip_prefix(&prefix)?;
    let value = yaml_scalar(rest);
    if value.is_empty() { None } else { Some(value) }
}

/// Minimal YAML subset: hostnames from an `ingress:` list, in order.
fn yaml_ingress_hostnames(raw: &str) -> Vec<String> {
    let mut hostnames = Vec::new();
    let mut in_ingress = false;
    let mut ingress_indent = 0usize;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();

        if !in_ingress {
            if trimmed == "ingress:" {
                in_ingress = true;
                ingress_indent = indent;
            }
            continue;
        }

        if indent <= ingress_indent && !trimmed.starts_with("- ") {
            in_ingress = false;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("- ") {
            if let Some(hostname) = yaml_key_value(rest.trim(), "hostname") {
                hostnames.push(hostname);
            }
            continue;
        }

        if let Some(hostname) = yaml_key_value(trimmed, "hostname") {
            hostnames.push(hostname);
        }
    }
    hostnames
}

/// 读取 cloudflared 配置并按扩展名分流解析（`.json` 走 serde_json，
/// 其余走 YAML 子集），返回首个能通过 hostname 归一化的 ingress
/// hostname 与可选错误：读不到文件或 JSON 非法时给出面向用户的错误；
/// 解析成功但无可用 hostname 时两者均为 `None`。
fn extract_hostname_from_cloudflared_config_detailed(
    config_path: &Path,
) -> (Option<String>, Option<String>) {
    let raw = match std::fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(_) => {
            return (
                None,
                Some("Could not read the managed local tunnel config file. Check that the file exists and is accessible.".to_string()),
            );
        }
    };

    let extension = config_path
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let ingress_hostnames: Vec<String> = if extension == "json" {
        match serde_json::from_str::<Value>(&raw) {
            Ok(parsed) => parsed
                .get("ingress")
                .and_then(Value::as_array)
                .map(|rules| {
                    rules
                        .iter()
                        .filter_map(|rule| rule.get("hostname").and_then(|value| value.as_str()))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            Err(_) => {
                return (
                    None,
                    Some("Managed local tunnel config is invalid. Use a valid cloudflared YAML/JSON config file.".to_string()),
                );
            }
        }
    } else {
        yaml_ingress_hostnames(&raw)
    };

    for hostname in ingress_hostnames {
        if let Some(hostname) = normalize_cloudflare_tunnel_hostname(Some(&hostname)) {
            return (Some(hostname), None);
        }
    }
    (None, None)
}

/// 默认配置路径：`$HOME/.cloudflared/config.yml`。
fn get_default_cloudflared_config_path() -> PathBuf {
    Path::new(&real_home())
        .join(".cloudflared")
        .join("config.yml")
}

/// managed-local 配置检查结果（诊断与启动前检查共用）。
#[derive(Debug, Clone)]
pub struct ManagedLocalInspection {
    /// 配置可读、解析成功且 hostname 可解析。
    pub ok: bool,
    /// 实际生效的配置文件路径（未指定时为默认路径）。
    pub effective_config_path: String,
    /// 优先取显式 hostname，回退到配置内的 ingress hostname。
    pub resolved_hostname: Option<String>,
    /// 失败原因（面向用户的文案）；成功时为 `None`。
    pub error: Option<String>,
}

/// `inspectManagedLocalCloudflareConfig({ configPath, hostname })`.
pub fn inspect_managed_local_cloudflare_config(
    config_path: Option<&str>,
    hostname: Option<&str>,
) -> ManagedLocalInspection {
    let requested_path = config_path.map(str::trim).unwrap_or_default();
    let effective_config_path = if requested_path.is_empty() {
        get_default_cloudflared_config_path()
    } else {
        PathBuf::from(requested_path)
    };

    let label = if requested_path.is_empty() {
        "Managed local tunnel default config"
    } else {
        "Managed local tunnel config"
    };
    if let Err(error) = assert_readable_file(&effective_config_path, label) {
        return ManagedLocalInspection {
            ok: false,
            effective_config_path: effective_config_path.to_string_lossy().into_owned(),
            resolved_hostname: None,
            error: Some(error),
        };
    }

    let (config_hostname, parse_error) =
        extract_hostname_from_cloudflared_config_detailed(&effective_config_path);
    if let Some(error) = parse_error {
        return ManagedLocalInspection {
            ok: false,
            effective_config_path: effective_config_path.to_string_lossy().into_owned(),
            resolved_hostname: None,
            error: Some(error),
        };
    }

    let resolved_hostname = normalize_cloudflare_tunnel_hostname(hostname).or(config_hostname);
    let Some(resolved_hostname) = resolved_hostname else {
        return ManagedLocalInspection {
            ok: false,
            effective_config_path: effective_config_path.to_string_lossy().into_owned(),
            resolved_hostname: None,
            error: Some("Managed local tunnel hostname is required (set --hostname or include ingress hostname in config).".to_string()),
        };
    };

    ManagedLocalInspection {
        ok: true,
        effective_config_path: effective_config_path.to_string_lossy().into_owned(),
        resolved_hostname: Some(resolved_hostname),
        error: None,
    }
}

/// A spawned child whose exit also runs a cleanup (temp dir removal).
struct WatchedChild {
    /// 子进程 stdout/stderr 的输出 chunk 流（`spawn_output_readers` 建立）。
    chunks: mpsc::Receiver<ChildChunk>,
    /// 子进程退出码的一次性接收端（守护任务完成清理后才触发）。
    exit: tokio::sync::oneshot::Receiver<Option<i32>>,
    /// 终止子进程的回调（与 runner 共享同一 Arc 句柄）。
    kill: Arc<dyn Fn() + Send + Sync>,
}

/// 包裹 Spawned：启动输出读取器，并 spawn 一个守护任务——子进程退出后
/// 先删除临时目录，再把退出码转交 `exit` 通道。
fn watch_child(mut spawned: Spawned, cleanup_dir: Option<PathBuf>) -> WatchedChild {
    let chunks = spawn_output_readers(&mut spawned);
    let kill = spawned.kill.clone();
    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    let exit = spawned.exit;
    tokio::spawn(async move {
        let code = exit.await.unwrap_or(None);
        if let Some(dir) = &cleanup_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        let _ = exit_tx.send(code);
    });
    WatchedChild {
        chunks,
        exit: exit_rx,
        kill,
    }
}

/// 删除临时目录（best-effort，失败忽略）。
fn cleanup_temp_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// 写入 token 文件：确保父目录存在后以私有权限落盘（不落 shell 历史）。
async fn write_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    super::managed_config::write_private_file_for_tests(path, token).await
}

/// `startCloudflareQuickTunnel({ originUrl })`.
pub async fn start_cloudflare_quick_tunnel(
    origin_url: &str,
    runner: &dyn CommandRunner,
    opts: &CloudflareTunnelOpts,
) -> Result<TunnelController, String> {
    let cf_check = check_cloudflared_available_raw(runner);
    if !cf_check.available {
        print_cloudflare_tunnel_install_help();
        return Err("cloudflared is not installed".to_string());
    }

    println!(
        "Using cloudflared: {} ({})",
        cf_check.path.clone().unwrap_or_default(),
        cf_check.version.clone().unwrap_or_default()
    );

    let temp_dir = super::managed_config::make_temp_dir("ompchamber-cf-")
        .map_err(|error| error.to_string())?;

    let env = cloudflared_env(&[("HOME", temp_dir.to_string_lossy().into_owned())]);
    let args = vec![
        "tunnel".to_string(),
        "--url".to_string(),
        origin_url.to_string(),
    ];
    let command = cf_check.path.unwrap_or_else(|| "cloudflared".to_string());
    let spawned = runner
        .spawn(&command, &args, &env)
        .map_err(|error| format!("Cloudflared error: {error}"))?;
    let mut child = watch_child(spawned, Some(temp_dir.clone()));

    let mut public_url: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(opts.quick_timeout_ms);

    let outcome: Result<String, String> = loop {
        tokio::select! {
            chunk = child.chunks.recv() => {
                if let Some(chunk) = chunk {
                    if chunk.stream == ChildStream::Stderr {
                        eprint!("{}", chunk.text);
                    }
                    if public_url.is_none() {
                        public_url = extract_try_cloudflare_url(&chunk.text);
                    }
                    if let Some(url) = public_url.clone() {
                        break Ok(url);
                    }
                } else {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            }
            exit = &mut child.exit => {
                let code = exit.unwrap_or(None);
                cleanup_temp_dir(&temp_dir);
                break Err(format!(
                    "Cloudflared exited with code {}",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string())
                ));
            }
            _ = tokio::time::sleep_until(deadline) => {
                (child.kill)();
                cleanup_temp_dir(&temp_dir);
                break Err("Tunnel URL not received within 30 seconds".to_string());
            }
        }
    };

    let public_url = outcome?;
    let kill = child.kill;
    Ok(TunnelController {
        provider: None,
        mode: TUNNEL_MODE_QUICK.to_string(),
        public_url: Some(public_url),
        stop: Some(kill),
        effective_config_path: None,
        resolved_hostname: None,
    })
}

/// 等待 managed 隧道就绪：优先等就绪日志行；命中致命日志或子进程退出
/// 立即报错；超过 `liveness_fallback_ms` 时只要期间有任意输出即放行
/// （部分版本不打印标准就绪行）；`managed_startup_timeout_ms` 为硬超时。
async fn wait_for_managed_tunnel_ready(
    child: &mut WatchedChild,
    mode_label: &str,
    opts: &CloudflareTunnelOpts,
) -> Result<(), String> {
    let mut saw_output = false;
    let fallback_sleep = tokio::time::sleep_until(
        tokio::time::Instant::now() + Duration::from_millis(opts.liveness_fallback_ms),
    );
    let hard_sleep = tokio::time::sleep_until(
        tokio::time::Instant::now() + Duration::from_millis(opts.managed_startup_timeout_ms),
    );
    tokio::pin!(fallback_sleep);
    tokio::pin!(hard_sleep);
    let mut fallback_fired = false;

    loop {
        tokio::select! {
            chunk = child.chunks.recv() => {
                let Some(chunk) = chunk else {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    continue;
                };
                if !chunk.text.trim().is_empty() {
                    saw_output = true;
                }
                if chunk.stream == ChildStream::Stderr {
                    eprint!("{}", chunk.text);
                }
                for line in chunk.text.split('\n') {
                    let line = line.trim_end_matches('\r').trim();
                    if line.is_empty() {
                        continue;
                    }
                    if is_cloudflared_ready_log_line(line) {
                        return Ok(());
                    }
                    if is_cloudflared_fatal_log_line(line) {
                        return Err(format!("Cloudflared failed to start {mode_label}: {line}"));
                    }
                }
            }
            exit = &mut child.exit => {
                let code = exit.unwrap_or(None);
                return Err(format!(
                    "Cloudflared exited while starting {mode_label} (code {})",
                    code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string())
                ));
            }
            _ = &mut fallback_sleep, if !fallback_fired => {
                fallback_fired = true;
                if saw_output {
                    return Ok(());
                }
            }
            _ = &mut hard_sleep => {
                return Err(format!(
                    "Timed out waiting for cloudflared to initialize {mode_label}. Check your tunnel config and credentials."
                ));
            }
        }
    }
}

/// `startCloudflareManagedRemoteTunnel({ token, hostname, tokenFilePath })`.
pub async fn start_cloudflare_managed_remote_tunnel(
    token: Option<&str>,
    hostname: Option<&str>,
    token_file_path: Option<&str>,
    runner: &dyn CommandRunner,
    opts: &CloudflareTunnelOpts,
) -> Result<TunnelController, String> {
    let cf_check = check_cloudflared_available_raw(runner);
    if !cf_check.available {
        print_cloudflare_tunnel_install_help();
        return Err("cloudflared is not installed".to_string());
    }

    let normalized_token = token.map(str::trim).unwrap_or_default();
    let normalized_host = hostname.map(str::trim).unwrap_or_default().to_lowercase();

    if normalized_token.is_empty() {
        return Err("Managed remote tunnel token is required".to_string());
    }
    if normalized_host.is_empty() {
        return Err("Managed remote tunnel hostname is required".to_string());
    }

    let mut temp_token_dir: Option<PathBuf> = None;
    let effective_token_file = match token_file_path {
        Some(path) => PathBuf::from(path),
        None => {
            let dir = super::managed_config::make_temp_dir("ompchamber-cf-token-")
                .map_err(|error| error.to_string())?;
            let path = dir.join("token");
            write_token_file(&path, normalized_token)
                .await
                .map_err(|error| error.to_string())?;
            temp_token_dir = Some(dir);
            path
        }
    };

    let env = cloudflared_env(&[]);
    let args = vec![
        "tunnel".to_string(),
        "run".to_string(),
        "--token-file".to_string(),
        effective_token_file.to_string_lossy().into_owned(),
    ];
    let command = cf_check.path.unwrap_or_else(|| "cloudflared".to_string());
    let spawned = runner
        .spawn(&command, &args, &env)
        .map_err(|error| format!("Cloudflared error: {error}"))?;
    let mut child = watch_child(spawned, temp_token_dir.clone());
    let public_url = format!("https://{normalized_host}");

    if let Err(error) =
        wait_for_managed_tunnel_ready(&mut child, "managed-remote tunnel", opts).await
    {
        (child.kill)();
        if let Some(dir) = &temp_token_dir {
            cleanup_temp_dir(dir);
        }
        return Err(error);
    }

    let kill = child.kill;
    let cleanup = temp_token_dir;
    let stop = Arc::new(move || {
        kill();
        if let Some(dir) = &cleanup {
            cleanup_temp_dir(dir);
        }
    });
    Ok(TunnelController {
        provider: None,
        mode: TUNNEL_MODE_MANAGED_REMOTE.to_string(),
        public_url: Some(public_url),
        stop: Some(stop),
        effective_config_path: None,
        resolved_hostname: None,
    })
}

/// `startCloudflareManagedLocalTunnel({ configPath, hostname })`.
pub async fn start_cloudflare_managed_local_tunnel(
    config_path: Option<&str>,
    hostname: Option<&str>,
    runner: &dyn CommandRunner,
    opts: &CloudflareTunnelOpts,
) -> Result<TunnelController, String> {
    let cf_check = check_cloudflared_available_raw(runner);
    if !cf_check.available {
        print_cloudflare_tunnel_install_help();
        return Err("cloudflared is not installed".to_string());
    }

    let requested_path = config_path.map(str::trim).unwrap_or_default();
    let effective_config_path = if requested_path.is_empty() {
        get_default_cloudflared_config_path()
    } else {
        PathBuf::from(requested_path)
    };

    let label = if requested_path.is_empty() {
        "Managed local tunnel default config"
    } else {
        "Managed local tunnel config"
    };
    assert_readable_file(&effective_config_path, label)?;

    let (config_hostname, parse_error) =
        extract_hostname_from_cloudflared_config_detailed(&effective_config_path);
    if let Some(error) = parse_error {
        return Err(error);
    }

    let resolved_host = normalize_cloudflare_tunnel_hostname(hostname).or(config_hostname);

    let resolved_host = resolved_host.ok_or(
        "Managed local tunnel hostname is required (use --tunnel-hostname or add an ingress hostname to the cloudflared config)",
    )?;

    let mut args = vec!["tunnel".to_string()];
    if !requested_path.is_empty() {
        args.push("--config".to_string());
        args.push(effective_config_path.to_string_lossy().into_owned());
    }
    args.push("run".to_string());

    let env = cloudflared_env(&[]);
    let command = cf_check.path.unwrap_or_else(|| "cloudflared".to_string());
    let spawned = runner
        .spawn(&command, &args, &env)
        .map_err(|error| format!("Cloudflared error: {error}"))?;
    let mut child = watch_child(spawned, None);
    let public_url = format!("https://{resolved_host}");

    if let Err(error) =
        wait_for_managed_tunnel_ready(&mut child, "managed-local tunnel", opts).await
    {
        (child.kill)();
        return Err(error);
    }

    let kill = child.kill;
    Ok(TunnelController {
        provider: None,
        mode: TUNNEL_MODE_MANAGED_LOCAL.to_string(),
        public_url: Some(public_url),
        stop: Some(kill),
        effective_config_path: Some(effective_config_path.to_string_lossy().into_owned()),
        resolved_hostname: Some(resolved_host),
    })
}

// ---------------------------------------------------------------------------
// Provider adapter (providers/cloudflare.js)
// ---------------------------------------------------------------------------

/// 三种模式的能力描述（quick / managed-remote / managed-local），
/// 含各自的必填项与支持选项，供注册表与 UI 呈现。
pub static CLOUDFLARE_MODES: [ModeDescriptor; 3] = [
    ModeDescriptor {
        key: TUNNEL_MODE_QUICK,
        label: "Quick Tunnel",
        intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC,
        requires: &[],
        supports: &["sessionTTL"],
        stability: "ga",
    },
    ModeDescriptor {
        key: TUNNEL_MODE_MANAGED_REMOTE,
        label: "Managed Remote Tunnel",
        intent: TUNNEL_INTENT_PERSISTENT_PUBLIC,
        requires: &["token", "hostname"],
        supports: &["customDomain", "sessionTTL"],
        stability: "ga",
    },
    ModeDescriptor {
        key: TUNNEL_MODE_MANAGED_LOCAL,
        label: "Managed Local Tunnel",
        intent: TUNNEL_INTENT_PERSISTENT_PUBLIC,
        requires: &[],
        supports: &["configFile", "customDomain", "sessionTTL"],
        stability: "ga",
    },
];

/// 组装 provider 能力 JSON（默认模式 + 模式描述列表），供诊断与 UI。
pub fn cloudflare_capabilities_json() -> Value {
    json!({
        "provider": TUNNEL_PROVIDER_CLOUDFLARE,
        "defaults": { "mode": TUNNEL_MODE_QUICK, "optionDefaults": {} },
        "modes": CLOUDFLARE_MODES.iter().map(|mode| mode.to_json()).collect::<Vec<_>>(),
    })
}

/// Cloudflare provider 适配器：持有命令 runner、HTTP 客户端与超时选项，
/// 实现注册表的 `TunnelProvider` 契约。
pub struct CloudflareTunnelProvider {
    /// 命令解析/派生/探测/终止的执行后端（测试可注入 fake runner）。
    runner: Arc<dyn CommandRunner>,
    /// API 可达性探测使用的 reqwest 客户端。
    http: reqwest::Client,
    /// 测试注入的离线探测钩子；`None` 时走真实 HTTP 探测。
    api_probe: Option<ApiProbe>,
    /// 三种启动模式的超时参数。
    opts: CloudflareTunnelOpts,
}

/// 构造与可达性探测来源的选择。
impl CloudflareTunnelProvider {
    /// 以默认超时构造 provider。
    pub fn new(runner: Arc<dyn CommandRunner>, http: reqwest::Client) -> Self {
        Self {
            runner,
            http,
            api_probe: None,
            opts: CloudflareTunnelOpts::default(),
        }
    }

    /// Test seam (offline API reachability).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_api_probe(mut self, probe: ApiProbe) -> Self {
        self.api_probe = Some(probe);
        self
    }

    /// 选择可达性探测来源：优先测试注入的 probe，否则用真实 HTTP
    /// 探测 api.trycloudflare.com（5 秒超时）。
    fn reachability(&self) -> BoxFuture<'static, Reachability> {
        if let Some(probe) = &self.api_probe {
            return probe();
        }
        let http = self.http.clone();
        Box::pin(async move { check_cloudflare_api_reachability(&http, 5_000).await })
    }
}

/// managed-remote token 形状校验：非空且不含任何空白字符；
/// 返回 (是否有效, 面向用户的说明文案)。
fn validate_token_shape(value: Option<&str>) -> (bool, String) {
    let Some(value) = value else {
        return (false, "Managed remote token is missing.".to_string());
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return (false, "Managed remote token is missing.".to_string());
    }
    if trimmed.chars().any(char::is_whitespace) {
        return (
            false,
            "Managed remote token has whitespace; provide the raw token value.".to_string(),
        );
    }
    (true, "Managed remote token looks valid.".to_string())
}

/// 汇总一组检查项：fail 计数为 0 即 ready，同时统计 warning 数量。
fn create_mode_summary(checks: &[Value]) -> Value {
    let failures = checks
        .iter()
        .filter(|entry| entry["status"] == "fail")
        .count();
    let warnings = checks
        .iter()
        .filter(|entry| entry["status"] == "warn")
        .count();
    json!({
        "ready": failures == 0,
        "failures": failures,
        "warnings": warnings,
    })
}

/// 组装单个模式的诊断对象：检查列表、summary，以及除
/// `startup_readiness` 外的失败项作为 blockers（优先取 detail 文案）。
fn describe_mode(mode: &str, checks: Vec<Value>) -> Value {
    let summary = create_mode_summary(&checks);
    let blockers: Vec<Value> = checks
        .iter()
        .filter(|entry| entry["status"] == "fail" && entry["id"] != "startup_readiness")
        .map(|entry| {
            let detail = entry["detail"].as_str().unwrap_or_default();
            if !detail.is_empty() {
                Value::from(detail)
            } else {
                entry["label"].clone()
            }
        })
        .collect();
    json!({
        "mode": mode,
        "checks": checks,
        "summary": summary.clone(),
        "ready": summary["ready"].clone(),
        "blockers": blockers,
    })
}

/// 构造一条诊断检查项 JSON（id/label/status/detail 四字段）。
fn check_entry(id: &str, label: &str, status: &str, detail: impl Into<String>) -> Value {
    json!({ "id": id, "label": label, "status": status, "detail": detail.into() })
}

/// 注册表契约的 Cloudflare 实现：能力、可用性、按模式诊断与三种启动。
impl TunnelProvider for CloudflareTunnelProvider {
    /// provider 固定标识 `cloudflare`。
    fn id(&self) -> &'static str {
        TUNNEL_PROVIDER_CLOUDFLARE
    }

    /// 能力 JSON（委托 `cloudflare_capabilities_json`）。
    fn capabilities_json(&self) -> Value {
        cloudflare_capabilities_json()
    }

    /// 静态模式描述表（`CLOUDFLARE_MODES`）。
    fn mode_descriptors(&self) -> &'static [ModeDescriptor] {
        &CLOUDFLARE_MODES
    }

    /// 探测 cloudflared 可用性，并附上平台化的安装指引信息。
    fn check_availability(&self) -> BoxFuture<'static, AvailabilityInfo> {
        let runner = self.runner.clone();
        Box::pin(async move {
            let raw = check_cloudflared_available_raw(runner.as_ref());
            let install = get_tunnel_dependency_install_info(
                TUNNEL_PROVIDER_CLOUDFLARE,
                Platform::current().js_name(),
            );
            AvailabilityInfo {
                available: raw.available,
                version: raw.version,
                dependency: install.dependency,
                install_command: install.install_command,
                install_url: install.install_url,
                platform: install.platform,
                message: install.message,
            }
        })
    }

    /// 按模式诊断：依赖 + 网络作为 provider 级检查；quick /
    /// managed-local / managed-remote 各自组装检查列表，其中
    /// managed-remote 在未显式提供输入时允许回退到已保存 profile；
    /// 可用 `mode` 参数过滤只输出单个模式。
    fn diagnose(&self, request: DiagnoseRequest) -> BoxFuture<'static, Value> {
        let runner = self.runner.clone();
        let reachability = self.reachability();
        Box::pin(async move {
            let dependency = check_cloudflared_available_raw(runner.as_ref());
            let network = reachability.await;
            let install = get_tunnel_dependency_install_info(
                TUNNEL_PROVIDER_CLOUDFLARE,
                Platform::current().js_name(),
            );

            let provider_checks = vec![
                check_entry(
                    "dependency",
                    "cloudflared installed",
                    if dependency.available { "pass" } else { "fail" },
                    if dependency.available {
                        dependency
                            .version
                            .clone()
                            .filter(|value| !value.is_empty())
                            .or_else(|| dependency.path.clone())
                            .unwrap_or_else(|| "cloudflared available".to_string())
                    } else {
                        install.message.clone()
                    },
                ),
                check_entry(
                    "network",
                    "Cloudflare API reachable",
                    if network.reachable { "pass" } else { "fail" },
                    if network.reachable {
                        network
                            .status
                            .map(|status| format!("HTTP {status}"))
                            .unwrap_or_else(|| "Reachable".to_string())
                    } else {
                        network
                            .error
                            .clone()
                            .unwrap_or_else(|| "Could not reach api.trycloudflare.com".to_string())
                    },
                ),
            ];

            let startup_ready = dependency.available && network.reachable;
            let startup_detail = if startup_ready {
                "Provider dependency and network checks passed."
            } else {
                "Resolve provider checks before starting tunnels."
            };

            let quick_checks = vec![
                check_entry(
                    "startup_readiness",
                    "Provider startup readiness",
                    if startup_ready { "pass" } else { "fail" },
                    startup_detail,
                ),
                check_entry(
                    "quick_mode_prerequisites",
                    "Quick tunnel prerequisites",
                    if network.reachable { "pass" } else { "fail" },
                    if network.reachable {
                        "Cloudflare edge is reachable for quick tunnels."
                    } else {
                        "Cloudflare edge is not reachable for quick tunnels."
                    },
                ),
            ];

            let managed_local_inspection = inspect_managed_local_cloudflare_config(
                request.config_path.as_deref(),
                request.hostname.as_deref(),
            );
            let managed_local_checks = vec![
                check_entry(
                    "startup_readiness",
                    "Provider startup readiness",
                    if startup_ready { "pass" } else { "fail" },
                    startup_detail,
                ),
                check_entry(
                    "managed_local_config",
                    "Managed local config",
                    if managed_local_inspection.ok {
                        "pass"
                    } else {
                        "fail"
                    },
                    if managed_local_inspection.ok {
                        let hostname_suffix = managed_local_inspection
                            .resolved_hostname
                            .as_ref()
                            .map(|hostname| format!(" ({hostname})"))
                            .unwrap_or_default();
                        format!(
                            "{}{}",
                            managed_local_inspection.effective_config_path, hostname_suffix
                        )
                    } else {
                        managed_local_inspection.error.clone().unwrap_or_default()
                    },
                ),
            ];

            let normalized_host = normalize_cloudflare_tunnel_hostname(request.hostname.as_deref());
            let hostname_missing = normalized_host.is_none();
            let remote_token_validation = validate_token_shape(request.token.as_deref());
            let token_missing = request
                .token
                .as_deref()
                .map(|token| token.trim().is_empty())
                .unwrap_or(true);
            let has_saved_managed_remote_profile = request.has_saved_managed_remote_profile;
            let token_provided = request.token_provided;
            let hostname_provided = request.hostname_provided;
            let has_explicit_managed_remote_input = token_provided || hostname_provided;
            let can_use_saved_profile_for_hostname = !has_explicit_managed_remote_input
                && hostname_missing
                && has_saved_managed_remote_profile;
            let can_use_saved_profile_for_token = !has_explicit_managed_remote_input
                && token_missing
                && has_saved_managed_remote_profile;
            let saved_profile_ready_detail = "at least one saved profile present";
            let managed_remote_checks = vec![
                check_entry(
                    "startup_readiness",
                    "Provider startup readiness",
                    if startup_ready { "pass" } else { "fail" },
                    startup_detail,
                ),
                check_entry(
                    "managed_remote_hostname",
                    "Managed remote hostname",
                    if normalized_host.is_some() || can_use_saved_profile_for_hostname {
                        "pass"
                    } else {
                        "fail"
                    },
                    if let Some(host) = &normalized_host {
                        host.clone()
                    } else if can_use_saved_profile_for_hostname {
                        saved_profile_ready_detail.to_string()
                    } else {
                        "Managed remote hostname is required (use --hostname).".to_string()
                    },
                ),
                check_entry(
                    "managed_remote_token",
                    "Managed remote token",
                    if remote_token_validation.0 || can_use_saved_profile_for_token {
                        "pass"
                    } else {
                        "fail"
                    },
                    if can_use_saved_profile_for_token {
                        saved_profile_ready_detail.to_string()
                    } else {
                        remote_token_validation.1
                    },
                ),
            ];

            let all_modes = vec![
                describe_mode(TUNNEL_MODE_QUICK, quick_checks),
                describe_mode(TUNNEL_MODE_MANAGED_REMOTE, managed_remote_checks),
                describe_mode(TUNNEL_MODE_MANAGED_LOCAL, managed_local_checks),
            ];

            let mode_filter = request
                .mode
                .as_deref()
                .map(str::trim)
                .filter(|mode| !mode.is_empty())
                .map(str::to_lowercase);
            let modes = match mode_filter {
                Some(filter) => all_modes
                    .into_iter()
                    .filter(|entry| entry["mode"].as_str() == Some(filter.as_str()))
                    .collect(),
                None => all_modes,
            };

            json!({ "providerChecks": provider_checks, "modes": modes })
        })
    }

    /// 按请求模式分流启动：managed-remote / managed-local 走各自启动器，
    /// 默认 quick 模式（需要 `originUrl`，缺失时返回 validation error）。
    fn start(
        &self,
        request: TunnelStartRequest,
        context: StartContext,
    ) -> BoxFuture<'static, Result<TunnelController, StartFailure>> {
        let runner = self.runner.clone();
        let opts = self.opts.clone();
        Box::pin(async move {
            if request.mode == TUNNEL_MODE_MANAGED_REMOTE {
                return start_cloudflare_managed_remote_tunnel(
                    Some(&request.token),
                    Some(&request.hostname),
                    None,
                    runner.as_ref(),
                    &opts,
                )
                .await
                .map_err(StartFailure::Raw);
            }

            if request.mode == TUNNEL_MODE_MANAGED_LOCAL {
                return start_cloudflare_managed_local_tunnel(
                    request.config_path.as_deref(),
                    Some(&request.hostname),
                    runner.as_ref(),
                    &opts,
                )
                .await
                .map_err(StartFailure::Raw);
            }

            let Some(origin_url) = context.origin_url else {
                return Err(StartFailure::Service(TunnelServiceError::validation_error(
                    "originUrl is required for quick tunnel mode",
                )));
            };

            start_cloudflare_quick_tunnel(&origin_url, runner.as_ref(), &opts)
                .await
                .map_err(StartFailure::Raw)
        })
    }

    /// 停止一个运行中的控制器（执行其 stop 回调并清理临时目录）。
    fn stop(&self, controller: &TunnelController) {
        controller.stop();
    }

    /// 提取控制器的配置路径与已解析 hostname；无控制器时两字段为
    /// null（对齐 JS 可选链语义）。
    fn get_metadata(&self, controller: Option<&TunnelController>) -> Value {
        match controller {
            Some(controller) => json!({
                "configPath": controller.effective_config_path,
                "resolvedHostname": controller.resolved_hostname,
            }),
            // JS `controller?.getEffectiveConfigPath?.() ?? null` with no
            // controller yields nulls, not an error.
            None => json!({ "configPath": null, "resolvedHostname": null }),
        }
    }
}

/// 集成测试位于同目录 `cloudflare_tests.rs`（经 `#[path]` 挂载）。
#[cfg(test)]
#[path = "cloudflare_tests.rs"]
mod cloudflare_tests;
