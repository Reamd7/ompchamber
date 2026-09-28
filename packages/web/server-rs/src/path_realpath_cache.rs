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
//!
//! 中文说明：本模块是异步 realpath 解析器的 TTL + 插入序（LRU）缓存，带
//! in-flight 去重：对同一路径的并发解析通过 Notify 槽位共享一次调用（队长
//! 存储结果并唤醒跟随者；跟随者克隆结果，不递归重解析）。失败结果按更短的
//! failure TTL 记忆，避免热路径上的缺失文件反复敲打文件系统；
//! `fallback_on_error` 开启时把错误降级为返回输入路径（对应 JS 的同名开关）。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 成功结果的默认 TTL（10 分钟）。
const DEFAULT_SUCCESS_TTL_MS: u64 = 600_000;
/// 失败结果的默认 TTL（1 分钟，比成功短以更快重试）。
const DEFAULT_FAILURE_TTL_MS: u64 = 60_000;
/// 缓存条目数上限（LRU 驱逐最旧条目）。
const DEFAULT_MAX_ENTRIES: usize = 256;

/// 实际执行 realpath 的解析函数类型：同步捕获、返回异步 Future。
type ResolveFn = Arc<
    dyn Fn(&str) -> Pin<Box<dyn Future<Output = std::io::Result<String>> + Send>> + Send + Sync,
>;

/// Leader/follower slot for one in-flight resolve. The outcome is stored as
/// message strings so every follower can clone it.
/// 中文说明：结果以字符串消息存储（成功值或错误文本），使每个跟随者都能克隆。
struct InFlight {
    /// 队长完成时唤醒所有跟随者的通知原语。
    notify: tokio::sync::Notify,
    /// 队长写入的结果槽（None = 尚未完成）。
    result: Mutex<Option<Result<String, String>>>,
}

/// 缓存条目：解析进行中（in-flight 槽位）或已落定（成功/失败 + 过期时间）。
#[derive(Clone)]
enum Entry {
    /// 同一路径的解析正在进行，等待者通过 InFlight 槽位取结果。
    InFlight(Arc<InFlight>),
    /// 已完成解析的结果；value 与 error 互斥。
    Settled {
        /// 成功解析出的真实路径（失败记忆时为原始输入路径）。
        value: Option<String>,
        /// 失败时的错误文本（成功时为 None）。
        error: Option<String>,
        /// 过期时间戳（毫秒），按成功/失败各自的 TTL 计算。
        expires_at: u64,
    },
}

/// 互斥锁保护的核心存储：键 → 条目映射 + 插入序（LRU 依据）。
struct Inner {
    /// 路径 → 缓存条目的映射。
    map: HashMap<String, Entry>,
    /// 按插入时间排列的键序列，队首即最旧（LRU 驱逐对象）。
    order: Vec<String>,
}

/// 带 TTL/LRU/in-flight 去重的 realpath 缓存主结构。
pub struct RealpathCache {
    /// 实际的解析函数。
    resolve: ResolveFn,
    /// 成功结果的缓存时长（毫秒）。
    success_ttl_ms: u64,
    /// 失败结果的缓存时长（毫秒）。
    failure_ttl_ms: u64,
    /// 缓存最大条目数；0 表示不缓存。
    max_entries: usize,
    /// 出错时是否降级返回输入路径而非上抛错误。
    fallback_on_error: bool,
    /// 核心存储（map + LRU 序列）。
    inner: Mutex<Inner>,
    /// 当前时间函数（毫秒），测试可注入固定时钟。
    now: Box<dyn Fn() -> u64 + Send + Sync>,
}

/// 默认时钟：当前 Unix epoch 毫秒数（失败时返回 0）。
fn default_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// 缓存的构造、配置与解析入口。
impl RealpathCache {
    /// 以默认 TTL/容量与注入的解析函数创建缓存。
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

    /// 覆盖成功/失败 TTL 的链式构造器。
    pub fn with_ttls(mut self, success: Duration, failure: Duration) -> Self {
        self.success_ttl_ms = success.as_millis() as u64;
        self.failure_ttl_ms = failure.as_millis() as u64;
        self
    }

    /// 开启"出错降级返回输入路径"模式的链式构造器。
    pub fn with_fallback_on_error(mut self, fallback: bool) -> Self {
        self.fallback_on_error = fallback;
        self
    }

    /// 清空全部缓存条目与 LRU 序列。
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.map.clear();
        inner.order.clear();
    }

    /// 当前缓存条目数（含 in-flight）。
    pub fn size(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .len()
    }

    /// 把键移到 LRU 序列尾部（标记为最新使用）。
    fn touch(&self, key: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(position) = inner.order.iter().position(|k| k == key) {
            let k = inner.order.remove(position);
            inner.order.push(k);
        }
    }

    /// 超出容量上限时从队首驱逐最旧条目，直到回到限额内。
    fn prune(&self, inner: &mut Inner) {
        while inner.map.len() > self.max_entries {
            let Some(oldest) = inner.order.first().cloned() else {
                return;
            };
            inner.order.remove(0);
            inner.map.remove(&oldest);
        }
    }

    /// 写入一条已落定的结果：先移除旧条目，再按给定 TTL 插入并触发 LRU 驱逐；
    /// `max_entries == 0` 时为空操作。
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

    /// 根据降级开关把解析错误转换为"返回输入路径"或重建的 io::Error。
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
    /// 中文说明：空输入原样返回；命中未过期缓存直接取值；同键 in-flight 时
    /// 作为跟随者等待队长结果；否则自己成为队长执行解析、写入缓存并唤醒跟随者。
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

/// realpath 缓存的单元测试。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 构造同步计数解析器：正常路径返回 `<path>/real`，含 "missing" 的路径报 NotFound；
    /// 返回值附带调用计数器以断言去重效果。
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

    /// 验证：成功结果在 TTL 内命中缓存，第二次解析不触发新的解析调用。
    #[tokio::test]
    async fn caches_successes() {
        let (resolve, calls) = sync_resolver();
        let cache = RealpathCache::new(resolve);
        assert_eq!(cache.resolve("/a").await.unwrap(), "/a/real");
        assert_eq!(cache.resolve("/a").await.unwrap(), "/a/real");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.size(), 1);
    }

    /// 验证：同一路径的并发解析共享一次底层调用（in-flight 去重）。
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

    /// 验证：失败在 failure TTL 内被记忆（不再调用解析器）；降级模式下失败
    /// 返回输入路径且同样只调用一次。
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

    /// 验证：队长出错时跟随者得到与队长一致的错误结果（无丢失、无重解析死锁）。
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

    /// 验证：超过容量上限后驱逐最旧条目，条目数不超过 max_entries。
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
