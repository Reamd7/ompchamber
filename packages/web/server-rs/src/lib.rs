//! Rust port of the OpenChamber web server (`packages/web/server`).
//!
//! Port scope excludes `server/lib/omp-host` — that module stays TypeScript
//! running under Bun and is spawned as a managed child process by
//! [`engine::EngineState`]. Per-file port status lives in
//! `packages/web/server-rs/PORT-MANIFEST.md`.
//!
//! Module contract: each ported JS module owns `src/<name>/mod.rs` exposing
//! `pub fn router(ctx: crate::context::RouterContext) -> axum::Router`.
//!
//! 中文说明：本 crate 是 OpenChamber web 服务器的 Rust 移植版（替代
//! `packages/web/server` 的 axum 实现）。`server/lib/omp-host` 不在移植范围
//! 内——它保持 TypeScript 并由 [`engine::EngineState`] 作为受管子进程拉起。
//! 模块契约：每个移植模块在自己的 `src/<name>/mod.rs` 中暴露
//! `pub fn router(ctx: crate::context::RouterContext) -> axum::Router`。

/// agent 记忆存储（`server/lib/agent-memory/` 的移植）。
pub mod agent_memory;
/// 把 OpenChamber 暴露为 agent 可用的 OpenCode 自定义工具（agent-tool runtime）。
pub mod agent_tool;
/// agent 工具与应用内浏览器之间的请求/响应代理（browser-control）。
pub mod browser_control;
/// CLI 层：子命令分发、退出码、pid/instance/log 状态。
pub mod cli;
/// 远程客户端配对与 tunnel 会话鉴权（client-auth）。
pub mod client_auth;
/// serve 子命令的 CLI/env 配置解析。
pub mod config;
/// Electron 桌面壳的私有控制通道（newline JSON 协议）。
pub mod desktop_control;
/// `std::os::unix` 扩展 trait 的跨平台垫片。
pub mod os_compat;
/// 各模块路由共享的构造上下文（RouterContext）。
pub mod context;
/// 核心 HTTP 路由：状态/系统/设置工具与引擎健康等（core-routes）。
pub mod core_routes;
/// dev server 发现（枚举监听套接字）与相关路由。
pub mod dev_servers;
/// 语音界面：OpenAI 兼容 TTS/STT 代理、macOS say 合成、流式听写 WS。
pub mod dictation_tts;
/// omp-host 引擎子进程的受控生命周期（spawn/就绪探活/停机）。
pub mod engine;
/// 引擎环境/启动支持运行时（登录 shell 环境快照、PATH 增强等）。
pub mod engine_env;
/// 各模块共享的错误/响应约定（JSON 错误形状）。
pub mod error;
/// SSE 事件流协议、全局 hub、上游读取器与桥接（event-stream + watcher）。
pub mod event_stream;
/// `/api/fs/*` 文件系统路由族与模糊搜索。
pub mod fs_routes;
/// `/api/git/*` 路由族与 git 服务层。
pub mod git_service;
/// `/api/github/*` 路由（GitHub 集成）。
pub mod github;
/// 服务端 SSE 事件广播中枢（EventHub）。
pub mod hub;
/// 面向用户子进程的继承环境清理（如剔除 ARGV0）。
pub mod inherited_env;
/// Linear 集成：OAuth(PKCE)、token 存储、GraphQL、回调路由。
pub mod linear;
/// 持久化的 prompt 覆盖（magic-prompts）。
pub mod magic_prompts;
/// 为消息中实际引用的图片签发路径绑定的原图访问授权。
pub mod markdown_image_grants;
/// 通知子系统：web-push、APNs、触发状态机、桌面钩子。
pub mod notifications;
/// `pi_natives` 原生插件的运行时装配（复制/下载到每用户缓存）。
pub mod omp_host_natives;
/// OpenChamber CLI 的类型化控制契约与编排服务（action 白名单）。
pub mod openchamber_control;
/// OMPChamber 会话编排路由（创建/工作树/初始 prompt/goal）。
pub mod openchamber_sessions;
/// OpenCode 元信息面（openchamber-routes 等）。
pub mod opencode_meta;
/// OpenCode 插件/技能/片段面。
pub mod opencode_plugins;
/// OpenCode 配置 CRUD 路由。
pub mod opencode_routes;
/// 包管理器（openchamber-routes 依赖的库模块）。
pub mod package_manager;
/// 异步 realpath 解析的 TTL+LRU 缓存（含 in-flight 去重）。
pub mod path_realpath_cache;
/// 满足条件的 OpenCode 权限请求自动接受。
pub mod permission_auto_accept;
/// 项目知识存储：notes/todos/plan 文件（project-context）。
pub mod project_context;
/// 项目配置与项目 ID 解析（无路由的库模块）。
pub mod projects;
/// 凭证环境变量别名镜像（Google API key 系列）。
pub mod provider_env_aliases;
/// OpenCode wire-API 转发代理。
pub mod proxy;
/// PWA manifest 路由（`/manifest.webmanifest`）。
pub mod pwa_manifest;
/// AI 提供方配额用量跟踪。
pub mod quota;
/// 桌面实时代理：SSE/WS 白名单转发到桌面 loopback UI 运行时。
pub mod realtime_proxy;
/// 私有 relay 主机侧（移动端/浏览器远程接入）。
pub mod relay;
/// OpenChamber 计划任务：调度、markdown loop 发现、CRUD/run 路由。
pub mod scheduled_tasks;
/// 会话辅助（session-assist）：hub 消费者三件套。
pub mod session_assist;
/// UI 会话目录树的持久化（sessions-directories.json）。
pub mod session_folders;
/// 会话目标（goal）：文件式 objectives 与服务端创建/续跑循环。
pub mod session_goal;
/// 设置模块：settings-runtime/normalization 等。
pub mod settings;
/// 技能目录：扫描、安装、缓存、精选源与 GitHub 元数据。
pub mod skills_catalog;
/// 服务端直连 LLM 的小模型调用（复用 OpenCode provider 登录）。
pub mod small_model;
/// 静态 UI 资源服务与 SPA 回退。
pub mod static_assets;
/// 终端 HTTP + WebSocket 面（`/api/terminal/ws`）。
pub mod terminal;
/// tunnel 集成：注册表、路由、提供方（cloudflare/ngrok）等。
pub mod tunnels;
/// UI 密码门、会话 token、登录限流、origin 检查（ui-auth）。
pub mod ui_auth;
/// diff 的引导式走读生成（walkthrough）。
pub mod walkthrough;

/// 再导出共享的模块路由构造上下文类型。
pub use context::RouterContext;
