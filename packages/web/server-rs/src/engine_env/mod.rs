//! Port of the OpenCode environment/launch-support runtime:
//! `server/lib/opencode/env-runtime.js`, its `path-utils.js` dependency, the
//! PATH-augmentation half of `server/lib/opencode/server-utils-runtime.js`,
//! and `server/lib/opencode/managed-process-registry.js`.
//!
//! JS file → Rust file:
//! - `opencode/path-utils.js`
//!   → [`path_utils`] (`pathLooksUserConfigured`, `mergePathValues`)
//! - `opencode/env-runtime.js`
//!   → [`env_runtime`] (login-shell env snapshot incl. the Windows
//!   PowerShell/`cmd set` probes, `isExecutable`/`searchPathFor`, binary
//!   resolution for the omp-host runtime (bun) + node + bun + git with
//!   caching and `clearResolvedOpenCodeBinary`, shebang shim handling, and
//!   `resolveManagedOpenCodeLaunchSpec` with the full Windows wrapper chain:
//!   node_modules native binary → node-launcher → node/bun shebang →
//!   cmd-wrapper → executable passthrough)
//! - `server-utils-runtime.js` (`buildAugmentedPath`,
//!   `buildManagedOpenCodePath`, `buildWindowsManagedToolchainPath`,
//!   `getEnvValue`; `getLoginShellPath` from `server/index.js`)
//!   → [`env_runtime::EnvRuntime::build_augmented_path`] /
//!   [`env_runtime::EnvRuntime::build_managed_open_code_path`]
//! - `opencode/managed-process-registry.js`
//!   → [`managed_process_registry`] (per-pid `<pid>.json` records under
//!   `$OMPCHAMBER_MANAGED_PROCESS_REGISTRY` or
//!   `~/.config/ompchamber/managed-opencode`, register/unregister, and the
//!   startup reaper that kills only verified orphans — Unix `ps` ppid/command
//!   identification, Windows `tasklist` image identification, TERM→KILL
//!   escalation, `taskkill /T /F` on Windows)
//!
//! This is a service module, not a route family — like `inherited_env` and
//! `path_realpath_cache` it exposes functions and runtime handles instead of
//! a `router(ctx)`.
//!
//! # Wiring points (for the composition root; engine.rs is NOT edited here)
//!
//! 1. **Startup reaper** — JS `lifecycle.js` (~line 1070) calls
//!    `reapOrphanedProcesses({ log })` before spawning a new engine. Rust:
//!    `engine_env::managed_process_registry::default_registry()
//!    .reap_orphaned_processes(Some(log_fn)).await` before the managed spawn.
//! 2. **Engine spawn env** — JS `lifecycle.js` builds the child env from
//!    `buildManagedOpenCodePath()` (falling back to `buildAugmentedPath()`)
//!    plus the login-shell snapshot (`getManagedOpenCodeShellEnvSnapshot`).
//!    Rust: construct one `EnvRuntime`, `apply_login_shell_env_snapshot()`,
//!    then use `effective_env()` with `PATH` set to
//!    `build_managed_open_code_path().await` (or `build_augmented_path()`).
//!    This replaces engine.rs's interim conservative `augment_path`
//!    (engine.rs:151) at cutover.
//! 3. **Registration** — JS `lifecycle.js` (~line 525) records the child once
//!    the readiness line landed (never before, so a failed spawn stays
//!    untracked and killed): `register_managed_process(Some(pid),
//!    Some(std::process::id() as i64), Some(port), Some(binary),
//!    runtime)` with `runtime` = `OMPCHAMBER_RUNTIME` env or `"web"`.
//! 4. **Teardown** — JS `lifecycle.js` `closeManagedOpenCodeChild` (~line 358)
//!    terminates the child and unregisters ONLY after it actually exited (a
//!    child that survived teardown stays eligible for the next run's reaper):
//!    `unregister_managed_process(Some(pid)).await` in the shutdown path.
//! 5. **Binary re-resolution** — when the configured omp-host runtime binary
//!    changes (JS calls `clearResolvedOpenCodeBinary` on restart), call
//!    `EnvRuntime::clear_resolved_open_code_binary()` so the next
//!    `ensure_opencode_cli_env()` re-resolves.
//!
//! `resolveGitBinaryForSpawn` and `searchPathFor`/`isExecutable` are also the
//! seams the JS fs/git and terminal runtimes borrow; the Rust git_service
//! already carries its own `git_binary()` resolution.
//!
//! 中文说明：OpenCode 环境/启动支持运行时的 Rust 移植总模块，聚合三个子模块
//! 并统一再导出对外 API：`env_runtime`（登录 shell 环境快照、二进制解析与
//! Windows 包装链）、`managed_process_registry`（托管进程注册表与孤儿回收）、
//! `path_utils`（PATH 启发式）。这是一个服务型模块而非路由族——如同
//! `inherited_env`、`path_realpath_cache`，暴露的是函数与运行时句柄而不是
//! `router(ctx)`。JS 源文件与 Rust 文件的逐项对应、以及 composition root
//! 的五个接线点（启动回收、引擎 spawn 环境、注册、注销、二进制重解析）
//! 见上方英文说明。

/// 登录 shell 环境快照、可执行文件判定/PATH 搜索、omp-host 运行时与
/// node/bun/git 二进制解析（含缓存、清理与 shebang shim 处理），以及带
/// Windows 包装链的托管启动规格解析（`env-runtime.js` 的移植）。
pub mod env_runtime;
/// 托管 OpenCode 进程注册表：每 pid 一个 `<pid>.json` 记录 + 启动时孤儿
/// 回收（`managed-process-registry.js` 的移植，详见子模块文档）。
pub mod managed_process_registry;
/// PATH 纯字符串工具：用户配置判定与保序合并（`path-utils.js` 的移植）。
pub mod path_utils;

// 再导出 env_runtime 的公开类型与函数（组合根经本模块统一取用）。
pub use env_runtime::{
    BinarySource, EnvRuntime, ManagedLaunchSpec, Platform, SpawnFn, SpawnOptions, SpawnOutput,
    WrapperType, host_platform,
};
// 再导出托管进程注册表的公开 API。
pub use managed_process_registry::{
    ManagedProcessRegistry, ReapSummary, RegistryEntry, RegistryFs, command_identifies_our_server,
    default_registry, resolve_registry_dir,
};
// 再导出 PATH 工具函数。
pub use path_utils::{merge_path_values, path_looks_user_configured};

/// re-export 接线测试：三个子模块的关键 API 都能经由本模块访问。
#[cfg(test)]
mod tests {
    use super::*;

    /// The JS module contract the JS tests pin: pure helpers compose into the
    /// runtime's PATH strategies.
    /// 中文：JS 测试所固定的模块契约——纯辅助函数可组合进运行时的
    /// PATH 策略。
    #[test]
    fn reexports_are_wired() {
        assert_eq!(merge_path_values("/a", "/b", ':'), "/a:/b");
        assert!(path_looks_user_configured(
            "/opt/homebrew/bin",
            "/home/u",
            ':'
        ));
        assert!(command_identifies_our_server(
            "omp-host.exe serve --port 58941",
            Some(58941)
        ));
        #[cfg(unix)]
        assert_eq!(host_platform(), Platform::Unix);
        #[cfg(windows)]
        assert_eq!(host_platform(), Platform::Windows);
        let _ = default_registry();
    }
}
