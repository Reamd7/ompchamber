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

use std::sync::Arc;

use serde_json::{Value, json};

use super::error::WalkthroughError;
use super::small_model::BoxFuture;
use crate::git_service::service::GitService;

/// `WORKING_TREE_SCOPES`.
const WORKING_TREE_SCOPES: [&str; 3] = ["all", "staged", "working"];

/// A normalized walkthrough source (`parseSource`'s output).
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    WorkingTree { scope: String },
    Branch { base_ref: String, head_ref: String },
    Pr { number: i64 },
}

impl Source {
    /// The normalized descriptor the routes echo back to the client.
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
#[derive(Clone)]
pub struct GitDeps {
    pub repository_root: Arc<dyn Fn(String) -> BoxFuture<Result<String, String>> + Send + Sync>,
    pub diff: Arc<dyn Fn(String, bool) -> BoxFuture<Result<String, String>> + Send + Sync>,
    pub range_diff:
        Arc<dyn Fn(String, String, String) -> BoxFuture<Result<String, String>> + Send + Sync>,
    pub untracked_paths:
        Arc<dyn Fn(String) -> BoxFuture<Result<Vec<String>, String>> + Send + Sync>,
    pub untracked_diffs:
        Arc<dyn Fn(String, Vec<String>) -> BoxFuture<Result<Vec<String>, String>> + Send + Sync>,
}

impl GitDeps {
    /// Production wiring onto the ported git service. Default `contextLines`
    /// of 3 matches the JS defaults (`getDiff`, `getRangeDiff`,
    /// `getUntrackedDiffs`).
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
pub type PrDiffFn =
    Arc<dyn Fn(String, i64) -> BoxFuture<Result<(String, Value), WalkthroughError>> + Send + Sync>;

/// `loadSourceSections`: resolve a source into diff sections plus the source's
/// pass-through metadata.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn parse_source_rejects_unknown_kinds() {
        let error = parse_source(Some(&json!({ "kind": "spelunking" }))).unwrap_err();
        assert_eq!(error.message, "Unknown source kind \"spelunking\"");
        let error = parse_source(Some(&json!({}))).unwrap_err();
        assert_eq!(error.message, "Unknown source kind \"undefined\"");
    }

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

    #[tokio::test]
    async fn pr_sources_answer_500_without_a_differ() {
        let deps = deps_returning("");
        let error = load_source_sections("/repo", &Source::Pr { number: 7 }, &deps, None)
            .await
            .unwrap_err();
        assert_eq!(error.status, 500);
        assert_eq!(error.message, "Pull request diffs are unavailable");
    }

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
