//! Port of `server/lib/tunnels/executable-search.js`: cross-platform
//! executable discovery including Windows Store app aliases, with an
//! injectable filesystem seam (`fsLike` in the JS).
//! （中文说明）跨平台可执行文件查找：在 PATH 目录中解析 tunnel 依赖
//! 二进制（cloudflared/ngrok）的绝对路径，Windows 上额外覆盖 Microsoft
//! Store 应用执行别名目录；文件系统访问通过 `FsLike` trait 注入，
//! 与 JS 版的 `fsLike` 参数一一对应，使纯逻辑可在单测中离线验证。

use std::collections::HashMap;

use super::types::Platform;

/// 环境变量快照（`String -> String`），对应 JS 的 `process.env` 平面对象。
/// 由调用方显式传入而非直接读 `std::env`，便于测试复现各平台的环境形态。
pub type EnvMap = HashMap<String, String>;

/// `fsLike` seam: `statSync(path).isFile()` and `accessSync(path, X_OK)`.
/// 把文件系统探查抽象成两个方法，测试用内存实现替换真实磁盘访问；
/// 生产路径由 `RealFs` 承接。
pub trait FsLike: Send + Sync {
    /// 判断路径存在且为普通文件，对应 JS `statSync(path).isFile()`。
    fn stat_is_file(&self, path: &str) -> bool;
    /// 判断当前用户对路径拥有可执行权限，对应 JS `accessSync(path, X_OK)`。
    fn access_executable(&self, path: &str) -> bool;
}

/// 生产环境实现：直接调用 `std::fs`；IO 失败一律按"不是文件/不可执行"
/// 处理（返回 false），让上层继续尝试下一个候选路径而不是中断查找。
pub struct RealFs;

/// 真实文件系统下的接缝实现。
impl FsLike for RealFs {
    /// `metadata` 成功且 `is_file()` 为真才返回 true，任何错误返回 false。
    fn stat_is_file(&self, path: &str) -> bool {
        std::fs::metadata(path)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
    }

    /// Unix：检查 mode 是否含任一执行位（mode & 0o111 非 0）；
    /// 非 Unix 无执行位语义，恒返回 true。
    fn access_executable(&self, path: &str) -> bool {
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
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

/// 按候选键顺序返回第一个"存在且非空白"的环境变量值，全部缺失时返回
/// 空串。Windows 的变量名大小写不敏感，调用方通常传入
/// `PATH`/`Path`/`path` 等多个变体。
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

/// 生成目录去重键：先去首尾空白；Win32 再转小写（NTFS 路径不区分
/// 大小写），其他平台保持原样以保留 POSIX 的大小写敏感性。
fn normalize_search_directory_key(directory: &str, platform: Platform) -> String {
    let trimmed = directory.trim();
    if platform == Platform::Win32 {
        trimmed.to_lowercase()
    } else {
        trimmed.to_string()
    }
}

/// 以反斜杠拼接 Windows 路径：先把两段中的 `/` 归一为 `\`，再去掉
/// 接缝两侧多余的分隔符；任一段为空时直接返回另一段，避免产生
/// `C:\` 之类的悬空分隔符。
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

/// 计算 Windows Store 应用执行别名目录（`...\Microsoft\WindowsApps`）：
/// 依次尝试 `LOCALAPPDATA`、`USERPROFILE` 派生路径，最后退回传入的
/// home 目录，保证任何环境下都能得到一个可用候选。
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
///
/// 解析可执行文件的搜索目录列表：按平台分隔符（Win32 用 `;`，其余用
/// `:`）拆分 PATH 并过滤空项，Win32 额外追加 WindowsApps 别名目录；
/// 最后按归一化键保序去重，首个出现的目录胜出。
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
///
/// 构造传给子进程的环境：把去重后的搜索目录重新用平台分隔符拼回
/// PATH。Win32 下同时写入 `PATH`/`Path`/`path` 三个大小写变体，防止
/// 子进程读取其中任意一种拼写时漏掉 WindowsApps；其余平台只写 `PATH`。
/// 返回的是原环境的克隆加新 PATH，不修改入参。
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

/// 返回可执行文件的候选扩展名：Win32 读取 `PATHEXT`（缺省
/// `.EXE;.CMD;.BAT;.COM`），统一转小写并补前导点；非 Windows 平台返回
/// 单个空串，表示只按命令名原样查找。
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
///
/// 逐目录查找命令：Win32 对每个候选扩展名拼 `命令名+扩展名`，其他
/// 平台直接用命令名本身；候选必须是普通文件，且非 Win32 平台还须
/// 通过执行位检查。命中返回绝对路径；命令名为空白或全部未命中时
/// 返回 None。
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
    /// 启动命令：PATH 解析命中时为绝对路径；Win32 别名兜底时为原命令名。
    pub command: String,
    /// 传给子进程的完整环境变量（PATH 已扩展为含 WindowsApps 的去重列表）。
    pub env: EnvMap,
}

/// 一步完成"PATH 解析 + 环境构造"，产出可直接 spawn 的目标：命中则
/// 返回绝对路径与新环境；Win32 未命中时仍返回原命令名 —— Windows
/// Store 应用执行别名可被 CreateProcess 启动，却会让 stat/access 得到
/// EACCES，因此把成败交给后续的版本探测；非 Win32 未命中返回 None。
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
///
/// 捕获当前进程的全部环境变量；与 `real_home` 一起构成非测试路径的
/// 默认输入。
pub fn real_env() -> EnvMap {
    std::env::vars().collect()
}

/// 当前工作目录（`process.cwd()` 的等价物）；获取失败返回空串。
pub fn real_cwd() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 当前用户主目录：Windows 读 `USERPROFILE`，其余平台读 `HOME`；
/// 变量缺失时返回空串。
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

/// 覆盖目录解析、Windows PATH 大小写变体、WindowsApps 别名兜底、
/// POSIX 执行位过滤与 Win32 启动目标回退逻辑的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 内存版 `FsLike`：用显式文件列表和固定的执行位结果替代真实磁盘。
    struct MapFs {
    /// 视为"存在且为普通文件"的路径集合。
        files: Vec<String>,
    /// 所有路径统一的 `access_executable` 返回值。
        executable: bool,
    }

    /// `MapFs` 对接缝的实现：查列表 + 固定执行位。
    impl FsLike for MapFs {
        /// 路径在 `files` 列表中即视为普通文件。
        fn stat_is_file(&self, path: &str) -> bool {
            self.files.iter().any(|file| file == path)
        }

        /// 恒返回构造时给定的 `executable` 标志。
        fn access_executable(&self, _path: &str) -> bool {
            self.executable
        }
    }

    /// 由 `(&str, &str)` 数组构造 `EnvMap` 的便捷助手。
    fn env_from(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// 行为契约：Win32 下搜索目录自动追加 LOCALAPPDATA 派生的
    /// WindowsApps 别名目录。
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

    /// 行为契约：`PATH` 键缺失时回退读取 `Path` 变体，仍能按序拆出目录。
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

    /// 行为契约：PATH 未包含 WindowsApps 时，仍能在别名目录中按
    /// PATHEXT 扩展名找到 `ngrok.exe`。
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

    /// 行为契约：POSIX 上无执行位的匹配会被跳过，置位后才返回该路径。
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

    /// 行为契约：Win32 上 stat 全部失败时仍返回原命令名作为启动目标，
    /// 且其环境 PATH 已带上 WindowsApps 目录。
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

    /// 行为契约：非 Win32 平台未命中任何候选时返回 None（无别名兜底）。
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

    /// 行为契约：构造出的环境中 `PATH`/`Path`/`path` 三个变体保持一致，
    /// 且都以 WindowsApps 目录结尾。
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

    /// 行为契约：空白命令名直接返回 None，不访问文件系统。
    #[test]
    fn empty_command_returns_none() {
        let env = EnvMap::new();
        assert_eq!(
            find_executable_on_path("   ", &env, Platform::Posix, "/home/ada", &RealFs),
            None
        );
    }
}
