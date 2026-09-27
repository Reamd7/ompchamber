//! Minimal port of `server/lib/opencode/auth.js` (the parts small-model
//! uses): read `~/.local/share/opencode/auth.json` and write it back with a
//! backup + 0600 modes after the OpenAI OAuth refresh. The wider API
//! (`removeProviderAuth` and friends) belongs to the opencode module port.

use std::path::PathBuf;

use serde_json::Value;

/// Injectable seam mirroring the module functions `call.js` imports.
pub trait AuthStore: Send + Sync {
    fn read(&self) -> Result<Value, String>;
    fn write(&self, auth: &Value) -> Result<(), String>;
}

pub type SharedAuthStore = std::sync::Arc<dyn AuthStore>;

pub fn opencode_data_dir() -> Option<PathBuf> {
    crate::config::home_dir().map(|home| home.join(".local").join("share").join("opencode"))
}

pub fn auth_file_path() -> PathBuf {
    opencode_data_dir()
        .unwrap_or_else(|| PathBuf::from(".local/share/opencode"))
        .join("auth.json")
}

/// Filesystem-backed store over the fixed OpenCode auth location.
pub struct FsAuthStore {
    path: PathBuf,
}

impl FsAuthStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl Default for FsAuthStore {
    fn default() -> Self {
        Self::new(auth_file_path())
    }
}

impl AuthStore for FsAuthStore {
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

    fn write(&self, auth: &Value) -> Result<(), String> {
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
pub struct MemoryAuthStore {
    pub value: std::sync::Mutex<Value>,
}

impl MemoryAuthStore {
    pub fn new(value: Value) -> Self {
        Self {
            value: std::sync::Mutex::new(value),
        }
    }
}

impl AuthStore for MemoryAuthStore {
    fn read(&self) -> Result<Value, String> {
        Ok(self
            .value
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone())
    }

    fn write(&self, auth: &Value) -> Result<(), String> {
        *self.value.lock().unwrap_or_else(|error| error.into_inner()) = auth.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::small_model::http::now_ms;
    use serde_json::json;

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
