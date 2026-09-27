//! Port of `server/lib/walkthrough/sources.js`.
//!
//! A walkthrough source resolves to one or more diff *sections*. A section is
//! a patch plus the scope its hunk ids live in; keeping staged and
//! working-tree changes in separate scopes means a stop written against
//! staged code never silently re-anchors onto an unstaged edit of the same
//! lines.
//!
//! The git calls are a seam (`GitDeps`) mirroring the JS module's imports of
//! `../git/service.js` — production wires [`GitDeps::real`] onto the ported
//! [`crate::git_service`] and tests inject closures, exactly where the JS
//! tests mock the git module.
//!
//! 中文说明：source 解析为一到多个 diff section；staged 与 working-tree
//! 分属不同 scope，避免 stop 锚点跨范围静默漂移。git 调用通过 `GitDeps`
//! 接缝注入，生产环境接到移植后的 git 服务，测试注入闭包假实现。

use std::sync::Arc;

use serde_json::{Value, json};

use super::error::WalkthroughError;
use super::small_model::BoxFuture;
use crate::git_service::service::GitService;

/// `WORKING_TREE_SCOPES`.
/// 中文补充：scope 决定取哪些 diff section，不在集合内的值直接 400。
const WORKING_TREE_SCOPES: [&str; 3] = ["all", "staged", "working"];

/// A normalized walkthrough source (`parseSource`'s output).
/// 中文补充：三种 source 分别对应工作区、分支对比与 pull request。
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
/// 工作树 source；`scope` 取 all（默认）/ staged / working。
    WorkingTree { scope: String },
/// 分支对比 source；`base_ref`/`head_ref` 均已 trim 且必填。
    Branch { base_ref: String, head_ref: String },
/// pull request source；`number` 为正整数 PR 编号。
    Pr { number: i64 },
}

/// source 的序列化辅助。
impl Source {
    /// The normalized descriptor the routes echo back to the client.
/// 中文补充：该形状也是缓存键与任务注册键的输入之一。
    pub fn to_value(&self) -> Value {
        match self {
            Source::WorkingTree { scope } => json!({ "kind": "working-tree", "scope": scope }),
            Source::Branch { base_ref, head_ref } => json!({
                "kind": "branch",
                "baseRef": base_ref,
                "headRef": head_ref,
            }),
            Source::Pr { number } => json!({ "kind": "pr", "number": number }),
        }
    }
}

/// JS `Number(value)` for the shapes a client may send.
/// 中文补充：字符串按 JS 数字语法解析（空串为 0），其余形状得 NaN，
/// 由调用方判定是否可用。
fn js_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                // JS `Number('')` is 0.
                return Some(0.0);
            }
            trimmed.parse::<f64>().ok().or(Some(f64::NAN))
        }
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        Value::Null => Some(0.0),
        _ => Some(f64::NAN),
    }
}

/// `parseSource`: normalize and validate an untrusted source descriptor from
/// the client. Every error message matches the JS `WalkthroughSourceError`.
/// 中文补充：错误一律 400 且消息逐字对齐 JS，方便客户端行为一致。
pub fn parse_source(raw: Option<&Value>) -> Result<Source, WalkthroughError> {
    let source_error = |message: String| WalkthroughError::new(message, 400);
    let Some(object) = raw.filter(|value| value.is_object()) else {
        return Err(source_error("source is required".to_string()));
    };

    match object.get("kind").and_then(Value::as_str) {
        Some("working-tree") => {
            let scope = object
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("all")
                .to_string();
            if !WORKING_TREE_SCOPES.contains(&scope.as_str()) {
                return Err(source_error(format!(
                    "Unknown working-tree scope \"{scope}\""
                )));
            }
            Ok(Source::WorkingTree { scope })
        }
        Some("branch") => {
            let trim = |key: &str| -> String {
                object
                    .get(key)
                    .and_then(Value::as_str)
                    .map(|value| value.trim().to_string())
                    .unwrap_or_default()
            };
            let (base_ref, head_ref) = (trim("baseRef"), trim("headRef"));
            if base_ref.is_empty() || head_ref.is_empty() {
                return Err(source_error(
                    "branch sources require baseRef and headRef".to_string(),
                ));
            }
            Ok(Source::Branch { base_ref, head_ref })
        }
        Some("pr") => {
            let raw_number = object.get("number").cloned().unwrap_or(Value::Null);
            let Some(number) = js_number(&raw_number) else {
                return Err(source_error(
                    "pr sources require a positive number".to_string(),
                ));
            };
            if !number.is_finite()
                || number.fract() != 0.0
                || number <= 0.0
                || number > i64::MAX as f64
            {
                return Err(source_error(
                    "pr sources require a positive number".to_string(),
                ));
            }
            Ok(Source::Pr {
                number: number as i64,
            })
        }
        other => {
            let kind = other
                .map(str::to_string)
                .unwrap_or_else(|| "undefined".to_string());
            Err(source_error(format!("Unknown source kind \"{kind}\"")))
        }
    }
}

/// `sourceKey`: stable string form of a source, used as the pointer key and
/// as part of the cache key. Must not change shape casually — it addresses
/// persisted files.
/// 中文补充：形状一旦改变旧持久化文件将无法寻址，需同步迁移。
pub fn source_key(source: &Source) -> String {
    match source {
        Source::WorkingTree { scope } => format!("working-tree:{scope}"),
        Source::Branch { base_ref, head_ref } => format!("branch:{base_ref}...{head_ref}"),
        Source::Pr { number } => format!("pr:{number}"),
    }
}

/// The git service surface this module consumes (JS imports of
/// `../git/service.js`). Each closure returns the same `Result<_, String>`
/// the ported service does; errors surface as 500s like the JS rejections.
/// 中文补充：以闭包而非 trait 定义，使测试能在字段粒度上做局部替换。
#[derive(Clone)]
pub struct GitDeps {
/// 解析目录所属仓库的根路径。
    pub repository_root: Arc<dyn Fn(String) -> BoxFuture<Result<String, String>> + Send + Sync>,
/// 获取 diff；第二个参数为 true 表示 staged diff。
    pub diff: Arc<dyn Fn(String, bool) -> BoxFuture<Result<String, String>> + Send + Sync>,
/// 获取两个 ref 之间的 range diff。
    pub range_diff:
        Arc<dyn Fn(String, String, String) -> BoxFuture<Result<String, String>> + Send + Sync>,
/// 列出未跟踪文件路径。
    pub untracked_paths:
        Arc<dyn Fn(String) -> BoxFuture<Result<Vec<String>, String>> + Send + Sync>,
/// 获取指定未跟踪文件列表的 diff。
    pub untracked_diffs:
        Arc<dyn Fn(String, Vec<String>) -> BoxFuture<Result<Vec<String>, String>> + Send + Sync>,
}

/// GitDeps 的装配实现。
impl GitDeps {
    /// Production wiring onto the ported git service. Default `contextLines`
    /// of 3 matches the JS defaults (`getDiff`, `getRangeDiff`,
    /// `getUntrackedDiffs`).
/// 中文补充：每个闭包克隆一份 service Arc，避免借用外层变量。
    pub fn real(service: Arc<GitService>) -> Self {
        let for_root = Arc::clone(&service);
        let for_diff = Arc::clone(&service);
        let for_range = Arc::clone(&service);
        let for_list = Arc::clone(&service);
        let for_untracked = Arc::clone(&service);
        Self {
            repository_root: Arc::new(move |directory: String| {
                let service = Arc::clone(&for_root);
                Box::pin(async move { service.get_repository_root(&directory).await })
            }),
            diff: Arc::new(move |directory: String, staged: bool| {
                let service = Arc::clone(&for_diff);
                Box::pin(async move { service.get_diff(&directory, None, staged, Some(3)).await })
            }),
            range_diff: Arc::new(move |directory: String, base: String, head: String| {
                let service = Arc::clone(&for_range);
                Box::pin(async move {
                    service
                        .get_range_diff(&directory, &base, &head, None, 3)
                        .await
                })
            }),
            untracked_paths: Arc::new(move |directory: String| {
                let service = Arc::clone(&for_list);
                Box::pin(async move { service.list_untracked_paths(&directory).await })
            }),
            untracked_diffs: Arc::new(move |directory: String, paths: Vec<String>| {
                let service = Arc::clone(&for_untracked);
                Box::pin(async move { service.get_untracked_diffs(&directory, &paths, 3).await })
            }),
        }
    }
}

/// The PR-diff seam (`deps.getPullRequestDiff` in the JS). Production wires
/// [`super::pull_request::pull_request_differ`]; `None` answers the JS
/// "Pull request diffs are unavailable" 500.
/// 中文补充：签名为 (目录, PR 编号) → (patch, 元数据)。
pub type PrDiffFn =
    Arc<dyn Fn(String, i64) -> BoxFuture<Result<(String, Value), WalkthroughError>> + Send + Sync>;

/// `loadSourceSections`: resolve a source into diff sections plus the source's
/// pass-through metadata.
/// 中文补充：git/PR 接缝错误映射为 500；空 patch 产生空 section 列表。
pub async fn load_source_sections(
    directory: &str,
    source: &Source,
    deps: &GitDeps,
    pr_diff: Option<&PrDiffFn>,
) -> Result<(Vec<super::digest::Section>, Value), WalkthroughError> {
    let git_failure = |error: String| WalkthroughError::internal(error);

    match source {
        Source::WorkingTree { scope } => {
            let mut sections = Vec::new();

            if scope == "all" || scope == "staged" {
                let patch = (deps.diff)(directory.to_string(), true)
                    .await
                    .map_err(git_failure)?;
                if !patch.trim().is_empty() {
                    sections.push(super::digest::Section {
                        scope: "staged".to_string(),
                        patch,
                    });
                }
            }

            if scope == "all" || scope == "working" {
                let patch = (deps.diff)(directory.to_string(), false)
                    .await
                    .map_err(git_failure)?;
                let untracked_paths = (deps.untracked_paths)(directory.to_string())
                    .await
                    .map_err(git_failure)?;
                let untracked = if untracked_paths.is_empty() {
                    Vec::new()
                } else {
                    (deps.untracked_diffs)(directory.to_string(), untracked_paths)
                        .await
                        .map_err(git_failure)?
                        .into_iter()
                        .filter(|patch| !patch.trim().is_empty())
                        .collect::<Vec<_>>()
                };
                // `git diff` never reports untracked files, so a brand-new
                // file would be invisible in a walkthrough of local work.
                let combined = std::iter::once(patch)
                    .chain(untracked)
                    .filter(|patch| !patch.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !combined.trim().is_empty() {
                    sections.push(super::digest::Section {
                        scope: "working".to_string(),
                        patch: combined,
                    });
                }
            }

            Ok((sections, json!({})))
        }
        Source::Branch { base_ref, head_ref } => {
            let patch =
                (deps.range_diff)(directory.to_string(), base_ref.clone(), head_ref.clone())
                    .await
                    .map_err(git_failure)?;
            let sections = if patch.trim().is_empty() {
                Vec::new()
            } else {
                vec![super::digest::Section {
                    scope: "branch".to_string(),
                    patch,
                }]
            };
            Ok((
                sections,
                json!({ "baseRef": base_ref, "headRef": head_ref }),
            ))
        }
        Source::Pr { number } => {
            let Some(pr_diff) = pr_diff else {
                return Err(WalkthroughError::with_code(
                    "Pull request diffs are unavailable",
                    500,
                    "pr-diffs-unavailable",
                ));
            };
            let (patch, meta) = (pr_diff)(directory.to_string(), *number).await?;
            let sections = if patch.trim().is_empty() {
                Vec::new()
            } else {
                vec![super::digest::Section {
                    scope: format!("pr:{number}"),
                    patch,
                }]
            };
            Ok((sections, meta))
        }
    }
}

/// sources 模块单测：source 解析校验、source key 形状与 diff section 组装。
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 构造固定返回指定 diff（其余调用返回空）的假 git 接缝。
    fn deps_returning(diff: &'static str) -> GitDeps {
        GitDeps {
            repository_root: Arc::new(|_directory| Box::pin(async { Ok("/repo".to_string()) })),
            diff: Arc::new(move |_directory, _staged| {
                Box::pin(async move { Ok(diff.to_string()) })
            }),
            range_diff: Arc::new(|_d, _b, _h| Box::pin(async { Ok(String::new()) })),
            untracked_paths: Arc::new(|_directory| Box::pin(async { Ok(Vec::new()) })),
            untracked_diffs: Arc::new(|_d, _paths| Box::pin(async { Ok(Vec::new()) })),
        }
    }

    /// 缺失或非对象的 source 被拒绝并报「source is required」（400）。
    #[test]
    fn parse_source_rejects_missing_and_non_object_sources() {
        let error = parse_source(None).unwrap_err();
        assert_eq!(error.message, "source is required");
        assert_eq!(error.status, 400);
        let error = parse_source(Some(&json!("working-tree"))).unwrap_err();
        assert_eq!(error.message, "source is required");
        let error = parse_source(Some(&json!(null))).unwrap_err();
        assert_eq!(error.message, "source is required");
    }

    /// working-tree scope 归一化：缺省为 all，未知值报错。
    #[test]
    fn parse_source_normalizes_working_tree_scopes() {
        let source = parse_source(Some(&json!({ "kind": "working-tree" }))).unwrap();
        assert_eq!(
            source,
            Source::WorkingTree {
                scope: "all".to_string()
            }
        );
        let source =
            parse_source(Some(&json!({ "kind": "working-tree", "scope": "staged" }))).unwrap();
        assert_eq!(
            source,
            Source::WorkingTree {
                scope: "staged".to_string()
            }
        );
        let error =
            parse_source(Some(&json!({ "kind": "working-tree", "scope": "nope" }))).unwrap_err();
        assert_eq!(error.message, "Unknown working-tree scope \"nope\"");
    }

    /// branch source 的 ref 会被 trim，且 baseRef/headRef 缺一不可。
    #[test]
    fn parse_source_validates_branch_refs() {
        let source = parse_source(Some(&json!({
            "kind": "branch", "baseRef": " main ", "headRef": "feature"
        })))
        .unwrap();
        assert_eq!(
            source,
            Source::Branch {
                base_ref: "main".to_string(),
                head_ref: "feature".to_string()
            }
        );
        let error = parse_source(Some(&json!({ "kind": "branch", "baseRef": "" }))).unwrap_err();
        assert_eq!(error.message, "branch sources require baseRef and headRef");
    }

    /// PR 编号接受数字与数字字符串（JS Number 强转），拒绝非正数与非整数。
    #[test]
    fn parse_source_validates_pr_numbers() {
        let source = parse_source(Some(&json!({ "kind": "pr", "number": 2122 }))).unwrap();
        assert_eq!(source, Source::Pr { number: 2122 });
        // JS `Number("2122")` coerces.
        let source = parse_source(Some(&json!({ "kind": "pr", "number": "2122" }))).unwrap();
        assert_eq!(source, Source::Pr { number: 2122 });
        for bad in [json!(0), json!(-1), json!(2.5), json!("x"), json!(null)] {
            let error = parse_source(Some(&json!({ "kind": "pr", "number": bad }))).unwrap_err();
            assert_eq!(error.message, "pr sources require a positive number");
        }
    }

    /// 未知 kind（含缺失 kind）被拒绝并回显该 kind。
    #[test]
    fn parse_source_rejects_unknown_kinds() {
        let error = parse_source(Some(&json!({ "kind": "spelunking" }))).unwrap_err();
        assert_eq!(error.message, "Unknown source kind \"spelunking\"");
        let error = parse_source(Some(&json!({}))).unwrap_err();
        assert_eq!(error.message, "Unknown source kind \"undefined\"");
    }

    /// source_key 的字符串形状保持稳定，因为它寻址持久化文件。
    #[test]
    fn source_key_has_a_stable_shape() {
        assert_eq!(
            source_key(&Source::WorkingTree {
                scope: "all".to_string()
            }),
            "working-tree:all"
        );
        assert_eq!(
            source_key(&Source::Branch {
                base_ref: "main".to_string(),
                head_ref: "feature".to_string()
            }),
            "branch:main...feature"
        );
        assert_eq!(source_key(&Source::Pr { number: 2122 }), "pr:2122");
    }

    /// scope=all 时 staged 与 working（含 untracked 文件）分为两个 section。
    #[tokio::test]
    async fn working_tree_splits_staged_and_untracked_sections() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deps = GitDeps {
            repository_root: Arc::new(|_d| Box::pin(async { Ok("/repo".to_string()) })),
            diff: Arc::new(|directory: String, staged: bool| {
                Box::pin(async move {
                    assert_eq!(directory, "/repo");
                    Ok(if staged {
                        "diff --git a/s b/s\n".to_string()
                    } else {
                        "diff --git a/w b/w\n".to_string()
                    })
                })
            }),
            range_diff: Arc::new(|_d, _b, _h| {
                let future: BoxFuture<Result<String, String>> =
                    Box::pin(async { Ok(String::new()) });
                future
            }),
            untracked_paths: {
                let calls = Arc::clone(&calls);
                Arc::new(move |_d| {
                    let calls = Arc::clone(&calls);
                    let future: BoxFuture<Result<Vec<String>, String>> = Box::pin(async move {
                        calls.lock().unwrap().push("untracked-list");
                        Ok(vec!["new.ts".to_string()])
                    });
                    future
                })
            },
            untracked_diffs: Arc::new(|_d, _paths| {
                Box::pin(async { Ok(vec!["diff --git a/new.ts b/new.ts\n".to_string()]) })
            }),
        };

        let (sections, meta) = load_source_sections(
            "/repo",
            &Source::WorkingTree {
                scope: "all".to_string(),
            },
            &deps,
            None,
        )
        .await
        .unwrap();

        assert_eq!(meta, json!({}));
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].scope, "staged");
        assert_eq!(sections[0].patch, "diff --git a/s b/s\n");
        assert_eq!(sections[1].scope, "working");
        assert!(sections[1].patch.contains("a/w b/w"));
        assert!(sections[1].patch.contains("a/new.ts b/new.ts"));
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    /// staged scope 不取工作区 diff，working scope 不取 staged diff。
    #[tokio::test]
    async fn working_scope_omits_staged_and_staged_scope_omits_working() {
        let deps = deps_returning("diff --git a/x b/x\n");
        let (sections, _) = load_source_sections(
            "/repo",
            &Source::WorkingTree {
                scope: "staged".to_string(),
            },
            // The staged call happens through `diff(directory, true)`.
            &GitDeps {
                diff: Arc::new(|_d, staged| {
                    Box::pin(async move {
                        assert!(staged);
                        Ok("diff --git a/s b/s\n".to_string())
                    })
                }),
                ..deps
            },
            None,
        )
        .await
        .unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].scope, "staged");

        let deps = deps_returning("");
        let (sections, _) = load_source_sections(
            "/repo",
            &Source::WorkingTree {
                scope: "working".to_string(),
            },
            &GitDeps {
                diff: Arc::new(|_d, staged| {
                    Box::pin(async move {
                        assert!(!staged);
                        Ok("".to_string())
                    })
                }),
                ..deps
            },
            None,
        )
        .await
        .unwrap();
        assert!(sections.is_empty());
    }

    /// branch source 走 range diff，以单 section 返回并携带 ref 元数据。
    #[tokio::test]
    async fn branch_sources_use_the_range_diff() {
        let deps = GitDeps {
            repository_root: Arc::new(|_d| Box::pin(async { Ok("/repo".to_string()) })),
            diff: Arc::new(|_d, _s| Box::pin(async { Ok(String::new()) })),
            range_diff: Arc::new(|_directory, base, head| {
                Box::pin(async move {
                    assert_eq!(base, "main");
                    assert_eq!(head, "feature");
                    Ok("diff --git a/b b/b\n".to_string())
                })
            }),
            untracked_paths: Arc::new(|_d| Box::pin(async { Ok(Vec::new()) })),
            untracked_diffs: Arc::new(|_d, _p| Box::pin(async { Ok(Vec::new()) })),
        };

        let (sections, meta) = load_source_sections(
            "/repo",
            &Source::Branch {
                base_ref: "main".to_string(),
                head_ref: "feature".to_string(),
            },
            &deps,
            None,
        )
        .await
        .unwrap();

        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].scope, "branch");
        assert_eq!(meta, json!({ "baseRef": "main", "headRef": "feature" }));
    }

    /// 未注入 PR differ 时，PR source 返回 500「Pull request diffs are unavailable」。
    #[tokio::test]
    async fn pr_sources_answer_500_without_a_differ() {
        let deps = deps_returning("");
        let error = load_source_sections("/repo", &Source::Pr { number: 7 }, &deps, None)
            .await
            .unwrap_err();
        assert_eq!(error.status, 500);
        assert_eq!(error.message, "Pull request diffs are unavailable");
    }

    /// PR diff 来自接缝，section scope 按 PR 编号区分。
    #[tokio::test]
    async fn pr_sources_use_the_differ_and_scope_by_number() {
        let deps = deps_returning("");
        let differ: PrDiffFn = Arc::new(|directory, number| {
            Box::pin(async move {
                assert_eq!(directory, "/repo");
                assert_eq!(number, 2122);
                Ok((
                    "diff --git a/p b/p\n".to_string(),
                    json!({ "owner": "ompchamber", "repo": "ompchamber", "number": 2122 }),
                ))
            })
        });
        let (sections, meta) =
            load_source_sections("/repo", &Source::Pr { number: 2122 }, &deps, Some(&differ))
                .await
                .unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].scope, "pr:2122");
        assert_eq!(meta["owner"], json!("ompchamber"));
    }

    /// git 接缝的错误原样透传为 500 internal 错误。
    #[tokio::test]
    async fn git_failures_surface_as_internal_errors() {
        let deps = GitDeps {
            repository_root: Arc::new(|_d| Box::pin(async { Ok("/repo".to_string()) })),
            diff: Arc::new(|_d, _s| {
                let future: BoxFuture<Result<String, String>> =
                    Box::pin(async { Err("not a repository".to_string()) });
                future
            }),
            range_diff: Arc::new(|_d, _b, _h| Box::pin(async { Ok(String::new()) })),
            untracked_paths: Arc::new(|_d| Box::pin(async { Ok(Vec::new()) })),
            untracked_diffs: Arc::new(|_d, _p| Box::pin(async { Ok(Vec::new()) })),
        };
        let error = load_source_sections(
            "/repo",
            &Source::WorkingTree {
                scope: "all".to_string(),
            },
            &deps,
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(error.status, 500);
        assert_eq!(error.message, "not a repository");
    }
}
