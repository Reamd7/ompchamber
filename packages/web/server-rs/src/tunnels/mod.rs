//! Port of `server/lib/tunnels/` (registry, routes, executable-search,
//! install-help, managed-config, providers, types, index) plus
//! `server/lib/cloudflare-tunnel.js`, `server/lib/ngrok-tunnel.js`, and
//! `server/lib/dev-tunnel/` (host runtime + local client).
//!
//! JS file → Rust file:
//! - `tunnels/types.js` → `types.rs`
//! - `tunnels/install-help.js` → `install_help.rs`
//! - `tunnels/executable-search.js` → `executable_search.rs`
//! - `tunnels/managed-config.js` → `managed_config.rs`
//! - `tunnels/registry.js` → `registry.rs`
//! - `cloudflare-tunnel.js` + `tunnels/providers/cloudflare.js` → `cloudflare.rs`
//! - `ngrok-tunnel.js` + `tunnels/providers/ngrok.js` → `ngrok.rs`
//! - `tunnels/index.js` → `service.rs`
//! - `tunnels/routes.js` → `routes.rs`
//! - `opencode/tunnel-auth.js` → consumed from `crate::client_auth::tunnel_auth`
//! - `dev-tunnel/{runtime,client}.js` → `dev_tunnel.rs`
//! - child-process seam for the providers → `runner.rs`
//! （中文说明）tunnel 子系统入口：组装 provider 注册表与 HTTP/WS
//! 路由，覆盖 cloudflared/ngrok 依赖启动、managed 配置与浏览器直连的
//! dev-tunnel 两条通路；对外导出路由构造函数与 dev-tunnel 客户端类型。

/// cloudflared provider：quick/managed 模式的进程启动、就绪轮询与诊断。
mod cloudflare;
/// dev-tunnel：宿主端 WS 中继与本地客户端（浏览器直连本地端口）。
mod dev_tunnel;
/// 跨平台可执行文件查找（PATH 解析、WindowsApps 别名、环境构造）。
mod executable_search;
/// 各 provider/平台的安装命令与缺依赖提示文案。
mod install_help;
/// managed 模式的 cloudflared 配置文件生成与 managed-remote profile 读写。
mod managed_config;
/// ngrok provider：quick tunnel 启动、agent API URL 轮询与诊断。
mod ngrok;
/// provider 注册表：注册校验、大小写不敏感查找与能力列表。
mod registry;
/// HTTP/WS 路由层：tunnel REST 接口与 dev-tunnel WebSocket 升级处理。
mod routes;
/// 对外导出的公网 URL 查询助手（定义在 routes，供其它模块读取当前隧道地址）。
pub use routes::tunnel_public_url;
/// 子进程执行接缝（probe/spawn/输出泵送）与测试替身。
mod runner;
/// tunnel 服务层：start/stop 状态机，对 routes 的封装（JS index.js）。
mod service;
/// 共享类型与常量：平台枚举、模式描述、请求/错误形态。
mod types;

/// 对外导出的 dev-tunnel 客户端与状态类型（desktop/UI 侧使用）。
pub use dev_tunnel::{DevTunnelClient, DevTunnelState, ListedTunnel};

use crate::context::RouterContext;

/// Tunnel module router (JS: tunnel-wiring-runtime `initialize` +
/// createDevTunnelRuntime's upgrade path).
///
/// 以路由上下文构建 tunnel 子系统的 axum Router：REST 端点 +
/// dev-tunnel WebSocket 升级路径。
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router_with(routes::module_state(ctx))
}
