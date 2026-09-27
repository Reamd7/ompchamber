//! Port of `server/lib/tunnels/managed-config.js`: managed remote tunnel
//! token/preset persistence runtime. File paths and the persistence mutex are
//! injected (the JS receives them from `server/index.js` wiring).

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::settings::normalization::normalize_managed_remote_tunnel_hostname;

pub const CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRemoteTunnelEntry {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub token: String,
    pub updated_at: u64,
}

impl ManagedRemoteTunnelEntry {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRemoteTunnelConfig {
    pub version: u64,
    pub tunnels: Vec<ManagedRemoteTunnelEntry>,
}

impl ManagedRemoteTunnelConfig {
    pub fn empty() -> Self {
        Self {
            version: CLOUDFLARE_MANAGED_REMOTE_TUNNELS_VERSION,
            tunnels: Vec::new(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "version": self.version,
            "tunnels": self.tunnels.iter().map(ManagedRemoteTunnelEntry::to_json).collect::<Vec<_>>(),
        })
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn finite_number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

/// `sanitizeManagedRemoteTunnelConfigEntries`: drops entries missing any
/// required field, normalizes hostnames, dedupes by id and hostname.
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

pub struct ManagedTunnelConfigRuntime {
    file_path: PathBuf,
    legacy_file_path: PathBuf,
    /// `persistManagedRemoteTunnelConfigLock`: serializes read-modify-write.
    persist_lock: tokio::sync::Mutex<()>,
}

impl ManagedTunnelConfigRuntime {
    pub fn new(file_path: PathBuf, legacy_file_path: PathBuf) -> Self {
        Self {
            file_path,
            legacy_file_path,
            persist_lock: tokio::sync::Mutex::new(()),
        }
    }

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
#[allow(dead_code)]
pub(crate) async fn write_private_file_for_tests(
    path: &std::path::Path,
    body: &str,
) -> std::io::Result<()> {
    write_private_file(path, body).await
}

/// Unique directory under the system temp dir (JS `fs.mkdtempSync(prefix)`).
pub fn make_temp_dir(prefix: &str) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn missing_files_read_as_empty() {
        let (_file, _legacy, runtime) = temp_runtime("empty");
        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert_eq!(config.version, 1);
        assert!(config.tunnels.is_empty());
    }

    #[tokio::test]
    async fn malformed_file_reads_as_empty() {
        let (file, _legacy, runtime) = temp_runtime("malformed");
        std::fs::write(&file, "not json at all").unwrap();
        let config = runtime.read_managed_remote_tunnel_config_from_disk().await;
        assert!(config.tunnels.is_empty());
    }

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
