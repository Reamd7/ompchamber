//! Port of `server/lib/inherited-env.js`.
//!
//! Sanitizes environments inherited by user-facing child processes. Linux
//! AppImage runtimes export `ARGV0`, which zsh treats as argv[0] for every
//! spawned command, corrupting venv detection and friends
//! (openchamber/openchamber#2588). The PTY launch wrapper re-execs through
//! `env -u ARGV0` so the leak is gone before the shell starts.
//!
//! 本模块是 `server/lib/inherited-env.js` 的移植，负责净化继承给
//! 用户侧子进程的环境变量。PTY 启动包装器通过 `env -u ARGV0` 重新
//! exec，保证 shell 启动前 `ARGV0` 泄漏已被清除。

use std::collections::HashMap;
use std::path::Path;

/// Linux 上按序探测的 `env` 二进制路径，用于包装 PTY 启动命令。
const LINUX_ENV_BINARIES: [&str; 2] = ["/usr/bin/env", "/bin/env"];

/// Remove AppImage `ARGV0` from a mutable env map (JS mutates in place).
/// 从可变 env map 中就地移除 AppImage 泄漏的 `ARGV0`（与 JS 版一致，
/// 原地修改），并返回同一个 env 引用，便于调用点继续使用。
pub fn strip_app_image_argv0_leak(
    env: &mut HashMap<String, String>,
) -> &mut HashMap<String, String> {
    env.remove("ARGV0");
    env
}

/// Resolve a Linux PTY launch that drops native `ARGV0` before the shell
/// starts. No-op on non-Linux platforms.
/// 解析 Linux 平台的 PTY 启动命令：包装为
/// `env -u ARGV0 <executable> <args...>`，让 shell 启动前就剔除原生
/// `ARGV0`。非 Linux 平台或找不到可用的 env 二进制时不做包装，
/// 原样返回 (executable, args)。
pub fn resolve_linux_pty_launch(executable: &str, args: &[String]) -> (String, Vec<String>) {
    if !cfg!(target_os = "linux") {
        return (executable.to_string(), args.to_vec());
    }
    let Some(env_binary) = LINUX_ENV_BINARIES
        .iter()
        .find(|candidate| Path::new(candidate).exists())
    else {
        return (executable.to_string(), args.to_vec());
    };
    let mut wrapped = vec![
        "-u".to_string(),
        "ARGV0".to_string(),
        executable.to_string(),
    ];
    wrapped.extend(args.iter().cloned());
    (env_binary.to_string(), wrapped)
}

/// 环境净化与 PTY 启动包装的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 `strip_app_image_argv0_leak` 只移除 `ARGV0`，其余环境变量
    /// （如 PATH）原样保留。
    #[test]
    fn strips_argv0_only() {
        let mut env = HashMap::from([
            ("ARGV0".to_string(), "/tmp/AppDir/usr/bin/app".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ]);
        strip_app_image_argv0_leak(&mut env);
        assert!(!env.contains_key("ARGV0"));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
    }

    /// 验证非 Linux 平台上 PTY 启动不做包装，命令与参数原样透传。
    #[test]
    #[cfg(not(target_os = "linux"))]
    fn pty_launch_passthrough_off_linux() {
        let (exe, args) = resolve_linux_pty_launch("/bin/zsh", &["-l".to_string()]);
        assert_eq!(exe, "/bin/zsh");
        assert_eq!(args, vec!["-l".to_string()]);
    }
}
