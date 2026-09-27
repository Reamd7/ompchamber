//! Shared construction context handed to every ported module router.
//!
//! Contract for module ports (mirrors how `server/index.js` threads shared
//! runtimes into each `create*Runtime`): a module owns `src/<name>/mod.rs`
//! and exposes `pub fn router(ctx: RouterContext) -> axum::Router` returning a
//! fully-stateless router (apply any module-local state with
//! `Router::with_state` before returning). Modules never edit `lib.rs`,
//! `main.rs`, or `Cargo.toml`; new shared crates must be proposed in the
//! module's PORT-MANIFEST entry instead.
//!
//! 传递给每个移植模块路由器的共享构造上下文。模块移植契约要求各模块
//! 自持 `src/<name>/mod.rs` 并暴露接收 [`RouterContext`] 的 `router()`
//! 函数；新增共享依赖必须走既有字段，不允许模块改动装配文件。

use std::sync::Arc;

use crate::config::ServerConfig;
use crate::engine::EngineState;
use crate::hub::EventHub;

/// 交给每个模块 `router(ctx)` 的共享依赖集合；`Clone` 让各模块
/// 各自持有副本，互不借用。
#[derive(Clone)]
pub struct RouterContext {
    /// 服务端配置（端口、目录布局等），启动时加载后全局共享。
    pub config: Arc<ServerConfig>,
    /// 受管引擎与外部引擎的共享状态，模块借此发起引擎调用。
    pub engine: Arc<EngineState>,
    /// 跨模块事件中枢（SSE 等实时通道的事件广播）。
    pub hub: Arc<EventHub>,
}
