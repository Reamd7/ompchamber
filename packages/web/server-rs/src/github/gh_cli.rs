//! Port of `server/lib/github/gh-cli-credential.js` — `gh auth token`
//! invocation with a 30s cache (including negative results) and a 5s
//! subprocess timeout.
//!
//! 中文说明：通过 `gh auth token` 子进程读取 GitHub CLI 已登录的
//! OAuth token，作为 GitHub 认证的兜底来源。结果（包括"没有 token"
//! 的负结果）缓存 30 秒，避免频繁启动子进程；子进程有 5 秒硬超时，
//! 防止 `gh` 挂起阻塞调用方。

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// token 缓存有效期（毫秒）：正结果与负结果同样缓存，TTL 内不重复探测。
const CACHE_TTL_MS: u128 = 30_000;
/// `gh auth token` 子进程的硬超时（对应 JS 版的 timeout: 5000）。
const GH_CLI_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Where the token comes from; a seam so tests never spawn `gh`.
///
/// token 获取器 seam：同步返回 token 或 `None`。抽象为可注入的闭包，
/// 测试传入假实现即可避免真实启动 `gh` 子进程。
pub type TokenFetcher = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// `execFileSync('gh', ['auth', 'token'], { stdio: pipes, timeout: 5000,
/// windowsHide: true })` — trimmed stdout or None on any failure/timeout.
///
/// 在独立线程中执行 `gh auth token`（stdin 关闭、stdout/stderr 走管道、
/// Windows 下不弹控制台窗口），主线程经 mpsc 通道带 5 秒超时等待结果。
/// 非零退出码、超时或输出为空白均返回 `None`；成功时返回 trim 后的
/// stdout。
fn fetch_via_process() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut command = std::process::Command::new("gh");
        command
            .arg("auth")
            .arg("token")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // windowsHide: no console window on Windows.
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let output = command.output().ok();
        let _ = tx.send(output);
    });
    let output = rx.recv_timeout(GH_CLI_TIMEOUT).ok()??;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// `cachedToken`/`cachedAt`/`hasCachedToken` state.
///
/// `gh` CLI 凭证管理器：持有 (token 或 None, 抓取时刻) 的缓存与可替换
/// 的 fetcher，对应 JS 版模块级的 cachedToken / cachedAt /
/// hasCachedToken 三个变量。
pub struct GhCliCredential {
    /// 当前缓存：Some((token 或 None, 抓取时刻))；外层 None 表示尚未缓存。
    cache: Mutex<Option<(Option<String>, Instant)>>,
    /// token 获取器；生产环境绑定 fetch_via_process，测试注入假实现。
    fetcher: TokenFetcher,
}
/// 进程级全局单例，绑定真实的 `gh` 子进程 fetcher。
pub static GLOBAL_GH_CLI: LazyLock<Arc<GhCliCredential>> =
    LazyLock::new(|| Arc::new(GhCliCredential::with_fetcher(Arc::new(fetch_via_process))));

/// 全局实例访问、自定义 fetcher 构造，以及带 TTL 的 token 读取与缓存清理。
impl GhCliCredential {
    /// 取进程级全局实例（GLOBAL_GH_CLI 的便捷入口）。
    pub fn global() -> Arc<Self> {
        Arc::clone(&GLOBAL_GH_CLI)
    }

    /// 用自定义 fetcher 构造实例（测试注入用），缓存初始为空。
    pub fn with_fetcher(fetcher: TokenFetcher) -> Self {
        Self {
            cache: Mutex::new(None),
            fetcher,
        }
    }

    /// `getGhCliToken`: cached value within TTL, else refetch.
    ///
    /// 对应 JS 版 `getGhCliToken`：缓存未过期（30 秒 TTL 内）直接返回
    /// 缓存值（可能是 None 的负结果）；否则调用 fetcher 重新获取，结果
    /// trim 后空串视为 None，连同当前时刻一起写回缓存。
    pub fn token(&self) -> Option<String> {
        let now = Instant::now();
        {
            let cached = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((token, fetched_at)) = cached.as_ref()
                && now.duration_since(*fetched_at).as_millis() < CACHE_TTL_MS
            {
                return token.clone();
            }
        }
        let token = (self.fetcher)()
            .map(|raw| raw.trim().to_string())
            .filter(|t| !t.is_empty());
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((token.clone(), Instant::now()));
        token
    }

    /// `clearGhCliTokenCache`.
    ///
    /// 对应 JS 版 `clearGhCliTokenCache`：清空缓存，下一次 `token()`
    /// 将重新调用 fetcher。
    pub fn clear(&self) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 测试辅助：窥探当前缓存值，Some(None) 表示缓存了"无 token"。
    #[cfg(test)]
    fn cached_value(&self) -> Option<Option<String>> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(t, _)| t.clone())
    }
}

/// 验证 GhCliCredential 的 token trim、TTL 缓存（含负结果）与清缓存行为。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 验证 token() 对 fetcher 输出做 trim 并透传非空结果。
    #[test]
    fn token_is_trimmed_and_returned() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_fetch = calls.clone();
        let credential = GhCliCredential::with_fetcher(Arc::new(move || {
            calls_for_fetch.fetch_add(1, Ordering::SeqCst);
            Some("  token\n".to_string())
        }));
        assert_eq!(credential.token(), Some("token".to_string()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// 验证"无 token"的负结果也被缓存，clear() 之后才会重新探测。
    #[test]
    fn caches_unavailable_result_until_cleared() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_fetch = calls.clone();
        let credential = GhCliCredential::with_fetcher(Arc::new(move || {
            calls_for_fetch.fetch_add(1, Ordering::SeqCst);
            None
        }));
        assert!(credential.token().is_none());
        assert!(credential.token().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(credential.cached_value(), Some(None));

        credential.clear();
        assert!(credential.token().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// 验证 TTL 内多次调用 token() 只触发一次 fetcher。
    #[test]
    fn token_is_cached_across_calls_within_ttl() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_fetch = calls.clone();
        let credential = GhCliCredential::with_fetcher(Arc::new(move || {
            calls_for_fetch.fetch_add(1, Ordering::SeqCst);
            Some("tok".to_string())
        }));
        assert_eq!(credential.token(), Some("tok".to_string()));
        assert_eq!(credential.token(), Some("tok".to_string()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// 验证空白输出被视为无 token，而不是空字符串。
    #[test]
    fn empty_output_is_treated_as_no_token() {
        let credential = GhCliCredential::with_fetcher(Arc::new(|| Some("\n".to_string())));
        assert!(credential.token().is_none());
    }
}
