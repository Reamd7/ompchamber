//! Port of `server/lib/github/gh-cli-credential.js` — `gh auth token`
//! invocation with a 30s cache (including negative results) and a 5s
//! subprocess timeout.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

const CACHE_TTL_MS: u128 = 30_000;
const GH_CLI_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Where the token comes from; a seam so tests never spawn `gh`.
pub type TokenFetcher = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// `execFileSync('gh', ['auth', 'token'], { stdio: pipes, timeout: 5000,
/// windowsHide: true })` — trimmed stdout or None on any failure/timeout.
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
pub struct GhCliCredential {
    cache: Mutex<Option<(Option<String>, Instant)>>,
    fetcher: TokenFetcher,
}
pub static GLOBAL_GH_CLI: LazyLock<Arc<GhCliCredential>> =
    LazyLock::new(|| Arc::new(GhCliCredential::with_fetcher(Arc::new(fetch_via_process))));

impl GhCliCredential {
    pub fn global() -> Arc<Self> {
        Arc::clone(&GLOBAL_GH_CLI)
    }

    pub fn with_fetcher(fetcher: TokenFetcher) -> Self {
        Self {
            cache: Mutex::new(None),
            fetcher,
        }
    }

    /// `getGhCliToken`: cached value within TTL, else refetch.
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
    pub fn clear(&self) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    #[cfg(test)]
    fn cached_value(&self) -> Option<Option<String>> {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|(t, _)| t.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[test]
    fn empty_output_is_treated_as_no_token() {
        let credential = GhCliCredential::with_fetcher(Arc::new(|| Some("\n".to_string())));
        assert!(credential.token().is_none());
    }
}
