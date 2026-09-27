//! CLI layer port (`packages/web/bin`): subcommand dispatch, exit codes,
//! pid/instance/log state. Command groups live in sibling modules.
//!
//! 中文说明：CLI 层入口（对应 packages/web/bin）：退出码常量、CliError、
//! 输出模式（human/json/quiet）、argv 解析后的子命令分发（run/run_parsed），
//! 以及从 main.rs 迁来的 axum 应用组装根 compose_app 与 CORS origin
//! 判定。各命令组位于兄弟模块：args（选项解析）、serve、startup、tunnel、
//! lifecycle（stop/restart）、misc（status/logs 等）、process（pid/进程）、
//! paths、network、ui（终端渲染）、server_main（前台启动序列）、
//! update_connect。

/// 命令行选项解析（cli-args.js）：Options/Parsed 结构与 parse_args。
pub mod args;
/// stop/restart 命令组：定位实例并终止进程树。
pub mod lifecycle;
/// status/logs/schedule/session/models/projects/control 等杂项命令组。
pub mod misc;
/// serve 的 host/端口/UI 密码解析与网络暴露安全校验。
pub mod network;
/// 数据目录、run 目录、pid/instance/日志文件路径的统一解析。
pub mod paths;
/// pid/instance 文件读写、进程存活与身份校验、进程树终止。
pub mod process;
/// serve 命令：前台与守护两种运行模式。
pub mod serve;
/// 前台服务器的完整启动序列（原 main() 的内容）。
pub mod server_main;
/// startup status|enable|disable：开机自启集成（launchd/systemd/schtasks）。
pub mod startup;
/// tunnel 命令组：隧道创建与管理。
pub mod tunnel;
/// 终端 human 输出的 clack 风格行渲染（intro/info/success/error/outro）。
pub mod ui;
/// update 与 connect-url 命令组。
pub mod update_connect;

/// `bin/lib/cli-errors.js` EXIT_CODE.
/// 进程退出码（对应 JS 侧 EXIT_CODE 常量表）；随 CliError 返回给 main，
/// 决定进程最终退出状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitCode(pub i32);

/// 成功（0）。
pub const SUCCESS: ExitCode = ExitCode(0);
/// 一般错误（1）：IO 失败、外部命令失败、服务器启动失败等。
pub const GENERAL_ERROR: ExitCode = ExitCode(1);
/// 用法错误（2）：未知命令/选项、参数缺失或非法。
pub const USAGE_ERROR: ExitCode = ExitCode(2);
/// 缺少外部依赖（3）。
pub const MISSING_DEPENDENCY: ExitCode = ExitCode(3);
/// 认证配置错误（4）。
pub const AUTH_CONFIG_ERROR: ExitCode = ExitCode(4);
/// 网络运行时错误（5）。
pub const NETWORK_RUNTIME_ERROR: ExitCode = ExitCode(5);

/// `TunnelCliError`: a message plus its process exit code.
/// CLI 错误：面向用户的消息加进程退出码。main 据此打印
/// "Error: <message>"（json 模式输出 error 对象）并以 exit_code 退出。
#[derive(Debug)]
pub struct CliError {
    /// 面向用户的错误消息（human 模式直接打印）。
    pub message: String,
    /// 进程退出码。
    pub exit_code: ExitCode,
}

/// CliError 的构造辅助。
impl CliError {
    /// 以指定消息与退出码构造错误。
    pub fn new(message: impl Into<String>, exit_code: ExitCode) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }
    /// 构造 USAGE_ERROR（退出码 2）错误的快捷方式。
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(message, USAGE_ERROR)
    }
}

/// Display 直接输出 message，便于在日志与错误链上下文中使用。
impl std::fmt::Display for CliError {
    /// 写出错误消息本身。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Output mode helpers (`cli-output.js`): human (default), `--json`, `--quiet`.
/// 三种输出模式：human（默认多行可读文本）、json（机器可读）、quiet
/// （最简单行）。由 --json/--quiet 推导，决定所有命令的输出渲染分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// 默认人类可读输出。
    Human,
    /// --json：输出机器可读 JSON。
    Json,
    /// -q/--quiet：压缩到最简的一行输出。
    Quiet,
}

/// 从解析后的 Options 推导输出模式。
impl OutputMode {
    /// --json 优先于 --quiet，二者皆无则为 human。
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

/// 以 pretty 格式把 JSON 打到 stdout（json 模式的统一输出出口）。
pub fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

/// `cli.js main()` — parse argv, dispatch to a command group.
/// CLI 入口：读取 argv（跳过程序名）→ parse_args（失败打印错误并返回
/// 对应退出码）→ run_parsed 分发。返回值即进程退出码。
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

/// 解析后的分发核心。顺序：--version 先输出版本（按模式）；removed-flag
/// 错误优先报错退出（码 1）；--help 按命令组输出对应帮助文本；随后按
/// command 分发到各命令组，未知命令用 find_closest_match 给出
/// "Did you mean" 提示并返回 USAGE_ERROR。命令返回 Err 时按模式打印
/// 错误并返回其退出码。
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
/// compose_app 的 AllowOrigin::predicate 使用本函数。
fn packaged_or_local_dev_origin(origin: &str) -> bool {
    // 打包客户端的固定 origin 白名单（桌面自定义 scheme 与 Capacitor）。
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
/// 组装整个 axum 应用：合并全部功能路由（ui_auth/proxy/core/settings/
/// fs/event/sessions/git/... 等），套上 ui_auth 鉴权中间件与静态资源，
/// 最后叠加 CORS 层——Router::layer 只覆盖此前注册的路由，因此 CORS
/// 必须最后添加。从 main.rs 迁来，使 CLI 成为唯一的组装根。
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
/// 测试专用：加锁方式为先 lock 再修改环境变量，测试结束释放。
#[cfg(test)]
pub(crate) static TEST_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// cli-args.js `findClosestMatch`: levenshtein distance <= 3, case-insensitive.
/// 大小写不敏感的 Levenshtein 距离匹配（滚动数组实现），返回距离 <= 3
/// 的最近候选词；输入为空或全部超阈值时返回 None。
fn find_closest_match(input: &str, candidates: &[&'static str]) -> Option<&'static str> {
    // 经典滚动数组编辑距离：dp[j] 表示 a[0..=i] 与 b[0..=j] 的距离。
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

/// packaged_or_local_dev_origin 的 CORS 放行/拒绝行为用例。
#[cfg(test)]
mod cors_tests {
    use super::packaged_or_local_dev_origin;

    /// 验证打包 WebView 的固定 origin 全部被放行。
    #[test]
    fn packaged_webview_origins_are_allowed() {
        assert!(packaged_or_local_dev_origin("ompchamber-ui://app"));
        assert!(packaged_or_local_dev_origin("capacitor://localhost"));
        assert!(packaged_or_local_dev_origin("http://localhost"));
        assert!(packaged_or_local_dev_origin("https://localhost"));
    }

    /// 验证任意端口的 localhost/127.0.0.1 开发服务器 origin 被放行。
    #[test]
    fn local_dev_server_origins_are_allowed() {
        assert!(packaged_or_local_dev_origin("http://localhost:5173"));
        assert!(packaged_or_local_dev_origin("https://127.0.0.1:8080"));
    }

    /// 验证外部域名、非 http(s) scheme 与空 origin 均被拒绝。
    #[test]
    fn foreign_origins_are_rejected() {
        assert!(!packaged_or_local_dev_origin("https://evil.example.com"));
        assert!(!packaged_or_local_dev_origin("http://example.com:5173"));
        assert!(!packaged_or_local_dev_origin("file:///tmp"));
        assert!(!packaged_or_local_dev_origin(""));
    }
}
