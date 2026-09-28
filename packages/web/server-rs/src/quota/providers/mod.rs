//! Port of `server/lib/quota/providers/index.js` — the provider registry and
//! dispatcher. Registry order matches the JS `Object.entries` insertion
//! order because `GET /api/quota/providers` returns it.
//!
//! 配额 provider 注册表与分发器，移植自 `server/lib/quota/providers/index.js`。
//! 每个子模块对应一个上游额度服务（Claude、Codex、OpenRouter 等），
//! 统一暴露 `is_configured` 与 `fetch_quota` 两个入口，由本模块的
//! [`registry()`] 按固定顺序聚合。顺序必须与 JS 版 `Object.entries`
//! 的插入顺序保持一致，因为 `GET /api/quota/providers` 直接返回该列表，
//! 前端依赖稳定排序。

use crate::quota::deps::QuotaDeps;

/// 注册表中的单个 provider 条目：把「某个上游额度服务的探测与抓取」
/// 打包成一对函数指针，供路由层按 id 分发。
pub(crate) struct ProviderEntry {
/// 对外稳定标识（与 JS 版 provider id 完全一致），出现在
/// `/api/quota/providers` 响应及前端配置匹配中。
    pub id: &'static str,
/// 同步探测：给定当前依赖上下文判断该 provider 的凭证是否已配置。
/// 只做本地检查（如 token 是否存在），不发网络请求。
    pub is_configured: fn(&QuotaDeps) -> bool,
/// 异步抓取额度：接收共享的 QuotaRuntime，返回装箱 future，解析为
/// 一个可直接序列化给前端的 JSON 值（失败时通常是带 `error` 字段的
/// 对象，而不是 Err）。
    pub fetch: fn(
        std::sync::Arc<crate::quota::runtime::QuotaRuntime>,
    ) -> futures::future::BoxFuture<'static, serde_json::Value>,
}

/// 构造 ProviderEntry 的通用形式：显式给出 id、探测函数与抓取函数，
/// 用于同一模块注册多个变体的场景（如 copilot 主额度与 addon 额度、
/// minimax 国际站与国内站）。
macro_rules! provider_registry_entry {
    ($id:expr, $is_configured:expr, $fetch:expr) => {
        ProviderEntry {
            id: $id,
            is_configured: $is_configured,
            fetch: $fetch,
        }
    };
}

/// 构造 ProviderEntry 的简写形式：约定模块内提供 `is_configured` 与
/// `fetch_quota` 两个标准函数，直接以模块名引用它们。绝大多数
/// provider 只注册一个额度源，走这条路径。
macro_rules! provider {
    ($id:expr, $module:ident) => {
        ProviderEntry {
            id: $id,
            is_configured: $module::is_configured,
            fetch: $module::fetch_quota,
        }
    };
}

/// 返回静态 provider 注册表。利用函数内 static 保证数组只构造一次、
/// 整个进程共享同一份 `&'static` 切片，调用方零开销地迭代。
/// 条目顺序即 API 返回顺序，改动顺序会破坏与 JS 版输出的逐字节对齐。
pub(crate) fn registry() -> &'static [ProviderEntry] {
    // 静态注册表：编译期确定内容，首次访问时完成初始化，此后只读。
    static REGISTRY: &[ProviderEntry] = &[
        provider!("claude", claude),
        provider!("codex", codex),
        provider!("crof", crof),
        provider!("cursor", cursor),
        provider!("deepseek", deepseek),
        provider!("google", google),
        provider!("zai-coding-plan", zai),
        provider!("zhipuai-coding-plan", zhipuai),
        provider!("kimi-for-coding", kimi),
        provider!("openrouter", openrouter),
        provider!("nano-gpt", nanogpt),
        provider!("github-copilot", copilot),
        provider_registry_entry!(
            "github-copilot-addon",
            copilot::is_configured,
            copilot::fetch_quota_addon
        ),
        provider!("minimax-coding-plan", minimax),
        provider_registry_entry!(
            "minimax-cn-coding-plan",
            minimax::is_configured_cn,
            minimax::fetch_quota_cn
        ),
        provider!("ollama-cloud", ollama_cloud),
        provider!("wafer", wafer),
        provider!("opencode-go", opencode_go),
        provider!("neuralwatt", neuralwatt),
        provider!("xai", xai),
    ];
    REGISTRY
}

/// Anthropic Claude 订阅额度（claude.ai OAuth/API）。
pub mod claude;
/// OpenAI Codex（ChatGPT 计划）额度。
pub mod codex;
/// GitHub Copilot 额度，含主额度 `fetch_quota` 与 addon 额度两个入口。
pub mod copilot;
/// Crof（Claude 反代聚合）额度。
pub mod crof;
/// Cursor 额度。
pub mod cursor;
/// DeepSeek 额度。
pub mod deepseek;
/// Google（Gemini）额度。
pub mod google;
/// Kimi For Coding（月之暗面）额度。
pub mod kimi;
/// MiniMax 额度，含国际站与国内站（cn）两套探测/抓取入口。
pub mod minimax;
/// NanoGPT 额度。
pub mod nanogpt;
/// NeuralWatt 额度。
pub mod neuralwatt;
/// Ollama Cloud 额度。
pub mod ollama_cloud;
/// OpenAI 直连额度。未列入 registry（不出现在 provider 列表 API 中），
/// 由调用方通过模块路径直接使用（见 quota 测试的直接调用）。
pub mod openai;
/// OpenCode Go 额度。
pub mod opencode_go;
/// OpenRouter 额度。
pub mod openrouter;
/// Wafer 额度。
pub mod wafer;
/// xAI（Grok）额度。
pub mod xai;
/// 智谱 AI Coding Plan（z.ai）额度。
pub mod zai;
/// 智谱 GLM Coding Plan（bigmodel）额度。
pub mod zhipuai;
