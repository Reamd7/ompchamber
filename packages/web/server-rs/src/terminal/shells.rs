//! Port of `server/lib/terminal/shells.js`.
//!
//! Discovers executable shell families and resolves the persisted shell ID.
//! No command strings or arguments are ever accepted — only well-known shell
//! ids and executable paths. All environment/filesystem access is injected
//! (`createTerminalShellResolver` deps), mirroring the JS seam used by tests.

use std::collections::HashSet;

/// Known shell families, in discovery order.
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

pub fn is_terminal_shell_id(value: &str) -> bool {
    TERMINAL_SHELL_IDS.contains(&value)
}

/// `normalizeTerminalShell`: `'auto'` or a known id, else `None`.
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

const SHELL_LABELS: [(&str, &str); 3] = [
    ("pwsh", "PowerShell"),
    ("powershell", "Windows PowerShell"),
    ("cmd", "Command Prompt"),
];

fn shell_label(id: &str) -> &str {
    SHELL_LABELS
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, label)| *label)
        .unwrap_or(id)
}

/// `process.platform` stand-in for cross-platform tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Posix,
}

impl Platform {
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
pub struct ShellDeps {
    pub platform: Platform,
    pub env: Box<dyn Fn(&str) -> Option<String> + Send + Sync>,
    pub build_augmented_path: Box<dyn Fn() -> String + Send + Sync>,
    pub search_path_for: Box<dyn Fn(&str, &str) -> Option<String> + Send + Sync>,
    pub is_executable: Box<dyn Fn(&str) -> bool + Send + Sync>,
    /// `/etc/shells` contents (Posix only; `None` skips configured shells).
    pub read_etc_shells: Box<dyn Fn() -> Option<String> + Send + Sync>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellInfo {
    pub id: String,
    pub name: String,
    pub executable: Option<String>,
    pub supports_login: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedShell {
    pub id: String,
    pub executables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellError(pub String);

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub struct ShellResolver {
    deps: ShellDeps,
}

impl ShellResolver {
    pub fn new(deps: ShellDeps) -> Self {
        Self { deps }
    }

    fn env(&self, name: &str) -> Option<String> {
        (self.deps.env)(name)
    }

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
    pub fn augmented_path(&self) -> String {
        (self.deps.build_augmented_path)()
    }

    /// `resolve`: `None` preference (JS `preference ?? 'auto'`) means auto.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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

    #[test]
    fn shell_ids_from_paths_handle_windows_and_exe_suffixes() {
        assert_eq!(shell_id_from_path("/bin/zsh"), Some("zsh"));
        assert_eq!(shell_id_from_path("/opt/homebrew/bin/fish"), Some("fish"));
        assert_eq!(shell_id_from_path("C:\\Tools\\nu.exe"), Some("nu"));
        assert_eq!(shell_id_from_path("/usr/local/bin/python3"), None);
        assert_eq!(shell_id_from_path(""), None);
    }

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
