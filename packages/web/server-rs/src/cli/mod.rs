//! CLI layer port (`packages/web/bin`): subcommand dispatch, exit codes,
//! pid/instance/log state. Command groups live in sibling modules.

pub mod args;
pub mod lifecycle;
pub mod misc;
pub mod network;
pub mod paths;
pub mod process;
pub mod serve;
pub mod server_main;
pub mod startup;
pub mod tunnel;
pub mod update_connect;

/// `bin/lib/cli-errors.js` EXIT_CODE.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(pub i32);

pub const SUCCESS: ExitCode = ExitCode(0);
pub const GENERAL_ERROR: ExitCode = ExitCode(1);
pub const USAGE_ERROR: ExitCode = ExitCode(2);
pub const MISSING_DEPENDENCY: ExitCode = ExitCode(3);
pub const AUTH_CONFIG_ERROR: ExitCode = ExitCode(4);
pub const NETWORK_RUNTIME_ERROR: ExitCode = ExitCode(5);

/// `TunnelCliError`: a message plus its process exit code.
#[derive(Debug)]
pub struct CliError {
    pub message: String,
    pub exit_code: ExitCode,
}

impl CliError {
    pub fn new(message: impl Into<String>, exit_code: ExitCode) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(message, USAGE_ERROR)
    }
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Output mode helpers (`cli-output.js`): human (default), `--json`, `--quiet`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    Json,
    Quiet,
}

impl OutputMode {
    pub fn from_options(options: &args::Options) -> Self {
        if options.json {
            OutputMode::Json
        } else if options.quiet {
            OutputMode::Quiet
        } else {
            OutputMode::Human
        }
    }
}

pub fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

/// `cli.js main()` — parse argv, dispatch to a command group.
pub async fn run() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match args::parse_args(&argv) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("Error: {}", error.message);
            return error.exit_code.0;
        }
    };
    run_parsed(parsed).await
}

async fn run_parsed(parsed: args::Parsed) -> i32 {
    use args::Parsed;
    let mode = OutputMode::from_options(&parsed.options);

    if parsed.version_requested {
        match mode {
            OutputMode::Json => print_json(
                &serde_json::json!({ "version": crate::core_routes::ompchamber_version() }),
            ),
            _ => println!("{}", crate::core_routes::ompchamber_version()),
        }
        return 0;
    }

    if !parsed.removed_flag_errors.is_empty() {
        match mode {
            OutputMode::Json => print_json(&serde_json::json!({
                "status": "error",
                "error": {
                    "message": parsed.removed_flag_errors[0],
                    "details": parsed.removed_flag_errors,
                }
            })),
            _ => {
                for error in &parsed.removed_flag_errors {
                    eprintln!("Error: {error}");
                }
            }
        }
        return 1;
    }

    if parsed.help_requested {
        let text = match parsed.command.as_str() {
            "tunnel" => tunnel::help_text().to_string(),
            "startup" => startup::help_text().to_string(),
            "connect-url" => update_connect::connect_url_help_text().to_string(),
            "schedule" => misc::schedule_help_text().to_string(),
            "session" => misc::session_help_text().to_string(),
            "models" => misc::models_help_text().to_string(),
            "projects" => misc::projects_help_text().to_string(),
            "control" => misc::control_help_text().to_string(),
            _ => args::show_help(),
        };
        print!("{text}");
        return 0;
    }

    let options = parsed.options.clone();
    let result: Result<(), CliError> = match parsed.command.as_str() {
        "serve" => serve::command(&parsed, options).await,
        "stop" | "restart" => lifecycle::command(&parsed, options),
        "status" => misc::status_command(&parsed, options),
        "logs" => misc::logs_command(&parsed, options),
        "startup" => startup::command(&parsed, options),
        "tunnel" => tunnel::command(&parsed, options),
        "connect-url" => update_connect::connect_url_command(&parsed, options),
        "update" => update_connect::update_command(&parsed, options),
        "schedule" => misc::schedule_command(&parsed, options),
        "session" => misc::session_command(&parsed, options),
        "models" => misc::models_command(&parsed, options),
        "projects" => misc::projects_command(&parsed, options),
        "control" => misc::control_command(&parsed, options),
        "completion" => args::completion_command(&parsed, &options),
        other => {
            return match mode {
                OutputMode::Json => {
                    print_json(&serde_json::json!({
                        "status": "error",
                        "error": { "message": format!("Unknown command: {other}") }
                    }));
                    1
                }
                _ => {
                    eprintln!("Unknown command: {other}");
                    args::show_help_hint(&other);
                    1
                }
            };
        }
    };

    match result {
        Ok(()) => 0,
        Err(error) => {
            match mode {
                OutputMode::Json => print_json(&serde_json::json!({
                    "status": "error",
                    "error": { "message": error.message }
                })),
                _ => eprintln!("Error: {}", error.message),
            }
            error.exit_code.0
        }
    }
}

/// App composition (moved from main.rs so the CLI owns the single root).
pub fn compose_app(
    ctx: crate::context::RouterContext,
    config: &crate::config::ServerConfig,
    scheduled_tasks_router: axum::Router,
    fs_state: crate::fs_routes::FsState,
    markdown_image_grants_router: axum::Router,
    event_state: std::sync::Arc<crate::event_stream::EventStreamState>,
) -> axum::Router {
    use crate::{
        agent_memory, agent_tool, browser_control, client_auth, core_routes, dev_servers,
        dictation_tts, event_stream, fs_routes, git_service, github, linear, magic_prompts,
        notifications, openchamber_control, openchamber_sessions, opencode_meta, opencode_plugins,
        opencode_routes, permission_auto_accept, project_context, proxy, pwa_manifest, quota,
        realtime_proxy, relay, session_assist, session_folders, session_goal, settings,
        skills_catalog, small_model, static_assets, terminal, tunnels, ui_auth, walkthrough,
    };
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
        .merge(notifications::router_shared(ctx.clone(), event_state))
        .merge(relay::router(ctx.clone()))
        .merge(dictation_tts::router(ctx.clone()))
        .merge(realtime_proxy::router(ctx.clone()))
        .merge(pwa_manifest::router(ctx.clone()))
        .merge(scheduled_tasks_router)
        .layer(ui_auth::middleware(ctx))
        .merge(static_assets::router(config))
}

/// One crate-wide lock for tests that mutate process env (OMPCHAMBER_DATA_DIR,
/// OMPCHAMBER_HOST). Per-module mutexes cannot serialize cross-module env
/// mutation — parallel suites race data-dir resolution to the wrong fixture.
#[cfg(test)]
pub(crate) static TEST_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
