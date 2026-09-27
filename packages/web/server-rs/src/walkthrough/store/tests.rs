//! Tests ported from `server/lib/walkthrough/store.test.js`.
//!
//! 中文说明：walkthrough 磁盘缓存 Store 的单元测试。覆盖缓存键
//! build_cache_key 的稳定性与区分度、缓存条目与 pointer 的读写往返、
//! 损坏或版本不兼容条目降级为 miss、LRU 数量上限淘汰，以及按仓库
//! 存在性修剪 pointer 的行为。

use std::collections::HashMap;

use serde_json::{Value, json};

use super::{CacheEntry, Pointer, Store, build_cache_key};
use crate::walkthrough::digest::{DigestFile, Section, build_digest};
use crate::walkthrough::hunks::ParsedHunk;

/// 创建本用例专属的临时目录作为 Store 的 data_dir（进程 id、纳秒时间戳
/// 加原子计数器保证唯一），调用方负责在用例末尾清理。
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

/// 生成一个测试用 DigestFile：路径、状态与 hunk id 列表由参数指定，
/// 其余字段（scope=working、非二进制、行号计数等）取固定值；hunk 的
/// patch 与 body 留空——本套测试的缓存键只随路径、状态与 hunk id 变化。
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

/// 构造基准文件集：src/a.ts（modified，2 个 hunk）与 src/b.ts（added，
/// 1 个 hunk），作为缓存键测试中"相同输入"的基线。
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

/// 生成一个完整的 CacheEntry：walkthrough 版本 1、时间戳与仓库信息固定，
/// model/language 与基准键的维度一致，walkthrough 本体为单章节占位。
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

/// 契约：同一输入（仓库、source、模型、语言、文件集）两次构建的缓存键相同。
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

/// 契约：文件列表顺序不影响缓存键——diff 语义与文件枚举顺序无关。
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

/// 契约：任一文件的任一 hunk id 变化都会改变缓存键，
/// 保证代码改动后不会命中旧的 walkthrough。
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

/// 契约：仓库路径、source key、model id、provider id、语言五项中任意
/// 一项不同都产生不同的缓存键，本用例逐一对比断言。
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

/// 契约：缓存键是 64 个十六进制字符（SHA-256 摘要）。
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

/// 契约：写入的缓存条目能按原键读回，标题等字段与写入时一致。
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

/// 契约：读取不存在的键返回 None（缓存 miss），而不是报错。
#[test]
fn reports_a_miss_for_an_unknown_key() {
    let data_dir = temp_data_dir("miss");
    let store = Store::new(&data_dir);
    assert!(store.read_cached_walkthrough(&"0".repeat(64)).is_none());
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 契约：条目文件损坏（非法 JSON）时按 miss 处理返回 None，
/// 不让读取路径失败。
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

/// 契约：条目内的 walkthroughVersion 与当前版本不符时拒绝读回（视为
/// miss），防止旧格式数据流入新代码。
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

/// 契约：写入采用原子替换，完成后 entries 目录不残留 .tmp 临时文件。
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

/// 契约：写入超过数量上限（200 条）后，entries 目录中的 .json 条目数
/// 不超过上限——最久未使用的条目被淘汰。
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

/// 契约：pointer 写入后可按同一仓库与 source 读回；换 source key 读取
/// 则 miss——pointer 按 source 隔离，互不串扰。
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

/// 契约：不同仓库路径各自持有独立 pointer，写入互不覆盖。
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

/// 契约：prune_missing_repositories 只删除仓库目录确实消失的 pointer
/// 并返回删除数；仍存在的仓库（临时目录）不受影响。
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

/// 契约：仓库路径因权限等原因无法 stat（并非不存在）时，prune 不删除
/// 对应 pointer——无论权限是否生效，两种分支都保留 pointer。
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

/// 契约：pointer 文件损坏时 prune 跳过它而不是 panic 或报错。
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

/// 契约：service 实际用 build_digest 的产物构造缓存键——同一 diff 两次
/// 构建得到相同键，且 hunk 别名 h1 到真实 id 的映射稳定。
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
