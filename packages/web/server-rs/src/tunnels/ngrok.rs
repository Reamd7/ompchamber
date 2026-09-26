//! Port of `server/lib/ngrok-tunnel.js` plus
//! `server/lib/tunnels/providers/ngrok.js` (the provider adapter).

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{Value, json};

use super::cloudflare::Reachability;
use super::executable_search::{create_executable_search_env, real_env, real_home};
use super::install_help::get_tunnel_dependency_install_info;
use super::registry::{
    AvailabilityInfo, DiagnoseRequest, StartContext, StartFailure, TunnelController, TunnelProvider,
};
use super::runner::{ChildStream, CommandRunner, spawn_output_readers};
use super::types::{
    ModeDescriptor, Platform, TUNNEL_INTENT_EPHEMERAL_PUBLIC, TUNNEL_MODE_QUICK,
    TUNNEL_PROVIDER_NGROK, TunnelServiceError, TunnelStartRequest,
};

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_POLL_INTERVAL_MS: u64 = 250;
pub const NGROK_API_URL: &str = "http://127.0.0.1:4040/api/tunnels";
const NGROK_AUTHTOKEN_HELP: &str = "Run: ngrok config add-authtoken <your-ngrok-token>";

fn ngrok_install_message() -> String {
    get_tunnel_dependency_install_info(TUNNEL_PROVIDER_NGROK, Platform::current().js_name()).message
}

#[derive(Debug, Clone, Default)]
pub struct RawAvailability {
    pub available: bool,
    pub path: Option<String>,
    pub version: Option<String>,
}

/// `checkNgrokAvailable()`.
pub fn check_ngrok_available_raw(runner: &dyn CommandRunner) -> RawAvailability {
    let target = runner.resolve("ngrok");
    if let Some(target) = target
        && let Some(result) = runner.probe(&target.command, &["version"], &target.env)
        && result.status == Some(0)
    {
        let version = {
            let stdout = result.stdout.trim();
            if !stdout.is_empty() {
                stdout.to_string()
            } else {
                result.stderr.trim().to_string()
            }
        };
        return RawAvailability {
            available: true,
            path: Some(target.command),
            version: Some(version),
        };
    }
    RawAvailability {
        available: false,
        path: None,
        version: None,
    }
}

/// `checkNgrokAuthtokenConfigured(ngrokPath)`.
pub fn check_ngrok_authtoken_configured(
    runner: &dyn CommandRunner,
    ngrok_path: Option<&str>,
) -> (bool, String) {
    if let Ok(value) = std::env::var("NGROK_AUTHTOKEN")
        && !value.trim().is_empty()
    {
        return (true, "NGROK_AUTHTOKEN is set.".to_string());
    }

    let target = match ngrok_path {
        Some(path) => Some(super::executable_search::LaunchTarget {
            command: path.to_string(),
            env: create_executable_search_env(&real_env(), Platform::current(), &real_home()),
        }),
        None => runner.resolve("ngrok"),
    };
    let Some(target) = target else {
        return (false, ngrok_install_message());
    };

    match runner.probe(&target.command, &["config", "check"], &target.env) {
        Some(result) => {
            let output = format!("{}{}", result.stdout, result.stderr)
                .trim()
                .to_string();
            if result.status == Some(0) {
                (
                    true,
                    if output.is_empty() {
                        "ngrok config is valid.".to_string()
                    } else {
                        output
                    },
                )
            } else {
                (
                    false,
                    if output.is_empty() {
                        NGROK_AUTHTOKEN_HELP.to_string()
                    } else {
                        output
                    },
                )
            }
        }
        None => (false, ngrok_install_message()),
    }
}

/// `checkNgrokApiReachability()` (probe injected by the provider).
pub async fn check_ngrok_api_reachability(http: &reqwest::Client, timeout_ms: u64) -> Reachability {
    let request = http
        .get("https://api.ngrok.com/")
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

fn ngrok_env() -> super::executable_search::EnvMap {
    create_executable_search_env(&real_env(), Platform::current(), &real_home())
}

/// `normalizeNgrokPublicUrl(value)`: https URL on an ngrok host, one trailing
/// slash stripped (JS `parsed.toString().replace(/\/$/, '')`).
pub fn normalize_ngrok_public_url(value: Option<&str>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let Ok(parsed) = url::Url::parse(trimmed) else {
        return None;
    };
    if parsed.scheme() != "https" {
        return None;
    }
    let hostname = parsed.host_str()?;
    if !hostname.contains("ngrok") {
        return None;
    }
    let rendered = parsed.to_string();
    Some(rendered.strip_suffix('/').unwrap_or(&rendered).to_string())
}

/// `/https:\/\/[^\s"']+/i` — first https run without whitespace or quotes.
fn https_run(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut pos = 0;
    while pos + 8 <= bytes.len() {
        if line[pos..pos + 8].eq_ignore_ascii_case("https://") {
            let start = pos;
            let mut end = start + 8;
            while end < bytes.len()
                && !bytes[end].is_ascii_whitespace()
                && bytes[end] != b'"'
                && bytes[end] != b'\''
            {
                end += 1;
            }
            return Some(line[start..end].to_string());
        }
        pos += 1;
    }
    None
}

/// `extractNgrokPublicUrlFromText(text)`: per line, JSON `url`/`public_url`
/// first, then the https-run regex.
pub fn extract_ngrok_public_url_from_text(text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(parsed) = serde_json::from_str::<Value>(line) {
            let from_url = parsed
                .get("url")
                .and_then(|value| value.as_str())
                .and_then(|url| normalize_ngrok_public_url(Some(url)));
            if let Some(url) = from_url {
                return Some(url);
            }
            let from_public_url = parsed
                .get("public_url")
                .and_then(|value| value.as_str())
                .and_then(|url| normalize_ngrok_public_url(Some(url)));
            if let Some(url) = from_public_url {
                return Some(url);
            }
        }
        if let Some(matched) = https_run(line)
            && let Some(url) = normalize_ngrok_public_url(Some(&matched))
        {
            return Some(url);
        }
    }
    None
}

/// `normalizeNgrokDiagnosticText(value)`.
fn normalize_ngrok_diagnostic_text(value: &str) -> String {
    value
        .replace('\r', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn json_string_field(parsed: &Value, key: &str) -> Option<String> {
    parsed.get(key).and_then(Value::as_str).map(str::to_string)
}

fn is_error_level(level: &str) -> bool {
    matches!(level, "eror" | "error" | "crit")
}

/// `summarizeNgrokOutput(lines)`: most relevant ngrok log diagnostic.
pub fn summarize_ngrok_output(lines: &[String]) -> String {
    let non_empty: Vec<String> = lines
        .iter()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    if non_empty.is_empty() {
        return String::new();
    }

    for line in non_empty.iter().rev() {
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let level = json_string_field(&parsed, "lvl")
            .map(|value| value.to_lowercase())
            .unwrap_or_default();
        if !is_error_level(&level) {
            continue;
        }
        if let Some(err) = json_string_field(&parsed, "err") {
            let err = normalize_ngrok_diagnostic_text(&err);
            if !err.is_empty() && err != "<nil>" {
                return err;
            }
        }
    }

    for line in non_empty.iter().rev() {
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(err) = json_string_field(&parsed, "err") {
            let err = normalize_ngrok_diagnostic_text(&err);
            if !err.is_empty() && err != "<nil>" && !err.to_lowercase().contains("context canceled")
            {
                return err;
            }
        }
        if let Some(msg) = json_string_field(&parsed, "msg") {
            let lower = msg.to_lowercase();
            if ["failed", "error", "invalid", "auth"]
                .iter()
                .any(|needle| lower.contains(needle))
            {
                return normalize_ngrok_diagnostic_text(&msg);
            }
        }
    }

    let error_lines: Vec<String> = non_empty
        .iter()
        .filter(|line| line.to_lowercase().starts_with("error:"))
        .map(|line| {
            let stripped = strip_error_prefix(line);
            normalize_ngrok_diagnostic_text(stripped)
        })
        .filter(|line| !line.is_empty())
        .collect();
    if !error_lines.is_empty() {
        return error_lines[..error_lines.len().min(4)].join(" ");
    }

    let Some(last_line) = non_empty.last() else {
        return String::new();
    };
    if let Ok(parsed) = serde_json::from_str::<Value>(last_line) {
        if let Some(err) = json_string_field(&parsed, "err")
            && !err.trim().is_empty()
        {
            return normalize_ngrok_diagnostic_text(&err);
        }
        if let Some(msg) = json_string_field(&parsed, "msg")
            && !msg.trim().is_empty()
        {
            return normalize_ngrok_diagnostic_text(&msg);
        }
    }
    normalize_ngrok_diagnostic_text(last_line)
}

fn strip_error_prefix(line: &str) -> &str {
    let lower = line.to_lowercase();
    let Some(pos) = lower.find("error:") else {
        return line;
    };
    let after = &line[pos + "error:".len()..];
    after.trim_start()
}

/// `appendNgrokOutputSummary(message, lines)`.
fn append_ngrok_output_summary(message: &str, lines: &[String]) -> String {
    let summary = summarize_ngrok_output(lines);
    if summary.is_empty() {
        message.to_string()
    } else {
        format!("{message}: {summary}")
    }
}

/// `fetchNgrokPublicUrl()`: local ngrok agent API poll.
pub async fn fetch_ngrok_public_url(http: &reqwest::Client, api_url: &str) -> Option<String> {
    let response = http.get(api_url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload: Value = response.json().await.ok()?;
    let tunnels = payload.get("tunnels")?.as_array()?;
    let https_tunnel = tunnels.iter().find(|entry| {
        entry.get("proto").and_then(|value| value.as_str()) == Some("https")
            && entry
                .get("public_url")
                .and_then(|value| value.as_str())
                .and_then(|url| normalize_ngrok_public_url(Some(url)))
                .is_some()
    });
    let fallback = tunnels.iter().find(|entry| {
        entry
            .get("public_url")
            .and_then(|value| value.as_str())
            .and_then(|url| normalize_ngrok_public_url(Some(url)))
            .is_some()
    });
    let pick = https_tunnel.or(fallback)?;
    normalize_ngrok_public_url(pick.get("public_url").and_then(|value| value.as_str()))
}

#[derive(Debug, Clone)]
pub struct NgrokTunnelOpts {
    pub startup_timeout_ms: u64,
    pub poll_interval_ms: u64,
    pub api_url: String,
}

impl Default for NgrokTunnelOpts {
    fn default() -> Self {
        Self {
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            api_url: NGROK_API_URL.to_string(),
        }
    }
}

/// `startNgrokQuickTunnel({ port })`.
pub async fn start_ngrok_quick_tunnel(
    port: Option<u16>,
    runner: &dyn CommandRunner,
    http: &reqwest::Client,
    opts: &NgrokTunnelOpts,
) -> Result<TunnelController, String> {
    let ngrok_check = check_ngrok_available_raw(runner);
    if !ngrok_check.available {
        return Err(ngrok_install_message());
    }

    let (authtoken_configured, authtoken_detail) =
        check_ngrok_authtoken_configured(runner, ngrok_check.path.as_deref());
    if !authtoken_configured {
        let detail = if authtoken_detail.is_empty() {
            NGROK_AUTHTOKEN_HELP.to_string()
        } else {
            authtoken_detail
        };
        return Err(format!("ngrok authtoken is not configured. {detail}"));
    }

    let Some(port) = port else {
        return Err("A local port is required to start an ngrok tunnel".to_string());
    };

    let args = vec![
        "http".to_string(),
        "--log=stdout".to_string(),
        "--log-format=json".to_string(),
        format!("127.0.0.1:{port}"),
    ];
    let env = ngrok_env();
    let command = ngrok_check.path.unwrap_or_else(|| "ngrok".to_string());
    let mut spawned = runner
        .spawn(&command, &args, &env)
        .map_err(|error| format!("Ngrok failed to start: {error}"))?;
    let mut chunks = spawn_output_readers(&mut spawned);

    let mut public_url: Option<String> = None;
    let mut recent_output: Vec<String> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(opts.startup_timeout_ms);
    let api_url = opts.api_url.clone();
    let http = http.clone();
    let mut poll_tick = tokio::time::interval(Duration::from_millis(opts.poll_interval_ms.max(1)));
    poll_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let outcome: Result<String, String> = loop {
        tokio::select! {
            chunk = chunks.recv() => {
                if let Some(chunk) = chunk {
                    if chunk.stream == ChildStream::Stderr {
                        eprint!("{}", chunk.text);
                    }
                    if let Some(parsed) = extract_ngrok_public_url_from_text(&chunk.text) {
                        public_url = Some(parsed);
                    }
                    for line in chunk.text.split('\n') {
                        let trimmed = line.trim_end_matches('\r').trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        recent_output.push(trimmed.to_string());
                        if recent_output.len() > 200 {
                            recent_output.remove(0);
                        }
                    }
                    if let Some(url) = &public_url {
                        break Ok(url.clone());
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            exit = &mut spawned.exit => {
                let code = exit.unwrap_or(None);
                break Err(append_ngrok_output_summary(
                    &format!(
                        "Ngrok exited while starting (code {})",
                        code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".to_string())
                    ),
                    &recent_output,
                ));
            }
            _ = poll_tick.tick() => {
                if public_url.is_none() {
                    public_url = fetch_ngrok_public_url(&http, &api_url).await;
                }
                if let Some(url) = &public_url {
                    break Ok(url.clone());
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                (spawned.kill)();
                break Err(append_ngrok_output_summary(
                    "Ngrok tunnel URL not received within 30 seconds",
                    &recent_output,
                ));
            }
        }
    };

    let public_url = outcome?;
    let kill = spawned.kill;
    Ok(TunnelController {
        provider: None,
        mode: TUNNEL_MODE_QUICK.to_string(),
        public_url: Some(public_url),
        stop: Some(kill),
        effective_config_path: None,
        resolved_hostname: None,
    })
}

// ---------------------------------------------------------------------------
// Provider adapter (providers/ngrok.js)
// ---------------------------------------------------------------------------

pub static NGROK_MODES: [ModeDescriptor; 1] = [ModeDescriptor {
    key: TUNNEL_MODE_QUICK,
    label: "Quick Tunnel",
    intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC,
    requires: &[],
    supports: &["sessionTTL"],
    stability: "beta",
}];

pub fn ngrok_capabilities_json() -> Value {
    json!({
        "provider": TUNNEL_PROVIDER_NGROK,
        "defaults": { "mode": TUNNEL_MODE_QUICK, "optionDefaults": {} },
        "modes": NGROK_MODES.iter().map(|mode| mode.to_json()).collect::<Vec<_>>(),
    })
}

pub struct NgrokTunnelProvider {
    runner: Arc<dyn CommandRunner>,
    http: reqwest::Client,
    api_probe: Option<super::cloudflare::ApiProbe>,
    opts: NgrokTunnelOpts,
}

impl NgrokTunnelProvider {
    pub fn new(runner: Arc<dyn CommandRunner>, http: reqwest::Client) -> Self {
        Self {
            runner,
            http,
            api_probe: None,
            opts: NgrokTunnelOpts::default(),
        }
    }

    /// Test seam (offline API reachability).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_api_probe(mut self, probe: super::cloudflare::ApiProbe) -> Self {
        self.api_probe = Some(probe);
        self
    }

    fn reachability(&self) -> futures::future::BoxFuture<'static, Reachability> {
        if let Some(probe) = &self.api_probe {
            return probe();
        }
        let http = self.http.clone();
        Box::pin(async move { check_ngrok_api_reachability(&http, 5_000).await })
    }
}

impl TunnelProvider for NgrokTunnelProvider {
    fn id(&self) -> &'static str {
        TUNNEL_PROVIDER_NGROK
    }

    fn capabilities_json(&self) -> Value {
        ngrok_capabilities_json()
    }

    fn mode_descriptors(&self) -> &'static [ModeDescriptor] {
        &NGROK_MODES
    }

    fn check_availability(&self) -> BoxFuture<'static, AvailabilityInfo> {
        let runner = self.runner.clone();
        Box::pin(async move {
            let raw = check_ngrok_available_raw(runner.as_ref());
            let install = get_tunnel_dependency_install_info(
                TUNNEL_PROVIDER_NGROK,
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

    fn diagnose(&self, _request: DiagnoseRequest) -> BoxFuture<'static, Value> {
        let runner = self.runner.clone();
        let reachability = self.reachability();
        Box::pin(async move {
            let dependency = check_ngrok_available_raw(runner.as_ref());
            let (authtoken_configured, authtoken_detail) =
                check_ngrok_authtoken_configured(runner.as_ref(), dependency.path.as_deref());
            let network = reachability.await;
            let startup_ready = dependency.available && authtoken_configured && network.reachable;

            let provider_checks = vec![
                json!({
                    "id": "dependency",
                    "label": "ngrok installed",
                    "status": if dependency.available { "pass" } else { "fail" },
                    "detail": if dependency.available {
                        dependency.version.clone()
                            .filter(|v| !v.is_empty())
                            .or_else(|| dependency.path.clone())
                            .unwrap_or_else(|| "ngrok available".to_string())
                    } else {
                        ngrok_install_message()
                    },
                }),
                json!({
                    "id": "authtoken",
                    "label": "ngrok authtoken configured",
                    "status": if authtoken_configured { "pass" } else { "fail" },
                    "detail": if !authtoken_detail.is_empty() {
                        authtoken_detail.clone()
                    } else {
                        NGROK_AUTHTOKEN_HELP.to_string()
                    },
                }),
                json!({
                    "id": "network",
                    "label": "ngrok API reachable",
                    "status": if network.reachable { "pass" } else { "fail" },
                    "detail": if network.reachable {
                        network.status.map(|status| format!("HTTP {status}"))
                            .unwrap_or_else(|| "Reachable".to_string())
                    } else {
                        network.error.clone()
                            .unwrap_or_else(|| "Could not reach api.ngrok.com".to_string())
                    },
                }),
            ];

            let modes = vec![json!({
                "mode": TUNNEL_MODE_QUICK,
                "checks": [{
                    "id": "startup_readiness",
                    "label": "Provider startup readiness",
                    "status": if startup_ready { "pass" } else { "fail" },
                    "detail": if startup_ready {
                        "Provider dependency, auth, and network checks passed."
                    } else {
                        "Resolve provider checks before starting tunnels."
                    },
                }],
                "summary": {
                    "ready": startup_ready,
                    "failures": if startup_ready { 0 } else { 1 },
                    "warnings": 0,
                },
                "ready": startup_ready,
                "blockers": if startup_ready {
                    Value::Array(Vec::new())
                } else {
                    Value::Array(vec![Value::from("Resolve provider checks before starting tunnels.")])
                },
            })];

            json!({ "providerChecks": provider_checks, "modes": modes })
        })
    }

    fn start(
        &self,
        request: TunnelStartRequest,
        context: StartContext,
    ) -> BoxFuture<'static, Result<TunnelController, StartFailure>> {
        let runner = self.runner.clone();
        let opts = self.opts.clone();
        let http = self.http.clone();
        Box::pin(async move {
            if request.mode != TUNNEL_MODE_QUICK {
                return Err(StartFailure::Service(TunnelServiceError::new(
                    "mode_unsupported",
                    "Ngrok only supports 'quick' mode right now",
                )));
            }
            start_ngrok_quick_tunnel(context.active_port, runner.as_ref(), &http, &opts)
                .await
                .map_err(StartFailure::Raw)
        })
    }

    fn stop(&self, controller: &TunnelController) {
        controller.stop();
    }

    fn get_metadata(&self, _controller: Option<&TunnelController>) -> Value {
        Value::Null
    }
}

#[cfg(test)]
#[path = "ngrok_tests.rs"]
mod ngrok_tests;
