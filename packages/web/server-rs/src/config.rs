//! CLI/env option parsing, mirroring `server/lib/opencode/cli-options.js`
//! (`parseServeCliOptions`) plus the engine env resolution from
//! `env-config.js` (`resolveOpenCodeEnvConfig`).

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Web server port (JS: `options.port`, default 3000).
    pub port: u16,
    /// Explicit bind host from `--host` (JS: `options.host`).
    pub host: Option<String>,
    /// `--lan` maps to binding `0.0.0.0`.
    pub lan: bool,
    /// `--ui-password` / OMPCHAMBER_UI_PASSWORD / OPENCODE_UI_PASSWORD.
    pub ui_password: Option<String>,
    /// `--api-only` / OMPCHAMBER_API_ONLY: no browser UI assets.
    pub api_only: bool,
    /// OMPCHAMBER_DATA_DIR or `~/.config/ompchamber`.
    pub data_dir: PathBuf,
    /// OMPCHAMBER_DIST_DIR or `<web package>/dist`.
    pub dist_dir: PathBuf,
    /// Tunnel options are parsed for parity; wiring lands with the tunnels port.
    pub tunnel: TunnelOptions,
    /// Managed/external engine selection.
    pub engine: EngineConfig,
}

#[derive(Debug, Clone, Default)]
pub struct TunnelOptions {
    pub try_cf_tunnel: bool,
    pub provider: Option<String>,
    pub mode: Option<String>,
    pub config_path: Option<String>,
    pub token: Option<String>,
    pub hostname: Option<String>,
}

#[derive(Debug, Clone)]
pub enum EngineConfig {
    /// Spawn the omp host child (Bun + server/lib/omp-host/host.ts).
    Managed { hostname: String },
    /// Connect to an external OpenCode-compatible server; never spawn.
    External { base_url: String },
}

impl ServerConfig {
    /// Effective bind host: `--host` > `--lan` (0.0.0.0) > OMPCHAMBER_HOST > loopback.
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

fn env_flag_enabled(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("1" | "true"))
}

fn env_password() -> Option<String> {
    std::env::var("OMPCHAMBER_UI_PASSWORD")
        .or_else(|_| std::env::var("OPENCODE_UI_PASSWORD"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn default_data_dir() -> PathBuf {
    std::env::var("OMPCHAMBER_DATA_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config").join("ompchamber")))
        .unwrap_or_else(|| PathBuf::from(".ompchamber-data"))
}

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

fn env_port() -> Option<u16> {
    std::env::var("OMPCHAMBER_PORT")
        .ok()
        .and_then(|v| v.trim().parse::<u16>().ok())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_inline_and_separate_values() {
        let cfg = parse_server_config(&args(&["--port=8080", "--host", "0.0.0.0"]), 3000);
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.host.as_deref(), Some("0.0.0.0"));
    }

    #[test]
    fn invalid_port_falls_back_to_default() {
        let cfg = parse_server_config(&args(&["--port", "nope"]), 3000);
        assert_eq!(cfg.port, 3000);
    }

    #[test]
    fn does_not_consume_flag_like_values() {
        let cfg = parse_server_config(&args(&["--ui-password", "--api-only"]), 3000);
        assert_eq!(cfg.ui_password, None);
        assert!(cfg.api_only);
    }
    #[test]
    fn lan_binds_all_interfaces_when_no_host() {
        let cfg = parse_server_config(&args(&["--lan"]), 3000);
        assert_eq!(cfg.bind_host(), "0.0.0.0");
    }

    #[test]
    fn host_overrides_lan() {
        let cfg = parse_server_config(&args(&["--lan", "--host", "192.168.1.5"]), 3000);
        assert_eq!(cfg.bind_host(), "192.168.1.5");
    }

    #[test]
    fn flag_without_value() {
        let cfg = parse_server_config(&args(&["--api-only"]), 3000);
        assert!(cfg.api_only);
    }
}
