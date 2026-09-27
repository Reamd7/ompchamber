//! Port of the settings module: `server/lib/opencode/settings-runtime.js`,
//! `settings-helpers.js`, `settings-normalization-runtime.js` (plus the
//! tunnel type normalizers and `createProjectIdFromPath` they depend on) and
//! the `GET`/`PUT /api/config/settings` endpoints from `routes.js`.
//!
//! Persistence keeps settings.json as a raw JSON object so JS spread
//! semantics (unknown keys, undefined-clears) round-trip exactly; [`model::Settings`]
//! is the typed projection for consumers.
//!
//! 中文说明：本模块聚合 settings 相关子模块——runtime 负责读写与迁移，
//! helpers 负责响应格式化，normalization 负责 JS 语义清洗，routes 挂载
//! HTTP 端点；持久层始终保留原始 JSON map 以兼容 JS 展开语义。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::context::RouterContext;

/// settings 响应格式化与读取辅助（settings-helpers.js 的移植）。
pub mod helpers;
/// 持久化 settings 的类型化投影（见模块文档）。
pub mod model;
/// JS 语义的 settings 清洗运行时（settings-normalization-runtime.js 的移植）。
pub mod normalization;
/// `GET`/`PUT /api/config/settings` HTTP 端点。
pub mod routes;
/// settings 读写与迁移的运行时（settings-runtime.js 的移植）。
pub mod runtime;

/// 本模块的集成测试（仅测试构建）。
#[cfg(test)]
mod tests;
/// 类型化 Settings 视图的再导出，供外部消费方直接使用。
pub use model::Settings;
/// SettingsStore 的再导出。
pub use runtime::SettingsStore;

/// Shared store for the server's data directory. The singleton registry
/// keeps the persist lock and the one-shot migration flags process-wide,
/// mirroring the JS module-level runtime state.
/// 中文说明：同一 data 目录复用同一个 store 实例，从而共享持久化锁与迁移标记。
pub fn store(ctx: &RouterContext) -> Arc<SettingsStore> {
    store_for_path(&ctx.config.data_dir.join("settings.json"))
}

/// Shared store for an explicit settings.json path (used by tests and, later,
/// modules that operate on auxiliary data directories).
/// 中文说明：以 settings.json 路径为键做进程级单例；Weak 值允许无人引用时释放。
pub fn store_for_path(settings_path: &Path) -> Arc<SettingsStore> {
    // 进程级单例注册表：路径 → Weak<SettingsStore>，锁中毒时恢复而非 panic。
    static REGISTRY: LazyLock<Mutex<HashMap<PathBuf, Weak<SettingsStore>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut registry = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = registry.get(settings_path).and_then(Weak::upgrade) {
        return existing;
    }
    let store = Arc::new(SettingsStore::new(settings_path.to_path_buf()));
    registry.insert(settings_path.to_path_buf(), Arc::downgrade(&store));
    store
}

/// 构建 settings 路由的入口（供 server 路由装配调用）。
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(store(&ctx))
}
