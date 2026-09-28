//! Port of `server/lib/terminal/shells.js`.
//!
//! Discovers executable shell families and resolves the persisted shell ID.
//! No command strings or arguments are ever accepted — only well-known shell
//! ids and executable paths. All environment/filesystem access is injected
//! (`createTerminalShellResolver` deps), mirroring the JS seam used by tests.
//!
//! 中文说明：移植自 server/lib/terminal/shells.js。发现可执行的 shell
//! 家族，并把持久化的 shell 偏好解析为具体可执行文件。安全约束：绝不
//! 接受命令串或任意参数，只认白名单 shell id 与可执行路径；所有环境与
//! 文件系统访问经 ShellDeps 注入（对应 JS createTerminalShellResolver
//! 的依赖缝），测试借此注入假环境。

use std::collections::HashSet;

/// Known shell families, in discovery order.
/// 中文补充：同时充当发现顺序——auto 之外的条目按此数组次序出现。
pub const TERMINAL_SHELL_IDS: [&str; 10] = [
    "bash",
    "zsh",
    "sh",
    "fish",
    "pwsh",
    "powershell",
    "cmd",
    "dash",
    "ksh",
    "nu",
];

/// 判断值是否为白名单 shell id（精确匹配，不做归一化）。
pub fn is_terminal_shell_id(value: &str) -> bool {
    TERMINAL_SHELL_IDS.contains(&value)
}

/// `normalizeTerminalShell`: `'auto'` or a known id, else `None`.
/// 中文补充：trim + 小写后必须是 auto 或已知 id，否则 None——命令串在这里被拒绝。
pub fn normalize_terminal_shell(value: Option<&str>) -> Option<String> {
    let value = value?;
    let normalized = value.trim().to_ascii_lowercase();
    if normalized == "auto" || is_terminal_shell_id(&normalized) {
        Some(normalized)
    } else {
        None
    }
}

/// `shellIdFromPath`: basename (backslashes normalized), lowercased, `.exe`
/// stripped — must be a known shell id.
/// 中文补充：Windows 反斜杠归一为斜杠；识别失败返回 None。
pub fn shell_id_from_path(value: &str) -> Option<&'static str> {
    let normalized = value.replace('\\', "/");
    let filename = normalized
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let id = filename.strip_suffix(".exe").unwrap_or(&filename);
    TERMINAL_SHELL_IDS
        .iter()
        .find(|candidate| **candidate == id)
        .copied()
}

/// 特殊展示名映射（PowerShell 家族）；表外 shell 直接用 id 展示。
const SHELL_LABELS: [(&str, &str); 3] = [
    ("pwsh", "PowerShell"),
    ("powershell", "Windows PowerShell"),
    ("cmd", "Command Prompt"),
];

/// 查 id 对应的展示名；缺省回退为 id 本身。
fn shell_label(id: &str) -> &str {
    SHELL_LABELS
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, label)| *label)
        .unwrap_or(id)
}

/// `process.platform` stand-in for cross-platform tests.
/// 中文补充：决定默认候选序列、路径分隔符与登录参数表的平台分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Windows：ComSpec/PowerShell 优先，路径用反斜杠。
    Windows,
    /// Unix 系：SHELL 优先，/bin/zsh、/bin/bash、/bin/sh 回退。
    Posix,
}

/// 平台工具方法。
impl Platform {
    /// 编译目标对应的当前平台（测试可注入其它值）。
    pub fn current() -> Self {
        if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::Posix
        }
    }
}

/// `getTerminalShellLoginArgs`: built-in login-mode arguments for known shells
/// only; `None` means the shell has no supported login mode.
/// 中文补充：参数硬编码在白名单里，杜绝通过偏好注入任意参数。
pub fn get_terminal_shell_login_args(executable: &str, platform: Platform) -> Option<Vec<String>> {
    let id = shell_id_from_path(executable)?;
    match id {
        "bash" | "zsh" | "ksh" => Some(vec!["-l".to_string()]),
        "fish" | "nu" => Some(vec!["--login".to_string()]),
        "pwsh" if platform != Platform::Windows => Some(vec!["-Login".to_string()]),
        _ => None,
    }
}

/// Injected dependencies (`createTerminalShellResolver` options). `env` is a
/// live lookup so `OMPCHAMBER_TERMINAL_SHELL`/`SHELL` are read per resolution
/// like the JS closure over `process.env`.
/// 中文补充：env 是活查询——每次解析现读，等价 JS 对 process.env 的闭包。
pub struct ShellDeps {
    /// 目标平台，决定默认候选与路径分隔符。
    pub platform: Platform,
    /// 环境变量查询（OMPCHAMBER_TERMINAL_SHELL/SHELL 等覆盖项）。
    pub env: Box<dyn Fn(&str) -> Option<String> + Send + Sync>,
    /// 构造传给 PTY 的增强 PATH。
    pub build_augmented_path: Box<dyn Fn() -> String + Send + Sync>,
    /// 在给定搜索路径中按名字查找可执行文件。
    pub search_path_for: Box<dyn Fn(&str, &str) -> Option<String> + Send + Sync>,
    /// 判定路径是否为可执行文件。
    pub is_executable: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// `/etc/shells` contents (Posix only; `None` skips configured shells).
    /// 中文补充：Windows 下不参与发现。
    pub read_etc_shells: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

/// 面向客户端的 shell 条目（list 接口的输出）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellInfo {
    /// shell id（auto 表示跟随默认解析）。
    pub id: String,
    /// 展示名。
    pub name: String,
    /// 解析到的可执行路径；auto 可能解析不到（None）。
    pub executable: Option<String>,
    /// 是否支持登录模式（按登录参数表判定）。
    pub supports_login: bool,
}

/// 解析结果：选定的 shell id 与依序尝试的可执行文件列表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedShell {
    /// 选定 id。
    pub id: String,
    /// 候选可执行文件；spawn 逐个尝试，首个成功者生效。
    pub executables: Vec<String>,
}

/// 解析失败（包裹面向客户端的错误文案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellError(pub String);

/// 直接透出内部文案，供错误映射到 HTTP 响应。
impl std::fmt::Display for ShellError {
    /// 输出错误消息本体。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// shell 解析器：持有注入依赖，提供列表与解析能力。
pub struct ShellResolver {
    /// 注入的环境与文件系统依赖。
    deps: ShellDeps,
}

/// 发现与解析逻辑（listShells/resolveShell 的移植）。
impl ShellResolver {
    /// 以给定依赖构造解析器。
    pub fn new(deps: ShellDeps) -> Self {
        Self { deps }
    }

    /// 便捷的环境变量查询。
    fn env(&self, name: &str) -> Option<String> {
        (self.deps.env)(name)
    }

    /// 按平台分隔符拼接路径片段。
    fn join_path(&self, parts: &[&str]) -> String {
        let separator = if self.deps.platform == Platform::Windows {
            "\\"
        } else {
            "/"
        };
        parts.join(separator)
    }

    /// `resolveExecutable`: pathful candidates are used as-is, bare names are
    /// searched on the augmented PATH; falls back to the raw value.
    /// 中文补充：候选不可执行时整体 miss，交由上层报“不可用”。
    fn resolve_executable(&self, candidate: Option<&str>) -> Option<String> {
        let value = candidate?;
        if value.is_empty() {
            return None;
        }
        let found = if value.contains('/') || value.contains('\\') {
            Some(value.to_string())
        } else {
            (self.deps.search_path_for)(value, &(self.deps.build_augmented_path)())
        };
        if let Some(found) = found
            && (self.deps.is_executable)(&found)
        {
            return Some(found);
        }
        if (self.deps.is_executable)(value) {
            Some(value.to_string())
        } else {
            None
        }
    }

    /// `defaultCandidates`: environment overrides before platform defaults.
    /// 中文补充：Windows 用 ComSpec 与系统 PowerShell 路径兜底，Posix 用 /bin 三件套。
    fn default_candidates(&self) -> Vec<Option<String>> {
        match self.deps.platform {
            Platform::Windows => {
                let system_root = self
                    .env("SystemRoot")
                    .unwrap_or_else(|| "C:\\Windows".to_string());
                vec![
                    self.env("OMPCHAMBER_TERMINAL_SHELL"),
                    self.env("SHELL"),
                    self.env("ComSpec"),
                    Some(self.join_path(&[
                        &system_root,
                        "System32",
                        "WindowsPowerShell",
                        "v1.0",
                        "powershell.exe",
                    ])),
                    Some("pwsh.exe".to_string()),
                    Some("powershell.exe".to_string()),
                    Some("cmd.exe".to_string()),
                ]
            }
            Platform::Posix => vec![
                self.env("OMPCHAMBER_TERMINAL_SHELL"),
                self.env("SHELL"),
                Some("/bin/zsh".to_string()),
                Some("/bin/bash".to_string()),
                Some("/bin/sh".to_string()),
                Some("zsh".to_string()),
                Some("bash".to_string()),
                Some("sh".to_string()),
            ],
        }
    }

    /// `resolveCandidates`: resolve in order, drop misses, dedupe.
    /// 中文补充：去重按解析后的路径，避免同一可执行文件重复出现。
    fn resolve_candidates(&self, candidates: &[Option<String>]) -> Vec<String> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut resolved = Vec::new();
        for candidate in candidates {
            if let Some(found) = self.resolve_executable(candidate.as_deref())
                && seen.insert(found.clone())
            {
                resolved.push(found);
            }
        }
        resolved
    }

    /// `list`: available shell entries, `'auto'` first, then discovery order.
    /// 中文补充：auto 条目总是第一个，且带其解析出的可执行文件与登录支持。
    pub fn list(&self) -> Vec<ShellInfo> {
        let configured_shells: Vec<String> = if self.deps.platform != Platform::Windows {
            (self.deps.read_etc_shells)()
                .map(|contents| {
                    contents
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty() && !line.starts_with('#'))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        let candidates: Vec<Option<String>> = match self.deps.platform {
            Platform::Windows => {
                let mut all = self.default_candidates();
                all.extend(TERMINAL_SHELL_IDS.iter().map(|id| Some(id.to_string())));
                all
            }
            Platform::Posix => {
                let mut all = vec![self.env("OMPCHAMBER_TERMINAL_SHELL"), self.env("SHELL")];
                all.extend(configured_shells.into_iter().map(Some));
                all.extend(TERMINAL_SHELL_IDS.iter().map(|id| Some(id.to_string())));
                all.extend(
                    ["/bin/zsh", "/bin/bash", "/bin/sh"]
                        .iter()
                        .map(|path| Some(path.to_string())),
                );
                all
            }
        };

        let defaults = self.default_candidates();
        let auto_executable = self.resolve_candidates(&defaults).into_iter().next();
        let auto_supports_login = auto_executable
            .as_deref()
            .and_then(|exe| get_terminal_shell_login_args(exe, self.deps.platform))
            .is_some();

        let mut shells = vec![ShellInfo {
            id: "auto".to_string(),
            name: "Auto".to_string(),
            executable: auto_executable,
            supports_login: auto_supports_login,
        }];
        let mut seen: HashSet<String> = HashSet::from(["auto".to_string()]);
        for executable in self.resolve_candidates(&candidates) {
            if let Some(id) = shell_id_from_path(&executable)
                && seen.insert(id.to_string())
            {
                shells.push(ShellInfo {
                    id: id.to_string(),
                    name: shell_label(id).to_string(),
                    supports_login: get_terminal_shell_login_args(&executable, self.deps.platform)
                        .is_some(),
                    executable: Some(executable),
                });
            }
        }
        shells
    }

    /// The augmented PATH handed to spawned PTYs (JS `buildAugmentedPath`).
    /// 中文补充：即 ShellDeps.build_augmented_path 的直接转发。
    pub fn augmented_path(&self) -> String {
        (self.deps.build_augmented_path)()
    }

    /// `resolve`: `None` preference (JS `preference ?? 'auto'`) means auto.
    /// 中文补充：auto 返回全部默认候选；显式 id 必须在 list 结果中，否则报不可用。
    pub fn resolve(&self, preference: Option<&str>) -> Result<ResolvedShell, ShellError> {
        let normalized = normalize_terminal_shell(Some(preference.unwrap_or("auto")))
            .ok_or_else(|| ShellError("Invalid terminal shell".to_string()))?;
        if normalized == "auto" {
            return Ok(ResolvedShell {
                id: "auto".to_string(),
                executables: self.resolve_candidates(&self.default_candidates()),
            });
        }
        let selected = self
            .list()
            .into_iter()
            .find(|shell| shell.id == normalized)
            .ok_or_else(|| {
                ShellError(format!("Terminal shell \"{normalized}\" is not available"))
            })?;
        Ok(ResolvedShell {
            id: selected.id,
            executables: vec![selected.executable.unwrap_or_default()],
        })
    }
}

/// 单元测试：归一化、路径识别、登录参数表、各平台发现顺序与解析结果。
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// 构造带调用录制的测试依赖：search 调用被记录，可执行集合与
    /// 环境/PATH 由参数给出，返回 (resolver, search 调用记录)。
    fn test_deps(
        platform: Platform,
        env: HashMap<&str, &str>,
        executables: Vec<&str>,
        augmented_path: &str,
    ) -> (ShellResolver, Arc<Mutex<Vec<(String, String)>>>) {
        let executables: Vec<String> = executables.iter().map(|e| (*e).to_string()).collect();
        let searches: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let env_map: HashMap<String, String> = env
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        let recorder = Arc::clone(&searches);
        let search_executables = executables.clone();
        let check_executables = executables.clone();
        let augmented = augmented_path.to_string();
        let deps = ShellDeps {
            platform,
            env: Box::new(move |key: &str| env_map.get(key).cloned()),
            build_augmented_path: Box::new(move || augmented.clone()),
            search_path_for: Box::new(move |name: &str, search_path: &str| {
                recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((name.to_string(), search_path.to_string()));
                let suffixes: &[&str] = if platform == Platform::Windows {
                    &["", ".exe"]
                } else {
                    &[""]
                };
                let separator = if platform == Platform::Windows {
                    '\\'
                } else {
                    '/'
                };
                for suffix in suffixes {
                    let needle = format!("{separator}{name}{suffix}").to_ascii_lowercase();
                    if let Some(candidate) = search_executables
                        .iter()
                        .find(|candidate| candidate.to_ascii_lowercase().ends_with(&needle))
                    {
                        return Some(candidate.clone());
                    }
                }
                None
            }),
            is_executable: Box::new(move |candidate: &str| {
                check_executables.iter().any(|exe| exe == candidate)
            }),
            read_etc_shells: Box::new(|| None),
        };
        (ShellResolver::new(deps), searches)
    }

    /// 归一化接受空白与大小写变体、拒绝命令串与未知 id；None 偏好不归一化。
    #[test]
    fn normalizes_shell_preferences() {
        assert_eq!(
            normalize_terminal_shell(Some(" zsh ")),
            Some("zsh".to_string())
        );
        assert_eq!(
            normalize_terminal_shell(Some("AUTO")),
            Some("auto".to_string())
        );
        assert_eq!(
            normalize_terminal_shell(Some("pwsh")),
            Some("pwsh".to_string())
        );
        assert_eq!(normalize_terminal_shell(Some("zsh -c whoami")), None);
        assert_eq!(normalize_terminal_shell(Some("fishy")), None);
        assert_eq!(normalize_terminal_shell(None), None);
    }

    /// 路径识别处理 Unix 路径、Windows 反斜杠与 .exe 后缀；非 shell 家族返回 None。
    #[test]
    fn shell_ids_from_paths_handle_windows_and_exe_suffixes() {
        assert_eq!(shell_id_from_path("/bin/zsh"), Some("zsh"));
        assert_eq!(shell_id_from_path("/opt/homebrew/bin/fish"), Some("fish"));
        assert_eq!(shell_id_from_path("C:\\Tools\\nu.exe"), Some("nu"));
        assert_eq!(shell_id_from_path("/usr/local/bin/python3"), None);
        assert_eq!(shell_id_from_path(""), None);
    }

    /// 登录参数表只产出白名单参数；sh/dash 与 Windows pwsh 不支持登录模式。
    #[test]
    fn uses_only_known_platform_safe_login_arguments() {
        assert_eq!(
            get_terminal_shell_login_args("/bin/bash", Platform::Posix),
            Some(vec!["-l".to_string()])
        );
        assert_eq!(
            get_terminal_shell_login_args("/opt/homebrew/bin/fish", Platform::Posix),
            Some(vec!["--login".to_string()])
        );
        assert_eq!(
            get_terminal_shell_login_args("/usr/bin/nu", Platform::Posix),
            Some(vec!["--login".to_string()])
        );
        assert_eq!(
            get_terminal_shell_login_args("/usr/bin/pwsh", Platform::Posix),
            Some(vec!["-Login".to_string()])
        );
        assert_eq!(
            get_terminal_shell_login_args(
                "C:\\Program Files\\PowerShell\\7\\pwsh.exe",
                Platform::Windows
            ),
            None
        );
        assert_eq!(
            get_terminal_shell_login_args("/bin/dash", Platform::Posix),
            None
        );
        assert_eq!(
            get_terminal_shell_login_args("/bin/sh", Platform::Posix),
            None
        );
    }

    /// 裸名 shell 经增强 PATH 搜索被发现，且搜索确实发生在该 PATH 上。
    #[test]
    fn discovers_shells_from_the_augmented_pty_path() {
        let (resolver, recorder) = test_deps(
            Platform::Posix,
            HashMap::new(),
            vec!["/augmented/bin/fish"],
            "/augmented/bin",
        );
        let shells = resolver.list();
        assert!(shells.iter().any(|shell| shell
            == &ShellInfo {
                id: "fish".to_string(),
                name: "fish".to_string(),
                executable: Some("/augmented/bin/fish".to_string()),
                supports_login: true,
            }));
        let searches = recorder.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            searches
                .iter()
                .any(|(name, path)| name == "fish" && path == "/augmented/bin")
        );
    }

    /// Windows 下能发现路径安装的 bash.exe/nu.exe，并正确报告登录支持。
    #[test]
    fn discovers_supported_path_installed_shells_on_windows() {
        let (resolver, _) = test_deps(
            Platform::Windows,
            HashMap::new(),
            vec!["C:\\Tools\\bash.exe", "C:\\Tools\\nu.exe"],
            "C:\\Tools",
        );
        let shells = resolver.list();
        assert!(shells.iter().any(|shell| shell
            == &ShellInfo {
                id: "bash".to_string(),
                name: "bash".to_string(),
                executable: Some("C:\\Tools\\bash.exe".to_string()),
                supports_login: true,
            }));
        assert!(shells.iter().any(|shell| shell
            == &ShellInfo {
                id: "nu".to_string(),
                name: "nu".to_string(),
                executable: Some("C:\\Tools\\nu.exe".to_string()),
                supports_login: true,
            }));
    }

    /// auto 解析时环境覆盖（OMPCHAMBER_TERMINAL_SHELL、SHELL）排在平台默认之前。
    #[test]
    fn uses_environment_overrides_before_platform_defaults_for_auto() {
        let (resolver, _) = test_deps(
            Platform::Posix,
            HashMap::from([
                ("OMPCHAMBER_TERMINAL_SHELL", "/custom/zsh"),
                ("SHELL", "/bin/bash"),
            ]),
            vec!["/custom/zsh", "/bin/bash"],
            "/augmented/bin",
        );
        assert_eq!(
            resolver.resolve(Some("auto")).unwrap(),
            ResolvedShell {
                id: "auto".to_string(),
                executables: vec!["/custom/zsh".to_string(), "/bin/bash".to_string()]
            }
        );
    }

    /// /etc/shells 配置行进入发现结果（注释与空行忽略），非 shell 家族不出现。
    #[test]
    fn reads_configured_shells_from_etc_shells_when_available() {
        let executables: Vec<String> = vec![
            "/bin/zsh".to_string(),
            "/bin/bash".to_string(),
            "/bin/sh".to_string(),
        ];
        let contents = "/bin/zsh\n/bin/bash\n/bin/false\r\n# comment\n";
        let contents = contents.to_string();
        let search_executables = executables.clone();
        let check_executables = executables.clone();
        let deps = ShellDeps {
            platform: Platform::Posix,
            env: Box::new(|_| None),
            build_augmented_path: Box::new(|| "/augmented/bin".to_string()),
            search_path_for: Box::new(move |name: &str, _: &str| {
                let candidate = format!("/bin/{name}");
                search_executables.contains(&candidate).then_some(candidate)
            }),
            is_executable: Box::new(move |candidate: &str| {
                check_executables.iter().any(|exe| exe == candidate)
            }),
            read_etc_shells: Box::new(move || Some(contents.clone())),
        };
        let resolver = ShellResolver::new(deps);
        let shells = resolver.list();
        // zsh/bash/sh support login per the login-args table; /bin/false is not
        // a known shell family and never appears.
        assert!(
            shells
                .iter()
                .any(|shell| shell.id == "zsh" && shell.supports_login)
        );
        assert!(
            shells
                .iter()
                .any(|shell| shell.id == "bash" && shell.supports_login)
        );
        assert!(
            shells
                .iter()
                .any(|shell| shell.id == "sh" && !shell.supports_login)
        );
        assert!(!shells.iter().any(|shell| shell.id == "false"));
        assert_eq!(shells[0].id, "auto");
    }

    /// 显式解析可用 shell、对不可用/非法偏好报对应错误，None 偏好等价 auto。
    #[test]
    fn resolves_explicit_shells_and_reports_unavailable_ones() {
        let (resolver, _) = test_deps(
            Platform::Posix,
            HashMap::new(),
            vec!["/bin/sh"],
            "/augmented/bin",
        );
        assert_eq!(
            resolver.resolve(Some("sh")).unwrap(),
            ResolvedShell {
                id: "sh".to_string(),
                executables: vec!["/bin/sh".to_string()]
            }
        );
        assert_eq!(
            resolver.resolve(Some("fish")).unwrap_err(),
            ShellError("Terminal shell \"fish\" is not available".to_string())
        );
        assert_eq!(
            resolver.resolve(Some("zsh -c whoami")).unwrap_err(),
            ShellError("Invalid terminal shell".to_string())
        );
        // Null preference resolves like JS `preference ?? 'auto'`.
        assert_eq!(resolver.resolve(None).unwrap().id, "auto");
    }
}
