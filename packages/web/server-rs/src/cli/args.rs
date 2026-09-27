//! Port of `bin/lib/cli-args.js` `parseArgs`: the full option table,
//! positional command surface, removed-flag errors, help/version requests.
//!
//! 中文说明：cli-args.js `parseArgs` 的移植：完整选项表（Options 的每个
//! 字段对应一个 CLI 旗标）、位置参数命令面（serve/tunnel/startup/
//! schedule/session/control 等）、已移除旗标的错误文案、help/version
//! 请求标记。未知选项不在此处直接报错，而是收集进
//! removed_flag_errors 由上层统一处理。

use super::{CliError, USAGE_ERROR};

/// 未指定端口时的默认 Web 端口（3000）。
pub const DEFAULT_PORT: u16 = 3000;

/// 全部 CLI 选项的解析结果；旗标缺省时保持默认值。各命令组按需读取
/// 对应字段，explicit_* 布尔用于区分"用户显式给出"与"来自默认/环境"。
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// --port/-p：Web 服务器端口（已通过 1..=65535 校验）。
    pub port: Option<u16>,
    /// 用户是否显式指定了端口（影响端口被占用时的错误行为与自动换端口）。
    pub explicit_port: bool,
    /// --host：绑定地址；--lan 或 tunnel 外的 --hostname 会回填到它。
    pub host: Option<String>,
    /// --lan：局域网可用；未另给 host 时默认绑定 0.0.0.0。
    pub lan: bool,
    /// --ui-password：浏览器 UI 单密码；None 表示未给或"显式给出但待生成"。
    pub ui_password: Option<String>,
    /// 用户是否显式给出 --ui-password（即使值为空）。
    pub explicit_ui_password: bool,
    /// --provider：隧道/引擎 provider 名称。
    pub provider: Option<String>,
    /// --mode：provider 运行模式（如 quick）。
    pub mode: Option<String>,
    /// --profile：配置 profile 名称。
    pub profile: Option<String>,
    /// --name：命名资源（如会话/worktree）的名称。
    pub name: Option<String>,
    /// --title：展示标题。
    pub title: Option<String>,
    /// --worktree：git worktree 路径或名称。
    pub worktree: Option<String>,
    /// --branch：git 分支名。
    pub branch: Option<String>,
    /// --start-ref/--base：起始引用。
    pub start_ref: Option<String>,
    /// --upstream/--no-upstream：是否设置上游（Some(true)/Some(false)）。
    pub set_upstream: Option<bool>,
    /// --project：项目标识。
    pub project: Option<String>,
    /// --dir/--directory：工作目录。
    pub directory: Option<String>,
    /// --task：任务标识。
    pub task: Option<String>,
    /// --session：会话标识。
    pub session: Option<String>,
    /// --message：附加消息文本。
    pub message: Option<String>,
    /// --prompt：提示词文本。
    pub prompt: Option<String>,
    /// --model：模型名称。
    pub model: Option<String>,
    /// --daily：每日调度表达式。
    pub daily: Option<String>,
    /// --weekly：每周调度表达式。
    pub weekly: Option<String>,
    /// --once：单次调度时间。
    pub once: Option<String>,
    /// --time：调度时刻。
    pub time: Option<String>,
    /// --cron：cron 表达式。
    pub cron: Option<String>,
    /// --timezone：时区标识。
    pub timezone: Option<String>,
    /// --agent：代理标识。
    pub agent: Option<String>,
    /// --variant：变体选择。
    pub variant: Option<String>,
    /// --disabled：以禁用态创建（如调度任务）。
    pub disabled: bool,
    /// --goal：goal 模式旗标。
    pub goal: bool,
    /// --goal-token-budget：goal 的 token 预算。
    pub goal_token_budget: Option<String>,
    /// --config：配置文件路径。
    pub config_path: Option<String>,
    /// --token：bearer token 字面值。
    pub token: Option<String>,
    /// --token-file：读取 token 的文件路径。
    pub token_file: Option<String>,
    /// --token-stdin：从 stdin 读取 token。
    pub token_stdin: bool,
    /// --hostname：主机名；tunnel 命令之外作为 host 的别名。
    pub hostname: Option<String>,
    /// --server/--server-url：服务器 URL（非空校验）。
    pub server: Option<String>,
    /// --connect-ttl：connect 链接存活时间。
    pub connect_ttl: Option<String>,
    /// --session-ttl：会话存活时间。
    pub session_ttl: Option<String>,
    /// --json：机器可读 JSON 输出。
    pub json: bool,
    /// --all：作用于全部对象。
    pub all: bool,
    /// --last：作用于最近一条。
    pub last: bool,
    /// --last-assistant：最近一条助手消息。
    pub last_assistant: bool,
    /// --wait：等待完成。
    pub wait: bool,
    /// --timeout：超时时长。
    pub timeout: Option<String>,
    /// --with-status：附带状态信息。
    pub with_status: bool,
    /// --role：消息角色。
    pub role: Option<String>,
    /// --no-follow：Some(false) 表示不跟随输出流。
    pub follow: Option<bool>,
    /// --no-env-snapshot：Some(false) 表示 startup 集成不快照当前环境。
    pub env_snapshot: Option<bool>,
    /// --lines：输出行数（正整数，非法值静默忽略）。
    pub lines: Option<u32>,
    /// --limit：数量上限（正整数，非法值报 USAGE_ERROR）。
    pub limit: Option<u32>,
    /// --relay：relay 模式。
    pub relay: bool,
    /// --qr/--no-qr：是否显示二维码。
    pub qr: Option<bool>,
    /// 用户是否显式给出 --qr/--no-qr。
    pub explicit_qr: bool,
    /// --force：强制执行（跳过确认）。
    pub force: bool,
    /// --show-secrets：输出中显示密钥明文。
    pub show_secrets: bool,
    /// --dry-run：只演示不落盘。
    pub dry_run: bool,
    /// --plain：朴素输出（无装饰）。
    pub plain: bool,
    /// --quiet/-q：精简输出。
    pub quiet: bool,
    /// --foreground/--no-daemon：前台运行（不守护化）。
    pub foreground: bool,
    /// --api-only：仅 API 路由，不服务 UI 静态资源。
    pub api_only: bool,
    /// --suppress-unsafe-port-warning：关闭不安全端口告警。
    pub suppress_unsafe_port_warning: bool,
    /// --suppress-ui-password-warning：关闭 UI 密码缺失告警。
    pub suppress_ui_password_warning: bool,
    /// --suppress-quiet-output：quiet 模式下也不输出。
    pub suppress_quiet_output: bool,
    /// --suppress-startup-summary：不打印启动摘要。
    pub suppress_startup_summary: bool,
}

/// Options 的派生查询。
impl Options {
    /// 显式端口优先，否则回退解析 OMPCHAMBER_PORT 环境变量（在
    /// parse_args 收尾阶段调用）。
    fn effective_port(&self) -> Option<u16> {
        self.port.or_else(|| {
            std::env::var("OMPCHAMBER_PORT")
                .ok()
                .and_then(|v| v.trim().parse::<u16>().ok())
        })
    }
}

/// parse_args 的完整结果：命令结构（command 与各命令组的 action 提槽）、
/// 选项、已移除旗标错误、help/version 标记与原始位置参数。
#[derive(Debug, Clone)]
pub struct Parsed {
    /// 主命令（第一个位置参数；缺省 "serve"）。
    pub command: String,
    /// 二级命令（目前仅 tunnel 使用，缺省 "help"）。
    pub subcommand: Option<String>,
    /// tunnel 的第三个位置参数（具体动作）。
    pub tunnel_action: Option<String>,
    /// startup 的动作（缺省 "status"）。
    pub startup_action: Option<String>,
    /// schedule 的动作（缺省 "help"）。
    pub schedule_action: Option<String>,
    /// session 的动作（缺省 "help"）。
    pub session_action: Option<String>,
    /// control 的动作（缺省 "help"）。
    pub control_action: Option<String>,
    /// 全部旗标的解析结果。
    pub options: Options,
    /// 已移除/未知旗标的错误文案；非空时上层以退出码 1 统一报错。
    pub removed_flag_errors: Vec<String>,
    /// --help/-h 是否出现。
    pub help_requested: bool,
    /// --version/-v 是否出现。
    pub version_requested: bool,
    /// 全部位置参数原文（含命令本身）。
    pub positionals: Vec<String>,
}

/// 拆分后的选项 token：名称、内联值（--name=value 形式）与长短形式。
struct Token {
    /// 选项名（不含前导破折号）。
    name: String,
    /// = 后面的内联值；没有则为 None。
    inline_value: Option<String>,
    /// 是否为 -- 长形式（影响未知选项错误文案的前缀）。
    long: bool,
}

/// `splitOptionToken`: `--name[=value]` / `-name[=value]`; `--` is positional.
/// 解析 `--name[=value]` 与 `-name[=value]`；单独的 `--`、`-` 或普通
/// 参数返回 None（按位置参数处理）。
fn split_option_token(arg: &str) -> Option<Token> {
    if let Some(rest) = arg.strip_prefix("--") {
        if rest.is_empty() {
            return None;
        }
        let (name, inline) = match rest.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (rest.to_string(), None),
        };
        return Some(Token {
            name,
            inline_value: inline,
            long: true,
        });
    }
    if let Some(rest) = arg.strip_prefix('-') {
        if rest.is_empty() {
            return None;
        }
        let (name, inline) = match rest.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (rest.to_string(), None),
        };
        return Some(Token {
            name,
            inline_value: inline,
            long: false,
        });
    }
    None
}

/// 取选项值：优先非空内联值；否则取下一个 argv 元素（不能以 - 开头，
/// 负数端口的例外由调用方处理）。返回 (值, 下一个待处理下标)；无值可
/// 用时返回 (None, 原下标)。
fn consume_value(args: &[String], index: usize, inline: Option<&str>) -> (Option<String>, usize) {
    if let Some(value) = inline.filter(|v| !v.is_empty()) {
        return (Some(value.to_string()), index);
    }
    if let Some(candidate) = args.get(index + 1) {
        if !candidate.starts_with('-') {
            return (Some(candidate.clone()), index + 1);
        }
    }
    (None, index)
}

/// 主解析循环：逐 token 匹配选项表（value_opt! 宏统一处理"取值并写入
/// 字段"），位置参数收集到 positional；--port 做严格数字与 1..=65535
/// 校验并允许下一参数为负数形式的数字；未知/已移除旗标写入
/// removed_flag_errors 而非立即失败。收尾：确定 command 与各命令组的
/// action（tunnel/startup/schedule/session/control），--lan 回填
/// host=0.0.0.0，tunnel 外 --hostname 回填 host，端口回退 OMPCHAMBER_PORT。
pub fn parse_args(args: &[String]) -> Result<Parsed, CliError> {
    let mut options = Options::default();
    let mut removed_flag_errors: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut help_requested = false;
    let mut version_requested = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let Some(token) = split_option_token(arg) else {
            positional.push(arg.clone());
            i += 1;
            continue;
        };
        let name = token.name.as_str();
        let inline = token.inline_value.as_deref();
        let long = token.long;

        // 取选项值（内联或下一个参数）并写入 Options 的对应字段；
        // 无值时保持字段原值不动。
        macro_rules! value_opt {
            ($field:ident) => {{
                let (value, next) = consume_value(args, i, inline);
                i = next;
                if let Some(value) = value {
                    options.$field = Some(value);
                }
            }};
        }

        match name {
            "port" | "p" => {
                let (mut value, mut next) = consume_value(args, i, inline);
                if value.is_none() && inline.is_none() {
                    if let Some(candidate) = args.get(i + 1) {
                        if candidate.len() > 1
                            && candidate.starts_with('-')
                            && candidate[1..].chars().all(|c| c.is_ascii_digit())
                        {
                            value = Some(candidate.clone());
                            next = i + 1;
                        }
                    }
                }
                i = next;
                let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
                    return Err(CliError::new("Missing value for --port.", USAGE_ERROR));
                };
                let trimmed = value.trim();
                if !trimmed.chars().next().is_some_and(|c| c == '-' || c.is_ascii_digit())
                    || !trimmed.chars().skip(1).all(|c| c.is_ascii_digit())
                    || trimmed == "-"
                {
                    return Err(CliError::new(format!("Invalid port value: {value}"), USAGE_ERROR));
                }
                let parsed: i64 = trimmed.parse().map_err(|_| {
                    CliError::new(format!("Invalid port value: {value}"), USAGE_ERROR)
                })?;
                if !(1..=65535).contains(&parsed) {
                    return Err(CliError::new(format!("Invalid port value: {parsed}"), USAGE_ERROR));
                }
                options.port = Some(parsed as u16);
                options.explicit_port = true;
            }
            "host" => {
                let (value, next) = consume_value(args, i, inline);
                i = next;
                let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
                    return Err(CliError::new("Missing value for --host.", USAGE_ERROR));
                };
                options.host = Some(value.trim().to_string());
            }
            "lan" => options.lan = true,
            "ui-password" => {
                let (value, next) = consume_value(args, i, inline);
                i = next;
                options.ui_password = value;
                options.explicit_ui_password = true;
            }
            "provider" => value_opt!(provider),
            "mode" => value_opt!(mode),
            "profile" => value_opt!(profile),
            "name" => value_opt!(name),
            "title" => value_opt!(title),
            "worktree" => value_opt!(worktree),
            "branch" => value_opt!(branch),
            "start-ref" | "base" => value_opt!(start_ref),
            "upstream" => options.set_upstream = Some(true),
            "no-upstream" => options.set_upstream = Some(false),
            "project" => value_opt!(project),
            "dir" | "directory" => value_opt!(directory),
            "task" => value_opt!(task),
            "session" => value_opt!(session),
            "message" => value_opt!(message),
            "prompt" => value_opt!(prompt),
            "model" => value_opt!(model),
            "daily" => value_opt!(daily),
            "weekly" => value_opt!(weekly),
            "once" => value_opt!(once),
            "time" => value_opt!(time),
            "cron" => value_opt!(cron),
            "timezone" => value_opt!(timezone),
            "agent" => value_opt!(agent),
            "variant" => value_opt!(variant),
            "disabled" => options.disabled = true,
            "goal" => options.goal = true,
            "goal-token-budget" => {
                let (value, next) = consume_value(args, i, inline);
                i = next;
                options.goal_token_budget = value;
            }
            "config" => {
                let (value, next) = consume_value(args, i, inline);
                i = next;
                options.config_path = value;
            }
            "token" => value_opt!(token),
            "token-file" => value_opt!(token_file),
            "token-stdin" => options.token_stdin = true,
            "hostname" => value_opt!(hostname),
            "server" | "server-url" => {
                let (value, next) = consume_value(args, i, inline);
                i = next;
                let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
                    return Err(CliError::new("Missing value for --server.", USAGE_ERROR));
                };
                options.server = Some(value.trim().to_string());
            }
            "connect-ttl" => value_opt!(connect_ttl),
            "session-ttl" => value_opt!(session_ttl),
            "json" => options.json = true,
            "all" => options.all = true,
            "last" => options.last = true,
            "last-assistant" => options.last_assistant = true,
            "wait" => options.wait = true,
            "timeout" => value_opt!(timeout),
            "with-status" => options.with_status = true,
            "role" => value_opt!(role),
            "no-follow" => options.follow = Some(false),
            "no-env-snapshot" => options.env_snapshot = Some(false),
            "lines" => {
                let (value, next) = consume_value(args, i, inline);
                i = next;
                if let Some(parsed) = value.and_then(|v| v.trim().parse::<u32>().ok()) {
                    if parsed > 0 {
                        options.lines = Some(parsed);
                    }
                }
            }
            "limit" => {
                let (value, next) = consume_value(args, i, inline);
                i = next;
                let parsed = value
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .ok_or_else(|| CliError::new("Invalid limit value. Provide a positive integer.", USAGE_ERROR))?;
                if parsed < 1 {
                    return Err(CliError::new("Invalid limit value. Provide a positive integer.", USAGE_ERROR));
                }
                options.limit = Some(parsed as u32);
            }
            "relay" => options.relay = true,
            "qr" => {
                options.qr = Some(true);
                options.explicit_qr = true;
            }
            "no-qr" => {
                options.qr = Some(false);
                options.explicit_qr = true;
            }
            "force" => options.force = true,
            "show-secrets" => options.show_secrets = true,
            "dry-run" => options.dry_run = true,
            "plain" => options.plain = true,
            "quiet" | "q" => options.quiet = true,
            "help" | "h" => help_requested = true,
            "version" | "v" => version_requested = true,
            "foreground" | "no-daemon" => options.foreground = true,
            "api-only" => options.api_only = true,
            "suppress-unsafe-port-warning" => options.suppress_unsafe_port_warning = true,
            "suppress-ui-password-warning" => options.suppress_ui_password_warning = true,
            "suppress-quiet-output" => options.suppress_quiet_output = true,
            "suppress-startup-summary" => options.suppress_startup_summary = true,
            "daemon" | "d" => {}
            "try-cf-tunnel" => removed_flag_errors.push(
                "`--try-cf-tunnel` was removed. Use: ompchamber tunnel start --provider cloudflare --mode quick".into(),
            ),
            "tunnel-qr" => removed_flag_errors.push(
                "`--tunnel-qr` was removed. Use: ompchamber tunnel start ... --qr".into(),
            ),
            "tunnel-password-url" => removed_flag_errors.push(
                "`--tunnel-password-url` was removed. Use UI password auth directly after tunnel start.".into(),
            ),
            "tunnel-provider" | "tunnel-mode" | "tunnel-config" | "tunnel-token" | "tunnel-hostname" | "tunnel" => {
                removed_flag_errors.push(format!(
                    "`--{name}` was removed from top-level serve flow. Use: ompchamber tunnel start ..."
                ));
            }
            _ => {
                let prefix = if long { "--" } else { "-" };
                removed_flag_errors.push(format!("Unknown option: {prefix}{name}"));
            }
        }
        i += 1;
    }

    let command = positional
        .first()
        .cloned()
        .unwrap_or_else(|| "serve".to_string());
    let subcommand = if command == "tunnel" {
        Some(
            positional
                .get(1)
                .cloned()
                .unwrap_or_else(|| "help".to_string()),
        )
    } else {
        None
    };
    let tunnel_action = if command == "tunnel" {
        positional.get(2).cloned()
    } else {
        None
    };
    let startup_action = if command == "startup" {
        Some(
            positional
                .get(1)
                .cloned()
                .unwrap_or_else(|| "status".to_string()),
        )
    } else {
        None
    };
    let schedule_action = if command == "schedule" {
        Some(
            positional
                .get(1)
                .cloned()
                .unwrap_or_else(|| "help".to_string()),
        )
    } else {
        None
    };
    let session_action = if command == "session" {
        Some(
            positional
                .get(1)
                .cloned()
                .unwrap_or_else(|| "help".to_string()),
        )
    } else {
        None
    };
    let control_action = if command == "control" {
        Some(
            positional
                .get(1)
                .cloned()
                .unwrap_or_else(|| "help".to_string()),
        )
    } else {
        None
    };

    if options.lan && options.host.is_none() {
        options.host = Some("0.0.0.0".to_string());
    }
    if command != "tunnel" && options.hostname.is_some() && options.host.is_none() {
        options.host = options.hostname.clone();
    }
    if options.port.is_none() {
        options.port = options.effective_port();
    }

    Ok(Parsed {
        command,
        subcommand,
        tunnel_action,
        startup_action,
        schedule_action,
        session_action,
        control_action,
        options,
        removed_flag_errors,
        help_requested,
        version_requested,
        positionals: positional,
    })
}

/// 输出主帮助文本（内嵌 help/main.txt）；保留模板首尾空行以对齐 JS
/// console.log 的可观察输出。
pub fn show_help() -> String {
    // JS console.log(template) — the template's leading and trailing blank
    // lines are part of the observable output.
    format!(
        "\n{}\n\n",
        include_str!("help/main.txt").trim_end_matches('\n')
    )
}

/// 未知命令后的提示：给出最接近的候选（若找到）并引导 `ompchamber --help`。
pub fn show_help_hint(command: &str) {
    if let Some(closest) = find_closest_match(command) {
        eprintln!("Did you mean \"{closest}\"?");
    }
    eprintln!("Run `ompchamber --help` for usage.");
}

/// 与 mod.rs 的同名函数不同，此占位实现恒返回 None（候选表由上层持有）。
fn find_closest_match(_command: &str) -> Option<&'static str> {
    None
}

/// `ompchamber completion <shell>`：输出 bash/zsh/fish 补全脚本
/// （内嵌 help/completion-*.txt 资源）；其它 shell 报 USAGE_ERROR。
pub fn completion_command(_parsed: &Parsed, options: &Options) -> Result<(), super::CliError> {
    let shell = _parsed.positionals.get(1).map(String::as_str).unwrap_or("");
    let script = match shell {
        "bash" => include_str!("help/completion-bash.txt"),
        "zsh" => include_str!("help/completion-zsh.txt"),
        "fish" => include_str!("help/completion-fish.txt"),
        other => {
            return Err(super::CliError::usage(format!(
                "Unknown shell for completion: {other}. Supported: bash, zsh, fish."
            )));
        }
    };
    let _ = options;
    print!("{script}");
    Ok(())
}

/// parse_args 的行为契约：默认命令、端口校验、位置参数命令面、host
/// 别名回填、已移除旗标文案与 help/version/模式旗标。
#[cfg(test)]
mod tests {
    use super::*;

    /// 测试辅助：把 &str 切片转成 Vec<String> 后调用 parse_args。
    fn parse(argv: &[&str]) -> Result<Parsed, CliError> {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        parse_args(&args)
    }

    /// 验证无参数时默认命令为 serve 且端口未定。
    #[test]
    fn defaults_to_serve_command() {
        let parsed = parse(&[]).unwrap();
        assert_eq!(parsed.command, "serve");
        assert!(parsed.options.port.is_none());
    }

    /// 验证 --port/-p/--port= 三种形式等价；非法/越界/缺失值的错误文案与 JS 一致。
    #[test]
    fn port_flag_variants_match_js() {
        assert_eq!(parse(&["--port", "8080"]).unwrap().options.port, Some(8080));
        assert_eq!(parse(&["-p", "8080"]).unwrap().options.port, Some(8080));
        assert_eq!(parse(&["--port=8080"]).unwrap().options.port, Some(8080));
        assert_eq!(
            parse(&["--port", "-1"]).unwrap_err().message,
            "Invalid port value: -1"
        );
        assert_eq!(
            parse(&["--port", "0"]).unwrap_err().message,
            "Invalid port value: 0"
        );
        assert_eq!(
            parse(&["--port", "abc"]).unwrap_err().message,
            "Invalid port value: abc"
        );
        assert_eq!(
            parse(&["--port"]).unwrap_err().message,
            "Missing value for --port."
        );
    }

    /// 验证 tunnel/startup/schedule 的位置参数命令面与选项同时解析。
    #[test]
    fn positional_command_surface() {
        let parsed = parse(&["tunnel", "start", "--provider", "cloudflare"]).unwrap();
        assert_eq!(parsed.command, "tunnel");
        assert_eq!(parsed.subcommand.as_deref(), Some("start"));
        assert_eq!(parsed.options.provider.as_deref(), Some("cloudflare"));
        let parsed = parse(&["startup"]).unwrap();
        assert_eq!(parsed.startup_action.as_deref(), Some("status"));
        let parsed = parse(&["schedule"]).unwrap();
        assert_eq!(parsed.schedule_action.as_deref(), Some("help"));
    }

    /// 验证 --lan 在未显式给 host 时回填 0.0.0.0，显式 host 优先。
    #[test]
    fn lan_implies_bind_all_without_host() {
        let parsed = parse(&["--lan"]).unwrap();
        assert_eq!(parsed.options.host.as_deref(), Some("0.0.0.0"));
        let parsed = parse(&["--lan", "--host", "10.0.0.5"]).unwrap();
        assert_eq!(parsed.options.host.as_deref(), Some("10.0.0.5"));
    }

    /// 验证 tunnel 命令外 --hostname 作为 host 别名生效。
    #[test]
    fn hostname_is_host_alias_outside_tunnel() {
        let parsed = parse(&["--hostname", "192.168.1.9"]).unwrap();
        assert_eq!(parsed.options.host.as_deref(), Some("192.168.1.9"));
    }

    /// 验证 --ui-password 无值时置 explicit 标记但保留 None（待生成）。
    #[test]
    fn ui_password_empty_generates_flag() {
        let parsed = parse(&["--ui-password"]).unwrap();
        assert!(parsed.options.explicit_ui_password);
        assert!(parsed.options.ui_password.is_none());
    }

    /// 验证已移除旗标与未知选项的错误文案逐字匹配 JS。
    #[test]
    fn removed_flags_carry_js_messages() {
        let parsed = parse(&["--try-cf-tunnel"]).unwrap();
        assert_eq!(
            parsed.removed_flag_errors[0],
            "`--try-cf-tunnel` was removed. Use: ompchamber tunnel start --provider cloudflare --mode quick"
        );
        let parsed = parse(&["--nope"]).unwrap();
        assert_eq!(parsed.removed_flag_errors[0], "Unknown option: --nope");
        let parsed = parse(&["-z"]).unwrap();
        assert_eq!(parsed.removed_flag_errors[0], "Unknown option: -z");
    }

    /// 验证以 - 开头的下一参数不被当作选项值吞掉。
    #[test]
    fn values_starting_with_dash_are_not_consumed() {
        let parsed = parse(&["--name", "--force"]).unwrap();
        assert_eq!(parsed.options.name, None);
        assert!(parsed.options.force);
    }

    /// 验证 -h/--version 分别置位对应标记。
    #[test]
    fn help_and_version_flags() {
        let parsed = parse(&["-h"]).unwrap();
        assert!(parsed.help_requested);
        let parsed = parse(&["--version"]).unwrap();
        assert!(parsed.version_requested);
    }

    /// 验证 quiet/json/foreground 三个模式旗标可同时解析。
    #[test]
    fn quiet_json_foreground_modes() {
        let parsed = parse(&["--quiet", "--foreground", "--json"]).unwrap();
        assert!(parsed.options.quiet && parsed.options.foreground && parsed.options.json);
    }
}
