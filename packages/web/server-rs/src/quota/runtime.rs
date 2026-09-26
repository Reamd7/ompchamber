//! Provider runtime: shared coalescing slots and the dispatcher ported from
//! `providers/index.js` (`pendingFetches`, `listConfiguredQuotaProviders`,
//! `fetchQuotaForProvider`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};

use crate::quota::deps::QuotaDeps;
use crate::quota::providers::claude::ClaudeCache;
use crate::quota::providers::xai::XaiRefreshState;
use crate::quota::utils::build_result;
use serde_json::Value;

/// A coalesced in-flight operation (the JS `pendingFetch` promises): every
/// caller awaits a clone of the same future, and the slot is cleared as part
/// of resolving it — so a sequential follow-up call starts a fresh operation
/// exactly like the JS `.finally` cleanup, while concurrent callers still
/// share one result. A spawned driver keeps the future progressing even when
/// every caller is dropped mid-flight (JS promises always run to completion).
pub struct SharedSlot<T> {
    slot: Mutex<Option<(u64, Shared<BoxFuture<'static, T>>)>>,
    next_generation: AtomicU64,
}

impl<T> SharedSlot<T>
where
    T: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            slot: Mutex::new(None),
            next_generation: AtomicU64::new(0),
        }
    }

    pub fn clear(&self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn clear_if_current(&self, generation: u64) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
        {
            *slot = None;
        }
    }

    /// Return the in-flight shared future, inserting a new one when the slot
    /// is empty. Must run inside a tokio runtime (the driver task); all call
    /// sites are async handlers or provider futures.
    pub fn subscribe<F>(self: &Arc<Self>, make: F) -> Shared<BoxFuture<'static, T>>
    where
        F: FnOnce() -> BoxFuture<'static, T>,
    {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, existing)) = slot.as_ref() {
            return existing.clone();
        }
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let inner = make().shared();
        let cleanup = self.clone();
        let wrapped = async move {
            let value = inner.await;
            cleanup.clear_if_current(generation);
            value
        }
        .boxed()
        .shared();

        *slot = Some((generation, wrapped.clone()));
        drop(slot);

        // Driver: guarantees progress independent of caller polls.
        let driver = wrapped.clone();
        tokio::spawn(async move {
            let _ = driver.await;
        });
        wrapped
    }
}
impl<T> Default for SharedSlot<T>
where
    T: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

/// The quota runtime held by the router and shared with every provider.
pub struct QuotaRuntime {
    pub deps: QuotaDeps,
    /// `pendingFetches` — one coalescing slot per provider ID.
    pending: Mutex<HashMap<String, Arc<SharedSlot<Value>>>>,
    pub claude_cache: ClaudeCache,
    pub xai_refresh: XaiRefreshState,
}

impl QuotaRuntime {
    pub fn new(deps: QuotaDeps) -> Self {
        Self {
            deps,
            pending: Mutex::new(HashMap::new()),
            claude_cache: ClaudeCache::new(),
            xai_refresh: XaiRefreshState::new(),
        }
    }

    pub fn shared(deps: QuotaDeps) -> Arc<Self> {
        Arc::new(Self::new(deps))
    }

    /// `listConfiguredQuotaProviders` — registry order; per-provider errors
    /// are swallowed (the JS try/catch around `isConfigured`).
    pub fn list_configured(&self) -> Vec<&'static str> {
        crate::quota::providers::registry()
            .iter()
            .filter(|entry| (entry.is_configured)(&self.deps))
            .map(|entry| entry.id)
            .collect()
    }

    fn pending_slot(self: &Arc<Self>, provider_id: &str) -> Arc<SharedSlot<Value>> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(SharedSlot::new()))
            .clone()
    }

    /// `fetchQuotaForProvider` — coalesced per provider ID. Unsupported IDs
    /// still produce the
    /// `{ ok: false, configured: false, error: 'Unsupported provider' }`
    /// envelope (coalesced like the JS map entry).
    pub fn fetch_quota_for_provider(
        self: &Arc<Self>,
        provider_id: &str,
    ) -> BoxFuture<'static, Value> {
        let slot = self.pending_slot(provider_id);
        let rt = self.clone();
        let id = provider_id.to_string();
        let shared = slot.subscribe(move || {
            let now = (rt.deps.now)();
            match crate::quota::providers::registry()
                .iter()
                .find(|entry| entry.id == id)
            {
                Some(entry) => {
                    let fetch = entry.fetch;
                    let rt = rt.clone();
                    fetch(rt)
                }
                None => futures::future::ready(build_result(
                    &id,
                    &id,
                    false,
                    false,
                    None,
                    Some("Unsupported provider"),
                    None,
                    now,
                ))
                .boxed(),
            }
        });
        Box::pin(shared)
    }

    /// Test seam mirroring `resetClaudeQuotaCache`.
    pub fn reset_claude_cache(&self) {
        self.claude_cache.reset();
    }
}
