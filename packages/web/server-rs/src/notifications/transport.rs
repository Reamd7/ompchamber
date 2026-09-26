//! Shared injectable HTTP POST seam for the push and APNs runtimes (the JS
//! modules call global `fetch` / `http2`; tests inject fakes the same way
//! the JS tests stub globals).

use std::pin::Pin;
use std::sync::Arc;

/// Response facts the runtimes branch on: status code + body text.
pub struct HttpPostResponse {
    pub status: u16,
    pub body: String,
}

pub type HttpPostFuture = Pin<Box<dyn Future<Output = Result<HttpPostResponse, String>> + Send>>;

/// `(url, headers, body) -> response`. Transport-level failures surface as
/// `Err(reason)` (the JS `fetch` throw / request error paths).
pub type HttpPost =
    Arc<dyn Fn(&str, Vec<(String, String)>, Vec<u8>) -> HttpPostFuture + Send + Sync>;

/// Production transport over the shared reqwest client (rustls + HTTP/2,
/// so APNs direct mode negotiates h2 with Apple).
pub fn reqwest_post(client: reqwest::Client) -> HttpPost {
    Arc::new(
        move |url: &str, headers: Vec<(String, String)>, body: Vec<u8>| {
            let mut request = client.post(url).body(body);
            for (name, value) in headers {
                request = request.header(name, value);
            }
            Box::pin(async move {
                let response = request.send().await.map_err(|e| e.to_string())?;
                let status = response.status().as_u16();
                let body = response.text().await.map_err(|e| e.to_string())?;
                Ok(HttpPostResponse { status, body })
            })
        },
    )
}
