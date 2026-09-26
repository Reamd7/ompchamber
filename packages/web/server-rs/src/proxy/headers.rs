//! Port of `server/proxy-headers.js` + the `x-opencode-directory` header
//! normalization from `server/lib/opencode/proxy.js`
//! (`normalizeForwardedDirectoryHeaders`).
//!
//! Client credentials for the OMPChamber server (UI client tokens) must never
//! reach the managed OpenCode upstream — it only accepts its own auth, so a
//! forwarded client bearer turns every upstream response into a 401.

use axum::http::{HeaderMap, HeaderValue};

/// `filteredRequestHeaders` in proxy-headers.js.
pub(crate) const FILTERED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "host",
    "connection",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "accept-encoding",
];

/// `filteredResponseHeaders` in proxy-headers.js.
pub(crate) const FILTERED_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "content-length",
    "transfer-encoding",
    "keep-alive",
    "te",
    "trailer",
    "upgrade",
    "www-authenticate",
    "content-encoding",
];

/// Port of `collectForwardProxyHeaders`: hop-by-hop + credential request
/// headers are dropped, remaining headers are forwarded (duplicate values
/// joined with ", " the way node does), and the managed OpenCode
/// `Authorization` (when present) replaces any client credential.
pub(crate) fn collect_forward_request_headers(
    request_headers: &HeaderMap,
    auth_header: Option<&str>,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in request_headers.keys() {
        let normalized_key = name.as_str();
        if FILTERED_REQUEST_HEADERS.contains(&normalized_key) {
            continue;
        }
        let values: Vec<&HeaderValue> = request_headers
            .get_all(name)
            .iter()
            .filter(|value| !value.is_empty())
            .collect();
        if values.is_empty() {
            // JS: `if (!value) continue;` — empty/null values are skipped.
            continue;
        }
        if values.len() == 1 {
            headers.insert(name.clone(), values[0].clone());
        } else {
            let joined = values
                .iter()
                .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            if let Ok(value) = HeaderValue::from_str(&joined) {
                headers.insert(name.clone(), value);
            }
        }
    }
    if let Some(auth) = auth_header
        && let Ok(value) = HeaderValue::from_str(auth)
    {
        headers.insert("authorization", value);
    }
    headers
}

/// Port of `shouldForwardProxyResponseHeader`.
pub(crate) fn should_forward_response_header(key: &str) -> bool {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lowered = trimmed.to_ascii_lowercase();
    !FILTERED_RESPONSE_HEADERS.contains(&lowered.as_str())
}

/// Port of `applyForwardProxyResponseHeaders`: upstream response headers minus
/// the filtered set (content-encoding/transfer-encoding/www-authenticate/...),
/// ready to attach to the downstream response. Multi-valued headers keep every
/// value.
pub(crate) fn forward_response_headers(upstream_headers: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream_headers.iter() {
        if !should_forward_response_header(name.as_str()) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

/// Port of `normalizeForwardedDirectoryHeaders`: clients that URL-encode the
/// `x-opencode-directory` header mark it with
/// `x-opencode-directory-encoding: uri`; the raw value is percent-decoded and
/// the marker removed. Malformed values are left untouched for upstream to
/// reject.
pub(crate) fn normalize_directory_headers(headers: &mut HeaderMap) {
    let encoding_is_uri = headers
        .get("x-opencode-directory-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "uri")
        .unwrap_or(false);
    if !encoding_is_uri {
        return;
    }
    if let Some(raw) = headers.get("x-opencode-directory") {
        let raw_text = String::from_utf8_lossy(raw.as_bytes()).into_owned();
        // decodeURIComponent: invalid escapes throw — leave the header as-is.
        if let Some(decoded) = percent_decode_component(&raw_text)
            && let Ok(value) = HeaderValue::from_str(&decoded)
        {
            headers.insert("x-opencode-directory", value);
        }
    }
    headers.remove("x-opencode-directory-encoding");
}

/// `decodeURIComponent` semantics: `%XX` sequences decode to their byte;
/// anything else (including `+`) passes through; a malformed escape returns
/// `None` (JS throws, callers leave the original untouched).
pub(crate) fn percent_decode_component(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            // Need two hex digits after '%'.
            if i + 2 >= bytes.len() {
                return None;
            }
            let hi = hex_digit(bytes[i + 1])?;
            let lo = hex_digit(bytes[i + 2])?;
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// `URLSearchParams` lossy decoding: `+` means space, malformed `%XX`
/// sequences pass through literally (JS never throws here).
pub(crate) fn percent_decode_form_lossy(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
            {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
/// `URLSearchParams` serialization: `[A-Za-z0-9*-._]` stay, space becomes
/// `+`, everything else percent-encodes as uppercase UTF-8 hex.
pub(crate) fn percent_encode_form(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(*byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// `encodeURIComponent` serialization: `[A-Za-z0-9-_.!~*'()]` stay,
/// Only used by the Windows session-merge path (`encodeURIComponent` parity).
#[cfg(windows)]
pub(crate) fn percent_encode_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn header_map(entries: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (key, value) in entries {
            map.insert(
                axum::http::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn drops_filtered_request_headers_and_keeps_others() {
        let headers = collect_forward_request_headers(
            &header_map(&[
                ("accept", "application/json"),
                ("accept-encoding", "gzip, deflate, br"),
                ("connection", "keep-alive"),
                ("host", "example.com"),
                ("content-length", "12"),
                ("x-custom", "kept"),
            ]),
            None,
        );
        assert_eq!(headers.get("accept").unwrap(), "application/json");
        assert_eq!(headers.get("x-custom").unwrap(), "kept");
        assert!(headers.get("accept-encoding").is_none());
        assert!(headers.get("connection").is_none());
        assert!(headers.get("host").is_none());
        assert!(headers.get("content-length").is_none());
    }

    #[test]
    fn replaces_client_authorization_with_managed_auth() {
        let headers = collect_forward_request_headers(
            &header_map(&[("authorization", "Bearer oc_client_stale-ui-token")]),
            Some("Basic b3BlbmNvZGU6cHc="),
        );
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Basic b3BlbmNvZGU6cHc="
        );
    }

    #[test]
    fn drops_client_authorization_when_upstream_has_no_auth() {
        let headers = collect_forward_request_headers(
            &header_map(&[
                ("accept", "application/json"),
                ("authorization", "Bearer x"),
            ]),
            None,
        );
        assert!(headers.get("authorization").is_none());
        assert_eq!(headers.get("accept").unwrap(), "application/json");
    }

    #[test]
    fn skips_empty_header_values_like_node_falsy_check() {
        let headers = collect_forward_request_headers(&header_map(&[("x-empty", "")]), None);
        assert!(headers.get("x-empty").is_none());
    }

    #[test]
    fn drops_hop_by_hop_and_content_encoding_from_response_headers() {
        assert!(!should_forward_response_header("content-encoding"));
        assert!(!should_forward_response_header("Content-Encoding"));
        assert!(!should_forward_response_header("transfer-encoding"));
        assert!(!should_forward_response_header("www-authenticate"));
        assert!(!should_forward_response_header("  "));
        assert!(should_forward_response_header("content-type"));
        assert!(should_forward_response_header("etag"));
    }

    #[test]
    fn forwards_upstream_response_headers_without_filtered_set() {
        let upstream = header_map(&[
            ("content-type", "application/json"),
            ("etag", "W/\"abc\""),
            ("content-encoding", "gzip"),
            ("content-length", "42"),
        ]);
        let forwarded = forward_response_headers(&upstream);
        assert_eq!(forwarded.get("content-type").unwrap(), "application/json");
        assert_eq!(forwarded.get("etag").unwrap(), "W/\"abc\"");
        assert!(forwarded.get("content-encoding").is_none());
        assert!(forwarded.get("content-length").is_none());
    }

    #[test]
    fn normalizes_uri_encoded_directory_header() {
        let mut headers = header_map(&[
            ("x-opencode-directory", "%2FUsers%2Fgemini%2Frepo"),
            ("x-opencode-directory-encoding", "uri"),
        ]);
        normalize_directory_headers(&mut headers);
        assert_eq!(
            headers.get("x-opencode-directory").unwrap(),
            "/Users/gemini/repo"
        );
        assert!(headers.get("x-opencode-directory-encoding").is_none());
    }

    #[test]
    fn leaves_directory_headers_alone_without_uri_marker() {
        let mut headers = header_map(&[
            ("x-opencode-directory", "%2Frepo"),
            ("x-opencode-directory-encoding", "raw"),
        ]);
        normalize_directory_headers(&mut headers);
        assert_eq!(headers.get("x-opencode-directory").unwrap(), "%2Frepo");
        assert_eq!(headers.get("x-opencode-directory-encoding").unwrap(), "raw");
    }

    #[test]
    fn leaves_malformed_encoded_directory_value_but_removes_marker() {
        let mut headers = header_map(&[
            ("x-opencode-directory", "%zz-broken"),
            ("x-opencode-directory-encoding", "uri"),
        ]);
        normalize_directory_headers(&mut headers);
        // JS: decodeURIComponent throws → the value stays untouched, but the
        // `delete headers['x-opencode-directory-encoding']` after the
        // try/catch still runs.
        assert_eq!(headers.get("x-opencode-directory").unwrap(), "%zz-broken");
        assert!(headers.get("x-opencode-directory-encoding").is_none());
    }

    #[test]
    fn percent_decode_component_matches_decode_uri_component() {
        assert_eq!(
            percent_decode_component("%2Frepo%20name").as_deref(),
            Some("/repo name")
        );
        assert_eq!(percent_decode_component("plain").as_deref(), Some("plain"));
        assert_eq!(
            percent_decode_component("%E4%B8%AD").as_deref(),
            Some("\u{4e2d}")
        );
        assert!(percent_decode_component("%2").is_none());
        assert!(percent_decode_component("%zz").is_none());
    }

    #[test]
    fn percent_decode_form_lossy_keeps_malformed_escapes() {
        assert_eq!(percent_decode_form_lossy("a+b"), "a b");
        assert_eq!(percent_decode_form_lossy("%2Fx"), "/x");
        assert_eq!(percent_decode_form_lossy("%zz"), "%zz");
    }
    #[test]
    #[cfg(windows)]
    fn percent_encode_component_matches_encode_uri_component() {
        assert_eq!(percent_encode_component("/a b&c"), "%2Fa%20b%26c");
        assert_eq!(percent_encode_component("x-y_z.~!*()'"), "x-y_z.~!*()'");
    }

    #[test]
    fn percent_encode_form_matches_url_search_params() {
        assert_eq!(
            percent_encode_form("/Users/x/Yo Yo"),
            "%2FUsers%2Fx%2FYo+Yo"
        );
        assert_eq!(percent_encode_form("a-b_c.d*e"), "a-b_c.d*e");
    }
}
