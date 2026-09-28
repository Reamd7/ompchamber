//! Port of `server/lib/github/auth.js` — GitHub auth storage, multi-account
//! support, client id / scope resolution, gh-CLI flags in settings.json.
//!
//! Storage: `<data_dir>/github-auth.json` where data_dir is
//! `OMPCHAMBER_DATA_DIR` or `~/.config/ompchamber` (the server's resolved
//! data dir, threaded in via `RouterContext::config`). Writes are atomic
//! (tmp + rename) with file mode 0600 (best-effort). Client id/scope
//! resolution: env → settings.json → built-in default.
//!
//! 中文概述：GitHub OAuth 凭据的本地存储与多账号管理（github-auth.json），
//! 外加 OAuth client id/scope 解析与 gh-CLI 登录开关（settings.json）。
//! 凭据写入均为“临时文件 + rename”的原子替换，并尽力设置 0600 权限。

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::github::rate_limit::now_ms;

/// gh-CLI 登录在多账号列表中使用的固定账号 id。
pub const GH_CLI_ACCOUNT_ID: &str = "gh-cli";
/// 内置默认的 GitHub OAuth App client id（env 与 settings 均未配置时兜底）。
const DEFAULT_GITHUB_CLIENT_ID: &str = "Ov23lizomPOC3eFYo56r";
/// 内置默认的 OAuth scope 列表。
const DEFAULT_GITHUB_SCOPES: &str = "repo read:org workflow read:user user:email";

/// 与凭据关联的 GitHub 用户信息（登录名、头像、id、姓名、邮箱）。
#[derive(Debug, Clone, PartialEq)]
pub struct AuthUser {
/// 用户登录名。
    pub login: Option<String>,
/// 头像 URL。
    pub avatar_url: Option<String>,
/// GitHub 数字 id。
    pub id: Option<i64>,
/// 显示姓名。
    pub name: Option<String>,
/// 公开邮箱。
    pub email: Option<String>,
}

/// 用户的 JSON 序列化与反序列化。
impl AuthUser {
    /// `normalizeAuthEntry`'s user shape: every field present, null-filled.
/// 序列化为前端用户形状：所有字段都出现（缺失以 null 填充）。
    pub(crate) fn to_json_null_filled(&self) -> Value {
        json!({
            "login": self.login,
            "avatarUrl": self.avatar_url,
            "id": self.id,
            "name": self.name,
            "email": self.email,
        })
    }

/// 从 JSON 对象还原用户；输入非对象返回 `None`。
    fn from_json(value: &Value) -> Option<AuthUser> {
        let obj = value.as_object()?;
        Some(AuthUser {
            login: string_field(obj, "login"),
            avatar_url: string_field(obj, "avatarUrl"),
            id: obj.get("id").and_then(Value::as_i64),
            name: string_field(obj, "name"),
            email: string_field(obj, "email"),
        })
    }
}

/// 取对象里的字符串字段；非字符串或缺失返回 `None`。
fn string_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

/// 一条已保存的 GitHub 登录凭据（token、scope、用户、是否当前账号）。
#[derive(Debug, Clone, PartialEq)]
pub struct AuthEntry {
/// OAuth access token。
    pub access_token: String,
/// 授权 scope 串。
    pub scope: String,
/// token 类型（默认 bearer）。
    pub token_type: String,
/// 创建时间（毫秒时间戳）。
    pub created_at: Option<i64>,
/// 关联的用户信息。
    pub user: Option<AuthUser>,
/// 是否为当前激活账号。
    pub current: bool,
/// 账号唯一 id（显式值或按 user/token 推导）。
    pub account_id: String,
}

/// 条目的存储序列化。
impl AuthEntry {
/// 转成 github-auth.json 使用的 JSON 对象（camelCase 字段名）。
    fn to_json(&self) -> Value {
        json!({
            "accessToken": self.access_token,
            "scope": self.scope,
            "tokenType": self.token_type,
            "createdAt": self.created_at,
            "user": self.user.as_ref().map(AuthUser::to_json_null_filled),
            "current": self.current,
            "accountId": self.account_id,
        })
    }
}

/// Account summary shape returned by `getGitHubAuthAccounts()`.
/// 账号摘要：id、用户、scope 与是否当前。
pub struct AuthAccountSummary {
/// 账号 id。
    pub id: String,
/// 账号用户信息。
    pub user: AuthUser,
/// 授权 scope。
    pub scope: String,
/// 是否当前激活。
    pub current: bool,
}

/// 摘要的接口序列化。
impl AuthAccountSummary {
/// 转成 `getGitHubAuthAccounts()` 返回的 JSON 对象。
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "user": self.user.to_json_null_filled(),
            "scope": self.scope,
            "current": self.current,
        })
    }
}

/// 基于数据目录的 GitHub 凭据存储。
#[derive(Debug)]
pub struct AuthStore {
/// 存储根目录（github-auth.json 与 settings.json 所在目录）。
    data_dir: PathBuf,
}

/// 凭据读写、账号切换与 gh-CLI 设置项。
impl AuthStore {
/// 以指定数据目录构造。
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

/// 返回存储根目录。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

/// 凭据文件完整路径（`<data_dir>/github-auth.json`）。
    fn storage_file(&self) -> PathBuf {
        self.data_dir.join("github-auth.json")
    }

/// 设置文件完整路径（`<data_dir>/settings.json`）。
    fn settings_file(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }

/// 确保数据目录存在（幂等创建）。
    fn ensure_storage_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.data_dir)
    }

    /// `readJsonFile`: missing/blank/non-object/undecodable → None.
/// 读取并解析凭据文件：文件缺失、空白、非法 JSON 或非对象/数组均返回 `None`。
    fn read_json_file(&self) -> Option<Value> {
        let _ = self.ensure_storage_dir();
        let path = self.storage_file();
        if !path.exists() {
            return None;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::error!("Failed to read GitHub auth file: {error}");
                return None;
            }
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(parsed) if parsed.is_object() || parsed.is_array() => Some(parsed),
            Ok(_) => None,
            Err(error) => {
                tracing::error!("Failed to read GitHub auth file: {error}");
                None
            }
        }
    }

    /// `writeJsonFile`: atomic tmp+rename, 2-space JSON, mode 0600.
/// 原子写入凭据文件：先写唯一 tmp 文件再 rename 覆盖，尽力设置 0600 权限。
    fn write_json_file(&self, payload: &Value) -> std::io::Result<()> {
        self.ensure_storage_dir()?;
        let target = self.storage_file();
        let tmp_file = self.data_dir.join(format!(
            "github-auth.json.{}.{}.tmp",
            std::process::id(),
            now_ms()
        ));
        let body = serde_json::to_string_pretty(payload).unwrap_or_default();
        std::fs::write(&tmp_file, body)?;
        let _ = set_mode_600(&tmp_file);
        std::fs::rename(&tmp_file, &target)?;
        let _ = set_mode_600(&target);
        Ok(())
    }

/// 读取 settings.json；缺失或解析失败一律返回空对象。
    fn read_settings_file(&self) -> Value {
        let path = self.settings_file();
        if !path.exists() {
            return json!({});
        }
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<Value>(&raw).unwrap_or_else(|_| json!({})),
            Err(_) => json!({}),
        }
    }

    /// `writeSettingsFile`: same atomic shape as the auth file.
/// 与凭据文件相同的原子写入方式保存 settings.json。
    fn write_settings_file(&self, settings: &Value) -> std::io::Result<()> {
        self.ensure_storage_dir()?;
        let target = self.settings_file();
        let tmp_file = self.data_dir.join(format!(
            "settings.json.{}.{}.tmp",
            std::process::id(),
            now_ms()
        ));
        let body = serde_json::to_string_pretty(settings).unwrap_or_default();
        std::fs::write(&tmp_file, body)?;
        let _ = set_mode_600(&tmp_file);
        std::fs::rename(&tmp_file, &target)?;
        let _ = set_mode_600(&target);
        Ok(())
    }

    /// `resolveAccountId`: explicit → user.login → user.id → token prefix.
/// 账号 id 推导顺序：显式 accountId → user.login → user.id → `token:` + token 前 8 字符。
    fn resolve_account_id(user: Option<&AuthUser>, access_token: &str, account_id: &str) -> String {
        if !account_id.trim().is_empty() {
            return account_id.trim().to_string();
        }
        if let Some(user) = user {
            if let Some(login) = user.login.as_deref()
                && !login.trim().is_empty()
            {
                return login.trim().to_string();
            }
            if let Some(id) = user.id {
                return id.to_string();
            }
        }
        if !access_token.trim().is_empty() {
            let prefix: String = access_token.chars().take(8).collect();
            return format!("token:{prefix}");
        }
        String::new()
    }

    /// `normalizeAuthEntry`.
/// 校验并补全单条凭据：无 accessToken 的条目丢弃；
/// 补默认 tokenType（bearer）、current（false）与 accountId。
    fn normalize_auth_entry(entry: &Value) -> Option<AuthEntry> {
        let obj = entry.as_object()?;
        let access_token = obj.get("accessToken").and_then(Value::as_str).unwrap_or("");
        if access_token.is_empty() {
            return None;
        }
        let user = obj
            .get("user")
            .filter(|u| u.is_object())
            .and_then(AuthUser::from_json);
        let account_id = Self::resolve_account_id(
            user.as_ref(),
            access_token,
            obj.get("accountId").and_then(Value::as_str).unwrap_or(""),
        );
        Some(AuthEntry {
            access_token: access_token.to_string(),
            scope: obj
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            token_type: obj
                .get("tokenType")
                .and_then(Value::as_str)
                .unwrap_or("bearer")
                .to_string(),
            created_at: obj.get("createdAt").and_then(Value::as_i64),
            user,
            current: obj.get("current").and_then(Value::as_bool).unwrap_or(false),
            account_id,
        })
    }

    /// `normalizeAuthList`: dedupe `current`, default the first entry, fill
    /// missing account ids. Returns whether anything changed.
/// 归一化整份列表：仅保留首个 current（其余清零，无 current 时默认第一条），
/// 回填缺失的账号 id；返回 (列表, 归一化是否改变了内容)。
    fn normalize_auth_list(raw: Value) -> (Vec<AuthEntry>, bool) {
        let candidates: Vec<Value> = if let Some(items) = raw.as_array() {
            items.clone()
        } else {
            vec![raw]
        };
        let mut list: Vec<AuthEntry> = candidates
            .iter()
            .filter_map(Self::normalize_auth_entry)
            .collect();

        if list.is_empty() {
            return (Vec::new(), false);
        }

        let mut changed = false;
        let mut current_found = false;
        for entry in list.iter_mut() {
            if entry.current && !current_found {
                current_found = true;
            } else if entry.current && current_found {
                entry.current = false;
                changed = true;
            }
        }
        if !current_found && !list.is_empty() {
            list[0].current = true;
            changed = true;
        }
        for entry in list.iter_mut() {
            if entry.account_id.is_empty() {
                entry.account_id =
                    Self::resolve_account_id(entry.user.as_ref(), &entry.access_token, "");
                changed = true;
            }
        }
        (list, changed)
    }

/// 凭据列表序列化为 JSON 数组。
    fn entries_to_json(list: &[AuthEntry]) -> Value {
        Value::Array(list.iter().map(AuthEntry::to_json).collect())
    }

    /// `readAuthList`: read + normalize; writes back when normalization
    /// changed anything.
/// 读取 + 归一化；归一化改动了内容时顺手把结果写回文件。
    pub fn read_auth_list(&self) -> Vec<AuthEntry> {
        let Some(data) = self.read_json_file() else {
            return Vec::new();
        };
        let (list, changed) = Self::normalize_auth_list(data);
        if changed {
            let _ = self.write_json_file(&Self::entries_to_json(&list));
        }
        list
    }

/// 整体覆盖写入凭据列表（忽略 IO 错误）。
    fn write_auth_list(&self, list: &[AuthEntry]) {
        let _ = self.write_json_file(&Self::entries_to_json(list));
    }

    /// `getGitHubAuth`.
/// 取当前账号（current 标记，缺失时取第一条）；无有效 token 返回 `None`。
    pub fn get_github_auth(&self) -> Option<AuthEntry> {
        let list = self.read_auth_list();
        let current = list.iter().find(|e| e.current).or_else(|| list.first())?;
        if current.access_token.is_empty() {
            return None;
        }
        Some(current.clone())
    }

    /// `getGitHubAuthAccounts`.
/// 列出全部账号摘要；无用户信息或缺账号 id 的条目被跳过。
    pub fn get_github_auth_accounts(&self) -> Vec<AuthAccountSummary> {
        self.read_auth_list()
            .into_iter()
            .filter_map(|entry| {
                let user = entry.user?;
                if entry.account_id.is_empty() {
                    return None;
                }
                Some(AuthAccountSummary {
                    id: entry.account_id,
                    user,
                    scope: entry.scope,
                    current: entry.current,
                })
            })
            .collect()
    }

    /// `clearGitHubAuth`: remove the current account; delete the file when it
    /// was the only one. Mirrors the JS try/catch (io failures → `false`).
/// 删除当前账号；IO 失败仅记录日志并返回 false。
    pub fn clear_github_auth(&self) -> bool {
        match self.clear_github_auth_inner() {
            Ok(()) => true,
            Err(error) => {
                tracing::error!("Failed to clear GitHub auth file: {error}");
                false
            }
        }
    }

/// 清除逻辑本体：仅剩当前账号时直接删除文件，
/// 否则改写剩余列表并把第一条设为 current。
    fn clear_github_auth_inner(&self) -> std::io::Result<()> {
        let list = self.read_auth_list();
        if list.is_empty() {
            return Ok(());
        }
        let mut remaining: Vec<AuthEntry> = list.into_iter().filter(|e| !e.current).collect();
        if remaining.is_empty() {
            let path = self.storage_file();
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            return Ok(());
        }
        for (index, entry) in remaining.iter_mut().enumerate() {
            entry.current = index == 0;
        }
        self.write_auth_list(&remaining);
        Ok(())
    }

    /// `setGitHubAuth`. Throws `accessToken is required` in JS — surfaced as
    /// `Err` here.
/// 新增/更新账号并设为当前（同 accountId 原位替换）；空 token 报错。
    pub fn set_github_auth(
        &self,
        access_token: &str,
        scope: &str,
        token_type: &str,
        user: Option<AuthUser>,
        account_id: Option<&str>,
    ) -> Result<AuthEntry, String> {
        if access_token.is_empty() {
            return Err("accessToken is required".to_string());
        }
        let resolved_account_id =
            Self::resolve_account_id(user.as_ref(), access_token, account_id.unwrap_or(""));
        let mut list = self.read_auth_list();
        let existing_index = list
            .iter()
            .position(|entry| entry.account_id == resolved_account_id);
        let next_entry = AuthEntry {
            access_token: access_token.to_string(),
            scope: scope.to_string(),
            token_type: if token_type.is_empty() {
                "bearer".to_string()
            } else {
                token_type.to_string()
            },
            created_at: Some(now_ms()),
            user: user.clone(),
            current: true,
            account_id: resolved_account_id.clone(),
        };
        let target_index = match existing_index {
            Some(index) => {
                list[index] = next_entry.clone();
                index
            }
            None => {
                list.push(next_entry.clone());
                list.len() - 1
            }
        };
        for (index, entry) in list.iter_mut().enumerate() {
            entry.current = index == target_index;
        }
        self.write_auth_list(&list);
        Ok(next_entry)
    }

    /// `activateGitHubAuth` (includes the `setGhCliActive(false)` side effect).
/// 按 id 切换当前账号，同时停用 gh-CLI 登录；找不到或空 id 返回 false。
    pub fn activate_github_auth(&self, account_id: &str) -> bool {
        let trimmed = account_id.trim();
        if trimmed.is_empty() {
            return false;
        }
        let mut list = self.read_auth_list();
        let index = list.iter().position(|entry| entry.account_id == trimmed);
        let Some(index) = index else {
            return false;
        };
        self.set_gh_cli_active(false);
        for (idx, entry) in list.iter_mut().enumerate() {
            entry.current = idx == index;
        }
        self.write_auth_list(&list);
        true
    }

    /// `getGitHubClientId`: env → settings.json → default.
/// OAuth client id 解析：环境变量 → settings.json → 内置默认。
    pub fn get_github_client_id(&self) -> String {
        if let Ok(raw) = std::env::var("OMPCHAMBER_GITHUB_CLIENT_ID") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        if let Some(stored) = self
            .read_settings_file()
            .get("githubClientId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return stored.to_string();
        }
        DEFAULT_GITHUB_CLIENT_ID.to_string()
    }

    /// `getGitHubScopes`: env → settings.json → default.
/// OAuth scope 解析：环境变量 → settings.json → 内置默认。
    pub fn get_github_scopes(&self) -> String {
        if let Ok(raw) = std::env::var("OMPCHAMBER_GITHUB_SCOPES") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        if let Some(stored) = self
            .read_settings_file()
            .get("githubScopes")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            return stored.to_string();
        }
        DEFAULT_GITHUB_SCOPES.to_string()
    }

/// settings.json 中的 ghCliDisabled 标志（默认未禁用）。
    pub fn is_gh_cli_disabled(&self) -> bool {
        self.read_settings_file()
            .get("ghCliDisabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

/// 设置禁用标志；置为禁用时会同时把 ghCliActive 清为 false。
    pub fn set_gh_cli_disabled(&self, disabled: bool) {
        let mut settings = self.read_settings_file();
        if !settings.is_object() {
            settings = json!({});
        }
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("ghCliDisabled".to_string(), Value::Bool(disabled));
            if disabled {
                obj.insert("ghCliActive".to_string(), Value::Bool(false));
            }
        }
        let _ = self.write_settings_file(&settings);
    }

/// gh-CLI 登录是否激活：未被禁用且 ghCliActive 为 true。
    pub fn is_gh_cli_active(&self) -> bool {
        let settings = self.read_settings_file();
        !self.is_gh_cli_disabled()
            && settings
                .get("ghCliActive")
                .and_then(Value::as_bool)
                .unwrap_or(false)
    }

/// 设置激活标志；禁用状态下强制写入 false。
    pub fn set_gh_cli_active(&self, active: bool) {
        let mut settings = self.read_settings_file();
        if !settings.is_object() {
            settings = json!({});
        }
        let disabled = self.is_gh_cli_disabled();
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("ghCliActive".to_string(), Value::Bool(active && !disabled));
        }
        let _ = self.write_settings_file(&settings);
    }
}

/// Unix 下把文件权限收紧为 0600（仅属主可读写）。
#[cfg(unix)]
fn set_mode_600(path: &Path) -> std::io::Result<()> {
use crate::os_compat::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// 非 Unix 平台无对应权限概念，直接返回成功。
#[cfg(not(unix))]
fn set_mode_600(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// 凭据存储单元测试：多账号切换、原子写与 settings 回退均对齐 JS 行为。
#[cfg(test)]
mod tests {
    use super::*;

/// 建立独立的临时数据目录构造 AuthStore。
    fn temp_store(tag: &str) -> (AuthStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-ghauth-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (AuthStore::new(dir.clone()), dir)
    }

/// 造一个字段齐全的测试用户。
    fn user(login: &str) -> AuthUser {
        AuthUser {
            login: Some(login.to_string()),
            avatar_url: Some(format!("https://avatars/{login}.png")),
            id: Some(42),
            name: Some(login.to_string()),
            email: Some(format!("{login}@example.com")),
        }
    }

/// 验证：无凭据文件时既无当前 auth 也无账号列表。
    #[test]
    fn missing_file_yields_no_auth() {
        let (store, _dir) = temp_store("missing");
        assert!(store.get_github_auth().is_none());
        assert!(store.get_github_auth_accounts().is_empty());
    }

/// 验证：写入后可完整读回 token/scope/user，账号 id 取 login。
    #[test]
    fn roundtrip_persists_and_reads_back() {
        let (store, _dir) = temp_store("roundtrip");
        let entry = store
            .set_github_auth(
                "tok_abcdef123456",
                "repo",
                "bearer",
                Some(user("octocat")),
                None,
            )
            .unwrap();
        assert_eq!(entry.account_id, "octocat");
        assert!(entry.current);

        let read = store.get_github_auth().unwrap();
        assert_eq!(read.access_token, "tok_abcdef123456");
        assert_eq!(read.scope, "repo");
        assert_eq!(read.token_type, "bearer");
        assert_eq!(
            read.user.as_ref().unwrap().login.as_deref(),
            Some("octocat")
        );
        assert_eq!(read.account_id, "octocat");

        let accounts = store.get_github_auth_accounts();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, "octocat");
        assert!(accounts[0].current);
    }

/// 验证：落盘为两空格缩进 JSON、0600 权限且无 tmp 残留。
    #[test]
    fn file_is_written_with_two_space_json_and_mode_600() {
        let (store, dir) = temp_store("format");
        store
            .set_github_auth(
                "tok_abcdef123456",
                "repo",
                "bearer",
                Some(user("octocat")),
                None,
            )
            .unwrap();
        let raw = std::fs::read_to_string(dir.join("github-auth.json")).unwrap();
        assert!(raw.contains("\n    \"accessToken\": \"tok_abcdef123456\""));
        #[cfg(unix)]
        {
use crate::os_compat::PermissionsExt;
            let mode = std::fs::metadata(dir.join("github-auth.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // No leftover tmp files after atomic rename.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

/// 验证：空 accessToken 报 "accessToken is required"。
    #[test]
    fn set_requires_access_token() {
        let (store, _dir) = temp_store("required");
        assert_eq!(
            store
                .set_github_auth("", "repo", "bearer", None, None)
                .unwrap_err(),
            "accessToken is required"
        );
    }

/// 验证：写入第二个账号后 current 自动切换到它。
    #[test]
    fn second_account_switches_current_on_set() {
        let (store, _dir) = temp_store("multi");
        store
            .set_github_auth("token1111aaaa", "repo", "bearer", Some(user("first")), None)
            .unwrap();
        store
            .set_github_auth(
                "token2222bbbb",
                "repo",
                "bearer",
                Some(user("second")),
                None,
            )
            .unwrap();
        let list = store.read_auth_list();
        assert_eq!(list.len(), 2);
        assert!(!list[0].current);
        assert!(list[1].current);
        assert_eq!(store.get_github_auth().unwrap().account_id, "second");
    }

/// 验证：更新已有账号时原位替换、不新增条目并成为 current。
    #[test]
    fn updating_existing_account_keeps_position_and_makes_current() {
        let (store, _dir) = temp_store("update");
        store
            .set_github_auth("token1111aaaa", "repo", "bearer", Some(user("first")), None)
            .unwrap();
        store
            .set_github_auth(
                "token2222bbbb",
                "repo",
                "bearer",
                Some(user("second")),
                None,
            )
            .unwrap();
        store
            .set_github_auth(
                "token3333cccc",
                "repo:status",
                "bearer",
                Some(user("first")),
                None,
            )
            .unwrap();
        let current = store.get_github_auth().unwrap();
        assert_eq!(current.account_id, "first");
        assert_eq!(current.access_token, "token3333cccc");
        assert_eq!(store.read_auth_list().len(), 2);
    }

/// 验证：activate 按 id 切换 current；未知 id 与空白 id 返回 false。
    #[test]
    fn activate_switches_current_and_flags() {
        let (store, _dir) = temp_store("activate");
        store
            .set_github_auth("token1111aaaa", "repo", "bearer", Some(user("first")), None)
            .unwrap();
        store
            .set_github_auth(
                "token2222bbbb",
                "repo",
                "bearer",
                Some(user("second")),
                None,
            )
            .unwrap();
        assert!(store.activate_github_auth("first"));
        assert_eq!(store.get_github_auth().unwrap().account_id, "first");
        assert!(!store.activate_github_auth("nobody"));
        assert!(!store.activate_github_auth("  "));
    }

/// 验证：clear 只删当前账号；删到最后一条时整个文件被移除。
    #[test]
    fn clear_removes_only_current_and_deletes_file_when_last() {
        let (store, dir) = temp_store("clear");
        store
            .set_github_auth("token1111aaaa", "repo", "bearer", Some(user("first")), None)
            .unwrap();
        store
            .set_github_auth(
                "token2222bbbb",
                "repo",
                "bearer",
                Some(user("second")),
                None,
            )
            .unwrap();
        assert!(store.clear_github_auth());
        assert_eq!(store.read_auth_list().len(), 1);
        assert_eq!(store.get_github_auth().unwrap().account_id, "first");
        assert!(store.clear_github_auth());
        assert!(!dir.join("github-auth.json").exists());
        assert!(store.get_github_auth().is_none());
    }

/// 验证：normalize 回填账号 id/唯一 current，且仅在有变化时回写文件。
    #[test]
    fn normalize_backfills_account_id_and_single_current() {
        let (store, dir) = temp_store("normalize");
        // Raw file with one current flag and no account ids: entry-level
        // normalization already resolves ids, so nothing changes and the file
        // is not rewritten (JS `changed` stays false).
        let raw_input =
            r#"[{"accessToken":"tokenAAAA1111"},{"accessToken":"tokenBBBB2222","current":true}]"#;
        std::fs::write(dir.join("github-auth.json"), raw_input).unwrap();
        let list = store.read_auth_list();
        assert_eq!(list.len(), 2);
        assert!(!list[0].current);
        assert!(list[1].current);
        assert_eq!(list[0].account_id, "token:tokenAAA");
        assert_eq!(
            std::fs::read_to_string(dir.join("github-auth.json")).unwrap(),
            raw_input
        );

        // No current flag at all → first entry becomes current and the file
        // IS written back.
        let (store, dir) = temp_store("normalize2");
        std::fs::write(
            dir.join("github-auth.json"),
            r#"[{"accessToken":"tokenAAAA1111"},{"accessToken":"tokenBBBB2222"}]"#,
        )
        .unwrap();
        let list = store.read_auth_list();
        assert!(list[0].current);
        assert!(!list[1].current);
        let raw = std::fs::read_to_string(dir.join("github-auth.json")).unwrap();
        assert!(raw.contains("\"current\": true"));
    }

/// 验证：client id/scope 优先取 settings.json，缺失时回退内置默认。
    #[test]
    fn client_id_and_scopes_resolve_settings_then_default() {
        let (store, dir) = temp_store("clientid");
        std::fs::write(
            dir.join("settings.json"),
            r#"{"githubClientId":"from-settings","githubScopes":"from settings"}"#,
        )
        .unwrap();
        assert_eq!(store.get_github_client_id(), "from-settings");
        assert_eq!(store.get_github_scopes(), "from settings");
        std::fs::remove_file(dir.join("settings.json")).unwrap();
        assert_eq!(store.get_github_client_id(), DEFAULT_GITHUB_CLIENT_ID);
        assert_eq!(store.get_github_scopes(), DEFAULT_GITHUB_SCOPES);
    }

/// 验证：gh-CLI 禁用/激活标志经 settings.json 往返、禁用优先于激活，
/// 且 activate 账号会停用 gh-CLI。
    #[test]
    fn gh_cli_flags_roundtrip_through_settings() {
        let (store, dir) = temp_store("ghcliflags");
        assert!(!store.is_gh_cli_disabled());
        store.set_gh_cli_disabled(true);
        assert!(store.is_gh_cli_disabled());
        // Disabling also deactivates.
        assert!(!store.is_gh_cli_active());
        let raw = std::fs::read_to_string(dir.join("settings.json")).unwrap();
        assert!(raw.contains("\"ghCliDisabled\": true"));
        assert!(raw.contains("\"ghCliActive\": false"));
        // Cannot activate while disabled.
        store.set_gh_cli_active(true);
        assert!(!store.is_gh_cli_active());
        store.set_gh_cli_disabled(false);
        store.set_gh_cli_active(true);
        assert!(store.is_gh_cli_active());
        // activate_github_auth deactivates gh CLI.
        store
            .set_github_auth("token1111aaaa", "repo", "bearer", Some(user("first")), None)
            .unwrap();
        assert!(store.is_gh_cli_active());
        assert!(store.activate_github_auth("first"));
        assert!(!store.is_gh_cli_active());
    }
}
