//! Port of `bin/lib/cli-startup.js` (+ `bin/lib/commands-startup.js`):
//! `ompchamber startup status|enable|disable` — native user-startup
//! integration via launchd (macOS), systemd user units (Linux), and
//! schtasks (Windows).
//!
//! JS→Rust notes:
//! - `process.execPath` maps to the resolved current executable;
//!   `resolveCliEntrypoint()` collapses into it (the Rust binary is both
//!   runtime and CLI — there is no separate cli.js to exec), so the macOS
//!   wrapper runs `exec <exe> serve --foreground --port N ...`.
//! - `spawnSync` maps to [`StartupCommandRunner`] (std::process::Command),
//!   injectable so tests can fake launchctl/systemctl/schtasks.
//! - Platform dispatch is a parameter ([`StartupPlatform`]) so plist/unit
//!   generation is testable on any host.
//! - Env snapshots: the CLI's current environment is filtered through
//!   `shouldPersistStartupEnv` and written into the service (plist
//!   EnvironmentVariables / startup.env for systemd+Windows);
//!   `--no-env-snapshot` skips the snapshot exactly like the JS.
//! - clack intro/log/outro render as plain lines (title, message,
//!   outro) per the serve port's human-output convention.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::args::{DEFAULT_PORT, Options, Parsed};
use super::{CliError, GENERAL_ERROR, OutputMode, USAGE_ERROR};

const STARTUP_SERVICE_ID: &str = "dev.ompchamber.web";

/// `getStartupServicePaths` platform discriminator (process.platform).
/// The non-host variants are constructed by tests and by `current_platform`
/// on their own target platforms; kept unconditionally so the service-path,
/// plist/unit builders, and enable/disable flows compile everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum StartupPlatform {
    Macos,
    Linux,
    Windows,
    Other,
}

pub(crate) fn current_platform() -> StartupPlatform {
    #[cfg(target_os = "macos")]
    {
        StartupPlatform::Macos
    }
    #[cfg(target_os = "linux")]
    {
        StartupPlatform::Linux
    }
    #[cfg(target_os = "windows")]
    {
        StartupPlatform::Windows
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        StartupPlatform::Other
    }
}

pub(crate) struct StartupServicePaths {
    pub platform: String,
    pub service_path: Option<PathBuf>,
    pub kind: StartupPlatform,
}

fn get_startup_service_paths(platform: StartupPlatform, home: &Path) -> StartupServicePaths {
    let kind = platform;
    let (name, path) = match platform {
        StartupPlatform::Macos => (
            "macos",
            Some(
                home.join("Library")
                    .join("LaunchAgents")
                    .join(format!("{STARTUP_SERVICE_ID}.plist")),
            ),
        ),
        StartupPlatform::Linux => (
            "linux",
            Some(
                home.join(".config")
                    .join("systemd")
                    .join("user")
                    .join("ompchamber.service"),
            ),
        ),
        // JS keeps the task name (not a filesystem path) as servicePath here.
        StartupPlatform::Windows => ("windows", Some(PathBuf::from(STARTUP_SERVICE_ID))),
        StartupPlatform::Other => (std::env::consts::OS, None),
    };
    StartupServicePaths {
        platform: name.to_string(),
        service_path: path,
        kind,
    }
}

// ── quoting/escaping (cli-startup.js) ─────────────────────────────────

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn systemd_escape_arg(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn startup_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn systemd_unit_path(value: &str) -> String {
    value.replace('\\', "\\\\").replace(' ', "\\x20")
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn startup_env_file_quote(value: &str) -> String {
    startup_shell_quote(value)
}

fn systemd_env_file_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$")
    )
}

// ── env snapshot ───────────────────────────────────────────────────────

/// The startup-relevant slice of CLI `Options`.
pub(crate) struct StartupEnvOptions<'a> {
    pub host: Option<&'a str>,
    pub ui_password: Option<&'a str>,
    pub api_only: bool,
    /// JS `options.envSnapshot !== false`.
    pub env_snapshot: bool,
}

fn startup_env_options(options: &Options) -> StartupEnvOptions<'_> {
    StartupEnvOptions {
        host: options.host.as_deref().filter(|h| !h.is_empty()),
        ui_password: options
            .ui_password
            .as_deref()
            .filter(|p| !p.trim().is_empty()),
        api_only: options.api_only,
        env_snapshot: options.env_snapshot != Some(false),
    }
}

/// `shouldPersistStartupEnv`: valid identifier key, single-line string
/// value, not a shell/session implementation detail.
fn should_persist_startup_env(key: &str, value: &str) -> bool {
    let mut chars = key.chars();
    let valid_key = match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    };
    if !valid_key {
        return false;
    }
    if value.contains('\r') || value.contains('\n') {
        return false;
    }
    // These are shell/session implementation details, not app configuration.
    const VOLATILE_KEYS: [&str; 27] = [
        "_",
        "BASH_ENV",
        "COLUMNS",
        "CONDA_DEFAULT_ENV",
        "CONDA_PREFIX",
        "CONDA_PROMPT_MODIFIER",
        "CONDA_SHLVL",
        "ENV",
        "HISTFILE",
        "HISTFILESIZE",
        "HISTSIZE",
        "LINES",
        "OLDPWD",
        "PROMPT",
        "PROMPT_COMMAND",
        "PS1",
        "PS2",
        "PS3",
        "PS4",
        "PWD",
        "PYENV_VERSION",
        "SHLVL",
        "TERM",
        "TERM_PROGRAM",
        "TERM_PROGRAM_VERSION",
        "TTY",
        "VIRTUAL_ENV",
    ];
    // (kept as a list; `VIRTUAL_ENV_PROMPT` appended below)
    const VOLATILE_KEYS_EXTRA: [&str; 1] = ["VIRTUAL_ENV_PROMPT"];
    !VOLATILE_KEYS.contains(&key) && !VOLATILE_KEYS_EXTRA.contains(&key)
}

fn path_resolve(value: &Path) -> PathBuf {
    crate::package_manager::paths::resolve_lexically(&if value.is_absolute() {
        value.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(value)
    })
}

/// `collectStartupEnv` over an injected environment (tests pass a fixed
/// one; production passes `std::env::vars`).
pub(crate) fn collect_startup_env(
    env_vars: &BTreeMap<String, String>,
    options: &StartupEnvOptions<'_>,
    bun_from_path: Option<&Path>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if options.env_snapshot {
        for (key, value) in env_vars {
            if should_persist_startup_env(key, value) {
                env.insert(key.clone(), value.clone());
            }
        }
        // JS: OMPCHAMBER_OMP_HOST_RUNTIME env override, else PATH lookup for bun.
        let omp_host_runtime = env_vars
            .get("OMPCHAMBER_OMP_HOST_RUNTIME")
            .cloned()
            .or_else(|| bun_from_path.map(|path| path.display().to_string()));
        if let Some(runtime) = omp_host_runtime {
            if !runtime.trim().is_empty() {
                env.insert(
                    "OMPCHAMBER_OMP_HOST_RUNTIME".to_string(),
                    runtime.trim().to_string(),
                );
            }
        }
    }
    if let Some(password) = options.ui_password {
        env.insert("OMPCHAMBER_UI_PASSWORD".to_string(), password.to_string());
    }
    if options.api_only {
        env.insert("OMPCHAMBER_API_ONLY".to_string(), "true".to_string());
    }
    if let Some(data_dir) = env_vars
        .get("OMPCHAMBER_DATA_DIR")
        .filter(|value| !value.trim().is_empty())
    {
        env.insert(
            "OMPCHAMBER_DATA_DIR".to_string(),
            path_resolve(Path::new(data_dir.trim()))
                .display()
                .to_string(),
        );
    }
    env
}

pub(crate) fn startup_env_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("startup.env")
}

fn macos_startup_wrapper_path(data_dir: &Path) -> PathBuf {
    data_dir.join("bin").join("OMPChamber")
}

fn windows_startup_wrapper_path(data_dir: &Path) -> PathBuf {
    data_dir.join("bin").join("OpenChamber.ps1")
}

fn write_file_mode(
    path: &Path,
    content: &str,
    dir_mode: u32,
    file_mode: u32,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_dir_mode(parent, dir_mode);
    }
    std::fs::write(path, content)?;
    set_file_mode(path, file_mode);
    Ok(())
}

#[cfg(unix)]
fn set_dir_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_dir_mode(_path: &Path, _mode: u32) {}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _mode: u32) {}

/// `writeStartupEnvFile`: `KEY=quote(value)` lines, 0600, parent 0700.
fn write_startup_env_file(
    env: &BTreeMap<String, String>,
    env_file: &Path,
    quote: fn(&str) -> String,
) -> PathBuf {
    let lines: Vec<String> = env
        .iter()
        .map(|(key, value)| format!("{key}={}", quote(value)))
        .collect();
    let content = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    let _ = write_file_mode(env_file, &content, 0o700, 0o600);
    env_file.to_path_buf()
}

fn remove_startup_env_file(data_dir: &Path) {
    let _ = std::fs::remove_file(startup_env_file_path(data_dir));
}

/// `buildStartupArgs` — the Rust binary is the entrypoint, so the argv is
/// the serve invocation itself.
fn build_startup_args(port: u16, host: Option<&str>, api_only: bool) -> Vec<String> {
    let mut args = vec![
        "serve".to_string(),
        "--foreground".to_string(),
        "--port".to_string(),
        port.to_string(),
    ];
    if let Some(host) = host.filter(|h| !h.is_empty()) {
        args.push("--host".to_string());
        args.push(host.to_string());
    }
    if api_only {
        args.push("--api-only".to_string());
    }
    args
}

fn macos_wrapper_content(exe: &Path, args: &[String]) -> String {
    let quoted = args
        .iter()
        .map(|arg| startup_shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "#!/bin/sh\nexec {} {}\n",
        startup_shell_quote(&exe.display().to_string()),
        quoted
    )
}

fn write_macos_startup_wrapper(exe: &Path, args: &[String], wrapper: &Path) -> PathBuf {
    let _ = write_file_mode(wrapper, &macos_wrapper_content(exe, args), 0o700, 0o700);
    wrapper.to_path_buf()
}

fn windows_wrapper_content(exe: &Path, args: &[String], env_file: &Path) -> String {
    let startup_args = args
        .iter()
        .map(|arg| powershell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    [
        format!("$envFile={}", powershell_quote(&env_file.display().to_string())),
        r#"if (Test-Path $envFile) { Get-Content $envFile | ForEach-Object { if ($_ -match '^([^=]+)=(.*)$') { $v=$matches[2]; if ($v.StartsWith("'") -and $v.EndsWith("'")) { $v=$v.Substring(1,$v.Length-2).Replace("'\\''","'") }; [Environment]::SetEnvironmentVariable($matches[1], $v, 'Process') } } }"#.to_string(),
        format!("& {} {}", powershell_quote(&exe.display().to_string()), startup_args),
    ]
    .join("; ")
}

fn write_windows_startup_wrapper(
    exe: &Path,
    args: &[String],
    env_file: &Path,
    wrapper: &Path,
) -> PathBuf {
    let _ = write_file_mode(
        wrapper,
        &windows_wrapper_content(exe, args, env_file),
        0o700,
        0o700,
    );
    wrapper.to_path_buf()
}

fn build_windows_startup_task_command(wrapper_path: &Path) -> String {
    format!(
        "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
        wrapper_path.display()
    )
}

/// `buildMacosLaunchAgent` — writes the wrapper, returns the plist body.
fn build_macos_launch_agent(env: &BTreeMap<String, String>, home: &Path, wrapper: &Path) -> String {
    let arg_xml = std::iter::once(wrapper.display().to_string())
        .map(|arg| format!("    <string>{}</string>", escape_xml(&arg)))
        .collect::<Vec<_>>()
        .join("\n");
    let env_xml = if env.is_empty() {
        String::new()
    } else {
        let entries = env
            .iter()
            .map(|(key, value)| {
                format!(
                    "    <key>{}</key>\n    <string>{}</string>",
                    escape_xml(key),
                    escape_xml(value)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!("  <key>EnvironmentVariables</key>\n  <dict>\n{entries}\n  </dict>\n")
    };
    let log_dir = home.join("Library").join("Logs").join("OMPChamber");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{STARTUP_SERVICE_ID}</string>
  <key>ProgramArguments</key>
  <array>
{arg_xml}
  </array>
{env_xml}  <key>ProcessType</key>
  <string>Background</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>WorkingDirectory</key>
  <string>{working_dir}</string>
  <key>StandardOutPath</key>
  <string>{stdout_path}</string>
  <key>StandardErrorPath</key>
  <string>{stderr_path}</string>
</dict>
</plist>
"#,
        working_dir = escape_xml(&home.display().to_string()),
        stdout_path = escape_xml(&log_dir.join("startup.log").display().to_string()),
        stderr_path = escape_xml(&log_dir.join("startup.err.log").display().to_string()),
    )
}

/// `buildSystemdUserService`.
fn build_systemd_user_service(exe: &Path, args: &[String], env_file: &Path, home: &Path) -> String {
    let quoted_args = args
        .iter()
        .map(|arg| format!("\"{}\"", systemd_escape_arg(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\nDescription=OMPChamber web server\nAfter=network-online.target\n\n[Service]\nType=simple\nEnvironmentFile=-{env_file}\nExecStart=\"{exe}\" {args}\nWorkingDirectory={working_dir}\nRestart=always\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        env_file = systemd_escape_arg(&env_file.display().to_string()),
        exe = systemd_escape_arg(&exe.display().to_string()),
        args = quoted_args,
        working_dir = systemd_unit_path(&home.display().to_string()),
    )
}

// ── command runner (spawnSync) ────────────────────────────────────────

#[derive(Clone, Debug)]
pub(crate) struct StartupCommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub(crate) trait StartupCommandRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<StartupCommandOutput>;
}

struct SystemStartupRunner;

impl StartupCommandRunner for SystemStartupRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<StartupCommandOutput> {
        let output = std::process::Command::new(program).args(args).output()?;
        Ok(StartupCommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// `runStartupCommand`: spawn failure and non-zero exit (unless
/// `allow_failure`) become a GENERAL_ERROR CliError.
fn run_startup_command(
    runner: &dyn StartupCommandRunner,
    program: &str,
    args: &[&str],
    allow_failure: bool,
) -> Result<StartupCommandOutput, CliError> {
    match runner.run(program, args) {
        // JS throws the spawn error itself (plain Error → exit 1).
        Err(error) => Err(CliError::new(error.to_string(), GENERAL_ERROR)),
        Ok(output) if output.status != 0 && !allow_failure => {
            let detail = if !output.stderr.trim().is_empty() {
                output.stderr.trim().to_string()
            } else {
                output.stdout.trim().to_string()
            };
            let suffix = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            Err(CliError::new(
                format!("{} {} failed{suffix}", program, args.join(" ")),
                GENERAL_ERROR,
            ))
        }
        Ok(output) => Ok(output),
    }
}

/// `process.getuid()` — resolved through `id -u` (no libc dependency).
fn current_uid() -> u32 {
    if let Ok(output) = std::process::Command::new("id").arg("-u").output() {
        if let Ok(parsed) = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
        {
            return parsed;
        }
    }
    std::env::var("UID")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

// ── status / enable / disable ─────────────────────────────────────────

/// `getStartupStatus` result (JSON shape mirrors the JS exactly, including
/// which keys are present).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct StartupResult {
    pub action: String,
    pub supported: bool,
    pub platform: String,
    pub enabled: bool,
    /// `Some(_)` = the `active` key is emitted (value may be JSON null);
    /// `None` = the key is absent (unsupported platform).
    pub active: Option<bool>,
    pub active_state: Option<String>,
    pub service_path: Option<String>,
}

fn get_startup_status(
    runner: &dyn StartupCommandRunner,
    platform: StartupPlatform,
    home: &Path,
) -> StartupResult {
    let paths = get_startup_service_paths(platform, home);
    let platform = paths.platform.clone();
    let unsupported = |action: &str| StartupResult {
        action: action.to_string(),
        supported: false,
        platform: platform.clone(),
        enabled: false,
        active: None,
        active_state: None,
        service_path: None,
    };
    let Some(service_path) = paths.service_path else {
        return unsupported("status");
    };

    let finalize = |action: &str,
                    enabled: bool,
                    active: Option<bool>,
                    active_state: Option<String>| StartupResult {
        action: action.to_string(),
        supported: true,
        platform: platform.clone(),
        enabled,
        active,
        active_state,
        service_path: Some(service_path.display().to_string()),
    };

    match paths.kind {
        StartupPlatform::Windows => {
            let result = run_startup_command(
                runner,
                "schtasks.exe",
                &["/Query", "/TN", STARTUP_SERVICE_ID],
                true,
            );
            let enabled = matches!(result, Ok(output) if output.status == 0);
            finalize("status", enabled, None, None)
        }
        StartupPlatform::Linux => {
            let enabled_result = run_startup_command(
                runner,
                "systemctl",
                &["--user", "is-enabled", "ompchamber.service"],
                true,
            );
            let active_result = run_startup_command(
                runner,
                "systemctl",
                &["--user", "is-active", "ompchamber.service"],
                true,
            );
            let active_state = active_result
                .map(|output| {
                    let trimmed = output.stdout.trim().to_string();
                    if trimmed.is_empty() {
                        "inactive".to_string()
                    } else {
                        trimmed
                    }
                })
                .unwrap_or_else(|_| "inactive".to_string());
            let enabled =
                matches!(enabled_result, Ok(output) if output.status == 0) || service_path.exists();
            finalize(
                "status",
                enabled,
                Some(active_state == "active"),
                Some(active_state),
            )
        }
        _ => finalize("status", service_path.exists(), None, None),
    }
}

/// Everything `enableStartupService` needs beyond the flag slice.
pub(crate) struct StartupInstall<'a> {
    pub home: &'a Path,
    pub data_dir: &'a Path,
    pub exe: &'a Path,
    pub port: u16,
    pub uid: u32,
}

fn enable_startup_service(
    runner: &dyn StartupCommandRunner,
    platform: StartupPlatform,
    install: &StartupInstall<'_>,
    env: &BTreeMap<String, String>,
    options: &StartupEnvOptions<'_>,
) -> Result<StartupResult, CliError> {
    let paths = get_startup_service_paths(platform, install.home);
    let Some(service_path) = paths.service_path else {
        return Err(CliError::usage(format!(
            "Startup integration is not supported on {}.",
            paths.platform
        )));
    };
    let args = build_startup_args(install.port, options.host, options.api_only);
    match paths.kind {
        StartupPlatform::Macos => {
            remove_startup_env_file(install.data_dir);
            let wrapper = write_macos_startup_wrapper(
                install.exe,
                &args,
                &macos_startup_wrapper_path(install.data_dir),
            );
            let plist = build_macos_launch_agent(env, install.home, &wrapper);
            write_file_mode(&service_path, &plist, 0o700, 0o600)
                .map_err(|error| CliError::new(error.to_string(), GENERAL_ERROR))?;
            let _ = write_file_mode(
                &install.home.join("Library").join("Logs").join("OMPChamber"),
                "",
                0o700,
                0o600,
            );
            let gui = format!("gui/{}", install.uid);
            run_startup_command(
                runner,
                "/bin/launchctl",
                &["bootout", &gui, &service_path.display().to_string()],
                true,
            )?;
            run_startup_command(
                runner,
                "/bin/launchctl",
                &["bootstrap", &gui, &service_path.display().to_string()],
                false,
            )?;
            let target = format!("{gui}/{STARTUP_SERVICE_ID}");
            run_startup_command(
                runner,
                "/bin/launchctl",
                &["kickstart", "-k", &target],
                true,
            )?;
        }
        StartupPlatform::Linux => {
            write_startup_env_file(
                env,
                &startup_env_file_path(install.data_dir),
                systemd_env_file_quote,
            );
            let unit = build_systemd_user_service(
                install.exe,
                &args,
                &startup_env_file_path(install.data_dir),
                install.home,
            );
            write_file_mode(&service_path, &unit, 0o700, 0o600)
                .map_err(|error| CliError::new(error.to_string(), GENERAL_ERROR))?;
            run_startup_command(runner, "systemctl", &["--user", "daemon-reload"], false)?;
            run_startup_command(
                runner,
                "systemctl",
                &["--user", "enable", "--now", "ompchamber.service"],
                false,
            )?;
        }
        StartupPlatform::Windows => {
            write_startup_env_file(
                env,
                &startup_env_file_path(install.data_dir),
                startup_env_file_quote,
            );
            let wrapper = write_windows_startup_wrapper(
                install.exe,
                &args,
                &startup_env_file_path(install.data_dir),
                &windows_startup_wrapper_path(install.data_dir),
            );
            let task_command = build_windows_startup_task_command(&wrapper);
            run_startup_command(
                runner,
                "schtasks.exe",
                &[
                    "/Create",
                    "/TN",
                    STARTUP_SERVICE_ID,
                    "/SC",
                    "ONLOGON",
                    "/RL",
                    "LIMITED",
                    "/F",
                    "/TR",
                    &task_command,
                ],
                false,
            )?;
            run_startup_command(
                runner,
                "schtasks.exe",
                &["/Run", "/TN", STARTUP_SERVICE_ID],
                true,
            )?;
        }
        StartupPlatform::Other => {}
    }
    Ok(get_startup_status(runner, platform, install.home))
}

fn disable_startup_service(
    runner: &dyn StartupCommandRunner,
    platform: StartupPlatform,
    home: &Path,
    data_dir: &Path,
) -> Result<StartupResult, CliError> {
    let paths = get_startup_service_paths(platform, home);
    let Some(service_path) = paths.service_path else {
        return Err(CliError::usage(format!(
            "Startup integration is not supported on {}.",
            paths.platform
        )));
    };
    match paths.kind {
        StartupPlatform::Macos => {
            let gui = format!("gui/{}", current_uid());
            run_startup_command(
                runner,
                "/bin/launchctl",
                &["bootout", &gui, &service_path.display().to_string()],
                true,
            )?;
            let _ = std::fs::remove_file(&service_path);
        }
        StartupPlatform::Linux => {
            run_startup_command(
                runner,
                "systemctl",
                &["--user", "disable", "--now", "ompchamber.service"],
                true,
            )?;
            let _ = std::fs::remove_file(&service_path);
            run_startup_command(runner, "systemctl", &["--user", "daemon-reload"], true)?;
        }
        StartupPlatform::Windows => {
            run_startup_command(
                runner,
                "schtasks.exe",
                &["/End", "/TN", STARTUP_SERVICE_ID],
                true,
            )?;
            run_startup_command(
                runner,
                "schtasks.exe",
                &["/Delete", "/TN", STARTUP_SERVICE_ID, "/F"],
                true,
            )?;
            let _ = std::fs::remove_file(windows_startup_wrapper_path(data_dir));
            remove_startup_env_file(data_dir);
        }
        StartupPlatform::Other => {}
    }
    Ok(get_startup_status(runner, platform, home))
}

// ── output rendering (commands-startup.js) ────────────────────────────

fn startup_result_json(result: &StartupResult) -> serde_json::Value {
    let mut value = serde_json::json!({
        "action": result.action,
        "supported": result.supported,
        "platform": result.platform,
        "enabled": result.enabled,
    });
    if let Some(active) = result.active {
        value["active"] = serde_json::json!(active);
    }
    if let Some(state) = &result.active_state {
        value["activeState"] = serde_json::json!(state);
    }
    match &result.service_path {
        Some(path) => value["servicePath"] = serde_json::json!(path),
        None => value["servicePath"] = serde_json::Value::Null,
    }
    value
}

fn startup_quiet_line(result: &StartupResult) -> String {
    format!(
        "startup {} platform:{} supported:{}{}",
        if result.enabled {
            "enabled"
        } else {
            "disabled"
        },
        result.platform,
        if result.supported { "yes" } else { "no" },
        result
            .service_path
            .as_deref()
            .filter(|path| !path.is_empty())
            .map(|path| format!(" path:{path}"))
            .unwrap_or_default()
    )
}

/// clack human output as plain lines: title, status lines, outro.
fn startup_human_lines(result: &StartupResult) -> Vec<String> {
    let mut lines = vec!["OMPChamber Startup".to_string()];
    let push_status = |message: String, detail: Option<String>, lines: &mut Vec<String>| {
        lines.push(message);
        if let Some(detail) = detail {
            lines.push(format!("  {detail}"));
        }
    };
    push_status(
        format!(
            "startup {}",
            if result.enabled {
                "enabled"
            } else {
                "disabled"
            }
        ),
        result.service_path.clone().filter(|path| !path.is_empty()),
        &mut lines,
    );
    if let Some(state) = &result.active_state {
        // logStatus(active ? 'success' : state === 'failed' ? 'error' : 'warning', `service ${state}`)
        lines.push(format!("service {state}"));
    }
    if result.action == "enable" {
        push_status(
            "service command".to_string(),
            Some("ompchamber serve --foreground".to_string()),
            &mut lines,
        );
    }
    lines.push(if result.action == "status" {
        "status complete".to_string()
    } else {
        format!("{} complete", result.action)
    });
    lines
}

/// commands-startup.js post-action validation.
fn validate_startup_result(result: &StartupResult) -> Result<(), CliError> {
    if !result.supported {
        return Err(CliError::usage(format!(
            "Startup integration is not supported on {}.",
            result.platform
        )));
    }
    if result.action == "enable" && result.active_state.as_deref() == Some("failed") {
        return Err(CliError::new(
            "Startup service was installed but failed to start. Run `journalctl --user -u ompchamber.service -n 80 --no-pager` for details.",
            GENERAL_ERROR,
        ));
    }
    Ok(())
}

/// `commands-startup.js` `startupCommand` (the `ompchamber startup` entry).
pub(crate) fn startup_command_with(
    runner: &dyn StartupCommandRunner,
    platform: StartupPlatform,
    home: &Path,
    data_dir: &Path,
    exe: &Path,
    uid: u32,
    action: &str,
    env_vars: &BTreeMap<String, String>,
    bun_from_path: Option<&Path>,
    options: &Options,
    mode: OutputMode,
) -> Result<(), CliError> {
    let normalized = action.trim().to_lowercase();
    if !matches!(normalized.as_str(), "status" | "enable" | "disable") {
        return Err(CliError::new(
            format!("Unknown startup subcommand '{action}'. Use 'ompchamber startup --help'."),
            USAGE_ERROR,
        ));
    }

    let env_options = startup_env_options(options);
    let result = if normalized == "enable" {
        let install = StartupInstall {
            home,
            data_dir,
            exe,
            port: options.port.unwrap_or(DEFAULT_PORT),
            uid,
        };
        let env = collect_startup_env(env_vars, &env_options, bun_from_path);
        enable_startup_service(runner, platform, &install, &env, &env_options)?
    } else if normalized == "disable" {
        disable_startup_service(runner, platform, home, data_dir)?
    } else {
        get_startup_status(runner, platform, home)
    }
    .with_action(&normalized);

    validate_startup_result(&result)?;

    match mode {
        OutputMode::Json => super::print_json(&startup_result_json(&result)),
        OutputMode::Quiet => println!("{}", startup_quiet_line(&result)),
        OutputMode::Human => {
            for line in startup_human_lines(&result) {
                println!("{line}");
            }
        }
    }
    Ok(())
}

impl StartupResult {
    fn with_action(mut self, action: &str) -> Self {
        self.action = action.to_string();
        self
    }
}

pub fn help_text() -> &'static str {
    "\n OMPChamber Startup Commands\n\nUSAGE:\n  ompchamber startup <SUBCOMMAND> [OPTIONS]\n\nSUBCOMMANDS:\n  status      Show startup integration status\n  enable      Install and start native user startup integration\n  disable     Stop and remove native user startup integration\n\nOPTIONS:\n  -p, --port              Web server port used by startup service\n  --host                  Bind address used by startup service\n  --ui-password           Protect browser UI with single password\n  --api-only              Start API routes only, without serving browser UI assets\n  --no-env-snapshot       Do not save current environment for startup service\n  --json                  Output machine-readable JSON\n  -q, --quiet             Suppress non-essential output\n\nEXAMPLES:\n  ompchamber startup enable\n  ompchamber startup enable --port 3000\n  ompchamber startup enable --port 3000 --api-only --host 0.0.0.0\n  ompchamber startup status --json\n\n"
}

/// `ompchamber startup <action>` — dispatch entry (mod.rs contract).
pub fn command(parsed: &Parsed, options: Options) -> Result<(), CliError> {
    let mode = OutputMode::from_options(&options);
    let action = parsed
        .startup_action
        .clone()
        .unwrap_or_else(|| "status".to_string());
    let home = crate::config::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let data_dir = super::paths::data_dir();
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ompchamber"));
    let env_vars: BTreeMap<String, String> = std::env::vars().collect();
    let bun_from_path = search_path_for("bun");
    startup_command_with(
        &SystemStartupRunner,
        current_platform(),
        &home,
        &data_dir,
        &exe,
        current_uid(),
        &action,
        &env_vars,
        bun_from_path.as_deref(),
        &options,
        mode,
    )
}

/// `searchPathFor` (cli-executables.js): first executable match on PATH.
fn search_path_for(command: &str) -> Option<PathBuf> {
    let path_value = std::env::var("PATH").unwrap_or_default();
    let delimiter = if cfg!(windows) { ';' } else { ':' };
    for dir in path_value
        .split(delimiter)
        .filter(|segment| !segment.is_empty())
    {
        let candidate = Path::new(dir).join(command);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn temp_dir(tag: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-startup-{tag}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[derive(Clone, Default)]
    struct FakeRunner {
        calls: Arc<Mutex<Vec<(String, Vec<String>)>>>,
        results: Arc<Mutex<Vec<StartupCommandOutput>>>,
    }

    impl FakeRunner {
        fn with_results(results: Vec<StartupCommandOutput>) -> Self {
            Self {
                results: Arc::new(Mutex::new(results)),
                ..Default::default()
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    impl StartupCommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<StartupCommandOutput> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((
                    program.to_string(),
                    args.iter().map(|arg| arg.to_string()).collect(),
                ));
            let mut results = self
                .results
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if results.is_empty() {
                return Ok(StartupCommandOutput {
                    status: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            let output = results.remove(0);
            Ok(output)
        }
    }

    fn ok(stdout: &str) -> StartupCommandOutput {
        StartupCommandOutput {
            status: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn env_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn install<'a>(home: &'a Path, data_dir: &'a Path, exe: &'a Path) -> StartupInstall<'a> {
        StartupInstall {
            home,
            data_dir,
            exe,
            port: 3456,
            uid: 501,
        }
    }

    #[test]
    fn should_persist_startup_env_filters_volatile_and_multiline() {
        assert!(should_persist_startup_env("FOO", "bar"));
        assert!(!should_persist_startup_env("PWD", "/somewhere"));
        assert!(!should_persist_startup_env("SHLVL", "2"));
        assert!(!should_persist_startup_env("1BAD", "x"));
        assert!(!should_persist_startup_env("BAD-KEY", "x"));
        assert!(!should_persist_startup_env("MULTI", "a\nb"));
        assert!(!should_persist_startup_env("VIRTUAL_ENV_PROMPT", "(venv)"));
    }

    #[test]
    fn collect_startup_env_snapshots_filters_and_flags() {
        let env = env_map(&[
            ("FOO", "bar"),
            ("PWD", "/volatile"),
            ("OMPCHAMBER_DATA_DIR", "relative/dir"),
        ]);
        let options = StartupEnvOptions {
            host: Some("0.0.0.0"),
            ui_password: Some("sekret"),
            api_only: true,
            env_snapshot: true,
        };
        let collected = collect_startup_env(&env, &options, Some(Path::new("/opt/bun/bin/bun")));
        assert_eq!(collected.get("FOO").map(String::as_str), Some("bar"));
        assert!(!collected.contains_key("PWD"));
        assert_eq!(
            collected
                .get("OMPCHAMBER_OMP_HOST_RUNTIME")
                .map(String::as_str),
            Some("/opt/bun/bin/bun")
        );
        assert_eq!(
            collected.get("OMPCHAMBER_UI_PASSWORD").map(String::as_str),
            Some("sekret")
        );
        assert_eq!(
            collected.get("OMPCHAMBER_API_ONLY").map(String::as_str),
            Some("true")
        );
        let data_dir = collected
            .get("OMPCHAMBER_DATA_DIR")
            .expect("data dir snapshot");
        assert!(
            data_dir.starts_with('/'),
            "path.resolve makes it absolute: {data_dir}"
        );
        assert!(data_dir.ends_with("relative/dir"));
    }

    #[test]
    fn collect_startup_env_no_snapshot_still_carries_flags() {
        let env = env_map(&[("FOO", "bar")]);
        let options = StartupEnvOptions {
            host: None,
            ui_password: None,
            api_only: true,
            env_snapshot: false,
        };
        let collected = collect_startup_env(&env, &options, Some(Path::new("/bun")));
        assert!(!collected.contains_key("FOO"));
        assert!(!collected.contains_key("OMPCHAMBER_OMP_HOST_RUNTIME"));
        assert_eq!(
            collected.get("OMPCHAMBER_API_ONLY").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn macos_plist_snapshots_env_and_flags() {
        let home = temp_dir("plist-home");
        let data_dir = temp_dir("plist-data");
        let exe = PathBuf::from("/opt/ompchamber/bin/ompchamber");
        let env = env_map(&[("FOO", "bar"), ("OMPCHAMBER_UI_PASSWORD", "sekret")]);
        let args = build_startup_args(3456, Some("0.0.0.0"), true);
        let wrapper = macos_startup_wrapper_path(&data_dir);
        let plist = build_macos_launch_agent(&env, &home, &wrapper);
        assert!(plist.contains("<string>dev.ompchamber.web</string>"));
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<string>sekret</string>"));
        assert!(plist.contains(&format!("<string>{}</string>", wrapper.display())));
        assert!(plist.contains(&format!(
            "<string>{}</string>",
            home.join("Library").join("Logs").join("OMPChamber").join("startup.log").display()
        )));
        // The wrapper script itself execs the binary with the serve argv.
        let wrapper_body = macos_wrapper_content(&exe, &args);
        assert_eq!(
            wrapper_body,
            "#!/bin/sh\nexec '/opt/ompchamber/bin/ompchamber' 'serve' '--foreground' '--port' '3456' '--host' '0.0.0.0' '--api-only'\n"
        );
    }

    #[test]
    fn macos_plist_omits_env_without_snapshot() {
        let home = temp_dir("plist-nosnap-home");
        let data_dir = temp_dir("plist-nosnap-data");
        let env = BTreeMap::new();
        let plist = build_macos_launch_agent(&env, &home, &macos_startup_wrapper_path(&data_dir));
        assert!(!plist.contains("EnvironmentVariables"));
        assert!(plist.contains("<key>ProcessType</key>"));
    }

    #[test]
    fn systemd_unit_and_env_file_generation() {
        let home = temp_dir("unit-home");
        let data_dir = temp_dir("unit-data");
        let exe = PathBuf::from("/opt/ompchamber/bin/ompchamber");
        let env = env_map(&[
            ("ANTHROPIC_API_KEY", "sk-test"),
            ("OMPCHAMBER_API_ONLY", "true"),
        ]);
        let env_file = startup_env_file_path(&data_dir);
        write_startup_env_file(&env, &env_file, systemd_env_file_quote);
        let body = std::fs::read_to_string(&env_file).expect("env file");
        assert_eq!(
            body,
            "ANTHROPIC_API_KEY=\"sk-test\"\nOMPCHAMBER_API_ONLY=\"true\"\n"
        );

        let args = build_startup_args(4321, None, false);
        let unit = build_systemd_user_service(&exe, &args, &env_file, &home);
        assert!(unit.contains("Description=OMPChamber web server\n"));
        assert!(unit.contains(&format!("EnvironmentFile=-{}", env_file.display())));
        assert!(unit.contains("ExecStart=\"/opt/ompchamber/bin/ompchamber\" \"serve\" \"--foreground\" \"--port\" \"4321\"\n"));
        assert!(unit.contains(&format!("WorkingDirectory={}\n", home.display())));
        assert!(unit.contains("Restart=always\nRestartSec=5\n"));
        assert!(unit.contains("WantedBy=default.target\n"));
    }

    #[test]
    fn systemd_env_file_quotes_dollar_and_backtick() {
        assert_eq!(systemd_env_file_quote("a$b`c\\d"), "\"a\\$b\\`c\\\\d\"");
        assert_eq!(startup_env_file_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn windows_wrapper_and_task_command() {
        let data_dir = temp_dir("win-data");
        let exe = PathBuf::from("C:\\ompchamber\\ompchamber.exe");
        let env_file = startup_env_file_path(&data_dir);
        let args = build_startup_args(3000, None, false);
        let wrapper = windows_wrapper_content(&exe, &args, &env_file);
        assert!(wrapper.starts_with(&format!("$envFile='{}'", env_file.display())));
        assert!(
            wrapper.contains("[Environment]::SetEnvironmentVariable($matches[1], $v, 'Process')")
        );
        assert!(wrapper.ends_with(&format!(
            "& 'C:\\ompchamber\\ompchamber.exe' 'serve' '--foreground' '--port' '3000'"
        )));
        let task = build_windows_startup_task_command(Path::new("C:\\data\\bin\\OpenChamber.ps1"));
        assert_eq!(
            task,
            "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"C:\\data\\bin\\OpenChamber.ps1\""
        );
    }

    #[test]
    fn linux_status_parses_systemctl_output() {
        let home = temp_dir("status-home");
        let runner = FakeRunner::with_results(vec![
            // is-enabled: disabled
            StartupCommandOutput {
                status: 1,
                stdout: String::new(),
                stderr: "disabled\n".to_string(),
            },
            // is-active: active
            ok("active\n"),
        ]);
        let status = get_startup_status(&runner, StartupPlatform::Linux, &home);
        assert!(status.supported);
        assert!(!status.enabled, "no unit file and is-enabled failed");
        assert_eq!(status.active, Some(true));
        assert_eq!(status.active_state.as_deref(), Some("active"));
        assert_eq!(
            status.service_path.as_deref(),
            Some(
                home.join(".config/systemd/user/ompchamber.service")
                    .display()
                    .to_string()
                    .as_str()
            )
        );

        // is-active prints nothing → "inactive"; unit file present → enabled.
        let runner = FakeRunner::with_results(vec![
            StartupCommandOutput {
                status: 1,
                stdout: String::new(),
                stderr: String::new(),
            },
            StartupCommandOutput {
                status: 3,
                stdout: "\n".to_string(),
                stderr: String::new(),
            },
        ]);
        let unit = home
            .join(".config")
            .join("systemd")
            .join("user")
            .join("ompchamber.service");
        std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
        std::fs::write(&unit, "[Unit]\n").unwrap();
        let status = get_startup_status(&runner, StartupPlatform::Linux, &home);
        assert!(status.enabled, "existing unit file counts as enabled");
        assert_eq!(status.active, Some(false));
        assert_eq!(status.active_state.as_deref(), Some("inactive"));
    }

    #[test]
    fn macos_status_reflects_plist_presence() {
        let home = temp_dir("macos-status");
        let runner = FakeRunner::default();
        let status = get_startup_status(&runner, StartupPlatform::Macos, &home);
        assert!(status.supported);
        assert!(!status.enabled);
        assert_eq!(status.active, None);
        assert!(status.active_state.is_none());
        let plist = home
            .join("Library")
            .join("LaunchAgents")
            .join("dev.ompchamber.web.plist");
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "<plist/>").unwrap();
        let status = get_startup_status(&runner, StartupPlatform::Macos, &home);
        assert!(status.enabled);
    }

    #[test]
    fn windows_status_uses_schtasks_exit() {
        let home = temp_dir("win-status");
        let runner = FakeRunner::with_results(vec![StartupCommandOutput {
            status: 0,
            stdout: "dev.ompchamber.web\n".to_string(),
            stderr: String::new(),
        }]);
        let status = get_startup_status(&runner, StartupPlatform::Windows, &home);
        assert!(status.enabled);
        assert_eq!(status.active, None);
        assert_eq!(status.service_path.as_deref(), Some("dev.ompchamber.web"));
    }

    #[test]
    fn unsupported_platform_status_shape() {
        let home = temp_dir("other-status");
        let runner = FakeRunner::default();
        let status = get_startup_status(&runner, StartupPlatform::Other, &home);
        assert!(!status.supported);
        assert_eq!(status.platform, std::env::consts::OS);
        assert!(!status.enabled);
        assert!(
            status.active.is_none()
                && status.active_state.is_none()
                && status.service_path.is_none()
        );
        let json = startup_result_json(&status.with_action("status"));
        assert!(
            json.get("active").is_none(),
            "unsupported status has no active key"
        );
        assert!(json["servicePath"].is_null());
    }

    #[test]
    fn enable_macos_writes_files_and_invokes_launchctl() {
        let home = temp_dir("enable-home");
        let data_dir = temp_dir("enable-data");
        let exe = PathBuf::from("/opt/ompchamber/bin/ompchamber");
        let runner = FakeRunner::default();
        let env = env_map(&[("FOO", "bar")]);
        let options = StartupEnvOptions {
            host: None,
            ui_password: None,
            api_only: false,
            env_snapshot: true,
        };
        let result = enable_startup_service(
            &runner,
            StartupPlatform::Macos,
            &install(&home, &data_dir, &exe),
            &env,
            &options,
        )
        .expect("enable");
        assert!(result.enabled);
        let plist = home
            .join("Library")
            .join("LaunchAgents")
            .join("dev.ompchamber.web.plist");
        let body = std::fs::read_to_string(&plist).expect("plist written");
        assert!(body.contains("<key>EnvironmentVariables</key>"));
        let wrapper = macos_startup_wrapper_path(&data_dir);
        assert!(wrapper.exists(), "wrapper written");
        assert!(
            !startup_env_file_path(&data_dir).exists(),
            "macos enable removes the env file"
        );
        let calls = runner.calls();
        let programs: Vec<&str> = calls.iter().map(|(program, _)| program.as_str()).collect();
        assert_eq!(
            programs,
            vec!["/bin/launchctl", "/bin/launchctl", "/bin/launchctl"]
        );
        assert_eq!(calls[2].1[0], "kickstart");
        assert_eq!(calls[2].1[2], "gui/501/dev.ompchamber.web");
        assert_eq!(
            calls[1].1,
            vec![
                "bootstrap".to_string(),
                "gui/501".to_string(),
                plist.display().to_string()
            ]
        );
    }

    #[test]
    fn enable_linux_failed_state_is_reported() {
        let home = temp_dir("enable-linux-home");
        let data_dir = temp_dir("enable-linux-data");
        let exe = PathBuf::from("/usr/bin/ompchamber");
        // daemon-reload, enable --now, is-enabled, is-active(failed)
        let runner = FakeRunner::with_results(vec![
            ok(""),
            ok("Created symlink\n"),
            ok("enabled\n"),
            ok("failed\n"),
        ]);
        let env = env_map(&[("FOO", "bar")]);
        let options = StartupEnvOptions {
            host: None,
            ui_password: None,
            api_only: false,
            env_snapshot: true,
        };
        let result = enable_startup_service(
            &runner,
            StartupPlatform::Linux,
            &install(&home, &data_dir, &exe),
            &env,
            &options,
        )
        .expect("enable");
        assert_eq!(result.active_state.as_deref(), Some("failed"));
        let validated = validate_startup_result(&result.with_action("enable"));
        assert_eq!(
            validated.err().map(|error| (error.message, error.exit_code)),
            Some((
                "Startup service was installed but failed to start. Run `journalctl --user -u ompchamber.service -n 80 --no-pager` for details.".to_string(),
                GENERAL_ERROR
            ))
        );
        // The env file is written with systemd quoting.
        let env_body = std::fs::read_to_string(startup_env_file_path(&data_dir)).expect("env file");
        assert!(env_body.contains("FOO=\"bar\"\n"));
        let unit = std::fs::read_to_string(home.join(".config/systemd/user/ompchamber.service"))
            .expect("unit");
        assert!(unit.contains(
            "ExecStart=\"/usr/bin/ompchamber\" \"serve\" \"--foreground\" \"--port\" \"3456\""
        ));
    }

    #[test]
    fn enable_failure_on_bootstrap_is_general_error() {
        let home = temp_dir("boot-home");
        let data_dir = temp_dir("boot-data");
        let exe = PathBuf::from("/x/ompchamber");
        // bootout (ok), bootstrap (fails)
        let runner = FakeRunner::with_results(vec![
            ok(""),
            StartupCommandOutput {
                status: 5,
                stdout: String::new(),
                stderr: "Bootstrap failed".to_string(),
            },
        ]);
        let options = StartupEnvOptions {
            host: None,
            ui_password: None,
            api_only: false,
            env_snapshot: false,
        };
        let error = enable_startup_service(
            &runner,
            StartupPlatform::Macos,
            &install(&home, &data_dir, &exe),
            &BTreeMap::new(),
            &options,
        )
        .expect_err("bootstrap failure");
        assert_eq!(error.exit_code, GENERAL_ERROR);
        assert!(error.message.contains("/bin/launchctl bootstrap"));
        assert!(error.message.ends_with("failed: Bootstrap failed"));
    }

    #[test]
    fn disable_macos_removes_plist() {
        let home = temp_dir("disable-home");
        let data_dir = temp_dir("disable-data");
        let plist = home
            .join("Library")
            .join("LaunchAgents")
            .join("dev.ompchamber.web.plist");
        std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
        std::fs::write(&plist, "<plist/>").unwrap();
        let runner = FakeRunner::default();
        let result = disable_startup_service(&runner, StartupPlatform::Macos, &home, &data_dir)
            .expect("disable");
        assert!(!plist.exists());
        assert!(!result.enabled);
        assert_eq!(runner.calls()[0].1[0], "bootout");
    }

    #[test]
    fn unknown_subcommand_error_matches_js() {
        let home = temp_dir("usage-home");
        let data_dir = temp_dir("usage-data");
        let options = Options::default();
        let error = startup_command_with(
            &FakeRunner::default(),
            StartupPlatform::Macos,
            &home,
            &data_dir,
            Path::new("/x/ompchamber"),
            501,
            "Bogus",
            &BTreeMap::new(),
            None,
            &options,
            OutputMode::Human,
        )
        .expect_err("unknown subcommand");
        assert_eq!(error.exit_code, USAGE_ERROR);
        assert_eq!(
            error.message,
            "Unknown startup subcommand 'Bogus'. Use 'ompchamber startup --help'."
        );
    }

    #[test]
    fn quiet_and_json_output_shapes() {
        let mut result = get_startup_status(
            &FakeRunner::default(),
            StartupPlatform::Macos,
            Path::new("/tmp/x"),
        );
        result.action = "enable".to_string();
        result.enabled = true;
        assert_eq!(
            startup_quiet_line(&result),
            "startup enabled platform:macos supported:yes path:/tmp/x/Library/LaunchAgents/dev.ompchamber.web.plist"
        );
        let json = startup_result_json(&result);
        assert_eq!(json["action"], "enable");
        assert_eq!(json["supported"], true);
        assert_eq!(json["enabled"], true);
        assert!(json["active"].is_null());
        assert_eq!(
            json["servicePath"],
            "/tmp/x/Library/LaunchAgents/dev.ompchamber.web.plist"
        );

        // Quiet omits the path segment when servicePath is null.
        let unsupported = StartupResult {
            action: "status".into(),
            supported: false,
            platform: "sunos".into(),
            enabled: false,
            active: None,
            active_state: None,
            service_path: None,
        };
        assert_eq!(
            startup_quiet_line(&unsupported),
            "startup disabled platform:sunos supported:no"
        );
        assert!(
            validate_startup_result(&unsupported)
                .expect_err("unsupported")
                .message
                .contains("Startup integration is not supported on sunos.")
        );
    }

    #[test]
    fn human_lines_match_js_flow() {
        let mut result = get_startup_status(
            &FakeRunner::default(),
            StartupPlatform::Macos,
            Path::new("/tmp/x"),
        );
        result.action = "enable".to_string();
        result.enabled = true;
        let lines = startup_human_lines(&result);
        assert_eq!(lines[0], "OMPChamber Startup");
        assert_eq!(lines[1], "startup enabled");
        assert_eq!(
            lines[2],
            "  /tmp/x/Library/LaunchAgents/dev.ompchamber.web.plist"
        );
        assert_eq!(lines[3], "service command");
        assert_eq!(lines[4], "  ompchamber serve --foreground");
        assert_eq!(lines[5], "enable complete");

        let mut linux = get_startup_status(
            &FakeRunner::default(),
            StartupPlatform::Linux,
            Path::new("/tmp/x"),
        );
        linux.action = "status".to_string();
        linux.active = Some(true);
        linux.active_state = Some("active".to_string());
        let lines = startup_human_lines(&linux);
        assert_eq!(lines[1], "startup enabled"); // FakeRunner answers is-enabled with success
        assert_eq!(lines[2], "  /tmp/x/.config/systemd/user/ompchamber.service");
        assert_eq!(lines[3], "service active");
        assert_eq!(lines.last().unwrap(), "status complete");
    }

    #[test]
    fn help_text_matches_show_startup_help() {
        assert!(help_text().starts_with("\n OMPChamber Startup Commands\n\nUSAGE:\n"));
        assert!(help_text().contains(
            "  --no-env-snapshot       Do not save current environment for startup service\n"
        ));
        assert!(help_text().ends_with("  ompchamber startup status --json\n\n"));
    }
}
