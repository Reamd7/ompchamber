//! Port of `server/lib/tunnels/managed-config.js`: managed remote tunnel
//! token/preset persistence runtime. File paths and the persistence mutex are
//! injected (the JS receives them from `server/index.js` wiring).
//! （中文说明）managed remote tunnel 的 token 与 preset 持久化运行时：
//! 维护一份版本化 JSON 配置（条目含 id/name/hostname/token/updatedAt），
//! 读取与写入都经过 sanitize（丢弃必填字段缺失的条目、归一化 hostname、
//! 按 id 与 hostname 去重），落盘走 0o600 私有文件；主文件缺失时从
//! legacy 文件迁移。文件路径与互斥锁由外部注入；settings 运行时在
//! preset 变化时调用同步接口裁剪条目。

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::settings::normalization::normalize_managed_remote_tunnel_hostname;

/// 受管 tunnel 配置文件的当前版本号。
pub const CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION: u64 = 1;

/// 单条受管 remote tunnel 的持久化记录（配置文件 tunnels 数组的元素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRemoteTunnelEntry {
    /// preset 标识（与 settings 中 preset 的 id 对齐）。
    pub id: String,
    /// 展示名称。
    pub name: String,
    /// 归一化后的公开 hostname（小写）。
    pub hostname: String,
    /// cloudflared tunnel token（敏感字段，仅写入 0o600 文件）。
    pub token: String,
    /// 最后更新时间（Unix 毫秒时间戳）。
    pub updated_at: u64,
}

/// 条目的序列化。
impl ManagedRemoteTunnelEntry {
    /// 输出驼峰键名与 JS 配置文件格式一致的 JSON 对象。
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "hostname": self.hostname,
            "token": self.token,
            "updatedAt": self.updated_at,
        })
    }
}

/// 配置文件整体结构：版本号加 tunnel 条目列表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRemoteTunnelConfig {
    /// 配置格式版本（当前恒为当前版本号常量）。
    pub version: u64,
    /// tunnel 条目（读盘与落盘前都会 sanitize）。
    pub tunnels: Vec<ManagedRemoteTunnelEntry>,
}

/// 构造与序列化。
impl ManagedRemoteTunnelConfig {
    /// 空配置：当前版本号加空列表，作为文件缺失或损坏时的兜底值。
    pub fn empty() -> Self {
        Self {
            version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
            tunnels: Vec::new(),
        }
    }

    /// 序列化为含 version 与 tunnels 的 JSON 对象。
    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "tunnels": self.tunnels.iter().map(ManagedRemoteTunnelEntry::to_json).collect::<Vec<_>>(),
        })
    }
}

/// 当前 Unix 时间戳（毫秒）；系统时钟早于 epoch 的异常情况下返回 0。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 取 JSON 值中的有限数值；NaN 与 Infinity 视为缺失返回 None。
fn finite_number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

/// `sanitizeManagedRemoteTunnelConfigEntries`: drops entries missing any
/// required field, normalizes hostnames, dedupes by id and hostname.
/// （中文）清洗 tunnels 数组：输入非数组返回空列表；逐条 trim
/// id/name/token、经 settings 的归一化函数处理 hostname（非法即丢弃
/// 整条），updatedAt 缺失或非有限数时补当前时间；任一必填字段为空的
/// 条目被丢弃；再按 id 与 hostname 各自去重，保留先出现者。
fn sanitize_managed_remote_tunnel_config_entries(value: &Value) -> Vec<ManagedRemoteTunnelEntry> {
    let Some(entries) = value.as_array() else {
        return Vec::new();
    };

    let mut result = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_hostnames = std::collections::HashSet::new();
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let hostname = entry
            .get("hostname")
            .map(|value| normalize_managed_remote_tunnel_hostname(value).unwrap_or_default())
            .unwrap_or_default();
        let token = entry
            .get("token")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        let updated_at = entry
            .get("updatedAt")
            .and_then(finite_number)
            .map(|value| value as u64)
            .unwrap_or_else(now_ms);

        if id.is_empty() || name.is_empty() || hostname.is_empty() || token.is_empty() {
            continue;
        }
        if seen_ids.contains(id) || seen_hostnames.contains(&hostname) {
            continue;
        }
        seen_ids.insert(id.to_string());
        seen_hostnames.insert(hostname.clone());
        result.push(ManagedRemoteTunnelEntry {
            id: id.to_string(),
            name: name.to_string(),
            hostname,
            token: token.to_string(),
            updated_at,
        });
    }
    result
}

/// 受管 tunnel 配置的读写运行时：绑定主文件与 legacy 文件两个路径，
/// 所有修改经由同一把持久化互斥锁串行执行。
pub struct ManagedTunnelConfigRuntime {
    /// 主配置文件路径（版本化新格式）。
    file_path: PathBuf,
    /// 旧版 named-tunnel 配置路径，仅主文件缺失时读取迁移。
    legacy_file_path: PathBuf,
    /// `persistManagedRemoteTunnelConfigLock`: serializes read-modify-write.
    /// （中文）对应 JS 的持久化锁：把读-改-写整个临界区串行化，
    /// 避免并发 upsert 互相覆盖丢数据。
    persist_lock: tokio::sync::Mutex<()>,
}

/// 配置读写、legacy 迁移与条目增查的完整实现。
impl ManagedTunnelConfigRuntime {
    /// 以主文件与 legacy 文件路径构造运行时。
    pub fn new(file_path: PathBuf, legacy_file_path: PathBuf) -> Self {
        Self {
            file_path,
            legacy_file_path,
            persist_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 把配置写盘：确保父目录存在后序列化为 pretty JSON，经 0o600
    /// 私有文件写入主路径。
    async fn write_managed_remote_tunnel_config_to_disk(
        &self,
        config: &ManagedRemoteTunnelConfig,
    ) -> std::io::Result<()> {
        if let Some(parent) = self.file_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let body = serde_json::to_string_pretty(&config.to_json())?;
        write_private_file(&self.file_path, &body).await
    }

    /// 从 legacy 文件迁移：读取失败或 JSON 损坏一律告警并返回空配置；
    /// 解析成功则 sanitize 后立即写入主文件并返回迁移结果（写失败同样
    /// 降级为空配置，下次读取会再次尝试迁移）。
    async fn migrate_managed_remote_tunnel_config_from_legacy_file(
        &self,
    ) -> ManagedRemoteTunnelConfig {
        let raw = match tokio::fs::read_to_string(&self.legacy_file_path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return ManagedRemoteTunnelConfig::empty();
            }
            Err(err) => {
                tracing::warn!("Failed to migrate legacy named tunnel config file: {err}");
                return ManagedRemoteTunnelConfig::empty();
            }
        };
        let migrated = match serde_json::from_str::<Value>(&raw) {
            Ok(parsed) => ManagedRemoteTunnelConfig {
                version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
                tunnels: sanitize_managed_remote_tunnel_config_entries(
                    parsed.get("tunnels").unwrap_or(&Value::Null),
                ),
            },
            Err(err) => {
                tracing::warn!("Failed to migrate legacy named tunnel config file: {err}");
                ManagedRemoteTunnelConfig::empty()
            }
        };
        if let Err(err) = self
            .write_managed_remote_tunnel_config_to_disk(&migrated)
            .await
        {
            tracing::warn!("Failed to migrate legacy named tunnel config file: {err}");
            return ManagedRemoteTunnelConfig::empty();
        }
        migrated
    }

    /// `readManagedRemoteTunnelConfigFromDisk`.
    /// （中文）读取主配置：文件不存在时触发 legacy 迁移；读取失败、
    /// JSON 损坏或顶层非对象均返回空配置（只告警不报错）；成功时强制
    /// version 为当前版本号并 sanitize tunnels。
    pub async fn read_managed_remote_tunnel_config_from_disk(&self) -> ManagedRemoteTunnelConfig {
        let raw = match tokio::fs::read_to_string(&self.file_path).await {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return self
                    .migrate_managed_remote_tunnel_config_from_legacy_file()
                    .await;
            }
            Err(err) => {
                tracing::warn!("Failed to read managed remote tunnel config file: {err}");
                return ManagedRemoteTunnelConfig::empty();
            }
        };
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(err) => {
                tracing::warn!("Failed to read managed remote tunnel config file: {err}");
                return ManagedRemoteTunnelConfig::empty();
            }
        };
        if !parsed.is_object() {
            return ManagedRemoteTunnelConfig::empty();
        }
        ManagedRemoteTunnelConfig {
            version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
            tunnels: sanitize_managed_remote_tunnel_config_entries(
                parsed.get("tunnels").unwrap_or(&Value::Null),
            ),
        }
    }

    /// `updateManagedRemoteTunnelConfig`: read → sanitize → mutate → sanitize →
    /// write, serialized by the persist lock.
    /// （中文）所有修改的统一入口：持锁后执行 读盘、sanitize、mutate、
    /// 再次 sanitize、写盘；写失败仅告警。双重 sanitize 保证 mutate
    /// 引入的脏数据不会落盘。
    async fn update<F>(&self, mutate: F)
    where
        F: FnOnce(ManagedRemoteTunnelConfig) -> ManagedRemoteTunnelConfig,
    {
        let _guard = self.persist_lock.lock().await;
        let current = self.read_managed_remote_tunnel_config_from_disk().await;
        let current = ManagedRemoteTunnelConfig {
            version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
            tunnels: sanitize_managed_remote_tunnel_config_entries(&Value::Array(
                current
                    .tunnels
                    .iter()
                    .map(|entry| entry.to_json())
                    .collect(),
            )),
        };
        let next = mutate(current);
        let sanitized = ManagedRemoteTunnelConfig {
            version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
            tunnels: sanitize_managed_remote_tunnel_config_entries(&Value::Array(
                next.tunnels.iter().map(|entry| entry.to_json()).collect(),
            )),
        };
        if let Err(err) = self
            .write_managed_remote_tunnel_config_to_disk(&sanitized)
            .await
        {
            tracing::warn!("Failed to write managed remote tunnel config file: {err}");
        }
    }

    /// `syncManagedRemoteTunnelConfigWithPresets(presets)` (called by the
    /// settings runtime when presets change).
    /// （中文）preset 变化后的同步：以 sanitize 后的 preset 列表为基准，
    /// 仅保留能按 id（或 hostname）匹配到已存 token 的条目——沿用旧
    /// token 与 updatedAt，名称等取 preset 新值；匹配不上的孤儿条目
    /// 被删除。
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn sync_managed_remote_tunnel_config_with_presets(&self, presets: &Value) {
        let sanitized_presets =
            crate::settings::normalization::normalize_managed_remote_tunnel_presets(presets)
                .unwrap_or_default();

        self.update(|current| {
            let by_id: std::collections::HashMap<String, ManagedRemoteTunnelEntry> = current
                .tunnels
                .iter()
                .cloned()
                .map(|entry| (entry.id.clone(), entry))
                .collect();
            let by_hostname: std::collections::HashMap<String, ManagedRemoteTunnelEntry> = current
                .tunnels
                .iter()
                .cloned()
                .map(|entry| (entry.hostname.clone(), entry))
                .collect();

            let mut next_tunnels = Vec::new();
            for preset in &sanitized_presets {
                let Some(preset) = preset.as_object() else {
                    continue;
                };
                let id = preset.get("id").and_then(Value::as_str).unwrap_or_default();
                let hostname = preset
                    .get("hostname")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let existing = by_id.get(id).or_else(|| by_hostname.get(hostname)).cloned();
                let Some(existing) = existing else {
                    continue;
                };
                next_tunnels.push(ManagedRemoteTunnelEntry {
                    id: id.to_string(),
                    name: preset
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    hostname: hostname.to_string(),
                    token: existing.token,
                    updated_at: existing.updated_at,
                });
            }

            ManagedRemoteTunnelConfig {
                version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
                tunnels: next_tunnels,
            }
        })
        .await;
    }

    /// `upsertManagedRemoteTunnelToken({ id, name, hostname, token })`.
    /// （中文）写入或替换一条 token 记录：四个字段任一在 trim 或归一化
    /// 后为空则静默忽略；否则先移除同 id 或同 hostname 的旧条目，再
    /// 追加 updated_at 为当前时间的新条目。
    pub async fn upsert_managed_remote_tunnel_token(
        &self,
        id: &str,
        name: &str,
        hostname: &str,
        token: &str,
    ) {
        let normalized_id = id.trim();
        let normalized_name = name.trim();
        let normalized_hostname =
            normalize_managed_remote_tunnel_hostname(&Value::from(hostname)).unwrap_or_default();
        let normalized_token = token.trim();
        if normalized_id.is_empty()
            || normalized_name.is_empty()
            || normalized_hostname.is_empty()
            || normalized_token.is_empty()
        {
            return;
        }

        let id = normalized_id.to_string();
        let name = normalized_name.to_string();
        let hostname = normalized_hostname;
        let token = normalized_token.to_string();
        self.update(move |current| {
            let mut without_conflicts: Vec<ManagedRemoteTunnelEntry> = current
                .tunnels
                .into_iter()
                .filter(|entry| entry.id != id && entry.hostname != hostname)
                .collect();
            without_conflicts.push(ManagedRemoteTunnelEntry {
                id,
                name,
                hostname,
                token,
                updated_at: now_ms(),
            });
            ManagedRemoteTunnelConfig {
                version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
                tunnels: without_conflicts,
            }
        })
        .await;
    }

    /// `resolveManagedRemoteTunnelToken({ presetId, hostname })`.
    /// （中文）按 preset id 优先、hostname 次之的顺序查找已存 token；
    /// 入参先 trim 与归一化，两者都未命中返回空串。
    pub async fn resolve_managed_remote_tunnel_token(
        &self,
        preset_id: &str,
        hostname: &str,
    ) -> String {
        let normalized_preset_id = preset_id.trim();
        let normalized_hostname =
            normalize_managed_remote_tunnel_hostname(&Value::from(hostname)).unwrap_or_default();
        let config = self.read_managed_remote_tunnel_config_from_disk().await;

        if !normalized_preset_id.is_empty()
            && let Some(entry) = config
                .tunnels
                .iter()
                .find(|entry| entry.id == normalized_preset_id)
            && !entry.token.is_empty()
        {
            return entry.token.clone();
        }

        if !normalized_hostname.is_empty()
            && let Some(entry) = config
                .tunnels
                .iter()
                .find(|entry| entry.hostname == normalized_hostname)
            && !entry.token.is_empty()
        {
            return entry.token.clone();
        }

        String::new()
    }
}

/// 以 0o600 权限私有写入：先写临时文件、设置权限后原子改名，确保
/// 敏感内容任何时刻都不会以全局可读形态出现在目标路径。
async fn write_private_file(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    // mode 0o600 via a temp file + rename keeps the token from ever appearing
    // world-readable on disk (JS: writeFile with mode 0o600).
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, body).await?;
    #[cfg(unix)]
    {
use crate::os_compat::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    tokio::fs::rename(&tmp, path).await
}

/// Crate-visible writer used by the cloudflared token file (mode 0o600).
/// （中文）crate 内复用的私有写入（cloudflared token 文件与测试使用）。
#[allow(dead_code)]
pub(crate) async fn write_private_file_for_tests(
    path: &std::path::Path,
    body: &str,
) -> std::io::Result<()> {
    write_private_file(path, body).await
}

/// Unique directory under the system temp dir (JS `fs.mkdtempSync(prefix)`).
/// （中文）在系统临时目录下创建唯一子目录：名称由前缀、进程 id、原子
/// 计数器与当前毫秒拼接，避免并发或重复调用互相冲突。
pub fn make_temp_dir(prefix: &str) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    // 进程内自增计数器：即便同一毫秒内多次调用也能生成唯一目录名。
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = format!(
        "{}{}-{}-{}",
        prefix,
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        now_ms()
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// 持久化运行时的行为测试：upsert/resolve 往返、冲突替换、preset 同步
/// 裁剪、legacy 迁移与异常文件降级。
#[cfg(test)]
mod tests {
    use super::*;

    /// 构造指向临时目录中 managed.json 与 legacy.json 的运行时（目录名
    /// 含进程 id 与时间戳，隔离并发用例）。
    fn temp_runtime(name: &str) -> (PathBuf, PathBuf, ManagedTunnelConfigRuntime) {
        let base = std::env::temp_dir().join(format!(
            "tunnels-managed-config-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&base).expect("create temp dir");
        let file = base.join("managed.json");
        let legacy = base.join("legacy.json");
        let runtime = ManagedTunnelConfigRuntime::new(file.clone(), legacy.clone());
        (file, legacy, runtime)
    }

    /// upsert 后字段归一化落盘，resolve 按 id 与 hostname 均能取回
    /// token，未命中返回空串。
    #[tokio::test]
    async fn upsert_and_resolve_roundtrip() {
        let (_file, _legacy, runtime) = temp_runtime("roundtrip");

        runtime
            .upsert_managed_remote_tunnel_token("preset-1", "My Tunnel", "Example.COM", "  tok1  ")
            .await;

        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert_eq!(config.version, 1);
        assert_eq!(config.tunnels.len(), 1);
        assert_eq!(config.tunnels[0].id, "preset-1");
        assert_eq!(config.tunnels[0].name, "My Tunnel");
        assert_eq!(config.tunnels[0].hostname, "example.com");
        assert_eq!(config.tunnels[0].token, "tok1");

        assert_eq!(
            runtime
                .resolve_managed_remote_tunnel_token("preset-1", "")
                .await,
            "tok1"
        );
        assert_eq!(
            runtime
                .resolve_managed_remote_tunnel_token("", "example.com")
                .await,
            "tok1"
        );
        assert_eq!(
            runtime
                .resolve_managed_remote_tunnel_token("nope", "nope.example")
                .await,
            ""
        );
    }

    /// 后续 upsert 会替换同 id 或同 hostname 的既有条目，最终仅剩一条。
    #[tokio::test]
    async fn upsert_replaces_conflicting_id_or_hostname() {
        let (_file, _legacy, runtime) = temp_runtime("conflicts");

        runtime
            .upsert_managed_remote_tunnel_token("id-a", "A", "a.example.com", "tok-a")
            .await;
        runtime
            .upsert_managed_remote_tunnel_token("id-b", "B", "a.example.com", "tok-b")
            .await;
        runtime
            .upsert_managed_remote_tunnel_token("id-b", "B2", "b.example.com", "tok-b2")
            .await;

        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert_eq!(config.tunnels.len(), 1);
        assert_eq!(config.tunnels[0].id, "id-b");
        assert_eq!(config.tunnels[0].hostname, "b.example.com");
        assert_eq!(config.tunnels[0].token, "tok-b2");
    }

    /// 字段为空白或 hostname 非法时 upsert 静默忽略，配置保持为空。
    #[tokio::test]
    async fn upsert_ignores_incomplete_input() {
        let (_file, _legacy, runtime) = temp_runtime("incomplete");
        runtime
            .upsert_managed_remote_tunnel_token(" ", "name", "host.example.com", "tok")
            .await;
        runtime
            .upsert_managed_remote_tunnel_token("id", "name", "not a host", "tok")
            .await;
        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert!(config.tunnels.is_empty());
    }

    /// preset 同步保留能匹配的条目（取新名称、留旧 token）并删除孤儿
    /// 条目。
    #[tokio::test]
    async fn sync_keeps_tokens_for_known_presets_and_drops_orphans() {
        let (_file, _legacy, runtime) = temp_runtime("sync");

        runtime
            .upsert_managed_remote_tunnel_token("preset-1", "Old Name", "a.example.com", "tok-a")
            .await;
        runtime
            .upsert_managed_remote_tunnel_token("preset-2", "Orphan", "orphan.example.com", "tok-o")
            .await;

        let presets = json!([
            { "id": "preset-1", "name": "New Name", "hostname": "a.example.com" },
            { "id": "preset-3", "name": "Unknown", "hostname": "c.example.com" },
        ]);
        runtime
            .sync_managed_remote_tunnel_config_with_presets(&presets)
            .await;

        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert_eq!(config.tunnels.len(), 1);
        assert_eq!(config.tunnels[0].id, "preset-1");
        assert_eq!(config.tunnels[0].name, "New Name");
        assert_eq!(config.tunnels[0].token, "tok-a");
    }

    /// preset 的 id 变化但 hostname 不变时按 hostname 匹配，token 得以
    /// 保留。
    #[tokio::test]
    async fn sync_matches_by_hostname_when_id_changed() {
        let (_file, _legacy, runtime) = temp_runtime("sync-hostname");
        runtime
            .upsert_managed_remote_tunnel_token("old-id", "Keep", "a.example.com", "tok-a")
            .await;
        let presets = json!([{ "id": "new-id", "name": "Keep", "hostname": "a.example.com" }]);
        runtime
            .sync_managed_remote_tunnel_config_with_presets(&presets)
            .await;
        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert_eq!(config.tunnels.len(), 1);
        assert_eq!(config.tunnels[0].id, "new-id");
        assert_eq!(config.tunnels[0].token, "tok-a");
    }

    /// 主文件缺失时从 legacy 迁移：trim、丢弃无效条目、保留原
    /// updatedAt，并把结果落盘到主文件。
    #[tokio::test]
    async fn migrates_legacy_named_tunnels_file() {
        let (file, legacy, runtime) = temp_runtime("legacy");
        let legacy_body = json!({
            "version": 0,
            "tunnels": [
                { "id": "  t1  ", "name": "One", "hostname": "one.example.com", "token": "tok-1", "updatedAt": 123 },
                { "id": "t2", "name": "", "hostname": "two.example.com", "token": "tok-2" }
            ]
        });
        std::fs::write(&legacy, serde_json::to_string(&legacy_body).unwrap()).unwrap();

        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert_eq!(config.version, 1);
        assert_eq!(config.tunnels.len(), 1);
        assert_eq!(config.tunnels[0].id, "t1");
        assert_eq!(config.tunnels[0].updated_at, 123);
        assert!(file.exists(), "migrated config persisted next to settings");
    }

    /// 主与 legacy 文件都缺失时读取返回版本 1 的空配置。
    #[tokio::test]
    async fn missing_files_read_as_empty() {
        let (_file, _legacy, runtime) = temp_runtime("empty");
        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert_eq!(config.version, 1);
        assert!(config.tunnels.is_empty());
    }

    /// 主文件内容不是合法 JSON 时读取降级为空配置。
    #[tokio::test]
    async fn malformed_file_reads_as_empty() {
        let (file, _legacy, runtime) = temp_runtime("malformed");
        std::fs::write(&file, "not json at all").unwrap();
        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert!(config.tunnels.is_empty());
    }

    /// 落盘文件是合法 JSON：version 为 1，条目为驼峰键名且 updatedAt
    /// 为整数。
    #[tokio::test]
    async fn persisted_file_is_valid_json_with_version_one() {
        let (file, _legacy, runtime) = temp_runtime("persist");
        runtime
            .upsert_managed_remote_tunnel_token("id", "Name", "h.example.com", "tok")
            .await;
        let raw = std::fs::read_to_string(&file).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["tunnels"][0]["hostname"], "h.example.com");
        assert!(parsed["tunnels"][0]["updatedAt"].is_u64());
    }

    /// 读取时对磁盘脏数据去重（重复 id、仅大小写不同的 hostname）只留
    /// 首条。
    #[tokio::test]
    async fn on_disk_sanitization_dedupes_bad_entries() {
        let (file, _legacy, runtime) = temp_runtime("sanitize");
        std::fs::write(
            &file,
            serde_json::to_string(&json!({
                "version": 1,
                "tunnels": [
                    { "id": "dup", "name": "A", "hostname": "a.example.com", "token": "t" },
                    { "id": "dup", "name": "B", "hostname": "b.example.com", "token": "t" },
                    { "id": "c", "name": "C", "hostname": "A.EXAMPLE.COM", "token": "t" }
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert_eq!(config.tunnels.len(), 1);
        assert_eq!(config.tunnels[0].id, "dup");
    }
}
