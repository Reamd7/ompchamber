//! Provider runtime: shared coalescing slots and the dispatcher ported from
//! `providers/index.js` (`pendingFetches`, `listConfiguredQuotaProviders`,
//! `fetchQuotaForProvider`).
//!
//! 中文说明：provider 运行时：共享的请求合并（coalescing）槽位与分发器，
//! 移植自 `providers/index.js` 的 `pendingFetches`、
//! `listConfiguredQuotaProviders` 与 `fetchQuotaForProvider`。

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
///
/// 中文说明：合并中的在途操作（对应 JS 的 `pendingFetch` promise）：每个
/// 调用方 await 同一 future 的克隆；future 完成时顺带清空槽位——后续的
/// 顺序调用会开启全新操作（等价 JS 的 `.finally` 清理），并发调用方仍共享
/// 同一结果。内部 spawn 一个 driver 任务，即使所有调用方中途 drop，
/// future 也继续推进（JS promise 总是跑完）。
pub struct SharedSlot<T> {
    /// 在途操作：代数编号 + 共享 future；`None` 表示当前无在途操作。
    slot: Mutex<Option<(u64, Shared<BoxFuture<'static, T>>)>>,
    /// 单调递增的代数计数器，用于防止旧操作的清理误删新操作（ABA）。
    next_generation: AtomicU64,
}

/// [`SharedSlot`] 的合并订阅与清理实现。
impl<T> SharedSlot<T>
where
    T: Clone + Send + Sync + 'static,
{
    /// 创建空槽位（无在途操作，代数从 0 起）。
    pub fn new() -> Self {
        Self {
            slot: Mutex::new(None),
            next_generation: AtomicU64::new(0),
        }
    }

    /// 无条件清空槽位（供测试或强制失效使用）。
    pub fn clear(&self) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 仅当槽内仍是给定代数时才清空：避免被新一代操作替换后，旧 future
    /// 的完成回调误删新 future。
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
    ///
    /// 中文说明：返回在途的共享 future；槽位为空时用 `make` 新建一个并
    /// 占位。必须在 tokio runtime 内调用（内部 spawn driver）；所有调用点
    /// 都是 async handler 或 provider future。
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
/// [`SharedSlot`] 的 `Default` 委托给 [`SharedSlot::new`]。
impl<T> Default for SharedSlot<T>
where
    T: Clone + Send + Sync + 'static,
{
    /// 等价于 [`SharedSlot::new`]。
    fn default() -> Self {
        Self::new()
    }
}

/// The quota runtime held by the router and shared with every provider.
///
/// 中文说明：路由持有、与所有 provider 共享的 quota 运行时：依赖注入、
/// 按 provider 合并的 pending 槽位，以及 Claude/xAI 各自的跨请求状态。
pub struct QuotaRuntime {
    /// 所有 provider 共用的依赖注入集合（HTTP、时钟、env、凭据读取等）。
    pub deps: QuotaDeps,
    /// `pendingFetches` — one coalescing slot per provider ID.
    ///
    /// 中文说明：对应 JS 的 `pendingFetches`——每个 provider ID 一个合并
    /// 槽位（map 由 Mutex 保护）。
    pending: Mutex<HashMap<String, Arc<SharedSlot<Value>>>>,
    /// Claude provider 的配额缓存（跨请求复用，带重置入口）。
    pub claude_cache: ClaudeCache,
    /// xAI provider 的用量刷新状态（跨请求共享）。
    pub xai_refresh: XaiRefreshState,
}

/// [`QuotaRuntime`] 的构造、provider 枚举与合并式配额分发。
impl QuotaRuntime {
    /// 以指定依赖创建 runtime（空 pending 表、全新 Claude 缓存与 xAI 状态）。
    pub fn new(deps: QuotaDeps) -> Self {
        Self {
            deps,
            pending: Mutex::new(HashMap::new()),
            claude_cache: ClaudeCache::new(),
            xai_refresh: XaiRefreshState::new(),
        }
    }

    /// 创建 `Arc` 包装的 runtime（路由与测试的常规入口）。
    pub fn shared(deps: QuotaDeps) -> Arc<Self> {
        Arc::new(Self::new(deps))
    }

    /// `listConfiguredQuotaProviders` — registry order; per-provider errors
    /// are swallowed (the JS try/catch around `isConfigured`).
    ///
    /// 中文说明：列出已配置的 provider（保持注册表顺序，对应 JS 的
    /// `listConfiguredQuotaProviders`）；单个 provider 的 `is_configured`
    /// 出错会被吞掉（对应 JS 包在 `isConfigured` 外的 try/catch）。
    pub fn list_configured(&self) -> Vec<&'static str> {
        crate::quota::providers::registry()
            .iter()
            .filter(|entry| (entry.is_configured)(&self.deps))
            .map(|entry| entry.id)
            .collect()
    }

    /// 取（或惰性创建）指定 provider 的合并槽位。
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
    ///
    /// 中文说明：按 provider ID 合并并发请求（对应 JS 的
    /// `fetchQuotaForProvider`）：同一 ID 的在途请求共享一个 future。
    /// 未注册的 ID 也会被合并，并产出
    /// `{ ok: false, configured: false, error: 'Unsupported provider' }`
    /// 错误信封（与 JS 往 map 里塞条目的行为一致）。
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
    ///
    /// 中文说明：测试 seam，对应 JS 的 `resetClaudeQuotaCache`：清空
    /// Claude 配额缓存以便重新拉取。
    pub fn reset_claude_cache(&self) {
        self.claude_cache.reset();
    }
}
