//! Port of `server/lib/tunnels/index.js` (`createTunnelService`): provider
//! orchestration with the start mutex, replace-on-change semantics, and
//! missing-dependency / startup-failure error mapping.
//! （中文说明）provider 编排服务（createTunnelService 的移植）：以互斥
//! 锁串行化启动请求；mode 与 provider 未变时复用现有隧道，变化时先停
//! 旧再启新；启动前检查依赖可用性，并把各类失败映射为带稳定错误码的
//! TunnelServiceError（provider_unsupported、missing_dependency、
//! startup_failed 等）。

use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::executable_search::{real_cwd, real_home};
use super::install_help::get_tunnel_dependency_install_info;
use super::registry::{
    AvailabilityInfo, StartContext, StartFailure, TunnelController, TunnelProviderRegistry,
};
use super::types::{
    Platform, TUNNEL_MODE_QUICK, TUNNEL_PROVIDER_CLOUDFLARE, TunnelServiceError,
    normalize_tunnel_start_request, validate_tunnel_start_request,
};

/// tunnel 生命周期编排服务：包装 provider registry 与共享的活动隧道
/// 槽位，对外提供 start/stop 与状态查询。
pub struct TunnelService {
    /// 已注册的 provider 集合（构造后只读）。
    registry: Arc<TunnelProviderRegistry>,
    /// 与外部共享的活动隧道槽位（支持服务层之外的停机路径）。
    controller: Arc<Mutex<Option<TunnelController>>>,
    /// 返回当前会话活动端口的回调，用于构造 origin URL。
    get_active_port: Arc<dyn Fn() -> Option<u16> + Send + Sync>,
    /// quick 模式启动成功后的告警回调（提示临时隧道不稳定的横幅）。
    on_quick_tunnel_warning: Option<Arc<dyn Fn() + Send + Sync>>,
    /// `startLock`: JS promise-chain mutex preventing concurrent starts from
    /// orphaning child processes.
    /// （中文）启动互斥锁：同一时刻只允许一个 start 流程，防止并发启动
    /// 泄漏孤儿子进程。
    start_lock: tokio::sync::Mutex<()>,
}

/// start 成功的结果，路由层据此构造响应。
#[derive(Debug)]
pub struct StartOutcome {
    /// 当前有效的公开 URL。
    pub public_url: String,
    /// 当前活动 mode。
    pub active_mode: String,
    /// provider 标识。
    pub provider: String,
    /// provider 附加元数据（无则为 null）。
    pub provider_metadata: Value,
}

/// 生命周期管理与状态查询实现。
impl TunnelService {
    /// 注入 registry、共享槽位、端口回调与告警回调构造服务。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<TunnelProviderRegistry>,
        controller: Arc<Mutex<Option<TunnelController>>>,
        get_active_port: Arc<dyn Fn() -> Option<u16> + Send + Sync>,
        on_quick_tunnel_warning: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        Self {
            registry,
            controller,
            get_active_port,
            on_quick_tunnel_warning,
            start_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 克隆当前活动 controller（无隧道为 None）；锁中毒时取回内部值
    /// 继续而不是 panic。
    fn current_controller(&self) -> Option<TunnelController> {
        self.controller
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 替换活动槽位；传入 None 表示清除。
    fn set_controller(&self, next: Option<TunnelController>) {
        *self
            .controller
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = next;
    }

    /// `stop()` — returns whether an active tunnel was stopped.
    /// （中文）停止活动隧道：优先委托对应 provider 的 stop 实现，无
    /// provider 时直接调用 controller 的 stop 句柄；随后清空槽位。
    /// 返回是否确有隧道被停止。
    pub fn stop(&self) -> bool {
        let Some(controller) = self.current_controller() else {
            return false;
        };
        let provider = controller
            .provider
            .as_deref()
            .and_then(|id| self.registry.get(id));
        if let Some(provider) = provider {
            provider.stop(&controller);
        } else {
            controller.stop();
        }
        self.set_controller(None);
        true
    }

    /// `checkAvailability(providerId)`.
    /// （中文）查询指定 provider 的可用性；未知 provider 返回
    /// provider_unsupported 错误。
    pub async fn check_availability(
        &self,
        provider_id: &str,
    ) -> Result<AvailabilityInfo, TunnelServiceError> {
        let Some(provider) = self.registry.get(provider_id) else {
            return Err(TunnelServiceError::new(
                "provider_unsupported",
                format!("Unsupported tunnel provider: {provider_id}"),
            ));
        };
        Ok(provider.check_availability().await)
    }

    /// `resolveActiveMode()`.
    /// （中文）当前隧道的 mode；无活动隧道返回 None。
    pub fn resolve_active_mode(&self) -> Option<String> {
        self.current_controller().map(|controller| controller.mode)
    }

    /// `resolveActiveProvider()`.
    /// （中文）当前隧道的 provider 标识；无活动隧道返回 None。
    pub fn resolve_active_provider(&self) -> Option<String> {
        self.current_controller()
            .and_then(|controller| controller.provider)
    }

    /// `getPublicUrl()`.
    /// （中文）当前公开 URL：有注册 provider 时委托其 resolve_public_url
    /// （managed 模式可动态推导），否则直接读 controller 存储值。
    pub fn get_public_url(&self) -> Option<String> {
        let controller = self.current_controller()?;
        match controller
            .provider
            .as_deref()
            .and_then(|id| self.registry.get(id))
        {
            Some(provider) => provider.resolve_public_url(&controller),
            None => controller.public_url,
        }
    }

    /// `getProviderMetadata()`.
    /// （中文）当前 provider 元数据：委托 provider 的 get_metadata；
    /// 无隧道或无 provider 返回 null。
    pub fn get_provider_metadata(&self) -> Value {
        let Some(controller) = self.current_controller() else {
            return Value::Null;
        };
        match controller
            .provider
            .as_deref()
            .and_then(|id| self.registry.get(id))
        {
            Some(provider) => provider.get_metadata(Some(&controller)),
            None => Value::Null,
        }
    }

    /// `start(rawRequest)`.
    /// （中文）启动或复用隧道，整体流程：持锁串行、归一化请求、解析
    /// provider（未知报 provider_unsupported）、按能力表校验；若现有
    /// 隧道与请求的 mode/provider 一致则直接复用其 URL，否则先停旧
    /// 隧道，检查依赖可用性（不可用报 missing_dependency 并附安装
    /// 指引），以活动端口构造 origin 调用 provider.start（服务错误
    /// 透传、原始错误包装为 startup_failed），落位槽位并回填 provider；
    /// 启动后仍无公开 URL 则停掉并报 startup_failed。quick 模式成功后
    /// 触发告警回调。返回 URL、mode、provider 与元数据。
    pub async fn start(&self, raw_request: Value) -> Result<StartOutcome, TunnelServiceError> {
        // Serialize starts (JS awaits the previous startLock holder).
        let _guard = self.start_lock.lock().await;

        let request = normalize_tunnel_start_request(
            &raw_request,
            &Value::Null,
            &real_home(),
            &real_cwd(),
            Platform::current(),
        )?;

        let Some(provider) = self.registry.get(&request.provider) else {
            return Err(TunnelServiceError::new(
                "provider_unsupported",
                format!("Unsupported tunnel provider: {}", request.provider),
            ));
        };

        validate_tunnel_start_request(&request, provider.id(), provider.mode_descriptors())?;

        let mut public_url = self
            .current_controller()
            .and_then(|controller| provider.as_ref().resolve_public_url(&controller));
        let active_mode = self.resolve_active_mode();
        let active_provider = self.resolve_active_provider();

        if public_url.is_some()
            && (active_mode.as_deref() != Some(request.mode.as_str())
                || active_provider.as_deref() != Some(request.provider.as_str()))
        {
            self.stop();
            public_url = None;
        }

        if public_url.is_none() {
            let availability = provider.check_availability().await;
            if !availability.available {
                let message = if !availability.message.trim().is_empty() {
                    availability.message
                } else if request.provider == TUNNEL_PROVIDER_CLOUDFLARE {
                    get_tunnel_dependency_install_info(
                        TUNNEL_PROVIDER_CLOUDFLARE,
                        Platform::current().js_name(),
                    )
                    .message
                } else {
                    format!(
                        "Required dependency for provider '{}' is missing",
                        request.provider
                    )
                };
                return Err(TunnelServiceError::new("missing_dependency", message));
            }

            let active_port = (self.get_active_port)();
            let origin_url = active_port.map(|port| format!("http://127.0.0.1:{port}"));

            let mut controller = match provider
                .start(
                    request.clone(),
                    StartContext {
                        active_port,
                        origin_url,
                    },
                )
                .await
            {
                Ok(controller) => controller,
                Err(StartFailure::Service(error)) => return Err(error),
                Err(StartFailure::Raw(message)) => {
                    let message = if message.trim().is_empty() {
                        "Failed to start tunnel".to_string()
                    } else {
                        message
                    };
                    return Err(TunnelServiceError::new("startup_failed", message));
                }
            };
            controller.provider = Some(request.provider.clone());
            self.set_controller(Some(controller.clone()));

            public_url = provider.resolve_public_url(&controller);
            if public_url.is_none() {
                self.stop();
                return Err(TunnelServiceError::new(
                    "startup_failed",
                    "Tunnel started but no public URL was assigned",
                ));
            }

            if request.mode == TUNNEL_MODE_QUICK
                && let Some(warn) = &self.on_quick_tunnel_warning
            {
                warn();
            }
        }

        let public_url = public_url.unwrap_or_default();
        let provider_metadata = provider.get_metadata(self.current_controller().as_ref());
        let active_mode = self.resolve_active_mode().unwrap_or_default();
        let provider = request.provider.clone();

        Ok(StartOutcome {
            public_url,
            active_mode,
            provider,
            provider_metadata,
        })
    }
}

/// 服务编排行为测试：启动错误透传、provider 变化时替换、同参数复用、
/// 未知 provider 与依赖缺失的错误码，以及空停机。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnels::registry::DiagnoseRequest;
    use crate::tunnels::registry::TunnelProvider;
    use crate::tunnels::types::TUNNEL_INTENT_EPHEMERAL_PUBLIC;
    use crate::tunnels::types::TunnelStartRequest;
    use futures::future::BoxFuture;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// 可编程的 provider 桩：可控可用性与启动失败，并统计启动次数。
    struct FakeProvider {
        /// provider 标识。
        id: &'static str,
        /// check_availability 报告的可用性。
        available: bool,
        /// start 被调用的次数（原子计数）。
        started: AtomicUsize,
        /// 非空时 start 返回该消息对应的原始失败。
        start_error: Option<String>,
    }

    /// 三种预设构造：默认可用、不可用、启动必败。
    impl FakeProvider {
        /// 默认可用且不注入失败的桩。
        fn new(id: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                available: true,
                started: AtomicUsize::new(0),
                start_error: None,
            })
        }

        /// 报告依赖不可用的桩（触发 missing_dependency 路径）。
        fn unavailable(id: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                available: false,
                started: AtomicUsize::new(0),
                start_error: None,
            })
        }

        /// start 时返回固定原始错误的桩（触发 startup_failed 包装）。
        fn failing(id: &'static str, message: &str) -> Arc<Self> {
            Arc::new(Self {
                id,
                available: true,
                started: AtomicUsize::new(0),
                start_error: Some(message.to_string()),
            })
        }
    }

    /// TunnelProvider 的最小桩实现：能力表仅含 quick 模式。
    impl TunnelProvider for FakeProvider {
        /// 返回桩的固定标识。
        fn id(&self) -> &'static str {
            self.id
        }

        /// 返回最小 capabilities JSON。
        fn capabilities_json(&self) -> Value {
            json!({ "provider": self.id })
        }

        /// 返回静态单元素 quick 能力表。
        fn mode_descriptors(&self) -> &'static [super::super::types::ModeDescriptor] {
            // 静态能力表：trait 方法需返回 'static 生命周期切片，用常量
            // 表满足（编译期构造，无运行时开销）。
            static MODES: [super::super::types::ModeDescriptor; 1] =
                [super::super::types::ModeDescriptor {
                    key: "quick",
                    label: "Quick",
                    intent: TUNNEL_INTENT_EPHEMERAL_PUBLIC,
                    requires: &[],
                    supports: &[],
                    stability: "ga",
                }];
            &MODES
        }

        /// 返回构造时设定的可用性，其余字段留空。
        fn check_availability(&self) -> BoxFuture<'static, AvailabilityInfo> {
            let available = self.available;
            Box::pin(async move {
                AvailabilityInfo {
                    available,
                    version: None,
                    dependency: String::new(),
                    install_command: String::new(),
                    install_url: String::new(),
                    platform: String::new(),
                    message: String::new(),
                }
            })
        }

        /// 返回空的诊断结构。
        fn diagnose(&self, _request: DiagnoseRequest) -> BoxFuture<'static, Value> {
            Box::pin(async { json!({ "providerChecks": [], "modes": [] }) })
        }

        /// 计数后按注入返回失败，或返回以 provider 命名的示例 URL 的
        /// controller。
        fn start(
            &self,
            request: TunnelStartRequest,
            _context: StartContext,
        ) -> BoxFuture<'static, Result<TunnelController, StartFailure>> {
            self.started.fetch_add(1, Ordering::SeqCst);
            if let Some(message) = &self.start_error {
                let message = message.clone();
                return Box::pin(async move { Err(StartFailure::Raw(message)) });
            }
            let url = format!("https://{}.example-tunnel.dev", request.provider);
            Box::pin(async move {
                Ok(TunnelController {
                    provider: None,
                    mode: request.mode,
                    public_url: Some(url),
                    stop: None,
                    effective_config_path: None,
                    resolved_hostname: None,
                })
            })
        }

        /// 调用 controller 自带的 stop 句柄。
        fn stop(&self, controller: &TunnelController) {
            controller.stop();
        }

        /// 恒返回 null。
        fn get_metadata(&self, _controller: Option<&TunnelController>) -> Value {
            Value::Null
        }
    }

    /// 用给定 provider 构建注册表与服务（固定活动端口 3000、无告警
    /// 回调），返回服务与共享槽位。
    fn service_with(
        providers: Vec<Arc<dyn TunnelProvider>>,
    ) -> (Arc<TunnelService>, Arc<Mutex<Option<TunnelController>>>) {
        let mut registry = TunnelProviderRegistry::new();
        for provider in providers {
            registry.register(provider).expect("register");
        }
        let registry = Arc::new(registry);
        let slot: Arc<Mutex<Option<TunnelController>>> = Arc::new(Mutex::new(None));
        let service = Arc::new(TunnelService::new(
            registry,
            slot.clone(),
            Arc::new(|| Some(3000)),
            None,
        ));
        (service, slot)
    }

    /// provider 的原始启动错误被包装为 startup_failed 并把消息透传给
    /// 路由调用方。
    #[tokio::test]
    async fn returns_provider_startup_errors_to_route_callers() {
        // JS index.test.js: 'returns provider startup errors to route callers'.
        let ngrok = FakeProvider::failing("ngrok", "ngrok authtoken is not configured");
        let (service, _slot) = service_with(vec![ngrok as Arc<dyn TunnelProvider>]);

        let error = service
            .start(json!({ "provider": "ngrok", "mode": "quick" }))
            .await
            .expect_err("startup failure");
        assert_eq!(error.code, "startup_failed");
        assert_eq!(error.message, "ngrok authtoken is not configured");
    }

    /// provider 变化时先停止旧隧道（stop 句柄被调用）再用新 provider
    /// 启动。
    #[tokio::test]
    async fn replaces_an_active_quick_tunnel_when_the_provider_changes() {
        // JS index.test.js: 'replaces an active quick tunnel when the provider
        // changes'.
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_for_provider = stopped.clone();

        let cloudflare = FakeProvider::new("cloudflare");
        let ngrok = FakeProvider::new("ngrok");

        let mut registry = TunnelProviderRegistry::new();
        registry
            .register(cloudflare.clone() as Arc<dyn TunnelProvider>)
            .unwrap();
        registry
            .register(ngrok.clone() as Arc<dyn TunnelProvider>)
            .unwrap();
        let registry = Arc::new(registry);
        let slot: Arc<Mutex<Option<TunnelController>>> = Arc::new(Mutex::new(None));

        // Pre-activate a cloudflare quick tunnel with a stop flag.
        {
            let stop = stopped_for_provider.clone();
            *slot.lock().unwrap() = Some(TunnelController {
                provider: Some("cloudflare".to_string()),
                mode: "quick".to_string(),
                public_url: Some("https://cloudflare.example".to_string()),
                stop: Some(Arc::new(move || stop.store(true, Ordering::SeqCst))),
                effective_config_path: None,
                resolved_hostname: None,
            });
        }

        let service = TunnelService::new(registry, slot, Arc::new(|| Some(3000)), None);
        let result = service
            .start(json!({ "provider": "ngrok", "mode": "quick" }))
            .await
            .expect("starts ngrok");

        assert!(stopped.load(Ordering::SeqCst), "previous tunnel stopped");
        assert_eq!(ngrok.started.load(Ordering::SeqCst), 1);
        assert_eq!(result.provider, "ngrok");
        assert_eq!(result.public_url, "https://ngrok.example-tunnel.dev");
    }

    /// mode 与 provider 均未变化时复用现有 URL，不触发新的 start。
    #[tokio::test]
    async fn reuses_active_tunnel_for_same_mode_and_provider() {
        let cloudflare = FakeProvider::new("cloudflare");
        let (service, slot) = service_with(vec![cloudflare.clone()]);
        *slot.lock().unwrap() = Some(TunnelController {
            provider: Some("cloudflare".to_string()),
            mode: "quick".to_string(),
            public_url: Some("https://existing.example".to_string()),
            stop: None,
            effective_config_path: None,
            resolved_hostname: None,
        });

        let result = service
            .start(json!({ "provider": "cloudflare", "mode": "quick" }))
            .await
            .expect("reuses");
        assert_eq!(result.public_url, "https://existing.example");
        assert_eq!(cloudflare.started.load(Ordering::SeqCst), 0, "no restart");
    }

    /// 未注册的 provider 返回 provider_unsupported 错误码与消息。
    #[tokio::test]
    async fn unknown_provider_is_rejected() {
        let (service, _slot) = service_with(vec![
            FakeProvider::new("cloudflare") as Arc<dyn TunnelProvider>
        ]);
        let error = service
            .start(json!({ "provider": "ngrok", "mode": "quick" }))
            .await
            .expect_err("unknown provider");
        assert_eq!(error.code, "provider_unsupported");
        assert_eq!(error.message, "Unsupported tunnel provider: ngrok");
    }

    /// 依赖不可用时返回 missing_dependency 且消息包含安装指引。
    #[tokio::test]
    async fn missing_dependency_reports_install_message() {
        let (service, _slot) = service_with(vec![
            FakeProvider::unavailable("cloudflare") as Arc<dyn TunnelProvider>
        ]);
        let error = service
            .start(json!({ "provider": "cloudflare", "mode": "quick" }))
            .await
            .expect_err("missing dependency");
        assert_eq!(error.code, "missing_dependency");
        assert!(
            error.message.contains("cloudflared is not installed"),
            "{}",
            error.message
        );
    }

    /// 无活动隧道时 stop 返回 false。
    #[tokio::test]
    async fn stop_without_active_tunnel_returns_false() {
        let (service, _slot) = service_with(vec![
            FakeProvider::new("cloudflare") as Arc<dyn TunnelProvider>
        ]);
        assert!(!service.stop());
    }

    /// 冒烟：quick 隧道告警横幅可正常打印（不逐字节断言输出）。
    #[test]
    fn print_tunnel_warning_writes_banner() {
        // Smoke: mirrors JS console.log output; not asserted byte-for-byte.
        super::super::cloudflare::print_tunnel_warning();
    }
}
