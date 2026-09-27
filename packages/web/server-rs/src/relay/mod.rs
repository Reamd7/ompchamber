//! Port of `server/lib/relay/` — the private relay host side.
//!
//! The private relay lets an OpenChamber client (mobile app, browser, or
//! another desktop) reach a user's OpenChamber instance through
//! OpenChamber-hosted infrastructure when the instance is not directly
//! reachable. The instance dials **outbound** to the relay; nothing needs to
//! be exposed inbound. Traffic is end-to-end encrypted between the two
//! endpoints — the relay infrastructure forwards opaque ciphertext and cannot
//! read application traffic.
//!
//! JS file → Rust file:
//! - `relay/e2ee.js` → `e2ee.rs` (crypto + responder handshake, byte-compatible
//!   with `packages/ui/src/lib/relay/*`; pinned vectors in-module)
//! - `relay/tunnel-codec.js` → `tunnel_codec.rs`
//! - `relay/tunnel-host.js` → `tunnel_host.rs`
//! - `relay/host-client.js` → `host_client.rs`
//! - `relay/service.js` → `service.rs` (+ `routes.rs`)
//! - `relay/signing-key.js` → `signing_key.rs`
//! - `relay/identity.js` → `identity.rs`
//! - `relay/host-lock.js` → `host_lock.rs`
//!
//! Routes: `GET/POST /api/ompchamber/relay/{status,enable,disable}`.
//!
//! 中文概述：私有 relay host 侧的模块入口与路由装配。实例主动向外
//! 拨号接入 relay（无需任何入站暴露），流量端到端加密，relay 基础
//! 设施只转发密文。本模块对外提供进程级缓存的 server_id 与管理路由
//! router。

/// E2EE 加密原语与 responder 侧握手（与 UI 侧 TS 字节级兼容，含 pinned 向量）。
pub(crate) mod e2ee;
/// 面向 relay 的 host 客户端：控制/数据连接的建立、鉴权与重连。
pub(crate) mod host_client;
/// 单机 relay-host 协作式声明（relay-host.lock + pid 探活互斥）。
pub(crate) mod host_lock;
/// host 身份组装：签名密钥、E2EE 密钥与 relay 鉴权签名闭包。
pub(crate) mod identity;
/// relay 管理 HTTP 路由（status/enable/disable）。
pub(crate) mod routes;
/// relay 服务编排与状态机（JS service.js 的主体逻辑）。
pub(crate) mod service;
/// ECDSA P-256 签名密钥管理与 serverId 派生。
pub(crate) mod signing_key;
/// tunnel 帧编解码、分片重组与出站批量缓冲。
pub(crate) mod tunnel_codec;
/// tunnel host 多路复用器：HTTP/WS 流的双向转发。
pub(crate) mod tunnel_host;

/// relay 模块的集成测试（含跨实现字节兼容验证）。
#[cfg(test)]
mod tests;

use crate::context::RouterContext;

/// The relay identity's stable server id (get_or_create, same store the JS
/// relay service uses). Exposed for /health and /api/version parity.
/// Derived once per process — the JS caches this on the relay service
/// instance; deriving per request would read settings + run P-256 math on
/// every /health.
///
/// 中文补充：tokio OnceCell 提供进程级缓存，派生一次后不再重复读取
/// settings 或做 P-256 计算；失败结果（None）同样被缓存。
pub async fn server_id(ctx: &RouterContext) -> Option<String> {
    // 进程级缓存（函数体内的嵌套 item，按规范用普通注释）：首次调用
    // 组装身份，此后所有请求复用同一结果。
    static CACHED: tokio::sync::OnceCell<Option<String>> = tokio::sync::OnceCell::const_new();
    CACHED
        .get_or_init(|| async {
            let store = crate::settings::store(ctx);
            let runtime = identity::RelayIdentityRuntime::new(store, identity::system_clock());
            runtime
                .get_relay_identity()
                .await
                .ok()
                .map(|identity| identity.server_id.clone())
        })
        .await
        .clone()
}

/// 装配 relay 管理路由：构造 RelayService 并注入 routes 的 ModuleState。
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router_with(routes::ModuleState {
        service: service::service(&ctx),
    })
}
