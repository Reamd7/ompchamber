//! Port of `server/lib/tunnels/index.js` (`createTunnelService`): provider
//! orchestration with the start mutex, replace-on-change semantics, and
//! missing-dependency / startup-failure error mapping.

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

pub struct TunnelService {
    registry: Arc<TunnelProviderRegistry>,
    controller: Arc<Mutex<Option<TunnelController>>>,
    get_active_port: Arc<dyn Fn() -> Option<u16> + Send + Sync>,
    on_quick_tunnel_warning: Option<Arc<dyn Fn() + Send + Sync>>,
    /// `startLock`: JS promise-chain mutex preventing concurrent starts from
    /// orphaning child processes.
    start_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug)]
pub struct StartOutcome {
    pub public_url: String,
    pub active_mode: String,
    pub provider: String,
    pub provider_metadata: Value,
}

impl TunnelService {
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

    fn current_controller(&self) -> Option<TunnelController> {
        self.controller
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn set_controller(&self, next: Option<TunnelController>) {
        *self
            .controller
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = next;
    }

    /// `stop()` — returns whether an active tunnel was stopped.
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
    pub fn resolve_active_mode(&self) -> Option<String> {
        self.current_controller().map(|controller| controller.mode)
    }

    /// `resolveActiveProvider()`.
    pub fn resolve_active_provider(&self) -> Option<String> {
        self.current_controller()
            .and_then(|controller| controller.provider)
    }

    /// `getPublicUrl()`.
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

    struct FakeProvider {
        id: &'static str,
        available: bool,
        started: AtomicUsize,
        start_error: Option<String>,
    }

    impl FakeProvider {
        fn new(id: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                available: true,
                started: AtomicUsize::new(0),
                start_error: None,
            })
        }

        fn unavailable(id: &'static str) -> Arc<Self> {
            Arc::new(Self {
                id,
                available: false,
                started: AtomicUsize::new(0),
                start_error: None,
            })
        }

        fn failing(id: &'static str, message: &str) -> Arc<Self> {
            Arc::new(Self {
                id,
                available: true,
                started: AtomicUsize::new(0),
                start_error: Some(message.to_string()),
            })
        }
    }

    impl TunnelProvider for FakeProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn capabilities_json(&self) -> Value {
            json!({ "provider": self.id })
        }

        fn mode_descriptors(&self) -> &'static [super::super::types::ModeDescriptor] {
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

        fn diagnose(&self, _request: DiagnoseRequest) -> BoxFuture<'static, Value> {
            Box::pin(async { json!({ "providerChecks": [], "modes": [] }) })
        }

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

        fn stop(&self, controller: &TunnelController) {
            controller.stop();
        }

        fn get_metadata(&self, _controller: Option<&TunnelController>) -> Value {
            Value::Null
        }
    }

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

    #[tokio::test]
    async fn stop_without_active_tunnel_returns_false() {
        let (service, _slot) = service_with(vec![
            FakeProvider::new("cloudflare") as Arc<dyn TunnelProvider>
        ]);
        assert!(!service.stop());
    }

    #[test]
    fn print_tunnel_warning_writes_banner() {
        // Smoke: mirrors JS console.log output; not asserted byte-for-byte.
        super::super::cloudflare::print_tunnel_warning();
    }
}
