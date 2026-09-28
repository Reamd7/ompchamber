//! Port of `opencode/auth.js`: the OpenCode `auth.json` store under
//! `~/.local/share/opencode` — read, defensive write (0700/0600 perms,
//! `.ompchamber.backup`), provider auth lookup/removal. The file's contents
//! are credentials and are never logged.
//!
//! 中文说明：管理 OpenCode 的 auth.json 凭据存储（位于
//! `~/.local/share/opencode`）。读取时对缺失或空白文件容错返回空对象；
//! 写入前先复制 `.ompchamber.backup` 备份并收紧权限（目录 0700、
//! 文件/备份 0600，仅 Unix）；另提供按 provider 的凭据查询与移除。
//! 文件内容为凭据，任何日志都不得包含其载荷。

use crate::os_compat::PermissionsExt;
use std::path::PathBuf;

use serde_json::{Map, Value};

use super::OpenCodeEnv;
use super::webutil::js_truthy;

/// 中文：auth.json 的绝对路径（数据目录下的 `auth.json`）。
pub(crate) fn auth_file(env: &OpenCodeEnv) -> PathBuf {
    env.data_dir.join("auth.json")
}

/// `readAuthFile`.
/// 中文：读取 auth.json 并解析为 JSON。文件不存在或内容为空白时返回
/// 空对象而非报错；读取失败与解析失败都会记录 error 日志（不含内容）
/// 并返回统一的错误文案。
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
/// 中文：防御性写入 auth.json：必要时先创建数据目录（Unix 下置 0700），
/// 旧文件先复制为 `.ompchamber.backup`（0600），再以 pretty JSON 覆写
/// 并把新文件权限设为 0600。任何失败路径都返回统一错误文案，且日志
/// 只记录路径与结果、绝不记录载荷。
pub(crate) fn write_auth_file(env: &OpenCodeEnv, auth: &Value) -> Result<(), String> {
// 局部辅助：把任意写入阶段错误折叠为统一文案（JS 内联的字符串映射）。
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

/// 中文：备份文件路径 = 原路径加 `.ompchamber.backup` 后缀。
fn backup_path(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".ompchamber.backup");
    PathBuf::from(name)
}

/// `removeProviderAuth` — JS truthiness on the stored entry decides whether
/// anything is there to remove.
/// 中文：移除指定 provider 的凭据条目。空 ID 直接报错；按 JS 真值语义
/// 判断条目是否存在（假值视为不存在并返回 `Ok(false)`），存在则删除后
/// 回写文件并返回 `Ok(true)`。
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
/// 中文：查询指定 provider 的凭据。等价于 JS 的 `auth[providerId] || null`：
/// 条目缺失或为假值（false/0/""/null）时一律返回 `None`。
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
