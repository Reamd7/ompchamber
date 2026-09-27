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
//!
//! 中文摘要：本模块为托管 opencode/omp-host 进程提供环境运行时——解析 Bun/Node/Git
//! 等依赖二进制、探测并缓存登录 shell 环境快照、组装增强 PATH 与 Windows 启动
//! 包装规格。对 `process.env` 的一切副作用以覆盖层（overlay）建模，spawn/env/
//! home/platform 四个 seam 均可注入，以便在任意宿主上测试 win32 行为。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::engine_env::path_utils::{merge_path_values, path_looks_user_configured};

/// JS `SHELL_PROBE_TIMEOUT_MS`: login-shell probes source the user's rc files;
/// a slow or interactive rc must not hold startup hostage.
/// 登录 shell 探测的超时上限（毫秒）：探测会加载用户 rc 文件，
/// 慢速或交互式 rc 不得阻塞启动。
const SHELL_PROBE_TIMEOUT_MS: u64 = 5_000;

/// JS `WINDOWS_BATCH_EXTENSIONS`.
/// Windows 批处理脚本扩展名集合（.cmd/.bat/.com）。
const WINDOWS_BATCH_EXTENSIONS: [&str; 3] = [".cmd", ".bat", ".com"];

/// `process.platform` as the JS module reads it. Injectable so the win32 flow
/// is testable on any host (the JS tests override `process.platform` the same
/// way while the node `path` builtin stays host-bound).
/// process.platform 的可注入抽象：使 win32 流程可在任意宿主上测试
/// （JS 测试同样覆写 process.platform，而 node path 内建仍绑定宿主）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// 非 Windows 平台（JS 的非 'win32' 分支）。
    Unix,
    /// Windows（JS 的 'win32'）。
    Windows,
}

/// 宿主实际平台：编译期按 windows 目标判定。
pub fn host_platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unix
    }
}

/// Host path separator / delimiter — mirrors the node `path` module the JS
/// code ran under (`path.sep`, `path.delimiter`).
/// 主机路径分隔符：Windows 为反斜杠，其它为正斜杠（对应 node path.sep）。
fn path_sep() -> &'static str {
    if cfg!(windows) { "\\" } else { "/" }
}

/// 主机 PATH 列表分隔符：Windows 为分号，其它为冒号（对应 node path.delimiter）。
fn path_delim() -> char {
    if cfg!(windows) { ';' } else { ':' }
}

// ---------------------------------------------------------------------------
// Small host-path helpers (node `path` semantics, both separators accepted)
// ---------------------------------------------------------------------------

/// 判断字符是否路径分隔符；同时接受 / 与 \（node 在 Windows 上的宽容行为）。
fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

/// JS path.basename：路径末段组件（忽略尾部分隔符），两种分隔符都识别。
fn basename(p: &str) -> String {
    let trimmed = p.trim_end_matches(is_sep);
    match trimmed.rfind(is_sep) {
        Some(i) => trimmed[i + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// JS path.dirname：去掉末段后的目录部分；根路径保留单个分隔符，
/// 无分隔符的相对路径返回 "."。
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
/// JS path.extname：末段组件的扩展名（含点）；仅前导点的组件（如 .opencode）
/// 视为无扩展名。
fn extname(p: &str) -> String {
    let name = basename(p);
    match name.rfind('.') {
        Some(0) | None => String::new(),
        Some(i) => name[i..].to_string(),
    }
}

/// String join with the host separator (node `path.join` for our inputs).
/// 以主机分隔符拼接各段（node path.join 在本模块输入下的行为）。
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
/// JS path.resolve(单参数)：词法绝对化与规范化——消除 . 与 ..、压缩分隔符；
/// 不解析符号链接（那是 canonicalize 的职责）。相对路径基于当前工作目录。
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
/// spawn seam 的调用选项（JS spawnSync options 中被本模块消费的部分）。
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    /// 超时毫秒数；None 表示不设超时（超时的探针会被放弃并按 status null 处理）。
    pub timeout_ms: Option<u64>,
}

/// spawn seam 的输出形状（JS spawnSync 返回的 status/stdout 投影）。
#[derive(Debug, Clone, Default)]
pub struct SpawnOutput {
    /// 子进程退出码；None 对应 JS 的 status: null（超时或被信号杀死）。
    pub status: Option<i32>,
    /// 标准输出（UTF-8 有损解码后的字符串）。
    pub stdout: String,
}

/// spawn seam 的返回 future 类型：装箱的异步 IO 结果，便于注入伪造实现与超时控制。
pub type SpawnFuture = Pin<Box<dyn Future<Output = std::io::Result<SpawnOutput>> + Send>>;
/// 可注入的 spawn seam：(程序, 参数, 选项) -> 异步结果；替代并对应 JS 注入的
/// spawnSync 依赖，Err 表示 spawn 本身失败（如 ENOENT）。
pub type SpawnFn = Arc<dyn Fn(String, Vec<String>, SpawnOptions) -> SpawnFuture + Send + Sync>;

/// Base environment read (`process.env` in JS). A snapshot function so the
/// case-insensitive [`EnvRuntime::get_env_value`] lookup can enumerate keys.
/// 底层环境读取 seam（JS 的 process.env）：返回完整快照，
/// 供大小写不敏感的 get_env_value 枚举键名。
pub type EnvSource = Arc<dyn Fn() -> HashMap<String, String> + Send + Sync>;

/// The injectable `deps.homedir` (JS `os.homedir()`).
/// 可注入的 deps.homedir（JS os.homedir()）。
pub type HomeFn = Arc<dyn Fn() -> PathBuf + Send + Sync>;

/// 生产 spawn seam：tokio 子进程，stdin 关闭、stdout/stderr 管道、kill_on_drop
/// 防止超时放弃后泄漏子进程；设置 timeout 时超时返回 status=None。
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

/// 生产环境快照 seam：每次调用把当前进程环境变量复制为 HashMap。
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

/// 生产 home seam：crate::config::home_dir()，取不到时退回 "."。
fn real_home() -> HomeFn {
    Arc::new(|| crate::config::home_dir().unwrap_or_else(|| PathBuf::from(".")))
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Where the omp-host runtime (Bun) was found. Mirrors the strings the JS
/// writes into `state.resolvedOpencodeBinarySource`.
/// omp-host runtime（Bun）的定位来源；as_str 输出与 JS 写入
/// state.resolvedOpencodeBinarySource 的字符串一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinarySource {
    /// 来自显式环境变量（OMPCHAMBER_OMP_HOST_RUNTIME / OPENCODE_BINARY）。
    Env,
    /// 来自 PATH 搜索。
    Path,
    /// 来自固定 fallback 路径（如 ~/.bun/bin、/opt/homebrew/bin）。
    Fallback,
    /// 来自内置 CLI 目录。
    Bundled,
    /// 已解析但来源未细分（ensure 流程的默认补记）。
    Unknown,
}

/// BinarySource 与 JS 字符串表示的互转。
impl BinarySource {
    /// 返回 JS 使用的来源字符串（"env"/"path"/"fallback"/"bundled"/"unknown"）。
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
/// 运行时共享状态（JS 闭包捕获并贯穿传递的 state 对象）。
/// cached_login_shell_env_snapshot 保留 JS 三态：外层 None=尚未探测，
/// Some(None)=已探测但无快照。
#[derive(Debug, Default)]
pub struct EnvRuntimeState {
    /// 登录 shell 快照缓存（未探测 / 已探测无快照 / 快照本体 三态）。
    pub cached_login_shell_env_snapshot: Option<Option<HashMap<String, String>>>,
    /// 已解析的 omp-host runtime 二进制路径缓存。
    pub resolved_opencode_binary: Option<String>,
    /// 最近一次解析的来源标记。
    pub resolved_opencode_binary_source: Option<BinarySource>,
    /// 已解析的 node 二进制路径缓存。
    pub resolved_node_binary: Option<String>,
    /// 已解析的 bun 二进制路径缓存。
    pub resolved_bun_binary: Option<String>,
    /// Windows 上已解析的 git 二进制缓存。
    pub resolved_git_binary: Option<String>,
}

/// JS `process.env` mutations performed by this runtime, as an overlay:
/// `Some(value)` sets, `None` deletes (AppImage `ARGV0`).
/// 本运行时对 process.env 的变更（overlay 形式）：
/// Some(v) 表示设值，None 表示删除该键（AppImage 的 ARGV0）。
type EnvOverrides = HashMap<String, Option<String>>;

// ---------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------

/// 环境运行时本体：共享状态与 env 覆盖层之外，持有四个可注入 seam
/// （env_source/spawn/home/platform），对应 JS createOpenCodeEnvRuntime(deps)。
pub struct EnvRuntime {
    /// 共享解析状态（快照与各二进制缓存），Mutex 保护。
    state: Mutex<EnvRuntimeState>,
    /// process.env 变更的覆盖层；不动真实进程环境（多线程下 set_var 不安全）。
    overrides: Mutex<EnvOverrides>,
    /// 底层环境快照 seam（生产为真实进程环境）。
    env_source: EnvSource,
    /// 子进程探测 seam（生产为 tokio 实现，见 real_spawn）。
    spawn: SpawnFn,
    /// 用户主目录 seam（JS os.homedir()）。
    home: HomeFn,
    /// 注入平台（JS process.platform）。
    platform: Platform,
}

/// 环境运行时实现（JS createOpenCodeEnvRuntime 返回的方法集）：
/// 依赖二进制定位、登录 shell 快照、PATH 组装与 Windows 启动包装。
impl EnvRuntime {
    /// Process-wide runtime (index.js module-level state).
    /// 进程级共享实例（对应 index.js 的模块级单例），首次访问时惰性初始化。
    pub fn shared() -> &'static EnvRuntime {
        // 进程级唯一实例：LazyLock 保证首次调用时初始化一次，此后所有调用共享。
        static SHARED: std::sync::LazyLock<EnvRuntime> = std::sync::LazyLock::new(EnvRuntime::new);
        &SHARED
    }

    /// Number of env overrides currently applied (login-shell snapshot keys
    /// plus the ARGV0 delete).
    /// 当前 env 覆盖层中设值项（Some）的数量，即登录 shell 快照写入的键数。
    pub fn shell_env_key_count(&self) -> usize {
        self.lock_overrides()
            .values()
            .filter(|value| value.is_some())
            .count()
    }

    /// JS `resolvedBunBinary` state: only `ensureBunCliEnv` sets it (the shim
    /// runtime paths); the plain managed omp-host launch never does.
    /// 读取已缓存的 bun 二进制路径；仅 ensure_bun_cli_env（shim 运行时路径）会写入。
    pub fn resolved_bun_binary(&self) -> Option<String> {
        self.lock_state().resolved_bun_binary.clone()
    }

    /// JS `resolvedNodeBinary` state: only `ensureNodeCliEnv` sets it.
    /// 读取已缓存的 node 二进制路径；仅 ensure_node_cli_env 会写入。
    pub fn resolved_node_binary(&self) -> Option<String> {
        self.lock_state().resolved_node_binary.clone()
    }

    /// Production runtime with real seams.
    /// 生产构造函数：使用真实 seam（进程 env、tokio spawn、config home_dir、宿主平台）。
    pub fn new() -> Self {
        Self::with_seams(
            real_env_source(),
            real_spawn(),
            real_home(),
            host_platform(),
        )
    }

    /// Test/consumer constructor with injected seams (JS `createOpenCodeEnvRuntime(deps)`).
    /// 注入全部 seam 的构造函数（JS createOpenCodeEnvRuntime(deps)），供测试与定制消费者使用。
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

    /// 获取共享状态锁；锁中毒时恢复内部数据继续（状态仅为缓存，无需 panic）。
    fn lock_state(&self) -> std::sync::MutexGuard<'_, EnvRuntimeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 获取 env 覆盖层锁；锁中毒时同样恢复数据继续。
    fn lock_overrides(&self) -> std::sync::MutexGuard<'_, EnvOverrides> {
        self.overrides.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 读取注入的平台值（process.platform 的可测试替身）。
    fn platform(&self) -> Platform {
        self.platform
    }

    /// `process.env[name]` after this runtime's overlay.
    /// 读取叠加覆盖层后的环境变量：先查覆盖表，未命中回落到底层 env 快照。
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
    /// JS getEnvValue：先精确匹配键名，再按 ASCII 大小写不敏感回退查找
    /// （兼容 Windows 环境变量大小写）；无匹配返回空串。
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
    /// 底层环境快照应用本运行时全部覆盖后的最终环境；
    /// 引擎 spawn 应以此为 env 来源（engine.rs 的接线点）。
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

    /// 写入一条 env 覆盖：Some(v) 设值，None 删除该键。
    fn set_override(&self, key: &str, value: Option<String>) {
        self.lock_overrides().insert(key.to_string(), value);
    }

    /// Test seam for the shared `state.cachedLoginShellEnvSnapshot` the JS
    /// tests pre-set.
    /// 测试 seam：预置登录 shell 快照缓存（JS 测试直接改 state 的等价物）。
    pub fn set_cached_login_shell_env_snapshot(&self, snapshot: Option<HashMap<String, String>>) {
        self.lock_state().cached_login_shell_env_snapshot = Some(snapshot);
    }

    /// 读取最近一次 omp-host runtime 解析的来源标记。
    pub fn resolved_opencode_binary_source(&self) -> Option<BinarySource> {
        self.lock_state().resolved_opencode_binary_source
    }

    /// 记录 omp-host runtime 解析来源。
    fn set_resolved_opencode_binary_source(&self, source: BinarySource) {
        self.lock_state().resolved_opencode_binary_source = Some(source);
    }

    // -- JS isExecutable ----------------------------------------------------

    /// JS isExecutable：候选存在且为普通文件才算；Windows 按扩展名
    /// （.exe/.cmd/.bat/.com，或无扩展名）判定，Unix 检查任一执行位。
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

    /// 读取 PATHEXT（兼容 PathExt 大小写，缺省 .COM;.EXE;.BAT;.CMD），
    /// 按 ; 拆分并去掉空段，返回扩展名变体列表。
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

    /// JS resolveWindowsExecutablePath：非 Windows 原样返回候选；Windows 上
    /// 带扩展名的候选直接校验可执行性，无扩展名的候选逐个 PATHEXT 变体补全后
    /// 校验，最后再试无扩展名本体；都不行返回 None。
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
    /// 在给定搜索 PATH 中按目录顺序查找二进制，返回第一个可执行候选的完整路径；
    /// Windows 上无扩展名的名字先尝试 PATHEXT 补全的候选名。纯查询，不修改任何状态。
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
    /// 以当前生效的 PATH 环境变量执行 search_path_for（JS 的单参数重载）。
    pub fn search_path_for_env(&self, binary_name: &str) -> Option<String> {
        let search_path = self.env_get("PATH").unwrap_or_default();
        self.search_path_for(binary_name, &search_path)
    }

    // -- JS prependToPath ---------------------------------------------------

    /// JS prependToPath：把目录前置到 PATH 覆盖层（已存在则不动），空目录忽略。
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
    /// JS parseNullSeparatedEnvSnapshot：解析 null 分隔的 KEY=VALUE 快照，
    /// 丢弃空段与空键段；windows=true 时把小写 Path 键补写为 PATH。
    /// 输入为空或全部无效返回 None。
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
    /// Windows 登录环境快照：依次尝试 pwsh.exe/powershell.exe（含 System32 内置
    /// 路径；脚本会合并 Machine/User/Process 三级 Path），全部失败后回退
    /// ComSpec 的 "cmd /d /s /c set"，其换行输出转 null 分隔再解析。
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
    /// 获取登录 shell 环境快照（按平台分派到 Windows/Unix 实现），
    /// 结果连同失败状态一起写入缓存——整个进程只探测一次。
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

    /// Unix 登录环境快照：依次用 $SHELL 与 /bin/zsh、/bin/bash、/bin/sh 执行
    /// -lic 'env -0'，解析第一个成功且非空的输出；超时受
    /// SHELL_PROBE_TIMEOUT_MS 约束。
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
    /// JS applyLoginShellEnvSnapshot：无条件删除 AppImage ARGV0；跳过
    /// PWD/OLDPWD/SHLVL/_/ARGV0 等易变键；仅填充当前为空的变量；
    /// shell PATH 合并到当前 PATH 之前。
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

    /// 构造内置 opencode CLI 候选路径列表：仅读取
    /// OMPCHAMBER_BUNDLED_OPENCODE_CLI_DIR 目录下的 opencode（Windows 为
    /// opencode.exe）；Electron 的 process.resourcesPath 在独立 server 不存在。
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

    /// 返回候选的规范化路径：优先 std::fs::canonicalize（解析符号链接），
    /// 失败时退回词法 resolve；空候选返回 None。
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

    /// 判断候选是否为内置 CLI：其规范化路径与任一内置候选的规范化路径完全一致。
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
    /// 解析启动托管 omp host 的运行时（Bun，历史上名为 resolveOpencodeCliPath）：
    /// 先取 OMPCHAMBER_OMP_HOST_RUNTIME 与 OPENCODE_BINARY 显式值（去除包裹引号），
    /// 再搜 PATH 中的 bun，最后尝试 home/.bun/bin 及常见安装位置 fallback；
    /// 命中时记录 BinarySource，找不到返回 None。
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
    /// 登录 shell 单二进制定位探测：依次用 $SHELL 与 /bin/zsh、/bin/bash、/bin/sh
    /// 执行 -lic 'command -v <binary>'，取输出最后一个词并校验可执行；
    /// 探测受 SHELL_PROBE_TIMEOUT_MS 超时约束。
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
    /// Windows where <binary> 探测：返回输出中第一行真实可执行的路径。
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

    /// JS resolveNodeCliPath：按 NODE_BINARY/OMPCHAMBER_NODE_BINARY 显式值 →
    /// PATH 搜索 → 常见 Unix 安装路径的顺序解析 node；仍失败时按平台回退到
    /// where（Windows）或登录 shell command -v 探测。
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

    /// JS resolveBunCliPath：按 BUN_BINARY/OMPCHAMBER_BUN_BINARY 显式值 →
    /// PATH 搜索 → home/.bun/bin 与常见 Unix 路径（Windows 另查 USERPROFILE 下
    /// 的 bun.exe/bun.cmd）的顺序解析 bun；仍失败时回退 where 或 command -v 探测。
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

    /// 返回缓存的 bun 二进制；未缓存时经 resolve_bun_cli_path 解析，
    /// 首次成功后把其目录前置到 PATH 并写入缓存。解析失败返回 None。
    pub async fn ensure_bun_cli_env(&self) -> Option<String> {
        if let Some(cached) = self.lock_state().resolved_bun_binary.clone() {
            return Some(cached);
        }
        let resolved = self.resolve_bun_cli_path().await?;
        self.prepend_to_path(&dirname(&resolved));
        self.lock_state().resolved_bun_binary = Some(resolved.clone());
        Some(resolved)
    }

    /// 返回缓存的 node 二进制；未缓存时经 resolve_node_cli_path 解析，
    /// 首次成功后把其目录前置到 PATH 并写入缓存。解析失败返回 None。
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

    /// JS normalizeExecutableCandidate：修剪空白后校验候选可执行性；
    /// Windows 上还会经 resolve_windows_executable_path 补全 PATHEXT 扩展名。
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

    /// 按宿主架构返回 Windows 原生 opencode npm 包名候选（x86_64 与 aarch64 一致，
    /// 均先 x64-baseline 后 x64 —— ARM64 原生包存在 Bun FFI 问题的临时规避）；
    /// 其它架构返回空表。
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

    /// 在 node_modules 目录中定位原生 Windows opencode.exe：先查
    /// opencode-ai/bin，再查各原生包的直装与嵌套安装位置。
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

    /// node_modules 内只有 JS 启动器（opencode-ai/bin/opencode）时，
    /// 构造以解析出的 node（缺省 "node"）承载该启动器的启动规格。
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

    /// 从 .cmd 包装脚本内容中提取 node_modules/opencode-ai/bin/opencode 引用，
    /// 相对脚本目录解析后向上回溯三级，得到 node_modules 根目录。
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

    /// 从 opencode 路径推断其所属 node_modules 目录：依次识别 bun 全局安装布局、
    /// npm .bin shim、包内 bin 布局与 npm 目录结构，以及 .cmd 包装脚本内的引用；
    /// 每个候选须经原生二进制或 node 启动器验证有效才返回，全部无效返回 None。
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

    /// JS resolveManagedOpenCodeLaunchSpec：把 opencode 路径换算成可安全 spawn 的
    /// 启动规格。非 Windows 原样透传；Windows 依次尝试 node_modules 原生二进制、
    /// node 启动器、node/bun shebang、PATHEXT 补全的可执行文件，并兜底保证
    /// .cmd/.bat 一定经 cmd.exe 启动（绝不直接 spawn 批处理）。
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

    /// 读取文件首行并提取 #! 之后的 shebang 解释器串；
    /// 路径为空、打开失败、无 #! 前缀或内容为空均返回 None。
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

    /// 依据 shim 首行 shebang 判定承载运行时：
    /// 含整词 node 返回 Node，含整词 bun 返回 Bun，否则 None。
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

    /// 为 shim 的 shebang 解释器预先解析并前置对应运行时（node 或 bun）到 PATH。
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

    /// JS ensureOpencodeCliEnv：返回缓存的 omp-host runtime；未缓存时优先采用
    /// OPENCODE_BINARY 已有可执行值，否则经 resolve_opencode_cli_path 解析，
    /// 成功后写入 OPENCODE_BINARY 覆盖、前置其目录到 PATH 并准备 shim 运行时。
    /// 解析失败返回 None。
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

    /// JS resolveGitBinaryForSpawn：非 Windows 直接返回 "git"；Windows 按
    /// GIT_BINARY/OMPCHAMBER_GIT_BINARY 显式值 → PATH 搜索 → 常见安装目录
    /// （Program Files、LocalAppData 等）的顺序解析，优先 .exe 结果并缓存；
    /// 全部失败兜底 "git.exe"。
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

    /// JS clearResolvedOpenCodeBinary：清空缓存的 omp-host runtime，使下次 ensure 重新解析。
    pub fn clear_resolved_open_code_binary(&self) {
        self.lock_state().resolved_opencode_binary = None;
    }

    // -- server-utils-runtime.js PATH builders ----------------------------------

    /// JS `getLoginShellPath` (server/index.js): the snapshot's PATH when
    /// present and non-empty, else null.
    /// 返回登录 shell 快照中非空的 PATH；无快照或快照 PATH 为空返回 None。
    pub async fn get_login_shell_path(&self) -> Option<String> {
        let snapshot = self.get_login_shell_env_snapshot().await?;
        match snapshot.get("PATH") {
            Some(path) if !path.is_empty() => Some(path.clone()),
            _ => None,
        }
    }

    /// JS buildWindowsManagedToolchainPath：枚举 Windows 包管理器/运行时目录
    /// （npm、nodejs、pnpm、bun、volta、yarn、scoop、chocolatey、WindowsApps 等；
    /// 相关环境变量缺失时按默认位置推导），忽略大小写去重后仅保留真实存在的
    /// 目录并以主机分隔符连接；非 Windows 返回空串。
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
    /// JS buildAugmentedPath：进程 PATH 看似用户自定义时以它为主、登录 shell PATH
    /// 为补充；反之以登录 shell PATH 为主；两侧条目按序去重合并。
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
    /// JS buildManagedOpenCodePath：登录 shell PATH → 进程 PATH → Windows 工具链
    /// 目录，逐层按序去重合并，供托管 opencode 进程使用。
    pub async fn build_managed_open_code_path(&self) -> String {
        let current_path = self.get_env_value("PATH");
        let login_shell_path = self.get_login_shell_path().await.unwrap_or_default();
        let base = merge_path_values(&login_shell_path, &current_path, path_delim());
        let toolchain = self.build_windows_managed_toolchain_path();
        merge_path_values(&base, &toolchain, path_delim())
    }
}

/// 默认实现委托 EnvRuntime::new（真实 seam 的生产运行时）。
impl Default for EnvRuntime {
    /// 构造使用真实 seam 的默认实例。
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Launch spec shapes (JS resolveManagedOpenCodeLaunchSpec return value)
// ---------------------------------------------------------------------------

/// 托管 opencode 启动规格中的包装类型（JS resolveManagedOpenCodeLaunchSpec 的
/// wrapperType 值）：描述二进制以何种间接方式被启动，用于 spawn 决策与遥测区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperType {
    /// JS `null`.
    /// 无包装：直接启动二进制（对应 JS 的 null）。
    None,
    /// 解析出的原生 opencode 可执行文件（如 node_modules 内的 opencode.exe）。
    NativeWrapper,
    /// 以 node 运行包内 bin/opencode 启动器脚本启动。
    NodeLauncher,
    /// shim 首行为 node shebang，由 node 解释器承载。
    NodeShebang,
    /// shim 首行为 bun shebang，由 bun 解释器承载。
    BunShebang,
    /// Windows 批处理 shim，经 ComSpec（cmd.exe）的 /c call 启动。
    CmdWrapper,
    /// 解析出的其它可执行文件（如按 PATHEXT 补全扩展名后的 .exe）。
    ExecutableWrapper,
}

/// WrapperType 与 JS wrapperType 字符串表示的互转。
impl WrapperType {
    /// 返回 JS 序列化用的 wrapperType 字符串；None 包装对应 JS 的 null。
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

/// 托管 opencode 的最终启动规格（JS resolveManagedOpenCodeLaunchSpec 的返回值）：
/// 真正要 spawn 的程序、传给它的参数与包装类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedLaunchSpec {
    /// 实际启动的程序：可能是解释器（node/bun）或 cmd.exe，而非 opencode 本体。
    pub binary: String,
    /// 传给 binary 的参数：被承载的 shim 路径，或 cmd 的 /d /s /c call 等序列。
    pub args: Vec<String>,
    /// 包装方式标记，用于区分启动路径（遥测/日志）。
    pub wrapper_type: WrapperType,
}

/// shim 首行 shebang 指向的 JS 运行时种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShimInterpreter {
    /// shebang 中出现整词 node。
    Node,
    /// shebang 中出现整词 bun。
    Bun,
}

/// JS `stripWrappingQuotes` — strip a single wrapping quote pair.
/// 去掉首尾成对的同类引号（单引号或双引号）及外侧空白；
/// 引号不成对或无引号时返回修剪后的原值。
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
/// 从 start 起在 haystack 中做 ASCII 大小写不敏感的子串查找，
/// 命中返回起始字节下标（首个匹配字节为 ASCII，必为字符边界）。
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

/// 判断字节是否为词字符（ASCII 字母数字或下划线），用于整词边界判断。
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// JS `/\bnode\b/i`-style whole-word, case-insensitive containment.
/// 大小写不敏感地判断 haystack 是否把 word 作为完整单词包含
/// （前后都不是词字符），对应 JS 的 /\bword\b/i 语义。
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
/// 在 pos 处匹配一段连续的 / 或 \ 分隔符，返回分隔符串之后的位置；
/// pos 处没有分隔符则返回 None。
fn match_sep_run(text: &str, pos: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = pos;
    while i < bytes.len() && (bytes[i] == b'/' || bytes[i] == b'\\') {
        i += 1;
    }
    if i == pos { None } else { Some(i) }
}

/// Case-insensitive literal match at exactly `pos`; returns the end index.
/// 在 pos 处做大小写不敏感的字面量匹配，成功返回结束下标；
/// 越界或内容不符返回 None。
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
/// 在包装脚本内容中查找 node_modules/opencode-ai/bin/opencode 引用
/// （各段间允许 / 与 \ 混用、可连续多个），返回保留原始大小写的匹配子串。
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
/// 把连续的 / 与 \ 分隔符串压缩为单个主机路径分隔符。
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

/// env-runtime 行为测试：通过注入 seam（env/spawn/home/platform）复现 JS 测试对
/// process.env、process.platform 与 spawnSync 的覆盖，验证 PATH 搜索、登录 shell
/// 快照、二进制定位与 Windows 启动包装等契约。
#[cfg(test)]
mod tests {
    use super::*;

    /// 创建以 tag 命名、带进程号与纳秒时间戳的唯一临时目录（已递归创建），
    /// 避免并行测试冲突；返回其路径。
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

    /// 测试辅助：记录 spawn seam 收到的 (program, args, options) 调用序列，
    /// 供测试断言实际发出的探测命令。
    type SpawnCalls = Arc<Mutex<Vec<(String, Vec<String>, SpawnOptions)>>>;

    /// 写入文件内容并（Unix 上）设为 0o755 可执行，返回路径字符串；
    /// 用于构造 PATH 搜索与 shebang 识别所需的候选二进制。
    fn write_executable(path: &Path, contents: &str) -> String {
        std::fs::write(path, contents).expect("write executable");
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path.to_string_lossy().to_string()
    }

    /// 由 &str 键值对构造返回固定快照的 EnvSource seam。
    fn env_of(map: &[(&str, &str)]) -> EnvSource {
        let owned: HashMap<String, String> = map
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(move || owned.clone())
    }

    /// 构造始终返回同一目录的 HomeFn seam。
    fn home_of(dir: PathBuf) -> HomeFn {
        Arc::new(move || dir.clone())
    }

    /// 永远返回 status=1、空 stdout 的 SpawnFn seam，让所有子进程探测失败。
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

    /// 验证 null 分隔快照解析：KEY=VALUE 收录，空段与无 "=" 或空键的段被丢弃。
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

    /// 验证空输入与全空段的快照都返回 None（JS 的 "无快照" 三态）。
    #[test]
    fn empty_snapshot_is_none() {
        assert!(EnvRuntime::parse_null_separated_env_snapshot("", false).is_none());
        assert!(EnvRuntime::parse_null_separated_env_snapshot("\0\0", false).is_none());
    }

    /// 验证 windows=true 时小写 "Path" 键被补写为大写 "PATH"；非 Windows 不做该修正。
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

    /// 验证 search_path_for 是纯查询：在显式搜索路径中命中候选，
    /// 且不向 env 覆盖层写入任何变更。
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

    /// 验证 apply_login_shell_env_snapshot：始终删除 AppImage ARGV0、跳过 PWD 等
    /// 易变键、只填充当前未设置的变量，并把 shell PATH 合并到当前 PATH 之前。
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

    /// 验证登录 shell 探测失败（无快照）时 ARGV0 仍会被清除（#2588 的行为契约）。
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

    /// 验证快照应用后 shell PATH 条目排在当前 PATH 之前并按序去重合并。
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

    /// 验证 resolve_opencode_cli_path 在无显式配置时从 PATH 找到 bun，
    /// 并把解析来源标记为 BinarySource::Path。
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

    /// 验证 is_bundled_open_code_cli_path 经 canonical 路径比对识别内置 CLI 目录下的
    /// opencode；同目录下的其它路径不匹配。
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

    /// 验证 OPENCODE_BINARY 显式指定的可执行文件优先于内置 CLI 候选，
    /// 来源标记为 BinarySource::Env。
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

    /// 验证 PATH 与 home 目录都找不到 bun 时落到绝对路径 fallback
    /// （/opt/homebrew/bin、/usr/local/bin）；宿主机不存在则返回 None。
    /// 两种宿主结果都断言对应的来源标记。
    #[test]
    fn falls_through_to_absolute_bun_fallbacks_or_none() {
        // The unix fallback list ends with two absolute paths the test
        // cannot control; assert whichever outcome the host dictates.
        // 探测宿主机上真实存在的绝对路径 fallback（测试无法控制这两个路径）。
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

    /// 验证 Windows 桌面版安装目录（LOCALAPPDATA 下的 Programs\OpenCode）
    /// 不会被自动识别为 omp-host runtime 候选。
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

    /// 验证 Windows 上 PATH 中的 bun.exe 优先于 home 目录 fallback，来源标记为 Path。
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

    /// 验证 WSL_BINARY 环境变量与 wsl.exe 不参与解析：结果为 None，
    /// 且记录到的所有 spawn 调用都不以 wsl.exe 为目标。
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

    /// 验证登录 shell 探测以 -lic 参数执行并携带 SHELL_PROBE_TIMEOUT_MS 超时；
    /// 探针超时（status 为 null）时依次降级尝试，最终返回 None。
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

    /// 验证 PowerShell 探测失败后回退到 cmd /c set，其 CRLF 输出
    /// 被转换为 null 分隔并正确解析（含 PATH 大小写修正）。
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

    /// 验证非 Windows 平台 resolve_managed_open_code_launch_spec 原样透传二进制
    /// （空输入回退为 "opencode"），不构造任何包装。
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

    /// 验证 Windows 下 .cmd shim 被包装为 ComSpec 的 "/d /s /c call <shim>" 调用，
    /// wrapper_type 标记为 CmdWrapper。
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

    /// 验证 npm 的 .cmd shim 依据脚本内容引用定位 node_modules 内的原生
    /// opencode.exe，wrapper_type 标记为 NativeWrapper。
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

    /// 验证 node shebang 分支优先于直连可执行文件分支：shim 由解析出的 node
    /// （或字面量 "node"）承载启动，wrapper_type 为 NodeShebang。
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

    /// 验证 opencode_shim_interpreter 能从 shim 首行 shebang 识别 node/bun 解释器，
    /// 其它 shebang（如 /bin/sh）返回 None。
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

    /// 验证 strip_wrapping_quotes 去除首尾成对的同类引号与外侧空白；
    /// 引号不成对或无引号的输入原样返回修剪结果。
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

    /// 验证 find_opencode_ai_bin_ref 在 cmd/bun 包装脚本内容中定位
    /// node_modules/opencode-ai/bin/opencode 引用：/ 与 \ 混用可匹配，
    /// 前缀残缺（如 node_modulesX）或无引用时不匹配。
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

    /// 验证 ensure_opencode_cli_env：首次解析写入 OPENCODE_BINARY 覆盖并把其目录
    /// 前置到 PATH；clear_resolved_open_code_binary 清缓存后可经覆盖值重新解析。
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

    /// 验证非 Windows 平台 resolve_git_binary_for_spawn 直接返回裸 "git"，不做任何探测。
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

    /// 验证 Windows 上 GIT_BINARY 显式指定的可执行文件优先于 PATH 与安装目录探测。
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

    /// 验证 build_augmented_path：进程 PATH 看似用户自定义时保持其顺序优先，
    /// 登录 shell 独有的目录按序去重后追加在后。
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

    /// 验证进程 PATH 仅为系统默认极简集合时，build_augmented_path 改以登录 shell
    /// PATH 为主、进程独有条目追加在后。
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

    /// 验证 build_managed_open_code_path：登录 shell PATH 条目在前，
    /// 进程 PATH 独有条目按序去重追加。
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

    /// 验证 Windows 下 build_managed_open_code_path 追加现存的包管理器目录
    /// （npm/nodejs/pnpm/yarn/chocolatey 等），且仅保留真实存在的目录。
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
