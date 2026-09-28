//! Port of `server/lib/fs/routes.js` + `search.js` — the `/api/fs/*` route
//! family (directory listing, file read/write/upload/delete/rename, reveal,
//! exec jobs, git-dir discovery) plus the fuzzy filesystem search runtime
//! used by other modules (project icon discovery).
//!
//! JS source map:
//! - `server/lib/fs/routes.js`  → `handlers.rs` (+ `workspace.rs`, `exec.rs`,
//!   `git_dirs.rs` for the helpers the file defines at module scope)
//! - `server/lib/fs/search.js`  → `search.rs`
//!
//! Composition-root dependencies the JS receives from `index.js` are ported
//! inline and documented at each use: `normalizeDirectoryPath`
//! (settings-normalization-runtime.js), `resolveProjectDirectory`
//! (opencode/project-directory-runtime.js), `ompchamberUserConfigRoot`
//! (index.js `~/.config/ompchamber`), and `resolveGitBinaryForSpawn`
//! (plain `git` off win32). The login-shell PATH merge
//! (`buildAugmentedPath`) is not ported — exec/clone inherit the parent PATH.

//!
//! 中文说明：本模块是 JS 版 `/api/fs/*` 路由族的 Rust 聚合入口。子模块
//! 划分对应原 JS 文件：`handlers.rs` 承载全部路由处理器，`workspace.rs`
//! 负责工作区边界判定与外部文件授权，`exec.rs` 提供命令执行原语、
//! git-read 缓存与 exec 任务存储，`git_dirs.rs` 发现嵌套 git 仓库，
//! `paths.rs` 复刻 Node `path` 语义，`search.rs` 是可被其它模块复用的
//! 模糊文件搜索运行时。路由组装入口为 [`router`]（自建状态）与
//! [`router_with`]（调用方持有状态，便于跨模块共享授权存储）。

/// 命令执行子系统：shell 命令运行、git 只读结果缓存与 exec 任务存储。
mod exec;
/// 嵌套 git 仓库发现（`/api/fs/git-dirs` 路由的底层遍历实现）。
mod git_dirs;
/// 各 `/api/fs/*` 路由的 axum 处理器及请求/响应的 JSON 形状约定。
pub mod handlers;
/// 路径原语：复刻 Node `path` / `fsPromises.realpath` 语义与 URI 编解码、
/// 随机标识符等工具函数。
mod paths;
/// 模糊文件系统搜索运行时（JS `search.js` 的移植，亦被非 fs 路由复用）。
pub mod search;
/// 工作区边界判定与外部文件（outside-file）授权存储。
mod workspace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};

pub use search::{SearchHit, SearchOptions, search_filesystem_files};
pub use workspace::OutsideGrantStore;

use crate::context::RouterContext;
use axum::Router;

/// Realpath memoization — port of path-realpath-cache.js as used by the
/// `/api/fs/list` route (success TTL 10 min, failure TTL 60 s, 256 entries).
///
/// 中文说明：realpath 结果的记忆化缓存，键为原始路径字符串，值为
/// (绝对过期时刻, 解析结果)。命中且未过期直接返回缓存；否则重新
/// canonicalize 并按成功/失败写入不同 TTL 的条目，实现与 JS 一致的
/// 行为（成功 10 分钟、失败 60 秒、上限 256 条）。
#[derive(Default)]
pub struct RealpathCache {
    /// 缓存条目表：路径字符串 → (绝对过期时刻 ms, canonicalize 结果或错误种类)。
    entries: Mutex<HashMap<String, (u64, Result<PathBuf, std::io::ErrorKind>)>>,
}

/// 成功解析结果的缓存有效期（10 分钟）。
const REALPATH_SUCCESS_TTL_MS: u64 = 600_000;
/// 解析失败结果的缓存有效期（60 秒，较短以便快速恢复瞬时故障）。
const REALPATH_FAILURE_TTL_MS: u64 = 60_000;
/// 缓存最大条目数；达到上限时整体清空（JS 侧为逐出最旧插入项）。
const REALPATH_MAX_ENTRIES: usize = 256;

/// 带过期检查的 realpath 缓存查询实现。
impl RealpathCache {
    /// 解析 `value` 的 canonical 路径，带 TTL 记忆化。
    ///
    /// 未过期命中时克隆缓存结果（失败条目重建为 `io::Error` 返回）；
    /// 否则调用 `paths::realpath` 重新解析并写入新条目。条目达到上限
    /// 时清空整表。锁中毒通过 `into_inner` 恢复，缓存永不 panic。
    pub fn resolve(&self, value: &Path) -> std::io::Result<PathBuf> {
        let key = value.to_string_lossy().into_owned();
        let now = exec::now_ms();
        {
            let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((expires_at, cached)) = entries.get(&key)
                && now < *expires_at
            {
                return cached.clone().map_err(std::io::Error::from);
            }
        }
        let result = paths::realpath(value);
        let ttl = if result.is_ok() {
            REALPATH_SUCCESS_TTL_MS
        } else {
            REALPATH_FAILURE_TTL_MS
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        // Cap the cache; the JS evicts oldest-inserted first, the map here is
        // unordered — the bound is what matters.
        if entries.len() >= REALPATH_MAX_ENTRIES {
            entries.clear();
        }
        entries.insert(
            key,
            (
                now + ttl,
                result.as_ref().map(|p| p.clone()).map_err(|e| e.kind()),
            ),
        );
        result
    }
}

/// `/api/fs` 路由族的共享运行时状态（`FsState::new` 一次性装配，进程内只读）。
pub struct FsInner {
    /// 外部文件授权存储（token → 授权条目），供跨模块铸造一次性授权。
    pub grants: OutsideGrantStore,
    /// exec 任务表（jobId → 任务），支撑 `GET /api/fs/exec/:jobId` 轮询。
    pub exec_jobs: exec::ExecJobStore,
    /// git 只读命令结果缓存（Arc 共享，与 exec 执行路径共用同一实例）。
    pub git_read_cache: Arc<exec::GitReadCache>,
    /// `/api/fs/list` 路由使用的 realpath 记忆化缓存。
    pub realpath_cache: RealpathCache,
    /// 命令执行硬超时（毫秒）；进程启动时从环境变量读取一次。
    pub command_timeout_ms: u64,
    /// `git check-ignore` 子进程超时（毫秒），供搜索运行时使用。
    pub git_check_ignore_timeout_ms: u64,
}

/// Module-local state owned by the returned router (`Router::with_state`).
///
/// 中文说明：路由持有的模块局部状态，内部是 `Arc<FsInner>`，克隆只增加
/// 引用计数；通过 `Deref` 透明暴露 `FsInner` 的全部字段。
#[derive(Clone)]
pub struct FsState {
    /// 共享状态指针；克隆 `FsState` 仅复制 Arc。
    inner: Arc<FsInner>,
}

/// 让 `FsState` 透明转发 `FsInner` 的全部字段（JS 中直接引用同一对象）。
impl std::ops::Deref for FsState {
    /// Deref 目标即内部状态本体。
    type Target = FsInner;
    /// 返回内部 `FsInner` 的引用。
    fn deref(&self) -> &FsInner {
        &self.inner
    }
}

/// 默认构造等价于 [`FsState::new`]。
impl Default for FsState {
    /// 委托给 [`FsState::new`]。
    fn default() -> Self {
        Self::new()
    }
}

/// 状态构造器与跨模块访问器。
impl FsState {
    /// 装配默认状态：从进程环境变量读取超时与缓存 TTL，授权存储、
    /// exec 任务表、git-read 缓存、realpath 缓存均为全新实例。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(FsInner {
                grants: OutsideGrantStore::default(),
                exec_jobs: exec::ExecJobStore::default(),
                git_read_cache: Arc::new(exec::GitReadCache::new(exec::git_read_cache_ttl_ms())),
                realpath_cache: RealpathCache::default(),
                command_timeout_ms: exec::command_timeout_ms(),
                git_check_ignore_timeout_ms: exec::git_check_ignore_timeout_ms(),
            }),
        }
    }

    /// Access to the outside-file grant store for cross-module callers that
    /// mint grants after an explicit user file pick (JS:
    /// `mintOutsideFileGrant`).
    ///
    /// 中文说明：返回内部授权存储的引用；跨模块调用方在用户显式选择
    /// 文件后调用其 `mint` 铸造授权，与 `/api/fs/raw` 校验的是同一份存储。
    pub fn grants(&self) -> &OutsideGrantStore {
        &self.inner.grants
    }
}

/// 构建自带新建状态的 `/api/fs` 路由器；需要与兄弟模块共享授权存储时
/// 改用 [`router_with`]。
pub fn router(ctx: RouterContext) -> Router {
    router_with(ctx, FsState::new())
}

/// Composition entry for main.rs: the caller owns the state so sibling
/// modules (markdown-image-grants) mint grants against the SAME store that
/// `/api/fs/raw` serves.
///
/// 中文说明：组合根入口。调用方持有 `state`，使兄弟模块（如
/// markdown-image-grants）铸造的授权与 `/api/fs/raw` 服务的存储是同一份。
/// JSON POST 路由族套用 50mb 请求体上限；upload 路由流式读取 octet-stream
/// 并按 OMPCHAMBER_FS_UPLOAD_MAX_BYTES 自行限流，因此禁用共享上限；
/// 最后统一挂上 ui_auth 模块的 `/api` 鉴权中间件。
pub fn router_with(ctx: RouterContext, state: FsState) -> Router {
    let core = Router::new()
        .route("/api/fs/home", get(handlers::home))
        .route("/api/fs/stat", get(handlers::stat))
        .route("/api/fs/read", get(handlers::read))
        .route("/api/fs/raw", get(handlers::raw))
        .route("/api/fs/serve/{*path}", get(handlers::serve))
        .route("/api/fs/list", get(handlers::list))
        .route("/api/fs/git-dirs", get(handlers::git_dirs))
        .route("/api/fs/exec/{jobId}", get(handlers::exec_job))
        // JSON-body POST family rides the `express.json({ limit: '50mb' })`
        // body budget that registerCommonRequestMiddleware applies to
        // `/api/fs` paths.
        .route("/api/fs/mkdir", post(handlers::mkdir))
        .route("/api/fs/write", post(handlers::write))
        .route("/api/fs/delete", post(handlers::delete))
        .route("/api/fs/rename", post(handlers::rename))
        .route("/api/fs/reveal", post(handlers::reveal))
        .route("/api/fs/clone", post(handlers::clone))
        .route("/api/fs/exec", post(handlers::exec))
        .route_layer(DefaultBodyLimit::max(handlers::JSON_BODY_LIMIT));

    // The upload route streams an octet-stream body and enforces its own
    // byte budget (OMPCHAMBER_FS_UPLOAD_MAX_BYTES), so the shared extractor
    // limit must not truncate it first.
    let upload = Router::new()
        .route("/api/fs/upload", post(handlers::upload))
        .route_layer(DefaultBodyLimit::disable());

    core.merge(upload)
        // `/api` requests pass through `requireApiAuth` before any route
        // (core-routes.js `app.use('/api', requireApiAuth)`); the ported
        // gate is owned by the ui_auth module.
        .route_layer(crate::ui_auth::middleware(ctx))
        .with_state(state)
}

/// 模块级集成测试（路由处理器行为契约，位于 `tests.rs`）。
#[cfg(test)]
mod tests;
