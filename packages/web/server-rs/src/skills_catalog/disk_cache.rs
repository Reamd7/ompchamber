//! Port of `server/lib/skills-catalog/disk-cache.js`: JSON cache persistence
//! under the OpenChamber data dir (`OMPCHAMBER_DATA_DIR` or
//! `~/.config/ompchamber`) with atomic temp-file renames and 0600 modes.
//!
//! 中文说明：skills-catalog 的 JSON 缓存持久化：文件位于 OpenChamber
//! 数据目录（`OMPCHAMBER_DATA_DIR` 或 `~/.config/ompchamber`），写入走
//! 临时文件原子改名且权限位 0600。

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// `resolveDataDir()`: resolved `OMPCHAMBER_DATA_DIR` (relative values are
/// resolved against the cwd like `path.resolve`) or the default config dir.
/// 环境变量优先（相对值按 cwd 解析，等价 path.resolve）；否则 home 下
/// 的 `.config/ompchamber`；拿不到 home 时退到 `.ompchamber-data`。
pub(crate) fn resolve_data_dir() -> PathBuf {
    let from_env = std::env::var("OMPCHAMBER_DATA_DIR")
        .ok()
        .filter(|value| !value.is_empty());
    if let Some(value) = from_env {
        let path = PathBuf::from(&value);
        if path.is_absolute() {
            return path;
        }
        // JS path.resolve(): relative data dirs anchor to process.cwd().
        return std::env::current_dir().unwrap_or_default().join(path);
    }
    crate::config::home_dir()
        .map(|home| home.join(".config").join("ompchamber"))
        .unwrap_or_else(|| PathBuf::from(".ompchamber-data"))
}

/// 当前 Unix 毫秒时间戳；时钟早于 epoch 时返回 0。
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// `readDiskCache(fileName)`: parsed object or `None` when missing,
/// unreadable, malformed, or not a JSON object.
/// 只接受 JSON 对象；缺失、不可读、畸形或非对象一律 None。
pub fn read_disk_cache(file_name: &str) -> Option<Value> {
    let path = resolve_data_dir().join(file_name);
    let raw = fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => Some(Value::Object(map)),
        _ => None,
    }
}

/// `writeDiskCache(fileName, data)`: atomic temp-file rename into the data
/// dir (mkdir -p parents, 0600 temp file). Failures unlink the temp file and
/// report `false`; the in-memory cache stays authoritative.
/// 序列化后写临时文件再原子改名；失败清理临时文件并返回 false，内存
/// 缓存仍是权威数据源。
pub fn write_disk_cache(file_name: &str, data: &Value) -> bool {
    let file_path = resolve_data_dir().join(file_name);
    let base_name = file_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let temp_path = file_path.with_file_name(format!(
        "{base_name}.{}.{}.tmp",
        std::process::id(),
        now_millis()
    ));

    if write_and_rename(&file_path, &temp_path, data) {
        return true;
    }
    let _ = fs::remove_file(&temp_path);
    false
}

/// 写入核心：建父目录、以 0600 权限写临时文件（unix 分支）、原子改名
/// 到目标路径；任何一步失败都返回 false。
fn write_and_rename(file_path: &Path, temp_path: &Path, data: &Value) -> bool {
    let Ok(serialized) = serde_json::to_string(data) else {
        return false;
    };
    (|| -> std::io::Result<()> {
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::io::Write;
use crate::os_compat::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(temp_path)?;
            file.write_all(serialized.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            fs::write(temp_path, serialized.as_bytes())?;
        }
        fs::rename(temp_path, file_path)?;
        Ok(())
    })()
    .is_ok()
}

/// 磁盘缓存测试：对象往返、异常输入读 None、父目录创建与原子替换。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills_catalog::test_support::{EnvGuard, TEST_LOCK, unique_temp_dir};
    use serde_json::json;

    /// 行为契约：写入的对象能原样读回。
    #[test]
    fn round_trips_an_object() {
        let _guard = TEST_LOCK.blocking_lock();
        let dir = unique_temp_dir("skills-disk-cache");
        let _env = EnvGuard::set("OMPCHAMBER_DATA_DIR", dir.to_str().expect("utf8"));

        let payload = json!({"repo::sub::id": {"expiresAt": 123, "value": {"items": []}}});
        assert!(write_disk_cache("skills-catalog-cache.json", &payload));
        assert_eq!(read_disk_cache("skills-catalog-cache.json"), Some(payload));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 行为契约：缺失、畸形、数组与 null 文件都读为 None。
    #[test]
    fn missing_and_malformed_files_read_as_none() {
        let _guard = TEST_LOCK.blocking_lock();
        let dir = unique_temp_dir("skills-disk-cache-missing");
        let _env = EnvGuard::set("OMPCHAMBER_DATA_DIR", dir.to_str().expect("utf8"));

        assert_eq!(read_disk_cache("nope.json"), None);

        std::fs::write(dir.join("broken.json"), "{not json").expect("write");
        assert_eq!(read_disk_cache("broken.json"), None);

        std::fs::write(dir.join("array.json"), "[1,2]").expect("write");
        assert_eq!(read_disk_cache("array.json"), None);

        std::fs::write(dir.join("null.json"), "null").expect("write");
        assert_eq!(read_disk_cache("null.json"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 行为契约：自动创建父目录、替换旧文件且不留临时文件。
    #[test]
    fn creates_parent_directories_and_replaces_atomically() {
        let _guard = TEST_LOCK.blocking_lock();
        let dir = unique_temp_dir("skills-disk-cache-nested");
        let _env = EnvGuard::set("OMPCHAMBER_DATA_DIR", dir.to_str().expect("utf8"));

        let nested = json!({"a": 1});
        assert!(write_disk_cache("nested/deep/cache.json", &nested));
        assert!(dir.join("nested/deep/cache.json").exists());

        let replacement = json!({"a": 2});
        assert!(write_disk_cache("nested/deep/cache.json", &replacement));
        assert_eq!(read_disk_cache("nested/deep/cache.json"), Some(replacement));

        // No temp files left behind after successful writes.
        let leftovers: Vec<_> = std::fs::read_dir(dir.join("nested/deep"))
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
