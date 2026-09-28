/**
 * Shared response shapes for OpenCode config mutations.
 *
 * Settings writes persist to disk immediately but defer the OpenCode restart
 * so the UI can accumulate pending changes and apply them once via
 * POST /api/config/reload ("Apply & Restart OpenCode").
 */
/**
 * （中文说明）OpenCode 配置变更接口的共享响应构造器。
 *
 * 设置写入立即落盘，但把 OpenCode 的重启推迟到用户在界面点击
 * “Apply & Restart OpenCode”（POST /api/config/reload）统一执行；
 * 外部托管形态则改为提示用户手动重启。
 */

/**
 * 构造“已保存、重启已推迟”的响应体：success 与 requiresRestart 为 true，
 * restartDeferred 标记让 UI 聚合待应用变更而不是立即重启。
 * @param {string} message - 展示给用户的提示文案。
 * @returns {{ success: true, requiresReload: false, requiresRestart: true,
 *   restartDeferred: true, message: string }}
 */
export function buildDeferredRestartResponse(message) {
  return {
    success: true,
    requiresReload: false,
    requiresRestart: true,
    restartDeferred: true,
    message,
  };
}

/**
 * 构造“需外部手动重启”的响应体（requiresManualRestart: true）：用于服务
 * 器由外部管理、无法自行重启 OpenCode 的部署形态。
 * @param {string} message - 展示给用户的提示文案。
 * @returns {{ success: true, requiresReload: false, requiresManualRestart: true,
 *   message: string }}
 */
export function buildExternalManualRestartResponse(message) {
  return {
    success: true,
    requiresReload: false,
    requiresManualRestart: true,
    message,
  };
}
