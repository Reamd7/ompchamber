//! Port of `server/lib/markdown-image-grants/routes.js`.
//!
//! Mints path-bound raw-file grants for images an assistant message actually
//! references: the assistant's own markdown is authoritative (a remote client
//! cannot mint grants for unreferenced paths), the file must be inside the
//! session directory (or the approved temp root, symlink-resolved), have an
//! image signature, and fit the size cap. Outside-workspace files get an
//! `OutsideGrantStore` grant reusable by `/api/fs/raw` — the same store the
//! fs routes serve, shared via `fs_routes::router_with`.
//! 本模块是 `server/lib/markdown-image-grants/routes.js` 的 Rust 移植。
//!
//! 为 assistant 消息实际引用到的图片铸造路径绑定的 raw-file grant：
//! assistant 自己的 markdown 是权威来源（远端客户端无法为未引用的
//! 路径铸造 grant）；文件必须位于 session 目录内（或经 symlink 解析后
//! 位于获批的 temp root 内）、具有图片签名、不超过大小上限。工作区
//! 之外的文件会得到一个 OutsideGrantStore 的 grant，可被 /api/fs/raw
//! 复用——与 fs 路由同一存储，经 fs_routes::router_with 共享。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::context::RouterContext;
use crate::fs_routes::FsState;

/// 单张图片的大小上限（10 MiB），超出即拒绝。
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// 单次请求允许携带的图片源数量上限。
const MAX_IMAGE_SOURCES: usize = 12;

/// 读取字符串值并 trim；非字符串返回空串。
fn as_string(value: &Value) -> String {
    value.as_str().unwrap_or_default().trim().to_string()
}

/// 对应 Node 的 path.relative(root, target) 包含性判断（纯词法比较）。
/// Node `path.relative(root, target)` containment check (lexical).
fn is_within(target: &Path, root: &Path) -> bool {
    match target.strip_prefix(root) {
        Ok(relative) => !relative.as_os_str().is_empty() || same_path(target, root),
        Err(_) => false,
    }
}

/// 两个路径是否等价：直接相等，或字符串表示一致。
fn same_path(a: &Path, b: &Path) -> bool {
    a == b || a.to_string_lossy() == b.to_string_lossy()
}

/// 对应 JS parseFileSource：接受 file:// URL（仅限无 host 或 localhost，
/// 做 percent-decode）或普通路径（剥离 query/fragment）；非法输入返回空串。
/// `parseFileSource`: file:// URLs (localhost only, %-decoded) or plain
/// paths with query/fragment stripped.
fn parse_file_source(source: &str) -> String {
    if source.len() >= 7 && source[..7].eq_ignore_ascii_case("file://") {
        let Ok(url) = url::Url::parse(source) else {
            return String::new();
        };
        if url.scheme() != "file" {
            return String::new();
        }
        match url.host_str() {
            None | Some("localhost") => {}
            Some(_) => return String::new(),
        }
        let pathname = percent_decode(url.path());
        // Windows drive letters arrive as /C:/ — JS strips the leading slash.
        let bytes = pathname.as_bytes();
        if bytes.len() >= 3
            && bytes[0] == b'/'
            && bytes[1].is_ascii_alphabetic()
            && bytes[2] == b':'
        {
            return pathname[1..].to_string();
        }
        return pathname;
    }
    let pathname = source.split(['?', '#']).next().unwrap_or_default();
    percent_decode(pathname)
}

/// 百分号解码：把 %XX 还原为字节；非法序列原样保留。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() + 1 && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 对应 JS hasImageSignature：识别 PNG/JPEG/GIF87a/GIF89a/RIFF-WEBP 魔数。
/// `hasImageSignature`: PNG/JPEG/GIF87a/GIF89a/RIFF-WEBP.
fn has_image_signature(bytes: &[u8]) -> bool {
    if bytes.len() >= 8
        && bytes[0] == 0x89
        && &bytes[1..4] == b"PNG"
        && bytes[4] == 0x0d
        && bytes[5] == 0x0a
        && bytes[6] == 0x1a
        && bytes[7] == 0x0a
    {
        return true;
    }
    if bytes.len() >= 3 && bytes[0] == 0xff && bytes[1] == 0xd8 && bytes[2] == 0xff {
        return true;
    }
    let header = String::from_utf8_lossy(&bytes[..bytes.len().min(12)]).into_owned();
    header.starts_with("GIF87a")
        || header.starts_with("GIF89a")
        || (header.starts_with("RIFF")
            && &header[8.min(header.len())..12.min(header.len())] == "WEBP")
}

/// 归一化引用式图片的 label：trim、连续空白折叠为单个空格、转小写。
fn normalize_reference_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_space = false;
    for ch in value.trim().chars() {
        if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(ch.to_lowercase().next().unwrap_or(ch));
            last_was_space = false;
        }
    }
    out
}

/// CommonMark 定义的 ASCII 标点集合（反斜杠转义的目标字符）。
const PUNCTUATION: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";

/// 对应 JS unescapeMarkdownDestination：去掉转义 ASCII 标点（含 \\）
/// 前的反斜杠。
/// `unescapeMarkdownDestination`: drop the backslash before escaped ASCII
/// punctuation (and `\\`).
fn unescape_markdown_destination(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(&next) = chars.peek()
            && (next == '\\' || PUNCTUATION.contains(next))
        {
            out.push(next);
            chars.next();
            continue;
        }
        out.push(ch);
    }
    out
}

/// 判断 index 处的字符是否被奇数个前导反斜杠转义。
fn is_escaped_at(value: &str, index: usize) -> bool {
    let bytes = value.as_bytes();
    let mut slashes = 0;
    let mut cursor = index as isize - 1;
    while cursor >= 0 && bytes[cursor as usize] == b'\\' {
        slashes += 1;
        cursor -= 1;
    }
    slashes % 2 == 1
}

/// 从 start 起查找下一个未被转义的 ]。
fn find_closing_bracket(value: &str, start: usize) -> Option<usize> {
    let bytes = value.as_bytes();
    for cursor in start..bytes.len() {
        if bytes[cursor] == b']' && !is_escaped_at(value, cursor) {
            return Some(cursor);
        }
    }
    None
}

/// 是否为 CommonMark 认定的空白字节。
fn is_whitespace_byte(byte: u8) -> bool {
    byte == b' ' || byte == b'\t' || byte == b'\n' || byte == b'\r' || byte == 0x0b || byte == 0x0c
}

/// 定位内联图片 ![alt](dest "title") 的收尾 )：允许空白与引号/括号
/// 包裹的 title。
fn find_inline_image_end(value: &str, start: usize) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut cursor = start;
    while cursor < bytes.len() && is_whitespace_byte(bytes[cursor]) {
        cursor += 1;
    }
    if cursor < bytes.len() && bytes[cursor] == b')' {
        return Some(cursor);
    }
    if cursor >= bytes.len() {
        return None;
    }
    let opener = bytes[cursor];
    let closer = match opener {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    cursor += 1;
    while cursor < bytes.len() {
        if bytes[cursor] == closer && !is_escaped_at(value, cursor) {
            cursor += 1;
            while cursor < bytes.len() && is_whitespace_byte(bytes[cursor]) {
                cursor += 1;
            }
            return if cursor < bytes.len() && bytes[cursor] == b')' {
                Some(cursor)
            } else {
                None
            };
        }
        cursor += 1;
    }
    None
}

/// 内联图片 destination 的解析结果。
struct InlineDestination {
    /// 反转义后的目标地址。
    source: String,
    /// 整个内联图片（含收尾括号）结束处的字节索引。
    end: usize,
}

/// 解析 ![alt]( 之后到 ) 之前的 destination：支持 <尖括号> 形态与含
/// 配对括号/转义字符的裸形态。
fn parse_inline_destination(value: &str, start: usize) -> Option<InlineDestination> {
    let bytes = value.as_bytes();
    let mut cursor = start;
    while cursor < bytes.len() && is_whitespace_byte(bytes[cursor]) {
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return None;
    }
    if bytes[cursor] == b'<' {
        let end = value[cursor + 1..]
            .find('>')
            .map(|offset| cursor + 1 + offset)?;
        let image_end = find_inline_image_end(value, end + 1)?;
        return Some(InlineDestination {
            source: unescape_markdown_destination(&value[cursor + 1..end]),
            end: image_end,
        });
    }

    let mut source = String::new();
    let mut depth = 0;
    while cursor < bytes.len() {
        let ch = bytes[cursor] as char;
        if ch == '\\' && cursor + 1 < bytes.len() {
            source.push(ch);
            source.push(bytes[cursor + 1] as char);
            cursor += 2;
            continue;
        }
        if ch == '(' {
            depth += 1;
            source.push(ch);
            cursor += 1;
            continue;
        }
        if ch == ')' {
            if depth == 0 {
                return Some(InlineDestination {
                    source: unescape_markdown_destination(&source),
                    end: cursor,
                });
            }
            depth -= 1;
            source.push(ch);
            cursor += 1;
            continue;
        }
        if is_whitespace_byte(bytes[cursor]) && depth == 0 {
            let image_end = find_inline_image_end(value, cursor)?;
            return Some(InlineDestination {
                source: unescape_markdown_destination(&source),
                end: image_end,
            });
        }
        source.push(ch);
        cursor += 1;
    }
    None
}

/// 解析引用定义 [label]: 之后的 destination：<尖括号> 形态，或保留
/// 转义的非空白连续段。
fn parse_definition_destination(value: &str) -> String {
    let trimmed = value.trim_start();
    if trimmed.starts_with('<') {
        return match trimmed[1..].find('>') {
            Some(end) => unescape_markdown_destination(&trimmed[1..1 + end]),
            None => String::new(),
        };
    }
    // `^(?:\\.|\S)+` — non-space runs with escaped characters kept whole.
    let mut out = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if ch == '\\' {
            let mut escaped = String::from(ch);
            chars.next();
            if let Some(&next) = chars.peek() {
                escaped.push(next);
                chars.next();
            }
            out.push_str(&escaped);
            continue;
        }
        if ch.is_whitespace() {
            break;
        }
        out.push(ch);
        chars.next();
    }
    unescape_markdown_destination(&out)
}

/// 收集消息 text parts 的行：剔除围栏代码块内的行，并移除行内 code span。
/// Lines from text parts with fenced code blocks and inline code spans
/// removed.
fn collect_markdown_lines_outside_code(message: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(parts) = message.get("parts").and_then(Value::as_array) else {
        return lines;
    };
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        let mut fence: Option<(char, usize)> = None;
        for line in text.split('\n') {
            let fence_marker = leading_fence_marker(line);
            if let Some(marker) = fence_marker {
                if fence.is_none() {
                    fence = Some((marker.chars().next().unwrap(), marker.len()));
                } else if let Some((fence_char, fence_size)) = fence
                    && marker.starts_with(fence_char)
                    && marker.len() >= fence_size
                {
                    fence = None;
                }
                continue;
            }
            if fence.is_some() {
                continue;
            }
            lines.push(strip_inline_code(line));
        }
    }
    lines
}

/// 识别行首（最多 3 个空格缩进）的 ``` 或 ~~~ 围栏标记。
/// `^\s{0,3}(`{3,}|~{3,})` fence marker at line start.
fn leading_fence_marker(line: &str) -> Option<String> {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let mut backticks = 0;
    let mut tildes = 0;
    for ch in rest.chars() {
        match ch {
            '`' if tildes == 0 => backticks += 1,
            '~' if backticks == 0 => tildes += 1,
            _ => break,
        }
    }
    if backticks >= 3 {
        Some("`".repeat(backticks))
    } else if tildes >= 3 {
        Some("~".repeat(tildes))
    } else {
        None
    }
}

/// 把行内 code span（反引号包裹的片段）替换为空串，对应 JS 正则
/// /`+[^`]*`+/g。
/// Replace `/`+[^`]+`+` spans (JS regex `` /`+[^`]*`+/g ``) with ''.
fn strip_inline_code(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '`' {
            let mut run = 0;
            while index + run < chars.len() && chars[index + run] == '`' {
                run += 1;
            }
            // Find a closing run of backticks after non-backtick content.
            let mut cursor = index + run;
            while cursor < chars.len() && chars[cursor] != '`' {
                cursor += 1;
            }
            if cursor < chars.len() {
                let mut closing = 0;
                while cursor + closing < chars.len() && chars[cursor + closing] == '`' {
                    closing += 1;
                }
                if closing >= 1 {
                    index = cursor + closing;
                    continue;
                }
            }
            out.push_str(&line.chars().skip(index).take(run).collect::<String>());
            index += run;
            continue;
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

/// 提取消息引用的全部图片来源（内联图片 + 引用定义）。
/// All image sources (inline + reference definitions) a message references.
pub fn markdown_image_sources(message: &Value) -> HashSet<String> {
    let mut sources = HashSet::new();
    let markdown_lines = collect_markdown_lines_outside_code(message);
    let mut definitions: HashMap<String, String> = HashMap::new();
    for line in &markdown_lines {
        if let Some((label, destination)) = definition_line(line) {
            let source = parse_definition_destination(&destination);
            if !source.is_empty() {
                definitions.insert(normalize_reference_label(&label), source);
            }
        }
    }

    for line in &markdown_lines {
        let bytes = line.as_bytes();
        let mut cursor = 0;
        while cursor < bytes.len() {
            if bytes[cursor] != b'!'
                || cursor + 1 >= bytes.len()
                || bytes[cursor + 1] != b'['
                || is_escaped_at(line, cursor)
            {
                cursor += 1;
                continue;
            }
            let Some(alt_end) = find_closing_bracket(line, cursor + 2) else {
                cursor += 1;
                continue;
            };
            let alt = line[cursor + 2..alt_end].to_string();
            let next = bytes.get(alt_end + 1).copied();
            if next == Some(b'(') {
                if let Some(parsed) = parse_inline_destination(line, alt_end + 2) {
                    if !parsed.source.is_empty() {
                        sources.insert(parsed.source.clone());
                    }
                    cursor = parsed.end;
                } else {
                    cursor = alt_end;
                }
                cursor += 1;
                continue;
            }
            let mut label = alt.clone();
            if next == Some(b'[') {
                let Some(label_end) = find_closing_bracket(line, alt_end + 2) else {
                    cursor = alt_end + 1;
                    continue;
                };
                let explicit = &line[alt_end + 2..label_end];
                if !explicit.is_empty() {
                    label = explicit.to_string();
                }
                cursor = label_end;
            } else {
                cursor = alt_end;
            }
            if let Some(source) = definitions.get(&normalize_reference_label(&label)) {
                sources.insert(source.clone());
            }
            cursor += 1;
        }
    }
    sources
}

/// 识别 ^\s{0,3}\[label\]: destination 形态的链接定义行，
/// 返回 (label, 冒号之后的剩余部分)。
/// `^\s{0,3}\[([^\]]+)]\s*:\s*(.*)$` link-definition line.
fn definition_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();
    if indent > 3 || !trimmed.starts_with('[') {
        return None;
    }
    let close = trimmed.find(']')?;
    let label = &trimmed[1..close];
    let rest = &trimmed[close + 1..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':')?;
    Some((label.to_string(), rest.trim_start().to_string()))
}

/// 从 OpenCode 服务拉取指定消息：404 或响应缺 info/parts 结构时返回
/// Ok(None)；服务不可用或非成功状态返回 Err(描述)。
async fn fetch_message(
    engine: &crate::engine::EngineState,
    session_id: &str,
    message_id: &str,
    directory: &str,
) -> Result<Option<Value>, String> {
    let Some(base) = engine.base_url() else {
        return Err("OpenCode service unavailable".to_string());
    };
    let url = format!(
        "{base}/session/{}/message/{}?directory={}",
        encode_uri_component(session_id),
        encode_uri_component(message_id),
        encode_uri_component(directory),
    );
    let mut request = engine
        .http()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .header("accept", "application/json")
        .header("x-opencode-directory", encode_uri_component(directory));
    if let Some(auth) = engine.auth_header() {
        request = request.header("authorization", auth);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    if response.status().as_u16() == 404 {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("OpenCode returned {}", response.status()));
    }
    let text = response.text().await.map_err(|e| e.to_string())?;
    let message: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let has_info = message.get("info").is_some_and(Value::is_object);
    let has_parts = message.get("parts").and_then(Value::as_array).is_some();
    Ok(if has_info && has_parts {
        Some(message)
    } else {
        None
    })
}

/// inspect_image 的判定结果。
#[derive(Debug)]
enum InspectStatus {
    /// 通过全部校验，可以铸造 grant。
    Ready {
        /// 通过校验的文件路径（工作区内为原始路径，工作区外为 canonical 路径）。
        path: PathBuf,
        /// 是否位于工作区之外（决定是否需要 OutsideGrantStore）。
        outside_workspace: bool,
    },
    /// 文件不存在。
    Missing,
    /// 其它校验失败（越界、超限、非图片等）。
    Error,
}

/// 校验单个图片源：解析 file:// 路径（相对路径按 session 目录做词法
/// 解析）；canonicalize 后必须位于允许的根内、是普通文件、不超大小
/// 上限且具有图片签名。
async fn inspect_image(source: &str, directory: &Path, approved_temp_root: &Path) -> InspectStatus {
    let parsed = parse_file_source(source);
    if parsed.is_empty() {
        return InspectStatus::Error;
    }
    let parsed_path = Path::new(&parsed);
    let source_path = if parsed_path.is_absolute() {
        parsed_path.to_path_buf()
    } else {
        lexical_resolve(directory, parsed_path)
    };
    let workspace_root = directory.to_path_buf();
    let outside_workspace = !is_within(&source_path, &workspace_root);
    let root = if outside_workspace {
        approved_temp_root.to_path_buf()
    } else {
        workspace_root
    };

    // Resolve symlinks before comparing roots; lexical prefixes are not an
    // authorization boundary.
    let canonical_root = match tokio::fs::canonicalize(&root).await {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return InspectStatus::Missing,
        Err(_) => return InspectStatus::Error,
    };
    let canonical_path = match tokio::fs::canonicalize(&source_path).await {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return InspectStatus::Missing,
        Err(_) => return InspectStatus::Error,
    };
    if !is_within(&canonical_path, &canonical_root) {
        return InspectStatus::Error;
    }
    let metadata = match tokio::fs::metadata(&canonical_path).await {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return InspectStatus::Missing,
        Err(_) => return InspectStatus::Error,
    };
    if !metadata.is_file() || metadata.len() > MAX_IMAGE_BYTES {
        return InspectStatus::Error;
    }
    match tokio::fs::read(&canonical_path).await {
        Ok(bytes) if bytes.len() <= MAX_IMAGE_BYTES as usize => {
            if !has_image_signature(&bytes[..bytes.len().min(12)]) {
                return InspectStatus::Error;
            }
            InspectStatus::Ready {
                path: if outside_workspace {
                    canonical_path
                } else {
                    source_path
                },
                outside_workspace,
            }
        }
        Ok(_) => InspectStatus::Error,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => InspectStatus::Missing,
        Err(_) => InspectStatus::Error,
    }
}

/// 词法拼接 base 与 relative（消解 . 与 ..，不触碰文件系统）。
fn lexical_resolve(base: &Path, relative: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for component in base.components() {
        out.push(component.as_os_str().to_os_string());
    }
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str().to_os_string()),
        }
    }
    out.iter().collect()
}

/// 对应 JS encodeURIComponent：未保留字符原样输出，其余编码为
/// 大写十六进制的 %XX。
fn encode_uri_component(value: &str) -> String {
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

/// 路由共享状态。
#[derive(Clone)]
struct ModuleState {
    /// 路由上下文（含引擎，用于拉取消息）。
    ctx: RouterContext,
    /// fs 路由状态（提供 OutsideGrantStore）。
    fs: FsState,
    /// 获批的临时文件根目录（工作区外文件必须位于其中）。
    approved_temp_root: PathBuf,
}

/// POST /api/ompchamber/sessions/{sessionId}/markdown-image-grants：
/// 校验请求参数与目录，拉取 assistant 消息并以消息自身的 markdown 为
/// 权威来源，逐个 source 检查并铸造 grant；每个 source 返回
/// ready/missing/error 及对应的 grant 信息。
async fn mint_grants(
    State(state): State<ModuleState>,
    AxumPath(session_id): AxumPath<String>,
    body: Option<Json<Value>>,
) -> Response {
    let body = body.map(|Json(value)| value).unwrap_or_else(|| json!({}));
    let session_id = session_id.trim().to_string();
    let message_id = as_string(body.get("messageId").unwrap_or(&Value::Null));
    let sources: Vec<String> = body
        .get("sources")
        .and_then(Value::as_array)
        .map(|list| {
            let mut seen = HashSet::new();
            list.iter()
                .map(as_string)
                .filter(|value| !value.is_empty() && seen.insert(value.clone()))
                .collect()
        })
        .unwrap_or_default();
    if session_id.is_empty()
        || message_id.is_empty()
        || sources.is_empty()
        || sources.len() > MAX_IMAGE_SOURCES
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "sessionId, messageId, and 1-12 sources are required" })),
        )
            .into_response();
    }
    let directory_raw = as_string(body.get("directory").unwrap_or(&Value::Null));
    let validated = match crate::core_routes::directory::validate_directory_path(&directory_raw) {
        Ok(validated) => validated,
        Err(error) => {
            let message = if error.is_empty() {
                "Invalid directory".to_string()
            } else {
                error
            };
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response();
        }
    };
    let directory = validated.directory;

    let message = match fetch_message(
        &state.ctx.engine,
        &session_id,
        &message_id,
        &directory.to_string_lossy(),
    )
    .await
    {
        Ok(message) => message,
        Err(error) => {
            tracing::warn!("[MarkdownImageGrants] failed to prepare images: {error}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "Failed to prepare session images" })),
            )
                .into_response();
        }
    };
    let authorized = message.as_ref().is_some_and(|message| {
        message.pointer("/info/id").and_then(Value::as_str) == Some(message_id.as_str())
            && message.pointer("/info/role").and_then(Value::as_str) == Some("assistant")
    });
    let Some(message) = message.filter(|_| authorized) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "Assistant message not found" })),
        )
            .into_response();
    };

    // Assistant text is authoritative.
    let referenced = markdown_image_sources(&message);
    let mut results = Vec::with_capacity(sources.len());
    for source in &sources {
        if !referenced.contains(source) {
            results.push(json!({ "source": source, "status": "error" }));
            continue;
        }
        match inspect_image(source, &directory, &state.approved_temp_root).await {
            InspectStatus::Ready {
                path,
                outside_workspace,
            } => {
                let grant = if outside_workspace {
                    state
                        .fs
                        .grants()
                        .mint(&path.to_string_lossy(), &["raw"])
                        .await
                        .ok()
                } else {
                    None
                };
                results.push(json!({
                    "source": source,
                    "status": "ready",
                    "path": path.to_string_lossy(),
                    "outsideFileGrant": grant.as_ref().and_then(|g| g.get("outsideFileGrant")).cloned().unwrap_or(Value::Null),
                    "expiresAt": grant.as_ref().and_then(|g| g.get("expiresAt")).cloned().unwrap_or(Value::Null),
                }));
            }
            InspectStatus::Missing => {
                results.push(json!({ "source": source, "status": "missing" }))
            }
            InspectStatus::Error => results.push(json!({ "source": source, "status": "error" })),
        }
    }
    Json(json!({ "results": results })).into_response()
}

/// 构建 markdown-image-grants 路由；approved temp root 固定为
/// <系统临时目录>/opencode。
pub fn router(ctx: RouterContext, fs: FsState) -> Router {
    let approved_temp_root = std::env::temp_dir().join("opencode");
    Router::new()
        .route(
            "/api/ompchamber/sessions/{sessionId}/markdown-image-grants",
            post(mint_grants),
        )
        .with_state(ModuleState {
            ctx,
            fs,
            approved_temp_root,
        })
}

/// markdown 图片源提取与图片校验的行为测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 构造带单条 text part 的 assistant 消息 JSON。
    fn message_with_text(text: &str) -> Value {
        json!({ "info": { "id": "m1", "role": "assistant" }, "parts": [ { "type": "text", "text": text } ] })
    }

    /// 验证内联与引用定义两种形态的图片来源都会被提取。
    #[test]
    fn collects_inline_and_reference_sources() {
        let message = message_with_text(
            "![alt](/abs/img.png) ![x](relative/pic.jpg)\n\n[ref]: /imgs/ref.gif\n![ref]\n![missing][ref2]\n[ref2]: <spaced dir/we.png>\n![ref2][]",
        );
        let sources = markdown_image_sources(&message);
        assert!(sources.contains("/abs/img.png"));
        assert!(sources.contains("relative/pic.jpg"));
        assert!(sources.contains("/imgs/ref.gif"));
        assert!(sources.contains("spaced dir/we.png"));
        assert_eq!(sources.len(), 4);
    }

    /// 验证围栏代码块与行内 code 中的图片语法不被提取。
    #[test]
    fn ignores_code_fences_and_inline_code() {
        let message = message_with_text(
            "```\n![in](/fence.png)\n```\ninline `![in2](/code.png)` then ![ok](/ok.png)",
        );
        let sources = markdown_image_sources(&message);
        assert!(!sources.contains("/fence.png"));
        assert!(!sources.contains("/code.png"));
        assert!(sources.contains("/ok.png"));
    }

    /// 验证反斜杠转义的 ! 不构成图片语法。
    #[test]
    fn escaped_bangs_do_not_count() {
        let message = message_with_text("\\![not](/no.png)\n![yes](/yes.png)");
        let sources = markdown_image_sources(&message);
        assert!(!sources.contains("/no.png"));
        assert!(sources.contains("/yes.png"));
    }

    /// 验证 destination 的反转义只作用于 ASCII 标点：反斜杠+空格保持字面。
    #[test]
    fn unescapes_punctuation_in_destinations() {
        // Backslash-punctuation is unescaped; backslash-space is NOT (JS
        // unescapeMarkdownDestination only strips the ASCII punctuation
        // class), so it stays literal in the source.
        let message = message_with_text("![a](/path\\ with\\ spaces.png)");
        let sources = markdown_image_sources(&message);
        assert!(sources.contains("/path\\ with\\ spaces.png"));
    }

    /// 验证 file:// URL 解析（localhost、拒绝远程 host、percent-decode）
    /// 与普通路径的 query 剥离。
    #[test]
    fn parses_file_urls_and_strips_queries() {
        assert_eq!(parse_file_source("file:///tmp/a%20b.png"), "/tmp/a b.png");
        assert_eq!(
            parse_file_source("file://localhost/tmp/x.png"),
            "/tmp/x.png"
        );
        assert_eq!(parse_file_source("file://evil/tmp/x.png"), "");
        assert_eq!(parse_file_source("/tmp/x.png?v=1"), "/tmp/x.png");
    }

    /// 验证各图片格式的魔数识别与非图片内容的拒绝。
    #[test]
    fn image_signatures() {
        assert!(has_image_signature(&[
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a
        ]));
        assert!(has_image_signature(&[0xff, 0xd8, 0xff]));
        assert!(has_image_signature(b"GIF89a....."));
        assert!(has_image_signature(b"RIFF1234WEBP"));
        assert!(!has_image_signature(b"#!/bin/sh\n"));
        assert!(!has_image_signature(b""));
    }

    /// 验证 inspect_image 的包含性、图片签名与存在性判定（词法逃逸
    /// 不被授权）。
    #[tokio::test]
    async fn inspect_enforces_signature_and_containment() {
        let dir = std::env::temp_dir().join(format!(
            "oc-mig-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];
        std::fs::write(dir.join("sub/ok.png"), &png).unwrap();
        std::fs::write(dir.join("sub/not-image.txt"), b"plain text").unwrap();

        match inspect_image("sub/ok.png", &dir, &dir).await {
            InspectStatus::Ready {
                path,
                outside_workspace,
            } => {
                assert!(!outside_workspace);
                assert!(path.ends_with("sub/ok.png"));
            }
            other => panic!("expected ready, got {other:?}"),
        }
        assert!(matches!(
            inspect_image("sub/not-image.txt", &dir, &dir).await,
            InspectStatus::Error
        ));
        assert!(matches!(
            inspect_image("sub/gone.png", &dir, &dir).await,
            InspectStatus::Missing
        ));
        // Lexical escape is not authorized: resolves outside the root.
        assert!(matches!(
            inspect_image("../escape.png", &dir, &dir).await,
            InspectStatus::Error | InspectStatus::Missing
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
