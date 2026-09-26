//! The walkthrough's seam onto the small-model service.
//!
//! JS precedent: `walkthrough/index.js` calls `describeSmallModel` and
//! `generateSmallModelText` from `../small-model/index.js`. That module is
//! being ported separately; until its port lands this seam reports
//! unavailability (`describe` → `Ok(None)`, the `no-model` path the JS
//! already handles) and generation is unreachable past model resolution.
//!
//! Wiring point: when `src/small_model/` lands, construct the real closures
//! here (or in the composition root) mapping onto the ported
//! describe/generate entry points; everything else in this module is the
//! contract those closures must satisfy.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::error::WalkthroughError;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Cooperative cancel signal for a running generation. Only an explicit
/// `POST /api/walkthrough/cancel` trips it — leaving the page does not
/// (a dropped connection and a deliberate cancel look identical at the
/// socket, and generation outlives its request).
#[derive(Clone, Default)]
pub struct CancelSignal {
    inner: Arc<CancelInner>,
}

#[derive(Default)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelSignal {
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves once cancelled.
    pub async fn cancelled(&self) {
        while !self.is_cancelled() {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    /// Whether two handles point at the same cancellation token.
    pub fn same(&self, other: &CancelSignal) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// `describeSmallModel`'s request shape. `output_reserve_tokens` is the
/// walkthrough's budget rule handed to the resolver (JS
/// `outputReserveTokens: walkthroughOutputTokens`) so the reserve subtracted
/// from the input allowance and the number later requested cannot drift apart.
#[derive(Clone)]
pub struct DescribeModelRequest {
    pub directory: String,
    /// `provider/model` outranking the setting and the small-model chain.
    pub override_model: Option<String>,
    pub output_reserve_tokens: Arc<dyn Fn(i64, Option<i64>) -> i64 + Send + Sync>,
}

/// `describeSmallModel`'s result (`resolved` + `hasLogin`, `inputCharBudget`,
/// `contextTokens`, `contextKnown`, `outputTokens`, `structuredOutput`,
/// `outputTokenLimit`).
#[derive(Debug, Clone)]
pub struct ModelDescription {
    pub provider_id: String,
    pub model_id: String,
    pub source: String,
    pub has_login: Option<bool>,
    pub input_char_budget: i64,
    pub context_tokens: Option<i64>,
    pub context_known: Option<bool>,
    pub output_tokens: Option<i64>,
    pub structured_output: Option<bool>,
    pub output_token_limit: Option<i64>,
}

impl ModelDescription {
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

    /// `modelLabel`.
    pub fn label(&self) -> String {
        self.key()
    }

    /// The full describe shape as JSON (camelCase, like the JS return).
    pub fn to_value(&self) -> Value {
        json!({
            "providerID": self.provider_id,
            "modelID": self.model_id,
            "source": self.source,
            "hasLogin": self.has_login,
            "inputCharBudget": self.input_char_budget,
            "contextTokens": self.context_tokens,
            "contextKnown": self.context_known,
            "outputTokens": self.output_tokens,
            "structuredOutput": self.structured_output,
            "outputTokenLimit": self.output_token_limit,
        })
    }

    /// The entry subset the walkthrough records with its results
    /// (`{ providerID, modelID, source }`).
    pub fn to_entry_value(&self) -> Value {
        json!({
            "providerID": self.provider_id,
            "modelID": self.model_id,
            "source": self.source,
        })
    }
}

/// Errors surfaced by the small-model chain that the walkthrough maps onto
/// structured HTTP failures. `status` is the raw provider/HTTP status (a 4xx
/// there means "this provider rejects the request shape"); `status_code` is
/// the caller-facing status the JS errors carry as `statusCode`.
#[derive(Debug, Clone, Default)]
pub struct SmallModelError {
    pub code: Option<String>,
    pub status: Option<u16>,
    pub status_code: Option<u16>,
    pub message: String,
    pub required_chars: Option<i64>,
    pub available_chars: Option<i64>,
    pub aborted: bool,
}

impl SmallModelError {
    pub fn context_too_small(message: String, required: i64, available: i64) -> Self {
        Self {
            code: Some("context-too-small".to_string()),
            message,
            required_chars: Some(required),
            available_chars: Some(available),
            ..Default::default()
        }
    }

    pub fn output_exhausted(message: String) -> Self {
        Self {
            code: Some("output-exhausted".to_string()),
            message,
            ..Default::default()
        }
    }

    pub fn no_provider_login(message: String) -> Self {
        Self {
            code: Some("no-provider-login".to_string()),
            status_code: Some(401),
            message,
            ..Default::default()
        }
    }
}

/// `generateSmallModelText`'s request shape.
pub struct GenerateTextRequest {
    pub prompt: String,
    pub system: String,
    pub directory: String,
    /// `provider/model`.
    pub model: String,
    pub response_schema: Option<Value>,
    /// The walkthrough always asks for `'error'` on overflow.
    pub on_overflow_error: bool,
    pub timeout_ms: u64,
    /// The number the input budget was already reduced by, not a fresh guess.
    pub max_output_tokens: i64,
    pub cancel: CancelSignal,
}

/// `generateSmallModelText`'s result (only `.text` is consumed here).
#[derive(Debug, Clone)]
pub struct GenerateTextOutput {
    pub text: String,
}

pub type DescribeFuture = BoxFuture<Result<Option<ModelDescription>, SmallModelError>>;
pub type GenerateFuture = BoxFuture<Result<GenerateTextOutput, SmallModelError>>;

pub type DescribeModelFn = Arc<dyn Fn(DescribeModelRequest) -> DescribeFuture + Send + Sync>;
pub type GenerateTextFn = Arc<dyn Fn(GenerateTextRequest) -> GenerateFuture + Send + Sync>;

/// The seam onto the small-model service.
#[derive(Clone)]
pub struct SmallModelSeam {
    pub describe: DescribeModelFn,
    pub generate: GenerateTextFn,
}

/// The not-yet-wired default: no model resolves, so reads report
/// `readiness: { ready: false, reason: 'no-model' }` and generation answers
/// the JS `no-model` 404. This mirrors a server whose small-model chain has
/// no authenticated provider rather than inventing one.
pub fn unavailable_small_model() -> SmallModelSeam {
    SmallModelSeam {
        describe: Arc::new(|_request| Box::pin(async { Ok(None) })),
        generate: Arc::new(|_request| {
            Box::pin(async {
                Err(SmallModelError {
                    status_code: Some(503),
                    message: "The small-model service is not available".to_string(),
                    ..Default::default()
                })
            })
        }),
    }
}

/// Map a seam error into the walkthrough's HTTP error, exactly like the JS
/// `asRequestFailure` (falling back to `None` for errors it does not own).
pub fn as_request_failure(
    model: &ModelDescription,
    error: &SmallModelError,
) -> Option<WalkthroughError> {
    if error.code.as_deref() == Some("context-too-small") {
        let mut failure = WalkthroughError::with_code(&error.message, 409, "context-too-small")
            .model(model.to_value());
        if let Some(required) = error.required_chars {
            failure = failure.required_chars(required);
        }
        if let Some(available) = error.available_chars {
            failure = failure.available_chars(available);
        }
        return Some(failure);
    }
    if error.code.as_deref() == Some("output-exhausted") {
        return Some(
            WalkthroughError::with_code(&error.message, 409, "output-exhausted")
                .model(model.to_value()),
        );
    }
    if error.code.as_deref() == Some("no-provider-login") {
        return Some(
            WalkthroughError::with_code(&error.message, 401, "no-provider-login")
                .model(model.to_value()),
        );
    }
    None
}

/// `refusesSchema(error)`: a structured-output refusal (the call layer throws
/// `code: 'structured-output-unsupported'`) or any 4xx from the provider — a
/// rejected request shape is not a dead end, the shape can travel in the
/// prompt instead.
pub fn refuses_schema(error: &SmallModelError) -> bool {
    error.code.as_deref() == Some("structured-output-unsupported")
        || error
            .status
            .is_some_and(|status| (400..500).contains(&status))
}

/// An aborted call (explicit cancel) maps to the generic JS failure the
/// AbortError produced: no status code, so the route answered 500 with the
/// abort message.
pub fn abort_error() -> SmallModelError {
    SmallModelError {
        message: "The operation was aborted".to_string(),
        aborted: true,
        ..Default::default()
    }
}

/// Shared test/inspection helper: guards used by the service to detect an
/// aborted call and translate it to the JS failure shape.
pub struct AbortGuard {
    cancel: CancelSignal,
    raised: Mutex<bool>,
}

impl AbortGuard {
    pub fn new(cancel: CancelSignal) -> Self {
        Self {
            cancel,
            raised: Mutex::new(false),
        }
    }

    pub fn tripped(&self) -> bool {
        *self.raised.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn check(&self) -> Result<(), SmallModelError> {
        if self.cancel.is_cancelled() {
            *self.raised.lock().unwrap_or_else(|e| e.into_inner()) = true;
            return Err(abort_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancel_signal_resolves_waiters() {
        let signal = CancelSignal::default();
        assert!(!signal.is_cancelled());

        let waiter = {
            let signal = signal.clone();
            tokio::spawn(async move {
                signal.cancelled().await;
                true
            })
        };
        // Give the waiter a chance to observe the not-yet-cancelled state.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        signal.cancel();
        assert!(signal.is_cancelled());
        assert!(waiter.await.unwrap());
    }

    #[test]
    fn same_detects_identity_of_the_underlying_token() {
        let a = CancelSignal::default();
        let b = a.clone();
        let c = CancelSignal::default();
        assert!(a.same(&b));
        assert!(!a.same(&c));
    }

    #[test]
    fn model_description_serializes_like_the_js_shape() {
        let model = ModelDescription {
            provider_id: "anthropic".to_string(),
            model_id: "claude-haiku-4-5".to_string(),
            source: "config".to_string(),
            has_login: Some(true),
            input_char_budget: 1_000_000,
            context_tokens: Some(200_000),
            context_known: Some(true),
            output_tokens: Some(50_000),
            structured_output: Some(true),
            output_token_limit: Some(64_000),
        };
        let value = model.to_value();
        assert_eq!(value["providerID"], json!("anthropic"));
        assert_eq!(value["inputCharBudget"], json!(1_000_000));
        assert_eq!(model.label(), "anthropic/claude-haiku-4-5");
        assert_eq!(model.to_entry_value()["source"], json!("config"));
    }

    #[test]
    fn refuses_schema_on_structured_output_refusal_or_4xx() {
        assert!(refuses_schema(&SmallModelError {
            code: Some("structured-output-unsupported".to_string()),
            ..Default::default()
        }));
        assert!(refuses_schema(&SmallModelError {
            status: Some(400),
            ..Default::default()
        }));
        assert!(!refuses_schema(&SmallModelError {
            status: Some(500),
            ..Default::default()
        }));
        assert!(!refuses_schema(&SmallModelError::default()));
    }

    #[test]
    fn maps_structured_request_failures_to_http_errors() {
        let model = ModelDescription {
            provider_id: "p".to_string(),
            model_id: "m".to_string(),
            source: "request".to_string(),
            has_login: None,
            input_char_budget: 10,
            context_tokens: None,
            context_known: None,
            output_tokens: None,
            structured_output: None,
            output_token_limit: None,
        };
        let mapped = as_request_failure(
            &model,
            &SmallModelError::context_too_small("too big".to_string(), 20, 10),
        )
        .unwrap();
        assert_eq!(mapped.status, 409);
        assert_eq!(mapped.code.as_deref(), Some("context-too-small"));
        assert_eq!(mapped.required_chars, Some(20));
        assert_eq!(mapped.available_chars, Some(10));

        let mapped = as_request_failure(
            &model,
            &SmallModelError::no_provider_login("no login".to_string()),
        )
        .unwrap();
        assert_eq!(mapped.status, 401);
        assert_eq!(mapped.code.as_deref(), Some("no-provider-login"));

        assert!(as_request_failure(&model, &SmallModelError::default()).is_none());
    }

    #[tokio::test]
    async fn unavailable_seam_describes_no_model() {
        let seam = unavailable_small_model();
        let request = DescribeModelRequest {
            directory: "/repo".to_string(),
            override_model: None,
            output_reserve_tokens: Arc::new(|_context, _limit| 24_000),
        };
        assert!((seam.describe)(request).await.unwrap().is_none());
    }
}
