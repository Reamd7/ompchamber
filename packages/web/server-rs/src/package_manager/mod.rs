//! Port of `packages/web/server/lib/package-manager.js`.
//!
//! The JS module is a library (no routes of its own): `openchamber-routes.js`
//! consumes `checkForUpdates`/`getUpdateCommand`/`detectPackageManagerDetails`
//! for `/api/ompchamber/update-check` and `/api/ompchamber/update-install`,
//! and the CLI `openchamber update` consumes `detectPackageManager`,
//! `getCurrentVersion`, `checkForUpdates`, and `executeUpdate`. Those routes
//! are owned by `opencode_meta`; this module exposes the same functions on
//! [`PackageManagerRuntime`] and [`router`] stays empty.
//!
//! Seams: process probes go through [`spawn::CommandRunner`] and outbound
//! fetches through [`http::HttpTransport`] (JS `spawnSync` + global `fetch`),
//! so tests inject fakes exactly like the vitest mocks.
//!
//! Known divergences from the JS (unobservable or environment-inherent):
//! - `process.execPath` maps to `std::env::current_exe()` and `process.argv[1]`
//!   to `std::env::args().nth(1)` — the strongest Node-runtime equivalent.
//! - `OMPCHAMBER_UPDATE_API_URL` is read per call instead of once at module
//!   load (the JS value is fixed for the process either way).
//! - `getLatestVersion` intentionally sends no `User-Agent` header, mirroring
//!   the JS `fetch` (so GitHub's UA requirement rejects it identically).
//!
//! 中文说明：JS 原模块是纯库（无自有路由），本移植在 [`PackageManagerRuntime`]
//! 上暴露同名函数供 `opencode_meta` 的路由与 CLI 复用。子进程探测走
//! [`spawn::CommandRunner`]、出站请求走 [`http::HttpTransport`]，测试注入
//! fake 的方式与 vitest 打桩一一对应。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use crate::context::RouterContext;

/// 包管理器探测（`detectPackageManager` 家族、探测缓存与命令引号处理）。
pub mod detect;
/// 出站 HTTP 传输接缝（生产 reqwest / 测试 fake）。
pub mod http;
/// 纯函数：路径归一化、platform/arch 映射、安装 ID、版本比较。
pub mod paths;
/// 子进程执行接缝（生产 tokio / 测试 fake）。
pub mod spawn;
/// 更新检查与执行（checkForUpdates / executeUpdate 等）。
pub mod update;

/// 对齐 vitest 套件的行为测试。
#[cfg(test)]
mod tests;

pub use detect::{
    PackageManagerDetails, clear_detection_cache, detect_package_manager_from_install_path,
    detect_package_manager_from_invocation_path, detect_package_manager_from_runtime_path,
    quote_command,
};
pub use update::{CheckForUpdatesOptions, ExecuteUpdateOptions, UpdateExecution, UpdateInfo};

/// 当前 web 包的 npm 包名（升级提示输出用）。
const PACKAGE_NAME: &str = "@ompchamber/web";
/// 上游仓库 `owner/repo`（拼装 release / tarball URL）。
const GITHUB_REPO: &str = "Reamd7/ompchamber";
/// CHANGELOG.md 的 raw 地址（更新说明切片的数据源）。
const CHANGELOG_URL: &str = "https://raw.githubusercontent.com/Reamd7/ompchamber/main/CHANGELOG.md";
/// GitHub releases 页面前缀（拼 release/tag 链接）。
const GITHUB_RELEASES_URL: &str = "https://github.com/Reamd7/ompchamber/releases";
/// GitHub Releases REST API 前缀（latest 与 tag 查询）。
const GITHUB_RELEASES_API_URL: &str = "https://api.github.com/repos/Reamd7/ompchamber/releases";
// OMPChamber is distributed as release tarballs, not on the npm registry;
// the version-less URL always resolves to the newest published release.
/// 永远指向最新发布；有具体版本时则用带版本号的 download URL。
const RELEASE_TARBALL_LATEST_URL: &str =
    "https://github.com/Reamd7/ompchamber/releases/latest/download/ompchamber-web-latest.tgz";

/// 编译期常量，不随运行时工作目录变化。
fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// JS `__dirname` is `<web package>/server/lib`, so the "current package" is
/// the web package root — here `<web package>/server-rs/..`.
/// 与 JS 一样只做词法上溯（`server-rs/..`），不解析符号链接。
fn default_package_root() -> PathBuf {
    paths::resolve_lexically(&crate_dir().join(".."))
}

/// Injected environment + runtime identity for the JS module state.
///
/// Construction is cheap and stateless apart from the process-wide detection
/// cache (JS `cachedDetectedPm`); share one instance via [`shared`].
/// 生产环境经 [`shared`] 共享单例；测试用 with_* 系列注入各接缝。
pub struct PackageManagerRuntime {
    /// JS `spawnSync` 的执行接缝。
    runner: Arc<dyn spawn::CommandRunner>,
    /// JS 全局 `fetch` 的传输接缝。
    transport: Arc<dyn http::HttpTransport>,
    /// Test seam: `Some(value)` forces the var, `None` forces unset, missing
    /// keys fall through to the real environment.
    /// 键存在时 `Some(v)` 强制设置、`None` 强制视作未设置。
    env: Option<HashMap<String, Option<String>>>,
    /// `process.argv[1]` override (`None` → real argv).
    /// 探测包管理器的输入之一（脚本入口路径）。
    invoked_path: Option<String>,
    /// `process.execPath` override (`None` → current executable).
    /// Node 可执行文件路径的等价物。
    exec_path: Option<String>,
    /// JS `getCurrentPackagePath()` override (tests pin a fake install).
    /// 测试固定假安装根目录用。
    package_root: Option<PathBuf>,
}

/// 构造器、接缝注入器与 process.* 语义访问器。
impl PackageManagerRuntime {
    /// Production runtime: tokio spawns + reqwest (rustls) transport.
    /// tokio 子进程 + reqwest（rustls，10 秒连接超时）。
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            runner: Arc::new(spawn::TokioCommandRunner),
            transport: Arc::new(http::ReqwestTransport::new(client)),
            env: None,
            invoked_path: None,
            exec_path: None,
            package_root: None,
        }
    }

    /// Test constructor with both seams injected.
    /// env 等其余接缝保持默认（不覆盖）。
    pub fn with_seams(
        runner: Arc<dyn spawn::CommandRunner>,
        transport: Arc<dyn http::HttpTransport>,
    ) -> Self {
        Self {
            runner,
            transport,
            env: None,
            invoked_path: None,
            exec_path: None,
            package_root: None,
        }
    }

    /// Force env vars: `Some(value)` sets, `None` unsets.
    /// 构建者模式注入；覆盖值经 [`Self::env_var`] 生效。
    pub fn with_env_overrides(mut self, env: HashMap<String, Option<String>>) -> Self {
        self.env = Some(env);
        self
    }

    /// 传 `Some` 固定 argv[1]，传 `None` 回退真实 argv。
    pub fn with_invoked_path(mut self, invoked_path: Option<String>) -> Self {
        self.invoked_path = invoked_path;
        self
    }

    /// 传 `Some` 固定 execPath，传 `None` 回退当前可执行文件。
    pub fn with_exec_path(mut self, exec_path: Option<String>) -> Self {
        self.exec_path = exec_path;
        self
    }

    /// 覆盖 `getCurrentPackagePath()` 的返回值。
    pub fn with_package_root(mut self, package_root: PathBuf) -> Self {
        self.package_root = Some(package_root);
        self
    }

    /// JS `process.env[key]` through the env seam.
    /// 覆盖表优先于真实环境变量。
    pub(crate) fn env_var(&self, key: &str) -> Option<String> {
        if let Some(overrides) = &self.env
            && let Some(value) = overrides.get(key)
        {
            return value.clone();
        }
        std::env::var(key).ok()
    }

    /// JS `process.argv?.[1]`.
    /// 入口脚本路径；无 argv 时为 `None`。
    pub(crate) fn invoked_path(&self) -> Option<String> {
        self.invoked_path
            .clone()
            .or_else(|| std::env::args().nth(1))
    }

    /// JS `process.execPath`.
    /// 当前进程可执行文件路径；取不到时为 `None`。
    pub(crate) fn exec_path(&self) -> Option<String> {
        self.exec_path.clone().or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        })
    }

    /// JS `getCurrentPackagePath()` — the web package root.
    /// 测试未覆盖时用 `default_package_root()`。
    pub(crate) fn package_root(&self) -> PathBuf {
        self.package_root
            .clone()
            .unwrap_or_else(default_package_root)
    }

    /// 探测逻辑比较安装路径前缀时使用。
    pub(crate) fn current_package_path_string(&self) -> String {
        self.package_root().to_string_lossy().into_owned()
    }
}

/// 默认行为与生产构造器一致。
impl Default for PackageManagerRuntime {
    /// 委托 [`PackageManagerRuntime::new`]。
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide default runtime (the JS module singleton).
/// 首次访问惰性初始化，之后所有调用共享同一实例。
pub fn shared() -> &'static PackageManagerRuntime {
    // 惰性初始化的进程级单例；LazyLock 保证并发安全。
    static SHARED: LazyLock<PackageManagerRuntime> = LazyLock::new(PackageManagerRuntime::new);
    &SHARED
}

/// The JS module owns no routes; `openchamber-routes.js` (ported in
/// `opencode_meta`) registers `/api/ompchamber/update-check` and
/// `/api/ompchamber/update-install` on top of this library.
/// 返回空 Router 仅为与其它模块的 `router(ctx)` 接口保持一致。
pub fn router(_ctx: RouterContext) -> axum::Router {
    axum::Router::new()
}
