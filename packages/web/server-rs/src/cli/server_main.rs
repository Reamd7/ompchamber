//! The server boot sequence as a callable function (foreground serve).
//!
//! Extracted verbatim from the former `main()` so the CLI serve command can
//! run the server in-process (foreground) while `main` dispatches commands.
//!
//! 中文说明：前台（foreground）serve 模式的服务器启动序列，即原 main()
//! 的完整内容：tracing 初始化、UI 密码注入、解析 ServerConfig、按需启动
//! desktop control 通道、启动托管/外部引擎、构建事件总线与各路由
//! （最终经 cli::compose_app 合成 axum Router）、绑定端口并以优雅退出
//! 方式运行。CLI 的 `serve --foreground` 分支与守护模式 spawn 出的子进程
//! 都从这里进入。

use std::sync::Arc;
use std::time::Duration;

/// 前台启动服务器的全部参数，由 serve 命令（前台分支或守护子进程的
/// argv）构造。
pub struct RunServerOptions {
    /// 监听端口。
    pub port: u16,
    /// 绑定地址（IP 或主机名）。
    pub host: String,
    /// UI 密码；Some 时在启动早期写入 OMPCHAMBER_UI_PASSWORD 环境变量。
    pub ui_password: Option<String>,
    /// 仅暴露 API 路由，不服务浏览器 UI 静态资源。
    pub api_only: bool,
}

/// 启动并在前台运行 OMPChamber 服务器，直到收到退出信号。步骤：初始化
/// tracing → 注入 UI 密码 → 解析配置并创建数据目录 → （按
/// OMPCHAMBER_DESKTOP_CONTROL 环境变量）启动 desktop control 通道 →
/// 启动托管引擎或连接外部引擎（未就绪则报错退出）→ 构建
/// EventHub/RouterContext → 启动事件流、session goal 桥、定时任务调度
/// 器 → compose_app 组装路由 → 绑定监听（AddrInUse 时重试 20 秒）→
/// 优雅退出后关闭引擎。返回 Err 时由 CLI 包装为 GENERAL_ERROR。
pub async fn run_server(options: RunServerOptions) -> anyhow::Result<()> {
    use crate::config::{EngineConfig, parse_server_config};
    use crate::context::RouterContext;
    use crate::engine::EngineState;
    use crate::hub::EventHub;
    use crate::scheduled_tasks;
    use crate::{event_stream, fs_routes, markdown_image_grants, session_goal};

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // SAFETY: single-threaded boot phase before the server spawns workers.
    if let Some(password) = options.ui_password.as_deref().filter(|p| !p.is_empty()) {
        unsafe { std::env::set_var("OMPCHAMBER_UI_PASSWORD", password) };
    }

    let mut argv: Vec<String> = vec![
        "--port".to_string(),
        options.port.to_string(),
        "--host".to_string(),
        options.host.clone(),
    ];
    if options.api_only {
        argv.push("--api-only".to_string());
    }
    let config = Arc::new(parse_server_config(&argv, options.port));
    std::fs::create_dir_all(&config.data_dir)?;

    // Desktop control channel (Electron spawn sets the env): must start
    // before composition so the notification hooks register in time.
    let mut _control_task = None;
    if std::env::var("OMPCHAMBER_DESKTOP_CONTROL").as_deref() == Ok("true") {
        match crate::desktop_control::serve(config.data_dir.clone()).await {
            Ok(task) => _control_task = Some(task),
            Err(error) => tracing::warn!("desktop control channel failed to start: {error}"),
        }
    }

    let engine: Arc<EngineState> = match &config.engine {
        EngineConfig::Managed { .. } => {
            let state = EngineState::start_managed(&config).await?;
            state.spawn_health_monitor();
            state
        }
        EngineConfig::External { base_url } => {
            tracing::info!("Connecting to external engine at {base_url}");
            let password = std::env::var("OPENCODE_SERVER_PASSWORD")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
            let state = EngineState::external(base_url.clone(), password);
            if !state.probe_health().await {
                tracing::warn!("external engine at {base_url} did not answer /global/health");
            }
            state
        }
    };
    if !engine.is_ready() {
        anyhow::bail!("engine did not become ready");
    }

    let hub = EventHub::new();
    let ctx = RouterContext {
        config: Arc::clone(&config),
        engine: Arc::clone(&engine),
        hub: Arc::clone(&hub),
    };

    crate::desktop_control::set_engine(Arc::clone(&engine));

    let event_state = event_stream::start(ctx.clone()).await;
    let goal_runtime = session_goal::runtime(&ctx);
    let _goal_bridge = session_goal::spawn_hub_bridge(goal_runtime, Arc::clone(&hub));
    let sse_clients = scheduled_tasks::routes::SseClients::new();
    let (scheduler_runtime, scheduler_service) =
        scheduled_tasks::build_runtime(&ctx, Arc::clone(&sse_clients));
    if let Err(error) = scheduler_runtime.start().await {
        tracing::warn!("scheduled-task scheduler failed to start: {error}");
    }
    {
        let scheduler_probe = scheduler_service.clone();
        crate::desktop_control::set_scheduler_status(Arc::new(move || {
            scheduler_probe.status()
        }));
    }
    let scheduled_tasks_router =
        scheduled_tasks::routes::router_shared(sse_clients, scheduler_service);

    let fs_state = fs_routes::FsState::new();
    let markdown_image_grants_router = markdown_image_grants::router(ctx.clone(), fs_state.clone());

    let app = crate::cli::compose_app(
        ctx,
        &config,
        scheduled_tasks_router,
        fs_state,
        markdown_image_grants_router,
        event_state,
    );

    let host = config.bind_host();
    let port = config.port;
    let listener = bind_with_retry(&host, port).await?;
    tracing::info!("OMPChamber (Rust) server listening on http://{host}:{port}");

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());
    server.await?;

    tracing::info!("shutting down…");
    engine.shutdown().await;
    tracing::info!("shutdown complete");
    Ok(())
}

/// 绑定 (host, port)：遇到 AddrInUse 每 500ms 重试，最多 20 秒后报
/// "port ... still in use after 20s"；其它绑定错误立即上抛。
async fn bind_with_retry(host: &str, port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match tokio::net::TcpListener::bind((host, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!("port {port} still in use after 20s");
                }
                tracing::warn!("port {port} in use; retrying…");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// 优雅退出的触发源：Ctrl+C、SIGTERM（非 Unix 平台永远等不到该信号，
/// 用 pending future 占位）或 desktop control 通道的关闭请求；任一到达
/// 即完成 future，触发 axum 的 graceful shutdown。
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
        _ = crate::desktop_control::shutdown_requested() => {
            tracing::info!("shutdown requested via desktop control channel");
        },
    }
}
