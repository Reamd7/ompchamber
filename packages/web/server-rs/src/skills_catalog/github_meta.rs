//! Port of `server/lib/skills-catalog/github-meta.js`: best-effort GitHub
//! repository metadata (stars, last push) for catalog enrichment. Failures
//! resolve to `null`, never throw, are deduplicated in-flight, and are
//! cached — 3h on success, 5min for failures — in memory and on disk
//! (`skills-github-meta.json` in the data dir). The HTTP fetch is a
//! 1500ms-budget transport seam (JS mocks `globalThis.fetch` in tests).

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use crate::skills_catalog::disk_cache::{now_millis, read_disk_cache, write_disk_cache};

const GITHUB_API_BASE: &str = "https://api.github.com";
const CACHE_TTL_MS: u64 = 3 * 60 * 60 * 1000;
const FAILURE_CACHE_TTL_MS: u64 = 5 * 60 * 1000;
const FETCH_TIMEOUT_MS: u64 = 1_500;
const DISK_CACHE_FILE: &str = "skills-github-meta.json";
const DISK_WRITE_DELAY_MS: u64 = 1_000;

/// `{ stars, repoUpdatedAt }` — both `null` when unknown.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RepoMeta {
    pub stars: Option<i64>,
    pub repo_updated_at: Option<String>,
}

/// Transport seam for the `fetch()` call against `api.github.com`.
/// Returns `(status, body)` on the wire or an error message (timeouts,
/// network failures — the JS catch path).
pub trait MetaTransport: Send + Sync {
    fn fetch(&self, url: &str) -> BoxFuture<'static, Result<(u16, Vec<u8>), String>>;
}

/// Default transport: reqwest (rustls) with the 1500ms total budget.
#[derive(Debug, Clone)]
pub struct ReqwestMetaTransport {
    client: reqwest::Client,
}

impl Default for ReqwestMetaTransport {
    fn default() -> Self {
        ReqwestMetaTransport {
            client: reqwest::Client::builder()
                .timeout(Duration::from_millis(FETCH_TIMEOUT_MS))
                .build()
                .unwrap_or_default(),
        }
    }
}

impl MetaTransport for ReqwestMetaTransport {
    fn fetch(&self, url: &str) -> BoxFuture<'static, Result<(u16, Vec<u8>), String>> {
        let client = self.client.clone();
        let url = url.to_string();
        Box::pin(async move {
            let response = client
                .get(&url)
                .header("accept", "application/vnd.github+json")
                .send()
                .await
                .map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            let body = response.bytes().await.map_err(|error| error.to_string())?;
            Ok((status, body.to_vec()))
        })
    }
}

static DEFAULT_TRANSPORT: LazyLock<Arc<dyn MetaTransport>> =
    LazyLock::new(|| Arc::new(ReqwestMetaTransport::default()));

#[derive(Debug, Clone)]
struct MetaEntry {
    expires_at: u64,
    value: RepoMeta,
}

struct MetaState {
    meta_cache: HashMap<String, MetaEntry>,
    in_flight: HashMap<String, Shared<BoxFuture<'static, Option<RepoMeta>>>>,
    disk_loaded: bool,
    disk_write_timer: Option<tokio::task::JoinHandle<()>>,
}

static STATE: LazyLock<Mutex<MetaState>> = LazyLock::new(|| {
    Mutex::new(MetaState {
        meta_cache: HashMap::new(),
        in_flight: HashMap::new(),
        disk_loaded: false,
        disk_write_timer: None,
    })
});

fn lock_state() -> MutexGuard<'static, MetaState> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `parseMeta(payload)`: `{ stars, repoUpdatedAt }` for object payloads,
/// `None` otherwise (`stars` only when `stargazers_count` is a finite
/// number, `repoUpdatedAt` only for non-empty strings).
fn parse_meta(payload: &Value) -> Option<RepoMeta> {
    if !payload.is_object() {
        return None;
    }
    let stars = payload
        .get("stargazers_count")
        .and_then(Value::as_f64)
        .filter(|stars| stars.is_finite())
        .map(|stars| stars as i64);
    let repo_updated_at = payload
        .get("pushed_at")
        .and_then(Value::as_str)
        .filter(|pushed_at| !pushed_at.is_empty())
        .map(str::to_string);
    Some(RepoMeta {
        stars,
        repo_updated_at,
    })
}

fn load_disk_entries(state: &mut MetaState) {
    if state.disk_loaded {
        return;
    }
    state.disk_loaded = true;
    let Some(Value::Object(persisted)) = read_disk_cache(DISK_CACHE_FILE) else {
        return;
    };
    let now = now_millis();
    for (repo, entry) in persisted {
        let Some(fields) = entry.as_object() else {
            continue;
        };
        let valid = fields
            .get("expiresAt")
            .and_then(Value::as_u64)
            .is_some_and(|expires_at| expires_at > now)
            && fields.get("value").is_some_and(Value::is_object);
        if valid {
            let expires_at = fields
                .get("expiresAt")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let value = fields
                .get("value")
                .and_then(|value| serde_json::from_value::<RepoMeta>(value.clone()).ok())
                .unwrap_or_default();
            state
                .meta_cache
                .insert(repo, MetaEntry { expires_at, value });
        }
    }
}

fn schedule_disk_write(state: &mut MetaState) {
    if state.disk_write_timer.is_some() {
        return;
    }
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let task = handle.spawn(async move {
        tokio::time::sleep(Duration::from_millis(DISK_WRITE_DELAY_MS)).await;
        let mut state = lock_state();
        state.disk_write_timer = None;
        let now = now_millis();
        let mut persisted = serde_json::Map::new();
        for (repo, entry) in &state.meta_cache {
            if entry.expires_at > now {
                persisted.insert(
                    repo.clone(),
                    serde_json::json!({
                        "expiresAt": entry.expires_at,
                        "value": entry.value,
                    }),
                );
            }
        }
        write_disk_cache(DISK_CACHE_FILE, &Value::Object(persisted));
    });
    state.disk_write_timer = Some(task);
}

fn cache_repo_meta(repo: &str, value: RepoMeta, ttl_ms: u64) {
    let mut state = lock_state();
    state.meta_cache.insert(
        repo.to_string(),
        MetaEntry {
            expires_at: now_millis() + ttl_ms,
            value,
        },
    );
    schedule_disk_write(&mut state);
}

/// `fetchRepoMeta`: cached lookup, in-flight dedup, transport fetch, failure
/// caching. `None` mirrors the JS `null` returns.
async fn fetch_repo_meta(
    transport: &Arc<dyn MetaTransport>,
    normalized_repo: &str,
) -> Option<RepoMeta> {
    {
        let mut state = lock_state();
        load_disk_entries(&mut state);
        if let Some(entry) = state.meta_cache.get(normalized_repo)
            && now_millis() < entry.expires_at
        {
            return Some(entry.value.clone());
        }
    }

    let run: Shared<BoxFuture<'static, Option<RepoMeta>>> = {
        let mut state = lock_state();
        if let Some(existing) = state.in_flight.get(normalized_repo) {
            existing.clone()
        } else {
            let transport = Arc::clone(transport);
            let repo = normalized_repo.to_string();
            let future = async move {
                let url = format!("{GITHUB_API_BASE}/repos/{repo}");
                let outcome = match transport.fetch(&url).await {
                    Err(_) => {
                        cache_repo_meta(&repo, RepoMeta::default(), FAILURE_CACHE_TTL_MS);
                        None
                    }
                    Ok((status, body)) => {
                        if !(200..300).contains(&status) {
                            // Cache failures briefly so repeated catalog loads
                            // do not re-hit a rate-limited API.
                            cache_repo_meta(&repo, RepoMeta::default(), FAILURE_CACHE_TTL_MS);
                            None
                        } else {
                            match serde_json::from_slice::<Value>(&body) {
                                // response.json() throwing is the JS catch
                                // path: failure cache + null.
                                Err(_) => {
                                    cache_repo_meta(
                                        &repo,
                                        RepoMeta::default(),
                                        FAILURE_CACHE_TTL_MS,
                                    );
                                    None
                                }
                                Ok(payload) => match parse_meta(&payload) {
                                    Some(meta) => {
                                        cache_repo_meta(&repo, meta.clone(), CACHE_TTL_MS);
                                        Some(meta)
                                    }
                                    // parseMeta() === null: returned, not cached.
                                    None => None,
                                },
                            }
                        }
                    }
                };
                lock_state().in_flight.remove(&repo);
                outcome
            }
            .boxed()
            .shared();
            state
                .in_flight
                .insert(normalized_repo.to_string(), future.clone());
            future
        }
    };

    run.await
}

/// `fetchGitHubRepoMetas(normalizedRepos)`: metadata per repo (empty/null
/// entries filtered, repos deduplicated), lookups run concurrently.
pub async fn fetch_github_repo_metas(repos: &[String]) -> HashMap<String, Option<RepoMeta>> {
    fetch_github_repo_metas_with(Arc::clone(&DEFAULT_TRANSPORT), repos).await
}

/// Transport-injectable variant (the seam the JS tests reach by mocking
/// `globalThis.fetch`).
pub async fn fetch_github_repo_metas_with(
    transport: Arc<dyn MetaTransport>,
    repos: &[String],
) -> HashMap<String, Option<RepoMeta>> {
    let mut seen = HashSet::new();
    let mut unique: Vec<String> = Vec::new();
    for repo in repos {
        if !repo.is_empty() && seen.insert(repo.clone()) {
            unique.push(repo.clone());
        }
    }

    let lookups = unique.into_iter().map(|repo| {
        let transport = Arc::clone(&transport);
        async move {
            let meta = fetch_repo_meta(&transport, &repo).await;
            (repo, meta)
        }
    });
    futures::future::join_all(lookups)
        .await
        .into_iter()
        .collect()
}

/// `clearGitHubMetaCache()`: test-only in-memory cache reset (JS also latches
/// the disk loader so no disk state is re-imported afterwards).
pub fn clear_github_meta_cache() {
    let mut state = lock_state();
    state.meta_cache.clear();
    state.in_flight.clear();
    state.disk_loaded = true;
}

/// Test-only full reset (also clears the disk latch and pending writes).
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut state = lock_state();
    if let Some(timer) = state.disk_write_timer.take() {
        timer.abort();
    }
    state.meta_cache.clear();
    state.in_flight.clear();
    state.disk_loaded = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills_catalog::test_support::{EnvGuard, TEST_LOCK, unique_temp_dir};
    use serde_json::json;

    struct ScriptedTransport {
        responses: Mutex<Vec<Result<(u16, Vec<u8>), String>>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedTransport {
        fn ok(payload: Value) -> Result<(u16, Vec<u8>), String> {
            Ok((200, serde_json::to_vec(&payload).expect("serialize")))
        }

        fn status(status: u16, body: &str) -> Result<(u16, Vec<u8>), String> {
            Ok((status, body.as_bytes().to_vec()))
        }

        fn new(responses: Vec<Result<(u16, Vec<u8>), String>>) -> Arc<Self> {
            Arc::new(ScriptedTransport {
                responses: Mutex::new(responses),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn call_count(&self) -> usize {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .len()
        }
    }

    impl MetaTransport for ScriptedTransport {
        fn fetch(&self, url: &str) -> BoxFuture<'static, Result<(u16, Vec<u8>), String>> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(url.to_string());
            let response = self
                .responses
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .pop()
                .unwrap_or_else(|| Self::status(500, "no scripted response"));
            Box::pin(async move { response })
        }
    }

    // Responses are popped from the end (LIFO); reverse for FIFO scripting.
    fn scripted(responses: Vec<Result<(u16, Vec<u8>), String>>) -> Arc<ScriptedTransport> {
        let mut responses = responses;
        responses.reverse();
        ScriptedTransport::new(responses)
    }

    #[tokio::test]
    async fn returns_stars_and_pushed_at_from_the_github_api() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::ok(json!({
            "stargazers_count": 42,
            "pushed_at": "2026-08-01T00:00:00Z",
        }))]);

        let metas =
            fetch_github_repo_metas_with(transport, &["anthropics/skills".to_string()]).await;

        assert_eq!(
            metas.get("anthropics/skills"),
            Some(&Some(RepoMeta {
                stars: Some(42),
                repo_updated_at: Some("2026-08-01T00:00:00Z".to_string()),
            }))
        );
        clear_github_meta_cache();
    }

    #[tokio::test]
    async fn resolves_failed_lookups_to_null_without_throwing() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::status(403, "rate limited")]);

        let metas =
            fetch_github_repo_metas_with(transport, &["anthropics/skills".to_string()]).await;

        assert_eq!(metas.get("anthropics/skills"), Some(&None));
        clear_github_meta_cache();
    }

    #[tokio::test]
    async fn caches_failed_lookups_briefly_to_avoid_repeat_hits() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::status(403, "rate limited")]);

        fetch_github_repo_metas_with(transport.clone(), &["anthropics/skills".to_string()]).await;
        let second =
            fetch_github_repo_metas_with(transport.clone(), &["anthropics/skills".to_string()])
                .await;

        assert_eq!(transport.call_count(), 1, "failure should be cached");
        assert_eq!(
            second.get("anthropics/skills"),
            Some(&Some(RepoMeta::default()))
        );
        clear_github_meta_cache();
    }

    #[tokio::test]
    async fn deduplicates_repositories_and_filters_empty() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::ok(json!({
            "stargazers_count": 1,
            "pushed_at": null,
        }))]);

        let metas = fetch_github_repo_metas_with(
            transport,
            &["a/b".to_string(), "a/b".to_string(), String::new()],
        )
        .await;

        assert_eq!(metas.len(), 1, "one unique non-empty repo: {metas:?}");
        assert_eq!(
            metas.get("a/b"),
            Some(&Some(RepoMeta {
                stars: Some(1),
                repo_updated_at: None,
            }))
        );
        clear_github_meta_cache();
    }

    #[tokio::test]
    async fn transport_errors_are_failure_cached() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![Err("network down".to_string())]);

        let metas = fetch_github_repo_metas_with(transport, &["a/b".to_string()]).await;
        assert_eq!(metas.get("a/b"), Some(&None));
        assert_eq!(
            lock_state().meta_cache.get("a/b").map(|entry| &entry.value),
            Some(&RepoMeta::default()),
            "failure entry cached"
        );
        clear_github_meta_cache();
    }

    #[tokio::test]
    async fn non_object_json_payloads_return_null_without_caching() {
        let _guard = TEST_LOCK.lock().await;
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::ok(json!([1, 2, 3]))]);

        let metas = fetch_github_repo_metas_with(transport, &["a/b".to_string()]).await;
        assert_eq!(metas.get("a/b"), Some(&None));
        assert!(
            lock_state().meta_cache.get("a/b").is_none(),
            "parseMeta() null must not be cached"
        );
        clear_github_meta_cache();
    }

    #[tokio::test]
    async fn persists_entries_to_disk_and_reloads_them() {
        let _guard = TEST_LOCK.lock().await;
        let dir = unique_temp_dir("github-meta-test");
        let _env = EnvGuard::set("OMPCHAMBER_DATA_DIR", dir.to_str().expect("utf8"));
        reset_for_tests();

        let transport = scripted(vec![ScriptedTransport::ok(json!({
            "stargazers_count": 7,
            "pushed_at": "2026-01-01T00:00:00Z",
        }))]);
        fetch_github_repo_metas_with(transport, &["a/b".to_string()]).await;
        tokio::time::sleep(Duration::from_millis(1_200)).await;

        let raw = std::fs::read_to_string(dir.join("skills-github-meta.json")).expect("disk file");
        let on_disk: Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(
            on_disk.pointer("/a~1b/value/stars"),
            Some(&json!(7)),
            "slash-escaped repo key with stars: {raw}"
        );

        // A later "process" (fresh memory, same disk) reuses the entry
        // without hitting the transport.
        reset_for_tests();
        let transport = scripted(vec![ScriptedTransport::status(500, "should not be called")]);
        let metas = fetch_github_repo_metas_with(transport, &["a/b".to_string()]).await;
        assert_eq!(
            metas.get("a/b"),
            Some(&Some(RepoMeta {
                stars: Some(7),
                repo_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
            }))
        );
        clear_github_meta_cache();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_meta_tolerates_missing_and_non_numeric_fields() {
        assert_eq!(
            parse_meta(&json!({ "stargazers_count": "many", "pushed_at": "" })),
            Some(RepoMeta {
                stars: None,
                repo_updated_at: None
            })
        );
        assert_eq!(parse_meta(&json!("scalar")), None);
        assert_eq!(parse_meta(&Value::Null), None);
    }
}
