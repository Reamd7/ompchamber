//! Port of `server/lib/skills-catalog/cache.js`: an in-memory TTL cache for
//! skill scan results with debounced disk persistence
//! (`skills-catalog-cache.json` in the data dir), per-key in-flight
//! deduplication, and a global concurrency limit of two concurrent loaders.
//! Only successful (`ok: true`) scans are cached.
//!
//! 中文说明：技能扫描结果的内存 TTL 缓存，附带防抖磁盘持久化（数据
//! 目录下的 `skills-catalog-cache.json`）、按 key 的在途去重和全局
//! “最多两个并发 loader”的信号量限流。只有成功（ok: true）的扫描才会
//! 被缓存。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::skills_catalog::disk_cache::{now_millis, read_disk_cache, write_disk_cache};
use crate::skills_catalog::scan::ScanResult;

/// 缓存条目的默认 TTL（3 小时）。
const DEFAULT_TTL_MS: u64 = 3 * 60 * 60 * 1000;
/// 磁盘缓存文件名（位于数据目录内）。
const DISK_CACHE_FILE: &str = "skills-catalog-cache.json";
/// 全局并发 loader 上限（2），防止同时打爆过多 git 扫描。
const MAX_CONCURRENT_SCANS: usize = 2;
/// 磁盘写入防抖延迟（1s），合并短时间内的多次更新。
const DISK_WRITE_DELAY_MS: u64 = 1_000;

/// 单个缓存条目：到期时间戳与扫描结果 JSON 值。
#[derive(Debug, Clone)]
struct CacheEntry {
    /// 条目到期时间（Unix 毫秒）。
    expires_at: u64,
    /// 缓存的 ScanResult 序列化值。
    value: Value,
}

/// 模块级共享状态：缓存表、在途表、磁盘加载闩锁与防抖写任务。
struct CacheState {
    /// key → 缓存条目。
    cache: HashMap<String, CacheEntry>,
    /// key → 在途共享 future（并发去重）。
    in_flight: HashMap<String, Shared<BoxFuture<'static, ScanResult>>>,
    /// 磁盘缓存是否已一次性导入。
    disk_loaded: bool,
    /// 挂起中的防抖磁盘写任务句柄。
    disk_write_timer: Option<tokio::task::JoinHandle<()>>,
}

/// 全局缓存状态互斥锁（惰性初始化）。
static STATE: LazyLock<Mutex<CacheState>> = LazyLock::new(|| {
    Mutex::new(CacheState {
        cache: HashMap::new(),
        in_flight: HashMap::new(),
        disk_loaded: false,
        disk_write_timer: None,
    })
});

/// 全局扫描并发限流信号量（2 个许可）。
static SCAN_LIMIT: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_SCANS));

/// 锁定状态；锁中毒时恢复内部数据继续用。
fn lock_state() -> MutexGuard<'static, CacheState> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `getCacheKey({ normalizedRepo, subpath, identityId })` →
/// `repo::subpath::identity` (each part trimmed, missing → empty).
/// 输出 `repo::subpath::identity` 形式的 key，各段 trim、缺失记空串，
/// 与 JS 侧逐字符一致。
pub fn get_cache_key(normalized_repo: &str, subpath: &str, identity_id: &str) -> String {
    format!(
        "{}::{}::{}",
        normalized_repo.trim(),
        subpath.trim(),
        identity_id.trim()
    )
}

/// `loadDiskEntries()`: one-shot import of unexpired disk entries.
/// 首次调用导入磁盘上未过期且结构合法的条目并闩锁，此后不再读盘。
fn load_disk_entries(state: &mut CacheState) {
    if state.disk_loaded {
        return;
    }
    state.disk_loaded = true;
    let Some(Value::Object(persisted)) = read_disk_cache(DISK_CACHE_FILE) else {
        return;
    };
    let now = now_millis();
    for (key, entry) in persisted {
        let Some(fields) = entry.as_object() else {
            continue;
        };
        let valid = fields
            .get("expiresAt")
            .and_then(Value::as_u64)
            .is_some_and(|expires_at| expires_at > now)
            && fields.get("value").is_some_and(Value::is_object);
        if valid {
            let expires_at = fields
                .get("expiresAt")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            if let Some(value) = fields.get("value").cloned() {
                state.cache.insert(key, CacheEntry { expires_at, value });
            }
        }
    }
}

/// `scheduleDiskWrite()`: coalesce writes with a 1s timer (JS `unref`s it;
/// the tokio task never blocks runtime shutdown).
/// 已有防抖任务则直接返回；否则在当前 runtime 排一个 1s 后的任务，
/// 到点收集未过期条目整体写盘。无 runtime 时无法排程，静默跳过。
fn schedule_disk_write(state: &mut CacheState) {
    if state.disk_write_timer.is_some() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // Called outside a runtime: nothing to schedule on. JS could not run
        // the timer without an event loop either.
        return;
    };
    let task = handle.spawn(async move {
        tokio::time::sleep(Duration::from_millis(DISK_WRITE_DELAY_MS)).await;
        let mut state = lock_state();
        state.disk_write_timer = None;
        let now = now_millis();
        let mut persisted = serde_json::Map::new();
        for (key, entry) in &state.cache {
            if entry.expires_at > now {
                persisted.insert(
                    key.clone(),
                    serde_json::json!({ "expiresAt": entry.expires_at, "value": entry.value }),
                );
            }
        }
        write_disk_cache(DISK_CACHE_FILE, &Value::Object(persisted));
    });
    state.disk_write_timer = Some(task);
}

/// `getCachedScan(key)`: the cached value while unexpired, else `None`
/// (expired entries are dropped).
/// 命中且未过期返回缓存值；过期条目被顺带删除并返回 None。
pub fn get_cached_scan(key: &str) -> Option<Value> {
    let mut state = lock_state();
    load_disk_entries(&mut state);
    let entry = state.cache.get(key)?.clone();
    if now_millis() >= entry.expires_at {
        state.cache.remove(key);
        return None;
    }
    Some(entry.value)
}

/// `setCachedScan(key, value, ttlMs)`: store with a TTL (`None` → the 3h
/// default).
/// 以 now+TTL 写入并调度防抖磁盘写；ttl_ms 为 None 时用 3 小时默认值。
pub fn set_cached_scan(key: &str, value: Value, ttl_ms: Option<u64>) {
    let ttl = ttl_ms.unwrap_or(DEFAULT_TTL_MS);
    let mut state = lock_state();
    state.cache.insert(
        key.to_string(),
        CacheEntry {
            expires_at: now_millis() + ttl,
            value,
        },
    );
    schedule_disk_write(&mut state);
}

/// `clearCache()`: drop all in-memory scan cache state.
/// 清空内存缓存与在途表（磁盘文件不动，闩锁也不重置）。
pub fn clear_cache() {
    let mut state = lock_state();
    state.cache.clear();
    state.in_flight.clear();
}

/// `scanWithCache(key, loader, { refresh })`: cache lookup (unless
/// `refresh`), per-key in-flight deduplication, and a global
/// two-concurrent-loader limit. Only `ok: true` results are cached.
/// 非 refresh 时先查缓存（畸形缓存载荷回落到重新加载）；未命中则取/建
/// 在途 shared future：拿到全局许可后执行 loader，仅 ok 结果写缓存，
/// 完成后摘除在途表项。同 key 并发只跑一次 loader。
pub async fn scan_with_cache<F>(key: &str, loader: F, refresh: bool) -> ScanResult
where
    F: Future<Output = ScanResult> + Send + 'static,
{
    if !refresh
        && let Some(value) = get_cached_scan(key)
        && let Ok(result) = serde_json::from_value::<ScanResult>(value)
    {
        return result;
    }
    // Malformed cached payloads (e.g. hand-edited files) fall through
    // to a fresh loader run.

    let run: Shared<BoxFuture<'static, ScanResult>> = {
        let mut state = lock_state();
        if let Some(existing) = state.in_flight.get(key) {
            existing.clone()
        } else {
            let key_owned = key.to_string();
            let future = async move {
                let _permit = SCAN_LIMIT.acquire().await.ok();
                let result = loader.await;
                if result.ok
                    && let Ok(value) = serde_json::to_value(&result)
                {
                    set_cached_scan(&key_owned, value, None);
                }
                lock_state().in_flight.remove(&key_owned);
                result
            }
            .boxed()
            .shared();
            state.in_flight.insert(key.to_string(), future.clone());
            future
        }
    };

    run.await
}

/// Test-only: reset every module-global (memory, in-flight, disk latch, and
/// any pending debounced write) so tests start from a clean slate.
/// 清内存、清在途、终止挂起的防抖写并复位磁盘闩锁，供测试隔离。
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut state = lock_state();
    if let Some(timer) = state.disk_write_timer.take() {
        timer.abort();
    }
    state.cache.clear();
    state.in_flight.clear();
    state.disk_loaded = false;
}

/// cache 测试：并发去重、并发限流、失败不缓存、refresh 旁路、磁盘
/// 持久化与重新加载、key 格式与 TTL 过期。
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::skills_catalog::error::CatalogError;
    use crate::skills_catalog::scan::SkillCatalogItem;
    use crate::skills_catalog::test_support::{EnvGuard, TEST_LOCK, unique_temp_dir};

    /// 构造一个最小合法的目录条目。
    fn item(skill_name: &str) -> SkillCatalogItem {
        SkillCatalogItem {
            repo_source: "s".to_string(),
            repo_subpath: None,
            skill_dir: format!("d/{skill_name}"),
            skill_name: skill_name.to_string(),
            frontmatter_name: None,
            description: None,
            installable: true,
            warnings: None,
        }
    }

    /// 构造 ok: true 的空扫描结果。
    fn ok_result(items: Vec<SkillCatalogItem>) -> ScanResult {
        ScanResult {
            ok: true,
            normalized_repo: Some("a/b".to_string()),
            effective_subpath: None,
            items: Some(items),
            error: None,
        }
    }

    /// 行为契约：同 key 并发只跑一次 loader 且结果一致。
    #[tokio::test]
    async fn deduplicates_concurrent_loaders_for_the_same_key() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();
        clear_cache();

        let calls = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let make_loader = {
            let counter = calls.clone();
            move || {
                let counter = counter.clone();
                async move {
                    *counter
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    ok_result(Vec::new())
                }
            }
        };

        let (a, b) = tokio::join!(
            scan_with_cache("k", make_loader(), false),
            scan_with_cache("k", make_loader(), false),
        );

        assert_eq!(
            *calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            1,
            "loader should run once"
        );
        assert_eq!(a, b);
        clear_cache();
    }

    /// 行为契约：不同 key 并发时同时在跑的 loader 峰值不超过 2。
    #[tokio::test]
    async fn limits_concurrent_scans_across_different_keys() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();
        clear_cache();

        let running = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..6 {
            let running = running.clone();
            let peak = peak.clone();
            handles.push(tokio::spawn(async move {
                scan_with_cache(
                    &format!("key-{i}"),
                    async move {
                        let now = running.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        peak.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        running.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        ok_result(Vec::new())
                    },
                    false,
                )
                .await
            }));
        }
        for handle in handles {
            handle.await.expect("join");
        }

        assert!(
            peak.load(std::sync::atomic::Ordering::SeqCst) <= 2,
            "peak concurrency {} exceeded the limit",
            peak.load(std::sync::atomic::Ordering::SeqCst)
        );
        clear_cache();
    }

    /// 行为契约：失败的扫描结果不进缓存。
    #[tokio::test]
    async fn does_not_cache_failed_scans() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();
        clear_cache();

        let failed = ScanResult {
            ok: false,
            error: Some(CatalogError::network("x")),
            ..ok_result(Vec::new())
        };
        let _ = scan_with_cache("bad", async move { failed }, false).await;

        assert_eq!(get_cached_scan("bad"), None);
        clear_cache();
    }

    /// 行为契约：refresh 跳过缓存直接重跑 loader 并更新缓存值。
    #[tokio::test]
    async fn refresh_bypasses_the_cache() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();
        clear_cache();

        set_cached_scan(
            "fresh",
            serde_json::to_value(&ok_result(vec![item("cached")])).expect("serialize"),
            None,
        );

        let result = scan_with_cache(
            "fresh",
            async move { ok_result(vec![item("reloaded")]) },
            true,
        )
        .await;

        assert_eq!(
            result
                .items
                .as_deref()
                .map(|items| items[0].skill_name.as_str()),
            Some("reloaded"),
            "refresh must re-run the loader"
        );
        let cached = get_cached_scan("fresh").expect("refreshed value cached");
        assert_eq!(
            cached.pointer("/items/0/skillName").and_then(Value::as_str),
            Some("reloaded")
        );
        clear_cache();
    }

    /// 行为契约：成功扫描防抖落盘，条目结构含 expiresAt 与 value。
    #[tokio::test]
    async fn persists_successful_scans_to_disk_for_later_processes() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();
        let dir = unique_temp_dir("skills-cache-test");
        let _env = EnvGuard::set("OMPCHAMBER_DATA_DIR", dir.to_str().expect("utf8"));
        clear_cache();

        let result = ok_result(vec![SkillCatalogItem {
            repo_source: "s".to_string(),
            repo_subpath: None,
            skill_dir: "d".to_string(),
            skill_name: "x".to_string(),
            frontmatter_name: None,
            description: None,
            installable: true,
            warnings: None,
        }]);
        let _ = scan_with_cache("persisted", async move { result }, false).await;
        tokio::time::sleep(Duration::from_millis(1_200)).await;

        let on_disk: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("skills-catalog-cache.json")).expect("cache file"),
        )
        .expect("json");
        assert_eq!(
            on_disk.pointer("/persisted/value/items/0/skillName"),
            Some(&json!("x"))
        );
        clear_cache();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 行为契约：缓存 key 格式与 JS 侧逐字符一致。
    #[test]
    fn cache_key_format_matches_js() {
        assert_eq!(get_cache_key("a/b", "skills", "id1"), "a/b::skills::id1");
        assert_eq!(get_cache_key(" a/b ", "", ""), "a/b::::");
        assert_eq!(get_cache_key("", "sub", ""), "::sub::");
    }

    /// 行为契约：零 TTL 条目立即过期，读取时被清除。
    #[tokio::test]
    async fn ttl_expiry_drops_entries() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();
        clear_cache();

        set_cached_scan(
            "short",
            serde_json::to_value(&ok_result(Vec::new())).expect("serialize"),
            Some(0),
        );
        assert_eq!(get_cached_scan("short"), None, "zero-TTL entry is expired");
        clear_cache();
    }

    /// 行为契约：磁盘上未过期条目被导入、过期条目被跳过。
    #[tokio::test]
    async fn loads_unexpired_entries_from_disk() {
        let _guard = TEST_LOCK.lock().await;
        let dir = unique_temp_dir("skills-cache-disk");
        let _env = EnvGuard::set("OMPCHAMBER_DATA_DIR", dir.to_str().expect("utf8"));
        reset_for_tests();

        let future_expiry = now_millis() + 60_000;
        let payload = json!({
            "from-disk": {
                "expiresAt": future_expiry,
                "value": { "ok": true, "normalizedRepo": "a/b", "effectiveSubpath": null, "items": [] }
            },
            "expired": {
                "expiresAt": now_millis() - 1_000,
                "value": { "ok": true, "items": [] }
            }
        });
        std::fs::write(
            dir.join("skills-catalog-cache.json"),
            serde_json::to_string(&payload).expect("serialize"),
        )
        .expect("write cache");

        let loaded = get_cached_scan("from-disk").expect("unexpired entry loads");
        assert_eq!(loaded.pointer("/normalizedRepo"), Some(&json!("a/b")));
        assert_eq!(get_cached_scan("expired"), None, "expired entry skipped");
        clear_cache();
        std::fs::remove_dir_all(&dir).ok();
    }
}
