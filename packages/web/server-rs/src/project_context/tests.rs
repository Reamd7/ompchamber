//! Ports of `runtime.test.js` and `routes.http.test.js`.
//!
//! Runtime tests run against a real temp projects directory with a counting
//! id factory (the JS `createId` seam). Route tests mount the real router via
//! oneshot over the same runtime — the JS http suite used fake runtimes to
//! catch missing body parsers; here the body-parser semantics are part of the
//! port, so exercising them through the real handlers is strictly stronger.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::routes;
use super::runtime::{NoteOrigin, ProjectContextRuntime, parse_plan_markdown};

const PROJECT_ID: &str = "path_dGVzdA";

static DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn temp_projects_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ompchamber-project-context-{tag}-{}-{}",
        std::process::id(),
        DIR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp projects dir");
    dir
}

fn make_runtime(dir: &Path) -> ProjectContextRuntime {
    let counter = Arc::new(StdMutex::new(0u64));
    let factory: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(move || {
        let mut value = counter.lock().unwrap_or_else(|error| error.into_inner());
        *value += 1;
        format!("plan-{value}")
    });
    ProjectContextRuntime::new(dir.to_path_buf()).with_id_factory(factory)
}

fn context_path(dir: &Path) -> PathBuf {
    dir.join(PROJECT_ID).join("context.json")
}

fn plans_dir(dir: &Path) -> PathBuf {
    dir.join(PROJECT_ID).join("plans")
}

fn legacy_config_path(dir: &Path) -> PathBuf {
    dir.join(format!("{PROJECT_ID}.json"))
}

fn write_json(path: &Path, value: &Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(
        path,
        serde_json::to_string_pretty(value).expect("serialize"),
    )
    .expect("write json");
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("read json")).expect("parse json")
}

// ---------------------------------------------------------------------------
// projectId validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rejects_traversal_and_empty_project_ids() {
    let dir = temp_projects_dir("validate");
    let runtime = make_runtime(&dir);

    let error = runtime.read_context("../escape").await.unwrap_err();
    assert!(error.to_string().contains("unsupported characters"));
    let error = runtime.read_context("a/b").await.unwrap_err();
    assert!(error.to_string().contains("unsupported characters"));
    let error = runtime.read_context("   ").await.unwrap_err();
    assert_eq!(error.to_string(), "projectId is required");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// readContext
// ---------------------------------------------------------------------------

#[tokio::test]
async fn missing_context_file_is_authoritative_empty() {
    let dir = temp_projects_dir("empty");
    let runtime = make_runtime(&dir);

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.version, 2);
    assert!(context.notes.is_empty());
    assert!(context.todos.is_empty());
    assert!(context.plans.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn malformed_stored_context_fails_instead_of_reading_empty() {
    let dir = temp_projects_dir("malformed");
    let runtime = make_runtime(&dir);
    std::fs::create_dir_all(context_path(&dir).parent().unwrap()).unwrap();
    std::fs::write(context_path(&dir), "{ not json").unwrap();

    let error = runtime.read_context(PROJECT_ID).await.unwrap_err();
    assert!(error.to_string().contains("malformed"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn drops_malformed_todo_and_plan_entries_without_failing() {
    let dir = temp_projects_dir("drops");
    let runtime = make_runtime(&dir);
    write_json(
        &context_path(&dir),
        &json!({
            "version": 2,
            "notes": [{ "id": "n1", "body": "kept", "createdAt": 1, "updatedAt": 1, "source": "manual" }],
            "todos": [
                { "id": "a", "text": "ok", "completed": false, "createdAt": 1 },
                { "id": "", "text": "no id" },
                { "text": "no id" }
            ],
            "plans": [
                { "id": "p1", "file": "a.md", "title": "A", "createdAt": 2 },
                { "id": "p2", "file": "../escape.md", "title": "Bad", "createdAt": 3 },
                { "id": "p3", "file": "no-extension", "createdAt": 4 }
            ]
        }),
    );

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(
        context
            .notes
            .iter()
            .map(|n| n.body.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
    assert_eq!(
        context
            .todos
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    assert_eq!(
        context
            .plans
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["p1"]
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn clamps_a_stored_note_body_to_the_maximum_length() {
    let dir = temp_projects_dir("clamp-note");
    let runtime = make_runtime(&dir);
    write_json(
        &context_path(&dir),
        &json!({
            "version": 2,
            "notes": [{ "id": "n1", "body": "x".repeat(5000), "createdAt": 1, "updatedAt": 1 }],
            "todos": [],
            "plans": []
        }),
    );

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.notes[0].body.len(), 3000);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn converts_a_version_1_string_note_into_one_entry() {
    let dir = temp_projects_dir("v1-string");
    let runtime = make_runtime(&dir);
    write_json(
        &context_path(&dir),
        &json!({ "version": 1, "notes": "legacy blob", "todos": [], "plans": [] }),
    );

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.notes.len(), 1);
    assert_eq!(context.notes[0].body, "legacy blob");
    assert_eq!(context.notes[0].source, "manual");
    assert!(!context.notes[0].pinned);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn an_empty_version_1_string_converts_to_no_notes() {
    let dir = temp_projects_dir("v1-empty");
    let runtime = make_runtime(&dir);
    write_json(
        &context_path(&dir),
        &json!({ "version": 1, "notes": "   ", "todos": [], "plans": [] }),
    );

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert!(context.notes.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn newest_note_is_listed_first() {
    let dir = temp_projects_dir("order-notes");
    let runtime = make_runtime(&dir);
    write_json(
        &context_path(&dir),
        &json!({
            "version": 2,
            "notes": [
                { "id": "old", "body": "old", "createdAt": 1, "updatedAt": 1 },
                { "id": "new", "body": "new", "createdAt": 9, "updatedAt": 9 }
            ],
            "todos": [],
            "plans": []
        }),
    );

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(
        context
            .notes
            .iter()
            .map(|n| n.id.as_str())
            .collect::<Vec<_>>(),
        vec!["new", "old"]
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// legacy migration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migration_moves_the_three_keys_and_preserves_the_rest() {
    let dir = temp_projects_dir("migrate");
    let runtime = make_runtime(&dir);
    std::fs::create_dir_all(plans_dir(&dir)).unwrap();
    std::fs::write(plans_dir(&dir).join("10-old.md"), "# Old plan\n\nbody here").unwrap();
    write_json(
        &legacy_config_path(&dir),
        &json!({
            "projectPath": "/tmp/test",
            "setup-worktree": ["bun install"],
            "projectActions": [{ "id": "a", "name": "Dev", "command": "bun dev" }],
            "projectNotes": "legacy notes",
            "projectTodos": [{ "id": "t1", "text": "legacy todo", "completed": true, "createdAt": 5 }],
            "projectPlanFiles": [
                { "id": "p1", "path": plans_dir(&dir).join("10-old.md"), "createdAt": 10 }
            ]
        }),
    );

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(
        context
            .notes
            .iter()
            .map(|n| n.body.as_str())
            .collect::<Vec<_>>(),
        vec!["legacy notes"]
    );
    assert_eq!(context.todos.len(), 1);
    assert_eq!(context.todos[0].id, "t1");
    assert_eq!(context.todos[0].text, "legacy todo");
    assert!(context.todos[0].completed);
    assert_eq!(context.todos[0].created_at, serde_json::Number::from(5u64));
    assert_eq!(
        context.plans,
        vec![super::runtime::PlanLink {
            id: "p1".to_string(),
            file: "10-old.md".to_string(),
            title: "Old plan".to_string(),
            created_at: serde_json::Number::from(10u64),
            pinned: false,
        }]
    );

    assert_eq!(
        read_json(&legacy_config_path(&dir)),
        json!({
            "projectPath": "/tmp/test",
            "setup-worktree": ["bun install"],
            "projectActions": [{ "id": "a", "name": "Dev", "command": "bun dev" }],
        })
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn migration_recovers_a_plan_recorded_outside_the_plans_dir() {
    let dir = temp_projects_dir("migrate-stray");
    let runtime = make_runtime(&dir);
    let stray_path = dir.join("stray.md");
    std::fs::write(&stray_path, "# Stray\n\nrecovered").unwrap();
    write_json(
        &legacy_config_path(&dir),
        &json!({
            "projectPlanFiles": [{ "id": "p1", "path": stray_path, "createdAt": 10 }]
        }),
    );

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.plans.len(), 1);
    assert_eq!(context.plans[0].file, "stray.md");
    assert_eq!(context.plans[0].title, "Stray");
    let recovered = std::fs::read_to_string(plans_dir(&dir).join("stray.md")).unwrap();
    assert!(recovered.contains("recovered"));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn migration_drops_a_link_whose_markdown_is_gone() {
    let dir = temp_projects_dir("migrate-gone");
    let runtime = make_runtime(&dir);
    write_json(
        &legacy_config_path(&dir),
        &json!({
            "projectNotes": "kept",
            "projectPlanFiles": [
                { "id": "gone", "path": plans_dir(&dir).join("missing.md"), "createdAt": 10 }
            ]
        }),
    );

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(
        context
            .notes
            .iter()
            .map(|n| n.body.as_str())
            .collect::<Vec<_>>(),
        vec!["kept"]
    );
    assert!(context.plans.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn migration_does_not_run_without_context_keys() {
    let dir = temp_projects_dir("migrate-skip");
    let runtime = make_runtime(&dir);
    write_json(
        &legacy_config_path(&dir),
        &json!({ "setup-worktree": ["bun install"] }),
    );

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.version, 2);
    assert!(context.notes.is_empty() && context.todos.is_empty() && context.plans.is_empty());
    assert!(!context_path(&dir).exists());
    assert_eq!(
        read_json(&legacy_config_path(&dir)),
        json!({ "setup-worktree": ["bun install"] })
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn migration_is_idempotent_across_repeated_reads() {
    let dir = temp_projects_dir("migrate-idempotent");
    let runtime = make_runtime(&dir);
    write_json(
        &legacy_config_path(&dir),
        &json!({ "projectNotes": "once", "projectTodos": [] }),
    );

    let first = runtime.read_context(PROJECT_ID).await.unwrap();
    let second = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(read_json(&legacy_config_path(&dir)), json!({}));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn concurrent_reads_converge_on_the_same_migrated_content() {
    let dir = temp_projects_dir("migrate-concurrent");
    let runtime = make_runtime(&dir);
    write_json(
        &legacy_config_path(&dir),
        &json!({ "projectNotes": "concurrent", "projectTodos": [] }),
    );

    let results = futures::future::join_all((0..3).map(|_| {
        let runtime = runtime.clone();
        async move { runtime.read_context(PROJECT_ID).await }
    }))
    .await;

    for result in results {
        let context = result.unwrap();
        assert_eq!(
            context
                .notes
                .iter()
                .map(|n| n.body.as_str())
                .collect::<Vec<_>>(),
            vec!["concurrent"]
        );
    }
    assert_eq!(
        read_json(&context_path(&dir))["notes"][0]["body"],
        json!("concurrent")
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// todos
// ---------------------------------------------------------------------------

#[tokio::test]
async fn todos_round_trip_through_disk() {
    let dir = temp_projects_dir("todos");
    let runtime = make_runtime(&dir);

    runtime
        .save_todos(
            PROJECT_ID,
            &json!([{ "id": "t1", "text": "do it", "completed": false, "createdAt": 1 }]),
        )
        .await
        .unwrap();

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.todos.len(), 1);
    assert_eq!(context.todos[0].id, "t1");
    assert_eq!(context.todos[0].text, "do it");
    assert!(!context.todos[0].completed);
    assert_eq!(context.todos[0].created_at, serde_json::Number::from(1u64));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn saving_todos_preserves_notes_and_plans() {
    let dir = temp_projects_dir("todos-keep");
    let runtime = make_runtime(&dir);

    let note = runtime
        .create_note(PROJECT_ID, &json!({ "body": "keep me" }))
        .await
        .unwrap();
    let plan = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "Keep me", "body": "x" }))
        .await
        .unwrap();

    runtime
        .save_todos(
            PROJECT_ID,
            &json!([{ "id": "t1", "text": "todo", "createdAt": 1 }]),
        )
        .await
        .unwrap();

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(
        context
            .notes
            .iter()
            .map(|n| n.id.as_str())
            .collect::<Vec<_>>(),
        vec![note.note.id.as_str()]
    );
    assert_eq!(
        context
            .plans
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec![plan.plan.id.as_str()]
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn concurrent_todo_writes_serialize_without_losing_the_file() {
    let dir = temp_projects_dir("todos-concurrent");
    let runtime = make_runtime(&dir);

    let writes = [
        json!([{ "id": "1", "text": "one", "createdAt": 1 }]),
        json!([{ "id": "2", "text": "two", "createdAt": 2 }]),
    ]
    .map(|todos| {
        let runtime = runtime.clone();
        async move { runtime.save_todos(PROJECT_ID, &todos).await }
    });
    futures::future::join_all(writes).await;

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.todos.len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn oversized_todo_text_is_clamped() {
    let dir = temp_projects_dir("todos-clamp");
    let runtime = make_runtime(&dir);

    runtime
        .save_todos(
            PROJECT_ID,
            &json!([{ "id": "t1", "text": "z".repeat(300), "createdAt": 1 }]),
        )
        .await
        .unwrap();

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.todos[0].text.len(), 120);

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// notes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_note_prepends_and_reports_manual_source() {
    let dir = temp_projects_dir("notes-create");
    let runtime = make_runtime(&dir);

    let first = runtime
        .create_note(PROJECT_ID, &json!({ "body": "first" }))
        .await
        .unwrap();
    let second = runtime
        .create_note(PROJECT_ID, &json!({ "body": "second" }))
        .await
        .unwrap();

    assert_eq!(first.note.source, "manual");
    assert!(!first.note.pinned);
    assert_eq!(
        second
            .context
            .notes
            .iter()
            .map(|n| n.body.as_str())
            .collect::<Vec<_>>(),
        vec!["second", "first"]
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn create_note_records_selection_provenance() {
    let dir = temp_projects_dir("notes-origin");
    let runtime = make_runtime(&dir);

    let result = runtime
        .create_note(
            PROJECT_ID,
            &json!({
                "body": "insight",
                "source": "selection",
                "origin": { "sessionId": "ses_1", "messageId": "msg_1" }
            }),
        )
        .await
        .unwrap();

    assert_eq!(result.note.source, "selection");
    assert_eq!(
        result.note.origin,
        Some(NoteOrigin {
            session_id: "ses_1".to_string(),
            message_id: Some("msg_1".to_string()),
        })
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn create_note_drops_an_origin_without_a_session() {
    let dir = temp_projects_dir("notes-origin-drop");
    let runtime = make_runtime(&dir);

    let result = runtime
        .create_note(
            PROJECT_ID,
            &json!({ "body": "x", "origin": { "messageId": "msg_1" } }),
        )
        .await
        .unwrap();
    assert_eq!(result.note.origin, None);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn create_note_rejects_an_empty_body() {
    let dir = temp_projects_dir("notes-empty");
    let runtime = make_runtime(&dir);

    let error = runtime
        .create_note(PROJECT_ID, &json!({ "body": "   " }))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "body is required");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn create_note_clamps_an_oversized_body() {
    let dir = temp_projects_dir("notes-clamp");
    let runtime = make_runtime(&dir);

    let result = runtime
        .create_note(PROJECT_ID, &json!({ "body": "y".repeat(4000) }))
        .await
        .unwrap();
    assert_eq!(result.note.body.len(), 3000);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_note_bumps_updated_at_without_touching_created_at() {
    let dir = temp_projects_dir("notes-update");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_note(PROJECT_ID, &json!({ "body": "before" }))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;

    let result = runtime
        .update_note(PROJECT_ID, &created.note.id, &json!({ "body": "after" }))
        .await
        .unwrap()
        .expect("note exists");
    assert_eq!(result.note.body, "after");
    assert_eq!(result.note.created_at, created.note.created_at);
    assert!(
        result.note.updated_at.as_f64().unwrap_or(0.0)
            > created.note.updated_at.as_f64().unwrap_or(0.0)
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn pinning_a_note_alone_leaves_body_and_updated_at_untouched() {
    let dir = temp_projects_dir("notes-pin");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_note(PROJECT_ID, &json!({ "body": "body" }))
        .await
        .unwrap();

    let result = runtime
        .update_note(PROJECT_ID, &created.note.id, &json!({ "pinned": true }))
        .await
        .unwrap()
        .expect("note exists");
    assert!(result.note.pinned);
    assert_eq!(result.note.body, "body");
    assert_eq!(result.note.updated_at, created.note.updated_at);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_note_rejects_an_empty_patch() {
    let dir = temp_projects_dir("notes-patch-empty");
    let runtime = make_runtime(&dir);
    let created = runtime
        .create_note(PROJECT_ID, &json!({ "body": "body" }))
        .await
        .unwrap();

    let error = runtime
        .update_note(PROJECT_ID, &created.note.id, &json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "body or pinned is required");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_note_rejects_blanking_the_body() {
    let dir = temp_projects_dir("notes-blank");
    let runtime = make_runtime(&dir);
    let created = runtime
        .create_note(PROJECT_ID, &json!({ "body": "body" }))
        .await
        .unwrap();

    let error = runtime
        .update_note(PROJECT_ID, &created.note.id, &json!({ "body": "  " }))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "body is required");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_note_returns_none_for_an_unknown_note() {
    let dir = temp_projects_dir("notes-unknown");
    let runtime = make_runtime(&dir);

    let result = runtime
        .update_note(PROJECT_ID, "missing", &json!({ "body": "x" }))
        .await
        .unwrap();
    assert!(result.is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn delete_note_removes_only_the_requested_note() {
    let dir = temp_projects_dir("notes-delete");
    let runtime = make_runtime(&dir);

    let keep = runtime
        .create_note(PROJECT_ID, &json!({ "body": "keep" }))
        .await
        .unwrap();
    let drop = runtime
        .create_note(PROJECT_ID, &json!({ "body": "drop" }))
        .await
        .unwrap();

    let result = runtime
        .delete_note(PROJECT_ID, &drop.note.id)
        .await
        .unwrap();
    assert!(result.deleted);
    assert_eq!(
        result
            .context
            .notes
            .iter()
            .map(|n| n.id.as_str())
            .collect::<Vec<_>>(),
        vec![keep.note.id.as_str()]
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn deleting_an_unknown_note_reports_no_deletion() {
    let dir = temp_projects_dir("notes-delete-unknown");
    let runtime = make_runtime(&dir);

    let result = runtime.delete_note(PROJECT_ID, "missing").await.unwrap();
    assert!(!result.deleted);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn create_note_refuses_to_grow_past_the_limit() {
    let dir = temp_projects_dir("notes-limit");
    let runtime = make_runtime(&dir);

    let notes: Vec<Value> = (0..200)
        .map(|index| {
            json!({ "id": format!("n{index}"), "body": format!("note {index}"), "createdAt": index, "updatedAt": index })
        })
        .collect();
    write_json(
        &context_path(&dir),
        &json!({ "version": 2, "notes": notes, "todos": [], "plans": [] }),
    );

    let error = runtime
        .create_note(PROJECT_ID, &json!({ "body": "one too many" }))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "A project can hold at most 200 notes");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn concurrent_note_creates_all_survive() {
    let dir = temp_projects_dir("notes-concurrent");
    let runtime = make_runtime(&dir);

    let bodies = ["a", "b", "c"].map(|body| {
        let runtime = runtime.clone();
        async move {
            runtime
                .create_note(PROJECT_ID, &json!({ "body": body }))
                .await
        }
    });
    futures::future::join_all(bodies).await;

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    let mut bodies: Vec<String> = context.notes.iter().map(|n| n.body.clone()).collect();
    bodies.sort();
    assert_eq!(
        bodies,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// plans
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_plan_writes_markdown_and_reads_back() {
    let dir = temp_projects_dir("plans-create");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(
            PROJECT_ID,
            &json!({ "title": "My Plan", "body": "step one" }),
        )
        .await
        .unwrap();
    let (timestamp, _) = created.plan.file.split_once('-').expect("timestamped name");
    assert!(timestamp.chars().all(|c| c.is_ascii_digit()));
    assert!(created.plan.file.ends_with("-my-plan.md"));

    let read = runtime
        .read_plan(PROJECT_ID, &created.plan.id)
        .await
        .unwrap()
        .expect("plan readable");
    assert_eq!(read.title, "My Plan");
    assert_eq!(read.body, "step one");
    assert_eq!(read.raw, "# My Plan\n\nstep one");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn newest_plan_is_listed_first() {
    let dir = temp_projects_dir("plans-order");
    let runtime = make_runtime(&dir);

    let first = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "First", "body": "a" }))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    let second = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "Second", "body": "b" }))
        .await
        .unwrap();

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(
        context
            .plans
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec![second.plan.id.as_str(), first.plan.id.as_str()]
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn reading_an_unknown_plan_returns_none() {
    let dir = temp_projects_dir("plans-unknown");
    let runtime = make_runtime(&dir);
    let result = runtime.read_plan(PROJECT_ID, "nope").await.unwrap();
    assert!(result.is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn reading_a_plan_whose_markdown_was_deleted_returns_none() {
    let dir = temp_projects_dir("plans-md-gone");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "Doomed", "body": "x" }))
        .await
        .unwrap();
    std::fs::remove_file(plans_dir(&dir).join(&created.plan.file)).unwrap();

    let result = runtime
        .read_plan(PROJECT_ID, &created.plan.id)
        .await
        .unwrap();
    assert!(result.is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn delete_plan_removes_entry_and_markdown() {
    let dir = temp_projects_dir("plans-delete");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "Bye", "body": "x" }))
        .await
        .unwrap();

    let result = runtime
        .delete_plan(PROJECT_ID, &created.plan.id)
        .await
        .unwrap();
    assert!(result.deleted);
    assert!(result.context.plans.is_empty());
    assert!(!plans_dir(&dir).join(&created.plan.file).exists());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn deleting_an_unknown_plan_keeps_state() {
    let dir = temp_projects_dir("plans-delete-unknown");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "Stay", "body": "x" }))
        .await
        .unwrap();

    let result = runtime.delete_plan(PROJECT_ID, "missing").await.unwrap();
    assert!(!result.deleted);
    assert_eq!(
        result
            .context
            .plans
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec![created.plan.id.as_str()]
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn plans_created_in_the_same_millisecond_do_not_collide() {
    let dir = temp_projects_dir("plans-collide");
    let runtime = make_runtime(&dir);

    let creates = ["a", "b"].map(|body| {
        let runtime = runtime.clone();
        async move {
            runtime
                .create_plan(PROJECT_ID, &json!({ "title": "Same", "body": body }))
                .await
                .unwrap()
        }
    });
    let created = futures::future::join_all(creates).await;

    let files: std::collections::HashSet<&str> = created
        .iter()
        .map(|result| result.plan.file.as_str())
        .collect();
    assert_eq!(files.len(), 2);
    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.plans.len(), 2);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_plan_rewrites_markdown_verbatim_and_rederives_title() {
    let dir = temp_projects_dir("plans-update");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "Old", "body": "first" }))
        .await
        .unwrap();

    let result = runtime
        .update_plan(
            PROJECT_ID,
            &created.plan.id,
            &json!({ "raw": "# New title\n\n- step\n- step two\n" }),
        )
        .await
        .unwrap()
        .expect("plan exists");
    assert_eq!(result.plan.title, "New title");
    assert_eq!(result.plan.file, created.plan.file);

    assert_eq!(
        std::fs::read_to_string(plans_dir(&dir).join(&created.plan.file)).unwrap(),
        "# New title\n\n- step\n- step two\n"
    );
    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.plans[0].title, "New title");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_plan_keeps_the_file_name_when_the_title_changes() {
    let dir = temp_projects_dir("plans-rename");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "Original", "body": "x" }))
        .await
        .unwrap();

    runtime
        .update_plan(
            PROJECT_ID,
            &created.plan.id,
            &json!({ "raw": "# Totally different\n\nx" }),
        )
        .await
        .unwrap()
        .unwrap();

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(context.plans[0].file, created.plan.file);
    assert_eq!(context.plans.len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_plan_returns_none_without_writing_anything() {
    let dir = temp_projects_dir("plans-update-unknown");
    let runtime = make_runtime(&dir);

    let result = runtime
        .update_plan(PROJECT_ID, "missing", &json!({ "raw": "# X" }))
        .await
        .unwrap();
    assert!(result.is_none());
    assert!(tokio::fs::read_dir(plans_dir(&dir)).await.is_err());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_plan_refuses_to_recreate_deleted_markdown() {
    let dir = temp_projects_dir("plans-resurrect");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "Gone", "body": "x" }))
        .await
        .unwrap();
    std::fs::remove_file(plans_dir(&dir).join(&created.plan.file)).unwrap();

    let result = runtime
        .update_plan(
            PROJECT_ID,
            &created.plan.id,
            &json!({ "raw": "# Resurrected" }),
        )
        .await
        .unwrap();
    assert!(result.is_none());
    assert!(!plans_dir(&dir).join(&created.plan.file).exists());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_plan_rejects_a_non_string_payload() {
    let dir = temp_projects_dir("plans-raw");
    let runtime = make_runtime(&dir);
    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "A", "body": "x" }))
        .await
        .unwrap();

    let error = runtime
        .update_plan(PROJECT_ID, &created.plan.id, &json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "raw is required");

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn update_plan_does_not_disturb_notes_or_todos() {
    let dir = temp_projects_dir("plans-undisturb");
    let runtime = make_runtime(&dir);

    runtime
        .create_note(PROJECT_ID, &json!({ "body": "keep me" }))
        .await
        .unwrap();
    runtime
        .save_todos(
            PROJECT_ID,
            &json!([{ "id": "t1", "text": "keep", "completed": false, "createdAt": 1 }]),
        )
        .await
        .unwrap();
    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "A", "body": "x" }))
        .await
        .unwrap();

    runtime
        .update_plan(PROJECT_ID, &created.plan.id, &json!({ "raw": "# B\n\ny" }))
        .await
        .unwrap()
        .unwrap();

    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert_eq!(
        context
            .notes
            .iter()
            .map(|n| n.body.as_str())
            .collect::<Vec<_>>(),
        vec!["keep me"]
    );
    assert_eq!(context.todos.len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn pinning_a_plan_leaves_its_title_and_file_alone() {
    let dir = temp_projects_dir("plans-pin");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "Pin me", "body": "x" }))
        .await
        .unwrap();

    let result = runtime
        .set_plan_pinned(PROJECT_ID, &created.plan.id, true)
        .await
        .unwrap()
        .expect("plan exists");
    let mut expected = created.plan.clone();
    expected.pinned = true;
    assert_eq!(result.plan, expected);
    let context = runtime.read_context(PROJECT_ID).await.unwrap();
    assert!(context.plans[0].pinned);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn pinning_an_unknown_plan_returns_none() {
    let dir = temp_projects_dir("plans-pin-unknown");
    let runtime = make_runtime(&dir);
    let result = runtime
        .set_plan_pinned(PROJECT_ID, "missing", true)
        .await
        .unwrap();
    assert!(result.is_none());

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn editing_a_plan_preserves_its_pin_state() {
    let dir = temp_projects_dir("plans-pin-edit");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "A", "body": "x" }))
        .await
        .unwrap();
    runtime
        .set_plan_pinned(PROJECT_ID, &created.plan.id, true)
        .await
        .unwrap()
        .unwrap();

    let result = runtime
        .update_plan(PROJECT_ID, &created.plan.id, &json!({ "raw": "# B\n\ny" }))
        .await
        .unwrap()
        .unwrap();
    assert!(result.plan.pinned);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn an_untitled_plan_still_produces_a_titled_markdown_file() {
    let dir = temp_projects_dir("plans-untitled");
    let runtime = make_runtime(&dir);

    let created = runtime
        .create_plan(PROJECT_ID, &json!({ "title": "", "body": "" }))
        .await
        .unwrap();
    let read = runtime
        .read_plan(PROJECT_ID, &created.plan.id)
        .await
        .unwrap()
        .expect("plan readable");
    assert_eq!(read.title, "Plan");
    assert_eq!(read.body, "");

    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// parsePlanMarkdown
// ---------------------------------------------------------------------------

#[test]
fn parse_reads_the_leading_heading_as_the_title() {
    assert_eq!(
        parse_plan_markdown("# Title\n\nbody"),
        super::runtime::ParsedPlan {
            title: "Title".to_string(),
            body: "body".to_string(),
        }
    );
}

#[test]
fn parse_falls_back_to_the_first_non_empty_line() {
    assert_eq!(
        parse_plan_markdown("\n\njust text\nmore"),
        super::runtime::ParsedPlan {
            title: "just text".to_string(),
            body: "just text\nmore".to_string(),
        }
    );
}

#[test]
fn parse_normalizes_crlf_input() {
    assert_eq!(
        parse_plan_markdown("# Title\r\n\r\nbody"),
        super::runtime::ParsedPlan {
            title: "Title".to_string(),
            body: "body".to_string(),
        }
    );
}

#[test]
fn parse_empty_input_yields_the_default_title() {
    assert_eq!(
        parse_plan_markdown(""),
        super::runtime::ParsedPlan {
            title: "Plan".to_string(),
            body: String::new(),
        }
    );
}

#[test]
fn parse_heading_requires_whitespace_after_the_hash_marks() {
    assert_eq!(
        parse_plan_markdown("## No space\n\nbody"),
        super::runtime::ParsedPlan {
            title: "No space".to_string(),
            body: "## No space\n\nbody".to_string(),
        }
    );
}

// ---------------------------------------------------------------------------
// Routes (ports of routes.http.test.js, over the real runtime)
// ---------------------------------------------------------------------------

mod routes_tests {
    use super::*;
    use axum::http::Method;
    use axum::response::Response;

    fn app(dir: &Path) -> axum::Router {
        routes::router(Arc::new(make_runtime(dir)))
    }

    async fn body_json(response: Response) -> Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json body")
    }

    fn json_request(method: Method, uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    fn plain_request(method: Method, uri: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("request")
    }

    const BASE: &str = "/api/project-context/path_dGVzdA";

    #[tokio::test]
    async fn get_returns_the_context() {
        let dir = temp_projects_dir("route-get");
        let app = app(&dir);

        let response = app.oneshot(plain_request(Method::GET, BASE)).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({ "version": 2, "notes": [], "todos": [], "plans": [] })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn put_todos_accepts_a_json_body_and_persists() {
        let dir = temp_projects_dir("route-todos");
        let app = app(&dir);

        let response = app
            .clone()
            .oneshot(json_request(
                Method::PUT,
                &format!("{BASE}/todos"),
                r#"{"todos":[{"id":"t1","text":"one"}]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let persisted = body_json(response).await;
        assert_eq!(persisted["todos"][0]["id"], json!("t1"));

        let stored = read_json(&context_path(&dir));
        assert_eq!(stored["todos"][0]["text"], json!("one"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn put_todos_rejects_a_non_json_body() {
        let dir = temp_projects_dir("route-todos-plain");
        let app = app(&dir);

        let request = Request::builder()
            .method(Method::PUT)
            .uri(format!("{BASE}/todos"))
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from(r#"{"todos":[]}"#))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "Body must be an object" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn put_todos_rejects_malformed_todo_shapes() {
        let dir = temp_projects_dir("route-todos-shape");
        let app = app(&dir);

        let response = app
            .oneshot(json_request(
                Method::PUT,
                &format!("{BASE}/todos"),
                r#"{"todos":[{"id":1,"text":"bad id type"}]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "todos must be an array of todo items" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn put_todos_rejects_syntactically_broken_json() {
        let dir = temp_projects_dir("route-todos-broken");
        let app = app(&dir);

        let response = app
            .oneshot(json_request(
                Method::PUT,
                &format!("{BASE}/todos"),
                r#"{oops"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn post_note_creates_with_provenance_and_persists() {
        let dir = temp_projects_dir("route-note-create");
        let app = app(&dir);

        let response = app
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("{BASE}/notes"),
                r#"{"body":"hello","source":"selection","origin":{"sessionId":"ses_1"}}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = body_json(response).await;
        assert_eq!(body["note"]["body"], json!("hello"));
        assert_eq!(body["note"]["source"], json!("selection"));
        assert_eq!(body["note"]["origin"], json!({ "sessionId": "ses_1" }));
        assert!(body["context"]["notes"].as_array().unwrap().len() == 1);

        let stored = read_json(&context_path(&dir));
        assert_eq!(stored["notes"][0]["body"], json!("hello"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn post_note_rejects_a_genuinely_malformed_body() {
        let dir = temp_projects_dir("route-note-malformed");
        let app = app(&dir);

        let response = app
            .oneshot(json_request(
                Method::POST,
                &format!("{BASE}/notes"),
                r#"{"notBody":"nope"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "body must be a string" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn post_note_rejects_a_non_object_body() {
        let dir = temp_projects_dir("route-note-array");
        let app = app(&dir);

        let response = app
            .oneshot(json_request(
                Method::POST,
                &format!("{BASE}/notes"),
                r#"[1,2]"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "Body must be an object" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn post_note_rejects_an_unknown_source() {
        let dir = temp_projects_dir("route-note-source");
        let app = app(&dir);

        let response = app
            .oneshot(json_request(
                Method::POST,
                &format!("{BASE}/notes"),
                r#"{"body":"hello","source":"somewhere-else"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "source must be manual, selection, or agent" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn post_note_rejects_a_non_object_origin() {
        let dir = temp_projects_dir("route-note-origin");
        let app = app(&dir);

        let response = app
            .oneshot(json_request(
                Method::POST,
                &format!("{BASE}/notes"),
                r#"{"body":"hello","origin":5}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "origin must be an object" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    async fn seed_one_note(_dir: &Path, runtime: &ProjectContextRuntime) -> String {
        runtime
            .create_note(PROJECT_ID, &json!({ "body": "seed" }))
            .await
            .unwrap()
            .note
            .id
    }

    #[tokio::test]
    async fn patch_note_pins_without_touching_the_body() {
        let dir = temp_projects_dir("route-note-pin");
        let app = app(&dir);
        let runtime = make_runtime(&dir);
        let note_id = seed_one_note(&dir, &runtime).await;

        let response = app
            .oneshot(json_request(
                Method::PATCH,
                &format!("{BASE}/notes/{note_id}"),
                r#"{"pinned":true}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["note"]["pinned"], json!(true));
        assert_eq!(body["note"]["body"], json!("seed"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn patch_note_edits_the_body() {
        let dir = temp_projects_dir("route-note-edit");
        let app = app(&dir);
        let runtime = make_runtime(&dir);
        let note_id = seed_one_note(&dir, &runtime).await;

        let response = app
            .oneshot(json_request(
                Method::PATCH,
                &format!("{BASE}/notes/{note_id}"),
                r#"{"body":"edited"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["note"]["body"], json!("edited"));
        assert!(body["note"].get("pinned").is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn patch_note_rejects_a_non_boolean_pin() {
        let dir = temp_projects_dir("route-note-pin-type");
        let app = app(&dir);

        let response = app
            .oneshot(json_request(
                Method::PATCH,
                &format!("{BASE}/notes/n1"),
                r#"{"pinned":"yes"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "pinned must be a boolean" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn patch_note_returns_404_for_an_unknown_note() {
        let dir = temp_projects_dir("route-note-404");
        let app = app(&dir);

        let response = app
            .oneshot(json_request(
                Method::PATCH,
                &format!("{BASE}/notes/nope"),
                r#"{"body":"x"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "Note not found" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn delete_note_returns_the_context_and_404_for_unknown() {
        let dir = temp_projects_dir("route-note-delete");
        let app = app(&dir);
        let runtime = make_runtime(&dir);
        let note_id = seed_one_note(&dir, &runtime).await;

        let response = app
            .clone()
            .oneshot(plain_request(
                Method::DELETE,
                &format!("{BASE}/notes/{note_id}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["notes"], json!([]));

        let response = app
            .oneshot(plain_request(Method::DELETE, &format!("{BASE}/notes/nope")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "Note not found" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    async fn seed_one_plan(_dir: &Path, runtime: &ProjectContextRuntime) -> String {
        runtime
            .create_plan(PROJECT_ID, &json!({ "title": "Seed", "body": "x" }))
            .await
            .unwrap()
            .plan
            .id
    }

    #[tokio::test]
    async fn patch_plan_pins() {
        let dir = temp_projects_dir("route-plan-pin");
        let app = app(&dir);
        let runtime = make_runtime(&dir);
        let plan_id = seed_one_plan(&dir, &runtime).await;

        let response = app
            .clone()
            .oneshot(json_request(
                Method::PATCH,
                &format!("{BASE}/plans/{plan_id}"),
                r#"{"pinned":true}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["plan"]["pinned"], json!(true));

        let response = app
            .oneshot(json_request(
                Method::PATCH,
                &format!("{BASE}/plans/{plan_id}"),
                r#"{"pinned":"yes"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "pinned must be a boolean" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn get_plan_returns_the_parsed_shape() {
        let dir = temp_projects_dir("route-plan-get");
        let app = app(&dir);
        let runtime = make_runtime(&dir);
        let plan_id = seed_one_plan(&dir, &runtime).await;

        let response = app
            .clone()
            .oneshot(plain_request(
                Method::GET,
                &format!("{BASE}/plans/{plan_id}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        let object = body.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["body", "createdAt", "file", "id", "raw", "title"]
        );
        assert_eq!(body["title"], json!("Seed"));
        assert_eq!(body["raw"], json!("# Seed\n\nx"));

        let response = app
            .oneshot(plain_request(Method::GET, &format!("{BASE}/plans/nope")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "Plan not found" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn put_plan_saves_raw_content() {
        let dir = temp_projects_dir("route-plan-put");
        let app = app(&dir);
        let runtime = make_runtime(&dir);
        let plan_id = seed_one_plan(&dir, &runtime).await;

        let response = app
            .clone()
            .oneshot(json_request(
                Method::PUT,
                &format!("{BASE}/plans/{plan_id}"),
                r##"{"raw":"# A\n\nx"}"##,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["title"], json!("A"));
        assert_eq!(body["body"], json!("x"));
        assert_eq!(body["raw"], json!("# A\n\nx"));

        let response = app
            .clone()
            .oneshot(json_request(
                Method::PUT,
                &format!("{BASE}/plans/{plan_id}"),
                r#"{"body":"wrong field"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "raw must be a string" })
        );

        let response = app
            .oneshot(json_request(
                Method::PUT,
                &format!("{BASE}/plans/nope"),
                r##"{"raw":"# B"}"##,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "Plan not found" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn post_plan_creates_and_validates() {
        let dir = temp_projects_dir("route-plan-post");
        let app = app(&dir);

        let response = app
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("{BASE}/plans"),
                r#"{"title":"A","body":"text"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = body_json(response).await;
        assert_eq!(body["plan"]["title"], json!("A"));
        assert!(body["plan"]["file"].as_str().unwrap().ends_with("-a.md"));
        assert_eq!(body["context"]["plans"].as_array().unwrap().len(), 1);

        let response = app
            .clone()
            .oneshot(json_request(
                Method::POST,
                &format!("{BASE}/plans"),
                r#"{"title":5,"body":"text"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "title must be a string" })
        );

        let response = app
            .oneshot(json_request(
                Method::POST,
                &format!("{BASE}/plans"),
                r#"{"title":"A"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "body must be a string" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn delete_plan_returns_the_context_and_404_for_unknown() {
        let dir = temp_projects_dir("route-plan-delete");
        let app = app(&dir);
        let runtime = make_runtime(&dir);
        let plan_id = seed_one_plan(&dir, &runtime).await;

        let response = app
            .clone()
            .oneshot(plain_request(
                Method::DELETE,
                &format!("{BASE}/plans/{plan_id}"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["plans"], json!([]));

        let response = app
            .oneshot(plain_request(Method::DELETE, &format!("{BASE}/plans/nope")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "Plan not found" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn traversal_project_id_is_a_client_error() {
        let dir = temp_projects_dir("route-traversal");
        let app = app(&dir);

        let response = app
            .oneshot(plain_request(
                Method::GET,
                "/api/project-context/..%2Fescape",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "projectId contains unsupported characters" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn malformed_stored_context_is_a_server_error_with_the_exact_message() {
        let dir = temp_projects_dir("route-malformed");
        let app = app(&dir);
        std::fs::create_dir_all(context_path(&dir).parent().unwrap()).unwrap();
        std::fs::write(context_path(&dir), "{ not json").unwrap();

        let response = app.oneshot(plain_request(Method::GET, BASE)).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "Stored project context is malformed" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn note_limit_surfaces_as_a_server_error() {
        let dir = temp_projects_dir("route-limit");
        let app = app(&dir);

        let notes: Vec<Value> = (0..200)
            .map(|index| {
                json!({ "id": format!("n{index}"), "body": format!("note {index}"), "createdAt": index, "updatedAt": index })
            })
            .collect();
        write_json(
            &context_path(&dir),
            &json!({ "version": 2, "notes": notes, "todos": [], "plans": [] }),
        );

        let response = app
            .oneshot(json_request(
                Method::POST,
                &format!("{BASE}/notes"),
                r#"{"body":"one too many"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            body_json(response).await,
            json!({ "error": "A project can hold at most 200 notes" })
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn oversized_body_is_rejected_as_payload_too_large() {
        let dir = temp_projects_dir("route-limit-bytes");
        let app = app(&dir);

        let big = "x".repeat(1024 * 1024 + 1);
        let response = app
            .oneshot(json_request(Method::PUT, &format!("{BASE}/todos"), &big))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);

        std::fs::remove_dir_all(&dir).ok();
    }
}
