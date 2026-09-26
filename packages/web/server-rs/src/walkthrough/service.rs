//! Port of `server/lib/walkthrough/index.js` — orchestration.
//!
//! Walkthrough generation is always user-initiated and never automatic: it
//! costs tokens, and a background regeneration on every keystroke would be a
//! way to spend a budget without anyone deciding to.
//!
//! Generation outlives the request that started it: a dropped connection and
//! a deliberate cancel look identical at the socket, so jobs run on their own
//! task and are keyed by repository + source — a client that comes back
//! attaches to the running job instead of starting a second one, and
//! cancelling is an explicit request rather than a side effect of leaving.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use serde_json::{Value, json};

use super::digest::{DigestBuild, build_digest};
use super::error::WalkthroughError;
use super::hunks::{HunkIndex, index_hunks};
use super::languages::normalize_language;
use super::model_settings::read_walkthrough_model_override;
use super::prompt::{JSON_SHAPE_INSTRUCTION, PromptInput, build_prompt};
use super::schema::{
    NormalizedWalkthrough, WALKTHROUGH_VERSION, normalize_walkthrough, parse_model_json,
    response_schema,
};
use super::small_model::{
    AbortGuard, CancelSignal, DescribeModelRequest, GenerateTextRequest, ModelDescription,
    SmallModelError, SmallModelSeam, as_request_failure, refuses_schema,
};
use super::sources::{GitDeps, PrDiffFn, Source, load_source_sections, parse_source, source_key};
use super::store::{CacheEntry, Pointer, Store, build_cache_key};

/// A hang guard, not a pace-setter. Losing a nearly-finished generation
/// wastes real money and minutes, while an over-long deadline only holds a
/// job slot, so this errs long. It scales because a three-hunk edit and a
/// 500-hunk pull request have no business sharing a deadline.
pub const GENERATION_TIMEOUT_BASE_MS: u64 = 120_000;
pub const GENERATION_TIMEOUT_PER_HUNK_MS: u64 = 1_000;
pub const GENERATION_TIMEOUT_MAX_MS: u64 = 900_000;

pub fn generation_timeout_ms(hunk_count: i64) -> u64 {
    GENERATION_TIMEOUT_MAX_MS
        .min(GENERATION_TIMEOUT_BASE_MS + hunk_count.max(0) as u64 * GENERATION_TIMEOUT_PER_HUNK_MS)
}

// A full walkthrough is a few thousand tokens of JSON. The budget exists for
// what comes before it: reasoning models spend the same allowance thinking
// and return nothing when it runs out, which is a bill for no answer.
//
// So the ask is derived from the model rather than fixed. A flat 24k was the
// same number for a 64k-context model and for one that admits to 384k output
// tokens, and on the latter it was the only reason generation failed.
//
// The reserve subtracted from the input budget is the same number, always:
// ask for more than was reserved and a large diff overruns the context
// mid-answer, which surfaces as a truncation bug rather than a budgeting one.
pub const MIN_OUTPUT_TOKENS: i64 = 24_000;
// A ceiling, because the reserve is taken out of the input allowance: a model
// that would let us ask for 384k tokens of answer would also let us spend a
// third of a million tokens of context reserving them, and no walkthrough
// needs that much thinking.
pub const MAX_OUTPUT_TOKENS: i64 = 96_000;
// Above this share of the context, the reserve starts costing more diff than
// the extra room is worth.
pub const OUTPUT_CONTEXT_SHARE: f64 = 0.25;

/// Answer allowance for a specific model: as much as it admits it can emit,
/// bounded by a share of its context and never below what this feature always
/// asked for.
pub fn walkthrough_output_tokens(context_tokens: i64, output_token_limit: Option<i64>) -> i64 {
    let context = context_tokens.max(0);
    let wanted = MAX_OUTPUT_TOKENS
        .min(MIN_OUTPUT_TOKENS.max((context as f64 * OUTPUT_CONTEXT_SHARE) as i64));
    // A model whose own limit is below the floor gets its limit: asking for
    // more than a provider allows is rejected outright by some and ignored by
    // others.
    match output_token_limit {
        Some(limit) if limit > 0 => wanted.min(limit),
        _ => wanted,
    }
}

/// The reserve rule handed to model resolution.
fn output_reserve_rule() -> Arc<dyn Fn(i64, Option<i64>) -> i64 + Send + Sync> {
    Arc::new(|context_tokens, output_token_limit| {
        walkthrough_output_tokens(context_tokens, output_token_limit)
    })
}

/// JS `.length` counts UTF-16 code units; readiness is measured in that unit.
pub fn utf16_len(text: &str) -> i64 {
    text.encode_utf16().count() as i64
}

/// Coarse stages, reported so a long wait is legible. Only phases a person
/// can actually wait on are named: building the digest and reading the cache
/// take single-digit milliseconds, so they get no rows. `retrying` appears
/// only when a provider rejects the schema and the prompt-side fallback runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Collecting,
    Asking,
    Retrying,
    Assembling,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Collecting => "collecting",
            Stage::Asking => "asking",
            Stage::Retrying => "retrying",
            Stage::Assembling => "assembling",
        }
    }
}

type JobOutcome = Arc<Result<Value, WalkthroughError>>;
type SharedJobFuture = futures::future::Shared<futures::future::BoxFuture<'static, JobOutcome>>;

struct Job {
    cancel: CancelSignal,
    stage: Mutex<Stage>,
    result: SharedJobFuture,
}

/// The walkthrough service: job registry, schema-refusal memory, and the
/// store, wired to the git/small-model/PR seams.
pub struct WalkthroughService {
    store: Store,
    data_dir: PathBuf,
    git: GitDeps,
    small_model: SmallModelSeam,
    pr_diff: Option<PrDiffFn>,
    /// JS module-level `jobs` map. Keyed by `repoRoot\0sourceKey`.
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    /// JS module-level `schemaRefusedBy` set: providers that answered a
    /// schema request with a 4xx. Process-lifetime only, on purpose — a
    /// provider that gains structured-output support should not need a
    /// settings change to be tried again, and a restart is enough.
    schema_refused_by: Mutex<HashSet<String>>,
}

impl WalkthroughService {
    pub fn new(
        data_dir: PathBuf,
        git: GitDeps,
        small_model: SmallModelSeam,
        pr_diff: Option<PrDiffFn>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store: Store::new(&data_dir),
            data_dir,
            git,
            small_model,
            pr_diff,
            jobs: Mutex::new(HashMap::new()),
            schema_refused_by: Mutex::new(HashSet::new()),
        })
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    fn job_key(repo_root: &str, source_key: &str) -> String {
        format!("{repo_root}\0{source_key}")
    }

    fn set_stage(&self, job_key: &str, stage: Stage) {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.get(job_key) {
            *job.stage.lock().unwrap_or_else(|e| e.into_inner()) = stage;
        }
    }

    /// `getGenerationStage`: current stage of a running generation, or `None`
    /// when nothing is running. Reads memory only — no git, no network — so
    /// it is cheap to poll.
    pub fn generation_stage(&self, repo_root: &str, source_key: &str) -> Option<&'static str> {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.get(&Self::job_key(repo_root, source_key))
            .map(|job| job.stage.lock().unwrap_or_else(|e| e.into_inner()).as_str())
    }

    /// `isGenerating`: whether a generation is currently running for a
    /// source. Lets a reconnecting client show progress instead of an empty
    /// panel.
    pub fn is_generating(&self, repo_root: &str, source_key: &str) -> bool {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.contains_key(&Self::job_key(repo_root, source_key))
    }

    /// `getRepositoryRootFor`: resolve the pair the job registry is keyed by,
    /// for callers that need to look a job up without doing any diff work.
    pub async fn repository_root_for(
        &self,
        directory: &str,
        raw_source: Option<&Value>,
    ) -> Result<(String, String), WalkthroughError> {
        let source = parse_source(raw_source)?;
        let repo_root = (self.git.repository_root)(directory.to_string())
            .await
            .map_err(WalkthroughError::internal)?;
        Ok((repo_root, source_key(&source)))
    }

    /// `cancelWalkthroughGeneration`: stop a running generation. Only an
    /// explicit request does this — leaving the page does not.
    pub async fn cancel_generation(
        &self,
        directory: &str,
        raw_source: Option<&Value>,
    ) -> Result<Value, WalkthroughError> {
        let source = parse_source(raw_source)?;
        let repo_root = (self.git.repository_root)(directory.to_string())
            .await
            .map_err(WalkthroughError::internal)?;
        let key = Self::job_key(&repo_root, &source_key(&source));
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        match jobs.get(&key) {
            Some(job) => {
                job.cancel.cancel();
                Ok(json!({ "cancelled": true }))
            }
            None => Ok(json!({ "cancelled": false })),
        }
    }

    /// `resolveModel`: an explicit per-review choice outranks the saved
    /// setting, which in turn outranks the small-model chain — the user
    /// picking a roomier model for a risky change is the most specific intent
    /// there is.
    async fn resolve_model(
        &self,
        directory: &str,
        explicit_model: Option<&str>,
    ) -> Result<Option<ModelDescription>, SmallModelError> {
        let override_model = explicit_model
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| read_walkthrough_model_override(&self.data_dir));
        let request = DescribeModelRequest {
            directory: directory.to_string(),
            override_model,
            output_reserve_tokens: output_reserve_rule(),
        };
        (self.small_model.describe)(request).await
    }

    /// `loadCurrentDiff`: current diff for a source, parsed into files and
    /// hunks.
    async fn load_current_diff(
        &self,
        directory: &str,
        source: &Source,
    ) -> Result<DigestBuild, WalkthroughError> {
        let (sections, _meta) =
            load_source_sections(directory, source, &self.git, self.pr_diff.as_ref()).await?;
        Ok(build_digest(&sections))
    }

    /// `getWalkthrough`: read the last walkthrough for a source, resolved
    /// against the current diff. Never generates and never spends tokens.
    pub async fn get_walkthrough(
        &self,
        directory: &str,
        raw_source: Option<&Value>,
        explicit_model: Option<&str>,
        raw_language: Option<&str>,
    ) -> Result<Value, WalkthroughError> {
        let source = parse_source(raw_source)?;
        let repo_root = (self.git.repository_root)(directory.to_string())
            .await
            .map_err(WalkthroughError::internal)?;
        let key = source_key(&source);
        let language = normalize_language(raw_language);

        let pointer = self.store.read_pointer(&repo_root, &key);
        // One diff, one model lookup, both answers. These used to be separate
        // endpoints the client called in parallel, which meant every panel
        // open ran the whole git pipeline twice.
        let built = self.load_current_diff(directory, &source).await?;
        let model = self
            .resolve_model(directory, explicit_model)
            .await
            .unwrap_or(None);
        let files = &built.files;
        let hunk_index = index_hunks(files);
        let readiness = self.compute_readiness(&built, model.as_ref(), &source, &language);

        let mut base = json!({
            "source": source.to_value(),
            "hunks": serialize_hunks(files),
            "hunkCount": hunk_index.len(),
            "readiness": readiness,
            "generating": self.is_generating(&repo_root, &key),
        });

        // Ask the cache for *this* request before falling back to the
        // pointer. The pointer only knows which walkthrough was generated
        // here last, which after a model or language switch is the answer to
        // a different question: the panel would keep showing the English
        // review while the picker said Ukrainian, even though the Ukrainian
        // one was sitting in the cache. The key is computed from the diff
        // this read already parsed, so this costs a file read and no git work
        // at all.
        let requested_key = model.as_ref().map(|model| {
            build_cache_key(
                &repo_root,
                &key,
                &model.provider_id,
                &model.model_id,
                &language,
                files,
            )
        });

        let requested = requested_key
            .as_ref()
            .and_then(|key| self.store.read_cached_walkthrough(key));
        let fallback = pointer
            .as_ref()
            .and_then(|pointer| self.store.read_cached_walkthrough(&pointer.cache_key));
        let entry = requested.as_ref().or(fallback.as_ref());
        let entry = match entry {
            Some(entry) => entry.clone(),
            None => {
                // No pointer, or the pointer outlived its entry (eviction,
                // manual cleanup). "No walkthrough" is the truthful answer
                // either way; the pointer is left for the next generation to
                // overwrite.
                base["walkthrough"] = Value::Null;
                return Ok(base);
            }
        };
        // Showing it makes it the last walkthrough shown here, and a
        // regeneration re-authors from whatever the reader is actually looking
        // at. Only written when it moved, so an unchanged read stays a pure
        // read.
        if let (Some(requested), Some(requested_key)) = (&requested, &requested_key) {
            let pointer_matches = pointer
                .as_ref()
                .is_some_and(|pointer| pointer.cache_key == *requested_key);
            if !pointer_matches {
                self.store.write_pointer(
                    &repo_root,
                    &key,
                    &Pointer {
                        repo_root: Some(repo_root.clone()),
                        source_key: Some(key.clone()),
                        cache_key: requested_key.clone(),
                        generated_at: Some(requested.generated_at.clone()),
                    },
                );
            }
        }

        let staleness = resolve_against_current(&entry.walkthrough, &hunk_index);
        base["walkthrough"] = entry.walkthrough.clone();
        base["model"] = entry.model.clone().unwrap_or(Value::Null);
        // The language the text on screen is actually written in, which is
        // not necessarily the one being asked for now. The picker needs the
        // difference: it is what lets it default to what produced this rather
        // than to a setting.
        base["language"] = entry
            .language
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null);
        base["generatedAt"] = Value::String(entry.generated_at.clone());
        base["isStale"] = Value::Bool(staleness.is_stale);
        base["missingHunkIds"] = json!(staleness.missing_hunk_ids);
        base["staleStopIds"] = json!(staleness.stale_stop_ids);
        base["uncoveredHunkIds"] = json!(staleness.uncovered_hunk_ids);
        Ok(base)
    }

    /// `computeReadiness`: whether the resolved model can do this job,
    /// computed from a digest the caller already built.
    fn compute_readiness(
        &self,
        built: &DigestBuild,
        model: Option<&ModelDescription>,
        source: &Source,
        language: &str,
    ) -> Value {
        let Some(model) = model else {
            return json!({ "ready": false, "reason": "no-model" });
        };

        if built.hunk_count == 0 {
            // "Only a lockfile changed" is a different answer from "nothing
            // changed", and the user can act on it (commit and move on)
            // rather than wonder why the review refuses.
            let reason =
                if !built.files.is_empty() && built.generated_file_count == built.files.len() {
                    "only-generated"
                } else {
                    "empty-diff"
                };
            return json!({
                "ready": false,
                "reason": reason,
                "model": model.to_value(),
                "generatedFileCount": built.generated_file_count,
            });
        }

        // A resolved override/config model can still have no usable login.
        // Refuse up front and omit the model — offering an unauthenticated
        // selection in the picker is what made the old raw auth error feel
        // like a product bug.
        if model.has_login == Some(false) {
            return json!({ "ready": false, "reason": "no-provider-login" });
        }

        // Built with the same language the generation would use: the
        // instruction is part of the prompt, so a readiness answer computed
        // without it would be measuring a request nobody is going to send.
        let (prompt, system) = build_prompt(PromptInput {
            digest_json: &built.digest_json,
            file_count: built.file_count,
            hunk_count: built.hunk_count,
            source: &source.to_value(),
            previous_walkthrough: None,
            language: Some(language),
        });
        let required_chars = utf16_len(&prompt) + utf16_len(&system);

        if model.structured_output == Some(false) {
            return json!({
                "ready": false,
                "reason": "structured-output-unsupported",
                "model": model.to_value(),
                "requiredChars": required_chars,
            });
        }

        if required_chars > model.input_char_budget {
            return json!({
                "ready": false,
                "reason": "context-too-small",
                "model": model.to_value(),
                "requiredChars": required_chars,
                "availableChars": model.input_char_budget,
            });
        }

        json!({
            "ready": true,
            "model": model.to_value(),
            "requiredChars": required_chars,
            "availableChars": model.input_char_budget,
            "hunkCount": built.hunk_count,
            "fileCount": built.file_count,
        })
    }

    /// `generateWalkthrough`: generate a walkthrough for a source. Returns
    /// the cached entry when the diff, model, and prompt are all unchanged —
    /// which also means returning to a previous state of the working tree
    /// costs nothing. A concurrent call for the same source attaches to the
    /// running job.
    pub async fn generate_walkthrough(
        self: &Arc<Self>,
        directory: &str,
        raw_source: Option<&Value>,
        force: bool,
        explicit_model: Option<&str>,
        raw_language: Option<&str>,
    ) -> Result<Value, WalkthroughError> {
        let source = parse_source(raw_source)?;
        let repo_root = (self.git.repository_root)(directory.to_string())
            .await
            .map_err(WalkthroughError::internal)?;
        let key = source_key(&source);
        let language = normalize_language(raw_language);

        let job_key = Self::job_key(&repo_root, &key);
        // Attach to a running job rather than starting a second one. A user
        // who refreshed and pressed the button again wants the answer, not
        // two bills. The registry lock is released before awaiting so the
        // handler future stays Send.
        let running = {
            let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            jobs.get(&job_key).map(|job| job.result.clone())
        };
        if let Some(shared) = running {
            let outcome = shared.await;
            return match &*outcome {
                Ok(value) => Ok(value.clone()),
                Err(error) => Err(error.clone()),
            };
        }

        // Spawned (not polled inline) so the generation survives a dropped
        // request — the JS promise kept running on the event loop for the
        // same reason.
        let explicit_model = explicit_model.map(str::to_string);
        let cancel = CancelSignal::default();
        let task_cancel = cancel.clone();
        let service = Arc::clone(self);
        let directory = directory.to_string();
        let key_for_cleanup = job_key.clone();
        let handle = tokio::spawn(async move {
            let outcome = service
                .run_generation(
                    &directory,
                    &source,
                    &repo_root,
                    &key,
                    force,
                    explicit_model.clone(),
                    language,
                    task_cancel.clone(),
                )
                .await;
            // `.finally(...)`: drop the job registration when this task was
            // the one that created it.
            {
                let mut jobs = service.jobs.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(current) = jobs.get(&key_for_cleanup)
                    && current.cancel.same(&task_cancel)
                {
                    jobs.remove(&key_for_cleanup);
                }
            }
            Arc::new(outcome)
        });

        let shared: SharedJobFuture = handle
            .map(|join| match join {
                Ok(outcome) => outcome,
                Err(error) => Arc::new(Err(WalkthroughError::internal(format!(
                    "generation task failed: {error}"
                )))),
            })
            .boxed()
            .shared();

        {
            let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
            // A rival may have registered between the check and here; the
            // later registration wins and the earlier task still completes
            // and writes its entry, exactly like the JS race.
            jobs.insert(
                job_key,
                Arc::new(Job {
                    cancel,
                    stage: Mutex::new(Stage::Collecting),
                    result: shared.clone(),
                }),
            );
        }

        let outcome = shared.await;
        match &*outcome {
            Ok(value) => Ok(value.clone()),
            Err(error) => Err(error.clone()),
        }
    }
    #[allow(clippy::too_many_arguments)]
    async fn run_generation(
        &self,
        directory: &str,
        source: &Source,
        repo_root: &str,
        key: &str,
        force: bool,
        explicit_model: Option<String>,
        language: String,
        cancel: CancelSignal,
    ) -> Result<Value, WalkthroughError> {
        let job_key = Self::job_key(repo_root, key);
        let model = match self
            .resolve_model(directory, explicit_model.as_deref())
            .await
        {
            Ok(Some(model)) => model,
            Ok(None) => {
                return Err(WalkthroughError::with_code(
                    "No model is available — sign in to a provider first",
                    404,
                    "no-model",
                ));
            }
            Err(error) => return Err(WalkthroughError::internal(error.message)),
        };
        if model.has_login == Some(false) {
            return Err(WalkthroughError::with_code(
                format!(
                    "No OpenCode login found for provider \"{}\" — sign in or choose a different model",
                    model.provider_id
                ),
                401,
                "no-provider-login",
            )
            .model(model.to_value()));
        }

        let built = self.load_current_diff(directory, source).await?;
        let abort_guard = AbortGuard::new(cancel.clone());
        self.set_stage(&job_key, Stage::Asking);
        if let Err(error) = abort_guard.check() {
            return Err(WalkthroughError::internal(error.message));
        }
        if built.hunk_count == 0 {
            if !built.files.is_empty() && built.generated_file_count == built.files.len() {
                return Err(WalkthroughError::with_code(
                    "Only generated files changed — there is nothing to review",
                    400,
                    "only-generated",
                ));
            }
            return Err(WalkthroughError::with_code(
                "There are no changes to review",
                400,
                "empty-diff",
            ));
        }

        let cache_key = build_cache_key(
            repo_root,
            key,
            &model.provider_id,
            &model.model_id,
            &language,
            &built.files,
        );

        let hunk_index = index_hunks(&built.files);

        if !force && let Some(cached) = self.store.read_cached_walkthrough(&cache_key) {
            self.store.write_pointer(
                repo_root,
                key,
                &Pointer {
                    repo_root: Some(repo_root.to_string()),
                    source_key: Some(key.to_string()),
                    cache_key: cache_key.clone(),
                    generated_at: Some(cached.generated_at.clone()),
                },
            );
            let staleness = resolve_against_current(&cached.walkthrough, &hunk_index);
            return Ok(json!({
                "source": source.to_value(),
                "walkthrough": cached.walkthrough,
                "model": cached.model.clone().unwrap_or(Value::Null),
                "language": cached.language.clone().map(Value::String).unwrap_or(Value::Null),
                "generatedAt": cached.generated_at,
                "fromCache": true,
                "hunks": serialize_hunks(&built.files),
                "hunkCount": built.hunk_count,
                "isStale": staleness.is_stale,
                "missingHunkIds": staleness.missing_hunk_ids,
                "staleStopIds": staleness.stale_stop_ids,
                "uncoveredHunkIds": staleness.uncovered_hunk_ids,
            }));
        }

        // A forced regeneration hands the model its own previous narrative so
        // it can keep what is still true instead of starting from a blank
        // page. The old anchors are deliberately not included — they belong
        // to code that has moved.
        let mut previous_walkthrough: Option<Value> = None;
        if let Some(pointer) = self.store.read_pointer(repo_root, key)
            && let Some(previous_entry) = self.store.read_cached_walkthrough(&pointer.cache_key)
            && previous_entry.cache_key.as_deref() != Some(cache_key.as_str())
        {
            previous_walkthrough = Some(previous_entry.walkthrough);
        }

        let (prompt, system) = build_prompt(PromptInput {
            digest_json: &built.digest_json,
            file_count: built.file_count,
            hunk_count: built.hunk_count,
            source: &source.to_value(),
            previous_walkthrough: previous_walkthrough.as_ref(),
            language: Some(&language),
        });

        if model.structured_output == Some(false) {
            return Err(WalkthroughError::with_code(
                format!(
                    "{} cannot produce structured output — choose a different small model",
                    model.label()
                ),
                409,
                "structured-output-unsupported",
            )
            .model(model.to_value()));
        }

        let timeout_ms = generation_timeout_ms(built.hunk_count as i64);
        // The number the input budget was already reduced by, not a fresh
        // guess.
        let max_output_tokens = model.output_tokens.unwrap_or(MIN_OUTPUT_TOKENS);

        let run = |request_prompt: String, request_system: String, schema: Option<Value>| {
            let seam = self.small_model.clone();
            let directory = directory.to_string();
            let model_label = format!("{}/{}", model.provider_id, model.model_id);
            let cancel = cancel.clone();
            async move {
                let request = GenerateTextRequest {
                    prompt: request_prompt,
                    system: request_system,
                    directory,
                    model: model_label,
                    response_schema: schema,
                    // The walkthrough always refuses truncation rather than
                    // reviewing a silently clipped diff.
                    on_overflow_error: true,
                    timeout_ms,
                    max_output_tokens,
                    cancel,
                };
                (seam.generate)(request).await
            }
        };

        // Roughly half the catalog does not declare `structured_output`, and
        // some of those providers reject the schema outright. A rejected
        // request shape is not a dead end: the shape can travel in the prompt
        // instead, and the response parser is already tolerant of imperfect
        // JSON.
        let with_schema = || run(prompt.clone(), system.clone(), Some(response_schema()));
        let without_schema = || {
            run(
                prompt.clone(),
                format!("{system}\n{JSON_SHAPE_INSTRUCTION}"),
                None,
            )
        };

        let map_failure = |error: &SmallModelError| -> Option<WalkthroughError> {
            as_request_failure(&model, error)
        };

        let already_refused = self
            .schema_refused_by
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&model.key());

        let raw_text = if already_refused {
            // Already known to refuse: skip straight to the fallback rather
            // than pay for a call whose failure is a foregone conclusion.
            self.set_stage(&job_key, Stage::Retrying);
            match without_schema().await {
                Ok(output) => (output.text, false),
                Err(error) => {
                    let failure = map_failure(&error);
                    return Err(
                        failure.unwrap_or_else(|| WalkthroughError::internal(error.message))
                    );
                }
            }
        } else {
            match with_schema().await {
                Ok(output) => (output.text, true),
                Err(error) => {
                    if let Some(failure) = map_failure(&error) {
                        return Err(failure);
                    }
                    if !refuses_schema(&error) {
                        return Err(WalkthroughError::internal(error.message));
                    }

                    self.schema_refused_by
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(model.key());
                    self.set_stage(&job_key, Stage::Retrying);
                    match without_schema().await {
                        Ok(output) => (output.text, false),
                        Err(fallback_error) => {
                            let failure = map_failure(&fallback_error);
                            return Err(failure.unwrap_or_else(|| {
                                WalkthroughError::internal(fallback_error.message)
                            }));
                        }
                    }
                }
            }
        };
        let (raw, used_schema) = raw_text;

        self.set_stage(&job_key, Stage::Assembling);
        if let Err(error) = abort_guard.check() {
            return Err(WalkthroughError::internal(error.message));
        }

        let walkthrough: NormalizedWalkthrough = match parse_model_json(&raw)
            .map_err(|message| message.to_string())
            .and_then(|parsed| {
                normalize_walkthrough(&parsed, &built.id_by_alias).map_err(|m| m.to_string())
            }) {
            Ok(walkthrough) => walkthrough,
            Err(_message) => {
                // Without schema support the model was asked for JSON in
                // prose and did not deliver: that is a capability problem the
                // user can fix by switching model, so it gets the picker
                // rather than a parser error.
                if !used_schema {
                    return Err(WalkthroughError::with_code(
                        format!(
                            "{} could not return the structured response a walkthrough needs",
                            model.label()
                        ),
                        409,
                        "structured-output-unsupported",
                    )
                    .model(model.to_value()));
                }
                return Err(WalkthroughError::with_code(
                    format!(
                        "{} did not return a usable walkthrough — try a different small model",
                        model.label()
                    ),
                    502,
                    "invalid-walkthrough",
                )
                .model(model.to_value()));
            }
        };

        let generated_at = crate::git_service::service::iso_now();
        let entry = CacheEntry {
            walkthrough_version: WALKTHROUGH_VERSION,
            cache_key: Some(cache_key.clone()),
            generated_at: generated_at.clone(),
            repo_root: Some(repo_root.to_string()),
            source_key: Some(key.to_string()),
            model: Some(model.to_entry_value()),
            language: Some(language.clone()),
            walkthrough: walkthrough.to_value(),
        };

        // A failed write costs a regeneration next time; it must never fail
        // the request that already produced a good walkthrough.
        self.store.write_cached_walkthrough(&cache_key, &entry);
        self.store.write_pointer(
            repo_root,
            key,
            &Pointer {
                repo_root: Some(repo_root.to_string()),
                source_key: Some(key.to_string()),
                cache_key: cache_key.clone(),
                generated_at: Some(generated_at.clone()),
            },
        );

        let staleness = resolve_against_current(&entry.walkthrough, &hunk_index);
        Ok(json!({
            "source": source.to_value(),
            "walkthrough": entry.walkthrough,
            "model": entry.model.clone().unwrap_or(Value::Null),
            "language": Value::String(language),
            "generatedAt": generated_at,
            "fromCache": false,
            "hunks": serialize_hunks(&built.files),
            "hunkCount": built.hunk_count,
            "isStale": staleness.is_stale,
            "missingHunkIds": staleness.missing_hunk_ids,
            "staleStopIds": staleness.stale_stop_ids,
            "uncoveredHunkIds": staleness.uncovered_hunk_ids,
        }))
    }
}

/// `serializeHunks`: the client's hunk index (id → patch) alongside the
/// walkthrough, so the client never recomputes ids.
fn serialize_hunks(files: &[super::digest::DigestFile]) -> Value {
    let hunks: Vec<Value> = files
        .iter()
        .flat_map(|file| {
            file.hunks.iter().map(|hunk| {
                json!({
                    "id": hunk.id,
                    "path": file.path,
                    "oldPath": file.old_path,
                    "status": file.status,
                    "scope": file.scope,
                    "header": hunk.header,
                    "newStart": hunk.new_start,
                    "added": hunk.added,
                    "deleted": hunk.deleted,
                    "patch": hunk.patch,
                })
            })
        })
        .collect();
    Value::Array(hunks)
}

/// `resolveAgainstCurrent`: compare a stored walkthrough against the diff as
/// it is right now.
///
/// Staleness is not a heuristic here: a hunk id is a hash of the hunk's
/// content, so an anchor that no longer resolves is proof that the code it
/// described has changed or gone. Anchors that still resolve are still
/// accurate.
fn resolve_against_current(walkthrough: &Value, hunk_index: &HunkIndex) -> Staleness {
    let mut missing_hunk_ids: Vec<String> = Vec::new();
    let mut stale_stop_ids: Vec<String> = Vec::new();

    let chapters = walkthrough
        .get("chapters")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for chapter in &chapters {
        let stops = chapter
            .get("stops")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for stop in &stops {
            let ids = stop
                .get("hunkIds")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let missing: Vec<String> = ids
                .iter()
                .filter_map(Value::as_str)
                .filter(|id| !hunk_index.has(id))
                .map(str::to_string)
                .collect();
            if missing.is_empty() {
                continue;
            }
            missing_hunk_ids.extend(missing);
            if let Some(stop_id) = stop.get("id").and_then(Value::as_str) {
                stale_stop_ids.push(stop_id.to_string());
            }
        }
    }

    // Whatever the model did not anchor is computed as the uncovered tail, so
    // the reader can always answer "have I seen everything that changed". No
    // hunk disappears from the view.
    let covered: HashSet<String> = chapters
        .iter()
        .flat_map(|chapter| {
            chapter
                .get("stops")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .flat_map(|stop| {
            stop.get("hunkIds")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|id| id.as_str().map(str::to_string))
        .collect();
    let uncovered_hunk_ids: Vec<String> = hunk_index
        .ids()
        .filter(|id| !covered.contains(*id))
        .map(str::to_string)
        .collect();

    Staleness {
        is_stale: !missing_hunk_ids.is_empty(),
        missing_hunk_ids,
        stale_stop_ids,
        uncovered_hunk_ids,
    }
}

struct Staleness {
    is_stale: bool,
    missing_hunk_ids: Vec<String>,
    stale_stop_ids: Vec<String>,
    uncovered_hunk_ids: Vec<String>,
}

/// `pruneMissingRepositories` housekeeping, deferred off the request path —
/// the JS module scheduled it on first import; here the router spawn defers
/// it behind startup.
pub fn spawn_housekeeping(store: Arc<Store>) {
    tokio::spawn(async move {
        // Housekeeping failing is not worth surfacing or retrying.
        let _ = store.prune_missing_repositories().await;
    });
}
