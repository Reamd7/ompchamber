//! Port of `server/lib/linear/status.js` — session status comments on Linear
//! issues, deduplicated per OpenChamber session id. Posts nothing unless the
//! user opted in and the session origin is publicly reachable; the dedupe file
//! keeps the newest 500 sessions (insertion order, like the JS object keys).

use std::sync::Arc;

use serde_json::{Value, json};

use super::issues::create_linear_issue_comment;
use super::parse::{is_plain_object, read_trimmed_string};
use super::{LinearError, LinearState};

const LINEAR_SESSION_STATUS_KINDS: [&str; 3] = ["started", "completed", "failure"];
pub const MAX_SESSION_STATUS_RECORDS: usize = 500;

const PRIVATE_HOST_SUFFIXES: [&str; 5] =
    [".local", ".localhost", ".internal", ".lan", ".home.arpa"];

/// JS `readSessionOrigin`: http(s) origins without path/query/fragment or
/// credentials, normalized to `origin`.
pub fn read_session_origin(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Ok(url) = url::Url::parse(trimmed) else {
        return String::new();
    };
    if !matches!(url.scheme(), "http" | "https") {
        return String::new();
    }
    if !url.username().is_empty() || url.password().is_some() {
        return String::new();
    }
    if url.query().is_some() || url.fragment().is_some() {
        return String::new();
    }
    let path = url.path();
    if !path.is_empty() && path != "/" {
        return String::new();
    }
    url.origin().ascii_serialization()
}

/// JS `isPrivateIpv4`.
fn is_private_ipv4(hostname: &str) -> bool {
    let parts: Vec<&str> = hostname.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let octets: Vec<i64> = parts
        .iter()
        .map(|part| {
            if part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
                -1
            } else {
                part.parse::<i64>().unwrap_or(-1)
            }
        })
        .collect();
    if octets.iter().any(|octet| *octet < 0 || *octet > 255) {
        return false;
    }
    let (a, b) = (octets[0], octets[1]);
    if a == 0 || a == 10 || a == 127 {
        return true;
    }
    if a == 169 && b == 254 {
        return true;
    }
    if a == 172 && (16..=31).contains(&b) {
        return true;
    }
    if a == 192 && b == 168 {
        return true;
    }
    // 100.64.0.0/10 is carrier-grade NAT, which Tailscale and similar
    // overlays use.
    a == 100 && (64..=127).contains(&b)
}

/// JS `isPrivateIpv6`.
fn is_private_ipv6(hostname: &str) -> bool {
    let address = hostname
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_lowercase();
    if address == "::1" || address == "::" {
        return true;
    }
    // fc00::/7 (unique local) and fe80::/10 (link local).
    let bytes = address.as_bytes();
    bytes.starts_with(b"fc")
        || bytes.starts_with(b"fd")
        || bytes.starts_with(b"fe8")
        || bytes.starts_with(b"fe9")
        || bytes.starts_with(b"fea")
        || bytes.starts_with(b"feb")
}
/// A session link is only worth writing into Linear when somebody other than
/// the person who started the session can open it. Loopback, private LAN and
/// overlay-network addresses reach nobody else, so they do not qualify.
pub fn is_public_session_origin(value: &str) -> bool {
    let origin = read_session_origin(value);
    if origin.is_empty() {
        return false;
    }
    let Ok(url) = url::Url::parse(&origin) else {
        return false;
    };
    let Some(hostname) = url.host_str().map(str::to_lowercase) else {
        return false;
    };
    if hostname.is_empty() || hostname == "localhost" {
        return false;
    }
    if PRIVATE_HOST_SUFFIXES
        .iter()
        .any(|suffix| hostname.ends_with(suffix))
    {
        return false;
    }
    if hostname.contains(':') {
        return !is_private_ipv6(&hostname);
    }
    if !hostname.is_empty() && hostname.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return !is_private_ipv4(&hostname);
    }
    // A bare single-label host is a LAN machine name, not a routable address.
    hostname.contains('.')
}

/// JS `buildLinearSessionOpenUrl`.
pub fn build_linear_session_open_url(session_id: &str, session_origin: &str) -> String {
    let id = session_id.trim();
    let origin = read_session_origin(session_origin);
    if origin.is_empty() {
        return String::new();
    }
    format!("{origin}/?session={}", urlencode(id))
}

/// JS `encodeURIComponent`.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
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
            | b')' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn status_word(kind: &str) -> &'static str {
    match kind {
        "started" => "started",
        "completed" => "completed",
        _ => "failed",
    }
}

/// JS `buildLinearSessionStatusComment`.
pub fn build_linear_session_status_comment(kind: &str, session_url: &str) -> String {
    let url = session_url.trim();
    let label = format!("OpenChamber session {}", status_word(kind));
    if url.is_empty() {
        return label;
    }
    // The comment already lives on the issue, so it says only what happened
    // and links to the session. Issue titles routinely contain brackets
    // ("[Bug] …"), which would break this markdown link if repeated.
    format!("[{label}]({url})")
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatusRecord {
    pub issue_identifier: String,
    pub session_origin: Option<String>,
    pub organization_id: Option<String>,
    pub started: bool,
    pub completed: bool,
    pub failure: bool,
}

/// JS `readRecord`.
fn read_record(value: &Value) -> Option<StatusRecord> {
    if !is_plain_object(value) {
        return None;
    }
    let issue_identifier = read_trimmed_string(&value["issueIdentifier"]);
    if issue_identifier.is_empty() {
        return None;
    }
    Some(StatusRecord {
        issue_identifier,
        session_origin: {
            let origin = read_session_origin_from_value(&value["sessionOrigin"]);
            (!origin.is_empty()).then_some(origin)
        },
        organization_id: optional(&value["organizationId"]),
        started: matches!(value["started"], Value::Bool(true)),
        completed: matches!(value["completed"], Value::Bool(true)),
        failure: matches!(value["failure"], Value::Bool(true)),
    })
}

fn read_session_origin_from_value(value: &Value) -> String {
    read_session_origin(value.as_str().unwrap_or_default())
}

fn optional(value: &Value) -> Option<String> {
    let trimmed = read_trimmed_string(value);
    (!trimmed.is_empty()).then_some(trimmed)
}

impl LinearState {
    fn status_file(&self) -> std::path::PathBuf {
        self.data_dir.join("linear-session-status.json")
    }

    /// JS `readRecords`, preserving the file's top-level key order (insertion
    /// order in JS) so "keep the newest" pruning matches.
    fn read_records(&self) -> Result<Vec<(String, StatusRecord)>, LinearError> {
        let file_path = self.status_file();
        if !file_path.exists() {
            return Ok(Vec::new());
        }
        let raw = std::fs::read_to_string(&file_path).map_err(|_| {
            LinearError::session_malformed("Linear session status file is malformed")
        })?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let parsed: Value = serde_json::from_str(trimmed).map_err(|_| {
            LinearError::session_malformed("Linear session status file is malformed")
        })?;
        if !is_plain_object(&parsed) {
            return Err(LinearError::session_malformed(
                "Linear session status file is malformed",
            ));
        }
        let Some(object) = parsed.as_object() else {
            return Err(LinearError::session_malformed(
                "Linear session status file is malformed",
            ));
        };
        let order = super::top_level_key_order(trimmed);
        let mut next: Vec<(String, StatusRecord)> = Vec::new();
        // Scanner order first (insertion order); any keys the scanner missed
        // follow in the map's own order.
        for session_id in order {
            if next.iter().any(|(key, _)| key == &session_id) {
                continue;
            }
            if let Some(record) = object.get(&session_id).and_then(read_record) {
                next.push((session_id, record));
            }
        }
        for (session_id, value) in object {
            if next.iter().any(|(key, _)| key == session_id) {
                continue;
            }
            if let Some(record) = read_record(value) {
                next.push((session_id.clone(), record));
            }
        }
        Ok(next)
    }

    /// JS `writeRecords` (with pruning applied by the caller).
    fn write_records(&self, records: &[(String, StatusRecord)]) -> Result<(), LinearError> {
        let body = render_records_json(records);
        super::write_file_atomic_600(&self.status_file(), &body)
            .map_err(|e| LinearError::plain(e.to_string()))
    }
}

/// JS `pruneSessionStatusRecords`: keep the newest `limit` records.
pub fn prune_session_status_records(
    records: Vec<(String, StatusRecord)>,
    limit: usize,
) -> Vec<(String, StatusRecord)> {
    if records.len() <= limit {
        return records;
    }
    records[records.len() - limit..].to_vec()
}

/// Render the records document exactly like `JSON.stringify(payload, null,
/// 2)` writes the JS insertion-ordered object (key order preserved).
fn render_records_json(records: &[(String, StatusRecord)]) -> String {
    if records.is_empty() {
        return "{}".to_string();
    }
    let mut out = String::from("{\n");
    for (index, (session_id, record)) in records.iter().enumerate() {
        if index > 0 {
            out.push_str(",\n");
        }
        out.push_str("  ");
        out.push_str(&js_quoted(session_id));
        out.push_str(": ");
        out.push_str(&render_record(record));
    }
    out.push_str("\n}");
    out
}

fn render_record(record: &StatusRecord) -> String {
    format!(
        "{{\n    \"issueIdentifier\": {},\n    \"sessionOrigin\": {},\n    \"organizationId\": {},\n    \"started\": {},\n    \"completed\": {},\n    \"failure\": {}\n  }}",
        js_quoted(&record.issue_identifier),
        optional_js(record.session_origin.as_deref()),
        optional_js(record.organization_id.as_deref()),
        record.started,
        record.completed,
        record.failure
    )
}

fn optional_js(value: Option<&str>) -> String {
    value.map(js_quoted).unwrap_or_else(|| "null".to_string())
}

fn js_quoted(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

#[derive(Debug, Clone, Default)]
pub struct SessionStatusInput {
    pub kind: String,
    pub session_id: String,
    pub issue_identifier: String,
    pub session_origin: String,
    pub organization_id: String,
}

/// JS `postOnce`.
async fn post_once(
    state: &Arc<LinearState>,
    input: &SessionStatusInput,
) -> Result<Value, LinearError> {
    let kind = input.kind.trim();
    let session_id = input.session_id.trim();
    if !LINEAR_SESSION_STATUS_KINDS.contains(&kind) || session_id.is_empty() {
        return Err(LinearError::invalid("kind and sessionId are required"));
    }

    // Disconnected answers first so the picker and panel keep showing their
    // "connect Linear" state whatever the comment preference says.
    let auth = state.get_auth();
    let Some(auth) = auth else {
        return Ok(json!({ "connected": false }));
    };
    if !state.session_comments_enabled() {
        return Ok(json!({ "connected": true, "posted": false, "skipped": "disabled" }));
    }

    let mut records = state.read_records()?;
    let existing = records
        .iter()
        .find(|(key, _)| key == session_id)
        .map(|(_, record)| record.clone());
    let flag = |record: &StatusRecord| match kind {
        "started" => record.started,
        "completed" => record.completed,
        _ => record.failure,
    };
    if let Some(existing) = &existing
        && flag(existing)
    {
        return Ok(json!({ "connected": true, "posted": false, "skipped": "already-posted" }));
    }
    if kind != "started" && !existing.as_ref().is_some_and(|record| record.started) {
        return Ok(json!({ "connected": true, "posted": false, "skipped": "not-started" }));
    }

    let issue_identifier = if !input.issue_identifier.trim().is_empty() {
        input.issue_identifier.trim().to_string()
    } else {
        existing
            .as_ref()
            .map(|record| record.issue_identifier.clone())
            .unwrap_or_default()
    };
    if issue_identifier.is_empty() {
        return Err(LinearError::invalid("issueIdentifier is required"));
    }

    let session_origin = {
        let from_input = read_session_origin(&input.session_origin);
        if !from_input.is_empty() {
            from_input
        } else {
            existing
                .as_ref()
                .and_then(|record| record.session_origin.clone())
                .map(|origin| origin.trim().to_string())
                .unwrap_or_default()
        }
    };
    // Without an origin other people can reach, the comment would carry a
    // link only its author could open. Say nothing rather than publish a
    // dead link.
    if !is_public_session_origin(&session_origin) {
        return Ok(json!({ "connected": true, "posted": false, "skipped": "origin-not-public" }));
    }
    let session_url = build_linear_session_open_url(session_id, &session_origin);
    let organization_id = {
        let from_input = input.organization_id.trim();
        let from_existing = existing
            .as_ref()
            .and_then(|record| record.organization_id.clone())
            .unwrap_or_default();
        let from_auth = auth.workspace_id.clone();
        if !from_input.is_empty() {
            from_input.to_string()
        } else if !from_existing.is_empty() {
            from_existing
        } else {
            from_auth
        }
    };
    let body = build_linear_session_status_comment(kind, &session_url);
    let comment_result = create_linear_issue_comment(
        state,
        &issue_identifier,
        &Value::String(body),
        Some(&organization_id),
    )
    .await?;
    if comment_result["connected"] == json!(false) {
        return Ok(json!({ "connected": false }));
    }
    if comment_result["comment"].is_null() {
        return Ok(json!({ "connected": true, "posted": false, "skipped": "issue-not-found" }));
    }

    let record = StatusRecord {
        issue_identifier,
        session_origin: (!session_origin.is_empty()).then(|| session_origin.clone()),
        organization_id: (!organization_id.is_empty()).then(|| organization_id.clone()),
        started: existing.as_ref().is_some_and(|r| r.started) || kind == "started",
        completed: existing.as_ref().is_some_and(|r| r.completed) || kind == "completed",
        failure: existing.as_ref().is_some_and(|r| r.failure) || kind == "failure",
    };
    match records.iter_mut().find(|(key, _)| key == session_id) {
        Some(entry) => entry.1 = record,
        None => records.push((session_id.to_string(), record)),
    }
    state.write_records(&prune_session_status_records(
        records,
        MAX_SESSION_STATUS_RECORDS,
    ))?;
    Ok(json!({
        "connected": true,
        "posted": true,
        "commentId": comment_result["comment"]["id"],
    }))
}

/// JS `postLinearSessionStatus`: concurrent posts for the same session and
/// kind share one in-flight promise.
pub async fn post_linear_session_status(
    state: &Arc<LinearState>,
    input: &SessionStatusInput,
) -> Result<Value, LinearError> {
    let kind = input.kind.trim().to_string();
    let session_id = input.session_id.trim().to_string();
    let key = format!("{session_id}:{kind}");
    let future = {
        let mut inflight = state
            .status_inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match inflight.get(&key) {
            Some(existing) => existing.clone(),
            None => {
                let state_for_post = state.clone();
                let input = input.clone();
                let future =
                    super::shared(Box::pin(
                        async move { post_once(&state_for_post, &input).await },
                    ));
                inflight.insert(key.clone(), future.clone());
                future
            }
        }
    };
    let result = future.await;
    state
        .status_inflight
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    result
}
