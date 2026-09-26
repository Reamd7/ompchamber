//! Port of `server/lib/quota/` — quota usage tracking for AI providers.
//!
//! Layout mirrors the JS module:
//! - [`utils`]: `quota/utils/` shared coercion, formatters, transformers.
//! - [`credentials`]: `quota/credentials/` managed-credential store.
//! - [`providers`]: `quota/providers/*` — every provider fetch, auth
//!   resolution, and transform, plus the dispatcher registry.
//! - [`runtime`]: the coalescing `pendingFetches` map and Claude/xAI caches.
//! - [`routes`]: `registerQuotaRoutes` on axum.
//!
//! Known port gaps (documented for PORT-MANIFEST):
//! - `Date.parse` is an ISO-8601 subset; offset-less timestamps read as UTC
//!   (JS would apply the server's local zone).
//! - `resetAtFormatted`/`resetAfterFormatted` use deterministic en-US/UTC
//!   labels; JS renders them in the server locale.
//! - Body-parse failures on the credential PUT answer with axum's text
//!   rejection (JS Express answers its HTML error page); both are 400s.
//! - Network error strings carry the reqwest message where JS carried the
//!   Node fetch message (`fetch failed`); mapped messages ("Request timed
//!   out", "Invalid response from provider", provider status strings) match.

pub mod credentials;
pub mod deps;
pub mod http;
pub mod providers;
pub mod routes;
pub mod runtime;
pub mod utils;

use crate::context::RouterContext;

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod tests_support;

/// `registerQuotaRoutes(app, { getQuotaProviders })`.
pub fn router(ctx: RouterContext) -> axum::Router {
    routes::router(ctx)
}
