//! Port of `server/lib/tunnels/executable-search.js`: cross-platform
//! executable discovery including Windows Store app aliases, with an
//! injectable filesystem seam (`fsLike` in the JS).

use std::collections::HashMap;

use super::types::Platform;

pub type EnvMap = HashMap<String, String>;

/// `fsLike` seam: `statSync(path).isFile()` and `accessSync(path, X_OK)`.
pub trait FsLike: Send + Sync {
    fn stat_is_file(&self, path: &str) -> bool;
    fn access_executable(&self, path: &str) -> bool;
}

pub struct RealFs;

impl FsLike for RealFs {
    fn stat_is_file(&self, path: &str) -> bool {
        std::fs::metadata(path)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
    }

    fn access_executable(&self, path: &str) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path)
                .map(|meta| meta.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
}

fn get_env_value(env: &EnvMap, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = env.get(*key)
            && !value.trim().is_empty()
        {
            return value.clone();
        }
    }
    String::new()
}

fn normalize_search_directory_key(directory: &str, platform: Platform) -> String {
    let trimmed = directory.trim();
    if platform == Platform::Win32 {
        trimmed.to_lowercase()
    } else {
        trimmed.to_string()
    }
}

fn win_join(base: &str, rest: &str) -> String {
    let base = base.replace('/', "\\").trim_end_matches('\\').to_string();
    let rest = rest.replace('/', "\\").trim_start_matches('\\').to_string();
    if base.is_empty() {
        rest
    } else if rest.is_empty() {
        base
    } else {
        format!("{base}\\{rest}")
    }
}

fn get_windows_apps_directory(env: &EnvMap, home: &str) -> String {
    let local_app_data = get_env_value(env, &["LOCALAPPDATA", "LocalAppData", "localappdata"]);
    if !local_app_data.is_empty() {
        return win_join(&local_app_data, "Microsoft/WindowsApps");
    }

    let user_profile = get_env_value(env, &["USERPROFILE", "UserProfile", "userprofile"]);
    if !user_profile.is_empty() {
        return win_join(&user_profile, "AppData/Local/Microsoft/WindowsApps");
    }

    win_join(home, "AppData/Local/Microsoft/WindowsApps")
}

/// `getExecutableSearchDirectories({ env, platform })`.
pub fn get_executable_search_directories(
    env: &EnvMap,
    platform: Platform,
    home: &str,
) -> Vec<String> {
    let delimiter = if platform == Platform::Win32 {
        ';'
    } else {
        ':'
    };
    let path_value = get_env_value(env, &["PATH", "Path", "path"]);
    let mut directories: Vec<String> = path_value
        .split(delimiter)
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect();

    if platform == Platform::Win32 {
        directories.push(get_windows_apps_directory(env, home));
    }

    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for directory in directories {
        let key = normalize_search_directory_key(&directory, platform);
        if key.is_empty() || !seen.insert(key) {
            continue;
        }
        unique.push(directory);
    }
    unique
}

/// `createExecutableSearchEnv({ env, platform })`.
pub fn create_executable_search_env(env: &EnvMap, platform: Platform, home: &str) -> EnvMap {
    let delimiter = if platform == Platform::Win32 {
        ';'
    } else {
        ':'
    };
    let path_value =
        get_executable_search_directories(env, platform, home).join(&delimiter.to_string());
    let mut next_env = env.clone();
    if platform == Platform::Win32 {
        next_env.insert("PATH".to_string(), path_value.clone());
        next_env.insert("Path".to_string(), path_value.clone());
        next_env.insert("path".to_string(), path_value);
    } else {
        next_env.insert("PATH".to_string(), path_value);
    }
    next_env
}

fn get_executable_extensions(env: &EnvMap, platform: Platform) -> Vec<String> {
    if platform != Platform::Win32 {
        return vec![String::new()];
    }

    let raw = env
        .get("PATHEXT")
        .or_else(|| env.get("PathExt"))
        .or_else(|| env.get("pathext"))
        .cloned()
        .unwrap_or_else(|| ".EXE;.CMD;.BAT;.COM".to_string());
    raw.split(';')
        .map(|ext| ext.trim().to_lowercase())
        .filter(|ext| !ext.is_empty())
        .map(|ext| {
            if ext.starts_with('.') {
                ext
            } else {
                format!(".{ext}")
            }
        })
        .collect()
}

/// `findExecutableOnPath(command, { env, platform, fsLike })`.
pub fn find_executable_on_path(
    command: &str,
    env: &EnvMap,
    platform: Platform,
    home: &str,
    fs_like: &dyn FsLike,
) -> Option<String> {
    let command_name = command.trim();
    if command_name.is_empty() {
        return None;
    }

    let directories = get_executable_search_directories(env, platform, home);
    let extensions = get_executable_extensions(env, platform);

    for directory in directories {
        for extension in &extensions {
            let file_name = if platform == Platform::Win32 {
                format!("{command_name}{extension}")
            } else {
                command_name.to_string()
            };
            let candidate = super::types::platform_join(&directory, &file_name, platform);
            if !fs_like.stat_is_file(&candidate) {
                continue;
            }
            if platform != Platform::Win32 && !fs_like.access_executable(&candidate) {
                continue;
            }
            return Some(candidate);
        }
    }

    None
}

/// `resolveExecutableLaunchTarget(command, options)`.
#[derive(Debug, Clone)]
pub struct LaunchTarget {
    pub command: String,
    pub env: EnvMap,
}

pub fn resolve_executable_launch_target(
    command: &str,
    env: &EnvMap,
    platform: Platform,
    home: &str,
    fs_like: &dyn FsLike,
) -> Option<LaunchTarget> {
    let resolved_path = find_executable_on_path(command, env, platform, home, fs_like);
    let next_env = create_executable_search_env(env, platform, home);
    if let Some(resolved) = resolved_path {
        return Some(LaunchTarget {
            command: resolved,
            env: next_env,
        });
    }

    // Windows Store app execution aliases are launchable through CreateProcess
    // but can reject fs.stat/fs.access with EACCES. Let the version probe decide.
    let trimmed = command.trim();
    if platform == Platform::Win32 && !trimmed.is_empty() {
        return Some(LaunchTarget {
            command: trimmed.to_string(),
            env: next_env,
        });
    }

    None
}

/// The real process environment and home directory (`process.env`/`os.homedir()`).
pub fn real_env() -> EnvMap {
    std::env::vars().collect()
}

pub fn real_cwd() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn real_home() -> String {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE").unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME").unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MapFs {
        files: Vec<String>,
        executable: bool,
    }

    impl FsLike for MapFs {
        fn stat_is_file(&self, path: &str) -> bool {
            self.files.iter().any(|file| file == path)
        }

        fn access_executable(&self, _path: &str) -> bool {
            self.executable
        }
    }

    fn env_from(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn adds_windowsapps_alias_directory_on_windows() {
        let env = env_from(&[
            ("PATH", "C:\\Tools"),
            ("LOCALAPPDATA", "C:\\Users\\Ada\\AppData\\Local"),
        ]);

        let directories =
            get_executable_search_directories(&env, Platform::Win32, "C:\\Users\\Ada");
        assert!(
            directories
                .contains(&"C:\\Users\\Ada\\AppData\\Local\\Microsoft\\WindowsApps".to_string())
        );
    }

    #[test]
    fn reads_windows_path_casing_when_path_missing() {
        let env = env_from(&[
            ("Path", "C:\\Tools;C:\\MoreTools"),
            ("LOCALAPPDATA", "C:\\Users\\Ada\\AppData\\Local"),
        ]);

        let directories =
            get_executable_search_directories(&env, Platform::Win32, "C:\\Users\\Ada");
        assert_eq!(directories[0], "C:\\Tools");
        assert_eq!(directories[1], "C:\\MoreTools");
    }

    #[test]
    fn finds_windows_store_alias_even_when_path_omits_windowsapps() {
        let alias_path = "C:\\Users\\Ada\\AppData\\Local\\Microsoft\\WindowsApps\\ngrok.exe";
        let env = env_from(&[
            ("PATH", "C:\\Tools"),
            ("LOCALAPPDATA", "C:\\Users\\Ada\\AppData\\Local"),
            ("PATHEXT", ".EXE;.CMD"),
        ]);
        let fs = MapFs {
            files: vec![alias_path.to_string()],
            executable: true,
        };

        let resolved =
            find_executable_on_path("ngrok", &env, Platform::Win32, "C:\\Users\\Ada", &fs);
        assert_eq!(resolved.as_deref(), Some(alias_path));
    }

    #[test]
    fn skips_non_executable_matches_on_posix() {
        let candidate = "/opt/tools/mybin";
        let env = env_from(&[("PATH", "/opt/tools")]);
        let fs = MapFs {
            files: vec![candidate.to_string()],
            executable: false,
        };
        assert_eq!(
            find_executable_on_path("mybin", &env, Platform::Posix, "/home/ada", &fs),
            None
        );

        let fs = MapFs {
            files: vec![candidate.to_string()],
            executable: true,
        };
        assert_eq!(
            find_executable_on_path("mybin", &env, Platform::Posix, "/home/ada", &fs),
            Some(candidate.to_string())
        );
    }

    #[test]
    fn returns_windows_launch_target_with_windowsapps_on_path_when_stat_fails() {
        let env = env_from(&[
            ("PATH", "C:\\Windows\\System32"),
            ("LOCALAPPDATA", "C:\\Users\\Ada\\AppData\\Local"),
        ]);
        let fs = MapFs {
            files: vec![],
            executable: true,
        };

        let target =
            resolve_executable_launch_target("ngrok", &env, Platform::Win32, "C:\\Users\\Ada", &fs)
                .expect("launch target for win32 app alias");
        assert_eq!(target.command, "ngrok");
        assert!(
            target
                .env
                .get("Path")
                .map(|value| value
                    .contains("C:\\Users\\Ada\\AppData\\Local\\Microsoft\\WindowsApps"))
                .unwrap_or(false)
        );
    }

    #[test]
    fn posix_without_match_returns_none() {
        let env = env_from(&[("PATH", "/opt/tools")]);
        let fs = MapFs {
            files: vec![],
            executable: true,
        };
        assert!(
            resolve_executable_launch_target("missing", &env, Platform::Posix, "/home/ada", &fs)
                .is_none()
        );
    }

    #[test]
    fn keeps_windows_path_variants_in_sync() {
        let env = env_from(&[
            ("PATH", "C:\\Windows\\System32"),
            ("LOCALAPPDATA", "C:\\Users\\Ada\\AppData\\Local"),
        ]);

        let next = create_executable_search_env(&env, Platform::Win32, "C:\\Users\\Ada");
        assert_eq!(next.get("PATH"), next.get("Path"));
        assert_eq!(next.get("path"), next.get("Path"));
        assert!(
            next.get("Path")
                .unwrap()
                .ends_with("C:\\Users\\Ada\\AppData\\Local\\Microsoft\\WindowsApps")
        );
    }

    #[test]
    fn empty_command_returns_none() {
        let env = EnvMap::new();
        assert_eq!(
            find_executable_on_path("   ", &env, Platform::Posix, "/home/ada", &RealFs),
            None
        );
    }
}
