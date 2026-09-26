//! Direct GitHub REST client replacing `@octokit/rest` (`octokit.js`).
//!
//! JS precedent (`server/lib/github/octokit.js` + the installed octokit
//! packages): every REST call goes to `https://api.github.com` with
//! `accept: application/vnd.github.v3+json`, `authorization: token <t>`
//! (`bearer` for JWT-shaped tokens) and an 8s per-request timeout; GETs go
//! through a conditional-request ETag cache (304 replayed as 200, cache keyed
//! by `token\nurl`, max 300 entries, LRU touch) so unchanged polls do not
//! count against the REST rate limit.
//!
//! Error semantics mirror `@octokit/request`'s `fetch-wrapper.js` /
//! `request-error`: status >= 400 raises an error whose message is built by
//! `toErrorMessage(data)` (`message: errors…` for validation payloads), with
//! `status`, response headers and parsed body attached. 304 without a cached
//! entry surfaces as "Not modified".
//!
//! The transport is a seam (`Transport`) so tests drive routes against a fake
//! instead of api.github.com.

use std::sync::{Arc, LazyLock, Mutex};

use futures::future::BoxFuture;
use serde_json::Value;

/// `octokit.js` `OCTOKIT_REQUEST_TIMEOUT_MS`.
const REQUEST_TIMEOUT_MS: u64 = 8_000;
/// `octokit.js` `ETAG_CACHE_MAX_ENTRIES`.
const ETAG_CACHE_MAX_ENTRIES: usize = 300;

pub const GITHUB_API_BASE: &str = "https://api.github.com";
const DEFAULT_ACCEPT: &str = "application/vnd.github.v3+json";

/// A single GitHub API request as octokit would issue it.
#[derive(Debug, Clone)]
pub struct GithubRequest {
    pub method: String,
    /// Absolute URL (base + encoded path + query).
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct GithubResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Mirror of octokit's `RequestError`: `status`, `message`,
/// `response.headers`, `response.data`.
#[derive(Debug, Clone)]
pub struct GithubError {
    pub status: Option<u16>,
    pub message: String,
    pub headers: Vec<(String, String)>,
    /// Parsed response body (`response.data`) when it was JSON/text.
    pub data: Option<Value>,
}

impl GithubError {
    /// Case-insensitive response header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }
}

pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

impl GithubResponse {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        header_value(&self.headers, name)
    }

    /// `getResponseData` from octokit's fetch-wrapper: `application/json`
    /// bodies parse as JSON (falling back to the raw string), `text/*` or
    /// `charset=utf-8` bodies decode as text, anything else stays opaque.
    pub fn octokit_data(&self) -> Option<Value> {
        let content_type = self.header("content-type").unwrap_or("");
        let mimetype = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let charset_utf8 = content_type
            .split(';')
            .skip(1)
            .any(|p| p.trim().to_ascii_lowercase().starts_with("charset=utf-8"));
        let text = String::from_utf8_lossy(&self.body).to_string();
        if mimetype == "application/json" || mimetype == "application/scim+json" {
            return Some(serde_json::from_str(&text).unwrap_or(Value::String(text)));
        }
        if mimetype.starts_with("text/") || (charset_utf8 && mimetype != "application/octet-stream")
        {
            return Some(Value::String(text));
        }
        None
    }
}

/// `toErrorMessage` from octokit's fetch-wrapper.
fn to_error_message(data: Option<&Value>) -> String {
    match data {
        None | Some(Value::Null) => "Unknown error".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(v @ Value::Object(obj)) => {
            let message = obj.get("message").and_then(Value::as_str);
            match message {
                Some(message) => {
                    let suffix = obj
                        .get("documentation_url")
                        .and_then(Value::as_str)
                        .map(|url| format!(" - {url}"))
                        .unwrap_or_default();
                    match obj.get("errors").and_then(Value::as_array) {
                        Some(errors) => {
                            let joined = errors
                                .iter()
                                .map(|e| serde_json::to_string(e).unwrap_or_default())
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("{message}: {joined}{suffix}")
                        }
                        None => format!("{message}{suffix}"),
                    }
                }
                None => format!("Unknown error: {v}"),
            }
        }
        Some(other) => format!("Unknown error: {other}"),
    }
}

/// Build a `GithubError` the way octokit does for status >= 400.
fn error_from_response(response: &GithubResponse) -> GithubError {
    let data = response.octokit_data();
    GithubError {
        status: Some(response.status),
        message: to_error_message(data.as_ref()),
        headers: response.headers.clone(),
        data,
    }
}

/// HTTP transport seam. Production: reqwest with the octokit timeout + ETag
/// conditional cache. Tests: canned responses.
pub type Transport = Arc<
    dyn Fn(GithubRequest) -> BoxFuture<'static, Result<GithubResponse, GithubError>> + Send + Sync,
>;

struct EtagEntry {
    etag: String,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
}

/// Process-global ETag cache (`octokit.js` `etagCache`), keyed
/// `token\nurl`, insertion-ordered with LRU touch, capped at 300 entries.
static ETAG_CACHE: LazyLock<Mutex<Vec<(String, EtagEntry)>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

fn etag_cache_get(key: &str) -> Option<EtagEntry> {
    ETAG_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, e)| EtagEntry {
            etag: e.etag.clone(),
            body: e.body.clone(),
            headers: e.headers.clone(),
        })
}

fn etag_cache_remember(key: String, entry: EtagEntry) {
    let mut cache = ETAG_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|(k, _)| k != &key);
    cache.push((key, entry));
    if cache.len() > ETAG_CACHE_MAX_ENTRIES {
        cache.remove(0);
    }
}

/// AbortSignal.timeout semantics: octokit rethrows the abort as a 500 error.
fn abort_error() -> GithubError {
    GithubError {
        status: Some(500),
        message: "The operation was aborted".to_string(),
        headers: Vec::new(),
        data: None,
    }
}

fn transport_error(message: String) -> GithubError {
    GithubError {
        status: Some(500),
        message,
        headers: Vec::new(),
        data: None,
    }
}

fn collect_headers(response: &reqwest::Response) -> Vec<(String, String)> {
    response
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Production transport: reqwest + 8s timeout + ETag conditional requests
/// (`octokit.js` `createConditionalFetch(token)`).
pub fn default_transport(token: &str) -> Transport {
    let token = token.to_string();
    Arc::new(move |request: GithubRequest| {
        let token = token.clone();
        Box::pin(async move {
            let is_get = request.method.eq_ignore_ascii_case("GET");
            let cache_key = format!("{token}\n{}", request.url);
            let cached = if is_get {
                etag_cache_get(&cache_key)
            } else {
                None
            };

            let mut headers = request.headers.clone();
            if let Some(cached) = cached.as_ref() {
                headers.push(("if-none-match".to_string(), cached.etag.clone()));
            }

            let method = match reqwest::Method::from_bytes(request.method.as_bytes()) {
                Ok(m) => m,
                Err(e) => return Err(transport_error(e.to_string())),
            };
            let url = match reqwest::Url::parse(&request.url) {
                Ok(u) => u,
                Err(e) => return Err(transport_error(e.to_string())),
            };
            let mut req = token_client().request(method, url);
            for (name, value) in &headers {
                req = req.header(name, value);
            }
            if let Some(body) = request.body.clone() {
                req = req.body(body);
            }

            let send = async {
                match req.send().await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let headers = collect_headers(&resp);
                        let body = resp
                            .bytes()
                            .await
                            .map_err(|e| transport_error(e.to_string()))?;
                        Ok(GithubResponse {
                            status,
                            headers,
                            body: body.to_vec(),
                        })
                    }
                    Err(e) => Err(transport_error(e.to_string())),
                }
            };
            let response = match tokio::time::timeout(
                std::time::Duration::from_millis(REQUEST_TIMEOUT_MS),
                send,
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => return Err(abort_error()),
            };

            if response.status == 304 {
                if let Some(cached) = cached {
                    let EtagEntry {
                        etag,
                        body,
                        headers,
                    } = cached;
                    etag_cache_remember(
                        cache_key,
                        EtagEntry {
                            etag: etag.clone(),
                            body: body.clone(),
                            headers: headers.clone(),
                        },
                    );
                    return Ok(GithubResponse {
                        status: 200,
                        headers,
                        body,
                    });
                }
                let data = response.octokit_data();
                return Err(GithubError {
                    status: Some(304),
                    message: "Not modified".to_string(),
                    headers: response.headers,
                    data,
                });
            }

            if response.status == 204 || response.status == 205 {
                return Ok(GithubResponse {
                    status: response.status,
                    headers: response.headers,
                    body: Vec::new(),
                });
            }

            if (400..600).contains(&response.status) {
                return Err(error_from_response(&response));
            }

            if is_get
                && response.status < 400
                && let Some(etag) = response.header("etag").map(str::to_string)
            {
                etag_cache_remember(
                    cache_key,
                    EtagEntry {
                        etag,
                        body: response.body.clone(),
                        headers: response.headers.clone(),
                    },
                );
            }

            Ok(response)
        })
    })
}

fn token_client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(reqwest::Client::new);
    &CLIENT
}

/// `withAuthorizationPrefix`: JWTs use `bearer`, everything else `token`.
pub fn authorization_header(token: &str) -> String {
    if token.split('.').count() == 3 {
        format!("bearer {token}")
    } else {
        format!("token {token}")
    }
}

/// URL-template `{var}` expansion (`encodeUnreserved`): encodeURIComponent
/// plus escaping `!'()*`.
pub fn encode_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// JS `encodeURIComponent`: leaves `A-Za-z0-9-_.!~*'()` untouched.
pub fn encode_query_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `addQueryParameters`' `q` special case: split on `+`, encode each part,
/// rejoin with `+` (so `c++` survives and spaces become `%20`).
pub fn encode_search_query(q: &str) -> String {
    q.split('+')
        .map(encode_query_component)
        .collect::<Vec<_>>()
        .join("+")
}

/// The GitHub API client (`createOctokit(token)` equivalent).
#[derive(Clone)]
pub struct GithubClient {
    transport: Transport,
    /// `authorization` header value octokit's auth-token plugin would set.
    authorization: String,
}

impl GithubClient {
    pub fn new(token: &str) -> Self {
        Self {
            transport: default_transport(token),
            authorization: authorization_header(token),
        }
    }

    pub fn with_transport(transport: Transport) -> Self {
        // Fake transports do not authenticate; mirror the empty-token shape.
        Self {
            transport,
            authorization: String::new(),
        }
    }

    fn build_request(
        &self,
        method: &str,
        path: &str,
        query: Vec<(String, String)>,
        body: Option<Value>,
        accept: &str,
    ) -> GithubRequest {
        let mut url = format!("{GITHUB_API_BASE}{path}");
        if !query.is_empty() {
            let qs = query
                .iter()
                .map(|(k, v)| format!("{k}={}", encode_query_component(v)))
                .collect::<Vec<_>>()
                .join("&");
            url.push('?');
            url.push_str(&qs);
        }
        let mut request = GithubRequest {
            method: method.to_string(),
            url,
            headers: vec![
                ("accept".to_string(), accept.to_string()),
                (
                    "user-agent".to_string(),
                    "octokit-rest.js/rust-port".to_string(),
                ),
                ("authorization".to_string(), self.authorization.clone()),
            ],
            body: None,
        };
        if let Some(body) = body {
            request.body = Some(serde_json::to_vec(&body).unwrap_or_default());
            request.headers.push((
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ));
        }
        request
    }

    /// GET with default accept, returning parsed data.
    async fn get_json(
        &self,
        path: &str,
        query: Vec<(String, String)>,
    ) -> Result<Value, GithubError> {
        let req = self.build_request("GET", path, query, None, DEFAULT_ACCEPT);
        let resp = (self.transport)(req).await?;
        Ok(resp.octokit_data().unwrap_or(Value::Null))
    }

    /// GET returning parsed data plus response headers (`link` drives
    /// has-more checks in routes).
    async fn get_json_with_headers(
        &self,
        path: &str,
        query: Vec<(String, String)>,
    ) -> Result<(Value, Vec<(String, String)>), GithubError> {
        let req = self.build_request("GET", path, query, None, DEFAULT_ACCEPT);
        let resp = (self.transport)(req).await?;
        Ok((resp.octokit_data().unwrap_or(Value::Null), resp.headers))
    }

    async fn send_body(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, GithubError> {
        let req = self.build_request(method, path, Vec::new(), body, DEFAULT_ACCEPT);
        let resp = (self.transport)(req).await?;
        Ok(resp.octokit_data().unwrap_or(Value::Null))
    }

    // ---- users ----

    pub async fn users_get_authenticated(&self) -> Result<Value, GithubError> {
        self.get_json("/user", Vec::new()).await
    }

    pub async fn users_list_emails(&self) -> Result<Value, GithubError> {
        self.get_json(
            "/user/emails",
            vec![("per_page".to_string(), "100".to_string())],
        )
        .await
    }

    // ---- repos ----

    pub async fn repos_get(&self, owner: &str, repo: &str) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json(&path, Vec::new()).await
    }

    pub async fn repos_get_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/branches/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(branch)
        );
        self.get_json(&path, Vec::new()).await
    }

    pub async fn repos_list_branches(
        &self,
        owner: &str,
        repo: &str,
        page: u32,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/branches",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json(
            &path,
            vec![
                ("per_page".to_string(), "100".to_string()),
                ("page".to_string(), page.to_string()),
            ],
        )
        .await
    }

    pub async fn repos_combined_status(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/commits/{}/status",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(git_ref)
        );
        self.get_json(&path, Vec::new()).await
    }

    pub async fn repos_collaborator_permission(
        &self,
        owner: &str,
        repo: &str,
        username: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/collaborators/{}/permission",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(username)
        );
        self.get_json(&path, Vec::new()).await
    }

    pub async fn git_get_ref(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/git/ref/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(git_ref)
        );
        self.get_json(&path, Vec::new()).await
    }

    // ---- pulls ----

    /// `pulls.list`. Returns parsed data plus headers.
    pub async fn pulls_list(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
        head: Option<&str>,
        per_page: u32,
        page: Option<u32>,
    ) -> Result<(Value, Vec<(String, String)>), GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        let mut query = vec![
            ("state".to_string(), state.to_string()),
            ("per_page".to_string(), per_page.to_string()),
        ];
        if let Some(head) = head {
            query.push(("head".to_string(), head.to_string()));
        }
        if let Some(page) = page {
            query.push(("page".to_string(), page.to_string()));
        }
        self.get_json_with_headers(&path, query).await
    }

    pub async fn pulls_get(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, Vec::new()).await
    }

    pub async fn pulls_create(
        &self,
        owner: &str,
        repo: &str,
        payload: &Value,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.send_body("POST", &path, Some(payload.clone())).await
    }

    pub async fn pulls_update(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
        payload: &Value,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.send_body("PATCH", &path, Some(payload.clone())).await
    }

    pub async fn pulls_merge(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
        merge_method: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}/merge",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.send_body(
            "PUT",
            &path,
            Some(serde_json::json!({ "merge_method": merge_method })),
        )
        .await
    }

    pub async fn pulls_list_files(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}/files",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

    pub async fn pulls_list_review_comments(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}/comments",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

    /// `GET /repos/{owner}/{repo}/pulls/{n}` with
    /// `accept: application/vnd.github.v3.diff` — returns the diff text.
    pub async fn pulls_get_diff(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Option<String>, GithubError> {
        let path = format!(
            "/repos/{}/{}/pulls/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        let req = self.build_request(
            "GET",
            &path,
            Vec::new(),
            None,
            "application/vnd.github.v3.diff",
        );
        let resp = (self.transport)(req).await?;
        if resp.status >= 400 {
            return Err(error_from_response(&resp));
        }
        Ok(Some(String::from_utf8_lossy(&resp.body).to_string()))
    }

    // ---- checks / actions ----

    pub async fn checks_list_for_ref(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/commits/{}/check-runs",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(git_ref)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

    pub async fn checks_list_annotations(
        &self,
        owner: &str,
        repo: &str,
        check_run_id: i64,
        page: u32,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/check-runs/{check_run_id}/annotations",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json(
            &path,
            vec![
                ("per_page".to_string(), "50".to_string()),
                ("page".to_string(), page.to_string()),
            ],
        )
        .await
    }

    pub async fn actions_list_jobs(
        &self,
        owner: &str,
        repo: &str,
        run_id: i64,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/actions/runs/{run_id}/jobs",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

    // ---- issues ----

    pub async fn issues_list_for_repo(
        &self,
        owner: &str,
        repo: &str,
        state: &str,
        per_page: u32,
        page: u32,
    ) -> Result<(Value, Vec<(String, String)>), GithubError> {
        let path = format!(
            "/repos/{}/{}/issues",
            encode_path_segment(owner),
            encode_path_segment(repo)
        );
        self.get_json_with_headers(
            &path,
            vec![
                ("state".to_string(), state.to_string()),
                ("per_page".to_string(), per_page.to_string()),
                ("page".to_string(), page.to_string()),
            ],
        )
        .await
    }

    pub async fn issues_get(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/issues/{}",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, Vec::new()).await
    }

    pub async fn issues_list_comments(
        &self,
        owner: &str,
        repo: &str,
        number: &str,
    ) -> Result<Value, GithubError> {
        let path = format!(
            "/repos/{}/{}/issues/{}/comments",
            encode_path_segment(owner),
            encode_path_segment(repo),
            encode_path_segment(number)
        );
        self.get_json(&path, vec![("per_page".to_string(), "100".to_string())])
            .await
    }

    // ---- search ----

    pub async fn search_issues(
        &self,
        q: &str,
        per_page: u32,
        page: u32,
    ) -> Result<Value, GithubError> {
        // `q` gets the split-on-`+` encoding; other params are plain encoded.
        let url = format!(
            "{GITHUB_API_BASE}/search/issues?q={}&per_page={per_page}&page={page}",
            encode_search_query(q)
        );
        let req = GithubRequest {
            method: "GET".to_string(),
            url,
            headers: vec![
                ("accept".to_string(), DEFAULT_ACCEPT.to_string()),
                (
                    "user-agent".to_string(),
                    "octokit-rest.js/rust-port".to_string(),
                ),
                ("authorization".to_string(), self.authorization.clone()),
            ],
            body: None,
        };
        let resp = (self.transport)(req).await?;
        Ok(resp.octokit_data().unwrap_or(Value::Null))
    }

    // ---- graphql ----

    pub async fn graphql(&self, query: &str, variables: Value) -> Result<Value, GithubError> {
        let payload = serde_json::json!({ "query": query, "variables": variables });
        let req = self.build_request(
            "POST",
            "/graphql",
            Vec::new(),
            Some(payload),
            DEFAULT_ACCEPT,
        );
        let resp = (self.transport)(req).await?;
        if resp.status >= 400 {
            return Err(error_from_response(&resp));
        }
        let data = resp.octokit_data().unwrap_or(Value::Null);
        // octokit graphql throws on body-level errors; carry the HTTP status so
        // the 403 branch in pr/ready behaves like octokit's RequestError.
        if let Some(errors) = data.get("errors").and_then(Value::as_array) {
            let message = errors
                .first()
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Graphql error")
                .to_string();
            return Err(GithubError {
                status: Some(resp.status),
                message,
                headers: resp.headers,
                data: Some(data),
            });
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_response(status: u16, body: Value) -> GithubResponse {
        GithubResponse {
            status,
            headers: vec![(
                "content-type".to_string(),
                "application/json; charset=utf-8".to_string(),
            )],
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    fn fake(
        responder: impl Fn(&GithubRequest) -> Result<GithubResponse, GithubError>
        + Send
        + Sync
        + 'static,
    ) -> GithubClient {
        GithubClient::with_transport(Arc::new(move |req: GithubRequest| {
            let result = responder(&req);
            Box::pin(async move { result })
        }))
    }

    #[tokio::test]
    async fn error_message_mirrors_octokit_validation_failed_shape() {
        let data = serde_json::json!({
            "message": "Validation Failed",
            "errors": [{ "resource": "PullRequest", "code": "invalid", "field": "head" }]
        });
        let response = json_response(422, data);
        let error = error_from_response(&response);
        assert_eq!(error.status, Some(422));
        // routes.js checks these exact substrings on error.message.
        assert!(error.message.contains("Validation Failed"));
        assert!(error.message.contains("\"field\":\"head\""));
        assert!(error.message.contains("\"code\":\"invalid\""));
    }

    #[tokio::test]
    async fn error_message_uses_documentation_url_suffix() {
        let data = serde_json::json!({
            "message": "Not Found",
            "documentation_url": "https://docs.github.com"
        });
        let response = json_response(404, data);
        let error = error_from_response(&response);
        assert_eq!(error.message, "Not Found - https://docs.github.com");
    }

    #[test]
    fn authorization_prefix_matches_octokit() {
        assert_eq!(authorization_header("abc"), "token abc");
        assert_eq!(authorization_header("a.b.c"), "bearer a.b.c");
    }

    #[test]
    fn path_and_query_encoding_match_octokit() {
        assert_eq!(encode_path_segment("feat/x"), "feat%2Fx");
        assert_eq!(encode_path_segment("a b"), "a%20b");
        assert_eq!(encode_query_component("a b"), "a%20b");
        // encodeURIComponent escapes literal "+"; only the q-splitting preserves it
        assert_eq!(encode_query_component("c++"), "c%2B%2B");
        // q: spaces become %20, literal + survives
        assert_eq!(
            encode_search_query("repo:o/r fix type:issue"),
            "repo%3Ao%2Fr%20fix%20type%3Aissue"
        );
        assert_eq!(encode_search_query("c++ lang"), "c++%20lang");
    }

    #[tokio::test]
    async fn pulls_list_builds_octokit_url_shape() {
        let client = fake(|req| {
            assert_eq!(req.method, "GET");
            assert_eq!(
                req.url,
                "https://api.github.com/repos/acme/app/pulls?state=open&per_page=100&head=acme%3Afeature"
            );
            assert_eq!(
                req.headers
                    .iter()
                    .find(|(k, _)| k == "accept")
                    .map(|(_, v)| v.as_str()),
                Some("application/vnd.github.v3+json")
            );
            Ok(json_response(200, serde_json::json!([])))
        });
        let (data, _) = client
            .pulls_list("acme", "app", "open", Some("acme:feature"), 100, None)
            .await
            .unwrap();
        assert_eq!(data, serde_json::json!([]));
    }

    #[tokio::test]
    async fn search_issues_encodes_q_with_plus_splitting() {
        let client = fake(|req| {
            assert_eq!(
                req.url,
                "https://api.github.com/search/issues?q=repo%3Aacme%2Fapp%20fix%20type%3Aissue&per_page=50&page=2"
            );
            Ok(json_response(
                200,
                serde_json::json!({ "total_count": 0, "items": [] }),
            ))
        });
        let data = client
            .search_issues("repo:acme/app fix type:issue", 50, 2)
            .await
            .unwrap();
        assert_eq!(data["total_count"], 0);
    }

    #[tokio::test]
    async fn graphql_posts_query_and_variables() {
        let client = fake(|req| {
            assert_eq!(req.method, "POST");
            assert_eq!(req.url, "https://api.github.com/graphql");
            let body: Value = serde_json::from_slice(req.body.as_deref().unwrap()).unwrap();
            assert!(
                body["query"]
                    .as_str()
                    .unwrap()
                    .contains("markPullRequestReadyForReview")
            );
            assert_eq!(body["variables"]["pullRequestId"], "node-1");
            Ok(json_response(200, serde_json::json!({ "data": {} })))
        });
        client
            .graphql(
                "mutation { markPullRequestReadyForReview }",
                serde_json::json!({ "pullRequestId": "node-1" }),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn non_json_success_body_parses_as_text() {
        let client = fake(|_| {
            Ok(GithubResponse {
                status: 200,
                headers: vec![(
                    "content-type".to_string(),
                    "application/vnd.github.v3.diff; charset=utf-8".to_string(),
                )],
                body: b"diff --git a/x".to_vec(),
            })
        });
        let diff = client.pulls_get_diff("o", "r", "1").await.unwrap();
        assert_eq!(diff.as_deref(), Some("diff --git a/x"));
    }
}
