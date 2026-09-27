//! Tests for the git_service module port: argv construction via an
//! injectable runner fake, the path-safety checks the JS tests pin, route
//! shapes via `Router::oneshot` against the same fake, long-path (issue
//! #2746) normalization, and the pure parsers.
//!
//! 中文说明：git_service 模块移植的测试：通过可注入的假 runner 断言 argv
//! 构造、JS 测试固定的路径安全检查、用 `Router::oneshot` 配合同一假 runner
//! 验证路由形状、长路径（issue #2746）规范化，以及各类纯解析函数。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use tower::ServiceExt;

use super::exec::{GitCommandResult, GitRunner};
use super::identity::IdentityStorage;
use super::service::{GitService, NOT_A_REPO_MESSAGE};

// ---------------------------------------------------------------------------
// Fake runner
// ---------------------------------------------------------------------------

/// 脚本化 git 响应：按 argv 匹配返回预置结果；未匹配（`None`）视为空输出成功。
type Script = Arc<dyn Fn(&[String]) -> Option<GitCommandResult> + Send + Sync>;

/// 记录调用的假 git 辅助类型。
struct FakeGit {
    /// 已记录的 (cwd, argv) 调用列表。
    calls: Arc<Mutex<Vec<(PathBuf, Vec<String>)>>>,
}

/// 构造成功结果的快捷方式（stdout 取入参）。
fn ok(stdout: &str) -> GitCommandResult {
    GitCommandResult::ok(stdout.to_string())
}

/// 构造失败结果的快捷方式（stdout 为空、退出码 1）。
fn fail(stderr: &str) -> GitCommandResult {
    GitCommandResult::fail("", stderr)
}

/// Runner that records every invocation and answers from `script`; unscripted
/// commands succeed with empty output (like git's quiet success).
/// 构造使用脚本化 runner 的 GitService 并返回调用记录句柄；未脚本化的命令
/// 以空输出成功（模拟 git 的静默成功）。
fn fake_runner(script: Script) -> (GitService, Arc<Mutex<Vec<(PathBuf, Vec<String>)>>>) {
    let calls: Arc<Mutex<Vec<(PathBuf, Vec<String>)>>> = Arc::new(Mutex::new(Vec::new()));
    let runner: GitRunner = {
        let calls = Arc::clone(&calls);
        Arc::new(move |cwd, args, _env| {
            let script = Arc::clone(&script);
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                let result = script(&args).unwrap_or_else(|| ok(""));
                calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((cwd, args));
                result
            })
        })
    };
    (GitService::with_runner(runner), calls)
}

/// 创建带随机后缀的唯一临时目录（测试夹具）。
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ompchamber-git-svc-{}-{}-{}",
        tag,
        std::process::id(),
        suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 生成 8 位十六进制随机后缀，避免并行测试冲突。
fn suffix() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..8)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

/// 取调用记录快照，仅保留 argv（丢弃 cwd）。
fn calls_snapshot(calls: &Arc<Mutex<Vec<(PathBuf, Vec<String>)>>>) -> Vec<Vec<String>> {
    calls
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(_, args)| args.clone())
        .collect()
}

/// 判断调用记录中是否存在以给定前缀开头的 argv。
fn has_call(calls: &[Vec<String>], prefix: &[&str]) -> bool {
    calls.iter().any(|args| {
        prefix.len() <= args.len()
            && prefix
                .iter()
                .zip(args.iter())
                .all(|(want, got)| got.as_str() == *want)
    })
}

// ---------------------------------------------------------------------------
// getStatus
// ---------------------------------------------------------------------------

/// 预置 getStatus 所需的 git 响应脚本：分支跟踪行、文件状态、numstat
/// 以及 MERGE_HEAD/upstream 远端探测。
fn status_script(repo: &str) -> Script {
    let repo = repo.to_string();
    Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--git-dir"] => Some(ok(".git")),
            ["rev-parse", "--show-toplevel"] => Some(ok(&repo)),
            ["status", "--porcelain", "-b", "-u", "--null", "-uall"] => Some(GitCommandResult {
                success: true,
                exit_code: 0,
                stdout: "## main...origin/main [ahead 1, behind 2]\0 M file.txt\0?? new.ts\0"
                    .into(),
                stderr: String::new(),
                message: String::new(),
                stdout_bytes: Vec::new(),
            }),
            ["diff", "--cached", "--numstat"] => Some(ok("1\t1\tsrc/a.ts\n")),
            ["diff", "--numstat"] => Some(ok("2\t0\tsrc/b.ts\n")),
            ["rev-parse", "--verify", "--quiet", "MERGE_HEAD"] => Some(fail("fatal: bad revision")),
            ["remote", "get-url", "upstream"] => Some(fail("fatal: No such remote 'upstream'")),
            _ => None,
        }
    })
}

/// 验证 getStatus 解析出分支、tracking、ahead/behind、文件状态与 diff
/// 统计，且无 merge/rebase 进行时也不多出 upstreamComparison 字段。
#[tokio::test]
async fn get_status_parses_tracking_files_and_stats() {
    let repo = temp_dir("status");
    let repo_text = repo.to_string_lossy().to_string();
    let (service, _calls) = fake_runner(status_script(&repo_text));

    let status = service.get_status(&repo_text, None).await.expect("status");
    assert_eq!(status["current"], "main");
    assert_eq!(status["tracking"], "origin/main");
    assert_eq!(status["ahead"], 1);
    assert_eq!(status["behind"], 2);
    assert_eq!(
        status["files"],
        serde_json::json!([
            { "path": "file.txt", "index": " ", "working_dir": "M" },
            { "path": "new.ts", "index": "?", "working_dir": "?" },
        ])
    );
    assert_eq!(status["isClean"], false);
    assert_eq!(status["diffStats"]["src/a.ts"]["insertions"], 1);
    assert_eq!(status["diffStats"]["src/b.ts"]["deletions"], 0);
    assert_eq!(status["mergeInProgress"], serde_json::Value::Null);
    assert_eq!(status["rebaseInProgress"], serde_json::Value::Null);
    assert!(status.get("upstreamComparison").is_none());
}

/// 验证 light 模式既不执行 numstat 命令，也不产出 diffStats 字段。
#[tokio::test]
async fn get_status_light_mode_skips_stats() {
    let repo = temp_dir("status-light");
    let repo_text = repo.to_string_lossy().to_string();
    let (service, calls) = fake_runner(status_script(&repo_text));

    let status = service
        .get_status(&repo_text, Some("light"))
        .await
        .expect("status");
    assert!(status.get("diffStats").is_none());
    let calls = calls_snapshot(&calls);
    assert!(!has_call(&calls, &["diff", "--cached", "--numstat"]));
    assert!(!has_call(&calls, &["diff", "--numstat"]));
}

/// 验证非仓库目录返回固定的 NOT_A_REPO_MESSAGE 错误文本。
#[tokio::test]
async fn get_status_rejects_non_repo_with_pinned_message() {
    let (service, _calls) = fake_runner(Arc::new(|args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--git-dir"] => Some(fail(
                "fatal: not a git repository (or any of the parent directories): .git",
            )),
            _ => None,
        }
    }));
    let error = service
        .get_status("/tmp/definitely-not-a-repo", None)
        .await
        .unwrap_err();
    assert_eq!(error, NOT_A_REPO_MESSAGE);
}

// ---------------------------------------------------------------------------
// Staging argv + path safety
// ---------------------------------------------------------------------------

/// 验证 stage_files 去空白、去空项并去重后构造单条 `add -- <paths>` 批量 argv。
#[tokio::test]
async fn stage_files_builds_add_argv_with_resolved_repo_paths() {
    let repo = temp_dir("stage");
    let repo_text = repo.to_string_lossy().to_string();
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["cat-file", "-e", path] => {
                if path.starts_with(':') {
                    Some(ok(""))
                } else {
                    None
                }
            }
            _ => None,
        }
    });
    let (service, calls) = fake_runner(script);

    service
        .stage_files(
            &repo_text,
            &[
                serde_json::json!("src/a.ts"),
                serde_json::json!(" src/a.ts "),
                serde_json::json!(""),
                serde_json::json!("src/b.ts"),
            ],
        )
        .await
        .expect("stage");

    let calls = calls_snapshot(&calls);
    assert!(
        has_call(&calls, &["add", "--", "src/a.ts", "src/b.ts"]),
        "expected batch add argv, got {:?}",
        calls
    );
}

/// 验证越库路径在校验阶段即被拒绝，且不执行任何 git 命令。
#[tokio::test]
async fn stage_files_rejects_paths_outside_repository_before_running_git() {
    let (service, calls) = fake_runner(Arc::new(|_| None));
    let error = service
        .stage_files("/repo", &[serde_json::json!("../secret.txt")])
        .await
        .unwrap_err();
    assert_eq!(error, "Path is outside repository: ../secret.txt");
    assert!(
        calls_snapshot(&calls).is_empty(),
        "git must not run for unsafe paths"
    );
}

/// 验证 unstage_files 构造 `restore --staged -- <paths>` argv。
#[tokio::test]
async fn unstage_files_builds_restore_staged_argv() {
    let repo = temp_dir("unstage");
    let repo_text = repo.to_string_lossy().to_string();
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["cat-file", "-e", path] if path.starts_with(':') => Some(ok("")),
            _ => None,
        }
    });
    let (service, calls) = fake_runner(script);

    service
        .unstage_files(&repo_text, &[serde_json::json!("a.ts")])
        .await
        .expect("unstage");

    let calls = calls_snapshot(&calls);
    assert!(has_call(&calls, &["restore", "--staged", "--", "a.ts"]));
}

// ---------------------------------------------------------------------------
// commit argv + summary parse
// ---------------------------------------------------------------------------

/// 验证 commit 使用 `core.abbrev=40` 前缀的 commit argv，并从输出摘要解析
/// 提交哈希、分支与变更统计。
#[tokio::test]
async fn commit_builds_simple_git_commit_argv_and_parses_summary() {
    let repo = temp_dir("commit");
    let repo_text = repo.to_string_lossy().to_string();
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["status", "--porcelain", "-b", "-u", "--null"] => Some(GitCommandResult {
                success: true,
                exit_code: 0,
                stdout: "## main\0M  src/a.ts\0".into(),
                stderr: String::new(),
                message: String::new(),
                stdout_bytes: Vec::new(),
            }),
            ["-c", "core.abbrev=40", "commit", "-m", message, _path] => Some(ok(&format!(
                "[main 1a2b3c4] {}\n 1 file changed, 2 insertions(+)\n",
                message
            ))),
            _ => None,
        }
    });
    let (service, calls) = fake_runner(script);

    let result = service
        .commit(
            &repo_text,
            "feat: ship",
            &serde_json::json!({ "files": ["src/a.ts"] }),
        )
        .await
        .expect("commit");

    assert_eq!(result["success"], true);
    assert_eq!(result["commit"], "1a2b3c4");
    assert_eq!(result["branch"], "main");
    assert_eq!(result["summary"]["changes"], 1);
    assert_eq!(result["summary"]["insertions"], 2);
    assert_eq!(result["summary"]["deletions"], 0);

    let calls = calls_snapshot(&calls);
    assert!(has_call(
        &calls,
        &[
            "-c",
            "core.abbrev=40",
            "commit",
            "-m",
            "feat: ship",
            "src/a.ts"
        ]
    ));
}

/// 验证 addAll 选项先执行 `add .` 再 commit，摘要统计解析正确。
#[tokio::test]
async fn commit_add_all_runs_plain_add_dot() {
    let repo = temp_dir("commit-all");
    let repo_text = repo.to_string_lossy().to_string();
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["add", "."] => Some(ok("")),
            ["-c", "core.abbrev=40", "commit", "-m", ..] => Some(ok(
                "[main deadbeef] all\n 2 files changed, 3 insertions(+), 1 deletion(-)",
            )),
            _ => None,
        }
    });
    let (service, calls) = fake_runner(script);

    let result = service
        .commit(&repo_text, "all", &serde_json::json!({ "addAll": true }))
        .await
        .expect("commit");
    assert_eq!(result["summary"]["insertions"], 3);
    assert_eq!(result["summary"]["deletions"], 1);

    let calls = calls_snapshot(&calls);
    assert!(has_call(&calls, &["add", "."]));
    assert!(has_call(
        &calls,
        &["-c", "core.abbrev=40", "commit", "-m", "all"]
    ));
}

// ---------------------------------------------------------------------------
// Branch operations argv
// ---------------------------------------------------------------------------

/// 验证创建分支走 `checkout -b <name> <start-point>` argv。
#[tokio::test]
async fn create_branch_uses_checkout_b_with_start_point() {
    let repo = temp_dir("branch");
    let repo_text = repo.to_string_lossy().to_string();
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            _ => None,
        }
    });
    let (service, calls) = fake_runner(script);

    let result = service
        .create_branch(&repo_text, "feature", Some("main"))
        .await
        .expect("create");
    assert_eq!(result["success"], true);
    assert_eq!(result["branch"], "feature");

    let calls = calls_snapshot(&calls);
    assert!(has_call(&calls, &["checkout", "-b", "feature", "main"]));
}

/// 验证检出远端分支名时自动建立本地跟踪分支，且返回实际检出的分支名。
#[tokio::test]
async fn checkout_branch_resolves_remote_pick_to_tracking_branch() {
    let repo = temp_dir("checkout");
    let repo_text = repo.to_string_lossy().to_string();
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["show-ref", "--verify", "refs/heads/origin/react"] => {
                Some(fail("fatal: refs/heads/origin/react - not a valid ref"))
            }
            ["remote"] => Some(ok("origin\n")),
            ["show-ref", "--verify", "refs/remotes/origin/react"] => {
                Some(ok("abc123 refs/remotes/origin/react"))
            }
            ["show-ref", "--verify", "refs/heads/react"] => Some(fail("fatal: not a valid ref")),
            ["checkout", "-b", "react", "--track", "origin/react"] => {
                Some(ok("Switched to a new branch 'react'"))
            }
            _ => None,
        }
    });
    let (service, calls) = fake_runner(script);

    let result = service
        .checkout_branch(&repo_text, "origin/react")
        .await
        .expect("checkout");
    // The branch actually checked out is reported, not the requested name.
    assert_eq!(result["branch"], "react");
    let calls = calls_snapshot(&calls);
    assert!(has_call(
        &calls,
        &["checkout", "-b", "react", "--track", "origin/react"]
    ));
}

/// 验证本地与远端都不存在的分支 fetch 失败时，返回
/// "Failed to fetch <branch> from origin" 错误。
#[tokio::test]
async fn checkout_branch_reports_remote_only_branch_fetch_failure() {
    let repo = temp_dir("checkout-fetch");
    let repo_text = repo.to_string_lossy().to_string();
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["show-ref", "--verify", reference] => {
                Some(fail(&format!("fatal: '{}' - not a valid ref", reference)))
            }
            ["remote"] => Some(ok("origin\n")),
            ["fetch", "origin", "never-pushed"] => {
                Some(fail("fatal: couldn't find remote ref never-pushed"))
            }
            _ => None,
        }
    });
    let (service, _calls) = fake_runner(script);

    let error = service
        .checkout_branch(&repo_text, "remotes/origin/never-pushed")
        .await
        .unwrap_err();
    assert!(
        error.contains("Failed to fetch never-pushed from origin"),
        "got: {}",
        error
    );
}

// ---------------------------------------------------------------------------
// Range helpers
// ---------------------------------------------------------------------------

/// 验证 range diff 对本地不存在的 ref 返回固定的 "not available locally" 错误。
#[tokio::test]
async fn range_diff_names_unfetched_ref_plainly() {
    let repo = temp_dir("range");
    let repo_text = repo.to_string_lossy().to_string();
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["rev-parse", "--verify", "refs/remotes/origin/react"] => {
                Some(fail("fatal: ref not found"))
            }
            ["rev-parse", "--verify", "refs/heads/react"] => Some(fail("fatal: ref not found")),
            ["for-each-ref", "--count=1", ..] => Some(ok("")),
            ["rev-parse", "--verify", "--quiet", ref_arg] if ref_arg.ends_with("^{commit}") => {
                Some(fail("fatal: needed a single revision"))
            }
            _ => None,
        }
    });
    let (service, _calls) = fake_runner(script);

    let error = service
        .get_range_diff(&repo_text, "react", "next", None, 3)
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "Ref \"react\" is not available locally. Fetch it before comparing."
    );
}

/// 验证 range files 解析 `-z` porcelain 输出：rename 条目取目标路径，
/// 状态归一为 R，含空格文件名正确还原。
#[tokio::test]
async fn range_files_parses_rename_destinations() {
    let repo = temp_dir("range-files");
    let repo_text = repo.to_string_lossy().to_string();
    let porcelain = "A\0added.txt\0M\0README.md\0R100\0old name.txt\0new name.txt\0";
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["rev-parse", "--verify", "refs/remotes/origin/main"] => {
                Some(ok("abc refs/remotes/origin/main"))
            }
            ["rev-parse", "--verify", "--quiet", ref_arg] if ref_arg.ends_with("^{commit}") => {
                Some(ok("abc"))
            }
            ["diff", "--name-status", "-z", "-C", ..] => Some(ok(porcelain)),
            _ => None,
        }
    });
    let (service, _calls) = fake_runner(script);

    let files = service
        .get_range_files(&repo_text, "main", "feature")
        .await
        .expect("range files");
    assert_eq!(
        serde_json::Value::Array(files),
        serde_json::json!([
            { "path": "added.txt", "status": "A" },
            { "path": "README.md", "status": "M" },
            { "path": "new name.txt", "status": "R" },
        ])
    );
}

// ---------------------------------------------------------------------------
// Long paths (issue #2746)
// ---------------------------------------------------------------------------

/// 验证 worktree 填充错误含 "Filename too long" 时附加长路径指引
/// （core.longpaths），其它错误原样透传。
#[test]
fn populate_error_appends_path_length_guidance() {
    let message = super::service_ops::format_worktree_populate_error(
        "error: unable to create file x: Filename too long",
    );
    assert!(message.contains("Filename too long"));
    assert!(message.contains("path-length limit"));
    assert!(message.contains("core.longpaths"));

    let passthrough = super::service_ops::format_worktree_populate_error("boom");
    assert_eq!(passthrough, "boom");
}

/// 验证填充流程先设置 core.longpaths、再执行带该配置的 reset --hard（顺序断言）。
#[tokio::test]
async fn populate_enables_longpaths_then_resets_hard() {
    let repo = temp_dir("longpaths");
    let repo_text = repo.to_string_lossy().to_string();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["config", "--get", "core.longpaths"] => Some(ok("")),
            ["config", "core.longpaths", "true"] => Some(ok("")),
            ["-c", "core.longpaths=true", "reset", "--hard"] => Some(ok("HEAD is now at abc")),
            _ => None,
        }
    });
    let (service, calls) = fake_runner(script);

    service
        .populate_worktree_with_lock_recovery(&repo_text)
        .await
        .expect("populate");

    let calls = calls_snapshot(&calls);
    let config_index = calls
        .iter()
        .position(|args| {
            args == &[
                "config".to_string(),
                "core.longpaths".to_string(),
                "true".to_string(),
            ]
        })
        .expect("config set");
    let reset_index = calls
        .iter()
        .position(|args| args[0] == "-c" && args[1] == "core.longpaths=true" && args[2] == "reset")
        .expect("reset with -c core.longpaths");
    assert!(
        config_index < reset_index,
        "longpaths must be enabled before reset --hard"
    );
}

/// 验证重试耗尽后字节未变化的陈旧 index.lock 会被删除，恢复最终成功。
#[tokio::test]
async fn populate_recovers_from_stale_unchanged_index_lock() {
    let repo = temp_dir("stale-lock");
    let repo_text = repo.to_string_lossy().to_string();
    let lock_path = repo.join("index.lock");
    std::fs::write(&lock_path, b"stale").unwrap();
    let lock_text = lock_path.to_string_lossy().to_string();

    let attempts = Arc::new(Mutex::new(0u32));
    let script_attempts = Arc::clone(&attempts);
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["config", "--get", "core.longpaths"] => Some(ok("true")),
            ["-c", "core.longpaths=true", "reset", "--hard"] => {
                let mut count = script_attempts.lock().unwrap_or_else(|e| e.into_inner());
                *count += 1;
                if *count <= 3 {
                    Some(fail(
                        "fatal: Unable to create '/x/.git/index.lock': File exists.\n\nAnother git process seems to be running in this repository",
                    ))
                } else {
                    Some(ok("HEAD is now at abc"))
                }
            }
            ["rev-parse", "--git-path", "index.lock"] => Some(ok(&lock_text)),
            _ => None,
        }
    });
    let (service, _calls) = fake_runner(script);

    service
        .populate_worktree_with_lock_recovery(&repo_text)
        .await
        .expect("stale lock recovered");
    assert_eq!(*attempts.lock().unwrap_or_else(|e| e.into_inner()), 4);
    // The byte-identical lock file was removed.
    assert!(!lock_path.exists());
}

// ---------------------------------------------------------------------------
// Pure parsers
// ---------------------------------------------------------------------------

/// 验证分支创建来源解析：仅接受非 HEAD、非完整哈希的 reflog 起点。
#[test]
fn branch_creation_source_rules() {
    use super::service::parse_branch_creation_source;
    let reflog = "commit: abc123\nbranch: Created from origin/main\nreset: moving to HEAD";
    assert_eq!(
        parse_branch_creation_source(reflog),
        Some("origin/main".to_string())
    );

    assert_eq!(
        parse_branch_creation_source("branch: Created from HEAD"),
        None
    );
    assert_eq!(
        parse_branch_creation_source("branch: Created from HEAD@{0}"),
        None
    );
    assert_eq!(
        parse_branch_creation_source(
            "branch: Created from 9a3b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b"
        ),
        None
    );
    assert_eq!(
        parse_branch_creation_source("commit: abc\nreset: moving"),
        None
    );
    assert_eq!(parse_branch_creation_source(""), None);
}

/// 验证日志 base ref 的本地优先解析：本地分支存在则优先，其次
/// origin/<from>，都不可用则原样返回；空白/缺失返回 None。
#[tokio::test]
async fn resolve_base_ref_local_first_semantics() {
    use super::service::resolve_base_ref_for_log;
    use std::pin::Pin;

    let check = |refs: &'static [&'static str]| {
        move |reference: String| -> Pin<Box<dyn Future<Output = bool> + Send>> {
            let refs = refs;
            Box::pin(async move { refs.iter().any(|known| *known == reference) })
        }
    };

    // Local wins even when origin also exists.
    assert_eq!(
        resolve_base_ref_for_log(Some("main"), check(&["main", "refs/remotes/origin/main"])).await,
        Some("main".to_string())
    );
    // Falls back to origin/<from>.
    assert_eq!(
        resolve_base_ref_for_log(Some("main"), check(&["refs/remotes/origin/main"])).await,
        Some("origin/main".to_string())
    );
    // Neither resolves → the original ref, so git surfaces the real error.
    assert_eq!(
        resolve_base_ref_for_log(Some("nope"), check(&[])).await,
        Some("nope".to_string())
    );
    // Blank / missing → undefined.
    assert_eq!(resolve_base_ref_for_log(None, check(&["main"])).await, None);
    assert_eq!(
        resolve_base_ref_for_log(Some(""), check(&["main"])).await,
        None
    );
    assert_eq!(
        resolve_base_ref_for_log(Some("   "), check(&["main"])).await,
        None
    );
}

// ---------------------------------------------------------------------------
// Routes (oneshot against the fake service layer)
// ---------------------------------------------------------------------------

/// 构造带临时目录、独立 identity 存储与空 EventHub 的路由测试状态。
fn test_state(service: GitService) -> super::GitState {
    super::GitState {
        service: Arc::new(service),
        identity: Arc::new(IdentityStorage::with_root(temp_dir("identity"))),
        hub: crate::hub::EventHub::new(),
        data_dir: temp_dir("data"),
    }
}

/// 以给定的方法/URI/body 对 git 路由发起单次请求并返回响应。
async fn oneshot(
    state: super::GitState,
    method: &str,
    uri: &str,
    body: Option<&str>,
) -> axum::response::Response {
    let router = super::routes::routes().with_state(state);
    let mut request = Request::builder().method(method).uri(uri);
    let request = if let Some(body) = body {
        request = request.header("content-type", "application/json");
        request.body(Body::from(body.to_string())).unwrap()
    } else {
        request.body(Body::empty()).unwrap()
    };
    router.oneshot(request).await.unwrap()
}

/// 读取响应体并解析为 JSON（解析失败时返回 Null）。
async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// 验证 status 路由对非仓库目录返回 200 与 isGitRepository:false 的软失败负载。
#[tokio::test]
async fn route_status_soft_non_repo_payload() {
    let repo = temp_dir("route-status");
    let repo_text = repo.to_string_lossy().to_string();
    // Non-repo: rev-parse --git-dir fails.
    let (service, _calls) = fake_runner(Arc::new(|args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--git-dir"] => Some(fail("fatal: not a git repository")),
            _ => None,
        }
    }));
    let response = oneshot(
        test_state(service),
        "GET",
        &format!("/api/git/status?directory={}", url_encode(&repo_text)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        serde_json::json!({
            "isGitRepository": false,
            "files": [],
            "branch": null,
            "ahead": 0,
            "behind": 0,
        })
    );
}

/// 保留路径安全字符的最小 URL 百分号编码（用于 query 参数拼装）。
fn url_encode(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '/' || c == '.' || c == '_' || c == '-' {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32 as u8)
            }
        })
        .collect()
}

/// 验证 status 路由对真实仓库返回核心状态字段的形状。
#[tokio::test]
async fn route_status_answers_repo_shape() {
    let repo = temp_dir("route-status-repo");
    let repo_text = repo.to_string_lossy().to_string();
    let (service, _calls) = fake_runner(status_script(&repo_text));
    let response = oneshot(
        test_state(service),
        "GET",
        &format!("/api/git/status?directory={}", url_encode(&repo_text)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = body_json(response).await;
    assert_eq!(body["current"], "main");
    assert_eq!(body["tracking"], "origin/main");
    assert_eq!(body["files"][0]["working_dir"], "M");
}

/// 验证 stage 路由：非法负载返回 400 且不触发 git；旧版单 path 负载与
/// 批量 paths 负载都能正确 stage。
#[tokio::test]
async fn route_stage_validation_and_success() {
    // Invalid payloads are rejected before git runs.
    let (service, calls) = fake_runner(Arc::new(|_| None));
    let response = oneshot(
        test_state(service),
        "POST",
        "/api/git/stage?directory=/repo",
        Some(r#"{"paths":[" ",null]}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"],
        "path parameter is required"
    );
    assert!(calls_snapshot(&calls).is_empty());

    // Legacy single path payload stages.
    let repo = temp_dir("route-stage");
    let repo_text = repo.to_string_lossy().to_string();
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["cat-file", "-e", path] if path.starts_with(':') => Some(ok("")),
            _ => None,
        }
    });
    let (service, calls) = fake_runner(script);
    let response = oneshot(
        test_state(service),
        "POST",
        &format!("/api/git/stage?directory={}", url_encode(&repo_text)),
        Some(r#"{"path":"a.ts"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["success"], true);
    assert!(has_call(&calls_snapshot(&calls), &["add", "--", "a.ts"]));

    // Bulk paths payload stages all.
    let repo2 = temp_dir("route-stage-bulk");
    let repo2_text = repo2.to_string_lossy().to_string();
    let script_repo = repo2_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["cat-file", "-e", path] if path.starts_with(':') => Some(ok("")),
            _ => None,
        }
    });
    let (service, calls) = fake_runner(script);
    let response = oneshot(
        test_state(service),
        "POST",
        &format!("/api/git/stage?directory={}", url_encode(&repo2_text)),
        Some(r#"{"paths":["a.ts","b.ts"]}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(has_call(
        &calls_snapshot(&calls),
        &["add", "--", "a.ts", "b.ts"]
    ));
}

/// 验证涉及 hash/mode 参数的路由对非法输入返回 400 与固定错误文本。
#[tokio::test]
async fn route_hash_validation_rejections() {
    let (service, _calls) = fake_runner(Arc::new(|_| None));
    let state = test_state(service);

    let response = oneshot(
        state.clone(),
        "POST",
        "/api/git/checkout-commit?directory=/repo",
        Some(r#"{"hash":"--hard"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["error"], "Invalid commit hash");

    let response = oneshot(
        state.clone(),
        "POST",
        "/api/git/cherry-pick?directory=/repo",
        Some(r#"{"hash":"HEAD"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["error"], "Invalid commit hash");

    let response = oneshot(
        state.clone(),
        "POST",
        "/api/git/reset-to-commit?directory=/repo",
        Some(r#"{"hash":"1234567890abcdef","mode":"keep"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"],
        "mode must be soft, mixed, or hard"
    );

    let response = oneshot(
        state,
        "GET",
        "/api/git/commit-file-diff?directory=/repo&hash=notahash&path=a.ts",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"],
        "hash must be a valid commit SHA"
    );
}

/// 验证 commit 与分支相关路由对缺失或非法字段返回 400 与固定错误文本。
#[tokio::test]
async fn route_commit_and_branch_validations() {
    let (service, _calls) = fake_runner(Arc::new(|_| None));
    let state = test_state(service);

    let response = oneshot(
        state.clone(),
        "POST",
        "/api/git/commit?directory=/repo",
        Some(r#"{"message":""}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["error"], "message is required");

    let response = oneshot(
        state.clone(),
        "POST",
        "/api/git/branches?directory=/repo",
        Some(r#"{"name":""}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["error"], "name is required");

    let response = oneshot(
        state.clone(),
        "POST",
        "/api/git/branch-push-status?directory=/repo",
        Some(r#"{"branches":"main"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"],
        "branches must be an array of branch names"
    );

    let response = oneshot(
        state,
        "PUT",
        "/api/git/branches/rename?directory=/repo",
        Some(r#"{"oldName":"a"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(response).await["error"], "newName is required");
}

/// 验证 diff 路由缺少 path 参数时返回 400。
#[tokio::test]
async fn route_diff_requires_path() {
    let (service, _calls) = fake_runner(Arc::new(|_| None));
    let response = oneshot(
        test_state(service),
        "GET",
        "/api/git/diff?directory=/repo",
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"],
        "path parameter is required"
    );
}

/// 验证一组代表性路由缺少 directory 参数时统一返回 400
/// 与 "directory parameter is required"。
#[tokio::test]
async fn route_missing_directory_is_400() {
    let (service, _calls) = fake_runner(Arc::new(|_| None));
    let state = test_state(service);
    for (method, path) in [
        ("GET", "/api/git/status"),
        ("GET", "/api/git/branches"),
        ("GET", "/api/git/log"),
        ("GET", "/api/git/worktrees"),
        ("POST", "/api/git/commit"),
        ("POST", "/api/git/pull"),
        ("POST", "/api/git/rebase"),
    ] {
        let response = oneshot(state.clone(), method, path, None).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "{} {}",
            method,
            path
        );
        assert_eq!(
            body_json(response).await["error"],
            "directory parameter is required"
        );
    }
}

/// 验证 worktrees 路由把 porcelain 输出映射为 name/branch/path 列表
/// （含 detached 条目与嵌套分支名）。
#[tokio::test]
async fn route_worktrees_lists_porcelain_entries() {
    let repo = temp_dir("route-worktrees");
    let repo_text = repo.to_string_lossy().to_string();
    let porcelain = "worktree /repo\nHEAD abc1234567890\ndetached\n\nworktree /repo/.git/worktrees/feature\nHEAD def4567890123\nbranch refs/heads/feature/x\n\n";
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["worktree", "list", "--porcelain"] => Some(ok(porcelain)),
            _ => None,
        }
    });
    let (service, _calls) = fake_runner(script);
    let response = oneshot(
        test_state(service),
        "GET",
        &format!("/api/git/worktrees?directory={}", url_encode(&repo_text)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        body_json(response).await,
        serde_json::json!([
            { "head": "abc1234567890", "name": "repo", "branch": "", "path": "/repo" },
            { "head": "def4567890123", "name": "feature", "branch": "feature/x", "path": "/repo/.git/worktrees/feature" },
        ])
    );
}

/// 验证 identities CRUD 路由：空列表起步、创建、重复 ID 返回 400、
/// 更新与删除成功。
#[tokio::test]
async fn route_identities_crud_shapes() {
    let (service, _calls) = fake_runner(Arc::new(|_| None));
    let state = test_state(service);

    let response = oneshot(state.clone(), "GET", "/api/git/identities", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await, serde_json::json!([]));

    let response = oneshot(
        state.clone(),
        "POST",
        "/api/git/identities",
        Some(r#"{"id":"work","userName":"Ada","userEmail":"ada@example.com"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let created = body_json(response).await;
    assert_eq!(created["id"], "work");
    assert_eq!(created["authType"], "ssh");

    let response = oneshot(
        state.clone(),
        "POST",
        "/api/git/identities",
        Some(r#"{"id":"work","userName":"Dupe","userEmail":"dupe@example.com"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(response).await["error"],
        "Profile with ID \"work\" already exists"
    );

    let response = oneshot(
        state.clone(),
        "PUT",
        "/api/git/identities/work",
        Some(r#"{"color":"string"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["color"], "string");

    let response = oneshot(state, "DELETE", "/api/git/identities/work", None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_json(response).await["success"], true);
}

/// 验证 global identity 未配置时 set-identity 返回 404 与固定错误文本。
#[tokio::test]
async fn route_set_identity_global_falls_back_to_ssh_key() {
    let repo = temp_dir("set-identity");
    let repo_text = repo.to_string_lossy().to_string();
    // Global identity read happens in $HOME; a missing global config yields
    // the 404 path without touching the repo.
    let (service, _calls) = fake_runner(Arc::new(|_| None));
    let response = oneshot(
        test_state(service),
        "POST",
        &format!("/api/git/set-identity?directory={}", url_encode(&repo_text)),
        Some(r#"{"profileId":"global"}"#),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(response).await["error"],
        "Global identity is not configured"
    );
}

// ---------------------------------------------------------------------------
// getLog
// ---------------------------------------------------------------------------

/// 验证 get_log 的 --all 模式解析记录分隔符边界、refs/parents 与变更统计。
#[tokio::test]
async fn log_all_mode_parses_boundaries_and_stats() {
    let repo = temp_dir("log-all");
    let repo_text = repo.to_string_lossy().to_string();
    let raw = concat!(
        "\u{1e}abc1234\u{1f}def5678\u{1f}Ada\u{1f}ada@x.com\u{1f}2024-01-01\u{1f}subject one\u{1f}HEAD -> main\n",
        " 2 files changed, 5 insertions(+), 1 deletion(-)\n",
        "\u{1e}def5678\u{1f}\u{1f}Ada\u{1f}ada@x.com\u{1f}2024-01-02\u{1f}subject two\u{1f}\n",
        " 1 file changed, 2 deletions(-)"
    );
    let script_repo = repo_text.clone();
    let script: Script = Arc::new(move |args: &[String]| {
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        match argv.as_slice() {
            ["rev-parse", "--show-toplevel"] => Some(ok(&script_repo)),
            ["log", "--max-count=50", "--all", ..] => Some(ok(raw)),
            _ => None,
        }
    });
    let (service, _calls) = fake_runner(script);

    let options = super::service_ops::LogOptions {
        all: true,
        ..Default::default()
    };
    let log = service.get_log(&repo_text, &options).await.expect("log");
    let all = log["all"].as_array().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["hash"], "abc1234");
    assert_eq!(all[0]["refs"], "HEAD -> main");
    assert_eq!(all[0]["parents"], serde_json::json!(["def5678"]));
    assert_eq!(all[0]["filesChanged"], 2);
    assert_eq!(all[0]["insertions"], 5);
    assert_eq!(all[0]["deletions"], 1);
    assert_eq!(all[1]["deletions"], 2);
    assert_eq!(log["total"], 2);
    assert_eq!(log["latest"]["hash"], "abc1234");
}
