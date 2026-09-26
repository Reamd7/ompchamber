//! Port of `server/lib/walkthrough/store.js`.
//!
//! Two artifacts with two different jobs.
//!
//! Cache entries are content-addressed and immutable: the key is derived from
//! the *current* diff, so a hit means "this walkthrough was written about
//! exactly this code". There is no freshness question to ask of an entry —
//! staleness is a miss.
//!
use super::digest::DigestFile;
use super::schema::{PROMPT_VERSION, WALKTHROUGH_VERSION};

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const MAX_ENTRIES: usize = 200;
const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const PRUNE_LIMIT: usize = 500;

fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// One cache entry as written to `entries/<sha256>.json`. Field order mirrors
/// the JS spread (`{ walkthroughVersion, ...entry }`); absent optional fields
/// are omitted like `JSON.stringify` drops `undefined`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    pub walkthrough_version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    pub generated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub walkthrough: Value,
}

impl CacheEntry {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// One pointer as written to `pointers/<sha256>.json`. Only `cacheKey` is
/// required when reading; writes always include the full shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pointer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    pub cache_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
}

/// The walkthrough store rooted at `<data_dir>/walkthroughs`.
pub struct Store {
    entries_dir: PathBuf,
    pointers_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: &Path) -> Self {
        let walkthrough_dir = data_dir.join("walkthroughs");
        Self {
            entries_dir: walkthrough_dir.join("entries"),
            pointers_dir: walkthrough_dir.join("pointers"),
        }
    }

    pub fn entries_dir(&self) -> &Path {
        &self.entries_dir
    }

    pub fn pointers_dir(&self) -> &Path {
        &self.pointers_dir
    }

    fn entry_path(&self, cache_key: &str) -> PathBuf {
        self.entries_dir.join(format!("{cache_key}.json"))
    }

    fn pointer_path(&self, repo_root: &str, source_key: &str) -> PathBuf {
        let digest = sha256_hex(&format!("{repo_root}\0{source_key}"));
        self.pointers_dir.join(format!("{digest}.json"))
    }

    /// `readJson`: missing, unreadable, or corrupt all mean the same thing to
    /// callers — no usable cached walkthrough. Never an error: a bad cache
    /// file must not break the feature.
    fn read_json(&self, path: &Path) -> Option<Value> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
            return None;
        }
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// `writeJsonAtomic`: atomic so a crash mid-write leaves the previous
    /// entry intact rather than a half-written file that later fails to parse.
    fn write_json_atomic(&self, path: &Path, value: &Value) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        if let Err(error) = std::fs::create_dir_all(parent) {
            tracing::error!("[walkthrough] failed to create store directory: {error}");
            return false;
        }
        let mut tmp_name = path.as_os_str().to_os_string();
        tmp_name.push(format!(".{}.{}.tmp", std::process::id(), now_millis()));
        let tmp = std::path::PathBuf::from(tmp_name);
        let body = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
        match std::fs::write(&tmp, body) {
            Ok(()) => match std::fs::rename(&tmp, path) {
                Ok(()) => true,
                Err(error) => {
                    tracing::error!("[walkthrough] failed to write store file: {error}");
                    let _ = std::fs::remove_file(&tmp);
                    false
                }
            },
            Err(error) => {
                tracing::error!("[walkthrough] failed to write store file: {error}");
                let _ = std::fs::remove_file(&tmp);
                false
            }
        }
    }

    /// `isWalkthroughEntry` + read: an entry written by an incompatible
    /// version reads as a miss rather than a crash.
    pub fn read_cached_walkthrough(&self, cache_key: &str) -> Option<CacheEntry> {
        let value = self.read_json(&self.entry_path(cache_key))?;
        let entry: CacheEntry = serde_json::from_value(value).ok()?;
        if entry.walkthrough_version != WALKTHROUGH_VERSION {
            return None;
        }
        let chapters_ok = entry
            .walkthrough
            .get("chapters")
            .is_some_and(Value::is_array);
        if entry.walkthrough.is_null() || !chapters_ok {
            return None;
        }
        Some(entry)
    }

    /// `writeCachedWalkthrough`: writes `{ walkthroughVersion, ...entry }`
    /// and evicts least-recently-used entries past the bounds.
    pub fn write_cached_walkthrough(&self, cache_key: &str, entry: &CacheEntry) -> bool {
        let mut value = serde_json::to_value(entry).unwrap_or(Value::Null);
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "walkthroughVersion".to_string(),
                Value::from(WALKTHROUGH_VERSION),
            );
        }
        let written = self.write_json_atomic(&self.entry_path(cache_key), &value);
        if written {
            self.evict_entries();
        }
        written
    }

    /// `readPointer`.
    pub fn read_pointer(&self, repo_root: &str, source_key: &str) -> Option<Pointer> {
        let value = self.read_json(&self.pointer_path(repo_root, source_key))?;
        let pointer: Pointer = serde_json::from_value(value).ok()?;
        Some(pointer)
    }

    /// `writePointer`.
    pub fn write_pointer(&self, repo_root: &str, source_key: &str, pointer: &Pointer) -> bool {
        let value = serde_json::to_value(pointer).unwrap_or(Value::Null);
        self.write_json_atomic(&self.pointer_path(repo_root, source_key), &value)
    }

    /// `evictEntries`: bound the cache by count and total size, dropping
    /// least-recently-used entries. Pointers are tiny and are left alone; a
    /// pointer to an evicted entry simply reads as "no walkthrough", which is
    /// the truthful answer.
    fn evict_entries(&self) {
        let Ok(read_dir) = std::fs::read_dir(&self.entries_dir) else {
            return;
        };
        let mut files: Vec<(PathBuf, u64, u128)> = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.ends_with(".json") {
                continue;
            }
            let full = entry.path();
            if let Ok(metadata) = std::fs::metadata(&full) {
                let atime = metadata
                    .accessed()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis())
                    .unwrap_or_default();
                files.push((full, metadata.len(), atime));
            }
        }

        let mut total_bytes: u64 = files.iter().map(|(_, size, _)| size).sum();
        let mut count = files.len();
        if count <= MAX_ENTRIES && total_bytes <= MAX_TOTAL_BYTES {
            return;
        }

        files.sort_by_key(|(_, _, atime)| *atime);
        for (full, size, _) in files {
            if count <= MAX_ENTRIES && total_bytes <= MAX_TOTAL_BYTES {
                break;
            }
            if std::fs::remove_file(&full).is_ok() {
                count -= 1;
                total_bytes = total_bytes.saturating_sub(size);
            }
            // Files we cannot remove are skipped; the next write retries.
        }
    }

    /// `pruneMissingRepositories`: drop pointers for repositories that no
    /// longer exist. Only ever removes entries whose subject is provably gone.
    ///
    /// Housekeeping runs off the request path: the paths being checked are
    /// user repositories, where a worktree on an unplugged drive or an
    /// unreachable share can make one existence check hang for seconds, and
    /// the cap keeps a pathological directory from turning into a long tail
    /// of work. Returns the number of removed pointers.
    pub async fn prune_missing_repositories(&self) -> usize {
        let names: Vec<String> = match tokio::fs::read_dir(&self.pointers_dir).await {
            Ok(mut read_dir) => {
                let mut names = Vec::new();
                while let Ok(Some(entry)) = read_dir.next_entry().await {
                    if let Some(name) = entry.file_name().to_str()
                        && name.ends_with(".json")
                    {
                        names.push(name.to_string());
                    }
                }
                names
            }
            Err(_) => return 0,
        };

        let mut removed = 0usize;
        for name in names.into_iter().take(PRUNE_LIMIT) {
            let full = self.pointers_dir.join(&name);
            let raw = match tokio::fs::read_to_string(&full).await {
                Ok(raw) => raw,
                Err(_) => continue,
            };
            let value: Value = match serde_json::from_str(&raw) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(repo_root) = value.get("repoRoot").and_then(Value::as_str) else {
                continue;
            };

            match tokio::fs::metadata(repo_root).await {
                Ok(_) => continue,
                // Unreachable is not the same as gone. Only a definite "no
                // such file" justifies deleting: a disconnected share or a
                // permissions error must not cost the user their walkthroughs.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => continue,
            }

            if tokio::fs::remove_file(&full).await.is_ok() {
                removed += 1;
            }
            // Leave failures; the next prune retries.
        }
        removed
    }
}

/// `buildCacheKey`: content-addressed key. Every input that can change the
/// output is in here: change any of them and you get a miss rather than a
/// stale hit.
///
/// The canonical form is byte-identical to the JS `JSON.stringify` of the
/// equivalent object (field order, key order, compact separators), and files
/// are sorted by path in JS string order (UTF-16 code units).
pub fn build_cache_key(
    repo_root: &str,
    source_key: &str,
    provider_id: &str,
    model_id: &str,
    language: &str,
    files: &[DigestFile],
) -> String {
    let json = |value: &str| {
        // serde_json and JS `JSON.stringify` escape the same set for our
        // domain; non-ASCII passes through verbatim.
        serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
    };
    let mut sorted: Vec<&DigestFile> = files.iter().collect();
    sorted.sort_by(|a, b| {
        let a_key: Vec<u16> = a.path.encode_utf16().collect();
        let b_key: Vec<u16> = b.path.encode_utf16().collect();
        a_key.cmp(&b_key)
    });

    let mut files_json = String::from("[");
    for (index, file) in sorted.iter().enumerate() {
        if index > 0 {
            files_json.push(',');
        }
        files_json.push_str("{\"path\":");
        files_json.push_str(&json(&file.path));
        files_json.push_str(",\"status\":");
        files_json.push_str(&json(&file.status));
        files_json.push_str(",\"hunkIds\":[");
        for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            if hunk_index > 0 {
                files_json.push(',');
            }
            files_json.push_str(&json(&hunk.id));
        }
        files_json.push_str("]}");
    }
    files_json.push(']');

    let canonical = format!(
        "{{\"walkthroughVersion\":{},\"promptVersion\":{},\"repoRoot\":{},\"sourceKey\":{},\"providerID\":{},\"modelID\":{},\"language\":{},\"files\":{}}}",
        WALKTHROUGH_VERSION,
        PROMPT_VERSION,
        json(repo_root),
        json(source_key),
        json(provider_id),
        json(model_id),
        json(language),
        files_json,
    );
    sha256_hex(&canonical)
}

#[cfg(test)]
mod tests;
