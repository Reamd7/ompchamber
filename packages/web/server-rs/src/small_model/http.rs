//! HTTP transport seam for the small-model module.
//!
//! JS precedent: `call.js`, `runtime-providers.js` and `models-metadata.js`
//! all issue requests through the global `fetch` (per-request
//! `AbortSignal.timeout`), which the tests replace with a mock. The Rust port
//! funnels every outbound request through one [`Fetch`] closure so tests can
//! drive the wire formats against a fake, and the production implementation
//! wraps a rustls reqwest client with the same per-request deadline semantics
//! (`requestSignal`).

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

/// One outbound request, mirroring the `fetch(url, init)` argument shape the
/// JS builds per wire format.
#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub method: String,
    pub url: String,
    /// Sent in order; production lowercases names into the header map, tests
    /// observe them verbatim.
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// `AbortSignal.timeout` equivalent; `requestSignal` resolves the 60s
    /// default when unset.
    pub timeout_ms: u64,
}

#[derive(Debug, Clone)]
pub struct FetchResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl FetchResponse {
    /// Case-insensitive response header lookup (`response.headers.get`).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

pub type FetchFuture = BoxFuture<'static, Result<FetchResponse, String>>;
pub type Fetch = Arc<dyn Fn(FetchRequest) -> FetchFuture + Send + Sync>;

/// Production transport: rustls reqwest client with a hard per-request
/// deadline (`tokio::time::timeout`), mirroring `AbortSignal.timeout`.
pub fn reqwest_fetch() -> Fetch {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    Arc::new(move |request: FetchRequest| {
        let client = client.clone();
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|error| error.to_string())?;
            let mut builder = client
                .request(method, &request.url)
                .timeout(Duration::from_millis(request.timeout_ms.max(1)));
            for (name, value) in &request.headers {
                builder = builder.header(name.as_str(), value.as_str());
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let response = builder.send().await.map_err(|error| error.to_string())?;
            let status = response.status().as_u16();
            let mut headers = Vec::new();
            for (name, value) in response.headers() {
                if let Ok(value) = value.to_str() {
                    headers.push((name.as_str().to_string(), value.to_string()));
                }
            }
            let body = response.bytes().await.map_err(|error| error.to_string())?;
            Ok(FetchResponse {
                status,
                headers,
                body: body.to_vec(),
            })
        })
    })
}

/// `mergeHeadersCaseInsensitive` (call.js): overrides replace same-name
/// (case-insensitive) base entries while keeping their own casing/order.
pub fn merge_headers_case_insensitive(
    base: Vec<(String, String)>,
    overrides: Option<&[(String, String)]>,
) -> Vec<(String, String)> {
    let mut merged = base;
    let Some(overrides) = overrides else {
        return merged;
    };
    for (name, value) in overrides {
        merged.retain(|(existing, _)| !existing.eq_ignore_ascii_case(name));
        merged.push((name.clone(), value.clone()));
    }
    merged
}

/// Plain object spread of headers (`{...base, ...extra}` in call.js — exact
/// name replacement only, duplicates otherwise ride along).
pub fn spread_headers(
    base: Vec<(String, String)>,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    let mut merged = base;
    for (name, value) in extra {
        merged.retain(|(existing, _)| existing != name);
        merged.push((name.clone(), value.clone()));
    }
    merged
}

// ---------------------------------------------------------------------------
// JS string-semantics helpers (UTF-16 code-unit lengths / slices)
// ---------------------------------------------------------------------------

/// JS `String.prototype.length` (UTF-16 code units).
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// JS `String.prototype.slice(0, n)`: prefix of at most `n` UTF-16 code units.
/// Stops before a character that would split a surrogate pair.
pub fn utf16_prefix(text: &str, n: usize) -> &str {
    let mut taken = 0usize;
    for (index, character) in text.char_indices() {
        taken += character.len_utf16();
        if taken > n {
            return &text[..index];
        }
    }
    text
}

/// JS `encodeURIComponent` (production ALURL set: unreserved + `!'()*-._~`).
pub fn uri_encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(byte as char),
            b'-' | b'_' | b'.' | b'~' | b'!' | b'*' | b'(' | b')' | b'\'' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `Number(value) > 0 ? Number(value) : …` coercion for JSON body fields:
/// numbers and numeric strings coerce; anything else reads as NaN (None).
pub fn js_positive_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Millisecond clock shared by the cache TTLs and the OAuth expiry math.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
