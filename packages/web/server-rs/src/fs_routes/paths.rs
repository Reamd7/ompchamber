//! Path primitives mirroring the Node `path` module semantics the JS fs
//! routes depend on: lexical `resolve` (no symlink following), `relative`,
//! workspace containment, `normalizeDirectoryPath`
//! (settings-normalization-runtime.js), and small UUID / URI-component
//! helpers standing in for `crypto.randomUUID` / `decodeURIComponent`.

use std::path::{Component, Path, PathBuf};

use crate::config::home_dir;

/// settings-normalization-runtime.js `normalizeDirectoryPath`: trims, strips
/// wrapping quotes (Windows "copy as path" / quoted shell snippets), and
/// expands `~` / `~/...` against the home directory.
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
                    // Windows drive prefix (`C:`) followed by its root slash —
                    // the platform separator, like Node's win32 path.resolve.
                    out.push(if cfg!(windows) { "\\" } else { "/" });
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
pub fn is_path_within_root(resolved: &Path, root: &Path) -> bool {
    let resolved_root = resolve_path(&root.to_string_lossy());
    let relative = lexical_relative(&resolved_root, resolved);
    // A cross-root result comes back as the literal target; Node's win32
    // `isAbsolute` counts a drive-less root path (`\etc\passwd`) as absolute
    // while Rust's `Path::is_absolute` does not — treat a leading separator
    // as absolute too.
    !relative.starts_with("..")
        && !relative.starts_with('/')
        && !relative.starts_with('\\')
        && !Path::new(&relative).is_absolute()
}

/// `~/.config/ompchamber` — index.js `OMPCHAMBER_USER_CONFIG_ROOT`. The fs
/// workspace check always admits paths under this root regardless of the
/// active project.
pub fn user_config_root() -> PathBuf {
    let mut root = home_dir().unwrap_or_else(|| PathBuf::from("/"));
    root.push(".config");
    root.push("ompchamber");
    root
}

/// Node `fsPromises.realpath` (canonicalize). macOS note: `/tmp` resolves to
/// `/private/tmp`, same as Node. Windows note: Rust canonicalize yields
/// `\\?\`-prefixed verbatim paths; Node's realpath does not — strip them so
/// every consumer (wire responses, containment checks, grants) sees the same
/// form Node produced.
pub fn realpath(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(crate::settings::normalization::strip_verbatim_prefix)
}

/// Node `path.extname(p).toLowerCase()` ("" when there is no extension).
pub fn extname_lower(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

pub fn basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Node `path.dirname` (`/` stays `/`).
pub fn dirname(path: &Path) -> PathBuf {
    path.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// `crypto.randomUUID()`-shaped v4 identifier (rand-based; not
/// cryptographically guaranteed, which matches every fs-route use: job ids,
/// temp-file suffixes, grant tokens).
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
pub fn random_token(len: usize) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    (0..len)
        .map(|_| ALPHABET[(rand::random::<u32>() as usize) % ALPHABET.len()] as char)
        .collect()
}

/// `decodeURIComponent`: None on malformed escapes or invalid UTF-8
/// (callers keep the raw value, mirroring the JS try/catch).
pub fn decode_uri_component(value: &str) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn resolve_path_collapses_segments_lexically() {
        assert_eq!(resolve_path("/a/b/../c"), PathBuf::from("/a/c"));
        assert_eq!(resolve_path("/a/../../etc"), PathBuf::from("/etc"));
        assert_eq!(resolve_path("/a/./b//c/"), PathBuf::from("/a/b/c"));
        assert_eq!(resolve_path("/"), PathBuf::from("/"));
    }

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

    #[test]
    fn extname_and_basename_match_node_shapes() {
        assert_eq!(extname_lower(Path::new("/a/b.PNG")), "png");
        assert_eq!(extname_lower(Path::new("/a/.git")), "");
        assert_eq!(extname_lower(Path::new("/a/noext")), "");
        assert_eq!(basename(Path::new("/a/b/")), "b");
        assert_eq!(basename(Path::new("/")), "");
    }

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
