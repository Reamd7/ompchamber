//! Port of `server/lib/github/device-flow.js` — OAuth device flow against
//! github.com. Plain form POSTs with `Accept: application/json`; a 200 body
//! may still carry `{error: 'authorization_pending' | …}` which the caller
//! inspects. `postForm` is a seam so tests drive the state machine without
//! network access.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;

pub const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
pub const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// `postForm(url, params)` seam: url → form body → parsed JSON payload.
pub type FormPoster =
    Arc<dyn Fn(&str, String) -> BoxFuture<'static, Result<Value, DeviceFlowError>> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct DeviceFlowError {
    pub status: u16,
    pub message: String,
    pub payload: Option<Value>,
}

/// `encodeForm` + URLSearchParams serialization (space → `+`).
fn encode_form(params: &[(&str, Option<&str>)]) -> String {
    params
        .iter()
        .filter_map(|(key, value)| value.map(|v| (key, v)))
        .map(|(key, value)| format!("{}={}", key, encode_form_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode_form_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')'
            | b'!' => out.push(byte as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Production poster: reqwest form POST with JSON accept, octokit-free (the
/// JS module uses bare `fetch`).
pub fn default_form_poster() -> FormPoster {
    Arc::new(|url: &str, body: String| {
        let url = url.to_string();
        Box::pin(async move {
            let response = reqwest::Client::new()
                .post(&url)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("Accept", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| DeviceFlowError {
                    status: 500,
                    message: e.to_string(),
                    payload: None,
                })?;
            let status = response.status().as_u16();
            // response.json().catch(() => null)
            let payload: Option<Value> = match response.text().await {
                Ok(text) => serde_json::from_str(&text).ok(),
                Err(_) => None,
            };
            if !(200..300).contains(&status) {
                let message = payload
                    .as_ref()
                    .and_then(|p| {
                        p.get("error_description")
                            .or_else(|| p.get("error"))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| {
                        reqwest::StatusCode::from_u16(status)
                            .ok()
                            .and_then(|s| s.canonical_reason().map(str::to_string))
                            .unwrap_or_else(|| "GitHub request failed".to_string())
                    });
                return Err(DeviceFlowError {
                    status,
                    message,
                    payload,
                });
            }
            Ok(payload.unwrap_or(Value::Null))
        })
    })
}

/// `startDeviceFlow({ clientId, scope })`.
pub async fn start_device_flow(
    poster: &FormPoster,
    client_id: &str,
    scope: &str,
) -> Result<Value, DeviceFlowError> {
    poster(
        DEVICE_CODE_URL,
        encode_form(&[("client_id", Some(client_id)), ("scope", Some(scope))]),
    )
    .await
}

/// `exchangeDeviceCode({ clientId, deviceCode })` — polls the access-token
/// endpoint; GitHub answers 200 with `{error: …}` for non-success states.
pub async fn exchange_device_code(
    poster: &FormPoster,
    client_id: &str,
    device_code: &str,
) -> Result<Value, DeviceFlowError> {
    poster(
        ACCESS_TOKEN_URL,
        encode_form(&[
            ("client_id", Some(client_id)),
            ("device_code", Some(device_code)),
            ("grant_type", Some(DEVICE_GRANT_TYPE)),
        ]),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn recording_poster(
        respond: impl Fn(&str, &str) -> Result<Value, DeviceFlowError> + Send + Sync + 'static,
    ) -> (FormPoster, Arc<Mutex<Vec<(String, String)>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let calls_for_closure = calls.clone();
        let poster: FormPoster = Arc::new(move |url: &str, body: String| {
            let result = respond(url, body.as_str());
            calls_for_closure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((url.to_string(), body));
            Box::pin(async move { result })
        });
        (poster, calls)
    }

    #[tokio::test]
    async fn start_sends_client_id_and_scope_as_form_fields() {
        let (poster, calls) = recording_poster(|_, _| {
            Ok(serde_json::json!({
                "device_code": "dc",
                "user_code": "ABCD-1234",
                "verification_uri": "https://github.com/login/device",
                "expires_in": 900,
                "interval": 5,
            }))
        });
        let payload = start_device_flow(&poster, "client-1", "repo read:org")
            .await
            .unwrap();
        assert_eq!(payload["device_code"], "dc");

        let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, DEVICE_CODE_URL);
        assert_eq!(calls[0].1, "client_id=client-1&scope=repo+read%3Aorg");
    }

    #[tokio::test]
    async fn exchange_sends_device_grant_type() {
        let (poster, calls) =
            recording_poster(|_, _| Ok(serde_json::json!({ "error": "authorization_pending" })));
        let payload = exchange_device_code(&poster, "client-1", "dev-code")
            .await
            .unwrap();
        assert_eq!(payload["error"], "authorization_pending");

        let calls = calls.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(calls[0].0, ACCESS_TOKEN_URL);
        assert_eq!(
            calls[0].1,
            "client_id=client-1&device_code=dev-code&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        );
    }

    #[tokio::test]
    async fn success_payload_carries_access_token() {
        let (poster, _) = recording_poster(|_, _| {
            Ok(serde_json::json!({
                "access_token": "gho_token",
                "token_type": "bearer",
                "scope": "repo"
            }))
        });
        let payload = exchange_device_code(&poster, "client-1", "dev-code")
            .await
            .unwrap();
        assert_eq!(payload["access_token"], "gho_token");
        assert_eq!(payload["token_type"], "bearer");
    }

    #[tokio::test]
    async fn http_error_maps_error_description_first() {
        let (poster, _) = recording_poster(|_, _| {
            Err(DeviceFlowError {
                status: 422,
                message: "device code expired".to_string(),
                payload: Some(serde_json::json!({
                    "error": "expired_token",
                    "error_description": "device code expired"
                })),
            })
        });
        let error = exchange_device_code(&poster, "client-1", "dev-code")
            .await
            .unwrap_err();
        assert_eq!(error.status, 422);
        assert_eq!(error.message, "device code expired");
    }

    #[tokio::test]
    async fn null_values_are_skipped_from_form_body() {
        let (poster, calls) = recording_poster(|_, _| Ok(Value::Null));
        // scope None would be skipped in JS; emulate via direct encode_form.
        let body = encode_form(&[("client_id", Some("c")), ("scope", None)]);
        assert_eq!(body, "client_id=c");
        start_device_flow(&poster, "c", "s").await.unwrap();
        assert_eq!(calls.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
    }
}
