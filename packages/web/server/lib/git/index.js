// Git library public entrypoint
// Re-exports all Git operations, credentials, and identity storage functions
/**
 * Git 模块公共入口（barrel 文件）。
 *
 * 将 service.js 的 Git 操作（status、stage、worktree 等）、credentials.js
 * 的凭证发现与 identity-storage.js 的身份配置持久化统一 re-export，
 * 供 server 端路由与其它模块通过单一入口消费。
 */

export * from './service.js';
export * from './credentials.js';
export * from './identity-storage.js';
