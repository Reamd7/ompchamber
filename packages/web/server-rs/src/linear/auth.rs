//! Port of `server/lib/linear/auth.js` — Linear token storage
//! (`linear-auth.json`), the settings-backed client/broker configuration, and
//! the session-comments preference. The data dir is resolved exactly like the
//! JS module (`OPENCHAMBER_DATA_DIR` or `~/.config/openchamber`), which is
//! intentionally independent of the server's own `OMPCHAMBER_DATA_DIR`.
//! 中文说明：移植自 JS 版 auth.js。管理多 workspace 的 Linear OAuth 凭据
//! （`linear-auth.json` 的读取/合并/激活/清除），以及基于 settings.json 与
//! 环境变量的 client/broker 配置读取和会话状态评论开关。数据目录解析与
//! JS 版一致（`OPENCHAMBER_DATA_DIR` 或 `~/.config/openchamber`），与
//! 服务器自身的 `OMPCHAMBER_DATA_DIR` 刻意独立。

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::LinearState;
use super::parse::{
    is_plain_object, js_truthy, num, read_finite_number, read_trimmed_string, truthy_num,
};

/// 默认 Linear OAuth client id（可被 `OPENCHAMBER_LINEAR_CLIENT_ID` 或
/// settings.json 的 `linearClientId` 覆盖）。
pub const DEFAULT_LINEAR_CLIENT_ID: &str = "91bbe26a69a2c8568d3683f1e01e776c";
/// 默认 OAuth scope：读、写与创建评论。
const DEFAULT_LINEAR_SCOPES: &str = "read,write,comments:create";
/// 默认公开回调 broker 地址（OpenChamber 生产 API）。
const DEFAULT_LINEAR_BROKER_URL: &str = "https://api.openchamber.dev/v1/oauth/linear";
/// access token 刷新提前量（毫秒）：距到期不足 2 分钟即视为过期、需要刷新。
pub const ACCESS_TOKEN_REFRESH_SKEW_MS: f64 = 2.0 * 60_000.0;
/// 无法从 organization/user 推导 workspace 标识时的兜底值，
/// 兼容旧版单 workspace 存储格式。
const LEGACY_WORKSPACE_ID: &str = "legacy";
/// settings.json 中会话状态评论开关的键名。
const SESSION_COMMENTS_SETTING_KEY: &str = "linearSessionComments";

/// Linear 用户摘要（认证条目里携带的身份信息）。
#[derive(Debug, Clone, PartialEq)]
pub struct LinearUser {
    /// 用户 UUID（必填）。
    pub id: String,
    /// 用户全名。
    pub name: Option<String>,
    /// 用户显示名。
    pub display_name: Option<String>,
    /// 用户邮箱。
    pub email: Option<String>,
    /// 用户头像 URL。
    pub avatar_url: Option<String>,
}

/// 序列化为 JS 版一致的 camelCase 用户 JSON。
impl LinearUser {
    /// 生成用户 JSON（camelCase 键，`None` 字段为 null）。
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "displayName": self.display_name,
            "email": self.email,
            "avatarUrl": self.avatar_url,
        })
    }
}

/// Linear 组织（workspace）摘要。
#[derive(Debug, Clone, PartialEq)]
pub struct LinearOrganization {
    /// 组织 UUID（必填）。
    pub id: String,
    /// 组织名称（必填）。
    pub name: String,
    /// 组织 URL key（子域名）。
    pub url_key: Option<String>,
}

/// 序列化为 `{ id, name, urlKey }`。
impl LinearOrganization {
    /// 生成组织 JSON（`id`/`name`/`urlKey`）。
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "urlKey": self.url_key,
        })
    }
}

/// 单个 workspace 的 Linear 凭据条目（`linear-auth.json` 中 `workspaces`
/// 数组的一项）。
#[derive(Debug, Clone, PartialEq)]
pub struct LinearAuthEntry {
    /// Linear access token（必填，为空则整条被丢弃）。
    pub access_token: String,
    /// 用于刷新的 refresh token，公开客户端流程可能没有。
    pub refresh_token: Option<String>,
    /// token 类型，默认 `bearer`。
    pub token_type: String,
    /// access token 过期时间戳（毫秒），缺失视为已过期。
    pub expires_at: Option<f64>,
    /// 授权 scope 列表（逗号分隔）。
    pub scope: String,
    /// 条目创建时间戳（毫秒）。
    pub created_at: Option<f64>,
    /// 最近一次授权时间戳（毫秒），缺失时回退 created_at。
    pub authorized_at: Option<f64>,
    /// 授权用户信息。
    pub user: Option<LinearUser>,
    /// 授权组织信息。
    pub organization: Option<LinearOrganization>,
    /// 是否为当前激活 workspace（全列表最多一个为 true）。
    pub current: bool,
    /// workspace 标识：组织 UUID、`user:<用户id>` 或 `legacy`。
    pub workspace_id: String,
}

/// 序列化为 JS 版一致的单 workspace 存储格式。
impl LinearAuthEntry {
    /// 生成单条 workspace 的存储格式 JSON。
    pub fn to_json(&self) -> Value {
        json!({
            "accessToken": self.access_token,
            "refreshToken": self.refresh_token,
            "tokenType": self.token_type,
            "expiresAt": self.expires_at.map(num),
            "scope": self.scope,
            "createdAt": self.created_at.map(num),
            "authorizedAt": self.authorized_at.map(num),
            "user": self.user.as_ref().map(LinearUser::to_json),
            "organization": self.organization.as_ref().map(LinearOrganization::to_json),
            "current": self.current,
            "workspaceId": self.workspace_id,
        })
    }
}

/// 暴露给前端的 workspace 概要（不含任何 token）。
#[derive(Debug, Clone)]
pub struct WorkspaceSummary {
    /// workspace 标识。
    pub id: String,
    /// 组织名称（未知时为 `None`）。
    pub name: Option<String>,
    /// 组织 URL key。
    pub url_key: Option<String>,
    /// 是否为当前激活 workspace。
    pub current: bool,
    /// 授权用户信息。
    pub user: Option<LinearUser>,
    /// 授权时间戳（毫秒），缺失回退创建时间。
    pub authorized_at: Option<f64>,
}

/// 序列化为不含敏感信息的 workspace JSON。
impl WorkspaceSummary {
    /// 生成不含敏感信息的 workspace 概要 JSON。
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "urlKey": self.url_key,
            "current": self.current,
            "user": self.user.as_ref().map(LinearUser::to_json),
            "authorizedAt": self.authorized_at.map(num),
        })
    }
}

/// Input for `setLinearAuth`. `None` fields mirror absent properties; nested
/// `Option`s distinguish "present but null/invalid" (drops the stored value)
/// from "absent" (keeps it), matching `Object.prototype.hasOwnProperty`.
///
/// 中文说明：三层 `Option` 区分“属性缺失”（保留旧值）与“属性存在但为
/// null/非法”（清除旧值），复刻 JS 的 `hasOwnProperty` 合并语义。
#[derive(Debug, Clone, Default)]
pub struct SetLinearAuthInput {
    /// 新 access token（唯一必填项，空串报错）。
    pub access_token: String,
    /// 嵌套 Option：外层表示属性是否存在，内层 `None` 表示清除旧 refresh token。
    pub refresh_token: Option<Option<String>>,
    /// token 类型；缺省时沿用旧值或 `bearer`。
    pub token_type: Option<String>,
    /// 过期时间戳（毫秒）；缺失沿用旧值。
    pub expires_at: Option<f64>,
    /// scope；空值沿用旧值。
    pub scope: Option<String>,
    /// 嵌套 Option：外层表示属性是否存在，内层 `None` 表示清除旧用户。
    pub user: Option<Option<LinearUser>>,
    /// 嵌套 Option：外层表示属性是否存在，内层 `None` 表示清除旧组织。
    pub organization: Option<Option<LinearOrganization>>,
    /// 显式 workspace 标识；缺省时按 organization/user 推导。
    pub workspace_id: Option<String>,
}

/// JS `resolveDataDir`.
///
/// 中文说明：优先取 `OPENCHAMBER_DATA_DIR`（相对路径按 JS `path.resolve`
/// 锚定到 cwd），否则 `~/.config/openchamber`；取不到 home 目录时退回
/// 相对路径兜底。
pub fn resolve_data_dir() -> PathBuf {
    let from_env = super::env_value_raw("OPENCHAMBER_DATA_DIR");
    if !from_env.is_empty() {
        let path = PathBuf::from(&from_env);
        if path.is_absolute() {
            return path;
        }
        // JS `path.resolve` anchors relative paths at the cwd.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        return cwd.join(path);
    }
    crate::config::home_dir()
        .map(|home| home.join(".config").join("openchamber"))
        .unwrap_or_else(|| PathBuf::from(".config").join("openchamber"))
}

/// 从 JSON 读取用户：非对象或缺 id 返回 `None`，其余字段空串归一为 `None`。
fn normalize_user(value: &Value) -> Option<LinearUser> {
    if !is_plain_object(value) {
        return None;
    }
    let id = read_trimmed_string(&value["id"]);
    if id.is_empty() {
        return None;
    }
    Some(LinearUser {
        id,
        name: trimmed_or_none(&value["name"]),
        display_name: trimmed_or_none(&value["displayName"]),
        email: trimmed_or_none(&value["email"]),
        avatar_url: trimmed_or_none(&value["avatarUrl"]),
    })
}

/// 从 JSON 读取组织：非对象或缺 id/name 返回 `None`，urlKey 空串归一为 `None`。
fn normalize_organization(value: &Value) -> Option<LinearOrganization> {
    if !is_plain_object(value) {
        return None;
    }
    let id = read_trimmed_string(&value["id"]);
    let name = read_trimmed_string(&value["name"]);
    if id.is_empty() || name.is_empty() {
        return None;
    }
    Some(LinearOrganization {
        id,
        name,
        url_key: trimmed_or_none(&value["urlKey"]),
    })
}

/// 去首尾空白后空串视为 `None` 的字符串读取。
fn trimmed_or_none(value: &Value) -> Option<String> {
    let trimmed = read_trimmed_string(value);
    (!trimmed.is_empty()).then_some(trimmed)
}

/// 推导 workspace 标识：显式值优先，其次组织 UUID，再次 `user:<用户id>`，
/// 都没有时回退 `legacy`（旧格式文件兼容）。
fn resolve_linear_workspace_id(
    organization: Option<&LinearOrganization>,
    user: Option<&LinearUser>,
    workspace_id: &str,
) -> String {
    let explicit = workspace_id.trim();
    if !explicit.is_empty() {
        return explicit.to_string();
    }
    if let Some(org) = organization
        && !org.id.is_empty()
    {
        return org.id.clone();
    }
    if let Some(user) = user
        && !user.id.is_empty()
    {
        return format!("user:{}", user.id);
    }
    LEGACY_WORKSPACE_ID.to_string()
}

/// 从 JSON 读取单条认证：缺少 access token 直接丢弃；token_type 默认
/// `bearer`，authorizedAt 缺失时回退 createdAt。
fn normalize_auth_entry(raw: &Value) -> Option<LinearAuthEntry> {
    if !is_plain_object(raw) {
        return None;
    }
    let access_token = read_trimmed_string(&raw["accessToken"]);
    if access_token.is_empty() {
        return None;
    }
    let user = normalize_user(&raw["user"]);
    let organization = normalize_organization(&raw["organization"]);
    let created_at = read_finite_number(&raw["createdAt"]);
    let workspace_id = resolve_linear_workspace_id(
        organization.as_ref(),
        user.as_ref(),
        &read_trimmed_string(&raw["workspaceId"]),
    );
    Some(LinearAuthEntry {
        access_token,
        refresh_token: trimmed_or_none(&raw["refreshToken"]),
        token_type: trimmed_or_none(&raw["tokenType"]).unwrap_or_else(|| "bearer".to_string()),
        expires_at: read_finite_number(&raw["expiresAt"]),
        scope: read_trimmed_string(&raw["scope"]),
        created_at,
        authorized_at: truthy_num(read_finite_number(&raw["authorizedAt"]))
            .or(truthy_num(created_at)),
        user,
        organization,
        current: matches!(raw["current"], Value::Bool(true)),
        workspace_id,
    })
}

/// JS `normalizeAuthList`: returns the deduplicated list plus whether the
/// on-disk shape changed (legacy file, duplicate workspace, or missing
/// `current` flag) and must be rewritten.
///
/// 中文说明：兼容两种磁盘格式——新版 `workspaces` 数组与旧版顶层单条
/// 对象；按 workspace_id 去重保留首条，`current` 标记冲突只留第一个，
/// 全列表无标记时激活第一条。
fn normalize_auth_list(raw: &Value) -> (Vec<LinearAuthEntry>, bool) {
    let source: Vec<&Value> = match raw["workspaces"].as_array() {
        Some(entries) => entries.iter().collect(),
        None => {
            if js_truthy(&raw["accessToken"]) {
                vec![raw]
            } else {
                Vec::new()
            }
        }
    };
    let list: Vec<LinearAuthEntry> = source
        .iter()
        .filter_map(|entry| normalize_auth_entry(entry))
        .collect();

    if list.is_empty() {
        let changed = js_truthy(&raw["accessToken"]) || raw["workspaces"].is_array();
        return (Vec::new(), changed);
    }

    let mut changed = raw["workspaces"].as_array().is_none() && js_truthy(&raw["accessToken"]);
    let mut seen: Vec<String> = Vec::new();
    let mut deduped: Vec<LinearAuthEntry> = Vec::new();
    for entry in list {
        if seen.contains(&entry.workspace_id) {
            changed = true;
            continue;
        }
        seen.push(entry.workspace_id.clone());
        deduped.push(entry);
    }

    let mut current_found = false;
    for entry in deduped.iter_mut() {
        if entry.current && !current_found {
            current_found = true;
        } else if entry.current {
            entry.current = false;
            changed = true;
        }
    }

    if !current_found && let Some(first) = deduped.first_mut() {
        first.current = true;
        changed = true;
    }

    (deduped, changed)
}

/// JS `isLinearAccessTokenStale(expiresAt, now)`.
///
/// 中文说明：过期时间缺失、或距到期不足 2 分钟（`ACCESS_TOKEN_REFRESH_SKEW_MS`）
/// 即视为过期，需要刷新。
pub fn is_access_token_stale(expires_at: Option<f64>, now: f64) -> bool {
    match expires_at {
        Some(expiry) => expiry - ACCESS_TOKEN_REFRESH_SKEW_MS <= now,
        None => true,
    }
}

/// Linear 状态上的认证存储与配置读写实现（文件均位于 `resolve_data_dir` 下）。
impl LinearState {
    /// 认证存储文件路径：数据目录下的 `linear-auth.json`。
    pub fn storage_file(&self) -> PathBuf {
        self.data_dir.join("linear-auth.json")
    }

    /// 设置文件路径：数据目录下的 `settings.json`（与其它组件共享）。
    fn settings_file(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }

    /// JS `readJsonFile` (shared by the auth and settings files).
    ///
    /// 中文说明：文件不存在、为空、读取失败或不是 JSON 对象时一律返回
    /// `None`；读取/解析失败同时打 error 日志（沿用 JS 的统一日志文案）。
    fn read_json_file(path: &Path) -> Option<Value> {
        if !path.exists() {
            return None;
        }
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::error!("Failed to read Linear auth file: {error}");
                return None;
            }
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(parsed) if is_plain_object(&parsed) => Some(parsed),
            Ok(_) => None,
            Err(error) => {
                // JS logs the same "Linear auth file" message for settings.json
                // parse failures (the helper is shared); keep the exact string.
                tracing::error!("Failed to read Linear auth file: {error}");
                None
            }
        }
    }

    /// 读取认证列表并归一化；磁盘格式发生变化（旧格式/重复/缺 current
    /// 标记）时顺手重写回磁盘。
    pub fn read_auth_list(&self) -> Vec<LinearAuthEntry> {
        let Some(data) = Self::read_json_file(&self.storage_file()) else {
            return Vec::new();
        };
        let (list, changed) = normalize_auth_list(&data);
        if changed {
            let _ = self.write_auth_list(&list);
        }
        list
    }

    /// 写入认证列表：空列表直接删除文件，非空时以 `{ workspaces: [...] }`
    /// 格式原子写入（0600 权限）。
    pub fn write_auth_list(&self, list: &[LinearAuthEntry]) -> std::io::Result<()> {
        let file_path = self.storage_file();
        if list.is_empty() {
            if file_path.exists() {
                std::fs::remove_file(&file_path)?;
            }
            return Ok(());
        }
        let payload = json!({
            "workspaces": list.iter().map(LinearAuthEntry::to_json).collect::<Vec<_>>()
        });
        let body = serde_json::to_string_pretty(&payload)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        super::write_file_atomic_600(&file_path, &body)
    }

    /// 读取 settings.json，任何失败都降级为空对象（调用方拿默认值）。
    fn read_settings(&self) -> Value {
        Self::read_json_file(&self.settings_file()).unwrap_or_else(|| json!({}))
    }

    /// 原子写入 settings.json（0600 权限），序列化失败转为 io 错误。
    fn write_settings(&self, settings: Value) -> std::io::Result<()> {
        let body = serde_json::to_string_pretty(&settings)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        super::write_file_atomic_600(&self.settings_file(), &body)
    }

    /// 读取设置中的字符串值并去空白（键缺失得到空串）。
    fn read_setting_string(&self, key: &str) -> String {
        read_trimmed_string(&self.read_settings()[key])
    }

    /// 取当前认证条目：优先 `current` 标记的条目，没有则取第一条。
    pub fn get_auth(&self) -> Option<LinearAuthEntry> {
        let list = self.read_auth_list();
        list.iter()
            .find(|entry| entry.current)
            .cloned()
            .or_else(|| list.into_iter().next())
    }

    /// 按 workspace 标识取认证条目；标识为空时退化为 `get_auth`。
    pub fn get_auth_by_workspace_id(&self, workspace_id: &str) -> Option<LinearAuthEntry> {
        let id = workspace_id.trim();
        if id.is_empty() {
            return self.get_auth();
        }
        self.read_auth_list()
            .into_iter()
            .find(|entry| entry.workspace_id == id)
    }

    /// 列出全部 workspace 概要（不含 token），authorizedAt 缺失回退 createdAt。
    pub fn auth_workspaces(&self) -> Vec<WorkspaceSummary> {
        self.read_auth_list()
            .into_iter()
            .map(|entry| WorkspaceSummary {
                id: entry.workspace_id.clone(),
                name: entry.organization.as_ref().map(|org| org.name.clone()),
                url_key: entry
                    .organization
                    .as_ref()
                    .and_then(|org| org.url_key.clone()),
                current: entry.current,
                user: entry.user.clone(),
                authorized_at: truthy_num(entry.authorized_at).or(truthy_num(entry.created_at)),
            })
            .collect()
    }

    /// JS `setLinearAuth`.
    ///
    /// 中文说明：按 workspace 合并新旧字段（缺省沿用旧值）；`activate`
    /// 决定是否把该条设为 current，列表原本无 current 时强制激活本条。
    /// accessToken 为空返回 INVALID 错误。
    pub fn set_auth(
        &self,
        input: SetLinearAuthInput,
        activate: bool,
    ) -> Result<LinearAuthEntry, super::LinearError> {
        if input.access_token.trim().is_empty() {
            return Err(super::LinearError::plain("accessToken is required"));
        }
        let now = super::now_ms();
        let mut list = self.read_auth_list();
        let current = list
            .iter()
            .find(|entry| entry.current)
            .cloned()
            .or_else(|| list.first().cloned());

        let next_user = match &input.user {
            Some(user) => user.clone(),
            None => current.as_ref().and_then(|entry| entry.user.clone()),
        };
        let next_organization = match &input.organization {
            Some(organization) => organization.clone(),
            None => current
                .as_ref()
                .and_then(|entry| entry.organization.clone()),
        };
        // JS: `input?.workspaceId || (nextOrganization || nextUser ? '' : current?.workspaceId)`
        let workspace_id_arg = match input.workspace_id.as_deref().map(str::trim) {
            Some(explicit) if !explicit.is_empty() => explicit.to_string(),
            _ => {
                if next_organization.is_some() || next_user.is_some() {
                    String::new()
                } else {
                    current
                        .as_ref()
                        .map(|entry| entry.workspace_id.clone())
                        .unwrap_or_default()
                }
            }
        };
        let workspace_id = resolve_linear_workspace_id(
            next_organization.as_ref(),
            next_user.as_ref(),
            &workspace_id_arg,
        );

        let existing_index = list
            .iter()
            .position(|entry| entry.workspace_id == workspace_id);
        let previous: Option<LinearAuthEntry> = match existing_index {
            Some(index) => Some(list[index].clone()),
            None => {
                if next_organization.is_some() || next_user.is_some() {
                    None
                } else {
                    current.clone()
                }
            }
        };
        let target_index = match existing_index {
            Some(index) => Some(index),
            None => previous
                .as_ref()
                .filter(|_| next_organization.is_none() && next_user.is_none())
                .and_then(|p| {
                    list.iter()
                        .position(|entry| entry.workspace_id == p.workspace_id)
                }),
        };
        let was_current = previous.as_ref().is_some_and(|entry| entry.current);

        let input_token_type = input.token_type.clone().unwrap_or_default();
        let token_type = if input_token_type.trim().is_empty() {
            previous
                .as_ref()
                .map(|entry| entry.token_type.clone())
                .unwrap_or_else(|| "bearer".to_string())
        } else {
            input_token_type.trim().to_string()
        };
        let input_scope = input.scope.clone().unwrap_or_default();
        let scope = if input_scope.trim().is_empty() {
            previous
                .as_ref()
                .map(|entry| entry.scope.clone())
                .unwrap_or_default()
        } else {
            input_scope.trim().to_string()
        };

        let next = LinearAuthEntry {
            access_token: input.access_token.trim().to_string(),
            refresh_token: match input.refresh_token {
                Some(value) => value,
                None => previous
                    .as_ref()
                    .and_then(|entry| entry.refresh_token.clone()),
            },
            token_type,
            expires_at: input
                .expires_at
                .or(previous.as_ref().and_then(|entry| entry.expires_at)),
            scope,
            created_at: previous
                .as_ref()
                .and_then(|entry| truthy_num(entry.created_at))
                .or(Some(now)),
            authorized_at: if activate {
                Some(now)
            } else {
                previous
                    .as_ref()
                    .and_then(|entry| truthy_num(entry.authorized_at))
                    .or(previous
                        .as_ref()
                        .and_then(|entry| truthy_num(entry.created_at)))
                    .or(Some(now))
            },
            user: next_user,
            organization: next_organization,
            current: false,
            workspace_id: workspace_id.clone(),
        };

        match target_index {
            Some(index) => list[index] = next.clone(),
            None => list.push(next.clone()),
        }
        let written_index = target_index.unwrap_or(list.len() - 1);

        let any_current = list.iter().any(|entry| entry.current);
        if activate || !any_current {
            for (index, entry) in list.iter_mut().enumerate() {
                entry.current = index == written_index;
            }
        } else {
            list[written_index].current = was_current;
        }

        self.write_auth_list(&list)
            .map_err(|e| super::LinearError::plain(e.to_string()))?;
        Ok(list[written_index].clone())
    }

    /// JS `activateLinearAuth`.
    ///
    /// 中文说明：把指定 workspace 设为唯一 current；找不到标识或写入失败
    /// 返回 false。
    pub fn activate_auth(&self, workspace_id: &str) -> bool {
        let id = workspace_id.trim();
        if id.is_empty() {
            return false;
        }
        let mut list = self.read_auth_list();
        let Some(index) = list.iter().position(|entry| entry.workspace_id == id) else {
            return false;
        };
        for (idx, entry) in list.iter_mut().enumerate() {
            entry.current = idx == index;
        }
        self.write_auth_list(&list).is_ok()
    }

    /// JS `clearLinearAuth`: drops only the named (or current) workspace.
    ///
    /// 中文说明：只删除指定 workspace（无指定时删 current 条目），剩余列表
    /// 失去 current 时把第一条设为 current；任何 IO 失败记日志并返回 false。
    pub fn clear_auth(&self, workspace_id: Option<&str>) -> bool {
        let outcome = (|| -> std::io::Result<bool> {
            let list = self.read_auth_list();
            if list.is_empty() {
                return Ok(true);
            }
            let id = workspace_id.map(str::trim).filter(|v| !v.is_empty());
            let mut remaining: Vec<LinearAuthEntry> = match id {
                Some(id) => list
                    .into_iter()
                    .filter(|entry| entry.workspace_id != id)
                    .collect(),
                None => list.into_iter().filter(|entry| !entry.current).collect(),
            };
            if remaining.is_empty() {
                self.write_auth_list(&[])?;
                return Ok(true);
            }
            if !remaining.iter().any(|entry| entry.current)
                && let Some(first) = remaining.first_mut()
            {
                first.current = true;
            }
            self.write_auth_list(&remaining)?;
            Ok(true)
        })();
        outcome.unwrap_or_else(|error| {
            tracing::error!("Failed to clear Linear auth file: {error}");
            false
        })
    }

    /// JS `toLinearPublicStatus`.
    ///
    /// 中文说明：组装不含 token 的连接状态 JSON（connected/user/
    /// organization/workspaces/scope）；未连接时只返回 `{ connected: false }`。
    pub fn to_public_status(
        &self,
        auth: Option<&LinearAuthEntry>,
        workspaces: Option<Vec<WorkspaceSummary>>,
    ) -> Value {
        let workspaces = workspaces.unwrap_or_else(|| self.auth_workspaces());
        let Some(auth) = auth else {
            return json!({ "connected": false });
        };
        if auth.access_token.is_empty() {
            return json!({ "connected": false });
        }
        let mut payload = json!({
            "connected": true,
            "user": auth.user.as_ref().map(LinearUser::to_json),
            "organization": auth.organization.as_ref().map(LinearOrganization::to_json),
            "workspaces": workspaces.iter().map(WorkspaceSummary::to_json).collect::<Vec<_>>(),
        });
        if !auth.scope.is_empty() {
            payload["scope"] = json!(auth.scope);
        }
        payload
    }

    /// OAuth client id：环境变量 > settings.json > 内置默认值。
    pub fn get_client_id(&self) -> String {
        let from_env = self.env_value("OPENCHAMBER_LINEAR_CLIENT_ID");
        if !from_env.is_empty() {
            return from_env;
        }
        let stored = self.read_setting_string("linearClientId");
        if !stored.is_empty() {
            return stored;
        }
        DEFAULT_LINEAR_CLIENT_ID.to_string()
    }

    /// OAuth client secret（机密客户端才配置），环境变量优先于 settings.json，
    /// 未配置返回 `None`。
    pub fn get_client_secret(&self) -> Option<String> {
        let from_env = self.env_value("OPENCHAMBER_LINEAR_CLIENT_SECRET");
        if !from_env.is_empty() {
            return Some(from_env);
        }
        let stored = self.read_setting_string("linearClientSecret");
        (!stored.is_empty()).then_some(stored)
    }

    /// OAuth scope：环境变量 > settings.json > 默认 `read,write,comments:create`。
    pub fn get_scopes(&self) -> String {
        let from_env = self.env_value("OPENCHAMBER_LINEAR_SCOPES");
        if !from_env.is_empty() {
            return from_env;
        }
        let stored = self.read_setting_string("linearScopes");
        if !stored.is_empty() {
            return stored;
        }
        DEFAULT_LINEAR_SCOPES.to_string()
    }

    /// 回调 broker URL：环境变量 > settings.json > 默认生产地址；末尾斜杠去除。
    pub fn get_broker_url(&self) -> String {
        let from_env = self.env_value("OPENCHAMBER_LINEAR_BROKER_URL");
        if !from_env.is_empty() {
            return from_env.trim_end_matches('/').to_string();
        }
        let stored = self.read_setting_string("linearBrokerUrl");
        if !stored.is_empty() {
            return stored.trim_end_matches('/').to_string();
        }
        DEFAULT_LINEAR_BROKER_URL.to_string()
    }

    /// OAuth redirect URI：环境变量 > settings.json > broker URL 拼接 `/callback`。
    pub fn get_redirect_uri(&self) -> String {
        let from_env = self.env_value("OPENCHAMBER_LINEAR_REDIRECT_URI");
        if !from_env.is_empty() {
            return from_env;
        }
        let stored = self.read_setting_string("linearRedirectUri");
        if !stored.is_empty() {
            return stored;
        }
        format!("{}/callback", self.get_broker_url())
    }

    /// Status comments are opt-in: they are written into a Linear workspace
    /// other people read, so nothing is posted until the user turns them on.
    ///
    /// 中文说明：读取 settings.json 中 `linearSessionComments` 是否严格为
    /// true（默认关闭，写评论前必须显式开启）。
    pub fn session_comments_enabled(&self) -> bool {
        matches!(
            self.read_settings()[SESSION_COMMENTS_SETTING_KEY],
            Value::Bool(true)
        )
    }

    /// 写入会话评论开关并原样返回传入值（写失败仅记日志，仍返回新值）。
    pub fn set_session_comments_enabled(&self, enabled: bool) -> bool {
        let mut settings = self.read_settings();
        settings[SESSION_COMMENTS_SETTING_KEY] = json!(enabled);
        if let Err(error) = self.write_settings(settings) {
            tracing::error!("Failed to write Linear settings: {error}");
        }
        enabled
    }
}
