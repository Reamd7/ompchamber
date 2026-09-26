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

use crate::context::RouterContext;

pub mod dictation;
pub mod tts;

pub fn router(ctx: RouterContext) -> axum::Router {
    tts::router(ctx.clone()).merge(dictation::router(ctx))
}
