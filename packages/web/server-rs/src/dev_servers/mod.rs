//! Port of `server/lib/dev-servers/{parse,routes}.js`.
//!
//! Dev-server discovery by enumerating listening sockets (lsof → /proc/net/tcp
//! fallback; netstat on Windows). Discovery is advisory: a failed scan reports
//! failure (503 with reason), never an empty list, and failures are not cached.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;

const SCAN_TIMEOUT_MS: u64 = 2_500;
const CACHE_TTL: Duration = Duration::from_millis(3_000);

/// Ports that are listening but are never a preview target (parse.js's
/// deliberately short list).
const IGNORED_PORTS: [u16; 9] = [22, 53, 445, 631, 5432, 3306, 6379, 27017, 9229];

#[derive(Debug, Clone, PartialEq)]
pub struct Listener {
    pub port: u16,
    pub pid: Option<u32>,
    pub command: String,
}

const LOOPBACK_TOKENS: [&str; 4] = ["127.0.0.1", "localhost", "[::1]", "::1"];
const WILDCARD_TOKENS: [&str; 4] = ["*", "0.0.0.0", "[::]", "::"];

/// A bind address reachable from this machine over loopback. LAN-only binds
/// are excluded: `http://localhost:<port>` would not reach them.
pub fn is_locally_reachable_host(host: &str) -> bool {
    LOOPBACK_TOKENS.contains(&host) || WILDCARD_TOKENS.contains(&host)
}

fn split_host_port(value: &str) -> Option<(String, String)> {
    let raw = value.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some(rest) = raw.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = format!("[{}]", &rest[..close]);
        let after = &rest[close + 1..];
        let port = after.strip_prefix(':')?;
        return Some((host, port.to_string()));
    }
    let separator = raw.rfind(':')?;
    Some((
        raw[..separator].to_string(),
        raw[separator + 1..].to_string(),
    ))
}

fn to_port(value: &str) -> Option<u16> {
    value.parse::<u16>().ok()
}

/// Parses `lsof -iTCP -sTCP:LISTEN -P -n -F pcn` output: stateful field
/// records (`p`/`c` open a process, `n` lines belong to it), de-duplicated by
/// port, first pid-carrying entry wins.
pub fn parse_lsof_listeners(output: &str) -> Vec<Listener> {
    let mut by_port: std::collections::BTreeMap<u16, Listener> = std::collections::BTreeMap::new();
    let mut pid: Option<u32> = None;
    let mut command = String::new();

    for line in output.split('\n') {
        if line.is_empty() {
            continue;
        }
        let tag = line.as_bytes()[0] as char;
        let value = &line[1..];

        match tag {
            'p' => {
                pid = value.parse::<u32>().ok();
                command = String::new();
            }
            'c' => {
                command = value.trim().to_string();
            }
            'n' => {
                if value.contains("->") {
                    continue;
                }
                let Some((host, port_raw)) = split_host_port(value) else {
                    continue;
                };
                let Some(port) = to_port(&port_raw) else {
                    continue;
                };
                if !is_locally_reachable_host(&host) {
                    continue;
                }
                let entry = by_port.entry(port).or_insert(Listener {
                    port,
                    pid: None,
                    command: String::new(),
                });
                if entry.pid.is_none() {
                    entry.pid = pid;
                    entry.command = command.clone();
                }
            }
            _ => {}
        }
    }
    by_port.into_values().collect()
}

/// Parses `netstat -ano -p TCP` output (Windows): no command names.
pub fn parse_netstat_listeners(output: &str) -> Vec<Listener> {
    let mut by_port: std::collections::BTreeMap<u16, Listener> = std::collections::BTreeMap::new();
    for line in output.split('\n') {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        if !parts[0].eq_ignore_ascii_case("tcp") {
            continue;
        }
        if !parts[3].eq_ignore_ascii_case("LISTENING") {
            continue;
        }
        let Some((host, port_raw)) = split_host_port(parts[1]) else {
            continue;
        };
        let Some(port) = to_port(&port_raw) else {
            continue;
        };
        if !is_locally_reachable_host(&host) {
            continue;
        }
        let pid = parts.get(4).and_then(|v| v.parse::<u32>().ok());
        by_port.entry(port).or_insert(Listener {
            port,
            pid,
            command: String::new(),
        });
    }
    by_port.into_values().collect()
}

const PROC_STATE_LISTEN: &str = "0A";
const PROC_WILDCARD_ADDRESSES: [&str; 2] = ["00000000", "00000000000000000000000000000000"];
const PROC_LOOPBACK_ADDRESSES: [&str; 2] = ["0100007F", "00000000000000000000000001000000"];

/// Parses `/proc/net/tcp{,6}`: LISTEN sockets on wildcard or loopback binds,
/// no owner information.
pub fn parse_proc_net_tcp_listeners(output: &str) -> Vec<Listener> {
    let mut by_port: std::collections::BTreeMap<u16, Listener> = std::collections::BTreeMap::new();
    for line in output.split('\n') {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        if parts[3] != PROC_STATE_LISTEN {
            continue;
        }
        let Some((address, port_hex)) = parts[1].split_once(':') else {
            continue;
        };
        let normalized = address.to_ascii_uppercase();
        if !PROC_WILDCARD_ADDRESSES.contains(&normalized.as_str())
            && !PROC_LOOPBACK_ADDRESSES.contains(&normalized.as_str())
        {
            continue;
        }
        let Ok(port) = u16::from_str_radix(port_hex, 16) else {
            continue;
        };
        if port == 0 {
            continue;
        }
        by_port.entry(port).or_insert(Listener {
            port,
            pid: None,
            command: String::new(),
        });
    }
    by_port.into_values().collect()
}

/// Narrows raw listeners to preview candidates: own ports/pids and the
/// ignored-port list drop out; everything listening stays.
pub fn select_dev_server_candidates(
    listeners: Vec<Listener>,
    own_ports: &[u16],
    own_pids: &[u32],
) -> Vec<Listener> {
    listeners
        .into_iter()
        .filter(|entry| {
            !own_ports.contains(&entry.port)
                && !entry.pid.is_some_and(|pid| own_pids.contains(&pid))
                && !IGNORED_PORTS.contains(&entry.port)
        })
        .collect()
}

pub enum ScanOutcome {
    Ok { servers: Vec<Value> },
    Unavailable { reason: &'static str },
}

async fn run_command(binary: &str, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_millis(SCAN_TIMEOUT_MS),
        tokio::process::Command::new(binary)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if output.status.success() || !stdout.is_empty() {
        Some(stdout)
    } else {
        None
    }
}

async fn read_proc_listeners() -> Option<Vec<Listener>> {
    let mut tables: Option<Vec<Listener>> = None;
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = tokio::fs::read_to_string(path).await {
            let mut merged = tables.take().unwrap_or_default();
            for entry in parse_proc_net_tcp_listeners(&text) {
                if !merged.iter().any(|l| l.port == entry.port) {
                    merged.push(entry);
                }
            }
            merged.sort_by_key(|l| l.port);
            tables = Some(merged);
        }
    }
    tables
}

pub struct DevServerScanner {
    cache: Mutex<Option<(Instant, Vec<Value>)>>,
}

impl Default for DevServerScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl DevServerScanner {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(None),
        }
    }

    pub async fn discover(&self, own_ports: &[u16]) -> ScanOutcome {
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, servers)) = cache.as_ref()
                && at.elapsed() < CACHE_TTL
            {
                return ScanOutcome::Ok {
                    servers: servers.clone(),
                };
            }
        }

        let scan: Result<Vec<Listener>, &'static str> = if cfg!(windows) {
            match run_command("netstat", &["-ano", "-p", "TCP"]).await {
                Some(output) => Ok(parse_netstat_listeners(&output)),
                None => Err("netstat-unavailable"),
            }
        } else {
            match run_command("lsof", &["-iTCP", "-sTCP:LISTEN", "-P", "-n", "-F", "pcn"]).await {
                Some(output) => Ok(parse_lsof_listeners(&output)),
                None => match read_proc_listeners().await {
                    Some(listeners) => Ok(listeners),
                    None => Err("no-listener-source"),
                },
            }
        };

        let listeners = match scan {
            Ok(listeners) => listeners,
            // Not cached: a transient failure must not suppress the next attempt.
            Err(reason) => return ScanOutcome::Unavailable { reason },
        };

        let own_pid = std::process::id();
        let servers: Vec<Value> = select_dev_server_candidates(listeners, own_ports, &[own_pid])
            .into_iter()
            .map(|entry| {
                json!({
                    "port": entry.port,
                    "pid": entry.pid,
                    "command": entry.command,
                    "url": format!("http://localhost:{}/", entry.port),
                })
            })
            .collect();

        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((Instant::now(), servers.clone()));
        ScanOutcome::Ok { servers }
    }
}

#[derive(Clone)]
struct ModuleState {
    scanner: std::sync::Arc<DevServerScanner>,
    config: std::sync::Arc<crate::config::ServerConfig>,
}

async fn list_dev_servers(State(state): State<ModuleState>) -> Response {
    // Own listeners: the web port (plus the managed engine's port, which
    // RouterContext does not expose yet — tracked in PORT-MANIFEST).
    let own_ports = vec![state.config.port];
    match state.scanner.discover(&own_ports).await {
        ScanOutcome::Ok { servers } => Json(json!({ "servers": servers })).into_response(),
        ScanOutcome::Unavailable { reason } => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "Port discovery is unavailable", "reason": reason })),
        )
            .into_response(),
    }
}

pub fn router(ctx: RouterContext) -> Router {
    Router::new()
        .route("/api/dev-servers", get(list_dev_servers))
        .with_state(ModuleState {
            scanner: std::sync::Arc::new(DevServerScanner::new()),
            config: ctx.config,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsof_parser_is_stateful_and_dedupes_by_port() {
        let output =
            "p4242\nctest-server\nn127.0.0.1:5173\nn[::1]:5173\nn0.0.0.0:3000\np99\nnother\n";
        let listeners = parse_lsof_listeners(output);
        assert_eq!(
            listeners,
            vec![
                Listener {
                    port: 3000,
                    pid: Some(4242),
                    command: "test-server".into()
                },
                Listener {
                    port: 5173,
                    pid: Some(4242),
                    command: "test-server".into()
                },
            ]
        );
    }

    #[test]
    fn lsof_excludes_established_and_lan_binds() {
        let output = "p1\nca\nn127.0.0.1:5555->10.0.0.1:80\nn192.168.1.5:6000\np2\ncb\nn*:7000\n";
        let listeners = parse_lsof_listeners(output);
        assert_eq!(
            listeners,
            vec![Listener {
                port: 7000,
                pid: Some(2),
                command: "b".into()
            }]
        );
    }

    #[test]
    fn netstat_parser_filters_listen_state_only() {
        let output = "  TCP    0.0.0.0:8000     0.0.0.0:0    LISTENING    777\n  TCP    127.0.0.1:9000   10.0.0.1:5   ESTABLISHED 888\n";
        let listeners = parse_netstat_listeners(output);
        assert_eq!(
            listeners,
            vec![Listener {
                port: 8000,
                pid: Some(777),
                command: String::new()
            }]
        );
    }

    #[test]
    fn proc_parser_decodes_hex_ports_and_filters_binds() {
        let output = "  0: 0100007F:143C 00000000:0000 0A 00000000:00000000 00 00000000 0 0 0\n  1: 0100007F:143D 00000000:0000 01 00000000:00000000\n  2: C0A80105:143E 00000000:0000 0A\n  3: 00000000:1F90 00000000:0000 0A\n";
        let listeners = parse_proc_net_tcp_listeners(output);
        // 143C = 5180 (loopback), 1F90 = 8080 (wildcard); state 01 and LAN bind
        // dropped. Ascending port order, matching the JS sort.
        assert_eq!(
            listeners,
            vec![
                Listener {
                    port: 5180,
                    pid: None,
                    command: String::new()
                },
                Listener {
                    port: 8080,
                    pid: None,
                    command: String::new()
                },
            ]
        );
    }

    #[test]
    fn selection_excludes_own_ports_pids_and_ignored_ports() {
        let listeners = vec![
            Listener {
                port: 3000,
                pid: Some(1),
                command: String::new(),
            }, // own port
            Listener {
                port: 5432,
                pid: None,
                command: String::new(),
            }, // postgres
            Listener {
                port: 5173,
                pid: Some(42),
                command: String::new(),
            }, // own pid
            Listener {
                port: 4000,
                pid: Some(7),
                command: String::new(),
            }, // survives
        ];
        let selected = select_dev_server_candidates(listeners, &[3000], &[42]);
        assert_eq!(
            selected.iter().map(|l| l.port).collect::<Vec<_>>(),
            vec![4000]
        );
    }

    #[test]
    fn host_reachability_rules() {
        assert!(is_locally_reachable_host("127.0.0.1"));
        assert!(is_locally_reachable_host("localhost"));
        assert!(is_locally_reachable_host("[::1]"));
        assert!(is_locally_reachable_host("0.0.0.0"));
        assert!(is_locally_reachable_host("*"));
        assert!(!is_locally_reachable_host("192.168.1.5"));
    }
}
