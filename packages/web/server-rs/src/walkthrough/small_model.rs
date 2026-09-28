//! walkthrough 与 small-model 服务之间的 seam：describe（模型解析）与
//! generate（文本生成）两个可注入闭包，加上取消信号、错误映射等纯本地
//! 机制。small-model 模块的移植落地之前，默认 seam 一律报告“无模型”。
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

/// Send 的装箱 future（seam 闭包的统一返回类型）。
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// 协作式取消信号：只有显式的 POST /api/walkthrough/cancel 会触发；
/// 离开页面不算——断连与主动取消在 socket 层无法区分，且生成任务
/// 比它的请求活得久。
/// Cooperative cancel signal for a running generation. Only an explicit
/// `POST /api/walkthrough/cancel` trips it — leaving the page does not
/// (a dropped connection and a deliberate cancel look identical at the
/// socket, and generation outlives its request).
#[derive(Clone, Default)]
pub struct CancelSignal {
/// 共享的取消状态与唤醒通知。
    inner: Arc<CancelInner>,
}

/// 取消状态的共享本体。
#[derive(Default)]
struct CancelInner {
/// 已取消标志。
    cancelled: AtomicBool,
/// 唤醒等待者的通知。
    notify: tokio::sync::Notify,
}

/// 取消的触发与观察。
impl CancelSignal {
/// 触发取消并唤醒所有等待者。
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

/// 是否已被取消。
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

/// 异步等待直到被取消。
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

/// 两个句柄是否指向同一个取消令牌。
    /// Whether two handles point at the same cancellation token.
    pub fn same(&self, other: &CancelSignal) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// describe 请求：仓库目录、可选的 provider/model 覆盖与输出预留规则。
/// `describeSmallModel`'s request shape. `output_reserve_tokens` is the
/// walkthrough's budget rule handed to the resolver (JS
/// `outputReserveTokens: walkthroughOutputTokens`) so the reserve subtracted
/// from the input allowance and the number later requested cannot drift apart.
#[derive(Clone)]
pub struct DescribeModelRequest {
/// 仓库目录。
    pub directory: String,
    /// `provider/model` outranking the setting and the small-model chain.
/// 显式指定的 provider/model（优先于设置与 small-model 链）。
    pub override_model: Option<String>,
/// 由上下文与输出上限计算输出 token 预留的规则（闭包）。
    pub output_reserve_tokens: Arc<dyn Fn(i64, Option<i64>) -> i64 + Send + Sync>,
}

/// describe 的结果：解析出的模型、字符预算与各项能力。
/// `describeSmallModel`'s result (`resolved` + `hasLogin`, `inputCharBudget`,
/// `contextTokens`, `contextKnown`, `outputTokens`, `structuredOutput`,
/// `outputTokenLimit`).
#[derive(Debug, Clone)]
pub struct ModelDescription {
/// provider 标识。
    pub provider_id: String,
/// 模型标识。
    pub model_id: String,
/// 解析来源（override/config 等）。
    pub source: String,
/// 该 provider 是否有登录态（未知则 None）。
    pub has_login: Option<bool>,
/// 输入字符预算。
    pub input_char_budget: i64,
/// 上下文窗口 token 数（未知则 None）。
    pub context_tokens: Option<i64>,
/// 上下文窗口是否已知。
    pub context_known: Option<bool>,
/// 请求的输出 token 数。
    pub output_tokens: Option<i64>,
/// 是否支持 structured output。
    pub structured_output: Option<bool>,
/// 输出 token 上限。
    pub output_token_limit: Option<i64>,
}

/// 标识与 JSON 形态辅助。
impl ModelDescription {
/// provider/model 复合键。
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider_id, self.model_id)
    }

/// 展示标签（即复合键）。
    /// `modelLabel`.
    pub fn label(&self) -> String {
        self.key()
    }

/// 完整的 describe 形态（camelCase，对齐 JS 返回）。
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

/// 随结果记录的条目子集（providerID/modelID/source）。
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

/// small-model 链路错误：可映射为结构化 HTTP 失败。status 是 provider
/// 的原始状态码，status_code 是面向调用方的状态码（JS 的 statusCode）。
/// Errors surfaced by the small-model chain that the walkthrough maps onto
/// structured HTTP failures. `status` is the raw provider/HTTP status (a 4xx
/// there means "this provider rejects the request shape"); `status_code` is
/// the caller-facing status the JS errors carry as `statusCode`.
#[derive(Debug, Clone, Default)]
pub struct SmallModelError {
/// 机器可读错误码。
    pub code: Option<String>,
/// provider/HTTP 原始状态码。
    pub status: Option<u16>,
/// 面向调用方的状态码。
    pub status_code: Option<u16>,
/// 错误消息。
    pub message: String,
/// context-too-small 时 digest 所需字符数。
    pub required_chars: Option<i64>,
/// context-too-small 时模型可用字符数。
    pub available_chars: Option<i64>,
/// 是否为显式取消导致的中止。
    pub aborted: bool,
}

/// 三种结构化错误构造器。
impl SmallModelError {
/// 上下文不足错误（携带所需/可用字符数）。
    pub fn context_too_small(message: String, required: i64, available: i64) -> Self {
        Self {
            code: Some("context-too-small".to_string()),
            message,
            required_chars: Some(required),
            available_chars: Some(available),
            ..Default::default()
        }
    }

/// 输出 token 耗尽错误。
    pub fn output_exhausted(message: String) -> Self {
        Self {
            code: Some("output-exhausted".to_string()),
            message,
            ..Default::default()
        }
    }

/// provider 未登录错误（面向调用方 401）。
    pub fn no_provider_login(message: String) -> Self {
        Self {
            code: Some("no-provider-login".to_string()),
            status_code: Some(401),
            message,
            ..Default::default()
        }
    }
}

/// generate 请求：prompt、模型、schema、超时与取消信号。
/// `generateSmallModelText`'s request shape.
pub struct GenerateTextRequest {
/// user prompt。
    pub prompt: String,
/// system prompt。
    pub system: String,
/// 仓库目录。
    pub directory: String,
    /// `provider/model`.
/// provider/model 复合键。
    pub model: String,
/// structured output 的 JSON schema（可选）。
    pub response_schema: Option<Value>,
    /// The walkthrough always asks for `'error'` on overflow.
/// 溢出时报错而非截断（walkthrough 恒为 true）。
    pub on_overflow_error: bool,
/// 调用超时毫秒数。
    pub timeout_ms: u64,
    /// The number the input budget was already reduced by, not a fresh guess.
/// 输出 token 上限（输入预算已按同一数字扣减，不是新估值）。
    pub max_output_tokens: i64,
/// 协作取消信号。
    pub cancel: CancelSignal,
}

/// generate 的结果（此处只消费 text）。
/// `generateSmallModelText`'s result (only `.text` is consumed here).
#[derive(Debug, Clone)]
pub struct GenerateTextOutput {
/// 模型返回的原始文本。
    pub text: String,
}

/// describe 闭包的 future 类型（Ok(None) 表示无模型）。
pub type DescribeFuture = BoxFuture<Result<Option<ModelDescription>, SmallModelError>>;
/// generate 闭包的 future 类型。
pub type GenerateFuture = BoxFuture<Result<GenerateTextOutput, SmallModelError>>;

/// describe seam 闭包类型。
pub type DescribeModelFn = Arc<dyn Fn(DescribeModelRequest) -> DescribeFuture + Send + Sync>;
/// generate seam 闭包类型。
pub type GenerateTextFn = Arc<dyn Fn(GenerateTextRequest) -> GenerateFuture + Send + Sync>;

/// small-model 服务 seam：describe 与 generate 两个闭包。
/// The seam onto the small-model service.
#[derive(Clone)]
pub struct SmallModelSeam {
/// 模型解析闭包（None 表示无可用模型）。
    pub describe: DescribeModelFn,
/// 文本生成闭包。
    pub generate: GenerateTextFn,
}

/// 尚未接线时的默认 seam：describe 恒返回 None（走 JS 已有的 no-model
/// 路径），generate 恒 503——模拟“small-model 链没有已登录 provider”。
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

/// 把 seam 错误映射为结构化 HTTP 错误：context-too-small → 409（带
/// requiredChars/availableChars）、output-exhausted → 409、
/// no-provider-login → 401；不属于这三类的返回 None 交由调用方处理。
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

/// 是否应改走“输出形状写进 prompt”的回退路径：structured-output 被拒
/// 或 provider 返回任何 4xx（拒绝的是请求形状，不是死路）。
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

/// 显式取消对应的错误形状（aborted=true、无状态码，对齐 JS 的
/// AbortError 路径：路由以 500 返回中止消息）。
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

/// 服务用来检查取消、并保证 abort 错误只抛一次的守卫。
/// Shared test/inspection helper: guards used by the service to detect an
/// aborted call and translate it to the JS failure shape.
pub struct AbortGuard {
/// 监视的取消信号。
    cancel: CancelSignal,
/// 是否已抛出过 abort 错误。
    raised: Mutex<bool>,
}

/// 取消检查接口。
impl AbortGuard {
/// 绑定取消信号构造。
    pub fn new(cancel: CancelSignal) -> Self {
        Self {
            cancel,
            raised: Mutex::new(false),
        }
    }

/// 是否已抛出过 abort 错误。
    pub fn tripped(&self) -> bool {
        *self.raised.lock().unwrap_or_else(|e| e.into_inner())
    }

/// 已取消时置位并返回 abort 错误，未取消返回 Ok。
    pub fn check(&self) -> Result<(), SmallModelError> {
        if self.cancel.is_cancelled() {
            *self.raised.lock().unwrap_or_else(|e| e.into_inner()) = true;
            return Err(abort_error());
        }
        Ok(())
    }
}

/// 取消信号、序列化与错误映射的测试。
#[cfg(test)]
mod tests {
    use super::*;

/// cancel 唤醒所有等待者并置位标志。
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

/// same 按底层令牌判同源。
    #[test]
    fn same_detects_identity_of_the_underlying_token() {
        let a = CancelSignal::default();
        let b = a.clone();
        let c = CancelSignal::default();
        assert!(a.same(&b));
        assert!(!a.same(&c));
    }

/// to_value/label/to_entry_value 与 JS 的形状一致。
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

/// 仅 structured-output 拒绝或 4xx 触发 schema 回退。
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

/// 三种结构化错误映射到对应状态码；未识别错误得到 None。
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

/// 默认 seam 的 describe 返回 None（无模型）。
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
