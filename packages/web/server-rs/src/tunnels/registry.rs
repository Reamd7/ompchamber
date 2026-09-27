//! Port of `server/lib/tunnels/registry.js`: the provider registry. The JS
//! registry validates providers at registration and seals after construction;
//! here `register` validates and the registry is shared immutably afterwards
//! (inherent sealing — no mutation path survives the builder).
//! （中文说明）provider 注册表：集中保管 cloudflare/ngrok 等 tunnel
//! provider 实例，提供注册期校验、大小写不敏感查找与能力列表，供
//! service/routes 层按 provider id 分发请求。

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;

use super::types::{ModeDescriptor, TunnelServiceError, TunnelStartRequest};

/// `checkAvailability()` merged with install info (JS spreads the two).
///
/// `checkAvailability` 结果与 `install-help` 安装信息的合并视图：
/// 依赖缺失时前端可直接用 message/install_command 给出指引。
#[derive(Debug, Clone)]
pub struct AvailabilityInfo {
    /// 二进制是否可用（PATH 可解析且 --version 探测成功）。
    pub available: bool,
    /// 探测到的版本号字符串；不可用时为 None。
    pub version: Option<String>,
    /// 依赖二进制名（cloudflared/ngrok）。
    pub dependency: String,
    /// 当前平台的安装命令。
    pub install_command: String,
    /// 官方下载页 URL。
    pub install_url: String,
    /// 归一化平台标识（darwin/win32/linux）。
    pub platform: String,
    /// 面向用户的"未安装"提示文案。
    pub message: String,
}

/// `provider.start(request, { activePort, originUrl, ...options })` context.
///
/// start 调用的运行时上下文：本地端口与来源 URL，provider 据此决定
/// 转发目标；两者均可缺省（诊断性启动等场景）。
#[derive(Debug, Clone, Default)]
pub struct StartContext {
    /// 需要暴露到公网的本地端口。
    pub active_port: Option<u16>,
    /// 发起方 origin URL，用于展示/回调。
    pub origin_url: Option<String>,
}

/// Distinguishes an explicit `TunnelServiceError` thrown by the provider from
/// a plain `Error` (which `createTunnelService.start` re-wraps as
/// `startup_failed`).
///
/// 两种失败形态：Service 携带结构化错误码（原样直达客户端）；Raw 是
/// 裸字符串，service 层会将其重包为 startup_failed，对齐 JS 中
/// plain Error 的处理分支。
#[derive(Debug, Clone)]
pub enum StartFailure {
    /// provider 显式抛出的隧道服务错误（含错误码与消息）。
    Service(TunnelServiceError),
    /// 未分类失败的原始消息，由上层包装成 startup_failed。
    Raw(String),
}

/// 允许 `?` 把 `TunnelServiceError` 直接转换为 `StartFailure::Service`，
/// provider 实现里无需手写 match。
impl From<TunnelServiceError> for StartFailure {
    /// 包裹为 Service 变体，语义不变。
    fn from(error: TunnelServiceError) -> Self {
        StartFailure::Service(error)
    }
}

/// JS tunnel controller (`{ mode, stop(), process, getPublicUrl(), ... }`).
/// `public_url` is fixed by the time the start future resolves (the JS start
/// awaits readiness), so it is stored, not polled.
///
/// 运行中隧道的控制句柄；stop 闭包应幂等，public_url 在 start future
/// 完成时已就绪（与 JS 等待就绪后再返回的时序一致），故存值不轮询。
#[derive(Clone)]
pub struct TunnelController {
    /// Set by `createTunnelService.start` after the provider returns.
    ///
    /// 完成注册的 provider id。
    pub provider: Option<String>,
    /// 启动模式（quick/managed-remote 等）。
    pub mode: String,
    /// 已确认的公网 URL；未就绪或无 URL 为 None。
    pub public_url: Option<String>,
    /// 终止隧道的回调（杀子进程/关连接）；缺失表示无需清理。
    pub stop: Option<Arc<dyn Fn() + Send + Sync>>,
    /// 实际生效的 managed 配置文件路径（provider 生成配置时）。
    pub effective_config_path: Option<String>,
    /// 最终解析出的主机名（可能与请求值不同，如加了随机前缀）。
    pub resolved_hostname: Option<String>,
}

/// 手写 Debug：输出各字段值但跳过 stop 闭包（函数指针无可读输出）。
impl std::fmt::Debug for TunnelController {
    /// 列出可显示字段；finish_non_exhaustive 标记仍有省略字段。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TunnelController")
            .field("provider", &self.provider)
            .field("mode", &self.mode)
            .field("public_url", &self.public_url)
            .field("effective_config_path", &self.effective_config_path)
            .field("resolved_hostname", &self.resolved_hostname)
            .finish_non_exhaustive()
    }
}

/// 控制器的便捷操作。
impl TunnelController {
    /// 调用注入的停止回调；未设置或已停止时为 no-op。
    pub fn stop(&self) {
        if let Some(stop) = &self.stop {
            stop();
        }
    }
}

/// `provider.diagnose(request)` input (routes.js `doctorRequest`).
///
/// doctor 诊断请求的输入快照：mode/hostname/token 等显式字段，加上
/// "字段是否出现在请求中"的布尔标记，用于区分"未提供"与"提供了
/// 空值"。
#[derive(Debug, Clone, Default)]
pub struct DiagnoseRequest {
    /// 请求的隧道模式；未指定为 None（provider 取默认模式）。
    pub mode: Option<String>,
    /// 请求绑定的主机名。
    pub hostname: Option<String>,
    /// 提供的 tunnel token（如 cloudflare tunnel token）。
    pub token: Option<String>,
    /// 请求里是否出现 token 字段（与 token 值是否为空相互独立）。
    pub token_provided: bool,
    /// 请求里是否出现 hostname 字段。
    pub hostname_provided: bool,
    /// 指定的 managed 配置文件路径。
    pub config_path: Option<String>,
    /// 是否存在已保存的 managed-remote profile（影响 blockers 提示）。
    pub has_saved_managed_remote_profile: bool,
}

/// tunnel provider 统一契约：能力声明、可用性检查、doctor 诊断与
/// 隧道生命周期管理。注册表只依赖此 trait，cloudflare/ngrok 各自实现。
pub trait TunnelProvider: Send + Sync {
    /// provider 唯一标识（"cloudflare"/"ngrok"），注册与查找的键。
    fn id(&self) -> &'static str;
    /// 面向前端的能力 JSON（模式列表、默认模式、稳定性标记等）。
    fn capabilities_json(&self) -> Value;
    /// 支持模式的静态描述表（键、标签、intent、requires、稳定性）。
    fn mode_descriptors(&self) -> &'static [ModeDescriptor];
    /// 检查依赖是否安装可用，返回值已合并安装指引信息。
    fn check_availability(&self) -> BoxFuture<'static, AvailabilityInfo>;
    /// `{ providerChecks, modes }`.
    ///
    /// 返回 `{ providerChecks, modes }` 形态的 doctor 结果：各项依赖
    /// 检查结论与各模式的就绪状态/blockers。
    fn diagnose(&self, request: DiagnoseRequest) -> BoxFuture<'static, Value>;
    /// 启动一条隧道并等待公网 URL 就绪；失败以 `StartFailure` 区分
    /// 结构化错误与裸错误。
    fn start(
        &self,
        request: TunnelStartRequest,
        context: StartContext,
    ) -> BoxFuture<'static, Result<TunnelController, StartFailure>>;
    /// 停止 controller 所代表的隧道。
    fn stop(&self, controller: &TunnelController);
    /// 读取 controller 的公网 URL；默认实现直接返回存储值，需要动态
    /// 解析（如轮询 agent API）的 provider 可覆写。
    fn resolve_public_url(&self, controller: &TunnelController) -> Option<String> {
        controller.public_url.clone()
    }
    /// 返回隧道元数据 JSON（进程信息、配置路径等诊断用途）。
    fn get_metadata(&self, controller: Option<&TunnelController>) -> Value;
}

/// JS 注册校验要求的必备方法名清单；Rust trait 已静态保证，保留仅为
/// 与 JS 注册表行为对齐（见 `register`）。
const REQUIRED_PROVIDER_METHODS: [&str; 4] =
    ["start", "stop", "checkAvailability", "resolvePublicUrl"];

/// provider 容器：注册期校验，共享后不可变（构造完成后无变异路径，
/// 等价于 JS 版的 seal 语义）。
pub struct TunnelProviderRegistry {
    /// 按注册顺序保存的 provider 实例。
    providers: Vec<Arc<dyn TunnelProvider>>,
}

/// 注册与查询接口。
impl TunnelProviderRegistry {
    /// 创建空注册表。
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// `registry.register(provider)` — validation errors mirror the JS strings.
    ///
    /// 校验 id 非空且未重复（trim + 小写比较），错误串与 JS 版
    /// 一致；成功压入列表尾部。
    pub fn register(&mut self, provider: Arc<dyn TunnelProvider>) -> Result<(), String> {
        let id = provider.id();
        if id.trim().is_empty() {
            return Err("Tunnel provider must define a non-empty id".to_string());
        }
        // The trait guarantees the required methods; kept for parity with JS.
        let _ = REQUIRED_PROVIDER_METHODS;
        let key = id.trim().to_lowercase();
        if self.providers.iter().any(|existing| existing.id() == key) {
            return Err(format!("Tunnel provider '{key}' is already registered"));
        }
        self.providers.push(provider);
        Ok(())
    }

    /// `registry.get(providerId)` — trim + lowercase lookup, `None` for junk.
    ///
    /// 按 id 查找 provider：先 trim 再小写匹配；空白或未注册的
    /// id 返回 None。
    pub fn get(&self, provider_id: &str) -> Option<Arc<dyn TunnelProvider>> {
        let trimmed = provider_id.trim();
        if trimmed.is_empty() {
            return None;
        }
        let key = trimmed.to_lowercase();
        self.providers
            .iter()
            .find(|provider| provider.id() == key)
            .cloned()
    }

    /// `registry.listCapabilities()`.
    ///
    /// 依注册顺序返回所有 provider 的能力 JSON 列表。
    pub fn list_capabilities(&self) -> Vec<Value> {
        self.providers
            .iter()
            .map(|p| p.capabilities_json())
            .collect()
    }
}

/// Default 委托 `new`，保持空表语义。
impl Default for TunnelProviderRegistry {
    /// 等价于 `TunnelProviderRegistry::new()`。
    fn default() -> Self {
        Self::new()
    }
}

/// 注册表行为测试：大小写不敏感查找、重复注册拒绝与能力列表顺序。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnels::types::TUNNEL_INTENT_EPHEMERAL_PUBLIC;
    use serde_json::json;

    /// 最小 provider 桩：固定 id 与单模式描述，start 恒失败。
    struct StubProvider {
        /// provider 标识。
        id: &'static str,
    }

    /// 桩实现：所有方法返回最小占位值。
    impl TunnelProvider for StubProvider {
        /// 回传构造时给定的 id。
        fn id(&self) -> &'static str {
            self.id
        }

        /// 只含 provider 字段的最小能力对象。
        fn capabilities_json(&self) -> Value {
            json!({ "provider": self.id })
        }

        /// 单个 "quick" 模式描述（GA 稳定性）。
        fn mode_descriptors(&self) -> &'static [ModeDescriptor] {
            &[ModeDescriptor {
                key: "quick",
                label: "Quick",
                intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC,
                requires: &[],
                supports: &[],
                stability: "ga",
            }]
        }

        /// 恒返回"可用"，其余字段留空。
        fn check_availability(&self) -> BoxFuture<'static, AvailabilityInfo> {
            Box::pin(async {
                AvailabilityInfo {
                    available: true,
                    version: None,
                    dependency: String::new(),
                    install_command: String::new(),
                    install_url: String::new(),
                    platform: String::new(),
                    message: String::new(),
                }
            })
        }

        /// 空 providerChecks 与空 modes。
        fn diagnose(&self, _request: DiagnoseRequest) -> BoxFuture<'static, Value> {
            Box::pin(async { json!({ "providerChecks": [], "modes": [] }) })
        }

        /// 恒返回 Raw 失败（本桩不测试启动路径）。
        fn start(
            &self,
            _request: TunnelStartRequest,
            _context: StartContext,
        ) -> BoxFuture<'static, Result<TunnelController, StartFailure>> {
            Box::pin(async { Err(StartFailure::Raw("unused".to_string())) })
        }

        /// no-op。
        fn stop(&self, _controller: &TunnelController) {}

        /// 恒返回 null。
        fn get_metadata(&self, _controller: Option<&TunnelController>) -> Value {
            Value::Null
        }
    }

    /// 行为契约：查找对 id 大小写与首尾空白不敏感；空白/未注册返回 None。
    #[test]
    fn registers_and_looks_up_case_insensitively() {
        let mut registry = TunnelProviderRegistry::new();
        registry
            .register(Arc::new(StubProvider { id: "cloudflare" }))
            .expect("registers");

        assert!(registry.get(" CloudFlARE ").is_some());
        assert!(registry.get("").is_none());
        assert!(registry.get("   ").is_none());
        assert!(registry.get("ngrok").is_none());
    }

    /// 行为契约：重复注册同一 id 被拒绝，错误串与 JS 版一致。
    #[test]
    fn rejects_duplicate_registrations() {
        let mut registry = TunnelProviderRegistry::new();
        registry
            .register(Arc::new(StubProvider { id: "cloudflare" }))
            .expect("registers");
        let error = registry
            .register(Arc::new(StubProvider { id: "cloudflare" }))
            .expect_err("duplicate");
        assert_eq!(error, "Tunnel provider 'cloudflare' is already registered");
    }

    /// 行为契约：能力列表按注册顺序输出。
    #[test]
    fn lists_capabilities_in_registration_order() {
        let mut registry = TunnelProviderRegistry::new();
        registry
            .register(Arc::new(StubProvider { id: "cloudflare" }))
            .unwrap();
        registry
            .register(Arc::new(StubProvider { id: "ngrok" }))
            .unwrap();
        let capabilities = registry.list_capabilities();
        assert_eq!(capabilities.len(), 2);
        assert_eq!(capabilities[0]["provider"], "cloudflare");
        assert_eq!(capabilities[1]["provider"], "ngrok");
    }
}
