//! Port of `server/lib/path-realpath-cache.js` (as used by proxy.js:
//! `fallbackOnError: true`, 256 entries, 10min success / 60s failure TTL)
//! plus `createDirectoryQueryCanonicalizer` — the best-effort fallback that
//! rewrites stale symlink paths in `directory=` query params before requests
//! reach the engine. Settings and project selection normalize at source; this
//! keeps old clients working without blocking the proxy hot path.
//!
//! 中文说明：移植 path-realpath-cache.js（按 proxy.js 的用法：出错回退、
//! 256 条容量、成功 10 分钟 / 失败 60 秒 TTL）与目录查询规范化器——
//! 请求到达引擎前，尽力把 directory 查询参数里过期的符号链接路径改写
//! 为规范路径，让旧客户端无需改动即可继续工作。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::proxy::headers::{percent_decode_form_lossy, percent_encode_form};

/// 解析成功的缓存时长。
const SUCCESS_TTL: Duration = Duration::from_secs(600);
/// 解析失败的缓存时长（短缓存避免反复探测不存在的目录）。
const FAILURE_TTL: Duration = Duration::from_secs(60);
/// 缓存容量上限（超出后按插入顺序淘汰）。
const MAX_ENTRIES: usize = 256;

/// 一条缓存记录：解析结果与过期时刻。
#[derive(Clone)]
struct CacheEntry {
    /// realpath 解析结果（失败路径回退为原始输入）。
    value: String,
    /// 过期时刻；到期后的命中视为未命中。
    expires_at: Instant,
}

/// realpath 结果的进程内 LRU 缓存，键为原始路径字符串。
pub(crate) struct RealpathCache {
    /// 键 → 缓存记录。
    entries: Mutex<HashMap<String, CacheEntry>>,
    /// 插入顺序队列，超容量时淘汰队首。
    order: Mutex<VecDeque<String>>,
}

/// 缓存的查改与淘汰逻辑。
impl RealpathCache {
    /// 创建空缓存。
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            order: Mutex::new(VecDeque::new()),
        }
    }

    /// Resolve a path through `realpath(3)` with the JS cache semantics:
    /// failures fall back to the original value (and are remembered for the
    /// failure TTL so a missing directory is not hammered).
    /// 中文：经 realpath 解析路径；失败回退为原始值并按失败 TTL 短缓存，
    /// 避免反复探测不存在的目录。空输入直接原样返回。
    pub(crate) async fn resolve(&self, value: &str) -> String {
        if value.is_empty() {
            return value.to_string();
        }
        if let Some(hit) = self.lookup(value) {
            return hit;
        }

        let owned = value.to_string();
        let outcome =
            tokio::task::spawn_blocking(move || std::fs::canonicalize(&owned).map(path_to_string))
                .await;

        let (resolved, ttl) = match outcome {
            Ok(Ok(canonical)) if !canonical.is_empty() => (canonical, SUCCESS_TTL),
            // Failure (or empty result) falls back to the input, cached briefly.
            _ => (value.to_string(), FAILURE_TTL),
        };
        self.remember(value.to_string(), resolved.clone(), ttl);
        resolved
    }

    /// 查缓存：过期条目立即清除；命中时刷新 LRU 近期性。
    fn lookup(&self, key: &str) -> Option<String> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let hit = entries.get(key).cloned()?;
        if hit.expires_at <= now {
            entries.remove(key);
            if let Ok(mut order) = self.order.lock() {
                order.retain(|k| k != key);
            }
            return None;
        }
        let value = hit.value.clone();
        // Refresh recency like the JS delete+set on hit.
        entries.insert(key.to_string(), hit);
        if let Ok(mut order) = self.order.lock() {
            order.retain(|k| k != key);
            order.push_back(key.to_string());
        }
        Some(value)
    }
    /// 写缓存：覆盖旧值、刷新近期性，超容量时淘汰最旧条目。
    fn remember(&self, key: String, value: String, ttl: Duration) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut order = self.order.lock().unwrap_or_else(|e| e.into_inner());
        entries.remove(&key);
        order.retain(|k| k != &key);
        entries.insert(
            key.clone(),
            CacheEntry {
                value,
                expires_at: Instant::now() + ttl,
            },
        );
        order.push_back(key);
        while order.len() > MAX_ENTRIES {
            if let Some(oldest) = order.pop_front() {
                entries.remove(&oldest);
            }
        }
    }
}

/// PathBuf 转字符串；Windows 上剥掉 canonicalize 返回的 `\\?\` 前缀，
/// 保持与 Node realpath 一致的引擎兼容路径。
fn path_to_string(path: PathBuf) -> String {
    let text = path.to_string_lossy().into_owned();
    // std::fs::canonicalize returns verbatim `\\?\C:\...` paths on Windows;
    // node's fs.promises.realpath does not — strip the prefix so directory
    // params stay engine-compatible.
    #[cfg(windows)]
    {
        text.strip_prefix(r"\\?\")
            .map(str::to_string)
            .unwrap_or(text)
    }
    #[cfg(not(windows))]
    {
        text
    }
}

/// Port of `createDirectoryQueryCanonicalizer`: when the request URL carries a
/// `directory=` query param, resolve it to its canonical path and rewrite the
/// FIRST occurrence (dropping later duplicates, matching
/// `URLSearchParams.set`). Everything else stays byte-for-byte.
/// 中文：请求 URL 带 directory 参数时，解析为规范路径并改写第一次出现
/// （丢弃后续重复项，对齐 URLSearchParams.set 的语义）；URL 其余部分
/// 逐字节保留，无 directory 或解析结果未变化时原样返回。
pub(crate) async fn canonicalize_directory_query(
    cache: &RealpathCache,
    request_url: &str,
) -> String {
    if !request_url.contains("directory=") {
        return request_url.to_string();
    }
    let Some((path, query)) = request_url.split_once('?') else {
        return request_url.to_string();
    };

    let pairs: Vec<&str> = query.split('&').collect();
    let mut directory_index: Option<usize> = None;
    let mut directory_value: Option<String> = None;
    for (index, pair) in pairs.iter().enumerate() {
        let raw_key = pair.split('=').next().unwrap_or("");
        if percent_decode_form_lossy(raw_key) == "directory" {
            directory_index = Some(index);
            directory_value = pair
                .split_once('=')
                .map(|(_, raw_value)| percent_decode_form_lossy(raw_value))
                .filter(|decoded| !decoded.is_empty());
            break;
        }
    }
    let (Some(index), Some(directory)) = (directory_index, directory_value) else {
        return request_url.to_string();
    };

    let canonical = cache.resolve(&directory).await;
    if canonical == directory {
        return request_url.to_string();
    }

    let rewritten = format!("directory={}", percent_encode_form(&canonical));
    let mut kept: Vec<String> = Vec::with_capacity(pairs.len());
    for (position, pair) in pairs.iter().enumerate() {
        let raw_key = pair.split('=').next().unwrap_or("");
        if percent_decode_form_lossy(raw_key) == "directory" {
            if position == index {
                kept.push(rewritten.clone());
            } else {
                // URLSearchParams.set removes duplicates after the one it sets.
            }
            continue;
        }
        kept.push((*pair).to_string());
    }
    format!("{path}?{}", kept.join("&"))
}

/// 缓存与 directory 改写的行为测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 按名称 + 进程号创建（并清空重建）互不冲突的临时目录。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-proxy-realpath-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// 验证：符号链接路径被改写为 realpath，其余查询参数保留。
    #[tokio::test]
    async fn rewrites_directory_query_through_realpath() {
        let dir = temp_dir("rewrite");
        let real = dir.join("real");
        std::fs::create_dir_all(&real).expect("create real");
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(dir.join("link"));
            std::os::unix::fs::symlink(&real, dir.join("link")).expect("symlink");
        }
        let cache = RealpathCache::new();

        let encoded = percent_encode_form(dir.join("link").to_string_lossy().as_ref());
        let url = format!("/session?directory={encoded}&limit=500");
        let rewritten = canonicalize_directory_query(&cache, &url).await;

        assert_ne!(rewritten, url);
        assert!(rewritten.starts_with("/session?directory="));
        assert!(rewritten.contains("real"));
        assert!(
            rewritten.contains("limit=500"),
            "other params survive: {rewritten}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 验证：无 directory 参数的 URL 原样返回。
    #[tokio::test]
    async fn keeps_url_untouched_without_directory_param() {
        let cache = RealpathCache::new();
        let url = "/session?limit=500";
        assert_eq!(canonicalize_directory_query(&cache, url).await, url);
    }

    /// 验证：directory 为空值时 URL 原样返回。
    #[tokio::test]
    async fn keeps_url_untouched_for_empty_directory() {
        let cache = RealpathCache::new();
        let url = "/session?directory=";
        assert_eq!(canonicalize_directory_query(&cache, url).await, url);
    }

    /// 验证：目录不存在时失败缓存回退为原始输入。
    #[tokio::test]
    async fn keeps_url_untouched_for_missing_path() {
        let cache = RealpathCache::new();
        let missing = format!("/definitely/not/here-{}", std::process::id());
        let encoded = percent_encode_form(&missing);
        let url = format!("/session?directory={encoded}");
        assert_eq!(canonicalize_directory_query(&cache, &url).await, url);
    }

    /// 验证：改写后重复的 directory 参数只保留一个。
    #[tokio::test]
    async fn removes_duplicate_directory_pairs_after_rewrite() {
        let dir = temp_dir("dup");
        let real = dir.join("real");
        std::fs::create_dir_all(&real).expect("create real");
        #[cfg(unix)]
        {
            let _ = std::fs::remove_file(dir.join("link2"));
            std::os::unix::fs::symlink(&real, dir.join("link2")).expect("symlink");
        }
        let cache = RealpathCache::new();

        let encoded = percent_encode_form(dir.join("link2").to_string_lossy().as_ref());
        let url = format!("/session?directory={encoded}&directory=zzz");
        let rewritten = canonicalize_directory_query(&cache, &url).await;
        assert_eq!(rewritten.matches("directory=").count(), 1, "{rewritten}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 验证：失败结果被短缓存，第二次命中不再触磁盘。
    #[tokio::test]
    async fn caches_failures_and_falls_back_to_input() {
        let cache = RealpathCache::new();
        let missing = format!("/missing-{}/x", std::process::id());
        let first = cache.resolve(&missing).await;
        assert_eq!(first, missing);
        // Second hit comes from the failure cache entry (same answer, no fs hit).
        let second = cache.resolve(&missing).await;
        assert_eq!(second, missing);
    }
}
