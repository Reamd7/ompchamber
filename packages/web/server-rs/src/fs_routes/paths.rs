//! Path primitives mirroring the Node `path` module semantics the JS fs
//! routes depend on: lexical `resolve` (no symlink following), `relative`,
//! workspace containment, `normalizeDirectoryPath`
//! (settings-normalization-runtime.js), and small UUID / URI-component
//! helpers standing in for `crypto.randomUUID` / `decodeURIComponent`.
//!
//! 中文说明：路径原语工具集。核心是与 Node `path` 模块的语义对齐：
//! `resolve_path` 是纯词法折叠（不解析 symlink），需要 canonical 目标
//! 时另行调用 `realpath`——这一"两步走"与 JS 路由完全一致，工作区
//! 准入判定（`is_path_within_root`）依赖该词法特性。

use std::path::{Component, Path, PathBuf};

use crate::config::home_dir;

/// settings-normalization-runtime.js `normalizeDirectoryPath`: trims, strips
/// wrapping quotes (Windows "copy as path" / quoted shell snippets), and
/// expands `~` / `~/...` against the home directory.
///
/// 中文说明：目录参数规范化——去除首尾空白；剥离成对包裹引号
/// （Windows"复制文件地址"与 shell 片段常见）；把 `~` / `~/...`
/// 展开为 home 目录；无法确定 home 时原样返回。
pub fn normalize_directory_path(value: &str) -> String {
    let mut trimmed = value.trim();
    if trimmed.chars().count() >= 2 {
        let first = trimmed.chars().next().unwrap_or_default();
        let last = trimmed.chars().last().unwrap_or_default();
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            trimmed = trimmed[1..trimmed.len() - 1].trim();
        }
    }
    if trimmed.is_empty() {
        return trimmed.to_string();
    }
    let Some(home) = home_dir() else {
        return trimmed.to_string();
    };
    if trimmed == "~" {
        return home.to_string_lossy().into_owned();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        return home.join(rest).to_string_lossy().into_owned();
    }
    trimmed.to_string()
}

/// Node `path.resolve(input)`: make absolute against the cwd and lexically
/// collapse `.` / `..` segments. Symlinks are NOT resolved here — routes that
/// need canonical targets call [`realpath`] separately, exactly like the JS.
///
/// 中文说明：Node `path.resolve` 的复刻——绝对路径保持原根，相对
/// 路径以 cwd 为基；逐段折叠 `.` 与 `..`。仅在词法层面操作，
/// 不触碰文件系统，symlink 保持原样。
pub fn resolve_path(input: &str) -> PathBuf {
    let path = Path::new(input);
    let mut out = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    };
    for component in path.components() {
        match component {
            Component::RootDir => {
                let rendered = out.to_string_lossy().into_owned();
                if !rendered.is_empty() && rendered.ends_with(':') {
                    // Windows drive prefix (`C:`) followed by its root slash.
                    out.push("/");
                } else {
                    out = PathBuf::from("/");
                }
            }
            Component::Prefix(prefix) => {
                out = PathBuf::from(prefix.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(segment) => {
                out.push(segment);
            }
        }
    }
    out
}

/// Node `path.relative(from, to)` over already-resolved inputs. Returns the
/// literal absolute `to` when the roots differ (Windows cross-drive case);
/// on POSIX the roots always match.
///
/// 中文说明：Node `path.relative` 的复刻（要求输入已 resolve）：
/// 剥离公共前缀后用 `..` 回退再拼接；根不同（如 Windows 跨盘符）
/// 时直接返回 `to` 的绝对路径字符串，分隔符统一为 `/`。
pub fn lexical_relative(from: &Path, to: &Path) -> String {
    let from_components: Vec<Component<'_>> = from.components().collect();
    let to_components: Vec<Component<'_>> = to.components().collect();
    if from_components.first() != to_components.first() {
        return to.to_string_lossy().into_owned();
    }
    let mut shared = 0;
    while shared < from_components.len()
        && shared < to_components.len()
        && from_components[shared] == to_components[shared]
    {
        shared += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in shared..from_components.len() {
        parts.push("..".to_string());
    }
    for component in &to_components[shared..] {
        parts.push(component.as_os_str().to_string_lossy().into_owned());
    }
    parts.join("/")
}

/// routes.js `isPathWithinRoot`: `path.relative(root, resolved)` must not
/// escape through `..` (matching the JS `startsWith('..')` quirk) or come
/// back absolute. Both inputs are lexically resolved first.
///
/// 中文说明：`resolved` 是否位于 `root` 之内——先词法 resolve 两端，
/// 再要求相对路径不以 `..` 开头且不是绝对路径。该判定是全部
/// `/api/fs` 工作区准入的基础。
pub fn is_path_within_root(resolved: &Path, root: &Path) -> bool {
    let resolved_root = resolve_path(&root.to_string_lossy());
    let relative = lexical_relative(&resolved_root, resolved);
    !relative.starts_with("..") && !Path::new(&relative).is_absolute()
}

/// `~/.config/ompchamber` — index.js `OMPCHAMBER_USER_CONFIG_ROOT`. The fs
/// workspace check always admits paths under this root regardless of the
/// active project.
///
/// 中文说明：返回 `~/.config/ompchamber`（home 不可得时回落 `/`），
/// fs 工作区检查对位于该根下的路径恒放行。
pub fn user_config_root() -> PathBuf {
    let mut root = home_dir().unwrap_or_else(|| PathBuf::from("/"));
    root.push(".config");
    root.push("ompchamber");
    root
}

/// Node `fsPromises.realpath` (canonicalize). macOS note: `/tmp` resolves to
/// `/private/tmp`, same as Node.
///
/// 中文说明：canonicalize 的薄封装；macOS 上 `/tmp` 会解析为
/// `/private/tmp`，与 Node 行为一致。
pub fn realpath(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path)
}

/// Node `path.extname(p).toLowerCase()` ("" when there is no extension).
///
/// 中文说明：返回小写扩展名（不含点）；无扩展名或点号开头的
/// 隐藏文件名（如 `.git`）返回空串。
pub fn extname_lower(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Node `path.basename`（路径以分隔符结尾时取最后一段；`/` 得空串）。
pub fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Node `path.dirname` (`/` stays `/`).
///
/// 中文说明：返回父目录；根路径 `/` 的父仍是 `/`（与 Node 一致）。
pub fn dirname(path: &Path) -> PathBuf {
    path.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `crypto.randomUUID()`-shaped v4 identifier (rand-based; not
/// cryptographically guaranteed, which matches every fs-route use: job ids,
/// temp-file suffixes, grant tokens).
///
/// 中文说明：生成 v4 形状的 UUID（含版本/变体位），基于随机数；
/// 非密码学保证，与 JS 侧所有用途（任务 id、临时文件后缀、授权
/// token）的安全模型一致。
pub fn random_uuid() -> String {
    let mut bytes = rand::random::<[u8; 16]>();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Short lowercase-alphanumeric token (write tmp-file suffix).
///
/// 中文说明：生成指定长度的随机小写字母数字串（写临时文件的后缀）。
pub fn random_token(len: usize) -> String {
    /// 随机字符表：数字与小写字母共 36 个。
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    (0..len)
        .map(|_| ALPHABET[(rand::random::<u32>() as usize) % ALPHABET.len()] as char)
        .collect()
}

/// `decodeURIComponent`: None on malformed escapes or invalid UTF-8
/// (callers keep the raw value, mirroring the JS try/catch).
///
/// 中文说明：`decodeURIComponent` 的复刻——逐字节解析 `%XX` 转义，
/// 结果必须是合法 UTF-8；任何非法输入返回 None（调用方保留原始
/// 字符串，等价 JS 的 try/catch）。
pub fn decode_uri_component(value: &str) -> Option<String> {
    /// 十六进制字符转数值；非十六进制返回 None。
    fn hex_val(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hi = hex_val(*bytes.get(index + 1)?)?;
            let lo = hex_val(*bytes.get(index + 2)?)?;
            out.push(hi * 16 + lo);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// `encodeURIComponent` (RFC 3986 unreserved set + JS extras kept literal).
///
/// 中文说明：`encodeURIComponent` 的复刻——字母数字与 RFC 3986
/// 未保留符号（含 JS 保留的 `!'()*~`）保持字面，其余字节按
/// `%XX` 大写转义。
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// JS truthiness for JSON body values (`if (allowOutsideWorkspace)`).
///
/// 中文说明：JS 真值判定——仅 null、false、空串、0/NaN 为假，
/// 空数组/对象为真；用于 `if (allowOutsideWorkspace)` 之类的
/// 请求体字段判断。
pub fn js_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(flag) => *flag,
        serde_json::Value::Number(number) => number
            .as_f64()
            .map(|n| n != 0.0 && !n.is_nan())
            .unwrap_or(true),
        serde_json::Value::String(text) => !text.is_empty(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => true,
    }
}

/// 路径原语与 Node 语义对齐的单元测试。
#[cfg(test)]
mod tests {
    use super::*;

    /// 验证目录参数规范化：trim、剥引号、`~` 展开、home 缺失回退。
    #[test]
    fn normalize_directory_path_strips_quotes_and_expands_home() {
        assert_eq!(normalize_directory_path("  /repo  "), "/repo");
        assert_eq!(normalize_directory_path("\"/repo\""), "/repo");
        assert_eq!(normalize_directory_path("''"), "");
        let home = home_dir().expect("home");
        assert_eq!(PathBuf::from(normalize_directory_path("~")), home);
        assert_eq!(
            PathBuf::from(normalize_directory_path("~/src/app")),
            home.join("src").join("app")
        );
        assert_eq!(normalize_directory_path("/plain/path"), "/plain/path");
    }

    /// 验证词法 resolve 正确折叠 `.`/`..` 与重复分隔符。
    #[test]
    fn resolve_path_collapses_segments_lexically() {
        assert_eq!(resolve_path("/a/b/../c"), PathBuf::from("/a/c"));
        assert_eq!(resolve_path("/a/../../etc"), PathBuf::from("/etc"));
        assert_eq!(resolve_path("/a/./b//c/"), PathBuf::from("/a/b/c"));
        assert_eq!(resolve_path("/"), PathBuf::from("/"));
    }

    /// 验证 relative 与 containment 判定镜像 Node 行为，
    /// 包括 `..` 逃逸被拦截。
    #[test]
    fn lexical_relative_and_containment_mirror_node() {
        assert_eq!(
            lexical_relative(Path::new("/a"), Path::new("/a/b/c")),
            "b/c"
        );
        assert_eq!(lexical_relative(Path::new("/a/b"), Path::new("/a")), "..");
        assert_eq!(
            lexical_relative(Path::new("/a/b"), Path::new("/a/c")),
            "../c"
        );
        assert_eq!(lexical_relative(Path::new("/a"), Path::new("/a")), "");

        assert!(is_path_within_root(
            Path::new("/repo/file.txt"),
            Path::new("/repo")
        ));
        assert!(is_path_within_root(Path::new("/repo"), Path::new("/repo")));
        assert!(!is_path_within_root(
            Path::new("/etc/passwd"),
            Path::new("/repo")
        ));
        assert!(!is_path_within_root(
            Path::new("/repo/../etc"),
            Path::new("/repo")
        ));
    }

    /// 验证 extname/basename 的 Node 形状（大小写、隐藏文件、结尾分隔符）。
    #[test]
    fn extname_and_basename_match_node_shapes() {
        assert_eq!(extname_lower(Path::new("/a/b.PNG")), "png");
        assert_eq!(extname_lower(Path::new("/a/.git")), "");
        assert_eq!(extname_lower(Path::new("/a/noext")), "");
        assert_eq!(basename(Path::new("/a/b/")), "b");
        assert_eq!(basename(Path::new("/")), "");
    }

    /// 验证 URI 组件编解码与 JS 逐字节一致（中文、空格、非法转义）。
    #[test]
    fn uri_component_round_trips_like_js() {
        assert_eq!(encode_uri_component("文件.txt"), "%E6%96%87%E4%BB%B6.txt");
        assert_eq!(encode_uri_component("a b*c"), "a%20b*c");
        assert_eq!(
            decode_uri_component("%E6%96%87%E4%BB%B6.txt").as_deref(),
            Some("文件.txt")
        );
        assert_eq!(decode_uri_component("%zz"), None);
        assert_eq!(decode_uri_component("%E6%96"), None);
    }

    /// 验证 UUID 的 v4 形状（8-4-4-4-12 段、十六进制）与唯一性。
    #[test]
    fn random_uuid_has_v4_shape() {
        let id = random_uuid();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{id}"
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_ne!(random_uuid(), random_uuid());
    }

    /// 验证 JS 真值表对全部 JSON 值类型的覆盖。
    #[test]
    fn js_truthy_covers_json_values() {
        assert!(!js_truthy(&serde_json::Value::Null));
        assert!(!js_truthy(&serde_json::Value::Bool(false)));
        assert!(!js_truthy(&serde_json::json!("")));
        assert!(!js_truthy(&serde_json::json!(0)));
        assert!(js_truthy(&serde_json::json!(true)));
        assert!(js_truthy(&serde_json::json!("x")));
        assert!(js_truthy(&serde_json::json!([])));
    }
}
