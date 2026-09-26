//! Port of `server/lib/path-realpath-cache.js`.
//!
//! TTL + insertion-order (LRU) cache around an async realpath resolver, with
//! in-flight dedupe: concurrent resolves of the same path share one call via a
//! Notify slot (the leader stores the outcome and wakes followers; followers
//! clone it — no re-resolution recursion, which previously livelocked the
//! single-threaded test runtime). Failures are remembered for the shorter
//! failure TTL so a hot missing path does not hammer the filesystem;
//! `fallback_on_error` degrades to the input path instead of surfacing the
//! error (mirroring the JS flag).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_SUCCESS_TTL_MS: u64 = 600_000;
const DEFAULT_FAILURE_TTL_MS: u64 = 60_000;
const DEFAULT_MAX_ENTRIES: usize = 256;

type ResolveFn = Arc<
    dyn Fn(&str) -> Pin<Box<dyn Future<Output = std::io::Result<String>> + Send>> + Send + Sync,
>;

/// Leader/follower slot for one in-flight resolve. The outcome is stored as
/// message strings so every follower can clone it.
struct InFlight {
    notify: tokio::sync::Notify,
    result: Mutex<Option<Result<String, String>>>,
}

#[derive(Clone)]
enum Entry {
    InFlight(Arc<InFlight>),
    Settled {
        value: Option<String>,
        error: Option<String>,
        expires_at: u64,
    },
}

struct Inner {
    map: HashMap<String, Entry>,
    order: Vec<String>,
}

pub struct RealpathCache {
    resolve: ResolveFn,
    success_ttl_ms: u64,
    failure_ttl_ms: u64,
    max_entries: usize,
    fallback_on_error: bool,
    inner: Mutex<Inner>,
    now: Box<dyn Fn() -> u64 + Send + Sync>,
}

fn default_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl RealpathCache {
    pub fn new(resolve: ResolveFn) -> Self {
        Self {
            resolve,
            success_ttl_ms: DEFAULT_SUCCESS_TTL_MS,
            failure_ttl_ms: DEFAULT_FAILURE_TTL_MS,
            max_entries: DEFAULT_MAX_ENTRIES,
            fallback_on_error: false,
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                order: Vec::new(),
            }),
            now: Box::new(default_now_ms),
        }
    }

    pub fn with_ttls(mut self, success: Duration, failure: Duration) -> Self {
        self.success_ttl_ms = success.as_millis() as u64;
        self.failure_ttl_ms = failure.as_millis() as u64;
        self
    }

    pub fn with_fallback_on_error(mut self, fallback: bool) -> Self {
        self.fallback_on_error = fallback;
        self
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.map.clear();
        inner.order.clear();
    }

    pub fn size(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }

    fn touch(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(position) = inner.order.iter().position(|k| k == key) {
            let k = inner.order.remove(position);
            inner.order.push(k);
        }
    }

    fn prune(&self, inner: &mut Inner) {
        while inner.map.len() > self.max_entries {
            let Some(oldest) = inner.order.first().cloned() else {
                return;
            };
            inner.order.remove(0);
            inner.map.remove(&oldest);
        }
    }

    fn remember(
        &self,
        inner: &mut Inner,
        key: &str,
        value: Option<String>,
        error: Option<String>,
        ttl_ms: u64,
    ) {
        if self.max_entries == 0 {
            return;
        }
        inner.map.remove(key);
        if let Some(position) = inner.order.iter().position(|k| k == key) {
            inner.order.remove(position);
        }
        let expires_at = (self.now)() + ttl_ms;
        inner.map.insert(
            key.to_string(),
            Entry::Settled {
                value,
                error,
                expires_at,
            },
        );
        inner.order.push(key.to_string());
        self.prune(inner);
    }

    fn settle_from(
        &self,
        error: &std::io::Error,
        fallback_input: &str,
    ) -> Result<String, std::io::Error> {
        if self.fallback_on_error {
            Ok(fallback_input.to_string())
        } else {
            Err(std::io::Error::new(error.kind(), error.to_string()))
        }
    }

    /// Resolve a path through the cache. Empty inputs pass through unchanged
    /// (JS returns the value verbatim).
    pub async fn resolve(&self, value: &str) -> Result<String, std::io::Error> {
        if value.is_empty() {
            return Ok(value.to_string());
        }

        // Fast paths + leader registration under the lock.
        let leader_slot = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let now = (self.now)();
            match inner.map.get(value) {
                Some(Entry::InFlight(_slot)) => None,
                Some(Entry::Settled {
                    value: cached,
                    error,
                    expires_at,
                }) if *expires_at > now => {
                    let cached = cached.clone();
                    let error = error.clone();
                    // Touch insertion order while still holding the guard —
                    // self.touch() would re-lock this non-reentrant mutex.
                    if let Some(position) = inner.order.iter().position(|k| k == value) {
                        let k = inner.order.remove(position);
                        inner.order.push(k);
                    }
                    drop(inner);
                    if let Some(error) = error {
                        if self.fallback_on_error {
                            return Ok(value.to_string());
                        }
                        return Err(std::io::Error::other(error));
                    }
                    return Ok(cached.unwrap_or_else(|| value.to_string()));
                }
                _ => {
                    inner.map.remove(value);
                    if let Some(position) = inner.order.iter().position(|k| k == value) {
                        inner.order.remove(position);
                    }
                    let slot = Arc::new(InFlight {
                        notify: tokio::sync::Notify::new(),
                        result: Mutex::new(None),
                    });
                    inner
                        .map
                        .insert(value.to_string(), Entry::InFlight(Arc::clone(&slot)));
                    self.prune(&mut inner);
                    drop(inner);
                    Some(slot)
                }
            }
        };

        // Follower: wait for the leader's outcome and clone it. The
        // notified() future is created BEFORE checking the slot so a
        // notify_waiters() racing the check cannot be lost.
        let Some(slot) = leader_slot else {
            let slot = {
                let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                match inner.map.get(value) {
                    Some(Entry::InFlight(slot)) => Arc::clone(slot),
                    // The leader settled between the two locks: recurse once
                    // through the settled fast path.
                    _ => return Box::pin(self.resolve(value)).await,
                }
            };
            loop {
                let notified = slot.notify.notified();
                if let Some(outcome) = slot
                    .result
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                {
                    return outcome.map_err(std::io::Error::other);
                }
                notified.await;
            }
        };

        // Leader: run the resolver outside the lock, remember, then publish.
        let resolve = Arc::clone(&self.resolve);
        let input = value.to_string();
        let outcome = match resolve(&input).await {
            Ok(resolved) => {
                let next = if resolved.is_empty() {
                    input.clone()
                } else {
                    resolved
                };
                Ok(next.clone())
            }
            Err(error) => self.settle_from(&error, &input),
        };

        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match &outcome {
                Ok(resolved) => {
                    let resolved = resolved.clone();
                    self.remember(&mut inner, value, Some(resolved), None, self.success_ttl_ms);
                }
                Err(error) => {
                    let message = error.to_string();
                    self.remember(
                        &mut inner,
                        value,
                        Some(input.clone()),
                        Some(message),
                        self.failure_ttl_ms,
                    );
                }
            }
        }

        let shared = match &outcome {
            Ok(value) => Ok(value.clone()),
            Err(error) => Err(error.to_string()),
        };
        *slot.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(shared);
        slot.notify.notify_waiters();
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sync_resolver() -> (ResolveFn, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_fn = Arc::clone(&calls);
        let resolve: ResolveFn = Arc::new(move |path: &str| {
            let calls = Arc::clone(&calls_for_fn);
            let path = path.to_string();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                if path.contains("missing") {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "no such file",
                    ))
                } else {
                    Ok(format!("{path}/real"))
                }
            })
        });
        (resolve, calls)
    }

    #[tokio::test]
    async fn caches_successes() {
        let (resolve, calls) = sync_resolver();
        let cache = RealpathCache::new(resolve);
        assert_eq!(cache.resolve("/a").await.unwrap(), "/a/real");
        assert_eq!(cache.resolve("/a").await.unwrap(), "/a/real");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.size(), 1);
    }

    #[tokio::test]
    async fn concurrent_resolves_share_one_call() {
        let (resolve, calls) = sync_resolver();
        let cache = Arc::new(RealpathCache::new(resolve));
        let results = futures::future::join_all((0..8).map(|_| {
            let cache = Arc::clone(&cache);
            async move { cache.resolve("/shared").await }
        }))
        .await;
        for result in results {
            assert_eq!(result.unwrap(), "/shared/real");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failures_are_remembered_within_failure_ttl() {
        let (resolve, calls) = sync_resolver();
        let strict = RealpathCache::new(Arc::clone(&resolve));
        assert!(strict.resolve("/missing").await.is_err());
        // Within the failure TTL the cached error surfaces without a second call.
        assert!(strict.resolve("/missing").await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let lenient = RealpathCache::new(resolve).with_fallback_on_error(true);
        assert_eq!(lenient.resolve("/missing2").await.unwrap(), "/missing2");
        assert_eq!(lenient.resolve("/missing2").await.unwrap(), "/missing2");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn leader_yields_identical_outcome_to_followers_on_error() {
        let (resolve, _) = sync_resolver();
        let cache = Arc::new(RealpathCache::new(resolve));
        let results = futures::future::join_all((0..4).map(|_| {
            let cache = Arc::clone(&cache);
            async move { cache.resolve("/missing").await }
        }))
        .await;
        assert!(results.iter().all(|r| r.is_err()));
    }

    #[tokio::test]
    async fn eviction_keeps_newest_entries() {
        let (resolve, _) = sync_resolver();
        let cache = RealpathCache::new(resolve);
        for index in 0..DEFAULT_MAX_ENTRIES + 8 {
            cache.resolve(&format!("/p{index}")).await.unwrap();
        }
        assert!(cache.size() <= DEFAULT_MAX_ENTRIES);
    }
}
