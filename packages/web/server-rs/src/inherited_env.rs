//! Port of `server/lib/inherited-env.js`.
//!
//! Sanitizes environments inherited by user-facing child processes. Linux
//! AppImage runtimes export `ARGV0`, which zsh treats as argv[0] for every
//! spawned command, corrupting venv detection and friends
//! (openchamber/openchamber#2588). The PTY launch wrapper re-execs through
//! `env -u ARGV0` so the leak is gone before the shell starts.

use std::collections::HashMap;
use std::path::Path;

const LINUX_ENV_BINARIES: [&str; 2] = ["/usr/bin/env", "/bin/env"];

/// Remove AppImage `ARGV0` from a mutable env map (JS mutates in place).
pub fn strip_app_image_argv0_leak(
    env: &mut HashMap<String, String>,
) -> &mut HashMap<String, String> {
    env.remove("ARGV0");
    env
}

/// Resolve a Linux PTY launch that drops native `ARGV0` before the shell
/// starts. No-op on non-Linux platforms.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn pty_launch_passthrough_off_linux() {
        let (exe, args) = resolve_linux_pty_launch("/bin/zsh", &["-l".to_string()]);
        assert_eq!(exe, "/bin/zsh");
        assert_eq!(args, vec!["-l".to_string()]);
    }
}
