//! Tests ported from `server/lib/walkthrough/jobs.test.js`,
//! `language.test.js`, and the readiness paths of `reproduce-2607.test.js`.
//!
//! The git service and the small-model chain are faked at exactly the seams
//! the JS tests mock (`../git/service.js` and `../small-model/index.js`), so
//! the real digest, prompt, normalization, and store paths are exercised.

//!
//! 中文说明：git 与小模型链在接缝处注入假实现，真实的 digest、prompt、
//! 归一化与 store 路径全部被覆盖；断言对齐 JS 版行为。
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::service::{
    WalkthroughService, generation_timeout_ms, utf16_len, walkthrough_output_tokens,
};
use super::small_model::{
    CancelSignal, DescribeModelRequest, GenerateTextOutput, GenerateTextRequest, ModelDescription,
    SmallModelError, SmallModelSeam, abort_error,
};
use super::sources::GitDeps;
use crate::walkthrough::error::WalkthroughError;

/// 测试默认的 source 描述符（working-tree / all）。
fn source_value() -> Value {
    json!({ "kind": "working-tree", "scope": "all" })
}
/// 默认 source 对应的稳定键。
const SOURCE_KEY: &str = "working-tree:all";
/// 假 git 接缝固定返回的仓库根。
const REPO_ROOT: &str = "/repo";

/// 默认的未暂存 diff：单文件、单 hunk、一行新增。
const PATCH: &str = "diff --git a/src/a.ts b/src/a.ts\n--- a/src/a.ts\n+++ b/src/a.ts\n@@ -1,1 +1,2 @@\n+const added = true;\n";

/// 假模型默认返回的合法 walkthrough JSON（单章单 stop，锚定 h1）。
const RESPONSE: &str = r#"{"title":"Change","focus":"why","chapters":[{"title":"Data","icon":"doc","blurb":"","stops":[{"title":"Adds a flag","hunks":["h1"],"importance":"normal","prose":"It adds a flag."}]}]}"#;

/// 构造一个各方面都可用的默认模型描述。
fn default_model() -> ModelDescription {
    ModelDescription {
        provider_id: "anthropic".to_string(),
        model_id: "claude-haiku-4-5".to_string(),
        source: "config".to_string(),
        has_login: Some(true),
        input_char_budget: 1_000_000,
        context_tokens: Some(200_000),
        context_known: Some(true),
        output_tokens: None,
        structured_output: Some(true),
        output_token_limit: None,
    }
}

/// 记录一次模型 generate 调用的关键参数。
#[derive(Debug, Clone)]
struct GenerateCall {
    /// 请求的 system prompt。
    system: String,
    /// 请求是否携带 response schema。
    used_schema: bool,
    /// 请求申请的输出 token 数。
    max_output_tokens: i64,
    /// 请求的 `provider/model` 标签。
    model: String,
}

/// 假接缝的共享可变状态：既驱动行为，也记录调用。
#[derive(Default)]
struct FakeState {
    /// describe 返回的模型；置 `None` 模拟无模型。
    model: Mutex<Option<ModelDescription>>,
    /// generate 返回的原始文本。
    response_text: Mutex<String>,
    /// Park the model call until `released` (or cancel) when set.
    /// 中文补充：用于模拟一次挂起的长时间模型调用。
    hang: AtomicBool,
    /// 解除挂起的开关。
    released: AtomicBool,
    /// Answer a schema request with a 400 (provider refused the shape).
    /// 中文补充：驱动 schema-拒绝后的降级重试路径。
    fail_schema_with_400: AtomicBool,
    /// 每次 generate 调用的参数记录。
    generate_calls: Mutex<Vec<GenerateCall>>,
    /// 每次模型调用时观察到的阶段快照。
    seen_stages: Mutex<Vec<Option<&'static str>>>,
    /// The unstaged diff the fake git returns (mutable for cache-miss tests).
    /// 中文补充：换 diff 即换缓存键，用于制造缓存未命中。
    unstaged_patch: Mutex<String>,
    /// 回填的服务句柄，让假 generate 能读取真实阶段。
    service_slot: Mutex<Option<Arc<WalkthroughService>>>,
}

/// 假状态的便捷读取与修改方法。
impl FakeState {
    /// 已发生的模型调用次数。
    fn calls(&self) -> usize {
        self.generate_calls.lock().unwrap().len()
    }

    /// 最近一次模型调用的参数记录。
    fn last_call(&self) -> GenerateCall {
        self.generate_calls.lock().unwrap().last().unwrap().clone()
    }

    /// 替换假 git 返回的未暂存 diff。
    fn set_unstaged(&self, patch: &str) {
        *self.unstaged_patch.lock().unwrap() = patch.to_string();
    }
}

/// 一个测试用的服务实例及其假状态与临时目录。
struct Harness {
    /// 被测服务。
    service: Arc<WalkthroughService>,
    /// 驱动假接缝的共享状态。
    state: Arc<FakeState>,
    /// 临时数据目录，drop 时整体删除。
    data_dir: PathBuf,
}

/// 创建按「测试名-进程-序号」命名的唯一临时目录，避免并行测试互踩。
fn temp_data_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "walkthrough-jobs-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// 以默认模型搭建测试 harness。
fn harness(label: &str) -> Harness {
    harness_with(label, default_model())
}

/// 以指定模型搭建 harness：注入假 describe/generate/git 接缝，store 落在真实临时目录。
fn harness_with(label: &str, model: ModelDescription) -> Harness {
    let state = Arc::new(FakeState {
        model: Mutex::new(Some(model)),
        response_text: Mutex::new(RESPONSE.to_string()),
        unstaged_patch: Mutex::new(PATCH.to_string()),
        ..FakeState::default()
    });

    let describe_state = Arc::clone(&state);
    let describe = Arc::new(move |_request: DescribeModelRequest| {
        let state = Arc::clone(&describe_state);
        let future: super::small_model::BoxFuture<
            Result<Option<ModelDescription>, SmallModelError>,
        > = Box::pin(async move { Ok(state.model.lock().unwrap().clone()) });
        future
    });

    let generate_state = Arc::clone(&state);
    let generate = Arc::new(move |request: GenerateTextRequest| {
        let state = Arc::clone(&generate_state);
        let future: super::small_model::BoxFuture<Result<GenerateTextOutput, SmallModelError>> =
            Box::pin(async move {
                if let Some(service) = state.service_slot.lock().unwrap().clone() {
                    state
                        .seen_stages
                        .lock()
                        .unwrap()
                        .push(service.generation_stage(REPO_ROOT, SOURCE_KEY));
                }
                let used_schema = request.response_schema.is_some();
                state.generate_calls.lock().unwrap().push(GenerateCall {
                    system: request.system.clone(),
                    used_schema,
                    max_output_tokens: request.max_output_tokens,
                    model: request.model.clone(),
                });

                if used_schema && state.fail_schema_with_400.load(Ordering::SeqCst) {
                    return Err(SmallModelError {
                        message: "bad request".to_string(),
                        status: Some(400),
                        ..Default::default()
                    });
                }
                // Park until released or cancelled, mirroring a long model call.
                while state.hang.load(Ordering::SeqCst) && !state.released.load(Ordering::SeqCst) {
                    if request.cancel.is_cancelled() {
                        return Err(abort_error());
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                if request.cancel.is_cancelled() {
                    return Err(abort_error());
                }
                Ok(GenerateTextOutput {
                    text: state.response_text.lock().unwrap().clone(),
                })
            });
        future
    });

    let seam = SmallModelSeam { describe, generate };

    let unstaged = Arc::clone(&state);
    let git = GitDeps {
        repository_root: Arc::new(|_directory| Box::pin(async { Ok(REPO_ROOT.to_string()) })),
        diff: Arc::new(move |_directory, staged| {
            let unstaged = Arc::clone(&unstaged);
            Box::pin(async move {
                if staged {
                    Ok(String::new())
                } else {
                    Ok(unstaged.unstaged_patch.lock().unwrap().clone())
                }
            })
        }),
        range_diff: Arc::new(|_d, _b, _h| Box::pin(async { Ok(String::new()) })),
        untracked_paths: Arc::new(|_d| Box::pin(async { Ok(Vec::new()) })),
        untracked_diffs: Arc::new(|_d, _p| Box::pin(async { Ok(Vec::new()) })),
    };

    let data_dir = temp_data_dir(label);
    let service = WalkthroughService::new(data_dir.clone(), git, seam, None);
    *state.service_slot.lock().unwrap() = Some(Arc::clone(&service));

    Harness {
        service,
        state,
        data_dir,
    }
}

/// 测试结束后清理临时数据目录。
impl Drop for Harness {
    /// 删除 harness 的临时目录。
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

/// 轮询等待谓词成立，超时 panic（对应 JS 的 waitFor）。
async fn wait_until(predicate: impl Fn() -> bool) {
    for _ in 0..2000 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("waitFor timed out");
}

/// 以默认参数（无语言、无显式模型、不强制）发起生成请求。
async fn generate(service: &Arc<WalkthroughService>) -> Result<Value, WalkthroughError> {
    service
        .generate_walkthrough("/repo", Some(&source_value()), false, None, None)
        .await
}

/// 以指定语言发起生成请求。
async fn generate_in(
    service: &Arc<WalkthroughService>,
    language: &str,
) -> Result<Value, WalkthroughError> {
    service
        .generate_walkthrough("/repo", Some(&source_value()), false, None, Some(language))
        .await
}

/// 以指定语言读取 walkthrough，断言成功并返回响应对象。
async fn read_in(service: &Arc<WalkthroughService>, language: &str) -> Value {
    service
        .get_walkthrough("/repo", Some(&source_value()), None, Some(language))
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Generation jobs
// ---------------------------------------------------------------------------

/// 第二个请求附着到运行中的任务：只付一次模型调用，两个结果相同。
#[tokio::test]
async fn runs_a_second_request_against_the_same_job_instead_of_paying_twice() {
    let harness = harness("attach");
    harness.state.hang.store(true, Ordering::SeqCst);

    let first = {
        let service = Arc::clone(&harness.service);
        tokio::spawn(async move { generate(&service).await })
    };
    // Let the first call reach the model before the second arrives, which is
    // what a refresh-then-press-again actually looks like.
    wait_until(|| harness.state.calls() == 1).await;
    let second = {
        let service = Arc::clone(&harness.service);
        tokio::spawn(async move { generate(&service).await })
    };
    // Let the second call run far enough to attach to the running job before
    // the model call is released — what a refresh-then-press-again looks like.
    tokio::time::sleep(Duration::from_millis(20)).await;

    harness.state.released.store(true, Ordering::SeqCst);
    let (a, b) = (
        first.await.unwrap().unwrap(),
        second.await.unwrap().unwrap(),
    );

    assert_eq!(harness.state.calls(), 1);
    assert_eq!(a["walkthrough"]["title"], json!("Change"));
    assert_eq!(a, b);
}

/// `isGenerating` 在任务运行时为真、结束后为假。
#[tokio::test]
async fn reports_a_running_job_so_a_returning_client_can_show_progress() {
    let harness = harness("running");
    harness.state.hang.store(true, Ordering::SeqCst);

    let running = {
        let service = Arc::clone(&harness.service);
        tokio::spawn(async move { generate(&service).await })
    };
    wait_until(|| harness.service.is_generating(REPO_ROOT, SOURCE_KEY)).await;

    harness.state.released.store(true, Ordering::SeqCst);
    running.await.unwrap().unwrap();
    assert!(!harness.service.is_generating(REPO_ROOT, SOURCE_KEY));
}

/// 只有显式取消请求会中止任务：返回 abort 错误并注销注册。
#[tokio::test]
async fn stops_only_on_an_explicit_cancel() {
    let harness = harness("cancel");
    harness.state.hang.store(true, Ordering::SeqCst);

    let running = {
        let service = Arc::clone(&harness.service);
        tokio::spawn(async move { generate(&service).await })
    };
    wait_until(|| harness.service.is_generating(REPO_ROOT, SOURCE_KEY)).await;

    let cancelled = harness
        .service
        .cancel_generation("/repo", Some(&source_value()))
        .await
        .unwrap();
    assert_eq!(cancelled, json!({ "cancelled": true }));

    let error = running.await.unwrap().unwrap_err();
    assert_eq!(error.status, 500);
    assert_eq!(error.message, "The operation was aborted");
    assert!(!harness.service.is_generating(REPO_ROOT, SOURCE_KEY));
}

/// 无任务运行时取消返回 `{"cancelled": false}`。
#[tokio::test]
async fn reports_nothing_to_cancel_when_no_job_is_running() {
    let harness = harness("cancel-none");
    let cancelled = harness
        .service
        .cancel_generation("/repo", Some(&source_value()))
        .await
        .unwrap();
    assert_eq!(cancelled, json!({ "cancelled": false }));
}

// The reserve and the request must be the same number: asking for more than
// was subtracted from the input allowance overruns the context mid-answer.
/// 请求的 max_output_tokens 必须等于模型解析时预留的储备数字。
#[tokio::test]
async fn requests_exactly_the_budget_the_model_resolution_reserved() {
    let mut model = default_model();
    model.provider_id = "opencode-go".to_string();
    model.model_id = "deepseek-v4-flash".to_string();
    model.output_tokens = Some(96_000);
    model.output_token_limit = Some(384_000);
    let harness = harness_with("budget", model);

    generate(&harness.service).await.unwrap();

    assert_eq!(harness.state.last_call().max_output_tokens, 96_000);
    assert_eq!(
        harness.state.last_call().model,
        "opencode-go/deepseek-v4-flash"
    );
}

/// 首次生成完成后，相同请求直接命中缓存，不再调用模型。
#[tokio::test]
async fn serves_the_cache_once_the_job_has_finished_without_calling_the_model_again() {
    let harness = harness("cache");

    generate(&harness.service).await.unwrap();
    let calls_after_first = harness.state.calls();

    let second = generate(&harness.service).await.unwrap();

    assert_eq!(second["fromCache"], json!(true));
    assert_eq!(harness.state.calls(), calls_after_first);
}

// ---------------------------------------------------------------------------
// Generation timeout (a fixed deadline made a three-hunk edit and a 500-hunk
// pull request wait the same)
// ---------------------------------------------------------------------------

/// 小 diff 拿到基础超时下限。
#[test]
fn generation_timeout_gives_a_small_diff_a_floor() {
    assert_eq!(generation_timeout_ms(0), 120_000);
    assert_eq!(generation_timeout_ms(3), 123_000);
}

/// 超时随 hunk 数量线性增长。
#[test]
fn generation_timeout_grows_with_the_work() {
    assert!(generation_timeout_ms(515) > generation_timeout_ms(138));
    assert_eq!(generation_timeout_ms(515), 635_000);
}

/// 超时被上限夹住，不随 hunk 数无限增长。
#[test]
fn generation_timeout_stays_bounded() {
    assert_eq!(generation_timeout_ms(100_000), 900_000);
}

// ---------------------------------------------------------------------------
// Output budget (the failure this replaced: a flat 24k ask, spent entirely on
// reasoning by a model that advertises 384k output tokens)
// ---------------------------------------------------------------------------

/// 宽裕模型的输出预算远超旧的固定 24k。
#[test]
fn output_budget_asks_a_roomy_model_for_far_more_than_the_old_fixed_budget() {
    assert_eq!(walkthrough_output_tokens(1_000_000, Some(384_000)), 96_000);
}

/// 预算绝不超过模型自报的输出上限。
#[test]
fn output_budget_never_asks_for_more_than_the_model_says_it_can_emit() {
    assert_eq!(walkthrough_output_tokens(202_752, Some(32_768)), 32_768);
}

/// 预算被上下文份额约束。
#[test]
fn output_budget_keeps_the_reserve_to_a_share_of_the_context() {
    assert_eq!(walkthrough_output_tokens(200_000, Some(64_000)), 50_000);
}

/// 小上下文或未编目模型仍保住旧的 24k 下限。
#[test]
fn output_budget_holds_the_old_floor_for_a_small_or_uncatalogued_model() {
    assert_eq!(walkthrough_output_tokens(64_000, None), 24_000);
    assert_eq!(walkthrough_output_tokens(0, None), 24_000);
}

/// 模型自身上限低于下限时，以模型上限为准。
#[test]
fn output_budget_yields_to_a_model_whose_own_limit_is_below_the_floor() {
    assert_eq!(walkthrough_output_tokens(128_000, Some(8_192)), 8_192);
}

// ---------------------------------------------------------------------------
// Generation stages
// ---------------------------------------------------------------------------

/// 模型运行期间阶段为 asking，任务结束后阶段清空。
#[tokio::test]
async fn reports_asking_while_the_model_runs_and_clears_when_the_job_ends() {
    let harness = harness("stage-asking");
    harness.state.hang.store(true, Ordering::SeqCst);

    let running = {
        let service = Arc::clone(&harness.service);
        tokio::spawn(async move { generate(&service).await })
    };
    wait_until(|| harness.service.generation_stage(REPO_ROOT, SOURCE_KEY) == Some("asking")).await;

    harness.state.released.store(true, Ordering::SeqCst);
    running.await.unwrap().unwrap();

    assert_eq!(
        harness.service.generation_stage(REPO_ROOT, SOURCE_KEY),
        None
    );
}

/// 只有 provider 拒绝 schema 才会观察到 retrying 阶段。
#[tokio::test]
async fn reports_retrying_only_when_a_provider_rejects_the_schema() {
    let harness = harness("stage-retrying");
    harness
        .state
        .fail_schema_with_400
        .store(true, Ordering::SeqCst);

    generate(&harness.service).await.unwrap();

    let seen = harness.state.seen_stages.lock().unwrap().clone();
    assert_eq!(seen, vec![Some("asking"), Some("retrying")]);
}

// Retrying the schema on every generation means paying for a call already
// known to fail; the refusal has to be remembered.
/// 拒绝过 schema 的模型在后续生成中直接走无 schema 路径。
#[tokio::test]
async fn stops_sending_a_schema_to_a_model_that_already_rejected_one() {
    let harness = harness("schema-memory");
    harness
        .state
        .fail_schema_with_400
        .store(true, Ordering::SeqCst);

    generate(&harness.service).await.unwrap();
    let sent: Vec<bool> = harness
        .state
        .generate_calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| call.used_schema)
        .collect();
    assert_eq!(sent, vec![true, false]);

    // A different diff, so the cache cannot answer instead.
    harness
        .state
        .set_unstaged(&PATCH.replace("const added = true;", "const added = false;"));
    generate(&harness.service).await.unwrap();

    let sent: Vec<bool> = harness
        .state
        .generate_calls
        .lock()
        .unwrap()
        .iter()
        .map(|call| call.used_schema)
        .collect();
    assert_eq!(sent, vec![true, false, false]);
}

// ---------------------------------------------------------------------------
// Issue 2607: a walkthrough model whose provider has no usable login
// ---------------------------------------------------------------------------

/// 构造 provider 无可用登录的模型描述（issue 2607 场景）。
fn unauthenticated_model() -> ModelDescription {
    ModelDescription {
        provider_id: "deepseek".to_string(),
        model_id: "deepseek-v4-flash".to_string(),
        source: "config".to_string(),
        has_login: Some(false),
        input_char_budget: 1_000_000,
        context_tokens: Some(128_000),
        context_known: Some(true),
        output_tokens: None,
        structured_output: None,
        output_token_limit: None,
    }
}

/// 无登录时 readiness 报 no-provider-login 且不回传模型。
#[tokio::test]
async fn readiness_reports_not_ready_without_a_login_and_omits_the_model() {
    let harness = harness_with("2607-read", unauthenticated_model());

    let result = harness
        .service
        .get_walkthrough("/repo", Some(&source_value()), None, None)
        .await
        .unwrap();

    assert_eq!(result["readiness"]["ready"], json!(false));
    assert_eq!(result["readiness"]["reason"], json!("no-provider-login"));
    // Unusable models must not be offered as the current selection.
    assert!(result["readiness"].get("model").is_none());
}

/// 无登录时生成返回 401 结构化错误并携带模型信息。
#[tokio::test]
async fn generation_rejects_with_structured_no_provider_login() {
    let harness = harness_with("2607-gen", unauthenticated_model());

    let error = generate(&harness.service).await.unwrap_err();
    assert_eq!(error.status, 401);
    assert_eq!(error.code.as_deref(), Some("no-provider-login"));
    assert_eq!(
        error.message,
        "No OpenCode login found for provider \"deepseek\" — sign in or choose a different model"
    );
    assert_eq!(
        error.model.as_ref().unwrap()["providerID"],
        json!("deepseek")
    );
    assert_eq!(
        error.model.as_ref().unwrap()["modelID"],
        json!("deepseek-v4-flash")
    );
}

// ---------------------------------------------------------------------------
// Readiness reasons
// ---------------------------------------------------------------------------

/// 模型解析失败时 readiness 报 no-model。
#[tokio::test]
async fn readiness_reports_no_model_when_resolution_fails() {
    let harness = harness("no-model");
    *harness.state.model.lock().unwrap() = None;

    let result = harness
        .service
        .get_walkthrough("/repo", Some(&source_value()), None, None)
        .await
        .unwrap();

    assert_eq!(result["readiness"]["ready"], json!(false));
    assert_eq!(result["readiness"]["reason"], json!("no-model"));
}

/// readiness 区分「无变更」与「只有生成文件变更」两种拒绝理由。
#[tokio::test]
async fn readiness_distinguishes_empty_from_only_generated() {
    let empty = harness("empty-diff");
    empty.state.set_unstaged("");
    let result = empty
        .service
        .get_walkthrough("/repo", Some(&source_value()), None, None)
        .await
        .unwrap();
    assert_eq!(result["readiness"]["reason"], json!("empty-diff"));

    let generated = harness("only-generated");
    generated
        .state
        .set_unstaged("diff --git a/bun.lock b/bun.lock\n--- a/bun.lock\n+++ b/bun.lock\n@@ -1,1 +1,2 @@\n+  \"a\": 1,\n");
    let result = generated
        .service
        .get_walkthrough("/repo", Some(&source_value()), None, None)
        .await
        .unwrap();
    assert_eq!(result["readiness"]["reason"], json!("only-generated"));
    assert_eq!(result["readiness"]["generatedFileCount"], json!(1));
}

/// 不支持 structured output 的模型被 readiness 拒绝并给出 requiredChars。
#[tokio::test]
async fn readiness_refuses_models_without_structured_output() {
    let mut model = default_model();
    model.structured_output = Some(false);
    let harness = harness_with("structured", model);

    let result = harness
        .service
        .get_walkthrough("/repo", Some(&source_value()), None, None)
        .await
        .unwrap();

    assert_eq!(
        result["readiness"]["reason"],
        json!("structured-output-unsupported")
    );
    assert!(result["readiness"]["requiredChars"].is_u64());
}

/// prompt 超出模型字符预算时 readiness 报 context-too-small。
#[tokio::test]
async fn readiness_refuses_when_the_prompt_does_not_fit_the_context() {
    let mut model = default_model();
    model.input_char_budget = 10;
    let harness = harness_with("too-small", model);

    let result = harness
        .service
        .get_walkthrough("/repo", Some(&source_value()), None, None)
        .await
        .unwrap();

    assert_eq!(result["readiness"]["reason"], json!("context-too-small"));
    assert_eq!(result["readiness"]["availableChars"], json!(10));
    let required = result["readiness"]["requiredChars"].as_i64().unwrap();
    assert!(required > 10);
}

/// 模型可用时 readiness 为 ready，并附带 hunk/file 计数与模型信息。
#[tokio::test]
async fn readiness_is_ready_for_a_capable_model_with_counts() {
    let harness = harness("ready");

    let result = harness
        .service
        .get_walkthrough("/repo", Some(&source_value()), None, None)
        .await
        .unwrap();

    assert_eq!(result["readiness"]["ready"], json!(true));
    assert_eq!(result["readiness"]["hunkCount"], json!(1));
    assert_eq!(result["readiness"]["fileCount"], json!(1));
    assert_eq!(
        result["readiness"]["model"]["providerID"],
        json!("anthropic")
    );
}

// ---------------------------------------------------------------------------
// Errors for degenerate diffs
// ---------------------------------------------------------------------------

/// 空 diff 的生成请求被 400 拒绝，且不调用模型。
#[tokio::test]
async fn generation_refuses_an_empty_diff() {
    let harness = harness("gen-empty");
    harness.state.set_unstaged("");

    let error = generate(&harness.service).await.unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(error.code.as_deref(), Some("empty-diff"));
    assert_eq!(error.message, "There are no changes to review");
    assert_eq!(harness.state.calls(), 0);
}

/// 只有生成文件变更时，生成被 400 拒绝（only-generated）。
#[tokio::test]
async fn generation_refuses_an_only_generated_diff() {
    let harness = harness("gen-generated");
    harness
        .state
        .set_unstaged("diff --git a/pnpm-lock.yaml b/pnpm-lock.yaml\n--- a/pnpm-lock.yaml\n+++ b/pnpm-lock.yaml\n@@ -1,1 +1,2 @@\n+  a: 1\n");

    let error = generate(&harness.service).await.unwrap_err();
    assert_eq!(error.status, 400);
    assert_eq!(error.code.as_deref(), Some("only-generated"));
    assert_eq!(
        error.message,
        "Only generated files changed — there is nothing to review"
    );
}

/// 无可用模型时生成返回 404 no-model。
#[tokio::test]
async fn generation_refuses_without_a_model() {
    let harness = harness("gen-no-model");
    *harness.state.model.lock().unwrap() = None;

    let error = generate(&harness.service).await.unwrap_err();
    assert_eq!(error.status, 404);
    assert_eq!(error.code.as_deref(), Some("no-model"));
    assert_eq!(
        error.message,
        "No model is available — sign in to a provider first"
    );
}

// ---------------------------------------------------------------------------
// Invalid model JSON paths
// ---------------------------------------------------------------------------

/// 模型输出非法 JSON 映射为结构化失败；无 schema 路径下视为能力问题报 409。
#[tokio::test]
async fn invalid_model_output_maps_to_structured_failures() {
    let invalid = harness("invalid-json");
    *invalid.state.response_text.lock().unwrap() = "no json at all".to_string();
    let error = generate(&invalid.service).await.unwrap_err();
    assert_eq!(error.status, 502);
    assert_eq!(error.code.as_deref(), Some("invalid-walkthrough"));
    assert!(
        error
            .message
            .contains("did not return a usable walkthrough")
    );

    // Without schema support the same failure is a capability problem.
    let fallback = harness("invalid-json-fallback");
    fallback
        .state
        .fail_schema_with_400
        .store(true, Ordering::SeqCst);
    *fallback.state.response_text.lock().unwrap() = "no json at all".to_string();
    let error = generate(&fallback.service).await.unwrap_err();
    assert_eq!(error.status, 409);
    assert_eq!(error.code.as_deref(), Some("structured-output-unsupported"));
    assert!(
        error
            .message
            .contains("could not return the structured response")
    );
}

/// 模型调用的上下文/输出耗尽错误映射为 409 并携带字符数。
#[tokio::test]
async fn context_and_output_failures_from_the_model_call_map_to_409() {
    let harness = harness("ctx-fail");
    // Swap the generate seam by driving one call whose response signals the
    // failure — simpler: assert the mapping directly through the seam error.
    let model = default_model();
    let mapped = crate::walkthrough::small_model::as_request_failure(
        &model,
        &SmallModelError::context_too_small("Input is too large".to_string(), 20, 10),
    )
    .unwrap();
    assert_eq!(mapped.status, 409);
    assert_eq!(mapped.required_chars, Some(20));
    assert_eq!(mapped.available_chars, Some(10));

    let mapped = crate::walkthrough::small_model::as_request_failure(
        &model,
        &SmallModelError::output_exhausted("spent the budget on reasoning".to_string()),
    )
    .unwrap();
    assert_eq!(mapped.status, 409);
    assert_eq!(mapped.code.as_deref(), Some("output-exhausted"));
    let _ = harness;
}

// ---------------------------------------------------------------------------
// Language behavior (language.test.js)
// ---------------------------------------------------------------------------

/// 语言指令进入 system prompt，且语言随生成结果一起记录。
#[tokio::test]
async fn sends_the_instruction_and_records_the_language_with_the_result() {
    let harness = harness("lang-uk");

    let result = generate_in(&harness.service, "uk").await.unwrap();

    assert!(
        harness
            .state
            .last_call()
            .system
            .contains("Write all prose in Ukrainian")
    );
    assert_eq!(result["language"], json!("uk"));
}

/// 不同语言使用不同的缓存条目，互不顶替。
#[tokio::test]
async fn does_not_serve_one_language_from_the_other_language_cache_entry() {
    let harness = harness("lang-separate");

    generate_in(&harness.service, "uk").await.unwrap();
    let calls_after_uk = harness.state.calls();

    let english = generate_in(&harness.service, "en").await.unwrap();

    assert_eq!(english["fromCache"], json!(false));
    assert_eq!(harness.state.calls(), calls_after_uk + 1);
    assert_eq!(english["language"], json!("en"));
}

// Switching away and back must not cost a second generation: the earlier
// walkthrough is still addressed by its own key.
/// 切回之前的语言时直接命中其缓存条目，不再生成。
#[tokio::test]
async fn returns_the_earlier_language_from_cache_when_asked_for_again() {
    let harness = harness("lang-back");

    generate_in(&harness.service, "uk").await.unwrap();
    generate_in(&harness.service, "en").await.unwrap();
    let calls = harness.state.calls();

    let back = generate_in(&harness.service, "uk").await.unwrap();

    assert_eq!(back["fromCache"], json!(true));
    assert_eq!(back["language"], json!("uk"));
    assert_eq!(harness.state.calls(), calls);
}

// The pointer only knows what was generated here last. After a language
// switch that is the answer to a different question, and reading it instead
// left the panel showing English while the picker said Ukrainian.
/// 读取优先返回所请求语言对应的条目，而不是指针指向的最近一条。
#[tokio::test]
async fn reads_back_the_walkthrough_written_in_the_language_being_asked_for() {
    let harness = harness("lang-read");

    generate_in(&harness.service, "uk").await.unwrap();
    generate_in(&harness.service, "en").await.unwrap();
    let calls = harness.state.calls();

    let ukrainian = read_in(&harness.service, "uk").await;

    assert_eq!(ukrainian["language"], json!("uk"));
    assert_eq!(ukrainian["walkthrough"]["title"], json!("Change"));
    assert_eq!(harness.state.calls(), calls);
}

/// 反复切换语言读取始终命中缓存，零次新的模型调用。
#[tokio::test]
async fn switches_back_and_forth_without_generating_anything() {
    let harness = harness("lang-switch");

    generate_in(&harness.service, "uk").await.unwrap();
    generate_in(&harness.service, "ja").await.unwrap();
    let calls = harness.state.calls();

    assert_eq!(
        read_in(&harness.service, "uk").await["language"],
        json!("uk")
    );
    assert_eq!(
        read_in(&harness.service, "ja").await["language"],
        json!("ja")
    );
    assert_eq!(
        read_in(&harness.service, "uk").await["language"],
        json!("uk")
    );
    assert_eq!(harness.state.calls(), calls);
}

// Falling back is still right: an English review beats an empty panel, and
// the response says which language it is in so the panel can be honest.
/// 所请求语言无条目时回退到最近一次 walkthrough，并如实报告其语言。
#[tokio::test]
async fn falls_back_to_the_last_walkthrough_when_none_exists_in_that_language() {
    let harness = harness("lang-fallback");

    generate_in(&harness.service, "en").await.unwrap();
    let korean = read_in(&harness.service, "ko").await;

    assert_eq!(korean["walkthrough"]["title"], json!("Change"));
    assert_eq!(korean["language"], json!("en"));
}

/// 未知语言按英文处理而不是失败。
#[tokio::test]
async fn treats_an_unknown_language_as_english_rather_than_failing() {
    let harness = harness("lang-unknown");

    let result = generate_in(&harness.service, "kl").await.unwrap();

    assert_eq!(result["language"], json!("en"));
    assert!(!harness.state.last_call().system.contains("Write all prose"));
}

// ---------------------------------------------------------------------------
// Response shape details
// ---------------------------------------------------------------------------

/// 读取接口完整报告 hunks、陈旧度与覆盖；代码变更后旧 walkthrough 判 stale。
#[tokio::test]
async fn get_walkthrough_reports_hunks_staleness_and_coverage() {
    let harness = harness("shape");

    let before = harness
        .service
        .get_walkthrough("/repo", Some(&source_value()), None, None)
        .await
        .unwrap();
    assert_eq!(before["walkthrough"], Value::Null);
    assert_eq!(before["hunkCount"], json!(1));
    assert_eq!(before["generating"], json!(false));
    let hunks = before["hunks"].as_array().unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0]["path"], json!("src/a.ts"));
    assert_eq!(hunks[0]["status"], json!("modified"));
    assert_eq!(hunks[0]["scope"], json!("working"));
    assert_eq!(hunks[0]["newStart"], json!(1));
    assert_eq!(hunks[0]["added"], json!(1));
    assert!(
        hunks[0]["patch"]
            .as_str()
            .unwrap()
            .starts_with("diff --git")
    );

    let generated = generate(&harness.service).await.unwrap();
    assert_eq!(generated["fromCache"], json!(false));
    assert_eq!(generated["hunkCount"], json!(1));
    assert_eq!(generated["isStale"], json!(false));
    assert!(generated["uncoveredHunkIds"].as_array().unwrap().is_empty());
    assert_eq!(generated["model"]["providerID"], json!("anthropic"));

    // The hunk id changed with the edit, so the stored walkthrough is stale
    // and the edited hunk shows up uncovered.
    harness
        .state
        .set_unstaged(&PATCH.replace("const added = true;", "const added = false;"));
    let after = harness
        .service
        .get_walkthrough("/repo", Some(&source_value()), None, None)
        .await
        .unwrap();
    assert_eq!(after["isStale"], json!(true));
    assert_eq!(after["missingHunkIds"].as_array().unwrap().len(), 1);
    assert_eq!(
        after["staleStopIds"].as_array().unwrap(),
        &vec![json!("stop-1-1")]
    );
    assert_eq!(after["uncoveredHunkIds"].as_array().unwrap().len(), 1);
}

/// `utf16_len` 按 UTF-16 码元计数，与 JS `.length` 一致。
#[test]
fn utf16_len_counts_code_units_like_js_length() {
    assert_eq!(utf16_len("abc"), 3);
    // U+1F600 is two UTF-16 code units, one Rust char.
    assert_eq!(utf16_len("a\u{1F600}b"), 4);
}

/// 取消信号可独立于服务使用：初始未取消，cancel 后置位。
#[test]
fn cancel_signal_wakes_without_a_service() {
    let signal = CancelSignal::default();
    assert!(!signal.is_cancelled());
    signal.cancel();
    assert!(signal.is_cancelled());
}
