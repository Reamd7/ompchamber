//! Pure helpers ported from `package-manager.js`: path normalization and
//! comparison, platform/arch mapping, the install-id file, and the
//! semver-like comparison used by update checks.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// process.* shims
// ---------------------------------------------------------------------------

/// `process.platform` (`std::env::consts::OS` mapped onto the Node names).
pub fn process_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

pub fn is_windows() -> bool {
    cfg!(target_os = "windows")
}

/// `process.arch`.
pub fn process_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86_64") {
        "x64"
    } else {
        "unknown"
    }
}

/// `os.homedir()` (Node resolves USERPROFILE on Windows, HOME elsewhere).
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

// ---------------------------------------------------------------------------
// map*/normalize* helpers
// ---------------------------------------------------------------------------

pub fn map_platform(value: &str) -> &'static str {
    match value {
        "darwin" => "macos",
        "win32" => "windows",
        "linux" => "linux",
        _ => "web",
    }
}

pub fn map_arch(value: &str) -> &'static str {
    match value {
        "arm64" | "aarch64" => "arm64",
        "x64" | "amd64" => "x64",
        _ => "unknown",
    }
}

pub fn normalize_app_type(value: Option<&str>) -> &'static str {
    match value {
        Some("web") => "web",
        Some("desktop-electron") => "desktop-electron",
        Some("vscode") => "vscode",
        Some("mobile-capacitor") => "mobile-capacitor",
        _ => "web",
    }
}

pub fn normalize_device_class(value: Option<&str>) -> &'static str {
    match value {
        Some("mobile") => "mobile",
        Some("tablet") => "tablet",
        Some("desktop") => "desktop",
        Some("unknown") => "unknown",
        _ => "unknown",
    }
}

pub fn normalize_platform(value: Option<&str>) -> &'static str {
    match value {
        Some("macos") => "macos",
        Some("windows") => "windows",
        Some("linux") => "linux",
        Some("web") => "web",
        Some("android") => "android",
        Some("ios") => "ios",
        _ => map_platform(process_platform()),
    }
}

pub fn normalize_arch(value: Option<&str>) -> &'static str {
    match value {
        Some("arm64") => "arm64",
        Some("x64") => "x64",
        Some("unknown") => "unknown",
        _ => map_arch(process_arch()),
    }
}

/// `path.resolve(input)` — absolutize against the cwd and lexically remove
/// `.`/`..` segments (no symlink resolution, exactly like Node).
pub fn resolve_lexically(input: &Path) -> PathBuf {
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        cwd.join(input)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// JS `normalizePathForComparison`: resolved + normalized (lowercased on
/// Windows); `None` for empty input.
pub fn normalize_path_for_comparison(input: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    let mut normalized = resolve_lexically(Path::new(input))
        .to_string_lossy()
        .into_owned();
    if is_windows() {
        normalized = normalized.to_lowercase();
    }
    Some(normalized)
}

/// JS `getComparablePaths`: the normalized path plus its realpath (when it
/// exists), for symlink-insensitive comparison.
pub fn get_comparable_paths(input: &str) -> HashSet<String> {
    let mut paths = HashSet::new();
    if let Some(normalized) = normalize_path_for_comparison(input) {
        paths.insert(normalized);
    }
    if let Ok(real) = std::fs::canonicalize(input)
        && let Some(normalized_real) = normalize_path_for_comparison(&real.to_string_lossy())
    {
        paths.insert(normalized_real);
    }
    paths
}

/// JS `pathSetContains`.
pub fn path_set_contains(a: &HashSet<String>, b: &HashSet<String>) -> bool {
    a.iter().any(|value| b.contains(value))
}

/// JS `getUniquePaths`: dedupe by normalized form, keep the resolved original.
pub fn get_unique_paths(paths: &[String]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for value in paths {
        let Some(normalized) = normalize_path_for_comparison(value) else {
            continue;
        };
        if seen.contains(&normalized) {
            continue;
        }
        seen.insert(normalized);
        result.push(resolve_lexically(Path::new(value)));
    }
    result
}

// ---------------------------------------------------------------------------
// install id (getOMPChamberConfigDir / getOrCreateInstallId)
// ---------------------------------------------------------------------------

/// JS `getOMPChamberConfigDir`: `%APPDATA%/ompchamber` on Windows, otherwise
/// `~/.config/ompchamber` (deliberately ignoring XDG_CONFIG_HOME).
pub fn get_ompchamber_config_dir(home: Option<&Path>, appdata: Option<&str>) -> Option<PathBuf> {
    if is_windows() {
        if let Some(appdata) = appdata {
            return Some(Path::new(appdata).join("ompchamber"));
        }
        // Node falls through to os.homedir() when APPDATA is unset.
        return home.map(|home| home.join(".config").join("ompchamber"));
    }
    home.map(|home| home.join(".config").join("ompchamber"))
}

/// JS `sanitizeInstallScope`.
pub fn sanitize_install_scope(scope: &str) -> &str {
    match scope {
        "desktop-electron" | "vscode" | "web" | "mobile-capacitor" => scope,
        _ => "web",
    }
}

/// `crypto.randomUUID()` (v4), hand-rolled over `rand`.
pub fn random_uuid_v4() -> String {
    let mut bytes = rand::random::<u128>().to_le_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// JS `getOrCreateInstallId` against an explicit config dir (the runtime
/// resolves the dir from the environment seam).
pub fn get_or_create_install_id(config_dir: &Path, scope: &str) -> std::io::Result<String> {
    let normalized_scope = sanitize_install_scope(scope);
    let id_path = config_dir.join(format!("install-id-{normalized_scope}"));

    if let Ok(existing) = std::fs::read_to_string(&id_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let install_id = random_uuid_v4();
    std::fs::create_dir_all(config_dir)?;
    std::fs::write(&id_path, format!("{install_id}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&id_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(install_id)
}

// ---------------------------------------------------------------------------
// version comparison
// ---------------------------------------------------------------------------

/// JS `parseVersionForComparison`: strip `v`, drop build metadata, split the
/// core on `.`, parseInt each part (invalid → 0), remember prerelease.
pub(crate) struct ParsedVersion {
    pub parts: Vec<i64>,
    pub prerelease: bool,
}

/// `Number.parseInt(part || '0', 10)` — leading whitespace, optional sign,
/// leading digits; anything else parses to 0.
fn js_parse_int(part: &str) -> i64 {
    let trimmed = part.trim_start();
    let mut digits = String::new();
    let mut chars = trimmed.chars();
    if let Some(first) = chars.next() {
        if first == '+' || first == '-' || first.is_ascii_digit() {
            digits.push(first);
        } else {
            return 0;
        }
    }
    for ch in chars {
        if ch.is_ascii_digit() {
            digits.push(ch);
        } else {
            break;
        }
    }
    digits.parse::<i64>().unwrap_or(0)
}

pub(crate) fn parse_version_for_comparison(value: Option<&str>) -> ParsedVersion {
    let raw = value.unwrap_or("");
    let without_v = raw.strip_prefix('v').unwrap_or(raw);
    let normalized = without_v.split('+').next().unwrap_or("");
    let prerelease_index = normalized.find('-');
    let core = match prerelease_index {
        Some(index) => &normalized[..index],
        None => normalized,
    };
    let parts = core.split('.').map(js_parse_int).collect();
    ParsedVersion {
        parts,
        prerelease: prerelease_index.is_some(),
    }
}

/// JS `compareVersions`: >0 when `left` is newer, <0 older, 0 equal. Any
/// prerelease sorts below the same core version.
pub(crate) fn compare_versions(left: Option<&str>, right: Option<&str>) -> i32 {
    let a = parse_version_for_comparison(left);
    let b = parse_version_for_comparison(right);
    let length = a.parts.len().max(b.parts.len());
    for index in 0..length {
        let left_part = a.parts.get(index).copied().unwrap_or(0);
        let right_part = b.parts.get(index).copied().unwrap_or(0);
        let diff = left_part.checked_sub(right_part).unwrap_or(1);
        if diff != 0 {
            return diff.clamp(-1, 1) as i32;
        }
    }
    if a.prerelease != b.prerelease {
        return if a.prerelease { -1 } else { 1 };
    }
    0
}

// ---------------------------------------------------------------------------
// changelog section parsing
// ---------------------------------------------------------------------------

/// JS `changelog.split(/^## /m).slice(1)` — sections start after each
/// line-leading `## ` separator; the preamble is dropped.
pub(crate) fn split_h2_sections(text: &str) -> Vec<String> {
    let mut sections: Vec<String> = vec![String::new()];
    for line in text.split_inclusive('\n') {
        if let Some(rest) = line.strip_prefix("## ") {
            sections.push(String::from(rest));
        } else if let Some(last) = sections.last_mut() {
            last.push_str(line);
        }
    }
    sections.remove(0);
    sections
}

/// The `^\[(\d+\.\d+\.\d+)\]` header of a changelog section, if any.
pub(crate) fn changelog_section_version(section: &str) -> Option<&str> {
    let rest = section.strip_prefix('[')?;
    let end = rest.find(']')?;
    let inner = &rest[..end];
    let parts: Vec<&str> = inner.split('.').collect();
    if parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    {
        Some(inner)
    } else {
        None
    }
}
