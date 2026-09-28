//! walkthrough 的磁盘存储，两个目录两种职责：entries/ 存内容寻址的
//! 不可变缓存条目（key 由当前 diff 派生，命中即“这份导览正是为当前
//! 代码写的”，过期即 miss）；pointers/ 存 repo+source → cacheKey 指针。
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

/// 缓存条目数量上限（触发 LRU 驱逐）。
const MAX_ENTRIES: usize = 200;
/// entries 目录总字节数上限。
const MAX_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
/// 单个 JSON 文件允许读取的字节数上限（防异常大文件）。
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// 单次指针清理最多处理的文件数。
const PRUNE_LIMIT: usize = 500;

/// 计算 SHA-256 并返回小写十六进制串。
fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 当前 Unix 毫秒时间戳（时钟早于纪元时返回 0）。
fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// 写入 entries/<sha256>.json 的一条缓存（字段顺序与省略对齐 JS 序列化）。
/// One cache entry as written to `entries/<sha256>.json`. Field order mirrors
/// the JS spread (`{ walkthroughVersion, ...entry }`); absent optional fields
/// are omitted like `JSON.stringify` drops `undefined`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
/// 写入时的 WALKTHROUGH_VERSION（版本不匹配的旧条目读作 miss）。
    pub walkthrough_version: i64,
/// 所属 cache key（部分写入路径不落盘该字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
/// 生成时间（ISO 字符串）。
    pub generated_at: String,
/// 生成时的仓库根路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
/// 生成时的 source key。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
/// 生成所用模型的描述子集。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
/// 输出语言 tag。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
/// walkthrough 结果本体（chapters 等）。
    pub walkthrough: Value,
}

/// 序列化辅助。
impl CacheEntry {
/// 序列化为 JSON Value（失败得 Null）。
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// 写入 pointers/<sha256>.json 的指针：repo+source 到 cacheKey 的映射。
/// One pointer as written to `pointers/<sha256>.json`. Only `cacheKey` is
/// required when reading; writes always include the full shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pointer {
/// 仓库根路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_root: Option<String>,
/// source key。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
/// 指向的缓存条目 key（读取时唯一必填项）。
    pub cache_key: String,
/// 指向条目的生成时间。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
}

/// 根于 <data_dir>/walkthroughs 的存储：entries 与 pointers 两个子目录。
/// The walkthrough store rooted at `<data_dir>/walkthroughs`.
pub struct Store {
/// 缓存条目目录。
    entries_dir: PathBuf,
/// 指针目录。
    pointers_dir: PathBuf,
}

/// 缓存与指针的读写及维护（驱逐、清理）。
impl Store {
/// 由 data_dir 推导两个子目录路径（不创建目录）。
    pub fn new(data_dir: &Path) -> Self {
        let walkthrough_dir = data_dir.join("walkthroughs");
        Self {
            entries_dir: walkthrough_dir.join("entries"),
            pointers_dir: walkthrough_dir.join("pointers"),
        }
    }

/// 条目目录路径。
    pub fn entries_dir(&self) -> &Path {
        &self.entries_dir
    }

/// 指针目录路径。
    pub fn pointers_dir(&self) -> &Path {
        &self.pointers_dir
    }

/// cacheKey → entries/<key>.json 的路径。
    fn entry_path(&self, cache_key: &str) -> PathBuf {
        self.entries_dir.join(format!("{cache_key}.json"))
    }

/// repo+source（以 \0 连接后取 SHA-256）→ pointers/<digest>.json 的路径。
    fn pointer_path(&self, repo_root: &str, source_key: &str) -> PathBuf {
        let digest = sha256_hex(&format!("{repo_root}\0{source_key}"));
        self.pointers_dir.join(format!("{digest}.json"))
    }

/// 容错读取 JSON：缺失、非文件、超限或解析失败一律 None，绝不报错。
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

/// 先写临时文件再 rename 的原子写：崩溃不会留下半写文件；失败时清理
/// 临时文件并返回 false。
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

/// 读取缓存条目：版本不匹配或 walkthrough 形状非法都读作 miss。
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

/// 写入条目（补 walkthroughVersion 字段）并按需触发 LRU 驱逐。
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

/// 读取 repo+source 的指针（缺失或损坏即 None）。
    /// `readPointer`.
    pub fn read_pointer(&self, repo_root: &str, source_key: &str) -> Option<Pointer> {
        let value = self.read_json(&self.pointer_path(repo_root, source_key))?;
        let pointer: Pointer = serde_json::from_value(value).ok()?;
        Some(pointer)
    }

/// 原子写入指针。
    /// `writePointer`.
    pub fn write_pointer(&self, repo_root: &str, source_key: &str, pointer: &Pointer) -> bool {
        let value = serde_json::to_value(pointer).unwrap_or(Value::Null);
        self.write_json_atomic(&self.pointer_path(repo_root, source_key), &value)
    }

/// 超过条目数或总字节上限时按 atime 淘汰最久未用的条目；指针很小不动，
/// 指向已驱逐条目的指针读作“没有导览”（诚实的答案）；删除失败留待下次。
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

/// 删除仓库目录已确定不存在的指针，返回删除数。只删“确实不存在”的：
/// 断连的共享盘或权限错误不算；上限 PRUNE_LIMIT 个，避免病态目录拖成
/// 长尾工作。
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

/// 由全部影响产出的输入构建内容寻址 key：规范 JSON 与 JS 的 stringify
/// 逐字节一致，文件按 JS 字符串序（UTF-16 码元）排序后整体取 SHA-256；
/// 任一输入变化都产生 miss 而非陈旧命中。
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

/// 存储层测试（见 tests 子模块文件）。
#[cfg(test)]
mod tests;
