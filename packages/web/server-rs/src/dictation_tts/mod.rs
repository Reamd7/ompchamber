//! Port of `server/lib/tts/*` + `server/lib/dictation/*` — speech surfaces:
//! OpenAI-compatible TTS/STT proxy routes, macOS `say` synthesis with
//! language-aware voice selection, and the streaming dictation WebSocket
//! with local sherpa-onnx model management.
//!
//! Known gap (deliberate, see `dictation/local.rs`): the native
//! `sherpa-onnx-node` recognizer/speaker cannot be linked with the allowed
//! crates, so the local engine seam defaults to Unavailable with the JS's
//! exact error shapes; downloads, install checks, status, and the
//! OpenAI-compatible paths are fully ported.
//!
//! 中文说明：语音相关路由的总入口——合并 TTS（OpenAI 兼容代理与 macOS
//! `say` 合成）和听写（流式 WebSocket + 本地模型管理）两组子路由。

use crate::context::RouterContext;

/// 流式听写子系统：WebSocket 状态机、会话抽象、本地引擎 seam 与模型目录。
pub mod dictation;
/// TTS/STT 子系统：OpenAI 兼容代理路由与 macOS `say` 合成。
pub mod tts;

/// 组装并合并 tts 与 dictation 两组子路由。
pub fn router(ctx: RouterContext) -> axum::Router {
    tts::router(ctx.clone()).merge(dictation::router(ctx))
}
/// 原生 sherpa-onnx 本地引擎（仅在 `local-speech` feature 下编译）。
#[cfg(feature = "local-speech")]
pub mod native_sherpa;
