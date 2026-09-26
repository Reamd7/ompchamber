//! Tests ported from `server/lib/walkthrough/store.test.js`.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{CacheEntry, Pointer, Store, build_cache_key};
use crate::walkthrough::digest::{DigestFile, Section, build_digest};
use crate::walkthrough::hunks::ParsedHunk;

fn temp_data_dir(label: &str) -> std::path::PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "walkthrough-store-{label}-{}-{}",
        std::process::id(),
        now + unique as u128,
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn digest_file(path: &str, status: &str, hunk_ids: &[&str]) -> DigestFile {
    DigestFile {
        path: path.to_string(),
        old_path: None,
        status: status.to_string(),
        scope: "working".to_string(),
        binary: false,
        generated: false,
        hunks: hunk_ids
            .iter()
            .map(|id| ParsedHunk {
                id: id.to_string(),
                header: "@@ -1,1 +1,2 @@".to_string(),
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 2,
                added: 1,
                deleted: 0,
                patch: String::new(),
                body: String::new(),
            })
            .collect(),
    }
}

fn base_files() -> Vec<DigestFile> {
    vec![
        digest_file(
            "src/a.ts",
            "modified",
            &["working:src/a.ts:aaaa1111", "working:src/a.ts:bbbb2222"],
        ),
        digest_file("src/b.ts", "added", &["working:src/b.ts:cccc3333"]),
    ]
}

fn entry(cache_key: &str) -> CacheEntry {
    CacheEntry {
        walkthrough_version: 1,
        cache_key: Some(cache_key.to_string()),
        generated_at: "2026-08-02T00:00:00.000Z".to_string(),
        repo_root: Some("/repo".to_string()),
        source_key: Some("working-tree:all".to_string()),
        model: Some(json!({
            "providerID": "anthropic",
            "modelID": "claude-haiku-4-5",
        })),
        language: Some("en".to_string()),
        walkthrough: json!({
            "title": "x",
            "focus": "",
            "chapters": [{ "id": "chapter-1", "stops": [] }],
        }),
    }
}

#[test]
fn build_cache_key_is_stable_for_identical_input() {
    let a = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &base_files(),
    );
    let b = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &base_files(),
    );
    assert_eq!(a, b);
}

#[test]
fn build_cache_key_ignores_file_ordering() {
    let original = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &base_files(),
    );
    let mut reordered = base_files();
    reordered.reverse();
    let reordered = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &reordered,
    );
    assert_eq!(reordered, original);
}

#[test]
fn build_cache_key_changes_when_any_hunk_changes() {
    let original = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &base_files(),
    );
    let mut files = base_files();
    files[0] = digest_file(
        "src/a.ts",
        "modified",
        &["working:src/a.ts:aaaa1111", "working:src/a.ts:dddd4444"],
    );
    let edited = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &files,
    );
    assert_ne!(edited, original);
}

#[test]
fn build_cache_key_separates_repositories_sources_models_and_languages() {
    let original = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &base_files(),
    );
    assert_ne!(
        build_cache_key(
            "/other",
            "working-tree:all",
            "anthropic",
            "claude-haiku-4-5",
            "en",
            &base_files()
        ),
        original
    );
    assert_ne!(
        build_cache_key(
            "/repo",
            "working-tree:staged",
            "anthropic",
            "claude-haiku-4-5",
            "en",
            &base_files()
        ),
        original
    );
    assert_ne!(
        build_cache_key(
            "/repo",
            "working-tree:all",
            "anthropic",
            "other-model",
            "en",
            &base_files()
        ),
        original
    );
    assert_ne!(
        build_cache_key(
            "/repo",
            "working-tree:all",
            "google",
            "claude-haiku-4-5",
            "en",
            &base_files()
        ),
        original
    );
    assert_ne!(
        build_cache_key(
            "/repo",
            "working-tree:all",
            "anthropic",
            "claude-haiku-4-5",
            "uk",
            &base_files()
        ),
        original
    );
}

#[test]
fn build_cache_key_produces_a_hex_sha256() {
    let key = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &[digest_file("src/a.ts", "modified", &["h"])],
    );
    assert_eq!(key.len(), 64);
    assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn cache_entries_round_trip() {
    let data_dir = temp_data_dir("entries");
    let store = Store::new(&data_dir);
    let key = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &base_files(),
    );
    assert!(store.write_cached_walkthrough(&key, &entry(&key)));

    let read = store.read_cached_walkthrough(&key).expect("entry present");
    assert_eq!(read.walkthrough["title"], json!("x"));
    assert_eq!(read.cache_key.as_deref(), Some(key.as_str()));
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn reports_a_miss_for_an_unknown_key() {
    let data_dir = temp_data_dir("miss");
    let store = Store::new(&data_dir);
    assert!(store.read_cached_walkthrough(&"0".repeat(64)).is_none());
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn treats_a_corrupt_entry_as_a_miss_rather_than_failing() {
    let data_dir = temp_data_dir("corrupt");
    let store = Store::new(&data_dir);
    let key = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &base_files(),
    );
    store.write_cached_walkthrough(&key, &entry(&key));
    std::fs::write(
        store.entries_dir().join(format!("{key}.json")),
        "{ not json",
    )
    .unwrap();

    assert!(store.read_cached_walkthrough(&key).is_none());
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn rejects_an_entry_written_by_an_incompatible_version() {
    let data_dir = temp_data_dir("version");
    let store = Store::new(&data_dir);
    let key = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &base_files(),
    );
    store.write_cached_walkthrough(&key, &entry(&key));
    let file = store.entries_dir().join(format!("{key}.json"));
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let mut bumped = stored.clone();
    bumped["walkthroughVersion"] = json!(999);
    std::fs::write(&file, serde_json::to_string(&bumped).unwrap()).unwrap();

    assert!(store.read_cached_walkthrough(&key).is_none());
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn leaves_no_temp_files_behind() {
    let data_dir = temp_data_dir("tmp");
    let store = Store::new(&data_dir);
    let key = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "en",
        &base_files(),
    );
    store.write_cached_walkthrough(&key, &entry(&key));

    let leftovers: Vec<_> = std::fs::read_dir(store.entries_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|name| name.contains(".tmp"))
        })
        .collect();
    assert!(leftovers.is_empty());
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn evicts_least_recently_used_entries_past_the_count_limit() {
    let data_dir = temp_data_dir("evict");
    let store = Store::new(&data_dir);
    const MAX_ENTRIES: usize = 200;

    for index in 0..(MAX_ENTRIES + 10) {
        let key = build_cache_key(
            "/repo",
            &format!("source-{index}"),
            "anthropic",
            "claude-haiku-4-5",
            "en",
            &base_files(),
        );
        store.write_cached_walkthrough(&key, &entry(&key));
    }

    let remaining = std::fs::read_dir(store.entries_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(".json"))
        })
        .count();
    assert!(remaining <= MAX_ENTRIES);
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn pointers_round_trip_and_stay_scoped_to_their_source() {
    let data_dir = temp_data_dir("pointers");
    let store = Store::new(&data_dir);
    store.write_pointer(
        "/repo",
        "working-tree:all",
        &Pointer {
            repo_root: Some("/repo".to_string()),
            source_key: Some("working-tree:all".to_string()),
            cache_key: "abc".to_string(),
            generated_at: Some("now".to_string()),
        },
    );

    let read = store
        .read_pointer("/repo", "working-tree:all")
        .expect("pointer present");
    assert_eq!(read.cache_key, "abc");
    assert!(store.read_pointer("/repo", "working-tree:staged").is_none());
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn keeps_different_repositories_apart() {
    let data_dir = temp_data_dir("repo-apart");
    let store = Store::new(&data_dir);
    store.write_pointer(
        "/repo-a",
        "working-tree:all",
        &Pointer {
            repo_root: Some("/repo-a".to_string()),
            source_key: None,
            cache_key: "a".to_string(),
            generated_at: None,
        },
    );
    store.write_pointer(
        "/repo-b",
        "working-tree:all",
        &Pointer {
            repo_root: Some("/repo-b".to_string()),
            source_key: None,
            cache_key: "b".to_string(),
            generated_at: None,
        },
    );

    assert_eq!(
        store
            .read_pointer("/repo-a", "working-tree:all")
            .unwrap()
            .cache_key,
        "a"
    );
    assert_eq!(
        store
            .read_pointer("/repo-b", "working-tree:all")
            .unwrap()
            .cache_key,
        "b"
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[tokio::test]
async fn prunes_only_pointers_whose_repository_is_gone() {
    let data_dir = temp_data_dir("prune");
    let store = Store::new(&data_dir);
    let live_repo = temp_data_dir("live-repo");
    store.write_pointer(
        live_repo.to_str().unwrap(),
        "working-tree:all",
        &Pointer {
            repo_root: Some(live_repo.to_str().unwrap().to_string()),
            source_key: None,
            cache_key: "live".to_string(),
            generated_at: None,
        },
    );
    store.write_pointer(
        "/definitely/not/here",
        "working-tree:all",
        &Pointer {
            repo_root: Some("/definitely/not/here".to_string()),
            source_key: None,
            cache_key: "dead".to_string(),
            generated_at: None,
        },
    );

    assert_eq!(store.prune_missing_repositories().await, 1);
    assert!(
        store
            .read_pointer(live_repo.to_str().unwrap(), "working-tree:all")
            .is_some()
    );
    assert!(
        store
            .read_pointer("/definitely/not/here", "working-tree:all")
            .is_none()
    );
    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&live_repo);
}

#[tokio::test]
async fn keeps_a_pointer_whose_repository_is_merely_unreachable() {
    let data_dir = temp_data_dir("unreachable");
    let store = Store::new(&data_dir);
    // A path we cannot stat for a reason other than absence — an unplugged
    // drive or a dead share behaves this way. Deleting then would cost the
    // user walkthroughs for a repository that still exists.
    let blocked = temp_data_dir("blocked");
    let inner = blocked.join("inner");
    let inaccessible = inner.join("repo");
    std::fs::create_dir_all(&inaccessible).unwrap();

    store.write_pointer(
        inaccessible.to_str().unwrap(),
        "working-tree:all",
        &Pointer {
            repo_root: Some(inaccessible.to_str().unwrap().to_string()),
            source_key: None,
            cache_key: "blocked".to_string(),
            generated_at: None,
        },
    );

    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(&inner).unwrap().permissions();
    permissions.set_mode(0o000);
    let restricted = std::fs::set_permissions(&inner, permissions).is_ok();

    let removed = store.prune_missing_repositories().await;
    let kept = store
        .read_pointer(inaccessible.to_str().unwrap(), "working-tree:all")
        .is_some();

    let mut permissions = std::fs::metadata(&inner).unwrap().permissions();
    permissions.set_mode(0o755);
    let _ = std::fs::set_permissions(&inner, permissions);

    // When the platform ignores the permission bit (root), the stat succeeds
    // and the pointer is kept anyway — both branches keep the pointer.
    if restricted {
        assert_eq!(removed, 0);
    }
    assert!(kept);
    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&blocked);
}

#[tokio::test]
async fn survives_a_pointer_file_it_cannot_parse() {
    let data_dir = temp_data_dir("broken-pointer");
    let store = Store::new(&data_dir);
    std::fs::create_dir_all(store.pointers_dir()).unwrap();
    std::fs::write(store.pointers_dir().join("broken.json"), "{ not json").unwrap();

    // A broken pointer is skipped, not fatal — the prune resolves.
    let _removed = store.prune_missing_repositories().await;
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn build_cache_key_agrees_with_a_real_digest_build() {
    // The service builds keys from `build_digest` output; a stable mapping
    // between the same diff and the same key is the cache's contract.
    let patch = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,1 +1,2 @@\n+const added = true;\n";
    let built = build_digest(&[Section {
        scope: "working".to_string(),
        patch: patch.to_string(),
    }]);
    let first = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "uk",
        &built.files,
    );
    let second = build_cache_key(
        "/repo",
        "working-tree:all",
        "anthropic",
        "claude-haiku-4-5",
        "uk",
        &build_digest(&[Section {
            scope: "working".to_string(),
            patch: patch.to_string(),
        }])
        .files,
    );
    assert_eq!(first, second);
    assert_eq!(built.files.len(), 1);
    assert_eq!(built.files[0].hunks.len(), 1);
    assert_eq!(
        built.id_by_alias,
        HashMap::from([("h1".to_string(), built.files[0].hunks[0].id.clone())])
    );
}
