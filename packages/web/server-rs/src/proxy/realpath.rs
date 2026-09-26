//! Port of `server/lib/path-realpath-cache.js` (as used by proxy.js:
//! `fallbackOnError: true`, 256 entries, 10min success / 60s failure TTL)
//! plus `createDirectoryQueryCanonicalizer` — the best-effort fallback that
//! rewrites stale symlink paths in `directory=` query params before requests
//! reach the engine. Settings and project selection normalize at source; this
//! keeps old clients working without blocking the proxy hot path.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::proxy::headers::{percent_decode_form_lossy, percent_encode_form};

const SUCCESS_TTL: Duration = Duration::from_secs(600);
const FAILURE_TTL: Duration = Duration::from_secs(60);
const MAX_ENTRIES: usize = 256;

#[derive(Clone)]
struct CacheEntry {
    value: String,
    expires_at: Instant,
}

pub(crate) struct RealpathCache {
    entries: Mutex<HashMap<String, CacheEntry>>,
    order: Mutex<VecDeque<String>>,
}

impl RealpathCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            order: Mutex::new(VecDeque::new()),
        }
    }

    /// Resolve a path through `realpath(3)` with the JS cache semantics:
    /// failures fall back to the original value (and are remembered for the
    /// failure TTL so a missing directory is not hammered).
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn keeps_url_untouched_without_directory_param() {
        let cache = RealpathCache::new();
        let url = "/session?limit=500";
        assert_eq!(canonicalize_directory_query(&cache, url).await, url);
    }

    #[tokio::test]
    async fn keeps_url_untouched_for_empty_directory() {
        let cache = RealpathCache::new();
        let url = "/session?directory=";
        assert_eq!(canonicalize_directory_query(&cache, url).await, url);
    }

    #[tokio::test]
    async fn keeps_url_untouched_for_missing_path() {
        let cache = RealpathCache::new();
        let missing = format!("/definitely/not/here-{}", std::process::id());
        let encoded = percent_encode_form(&missing);
        let url = format!("/session?directory={encoded}");
        assert_eq!(canonicalize_directory_query(&cache, &url).await, url);
    }

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
