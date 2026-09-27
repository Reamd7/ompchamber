//! Port of `server/lib/opencode/env-runtime.js` plus the PATH-augmentation
//! half of `server/lib/opencode/server-utils-runtime.js`
//! (`buildAugmentedPath` / `buildManagedOpenCodePath` / `getEnvValue` /
//! `buildWindowsManagedToolchainPath`).
//!
//! JS `createOpenCodeEnvRuntime(deps)` closes over a mutable shared `state`
//! object and the real `process.env`. The Rust port keeps that shape testable:
//! - the shared state lives in [`EnvRuntimeState`] behind a lock;
//! - `process.env` mutations (`prependToPath`, `process.env.OPENCODE_BINARY =
//!   resolved`, the AppImage `ARGV0` delete) are modeled as an overlay
//!   ([`EnvRuntime::effective_env`]) instead of mutating the real process
//!   environment (`std::env::set_var` is unsafe while the server is
//!   multi-threaded);
//! - the JS `spawnSync` dependency is an injectable async seam ([`SpawnFn`])
//!   so login-shell probes can be faked; the production seam still enforces
//!   the JS `SHELL_PROBE_TIMEOUT_MS` bound by killing the probe;
//! - `process.platform` reads branch on the injectable [`Platform`] (tests
//!   exercise the win32 flow on any host, like the JS tests that override
//!   `process.platform`), while `path`-module behavior (separator, delimiter,
//!   lexical resolve) stays host-bound exactly like the node `path` builtin
//!   the JS tests ran under.
//!
//! Known gaps (see PORT-MANIFEST.md):
//! - `process.resourcesPath` (Electron-only) does not exist for the bundled
//!   CLI candidate list; only `OMPCHAMBER_BUNDLED_OPENCODE_CLI_DIR` is read.
//! - JS `bundledOpenCodeCliFallback` (and the `resolveBundledOpenCodeCliPath`
//!   / `isWindowsOpenCodeDesktopAppPath` helpers only it called) is dead code
//!   in the JS module — defined, never called — and is intentionally not
//!   ported.
//! - The Bun-only libc `unsetenv` half of `clearAppImageArgv0FromProcessEnv`
//!   is a JS-runtime artifact; the Rust overlay delete covers the observable
//!   behavior (see `src/inherited_env.rs` for the PTY-side wrapper).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::engine_env::path_utils::{merge_path_values, path_looks_user_configured};

/// JS `SHELL_PROBE_TIMEOUT_MS`: login-shell probes source the user's rc files;
/// a slow or interactive rc must not hold startup hostage.
const SHELL_PROBE_TIMEOUT_MS: u64 = 5_000;

/// JS `WINDOWS_BATCH_EXTENSIONS`.
const WINDOWS_BATCH_EXTENSIONS: [&str; 3] = [".cmd", ".bat", ".com"];

/// `process.platform` as the JS module reads it. Injectable so the win32 flow
/// is testable on any host (the JS tests override `process.platform` the same
/// way while the node `path` builtin stays host-bound).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Unix,
    Windows,
}

pub fn host_platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unix
    }
}

/// Host path separator / delimiter — mirrors the node `path` module the JS
/// code ran under (`path.sep`, `path.delimiter`).
fn path_sep() -> &'static str {
    if cfg!(windows) { "\\" } else { "/" }
}

fn path_delim() -> char {
    if cfg!(windows) { ';' } else { ':' }
}

// ---------------------------------------------------------------------------
// Small host-path helpers (node `path` semantics, both separators accepted)
// ---------------------------------------------------------------------------

fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

fn basename(p: &str) -> String {
    let trimmed = p.trim_end_matches(is_sep);
    match trimmed.rfind(is_sep) {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

fn dirname(p: &str) -> String {
    let trimmed = p.trim_end_matches(is_sep);
    match trimmed.rfind(is_sep) {
        Some(0) => p[..1].to_string(),
        Some(i) => {
            let dir = p[..i].trim_end_matches(is_sep);
            if dir.is_empty() {
                p[..1].to_string()
            } else {
                dir.to_string()
            }
        }
        None => ".".to_string(),
    }
}

/// JS `path.extname`: extension of the final component, including the dot;
/// a leading-dot-only component (`.opencode`) has no extension.
fn extname(p: &str) -> String {
    let name = basename(p);
    match name.rfind('.') {
        Some(0) | None => String::new(),
        Some(i) => name[i..].to_string(),
    }
}

/// String join with the host separator (node `path.join` for our inputs).
fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.trim_end_matches(is_sep))
        .filter(|p| !p.is_empty())
        .collect::<Vec<&str>>()
        .join(path_sep())
}

/// JS `path.resolve(single)` — lexical absolutization + normalization (no
/// symlink resolution; that is `canonicalize` below).
fn lexical_resolve(p: &str) -> String {
    let (prefix, rest) = if p.len() >= 2 && p.as_bytes()[1] == b':' {
        (&p[..2], &p[2..])
    } else {
        ("", p)
    };
    let absolute = rest.starts_with('/') || rest.starts_with('\\');
    let base = if absolute {
        String::new()
    } else {
        std::env::current_dir()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let combined = format!(
        "{base}{}{rest}",
        if base.is_empty() { "" } else { path_sep() }
    );
    let mut stack: Vec<&str> = Vec::new();
    for part in combined.split(is_sep) {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    let body = stack.join(path_sep());
    if absolute || !prefix.is_empty() {
        format!("{prefix}{}{body}", path_sep())
    } else if body.is_empty() {
        ".".to_string()
    } else {
        body
    }
}

// ---------------------------------------------------------------------------
// Seams
// ---------------------------------------------------------------------------

/// The JS `spawnSync` dependency: `(program, args, options) -> { status, stdout }`.
/// `Ok(Err)` mirrors a thrown spawn error (ENOENT); `status: None` mirrors a
/// null status (timeout / killed by signal).
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct SpawnOutput {
    pub status: Option<i32>,
    pub stdout: String,
}

pub type SpawnFuture = Pin<Box<dyn Future<Output = std::io::Result<SpawnOutput>> + Send>>;
pub type SpawnFn = Arc<dyn Fn(String, Vec<String>, SpawnOptions) -> SpawnFuture + Send + Sync>;

/// Base environment read (`process.env` in JS). A snapshot function so the
/// case-insensitive [`EnvRuntime::get_env_value`] lookup can enumerate keys.
pub type EnvSource = Arc<dyn Fn() -> HashMap<String, String> + Send + Sync>;

/// The injectable `deps.homedir` (JS `os.homedir()`).
pub type HomeFn = Arc<dyn Fn() -> PathBuf + Send + Sync>;

fn real_spawn() -> SpawnFn {
    Arc::new(
        |program: String, args: Vec<String>, options: SpawnOptions| {
            Box::pin(async move {
                let mut command = tokio::process::Command::new(&program);
                command
                    .args(&args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    // A probe that overruns its timeout is abandoned; dropping the
                    // output future must not leak the child.
                    .kill_on_drop(true);
                let output_fut = command.output();
                let output = match options.timeout_ms {
                    Some(ms) => {
                        match tokio::time::timeout(Duration::from_millis(ms), output_fut).await {
                            Ok(result) => result?,
                            Err(_elapsed) => {
                                return Ok(SpawnOutput {
                                    status: None,
                                    stdout: String::new(),
                                });
                            }
                        }
                    }
                    None => output_fut.await?,
                };
                Ok(SpawnOutput {
                    status: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                })
            })
        },
    )
}

fn real_env_source() -> EnvSource {
    Arc::new(|| {
        std::env::vars_os()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.to_string_lossy().to_string(),
                )
            })
            .collect()
    })
}

fn real_home() -> HomeFn {
    Arc::new(|| crate::config::home_dir().unwrap_or_else(|| PathBuf::from(".")))
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Where the omp-host runtime (Bun) was found. Mirrors the strings the JS
/// writes into `state.resolvedOpencodeBinarySource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarySource {
    Env,
    Path,
    Fallback,
    Bundled,
    Unknown,
}

impl BinarySource {
    pub fn as_str(self) -> &'static str {
        match self {
            BinarySource::Env => "env",
            BinarySource::Path => "path",
            BinarySource::Fallback => "fallback",
            BinarySource::Bundled => "bundled",
            BinarySource::Unknown => "unknown",
        }
    }
}

/// The shared `state` object JS threads into `createOpenCodeEnvRuntime`.
/// `cached_login_shell_env_snapshot` keeps the JS tri-state: outer `None` is
/// "not probed yet", `Some(None)` is "probed, no snapshot available".
#[derive(Debug, Default)]
pub struct EnvRuntimeState {
    pub cached_login_shell_env_snapshot: Option<Option<HashMap<String, String>>>,
    pub resolved_opencode_binary: Option<String>,
    pub resolved_opencode_binary_source: Option<BinarySource>,
    pub resolved_node_binary: Option<String>,
    pub resolved_bun_binary: Option<String>,
    pub resolved_git_binary: Option<String>,
}

/// JS `process.env` mutations performed by this runtime, as an overlay:
/// `Some(value)` sets, `None` deletes (AppImage `ARGV0`).
type EnvOverrides = HashMap<String, Option<String>>;

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

pub struct EnvRuntime {
    state: Mutex<EnvRuntimeState>,
    overrides: Mutex<EnvOverrides>,
    env_source: EnvSource,
    spawn: SpawnFn,
    home: HomeFn,
    platform: Platform,
}

impl EnvRuntime {
    /// Process-wide runtime (index.js module-level state).
    pub fn shared() -> &'static EnvRuntime {
        static SHARED: std::sync::LazyLock<EnvRuntime> = std::sync::LazyLock::new(EnvRuntime::new);
        &SHARED
    }

    /// Number of env overrides currently applied (login-shell snapshot keys
    /// plus the ARGV0 delete).
    pub fn shell_env_key_count(&self) -> usize {
        self.lock_overrides()
            .values()
            .filter(|value| value.is_some())
            .count()
    }

    /// JS `resolvedBunBinary` state: only `ensureBunCliEnv` sets it (the shim
    /// runtime paths); the plain managed omp-host launch never does.
    pub fn resolved_bun_binary(&self) -> Option<String> {
        self.lock_state().resolved_bun_binary.clone()
    }

    /// JS `resolvedNodeBinary` state: only `ensureNodeCliEnv` sets it.
    pub fn resolved_node_binary(&self) -> Option<String> {
        self.lock_state().resolved_node_binary.clone()
    }

    /// Production runtime with real seams.
    pub fn new() -> Self {
        Self::with_seams(
            real_env_source(),
            real_spawn(),
            real_home(),
            host_platform(),
        )
    }

    /// Test/consumer constructor with injected seams (JS `createOpenCodeEnvRuntime(deps)`).
    pub fn with_seams(
        env_source: EnvSource,
        spawn: SpawnFn,
        home: HomeFn,
        platform: Platform,
    ) -> Self {
        Self {
            state: Mutex::new(EnvRuntimeState::default()),
            overrides: Mutex::new(EnvOverrides::new()),
            env_source,
            spawn,
            home,
            platform,
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, EnvRuntimeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_overrides(&self) -> std::sync::MutexGuard<'_, EnvOverrides> {
        self.overrides.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn platform(&self) -> Platform {
        self.platform
    }

    /// `process.env[name]` after this runtime's overlay.
    fn env_get(&self, name: &str) -> Option<String> {
        {
            let overrides = self.lock_overrides();
            if let Some(value) = overrides.get(name) {
                return value.clone();
            }
        }
        (self.env_source)().get(name).cloned()
    }

    /// JS `getEnvValue`: exact key first, then a case-insensitive fallback
    /// (Windows env casing).
    fn get_env_value(&self, name: &str) -> String {
        let env = self.effective_env();
        if let Some(value) = env.get(name) {
            return value.clone();
        }
        env.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    /// Base environment with this runtime's overlay applied. This is the env
    /// the engine spawn should use (wiring point for `engine.rs`).
    pub fn effective_env(&self) -> HashMap<String, String> {
        let mut env = (self.env_source)();
        let overrides = self.lock_overrides();
        for (key, value) in overrides.iter() {
            match value {
                Some(v) => {
                    env.insert(key.clone(), v.clone());
                }
                None => {
                    env.remove(key);
                }
            }
        }
        env
    }

    fn set_override(&self, key: &str, value: Option<String>) {
        self.lock_overrides().insert(key.to_string(), value);
    }

    /// Test seam for the shared `state.cachedLoginShellEnvSnapshot` the JS
    /// tests pre-set.
    pub fn set_cached_login_shell_env_snapshot(&self, snapshot: Option<HashMap<String, String>>) {
        self.lock_state().cached_login_shell_env_snapshot = Some(snapshot);
    }

    pub fn resolved_opencode_binary_source(&self) -> Option<BinarySource> {
        self.lock_state().resolved_opencode_binary_source
    }

    fn set_resolved_opencode_binary_source(&self, source: BinarySource) {
        self.lock_state().resolved_opencode_binary_source = Some(source);
    }

    // -- JS isExecutable ----------------------------------------------------

    pub fn is_executable(&self, file_path: &str) -> bool {
        let Ok(metadata) = std::fs::metadata(file_path) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        if self.platform() == Platform::Windows {
            let ext = extname(file_path).to_lowercase();
            if ext.is_empty() {
                return true;
            }
            return matches!(ext.as_str(), ".exe" | ".cmd" | ".bat" | ".com");
        }
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    // -- JS resolveWindowsExecutablePath ------------------------------------

    fn pathext_variants(&self) -> Vec<String> {
        let raw = self
            .env_get("PATHEXT")
            .filter(|v| !v.is_empty())
            .or_else(|| self.env_get("PathExt").filter(|v| !v.is_empty()))
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
        let mut variants = Vec::new();
        for piece in raw.split(';') {
            let normalized = piece.trim();
            if normalized.is_empty() {
                continue;
            }
            variants.push(normalized.to_string());
        }
        variants
    }

    fn resolve_windows_executable_path(&self, candidate: &str) -> Option<String> {
        if self.platform() != Platform::Windows {
            return Some(candidate.to_string());
        }
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            return None;
        }
        if !extname(trimmed).is_empty() {
            return if self.is_executable(trimmed) {
                Some(trimmed.to_string())
            } else {
                None
            };
        }
        for normalized_ext in self.pathext_variants() {
            let with_ext = if normalized_ext.starts_with('.') {
                format!("{trimmed}{normalized_ext}")
            } else {
                format!("{trimmed}.{normalized_ext}")
            };
            if self.is_executable(&with_ext) {
                return Some(with_ext);
            }
        }
        if self.is_executable(trimmed) {
            Some(trimmed.to_string())
        } else {
            None
        }
    }

    // -- JS searchPathFor ---------------------------------------------------

    /// JS `searchPathFor(binaryName, searchPath)`. Does not mutate anything.
    pub fn search_path_for(&self, binary_name: &str, search_path: &str) -> Option<String> {
        let trimmed = binary_name.trim();
        if trimmed.is_empty() {
            return None;
        }

        let parts: Vec<&str> = search_path
            .split(path_delim())
            .filter(|s| !s.is_empty())
            .collect();
        let mut candidate_names: Vec<String> = Vec::new();

        if self.platform() == Platform::Windows && extname(trimmed).is_empty() {
            for normalized_ext in self.pathext_variants() {
                let candidate_name = if normalized_ext.starts_with('.') {
                    format!("{trimmed}{normalized_ext}")
                } else {
                    format!("{trimmed}.{normalized_ext}")
                };
                if !candidate_names
                    .iter()
                    .any(|existing| existing.to_lowercase() == candidate_name.to_lowercase())
                {
                    candidate_names.push(candidate_name);
                }
            }
        }
        candidate_names.push(trimmed.to_string());

        for dir in parts {
            for candidate_name in &candidate_names {
                let candidate = join(&[dir, candidate_name]);
                if self.is_executable(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    /// JS `searchPathFor(binaryName)` with the default `process.env.PATH`.
    pub fn search_path_for_env(&self, binary_name: &str) -> Option<String> {
        let search_path = self.env_get("PATH").unwrap_or_default();
        self.search_path_for(binary_name, &search_path)
    }

    // -- JS prependToPath ---------------------------------------------------

    fn prepend_to_path(&self, dir: &str) {
        let trimmed = dir.trim();
        if trimmed.is_empty() {
            return;
        }
        let current = self.env_get("PATH").unwrap_or_default();
        let parts: Vec<&str> = current
            .split(path_delim())
            .filter(|s| !s.is_empty())
            .collect();
        if parts.contains(&trimmed) {
            return;
        }
        let mut entries = vec![trimmed.to_string()];
        entries.extend(parts.iter().map(|p| p.to_string()));
        self.set_override("PATH", Some(entries.join(&path_delim().to_string())));
    }

    // -- Login-shell env snapshot -------------------------------------------

    /// JS `parseNullSeparatedEnvSnapshot` (pure). `windows` mirrors the
    /// `process.platform === 'win32'` PATH-casing fixup.
    pub fn parse_null_separated_env_snapshot(
        raw: &str,
        windows: bool,
    ) -> Option<HashMap<String, String>> {
        if raw.is_empty() {
            return None;
        }
        let mut result: HashMap<String, String> = HashMap::new();
        for entry in raw.split('\0') {
            if entry.is_empty() {
                continue;
            }
            if let Some(idx) = entry.find('=') {
                if idx == 0 {
                    continue;
                }
                result.insert(entry[..idx].to_string(), entry[idx + 1..].to_string());
            }
        }
        if result.is_empty() {
            return None;
        }
        if windows && !result.contains_key("PATH") {
            let path_value = result
                .iter()
                .find(|(key, _)| key.to_lowercase() == "path")
                .map(|(_, value)| value.clone());
            if let Some(value) = path_value {
                result.insert("PATH".to_string(), value);
            }
        }
        Some(result)
    }

    /// JS `getWindowsShellEnvSnapshot`: PowerShell candidates, then the
    /// `cmd /c set` fallback.
    async fn get_windows_shell_env_snapshot(&self) -> Option<HashMap<String, String>> {
        let ps_script = [
            "$entries = [ordered]@{}",
            "Get-ChildItem Env: | ForEach-Object { $entries[$_.Name] = $_.Value }",
            "$pathValues = @([Environment]::GetEnvironmentVariable('Path', 'Machine'), [Environment]::GetEnvironmentVariable('Path', 'User'), [Environment]::GetEnvironmentVariable('Path', 'Process')) | Where-Object { $_ }",
            "if ($pathValues.Count -gt 0) { $entries['Path'] = ($pathValues -join ';') }",
            "$entries.GetEnumerator() | ForEach-Object { [Console]::Out.Write($_.Name); [Console]::Out.Write('='); [Console]::Out.Write($_.Value); [Console]::Out.Write([char]0) }",
        ]
        .join("; ");

        let system_root = self
            .env_get("SystemRoot")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "C:\\Windows".to_string());
        let powershell_candidates = [
            "pwsh.exe".to_string(),
            "powershell.exe".to_string(),
            join(&[
                &system_root,
                "System32",
                "WindowsPowerShell",
                "v1.0",
                "powershell.exe",
            ]),
        ];

        for shell_path in &powershell_candidates {
            let result = (self.spawn)(
                shell_path.clone(),
                vec![
                    "-NoLogo".to_string(),
                    "-Command".to_string(),
                    ps_script.clone(),
                ],
                SpawnOptions::default(),
            )
            .await;
            if let Ok(output) = result
                && output.status == Some(0)
                && let Some(parsed) = Self::parse_null_separated_env_snapshot(&output.stdout, true)
            {
                return Some(parsed);
            }
        }

        let comspec = self
            .env_get("ComSpec")
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "cmd.exe".to_string());
        let result = (self.spawn)(
            comspec,
            vec![
                "/d".to_string(),
                "/s".to_string(),
                "/c".to_string(),
                "set".to_string(),
            ],
            SpawnOptions::default(),
        )
        .await;
        if let Ok(output) = result
            && output.status == Some(0)
            && !output.stdout.is_empty()
        {
            let nul_separated = output.stdout.replace("\r\n", "\0").replace('\n', "\0");
            return Self::parse_null_separated_env_snapshot(&nul_separated, true);
        }
        None
    }

    /// JS `getLoginShellEnvSnapshot` — probes once, then caches (including
    /// the failure tri-state).
    pub async fn get_login_shell_env_snapshot(&self) -> Option<HashMap<String, String>> {
        if let Some(cached) = self.lock_state().cached_login_shell_env_snapshot.clone() {
            return cached;
        }

        let snapshot = if self.platform() == Platform::Windows {
            self.get_windows_shell_env_snapshot().await
        } else {
            self.get_unix_login_shell_env_snapshot().await
        };
        self.lock_state().cached_login_shell_env_snapshot = Some(snapshot.clone());
        snapshot
    }

    async fn get_unix_login_shell_env_snapshot(&self) -> Option<HashMap<String, String>> {
        let mut shell_candidates: Vec<String> = Vec::new();
        if let Some(shell) = self.env_get("SHELL")
            && !shell.is_empty()
        {
            shell_candidates.push(shell);
        }
        shell_candidates.extend(
            ["/bin/zsh", "/bin/bash", "/bin/sh"]
                .iter()
                .map(|s| s.to_string()),
        );

        for shell_path in shell_candidates {
            if !self.is_executable(&shell_path) {
                continue;
            }
            let result = (self.spawn)(
                shell_path.clone(),
                vec!["-lic".to_string(), "env -0".to_string()],
                SpawnOptions {
                    timeout_ms: Some(SHELL_PROBE_TIMEOUT_MS),
                },
            )
            .await;
            let Ok(output) = result else {
                continue;
            };
            if output.status != Some(0) {
                continue;
            }
            if let Some(parsed) = Self::parse_null_separated_env_snapshot(&output.stdout, false) {
                return Some(parsed);
            }
        }
        None
    }

    /// JS `applyLoginShellEnvSnapshot`: always clears AppImage `ARGV0`, fills
    /// unset vars from the snapshot, and merges the shell PATH ahead of the
    /// current one.
    pub async fn apply_login_shell_env_snapshot(&self) {
        // Always clear AppImage ARGV0, even when no login-shell snapshot is
        // available (#2588).
        self.set_override("ARGV0", None);

        let snapshot = match self.get_login_shell_env_snapshot().await {
            Some(snapshot) => snapshot,
            None => return,
        };

        let skip_keys = ["PWD", "OLDPWD", "SHLVL", "_", "ARGV0"];
        for (key, value) in &snapshot {
            if skip_keys.contains(&key.as_str()) {
                continue;
            }
            if let Some(existing) = self.env_get(key)
                && !existing.is_empty()
            {
                continue;
            }
            self.set_override(key, Some(value.clone()));
        }

        let current_path = self.env_get("PATH").unwrap_or_default();
        let shell_path = snapshot.get("PATH").cloned().unwrap_or_default();
        if shell_path.is_empty() {
            return;
        }
        self.set_override(
            "PATH",
            Some(merge_path_values(&shell_path, &current_path, path_delim())),
        );
    }

    // -- Bundled CLI ----------------------------------------------------------

    fn bundled_open_code_cli_candidates(&self) -> Vec<String> {
        let names: [&str; 1] = if self.platform() == Platform::Windows {
            ["opencode.exe"]
        } else {
            ["opencode"]
        };
        // JS also considers `process.resourcesPath/opencode-cli`; that is an
        // Electron-only global the standalone server never has.
        let mut roots: Vec<String> = Vec::new();
        if let Some(dir) = self.env_get("OMPCHAMBER_BUNDLED_OPENCODE_CLI_DIR") {
            let trimmed = dir.trim();
            if !trimmed.is_empty() {
                roots.push(trimmed.to_string());
            }
        }
        let mut candidates = Vec::new();
        for root in &roots {
            for name in names {
                candidates.push(join(&[root, name]));
            }
        }
        candidates
    }

    fn canonical_executable_path(&self, candidate: &str) -> Option<String> {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            return None;
        }
        match std::fs::canonicalize(trimmed) {
            Ok(real) => Some(real.to_string_lossy().to_string()),
            Err(_) => Some(lexical_resolve(trimmed)),
        }
    }

    pub fn is_bundled_open_code_cli_path(&self, candidate: &str) -> bool {
        let Some(canonical_candidate) = self.canonical_executable_path(candidate) else {
            return false;
        };
        self.bundled_open_code_cli_candidates()
            .iter()
            .any(|bundled| {
                self.canonical_executable_path(bundled)
                    .is_some_and(|canonical| canonical == canonical_candidate)
            })
    }

    // -- Binary resolution ----------------------------------------------------

    /// JS `resolveOpencodeCliPath` — resolves the RUNTIME that launches the
    /// managed omp host (Bun), not an opencode CLI. Kept under its historical
    /// name because the resolution snapshot, PATH augmentation, and settings
    /// plumbing all flow through it.
    pub fn resolve_opencode_cli_path(&self) -> Option<String> {
        let mut explicit: Vec<String> = Vec::new();
        for name in ["OMPCHAMBER_OMP_HOST_RUNTIME", "OPENCODE_BINARY"] {
            if let Some(value) = self.env_get(name) {
                let stripped = strip_wrapping_quotes(&value);
                if !stripped.is_empty() {
                    explicit.push(stripped);
                }
            }
        }
        for candidate in &explicit {
            if self.is_executable(candidate) {
                self.set_resolved_opencode_binary_source(BinarySource::Env);
                return Some(candidate.clone());
            }
        }

        if let Some(resolved) = self.search_path_for_env("bun") {
            self.set_resolved_opencode_binary_source(BinarySource::Path);
            return Some(resolved);
        }

        let home = (self.home)();
        let fallbacks: Vec<PathBuf> = if self.platform() == Platform::Windows {
            vec![home.join(".bun").join("bin").join("bun.exe")]
        } else {
            vec![
                home.join(".bun").join("bin").join("bun"),
                PathBuf::from("/opt/homebrew/bin/bun"),
                PathBuf::from("/usr/local/bin/bun"),
            ]
        };
        for candidate in &fallbacks {
            if let Some(candidate_str) = candidate.to_str()
                && self.is_executable(candidate_str)
            {
                self.set_resolved_opencode_binary_source(BinarySource::Fallback);
                return Some(candidate_str.to_string());
            }
        }

        None
    }

    /// Login shell probe for a single binary: `$SHELL -lic 'command -v X'`.
    async fn probe_shell_command_v(&self, binary: &str) -> Option<String> {
        let mut shells: Vec<String> = Vec::new();
        if let Some(shell) = self.env_get("SHELL")
            && !shell.is_empty()
        {
            shells.push(shell);
        }
        shells.extend(
            ["/bin/zsh", "/bin/bash", "/bin/sh"]
                .iter()
                .map(|s| s.to_string()),
        );
        for shell in shells {
            if !self.is_executable(&shell) {
                continue;
            }
            let result = (self.spawn)(
                shell,
                vec!["-lic".to_string(), format!("command -v {binary}")],
                SpawnOptions {
                    timeout_ms: Some(SHELL_PROBE_TIMEOUT_MS),
                },
            )
            .await;
            let Ok(output) = result else {
                continue;
            };
            if output.status != Some(0) {
                continue;
            }
            // JS `.trim().split(/\s+/).pop()`: split_whitespace already drops
            // the empties a bare split would produce.
            let found = output.stdout.split_whitespace().last()?;
            if self.is_executable(found) {
                return Some(found.to_string());
            }
        }
        None
    }

    /// Windows `where <binary>` probe.
    async fn probe_where(&self, binary: &str) -> Option<String> {
        let result = (self.spawn)(
            "where".to_string(),
            vec![binary.to_string()],
            SpawnOptions::default(),
        )
        .await;
        let Ok(output) = result else {
            return None;
        };
        if output.status != Some(0) {
            return None;
        }
        output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .find(|line| self.is_executable(line))
            .map(str::to_string)
    }

    pub async fn resolve_node_cli_path(&self) -> Option<String> {
        let mut explicit: Vec<String> = Vec::new();
        for name in ["NODE_BINARY", "OMPCHAMBER_NODE_BINARY"] {
            if let Some(value) = self.env_get(name) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    explicit.push(trimmed.to_string());
                }
            }
        }
        for candidate in &explicit {
            if self.is_executable(candidate) {
                return Some(candidate.clone());
            }
        }
        if let Some(resolved) = self.search_path_for_env("node") {
            return Some(resolved);
        }
        // JS applies these "unix fallbacks" on every platform; off Unix they
        // simply never exist.
        for candidate in [
            "/opt/homebrew/bin/node",
            "/usr/local/bin/node",
            "/usr/bin/node",
            "/bin/node",
        ] {
            if self.is_executable(candidate) {
                return Some(candidate.to_string());
            }
        }

        if self.platform() == Platform::Windows {
            return self.probe_where("node").await;
        }
        self.probe_shell_command_v("node").await
    }

    pub async fn resolve_bun_cli_path(&self) -> Option<String> {
        let mut explicit: Vec<String> = Vec::new();
        for name in ["BUN_BINARY", "OMPCHAMBER_BUN_BINARY"] {
            if let Some(value) = self.env_get(name) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    explicit.push(trimmed.to_string());
                }
            }
        }
        for candidate in &explicit {
            if self.is_executable(candidate) {
                return Some(candidate.clone());
            }
        }
        if let Some(resolved) = self.search_path_for_env("bun") {
            return Some(resolved);
        }
        let home = (self.home)();
        for candidate in [
            home.join(".bun").join("bin").join("bun"),
            PathBuf::from("/opt/homebrew/bin/bun"),
            PathBuf::from("/usr/local/bin/bun"),
            PathBuf::from("/usr/bin/bun"),
            PathBuf::from("/bin/bun"),
        ] {
            if let Some(candidate_str) = candidate.to_str()
                && self.is_executable(candidate_str)
            {
                return Some(candidate_str.to_string());
            }
        }

        if self.platform() == Platform::Windows {
            let user_profile = self
                .env_get("USERPROFILE")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or(home);
            for candidate in [
                user_profile.join(".bun").join("bin").join("bun.exe"),
                user_profile.join(".bun").join("bin").join("bun.cmd"),
            ] {
                if let Some(candidate_str) = candidate.to_str()
                    && self.is_executable(candidate_str)
                {
                    return Some(candidate_str.to_string());
                }
            }
            return self.probe_where("bun").await;
        }
        self.probe_shell_command_v("bun").await
    }

    pub async fn ensure_bun_cli_env(&self) -> Option<String> {
        if let Some(cached) = self.lock_state().resolved_bun_binary.clone() {
            return Some(cached);
        }
        let resolved = self.resolve_bun_cli_path().await?;
        self.prepend_to_path(&dirname(&resolved));
        self.lock_state().resolved_bun_binary = Some(resolved.clone());
        Some(resolved)
    }

    pub async fn ensure_node_cli_env(&self) -> Option<String> {
        if let Some(cached) = self.lock_state().resolved_node_binary.clone() {
            return Some(cached);
        }
        let resolved = self.resolve_node_cli_path().await?;
        self.prepend_to_path(&dirname(&resolved));
        self.lock_state().resolved_node_binary = Some(resolved.clone());
        Some(resolved)
    }

    // -- JS normalizeExecutableCandidate -------------------------------------

    fn normalize_executable_candidate(&self, value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return None;
        }
        if self.platform() == Platform::Windows {
            self.resolve_windows_executable_path(trimmed)
        } else if self.is_executable(trimmed) {
            Some(trimmed.to_string())
        } else {
            None
        }
    }

    // -- Windows native opencode package resolution ---------------------------

    fn windows_native_opencode_package_names() -> Vec<&'static str> {
        // TEMPORARY WORKAROUND — Windows ARM64: native opencode.exe fails with
        // a Bun FFI/TinyCC dlopen error; the bundler ships x64-baseline
        // instead, so the resolver looks for the same package here.
        match std::env::consts::ARCH {
            "aarch64" => vec!["opencode-windows-x64-baseline", "opencode-windows-x64"],
            "x86_64" => vec!["opencode-windows-x64-baseline", "opencode-windows-x64"],
            _ => Vec::new(),
        }
    }

    fn resolve_native_opencode_binary_from_node_modules(
        &self,
        node_modules_dir: Option<&str>,
    ) -> Option<String> {
        let node_modules_dir = node_modules_dir?.trim();
        if node_modules_dir.is_empty() {
            return None;
        }

        let package_shim = join(&[node_modules_dir, "opencode-ai", "bin", "opencode.exe"]);
        if self.is_executable(&package_shim) {
            return Some(package_shim);
        }

        for package_name in Self::windows_native_opencode_package_names() {
            let candidates = [
                join(&[node_modules_dir, package_name, "bin", "opencode.exe"]),
                join(&[
                    node_modules_dir,
                    "opencode-ai",
                    "node_modules",
                    package_name,
                    "bin",
                    "opencode.exe",
                ]),
            ];
            for candidate in candidates {
                if self.is_executable(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    async fn resolve_opencode_node_launch_spec_from_node_modules(
        &self,
        node_modules_dir: Option<&str>,
    ) -> Option<ManagedLaunchSpec> {
        let node_modules_dir = node_modules_dir?.trim();
        if node_modules_dir.is_empty() {
            return None;
        }

        let launcher = join(&[node_modules_dir, "opencode-ai", "bin", "opencode"]);
        if !self.is_executable(&launcher) && !Path::new(&launcher).exists() {
            return None;
        }

        let node_binary = self
            .ensure_node_cli_env()
            .await
            .unwrap_or_else(|| "node".to_string());
        Some(ManagedLaunchSpec {
            binary: node_binary,
            args: vec![launcher],
            wrapper_type: WrapperType::NodeLauncher,
        })
    }

    fn resolve_node_modules_dir_from_cmd_wrapper(&self, wrapper_path: &str) -> Option<String> {
        if wrapper_path.is_empty() {
            return None;
        }
        let content = std::fs::read_to_string(wrapper_path).ok()?;
        let matched = find_opencode_ai_bin_ref(&content)?;
        let normalized = replace_runs_of_seps(&matched);
        let launcher_path = lexical_resolve(&join(&[&dirname(wrapper_path), &normalized]));
        let dir1 = dirname(&launcher_path);
        let dir2 = dirname(&dir1);
        Some(dirname(&dir2))
    }

    async fn resolve_opencode_node_modules_dir(&self, opencode_path: &str) -> Option<String> {
        let trimmed = opencode_path.trim();
        if trimmed.is_empty() {
            return None;
        }

        let normalized = lexical_resolve(trimmed);
        let lower = normalized.to_lowercase();
        let file_dir = dirname(&normalized);
        let sep = path_sep();
        let mut node_modules_candidates: Vec<String> = Vec::new();
        let push_candidate = |candidate: Option<String>, list: &mut Vec<String>| {
            if let Some(candidate) = candidate {
                let trimmed = candidate.trim();
                if !trimmed.is_empty() && !list.iter().any(|existing| existing == trimmed) {
                    list.push(trimmed.to_string());
                }
            }
        };

        if lower.contains(&format!("{sep}.bun{sep}bin{sep}opencode")) {
            let bun_root = dirname(&dirname(&normalized));
            push_candidate(
                Some(join(&[&bun_root, "install", "global", "node_modules"])),
                &mut node_modules_candidates,
            );
        }

        for suffix in [
            format!("{sep}node_modules{sep}.bin{sep}opencode"),
            format!("{sep}node_modules{sep}.bin{sep}opencode.cmd"),
            format!("{sep}node_modules{sep}.bin{sep}opencode.bat"),
            format!("{sep}node_modules{sep}.bin{sep}opencode.exe"),
        ] {
            if lower.ends_with(&suffix) {
                push_candidate(Some(dirname(&file_dir)), &mut node_modules_candidates);
            }
        }

        if lower.ends_with(&format!(
            "{sep}node_modules{sep}opencode-ai{sep}bin{sep}opencode"
        )) {
            push_candidate(
                Some(dirname(&dirname(&file_dir))),
                &mut node_modules_candidates,
            );
        }

        if basename(&file_dir).to_lowercase() == "npm" {
            push_candidate(
                Some(join(&[&file_dir, "node_modules"])),
                &mut node_modules_candidates,
            );
        }

        if WINDOWS_BATCH_EXTENSIONS.contains(&extname(&normalized).to_lowercase().as_str()) {
            push_candidate(
                self.resolve_node_modules_dir_from_cmd_wrapper(&normalized),
                &mut node_modules_candidates,
            );
        }

        for candidate in &node_modules_candidates {
            if self
                .resolve_native_opencode_binary_from_node_modules(Some(candidate))
                .is_some()
                || self
                    .resolve_opencode_node_launch_spec_from_node_modules(Some(candidate))
                    .await
                    .is_some()
            {
                return Some(candidate.clone());
            }
        }

        None
    }

    // -- JS resolveManagedOpenCodeLaunchSpec ----------------------------------

    pub async fn resolve_managed_open_code_launch_spec(
        &self,
        opencode_path: &str,
    ) -> ManagedLaunchSpec {
        let trimmed = opencode_path.trim();
        let fallback_binary = if trimmed.is_empty() {
            "opencode".to_string()
        } else {
            trimmed.to_string()
        };

        if self.platform() != Platform::Windows {
            return ManagedLaunchSpec {
                binary: fallback_binary,
                args: Vec::new(),
                wrapper_type: WrapperType::None,
            };
        }

        let ext = extname(&fallback_binary).to_lowercase();
        let mut candidate_paths = vec![fallback_binary.clone()];
        if WINDOWS_BATCH_EXTENSIONS.contains(&ext.as_str()) {
            let stem = &fallback_binary[..fallback_binary.len() - ext.len()];
            candidate_paths.push(format!("{stem}.exe"));
        }

        for candidate in &candidate_paths {
            let node_modules_dir = self.resolve_opencode_node_modules_dir(candidate).await;
            if let Some(native_binary) =
                self.resolve_native_opencode_binary_from_node_modules(node_modules_dir.as_deref())
            {
                return ManagedLaunchSpec {
                    wrapper_type: if native_binary == fallback_binary {
                        WrapperType::None
                    } else {
                        WrapperType::NativeWrapper
                    },
                    binary: native_binary,
                    args: Vec::new(),
                };
            }

            if let Some(spec) = self
                .resolve_opencode_node_launch_spec_from_node_modules(node_modules_dir.as_deref())
                .await
            {
                return spec;
            }

            match self.opencode_shim_interpreter(candidate) {
                Some(ShimInterpreter::Node) => {
                    return ManagedLaunchSpec {
                        binary: self
                            .ensure_node_cli_env()
                            .await
                            .unwrap_or_else(|| "node".to_string()),
                        args: vec![candidate.clone()],
                        wrapper_type: WrapperType::NodeShebang,
                    };
                }
                Some(ShimInterpreter::Bun) => {
                    return ManagedLaunchSpec {
                        binary: self
                            .ensure_bun_cli_env()
                            .await
                            .unwrap_or_else(|| "bun".to_string()),
                        args: vec![candidate.clone()],
                        wrapper_type: WrapperType::BunShebang,
                    };
                }
                None => {}
            }

            if let Some(direct_binary) = self.normalize_executable_candidate(candidate) {
                let direct_ext = extname(&direct_binary).to_lowercase();
                if WINDOWS_BATCH_EXTENSIONS.contains(&direct_ext.as_str()) {
                    return ManagedLaunchSpec {
                        binary: self
                            .env_get("ComSpec")
                            .filter(|v| !v.is_empty())
                            .unwrap_or_else(|| "cmd.exe".to_string()),
                        args: vec![
                            "/d".to_string(),
                            "/s".to_string(),
                            "/c".to_string(),
                            "call".to_string(),
                            direct_binary,
                        ],
                        wrapper_type: WrapperType::CmdWrapper,
                    };
                }
                return ManagedLaunchSpec {
                    wrapper_type: if direct_binary == fallback_binary {
                        WrapperType::None
                    } else {
                        WrapperType::ExecutableWrapper
                    },
                    binary: direct_binary,
                    args: Vec::new(),
                };
            }
        }

        // Final fallback: never hand a raw .cmd/.bat to spawn(shell:false) —
        // cmd shims need cmd.exe, and unquoted space-containing paths break.
        if WINDOWS_BATCH_EXTENSIONS.contains(&ext.as_str()) {
            return ManagedLaunchSpec {
                binary: self
                    .env_get("ComSpec")
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "cmd.exe".to_string()),
                args: vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    "call".to_string(),
                    fallback_binary,
                ],
                wrapper_type: WrapperType::CmdWrapper,
            };
        }

        ManagedLaunchSpec {
            binary: fallback_binary,
            args: Vec::new(),
            wrapper_type: WrapperType::None,
        }
    }

    // -- Shebangs -------------------------------------------------------------

    fn read_shebang(&self, opencode_path: &str) -> Option<String> {
        if opencode_path.is_empty() {
            return None;
        }
        use std::io::Read;
        let mut file = std::fs::File::open(opencode_path).ok()?;
        let mut buf = vec![0u8; 256];
        let bytes = file.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..bytes]).to_string();
        let first_line = head.split('\n').next().unwrap_or_default();
        let first_line = first_line.strip_suffix('\r').unwrap_or(first_line);
        if !first_line.starts_with("#!") {
            return None;
        }
        let shebang = first_line[2..].trim();
        if shebang.is_empty() {
            return None;
        }
        Some(shebang.to_string())
    }

    fn opencode_shim_interpreter(&self, opencode_path: &str) -> Option<ShimInterpreter> {
        let shebang = self.read_shebang(opencode_path)?;
        if contains_word_ci(&shebang, "node") {
            return Some(ShimInterpreter::Node);
        }
        if contains_word_ci(&shebang, "bun") {
            return Some(ShimInterpreter::Bun);
        }
        None
    }

    async fn ensure_opencode_shim_runtime(&self, opencode_path: &str) {
        match self.opencode_shim_interpreter(opencode_path) {
            Some(ShimInterpreter::Node) => {
                self.ensure_node_cli_env().await;
            }
            Some(ShimInterpreter::Bun) => {
                self.ensure_bun_cli_env().await;
            }
            None => {}
        }
    }

    // -- JS ensureOpencodeCliEnv ----------------------------------------------

    pub async fn ensure_opencode_cli_env(&self) -> Option<String> {
        let cached = self.lock_state().resolved_opencode_binary.clone();
        if let Some(cached) = cached {
            self.ensure_opencode_shim_runtime(&cached).await;
            return Some(cached);
        }

        let existing = self
            .env_get("OPENCODE_BINARY")
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if !existing.is_empty() && self.is_executable(&existing) {
            {
                let mut state = self.lock_state();
                state.resolved_opencode_binary = Some(existing.clone());
                if state.resolved_opencode_binary_source.is_none() {
                    state.resolved_opencode_binary_source = Some(BinarySource::Env);
                }
            }
            self.prepend_to_path(&dirname(&existing));
            self.ensure_opencode_shim_runtime(&existing).await;
            return Some(existing);
        }

        if let Some(resolved) = self.resolve_opencode_cli_path() {
            self.set_override("OPENCODE_BINARY", Some(resolved.clone()));
            self.prepend_to_path(&dirname(&resolved));
            self.ensure_opencode_shim_runtime(&resolved).await;
            {
                let mut state = self.lock_state();
                state.resolved_opencode_binary = Some(resolved.clone());
                if state.resolved_opencode_binary_source.is_none() {
                    state.resolved_opencode_binary_source = Some(BinarySource::Unknown);
                }
            }
            tracing::info!("Resolved omp host runtime: {resolved}");
            return Some(resolved);
        }

        None
    }

    // -- JS resolveGitBinaryForSpawn -------------------------------------------

    pub fn resolve_git_binary_for_spawn(&self) -> String {
        if self.platform() != Platform::Windows {
            return "git".to_string();
        }
        if let Some(cached) = self.lock_state().resolved_git_binary.clone() {
            return cached;
        }

        let mut explicit: Vec<String> = Vec::new();
        for name in ["GIT_BINARY", "OMPCHAMBER_GIT_BINARY"] {
            if let Some(value) = self.env_get(name) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    explicit.push(trimmed.to_string());
                }
            }
        }
        for candidate in &explicit {
            if self.is_executable(candidate) {
                self.lock_state().resolved_git_binary = Some(candidate.clone());
                return candidate.clone();
            }
        }

        let normalize_candidate = |candidate: Option<String>| -> Option<String> {
            let candidate = candidate?;
            let trimmed = strip_wrapping_quotes(&candidate);
            if trimmed.is_empty() {
                return None;
            }
            Some(trimmed)
        };

        let mut candidates: Vec<String> = Vec::new();
        for binary in ["git", "git.exe"] {
            if let Some(candidate) = normalize_candidate(self.search_path_for_env(binary))
                && self.is_executable(&candidate)
            {
                candidates.push(candidate);
            }
        }

        let mut program_roots: Vec<String> = Vec::new();
        for name in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            if let Some(value) = self.env_get(name) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    program_roots.push(trimmed.to_string());
                }
            }
        }
        for root in &program_roots {
            for rel in [
                vec!["Git", "cmd", "git.exe"],
                vec!["Git", "bin", "git.exe"],
                vec!["Git", "mingw64", "bin", "git.exe"],
                vec!["Programs", "Git", "cmd", "git.exe"],
                vec!["Programs", "Git", "bin", "git.exe"],
            ] {
                let candidate = join(&{
                    let mut parts = vec![root.as_str()];
                    parts.extend(rel.iter().copied());
                    parts
                });
                if let Some(normalized) = normalize_candidate(Some(candidate))
                    && self.is_executable(&normalized)
                {
                    candidates.push(normalized);
                }
            }
        }

        let preferred_exe = candidates
            .iter()
            .find(|candidate| candidate.to_lowercase().ends_with(".exe"))
            .cloned();
        let resolved = preferred_exe
            .or_else(|| candidates.first().cloned())
            .unwrap_or_else(|| "git.exe".to_string());
        self.lock_state().resolved_git_binary = Some(resolved.clone());
        resolved
    }

    // -- JS clearResolvedOpenCodeBinary ----------------------------------------

    pub fn clear_resolved_open_code_binary(&self) {
        self.lock_state().resolved_opencode_binary = None;
    }

    // -- server-utils-runtime.js PATH builders ----------------------------------

    /// JS `getLoginShellPath` (server/index.js): the snapshot's PATH when
    /// present and non-empty, else null.
    pub async fn get_login_shell_path(&self) -> Option<String> {
        let snapshot = self.get_login_shell_env_snapshot().await?;
        match snapshot.get("PATH") {
            Some(path) if !path.is_empty() => Some(path.clone()),
            _ => None,
        }
    }

    fn build_windows_managed_toolchain_path(&self) -> String {
        if self.platform() != Platform::Windows {
            return String::new();
        }

        let home = (self.home)();
        let home_str = home.to_string_lossy().to_string();
        let user_profile = {
            let value = self.get_env_value("USERPROFILE");
            if !value.is_empty() {
                value
            } else {
                home_str.clone()
            }
        };
        let app_data = {
            let value = self.get_env_value("APPDATA");
            if !value.is_empty() {
                value
            } else {
                join(&[&user_profile, "AppData", "Roaming"])
            }
        };
        let local_app_data = {
            let value = self.get_env_value("LOCALAPPDATA");
            if !value.is_empty() {
                value
            } else {
                join(&[&user_profile, "AppData", "Local"])
            }
        };
        let program_files = {
            let value = self.get_env_value("ProgramFiles");
            if !value.is_empty() {
                value
            } else {
                "C:\\Program Files".to_string()
            }
        };
        let program_files_x86 = self.get_env_value("ProgramFiles(x86)");
        let program_data = {
            let value = self.get_env_value("ProgramData");
            if !value.is_empty() {
                value
            } else {
                "C:\\ProgramData".to_string()
            }
        };
        let bun_install = self.get_env_value("BUN_INSTALL");
        let volta_home = self.get_env_value("VOLTA_HOME");
        let scoop = self.get_env_value("SCOOP");
        let scoop_global = self.get_env_value("SCOOP_GLOBAL");
        let pnpm_home = self.get_env_value("PNPM_HOME");

        let mut candidates: Vec<String> =
            vec![join(&[&app_data, "npm"]), join(&[&program_files, "nodejs"])];
        if !program_files_x86.is_empty() {
            candidates.push(join(&[&program_files_x86, "nodejs"]));
        }
        candidates.push(join(&[&local_app_data, "Programs", "nodejs"]));
        if !pnpm_home.is_empty() {
            candidates.push(pnpm_home);
        }
        candidates.push(join(&[&local_app_data, "pnpm"]));
        if !bun_install.is_empty() {
            candidates.push(join(&[&bun_install, "bin"]));
        }
        candidates.push(join(&[&user_profile, ".bun", "bin"]));
        if !volta_home.is_empty() {
            candidates.push(join(&[&volta_home, "bin"]));
        }
        candidates.push(join(&[&local_app_data, "Volta", "bin"]));
        candidates.push(join(&[&local_app_data, "Yarn", "bin"]));
        candidates.push(join(&[
            &local_app_data,
            "Yarn",
            "Data",
            "global",
            "node_modules",
            ".bin",
        ]));
        if !scoop.is_empty() {
            candidates.push(join(&[&scoop, "shims"]));
        }
        candidates.push(join(&[&user_profile, "scoop", "shims"]));
        if !scoop_global.is_empty() {
            candidates.push(join(&[&scoop_global, "shims"]));
        }
        candidates.push(join(&[&program_data, "chocolatey", "bin"]));
        candidates.push(join(&[&local_app_data, "Microsoft", "WindowsApps"]));
        candidates.push(join(&[&user_profile, ".opencode", "bin"]));
        candidates.push(join(&[&user_profile, ".local", "bin"]));

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut existing: Vec<String> = Vec::new();
        for candidate in candidates {
            let trimmed = candidate.trim().to_string();
            if trimmed.is_empty() {
                continue;
            }
            let normalized = trimmed.to_lowercase();
            if !seen.insert(normalized) {
                continue;
            }
            if Path::new(&trimmed).exists() {
                existing.push(trimmed);
            }
        }
        existing.join(&path_delim().to_string())
    }

    /// JS `buildAugmentedPath`: keep a user-configured process PATH ahead of
    /// the login-shell PATH; when the process PATH looks like a bare system
    /// default, prefer the login shell and append the process entries.
    pub async fn build_augmented_path(&self) -> String {
        let current_path = self.get_env_value("PATH");
        let login_shell_path = self.get_login_shell_path().await.unwrap_or_default();
        let home = (self.home)().to_string_lossy().to_string();
        let current_looks_user_configured =
            path_looks_user_configured(&current_path, &home, path_delim());
        let (primary, fallback) = if current_looks_user_configured {
            (current_path, login_shell_path)
        } else {
            (login_shell_path, current_path)
        };
        merge_path_values(&primary, &fallback, path_delim())
    }

    /// JS `buildManagedOpenCodePath`: the login-shell PATH first, then the
    /// process PATH, then the existing Windows package-manager directories.
    pub async fn build_managed_open_code_path(&self) -> String {
        let current_path = self.get_env_value("PATH");
        let login_shell_path = self.get_login_shell_path().await.unwrap_or_default();
        let base = merge_path_values(&login_shell_path, &current_path, path_delim());
        let toolchain = self.build_windows_managed_toolchain_path();
        merge_path_values(&base, &toolchain, path_delim())
    }
}

impl Default for EnvRuntime {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Launch spec shapes (JS resolveManagedOpenCodeLaunchSpec return value)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperType {
    /// JS `null`.
    None,
    NativeWrapper,
    NodeLauncher,
    NodeShebang,
    BunShebang,
    CmdWrapper,
    ExecutableWrapper,
}

impl WrapperType {
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            WrapperType::None => None,
            WrapperType::NativeWrapper => Some("native-wrapper"),
            WrapperType::NodeLauncher => Some("node-launcher"),
            WrapperType::NodeShebang => Some("node-shebang"),
            WrapperType::BunShebang => Some("bun-shebang"),
            WrapperType::CmdWrapper => Some("cmd-wrapper"),
            WrapperType::ExecutableWrapper => Some("executable-wrapper"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedLaunchSpec {
    pub binary: String,
    pub args: Vec<String>,
    pub wrapper_type: WrapperType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShimInterpreter {
    Node,
    Bun,
}

/// JS `stripWrappingQuotes` — strip a single wrapping quote pair.
pub fn strip_wrapping_quotes(value: &str) -> String {
    let trimmed = value.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[trimmed.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return trimmed[1..trimmed.len() - 1].trim().to_string();
        }
    }
    trimmed.to_string()
}

/// Case-insensitive ASCII substring search returning a byte index.
fn find_ascii_ci(haystack: &str, needle: &str, start: usize) -> Option<usize> {
    let hay = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.is_empty() || hay.len() < needle_bytes.len() {
        return None;
    }
    let mut i = start;
    while i + needle_bytes.len() <= hay.len() {
        if hay[i..i + needle_bytes.len()]
            .iter()
            .zip(needle_bytes)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            // The first matched byte equals an ASCII needle byte, so `i` is a
            // char boundary.
            return Some(i);
        }
        i += 1;
    }
    None
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// JS `/\bnode\b/i`-style whole-word, case-insensitive containment.
fn contains_word_ci(haystack: &str, word: &str) -> bool {
    let hay = haystack.as_bytes();
    let word_bytes = word.as_bytes();
    let mut i = 0;
    while i + word_bytes.len() <= hay.len() {
        if hay[i..i + word_bytes.len()]
            .iter()
            .zip(word_bytes)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            let before_ok = i == 0 || !is_word_byte(hay[i - 1]);
            let after = i + word_bytes.len();
            let after_ok = after >= hay.len() || !is_word_byte(hay[after]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Match `[\\/]+` at `pos`; returns the index after the separator run.
fn match_sep_run(text: &str, pos: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = pos;
    while i < bytes.len() && (bytes[i] == b'/' || bytes[i] == b'\\') {
        i += 1;
    }
    if i == pos { None } else { Some(i) }
}

/// Case-insensitive literal match at exactly `pos`; returns the end index.
fn match_lit_at(text: &str, pos: usize, lit: &str) -> Option<usize> {
    let end = pos + lit.len();
    let slice = text.get(pos..end)?;
    if slice.len() == lit.len()
        && slice
            .as_bytes()
            .iter()
            .zip(lit.as_bytes())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    {
        Some(end)
    } else {
        None
    }
}

/// JS `/node_modules[\\/]+opencode-ai[\\/]+bin[\\/]+opencode/i` — returns the
/// matched substring (original casing), like `match[0]`.
fn find_opencode_ai_bin_ref(content: &str) -> Option<String> {
    let mut search_from = 0;
    while let Some(start) = find_ascii_ci(content, "node_modules", search_from) {
        let mut pos = start + "node_modules".len();
        let mut ok = true;
        for part in ["opencode-ai", "bin", "opencode"] {
            match match_sep_run(content, pos)
                .and_then(|after_seps| match_lit_at(content, after_seps, part))
            {
                Some(next) => pos = next,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            return Some(content[start..pos].to_string());
        }
        search_from = start + 1;
    }
    None
}

/// Replace runs of `/` and `\` with the host separator (JS
/// `match[0].replace(/[\\/]+/g, path.sep)`).
fn replace_runs_of_seps(value: &str) -> String {
    let sep = path_sep();
    let mut out = String::with_capacity(value.len());
    let mut in_run = false;
    for c in value.chars() {
        if c == '/' || c == '\\' {
            if !in_run {
                out.push_str(sep);
                in_run = true;
            }
        } else {
            out.push(c);
            in_run = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-env-runtime-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    type SpawnCalls = Arc<Mutex<Vec<(String, Vec<String>, SpawnOptions)>>>;

    fn write_executable(path: &Path, contents: &str) -> String {
        std::fs::write(path, contents).expect("write executable");
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path.to_string_lossy().to_string()
    }

    fn env_of(map: &[(&str, &str)]) -> EnvSource {
        let owned: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(move || owned.clone())
    }

    fn home_of(dir: PathBuf) -> HomeFn {
        Arc::new(move || dir.clone())
    }

    fn noop_spawn() -> SpawnFn {
        Arc::new(|_program, _args, _options| {
            Box::pin(async {
                Ok(SpawnOutput {
                    status: Some(1),
                    stdout: String::new(),
                })
            })
        })
    }

    // -- parse_null_separated_env_snapshot -----------------------------------

    #[test]
    fn parses_null_separated_snapshot() {
        let parsed = EnvRuntime::parse_null_separated_env_snapshot(
            "PATH=/usr/bin:/bin\0HOME=/home/u\0\0BAD\0= novalue\0",
            false,
        )
        .expect("snapshot");
        assert_eq!(
            parsed.get("PATH").map(String::as_str),
            Some("/usr/bin:/bin")
        );
        assert_eq!(parsed.get("HOME").map(String::as_str), Some("/home/u"));
        assert!(!parsed.contains_key("BAD"));
    }

    #[test]
    fn empty_snapshot_is_none() {
        assert!(EnvRuntime::parse_null_separated_env_snapshot("", false).is_none());
        assert!(EnvRuntime::parse_null_separated_env_snapshot("\0\0", false).is_none());
    }

    #[test]
    fn windows_snapshot_fixes_path_casing() {
        let parsed = EnvRuntime::parse_null_separated_env_snapshot("Path=C:\\x\0A=1\0", true)
            .expect("snapshot");
        assert_eq!(parsed.get("PATH").map(String::as_str), Some("C:\\x"));
        // Off Windows the casing fixup does not apply.
        let unix = EnvRuntime::parse_null_separated_env_snapshot("Path=/x\0A=1\0", false)
            .expect("snapshot");
        assert!(!unix.contains_key("PATH"));
    }

    // -- searchPathFor ---------------------------------------------------------

    #[test]
    fn searches_explicit_path_without_mutating_env() {
        let default_dir = temp_dir("default-path");
        let explicit_dir = temp_dir("explicit-path");
        let binary = write_executable(&explicit_dir.join("custom-shell"), "#!/bin/sh\nexit 0\n");

        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", default_dir.to_str().unwrap())]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Unix,
        );

        assert_eq!(
            runtime.search_path_for("custom-shell", explicit_dir.to_str().unwrap()),
            Some(binary)
        );
        // No process-env mutation: the overlay stays empty.
        assert_eq!(
            runtime.env_get("PATH").as_deref(),
            Some(default_dir.to_str().unwrap())
        );
    }

    // -- applyLoginShellEnvSnapshot ---------------------------------------------

    #[tokio::test]
    async fn applies_snapshot_clears_argv0_and_fills_missing() {
        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", "/usr/bin"), ("ARGV0", "/app.AppImage")]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Unix,
        );
        let snapshot: HashMap<String, String> = [
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("ARGV0".to_string(), "/leaked.AppImage".to_string()),
            ("OMPCHAMBER_MARKER".to_string(), "1".to_string()),
            ("PWD".to_string(), "/should/skip".to_string()),
        ]
        .into_iter()
        .collect();
        runtime.set_cached_login_shell_env_snapshot(Some(snapshot));

        runtime.apply_login_shell_env_snapshot().await;

        let env = runtime.effective_env();
        assert!(!env.contains_key("ARGV0"));
        assert_eq!(env.get("OMPCHAMBER_MARKER").map(String::as_str), Some("1"));
        assert!(!env.contains_key("PWD"));
        // Shell PATH merges ahead of the current PATH.
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
    }

    #[tokio::test]
    async fn clears_argv0_even_without_snapshot() {
        let runtime = EnvRuntime::with_seams(
            env_of(&[("ARGV0", "/app.AppImage")]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Unix,
        );
        runtime.set_cached_login_shell_env_snapshot(None);

        runtime.apply_login_shell_env_snapshot().await;

        assert!(!runtime.effective_env().contains_key("ARGV0"));
    }

    #[tokio::test]
    async fn merges_shell_path_ahead_of_current() {
        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", "/usr/local/bin:/usr/bin")]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Unix,
        );
        runtime.set_cached_login_shell_env_snapshot(Some(
            [(
                "PATH".to_string(),
                "/shell/bin:/usr/bin:/extra/bin".to_string(),
            )]
            .into_iter()
            .collect(),
        ));

        runtime.apply_login_shell_env_snapshot().await;

        assert_eq!(
            runtime.effective_env().get("PATH").map(String::as_str),
            Some("/shell/bin:/usr/bin:/extra/bin:/usr/local/bin")
        );
    }

    // -- resolveOpencodeCliPath ---------------------------------------------------

    #[tokio::test]
    async fn resolves_omp_host_runtime_from_path() {
        let path_dir = temp_dir("path-bun");
        let path_binary = write_executable(&path_dir.join("bun"), "#!/bin/sh\nexit 0\n");
        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", path_dir.to_str().unwrap())]),
            noop_spawn(),
            home_of(temp_dir("empty-home")),
            Platform::Unix,
        );

        assert_eq!(runtime.resolve_opencode_cli_path(), Some(path_binary));
        assert_eq!(
            runtime.resolved_opencode_binary_source(),
            Some(BinarySource::Path)
        );
    }

    #[test]
    fn recognizes_bundled_cli_by_canonical_path() {
        let bundled_dir = temp_dir("bundled-opencode");
        let bundled_binary = write_executable(&bundled_dir.join("opencode"), "#!/bin/sh\nexit 0\n");
        let runtime = EnvRuntime::with_seams(
            env_of(&[(
                "OMPCHAMBER_BUNDLED_OPENCODE_CLI_DIR",
                bundled_dir.to_str().unwrap(),
            )]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Unix,
        );

        assert!(runtime.is_bundled_open_code_cli_path(&bundled_binary));
        assert!(
            !runtime.is_bundled_open_code_cli_path(bundled_dir.join("other").to_str().unwrap())
        );
    }

    #[test]
    fn explicit_binary_beats_bundled_cli() {
        let bundled_dir = temp_dir("bundled-opencode");
        let explicit_dir = temp_dir("explicit-opencode");
        let bundled_binary = write_executable(&bundled_dir.join("opencode"), "#!/bin/sh\nexit 0\n");
        let explicit_binary =
            write_executable(&explicit_dir.join("opencode"), "#!/bin/sh\nexit 0\n");
        let runtime = EnvRuntime::with_seams(
            env_of(&[
                (
                    "OMPCHAMBER_BUNDLED_OPENCODE_CLI_DIR",
                    bundled_dir.to_str().unwrap(),
                ),
                ("OPENCODE_BINARY", &explicit_binary),
            ]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Unix,
        );

        assert_eq!(runtime.resolve_opencode_cli_path(), Some(explicit_binary));
        assert_eq!(
            runtime.resolved_opencode_binary_source(),
            Some(BinarySource::Env)
        );
        assert_ne!(
            runtime.resolve_opencode_cli_path(),
            Some(bundled_binary.clone())
        );
    }

    #[test]
    fn falls_through_to_absolute_bun_fallbacks_or_none() {
        // The unix fallback list ends with two absolute paths the test
        // cannot control; assert whichever outcome the host dictates.
        fn host_bun_fallback() -> Option<&'static str> {
            ["/opt/homebrew/bin/bun", "/usr/local/bin/bun"]
                .into_iter()
                .find(|candidate| {
                    std::fs::metadata(candidate)
                        .map(|m| {
                            m.is_file() && {
                                #[cfg(unix)]
                                {
use crate::os_compat::PermissionsExt;
                                    m.permissions().mode() & 0o111 != 0
                                }
                                #[cfg(not(unix))]
                                {
                                    true
                                }
                            }
                        })
                        .unwrap_or(false)
                })
        }

        let empty_path = temp_dir("empty-path");
        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", empty_path.to_str().unwrap())]),
            noop_spawn(),
            home_of(temp_dir("empty-home")),
            Platform::Unix,
        );

        match host_bun_fallback() {
            Some(fallback) => {
                assert_eq!(
                    runtime.resolve_opencode_cli_path(),
                    Some(fallback.to_string())
                );
                assert_eq!(
                    runtime.resolved_opencode_binary_source(),
                    Some(BinarySource::Fallback)
                );
            }
            None => {
                assert_eq!(runtime.resolve_opencode_cli_path(), None);
                assert_eq!(runtime.resolved_opencode_binary_source(), None);
            }
        }
    }

    #[test]
    fn windows_desktop_app_is_not_auto_detected() {
        let local_app_data = temp_dir("localappdata");
        let desktop_dir = local_app_data.join("Programs").join("OpenCode");
        std::fs::create_dir_all(&desktop_dir).expect("mkdir");
        std::fs::write(desktop_dir.join("OpenCode.exe"), "").expect("write");
        let empty_path = temp_dir("empty-path");
        let runtime = EnvRuntime::with_seams(
            env_of(&[
                ("LOCALAPPDATA", local_app_data.to_str().unwrap()),
                ("PATH", empty_path.to_str().unwrap()),
                ("SystemRoot", temp_dir("empty-systemroot").to_str().unwrap()),
            ]),
            noop_spawn(),
            home_of(temp_dir("empty-home")),
            Platform::Windows,
        );

        assert_eq!(runtime.resolve_opencode_cli_path(), None);
    }

    #[test]
    fn windows_path_resolution_ahead_of_home_fallback() {
        let path_dir = temp_dir("cli");
        std::fs::write(path_dir.join("bun.exe"), "").expect("write");
        let runtime = EnvRuntime::with_seams(
            env_of(&[
                ("PATH", path_dir.to_str().unwrap()),
                // Pin PATHEXT so the resolution does not depend on the host
                // filesystem's case sensitivity across default extensions.
                ("PATHEXT", ".exe"),
            ]),
            noop_spawn(),
            home_of(temp_dir("empty-home")),
            Platform::Windows,
        );

        assert_eq!(
            runtime.resolve_opencode_cli_path(),
            Some(path_dir.join("bun.exe").to_string_lossy().to_string())
        );
        assert_eq!(
            runtime.resolved_opencode_binary_source(),
            Some(BinarySource::Path)
        );
    }

    #[tokio::test]
    async fn wsl_fallback_paths_are_not_used() {
        let dir = temp_dir("wsl-opencode");
        let wsl_binary = dir.join("wsl.exe");
        std::fs::write(&wsl_binary, "").expect("write wsl.exe");
        let calls: SpawnCalls = Arc::new(Mutex::new(Vec::new()));
        let spawn: SpawnFn = {
            let calls = Arc::clone(&calls);
            Arc::new(move |program, args, options| {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((program, args, options));
                    Ok(SpawnOutput {
                        status: Some(1),
                        stdout: String::new(),
                    })
                })
            })
        };
        let runtime = EnvRuntime::with_seams(
            env_of(&[
                ("PATH", dir.to_str().unwrap()),
                ("SystemRoot", dir.to_str().unwrap()),
                ("WSL_BINARY", wsl_binary.to_str().unwrap()),
            ]),
            spawn,
            home_of(temp_dir("empty-home")),
            Platform::Windows,
        );

        assert_eq!(runtime.resolve_opencode_cli_path(), None);
        assert_eq!(runtime.resolved_opencode_binary_source(), None);
        let recorded = calls.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            recorded
                .iter()
                .all(|(program, _, _)| program.as_str() != wsl_binary.to_string_lossy())
        );
    }

    // -- login shell probes ---------------------------------------------------

    #[tokio::test]
    async fn login_shell_probes_are_bounded_and_fall_through() {
        let calls: SpawnCalls = Arc::new(Mutex::new(Vec::new()));
        let spawn: SpawnFn = {
            let calls = Arc::clone(&calls);
            Arc::new(move |program, args, options| {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    calls
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((program, args, options));
                    // What spawnSync reports when `timeout` fires: no status.
                    Ok(SpawnOutput {
                        status: None,
                        stdout: String::new(),
                    })
                })
            })
        };
        let runtime = EnvRuntime::with_seams(
            env_of(&[
                ("PATH", temp_dir("empty-path").to_str().unwrap()),
                ("SHELL", "/bin/sh"),
            ]),
            spawn,
            home_of(temp_dir("empty-home")),
            Platform::Unix,
        );

        assert!(runtime.get_login_shell_env_snapshot().await.is_none());
        let recorded = calls.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!recorded.is_empty());
        for (_, args, options) in recorded.iter() {
            assert!(args.contains(&"-lic".to_string()));
            assert_eq!(options.timeout_ms, Some(SHELL_PROBE_TIMEOUT_MS));
            assert_eq!(options.timeout_ms, Some(5_000));
        }
    }

    #[tokio::test]
    async fn windows_shell_env_snapshot_falls_back_to_cmd_set() {
        let spawn: SpawnFn = Arc::new(|program, _args, _options| {
            Box::pin(async move {
                if program == "cmd.exe" {
                    Ok(SpawnOutput {
                        status: Some(0),
                        stdout: "PATH=C:\\x\r\nFOO=bar\r\n".to_string(),
                    })
                } else {
                    Ok(SpawnOutput {
                        status: Some(1),
                        stdout: String::new(),
                    })
                }
            })
        });
        let runtime = EnvRuntime::with_seams(
            env_of(&[("ComSpec", "cmd.exe")]),
            spawn,
            home_of(temp_dir("home")),
            Platform::Windows,
        );

        let snapshot = runtime
            .get_login_shell_env_snapshot()
            .await
            .expect("snapshot");
        assert_eq!(snapshot.get("PATH").map(String::as_str), Some("C:\\x"));
        assert_eq!(snapshot.get("FOO").map(String::as_str), Some("bar"));
    }

    // -- resolveManagedOpenCodeLaunchSpec ----------------------------------------

    #[tokio::test]
    async fn unix_launch_spec_passes_binary_through() {
        let runtime = EnvRuntime::with_seams(
            env_of(&[]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Unix,
        );
        assert_eq!(
            runtime
                .resolve_managed_open_code_launch_spec("/usr/local/bin/opencode")
                .await,
            ManagedLaunchSpec {
                binary: "/usr/local/bin/opencode".to_string(),
                args: Vec::new(),
                wrapper_type: WrapperType::None,
            }
        );
        assert_eq!(
            runtime.resolve_managed_open_code_launch_spec("  ").await,
            ManagedLaunchSpec {
                binary: "opencode".to_string(),
                args: Vec::new(),
                wrapper_type: WrapperType::None,
            }
        );
    }

    #[tokio::test]
    async fn windows_cmd_shim_launches_through_comspec() {
        let dir = temp_dir("opencode-cmd");
        let shim = dir.join("opencode.cmd");
        std::fs::write(&shim, "@echo off\r\nexit /b 0\r\n").expect("write shim");
        let runtime = EnvRuntime::with_seams(
            env_of(&[("ComSpec", "C:\\Windows\\System32\\cmd.exe")]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Windows,
        );

        let spec = runtime
            .resolve_managed_open_code_launch_spec(shim.to_str().unwrap())
            .await;
        assert_eq!(
            spec,
            ManagedLaunchSpec {
                binary: "C:\\Windows\\System32\\cmd.exe".to_string(),
                args: vec![
                    "/d".to_string(),
                    "/s".to_string(),
                    "/c".to_string(),
                    "call".to_string(),
                    shim.to_string_lossy().to_string(),
                ],
                wrapper_type: WrapperType::CmdWrapper,
            }
        );
    }

    #[tokio::test]
    async fn npm_cmd_shim_resolves_packaged_windows_executable() {
        let npm_dir = temp_dir("opencode-npm");
        let shim = npm_dir.join("opencode.cmd");
        let native_binary = npm_dir
            .join("node_modules")
            .join("opencode-ai")
            .join("bin")
            .join("opencode.exe");
        std::fs::create_dir_all(native_binary.parent().expect("parent")).expect("mkdir");
        std::fs::write(&native_binary, "").expect("write native");
        std::fs::write(
            &shim,
            "@ECHO off\r\n\"%dp0%\\node_modules\\opencode-ai\\bin\\opencode.exe\" %*\r\n",
        )
        .expect("write shim");
        let runtime = EnvRuntime::with_seams(
            env_of(&[]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Windows,
        );

        let spec = runtime
            .resolve_managed_open_code_launch_spec(shim.to_str().unwrap())
            .await;
        assert_eq!(
            spec,
            ManagedLaunchSpec {
                binary: native_binary.to_string_lossy().to_string(),
                args: Vec::new(),
                wrapper_type: WrapperType::NativeWrapper,
            }
        );
    }

    #[tokio::test]
    async fn node_shebang_shim_launches_under_node() {
        let dir = temp_dir("opencode-node-shim");
        let shim = write_executable(&dir.join("opencode"), "#!/usr/bin/env node\n");
        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", dir.to_str().unwrap())]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Windows,
        );

        let spec = runtime.resolve_managed_open_code_launch_spec(&shim).await;
        // The shebang branch wins ahead of the direct-binary fallback: the
        // shim is launched via the resolved node runtime (or the literal
        // `node` when nothing resolves, like the JS `|| 'node'`).
        assert_eq!(spec.wrapper_type, WrapperType::NodeShebang);
        assert_eq!(spec.args, vec![shim.clone()]);
        assert!(spec.binary == "node" || runtime.is_executable(&spec.binary));
    }

    // -- shebang helpers ----------------------------------------------------------

    #[test]
    fn reads_shebang_interpreters() {
        let dir = temp_dir("shebangs");
        let node_shim = write_executable(&dir.join("a"), "#!/usr/bin/env node\n");
        let bun_shim = write_executable(&dir.join("b"), "#!/Users/x/.bun/bin/bun\n");
        let plain = write_executable(&dir.join("c"), "#!/bin/sh\n");
        let runtime = EnvRuntime::with_seams(
            env_of(&[]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Unix,
        );
        assert_eq!(
            runtime.opencode_shim_interpreter(&node_shim),
            Some(ShimInterpreter::Node)
        );
        assert_eq!(
            runtime.opencode_shim_interpreter(&bun_shim),
            Some(ShimInterpreter::Bun)
        );
        assert_eq!(runtime.opencode_shim_interpreter(&plain), None);
    }

    #[test]
    fn strips_wrapping_quotes() {
        assert_eq!(strip_wrapping_quotes("  '/x/y' "), "/x/y");
        assert_eq!(
            strip_wrapping_quotes("\"C:\\tools\\git\""),
            "C:\\tools\\git"
        );
        assert_eq!(strip_wrapping_quotes("plain"), "plain");
        assert_eq!(strip_wrapping_quotes("\"unmatched"), "\"unmatched");
    }

    #[test]
    fn finds_opencode_ai_bin_refs() {
        assert_eq!(
            find_opencode_ai_bin_ref(
                "@ECHO off\r\n\"%dp0%\\node_modules\\opencode-ai\\bin\\opencode.exe\" %*\r\n"
            )
            .as_deref(),
            Some("node_modules\\opencode-ai\\bin\\opencode")
        );
        assert_eq!(
            find_opencode_ai_bin_ref("bun /repo/node_modules/opencode-ai/bin/opencode serve")
                .as_deref(),
            Some("node_modules/opencode-ai/bin/opencode")
        );
        assert_eq!(find_opencode_ai_bin_ref("nothing here"), None);
        assert_eq!(
            find_opencode_ai_bin_ref("node_modulesX/opencode-ai/bin/opencode"),
            None
        );
    }

    // -- ensureOpencodeCliEnv / clearResolvedOpenCodeBinary ------------------------

    #[tokio::test]
    async fn ensure_opencode_cli_env_caches_and_clear_resets() {
        let path_dir = temp_dir("ensure-bun");
        let bun = write_executable(&path_dir.join("bun"), "#!/bin/sh\nexit 0\n");
        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", path_dir.to_str().unwrap())]),
            noop_spawn(),
            home_of(temp_dir("empty-home")),
            Platform::Unix,
        );

        let resolved = runtime.ensure_opencode_cli_env().await.expect("resolved");
        assert_eq!(resolved, bun);
        // JS sets process.env.OPENCODE_BINARY and prepends the dir to PATH.
        assert_eq!(
            runtime
                .effective_env()
                .get("OPENCODE_BINARY")
                .map(String::as_str),
            Some(bun.as_str())
        );
        assert!(
            runtime
                .effective_env()
                .get("PATH")
                .is_some_and(|p| p.starts_with(path_dir.to_str().unwrap()))
        );

        runtime.clear_resolved_open_code_binary();
        assert_eq!(runtime.lock_state().resolved_opencode_binary, None);
        // Re-resolves after the clear (now via the OPENCODE_BINARY override).
        assert_eq!(runtime.ensure_opencode_cli_env().await, Some(bun));
    }

    // -- git binary ------------------------------------------------------------------

    #[test]
    fn git_binary_is_plain_git_off_windows() {
        let runtime = EnvRuntime::with_seams(
            env_of(&[]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Unix,
        );
        assert_eq!(runtime.resolve_git_binary_for_spawn(), "git");
    }

    #[test]
    fn git_binary_prefers_explicit_env_binary_on_windows() {
        let dir = temp_dir("git-bin");
        let git_exe = dir.join("mygit.exe");
        std::fs::write(&git_exe, "").expect("write");
        let runtime = EnvRuntime::with_seams(
            env_of(&[("GIT_BINARY", git_exe.to_str().unwrap())]),
            noop_spawn(),
            home_of(temp_dir("home")),
            Platform::Windows,
        );
        assert_eq!(
            runtime.resolve_git_binary_for_spawn(),
            git_exe.to_string_lossy().to_string()
        );
    }

    // -- PATH builders (server-utils-runtime.js) --------------------------------------

    #[tokio::test]
    async fn augmented_path_preserves_user_configured_order() {
        let home = temp_dir("augmented-home");
        let home_str = home.to_str().unwrap().to_string();
        let current = [
            format!("{home_str}/.bun/bin"),
            format!("{home_str}/Library/pnpm"),
            "/opt/homebrew/bin".to_string(),
            "/usr/bin".to_string(),
        ]
        .join(":");
        let shell = [
            format!("{home_str}/.bun/bin"),
            "/opt/homebrew/bin".to_string(),
            format!("{home_str}/.cargo/bin"),
            "/usr/bin".to_string(),
        ]
        .join(":");
        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", current.as_str())]),
            noop_spawn(),
            home_of(home),
            Platform::Unix,
        );
        runtime.set_cached_login_shell_env_snapshot(Some(
            [("PATH".to_string(), shell)].into_iter().collect(),
        ));

        assert_eq!(
            runtime.build_augmented_path().await,
            [
                format!("{home_str}/.bun/bin"),
                format!("{home_str}/Library/pnpm"),
                "/opt/homebrew/bin".to_string(),
                "/usr/bin".to_string(),
                format!("{home_str}/.cargo/bin"),
            ]
            .join(":")
        );
    }

    #[tokio::test]
    async fn augmented_path_prefers_login_shell_when_process_path_minimal() {
        let home = temp_dir("augmented-minimal-home");
        let home_str = home.to_str().unwrap().to_string();
        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", "/usr/local/bin:/usr/bin:/bin")]),
            noop_spawn(),
            home_of(home),
            Platform::Unix,
        );
        runtime.set_cached_login_shell_env_snapshot(Some(
            [(
                "PATH".to_string(),
                [
                    format!("{home_str}/.bun/bin"),
                    "/opt/homebrew/bin".to_string(),
                    "/usr/bin".to_string(),
                ]
                .join(":"),
            )]
            .into_iter()
            .collect(),
        ));

        assert_eq!(
            runtime.build_augmented_path().await,
            [
                format!("{home_str}/.bun/bin"),
                "/opt/homebrew/bin".to_string(),
                "/usr/bin".to_string(),
                "/usr/local/bin".to_string(),
                "/bin".to_string(),
            ]
            .join(":")
        );
    }

    #[tokio::test]
    async fn managed_path_prefers_shell_path_then_appends_process_entries() {
        let home = temp_dir("managed-home");
        let home_str = home.to_str().unwrap().to_string();
        let current = [
            format!("{home_str}/.opencode/bin"),
            format!("{home_str}/.bun/bin"),
            format!("{home_str}/Library/pnpm"),
            "/opt/homebrew/bin".to_string(),
            "/usr/bin".to_string(),
        ]
        .join(":");
        let shell = [
            format!("{home_str}/.opencode/bin"),
            format!("{home_str}/.bun/bin"),
            "/opt/homebrew/bin".to_string(),
            "/usr/bin".to_string(),
            format!("{home_str}/.cargo/bin"),
        ]
        .join(":");
        let runtime = EnvRuntime::with_seams(
            env_of(&[("PATH", current.as_str())]),
            noop_spawn(),
            home_of(home),
            Platform::Unix,
        );
        runtime.set_cached_login_shell_env_snapshot(Some(
            [("PATH".to_string(), shell)].into_iter().collect(),
        ));

        assert_eq!(
            runtime.build_managed_open_code_path().await,
            [
                format!("{home_str}/.opencode/bin"),
                format!("{home_str}/.bun/bin"),
                "/opt/homebrew/bin".to_string(),
                "/usr/bin".to_string(),
                format!("{home_str}/.cargo/bin"),
                format!("{home_str}/Library/pnpm"),
            ]
            .join(":")
        );
    }

    #[tokio::test]
    async fn windows_managed_path_keeps_existing_package_manager_dirs() {
        let root = temp_dir("win-path");
        let system_dir = root.join("System32");
        let app_data = root.join("Roaming");
        let program_files = root.join("Program Files");
        let local_app_data = root.join("Local");
        let program_data = root.join("ProgramData");
        let user_profile = root.join("User");

        let npm_bin = app_data.join("npm");
        let node_bin = program_files.join("nodejs");
        let pnpm_home = local_app_data.join("pnpm");
        let yarn_bin = local_app_data.join("Yarn").join("bin");
        let choco_bin = program_data.join("chocolatey").join("bin");
        for dir in [
            &system_dir,
            &npm_bin,
            &node_bin,
            &pnpm_home,
            &yarn_bin,
            &choco_bin,
        ] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }

        let runtime = EnvRuntime::with_seams(
            env_of(&[
                ("PATH", system_dir.to_str().unwrap()),
                ("APPDATA", app_data.to_str().unwrap()),
                ("ProgramFiles", program_files.to_str().unwrap()),
                ("LOCALAPPDATA", local_app_data.to_str().unwrap()),
                ("ProgramData", program_data.to_str().unwrap()),
                ("USERPROFILE", user_profile.to_str().unwrap()),
            ]),
            noop_spawn(),
            home_of(user_profile),
            Platform::Windows,
        );

        let expected = [
            system_dir, npm_bin, node_bin, pnpm_home, yarn_bin, choco_bin,
        ]
        .iter()
        .map(|d| d.to_string_lossy().to_string())
        .collect::<Vec<String>>()
        .join(&path_delim().to_string());
        assert_eq!(runtime.build_managed_open_code_path().await, expected);
    }
}
