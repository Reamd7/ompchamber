//! HTTP transport seam for the small-model module.
//!
//! JS precedent: `call.js`, `runtime-providers.js` and `models-metadata.js`
//! all issue requests through the global `fetch` (per-request
//! `AbortSignal.timeout`), which the tests replace with a mock. The Rust port
//! funnels every outbound request through one [`Fetch`] closure so tests can
//! drive the wire formats against a fake, and the production implementation
//! wraps a rustls reqwest client with the same per-request deadline semantics
//! (`requestSignal`).
//!
//! 中文说明：small-model 的 HTTP 通道缝（seam）。所有出站请求统一经一个可注入的
//! `Fetch` 闭包发出：生产实现是 rustls reqwest client（保持逐请求超时语义），
//! 测试换成 fake 直接校验线格式。

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

/// One outbound request, mirroring the `fetch(url, init)` argument shape the
/// JS builds per wire format.
///
/// 中文补充：与 JS `fetch(url, init)` 参数一一对应的出站请求描述。
#[derive(Debug, Clone)]
pub struct FetchRequest {
    /// HTTP 方法名。
    pub method: String,
    /// 完整请求 URL。
    pub url: String,
    /// Sent in order; production lowercases names into the header map, tests
    /// observe them verbatim.
    pub headers: Vec<(String, String)>,
    /// 请求体字节；None 表示无 body。
    pub body: Option<Vec<u8>>,
    /// `AbortSignal.timeout` equivalent; `requestSignal` resolves the 60s
    /// default when unset.
    pub timeout_ms: u64,
}

/// 出站请求的响应：状态码、原始响应头（保留大小写、可重复）与响应体字节。
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// HTTP 状态码（304 等协商结果原样保留）。
    pub status: u16,
    /// 响应头名值对列表。
    pub headers: Vec<(String, String)>,
    /// 响应体字节。
    pub body: Vec<u8>,
}

/// 响应读取的便捷方法。
impl FetchResponse {
    /// Case-insensitive response header lookup (`response.headers.get`).
    ///
    /// 中文补充：按名称大小写不敏感地取第一个匹配的响应头。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 以 UTF-8 有损解码响应体（对应 JS 的 response.text()）。
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
}

/// 单次出站请求的 boxed 异步结果：成功为响应，失败为错误消息字符串。
pub type FetchFuture = BoxFuture<'static, Result<FetchResponse, String>>;
/// 可注入的出站请求通道；生产实现见 reqwest_fetch，测试替换为 fake 闭包。
pub type Fetch = Arc<dyn Fn(FetchRequest) -> FetchFuture + Send + Sync>;

/// Production transport: rustls reqwest client with a hard per-request
/// deadline (`tokio::time::timeout`), mirroring `AbortSignal.timeout`.
///
/// 中文补充：连接超时 10s，整体按请求的 timeout_ms 硬截止（等价 AbortSignal.timeout）；
/// 任何 IO/解析错误都折算成字符串错误返回。
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
///
/// 中文补充：override 逐个以大小写不敏感的方式顶掉 base 中的同名项，自身大小写与
/// 顺序保持不变。
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
///
/// 中文补充：等价 JS 对象展开——只有名字完全相同才被顶掉，其余重复项原样保留。
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
///
/// 中文补充：按 UTF-16 码元计长（BMP 外字符记 2）。
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// JS `String.prototype.slice(0, n)`: prefix of at most `n` UTF-16 code units.
/// Stops before a character that would split a surrogate pair.
///
/// 中文补充：取不超过 n 个 UTF-16 码元的前缀；若边界会劈开代理对，则停在完整字符前。
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
///
/// 中文补充：按生产 ALURL 字符集转义（unreserved 加 !'()*-._~），其余字节转为
/// 大写十六进制的百分号编码。
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
///
/// 中文补充：数字与数字字符串可折算为正数；其余（null、布尔、非数字串）按 NaN
/// 处理返回 None。
pub fn js_positive_number(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Millisecond clock shared by the cache TTLs and the OAuth expiry math.
///
/// 中文补充：Unix 纪元以来的毫秒数；时钟早于纪元时返回 0。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
