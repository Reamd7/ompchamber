//! Port of `server/lib/opencode/npm-registry.js` — npm package metadata
//! lookups with a 1h TTL cache and in-flight request deduplication.
//!
//! JS source map: `lookupNpmPackage` → [`lookup_npm_package`],
//! `getNpmInfo` → [`get_npm_info`], `clearCache` → [`clear_cache`]. The JS
//! module is a library (no routes of its own); its consumers in the JS tree
//! are the plugin/npm status routes, which the `opencode_plugins` module
//! ports with its own copy of this client. This copy serves the
//! `opencode_meta` module surface.
//!
//! Divergence vs the JS (noted for PORT-MANIFEST): duplicate concurrent
//! lookups for the same name wait on a per-name mutex and then hit the cache
//! instead of sharing one in-flight request (same wire result, one request
//! either way), and the network error text is reqwest's rather than Node's.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{Map, Value};

use super::http::{HttpFetch, HttpRequest, default_fetch};

const NPM_CACHE_TTL_MS: u64 = 3_600_000;
const NPM_FETCH_TIMEOUT_MS: u64 = 5_000;
pub(crate) const NPM_REGISTRY_BASE: &str = "https://registry.npmjs.org";

/// JS `NpmLookupResult` — success payload or lookup error.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NpmInfo {
    Ok {
        /// `distTags.latest ?? null`.
        latest: Option<String>,
        versions: Vec<String>,
        dist_tags: Map<String, Value>,
    },
    Err {
        /// JS `status`: numeric HTTP status or the literal `'network'`.
        status: NpmStatus,
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NpmStatus {
    Code(u16),
    Network,
}

impl NpmInfo {
    /// JS wire shape (`{ ok, latest, versions, distTags }` /
    /// `{ ok, status, error }`).
    pub(crate) fn to_value(&self) -> Value {
        match self {
            NpmInfo::Ok {
                latest,
                versions,
                dist_tags,
            } => serde_json::json!({
                "ok": true,
                "latest": latest,
                "versions": versions,
                "distTags": Value::Object(dist_tags.clone()),
            }),
            NpmInfo::Err { status, error } => match status {
                NpmStatus::Code(code) => serde_json::json!({
                    "ok": false,
                    "status": code,
                    "error": error,
                }),
                NpmStatus::Network => serde_json::json!({
                    "ok": false,
                    "status": "network",
                    "error": error,
                }),
            },
        }
    }

    /// JS `cacheResult`: only definitive answers (success or 404) are cached.
    fn is_definitive(&self) -> bool {
        match self {
            NpmInfo::Ok { .. } => true,
            NpmInfo::Err {
                status: NpmStatus::Code(404),
                ..
            } => true,
            NpmInfo::Err { .. } => false,
        }
    }
}

/// Injectable transport + registry base (tests point at canned responses).
pub(crate) struct NpmIo {
    pub fetch: HttpFetch,
    pub registry_base: String,
}

impl Default for NpmIo {
    fn default() -> Self {
        Self {
            fetch: default_fetch(),
            registry_base: NPM_REGISTRY_BASE.to_string(),
        }
    }
}

struct CacheEntry {
    fetched_at: u64,
    payload: NpmInfo,
}

/// JS module state: `_cache`, `_inFlight`, `_userAgent`.
static CACHE: LazyLock<Mutex<HashMap<String, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static INFLIGHT: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static USER_AGENT: LazyLock<String> = LazyLock::new(|| {
    let pkg_path = web_package_root().join("package.json");
    match std::fs::read_to_string(&pkg_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    {
        Some(pkg) if pkg.get("version").and_then(Value::as_str).is_some() => {
            format!(
                "ompchamber-server/{}",
                pkg["version"].as_str().unwrap_or_default()
            )
        }
        _ => "ompchamber-server/dev".to_string(),
    }
});

/// JS `_getPackageJsonPath()` resolves to the web package's package.json.
fn web_package_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// JS `encodeName`: `encodeURIComponent(name).replace(/^%40/, '@')`.
pub(crate) fn encode_name(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len());
    for &byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            )
        {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    match encoded.strip_prefix("%40") {
        Some(rest) => format!("@{rest}"),
        None => encoded,
    }
}

/// JS `parseDistTags`: string-valued entries of an object (arrays dropped).
fn parse_dist_tags(value: Option<&Value>) -> Map<String, Value> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Map::new();
    };
    map.iter()
        .filter(|(_, value)| value.is_string())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// JS `parseVersions`: keys of an object.
fn parse_versions(value: Option<&Value>) -> Vec<String> {
    let Some(map) = value.and_then(Value::as_object) else {
        return Vec::new();
    };
    map.keys().cloned().collect()
}

/// JS `lookupNpmPackage` — one direct registry fetch (never fails; errors
/// come back as `NpmInfo::Err`).
pub(crate) async fn lookup_npm_package(io: &NpmIo, name: &str) -> NpmInfo {
    let url = format!("{}/{}", io.registry_base, encode_name(name));
    let request = HttpRequest::get(url)
        .header("User-Agent", USER_AGENT.as_str())
        .header("Accept", "application/json")
        .timeout(NPM_FETCH_TIMEOUT_MS);
    match (io.fetch)(request).await {
        Ok(response) if response.ok() => match serde_json::from_slice::<Value>(&response.body) {
            Ok(data) => {
                let dist_tags = parse_dist_tags(data.get("dist-tags"));
                let latest = dist_tags
                    .get("latest")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                NpmInfo::Ok {
                    latest,
                    versions: parse_versions(data.get("versions")),
                    dist_tags,
                }
            }
            // JS: a body parse failure lands in the catch → 'network'.
            Err(error) => NpmInfo::Err {
                status: NpmStatus::Network,
                error: error.to_string(),
            },
        },
        Ok(response) if response.status == 404 => NpmInfo::Err {
            status: NpmStatus::Code(404),
            error: "Package not found".to_string(),
        },
        Ok(response) => NpmInfo::Err {
            status: NpmStatus::Code(response.status),
            error: format!("Registry returned {}", response.status),
        },
        Err(error) => NpmInfo::Err {
            status: NpmStatus::Network,
            error: error.message(),
        },
    }
}

/// JS `getNpmInfo(name, { forceRefresh })` — TTL cache + per-name request
/// serialization.
pub(crate) async fn get_npm_info(io: &NpmIo, name: &str, force_refresh: bool) -> NpmInfo {
    let now = now_ms();
    if !force_refresh {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(name)
            && now.saturating_sub(entry.fetched_at) < NPM_CACHE_TTL_MS
        {
            return entry.payload.clone();
        }
    }

    let guard = {
        let mut inflight = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(inflight.entry(name.to_string()).or_default())
    };
    let _serial = guard.lock().await;
    // Another caller may have finished the lookup while we waited.
    if !force_refresh {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = cache.get(name)
            && now_ms().saturating_sub(entry.fetched_at) < NPM_CACHE_TTL_MS
        {
            return entry.payload.clone();
        }
    }

    let result = lookup_npm_package(io, name).await;
    if result.is_definitive() {
        CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(
            name.to_string(),
            CacheEntry {
                fetched_at: now_ms(),
                payload: result.clone(),
            },
        );
    }
    result
}

/// JS `clearCache` (test seam).
pub(crate) fn clear_cache() {
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    INFLIGHT.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
pub(crate) mod testing {
    /// Force a cache entry with an explicit `fetchedAt` (TTL expiry tests).
    pub(crate) fn set_cache_entry(name: &str, fetched_at: u64, payload: super::NpmInfo) {
        super::CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                name.to_string(),
                super::CacheEntry {
                    fetched_at,
                    payload,
                },
            );
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
