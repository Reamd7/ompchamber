//! Tests porting `server/lib/projects/project-config.test.js` plus the
//! acceptance targets: JSONC-tolerant parse + broken-layer isolation,
//! unknown-field round-trips, worktree-root resolution via an injectable git
//! runner (see `worktree.rs`), and project-id derivation (`project_id.rs`).
//!
//! （中文说明）移植 `server/lib/projects/project-config.test.js`，并覆盖验收
//! 目标：JSONC 容错解析与损坏层隔离、未知字段往返保留、经可注入 git runner
//! 的 worktree 根解析（见 `worktree.rs`）、以及项目 id 派生（`project_id.rs`）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use super::runtime::ProjectConfigRuntime;
use super::{Execution, LoopEntry, Schedule, ScheduledTask};

/// 临时目录命名用的全局递增计数器：让同进程内并发测试各自拿到唯一目录。
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 测试用临时目录守卫（元组字段即目录路径）：创建时生成唯一路径，Drop 时递归删除。
struct TempDir(PathBuf);

/// 临时目录的构造与路径访问。
impl TempDir {
    /// 在系统临时目录下创建带 tag、进程 id 与递增计数的唯一目录；创建失败即 panic。
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "oc-projects-{}-{}-{}",
            tag,
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).expect("temp dir");
        TempDir(path)
    }

    /// 返回临时目录的路径。
    fn path(&self) -> &Path {
        &self.0
    }
}

/// 测试结束（含 panic 展开）时自动清理临时目录。
impl Drop for TempDir {
    /// 递归删除临时目录，忽略删除失败（不阻塞测试收尾）。
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 构建指向 `dir` 的测试 runtime，并把任务 id 工厂固定为 `task-fixed-id` 便于断言。
fn test_runtime(dir: &Path) -> ProjectConfigRuntime {
    ProjectConfigRuntime::new(dir.to_path_buf())
        .with_task_id_factory(Arc::new(|| "task-fixed-id".to_string()))
}

/// 生成一个每日 09:30（UTC）执行的示例任务 JSON。
fn daily_task(name: &str) -> Value {
    json!({
        "name": name,
        "enabled": true,
        "schedule": { "kind": "daily", "time": "09:30", "timezone": "UTC" },
        "execution": { "prompt": "Summarize", "providerID": "openai", "modelID": "gpt-4.1" },
    })
}

/// 生成名为 `name` 的 markdown loop 条目（cron `0 9 * * *`，UTC）。
fn loop_entry(name: &str) -> LoopEntry {
    LoopEntry {
        scope: "project".to_string(),
        file_path: format!("/repo/.agents/loops/{name}.md"),
        definition: Some(json!({
            "name": name,
            "enabled": true,
            "schedule": { "kind": "cron", "cron": "0 9 * * *", "timezone": "UTC" },
            "execution": {
                "prompt": format!("Loop prompt for {name}"),
                "providerID": "openai",
                "modelID": "gpt-4.1",
            },
        })),
    }
}

/// 读取并解析磁盘上的项目配置 JSON；失败即 panic（仅供测试断言使用）。
fn read_stored(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// 断言任务为 daily 调度并取出其 times 列表，否则 panic。
fn daily_times(task: &ScheduledTask) -> &[String] {
    match &task.schedule {
        Schedule::Daily { times, .. } => times,
        other => panic!("expected daily schedule, got {:?}", other.kind()),
    }
}

/// 新建任务会持久化，重新读取后 id、名称、时区与触发时间保持一致。
#[tokio::test]
async fn creates_and_persists_a_scheduled_task() {
    let temp = TempDir::new("create");
    let runtime = test_runtime(temp.path());
    let result = runtime
        .upsert_scheduled_task("project-test", &daily_task("Nightly digest"))
        .await
        .unwrap();

    assert!(result.created);
    assert_eq!(result.task.id, "task-fixed-id");
    let reloaded = runtime.list_scheduled_tasks("project-test").await.unwrap();
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].name, "Nightly digest");
    assert_eq!(reloaded[0].schedule.timezone(), "UTC");
    assert_eq!(daily_times(&reloaded[0]), &["09:30".to_string()]);
}

/// 非法 cron 表达式被拒绝，错误信息指向 `schedule.cron`。
#[tokio::test]
async fn rejects_invalid_cron_expressions() {
    let temp = TempDir::new("cron-invalid");
    let runtime = test_runtime(temp.path());
    let error = runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "Invalid cron task",
                "enabled": true,
                "schedule": { "kind": "cron", "cron": "invalid cron", "timezone": "UTC" },
                "execution": { "prompt": "Run checks", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("schedule.cron is invalid"),
        "got: {error}"
    );
}

/// 合法 cron 被接受，而永不匹配的日期（2 月 31 日）被判定为非法。
#[tokio::test]
async fn accepts_valid_cron_and_rejects_impossible_dates() {
    let temp = TempDir::new("cron-valid");
    let runtime = test_runtime(temp.path());
    runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "Valid cron",
                "enabled": true,
                "schedule": { "kind": "cron", "cron": "0 9 * * *", "timezone": "UTC" },
                "execution": { "prompt": "Run", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();

    let impossible = runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "Feb 31",
                "enabled": true,
                "schedule": { "kind": "cron", "cron": "0 0 31 2 *", "timezone": "UTC" },
                "execution": { "prompt": "Run", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap_err();
    assert!(impossible.to_string().contains("schedule.cron is invalid"));
}

/// 写入任务时保留配置文件里本构建不认识的所有顶层键（notes/todos/actions 等）。
#[tokio::test]
async fn preserves_unknown_project_config_keys_when_writing() {
    let temp = TempDir::new("preserve-keys");
    let runtime = test_runtime(temp.path());
    let project_id = "path_preserve";
    let file_path = runtime.resolve_project_config_path(project_id).unwrap();
    std::fs::create_dir_all(temp.path()).unwrap();
    std::fs::write(
        &file_path,
        serde_json::to_string_pretty(&json!({
            "projectNotes": "hello notes",
            "projectTodos": [{ "id": "t1", "text": "buy milk", "completed": false, "createdAt": 1 }],
            "projectActions": [{ "id": "a1", "name": "Run", "command": "bun run dev" }],
            "projectActionsPrimaryId": "a1",
            "setup-worktree": ["bun install"],
            "projectPlanFiles": [{ "id": "p1", "path": "/tmp/plans/p1.md", "createdAt": 2 }],
            "projectPath": "/tmp/demo",
        }))
        .unwrap(),
    )
    .unwrap();

    runtime
        .upsert_scheduled_task(project_id, &daily_task("nightly"))
        .await
        .unwrap();

    let raw = read_stored(&file_path);
    assert_eq!(raw["projectNotes"], "hello notes");
    assert_eq!(raw["projectTodos"][0]["text"], "buy milk");
    assert_eq!(raw["projectActions"].as_array().unwrap().len(), 1);
    assert_eq!(raw["projectActionsPrimaryId"], "a1");
    assert_eq!(raw["setup-worktree"], json!(["bun install"]));
    assert_eq!(raw["projectPlanFiles"][0]["id"], "p1");
    assert_eq!(raw["projectPath"], "/tmp/demo");
    assert_eq!(raw["scheduledTasks"].as_array().unwrap().len(), 1);
    assert_eq!(raw["version"], 1);
}

/// 读取列表不改动已存储的 state 时间戳（createdAt/updatedAt 原样往返）。
#[tokio::test]
async fn preserves_scheduled_task_state_timestamps_when_listing() {
    let temp = TempDir::new("timestamps");
    let runtime = test_runtime(temp.path());
    let project_id = "timestamp_preserve";
    let file_path = runtime.resolve_project_config_path(project_id).unwrap();
    std::fs::write(
        &file_path,
        serde_json::to_string_pretty(&json!({
            "scheduledTasks": [{
                "id": "task-existing",
                "name": "nightly",
                "enabled": true,
                "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "UTC" },
                "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
                "state": { "createdAt": 10, "updatedAt": 20, "lastStatus": "idle" },
            }],
        }))
        .unwrap(),
    )
    .unwrap();

    let first = runtime.list_scheduled_tasks(project_id).await.unwrap();
    let second = runtime.list_scheduled_tasks(project_id).await.unwrap();
    assert_eq!(first[0].state.created_at, 10);
    assert_eq!(first[0].state.updated_at, 20);
    assert_eq!(second[0].state.updated_at, 20);
}

/// `modelRole=default` 的执行配置无需显式 providerID/modelID 即可保存。
#[tokio::test]
async fn persists_model_role_default_tasks_without_explicit_identifiers() {
    let temp = TempDir::new("model-role");
    let runtime = test_runtime(temp.path());
    runtime
        .upsert_scheduled_task(
            "model_role_default",
            &json!({
                "name": "follows-default-role",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "09:00", "timezone": "UTC" },
                "execution": { "prompt": "run", "modelRole": "default" },
            }),
        )
        .await
        .unwrap();

    let tasks = runtime
        .list_scheduled_tasks("model_role_default")
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].execution.model_role.as_deref(), Some("default"));
    assert!(tasks[0].execution.provider_id.is_none());
    assert!(tasks[0].execution.model_id.is_none());
}

/// 既无 providerID 又无 `modelRole=default` 的执行配置被拒绝。
#[tokio::test]
async fn rejects_identifier_free_execution_without_model_role_default() {
    let temp = TempDir::new("model-missing");
    let runtime = test_runtime(temp.path());
    let error = runtime
        .upsert_scheduled_task(
            "model_role_missing",
            &json!({
                "name": "no-model",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "09:00", "timezone": "UTC" },
                "execution": { "prompt": "run" },
            }),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("execution.providerID is required")
    );
}

/// 半指定模型（只有 providerID 缺 modelID）被拒绝。
#[tokio::test]
async fn rejects_half_pinned_models() {
    let temp = TempDir::new("model-half");
    let runtime = test_runtime(temp.path());
    let error = runtime
        .upsert_scheduled_task(
            "model_role_half",
            &json!({
                "name": "half-model",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "09:00", "timezone": "UTC" },
                "execution": { "prompt": "run", "providerID": "openai" },
            }),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("execution.modelID is required"));
}

/// once 调度接受 date+time+timezone 并原样保留。
#[tokio::test]
async fn accepts_one_time_schedule_with_date_and_time() {
    let temp = TempDir::new("once");
    let runtime = test_runtime(temp.path());
    let result = runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "One-time review",
                "enabled": true,
                "schedule": { "kind": "once", "date": "2026-04-20", "time": "13:45", "timezone": "Europe/Kyiv" },
                "execution": { "prompt": "Create a release summary", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();
    match &result.task.schedule {
        Schedule::Once {
            date,
            time,
            timezone,
        } => {
            assert_eq!(date, "2026-04-20");
            assert_eq!(time, "13:45");
            assert_eq!(timezone, "Europe/Kyiv");
        }
        other => panic!("expected once schedule, got {}", other.kind()),
    }
}

/// once 调度拒绝不存在的日期（2 月 30 日）与非法时间（24:00）。
#[tokio::test]
async fn rejects_impossible_once_dates_and_times() {
    let temp = TempDir::new("once-bad");
    let runtime = test_runtime(temp.path());
    let base = json!({
        "name": "bad once",
        "enabled": true,
        "schedule": { "kind": "once", "date": "2026-02-30", "time": "10:00", "timezone": "UTC" },
        "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
    });
    let error = runtime
        .upsert_scheduled_task("project-test", &base)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("schedule.date must be YYYY-MM-DD for once schedule")
    );

    let bad_time = json!({
        "name": "bad once",
        "enabled": true,
        "schedule": { "kind": "once", "date": "2026-02-28", "time": "24:00", "timezone": "UTC" },
        "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
    });
    let error = runtime
        .upsert_scheduled_task("project-test", &bad_time)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("schedule.time must be HH:mm for once schedule")
    );
}

/// JSONC（行/块注释与尾逗号）配置可读取，且随后的写入保留未知键。
#[tokio::test]
async fn jsonc_tolerant_parse_accepts_comments_and_trailing_commas() {
    let temp = TempDir::new("jsonc");
    let runtime = test_runtime(temp.path());
    let project_id = "jsonc-tolerant";
    let file_path = runtime.resolve_project_config_path(project_id).unwrap();
    std::fs::write(
        &file_path,
        r#"{
  // hand-edited project config
  "projectNotes": "noted", /* block comment */
  "scheduledTasks": [
    {
      "id": "task-existing",
      "name": "nightly",
      "enabled": true,
      "schedule": { "kind": "daily", "times": ["09:00",], "timezone": "UTC", },
      "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
      "state": { "createdAt": 10, "updatedAt": 20, "lastStatus": "idle", },
    },
  ],
}"#,
    )
    .unwrap();

    let tasks = runtime.list_scheduled_tasks(project_id).await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, "task-existing");
    assert_eq!(tasks[0].state.created_at, 10);

    // A write over the tolerant file keeps the unknown keys.
    runtime
        .upsert_scheduled_task(project_id, &daily_task("added"))
        .await
        .unwrap();
    let raw = read_stored(&file_path);
    assert_eq!(raw["projectNotes"], "noted");
    assert_eq!(raw["scheduledTasks"].as_array().unwrap().len(), 2);
}

/// 配置文件损坏时读、写两条路径都报错，绝不静默清空或覆盖磁盘上的坏文件。
#[tokio::test]
async fn a_broken_config_file_is_an_isolated_error_never_silently_empty() {
    let temp = TempDir::new("broken-file");
    let runtime = test_runtime(temp.path());
    let file_path = runtime.resolve_project_config_path("broken").unwrap();
    std::fs::write(&file_path, "{ \"scheduledTasks\": [").unwrap();

    let list_error = runtime.list_scheduled_tasks("broken").await.unwrap_err();
    assert!(
        list_error
            .to_string()
            .contains("failed to parse project config")
    );
    // The write path also refuses to flush an empty config over the broken
    // file: the read inside the write errors out first.
    let upsert_error = runtime
        .upsert_scheduled_task("broken", &daily_task("x"))
        .await
        .unwrap_err();
    assert!(
        upsert_error
            .to_string()
            .contains("failed to parse project config")
    );
    // And the broken bytes are still on disk.
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "{ \"scheduledTasks\": ["
    );
}

/// 单条损坏任务被隔离跳过，合法兄弟任务照常读取且写回时保持原始字节。
#[tokio::test]
async fn a_broken_task_is_skipped_in_isolation_without_blocking_valid_siblings() {
    let temp = TempDir::new("broken-task");
    let runtime = test_runtime(temp.path());
    let file_path = runtime.resolve_project_config_path("isolate").unwrap();
    std::fs::write(
        &file_path,
        serde_json::to_string_pretty(&json!({
            "scheduledTasks": [
                { "id": "broken", "name": "no schedule at all" },
                {
                    "id": "good",
                    "name": "good task",
                    "enabled": true,
                    "schedule": { "kind": "daily", "times": ["08:00"], "timezone": "UTC" },
                    "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
                    "state": { "createdAt": 5, "updatedAt": 6, "lastStatus": "idle" }
                }
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    let tasks = runtime.list_scheduled_tasks("isolate").await.unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, "good");
    // A write keeps the valid task and its stored bytes verbatim.
    runtime
        .upsert_scheduled_task(
            "isolate",
            &json!({
                "id": "added",
                "name": "added task",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "09:00", "timezone": "UTC" },
                "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();
    let raw = read_stored(&file_path);
    let stored_good = raw["scheduledTasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == "good")
        .unwrap();
    assert_eq!(stored_good["state"]["createdAt"], 5);
}

/// 无效 IANA 时区与越界 weekday（0-6 之外）均被拒绝。
#[tokio::test]
async fn rejects_invalid_timezones_and_weekdays() {
    let temp = TempDir::new("tz");
    let runtime = test_runtime(temp.path());
    let error = runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "bad tz",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "09:00", "timezone": "Not/A Zone" },
                "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("schedule.timezone must be a valid IANA timezone")
    );

    let error = runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "bad weekdays",
                "enabled": true,
                "schedule": { "kind": "weekly", "time": "09:00", "weekdays": [0, 9], "timezone": "UTC" },
                "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("schedule.weekdays must include values from 0 to 6 for weekly schedule")
    );
}

/// weekly 调度对 weekdays 去重排序、times 去重排序，并合并旧版单数 `time` 字段。
#[tokio::test]
async fn weekly_weekdays_dedupe_and_sort_and_times_merge_with_legacy_time() {
    let temp = TempDir::new("weekly");
    let runtime = test_runtime(temp.path());
    runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "weekly",
                "enabled": true,
                "schedule": {
                    "kind": "weekly",
                    "times": ["10:00", "09:00", "10:00"],
                    "time": "23:30",
                    "weekdays": [3, 1, 3, 0],
                    "timezone": "UTC"
                },
                "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();
    let tasks = runtime.list_scheduled_tasks("project-test").await.unwrap();
    match &tasks[0].schedule {
        Schedule::Weekly {
            times, weekdays, ..
        } => {
            assert_eq!(
                times,
                &[
                    "09:00".to_string(),
                    "10:00".to_string(),
                    "23:30".to_string()
                ]
            );
            assert_eq!(weekdays, &[0u8, 1, 3]);
        }
        other => panic!("expected weekly schedule, got {}", other.kind()),
    }
}

/// 更新任务时调度里缺省的 times/timezone 回退沿用已存在的值。
#[tokio::test]
async fn schedule_updates_fall_back_to_existing_times_and_timezone() {
    let temp = TempDir::new("fallback");
    let runtime = test_runtime(temp.path());
    let created = runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "fallback",
                "enabled": true,
                "schedule": { "kind": "daily", "times": ["09:00"], "timezone": "Europe/Kyiv" },
                "execution": { "prompt": "run", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();

    // Re-save the same task with a schedule that carries no times/timezone:
    // the existing values carry over.
    let mut patch = serde_json::to_value(&created.task).unwrap();
    patch["schedule"] = json!({ "kind": "daily" });
    let updated = runtime
        .upsert_scheduled_task("project-test", &patch)
        .await
        .unwrap();
    match &updated.task.schedule {
        Schedule::Daily { times, timezone } => {
            assert_eq!(times, &["09:00".to_string()]);
            assert_eq!(timezone, "Europe/Kyiv");
        }
        other => panic!("expected daily schedule, got {}", other.kind()),
    }
}

/// 删除已存在任务返回 `deleted=true` 且列表同步；删除不存在的任务返回 `deleted=false`。
#[tokio::test]
async fn delete_removes_a_task_and_reports_missing_ones() {
    let temp = TempDir::new("delete");
    let runtime = test_runtime(temp.path());
    runtime
        .upsert_scheduled_task("project-test", &daily_task("one"))
        .await
        .unwrap();
    runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "id": "other-task",
                "name": "Other",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "10:00", "timezone": "UTC" },
                "execution": { "prompt": "Other", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();

    let deleted = runtime
        .delete_scheduled_task("project-test", "other-task")
        .await
        .unwrap();
    assert!(deleted.deleted);
    assert_eq!(deleted.tasks.len(), 1);
    assert_eq!(deleted.tasks[0].name, "one");

    let missing = runtime
        .delete_scheduled_task("project-test", "nope")
        .await
        .unwrap();
    assert!(!missing.deleted);
}

/// 项目 id 空白或含非法字符报错；带空白的 id 清洗后与原 id 解析到同一文件。
#[tokio::test]
async fn task_id_validation_and_sanitization_errors() {
    let temp = TempDir::new("sanitize");
    let runtime = test_runtime(temp.path());
    let error = runtime.list_scheduled_tasks("   ").await.unwrap_err();
    assert!(error.to_string().contains("projectId is required"));
    let error = runtime.list_scheduled_tasks("a/b").await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("projectId contains unsupported characters")
    );
    // Trimmed ids resolve to the same file.
    let path_a = runtime.resolve_project_config_path("proj").unwrap();
    let path_b = runtime.resolve_project_config_path("  proj ").unwrap();
    assert_eq!(path_a, path_b);
    assert!(path_a.ends_with("proj.json"));
}

// ============== loop reconciliation (JS `project-config loop reconciliation`) ==============

/// 新发现的 loop 生成确定性 id（`loop:<scope>:<name>`）并持久化，state.createdAt > 0。
#[tokio::test]
async fn creates_tasks_for_discovered_loops_with_deterministic_ids() {
    let temp = TempDir::new("loop-create");
    let runtime = test_runtime(temp.path());
    let tasks = runtime
        .reconcile_loop_tasks(
            "project-test",
            &[loop_entry("daily-digest"), loop_entry("weekly-report")],
        )
        .await
        .unwrap();

    assert_eq!(tasks.len(), 2);
    let digest = tasks
        .iter()
        .find(|task| task.name == "daily-digest")
        .unwrap();
    assert_eq!(digest.id, "loop:project:daily-digest");
    assert_eq!(digest.execution.provider_id.as_deref(), Some("openai"));
    assert_eq!(
        digest.loop_file.as_deref(),
        Some("/repo/.agents/loops/daily-digest.md")
    );

    let reloaded = runtime.list_scheduled_tasks("project-test").await.unwrap();
    assert_eq!(reloaded.len(), 2);
    assert!(reloaded[0].state.created_at > 0);
}

/// 同名 JSON 任务被 loop 收养：保留原 id 与运行 state，定义以 loop 为准。
#[tokio::test]
async fn adopts_an_existing_task_by_name_preserving_id_and_state() {
    let temp = TempDir::new("loop-adopt");
    let runtime = test_runtime(temp.path());
    let created = runtime
        .upsert_scheduled_task("project-test", &daily_task("daily-digest"))
        .await
        .unwrap();

    let first = runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    let adopted = first
        .iter()
        .find(|task| task.id == created.task.id)
        .unwrap();
    assert_eq!(adopted.name, "daily-digest");
    assert_eq!(
        adopted.loop_file.as_deref(),
        Some("/repo/.agents/loops/daily-digest.md")
    );

    runtime
        .update_scheduled_task_state(
            "project-test",
            &adopted.id,
            &json!({ "nextRunAt": 123456, "lastRunAt": 111, "lastStatus": "success" }),
        )
        .await
        .unwrap();

    let second = runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    let again = second
        .iter()
        .find(|task| task.id == created.task.id)
        .unwrap();
    assert_eq!(again.state.next_run_at, Some(123456));
    assert_eq!(again.state.last_run_at, Some(111));
    assert_eq!(again.state.last_status, "success");
    assert_eq!(
        again.loop_file.as_deref(),
        Some("/repo/.agents/loops/daily-digest.md")
    );
}

/// 驱动文件被移除后，loop 来源的任务在下一次 reconcile 时被取消调度。
#[tokio::test]
async fn unschedules_a_loop_sourced_task_when_its_file_is_removed() {
    let temp = TempDir::new("loop-remove");
    let runtime = test_runtime(temp.path());
    runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    let tasks = runtime
        .reconcile_loop_tasks("project-test", &[])
        .await
        .unwrap();
    assert!(tasks.is_empty());
    assert!(
        runtime
            .list_scheduled_tasks("project-test")
            .await
            .unwrap()
            .is_empty()
    );
}

/// 无匹配 loop 时，纯 JSON 配置的任务在 reconcile 后保持不动。
#[tokio::test]
async fn leaves_json_configured_tasks_untouched_when_no_loop_matches() {
    let temp = TempDir::new("loop-untouched");
    let runtime = test_runtime(temp.path());
    let created = runtime
        .upsert_scheduled_task("project-test", &daily_task("json-only"))
        .await
        .unwrap();
    let tasks = runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("loop-only")])
        .await
        .unwrap();
    assert_eq!(tasks.len(), 2);
    assert!(tasks.iter().any(|task| task.id == created.task.id));
    assert!(tasks.iter().any(|task| task.name == "loop-only"));
}

/// 被同名 loop 收养过的任务，在 loop 文件消失后随之被移除。
#[tokio::test]
async fn removes_a_task_after_a_loop_with_the_same_name_adopted_then_vanished() {
    let temp = TempDir::new("loop-vanish");
    let runtime = test_runtime(temp.path());
    let created = runtime
        .upsert_scheduled_task("project-test", &daily_task("daily-digest"))
        .await
        .unwrap();
    runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    let after_removal = runtime
        .reconcile_loop_tasks("project-test", &[])
        .await
        .unwrap();
    assert!(after_removal.iter().all(|task| task.id != created.task.id));
}

/// 非法 loop 定义被跳过（仅告警），不影响合法 loop 任务的生成。
#[tokio::test]
async fn skips_invalid_loop_definitions_without_blocking_valid_ones() {
    let temp = TempDir::new("loop-invalid");
    let runtime = test_runtime(temp.path());
    let mut bad = loop_entry("bad-loop");
    bad.definition = Some(json!({
        "name": "bad-loop",
        "enabled": true,
        "schedule": { "kind": "cron", "cron": "not a cron", "timezone": "UTC" },
        "execution": { "prompt": "nope", "providerID": "openai", "modelID": "gpt-4.1" },
    }));
    let tasks = runtime
        .reconcile_loop_tasks("project-test", &[bad, loop_entry("good-loop")])
        .await
        .unwrap();
    let names: Vec<&str> = tasks.iter().map(|task| task.name.as_str()).collect();
    assert_eq!(names, ["good-loop"]);
}

/// loop 改名时任务原位更新：id 不变，名称跟随 loop 新名。
#[tokio::test]
async fn renames_a_loop_sourced_task_in_place_when_the_loop_name_changes() {
    let temp = TempDir::new("loop-rename");
    let runtime = test_runtime(temp.path());
    let first = runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    let original = &first[0];

    let renamed_entry = LoopEntry {
        scope: "project".to_string(),
        file_path: "/repo/.agents/loops/daily-digest.md".to_string(),
        definition: Some(json!({
            "name": "digest",
            "enabled": true,
            "schedule": { "kind": "cron", "cron": "0 9 * * *", "timezone": "UTC" },
            "execution": { "prompt": "Loop prompt for digest", "providerID": "openai", "modelID": "gpt-4.1" },
        })),
    };
    let renamed = runtime
        .reconcile_loop_tasks("project-test", &[renamed_entry])
        .await
        .unwrap();
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0].id, original.id);
    assert_eq!(renamed[0].name, "digest");
    assert_eq!(
        renamed[0].loop_file.as_deref(),
        Some("/repo/.agents/loops/daily-digest.md")
    );
}

/// UI 对 loop 任务的改名在下一次 reconcile 时被 loop 定义覆盖回原名。
#[tokio::test]
async fn reverts_a_ui_rename_of_a_loop_task_back_to_the_loop_name() {
    let temp = TempDir::new("loop-ui-rename");
    let runtime = test_runtime(temp.path());
    runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    let created = runtime
        .list_scheduled_tasks("project-test")
        .await
        .unwrap()
        .remove(0);

    runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "id": created.id,
                "name": "renamed-by-ui",
                "enabled": true,
                "loopFile": created.loop_file,
                "schedule": { "kind": "daily", "time": "09:30", "timezone": "UTC" },
                "execution": { "prompt": "UI prompt", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();

    let after = runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, created.id);
    assert_eq!(after[0].name, "daily-digest");
    assert_eq!(after[0].execution.prompt, "Loop prompt for daily-digest");
}

/// loop 文件存在但暂不可解析（definition 为 None）时保留任务及其运行 state。
#[tokio::test]
async fn keeps_a_loop_sourced_task_while_its_file_exists_but_is_unparseable() {
    let temp = TempDir::new("loop-unparseable");
    let runtime = test_runtime(temp.path());
    let first = runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    let original = &first[0];
    runtime
        .update_scheduled_task_state(
            "project-test",
            &original.id,
            &json!({ "nextRunAt": 123456, "lastRunAt": 111, "lastStatus": "success" }),
        )
        .await
        .unwrap();

    let unparseable = LoopEntry {
        scope: "project".to_string(),
        file_path: "/repo/.agents/loops/daily-digest.md".to_string(),
        definition: None,
    };
    let after = runtime
        .reconcile_loop_tasks("project-test", &[unparseable])
        .await
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, original.id);
    assert_eq!(
        after[0].loop_file.as_deref(),
        Some("/repo/.agents/loops/daily-digest.md")
    );
    assert_eq!(after[0].state.next_run_at, Some(123456));
    assert_eq!(after[0].state.last_status, "success");
}

/// 同一 loop 文件的孤儿重复任务在 reconcile 时被清除。
#[tokio::test]
async fn unschedules_orphan_duplicates_of_the_same_loop_file() {
    let temp = TempDir::new("loop-orphan");
    let runtime = test_runtime(temp.path());
    let first = runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    let original = &first[0];
    runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "id": "zombie-copy",
                "name": "daily-digest-copy",
                "enabled": true,
                "loopFile": "/repo/.agents/loops/daily-digest.md",
                "schedule": { "kind": "daily", "time": "09:30", "timezone": "UTC" },
                "execution": { "prompt": "Stale copy", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();

    let after = runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].id, original.id);
    assert!(!after.iter().any(|task| task.id == "zombie-copy"));
}

/// 收养 JSON 任务时保留 UI 写入的 execution 扩展字段（variant/goal 等），prompt 以 loop 为准。
#[tokio::test]
async fn preserves_ui_only_execution_fields_when_adopting_a_json_task() {
    let temp = TempDir::new("loop-preserve");
    let runtime = test_runtime(temp.path());
    let created = runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "daily-digest",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "09:30", "timezone": "UTC" },
                "execution": {
                    "prompt": "JSON prompt",
                    "providerID": "openai",
                    "modelID": "gpt-4.1",
                    "variant": "fast",
                    "goalEnabled": true,
                    "goalTokenBudget": 20000,
                    "permissionAutoAccept": true
                }
            }),
        )
        .await
        .unwrap();

    let adopted = runtime
        .reconcile_loop_tasks("project-test", &[loop_entry("daily-digest")])
        .await
        .unwrap();
    let task = adopted
        .iter()
        .find(|entry| entry.id == created.task.id)
        .unwrap();
    assert_eq!(task.execution.prompt, "Loop prompt for daily-digest");
    assert_eq!(task.execution.variant.as_deref(), Some("fast"));
    assert_eq!(task.execution.goal_enabled, Some(true));
    assert_eq!(task.execution.goal_token_budget, Some(20000));
    assert_eq!(task.execution.permission_auto_accept, Some(true));
}

// ============== fields this build does not know ==============

/// 造一个带"未来字段"的任务：先正常写入，再在磁盘原始 JSON 上手工注入本构建
/// 不认识的 execution/state/顶层字段，返回任务 id 与配置文件路径。
async fn seed_foreign_task(runtime: &ProjectConfigRuntime) -> (String, PathBuf) {
    let created = runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "name": "Nightly digest",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "09:30", "timezone": "UTC" },
                "execution": { "prompt": "Summarize", "providerID": "openai", "modelID": "gpt-4.1", "goalEnabled": true },
            }),
        )
        .await
        .unwrap();
    let file_path = runtime.resolve_project_config_path("project-test").unwrap();
    let mut stored = read_stored(&file_path);
    stored["scheduledTasks"][0]["execution"]["futureExecutionField"] = json!("keep me");
    stored["scheduledTasks"][0]["state"]["futureStateField"] = json!(42);
    stored["scheduledTasks"][0]["futureTopLevelField"] = json!(true);
    std::fs::write(&file_path, serde_json::to_string_pretty(&stored).unwrap()).unwrap();
    (created.task.id.clone(), file_path)
}

/// 从磁盘配置按 id 取出单个任务的原始 JSON；不存在即 panic。
fn read_stored_task(file_path: &Path, id: &str) -> Value {
    let stored = read_stored(file_path);
    stored["scheduledTasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["id"] == id)
        .cloned()
        .expect("stored task present")
}

/// 状态更新与条件认领更新都不会丢失任务上的未来（未知）字段。
#[tokio::test]
async fn foreign_fields_survive_a_state_update_and_the_claim_update() {
    let temp = TempDir::new("foreign-state");
    let runtime = test_runtime(temp.path());
    let (id, file_path) = seed_foreign_task(&runtime).await;

    runtime
        .update_scheduled_task_state(
            "project-test",
            &id,
            &json!({ "lastStatus": "success", "lastRunAt": 1000 }),
        )
        .await
        .unwrap();
    let stored = read_stored_task(&file_path, &id);
    assert_eq!(stored["execution"]["futureExecutionField"], "keep me");
    assert_eq!(stored["execution"]["goalEnabled"], true);
    assert_eq!(stored["futureTopLevelField"], true);
    assert_eq!(stored["state"]["lastStatus"], "success");

    runtime
        .update_scheduled_task_state_if(
            "project-test",
            &id,
            &|_task: &ScheduledTask| true,
            &json!({ "lastScheduledFor": 5000 }),
        )
        .await
        .unwrap();
    let stored = read_stored_task(&file_path, &id);
    assert_eq!(stored["execution"]["futureExecutionField"], "keep me");
    assert_eq!(stored["state"]["lastScheduledFor"], 5000);
}

/// 新增/删除其他任务或空 reconcile 时，未触碰任务上的未来字段原样保留。
#[tokio::test]
async fn foreign_fields_survive_writes_that_touch_other_tasks() {
    let temp = TempDir::new("foreign-other");
    let runtime = test_runtime(temp.path());
    let (id, file_path) = seed_foreign_task(&runtime).await;

    let other = runtime
        .upsert_scheduled_task(
            "project-test",
            &json!({
                "id": "other-task",
                "name": "Other",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "10:00", "timezone": "UTC" },
                "execution": { "prompt": "Other", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        read_stored_task(&file_path, &id)["execution"]["futureExecutionField"],
        "keep me"
    );

    runtime
        .delete_scheduled_task("project-test", &other.task.id)
        .await
        .unwrap();
    assert_eq!(
        read_stored_task(&file_path, &id)["execution"]["futureExecutionField"],
        "keep me"
    );

    runtime
        .reconcile_loop_tasks("project-test", &[])
        .await
        .unwrap();
    assert_eq!(
        read_stored_task(&file_path, &id)["execution"]["futureExecutionField"],
        "keep me"
    );
}

/// 仅当任务本身被显式保存（upsert）时，其上的未来字段才被重新序列化丢弃。
#[tokio::test]
async fn foreign_fields_are_dropped_only_when_the_task_itself_is_saved() {
    let temp = TempDir::new("foreign-save");
    let runtime = test_runtime(temp.path());
    let (id, file_path) = seed_foreign_task(&runtime).await;

    let tasks = runtime.list_scheduled_tasks("project-test").await.unwrap();
    let mut renamed = serde_json::to_value(&tasks[0]).unwrap();
    renamed["name"] = json!("Renamed");
    runtime
        .upsert_scheduled_task("project-test", &renamed)
        .await
        .unwrap();

    let stored = read_stored_task(&file_path, &id);
    assert_eq!(stored["name"], "Renamed");
    assert!(stored["execution"].get("futureExecutionField").is_none());
}

// ============== conditional state updates (occurrence claim) ==============

/// 条件状态更新只在谓词通过时写盘；第二次同值认领不产生写入。
#[tokio::test]
async fn conditionally_updates_state_only_when_the_predicate_passes() {
    let temp = TempDir::new("claim");
    let runtime = test_runtime(temp.path());
    let created = runtime
        .upsert_scheduled_task("project-test", &daily_task("claim-me"))
        .await
        .unwrap();
    let scheduled_for: u64 = 1_767_264_000_000;

    let first = runtime
        .update_scheduled_task_state_if(
            "project-test",
            &created.task.id,
            &|task: &ScheduledTask| task.state.last_scheduled_for.is_none(),
            &json!({ "lastScheduledFor": scheduled_for, "lastStatus": "running", "nextRunAt": scheduled_for + 86_400_000 }),
        )
        .await
        .unwrap();
    assert!(first.updated);
    assert_eq!(
        first.task.as_ref().unwrap().state.last_scheduled_for,
        Some(scheduled_for)
    );

    let second = runtime
        .update_scheduled_task_state_if(
            "project-test",
            &created.task.id,
            &|task: &ScheduledTask| task.state.last_scheduled_for != Some(scheduled_for),
            &json!({ "lastScheduledFor": scheduled_for, "lastStatus": "running" }),
        )
        .await
        .unwrap();
    assert!(!second.updated);
    assert_eq!(
        second.task.as_ref().unwrap().state.last_scheduled_for,
        Some(scheduled_for)
    );

    let reloaded = runtime.list_scheduled_tasks("project-test").await.unwrap();
    assert_eq!(reloaded[0].state.last_scheduled_for, Some(scheduled_for));
}

/// 目标任务不存在时状态更新返回 `updated=false`；空白 taskId 报 `taskId is required`。
#[tokio::test]
async fn state_updates_for_missing_tasks_report_not_updated() {
    let temp = TempDir::new("state-missing");
    let runtime = test_runtime(temp.path());
    let result = runtime
        .update_scheduled_task_state("project-test", "nope", &json!({ "lastStatus": "success" }))
        .await
        .unwrap();
    assert!(!result.updated);
    assert!(result.task.is_none());

    let error = runtime
        .update_scheduled_task_state("project-test", "  ", &json!({}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("taskId is required"));
}

/// 状态补丁中的 null 会清空可选字段，且序列化结果完全省略被清空的键。
#[tokio::test]
async fn null_patch_values_clear_optional_state_fields() {
    let temp = TempDir::new("null-clear");
    let runtime = test_runtime(temp.path());
    let created = runtime
        .upsert_scheduled_task("project-test", &daily_task("clears"))
        .await
        .unwrap();
    runtime
        .update_scheduled_task_state(
            "project-test",
            &created.task.id,
            &json!({ "nextRunAt": 500, "lastError": "boom", "lastSessionId": "s1" }),
        )
        .await
        .unwrap();
    let seeded = runtime
        .list_scheduled_tasks("project-test")
        .await
        .unwrap()
        .remove(0);
    assert_eq!(seeded.state.next_run_at, Some(500));
    assert_eq!(seeded.state.last_error.as_deref(), Some("boom"));

    let cleared = runtime
        .update_scheduled_task_state(
            "project-test",
            &created.task.id,
            &json!({ "nextRunAt": null, "lastError": null, "lastSessionId": null }),
        )
        .await
        .unwrap();
    let task = cleared.task.unwrap();
    assert_eq!(task.state.next_run_at, None);
    assert_eq!(task.state.last_error, None);
    assert_eq!(task.state.last_session_id, None);
    assert_eq!(task.state.last_status, "idle");

    // Serialized state omits the cleared fields entirely.
    let value = serde_json::to_value(&task).unwrap();
    let state = value["state"].as_object().unwrap();
    assert!(!state.contains_key("nextRunAt"));
    assert!(!state.contains_key("lastError"));
}

// ============== cross-process locking ==============

/// 计算某项目配置对应的 `<config>.lock` 路径。
fn lock_path_for(runtime: &ProjectConfigRuntime, project_id: &str) -> PathBuf {
    PathBuf::from(format!(
        "{}.lock",
        runtime
            .resolve_project_config_path(project_id)
            .unwrap()
            .to_string_lossy()
    ))
}

/// 共享同一 projects 目录的两个 runtime 并发写同任务不丢失更新，且锁文件随后被清理。
#[tokio::test]
async fn serializes_concurrent_writes_across_two_runtimes_sharing_a_projects_dir() {
    let temp = TempDir::new("lock-contention");
    let runtime_a = test_runtime(temp.path());
    let runtime_b = test_runtime(temp.path());

    runtime_a
        .upsert_scheduled_task("project-lock", &daily_task("contended"))
        .await
        .unwrap();

    let patch_a = json!({ "lastStatus": "success", "lastRunAt": 100 });
    let patch_b = json!({ "lastStatus": "error", "lastRunAt": 200 });
    let (a, b) = tokio::join!(
        runtime_a.update_scheduled_task_state("project-lock", "task-fixed-id", &patch_a),
        runtime_b.update_scheduled_task_state("project-lock", "task-fixed-id", &patch_b),
    );
    a.unwrap();
    b.unwrap();

    let tasks = runtime_a
        .list_scheduled_tasks("project-lock")
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert!(["success", "error"].contains(&tasks[0].state.last_status.as_str()));
    assert!([100, 200].contains(&tasks[0].state.last_run_at.unwrap()));

    assert!(!lock_path_for(&runtime_a, "project-lock").exists());
}

/// 锁龄超过阈值的锁被窃取恢复，操作完成后锁文件被清理。
#[tokio::test]
async fn recovers_from_a_stale_by_age_project_config_lock_and_cleans_it_up() {
    let temp = TempDir::new("lock-age");
    let runtime = test_runtime(temp.path());
    let lock_path = lock_path_for(&runtime, "stale-age");
    std::fs::create_dir_all(temp.path()).unwrap();
    std::fs::write(
        &lock_path,
        serde_json::to_string(&json!({ "pid": std::process::id(), "at": 0 })).unwrap(),
    )
    .unwrap();

    let created = runtime
        .upsert_scheduled_task("stale-age", &daily_task("after-stale"))
        .await
        .unwrap();
    assert!(created.created);
    assert!(!lock_path.exists());
}

/// 属主 pid 已死的锁被恢复（即使时间戳是新鲜的，验证 dead-pid 路径）。
#[tokio::test]
async fn recovers_from_a_stale_by_dead_pid_project_config_lock() {
    let temp = TempDir::new("lock-pid");
    let runtime = test_runtime(temp.path());
    let lock_path = lock_path_for(&runtime, "stale-pid");
    std::fs::create_dir_all(temp.path()).unwrap();
    // PID unlikely to exist; kill(pid, 0) fails with ESRCH.
    std::fs::write(
        &lock_path,
        serde_json::to_string(&json!({ "pid": 2_147_483_647, "at": 0 })).unwrap(),
    )
    .unwrap();

    // `at: 0` would also be stale by age; rewrite with a fresh timestamp so
    // only the dead-pid path can recover.
    let fresh = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    std::fs::write(
        &lock_path,
        serde_json::to_string(&json!({ "pid": 2_147_483_647, "at": fresh })).unwrap(),
    )
    .unwrap();

    let created = runtime
        .upsert_scheduled_task("stale-pid", &daily_task("after-dead-pid"))
        .await
        .unwrap();
    assert!(created.created);
    assert!(!lock_path.exists());
}

/// 守卫释放时不会误删已被他人窃取替换的锁文件。
#[tokio::test]
async fn release_does_not_unlink_a_lock_stolen_by_another_holder() {
    let temp = TempDir::new("lock-own");
    let runtime = test_runtime(temp.path());
    let lock_path = lock_path_for(&runtime, "lock-own");
    let guard = runtime.acquire_project_file_lock("lock-own").await.unwrap();

    // A stale-recovery steal replaces the payload while we hold the guard.
    let stolen_pid = std::process::id() as i64 + 1;
    std::fs::write(
        &lock_path,
        serde_json::to_string(&json!({ "pid": stolen_pid, "at": 0 })).unwrap(),
    )
    .unwrap();
    guard.release().await;

    let raw = std::fs::read_to_string(&lock_path).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&raw).unwrap()["pid"],
        stolen_pid
    );
}

/// 活着的持锁者不释放时按 JS 消息超时报错；随后写链已释放，后续写入可正常完成。
#[tokio::test]
async fn times_out_when_a_live_lock_holder_never_releases_and_then_recovers() {
    let temp = TempDir::new("lock-timeout");
    let runtime = test_runtime(temp.path()).with_lock_timings(200, 60_000, 10);
    let lock_path = lock_path_for(&runtime, "lock-timeout");
    std::fs::create_dir_all(temp.path()).unwrap();
    // Current process is alive and the payload is fresh: acquire must wait,
    // then fail with the JS timeout message.
    std::fs::write(
        &lock_path,
        serde_json::to_string(&json!({ "pid": std::process::id(), "at": system_time_ms() }))
            .unwrap(),
    )
    .unwrap();

    let error = runtime
        .upsert_scheduled_task("lock-timeout", &daily_task("blocked"))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("timeout acquiring project config lock for lock-timeout"),
        "got: {error}"
    );

    // Remove the hostile lock; the in-process chain must have been released,
    // so a later write completes instead of hanging.
    std::fs::remove_file(&lock_path).unwrap();
    let created = runtime
        .upsert_scheduled_task(
            "lock-timeout",
            &json!({
                "name": "after-timeout",
                "enabled": true,
                "schedule": { "kind": "daily", "time": "12:00", "timezone": "UTC" },
                "execution": { "prompt": "Run after timeout", "providerID": "openai", "modelID": "gpt-4.1" },
            }),
        )
        .await
        .unwrap();
    assert!(created.created);
    let listed = runtime.list_scheduled_tasks("lock-timeout").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "after-timeout");
}

/// payload 不可解析的锁退化为按 mtime 年龄判断失效并恢复。
#[tokio::test]
async fn recovers_from_an_unparseable_lock_using_mtime_age() {
    let temp = TempDir::new("lock-mtime");
    let runtime = test_runtime(temp.path());
    let lock_path = lock_path_for(&runtime, "stale-unparseable");
    std::fs::create_dir_all(temp.path()).unwrap();
    std::fs::write(&lock_path, "{not-json").unwrap();
    let stale = SystemTime::now() - Duration::from_millis(120_000);
    let file = std::fs::File::options()
        .write(true)
        .open(&lock_path)
        .unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(stale))
        .unwrap();
    drop(file);

    let created = runtime
        .upsert_scheduled_task("stale-unparseable", &daily_task("after-unparseable"))
        .await
        .unwrap();
    assert!(created.created);
    assert!(!lock_path.exists());
}

/// 当前 Unix epoch 毫秒时间戳（测试构造锁 payload 用）。
fn system_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ============== model-level unit checks ==============

/// Execution 序列化使用 JS 端驼峰字段名（providerID/modelID/goalTokenBudget 等）。
#[test]
fn execution_wire_shape_uses_js_field_names() {
    let execution = serde_json::to_value(Execution {
        prompt: "p".to_string(),
        provider_id: Some("openai".to_string()),
        model_id: Some("gpt-4.1".to_string()),
        model_role: None,
        variant: Some("fast".to_string()),
        agent: Some("build".to_string()),
        goal_enabled: Some(true),
        goal_token_budget: Some(20_000),
        permission_auto_accept: Some(true),
    })
    .unwrap();
    let object = execution.as_object().unwrap();
    assert!(object.contains_key("providerID"));
    assert!(object.contains_key("modelID"));
    assert!(object.contains_key("goalTokenBudget"));
    assert!(object.contains_key("permissionAutoAccept"));
}

/// ScheduledTask 序列化形状与 JS 对象一致：loopFile、schedule.kind、state.nextRunAt 及顶层键集合。
#[test]
fn task_wire_shape_matches_the_js_object() {
    let task = ScheduledTask {
        id: "t".to_string(),
        name: "n".to_string(),
        enabled: true,
        schedule: Schedule::Daily {
            times: vec!["09:30".to_string()],
            timezone: "UTC".to_string(),
        },
        execution: Execution {
            prompt: "p".to_string(),
            provider_id: Some("openai".to_string()),
            model_id: Some("gpt-4.1".to_string()),
            model_role: None,
            variant: None,
            agent: None,
            goal_enabled: None,
            goal_token_budget: None,
            permission_auto_accept: None,
        },
        state: super::TaskState {
            created_at: 1,
            updated_at: 2,
            last_status: "idle".to_string(),
            last_run_at: None,
            last_duration_ms: None,
            next_run_at: Some(3),
            last_scheduled_for: None,
            last_session_id: None,
            last_error: None,
        },
        loop_file: Some("/repo/loop.md".to_string()),
    };
    let value = serde_json::to_value(&task).unwrap();
    assert_eq!(value["loopFile"], "/repo/loop.md");
    assert_eq!(value["schedule"]["kind"], "daily");
    assert_eq!(value["state"]["nextRunAt"], 3);
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    // serde_json without `preserve_order` emits object keys alphabetically —
    // a cosmetic-only deviation from the JS insertion order.
    assert_eq!(
        keys,
        [
            "enabled",
            "execution",
            "id",
            "loopFile",
            "name",
            "schedule",
            "state"
        ]
    );
}

/// 半截/非 object 根解析报错，非 object 与空内容降级为空 map，严格 JSON 正常解析。
#[test]
fn jsonc_parse_rejects_partial_and_non_object_roots_like_the_js() {
    use super::runtime::parse_project_config_text;
    // Partial parse is an error, never an authoritative stub.
    assert!(parse_project_config_text("{").is_err());
    assert!(parse_project_config_text("mcp:\n  key: value\n").is_err());
    assert!(parse_project_config_text("[][]").is_err());
    // Non-object and value-less roots degrade to `{}`.
    assert!(parse_project_config_text("[1, 2]").unwrap().is_empty());
    assert!(parse_project_config_text("42").unwrap().is_empty());
    assert!(
        parse_project_config_text("// comment only\n")
            .unwrap()
            .is_empty()
    );
    // Valid strict JSON parses identically.
    let parsed = parse_project_config_text("{\"a\": 1}").unwrap();
    assert_eq!(parsed.get("a"), Some(&json!(1)));
}

/// 认领路径返回的 Future 满足 Send（谓词带 `+Send+Sync` 约束，可被 `tokio::spawn`）。
#[tokio::test]
async fn claim_predicate_future_is_send() {
    // The scheduled-tasks claim path spawns under tokio::spawn: the runtime
    // future must stay Send with the +Send+Sync predicate bound.
    // 编译期断言辅助：要求 Future 满足 Send。
    fn assert_send<F: Future + Send>(_: &F) {}
    let temp = TempDir::new("send-check");
    let runtime = test_runtime(temp.path());
    let predicate = |task: &ScheduledTask| task.state.last_scheduled_for.is_none();
    let patch = json!({ "lastStatus": "running" });
    let future =
        runtime.update_scheduled_task_state_if("project-test", "task-x", &predicate, &patch);
    assert_send(&future);
    drop(future);
}
