//! Port of `server/lib/git/identity-storage.js` (git identity profiles
//! persisted at `~/.config/ompchamber/git-identities.json`) and
//! `server/lib/git/credentials.js` (`~/.git-credentials` host/username
//! discovery).
//!
//! The storage root is injectable (`IdentityStorage::default()` pins the JS
//! home-directory path) so route tests can point it at a temp directory.
//!
//! （中文概述）本模块承载两块移植逻辑：其一，git 身份档案
//! （identity profiles）的 JSON 持久化存储；其二，从
//! `~/.git-credentials` 逐行发现 host/username 凭据。存储根目录可注入
//! （[`IdentityStorage::with_root`]），生产默认沿用 JS 版的 home 目录
//! 路径，路由测试可指向临时目录。

use std::path::PathBuf;

use serde_json::{Value, json};

/// git 身份档案的 JSON 文件存储：读取/写入 `<root>/git-identities.json`，
/// 所有档案以 `serde_json::Value` 形式操作，错误以 `String` 返回
/// （消息与 JS 版逐字一致）。
pub struct IdentityStorage {
    /// 存储根目录；文件名固定为 `git-identities.json`。
    root: PathBuf,
}

/// 生产默认构造：存储位置与 JS 版 `identity-storage.js` 保持一致。
impl Default for IdentityStorage {
    /// 存储根为 `~/.config/ompchamber`；取不到 home 目录时
    /// 退化为相对路径 `.`（与 JS 版兜底行为一致）。
    fn default() -> Self {
        Self {
            root: crate::config::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
                .join("ompchamber"),
        }
    }
}

/// 身份档案的增删改查与落盘：读写均整文件覆盖，
/// 校验错误消息保持与 JS `identity-storage.js` 逐字一致。
impl IdentityStorage {
    /// 以指定目录为存储根构造实例（测试注入临时目录用）。
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 返回落盘文件完整路径：`<root>/git-identities.json`。
    fn storage_file(&self) -> PathBuf {
        self.root.join("git-identities.json")
    }

    /// 幂等创建存储根目录；失败被忽略——后续 `load` 会兜底为空结构，
    /// `save` 则把写盘错误向上传播。
    fn ensure_dir(&self) {
        let _ = std::fs::create_dir_all(&self.root);
    }

    /// JS `loadProfiles` — missing file or parse failure → `{ profiles: [] }`.
    ///
    /// 读取并解析存储文件：文件缺失或 JSON 解析失败时统一返回
    /// `{ "profiles": [] }` 兜底结构，绝不向调用方报错（与 JS 行为一致）。
    pub fn load(&self) -> Value {
        self.ensure_dir();
        let path = self.storage_file();
        let Ok(content) = std::fs::read_to_string(&path) else {
            return json!({ "profiles": [] });
        };
        match serde_json::from_str::<Value>(&content) {
            Ok(value) => value,
            Err(_) => json!({ "profiles": [] }),
        }
    }

    /// JS `saveProfiles` — pretty-printed write; failure propagates.
    ///
    /// 以 pretty 格式序列化后整体覆写存储文件；序列化或写盘失败时
    /// 错误信息以 `Err(String)` 向上传播。
    pub fn save(&self, data: &Value) -> Result<(), String> {
        self.ensure_dir();
        let pretty = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
        std::fs::write(self.storage_file(), pretty).map_err(|e| e.to_string())
    }

    /// 读取全部档案：文件缺失或 `profiles` 字段不是数组时返回空 `Vec`。
    pub fn get_profiles(&self) -> Vec<Value> {
        self.load()
            .get("profiles")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// 按 `id` 精确匹配查询单个档案；不存在时返回 `None`。
    pub fn get_profile(&self, id: &str) -> Option<Value> {
        self.get_profiles()
            .into_iter()
            .find(|profile| profile.get("id").and_then(Value::as_str) == Some(id))
    }

    /// JS `createProfile` — validation errors are the exact JS messages.
    ///
    /// 新建档案：先做唯一性与必填校验（id 已存在、id/userName/userEmail
    /// 任一为空分别返回 JS 同款错误消息），再补默认值——`authType` 缺省为
    /// `ssh`、`name` 缺省取 `userName`、`color` 缺省为 `keyword`、
    /// `icon` 缺省为 `branch`；sshKey/signingKey/host 空值归一为 `null`。
    /// 落盘成功后返回新建的档案对象。
    pub fn create_profile(&self, profile_data: &Value) -> Result<Value, String> {
        let mut profiles = self.get_profiles();
        let text = |field: &str| {
            profile_data
                .get(field)
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default()
        };
        let id = text("id");
        let user_name = text("userName");
        let user_email = text("userEmail");

        if profiles
            .iter()
            .any(|profile| profile.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            return Err(format!("Profile with ID \"{}\" already exists", id));
        }
        if id.is_empty() || user_name.is_empty() || user_email.is_empty() {
            return Err("Profile must have id, userName, and userEmail".to_string());
        }

        let auth_type = {
            let value = text("authType");
            if value.is_empty() {
                "ssh".to_string()
            } else {
                value
            }
        };
        let name = {
            let value = text("name");
            if value.is_empty() {
                user_name.clone()
            } else {
                value
            }
        };
        let new_profile = json!({
            "id": id,
            "name": name,
            "userName": user_name,
            "userEmail": user_email,
            "authType": auth_type,
            "sshKey": optional_text(profile_data.get("sshKey")),
            "signCommits": profile_data.get("signCommits").cloned(),
            "signingKey": optional_text(profile_data.get("signingKey")),
            "host": optional_text(profile_data.get("host")),
            "color": non_empty_or(text("color"), "keyword"),
            "icon": non_empty_or(text("icon"), "branch"),
        });
        profiles.push(new_profile.clone());
        self.save(&json!({ "profiles": profiles }))?;
        Ok(new_profile)
    }

    /// JS `updateProfile` — shallow merge, `id` immutable.
    ///
    /// 按 id 定位档案并对顶层字段做浅合并（更新体逐键覆盖旧值）；
    /// 合并后无论更新体写什么，`id` 字段始终强制回写为原值（不可变）。
    /// 档案不存在时返回 `Err`（JS 同款消息）。
    pub fn update_profile(&self, id: &str, updates: &Value) -> Result<Value, String> {
        let mut profiles = self.get_profiles();
        let index = profiles
            .iter()
            .position(|profile| profile.get("id").and_then(Value::as_str) == Some(id))
            .ok_or_else(|| format!("Profile with ID \"{}\" not found", id))?;

        let mut merged = profiles[index].clone();
        if let (Some(merged_map), Some(update_map)) = (merged.as_object_mut(), updates.as_object())
        {
            for (key, value) in update_map {
                merged_map.insert(key.clone(), value.clone());
            }
        }
        if let Some(original_id) = profiles[index].get("id").cloned()
            && let Some(merged_map) = merged.as_object_mut()
        {
            merged_map.insert("id".to_string(), original_id);
        }
        profiles[index] = merged.clone();
        self.save(&json!({ "profiles": profiles }))?;
        Ok(merged)
    }

    /// JS `deleteProfile`.
    ///
    /// 按 id 过滤删除：id 不存在（一条都没删掉）时返回 `Err`，
    /// 删除成功并落盘后返回 `Ok(true)`。
    pub fn delete_profile(&self, id: &str) -> Result<bool, String> {
        let profiles = self.get_profiles();
        let filtered: Vec<Value> = profiles
            .iter()
            .filter(|profile| profile.get("id").and_then(Value::as_str) != Some(id))
            .cloned()
            .collect();
        if filtered.len() == profiles.len() {
            return Err(format!("Profile with ID \"{}\" not found", id));
        }
        self.save(&json!({ "profiles": filtered }))?;
        Ok(true)
    }
}

/// 把 `Option<&Value>` 归一为 JSON 字符串：非空字符串原样返回，
/// 缺失、非字符串或空串一律置为 `Value::Null`（用于可选档案字段）。
fn optional_text(value: Option<&Value>) -> Value {
    match value.and_then(Value::as_str) {
        Some(text) if !text.is_empty() => Value::String(text.to_string()),
        _ => Value::Null,
    }
}

/// 空字符串则取 `fallback`，否则原样返回（用于 color/icon 默认值）。
fn non_empty_or(value: String, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

// ---------------------------------------------------------------------------
// credentials.js
// ---------------------------------------------------------------------------

/// JS `discoverGitCredentials` — host + username pairs from
/// `~/.git-credentials` lines (`https://user@host/path…`), deduped, invalid
/// lines skipped.
///
/// 从 `home/.git-credentials` 逐行解析出 `{ host, username }` 数组：
/// 跳过空行、无法解析的行以及 host 或 username 为空的条目，
/// 并按 (host, username) 二元组去重；文件不可读时返回空数组。
/// `home` 为 `None` 时回退到进程的 home 目录探测。
pub fn discover_git_credentials(home: Option<&std::path::Path>) -> Vec<Value> {
    let home = home
        .map(PathBuf::from)
        .or_else(crate::config::home_dir)
        .unwrap_or_default();
    let credentials_path = home.join(".git-credentials");
    let Ok(content) = std::fs::read_to_string(&credentials_path) else {
        return Vec::new();
    };

    let mut credentials: Vec<Value> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(url) = parse_stored_credential_url(trimmed) else {
            continue;
        };
        let (host, username) = url;
        if host.is_empty() || username.is_empty() {
            continue;
        }
        if credentials.iter().any(|entry| {
            entry.get("host") == Some(&Value::String(host.clone()))
                && entry.get("username") == Some(&Value::String(username.clone()))
        }) {
            continue;
        }
        credentials.push(json!({ "host": host, "username": username }));
    }
    credentials
}

/// Minimal WHATWG URL parse of the stored credential line
/// (`scheme://user:pass@host/path`): hostname + pathname (empty for "/"),
/// username; percent-decoded like JS `URL`.
///
/// 手写的极简 URL 解析（不引入 url crate）：拆出
/// `scheme://user:pass@host[:port]/path` 各部分，返回
/// `(hostname+pathname, username)`——hostname 小写化并剥掉端口
/// （含 IPv6 字面量按 `]` 分段的处理），路径为空或 `/` 时省略，
/// username 做 percent-decode。行内缺 `://` 或 scheme 为空返回 `None`。
fn parse_stored_credential_url(line: &str) -> Option<(String, String)> {
    let (scheme, rest) = line.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(index) => (&authority[..index], &authority[index + 1..]),
        None => ("", authority),
    };
    let username = match userinfo.split_once(':') {
        Some((user, _)) => user,
        None => userinfo,
    };
    let hostname = hostport
        .split(']')
        .next_back()
        .unwrap_or(hostport)
        .rsplit(':')
        .next_back()
        .unwrap_or("")
        .to_lowercase();
    let pathname = if path.is_empty() || path == "/" {
        String::new()
    } else {
        path.to_string()
    };
    let host = format!("{}{}", hostname, pathname);
    Some((host, percent_decode(username)))
}

/// 解码 `%XX` 十六进制转义：任意字节值均接受，非法或不完整的转义
/// 原样保留 `%` 字符；解码结果经 `from_utf8_lossy` 收敛为 `String`。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&String::from_utf8_lossy(&bytes[i + 1..i + 3]), 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 身份存储 CRUD 与 `~/.git-credentials` 凭据发现的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 新建一个以「进程 id + 8 位随机十六进制后缀」命名的临时目录，
    /// 返回其路径，作为测试专用的存储根。
    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ompchamber-git-identity-{}-{}",
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 生成 8 个随机十六进制字符组成的后缀，避免并发测试的临时目录冲突。
    fn rand_suffix() -> String {
        use rand::Rng;
        let mut rng = rand::rng();
        (0..8)
            .map(|_| format!("{:x}", rng.random_range(0..16)))
            .collect()
    }

    /// 验证档案 CRUD 全链路契约：空库起步返回空数组、创建时默认值填充
    /// （authType/color/icon/name）、重复 id 与缺字段返回 JS 同款错误消息、
    /// 更新时 `id` 不可被覆盖、删除后二次删除报错。
    #[test]
    fn profiles_roundtrip_with_validation() {
        let root = temp_root();
        let storage = IdentityStorage::with_root(&root);
        assert_eq!(storage.get_profiles(), Vec::<Value>::new());

        let profile = storage
            .create_profile(&json!({
                "id": "work",
                "userName": "Ada",
                "userEmail": "ada@example.com",
                "sshKey": "/keys/ada",
            }))
            .expect("create");
        assert_eq!(profile.get("authType"), Some(&json!("ssh")));
        assert_eq!(profile.get("color"), Some(&json!("keyword")));
        assert_eq!(profile.get("icon"), Some(&json!("branch")));
        assert_eq!(profile.get("name"), Some(&json!("Ada")));
        assert_eq!(profile.get("sshKey"), Some(&json!("/keys/ada")));
        assert_eq!(profile.get("signCommits"), Some(&Value::Null));

        let err = storage
            .create_profile(&json!({ "id": "work", "userName": "x", "userEmail": "y" }))
            .unwrap_err();
        assert_eq!(err, "Profile with ID \"work\" already exists");
        let err = storage
            .create_profile(&json!({ "id": "partial" }))
            .unwrap_err();
        assert_eq!(err, "Profile must have id, userName, and userEmail");

        let updated = storage
            .update_profile("work", &json!({ "color": "string", "id": "changed" }))
            .expect("update");
        assert_eq!(updated.get("color"), Some(&json!("string")));
        assert_eq!(updated.get("id"), Some(&json!("work")));

        assert!(storage.update_profile("missing", &json!({})).is_err());
        assert!(storage.delete_profile("work").unwrap());
        assert!(storage.delete_profile("work").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 验证凭据发现契约：有效行提取 host+username（含路径）、
    /// 相同 (host, username) 去重、无法解析的行跳过、缺 username 的行跳过。
    #[test]
    fn credentials_parse_dedupe_and_skip() {
        let root = temp_root();
        let credentials = root.join(".git-credentials");
        std::fs::write(
            &credentials,
            "https://ada@example.com\nhttps://bob@github.com/org/repo\nhttps://ada@example.com\nnot a url\nhttps://example.com\n",
        )
        .unwrap();
        let found = discover_git_credentials(Some(&root));
        assert_eq!(
            found,
            vec![
                json!({ "host": "example.com", "username": "ada" }),
                json!({ "host": "github.com/org/repo", "username": "bob" }),
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 验证 `~/.git-credentials` 文件缺失时返回空数组而非报错。
    #[test]
    fn credentials_missing_file_is_empty() {
        let root = temp_root();
        assert!(discover_git_credentials(Some(&root)).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
