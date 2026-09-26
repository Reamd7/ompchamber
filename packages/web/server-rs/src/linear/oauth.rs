//! Port of `server/lib/linear/oauth.js` — authorization-code + PKCE S256,
//! the public callback-broker handoff (register/poll/acknowledge), token
//! refresh, and revocation. Pending authorizations live in module state
//! (JS module-level `Map`s) keyed by OAuth state, pruned by a 10 minute TTL.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::auth::SetLinearAuthInput;
use super::http::{HttpBody, HttpMethod, HttpRequest};
use super::parse::{is_plain_object, is_string, read_finite_number, read_trimmed_string};
use super::{LinearError, LinearState};

pub const LINEAR_AUTHORIZE_URL: &str = "https://linear.app/oauth/authorize";
pub const LINEAR_TOKEN_URL: &str = "https://api.linear.app/oauth/token";
pub const LINEAR_REVOKE_URL: &str = "https://api.linear.app/oauth/revoke";
pub const PENDING_AUTHORIZATION_TTL_MS: f64 = 10.0 * 60_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOrigin {
    Web,
    Desktop,
}

impl AuthOrigin {
    fn parse(value: &str) -> Self {
        if value == "desktop" {
            AuthOrigin::Desktop
        } else {
            AuthOrigin::Web
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingAuthorization {
    pub code_verifier: String,
    pub redirect_uri: String,
    pub origin: AuthOrigin,
    pub broker: Option<BrokerInfo>,
    pub expires_at: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BrokerInfo {
    pub url: String,
    pub claim_secret: String,
}

#[derive(Debug, Clone)]
pub struct TokenBundle {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_at: f64,
    pub scope: String,
}

#[derive(Debug, Clone)]
pub struct AuthorizationResult {
    pub tokens: TokenBundle,
    pub origin: AuthOrigin,
    pub broker_receipt: Option<BrokerReceipt>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BrokerReceipt {
    pub state: String,
    pub url: String,
    pub claim_secret: String,
}

#[derive(Debug, Clone, Default)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
    pub error: String,
    pub error_description: String,
}

/// JS `createPkcePair`.
pub fn create_pkce_pair() -> (String, String) {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

fn random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

impl LinearState {
    fn prune_expired_pending(&self, now: f64) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.retain(|_, entry| entry.expires_at > now);
    }

    /// JS `startAuthorization`.
    pub async fn start_authorization(self: &Arc<Self>, origin: &str) -> Result<Value, LinearError> {
        let client_id = self.get_client_id();
        if client_id.is_empty() {
            return Err(LinearError::oauth(
                "Linear OAuth client not configured. Set OPENCHAMBER_LINEAR_CLIENT_ID.",
                "LINEAR_CLIENT_ID_MISSING",
            ));
        }

        let now = super::now_ms();
        self.prune_expired_pending(now);
        let (verifier, challenge) = create_pkce_pair();
        let state = random_token();
        let broker_url = self.get_broker_url();
        let configured_redirect_uri = self.get_redirect_uri();
        let uses_broker = configured_redirect_uri == broker_callback_url(&broker_url);
        let claim_secret = uses_broker.then(random_token);
        let redirect_uri = if uses_broker {
            let claim = claim_secret.clone().unwrap_or_default();
            register_broker_transaction(self, &broker_url, &state, &claim).await?
        } else {
            configured_redirect_uri
        };
        let scope = self.get_scopes();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.insert(
                state.clone(),
                PendingAuthorization {
                    code_verifier: verifier,
                    redirect_uri: redirect_uri.clone(),
                    origin: AuthOrigin::parse(origin),
                    broker: uses_broker.then(|| BrokerInfo {
                        url: broker_url,
                        claim_secret: claim_secret.unwrap_or_default(),
                    }),
                    expires_at: now + PENDING_AUTHORIZATION_TTL_MS,
                },
            );
        }

        let mut url =
            url::Url::parse(LINEAR_AUTHORIZE_URL).map_err(|e| LinearError::plain(e.to_string()))?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.append_pair("response_type", "code");
            pairs.append_pair("client_id", &client_id);
            pairs.append_pair("redirect_uri", &redirect_uri);
            pairs.append_pair("scope", &scope);
            pairs.append_pair("state", &state);
            pairs.append_pair("code_challenge", &challenge);
            pairs.append_pair("code_challenge_method", "S256");
            pairs.append_pair("actor", "user");
            pairs.append_pair("prompt", "consent");
        }
        Ok(json!({
            "authorizationUrl": url.to_string(),
            "expiresIn": (PENDING_AUTHORIZATION_TTL_MS / 1000.0).floor() as i64,
            "scope": scope,
        }))
    }

    /// JS `consumeAuthorizationCallback`.
    pub async fn consume_authorization_callback(
        self: &Arc<Self>,
        query: &CallbackQuery,
    ) -> Result<AuthorizationResult, LinearError> {
        let now = super::now_ms();
        self.prune_expired_pending(now);
        let state = read_trimmed_string(&Value::String(query.state.clone()));
        let pending = if !state.is_empty() {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&state)
                .cloned()
        } else {
            None
        };

        let error = read_trimmed_string(&Value::String(query.error.clone()));
        if !error.is_empty() {
            if !state.is_empty() {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&state);
            }
            return Err(LinearError::oauth_with_origin(
                if query.error_description.trim().is_empty() {
                    error.clone()
                } else {
                    query.error_description.trim().to_string()
                },
                &error.to_uppercase(),
                pending.map(|p| p.origin),
            ));
        }
        let code = read_trimmed_string(&Value::String(query.code.clone()));
        if code.is_empty() {
            if !state.is_empty() {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&state);
            }
            return Err(LinearError::oauth_with_origin(
                "Linear did not return an authorization code.",
                "MISSING_CODE",
                pending.map(|p| p.origin),
            ));
        }
        let Some(pending) = pending.filter(|p| !p.code_verifier.is_empty()) else {
            return Err(LinearError::oauth(
                "This authorization session has expired or is unknown to the running app. Return to OpenChamber and click Connect again.",
                "UNKNOWN_STATE",
            ));
        };

        let mut body: Vec<(String, String)> = vec![
            ("grant_type".into(), "authorization_code".into()),
            ("code".into(), code.clone()),
            ("redirect_uri".into(), pending.redirect_uri.clone()),
            ("client_id".into(), self.get_client_id()),
            ("code_verifier".into(), pending.code_verifier.clone()),
        ];
        if let Some(secret) = self.get_client_secret() {
            body.push(("client_secret".into(), secret));
        }

        let result = post_form(self, LINEAR_TOKEN_URL, &body).await;
        match result {
            Ok(tokens) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&state);
                Ok(AuthorizationResult {
                    tokens,
                    origin: pending.origin,
                    broker_receipt: None,
                })
            }
            Err(mut error) => {
                error.origin = Some(pending.origin);
                Err(error)
            }
        }
    }

    /// JS `pollAuthorizationBroker`: polls every pending broker authorization
    /// (deduplicated per state) and returns the first completed hand-off. A
    /// rejected poll aborts the loop, exactly like the JS `await` chain.
    pub async fn poll_authorization_broker(
        self: &Arc<Self>,
    ) -> Result<Option<AuthorizationResult>, LinearError> {
        self.prune_expired_pending(super::now_ms());
        let states: Vec<String> = {
            let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending
                .iter()
                .filter(|(_, entry)| entry.broker.is_some())
                .map(|(state, _)| state.clone())
                .collect()
        };
        for state in states {
            let poll = {
                let mut polls = self.broker_polls.lock().unwrap_or_else(|e| e.into_inner());
                match polls.get(&state) {
                    Some(existing) => existing.clone(),
                    None => {
                        let future =
                            super::shared(Box::pin(poll_broker_state(self.clone(), state.clone())));
                        polls.insert(state.clone(), future.clone());
                        future
                    }
                }
            };
            let result = poll.await?;
            self.broker_polls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&state);
            if result.is_some() {
                return Ok(result);
            }
        }
        Ok(None)
    }

    /// JS `completeAuthorizationBroker`.
    pub async fn complete_authorization_broker(
        self: &Arc<Self>,
        receipt: &BrokerReceipt,
    ) -> Result<bool, LinearError> {
        if receipt.url.is_empty() || receipt.state.is_empty() || receipt.claim_secret.is_empty() {
            return Ok(false);
        }
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: format!("{}/complete", receipt.url),
            headers: vec![
                ("Accept".into(), "application/json".into()),
                ("Content-Type".into(), "application/json".into()),
            ],
            body: HttpBody::Json(
                json!({ "state": receipt.state, "claimSecret": receipt.claim_secret }).to_string(),
            ),
        };
        let response = self
            .transport
            .send(request)
            .await
            .map_err(LinearError::plain)?;
        if !response.is_ok() {
            return Err(LinearError::oauth_with_status(
                format!(
                    "Could not acknowledge Linear authorization result ({})",
                    response.status
                ),
                "LINEAR_BROKER_FAILED",
                Some(response.status),
            ));
        }
        Ok(true)
    }

    /// JS `refreshAccessToken`.
    pub async fn refresh_access_token(
        self: &Arc<Self>,
        refresh_token: &str,
    ) -> Result<TokenBundle, LinearError> {
        let token = refresh_token.trim();
        if token.is_empty() {
            return Err(LinearError::oauth(
                "refresh_token is required",
                "MISSING_REFRESH_TOKEN",
            ));
        }
        let mut body: Vec<(String, String)> = vec![
            ("grant_type".into(), "refresh_token".into()),
            ("refresh_token".into(), token.to_string()),
            ("client_id".into(), self.get_client_id()),
        ];
        if let Some(secret) = self.get_client_secret() {
            body.push(("client_secret".into(), secret));
        }
        post_form(self, LINEAR_TOKEN_URL, &body).await
    }

    /// JS `revokeToken`.
    pub async fn revoke_token(self: &Arc<Self>, token: &str, token_type_hint: &str) -> bool {
        let value = token.trim();
        if value.is_empty() {
            return false;
        }
        let mut body: Vec<(String, String)> = vec![("token".into(), value.to_string())];
        if token_type_hint == "access_token" || token_type_hint == "refresh_token" {
            body.push(("token_type_hint".into(), token_type_hint.to_string()));
        }
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: LINEAR_REVOKE_URL.to_string(),
            headers: vec![(
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            )],
            body: HttpBody::Form(encode_form(&body)),
        };
        match self.transport.send(request).await {
            Ok(response) => response.status == 200,
            Err(_) => false,
        }
    }
}

/// JS `brokerCallbackUrl`.
pub fn broker_callback_url(broker_url: &str) -> String {
    format!("{}/callback", broker_url.trim_end_matches('/'))
}

fn encode_form(body: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in body {
        serializer.append_pair(key, value);
    }
    serializer.finish()
}

async fn register_broker_transaction(
    state: &Arc<LinearState>,
    broker_url: &str,
    oauth_state: &str,
    claim_secret: &str,
) -> Result<String, LinearError> {
    let request = HttpRequest {
        method: HttpMethod::Post,
        url: format!("{broker_url}/start"),
        headers: vec![
            ("Accept".into(), "application/json".into()),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: HttpBody::Json(
            json!({ "state": oauth_state, "claimSecret": claim_secret }).to_string(),
        ),
    };
    let response = state
        .transport
        .send(request)
        .await
        .map_err(LinearError::plain)?;
    let payload = read_json_response(response, "Could not start Linear authorization broker")?;
    let redirect_uri = read_trimmed_string(&payload["redirectUri"]);
    if redirect_uri.is_empty() || redirect_uri != broker_callback_url(broker_url) {
        return Err(LinearError::oauth(
            "Linear authorization broker returned an unexpected callback URL",
            "LINEAR_BROKER_FAILED",
        ));
    }
    Ok(redirect_uri)
}

/// JS `readJsonResponse` for broker endpoints: non-OK failures surface
/// `payload.error` (or a fallback with the status) as `LINEAR_BROKER_FAILED`.
fn read_json_response(
    response: super::http::HttpResponse,
    fallback_message: &str,
) -> Result<Value, LinearError> {
    let payload = response.json();
    if !response.is_ok() {
        let message = payload
            .as_ref()
            .filter(|p| is_plain_object(p))
            .map(|p| read_trimmed_string(&p["error"]))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| format!("{fallback_message} ({})", response.status));
        return Err(LinearError::oauth_with_status(
            message,
            "LINEAR_BROKER_FAILED",
            Some(response.status),
        ));
    }
    match payload {
        Some(payload) if is_plain_object(&payload) => Ok(payload),
        _ => Err(LinearError::oauth(
            format!("{fallback_message}: invalid response"),
            "LINEAR_BROKER_FAILED",
        )),
    }
}

async fn poll_broker_state(
    state: Arc<LinearState>,
    oauth_state: String,
) -> Result<Option<AuthorizationResult>, LinearError> {
    let pending = {
        let pending = state.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.get(&oauth_state).cloned()
    };
    // JS: `if (!pending?.broker) continue;` — an entry without broker info is
    // skipped by the caller before the future is created; a vanished entry
    // resolves to "still pending".
    let broker = match pending.and_then(|entry| entry.broker) {
        Some(broker) => broker,
        None => return Ok(None),
    };
    let request = HttpRequest {
        method: HttpMethod::Post,
        url: format!("{}/poll", broker.url),
        headers: vec![
            ("Accept".into(), "application/json".into()),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: HttpBody::Json(
            json!({ "state": oauth_state, "claimSecret": broker.claim_secret }).to_string(),
        ),
    };
    let response = state
        .transport
        .send(request)
        .await
        .map_err(LinearError::plain)?;
    if response.status == 202 {
        return Ok(None);
    }
    let payload = read_json_response(response, "Could not read Linear authorization result")?;
    let status = read_trimmed_string(&payload["status"]);
    if status == "complete" {
        let query = CallbackQuery {
            code: read_trimmed_string(&payload["code"]),
            state: oauth_state.clone(),
            ..CallbackQuery::default()
        };
        let mut result = state.consume_authorization_callback(&query).await?;
        result.broker_receipt = Some(BrokerReceipt {
            state: oauth_state,
            url: broker.url,
            claim_secret: broker.claim_secret,
        });
        Ok(Some(result))
    } else if status == "failed" {
        let query = CallbackQuery {
            state: oauth_state,
            error: read_trimmed_string(&payload["error"]),
            error_description: read_trimmed_string(&payload["errorDescription"]),
            ..CallbackQuery::default()
        };
        let result = state.consume_authorization_callback(&query).await?;
        Ok(Some(result))
    } else {
        Err(LinearError::oauth(
            "Linear authorization broker returned an unexpected result",
            "LINEAR_BROKER_FAILED",
        ))
    }
}

/// JS `postForm`.
async fn post_form(
    state: &Arc<LinearState>,
    url: &str,
    body: &[(String, String)],
) -> Result<TokenBundle, LinearError> {
    let request = HttpRequest {
        method: HttpMethod::Post,
        url: url.to_string(),
        headers: vec![
            ("Accept".into(), "application/json".into()),
            (
                "Content-Type".into(),
                "application/x-www-form-urlencoded".into(),
            ),
        ],
        body: HttpBody::Form(encode_form(body)),
    };
    let response = state
        .transport
        .send(request)
        .await
        .map_err(LinearError::plain)?;
    let payload = response.json();
    if !response.is_ok() {
        let description = payload
            .as_ref()
            .filter(|p| is_plain_object(p))
            .map(|p| {
                if read_trimmed_string(&p["error_description"]).is_empty() {
                    read_trimmed_string(&p["error"])
                } else {
                    read_trimmed_string(&p["error_description"])
                }
            })
            .filter(|m| !m.is_empty())
            .unwrap_or_default();
        let code = payload
            .as_ref()
            .filter(|p| is_plain_object(p))
            .map(|p| read_trimmed_string(&p["error"]).to_uppercase())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| "LINEAR_OAUTH_FAILED".to_string());
        return Err(LinearError::oauth_with_status(
            if description.is_empty() {
                format!("Linear token request failed ({})", response.status)
            } else {
                description
            },
            &code,
            Some(response.status),
        ));
    }
    parse_token_payload(payload.as_ref())
}

/// JS `parseTokenPayload`.
fn parse_token_payload(payload: Option<&Value>) -> Result<TokenBundle, LinearError> {
    let Some(payload) = payload.filter(|p| is_plain_object(p)) else {
        return Err(LinearError::oauth(
            "Linear token response was empty",
            "LINEAR_OAUTH_FAILED",
        ));
    };
    let error = read_trimmed_string(&payload["error"]);
    if !error.is_empty() {
        let description = read_trimmed_string(&payload["error_description"]);
        return Err(LinearError::oauth(
            if description.is_empty() {
                error.clone()
            } else {
                description
            },
            &error.to_uppercase(),
        ));
    }
    let access_token = read_trimmed_string(&payload["access_token"]);
    if access_token.is_empty() {
        return Err(LinearError::oauth(
            "Linear token response was missing access_token",
            "LINEAR_OAUTH_FAILED",
        ));
    }
    Ok(TokenBundle {
        access_token,
        refresh_token: optional_trimmed(&payload["refresh_token"]),
        token_type: optional_trimmed(&payload["token_type"]).unwrap_or_else(|| "bearer".into()),
        expires_at: read_expires_at(&payload["expires_in"], super::now_ms()),
        scope: normalize_scope(&payload["scope"]),
    })
}

fn optional_trimmed(value: &Value) -> Option<String> {
    let trimmed = read_trimmed_string(value);
    (!trimmed.is_empty()).then_some(trimmed)
}

/// JS `normalizeScope`.
fn normalize_scope(scope: &Value) -> String {
    match scope {
        Value::String(value) => value.trim().to_string(),
        Value::Array(items) => items
            .iter()
            .filter(|item| is_string(item))
            .map(|item| item.as_str().unwrap_or_default().trim().to_string())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    }
}

/// JS `readExpiresAt`.
fn read_expires_at(expires_in: &Value, now: f64) -> f64 {
    match read_finite_number(expires_in) {
        Some(seconds) if seconds > 0.0 => now + seconds.floor() * 1000.0,
        _ => now + 24.0 * 60.0 * 60.0 * 1000.0,
    }
}

/// Convert an OAuth result into the `setLinearAuth` input the routes use
/// (JS `storeAuthorizationResult` spreads the token fields).
pub fn result_to_set_input(result: &AuthorizationResult) -> SetLinearAuthInput {
    SetLinearAuthInput {
        access_token: result.tokens.access_token.clone(),
        refresh_token: Some(result.tokens.refresh_token.clone()),
        token_type: Some(result.tokens.token_type.clone()),
        expires_at: Some(result.tokens.expires_at),
        scope: Some(result.tokens.scope.clone()),
        user: None,
        organization: None,
        workspace_id: None,
    }
}
