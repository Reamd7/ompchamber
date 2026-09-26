//! Port of the package-manager detection half of `package-manager.js`:
//! `detectPackageManagerDetails()` and its probe helpers (command resolution,
//! global bin/root discovery, install ownership, env hints). Mirrors the JS
//! decision order exactly; the result is cached process-wide like the JS
//! module-level `cachedDetectedPm`.

use std::path::PathBuf;
use std::time::Duration;

use super::PackageManagerRuntime;
use super::paths::{
    get_comparable_paths, get_unique_paths, is_windows, path_set_contains, resolve_lexically,
};
use super::spawn::CommandOutput;
use serde::Serialize;

/// JS `detectPackageManagerDetails()` result.
#[derive(Debug, Clone, Serialize)]
pub struct PackageManagerDetails {
    #[serde(rename = "packageManager")]
    pub package_manager: String,
    pub reason: String,
    #[serde(rename = "packagePath")]
    pub package_path: Option<String>,
    #[serde(rename = "packageManagerCommand")]
    pub package_manager_command: Option<String>,
    #[serde(rename = "globalNodeModulesRoot")]
    pub global_node_modules_root: Option<String>,
}

/// JS module-level `cachedDetectedPm`.
static CACHED_DETECTED_PM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Test/admin seam for the JS `vi.resetModules()` reset of the module cache.
pub fn clear_detection_cache() {
    *CACHED_DETECTED_PM.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn cached_detected_pm() -> Option<String> {
    CACHED_DETECTED_PM
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

fn set_cached_detected_pm(pm: &str) {
    *CACHED_DETECTED_PM.lock().unwrap_or_else(|e| e.into_inner()) = Some(pm.to_string());
}

const KNOWN_PMS: [&str; 4] = ["npm", "pnpm", "yarn", "bun"];

/// JS `detectPackageManagerFromInstallPath`.
pub fn detect_package_manager_from_install_path(pkg_path: Option<&str>) -> Option<&'static str> {
    let pkg_path = pkg_path?;
    let normalized = pkg_path.replace('\\', "/").to_lowercase();
    if normalized.contains("/.pnpm/") || normalized.contains("/pnpm/") {
        return Some("pnpm");
    }
    if normalized.contains("/.yarn/") {
        return Some("yarn");
    }
    if normalized.contains("/.bun/") || normalized.contains("/bun/install/") {
        return Some("bun");
    }
    if normalized.contains("/node_modules/") {
        return Some("npm");
    }
    None
}

/// JS `detectPackageManagerFromRuntimePath` (`process.execPath`).
pub fn detect_package_manager_from_runtime_path(
    runtime_path: Option<&str>,
) -> Option<&'static str> {
    let runtime_path = runtime_path?;
    let normalized = runtime_path.replace('\\', "/").to_lowercase();
    if normalized.contains("/.bun/bin/bun")
        || normalized.ends_with("/bun")
        || normalized.ends_with("/bun.exe")
    {
        return Some("bun");
    }
    if normalized.contains("/pnpm/") {
        return Some("pnpm");
    }
    if normalized.contains("/yarn/") {
        return Some("yarn");
    }
    if normalized.contains("/node") || normalized.ends_with("/node.exe") {
        return Some("npm");
    }
    None
}

/// JS `detectPackageManagerFromInvocationPath` (`process.argv[1]`).
pub fn detect_package_manager_from_invocation_path(
    invoked_path: Option<&str>,
) -> Option<&'static str> {
    let invoked_path = invoked_path?;
    let normalized = invoked_path.replace('\\', "/").to_lowercase();
    if normalized.contains("/.bun/bin/") {
        return Some("bun");
    }
    if normalized.contains("/.pnpm/") {
        return Some("pnpm");
    }
    if normalized.contains("/.yarn/") {
        return Some("yarn");
    }
    None
}

/// JS `quoteCommand`.
pub fn quote_command(command: &str) -> String {
    if command.is_empty() {
        return command.to_string();
    }
    if !command.chars().any(|c| c.is_whitespace()) {
        return command.to_string();
    }
    if is_windows() {
        format!("\"{}\"", command.replace('"', "\"\""))
    } else {
        format!("'{}'", command.replace('\'', "'\\''"))
    }
}

impl PackageManagerRuntime {
    /// JS `getCommandOutput`: status-0 stdout (trimmed), else `None`.
    pub(crate) async fn get_command_output(&self, command: &str, args: &[&str]) -> Option<String> {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        let result: Option<CommandOutput> = self
            .runner
            .run(command, &args, Duration::from_secs(10))
            .await;
        let output = result?;
        if output.status != 0 {
            return None;
        }
        let stdout = output.stdout.trim().to_string();
        (!stdout.is_empty()).then_some(stdout)
    }

    /// JS `isCommandAvailable` (`spawnSync(command, ['--version'])`).
    pub(crate) async fn is_command_available(&self, command: &str) -> bool {
        let args = vec!["--version".to_string()];
        match self
            .runner
            .run(command, &args, Duration::from_secs(5))
            .await
        {
            Some(output) => output.status == 0,
            None => false,
        }
    }

    /// JS `isPackageInstalledWith`.
    pub(crate) async fn is_package_installed_with(&self, pm: &str) -> bool {
        let pm_command = self.resolve_package_manager_command(pm).await;
        let args: Vec<String> = match pm {
            "pnpm" => vec!["list", "-g", "--depth=0", super::PACKAGE_NAME]
                .into_iter()
                .map(String::from)
                .collect(),
            "yarn" => vec!["global", "list", "--depth=0"]
                .into_iter()
                .map(String::from)
                .collect(),
            "bun" => vec!["pm", "ls", "-g"]
                .into_iter()
                .map(String::from)
                .collect(),
            _ => vec!["list", "-g", "--depth=0", super::PACKAGE_NAME]
                .into_iter()
                .map(String::from)
                .collect(),
        };
        match self
            .runner
            .run(&pm_command, &args, Duration::from_secs(10))
            .await
        {
            Some(output) => {
                output.status == 0
                    && (output.stdout.contains(super::PACKAGE_NAME)
                        || output.stdout.contains("ompchamber"))
            }
            None => false,
        }
    }

    /// JS `getPackageManagerCommandCandidates`.
    fn pm_command_candidates(&self, pm: &str) -> Vec<String> {
        let mut candidates: Vec<String> = Vec::new();
        if pm == "bun" {
            let bun_executable = if is_windows() { "bun.exe" } else { "bun" };
            if let Some(bun_install) = self.env_var("BUN_INSTALL")
                && !bun_install.is_empty()
            {
                candidates.push(
                    PathBuf::from(&bun_install)
                        .join("bin")
                        .join(bun_executable)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            if let Some(home) = self.env_var("HOME")
                && !home.is_empty()
            {
                candidates.push(
                    PathBuf::from(&home)
                        .join(".bun")
                        .join("bin")
                        .join(bun_executable)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            if let Some(user_profile) = self.env_var("USERPROFILE")
                && !user_profile.is_empty()
            {
                candidates.push(
                    PathBuf::from(&user_profile)
                        .join(".bun")
                        .join("bin")
                        .join(bun_executable)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        candidates.push(pm.to_string());
        let mut unique: Vec<String> = Vec::new();
        for candidate in candidates {
            if !candidate.is_empty() && !unique.contains(&candidate) {
                unique.push(candidate);
            }
        }
        unique
    }

    /// JS `resolvePackageManagerCommand`.
    pub(crate) async fn resolve_package_manager_command(&self, pm: &str) -> String {
        let candidates = self.pm_command_candidates(pm);
        for candidate in &candidates {
            if self.is_command_available(candidate).await {
                return candidate.clone();
            }
        }
        pm.to_string()
    }

    /// JS `getGlobalBinDirs`.
    async fn get_global_bin_dirs(&self, pm: &str) -> Vec<PathBuf> {
        let pm_command = self.resolve_package_manager_command(pm).await;
        if !self.is_command_available(&pm_command).await {
            return Vec::new();
        }

        let mut dirs: Vec<String> = Vec::new();
        match pm {
            "pnpm" => {
                if let Some(pnpm_bin) = self.get_command_output(&pm_command, &["bin", "-g"]).await {
                    dirs.push(pnpm_bin);
                }
                if let Some(pnpm_prefix) = self
                    .get_command_output(&pm_command, &["prefix", "-g"])
                    .await
                {
                    dirs.push(
                        if is_windows() {
                            PathBuf::from(&pnpm_prefix)
                        } else {
                            PathBuf::from(&pnpm_prefix).join("bin")
                        }
                        .to_string_lossy()
                        .into_owned(),
                    );
                }
            }
            "yarn" => {
                if let Some(yarn_bin) = self
                    .get_command_output(&pm_command, &["global", "bin"])
                    .await
                {
                    dirs.push(yarn_bin);
                }
            }
            "bun" => {
                if let Some(bun_bin) = self
                    .get_command_output(&pm_command, &["pm", "bin", "-g"])
                    .await
                {
                    dirs.push(bun_bin);
                }
            }
            _ => {
                if let Some(npm_prefix) = self
                    .get_command_output(&pm_command, &["prefix", "-g"])
                    .await
                {
                    dirs.push(
                        if is_windows() {
                            PathBuf::from(&npm_prefix)
                        } else {
                            PathBuf::from(&npm_prefix).join("bin")
                        }
                        .to_string_lossy()
                        .into_owned(),
                    );
                }
            }
        }

        get_unique_paths(&dirs)
    }

    /// JS `getGlobalNodeModulesRoots`.
    async fn get_global_node_modules_roots(&self, pm: &str) -> Vec<PathBuf> {
        let pm_command = self.resolve_package_manager_command(pm).await;
        if !self.is_command_available(&pm_command).await {
            return Vec::new();
        }

        let mut roots: Vec<String> = Vec::new();
        match pm {
            "pnpm" => {
                if let Some(pnpm_root) = self.get_command_output(&pm_command, &["root", "-g"]).await
                {
                    roots.push(pnpm_root);
                }
                if let Some(pnpm_prefix) = self
                    .get_command_output(&pm_command, &["prefix", "-g"])
                    .await
                {
                    roots.push(
                        if is_windows() {
                            PathBuf::from(&pnpm_prefix).join("node_modules")
                        } else {
                            PathBuf::from(&pnpm_prefix).join("lib").join("node_modules")
                        }
                        .to_string_lossy()
                        .into_owned(),
                    );
                }
            }
            "yarn" => {
                if let Some(yarn_dir) = self
                    .get_command_output(&pm_command, &["global", "dir"])
                    .await
                {
                    roots.push(
                        PathBuf::from(yarn_dir)
                            .join("node_modules")
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
            "bun" => {
                if let Some(bun_bin_dir) = self
                    .get_command_output(&pm_command, &["pm", "bin", "-g"])
                    .await
                {
                    let bin = PathBuf::from(&bun_bin_dir);
                    roots.push(
                        resolve_lexically(
                            &bin.join("..")
                                .join("install")
                                .join("global")
                                .join("node_modules"),
                        )
                        .to_string_lossy()
                        .into_owned(),
                    );
                    roots.push(
                        resolve_lexically(&bin.join("..").join("..").join("node_modules"))
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
            _ => {
                if let Some(npm_root) = self.get_command_output(&pm_command, &["root", "-g"]).await
                {
                    roots.push(npm_root);
                }
                if let Some(npm_prefix) = self
                    .get_command_output(&pm_command, &["prefix", "-g"])
                    .await
                {
                    roots.push(
                        if is_windows() {
                            PathBuf::from(&npm_prefix).join("node_modules")
                        } else {
                            PathBuf::from(&npm_prefix).join("lib").join("node_modules")
                        }
                        .to_string_lossy()
                        .into_owned(),
                    );
                }
            }
        }

        get_unique_paths(&roots)
    }

    /// JS `getOwnedPackagePathsFromGlobalBins`.
    async fn get_owned_package_paths_from_global_bins(&self, pm: &str) -> Vec<PathBuf> {
        let mut package_paths: Vec<String> = Vec::new();
        for bin_dir in self.get_global_bin_dirs(pm).await {
            let binary_name = if is_windows() {
                "ompchamber.cmd"
            } else {
                "ompchamber"
            };
            let binary_path = bin_dir.join(binary_name);
            if !binary_path.exists() {
                continue;
            }
            if let Ok(real_binary_path) = std::fs::canonicalize(&binary_path) {
                package_paths.push(
                    real_binary_path
                        .join("..")
                        .join("..")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        get_unique_paths(&package_paths)
    }

    /// JS `packageManagerOwnsCurrentInstall`.
    async fn package_manager_owns_current_install(&self, pm: &str) -> bool {
        let current_package_path = self.current_package_path_string();
        let current_package_paths = get_comparable_paths(&current_package_path);
        let mut candidate_paths: Vec<Option<PathBuf>> = self
            .get_global_node_modules_roots(pm)
            .await
            .into_iter()
            .map(|root| Some(root.join("@ompchamber").join("web")))
            .collect();
        candidate_paths.extend(
            self.get_owned_package_paths_from_global_bins(pm)
                .await
                .into_iter()
                .map(Some),
        );

        for candidate_path in candidate_paths.into_iter().flatten() {
            let candidate = candidate_path.to_string_lossy().into_owned();
            if path_set_contains(&current_package_paths, &get_comparable_paths(&candidate)) {
                return true;
            }
        }
        false
    }

    /// JS `getGlobalNodeModulesRoots(pm)[0] || null` rendered for details.
    async fn first_global_node_modules_root(&self, pm: &str) -> Option<String> {
        self.get_global_node_modules_roots(pm)
            .await
            .into_iter()
            .next()
            .map(|root| root.to_string_lossy().into_owned())
    }

    /// The shared details body for every successful detection branch.
    async fn resolved_details(&self, pm: &str, reason: &str) -> PackageManagerDetails {
        PackageManagerDetails {
            package_manager: pm.to_string(),
            reason: reason.to_string(),
            package_path: Some(self.current_package_path_string()),
            package_manager_command: Some(self.resolve_package_manager_command(pm).await),
            global_node_modules_root: self.first_global_node_modules_root(pm).await,
        }
    }

    /// JS `detectPackageManagerDetails()` — the full decision tree.
    pub async fn detect_package_manager_details(&self) -> PackageManagerDetails {
        // Desktop (Electron) runtime: detection is worthless there and every
        // spawnSync probe would block the main event loop. Short-circuit.
        if self.env_var("OMPCHAMBER_RUNTIME").as_deref() == Some("desktop") {
            return PackageManagerDetails {
                package_manager: "electron".to_string(),
                reason: "desktop-runtime".to_string(),
                package_path: None,
                package_manager_command: None,
                global_node_modules_root: None,
            };
        }

        if let Some(cached) = cached_detected_pm() {
            return self.resolved_details(&cached, "cached").await;
        }

        let forced_pm = self
            .env_var("OMPCHAMBER_PACKAGE_MANAGER")
            .map(|value| value.trim().to_string())
            .unwrap_or_default();
        if KNOWN_PMS.contains(&forced_pm.as_str()) {
            let forced_pm_command = self.resolve_package_manager_command(&forced_pm).await;
            if self.is_command_available(&forced_pm_command).await {
                set_cached_detected_pm(&forced_pm);
                return self.resolved_details(&forced_pm, "forced-env").await;
            }
        }

        // First prefer the package manager that demonstrably owns the current
        // install.
        let install_path_pm =
            detect_package_manager_from_install_path(Some(&self.current_package_path_string()));
        if let Some(install_path_pm) = install_path_pm
            && self
                .package_manager_owns_current_install(install_path_pm)
                .await
        {
            set_cached_detected_pm(install_path_pm);
            return self
                .resolved_details(install_path_pm, "install-path-owner")
                .await;
        }

        for candidate in ["pnpm", "yarn", "bun", "npm"] {
            if self.package_manager_owns_current_install(candidate).await {
                set_cached_detected_pm(candidate);
                return self.resolved_details(candidate, "global-root-owner").await;
            }
        }

        // Fall back to weaker hints only when ownership cannot be established.
        let user_agent = self.env_var("npm_config_user_agent").unwrap_or_default();
        let mut hinted_pm: Option<&'static str> = None;
        if user_agent.starts_with("pnpm") {
            hinted_pm = Some("pnpm");
        } else if user_agent.starts_with("yarn") {
            hinted_pm = Some("yarn");
        } else if user_agent.starts_with("bun") {
            hinted_pm = Some("bun");
        } else if user_agent.starts_with("npm") {
            hinted_pm = Some("npm");
        }

        let exec_path = self.env_var("npm_execpath").unwrap_or_default();
        if hinted_pm.is_none() {
            if exec_path.contains("pnpm") {
                hinted_pm = Some("pnpm");
            } else if exec_path.contains("yarn") {
                hinted_pm = Some("yarn");
            } else if exec_path.contains("bun") {
                hinted_pm = Some("bun");
            } else if exec_path.contains("npm") {
                hinted_pm = Some("npm");
            }
        }

        let invoked_pm =
            detect_package_manager_from_invocation_path(self.invoked_path().as_deref());
        if hinted_pm.is_none() {
            hinted_pm = invoked_pm;
        }

        if hinted_pm.is_none() {
            hinted_pm = install_path_pm;
        }

        // Validate the hint against package visibility, but only after
        // ownership checks failed.
        if let Some(hinted_pm) = hinted_pm {
            let command = self.resolve_package_manager_command(hinted_pm).await;
            if self.is_command_available(&command).await
                && self.is_package_installed_with(hinted_pm).await
            {
                set_cached_detected_pm(hinted_pm);
                return self
                    .resolved_details(hinted_pm, "hinted-visible-install")
                    .await;
            }
        }

        let runtime_pm = detect_package_manager_from_runtime_path(self.exec_path().as_deref());
        if let Some(runtime_pm) = runtime_pm {
            let command = self.resolve_package_manager_command(runtime_pm).await;
            if self.is_command_available(&command).await
                && self.is_package_installed_with(runtime_pm).await
            {
                set_cached_detected_pm(runtime_pm);
                return self
                    .resolved_details(runtime_pm, "runtime-visible-install")
                    .await;
            }
        }
        // Last resort: pick a PM that can at least see the package.
        for candidate in ["pnpm", "yarn", "bun", "npm"] {
            let command = self.resolve_package_manager_command(candidate).await;
            if self.is_command_available(&command).await
                && self.is_package_installed_with(candidate).await
            {
                set_cached_detected_pm(candidate);
                return self
                    .resolved_details(candidate, "last-resort-visible-install")
                    .await;
            }
        }

        set_cached_detected_pm("npm");
        self.resolved_details("npm", "default-fallback").await
    }

    /// JS `detectPackageManager()`.
    pub async fn detect_package_manager(&self) -> String {
        self.detect_package_manager_details().await.package_manager
    }
}
