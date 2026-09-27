//! Port of `opencode/auth.js`: the OpenCode `auth.json` store under
//! `~/.local/share/opencode` — read, defensive write (0700/0600 perms,
//! `.ompchamber.backup`), provider auth lookup/removal. The file's contents
//! are credentials and are never logged.

use crate::os_compat::PermissionsExt;
use std::path::PathBuf;

use serde_json::{Map, Value};

use super::OpenCodeEnv;
use super::webutil::js_truthy;

pub(crate) fn auth_file(env: &OpenCodeEnv) -> PathBuf {
    env.data_dir.join("auth.json")
}

/// `readAuthFile`.
pub(crate) fn read_auth_file(env: &OpenCodeEnv) -> Result<Value, String> {
    let path = auth_file(env);
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let content = std::fs::read_to_string(&path).map_err(|error| {
        tracing::error!("Failed to read auth file: {error}");
        "Failed to read OpenCode auth configuration"
    })?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(trimmed).map_err(|error| {
        tracing::error!("Failed to read auth file: {error}");
        "Failed to read OpenCode auth configuration".to_string()
    })
}

/// `writeAuthFile` — never logs the payload.
pub(crate) fn write_auth_file(env: &OpenCodeEnv, auth: &Value) -> Result<(), String> {
    fn failure<T>(_: T) -> String {
        "Failed to write OpenCode auth configuration".to_string()
    }
    if !env.data_dir.exists() {
        std::fs::create_dir_all(&env.data_dir).map_err(failure)?;
    }
    #[cfg(unix)]
    if let Ok(perms) = std::fs::metadata(&env.data_dir) {
        let mut perms = perms.permissions();
        perms.set_mode(0o700);
        let _ = std::fs::set_permissions(&env.data_dir, perms);
    }

    let path = auth_file(env);
    if path.exists() {
        let backup = backup_path(&path);
        std::fs::copy(&path, &backup).map_err(failure)?;
        #[cfg(unix)]
        if let Ok(perms) = std::fs::metadata(&backup) {
            let mut perms = perms.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(&backup, perms);
        }
        tracing::info!("Created auth backup: {}", backup.display());
    }

    let serialized = serde_json::to_string_pretty(auth).map_err(failure)?;
    std::fs::write(&path, serialized).map_err(failure)?;
    #[cfg(unix)]
    if let Ok(perms) = std::fs::metadata(&path) {
        let mut perms = perms.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(&path, perms);
    }
    tracing::info!("Successfully wrote auth file");
    Ok(())
}

fn backup_path(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".ompchamber.backup");
    PathBuf::from(name)
}

/// `removeProviderAuth` — JS truthiness on the stored entry decides whether
/// anything is there to remove.
pub(crate) fn remove_provider_auth(env: &OpenCodeEnv, provider_id: &str) -> Result<bool, String> {
    if provider_id.is_empty() {
        return Err("Provider ID is required".to_string());
    }
    let mut auth = read_auth_file(env)?;
    let present = auth.get(provider_id).map(js_truthy).unwrap_or(false);
    if !present {
        tracing::info!("Provider {provider_id} not found in auth file, nothing to remove");
        return Ok(false);
    }
    if let Value::Object(map) = &mut auth {
        map.remove(provider_id);
    }
    write_auth_file(env, &auth)?;
    tracing::info!("Removed provider auth: {provider_id}");
    Ok(true)
}

/// `getProviderAuth`: `auth[providerId] || null` — a stored falsy JSON
/// scalar also reads as absent.
pub(crate) fn get_provider_auth(
    env: &OpenCodeEnv,
    provider_id: &str,
) -> Result<Option<Value>, String> {
    let auth = read_auth_file(env)?;
    let value = auth.get(provider_id).cloned().unwrap_or(Value::Null);
    if !js_truthy(&value) {
        return Ok(None);
    }
    Ok(Some(value))
}
