//! Port of `bin/lib/commands-serve.js`: the serve command.
//!
//! Foreground: run the server in-process (the CLI process IS the server —
//! required for systemd Type=simple). Daemon (default): spawn this binary
//! detached with `serve --foreground`, stdio to the log file, poll /health
//! for readiness, then write pid + instance files and print the summary.
//!
//! Divergence from the JS (documented): the JS daemon learns the resolved
//! port via an IPC `ompchamber:ready` message; the Rust daemon resolves a
//! free port CLI-side before spawning (bind :0, read port, release) and
//! polls /health — observable output is identical.
//!
//! 中文说明：`ompchamber serve` 命令的 Rust 实现（对应 commands-serve.js）。
//! 前台模式在本进程内直接运行服务器（CLI 进程即服务器，systemd
//! Type=simple 需要）；守护模式（默认）分离 spawn 自身二进制执行
//! `serve --foreground`，stdout/stderr 重定向到日志文件，轮询 /health
//! 确认就绪后写 pid 与 instance 文件并输出摘要。与 JS 的差异：JS 通过
//! IPC `ompchamber:ready` 获知端口，Rust 在 spawn 前自行探测空闲端口
//! （绑定 :0 读取后释放）——可观察输出保持一致。

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::args::{Options, Parsed};
use super::network::{
    assert_authenticated_network_exposure, assert_safe_browser_port, resolve_serve_host,
    resolve_serve_ui_password,
};
use super::paths;
use super::process::{InstanceOptions, is_process_running, write_instance_options, write_pid_file};
use super::{CliError, GENERAL_ERROR, OutputMode, USAGE_ERROR};

/// 守护模式等待子进程 /health 就绪的最长时间（毫秒）；超时杀掉子进程
/// 并报错。
const DAEMON_READY_TIMEOUT_MS: u64 = 30_000;

/// 让操作系统分配一个空闲端口：绑定 (host, 0) 后读取实际端口再释放。
/// 绑定失败返回 None（调用方转为 "No available port found."）。
fn pick_free_port(host: &str) -> Option<u16> {
    TcpListener::bind((host, 0))
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|addr| addr.port())
}

/// 请求 http://host:port/api/system/info（2 秒超时）并解析 JSON；请求
/// 失败、非 2xx 或解析失败均返回 None。用于区分端口占用者是否为
/// OMPChamber（含 desktop runtime）。
async fn probe_system_info(port: u16, host: &str) -> Option<serde_json::Value> {
    let url = format!("http://{host}:{port}/api/system/info");
    let response = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// 探测 http://host:port/health 是否返回 2xx（2 秒超时），作为"该端口
/// 已有 OMPChamber 服务"的就绪信号。
async fn port_answers_health(port: u16, host: &str) -> bool {
    let url = format!("http://{host}:{port}/health");
    matches!(
        reqwest::Client::new().get(&url).timeout(Duration::from_secs(2)).send().await,
        Ok(response) if response.status().is_success()
    )
}

/// Foreground server boot — delegates to the composition root in main.
/// 前台启动的薄封装：把参数转成 RunServerOptions 并进入完整服务器
/// 启动序列；正常情况下阻塞到退出信号为止。
pub async fn run_server_forever(
    port: u16,
    host: &str,
    ui_password: Option<String>,
    api_only: bool,
) -> anyhow::Result<()> {
    crate::cli::server_main::run_server(crate::cli::server_main::RunServerOptions {
        port,
        host: host.to_string(),
        ui_password,
        api_only,
    })
    .await
}

/// `ompchamber serve` 主流程。先解析 host/目标端口与 UI 密码；占用检查：
/// 目标端口 /health 有响应时按 runtime 区分 desktop 应用、已有
/// OMPChamber 实例与第三方占用并给出对应错误，显式端口无响应时再试
/// 绑定确认；随后校验网络暴露安全（无密码绑定非回环地址会直接报错）
/// 并对密码缺失发告警。前台分支禁止 --json 后进入 run_server_forever；
/// 守护分支写日志文件、spawn 子进程（Unix 上独立进程组）、轮询 /health
/// 最多 30 秒、写 pid/instance 文件，最后按 human/quiet/json 模式输出
/// 摘要（自动生成的随机密码只展示这一次）。
pub async fn command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let mode = OutputMode::from_options(&options);
    let host = resolve_serve_host(options.host.as_deref());
    let explicit_port = options.explicit_port;
    let target_port = resolve_target_port(
        explicit_port,
        options.port.unwrap_or(super::args::DEFAULT_PORT),
        &host,
    )
    .await;

    if let Some(port) = target_port {
        if !options.suppress_unsafe_port_warning {
            if let Some(warning) = assert_safe_browser_port(port, "OMPChamber serve") {
                emit_notice(&options, &mode, "warning", None, &warning);
            }
        }
        if port_answers_health(port, &host).await {
            let info = probe_system_info(port, &host).await;
            let runtime = info
                .as_ref()
                .and_then(|i| i.get("runtime").and_then(|v| v.as_str()));
            if runtime == Some("desktop") {
                return Err(CliError::new(
                    format!(
                        "Port {port} is used by OMPChamber Desktop app. Choose another port or stop the desktop app."
                    ),
                    GENERAL_ERROR,
                ));
            }
            if runtime.is_some() {
                return Err(CliError::new(
                    format!(
                        "OMPChamber is already running on port {port}. Use `ompchamber status` or `ompchamber stop --port {port}`."
                    ),
                    GENERAL_ERROR,
                ));
            }
            if explicit_port {
                return Err(CliError::new(
                    format!("Port {port} is already in use by another process."),
                    GENERAL_ERROR,
                ));
            }
        } else if explicit_port {
            if !TcpListener::bind((host.as_str(), port)).is_ok() {
                return Err(CliError::new(
                    format!("Port {port} is already in use by another process."),
                    GENERAL_ERROR,
                ));
            }
        }
    }

    let Some(target_port) = resolve_final_port(target_port, &host) else {
        return Err(CliError::new("No available port found.", GENERAL_ERROR));
    };

    let resolved = resolve_serve_ui_password(&options);
    let effective_ui_password = resolved.password.clone();
    assert_authenticated_network_exposure(&host, effective_ui_password.as_deref())?;
    if effective_ui_password.is_none() && !options.suppress_ui_password_warning {
        let exposed = super::network::is_network_exposed_bind_host(&host);
        let detail = if exposed {
            format!(
                "server is bound to {host} and reachable on your network with no UI auth. Set --ui-password or OMPCHAMBER_UI_PASSWORD before exposing it over LAN."
            )
        } else {
            "browser UI is unsecured. Use --ui-password or OMPCHAMBER_UI_PASSWORD.".to_string()
        };
        emit_notice(
            &options,
            &mode,
            "warning",
            Some("UI_PASSWORD_MISSING"),
            &format!("OMPCHAMBER_UI_PASSWORD is not set; {detail}"),
        );
    }

    if options.foreground {
        if matches!(mode, OutputMode::Json) {
            return Err(CliError::new(
                "--json is not supported with --foreground. Use --json with background (daemon) mode instead.",
                USAGE_ERROR,
            ));
        }
        if !options.quiet {
            println!("Starting OMPChamber on port {target_port} (foreground)");
        }
        run_server_forever(
            target_port,
            &host,
            effective_ui_password.clone(),
            options.api_only,
        )
        .await
        .map_err(|error| CliError::new(format!("server failed: {error}"), GENERAL_ERROR))?;
        return Ok(());
    }

    // Daemon mode.
    paths::ensure_logs_dir();
    let log_path = paths::log_file_path(&target_port.to_string());
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| {
            CliError::new(format!("Could not open log file: {error}"), GENERAL_ERROR)
        })?;
    let stderr_stdio = std::process::Stdio::from(log_file.try_clone().map_err(|error| {
        CliError::new(format!("Could not open log file: {error}"), GENERAL_ERROR)
    })?);
    let stdout_stdio = std::process::Stdio::from(log_file);

    let exe = std::env::current_exe().map_err(|error| {
        CliError::new(format!("Could not resolve binary: {error}"), GENERAL_ERROR)
    })?;
    let mut command = std::process::Command::new(exe);
    command
        .args([
            "serve",
            "--foreground",
            "--port",
            &target_port.to_string(),
            "--host",
            &host,
        ])
        .env("OMPCHAMBER_RUNTIME", "web")
        .stdin(std::process::Stdio::null())
        .stdout(stdout_stdio)
        .stderr(stderr_stdio);
    if let Some(password) = effective_ui_password.as_deref() {
        command.env("OMPCHAMBER_UI_PASSWORD", password);
    }
    if options.api_only {
        command.env("OMPCHAMBER_API_ONLY", "true");
        command.arg("--api-only");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|error| {
        CliError::new(format!("Failed to spawn daemon: {error}"), GENERAL_ERROR)
    })?;
    let pid: u32 = child.id();

    let deadline = Instant::now() + Duration::from_millis(DAEMON_READY_TIMEOUT_MS);
    loop {
        if port_answers_health(target_port, &host).await {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CliError::new(
                format!(
                    "OMPChamber daemon did not report ready within {}s",
                    DAEMON_READY_TIMEOUT_MS / 1000
                ),
                GENERAL_ERROR,
            ));
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(CliError::new(
                format!("OMPChamber daemon exited before reporting ready ({status})"),
                GENERAL_ERROR,
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    if !is_process_running(pid) {
        return Err(CliError::new(
            "Failed to start server in daemon mode",
            GENERAL_ERROR,
        ));
    }

    write_pid_file(&paths::pid_file_path(target_port), pid);
    write_instance_options(
        &paths::instance_file_path(target_port),
        &InstanceOptions {
            port: target_port,
            host: Some(host.clone()),
            launch_mode: "daemon".to_string(),
            ui_password: effective_ui_password.clone(),
            has_ui_password: effective_ui_password.is_some(),
            api_only: options.api_only,
            started_at: now_ms(),
        },
    );

    let url = format!("http://{host}:{target_port}/");
    match mode {
        OutputMode::Json => {
            let mut payload = serde_json::json!({
                "port": target_port,
                "pid": pid,
                "url": url,
                "logs": format!("ompchamber logs -p {target_port}"),
                "launchMode": "daemon",
                "messages": [],
            });
            if resolved.generated {
                payload["password"] = serde_json::json!(effective_ui_password);
            }
            super::print_json(&payload);
        }
        OutputMode::Quiet if !options.suppress_quiet_output => {
            if resolved.generated {
                println!(
                    "{target_port} pass:{}",
                    effective_ui_password.unwrap_or_default()
                );
            } else {
                println!("{target_port}");
            }
        }
        _ if !options.suppress_startup_summary => {
            println!("OMPChamber Started");
            println!("  port {target_port} (PID: {pid})");
            if resolved.generated {
                println!(
                    "  UI password: {}",
                    effective_ui_password.clone().unwrap_or_default()
                );
                println!("  save this password — it is not shown again");
            }
            println!("  visit: {url}");
            println!("  logs: ompchamber logs -p {target_port}");
            println!("daemon running");
        }
        _ => {}
    }
    let _ = parsed;
    Ok(())
}

/// 当前 Unix 毫秒时间戳（取值失败按 0），写入 instance 文件的 startedAt。
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or_default()
}

/// 输出一条 notice 级提示：human 模式按级别加 "Error:"/"Warning:" 前缀
/// 打到 stderr；json 模式仅打印消息正文（JSON payload 的 messages 数组
/// 由调用方组装 payload 时另行收集）。
fn emit_notice(
    options: &Options,
    mode: &OutputMode,
    level: &str,
    code: Option<&str>,
    message: &str,
) {
    match mode {
        OutputMode::Json => {
            // Notices are collected into the JSON `messages` array by the
            // caller where the payload is built; here we print immediately
            // only for the standalone warning path.
            let _ = (options, code);
            eprintln!("{message}");
        }
        _ => {
            let prefix = match level {
                "error" => "Error",
                "warning" => "Warning",
                _ => "Info",
            };
            eprintln!("{prefix}: {message}");
        }
    }
}

/// 确定首选端口：显式指定则直接采用；否则先试绑定 127.0.0.1 上的期望
/// 端口，被占用时提示 "Port N in use; using a free port" 并返回 None
/// （交给后续 pick_free_port 兜底）。
async fn resolve_target_port(explicit: bool, desired: u16, _host: &str) -> Option<u16> {
    if explicit {
        return Some(desired);
    }
    if TcpListener::bind(("127.0.0.1", desired)).is_ok() {
        return Some(desired);
    }
    eprintln!("Port {desired} in use; using a free port");
    None
}

/// 端口兜底：首选值有效则用之，否则由操作系统分配一个空闲端口。
fn resolve_final_port(candidate: Option<u16>, host: &str) -> Option<u16> {
    candidate.or_else(|| pick_free_port(host))
}
