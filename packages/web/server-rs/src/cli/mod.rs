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
pub mod ui;
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
            // cli.js: unknown command → USAGE_ERROR with closest-match hint.
            const KNOWN: [&str; 13] = [
                "serve", "stop", "restart", "status", "schedule", "session", "models", "projects",
                "control", "tunnel", "startup", "logs", "update",
            ];
            let suggestion = find_closest_match(other, &KNOWN);
            let hint = suggestion
                .map(|s| format!(" Did you mean '{s}'?"))
                .unwrap_or_default();
            return match mode {
                OutputMode::Json => {
                    print_json(&serde_json::json!({
                        "status": "error",
                        "error": { "message": format!("Unknown command '{other}'.{hint}") },
                        "messages": [{ "level": "info", "code": "USAGE_HELP", "message": "Use --help to see available commands" }]
                    }));
                    USAGE_ERROR.0
                }
                _ => {
                    eprintln!("Error: Unknown command '{other}'.{hint}");
                    eprintln!("Use --help to see available commands");
                    USAGE_ERROR.0
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

/// The CORS mirror set from the JS server's express middleware: packaged
/// WebView clients whose origin never matches the server host (desktop
/// `ompchamber-ui://app`, iOS/Android Capacitor), localhost for local dev,
/// plus arbitrary localhost/127.0.0.1 dev-server ports. Without these the
/// packaged desktop UI (loaded from the custom scheme) is blocked by the
/// browser on every request — reproduced against the alpha.4 app.
fn packaged_or_local_dev_origin(origin: &str) -> bool {
    const PACKAGED: [&str; 4] = [
        "ompchamber-ui://app",
        "capacitor://localhost",
        "http://localhost",
        "https://localhost",
    ];
    if PACKAGED.contains(&origin) {
        return true;
    }
    // isLocalDevClientOrigin: http(s)://(localhost|127.0.0.1):<port>
    let (scheme, rest) = match origin.split_once("://") {
        Some(pair) => pair,
        None => return false,
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some(pair) => pair,
        None => return false,
    };
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    host == "localhost" || host == "127.0.0.1"
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
    use tower_http::cors::{AllowOrigin, CorsLayer};

    let cors = CorsLayer::new()
        // Mirror the request origin for packaged WebView clients and local
        // dev servers (same predicate as the JS express middleware).
        .allow_origin(AllowOrigin::predicate(|origin, _request_parts| {
            origin
                .to_str()
                .map(packaged_or_local_dev_origin)
                .unwrap_or(false)
        }))
        .allow_credentials(true)
        .allow_methods([
            http::Method::GET,
            http::Method::POST,
            http::Method::PUT,
            http::Method::PATCH,
            http::Method::DELETE,
            http::Method::OPTIONS,
        ])
        .allow_headers([
            http::header::CONTENT_TYPE,
            http::header::AUTHORIZATION,
            http::header::ACCEPT,
            http::HeaderName::from_static("x-requested-with"),
            http::header::CACHE_CONTROL,
            http::HeaderName::from_static("x-opencode-directory"),
            http::HeaderName::from_static("x-opencode-directory-encoding"),
            http::HeaderName::from_static("x-omp-epoch"),
            http::HeaderName::from_static("last-event-id"),
            http::HeaderName::from_static("ngrok-skip-browser-warning"),
        ])
        .expose_headers([http::HeaderName::from_static("x-next-cursor")]);

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
        // Applied last so it covers every route merged above (Router::layer
        // only wraps routes registered before it).
        .layer(cors)
}

/// One crate-wide lock for tests that mutate process env (OMPCHAMBER_DATA_DIR,
/// OMPCHAMBER_HOST). Per-module mutexes cannot serialize cross-module env
/// mutation — parallel suites race data-dir resolution to the wrong fixture.
#[cfg(test)]
pub(crate) static TEST_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// cli-args.js `findClosestMatch`: levenshtein distance <= 3, case-insensitive.
fn find_closest_match(input: &str, candidates: &[&'static str]) -> Option<&'static str> {
    fn levenshtein(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut dp: Vec<usize> = (0..=b.len()).collect();
        for (i, ca) in a.iter().enumerate() {
            let mut prev = dp[0];
            dp[0] = i + 1;
            for (j, cb) in b.iter().enumerate() {
                let temp = dp[j + 1];
                dp[j + 1] = (dp[j] + 1)
                    .min(dp[j + 1] + 1)
                    .min(prev + usize::from(ca != cb));
                prev = temp;
            }
        }
        dp[b.len()]
    }
    if input.is_empty() {
        return None;
    }
    let normalized = input.to_lowercase();
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let distance = levenshtein(&normalized, &candidate.to_lowercase());
        if distance < best.map(|(d, _)| d).unwrap_or(4) {
            best = Some((distance, candidate));
        }
    }
    best.filter(|(d, _)| *d <= 3).map(|(_, c)| c)
}

#[cfg(test)]
mod cors_tests {
    use super::packaged_or_local_dev_origin;

    #[test]
    fn packaged_webview_origins_are_allowed() {
        assert!(packaged_or_local_dev_origin("ompchamber-ui://app"));
        assert!(packaged_or_local_dev_origin("capacitor://localhost"));
        assert!(packaged_or_local_dev_origin("http://localhost"));
        assert!(packaged_or_local_dev_origin("https://localhost"));
    }

    #[test]
    fn local_dev_server_origins_are_allowed() {
        assert!(packaged_or_local_dev_origin("http://localhost:5173"));
        assert!(packaged_or_local_dev_origin("https://127.0.0.1:8080"));
    }

    #[test]
    fn foreign_origins_are_rejected() {
        assert!(!packaged_or_local_dev_origin("https://evil.example.com"));
        assert!(!packaged_or_local_dev_origin("http://example.com:5173"));
        assert!(!packaged_or_local_dev_origin("file:///tmp"));
        assert!(!packaged_or_local_dev_origin(""));
    }
}
