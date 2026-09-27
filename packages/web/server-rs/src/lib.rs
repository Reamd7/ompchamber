//! Rust port of the OpenChamber web server (`packages/web/server`).
//!
//! Port scope excludes `server/lib/omp-host` — that module stays TypeScript
//! running under Bun and is spawned as a managed child process by
//! [`engine::EngineState`]. Per-file port status lives in
//! `packages/web/server-rs/PORT-MANIFEST.md`.
//!
//! Module contract: each ported JS module owns `src/<name>/mod.rs` exposing
//! `pub fn router(ctx: crate::context::RouterContext) -> axum::Router`.

pub mod agent_memory;
pub mod agent_tool;
pub mod browser_control;
pub mod cli;
pub mod client_auth;
pub mod config;
pub mod desktop_control;
pub mod context;
pub mod core_routes;
pub mod dev_servers;
pub mod dictation_tts;
pub mod engine;
pub mod engine_env;
pub mod error;
pub mod event_stream;
pub mod fs_routes;
pub mod git_service;
pub mod github;
pub mod hub;
pub mod inherited_env;
pub mod linear;
pub mod magic_prompts;
pub mod markdown_image_grants;
pub mod notifications;
pub mod omp_host_natives;
pub mod openchamber_control;
pub mod openchamber_sessions;
pub mod opencode_meta;
pub mod opencode_plugins;
pub mod opencode_routes;
pub mod package_manager;
pub mod path_realpath_cache;
pub mod permission_auto_accept;
pub mod project_context;
pub mod projects;
pub mod provider_env_aliases;
pub mod proxy;
pub mod pwa_manifest;
pub mod quota;
pub mod realtime_proxy;
pub mod relay;
pub mod scheduled_tasks;
pub mod session_assist;
pub mod session_folders;
pub mod session_goal;
pub mod settings;
pub mod skills_catalog;
pub mod small_model;
pub mod static_assets;
pub mod terminal;
pub mod tunnels;
pub mod ui_auth;
pub mod walkthrough;

pub use context::RouterContext;
