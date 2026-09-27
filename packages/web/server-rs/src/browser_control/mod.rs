//! Port of `server/lib/browser-control/` — request/response broker between
//! the agent tool and the in-app browser.
//!
//! - [`broker`] owns request lifetime: it publishes one action through the
//!   injected [`broker::EmitRequest`], holds the pending request, and settles
//!   it on a client result, a timeout, or a cancel signal. It knows nothing
//!   about transports.
//! - [`routes`] is the callback pair: `POST /api/browser-control/claim`
//!   (exactly one client may act) and `POST /api/browser-control/result`
//!   (the outcome). They validate the envelope and hand it to the broker.
//! - The only caller is `openchamber-control/service.js` (maps the
//!   `browser.*` actions of the `openchamber_web` tool onto
//!   [`broker::BrowserControlBroker::request`]); the client half lives in
//!   `packages/ui/src/lib/browser/controlClient.ts`.
//!
//! Invariants (see the JS module's DOCUMENTATION.md):
//! - Capability belongs to the connection, not to configuration: a client
//!   declares it by opening `/api/ompchamber/events?browser=1`.
//! - Exactly one client performs a request: first claim wins, a claim for a
//!   settled request is refused, and a late result is `matched: false`, not
//!   an error.
//! - Nobody listening is answered immediately with a 503 describing the
//!   environment, never by blocking for the full timeout.
//! - A client that accepted a request and then disappeared still times out.
//!
//! # Composition seam (honest gap)
//!
//! index.js:1323 constructs the broker with an `emitRequest` that writes
//! `ompchamber:browser-control-request` frames to the `/api/ompchamber/events`
//! SSE clients (`uiOMPChamberEventClients`), skipping clients without the
//! `?browser=1` capability for every action except `browser.open`, and
//! returning how many clients were reached. In this port that client set
//! lives in [`crate::scheduled_tasks::routes::SseClients`], which exposes
//! neither the per-connection capability flag nor a delivery count, so the
//! wiring cannot be reproduced on top of it from here (the module is frozen;
//! seam recorded for the composition owner). Until composition supplies a
//! real [`broker::EmitRequest`] through [`router_shared`], [`router`]'s
//! default broker reaches zero clients and every request fails fast with the
//! JS 503 message instead of blocking — the honest "not here" answer, never
//! a fake timeout.
//!
//! 中文说明：本目录是 `server/lib/browser-control/` 的移植。`broker` 负责请求生命周期
//!（发布、在途持有、按结果/超时/取消收结），`routes` 提供 claim/result 回调对；唯一
//! 调用方是 `openchamber-control/service.js`（把 `openchamber_web` 工具的 `browser.*`
//! action 映射到 broker），客户端半边在 `packages/ui/src/lib/browser/controlClient.ts`。
//! 组合缺口：`SseClients` 尚未暴露按连接的能力标志与送达计数，默认 [`router`] 的
//! broker 到达零客户端，所有请求按 JS 语义快速返回 503，绝不伪造超时。

/// 请求/响应 broker：发布动作、保存待处理、按超时与取消收结；不感知传输层。
pub mod broker;
/// 回调路由对：claim（唯一执行权）与 result（结果回传），校验信封后交给 broker。
pub mod routes;

/// 重导出 broker 的公开类型，供服务层与组合方使用。
pub use broker::{
    BrowserControlBroker, BrowserControlError, CancelSignal, ClientResult, CreateId,
    DEFAULT_TIMEOUT_MS, EmitRequest, MAX_TIMEOUT_MS, NO_CLIENTS_MESSAGE, OutgoingRequest,
    RequestOptions,
};
/// 重导出组合入口：由调用方持有并传入共享 broker。
pub use routes::router_shared;

use crate::context::RouterContext;

/// Default emit for the standalone module router: no transport is wired at
/// module scope (see the composition-seam note above), so it reaches zero
/// clients and the broker answers with the JS 503 "no client connected"
/// error rather than pretending someone is listening.
/// 中文：模块级默认无传输可达，返回 0 触发快速失败路径。
fn unwired_emit(_: &OutgoingRequest) -> usize {
    0
}

/// 默认模块路由：broker 使用 uuid id 工厂与未接线的 emit（见上方组合缺口说明），
/// 行为等同于 JS 侧“无客户端连接”的 503 快速失败。
pub fn router(ctx: RouterContext) -> axum::Router {
    // index.js supplies `createId: () => browser-<uuid>`; the emit seam is
    // documented above.
    routes::router_shared(
        ctx,
        BrowserControlBroker::with_id_factory(unwired_emit, broker::uuid_request_id),
    )
}
