//! Port of `server/lib/linear/client.js` — the GraphQL helper (with Linear's
//! `public-file-urls-expire-in` header), viewer/organization identity lookup,
//! and access-token refresh with one in-flight refresh per workspace.

use std::sync::Arc;

use serde_json::{Value, json};

use super::auth::{LinearOrganization, LinearUser, SetLinearAuthInput, is_access_token_stale};
use super::http::{HttpBody, HttpMethod, HttpRequest};
use super::parse::{is_plain_object, read_trimmed_string};
use super::{LinearError, LinearState};

const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";
const VIEWER_QUERY: &str =
    "{ viewer { id name displayName email avatarUrl } organization { id name urlKey } }";
// Linear file URLs in GraphQL need this header or the browser cannot load
// uploads.linear.app images (comment screenshots, description images).
const LINEAR_PUBLIC_FILE_URL_TTL_SECONDS: &str = "3600";

#[derive(Debug, Clone, Default)]
pub struct Identity {
    pub user: Option<LinearUser>,
    pub organization: Option<LinearOrganization>,
}

/// JS `fetchLinearGraphql`: returns the `data` object or a
/// `LinearApiError`-shaped failure.
pub async fn fetch_linear_graphql(
    state: &Arc<LinearState>,
    access_token: &str,
    query: &str,
    variables: Option<&Value>,
) -> Result<Value, LinearError> {
    let token = access_token.trim();
    if token.is_empty() {
        return Err(LinearError::api("Linear is not connected", 401));
    }

    let mut body = json!({ "query": query });
    if let Some(variables) = variables.filter(|v| is_plain_object(v)) {
        body["variables"] = variables.clone();
    }

    let request = HttpRequest {
        method: HttpMethod::Post,
        url: LINEAR_GRAPHQL_URL.to_string(),
        headers: vec![
            ("Accept".into(), "application/json".into()),
            ("Content-Type".into(), "application/json".into()),
            ("Authorization".into(), format!("Bearer {token}")),
            (
                "public-file-urls-expire-in".into(),
                LINEAR_PUBLIC_FILE_URL_TTL_SECONDS.to_string(),
            ),
        ],
        body: HttpBody::Json(body.to_string()),
    };
    let response = state
        .transport
        .send(request)
        .await
        .map_err(LinearError::plain)?;
    let payload = response.json();
    if response.status == 401 {
        return Err(LinearError::api("Linear token expired or revoked", 401));
    }
    if !response.is_ok() {
        return Err(LinearError::api(
            format!("Linear GraphQL request failed ({})", response.status),
            response.status,
        ));
    }
    let Some(payload) = payload.filter(is_plain_object) else {
        return Err(LinearError::api(
            "Linear GraphQL response was not JSON",
            502,
        ));
    };
    if is_plain_object(&payload["data"]) {
        return Ok(payload["data"].clone());
    }
    let (message, user_error, status) = read_graphql_error(&payload);
    Err(LinearError::api_with_user_flag(
        if message.is_empty() {
            "Linear GraphQL response did not include data".to_string()
        } else {
            message
        },
        status,
        user_error,
    ))
}

/// JS `readGraphqlError`.
fn read_graphql_error(payload: &Value) -> (String, bool, u16) {
    let errors = payload["errors"].as_array();
    let first = errors
        .and_then(|entries| entries.first())
        .filter(|entry| is_plain_object(entry));
    let Some(first) = first else {
        return (String::new(), false, 502);
    };
    let extensions = super::parse::as_plain_object(&first["extensions"]);
    let presentable = extensions
        .map(|e| read_trimmed_string(&e["userPresentableMessage"]))
        .unwrap_or_default();
    let mut constraint = String::new();
    if let Some(validation_errors) = extensions.and_then(|e| e["validationErrors"].as_array()) {
        'outer: for entry in validation_errors {
            let Some(constraints) = entry["constraints"].as_object() else {
                continue;
            };
            for value in constraints.values() {
                let text = read_trimmed_string(value);
                if !text.is_empty() {
                    constraint = text;
                    break 'outer;
                }
            }
        }
    }
    let message = if !presentable.is_empty() {
        presentable
    } else if !constraint.is_empty() {
        constraint
    } else {
        read_trimmed_string(&first["message"])
    };
    let code = extensions
        .map(|e| read_trimmed_string(&e["code"]))
        .unwrap_or_default();
    let extensions_user_error = extensions
        .map(|e| matches!(e["userError"], Value::Bool(true)))
        .unwrap_or(false);
    let user_error = extensions_user_error
        || code == "INVALID_INPUT"
        || code == "INPUT_ERROR"
        || starts_with_ci(&message, "entity not found")
        || starts_with_ci(&message, "argument validation");
    (message, user_error, if user_error { 400 } else { 502 })
}

/// JS `/^prefix/i` regex anchoring.
fn starts_with_ci(haystack: &str, prefix: &str) -> bool {
    haystack.len() >= prefix.len() && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// JS `readIdentity`.
fn read_identity(data: &Value) -> Option<Identity> {
    let viewer = super::parse::as_plain_object(&data["viewer"])?;
    let viewer_id = read_trimmed_string(&viewer["id"]);
    if viewer_id.is_empty() {
        return None;
    }
    let organization = super::parse::as_plain_object(&data["organization"]);
    let organization_id = organization
        .map(|o| read_trimmed_string(&o["id"]))
        .unwrap_or_default();
    let organization_name = organization
        .map(|o| read_trimmed_string(&o["name"]))
        .unwrap_or_default();
    Some(Identity {
        user: Some(LinearUser {
            id: viewer_id,
            name: optional(&viewer["name"]),
            display_name: optional(&viewer["displayName"]),
            email: optional(&viewer["email"]),
            avatar_url: optional(&viewer["avatarUrl"]),
        }),
        organization: (!organization_id.is_empty() && !organization_name.is_empty()).then(|| {
            LinearOrganization {
                id: organization_id,
                name: organization_name,
                url_key: organization.and_then(|o| optional(&o["urlKey"])),
            }
        }),
    })
}

fn optional(value: &Value) -> Option<String> {
    let trimmed = read_trimmed_string(value);
    (!trimmed.is_empty()).then_some(trimmed)
}
/// JS `fetchLinearIdentity`.
pub async fn fetch_linear_identity(
    state: &Arc<LinearState>,
    access_token: &str,
) -> Result<Identity, LinearError> {
    let data = fetch_linear_graphql(state, access_token, VIEWER_QUERY, None).await?;
    read_identity(&data)
        .ok_or_else(|| LinearError::api("Linear GraphQL response did not include a viewer", 502))
}

/// JS `refreshWorkspaceAuth` — refreshes the given (possibly non-current)
/// workspace entry and persists the rotated tokens without activating it.
async fn refresh_workspace_auth(
    state: Arc<LinearState>,
    auth: super::auth::LinearAuthEntry,
) -> Result<String, LinearError> {
    let refresh_token = auth
        .refresh_token
        .clone()
        .ok_or_else(|| LinearError::oauth("refresh_token is required", "MISSING_REFRESH_TOKEN"))?;
    let tokens = state.refresh_access_token(&refresh_token).await?;
    let next = state.set_auth(
        SetLinearAuthInput {
            access_token: tokens.access_token.clone(),
            refresh_token: Some(tokens.refresh_token.clone().or(auth.refresh_token.clone())),
            token_type: Some(tokens.token_type.clone()),
            expires_at: Some(tokens.expires_at),
            scope: Some(if tokens.scope.is_empty() {
                auth.scope.clone()
            } else {
                tokens.scope.clone()
            }),
            user: Some(auth.user.clone()),
            organization: Some(auth.organization.clone()),
            workspace_id: Some(auth.workspace_id.clone()),
        },
        false,
    )?;
    Ok(next.access_token)
}

/// JS `getValidLinearAccessToken`: returns a non-stale token, refreshing (and
/// persisting the rotated refresh token) when needed. Concurrent callers for
/// the same workspace share one in-flight refresh.
pub async fn get_valid_linear_access_token(
    state: &Arc<LinearState>,
    workspace_id: Option<&str>,
) -> Result<Option<String>, LinearError> {
    let id = workspace_id.map(str::trim).filter(|v| !v.is_empty());
    let auth = match id {
        Some(id) => state.get_auth_by_workspace_id(id),
        None => state.get_auth(),
    };
    let Some(auth) = auth else {
        return Ok(None);
    };
    if !is_access_token_stale(auth.expires_at, super::now_ms()) {
        return Ok(Some(auth.access_token));
    }
    let Some(_refresh_token) = auth.refresh_token.clone() else {
        state.clear_auth(Some(&auth.workspace_id));
        return Ok(None);
    };

    let key = auth.workspace_id.clone();
    let refresh = {
        let mut inflight = state
            .refresh_inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match inflight.get(&key) {
            Some(existing) => existing.clone(),
            None => {
                let state_for_refresh = state.clone();
                let auth_for_refresh = auth.clone();
                let key_workspace = key.clone();
                let future = super::shared(Box::pin(async move {
                    match refresh_workspace_auth(state_for_refresh.clone(), auth_for_refresh).await
                    {
                        Ok(token) => Ok(Some(token)),
                        Err(error) => {
                            if error.code.as_deref() == Some("INVALID_GRANT")
                                || error.http_status() == Some(400)
                                || error.http_status() == Some(401)
                            {
                                state_for_refresh.clear_auth(Some(&key_workspace));
                                Ok(None)
                            } else {
                                Err(error)
                            }
                        }
                    }
                }));
                inflight.insert(key.clone(), future.clone());
                future
            }
        }
    };
    let result = refresh.await;
    state
        .refresh_inflight
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    result
}
