//! Port of `server/lib/tts/base-url.js` — validation and normalization of
//! custom OpenAI-compatible base URLs shared by the TTS speak route, the STT
//! transcription proxy, and the dictation OpenAI-compatible session.
//!
//! Security posture is preserved: remote hosts are rejected unless the
//! desktop runtime is active or `OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS`
//! explicitly opts in; query strings, fragments, and credentials are stripped.

use url::Url;

const LOCAL_BASE_URL_HOSTS: [&str; 4] = ["localhost", "127.0.0.1", "::1", "host.docker.internal"];

fn is_env_flag_enabled(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let normalized = value.trim().to_ascii_lowercase();
    normalized == "1" || normalized == "true"
}

/// `isAllowedLocalHost`: the hostname (brackets stripped for IPv6 literals,
/// lowercased) must be one of the loopback-equivalent hosts.
fn is_allowed_local_host(hostname: &str) -> bool {
    let trimmed = hostname.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return false;
    }
    let unbracketed = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(&trimmed);
    LOCAL_BASE_URL_HOSTS.contains(&unbracketed)
}

/// `normalizeCustomOpenAIBaseURL` with the two env reads hoisted into
/// arguments so the decision table stays testable without global state.
///
/// Returns `Err(message)` for rejected URLs, `Ok(None)` for absent/blank
/// input (the JS `{ value: undefined }`), and `Ok(Some(url))` for the
/// normalized `scheme//host[:port]path` form.
pub fn normalize_custom_openai_base_url_with(
    value: Option<&str>,
    runtime_env: Option<&str>,
    allow_remote_env: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let mut parsed = match Url::parse(trimmed) {
        Ok(url) => url,
        Err(_) => return Err("Custom server URL is invalid".to_string()),
    };

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err("Custom server URL must use http or https".to_string());
    }

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("Custom server URL must not include credentials".to_string());
    }

    let is_desktop = runtime_env.is_some_and(|value| value.trim().eq_ignore_ascii_case("desktop"));
    let has_explicit_flag = allow_remote_env.is_some_and(|value| !value.trim().is_empty());
    let allow_remote = if has_explicit_flag {
        is_env_flag_enabled(allow_remote_env)
    } else {
        is_desktop
    };
    // `Url::host_str` is already lowercased and unbracketed for IPv6.
    let hostname = parsed.host_str().unwrap_or_default().to_string();
    if !allow_remote && !is_allowed_local_host(&hostname) {
        return Err(
            "Remote custom server URLs are disabled. Set OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS=true to allow this host."
                .to_string(),
        );
    }

    parsed.set_fragment(None);
    parsed.set_query(None);
    // JS `parsed.pathname.replace(/\/+$/, '')`: strip ALL trailing slashes,
    // then keep the (possibly empty) remainder.
    let pathname = parsed.path();
    let normalized_path = pathname.trim_end_matches('/');
    // WHATWG `url.host`: lowercased host plus an explicit port. The url
    // crate's `host_str` serializes IPv6 hosts bracketed and elides
    // scheme-default ports, matching the WHATWG `parsed.host` the JS
    // interpolates.
    let host = match parsed.port() {
        Some(port) => format!("{hostname}:{port}"),
        None => hostname.clone(),
    };
    Ok(Some(format!(
        "{}://{host}{normalized_path}",
        parsed.scheme()
    )))
}

/// Production entrypoint reading the two env vars at call time (the JS reads
/// `process.env` on every call).
pub fn normalize_custom_openai_base_url(value: Option<&str>) -> Result<Option<String>, String> {
    normalize_custom_openai_base_url_with(
        value,
        std::env::var("OMPCHAMBER_RUNTIME").ok().as_deref(),
        std::env::var("OMPCHAMBER_ALLOW_REMOTE_OPENAI_COMPAT_URLS")
            .ok()
            .as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const REMOTE: &str = "https://my-tts-server.example.com/v1";

    #[test]
    fn rejects_remote_urls_when_runtime_is_not_set() {
        let result = normalize_custom_openai_base_url_with(Some(REMOTE), None, None);
        let Err(message) = result else {
            panic!("expected rejection");
        };
        assert!(message.starts_with("Remote custom server URLs are disabled"));
    }

    #[test]
    fn allows_remote_urls_when_runtime_is_desktop() {
        let result = normalize_custom_openai_base_url_with(Some(REMOTE), Some("desktop"), None);
        assert_eq!(result.unwrap().as_deref(), Some(REMOTE));
    }

    #[test]
    fn allows_remote_urls_when_env_flag_is_true() {
        let result = normalize_custom_openai_base_url_with(Some(REMOTE), None, Some("true"));
        assert_eq!(result.unwrap().as_deref(), Some(REMOTE));
    }

    #[test]
    fn denies_remote_urls_on_desktop_when_env_flag_is_false() {
        let result =
            normalize_custom_openai_base_url_with(Some(REMOTE), Some("desktop"), Some("false"));
        assert!(result.unwrap_err().starts_with("Remote custom server URLs"));
    }

    #[test]
    fn allows_localhost_urls_regardless_of_runtime() {
        let result =
            normalize_custom_openai_base_url_with(Some("http://localhost:8880/v1"), None, None);
        assert_eq!(result.unwrap().as_deref(), Some("http://localhost:8880/v1"));
    }

    #[test]
    fn allows_ipv6_loopback() {
        let result =
            normalize_custom_openai_base_url_with(Some("http://[::1]:8880/v1"), None, None);
        assert_eq!(result.unwrap().as_deref(), Some("http://[::1]:8880/v1"));
    }

    #[test]
    fn strips_query_strings_and_trailing_slashes() {
        let result = normalize_custom_openai_base_url_with(
            Some("https://my-server.com/v1/?key=123"),
            Some("desktop"),
            None,
        );
        assert_eq!(result.unwrap().as_deref(), Some("https://my-server.com/v1"));
    }

    #[test]
    fn strips_all_trailing_slashes_and_root_path() {
        let root =
            normalize_custom_openai_base_url_with(Some("http://localhost:8880///"), None, None);
        assert_eq!(root.unwrap().as_deref(), Some("http://localhost:8880"));
    }

    #[test]
    fn rejects_invalid_urls_and_non_http_schemes() {
        assert_eq!(
            normalize_custom_openai_base_url_with(Some("my-server.com/v1"), None, None)
                .unwrap_err(),
            "Custom server URL is invalid"
        );
        assert_eq!(
            normalize_custom_openai_base_url_with(Some("ftp://localhost/v1"), None, None)
                .unwrap_err(),
            "Custom server URL must use http or https"
        );
    }

    #[test]
    fn rejects_embedded_credentials() {
        assert_eq!(
            normalize_custom_openai_base_url_with(
                Some("http://user:pw@localhost:8880/v1"),
                None,
                None
            )
            .unwrap_err(),
            "Custom server URL must not include credentials"
        );
    }

    #[test]
    fn blank_and_missing_values_map_to_none() {
        assert!(
            normalize_custom_openai_base_url_with(None, None, None)
                .unwrap()
                .is_none()
        );
        assert!(
            normalize_custom_openai_base_url_with(Some("   "), None, None)
                .unwrap()
                .is_none()
        );
    }
}
