//! Port of `server/lib/tunnels/registry.js`: the provider registry. The JS
//! registry validates providers at registration and seals after construction;
//! here `register` validates and the registry is shared immutably afterwards
//! (inherent sealing — no mutation path survives the builder).

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;

use super::types::{ModeDescriptor, TunnelServiceError, TunnelStartRequest};

/// `checkAvailability()` merged with install info (JS spreads the two).
#[derive(Debug, Clone)]
pub struct AvailabilityInfo {
    pub available: bool,
    pub version: Option<String>,
    pub dependency: String,
    pub install_command: String,
    pub install_url: String,
    pub platform: String,
    pub message: String,
}

/// `provider.start(request, { activePort, originUrl, ...options })` context.
#[derive(Debug, Clone, Default)]
pub struct StartContext {
    pub active_port: Option<u16>,
    pub origin_url: Option<String>,
}

/// Distinguishes an explicit `TunnelServiceError` thrown by the provider from
/// a plain `Error` (which `createTunnelService.start` re-wraps as
/// `startup_failed`).
#[derive(Debug, Clone)]
pub enum StartFailure {
    Service(TunnelServiceError),
    Raw(String),
}

impl From<TunnelServiceError> for StartFailure {
    fn from(error: TunnelServiceError) -> Self {
        StartFailure::Service(error)
    }
}

/// JS tunnel controller (`{ mode, stop(), process, getPublicUrl(), ... }`).
/// `public_url` is fixed by the time the start future resolves (the JS start
/// awaits readiness), so it is stored, not polled.
#[derive(Clone)]
pub struct TunnelController {
    /// Set by `createTunnelService.start` after the provider returns.
    pub provider: Option<String>,
    pub mode: String,
    pub public_url: Option<String>,
    pub stop: Option<Arc<dyn Fn() + Send + Sync>>,
    pub effective_config_path: Option<String>,
    pub resolved_hostname: Option<String>,
}

impl std::fmt::Debug for TunnelController {
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

impl TunnelController {
    pub fn stop(&self) {
        if let Some(stop) = &self.stop {
            stop();
        }
    }
}

/// `provider.diagnose(request)` input (routes.js `doctorRequest`).
#[derive(Debug, Clone, Default)]
pub struct DiagnoseRequest {
    pub mode: Option<String>,
    pub hostname: Option<String>,
    pub token: Option<String>,
    pub token_provided: bool,
    pub hostname_provided: bool,
    pub config_path: Option<String>,
    pub has_saved_managed_remote_profile: bool,
}

pub trait TunnelProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities_json(&self) -> Value;
    fn mode_descriptors(&self) -> &'static [ModeDescriptor];
    fn check_availability(&self) -> BoxFuture<'static, AvailabilityInfo>;
    /// `{ providerChecks, modes }`.
    fn diagnose(&self, request: DiagnoseRequest) -> BoxFuture<'static, Value>;
    fn start(
        &self,
        request: TunnelStartRequest,
        context: StartContext,
    ) -> BoxFuture<'static, Result<TunnelController, StartFailure>>;
    fn stop(&self, controller: &TunnelController);
    fn resolve_public_url(&self, controller: &TunnelController) -> Option<String> {
        controller.public_url.clone()
    }
    fn get_metadata(&self, controller: Option<&TunnelController>) -> Value;
}

const REQUIRED_PROVIDER_METHODS: [&str; 4] =
    ["start", "stop", "checkAvailability", "resolvePublicUrl"];

pub struct TunnelProviderRegistry {
    providers: Vec<Arc<dyn TunnelProvider>>,
}

impl TunnelProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    /// `registry.register(provider)` — validation errors mirror the JS strings.
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
    pub fn list_capabilities(&self) -> Vec<Value> {
        self.providers
            .iter()
            .map(|p| p.capabilities_json())
            .collect()
    }
}

impl Default for TunnelProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnels::types::TUNNEL_INTENT_EPHEMERAL_PUBLIC;
    use serde_json::json;

    struct StubProvider {
        id: &'static str,
    }

    impl TunnelProvider for StubProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn capabilities_json(&self) -> Value {
            json!({ "provider": self.id })
        }

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

        fn diagnose(&self, _request: DiagnoseRequest) -> BoxFuture<'static, Value> {
            Box::pin(async { json!({ "providerChecks": [], "modes": [] }) })
        }

        fn start(
            &self,
            _request: TunnelStartRequest,
            _context: StartContext,
        ) -> BoxFuture<'static, Result<TunnelController, StartFailure>> {
            Box::pin(async { Err(StartFailure::Raw("unused".to_string())) })
        }

        fn stop(&self, _controller: &TunnelController) {}

        fn get_metadata(&self, _controller: Option<&TunnelController>) -> Value {
            Value::Null
        }
    }

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
