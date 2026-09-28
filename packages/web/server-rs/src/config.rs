//! CLI/env option parsing, mirroring `server/lib/opencode/cli-options.js`
//! (`parseServeCliOptions`) plus the engine env resolution from
//! `env-config.js` (`resolveOpenCodeEnvConfig`).
//!
//! 中文说明：本模块负责 CLI/环境变量选项解析，对应 JS 版的
//! `server/lib/opencode/cli-options.js`（`parseServeCliOptions`）与
//! `env-config.js`（`resolveOpenCodeEnvConfig`）的引擎环境解析部分。

use std::path::PathBuf;

/// serve 子命令的完整配置：端口、绑定地址、UI 密码、目录、tunnel 与引擎选择。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Web server port (JS: `options.port`, default 3000).
    /// 中文说明：Web 服务器端口（JS：`options.port`，默认 3000）。
    pub port: u16,
    /// Explicit bind host from `--host` (JS: `options.host`).
    /// 中文说明：`--host` 显式指定的绑定主机（JS：`options.host`）。
    pub host: Option<String>,
    /// `--lan` maps to binding `0.0.0.0`.
    /// 中文说明：`--lan` 等价于绑定 `0.0.0.0`（对所有接口开放）。
    pub lan: bool,
    /// `--ui-password` / OMPCHAMBER_UI_PASSWORD / OPENCODE_UI_PASSWORD.
    /// 中文说明：UI 密码，来自 `--ui-password` / OMPCHAMBER_UI_PASSWORD / OPENCODE_UI_PASSWORD。
    pub ui_password: Option<String>,
    /// `--api-only` / OMPCHAMBER_API_ONLY: no browser UI assets.
    /// 中文说明：`--api-only` / OMPCHAMBER_API_ONLY：不提供浏览器 UI 资源。
    pub api_only: bool,
    /// OMPCHAMBER_DATA_DIR or `~/.config/ompchamber`.
    /// 中文说明：数据目录，OMPCHAMBER_DATA_DIR 或 `~/.config/ompchamber`。
    pub data_dir: PathBuf,
    /// OMPCHAMBER_DIST_DIR or `<web package>/dist`.
    /// 中文说明：UI 构建产物目录，OMPCHAMBER_DIST_DIR 或 `<web package>/dist`。
    pub dist_dir: PathBuf,
    /// Tunnel options are parsed for parity; wiring lands with the tunnels port.
    /// 中文说明：tunnel 选项为对齐 JS 而解析；实际接线随 tunnels 移植落地。
    pub tunnel: TunnelOptions,
    /// Managed/external engine selection.
    /// 中文说明：引擎选择：受管拉起子进程或连接外部服务器。
    pub engine: EngineConfig,
}

/// tunnel 相关选项（为与 JS CLI 对齐而解析；实际接线在 tunnels 模块移植时落地）。
#[derive(Debug, Clone, Default)]
pub struct TunnelOptions {
    /// 是否尝试自动建立 Cloudflare tunnel。
    pub try_cf_tunnel: bool,
    /// tunnel 提供方（如 cloudflare / ngrok）。
    pub provider: Option<String>,
    /// tunnel 模式。
    pub mode: Option<String>,
    /// tunnel 配置文件路径。
    pub config_path: Option<String>,
    /// tunnel 认证 token。
    pub token: Option<String>,
    /// tunnel 对外主机名。
    pub hostname: Option<String>,
}

/// 引擎运行模式：受管拉起 omp-host 子进程，或连接外部 OpenCode 兼容服务器。
#[derive(Debug, Clone)]
pub enum EngineConfig {
    /// Spawn the omp host child (Bun + server/lib/omp-host/host.ts).
    /// 中文说明：拉起 omp host 子进程（Bun + server/lib/omp-host/host.ts），
    /// hostname 为其监听地址。
    Managed { hostname: String },
    /// Connect to an external OpenCode-compatible server; never spawn.
    /// 中文说明：连接外部 OpenCode 兼容服务器（OPENCODE_HOST 等指定），绝不 spawn。
    External { base_url: String },
}

/// `ServerConfig` 的派生辅助方法。
impl ServerConfig {
    /// Effective bind host: `--host` > `--lan` (0.0.0.0) > OMPCHAMBER_HOST > loopback.
    /// 中文说明：生效的绑定主机，优先级 `--host` > `--lan`(0.0.0.0) >
    /// OMPCHAMBER_HOST > 127.0.0.1。
    pub fn bind_host(&self) -> String {
        if let Some(host) = self.host.as_deref() {
            return host.to_string();
        }
        if self.lan {
            return "0.0.0.0".to_string();
        }
        std::env::var("OMPCHAMBER_HOST")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string())
    }
}

/// 环境布尔开关是否启用：仅 "1" 或 "true"（不区分大小写由 matches! 处理 ASCII）视为真。
fn env_flag_enabled(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("1" | "true"))
}

/// 读取 UI 密码环境变量：OMPCHAMBER_UI_PASSWORD 优先，其次 OPENCODE_UI_PASSWORD；
/// 空值视为未设置。
fn env_password() -> Option<String> {
    std::env::var("OMPCHAMBER_UI_PASSWORD")
        .or_else(|_| std::env::var("OPENCODE_UI_PASSWORD"))
        .ok()
        .filter(|v| !v.is_empty())
}

/// 解析默认数据目录：OMPCHAMBER_DATA_DIR > `~/.config/ompchamber` > 兜底 `.ompchamber-data`。
fn default_data_dir() -> PathBuf {
    std::env::var("OMPCHAMBER_DATA_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config").join("ompchamber")))
        .unwrap_or_else(|| PathBuf::from(".ompchamber-data"))
}

/// 用户主目录：Windows 上优先 USERPROFILE（对齐 Node 的 os.homedir()），
/// 其余平台用 HOME；未设置时返回 None。
pub fn home_dir() -> Option<PathBuf> {
    // Mirror Node's os.homedir(): USERPROFILE on Windows, HOME elsewhere.
    // HOME alone made /api/fs/home (and the default data dir) fail on
    // Windows, where HOME is typically unset.
    let raw = if cfg!(windows) {
        std::env::var("USERPROFILE")
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| std::env::var("HOME").ok().filter(|v| !v.is_empty()))
    } else {
        std::env::var("HOME").ok().filter(|v| !v.is_empty())
    };
    raw.map(PathBuf::from)
}

/// 解析默认 UI 产物目录：OMPCHAMBER_DIST_DIR 优先，否则取 crate 目录旁的
/// `../dist`（即 packages/web/dist）。
fn default_dist_dir() -> PathBuf {
    // Crate lives at packages/web/server-rs; the web UI build output is
    // packages/web/dist (JS: path.join(__dirname, '..', 'dist')).
    std::env::var("OMPCHAMBER_DIST_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(|v| PathBuf::from(v.trim()))
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("dist")
        })
}

/// 读取 OMPCHAMBER_PORT 环境变量并解析为 u16；缺失或非法时返回 None。
fn env_port() -> Option<u16> {
    std::env::var("OMPCHAMBER_PORT")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
}

/// 解析引擎模式：OPENCODE_HOST 指向外部服务器；OPENCODE_PORT + skip-start
/// 组合也指向外部；否则受管模式，hostname 取 OMPCHAMBER_OPENCODE_HOSTNAME
///（非法值回退 127.0.0.1）。
fn resolve_engine_config() -> EngineConfig {
    let skip_start = env_flag_enabled("OPENCODE_SKIP_START")
        || env_flag_enabled("OMPCHAMBER_SKIP_OPENCODE_START");
    if let Ok(host) = std::env::var("OPENCODE_HOST") {
        let host = host.trim().trim_end_matches('/').to_string();
        if !host.is_empty() {
            return EngineConfig::External { base_url: host };
        }
    }
    if let Ok(port) = std::env::var("OPENCODE_PORT")
        && let Ok(port) = port.trim().parse::<u16>()
        && skip_start
    {
        return EngineConfig::External {
            base_url: format!("http://127.0.0.1:{port}"),
        };
    }
    if skip_start {
        // Skip-start without a reachable target: stay managed; the lifecycle
        // surfaces the missing target honestly rather than fabricating one.
    }
    let hostname = match std::env::var("OMPCHAMBER_OPENCODE_HOSTNAME") {
        Ok(value) if !value.trim().is_empty() => {
            let host = value.trim().to_string();
            match host.parse::<std::net::IpAddr>() {
                Ok(_) => host,
                // README: invalid values are rejected with an error and fall
                // back to loopback.
                Err(_) => {
                    tracing::warn!(
                        "Invalid OMPCHAMBER_OPENCODE_HOSTNAME {host:?}; falling back to 127.0.0.1"
                    );
                    "127.0.0.1".to_string()
                }
            }
        }
        _ => "127.0.0.1".to_string(),
    };
    EngineConfig::Managed { hostname }
}

/// Parse serve options from `args` (without the program name) and the process
/// environment. Mirrors `parseServeCliOptions` semantics: `--opt value` or
/// `--opt=value`; values starting with `--` are not consumed; an invalid port
/// falls back to the default.
/// 中文说明：环境变量先提供各选项初值，命令行参数再覆盖；
/// `--opt value` 与 `--opt=value` 均支持，形如 `--` 开头的值不消费，
/// 非法端口回退默认值。
pub fn parse_server_config(args: &[String], default_port: u16) -> ServerConfig {
    let mut port = env_port().unwrap_or(default_port);
    let mut host: Option<String> = None;
    let mut ui_password: Option<String> = env_password();
    let mut api_only = env_flag_enabled("OMPCHAMBER_API_ONLY");
    let mut lan = false;
    let mut tunnel = TunnelOptions {
        try_cf_tunnel: env_flag_enabled("OMPCHAMBER_TRY_CF_TUNNEL"),
        provider: std::env::var("OMPCHAMBER_TUNNEL_PROVIDER")
            .ok()
            .filter(|v| !v.is_empty()),
        mode: std::env::var("OMPCHAMBER_TUNNEL_MODE")
            .ok()
            .filter(|v| !v.is_empty()),
        config_path: std::env::var("OMPCHAMBER_TUNNEL_CONFIG")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        token: std::env::var("OMPCHAMBER_TUNNEL_TOKEN")
            .ok()
            .filter(|v| !v.is_empty()),
        hostname: std::env::var("OMPCHAMBER_TUNNEL_HOSTNAME")
            .ok()
            .filter(|v| !v.is_empty()),
    };

    // 取值辅助：优先 `=` 内联值，否则取下一个不以 `--` 开头的参数；
    // 返回 (值, 下一个待处理下标)。
    let consume_value =
        |i: usize, inline: Option<&str>, args: &[String]| -> (Option<String>, usize) {
            if let Some(value) = inline {
                return (Some(value.to_string()), i);
            }
            if let Some(next) = args.get(i + 1)
                && !next.starts_with("--")
            {
                return (Some(next.clone()), i + 1);
            }
            (None, i)
        };

    // 主扫描循环：跳过非 `--` 前缀参数，按选项名分发取值。
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        let Some(rest) = arg.strip_prefix("--") else {
            continue;
        };
        let (name, inline) = match rest.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (rest, None),
        };

        match name {
            "port" | "p" => {
                let (value, next) = consume_value(i - 1, inline, args);
                i = next + 1;
                if let Some(v) = value {
                    port = v.trim().parse::<u16>().unwrap_or(default_port);
                }
            }
            "host" => {
                let (value, next) = consume_value(i - 1, inline, args);
                i = next + 1;
                let trimmed = value.unwrap_or_default().trim().to_string();
                host = (!trimmed.is_empty()).then_some(trimmed);
            }
            "lan" => lan = true,
            "ui-password" => {
                let (value, next) = consume_value(i - 1, inline, args);
                i = next + 1;
                ui_password = value.filter(|v| !v.is_empty());
            }
            "api-only" => api_only = true,
            "try-cf-tunnel" => tunnel.try_cf_tunnel = true,
            "tunnel-provider" => {
                let (value, next) = consume_value(i - 1, inline, args);
                i = next + 1;
                if let Some(v) = value {
                    tunnel.provider = Some(v);
                }
            }
            "tunnel-mode" => {
                let (value, next) = consume_value(i - 1, inline, args);
                i = next + 1;
                if let Some(v) = value {
                    tunnel.mode = Some(v);
                }
            }
            "tunnel-config" => {
                let (value, next) = consume_value(i - 1, inline, args);
                i = next + 1;
                if let Some(v) = value {
                    tunnel.config_path = Some(v);
                }
            }
            "tunnel-token" => {
                let (value, next) = consume_value(i - 1, inline, args);
                i = next + 1;
                if let Some(v) = value {
                    tunnel.token = Some(v);
                }
            }
            "tunnel-hostname" => {
                let (value, next) = consume_value(i - 1, inline, args);
                i = next + 1;
                if let Some(v) = value {
                    tunnel.hostname = Some(v);
                }
            }
            "tunnel" => {
                let (value, next) = consume_value(i - 1, inline, args);
                i = next + 1;
                tunnel.config_path = value.or(tunnel.config_path.take().filter(|_| false));
            }
            _ => {}
        }
    }

    ServerConfig {
        port,
        host,
        lan,
        ui_password,
        api_only,
        data_dir: default_data_dir(),
        dist_dir: default_dist_dir(),
        tunnel,
        engine: resolve_engine_config(),
    }
}

/// 配置解析的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 把字符串切片转成 `Vec<String>` 便于传给 `parse_server_config`。
    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// 验证：`--opt=value` 内联形式与 `--opt value` 分离形式都能解析。
    #[test]
    fn parses_inline_and_separate_values() {
        let cfg = parse_server_config(&args(&["--port=8080", "--host", "0.0.0.0"]), 3000);
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.host.as_deref(), Some("0.0.0.0"));
    }

    /// 验证：非法端口值回退到默认端口。
    #[test]
    fn invalid_port_falls_back_to_default() {
        let cfg = parse_server_config(&args(&["--port", "nope"]), 3000);
        assert_eq!(cfg.port, 3000);
    }

    /// 验证：值位置上的 `--flag` 不被消费为选项值。
    #[test]
    fn does_not_consume_flag_like_values() {
        let cfg = parse_server_config(&args(&["--ui-password", "--api-only"]), 3000);
        assert_eq!(cfg.ui_password, None);
        assert!(cfg.api_only);
    }
    /// 验证：`--lan` 在未指定 host 时绑定所有接口（0.0.0.0）。
    #[test]
    fn lan_binds_all_interfaces_when_no_host() {
        let cfg = parse_server_config(&args(&["--lan"]), 3000);
        assert_eq!(cfg.bind_host(), "0.0.0.0");
    }

    /// 验证：显式 `--host` 优先级高于 `--lan`。
    #[test]
    fn host_overrides_lan() {
        let cfg = parse_server_config(&args(&["--lan", "--host", "192.168.1.5"]), 3000);
        assert_eq!(cfg.bind_host(), "192.168.1.5");
    }

    /// 验证：无值开关（如 `--api-only`）可单独出现。
    #[test]
    fn flag_without_value() {
        let cfg = parse_server_config(&args(&["--api-only"]), 3000);
        assert!(cfg.api_only);
    }
}
