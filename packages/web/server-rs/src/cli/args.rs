//! Port of `bin/lib/cli-args.js` `parseArgs`: the full option table,
//! positional command surface, removed-flag errors, help/version requests.

use super::{CliError, USAGE_ERROR};

pub const DEFAULT_PORT: u16 = 3000;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub port: Option<u16>,
    pub explicit_port: bool,
    pub host: Option<String>,
    pub lan: bool,
    pub ui_password: Option<String>,
    pub explicit_ui_password: bool,
    pub provider: Option<String>,
    pub mode: Option<String>,
    pub profile: Option<String>,
    pub name: Option<String>,
    pub title: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub start_ref: Option<String>,
    pub set_upstream: Option<bool>,
    pub project: Option<String>,
    pub directory: Option<String>,
    pub task: Option<String>,
    pub session: Option<String>,
    pub message: Option<String>,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub daily: Option<String>,
    pub weekly: Option<String>,
    pub once: Option<String>,
    pub time: Option<String>,
    pub cron: Option<String>,
    pub timezone: Option<String>,
    pub agent: Option<String>,
    pub variant: Option<String>,
    pub disabled: bool,
    pub goal: bool,
    pub goal_token_budget: Option<String>,
    pub config_path: Option<String>,
    pub token: Option<String>,
    pub token_file: Option<String>,
    pub token_stdin: bool,
    pub hostname: Option<String>,
    pub server: Option<String>,
    pub connect_ttl: Option<String>,
    pub session_ttl: Option<String>,
    pub json: bool,
    pub all: bool,
    pub last: bool,
    pub last_assistant: bool,
    pub wait: bool,
    pub timeout: Option<String>,
    pub with_status: bool,
    pub role: Option<String>,
    pub follow: Option<bool>,
    pub env_snapshot: Option<bool>,
    pub lines: Option<u32>,
    pub limit: Option<u32>,
    pub relay: bool,
    pub qr: Option<bool>,
    pub explicit_qr: bool,
    pub force: bool,
    pub show_secrets: bool,
    pub dry_run: bool,
    pub plain: bool,
    pub quiet: bool,
    pub foreground: bool,
    pub api_only: bool,
    pub suppress_unsafe_port_warning: bool,
    pub suppress_ui_password_warning: bool,
    pub suppress_quiet_output: bool,
    pub suppress_startup_summary: bool,
}

impl Options {
    fn effective_port(&self) -> Option<u16> {
        self.port.or_else(|| {
            std::env::var("OMPCHAMBER_PORT")
                .ok()
                .and_then(|v| v.trim().parse::<u16>().ok())
        })
    }
}

#[derive(Debug, Clone)]
pub struct Parsed {
    pub command: String,
    pub subcommand: Option<String>,
    pub tunnel_action: Option<String>,
    pub startup_action: Option<String>,
    pub schedule_action: Option<String>,
    pub session_action: Option<String>,
    pub control_action: Option<String>,
    pub options: Options,
    pub removed_flag_errors: Vec<String>,
    pub help_requested: bool,
    pub version_requested: bool,
    pub positionals: Vec<String>,
}

struct Token {
    name: String,
    inline_value: Option<String>,
    long: bool,
}

/// `splitOptionToken`: `--name[=value]` / `-name[=value]`; `--` is positional.
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

pub fn show_help() -> String {
    // JS console.log(template) — the template's leading and trailing blank
    // lines are part of the observable output.
    format!(
        "\n{}\n\n",
        include_str!("help/main.txt").trim_end_matches('\n')
    )
}

pub fn show_help_hint(command: &str) {
    if let Some(closest) = find_closest_match(command) {
        eprintln!("Did you mean \"{closest}\"?");
    }
    eprintln!("Run `ompchamber --help` for usage.");
}

fn find_closest_match(_command: &str) -> Option<&'static str> {
    None
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Result<Parsed, CliError> {
        let args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
        parse_args(&args)
    }

    #[test]
    fn defaults_to_serve_command() {
        let parsed = parse(&[]).unwrap();
        assert_eq!(parsed.command, "serve");
        assert!(parsed.options.port.is_none());
    }

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

    #[test]
    fn lan_implies_bind_all_without_host() {
        let parsed = parse(&["--lan"]).unwrap();
        assert_eq!(parsed.options.host.as_deref(), Some("0.0.0.0"));
        let parsed = parse(&["--lan", "--host", "10.0.0.5"]).unwrap();
        assert_eq!(parsed.options.host.as_deref(), Some("10.0.0.5"));
    }

    #[test]
    fn hostname_is_host_alias_outside_tunnel() {
        let parsed = parse(&["--hostname", "192.168.1.9"]).unwrap();
        assert_eq!(parsed.options.host.as_deref(), Some("192.168.1.9"));
    }

    #[test]
    fn ui_password_empty_generates_flag() {
        let parsed = parse(&["--ui-password"]).unwrap();
        assert!(parsed.options.explicit_ui_password);
        assert!(parsed.options.ui_password.is_none());
    }

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

    #[test]
    fn values_starting_with_dash_are_not_consumed() {
        let parsed = parse(&["--name", "--force"]).unwrap();
        assert_eq!(parsed.options.name, None);
        assert!(parsed.options.force);
    }

    #[test]
    fn help_and_version_flags() {
        let parsed = parse(&["-h"]).unwrap();
        assert!(parsed.help_requested);
        let parsed = parse(&["--version"]).unwrap();
        assert!(parsed.version_requested);
    }

    #[test]
    fn quiet_json_foreground_modes() {
        let parsed = parse(&["--quiet", "--foreground", "--json"]).unwrap();
        assert!(parsed.options.quiet && parsed.options.foreground && parsed.options.json);
    }
}
