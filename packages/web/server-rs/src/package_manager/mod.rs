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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use crate::context::RouterContext;

pub mod detect;
pub mod http;
pub mod paths;
pub mod spawn;
pub mod update;

#[cfg(test)]
mod tests;

pub use detect::{
    PackageManagerDetails, clear_detection_cache, detect_package_manager_from_install_path,
    detect_package_manager_from_invocation_path, detect_package_manager_from_runtime_path,
    quote_command,
};
pub use update::{CheckForUpdatesOptions, ExecuteUpdateOptions, UpdateExecution, UpdateInfo};

const PACKAGE_NAME: &str = "@ompchamber/web";
const GITHUB_REPO: &str = "Reamd7/ompchamber";
const CHANGELOG_URL: &str = "https://raw.githubusercontent.com/Reamd7/ompchamber/main/CHANGELOG.md";
const GITHUB_RELEASES_URL: &str = "https://github.com/Reamd7/ompchamber/releases";
const GITHUB_RELEASES_API_URL: &str = "https://api.github.com/repos/Reamd7/ompchamber/releases";
// OMPChamber is distributed as release tarballs, not on the npm registry;
// the version-less URL always resolves to the newest published release.
const RELEASE_TARBALL_LATEST_URL: &str =
    "https://github.com/Reamd7/ompchamber/releases/latest/download/ompchamber-web-latest.tgz";

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// JS `__dirname` is `<web package>/server/lib`, so the "current package" is
/// the web package root — here `<web package>/server-rs/..`.
fn default_package_root() -> PathBuf {
    paths::resolve_lexically(&crate_dir().join(".."))
}

/// Injected environment + runtime identity for the JS module state.
///
/// Construction is cheap and stateless apart from the process-wide detection
/// cache (JS `cachedDetectedPm`); share one instance via [`shared`].
pub struct PackageManagerRuntime {
    runner: Arc<dyn spawn::CommandRunner>,
    transport: Arc<dyn http::HttpTransport>,
    /// Test seam: `Some(value)` forces the var, `None` forces unset, missing
    /// keys fall through to the real environment.
    env: Option<HashMap<String, Option<String>>>,
    /// `process.argv[1]` override (`None` → real argv).
    invoked_path: Option<String>,
    /// `process.execPath` override (`None` → current executable).
    exec_path: Option<String>,
    /// JS `getCurrentPackagePath()` override (tests pin a fake install).
    package_root: Option<PathBuf>,
}

impl PackageManagerRuntime {
    /// Production runtime: tokio spawns + reqwest (rustls) transport.
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
    pub fn with_env_overrides(mut self, env: HashMap<String, Option<String>>) -> Self {
        self.env = Some(env);
        self
    }

    pub fn with_invoked_path(mut self, invoked_path: Option<String>) -> Self {
        self.invoked_path = invoked_path;
        self
    }

    pub fn with_exec_path(mut self, exec_path: Option<String>) -> Self {
        self.exec_path = exec_path;
        self
    }

    pub fn with_package_root(mut self, package_root: PathBuf) -> Self {
        self.package_root = Some(package_root);
        self
    }

    /// JS `process.env[key]` through the env seam.
    pub(crate) fn env_var(&self, key: &str) -> Option<String> {
        if let Some(overrides) = &self.env
            && let Some(value) = overrides.get(key)
        {
            return value.clone();
        }
        std::env::var(key).ok()
    }

    /// JS `process.argv?.[1]`.
    pub(crate) fn invoked_path(&self) -> Option<String> {
        self.invoked_path
            .clone()
            .or_else(|| std::env::args().nth(1))
    }

    /// JS `process.execPath`.
    pub(crate) fn exec_path(&self) -> Option<String> {
        self.exec_path.clone().or_else(|| {
            std::env::current_exe()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        })
    }

    /// JS `getCurrentPackagePath()` — the web package root.
    pub(crate) fn package_root(&self) -> PathBuf {
        self.package_root
            .clone()
            .unwrap_or_else(default_package_root)
    }

    pub(crate) fn current_package_path_string(&self) -> String {
        self.package_root().to_string_lossy().into_owned()
    }
}

impl Default for PackageManagerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide default runtime (the JS module singleton).
pub fn shared() -> &'static PackageManagerRuntime {
    static SHARED: LazyLock<PackageManagerRuntime> = LazyLock::new(PackageManagerRuntime::new);
    &SHARED
}

/// The JS module owns no routes; `openchamber-routes.js` (ported in
/// `opencode_meta`) registers `/api/ompchamber/update-check` and
/// `/api/ompchamber/update-install` on top of this library.
pub fn router(_ctx: RouterContext) -> axum::Router {
    axum::Router::new()
}
