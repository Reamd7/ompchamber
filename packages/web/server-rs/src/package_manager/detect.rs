//! Port of the package-manager detection half of `package-manager.js`:
//! `detectPackageManagerDetails()` and its probe helpers (command resolution,
//! global bin/root discovery, install ownership, env hints). Mirrors the JS
//! decision order exactly; the result is cached process-wide like the JS
//! module-level `cachedDetectedPm`.
//!
//! 中文说明：移植 `package-manager.js` 的包管理器探测半边：
//! `detectPackageManagerDetails()` 及其探针（命令解析、全局
//! bin/root 发现、安装归属判定、环境变量提示）。判定顺序与 JS
//! 完全一致，且像 JS 模块级 `cachedDetectedPm` 一样做进程级缓存。

use std::path::PathBuf;
use std::time::Duration;

use super::PackageManagerRuntime;
use super::paths::{
    get_comparable_paths, get_unique_paths, is_windows, path_set_contains, resolve_lexically,
};
use super::spawn::CommandOutput;
use serde::Serialize;

/// JS `detectPackageManagerDetails()` result.
/// 中文：`detectPackageManagerDetails()` 的结果体（序列化为 JS 同名
/// camelCase 字段），含判定原因 reason 与包路径/命令/全局 node_modules
/// 根等补充信息。
#[derive(Debug, Clone, Serialize)]
pub struct PackageManagerDetails {
    #[serde(rename = "packageManager")]
    /// 判定出的包管理器名（npm/pnpm/yarn/bun/electron）。
    pub package_manager: String,
    /// 判定原因标签（cached/forced-env/install-path-owner/...）。
    pub reason: String,
    #[serde(rename = "packagePath")]
    /// 当前安装包的路径（desktop 短路时为 None）。
    pub package_path: Option<String>,
    #[serde(rename = "packageManagerCommand")]
    /// 解析出的包管理器可执行命令。
    pub package_manager_command: Option<String>,
    #[serde(rename = "globalNodeModulesRoot")]
    /// 首个全局 node_modules 根目录。
    pub global_node_modules_root: Option<String>,
}

/// JS module-level `cachedDetectedPm`.
/// 中文：进程级缓存的探测结果（对应 JS 模块级 `cachedDetectedPm`）。
static CACHED_DETECTED_PM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Test/admin seam for the JS `vi.resetModules()` reset of the module cache.
/// 中文：清空探测缓存；测试/管理用途，对应 JS `vi.resetModules()`
/// 对模块级缓存的复位。
pub fn clear_detection_cache() {
    *CACHED_DETECTED_PM.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 中文：读取缓存中的包管理器名（未缓存返回 None）。
fn cached_detected_pm() -> Option<String> {
    CACHED_DETECTED_PM
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// 中文：写入进程级缓存。
fn set_cached_detected_pm(pm: &str) {
    *CACHED_DETECTED_PM.lock().unwrap_or_else(|e| e.into_inner()) = Some(pm.to_string());
}

/// 中文：参与判定的已知包管理器名单（含 `$OMPCHAMBER_PACKAGE_MANAGER`
/// 强制值的合法性校验）。
const KNOWN_PMS: [&str; 4] = ["npm", "pnpm", "yarn", "bun"];

/// JS `detectPackageManagerFromInstallPath`.
/// 中文：由安装路径特征推断包管理器：路径含 `/.pnpm/` 或 `/pnpm/`
/// 判 pnpm，`/.yarn/` 判 yarn，`/.bun/` 或 `/bun/install/` 判 bun，
/// `/node_modules/` 判 npm；均不匹配或入参为 None 返回 None。
/// 反斜杠先归一化（兼容 Windows 路径）。
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
/// 中文：由运行时可执行路径（JS `process.execPath`）推断包管理器：
/// bun/pnpm/yarn/node 的路径特征逐一匹配，都不中返回 None。
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
/// 中文：由调用入口路径（JS `process.argv[1]`）推断包管理器：
/// `/.bun/bin/`、`/.pnpm/`、`/.yarn/` 三种特征，均不中返回 None。
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
/// 中文：命令引用处理（JS `quoteCommand`）：无空白原样返回；
/// 有空白时 Windows 用双引号并把内部 `"` 加倍，类 Unix 用单引号并把
/// 内部 `'` 转义为 `'\''`。
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

/// 中文：包管理器探测的运行时方法集：探针执行、命令解析、全局目录
/// 发现与归属判定，最终汇入 `detect_package_manager_details`。
impl PackageManagerRuntime {
    /// JS `getCommandOutput`: status-0 stdout (trimmed), else `None`.
    /// 中文：执行命令并取 trim 后的 stdout（JS `getCommandOutput`）：
    /// 10 秒上限、非零退出码、无输出或 spawn 失败一律返回 None。
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
    /// 中文：命令可用性探测（JS `isCommandAvailable`）：执行
    /// `command --version`（5 秒上限），退出码 0 即视为可用。
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
    /// 中文：判断本包是否被指定包管理器全局安装（JS
    /// `isPackageInstalledWith`）：按 PM 组装各自的 list 命令，
    /// 退出码 0 且 stdout 提到包名或 "ompchamber" 才算安装。
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
    /// 中文：包管理器命令候选（JS `getPackageManagerCommandCandidates`）：
    /// bun 额外尝试 `$BUN_INSTALL/bin`、`$HOME/.bun/bin`、
    /// `$USERPROFILE/.bun/bin` 下的绝对路径，最后总追加分包管理器名；
    /// 去重去空后按序返回。
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
    /// 中文：解析包管理器实际可用的命令（JS `resolvePackageManagerCommand`）：
    /// 逐个候选做可用性探测，第一个可用者胜出，全不可用回退裸名。
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
    /// 中文：发现包管理器的全局 bin 目录（JS `getGlobalBinDirs`）：按
    /// PM 执行各自的 bin/prefix 查询命令（pnpm `bin -g` + `prefix -g`、
    /// yarn `global bin`、bun `pm bin -g`、npm `prefix -g`；Windows 下
    /// prefix 本身即 bin，类 Unix 需拼 `bin`），去重返回。
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
    /// 中文：发现全局 node_modules 根（JS `getGlobalNodeModulesRoots`）：
    /// 各 PM 的 root/dir 查询 + prefix 推导（npm/pnpm 类 Unix 为
    /// `<prefix>/lib/node_modules`、Windows 为 `<prefix>/node_modules`；
    /// yarn 为 `<global dir>/node_modules`； bun 由 bin 目录回溯两级推
    /// 两个候选），去重返回。
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
    /// 中文：从全局 bin 目录反查本包安装路径（JS
    /// `getOwnedPackagePathsFromGlobalBins`）：在 bin 目录下找
    /// `ompchamber`（Windows 为 `ompchamber.cmd`），canonicalize 后回溯
    /// 两级取包根；不存在的 bin 目录跳过。
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
    /// 中文：判断包管理器是否拥有当前安装（JS
    /// `packageManagerOwnsCurrentInstall`）：把全局 node_modules 根拼上
    /// `@ompchamber/web` 以及 bin 反查路径，与当前包路径做可比较
    /// （realpath 归一化）路径比对，命中即拥有。
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
    /// 中文：取首个全局 node_modules 根并渲染为字符串，供详情体使用；
    /// 无根时为 None。
    async fn first_global_node_modules_root(&self, pm: &str) -> Option<String> {
        self.get_global_node_modules_roots(pm)
            .await
            .into_iter()
            .next()
            .map(|root| root.to_string_lossy().into_owned())
    }

    /// The shared details body for every successful detection branch.
    /// 中文：所有成功判定分支共用的详情体：包路径、解析出的命令与
    /// 首个全局 node_modules 根。
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
    /// 中文：完整判定树（JS `detectPackageManagerDetails`），顺序：
    /// desktop 运行时短路为 electron → 进程缓存 →
    /// `$OMPCHAMBER_PACKAGE_MANAGER` 强制值（需命令可用）→ 安装路径
    /// 推断且归属成立 → pnpm/yarn/bun/npm 逐一归属判定 → 弱提示
    /// （npm_config_user_agent → npm_execpath → 调用路径 → 安装路径）
    /// 需命令可用且能看到安装 → 运行时路径同样验证 → 兜底扫描能看到
    /// 安装的 PM → 最终回退 npm。除 desktop/缓存外的每个命中都写入
    /// 进程级缓存。
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
    /// 中文：只取包管理器名（JS `detectPackageManager`）：详情体的
    /// `packageManager` 字段。
    pub async fn detect_package_manager(&self) -> String {
        self.detect_package_manager_details().await.package_manager
    }
}
