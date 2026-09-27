//! Minimal port of `server/lib/opencode/auth.js` (the parts small-model
//! uses): read `~/.local/share/opencode/auth.json` and write it back with a
//! backup + 0600 modes after the OpenAI OAuth refresh. The wider API
//! (`removeProviderAuth` and friends) belongs to the opencode module port.
//!
//! 中文说明：`server/lib/opencode/auth.js` 的最小移植（仅 small-model 用到的部分）：
//! 读取 ~/.local/share/opencode/auth.json，并在 OpenAI OAuth 刷新后带备份与 0600
//! 权限写回。更完整的 API（removeProviderAuth 等）属于 opencode 模块的移植范围。

use std::path::PathBuf;

use serde_json::Value;

/// Injectable seam mirroring the module functions `call.js` imports.
///
/// 中文补充：call.js 依赖的读/写两个模块函数的可注入抽象。
pub trait AuthStore: Send + Sync {
    /// 读取当前 auth 内容；实现自行决定缺失/损坏文件的处理。
    fn read(&self) -> Result<Value, String>;
    /// 全量写回 auth 内容；实现负责备份与权限收紧。
    fn write(&self, auth: &Value) -> Result<(), String>;
}

/// 共享的 auth 存储句柄。
pub type SharedAuthStore = std::sync::Arc<dyn AuthStore>;

/// OpenCode 数据目录 ~/.local/share/opencode；取不到 home 目录时返回 None。
pub fn opencode_data_dir() -> Option<PathBuf> {
    crate::config::home_dir().map(|home| home.join(".local").join("share").join("opencode"))
}

/// auth.json 的固定路径；无 home 时退化为相对路径，只求错误可定位。
pub fn auth_file_path() -> PathBuf {
    opencode_data_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share/opencode"))
        .join("auth.json")
}

/// Filesystem-backed store over the fixed OpenCode auth location.
///
/// 中文补充：落在上述固定路径上的文件系统实现。
pub struct FsAuthStore {
    /// auth.json 完整路径。
    path: PathBuf,
}

/// 构造入口。
impl FsAuthStore {
    /// 以显式路径构造（测试与生产共用同一实现）。
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

/// 默认实例指向 OpenCode auth.json 的固定路径。
impl Default for FsAuthStore {
    /// 以 OpenCode auth.json 的固定路径构造默认实例。
    fn default() -> Self {
        Self::new(auth_file_path())
    }
}

/// 文件系统实现：读侧把缺失/空白文件视作空对象；写侧建目录、备份旧文件并收紧权限。
impl AuthStore for FsAuthStore {
    /// 读取并解析 JSON：文件缺失或内容空白返回空对象；IO 失败或 JSON 损坏返回与
    /// JS 版一致的错误消息。
    fn read(&self) -> Result<Value, String> {
        if !self.path.exists() {
            return Ok(Value::Object(Default::default()));
        }
        let content = std::fs::read_to_string(&self.path)
            .map_err(|_| "Failed to read OpenCode auth configuration".to_string())?;
        if content.trim().is_empty() {
            return Ok(Value::Object(Default::default()));
        }
        serde_json::from_str(content.trim())
            .map_err(|_| "Failed to read OpenCode auth configuration".to_string())
    }

    /// 写回流程：确保父目录存在（unix 下 0700）→ 旧文件先备份为 .ompchamber.backup
    /// 后缀文件（0600）→ 以 pretty JSON 写入并把新文件权限设为 0600；任何一步失败
    /// 都返回统一的错误消息。
    fn write(&self, auth: &Value) -> Result<(), String> {
        // 统一的写失败错误消息（与 JS 版文案一致）。
        fn fail() -> String {
            "Failed to write OpenCode auth configuration".to_string()
        }
        let dir = self
            .path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&dir).map_err(|_| fail())?;
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        if self.path.exists() {
            let backup = self.path.with_extension("json.ompchamber.backup");
            std::fs::copy(&self.path, &backup).map_err(|_| fail())?;
            #[cfg(unix)]
            {
use crate::os_compat::PermissionsExt;
                let _ = std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600));
            }
            tracing::info!("Created auth backup: {}", backup.display());
        }
        let payload = serde_json::to_string_pretty(auth).map_err(|_| fail())?;
        std::fs::write(&self.path, payload).map_err(|_| fail())?;
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::info!("Successfully wrote auth file");
        Ok(())
    }
}

/// In-memory store for tests (the JS suites mock `readAuthFile`).
///
/// 中文补充：测试用内存实现（对应 JS 测试里 mock readAuthFile）。
pub struct MemoryAuthStore {
    /// 当前值；Mutex 保证并发读写安全。
    pub value: std::sync::Mutex<Value>,
}

/// 构造入口。
impl MemoryAuthStore {
    /// 以给定 JSON 值构造。
    pub fn new(value: Value) -> Self {
        Self {
            value: std::sync::Mutex::new(value),
        }
    }
}

/// 内存实现：读返回当前值的克隆快照，写整体替换。
impl AuthStore for MemoryAuthStore {
    /// 返回当前值的克隆（锁中毒时也恢复出内部值）。
    fn read(&self) -> Result<Value, String> {
        Ok(self
            .value
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone())
    }

    /// 整体替换当前值（锁中毒时同样先恢复再写入）。
    fn write(&self, auth: &Value) -> Result<(), String> {
        *self.value.lock().unwrap_or_else(|error| error.into_inner()) = auth.clone();
        Ok(())
    }
}

/// 文件存储的往返、备份时机与损坏文件错误消息测试。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::small_model::http::now_ms;
    use serde_json::json;

    /// 验证读写往返一致，且备份仅在覆盖已存在的文件时生成、内容保留上一次写入。
    #[test]
    fn fs_store_round_trips_with_backup() {
        let dir = std::env::temp_dir().join(format!("sm-auth-{}", now_ms()));
        let path = dir.join("auth.json");
        let store = FsAuthStore::new(path.clone());
        assert_eq!(store.read().unwrap(), json!({}));
        store
            .write(&json!({ "openai": { "type": "api", "key": "k" } }))
            .unwrap();
        assert_eq!(store.read().unwrap()["openai"]["key"], json!("k"));
        // The backup is only created when overwriting an existing file.
        assert!(!dir.join("auth.json.ompchamber.backup").exists());
        store
            .write(&json!({ "anthropic": { "type": "api", "key": "sk" } }))
            .unwrap();
        assert!(dir.join("auth.json.ompchamber.backup").exists());
        let backup: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("auth.json.ompchamber.backup")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            backup["openai"]["key"],
            json!("k"),
            "the backup preserved the previous write's content"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 验证损坏的 auth.json 读取失败并返回与 JS 版完全一致的错误消息。
    #[test]
    fn fs_store_rejects_corrupt_file_with_the_js_message() {
        let dir = std::env::temp_dir().join(format!("sm-auth-bad-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, "{not json").unwrap();
        let store = FsAuthStore::new(path);
        assert_eq!(
            store.read().unwrap_err(),
            "Failed to read OpenCode auth configuration"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
