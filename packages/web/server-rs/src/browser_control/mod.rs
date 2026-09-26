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

pub mod broker;
pub mod routes;

pub use broker::{
    BrowserControlBroker, BrowserControlError, CancelSignal, ClientResult, CreateId,
    DEFAULT_TIMEOUT_MS, EmitRequest, MAX_TIMEOUT_MS, NO_CLIENTS_MESSAGE, OutgoingRequest,
    RequestOptions,
};
pub use routes::router_shared;

use crate::context::RouterContext;

/// Default emit for the standalone module router: no transport is wired at
/// module scope (see the composition-seam note above), so it reaches zero
/// clients and the broker answers with the JS 503 "no client connected"
/// error rather than pretending someone is listening.
fn unwired_emit(_: &OutgoingRequest) -> usize {
    0
}

pub fn router(ctx: RouterContext) -> axum::Router {
    // index.js supplies `createId: () => browser-<uuid>`; the emit seam is
    // documented above.
    routes::router_shared(
        ctx,
        BrowserControlBroker::with_id_factory(unwired_emit, broker::uuid_request_id),
    )
}
