//! Entry point of the Rust OpenChamber web server (server only — the
//! `packages/web/bin` CLI layer is intentionally not ported).
//!
//! Mirrors the boot path `server/index.js main()`: parse serve options
//! (`server/lib/opencode/cli-options.js`), prepare the data dir, start (or
//! connect to) the engine, compose route modules, bind with EADDRINUSE
//! retry, and shut down gracefully on SIGINT/SIGTERM including the managed
//! engine child.

use std::sync::Arc;
use std::time::Duration;

use ompchamber_server::config::{EngineConfig, ServerConfig, parse_server_config};
use ompchamber_server::context::RouterContext;
use ompchamber_server::engine::EngineState;
use ompchamber_server::hub::EventHub;
use ompchamber_server::{
    agent_memory, agent_tool, browser_control, client_auth, core_routes, dev_servers,
    dictation_tts, event_stream, fs_routes, git_service, github, linear, magic_prompts,
    markdown_image_grants, notifications, openchamber_control, openchamber_sessions, opencode_meta,
    opencode_plugins, opencode_routes, permission_auto_accept, project_context, proxy,
    pwa_manifest, quota, realtime_proxy, relay, scheduled_tasks, session_assist, session_folders,
    session_goal, settings, skills_catalog, small_model, static_assets, terminal, tunnels, ui_auth,
    walkthrough,
};

const DEFAULT_PORT: u16 = 3000;
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const BIND_RETRY_WINDOW: Duration = Duration::from_secs(20);

fn print_usage() {
    println!(
        "ompchamber-server — Rust port of the OpenChamber web server\n\n\
         USAGE:\n  ompchamber-server [--port N] [--host H] [--lan] [--ui-password P] [--api-only]\n\n\
         Scope: server only. The `packages/web/bin` CLI layer is intentionally\n\
         not ported; see packages/web/server-rs/PORT-MANIFEST.md."
    );
}

fn build_app(
    ctx: RouterContext,
    config: &ServerConfig,
    scheduled_tasks_router: axum::Router,
    fs_state: fs_routes::FsState,
    markdown_image_grants_router: axum::Router,
    event_state: std::sync::Arc<event_stream::EventStreamState>,
) -> axum::Router {
    axum::Router::new()
        .merge(ui_auth::router(ctx.clone()))
        .merge(proxy::router(ctx.clone()))
        .merge(core_routes::router(ctx.clone()))
        .merge(settings::router(ctx.clone()))
        .merge(fs_routes::router_with(ctx.clone(), fs_state))
        .merge(event_stream::router(ctx.clone()))
        .merge(session_folders::router(ctx.clone()))
        .merge(permission_auto_accept::router(ctx.clone()))
        .merge(magic_prompts::router(ctx.clone()))
        .merge(terminal::router(ctx.clone()))
        .merge(dev_servers::router(ctx.clone()))
        .merge(git_service::router(ctx.clone()))
        .merge(github::router(ctx.clone()))
        .merge(openchamber_sessions::router(ctx.clone()))
        .merge(openchamber_control::router(ctx.clone()))
        .merge(agent_tool::router(ctx.clone()))
        .merge(opencode_routes::router(ctx.clone()))
        .merge(opencode_plugins::router(ctx.clone()))
        .merge(project_context::router(ctx.clone()))
        .merge(agent_memory::router(ctx.clone()))
        .merge(session_assist::router(ctx.clone()))
        .merge(skills_catalog::router(ctx.clone()))
        .merge(quota::router(ctx.clone()))
        .merge(linear::router(ctx.clone()))
        .merge(small_model::router(ctx.clone()))
        .merge(walkthrough::router(ctx.clone()))
        .merge(browser_control::router(ctx.clone()))
        .merge(client_auth::router(ctx.clone()))
        .merge(tunnels::router(ctx.clone()))
        .merge(markdown_image_grants_router)
        .merge(opencode_meta::router(ctx.clone()))
        .merge(notifications::router_shared(
            ctx.clone(),
            Arc::clone(&event_state),
        ))
        .merge(relay::router(ctx.clone()))
        .merge(dictation_tts::router(ctx.clone()))
        .merge(realtime_proxy::router(ctx.clone()))
        .merge(pwa_manifest::router(ctx.clone()))
        .merge(scheduled_tasks_router)
        .layer(ui_auth::middleware(ctx))
        .merge(static_assets::router(config))
}

async fn bind_with_retry(host: &str, port: u16) -> anyhow::Result<tokio::net::TcpListener> {
    // server-startup-runtime.js: a restart races the previous instance's
    // graceful shutdown for the port; retry EADDRINUSE for a bounded window.
    let deadline = tokio::time::Instant::now() + BIND_RETRY_WINDOW;
    loop {
        match tokio::net::TcpListener::bind((host, port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "port {port} still in use after {}s",
                        BIND_RETRY_WINDOW.as_secs()
                    );
                }
                tracing::warn!("port {port} in use; retrying…");
                tokio::time::sleep(BIND_RETRY_INTERVAL).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

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
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .iter()
        .any(|a| a == "-h" || a == "--help" || a == "help")
    {
        print_usage();
        return Ok(());
    }

    let config = Arc::new(parse_server_config(&args, DEFAULT_PORT));
    std::fs::create_dir_all(&config.data_dir)?;

    // Engine: managed child or external server.
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

    // index.js runtime composition: the event stream owns the upstream
    // reader + watcher; the goal runtime subscribes to the hub bridge; the
    // scheduled-task scheduler boots after the engine is up.
    let event_state = event_stream::start(ctx.clone()).await;
    let goal_runtime = session_goal::runtime(&ctx);
    let _goal_bridge = session_goal::spawn_hub_bridge(goal_runtime, Arc::clone(&hub));
    let sse_clients = scheduled_tasks::routes::SseClients::new();
    let (scheduler_runtime, scheduler_service) =
        scheduled_tasks::build_runtime(&ctx, Arc::clone(&sse_clients));
    if let Err(error) = scheduler_runtime.start().await {
        tracing::warn!("scheduled-task scheduler failed to start: {error}");
    }
    let scheduled_tasks_router =
        scheduled_tasks::routes::router_shared(sse_clients, scheduler_service);

    // The markdown-image-grants module mints grants against the SAME store
    // the fs routes serve, so the state is built once and shared.
    let fs_state = fs_routes::FsState::new();
    let markdown_image_grants_router = markdown_image_grants::router(ctx.clone(), fs_state.clone());

    let app = build_app(
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
