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

pub mod env_runtime;
pub mod managed_process_registry;
pub mod path_utils;

pub use env_runtime::{
    BinarySource, EnvRuntime, ManagedLaunchSpec, Platform, SpawnFn, SpawnOptions, SpawnOutput,
    WrapperType, host_platform,
};
pub use managed_process_registry::{
    ManagedProcessRegistry, ReapSummary, RegistryEntry, RegistryFs, command_identifies_our_server,
    default_registry, resolve_registry_dir,
};
pub use path_utils::{merge_path_values, path_looks_user_configured};

#[cfg(test)]
mod tests {
    use super::*;

    /// The JS module contract the JS tests pin: pure helpers compose into the
    /// runtime's PATH strategies.
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
